//! Asking the terminal, once, before the event loop starts: which graphics it has and how big a cell is.
//!
//! The questions are written together, with the primary-device-attributes question last: every terminal answers that one,
//! and answers come back in the order they were asked, so its arrival means everything else that was going to be
//! answered has been, and nothing is left on the input to be taken for typing. A terminal that answers nothing at all is
//! simply waited for until [`BUDGET`] runs out ([`REMOTE_BUDGET`] over a connection to another computer, where an answer
//! takes a round trip). An answer that comes later than that would reach the key handler as typing, so two more things
//! are done: the Kitty graphics question, whose answer is an APC string that a key reader takes for Alt+`_` followed by
//! text, is not asked at all when running remotely, and after the questions the input is read and thrown away for a
//! short while ([`settle`]), keeping only a key that quits.
//!
//! What comes back is read as raw bytes where the system lets a program do that safely (Unix: the terminal in its
//! raw mode with a read timeout) and as the key events the console turns it into where it does not (Windows). Either way
//! it goes only to [`parse_replies`], which keeps numbers and drops everything else, and
//! never to the key handler: this runs before the first key is read.

use super::detect::{self, MAX_REPLY_BYTES, Replies, TermEnv, parse_replies};
use super::{CellPx, ImageChoice, Protocol};
use std::io::Write;
use std::time::{Duration, Instant};

/// How long to wait for the answers in all, however many questions there are.
pub const BUDGET: Duration = Duration::from_millis(300);

/// The same where the answers cross a network (SSH and the like): the device-attributes answer still ends the wait the
/// moment it arrives, so this only matters to a terminal that is slow or silent.
pub const REMOTE_BUDGET: Duration = Duration::from_millis(800);

/// How long after the questions the input is read and thrown away, to take in an answer that came after the wait
/// ended.
pub const SETTLE: Duration = Duration::from_millis(150);

/// Most events [`settle`] reads: a bound for a terminal that never stops sending.
const MAX_SETTLE_EVENTS: usize = 4096;

/// What to ask and how long to wait for it, given whether the Kitty question is wanted and whether this runs remotely
/// (`config::runs_remotely`): the Kitty question is never asked remotely, and the wait is longer.
pub fn plan(ask_kitty: bool, remote: bool) -> (bool, Duration) {
    if remote {
        (false, REMOTE_BUDGET)
    } else {
        (ask_kitty, BUDGET)
    }
}

/// Read events with `next` (given how long it may wait, it returns an event or `None`) for `window` and drop them all,
/// except those `quits` says are a key that quits, which are returned in the order they came so that a person can still
/// leave. What a terminal's late answer turns into when read as keys (Alt+`_`, letters, digits, `=`, `;`) is what this
/// throws away. At most [`MAX_SETTLE_EVENTS`] are read.
pub fn settle<E>(
    mut next: impl FnMut(Duration) -> Option<E>,
    quits: impl Fn(&E) -> bool,
    window: Duration,
) -> Vec<E> {
    let started = Instant::now();
    let mut kept = Vec::new();
    let mut read = 0;
    while read < MAX_SETTLE_EVENTS
        && let Some(left) = window
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
    {
        if let Some(event) = next(left.min(Duration::from_millis(25))) {
            read += 1;
            if quits(&event) {
                kept.push(event);
            }
        }
    }
    kept
}

/// Whether `event` is Ctrl-C or Ctrl-Q pressed: the keys that must still work while input is being thrown away.
pub fn is_quit_key(event: &ratatui::crossterm::event::Event) -> bool {
    use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    matches!(
        event,
        Event::Key(key)
            if key.kind == KeyEventKind::Press
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c' | 'q' | 'C' | 'Q'))
    )
}

/// [`settle`] on the real terminal's input, for [`SETTLE`].
pub fn settle_input() -> Vec<ratatui::crossterm::event::Event> {
    use ratatui::crossterm::event;
    settle(
        |wait| {
            if event::poll(wait).unwrap_or(false) {
                event::read().ok()
            } else {
                None
            }
        },
        is_quit_key,
        SETTLE,
    )
}
/// The questions: the cell size in pixels, the text area in pixels, optionally the Kitty graphics query, and last the
/// primary device attributes. The Kitty query asks for a one-pixel image with `a=q` (query only: nothing is stored or
/// shown) and id 31; its answer, if the terminal speaks the protocol, is `i=31;OK`.
pub fn questions(ask_kitty: bool) -> Vec<u8> {
    let mut out = b"\x1b[16t\x1b[14t".to_vec();
    if ask_kitty {
        out.extend_from_slice(
            format!(
                "\x1b_Gi={},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\",
                detect::KITTY_QUERY_ID
            )
            .as_bytes(),
        );
    }
    out.extend_from_slice(b"\x1b[c");
    out
}

