//! The settings that can be given in more than one place, and how they combine.
//!
//! A setting comes from the first of these that has it: a command-line flag, an environment variable, the settings file
//! (`config.toml`), the built-in default. This module is pure: it takes the flags, an environment lookup and the
//! text of the file, and returns the resolved [`Settings`] or an error that names the place the bad value came from.
//! Reading the environment and the disk is `config.rs`'s job.
//!
//! The file is TOML with one optional key per setting (see [`KEYS`]). A key it does not know is an error, so a
//! misspelt setting is noticed instead of being ignored.

use crate::bell::BellMode;
use crate::clipboard::Mode as ClipboardMode;
use crate::editor::Keymap;
use crate::gfx::{CellPx, ImageChoice};
use crate::presence::PresenceStyle;
use crate::status::AwayAfter;
use serde::Deserialize;
use std::collections::BTreeMap;

/// One setting: its name in the file, the flag and environment variable that override it, and what it is for.
pub struct Key {
    pub name: &'static str,
    pub flag: &'static str,
    pub env: &'static str,
    /// One line for the starter file.
    pub doc: &'static str,
    /// An example value, as TOML.
    pub sample: &'static str,
}

pub const KEYS: [Key; 21] = [
    Key {
        name: "backend",
        flag: "--backend",
        env: "VTTEAMS_BACKEND",
        doc: "graph (Microsoft Graph, the official API; needs an app registration your tenant allows) or teams (the unofficial Teams web API; read the disclaimer in the README before using it).",
        sample: "\"graph\"",
    },
    Key {
        name: "keymap",
        flag: "--keymap",
        env: "VTTEAMS_KEYMAP",
        doc: "standard (a text box: Shift selects, Ctrl-C/X/V/Z, Enter sends) or vim (modal editing).",
        sample: "\"standard\"",
    },
    Key {
        name: "enter",
        flag: "--enter",
        env: "VTTEAMS_ENTER",
        doc: "What Enter does in the message box with the standard keymap: send (the default; Shift/Ctrl/Alt-Enter adds a line) or newline (Enter adds a line; Shift/Ctrl/Alt-Enter sends). Ctrl-S always sends and Ctrl-J always adds a line, which is what a terminal that cannot tell Shift-Enter from Enter (over SSH, mostly) leaves. With the vim keymap Enter in insert mode adds a line and Shift/Ctrl/Alt-Enter sends.",
        sample: "\"send\"",
    },
    Key {
        name: "images",
        flag: "--images",
        env: "VTTEAMS_IMAGES",
        doc: "How pictures are drawn: auto (the default: the terminal's own graphics, Sixel, Kitty graphics or iTerm2's inline images, when the terminal is found to have them, otherwise blocks), sixel, kitty or iterm (use that protocol without checking), or characters only: blocks (every font), sextant or octant (finer, but only in a font that draws them), ascii or off. Pictures drawn with the terminal's graphics need to know how big a cell is in pixels (see cell_pixels).",
        sample: "\"auto\"",
    },
    Key {
        name: "cell_pixels",
        flag: "--cell-pixels",
        env: "VTTEAMS_CELL_PIXELS",
        doc: "The size of one character cell of the terminal in pixels, as WIDTHxHEIGHT, for pictures drawn with the terminal's graphics: auto (the default: the terminal's answer, or the window's pixel size where the system gives it) or a size such as 10x20. Where the terminal does not say, which Windows Terminal may not, 9x18 is assumed for Windows Terminal (a little smaller than its default font gives) and pictures elsewhere stay block art; set this to your font's real cell size if pictures look the wrong size.",
        sample: "\"10x20\"",
    },
    Key {
        name: "clipboard",
        flag: "--clipboard",
        env: "VTTEAMS_CLIPBOARD",
        doc: "Where copies go: auto (this computer's clipboard; the terminal's over SSH or when that fails), system, or osc52 (ask the terminal to set its clipboard, which works over SSH; tmux needs set-clipboard on).",
        sample: "\"auto\"",
    },
    Key {
        name: "favorites",
        flag: "--favorites",
        env: "VTTEAMS_FAVORITES",
        doc: "top (favourite chats are pinned above the rest under a divider; the default) or off (listed with the others, still starred). F8 toggles it while running.",
        sample: "\"top\"",
    },
    Key {
        name: "max_chats",
        flag: "--max-chats",
        env: "VTTEAMS_MAX_CHATS",
        doc: "How many recent chats to list (default 100 on Graph, 300 on the Teams web API, whose limit is 500). Favourites are listed on top of this number.",
        sample: "300",
    },
    Key {
        name: "log",
        flag: "--log",
        env: "VTTEAMS_LOG",
        doc: "How much to write to vtteams.log: off, error, warn, info, debug or trace (default: trace in a debug build, info otherwise).",
        sample: "\"info\"",
    },
    Key {
        name: "browser",
        flag: "--browser",
        env: "VTTEAMS_BROWSER",
        doc: "Teams: the browser to sign in with, as a path (default: Edge or Chrome, whichever is installed).",
        sample: "\"C:/Program Files/Google/Chrome/Application/chrome.exe\"",
    },
    Key {
        name: "client_id",
        flag: "--client-id",
        env: "VTTEAMS_CLIENT_ID",
        doc: "Graph: the Azure app (public client) id to sign in with (default: Microsoft's own, which a tenant may block).",
        sample: "\"00000000-0000-0000-0000-000000000000\"",
    },
    Key {
        name: "tenant",
        flag: "--tenant",
        env: "VTTEAMS_TENANT",
        doc: "Graph: the tenant id or domain, or organizations (the default).",
        sample: "\"organizations\"",
    },
    Key {
        name: "push",
        flag: "--push",
        env: "VTTEAMS_PUSH",
        doc: "Teams: on (default; a background connection to Teams' push channel makes the app notice new messages, reactions and edits sooner) or off (poll only).",
        sample: "\"on\"",
    },
    Key {
        name: "bell",
        flag: "--bell",
        env: "VTTEAMS_BELL",
        doc: "When to ring the terminal's bell for a new message from someone else, which most terminals turn into a sound or a flashing tab or taskbar entry: off (the default), background (only while the terminal is behind other windows; needs a terminal that reports focus, as Windows Terminal, kitty, WezTerm, iTerm2 and xterm do) or always.",
        sample: "\"off\"",
    },
    Key {
        name: "activity",
        flag: "--activity",
        env: "VTTEAMS_ACTIVITY",
        doc: "Teams: on (default) or off: tell Teams, while vtteams is running, whether you are at the keyboard, so that you show as available to other people and as away when you are idle, as the Teams app does. Off leaves your status to your other devices (vtteams then does not make you appear online). Choosing a status with F9 or :status works either way.",
        sample: "\"on\"",
    },
    Key {
        name: "away_after",
        flag: "--away-after",
        env: "VTTEAMS_AWAY_AFTER",
        doc: "Teams: how many minutes without a key before Teams shows you as away (default 5), or never to be reported as at the keyboard all the time. Only the keys pressed in vtteams count, so with the terminal open beside the work you do elsewhere, raise it or use never. :status away-after changes it for the session.",
        sample: "5",
    },
    Key {
        name: "presence",
        flag: "--presence",
        env: "VTTEAMS_PRESENCE",
        doc: "Teams: how people's availability is shown next to a 1:1 chat and in its header: emoji (the default; a coloured circle as Teams draws it), dot (a one-column coloured dot, for a terminal or font that draws emoji badly) or off (not shown, and not asked for).",
        sample: "\"emoji\"",
    },
    Key {
        name: "title",
        flag: "--title",
        env: "VTTEAMS_TITLE",
        doc: "on (default) or off: name the terminal tab vtteams (with the header's keyboard glyph) while running and put the old title back on exit (needs a terminal with a title stack, such as xterm, kitty, WezTerm, iTerm2 or GNOME Terminal).",
        sample: "\"on\"",
    },
    Key {
        name: "hyperlinks",
        flag: "--hyperlinks",
        env: "VTTEAMS_HYPERLINKS",
        doc: "on (default) or off: mark each link in a message as a terminal hyperlink (OSC 8), so Ctrl-click (or a click, as the terminal does it) opens the whole address even where the text shown is shortened or wrapped over several rows. Only web and mail addresses of plain printable characters are marked. Choose off for a terminal that prints the marks as stray characters.",
        sample: "\"on\"",
    },
    Key {
        name: "credentials",
        flag: "--credentials",
        env: "VTTEAMS_CREDENTIALS",
        doc: "How the stored sign-in is protected: auto or keychain (the default: encrypted with a key held in this computer's credential store, Windows Credential Manager, the macOS Keychain or the Linux Secret Service; vtteams stops with a message if that cannot be used, rather than writing the sign-in as readable text) or file (a plain file readable by anything running as you; choose it on a computer with no credential store, such as a Linux server with no desktop session).",
        sample: "\"auto\"",
    },
    Key {
        name: "cache",
        flag: "--cache",
        env: "VTTEAMS_CACHE",
        doc: "on (default) or off: keep an encrypted copy of the chat list and the teams list (names and who is in which chat, never any message text) so they appear at once at start-up, before Teams has answered. It is never written as readable text: it needs the same credential store key, so with credentials = file it is only kept when that store works. off also deletes an existing cache.",
        sample: "\"on\"",
    },
];

