//! Application coordination, background work and event routing.

mod cache;
mod chat;
mod chats;
mod edit;
mod favorites;
mod gfx;
mod images;
mod input;
mod mention;
mod notify;
mod post;
mod presence;
mod reactions;
mod read;
mod reply;
mod roster;
mod search;
mod sign_in;
mod status;
mod teams;

use crate::bell::Bell;
pub use chat::ChatState;
use chat::Fetch;
use chats::ChatList;
use edit::Editing;
use gfx::Gfx;
use images::Images;
use presence::Presences;
use reactions::Reactions;
use read::ReadState;
use reply::Replying;
pub use search::{SearchRow, SearchScope, find_matches, search_list_rows, search_time};
pub use sign_in::SignIn;
use status::Own;
pub use teams::{Opened, RowKey, TeamRow, Teams, favorite_count, favorite_label};

use crate::cadence;
use crate::clipboard::{self, Clipboard};
use crate::config;
use crate::convert;
use crate::editor::{Editor, Keymap};
use crate::imgart::ImgMode;
use crate::opener::{self, Opener};
use crate::render::{Ctx, ImgState};
use crate::view::{Tab, View};
use crate::workers::{Job, Lane, Workers};
use chrono::Utc;
use image::RgbImage;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use teams_client::{
    Backend, ChannelPost, Chat, Contact, Delivery, EmojiInfo, Error, Me, Msg, Page, PushHint,
};

