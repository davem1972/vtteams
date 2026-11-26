mod app;
mod banner;
mod bell;
mod cache;
mod cadence;
mod clipboard;
mod config;
mod convert;
mod demo;
mod diag;
mod editor;
mod emoji;
mod fence;
mod gfx;
mod highlight;
mod hyperlink;
mod imgart;
mod inline_style;
mod opener;
mod output;
mod paste;
mod presence;
mod render;
mod settings;
mod sign_in_modal;
mod status;
mod termtext;
#[cfg(test)]
mod testdir;
mod theme;
mod title;
mod transcript;
mod ui;
mod vault;
mod view;
mod workers;

use app::{App, describe};
use clipboard::Clipboard as _;
use editor::Mode;
use ratatui::crossterm::cursor::{SetCursorStyle, Show};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::supports_keyboard_enhancement;
use std::sync::Arc;
use std::time::{Duration, Instant};
use teams_client::Backend;
use teams_client::graph::{DeviceCodePrompt, GraphAuth, GraphBackend, GraphTokens};
use teams_client::teams::{self, TeamsBackend, TeamsSession};

const HELP: &str = "\
vtteams - a rich terminal UI for Microsoft Teams chats (Microsoft Graph, or with --teams the unofficial Teams web API)

USAGE:
    vtteams [OPTIONS]       start the UI (signs in on first run)
    vtteams login           sign in (Graph: device code) and exit
    vtteams logout          forget the stored sign-ins, the encryption key, the cache and the Teams browser profile
    vtteams config          show the settings in effect and where each came from
    vtteams config init     write a commented starter settings file (never replaces one)

OPTIONS:
    --demo                  use built-in demo conversations; no account needed
    --backend <SERVICE>     graph (default, the official Microsoft Graph API) or teams (unofficial, see --teams)   [env VTTEAMS_BACKEND]
    --teams                 same as --backend teams: the UNOFFICIAL Teams web API (not sanctioned by Microsoft; see the
                            disclaimer in the README before using it)
    --browser <PATH>        browser used for the Teams sign-in   [env VTTEAMS_BROWSER]
    --client-id <ID>        Azure app (public client) id (Graph)   [env VTTEAMS_CLIENT_ID]
    --tenant <TENANT>       tenant id/domain, or 'organizations' (Graph)  [env VTTEAMS_TENANT]
    --images <MODE>         auto (default: the terminal's own graphics, Sixel, Kitty graphics or iTerm2 inline images, where
                            the terminal has them, else blocks), sixel, kitty, iterm (use that protocol without checking),
                            blocks (every font draws it), sextant, octant (finest; needs a font with Unicode 16 octants),
                            ascii, off   [env VTTEAMS_IMAGES]
    --cell-pixels <WxH>     the size of a terminal cell in pixels, such as 10x20, for pictures drawn with graphics; auto
                            (default) asks the terminal   [env VTTEAMS_CELL_PIXELS]
    --terminal-report       ask the terminal what it can draw (the same questions as start-up, then a few more), print
                            what the environment said, what was sent and every byte that came back (escaped), and the
                            decision, then exit; for finding out why pictures are block art
    --clipboard <MODE>      where copies go: auto (default; this computer's clipboard, the terminal's over SSH or when that
                            fails), system, or osc52 (the terminal's, which works over SSH; tmux needs set-clipboard on)
                            [env VTTEAMS_CLIPBOARD]
    --favorites <MODE>      top (default: favourite chats pinned above the rest) or off   [env VTTEAMS_FAVORITES]
    --max-chats <N>         how many recent chats to list   [env VTTEAMS_MAX_CHATS; default 100 on Graph, 300 on Teams web (at most 500);
                            favourites are listed on top of this]
    --push <MODE>           Teams: on (default; a background connection to the push channel makes the app notice
                            new messages, reactions and edits sooner) or off (poll only)   [env VTTEAMS_PUSH]
    --activity <MODE>       Teams: on (default) or off: tell Teams while vtteams runs whether you are at the keyboard, so you
                            show as available (and as away when idle) as in the Teams app   [env VTTEAMS_ACTIVITY]
    --away-after <MINUTES>  Teams: minutes without a key before you show as away (default 5), or never   [env VTTEAMS_AWAY_AFTER]
    --bell <WHEN>           when a new message from someone else rings the terminal's bell (a sound, or a flashing tab or
                            taskbar entry, as the terminal does it): off (default), background (only while the terminal is
                            behind other windows) or always   [env VTTEAMS_BELL]
    --presence <STYLE>      Teams: how availability is shown next to 1:1 chats and in their header: emoji (default; a
                            coloured circle as Teams draws it), dot (a one-column coloured dot) or off   [env VTTEAMS_PRESENCE]
    --title <MODE>          on (default) or off: name the terminal tab vtteams while running and put the old title back
                            on exit (needs a terminal with a title stack)   [env VTTEAMS_TITLE]
    --hyperlinks <MODE>     on (default) or off: mark links as terminal hyperlinks (OSC 8), so Ctrl-click opens the whole
                            address even when the text shown is shortened or wrapped   [env VTTEAMS_HYPERLINKS]
    --credentials <MODE>    auto or keychain (default): keep the stored sign-in encrypted with a key held in this computer's
                            credential store, and stop with a message if that store cannot be used; file: a plain file,
                            readable by anything running as you (for a computer with no credential store)   [env VTTEAMS_CREDENTIALS]
    --cache <MODE>          on (default) or off: keep an encrypted copy of the chat and teams lists so they appear at once at
                            start-up (never any message text; off also deletes it)   [env VTTEAMS_CACHE]
    --record-shapes <DIR>   write the structure of every API exchange (content masked, secrets redacted) to DIR,
                            to show what a service really sends   [env VTTEAMS_RECORD_SHAPES]
    --log-headers <NAMES>   log the response headers whose names match, one line per response, to find out what a
                            service says about its connections: comma-separated, case-insensitive, `*` matches any
                            run of characters, `!name` excludes (e.g. `connection,keep-alive,via,x-ms*,!x-ms-latency`).
                            Needs --log info or more; credentials are never logged   [env VTTEAMS_LOG_HEADERS]
    --keymap <KEYS>         standard (default): type directly, Shift selects, Ctrl-C/X/V/Z copy/cut/paste/undo, Enter
                            sends, F1 shows the keys; vim: modal editing as in vim   [env VTTEAMS_KEYMAP]
    --enter <WHAT>          send (default) or newline: what Enter does in the standard keymap's message box; the other way
                            of pressing it (Shift/Ctrl/Alt-Enter) does the opposite; Ctrl-S always sends   [env VTTEAMS_ENTER]
    --log <LEVEL>           how much to write to vtteams.log in the settings directory: off, error, warn, info,
                            debug or trace (default: trace in a debug build, info otherwise)   [env VTTEAMS_LOG]
    --config <PATH>         read settings from this file instead of config.toml in the settings directory
                            [env VTTEAMS_CONFIG]
    --help                  show this help
    --version               show the version and exit