/// How the stored sign-in is protected.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CredentialsMode {
    /// Encrypted with a key in the operating system's credential store, which is used if it works (`keychain` is the same
    /// today; the two are kept apart so that `auto` can later fall back to something else without changing what
    /// `keychain` promises).
    #[default]
    Auto,
    /// Encrypted with a key in the credential store; start-up stops when that cannot be used.
    Keychain,
    /// A plain file, readable by anything running as the person.
    File,
}

impl CredentialsMode {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "keychain" => Some(Self::Keychain),
            "file" => Some(Self::File),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Keychain => "keychain",
            Self::File => "file",
        }
    }

    /// Whether the sign-in is encrypted.
    pub fn encrypts(self) -> bool {
        self != Self::File
    }
}

/// Which service to talk to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BackendKind {
    #[default]
    Graph,
    Teams,
}

impl BackendKind {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "graph" => Some(Self::Graph),
            "teams" => Some(Self::Teams),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Graph => "graph",
            Self::Teams => "teams",
        }
    }
}

/// Where a resolved value came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Flag(&'static str),
    Env(&'static str),
    File,
    Default,
}

/// A setting given on the command line: which key, the value as typed, and the flag it came from (for messages).
#[derive(Clone, Debug)]
pub struct Flag {
    pub key: &'static str,
    pub value: String,
    pub label: &'static str,
}

/// The values in a settings file, by key.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FileValues(BTreeMap<&'static str, String>);

