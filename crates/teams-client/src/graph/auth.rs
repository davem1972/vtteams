//! Microsoft Graph sign-in: the OAuth2 device-code flow for the Microsoft identity platform v2, and refresh tokens.

use crate::error::{AuthError, Error, Result, StoreError};
use crate::headerlog::HeaderLog;
use crate::http;
use crate::store::{
    CredentialStore, FORMAT_VERSION, check_version, format_version, load_json, save_json,
};
use crate::unix_now;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::time::Duration;

/// "Microsoft Graph Command Line Tools": a Microsoft-published public client that supports the device-code flow.
/// A tenant may block it; register your own app (public client, "Allow public client flows") and use its id.
pub const DEFAULT_CLIENT_ID: &str = "14d82eec-204b-4c2f-b7e8-296a70dab67e";
/// Any work or school account.
pub const DEFAULT_TENANT: &str = "organizations";
/// Delegated permissions requested: read the profile and other users' basic profiles (people search), and read and
/// write the user's chats.
pub const SCOPES: &str = "offline_access User.Read User.ReadBasic.All Chat.ReadWrite";

const LOGIN_BASE: &str = "https://login.microsoftonline.com";
/// How long one sign-in or token request may take altogether.
const AGENT_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait we back off to after transient failures while polling.
const MAX_POLL_SECS: u64 = 30;
/// How much longer to wait after a transient failure or a `slow_down` request (tests do not wait).
const BACKOFF_STEP_SECS: u64 = if cfg!(test) { 0 } else { 5 };

/// Which Azure app and tenant to sign in with.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct GraphConfig {
    /// Application (client) id of a public client with the device-code flow enabled.
    pub client_id: String,
    /// Tenant id or domain, or `organizations`.
    pub tenant: String,
}

impl GraphConfig {
    /// A configuration for `client_id` in `tenant`.
    pub fn new(client_id: impl Into<String>, tenant: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            tenant: tenant.into(),
        }
    }
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self::new(DEFAULT_CLIENT_ID, DEFAULT_TENANT)
    }
}

/// What to show the user during a device-code sign-in.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DeviceCodePrompt {
    /// The page to open.
    pub verification_uri: String,
    /// The code to enter there.
    pub user_code: String,
    /// Microsoft's own instructions, when it sent some.
    pub message: Option<String>,
}

/// The stored form. Private, so the serialisation format is not part of this crate's API.
#[derive(Clone, Serialize, Deserialize)]
struct Stored {
    /// Format of this document; see [`FORMAT_VERSION`].
    #[serde(default = "format_version")]
    version: u32,
    access_token: String,
    refresh_token: Option<String>,
    /// Unix seconds.
    expires_at: i64,
    client_id: String,
}

/// An access token with its refresh token. `Debug` hides the secrets.
#[derive(Clone)]
pub struct GraphTokens(Stored);

impl fmt::Debug for GraphTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GraphTokens")
            .field("expires_at", &self.0.expires_at)
            .field("client_id", &self.0.client_id)
            .finish_non_exhaustive()
    }
}

impl GraphTokens {
    /// Tokens that are fresh and not tied to any real sign-in, for tests.
    #[cfg(test)]
    pub(crate) fn for_test(access_token: &str) -> Self {
        Self(Stored {
            version: FORMAT_VERSION,
            access_token: access_token.to_string(),
            refresh_token: Some("test-refresh".into()),
            expires_at: unix_now() + 3600,
            client_id: DEFAULT_CLIENT_ID.to_string(),
        })
    }

    /// Expired tokens for testing refresh failures.
    #[cfg(test)]
    pub(crate) fn expired_for_test(access_token: &str) -> Self {
        Self(Stored {
            version: FORMAT_VERSION,
            access_token: access_token.to_string(),
            refresh_token: Some("test-refresh".into()),
            expires_at: unix_now() - 100,
            client_id: DEFAULT_CLIENT_ID.to_string(),
        })
    }

    /// The bearer token for Graph requests.
    pub(crate) fn access_token(&self) -> &str {
        &self.0.access_token
    }

    #[cfg(test)]
    pub(crate) fn refresh_token(&self) -> Option<&str> {
        self.0.refresh_token.as_deref()
    }

    /// True when the access token is usable for at least another minute.
    #[must_use]
    pub fn fresh(&self) -> bool {
        unix_now().saturating_add(60) < self.0.expires_at
    }

