//! The Teams web API backend (`chatsvc`), the service the Teams web app itself talks to.
//!
//! **Disclaimer.** This API is unofficial and unsanctioned: Microsoft has not published it and does not support it, and
//! this client presents itself to it as Microsoft's own Teams web client, which may breach Microsoft's terms and an
//! organisation's policies and can get the account restricted. It is provided for interest and research, not
//! recommended, and used entirely at the user's own risk. Use the Graph backend for anything that matters.
//!
//! The approach (sign in through the user's own browser, read the web app's MSAL cache, mint API tokens from the refresh
//! token) and the request shapes follow the [msteams-mcp](https://github.com/m0nkmaster/msteams-mcp) project (MIT), with
//! behaviour checked against other open-source Teams clients; this is a separate implementation and contains none of
//! their code. The shapes are undocumented and may change without notice. Sign in with [`sign_in`] (it drives the user's own browser), keep the
//! [`TeamsSession`] in a [`CredentialStore`], and hand it to [`TeamsBackend::new`].

mod auth;
mod browser;
mod channels;
mod emoji_catalog;
mod favourites;
mod incremental;
mod parse;
mod search;
// Named for what it reads, which is also what this module is called.
#[allow(clippy::module_inception)]
mod teams;
mod trouter;

pub use auth::{TeamsSession, sign_in};
pub use browser::BrowserOptions;

use crate::error::{Error, Result};
use crate::headerlog::HeaderLog;
use crate::http::{self, Call, Headers, urlencode};
use crate::lock;
use crate::model::{
    Availability, Backend, ChannelActivity, ChannelPost, Chat, ChatKind, Contact, Delivery, Me,
    Member, Msg, OutMention, Page, Presence, PushHint, Reaction, ReplyTarget, SearchPage, Team,
};
use crate::record::ShapeRecorder;
use crate::shares;
use crate::store::CredentialStore;
use crate::text::{clean, id_tag};
use crate::unix_now as now;
use crate::watch::{self, Watch};
use auth::{Live, TeamsAuth, trusted_url};
use chrono::{DateTime, TimeZone, Utc};
use favourites::{FavouriteCache, FavouriteSource, folder_write_unsupported};
use image::RgbImage;
use parse::{
    Parsed, guid, is_guid, mri, one_on_one_id, pair_members, parse_conversation, parse_msg,
    parse_people, qtd_msgs_json, reply_quote_html, teams_mentions,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, TryLockError, Weak};
use std::thread;
use std::time::{Duration, Instant};

const PEOPLE_URL: &str = "https://substrate.office.com/search/api/v1/suggestions?scenario=powerbar";
/// Teams' presence service: it answers "how available are these people" (`/v1/presence/getpresence/`) and takes
/// what a person says about themselves (`/v1/me/...`).
const PRESENCE_BASE: &str = "https://presence.teams.microsoft.com";
/// How many people one presence request asks about.
const PRESENCE_BATCH: usize = 100;
/// The client build the Teams web app announces; override it with [`TeamsBackend::with_client_version`] if the service
/// starts rejecting this one.
const DEFAULT_CLIENT_VERSION: &str = "1415/1.0.0.2025010401";
/// The most conversations the service returns in one list request.
/// Refused incremental reads in a row (none worked in between) after which they are given up for the session.
const MAX_INCREMENT_REFUSALS: u32 = 3;
/// The most conversations one request is asked for. The service answered 500 (a whole account of 479 conversations in
/// one answer) and refused 1000 with a 400, so 500 is as far as this has been checked.
const MAX_CHATS: usize = 500;
/// How long one API request, and one picture download, may take altogether.
const API_TIMEOUT: Duration = Duration::from_secs(30);
const CDN_TIMEOUT: Duration = Duration::from_secs(20);
/// How many chats are listed unless told otherwise. A full read of 300 is about 0.9 MB and half a second; later reads
/// only fetch what changed.
const DEFAULT_CHATS: usize = 300;
/// After a failed profile lookup, wait this long before asking again; each failure in a row doubles it, up to
/// [`MAX_LOOKUP_PAUSE`].
const LOOKUP_PAUSE: Duration = Duration::from_secs(300);
const MAX_LOOKUP_PAUSE: Duration = Duration::from_secs(3600);
/// How many ids one profile lookup asks about.
const NAME_BATCH: usize = 50;
/// After a lookup fails on the service's side (a 5xx, typically its own 10 s limit), the batch is split in halves to
/// find what it chokes on. This is how long, from the first failure, that may keep going in one call, since each
/// failing half costs the service's whole timeout.
const SPLIT_BUDGET: Duration = Duration::from_secs(40);
/// Group and meeting chats without a name need their member list to be titled. At most this many are asked about in
/// one read of the list, a few at a time, so a long list is titled over several refreshes instead of in one burst.
const MEMBER_LOOKUPS_PER_READ: usize = 24;
const MEMBER_LOOKUPS_AT_ONCE: usize = 4;
/// How long lookups wait when the service says to slow down without saying for how long, and the longest they wait.
const DEFAULT_THROTTLE_PAUSE: Duration = Duration::from_secs(10);
const MAX_THROTTLE_PAUSE: Duration = Duration::from_secs(300);

/// Clears the "the helper thread is busy" flag when dropped, so a job that ends any way at all, a panic included,
/// lets the next one start.
struct Running<'a>(&'a AtomicBool);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// How long to leave best-effort lookups alone when `e` is the service asking us to slow down.
fn throttle_pause(e: &Error) -> Option<Duration> {
    match e {
        Error::Throttled { retry_after, .. } => Some(
            retry_after
                .unwrap_or(DEFAULT_THROTTLE_PAUSE)
                .min(MAX_THROTTLE_PAUSE),
        ),
        _ => None,
    }
}
const PROFILE_QUERY: &str = "isMailAddress=false&enableGuest=true&skypeTeamsInfo=true&canBeSmtpAddress=false&includeIBBarredUsers=true&includeDisabledAccounts=true";

/// Whether `url` is hosted by Teams' media service, which is where message images live: `*.asm.skype.com` and any
/// subdomain of `teams.microsoft.com` (the newer gateway is `<region>-prod.asyncgw.teams.microsoft.com`, with the same
/// `/v1/objects/<id>/views/...` paths). Images are fetched with the skypetoken as `Authorization: skype_token`, so
/// nothing else is ever fetched. A subdomain of `teams.microsoft.com` is a Teams host, to which the skypetoken already
/// goes for every API call (`auth::trusted_url`), so nothing new is trusted by naming it here. The bare
/// `teams.microsoft.com` is not included.
fn is_media_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https"
            && u.port().is_none()
            && u.username().is_empty()
            && u.password().is_none()
            && u.host_str().is_some_and(|h| {
                h == "asm.skype.com"
                    || h.ends_with(".asm.skype.com")
                    || h.ends_with(".teams.microsoft.com")
            })
    })
}

/// Which access token a request carries: the chat service takes the spaces token, the aggregator its own.
#[derive(Clone, Copy)]
enum Bearer {
    Spaces,
    Csa,
    Presence,
}

/// What is known of one chat's messages that are the same one sent again (see `TeamsBackend::without_resends`).
#[derive(Default)]
struct Resends {
    /// The message shown for each (sender, client message id, content): the first to arrive.
    kept: HashMap<u64, String>,
    /// The ids of the later copies, left out whenever they come again (an edit or a reaction on one of them brings it
    /// back in an incremental read, on its own).
    dropped: HashSet<String>,
}

/// Most entries kept for one chat's resends before they are forgotten and learned again.
const MAX_RESENDS: usize = 5_000;

/// How long cached credentials must still last for a caller to use them (see [`TeamsBackend::live`]).
#[derive(Clone, Copy)]
enum Enough {
    /// More than two minutes of the access token and five of the skype token: the point at which a renewal starts.
    Comfortably,
    /// Not yet run out (with a few seconds' margin for the request to arrive): good enough while another caller is
    /// already renewing.
    Barely,
}

/// The margin, in seconds, of [`Enough::Barely`].
const RUNNING_OUT: i64 = 10;

/// Chats over the Teams web API.
pub struct TeamsBackend {
    auth: TeamsAuth,
    /// The credentials last obtained. Held only to read or replace them (never across a request or while another
    /// lock is taken), so reading them never waits for a renewal; see [`Self::live`] for the order of the two locks.
    live: Mutex<Option<Live>>,
    /// Held for a whole renewal of those credentials (the token refresh and the service's authorisation), so that
    /// callers that need one share a single renewal. Taken before `live`, never while holding it.
    live_gate: Mutex<()>,
    /// Incremented after each renewal attempt, so callers waiting on the gate can tell that one finished and share
    /// its failure instead of each attempting their own renewal during an outage.
    renewal_attempt: AtomicU64,
    api: ureq::Agent,
    cdn: ureq::Agent,
    /// Entra object id -> display name, learned from message senders and profile lookups.
    names: Mutex<HashMap<String, String>>,
    /// Chat id -> member object ids, for chats whose list entry doesn't say.
    members: Mutex<HashMap<String, Vec<String>>>,
    /// Substrate (directory search) token and its expiry.
    substrate: Mutex<Option<(String, i64)>>,
    /// Chat service aggregator token (conversation folders) and its expiry.
    csa: Mutex<Option<(String, i64)>>,
    /// Presence service token and its expiry.
    presence: Mutex<Option<(String, i64)>>,
    /// Microsoft Graph token (resolving a SharePoint/OneDrive attachment to a direct download, `shares.rs`) and its
    /// expiry. Requested only when a message actually has one such attachment, not on every poll.
    graph: Mutex<Option<(String, i64)>>,
    /// The token `send_message` sends a message with (see `auth::IC3_SCOPE`) and its expiry.
    ic3: Mutex<Option<(String, i64)>>,
    /// The favourite source [`favourites`](Self::favourites) found last time it ran, so [`Backend::can_favorite`]
    /// (a synchronous, no-network method) can say false once a folder-sourced account is known, instead of always
    /// optimistically true, and so [`Backend::set_favorite`] can refuse before any request once that is known rather
    /// than after reading the folder again. `None` until the first successful read; treated as "maybe `Property`".
    favourite_source: Mutex<Option<FavouriteSource>>,
    /// The favourites the last successful read found, with their source: what the chat list shows if a later read
    /// fails for the moment. Never used to work out a change (see `favourites`).
    last_favourites: Mutex<Option<(serde_json::Map<String, Value>, FavouriteSource)>>,
    /// Held for the whole read-modify-write in [`set_favorite`](Self::set_favorite): the favourites are one shared
    /// value, so two calls for different chats running at once would each compute their change from a snapshot that
    /// does not have the other's, and whichever write lands last would silently undo it.
    favourite_write: Mutex<()>,
    /// 1:1 chats opened here that Teams may not have a conversation for yet; created on the first send if needed.
    fresh: Mutex<HashSet<String>>,
    /// The directory search endpoint, replaceable so tests can talk to a local server.
    people_url: String,
    /// The message search endpoint (Substrate), replaceable so tests can talk to a local server.
    search_url: String,
    /// The presence service, replaceable so tests can talk to a local server.
    presence_base: String,
    /// This program as one of the places you are online (the id the presence service knows it by), and whether the
    /// service has been told about it.
    endpoint: String,
    endpoint_registered: AtomicBool,
    /// Microsoft Graph's own base URL (resolving a SharePoint/OneDrive attachment, `shares.rs`); replaceable so
    /// tests can talk to a local server.
    graph_base: String,
    recorder: Option<Arc<ShapeRecorder>>,
    max_chats: usize,
    client_version: String,
    /// Object ids already looked up, so ones the directory does not name are not asked about on every refresh.
    looked_up: Mutex<HashSet<String>>,
    /// Object ids a name lookup asked about that its answer left out (the service answered the rest, so it is not the
    /// lookup failing): accounts the directory no longer has, such as people who have left. `Member::not_in_directory`.
    unlisted: Mutex<HashSet<String>>,
    /// Bumped whenever `unlisted` changes, so the helper thread can tell the app a list read would draw it differently.
    unlisted_changes: AtomicU64,
    /// The people search refused the request that asks for phone and office too: ask the old way this session.
    people_extras_refused: AtomicBool,
    /// Repeats already noted in the log (a hash of the ids involved), so a poll that sees them again says nothing.
    repeats_seen: Mutex<HashSet<u64>>,
    /// Per chat, the messages that are copies of one sent again.
    resends: Mutex<HashMap<String, Resends>>,
    /// Profile lookups are not attempted again before this instant (set after a failure).
    lookup_paused_until: Mutex<Option<Instant>>,
    /// Lookups that failed outright in a row, so the wait before the next one grows (see `pause_lookups`).
    lookup_failures: AtomicU32,
    /// This backend, once [`attach`](Self::attach) was called, so a lookup can run on a thread of its own.
    background: OnceLock<Weak<TeamsBackend>>,
    /// Whether such a lookup is running, so there is never more than one.
    lookup_running: AtomicBool,
    /// For tests: how many times this backend was asked for its credentials (`live`).
    #[cfg(test)]
    live_calls: std::sync::atomic::AtomicUsize,
    /// For tests: the next name lookup on the helper thread panics.
    #[cfg(test)]
    panic_in_lookup: AtomicBool,
    /// Member lookups (for titles) are not attempted before this instant (set when the service asks us to slow down).
    members_paused_until: Mutex<Option<Instant>>,
    /// When a member lookup last failed for a reason other than throttling (a chat left or no longer visible, say).
    /// Such a chat is not retried for [`Self::full_refresh`]: without this, a handful of permanently failing chats
    /// would occupy the whole [`MEMBER_LOOKUPS_PER_READ`] budget on every read and no other chat would ever get a
    /// title.
    failed_members: Mutex<HashMap<String, Instant>>,
    /// The newest page of recently read chats, so later reads can ask only for what changed (see `incremental`).
    threads: Mutex<HashMap<String, incremental::Thread>>,
    /// The conversation list as last read.
    chat_index: Mutex<Option<incremental::ChatIndex>>,
    /// What the conversation list said about the teams and channels in it the last time it was read (which is not
    /// the list of teams: see `teams.rs`).
    channel_activity: Mutex<teams::ActivityMap>,
    /// Whether [`Backend::chats`] also reads the user's favourites (one more small request), set by
    /// [`with_favourites`](Self::with_favourites).
    read_favourites: bool,
    /// A favourite older than the page, fetched by id in [`add_favourites`](Self::add_favourites), kept for
    /// [`Self::full_refresh`] so a chat whose details rarely change is not re-fetched on every scheduled list
    /// refresh (as short as 20 s), only on the bound that governs every other remembered read.
    favourite_cache: Mutex<FavouriteCache>,
    /// Whether to read changes only; cleared by [`with_incremental_reads`](Self::with_incremental_reads) or when the
    /// service refuses an incremental request that a full one then satisfies.
    incremental: AtomicBool,
    /// Incremental reads refused in a row; reset when one works.
    increment_refusals: AtomicU32,
    /// How long remembered reads are built on before everything is read again.
    full_refresh: Duration,
    /// Notices fields and values the service has not sent before (logged, names only).
    watch: Watch,
    /// Teams' emoji catalogue, downloaded the first time a reaction needs it.
    emoji_catalog: Mutex<Option<Arc<emoji_catalog::Catalog>>>,
    /// Where the catalogue is published, newest first (replaceable so tests can serve their own).
    emoji_catalog_urls: Vec<String>,
    /// Until when a failed download is not tried again.
    emoji_catalog_retry: Mutex<Option<Instant>>,
    /// How long that is.
    emoji_catalog_pause: Duration,
    /// Whether [`Self::connect_push`] does anything, set by [`with_push`](Self::with_push).
    push_enabled: bool,
    /// So a second [`Self::connect_push`] call is a no-op: the connection is started once, not once per call.
    push_started: AtomicBool,
    /// Hints queued by the push connection, drained by [`Backend::poll_pushes`].
    push_queue: Mutex<Vec<PushHint>>,
    /// Trouter's own entry point; replaceable so tests can talk to a local server.
    trouter_host: String,
}

impl fmt::Debug for TeamsBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TeamsBackend").finish_non_exhaustive()
    }
}

impl TeamsBackend {
    /// A backend for `session`. Renewed sessions are saved to `store`; a failure to save is reported through
    /// [`Backend::take_warnings`] rather than failing the call that triggered the renewal. `browser` is used for the
    /// silent renewal that happens when Entra ID says the stored grant is no longer good.
    #[must_use]
    pub fn new(
        session: TeamsSession,
        store: Arc<dyn CredentialStore>,
        browser: BrowserOptions,
    ) -> Self {
        Self {
            auth: TeamsAuth::new(session, store, browser),
            live: Mutex::new(None),
            live_gate: Mutex::new(()),
            renewal_attempt: AtomicU64::new(0),
            api: http::agent(API_TIMEOUT),
            cdn: http::agent(CDN_TIMEOUT),
            names: Mutex::new(HashMap::new()),
            members: Mutex::new(HashMap::new()),
            substrate: Mutex::new(None),
            csa: Mutex::new(None),
            presence: Mutex::new(None),
            graph: Mutex::new(None),
            ic3: Mutex::new(None),
            favourite_source: Mutex::new(None),
            last_favourites: Mutex::new(None),
            favourite_write: Mutex::new(()),
            fresh: Mutex::new(HashSet::new()),
            people_url: PEOPLE_URL.to_string(),
            search_url: search::SEARCH_URL.to_string(),
            presence_base: PRESENCE_BASE.to_string(),
            endpoint: guid(),
            endpoint_registered: AtomicBool::new(false),
            graph_base: shares::GRAPH_BASE.to_string(),
            recorder: None,
            max_chats: DEFAULT_CHATS,
            client_version: DEFAULT_CLIENT_VERSION.to_string(),
            looked_up: Mutex::new(HashSet::new()),
            unlisted: Mutex::new(HashSet::new()),
            unlisted_changes: AtomicU64::new(0),
            people_extras_refused: AtomicBool::new(false),
            repeats_seen: Mutex::new(HashSet::new()),
            resends: Mutex::new(HashMap::new()),
            lookup_paused_until: Mutex::new(None),
            lookup_failures: AtomicU32::new(0),
            background: OnceLock::new(),
            lookup_running: AtomicBool::new(false),
            #[cfg(test)]
            live_calls: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            panic_in_lookup: AtomicBool::new(false),
            members_paused_until: Mutex::new(None),
            failed_members: Mutex::new(HashMap::new()),
            threads: Mutex::new(HashMap::new()),
            chat_index: Mutex::new(None),
            channel_activity: Mutex::new(HashMap::new()),
            read_favourites: true,
            favourite_cache: Mutex::new(HashMap::new()),
            incremental: AtomicBool::new(true),
            increment_refusals: AtomicU32::new(0),
            full_refresh: incremental::FULL_REFRESH,
            watch: Watch::default(),
            emoji_catalog: Mutex::new(None),
            emoji_catalog_retry: Mutex::new(None),
            emoji_catalog_pause: Duration::from_secs(60),
            emoji_catalog_urls: emoji_catalog::CATALOG_URLS
                .iter()
                .map(|u| (*u).to_string())
                .collect(),
            push_enabled: true,
            push_started: AtomicBool::new(false),
            push_queue: Mutex::new(Vec::new()),
            trouter_host: trouter::TROUTER_HOST.to_string(),
        }
    }

    /// Whether later reads of a chat and of the chat list ask only for what changed (the default), with a full read
    /// every five minutes. Turn it off to read everything every time.
    #[must_use]
    pub fn with_incremental_reads(self, on: bool) -> Self {
        self.incremental.store(on, Ordering::Relaxed);
        self
    }

    /// Whether [`Backend::chats`] reads the user's favourites and marks them in [`Chat::favorite`] (the default; one
    /// small extra request per read, plus one for each favourite older than the newest chats). Turn it off to list
    /// chats without them.
    #[must_use]
    pub fn with_favourites(mut self, on: bool) -> Self {
        self.read_favourites = on;
        self
    }

    /// Whether [`Self::connect_push`] does anything (the default). Off keeps the connection from ever being
    /// started, so the backend is purely poll-based.
    #[must_use]
    pub fn with_push(mut self, on: bool) -> Self {
        self.push_enabled = on;
        self
    }

    /// Record the shape of every exchange (see [`ShapeRecorder`]).
    #[must_use]
    pub fn with_recorder(mut self, recorder: Arc<ShapeRecorder>) -> Self {
        self.auth = self.auth.with_recorder(Arc::clone(&recorder));
        self.recorder = Some(recorder);
        self
    }

    /// Log the response headers `log` selects, one line per response (see [`HeaderLog`]), for diagnosing connection
    /// problems. Request headers (the bearer token, the skypetoken) are never logged, and credential-bearing response
    /// headers are withheld. The push channel's own WebSocket is not an HTTP agent request and is not covered.
    #[must_use]
    pub fn with_header_log(mut self, log: HeaderLog) -> Self {
        let log = Arc::new(log);
        self.api = http::logging_agent(API_TIMEOUT, &log);
        self.cdn = http::logging_agent(CDN_TIMEOUT, &log);
        self.auth = self.auth.with_header_log(&log);
        self
    }

    /// How many recent chats [`Backend::chats`] asks for: 300 by default, at most 500 (the most the service has been
    /// seen to answer; it refuses 1000), at least 1. Favourites are listed on top of this number.
    #[must_use]
    pub fn with_max_chats(mut self, max: usize) -> Self {
        self.max_chats = max.clamp(1, MAX_CHATS);
        self
    }

    /// The `X-Ms-Client-Version` sent with chat requests.
    #[must_use]
    pub fn with_client_version(mut self, version: impl Into<String>) -> Self {
        self.client_version = version.into();
        self
    }

    /// The Substrate token; `force` discards the cached one (after a 401).
    fn substrate_token(&self, force: bool) -> Result<String> {
        let mut cached = lock(&self.substrate);
        if !force
            && let Some((t, _)) = cached
                .as_ref()
                .filter(|(_, exp)| now().saturating_add(120) < *exp)
        {
            return Ok(t.clone());
        }
        let (token, expires_in) = self.auth.substrate_token()?;
        *cached = Some((token.clone(), now().saturating_add(expires_in.max(0))));
        Ok(token)
    }

    /// The chat service aggregator's token; `force` discards the cached one (after a 401).
    fn csa_token(&self, force: bool) -> Result<String> {
        let mut cached = lock(&self.csa);
        if !force
            && let Some((t, _)) = cached
                .as_ref()
                .filter(|(_, exp)| now().saturating_add(120) < *exp)
        {
            return Ok(t.clone());
        }
        let (token, expires_in) = self.auth.csa_token()?;
        *cached = Some((token.clone(), now().saturating_add(expires_in.max(0))));
        Ok(token)
    }

    /// Teams files many reactions under a plain word (`salute`, `fire`) that only its emoji catalogue turns into an
    /// emoji, so a reaction left as such a word is looked up there and replaced by the emoji; two ids for one emoji
    /// (`like`, `yes`) become one reaction. The catalogue is a public file read once, and only when a message has such
    /// a reaction. What it cannot place, or all of it when it cannot be read, is shown as the word, cut short.
    fn resolve_reaction_ids(&self, messages: &mut [Msg]) {
        let unresolved = |m: &Msg| m.reactions.iter().any(|r| r.emoji.is_ascii());
        if !messages.iter().any(unresolved) {
            return;
        }
        let catalog = match self.catalog() {
            Ok(catalog) => Some(catalog),
            Err(e) => {
                log::debug!("reactions are shown by their ids: {}", e.for_log());
                None
            }
        };
        for m in messages.iter_mut().filter(|m| unresolved(m)) {
            let mut merged: Vec<Reaction> = Vec::new();
            for r in std::mem::take(&mut m.reactions) {
                let emoji = if r.emoji.is_ascii() {
                    let found = catalog.as_ref().and_then(|c| c.emoji_for(&r.emoji));
                    crate::emoji::shown(found.unwrap_or(&r.emoji))
                } else {
                    r.emoji.clone()
                };
                match merged
                    .iter_mut()
                    .find(|o| crate::emoji::same_emoji(&o.emoji, &emoji))
                {
                    Some(o) => {
                        // Someone who reacted under both ids counts once.
                        let again = r.users.iter().filter(|u| o.users.contains(u)).count();
                        o.count += r.count.saturating_sub(again);
                        let new_indices: Vec<usize> = r
                            .users
                            .iter()
                            .enumerate()
                            .filter_map(|(i, u)| (!o.users.contains(u)).then_some(i))
                            .collect();
                        for i in new_indices {
                            o.users.push(r.users[i].clone());
                            if i < r.names.len() {
                                o.names.push(r.names[i].clone());
                            }
                        }
                    }
                    None => merged.push(Reaction {
                        emoji,
                        count: r.count,
                        users: r.users,
                        names: r.names,
                    }),
                }
            }
            m.reactions = merged;
        }
    }

    /// Fill in who reacted (`Reaction::names`, parallel to `users`) from the names this backend already knows. Ids it
    /// has never asked about are looked up on the `teams-names` helper thread when the backend is attached (the same
    /// one-at-a-time job and pauses as the chat titles' lookups, and each id is asked about once), and a hint naming
    /// `chat_id` is queued when that taught it a name, so the app reads the chat again soon and the names appear. Not
    /// attached (the tests), nothing is asked: the names stay empty unless already known.
    fn fill_reactor_names(&self, chat_id: &str, messages: &mut [Msg]) {
        let mut unknown: Vec<String> = Vec::new();
        {
            let names = lock(&self.names);
            for r in messages.iter_mut().flat_map(|m| m.reactions.iter_mut()) {
                r.names.resize(r.users.len(), String::new());
                for (user, name) in r.users.iter().zip(r.names.iter_mut()) {
                    if name.is_empty() {
                        match names.get(user) {
                            Some(known) => name.clone_from(known),
                            None => unknown.push(user.clone()),
                        }
                    }
                }
            }
        }
        {
            let me_id = self.auth.me().0;
            let asked = lock(&self.looked_up);
            unknown.retain(|id| is_guid(id) && *id != me_id && !asked.contains(id));
        }
        unknown.sort_unstable();
        unknown.dedup();
        if unknown.is_empty() {
            return;
        }
        let Some(me) = self.background.get().and_then(Weak::upgrade) else {
            return;
        };
        if lock(&self.lookup_paused_until).is_some_and(|until| Instant::now() < until)
            || me.lookup_running.swap(true, Ordering::SeqCst)
        {
            // Paused, or the helper is busy: the next read of the chat asks again.
            return;
        }
        let chat_id = chat_id.to_string();
        let spawned = thread::Builder::new()
            .name("teams-names".to_string())
            .spawn({
                let me = Arc::clone(&me);
                move || {
                    let _running = Running(&me.lookup_running);
                    let result = me
                        .live(None)
                        .and_then(|live| me.resolve_names(&live, &unknown, false));
                    let learned = unknown
                        .iter()
                        .filter(|id| !me.name_of(id).is_empty())
                        .count();
                    log::debug!(
                        "reactions: {} of {} name(s) of people who reacted learned",
                        learned,
                        unknown.len()
                    );
                    if learned > 0 {
                        trouter::queue_hint(&mut lock(&me.push_queue), PushHint::Chat(chat_id));
                    }
                    if let Err(e) = result {
                        me.auth.warn(e);
                    }
                }
            });
        if spawned.is_err() {
            me.lookup_running.store(false, Ordering::SeqCst);
            log::warn!("could not start the name lookup thread; reactions may lack names");
        }
    }
    /// A request to Teams' presence service, with the token of its own.
    fn presence_request(
        &self,
        operation: &'static str,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let url = format!("{}{path}", self.presence_base);
        self.request_with(Bearer::Presence, operation, method, &url, body)
    }

    /// The presence service's token; `force` discards the cached one (after a 401). A tenant that withholds this
    /// scope fails presence alone: `token` only recovers through the browser for `SPACES_SCOPE`.
    fn presence_token(&self, force: bool) -> Result<String> {
        let mut cached = lock(&self.presence);
        if !force
            && let Some((t, _)) = cached
                .as_ref()
                .filter(|(_, exp)| now().saturating_add(120) < *exp)
        {
            return Ok(t.clone());
        }
        let (token, expires_in) = self.auth.presence_token()?;
        *cached = Some((token.clone(), now().saturating_add(expires_in.max(0))));
        Ok(token)
    }

    /// The Graph token (resolving a SharePoint/OneDrive attachment, `shares.rs`); `force` discards the cached one
    /// (after a 401). A tenant that withholds this scope (seen on restricted clouds) fails this call with the
    /// rejection rather than any other scope's, and does not pause `SPACES_SCOPE`'s own browser renewal: `token`
    /// only recovers through the browser for that one scope (see its doc comment).
    fn graph_token(&self, force: bool) -> Result<String> {
        let mut cached = lock(&self.graph);
        if !force
            && let Some((t, _)) = cached
                .as_ref()
                .filter(|(_, exp)| now().saturating_add(120) < *exp)
        {
            return Ok(t.clone());
        }
        let (token, expires_in) = self.auth.graph_token()?;
        *cached = Some((token.clone(), now().saturating_add(expires_in.max(0))));
        Ok(token)
    }

    /// The token `send_message` sends a message with (see `auth::IC3_SCOPE`); `force` discards the cached one
    /// (after a 401). A tenant that withholds this scope fails this call with the rejection rather than any other
    /// scope's, and does not pause `SPACES_SCOPE`'s own browser renewal, the same way `graph_token` does not.
    fn ic3_token(&self, force: bool) -> Result<String> {
        let mut cached = lock(&self.ic3);
        if !force
            && let Some((t, _)) = cached
                .as_ref()
                .filter(|(_, exp)| now().saturating_add(120) < *exp)
        {
            return Ok(t.clone());
        }
        let (token, expires_in) = self.auth.ic3_token()?;
        *cached = Some((token.clone(), now().saturating_add(expires_in.max(0))));
        Ok(token)
    }

    /// Posts a message with this backend's own IC3-scoped token as the sole bearer, plus a few other headers a
    /// genuine reply's own captured request had: no skypetoken `Authentication` header, no client-version header,
    /// `Referer`, `X-Ms-Request-Priority`, `BehaviorOverride`, `X-Ms-Test-User`, `X-Ms-Migration`, `DNT`. All of
    /// this, and the IC3 token's own stamped identity claims (`fromGivenNameInToken` and friends on the stored
    /// message), were confirmed live, 2026-11-06, to make the request and the stored message an exact match for a
    /// genuine reply's own — and a reply sent this way still showed "this quoted message isn't verified by Teams"
    /// in the real client regardless (see the "Reply quotes" convention and `docs/teams-web-protocol.md`'s
    /// "Reply-quote verification" for the full history, including a `clientinfo` header — an explicit claim to be
    /// a specific other client and build — tried as a last resort and found to make no difference either, then
    /// reverted). Whatever the real client checks is evidently not observable in this request at all. Falls back
    /// to the ordinary spaces-token-plus-skypetoken bearer `request` uses for every other chatsvc call if an
    /// IC3-scoped token cannot be had at all (a restricted cloud withholding just this scope, say).
    fn send_message(&self, url: &str, body: &Value) -> Result<Value> {
        const OPERATION: &str = "send message";
        // Before any token is asked for or any header built: the same rule `request_with` applies, so the IC3 bearer
        // goes only to a Teams host too, whatever address built the URL.
        if !trusted_url(url) {
            return Err(Error::UntrustedUrl {
                url: url.to_string(),
            });
        }
        if let Err(e) = self.ic3_token(false) {
            log::warn!(
                "{OPERATION}: no IC3-scoped token ({}), sending without it",
                e.for_log()
            );
            return self.request(OPERATION, "POST", url, Some(body));
        }
        let call = Call {
            agent: &self.api,
            recorder: self.recorder.as_deref(),
            operation: OPERATION,
            method: "POST",
            url,
            body: Some(body),
        };
        http::call_json(&call, &mut |renewed| -> Result<Headers> {
            Ok(vec![
                (
                    "Authorization",
                    format!("Bearer {}", self.ic3_token(renewed)?),
                ),
                ("Referer", "https://teams.microsoft.com/".to_string()),
                ("X-Ms-Request-Priority", "0".to_string()),
                ("BehaviorOverride", "redirectAs404".to_string()),
                ("X-Ms-Test-User", "False".to_string()),
                ("X-Ms-Migration", "True".to_string()),
                ("DNT", "1".to_string()),
            ])
        })
    }

