//! `vtteams --terminal-report`: what start-up learns about the terminal, shown step by step, for working out why pictures
//! are or are not drawn with graphics in a terminal.
//!
//! It runs the same questions, the same wait and the same reader as start-up (`probe::ask_traced`), then a few more
//! labelled as extra, and prints what the environment said, what was sent, every byte that came back, how long the
//! wait took, what the strict reader kept and dropped, and the decision with its reason. Whatever the terminal sent is
//! untrusted, so it is only ever printed escaped: the characters ESC and backslash, every control character and every
//! byte above 0x7E become `\x..` forms, and nothing else is shown.

use super::detect::{self, MAX_REPLY_BYTES, Replies, TermEnv, parse_replies_counted};
use super::probe::{self, Asked, Facts, Source};
use super::{CellPx, ImageChoice, Protocol};
use std::fmt::Write as _;
use std::io::Write;
use std::time::{Duration, Instant};

/// The most characters of an environment value that are shown.
const MAX_VALUE: usize = 80;
/// The most key events listed.
const MAX_EVENTS_SHOWN: usize = 60;
/// How long the extra probes wait for an answer, and how long after the first byte for the rest.
const PROBE_WAIT: Duration = Duration::from_millis(300);
const PROBE_SETTLE: Duration = Duration::from_millis(40);
/// How long to listen after start-up's own wait is over, for answers that were slower than it allows.
const LATE_WAIT: Duration = Duration::from_millis(700);

/// The environment variables that decide which protocol is expected, with what they hold.
pub const VARIABLES: [&str; 7] = [
    "TERM",
    "TERM_PROGRAM",
    "KITTY_WINDOW_ID",
    "WT_SESSION",
    "ITERM_SESSION_ID",
    "TMUX",
    "STY",
];

/// `bytes` as text that is safe to print anywhere: printable ASCII as it is, backslash as `\\`, and every other byte as
/// `\xNN` (so ESC is `\x1b`).
pub fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(char::from(b)),
            _ => {
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out
}

/// An environment value cut to [`MAX_VALUE`] characters and escaped.
fn value_text(value: &str) -> String {
    let cut: Vec<u8> = value.bytes().take(MAX_VALUE).collect();
    let more = if value.len() > MAX_VALUE { "..." } else { "" };
    format!("{}{more}", escape(&cut))
}

/// A question sent as part of a probe, and what came back.
#[derive(Debug)]
pub struct Probe {
    pub name: &'static str,
    pub sent: Vec<u8>,
    pub got: Vec<u8>,
    pub elapsed: Duration,
}

/// Everything the report says, gathered.
#[derive(Debug)]
pub struct Findings {
    pub version: String,
    pub system: &'static str,
    /// Standard input and output are terminals.
    pub interactive: bool,
    pub size: Option<(u16, u16)>,
    pub variables: Vec<(&'static str, Option<String>)>,
    pub env: TermEnv,
    /// This runs over a connection to another computer (`config::runs_remotely`), which changes what start-up asks.
    pub remote: bool,
    pub system_cell: Option<CellPx>,
    /// The console API's own idea of the font size (Windows only; informational).
    pub console_font: Option<(i32, i32)>,
    pub asked: Option<Asked>,
    /// The key events the system handed over before they became bytes.
    pub events: Vec<String>,
    /// Bytes that arrived after the wait was over.
    pub late: Vec<u8>,
    pub probes: Vec<Probe>,
    pub facts: Facts,
}

/// The names of the questions in [`probe::questions`], in the order they are asked.
fn question_names(ask_kitty: bool) -> Vec<&'static str> {
    let mut names = vec![
        "cell size in pixels (CSI 16 t)",
        "text area in pixels (CSI 14 t)",
    ];
    if ask_kitty {
        names.push("Kitty graphics query (APC G ... a=q)");
    }
    names.push("device attributes (CSI c), always last");
    names
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// The report as text. Pure: everything comes from `findings`.
pub fn render(f: &Findings) -> String {
    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };
    line(format!("vtteams terminal report ({})", f.version));
    line("Paste all of this into a report. Every byte the terminal sent is shown escaped (\\x1b is ESC); nothing from the terminal is printed raw.".into());
    line(String::new());
    line(format!("system: {}", f.system));
    line(format!(
        "standard input and output are terminals: {}",
        yes(f.interactive)
    ));
    line(format!(
        "terminal size: {}",
        f.size.map_or_else(
            || "unknown".to_string(),
            |(w, h)| format!("{w} x {h} cells")
        )
    ));
    line(format!(
        "cell size from the system's pixel size of the window: {}",
        f.system_cell
            .map_or_else(|| "not available".to_string(), CellPx::label)
    ));
    if let Some((w, h)) = f.console_font {
        line(format!(
            "console API font size (Windows; often a fixed value under ConPTY, so not used): {w}x{h}"
        ));
    }
    line(String::new());
    line("environment (name = value):".into());
    for (name, value) in &f.variables {
        line(format!(
            "  {name} = {}",
            value
                .as_deref()
                .map_or_else(|| "(not set)".to_string(), value_text)
        ));
    }
    let hint = detect::hint(&f.env);
    line(format!(
        "the environment names: {}",
        match hint {
            Some(detect::Hint::Kitty) => "a Kitty-graphics terminal",
            Some(detect::Hint::Iterm) => "iTerm2",
            Some(detect::Hint::Sixel) if f.env.windows_terminal => "Windows Terminal (Sixel)",
            Some(detect::Hint::Sixel) => "a Sixel terminal",
            None => "nothing known",
        }
    ));
    line(format!(
        "inside tmux or screen: {}",
        yes(detect::in_multiplexer(&f.env))
    ));
    line(String::new());