Most options can also be set in config.toml (run `vtteams config init` for a commented starter file, `vtteams config` to see
what is in effect): --backend, --keymap, --enter, --images, --cell-pixels, --clipboard, --favorites, --max-chats, --push, --activity, --away-after, --bell, --presence, --title, --hyperlinks, --credentials, --cache, --log, --browser,
--client-id and --tenant. A flag beats its VTTEAMS_* variable, which beats the file, which beats the default.

NON-INTERACTIVE (for scripts and checks):
    --dump                  render one frame as plain text to stdout and exit
    --ansi                  with --dump: emit 24-bit colour escapes (cat it in a truecolor terminal)
    --width <N> --height <N>    size of the dumped frame (default 130 x 46)
    --chat <N|text>         select chat number N (1-based) or the first whose title contains text
    --keys <KEYS>           feed keys first: plain characters plus <Enter> <Esc> <Tab> <BS> <Space> <Up> <Down> <Left> <Right>
                            <Home> <End> <PgUp> <PgDn> <Del> <F1>.., with C-, S-, A- prefixes: <C-s> <S-Enter> <C-S-Left>

Press F1 inside the app for the key reference (? also works in the vim keymap).
";

#[derive(Default)]
struct Args {
    command: Option<String>,
    demo: bool,
    backend: Option<String>,
    /// The flag that set `backend`, for messages: `--backend`, or `--teams`.
    backend_flag: &'static str,
    browser: Option<String>,
    client_id: Option<String>,
    tenant: Option<String>,
    images: Option<String>,
    cell_pixels: Option<String>,
    clipboard: Option<String>,
    favorites: Option<String>,
    max_chats: Option<String>,
    push: Option<String>,
    presence: Option<String>,
    activity: Option<String>,
    away_after: Option<String>,
    bell: Option<String>,
    title: Option<String>,
    hyperlinks: Option<String>,
    credentials: Option<String>,
    cache: Option<String>,
    config: Option<String>,
    config_init: bool,
    record_shapes: Option<String>,
    log_headers: Option<String>,
    log: Option<String>,
    keymap: Option<String>,
    enter: Option<String>,
    dump: bool,
    terminal_report: bool,
    ansi: bool,
    width: Option<u16>,
    height: Option<u16>,
    chat: Option<String>,
    keys: Option<String>,
}

/// What the command line asks for.
enum Command {
    Help,
    Version,
    Run(Box<Args>),
}

