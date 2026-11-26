//! Which graphics protocol the terminal has: the hints in the environment, the strict reader for what the terminal
//! answers when asked, and the rule that combines them.
//!
//! A terminal's reply arrives on stdin, so whatever prints to the terminal can forge one: it is untrusted input, like
//! the clipboard's and the title's (those are never read at all, for that reason). Here a reply is read only before the
//! event loop starts, by [`parse_replies`], which accepts exactly four numeric forms and drops every other byte without
//! echoing, acting on or logging it:
//!
//! | what was asked                | reply                              | read as                          |
//! |-------------------------------|------------------------------------|----------------------------------|
//! | primary device attributes     | `ESC [ ? Ps ; Ps ... c`            | the list (4 means Sixel)         |
//! | the cell size in pixels       | `ESC [ 6 ; height ; width t`       | the cell size                    |
//! | the text area size in pixels  | `ESC [ 4 ; height ; width t`       | the cell size, with the columns  |
//! | the Kitty graphics query      | `ESC _ G i=31;OK ESC \`            | Kitty graphics are available     |
//!
//! Numbers are plain decimal with no sign and no leading zero. Nothing the terminal says is ever text that is shown or
//! used, only a number, so there is nothing a forged reply could make us do beyond choosing a protocol the person could
//! have chosen themselves.

use super::{CellPx, Protocol};

/// The environment variables that say what the terminal is, read by `config.rs` (the one place that reads the
/// environment) and handed in. Text is lower-cased.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TermEnv {
    pub term: String,
    pub term_program: String,
    pub kitty_window: bool,
    pub windows_terminal: bool,
    pub iterm_session: bool,
    pub tmux: bool,
    pub screen: bool,
}

impl TermEnv {
    /// Read the relevant variables through `get`. A variable that is set but empty counts as unset.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let set = |name: &str| get(name).is_some_and(|v| !v.trim().is_empty());
        let text = |name: &str| {
            get(name)
                .map(|v| v.trim().to_ascii_lowercase())
                .unwrap_or_default()
        };
        Self {
            term: text("TERM"),
            term_program: text("TERM_PROGRAM"),
            kitty_window: set("KITTY_WINDOW_ID"),
            windows_terminal: set("WT_SESSION"),
            iterm_session: set("ITERM_SESSION_ID"),
            tmux: set("TMUX"),
            screen: set("STY"),
        }
    }
}

/// What the environment suggests, before anything is asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hint {
    Kitty,
    Iterm,
    Sixel,
}

impl Hint {
    fn protocol(self) -> Protocol {
        match self {
            Self::Kitty => Protocol::Kitty,
            Self::Iterm => Protocol::Iterm,
            Self::Sixel => Protocol::Sixel,
        }
    }
}

/// Inside tmux or screen: what is drawn there does not reach the terminal unless it is configured to pass it
/// through, so graphics are off unless the person chooses a protocol.
pub fn in_multiplexer(env: &TermEnv) -> bool {
    env.tmux
        || env.screen
        || env.term.starts_with("screen")
        || env.term.starts_with("tmux")
        || env.term_program == "tmux"
}

/// The protocol the environment points to, if any.
pub fn hint(env: &TermEnv) -> Option<Hint> {
    let kitty = env.kitty_window
        || env.term == "xterm-kitty"
        || env.term == "xterm-ghostty"
        || env.term_program == "ghostty"
        || env.term_program == "wezterm"
        || env.term == "wezterm";
    if kitty {
        Some(Hint::Kitty)
    } else if env.term_program == "iterm.app" || env.iterm_session {
        Some(Hint::Iterm)
    } else if env.windows_terminal || env.term.starts_with("foot") || env.term == "mlterm" {
        Some(Hint::Sixel)
    } else {
        None
    }
}

/// Whether the Kitty graphics query is worth sending. Only where the environment says the terminal is one that
/// speaks it: the answer to that query is an APC string, which a key reader that does not know it would take for typing,
/// so it is not sent to terminals that have no reason to understand it.
pub fn asks_kitty(env: &TermEnv) -> bool {
    !in_multiplexer(env) && hint(env) == Some(Hint::Kitty)
}

