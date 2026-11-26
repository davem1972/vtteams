//! A conversation's messages and the transitions that keep fetching, selection and rendering in sync.

use super::{describe_running, throttle_hold};
use crate::cadence::Schedule;
use crate::render::Ctx;
use crate::transcript::{self, Transcript, TranscriptKey};
use crate::workers::Lane;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::time::Instant;
use teams_client::{Delivery, Error, Msg, Page, Reaction, ReplyTarget, same_emoji};

/// Chats of mostly events get a bounded number of older pages to find something to read.
pub(super) const AUTOFILL_MIN_MESSAGES: usize = 8;
pub(super) const AUTOFILL_MAX_PAGES: u8 = 6;

/// Only one message request can be in flight for a conversation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fetch {
    Newest {
        quiet: bool,
    },
    Older {
        automatic: bool,
    },
    /// An older page fetched to reach a message found by a search (see [`Seek`]). Quiet like an automatic page, and it
    /// spends none of the automatic-page budget: someone asked for it, but what they asked for is the message, not a
    /// spinner.
    Seek,
}

impl Fetch {
    pub(super) fn is_older(self) -> bool {
        matches!(self, Self::Older { .. } | Self::Seek)
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Seek => "older page (looking for a message)",
            _ if self.is_older() => "older page",
            _ => "poll",
        }
    }

    /// Which queue the request waits in: what a person is watching a spinner for (the first page of a chat just
    /// opened, `:refresh`, scrolling back for older messages) runs on the interactive lane, so it does not wait behind
    /// slow background calls; the quiet scheduled poll, the automatic pages and a search's seek (which can chain
    /// many pages) do not.
    pub(super) fn lane(self) -> Lane {
        match self {
            Self::Newest { quiet: false } | Self::Older { automatic: false } => Lane::Interactive,
            Self::Newest { quiet: true } | Self::Older { automatic: true } | Self::Seek => {
                Lane::Background
            }
        }
    }
}

/// A message to be brought into the loaded ones by fetching older pages, because a search found it further back than
/// what is loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Seek {
    pub(super) message_id: String,
    /// When the message was sent: once the oldest loaded message is older than this and the message is still not there,
    /// it is not going to be in any older page.
    pub(super) sent: DateTime<Utc>,
    /// Older pages requested for it so far.
    pub(super) pages: u8,
    /// What the status line says when it is found.
    pub(super) found: String,
}

/// Most older pages a seek asks for before giving up.
pub(super) const MAX_SEEK_PAGES: u8 = 60;

pub struct ChatState {
    msgs: Vec<Msg>,
    pub older: Option<String>,
    pub loaded: bool,
    fetch: Option<Fetch>,
    /// Older pages actually requested by the app, bounded for this conversation's lifetime.
    autofill_pages: u8,
    pub error: Option<String>,
    /// Timing, backoff and service hold-off for this conversation's requests.
    pub sched: Schedule,
    /// A new incoming or outgoing message makes polling faster; loading history does not.
    pub last_exchange: Option<Instant>,
    /// Every change to messages invalidates the transcript, including local sends and reactions.
    version: u64,
    pub sel: usize,
    /// Stay at the newest message, including when automatic history pages arrive.
    pub follow: bool,
    pub scroll: usize,
    /// Ask the next layout to scroll just enough to show the selected message.
    pub reveal: bool,
    /// Frozen when opening the chat, so the "new" divider survives read-marker updates.
    pub unread_marker: Option<DateTime<Utc>>,
    pub transcript: Transcript,
    cache: transcript::Cache,
    /// Original Markdown survives until delivery is confirmed or the text is restored to the editor.
    local_md: HashMap<String, String>,
    /// The reply quote a pending send carried, kept the same way as `local_md` so a refused send's text, restored
    /// to the editor for resending, carries its quote again too rather than silently losing it.
    local_reply: HashMap<String, ReplyTarget>,
    /// The title a pending post to a channel carried, kept so a refused post's title can be put back with its text.
    local_subject: HashMap<String, String>,
    /// Edits saved here and shown at once (`apply_edit`), by message id: the `modified` time the message had when it was
    /// edited and the text it now shows. A poll answered with the copy from before the edit (sent before the edit was
    /// made) has that same time, so it must not put the old text back; a copy with a later time is the service's own
    /// (this edit, a reaction, another device's edit) and ends the entry.
    local_edits: HashMap<String, LocalEdit>,
    /// A message being reached by fetching older pages (see `app/search.rs`).
    pub(super) seek: Option<Seek>,
}