    /// Extract the tenant ID from the access token's `tid` claim. Returns `None` if the token cannot be decoded
    /// or the claim is missing (the token is a JWT whose payload is base64-encoded JSON, but this makes no
    /// assumptions about its validity or signature).
    #[must_use]
    pub fn tenant_id(&self) -> Option<String> {
        tenant_from_jwt(&self.0.access_token)
    }

    /// The signed-in user's Entra object id, from the access token's `oid` claim (read the same way as
    /// [`GraphTokens::tenant_id`]: no signature is checked). `None` when the token cannot be decoded or has no such
    /// claim. It lets an application tell accounts apart before any request has been answered.
    #[must_use]
    pub fn user_id(&self) -> Option<String> {
        jwt_claim(&self.0.access_token, "oid")
    }

    /// Read tokens saved by [`GraphTokens::save`].
    ///
    /// # Errors
    /// [`StoreError`] if the store cannot be read or its contents are not valid.
    pub fn load(store: &dyn CredentialStore) -> Result<Option<Self>, StoreError> {
        let stored = load_json::<Stored>(store)?;
        if let Some(s) = &stored {
            check_version(s.version)?;
        }
        Ok(stored.map(Self))
    }

    /// Save the tokens.
    ///
    /// # Errors
    /// [`StoreError`] if the store cannot be written.
    pub fn save(&self, store: &dyn CredentialStore) -> Result<(), StoreError> {
        save_json(store, &self.0)
    }
}

#[derive(Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
    #[serde(default)]
    message: Option<String>,
}

fn default_interval() -> u64 {
    5
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    error: Option<String>,
    error_description: Option<String>,
}

impl TokenResponse {
    /// The identity provider's refusal, as a typed error.
    fn rejection(self) -> AuthError {
        AuthError::Rejected {
            code: self.error.unwrap_or_else(|| "unknown_error".into()),
            description: self
                .error_description
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or("")
                .to_string(),
        }
    }
}

/// Extract the `tid` (tenant ID) claim from a JWT access token. A JWT is three base64-encoded parts separated by dots;
/// the middle one is the payload. Returns `None` if the token is not three parts, the payload cannot be decoded, the
/// JSON cannot be parsed, or the `tid` claim is missing or not a string. Does not verify the token's signature.
fn tenant_from_jwt(token: &str) -> Option<String> {
    jwt_claim(token, "tid")
}

/// The string claim `name` of a JWT's payload, read without checking the signature (see [`tenant_from_jwt`]).
fn jwt_claim(token: &str, name: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let json: Value = serde_json::from_slice(&payload).ok()?;
    json[name].as_str().map(String::from)
}

/// Signs in to Microsoft Graph and renews tokens.
pub struct GraphAuth {
    config: GraphConfig,
    agent: ureq::Agent,
    /// `https://login.microsoftonline.com`, replaceable so tests can talk to a local server.
    login_base: String,
}

