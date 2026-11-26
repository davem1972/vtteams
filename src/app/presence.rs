//! Whether the people you have a 1:1 chat with are around: what is known, what to ask the service about and when.
//!
//! Best effort and quiet. A person's availability is asked for the people on screen (the open chat's, and those of the
//! chats near the top of what the list shows) every minute while someone is using the program, less often when idle,
//! and not at all when away or when the service cannot say. A failure leaves what was known as it was, backs off like
//! any poll, and is said once; it never blocks anything or reads as a lost sign-in (see `Ev::sign_in_error`).

use super::{App, Ev, Overlay, describe_side, throttle_hold};
use crate::banner::{Entry, Kind};
use crate::cadence::{self, Engagement, Schedule};
use crate::presence::PresenceStyle;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use teams_client::{Chat, ChatKind, Error, Member, Presence};

/// How often availability is asked for while someone is at the keyboard, and when the terminal is idle or behind
/// other windows. (Another client polls once a minute.)
const REFRESH_ACTIVE: Duration = Duration::from_secs(60);
const REFRESH_IDLE: Duration = Duration::from_secs(180);
/// How many chats, from the top of what the list is showing, are asked about.
const WINDOW: usize = 60;
/// How many people of the open group or meeting chat are asked about, for their status messages and out-of-office
/// notices (nobody is asked about in a channel: it has no roster).
const GROUP_PEOPLE: usize = 40;
/// People not asked about yet are asked about soon, but not more often than this.
const NEW_PEOPLE_GAP: Duration = Duration::from_secs(3);

pub(super) struct Presences {
    style: PresenceStyle,
    pub(super) known: HashMap<String, Presence>,
    /// People an answer has been received for, whether or not it said anything about them.
    asked: HashSet<String>,
    pub(super) in_flight: bool,
    pub(super) sched: Schedule,
    /// The backend cannot do this at all: never ask again.
    unsupported: bool,
    /// The person has been told that it is not working, once.
    pub(super) told: bool,
}

impl Presences {
    pub(super) fn new() -> Self {
        Self {
            style: PresenceStyle::default(),
            known: HashMap::new(),
            asked: HashSet::new(),
            in_flight: false,
            sched: Schedule::new(),
            unsupported: false,
            told: false,
        }
    }

    pub(super) fn refresh_soon(&mut self) {
        self.sched.refresh_soon();
    }

    /// What is shown of `id` until the service next says.
    pub(super) fn set_known(&mut self, id: &str, presence: Presence) {
        self.known.insert(id.to_string(), presence);
    }

    /// Whether to ask now about `wanted`.
    fn should_ask(&self, wanted: &[String], facts: &cadence::Facts) -> bool {
        if self.style == PresenceStyle::Off
            || self.unsupported
            || self.in_flight
            || wanted.is_empty()
            || cadence::is_away(facts)
            || self.sched.held()
        {
            return false;
        }
        let interval = if cadence::engagement(facts) == Engagement::Idle {
            REFRESH_IDLE
        } else {
            REFRESH_ACTIVE
        };
        let someone_new = wanted.iter().any(|id| !self.asked.contains(id));
        self.sched.due(interval)
            || (someone_new && self.sched.fails() == 0 && self.sched.since_last() >= NEW_PEOPLE_GAP)
    }

    fn begin(&mut self) {
        self.in_flight = true;
        self.sched.started();
    }

    fn answered(&mut self, asked: &[String], answer: HashMap<String, Presence>) {
        self.in_flight = false;
        self.sched.succeeded();
        for id in asked {
            self.asked.insert(id.clone());
            // What the service does not say about someone is not known, not what it said last time.
            match answer.get(id) {
                Some(p) => self.known.insert(id.clone(), p.clone()),
                None => self.known.remove(id),
            };
        }
    }

    fn failed(&mut self, error: &Error) {
        self.in_flight = false;
        if matches!(error, Error::Unsupported { .. }) {
            self.unsupported = true;
        } else {
            self.sched.failed(throttle_hold(error));
        }
    }
}

