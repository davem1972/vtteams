//! The recent-chat list, pending chats and its refresh lifecycle.

use super::{describe_running, throttle_hold};
use crate::cadence::Schedule;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use teams_client::{Chat, ChatKind, Error, Msg, Preview, preview_text};

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
enum Refresh {
    #[default]
    Idle,
    Visible,
    Quiet,
}

pub struct ChatList {
    items: Vec<Chat>,
    /// A chat opened here remains listed until the service includes it in its own list.
    pending: Vec<Chat>,
    loaded: bool,
    refresh: Refresh,
    error: Option<String>,
    pub(super) sched: Schedule,
    /// Chats with a favourite change in flight, so a second toggle on the same one waits for the first answer
    /// instead of racing it: both would read-modify-write one shared, service-side value, and whichever answer
    /// arrives last would win, not necessarily the one the person asked for last.
    favorite_pending: HashSet<String>,
    /// The list is what the last session left (the start-up cache) and the service has not answered yet.
    cached: bool,
    /// When the list was last read from the service (or, while `cached`, when the cache says it was).
    read_at: Option<DateTime<Utc>>,
}

impl ChatList {
    pub(super) fn new() -> Self {
        Self {
            items: Vec::new(),
            pending: Vec::new(),
            loaded: false,
            refresh: Refresh::Idle,
            error: None,
            sched: Schedule::new(),
            favorite_pending: HashSet::new(),
            cached: false,
            read_at: None,
        }
    }

    pub fn items(&self) -> &[Chat] {
        &self.items
    }

    pub fn loaded(&self) -> bool {
        self.loaded
    }

    /// Whether the list on screen is the cached one, not yet replaced by the service's own.
    pub fn is_cached(&self) -> bool {
        self.cached
    }

    /// When the list was read from the service, as far as is known.
    pub(super) fn read_at(&self) -> Option<DateTime<Utc>> {
        self.read_at
    }