/// Where the answers come from.
pub trait Source {
    /// What arrived within about `wait`, which may be nothing. Returns early when something has.
    fn read(&mut self, wait: Duration) -> Vec<u8>;

    /// What the system handed over before it became bytes (the key events of a Windows console), one line each, as
    /// text that is safe to print; what has been read since the last call. Empty where the answers are bytes already.
    fn trace(&mut self) -> Vec<String> {
        Vec::new()
    }
}

/// What a question and its waiting came to, for the log and for `--terminal-report`.
#[derive(Debug)]
pub struct Asked {
    pub replies: Replies,
    /// Everything that arrived, as bytes, up to [`MAX_REPLY_BYTES`].
    pub raw: Vec<u8>,
    /// How long the wait took.
    pub elapsed: Duration,
    /// The wait ended because the device-attributes answer arrived (rather than because the budget ran out).
    pub answered: bool,
}

/// Write the questions to `out` and read the answers from `source`, until the device-attributes answer arrives or
/// `budget` runs out.
pub fn ask<S: Source + ?Sized>(
    out: &mut impl Write,
    source: &mut S,
    ask_kitty: bool,
    budget: Duration,
) -> std::io::Result<Replies> {
    ask_traced(out, source, ask_kitty, budget).map(|asked| asked.replies)
}

/// [`ask`], also saying what arrived and how long it took.
pub fn ask_traced<S: Source + ?Sized>(
    out: &mut impl Write,
    source: &mut S,
    ask_kitty: bool,
    budget: Duration,
) -> std::io::Result<Asked> {
    out.write_all(&questions(ask_kitty))?;
    out.flush()?;
    let started = Instant::now();
    let mut got: Vec<u8> = Vec::new();
    let mut answered = false;
    while let Some(left) = budget
        .checked_sub(started.elapsed())
        .filter(|d| !d.is_zero())
    {
        got.extend(source.read(left.min(Duration::from_millis(50))));
        got.truncate(MAX_REPLY_BYTES);
        if parse_replies(&got).attributes.is_some() {
            answered = true;
            break;
        }
    }
    let elapsed = started.elapsed();
    // Whatever is already waiting behind the answer is taken too, so that none of it is left for the key reader.
    got.extend(source.read(Duration::from_millis(5)));
    got.truncate(MAX_REPLY_BYTES);
    Ok(Asked {
        replies: parse_replies(&got),
        raw: got,
        elapsed,
        answered,
    })
}

/// A key event as far as reading a terminal's answer goes. Only a Windows console hands an answer over this way, but
/// the translation is tested on every platform.
#[cfg(any(windows, test))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReplyKey {
    Esc,
    Char { c: char, alt: bool, ctrl: bool },
}

/// The bytes a run of key events stands for, where the answer reached the program as key events (Windows).
///
/// The console builds those events from the text the terminal sent, and an ESC does not survive it as an ESC: it comes
/// as Ctrl+`[` (how a keyboard types it) or is left out altogether by the layer above, so that the answer
/// `ESC [ ? 6 1 ; 4 c` arrives as `[?61;4c`. An ESC is put back where the answer was Ctrl+`[`, and in front of a `[`
/// that no ESC stands before. The result goes to the same strict reader as any other answer, which still accepts only the
/// exact numeric forms, so nothing typed at the keyboard in that moment becomes a command.
#[cfg(any(windows, test))]
pub fn bytes_from_keys(keys: &[ReplyKey]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    for key in keys {
        match *key {
            ReplyKey::Esc => out.push(0x1b),
            ReplyKey::Char {
                c: '[', ctrl: true, ..
            } => out.push(0x1b),
            ReplyKey::Char { c, alt, .. } => {
                if alt || (c == '[' && out.last() != Some(&0x1b)) {
                    out.push(0x1b);
                }
                let mut utf8 = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
            }
        }
    }
    out
}

/// A character as text that is safe to print: U+005B '[' for a printable ASCII one, its number alone for any other.
#[cfg(any(windows, test))]
pub fn describe_char(c: char) -> String {
    if c.is_ascii_graphic() || c == ' ' {
        format!("U+{:04X} '{c}'", u32::from(c))
    } else {
        format!("U+{:04X}", u32::from(c))
    }
}

/// What is known about the terminal for drawing pictures.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Facts {
    /// The protocol detection found; unused when a protocol was chosen by name.
    pub detected: Option<Protocol>,
    /// The cell size in pixels, if it could be learned.
    pub cell: Option<CellPx>,
    /// Whether the cell size came from the system (the terminal window's pixel size), which can be read again when the
    /// window changes size, as opposed to an answer given once or a size the person set.
    pub cell_from_system: bool,
    /// The cell size is [`super::ASSUMED_WINDOWS_TERMINAL_CELL`], an assumption: Windows Terminal, which draws Sixel,
    /// was not asked or did not say how big its cells are, and the person has not said either.
    pub cell_assumed: bool,
    /// Why, in a few fixed words (for the log).
    pub why: &'static str,
}

