//! Who is in the open group or meeting chat, for the line of names under its title.
//!
//! The chat list names the members of a chat only when it has to (an untitled one), so a group or meeting chat with a
//! name of its own arrives with no roster. The open chat's is asked for once and remembered; people the directory has
//! not named yet are looked for again a few times, then left as a count. A side call: nothing waits for it, a failure
//! is logged and never read as a lost sign-in (see `Ev::sign_in_error`), and `Error::Unsupported` ends it.

use super::{App, Ev};
use chrono::{DateTime, Utc};
use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use teams_client::{Chat, ChatKind, Member};

/// The most people a channel offers for an `@name`.
const MAX_POSTERS: usize = 300;

/// Time between asks about one chat while some of its people have no name yet, and how often that is repeated.
const RETRY_GAP: Duration = Duration::from_secs(6);
const MAX_TRIES: u32 = 5;

/// `members` with each name as a mention carries it (`convert::mention_name`: one line, no mention markers), borrowed
/// when none needed changing.
fn tidy_names(members: &[Member]) -> Cow<'_, [Member]> {
    if members
        .iter()
        .all(|m| crate::convert::mention_name(&m.name) == m.name)
    {
        return Cow::Borrowed(members);
    }
    Cow::Owned(
        members
            .iter()
            .map(|m| Member {
                not_in_directory: false,
                name: crate::convert::mention_name(&m.name),
                id: m.id.clone(),
            })
            .collect(),
    )
}

struct Roster {
    members: Vec<Member>,
    /// How many times it has been asked for (successfully or not).
    tries: u32,
    asked_at: Instant,
}

pub(super) struct Rosters {
    by_chat: HashMap<String, Roster>,
    in_flight: bool,
    /// The backend cannot say who is in a chat: never ask again.
    unsupported: bool,
}

impl Rosters {
    pub(super) fn new() -> Self {
        Self {
            by_chat: HashMap::new(),
            in_flight: false,
            unsupported: false,
        }
    }

    /// Whether `chat` (the open one) should be asked about now.
    fn due(&self, chat: &Chat) -> bool {
        if self.unsupported
            || self.in_flight
            || chat.kind == ChatKind::OneOnOne
            || !chat.members.is_empty()
        {
            return false;
        }
        match self.by_chat.get(&chat.id) {
            None => true,
            Some(r) => {
                r.tries < MAX_TRIES
                    && (r.members.is_empty() || r.members.iter().any(|m| m.name.is_empty()))
                    && r.asked_at.elapsed() >= RETRY_GAP
            }
        }
    }
}

impl App {
    /// The people of `chat` as far as they are known: the list's own roster, else the one asked for.
    pub fn attendees<'a>(&'a self, chat: &'a Chat) -> &'a [Member] {
        if !chat.members.is_empty() {
            return &chat.members;
        }
        self.rosters
            .by_chat
            .get(&chat.id)
            .map_or(&[], |r| r.members.as_slice())
    }