/// What the terminal answered. Every field is only ever a number or a yes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Replies {
    /// The primary device attributes, when the terminal answered that question (every terminal does, so this is also
    /// the sign that the others have been answered, or never will be).
    pub attributes: Option<Vec<u16>>,
    /// The Kitty graphics query was answered "OK".
    pub kitty_ok: bool,
    /// The cell size in pixels as (width, height), as the terminal said it.
    pub cell: Option<(u32, u32)>,
    /// The text area in pixels as (width, height).
    pub text_area: Option<(u32, u32)>,
}

impl Replies {
    /// The terminal said it draws Sixel.
    pub fn sixel(&self) -> bool {
        self.attributes
            .as_ref()
            .is_some_and(|attributes| attributes.contains(&4))
    }
}

/// Never look at more than this many bytes of what the terminal sent.
pub const MAX_REPLY_BYTES: usize = 4096;

const ESC: u8 = 0x1b;
/// The longest parameter list a reply may have.
const MAX_PARAMS: usize = 16;
/// The id the Kitty query carries, and so the id its answer names.
pub const KITTY_QUERY_ID: u32 = 31;

enum Reply {
    Attributes(Vec<u16>),
    KittyOk,
    Cell(u32, u32),
    TextArea(u32, u32),
}

/// Read the replies in what arrived. Bytes that are not part of one of the exact forms in the module's table are skipped
/// one at a time, so junk before, between or after replies changes nothing, and nothing is ever an error. Where a reply
/// is given twice the first one counts.
pub fn parse_replies(bytes: &[u8]) -> Replies {
    parse_replies_counted(bytes).0
}

/// [`parse_replies`], also saying how many bytes were part of the replies it accepted (the rest were dropped).
pub fn parse_replies_counted(bytes: &[u8]) -> (Replies, usize) {
    let bytes = &bytes[..bytes.len().min(MAX_REPLY_BYTES)];
    let mut replies = Replies::default();
    let mut accepted = 0;
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != ESC {
            at += 1;
            continue;
        }
        let Some((reply, len)) = reply_at(&bytes[at..]) else {
            at += 1;
            continue;
        };
        at += len;
        accepted += len;
        match reply {
            Reply::Attributes(list) => {
                replies.attributes.get_or_insert(list);
            }
            Reply::KittyOk => replies.kitty_ok = true,
            Reply::Cell(h, w) => {
                replies.cell.get_or_insert((w, h));
            }
            Reply::TextArea(h, w) => {
                replies.text_area.get_or_insert((w, h));
            }
        }
    }
    (replies, accepted)
}

/// A reply starting at `b[0]` (an ESC), and how many bytes it takes.
fn reply_at(b: &[u8]) -> Option<(Reply, usize)> {
    match (b.get(1)?, b.get(2)?) {
        (b'[', b'?') => {
            let (list, end) = params(b, 3)?;
            if b.get(end) != Some(&b'c') {
                return None;
            }
            let attributes = list
                .into_iter()
                .map(|n| u16::try_from(n).ok())
                .collect::<Option<Vec<u16>>>()?;
            Some((Reply::Attributes(attributes), end + 1))
        }
        (b'[', b'4' | b'6') => {
            let (list, end) = params(b, 2)?;
            if b.get(end) != Some(&b't') {
                return None;
            }
            match list[..] {
                [6, h, w] => Some((Reply::Cell(h, w), end + 1)),
                [4, h, w] => Some((Reply::TextArea(h, w), end + 1)),
                _ => None,
            }
        }
        (b'_', b'G') => {
            // The whole answer is `i=31;OK`; anything else, an error answer included, is not an OK.
            let body = &b[3..];
            let stop = body.iter().take(64).position(|c| *c == ESC)?;
            let expected = format!("i={KITTY_QUERY_ID};OK");
            (body.get(stop + 1) == Some(&b'\\') && &body[..stop] == expected.as_bytes())
                .then_some((Reply::KittyOk, 3 + stop + 2))
        }
        _ => None,
    }
}