impl App {
    /// Ask about the people on screen at the next tick whatever the schedule says (a one-off frame has no minute to
    /// wait for it).
    pub fn presence_soon(&mut self) {
        self.presences.refresh_soon();
    }

    /// How availability is shown (set from the `presence` setting).
    pub fn set_presence_style(&mut self, style: PresenceStyle) {
        self.presences.style = style;
    }

    pub fn presence_style(&self) -> PresenceStyle {
        self.presences.style
    }

    /// How many columns the list reserves in front of each name for the availability mark and the space after it: none
    /// when it is off or the backend cannot say who is around, so nothing is given up for a mark that cannot come.
    pub fn presence_columns(&self) -> usize {
        if self.backend.can_presence() && self.presences.style != PresenceStyle::Off {
            self.presences.style.width() + 1
        } else {
            0
        }
    }

    /// The other person of a 1:1 chat whose availability means something: not someone the directory no longer lists,
    /// who would only ever be "offline" and is not asked about.
    fn partner_of<'a>(&self, chat: &'a Chat) -> Option<&'a str> {
        if chat.kind != ChatKind::OneOnOne {
            return None;
        }
        chat.members
            .iter()
            .find(|m| m.id != self.me_id() && !m.id.is_empty())
            .filter(|m| !m.not_in_directory)
            .map(|m| m.id.as_str())
    }

    /// How you are shown to others, as far as the service has said (or you have just chosen).
    pub fn own_presence(&self) -> Option<&Presence> {
        if self.presences.style == PresenceStyle::Off {
            return None;
        }
        self.presences.known.get(self.me.as_ref()?.id.as_str())
    }

    /// What is known of how available the other person of a 1:1 `chat` is.
    /// How available `id` is, as far as the service has said (the `:new` picker's people).
    pub fn presence_of_id(&self, id: &str) -> Option<&Presence> {
        if self.presences.style == PresenceStyle::Off {
            return None;
        }
        self.presences.known.get(id)
    }

    pub fn presence_of(&self, chat: &Chat) -> Option<&Presence> {
        if self.presences.style == PresenceStyle::Off {
            return None;
        }
        self.presences.known.get(self.partner_of(chat)?)
    }

    /// The people whose availability is worth knowing now: the open chat's, then those of the chats the list shows.
    fn wanted_presence(&self) -> Vec<String> {
        let items = self.chats.items();
        // You first: it is what the header shows.
        let mut ids: Vec<String> = self.me.iter().map(|me| me.id.clone()).collect();
        let mut want = |id: &str| {
            if !ids.iter().any(|known| known == id) {
                ids.push(id.to_string());
            }
        };
        // The people offered by `:new`, while the picker is open: what it is showing now.
        if let Overlay::People { people, .. } = &self.overlay {
            for p in people.iter().take(crate::app::MAX_PEOPLE_SHOWN) {
                if !p.id.is_empty() {
                    want(&p.id);
                }
            }
        }
        if let Some(chat) = self.current_chat() {
            if let Some(id) = self.partner_of(chat) {
                want(id);
            }
            // Everyone in an open group or meeting chat, for their notes (the list draws no marks for them).
            for person in self.note_people(chat) {
                want(&person.id);
            }
        }
        for &i in self
            .visible_chats()
            .iter()
            .skip(self.view.list_offset)
            .take(WINDOW)
        {
            if let Some(id) = self.partner_of(&items[i]) {
                want(id);
            }
        }
        ids
    }

    /// The people of the open `chat` whose status messages and out-of-office notices can be shown: everyone but you
    /// (with an id and a name), at most [`GROUP_PEOPLE`]. A channel has no roster, so none.
    fn note_people<'a>(&'a self, chat: &'a Chat) -> Vec<&'a Member> {
        if self.presences.style == PresenceStyle::Off {
            return Vec::new();
        }
        let me = self.me_id();
        self.attendees(chat)
            .iter()
            .filter(|m| !m.id.is_empty() && m.id != me)
            .take(GROUP_PEOPLE)
            .collect()
    }

    /// Who in the open chat has a status message or an out-of-office notice up at `now`, out-of-office first: what the
    /// banner above the message box says. Not you, nobody in a channel, nothing when presence is off or the service
    /// cannot say. A note whose time is up is not shown, and neither is an out-of-office reply that has ended, whatever
    /// the calendar flag still says.
    pub fn banner_entries(&self, now: DateTime<Utc>) -> Vec<Entry> {
        if self.presences.style == PresenceStyle::Off || !self.backend.can_presence() {
            return Vec::new();
        }
        let Some(chat) = self.current_chat() else {
            return Vec::new();
        };
        let mut away = Vec::new();
        let mut said = Vec::new();
        for person in self.note_people(chat) {
            let Some(presence) = self.presences.known.get(&person.id) else {
                continue;
            };
            let name = if person.name.is_empty() && chat.kind == ChatKind::OneOnOne {
                chat.title(self.me_id())
            } else {
                person.name.clone()
            };
            if name.is_empty() {
                continue;
            }
            if presence.out_of_office {
                match &presence.out_of_office_note {
                    Some(note) if !note.is_current(now) => {}
                    note => away.push(Entry {
                        kind: Kind::OutOfOffice,
                        name: name.clone(),
                        text: note.as_ref().map(|n| n.text.clone()),
                        until: note.as_ref().and_then(|n| n.expires),
                    }),
                }
            }
            if let Some(note) = presence.note.as_ref().filter(|n| n.is_current(now)) {
                said.push(Entry {
                    kind: Kind::Status,
                    name,
                    text: Some(note.text.clone()),
                    until: None,
                });
            }
        }
        away.extend(said);
        away
    }

    /// Ask for availability if it is time to (called from the tick).
    pub(super) fn refresh_presence(&mut self, facts: &cadence::Facts) {
        if !self.backend.can_presence() {
            return;
        }
        let wanted = self.wanted_presence();
        if !self.presences.should_ask(&wanted, facts) {
            return;
        }
        log::debug!("presence: asking about {} people", wanted.len());
        self.presences.begin();
        let asked = wanted.clone();
        self.spawn_call(
            move |b| b.presence(&wanted),
            move |result| Ev::Presence { asked, result },
        );
    }

    pub(super) fn on_presence(
        &mut self,
        asked: &[String],
        result: Result<HashMap<String, Presence>, Error>,
    ) {
        match result {
            Ok(answer) => self.presences.answered(asked, answer),
            Err(e) => {
                let first = self.presences.sched.fails() == 0 && !self.presences.told;
                log::warn!("presence: {}", e.for_log());
                self.presences.failed(&e);
                // A side feature: said once, in the status line, as what it is (never as a lost sign-in).
                if first && !self.presences.unsupported {
                    self.presences.told = true;
                    self.set_status(
                        format!("availability not shown: {}", describe_side(&e)),
                        true,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use teams_client::Availability;

    #[test]
    fn the_people_the_new_chat_picker_offers_are_asked_about_while_it_is_open() {
        let (mut app, _) = App::for_test(std::sync::Arc::new(crate::demo::DemoBackend::instant()));
        let stranger = teams_client::Contact {
            id: "u-nobody-with-a-chat".into(),
            name: "Barbara Liskov".into(),
            ..Default::default()
        };
        assert!(!app.wanted_presence().contains(&stranger.id));
        app.overlay = Overlay::People {
            people: vec![stranger.clone()],
            sel: 0,
        };
        assert!(app.wanted_presence().contains(&stranger.id));
        app.overlay = Overlay::None;
        assert!(
            !app.wanted_presence().contains(&stranger.id),
            "not once the picker has closed"
        );
    }
    fn id(n: u8) -> String {
        format!("{n:08}-0000-4000-8000-000000000000")
    }

    #[test]
    fn an_answer_replaces_what_was_known_and_drops_whoever_it_no_longer_mentions() {
        let mut p = Presences::new();
        let (a, b) = (id(1), id(2));
        let busy = Presence::new(Availability::Busy, false);
        let away = Presence::new(Availability::Away, false);
        p.begin();
        p.answered(
            &[a.clone(), b.clone()],
            HashMap::from([(a.clone(), busy.clone()), (b.clone(), busy.clone())]),
        );
        assert_eq!(p.known.get(&a), Some(&busy));
        // Next time: a has changed, and nothing is said about b: b is no longer shown as busy.
        p.answered(
            &[a.clone(), b.clone()],
            HashMap::from([(a.clone(), away.clone())]),
        );
        assert_eq!(p.known.get(&a), Some(&away));
        assert!(!p.known.contains_key(&b));
        assert!(
            p.asked.contains(&b),
            "but b has been asked about: not asked again just for being new"
        );
        assert!(!p.in_flight);
    }

    #[test]
    fn a_failure_keeps_what_was_known_and_backs_off_but_an_unsupported_backend_stops_asking() {
        let mut p = Presences::new();
        let a = id(1);
        let free = Presence::new(Availability::Available, false);
        p.answered(
            std::slice::from_ref(&a),
            HashMap::from([(a.clone(), free.clone())]),
        );
        p.begin();
        p.failed(&Error::Auth(teams_client::AuthError::NotSignedIn));
        assert_eq!(p.known.get(&a), Some(&free), "an old answer beats none");
        assert_eq!(p.sched.fails(), 1);
        assert!(!p.unsupported && !p.in_flight);
        p.begin();
        p.failed(&Error::Unsupported {
            operation: "read presence",
            detail: "x",
        });
        assert!(p.unsupported);
        let facts = cadence::Facts {
            since_input: Duration::ZERO,
            focused: true,
            since_exchange: None,
        };
        p.sched.refresh_soon();
        assert!(!p.should_ask(&[a], &facts), "never again");
    }

    #[test]
    fn nobody_is_asked_about_when_away_off_or_already_asking_and_new_people_are_asked_soon() {
        let a = id(1);
        let active = cadence::Facts {
            since_input: Duration::ZERO,
            focused: true,
            since_exchange: None,
        };
        let away = cadence::Facts {
            since_input: Duration::from_secs(3 * 3600),
            ..active
        };
        let mut p = Presences::new();
        p.sched
            .set_last(std::time::Instant::now() - Duration::from_secs(10));
        assert!(
            p.should_ask(std::slice::from_ref(&a), &active),
            "someone not asked about yet"
        );
        assert!(
            !p.should_ask(std::slice::from_ref(&a), &away),
            "away from the keyboard"
        );
        assert!(!p.should_ask(&[], &active), "nobody wanted");
        p.begin();
        assert!(
            !p.should_ask(std::slice::from_ref(&a), &active),
            "one already in flight"
        );
        p.answered(std::slice::from_ref(&a), HashMap::new());
        // Asked a moment ago and nothing new: not before the interval is up.
        assert!(!p.should_ask(std::slice::from_ref(&a), &active));
        // A new person on screen is asked about without waiting out the minute...
        p.sched
            .set_last(std::time::Instant::now() - Duration::from_secs(10));
        assert!(p.should_ask(&[a.clone(), id(2)], &active));
        // ...unless the last try failed (back off).
        p.failed(&Error::Auth(teams_client::AuthError::NotSignedIn));
        p.sched
            .set_last(std::time::Instant::now() - Duration::from_secs(10));
        assert!(!p.should_ask(&[a, id(2)], &active));
        p.style = PresenceStyle::Off;
        p.sched.refresh_soon();
        assert!(!p.should_ask(&[id(3)], &active), "turned off");
    }
}