/// Emoji per row in the emoji selector.
pub const EMOJI_COLUMNS: usize = 10;
/// The most rows of the emoji grid shown at once, and so how far PgUp/PgDn move.
pub const EMOJI_ROWS: usize = 8;
/// Longest search typed into the emoji selector.
const MAX_EMOJI_QUERY: usize = 32;
const OPEN_DWELL: Duration = Duration::from_millis(150);
/// The least time between two steps of the animation clock (`App::tick`). The loop wakes every 60 ms when idle, and sooner
/// when a key arrives or a picture is being encoded; the clock does not run faster for that.
const TICK_EVERY: Duration = Duration::from_millis(50);
/// The header's gradient moves once for this many ticks (240 ms: about four times a second at most).
const HEADER_EVERY: usize = 4;
/// How often to retry [`App::fetch_me`] while it has not yet succeeded; `Schedule::due` backs this off on repeated
/// failures the same way the chat list and message polls do.
const ME_RETRY: Duration = Duration::from_secs(20);
/// How long the service asked us to wait, when `e` is a throttled answer (a missing hint counts as a default wait).
fn throttle_hold(e: &Error) -> Option<Duration> {
    match e {
        Error::Throttled { retry_after, .. } => {
            Some(retry_after.unwrap_or(cadence::THROTTLED_WAIT))
        }
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Chats,
    Messages,
    Editor,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Overlay {
    None,
    /// The key reference, scrolled down by this many lines.
    Help(usize),
    Links(Vec<String>),
    /// Directory search results to choose from (`:new`).
    People {
        people: Vec<Contact>,
        sel: usize,
    },
    /// The emoji selector: type to search, arrows to move, Enter to choose.
    Emoji {
        target: EmojiTarget,
        query: String,
        sel: usize,
    },
    /// Choose your own status: the entries of `status::CHOICES`, `sel` the one under the cursor.
    Status {
        sel: usize,
    },
    /// Search the messages of the open chat; what is typed, found and selected is `App::search` (`app/search.rs`).
    Search,
    /// The sign-in has expired (or is being renewed): what happened, and buttons to sign in again, copy the details
    /// or close. It opens by itself, so keys are ignored for a moment after `since`; someone typing a message must not
    /// have the next letters taken as answers.
    SignIn {
        since: Instant,
    },
}

/// What choosing an emoji in the selector does.
#[derive(Clone, PartialEq, Debug)]
pub enum EmojiTarget {
    /// Put it in the message box, at the cursor.
    Insert,
    /// React to a message with it; choosing one the user already reacted with takes the reaction back.
    React {
        chat_id: String,
        message_id: String,
        /// The emoji the user has already reacted with on that message.
        mine: Vec<String>,
    },
}

/// What a background job reports back. Errors stay typed here; they become text only when shown (see [`describe`]).
pub enum Ev {
    Me(Result<Me, Error>),
    Chats(Result<Vec<Chat>, Error>),
    People {
        query: String,
        result: Result<Vec<Contact>, Error>,
    },
    Opened(Result<Chat, Error>),
    /// The emoji the service accepts as reactions.
    ReactionEmoji(Result<Vec<EmojiInfo>, Error>),
    /// The teams the person is in, with their channels. A side call: never read as a lost sign-in.
    Teams(Result<Vec<teams_client::Team>, Error>),
    Messages {
        chat_id: String,
        fetch: Fetch,
        result: Result<Page, Error>,
    },
    Sent {
        chat_id: String,
        local_id: String,
        result: Result<Msg, Error>,
    },
    Image {
        url: String,
        result: Result<RgbImage, Error>,
    },
    /// A picture was encoded for the terminal's graphics protocol, or could not be (as text for the log).
    Encoded {
        key: gfx::EncodeKey,
        result: Result<Arc<crate::gfx::encode::Encoded>, String>,
    },
    /// The service was told a chat is read up to a message.
    MarkedRead {
        chat_id: String,
        result: Result<(), Error>,
    },
    /// A chat was added to the favourites or taken out (`on`); `previous` is the place it had before, to put back if
    /// the service refuses.
    Favorited {
        chat_id: String,
        on: bool,
        previous: Option<u32>,
        result: Result<(), Error>,
    },
    /// Your edit of a message was saved, or refused.
    Edited {
        chat_id: String,
        message_id: String,
        /// The HTML that was sent, shown at once.
        html: String,
        /// Which attempt this is, so a confirmation for one cancelled locally and then redone is known to be stale.
        token: u64,
        result: Result<(), Error>,
    },
    /// How available the people asked about are (a person the service says nothing about is left out).
    Presence {
        asked: Vec<String>,
        result: Result<std::collections::HashMap<String, teams_client::Presence>, Error>,
    },
    /// Who is in a group or meeting chat whose list entry did not say.
    Roster {
        chat_id: String,
        result: Result<Vec<teams_client::Member>, Error>,
    },
    /// The service was told whether you are at the keyboard.
    Reported {
        active: bool,
        result: Result<(), Error>,
    },
    /// Your own status was set (`Some`) or given up (`None`).
    StatusSet {
        choice: Option<teams_client::Availability>,
        result: Result<(), Error>,
    },
    /// A page of the service's search results for `query` in `chat` (or, in the team scope, in the channels of its
    /// team), starting at `from`. `seq` says which search of the popup it answers: one that was typed over, closed,
    /// switched to the other scope or left behind by switching chats is not wanted any more.
    Searched {
        chat: String,
        scope: SearchScope,
        query: String,
        from: usize,
        seq: u64,
        result: Result<teams_client::SearchPage, Error>,
    },
    /// A reaction was added (`on`) or taken back.
    Reacted {
        chat_id: String,
        message_id: String,
        emoji: String,
        on: bool,
        result: Result<(), Error>,
    },
    /// A problem the backend wants the user to hear about that did not fail an operation.
    Warning(Error),
    /// The outcome of something done off the UI thread that is not a backend call (opening a browser).
    Notice {
        text: String,
        error: bool,
    },
    /// The browser sign-in that was started to replace an expired one ended.
    SignedIn(Result<(), Error>),
    /// The start-up cache was written (its size), or could not be. Only logged: nobody asked for it.
    CacheWritten(Result<usize, String>),
}

impl Ev {
    /// The error of a call that can say the whole sign-in is gone: the ones that use the main chat credentials.
    /// A call that needs its own, separately granted permission (a picture from a shared drive, the people search, the
    /// favourites folder, the reaction list) can be refused on its own while ordinary chat reading still works, and
    /// must not be read as the session ending; the library says the same for its token requests.
    fn sign_in_error(&self) -> Option<&Error> {
        match self {
            Ev::Me(Err(e))
            | Ev::Chats(Err(e))
            | Ev::Opened(Err(e))
            | Ev::Messages { result: Err(e), .. }
            | Ev::Sent { result: Err(e), .. }
            | Ev::Edited { result: Err(e), .. } => Some(e),
            _ => None,
        }
    }
}

/// How a backend error reads when starting up or signing in: the library's message, plus what to do when the cure is
/// to sign in again.
pub fn describe(e: &Error) -> String {
    if e.needs_sign_in() {
        format!("{e}; run `vtteams logout`, then start vtteams to sign in again")
    } else {
        e.to_string()
    }
}

/// How a backend error reads once the program is running. A lost sign-in is short here, because the popup has the
/// whole story and F6 the cure (for a service that cannot sign in again from inside, F6 says what to do).
pub fn describe_running(e: &Error) -> String {
    if e.needs_sign_in() {
        "signed out: press F6 to sign in again".to_string()
    } else {
        e.to_string()
    }
}

/// How the failure of a call that does not decide whether the sign-in is intact reads (see [`Ev::sign_in_error`]): the
/// library's message as it is, never "signed out".
pub fn describe_side(e: &Error) -> String {
    e.to_string()
}

/// What the app uses of the machine around it, so tests can substitute all of it.
struct Desktop {
    clipboard: Arc<dyn Clipboard>,
    opener: Arc<dyn Opener>,
    image_dir: Result<PathBuf, String>,
    open_dwell: Duration,
}

/// The stand-ins an app built by [`App::for_test`] uses, for a test to read and fill.
#[cfg(test)]
pub struct Fakes {
    pub clipboard: Arc<clipboard::Memory>,
    pub opener: Arc<opener::Recorder>,
}

pub struct App {
    pub backend: Arc<dyn Backend>,
    pub me: Option<Me>,
    /// Timing for retrying [`App::fetch_me`] while [`App::me`] is still `None`: a flaky start, or a sign-in that was
    /// lost before the first answer arrived, must not leave `me_id()` empty (and reactions, edits and mention/self
    /// checks with it) for the rest of the session.
    me_sched: cadence::Schedule,
    me_fetching: bool,
    pub chats: ChatList,
    /// When a key, mouse or paste event last arrived; with `focused` and the open chat's recent traffic it sets how
    /// often to poll (see `cadence`).
    last_input: Instant,
    /// Whether the terminal is in front. Terminals that do not report focus are always treated as focused.
    focused: bool,
    workers: Workers,
    clipboard: Arc<dyn Clipboard>,
    opener: Arc<dyn Opener>,
    /// Where pictures are saved to be opened in the browser.
    image_dir: Result<PathBuf, String>,
    /// Removes a test app's picture directory when the app is dropped.
    #[cfg(test)]
    scratch: Option<crate::testdir::TestDir>,
    /// How long a chat must stay selected before its messages are fetched, so scrolling through the list does not
    /// fetch every chat it passes.
    open_dwell: Duration,
    pub current: Option<String>,
    selected_at: Instant,
    pub filter: String,
    pub filtering: bool,
    pub unread_only: bool,
    pub focus: Focus,
    pub states: HashMap<String, ChatState>,
    read: ReadState,
    drafts: HashMap<String, String>,
    pub editor: Editor,
    /// How keys are read: vim's modes, or the standard keys of a text box (see `Keymap`).
    pub keymap: Keymap,
    /// Whether a plain Enter sends in the standard keymap (see Editor::set_enter_sends); the hints follow it.
    pub enter_sends: bool,
    images: Images,
    /// Pictures drawn with the terminal's graphics protocol (see pp/gfx.rs).
    gfx: Gfx,
    /// URLs the user explicitly requested to load (unverified images from outside the organisation).
    explicit_loads: HashSet<String>,
    pub img_mode: ImgMode,
    tx: Sender<Ev>,
    rx: Receiver<Ev>,
    pub status: Option<(String, Instant, bool)>,
    /// The animation clock: one step about every 60 ms while the loop runs (never faster, however often it wakes), which
    /// the spinners are drawn from (two steps to a frame of them).
    pub tick: usize,
    /// When `tick` last moved.
    tick_at: Instant,
    /// The step of `tick` the header's gradient is drawn at: it moves a few times a second, and not at all while nobody
    /// can see it (see [`App::animate`]).
    pub header_phase: usize,
    pub quit: bool,
    pub overlay: Overlay,
    pub cmdline: Option<String>,
    pending_g: bool,
    ctrl_w: bool,
    /// The message being edited, if one is.
    editing: Option<Editing>,
    /// The message the next one sent will quote, if any (see `AGENTS.md`, "Messages, mentions and replies").
    replying: Option<Replying>,
    /// The title the next post to a channel will have, set with `:subject` (see `app/post.rs`).
    subject: Option<String>,
    /// The channel whose first-post question was just asked, and when (see `app/post.rs`).
    post_asked: Option<(String, Instant)>,
    /// `post_asked` as it stood when the key now being handled arrived: only the very next key answers the question.
    post_armed: Option<(String, Instant)>,
    /// Channels this session has posted or replied to; the first-post question is not asked again for them.
    posted_channels: HashSet<String>,
    /// The token of the latest edit dispatched for a message, kept past cancellation so a confirmation for an
    /// abandoned attempt (cancelled locally while still in flight, then the same message edited again) is known to
    /// be stale instead of overwriting the newer one it lost the race with.
    edit_tokens: HashMap<String, u64>,
    edit_seq: u64,
    /// Whether favourite chats are listed first, under a divider (the default), or among the rest.
    pub pin_favorites: bool,
    /// Which list the left pane shows. `Teams` only when the backend can list teams (see `teams_available`).
    pub tab: Tab,
    /// The teams and channels, and how they are folded.
    pub teams: Teams,
    /// What the cursor of the teams list stands on (the first row when it is on nothing listed now).
    team_cursor: Option<RowKey>,
    local_n: u64,
    last_yank: Option<String>,
    /// Where things were drawn and how far each pane is scrolled; mouse events are matched against it.
    pub view: View,
    /// Directory lookups / chat creations in flight.
    lookups: usize,
    reactions: Reactions,
    presences: Presences,
    rosters: roster::Rosters,
    mention: mention::MentionState,
    own: Own,
    /// The column Up and Down in the message box are keeping, and where the cursor was left by the move that kept it:
    /// valid only while the cursor is still there (see move_editor_by_rows).
    editor_goal: Option<(usize, (usize, usize))>,
    bell: Bell,
    /// Searching the open chat's messages: the popup's text and rows, and the hits to step through afterwards.
    search: search::Search,
    pub sign_in: SignIn,
    /// When F6 was pressed while the sign-in was fine; a second press soon after goes ahead (see `sign_in_again`).
    sign_in_asked: Option<Instant>,
    pub sending: usize,
    /// Never tell the service anything about what was read. Set for a one-frame `--dump`, which looks at the newest
    /// chat without anyone reading it and must not change what other devices show as unread.
    pub read_only: bool,
    /// Mark links as terminal hyperlinks when drawing (the `hyperlinks` setting; see `hyperlink.rs`). Off until `main` turns it
    /// on, so tests and `--dump`, whose frames are read as text, see none.
    pub hyperlinks: bool,
    /// The encrypted copy of the lists kept for the next start-up, if there is one (see `app/cache.rs`).
    cache: Option<Arc<crate::cache::Cache>>,
    /// The lists have been read from the service since the cache was last written.
    cache_dirty: bool,
    /// When the cache was last written.
    cache_last: Option<Instant>,
    /// The least time between two writes of the cache.
    cache_gap: Duration,
    /// Who is signed in, as the cache says, until the service has said so itself: it is what tells the person's own
    /// name in a chat's title from the others, and the bell's own messages from theirs, before `me` has arrived.
    account_hint: Option<String>,
}

/// How many search results the picker offers: one for each of the keys 1-9 that jump to it.
pub(crate) const MAX_PEOPLE_SHOWN: usize = 9;

/// How long after the first F6 on a working sign-in a second one counts as meant.
const SIGN_IN_CONFIRM_WINDOW: Duration = Duration::from_secs(10);

fn open_url(opener: &dyn Opener, url: &str) -> Result<(), String> {
    if !["https://", "http://", "mailto:"]
        .iter()
        .any(|p| url.starts_with(p))
    {
        return Err("refusing to open a non-web link".into());
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("refusing to open a link with spaces or control characters".into());
    }
    // A mail link opens a new message to the address; the parameters some mail programs honour (subject, body,
    // attachments) are not the sender's to choose.
    match url.strip_prefix("mailto:") {
        Some(rest) => opener.open(&format!(
            "mailto:{}",
            rest.split(['?', '#']).next().unwrap_or("")
        )),
        None => opener.open(url),
    }
}

/// Write `bytes` to `path`, readable by the current user only on Unix (the directory is private too, but it may have
/// existed before with a looser mode).
fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// How often `open_image` sweeps `dir` for files to clean up, rather than on every picture opened.
const IMAGE_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Show a decoded image in the browser. The image's own URL needs our credentials, which a browser doesn't have, so it
/// is saved (as PNG, in the private `dir`) and opened from there. Files older than a day are cleaned up, at most once
/// an hour: someone opening several pictures in a row should not repeat a full directory sweep for each one.
fn open_image(opener: &dyn Opener, dir: &Path, img: &RgbImage) -> Result<(), String> {
    use std::hash::{Hash, Hasher};
    static LAST_SWEEP: Mutex<Option<Instant>> = Mutex::new(None);
    teams_client::create_private_dir(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let due = {
        let mut last = LAST_SWEEP.lock().unwrap_or_else(PoisonError::into_inner);
        let due = last.is_none_or(|t| t.elapsed() > IMAGE_SWEEP_INTERVAL);
        if due {
            *last = Some(Instant::now());
        }
        due
    };
    if due {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(24 * 3600));
            if old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| format!("cannot encode image: {e}"))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    img.as_raw().hash(&mut hasher);
    let path = dir.join(format!("image-{:016x}.png", hasher.finish()));
    write_private_file(&path, png.get_ref())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let url = url::Url::from_file_path(&path).map_err(|()| "bad image path".to_string())?;
    opener.open(url.as_str())
}

impl App {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        let workers = Workers::new(&backend);
        let desktop = Desktop {
            clipboard: Arc::new(clipboard::System::new()),
            opener: Arc::new(opener::Browser),
            image_dir: config::images_dir(),
            open_dwell: OPEN_DWELL,
        };
        Self::with_parts(backend, workers, desktop)
    }

    /// An app that touches nothing outside itself: background work runs on the calling thread, the clipboard is in
    /// memory, links are recorded instead of opened, and a chat opens as soon as it is selected. No threads, timers or
    /// system state, so a test's outcome does not depend on timing.
    #[cfg(test)]
    pub fn for_test(backend: Arc<dyn Backend>) -> (Self, Fakes) {
        let fakes = Fakes {
            clipboard: Arc::new(clipboard::Memory::default()),
            opener: Arc::new(opener::Recorder::default()),
        };
        // Each test app has a picture directory of its own, created only if a picture is opened and removed when the
        // app is dropped.
        let scratch = crate::testdir::TestDir::reserve("test-images");
        let desktop = Desktop {
            clipboard: fakes.clipboard.clone(),
            opener: fakes.opener.clone(),
            image_dir: Ok(scratch.to_path_buf()),
            open_dwell: Duration::ZERO,
        };
        let mut app = Self::with_parts(backend, Workers::inline(), desktop);
        app.scratch = Some(scratch);
        (app, fakes)
    }

    /// Use `clipboard` for copying and pasting (the one `App::new` starts with is the system clipboard alone).
    pub fn set_clipboard(&mut self, clipboard: Arc<dyn Clipboard>) {
        self.clipboard = clipboard;
    }

    fn with_parts(backend: Arc<dyn Backend>, workers: Workers, desktop: Desktop) -> Self {
        let (tx, rx) = channel();
        Self {
            workers,
            clipboard: desktop.clipboard,
            opener: desktop.opener,
            image_dir: desktop.image_dir,
            #[cfg(test)]
            scratch: None,
            open_dwell: desktop.open_dwell,
            backend,
            me: None,
            me_sched: cadence::Schedule::new(),
            me_fetching: false,
            chats: ChatList::new(),
            last_input: Instant::now(),
            focused: true,
            current: None,
            selected_at: Instant::now(),
            filter: String::new(),
            filtering: false,
            unread_only: false,
            focus: Focus::Chats,
            states: HashMap::new(),
            read: ReadState::default(),
            drafts: HashMap::new(),
            editor: Editor::with_keymap(Keymap::default()),
            keymap: Keymap::default(),
            enter_sends: true,
            images: Images::default(),
            gfx: Gfx::default(),
            explicit_loads: HashSet::new(),
            img_mode: ImgMode::DEFAULT,
            tx,
            rx,
            status: None,
            tick: 0,
            tick_at: Instant::now(),
            header_phase: 0,
            quit: false,
            overlay: Overlay::None,
            cmdline: None,
            pending_g: false,
            ctrl_w: false,
            editing: None,
            replying: None,
            subject: None,
            post_asked: None,
            post_armed: None,
            posted_channels: HashSet::new(),
            edit_tokens: HashMap::new(),
            edit_seq: 0,
            pin_favorites: true,
            tab: Tab::Chats,
            teams: Teams::new(),
            team_cursor: None,
            local_n: 0,
            last_yank: None,
            view: View::default(),
            lookups: 0,
            reactions: Reactions::new(),
            presences: Presences::new(),
            rosters: roster::Rosters::new(),
            mention: mention::MentionState::default(),
            own: Own::new(),
            editor_goal: None,
            bell: Bell::default(),
            search: search::Search::default(),
            sign_in: SignIn::Fine,
            sign_in_asked: None,
            sending: 0,
            read_only: false,
            hyperlinks: false,
            cache: None,
            cache_dirty: false,
            cache_last: None,
            cache_gap: cache::WRITE_GAP,
            account_hint: None,
        }
    }

    pub fn me_id(&self) -> &str {
        match &self.me {
            Some(me) => me.id.as_str(),
            None => self.account_hint.as_deref().unwrap_or(""),
        }
    }

    // -----------------------------------------------------------------------
    // Background work
    // -----------------------------------------------------------------------

    /// Run `call` on a background worker and deliver `wrap(result)` to the event loop: polling, lookups, pictures and
    /// anything that may take long (see [`Lane`]). A panic inside `call` is contained and becomes an error result, so
    /// whatever flag marks the request as in flight is always cleared.
    fn spawn_call<T: Send + 'static>(
        &self,
        call: impl FnOnce(&dyn Backend) -> Result<T, Error> + Send + 'static,
        wrap: impl FnOnce(Result<T, Error>) -> Ev + Send + 'static,
    ) {
        self.spawn_call_in(Lane::Background, call, wrap);
    }

    /// [`App::spawn_call`] for what a person has just asked for and is waiting on (a send, an edit, a reaction, a
    /// search, opening a chat): it runs on the interactive lane, so it never queues behind eight slow background calls.
    /// Only for a call that ends after about one request. Nothing that can run for minutes (signing in again in a
    /// browser) belongs here, since two of those would take the whole lane.
    fn spawn_action<T: Send + 'static>(
        &self,
        call: impl FnOnce(&dyn Backend) -> Result<T, Error> + Send + 'static,
        wrap: impl FnOnce(Result<T, Error>) -> Ev + Send + 'static,
    ) {
        self.spawn_call_in(Lane::Interactive, call, wrap);
    }

    fn spawn_call_in<T: Send + 'static>(
        &self,
        lane: Lane,
        call: impl FnOnce(&dyn Backend) -> Result<T, Error> + Send + 'static,
        wrap: impl FnOnce(Result<T, Error>) -> Ev + Send + 'static,
    ) {
        let tx = self.tx.clone();
        let job: Job = Box::new(move |backend| {
            let result = catch_unwind(AssertUnwindSafe(|| call(backend))).unwrap_or(Err(
                Error::Unexpected {
                    operation: "background task",
                    detail: "panicked; this is a bug (see vtteams.log)",
                },
            ));
            // A closed channel means the UI is shutting down, so there is nobody left to tell.
            let _ = tx.send(wrap(result));
            for warning in backend.take_warnings() {
                let _ = tx.send(Ev::Warning(warning));
            }
        });
        self.run(lane, job);
    }

    /// Run something that is not a backend call (opening a browser, writing a file) off the UI thread.
    fn spawn_task(&self, task: impl FnOnce() -> Ev + Send + 'static) {
        let tx = self.tx.clone();
        let job: Job = Box::new(move |_| {
            let ev = catch_unwind(AssertUnwindSafe(task)).unwrap_or(Ev::Notice {
                text: "a background task panicked; this is a bug (see vtteams.log)".into(),
                error: true,
            });
            let _ = tx.send(ev);
        });
        self.run(Lane::Background, job);
    }

    /// Run CPU-bound work that is not a backend call (encoding a picture) off the UI thread, on a thread of its own, so
    /// that it never waits in the pool's queue behind calls that sit on the network for seconds. Where the pool runs
    /// jobs on the calling thread (tests), so does this.
    fn spawn_cpu_task(&self, task: impl FnOnce() -> Ev + Send + 'static) {
        if self.workers.is_inline() {
            return self.spawn_task(task);
        }
        let tx = self.tx.clone();
        let slot = Arc::new(Mutex::new(Some(task)));
        let taken = Arc::clone(&slot);
        let spawned = std::thread::Builder::new()
            .name("vtteams-encode".into())
            .spawn(move || {
                let task = taken.lock().unwrap_or_else(PoisonError::into_inner).take();
                if let Some(task) = task {
                    let ev = catch_unwind(AssertUnwindSafe(task)).unwrap_or(Ev::Notice {
                        text: "a background task panicked; this is a bug (see vtteams.log)".into(),
                        error: true,
                    });
                    let _ = tx.send(ev);
                }
            });
        if spawned.is_err() {
            // No thread to be had: the pool, then, as before.
            let task = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(task) = task {
                self.spawn_task(task);
            }
        }
    }

    fn run(&self, lane: Lane, job: Job) {
        // If no worker could be started at all, degrade to running on this thread rather than never running.
        if let Err(job) = self.workers.submit(lane, job) {
            job(self.backend.as_ref());
        }
    }

    pub fn start(&mut self) {
        self.fetch_me();
        // A list the cache has put on screen is replaced quietly: the "cached" marker says it is old.
        self.refresh_chats_as(self.chats.loaded());
    }

    /// Ask the backend who is signed in, if that is not already known. Safe to call while one is already in flight
    /// or already known; it does nothing then. Retried from [`App::on_tick`] (see [`App::me_sched`]) until it
    /// succeeds, because without it reactions, editing and mention/self checks silently do nothing.
    fn fetch_me(&mut self) {
        if self.me.is_some() || self.me_fetching {
            return;
        }
        self.me_fetching = true;
        self.me_sched.started();
        self.spawn_call(|b| b.me(), Ev::Me);
    }

    pub fn refresh_chats(&mut self) {
        self.refresh_chats_as(false);
    }

    /// Refresh the chat list; `quiet` for the scheduled refresh of a list that is already showing, which has no
    /// spinner: it would flash every few seconds for nothing anyone asked for.
    fn refresh_chats_as(&mut self, quiet: bool) {
        if !self.chats.start_refresh(quiet) {
            return;
        }
        log::debug!(
            "chat list: refresh started ({:?}, {} failure(s) in a row)",
            cadence::engagement(&self.cadence_facts()),
            self.chats.sched.fails()
        );
        self.spawn_call(|b| b.chats(), Ev::Chats);
    }

    /// Fetch history on request, or visibly refresh the newest page.
    fn load_messages(&mut self, chat_id: &str, older: bool) -> bool {
        let fetch = if older {
            Fetch::Older { automatic: false }
        } else {
            Fetch::Newest { quiet: false }
        };
        self.start_message_fetch(chat_id, fetch)
    }

    fn start_message_fetch(&mut self, chat_id: &str, fetch: Fetch) -> bool {
        // Without a sign-in nothing is asked for (see `on_tick`): every request would fail, and scrolling or
        // selecting a message can reach here directly, unlike the scheduled poll and `:refresh`, which check first.
        if !self.sign_in.is_online() {
            return false;
        }
        // Asking again does not make a service that said "wait" answer sooner; it makes it say so again. The scheduled
        // poll checks this through `Schedule::due`; scrolling back, filling up a chat and opening one come here
        // directly. Only what someone asked for is told about it.
        if let Some(left) = self.states.get(chat_id).and_then(|st| st.sched.held_for()) {
            let unasked = matches!(
                fetch,
                Fetch::Older { automatic: true } | Fetch::Newest { quiet: true }
            );
            if !unasked {
                self.set_status(
                    format!(
                        "the service asked us to wait; try again in {} s",
                        left.as_secs() + 1
                    ),
                    true,
                );
            }
            return false;
        }
        let st = self
            .states
            .entry(chat_id.to_string())
            .or_insert_with(ChatState::new);
        if !st.start_fetch(fetch) {
            return false;
        }
        let cursor = if fetch.is_older() {
            st.older.clone()
        } else {
            None
        };
        log::debug!(
            "chat {}: {} started ({} failure(s) in a row)",
            teams_client::id_tag(chat_id),
            fetch.label(),
            st.sched.fails()
        );
        let id = chat_id.to_string();
        let chat_id = id.clone();
        self.spawn_call_in(
            fetch.lane(),
            move |b| b.messages(&id, cursor.as_deref()),
            move |result| Ev::Messages {
                chat_id,
                fetch,
                result,
            },
        );
        true
    }

    /// What the polling policy needs to know right now.
    fn cadence_facts(&self) -> cadence::Facts {
        let exchange = self
            .current
            .as_ref()
            .and_then(|id| self.states.get(id))
            .and_then(|st| st.last_exchange);
        cadence::Facts {
            since_input: self.last_input.elapsed(),
            focused: self.focused,
            since_exchange: exchange.map(|t| t.elapsed()),
        }
    }

    /// Choose whether a plain Enter sends (the default) or adds a line in the standard keymap's message box.
    pub fn set_enter_sends(&mut self, sends: bool) {
        self.enter_sends = sends;
        self.editor.set_enter_sends(sends);
    }

    /// Choose how keys are read. The editor follows, so call it before the first key.
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
        self.editor.set_keymap(keymap);
        self.ctrl_w = false;
        self.pending_g = false;
    }

    /// The terminal was brought to the front or sent behind other windows. Coming back counts as input and fetches
    /// the chat list and the open chat at once, so what happened while away is on screen as you arrive.
    pub fn set_focused(&mut self, focused: bool) {
        if focused != self.focused {
            log::debug!(
                "terminal {}",
                if focused {
                    "in front: refreshing now"
                } else {
                    "behind other windows"
                }
            );
        }
        if focused && !self.focused {
            self.last_input = Instant::now();
            self.refresh_soon();
        }
        self.focused = focused;
    }

    /// Fetch the chat list and the open chat at the next tick, whatever their intervals say.
    fn refresh_soon(&mut self) {
        self.chats.sched.refresh_soon();
        self.presences.refresh_soon();
        if let Some(st) = self.current.as_ref().and_then(|id| self.states.get_mut(id)) {
            st.sched.refresh_soon();
        }
    }

    pub fn is_busy(&self) -> bool {
        self.chats.shows_loading()
            || self.teams.shows_loading()
            || self.sending > 0
            || self.lookups > 0
            || self
                .current
                .as_ref()
                .and_then(|c| self.states.get(c))
                .is_some_and(ChatState::shows_loading)
    }

    /// Nothing in flight: all loads and image downloads have finished, and no job of either worker lane is queued or
    /// running (a call nothing else tracks, an interactive one included, would otherwise end a `--dump` early).
    pub fn settled(&self) -> bool {
        self.workers.in_flight() == 0
            && !self.is_busy()
            && !self.chats.is_fetching()
            && !self.teams.is_fetching()
            && !self.states.values().any(ChatState::is_fetching)
            && !self.images.is_busy()
            && !self.search.is_busy()
            && !self.presences.in_flight
    }

    /// A backend call ended in an error: if it means the sign-in is gone and this service can sign in again while
    /// running, say so (the banner) and stop fetching until it is done.
    fn note_sign_in_lost(&mut self, e: &Error) {
        if self.sign_in.note_lost(e, self.backend.can_sign_in_again()) {
            log::warn!("the sign-in is gone ({}); waiting for F6", e.for_log());
            self.open_sign_in_popup();
        }
    }

    /// Show the sign-in popup, unless the person is in the middle of something else in a popup. Already showing it
    /// (a retry just failed) leaves it alone rather than rearming its "ignore accidental keys" grace window right
    /// when someone reacts to the failure and presses Enter or clicks again.
    fn open_sign_in_popup(&mut self) {
        if matches!(self.overlay, Overlay::None) {
            self.overlay = Overlay::SignIn {
                since: Instant::now(),
            };
        }
    }

    /// Sign in again in the browser, without leaving the program: what is being typed stays where it is.
    pub fn sign_in_again(&mut self) {
        if !self.backend.can_sign_in_again() {
            self.set_status(
                "this service cannot sign in again from here; run `vtteams logout`, then start vtteams",
                true,
            );
            return;
        }
        // Nothing is wrong as far as vtteams knows: opening a browser window on a working session is almost always an
        // accidental key, so ask once. Pressing it again within the window means it.
        if self.sign_in.is_online() {
            let confirmed = self
                .sign_in_asked
                .take()
                .is_some_and(|at| at.elapsed() < SIGN_IN_CONFIRM_WINDOW);
            if !confirmed {
                self.sign_in_asked = Some(Instant::now());
                self.set_status(
                    "you are signed in; press F6 again to sign in anyway (a browser window opens)",
                    false,
                );
                return;
            }
        }
        if !self.sign_in.begin() {
            self.set_status("the sign-in is waiting in your browser", false);
            return;
        }
        // Background, never the interactive lane: this waits for a person in a browser for up to half an hour.
        self.spawn_call(|b| b.sign_in_again(), Ev::SignedIn);
    }

    pub fn set_status(&mut self, msg: impl Into<String>, err: bool) {
        self.status = Some((msg.into(), Instant::now(), err));
    }

    /// Move the header's gradient on, as often as `HEADER_EVERY` ticks allow, while someone can see it: the terminal in
    /// front (a terminal that never says it is behind other windows counts as in front) and the person not away. While
    /// nobody can the gradient stands still, so an idle app that is not looked at sends the terminal nothing.
    pub(crate) fn animate(&mut self) {
        let seen = self.focused && !cadence::is_away(&self.cadence_facts());
        if seen {
            self.header_phase = self.tick / HEADER_EVERY * HEADER_EVERY;
        }
    }

    /// Say when a key was last pressed, to put the person at the keyboard or far from it.
    #[cfg(test)]
    pub fn set_last_input(&mut self, at: Instant) {
        self.last_input = at;
    }

    pub fn on_tick(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.tick_at) >= TICK_EVERY {
            self.tick = self.tick.wrapping_add(1);
            self.tick_at = now;
        }
        self.animate();
        while let Ok(ev) = self.rx.try_recv() {
            self.on_event(ev);
        }
        // A hint from a backend's own push channel, if it has one (Trouter, for Teams web): make the one resource
        // it is about due at once, so the existing poll-and-merge path picks it up sooner rather than waiting out
        // its normal interval. Not a second way to apply an update -- an empty result either way is business as
        // usual for a purely poll-based backend.
        for hint in self.backend.poll_pushes() {
            match hint {
                // The open chat polls itself, so waking its schedule is enough. Any other chat -- one never opened this
                // session, or one visited earlier whose state is kept but is no longer polled -- has nothing of its
                // own to wake, so its preview, unread mark and place in the list are refreshed instead.
                PushHint::Chat(id) => {
                    if let Some(st) = self.states.get_mut(&id) {
                        st.sched.refresh_soon();
                    }
                    if self.current.as_deref() != Some(id.as_str()) {
                        self.chats.sched.refresh_soon();
                    }
                }
                // `PushHint::Unknown`, and anything a later library version adds: refresh broadly rather than
                // risk silently ignoring a hint `PushHint` is `#[non_exhaustive]` for exactly this reason.
                _ => self.refresh_soon(),
            }
        }
        if self
            .status
            .as_ref()
            .is_some_and(|(_, t, err)| t.elapsed() > Duration::from_secs(if *err { 12 } else { 4 }))
        {
            self.status = None;
        }
        let facts = self.cadence_facts();
        // Without a sign-in nothing is asked for: every request would fail, and the person is told what to do.
        let online = self.sign_in.is_online();
        if online && self.me.is_none() && !self.me_fetching && self.me_sched.due(ME_RETRY) {
            self.fetch_me();
        }
        // Also before the first list has arrived: a first load that failed is tried again on the schedule.
        if online
            && !self.chats.is_fetching()
            && self.chats.sched.due(cadence::chats_refresh(&facts))
        {
            self.refresh_chats_as(self.chats.loaded());
        }
        if online {
            self.refresh_presence(&facts);
            self.refresh_roster();
            self.report_activity_if_due(&facts);
            self.refresh_teams_if_due();
        }
        self.save_cache_if_due();
        // What the chat list said about channels costs no request, so it is merged over the teams list on every tick.
        self.merge_channel_activity();
        if let Some(id) = self.current.clone() {
            // A search hit being reached by fetching older pages carries on when the request slot is free.
            self.drive_seek(&id, false);
            self.keep_search_rows_current();
            let (loaded, loading, scheduled, failed) = match self.states.get(&id) {
                Some(s) => (
                    s.loaded,
                    s.is_fetching(),
                    s.sched.due(cadence::messages_poll(&facts)),
                    s.sched.fails() > 0,
                ),
                None => (false, false, false, false),
            };
            // The first attempt waits for the selection to settle; every later one, including a retry after a failed
            // first load, is fetched when the chat's schedule says (see `Schedule`).
            let due = if loaded || failed {
                scheduled
            } else {
                self.selected_at.elapsed() >= self.open_dwell
            };
            if due && !loading && online {
                self.start_message_fetch(&id, Fetch::Newest { quiet: loaded });
            }
            // Messages that arrive while the terminal is in the background are not read yet.
            if loaded && self.focused && online {
                self.mark_current_read();
                self.mark_current_read_on_the_service(&facts);
            }
        }
        if online {
            while let Some(url) = self.images.next_download() {
                let wanted = url.clone();
                let is_explicit = self.explicit_loads.contains(&url);
                // A picture someone asked to load (Ctrl-L / L) is waited on; one fetched because it scrolled into
                // view is not.
                self.spawn_call_in(
                    if is_explicit {
                        Lane::Interactive
                    } else {
                        Lane::Background
                    },
                    move |b| {
                        if is_explicit {
                            b.image_from_outside(&wanted)
                        } else {
                            b.image(&wanted)
                        }
                    },
                    move |result| Ev::Image { url, result },
                );
            }
        }
        self.dispatch_encodes();
    }

    /// Tell the service that the open chat is read up to its newest message, once per message, while the person is
    /// actually here (the terminal is in front and they have used it within the last half hour). Their other apps
    /// then stop showing the chat as unread.
    fn mark_current_read_on_the_service(&mut self, facts: &cadence::Facts) {
        // Messages arriving in the open chat keep it a "conversation", which would hide that nobody has touched the
        // keyboard for hours: marking read tells every other device the messages were seen, so only input counts.
        if self.read_only || !self.backend.marks_read() || cadence::is_away(facts) {
            return;
        }
        let Some(chat_id) = self.current.clone() else {
            return;
        };
        let Some(newest) = self.states.get(&chat_id).and_then(|st| {
            st.messages()
                .iter()
                .rev()
                .find(|m| m.delivery == Delivery::Sent && !m.id.starts_with("local-"))
        }) else {
            return;
        };
        let (message_id, arrived) = (newest.id.clone(), newest.created);
        // A channel has no read marker to compare with: when the service does not call it unread, it has read it up to
        // its newest message already.
        let server_read = match self.teams.channel(&chat_id) {
            Some((_, ch)) => ch.last.as_ref().map(|p| p.created).filter(|_| !ch.unread),
            None => self
                .chats
                .items()
                .iter()
                .find(|c| c.id == chat_id)
                .and_then(|c| c.last_read),
        };
        if !self
            .read
            .begin_service_mark(&chat_id, arrived, server_read, Instant::now())
        {
            return;
        }
        log::debug!(
            "chat {}: marking read up to a message",
            teams_client::id_tag(&chat_id)
        );
        let chat = chat_id.clone();
        self.spawn_call(
            move |b| b.mark_read(&chat, &message_id, arrived),
            move |result| Ev::MarkedRead { chat_id, result },
        );
    }

    fn mark_current_read(&mut self) {
        let Some(id) = self.current.clone() else {
            return;
        };
        if self.current_channel().is_some() {
            self.mark_current_channel_read();
            return;
        }
        if let Some(created) = self
            .chats
            .items()
            .iter()
            .find(|c| c.id == id)
            .and_then(|c| c.preview.as_ref())
            .map(|p| p.created)
        {
            self.read.mark_local(id, created);
        }
    }

    fn on_event(&mut self, ev: Ev) {
        if let Some(e) = ev.sign_in_error() {
            self.note_sign_in_lost(e);
        }
        match ev {
            Ev::SignedIn(result) => match result {
                Ok(()) => {
                    log::info!("signed in again");
                    self.sign_in.succeed();
                    self.forget_reported_activity();
                    // Pictures that failed while the sign-in was lost (or on a permission it has now) get another go.
                    self.images.retry_failed();
                    if matches!(self.overlay, Overlay::SignIn { .. }) {
                        self.overlay = Overlay::None;
                    }
                    // Everything waited for the sign-in; fetch it all now.
                    self.chats.sched.refresh_soon();
                    self.teams.sched.refresh_soon();
                    for st in self.states.values_mut() {
                        st.sched.refresh_soon();
                    }
                    self.fetch_me();
                    self.set_status("signed in again", false);
                }
                Err(e) => {
                    log::warn!("signing in again failed: {}", e.for_log());
                    self.sign_in.fail(&e);
                    self.open_sign_in_popup();
                }
            },
            Ev::Me(r) => {
                self.me_fetching = false;
                match r {
                    Ok(me) => {
                        self.me = Some(me);
                        self.me_sched.succeeded();
                    }
                    Err(e) => {
                        self.me_sched.failed(throttle_hold(&e));
                        self.set_status(format!("profile: {}", describe_running(&e)), true);
                    }
                }
            }
            Ev::Chats(r) => {
                match r {
                    Ok(chats) => {
                        log::debug!(
                            "chat list: {} chat(s) in {} ms",
                            chats.len(),
                            self.chats.sched.since_last().as_millis()
                        );
                        self.chats.receive(chats);
                        self.mark_cache_dirty();
                        self.bell_for_list();
                        // An open channel is not in the chat list and is not gone because of that.
                        let current_gone = self.current.is_some()
                            && self.current_channel().is_none()
                            && !self
                                .chats
                                .items()
                                .iter()
                                .any(|c| Some(&c.id) == self.current.as_ref());
                        // A chat dropping out of a refreshed list (the `max_chats` cap, or it was left or hidden
                        // elsewhere) must not silently pull someone away, mid-draft, into sending the wrong chat:
                        // a scheduled refresh runs every 20-180 s even while nobody asked for one. Move on only when
                        // there is nothing of theirs that a silent switch could lose.
                        let move_on = self.current.is_none()
                            || (current_gone
                                && self.editor.is_empty()
                                && self.focus != Focus::Editor);
                        if move_on {
                            // The conversation with the latest activity, not the top of the list: that may be a
                            // favourite from long ago.
                            let newest = self
                                .visible_chats()
                                .into_iter()
                                .max_by_key(|&i| self.chats.items()[i].updated)
                                .map(|i| self.chats.items()[i].id.clone());
                            if let Some(id) = newest {
                                self.select_chat(&id);
                            }
                        }
                    }
                    Err(e) => {
                        let hold = throttle_hold(&e);
                        log::warn!(
                            "chat list: refresh failed after {} ms ({} in a row){}: {}",
                            self.chats.sched.since_last().as_millis(),
                            self.chats.sched.fails() + 1,
                            hold.map_or_else(String::new, |h| format!(
                                ", throttled for {} s",
                                h.as_secs()
                            )),
                            e.for_log()
                        );
                        self.chats.fail(&e);
                        let text = describe_running(&e);
                        if self.chats.loaded() {
                            // The list is on screen already, so the failure would otherwise go unseen.
                            self.set_status(format!("cannot refresh chats: {text}"), true);
                        }
                    }
                }
            }
            Ev::Teams(result) => self.on_teams(result),
            Ev::People { query, result } => {
                self.lookups = self.lookups.saturating_sub(1);
                match result {
                    Err(e) => {
                        self.set_status(format!("search failed: {}", describe_side(&e)), true)
                    }
                    Ok(people) if people.is_empty() => {
                        self.set_status(format!("no one found for '{query}'"), true)
                    }
                    Ok(mut people) if people.len() == 1 => self.open_contact(people.remove(0)),
                    Ok(mut people) => {
                        // The picker draws nine, jumps with 1-9; the rest would be selectable but never seen.
                        people.truncate(MAX_PEOPLE_SHOWN);
                        self.status = None;
                        self.overlay = Overlay::People { people, sel: 0 };
                        // How available they are is part of choosing: asked for at once, not at the next round.
                        self.presences.refresh_soon();
                    }
                }
            }
            Ev::Opened(result) => {
                self.lookups = self.lookups.saturating_sub(1);
                match result {
                    Ok(chat) => self.show_chat(chat),
                    Err(e) => {
                        self.set_status(format!("cannot open chat: {}", describe_running(&e)), true)
                    }
                }
            }
            Ev::Messages {
                chat_id,
                fetch,
                result,
            } => self.on_messages(chat_id, fetch, result),
            Ev::Sent {
                chat_id,
                local_id,
                result,
            } => {
                self.sending = self.sending.saturating_sub(1);
                let me_id = self.me_id().to_string();
                let Some(st) = self.states.get_mut(&chat_id) else {
                    return;
                };
                match result {
                    Ok(sent) => {
                        st.confirm_send(&local_id, sent.clone(), &me_id);
                        self.chats.record_sent(&chat_id, &sent);
                    }
                    Err(e) => {
                        // A refusal means nothing was sent. A dropped connection or an unreadable answer means the
                        // message may have been delivered, so it must not be offered for an easy resend.
                        let unconfirmed = e.delivery_unknown();
                        let reason = describe_running(&e);
                        // The box is only offered back when nothing else has claimed it since: an edit or a reply
                        // started meanwhile would otherwise be overwritten, or the failed text saved over another
                        // message.
                        let editor_available = self.current.as_deref() == Some(chat_id.as_str())
                            && self.editor.is_empty()
                            && self.editing.is_none()
                            && self.replying.is_none();
                        let subject = st.take_pending_subject(&local_id);
                        if let Some((md, reply_to)) =
                            st.fail_send(&local_id, &reason, unconfirmed, editor_available)
                        {
                            self.editor.set_text(&md);
                            self.replying = reply_to.map(|target| Replying { target });
                            // A title typed meanwhile is the newer one.
                            if self.subject.is_none() {
                                self.subject = subject;
                            }
                        }

                        let what = if unconfirmed {
                            "send not confirmed"
                        } else {
                            "send failed"
                        };
                        self.set_status(format!("{what}: {reason}"), true);
                    }
                }
            }
            // The backend has logged the warning itself; this puts it in front of the user.
            Ev::Edited {
                chat_id,
                message_id,
                html,
                token,
                result,
            } => self.on_edited(chat_id, message_id, html, token, result),
            Ev::Favorited {
                chat_id,
                on,
                previous,
                result,
            } => self.on_favorited(&chat_id, on, previous, result),
            Ev::Searched {
                chat,
                scope,
                query,
                from,
                seq,
                result,
            } => self.on_searched(&chat, scope, &query, from, seq, result),
            Ev::Presence { asked, result } => self.on_presence(&asked, result),
            Ev::Roster { chat_id, result } => self.on_roster(chat_id, result),
            Ev::Reported { active, result } => self.on_reported(active, result),
            Ev::StatusSet { choice, result } => self.on_status_set(choice, result),
            Ev::Warning(e) => self.set_status(describe_side(&e), true),
            Ev::Notice { text, error } => self.set_status(text, error),
            Ev::CacheWritten(result) => match result {
                Ok(bytes) => log::debug!("start-up cache: written ({bytes} bytes)"),
                Err(why) => log::warn!("start-up cache: not written: {why}"),
            },
            Ev::MarkedRead { chat_id, result } => {
                if let Err(e) = result {
                    // Not worth interrupting anyone for; the chat is tried again after a pause.
                    log::warn!(
                        "chat {}: could not mark it read: {}",
                        teams_client::id_tag(&chat_id),
                        e.for_log()
                    );
                    self.read.fail_service_mark(&chat_id, &e, Instant::now());
                }
            }
            Ev::ReactionEmoji(result) => match result {
                Ok(offered) => {
                    self.reactions.receive_catalog(offered);
                }
                Err(e) => {
                    log::warn!("reaction list unavailable: {}", e.for_log());
                    if self.reactions.fail_catalog() {
                        self.set_status(
                            format!(
                                "only the classic reactions are offered: {}",
                                describe_side(&e)
                            ),
                            true,
                        );
                    }
                }
            },
            Ev::Reacted {
                chat_id,
                message_id,
                emoji,
                on,
                result,
            } => {
                self.reactions.complete(&chat_id, &message_id);
                match result {
                    Ok(()) => self.set_status(
                        format!("{} {emoji}", if on { "reacted" } else { "took back" }),
                        false,
                    ),
                    Err(e) => {
                        log::warn!("reaction failed: {}", e.for_log());
                        self.apply_reaction(&chat_id, &message_id, &emoji, !on);
                        self.set_status(format!("reaction failed: {}", describe_side(&e)), true);
                    }
                }
                if let Some(st) = self.states.get_mut(&chat_id) {
                    st.sched.refresh_soon();
                }
            }
            Ev::Image { url, result } => {
                let state = match result {
                    Ok(img) => {
                        log::debug!("image ready: {}x{}", img.width(), img.height());
                        ImgState::Ready(Arc::new(img))
                    }
                    Err(Error::UnverifiedContent {
                        url: unverified_url,
                        ..
                    }) => {
                        log::debug!(
                            "image not loaded: from outside organisation: {}",
                            teams_client::id_tag(&unverified_url)
                        );
                        ImgState::Unverified(unverified_url)
                    }
                    Err(e) => {
                        log::warn!("image failed: {}", e.for_log());
                        ImgState::Failed(describe_side(&e).chars().take(60).collect())
                    }
                };
                self.forget_encodings(&url);
                if matches!(state, ImgState::Ready(_)) {
                    self.note_decoded(&url);
                }
                self.images.complete(&url, state);
            }
            Ev::Encoded { key, result } => self.on_encoded(key, result),
        }
    }

    fn on_messages(&mut self, chat_id: String, fetch: Fetch, result: Result<Page, Error>) {
        let me_id = self.me_id().to_string();
        let is_current = self.current.as_deref() == Some(chat_id.as_str());
        let Some(st) = self.states.get_mut(&chat_id) else {
            return;
        };
        let (was_loaded, received) = (st.loaded, result.is_ok());
        let fetch_more = match result {
            Err(e) => {
                let hold = throttle_hold(&e);
                log::warn!(
                    "chat {}: {} failed after {} ms ({} in a row){}: {}",
                    teams_client::id_tag(&chat_id),
                    fetch.label(),
                    st.sched.since_last().as_millis(),
                    st.sched.fails() + 1,
                    hold.map_or_else(String::new, |h| format!(
                        ", throttled for {} s",
                        h.as_secs()
                    )),
                    e.for_log()
                );
                st.fail_fetch(&e);
                // An automatic autofill page is a background mechanism nobody asked for (see "Chats of mostly
                // events"); a quiet poll or a page someone scrolled to still surfaces its own failure. A page fetched
                // to reach a search hit says so itself (`drive_seek`).
                if is_current && !matches!(fetch, Fetch::Older { automatic: true } | Fetch::Seek) {
                    self.set_status(describe_running(&e), true);
                }
                false
            }
            Ok(page) => {
                log::debug!(
                    "chat {}: {} returned {} message(s) in {} ms",
                    teams_client::id_tag(&chat_id),
                    fetch.label(),
                    page.messages.len(),
                    st.sched.since_last().as_millis()
                );
                st.receive_page(fetch, page, &me_id)
            }
        };
        if received {
            self.bell_for_chat(&chat_id, was_loaded);
        }
        if fetch_more && is_current {
            self.start_message_fetch(&chat_id, Fetch::Older { automatic: true });
        }
        // A page that arrived, or a request that failed, is what a seek was waiting for.
        self.drive_seek(&chat_id, !received && fetch == Fetch::Seek);
    }

    // -----------------------------------------------------------------------
    // Chat list
    // -----------------------------------------------------------------------

    pub fn is_unread(&self, chat: &Chat) -> bool {
        self.read.is_unread(chat, self.me_id())
    }

    /// Unread messages that @mention the signed-in user, across chats loaded this session (`self.states`). A chat
    /// never opened this session has no `ChatState` to look at yet, so it undercounts rather than guesses from the
    /// chat list's own plain-text preview; opening the chat catches it up.
    pub fn outstanding_mentions(&self) -> usize {
        let me = self.me_id();
        self.states
            .iter()
            .filter_map(|(id, state)| {
                let chat = self.chats.items().iter().find(|c| &c.id == id)?;
                let read = self.read.read_marker(chat);
                Some(
                    state
                        .messages()
                        .iter()
                        .filter(|m| {
                            !m.deleted
                                && !m.system
                                && m.from_id.as_deref() != Some(me)
                                && m.mentions_me(me)
                                && read.is_none_or(|r| m.created > r)
                        })
                        .count(),
                )
            })
            .sum()
    }

    /// Indices into `chats` that pass the filters, in list order: favourites first, in the order the user gave them,
    /// when they are pinned (`pin_favorites`), then the others newest first.
    pub fn visible_chats(&self) -> Vec<usize> {
        let q = self.filter.to_lowercase();
        let me = self.me_id();
        let items = self.chats.items();
        let mut visible: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                (!self.unread_only
                    || self.is_unread(c)
                    || self.current.as_deref() == Some(c.id.as_str()))
                    && (q.is_empty()
                        || c.title(me).to_lowercase().contains(&q)
                        || c.preview
                            .as_ref()
                            .is_some_and(|p| p.text.to_lowercase().contains(&q)))
            })
            .map(|(i, _)| i)
            .collect();
        if self.pin_favorites {
            // Stable: the others keep their order.
            visible.sort_by_key(|&i| (items[i].favorite.is_none(), items[i].favorite.unwrap_or(0)));
        }
        visible
    }

    /// How many of the first of `visible` are pinned favourites (0 when favourites are not pinned).
    pub fn pinned_count(&self, visible: &[usize]) -> usize {
        if !self.pin_favorites {
            return 0;
        }
        let items = self.chats.items();
        visible
            .iter()
            .take_while(|&&i| items[i].favorite.is_some())
            .count()
    }

    pub fn current_chat(&self) -> Option<&Chat> {
        let id = self.current.as_ref()?;
        self.chats.items().iter().find(|c| &c.id == id)
    }

    pub fn state(&self) -> Option<&ChatState> {
        self.states.get(self.current.as_ref()?)
    }

    pub fn state_mut(&mut self) -> Option<&mut ChatState> {
        let id = self.current.clone()?;
        self.states.get_mut(&id)
    }

    /// Make the open chat's transcript be built again at the next frame, as a new message would, without changing it.
    #[cfg(test)]
    pub(crate) fn invalidate_open_transcript(&mut self) {
        if let Some(st) = self.state_mut() {
            st.edit_messages(|_| {});
        }
    }

    pub fn select_chat(&mut self, id: &str) {
        if self.current.as_deref() == Some(id) {
            return;
        }
        // The text in the box is the message being edited, not a draft: put the real draft back first.
        self.cancel_edit(false);
        self.cancel_reply();
        self.cancel_subject();
        // What was found in the chat being left, and any hunt for a message in it, is of no use in the next one.
        self.leave_chat_search();
        if let Some(old) = self.current.take() {
            let text = self.editor.text();
            if text.trim().is_empty() {
                self.drafts.remove(&old);
            } else {
                self.drafts.insert(old, text);
            }
        }
        self.current = Some(id.to_string());
        self.selected_at = Instant::now();
        match self.drafts.get(id) {
            Some(d) => self.editor.set_text(d),
            None => self.editor.clear(),
        }
        let marker = self
            .chats
            .items()
            .iter()
            .find(|c| c.id == id)
            .filter(|c| self.is_unread(c))
            .and_then(|c| c.last_read);
        let st = self
            .states
            .entry(id.to_string())
            .or_insert_with(ChatState::new);
        // Set the "new" divider position from the current unread state.
        st.unread_marker = marker;
        st.reveal = true;
        if st.loaded && st.follow {
            st.sel = st.messages().len().saturating_sub(1);
        }
    }

    fn move_chat(&mut self, delta: isize) {
        let vis = self.visible_chats();
        if vis.is_empty() {
            return;
        }
        let pos = vis
            .iter()
            .position(|&i| Some(&self.chats.items()[i].id) == self.current.as_ref());
        let next = match pos {
            Some(p) => (p as isize + delta).clamp(0, vis.len() as isize - 1) as usize,
            None => 0,
        };
        let id = self.chats.items()[vis[next]].id.clone();
        self.select_chat(&id);
    }

    fn goto_chat(&mut self, last: bool) {
        let vis = self.visible_chats();
        let pick = if last { vis.last() } else { vis.first() };
        if let Some(&i) = pick {
            let id = self.chats.items()[i].id.clone();
            self.select_chat(&id);
        }
    }

    // -----------------------------------------------------------------------
    // Transcript layout (called by the UI before drawing)
    // -----------------------------------------------------------------------

    fn build_known_names(&self, chat_id: &str) -> HashMap<String, String> {
        let mut names = HashMap::new();
        if let Some(chat) = self.chats.items().iter().find(|c| c.id == chat_id) {
            for m in self.attendees(chat) {
                if !m.id.is_empty() && !m.name.is_empty() {
                    names.insert(m.id.clone(), m.name.clone());
                }
            }
        }
        if let Some(st) = self.states.get(chat_id) {
            for m in st.messages() {
                if let Some(id) = &m.from_id
                    && !id.is_empty()
                    && !m.from_name.is_empty()
                {
                    names.insert(id.clone(), m.from_name.clone());
                }
            }
        }
        names
    }

    /// The people of chat `chat_id` the directory no longer lists (`Member::not_in_directory`), by id.
    fn unlisted_people(&self, chat_id: &str) -> std::collections::BTreeSet<String> {
        self.chats
            .items()
            .iter()
            .find(|c| c.id == chat_id)
            .map(|c| {
                c.members
                    .iter()
                    .filter(|m| m.not_in_directory)
                    .map(|m| m.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn layout_transcript(&mut self, width: usize, height: usize) {
        let Some(id) = self.current.clone() else {
            return;
        };
        let me_id = self.me_id().to_string();
        let spinner_idx = (self.tick / 2) % crate::theme::SPINNER.len();
        let mode = self.img_mode;
        let known_names = self.build_known_names(&id);
        let unlisted = self.unlisted_people(&id);
        let Some(st) = self.states.get_mut(&id) else {
            return;
        };
        let spinner = if self.images.is_downloading()
            || st
                .messages()
                .iter()
                .any(|m| m.delivery == Delivery::Sending)
        {
            spinner_idx
        } else {
            0
        };
        let backend = &self.backend;
        let serves = |url: &str| backend.serves_image(url);
        let ctx = Ctx {
            images: self.images.store(),
            mode,
            spinner: crate::theme::SPINNER[spinner_idx],
            fetchable: &serves,
            keymap: self.keymap,
        };
        let people = crate::transcript::People {
            names: &known_names,
            unlisted: &unlisted,
        };
        st.layout(width, height, spinner, &ctx, &me_id, &people);

        // Fetch only images near the viewport (a screen either side), and keep memory bounded by dropping decoded
        // images far from it; scrolling back re-fetches them.
        let near =
            st.scroll.saturating_sub(height)..st.scroll.saturating_add(height.saturating_mul(2));
        let nearby: Vec<String> = st
            .transcript
            .image_rows
            .iter()
            .filter(|(rows, _)| rows.start < near.end && rows.end > near.start)
            .map(|(_, url)| url.clone())
            .collect();
        // Filter to only images that should be loaded: served by backend, and either not unverified
        // or explicitly approved for loading.
        let store = self.images.store();
        let explicit_loads = &self.explicit_loads;
        let accepted: Vec<String> = nearby
            .into_iter()
            .filter(|url| {
                serves(url)
                    && match store.map.get(url) {
                        Some(ImgState::Unverified(_)) => explicit_loads.contains(url),
                        _ => true,
                    }
            })
            .collect();
        self.images
            .observe_viewport(&accepted, mode != ImgMode::Off, |_| true);
    }

    // -----------------------------------------------------------------------
    // Keys
    // -----------------------------------------------------------------------

    /// Route keys in precedence order: popup, command panel, filter, global shortcuts, then the focused pane.
    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        self.last_input = Instant::now();
        // The question before a first post is answered by the very next key being a send; any other key drops it.
        self.post_armed = self.post_asked.take();
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            self.quit = true;
            return;
        }
        match self.overlay.clone() {
            Overlay::Help(scroll) => {
                let height = self.view.geometry.panel.bottom();
                let rows = crate::ui::help_rows(self.keymap, height);
                let most = crate::ui::help_lines(self.keymap)
                    .len()
                    .saturating_sub(rows);
                let page = rows.max(1);
                let next = match key.code {
                    KeyCode::Up | KeyCode::Char('k') => Some(scroll.saturating_sub(1)),
                    KeyCode::Down | KeyCode::Char('j') => Some((scroll + 1).min(most)),
                    KeyCode::PageUp => Some(scroll.saturating_sub(page)),
                    KeyCode::PageDown => Some((scroll + page).min(most)),
                    KeyCode::Home => Some(0),
                    KeyCode::End => Some(most),
                    _ => None,
                };
                self.overlay = next.map_or(Overlay::None, Overlay::Help);
                return;
            }
            Overlay::Links(links) => {
                self.overlay = Overlay::None;
                if let KeyCode::Char(c @ '1'..='9') = key.code
                    && let Some(url) = links.get(c as usize - '1' as usize)
                {
                    self.open_link(url);
                }
                return;
            }
            Overlay::People { people, sel } => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let last = people.len().saturating_sub(1);
                let pick = match key.code {
                    KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => Err((sel + 1).min(last)),
                    KeyCode::Char('n') if ctrl => Err((sel + 1).min(last)),
                    KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                        Err(sel.saturating_sub(1))
                    }
                    KeyCode::Char('p') if ctrl => Err(sel.saturating_sub(1)),
                    KeyCode::Enter => Ok(Some(sel)),
                    KeyCode::Char(c @ '1'..='9') => Ok(Some(c as usize - '1' as usize)),
                    KeyCode::Esc | KeyCode::Char('q') => Ok(None),
                    _ => Err(sel),
                };
                match pick {
                    Err(sel) => self.overlay = Overlay::People { people, sel },
                    Ok(chosen) => {
                        self.overlay = Overlay::None;
                        if let Some(who) = chosen.and_then(|i| people.get(i)) {
                            self.open_contact(who.clone());
                        }
                    }
                }
                return;
            }
            Overlay::Emoji { target, query, sel } => {
                self.on_emoji_key(key, target, query, sel);
                return;
            }
            Overlay::Status { sel } => {
                self.on_status_key(key, sel);
                return;
            }
            Overlay::Search => {
                self.on_search_key(key);
                return;
            }
            Overlay::SignIn { since } => {
                self.on_sign_in_key(key, since);
                return;
            }
            Overlay::None => {}
        }
        if self.cmdline.is_some() {
            self.on_cmdline_key(key);
            return;
        }
        // Alt-1 and Alt-2 pick the tab of the left pane (standard keymap), even from the message box or a filter.
        if self.keymap == Keymap::Standard
            && let Some(tab) = self.tab_shortcut(&key)
        {
            self.show_tab(tab);
            return;
        }
        if self.filtering {
            self.on_filter_key(key);
            return;
        }
        // F1 is help in every keymap and every pane (`?` is only help where it is not a character to type).
        if key.code == KeyCode::F(1) {
            self.overlay = Overlay::Help(0);
            return;
        }
        // F6 signs in again when the sign-in has expired, in every keymap and pane, without touching the message box.
        if key.code == KeyCode::F(6) {
            self.sign_in_again();
            return;
        }
        // F9 chooses your own status (available, busy, do not disturb, ...), in every keymap and pane.
        if key.code == KeyCode::F(9) {
            self.open_status_dialog();
            return;
        }
        // F7 favourites the open chat (or takes it out) and F8 pins the favourites above the rest or lists them with
        // the others, in every keymap and pane.
        if key.code == KeyCode::F(7) {
            self.toggle_favorite();
            return;
        }
        if key.code == KeyCode::F(8) {
            self.toggle_pinned();
            return;
        }
        // F4 edits a message of yours in every keymap: the selected one in the conversation, else your last one.
        if key.code == KeyCode::F(4) {
            if self.focus == Focus::Messages {
                self.edit_selected();
            } else {
                self.edit_last_own();
            }
            return;
        }
        // F2 is the emoji selector in every keymap and pane (Ctrl-E as well in the standard keymap).
        if key.code == KeyCode::F(2) {
            self.open_emoji_selector();
            return;
        }
        if self.keymap == Keymap::Standard {
            if self.on_standard_global_key(key) {
                return;
            }
        } else {
            // `^W` (set in the transcript or the editor) waits for one more key; handling it here means no pane can
            // leave it armed for the next, unrelated keypress.
            if std::mem::take(&mut self.ctrl_w) {
                self.window_command(key.code);
                return;
            }
            // Ctrl-h / Ctrl-l move between panes from anywhere, including the editor's insert mode, where they take
            // precedence over vim's Ctrl-h backspace (the Backspace key deletes).
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
            {
                match key.code {
                    KeyCode::Char('h') => return self.pane_left(),
                    KeyCode::Char('l') => return self.pane_right(),
                    _ => {}
                }
            }
        }
        match (self.keymap, self.focus) {
            (Keymap::Vim, Focus::Chats) => self.on_chats_key(key),
            (Keymap::Vim, Focus::Messages) => self.on_messages_key(key),
            (Keymap::Standard, Focus::Chats) => self.on_chats_key_standard(key),
            (Keymap::Standard, Focus::Messages) => self.on_messages_key_standard(key),
            (_, Focus::Editor) => self.on_editor_key(key),
        }
    }

    // -----------------------------------------------------------------------
    // Sending
    // -----------------------------------------------------------------------

    pub fn send(&mut self) {
        // A channel whose service cannot post: nothing is sent and the box keeps what was typed. The one place a send
        // starts from, so every key that sends is covered.
        let in_channel = self.current_channel().is_some();
        if in_channel && !self.backend.can_post_to_channels() {
            self.set_status(teams::NO_POSTING, true);
            return;
        }
        // Enter while a message is being edited saves the edit.
        if self.editing.is_some() {
            self.finish_edit();
            return;
        }
        let Some(id) = self.current.clone() else {
            return;
        };
        let md = self.editor.text();
        if md.trim().is_empty() {
            self.set_status("nothing to send", true);
            return;
        }
        // The first post to a channel this session is asked about before it goes to everyone in it.
        if in_channel && !self.confirm_post(&id) {
            return;
        }
        let me = self.me.clone().unwrap_or(Me {
            id: String::new(),
            name: "You".into(),
        });
        // An `@name` that is one of these people becomes a mention; any other stays the plain text it was typed as.
        let members = self.mention_candidates().into_owned();
        let (html, mentions, ambiguous) = convert::markdown_to_html_noting(
            &md,
            &members,
            &me.id,
            self.backend.code_block_style(),
        );
        let reply_to = self.replying.take().map(|r| r.target);
        // In a channel the reply target is the post whose thread is answered, and only a new post has a title.
        let thread = in_channel
            .then(|| reply_to.as_ref().map(|t| t.id.clone()))
            .flatten();
        let subject = if in_channel && thread.is_none() {
            self.subject.take()
        } else {
            None
        };
        self.local_n += 1;
        let local_id = format!("local-{}", self.local_n);
        let now = Utc::now();
        let pending = Msg {
            root_id: thread.clone(),
            id: local_id.clone(),
            created: now,
            modified: now,
            edited: None,
            from_id: Some(me.id.clone()),
            from_name: me.name.clone(),
            system: false,
            deleted: false,
            html: true,
            body: html.clone(),
            subject: subject.clone(),
            attachments: vec![],
            reactions: vec![],
            mentions: mentions.iter().map(|m| m.user_id.clone()).collect(),
            delivery: Delivery::Sending,
        };
        let st = self.states.entry(id.clone()).or_insert_with(ChatState::new);
        st.add_pending_message(pending, md, reply_to.clone());
        if let Some(subject) = &subject {
            st.keep_pending_subject(&local_id, subject.clone());
        }
        self.editor.clear();
        self.drafts.remove(&id);
        self.sending += 1;
        let (cid, lid) = (id.clone(), local_id);
        let sent_to = cid.clone();
        let wrap = move |result| Ev::Sent {
            chat_id: sent_to,
            local_id: lid,
            result,
        };
        if in_channel {
            self.posted_channels.insert(id);
            self.spawn_action(
                move |b| {
                    let mut post = ChannelPost::new(&html).with_mentions(&mentions);
                    if let Some(subject) = subject.as_deref() {
                        post = post.with_subject(subject);
                    }
                    if let Some(root) = thread.as_deref() {
                        post = post.in_thread(root);
                    }
                    b.post_to_channel(&cid, &post)
                },
                wrap,
            );
        } else {
            self.spawn_action(
                move |b| b.send(&cid, &html, &mentions, reply_to.as_ref()),
                wrap,
            );
        }
        self.say_ambiguous_mentions(&ambiguous);
    }
}

#[cfg(test)]
mod tests;
