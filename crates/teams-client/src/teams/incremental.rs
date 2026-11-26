//! Reading only what changed since the last read.
//!
//! The chat service supports two kinds of incremental read, both observed against a live tenant:
//!
//! * A conversation's messages can be asked for from a `startTime`. The bound is a message's `version` (milliseconds on
//!   the service's clock), inclusive, and the version of a message changes whenever it is edited, reacted to or deleted
//!   (a deleted message stays, with its content emptied). So "everything with a version at or after the newest one
//!   seen" returns the newest message again plus whatever changed, including changes to older messages, in about 2 kB
//!   instead of a 60 kB page.
//! * The conversation list carries a `syncState` link. Following it returns the conversations that changed since the
//!   link was issued, plus a link for the next call. Right after a full read the answers still repeat most of the list
//!   and narrow by one step per call (about fourteen calls); once caught up an unchanged poll is empty. Every answer is
//!   a superset of the changes since the link, so merging answers is correct before and after that point.
//!
//! The state here is the backend's own memory of what it last read. It is never trusted for long: [`FULL_REFRESH`]
//! bounds how stale it can get, which also covers what an incremental read cannot express (a chat left elsewhere, a
//! message that was stored out of order).

use crate::model::{Msg, Page};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long the remembered view is used before everything is read again.
pub(super) const FULL_REFRESH: Duration = Duration::from_secs(300);
/// Messages whose version is within this many milliseconds of the newest one seen are asked for again, so a message
/// stored a moment out of order is not skipped.
pub(super) const OVERLAP_MS: u64 = 5_000;
/// Messages per request; an incremental answer this long may have been cut short, so it is not trusted.
pub(super) const PAGE: usize = 50;
/// Messages remembered per chat.
const KEEP: usize = 200;
/// Chats whose messages are remembered at once.
const MAX_THREADS: usize = 16;

/// A message's position on the service's clock, in milliseconds: its `version`, else its `id`.
pub(super) fn stamp(m: &Value) -> Option<u64> {
    ["version", "id"]
        .iter()
        .find_map(|k| m[*k].as_str().and_then(|s| s.parse().ok()))
}

/// The newest page of one chat as last read, and where to continue from.
pub(super) struct Thread {
    messages: Vec<Msg>,
    older: Option<String>,
    cursor: u64,
    read_at: Instant,
    used_at: Instant,
    /// Set when trimming to [`KEEP`] has dropped messages, so [`Self::since`] refuses to build on this thread any
    /// more: `older` was captured by the full read that started it and does not move when the front is trimmed, so
    /// fetching older history from here would land after the trimmed messages and silently skip them. The next read
    /// has to be a full one, which remembers a fresh `older` that matches what is actually still held.
    needs_full: bool,
}

impl Thread {
    pub(super) fn new(messages: Vec<Msg>, older: Option<String>, cursor: u64) -> Self {
        let now = Instant::now();
        Self {
            messages,
            older,
            cursor,
            read_at: now,
            used_at: now,
            needs_full: false,
        }
    }

    /// The bound to ask for, or `None` when the page was read too long ago to build on, or trimming has made it
    /// unsafe to (see [`Self::needs_full`]).
    pub(super) fn since(&mut self, max_age: Duration) -> Option<u64> {
        self.used_at = Instant::now();
        (!self.needs_full && self.read_at.elapsed() < max_age)
            .then(|| self.cursor.saturating_sub(OVERLAP_MS))
    }

    /// Fold in the messages an incremental read returned and give back the page as a full read would show it, with
    /// anything older that changed.
    pub(super) fn merge(&mut self, incoming: Vec<Msg>, newest: Option<u64>) -> Page {
        for m in incoming {
            match self.messages.iter_mut().find(|e| e.id == m.id) {
                Some(existing) => *existing = m,
                None => self.messages.push(m),
            }
        }
        self.messages.sort_by_key(|m| m.created);
        if let Some(n) = newest {
            self.cursor = self.cursor.max(n);
        }
        let page = Page {
            messages: self.messages.clone(),
            older: self.older.clone(),
        };
        let excess = self.messages.len().saturating_sub(KEEP);
        if excess > 0 {
            self.messages.drain(..excess);
            self.needs_full = true;
        }
        page
    }
}

/// Remember `thread` for `chat_id`, forgetting the least recently used chat if there are too many.
pub(super) fn remember(threads: &mut HashMap<String, Thread>, chat_id: &str, thread: Thread) {
    if !threads.contains_key(chat_id)
        && threads.len() >= MAX_THREADS
        && let Some(oldest) = threads
            .iter()
            .min_by_key(|(_, t)| t.used_at)
            .map(|(id, _)| id.clone())
    {
        threads.remove(&oldest);
    }
    threads.insert(chat_id.to_string(), thread);
}

/// The conversation list as last read, kept current by following the service's `syncState` links.
pub(super) struct ChatIndex {
    conversations: HashMap<String, Value>,
    /// The link to follow for the next change; `None` when the service gave none (then every read is a full one).
    pub(super) sync: Option<String>,
    read_at: Instant,
}