/// Decimal numbers separated by `;`, starting at `b[from]`: each one to six digits with no leading zero (a lone 0 is
/// fine), at most [`MAX_PARAMS`] of them. Gives the numbers and the index just after the last digit; an empty parameter, a
/// sign, a letter or anything else in it is not a list.
fn params(b: &[u8], from: usize) -> Option<(Vec<u32>, usize)> {
    let mut list = Vec::new();
    let mut at = from;
    loop {
        let digits = b[at..]
            .iter()
            .take(7)
            .take_while(|c| c.is_ascii_digit())
            .count();
        if digits == 0 || digits > 6 || (digits > 1 && b[at] == b'0') {
            return None;
        }
        let value = b[at..at + digits]
            .iter()
            .fold(0u32, |n, d| n * 10 + u32::from(d - b'0'));
        list.push(value);
        at += digits;
        if list.len() > MAX_PARAMS {
            return None;
        }
        if b.get(at) == Some(&b';') {
            at += 1;
        } else {
            return Some((list, at));
        }
    }
}

/// The cell size in pixels, from what the terminal said: its own report of the cell, else its text area divided by the
/// number of columns and rows.
pub fn cell_size(replies: &Replies, columns: u16, rows: u16) -> Option<CellPx> {
    if let Some((w, h)) = replies.cell
        && let Some(cell) = CellPx::new(w, h)
    {
        return Some(cell);
    }
    let (w, h) = replies.text_area?;
    if columns == 0 || rows == 0 {
        return None;
    }
    CellPx::new(w / u32::from(columns), h / u32::from(rows))
}

/// What detection decided, and why in a few fixed words (for the log).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Detection {
    pub protocol: Option<Protocol>,
    pub why: &'static str,
}