    /// Show `chats` from the start-up cache until the service answers. Never over a list that has been read: `read_at`
    /// is when the cache says it was read. The refresh schedule is not touched, so the first real read happens as usual.
    pub(super) fn receive_cached(&mut self, mut chats: Vec<Chat>, read_at: DateTime<Utc>) {
        if self.loaded {
            return;
        }
        chats.sort_by_key(|c| std::cmp::Reverse(c.updated));
        self.items = chats;
        self.loaded = true;
        self.cached = true;
        self.read_at = Some(read_at);
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(super) fn is_fetching(&self) -> bool {
        self.refresh != Refresh::Idle
    }

    pub(super) fn shows_loading(&self) -> bool {
        self.refresh == Refresh::Visible
    }

    pub(super) fn start_refresh(&mut self, quiet: bool) -> bool {
        if self.is_fetching() {
            return false;
        }
        self.refresh = if quiet {
            Refresh::Quiet
        } else {
            Refresh::Visible
        };
        self.sched.started();
        true
    }

    pub(super) fn receive(&mut self, mut chats: Vec<Chat>) {
        // A group the backend has not read the people of yet comes back with no topic and no members, which would be
        // titled "(untitled chat)". Whoever the list held for it before (typically the start-up cache) is better than
        // that until the real roster arrives; an empty roster only ever means "not known", since a real group has at
        // least you in it, so this cannot hide a change.
        for chat in &mut chats {
            if chat.kind != ChatKind::OneOnOne
                && chat.members.is_empty()
                && chat.topic.is_none()
                && let Some(known) = self
                    .items
                    .iter()
                    .find(|old| old.id == chat.id && !old.members.is_empty())
            {
                chat.members.clone_from(&known.members);
            }
        }
        self.pending.retain(|p| !chats.iter().any(|c| c.id == p.id));
        chats.extend(self.pending.iter().cloned());
        chats.sort_by_key(|c| std::cmp::Reverse(c.updated));
        self.items = chats;
        self.loaded = true;
        self.cached = false;
        self.read_at = Some(Utc::now());
        self.refresh = Refresh::Idle;
        self.error = None;
        self.sched.succeeded();
    }

    pub(super) fn fail(&mut self, error: &Error) {
        self.refresh = Refresh::Idle;
        self.sched.failed(throttle_hold(error));
        self.error = Some(describe_running(error));
    }

    /// Set where `chat_id` is among the favourites (`None` takes it out), at once, before the service has answered.
    pub(super) fn set_favorite(&mut self, chat_id: &str, favorite: Option<u32>) {
        for c in self.items.iter_mut().chain(self.pending.iter_mut()) {
            if c.id == chat_id {
                c.favorite = favorite;
            }
        }
    }

    /// Reserve before changing a chat's favourite status, so a second change to the same chat waits for the first
    /// answer (mirrors `Reactions::begin`).
    pub(super) fn begin_favorite(&mut self, chat_id: String) -> bool {
        self.favorite_pending.insert(chat_id)
    }

    pub(super) fn complete_favorite(&mut self, chat_id: &str) {
        self.favorite_pending.remove(chat_id);
    }

    /// The place a chat added to the favourites now would take: after the last one.
    pub(super) fn next_favorite(&self) -> u32 {
        self.items
            .iter()
            .filter_map(|c| c.favorite)
            .max()
            .unwrap_or(0)
            + 1
    }

    pub(super) fn remember(&mut self, chat: Chat) {
        if !self.items.iter().any(|c| c.id == chat.id) {
            self.pending.push(chat.clone());
            self.items.insert(0, chat);
        }
    }

    pub(super) fn record_sent(&mut self, chat_id: &str, sent: &Msg) {
        if let Some(c) = self.items.iter_mut().find(|c| c.id == chat_id) {
            c.updated = sent.created;
            c.preview = Some(Preview {
                created: sent.created,
                from_id: sent.from_id.clone(),
                from_name: Some(sent.from_name.clone()),
                text: preview_text(&sent.body, sent.html),
                automated: false,
            });
            if let Some(p) = self.pending.iter_mut().find(|p| p.id == chat_id) {
                p.clone_from(c);
            }
            self.items.sort_by_key(|c| std::cmp::Reverse(c.updated));
        }
    }

    #[cfg(test)]
    pub(super) fn edit_items(&mut self, edit: impl FnOnce(&mut Vec<Chat>)) {
        edit(&mut self.items);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: &str, topic: Option<&str>, members: &[(&str, &str)]) -> Chat {
        Chat {
            id: id.into(),
            kind: ChatKind::Group,
            topic: topic.map(String::from),
            members: members
                .iter()
                .map(|(id, name)| teams_client::Member {
                    not_in_directory: false,
                    id: (*id).into(),
                    name: (*name).into(),
                })
                .collect(),
            updated: Utc::now(),
            preview: None,
            last_read: None,
            web_url: None,
            favorite: None,
        }
    }

    #[test]
    fn a_group_whose_people_are_not_known_yet_keeps_what_the_list_held_for_it() {
        let mut chats = ChatList::new();
        chats.receive(vec![group("g", None, &[("a", "Ada"), ("me", "Me")])]);
        // The next list names the group but not its people yet: the people already known stay.
        chats.receive(vec![group("g", None, &[])]);
        assert_eq!(chats.items()[0].members.len(), 2);
        assert_eq!(chats.items()[0].title("me"), "Ada");
        // Once the backend knows them, its own answer wins, including a changed roster.
        chats.receive(vec![group("g", None, &[("b", "Grace"), ("me", "Me")])]);
        assert_eq!(chats.items()[0].title("me"), "Grace");
    }

    #[test]
    fn a_chat_with_its_own_title_or_that_is_new_is_left_as_the_backend_gave_it() {
        let mut chats = ChatList::new();
        chats.receive(vec![group("g", None, &[("a", "Ada"), ("me", "Me")])]);
        // A named group (no roster needed) and a group the list never held stay empty.
        chats.receive(vec![
            group("g", Some("Platform"), &[]),
            group("h", None, &[]),
        ]);
        let by_id = |id: &str| chats.items().iter().find(|c| c.id == id).unwrap().clone();
        assert!(by_id("g").members.is_empty(), "it has a title of its own");
        assert!(by_id("h").members.is_empty(), "nothing was known about it");
    }
    #[test]
    fn a_favourite_change_in_flight_blocks_a_second_one_to_the_same_chat_but_not_others() {
        let mut chats = ChatList::new();
        assert!(chats.begin_favorite("a".into()));
        assert!(
            !chats.begin_favorite("a".into()),
            "the first change is still in flight"
        );
        assert!(
            chats.begin_favorite("b".into()),
            "a different chat is not blocked"
        );
        chats.complete_favorite("a");
        assert!(chats.begin_favorite("a".into()), "free again once answered");
    }

    #[test]
    fn a_quiet_request_keeps_its_visibility_until_completion_and_can_retry_after_failure() {
        let mut chats = ChatList::new();
        chats.receive(vec![]);
        assert!(chats.start_refresh(true));
        assert!(
            !chats.start_refresh(false),
            "the existing request owns the slot"
        );
        assert!(chats.is_fetching());
        assert!(!chats.shows_loading());
        chats.fail(&Error::Unexpected {
            operation: "refresh chats",
            detail: "failed",
        });
        assert!(chats.loaded(), "a failed refresh keeps the loaded list");
        assert!(!chats.is_fetching());
        assert!(!chats.shows_loading());
        assert!(chats.error().is_some());
        assert!(chats.start_refresh(false));
        assert!(chats.shows_loading());
        chats.receive(vec![]);
        assert!(!chats.is_fetching());
        assert!(chats.error().is_none());
    }
}