/// An edit shown here before the service has a copy of it: the message's `modified` when it was edited, the text it
/// shows now, and when it was edited (see `ChatState::local_edits`).
type LocalEdit = (DateTime<Utc>, String, Option<DateTime<Utc>>);

impl ChatState {
    pub(super) fn new() -> Self {
        Self {
            msgs: Vec::new(),
            older: None,
            loaded: false,
            fetch: None,
            autofill_pages: 0,
            error: None,
            sched: Schedule::new(),
            last_exchange: None,
            version: 0,
            sel: 0,
            follow: true,
            scroll: 0,
            reveal: true,
            unread_marker: None,
            transcript: Transcript::default(),
            cache: transcript::Cache::new(),
            local_md: HashMap::new(),
            local_reply: HashMap::new(),
            local_subject: HashMap::new(),
            local_edits: HashMap::new(),
            seek: None,
        }
    }

    pub fn messages(&self) -> &[Msg] {
        &self.msgs
    }

    /// Changes whenever the messages do, so something derived from them can tell it is out of date.
    pub(super) fn version(&self) -> u64 {
        self.version
    }

    /// Where the message with `id` is among the loaded ones.
    pub(super) fn position_of(&self, id: &str) -> Option<usize> {
        self.msgs.iter().position(|m| m.id == id)
    }

    /// When the oldest message loaded so far was sent (a message still being sent does not count).
    pub(super) fn oldest_loaded(&self) -> Option<DateTime<Utc>> {
        self.msgs
            .iter()
            .filter(|m| !m.id.starts_with("local-"))
            .map(|m| m.created)
            .min()
    }

    /// Select the message at `index` and scroll it into view, leaving the newest-message follow mode.
    pub(super) fn select_at(&mut self, index: usize) {
        self.sel = index.min(self.msgs.len().saturating_sub(1));
        self.follow = false;
        self.reveal = true;
    }

    pub fn shows_loading(&self) -> bool {
        matches!(
            self.fetch,
            Some(Fetch::Newest { quiet: false } | Fetch::Older { automatic: false })
        )
    }

    pub(super) fn is_fetching(&self) -> bool {
        self.fetch.is_some()
    }

    /// Claim the request before dispatch, spending the automatic-page budget only if it can start.
    pub(super) fn start_fetch(&mut self, fetch: Fetch) -> bool {
        if self.is_fetching() || (fetch.is_older() && self.older.is_none()) {
            return false;
        }
        if fetch == (Fetch::Older { automatic: true }) {
            if self.autofill_pages >= AUTOFILL_MAX_PAGES {
                return false;
            }
            self.autofill_pages += 1;
        }
        self.fetch = Some(fetch);
        self.sched.started();
        true
    }

    pub(super) fn fail_fetch(&mut self, error: &Error) {
        self.fetch = None;
        self.sched.failed(throttle_hold(error));
        self.error = Some(describe_running(error));
    }

