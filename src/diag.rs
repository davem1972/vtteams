//! The diagnostics file, `vtteams.log` in the config directory.
//!
//! The terminal belongs to the UI, so what is worth knowing but not worth interrupting the user for (a worker panic, a
//! failed poll, how a read was done, a field the service has not sent before) goes to this file through the `log`
//! facade. `teams-client` logs through the same facade, so one file tells the whole story: requests with their
//! status and timing, incremental versus full reads, polling decisions, slow frames, and changes in what the services
//! send. Records carry no message text, names, tokens or whole identifiers.
//!
//! Records from vtteams's own crates are written at the chosen level; every other crate is held to warnings and errors so
//! the HTTP and TLS libraries do not bury the file. A failure to write is deliberately ignored: this is the channel of
//! last resort and has nowhere left to report to.

use crate::config;
use log::{LevelFilter, Log, Metadata, Record};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// Rotate the file once it passes this size, keeping one previous generation (`vtteams.log.1`). Debug builds trace, so
/// they get more room.
const MAX_BYTES: u64 = if cfg!(debug_assertions) {
    8 * 1024 * 1024
} else {
    1024 * 1024
};
/// Longest record written; the rest is cut off.
const MAX_RECORD_CHARS: usize = 4_000;

/// Install the file logger at `level` and return the file's path: `Ok(None)` when logging is off, `Err` with the
/// reason when it was asked for and cannot be had (the app then runs without a log, and says so).
pub fn init(level: LevelFilter) -> Result<Option<PathBuf>, String> {
    if level == LevelFilter::Off {
        return Ok(None);
    }
    let dir = config::config_dir()?;
    teams_client::create_private_dir(&dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join("vtteams.log");
    let logger = FileLog::open(path.clone(), level, MAX_BYTES)
        .ok_or_else(|| format!("cannot open {}", path.display()))?;
    // The logger lives as long as the process, so leaking it is how a `'static` logger is made.
    log::set_logger(Box::leak(Box::new(logger))).map_err(|_| "a logger is already installed")?;
    log::set_max_level(level);
    Ok(Some(path))
}

/// Whether `target` (a module path) belongs to vtteams or its client library.
fn ours(target: &str) -> bool {
    ["vtteams", "teams_client"].iter().any(|name| {
        target
            .strip_prefix(name)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

/// A record on one line, safe to read in any editor: control characters are shown escaped, so text that came from the
/// network cannot forge lines or move a cursor.
fn single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_RECORD_CHARS));
    for (n, c) in text.chars().enumerate() {
        if n == MAX_RECORD_CHARS {
            out.push_str("...");
            break;
        }
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push(' '),
            c if c.is_control() || is_invisible_format(c) => {
                out.push_str(&format!("\\u{{{:x}}}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Characters that change how text is laid out or read without being drawn: line and paragraph separators, zero-width
/// marks, and the bidi controls. They are shown escaped like control characters.
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{2028}' | '\u{2029}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{061C}' | '\u{FEFF}'
    )
}

struct Sink {
    path: PathBuf,
    file: File,
    written: u64,
    max: u64,
    /// Neither rotating nor starting the file over worked: nothing more is written rather than let it grow.
    stopped: bool,
}

impl Sink {
    fn open(path: &Path) -> Option<(File, u64)> {
        Self::open_with(path, false)
    }

    /// The file for appending, or (`start_over`) emptied first.
    fn open_with(path: &Path, start_over: bool) -> Option<(File, u64)> {
        let mut options = OpenOptions::new();
        options.create(true);
        if start_over {
            options.write(true).truncate(true);
        } else {
            options.append(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path).ok()?;
        let len = file.metadata().map_or(0, |m| m.len());
        Some((file, len))
    }

    fn write_line(&mut self, line: &str) {
        if self.stopped {
            return;
        }
        let bytes = line.len() as u64 + 1;
        if self.written + bytes > self.max {
            let mut previous = self.path.clone().into_os_string();
            previous.push(".1");
            let rotated = std::fs::rename(&self.path, previous).is_ok();
            // If the old generation cannot be kept (something is in the way, or the file is held open), the file is
            // started over instead, so a log that cannot rotate does not grow without limit.
            match Self::open_with(&self.path, !rotated) {
                Some((file, len)) => {
                    self.file = file;
                    self.written = len;
                }
                None => {
                    self.stopped = true;
                    return;
                }
            }
        }
        if writeln!(self.file, "{line}").is_ok() {
            self.written += bytes;
        }
    }
}

struct FileLog {
    level: LevelFilter,
    sink: Mutex<Sink>,
}

impl FileLog {
    fn open(path: PathBuf, level: LevelFilter, max: u64) -> Option<Self> {
        let (file, written) = Sink::open(&path)?;
        Some(Self {
            level,
            sink: Mutex::new(Sink {
                path,
                file,
                written,
                max,
                stopped: false,
            }),
        })
    }

    fn allows(&self, metadata: &Metadata<'_>) -> bool {
        let limit = if ours(metadata.target()) {
            self.level
        } else {
            self.level.min(LevelFilter::Warn)
        };
        metadata.level() <= limit
    }
}

impl Log for FileLog {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.allows(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        if !self.allows(record.metadata()) {
            return;
        }
        let thread = std::thread::current();
        let line = format!(
            "{} {:<5} [{}] {}: {}",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"),
            record.level(),
            thread.name().unwrap_or("?"),
            record.target(),
            single_line(&record.args().to_string())
        );
        // A holder that panicked cannot have left the sink half written in any way that matters to a log.
        let mut sink = self.sink.lock().unwrap_or_else(PoisonError::into_inner);
        sink.write_line(&line);
    }

    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Level;

    fn temp_dir(name: &str) -> crate::testdir::TestDir {
        crate::testdir::TestDir::new(&format!("diag-{name}"))
    }

    fn record(logger: &FileLog, level: Level, target: &str, message: &str) {
        logger.log(
            &Record::builder()
                .level(level)
                .target(target)
                .args(format_args!("{message}"))
                .build(),
        );
    }

    #[test]
    fn our_crates_are_logged_at_the_chosen_level_and_others_only_from_warnings() {
        let dir = temp_dir("levels");
        let path = dir.join("vtteams.log");
        let logger = FileLog::open(path.clone(), LevelFilter::Trace, 1 << 20).unwrap();
        record(&logger, Level::Trace, "vtteams::app", "ours, trace");
        record(
            &logger,
            Level::Debug,
            "teams_client::teams",
            "library, debug",
        );
        record(&logger, Level::Debug, "ureq::run", "dependency, debug");
        record(&logger, Level::Warn, "ureq::run", "dependency, warn");
        record(
            &logger,
            Level::Debug,
            "vtteams_other::x",
            "a similar name, debug",
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("ours, trace") && text.contains("library, debug"));
        assert!(text.contains("dependency, warn"));
        assert!(!text.contains("dependency, debug") && !text.contains("similar name"));
        let quiet = FileLog::open(dir.join("q.log"), LevelFilter::Warn, 1 << 20).unwrap();
        record(&quiet, Level::Info, "vtteams::app", "info");
        record(&quiet, Level::Error, "vtteams::app", "error");
        let text = std::fs::read_to_string(dir.join("q.log")).unwrap();
        assert!(text.contains("error") && !text.contains("info"));
    }

    #[test]
    fn a_record_is_one_line_with_time_level_thread_and_target() {
        let dir = temp_dir("format");
        let path = dir.join("vtteams.log");
        let logger = FileLog::open(path.clone(), LevelFilter::Info, 1 << 20).unwrap();
        record(&logger, Level::Info, "vtteams::app", "hello");
        let text = std::fs::read_to_string(&path).unwrap();
        let line = text.lines().next().unwrap();
        assert!(line.contains(" INFO  ["), "{line}");
        assert!(line.contains("vtteams::app: hello"), "{line}");
        assert!(line.starts_with("20") && line.contains('Z'), "{line}");
        assert_eq!(text.lines().count(), 1);
    }

    #[test]
    fn text_from_the_network_cannot_forge_lines_or_carry_escape_sequences() {
        let line = single_line("a\nFORGED 2026 INFO [x] y\r\u{1b}[31mred\u{7}\ttab");
        assert!(!line.contains('\n') && !line.contains('\r') && !line.contains('\u{1b}'));
        assert!(line.contains("\\n") && line.contains("\\u{1b}") && line.contains("\\u{7}"));
        let long = single_line(&"x".repeat(MAX_RECORD_CHARS + 100));
        assert_eq!(long.chars().count(), MAX_RECORD_CHARS + 3);
    }

    #[test]
    fn layout_controls_and_bidi_marks_are_shown_escaped() {
        let line = single_line("a\u{2028}b\u{202E}c\u{200B}d\u{FEFF}e");
        assert_eq!(line, "a\\u{2028}b\\u{202e}c\\u{200b}d\\u{feff}e");
    }

    #[test]
    fn a_file_that_cannot_rotate_is_started_over_instead_of_growing() {
        let dir = temp_dir("stuck");
        let path = dir.join("vtteams.log");
        // A directory where the previous generation would go makes the rename fail.
        std::fs::create_dir(dir.join("vtteams.log.1")).unwrap();
        let logger = FileLog::open(path.clone(), LevelFilter::Info, 400).unwrap();
        for n in 0..60 {
            record(
                &logger,
                Level::Info,
                "vtteams::app",
                &format!("record number {n}"),
            );
        }
        let current = std::fs::metadata(&path).unwrap().len();
        assert!(current <= 400, "{current}");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("record number 59")
        );
    }

    #[test]
    fn the_file_rotates_keeping_one_previous_generation() {
        let dir = temp_dir("rotate");
        let path = dir.join("vtteams.log");
        let logger = FileLog::open(path.clone(), LevelFilter::Info, 400).unwrap();
        for n in 0..30 {
            record(
                &logger,
                Level::Info,
                "vtteams::app",
                &format!("record number {n}"),
            );
        }
        let current = std::fs::metadata(&path).unwrap().len();
        let previous = std::fs::metadata(dir.join("vtteams.log.1")).unwrap().len();
        assert!(current <= 400 && previous <= 400, "{current} {previous}");
        let last = std::fs::read_to_string(&path).unwrap();
        assert!(last.contains("record number 29"));
        assert!(!dir.join("vtteams.log.2").exists());
    }
}