/// The file as written. A key that is not here is an error, so misspellings are caught.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    backend: Option<String>,
    keymap: Option<String>,
    enter: Option<String>,
    images: Option<String>,
    cell_pixels: Option<String>,
    clipboard: Option<String>,
    favorites: Option<String>,
    max_chats: Option<u32>,
    log: Option<String>,
    browser: Option<String>,
    client_id: Option<String>,
    tenant: Option<String>,
    push: Option<String>,
    activity: Option<String>,
    /// Minutes as a number or as text, or `never`.
    away_after: Option<Loose>,
    bell: Option<String>,
    presence: Option<String>,
    title: Option<String>,
    hyperlinks: Option<String>,
    credentials: Option<String>,
    cache: Option<String>,
}

/// A value that may be written as a number or as text (`away_after = 10` and `away_after = "never"`).
#[derive(Deserialize)]
#[serde(untagged)]
enum Loose {
    Number(i64),
    Text(String),
}

impl Loose {
    fn into_text(self) -> String {
        match self {
            Self::Number(n) => n.to_string(),
            Self::Text(s) => s,
        }
    }
}

impl FileValues {
    /// Parse the text of a settings file. The error is TOML's own, which names the line and column.
    pub fn parse(text: &str) -> Result<Self, String> {
        let doc: Document = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut values = BTreeMap::new();
        let mut put = |key: &'static str, value: Option<String>| {
            if let Some(v) = value {
                values.insert(key, v);
            }
        };
        put("backend", doc.backend);
        put("keymap", doc.keymap);
        put("enter", doc.enter);
        put("images", doc.images);
        put("cell_pixels", doc.cell_pixels);
        put("clipboard", doc.clipboard);
        put("favorites", doc.favorites);
        put("max_chats", doc.max_chats.map(|n| n.to_string()));
        put("log", doc.log);
        put("browser", doc.browser);
        put("client_id", doc.client_id);
        put("tenant", doc.tenant);
        put("push", doc.push);
        put("activity", doc.activity);
        put("away_after", doc.away_after.map(Loose::into_text));
        put("bell", doc.bell);
        put("presence", doc.presence);
        put("title", doc.title);
        put("hyperlinks", doc.hyperlinks);
        put("credentials", doc.credentials);
        put("cache", doc.cache);
        Ok(Self(values))
    }
}

