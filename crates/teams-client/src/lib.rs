//! A blocking client for Microsoft Teams chats.
//!
//! Two backends implement the same [`Backend`] trait, so an application can switch between them without caring
//! which service it is talking to:
//!
//! * `graph`: Microsoft Graph with delegated permissions and the device-code sign-in flow. Documented and stable,
//!   but needs an Azure app registration the tenant allows.
//! * `teams`: the web API the Teams web app itself uses. **Unofficial and unsanctioned** (see the disclaimer in the
//!   README): Microsoft has not published it and does not support it, this client presents itself to it as Microsoft's own
//!   web client, and using it may breach Microsoft's terms and an organisation's policies and get the account restricted.
//!   It is here for interest and research, is not recommended, is used entirely at the user's own risk, and may change or
//!   stop working without notice. Use `graph`, the official API, for anything that matters. It builds on the
//!   [msteams-mcp](https://github.com/m0nkmaster/msteams-mcp) project (MIT).
//!
//! The crate never decides where credentials live, takes settings from the environment, or prints anything (finding a browser for the Teams sign-in looks at `PATH` and the standard install folders): the application hands
//! each backend a [`CredentialStore`] ([`FileStore`], [`MemoryStore`], or its own) and receives failures as typed
//! [`Error`]s. Text from the network is sanitised of terminal control characters before it reaches the caller.
//!
//! # Cargo features
//!
//! | Feature | Default | Adds |
//! |---|---|---|
//! | `graph` | yes | the `graph` module (device-code sign-in, Graph chats) |
//! | `teams` | yes | the `teams` module (browser sign-in over DevTools, the Teams web API) |
//!
//! The model, [`Error`], the credential stores and the text helpers are always available, so a crate can implement
//! [`Backend`] itself without pulling in either service. The examples below need both features.
//!
//! # Logging
//!
//! The crate reports what it does through the [`log`](https://docs.rs/log) facade and installs no logger, so it prints
//! nothing unless the application does. Records are made for each request (operation, method, host, status, time, size
//! and, for the Teams backend, whether a read was incremental), credential renewals and warnings. They never contain
//! tokens, message text, names, URLs with their query, or whole identifiers (see [`id_tag`]).
//!
//! # Seeing what a service really sends
//!
//! Attach a `ShapeRecorder` with `with_recorder` and every API exchange is written to a directory as its status plus
//! the JSON structure with all content masked (letters, digits and values reduced to character classes, secrets
//! redacted). The files show formats and field names without recording anything personal, which is what is needed to
//! confirm assumptions about a service without sharing a conversation.
//!
//! # Threading, timeouts and limits
//!
//! * All calls block, so run them on a worker thread when there is a UI to keep responsive. Backends are
//!   `Send + Sync`; concurrent calls are fine, and credential renewal is serialised internally.
//! * Requests time out after 30 s (20 s for image downloads). Image downloads are capped at 10 MB and decoded within
//!   fixed dimension (8192 px) and memory (128 MiB) limits, kept whole under 1.7 million pixels and 2400 px on a
//!   side, else scaled down to fit both by area.
//! * The chat list covers recent activity only: 100 chats on Graph and 300 on the Teams web API by default
//!   (`with_max_chats` on each backend changes it; the Teams web API has been seen to serve 500 per request and
//!   refuses 1000). Favourites (Teams web) are listed whatever their age, on top of that number.
//! * The Teams web backend reads changes only after a chat's or the list's first read and gives back the same whole
//!   page or list a full read would (see `TeamsBackend::with_incremental_reads`); it reads everything again every five
//!   minutes.
//! * HTTP 429 comes back as [`Error::Throttled`] with the service's `Retry-After`; the crate does not retry by
//!   itself, so callers should back off (see [`Error::is_transient`]).
//!
//! # Stability
//!
//! The crate is pre-1.0: minor versions may change the public API. Record types such as [`Chat`] and [`Msg`] have
//! public fields and may gain fields; error enums are `#[non_exhaustive]`.
//!
//! # Example: Microsoft Graph
//!
//! ```no_run
//! use std::sync::Arc;
//! use teams_client::graph::{GraphAuth, GraphBackend, GraphConfig, GraphTokens};
//! use teams_client::{Backend, FileStore};
//!
//! # fn main() -> teams_client::Result<()> {
//! let store = Arc::new(FileStore::new("graph-token.json"));
//! let auth = GraphAuth::new(GraphConfig::default());
//! let tokens = match auth.cached(store.as_ref())? {
//!     Some(tokens) => tokens,
//!     None => {
//!         let tokens = auth.device_login(&|prompt| println!("Open {} and enter {}", prompt.verification_uri, prompt.user_code))?;
//!         tokens.save(store.as_ref())?;
//!         tokens
//!     }
//! };
//! let backend = GraphBackend::new(auth, tokens, store);
//! println!("signed in as {}", backend.me()?.name);
//! # Ok(())
//! # }
//! ```
//!
//! # Example: Teams web API
//!
//! ```no_run
//! use std::sync::Arc;
//! use teams_client::teams::{self, BrowserOptions, TeamsBackend, TeamsSession};
//! use teams_client::{Backend, FileStore};
//!
//! # fn main() -> teams_client::Result<()> {
//! let store = Arc::new(FileStore::new("teams-session.json"));
//! let browser = BrowserOptions::new("browser-profile");
//! let session = match TeamsSession::load(store.as_ref())? {
//!     Some(session) => session,
//!     None => teams::sign_in(&browser, store.as_ref(), &|| println!("A browser window will open; sign in to Teams."))?,
//! };
//! let backend = TeamsBackend::new(session, store, browser);
//! let me = backend.me()?;
//! for chat in backend.chats()? {
//!     println!("{}", chat.title(&me.id));
//! }
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]
// A library never prints: results come back as values, problems as errors or warnings.
#![warn(clippy::print_stdout, clippy::print_stderr)]