    /// Merge a page while preserving either the selected message or following the newest one.
    /// Returns whether another automatic older page would be useful.
    pub(super) fn receive_page(&mut self, fetch: Fetch, page: Page, me_id: &str) -> bool {
        self.fetch = None;
        self.error = None;
        self.sched.succeeded();
        let mut changed = true;
        match fetch {
            Fetch::Older { .. } | Fetch::Seek => {
                let automatic = fetch == (Fetch::Older { automatic: true });
                let before = self.msgs.len();
                merge_messages(&mut self.msgs, page.messages, me_id);
                let added = self.msgs.len() - before;
                self.older = page.older;
                if automatic && self.follow {
                    self.sel = self.msgs.len().saturating_sub(1);
                } else {
                    self.sel += added;
                    self.follow = false;
                    self.reveal = true;
                }
            }
            Fetch::Newest { .. } if !self.loaded => {
                // A message sent before the first page arrived (a `local-` id, its text kept in `local_md`) is the
                // only thing that can already be in `self.msgs` here; it must not be lost by the one load that does
                // not go through `merge_messages`, which is what protects it every other time.
                let pending = std::mem::take(&mut self.msgs);
                self.msgs = page.messages;
                self.older = page.older;
                merge_messages(&mut self.msgs, pending, me_id);
                self.loaded = true;
                self.sel = self.msgs.len().saturating_sub(1);
                self.follow = true;
                self.reveal = true;
            }
            Fetch::Newest { .. } => {
                let merged = merge_messages(&mut self.msgs, page.messages, me_id);
                // An unchanged poll leaves the transcript as it is (see Merged::changed).
                changed = merged.changed;
                if merged.fresh > 0 {
                    self.last_exchange = Some(Instant::now());
                }
                if self.follow {
                    self.sel = self.msgs.len().saturating_sub(1);
                    self.reveal = true;
                }
            }
        }
        // A copy from before an edit shown here must not undo it, even for one frame.
        self.keep_local_edits();
        if changed {
            self.version += 1;
        }
        let messages = self.msgs.iter().filter(|m| !m.system && !m.deleted).count();
        self.older.is_some()
            && messages < AUTOFILL_MIN_MESSAGES
            && self.autofill_pages < AUTOFILL_MAX_PAGES
    }

    /// Put locally saved edits back over copies of their messages that predate them (see `local_edits`), and forget
    /// the ones the service has caught up with. Returns whether a message was put back.
    fn keep_local_edits(&mut self) -> bool {
        let mut put_back = false;
        let msgs = &mut self.msgs;
        self.local_edits.retain(|id, (at, body, edited)| {
            let Some(m) = msgs.iter_mut().find(|m| &m.id == id) else {
                return false;
            };
            if m.modified > *at {
                return false;
            }
            if m.body != *body || m.edited != *edited {
                m.html = true;
                m.body.clone_from(body);
                m.edited = *edited;
                put_back = true;
            }
            true
        });
        put_back
    }
    pub(super) fn add_pending_message(
        &mut self,
        message: Msg,
        markdown: String,
        reply_to: Option<ReplyTarget>,
    ) {
        self.local_md.insert(message.id.clone(), markdown);
        if let Some(reply_to) = reply_to {
            self.local_reply.insert(message.id.clone(), reply_to);
        }
        self.msgs.push(message);
        self.last_exchange = Some(Instant::now());
        self.sel = self.msgs.len() - 1;
        self.follow = true;
        self.reveal = true;
        self.version += 1;
    }

    /// Remember the title a pending post carries (see `take_pending_subject`).
    pub(super) fn keep_pending_subject(&mut self, local_id: &str, subject: String) {
        self.local_subject.insert(local_id.to_string(), subject);
    }

    /// The title a pending post carried, spent: given back when the send failed, so a refused post can be restored with
    /// it (call before `fail_send`).
    pub(super) fn take_pending_subject(&mut self, local_id: &str) -> Option<String> {
        self.local_subject.remove(local_id)
    }

    pub(super) fn confirm_send(&mut self, local_id: &str, sent: Msg, me_id: &str) {
        self.local_md.remove(local_id);
        self.local_reply.remove(local_id);
        self.local_subject.remove(local_id);
        self.msgs.retain(|m| m.id != local_id);
        merge_messages(&mut self.msgs, vec![sent], me_id);
        self.version += 1;
    }

    /// Restore a refused send only when its editor is available, with the reply it was quoting (if any) so a
    /// resend carries it again. An uncertain delivery always stays in the transcript.
    pub(super) fn fail_send(
        &mut self,
        local_id: &str,
        reason: &str,
        unconfirmed: bool,
        editor_available: bool,
    ) -> Option<(String, Option<ReplyTarget>)> {
        self.version += 1;
        let reply_to = self.local_reply.remove(local_id);
        if !unconfirmed
            && editor_available
            && let Some(md) = self.local_md.remove(local_id)
        {
            self.msgs.retain(|m| m.id != local_id);
            return Some((md, reply_to));
        }
        if let Some(m) = self.msgs.iter_mut().find(|m| m.id == local_id) {
            m.delivery = Delivery::Failed(if unconfirmed {
                format!(
                    "not confirmed: {reason}. It may have been delivered; check the chat before sending again"
                )
            } else {
                reason.to_string()
            });
        }
        None
    }

