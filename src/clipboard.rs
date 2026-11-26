//! The clipboard, behind a trait so the app can be tested without touching the real one.
//!
//! Two ways to reach it. The operating system's (`arboard`) is the clipboard of the machine vtteams runs on, which is
//! the right one only when that is also where the person sits. The terminal's, set with an OSC 52 escape sequence, is
//! the clipboard of the machine the terminal runs on, so it also works over SSH. [`Layered`] chooses between them.
//!
//! Only *writing* through the terminal is supported, never reading. A read is a query the terminal answers on stdin
//! with the clipboard's contents; anything that can print to the terminal (a remote host, a hostile file that is
//! `cat`ed) could make that happen and see what was last copied, a password for instance. Pasting therefore comes from
//! the system clipboard when vtteams runs locally, and otherwise from the terminal's own paste (which arrives as a
//! bracketed-paste event) or from the last thing copied inside vtteams.
//!
//! A clipboard call can block (another program holding it open, a remote session that is slow to answer), so the
//! system clipboard does its work on a thread of its own (one for the program's life, which keeps the clipboard open) and
//! the UI gives up waiting for an answer after [`TIMEOUT`] instead of freezing.

use std::io::Write;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long the UI waits for the system clipboard.
const TIMEOUT: Duration = Duration::from_millis(400);

/// The most text sent through the terminal in one copy. Terminals cap the whole sequence (xterm at 100,000 bytes) and
/// some drop an oversize one without a word, so a larger copy is refused here, where it can be said.
pub const MAX_TERMINAL_COPY: usize = 74_994;

/// Where a copy went.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Via {
    /// The operating system's clipboard.
    System,
    /// The terminal was asked to set its clipboard; it does not say whether it did.
    Terminal,
}

/// Which clipboard to write to (the `clipboard` setting).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    /// The system clipboard, or the terminal's when vtteams runs remotely or the system clipboard fails.
    #[default]
    Auto,
    /// Only the system clipboard.
    System,
    /// Only the terminal's (OSC 52).
    Osc52,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "system" => Some(Self::System),
            "osc52" => Some(Self::Osc52),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::System => "system",
            Self::Osc52 => "osc52",
        }
    }
}

pub trait Clipboard: Send + Sync + std::fmt::Debug {
    /// The clipboard's text, or `None` if it holds none or cannot be read.
    fn get(&self) -> Option<String>;

    /// Replace the clipboard's text, and say which clipboard it went to.
    ///
    /// # Errors
    /// A message describing why the clipboard could not be written.
    fn set(&self, text: &str) -> Result<Via, String>;
}

enum Request {
    Get(Sender<Option<String>>),
    Set(String, Sender<Result<(), String>>),
}

/// A clipboard worker on a dedicated thread, holding a store `S` for its lifetime. The store is created on that thread
/// and never leaves it, so `S` need not be `Send` (the `fn() -> S` marker keeps the worker `Send` and `Sync` either way).
#[derive(Debug)]
struct Worker<S> {
    tx: Sender<Request>,
    _store: std::marker::PhantomData<fn() -> S>,
}

impl<S: ClipboardStore + 'static> Worker<S> {
    fn new() -> Self {
        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("vtteams-clipboard".into())
            .spawn(move || {
                let mut store: Option<S> = None;
                // Created on first use, and again on the next request if creating it failed.
                let open = |store: &mut Option<S>| -> Result<(), String> {
                    if store.is_none() {
                        *store = Some(S::new()?);
                    }
                    Ok(())
                };
                while let Ok(req) = rx.recv() {
                    match req {
                        Request::Get(reply) => {
                            let result = open(&mut store)
                                .ok()
                                .and_then(|()| store.as_mut().and_then(|s| s.get()));
                            let _ = reply.send(result);
                        }
                        Request::Set(text, reply) => {
                            let result = open(&mut store).and_then(|()| match store.as_mut() {
                                Some(s) => s.set(text),
                                None => Err("the clipboard could not be opened".to_string()),
                            });
                            let _ = reply.send(result);
                        }
                    }
                }
            })
            .ok();
        Self {
            tx,
            _store: std::marker::PhantomData,
        }
    }

    fn get(&self) -> Option<String> {
        let (reply_tx, reply_rx) = channel();
        self.tx.send(Request::Get(reply_tx)).ok()?;
        reply_rx.recv_timeout(TIMEOUT).ok().flatten()
    }

    fn set(&self, text: &str) -> Result<(), String> {
        let (reply_tx, reply_rx) = channel();
        self.tx
            .send(Request::Set(text.to_string(), reply_tx))
            .map_err(|_| "clipboard thread ended".to_string())?;
        reply_rx
            .recv_timeout(TIMEOUT)
            .map_err(|_| "the clipboard did not answer".to_string())?
    }
}