    match &f.asked {
        None => {
            line("the start-up query was not run: standard input or output is not a terminal, or the system gives no way to read the answers.".into());
        }
        Some(asked) => {
            let (kitty, budget) = probe::plan(detect::asks_kitty(&f.env), f.remote);
            line(format!(
                "start-up query (what vtteams sends before it draws the first screen; waits at most {} ms{}):",
                budget.as_millis(),
                if f.remote {
                    "; running remotely, so no Kitty question"
                } else {
                    ""
                }
            ));
            line(format!("  sent: {}", escape(&probe::questions(kitty))));
            for (i, name) in question_names(kitty).iter().enumerate() {
                line(format!("    question {}: {name}", i + 1));
            }
            line(format!(
                "  waited {} ms; it ended because {}",
                asked.elapsed.as_millis(),
                if asked.answered {
                    "the device attributes arrived"
                } else {
                    "the wait ran out with no device attributes"
                }
            ));
            line(format!(
                "  received {} bytes: {}",
                asked.raw.len(),
                if asked.raw.is_empty() {
                    "(nothing)".to_string()
                } else {
                    escape(&asked.raw)
                }
            ));
            if !f.events.is_empty() {
                line(format!(
                    "  the console handed over {} key events (what a Windows program is given instead of bytes):",
                    f.events.len()
                ));
                for (i, event) in f.events.iter().take(MAX_EVENTS_SHOWN).enumerate() {
                    line(format!("    {:>3}. {event}", i + 1));
                }
                if f.events.len() > MAX_EVENTS_SHOWN {
                    line(format!(
                        "    (and {} more)",
                        f.events.len() - MAX_EVENTS_SHOWN
                    ));
                }
                line("  (the bytes above were rebuilt from those: an ESC comes as Ctrl+[ or goes missing, and is put back)".into());
            }
            let (replies, accepted) = parse_replies_counted(&asked.raw);
            line(format!(
                "  the strict reader kept {accepted} of {} bytes and dropped {}",
                asked.raw.len(),
                asked.raw.len() - accepted.min(asked.raw.len())
            ));
            describe_replies(&replies, &mut line);
        }
    }
    if !f.late.is_empty() {
        line(String::new());
        line(format!(
            "{} more bytes arrived within {} ms after the wait was over; in the real program these would have been typed into the message box: {}",
            f.late.len(),
            LATE_WAIT.as_millis(),
            escape(&f.late)
        ));
    } else if f.asked.is_some() {
        line(format!(
            "nothing more arrived in the {} ms after the wait was over",
            LATE_WAIT.as_millis()
        ));
    }
    if !f.probes.is_empty() {
        line(String::new());
        line("extra probes (vtteams does not use these; they show what this terminal answers at all, one question at a time):".into());
        for p in &f.probes {
            line(format!(
                "  {}: sent {} -> {}",
                p.name,
                escape(&p.sent),
                if p.got.is_empty() {
                    format!("no answer in {} ms", p.elapsed.as_millis())
                } else {
                    format!(
                        "{} bytes after {} ms: {}",
                        p.got.len(),
                        p.elapsed.as_millis(),
                        escape(&p.got)
                    )
                }
            ));
        }
    }
    line(String::new());
    line("decision for images = auto:".into());
    line(format!(
        "  protocol: {}",
        f.facts.detected.map_or("none (block art)", Protocol::label)
    ));
    line(format!("  because: {}", f.facts.why));
    line(format!(
        "  cell size: {}",
        match (f.facts.cell, f.facts.cell_assumed) {
            (Some(c), true) => format!(
                "{} px, ASSUMED (nothing told vtteams the real size; set cell_pixels to your font's cell size if pictures look the wrong size)",
                c.label()
            ),
            (Some(c), false) => format!("{} px", c.label()),
            (None, _) => "unknown (pictures stay block art; set cell_pixels)".to_string(),
        }
    ));
    out
}