impl ChatIndex {
    /// The index after a full read. `chain` is an earlier chain to keep following: it has caught up by now, and a
    /// fresh link would start the catch-up again. Anything it returns is the conversation as it is now, so it cannot
    /// replace a newer state with an older one.
    pub(super) fn from_full(
        answer: &Value,
        link: impl Fn(&str) -> bool,
        chain: Option<String>,
    ) -> Self {
        let own = answer["_metadata"]["syncState"]
            .as_str()
            .filter(|l| link(l))
            .map(String::from);
        let mut index = Self {
            conversations: HashMap::new(),
            sync: chain.or(own),
            read_at: Instant::now(),
        };
        index.upsert(answer);
        index
    }

    pub(super) fn is_fresh(&self, max_age: Duration) -> bool {
        self.read_at.elapsed() < max_age
    }

    /// Fold in an incremental answer and move to the link it names (none if it names no usable one).
    pub(super) fn merge(&mut self, answer: &Value, link: impl Fn(&str) -> bool) {
        self.upsert(answer);
        self.sync = answer["_metadata"]["syncState"]
            .as_str()
            .filter(|l| link(l))
            .map(String::from);
    }

    fn upsert(&mut self, answer: &Value) {
        let nothing =
            |v: &Value| v.is_null() || v.as_object().is_some_and(serde_json::Map::is_empty);
        for c in answer["conversations"].as_array().into_iter().flatten() {
            let Some(id) = c["id"].as_str() else {
                continue;
            };
            let mut entry = c.clone();
            // The change feed also sends conversations with an empty `lastMessage`; for one that is already listed
            // that says nothing about its last message, so the one known stays and the rest of the entry is renewed.
            if nothing(&entry["lastMessage"])
                && let Some(known) = self.conversations.get(id)
                && !nothing(&known["lastMessage"])
            {
                entry["lastMessage"] = known["lastMessage"].clone();
            }
            self.conversations.insert(id.to_string(), entry);
        }
    }