fn parse_args() -> Result<Command, String> {
    let args = std::env::args_os()
        .skip(1)
        .map(|a| {
            a.into_string()
                .map_err(|_| "arguments must be valid Unicode".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    parse_args_from(args)
}

/// Frames larger than this are not drawn (a mistyped size would otherwise ask for billions of cells).
const MAX_DUMP_SIDE: u16 = 1_000;

fn parse_args_from(argv: Vec<String>) -> Result<Command, String> {
    let mut args = Args {
        backend_flag: "--backend",
        ..Args::default()
    };
    let mut it = argv.into_iter();
    // A value is the next word, unless that is another option (`--browser --teams` forgot the path; it did not mean
    // a browser called "--teams").
    let value = |it: &mut dyn Iterator<Item = String>, flag: &str| {
        it.next()
            .filter(|v| !v.starts_with("--"))
            .ok_or(format!("{flag} needs a value"))
    };
    let side = |text: String, flag: &str| {
        text.parse::<u16>()
            .ok()
            .filter(|n| (1..=MAX_DUMP_SIDE).contains(n))
            .ok_or(format!("{flag} must be a number from 1 to {MAX_DUMP_SIDE}"))
    };
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--version" | "-V" => return Ok(Command::Version),
            "login" | "logout" | "config" => args.command = Some(flag),
            "init" if args.command.as_deref() == Some("config") => args.config_init = true,
            "--backend" => {
                args.backend = Some(value(&mut it, &flag)?);
                args.backend_flag = "--backend";
            }
            "--config" => args.config = Some(value(&mut it, &flag)?),
            "--demo" => args.demo = true,
            "--teams" => {
                args.backend = Some("teams".into());
                args.backend_flag = "--teams";
            }
            "--browser" => args.browser = Some(value(&mut it, &flag)?),
            "--ansi" => args.ansi = true,
            "--dump" => args.dump = true,
            "--terminal-report" => args.terminal_report = true,
            "--client-id" => args.client_id = Some(value(&mut it, &flag)?),
            "--tenant" => args.tenant = Some(value(&mut it, &flag)?),
            "--chat" => args.chat = Some(value(&mut it, &flag)?),
            "--record-shapes" => args.record_shapes = Some(value(&mut it, &flag)?),
            "--log-headers" => args.log_headers = Some(value(&mut it, &flag)?),
            "--log" => args.log = Some(value(&mut it, &flag)?),
            "--keymap" => args.keymap = Some(value(&mut it, &flag)?),
            "--enter" => args.enter = Some(value(&mut it, &flag)?),
            "--max-chats" => args.max_chats = Some(value(&mut it, &flag)?),
            "--keys" => args.keys = Some(value(&mut it, &flag)?),
            "--width" => args.width = Some(side(value(&mut it, &flag)?, "--width")?),
            "--height" => args.height = Some(side(value(&mut it, &flag)?, "--height")?),
            "--images" => args.images = Some(value(&mut it, &flag)?),
            "--cell-pixels" => args.cell_pixels = Some(value(&mut it, &flag)?),
            "--clipboard" => args.clipboard = Some(value(&mut it, &flag)?),
            "--favorites" => args.favorites = Some(value(&mut it, &flag)?),
            "--push" => args.push = Some(value(&mut it, &flag)?),
            "--title" => args.title = Some(value(&mut it, &flag)?),
            "--hyperlinks" => args.hyperlinks = Some(value(&mut it, &flag)?),
            "--presence" => args.presence = Some(value(&mut it, &flag)?),
            "--bell" => args.bell = Some(value(&mut it, &flag)?),
            "--activity" => args.activity = Some(value(&mut it, &flag)?),
            "--away-after" => args.away_after = Some(value(&mut it, &flag)?),
            "--credentials" => args.credentials = Some(value(&mut it, &flag)?),
            "--cache" => args.cache = Some(value(&mut it, &flag)?),
            other => return Err(format!("unknown option {other} (try --help)")),
        }
    }
    Ok(Command::Run(Box::new(args)))
}

impl Args {
    /// The settings given on the command line, as `settings::resolve` takes them.
    fn flags(&self) -> Vec<settings::Flag> {
        let mut out = Vec::new();
        let mut add = |key: &'static str, value: &Option<String>, label: &'static str| {
            if let Some(value) = value {
                out.push(settings::Flag {
                    key,
                    value: value.clone(),
                    label,
                });
            }
        };
        add("backend", &self.backend, self.backend_flag);
        add("keymap", &self.keymap, "--keymap");
        add("enter", &self.enter, "--enter");
        add("images", &self.images, "--images");
        add("cell_pixels", &self.cell_pixels, "--cell-pixels");
        add("clipboard", &self.clipboard, "--clipboard");
        add("favorites", &self.favorites, "--favorites");
        add("max_chats", &self.max_chats, "--max-chats");
        add("log", &self.log, "--log");
        add("browser", &self.browser, "--browser");
        add("client_id", &self.client_id, "--client-id");
        add("tenant", &self.tenant, "--tenant");
        add("push", &self.push, "--push");
        add("presence", &self.presence, "--presence");
        add("bell", &self.bell, "--bell");
        add("activity", &self.activity, "--activity");
        add("away_after", &self.away_after, "--away-after");
        add("title", &self.title, "--title");
        add("hyperlinks", &self.hyperlinks, "--hyperlinks");
        add("credentials", &self.credentials, "--credentials");
        add("cache", &self.cache, "--cache");
        out
    }
}

/// `vtteams 0.1.0 (v0.1.0-3-gabc1234)`: the package version, and the commit it was built from (see `build.rs`).
fn version_line() -> String {
    format!(
        "vtteams {} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("VTTEAMS_BUILD_VERSION")
    )
}

fn main() {
    let args = match parse_args() {
        Ok(Command::Run(a)) => *a,
        Ok(Command::Help) => {
            print!("{HELP}");
            return;
        }
        Ok(Command::Version) => {
            println!("{}", version_line());
            return;
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    let code = match run(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    };
    // Background worker and task threads (chat/image polling, browser launches, the clipboard helper) are never
    // joined (`Workers::submit`, `App::spawn_call`/`spawn_task`): they are expected to be cut off by process exit,
    // not waited for. Returning normally from `main` instead makes the Rust runtime (and, on Windows, `ExitProcess`)
    // wait for those threads to reach a safe point, which can hang or get the process killed abnormally by whatever
    // is watching it (`cargo run` then reports an unrelated-looking exit code) if one is blocked inside a network
    // call. `process::exit` tears the process down immediately instead.
    std::process::exit(code);
}

/// `vtteams config`: show the settings in effect, or write a starter file.
fn config_command(args: &Args) -> Result<(), String> {
    let (path, _) = config::settings_path(args.config.clone())?;
    if args.config_init {
        config::write_starter(&path)?;
        println!(
            "wrote {}; every setting in it is commented out",
            path.display()
        );
        return Ok(());
    }
    let loaded = config::load(&args.flags(), args.config.clone())?;
    println!(
        "settings file: {} ({})",
        path.display(),
        if loaded.found {
            "found"
        } else {
            "not found; `vtteams config init` writes a starter file"
        }
    );
    println!();
    print!("{}", loaded.settings.describe("settings file"));
    println!();
    if !loaded.settings.credentials.encrypts() {
        let dir = config::config_dir().map_or_else(|e| e, |d| d.display().to_string());
        println!(
            "note: credentials = file keeps your sign-in as readable text in {dir}, which anything running as you, and \
             anyone with a copy of that folder, can read. The cache is only kept when the credential store works \
             (it is never written as readable text)."
        );
        println!();
    }
    println!(
        "A flag beats its VTTEAMS_* variable, which beats the settings file, which beats the default."
    );
    Ok(())
}

fn run(args: Args) -> Result<(), String> {
    // A diagnostic that asks the terminal and prints what it said; it needs no settings, no sign-in and no screen.
    if args.terminal_report {
        gfx::report::run(
            &config::terminal_env(),
            config::terminal_variables(),
            version_line(),
        );
        return Ok(());
    }
    match args.command.as_deref() {
        Some("config") => return config_command(&args),
        // Signing out must work even when the settings file is broken.
        Some("logout") => return logout(),
        _ => {}
    }
    let loaded = config::load(&args.flags(), args.config.clone())?;
    let settings = &loaded.settings;
    let keymap = settings.keymap;
    let level = settings.log;
    let log_file = match diag::init(level) {
        Ok(path) => path,
        Err(why) => {
            eprintln!("warning: no log file ({why}); continuing without one");
            None
        }
    };
    log::info!(
        "vtteams {} ({} build) starting; {} keys, log level {level}, writing to {}",
        env!("CARGO_PKG_VERSION"),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        keymap.label(),
        log_file
            .as_ref()
            .map_or_else(|| "(no file)".into(), |p| p.display().to_string())
    );
    log::info!(
        "settings file {} ({})",
        loaded.path.display(),
        if loaded.found { "read" } else { "not found" }
    );
    let login = args.command.as_deref() == Some("login");
    let use_teams = settings.backend == settings::BackendKind::Teams;
    // The demo makes no API exchanges, so there is nothing to record.
    let recorder = if args.demo {
        None
    } else {
        config::shape_recorder(args.record_shapes.clone())?
    };
    if let Some(r) = &recorder {
        eprintln!(
            "recording the shape of API exchanges to {} (content is masked, secrets are redacted)",
            r.dir().display()
        );
    }
    // The demo makes no HTTP requests either.
    let header_log = if args.demo {
        None
    } else {
        config::header_log(args.log_headers.clone())?
    };
    if header_log.is_some() {
        eprintln!(
            "logging the selected response headers to the log at info level (needs --log info or more; \
             request headers are never logged, and credential-bearing response headers are withheld)"
        );
    }
    let max_chats = settings.max_chats;
    // The key that seals the stored sign-in and the cache. With `credentials = auto` or `keychain` a credential store
    // that cannot be used stops start-up here, before anything is signed in or written, rather than falling back to
    // readable text. The demo stores nothing.
    let vault = if args.demo {
        config::Vault::none(settings.credentials)
    } else {
        config::open_vault(settings.credentials)?
    };
    // Who is signed in, known from what is stored before any request has been answered: what the cache is sealed to.
    let mut account: Option<String> = None;
    let backend: Arc<dyn Backend> = if args.demo {
        Arc::new(demo::DemoBackend::new())
    } else if use_teams {
        let credentials = config::credentials(config::Service::Teams, &vault)?;
        tell_once(&credentials.notes);
        let store = credentials.store.clone();
        let browser = config::browser_options(settings.browser.as_deref())?;
        let existing = if login {
            None
        } else {
            TeamsSession::load(store.as_ref())
                .map_err(|e| format!("{e}; run `vtteams logout` to start over"))?
        };
        let session = match existing {
            Some(s) => s,
            None if args.dump => return Err("not signed in; run `vtteams login --teams` first".into()),
            None => teams::sign_in(&browser, store.as_ref(), &|| {
                eprintln!();
                eprintln!("  A browser window is opening on Teams. Sign in there; it closes by itself when you are in.");
                eprintln!("  (Teams may take a moment to finish loading before vtteams sees the sign-in.)");
                eprintln!("  While that window is open, its debugging port is reachable by every user of this computer.");
                eprintln!("  Do not sign in this way on a computer you share.");
            })
            .map_err(|e| describe(&e))?,
        };
        if login {
            println!(
                "signed in as {}; session cached at {}",
                session.display_name(),
                credentials.path.display()
            );
            return Ok(());
        }
        account = Some(session.user_id().to_string());
        // A one-frame dump has no use for a push channel, and registering one is a write to the live service.
        let mut backend =
            TeamsBackend::new(session, store, browser).with_push(settings.push && !args.dump);
        if let Some(n) = max_chats {
            backend = backend.with_max_chats(n);
        }
        if let Some(r) = &recorder {
            backend = backend.with_recorder(Arc::clone(r));
        }
        if let Some(l) = &header_log {
            backend = backend.with_header_log(l.clone());
        }
        let backend = Arc::new(backend);
        // Name lookups for chat titles can take a minute when the directory is slow; they run on their own thread so
        // the chat list never waits for them.
        backend.attach();
        backend.connect_push();
        backend
    } else {
        let credentials = config::credentials(config::Service::Graph, &vault)?;
        tell_once(&credentials.notes);
        let store = credentials.store.clone();
        let auth = GraphAuth::new(config::graph_config(
            settings.client_id.as_deref(),
            settings.tenant.as_deref(),
        ));
        let cached = if login {
            None
        } else {
            auth.cached(store.as_ref()).map_err(|e| match e {
                // A stored sign-in that cannot be read (an encrypted one whose key is gone, say) is cured by starting over.
                teams_client::Error::Store(_) => {
                    format!("{}; run `vtteams logout` to start over", describe(&e))
                }
                _ => describe(&e),
            })?
        };
        let tokens = match cached {
            Some(t) => t,
            None if args.dump => return Err("not signed in; run `vtteams login` first".into()),
            None => {
                let tokens = sign_in(&auth)?;
                tokens
                    .save(store.as_ref())
                    .map_err(|e| format!("signed in, but {e}"))?;
                tokens
            }
        };
        if login {
            println!("signed in; token cached at {}", credentials.path.display());
            return Ok(());
        }
        account = tokens.user_id();
        let mut backend = GraphBackend::new(auth, tokens, store);
        if let Some(n) = max_chats {
            backend = backend.with_max_chats(n);
        }
        if let Some(r) = &recorder {
            backend = backend.with_recorder(Arc::clone(r));
        }
        if let Some(l) = &header_log {
            backend = backend.with_header_log(l.clone());
        }
        Arc::new(backend)
    };

    let mut app = App::new(backend);
    app.set_keymap(keymap);
    app.set_enter_sends(settings.enter_sends);
    app.pin_favorites = settings.pin_favorites;
    app.set_presence_style(settings.presence);
    app.set_bell_mode(settings.bell);
    app.set_activity(settings.activity && !args.dump, settings.away_after);
    app.read_only = args.dump;
    // A dump prints the frame's text, which must hold no escape sequence.
    app.hyperlinks = settings.hyperlinks && !args.dump;
    // Not when dumping a frame to stdout: a copy would put an escape sequence into the dump.
    if !args.dump {
        app.set_clipboard(config::clipboard(settings.clipboard));
    }
    // A one-frame dump draws no graphics, and asks the terminal nothing: block art, in the glyph set asked for.
    if args.dump {
        app.set_image_choice(gfx::ImageChoice::Art(settings.images.art_mode()));
    }
    // The cache is for the interactive screen only: a one-frame dump waits for the real answer, and the demo has nothing
    // of anyone's to keep.
    if !args.demo && !args.dump {
        let service = if use_teams {
            config::Service::Teams
        } else {
            config::Service::Graph
        };
        match config::cache_dir() {
            Ok(dir) if settings.cache => {
                match config::open_cache(&vault, &dir, service, account.as_deref()) {
                    Some(cache) => app.use_cache(Arc::new(cache)),
                    None => log::info!(
                        "start-up cache: off for this run (no credential store key, or no account id)"
                    ),
                }
            }
            Ok(dir) => config::remove_cache(&dir),
            Err(e) => log::warn!("start-up cache: off ({e})"),
        }
    }
    // The first code block would otherwise build the syntax highlighter on the UI thread, a freeze of a third of a second
    // in a release build and seconds in a debug one. Starting it now lets it run while the rest of start-up waits.
    if highlight::warms_at_start(args.dump) {
        highlight::warm_in_background();
    }
    app.start();
    if args.dump {
        dump(app, &args)
    } else {
        run_tui(app, settings.title, (settings.images, settings.cell_pixels))
    }
}

/// Say something once at start-up, before the screen is taken over: on the terminal, and in the log.
fn tell_once(notes: &[String]) {
    for note in notes {
        log::warn!("{note}");
        eprintln!("warning: {note}");
    }
}

/// Forget every stored sign-in (plain or encrypted), the key that encrypts them, the cache, and the browser profile that
/// holds the Microsoft single-sign-on cookies.
fn logout() -> Result<(), String> {
    // Forget everything that can be forgotten, whatever fails on the way: stopping at the first error would leave the
    // other sign-ins (and the browser's cookies) behind for someone who asked to be signed out of all of them.
    let config_dir = config::config_dir();
    let cache_dir = config::cache_dir();
    let keys = vault::OsKeys::open()
        .map(|keys| Arc::new(keys) as Arc<dyn vault::KeyProvider>)
        .map_err(|e| e.to_string());
    let done = config::forget_all(
        config_dir.as_deref().map_err(Clone::clone),
        cache_dir.as_deref().map_err(Clone::clone),
        keys,
    );
    if done.removed {
        println!("signed out (stored sign-ins, the encryption key and the cache deleted)");
    } else if done.failures.is_empty() {
        println!("no stored sign-in found");
    }
    if done.failures.is_empty() {
        Ok(())
    } else {
        Err(done.failures.join("; "))
    }
}

fn sign_in(auth: &GraphAuth) -> Result<GraphTokens, String> {
    auth.device_login(&|prompt: &DeviceCodePrompt| {
        let copied = clipboard::System::new().set(&prompt.user_code).is_ok();
        eprintln!();
        match &prompt.message {
            Some(message) if !message.is_empty() => eprintln!("  {message}"),
            _ => eprintln!(
                "  To sign in, open  {}  and enter the code  {}",
                prompt.verification_uri, prompt.user_code
            ),
        }
        if copied {
            eprintln!("  (the code is on your clipboard)");
        }
        eprintln!("  Waiting for you to finish signing in...");
        let _ = webbrowser::open(&prompt.verification_uri);
    })
    .map_err(|e| describe(&e))
}
// ---------------------------------------------------------------------------
// Interactive
// ---------------------------------------------------------------------------

/// ratatui's own panic hook restores the terminal when *any* thread panics, which would wreck the screen for a panic
/// that another thread has already contained (a worker, the clipboard, the HTML converter: each reports an error
/// instead), and it does not know about the extra modes we switch on. Replace it with one that leaves the screen alone
/// unless the UI thread (the main thread) is the one that crashed, in which case it switches every mode back off
/// before handing over to ratatui's hook. The log gets the thread and the place, never the panic message: that can
/// quote the text being processed.
/// Whether the terminal was asked to report keys with the kitty keyboard protocol (see [`enhance_keys`]).
static ENHANCED_KEYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A plain terminal sends the same byte for Enter, Shift-Enter and Ctrl-Enter, so vtteams cannot tell them apart; one that
/// speaks the kitty keyboard protocol (kitty, WezTerm, foot, Ghostty, iTerm2, recent Windows Terminal and others) says
/// which modifiers were held, over SSH as well, if asked. Ask only when it answers the query; Windows reports modifiers
/// by itself and answers no.
fn enhance_keys() {
    let asked = supports_keyboard_enhancement().unwrap_or(false)
        && execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
    ENHANCED_KEYS.store(asked, std::sync::atomic::Ordering::Relaxed);
    log::debug!(
        "kitty keyboard protocol: {}",
        if asked { "on" } else { "not available" }
    );
}

/// Undo [`enhance_keys`]; harmless when it was not done.
fn restore_keys() {
    if ENHANCED_KEYS.swap(false, std::sync::atomic::Ordering::Relaxed) {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
}

fn install_panic_hook() {
    let restore_terminal_and_print = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        match info.location() {
            Some(at) => log::error!("panic in thread {name} at {}:{}", at.file(), at.line()),
            None => log::error!("panic in thread {name}"),
        }
        if name != "main" {
            return;
        }
        // First, so that nothing that follows (the terminal's own reset, the panic message) is held back by an update a
        // frame left open, and so that the pictures of the terminal's own layer go with it.
        gfx::present::emergency_cleanup(&mut std::io::stdout());
        let _ = execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            DisableFocusChange,
            SetCursorStyle::DefaultUserShape,
            Show
        );
        restore_keys();
        title::restore();
        restore_terminal_and_print(info);
    }));
}