    /// A Microsoft Graph call with this backend's own Graph-scoped token: `Authorization` only, none of the
    /// Teams-specific headers [`Self::request_with`] sends (the skypetoken, `Origin`/`Referer` for
    /// `teams.microsoft.com`), since Graph is a separate, official service and none of those are its to receive.
    fn graph_request(
        &self,
        operation: &'static str,
        method: &str,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        // Graph's real host, or (in a test) this backend's own, replaceable idea of where it is.
        if !(url.starts_with(&self.graph_base) || shares::is_graph_url(url)) {
            return Err(Error::UntrustedUrl {
                url: url.to_string(),
            });
        }
        let call = Call {
            agent: &self.api,
            recorder: self.recorder.as_deref(),
            operation,
            method,
            url,
            body,
        };
        http::call_json(&call, &mut |renewed| -> Result<Headers> {
            let token = self.graph_token(renewed)?;
            Ok(vec![("Authorization", format!("Bearer {token}"))])
        })
    }

    /// Internal implementation for fetching images, with `skip_tenant_check` bypassing the tenant verification for
    /// explicitly requested content from outside the signed-in user's organization.
    fn fetch_image_impl(&self, url: &str, skip_tenant_check: bool) -> Result<RgbImage> {
        // A pasted or shared picture is on the sender's OneDrive, not Teams' own media service: resolved via
        // Graph's Shares API (shares.rs) to a download URL that is itself already authenticated, so it is fetched
        // with no credential of ours at all. There is no list of hosts to check it against (Graph chooses it), so
        // `shares::is_download_url` refuses what is never a public download: not https, an address instead of a
        // name, `localhost`, a user name or an unusual port, for each redirect hop too. Only pictures from the signed-in
        // user's own tenant are auto-loaded; those from other tenants (or whose tenant cannot be verified) are
        // refused unless explicitly requested.
        if shares::is_sharepoint_url(url) {
            let v = self.graph_request(
                "resolve shared file",
                "GET",
                &shares::shared_driveitem_url(&self.graph_base, url),
                None,
            )?;
            if !skip_tenant_check {
                let content_tenant = shares::tenant_id(&v);
                let my_tenant = self.auth.tenant_id();
                // A missing tenant ID for the content is unverified and not loaded automatically. The signed-in user's
                // tenant is always known (it is in the stored session), so only the content's tenant can be missing.
                if !shares::same_tenant(content_tenant.as_deref(), Some(&my_tenant)) {
                    return Err(Error::UnverifiedContent {
                        operation: "fetch image",
                        url: url.to_string(),
                    });
                }
            }
            let download_url = shares::download_url(&v)?;
            return http::fetch_image(&self.cdn, &download_url, "", &shares::is_download_url);
        }
        // The skypetoken must never go to a host named inside message content, except Teams' own media service.
        if !self.serves_image(url) {
            return Err(Error::UntrustedUrl {
                url: url.to_string(),
            });
        }
        let live = self.live(None)?;
        http::fetch_image(
            &self.cdn,
            url,
            &format!("skype_token {}", live.skype_token),
            &is_media_url,
        )
    }

    fn is_fresh(&self, chat_id: &str) -> bool {
        lock(&self.fresh).contains(chat_id)
    }

    /// Create the conversation for a 1:1 chat that has never had a message (what Teams does on the first send).
    fn create_one_on_one(&self, live: &Live, chat_id: &str) -> Result<()> {
        let members: Vec<Value> = pair_members(chat_id)
            .iter()
            .map(|id| json!({ "id": format!("8:orgid:{id}"), "role": "Admin" }))
            .collect();
        let body = json!({ "members": members, "properties": { "threadType": "chat", "fixedRoster": true, "uniquerosterthread": true } });
        self.request(
            "create chat",
            "POST",
            &format!("{}/v1/threads", live.chat_base),
            Some(&body),
        )
        .map(|_| ())
    }