/// Everything resolved: typed values for the app, and where each came from for `vtteams config`.
#[derive(Debug)]
pub struct Settings {
    pub backend: BackendKind,
    pub keymap: Keymap,
    /// Whether a plain Enter sends in the standard keymap (otherwise it adds a line).
    pub enter_sends: bool,
    /// How pictures are drawn (see gfx::ImageChoice).
    pub images: ImageChoice,
    /// The size of a character cell in pixels, if the person gave it.
    pub cell_pixels: Option<CellPx>,
    pub clipboard: ClipboardMode,
    /// Whether favourite chats are pinned above the rest.
    pub pin_favorites: bool,
    pub max_chats: Option<usize>,
    pub log: log::LevelFilter,
    pub browser: Option<String>,
    pub client_id: Option<String>,
    pub tenant: Option<String>,
    /// Teams: whether a background connection to Trouter, Teams' push channel, makes the app notice updates
    /// sooner than polling alone would. See `AGENTS.md`, "Settings and keymaps" and `teams-client`'s
    /// `TeamsBackend::with_push`.
    pub push: bool,
    /// Teams: whether the program tells the service you are at the keyboard (and so shows you as available).
    pub activity: bool,
    /// How long without a key before you are reported as away, or never.
    pub away_after: AwayAfter,
    /// When a new message rings the terminal's bell.
    pub bell: BellMode,
    /// How availability is shown next to people (see `presence.rs`), or not at all.
    pub presence: PresenceStyle,
    /// Whether the terminal tab is titled "vtteams" while the app runs (and the old title put back afterwards).
    pub title: bool,
    /// Whether links are marked as terminal hyperlinks (OSC 8; see `hyperlink.rs`).
    pub hyperlinks: bool,
    /// How the stored sign-in is protected (see `vault.rs`).
    pub credentials: CredentialsMode,
    /// Whether the encrypted start-up cache is kept (see `cache.rs`).
    pub cache: bool,
    shown: Vec<(&'static str, String, Source)>,
}

fn default_log_level() -> log::LevelFilter {
    if cfg!(debug_assertions) {
        log::LevelFilter::Trace
    } else {
        log::LevelFilter::Info
    }
}

/// The first non-blank value for `key`: flags (the last one given wins), then the environment, then the file.
fn pick(
    key: &Key,
    flags: &[Flag],
    env: &dyn Fn(&str) -> Option<String>,
    file: &FileValues,
) -> Option<(String, Source)> {
    let nonblank = |v: &str| !v.trim().is_empty();
    if let Some(f) = flags
        .iter()
        .rev()
        .find(|f| f.key == key.name && nonblank(&f.value))
    {
        return Some((f.value.trim().to_string(), Source::Flag(f.label)));
    }
    if let Some(v) = env(key.env).filter(|v| nonblank(v)) {
        return Some((v.trim().to_string(), Source::Env(key.env)));
    }
    file.0
        .get(key.name)
        .filter(|v| nonblank(v))
        .map(|v| (v.trim().to_string(), Source::File))
}

/// Resolve every setting. `file_name` names the settings file in error messages.
pub fn resolve(
    flags: &[Flag],
    env: &dyn Fn(&str) -> Option<String>,
    file: &FileValues,
    file_name: &str,
) -> Result<Settings, String> {
    let mut s = Settings {
        backend: BackendKind::default(),
        keymap: Keymap::default(),
        enter_sends: true,
        images: ImageChoice::DEFAULT,
        cell_pixels: None,
        clipboard: ClipboardMode::default(),
        pin_favorites: true,
        max_chats: None,
        log: default_log_level(),
        browser: None,
        client_id: None,
        tenant: None,
        push: true,
        activity: true,
        away_after: AwayAfter::default(),
        bell: BellMode::default(),
        presence: PresenceStyle::default(),
        title: true,
        hyperlinks: true,
        credentials: CredentialsMode::default(),
        cache: true,
        shown: Vec::new(),
    };
    for key in &KEYS {
        let Some((value, source)) = pick(key, flags, env, file) else {
            let shown = match key.name {
                "backend" => s.backend.label().into(),
                "keymap" => s.keymap.label().into(),
                "enter" => "send".into(),
                "images" => ImageChoice::DEFAULT.label().into(),
                "cell_pixels" => format!(
                    "auto (what the terminal says; {} is assumed in Windows Terminal when it says nothing)",
                    crate::gfx::ASSUMED_WINDOWS_TERMINAL_CELL.label()
                ),
                "clipboard" => s.clipboard.label().into(),
                "favorites" => "top".into(),
                "max_chats" => "(100 on Graph, 300 on the Teams web API)".into(),
                "log" => s.log.to_string().to_ascii_lowercase(),
                "browser" => "(Edge or Chrome, whichever is installed)".into(),
                "client_id" => "(Microsoft's own public client)".into(),
                "push" | "title" | "hyperlinks" | "cache" => "on".into(),
                "credentials" => s.credentials.label().into(),
                "presence" => s.presence.label().into(),
                "bell" => s.bell.label().into(),
                "activity" => "on".into(),
                "away_after" => AwayAfter::default().label(),
                "tenant" => "(organizations)".into(),
                // A setting added to `KEYS` without a line here says so, rather than showing another one's default
                // (a test checks that none does).
                _ => "(not set)".into(),
            };
            s.shown.push((key.name, shown, Source::Default));
            continue;
        };
        let bad = |hint: &str| match &source {
            Source::Flag(flag) => format!("bad {flag} '{value}' ({hint})"),
            Source::Env(var) => format!("bad {var} '{value}' in the environment ({hint})"),
            Source::File | Source::Default => {
                format!("bad {} '{value}' in {file_name} ({hint})", key.name)
            }
        };
        match key.name {
            "backend" => {
                s.backend = BackendKind::parse(&value).ok_or_else(|| bad("use graph or teams"))?
            }
            "keymap" => {
                s.keymap = Keymap::parse(&value).ok_or_else(|| bad("use vim or standard"))?
            }
            "enter" => {
                s.enter_sends = match value.to_ascii_lowercase().as_str() {
                    "send" => true,
                    "newline" | "line" => false,
                    _ => return Err(bad("use send or newline")),
                };
            }
            "images" => {
                s.images = ImageChoice::parse(&value).ok_or_else(|| {
                    bad("use auto, sixel, kitty, iterm, octant, sextant, blocks, ascii or off")
                })?;
            }
            "cell_pixels" => {
                s.cell_pixels = if value.eq_ignore_ascii_case("auto") {
                    None
                } else {
                    Some(CellPx::parse(&value).ok_or_else(|| {
                        bad("use auto, or a size such as 10x20: the width and height of a cell in pixels")
                    })?)
                };
            }
            "clipboard" => {
                s.clipboard =
                    ClipboardMode::parse(&value).ok_or_else(|| bad("use auto, system or osc52"))?;
            }
            "favorites" => {
                s.pin_favorites = match value.to_ascii_lowercase().as_str() {
                    "top" | "pinned" | "on" => true,
                    "off" | "none" | "list" => false,
                    _ => return Err(bad("use top or off")),
                };
            }
            "max_chats" => {
                let n = value.parse().ok().filter(|n| *n > 0);
                s.max_chats = Some(n.ok_or_else(|| bad("use a whole number above 0"))?);
            }
            "log" => {
                s.log = value
                    .parse()
                    .map_err(|_| bad("use off, error, warn, info, debug or trace"))?;
            }
            "browser" => s.browser = Some(value.clone()),
            "client_id" => s.client_id = Some(value.clone()),
            "tenant" => s.tenant = Some(value.clone()),
            "push" => {
                s.push = match value.to_ascii_lowercase().as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return Err(bad("use on or off")),
                };
            }
            "activity" => {
                s.activity = match value.to_ascii_lowercase().as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return Err(bad("use on or off")),
                };
            }
            "away_after" => {
                s.away_after = AwayAfter::parse(&value)
                    .ok_or_else(|| bad("use a number of minutes from 1 to 1440, or never"))?;
            }
            "bell" => {
                s.bell =
                    BellMode::parse(&value).ok_or_else(|| bad("use off, background or always"))?;
            }
            "presence" => {
                s.presence =
                    PresenceStyle::parse(&value).ok_or_else(|| bad("use emoji, dot or off"))?;
            }
            "title" => {
                s.title = match value.to_ascii_lowercase().as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return Err(bad("use on or off")),
                };
            }
            "hyperlinks" => {
                s.hyperlinks = match value.to_ascii_lowercase().as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return Err(bad("use on or off")),
                };
            }
            "cache" => {
                s.cache = match value.to_ascii_lowercase().as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return Err(bad("use on or off")),
                };
            }
            "credentials" => {
                s.credentials = CredentialsMode::parse(&value)
                    .ok_or_else(|| bad("use auto, keychain or file"))?;
            }
            // The names come from the static `KEYS` table, never from the user; a setting without a case would
            // otherwise be read, listed and silently ignored. A test resolves every setting, so it cannot get here.
            other => unreachable!("the setting {other:?} has no case in resolve"),
        }
        s.shown.push((key.name, value, source));
    }
    Ok(s)
}

