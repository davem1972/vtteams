//! Telling the terminal a new message arrived (`bell` setting): the list of chats and the open chat's messages are
//! looked at for something newer than has been seen, and the event loop rings the bell when asked (`take_bell`).

use super::App;
use crate::bell::BellMode;
use chrono::Utc;
use std::time::Instant;
use teams_client::{Delivery, Preview};

impl App {
    pub fn set_bell_mode(&mut self, mode: BellMode) {
        self.bell.set_mode(mode);
    }

    /// Whether the bell should ring now; asking clears it.
    pub fn take_bell(&mut self) -> bool {
        self.bell.take()
    }

    /// A new chat list has arrived: remember what it holds the first time, and after that ring for a chat whose newest
    /// message is somebody else's and newer than any seen.
    pub(super) fn bell_for_list(&mut self) {
        let me = self.me_id().to_string();
        let first = !self.bell.started();
        // Without knowing who we are, no message can be told from our own.
        if me.is_empty() && !first {
            return;
        }
        let (in_front, now, wall) = (self.focused, Instant::now(), Utc::now());
        for chat in self.chats.items() {
            let Some(p) = &chat.preview else { continue };
            if first {
                self.bell.baseline(&chat.id, p.created);
            } else {
                self.bell.observe(
                    &chat.id,
                    p.created,
                    from_someone_else(p, &me),
                    in_front,
                    now,
                    wall,
                );
            }
        }
        // Only a list that was read with the name of who we are counts as the starting point.
        if first && !me.is_empty() {
            self.bell.start();
        }
    }

    /// A page of `chat_id`'s messages was received. `was_loaded` says the chat had messages already: the first page of
    /// a chat only sets the starting point.
    pub(super) fn bell_for_chat(&mut self, chat_id: &str, was_loaded: bool) {
        let me = self.me_id().to_string();
        let Some(newest) = self.states.get(chat_id).and_then(|st| {
            st.messages().iter().rev().find(|m| {
                !m.system
                    && !m.deleted
                    && m.delivery == Delivery::Sent
                    && !m.id.starts_with("local-")
            })
        }) else {
            return;
        };
        let (created, foreign) = (
            newest.created,
            !me.is_empty() && newest.from_id.as_deref().is_some_and(|f| f != me),
        );
        if was_loaded {
            self.bell.observe(
                chat_id,
                created,
                foreign,
                self.focused,
                Instant::now(),
                Utc::now(),
            );
        } else {
            self.bell.baseline(chat_id, created);
        }
    }
}

/// Whether a chat's newest message is a person's other than the signed-in user (not a notice a service posted).
fn from_someone_else(p: &Preview, me: &str) -> bool {
    !p.automated && p.from_id.as_deref().is_some_and(|f| f != me)
}