impl fmt::Debug for GraphAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GraphAuth")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl GraphAuth {
    /// An authenticator for `config`.
    #[must_use]
    pub fn new(config: GraphConfig) -> Self {
        Self {
            config,
            agent: http::agent(AGENT_TIMEOUT),
            login_base: LOGIN_BASE.to_string(),
        }
    }

    /// Log the response headers `log` selects, for the sign-in and token requests too.
    pub(crate) fn with_header_log(mut self, log: &std::sync::Arc<HeaderLog>) -> Self {
        self.agent = http::logging_agent(AGENT_TIMEOUT, log);
        self
    }

    #[cfg(test)]
    pub(super) fn with_login_base(mut self, base: &str) -> Self {
        self.login_base = base.to_string();
        self
    }

    fn endpoint(&self, leaf: &str) -> String {
        format!(
            "{}/{}/oauth2/v2.0/{leaf}",
            self.login_base, self.config.tenant
        )
    }

    /// POST a form to the token endpoint. Throttling and server errors are errors; an OAuth refusal (4xx with an
    /// `error` field) is returned as a [`TokenResponse`] for the caller to interpret (`authorization_pending` is not
    /// a failure while polling).
    fn post_token(&self, operation: &'static str, form: &[(&str, &str)]) -> Result<TokenResponse> {
        const MAX_TOKEN_RESPONSE_BYTES: u64 = 128 * 1024;
        let mut resp = self
            .agent
            .post(&self.endpoint("token"))
            .send_form(form.iter().copied())
            .map_err(|e| Error::transport(operation, e))?;
        http::transient_failure(operation, &mut resp)?;
        http::body_json(operation, &mut resp, MAX_TOKEN_RESPONSE_BYTES)
    }

    fn to_tokens(&self, r: TokenResponse, old_refresh: Option<String>) -> Result<GraphTokens> {
        match r.access_token {
            Some(access_token) => Ok(GraphTokens(Stored {
                version: FORMAT_VERSION,
                access_token,
                refresh_token: r.refresh_token.or(old_refresh),
                expires_at: unix_now().saturating_add(r.expires_in.unwrap_or(3600).max(0)),
                client_id: self.config.client_id.clone(),
            })),
            None => Err(r.rejection().into()),
        }
    }

    /// Exchange the refresh token for a new access token. The result is not stored; see [`GraphAuth::cached`].
    ///
    /// # Errors
    /// [`AuthError::NotSignedIn`] if there is no refresh token, [`AuthError::Rejected`] if Entra ID refuses it,
    /// [`Error::Throttled`] or a transport or decode [`Error`] otherwise.
    pub fn refresh(&self, tokens: &GraphTokens) -> Result<GraphTokens> {
        let refresh = tokens
            .0
            .refresh_token
            .clone()
            .ok_or(AuthError::NotSignedIn)?;
        let r = self.post_token(
            "refresh Graph sign-in",
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &self.config.client_id),
                ("refresh_token", &refresh),
                ("scope", SCOPES),
            ],
        )?;
        self.to_tokens(r, Some(refresh))
    }

    /// Interactive device-code sign-in: `announce` is called once with what the user must do, then this waits for
    /// them to finish. The result is not stored; call [`GraphTokens::save`].
    ///
    /// A network hiccup or throttling while waiting does not abandon the sign-in: polling backs off and continues
    /// until the code expires.
    ///
    /// # Errors
    /// [`AuthError::Rejected`] if sign-in is declined or the app is not allowed, [`AuthError::TimedOut`] if the
    /// code expires, otherwise a transport or decode [`Error`].
    pub fn device_login(&self, announce: &dyn Fn(&DeviceCodePrompt)) -> Result<GraphTokens> {
        const START: &str = "start Graph sign-in";
        const MAX_DEVICE_CODE_RESPONSE_BYTES: u64 = 128 * 1024;
        let mut resp = self
            .agent
            .post(&self.endpoint("devicecode"))
            .send_form([
                ("client_id", self.config.client_id.as_str()),
                ("scope", SCOPES),
            ])
            .map_err(|e| Error::transport(START, e))?;
        let body: Value = http::body_json(START, &mut resp, MAX_DEVICE_CODE_RESPONSE_BYTES)?;
        if let Some(code) = body["error"].as_str() {
            return Err(AuthError::Rejected {
                code: code.to_string(),
                description: body["error_description"]
                    .as_str()
                    .unwrap_or("")
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string(),
            }
            .into());
        }
        let dc: DeviceCode = serde_json::from_value(body).map_err(|e| Error::decode(START, e))?;
        // All three are shown to the person at a terminal, and the page is one they are told to type a code into: only
        // an https address is a sign-in page, and none of the text may carry terminal controls.
        let page = url::Url::parse(dc.verification_uri.trim())
            .ok()
            .filter(|u| u.scheme() == "https" && u.host_str().is_some())
            .ok_or(Error::Unexpected {
                operation: START,
                detail: "the sign-in page the service named is not an https address",
            })?;
        announce(&DeviceCodePrompt {
            verification_uri: page.to_string(),
            user_code: crate::clean(&dc.user_code),
            message: dc
                .message
                .map(|m| crate::clean(&m))
                .filter(|m| !m.trim().is_empty()),
        });

        let deadline =
            unix_now().saturating_add(i64::try_from(dc.expires_in).unwrap_or(i64::MAX / 2));
        // Never poll faster than once a second (the tests script their own replies and poll immediately).
        let mut interval = if cfg!(test) {
            dc.interval
        } else {
            dc.interval.max(1)
        };
        while unix_now() < deadline {
            std::thread::sleep(Duration::from_secs(interval));
            let polled = self.post_token(
                "poll Graph sign-in",
                &[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", &self.config.client_id),
                    ("device_code", &dc.device_code),
                ],
            );
            let r = match polled {
                Ok(r) => r,
                Err(e) if e.is_transient() => {
                    interval = (interval + BACKOFF_STEP_SECS).min(MAX_POLL_SECS);
                    continue;
                }
                Err(e) => return Err(e),
            };
            match r.error.as_deref() {
                None => return self.to_tokens(r, None),
                Some("authorization_pending") => {}
                Some("slow_down") => interval = (interval + BACKOFF_STEP_SECS).min(MAX_POLL_SECS),
                Some(_) => return Err(r.rejection().into()),
            }
        }
        Err(AuthError::TimedOut.into())
    }

    /// Stored tokens that are usable now, renewing (and re-storing) them when the access token has expired.
    /// `None` means the user has to sign in: nothing is stored, it was for another app, or Entra ID no longer
    /// accepts the refresh token.
    ///
    /// # Errors
    /// [`StoreError`] if the store fails, or a transient [`Error`] if the service could not be reached; a network
    /// failure is *not* reported as "sign in again".
    pub fn cached(&self, store: &dyn CredentialStore) -> Result<Option<GraphTokens>> {
        let Some(tokens) =
            GraphTokens::load(store)?.filter(|t| t.0.client_id == self.config.client_id)
        else {
            return Ok(None);
        };
        if tokens.fresh() {
            return Ok(Some(tokens));
        }
        match self.refresh(&tokens) {
            Ok(renewed) => {
                renewed.save(store)?;
                Ok(Some(renewed))
            }
            Err(Error::Auth(AuthError::NotSignedIn)) => Ok(None),
            Err(Error::Auth(a)) if a.is_grant_problem() => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use crate::testserver::{Reply, serve};

    fn tokens(expires_in: i64) -> GraphTokens {
        GraphTokens(Stored {
            version: FORMAT_VERSION,
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: unix_now() + expires_in,
            client_id: DEFAULT_CLIENT_ID.into(),
        })
    }

    fn auth_against(server: &crate::testserver::Server) -> GraphAuth {
        GraphAuth::new(GraphConfig::default()).with_login_base(server.origin())
    }

    #[test]
    fn documents_without_a_version_load_and_newer_ones_are_refused() {
        let store = MemoryStore::new();
        store
            .save(br#"{"access_token":"a","refresh_token":"r","expires_at":1,"client_id":"c"}"#)
            .unwrap();
        assert!(
            GraphTokens::load(&store).unwrap().is_some(),
            "a document without a version is version 1"
        );
        store
            .save(br#"{"version":99,"access_token":"a","refresh_token":"r","expires_at":1,"client_id":"c"}"#)
            .unwrap();
        assert!(matches!(
            GraphTokens::load(&store),
            Err(StoreError::UnsupportedVersion {
                found: 99,
                supported: 1
            })
        ));
        tokens(60).save(&store).unwrap();
        let saved = String::from_utf8(store.load().unwrap().unwrap()).unwrap();
        assert!(saved.contains("\"version\": 1"), "{saved}");
    }

    #[test]
    fn freshness_leaves_a_minute_of_margin() {
        assert!(tokens(3600).fresh());
        assert!(!tokens(30).fresh());
        assert!(!tokens(-5).fresh());
    }

    #[test]
    fn tokens_round_trip_through_a_store_and_debug_hides_secrets() {
        let store = MemoryStore::new();
        assert!(GraphTokens::load(&store).unwrap().is_none());
        tokens(3600).save(&store).unwrap();
        let back = GraphTokens::load(&store).unwrap().unwrap();
        assert_eq!(back.access_token(), "at");
        let shown = format!("{back:?}");
        assert!(!shown.contains("at\"") && !shown.contains("rt"), "{shown}");
    }

    #[test]
    fn cached_ignores_tokens_for_another_app_and_returns_fresh_ones_untouched() {
        let store = MemoryStore::new();
        tokens(3600).save(&store).unwrap();
        let ours = GraphAuth::new(GraphConfig::default());
        assert!(ours.cached(&store).unwrap().is_some());
        let other = GraphAuth::new(GraphConfig::new("some-other-app", DEFAULT_TENANT));
        assert!(other.cached(&store).unwrap().is_none());
    }

    #[test]
    fn a_rejection_becomes_a_typed_error_with_the_first_line_of_the_description() {
        let r = TokenResponse {
            access_token: None,
            refresh_token: None,
            expires_in: None,
            error: Some("invalid_grant".into()),
            error_description: Some("AADSTS700084: expired\r\nTrace ID: x".into()),
        };
        match r.rejection() {
            AuthError::Rejected { code, description } => assert_eq!(
                (code.as_str(), description.trim()),
                ("invalid_grant", "AADSTS700084: expired")
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn refresh_sends_the_grant_and_keeps_the_old_refresh_token_when_none_is_rotated() {
        let server = serve(vec![
            Reply::status(200).json(r#"{"access_token":"new-at","expires_in":3600}"#),
        ]);
        let renewed = auth_against(&server).refresh(&tokens(-10)).unwrap();
        assert_eq!(renewed.access_token(), "new-at");
        assert_eq!(renewed.0.refresh_token.as_deref(), Some("rt"));
        let seen = server.seen();
        assert_eq!(seen[0].path, "/organizations/oauth2/v2.0/token");
        assert!(
            seen[0].body.contains("grant_type=refresh_token")
                && seen[0].body.contains("refresh_token=rt"),
            "{}",
            seen[0].body
        );
    }

    #[test]
    fn refresh_failures_are_classified() {
        let server = serve(vec![
            Reply::status(400).json(
                r#"{"error":"invalid_grant","error_description":"AADSTS700084: gone\r\nmore"}"#,
            ),
            Reply::status(429).header("Retry-After", "3").body(""),
            Reply::status(503).body("down"),
        ]);
        let auth = auth_against(&server);
        let t = tokens(-10);
        match auth.refresh(&t) {
            Err(Error::Auth(a)) => assert!(a.is_grant_problem() && a.needs_sign_in(), "{a:?}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            auth.refresh(&t),
            Err(Error::Throttled {
                retry_after: Some(_),
                ..
            })
        ));
        assert!(matches!(
            auth.refresh(&t),
            Err(Error::Status { status: 503, .. })
        ));
    }

    #[test]
    fn cached_treats_a_refused_grant_as_sign_in_again_but_not_a_network_blip() {
        let store = MemoryStore::new();
        tokens(-10).save(&store).unwrap();
        let server = serve(vec![
            Reply::status(400).json(r#"{"error":"invalid_grant"}"#),
            Reply::status(503).body(""),
        ]);
        let auth = auth_against(&server);
        assert!(
            auth.cached(&store).unwrap().is_none(),
            "refused grant means sign in again"
        );
        let err = auth.cached(&store).unwrap_err();
        assert!(err.is_transient() && !err.needs_sign_in(), "{err:?}");
    }

    #[test]
    fn cached_reports_a_misconfigured_client_instead_of_asking_to_sign_in_again() {
        let store = MemoryStore::new();
        tokens(-10).save(&store).unwrap();
        let server = serve(vec![Reply::status(400).json(
            r#"{"error":"unauthorized_client","error_description":"the app is not allowed this grant"}"#,
        )]);
        let auth = auth_against(&server);
        let err = auth.cached(&store).unwrap_err();
        assert!(
            !err.needs_sign_in(),
            "a misconfigured client is not fixed by signing in again: {err:?}"
        );
    }

    #[test]
    fn device_login_waits_through_pending_and_transient_failures() {
        let server = serve(vec![
            Reply::status(200).json(r#"{"device_code":"dc","user_code":"ABCD","verification_uri":"https://example.test/device","expires_in":60,"interval":0}"#),
            Reply::status(400).json(r#"{"error":"authorization_pending"}"#),
            Reply::status(503).body("blip"),
            Reply::status(200).json(r#"{"access_token":"at2","refresh_token":"rt2","expires_in":3600}"#),
        ]);
        let shown = std::cell::RefCell::new(None);
        let tokens = auth_against(&server)
            .device_login(&|p| *shown.borrow_mut() = Some(p.user_code.clone()))
            .unwrap();
        assert_eq!(tokens.access_token(), "at2");
        assert_eq!(shown.borrow().as_deref(), Some("ABCD"));
        assert_eq!(server.seen().len(), 4);
    }

    #[test]
    fn what_the_service_says_to_show_is_cleaned_and_the_page_must_be_https() {
        let hostile = r#"{"device_code":"dc","user_code":"AB\u001b[2JCD","verification_uri":"https://example.test/device","message":"Go to the page\u001b]52;c;eA==\u0007 now","expires_in":60,"interval":0}"#;
        let server = serve(vec![
            Reply::status(200).json(hostile),
            Reply::status(200)
                .json(r#"{"access_token":"at","refresh_token":"rt","expires_in":3600}"#),
        ]);
        let shown = std::cell::RefCell::new(None);
        auth_against(&server)
            .device_login(&|p| *shown.borrow_mut() = Some(p.clone()))
            .unwrap();
        let p = shown.borrow().clone().unwrap();
        assert!(!p.user_code.contains('\u{1b}') && !p.message.unwrap().contains('\u{1b}'));
        assert_eq!(p.verification_uri, "https://example.test/device");

        for page in ["http://example.test/device", "javascript:alert(1)", "u", ""] {
            let body = format!(
                r#"{{"device_code":"dc","user_code":"X","verification_uri":"{page}","expires_in":60,"interval":0}}"#
            );
            let server = serve(vec![Reply::status(200).json(&body)]);
            let shown = std::cell::Cell::new(false);
            let err = auth_against(&server)
                .device_login(&|_| shown.set(true))
                .unwrap_err();
            assert!(matches!(err, Error::Unexpected { .. }), "{page}: {err:?}");
            assert!(!shown.get(), "{page} was never shown to anyone");
            assert_eq!(
                server.seen().len(),
                1,
                "{page}: nothing polled for a code nobody saw"
            );
        }
    }

    #[test]
    fn device_login_stops_at_a_real_refusal() {
        let server = serve(vec![
            Reply::status(200).json(r#"{"device_code":"dc","user_code":"X","verification_uri":"https://example.test/device","expires_in":60,"interval":0}"#),
            Reply::status(400).json(r#"{"error":"authorization_declined","error_description":"user said no"}"#),
        ]);
        let err = auth_against(&server).device_login(&|_| {}).unwrap_err();
        assert!(
            matches!(err, Error::Auth(AuthError::Rejected { ref code, .. }) if code == "authorization_declined"),
            "{err:?}"
        );
    }

    #[test]
    fn tenant_from_jwt_extracts_the_tid_claim() {
        // A minimal JWT with {"tid": "abc-123"} as the payload (header and signature are dummy values).
        let token = "header.eyJ0aWQiOiAiYWJjLTEyMyJ9.signature";
        assert_eq!(tenant_from_jwt(token), Some("abc-123".to_string()));
        // Not three parts.
        assert_eq!(tenant_from_jwt("not.a.jwt.with.extra"), None);
        assert_eq!(tenant_from_jwt("only-two"), None);
        // Payload is not valid base64.
        assert_eq!(tenant_from_jwt("x.not-base64!!!.y"), None);
        // Payload is not valid JSON.
        assert_eq!(tenant_from_jwt("x.aW52YWxpZC1qc29u.y"), None);
        // Missing tid claim.
        let no_tid = "x.e30.y"; // e30 is base64 for {}
        assert_eq!(tenant_from_jwt(no_tid), None);
        // tid is not a string.
        let tid_number = "x.eyJ0aWQiOiAxMjN9.y"; // {"tid": 123}
        assert_eq!(tenant_from_jwt(tid_number), None);
    }

    #[test]
    fn graph_tokens_expose_the_user_id_from_the_oid_claim() {
        let mut stored = tokens(3600).0;
        // {"oid": "user-guid"}
        stored.access_token = "header.eyJvaWQiOiAidXNlci1ndWlkIn0.sig".to_string();
        assert_eq!(GraphTokens(stored).user_id(), Some("user-guid".to_string()));
        let mut none = tokens(3600).0;
        none.access_token = "x.e30.y".to_string();
        assert_eq!(GraphTokens(none).user_id(), None);
    }

    #[test]
    fn graph_tokens_expose_tenant_id_from_access_token() {
        let mut stored = tokens(3600).0;
        // Real-ish JWT with tid claim (simplified, not a valid signature).
        stored.access_token = "header.eyJ0aWQiOiAidGVuYW50LWd1aWQifQ.sig".to_string();
        let t = GraphTokens(stored);
        assert_eq!(t.tenant_id(), Some("tenant-guid".to_string()));
        // Token with no tid claim.
        let mut no_tid = tokens(3600).0;
        no_tid.access_token = "x.e30.y".to_string();
        assert_eq!(GraphTokens(no_tid).tenant_id(), None);
    }
}
