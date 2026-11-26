//! Favourite chats: marking one, and choosing whether they are pinned above the rest.

use super::{App, Ev, Focus, describe_side};

impl App {
    /// Favourite the open chat, or take it out of the favourites. The list changes at once; the service's answer
    /// either confirms it or puts the chat back where it was and says why.
    pub(super) fn toggle_favorite(&mut self) {
        if !self.backend.can_favorite() {
            self.set_status("this service has no favourites", true);
            return;
        }
        // A channel is not a chat in the favourites; the teams list's own favourite is not changed from here yet.
        if self.current_channel().is_some() || (self.on_teams_tab() && self.focus == Focus::Chats) {
            self.say_channels_are_not_favourites();
            return;
        }
        let Some(chat) = self.current_chat() else {
            self.set_status("select a chat to favourite", true);
            return;
        };
        let (chat_id, previous) = (chat.id.clone(), chat.favorite);
        if !self.chats.begin_favorite(chat_id.clone()) {
            self.set_status("this chat's favourite status is still being saved", true);
            return;
        }
        let on = previous.is_none();
        let place = on.then(|| self.chats.next_favorite());
        self.chats.set_favorite(&chat_id, place);
        let id = chat_id.clone();
        self.spawn_action(
            move |b| b.set_favorite(&id, on),
            move |result| Ev::Favorited {
                chat_id,
                on,
                previous,
                result,
            },
        );
    }

    /// The service's answer to [`App::toggle_favorite`].
    pub(super) fn on_favorited(
        &mut self,
        chat_id: &str,
        on: bool,
        previous: Option<u32>,
        result: Result<(), teams_client::Error>,
    ) {
        self.chats.complete_favorite(chat_id);
        match result {
            Ok(()) => {
                self.set_status(
                    if on {
                        "added to your favourites"
                    } else {
                        "taken out of your favourites"
                    },
                    false,
                );
                // The service keeps the order; take it from there.
                self.chats.sched.refresh_soon();
            }
            Err(e) => {
                self.chats.set_favorite(chat_id, previous);
                self.set_status(
                    format!("could not change your favourites: {}", describe_side(&e)),
                    true,
                );
            }
        }
    }

    /// Favourites on top under a divider, or among the other chats (still marked with a star). In the teams list the
    /// favourite channels are a section at the top, or not there at all.
    pub(super) fn toggle_pinned(&mut self) {
        self.pin_favorites = !self.pin_favorites;
        if self.teams.loaded() && self.team_cursor.is_some() {
            self.teams_keep_cursor();
        }
        self.set_status(
            match (self.on_teams_tab(), self.pin_favorites) {
                (false, true) => "favourites are pinned at the top",
                (false, false) => "favourites are listed with the other chats",
                (true, true) => "favourite channels are shown at the top",
                (true, false) => "favourite channels are not shown",
            },
            false,
        );
    }
}
