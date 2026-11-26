//! Editing a message you sent. The message goes into the message box, Enter saves it, Esc gives up, and whatever was
//! being typed before comes back either way. Anyone's edited messages are marked in the transcript (`Msg::edited`).

use super::{App, Ev, Focus, describe_running};
use crate::convert;
use crate::editor::Mode;
use crate::inline_style;
use chrono::Utc;
use teams_client::{Delivery, Me, Msg};

/// Formatting vtteams cannot put in the message box and so would lose on saving.
const UNEDITABLE_MARKUP: [&str; 7] = ["<u>", "<s>", "<strike", "<mark", "style=", "<img", "<table"];

/// A message being edited.
pub(super) struct Editing {
    chat_id: String,
    message_id: String,
    /// The text as it was put in the box, to tell whether anything changed.
    original: String,
    /// What was in the box before, put back afterwards.
    draft: String,
    /// The edit has been sent and its outcome is not known yet.
    saving: bool,
    /// The message is a post in a channel, which is edited by its own call.
    in_channel: bool,
    /// The title of the post being edited, restated by the edit because the service drops a title an edit does not
    /// repeat.
    subject: Option<String>,
}

/// The Markdown to edit for `m`: what the author would have typed to make it.
fn editable_text(m: &Msg, me_id: &str) -> String {
    if !m.html {
        return m.body.clone();
    }
    let md = inline_style::strip(&convert::html_to_markdown(&m.body, me_id));
    unbold_mentions(&md)
}