fn run_tui(
    mut app: App,
    set_title: bool,
    (images, cell_pixels): (gfx::ImageChoice, Option<gfx::CellPx>),
) -> Result<(), String> {
    // ratatui's own start-up is kept as it is (raw mode, the alternate screen, a panic hook that undoes both); the
    // terminal it hands back is not used, because frames are drawn and written by `output::Display`, in one write each.
    ratatui::try_init().map_err(|e| format!("cannot use the terminal: {e}"))?;
    let mut display = output::Display::new(output::TerminalOut::open())
        .map_err(|e| format!("cannot use the terminal: {e}"))?;
    install_panic_hook();
    // Which graphics the terminal has, found out now, before the first key is read: what it answers is read here, by the
    // strict reader in `gfx::detect`, and is never seen by the key handler.
    let size = display.size().map_or((0, 0), |s| (s.width, s.height));
    let asked = std::cell::Cell::new(false);
    let facts = gfx::probe::learn(
        images,
        &config::terminal_env(),
        cell_pixels,
        gfx::probe::system_cell(),
        size,
        |kitty| {
            asked.set(true);
            gfx::probe::query_terminal(kitty)
        },
    );
    // An answer that came after the wait ended is on the input by now or about to be, and would be typed into the
    // message box as text. Everything arriving for a moment is thrown away, except a key that quits.
    let early_keys = if asked.get() {
        gfx::probe::settle_input()
    } else {
        Vec::new()
    };
    for event in early_keys {
        if let Event::Key(key) = event {
            app.on_key(key);
        }
    }
    app.set_images(images, facts);
    log::info!(
        "pictures: images = {}, protocol {}, cell size {}{} ({})",
        images.label(),
        app.graphics()
            .map_or("none (block art)", |g| g.protocol.label()),
        facts
            .cell
            .map_or_else(|| "unknown".to_string(), gfx::CellPx::label),
        if facts.cell_assumed {
            " (an ASSUMPTION: nothing told vtteams the real size; set cell_pixels to your font's cell size if pictures look the wrong size)"
        } else {
            ""
        },
        facts.why
    );
    let _ = execute!(
        std::io::stdout(),
        EnableBracketedPaste,
        EnableMouseCapture,
        EnableFocusChange
    );
    enhance_keys();
    if set_title {
        title::set();
    }
    let result = event_loop(&mut display, &mut app);
    // An error out of the loop can come between a frame's start and end (a write that failed part way), and with a
    // picture still in the terminal's own layer: end the update and take the pictures away, as the panic hook does.
    gfx::present::emergency_cleanup(&mut std::io::stdout());
    let _ = execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        DisableFocusChange,
        SetCursorStyle::DefaultUserShape,
        Show
    );
    restore_keys();
    title::restore();
    ratatui::restore();
    result.map_err(|e| e.to_string())
}

