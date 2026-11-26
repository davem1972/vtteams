//! Completing an `@name` while it is typed in the message box.
//!
//! The people offered are the open chat's (`App::attendees`) other than you, matched on the start of any word of their
//! name; in a channel they are the people who have posted in it (`App::mention_candidates`), most recent first, since
//! a channel's own roster cannot be read. The list is worked out from the text and the cursor each time it is needed, so there is almost nothing to
//! keep: which entry is chosen, and the `@` the person dismissed. Choosing writes the full display name, which is what
//! `convert::extract_mentions` looks for when the message is sent.

use super::{App, Focus, Overlay};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use teams_client::Member;

/// How many people are offered at once.
pub const MAX_SUGGESTIONS: usize = 6;

#[derive(Default)]
pub(super) struct MentionState {
    /// The chosen entry for the text typed after the `@` (a different text starts again at the first).
    chosen: Option<(String, usize)>,
    /// The `@` (row, column) whose list was closed with Esc: it is not offered again until that `@` is gone.
    dismissed: Option<(usize, usize)>,
}

/// What the message box offers for the `@name` being typed.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestions {
    /// The column of the `@` in the cursor's line.
    pub from: usize,
    /// The text typed after it.
    pub query: String,
    pub people: Vec<Member>,
    pub sel: usize,
}

/// Whether `name` has a word, or a run of words, starting with `query` (compared without regard to case).
fn matches(name: &str, query: &str) -> bool {
    let name = name.to_lowercase();
    let query = query.to_lowercase();
    name.char_indices()
        .filter(|(i, _)| *i == 0 || name[..*i].ends_with(|c: char| c.is_whitespace() || c == '-'))
        .any(|(i, _)| name[i..].starts_with(&query))
}

impl App {
    /// The people to offer for the `@name` at the cursor, if one is being typed and there is anyone to offer.
    pub fn mention_suggestions(&self) -> Option<Suggestions> {
        if self.focus != Focus::Editor || self.overlay != Overlay::None {
            return None;
        }
        let (from, query) = self.editor.mention_at_cursor()?;
        let (row, _) = self.editor.cursor();
        if self.mention.dismissed == Some((row, from)) {
            return None;
        }
        let me = self.me_id();
        let in_channel = self.current_channel().is_some();
        if !in_channel {
            self.current_chat()?;
        }
        let mut people: Vec<Member> = Vec::new();
        for m in self.mention_candidates().iter() {
            if m.id != me
                && !m.name.trim().is_empty()
                && matches(&m.name, &query)
                && !people.iter().any(|p| p.id == m.id)
            {
                people.push(m.clone());
            }
        }
        if people.is_empty() {
            return None;
        }
        // Those whose whole name starts with what was typed come first, then by name. In a channel the people come
        // most recent poster first and stay in that order: the stable sort keeps it among equals.
        let q = query.to_lowercase();
        people.sort_by_cached_key(|p| {
            (
                !p.name.to_lowercase().starts_with(&q),
                if in_channel {
                    String::new()
                } else {
                    p.name.to_lowercase()
                },
            )
        });
        people.truncate(MAX_SUGGESTIONS);
        let sel = match &self.mention.chosen {
            Some((text, i)) if *text == query => (*i).min(people.len() - 1),
            _ => 0,
        };
        Some(Suggestions {
            from,
            query,
            people,
            sel,
        })
    }

    /// A key in the message box while a completion is offered. True when it was used.
    pub(super) fn on_mention_key(&mut self, key: &KeyEvent) -> bool {
        let Some(s) = self.mention_suggestions() else {
            return false;
        };
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
        {
            return false;
        }
        let n = s.people.len();
        match key.code {
            KeyCode::Up => {
                self.mention.chosen = Some((s.query, (s.sel + n - 1) % n));
            }
            KeyCode::Down => {
                self.mention.chosen = Some((s.query, (s.sel + 1) % n));
            }
            // Enter is a new line in vim's insert mode and sends in the standard keymap: with a list open it chooses.
            KeyCode::Tab | KeyCode::Enter => self.accept_mention(&s),
            KeyCode::Esc => {
                let (row, _) = self.editor.cursor();
                self.mention.dismissed = Some((row, s.from));
            }
            _ => return false,
        }
        true
    }

    fn accept_mention(&mut self, s: &Suggestions) {
        let name = s.people[s.sel].name.trim();
        self.editor
            .replace_before_cursor(s.from, &format!("@{name} "));
        self.mention.chosen = None;
    }

    /// Say that these typed `@name`s were not sent as mentions because more than one person has the name: the text
    /// went as plain words, and whoever was meant has not been pinged. Both people are still offered while typing; the
    /// popup cannot tell them apart, so it is at send time that the person finds out.
    pub(super) fn say_ambiguous_mentions(&mut self, names: &[String]) {
        let names: Vec<String> = names.iter().map(|n| teams_client::clean(n)).collect();
        let said = match names.as_slice() {
            [] => return,
            [one] => format!("two people are called {one}: the mention was not sent as a mention"),
            many => format!(
                "more than one person is called each of {}: those mentions were not sent as mentions",
                many.join(", ")
            ),
        };
        self.set_status(said, true);
    }

    /// Forget a dismissal once the `@` it was for is gone (called after every key in the message box).
    pub(super) fn settle_mention(&mut self) {
        if self.editor.mention_at_cursor().is_none() {
            self.mention.dismissed = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn a_name_matches_where_any_of_its_words_begin() {
        assert!(matches("Ada Lovelace", ""));
        assert!(matches("Ada Lovelace", "ad"));
        assert!(matches("Ada Lovelace", "LOVE"));
        assert!(matches("Ada Lovelace", "ada l"));
        assert!(matches("Anne-Marie Smith", "mar"));
        assert!(!matches("Ada Lovelace", "ovel"));
        assert!(!matches("Ada Lovelace", "ada x"));
    }
}