    /// Show your saved edit at once; the next poll supplies the service's copy. `modified` is deliberately left
    /// alone: it is what `merge_messages` compares to decide whether an incoming copy is newer than what is shown,
    /// and the service's own timestamp for this very edit (set before it answered our request) is earlier than our
    /// local clock here. Bumping it to `at` would make every future poll's copy of this message look older than the
    /// one shown, including the "next poll" the doc above promises, so a reaction someone else adds afterwards, or a
    /// later edit from another device, would never show. `Msg::edited`, which only this changes, already redraws the
    /// message at once (`transcript::message_key` hashes it); the same rule reactions use (`apply_reaction`, which
    /// does not touch `modified` either).
    pub(super) fn apply_edit(
        &mut self,
        message_id: &str,
        html: &str,
        at: chrono::DateTime<chrono::Utc>,
    ) {
        if let Some(m) = self.msgs.iter_mut().find(|m| m.id == message_id) {
            m.html = true;
            m.body = html.to_string();
            m.edited = Some(at);
            self.version += 1;
            self.local_edits.insert(
                message_id.to_string(),
                (m.modified, m.body.clone(), m.edited),
            );
        }
    }

    /// Apply or withdraw an optimistic reaction; the next poll supplies the service's truth.
    pub(super) fn apply_reaction(&mut self, message_id: &str, emoji: &str, on: bool, me: &str) {
        if me.is_empty() {
            return;
        }
        let Some(m) = self.msgs.iter_mut().find(|m| m.id == message_id) else {
            return;
        };
        let at = m.reactions.iter().position(|r| same_emoji(&r.emoji, emoji));
        match (at, on) {
            (Some(i), true) => {
                let r = &mut m.reactions[i];
                if !r.users.iter().any(|u| u.eq_ignore_ascii_case(me)) {
                    r.users.push(me.to_string());
                    r.count += 1;
                }
            }
            (None, true) => m.reactions.push(Reaction {
                emoji: emoji.to_string(),
                count: 1,
                users: vec![me.to_string()],
                names: Vec::new(),
            }),
            (Some(i), false) => {
                let r = &mut m.reactions[i];
                if let Some(pos) = r.users.iter().position(|u| u.eq_ignore_ascii_case(me)) {
                    r.users.remove(pos);
                    r.count = r.count.saturating_sub(1);
                }
                if r.count == 0 {
                    m.reactions.remove(i);
                }
            }
            (None, false) => {}
        }
        self.version += 1;
    }

    /// Rebuild from the owned message version, then clamp scrolling to the resulting rows.
    pub(super) fn layout(
        &mut self,
        width: usize,
        height: usize,
        spinner: usize,
        ctx: &Ctx<'_>,
        me: &str,
        people: &transcript::People<'_>,
    ) {
        let key = TranscriptKey {
            unlisted: people.unlisted.clone(),
            width,
            version: self.version,
            images: ctx.images.version,
            mode: ctx.mode,
            spinner,
            day: chrono::Local::now().date_naive(),
            unread_marker: self.unread_marker,
        };
        if self.transcript.key.as_ref() != Some(&key) {
            self.transcript = transcript::build(
                &self.msgs,
                me,
                self.unread_marker,
                key,
                ctx,
                &mut self.cache,
                people.names,
            );
        }
        let max_scroll = self.transcript.lines.len().saturating_sub(height);
        if self.follow {
            self.scroll = max_scroll;
        } else if self.reveal
            && let (Some(&s), Some(&e)) = (
                self.transcript.starts.get(self.sel),
                self.transcript.ends.get(self.sel),
            )
        {
            if s < self.scroll || e + 1 - s > height {
                self.scroll = s.saturating_sub(1);
            } else if e >= self.scroll + height {
                self.scroll = e + 1 - height;
            }
        }
        self.reveal = false;
        self.scroll = self.scroll.min(max_scroll);
        if !self.follow && self.scroll == max_scroll && self.sel + 1 >= self.msgs.len() {
            self.follow = true;
        }
    }