#[cfg(any(feature = "graph", feature = "teams"))]
mod emoji;
mod error;
#[cfg(feature = "graph")]
pub mod graph;
#[cfg(any(feature = "graph", feature = "teams"))]
mod headerlog;
#[cfg(any(feature = "graph", feature = "teams"))]
mod http;
mod model;
#[cfg(any(feature = "graph", feature = "teams"))]
mod record;
#[cfg(any(feature = "graph", feature = "teams"))]
mod shares;
mod store;
#[cfg(feature = "teams")]
pub mod teams;
#[cfg(test)]
mod testdir;
#[cfg(all(test, any(feature = "graph", feature = "teams")))]
mod testserver;
mod text;
#[cfg(any(feature = "graph", feature = "teams"))]
mod watch;

#[cfg(any(feature = "graph", feature = "teams"))]
pub use emoji::{canonical as emoji_key, same_emoji};
pub use error::{AuthError, BrowserError, Error, Result, StoreError};
#[cfg(any(feature = "graph", feature = "teams"))]
pub use headerlog::HeaderLog;
pub use model::{
    Attachment, Availability, Backend, Channel, ChannelActivity, ChannelKind, ChannelPost, Chat,
    ChatKind, CodeBlockStyle, Contact, Delivery, EmojiInfo, MAX_SEARCH_CONVERSATIONS, Me, Member,
    Msg, OutMention, Page, Presence, Preview, PushHint, Reaction, ReplyTarget, SearchHit,
    SearchPage, StatusNote, Team,
};
#[cfg(any(feature = "graph", feature = "teams"))]
pub use record::ShapeRecorder;
pub use store::{CredentialStore, FileStore, MemoryStore, create_private_dir};
pub use text::{clean, html_to_plain, id_tag, preview_text};

// Types from these crates appear in this crate's public API (`DateTime<Utc>` in the model, `RgbImage` from
// `Backend::image`), so they are re-exported: depend on these paths and you always use the same versions we do.
pub use chrono;
pub use image;

#[cfg(any(feature = "graph", feature = "teams"))]
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Seconds since the Unix epoch.
#[cfg(any(feature = "graph", feature = "teams"))]
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Lock a mutex, recovering from poisoning. Every mutex in this crate guards a cache or a credential that is replaced
/// wholesale, so the data is valid at every point and a thread that panicked while holding the lock cannot have left
/// it half-updated.
#[cfg(any(feature = "graph", feature = "teams"))]
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
