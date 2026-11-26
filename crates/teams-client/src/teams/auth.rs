//! Sign-in for the Teams web API (the one the Teams web app itself uses).
//!
//! The user signs in to the Teams web app in their own browser (see `browser`). We read the app's MSAL cache, keep
//! only the refresh token and identity, and from then on mint what the API wants over plain HTTP:
//!
//! 1. an access token for `https://api.spaces.skype.com` (refresh-token grant against Entra ID, with the SPA `Origin`),
//! 2. a "skypetoken" from `authsvc.teams.microsoft.com/v1.0/authz`, which also names the regional chat service.
//!
//! SPA refresh tokens have a fixed lifetime, so when Entra says the grant is no longer good we reopen the private
//! browser profile headless and let Teams' own SSO renew the cache; only if that fails is an interactive sign-in
//! needed again. A failed silent renewal pauses further attempts for a while, so a persistent problem cannot launch a
//! browser on every poll.

use super::browser::{BrowserOptions, capture, find_browser};
use crate::error::{AuthError, Error, Result, StoreError, is_grant_code};
use crate::headerlog::HeaderLog;
use crate::http::{self, Call};
use crate::record::ShapeRecorder;
use crate::store::{
    CredentialStore, FORMAT_VERSION, check_version, format_version, load_json, save_json,
};
use crate::{lock, unix_now};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

/// The first-party Teams web app (a public SPA client); preferred when the cache holds several refresh tokens.
const TEAMS_CLIENT_ID: &str = "5e3ce6c0-2b1f-4285-8d4b-75ee78787346";
const SPACES_SCOPE: &str = "https://api.spaces.skype.com/.default offline_access";
const SUBSTRATE_SCOPE: &str = "https://substrate.office.com/.default offline_access";
const CSA_SCOPE: &str = "https://chatsvcagg.teams.microsoft.com/.default offline_access";
/// Teams' presence service (`presence.teams.microsoft.com`) takes a token of its own, got like the others from the
/// refresh token. Behaviour from other open-source clients (`docs/teams-web-protocol.md`); not verified live.
const PRESENCE_SCOPE: &str = "https://presence.teams.microsoft.com/.default offline_access";
/// Resolving a SharePoint/OneDrive attachment to a direct download (`shares.rs`). Some restricted clouds withhold
/// this scope; `token`'s own doc comment says why that is returned as a plain rejection rather than chased through
/// the browser the way [`SPACES_SCOPE`] is.
const GRAPH_SCOPE: &str = "https://graph.microsoft.com/.default offline_access";
/// The resource the real Teams web client's own chat-sending calls are scoped to (confirmed live, 2026-11-06, from
/// its own captured request). Its token carries `given_name`/`family_name` claims that [`SPACES_SCOPE`]'s does
/// not; the server stamps those onto a message sent with it, which turned out to be what the real client checks
/// to show a reply's quote as verified rather than flagging it (see `AGENTS.md`, "Messages, mentions and replies" and
/// `docs/teams-web-protocol.md`'s "Reply-quote verification").
const IC3_SCOPE: &str = "https://ic3.teams.office.com/.default offline_access";
const LOGIN_BASE: &str = "https://login.microsoftonline.com";
const AUTHZ_URL: &str = "https://authsvc.teams.microsoft.com/v1.0/authz";
/// How long one sign-in or renewal request may take altogether.
const AGENT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_ORIGIN: &str = "https://teams.microsoft.com";
const INTERACTIVE_TIMEOUT: Duration = Duration::from_secs(1800);
const SILENT_TIMEOUT: Duration = Duration::from_secs(30);
/// After a failed silent renewal, wait this long before launching the browser again.
const RENEWAL_PAUSE: Duration = Duration::from_secs(300);
/// How long a scope is left alone after Entra refused its grant: five minutes, doubling with each refusal in a row,
/// at most an hour. Without this every caller (presence, activity reports, sends, favourites, ...) asked again on its
/// own schedule, which was seen live as about three refused requests a minute, for as long as the grant stayed dead.
const REFUSAL_PAUSE: Duration = Duration::from_secs(300);
const REFUSAL_PAUSE_MAX: Duration = Duration::from_secs(3600);

/// What stands in for the browser when tests run the silent renewal.
#[cfg(test)]
type RecoverHook = Box<dyn Fn(&TeamsAuth) -> Result<()> + Send + Sync>;

/// The host a scope is for (`presence.teams.microsoft.com`), for a log line; scopes are URLs plus `offline_access`.
fn scope_host(scope: &str) -> String {
    url::Url::parse(scope.split(' ').next().unwrap_or(scope))
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "a service".to_string())
}

/// Entra's refusal of one scope's grant, remembered so that the scope is not asked about again at once.
#[derive(Debug)]
struct Refusal {
    /// Not asked about again before this instant.
    until: Instant,
    /// Refusals in a row: any success for the scope, or a new sign-in, starts the count over.
    strikes: u32,
    code: String,
    description: String,
}

/// The stored form. Private, so the serialisation format is not part of this crate's API.
#[derive(Clone, Serialize, Deserialize)]
struct Stored {
    /// Format of this document; see [`FORMAT_VERSION`].
    #[serde(default = "format_version")]
    version: u32,
    client_id: String,
    tenant: String,
    /// The user's Entra object id (a GUID); Teams calls it `8:orgid:<oid>`.
    oid: String,
    name: String,
    refresh_token: String,
    /// Origin the SPA tokens were issued to; Entra insists on it in the refresh request.
    origin: String,
}

/// A Teams web sign-in: who the user is, and the refresh token that keeps them signed in. Obtain one with
/// [`sign_in`] or [`TeamsSession::load`]. `Debug` hides the token.
#[derive(Clone)]
pub struct TeamsSession(Stored);

impl fmt::Debug for TeamsSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TeamsSession")
            .field("oid", &self.0.oid)
            .field("name", &self.0.name)
            .field("tenant", &self.0.tenant)
            .finish_non_exhaustive()
    }
}

impl TeamsSession {
    /// A session for tests; its refresh token is never valid anywhere.
    #[cfg(test)]
    pub(crate) fn for_test(oid: &str, name: &str) -> Self {
        Self(Stored {
            version: FORMAT_VERSION,
            client_id: "test-client".into(),
            tenant: "test-tenant".into(),
            oid: oid.into(),
            name: name.into(),
            refresh_token: "test-refresh".into(),
            origin: DEFAULT_ORIGIN.into(),
        })
    }

    /// The signed-in user's Entra object id.
    #[must_use]
    pub fn user_id(&self) -> &str {
        &self.0.oid
    }

    /// The signed-in user's display name; empty if it was not in the browser's cache.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.0.name
    }

    /// The signed-in user's Entra tenant id (a GUID).
    #[must_use]
    pub fn tenant_id(&self) -> &str {
        &self.0.tenant
    }

    /// Read a session saved by [`TeamsSession::save`].
    ///
    /// # Errors
    /// [`StoreError`] if the store cannot be read or its contents are not valid.
    pub fn load(store: &dyn CredentialStore) -> Result<Option<Self>, StoreError> {
        let stored = load_json::<Stored>(store)?;
        if let Some(s) = &stored {
            check_version(s.version)?;
            // The same rule the browser's cache is held to when a sign-in is first read: what is stored could have
            // been edited since, and these two go into the token endpoint's address.
            if !is_valid_id(&s.tenant) || !is_valid_id(&s.oid) {
                return Err(StoreError::Corrupt {
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "the stored sign-in names a tenant or account that is not an identifier",
                    )),
                });
            }
        }
        Ok(stored.map(Self))
    }

    /// Save the session.
    ///
    /// # Errors
    /// [`StoreError`] if the store cannot be written.
    pub fn save(&self, store: &dyn CredentialStore) -> Result<(), StoreError> {
        save_json(store, &self.0)
    }
}

/// What the API calls need.
#[derive(Clone)]
pub(crate) struct Live {
    pub(crate) spaces_token: String,
    pub(crate) spaces_exp: i64,
    pub(crate) skype_token: String,
    pub(crate) skype_exp: i64,
    /// `https://<host>/api/chatsvc/<region>`
    pub(crate) chat_base: String,
    /// `https://<host>/api/mt/part/<region>-<partition>`; empty if the service did not name a trusted one.
    pub(crate) mt_base: String,
    /// `https://<host>/api/csa/<region>`, the chat service aggregator; empty if the service did not name a trusted one.
    pub(crate) csa_base: String,
}