fn describe_replies(replies: &Replies, line: &mut impl FnMut(String)) {
    line(format!(
        "  device attributes: {}",
        replies.attributes.as_ref().map_or_else(
            || "no answer".to_string(),
            |a| format!(
                "{a:?} (Sixel {})",
                if replies.sixel() {
                    "listed"
                } else {
                    "not listed"
                }
            )
        )
    ));
    line(format!(
        "  Kitty graphics answer: {}",
        yes(replies.kitty_ok)
    ));
    line(format!(
        "  cell size answer: {}",
        replies
            .cell
            .map_or_else(|| "none".to_string(), |(w, h)| format!("{w}x{h} px"))
    ));
    line(format!(
        "  text area answer: {}",
        replies
            .text_area
            .map_or_else(|| "none".to_string(), |(w, h)| format!("{w}x{h} px"))
    ));
}

/// Read from `source` until nothing has come for [`PROBE_SETTLE`] after the first byte, or [`PROBE_WAIT`] has gone.
fn listen(source: &mut dyn Source) -> (Vec<u8>, Duration) {
    let started = Instant::now();
    let mut got: Vec<u8> = Vec::new();
    let mut last = None::<Instant>;
    loop {
        let chunk = source.read(Duration::from_millis(30));
        if !chunk.is_empty() {
            got.extend(chunk);
            got.truncate(MAX_REPLY_BYTES);
            last = Some(Instant::now());
        }
        let quiet = last.is_some_and(|at| at.elapsed() >= PROBE_SETTLE);
        if quiet || started.elapsed() >= PROBE_WAIT {
            return (got, started.elapsed());
        }
    }
}

/// The extra questions, one at a time.
const PROBES: [(&str, &[u8]); 8] = [
    ("device attributes (CSI c)", b"\x1b[c"),
    ("secondary attributes (CSI > c)", b"\x1b[>c"),
    ("cell size in pixels (CSI 16 t)", b"\x1b[16t"),
    ("text area in pixels (CSI 14 t)", b"\x1b[14t"),
    ("text area in characters (CSI 18 t)", b"\x1b[18t"),
    ("Sixel geometry (CSI ? 2 ; 1 ; 0 S)", b"\x1b[?2;1;0S"),
    (
        "Sixel colour registers (CSI ? 1 ; 1 ; 0 S)",
        b"\x1b[?1;1;0S",
    ),
    ("synchronized output (CSI ? 2026 $ p)", b"\x1b[?2026$p"),
];

