//! A paste that arrives as key presses.
//!
//! A terminal that can tell a paste from typing says so (bracketed paste: `Event::Paste`). The Windows console
//! cannot: crossterm has no paste support there, so pasting a text is the same as typing it, one key press for each
//! character and an Enter for each line break, all queued at once. Typed, the text goes wrong in ways typing cannot
//! show: in the standard keymap the first Enter sends the message, and in vim's insert mode every Enter copies the
//! indentation of the line before it, on top of the indentation the pasted line already has, so code staircases off
//! the right edge.
//!
//! A person cannot type twelve different characters between two looks at the keyboard (a slow frame holds back a
//! handful, a held-down key repeats one character), so that many text keys read together are a paste, and become the
//! `Event::Paste` the other terminals send. Pure, so it is tested without a terminal.

use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use std::time::{Duration, Instant};

/// This many text keys read together, not all the same, are a paste.
const MIN_RUN: usize = 12;
/// A line break followed at once by indentation, in a run at least this long, is a paste however short: typing does
/// not queue that, and typed with vim's auto-indent the indentation would be doubled.
const MIN_INDENTED_RUN: usize = 4;
/// Text keys arriving this soon after a paste belong to it: a long paste is read in several pieces, and the tail of
/// one must not be taken for typing (its Enter would send the message).
const CONTINUES_FOR: Duration = Duration::from_millis(200);

/// Turns runs of pasted key presses into paste events.
#[derive(Debug, Default)]
pub struct Coalescer {
    /// When the last piece of a paste was seen.
    last_paste: Option<Instant>,
}

/// What an event is for this purpose.
enum Piece {
    /// A key that types this character (Enter is a line break, Tab a tab).
    Text(char),
    /// A key release: it goes with the press before it and does not end a run.
    Release,
    /// Anything else: a shortcut, an arrow, the mouse, a focus change. It ends a run.
    Other,
}

fn classify(event: &Event) -> Piece {
    let Event::Key(key) = event else {
        return Piece::Other;
    };
    if key.kind == KeyEventKind::Release {
        return Piece::Release;
    }
    // Shift only changes which character a key types; anything else makes it a shortcut.
    // AltGr arrives as Ctrl+Alt on Windows.
    let mods_minus_shift = key.modifiers.difference(KeyModifiers::SHIFT);
    let plain = mods_minus_shift.is_empty()
        || mods_minus_shift == KeyModifiers::CONTROL | KeyModifiers::ALT;
    match key.code {
        KeyCode::Char(c) if plain => Piece::Text(c),
        KeyCode::Enter if key.modifiers.is_empty() => Piece::Text('\n'),
        KeyCode::Tab if key.modifiers.is_empty() => Piece::Text('\t'),
        _ => Piece::Other,
    }
}

impl Coalescer {
    /// `events` as one read of the terminal's queue gave them, with each paste in them made into one `Event::Paste`.
    /// Everything else is passed on, in order.
    pub fn process(&mut self, events: Vec<Event>, now: Instant) -> Vec<Event> {
        let mut out = Vec::with_capacity(events.len());
        let mut run: Vec<Event> = Vec::new();
        let mut text = String::new();
        let mut presses = 0;
        // Whether this read picks up where a paste read a moment ago left off: only the first run can be its tail.
        let mut continuing = self
            .last_paste
            .is_some_and(|at| now.saturating_duration_since(at) < CONTINUES_FOR);
        for event in events {
            match classify(&event) {
                Piece::Text(c) => {
                    text.push(c);
                    presses += 1;
                    run.push(event);
                }
                Piece::Release if !run.is_empty() => run.push(event),
                Piece::Release | Piece::Other => {
                    self.end_run(
                        &mut run,
                        &mut text,
                        &mut presses,
                        &mut continuing,
                        &mut out,
                        now,
                    );
                    out.push(event);
                }
            }
        }
        self.end_run(
            &mut run,
            &mut text,
            &mut presses,
            &mut continuing,
            &mut out,
            now,
        );
        out
    }

