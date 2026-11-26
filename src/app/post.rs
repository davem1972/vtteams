//! Posting to a channel: the question before the first post to each channel in a session, and the title (`:subject`)
//! of the next post. The sending itself is `App::send`, which a channel shares with a chat; a reply in a thread is
//! `app/reply.rs`, and editing a post is `app/edit.rs`.
//!
//! Everything written to a channel is read by everyone in it, which a person sending to a chat they picked from a list
//! of a few people does not have to think about, so the first send to a channel asks once (like F6 on a sign-in that
//! is fine does): the answer is the next key being a send again, at least [`POST_CONFIRM_GAP`] after the question was
//! shown and within [`POST_CONFIRM_WINDOW`] of it (a held-down key repeating is not an answer, and keeps the question
//! asked afresh); any other key, a click, a paste, or waiting, drops the question and the next send asks again. Once a post or reply has gone to a channel, it is not
//! asked about again for that channel until vtteams is started again.

use super::App;
use std::time::{Duration, Instant};
use teams_client::clean;

/// How long after the question a second send counts as the answer to it.
pub(super) const POST_CONFIRM_WINDOW: Duration = Duration::from_secs(10);

/// How long after the question was shown a send is still too early to be its answer: a key held down, or one that
/// repeats, sends again at once, and it is the person's reading of the question that the answer has to follow.
pub(super) const POST_CONFIRM_GAP: Duration = Duration::from_millis(400);

/// The longest title the box accepts (the library refuses a longer one too).
pub(super) const MAX_SUBJECT_CHARS: usize = 200;

impl App {
    /// The title the next post to the open channel will have, if one was set.
    pub fn pending_subject(&self) -> Option<&str> {
        self.subject.as_deref()
    }

    /// Whether the open conversation is a channel this service takes posts in.
    pub fn can_post_here(&self) -> bool {
        self.current_channel().is_some() && self.backend.can_post_to_channels()
    }

    /// Whether `send` is to be answered with the question first: the first post to this channel in the session, not
    /// yet answered by a send as the very next key. Records the question and says it when it has to be asked.
    pub(super) fn confirm_post(&mut self, channel: &str) -> bool {
        if self.posted_channels.contains(channel) {
            return true;
        }
        let armed = self
            .post_armed
            .take()
            .filter(|(asked, at)| asked == channel && at.elapsed() < POST_CONFIRM_WINDOW);
        if let Some((_, at)) = armed {
            if at.elapsed() >= POST_CONFIRM_GAP {
                // The question has been answered; it is not left on the status line.
                self.status = None;
                return true;
            }
            // Too soon to have been read: a held-down Enter or a repeating key. The question stays asked, counted from
            // this key, so that a key held down never reaches the end of the gap and confirms, however long it is
            // held; the next send after the person lets go and waits is the answer.
            self.post_asked = Some((channel.to_string(), Instant::now()));
            let again = self.post_again_hint();
            self.set_status(
                format!("That came too quickly to be an answer to the question. {again}"),
                false,
            );
            return false;
        }
        let place = self.current_channel().map_or_else(
            || "this channel".to_string(),
            |o| format!("{} › {}", o.team, o.name),
        );
        self.post_asked = Some((channel.to_string(), Instant::now()));
        let again = self.post_again_hint();
        self.set_status(
            format!("Post to {place}? Everyone in the channel will see it. {again}"),
            false,
        );
        false
    }

    /// What to press to answer the question.
    fn post_again_hint(&self) -> &'static str {
        if self.keymap == crate::editor::Keymap::Standard && self.enter_sends {
            "Press Enter again to post."
        } else {
            "Send again to post."
        }
    }

    /// `:subject <text>` sets the title of the next post, `:subject` shows it, `:subject -` clears it.
    pub(super) fn run_subject_command(&mut self, arg: &str) {
        if self.current_channel().is_none() {
            self.set_status("a subject is for a post in a channel", true);
            return;
        }
        if !self.backend.can_post_to_channels() {
            self.set_status(super::teams::NO_POSTING, true);
            return;
        }
        if arg == "-" {
            self.cancel_subject();
            self.set_status("subject cleared", false);
            return;
        }
        if arg.is_empty() {
            let said = match &self.subject {
                Some(s) => format!("subject of the next post: {s}"),
                None => "no subject; :subject <text> gives the next post one".to_string(),
            };
            self.set_status(said, false);
            return;
        }
        if self.editing.is_some() {
            self.set_status("editing keeps the post's own subject", true);
            return;
        }
        if self.replying.is_some() {
            self.set_status(
                "a reply has no subject; Esc stops replying, then :subject sets one",
                true,
            );
            return;
        }
        // One line of printable text.
        let subject = clean(arg).split_whitespace().collect::<Vec<_>>().join(" ");
        if subject.is_empty() {
            self.set_status("the subject is empty; :subject - clears it", true);
        } else if subject.chars().count() > MAX_SUBJECT_CHARS {
            self.set_status(
                format!("a subject is at most {MAX_SUBJECT_CHARS} characters"),
                true,
            );
        } else {
            self.subject = Some(subject);
            self.set_status(
                "subject set for the next post; :subject - or Esc in an empty box clears it",
                false,
            );
        }
    }

    /// Forget the title of the next post.
    pub(super) fn cancel_subject(&mut self) {
        self.subject = None;
    }
}