trait ClipboardStore {
    fn new() -> Result<Self, String>
    where
        Self: Sized;
    fn get(&mut self) -> Option<String>;
    fn set(&mut self, text: String) -> Result<(), String>;
}

struct ArboardStore(arboard::Clipboard);

impl std::fmt::Debug for ArboardStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArboardStore").finish_non_exhaustive()
    }
}

impl ClipboardStore for ArboardStore {
    fn new() -> Result<Self, String> {
        arboard::Clipboard::new()
            .map(Self)
            .map_err(|e| e.to_string())
    }

    fn get(&mut self) -> Option<String> {
        self.0.get_text().ok()
    }

    fn set(&mut self, text: String) -> Result<(), String> {
        self.0.set_text(text).map_err(|e| e.to_string())
    }
}

/// The operating system's clipboard.
#[derive(Debug)]
pub struct System {
    worker: Worker<ArboardStore>,
}

impl System {
    pub fn new() -> Self {
        Self {
            worker: Worker::new(),
        }
    }
}

impl Default for System {
    fn default() -> Self {
        Self::new()
    }
}

impl Clipboard for System {
    fn get(&self) -> Option<String> {
        self.worker.get()
    }

    fn set(&self, text: &str) -> Result<Via, String> {
        self.worker.set(text).map(|()| Via::System)
    }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (the encoding OSC 52 carries, and the graphics protocols' payloads).
pub(crate) fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The escape sequence that asks a terminal to put `text` on its clipboard: `OSC 52 ; c ; <base64> BEL`.
///
/// # Errors
/// If `text` is larger than [`MAX_TERMINAL_COPY`] bytes.
pub fn osc52_sequence(text: &str) -> Result<String, String> {
    if text.len() > MAX_TERMINAL_COPY {
        return Err(format!(
            "{} KB is too much for the terminal's clipboard (the limit is {} KB)",
            text.len().div_ceil(1000),
            MAX_TERMINAL_COPY / 1000
        ));
    }
    Ok(format!("\x1b]52;c;{}\x07", base64(text.as_bytes())))
}

/// The terminal's clipboard, set by writing an OSC 52 sequence to `W` (the terminal). Cannot be read.
#[derive(Debug)]
pub struct Terminal<W: Write + Send> {
    out: Mutex<W>,
}

impl<W: Write + Send> Terminal<W> {
    pub fn new(out: W) -> Self {
        Self {
            out: Mutex::new(out),
        }
    }
}

impl<W: Write + Send + std::fmt::Debug> Clipboard for Terminal<W> {
    fn get(&self) -> Option<String> {
        None
    }

    fn set(&self, text: &str) -> Result<Via, String> {
        let sequence = osc52_sequence(text)?;
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        out.write_all(sequence.as_bytes())
            .and_then(|()| out.flush())
            .map_err(|e| format!("cannot write to the terminal: {e}"))?;
        Ok(Via::Terminal)
    }
}

/// Chooses between the system clipboard and the terminal's according to the [`Mode`] and where vtteams runs.
#[derive(Debug)]
pub struct Layered {
    mode: Mode,
    /// vtteams runs on another machine than the terminal (an SSH session) or has no display of its own, so the system
    /// clipboard is not the one in front of the person.
    remote: bool,
    system: Arc<dyn Clipboard>,
    terminal: Arc<dyn Clipboard>,
}

impl Layered {
    pub fn new(
        mode: Mode,
        remote: bool,
        system: Arc<dyn Clipboard>,
        terminal: Arc<dyn Clipboard>,
    ) -> Self {
        Self {
            mode,
            remote,
            system,
            terminal,
        }
    }

    /// Whether copies go to the terminal before the system clipboard is tried.
    fn terminal_first(&self) -> bool {
        match self.mode {
            Mode::Osc52 => true,
            Mode::System => false,
            Mode::Auto => self.remote,
        }
    }
}

impl Clipboard for Layered {
    /// The system clipboard, unless vtteams runs remotely: the remote machine's clipboard is not what the person copied.
    fn get(&self) -> Option<String> {
        match self.mode {
            Mode::Osc52 => None,
            Mode::Auto if self.remote => None,
            _ => self.system.get(),
        }
    }

    fn set(&self, text: &str) -> Result<Via, String> {
        match (self.mode, self.terminal_first()) {
            (Mode::System, _) => self.system.set(text),
            (_, true) => self.terminal.set(text),
            // Locally the system clipboard; the terminal's if that is not available.
            (_, false) => self.system.set(text).or_else(|why| {
                self.terminal
                    .set(text)
                    .map_err(|also| format!("{why}; and through the terminal: {also}"))
            }),
        }
    }
}

/// A clipboard held in memory, for tests.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct Memory(std::sync::Mutex<Option<String>>);

#[cfg(test)]
impl Memory {
    /// Put `text` on the clipboard as another program would.
    pub fn put(&self, text: &str) {
        *self.0.lock().unwrap() = Some(text.to_string());
    }
}

#[cfg(test)]
impl Clipboard for Memory {
    fn get(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }

    fn set(&self, text: &str) -> Result<Via, String> {
        self.put(text);
        Ok(Via::System)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MemoryStore(String);

    impl ClipboardStore for MemoryStore {
        fn new() -> Result<Self, String> {
            Ok(Self(String::new()))
        }

        fn get(&mut self) -> Option<String> {
            if self.0.is_empty() {
                None
            } else {
                Some(self.0.clone())
            }
        }

        fn set(&mut self, text: String) -> Result<(), String> {
            self.0 = text;
            Ok(())
        }
    }

    /// A clipboard another program is holding: every call takes far longer than the UI waits.
    struct StuckStore;

    impl ClipboardStore for StuckStore {
        fn new() -> Result<Self, String> {
            Ok(Self)
        }

        fn get(&mut self) -> Option<String> {
            std::thread::sleep(TIMEOUT * 5);
            Some("late".into())
        }

        fn set(&mut self, _: String) -> Result<(), String> {
            std::thread::sleep(TIMEOUT * 5);
            Ok(())
        }
    }

    #[test]
    fn a_clipboard_that_does_not_answer_is_given_up_on_not_waited_for() {
        let worker: Worker<StuckStore> = Worker::new();
        let started = std::time::Instant::now();
        assert_eq!(worker.get(), None);
        assert!(
            started.elapsed() < TIMEOUT * 3,
            "gave up promptly: {:?}",
            started.elapsed()
        );
        let started = std::time::Instant::now();
        assert!(
            worker.set("text").is_err(),
            "a write that was not answered is not called done"
        );
        assert!(
            started.elapsed() < TIMEOUT * 3,
            "gave up promptly: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn worker_serves_requests_in_order_and_holds_state() {
        let worker: Worker<MemoryStore> = Worker::new();
        assert_eq!(worker.set("first"), Ok(()));
        assert_eq!(worker.set("second"), Ok(()));
        assert_eq!(worker.get(), Some("second".to_string()));
    }

    /// A terminal whose output the test can read.
    #[derive(Clone, Debug, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Sink {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    /// A system clipboard that cannot be used.
    #[derive(Debug)]
    struct Broken;

    impl Clipboard for Broken {
        fn get(&self) -> Option<String> {
            None
        }
        fn set(&self, _: &str) -> Result<Via, String> {
            Err("no display".into())
        }
    }

    fn layered(mode: Mode, remote: bool, system_works: bool) -> (Layered, Arc<Memory>, Sink) {
        let memory = Arc::new(Memory::default());
        let sink = Sink::default();
        let system: Arc<dyn Clipboard> = if system_works {
            memory.clone()
        } else {
            Arc::new(Broken)
        };
        let terminal = Arc::new(Terminal::new(sink.clone()));
        (Layered::new(mode, remote, system, terminal), memory, sink)
    }

    #[test]
    fn base64_matches_the_standard_test_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            ("héllo ✓", "aMOpbGxvIOKckw=="),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain:?}");
        }
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn the_sequence_carries_the_text_only_as_base64_and_is_bounded() {
        assert_eq!(osc52_sequence("hi").unwrap(), "\x1b]52;c;aGk=\x07");
        // Whatever the text holds, including a sequence of its own, nothing but base64 is between the markers.
        let hostile = "\x1b]52;c;?\x07 \x1b[2J";
        let seq = osc52_sequence(hostile).unwrap();
        assert_eq!(seq.matches('\x1b').count(), 1);
        assert_eq!(seq.matches('\x07').count(), 1);
        let largest = "x".repeat(MAX_TERMINAL_COPY);
        assert!(osc52_sequence(&largest).unwrap().len() <= 100_000);
        let too_big = "x".repeat(MAX_TERMINAL_COPY + 1);
        assert!(osc52_sequence(&too_big).unwrap_err().contains("too much"));
    }

    #[test]
    fn the_terminal_clipboard_writes_the_sequence_and_cannot_be_read() {
        let sink = Sink::default();
        let terminal = Terminal::new(sink.clone());
        assert_eq!(terminal.set("hi"), Ok(Via::Terminal));
        assert_eq!(sink.text(), "\x1b]52;c;aGk=\x07");
        assert_eq!(terminal.get(), None);
        assert!(terminal.set(&"x".repeat(MAX_TERMINAL_COPY + 1)).is_err());
        assert_eq!(
            sink.text(),
            "\x1b]52;c;aGk=\x07",
            "nothing partial was written"
        );
    }

    #[test]
    fn locally_the_system_clipboard_is_used_and_the_terminal_is_the_fallback() {
        let (clip, memory, sink) = layered(Mode::Auto, false, true);
        assert_eq!(clip.set("one"), Ok(Via::System));
        assert_eq!(memory.get().as_deref(), Some("one"));
        assert_eq!(sink.text(), "", "the terminal was not asked");
        assert_eq!(clip.get().as_deref(), Some("one"));

        let (clip, _, sink) = layered(Mode::Auto, false, false);
        assert_eq!(clip.set("two"), Ok(Via::Terminal));
        assert!(sink.text().contains("]52;c;"));
        // Both failing says so, naming both reasons.
        let (clip, _, _) = layered(Mode::Auto, false, false);
        let why = clip.set(&"x".repeat(MAX_TERMINAL_COPY + 1)).unwrap_err();
        assert!(
            why.contains("no display") && why.contains("too much"),
            "{why}"
        );
    }

    #[test]
    fn remotely_the_terminal_is_used_and_the_remote_clipboard_is_not_read() {
        let (clip, memory, sink) = layered(Mode::Auto, true, true);
        memory.put("whatever is on the server");
        assert_eq!(clip.set("one"), Ok(Via::Terminal));
        assert!(sink.text().contains("]52;c;"));
        assert_eq!(
            memory.get().as_deref(),
            Some("whatever is on the server"),
            "not written"
        );
        assert_eq!(clip.get(), None, "not read either");
    }

    #[test]
    fn the_setting_can_force_either_clipboard() {
        let (clip, memory, sink) = layered(Mode::System, true, true);
        assert_eq!(clip.set("a"), Ok(Via::System));
        assert_eq!(
            (memory.get().as_deref(), sink.text().as_str()),
            (Some("a"), "")
        );
        assert_eq!(clip.get().as_deref(), Some("a"), "forced, so read");
        let (clip, memory, sink) = layered(Mode::Osc52, false, true);
        assert_eq!(clip.set("b"), Ok(Via::Terminal));
        assert!(memory.get().is_none() && sink.text().contains("]52;c;"));
        assert_eq!(clip.get(), None);
    }

    #[test]
    fn modes_parse_and_print() {
        for mode in [Mode::Auto, Mode::System, Mode::Osc52] {
            assert_eq!(Mode::parse(mode.label()), Some(mode));
        }
        assert_eq!(Mode::parse(" OSC52 "), Some(Mode::Osc52));
        assert_eq!(Mode::parse("tmux"), None);
        assert_eq!(Mode::default(), Mode::Auto);
    }

    #[test]
    fn the_memory_clipboard_round_trips() {
        let clip = Memory::default();
        assert_eq!(clip.get(), None);
        clip.set("hello").unwrap();
        assert_eq!(clip.get().as_deref(), Some("hello"));
    }
}
