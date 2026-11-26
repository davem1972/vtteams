//! Reaction catalogue loading and serialization of optimistic changes per message.

use crate::emoji;
use std::collections::HashSet;
use teams_client::EmojiInfo;

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
enum Catalog {
    #[default]
    Unknown,
    Loading,
    Loaded,
}

pub(super) struct Reactions {
    /// What a reaction can be.
    entries: Vec<emoji::Entry>,
    /// What can be written into a message: those, then the shortcode table's other emoji.
    insertable: Vec<emoji::Entry>,
    catalog: Catalog,
    /// Someone choosing a reaction (not only writing a message) is waiting for the list, so that its failing is worth
    /// saying.
    reacting: bool,
    pending: HashSet<(String, String)>,
}

impl Reactions {
    pub(super) fn new() -> Self {
        Self {
            entries: emoji::classic(),
            insertable: emoji::catalog().to_vec(),
            catalog: Catalog::Unknown,
            reacting: false,
            pending: HashSet::new(),
        }
    }

    pub(super) fn entries(&self) -> &[emoji::Entry] {
        &self.entries
    }

    pub(super) fn insert_entries(&self) -> &[emoji::Entry] {
        &self.insertable
    }

    /// Whether to ask the backend for the list now: it is not loaded and not being loaded. `reacting` says that the
    /// picker was opened to react, which a failure to load it is then reported for.
    pub(super) fn start_catalog(&mut self, reacting: bool) -> bool {
        if self.catalog == Catalog::Loaded {
            return false;
        }
        self.reacting |= reacting;
        if self.catalog != Catalog::Unknown {
            return false;
        }
        self.catalog = Catalog::Loading;
        true
    }

    pub(super) fn receive_catalog(&mut self, entries: Vec<EmojiInfo>) {
        self.insertable = emoji::insertable(entries.clone());
        self.entries = emoji::reactions(entries);
        self.catalog = Catalog::Loaded;
        self.reacting = false;
    }

    /// The list could not be had. True when someone was choosing a reaction, who is to be told that only the classic six
    /// are offered (a message can still be written from the shortcode table, so that is not worth a word).
    pub(super) fn fail_catalog(&mut self) -> bool {
        self.catalog = Catalog::Unknown;
        std::mem::take(&mut self.reacting)
    }

    pub(super) fn is_pending(&self, chat: &str, message: &str) -> bool {
        self.pending
            .contains(&(chat.to_string(), message.to_string()))
    }

    /// Reserve before changing the displayed reaction, so a second change waits for the first answer.
    pub(super) fn begin(&mut self, chat: String, message: String) -> bool {
        self.pending.insert((chat, message))
    }

    pub(super) fn complete(&mut self, chat: &str, message: &str) {
        self.pending
            .remove(&(chat.to_string(), message.to_string()));
    }

    #[cfg(test)]
    pub(super) fn catalog_loaded(&self) -> bool {
        self.catalog == Catalog::Loaded
    }

    #[cfg(test)]
    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_catalogue_keeps_the_classics_and_can_be_requested_again() {
        let mut reactions = Reactions::new();
        assert_eq!(reactions.entries().len(), 6);
        assert!(reactions.start_catalog(true));
        assert!(!reactions.start_catalog(true));
        assert!(reactions.fail_catalog(), "someone was reacting");
        assert_eq!(reactions.entries().len(), 6);
        assert!(reactions.start_catalog(false));
        assert!(
            !reactions.fail_catalog(),
            "a list asked for only to write a message fails quietly"
        );
        assert!(reactions.start_catalog(false));
        assert!(!reactions.start_catalog(true), "already being loaded");
        assert!(reactions.fail_catalog(), "but a reaction joined the wait");
        assert!(reactions.start_catalog(true));
        reactions.receive_catalog(vec![EmojiInfo {
            emoji: "🦀".into(),
            name: "crab".into(),
            aliases: vec![],
        }]);
        assert!(reactions.entries().iter().any(|e| e.emoji == "🦀"));
        assert!(
            !reactions.start_catalog(true),
            "a loaded catalogue is reused"
        );
    }

    #[test]
    fn pending_reactions_serialize_per_message_and_release_independently() {
        let mut reactions = Reactions::new();
        assert!(reactions.begin("chat".into(), "1".into()));
        assert!(!reactions.begin("chat".into(), "1".into()));
        assert!(reactions.begin("chat".into(), "2".into()));
        reactions.complete("chat", "1");
        assert!(!reactions.is_pending("chat", "1"));
        assert!(reactions.is_pending("chat", "2"));
        assert!(reactions.begin("chat".into(), "1".into()));
    }
}