/// Run the report on the real terminal and print it to standard output. The terminal's settings are put back before
/// anything is printed.
pub fn run(env: &TermEnv, variables: Vec<(&'static str, Option<String>)>, version: String) {
    use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
    use std::io::IsTerminal;

    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut findings = Findings {
        version,
        system: if cfg!(windows) {
            "windows"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else {
            "unix"
        },
        interactive,
        size: None,
        variables,
        env: env.clone(),
        remote: crate::config::runs_remotely(),
        system_cell: probe::system_cell(),
        console_font: console_font(),
        asked: None,
        events: Vec::new(),
        late: Vec::new(),
        probes: Vec::new(),
        facts: Facts::NONE,
    };
    if interactive && enable_raw_mode().is_ok() {
        findings.size = size().ok();
        let mut stdout = std::io::stdout();
        let (ask_kitty, budget) = probe::plan(detect::asks_kitty(env), findings.remote);
        let _ = probe::with_source(|source| {
            findings.asked = probe::ask_traced(&mut stdout, &mut *source, ask_kitty, budget).ok();
            findings.events = source.trace();
            let late_until = Instant::now() + LATE_WAIT;
            while Instant::now() < late_until {
                findings.late.extend(source.read(Duration::from_millis(50)));
                findings.late.truncate(MAX_REPLY_BYTES);
            }
            source.trace();
            for (name, question) in PROBES {
                let _ = stdout.write_all(question).and_then(|()| stdout.flush());
                let (got, elapsed) = listen(&mut *source);
                source.trace();
                findings.probes.push(Probe {
                    name,
                    sent: question.to_vec(),
                    got,
                    elapsed,
                });
            }
        });
        let _ = disable_raw_mode();
    }
    let replies = findings
        .asked
        .as_ref()
        .map(|a| a.replies.clone())
        .unwrap_or_default();
    findings.facts = probe::learn(
        ImageChoice::Auto,
        env,
        None,
        findings.system_cell,
        findings.size.unwrap_or((0, 0)),
        |_| Some(replies),
    );
    print!("{}", render(&findings));
}

#[cfg(windows)]
fn console_font() -> Option<(i32, i32)> {
    let size = crossterm_winapi::ScreenBuffer::current()
        .ok()?
        .font_info()
        .ok()?
        .size();
    Some((i32::from(size.width), i32::from(size.height)))
}

#[cfg(not(windows))]
fn console_font() -> Option<(i32, i32)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> TermEnv {
        TermEnv::from_lookup(|name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        })
    }

    fn findings(pairs: &[(&'static str, &str)], raw: &[u8]) -> Findings {
        let environment = env(pairs);
        let replies = detect::parse_replies(raw);
        Findings {
            version: "vtteams test".into(),
            system: "windows",
            interactive: true,
            size: Some((120, 30)),
            variables: VARIABLES
                .iter()
                .map(|n| {
                    (
                        *n,
                        pairs
                            .iter()
                            .find(|(k, _)| k == n)
                            .map(|(_, v)| (*v).to_string()),
                    )
                })
                .collect(),
            env: environment.clone(),
            remote: false,
            system_cell: None,
            console_font: Some((8, 16)),
            asked: Some(Asked {
                replies: replies.clone(),
                raw: raw.to_vec(),
                elapsed: Duration::from_millis(300),
                answered: replies.attributes.is_some(),
            }),
            events: Vec::new(),
            late: Vec::new(),
            probes: Vec::new(),
            facts: probe::learn(
                ImageChoice::Auto,
                &environment,
                None,
                None,
                (120, 30),
                |_| Some(replies),
            ),
        }
    }

    #[test]
    fn escaping_leaves_no_control_character_and_no_byte_above_ascii() {
        assert_eq!(escape(b"\x1b[?62;4c"), "\\x1b[?62;4c");
        assert_eq!(escape(b"a\\b"), "a\\\\b");
        assert_eq!(
            escape(&[0x07, 0x00, 0x7f, 0x80, 0xff, b'\n', b'\r']),
            "\\x07\\x00\\x7f\\x80\\xff\\x0a\\x0d"
        );
        // Every byte value comes out as plain printable ASCII.
        let all: Vec<u8> = (0..=255).collect();
        let text = escape(&all);
        assert!(text.chars().all(|c| (' '..='~').contains(&c)), "{text}");
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn a_value_from_the_environment_is_cut_and_escaped() {
        let long = "x".repeat(200);
        let shown = value_text(&long);
        assert_eq!(shown.len(), MAX_VALUE + 3);
        assert!(shown.ends_with("..."));
        assert_eq!(value_text("a\x1b]0;evil\x07b"), "a\\x1b]0;evil\\x07b");
    }

    #[test]
    fn a_report_of_a_silent_windows_terminal_says_what_was_sent_and_what_came_back() {
        let text = render(&findings(
            &[("WT_SESSION", "abc-123"), ("TERM", "xterm-256color")],
            b"",
        ));
        for expected in [
            "environment (name = value):",
            "  WT_SESSION = abc-123",
            "  TERM_PROGRAM = (not set)",
            "the environment names: Windows Terminal (Sixel)",
            "sent: \\x1b[16t\\x1b[14t\\x1b[c",
            "waited 300 ms; it ended because the wait ran out with no device attributes",
            "received 0 bytes: (nothing)",
            "device attributes: no answer",
            "protocol: sixel",
            "the environment names the terminal; it did not answer",
            "ASSUMED",
            "9x18",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
    }

    #[test]
    fn a_report_of_an_answering_terminal_lists_what_it_said_and_what_was_kept() {
        let raw = b"\x1b[6;20;10t\x1b[?61;4;6c";
        let text = render(&findings(&[("WT_SESSION", "x")], raw));
        for expected in [
            "received 20 bytes: \\x1b[6;20;10t\\x1b[?61;4;6c",
            "ended because the device attributes arrived",
            "the strict reader kept 20 of 20 bytes and dropped 0",
            "device attributes: [61, 4, 6] (Sixel listed)",
            "cell size answer: 10x20 px",
            "cell size: 10x20 px\n",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        assert!(!text.contains("ASSUMED"));
    }

    #[test]
    fn what_the_terminal_sent_is_never_printed_raw_whatever_it_is() {
        let mut f = findings(
            &[("TERM", "foot")],
            b"\x1b]0;pwned\x07\x1b[2J\x1b[?62;4c\x9b31m",
        );
        f.late = b"\x1b[31mred\x1b[0m\r\n".to_vec();
        f.probes.push(Probe {
            name: "device attributes (CSI c)",
            sent: b"\x1b[c".to_vec(),
            got: b"\x1b[?62c\x1b]52;c;?\x07".to_vec(),
            elapsed: Duration::from_millis(3),
        });
        f.events = vec!["Char U+005B '[' +ctrl press".into()];
        let text = render(&f);
        assert!(
            !text
                .bytes()
                .any(|b| b == 0x1b || b == 0x07 || b == 0x9b || b == b'\r'),
            "a control byte reached the output"
        );
        assert!(text.contains("\\x1b]0;pwned\\x07"));
        assert!(text.contains("more bytes arrived"));
        assert!(text.contains("1 key events") || text.contains("handed over 1 key events"));
        // Only newlines are control characters in the report.
        assert!(
            text.chars().all(|c| c == '\n' || !c.is_control()),
            "{text:?}"
        );
    }

    #[test]
    fn a_report_where_nothing_can_be_asked_says_so() {
        let mut f = findings(&[], b"");
        f.asked = None;
        f.interactive = false;
        let text = render(&f);
        assert!(text.contains("standard input and output are terminals: no"));
        assert!(text.contains("the start-up query was not run"));
    }

    #[test]
    fn the_list_of_key_events_is_capped() {
        let mut f = findings(&[], b"");
        f.events = (0..500)
            .map(|i| format!("Char U+0041 'A' press {i}"))
            .collect();
        let text = render(&f);
        assert_eq!(text.matches("Char U+0041").count(), MAX_EVENTS_SHOWN);
        assert!(text.contains("(and 440 more)"));
    }

    #[test]
    fn the_extra_probes_are_listed_with_their_answers_or_the_silence() {
        let mut f = findings(&[], b"");
        f.probes.push(Probe {
            name: "cell size in pixels (CSI 16 t)",
            sent: b"\x1b[16t".to_vec(),
            got: Vec::new(),
            elapsed: Duration::from_millis(300),
        });
        f.probes.push(Probe {
            name: "device attributes (CSI c)",
            sent: b"\x1b[c".to_vec(),
            got: b"\x1b[?61;4c".to_vec(),
            elapsed: Duration::from_millis(5),
        });
        let text = render(&f);
        assert!(
            text.contains("cell size in pixels (CSI 16 t): sent \\x1b[16t -> no answer in 300 ms")
        );
        assert!(text.contains(
            "device attributes (CSI c): sent \\x1b[c -> 8 bytes after 5 ms: \\x1b[?61;4c"
        ));
    }
}