    /// Decide what a finished run of text keys was, and move it to `out` either way.
    fn end_run(
        &mut self,
        run: &mut Vec<Event>,
        text: &mut String,
        presses: &mut usize,
        continuing: &mut bool,
        out: &mut Vec<Event>,
        now: Instant,
    ) {
        if run.is_empty() {
            return;
        }
        let continues = std::mem::take(continuing);
        let mut chars = text.chars();
        let one_key_held_down = chars.next().is_none_or(|first| chars.all(|c| c == first));
        let indented_lines =
            *presses >= MIN_INDENTED_RUN && (text.contains("\n ") || text.contains("\n\t"));
        if continues || indented_lines || (*presses >= MIN_RUN && !one_key_held_down) {
            out.push(Event::Paste(std::mem::take(text)));
            self.last_paste = Some(now);
            run.clear();
        } else {
            out.append(run);
        }
        text.clear();
        *presses = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    fn key(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    /// What a Windows console queues for `text` pasted: a press and a release for each key, an Enter for each line
    /// break (the terminal turns a line feed into a carriage return on the way in).
    fn pasted_as_keys(text: &str) -> Vec<Event> {
        let mut events = Vec::new();
        for c in text.chars() {
            let code = match c {
                '\n' | '\r' => KeyCode::Enter,
                '\t' => KeyCode::Tab,
                c => KeyCode::Char(c),
            };
            events.push(key(code, KeyModifiers::NONE));
            events.push(Event::Key(KeyEvent::new_with_kind(
                code,
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )));
        }
        events
    }

    fn typed(text: &str) -> Vec<Event> {
        text.chars()
            .map(|c| key(KeyCode::Char(c), KeyModifiers::NONE))
            .collect()
    }

    #[test]
    fn a_pasted_text_arriving_as_key_presses_becomes_one_paste_with_its_line_breaks() {
        let text = "ERROR #16008: Unable to save text\n        -=-=-=-=-\n        Import EnsLib.InteropTools\n       \n        Class A.B Extends (C, D)\n        {\n  > ERROR #16000: Line:1799";
        let out = Coalescer::default().process(pasted_as_keys(text), Instant::now());
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(matches!(&out[0], Event::Paste(p) if p == text), "{out:?}");
    }

    #[test]
    fn typing_is_left_alone_even_with_an_enter_in_it() {
        let now = Instant::now();
        let mut c = Coalescer::default();
        let mut keys = typed("ok");
        keys.push(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            c.process(keys.clone(), now),
            keys,
            "a few keys queued behind a slow frame"
        );
        // Eleven different characters is still more than a person gets ahead of the loop.
        let eleven = typed("abcdefghijk");
        assert_eq!(c.process(eleven.clone(), now), eleven);
    }

    #[test]
    fn a_short_paste_with_an_indented_line_is_still_a_paste() {
        // Typed into vim's insert mode this would come out `  a` / `    b`: the Enter copies the indentation and
        // the pasted line brings its own.
        let keys = [
            key(KeyCode::Char(' '), KeyModifiers::NONE),
            key(KeyCode::Char(' '), KeyModifiers::NONE),
            key(KeyCode::Char('a'), KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
            key(KeyCode::Char(' '), KeyModifiers::NONE),
            key(KeyCode::Char(' '), KeyModifiers::NONE),
            key(KeyCode::Char('b'), KeyModifiers::NONE),
        ];
        let out = Coalescer::default().process(keys.to_vec(), Instant::now());
        assert!(
            matches!(&out[..], [Event::Paste(p)] if p == "  a\n  b"),
            "{out:?}"
        );
        // A line break followed by a letter is ordinary typing, however it arrived.
        let plain = [
            key(KeyCode::Char('o'), KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
            key(KeyCode::Char('k'), KeyModifiers::NONE),
        ];
        assert_eq!(
            Coalescer::default().process(plain.to_vec(), Instant::now()),
            plain
        );
    }

    #[test]
    fn a_held_down_key_is_not_a_paste() {
        let held = typed(&"j".repeat(40));
        assert_eq!(
            Coalescer::default().process(held.clone(), Instant::now()),
            held
        );
    }

    #[test]
    fn a_shortcut_ends_a_run_and_each_side_is_judged_on_its_own() {
        let mut events = typed("abcdefghijklmno");
        events.push(key(KeyCode::Char('s'), KeyModifiers::CONTROL));
        events.extend(typed("xy"));
        events.push(Event::FocusGained);
        let out = Coalescer::default().process(events, Instant::now());
        assert_eq!(out.len(), 5, "{out:?}");
        assert!(matches!(&out[0], Event::Paste(p) if p == "abcdefghijklmno"));
        assert!(matches!(&out[1], Event::Key(k) if k.modifiers == KeyModifiers::CONTROL));
        assert!(matches!(&out[2], Event::Key(k) if k.code == KeyCode::Char('x')));
        assert!(matches!(&out[3], Event::Key(k) if k.code == KeyCode::Char('y')));
        assert!(matches!(out[4], Event::FocusGained));
    }

    #[test]
    fn the_tail_of_a_paste_read_a_moment_later_is_still_part_of_it() {
        let mut c = Coalescer::default();
        let t0 = Instant::now();
        let first = c.process(typed("a long line of pasted text"), t0);
        assert!(matches!(&first[..], [Event::Paste(_)]));
        // The rest of the text was not queued yet; it ends with a line break that must not send anything.
        let mut tail = typed("end");
        tail.push(key(KeyCode::Enter, KeyModifiers::NONE));
        let second = c.process(tail, t0 + Duration::from_millis(60));
        assert!(
            matches!(&second[..], [Event::Paste(p)] if p == "end\n"),
            "{second:?}"
        );
        // A key long after is typing again.
        let later = c.process(typed("x"), t0 + Duration::from_secs(5));
        assert!(matches!(&later[..], [Event::Key(_)]), "{later:?}");
    }

    #[test]
    fn keys_that_type_nothing_and_other_events_pass_through_in_order() {
        let events = vec![
            key(KeyCode::Up, KeyModifiers::NONE),
            Event::FocusLost,
            key(KeyCode::Enter, KeyModifiers::SHIFT),
            key(KeyCode::Esc, KeyModifiers::NONE),
        ];
        let out = Coalescer::default().process(events.clone(), Instant::now());
        assert_eq!(out, events);
    }

    #[test]
    fn a_shifted_letter_and_a_tab_are_text() {
        let mut keys = typed("fn main() {");
        keys.push(key(KeyCode::Char('A'), KeyModifiers::SHIFT));
        keys.push(key(KeyCode::Tab, KeyModifiers::NONE));
        keys.push(key(KeyCode::Char('x'), KeyModifiers::NONE));
        let out = Coalescer::default().process(keys, Instant::now());
        assert!(
            matches!(&out[..], [Event::Paste(p)] if p == "fn main() {A\tx"),
            "{out:?}"
        );
    }

    #[test]
    fn altgr_characters_are_coalesced_into_a_paste() {
        let mut keys = typed("Thanks for the note, mail me");
        keys.push(key(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        keys.extend(typed("x.de"));
        keys.push(key(KeyCode::Enter, KeyModifiers::NONE));
        keys.extend(typed("ok"));
        let out = Coalescer::default().process(keys, Instant::now());
        assert_eq!(out.len(), 1, "{out:?}");
        assert!(
            matches!(&out[0], Event::Paste(p) if p == "Thanks for the note, mail me@x.de\nok"),
            "{out:?}"
        );
    }

    #[test]
    fn real_shortcuts_still_pass_through_as_keys() {
        let mut keys = typed("abcdefghijklmno");
        keys.push(key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        keys.extend(typed("end"));
        let out = Coalescer::default().process(keys, Instant::now());
        assert_eq!(out.len(), 5, "{out:?}");
        assert!(
            matches!(&out[0], Event::Paste(p) if p == "abcdefghijklmno"),
            "{out:?}"
        );
        assert!(
            matches!(&out[1], Event::Key(k) if k.code == KeyCode::Char('v') && k.modifiers == KeyModifiers::CONTROL),
            "{out:?}"
        );
        assert!(
            matches!(&out[2], Event::Key(k) if k.code == KeyCode::Char('e')),
            "{out:?}"
        );
    }
}