impl fmt::Debug for Live {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Live")
            .field("chat_base", &self.chat_base)
            .field("mt_base", &self.mt_base)
            .field("csa_base", &self.csa_base)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
static TEST_ORIGINS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Let tests treat a local server as a Teams host. Each test server has its own port, so entries never collide.
#[cfg(test)]
pub(crate) fn trust_for_tests(origin: &str) {
    lock(&TEST_ORIGINS).push(origin.to_string());
}

/// Hosts that may receive our tokens: the Teams web endpoints, and nothing named inside message content.
pub(crate) fn trusted_url(url: &str) -> bool {
    #[cfg(test)]
    if lock(&TEST_ORIGINS)
        .iter()
        .any(|o| url.starts_with(o.as_str()))
    {
        return true;
    }
    let Ok(u) = url::Url::parse(url) else {
        return false;
    };
    let host = u.host_str().unwrap_or("");
    u.scheme() == "https"
        && [
            "teams.microsoft.com",
            "teams.cloud.microsoft",
            "teams.microsoft.us",
        ]
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?).ok()
}

/// Pull the sign-in out of Teams' MSAL cache (`localStorage` entries are JSON documents with a `credentialType`).
pub(crate) fn session_from_storage(
    origin: &str,
    storage: &Map<String, Value>,
) -> Option<TeamsSession> {
    sessions_from_storage(origin, storage, None)
        .into_iter()
        .next()
}

/// Whether `s` can be a tenant or object id: both end up in URLs (the token endpoint's path, request bodies), so
/// nothing but plain identifier characters, and not `.` or `..`, which a path would read as a step to another folder.
fn is_valid_id(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// Every sign-in the cache holds a refresh token for, best first: a token issued to the Teams client itself (what
/// another application signed in on the same page holds is not the Teams sign-in and is left out), and among those the
/// account `prefer_oid` names, if given. More than one is possible (another account in the same browser profile, or
/// an old token beside the one Teams has just renewed), so a caller that redeems them tries them in this order rather
/// than judging the first alone.
pub(crate) fn sessions_from_storage(
    origin: &str,
    storage: &Map<String, Value>,
    prefer_oid: Option<&str>,
) -> Vec<TeamsSession> {
    let entries: Vec<Value> = storage
        .values()
        .filter_map(|v| v.as_str())
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect();
    let of = |kind: &'static str| {
        entries
            .iter()
            .filter(move |e| e["credentialType"].as_str() == Some(kind))
    };
    let field = |e: &Value, k: &str| e[k].as_str().filter(|s| !s.is_empty()).map(String::from);
    let mut refreshes: Vec<&Value> = of("RefreshToken")
        .filter(|e| {
            field(e, "secret").is_some()
                && field(e, "homeAccountId").is_some()
                && e["clientId"].as_str() == Some(TEAMS_CLIENT_ID)
        })
        .collect();
    let oid_of = |e: &Value| {
        field(e, "homeAccountId")
            .and_then(|h| h.split('.').next().map(String::from))
            .unwrap_or_default()
    };
    // Stable, so the cache's own order still decides among equals.
    refreshes.sort_by_key(|e| prefer_oid.is_none_or(|want| oid_of(e) != want));

    refreshes
        .into_iter()
        .filter_map(|refresh| {
            let home = field(refresh, "homeAccountId")?;
            let (oid, home_tenant) = home.split_once('.').unwrap_or((&home, ""));
            let tenant = of("AccessToken")
                .filter(|e| field(e, "homeAccountId").as_deref() == Some(home.as_str()))
                .filter_map(|e| field(e, "realm"))
                .next()
                .or_else(|| Some(home_tenant.to_string()).filter(|t| !t.is_empty()))?;
            let name = of("IdToken")
                .filter(|e| field(e, "homeAccountId").as_deref() == Some(home.as_str()))
                .filter_map(|e| field(e, "secret"))
                .find_map(|t| jwt_claims(&t)?["name"].as_str().map(String::from))
                .unwrap_or_default();
            if !is_valid_id(&tenant) || !is_valid_id(oid) {
                return None;
            }
            Some(TeamsSession(Stored {
                version: FORMAT_VERSION,
                client_id: field(refresh, "clientId")?,
                tenant,
                oid: oid.to_string(),
                name: crate::clean(&name),
                refresh_token: field(refresh, "secret")?,
                origin: origin.to_string(),
            }))
        })
        .collect()
}
/// Interactive sign-in: open the user's browser on Teams, wait for them to finish, and save the session to `store`.
/// `announce` is called once just before the browser opens so the caller can say what is about to happen.
///
/// # Errors
/// [`BrowserError`](crate::BrowserError) if no usable browser is found or it cannot be driven,
/// [`AuthError::TimedOut`] if the user does not finish in thirty minutes, [`StoreError`] if saving fails.
pub fn sign_in(
    options: &BrowserOptions,
    store: &dyn CredentialStore,
    announce: &dyn Fn(),
) -> Result<TeamsSession> {
    find_browser(options.executable.as_deref())?;
    announce();
    let session = capture(options, false, INTERACTIVE_TIMEOUT, session_from_storage)?;
    session.save(store)?;
    Ok(session)
}

/// What the token endpoint gave for a refresh token.
struct Grant {
    access_token: String,
    expires_in: i64,
    /// The refresh token to use from now on, when Entra issued a new one.
    rotated: Option<String>,
}

/// Keeps API credentials fresh for one session.
pub(crate) struct TeamsAuth {
    browser: BrowserOptions,
    store: Arc<dyn CredentialStore>,
    agent: ureq::Agent,
    session: Mutex<TeamsSession>,
    warnings: Mutex<Vec<Error>>,
    /// Held while renewing (refresh and, if needed, browser recovery), so concurrent callers share one renewal and
    /// two browser launches can never fight over one profile directory.
    renewing: Mutex<()>,
    /// Silent renewal is not attempted again before this instant.
    paused_until: Mutex<Option<Instant>>,
    /// The scopes whose grant Entra refused, and until when each is left alone (see [`REFUSAL_PAUSE`]).
    refusals: Mutex<HashMap<String, Refusal>>,
    /// How many times a session has been adopted (a renewal through the browser, or a sign-in again): a caller that
    /// saw the count change knows the grant it was refused on has been replaced.
    renewals: AtomicU32,
    /// For tests: stands in for the browser when the silent renewal runs.
    #[cfg(test)]
    recover_hook: Mutex<Option<RecoverHook>>,
    /// An interactive sign-in is open in the browser. Requests then fail at once instead of queueing behind it for
    /// up to thirty minutes.
    signing_in: AtomicBool,
    login_base: String,
    authz_url: String,
    recorder: Option<Arc<ShapeRecorder>>,
}

impl TeamsAuth {
    pub(crate) fn new(
        session: TeamsSession,
        store: Arc<dyn CredentialStore>,
        browser: BrowserOptions,
    ) -> Self {
        Self {
            browser,
            store,
            agent: http::agent(AGENT_TIMEOUT),
            session: Mutex::new(session),
            warnings: Mutex::new(Vec::new()),
            renewing: Mutex::new(()),
            paused_until: Mutex::new(None),
            refusals: Mutex::new(HashMap::new()),
            renewals: AtomicU32::new(0),
            #[cfg(test)]
            recover_hook: Mutex::new(None),
            signing_in: AtomicBool::new(false),
            login_base: LOGIN_BASE.to_string(),
            authz_url: AUTHZ_URL.to_string(),
            recorder: None,
        }
    }