/// A mention reads `**@Name**` in the transcript; typed again it is just `@Name`.
fn unbold_mentions(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut rest = md;
    while let Some(start) = rest.find("**@") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("**") {
            Some(end) if end <= 80 && !after[..end].contains('\n') => {
                out.push_str(&after[..end]);
                rest = &after[end + 2..];
            }
            _ => {
                out.push_str("**");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

impl App {
    /// Whether a message is being edited (the message box then holds its text).
    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    #[cfg(test)]
    pub(super) fn why_not_editable_for_test(&self, m: &Msg) -> Option<&'static str> {
        self.why_not_editable(m)
    }

    /// Why `m` cannot be edited, if it cannot.
    fn why_not_editable(&self, m: &Msg) -> Option<&'static str> {
        let in_channel = self.current_channel().is_some();
        let can = if in_channel {
            self.backend.can_edit_posts()
        } else {
            self.backend.can_edit()
        };
        if !can {
            Some("this service cannot edit messages from here")
        } else if m.system {
            Some("that is an event, not a message")
        } else if m.deleted {
            Some("that message was deleted")
        } else if m.from_id.as_deref() != Some(self.me_id()) {
            Some("you can only edit your own messages")
        } else if in_channel && m.root_id.is_some() {
            // Which address a reply is edited at, and what it needs repeated, was not tried against the service.
            Some("editing a reply in a channel is not supported yet")
        } else if !matches!(m.delivery, Delivery::Sent) || m.id.starts_with("local-") {
            Some("that message has not been sent yet")
        } else {
            None
        }
    }

    /// Whether the post being edited has a title the edit keeps (the box says so).
    pub fn is_keeping_subject(&self) -> bool {
        self.editing.as_ref().is_some_and(|e| e.subject.is_some())
    }

    /// A service that cannot edit posts in channels: said, with nothing changed (the transcript's selection included).
    fn refuse_edit_in_channel(&mut self) -> bool {
        let refused = self.current_channel().is_some() && !self.backend.can_edit_posts();
        if refused {
            self.set_status("editing in channels is not available yet", true);
        }
        refused
    }

    /// Edit the selected message.
    pub(super) fn edit_selected(&mut self) {
        if self.refuse_edit_in_channel() {
            return;
        }
        let Some(m) = self.selected_msg().cloned() else {
            self.set_status("select one of your messages to edit", true);
            return;
        };
        self.begin_edit(&m);
    }

    /// Edit the last message you sent in this chat (Up in an empty message box, as in Teams and Slack).
    pub(super) fn edit_last_own(&mut self) {
        if self.refuse_edit_in_channel() {
            return;
        }
        // Checked here too, and before touching the selection: `begin_edit` refuses the same way, but only after
        // this has already jumped the transcript to a different message, a visible side effect from a no-op action.
        if self.editing.is_some() {
            self.set_status("already editing: Enter saves, Esc cancels", true);
            return;
        }
        let me = self.me_id().to_string();
        let Some(st) = self.state_mut() else { return };
        let last = st.messages().iter().rposition(|m| {
            m.from_id.as_deref() == Some(me.as_str())
                && !m.system
                && !m.deleted
                && matches!(m.delivery, Delivery::Sent)
                && !m.id.starts_with("local-")
        });
        let Some(i) = last else {
            self.set_status("you have not sent a message here to edit", true);
            return;
        };
        st.sel = i;
        st.reveal = true;
        let m = st.messages()[i].clone();
        self.begin_edit(&m);
    }

    fn begin_edit(&mut self, m: &Msg) {
        if self.refuse_edit_in_channel() {
            return;
        }
        if self.editing.is_some() {
            self.set_status("already editing: Enter saves, Esc cancels", true);
            return;
        }
        if let Some(why) = self.why_not_editable(m) {
            self.set_status(why, true);
            return;
        }
        let Some(chat_id) = self.current.clone() else {
            return;
        };
        let in_channel = self.current_channel().is_some();
        // Editing and replying both use the message box; starting one while the other is only pending (not yet
        // sent) must not leave a stale reply target attached to text that is no longer a reply.
        self.cancel_reply();
        let text = editable_text(m, self.me_id());
        let lossy = m.html
            && UNEDITABLE_MARKUP
                .iter()
                .any(|t| m.body.to_ascii_lowercase().contains(t));
        self.editing = Some(Editing {
            chat_id,
            message_id: m.id.clone(),
            original: text.clone(),
            draft: self.editor.text(),
            saving: false,
            in_channel,
            subject: m.subject.clone().filter(|_| in_channel),
        });
        self.editor.set_text(&text);
        self.editor.set_mode(Mode::Insert);
        self.focus = Focus::Editor;
        // Private-use characters (icon-font glyphs) are never shown, and so never edited: `inline_style::strip` takes
        // the whole range out of what comes from the network, and saving writes the message back without them.
        let icons = m.body.chars().any(inline_style::is_marker);
        let mut lost = Vec::new();
        if lossy {
            lost.push("underline, colour and highlight");
        }
        let kept = if self.is_keeping_subject() {
            " The subject is kept."
        } else {
            ""
        };
        if icons {
            lost.push("private-use characters such as icon-font glyphs");
        }
        let message = if lost.is_empty() {
            format!("editing: Enter saves, Esc cancels.{kept}")
        } else {
            format!(
                "editing: Enter saves, Esc cancels. {} cannot be edited here and are lost on saving.{kept}",
                lost.join(" and ")
            )
        };
        self.set_status(message, !lost.is_empty());
    }

    /// Put the draft back and stop editing. `to_messages` also moves to the conversation.
    pub(super) fn cancel_edit(&mut self, to_messages: bool) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        if editing.draft.is_empty() {
            self.editor.clear();
        } else {
            self.editor.set_text(&editing.draft);
        }
        if to_messages {
            self.focus = Focus::Messages;
        }
    }

    /// Enter in the message box while editing: send the new text.
    pub(super) fn finish_edit(&mut self) {
        let Some(editing) = self.editing.as_mut() else {
            return;
        };
        if editing.saving {
            self.set_status("still saving the edit", false);
            return;
        }
        let md = self.editor.text();
        if md.trim().is_empty() {
            self.set_status(
                "an edited message cannot be empty (deleting is not supported yet)",
                true,
            );
            return;
        }
        if md.trim() == editing.original.trim() {
            self.cancel_edit(true);
            self.set_status("no change", false);
            return;
        }
        editing.saving = true;
        let (chat_id, message_id) = (editing.chat_id.clone(), editing.message_id.clone());
        let (in_channel, subject) = (editing.in_channel, editing.subject.clone());
        self.edit_seq += 1;
        let token = self.edit_seq;
        self.edit_tokens.insert(message_id.clone(), token);
        let me = self.me.clone().unwrap_or(Me {
            id: String::new(),
            name: "You".into(),
        });
        // The people an `@name` can name: the chat's attendees, or the people who have posted in the channel.
        let members = self.mention_candidates().into_owned();
        let (html, mentions, ambiguous) = convert::markdown_to_html_noting(
            &md,
            &members,
            &me.id,
            self.backend.code_block_style(),
        );
        self.say_ambiguous_mentions(&ambiguous);
        let sent_html = html.clone();
        let (cid, mid) = (chat_id.clone(), message_id.clone());
        self.spawn_action(
            move |b| {
                if in_channel {
                    b.edit_post(&cid, &mid, subject.as_deref(), &html, &mentions)
                } else {
                    b.edit(&cid, &mid, &html, &mentions)
                }
            },
            move |result| Ev::Edited {
                chat_id,
                message_id,
                html: sent_html,
                token,
                result,
            },
        );
    }

    /// The outcome of an edit.
    pub(super) fn on_edited(
        &mut self,
        chat_id: String,
        message_id: String,
        html: String,
        token: u64,
        result: Result<(), teams_client::Error>,
    ) {
        // Cancelling an edit and then redoing it dispatches a second attempt while the first may still be in flight;
        // with no ordering guarantee across worker threads, the first's confirmation can arrive after the second's
        // and must not be allowed to overwrite it, or clear state that now belongs to the newer attempt.
        if self.edit_tokens.get(&message_id) != Some(&token) {
            log::debug!(
                "a stale edit confirmation was ignored: a later edit of the same message was already sent"
            );
            return;
        }
        let ours = self
            .editing
            .as_ref()
            .is_some_and(|e| e.chat_id == chat_id && e.message_id == message_id);
        match result {
            Ok(()) => {
                if let Some(st) = self.states.get_mut(&chat_id) {
                    st.apply_edit(&message_id, &html, Utc::now());
                    // The service's own copy replaces ours on the next poll.
                    st.sched.refresh_soon();
                }
                if ours {
                    self.cancel_edit(true);
                }
                self.set_status("message edited", false);
            }
            Err(e) => {
                if ours && let Some(editing) = self.editing.as_mut() {
                    editing.saving = false;
                }
                let what = if e.delivery_unknown() {
                    "edit not confirmed (it may have been saved; check the message)"
                } else {
                    "edit failed"
                };
                self.set_status(format!("{what}: {}", describe_running(&e)), true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mention_is_typed_again_without_its_bold() {
        assert_eq!(
            unbold_mentions("hi **@Ada Lovelace**, see"),
            "hi @Ada Lovelace, see"
        );
        assert_eq!(
            unbold_mentions("**bold** and **@A** and **@B**"),
            "**bold** and @A and @B"
        );
        assert_eq!(unbold_mentions("**@unclosed"), "**@unclosed");
        assert_eq!(unbold_mentions("no mention"), "no mention");
        let long = format!("**@{}** after", "x".repeat(200));
        assert!(
            unbold_mentions(&long).starts_with("**@x"),
            "a stray ** is left alone"
        );
    }
}