impl Facts {
    pub const NONE: Self = Self {
        detected: None,
        cell: None,
        cell_from_system: false,
        cell_assumed: false,
        why: "not asked",
    };
}

/// Work out the [`Facts`]: from the person's settings, the system's own answer for the cell size (`system_cell`), the
/// environment, and, when those do not settle it, the terminal's answers (`query`, given whether to ask the Kitty
/// question; called at most once). `size` is the terminal's columns and rows.
pub fn learn(
    choice: ImageChoice,
    env: &TermEnv,
    cell_override: Option<CellPx>,
    system_cell: Option<CellPx>,
    size: (u16, u16),
    query: impl FnOnce(bool) -> Option<Replies>,
) -> Facts {
    match choice {
        ImageChoice::Art(_) => {
            return Facts {
                why: "the images setting asks for art made of characters",
                ..Facts::NONE
            };
        }
        ImageChoice::Auto if detect::in_multiplexer(env) => {
            return Facts {
                why: "inside tmux or screen",
                ..Facts::NONE
            };
        }
        ImageChoice::Auto | ImageChoice::Protocol(_) => {}
    }
    let known = cell_override.or(system_cell);
    // A protocol chosen by name needs no detection, only a cell size when nothing else gave one.
    let ask = matches!(choice, ImageChoice::Auto) || known.is_none();
    let replies = if ask {
        query(detect::asks_kitty(env)).unwrap_or_default()
    } else {
        Replies::default()
    };
    let (detected, why) = match choice {
        ImageChoice::Auto => {
            let detection = detect::decide(env, &replies);
            (detection.protocol, detection.why)
        }
        _ => (None, "a protocol was chosen by name"),
    };
    let told = known.or_else(|| detect::cell_size(&replies, size.0, size.1));
    // Windows Terminal draws Sixel but may say nothing about the size of its cells, and a program on Windows has no
    // other way to learn it without `unsafe`. A size is then assumed, a little smaller than the terminal's default
    // font gives, because a picture too small leaves some blank space round it where one too big covers the text beside
    // it. Anything the terminal or the person says beats it, and it is only used for a protocol that is Sixel.
    let sixel = match choice {
        ImageChoice::Auto => detected == Some(Protocol::Sixel),
        ImageChoice::Protocol(p) => p == Protocol::Sixel,
        ImageChoice::Art(_) => false,
    };
    let assumed = (told.is_none() && sixel && env.windows_terminal)
        .then_some(super::ASSUMED_WINDOWS_TERMINAL_CELL);
    Facts {
        detected,
        cell: told.or(assumed),
        cell_from_system: cell_override.is_none() && system_cell.is_some(),
        cell_assumed: assumed.is_some(),
        why,
    }
}

/// The size of a cell in pixels as the system reports it (the terminal window's size in pixels divided by its columns and
/// rows), where it does. Not on Windows, and not in every terminal elsewhere; then `None`.
pub fn system_cell() -> Option<CellPx> {
    let size = ratatui::crossterm::terminal::window_size().ok()?;
    if size.columns == 0 || size.rows == 0 {
        return None;
    }
    CellPx::new(
        u32::from(size.width / size.columns),
        u32::from(size.height / size.rows),
    )
}

/// The terminal's answers, asked for on the real terminal. `None` when this is not an interactive terminal, or when the
/// system gives no way to read the answers safely.
pub fn query_terminal(ask_kitty: bool) -> Option<Replies> {
    with_source(|source| {
        query_with(
            &mut std::io::stdout(),
            source,
            ask_kitty,
            crate::config::runs_remotely(),
        )
        .ok()
    })?
}

/// [`ask`] as start-up does it: what to ask and how long to wait come from [`plan`].
pub fn query_with<S: Source + ?Sized>(
    out: &mut impl Write,
    source: &mut S,
    ask_kitty: bool,
    remote: bool,
) -> std::io::Result<Replies> {
    let (ask_kitty, budget) = plan(ask_kitty, remote);
    ask(out, source, ask_kitty, budget)
}

/// Run `f` with the terminal's input as a [`Source`], in the mode the answers can be read in, and put the terminal's
/// settings back afterwards. `None` when this is not an interactive terminal, or the system gives no safe way to read.
pub fn with_source<R>(f: impl FnOnce(&mut dyn Source) -> R) -> Option<R> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return None;
    }
    #[cfg(unix)]
    {
        let mut source = unix::Tty::open()?;
        Some(f(&mut source))
    }
    #[cfg(windows)]
    {
        let mut source = windows::Console::default();
        Some(f(&mut source))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = f;
        None
    }
}