/// Choose a protocol from the environment and from what the terminal answered (`replies` is empty when it was not asked
/// or did not answer).
///
/// - Inside tmux or screen: none.
/// - A terminal that answered the device-attributes question is believed over the environment: Kitty graphics if it
///   answered that query, iTerm2's protocol where the environment says iTerm2, Sixel if it lists attribute 4, none
///   otherwise (an older Windows Terminal answers and lacks 4).
/// - A terminal that did not answer anything is taken at the environment's word, when that names a protocol.
pub fn decide(env: &TermEnv, replies: &Replies) -> Detection {
    let said = |protocol, why| Detection {
        protocol: Some(protocol),
        why,
    };
    let none = |why| Detection {
        protocol: None,
        why,
    };
    if in_multiplexer(env) {
        return none("inside tmux or screen");
    }
    let hint = hint(env);
    if replies.attributes.is_none() {
        return match hint {
            Some(h) => said(
                h.protocol(),
                "the environment names the terminal; it did not answer",
            ),
            None => none("nothing in the environment, and the terminal did not answer"),
        };
    }
    if replies.kitty_ok {
        return said(
            Protocol::Kitty,
            "the terminal answered the Kitty graphics query",
        );
    }
    if hint == Some(Hint::Iterm) {
        return said(Protocol::Iterm, "the environment names iTerm2");
    }
    if replies.sixel() {
        return said(
            Protocol::Sixel,
            "the terminal lists Sixel in its attributes",
        );
    }
    none("the terminal answered and does not list a graphics protocol")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn env(pairs: &[(&str, &str)]) -> TermEnv {
        TermEnv::from_lookup(|name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        })
    }

    fn silent() -> Replies {
        Replies::default()
    }

    fn da1(list: &[u16]) -> Replies {
        Replies {
            attributes: Some(list.to_vec()),
            ..Replies::default()
        }
    }

    fn protocol(pairs: &[(&str, &str)], replies: &Replies) -> Option<Protocol> {
        decide(&env(pairs), replies).protocol
    }

    #[test]
    fn the_environment_names_the_terminals_that_are_known() {
        for (vars, expected) in [
            (vec![("TERM", "xterm-kitty")], Some(Hint::Kitty)),
            (vec![("KITTY_WINDOW_ID", "3")], Some(Hint::Kitty)),
            (vec![("TERM_PROGRAM", "WezTerm")], Some(Hint::Kitty)),
            (vec![("TERM_PROGRAM", "ghostty")], Some(Hint::Kitty)),
            (vec![("TERM", "xterm-ghostty")], Some(Hint::Kitty)),
            (vec![("TERM_PROGRAM", "iTerm.app")], Some(Hint::Iterm)),
            (vec![("ITERM_SESSION_ID", "w0t0p0")], Some(Hint::Iterm)),
            (vec![("WT_SESSION", "a-guid")], Some(Hint::Sixel)),
            (vec![("TERM", "foot")], Some(Hint::Sixel)),
            (vec![("TERM", "foot-extra")], Some(Hint::Sixel)),
            (vec![("TERM", "mlterm")], Some(Hint::Sixel)),
            (vec![("TERM", "xterm-256color")], None),
            (vec![("TERM_PROGRAM", "vscode")], None),
            (vec![("TERM_PROGRAM", "Apple_Terminal")], None),
            (vec![("TERM", "alacritty")], None),
            (vec![], None),
            (vec![("WT_SESSION", "")], None),
            (vec![("KITTY_WINDOW_ID", "  ")], None),
        ] {
            assert_eq!(hint(&env(&vars)), expected, "{vars:?}");
        }
    }

    #[test]
    fn with_no_answer_from_the_terminal_the_environment_decides() {
        for (vars, expected) in [
            (vec![("TERM", "xterm-kitty")], Some(Protocol::Kitty)),
            (vec![("TERM_PROGRAM", "WezTerm")], Some(Protocol::Kitty)),
            (vec![("TERM_PROGRAM", "iTerm.app")], Some(Protocol::Iterm)),
            (vec![("WT_SESSION", "x")], Some(Protocol::Sixel)),
            (vec![("TERM", "foot")], Some(Protocol::Sixel)),
            (vec![("TERM", "xterm-256color")], None),
            (vec![], None),
        ] {
            assert_eq!(protocol(&vars, &silent()), expected, "{vars:?}");
        }
    }

    #[test]
    fn an_answering_terminal_is_believed_over_the_environment() {
        // Windows Terminal that answers without attribute 4: an old one, no graphics.
        assert_eq!(protocol(&[("WT_SESSION", "x")], &da1(&[61, 6, 22])), None);
        assert_eq!(
            protocol(&[("WT_SESSION", "x")], &da1(&[61, 4])),
            Some(Protocol::Sixel)
        );
        // No hint at all, but the terminal says Sixel: xterm, foot, mlterm, anything.
        assert_eq!(
            protocol(&[("TERM", "xterm-256color")], &da1(&[64, 1, 2, 4, 6])),
            Some(Protocol::Sixel)
        );
        assert_eq!(protocol(&[("TERM", "xterm-256color")], &da1(&[1, 2])), None);
        // A terminal that claims to be kitty but does not answer the graphics query falls back to what it lists.
        assert_eq!(
            protocol(&[("TERM", "xterm-kitty")], &da1(&[62, 4])),
            Some(Protocol::Sixel)
        );
        assert_eq!(protocol(&[("TERM", "xterm-kitty")], &da1(&[62])), None);
        let kitty = Replies {
            kitty_ok: true,
            ..da1(&[62])
        };
        assert_eq!(
            protocol(&[("TERM", "xterm-kitty")], &kitty),
            Some(Protocol::Kitty)
        );
        // iTerm2 keeps its own protocol even though it also lists Sixel.
        assert_eq!(
            protocol(&[("TERM_PROGRAM", "iTerm.app")], &da1(&[62, 4])),
            Some(Protocol::Iterm)
        );
    }

    #[test]
    fn inside_tmux_or_screen_there_are_no_graphics() {
        for vars in [
            vec![
                ("TMUX", "/tmp/tmux-1000/default,1,0"),
                ("TERM", "xterm-kitty"),
            ],
            vec![("STY", "1234.pts-0.host"), ("TERM", "xterm-256color")],
            vec![("TERM", "screen-256color")],
            vec![("TERM", "tmux-256color"), ("WT_SESSION", "x")],
            vec![("TERM_PROGRAM", "tmux")],
        ] {
            assert!(in_multiplexer(&env(&vars)), "{vars:?}");
            assert_eq!(protocol(&vars, &silent()), None, "{vars:?}");
            assert_eq!(protocol(&vars, &da1(&[62, 4])), None, "{vars:?}");
            assert!(
                !asks_kitty(&env(&vars)),
                "no query goes through a multiplexer"
            );
        }
        assert!(!in_multiplexer(&env(&[("TERM", "xterm-256color")])));
    }

    #[test]
    fn the_kitty_query_is_sent_only_to_terminals_that_say_they_speak_it() {
        assert!(asks_kitty(&env(&[("TERM", "xterm-kitty")])));
        assert!(asks_kitty(&env(&[("TERM_PROGRAM", "ghostty")])));
        assert!(!asks_kitty(&env(&[("WT_SESSION", "x")])));
        assert!(!asks_kitty(&env(&[("TERM", "xterm-256color")])));
        assert!(!asks_kitty(&env(&[("TERM_PROGRAM", "iTerm.app")])));
    }

    #[test]
    fn real_replies_are_read() {
        let r = parse_replies(b"\x1b[6;20;10t\x1b[4;800;1000t\x1b_Gi=31;OK\x1b\\\x1b[?62;4;22c");
        assert_eq!(
            r.cell,
            Some((10, 20)),
            "width, height from `6 ; height ; width`"
        );
        assert_eq!(r.text_area, Some((1000, 800)));
        assert!(r.kitty_ok);
        assert_eq!(r.attributes, Some(vec![62, 4, 22]));
        assert!(r.sixel());
        assert_eq!(parse_replies(b"\x1b[?1;0c").attributes, Some(vec![1, 0]));
        assert!(!parse_replies(b"\x1b[?62;22c").sixel());
        assert_eq!(parse_replies(b""), Replies::default());
    }

    #[test]
    fn what_is_not_an_exact_numeric_reply_is_dropped() {
        for junk in [
            &b"\x1b[?62;4"[..],                   // truncated: no final byte
            b"\x1b[?62;4x",                       // wrong final byte
            b"\x1b[?62;;4c",                      // empty parameter
            b"\x1b[?c",                           // no parameters
            b"\x1b[?6a;4c",                       // a letter inside
            b"\x1b[?-6;4c",                       // a sign
            b"\x1b[?062;4c",                      // a leading zero
            b"\x1b[?1234567;4c",                  // seven digits
            b"\x1b[?99999;4c",                    // over what an attribute can be
            b"\x1b[6;20t",                        // too few numbers
            b"\x1b[6;20;10;5t",                   // too many
            b"\x1b[6;20;1x0t",                    // not a number
            b"\x1b[7;20;10t",                     // not a size report
            b"\x1b_Gi=31;ENOTSUPPORTED:no\x1b\\", // the kitty error answer
            b"\x1b_Gi=31;OK",                     // no terminator
            b"\x1b_Gi=31;OK\x07",                 // wrong terminator
            b"\x1b_Gi=32;OK\x1b\\",               // somebody else's id
            b"\x1b_Gi=31;OK!\x1b\\",              // text after OK
            b"\x1b_Ga=q;OK\x1b\\",
            b"\x1b]0;i=31;OK\x1b\\",
            b"\x1b[?1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17c", // more than 16 parameters
        ] {
            assert_eq!(parse_replies(junk), Replies::default(), "{junk:?}");
        }
    }

    #[test]
    fn a_reply_is_found_among_junk_and_the_first_one_counts() {
        let r = parse_replies(b"hello \x1b[?7 \x1b[?62;4c tail \x1b[?9c");
        assert_eq!(r.attributes, Some(vec![62, 4]));
        // An opener that never closes does not swallow the reply that follows it.
        let r = parse_replies(b"\x1b_Gi=31;\x1b[?62;4c");
        assert_eq!(r.attributes, Some(vec![62, 4]));
        assert!(!r.kitty_ok);
        let r = parse_replies(b"\x1b[6;20;10t\x1b[6;30;12t");
        assert_eq!(r.cell, Some((10, 20)));
    }

    #[test]
    fn only_the_first_few_kilobytes_are_looked_at() {
        let mut bytes = vec![b'x'; MAX_REPLY_BYTES];
        bytes.extend_from_slice(b"\x1b[?62;4c");
        assert_eq!(parse_replies(&bytes), Replies::default());
        let mut bytes = b"\x1b[?62;4c".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', MAX_REPLY_BYTES * 4));
        assert!(parse_replies(&bytes).sixel());
    }

    #[test]
    fn the_cell_size_comes_from_the_cell_report_then_the_text_area() {
        let cell = |w, h| Replies {
            cell: Some((w, h)),
            ..Replies::default()
        };
        assert_eq!(cell_size(&cell(10, 20), 100, 40), CellPx::new(10, 20));
        let area = Replies {
            text_area: Some((1000, 800)),
            ..Replies::default()
        };
        assert_eq!(cell_size(&area, 100, 40), CellPx::new(10, 20));
        assert_eq!(cell_size(&area, 0, 40), None, "no columns to divide by");
        assert_eq!(
            cell_size(
                &Replies {
                    cell: Some((0, 0)),
                    ..area.clone()
                },
                100,
                40
            ),
            CellPx::new(10, 20),
            "an absurd cell report falls through to the area"
        );
        assert_eq!(
            cell_size(&cell(1, 1), 100, 40),
            None,
            "and so does a size no cell has"
        );
        assert_eq!(cell_size(&Replies::default(), 100, 40), None);
    }

    /// Replies spelt the way a terminal spells them, for the properties below.
    fn canonical(replies: &Replies) -> Vec<Vec<u8>> {
        let mut forms = Vec::new();
        if let Some(list) = &replies.attributes {
            let list: Vec<String> = list.iter().map(u16::to_string).collect();
            forms.push(format!("\x1b[?{}c", list.join(";")).into_bytes());
        }
        if replies.kitty_ok {
            forms.push(b"\x1b_Gi=31;OK\x1b\\".to_vec());
        }
        if let Some((w, h)) = replies.cell {
            forms.push(format!("\x1b[6;{h};{w}t").into_bytes());
        }
        if let Some((w, h)) = replies.text_area {
            forms.push(format!("\x1b[4;{h};{w}t").into_bytes());
        }
        forms
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// Found by a property test: a device-attributes reply that never reaches its final `c` (`ESC [ ? 0` and then a
    /// stray `[`) is not an answer at all, so nothing is taken from it.
    #[test]
    fn a_device_attributes_reply_that_is_cut_short_is_dropped() {
        for input in [
            &b"\x1b[?0["[..],
            b"\x1b[?0",
            b"\x1b[?62;4[c",
            b"\x1b[?0\x1b[?",
        ] {
            assert_eq!(
                parse_replies(input),
                Replies::default(),
                "{:?}",
                String::from_utf8_lossy(input)
            );
        }
        // The same reply finished properly is still one.
        assert_eq!(parse_replies(b"\x1b[?0c").attributes, Some(vec![0]));
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic_and_anything_accepted_is_in_its_exact_form(
            bytes in proptest::collection::vec(any::<u8>(), 0..600)
        ) {
            let replies = parse_replies(&bytes);
            for form in canonical(&replies) {
                prop_assert!(contains(&bytes, &form), "accepted {form:?} that is not in the input");
            }
        }

        /// Bytes that are escape-sequence-shaped but wrong, in quantity: nothing is accepted that is not exact.
        #[test]
        fn mangled_replies_are_either_exact_or_dropped(
            body in "[0-9;?a-z\\[\\]_G= \\\\]{0,40}",
            lead in prop::sample::select(vec!["\x1b[", "\x1b[?", "\x1b_G", "\x1b[6;", "\x1b[4;"]),
        ) {
            let input = format!("{lead}{body}").into_bytes();
            let replies = parse_replies(&input);
            for form in canonical(&replies) {
                prop_assert!(contains(&input, &form), "accepted {form:?} from {input:?}");
            }
        }

        #[test]
        fn junk_around_a_reply_changes_nothing(
            before in proptest::collection::vec(any::<u8>().prop_filter("no escape", |b| *b != ESC), 0..100),
            after in proptest::collection::vec(any::<u8>().prop_filter("no escape", |b| *b != ESC), 0..100),
        ) {
            let reply = b"\x1b[6;20;10t\x1b[?62;4c\x1b_Gi=31;OK\x1b\\";
            let mut input = before;
            input.extend_from_slice(reply);
            input.extend_from_slice(&after);
            prop_assert_eq!(parse_replies(&input), parse_replies(reply));
        }
    }
}