    /// Credentials good for at least a few more minutes. Concurrent callers share one renewal instead of each starting
    /// their own: `live_gate` is held for the whole renewal, and every caller that needs one takes it and looks at the
    /// cache again before renewing, so the ones that waited find what the first one obtained. `stale` is the spaces
    /// token a caller just had refused with a 401, when this is a forced retry; if the cache has already moved past it
    /// (another caller renewed while this one waited for the gate), nothing more needs to happen. Renewing again
    /// regardless, as a bare `force: bool` would, serves every concurrent 401 its own grant and rotates the refresh
    /// token once per caller instead of once per actual expiry.
    ///
    /// **A caller whose credentials are still valid does not wait for a renewal it does not need.** The renewal is
    /// started early, while the cached credentials have minutes left ([`Enough::Comfortably`]); for that whole time
    /// they are still usable, so a caller without a 401 to answer for that finds the gate taken uses them at once
    /// rather than queueing behind the network. Callers with a `stale` token, with credentials that have actually run
    /// out or are missing, and every caller once the sign-in is known to be lost, wait for the gate and for the result
    /// as before.
    ///
    /// **Lock order:** `live_gate`, then (inside it, never held across anything else) `live`; `auth`'s own locks are
    /// taken under the gate only. The cache lock is never held while a request is made, while the gate is taken or
    /// while any `auth` lock is taken, so nothing takes them in the opposite order.
    fn live(&self, stale: Option<&str>) -> Result<Live> {
        #[cfg(test)]
        self.live_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(fresh) = self.cached_live(stale, Enough::Comfortably) {
            return Ok(fresh);
        }
        let attempt_before = self.renewal_attempt.load(Ordering::Relaxed);
        let _gate = match self.live_gate.try_lock() {
            Ok(gate) => gate,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                // Someone else is renewing. A caller with nothing to retry can go on with what is cached, if that
                // has not run out.
                if stale.is_none()
                    && let Some(current) = self.cached_live(None, Enough::Barely)
                {
                    log::trace!("Teams credentials are being renewed; using the current ones");
                    return Ok(current);
                }
                lock(&self.live_gate)
            }
        };
        // Another caller may have renewed while this one waited for the gate.
        if let Some(fresh) = self.cached_live(stale, Enough::Comfortably) {
            return Ok(fresh);
        }
        // A renewal attempt finished while this caller waited: use what it brought, even if it is short-lived, and if it
        // brought nothing it failed, so share that failure instead of renewing again. Not when that failure was the
        // sign-in being found dead: "paused" would hide that from the caller, who must be told it is signed out, and
        // asking the main scope again answers it at once from the remembered refusal, with no request.
        if self.renewal_attempt.load(Ordering::Relaxed) != attempt_before
            && !self.auth.sign_in_lost()
        {
            return self
                .cached_live(stale, Enough::Barely)
                .ok_or_else(|| crate::error::AuthError::RenewalPaused.into());
        }
        struct RenewalGuard<'a> {
            counter: &'a AtomicU64,
            armed: bool,
        }
        impl Drop for RenewalGuard<'_> {
            fn drop(&mut self) {
                if self.armed {
                    self.counter.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let mut guard = RenewalGuard {
            counter: &self.renewal_attempt,
            armed: false,
        };
        log::debug!("renewing the Teams session credentials");
        guard.armed = true;
        let fresh = self.auth.live()?;
        log::debug!(
            "Teams session renewed: access token good for {} s, skype token for {} s",
            fresh.spaces_exp - now(),
            fresh.skype_exp - now()
        );
        *lock(&self.live) = Some(fresh.clone());
        Ok(fresh)
    }

    /// The cached credentials, if they will do for this caller: with `stale`, any that are not the one refused;
    /// without, ones that are [`Enough`]. Never while the sign-in is known to be lost: once the grant is dead (a
    /// refusal found it, whichever scope was asking), the cached access tokens are not a reason to carry on as if
    /// nothing were wrong for the rest of their hour, and the next call for credentials fails as "signed out", so the
    /// person is told now, while they are at the keyboard.
    fn cached_live(&self, stale: Option<&str>, enough: Enough) -> Option<Live> {
        if self.auth.sign_in_lost() {
            return None;
        }
        let cached = lock(&self.live);
        let l = cached.as_ref()?;
        let good = match stale {
            Some(s) => l.spaces_token != s,
            None => {
                let margin = match enough {
                    Enough::Comfortably => (120, 300),
                    Enough::Barely => (RUNNING_OUT, RUNNING_OUT),
                };
                now().saturating_add(margin.0) < l.spaces_exp
                    && now().saturating_add(margin.1) < l.skype_exp
            }
        };
        good.then(|| l.clone())
    }

    fn request(
        &self,
        operation: &'static str,
        method: &str,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        self.request_with(Bearer::Spaces, operation, method, url, body)
    }

    /// [`request`](Self::request) with the bearer token a particular Teams service wants.
    fn request_with(
        &self,
        bearer: Bearer,
        operation: &'static str,
        method: &str,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        // Tokens only ever go to Teams hosts, even if a paging link in a response said otherwise.
        if !trusted_url(url) {
            return Err(Error::UntrustedUrl {
                url: url.to_string(),
            });
        }
        let call = Call {
            agent: &self.api,
            recorder: self.recorder.as_deref(),
            operation,
            method,
            url,
            body,
        };
        let mut used_spaces_token: Option<String> = None;
        http::call_json(&call, &mut |renewed| -> Result<Headers> {
            // On a 401, renew only the bearer that was refused: for a side bearer (CSA, Presence), renew that side token
            // without triggering a main renewal; for the main Spaces bearer, trigger the main renewal.
            let stale = (renewed && matches!(bearer, Bearer::Spaces))
                .then_some(used_spaces_token.as_deref())
                .flatten();
            let live = self.live(stale)?;
            used_spaces_token = Some(live.spaces_token.clone());
            let bearer = match bearer {
                Bearer::Spaces => live.spaces_token.clone(),
                Bearer::Csa => self.csa_token(renewed)?,
                Bearer::Presence => self.presence_token(renewed)?,
            };
            Ok(vec![
                ("Authentication", format!("skypetoken={}", live.skype_token)),
                ("Authorization", format!("Bearer {bearer}")),
                ("Origin", "https://teams.microsoft.com".to_string()),
                ("Referer", "https://teams.microsoft.com/".to_string()),
                ("Accept", "application/json".to_string()),
                ("X-Ms-Client-Version", self.client_version.clone()),
            ])
        })
    }
    /// Teams' emoji catalogue: downloaded once, from the first published address that answers, and kept. After a
    /// failure the download is not tried again for a minute, so a CDN that is down does not cost a wait for every
    /// reaction or opening of the picker. The lock is not held while downloading; two callers that arrive together
    /// may both download, which costs one extra request and nothing else.
    fn catalog(&self) -> Result<Arc<emoji_catalog::Catalog>> {
        if let Some(catalog) = lock(&self.emoji_catalog).as_ref() {
            return Ok(Arc::clone(catalog));
        }
        if lock(&self.emoji_catalog_retry).is_some_and(|until| Instant::now() < until) {
            return Err(Error::Unexpected {
                operation: "read the emoji list",
                detail: "the list could not be read a moment ago; not trying again yet",
            });
        }
        let mut last = None;
        for url in &self.emoji_catalog_urls {
            let fetched = http::fetch_public_text(
                &self.api,
                "read the emoji list",
                url,
                emoji_catalog::MAX_CATALOG_BYTES,
            )
            .and_then(|text| emoji_catalog::Catalog::parse(&text));
            match fetched {
                Ok(catalog) => {
                    let catalog = Arc::new(catalog);
                    *lock(&self.emoji_catalog) = Some(Arc::clone(&catalog));
                    *lock(&self.emoji_catalog_retry) = None;
                    return Ok(catalog);
                }
                Err(e) => {
                    log::warn!("the emoji list could not be read: {}", e.for_log());
                    last = Some(e);
                }
            }
        }
        *lock(&self.emoji_catalog_retry) = Some(Instant::now() + self.emoji_catalog_pause);
        Err(last.unwrap_or(Error::Unexpected {
            operation: "read the emoji list",
            detail: "no address to read it from",
        }))
    }

    fn incremental_on(&self) -> bool {
        self.incremental.load(Ordering::Relaxed)
    }

    /// A 4xx answer to an incremental request: the service may not accept that kind of request. (Timeouts and
    /// conflicts are not about the kind of request.)
    fn refusal(e: &Error) -> bool {
        matches!(
            e,
            Error::Status {
                status: 400..=499,
                ..
            }
        ) && !matches!(e.status(), Some(408 | 409 | 423 | 425))
    }

    /// An incremental request was refused but the full read that followed worked. One refusal is ordinary (a change
    /// feed link that has expired); the same thing three times running, with no incremental read that worked in
    /// between, means the service does not take them, and then they are given up. True when that is now.
    fn refusal_ends_increments(&self, operation: &'static str) -> bool {
        let n = self.increment_refusals.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= MAX_INCREMENT_REFUSALS {
            return true;
        }
        log::warn!(
            "{operation}: an incremental read was refused ({n} of {MAX_INCREMENT_REFUSALS}); reading in full"
        );
        false
    }

    /// An incremental read worked: refusals so far were isolated.
    fn increments_work(&self) {
        self.increment_refusals.store(0, Ordering::Relaxed);
    }

    /// Stop asking for increments for the rest of the session.
    fn give_up_on_increments(&self, operation: &'static str) {
        self.incremental.store(false, Ordering::Relaxed);
        log::warn!("{operation}: incremental reads are switched off for this session");
        self.auth.warn(Error::Unexpected {
            operation,
            detail: "the service refused an incremental read; reading everything each time instead",
        });
    }

    /// The raw conversations: those that changed since the last read when that can be known, else the whole list.
    fn conversations(&self, live: &Live) -> Result<Vec<Value>> {
        const OPERATION: &str = "list chats";
        let mut refused = false;
        if self.incremental_on() {
            let link = lock(&self.chat_index)
                .as_ref()
                .filter(|i| i.is_fresh(self.full_refresh))
                .and_then(|i| i.sync.clone());
            if let Some(link) = link {
                match self.request(OPERATION, "GET", &link, None) {
                    Ok(answer) => {
                        let changed = answer["conversations"]
                            .as_array()
                            .map_or(&[][..], Vec::as_slice);
                        self.watch_conversations(&answer, changed);
                        log::debug!(
                            "chat list: {} conversation(s) in the change feed",
                            changed.len()
                        );
                        self.increments_work();
                        if let Some(index) = lock(&self.chat_index).as_mut() {
                            index.merge(&answer, trusted_url);
                            // Entries without an id cannot be merged; they still go on to be reported as unreadable.
                            let mut all = index.conversations();
                            all.extend(
                                answer["conversations"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter(|c| c["id"].as_str().is_none())
                                    .cloned(),
                            );
                            return Ok(all);
                        }
                    }
                    Err(e) if Self::refusal(&e) => {
                        log::warn!(
                            "chat list: the change feed was refused ({}); reading in full",
                            e.for_log()
                        );
                        refused = true;
                        *lock(&self.chat_index) = None;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        let url = format!(
            "{}/v1/users/ME/conversations?view=msnp24Equivalent&pageSize={}",
            live.chat_base, self.max_chats
        );
        let answer = self.request(OPERATION, "GET", &url, None)?;
        let all = answer["conversations"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        self.watch_conversations(&answer, all);
        log::debug!(
            "chat list: read in full, {} conversation(s){}",
            all.len(),
            if answer["_metadata"]["syncState"].is_string() {
                ", with a change feed"
            } else {
                ", no change feed"
            }
        );
        if refused && self.refusal_ends_increments(OPERATION) {
            self.give_up_on_increments(OPERATION);
        } else if self.incremental_on() {
            let mut index = lock(&self.chat_index);
            let chain = index.as_ref().and_then(|i| i.sync.clone());
            *index = Some(incremental::ChatIndex::from_full(
                &answer,
                trusted_url,
                chain,
            ));
        }
        Ok(answer["conversations"]
            .as_array()
            .cloned()
            .unwrap_or_default())
    }

    /// The newest page of `chat_id` brought up to date from what changed since it was last read. `None` when there is
    /// nothing recent to build on or the answer may have been cut short; the caller then reads the page in full.
    fn messages_since(&self, chat_id: &str) -> Result<Option<Page>> {
        const OPERATION: &str = "read messages";
        let from = lock(&self.threads)
            .get_mut(chat_id)
            .and_then(|t| t.since(self.full_refresh));
        let Some(from) = from else {
            log::debug!(
                "messages {}: reading in full (nothing recent to build on)",
                id_tag(chat_id)
            );
            return Ok(None);
        };
        let live = self.live(None)?;
        let url = format!(
            "{}/v1/users/ME/conversations/{}/messages?pageSize={}&view=msnp24Equivalent&startTime={from}",
            live.chat_base,
            urlencode(chat_id),
            incremental::PAGE
        );
        let v = self.request(OPERATION, "GET", &url, None)?;
        let items = v["messages"].as_array().map_or(&[][..], Vec::as_slice);
        self.watch_messages(&v, items);
        self.watch_repeats(chat_id, items);
        if items.len() >= incremental::PAGE {
            log::debug!(
                "messages {}: the incremental answer has {} items and may be cut short; reading in full",
                id_tag(chat_id),
                items.len()
            );
            return Ok(None);
        }
        for m in items {
            self.learn(m);
        }
        let shown = self.without_resends(chat_id, items);
        let incoming = self.collect(OPERATION, &shown, parse_msg);
        let newest = items.iter().filter_map(incremental::stamp).max();
        log::debug!(
            "messages {}: {} changed since version {from}, newest now {newest:?}",
            id_tag(chat_id),
            items.len()
        );
        Ok(lock(&self.threads)
            .get_mut(chat_id)
            .map(|t| t.merge(incoming, newest)))
    }

    /// Note in the log, by counts and field names only, messages that look like repeats of one another: the same
    /// sender and content several times (and whether they share a client message id, which a client can use to show
    /// a message sent again after a failure once), and one message that holds the same paragraph several times.
    /// Never the text, an id or a name. Each is noted once.
    fn watch_repeats(&self, chat_id: &str, items: &[Value]) {
        use std::hash::{Hash, Hasher};
        // Everything below only feeds an info-level log line: with that level off (the default in a release build is info, a
        // person can set warn or off) there is nothing to look for. This is separate from without_resends, which
        // changes what is shown and always runs.
        if !log::log_enabled!(log::Level::Info) {
            return;
        }
        let first_time = |signature: &str| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            signature.hash(&mut hasher);
            let mut seen = lock(&self.repeats_seen);
            seen.len() < 256 && seen.insert(hasher.finish())
        };
        let mut same: HashMap<(&str, &str), Vec<&Value>> = HashMap::new();
        for m in items {
            let content = m["content"].as_str().unwrap_or("");
            if content.len() >= 8 {
                same.entry((m["from"].as_str().unwrap_or(""), content))
                    .or_default()
                    .push(m);
            }
        }
        for group in same.values().filter(|g| g.len() > 1) {
            let ids: Vec<String> = group.iter().map(|m| m["id"].to_string()).collect();
            if !first_time(&ids.join(",")) {
                continue;
            }
            let client_ids: Vec<&str> = group
                .iter()
                .filter_map(|m| m["clientmessageid"].as_str())
                .filter(|c| !c.is_empty())
                .collect();
            let shared =
                client_ids.len() == group.len() && client_ids.iter().all(|c| *c == client_ids[0]);
            let mut differing: Vec<&str> = group[0]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, _)| k.as_str())
                .filter(|k| group.iter().any(|m| m[*k] != group[0][*k]))
                .collect();
            differing.sort_unstable();
            let times: Vec<i64> = group
                .iter()
                .filter_map(|m| m["originalarrivaltime"].as_str())
                .filter_map(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.timestamp())
                .collect();
            let span = times
                .iter()
                .max()
                .zip(times.iter().min())
                .map(|(a, b)| a - b);
            log::info!(
                "messages {}: {} messages from one sender have identical content; share one client message id: {shared}; arrival times span {span:?} s; fields that differ: {differing:?}",
                id_tag(chat_id),
                group.len()
            );
        }
        for m in items {
            let content = m["content"].as_str().unwrap_or("");
            let mut paragraphs: HashMap<&str, usize> = HashMap::new();
            for block in content
                .split("</p>")
                .map(str::trim)
                .filter(|b| b.len() >= 20)
            {
                *paragraphs.entry(block).or_default() += 1;
            }
            let most = paragraphs.values().copied().max().unwrap_or(0);
            if most >= 3 && first_time(&format!("para {}", m["id"])) {
                log::info!(
                    "messages {}: one message holds the same paragraph {most} times ({} bytes in {} paragraphs)",
                    id_tag(chat_id),
                    content.len(),
                    paragraphs.values().sum::<usize>()
                );
            }
        }
    }

    /// `items` without the copies of a message that was sent again: a client that does not hear back sends the same
    /// message several times with one client message id (seen: five copies, over 46 s, in one chat), the service stores
    /// each as a message of its own, and the Teams client shows it once. Messages from one sender with the same client
    /// message id **and** the same content are one message, and the first to arrive is the one shown; a later copy,
    /// also when it comes on its own in an incremental read, is left out. A message that only shares an id with another
    /// (different words) is never dropped, nor is one without a client message id.
    fn without_resends(&self, chat_id: &str, items: &[Value]) -> Vec<Value> {
        use std::hash::{Hash, Hasher};
        let key_of = |m: &Value| -> Option<u64> {
            let (from, client, content) = (
                m["from"].as_str()?,
                m["clientmessageid"].as_str()?,
                m["content"].as_str()?,
            );
            if from.is_empty() || client.is_empty() || content.is_empty() {
                return None;
            }
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            (from, client, content).hash(&mut hasher);
            Some(hasher.finish())
        };
        let id_of = |m: &Value| {
            m["id"]
                .as_str()
                .map_or_else(|| m["id"].to_string(), String::from)
        };
        let mut guard = lock(&self.resends);
        let state = guard.entry(chat_id.to_string()).or_default();
        if state.kept.len() > MAX_RESENDS || state.dropped.len() > MAX_RESENDS {
            *state = Resends::default();
        }
        // Earliest first, so that the first to arrive is the one that is kept.
        let mut order: Vec<usize> = (0..items.len()).collect();
        order.sort_by_key(|&i| {
            (
                items[i]["originalarrivaltime"]
                    .as_str()
                    .or(items[i]["composetime"].as_str())
                    .unwrap_or("")
                    .to_string(),
                id_of(&items[i]).len(),
                id_of(&items[i]),
            )
        });
        let mut left_out = HashSet::new();
        for i in order {
            let id = id_of(&items[i]);
            if state.dropped.contains(&id) {
                left_out.insert(i);
                continue;
            }
            let Some(key) = key_of(&items[i]) else {
                continue;
            };
            match state.kept.get(&key) {
                Some(kept) if *kept != id => {
                    state.dropped.insert(id);
                    left_out.insert(i);
                }
                Some(_) => {}
                None => {
                    state.kept.insert(key, id);
                }
            }
        }
        if !left_out.is_empty() {
            log::debug!(
                "messages {}: {} copies of messages sent again left out",
                id_tag(chat_id),
                left_out.len()
            );
        }
        items
            .iter()
            .enumerate()
            .filter(|(i, _)| !left_out.contains(i))
            .map(|(_, m)| m.clone())
            .collect()
    }

    /// Note the fields, message types and answer structure of a page of messages (names and types only).
    fn watch_messages(&self, answer: &Value, items: &[Value]) {
        self.watch.fields("messages answer", [answer]);
        self.watch
            .fields("messages answer metadata", [&answer["_metadata"]]);
        self.watch.fields("message", items);
        self.watch
            .fields("message properties", items.iter().map(|m| &m["properties"]));
        self.watch.tokens(
            "message type",
            items.iter().filter_map(|m| m["messagetype"].as_str()),
        );
    }

    /// Note the structure of the conversation list (names and types only).
    fn watch_conversations(&self, answer: &Value, convs: &[Value]) {
        self.watch.fields("chat list answer", [answer]);
        self.watch
            .fields("chat list answer metadata", [&answer["_metadata"]]);
        self.watch.fields("conversation", convs);
        self.watch.fields(
            "conversation last message",
            convs.iter().map(|c| &c["lastMessage"]),
        );
        self.watch.fields(
            "conversation properties",
            convs.iter().map(|c| &c["properties"]),
        );
        self.watch.fields(
            "conversation thread properties",
            convs.iter().map(|c| &c["threadProperties"]),
        );
        self.watch.tokens(
            "last message type",
            convs
                .iter()
                .filter_map(|c| c["lastMessage"]["messagetype"].as_str()),
        );
    }

    /// Parse the elements of a list answer. Elements that are deliberately not shown are dropped quietly; if some
    /// could not be read at all, that is reported once as a warning, because it usually means the service changed its
    /// format and the list would otherwise just look shorter.
    fn collect<T>(
        &self,
        operation: &'static str,
        items: &[Value],
        parse: impl Fn(&Value) -> Parsed<T>,
    ) -> Vec<T> {
        let mut out = Vec::new();
        let (mut skipped, mut malformed) = (0usize, 0usize);
        for item in items {
            match parse(item) {
                Parsed::Item(t) => out.push(t),
                Parsed::Skipped => skipped += 1,
                Parsed::Malformed => {
                    malformed += 1;
                    if malformed <= 3 {
                        log::warn!(
                            "{operation}: unreadable item, shape {}",
                            watch::masked(item)
                        );
                    }
                }
            }
        }
        log::debug!(
            "{operation}: {} item(s), {} read, {skipped} skipped on purpose, {malformed} unreadable",
            items.len(),
            out.len()
        );
        if malformed > 0 {
            self.auth.warn(Error::Unexpected { operation, detail: "some items in the answer could not be read; the service format may have changed" });
        }
        out
    }

    /// Remember who a sender is, from the `from` + `imdisplayname` pair on a message.
    fn learn(&self, m: &Value) {
        if let (Some(id), Some(name)) = (
            m["from"].as_str().and_then(mri),
            m["imdisplayname"]
                .as_str()
                .map(clean)
                .filter(|n| !n.is_empty()),
        ) {
            lock(&self.names).insert(id, name);
        }
    }

    fn name_of(&self, id: &str) -> String {
        lock(&self.names).get(id).cloned().unwrap_or_default()
    }

    /// Stop looking names up for a while after a failure: five minutes, twice as long for each failure in a row (the
    /// service has been unable to answer, and asking again at once only adds load), at most an hour.
    fn pause_lookups(&self) {
        let failures = self.lookup_failures.fetch_add(1, Ordering::Relaxed).min(4);
        let wait = (LOOKUP_PAUSE * (1 << failures)).min(MAX_LOOKUP_PAUSE);
        *lock(&self.lookup_paused_until) = Some(Instant::now() + wait);
    }

    /// Let this backend do best-effort work, the lookup of names for chat titles, on a thread of its own instead of
    /// inside the call that wanted it. A lookup can take a minute when the directory is slow (each failing request
    /// waits out the service's own 10 s limit), and nothing about a title is worth making a list wait for: the list
    /// comes back at once with the names it has, and the next read has the rest. Without this call the lookup is done
    /// in the call, which is what a program that does not mind waiting, and the tests, get. Only the first call
    /// counts.
    pub fn attach(self: &Arc<Self>) {
        let _ = self.background.set(Arc::downgrade(self));
    }

    /// Look up the names of `ids`: on a thread of its own if the backend was attached and none is running, else here.
    fn look_up_names(&self, live: &Live, ids: &[String], named_too: bool) -> Result<()> {
        let Some(me) = self.background.get().and_then(Weak::upgrade) else {
            return self.resolve_names(live, ids, named_too);
        };
        let nothing_to_do = live.mt_base.is_empty()
            || lock(&self.lookup_paused_until).is_some_and(|until| Instant::now() < until);
        if nothing_to_do || me.lookup_running.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let ids = ids.to_vec();
        let spawned = thread::Builder::new()
            .name("teams-names".to_string())
            .spawn({
                let me = Arc::clone(&me);
                move || {
                    let _running = Running(&me.lookup_running);
                    #[cfg(test)]
                    {
                        if me.panic_in_lookup.load(Ordering::SeqCst) {
                            panic!("test-injected panic in name lookup");
                        }
                    }
                    let marks = me.unlisted_changes.load(Ordering::SeqCst);
                    let result = me
                        .live(None)
                        .and_then(|live| me.resolve_names(&live, &ids, named_too));
                    if let Err(e) = result {
                        me.auth.warn(e);
                    }
                    if me.marks_changed_since(marks) {
                        // Someone's place in the directory changed: the list is drawn differently now.
                        drop(_running);
                        me.titles_can_improve();
                    }
                }
            });
        if spawned.is_err() {
            me.lookup_running.store(false, Ordering::SeqCst);
            log::warn!("could not start the name lookup thread; titles may lack names");
        }
        Ok(())
    }

    /// One people search for `q` (Substrate's suggestions), asking for phone numbers and office location too when
    /// `extras` is set.
    fn search_people(&self, q: &str, extras: bool) -> Result<Vec<Contact>> {
        const OPERATION: &str = "search people";
        let mut fields = vec![
            "Id",
            "MRI",
            "DisplayName",
            "EmailAddresses",
            "GivenName",
            "Surname",
            "JobTitle",
            "Department",
            "CompanyName",
        ];
        if extras {
            fields.extend(["Phones", "OfficeLocation"]);
        }
        let body = json!({
            "EntityRequests": [{
                "Query": { "QueryString": q, "DisplayQueryString": q },
                "EntityType": "People",
                "Size": 10,
                "Fields": fields
            }],
            "cvid": guid(),
            "logicalId": guid()
        });
        let call = Call {
            agent: &self.api,
            recorder: self.recorder.as_deref(),
            operation: OPERATION,
            method: "POST",
            url: &self.people_url,
            body: Some(&body),
        };
        let answer = http::call_json(&call, &mut |renewed| -> Result<Headers> {
            Ok(vec![
                (
                    "Authorization",
                    format!("Bearer {}", self.substrate_token(renewed)?),
                ),
                ("Origin", "https://teams.microsoft.com".to_string()),
                ("Referer", "https://teams.microsoft.com/".to_string()),
                ("Accept", "application/json".to_string()),
            ])
        })?;
        Ok(parse_people(&answer))
    }

    /// The chat member `id`, with the name known for them and whether the directory left them out of a lookup.
    pub(crate) fn member(&self, id: String) -> Member {
        // Never the signed-in person, whatever an answer left out: they are plainly still here.
        let not_in_directory =
            lock(&self.unlisted).contains(&id) && !self.auth.me().0.eq_ignore_ascii_case(&id);
        Member {
            name: self.name_of(&id),
            not_in_directory,
            id,
        }
    }

    /// Look up display names for ids we have not seen as senders, and with `named_too` also those whose name is known
    /// already (from their messages): the people of the chat list, so the directory says whether it still lists them
    /// (`Member::not_in_directory`). Each id is asked about once; after a failure no lookups are attempted for a while.
    fn resolve_names(&self, live: &Live, ids: &[String], named_too: bool) -> Result<()> {
        if live.mt_base.is_empty()
            || lock(&self.lookup_paused_until).is_some_and(|until| Instant::now() < until)
        {
            return Ok(());
        }
        // Each id once, in the order first seen: a chat list names the same person in many chats, and a request that
        // repeats an id is longer than it needs to be.
        let unknown: Vec<&String> = {
            let asked = lock(&self.looked_up);
            let mut seen = HashSet::new();
            ids.iter()
                .filter(|i| {
                    (named_too || self.name_of(i).is_empty())
                        && !asked.contains(i.as_str())
                        && seen.insert(*i)
                })
                .collect()
        };
        if unknown.is_empty() {
            return Ok(());
        }
        let batches: Vec<&[&String]> = unknown.chunks(NAME_BATCH).collect();
        // Only sizes are logged, never an id or a name: what shape of request the service chokes on (a long one,
        // one with repeated ids) is what these lines are for.
        log::debug!(
            "names: {} unknown id(s), {} distinct, in {} batch(es) of up to {NAME_BATCH}",
            unknown.len(),
            unknown.iter().collect::<HashSet<_>>().len(),
            batches.len()
        );
        let url = format!(
            "{}/beta/users/fetchShortProfile?{PROFILE_QUERY}",
            live.mt_base
        );
        // Work still to do, the next one last. A batch the service fails on is split in halves and both are tried, so
        // one id it cannot look up costs the rest nothing but a few failed requests.
        let mut work: Vec<(String, &[&String])> = batches
            .iter()
            .enumerate()
            .rev()
            .map(|(n, b)| (format!("batch {} of {}", n + 1, batches.len()), *b))
            .collect();
        let mut split_until: Option<Instant> = None;
        let mut answered = false;
        let mut suspects: Vec<&String> = Vec::new();
        let mut first_failure: Option<Error> = None;
        while let Some((label, batch)) = work.pop() {
            let body = Value::Array(
                batch
                    .iter()
                    .map(|i| Value::String(format!("8:orgid:{i}")))
                    .collect(),
            );
            let shape = format!(
                "{label}: {} id(s), {} distinct, {} bytes sent",
                batch.len(),
                batch.iter().collect::<HashSet<_>>().len(),
                body.to_string().len()
            );
            let started = Instant::now();
            let v = match self.request("look up names", "POST", &url, Some(&body)) {
                Ok(v) => v,
                // Names are a nicety: when the service asks us to slow down we simply wait, without bothering anyone.
                Err(e) if throttle_pause(&e).is_some() => {
                    let wait = throttle_pause(&e).unwrap_or(DEFAULT_THROTTLE_PAUSE);
                    log::warn!(
                        "names: {shape}: the service asked to slow down after {} ms; lookups paused for {} s",
                        started.elapsed().as_millis(),
                        wait.as_secs()
                    );
                    *lock(&self.lookup_paused_until) = Some(Instant::now() + wait);
                    return Ok(());
                }
                Err(e)
                    if e.status().is_some_and(|s| s >= 500)
                        && split_until.is_none_or(|until| Instant::now() < until) =>
                {
                    log::warn!(
                        "names: {shape}: failed after {} ms: {}",
                        started.elapsed().as_millis(),
                        e.for_log()
                    );
                    split_until.get_or_insert(Instant::now() + SPLIT_BUDGET);
                    if let [only] = batch {
                        suspects.push(only);
                    } else {
                        let (first, second) = batch.split_at(batch.len() / 2);
                        work.push((format!("{label}, second half"), second));
                        work.push((format!("{label}, first half"), first));
                    }
                    first_failure.get_or_insert(e);
                    continue;
                }
                Err(e) => {
                    log::warn!(
                        "names: {shape}: failed after {} ms: {}",
                        started.elapsed().as_millis(),
                        e.for_log()
                    );
                    self.pause_lookups();
                    return Err(e);
                }
            };
            let profiles = v["value"].as_array().map_or(0, Vec::len);
            let mut names = lock(&self.names);
            let mut listed: HashSet<String> = HashSet::new();
            for p in v["value"].as_array().into_iter().flatten() {
                let Some(id) = p["mri"].as_str().and_then(mri) else {
                    continue;
                };
                listed.insert(id.clone());
                if let Some(name) = p["displayName"]
                    .as_str()
                    .map(clean)
                    .filter(|n| !n.is_empty())
                {
                    names.insert(id, name);
                }
            }
            drop(names);
            // An answer that lists nobody says nothing about anyone: it may be the service, not the people.
            let left_out = if listed.is_empty() {
                0
            } else {
                let mut unlisted = lock(&self.unlisted);
                let mut left_out = 0;
                let mut changed = false;
                for id in batch {
                    if listed.contains(id.as_str()) {
                        changed |= unlisted.remove(id.as_str());
                    } else {
                        left_out += 1;
                        changed |= unlisted.insert((*id).clone());
                    }
                }
                if changed {
                    self.unlisted_changes.fetch_add(1, Ordering::SeqCst);
                }
                left_out
            };
            log::debug!(
                "names: {shape}, {profiles} profile(s) back, {left_out} not in the directory, {} ms",
                started.elapsed().as_millis()
            );
            lock(&self.looked_up).extend(batch.iter().map(|id| (*id).clone()));
            answered = true;
            self.lookup_failures.store(0, Ordering::Relaxed);
        }
        let Some(failure) = first_failure else {
            return Ok(());
        };
        if answered {
            // Other lookups in this call worked, so the service is up and these ids are what it cannot do: stop asking
            // about them.
            log::warn!(
                "names: gave up on {} id(s) the service could not look up",
                suspects.len()
            );
            lock(&self.looked_up).extend(suspects.into_iter().cloned());
        } else {
            // Nothing was answered, so this is the service (or the network) and not any one id: try again later.
            self.pause_lookups();
            return Err(failure);
        }
        Ok(())
    }

    /// Read the rosters of the chats in `wanted` (group chats without a name, whose title needs their people) into
    /// the member cache the list is titled from. Returns how many were read and the first failure that is not the
    /// service asking us to slow down (that one only pauses the lookups: a title is not worth a warning).
    ///
    /// The first read goes alone and the rest only follow if it was not turned away, `MEMBER_LOOKUPS_AT_ONCE` at a
    /// time: a service that is throttling answers every request of a burst with a refusal (seen live at start-up, four
    /// refusals within 250 ms, and again on the next refresh), where one request would have told us to wait.
    fn fetch_rosters(&self, live: &Live, wanted: &[String]) -> (usize, Option<Error>) {
        let mut learned = 0;
        let mut failure: Option<Error> = None;
        let (first, rest) = wanted.split_at(wanted.len().min(1));
        for batch in std::iter::once(first)
            .chain(rest.chunks(MEMBER_LOOKUPS_AT_ONCE))
            .filter(|batch| !batch.is_empty())
        {
            let fetched: Vec<(String, Result<Vec<String>>)> = std::thread::scope(|s| {
                let handles: Vec<_> = batch
                    .iter()
                    .map(|id| (id, s.spawn(|| self.fetch_members(live, id))))
                    .collect();
                handles
                    .into_iter()
                    .map(|(id, h)| {
                        let result = h.join().unwrap_or_else(|_| {
                            Err(Error::Unexpected {
                                operation: "read chat members",
                                detail: "the lookup thread panicked",
                            })
                        });
                        (id.clone(), result)
                    })
                    .collect()
            });
            let mut known = lock(&self.members);
            let mut slow_down: Option<Duration> = None;
            for (id, result) in fetched {
                match result {
                    Ok(ids) => {
                        known.insert(id, ids);
                        learned += 1;
                    }
                    Err(e) => match throttle_pause(&e) {
                        Some(wait) => slow_down = slow_down.max(Some(wait)),
                        None => {
                            lock(&self.failed_members).insert(id, Instant::now());
                            failure = failure.or(Some(e));
                        }
                    },
                }
            }
            // Titles are not worth hammering the service for: stop, and try the rest after the wait.
            if let Some(wait) = slow_down {
                log::warn!(
                    "chat titles: the service asked to slow down; member lookups paused for {} s",
                    wait.as_secs()
                );
                *lock(&self.members_paused_until) = Some(Instant::now() + wait);
                break;
            }
        }
        (learned, failure)
    }

    /// Read `wanted` rosters and then the names of `ids` (and of the people just found) on the helper thread, and tell
    /// the caller through [`Backend::poll_pushes`] when the list can be titled better. Returns whether it started: it
    /// does not when the thread is busy (with this or with a name lookup), and the next read of the list asks again.
    fn start_roster_job(me: &Arc<Self>, wanted: Vec<String>, mut ids: Vec<String>) -> bool {
        if me.lookup_running.swap(true, Ordering::SeqCst) {
            return false;
        }
        let spawned = thread::Builder::new()
            .name("teams-names".to_string())
            .spawn({
                let me = Arc::clone(me);
                move || {
                    let running = Running(&me.lookup_running);
                    log::debug!(
                        "chat titles: reading {} roster(s) on the helper thread",
                        wanted.len()
                    );
                    let mut learned = 0;
                    let marks = me.unlisted_changes.load(Ordering::SeqCst);
                    let result = me.live(None).and_then(|live| {
                        let (read, failure) = me.fetch_rosters(&live, &wanted);
                        learned = read;
                        log::debug!("chat titles: {read} of {} roster(s) read", wanted.len());
                        if read > 0 {
                            // Titles made from names already known need no more waiting for; the names still to be
                            // looked up may take a while.
                            me.titles_can_improve();
                        }
                        {
                            let known = lock(&me.members);
                            ids.extend(
                                wanted
                                    .iter()
                                    .filter_map(|c| known.get(c))
                                    .flatten()
                                    .cloned(),
                            );
                        }
                        let named = me.resolve_names(&live, &ids, true);
                        failure.map_or(named, Err)
                    });
                    if let Err(e) = result {
                        me.auth.warn(e);
                    }
                    // The thread is free again before anyone is told: the read this prompts may need it for the
                    // rosters a long list still has (one job reads at most `MEMBER_LOOKUPS_PER_READ`).
                    drop(running);
                    if learned > 0 || me.marks_changed_since(marks) {
                        me.titles_can_improve();
                    }
                }
            });
        if spawned.is_err() {
            me.lookup_running.store(false, Ordering::SeqCst);
            log::warn!("could not start the roster lookup thread; titles may lack names");
        }
        spawned.is_ok()
    }

    /// Whether anyone's place in the directory (`unlisted`) has changed since `unlisted_changes` was `marks`.
    fn marks_changed_since(&self, marks: u64) -> bool {
        self.unlisted_changes.load(Ordering::SeqCst) != marks
    }

    /// Say, through the push hints, that a list read now would be titled better than the last one: the app refreshes
    /// at once instead of at its next scheduled read.
    fn titles_can_improve(&self) {
        trouter::queue_hint(&mut lock(&self.push_queue), PushHint::Unknown);
    }

    fn fetch_members(&self, live: &Live, chat_id: &str) -> Result<Vec<String>> {
        let url = format!(
            "{}/v1/threads/{}?view=msnp24Equivalent",
            live.chat_base,
            urlencode(chat_id)
        );
        let v = self.request("read chat members", "GET", &url, None)?;
        Ok(v["members"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().or(m["mri"].as_str()).and_then(mri))
            .collect())
    }
}

impl Backend for TeamsBackend {
    fn label(&self) -> String {
        "Teams web API".into()
    }

    fn members(&self, chat_id: &str) -> Result<Vec<Member>> {
        /// Most people of one chat that are returned (a very large meeting): the rest are not asked about.
        const MAX_ROSTER: usize = 500;
        let live = self.live(None)?;
        let known = lock(&self.members).get(chat_id).cloned();
        let mut ids = match known {
            Some(ids) => ids,
            None => {
                let ids = self.fetch_members(&live, chat_id)?;
                lock(&self.members).insert(chat_id.to_string(), ids.clone());
                ids
            }
        };
        ids.truncate(MAX_ROSTER);
        // Names are looked up (on a thread of their own when attached) and found by a later call; a failure to look
        // them up does not fail the roster.
        if let Err(e) = self.look_up_names(&live, &ids, true) {
            log::debug!("members: names: {}", e.for_log());
        }
        Ok(ids.into_iter().map(|id| self.member(id)).collect())
    }

    fn me(&self) -> Result<Me> {
        self.live(None)?;
        let (id, name) = self.auth.me();
        if id.is_empty() {
            return Err(Error::Unexpected {
                operation: "read profile",
                detail: "the stored session has no user id",
            });
        }
        Ok(Me {
            id,
            name: clean(&name),
        })
    }

    fn chats(&self) -> Result<Vec<Chat>> {
        const OPERATION: &str = "list chats";
        let live = self.live(None)?;
        let convs = self.conversations(&live)?;
        for c in &convs {
            self.learn(&c["lastMessage"]);
        }
        // The teams and channels of the list are not chats, but what they say of their own activity is remembered.
        self.note_channel_activity(&convs);
        let mut parsed: Vec<(Chat, Vec<String>)> =
            self.collect(OPERATION, &convs, parse_conversation);

        // Titles are best effort: a failed lookup leaves a chat untitled and is reported once as a warning, but never
        // fails the list. Group chats without a topic need their member list; fetch each once, a few at a time.
        let mut lookup_failure: Option<Error> = None;
        let paused = lock(&self.members_paused_until).is_some_and(|until| Instant::now() < until);
        let wanted: Vec<String> = {
            let known = lock(&self.members);
            let failed = lock(&self.failed_members);
            parsed
                .iter()
                .filter(|(c, ids)| {
                    ids.is_empty()
                        && c.topic.is_none()
                        && c.kind != ChatKind::OneOnOne
                        && !known.contains_key(&c.id)
                        && !failed
                            .get(&c.id)
                            .is_some_and(|at| at.elapsed() < self.full_refresh)
                })
                .map(|(c, _)| c.id.clone())
                .take(if paused { 0 } else { MEMBER_LOOKUPS_PER_READ })
                .collect()
        };
        // An attached backend (see `attach`) does not wait for the rosters: the list comes back at once with what it
        // knows (a chat whose people are not known yet is "(untitled chat)", never "(just you)") and the rosters are
        // read on the helper thread, which says when they are in. Without one they are read here, in the call.
        let background = if wanted.is_empty() {
            None
        } else {
            self.background.get().and_then(Weak::upgrade)
        };
        if background.is_none() {
            lookup_failure = self.fetch_rosters(&live, &wanted).1;
        }
        {
            let known = lock(&self.members);
            for (c, ids) in &mut parsed {
                if ids.is_empty() {
                    *ids = known.get(&c.id).cloned().unwrap_or_default();
                }
            }
        }
        let all: Vec<String> = parsed
            .iter()
            .flat_map(|(_, ids)| ids.iter().cloned())
            .collect();
        // The helper thread reads the rosters and then the names of everyone the list holds, so one pass titles what
        // it can; if one is already running the names (and these rosters) wait for the next read.
        let started = background
            .as_ref()
            .is_some_and(|me| Self::start_roster_job(me, wanted, all.clone()));
        if !started && let Err(e) = self.look_up_names(&live, &all, true) {
            lookup_failure = lookup_failure.or(Some(e));
        }
        if let Some(e) = lookup_failure {
            self.auth.warn(e);
        }

        let mut chats: Vec<Chat> = parsed
            .into_iter()
            .map(|(mut c, ids)| {
                c.members = ids.into_iter().map(|id| self.member(id)).collect();
                c
            })
            .collect();
        if self.read_favourites {
            self.add_favourites(&live, &mut chats);
        }
        chats.sort_by_key(|c| std::cmp::Reverse(c.updated));
        // Favourites are listed whatever their age; the cap is on the others.
        let mut others = 0;
        chats.retain(|c| {
            c.favorite.is_some() || {
                others += 1;
                others <= self.max_chats
            }
        });
        Ok(chats)
    }

    fn find_people(&self, query: &str) -> Result<Vec<Contact>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        // The Substrate token only ever goes to Substrate (or, in tests, a local stand-in).
        if !(self.people_url.starts_with("https://substrate.office.com/")
            || trusted_url(&self.people_url))
        {
            return Err(Error::UntrustedUrl {
                url: self.people_url.clone(),
            });
        }
        if !self.people_extras_refused.load(Ordering::Relaxed) {
            match self.search_people(q, true) {
                // Phone and office are asked for on top of what has always worked; a service that will not have them
                // is asked again at once the old way, and not asked for them again this session.
                Err(e) if e.status() == Some(400) => {
                    log::warn!(
                        "people search: the service refused the phone and office fields ({}); searching without them",
                        e.for_log()
                    );
                    self.people_extras_refused.store(true, Ordering::Relaxed);
                }
                other => return other,
            }
        }
        self.search_people(q, false)
    }

    fn can_list_teams(&self) -> bool {
        true
    }

    fn teams(&self) -> Result<Vec<Team>> {
        self.read_teams()
    }

    fn channel_activity(&self) -> Vec<ChannelActivity> {
        self.channel_activity_now()
    }

    fn can_search_messages(&self) -> bool {
        true
    }

    fn search_messages(&self, chat_id: &str, query: &str, from: usize) -> Result<SearchPage> {
        self.search_chat(chat_id, query, from)
    }

    fn can_search_conversations(&self) -> bool {
        true
    }

    fn search_conversations(
        &self,
        conversation_ids: &[String],
        query: &str,
        from: usize,
    ) -> Result<SearchPage> {
        self.search_scope(conversation_ids, query, from)
    }

    fn open_chat(&self, who: &Contact) -> Result<Chat> {
        if !is_guid(&who.id) {
            return Err(Error::InvalidInput {
                what: "contact id",
                value: who.id.clone(),
            });
        }
        let (me, my_name) = self.auth.me();
        // Searching for yourself finds the conversation Teams keeps for that, not a chat with two copies of you.
        if who.id.eq_ignore_ascii_case(&me) {
            return Ok(Chat {
                web_url: Some(format!(
                    "https://teams.microsoft.com/l/chat/{}/0",
                    urlencode(parse::NOTES_ID)
                )),
                id: parse::NOTES_ID.to_string(),
                kind: ChatKind::OneOnOne,
                topic: Some(parse::NOTES_TITLE.to_string()),
                members: Vec::new(),
                updated: Utc::now(),
                preview: None,
                last_read: None,
                favorite: None,
            });
        }
        let id = one_on_one_id(&me, &who.id);
        lock(&self.fresh).insert(id.clone());
        Ok(Chat {
            web_url: Some(format!(
                "https://teams.microsoft.com/l/chat/{}/0",
                urlencode(&id)
            )),
            id,
            kind: ChatKind::OneOnOne,
            topic: None,
            members: vec![
                Member {
                    not_in_directory: false,
                    id: me.to_ascii_lowercase(),
                    name: clean(&my_name),
                },
                Member {
                    not_in_directory: false,
                    id: who.id.clone(),
                    name: clean(&who.name),
                },
            ],
            updated: Utc::now(),
            preview: None,
            last_read: None,
            favorite: None,
        })
    }

    fn messages(&self, chat_id: &str, older: Option<&str>) -> Result<Page> {
        const OPERATION: &str = "read messages";
        let mut refused = false;
        if older.is_none() && self.incremental_on() {
            match self.messages_since(chat_id) {
                Ok(Some(mut page)) => {
                    self.increments_work();
                    self.resolve_reaction_ids(&mut page.messages);
                    self.fill_reactor_names(chat_id, &mut page.messages);
                    return Ok(page);
                }
                Ok(None) => {}
                Err(e) if Self::refusal(&e) => {
                    log::warn!(
                        "messages {}: the incremental read was refused ({}); reading in full",
                        id_tag(chat_id),
                        e.for_log()
                    );
                    refused = true;
                }
                Err(e) => return Err(e),
            }
        }
        let url = match older {
            Some(cursor) if trusted_url(cursor) => cursor.to_string(),
            Some(cursor) => {
                return Err(Error::UntrustedUrl {
                    url: cursor.to_string(),
                });
            }
            None => {
                let live = self.live(None)?;
                format!(
                    "{}/v1/users/ME/conversations/{}/messages?pageSize=50&view=msnp24Equivalent",
                    live.chat_base,
                    urlencode(chat_id)
                )
            }
        };
        let v = match self.request(OPERATION, "GET", &url, None) {
            Ok(v) => v,
            // A chat we just opened has no conversation yet; that is an empty chat, not a failure.
            Err(Error::Status {
                status: 403 | 404, ..
            }) if older.is_none() && self.is_fresh(chat_id) => return Ok(Page::default()),
            Err(e) => return Err(e),
        };
        let items = v["messages"].as_array().map_or(&[][..], Vec::as_slice);
        self.watch_messages(&v, items);
        self.watch_repeats(chat_id, items);
        for m in items {
            self.learn(m);
        }
        let shown = self.without_resends(chat_id, items);
        let mut messages = self.collect(OPERATION, &shown, parse_msg);
        messages.sort_by_key(|m| m.created);
        let backward = v["_metadata"]["backwardLink"]
            .as_str()
            .filter(|l| trusted_url(l))
            .map(String::from);
        log::debug!(
            "messages {}: {} read in full, {} shown, {}",
            id_tag(chat_id),
            if older.is_some() {
                "an older page"
            } else {
                "a page"
            },
            messages.len(),
            if backward.is_some() {
                "with an older page behind it"
            } else {
                "nothing older"
            }
        );
        if older.is_none() {
            if refused && self.refusal_ends_increments(OPERATION) {
                self.give_up_on_increments(OPERATION);
            } else if self.incremental_on()
                && let Some(newest) = items.iter().filter_map(incremental::stamp).max()
            {
                incremental::remember(
                    &mut lock(&self.threads),
                    chat_id,
                    incremental::Thread::new(messages.clone(), backward.clone(), newest),
                );
            }
        }
        // After the remembered copy is made: what is remembered is what the service said, not what it was made into.
        self.resolve_reaction_ids(&mut messages);
        self.fill_reactor_names(chat_id, &mut messages);
        Ok(Page {
            messages,
            older: backward,
        })
    }

    fn send(
        &self,
        chat_id: &str,
        html: &str,
        mentions: &[OutMention],
        reply_to: Option<&ReplyTarget>,
    ) -> Result<Msg> {
        let live = self.live(None).map_err(Error::not_sent)?;
        let (me, name) = self.auth.me();
        let (mentioned, mention_json) = teams_mentions(html, mentions);
        let content = match reply_to {
            Some(target) => format!("{}{mentioned}", reply_quote_html(target)),
            None => mentioned,
        };
        let mut body = message_body(chat_id, &me, &name, &content);
        // `properties.qtdMsgs` — a second, structured reference to the quoted message alongside the blockquote's
        // own, in the exact shape the real client's own capture had — is the part most likely doing the actual
        // checking of a reply (see `message_properties`).
        let mut properties =
            message_properties(&mention_json.unwrap_or_else(|| "[]".to_string()), "");
        if let Some(target) = reply_to {
            properties["qtdMsgs"] = json!(qtd_msgs_json(target));
        }
        body["properties"] = properties;
        let url = format!(
            "{}/v1/users/ME/conversations/{}/messages",
            live.chat_base,
            urlencode(chat_id)
        );
        let mut sent = self.send_message(&url, &body);
        if self.is_fresh(chat_id)
            && matches!(
                &sent,
                Err(Error::Status {
                    status: 403 | 404,
                    ..
                })
            )
        {
            // The first attempt was refused, so the message has not been sent; if creating the conversation fails in a
            // way that says nothing about delivery, that is still true of the message.
            self.create_one_on_one(&live, chat_id)
                .map_err(Error::not_sent)?;
            sent = self.send_message(&url, &body);
        }
        let v = sent?;
        lock(&self.fresh).remove(chat_id);
        let arrived = v["OriginalArrivalTime"]
            .as_i64()
            .unwrap_or_else(|| Utc::now().timestamp_millis());
        let created = Utc
            .timestamp_millis_opt(arrived)
            .single()
            .unwrap_or_else(Utc::now);
        Ok(Msg {
            id: arrived.to_string(),
            created,
            modified: created,
            edited: None,
            from_id: Some(me),
            from_name: clean(&name),
            system: false,
            deleted: false,
            html: true,
            body: content,
            subject: None,
            attachments: Vec::new(),
            reactions: Vec::new(),
            mentions: mentions.iter().map(|m| m.user_id.clone()).collect(),
            delivery: Delivery::Sent,
            root_id: None,
        })
    }

    fn can_favorite(&self) -> bool {
        // Optimistic until a read has told us otherwise: most accounts can, and finding out needs a request this
        // synchronous method cannot make.
        *lock(&self.favourite_source) != Some(FavouriteSource::Folder)
    }

    fn set_favorite(&self, chat_id: &str, on: bool) -> Result<()> {
        const OPERATION: &str = "change favourites";
        if chat_id.is_empty() || chat_id.len() > 256 || chat_id.chars().any(char::is_control) {
            return Err(Error::InvalidInput {
                what: "chat id",
                value: chat_id.chars().take(40).collect(),
            });
        }
        // A folder-sourced account is known to refuse this from the last read; fail before any request rather than
        // reading the folder again only to say so.
        if *lock(&self.favourite_source) == Some(FavouriteSource::Folder) {
            return Err(folder_write_unsupported(OPERATION));
        }
        // Held across the whole read-modify-write: a second call for a different chat, running at the same time,
        // must not compute its change from a snapshot that is missing this one's.
        let _write = lock(&self.favourite_write);
        let live = self.live(None)?;
        // Read the whole list, change it, put it back: the property is one value, and a list that cannot be read is
        // never replaced by one that lacks what could not be read.
        let (mut favourites, source) = self.favourites(&live)?;
        if source == FavouriteSource::Folder {
            // The current client keeps them in a folder; changing the old property would not show in Teams.
            return Err(folder_write_unsupported(OPERATION));
        }
        if on {
            if favourites.contains_key(chat_id) {
                return Ok(());
            }
            let next = favourites
                .values()
                .filter_map(Value::as_u64)
                .max()
                .unwrap_or(0)
                + 1;
            favourites.insert(chat_id.to_string(), json!(next));
        } else if favourites.remove(chat_id).is_none() {
            return Ok(());
        }
        let body = json!({ "favorites": Value::Object(favourites).to_string() });
        self.request(OPERATION, "PUT", &Self::favourites_url(&live), Some(&body))
            .map(|_| ())
    }

    fn can_edit(&self) -> bool {
        true
    }

    fn edit(
        &self,
        chat_id: &str,
        message_id: &str,
        html: &str,
        mentions: &[OutMention],
    ) -> Result<()> {
        const OPERATION: &str = "edit message";
        // Message ids are the time the service stored them, in milliseconds.
        if message_id.is_empty() || !message_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::InvalidInput {
                what: "message id",
                value: message_id.to_string(),
            });
        }
        let (content, mention_json) = teams_mentions(html, mentions);
        let properties = mention_json.map(|m| json!({ "mentions": m }));
        self.put_edit(OPERATION, chat_id, message_id, &content, properties)
    }

    fn react(&self, chat_id: &str, message_id: &str, emoji: &str, on: bool) -> Result<()> {
        const OPERATION: &str = "react to message";
        if !crate::emoji::is_emoji_text(emoji) {
            return Err(Error::InvalidInput {
                what: "reaction emoji",
                value: emoji.to_string(),
            });
        }
        // Message ids are the time the service stored them, in milliseconds.
        if message_id.is_empty() || !message_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::InvalidInput {
                what: "message id",
                value: message_id.to_string(),
            });
        }
        // The classic six are known by their words. Anything else is filed under the id Teams' own catalogue gives it,
        // and an emoji it does not list is refused: an id Teams does not know would be stored and then shown to
        // everyone in the chat as a broken image.
        let key = match crate::emoji::classic_key(emoji) {
            Some(classic) => classic.to_string(),
            None => self
                .catalog()?
                .key_for(emoji)
                .map(str::to_string)
                .ok_or_else(|| Error::InvalidInput {
                    what: "emoji Teams has a reaction for",
                    value: emoji.to_string(),
                })?,
        };
        let live = self.live(None)?;
        let url = format!(
            "{}/v1/users/ME/conversations/{}/messages/{message_id}/properties?name=emotions",
            live.chat_base,
            urlencode(chat_id)
        );
        let (method, body) = if on {
            (
                "PUT",
                json!({ "emotions": { "key": key, "value": Utc::now().timestamp_millis() } }),
            )
        } else {
            ("DELETE", json!({ "emotions": { "key": key } }))
        };
        log::debug!(
            "{OPERATION}: {} {key}",
            if on { "adding" } else { "removing" }
        );
        self.request(OPERATION, method, &url, Some(&body))
            .map(|_| ())
    }

    fn reaction_emoji(&self) -> Result<Vec<crate::model::EmojiInfo>> {
        Ok(self.catalog()?.infos())
    }

    fn can_sign_in_again(&self) -> bool {
        true
    }

    fn sign_in_again(&self) -> Result<()> {
        self.auth.reauthenticate()?;
        // What was cached was made with the old sign-in.
        *lock(&self.live) = None;
        Ok(())
    }

    fn can_presence(&self) -> bool {
        true
    }

    fn presence(&self, ids: &[String]) -> Result<HashMap<String, Presence>> {
        const OPERATION: &str = "read presence";
        let mut seen = HashSet::new();
        // Only object ids go into a request (it is built from them), each once.
        let asked: Vec<&String> = ids
            .iter()
            .filter(|id| is_guid(id) && seen.insert(id.as_str()))
            .collect();
        let mut out = HashMap::new();
        for batch in asked.chunks(PRESENCE_BATCH) {
            let body = Value::Array(
                batch
                    .iter()
                    .map(|id| json!({ "mri": format!("8:orgid:{id}") }))
                    .collect(),
            );
            let answer =
                self.presence_request(OPERATION, "POST", "/v1/presence/getpresence/", Some(&body))?;
            self.watch.fields("presence answer", [&answer]);
            if let Some(items) = answer.as_array() {
                self.watch.fields("presence", items);
                // The parts the notes sit in: field names and types only, never a note's words.
                let inner: Vec<&Value> = items.iter().map(|i| &i["presence"]).collect();
                self.watch.fields("presence details", inner.iter().copied());
                self.watch
                    .fields("presence note", inner.iter().map(|p| &p["note"]));
                self.watch.fields(
                    "presence calendar",
                    inner.iter().map(|p| &p["calendarData"]),
                );
                self.watch.fields(
                    "presence out-of-office note",
                    inner.iter().map(|p| &p["calendarData"]["outOfOfficeNote"]),
                );
            }
            out.extend(parse::presence(&answer, chrono::Utc::now()));
        }
        log::debug!(
            "presence: asked about {}, {} answered",
            asked.len(),
            out.len()
        );
        Ok(out)
    }

    fn can_set_status(&self) -> bool {
        true
    }

    fn set_status(&self, status: Option<Availability>) -> Result<()> {
        const OPERATION: &str = "set status";
        match status {
            Some(a) if !a.is_choosable() => Err(Error::InvalidInput {
                what: "availability a person can choose",
                value: a.label().to_string(),
            }),
            Some(a) => self
                .presence_request(
                    OPERATION,
                    "PUT",
                    "/v1/me/forceavailability/",
                    Some(&json!({ "availability": a.label() })),
                )
                .map(|_| ()),
            // Giving the choice up is believed to be taking the override away; the real client has not been seen doing it.
            None => self
                .presence_request(OPERATION, "DELETE", "/v1/me/forceavailability/", None)
                .map(|_| ()),
        }
    }

    fn report_activity(&self, active: bool) -> Result<()> {
        const OPERATION: &str = "report activity";
        let report = || {
            self.presence_request(
                OPERATION,
                "POST",
                "/v1/me/reportmyactivity/",
                Some(&json!({ "endpointId": self.endpoint, "isActive": active })),
            )
        };
        if self.endpoint_registered.load(Ordering::Relaxed) {
            match report() {
                Ok(_) => return Ok(()),
                // The service has forgotten this program as a place you are online: register it again.
                Err(Error::Status { status: 404, .. }) => {
                    self.endpoint_registered.store(false, Ordering::Relaxed);
                }
                Err(e) => return Err(e),
            }
        }
        self.presence_request(
            OPERATION,
            "PUT",
            "/v1/me/endpoints/",
            Some(&json!({
                "id": self.endpoint,
                "availability": "Available",
                "activity": "Available",
                "activityReporting": "Transport",
                "deviceType": "Desktop",
            })),
        )?;
        self.endpoint_registered.store(true, Ordering::Relaxed);
        report().map(|_| ())
    }

    fn marks_read(&self) -> bool {
        true
    }

    fn code_block_style(&self) -> crate::model::CodeBlockStyle {
        crate::model::CodeBlockStyle::EditorCard
    }

    fn mark_read(&self, chat_id: &str, message_id: &str, arrived: DateTime<Utc>) -> Result<()> {
        const OPERATION: &str = "mark chat read";
        // Message ids are the time the service stored them, in milliseconds.
        if message_id.is_empty() || !message_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::InvalidInput {
                what: "message id",
                value: message_id.to_string(),
            });
        }
        let live = self.live(None)?;
        let url = format!(
            "{}/v1/users/ME/conversations/{}/properties?name=consumptionhorizon",
            live.chat_base,
            urlencode(chat_id)
        );
        // The read marker the service sends back is `<read up to, ms>;<…>;<message id>`; this writes the same shape.
        let ms = arrived.timestamp_millis();
        let body = json!({ "consumptionhorizon": format!("{ms};{ms};{message_id}") });
        self.request(OPERATION, "PUT", &url, Some(&body))
            .map(|_| ())
    }

    fn serves_image(&self, url: &str) -> bool {
        is_media_url(url) || shares::is_sharepoint_url(url)
    }

    fn image(&self, url: &str) -> Result<RgbImage> {
        self.fetch_image_impl(url, false)
    }

    fn image_from_outside(&self, url: &str) -> Result<RgbImage> {
        self.fetch_image_impl(url, true)
    }

    fn take_warnings(&self) -> Vec<Error> {
        let mut warnings = self.auth.take_warnings();
        warnings.extend(
            self.recorder
                .as_ref()
                .and_then(|r| r.take_failure())
                .map(Error::from),
        );
        warnings
    }

    fn poll_pushes(&self) -> Vec<PushHint> {
        std::mem::take(&mut *lock(&self.push_queue))
    }

    fn can_post_to_channels(&self) -> bool {
        true
    }

    fn post_to_channel(&self, channel: &str, post: &ChannelPost<'_>) -> Result<Msg> {
        self.post_channel(channel, post)
    }

    fn can_edit_posts(&self) -> bool {
        true
    }

    fn edit_post(
        &self,
        channel: &str,
        message_id: &str,
        subject: Option<&str>,
        html: &str,
        mentions: &[OutMention],
    ) -> Result<()> {
        self.edit_channel_post(channel, message_id, subject, html, mentions)
    }
}