#[cfg(unix)]
mod unix {
    use super::Source;
    use rustix::termios::{
        LocalModes, OptionalActions, SpecialCodeIndex, Termios, tcgetattr, tcsetattr,
    };
    use std::time::Duration;

    /// The terminal on standard input, in the raw mode the application already put it in, with reads that give up after
    /// a tenth of a second instead of waiting for a key. The settings it found are put back when it is dropped.
    pub struct Tty {
        stdin: std::io::Stdin,
        saved: Termios,
    }

    impl Tty {
        pub fn open() -> Option<Self> {
            let stdin = std::io::stdin();
            let saved = tcgetattr(&stdin).ok()?;
            // In canonical mode a read waits for a whole line: not a mode to ask questions in.
            if saved.local_modes.contains(LocalModes::ICANON) {
                return None;
            }
            let mut timed = saved.clone();
            timed.special_codes[SpecialCodeIndex::VMIN] = 0;
            timed.special_codes[SpecialCodeIndex::VTIME] = 1;
            tcsetattr(&stdin, OptionalActions::Now, &timed).ok()?;
            Some(Self { stdin, saved })
        }
    }

    impl Source for Tty {
        fn read(&mut self, _wait: Duration) -> Vec<u8> {
            let mut buf = [0u8; 256];
            match rustix::io::read(&self.stdin, &mut buf) {
                Ok(n) => buf[..n].to_vec(),
                Err(_) => Vec::new(),
            }
        }
    }