    #[cfg(test)]
    pub(super) fn edit_messages(&mut self, edit: impl FnOnce(&mut Vec<Msg>)) {
        edit(&mut self.msgs);
        self.version += 1;
    }

    #[cfg(test)]
    pub(super) fn automatic_pages(&self) -> u8 {
        self.autofill_pages
    }
}

/// What [`merge_messages`] did to the list.
pub(super) struct Merged {
    /// How many messages from someone else are new.
    pub(super) fresh: usize,
    /// Whether anything a transcript shows is different now: a message was added, or one was replaced by a copy that
    /// differs from it. A poll that returns what is already held changes nothing, so the transcript need not be rebuilt.
    pub(super) changed: bool,
}

/// Insert or update by id, retaining newer edits and chronological order with local messages last.
pub(super) fn merge_messages(existing: &mut Vec<Msg>, incoming: Vec<Msg>, me_id: &str) -> Merged {
    let mut merged = Merged {
        fresh: 0,
        changed: false,
    };
    for m in incoming {
        match existing.iter_mut().find(|e| e.id == m.id) {
            Some(e) => {
                if m.modified >= e.modified && *e != m {
                    *e = m;
                    merged.changed = true;
                }
            }
            None => {
                if m.from_id.as_deref() != Some(me_id) {
                    merged.fresh += 1;
                }
                existing.push(m);
                merged.changed = true;
            }
        }
    }
    existing.sort_by_key(|m| (m.id.starts_with("local-"), m.created));
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message with fixed times, so two copies built apart are equal.
    fn fixed(id: &str, secs: i64, body: &str) -> Msg {
        use chrono::TimeZone;
        let t = chrono::Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap();
        Msg {
            root_id: None,
            id: id.into(),
            created: t,
            modified: t,
            edited: None,
            from_id: Some("someone".into()),
            from_name: "Someone".into(),
            system: false,
            deleted: false,
            html: true,
            body: body.into(),
            subject: None,
            attachments: vec![],
            reactions: vec![],
            mentions: vec![],
            delivery: teams_client::Delivery::Sent,
        }
    }

    fn page_of(messages: Vec<Msg>) -> Page {
        Page {
            messages,
            older: None,
        }
    }

    /// A chat that has loaded `messages` and is idle.
    fn loaded_with(messages: Vec<Msg>) -> ChatState {
        let mut chat = ChatState::new();
        assert!(chat.start_fetch(Fetch::Newest { quiet: false }));
        chat.receive_page(Fetch::Newest { quiet: false }, page_of(messages), "me");
        chat
    }

    fn poll(chat: &mut ChatState, messages: Vec<Msg>) {
        assert!(chat.start_fetch(Fetch::Newest { quiet: true }));
        chat.receive_page(Fetch::Newest { quiet: true }, page_of(messages), "me");
    }

    #[test]
    fn a_poll_that_returns_what_is_already_held_leaves_the_transcript_version_alone() {
        let held = vec![fixed("1", 1, "a"), fixed("2", 2, "b")];
        let mut chat = loaded_with(held.clone());
        let before = chat.version();
        poll(&mut chat, held);
        assert_eq!(
            chat.version(),
            before,
            "nothing changed, so nothing is rebuilt"
        );
    }

    #[test]
    fn a_poll_that_brings_a_new_edited_or_reacted_message_changes_the_version() {
        let held = vec![fixed("1", 1, "a"), fixed("2", 2, "b")];

        // A new message.
        let mut chat = loaded_with(held.clone());
        let before = chat.version();
        let mut more = held.clone();
        more.push(fixed("3", 3, "c"));
        poll(&mut chat, more);
        assert!(chat.version() > before, "a new message");

        // An edit: newer `modified` and different text.
        let mut chat = loaded_with(held.clone());
        let before = chat.version();
        let mut edited = held.clone();
        edited[1].body = "b, edited".into();
        edited[1].modified += chrono::Duration::seconds(30);
        poll(&mut chat, edited);
        assert!(chat.version() > before, "an edit");
        assert_eq!(chat.messages()[1].body, "b, edited");

        // A reaction on a message whose `modified` moved on.
        let mut chat = loaded_with(held.clone());
        let before = chat.version();
        let mut reacted = held.clone();
        reacted[0].reactions.push(teams_client::Reaction {
            emoji: "👍".into(),
            count: 1,
            users: vec![],
            names: vec![],
        });
        reacted[0].modified += chrono::Duration::seconds(5);
        poll(&mut chat, reacted);
        assert!(chat.version() > before, "a reaction");

        // The same `modified` but a different copy (names resolved later) is still a change.
        let mut chat = loaded_with(held.clone());
        let before = chat.version();
        let mut renamed = held.clone();
        renamed[0].from_name = "Someone Else".into();
        poll(&mut chat, renamed);
        assert!(chat.version() > before, "a different copy");
        assert_eq!(chat.messages()[0].from_name, "Someone Else");
    }

    #[test]
    fn a_poll_answered_with_the_copy_from_before_an_edit_does_not_undo_it() {
        let held = vec![fixed("1", 1, "<p>before</p>")];
        let mut chat = loaded_with(held.clone());
        chat.apply_edit("1", "<p>after</p>", chrono::Utc::now());
        // A poll sent before the edit was saved comes back with the old copy (same `modified`, old text).
        poll(&mut chat, held.clone());
        assert_eq!(
            chat.messages()[0].body,
            "<p>after</p>",
            "the old copy did not put the old text back"
        );
        assert!(chat.messages()[0].edited.is_some());
        // The service's own copy of the edit (a later `modified`) replaces ours and ends the override...
        let mut served = held[0].clone();
        served.body = "<p>after, as the service has it</p>".into();
        served.modified += chrono::Duration::seconds(3);
        served.edited = Some(served.modified);
        poll(&mut chat, vec![served.clone()]);
        assert_eq!(
            chat.messages()[0].body,
            "<p>after, as the service has it</p>"
        );
        // ...so a later change from elsewhere still shows, even one that goes back to old words.
        let mut later = served.clone();
        later.body = "<p>before</p>".into();
        later.modified += chrono::Duration::seconds(10);
        poll(&mut chat, vec![later]);
        assert_eq!(chat.messages()[0].body, "<p>before</p>");
    }

    #[test]
    fn a_reaction_from_the_service_after_an_edit_is_still_merged() {
        let held = vec![fixed("1", 1, "<p>before</p>")];
        let mut chat = loaded_with(held.clone());
        chat.apply_edit("1", "<p>after</p>", chrono::Utc::now());
        let mut reacted = held[0].clone();
        reacted.body = "<p>after</p>".into();
        reacted.modified += chrono::Duration::seconds(2);
        reacted.reactions.push(teams_client::Reaction {
            emoji: "👍".into(),
            count: 1,
            users: vec!["someone".into()],
            names: vec![String::new()],
        });
        poll(&mut chat, vec![reacted]);
        assert_eq!(chat.messages()[0].reactions.len(), 1);
        assert_eq!(chat.messages()[0].body, "<p>after</p>");
    }
    #[test]
    fn a_new_read_position_moves_the_new_divider_without_any_other_change() {
        let store = crate::render::ImageStore::default();
        let ctx = Ctx {
            images: &store,
            mode: crate::imgart::ImgMode::DEFAULT,
            spinner: "⠋",
            fetchable: &|_| false,
            keymap: crate::editor::Keymap::default(),
        };
        let mut chat = loaded_with(vec![
            fixed("1", 1, "<p>one</p>"),
            fixed("2", 2, "<p>two</p>"),
        ]);
        let has_divider = |chat: &ChatState| {
            chat.transcript
                .lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.content.contains("new")))
        };
        chat.layout(
            80,
            20,
            0,
            &ctx,
            "me",
            &transcript::People {
                names: &HashMap::new(),
                unlisted: &Default::default(),
            },
        );
        assert!(!has_divider(&chat));
        // The read position moves (the chat was opened while unread), and nothing else changes.
        chat.unread_marker = Some(chat.messages()[0].created);
        chat.layout(
            80,
            20,
            0,
            &ctx,
            "me",
            &transcript::People {
                names: &HashMap::new(),
                unlisted: &Default::default(),
            },
        );
        assert!(
            has_divider(&chat),
            "the divider is drawn without waiting for another change"
        );
        chat.unread_marker = None;
        chat.layout(
            80,
            20,
            0,
            &ctx,
            "me",
            &transcript::People {
                names: &HashMap::new(),
                unlisted: &Default::default(),
            },
        );
        assert!(!has_divider(&chat), "and taken away again");
    }
    #[test]
    fn an_older_page_still_changes_the_version() {
        let mut chat = loaded_with(vec![fixed("5", 5, "e")]);
        chat.older = Some("next".into());
        let before = chat.version();
        assert!(chat.start_fetch(Fetch::Older { automatic: false }));
        chat.receive_page(
            Fetch::Older { automatic: false },
            page_of(vec![fixed("4", 4, "d")]),
            "me",
        );
        assert!(chat.version() > before);
    }
    fn failure() -> Error {
        Error::Unexpected {
            operation: "test fetch",
            detail: "failed",
        }
    }

    #[test]
    fn automatic_paging_spends_its_budget_only_when_a_request_starts() {
        let mut chat = ChatState::new();
        let automatic = Fetch::Older { automatic: true };
        assert!(!chat.start_fetch(automatic), "there is no older page yet");
        assert_eq!(chat.automatic_pages(), 0);
        chat.older = Some("next".into());
        assert!(chat.start_fetch(Fetch::Older { automatic: false }));
        assert!(
            !chat.start_fetch(automatic),
            "another request owns the slot"
        );
        assert_eq!(chat.automatic_pages(), 0);
        chat.fail_fetch(&failure());

        for count in 1..=AUTOFILL_MAX_PAGES {
            assert!(chat.start_fetch(automatic));
            assert_eq!(chat.automatic_pages(), count);
            chat.fail_fetch(&failure());
            assert!(!chat.is_fetching(), "a failure releases the request slot");
        }
        assert!(
            !chat.start_fetch(automatic),
            "automatic requests have a hard limit"
        );
        assert!(
            chat.start_fetch(Fetch::Older { automatic: false }),
            "the user can still ask for history"
        );
    }

    #[test]
    fn an_automatic_older_page_does_not_show_the_loading_spinner() {
        let mut chat = ChatState::new();
        chat.older = Some("next".into());
        assert!(chat.start_fetch(Fetch::Older { automatic: true }));
        assert!(
            !chat.shows_loading(),
            "a background autofill page nobody asked for must stay quiet, like a quiet poll"
        );
        chat.fail_fetch(&failure());
        chat.older = Some("next".into());
        assert!(chat.start_fetch(Fetch::Older { automatic: false }));
        assert!(
            chat.shows_loading(),
            "a page the user scrolled to ask for still shows that it is loading"
        );
    }

    fn msg(id: &str, mins: i64) -> Msg {
        let t = Utc::now() - chrono::Duration::minutes(mins);
        Msg {
            root_id: None,
            id: id.into(),
            created: t,
            modified: t,
            edited: None,
            from_id: Some("ada".into()),
            from_name: "Ada".into(),
            system: false,
            deleted: false,
            html: true,
            body: id.into(),
            subject: None,
            attachments: vec![],
            reactions: vec![],
            mentions: vec![],
            delivery: Delivery::Sent,
        }
    }

    #[test]
    fn a_message_sent_before_the_first_page_arrives_is_not_lost_by_it() {
        let mut chat = ChatState::new();
        let mut pending = msg("local-1", 0);
        pending.from_id = Some("me".into());
        chat.add_pending_message(pending, "hi".into(), None);
        assert_eq!(
            chat.messages().len(),
            1,
            "nothing loaded yet but the send shows"
        );

        let server_msg = msg("m1", 5);
        assert!(!chat.loaded);
        chat.receive_page(
            Fetch::Newest { quiet: false },
            Page {
                messages: vec![server_msg],
                older: None,
            },
            "me",
        );
        assert!(chat.loaded);
        let ids: Vec<&str> = chat.messages().iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["m1", "local-1"],
            "the pending send survives the first load and stays last"
        );
    }

    fn reply_target() -> ReplyTarget {
        ReplyTarget {
            id: "m1".into(),
            from_id: Some("ada".into()),
            from_name: "Ada".into(),
            preview: "see you then".into(),
        }
    }

    #[test]
    fn a_refused_sends_reply_target_comes_back_for_resending() {
        let mut chat = ChatState::new();
        let mut pending = msg("local-1", 0);
        pending.from_id = Some("me".into());
        chat.add_pending_message(pending, "hi".into(), Some(reply_target()));
        let (md, reply_to) = chat
            .fail_send("local-1", "refused", false, true)
            .expect("restored for an easy resend");
        assert_eq!(md, "hi");
        assert_eq!(reply_to.map(|r| r.from_name), Some("Ada".to_string()));
        assert!(chat.messages().is_empty(), "the pending message is gone");
    }

    #[test]
    fn an_unconfirmed_sends_reply_target_is_not_returned() {
        let mut chat = ChatState::new();
        let mut pending = msg("local-1", 0);
        pending.from_id = Some("me".into());
        chat.add_pending_message(pending, "hi".into(), Some(reply_target()));
        assert!(
            chat.fail_send("local-1", "dropped", true, true).is_none(),
            "an uncertain delivery stays in the transcript, not offered for an easy resend"
        );
        assert_eq!(chat.messages().len(), 1);
    }

    #[test]
    fn an_edit_does_not_block_a_later_service_copy_from_being_merged() {
        let mut chat = ChatState::new();
        let original = msg("m1", 5);
        chat.receive_page(
            Fetch::Newest { quiet: false },
            Page {
                messages: vec![original.clone()],
                older: None,
            },
            "me",
        );
        let confirmed_at = Utc::now();
        chat.apply_edit("m1", "<p>edited</p>", confirmed_at);
        assert!(chat.messages()[0].body.contains("edited"));
        assert_eq!(chat.messages()[0].edited, Some(confirmed_at));

        // The next poll's copy carries the service's own edit timestamp, stamped before it answered our request, so
        // it is a little earlier than `confirmed_at`; it also carries a reaction someone else added in the meantime.
        // `modified` must not have been bumped to `confirmed_at`, or this copy would look older than what is shown
        // and never be merged.
        let mut server_copy = original;
        server_copy.html = true;
        server_copy.body = "<p>edited</p>".into();
        server_copy.modified = confirmed_at - chrono::Duration::milliseconds(500);
        server_copy.reactions = vec![Reaction {
            emoji: "👍".into(),
            count: 1,
            users: vec!["ada".into()],
            names: vec!["Ada".into()],
        }];
        chat.receive_page(
            Fetch::Newest { quiet: true },
            Page {
                messages: vec![server_copy],
                older: None,
            },
            "me",
        );
        assert_eq!(
            chat.messages()[0].reactions.len(),
            1,
            "the service's copy, with its reaction, was accepted rather than rejected as stale"
        );
    }

    #[test]
    fn quiet_refresh_and_visible_history_share_one_request_slot() {
        let mut chat = ChatState::new();
        let quiet = Fetch::Newest { quiet: true };
        assert!(chat.start_fetch(quiet));
        assert!(chat.is_fetching());
        assert!(!chat.shows_loading());
        chat.receive_page(
            quiet,
            Page {
                messages: vec![],
                older: Some("next".into()),
            },
            "me",
        );
        assert!(!chat.is_fetching());

        let history = Fetch::Older { automatic: false };
        assert!(chat.start_fetch(history));
        assert!(chat.shows_loading());
        assert!(!chat.start_fetch(quiet));
        chat.fail_fetch(&failure());
        assert!(!chat.shows_loading());
        assert!(chat.start_fetch(Fetch::Newest { quiet: false }));
        assert!(chat.shows_loading());
    }
}