impl Settings {
    /// The effective settings, one per line, with where each came from: what `vtteams config` prints.
    pub fn describe(&self, file_name: &str) -> String {
        let width = KEYS.iter().map(|k| k.name.len()).max().unwrap_or(0);
        let value_width = self
            .shown
            .iter()
            .map(|(_, v, _)| v.chars().count())
            .max()
            .unwrap_or(0);
        let mut out = String::new();
        for (name, value, source) in &self.shown {
            let from = match source {
                Source::Flag(flag) => (*flag).to_string(),
                Source::Env(var) => format!("{var} in the environment"),
                Source::File => file_name.to_string(),
                Source::Default => "default".to_string(),
            };
            out.push_str(&format!(
                "  {name:<width$}  {value:<value_width$}  [{from}]\n"
            ));
        }
        out
    }
}

/// The starter file `vtteams config init` writes: every setting, commented out, with what it does.
pub fn sample() -> String {
    let mut out = String::from(
        "# vtteams settings.\n\
         #\n\
         # Every line is optional and commented out. Remove the # in front of a setting to use it.\n\
         # Command-line flags and VTTEAMS_* environment variables override this file; run `vtteams config` to see\n\
         # what is in effect and where each value comes from.\n\n",
    );
    for key in &KEYS {
        out.push_str(&format!("# {}\n", key.doc));
        out.push_str(&format!(
            "# {} = {}    # flag {}, variable {}\n\n",
            key.name, key.sample, key.flag, key.env
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imgart::ImgMode;

    fn none(_: &str) -> Option<String> {
        None
    }

    fn flag(key: &'static str, value: &str, label: &'static str) -> Flag {
        Flag {
            key,
            value: value.into(),
            label,
        }
    }

    fn file(text: &str) -> FileValues {
        FileValues::parse(text).unwrap()
    }

    fn resolved(flags: &[Flag], env: &[(&str, &str)], text: &str) -> Result<Settings, String> {
        let env = |name: &str| {
            env.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        };
        resolve(flags, &env, &file(text), "config.toml")
    }

    #[test]
    fn a_flag_beats_the_environment_which_beats_the_file_which_beats_the_default() {
        let s = resolved(&[], &[], "").unwrap();
        assert_eq!(s.keymap, Keymap::Standard, "the default");
        assert!(s.enter_sends, "Enter sends unless told otherwise");
        assert!(
            !resolved(&[], &[("VTTEAMS_ENTER", "newline")], "")
                .unwrap()
                .enter_sends
        );
        assert!(
            !resolved(&[], &[], "enter = \"newline\"")
                .unwrap()
                .enter_sends
        );
        let e = resolved(&[flag("enter", "never", "--enter")], &[], "").unwrap_err();
        assert_eq!(e, "bad --enter 'never' (use send or newline)");
        let s = resolved(&[], &[], "keymap = \"vim\"").unwrap();
        assert_eq!(s.keymap, Keymap::Vim, "the file");
        let s = resolved(&[], &[("VTTEAMS_KEYMAP", "standard")], "keymap = \"vim\"").unwrap();
        assert_eq!(s.keymap, Keymap::Standard, "the environment");
        let flags = [flag("keymap", "vim", "--keymap")];
        let s = resolved(
            &flags,
            &[("VTTEAMS_KEYMAP", "standard")],
            "keymap = \"standard\"",
        )
        .unwrap();
        assert_eq!(s.keymap, Keymap::Vim, "a flag");
    }

    #[test]
    fn blank_values_do_not_count_as_given() {
        let flags = [flag("keymap", "  ", "--keymap")];
        let s = resolved(&flags, &[("VTTEAMS_KEYMAP", "")], "keymap = \"vim\"").unwrap();
        assert_eq!(
            s.keymap,
            Keymap::Vim,
            "blank flag and variable fall through to the file"
        );
        let s = resolved(&[], &[], "browser = \"\"").unwrap();
        assert_eq!(s.browser, None);
    }

    #[test]
    fn the_last_flag_for_a_key_wins() {
        let flags = [
            flag("backend", "teams", "--teams"),
            flag("backend", "graph", "--backend"),
        ];
        assert_eq!(
            resolved(&flags, &[], "").unwrap().backend,
            BackendKind::Graph
        );
    }

    #[test]
    fn every_setting_is_read_and_typed() {
        let s = resolved(
            &[],
            &[],
            r#"
            backend = "Teams"
            keymap = "STANDARD"
            images = "sextant"
            max_chats = 50
            log = "debug"
            browser = "/usr/bin/chromium"
            client_id = "abc"
            tenant = "contoso.com"
            "#,
        )
        .unwrap();
        assert_eq!(s.backend, BackendKind::Teams);
        assert_eq!(s.keymap, Keymap::Standard);
        assert_eq!(s.images, ImageChoice::Art(ImgMode::Sextant));
        assert_eq!(s.max_chats, Some(50));
        assert_eq!(s.log, log::LevelFilter::Debug);
        assert_eq!(s.browser.as_deref(), Some("/usr/bin/chromium"));
        assert_eq!(s.client_id.as_deref(), Some("abc"));
        assert_eq!(s.tenant.as_deref(), Some("contoso.com"));
    }

    #[test]
    fn pictures_are_auto_by_default_and_a_protocol_or_art_can_be_named() {
        use crate::gfx::Protocol;
        let s = resolved(&[], &[], "").unwrap();
        assert_eq!(s.images, ImageChoice::Auto, "the default");
        assert_eq!(s.cell_pixels, None);
        for (word, expected) in [
            ("auto", ImageChoice::Auto),
            ("sixel", ImageChoice::Protocol(Protocol::Sixel)),
            ("kitty", ImageChoice::Protocol(Protocol::Kitty)),
            ("iterm", ImageChoice::Protocol(Protocol::Iterm)),
            ("blocks", ImageChoice::Art(ImgMode::Blocks)),
            ("octant", ImageChoice::Art(ImgMode::Octant)),
            ("off", ImageChoice::Art(ImgMode::Off)),
        ] {
            let in_file = resolved(&[], &[], &format!("images = \"{word}\"")).unwrap();
            assert_eq!(in_file.images, expected, "{word} from the file");
            let from_env = resolved(&[], &[("VTTEAMS_IMAGES", word)], "images = \"off\"").unwrap();
            assert_eq!(from_env.images, expected, "{word} from the environment");
            let from_flag = resolved(
                &[flag("images", word, "--images")],
                &[("VTTEAMS_IMAGES", "off")],
                "",
            )
            .unwrap();
            assert_eq!(from_flag.images, expected, "{word} from the flag");
        }
        let e = resolved(&[flag("images", "png", "--images")], &[], "").unwrap_err();
        assert_eq!(
            e,
            "bad --images 'png' (use auto, sixel, kitty, iterm, octant, sextant, blocks, ascii or off)"
        );
    }

    #[test]
    fn the_cell_size_is_auto_or_width_x_height() {
        assert_eq!(
            resolved(&[], &[], "cell_pixels = \"10x20\"")
                .unwrap()
                .cell_pixels,
            CellPx::new(10, 20)
        );
        assert_eq!(
            resolved(
                &[],
                &[("VTTEAMS_CELL_PIXELS", "9x19")],
                "cell_pixels = \"10x20\""
            )
            .unwrap()
            .cell_pixels,
            CellPx::new(9, 19),
            "the environment beats the file"
        );
        assert_eq!(
            resolved(&[], &[], "cell_pixels = \"auto\"")
                .unwrap()
                .cell_pixels,
            None
        );
        for bad in ["10", "10x", "tall", "1x1", "10x20x30", "0x0"] {
            let e = resolved(&[], &[], &format!("cell_pixels = \"{bad}\"")).unwrap_err();
            assert!(
                e.starts_with(&format!(
                    "bad cell_pixels '{bad}' in config.toml (use auto, or a size such as 10x20"
                )),
                "{e}"
            );
        }
    }

    #[test]
    fn every_setting_is_handled_by_resolve_and_shows_a_default_of_its_own() {
        // Giving every setting its sample value from a flag runs every arm of the second `match` in `resolve`
        // (one without a case panics there), and reading it back shows the flag as its source.
        let flags: Vec<Flag> = KEYS
            .iter()
            .map(|k| flag(k.name, k.sample.trim_matches('"'), k.flag))
            .collect();
        let given = resolved(&flags, &[], "").unwrap().describe("config.toml");
        for key in &KEYS {
            let line = given
                .lines()
                .find(|l| l.split_whitespace().next() == Some(key.name))
                .unwrap_or_else(|| panic!("no line for {}", key.name));
            assert!(line.contains(&format!("[{}]", key.flag)), "{line}");
        }
        // With nothing given, no setting shows another one's default, or none at all.
        let defaults = resolved(&[], &[], "").unwrap().describe("config.toml");
        for key in &KEYS {
            let line = defaults
                .lines()
                .find(|l| l.split_whitespace().next() == Some(key.name))
                .unwrap_or_else(|| panic!("no line for {}", key.name));
            assert!(line.contains("[default]"), "{line}");
            assert!(
                !line.contains("(not set)"),
                "no default text for {}: {line}",
                key.name
            );
            assert_eq!(
                line.contains("(organizations)"),
                key.name == "tenant",
                "{line}"
            );
        }
    }

    #[test]
    fn the_away_timeout_is_five_minutes_and_takes_minutes_as_a_number_or_text_or_never() {
        use crate::status::AwayAfter::{Minutes, Never};
        let key = KEYS.iter().find(|k| k.name == "away_after").unwrap();
        assert_eq!(resolved(&[], &[], "").unwrap().away_after, Minutes(5));
        assert_eq!(
            resolved(&[], &[], "away_after = 10").unwrap().away_after,
            Minutes(10),
            "a number in the file"
        );
        assert_eq!(
            resolved(&[], &[], "away_after = \"never\"")
                .unwrap()
                .away_after,
            Never
        );
        assert_eq!(
            resolved(&[], &[(key.env, "20")], "away_after = 10")
                .unwrap()
                .away_after,
            Minutes(20),
            "the environment beats the file"
        );
        assert_eq!(
            resolved(
                &[flag("away_after", "never", key.flag)],
                &[(key.env, "20")],
                ""
            )
            .unwrap()
            .away_after,
            Never,
            "a flag beats the environment"
        );
        let e = resolved(&[flag("away_after", "0", key.flag)], &[], "").unwrap_err();
        assert_eq!(
            e,
            format!(
                "bad {} '0' (use a number of minutes from 1 to 1440, or never)",
                key.flag
            )
        );
    }

    #[test]
    fn the_bell_is_off_unless_asked_for_and_takes_off_background_or_always() {
        use crate::bell::BellMode::{Always, Background, Off};
        let key = KEYS.iter().find(|k| k.name == "bell").unwrap();
        assert_eq!(resolved(&[], &[], "").unwrap().bell, Off);
        assert_eq!(
            resolved(&[], &[], "bell = \"background\"").unwrap().bell,
            Background
        );
        assert_eq!(
            resolved(&[], &[(key.env, "ALWAYS")], "bell = \"background\"")
                .unwrap()
                .bell,
            Always,
            "the environment beats the file, and case does not matter"
        );
        assert_eq!(
            resolved(&[flag("bell", "off", key.flag)], &[(key.env, "always")], "")
                .unwrap()
                .bell,
            Off,
            "a flag beats the environment"
        );
        let e = resolved(&[flag("bell", "loud", key.flag)], &[], "").unwrap_err();
        assert_eq!(
            e,
            format!("bad {} 'loud' (use off, background or always)", key.flag)
        );
    }

    #[test]
    fn presence_defaults_to_emoji_and_takes_emoji_dot_or_off() {
        use crate::presence::PresenceStyle::{Dot, Emoji, Off};
        let key = KEYS.iter().find(|k| k.name == "presence").unwrap();
        assert_eq!(resolved(&[], &[], "").unwrap().presence, Emoji);
        assert_eq!(
            resolved(&[], &[], "presence = \"dot\"").unwrap().presence,
            Dot
        );
        assert_eq!(
            resolved(&[], &[(key.env, "OFF")], "presence = \"dot\"")
                .unwrap()
                .presence,
            Off,
            "the environment beats the file, and case does not matter"
        );
        assert_eq!(
            resolved(
                &[flag("presence", "emoji", key.flag)],
                &[(key.env, "off")],
                ""
            )
            .unwrap()
            .presence,
            Emoji,
            "a flag beats the environment"
        );
        let e = resolved(&[flag("presence", "sometimes", key.flag)], &[], "").unwrap_err();
        assert_eq!(
            e,
            format!("bad {} 'sometimes' (use emoji, dot or off)", key.flag)
        );
    }
    #[test]
    fn the_on_off_settings_follow_flag_environment_and_file_and_refuse_anything_else() {
        type Read = fn(&Settings) -> bool;
        let on_off: [(&str, Read); 5] = [
            ("push", |s| s.push),
            ("title", |s| s.title),
            ("hyperlinks", |s| s.hyperlinks),
            ("activity", |s| s.activity),
            ("cache", |s| s.cache),
        ];
        for (name, read) in on_off {
            let key = KEYS.iter().find(|k| k.name == name).unwrap();
            assert!(
                read(&resolved(&[], &[], "").unwrap()),
                "{name} is on unless told otherwise"
            );
            let off_in_file = format!("{name} = \"off\"");
            assert!(
                !read(&resolved(&[], &[], &off_in_file).unwrap()),
                "{name}: file"
            );
            assert!(
                read(&resolved(&[], &[(key.env, "ON")], &off_in_file).unwrap()),
                "{name}: the environment beats the file, and case does not matter"
            );
            let off_flag = [flag(name, "off", key.flag)];
            assert!(
                !read(&resolved(&off_flag, &[(key.env, "on")], "").unwrap()),
                "{name}: a flag beats the environment"
            );
            let e = resolved(&[flag(name, "maybe", key.flag)], &[], "").unwrap_err();
            assert_eq!(e, format!("bad {} 'maybe' (use on or off)", key.flag));
            let e = resolved(&[], &[(key.env, "1")], "").unwrap_err();
            assert!(e.contains(key.env) && e.contains("use on or off"), "{e}");
        }
    }

    #[test]
    fn credentials_are_encrypted_unless_the_file_is_asked_for_and_the_value_is_checked() {
        use CredentialsMode::{Auto, File, Keychain};
        let key = KEYS.iter().find(|k| k.name == "credentials").unwrap();
        assert_eq!(resolved(&[], &[], "").unwrap().credentials, Auto);
        assert!(Auto.encrypts() && Keychain.encrypts() && !File.encrypts());
        assert_eq!(
            resolved(&[], &[], "credentials = \"file\"")
                .unwrap()
                .credentials,
            File
        );
        assert_eq!(
            resolved(&[], &[(key.env, "KEYCHAIN")], "credentials = \"file\"")
                .unwrap()
                .credentials,
            Keychain,
            "the environment beats the file, and case does not matter"
        );
        assert_eq!(
            resolved(
                &[flag("credentials", "file", key.flag)],
                &[(key.env, "auto")],
                ""
            )
            .unwrap()
            .credentials,
            File,
            "a flag beats the environment"
        );
        let e = resolved(&[flag("credentials", "plain", key.flag)], &[], "").unwrap_err();
        assert_eq!(
            e,
            format!("bad {} 'plain' (use auto, keychain or file)", key.flag)
        );
        let shown = resolved(&[], &[], "").unwrap().describe("config.toml");
        let line = shown
            .lines()
            .find(|l| l.trim_start().starts_with("credentials"))
            .unwrap();
        assert!(
            line.contains("auto") && line.contains("[default]"),
            "{line}"
        );
        let line = shown
            .lines()
            .find(|l| l.trim_start().starts_with("cache"))
            .unwrap();
        assert!(line.contains("on") && line.contains("[default]"), "{line}");
    }

    #[test]
    fn the_clipboard_setting_takes_its_three_modes_and_refuses_others() {
        for given in ["auto", "system", "osc52"] {
            let s = resolved(&[], &[("VTTEAMS_CLIPBOARD", given)], "").unwrap();
            assert_eq!(s.clipboard.label(), given);
        }
        let e = resolved(&[], &[("VTTEAMS_CLIPBOARD", "paper")], "").unwrap_err();
        assert!(e.contains("use auto, system or osc52"), "{e}");
    }

    #[test]
    fn a_bad_value_names_where_it_came_from() {
        let e = resolved(&[flag("keymap", "emacs", "--keymap")], &[], "").unwrap_err();
        assert_eq!(e, "bad --keymap 'emacs' (use vim or standard)");
        let e = resolved(&[], &[("VTTEAMS_LOG", "loud")], "").unwrap_err();
        assert_eq!(
            e,
            "bad VTTEAMS_LOG 'loud' in the environment (use off, error, warn, info, debug or trace)"
        );
        let e = resolved(&[], &[], "images = \"fancy\"").unwrap_err();
        assert!(e.starts_with("bad images 'fancy' in config.toml"), "{e}");
        let e = resolved(&[], &[("VTTEAMS_MAX_CHATS", "0")], "").unwrap_err();
        assert!(
            e.contains("VTTEAMS_MAX_CHATS") && e.contains("above 0"),
            "{e}"
        );
        let e = resolved(&[], &[("VTTEAMS_MAX_CHATS", "many")], "").unwrap_err();
        assert!(e.contains("many"), "{e}");
        let e = resolved(&[], &[("VTTEAMS_BACKEND", "gopher")], "").unwrap_err();
        assert!(e.contains("graph or teams"), "{e}");
    }

    #[test]
    fn a_value_overridden_from_a_stronger_place_is_not_checked() {
        let s = resolved(
            &[flag("keymap", "vim", "--keymap")],
            &[],
            "keymap = \"nonsense\"",
        )
        .unwrap();
        assert_eq!(s.keymap, Keymap::Vim);
    }

    #[test]
    fn a_misspelt_or_mistyped_setting_is_an_error_that_points_at_it() {
        let e = FileValues::parse("keymap = \"vim\"\nkeymapp = \"standard\"\n").unwrap_err();
        assert!(e.contains("keymapp") && e.contains("line 2"), "{e}");
        let e = FileValues::parse("keymap = 3").unwrap_err();
        assert!(e.contains("line 1") && e.contains("string"), "{e}");
        let e = FileValues::parse("max_chats = \"many\"").unwrap_err();
        assert!(e.contains("max_chats") || e.contains("line 1"), "{e}");
        let e = FileValues::parse("keymap = ").unwrap_err();
        assert!(e.contains("line 1"), "{e}");
        assert!(FileValues::parse("").unwrap().0.is_empty());
        assert!(
            FileValues::parse("# only a comment\n")
                .unwrap()
                .0
                .is_empty()
        );
    }

    #[test]
    fn favourites_are_pinned_unless_told_otherwise_and_the_value_is_checked() {
        assert!(resolved(&[], &[], "").unwrap().pin_favorites);
        for (given, want) in [
            ("top", true),
            ("pinned", true),
            ("OFF", false),
            ("list", false),
        ] {
            let s = resolved(&[], &[("VTTEAMS_FAVORITES", given)], "").unwrap();
            assert_eq!(s.pin_favorites, want, "{given}");
        }
        assert!(
            !resolved(&[], &[], "favorites = \"off\"")
                .unwrap()
                .pin_favorites
        );
        let e = resolved(&[], &[("VTTEAMS_FAVORITES", "bottom")], "").unwrap_err();
        assert!(
            e.contains("VTTEAMS_FAVORITES") && e.contains("use top or off"),
            "{e}"
        );
    }

    #[test]
    fn the_starter_file_is_valid_and_uncommenting_it_gives_valid_settings() {
        let text = sample();
        assert_eq!(
            FileValues::parse(&text).unwrap(),
            FileValues::default(),
            "all commented out"
        );
        let live: String = text
            .lines()
            .map(|l| match l.strip_prefix("# ") {
                Some(rest)
                    if KEYS
                        .iter()
                        .any(|k| rest.starts_with(&format!("{} = ", k.name))) =>
                {
                    rest.to_string()
                }
                _ => l.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let values = FileValues::parse(&live).unwrap();
        assert_eq!(
            values.0.len(),
            KEYS.len(),
            "every setting is in the starter file"
        );
        resolve(&[], &none, &values, "config.toml").unwrap();
        for key in &KEYS {
            assert!(
                text.contains(key.flag) && text.contains(key.env),
                "{}",
                key.name
            );
            assert!(key.flag.starts_with("--") && key.env.starts_with("VTTEAMS_"));
        }
    }

    #[test]
    fn config_shows_each_value_with_its_source() {
        let flags = [flag("keymap", "standard", "--keymap")];
        let s = resolved(&flags, &[("VTTEAMS_LOG", "warn")], "images = \"blocks\"").unwrap();
        let text = s.describe("config.toml");
        let line = |name: &str| {
            text.lines()
                .find(|l| l.trim_start().starts_with(name))
                .unwrap_or_else(|| panic!("no line for {name} in\n{text}"))
                .to_string()
        };
        assert!(line("keymap").contains("standard") && line("keymap").contains("[--keymap]"));
        assert!(
            line("log").contains("warn")
                && line("log").contains("[VTTEAMS_LOG in the environment]")
        );
        assert!(line("images").contains("blocks") && line("images").contains("[config.toml]"));
        assert!(line("backend").contains("graph") && line("backend").contains("[default]"));
        assert_eq!(text.lines().count(), KEYS.len());
    }
}