    impl Drop for Tty {
        fn drop(&mut self) {
            let _ = tcsetattr(&self.stdin, OptionalActions::Now, &self.saved);
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::{ReplyKey, Source, bytes_from_keys};
    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use std::time::Duration;

    /// Most key events kept for the trace.
    const MAX_TRACE: usize = 600;

    /// The console's key events, read back into the characters that were typed. A terminal's answer to a question reaches
    /// a Windows program, if at all, as the key presses of its characters (see [`bytes_from_keys`]).
    #[derive(Default)]
    pub struct Console {
        /// Every key press read so far, so that what an ESC stands for can be told from what came before it.
        keys: Vec<ReplyKey>,
        /// How many bytes of what those keys stand for have been handed on.
        given: usize,
        trace: Vec<String>,
    }

    impl Source for Console {
        fn read(&mut self, wait: Duration) -> Vec<u8> {
            let mut left = wait;
            while event::poll(left).unwrap_or(false) {
                left = Duration::ZERO;
                let seen = match event::read() {
                    Ok(Event::Key(key)) => {
                        let kind = match key.kind {
                            KeyEventKind::Press => "press",
                            KeyEventKind::Repeat => "repeat",
                            KeyEventKind::Release => "release",
                        };
                        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                        let alt = key.modifiers.contains(KeyModifiers::ALT);
                        let (what, reply_key) = match key.code {
                            KeyCode::Esc => ("Esc".to_string(), Some(ReplyKey::Esc)),
                            KeyCode::Char(c) => (
                                format!("Char {}", super::describe_char(c)),
                                Some(ReplyKey::Char { c, alt, ctrl }),
                            ),
                            other => (format!("other key {other:?}"), None),
                        };
                        if key.kind != KeyEventKind::Release
                            && let Some(k) = reply_key
                        {
                            self.keys.push(k);
                        }
                        format!(
                            "{what}{}{} {kind}",
                            if ctrl { " +ctrl" } else { "" },
                            if alt { " +alt" } else { "" }
                        )
                    }
                    Ok(_) => "an event that is not a key".to_string(),
                    Err(_) => break,
                };
                if self.trace.len() < MAX_TRACE {
                    self.trace.push(seen);
                }
                if self.keys.len() >= super::MAX_REPLY_BYTES {
                    break;
                }
            }
            let all = bytes_from_keys(&self.keys);
            let new = all[self.given.min(all.len())..].to_vec();
            self.given = all.len();
            new
        }

        fn trace(&mut self) -> Vec<String> {
            std::mem::take(&mut self.trace)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::cell::Cell;
    use std::collections::VecDeque;

    /// A terminal that answers what it was scripted to, a piece at a time.
    struct Scripted(VecDeque<Vec<u8>>);

    impl Source for Scripted {
        fn read(&mut self, _wait: Duration) -> Vec<u8> {
            self.0.pop_front().unwrap_or_default()
        }
    }

    fn scripted(pieces: &[&[u8]]) -> Scripted {
        Scripted(pieces.iter().map(|p| p.to_vec()).collect())
    }

    fn env(pairs: &[(&str, &str)]) -> TermEnv {
        TermEnv::from_lookup(|name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        })
    }

    #[test]
    fn the_attributes_question_comes_last_and_the_kitty_one_only_when_asked() {
        let plain = String::from_utf8(questions(false)).unwrap();
        assert_eq!(plain, "\x1b[16t\x1b[14t\x1b[c");
        let kitty = String::from_utf8(questions(true)).unwrap();
        assert!(kitty.starts_with("\x1b[16t\x1b[14t\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"));
        assert!(kitty.ends_with("\x1b[c"));
        // Querying never stores or shows a picture: `a=q`, and the answer is not suppressed (we want it).
        assert!(kitty.contains("a=q"));
    }

    /// A terminal whose attributes answer comes only after `after`, as one that is far away or slow does.
    struct Slow {
        started: Instant,
        after: Duration,
        given: bool,
    }

    impl Source for Slow {
        fn read(&mut self, wait: Duration) -> Vec<u8> {
            std::thread::sleep(wait.min(Duration::from_millis(10)));
            if !self.given && self.started.elapsed() >= self.after {
                self.given = true;
                return b"\x1b[?62;4c".to_vec();
            }
            Vec::new()
        }
    }

    #[test]
    fn running_remotely_never_asks_the_kitty_question_and_waits_longer() {
        assert_eq!(plan(true, false), (true, BUDGET));
        assert_eq!(plan(false, false), (false, BUDGET));
        assert_eq!(plan(true, true), (false, REMOTE_BUDGET));
        assert_eq!(plan(false, true), (false, REMOTE_BUDGET));
        assert!(REMOTE_BUDGET >= Duration::from_millis(800) && REMOTE_BUDGET > BUDGET);
        // What is written: the Kitty question only locally, and never the one whose answer a key reader takes for typing.
        for (remote, kitty_sent) in [(false, true), (true, false)] {
            let mut sink = Vec::new();
            let mut source = scripted(&[b"\x1b[?62;4c"]);
            query_with(&mut sink, &mut source, true, remote).unwrap();
            assert_eq!(
                sink.windows(3).any(|w| w == b"\x1b_G"),
                kitty_sent,
                "remote = {remote}"
            );
        }
    }

    #[test]
    fn an_answer_after_the_local_wait_is_still_read_when_remote() {
        let slow = || Slow {
            started: Instant::now(),
            after: Duration::from_millis(450),
            given: false,
        };
        let local = query_with(&mut Vec::new(), &mut slow(), false, false).unwrap();
        assert!(!local.sixel(), "the local wait had ended: {local:?}");
        let remote = query_with(&mut Vec::new(), &mut slow(), false, true).unwrap();
        assert!(
            remote.sixel(),
            "the remote wait was long enough: {remote:?}"
        );
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> ratatui::crossterm::event::Event {
        ratatui::crossterm::event::Event::Key(KeyEvent::new(code, mods))
    }

    /// What a late Kitty answer (`ESC _ G i=31;OK ESC \`) is read as by a key reader: Alt+`_`, then the characters, then
    /// Alt+`\`.
    fn late_kitty_answer_as_keys() -> Vec<ratatui::crossterm::event::Event> {
        let mut keys = vec![key(KeyCode::Char('_'), KeyModifiers::ALT)];
        keys.extend(
            "Gi=31;OK"
                .chars()
                .map(|c| key(KeyCode::Char(c), KeyModifiers::NONE)),
        );
        keys.push(key(KeyCode::Char('\\'), KeyModifiers::ALT));
        keys
    }

    fn quiet_input(
        mut events: VecDeque<ratatui::crossterm::event::Event>,
    ) -> impl FnMut(Duration) -> Option<ratatui::crossterm::event::Event> {
        move |wait| {
            events.pop_front().or_else(|| {
                std::thread::sleep(wait);
                None
            })
        }
    }

    #[test]
    fn a_late_answer_is_thrown_away_and_a_key_that_quits_is_kept() {
        let mut events: VecDeque<_> = late_kitty_answer_as_keys().into();
        events.push_back(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        events.extend(late_kitty_answer_as_keys());
        events.push_back(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let started = Instant::now();
        let kept = settle(quiet_input(events), is_quit_key, Duration::from_millis(60));
        assert_eq!(
            kept,
            [
                key(KeyCode::Char('q'), KeyModifiers::CONTROL),
                key(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ],
            "only the quits, in order"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(60),
            "the window is waited out, not left early"
        );
    }

    #[test]
    fn only_ctrl_c_and_ctrl_q_pressed_count_as_a_quit() {
        use ratatui::crossterm::event::{Event, KeyEventKind};
        for c in ['c', 'q', 'C', 'Q'] {
            assert!(is_quit_key(&key(KeyCode::Char(c), KeyModifiers::CONTROL)));
        }
        for not in [
            key(KeyCode::Char('q'), KeyModifiers::NONE),
            key(KeyCode::Char('q'), KeyModifiers::ALT),
            key(KeyCode::Char('x'), KeyModifiers::CONTROL),
            key(KeyCode::Esc, KeyModifiers::CONTROL),
            Event::Resize(1, 1),
        ] {
            assert!(!is_quit_key(&not), "{not:?}");
        }
        let mut released = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        released.kind = KeyEventKind::Release;
        assert!(!is_quit_key(&Event::Key(released)));
    }

    #[test]
    fn what_a_terminal_that_never_stops_sends_is_bounded() {
        let mut sent = 0usize;
        let kept = settle(
            |_| {
                sent += 1;
                Some(key(KeyCode::Char('x'), KeyModifiers::NONE))
            },
            is_quit_key,
            Duration::from_secs(5),
        );
        assert!(kept.is_empty());
        assert_eq!(sent, MAX_SETTLE_EVENTS);
    }

    #[test]
    fn the_answers_are_read_in_pieces_and_the_wait_ends_with_the_attributes() {
        let mut sink = Vec::new();
        let mut source = scripted(&[b"\x1b[6;20;", b"10t\x1b[?62;4", b"c", b"\x1b[?9c"]);
        let replies = ask(&mut sink, &mut source, false, Duration::from_secs(5)).unwrap();
        assert_eq!(sink, questions(false), "the questions went to the terminal");
        assert_eq!(replies.cell, Some((10, 20)));
        assert!(replies.sixel());
        assert!(
            source.0.len() <= 1,
            "it stopped at the attributes (plus one read for what was waiting): {} pieces left",
            source.0.len()
        );
    }

    #[test]
    fn a_terminal_that_says_nothing_is_waited_for_only_so_long() {
        let started = Instant::now();
        let mut source = scripted(&[]);
        let replies = ask(
            &mut Vec::new(),
            &mut source,
            true,
            Duration::from_millis(40),
        )
        .unwrap();
        assert_eq!(replies, Replies::default());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn junk_and_forged_text_from_the_terminal_become_nothing() {
        let mut source = scripted(&[b"\x1b]0;rm -rf /\x07\x1b[?x1;2c\x1b[200~boo\x1b[201~"]);
        let replies = ask(
            &mut Vec::new(),
            &mut source,
            false,
            Duration::from_millis(30),
        )
        .unwrap();
        assert_eq!(replies, Replies::default());
    }

    #[test]
    fn nothing_is_asked_when_the_setting_or_the_environment_rules_graphics_out() {
        let asked = Cell::new(0);
        let query = |_: bool| {
            asked.set(asked.get() + 1);
            None
        };
        let art = learn(
            ImageChoice::Art(crate::imgart::ImgMode::Blocks),
            &env(&[("TERM", "xterm-kitty")]),
            None,
            None,
            (100, 40),
            query,
        );
        assert_eq!(art.detected, None);
        let tmux = learn(
            ImageChoice::Auto,
            &env(&[("TMUX", "x"), ("TERM", "xterm-kitty")]),
            None,
            None,
            (100, 40),
            query,
        );
        assert_eq!((tmux.detected, tmux.why), (None, "inside tmux or screen"));
        assert_eq!(asked.get(), 0, "no question went to the terminal");
    }

    #[test]
    fn auto_asks_once_and_believes_the_answer() {
        let asked = Cell::new(0);
        let replies = Replies {
            attributes: Some(vec![62, 4]),
            cell: Some((9, 19)),
            ..Replies::default()
        };
        let facts = learn(
            ImageChoice::Auto,
            &env(&[("TERM", "xterm-256color")]),
            None,
            None,
            (100, 40),
            |kitty| {
                assert!(!kitty, "an unknown terminal is not sent the kitty question");
                asked.set(asked.get() + 1);
                Some(replies)
            },
        );
        assert_eq!(asked.get(), 1);
        assert_eq!(facts.detected, Some(Protocol::Sixel));
        assert_eq!(facts.cell, CellPx::new(9, 19));
        assert!(!facts.cell_from_system);
    }

    #[test]
    fn the_cell_size_is_the_persons_then_the_systems_then_the_terminals() {
        let from_terminal = || {
            Some(Replies {
                cell: Some((9, 19)),
                ..Replies::default()
            })
        };
        let mine = CellPx::new(11, 22);
        let system = CellPx::new(10, 21);
        let wt = env(&[("WT_SESSION", "x")]);
        let facts = learn(ImageChoice::Auto, &wt, mine, system, (100, 40), |_| {
            from_terminal()
        });
        assert_eq!(facts.cell, mine);
        assert!(
            !facts.cell_from_system,
            "a size the person gave is not read again"
        );
        let facts = learn(ImageChoice::Auto, &wt, None, system, (100, 40), |_| {
            from_terminal()
        });
        assert_eq!(facts.cell, system);
        assert!(facts.cell_from_system);
        let facts = learn(ImageChoice::Auto, &wt, None, None, (100, 40), |_| {
            from_terminal()
        });
        assert_eq!(facts.cell, CellPx::new(9, 19));
        // A terminal that does not answer, and a system that does not know: the protocol is still the environment's,
        // and the size is the labelled assumption for Windows Terminal (see the next test).
        let facts = learn(ImageChoice::Auto, &wt, None, None, (100, 40), |_| None);
        assert_eq!(facts.detected, Some(Protocol::Sixel));
        assert_eq!(facts.cell, Some(crate::gfx::ASSUMED_WINDOWS_TERMINAL_CELL));
        assert!(facts.cell_assumed);
    }

    #[test]
    fn windows_terminal_that_says_nothing_gets_a_labelled_assumed_cell_size_and_only_then() {
        let assumed = Some(crate::gfx::ASSUMED_WINDOWS_TERMINAL_CELL);
        let wt = env(&[("WT_SESSION", "x")]);
        let silent = |_: bool| None;
        // Nothing answered, nothing set: the assumption, and it says so.
        let facts = learn(ImageChoice::Auto, &wt, None, None, (100, 40), silent);
        assert_eq!(
            (facts.detected, facts.cell, facts.cell_assumed),
            (Some(Protocol::Sixel), assumed, true)
        );
        // The same with the protocol named.
        let facts = learn(
            ImageChoice::Protocol(Protocol::Sixel),
            &wt,
            None,
            None,
            (100, 40),
            silent,
        );
        assert_eq!((facts.cell, facts.cell_assumed), (assumed, true));
        // The person's size beats it, and so does a reply, and neither is called an assumption.
        let facts = learn(
            ImageChoice::Auto,
            &wt,
            CellPx::new(11, 22),
            None,
            (100, 40),
            silent,
        );
        assert_eq!(
            (facts.cell, facts.cell_assumed),
            (CellPx::new(11, 22), false)
        );
        let reply = |_: bool| {
            Some(Replies {
                attributes: Some(vec![61, 4]),
                cell: Some((10, 20)),
                ..Replies::default()
            })
        };
        let facts = learn(ImageChoice::Auto, &wt, None, None, (100, 40), reply);
        assert_eq!(
            (facts.cell, facts.cell_assumed),
            (CellPx::new(10, 20), false)
        );
        // An answer that lacks Sixel still means no graphics, and then there is nothing to assume a size for.
        let no_sixel = |_: bool| {
            Some(Replies {
                attributes: Some(vec![61, 6]),
                ..Replies::default()
            })
        };
        let facts = learn(ImageChoice::Auto, &wt, None, None, (100, 40), no_sixel);
        assert_eq!(
            (facts.detected, facts.cell, facts.cell_assumed),
            (None, None, false)
        );
        // A terminal that lists Sixel but gives no size, in Windows Terminal: assumed; anywhere else: unknown.
        let sixel_only = |_: bool| {
            Some(Replies {
                attributes: Some(vec![61, 4]),
                ..Replies::default()
            })
        };
        let facts = learn(ImageChoice::Auto, &wt, None, None, (100, 40), sixel_only);
        assert_eq!((facts.cell, facts.cell_assumed), (assumed, true));
        let facts = learn(
            ImageChoice::Auto,
            &env(&[("TERM", "foot")]),
            None,
            None,
            (100, 40),
            sixel_only,
        );
        assert_eq!(
            (facts.detected, facts.cell, facts.cell_assumed),
            (Some(Protocol::Sixel), None, false)
        );
        // Other protocols, and nothing detected, are never given an assumed size.
        let facts = learn(
            ImageChoice::Protocol(Protocol::Kitty),
            &wt,
            None,
            None,
            (100, 40),
            silent,
        );
        assert_eq!((facts.cell, facts.cell_assumed), (None, false));
        let facts = learn(
            ImageChoice::Auto,
            &env(&[("TERM", "xterm-256color")]),
            None,
            None,
            (100, 40),
            silent,
        );
        assert_eq!(
            (facts.detected, facts.cell, facts.cell_assumed),
            (None, None, false)
        );
        // The system's own size is a size told, not assumed.
        let facts = learn(
            ImageChoice::Auto,
            &wt,
            None,
            CellPx::new(12, 24),
            (100, 40),
            silent,
        );
        assert_eq!(
            (facts.cell, facts.cell_assumed),
            (CellPx::new(12, 24), false)
        );
    }

    #[test]
    fn an_escape_the_console_turns_into_ctrl_bracket_or_loses_is_put_back() {
        let ch = |c| ReplyKey::Char {
            c,
            alt: false,
            ctrl: false,
        };
        let ctrl = |c| ReplyKey::Char {
            c,
            alt: false,
            ctrl: true,
        };
        let text = |s: &str| s.chars().map(ch).collect::<Vec<_>>();
        let expected = b"\x1b[?61;4;6c".to_vec();
        // As an ESC key event, as Ctrl+[ (how a keyboard types it) followed by the rest, or not at all.
        let mut as_esc = vec![ReplyKey::Esc];
        as_esc.extend(text("[?61;4;6c"));
        assert_eq!(bytes_from_keys(&as_esc), expected);
        let mut as_ctrl = vec![ctrl('[')];
        as_ctrl.extend(text("[?61;4;6c"));
        assert_eq!(bytes_from_keys(&as_ctrl), expected);
        assert_eq!(
            bytes_from_keys(&text("[?61;4;6c")),
            expected,
            "the ESC was lost"
        );
        // Two answers in a row.
        let mut two = text("[6;20;10t");
        two.extend(text("[?61;4c"));
        assert_eq!(bytes_from_keys(&two), b"\x1b[6;20;10t\x1b[?61;4c");
        // What is rebuilt is what the strict reader reads.
        let replies = parse_replies(&bytes_from_keys(&two));
        assert_eq!((replies.cell, replies.sixel()), (Some((10, 20)), true));
        // Alt+x is ESC x as ever; plain text has no ESC put in.
        assert_eq!(
            bytes_from_keys(&[ReplyKey::Char {
                c: 'x',
                alt: true,
                ctrl: false
            }]),
            b"\x1bx"
        );
        assert_eq!(bytes_from_keys(&text("abc")), b"abc");
        assert_eq!(bytes_from_keys(&[]), b"");
    }

    /// The keys a Windows console hands over, as `bytes_from_keys` gets them, for what the strict reader makes of
    /// arbitrary typing in the moment the question is asked: only the exact numeric answers survive.
    #[test]
    fn typing_during_the_question_cannot_become_an_answer() {
        let keys: Vec<ReplyKey> = "hello [?1;2;3x world [?c [6;1;1;1t"
            .chars()
            .map(|c| ReplyKey::Char {
                c,
                alt: false,
                ctrl: false,
            })
            .collect();
        assert_eq!(parse_replies(&bytes_from_keys(&keys)), Replies::default());
    }

    /// What came back from a terminal is shown in a trace as text that is safe to print: a printable character with its
    /// number, anything else (an ESC, a control character, a bidi override) by its number alone.
    #[test]
    fn a_character_from_a_terminal_is_described_without_ever_being_printed_raw() {
        assert_eq!(describe_char('['), "U+005B '['");
        assert_eq!(describe_char('c'), "U+0063 'c'");
        assert_eq!(describe_char(' '), "U+0020 ' '");
        for raw in ['\u{1b}', '\u{7}', '\u{9b}', '\u{202e}', '\n', 'é'] {
            let shown = describe_char(raw);
            assert_eq!(shown, format!("U+{:04X}", u32::from(raw)));
            assert!(
                shown.chars().all(|c| c.is_ascii_graphic()),
                "{shown:?} must be plain ASCII"
            );
        }
    }

    #[test]
    fn nothing_is_asked_where_there_is_no_terminal() {
        use std::io::IsTerminal;
        // Under the test harness the output is captured, so this is not a terminal; run by hand with --nocapture in a
        // real one, the question would be a real one and is not asked here.
        if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
            assert_eq!(query_terminal(true), None);
            assert_eq!(query_terminal(false), None);
        }
    }

    #[test]
    fn a_protocol_chosen_by_name_asks_only_for_what_it_lacks() {
        let asked = Cell::new(0);
        let query = |_: bool| {
            asked.set(asked.get() + 1);
            Some(Replies {
                cell: Some((9, 19)),
                ..Replies::default()
            })
        };
        let facts = learn(
            ImageChoice::Protocol(Protocol::Sixel),
            &env(&[]),
            CellPx::new(10, 20),
            None,
            (100, 40),
            query,
        );
        assert_eq!((asked.get(), facts.cell), (0, CellPx::new(10, 20)));
        let facts = learn(
            ImageChoice::Protocol(Protocol::Kitty),
            &env(&[("TMUX", "x")]),
            None,
            None,
            (100, 40),
            query,
        );
        assert_eq!(
            asked.get(),
            1,
            "forcing a protocol works inside tmux, whose setup is the person's"
        );
        assert_eq!(facts.cell, CellPx::new(9, 19));
        assert_eq!(facts.detected, None, "no detection happens");
    }
}