    pub(crate) fn with_recorder(mut self, recorder: Arc<ShapeRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// Log the response headers `log` selects, for the sign-in and renewal requests too.
    pub(crate) fn with_header_log(mut self, log: &Arc<HeaderLog>) -> Self {
        self.agent = http::logging_agent(AGENT_TIMEOUT, log);
        self
    }

    #[cfg(test)]
    pub(super) fn with_endpoints(mut self, login_base: &str, authz_url: &str) -> Self {
        self.login_base = login_base.to_string();
        self.authz_url = authz_url.to_string();
        self
    }

    /// The user's object id and display name.
    pub(crate) fn me(&self) -> (String, String) {
        let s = lock(&self.session);
        (s.0.oid.clone(), s.0.name.clone())
    }

    /// The user's tenant id.
    pub(crate) fn tenant_id(&self) -> String {
        lock(&self.session).0.tenant.clone()
    }

    pub(crate) fn take_warnings(&self) -> Vec<Error> {
        std::mem::take(&mut *lock(&self.warnings))
    }

    /// Report a problem that did not fail the operation in progress.
    pub(crate) fn warn(&self, e: Error) {
        log::warn!("{}", e.for_log());
        lock(&self.warnings).push(e);
    }

    /// Save `session`; a failure is reported as a warning because the in-memory session is still good.
    fn persist(&self, session: &TeamsSession) {
        if let Err(e) = session.save(self.store.as_ref()) {
            self.warn(e.into());
        }
    }

    /// An access token for `scope`. If Entra says the stored grant is no longer good, try to renew it through the
    /// browser profile (at most once per pause window); any other failure, such as throttling or a network error, is
    /// returned as it is, because launching a browser would not fix it.
    ///
    /// Recovery through the browser only runs for [`SPACES_SCOPE`], the scope every poll needs, and a refusal of any
    /// other scope first checks whether it is that scope that is refused too (see [`Self::token_locked`]): a tenant
    /// that withholds just the CSA, Substrate, presence or Graph scope rejects it the same way an expired grant is
    /// rejected, and a browser launch cannot fix a scope the account was never granted (nor should it make the whole
    /// session look signed out while ordinary chat reading works).
    fn token(&self, scope: &str) -> Result<(String, i64)> {
        let _single_flight = loop {
            if self.signing_in.load(Ordering::SeqCst) {
                return Err(AuthError::SignInInProgress.into());
            }
            match self.renewing.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        self.token_locked(scope)
    }

    /// [`Self::token`] for a caller that already holds the single-flight lock.
    ///
    /// **A refusal is acted on the first time it is seen, not just waited out.** A refused grant for a scope other
    /// than the main one says either that this scope alone is withheld, or that the whole grant has died (Teams'
    /// refresh tokens last a fixed 24 hours). Which it is decides what to do, so the main scope is asked for at once:
    /// if that is refused too, the silent renewal through the browser profile runs now (not an hour later, when the
    /// cached access token finally runs out), and if it works the scope is asked for again and nobody notices; if it
    /// fails the sign-in is known to be gone (`sign_in_lost`), which makes the next call for credentials fail as
    /// "signed out" so that the person is told. Only after that is the scope left alone for a pause.
    fn token_locked(&self, scope: &str) -> Result<(String, i64)> {
        // A grant Entra refused a moment ago is not asked about again: the answer would be the same, and it would be
        // one more refused request for every caller that polls.
        if let Some(refused) = self.refused_recently(scope) {
            return Err(refused);
        }
        let renewals = self.renewals.load(Ordering::SeqCst);
        let mut result = self.attempt(scope);
        if scope != SPACES_SCOPE && matches!(&result, Err(Error::Auth(a)) if a.is_grant_problem()) {
            let host = scope_host(scope);
            log::warn!(
                "Entra refused the grant for {host}; checking whether the whole sign-in has expired"
            );
            let main = self.token_locked(SPACES_SCOPE);
            if self.renewals.load(Ordering::SeqCst) != renewals {
                log::info!("the Teams sign-in was renewed; asking again for {host}");
                result = self.attempt(scope);
            } else if main.is_err() {
                log::warn!(
                    "the main Teams grant is refused too, so the whole sign-in has expired, not just {host}"
                );
            }
        }
        self.note_outcome(scope, &result);
        result
    }

    /// One try at a token for `scope`: renew it, and for the main scope, recover through the browser if Entra refuses
    /// the grant.
    fn attempt(&self, scope: &str) -> Result<(String, i64)> {
        match self.refresh(scope) {
            Err(Error::Auth(AuthError::Rejected { code, .. }))
                if scope == SPACES_SCOPE && is_grant_code(&code) =>
            {
                log::warn!(
                    "the stored Teams grant was rejected ({code}); renewing through the browser profile"
                );
                self.recover().map_err(|renewal| {
                    log::warn!("renewal through the browser failed: {}", renewal.for_log());
                    AuthError::Expired {
                        code,
                        renewal: Box::new(renewal),
                    }
                })?;
                log::info!("the Teams sign-in was renewed through the browser profile");
                // If even the renewed grant is refused, another browser launch would not help: pause them.
                self.refresh(scope).inspect_err(|e| {
                    if matches!(e, Error::Auth(a) if a.is_grant_problem()) {
                        *lock(&self.paused_until) = Some(Instant::now() + RENEWAL_PAUSE);
                    }
                })
            }
            other => other,
        }
    }

    /// For tests elsewhere: stand in for the browser when the silent renewal runs.
    #[cfg(test)]
    pub(super) fn set_recover_hook_for_test(
        &self,
        hook: impl Fn(&TeamsAuth) -> Result<()> + Send + Sync + 'static,
    ) {
        *lock(&self.recover_hook) = Some(Box::new(hook));
    }

    /// For tests: the sign-in is good again (what adopting a renewed session does to the refusals).
    #[cfg(test)]
    pub(super) fn restore_sign_in_for_test(&self) {
        lock(&self.refusals).clear();
    }

    /// For tests elsewhere: the sign-in has been found dead.
    #[cfg(test)]
    pub(super) fn lose_sign_in_for_test(&self) {
        lock(&self.refusals).insert(
            SPACES_SCOPE.to_string(),
            Refusal {
                until: Instant::now() + RENEWAL_PAUSE,
                strikes: 1,
                code: "invalid_grant".into(),
                description: String::new(),
            },
        );
    }

    /// Whether the main grant has been found dead and not renewed since: the sign-in is gone, whatever the cached
    /// access tokens still allow for a while. Credentials are then refused at once, so that the person is told.
    pub(crate) fn sign_in_lost(&self) -> bool {
        lock(&self.refusals).contains_key(SPACES_SCOPE)
    }

    /// The refusal to give back, without asking Entra, for a scope whose grant was refused and is still paused: the
    /// same error the request itself would have ended in (for the spaces scope, the renewal that is paused).
    fn refused_recently(&self, scope: &str) -> Option<Error> {
        let refusals = lock(&self.refusals);
        let refusal = refusals.get(scope).filter(|r| Instant::now() < r.until)?;
        Some(if scope == SPACES_SCOPE {
            AuthError::Expired {
                code: refusal.code.clone(),
                renewal: Box::new(AuthError::RenewalPaused.into()),
            }
            .into()
        } else {
            AuthError::Rejected {
                code: refusal.code.clone(),
                description: refusal.description.clone(),
            }
            .into()
        })
    }

    /// Remember how a request for `scope` ended: a success forgets any refusal; a refused grant starts or lengthens
    /// the pause. Anything else (throttling, a network error, a configuration problem) says nothing about the grant
    /// and is left to the callers' own back-off.
    fn note_outcome(&self, scope: &str, result: &Result<(String, i64)>) {
        let (code, description) = match result {
            Ok(_) => {
                lock(&self.refusals).remove(scope);
                return;
            }
            Err(Error::Auth(AuthError::Rejected { code, description })) if is_grant_code(code) => {
                (code.clone(), description.clone())
            }
            Err(Error::Auth(AuthError::Expired { code, .. })) => (code.clone(), String::new()),
            Err(_) => return,
        };
        let mut refusals = lock(&self.refusals);
        let strikes = refusals
            .get(scope)
            .map_or(1, |r| r.strikes.saturating_add(1));
        // The spaces scope keeps its own five-minute rhythm (a browser launch is the cost there); the others double.
        let pause = if scope == SPACES_SCOPE {
            RENEWAL_PAUSE
        } else {
            REFUSAL_PAUSE
                .saturating_mul(1 << (strikes - 1).min(4))
                .min(REFUSAL_PAUSE_MAX)
        };
        log::warn!(
            "Entra refused the grant for {} ({code}); not asking again for {} minute(s)",
            scope_host(scope),
            pause.as_secs() / 60
        );
        refusals.insert(
            scope.to_string(),
            Refusal {
                until: Instant::now() + pause,
                strikes,
                code,
                description,
            },
        );
    }

    /// Access token for the chat service aggregator (conversation folders, which hold the favourites) and its
    /// lifetime in seconds. The spaces token is refused there.
    pub(crate) fn csa_token(&self) -> Result<(String, i64)> {
        self.token(CSA_SCOPE)
    }

    /// Access token for the presence service and its lifetime in seconds.
    pub(crate) fn presence_token(&self) -> Result<(String, i64)> {
        self.token(PRESENCE_SCOPE)
    }

    /// Access token for the Substrate search service (directory / people search) and its lifetime in seconds.
    pub(crate) fn substrate_token(&self) -> Result<(String, i64)> {
        self.token(SUBSTRATE_SCOPE)
    }

    /// Access token for Microsoft Graph (resolving a SharePoint/OneDrive attachment, `shares.rs`) and its lifetime
    /// in seconds.
    pub(crate) fn graph_token(&self) -> Result<(String, i64)> {
        self.token(GRAPH_SCOPE)
    }

    /// Access token for sending a message (see [`IC3_SCOPE`]) and its lifetime in seconds.
    pub(crate) fn ic3_token(&self) -> Result<(String, i64)> {
        self.token(IC3_SCOPE)
    }

    /// Fresh API credentials: renew the access token (recovering through the browser if Entra refuses), then authz.
    pub(crate) fn live(&self) -> Result<Live> {
        let (token, expires_in) = self.token(SPACES_SCOPE)?;
        let mut live = self.authz(&token)?;
        live.spaces_exp = unix_now().saturating_add(expires_in.max(0));
        let mut session = lock(&self.session);
        if session.0.name.is_empty()
            && let Some(name) =
                jwt_claims(&token).and_then(|c| c["name"].as_str().map(crate::clean))
        {
            session.0.name = name;
            self.persist(&session);
        }
        Ok(live)
    }

    /// Renew an access token with the stored refresh token, keeping a rotated one.
    fn refresh(&self, scope: &str) -> Result<(String, i64)> {
        let s = lock(&self.session).0.clone();
        let grant = self.exchange(&s, scope)?;
        if let Some(rotated) = grant.rotated {
            let mut cur = lock(&self.session);
            cur.0.refresh_token = rotated;
            self.persist(&cur);
        }
        Ok((grant.access_token, grant.expires_in))
    }

    /// Redeem the refresh token in `s` for an access token for `scope`. Stores nothing.
    fn exchange(&self, s: &Stored, scope: &str) -> Result<Grant> {
        const OPERATION: &str = "renew Teams sign-in";
        let url = format!("{}/{}/oauth2/v2.0/token", self.login_base, s.tenant);
        let mut resp = self
            .agent
            .post(&url)
            .header("Origin", &s.origin)
            .send_form([
                ("grant_type", "refresh_token"),
                ("client_id", s.client_id.as_str()),
                ("refresh_token", s.refresh_token.as_str()),
                ("scope", scope),
            ])
            .map_err(|e| Error::transport(OPERATION, e))?;
        // Throttling and server errors say nothing about the grant; only an OAuth `error` document does.
        http::transient_failure(OPERATION, &mut resp)?;
        let status = resp.status().as_u16();
        let text = http::body_text(OPERATION, &mut resp)?;
        let call = Call {
            agent: &self.agent,
            recorder: self.recorder.as_deref(),
            operation: OPERATION,
            method: "POST",
            url: &url,
            body: None,
        };
        http::record(&call, Some(status), &text);
        let v = http::parse_json(OPERATION, &text)?;
        let Some(token) = v["access_token"].as_str() else {
            let description = v["error_description"]
                .as_str()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            return Err(match v["error"].as_str() {
                Some(code) => AuthError::Rejected {
                    code: code.to_string(),
                    description,
                }
                .into(),
                None => Error::Status {
                    operation: OPERATION,
                    status,
                    message: description,
                },
            });
        };
        let rotated = v["refresh_token"].as_str();
        log::debug!(
            "access token renewed, good for {} s{}",
            v["expires_in"].as_i64().unwrap_or(3600),
            if rotated.is_some() {
                "; the refresh token was rotated"
            } else {
                ""
            }
        );
        Ok(Grant {
            access_token: token.to_string(),
            expires_in: v["expires_in"].as_i64().unwrap_or(3600),
            rotated: rotated.map(String::from),
        })
    }

    fn authz(&self, spaces_token: &str) -> Result<Live> {
        const OPERATION: &str = "authorise Teams session";
        let mut resp = self
            .agent
            .post(&self.authz_url)
            .header("Authorization", &format!("Bearer {spaces_token}"))
            .header("Origin", DEFAULT_ORIGIN)
            .send_json(serde_json::json!({}))
            .map_err(|e| Error::transport(OPERATION, e))?;
        let call = Call {
            agent: &self.agent,
            recorder: self.recorder.as_deref(),
            operation: OPERATION,
            method: "POST",
            url: &self.authz_url,
            body: None,
        };
        let text = http::check_recorded(&call, &mut resp)?;
        let v = http::parse_json(OPERATION, &text)?;
        let skype_token = v["tokens"]["skypeToken"]
            .as_str()
            .ok_or(Error::Unexpected {
                operation: OPERATION,
                detail: "no skypetoken in the answer",
            })?;
        let chat_base = v["regionGtms"]["chatServiceAfd"]
            .as_str()
            .unwrap_or("")
            .trim_end_matches('/');
        let mt_base = v["regionGtms"]["middleTier"]
            .as_str()
            .unwrap_or("")
            .trim_end_matches('/');
        let csa_base = v["regionGtms"]["chatSvcAggAfd"]
            .as_str()
            .unwrap_or("")
            .trim_end_matches('/');
        if !trusted_url(chat_base) {
            return Err(Error::UntrustedUrl {
                url: chat_base.to_string(),
            });
        }
        Ok(Live {
            spaces_token: spaces_token.to_string(),
            spaces_exp: 0,
            skype_token: skype_token.to_string(),
            skype_exp: unix_now()
                .saturating_add(v["tokens"]["expiresIn"].as_i64().unwrap_or(3600).max(0)),
            chat_base: chat_base.to_string(),
            mt_base: if trusted_url(mt_base) {
                mt_base.to_string()
            } else {
                String::new()
            },
            csa_base: if trusted_url(csa_base) {
                csa_base.to_string()
            } else {
                String::new()
            },
        })
    }

    /// Reopen the private browser profile headless; Teams' own SSO renews the MSAL cache without any interaction.
    /// A failure pauses further attempts for [`RENEWAL_PAUSE`].
    ///
    /// The profile holds a refresh token from the last time the page signed in, which is not the one we keep (we
    /// rotate ours) and is as old as the sign-in: until Teams' SSO has run it is just as dead. So a token differing
    /// from ours proves nothing; see [`Self::working_session`].
    fn recover(&self) -> Result<()> {
        if lock(&self.paused_until).is_some_and(|until| Instant::now() < until) {
            return Err(AuthError::RenewalPaused.into());
        }
        #[cfg(test)]
        if let Some(hook) = lock(&self.recover_hook).as_ref() {
            return hook(self);
        }
        let mut dead = HashSet::from([lock(&self.session).0.refresh_token.clone()]);
        let renewed = capture(&self.browser, true, SILENT_TIMEOUT, |origin, storage| {
            self.working_session(&mut dead, origin, storage)
        })
        .and_then(|fresh| self.adopt(fresh));
        *lock(&self.paused_until) = renewed.is_err().then(|| Instant::now() + RENEWAL_PAUSE);
        renewed
    }

    /// Sign in again in a visible browser window and wait for it (up to thirty minutes). For when the silent renewal has
    /// failed. Requests made meanwhile fail at once with [`AuthError::SignInInProgress`] rather than wait.
    pub(crate) fn reauthenticate(&self) -> Result<()> {
        /// Clears the flag however the sign-in ends.
        struct Open<'a>(&'a AtomicBool);
        impl Drop for Open<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        if self.signing_in.swap(true, Ordering::SeqCst) {
            return Err(AuthError::SignInInProgress.into());
        }
        let _open = Open(&self.signing_in);
        // Wait for a silent renewal in progress to finish: two browsers must not share one profile.
        let _single_flight = lock(&self.renewing);
        find_browser(self.browser.executable.as_deref())?;
        let mut dead = HashSet::from([lock(&self.session).0.refresh_token.clone()]);
        let fresh = capture(
            &self.browser,
            false,
            INTERACTIVE_TIMEOUT,
            |origin, storage| self.working_session(&mut dead, origin, storage),
        )?;
        self.adopt(fresh)?;
        *lock(&self.paused_until) = None;
        Ok(())
    }

    /// The sign-in in the browser's MSAL cache, once Entra honours its refresh token. The cache can hold an old token
    /// (the one the page signed in with, long since expired) until Teams' SSO replaces it, so each token is tried
    /// before it is accepted, and one that is refused is remembered in `dead` and not tried again. The session returned
    /// carries the token Entra rotated it to, if it did. Any other failure just means "not yet".
    fn working_session(
        &self,
        dead: &mut HashSet<String>,
        origin: &str,
        storage: &Map<String, Value>,
    ) -> Option<TeamsSession> {
        let ours = lock(&self.session).0.oid.clone();
        for mut candidate in sessions_from_storage(origin, storage, Some(&ours)) {
            if dead.contains(&candidate.0.refresh_token) {
                continue;
            }
            if !ours.is_empty() && candidate.0.oid != ours {
                continue;
            }
            match self.exchange(&candidate.0, SPACES_SCOPE) {
                Ok(grant) => {
                    if let Some(rotated) = grant.rotated {
                        candidate.0.refresh_token = rotated;
                    }
                    return Some(candidate);
                }
                Err(Error::Auth(a)) if a.is_grant_problem() => {
                    log::debug!(
                        "a refresh token in the browser is not good (yet); waiting for Teams to renew it"
                    );
                    dead.insert(candidate.0.refresh_token);
                }
                Err(e) => {
                    log::debug!(
                        "could not try a refresh token in the browser: {}",
                        e.for_log()
                    );
                }
            }
        }
        None
    }
    /// Take over a session read from the browser, if it is the same person; the name is kept if it has none.
    fn adopt(&self, fresh: TeamsSession) -> Result<()> {
        let mut cur = lock(&self.session);
        if fresh.0.oid != cur.0.oid {
            return Err(AuthError::WrongAccount.into());
        }
        let name = if fresh.0.name.is_empty() {
            cur.0.name.clone()
        } else {
            fresh.0.name.clone()
        };
        *cur = TeamsSession(Stored { name, ..fresh.0 });
        self.persist(&cur);
        // A new grant: whatever was refused before may be good now.
        lock(&self.refusals).clear();
        self.renewals.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use crate::testserver::{Reply, Server, serve};
    use serde_json::json;

    fn entry(v: Value) -> Value {
        Value::String(v.to_string())
    }

    fn jwt(claims: Value) -> String {
        format!("h.{}.s", URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    fn session() -> TeamsSession {
        TeamsSession(Stored {
            version: FORMAT_VERSION,
            client_id: "c".into(),
            tenant: "tenant-1".into(),
            oid: "oid-1".into(),
            name: "Ada".into(),
            refresh_token: "old-refresh".into(),
            origin: DEFAULT_ORIGIN.into(),
        })
    }

    /// Auth wired to a scripted server for both endpoints, with a browser that cannot be found.
    fn auth_against(server: &Server, store: Arc<dyn CredentialStore>) -> TeamsAuth {
        trust_for_tests(server.origin());
        TeamsAuth::new(
            session(),
            store,
            BrowserOptions::new("unused-profile").with_executable("definitely/not/a/browser.exe"),
        )
        .with_endpoints(server.origin(), &server.url("/v1.0/authz"))
    }

    #[test]
    fn reads_the_msal_cache() {
        let mut ls = Map::new();
        ls.insert("unrelated".into(), json!("plain text"));
        ls.insert("a".into(), entry(json!({"credentialType": "RefreshToken", "secret": "other-rt", "clientId": "other", "homeAccountId": "oid1.tid1"})));
        ls.insert("b".into(), entry(json!({"credentialType": "RefreshToken", "secret": "teams-rt", "clientId": TEAMS_CLIENT_ID, "homeAccountId": "oid1.tid1"})));
        ls.insert("c".into(), entry(json!({"credentialType": "AccessToken", "secret": "at", "realm": "tid1", "homeAccountId": "oid1.tid1"})));
        ls.insert("d".into(), entry(json!({"credentialType": "IdToken", "homeAccountId": "oid1.tid1", "secret": jwt(json!({"name": "Ada Lovelace"}))})));
        let s = session_from_storage("https://teams.cloud.microsoft", &ls)
            .unwrap()
            .0;
        assert_eq!(
            (
                s.refresh_token.as_str(),
                s.oid.as_str(),
                s.tenant.as_str(),
                s.name.as_str()
            ),
            ("teams-rt", "oid1", "tid1", "Ada Lovelace")
        );
        assert_eq!(s.origin, "https://teams.cloud.microsoft");
    }

    #[test]
    fn only_a_token_issued_to_the_teams_client_is_taken_for_the_sign_in() {
        // Another application signed in on the same page is not the Teams sign-in, and its token would be refused (or
        // worse, stored as ours) when redeemed for Teams' scopes.
        let mut ls = Map::new();
        ls.insert("a".into(), entry(json!({"credentialType": "RefreshToken", "secret": "other-rt", "clientId": "another-app", "homeAccountId": "oid1.tid1"})));
        assert!(session_from_storage("https://teams.microsoft.com", &ls).is_none());
    }

    #[test]
    fn every_teams_token_in_the_cache_is_a_candidate_and_the_signed_in_account_comes_first() {
        let mut ls = Map::new();
        for (key, secret, home) in [
            ("a", "rt-other-account", "oid2.tid1"),
            ("b", "rt-ours", "oid1.tid1"),
        ] {
            ls.insert(key.into(), entry(json!({"credentialType": "RefreshToken", "secret": secret, "clientId": TEAMS_CLIENT_ID, "homeAccountId": home})));
        }
        let origin = "https://teams.microsoft.com";
        let all = sessions_from_storage(origin, &ls, Some("oid1"));
        let tokens: Vec<&str> = all.iter().map(|s| s.0.refresh_token.as_str()).collect();
        assert_eq!(
            tokens,
            ["rt-ours", "rt-other-account"],
            "the account being signed in again first, the other still there to be tried"
        );
        // With no account to prefer, the order is the cache's own and still deterministic.
        let first = session_from_storage(origin, &ls).unwrap();
        assert_eq!(first.0.refresh_token, "rt-other-account");
    }

    #[test]
    fn a_refused_token_does_not_stop_the_next_one_in_the_cache_being_tried() {
        let origin = "https://teams.microsoft.com";
        let server = serve(vec![
            Reply::status(400)
                .json(r#"{"error":"invalid_grant","error_description":"AADSTS700084: expired"}"#),
            Reply::status(200)
                .json(r#"{"access_token":"at","refresh_token":"rotated-good","expires_in":3600}"#),
        ]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        let mut ls = Map::new();
        // "stale" sorts before "zfresh" in the cache; both are for the signed-in account.
        for (key, secret) in [("a", "stale"), ("b", "zfresh")] {
            ls.insert(key.into(), entry(json!({"credentialType": "RefreshToken", "secret": secret, "clientId": TEAMS_CLIENT_ID, "homeAccountId": "oid-1.tid1"})));
        }
        ls.insert("c".into(), entry(json!({"credentialType": "AccessToken", "secret": "at", "realm": "tid1", "homeAccountId": "oid-1.tid1"})));
        let mut dead = HashSet::new();
        let got = auth
            .working_session(&mut dead, origin, &ls)
            .expect("the second token works");
        assert_eq!(got.0.refresh_token, "rotated-good");
        assert!(dead.contains("stale"));
    }

    #[test]
    fn another_accounts_token_is_not_redeemed_when_ours_fails() {
        let origin = "https://teams.microsoft.com";
        let server = serve(vec![
            Reply::status(400)
                .json(r#"{"error":"invalid_grant","error_description":"AADSTS700084: expired"}"#),
            Reply::status(200)
                .json(r#"{"access_token":"at","refresh_token":"rotated-other","expires_in":3600}"#),
        ]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        let mut ls = Map::new();
        // Our account (oid-1) comes first, another account second.
        for (key, secret, home) in [
            ("a", "rt-ours", "oid-1.tid1"),
            ("b", "rt-other", "oid-2.tid1"),
        ] {
            ls.insert(key.into(), entry(json!({"credentialType": "RefreshToken", "secret": secret, "clientId": TEAMS_CLIENT_ID, "homeAccountId": home})));
        }
        ls.insert("c".into(), entry(json!({"credentialType": "AccessToken", "secret": "at", "realm": "tid1", "homeAccountId": "oid-1.tid1"})));
        let mut dead = HashSet::new();
        let result = auth.working_session(&mut dead, origin, &ls);
        assert!(
            result.is_none(),
            "another account's session must not be returned"
        );
        assert_eq!(server.seen().len(), 1, "only our account's token was tried");
        assert!(dead.contains("rt-ours"));
    }

    #[test]
    fn no_refresh_token_means_not_signed_in_yet() {
        let mut ls = Map::new();
        ls.insert("c".into(), entry(json!({"credentialType": "AccessToken", "secret": "at", "realm": "t", "homeAccountId": "o.t"})));
        assert!(session_from_storage("https://teams.microsoft.com", &ls).is_none());
        assert!(session_from_storage("https://teams.microsoft.com", &Map::new()).is_none());
    }

    fn cache_with(refresh_token: &str) -> Map<String, Value> {
        let mut ls = Map::new();
        ls.insert("b".into(), entry(json!({"credentialType": "RefreshToken", "secret": refresh_token, "clientId": TEAMS_CLIENT_ID, "homeAccountId": "oid-1.tid1"})));
        ls.insert("c".into(), entry(json!({"credentialType": "AccessToken", "secret": "at", "realm": "tid1", "homeAccountId": "oid-1.tid1"})));
        ls
    }

    #[test]
    fn a_refresh_token_in_the_browser_is_accepted_only_once_entra_honours_it() {
        let origin = "https://teams.microsoft.com";
        let dead_reply = || {
            Reply::status(400)
                .json(r#"{"error":"invalid_grant","error_description":"AADSTS700084: expired"}"#)
        };
        // The cache holds the page's old token (a different string from the one vtteams rotated, but just as dead), then
        // after Teams' SSO a new one, which Entra rotates again.
        let server = serve(vec![
            dead_reply(),
            Reply::status(200)
                .json(r#"{"access_token":"at","refresh_token":"rotated-fresh","expires_in":3600}"#),
            Reply::status(503).body("down"),
        ]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        let mut dead = HashSet::from(["ours".to_string()]);
        // Our own (rotated) token is never tried again: no request.
        assert!(
            auth.working_session(&mut dead, origin, &cache_with("ours"))
                .is_none()
        );
        assert!(server.seen().is_empty());
        // The page's old token differs from ours but Entra refuses it: not a renewal, and not tried a second time.
        assert!(
            auth.working_session(&mut dead, origin, &cache_with("page-old"))
                .is_none()
        );
        assert!(
            auth.working_session(&mut dead, origin, &cache_with("page-old"))
                .is_none()
        );
        assert_eq!(server.seen().len(), 1);
        assert!(dead.contains("page-old"));
        // The token Teams stored after signing in again works, and the session carries the rotated one.
        let renewed = auth
            .working_session(&mut dead, origin, &cache_with("page-new"))
            .unwrap();
        assert_eq!(renewed.0.refresh_token, "rotated-fresh");
        assert!(server.seen()[1].body.contains("refresh_token=page-new"));
        // A failure that says nothing about the token (here a 503) is "not yet", and the token is tried again.
        assert!(
            auth.working_session(&mut dead, origin, &cache_with("page-newer"))
                .is_none()
        );
        assert!(!dead.contains("page-newer"));
        assert!(
            auth.working_session(&mut dead, origin, &Map::new())
                .is_none()
        );
    }
    #[test]
    fn while_a_sign_in_waits_in_the_browser_requests_fail_at_once() {
        // Nothing is scripted: any request to the server would come back as a 500 and show up in `seen`.
        let server = serve(vec![]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        auth.signing_in.store(true, Ordering::SeqCst);
        let started = Instant::now();
        assert!(matches!(
            auth.token(SPACES_SCOPE),
            Err(Error::Auth(AuthError::SignInInProgress))
        ));
        assert!(matches!(
            auth.live(),
            Err(Error::Auth(AuthError::SignInInProgress))
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(server.seen().is_empty());
        // A second sign-in cannot start on top of the first.
        assert!(matches!(
            auth.reauthenticate(),
            Err(Error::Auth(AuthError::SignInInProgress))
        ));
        assert!(
            !Error::Auth(AuthError::SignInInProgress).needs_sign_in(),
            "waiting for one is not a reason to start another"
        );
    }

    #[test]
    fn a_sign_in_that_cannot_open_a_browser_says_so_and_leaves_requests_working() {
        let server = serve(vec![
            Reply::status(200).json(r#"{"access_token":"at","expires_in":3600}"#),
        ]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        assert!(matches!(
            auth.reauthenticate(),
            Err(Error::Browser(crate::BrowserError::MissingExecutable(_)))
        ));
        assert!(
            !auth.signing_in.load(Ordering::SeqCst),
            "the flag is cleared whatever happened"
        );
        assert_eq!(auth.token(SPACES_SCOPE).unwrap().0, "at");
    }
    #[test]
    fn hostile_ids_are_rejected() {
        let mut ls = Map::new();
        ls.insert("b".into(), entry(json!({"credentialType": "RefreshToken", "secret": "rt", "clientId": "c", "homeAccountId": "oid.evil.example/x?"})));
        assert!(session_from_storage("https://teams.microsoft.com", &ls).is_none());
    }

    /// The tenant and the account go into the token endpoint's path, where `.` and `..` are steps to another place.
    #[test]
    fn dot_ids_are_rejected_when_read_from_the_cache_and_when_loaded() {
        for tenant in [".", ".."] {
            let mut ls = Map::new();
            ls.insert(
                "a".into(),
                entry(json!({"credentialType": "RefreshToken", "secret": "rt", "clientId": TEAMS_CLIENT_ID, "homeAccountId": "oid-1.good"})),
            );
            ls.insert(
                "b".into(),
                entry(json!({"credentialType": "AccessToken", "realm": tenant, "homeAccountId": "oid-1.good"})),
            );
            assert!(
                session_from_storage("https://teams.microsoft.com", &ls).is_none(),
                "tenant {tenant:?}"
            );
        }
        let mut ls = Map::new();
        ls.insert(
            "a".into(),
            entry(json!({"credentialType": "RefreshToken", "secret": "rt", "clientId": TEAMS_CLIENT_ID, "homeAccountId": "oid-1.good"})),
        );
        assert!(
            session_from_storage("https://teams.microsoft.com", &ls).is_some(),
            "the same cache with an ordinary tenant is a session"
        );

        let store = MemoryStore::new();
        let doc = |tenant: &str, oid: &str| {
            format!(
                r#"{{"client_id":"c","tenant":"{tenant}","oid":"{oid}","name":"n","refresh_token":"r","origin":"https://teams.microsoft.com"}}"#
            )
        };
        for (tenant, oid) in [
            ("..", "o"),
            (".", "o"),
            ("t", ".."),
            ("t", "."),
            ("t/x", "o"),
        ] {
            store.save(doc(tenant, oid).as_bytes()).unwrap();
            assert!(
                matches!(TeamsSession::load(&store), Err(StoreError::Corrupt { .. })),
                "{tenant:?} {oid:?}"
            );
        }
        store
            .save(doc("contoso.onmicrosoft.com", "o-1").as_bytes())
            .unwrap();
        assert!(TeamsSession::load(&store).unwrap().is_some());
    }

    #[test]
    fn only_teams_hosts_are_trusted() {
        assert!(trusted_url("https://teams.microsoft.com/api/chatsvc/amer"));
        assert!(trusted_url("https://amer.ng.msg.teams.microsoft.com/v1/x"));
        assert!(!trusted_url("http://teams.microsoft.com/x"));
        assert!(!trusted_url("https://teams.microsoft.com.evil.example/x"));
        assert!(!trusted_url("https://evilteams.microsoft.com.example/"));
        assert!(!trusted_url("not a url"));
    }

    #[test]
    fn documents_without_a_version_load_and_newer_ones_are_refused() {
        let store = MemoryStore::new();
        let doc = |extra: &str| {
            format!(
                r#"{{{extra}"client_id":"c","tenant":"t","oid":"o","name":"n","refresh_token":"r","origin":"https://teams.microsoft.com"}}"#
            )
        };
        store.save(doc("").as_bytes()).unwrap();
        assert!(
            TeamsSession::load(&store).unwrap().is_some(),
            "a document without a version is version 1"
        );
        store.save(doc(r#""version":99,"#).as_bytes()).unwrap();
        assert!(matches!(
            TeamsSession::load(&store),
            Err(StoreError::UnsupportedVersion {
                found: 99,
                supported: 1
            })
        ));
        session().save(&store).unwrap();
        assert!(
            String::from_utf8(store.load().unwrap().unwrap())
                .unwrap()
                .contains("\"version\": 1")
        );
    }

    #[test]
    fn sessions_round_trip_through_a_store_and_debug_hides_the_token() {
        let store = MemoryStore::new();
        assert!(TeamsSession::load(&store).unwrap().is_none());
        session().save(&store).unwrap();
        let back = TeamsSession::load(&store).unwrap().unwrap();
        assert_eq!((back.user_id(), back.display_name()), ("oid-1", "Ada"));
        assert!(!format!("{back:?}").contains("old-refresh"));
    }

    #[test]
    fn a_failed_save_becomes_a_warning_not_an_error() {
        struct Broken;
        impl CredentialStore for Broken {
            fn load(&self) -> Result<Option<Vec<u8>>, StoreError> {
                Ok(None)
            }
            fn save(&self, _: &[u8]) -> Result<(), StoreError> {
                Err(StoreError::Io {
                    action: "write",
                    path: "x".into(),
                    source: std::io::Error::other("disk full"),
                })
            }
            fn clear(&self) -> Result<bool, StoreError> {
                Ok(false)
            }
        }
        let auth = TeamsAuth::new(session(), Arc::new(Broken), BrowserOptions::new("unused"));
        auth.persist(&session());
        let warnings = auth.take_warnings();
        assert!(
            matches!(warnings.as_slice(), [Error::Store(StoreError::Io { .. })]),
            "{warnings:?}"
        );
        assert!(
            auth.take_warnings().is_empty(),
            "taken warnings are cleared"
        );
    }

    #[test]
    fn a_refresh_returns_the_token_and_persists_a_rotated_refresh_token() {
        let server = serve(vec![Reply::status(200).json(
            r#"{"access_token":"at","refresh_token":"rotated","expires_in":1800}"#,
        )]);
        let store = Arc::new(MemoryStore::new());
        let auth = auth_against(&server, store.clone());
        assert_eq!(auth.token(SPACES_SCOPE).unwrap(), ("at".to_string(), 1800));
        let seen = server.seen();
        assert_eq!(seen[0].path, "/tenant-1/oauth2/v2.0/token");
        assert_eq!(seen[0].header("origin"), Some(DEFAULT_ORIGIN));
        assert!(
            seen[0].body.contains("refresh_token=old-refresh")
                && seen[0]
                    .body
                    .contains("scope=https%3A%2F%2Fapi.spaces.skype.com"),
            "{}",
            seen[0].body
        );
        let saved = TeamsSession::load(store.as_ref()).unwrap().unwrap();
        assert_eq!(saved.0.refresh_token, "rotated");
    }

    #[test]
    fn throttling_and_server_errors_do_not_start_a_browser_recovery() {
        let server = serve(vec![
            Reply::status(429).header("Retry-After", "9").body(""),
            Reply::status(503).body("down"),
            Reply::status(400).json(
                r#"{"error":"invalid_request","error_description":"AADSTS9002327: bad origin"}"#,
            ),
        ]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        assert!(matches!(
            auth.token(SPACES_SCOPE),
            Err(Error::Throttled {
                retry_after: Some(_),
                ..
            })
        ));
        assert!(matches!(
            auth.token(SPACES_SCOPE),
            Err(Error::Status { status: 503, .. })
        ));
        // A configuration problem is reported as such; a browser would not fix it.
        match auth.token(SPACES_SCOPE) {
            Err(Error::Auth(a)) => assert!(
                matches!(a, AuthError::Rejected { .. }) && !a.is_grant_problem(),
                "{a:?}"
            ),
            other => panic!("{other:?}"),
        }
        assert!(
            lock(&auth.paused_until).is_none(),
            "no recovery was attempted"
        );
    }

    #[test]
    fn a_grant_rejection_on_the_csa_substrate_or_graph_scope_does_not_start_a_browser_recovery() {
        // A tenant that never granted the CSA, Substrate or Graph scope (a restricted cloud withholding Graph,
        // say) rejects it the same way an expired grant is rejected; none is the spaces scope every poll needs, so
        // a browser launch would not help and would make the whole session look signed out. Only `SPACES_SCOPE`
        // triggers recovery.
        let dead = || {
            Reply::status(400)
                .json(r#"{"error":"invalid_grant","error_description":"AADSTS700084: expired"}"#)
        };
        // Each refusal is followed by one request for the main scope, to tell a withheld scope from a dead grant; the
        // main scope is fine here, so nothing else happens.
        let main = || Reply::status(200).json(r#"{"access_token":"spaces","expires_in":3600}"#);
        let server = serve(vec![dead(), main(), dead(), main(), dead(), main()]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        match auth.token(CSA_SCOPE) {
            Err(Error::Auth(a)) => assert!(a.is_grant_problem(), "{a:?}"),
            other => panic!("{other:?}"),
        }
        match auth.token(SUBSTRATE_SCOPE) {
            Err(Error::Auth(a)) => assert!(a.is_grant_problem(), "{a:?}"),
            other => panic!("{other:?}"),
        }
        match auth.token(GRAPH_SCOPE) {
            Err(Error::Auth(a)) => assert!(a.is_grant_problem(), "{a:?}"),
            other => panic!("{other:?}"),
        }
        assert!(
            lock(&auth.paused_until).is_none(),
            "no recovery was attempted, and the spaces scope's renewals are not paused"
        );
        assert_eq!(
            server.seen().len(),
            6,
            "a refusal and a comparison each, and no browser was launched"
        );
        assert!(
            !auth.sign_in_lost(),
            "ordinary chat reading is not affected"
        );
    }

    #[test]
    fn a_dead_grant_tries_one_silent_renewal_and_then_pauses() {
        let dead = || {
            Reply::status(400)
                .json(r#"{"error":"invalid_grant","error_description":"AADSTS700084: expired"}"#)
        };
        let server = serve(vec![dead(), dead()]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        // The browser cannot be found, so the first renewal fails with that reason...
        match auth.token(SPACES_SCOPE) {
            Err(Error::Auth(AuthError::Expired { code, renewal })) => {
                assert_eq!(code, "invalid_grant");
                assert!(
                    matches!(
                        *renewal,
                        Error::Browser(crate::BrowserError::MissingExecutable(_))
                    ),
                    "{renewal:?}"
                );
            }
            other => panic!("{other:?}"),
        }
        // ...and the next poll does not try the browser again, nor ask Entra again about a grant it just refused.
        match auth.token(SPACES_SCOPE) {
            Err(Error::Auth(AuthError::Expired { renewal, .. })) => assert!(
                matches!(*renewal, Error::Auth(AuthError::RenewalPaused)),
                "{renewal:?}"
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            server.seen().len(),
            1,
            "the second poll did not send a request that was bound to be refused"
        );
        assert!(
            Error::Auth(AuthError::Expired {
                code: String::new(),
                renewal: Box::new(AuthError::RenewalPaused.into())
            })
            .needs_sign_in()
        );
    }

    fn dead_grant() -> Reply {
        Reply::status(400)
            .json(r#"{"error":"invalid_grant","error_description":"AADSTS700084: expired"}"#)
    }

    fn good_token(token: &str) -> Reply {
        Reply::status(200).json(&format!(
            r#"{{"access_token":"{token}","expires_in":3600}}"#
        ))
    }

    /// Pretend the pause on `scope` is over, so that the next request goes to the server again.
    fn end_pause(auth: &TeamsAuth, scope: &str) {
        let mut refusals = lock(&auth.refusals);
        let refusal = refusals.get_mut(scope).expect("the scope was refused");
        refusal.until = Instant::now() - Duration::from_secs(1);
    }

    /// Seen live: with the grant dead, presence, activity reports and sends each asked Entra again on their own
    /// schedule, about three refused requests a minute for as long as it lasted.
    #[test]
    fn a_scope_whose_grant_was_refused_is_not_asked_about_again_until_a_pause_is_over() {
        // Each time Entra is asked, the refusal is followed by a request for the main scope (a good one: it is only
        // this scope that is withheld).
        let server = serve(vec![
            dead_grant(),
            good_token("spaces"),
            dead_grant(),
            good_token("spaces"),
        ]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        let refusal = |r: Result<(String, i64)>| match r {
            Err(Error::Auth(AuthError::Rejected { code, .. })) => code,
            other => panic!("{other:?}"),
        };
        assert_eq!(refusal(auth.presence_token()), "invalid_grant");
        // However often the callers ask, in the pause nothing more is sent, and the answer is the same refusal.
        for _ in 0..5 {
            assert_eq!(refusal(auth.presence_token()), "invalid_grant");
        }
        assert_eq!(
            server.seen().len(),
            2,
            "the scope and the comparison, not twelve requests"
        );
        // The pause is over: one more try, and a longer pause after it (five minutes, then ten).
        end_pause(&auth, PRESENCE_SCOPE);
        assert_eq!(refusal(auth.presence_token()), "invalid_grant");
        assert_eq!(server.seen().len(), 4);
        let pause = lock(&auth.refusals)[PRESENCE_SCOPE].until - Instant::now();
        assert!(
            pause > Duration::from_secs(9 * 60) && pause <= Duration::from_secs(10 * 60),
            "{pause:?}"
        );
    }

    #[test]
    fn the_pause_doubles_up_to_an_hour_and_each_scope_has_its_own() {
        // A refusal and the comparison with the (good) main scope, every time.
        let server = serve(
            (0..8)
                .flat_map(|_| [dead_grant(), good_token("spaces")])
                .collect(),
        );
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        let mut minutes = Vec::new();
        for _ in 0..6 {
            let _ = auth.presence_token();
            minutes.push(
                (lock(&auth.refusals)[PRESENCE_SCOPE].until - Instant::now()).as_secs() / 60 + 1,
            );
            end_pause(&auth, PRESENCE_SCOPE);
        }
        assert_eq!(minutes, [5, 10, 20, 40, 60, 60]);
        // Another scope was never refused, so it is asked at once (and compared with the main scope).
        let before = server.seen().len();
        let _ = auth.ic3_token();
        assert_eq!(server.seen().len(), before + 2);
        assert!(lock(&auth.refusals).contains_key(IC3_SCOPE));
    }

    #[test]
    fn a_success_or_a_new_sign_in_ends_the_pause_and_starts_the_count_over() {
        let server = serve(vec![
            dead_grant(),
            good_token("spaces"), // the comparison with the main scope
            good_token("fresh"),
            dead_grant(),
            good_token("spaces"),
            dead_grant(),
            good_token("spaces"),
        ]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        let _ = auth.presence_token();
        end_pause(&auth, PRESENCE_SCOPE);
        assert_eq!(auth.presence_token().unwrap().0, "fresh");
        assert!(
            lock(&auth.refusals).is_empty(),
            "a token forgets the refusal"
        );
        // Refused again: the count starts at one, so the pause is five minutes and not ten.
        let _ = auth.presence_token();
        assert_eq!(lock(&auth.refusals)[PRESENCE_SCOPE].strikes, 1);
        // A session adopted from the browser (a sign-in again) forgets every refusal at once.
        let _ = auth.ic3_token();
        assert_eq!(lock(&auth.refusals).len(), 2);
        auth.adopt(session()).unwrap();
        assert!(lock(&auth.refusals).is_empty());
    }

    /// Only a refused grant is remembered. Throttling and server errors say nothing about the grant, and a
    /// configuration problem is not fixed by waiting: the callers' own back-off deals with those.
    #[test]
    fn other_failures_are_not_remembered_as_refusals() {
        let server = serve(vec![
            Reply::status(429).header("Retry-After", "9").body(""),
            Reply::status(503).body("down"),
            Reply::status(400).json(
                r#"{"error":"invalid_request","error_description":"AADSTS9002327: bad origin"}"#,
            ),
            good_token("later"),
        ]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        for _ in 0..3 {
            assert!(auth.presence_token().is_err());
        }
        assert!(lock(&auth.refusals).is_empty());
        assert_eq!(
            auth.presence_token().unwrap().0,
            "later",
            "every one of them was asked"
        );
        assert_eq!(server.seen().len(), 4);
    }

    fn renewed_session() -> TeamsSession {
        let mut s = session();
        s.0.refresh_token = "renewed".into();
        s
    }

    /// A refused scope may be that scope alone (a tenant withholding presence, say) or the whole grant gone. The main
    /// scope is asked at once to tell which, and when it is fine there is no browser and no "signed out".
    #[test]
    fn a_refused_side_scope_with_a_good_main_grant_is_left_at_that_and_does_not_look_signed_out() {
        let server = serve(vec![dead_grant(), good_token("spaces")]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        match auth.presence_token() {
            Err(Error::Auth(a)) => assert!(a.is_grant_problem(), "{a:?}"),
            other => panic!("{other:?}"),
        }
        let seen = server.seen();
        assert_eq!(seen.len(), 2, "the scope, then the main scope to compare");
        assert!(
            seen[1].body.contains("api.spaces.skype.com"),
            "{}",
            seen[1].body
        );
        assert!(!auth.sign_in_lost());
        assert!(
            lock(&auth.paused_until).is_none(),
            "no browser was launched"
        );
        let refusals = lock(&auth.refusals);
        assert!(refusals.contains_key(PRESENCE_SCOPE) && !refusals.contains_key(SPACES_SCOPE));
    }

    /// First evidence of a dead grant, on a side scope: renew the sign-in now, through the browser profile, and when
    /// that works ask again for what was refused, so that nothing is noticed.
    #[test]
    fn a_dead_grant_found_through_a_side_scope_is_renewed_at_once_and_the_scope_asked_again() {
        let server = serve(vec![
            dead_grant(),           // presence
            dead_grant(),           // the main scope, to compare: the whole grant is dead
            good_token("spaces"),   // the main scope, with the renewed grant
            good_token("presence"), // presence again
        ]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        *lock(&auth.recover_hook) = Some(Box::new(|a: &TeamsAuth| a.adopt(renewed_session())));
        assert_eq!(
            auth.presence_token().unwrap().0,
            "presence",
            "the caller gets its token as if nothing had happened"
        );
        let seen = server.seen();
        assert_eq!(seen.len(), 4);
        assert!(
            seen[0].body.contains("refresh_token=old-refresh"),
            "{}",
            seen[0].body
        );
        for later in &seen[2..] {
            assert!(
                later.body.contains("refresh_token=renewed"),
                "{}",
                later.body
            );
        }
        assert!(
            seen[3].body.contains("presence.teams.microsoft.com"),
            "{}",
            seen[3].body
        );
        assert!(!auth.sign_in_lost());
        assert!(lock(&auth.refusals).is_empty());
    }

    /// When the renewal fails too, the sign-in is known to be gone: the person has to be told (the next call for
    /// credentials fails as signed out), and only after that is anything left alone for a pause.
    #[test]
    fn a_dead_grant_that_cannot_be_renewed_is_marked_so_that_the_person_is_told() {
        let server = serve(vec![dead_grant(), dead_grant(), dead_grant()]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        assert!(auth.presence_token().is_err());
        assert!(
            auth.sign_in_lost(),
            "the whole sign-in is gone, not only presence"
        );
        assert!(lock(&auth.paused_until).is_some(), "the renewal was tried");
        match auth.live() {
            Err(e) => assert!(e.needs_sign_in(), "{e:?}"),
            Ok(_) => panic!("credentials must be refused once the sign-in is gone"),
        }
        // Another scope is asked about once; the main scope is not asked again while its renewal is paused.
        let before = server.seen().len();
        assert!(auth.ic3_token().is_err());
        assert_eq!(server.seen().len(), before + 1);
        // Signing in again clears it.
        auth.adopt(renewed_session()).unwrap();
        assert!(!auth.sign_in_lost());
    }

    #[test]
    fn authz_names_the_chat_service_and_refuses_untrusted_ones() {
        // `{origin}` in a scripted body becomes the test server's own address.
        let server = serve(vec![
            Reply::status(200).json(r#"{"tokens":{"skypeToken":"skype","expiresIn":3600},"regionGtms":{"chatServiceAfd":"{origin}/api/chatsvc/amer/","middleTier":"https://evil.example/mt"}}"#),
            Reply::status(200).json(r#"{"tokens":{"skypeToken":"s"},"regionGtms":{"chatServiceAfd":"https://evil.example/api"}}"#),
            Reply::status(200).json(r#"{"tokens":{},"regionGtms":{}}"#),
            Reply::status(403).json(r#"{"message":"nope"}"#),
        ]);
        let auth = auth_against(&server, Arc::new(MemoryStore::new()));
        let live = auth.authz("spaces").unwrap();
        assert_eq!(
            (live.skype_token.as_str(), live.chat_base.as_str()),
            (
                "skype",
                format!("{}/api/chatsvc/amer", server.origin()).as_str()
            )
        );
        assert!(
            live.mt_base.is_empty(),
            "an untrusted middle tier is dropped"
        );
        assert_eq!(
            server.seen()[0].header("authorization"),
            Some("Bearer spaces")
        );
        assert!(
            matches!(auth.authz("spaces"), Err(Error::UntrustedUrl { .. })),
            "an untrusted chat service is refused"
        );
        assert!(
            matches!(auth.authz("spaces"), Err(Error::Unexpected { .. })),
            "no skypetoken"
        );
        assert!(matches!(
            auth.authz("spaces"),
            Err(Error::Status { status: 403, .. })
        ));
    }
}