/// The body of a message a client sends, without its `properties` (see [`message_properties`]): the shape the Teams web
/// client writes, for a chat or a channel alike.
fn message_body(conversation_id: &str, me: &str, name: &str, content: &str) -> Value {
    let stamp = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    json!({
        "id": "-1",
        "type": "Message",
        "conversationid": conversation_id,
        "conversationLink": format!("blah/{conversation_id}"),
        "from": format!("8:orgid:{me}"),
        "fromUserId": format!("8:orgid:{me}"),
        "composetime": stamp,
        "originalarrivaltime": stamp,
        "content": content,
        "messagetype": "RichText/Html",
        "contenttype": "Text",
        "imdisplayname": name,
        "clientmessageid": Utc::now().timestamp_nanos_opt().unwrap_or_default().unsigned_abs().to_string(),
        "callId": "",
        "state": 0,
        "version": "0",
        "amsreferences": [],
        "crossPostChannels": [],
    })
}

/// The Teams client always writes this `properties` shape, empty fields and all, on every message it sends (confirmed
/// live, 2026-11-06, a captured DevTools request, `docs/teams-web-protocol.md`'s "Reply-quote verification"); a message
/// missing it is shown in the real client with a "this quoted message isn't verified by Teams" notice on its reply
/// quote, absent from one with it present. `mentions` is the JSON text of the mention list and `subject` the title of a
/// channel post (empty for everything else).
fn message_properties(mentions: &str, subject: &str) -> Value {
    json!({
        "mentions": mentions,
        "formatVariant": "TEAMS",
        "cards": "[]",
        "files": "[]",
        "links": "[]",
        "importance": "",
        "subject": subject,
        "title": "",
        "onbehalfof": null,
        "policyViolation": null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryStore;
    use crate::testserver::{Reply, Server, serve};

    pub(super) const ME: &str = "11111111-1111-1111-1111-111111111111";
    pub(super) const ADA: &str = "22222222-2222-2222-2222-222222222222";

    /// A backend whose credentials are already fresh and whose chat service is `server`.
    pub(super) fn backend(server: &Server) -> TeamsBackend {
        auth::trust_for_tests(server.origin());
        shares::allow_download_origin_for_tests(server.origin());
        // Most tests script the replies to one exact set of requests; the favourites have tests of their own.
        let b = TeamsBackend::new(
            TeamsSession::for_test(ME, "Me"),
            Arc::new(MemoryStore::new()),
            BrowserOptions::new("unused-profile"),
        )
        .with_favourites(false);
        *lock(&b.live) = Some(Live {
            spaces_token: "spaces-token".into(),
            spaces_exp: now() + 3600,
            skype_token: "skype-token".into(),
            skype_exp: now() + 86_400,
            chat_base: format!("{}/api/chatsvc/amer", server.origin()),
            mt_base: String::new(),
            csa_base: String::new(),
        });
        // `send_message` wants its own, separately-scoped token; seeded here, like `live` above, so sending in a
        // test never needs a real OAuth round trip.
        *lock(&b.ic3) = Some(("ic3-token".into(), now() + 3600));
        b
    }

    #[test]
    fn a_forced_renewal_does_nothing_once_another_caller_has_already_renewed_the_stale_token() {
        let server = serve(vec![]);
        let b = backend(&server);
        let stale = lock(&b.live).as_ref().unwrap().spaces_token.clone();
        let renewed = {
            let mut live = lock(&b.live).as_ref().unwrap().clone();
            live.spaces_token = "renewed-while-waiting-for-the-lock".into();
            live
        };
        *lock(&b.live) = Some(renewed.clone());
        let got = b
            .live(Some(&stale))
            .expect("the cache has already moved past the stale token");
        assert_eq!(
            got.spaces_token, renewed.spaces_token,
            "the token another caller already renewed is returned as is"
        );
        assert!(
            server.seen().is_empty(),
            "no request was sent: nothing needed renewing"
        );
    }

    /// A backend like [`backend`] whose renewals really go to `server` (the token endpoint and the service's
    /// authorisation), with the cached credentials running out `spaces_in` and `skype_in` seconds from now.
    fn renewing_backend(server: &Server, spaces_in: i64, skype_in: i64) -> TeamsBackend {
        let mut b = backend(server);
        b.auth = TeamsAuth::new(
            TeamsSession::for_test(ME, "Me"),
            Arc::new(MemoryStore::new()),
            BrowserOptions::new("unused-profile"),
        )
        .with_endpoints(server.origin(), &server.url("/v1.0/authz"));
        let mut cached = lock(&b.live);
        let live = cached.as_mut().unwrap();
        live.spaces_exp = now() + spaces_in;
        live.skype_exp = now() + skype_in;
        drop(cached);
        b
    }

    fn token_reply(token: &str) -> Reply {
        Reply::status(200).json(&json!({"access_token": token, "expires_in": 3600}).to_string())
    }

    fn authz_reply(skype_token: &str) -> Reply {
        Reply::status(200).json(
            &json!({"tokens": {"skypeToken": skype_token, "expiresIn": 3600},
                    "regionGtms": {"chatServiceAfd": "{origin}/api/chatsvc/amer"}})
            .to_string(),
        )
    }

    fn authz_requests(server: &Server) -> usize {
        server
            .seen()
            .iter()
            .filter(|s| s.path.ends_with("/v1.0/authz"))
            .count()
    }

    /// Wait until the server has been asked to authorise, which is when a renewal is in flight (the answer is
    /// delayed): nothing here depends on how fast threads start.
    fn wait_for_authz(server: &Server) {
        let started = Instant::now();
        while authz_requests(server) == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the renewal never reached the server"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// A burst of callers that all need new credentials (none cached that will do) renews once, not once each, and
    /// every one of them gets what that renewal brought. Callers after it use the result without renewing again.
    #[test]
    fn a_burst_of_callers_needing_a_renewal_renews_once_and_all_get_the_new_credentials() {
        let server = serve(vec![token_reply("renewed"), authz_reply("skype-renewed")]);
        // The cached credentials ran out a moment ago.
        let b = renewing_backend(&server, -5, -5);
        let got: Vec<Result<Live>> = thread::scope(|s| {
            let callers: Vec<_> = (0..8).map(|_| s.spawn(|| b.live(None))).collect();
            callers.into_iter().map(|c| c.join().unwrap()).collect()
        });
        for live in got {
            let live = live.expect("every caller is served");
            assert_eq!(live.spaces_token, "renewed");
            assert_eq!(live.skype_token, "skype-renewed");
        }
        assert_eq!(
            server.seen().len(),
            2,
            "one token request, one authorisation"
        );
        // No renewal storm afterwards: the new credentials are good for an hour.
        for _ in 0..10 {
            assert_eq!(b.live(None).unwrap().spaces_token, "renewed");
        }
        assert_eq!(
            server.seen().len(),
            2,
            "later callers use the renewed credentials"
        );
    }

    /// While the credentials are being renewed ahead of their expiry, callers whose cached ones are still valid are not
    /// held up by the renewal (they use the old ones); a caller that has just had a token refused, which must not be
    /// given that token again, waits and gets the new one.
    #[test]
    fn callers_with_credentials_still_valid_do_not_wait_for_a_renewal_in_flight() {
        let server = serve(vec![
            token_reply("renewed"),
            authz_reply("skype-renewed").delayed(Duration::from_millis(300)),
        ]);
        // Within the two minutes before the access token's expiry, when a renewal begins, but not yet expired.
        let b = renewing_backend(&server, 60, 3600);
        thread::scope(|s| {
            let renewer = s.spawn(|| b.live(None));
            wait_for_authz(&server);
            let retrying = s.spawn(|| b.live(Some("spaces-token")));
            let started = Instant::now();
            for _ in 0..20 {
                let live = b
                    .live(None)
                    .expect("the cached credentials are still valid");
                assert_eq!(live.spaces_token, "spaces-token", "the old credentials");
            }
            let waited = started.elapsed();
            assert!(
                waited < Duration::from_millis(100),
                "twenty calls took {waited:?} with a renewal in flight"
            );
            assert!(
                !renewer.is_finished(),
                "the renewal was still in flight while those calls were served"
            );
            assert_eq!(
                retrying.join().unwrap().unwrap().spaces_token,
                "renewed",
                "the caller whose token was refused is not given it again"
            );
            assert_eq!(renewer.join().unwrap().unwrap().spaces_token, "renewed");
        });
        assert_eq!(server.seen().len(), 2, "the whole burst renewed once");
        assert_eq!(b.live(None).unwrap().spaces_token, "renewed");
    }

    /// A caller whose cached credentials have really run out cannot use them, and waits for the renewal under way.
    #[test]
    fn callers_whose_credentials_have_run_out_wait_for_the_renewal_in_flight() {
        let server = serve(vec![
            token_reply("renewed"),
            authz_reply("skype-renewed").delayed(Duration::from_millis(300)),
        ]);
        let b = renewing_backend(&server, -5, 3600);
        thread::scope(|s| {
            let renewer = s.spawn(|| b.live(None));
            wait_for_authz(&server);
            let started = Instant::now();
            let waiting = b.live(None).expect("served by the renewal");
            assert_eq!(waiting.spaces_token, "renewed");
            assert!(
                started.elapsed() >= Duration::from_millis(100),
                "it waited for the renewal ({:?}) instead of using what had run out",
                started.elapsed()
            );
            assert_eq!(renewer.join().unwrap().unwrap().spaces_token, "renewed");
        });
        assert_eq!(server.seen().len(), 2, "the renewal was shared");
    }

    /// Once the sign-in is known to be lost, no caller is served from the cache, whether its credentials are
    /// comfortably valid or close to running out; each is told so, with no request. Repeated, because the case that
    /// matters is a caller already queued at the gate when the first one's renewal fails, which depends on timing.
    #[test]
    fn once_the_sign_in_is_lost_no_caller_is_served_from_the_cache() {
        for spaces_in in std::iter::repeat_n([3600, 60], 100).flatten() {
            let server = serve(vec![]);
            let b = renewing_backend(&server, spaces_in, 86_400);
            b.auth.lose_sign_in_for_test();
            thread::scope(|s| {
                let callers: Vec<_> = (0..4).map(|_| s.spawn(|| b.live(None))).collect();
                for c in callers {
                    let err = c.join().unwrap().unwrap_err();
                    assert!(err.needs_sign_in(), "{spaces_in}: {err:?}");
                }
            });
            assert!(
                server.seen().is_empty(),
                "{spaces_in}: told with no request"
            );
        }
    }

    /// A renewal that fails, or panics, lets go of the gate: the next caller renews.
    #[test]
    fn a_renewal_that_fails_or_panics_does_not_stop_the_next_caller_renewing() {
        // A failure: the authorisation is refused.
        let server = serve(vec![
            token_reply("refused"),
            Reply::status(500).body("down"),
            token_reply("renewed"),
            authz_reply("skype-renewed"),
        ]);
        let b = renewing_backend(&server, -5, -5);
        assert!(b.live(None).is_err());
        assert_eq!(b.live(None).unwrap().spaces_token, "renewed");

        // A panic, here in the stand-in for the browser that runs when the grant is refused.
        let server = serve(vec![
            Reply::status(400).json(r#"{"error":"invalid_grant","error_description":"expired"}"#),
            token_reply("renewed"),
            authz_reply("skype-renewed"),
        ]);
        let b = renewing_backend(&server, -5, -5);
        b.auth
            .set_recover_hook_for_test(|_| panic!("the renewal fell over"));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| b.live(None)));
        assert!(outcome.is_err(), "the panic came out of the renewal");
        assert_eq!(
            b.live(None).unwrap().spaces_token,
            "renewed",
            "the next caller renews instead of waiting for ever"
        );
    }

    #[test]
    fn callers_waiting_on_a_renewal_that_fails_share_its_failure() {
        let mut replies = vec![
            token_reply("t1"),
            Reply::status(500)
                .delayed(Duration::from_millis(300))
                .body("down"),
        ];
        for i in 2..=4 {
            replies.push(token_reply(&format!("t{i}")));
            replies.push(Reply::status(500).body("down"));
        }
        let server = serve(replies);
        let b = renewing_backend(&server, -5, -5);
        let results: Vec<Result<Live>> = thread::scope(|s| {
            let first = s.spawn(|| b.live(None));
            wait_for_authz(&server);
            let waiters: Vec<_> = (0..3).map(|_| s.spawn(|| b.live(None))).collect();
            let mut all = vec![first.join().unwrap()];
            all.extend(waiters.into_iter().map(|w| w.join().unwrap()));
            all
        });
        assert!(
            results.iter().all(Result::is_err),
            "every caller sees the outage"
        );
        assert_eq!(
            authz_requests(&server),
            1,
            "one failed renewal, shared by the waiters"
        );
    }

    fn contact(id: &str) -> Contact {
        Contact {
            id: id.into(),
            name: "Ada".into(),
            email: String::new(),
            detail: String::new(),
            ..Default::default()
        }
    }

    const CHAT: &str = "19:x@thread.v2";

    /// A message as the service sends it: `version` is when it last changed, the clock time follows the id.
    fn wire_msg(id: u64, version: u64, body: &str) -> Value {
        json!({"id": id.to_string(), "version": version.to_string(), "messagetype": "Text", "content": body,
               "originalarrivaltime": format!("2026-10-27T10:00:{:02}Z", id % 60)})
    }

    fn page(messages: &[Value]) -> String {
        json!({"messages": messages, "_metadata": {"backwardLink": "{origin}/api/chatsvc/amer/older"}})
            .to_string()
    }

    fn bodies(p: &Page) -> Vec<&str> {
        p.messages.iter().map(|m| m.body.as_str()).collect()
    }

    #[test]
    fn later_reads_of_a_chat_ask_only_for_what_changed_and_return_the_whole_page() {
        let server = serve(vec![
            Reply::status(200).json(&page(&[
                wire_msg(1, 100_000, "one"),
                wire_msg(2, 200_000, "two"),
            ])),
            Reply::status(200).json(&page(&[
                wire_msg(2, 200_000, "two"),
                wire_msg(1, 300_000, "one, edited"),
            ])),
        ]);
        let b = backend(&server);
        let first = b.messages(CHAT, None).unwrap();
        assert_eq!(bodies(&first), ["one", "two"]);
        let second = b.messages(CHAT, None).unwrap();
        assert_eq!(
            bodies(&second),
            ["one, edited", "two"],
            "a change to an older message arrives, the rest comes from memory"
        );
        assert_eq!(
            second.older, first.older,
            "the cursor for older pages is kept"
        );
        let seen = server.seen();
        assert!(!seen[0].path.contains("startTime"), "{}", seen[0].path);
        assert!(
            seen[1].path.contains("&startTime=195000"),
            "the newest version seen, less the overlap: {}",
            seen[1].path
        );
    }

    #[test]
    fn the_next_read_builds_on_the_newest_version_seen_so_far() {
        let server = serve(vec![
            Reply::status(200).json(&page(&[wire_msg(1, 100_000, "one")])),
            Reply::status(200).json(&page(&[wire_msg(2, 400_000, "two")])),
            Reply::status(200).json(&page(&[wire_msg(2, 400_000, "two")])),
        ]);
        let b = backend(&server);
        b.messages(CHAT, None).unwrap();
        assert_eq!(bodies(&b.messages(CHAT, None).unwrap()), ["one", "two"]);
        b.messages(CHAT, None).unwrap();
        assert!(
            server.seen()[2].path.contains("&startTime=395000"),
            "{}",
            server.seen()[2].path
        );
    }

    #[test]
    fn an_answer_that_may_have_been_cut_short_is_replaced_by_a_full_read() {
        let many: Vec<Value> = (1..=50).map(|i| wire_msg(i, 100_000 + i, "x")).collect();
        let server = serve(vec![
            Reply::status(200).json(&page(&[wire_msg(1, 100_000, "one")])),
            Reply::status(200).json(&page(&many)),
            Reply::status(200).json(&page(&[wire_msg(1, 100_000, "one, again")])),
        ]);
        let b = backend(&server);
        b.messages(CHAT, None).unwrap();
        let p = b.messages(CHAT, None).unwrap();
        assert_eq!(bodies(&p), ["one, again"]);
        assert!(!server.seen()[2].path.contains("startTime"));
    }

    #[test]
    fn a_failed_incremental_read_keeps_what_was_remembered() {
        let server = serve(vec![
            Reply::status(200).json(&page(&[wire_msg(1, 100_000, "one")])),
            Reply::status(500).json(r#"{"message":"boom"}"#),
            Reply::status(200).json(&page(&[wire_msg(1, 100_000, "one")])),
        ]);
        let b = backend(&server);
        b.messages(CHAT, None).unwrap();
        assert!(matches!(
            b.messages(CHAT, None),
            Err(Error::Status { status: 500, .. })
        ));
        b.messages(CHAT, None).unwrap();
        assert!(server.seen()[2].path.contains("startTime"));
        assert!(b.take_warnings().is_empty());
    }

    #[test]
    fn a_service_that_refuses_increments_is_read_in_full_from_then_on_with_a_warning() {
        let one = page(&[wire_msg(1, 100_000, "one")]);
        let refused = || Reply::status(400).json(r#"{"message":"bad startTime"}"#);
        let server = serve(vec![
            Reply::status(200).json(&one),
            refused(),
            Reply::status(200).json(&one),
            refused(),
            Reply::status(200).json(&one),
            refused(),
            Reply::status(200).json(&one),
            Reply::status(200).json(&one),
        ]);
        let b = backend(&server);
        b.messages(CHAT, None).unwrap();
        for _ in 0..3 {
            assert_eq!(bodies(&b.messages(CHAT, None).unwrap()), ["one"]);
        }
        assert!(
            b.take_warnings().len() == 1,
            "told once, when they are given up"
        );
        b.messages(CHAT, None).unwrap();
        let paths: Vec<bool> = server
            .seen()
            .iter()
            .map(|s| s.path.contains("startTime"))
            .collect();
        assert_eq!(paths, [false, true, false, true, false, true, false, false]);
        assert!(!b.incremental_on());
    }

    #[test]
    fn a_refusal_that_a_full_read_shares_is_not_blamed_on_increments() {
        let server = serve(vec![
            Reply::status(200).json(&page(&[wire_msg(1, 100_000, "one")])),
            Reply::status(404).json(r#"{"message":"gone"}"#),
            Reply::status(404).json(r#"{"message":"gone"}"#),
        ]);
        let b = backend(&server);
        b.messages(CHAT, None).unwrap();
        assert!(matches!(
            b.messages(CHAT, None),
            Err(Error::Status { status: 404, .. })
        ));
        assert!(b.incremental_on(), "the chat is gone; increments are fine");
    }

    #[test]
    fn remembered_reads_expire_and_can_be_turned_off() {
        let one = page(&[wire_msg(1, 100_000, "one")]);
        let server = serve(vec![
            Reply::status(200).json(&one),
            Reply::status(200).json(&one),
            Reply::status(200).json(&one),
            Reply::status(200).json(&one),
        ]);
        let mut b = backend(&server);
        b.full_refresh = Duration::ZERO;
        b.messages(CHAT, None).unwrap();
        b.messages(CHAT, None).unwrap();
        let b = backend(&server).with_incremental_reads(false);
        b.messages(CHAT, None).unwrap();
        b.messages(CHAT, None).unwrap();
        assert!(
            server.seen().iter().all(|s| !s.path.contains("startTime")),
            "{:?}",
            server.seen().iter().map(|s| &s.path).collect::<Vec<_>>()
        );
    }

    fn conversation(id: &str, at: &str) -> Value {
        json!({"id": format!("19:{id}@thread.v2"), "threadProperties": {"topic": id},
               "lastMessage": {"messagetype": "Text", "content": "x", "originalarrivaltime": at}})
    }

    fn list(conversations: &[Value], sync: Option<&str>) -> String {
        let mut v = json!({ "conversations": conversations });
        if let Some(link) = sync {
            v["_metadata"] = json!({ "syncState": link });
        }
        v.to_string()
    }

    fn titles(chats: &[Chat]) -> Vec<String> {
        chats.iter().map(|c| c.title(ME)).collect()
    }

    #[test]
    fn the_chat_list_follows_the_sync_links_and_merges_what_changed() {
        let server = serve(vec![
            Reply::status(200).json(&list(
                &[
                    conversation("a", "2026-10-27T10:00:00Z"),
                    conversation("b", "2026-10-27T11:00:00Z"),
                ],
                Some("{origin}/api/chatsvc/amer/sync/1"),
            )),
            Reply::status(200).json(&list(
                &[conversation("a", "2026-10-27T12:00:00Z")],
                Some("{origin}/api/chatsvc/amer/sync/2"),
            )),
            Reply::status(200).json(&list(&[], Some("{origin}/api/chatsvc/amer/sync/3"))),
        ]);
        let b = backend(&server);
        assert_eq!(titles(&b.chats().unwrap()), ["b", "a"]);
        assert_eq!(
            titles(&b.chats().unwrap()),
            ["a", "b"],
            "the changed chat moved up, the other came from memory"
        );
        assert_eq!(titles(&b.chats().unwrap()), ["a", "b"]);
        let paths: Vec<String> = server.seen().iter().map(|s| s.path.clone()).collect();
        assert!(
            paths[0].contains("/v1/users/ME/conversations?"),
            "{paths:?}"
        );
        assert_eq!(paths[1], "/api/chatsvc/amer/sync/1");
        assert_eq!(paths[2], "/api/chatsvc/amer/sync/2");
    }

    #[test]
    fn a_sync_link_to_another_host_is_never_followed() {
        let first = list(
            &[conversation("a", "2026-10-27T10:00:00Z")],
            Some("https://evil.example/sync"),
        );
        let server = serve(vec![
            Reply::status(200).json(&first),
            Reply::status(200).json(&first),
        ]);
        let b = backend(&server);
        b.chats().unwrap();
        b.chats().unwrap();
        assert!(
            server
                .seen()
                .iter()
                .all(|s| s.path.contains("/v1/users/ME/conversations?")),
            "both were full reads"
        );
    }

    #[test]
    fn a_full_read_keeps_following_the_chain_that_was_already_in_use() {
        let server = serve(vec![
            Reply::status(200).json(&list(
                &[conversation("a", "2026-10-27T10:00:00Z")],
                Some("{origin}/api/chatsvc/amer/sync/1"),
            )),
            Reply::status(200).json(&list(
                &[conversation("a", "2026-10-27T10:00:00Z")],
                Some("{origin}/api/chatsvc/amer/sync/9"),
            )),
        ]);
        let mut b = backend(&server);
        b.full_refresh = Duration::ZERO;
        b.chats().unwrap();
        b.chats().unwrap();
        let sync = lock(&b.chat_index).as_ref().and_then(|i| i.sync.clone());
        assert_eq!(
            sync,
            Some(server.url("/api/chatsvc/amer/sync/1")),
            "a fresh link would start the catch-up over"
        );
    }

    #[test]
    fn an_expired_sync_link_costs_one_full_read_and_a_fresh_chain_not_the_feature() {
        let first = list(
            &[conversation("a", "2026-10-27T10:00:00Z")],
            Some("{origin}/api/chatsvc/amer/sync/1"),
        );
        let second = list(
            &[conversation("a", "2026-10-27T10:00:00Z")],
            Some("{origin}/api/chatsvc/amer/sync/2"),
        );
        let server = serve(vec![
            Reply::status(200).json(&first),
            Reply::status(400).json(r#"{"message":"expired"}"#),
            Reply::status(200).json(&second),
            Reply::status(200).json(&list(&[], Some("{origin}/api/chatsvc/amer/sync/3"))),
        ]);
        let b = backend(&server);
        b.chats().unwrap();
        assert_eq!(
            titles(&b.chats().unwrap()),
            ["a"],
            "read in full after the refusal"
        );
        assert_eq!(
            titles(&b.chats().unwrap()),
            ["a"],
            "and the new chain is followed"
        );
        let followed: Vec<bool> = server
            .seen()
            .iter()
            .map(|s| s.path.contains("/sync/"))
            .collect();
        assert_eq!(followed, [false, true, false, true]);
        assert!(b.incremental_on());
        assert!(
            b.take_warnings().is_empty(),
            "an isolated refusal is only logged"
        );
    }

    #[test]
    fn a_service_that_keeps_refusing_the_change_feed_is_read_in_full_from_then_on_with_a_warning() {
        let first = list(
            &[conversation("a", "2026-10-27T10:00:00Z")],
            Some("{origin}/api/chatsvc/amer/sync/1"),
        );
        let refused = || Reply::status(400).json(r#"{"message":"no"}"#);
        let server = serve(vec![
            Reply::status(200).json(&first),
            refused(),
            Reply::status(200).json(&first),
            refused(),
            Reply::status(200).json(&first),
            refused(),
            Reply::status(200).json(&first),
            Reply::status(200).json(&first),
        ]);
        let b = backend(&server);
        b.chats().unwrap();
        for _ in 0..3 {
            assert_eq!(titles(&b.chats().unwrap()), ["a"]);
        }
        b.chats().unwrap();
        let followed: Vec<bool> = server
            .seen()
            .iter()
            .map(|s| s.path.contains("/sync/"))
            .collect();
        assert_eq!(
            followed,
            [false, true, false, true, false, true, false, false]
        );
        assert!(matches!(
            b.take_warnings().as_slice(),
            [Error::Unexpected {
                operation: "list chats",
                ..
            }]
        ));
    }

    #[test]
    fn an_incremental_read_that_works_resets_the_count_of_refusals() {
        let first = list(
            &[conversation("a", "2026-10-27T10:00:00Z")],
            Some("{origin}/api/chatsvc/amer/sync/1"),
        );
        let with_link = |n: u32| list(&[], Some(&format!("{{origin}}/api/chatsvc/amer/sync/{n}")));
        let refused = || Reply::status(400).json(r#"{"message":"expired"}"#);
        let mut replies = vec![Reply::status(200).json(&first)];
        // Two refusals, a read that works, two more refusals: never three in a row.
        replies.extend([refused(), Reply::status(200).json(&first)]);
        replies.extend([refused(), Reply::status(200).json(&first)]);
        replies.push(Reply::status(200).json(&with_link(2)));
        replies.extend([refused(), Reply::status(200).json(&first)]);
        replies.extend([refused(), Reply::status(200).json(&first)]);
        let server = serve(replies);
        let b = backend(&server);
        for _ in 0..6 {
            b.chats().unwrap();
        }
        assert!(b.incremental_on());
    }
    #[test]
    fn entries_without_an_id_are_still_reported_when_the_list_is_read_incrementally() {
        let server = serve(vec![
            Reply::status(200).json(&list(
                &[conversation("a", "2026-10-27T10:00:00Z")],
                Some("{origin}/api/chatsvc/amer/sync/1"),
            )),
            Reply::status(200).json(&list(&[json!({"no_id": true})], None)),
        ]);
        let b = backend(&server);
        b.chats().unwrap();
        assert_eq!(titles(&b.chats().unwrap()), ["a"]);
        assert!(matches!(
            b.take_warnings().as_slice(),
            [Error::Unexpected {
                operation: "list chats",
                ..
            }]
        ));
    }

    /// Every record the tests in this process make, to check what reaches a log.
    static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    struct Capture;

    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            // The HTTP library below logs request and response bytes at trace level; applications must not let
            // that through (vtteams logs other crates at warnings only), so only this crate's records are kept.
            if record.target().starts_with("teams_client") {
                lock(&LOGGED).push(format!("{} {}", record.level(), record.args()));
            }
        }

        fn flush(&self) {}
    }

    /// Install the capturing logger (once) and take the lock the tests that read LOGGED hold for their whole body:
    /// each of them clears the buffer first, so two running at once would wipe each other's lines.
    fn capture_logs() -> std::sync::MutexGuard<'static, ()> {
        static ONCE: std::sync::Once = std::sync::Once::new();
        static SERIAL: Mutex<()> = Mutex::new(());
        ONCE.call_once(|| {
            let _ = log::set_logger(&Capture);
            log::set_max_level(log::LevelFilter::Trace);
        });
        lock(&SERIAL)
    }

    #[test]
    fn the_log_describes_what_happens_and_what_changed_without_any_content() {
        let _serial = capture_logs();
        // `LOGGED` is one static shared by every test in this binary (the `log` facade allows only one logger per
        // process); clearing it here only discards what earlier tests left behind; log lines from another test that
        // happens to run at the same moment as this one can still land in between, so this test only trusts that its
        // own markers (`TOPSECRET*`, "Zaphod", "Beeblebrox") are specific enough that nothing else in the suite would
        // produce them.
        lock(&LOGGED).clear();
        let secret_chat = "19:abcdef-TOPSECRETID@thread.v2";
        let conv = |extra: Value| {
            let mut c = json!({"id": secret_chat, "threadProperties": {"topic": "TOPSECRET-TOPIC"},
                "lastMessage": {"messagetype": "Text", "content": "TOPSECRET-PREVIEW", "imdisplayname": "Zaphod Beeblebrox",
                                "originalarrivaltime": "2026-10-27T10:00:00Z"}});
            c.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            c
        };
        let message = |version: u64, body: &str, extra: Value| {
            let mut m = wire_msg(1, version, body);
            m["imdisplayname"] = json!("Zaphod Beeblebrox");
            m["properties"] = json!({"mentions": "[{\"displayName\":\"TOPSECRET-MENTION\"}]"});
            m.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            m
        };
        let server = serve(vec![
            Reply::status(200).json(&list(&[conv(json!({}))], Some("{origin}/api/chatsvc/amer/sync/1"))),
            Reply::status(200).json(&list(
                &[
                    conv(json!({"brandNewFieldQ7": "TOPSECRET-VALUE"})),
                    json!({"id": "19:other@thread.v2", "lastMessage": {"unknownShape": "TOPSECRET-UNREADABLE"}}),
                    // A channel: it is not a chat, but what it says of its own activity is remembered.
                    json!({"id": "19:TOPSECRET-CHANNEL@thread.tacv2",
                           "threadProperties": {"threadType": "topic", "productThreadType": "TeamsStandardChannel",
                                                "spaceThreadTopic": "TOPSECRET-SPACE"},
                           "properties": {"consumptionhorizon": "1;1;TOPSECRET-MARK"},
                           "lastMessage": {"messagetype": "Text", "content": "TOPSECRET-CHANNEL-TEXT",
                                           "imdisplayname": "Zaphod Beeblebrox", "originalarrivaltime": "2026-10-27T10:00:00Z",
                                           "from": "https://x.test/8:orgid:TOPSECRET-SENDER"}}),
                ],
                Some("{origin}/api/chatsvc/amer/sync/2"),
            )),
            Reply::status(200).json(&page(&[message(100_000, "TOPSECRET-BODY", json!({}))])),
            Reply::status(200).json(&page(&[message(200_000, "TOPSECRET-EDITED", json!({"newMessageFieldQ7": 1}))])),
            Reply::status(500).json(r#"{"message":"boom"}"#),
            // A search whose every field holds something that must not be logged, and one result it cannot read.
            Reply::status(200).json(
                &json!({"EntitySets": [{"ResultSets": [{
                    "Total": 2, "MoreResultsAvailable": false,
                    "Results": [
                        {"Id": "TOPSECRET-HITID", "HitHighlightedSummary": "<c0>TOPSECRET-SUMMARY</c0>",
                         "Source": {"InternetMessageId": 1_793_000_000_001_u64, "DateTimeSent": "2026-11-10T09:12:33Z",
                                    "Preview": "TOPSECRET-PREVIEW-OF-HIT", "searchNewFieldQ7": "TOPSECRET-FIELD-VALUE",
                                    "ChannelName": "TOPSECRET-CHANNELNAME", "TeamName": "TOPSECRET-TEAMNAME",
                                    "From": {"EmailAddress": {"Name": "Zaphod Beeblebrox", "Address": "zaphod@TOPSECRET.test"}}}},
                        {"Id": "TOPSECRET-UNREADABLE", "Source": {"Preview": "TOPSECRET-NO-ID", "From": {"Name": "Zaphod"}}}
                    ]}]}]})
                .to_string(),
            ),
            Reply::status(200).json(r#"{"EntitySets": [{"IsPartial": true}], "echo": "TOPSECRET-PARTIAL"}"#),
            // A search over two conversations: one hit in each, one from outside the scope and one that names none, each
            // holding names that must not be logged.
            Reply::status(200).json(
                &json!({"EntitySets": [{"ResultSets": [{
                    "Total": 4, "MoreResultsAvailable": false,
                    "Results": [
                        {"Source": {"InternetMessageId": 1_793_000_000_001_u64, "DateTimeSent": "2026-11-10T09:12:33Z",
                                    "ClientThreadId": "19:abcdef-TOPSECRETID@thread.v2;messageid=1793000000000",
                                    "ChannelName": "TOPSECRET-CHANNEL-ONE", "Preview": "TOPSECRET-ONE",
                                    "From": {"Name": "Zaphod"}}},
                        {"Source": {"InternetMessageId": 1_793_000_000_002_u64, "DateTimeSent": "2026-11-10T09:12:34Z",
                                    "ClientThreadId": "19:TOPSECRET-SECOND@thread.tacv2",
                                    "ChannelName": "TOPSECRET-CHANNEL-TWO", "Preview": "TOPSECRET-TWO"}},
                        {"Source": {"InternetMessageId": 1_793_000_000_003_u64, "DateTimeSent": "2026-11-10T09:12:35Z",
                                    "ClientThreadId": "19:TOPSECRET-ELSEWHERE@thread.tacv2",
                                    "ChannelName": "TOPSECRET-CHANNEL-THREE"}},
                        {"Source": {"InternetMessageId": 1_793_000_000_004_u64, "DateTimeSent": "2026-11-10T09:12:36Z",
                                    "ChannelName": "TOPSECRET-CHANNEL-FOUR"}}
                    ]}]}]})
                .to_string(),
            ),
            // The teams: every field holds something that must not be logged, and one team cannot be read.
            Reply::status(200).json(&teams::tests::teams_answer_with_folders(&[
                teams::tests::team(
                    "19:TOPSECRET-TEAMID@thread.tacv2",
                    "TOPSECRET-TEAM Zaphod",
                    vec![
                        teams::tests::channel(
                            "19:TOPSECRET-CH@thread.tacv2",
                            "TOPSECRET-CHANNEL Beeblebrox",
                            json!({"description": "TOPSECRET-DESCRIPTION", "channelNewFieldQ7": "TOPSECRET-VALUE",
                                   "channelType": 1, "threadSubType": "TOPSECRET-SUBTYPE",
                                   "lastMessage": teams::tests::last_message(json!({
                                       "content": "TOPSECRET-CHANNEL-WORDS", "imDisplayName": "Zaphod Beeblebrox",
                                       "lastMessageNewFieldQ7": "TOPSECRET-FIELD"}))}),
                        ),
                        json!({"displayName": "TOPSECRET-NO-ID-CHANNEL"}),
                    ],
                    json!({"description": "TOPSECRET-TEAM-DESCRIPTION", "teamNewFieldQ7": "TOPSECRET-VALUE"}),
                ),
                json!({"id": "TOPSECRET-BAD-TEAM-ID", "displayName": null, "extra": "TOPSECRET-UNREADABLE"}),
            ], json!([
                teams::tests::with(
                    teams::tests::folder("Favorites", false, vec![
                        teams::tests::folder_item("19:TOPSECRET-CH@thread.tacv2", "topic"),
                        teams::tests::folder_item("19:TOPSECRET-NOWHERE@thread.tacv2", "topic"),
                        teams::tests::with(
                            teams::tests::folder_item("19:TOPSECRET-FAVCHAT@thread.v2", "chat"),
                            json!({"itemNewFieldQ7": "TOPSECRET-VALUE"}),
                        ),
                    ]),
                    json!({"name": "TOPSECRET-FOLDER-NAME", "folderNewFieldQ7": "TOPSECRET-VALUE"}),
                ),
            ]))),
            // The person's order of their teams: every field holds something that must not be logged. It names the
            // team, a team that is not in the list, and has an entry that is not a team and an extra field.
            Reply::status(200).json(
                &json!({"cid": "TOPSECRET-CID", "cidHex": "TOPSECRET-CIDHEX", "dogfoodUser": false,
                        "primaryMemberName": "zaphod@TOPSECRET.test", "skypeName": "live:TOPSECRET-SKYPE",
                        "orderNewFieldQ7": "TOPSECRET-VALUE",
                        "teamsOrder": json!([
                            {"order": 1, "teamId": "19:TOPSECRET-STALE@thread.tacv2", "entryNewFieldQ7": "TOPSECRET-VALUE"},
                            {"order": 0, "teamId": "19:TOPSECRET-TEAMID@thread.tacv2"},
                            "TOPSECRET-NOT-AN-ENTRY",
                        ]).to_string()})
                .to_string(),
            ),
            Reply::status(500).json(r#"{"message":"boom"}"#),
            // A post with a title, a reply the service fails on, and an edit of a post.
            Reply::status(201).json(r#"{"OriginalArrivalTime": 1793095260000}"#),
            Reply::status(500).json(r#"{"message":"boom"}"#),
            Reply::status(200).body(""),
            // A post that mentions someone, and an edit that does.
            Reply::status(201).json(r#"{"OriginalArrivalTime": 1793095260000}"#),
            Reply::status(200).body(""),
            // Presence with a status message and an out-of-office reply: their words are shown, never logged.
            Reply::status(200).json(
                &json!([
                    {"mri": format!("8:orgid:{ADA}"), "etag": "1",
                     "presence": {"availability": "Away", "calendarData": {"isOutOfOffice": true,
                         "outOfOfficeNote": {"message": "TOPSECRET-OUT-OF-OFFICE-WORDS", "outNoteNewFieldQ7": "TOPSECRET-VALUE"}},
                         "note": {"message": "<a href=\"https://TOPSECRET.test\">TOPSECRET-STATUS-WORDS</a>",
                                  "noteNewFieldQ7": "TOPSECRET-VALUE"},
                         "presenceNewFieldQ7": "TOPSECRET-VALUE"}},
                ])
                .to_string(),
            ),
        ]);
        let mut b = backend(&server);
        b.presence_base = server.origin().to_string();
        *lock(&b.presence) = Some(("presence-token".into(), now() + 3600));
        b.search_url = server.url("/searchservice/api/v2/query");
        *lock(&b.substrate) = Some(("substrate-token".into(), now() + 3600));
        if let Some(live) = lock(&b.live).as_mut() {
            live.csa_base = server.url("/api/csa/amer");
        }
        *lock(&b.csa) = Some(("csa-token".into(), now() + 3600));
        b.chats().unwrap();
        b.chats().unwrap();
        b.messages(secret_chat, None).unwrap();
        b.messages(secret_chat, None).unwrap();
        assert!(b.messages(secret_chat, None).is_err());
        let found = b
            .search_messages(secret_chat, "TOPSECRET-QUERY \"TOPSECRET-INNER\" \\", 0)
            .unwrap();
        assert_eq!(found.hits.len(), 1);
        assert!(
            b.search_messages(secret_chat, "TOPSECRET-QUERY", 0)
                .is_err()
        );
        let both = b
            .search_conversations(
                &[
                    secret_chat.to_string(),
                    "19:TOPSECRET-SECOND@thread.tacv2".to_string(),
                ],
                "TOPSECRET-QUERY",
                0,
            )
            .unwrap();
        assert_eq!(both.hits.len(), 2, "one outside the scope, one naming none");
        assert_eq!(b.teams().unwrap().len(), 1);
        assert!(b.teams().is_err());
        assert_eq!(b.channel_activity().len(), 1);
        let secret_channel = "19:TOPSECRET-POSTCHANNEL@thread.tacv2";
        b.post_to_channel(
            secret_channel,
            &ChannelPost::new("<p>TOPSECRET-POSTBODY</p>").with_subject("TOPSECRET-SUBJECT"),
        )
        .unwrap();
        assert!(
            b.post_to_channel(
                secret_channel,
                &ChannelPost::new("<p>TOPSECRET-REPLYBODY</p>").in_thread("1793000000000"),
            )
            .is_err()
        );
        b.edit_post(
            secret_channel,
            "1793000000000",
            Some("TOPSECRET-SUBJECT"),
            "<p>TOPSECRET-EDITBODY</p>",
            &[],
        )
        .unwrap();
        // A post and an edit that mention someone: the name and the id are in the request and nowhere in the log.
        let secret_mention = [OutMention {
            id: 0,
            user_id: "deadbeef-0000-4000-8000-0000000000aa".to_string(),
            name: "TOPSECRET-MENTIONNAME".to_string(),
        }];
        b.post_to_channel(
            secret_channel,
            &ChannelPost::new(
                "<p><at id=\"0\">TOPSECRET-MENTIONNAME</at> TOPSECRET-MENTIONBODY</p>",
            )
            .with_mentions(&secret_mention),
        )
        .unwrap();
        b.edit_post(
            secret_channel,
            "1793000000000",
            None,
            "<p><at id=\"0\">TOPSECRET-MENTIONNAME</at> TOPSECRET-MENTIONEDIT</p>",
            &secret_mention,
        )
        .unwrap();
        assert!(
            b.post_to_channel(
                secret_channel,
                &ChannelPost::new("<p><at id=\"0\">TOPSECRET-MENTIONNAME</at></p>").with_mentions(
                    &[OutMention {
                        id: 0,
                        user_id: "TOPSECRET-MENTIONID".to_string(),
                        name: "TOPSECRET-MENTIONNAME".to_string(),
                    }]
                ),
            )
            .is_err()
        );
        // Refused before any request, from text that must not be named in the refusal either.
        assert!(
            b.post_to_channel(
                secret_channel,
                &ChannelPost::new("<p>x</p>").with_subject(&"TOPSECRET".repeat(30)),
            )
            .is_err()
        );
        let people = b.presence(&[ADA.to_string()]).unwrap();
        let ada = &people[ADA];
        assert_eq!(
            ada.note.as_ref().map(|n| n.text.as_str()),
            Some("TOPSECRET-STATUS-WORDS"),
            "the words are read (the test is that they are not logged)"
        );
        assert_eq!(
            ada.out_of_office_note.as_ref().map(|n| n.text.as_str()),
            Some("TOPSECRET-OUT-OF-OFFICE-WORDS")
        );

        let log = lock(&LOGGED).join("\n");
        for secret in ["TOPSECRET", "Zaphod", "Beeblebrox"] {
            assert!(!log.contains(secret), "{secret} reached the log:\n{log}");
        }
        for expected in [
            "list chats: GET",
            "chat list: read in full",
            "change feed",
            "read messages: GET",
            "changed since version",
            "unreadable item, shape",
            "brandNewFieldQ7: string",
            "newMessageFieldQ7: number",
            "-> 500",
            "search messages: POST",
            "search messages: chat ",
            "search messages: 2 conversation(s) from 0: 4 result(s), 2 read, 1 from another chat left out",
            "search messages: 2 item(s), 1 read",
            "searchNewFieldQ7: string",
            "post to channel ",
            "a post with a subject",
            "a reply, 26 bytes of HTML, 0 mention(s)",
            "a post, 66 bytes of HTML, 1 mention(s)",
            "list teams: GET",
            "list teams: 2 team(s) in the answer, 1 read with 1 channel(s)",
            "list teams: unreadable team, shape",
            "teamNewFieldQ7: string",
            "channelNewFieldQ7: string",
            "lastMessageNewFieldQ7: string",
            "channel type: values seen: 1 shared=false",
            "list teams: 2 team or channel favourite(s) in the folder, 1 matched a channel, 1 matched none, 1 chat(s)",
            "folderNewFieldQ7: string",
            "itemNewFieldQ7: string",
            "channel activity: 1 channel(s) in the chat list, 1 unread",
            "read the order of the teams: 2 team id(s) in the order, 1 matched a team, 1 matched none, 0 team(s) not in it",
            "orderNewFieldQ7: string",
            "entryNewFieldQ7: string",
            "presence: asked about 1, 1 answered",
            "presenceNewFieldQ7: string",
            "noteNewFieldQ7: string",
            "outNoteNewFieldQ7: string",
        ] {
            assert!(log.contains(expected), "missing {expected:?} in:\n{log}");
        }
        assert!(
            log.lines()
                .any(|l| l.starts_with("INFO") && l.contains("brandNewFieldQ7")),
            "a field that appears after the first sighting is announced at info"
        );
    }

    const CATALOG: &str = r#"{"categories": [{"id": "all", "emoticons": [
        {"id": "like", "description": "Like", "shortcuts": ["(like)"], "unicode": "👍", "keywords": []},
        {"id": "1f37b_clinkingbeermugs", "description": "Clinking beer mugs", "shortcuts": ["(clinkingbeermugs)"], "unicode": "🍻", "keywords": ["beer"]}]}]}"#;

    /// A backend that reads Teams' emoji catalogue from `server` as well.
    fn backend_with_catalog(server: &Server) -> TeamsBackend {
        let mut b = backend(server);
        b.emoji_catalog_urls = vec![server.url("/catalog.json")];
        b
    }

    #[test]
    fn an_edit_is_a_put_to_the_message_naming_the_message_it_replaces() {
        let server = serve(vec![
            Reply::status(200).body("null"),
            Reply::status(200).body("null"),
        ]);
        let b = backend(&server);
        assert!(b.can_edit());
        b.edit(CHAT, "1793790852152", "<p>new text</p>", &[])
            .unwrap();
        let mention = OutMention {
            id: 0,
            user_id: ADA.into(),
            name: "Ada".into(),
        };
        b.edit(
            CHAT,
            "1793790852152",
            "<p><at id=\"0\">Ada</at> hi</p>",
            &[mention],
        )
        .unwrap();
        let seen = server.seen();
        assert_eq!(seen[0].method, "PUT");
        assert!(
            seen[0]
                .path
                .ends_with("/conversations/19%3Ax@thread.v2/messages/1793790852152"),
            "{}",
            seen[0].path
        );
        assert_eq!(seen[0].header("authorization"), Some("Bearer spaces-token"));
        let body: Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(body["content"], "<p>new text</p>");
        assert_eq!(body["messagetype"], "RichText/Html");
        assert_eq!(body["skypeeditedid"], "1793790852152");
        assert!(body["properties"].is_null());
        let with_mention: Value = serde_json::from_str(&seen[1].body).unwrap();
        assert!(
            with_mention["content"]
                .as_str()
                .unwrap()
                .contains("schema.skype.com/Mention")
        );
        assert!(with_mention["properties"]["mentions"].is_string());
    }

    #[test]
    fn an_edit_of_a_message_with_an_unusable_id_is_refused_before_any_request() {
        let server = serve(vec![]);
        let b = backend(&server);
        for id in ["", "12x", "../1", "1700 000"] {
            assert!(
                matches!(
                    b.edit(CHAT, id, "<p>x</p>", &[]),
                    Err(Error::InvalidInput { .. })
                ),
                "{id:?}"
            );
        }
        assert!(server.seen().is_empty());
    }

    #[test]
    fn a_refused_edit_is_an_error() {
        let server = serve(vec![Reply::status(403).json(r#"{"message":"not yours"}"#)]);
        let b = backend(&server);
        assert!(matches!(
            b.edit(CHAT, "1700000000000", "<p>x</p>", &[]),
            Err(Error::Status { status: 403, .. })
        ));
    }

    #[test]
    fn a_reaction_is_added_with_put_and_removed_with_delete() {
        let server = serve(vec![
            Reply::status(200).body(""),
            Reply::status(200).json(CATALOG),
            Reply::status(200).body(""),
            Reply::status(200).body(""),
            Reply::status(200).body(""),
        ]);
        let b = backend_with_catalog(&server);
        b.react(CHAT, "1700000000000", "👍", true).unwrap();
        b.react(CHAT, "1700000000000", "🍻", true).unwrap();
        b.react(CHAT, "1700000000000", "🍻", false).unwrap();
        b.react(CHAT, "1700000000000", "👍", false).unwrap();
        let seen = server.seen();
        assert_eq!(
            seen.len(),
            5,
            "the catalogue is read once, by the first emoji that needs it"
        );
        assert_eq!(seen[1].method, "GET");
        assert!(seen[1].path.ends_with("/catalog.json"));
        assert_eq!(
            seen[1].header("authorization"),
            None,
            "the catalogue is public and gets no credentials"
        );
        for i in [0, 2, 3, 4] {
            assert!(
                seen[i]
                    .path
                    .ends_with("/messages/1700000000000/properties?name=emotions"),
                "{}",
                seen[i].path
            );
            assert_eq!(seen[i].header("authorization"), Some("Bearer spaces-token"));
        }
        let body = |i: usize| serde_json::from_str::<Value>(&seen[i].body).unwrap();
        assert_eq!(seen[0].method, "PUT");
        assert_eq!(body(0)["emotions"]["key"], "like", "the classic words");
        assert!(body(0)["emotions"]["value"].as_i64().unwrap() > 1_700_000_000_000);
        assert_eq!(seen[2].method, "PUT");
        assert_eq!(
            body(2)["emotions"]["key"],
            "1f37b_clinkingbeermugs",
            "any other emoji by the id the catalogue gives it"
        );
        assert_eq!(seen[3].method, "DELETE");
        assert_eq!(
            body(3),
            json!({"emotions": {"key": "1f37b_clinkingbeermugs"}}),
            "removal names the reaction only"
        );
        assert_eq!(seen[4].method, "DELETE");
        assert_eq!(body(4), json!({"emotions": {"key": "like"}}));
    }

    #[test]
    fn reading_a_chat_is_recorded_with_a_put_of_the_read_marker() {
        let server = serve(vec![Reply::status(200).body("")]);
        let b = backend(&server);
        assert!(b.marks_read());
        let arrived = Utc.timestamp_millis_opt(1_700_000_123_000).unwrap();
        b.mark_read(CHAT, "1700000123000", arrived).unwrap();
        let seen = &server.seen()[0];
        assert_eq!(seen.method, "PUT");
        assert!(
            seen.path.ends_with("/properties?name=consumptionhorizon"),
            "{}",
            seen.path
        );
        assert!(seen.path.contains("19%3Ax@thread.v2"), "{}", seen.path);
        assert_eq!(seen.header("authorization"), Some("Bearer spaces-token"));
        let body: Value = serde_json::from_str(&seen.body).unwrap();
        assert_eq!(
            body,
            json!({"consumptionhorizon": "1700000123000;1700000123000;1700000123000"})
        );
    }

    #[test]
    fn a_marker_for_a_message_id_that_is_not_a_time_is_refused_before_any_request() {
        let server = serve(vec![]);
        let b = backend(&server);
        for id in ["", "../x", "local-3", "12 3"] {
            assert!(
                matches!(
                    b.mark_read(CHAT, id, Utc::now()),
                    Err(Error::InvalidInput { .. })
                ),
                "{id:?}"
            );
        }
        assert!(server.seen().is_empty());
    }
    #[test]
    fn the_reaction_list_is_the_catalogue() {
        let server = serve(vec![Reply::status(200).json(CATALOG)]);
        let b = backend_with_catalog(&server);
        let emoji: Vec<String> = b
            .reaction_emoji()
            .unwrap()
            .into_iter()
            .map(|i| i.emoji)
            .collect();
        assert_eq!(emoji, ["👍", "🍻"]);
        b.reaction_emoji().unwrap();
        assert_eq!(server.seen().len(), 1, "kept after the first download");
    }

    #[test]
    fn a_message_sent_again_is_shown_once_even_when_a_copy_comes_later_on_its_own() {
        let wire = |id: u64, version: u64, client: Option<&str>, body: &str| {
            let mut m = wire_msg(id, version, body);
            m["from"] = json!(format!("https://x.test/v1/users/ME/contacts/8:orgid:{ADA}"));
            if let Some(c) = client {
                m["clientmessageid"] = json!(c);
            }
            m
        };
        let same = "I see you have already knocked out a few of the blockers";
        let first = page(&[
            wire(1, 100_001, Some("c1"), same),
            wire(2, 100_002, Some("c1"), same),
            wire(3, 100_003, Some("c1"), same),
            wire(4, 100_004, Some("c1"), same),
            wire(5, 100_005, Some("c1"), same),
            // The same words sent on purpose again (a different client message id), the id reused for other words,
            // and a message with no client message id at all: all different messages.
            wire(6, 100_006, Some("c2"), same),
            wire(7, 100_007, Some("c1"), "other words"),
            wire(8, 100_008, None, same),
        ]);
        let later = page(&[
            // A copy edited or reacted to, and another resend that arrives only now.
            wire(3, 900_003, Some("c1"), same),
            wire(9, 900_009, Some("c1"), same),
        ]);
        let server = serve(vec![
            Reply::status(200).json(&first),
            Reply::status(200).json(&later),
        ]);
        let b = backend(&server);
        let ids = |p: &Page| p.messages.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
        assert_eq!(
            ids(&b.messages(CHAT, None).unwrap()),
            ["1", "6", "7", "8"],
            "the first copy is the one shown"
        );
        assert_eq!(
            ids(&b.messages(CHAT, None).unwrap()),
            ["1", "6", "7", "8"],
            "copies that come back by themselves stay out"
        );
    }

    #[test]
    fn the_repeat_diagnostics_do_no_work_when_info_logging_is_off() {
        // Serialised with the other tests that read the shared log buffer, which also set the level.
        let _serial = capture_logs();
        let b = TeamsBackend::new(
            TeamsSession::for_test(ME, "Me"),
            Arc::new(MemoryStore::new()),
            BrowserOptions::new("unused-profile"),
        );
        let twin = |id: &str| json!({"id": id, "from": "8:orgid:u", "content": "an identical sentence long enough to count", "clientmessageid": "c1"});
        let items = [twin("1"), twin("2")];
        let before = log::max_level();
        log::set_max_level(log::LevelFilter::Warn);
        lock(&LOGGED).clear();
        b.watch_repeats("19:chat@thread.v2", &items);
        let (seen_off, logged_off) = (lock(&b.repeats_seen).len(), lock(&LOGGED).len());
        log::set_max_level(log::LevelFilter::Trace);
        b.watch_repeats("19:chat@thread.v2", &items);
        let (seen_on, logged_on) = (lock(&b.repeats_seen).len(), lock(&LOGGED).len());
        log::set_max_level(before);
        assert_eq!(
            (seen_off, logged_off),
            (0, 0),
            "nothing is scanned or noted with info off"
        );
        assert!(
            seen_on > 0 && logged_on > 0,
            "with info on the repeat is still noted"
        );
    }
    #[test]
    fn messages_that_repeat_one_another_are_noted_in_the_log_by_counts_and_field_names_alone() {
        let _serial = capture_logs();
        let same = |id: u64, client: &str| {
            let mut m = wire_msg(id, id, "REPEATEDSECRET sentence that is long enough");
            m["clientmessageid"] = json!(client);
            m
        };
        let paragraphs = {
            let mut m = wire_msg(900, 900, "");
            m["content"] = json!("<p>PARAGRAPHSECRET repeated text over and over</p>".repeat(4));
            m
        };
        let server = serve(vec![Reply::status(200).json(&page(&[
            same(1, "c1"),
            same(2, "c1"),
            same(3, "c1"),
            paragraphs,
            wire_msg(5, 5, "one other message, said once"),
        ]))]);
        let b = backend(&server);
        lock(&LOGGED).clear();
        b.messages(CHAT, None).unwrap();
        let log = lock(&LOGGED).join("\n");
        assert!(
            log.contains("3 messages from one sender have identical content; share one client message id: true"),
            "{log}"
        );
        assert!(
            log.contains("fields that differ:") && log.contains("\"id\""),
            "{log}"
        );
        assert!(log.contains("holds the same paragraph 4 times"), "{log}");
        assert!(
            !log.contains("REPEATEDSECRET") && !log.contains("PARAGRAPHSECRET"),
            "{log}"
        );
        // Read again, it says nothing more.
        lock(&LOGGED).clear();
        let again = serve(vec![Reply::status(200).json(&page(&[
            same(1, "c1"),
            same(2, "c1"),
            same(3, "c1"),
        ]))]);
        let mut b2 = backend(&again);
        b2.repeats_seen = Mutex::new(lock(&b.repeats_seen).clone());
        b2.messages(CHAT, None).unwrap();
        assert!(!lock(&LOGGED).join("\n").contains("identical content"));
    }

    #[test]
    fn the_roster_of_a_chat_is_read_once_and_named_as_far_as_the_names_are_known() {
        let members = json!({"members": [
            {"id": format!("8:orgid:{ADA}"), "role": "Admin"},
            {"id": format!("8:orgid:{ME}"), "role": "User"},
        ]});
        let server = serve(vec![
            Reply::status(200).json(&members.to_string()),
            Reply::status(200).json(&json!({"value": [{"mri": format!("8:orgid:{ADA}"), "displayName": "Ada Lovelace"}]}).to_string()),
        ]);
        let b = backend(&server);
        lock(&b.live).as_mut().unwrap().mt_base =
            format!("{}/api/mt/part/amer-02", server.origin());
        let roster = b.members("19:meeting@thread.v2").unwrap();
        let shown: Vec<(&str, &str)> = roster
            .iter()
            .map(|m| (m.id.as_str(), m.name.as_str()))
            .collect();
        assert_eq!(
            shown,
            [(ADA, "Ada Lovelace"), (ME, "")],
            "named as far as the directory knew"
        );
        // Asked again: the roster is remembered, nothing is fetched twice.
        let before = server.seen().len();
        assert_eq!(b.members("19:meeting@thread.v2").unwrap().len(), 2);
        assert_eq!(
            server.seen().len(),
            before,
            "the roster and the names are remembered"
        );
        assert!(
            server.seen()[0].path.contains("19%3Ameeting@thread.v2"),
            "{}",
            server.seen()[0].path
        );
    }

    #[test]
    fn someone_a_name_lookup_leaves_out_is_marked_not_in_the_directory_until_an_answer_lists_them()
    {
        const GONE: &str = "0b1c2d3e-0000-4000-8000-00000000abcd";
        let members = json!({"members": [
            {"id": format!("8:orgid:{ADA}"), "role": "Admin"},
            {"id": format!("8:orgid:{GONE}"), "role": "User"},
            {"id": format!("8:orgid:{ME}"), "role": "User"},
        ]});
        let listing = |people: &[(&str, &str)]| {
            let value: Vec<Value> = people
                .iter()
                .map(|(id, name)| json!({"mri": format!("8:orgid:{id}"), "displayName": name}))
                .collect();
            Reply::status(200).json(&json!({ "value": value }).to_string())
        };
        let server = serve(vec![
            Reply::status(200).json(&members.to_string()),
            // Three asked, one listed: the other two were left out of an answer that worked.
            listing(&[(ADA, "Ada Lovelace")]),
            // Asked again, listed this time; Ada is left out of this one.
            listing(&[(GONE, "Back Again")]),
            // An answer listing nobody.
            listing(&[]),
        ]);
        let b = backend(&server);
        lock(&b.live).as_mut().unwrap().mt_base =
            format!("{}/api/mt/part/amer-02", server.origin());
        let roster = b.members("19:meeting@thread.v2").unwrap();
        let shown: Vec<(&str, bool)> = roster
            .iter()
            .map(|m| (m.id.as_str(), m.not_in_directory))
            .collect();
        assert_eq!(
            shown,
            [(ADA, false), (GONE, true), (ME, false)],
            "the one left out is marked; the signed-in person never is"
        );
        let ask = |ids: &[&str]| {
            lock(&b.looked_up).clear();
            lock(&b.names).clear();
            let ids: Vec<String> = ids.iter().map(|i| (*i).to_string()).collect();
            b.resolve_names(&b.live(None).unwrap(), &ids, true).unwrap();
        };
        ask(&[GONE, ADA]);
        assert!(
            !b.member(GONE.to_string()).not_in_directory,
            "an answer that lists them again takes the mark away"
        );
        assert!(
            b.member(ADA.to_string()).not_in_directory,
            "and this one left Ada out"
        );
        ask(&[GONE]);
        assert!(
            !b.member(GONE.to_string()).not_in_directory,
            "an answer that lists nobody says nothing about anyone"
        );
        assert_eq!(server.seen().len(), 4);
    }

    #[test]
    fn a_reaction_filed_under_a_word_is_shown_as_the_emoji_the_catalogue_says_it_is() {
        let users = |ids: &[&str]| -> Vec<Value> {
            ids.iter()
                .map(|id| json!({"mri": format!("8:orgid:{id}"), "time": 1}))
                .collect()
        };
        let page = json!({"messages": [{
            "id": "1700000000001", "messagetype": "Text", "content": "I'm all good",
            "originalarrivaltime": "2026-10-27T10:00:00.000Z",
            "from": format!("https://x.test/v1/users/ME/contacts/8:orgid:{ADA}"),
            "imdisplayname": "Ada",
            "properties": {"emotions": [
                {"key": "salute", "users": users(&[ADA])},
                {"key": "like", "users": users(&[ME])},
                // Another id for the same emoji as `like`, from the same person and another.
                {"key": "yes", "users": users(&[ME, ADA])},
                {"key": "1f37b_clinkingbeermugs", "users": users(&[ADA])},
                // Not in the catalogue: the word, as before.
                {"key": "mysteryword", "users": users(&[ADA])},
                {"key": "averyveryverylongidnobodyhaseverlisted", "users": users(&[ADA])},
            ]},
        }], "_metadata": {}});
        let catalogue = r#"{"categories": [{"id": "all", "emoticons": [
            {"id": "like", "description": "Like", "shortcuts": ["(like)"], "unicode": "👍", "keywords": []},
            {"id": "yes", "description": "Yes", "shortcuts": ["(yes)"], "unicode": "👍", "keywords": []},
            {"id": "salute", "description": "Saluting face", "shortcuts": ["(salute)"], "unicode": "🫡", "keywords": []}]}]}"#;
        let server = serve(vec![
            Reply::status(200).json(&page.to_string()),
            Reply::status(200).json(catalogue),
        ]);
        let b = backend_with_catalog(&server);
        let got = b.messages(CHAT, None).unwrap();
        let shown: Vec<(&str, usize)> = got.messages[0]
            .reactions
            .iter()
            .map(|r| (r.emoji.as_str(), r.count))
            .collect();
        // Both words for a thumbs-up are one reaction; a word the catalogue lacks stays a word, cut to 16 characters.
        assert_eq!(
            shown,
            [
                ("🫡", 1),
                ("👍", 2),
                ("🍻", 1),
                ("mysteryword", 1),
                ("averyveryverylon", 1)
            ],
            "{shown:?}"
        );
        assert_eq!(
            got.messages[0].reactions[1].users.len(),
            2,
            "one person under both ids counts once"
        );
        assert_eq!(
            server.seen().len(),
            2,
            "the messages, and the catalogue once"
        );
    }

    #[test]
    fn reactions_are_shown_by_their_ids_when_the_catalogue_cannot_be_had() {
        let page = json!({"messages": [{
            "id": "1700000000001", "messagetype": "Text", "content": "hi",
            "originalarrivaltime": "2026-10-27T10:00:00.000Z",
            "from": format!("https://x.test/v1/users/ME/contacts/8:orgid:{ADA}"),
            "properties": {"emotions": [
                {"key": "salute", "users": [{"mri": format!("8:orgid:{ADA}")}]},
                {"key": "like", "users": [{"mri": format!("8:orgid:{ADA}")}]},
            ]},
        }], "_metadata": {}});
        let server = serve(vec![
            Reply::status(200).json(&page.to_string()),
            Reply::status(500).body("down"),
        ]);
        let b = backend_with_catalog(&server);
        let got = b.messages(CHAT, None).unwrap();
        let shown: Vec<&str> = got.messages[0]
            .reactions
            .iter()
            .map(|r| r.emoji.as_str())
            .collect();
        assert_eq!(
            shown,
            ["salute", "👍"],
            "the message still arrives, the word stands in for the emoji"
        );
    }

    #[test]
    fn an_emoji_teams_has_no_reaction_for_is_refused_without_sending_it() {
        let server = serve(vec![Reply::status(200).json(CATALOG)]);
        let b = backend_with_catalog(&server);
        assert!(matches!(
            b.react(CHAT, "17", "🦄", true),
            Err(Error::InvalidInput { .. })
        ));
        assert_eq!(server.seen().len(), 1, "only the catalogue was fetched");
    }

    #[test]
    fn an_unreadable_catalogue_is_an_error_and_is_tried_again_after_a_pause() {
        let server = serve(vec![
            Reply::status(500).body("no"),
            Reply::status(200).json(CATALOG),
            Reply::status(200).body(""),
        ]);
        let mut b = backend_with_catalog(&server);
        assert!(b.react(CHAT, "17", "🍻", true).is_err());
        let after_one_attempt = server.seen().len();
        // Within the pause nothing more is asked of the CDN.
        assert!(b.reaction_emoji().is_err());
        assert_eq!(server.seen().len(), after_one_attempt);
        b.emoji_catalog_pause = Duration::ZERO;
        *lock(&b.emoji_catalog_retry) = None;
        b.react(CHAT, "17", "🍻", true).unwrap();
    }

    #[test]
    fn a_reaction_that_cannot_be_sent_safely_is_refused_before_any_request() {
        let server = serve(vec![]);
        let b = backend_with_catalog(&server);
        for (message, emoji) in [
            ("../../x", "👍"),
            ("", "👍"),
            ("17", "not an emoji"),
            ("17", ""),
            ("17", "a\u{7}"),
        ] {
            assert!(
                matches!(
                    b.react(CHAT, message, emoji, true),
                    Err(Error::InvalidInput { .. })
                ),
                "{message:?} {emoji:?}"
            );
        }
        assert!(server.seen().is_empty());
    }

    #[test]
    fn a_refused_reaction_is_an_error_not_a_silent_success() {
        let server = serve(vec![Reply::status(403).json(r#"{"message":"no"}"#)]);
        let b = backend(&server);
        assert!(matches!(
            b.react(CHAT, "17", "👍", true),
            Err(Error::Status { status: 403, .. })
        ));
    }
    #[test]
    fn only_media_service_images_are_fetchable() {
        assert!(is_media_url(
            "https://eu-api.asm.skype.com/v1/objects/0-weu-d1-abc/views/imgo"
        ));
        assert!(!is_media_url("https://asm.skype.com.evil.example/x"));
        assert!(!is_media_url("http://eu-api.asm.skype.com/x"));
        assert!(!is_media_url("https://example.com/a.png"));
    }

    #[test]
    fn any_teams_subdomain_is_a_media_service_and_lookalikes_are_not() {
        assert!(is_media_url(
            "https://us-prod.asyncgw.teams.microsoft.com/v1/objects/0-eus-d5-abc/views/imgo?v=1"
        ));
        assert!(is_media_url(
            "https://eu-prod.asyncgw.teams.microsoft.com/v1/objects/0-weu-d1-abc/views/imgo"
        ));
        assert!(is_media_url(
            "https://us-prod.teams.microsoft.com/v1/objects/x"
        ));
        assert!(is_media_url("https://a.b.teams.microsoft.com/v1/objects/x"));
        // The bare Teams host, lookalike names, other schemes, ports and user names get no skypetoken.
        for no in [
            "https://teams.microsoft.com/v1/objects/0-eus-d5-abc/views/imgo",
            "https://teams.microsoft.com.evil.example/v1/objects/x",
            "https://evilteams.microsoft.com.example/v1/objects/x",
            "https://asyncgw.teams.microsoft.com.evil.example/v1/objects/x",
            "http://us-prod.asyncgw.teams.microsoft.com/v1/objects/x",
            "https://us-prod.asyncgw.teams.microsoft.com:8443/v1/objects/x",
            "https://user@us-prod.asyncgw.teams.microsoft.com/v1/objects/x",
            "https://us-prod.asyncgw.teams.microsoft.com@evil.example/v1/objects/x",
        ] {
            assert!(!is_media_url(no), "{no}");
        }
    }

    #[test]
    fn a_sharepoint_attachment_is_also_fetchable() {
        let server = serve(vec![]);
        let b = backend(&server);
        assert!(b.serves_image("https://contoso-my.sharepoint.com/personal/bob/doc.png"));
    }

    #[test]
    fn a_sharepoint_attachment_from_the_same_tenant_is_resolved_and_downloaded() {
        let server = serve(vec![
            Reply::status(200).json(
                &json!({
                    "@microsoft.graph.downloadUrl": "{origin}/dl",
                    "sharepointIds": { "tenantId": "test-tenant" }
                })
                .to_string(),
            ),
            Reply::status(200).body("not an image"),
        ]);
        let mut b = backend(&server);
        b.graph_base = server.origin().to_string();
        *lock(&b.graph) = Some(("graph-token".into(), now() + 3600));
        let url = "https://contoso-my.sharepoint.com/personal/bob/doc.png";
        let err = b.image(url).unwrap_err();
        assert!(
            matches!(err, Error::Decode { .. }),
            "reached and tried to decode the resolved download: {err:?}"
        );
        let seen = server.seen();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].path.contains("/shares/u!"), "{}", seen[0].path);
        assert_eq!(seen[0].header("authorization"), Some("Bearer graph-token"));
        assert_eq!(seen[1].path, "/dl");
        assert!(
            seen[1].header("authorization").is_none(),
            "the pre-authenticated download gets no credential of ours"
        );
    }

    #[test]
    fn a_sharepoint_attachment_from_another_tenant_is_refused() {
        let server = serve(vec![
            Reply::status(200).json(
                &json!({
                    "@microsoft.graph.downloadUrl": "https://external.example/dl",
                    "sharepointIds": { "tenantId": "different-tenant" }
                })
                .to_string(),
            ),
        ]);
        let mut b = backend(&server);
        b.graph_base = server.origin().to_string();
        *lock(&b.graph) = Some(("graph-token".into(), now() + 3600));
        let url = "https://external-my.sharepoint.com/personal/attacker/malicious.png";
        let err = b.image(url).unwrap_err();
        assert!(
            matches!(
                err,
                Error::UnverifiedContent {
                    operation: "fetch image",
                    ..
                }
            ),
            "should refuse: {err:?}"
        );
        let seen = server.seen();
        assert_eq!(seen.len(), 1, "should not have downloaded");
    }

    /// The cached access tokens last up to an hour, but a dead grant means the sign-in is already gone: credentials
    /// are refused at once, as "signed out", instead of the person finding out when the cache runs dry.
    #[test]
    fn credentials_are_refused_once_the_sign_in_is_known_to_be_gone_though_the_cache_is_still_good()
    {
        let server = serve(vec![]);
        let b = backend(&server);
        assert!(b.live(None).is_ok(), "the cache is good");
        b.auth.lose_sign_in_for_test();
        let err = b.live(None).unwrap_err();
        assert!(err.needs_sign_in(), "{err:?}");
        assert!(
            server.seen().is_empty(),
            "told from what is already known, with no request"
        );
    }

    /// Logging response headers is switched on once for the backend and has to reach every agent it makes requests
    /// with: the API one (resolving the shared file) and the picture one (downloading it).
    #[test]
    fn header_logging_covers_the_api_and_picture_agents_and_never_the_credentials() {
        let server = serve(vec![
            Reply::status(200).header("X-Cache", "HIT").json(
                &json!({
                    "@microsoft.graph.downloadUrl": "{origin}/dl",
                    "sharepointIds": { "tenantId": "test-tenant" }
                })
                .to_string(),
            ),
            Reply::status(200)
                .header("X-Cache", "MISS")
                .body("not an image"),
        ]);
        let log = HeaderLog::parse("x-cache,authorization").unwrap();
        let mut b = backend(&server).with_header_log(log.clone());
        b.graph_base = server.origin().to_string();
        *lock(&b.graph) = Some(("graph-token".into(), now() + 3600));
        let _ = b.image("https://contoso-my.sharepoint.com/personal/bob/doc.png");
        let lines = log.captured();
        assert_eq!(lines.len(), 2, "one for each agent: {lines:?}");
        assert!(lines[0].contains("x-cache=HIT"), "{}", lines[0]);
        assert!(lines[1].contains("x-cache=MISS"), "{}", lines[1]);
        assert!(
            lines.iter().all(|l| !l.contains("graph-token")),
            "{lines:?}"
        );
    }

    #[test]
    fn a_picture_from_another_tenant_is_downloaded_only_when_the_user_asks_for_it() {
        let answer = |tenant: Option<&str>| {
            let mut v = json!({ "@microsoft.graph.downloadUrl": "{origin}/dl" });
            if let Some(t) = tenant {
                v["sharepointIds"] = json!({ "tenantId": t });
            }
            Reply::status(200).json(&v.to_string())
        };
        for tenant in [Some("different-tenant"), None] {
            let server = serve(vec![
                answer(tenant),
                Reply::status(200).body("not an image"),
            ]);
            let mut b = backend(&server);
            b.graph_base = server.origin().to_string();
            *lock(&b.graph) = Some(("graph-token".into(), now() + 3600));
            let url = "https://external-my.sharepoint.com/personal/someone/pic.png";
            let err = b.image_from_outside(url).unwrap_err();
            assert!(
                matches!(err, Error::Decode { .. }),
                "{tenant:?}: the explicit call reached the download: {err:?}"
            );
            let seen = server.seen();
            assert_eq!(seen.len(), 2, "{tenant:?}");
            assert!(seen[0].path.contains("/shares/u!"), "{}", seen[0].path);
            assert_eq!(seen[1].path, "/dl");
            assert!(
                seen[1].header("authorization").is_none(),
                "an outside download still gets no credential of ours"
            );
        }
    }

    #[test]
    fn asking_for_an_outside_picture_does_not_widen_which_hosts_get_the_skypetoken() {
        let server = serve(vec![]);
        let b = backend(&server);
        let err = b
            .image_from_outside("https://evil.example/pic.png")
            .unwrap_err();
        assert!(matches!(err, Error::UntrustedUrl { .. }), "{err:?}");
        assert!(server.seen().is_empty());
    }

    #[test]
    fn a_sharepoint_attachment_with_no_tenant_id_is_refused() {
        let server =
            serve(vec![
                Reply::status(200).json(
                    &json!({ "@microsoft.graph.downloadUrl": "https://external.example/dl" })
                        .to_string(),
                ),
            ]);
        let mut b = backend(&server);
        b.graph_base = server.origin().to_string();
        *lock(&b.graph) = Some(("graph-token".into(), now() + 3600));
        let url = "https://unknown-my.sharepoint.com/personal/x/pic.png";
        let err = b.image(url).unwrap_err();
        assert!(
            matches!(err, Error::UnverifiedContent { .. }),
            "missing tenant ID should be unverified: {err:?}"
        );
    }

    /// Graph chooses the download URL and nothing checks it against a list, so it has to be what a public download
    /// is: https, a DNS name, no user name, the ordinary port. Anything else is refused before a request is made.
    #[test]
    fn a_download_url_that_is_not_a_public_https_name_is_never_requested() {
        let elsewhere = serve(vec![Reply::status(200).body("not an image")]);
        for target in shares::refused_download_targets(elsewhere.origin()) {
            let server = serve(vec![
                Reply::status(200).json(
                    &json!({
                        "@microsoft.graph.downloadUrl": target,
                        "sharepointIds": { "tenantId": "test-tenant" }
                    })
                    .to_string(),
                ),
            ]);
            let mut b = backend(&server);
            b.graph_base = server.origin().to_string();
            *lock(&b.graph) = Some(("graph-token".into(), now() + 3600));
            let err = b
                .image("https://contoso-my.sharepoint.com/personal/bob/doc.png")
                .unwrap_err();
            assert!(
                matches!(err, Error::UntrustedUrl { .. }),
                "{target}: {err:?}"
            );
            assert_eq!(server.seen().len(), 1, "{target}: only the lookup was sent");
        }
        assert!(
            elsewhere.seen().is_empty(),
            "nothing reached the other server"
        );
    }

    /// The same rule for every hop: a download that redirects somewhere it should not go is not followed there.
    #[test]
    fn a_download_that_redirects_to_something_that_is_not_a_public_https_name_is_not_followed() {
        let elsewhere = serve(vec![Reply::status(200).body("not an image")]);
        for target in shares::refused_download_targets(elsewhere.origin()) {
            let server = serve(vec![
                Reply::status(200).json(
                    &json!({
                        "@microsoft.graph.downloadUrl": "{origin}/dl",
                        "sharepointIds": { "tenantId": "test-tenant" }
                    })
                    .to_string(),
                ),
                Reply::status(302).header("Location", &target),
            ]);
            let mut b = backend(&server);
            b.graph_base = server.origin().to_string();
            *lock(&b.graph) = Some(("graph-token".into(), now() + 3600));
            let err = b
                .image("https://contoso-my.sharepoint.com/personal/bob/doc.png")
                .unwrap_err();
            assert!(
                matches!(err, Error::UntrustedUrl { .. }),
                "{target}: {err:?}"
            );
            assert_eq!(
                server.seen().len(),
                2,
                "{target}: the lookup and the first hop"
            );
        }
        assert!(
            elsewhere.seen().is_empty(),
            "no hop reached the other server"
        );
    }

    #[test]
    fn requests_carry_both_tokens_and_the_chat_list_is_titled_from_senders() {
        let id = format!("19:{ME}_{ADA}@unq.gbl.spaces");
        let list = json!({"conversations": [{
            "id": id, "threadProperties": {"threadType": "chat"},
            "lastMessage": {"messagetype": "Text", "content": "hi", "imdisplayname": "Ada Lovelace",
                            "from": format!("8:orgid:{ADA}"), "originalarrivaltime": "2026-10-27T10:00:00Z"}
        }]});
        let server = serve(vec![Reply::status(200).json(&list.to_string())]);
        let b = backend(&server);
        let chats = b.chats().unwrap();
        assert_eq!(chats[0].title(ME), "Ada Lovelace");
        let seen = &server.seen()[0];
        assert_eq!(
            seen.header("authentication"),
            Some("skypetoken=skype-token")
        );
        assert_eq!(seen.header("authorization"), Some("Bearer spaces-token"));
        assert_eq!(seen.header("origin"), Some("https://teams.microsoft.com"));
        assert!(
            seen.path
                .starts_with("/api/chatsvc/amer/v1/users/ME/conversations?"),
            "{}",
            seen.path
        );
        assert!(b.take_warnings().is_empty());
    }

    #[test]
    fn group_chats_without_a_topic_fetch_their_members_once() {
        let list = json!({"conversations": [{
            "id": "19:grp@thread.v2",
            "lastMessage": {"messagetype": "Text", "content": "x", "imdisplayname": "Ada", "from": format!("8:orgid:{ADA}"), "originalarrivaltime": "2026-10-27T10:00:00Z"}
        }]});
        let server = serve(vec![
            Reply::status(200).json(&list.to_string()),
            Reply::status(200).json(&json!({"members": [{"id": format!("8:orgid:{ME}")}, {"id": format!("8:orgid:{ADA}")}]}).to_string()),
            Reply::status(200).json(&list.to_string()),
        ]);
        let b = backend(&server);
        let first = b.chats().unwrap();
        assert_eq!(
            first[0].title(ME),
            "Ada",
            "name learned from the sender, members from the lookup"
        );
        b.chats().unwrap();
        let paths: Vec<String> = server.seen().iter().map(|s| s.path.clone()).collect();
        assert_eq!(paths.len(), 3);
        assert!(
            paths[1].contains("/v1/threads/19%3Agrp@thread.v2?"),
            "{paths:?}"
        );
        assert!(
            !paths[2].contains("grp@thread.v2?view"),
            "the second refresh does not look the members up again: {paths:?}"
        );
    }

    #[test]
    fn a_failed_member_lookup_is_a_warning_and_is_not_retried_until_the_next_full_refresh() {
        // A chat that permanently fails (left, or no longer visible) must not keep occupying a lookup slot on every
        // read: enough of them would starve every other chat of its title forever.
        let list = json!({"conversations": [{
            "id": "19:grp@thread.v2",
            "lastMessage": {"messagetype": "Text", "content": "x", "originalarrivaltime": "2026-10-27T10:00:00Z"}
        }]});
        let server = serve(vec![
            Reply::status(200).json(&list.to_string()),
            Reply::status(500).body("boom"),
            Reply::status(200).json(&list.to_string()),
            // No lookup reply here: too soon to retry.
            Reply::status(200).json(&list.to_string()),
            Reply::status(200).json(&two_members()),
        ]);
        let mut b = backend(&server);
        assert_eq!(b.chats().unwrap().len(), 1, "the list still comes back");
        let warnings = b.take_warnings();
        assert!(
            matches!(warnings.as_slice(), [Error::Status { status: 500, .. }]),
            "{warnings:?}"
        );
        assert!(
            lock(&b.members).is_empty(),
            "a failure is not cached as no members"
        );

        b.chats().unwrap();
        assert_eq!(
            server.seen().len(),
            3,
            "too soon to retry: only the list was asked for"
        );
        assert!(
            b.take_warnings().is_empty(),
            "not attempted, so nothing to warn about"
        );

        b.full_refresh = Duration::ZERO;
        b.chats().unwrap();
        assert_eq!(
            server.seen().len(),
            5,
            "retried once the back-off has passed"
        );
        assert_eq!(lock(&b.members).len(), 1);
    }

    /// A group chat with no topic, so its title needs a member lookup.
    fn unnamed_group(n: usize) -> Value {
        json!({"id": format!("19:grp{n}@thread.v2"), "lastMessage": {"messagetype": "Text", "content": "x",
               "imdisplayname": "Ada", "from": format!("8:orgid:{ADA}"), "originalarrivaltime": "2026-10-27T10:00:00Z"}})
    }

    fn two_members() -> String {
        json!({"members": [{"id": format!("8:orgid:{ME}")}, {"id": format!("8:orgid:{ADA}")}]})
            .to_string()
    }

    /// Seen live: a burst of four lookups at start-up was answered with four refusals within 250 ms.
    #[test]
    fn a_throttled_first_lookup_is_the_only_one_sent() {
        let groups: Vec<Value> = (0..6).map(unnamed_group).collect();
        let server = serve(vec![
            Reply::status(200).json(&list(&groups, None)),
            Reply::status(429).header("Retry-After", "5").json("{}"),
        ]);
        let b = backend(&server);
        b.chats().unwrap();
        assert_eq!(
            server.seen().len(),
            2,
            "the list and one lookup, not the list and a burst of four"
        );
        assert!(lock(&b.members_paused_until).is_some());
    }

    /// And when the first lookup is fine the rest are not held up one by one: they go a few at a time as before.
    #[test]
    fn after_a_good_first_lookup_the_rest_go_a_few_at_a_time() {
        let groups: Vec<Value> = (0..6).map(unnamed_group).collect();
        let mut replies = vec![Reply::status(200).json(&list(&groups, None))];
        replies.extend((0..6).map(|_| Reply::status(200).json(&two_members())));
        let server = serve(replies);
        let b = backend(&server);
        b.chats().unwrap();
        assert_eq!(server.seen().len(), 7, "the list and all six lookups");
        assert_eq!(lock(&b.members).len(), 6);
    }

    #[test]
    fn a_service_that_asks_for_a_pause_gets_one_and_nobody_is_told() {
        let one = list(&[unnamed_group(1)], None);
        let server = serve(vec![
            Reply::status(200).json(&one),
            Reply::status(429).header("Retry-After", "30").json("{}"),
            Reply::status(200).json(&one),
            Reply::status(200).json(&one),
            Reply::status(200).json(&two_members()),
        ]);
        let b = backend(&server);
        b.chats().unwrap();
        assert!(
            b.take_warnings().is_empty(),
            "a title is not worth interrupting anyone for"
        );
        assert!(
            lock(&b.members_paused_until).is_some(),
            "but the service''s request is honoured"
        );

        b.chats().unwrap();
        let seen = server.seen();
        assert_eq!(seen.len(), 3);
        assert!(
            !seen[2].path.contains("/v1/threads/"),
            "no lookup while paused: {}",
            seen[2].path
        );

        *lock(&b.members_paused_until) = None;
        let chats = b.chats().unwrap();
        assert_eq!(server.seen().len(), 5);
        assert_eq!(chats[0].members.len(), 2, "titled once the pause is over");
        assert!(b.take_warnings().is_empty());
    }

    #[test]
    fn permanently_failing_lookups_do_not_starve_a_chat_that_would_succeed() {
        // A full budget of chats that will never resolve (left, or no longer visible) must not occupy the lookup
        // slots forever: once the negative cache excludes them, the one chat that was never even tried gets its turn.
        let bad: Vec<Value> = (0..MEMBER_LOOKUPS_PER_READ).map(unnamed_group).collect();
        let mut groups = bad;
        groups.push(unnamed_group(MEMBER_LOOKUPS_PER_READ));
        let all = list(&groups, None);
        let mut script = vec![Reply::status(200).json(&all)];
        script.extend(
            (0..MEMBER_LOOKUPS_PER_READ).map(|_| Reply::status(404).json(r#"{"message":"gone"}"#)),
        );
        script.push(Reply::status(200).json(&all));
        script.push(Reply::status(200).json(&two_members()));
        let server = serve(script);
        let b = backend(&server);
        let chats = b.chats().unwrap();
        assert_eq!(
            chats.iter().filter(|c| !c.members.is_empty()).count(),
            0,
            "the budget went entirely to the 24 that will never succeed"
        );
        b.take_warnings();

        let chats = b.chats().unwrap();
        assert_eq!(
            chats.iter().filter(|c| !c.members.is_empty()).count(),
            1,
            "the one chat that was never tried is no longer starved"
        );
        assert_eq!(
            server.seen().len(),
            1 + MEMBER_LOOKUPS_PER_READ + 2,
            "only the untried chat was looked up on the second read"
        );
    }

    #[test]
    fn a_long_list_is_titled_a_few_chats_at_a_time_over_several_reads() {
        let groups: Vec<Value> = (0..30).map(unnamed_group).collect();
        let all = list(&groups, None);
        let mut script = vec![Reply::status(200).json(&all)];
        script
            .extend((0..MEMBER_LOOKUPS_PER_READ).map(|_| Reply::status(200).json(&two_members())));
        script.push(Reply::status(200).json(&all));
        script.extend(
            (0..30 - MEMBER_LOOKUPS_PER_READ).map(|_| Reply::status(200).json(&two_members())),
        );
        let server = serve(script);
        let b = backend(&server);
        b.chats().unwrap();
        assert_eq!(
            server.seen().len(),
            1 + MEMBER_LOOKUPS_PER_READ,
            "one read, a capped number of lookups"
        );
        assert_eq!(lock(&b.members).len(), MEMBER_LOOKUPS_PER_READ);
        b.chats().unwrap();
        assert_eq!(server.seen().len(), 2 + 30);
        assert_eq!(
            lock(&b.members).len(),
            30,
            "the rest were done on the next read"
        );
        assert!(b.take_warnings().is_empty());
    }

    #[test]
    fn a_lookup_that_fails_for_any_other_reason_is_still_reported() {
        let one = list(&[unnamed_group(1)], None);
        let server = serve(vec![
            Reply::status(200).json(&one),
            Reply::status(500).json(r#"{"message":"boom"}"#),
        ]);
        let b = backend(&server);
        b.chats().unwrap();
        assert!(matches!(
            b.take_warnings().as_slice(),
            [Error::Status { status: 500, .. }]
        ));
        assert!(
            lock(&b.members_paused_until).is_none(),
            "only throttling pauses the lookups"
        );
    }
    #[test]
    fn unreadable_conversations_are_reported_not_hidden() {
        let list = json!({"conversations": [{"no_id": true}, {"id": "19:ok@thread.v2", "threadProperties": {"topic": "Team"},
            "lastMessage": {"messagetype": "Text", "content": "x", "originalarrivaltime": "2026-10-27T10:00:00Z"}}]});
        let server = serve(vec![Reply::status(200).json(&list.to_string())]);
        let b = backend(&server);
        assert_eq!(b.chats().unwrap().len(), 1);
        assert!(matches!(
            b.take_warnings().as_slice(),
            [Error::Unexpected {
                operation: "list chats",
                ..
            }]
        ));
    }

    #[test]
    fn searching_for_yourself_opens_the_notes_conversation_not_a_chat_with_two_copies_of_you() {
        // No request is scripted: opening it needs none, and it is not created on the first send.
        let server = serve(vec![]);
        let b = backend(&server);
        let chat = b.open_chat(&contact(ME)).unwrap();
        assert_eq!(
            (chat.id.as_str(), chat.kind),
            ("48:notes", ChatKind::OneOnOne)
        );
        assert_eq!(chat.title(ME), "Notes to self");
        assert!(!b.is_fresh(&chat.id));
        let upper = b.open_chat(&contact(&ME.to_ascii_uppercase())).unwrap();
        assert_eq!(upper.id, "48:notes");
        assert!(server.seen().is_empty());
    }

    #[test]
    fn messages_on_a_fresh_chat_404_as_empty_but_a_real_chat_does_not() {
        let server = serve(vec![
            Reply::status(404).json(r#"{"message":"no such conversation"}"#),
            Reply::status(404).json(r#"{"message":"no such conversation"}"#),
        ]);
        let b = backend(&server);
        let fresh = b.open_chat(&contact(ADA)).unwrap();
        assert!(b.messages(&fresh.id, None).unwrap().messages.is_empty());
        assert!(matches!(
            b.messages("19:real@thread.v2", None),
            Err(Error::Status { status: 404, .. })
        ));
    }

    #[test]
    fn paging_follows_trusted_links_only() {
        let page = |link: &str| json!({"messages": [{"id": "1", "messagetype": "Text", "content": "x", "originalarrivaltime": "2026-10-27T10:00:00Z"}], "_metadata": {"backwardLink": link}});
        let server = serve(vec![
            Reply::status(200).json(&page("{origin}/api/chatsvc/amer/older").to_string()),
            Reply::status(200).json(&page("https://evil.example/older").to_string()),
        ]);
        let b = backend(&server).with_incremental_reads(false);
        let first = b.messages("19:x@thread.v2", None).unwrap();
        assert_eq!(
            first.older.as_deref(),
            Some(server.url("/api/chatsvc/amer/older").as_str())
        );
        assert_eq!(
            b.messages("19:x@thread.v2", None).unwrap().older,
            None,
            "an untrusted backward link is dropped"
        );
        assert!(matches!(
            b.messages("19:x@thread.v2", Some("https://evil.example/steal")),
            Err(Error::UntrustedUrl { .. })
        ));
        assert_eq!(
            server.seen().len(),
            2,
            "nothing was sent to the untrusted cursor"
        );
    }

    #[test]
    fn sending_to_a_new_chat_creates_the_conversation_when_teams_says_it_does_not_exist() {
        let server = serve(vec![
            Reply::status(404).json(r#"{"message":"conversation not found"}"#),
            Reply::status(201).body(""),
            Reply::status(200).json(r#"{"OriginalArrivalTime": 1793095260000}"#),
        ]);
        let b = backend(&server);
        let chat = b.open_chat(&contact(ADA)).unwrap();
        let sent = b.send(&chat.id, "<p>hello</p>", &[], None).unwrap();
        assert_eq!(
            (
                sent.id.as_str(),
                sent.body.as_str(),
                sent.from_id.as_deref()
            ),
            ("1793095260000", "<p>hello</p>", Some(ME))
        );
        let seen = server.seen();
        assert_eq!(seen.len(), 3);
        assert!(
            seen[0].path.ends_with("/messages")
                && seen[1].path.ends_with("/v1/threads")
                && seen[2].path.ends_with("/messages"),
            "{:?}",
            seen.iter().map(|s| &s.path).collect::<Vec<_>>()
        );
        let created: Value = serde_json::from_str(&seen[1].body).unwrap();
        assert_eq!(created["properties"]["uniquerosterthread"], true);
        assert_eq!(created["members"].as_array().unwrap().len(), 2);
        let posted: Value = serde_json::from_str(&seen[2].body).unwrap();
        assert_eq!(
            (
                posted["messagetype"].as_str(),
                posted["content"].as_str(),
                posted["from"].as_str()
            ),
            (
                Some("RichText/Html"),
                Some("<p>hello</p>"),
                Some(format!("8:orgid:{ME}").as_str())
            )
        );
        assert!(
            !b.is_fresh(&chat.id),
            "once sent, the chat is an ordinary one"
        );
    }

    #[test]
    fn a_send_carries_the_ic3_bearer_and_the_real_captures_other_headers() {
        let server = serve(vec![
            Reply::status(200).json(r#"{"OriginalArrivalTime": 1793095260000}"#),
        ]);
        backend(&server)
            .send("19:x@thread.v2", "<p>hi</p>", &[], None)
            .unwrap();
        let seen = &server.seen()[0];
        assert_eq!(seen.header("authorization"), Some("Bearer ic3-token"));
        for (name, value) in [
            ("x-ms-request-priority", "0"),
            ("behavioroverride", "redirectAs404"),
            ("x-ms-test-user", "False"),
            ("x-ms-migration", "True"),
            ("dnt", "1"),
        ] {
            assert_eq!(seen.header(name), Some(value), "{name}");
        }
        for absent in [
            "authentication",
            "origin",
            "x-ms-client-version",
            "clientinfo",
        ] {
            assert!(
                seen.header(absent).is_none(),
                "{absent} should not be sent with the IC3 bearer"
            );
        }
    }

    /// The IC3 bearer is a credential like the others: a chat service address that is not a Teams host gets nothing,
    /// not even an attempt, and the message is known not to have been sent.
    #[test]
    fn a_send_to_an_address_that_is_not_a_teams_host_sends_nothing() {
        let server = serve(vec![]);
        let elsewhere = serve(vec![
            Reply::status(200).json(r#"{"OriginalArrivalTime": 1793095260000}"#),
        ]);
        let b = backend(&server);
        lock(&b.live).as_mut().unwrap().chat_base =
            format!("{}/api/chatsvc/amer", elsewhere.origin());
        let err = b
            .send("19:x@thread.v2", "<p>hi</p>", &[], None)
            .unwrap_err();
        assert!(matches!(err, Error::UntrustedUrl { .. }), "{err:?}");
        assert!(!err.delivery_unknown(), "{err:?}");
        assert!(
            elsewhere.seen().is_empty(),
            "no request, so no bearer either"
        );
        assert!(server.seen().is_empty());
    }

    #[test]
    fn sending_to_an_ordinary_chat_does_not_create_anything_on_failure() {
        let server = serve(vec![Reply::status(404).json(r#"{"message":"gone"}"#)]);
        let b = backend(&server);
        assert!(matches!(
            b.send("19:real@thread.v2", "<p>x</p>", &[], None),
            Err(Error::Status { status: 404, .. })
        ));
        assert_eq!(server.seen().len(), 1);
    }

    #[test]
    fn mentions_are_sent_in_teams_markup_with_a_properties_list() {
        let server = serve(vec![
            Reply::status(200).json(r#"{"OriginalArrivalTime": 1793095260000}"#),
        ]);
        let mention = OutMention {
            id: 0,
            user_id: ADA.into(),
            name: "Ada".into(),
        };
        backend(&server)
            .send(
                "19:x@thread.v2",
                "<p><at id=\"0\">Ada</at> hi</p>",
                &[mention],
                None,
            )
            .unwrap();
        let posted: Value = serde_json::from_str(&server.seen()[0].body).unwrap();
        assert!(
            posted["content"]
                .as_str()
                .unwrap()
                .contains("schema.skype.com/Mention")
        );
        assert!(
            posted["properties"]["mentions"]
                .as_str()
                .unwrap()
                .contains(&format!("8:orgid:{ADA}"))
        );
    }

    #[test]
    fn every_send_carries_the_properties_a_genuine_client_always_sends() {
        let server = serve(vec![
            Reply::status(200).json(r#"{"OriginalArrivalTime": 1793095260000}"#),
        ]);
        backend(&server)
            .send("19:x@thread.v2", "<p>hi</p>", &[], None)
            .unwrap();
        let posted: Value = serde_json::from_str(&server.seen()[0].body).unwrap();
        let props = &posted["properties"];
        assert_eq!(props["mentions"].as_str(), Some("[]"));
        assert_eq!(props["formatVariant"].as_str(), Some("TEAMS"));
        assert_eq!(props["cards"].as_str(), Some("[]"));
        assert_eq!(props["files"].as_str(), Some("[]"));
        assert_eq!(props["links"].as_str(), Some("[]"));
    }

    #[test]
    fn a_reply_prepends_the_teams_quote_markup_before_the_message_body() {
        let server = serve(vec![
            Reply::status(200).json(r#"{"OriginalArrivalTime": 1793095260000}"#),
        ]);
        let target = ReplyTarget {
            id: "1793985834981".into(),
            from_id: Some(ADA.into()),
            from_name: "Ada".into(),
            preview: "see you then".into(),
        };
        let sent = backend(&server)
            .send("19:x@thread.v2", "<p>sounds good</p>", &[], Some(&target))
            .unwrap();
        let posted: Value = serde_json::from_str(&server.seen()[0].body).unwrap();
        let content = posted["content"].as_str().unwrap();
        assert!(
            content.starts_with(
                "<blockquote itemscope=\"\" itemtype=\"http://schema.skype.com/Reply\""
            ),
            "{content}"
        );
        assert!(content.ends_with("<p>sounds good</p>"), "{content}");
        assert_eq!(
            sent.body, content,
            "the returned message matches what was sent"
        );
        let qtd_msgs = posted["properties"]["qtdMsgs"].as_str().unwrap();
        assert!(
            qtd_msgs.contains("\"messageId\":\"1793985834981\""),
            "{qtd_msgs}"
        );
        assert!(
            qtd_msgs.contains(&format!("\"sender\":\"8:orgid:{ADA}\"")),
            "{qtd_msgs}"
        );
        assert!(qtd_msgs.contains("\"time\":1793985834981"), "{qtd_msgs}");
    }

    #[test]
    fn an_ordinary_send_has_no_qtd_msgs_property() {
        let server = serve(vec![
            Reply::status(200).json(r#"{"OriginalArrivalTime": 1793095260000}"#),
        ]);
        backend(&server)
            .send("19:x@thread.v2", "<p>hi</p>", &[], None)
            .unwrap();
        let posted: Value = serde_json::from_str(&server.seen()[0].body).unwrap();
        assert!(posted["properties"]["qtdMsgs"].is_null());
    }

    #[test]
    fn people_search_posts_the_query_with_a_substrate_token() {
        let reply = json!({"Groups": [{"Suggestions": [{"Id": ADA, "MRI": format!("8:orgid:{ADA}"), "DisplayName": "Ada Lovelace", "EmailAddresses": ["ada@x.test"]}]}]});
        let server = serve(vec![Reply::status(200).json(&reply.to_string())]);
        let mut b = backend(&server);
        b.people_url = server.url("/search/api/v1/suggestions?scenario=powerbar");
        *lock(&b.substrate) = Some(("substrate-token".into(), now() + 3600));
        let people = b.find_people("ada").unwrap();
        assert_eq!(
            (people[0].id.as_str(), people[0].email.as_str()),
            (ADA, "ada@x.test")
        );
        let seen = &server.seen()[0];
        assert_eq!(seen.header("authorization"), Some("Bearer substrate-token"));
        let body: Value = serde_json::from_str(&seen.body).unwrap();
        assert_eq!(body["EntityRequests"][0]["Query"]["QueryString"], "ada");
    }

    #[test]
    fn people_search_asks_for_phone_and_office_and_falls_back_at_once_if_the_service_refuses_them()
    {
        let reply = json!({"Groups": [{"Suggestions": [{"Id": ADA, "DisplayName": "Ada Lovelace",
            "Phones": [{"Type": "Business", "Number": "+1 555 0100"}], "OfficeLocation": "Building 4"}]}]});
        let fields = |seen: &crate::testserver::Seen| -> Vec<String> {
            let body: Value = serde_json::from_str(&seen.body).unwrap();
            body["EntityRequests"][0]["Fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f.as_str().unwrap().to_string())
                .collect()
        };
        // Accepted: the extras are asked for and read.
        let server = serve(vec![Reply::status(200).json(&reply.to_string())]);
        let mut b = backend(&server);
        b.people_url = server.url("/search/api/v1/suggestions?scenario=powerbar");
        *lock(&b.substrate) = Some(("substrate-token".into(), now() + 3600));
        let ada = b.find_people("ada").unwrap().remove(0);
        assert_eq!(
            (ada.phone.as_str(), ada.office.as_str()),
            ("+1 555 0100", "Building 4")
        );
        assert!(fields(&server.seen()[0]).iter().any(|f| f == "Phones"));

        // Refused: asked again at once without them, and not asked for them again.
        let server = serve(vec![
            Reply::status(400).json(r#"{"error": "unknown field"}"#),
            Reply::status(200).json(&reply.to_string()),
            Reply::status(200).json(&reply.to_string()),
        ]);
        let mut b = backend(&server);
        b.people_url = server.url("/search/api/v1/suggestions?scenario=powerbar");
        *lock(&b.substrate) = Some(("substrate-token".into(), now() + 3600));
        assert_eq!(
            b.find_people("ada").unwrap()[0].id,
            ADA,
            "the search still works"
        );
        assert_eq!(b.find_people("ada").unwrap()[0].id, ADA);
        let seen = server.seen();
        assert_eq!(seen.len(), 3, "one refusal, then one request a search");
        let without: Vec<Vec<String>> = seen.iter().map(fields).collect();
        assert!(without[0].iter().any(|f| f == "Phones"));
        assert!(
            without[1..]
                .iter()
                .all(|f| !f.iter().any(|f| f == "Phones" || f == "OfficeLocation")),
            "{without:?}"
        );

        // Any other failure is not taken for a refusal of the fields.
        let server = serve(vec![Reply::status(500).json("{}")]);
        let mut b = backend(&server);
        b.people_url = server.url("/search/api/v1/suggestions?scenario=powerbar");
        *lock(&b.substrate) = Some(("substrate-token".into(), now() + 3600));
        assert!(b.find_people("ada").is_err());
        assert!(!b.people_extras_refused.load(Ordering::Relaxed));
    }

    #[test]
    fn presence_asks_for_each_person_once_with_the_presence_token_and_reads_what_comes_back() {
        let reply = json!([
            {"mri": format!("8:orgid:{ADA}"), "etag": "1", "status": 20000,
             "presence": {"availability": "Busy", "activity": "InACall", "calendarData": {"isOutOfOffice": true}}},
            {"mri": format!("8:orgid:{ME}"), "etag": "2",
             "presence": {"availability": "SomethingNew"}},
            {"mri": "8:orgid:44444444-0000-4000-8000-000000000000", "etagMatch": true, "status": 20000},
            {"mri": "4:+15555550100", "presence": {"availability": "Available"}},
        ]);
        let server = serve(vec![Reply::status(200).json(&reply.to_string())]);
        let mut b = backend(&server);
        b.presence_base = server.origin().to_string();
        *lock(&b.presence) = Some(("presence-token".into(), now() + 3600));
        let asked = [
            ADA.to_string(),
            ME.to_string(),
            ADA.to_string(),
            "not a guid; 8:orgid:x".to_string(),
        ];
        let got = b.presence(&asked).unwrap();
        assert_eq!(
            got.get(ADA),
            Some(&Presence::new(crate::Availability::Busy, true)),
            "{got:?}"
        );
        assert_eq!(
            got.get(ME),
            Some(&Presence::new(crate::Availability::Unknown, false)),
            "a word this version does not know"
        );
        assert_eq!(
            got.len(),
            2,
            "no answer for the unchanged one or the phone number: {got:?}"
        );
        let seen = &server.seen()[0];
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.header("authorization"), Some("Bearer presence-token"));
        let body: Value = serde_json::from_str(&seen.body).unwrap();
        assert_eq!(
            body,
            json!([{"mri": format!("8:orgid:{ADA}")}, {"mri": format!("8:orgid:{ME}")}]),
            "each person once, and nothing that is not an object id"
        );
    }

    #[test]
    fn presence_for_many_people_goes_in_batches_and_a_refusal_is_an_error_not_a_guess() {
        let ids: Vec<String> = (0..250)
            .map(|n| format!("{n:08x}-0000-4000-8000-000000000000"))
            .collect();
        let server = serve(vec![
            Reply::status(200).json("[]"),
            Reply::status(200).json("[]"),
            Reply::status(200).json("[]"),
            Reply::status(403).body("no"),
        ]);
        let mut b = backend(&server);
        b.presence_base = server.origin().to_string();
        *lock(&b.presence) = Some(("presence-token".into(), now() + 3600));
        assert!(b.presence(&ids).unwrap().is_empty());
        let sizes: Vec<usize> = server
            .seen()
            .iter()
            .map(|s| serde_json::from_str::<Vec<Value>>(&s.body).unwrap().len())
            .collect();
        assert_eq!(sizes, [100, 100, 50]);
        assert!(matches!(
            b.presence(&ids[..3]),
            Err(Error::Status { status: 403, .. })
        ));
        assert!(
            b.presence(&[]).unwrap().is_empty(),
            "nobody to ask about, nothing asked"
        );
        assert_eq!(server.seen().len(), 4);
    }

    #[test]
    fn choosing_a_status_puts_it_with_the_presence_token_and_giving_the_choice_up_deletes_it() {
        let server = serve(vec![
            Reply::status(200).body(""),
            Reply::status(200).body(""),
        ]);
        let mut b = backend(&server);
        b.presence_base = server.origin().to_string();
        *lock(&b.presence) = Some(("presence-token".into(), now() + 3600));
        b.set_status(Some(Availability::DoNotDisturb)).unwrap();
        b.set_status(None).unwrap();
        let seen = server.seen();
        assert_eq!(
            (seen[0].method.as_str(), seen[0].path.as_str()),
            ("PUT", "/v1/me/forceavailability/")
        );
        assert_eq!(
            seen[0].header("authorization"),
            Some("Bearer presence-token")
        );
        assert_eq!(
            serde_json::from_str::<Value>(&seen[0].body).unwrap(),
            json!({"availability": "DoNotDisturb"})
        );
        assert_eq!(
            (seen[1].method.as_str(), seen[1].path.as_str()),
            ("DELETE", "/v1/me/forceavailability/")
        );
        // What the service works out for itself cannot be chosen, and nothing is sent for it.
        for not_a_choice in [
            Availability::AvailableIdle,
            Availability::BusyIdle,
            Availability::Unknown,
        ] {
            assert!(
                matches!(
                    b.set_status(Some(not_a_choice)),
                    Err(Error::InvalidInput { .. })
                ),
                "{not_a_choice:?}"
            );
        }
        assert_eq!(server.seen().len(), 2);
        assert!(b.can_set_status());
    }

    #[test]
    fn reporting_activity_registers_this_program_once_and_again_if_the_service_forgot_it() {
        let server = serve(vec![
            Reply::status(200).body(""),     // register
            Reply::status(200).body(""),     // report: active
            Reply::status(200).body(""),     // report: idle
            Reply::status(404).body("gone"), // report: the service has forgotten us
            Reply::status(200).body(""),     // register again
            Reply::status(200).body(""),     // report
        ]);
        let mut b = backend(&server);
        b.presence_base = server.origin().to_string();
        *lock(&b.presence) = Some(("presence-token".into(), now() + 3600));
        b.report_activity(true).unwrap();
        b.report_activity(false).unwrap();
        b.report_activity(true).unwrap();
        let seen = server.seen();
        let steps: Vec<(&str, &str)> = seen
            .iter()
            .map(|s| (s.method.as_str(), s.path.as_str()))
            .collect();
        assert_eq!(
            steps,
            [
                ("PUT", "/v1/me/endpoints/"),
                ("POST", "/v1/me/reportmyactivity/"),
                ("POST", "/v1/me/reportmyactivity/"),
                ("POST", "/v1/me/reportmyactivity/"),
                ("PUT", "/v1/me/endpoints/"),
                ("POST", "/v1/me/reportmyactivity/"),
            ]
        );
        let body = |i: usize| serde_json::from_str::<Value>(&seen[i].body).unwrap();
        // It says who it is, as a desktop client that is reporting, and the same id every time.
        assert_eq!(body(0)["activityReporting"], "Transport");
        let id = body(0)["id"].clone();
        assert!(id.as_str().is_some_and(is_guid), "{id}");
        assert_eq!(body(1), json!({"endpointId": id, "isActive": true}));
        assert_eq!(body(2), json!({"endpointId": id, "isActive": false}));
        assert_eq!(body(5), json!({"endpointId": id, "isActive": true}));
    }

    #[test]
    fn presence_refuses_to_send_its_token_to_an_unexpected_host() {
        let server = serve(vec![]);
        let mut b = backend(&server);
        b.presence_base = "https://evil.example".into();
        *lock(&b.presence) = Some(("presence-token".into(), now() + 3600));
        assert!(matches!(
            b.presence(&[ADA.to_string()]),
            Err(Error::UntrustedUrl { .. })
        ));
    }

    #[test]
    fn people_search_refuses_to_send_the_substrate_token_to_an_unexpected_host() {
        let server = serve(vec![]);
        let mut b = backend(&server);
        b.people_url = "https://evil.example/search".into();
        assert!(matches!(
            b.find_people("ada"),
            Err(Error::UntrustedUrl { .. })
        ));
        assert!(
            b.find_people("   ").unwrap().is_empty(),
            "an empty query asks nothing"
        );
    }

    #[test]
    fn throttling_is_surfaced_with_the_retry_hint_and_images_stay_on_the_media_host() {
        let server = serve(vec![
            Reply::status(429).header("Retry-After", "11").body(""),
        ]);
        let b = backend(&server);
        match b.chats() {
            Err(Error::Throttled {
                operation: "list chats",
                retry_after: Some(d),
            }) => assert_eq!(d.as_secs(), 11),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            b.image("https://example.com/a.png"),
            Err(Error::UntrustedUrl { .. })
        ));
        assert!(matches!(
            b.open_chat(&contact("not-a-guid")),
            Err(Error::InvalidInput {
                what: "contact id",
                ..
            })
        ));
    }

    fn one_to_one_list() -> String {
        let id = format!("19:{ME}_{ADA}@unq.gbl.spaces");
        json!({"conversations": [{
            "id": id,
            "lastMessage": {"messagetype": "Text", "content": "hi", "originalarrivaltime": "2026-10-27T10:00:00Z"}
        }]})
        .to_string()
    }

    fn with_directory(b: &TeamsBackend, server: &Server) {
        lock(&b.live).as_mut().unwrap().mt_base =
            format!("{}/api/mt/part/amer-02", server.origin());
    }

    #[test]
    fn names_the_directory_does_not_know_are_asked_about_once() {
        let server = serve(vec![
            Reply::status(200).json(&one_to_one_list()),
            Reply::status(200).json(r#"{"value": []}"#),
            Reply::status(200).json(&one_to_one_list()),
        ]);
        let b = backend(&server);
        with_directory(&b, &server);
        b.chats().unwrap();
        b.chats().unwrap();
        let paths: Vec<String> = server.seen().iter().map(|s| s.path.clone()).collect();
        assert_eq!(paths.len(), 3, "{paths:?}");
        assert!(paths[1].contains("fetchShortProfile"), "{paths:?}");
        assert!(
            !paths[2].contains("fetchShortProfile"),
            "the second refresh does not ask again: {paths:?}"
        );
    }

    #[test]
    fn a_name_lookup_logs_the_size_of_what_it_asked_and_what_came_back_but_never_an_id_or_a_name() {
        let _serial = capture_logs();
        // 111 ids, 61 of them distinct: each is asked about once, so two batches (50, 11) and no repeats in either.
        let ids: Vec<String> = (0..111)
            .map(|n| format!("11111111-0000-0000-0000-{:012}", n % 61))
            .collect();
        let profile = |n: u32| json!({"mri": format!("8:orgid:11111111-0000-0000-0000-{n:012}"), "displayName": "Zaphod Beeblebrox"});
        let server = serve(vec![
            Reply::status(200).json(&json!({"value": [profile(1), profile(2)]}).to_string()),
            Reply::status(400).body(
                "The operation didn't complete within the allowed timeout for 11111111-0000-0000-0000-000000000005 \
                 (see https://x.example/y) ada@contoso.com",
            ),
        ]);
        let b = backend(&server);
        with_directory(&b, &server);
        let live = b.live(None).unwrap();
        lock(&LOGGED).clear();
        assert!(b.resolve_names(&live, &ids, false).is_err());
        // What went over the wire: every id once, across both requests.
        let asked: Vec<String> = server
            .seen()
            .iter()
            .filter(|s| s.path.contains("fetchShortProfile"))
            .flat_map(|s| {
                serde_json::from_str::<Vec<String>>(&s.body).expect("the body is a list of ids")
            })
            .collect();
        assert_eq!(asked.len(), 61, "{asked:?}");
        assert_eq!(asked.iter().collect::<HashSet<_>>().len(), 61, "{asked:?}");
        let log = lock(&LOGGED).join("\n");
        for expected in [
            "names: 61 unknown id(s), 61 distinct, in 2 batch(es) of up to 50",
            "names: batch 1 of 2: 50 id(s), 50 distinct,",
            "bytes sent, 2 profile(s) back,",
            "names: batch 2 of 2: 11 id(s), 11 distinct,",
            "failed after",
            // The service's own explanation of the failure, as it said it, with identifiers masked.
            "HTTP 400: The operation didn't complete within the allowed timeout for <id> (see <url> <email>",
        ] {
            assert!(log.contains(expected), "missing {expected:?} in:\n{log}");
        }
        assert!(
            log.lines()
                .any(|l| l.starts_with("WARN") && l.contains("batch 2 of 2")),
            "a failed batch is a warning, with its size:\n{log}"
        );
        for content in [
            "11111111-0000",
            "Zaphod",
            "Beeblebrox",
            "x.example",
            "contoso",
        ] {
            assert!(!log.contains(content), "{content} reached the log:\n{log}");
        }
    }

    fn four_ids() -> Vec<String> {
        (1..=4)
            .map(|n| format!("22222222-0000-0000-0000-{n:012}"))
            .collect()
    }

    fn profiles_for(ids: &[&str]) -> String {
        let value: Vec<Value> = ids
            .iter()
            .map(|id| json!({"mri": format!("8:orgid:{id}"), "displayName": format!("Name {id}")}))
            .collect();
        json!({ "value": value }).to_string()
    }

    #[test]
    fn an_attached_backend_looks_names_up_on_its_own_thread_and_the_next_read_has_them() {
        let server = serve(vec![
            Reply::status(200).json(&one_to_one_list()),
            Reply::status(200).json(&profiles_for(&[ADA])),
            Reply::status(200).json(&one_to_one_list()),
        ]);
        let b = backend(&server);
        with_directory(&b, &server);
        let b = Arc::new(b);
        b.attach();
        // The list is not held up by the lookup (it is answered with whatever names there are)...
        let first = b.chats().unwrap();
        assert_eq!(first.len(), 1);
        // ...and once the thread has the answer, the next read titles the chat.
        let deadline = Instant::now() + Duration::from_secs(10);
        while b.name_of(ADA).is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(b.chats().unwrap()[0].title(ME), format!("Name {ADA}"));
        while b.lookup_running.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !b.lookup_running.load(Ordering::SeqCst),
            "the thread finishes and another can start"
        );
    }

    #[test]
    fn the_directory_is_asked_about_a_chat_partner_whose_name_is_known_from_messages_and_the_list_is_read_again()
     {
        let server = serve(vec![
            Reply::status(200).json(&one_to_one_list()),
            // The answer lists someone, but not Ada.
            Reply::status(200).json(&profiles_for(&[ME])),
            Reply::status(200).json(&one_to_one_list()),
        ]);
        let b = backend(&server);
        with_directory(&b, &server);
        // What her messages said: a name, so no lookup is needed for it.
        lock(&b.names).insert(ADA.to_string(), "Ada From Messages".to_string());
        let b = Arc::new(b);
        b.attach();
        let first = b.chats().unwrap();
        assert!(!first[0].members.iter().any(|m| m.not_in_directory));
        wait_until("the lookup", || {
            !b.lookup_running.load(Ordering::SeqCst) && server.seen().len() >= 2
        });
        let asked = server.seen();
        assert!(
            asked[1].path.contains("fetchShortProfile") && asked[1].body.contains(ADA),
            "asked about although her name was known: {asked:?}"
        );
        assert_eq!(
            b.poll_pushes(),
            [PushHint::Unknown],
            "the app is told a read would draw the list differently"
        );
        let second = b.chats().unwrap();
        let ada = second[0].members.iter().find(|m| m.id == ADA).unwrap();
        assert!(ada.not_in_directory);
        assert_eq!(
            ada.name, "Ada From Messages",
            "the name from her messages stays"
        );
        assert_eq!(server.seen().len(), 3, "and she is not asked about again");
    }

    const REACTED_CHAT: &str = "19:reactions@thread.v2";

    /// A page of one message that Ada and the signed-in user both reacted to with a like.
    fn reacted_page() -> String {
        let mut m = wire_msg(1, 100_000, "hello");
        m["properties"] = json!({"emotions": json!([{"key": "like", "users": [
            {"mri": format!("8:orgid:{ADA}")}, {"mri": format!("8:orgid:{ME}")}]}]).to_string()});
        page(&[m])
    }

    fn reactor_names(p: &Page) -> Vec<String> {
        p.messages[0].reactions[0].names.clone()
    }

    #[test]
    fn who_reacted_is_looked_up_on_the_helper_thread_and_the_chat_is_read_again_soon() {
        let server = serve(vec![
            Reply::status(200).json(&reacted_page()),
            Reply::status(200).json(&profiles_for(&[ADA])),
            Reply::status(200).json(&reacted_page()),
        ]);
        let b = backend(&server).with_incremental_reads(false);
        with_directory(&b, &server);
        let b = Arc::new(b);
        b.attach();
        let first = b.messages(REACTED_CHAT, None).unwrap();
        assert_eq!(
            reactor_names(&first),
            ["", ""],
            "nothing known yet, and the read did not wait"
        );
        wait_until("the lookup", || {
            !b.lookup_running.load(Ordering::SeqCst) && !b.name_of(ADA).is_empty()
        });
        assert_eq!(b.poll_pushes(), [PushHint::Chat(REACTED_CHAT.into())]);
        let second = b.messages(REACTED_CHAT, None).unwrap();
        assert_eq!(
            reactor_names(&second),
            [format!("Name {ADA}"), String::new()]
        );
        let asked = server.seen();
        assert_eq!(asked.len(), 3, "one lookup, and none on the second read");
        assert!(
            !asked[1].body.contains(ME),
            "the signed-in user is never looked up (the app says \"You\")"
        );
    }

    #[test]
    fn a_name_already_known_is_filled_in_with_no_request_and_no_hint() {
        let server = serve(vec![Reply::status(200).json(&reacted_page())]);
        let b = backend(&server).with_incremental_reads(false);
        with_directory(&b, &server);
        lock(&b.names).insert(ADA.into(), "Ada Lovelace".into());
        let b = Arc::new(b);
        b.attach();
        let read = b.messages(REACTED_CHAT, None).unwrap();
        assert_eq!(reactor_names(&read), ["Ada Lovelace", ""]);
        assert!(
            !b.lookup_running.load(Ordering::SeqCst),
            "no lookup started"
        );
        assert_eq!(server.seen().len(), 1);
        assert!(b.poll_pushes().is_empty());
    }

    #[test]
    fn a_lookup_that_learns_nothing_queues_no_hint_and_is_not_asked_again() {
        let server = serve(vec![
            Reply::status(200).json(&reacted_page()),
            Reply::status(200).json(r#"{"value": []}"#),
            Reply::status(200).json(&reacted_page()),
        ]);
        let b = backend(&server).with_incremental_reads(false);
        with_directory(&b, &server);
        let b = Arc::new(b);
        b.attach();
        b.messages(REACTED_CHAT, None).unwrap();
        wait_until("the lookup", || {
            !b.lookup_running.load(Ordering::SeqCst) && lock(&b.looked_up).contains(ADA)
        });
        assert!(
            b.poll_pushes().is_empty(),
            "nothing learned, nothing to read again for"
        );
        b.messages(REACTED_CHAT, None).unwrap();
        assert!(!b.lookup_running.load(Ordering::SeqCst));
        assert_eq!(server.seen().len(), 3, "the id is not asked about again");
    }

    #[test]
    fn a_backend_that_is_not_attached_asks_nothing_for_reactions() {
        let server = serve(vec![Reply::status(200).json(&reacted_page())]);
        let b = backend(&server).with_incremental_reads(false);
        with_directory(&b, &server);
        let read = b.messages(REACTED_CHAT, None).unwrap();
        assert_eq!(reactor_names(&read), ["", ""]);
        assert_eq!(server.seen().len(), 1);
        assert!(b.poll_pushes().is_empty());
    }
    #[test]
    fn a_name_lookup_that_panics_does_not_stop_later_lookups() {
        let server = serve(vec![Reply::status(200).json(&profiles_for(&[ADA]))]);
        let b = backend(&server);
        with_directory(&b, &server);
        let b = Arc::new(b);
        b.attach();
        let live = lock(&b.live).clone().unwrap();
        b.panic_in_lookup.store(true, Ordering::SeqCst);
        b.look_up_names(&live, &[ADA.to_string()], false).unwrap();
        wait_until("the panicking lookup to end", || {
            !b.lookup_running.load(Ordering::SeqCst)
        });
        assert!(
            server.seen().is_empty(),
            "the panic came before any request"
        );
        // The helper is free again, so the next lookup runs and learns the name.
        b.panic_in_lookup.store(false, Ordering::SeqCst);
        b.look_up_names(&live, &[ADA.to_string()], false).unwrap();
        wait_until("the second lookup", || !b.name_of(ADA).is_empty());
        assert_eq!(b.name_of(ADA), format!("Name {ADA}"));
    }
    /// How long the scripted service takes over each roster answer. It answers one request at a time, so reading
    /// every roster of a list of groups takes this many times their number.
    const ROSTER_DELAY: Duration = Duration::from_millis(100);

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// A list of `n` untitled groups, the answers to reading each one's roster (each `ROSTER_DELAY` late) and then
    /// `lists_after` more answers of the same list, in the order the requests come.
    fn groups_with_slow_rosters(n: usize, lists_after: usize) -> Vec<Reply> {
        let groups: Vec<Value> = (0..n).map(unnamed_group).collect();
        let all = list(&groups, None);
        let mut script = vec![Reply::status(200).json(&all)];
        script.extend((0..n).map(|_| {
            Reply::status(200)
                .json(&two_members())
                .delayed(ROSTER_DELAY)
        }));
        script.extend((0..lists_after).map(|_| Reply::status(200).json(&all)));
        script
    }

    fn untitled(chats: &[Chat]) -> usize {
        chats
            .iter()
            .filter(|c| c.members.is_empty() && c.title(ME) == "(untitled chat)")
            .count()
    }

    #[test]
    fn an_attached_backend_returns_the_list_at_once_and_reads_the_rosters_after_it() {
        const GROUPS: usize = MEMBER_LOOKUPS_PER_READ;
        let server = serve(groups_with_slow_rosters(GROUPS, 1));
        let b = Arc::new(backend(&server));
        b.attach();

        let started = Instant::now();
        let first = b.chats().unwrap();
        let took = started.elapsed();
        assert!(
            took < ROSTER_DELAY * GROUPS as u32 / 2,
            "the list waited for the rosters: {took:?}"
        );
        assert!(
            lock(&b.members).is_empty(),
            "not one roster answer can have arrived yet: the first is still being delayed"
        );
        // People not known yet is "(untitled chat)", the existing words for that, never "(just you)".
        assert_eq!(untitled(&first), GROUPS, "{:?}", titles(&first));
        assert!(
            b.poll_pushes().is_empty(),
            "nothing to say before the rosters are in"
        );

        wait_until("the rosters", || {
            !b.lookup_running.load(Ordering::SeqCst) && lock(&b.members).len() == GROUPS
        });
        assert!(
            b.poll_pushes().contains(&PushHint::Unknown),
            "the app is told a new read of the list is worth having"
        );
        let second = b.chats().unwrap();
        assert!(
            titles(&second).iter().all(|t| t == "Ada"),
            "{:?}",
            titles(&second)
        );
        assert_eq!(
            server.seen().len(),
            1 + GROUPS + 1,
            "the second read asked for no roster"
        );
        assert!(b.take_warnings().is_empty());
    }

    #[test]
    fn a_backend_that_is_not_attached_reads_the_rosters_in_the_call_as_it_always_did() {
        const GROUPS: usize = MEMBER_LOOKUPS_PER_READ;
        let server = serve(groups_with_slow_rosters(GROUPS, 0));
        let b = backend(&server);
        let started = Instant::now();
        let first = b.chats().unwrap();
        let took = started.elapsed();
        assert!(took >= ROSTER_DELAY * GROUPS as u32, "{took:?}");
        assert!(
            titles(&first).iter().all(|t| t == "Ada"),
            "{:?}",
            titles(&first)
        );
        assert_eq!(server.seen().len(), 1 + GROUPS);
        assert!(
            b.poll_pushes().is_empty(),
            "the caller already has the titles: nothing to tell it"
        );
    }

    #[test]
    fn a_throttled_first_roster_in_the_background_is_the_only_one_asked_for_and_pauses_the_rest() {
        let groups: Vec<Value> = (0..6).map(unnamed_group).collect();
        let all = list(&groups, None);
        let server = serve(vec![
            Reply::status(200).json(&all),
            Reply::status(429).header("Retry-After", "5").json("{}"),
            Reply::status(200).json(&all),
        ]);
        let b = Arc::new(backend(&server));
        b.attach();
        b.chats().unwrap();
        wait_until("the job", || !b.lookup_running.load(Ordering::SeqCst));
        assert_eq!(
            server.seen().len(),
            2,
            "the list and one roster, not the list and a burst"
        );
        assert!(lock(&b.members_paused_until).is_some());
        assert!(
            b.take_warnings().is_empty(),
            "a title is not worth a warning"
        );
        assert!(
            b.poll_pushes().is_empty(),
            "nothing was learned, so nothing to refresh for"
        );
        // While the service has asked for a pause no roster is asked for and no job is started.
        let again = b.chats().unwrap();
        assert_eq!(untitled(&again), 6);
        assert!(!b.lookup_running.load(Ordering::SeqCst));
        assert_eq!(server.seen().len(), 3, "only the list");
    }

    #[test]
    fn a_roster_that_fails_in_the_background_is_a_warning_and_is_not_retried_at_once() {
        let one = list(&[unnamed_group(1)], None);
        let server = serve(vec![
            Reply::status(200).json(&one),
            Reply::status(500).json(r#"{"message":"boom"}"#),
            Reply::status(200).json(&one),
        ]);
        let b = Arc::new(backend(&server));
        b.attach();
        b.chats().unwrap();
        wait_until("the job", || !b.lookup_running.load(Ordering::SeqCst));
        assert!(matches!(
            b.take_warnings().as_slice(),
            [Error::Status { status: 500, .. }]
        ));
        assert!(b.poll_pushes().is_empty());
        assert!(lock(&b.failed_members).contains_key("19:grp1@thread.v2"));
        assert_eq!(untitled(&b.chats().unwrap()), 1);
        assert!(!b.lookup_running.load(Ordering::SeqCst), "not tried again");
        assert_eq!(server.seen().len(), 3);
    }

    #[test]
    fn only_one_roster_job_runs_at_a_time() {
        let server = serve(vec![
            Reply::status(200).json(&list(&[unnamed_group(1)], None)),
            Reply::status(200).json(&list(&[unnamed_group(1)], None)),
            Reply::status(200).json(&two_members()),
        ]);
        let b = Arc::new(backend(&server));
        b.attach();
        // The helper thread is busy (with a roster job, or with a name lookup).
        b.lookup_running.store(true, Ordering::SeqCst);
        assert!(!TeamsBackend::start_roster_job(
            &b,
            vec!["x".into()],
            vec![]
        ));
        let busy = b.chats().unwrap();
        assert_eq!(untitled(&busy), 1);
        assert_eq!(server.seen().len(), 1, "no roster was asked for meanwhile");
        assert!(
            b.lookup_running.load(Ordering::SeqCst),
            "the other job's claim is left alone"
        );

        // Once it is free the next read starts the job.
        b.lookup_running.store(false, Ordering::SeqCst);
        b.chats().unwrap();
        wait_until("the roster", || {
            !b.lookup_running.load(Ordering::SeqCst) && lock(&b.members).len() == 1
        });
        assert_eq!(server.seen().len(), 3);
    }

    #[test]
    fn a_long_list_is_titled_over_several_jobs_each_prompting_the_next_read() {
        // More groups than one job reads: the hint after the first job lets the next read start the second.
        let n = MEMBER_LOOKUPS_PER_READ + 3;
        let groups: Vec<Value> = (0..n).map(unnamed_group).collect();
        let all = list(&groups, None);
        let mut script = vec![Reply::status(200).json(&all)];
        script
            .extend((0..MEMBER_LOOKUPS_PER_READ).map(|_| Reply::status(200).json(&two_members())));
        script.push(Reply::status(200).json(&all));
        script.extend((0..3).map(|_| Reply::status(200).json(&two_members())));
        let server = serve(script);
        let b = Arc::new(backend(&server));
        b.attach();
        b.chats().unwrap();
        wait_until("the first job", || {
            !b.lookup_running.load(Ordering::SeqCst)
                && lock(&b.members).len() == MEMBER_LOOKUPS_PER_READ
        });
        assert!(b.poll_pushes().contains(&PushHint::Unknown));
        // The app's read, prompted by the hint, finds the thread free and starts the next job.
        b.chats().unwrap();
        wait_until("the second job", || {
            !b.lookup_running.load(Ordering::SeqCst) && lock(&b.members).len() == n
        });
        assert!(b.poll_pushes().contains(&PushHint::Unknown));
        assert!(b.take_warnings().is_empty());
    }

    #[test]
    fn reading_rosters_in_the_background_logs_counts_and_no_content() {
        let _serial = capture_logs();
        lock(&LOGGED).clear();
        let group = json!({"id": "19:TOPSECRET-GROUP@thread.v2", "lastMessage": {"messagetype": "Text",
            "content": "TOPSECRET-WORDS", "imdisplayname": "Zaphod Beeblebrox", "originalarrivaltime": "2026-10-27T10:00:00Z"}});
        let roster = json!({"members": [{"id": "8:orgid:77777777-0000-0000-0000-000000000001"}]});
        let server = serve(vec![
            Reply::status(200).json(&list(&[group], None)),
            Reply::status(200).json(&roster.to_string()),
        ]);
        let b = Arc::new(backend(&server));
        b.attach();
        b.chats().unwrap();
        wait_until("the job", || {
            !b.lookup_running.load(Ordering::SeqCst) && lock(&b.members).len() == 1
        });
        let log = lock(&LOGGED).join("\n");
        for expected in [
            "chat titles: reading 1 roster(s) on the helper thread",
            "chat titles: 1 of 1 roster(s) read",
        ] {
            assert!(log.contains(expected), "missing {expected:?} in:\n{log}");
        }
        for content in ["TOPSECRET", "Zaphod", "Beeblebrox", "77777777-0000"] {
            assert!(!log.contains(content), "{content} reached the log:\n{log}");
        }
    }

    #[test]
    fn the_wait_after_lookups_fail_outright_grows_and_a_success_starts_it_over() {
        let server = serve(vec![]);
        let b = backend(&server);
        let wait = |b: &TeamsBackend| {
            b.pause_lookups();
            lock(&b.lookup_paused_until)
                .unwrap()
                .saturating_duration_since(Instant::now())
        };
        let minutes: Vec<u64> = (0..7).map(|_| wait(&b).as_secs().div_ceil(60)).collect();
        assert_eq!(
            minutes,
            [5, 10, 20, 40, 60, 60, 60],
            "doubling, never over an hour"
        );
        b.lookup_failures.store(0, Ordering::Relaxed);
        assert_eq!(wait(&b).as_secs().div_ceil(60), 5);
    }

    #[test]
    fn a_batch_the_service_fails_on_is_split_to_find_the_one_id_it_cannot_look_up() {
        let ids = four_ids();
        let server = serve(vec![
            Reply::status(500).body("timeout"), // all four
            Reply::status(200).json(&profiles_for(&[&ids[0], &ids[1]])), // first half
            Reply::status(500).body("timeout"), // second half
            Reply::status(200).json(&profiles_for(&[&ids[2]])), // third alone
            Reply::status(500).body("timeout"), // fourth alone
        ]);
        let b = backend(&server);
        with_directory(&b, &server);
        let live = b.live(None).unwrap();
        b.resolve_names(&live, &ids, false)
            .expect("the ids the service can look up are not held up by the one it cannot");
        assert_eq!(server.seen().len(), 5, "all four, then the halves");
        for id in &ids[..3] {
            assert_eq!(b.name_of(id), format!("Name {id}"));
        }
        assert!(b.name_of(&ids[3]).is_empty());
        // The failing one is given up on; nothing is asked again, and nothing is paused.
        b.resolve_names(&live, &ids, false).unwrap();
        assert_eq!(server.seen().len(), 5, "no further request");
        assert!(lock(&b.lookup_paused_until).is_none());
    }

    #[test]
    fn when_every_part_fails_it_is_the_service_not_an_id_and_nothing_is_given_up_on() {
        let ids = four_ids();
        // 4 -> 2 + 2 -> 1 + 1 + 1 + 1: seven requests, all refused.
        let server = serve((0..7).map(|_| Reply::status(500).body("timeout")).collect());
        let b = backend(&server);
        with_directory(&b, &server);
        let live = b.live(None).unwrap();
        assert!(b.resolve_names(&live, &ids, false).is_err());
        assert!(
            lock(&b.lookup_paused_until).is_some(),
            "lookups wait before trying again"
        );
        assert!(
            lock(&b.looked_up).is_empty(),
            "no id is written off because the service was down"
        );
    }

    #[test]
    fn a_failed_name_lookup_pauses_lookups_instead_of_repeating_them() {
        // The chat names two people: the lookup for both fails, and so does each on its own.
        let server = serve(vec![
            Reply::status(200).json(&one_to_one_list()),
            Reply::status(500).body("boom"),
            Reply::status(500).body("boom"),
            Reply::status(500).body("boom"),
            Reply::status(200).json(&one_to_one_list()),
        ]);
        let b = backend(&server);
        with_directory(&b, &server);
        assert_eq!(
            b.chats().unwrap().len(),
            1,
            "titles are best effort; the list still comes back"
        );
        assert!(matches!(
            b.take_warnings().as_slice(),
            [Error::Status { status: 500, .. }]
        ));
        b.chats().unwrap();
        assert_eq!(
            server.seen().len(),
            5,
            "no lookup was attempted while paused"
        );
        assert!(b.take_warnings().is_empty());
    }

    /// A favourites property as the service holds it: a JSON map, as a string, of conversation id to order.
    fn favourites_reply(entries: &[(&str, u32)]) -> String {
        let map: serde_json::Map<String, Value> = entries
            .iter()
            .map(|(id, n)| ((*id).to_string(), json!(n)))
            .collect();
        json!({ "favorites": Value::Object(map).to_string(), "cid": "x" }).to_string()
    }

    fn group(id: &str, topic: &str, at: &str) -> Value {
        json!({"id": id, "threadProperties": {"topic": topic},
            "lastMessage": {"messagetype": "Text", "content": "x", "originalarrivaltime": at}})
    }

    #[test]
    fn favourites_are_marked_and_one_older_than_the_page_is_fetched_by_itself() {
        let list = json!({"conversations": [
            group("19:new@thread.v2", "New", "2026-10-27T12:00:00Z"),
            group("19:fav@thread.v2", "Fav", "2026-10-27T11:00:00Z"),
        ]});
        let old = group("19:old@thread.v2", "Old favourite", "2020-01-01T00:00:00Z");
        let server = serve(vec![
            Reply::status(200).json(&list.to_string()),
            Reply::status(200).json(&favourites_reply(&[
                ("19:old@thread.v2", 1),
                ("19:fav@thread.v2", 2),
                ("19:gone@teams", 3),
            ])),
            // The missing ones are read in the order of their ids: `gone` first.
            Reply::status(404).json(r#"{"message":"gone"}"#),
            Reply::status(200).json(&old.to_string()),
        ]);
        let b = backend(&server).with_favourites(true);
        let chats = b.chats().unwrap();
        let order: Vec<(String, Option<u32>)> =
            chats.iter().map(|c| (c.title(ME), c.favorite)).collect();
        assert_eq!(
            order,
            [
                ("New".to_string(), None),
                ("Fav".to_string(), Some(2)),
                ("Old favourite".to_string(), Some(1))
            ],
            "newest first; the order the user gave is on each favourite"
        );
        let seen = server.seen();
        assert!(
            seen[1]
                .path
                .ends_with("/v1/users/ME/properties?name=favorites"),
            "{}",
            seen[1].path
        );
        assert!(
            seen[3]
                .path
                .contains("/conversations/19%3Aold@thread.v2?view=msnp24Equivalent"),
            "{}",
            seen[3].path
        );
        assert!(
            b.take_warnings().is_empty(),
            "a favourite that cannot be read is not worth a warning"
        );
    }

    #[test]
    fn a_favourite_older_than_the_page_is_not_re_fetched_until_the_next_full_refresh() {
        let list = json!({"conversations": [
            group("19:new@thread.v2", "New", "2026-10-27T12:00:00Z"),
        ]});
        let old = group("19:old@thread.v2", "Old favourite", "2020-01-01T00:00:00Z");
        let fav = favourites_reply(&[("19:old@thread.v2", 1)]);
        let server = serve(vec![
            Reply::status(200).json(&list.to_string()),
            Reply::status(200).json(&fav),
            Reply::status(200).json(&old.to_string()),
            // Second refresh, well within full_refresh: no third request.
            Reply::status(200).json(&list.to_string()),
            Reply::status(200).json(&fav),
            // Third refresh, after full_refresh has been made to have passed: fetched again.
            Reply::status(200).json(&list.to_string()),
            Reply::status(200).json(&fav),
            Reply::status(200).json(&old.to_string()),
        ]);
        let mut b = backend(&server).with_favourites(true);
        assert_eq!(b.chats().unwrap().len(), 2, "New, and the cached Old");
        assert_eq!(server.seen().len(), 3);
        assert_eq!(
            b.chats().unwrap().len(),
            2,
            "still cached, nothing re-fetched"
        );
        assert_eq!(
            server.seen().len(),
            5,
            "no request for the favourite's own conversation this time"
        );
        b.full_refresh = Duration::ZERO;
        assert_eq!(b.chats().unwrap().len(), 2);
        assert_eq!(server.seen().len(), 8, "fetched again once stale");
    }

    /// A backend that can reach the chat service aggregator at `server`, with a token for it.
    fn backend_with_folders(server: &Server) -> TeamsBackend {
        let b = backend(server).with_favourites(true);
        if let Some(live) = lock(&b.live).as_mut() {
            live.csa_base = format!("{}/api/csa/amer", server.origin());
        }
        *lock(&b.csa) = Some(("csa-token".into(), now() + 3600));
        b
    }

    fn folders_reply(favourites: &[(&str, &str)]) -> String {
        let items: Vec<Value> = favourites
            .iter()
            .map(|(id, kind)| json!({"conversationId": id, "threadType": kind}))
            .collect();
        json!({"conversationFolders": [
            {"folderType": "RecentChats", "isDeleted": false, "conversationFolderItems": []},
            {"folderType": "Favorites", "isDeleted": false, "conversationFolderItems": items},
        ], "conversationFolderOrder": []})
        .to_string()
    }

    #[test]
    fn favourites_come_from_the_favorites_folder_in_its_order_without_teams_and_channels() {
        let list = json!({"conversations": [
            group("19:new@thread.v2", "New", "2026-10-27T12:00:00Z"),
            group("19:fav@thread.v2", "Fav", "2026-10-27T11:00:00Z"),
        ]});
        let old = group("19:old@thread.v2", "Old favourite", "2020-01-01T00:00:00Z");
        let server = serve(vec![
            Reply::status(200).json(&list.to_string()),
            Reply::status(200).json(&folders_reply(&[
                ("19:old@thread.v2", "chat"),
                ("19:channel@thread.tacv2", "space"),
                ("19:topic@thread.tacv2", "topic"),
                ("19:fav@thread.v2", "chat"),
            ])),
            Reply::status(200).json(&old.to_string()),
        ]);
        let chats = backend_with_folders(&server).chats().unwrap();
        let order: Vec<(String, Option<u32>)> =
            chats.iter().map(|c| (c.title(ME), c.favorite)).collect();
        assert_eq!(
            order,
            [
                ("New".to_string(), None),
                ("Fav".to_string(), Some(2)),
                ("Old favourite".to_string(), Some(1))
            ],
            "channels take no place in the order"
        );
        let seen = server.seen();
        assert_eq!(
            seen.len(),
            3,
            "no request for a channel or the old property"
        );
        assert!(
            seen[1]
                .path
                .contains("/api/csa/amer/api/v1/teams/users/me/conversationFolders"),
            "{}",
            seen[1].path
        );
        assert_eq!(seen[1].header("authorization"), Some("Bearer csa-token"));
        assert_eq!(
            seen[0].header("authorization"),
            Some("Bearer spaces-token"),
            "the chat service keeps its own token"
        );
    }

    #[test]
    fn favourites_fall_back_to_the_older_property_when_the_account_has_no_folder_to_read() {
        let list = json!({"conversations": [group("19:a@thread.v2", "A", "2026-10-27T12:00:00Z")]});
        let property = favourites_reply(&[("19:a@thread.v2", 1)]);
        // The aggregator refusing the account outright, and an answer that has no Favorites folder in it, both say
        // this account has no folder to read.
        for folder in [
            Reply::status(404).json(r#"{"message":"no such service"}"#),
            Reply::status(200).json(r#"{"conversationFolders": []}"#),
        ] {
            let server = serve(vec![
                Reply::status(200).json(&list.to_string()),
                folder,
                Reply::status(200).json(&property),
            ]);
            let b = backend_with_folders(&server);
            let chats = b.chats().unwrap();
            assert_eq!(chats[0].favorite, Some(1));
            assert!(b.take_warnings().is_empty());
            assert!(
                server.seen()[2]
                    .path
                    .ends_with("/v1/users/ME/properties?name=favorites")
            );
        }
    }

    #[test]
    fn a_folder_that_cannot_be_read_just_now_is_not_taken_for_one_that_does_not_exist() {
        let list = json!({"conversations": [group("19:a@thread.v2", "A", "2026-10-27T12:00:00Z")]});
        let server = serve(vec![
            Reply::status(200).json(&list.to_string()),
            Reply::status(200).json(&folders_reply(&[("19:a@thread.v2", "chat")])),
            // Next refresh: the aggregator has a bad moment.
            Reply::status(200).json(&list.to_string()),
            Reply::status(500).body("down"),
        ]);
        let b = backend_with_folders(&server);
        assert_eq!(b.chats().unwrap()[0].favorite, Some(1));
        assert!(!b.can_favorite(), "known to live in the folder");
        let chats = b.chats().unwrap();
        assert_eq!(
            chats[0].favorite,
            Some(1),
            "the list keeps the favourites the last read found"
        );
        assert!(
            !b.can_favorite(),
            "and the account is still known to keep them in the folder"
        );
        // The property was never asked, and nothing can be written on the strength of a remembered list.
        assert!(matches!(
            b.set_favorite("19:b@thread.v2", true),
            Err(Error::Unsupported { .. })
        ));
        assert!(
            server
                .seen()
                .iter()
                .all(|s| s.method == "GET" && !s.path.contains("properties?name=favorites")),
            "{:?}",
            server
                .seen()
                .iter()
                .map(|s| s.path.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_first_read_of_the_folder_that_fails_for_the_moment_shows_no_favourites_not_the_old_property()
     {
        let list = json!({"conversations": [group("19:a@thread.v2", "A", "2026-10-27T12:00:00Z")]});
        let server = serve(vec![
            Reply::status(200).json(&list.to_string()),
            Reply::status(500).body("down"),
        ]);
        let b = backend_with_folders(&server);
        let chats = b.chats().unwrap();
        assert_eq!(chats[0].favorite, None);
        assert_eq!(server.seen().len(), 2, "no request for the older property");
        assert!(b.can_favorite(), "nothing is known about the account yet");
    }

    #[test]
    fn favourites_kept_in_a_folder_are_not_written_to_the_old_property() {
        let server = serve(vec![
            Reply::status(200).json(&folders_reply(&[("48:notes", "streamofnotes")])),
        ]);
        let b = backend_with_folders(&server);
        assert!(matches!(
            b.set_favorite("19:a@thread.v2", true),
            Err(Error::Unsupported { .. })
        ));
        assert!(
            server.seen().iter().all(|s| s.method == "GET"),
            "nothing was written"
        );
    }

    #[test]
    fn a_folder_sourced_account_is_remembered_so_can_favorite_and_a_later_set_favorite_need_no_request()
     {
        let server = serve(vec![
            Reply::status(200).json(&folders_reply(&[("48:notes", "streamofnotes")])),
        ]);
        let b = backend_with_folders(&server);
        assert!(b.can_favorite(), "unknown yet, so optimistically true");
        let live = b.live(None).unwrap();
        b.favourites(&live).unwrap();
        assert!(
            !b.can_favorite(),
            "now known to be folder-sourced, from the one read above"
        );
        assert!(matches!(
            b.set_favorite("19:a@thread.v2", true),
            Err(Error::Unsupported { .. })
        ));
        assert_eq!(
            server.seen().len(),
            1,
            "set_favorite failed from the cached source, without reading the folder again"
        );
    }

    #[test]
    fn favourites_are_kept_when_the_list_is_cut_to_its_cap_and_do_not_count_towards_it() {
        let list = json!({"conversations": [
            group("19:a@thread.v2", "A", "2026-10-27T12:00:00Z"),
            group("19:b@thread.v2", "B", "2026-10-27T11:00:00Z"),
            group("19:c@thread.v2", "C", "2026-10-27T10:00:00Z"),
            group("19:d@thread.v2", "D", "2026-10-27T09:00:00Z"),
        ]});
        let server = serve(vec![
            Reply::status(200).json(&list.to_string()),
            Reply::status(200).json(&favourites_reply(&[("19:d@thread.v2", 1)])),
        ]);
        let chats = backend(&server)
            .with_favourites(true)
            .with_max_chats(2)
            .chats()
            .unwrap();
        let titles: Vec<String> = chats.iter().map(|c| c.title(ME)).collect();
        assert_eq!(titles, ["A", "B", "D"], "two others and the favourite");
    }

    #[test]
    fn a_failure_to_read_the_favourites_leaves_the_list_as_it_was() {
        let list = json!({"conversations": [group("19:a@thread.v2", "A", "2026-10-27T12:00:00Z")]});
        let server = serve(vec![
            Reply::status(200).json(&list.to_string()),
            Reply::status(500).body("down"),
        ]);
        let b = backend(&server).with_favourites(true);
        let chats = b.chats().unwrap();
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].favorite, None);
        assert!(b.take_warnings().is_empty());
    }

    #[test]
    fn a_favourite_is_added_at_the_end_and_taken_out_leaving_the_others_as_they_are() {
        let before = favourites_reply(&[("48:notes", 1), ("19:x@unq.gbl.spaces", 2)]);
        let server = serve(vec![
            Reply::status(200).json(&before),
            Reply::status(200).body("null"),
            Reply::status(200).json(&before),
            Reply::status(200).body("null"),
        ]);
        let b = backend(&server);
        assert!(b.can_favorite());
        b.set_favorite("19:new@thread.v2", true).unwrap();
        b.set_favorite("48:notes", false).unwrap();
        let seen = server.seen();
        assert_eq!(seen[1].method, "PUT");
        assert!(
            seen[1]
                .path
                .ends_with("/v1/users/ME/properties?name=favorites")
        );
        let put = |i: usize| -> Value {
            let body: Value = serde_json::from_str(&seen[i].body).unwrap();
            serde_json::from_str(body["favorites"].as_str().expect("the value is a string"))
                .unwrap()
        };
        assert_eq!(
            put(1),
            json!({"48:notes": 1, "19:x@unq.gbl.spaces": 2, "19:new@thread.v2": 3})
        );
        assert_eq!(put(3), json!({"19:x@unq.gbl.spaces": 2}));
    }

    #[test]
    fn favouriting_what_is_one_already_or_unfavouriting_what_is_not_writes_nothing() {
        let current = favourites_reply(&[("48:notes", 1)]);
        let server = serve(vec![
            Reply::status(200).json(&current),
            Reply::status(200).json(&current),
        ]);
        let b = backend(&server);
        b.set_favorite("48:notes", true).unwrap();
        b.set_favorite("19:other@thread.v2", false).unwrap();
        assert!(server.seen().iter().all(|s| s.method == "GET"));
    }

    #[test]
    fn a_favourites_list_that_cannot_be_read_is_never_overwritten_and_odd_ids_are_refused() {
        let server = serve(vec![
            Reply::status(200).json(r#"{"favorites": "this is not a map"}"#),
            Reply::status(500).body("down"),
        ]);
        let b = backend(&server);
        assert!(matches!(
            b.set_favorite("19:a@thread.v2", true),
            Err(Error::Unexpected { .. })
        ));
        assert!(b.set_favorite("19:a@thread.v2", true).is_err());
        assert!(
            server.seen().iter().all(|s| s.method == "GET"),
            "nothing was written"
        );
        for id in ["", "bad\u{1b}id", &"x".repeat(300)] {
            assert!(
                matches!(b.set_favorite(id, true), Err(Error::InvalidInput { .. })),
                "{id:?}"
            );
        }
        assert_eq!(server.seen().len(), 2, "refused before any request");
    }
    #[test]
    fn the_list_size_and_client_version_are_configurable_within_the_services_limits() {
        let server = serve(vec![
            Reply::status(200).json(r#"{"conversations": []}"#),
            Reply::status(200).json(r#"{"conversations": []}"#),
        ]);
        backend(&server)
            .with_max_chats(25)
            .with_client_version("9/9.9")
            .chats()
            .unwrap();
        let first = &server.seen()[0];
        assert!(first.path.contains("pageSize=25"), "{}", first.path);
        assert_eq!(first.header("x-ms-client-version"), Some("9/9.9"));
        backend(&server).with_max_chats(10_000).chats().unwrap();
        assert!(
            server.seen()[1].path.contains("pageSize=500"),
            "the service has been seen to serve 500 and refuses 1000: {}",
            server.seen()[1].path
        );
        // Unless told, a few hundred.
        let server = serve(vec![Reply::status(200).json(r#"{"conversations": []}"#)]);
        backend(&server).chats().unwrap();
        assert!(
            server.seen()[0].path.contains("pageSize=300"),
            "{}",
            server.seen()[0].path
        );
    }

    #[test]
    fn a_recorder_sees_every_exchange_masked_and_its_own_failures_become_warnings() {
        let dir = std::env::temp_dir().join(format!(
            "teams-client-backend-shapes-{}",
            std::process::id()
        ));
        let list = json!({"conversations": [{
            "id": "19:grp@thread.v2", "threadProperties": {"topic": "Quarterly numbers"},
            "lastMessage": {"messagetype": "Text", "content": "private words here", "originalarrivaltime": "2026-10-27T10:00:00Z"}
        }]});
        let server = serve(vec![
            Reply::status(200).json(&list.to_string()),
            Reply::status(200).json(&list.to_string()),
        ]);
        let recorder = Arc::new(ShapeRecorder::new(&dir).unwrap());
        let b = backend(&server).with_recorder(Arc::clone(&recorder));
        b.chats().unwrap();
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        let text = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(
            text.contains("\"status\": 200") && text.contains("lastMessage"),
            "{text}"
        );
        for secret in [
            "private",
            "Quarterly",
            "skype-token",
            "spaces-token",
            "grp@thread",
        ] {
            assert!(!text.contains(secret), "{secret} leaked into {text}");
        }
        // Recording never fails a call; a recorder that cannot write is reported as a warning.
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, "a file where the directory should be").unwrap();
        b.chats().unwrap();
        assert!(matches!(b.take_warnings().as_slice(), [Error::Store(_)]));
        let _ = std::fs::remove_file(&dir);
    }

    /// `{origin}` in a reply body is replaced with the test server's own address (`testserver::serve`); used here
    /// so the registration answer can name the loopback server as the regional host for the next two requests.
    fn trouter_registration_reply() -> Reply {
        Reply::status(200).json(
            &json!({
                "socketio": "{origin}/",
                "surl": "{origin}/v4/f/x/",
                "registrarUrl": "{origin}/registrar",
                "connectparams": {"sr": "x"},
                "ccid": "abc",
            })
            .to_string(),
        )
    }

    #[test]
    fn registering_the_push_channel_sends_only_the_skypetoken_and_reads_the_regional_host() {
        let server = serve(vec![trouter_registration_reply()]);
        let mut b = backend(&server);
        b.trouter_host = format!("{}/", server.origin());
        trouter::register(
            &b.api,
            None,
            &b.trouter_host,
            "test-endpoint",
            "skype-token",
        )
        .unwrap();
        let seen = server.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "POST");
        assert!(seen[0].path.starts_with("/v4/a?epid="), "{}", seen[0].path);
        assert_eq!(seen[0].header("x-skypetoken"), Some("skype-token"));
        assert_eq!(seen[0].body, "", "an empty body, not the literal 'null'");
    }

    #[test]
    fn registering_refuses_a_socketio_host_the_answer_should_not_be_able_to_name() {
        let server = serve(vec![
            Reply::status(200).json(
                &json!({
                    "socketio": "https://evil.example/",
                    "surl": "{origin}/v4/f/x/",
                    "connectparams": {},
                    "ccid": "",
                })
                .to_string(),
            ),
        ]);
        let mut b = backend(&server);
        b.trouter_host = format!("{}/", server.origin());
        assert!(matches!(
            trouter::register(
                &b.api,
                None,
                &b.trouter_host,
                "test-endpoint",
                "skype-token"
            ),
            Err(Error::Unexpected { .. })
        ));
    }

    #[test]
    fn the_handshake_sends_a_non_empty_tc_and_reads_back_a_session_id() {
        let server = serve(vec![
            trouter_registration_reply(),
            Reply::status(200).body("01a1ae8bb2a2709191d44b963cf8e9ed:40:40:websocket"),
        ]);
        let mut b = backend(&server);
        b.trouter_host = format!("{}/", server.origin());
        let reg = trouter::register(
            &b.api,
            None,
            &b.trouter_host,
            "test-endpoint",
            "skype-token",
        )
        .unwrap();
        let sid = trouter::handshake(&b.api, &reg, "test-endpoint", "skype-token").unwrap();
        assert_eq!(sid, "01a1ae8bb2a2709191d44b963cf8e9ed");
        let seen = server.seen();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].method, "GET");
        assert!(
            seen[1].path.contains("tc=%7B%22cv%22%3A%222024.23.01.2%22"),
            "{}",
            seen[1].path
        );
        assert!(
            seen[1].path.contains("sr=x"),
            "connectparams were copied into the query: {}",
            seen[1].path
        );
        assert_eq!(seen[1].header("x-skypetoken"), Some("skype-token"));
    }
}
