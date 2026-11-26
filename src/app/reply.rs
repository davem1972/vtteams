//! Replying to a message: `r` / `Ctrl-R` marks which message the next one sent will quote. vtteams never builds
//! the quote text itself — it only hands the backend enough about the quoted message (see `AGENTS.md`, "Messages, mentions and replies") for the backend
//! to format its own wire markup with it, the same way `OutMention` lets a
//! backend format its own mention markup. In a channel a reply is a message in a post's thread instead: the target is the
//! post (`ReplyTarget::id` is the post's id, never a reply's), and nothing is quoted.

use super::{App, Focus};
use crate::editor::Mode;
use teams_client::{Delivery, Msg, ReplyTarget};

/// How much of the quoted message to summarise (Teams' own preview is one short line too).
const PREVIEW_MAX_CHARS: usize = 120;

/// A pending reply: the next message sent will quote `target`. Cleared whenever the open chat changes (see the
/// `cancel_reply` call sites beside every `cancel_edit` one), so by the time a message is actually sent it is
/// always for the chat it was started in, with nothing further to check here.
pub(super) struct Replying {
    pub(super) target: ReplyTarget,
}

fn preview(m: &Msg) -> String {
    let plain = teams_client::preview_text(&m.body, m.html);
    if plain.chars().count() <= PREVIEW_MAX_CHARS {
        return plain;
    }
    format!(
        "{}…",
        plain.chars().take(PREVIEW_MAX_CHARS).collect::<String>()
    )
}

impl App {
    /// Whether the next message sent will carry a reply quote.
    pub fn is_replying(&self) -> bool {
        self.replying.is_some()
    }

    /// The id of the message being replied to, if a reply is being written.
    pub fn reply_target_id(&self) -> Option<&str> {
        self.replying.as_ref().map(|r| r.target.id.as_str())
    }

    /// Stop replying without sending.
    pub(super) fn cancel_reply(&mut self) {
        self.replying = None;
    }

    /// Reply to the selected message. Nothing is pre-filled in the message box: the quote itself is built by the
    /// backend once the reply is actually sent.
    pub(super) fn reply_to_selected(&mut self) {
        if self.current_channel().is_some() {
            self.reply_in_thread();
            return;
        }
        if self.editing.is_some() {
            self.set_status("already editing: Enter saves, Esc cancels", true);
            return;
        }
        let Some(m) = self.selected_msg().filter(|m| !m.system) else {
            self.set_status("nothing to reply to", true);
            return;
        };
        // The quote needs the message's own, server-assigned id (`ReplyTarget::id`), which a message still being
        // sent does not have yet.
        if !matches!(m.delivery, Delivery::Sent) || m.id.starts_with("local-") {
            self.set_status("that message has not been sent yet", true);
            return;
        }
        // The name goes into the quote everyone in the chat reads, so it is the sender's real name: for your own
        // message that is yours, not the "You" the transcript shows only to you.
        let mine = m.from_id.as_deref() == Some(self.me_id());
        let from_name = match self
            .me
            .as_ref()
            .filter(|me| mine && !me.name.trim().is_empty())
        {
            Some(me) => me.name.clone(),
            None => m.from_name.clone(),
        };
        self.replying = Some(Replying {
            target: ReplyTarget {
                id: m.id.clone(),
                from_id: m.from_id.clone(),
                from_name,
                preview: preview(m),
            },
        });
        self.editor.set_mode(Mode::Insert);
        self.focus = Focus::Editor;
        self.set_status("replying: Enter sends, Esc cancels", false);
    }

    /// Reply in the thread of the selected message of a channel: the target is the post the thread belongs to (the
    /// selected message itself when it is a post, the post a reply names otherwise), and nothing is quoted. Nothing is
    /// pre-filled in the message box.
    fn reply_in_thread(&mut self) {
        if !self.backend.can_post_to_channels() {
            self.set_status("replying in channels is not available yet", true);
            return;
        }
        if self.editing.is_some() {
            self.set_status("already editing: Enter saves, Esc cancels", true);
            return;
        }
        let Some(m) = self.selected_msg().filter(|m| !m.system).cloned() else {
            self.set_status("nothing to reply to", true);
            return;
        };
        if m.deleted {
            self.set_status("that message was deleted", true);
            return;
        }
        if !matches!(m.delivery, Delivery::Sent) || m.id.starts_with("local-") {
            self.set_status("that message has not been sent yet", true);
            return;
        }
        if self.subject.is_some() {
            self.set_status(
                "a reply has no subject: :subject - clears it, then reply",
                true,
            );
            return;
        }
        let root_id = m.root_id.clone().unwrap_or_else(|| m.id.clone());
        // What the box names: the post's title, else who wrote it, as far as the post is loaded.
        let post = self
            .state()
            .and_then(|st| st.messages().iter().find(|p| p.id == root_id))
            .cloned();
        let (from_id, from_name, preview) = match &post {
            Some(p) => (
                p.from_id.clone(),
                p.from_name.clone(),
                p.subject.clone().unwrap_or_default(),
            ),
            None => (None, String::new(), String::new()),
        };
        self.replying = Some(Replying {
            target: ReplyTarget {
                id: root_id,
                from_id,
                from_name,
                preview,
            },
        });
        self.editor.set_mode(Mode::Insert);
        self.focus = Focus::Editor;
        self.set_status("replying in the thread: Enter posts, Esc cancels", false);
    }

    /// What the box says it is replying to in a channel: the post's title, else its sender, else nothing (a post that
    /// is not loaded).
    pub fn thread_label(&self) -> Option<String> {
        let target = &self.replying.as_ref()?.target;
        self.current_channel()?;
        let named = if target.preview.is_empty() {
            &target.from_name
        } else {
            &target.preview
        };
        Some(named.clone()).filter(|n| !n.is_empty())
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn msg(body: &str) -> Msg {
        Msg {
            root_id: None,
            id: "1".into(),
            created: chrono::Utc::now(),
            modified: chrono::Utc::now(),
            edited: None,
            from_id: Some("ada".into()),
            from_name: "Ada".into(),
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

    #[test]
    fn a_plain_text_message_is_previewed_as_written() {
        let mut m = msg("if a < b and c > d");
        m.html = false;
        assert_eq!(preview(&m), "if a < b and c > d");
    }

    #[test]
    fn replying_to_a_reply_previews_what_it_said_not_what_it_quoted() {
        let m = msg(
            r#"<blockquote itemtype="http://schema.skype.com/Reply"><strong>Bo</strong><p>old words</p></blockquote><p>new words</p>"#,
        );
        assert_eq!(preview(&m), "new words");
    }

    #[test]
    fn the_preview_is_one_line_and_capped() {
        assert_eq!(preview(&msg("<p>hi there</p>")), "hi there");
        let long = "x".repeat(PREVIEW_MAX_CHARS + 50);
        let p = preview(&msg(&format!("<p>{long}</p>")));
        assert_eq!(
            p.chars().count(),
            PREVIEW_MAX_CHARS + 1,
            "capped, plus the ellipsis"
        );
        assert!(p.ends_with('…'));
    }
}
