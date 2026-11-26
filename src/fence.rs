//! Recognising a fenced-code-block delimiter line (CommonMark: a run of at least three `` ` `` or `~` characters,
//! optionally with an info string after it), the one piece of Markdown that message rendering, outgoing-message
//! cleanup, mention scanning and the editor's own highlighting all need to agree on. Used to be five separate,
//! slightly different implementations (naive `starts_with("```")` checks that missed `~~~`, did not require the
//! closing marker to match the kind that opened it, and mistook a line like `` ```lang``` text `` — which both opens
//! and closes a fence on itself — for an opener that swallows the rest of the message as code).

/// Which fence character a delimiter line uses. CommonMark fences do not mix: a block opened with `` ` `` only closes
/// with `` ` ``, and one opened with `~` only closes with `~`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FenceKind {
    Backtick,
    Tilde,
}

impl FenceKind {
    fn marker(self) -> char {
        match self {
            Self::Backtick => '`',
            Self::Tilde => '~',
        }
    }
}

/// An open fence: what a caller tracking one needs to know to recognise its end. CommonMark closes a fence only
/// with a run of the same character at least as long as the one that opened it, which is how a block of code that
/// itself contains a ```` ``` ```` line is written (htmd, converting Teams' HTML, fences it with four or more).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fence {
    pub kind: FenceKind,
    /// How many marker characters the run has.
    pub len: usize,
}

impl Fence {
    /// A line that closes this fence.
    pub fn closer(self) -> String {
        self.kind.marker().to_string().repeat(self.len)
    }
}

/// Whether `line` is a fence delimiter, and if so the fence it opens (or closes) and the text after the marker run
/// (the info string, such as a language, on an opening line; empty on a closing one). `None` for anything else,
/// including a line that both opens and closes a fence on itself (`` ```lang``` text ``): CommonMark forbids the
/// marker character inside a backtick fence's info string, and a parser that treated this as an opener would read
/// everything after it as code until it happened to see another standalone fence line.
pub fn fence_marker(line: &str) -> Option<(Fence, &str)> {
    let t = line.trim_start();
    let kind = if t.starts_with("```") {
        FenceKind::Backtick
    } else if t.starts_with("~~~") {
        FenceKind::Tilde
    } else {
        return None;
    };
    let info = t.trim_start_matches(kind.marker());
    if info.contains(kind.marker()) {
        return None;
    }
    let len = t.chars().take_while(|&c| c == kind.marker()).count();
    Some((Fence { kind, len }, info.trim()))
}

/// Whether `line` closes the `open` fence: a run of the same character at least as long as the opening one, and
/// nothing after it. A shorter run, or one with an info string (```` ```rust ````), is code inside the block, not its
/// end.
pub fn fence_closes(line: &str, open: Fence) -> bool {
    fence_marker(line).is_some_and(|(found, info)| {
        found.kind == open.kind && found.len >= open.len && info.is_empty()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_not_a_fence() {
        assert_eq!(fence_marker("hello"), None);
        assert_eq!(
            fence_marker("  ``code``  "),
            None,
            "two backticks, not a fence"
        );
    }

    const TICKS: Fence = Fence {
        kind: FenceKind::Backtick,
        len: 3,
    };
    const TILDES: Fence = Fence {
        kind: FenceKind::Tilde,
        len: 3,
    };

    #[test]
    fn a_fence_line_gives_its_kind_length_and_info_string() {
        assert_eq!(fence_marker("```rust"), Some((TICKS, "rust")));
        assert_eq!(fence_marker("  ```  "), Some((TICKS, "")));
        assert_eq!(fence_marker("~~~python"), Some((TILDES, "python")));
        assert_eq!(
            fence_marker("````"),
            Some((
                Fence {
                    kind: FenceKind::Backtick,
                    len: 4
                },
                ""
            )),
            "a longer run is still a fence, and remembers its length"
        );
    }

    #[test]
    fn a_line_that_opens_and_closes_a_fence_on_itself_is_not_a_fence_marker() {
        // CommonMark forbids a backtick inside a backtick fence's info string; treating this as an opener would make
        // the rest of the message look like code.
        assert_eq!(fence_marker("```inline``` text"), None);
        assert_eq!(fence_marker("```x```"), None);
        // A tilde fence has no such restriction in CommonMark, but this crate keeps the same rule for both kinds,
        // for one consistent and simple test across callers.
        assert_eq!(fence_marker("~~~x~~~"), None);
    }

    #[test]
    fn a_fence_only_closes_one_opened_with_the_same_kind() {
        assert!(fence_closes("```", TICKS));
        assert!(
            !fence_closes("~~~", TICKS),
            "a tilde line does not close a backtick fence"
        );
        assert!(fence_closes("~~~", TILDES));
        assert!(!fence_closes("```", TILDES));
        assert!(
            !fence_closes("```x```", TICKS),
            "self-closing on one line is not a close either"
        );
    }

    #[test]
    fn a_fence_closes_only_with_a_run_at_least_as_long_and_nothing_after_it() {
        let four = fence_marker("````").unwrap().0;
        assert!(
            !fence_closes("```", four),
            "three backticks are code inside a four-backtick block"
        );
        assert!(fence_closes("````", four));
        assert!(fence_closes("`````", four), "a longer run closes too");
        assert!(
            fence_closes("````", TICKS),
            "a longer run closes a three-backtick fence"
        );
        assert!(
            !fence_closes("```rust", TICKS),
            "a line with a language is the start of another block, not the end of this one"
        );
        assert!(
            fence_closes("  ```  ", TICKS),
            "indentation and trailing spaces are fine"
        );
    }
}
