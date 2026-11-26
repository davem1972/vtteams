//! The terminal tab / window title while vtteams runs.
//!
//! The previous title is saved on the terminal's own title stack (xterm's `CSI 22 ; 0 t`) and brought back with
//! `CSI 23 ; 0 t`, so nothing is ever read from the terminal: the reply to a title query (`CSI 21 t`) arrives on stdin and
//! carries text chosen by whatever set the title, which is how terminals have been made to type commands. A terminal
//! without a title stack ignores both sequences and keeps the new title until the shell next sets its own.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// Push the current title and icon name, then set both to the header's keyboard glyph and the name.
const SET: &str = "\x1b[22;0t\x1b]0;\u{2328} vtteams\x07";
/// Pop what [`SET`] pushed.
const RESTORE: &str = "\x1b[23;0t";

/// Whether [`set`] has run and [`restore`] has not, so that restoring twice (the panic hook, then the normal exit) or
/// without having set anything never pops a title that is not ours.
static SET_DONE: AtomicBool = AtomicBool::new(false);

/// Set the title, remembering the old one. Failures to write are ignored: a title is never worth stopping for.
pub fn set() {
    set_on(&mut std::io::stdout());
}

/// Put the title back. Harmless when [`set`] was not called or the terminal has no title stack.
pub fn restore() {
    restore_on(&mut std::io::stdout());
}

fn set_on(out: &mut impl Write) {
    SET_DONE.store(true, Ordering::Relaxed);
    emit(out, SET);
}

fn restore_on(out: &mut impl Write) {
    if SET_DONE.swap(false, Ordering::Relaxed) {
        emit(out, RESTORE);
    }
}

fn emit(out: &mut impl Write, sequence: &str) {
    let _ = out.write_all(sequence.as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_title_is_pushed_before_ours_is_set() {
        let push = SET.find("\x1b[22;0t").unwrap();
        let set = SET.find("\x1b]0;").unwrap();
        assert!(push < set);
        assert!(SET.ends_with("vtteams\x07"));
    }

    #[test]
    fn the_title_text_holds_no_control_that_could_end_its_sequence_early() {
        let text = SET
            .split_once("\x1b]0;")
            .and_then(|(_, t)| t.strip_suffix('\x07'))
            .unwrap();
        assert!(!text.chars().any(char::is_control));
    }

    #[test]
    fn restoring_pops_the_stack_and_says_nothing_else() {
        assert_eq!(RESTORE, "\x1b[23;0t");
    }

    /// The one test that touches the flag the real functions share, so that it cannot race another.
    #[test]
    fn what_is_set_is_restored_once_and_only_when_it_was_set() {
        let written = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut out = Vec::new();
            f(&mut out);
            String::from_utf8(out).unwrap()
        };
        SET_DONE.store(false, Ordering::Relaxed);
        assert_eq!(
            written(&|o| restore_on(o)),
            "",
            "nothing to restore before a set"
        );
        assert_eq!(written(&|o| set_on(o)), SET);
        assert_eq!(written(&|o| restore_on(o)), RESTORE, "put back once");
        assert_eq!(
            written(&|o| restore_on(o)),
            "",
            "a second restore (the panic hook, then the normal exit) pops nothing more"
        );
    }
}