/// A frame slower than this is worth a record.
const SLOW_FRAME: Duration = Duration::from_millis(25);
/// A keystroke that takes this long to reach the screen is felt as lag.
const SLOW_KEY: Duration = Duration::from_millis(50);
/// While the loop is busy it is not reading keys, so a key typed meanwhile waits: busy this long is worth a record.
const SLOW_ITERATION: Duration = Duration::from_millis(30);

fn event_loop(
    display: &mut output::Display<output::TerminalOut>,
    app: &mut App,
) -> std::io::Result<()> {
    // A paste on a console that cannot report one arrives as key presses (see `paste.rs`).
    let mut pastes = paste::Coalescer::default();
    // When a paste was last logged: one that arrives in many small pieces is one line in the log, not hundreds.
    let mut paste_logged: Option<Instant> = None;
    // When the oldest keystroke not yet on screen was read.
    let mut key_at: Option<Instant> = None;
    while !app.quit {
        let frame_started = Instant::now();
        // Block cursor in normal/visual mode, bar in insert mode, like vim in a modern terminal.
        let shape = if app.editor.mode() == Mode::Insert {
            output::CursorShape::Bar
        } else {
            output::CursorShape::Block
        };
        // Everything the frame needs from the terminal, its pictures and its cursor included, goes out in one write; a
        // frame that changes nothing writes nothing (see `output`).
        let sent = display.frame(app, shape)?;
        let frame = frame_started.elapsed();
        if frame >= SLOW_FRAME {
            // Where the time went, to tell our own work (`render`) from the terminal taking its time over what was
            // written (`write`), and one asking of the terminal's size now, which is a round trip to it.
            let asking = Instant::now();
            let sized = display.size();
            log::debug!(
                "slow frame: {} ms ({}; the terminal's size asked afresh took {} ms{})",
                frame.as_millis(),
                sent.parts.describe(sent.bytes),
                asking.elapsed().as_millis(),
                if sized.is_ok() { "" } else { ", and failed" }
            );
        }
        if let Some(at) = key_at.take() {
            let lag = at.elapsed();
            if lag >= SLOW_KEY {
                log::debug!(
                    "slow keystroke: {} ms from key to screen (the frame took {} ms)",
                    lag.as_millis(),
                    frame.as_millis()
                );
            } else {
                log::trace!(
                    "keystroke on screen after {} ms (frame {} ms)",
                    lag.as_millis(),
                    frame.as_millis()
                );
            }
        }
        let polled = event::poll(app.poll_wait())?;
        // From here on the thread is working, not waiting for a key.
        let handling_started = Instant::now();
        if polled {
            // Read whatever is queued, so typing bursts cost one redraw and a paste can be told from typing.
            let mut queued = Vec::new();
            loop {
                queued.push(event::read()?);
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
            let read = queued.len();
            for event in pastes.process(queued, Instant::now()) {
                match event {
                    Event::Key(key) => {
                        key_at.get_or_insert_with(Instant::now);
                        app.on_key(key);
                    }
                    Event::Paste(text) => {
                        let piece =
                            paste_logged.is_some_and(|at| at.elapsed() < Duration::from_secs(1));
                        let note = format!(
                            "paste{}: {} characters (read as {read} events)",
                            if piece { " (more)" } else { "" },
                            text.chars().count()
                        );
                        // Only the start of a paste is news; the pieces after it are for tracing.
                        if piece {
                            log::trace!("{note}");
                        } else {
                            log::debug!("{note}");
                        }
                        paste_logged = Some(Instant::now());
                        app.on_paste(&text);
                    }
                    Event::Mouse(mouse) => app.on_mouse(mouse),
                    Event::FocusGained => app.set_focused(true),
                    Event::FocusLost => app.set_focused(false),
                    // The cells may have changed size too (a zoomed font), where the system can say.
                    Event::Resize(..) => app.on_resize(gfx::probe::system_cell()),
                }
            }
        }
        let handling = handling_started.elapsed();
        let tick_started = Instant::now();
        app.on_tick();
        let tick = tick_started.elapsed();
        if app.take_bell() {
            // BEL: the terminal decides what that is (a sound, a flashing tab or taskbar entry).
            use std::io::Write;
            let mut out = std::io::stdout();
            out.write_all(b"\x07")?;
            out.flush()?;
        }
        let busy = frame + handling + tick;
        if busy >= SLOW_ITERATION {
            log::debug!(
                "slow loop iteration: busy {} ms (draw {} ms, input {} ms, tick {} ms); a key typed meanwhile waits",
                busy.as_millis(),
                frame.as_millis(),
                handling.as_millis(),
                tick.as_millis()
            );
        }
    }
    display.teardown();
    Ok(())
}

// ---------------------------------------------------------------------------
// Dump
// ---------------------------------------------------------------------------

/// The keys named by `--keys`: plain characters, and `<Name>` for others. A name may carry modifiers as prefixes
/// (`<C-s>`, `<S-Enter>`, `<C-S-Left>`, `<A-Enter>`) and is one of Enter (CR), Esc, Tab, BS, Space, Up, Down, Left,
/// Right, Home, End, PgUp, PgDn, Del, Ins, F1 to F12, lt (a literal `<`), or a single character.
fn parse_keys(s: &str) -> Result<Vec<KeyEvent>, String> {
    let mut out = Vec::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '<' {
            out.push(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            continue;
        }
        let mut name = String::new();
        for n in chars.by_ref() {
            if n == '>' {
                break;
            }
            name.push(n);
        }
        out.push(parse_key_name(&name)?);
    }
    Ok(out)
}

fn parse_key_name(name: &str) -> Result<KeyEvent, String> {
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = name;
    loop {
        let lower = rest.to_ascii_lowercase();
        let bit = match lower.get(..2) {
            Some("c-") => KeyModifiers::CONTROL,
            Some("s-") => KeyModifiers::SHIFT,
            Some("a-") => KeyModifiers::ALT,
            _ => break,
        };
        modifiers |= bit;
        rest = &rest[2..];
    }
    let lower = rest.to_ascii_lowercase();
    let code = match lower.as_str() {
        "enter" | "cr" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "tab" if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
        "tab" => KeyCode::Tab,
        "bs" => KeyCode::Backspace,
        "space" => KeyCode::Char(' '),
        "lt" => KeyCode::Char('<'),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pgup" => KeyCode::PageUp,
        "pgdn" => KeyCode::PageDown,
        "del" => KeyCode::Delete,
        "ins" => KeyCode::Insert,
        f if f.starts_with('f') && f.len() > 1 => match f[1..].parse::<u8>() {
            Ok(n @ 1..=12) => KeyCode::F(n),
            _ => return Err(format!("unknown key <{name}>")),
        },
        _ => {
            let mut one = rest.chars();
            match (one.next(), one.next()) {
                (Some(c), None) => KeyCode::Char(if modifiers.contains(KeyModifiers::CONTROL) {
                    c.to_ascii_lowercase()
                } else {
                    c
                }),
                _ => return Err(format!("unknown key <{name}>")),
            }
        }
    };
    Ok(KeyEvent::new(code, modifiers))
}
fn pump(app: &mut App, timeout: Duration, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        app.on_tick();
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn dump(mut app: App, args: &Args) -> Result<(), String> {
    pump(&mut app, Duration::from_secs(20), |a| {
        a.chats.loaded() && a.me.is_some()
    });
    if !app.chats.loaded() {
        return Err(app
            .chats
            .error()
            .map(str::to_string)
            .unwrap_or_else(|| "timed out loading chats".into()));
    }
    if let Some(sel) = &args.chat {
        let me = app.me_id().to_string();
        let id = match sel.parse::<usize>() {
            Ok(n) => app
                .chats
                .items()
                .get(n.saturating_sub(1))
                .map(|c| c.id.clone()),
            Err(_) => app
                .chats
                .items()
                .iter()
                .find(|c| c.title(&me).to_lowercase().contains(&sel.to_lowercase()))
                .map(|c| c.id.clone()),
        };
        let id = id.ok_or_else(|| format!("no chat matches '{sel}'"))?;
        app.select_chat(&id);
    }
    let (w, h) = (args.width.unwrap_or(130), args.height.unwrap_or(46));
    let loaded = |a: &App| a.state().is_some_and(|s| s.loaded);
    pump(&mut app, Duration::from_secs(20), loaded);
    if let Some(keys) = &args.keys {
        for key in parse_keys(keys)? {
            // Layout runs between keys so scrolling/selection logic sees real geometry.
            ui::render_to_string(&mut app, w, h);
            let was_open = app.current.clone();
            app.on_key(key);
            // A conversation a key has just opened is read on a worker; the next key (an `@` to complete, say) is about
            // what it holds.
            if app.current != was_open && app.current.is_some() {
                pump(&mut app, Duration::from_secs(5), loaded);
            }
            // A search asked of the service is answered on a worker; the next key should find the answer in.
            if app.search_pending() {
                pump(&mut app, Duration::from_secs(5), |a| !a.search_pending());
            }
            // So is the teams list, which the first Alt-2 or `gt` asks for and the next keys are about.
            if app.teams_pending() {
                pump(&mut app, Duration::from_secs(5), |a| !a.teams_pending());
            }
        }
    }
    // A single frame has no minute to wait for availability: ask now, and the loop below waits for the answer.
    app.presence_soon();
    // Let images arrive, redrawing so newly requested ones are fetched.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        ui::render_to_string(&mut app, w, h);
        app.on_tick();
        if app.settled() {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    ui::render_to_string(&mut app, w, h);
    if args.ansi {
        print!("{}", ui::render_to_ansi(&mut app, w, h));
    } else {
        println!("{}", ui::render_to_string(&mut app, w, h));
    }
    if let Some(e) = app.state().and_then(|s| s.error.clone()) {
        eprintln!("note: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    fn parse(args: &[&str]) -> Result<Args, String> {
        match parse_args_from(args.iter().map(|a| (*a).to_string()).collect())? {
            Command::Run(a) => Ok(*a),
            Command::Help => Err("help".into()),
            Command::Version => Err("version".into()),
        }
    }

    #[test]
    fn every_setting_has_a_flag_that_reaches_it() {
        for key in &settings::KEYS {
            let flags = parse(&[key.flag, "x"]).unwrap().flags();
            assert!(
                flags.iter().any(|f| f.label == key.flag && f.value == "x"),
                "{} does not produce a setting",
                key.flag
            );
        }
    }

    #[test]
    fn of_two_flags_for_one_setting_the_one_typed_last_wins() {
        let flags = parse(&["--backend", "graph", "--teams"]).unwrap().flags();
        assert_eq!(
            flags
                .iter()
                .map(|f| (f.label, f.value.as_str()))
                .collect::<Vec<_>>(),
            [("--teams", "teams")]
        );
        let flags = parse(&["--teams", "--backend", "graph"]).unwrap().flags();
        assert_eq!(
            flags
                .iter()
                .map(|f| (f.label, f.value.as_str()))
                .collect::<Vec<_>>(),
            [("--backend", "graph")]
        );
    }

    #[test]
    fn terminal_report_is_a_diagnostic_flag_with_no_value_and_not_a_setting() {
        let args = parse(&["--terminal-report"]).unwrap();
        assert!(args.terminal_report);
        assert!(args.flags().is_empty(), "it reaches no setting");
        assert!(!parse(&["--demo"]).unwrap().terminal_report);
        assert!(settings::KEYS.iter().all(|k| k.flag != "--terminal-report"));
        assert!(HELP.contains("--terminal-report"));
    }

    #[test]
    fn log_headers_takes_a_selector_and_is_a_diagnostic_not_a_setting() {
        let args = parse(&["--log-headers", "connection,x-ms*,!x-ms-latency"]).unwrap();
        assert_eq!(
            args.log_headers.as_deref(),
            Some("connection,x-ms*,!x-ms-latency")
        );
        // Like --record-shapes it is a flag and a variable, not a row of the settings file.
        assert!(args.flags().iter().all(|f| f.label != "--log-headers"));
        // A value is never another option (`--log-headers --teams` forgot the names).
        assert!(parse(&["--log-headers"]).is_err());
        assert!(parse(&["--log-headers", "--teams"]).is_err());
    }

    #[test]
    fn bad_command_lines_are_errors_not_panics_or_absurd_sizes() {
        assert!(matches!(
            parse_args_from(vec!["--help".into()]),
            Ok(Command::Help)
        ));
        for flag in ["--version", "-V"] {
            assert!(matches!(
                parse_args_from(vec![flag.into()]),
                Ok(Command::Version)
            ));
        }
        let line = version_line();
        assert!(
            line.starts_with("vtteams ") && line.ends_with(')') && !line.contains('\n'),
            "{line}"
        );
        for bad in [
            vec!["--width", "0"],
            vec!["--width", "65535"],
            vec!["--height", "x"],
            vec!["--keymap"],
            vec!["--browser", "--teams"],
            vec!["--nonsense"],
        ] {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
        assert_eq!(parse(&["--width", "1000"]).unwrap().width, Some(1000));
    }
    use super::*;

    fn keys(s: &str) -> Vec<(KeyCode, KeyModifiers)> {
        parse_keys(s)
            .unwrap()
            .into_iter()
            .map(|k| (k.code, k.modifiers))
            .collect()
    }

    #[test]
    fn keys_are_plain_characters_or_named_with_optional_modifiers() {
        let none = KeyModifiers::NONE;
        assert_eq!(
            keys("a<Enter><lt>"),
            [
                (KeyCode::Char('a'), none),
                (KeyCode::Enter, none),
                (KeyCode::Char('<'), none)
            ]
        );
        assert_eq!(keys("<C-s>"), [(KeyCode::Char('s'), KeyModifiers::CONTROL)]);
        assert_eq!(keys("<S-Enter>"), [(KeyCode::Enter, KeyModifiers::SHIFT)]);
        assert_eq!(keys("<A-Enter>"), [(KeyCode::Enter, KeyModifiers::ALT)]);
        assert_eq!(
            keys("<C-S-Left>"),
            [(KeyCode::Left, KeyModifiers::CONTROL | KeyModifiers::SHIFT)]
        );
        assert_eq!(keys("<S-Tab>"), [(KeyCode::BackTab, KeyModifiers::SHIFT)]);
        assert_eq!(
            keys("<F5><PgUp><Del>"),
            [
                (KeyCode::F(5), none),
                (KeyCode::PageUp, none),
                (KeyCode::Delete, none)
            ]
        );
        assert_eq!(
            keys("<space><BS>"),
            [(KeyCode::Char(' '), none), (KeyCode::Backspace, none)]
        );
    }

    #[test]
    fn an_unknown_key_name_is_an_error_that_names_it() {
        for bad in ["<Nope>", "<F13>", "<C->", "<C-ab>"] {
            let e = parse_keys(bad).unwrap_err();
            assert!(e.contains("unknown key"), "{bad}: {e}");
        }
    }
}