    /// The people an `@name` typed in the message box can name: the open chat's attendees, or, for an open channel,
    /// the people who have posted in it. A channel's own roster cannot be read (the service gives one entry, you), so
    /// `Backend::members` is never asked about one.
    pub fn mention_candidates(&self) -> Cow<'_, [Member]> {
        if self.current_channel().is_some() {
            return Cow::Owned(
                self.channel_posters()
                    .into_iter()
                    .map(|m| Member {
                        not_in_directory: false,
                        name: crate::convert::mention_name(&m.name),
                        ..m
                    })
                    .collect(),
            );
        }
        match self.current_chat() {
            Some(chat) => tidy_names(self.attendees(chat)),
            None => Cow::Borrowed(&[]),
        }
    }

    /// The distinct people who have posted in the open channel's loaded messages, the most recent poster first: not
    /// you, nobody who is only in a system event or a deleted message, nobody with no name. At most
    /// [`MAX_POSTERS`]. Worked out from the messages each time it is asked (only while an `@` is being typed).
    fn channel_posters(&self) -> Vec<Member> {
        let me = self.me_id();
        let Some(state) = self.state() else {
            return Vec::new();
        };
        // The newest message of each sender, which also gives the name as it stands now.
        let mut newest: HashMap<&str, (DateTime<Utc>, &str)> = HashMap::new();
        for m in state.messages() {
            let Some(id) = m.from_id.as_deref() else {
                continue;
            };
            let name = m.from_name.trim();
            if m.system || m.deleted || id == me || name.is_empty() {
                continue;
            }
            match newest.get(id) {
                Some((at, _)) if *at >= m.created => {}
                _ => {
                    newest.insert(id, (m.created, name));
                }
            }
        }
        let mut people: Vec<(DateTime<Utc>, &str, &str)> = newest
            .into_iter()
            .map(|(id, (at, name))| (at, id, name))
            .collect();
        people.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        people.truncate(MAX_POSTERS);
        people
            .into_iter()
            .map(|(_, id, name)| Member {
                not_in_directory: false,
                id: id.to_string(),
                name: name.to_string(),
            })
            .collect()
    }

    /// Ask for the open chat's roster if it is missing (called from the tick).
    pub(super) fn refresh_roster(&mut self) {
        let Some(chat) = self.current_chat() else {
            return;
        };
        if !self.rosters.due(chat) {
            return;
        }
        let id = chat.id.clone();
        self.rosters.in_flight = true;
        self.spawn_call(
            {
                let id = id.clone();
                move |b| b.members(&id)
            },
            move |result| Ev::Roster {
                chat_id: id,
                result,
            },
        );
    }

    pub(super) fn on_roster(
        &mut self,
        chat_id: String,
        result: Result<Vec<Member>, teams_client::Error>,
    ) {
        self.rosters.in_flight = false;
        let previous = self.rosters.by_chat.get(&chat_id);
        let tries = previous.map_or(0, |r| r.tries) + 1;
        match result {
            Ok(members) => {
                self.rosters.by_chat.insert(
                    chat_id,
                    Roster {
                        members,
                        tries,
                        asked_at: Instant::now(),
                    },
                );
            }
            Err(e) => {
                log::warn!(
                    "chat {}: roster: {}",
                    teams_client::id_tag(&chat_id),
                    e.for_log()
                );
                if matches!(e, teams_client::Error::Unsupported { .. }) {
                    self.rosters.unsupported = true;
                } else {
                    let members = previous.map(|r| r.members.clone()).unwrap_or_default();
                    self.rosters.by_chat.insert(
                        chat_id,
                        Roster {
                            members,
                            tries,
                            asked_at: Instant::now(),
                        },
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(kind: ChatKind, members: Vec<Member>) -> Chat {
        Chat {
            id: "19:x@thread.v2".into(),
            kind,
            topic: Some("Planning".into()),
            members,
            updated: chrono::Utc::now(),
            preview: None,
            last_read: None,
            web_url: None,
            favorite: None,
        }
    }

    fn member(id: &str, name: &str) -> Member {
        Member {
            not_in_directory: false,
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn only_a_group_or_meeting_chat_without_a_roster_is_asked_about() {
        let r = Rosters::new();
        assert!(r.due(&chat(ChatKind::Meeting, vec![])));
        assert!(r.due(&chat(ChatKind::Group, vec![])));
        assert!(!r.due(&chat(ChatKind::OneOnOne, vec![])));
        assert!(!r.due(&chat(ChatKind::Group, vec![member("a", "Ada")])));
    }

    #[test]
    fn unnamed_people_are_asked_for_again_but_not_at_once_and_not_forever() {
        let c = chat(ChatKind::Meeting, vec![]);
        let mut r = Rosters::new();
        r.by_chat.insert(
            c.id.clone(),
            Roster {
                members: vec![member("a", "")],
                tries: 1,
                asked_at: Instant::now(),
            },
        );
        assert!(!r.due(&c), "just asked");
        r.by_chat.get_mut(&c.id).unwrap().asked_at = Instant::now() - RETRY_GAP;
        assert!(r.due(&c));
        r.by_chat.get_mut(&c.id).unwrap().tries = MAX_TRIES;
        assert!(!r.due(&c), "gave up");
        let r2 = {
            let mut r2 = Rosters::new();
            r2.by_chat.insert(
                c.id.clone(),
                Roster {
                    members: vec![member("a", "Ada")],
                    tries: 1,
                    asked_at: Instant::now() - RETRY_GAP,
                },
            );
            r2
        };
        assert!(!r2.due(&c), "everyone named: done");
    }

    #[test]
    fn nothing_is_asked_while_a_request_is_out_or_when_unsupported() {
        let c = chat(ChatKind::Group, vec![]);
        let mut r = Rosters::new();
        r.in_flight = true;
        assert!(!r.due(&c));
        r.in_flight = false;
        r.unsupported = true;
        assert!(!r.due(&c));
    }

    #[test]
    fn empty_roster_is_retried_within_limits() {
        let c = chat(ChatKind::Meeting, vec![]);
        let mut r = Rosters::new();
        r.by_chat.insert(
            c.id.clone(),
            Roster {
                members: vec![],
                tries: 1,
                asked_at: Instant::now()
                    .checked_sub(RETRY_GAP)
                    .expect("RETRY_GAP fits"),
            },
        );
        assert!(r.due(&c));
        r.by_chat.get_mut(&c.id).unwrap().asked_at = Instant::now();
        assert!(!r.due(&c), "just asked");
        r.by_chat.get_mut(&c.id).unwrap().tries = MAX_TRIES;
        assert!(!r.due(&c), "gave up");
    }
}