    pub(super) fn conversations(&self) -> Vec<Value> {
        self.conversations.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Delivery;
    use chrono::{TimeZone, Utc};
    use serde_json::json;

    fn msg(id: &str, secs: i64, body: &str) -> Msg {
        let t = Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap();
        Msg {
            id: id.into(),
            created: t,
            modified: t,
            edited: None,
            from_id: None,
            from_name: String::new(),
            system: false,
            deleted: false,
            html: false,
            body: body.into(),
            subject: None,
            attachments: vec![],
            reactions: vec![],
            mentions: vec![],
            delivery: Delivery::Sent,
            root_id: None,
        }
    }

    #[test]
    fn a_message_is_placed_by_its_version_else_its_id() {
        assert_eq!(stamp(&json!({"id": "5", "version": "9"})), Some(9));
        assert_eq!(stamp(&json!({"id": "5"})), Some(5));
        assert_eq!(stamp(&json!({"id": "5", "version": "x"})), Some(5));
        assert_eq!(stamp(&json!({"id": "x"})), None);
        assert_eq!(
            stamp(&json!({"id": 5})),
            None,
            "numbers are not what the service sends"
        );
    }

    #[test]
    fn a_change_that_names_a_listed_chat_without_a_last_message_does_not_empty_it() {
        let listed = json!({"conversations": [{"id": "19:a@thread.v2", "threadProperties": {"topic": "old"},
            "lastMessage": {"messagetype": "Text", "content": "hi", "originalarrivaltime": "2026-10-27T10:00:00Z"}}]});
        let mut index = ChatIndex::from_full(&listed, |_| true, None);
        let stub = json!({"conversations": [
            {"id": "19:a@thread.v2", "threadProperties": {"topic": "renamed"}, "lastMessage": {}},
            {"id": "19:new@thread.v2", "lastMessage": {}}
        ]});
        index.merge(&stub, |_| true);
        let all = index.conversations();
        let a = all.iter().find(|c| c["id"] == "19:a@thread.v2").unwrap();
        assert_eq!(
            a["lastMessage"]["content"], "hi",
            "the last message is kept"
        );
        assert_eq!(
            a["threadProperties"]["topic"], "renamed",
            "the rest is renewed"
        );
        let new = all.iter().find(|c| c["id"] == "19:new@thread.v2").unwrap();
        assert!(
            new["lastMessage"].as_object().unwrap().is_empty(),
            "nothing to keep for an unknown chat"
        );
    }
    #[test]
    fn merging_replaces_by_id_keeps_order_and_never_moves_the_cursor_back() {
        let mut t = Thread::new(
            vec![msg("1", 1, "a"), msg("2", 2, "b")],
            Some("more".into()),
            20_000,
        );
        let page = t.merge(
            vec![msg("1", 1, "a, edited"), msg("3", 3, "c")],
            Some(30_000),
        );
        let bodies: Vec<&str> = page.messages.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(bodies, ["a, edited", "b", "c"]);
        assert_eq!(page.older.as_deref(), Some("more"));
        t.merge(vec![], Some(10_000));
        assert_eq!(t.since(FULL_REFRESH), Some(30_000 - OVERLAP_MS));
    }

    #[test]
    fn a_change_to_a_message_older_than_what_is_kept_is_still_returned_once() {
        let many: Vec<Msg> = (0..i64::try_from(KEEP).unwrap())
            .map(|i| msg(&format!("m{i}"), 100 + i, "x"))
            .collect();
        let mut t = Thread::new(many, None, 1);
        let page = t.merge(vec![msg("old", 1, "edited long ago")], None);
        assert!(page.messages.iter().any(|m| m.id == "old"));
        assert_eq!(page.messages.len(), KEEP + 1);
        let next = t.merge(vec![], None);
        assert!(
            next.messages.iter().all(|m| m.id != "old"),
            "then it is dropped to bound memory"
        );
    }

    #[test]
    fn trimming_the_remembered_page_forces_the_next_read_to_be_a_full_one() {
        // `older` was captured by the full read that started this thread and does not move when the front is
        // trimmed; building on it after a trim would fetch older history from the wrong place and skip whatever
        // was just dropped. A fresh full read remembers a fresh, matching `older`, so after the one forced full
        // read the thread can be built on again.
        let many: Vec<Msg> = (0..i64::try_from(KEEP).unwrap())
            .map(|i| msg(&format!("m{i}"), 100 + i, "x"))
            .collect();
        let mut t = Thread::new(many, Some("older-link".into()), 1);
        assert!(
            t.since(Duration::from_secs(60)).is_some(),
            "nothing trimmed yet"
        );
        t.merge(vec![msg("new", 2, "pushes the oldest message out")], None);
        assert_eq!(
            t.since(Duration::from_secs(60)),
            None,
            "a trim happened; the next read must be in full"
        );
        let mut fresh = Thread::new(vec![], Some("fresh-older-link".into()), 2);
        assert!(
            fresh.since(Duration::from_secs(60)).is_some(),
            "a full read's fresh thread can be built on again"
        );
    }

    #[test]
    fn an_old_page_is_not_built_on() {
        let mut t = Thread::new(vec![], None, 100_000);
        assert_eq!(t.since(Duration::ZERO), None);
        assert!(t.since(Duration::from_secs(60)).is_some());
    }

    #[test]
    fn the_least_recently_used_chat_is_forgotten_first() {
        let mut threads = HashMap::new();
        for i in 0..MAX_THREADS {
            remember(&mut threads, &format!("c{i}"), Thread::new(vec![], None, 1));
        }
        threads.get_mut("c0").unwrap().since(FULL_REFRESH);
        remember(&mut threads, "new", Thread::new(vec![], None, 1));
        assert_eq!(threads.len(), MAX_THREADS);
        assert!(threads.contains_key("c0") && threads.contains_key("new"));
        assert!(!threads.contains_key("c1"));
    }

    #[test]
    fn the_index_follows_trusted_links_and_merges_by_id() {
        let trusted = |l: &str| l.starts_with("https://ok/");
        let full = json!({"conversations": [{"id": "a", "n": 1}, {"id": "b", "n": 1}],
                          "_metadata": {"syncState": "https://ok/1"}});
        let mut index = ChatIndex::from_full(&full, trusted, None);
        assert_eq!(index.sync.as_deref(), Some("https://ok/1"));
        index.merge(
            &json!({"conversations": [{"id": "b", "n": 2}, {"id": "c", "n": 1}],
                    "_metadata": {"syncState": "https://ok/2"}}),
            trusted,
        );
        let mut seen: Vec<(String, i64)> = index
            .conversations()
            .iter()
            .map(|c| {
                (
                    c["id"].as_str().unwrap().to_string(),
                    c["n"].as_i64().unwrap(),
                )
            })
            .collect();
        seen.sort();
        assert_eq!(seen, [("a".into(), 1), ("b".into(), 2), ("c".into(), 1)]);
        assert_eq!(index.sync.as_deref(), Some("https://ok/2"));
        index.merge(
            &json!({"conversations": [], "_metadata": {"syncState": "https://evil/3"}}),
            trusted,
        );
        assert_eq!(index.sync, None, "a link to another host is dropped");
    }

    #[test]
    fn a_full_read_keeps_the_chain_that_has_caught_up() {
        let trusted = |_: &str| true;
        let full = json!({"conversations": [], "_metadata": {"syncState": "https://ok/new"}});
        assert_eq!(
            ChatIndex::from_full(&full, trusted, Some("https://ok/caught-up".into()))
                .sync
                .as_deref(),
            Some("https://ok/caught-up")
        );
        assert_eq!(
            ChatIndex::from_full(&full, trusted, None).sync.as_deref(),
            Some("https://ok/new")
        );
    }
}
