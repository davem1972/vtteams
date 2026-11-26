//! How often to look for new messages and chat-list changes, and when each fetch is next due.
//!
//! Updates are fetched by polling, and how often depends on what the user is doing:
//!
//! | engagement   | when                                                          | open chat | chat list |
//! |--------------|---------------------------------------------------------------|-----------|-----------|
//! | conversation | a message arrived in or was sent from the open chat in the last 2 minutes | 4 s | 20 s |
//! | active       | input in the last 5 minutes, terminal in front                | 8 s       | 30 s      |
//! | idle         | no input for 5 minutes, or the terminal is behind other windows | 30 s    | 60 s      |
//! | away         | no input for 30 minutes                                       | 120 s     | 180 s     |
//!
//! A conversation is polled quickly even when the terminal is behind another window, because that is where the user
//! waits for an answer.
//!
//! [`Schedule`] tracks one polled resource (the chat list, or one chat's messages): when it was last fetched, whether
//! it should be fetched at once, how many fetches failed in a row, and a hold-off after the service asked us to wait.
//!
//! Everything here is pure, so the policy can be tested without a clock or a service.

use std::time::{Duration, Instant};

/// A message sent or received within this long makes the chat a conversation in progress.
const CONVERSATION_WINDOW: Duration = Duration::from_secs(120);
/// Without input for this long, the user is idle.
const IDLE_AFTER: Duration = Duration::from_secs(5 * 60);
/// Without input for this long, the user is away.
const AWAY_AFTER: Duration = Duration::from_secs(30 * 60);
/// How long to wait after a throttled answer that did not say how long.
pub const THROTTLED_WAIT: Duration = Duration::from_secs(30);
/// The longest hold-off honoured, however long the service asks for.
pub(crate) const MAX_HOLD: Duration = Duration::from_secs(10 * 60);

/// What the policy needs to know about the user.
#[derive(Clone, Copy, Debug)]
pub struct Facts {
    /// Time since the last key, mouse or paste event.
    pub since_input: Duration,
    /// Whether the terminal is in front. Terminals that cannot report it count as focused.
    pub focused: bool,
    /// Time since a message arrived in or was sent from the open chat, if one has.
    pub since_exchange: Option<Duration>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Engagement {
    Conversation,
    Active,
    Idle,
    Away,
}

pub fn engagement(facts: &Facts) -> Engagement {
    if facts
        .since_exchange
        .is_some_and(|t| t < CONVERSATION_WINDOW)
    {
        return Engagement::Conversation;
    }
    // A terminal behind other windows is at best idle, however recently a key was pressed in it.
    let quiet = if facts.focused {
        facts.since_input
    } else {
        facts.since_input.max(IDLE_AFTER)
    };
    if quiet < IDLE_AFTER {
        Engagement::Active
    } else if quiet < AWAY_AFTER {
        Engagement::Idle
    } else {
        Engagement::Away
    }
}

/// Whether the person has been away from the keyboard: nothing typed for [`AWAY_AFTER`] (or the terminal behind other
/// windows for that long). Unlike [`engagement`] this ignores messages arriving, which keep a chat polled quickly
/// but say nothing about whether anyone is there to read them.
pub fn is_away(facts: &Facts) -> bool {
    engagement(&Facts {
        since_exchange: None,
        ..*facts
    }) == Engagement::Away
}

/// How long to wait between fetches of the open chat.
pub fn messages_poll(facts: &Facts) -> Duration {
    Duration::from_secs(match engagement(facts) {
        Engagement::Conversation => 4,
        Engagement::Active => 8,
        Engagement::Idle => 30,
        Engagement::Away => 120,
    })
}

/// How long to wait between fetches of the chat list.
pub fn chats_refresh(facts: &Facts) -> Duration {
    Duration::from_secs(match engagement(facts) {
        Engagement::Conversation => 20,
        Engagement::Active => 30,
        Engagement::Idle => 60,
        Engagement::Away => 180,
    })
}

/// How long to wait before the next try after `fails` failures in a row: doubles each time, up to 16 times `base`.
pub fn backoff(base: Duration, fails: u32) -> Duration {
    base * 2u32.pow(fails.min(4))
}

/// When one polled resource is next due.
#[derive(Clone, Debug)]
pub struct Schedule {
    /// When the last fetch started or finished.
    last: Instant,
    /// Fetch at the next opportunity whatever the interval says.
    now: bool,
    /// Consecutive failures; the wait grows with each (see [`backoff`]).
    fails: u32,
    /// The service asked us to wait until then.
    hold_until: Option<Instant>,
}

impl Schedule {
    pub fn new() -> Self {
        Self {
            last: Instant::now(),
            now: false,
            fails: 0,
            hold_until: None,
        }
    }

    /// A fetch is starting.
    pub fn started(&mut self) {
        self.last = Instant::now();
        self.now = false;
    }

    /// A fetch succeeded.
    pub fn succeeded(&mut self) {
        self.last = Instant::now();
        self.fails = 0;
        self.hold_until = None;
    }

    /// A fetch failed. `hold` is how long the service asked us to wait (a throttled answer), if it did; it is
    /// honoured up to ten minutes.
    pub fn failed(&mut self, hold: Option<Duration>) {
        self.last = Instant::now();
        self.fails = self.fails.saturating_add(1);
        if let Some(hold) = hold {
            self.hold_until = Some(self.last + hold.min(MAX_HOLD));
        }
    }

    /// Fetch at the next opportunity, unless the service asked us to wait.
    pub fn refresh_soon(&mut self) {
        self.now = true;
    }

    /// Whether to fetch now, given the interval the user's activity calls for. A hold-off from the service beats
    /// everything else; failures stretch the interval.
    pub fn due(&self, interval: Duration) -> bool {
        if self.held() {
            return false;
        }
        self.now || self.last.elapsed() > backoff(interval, self.fails)
    }

    /// Whether the service asked us to wait and the wait is not over.
    pub fn held(&self) -> bool {
        self.held_for().is_some()
    }

    /// How much longer the service asked us to wait, if the wait is not over.
    pub fn held_for(&self) -> Option<Duration> {
        self.hold_until
            .and_then(|until| until.checked_duration_since(Instant::now()))
            .filter(|left| !left.is_zero())
    }

    pub fn fails(&self) -> u32 {
        self.fails
    }

    /// How long since the last fetch started or finished: called before `succeeded` or `failed`, how long it took.
    pub fn since_last(&self) -> Duration {
        self.last.elapsed()
    }

    #[cfg(test)]
    pub fn last(&self) -> Instant {
        self.last
    }

    #[cfg(test)]
    pub fn set_last(&mut self, last: Instant) {
        self.last = last;
    }
}

impl Default for Schedule {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn ago(n: u64) -> Instant {
        Instant::now()
            .checked_sub(secs(n))
            .expect("the clock has run long enough")
    }

    fn facts(since_input: u64, focused: bool, since_exchange: Option<u64>) -> Facts {
        Facts {
            since_input: secs(since_input),
            focused,
            since_exchange: since_exchange.map(secs),
        }
    }

    #[test]
    fn a_message_arriving_does_not_make_an_absent_person_present() {
        // Nothing typed for an hour, but a colleague posted a minute ago: polled as a conversation, still away.
        let absent = facts(3600, true, Some(60));
        assert_eq!(engagement(&absent), Engagement::Conversation);
        assert!(is_away(&absent));
        assert!(!is_away(&facts(60, true, Some(60))));
        assert!(!is_away(&facts(1799, true, None)));
        assert!(is_away(&facts(1800, true, None)));
        // A terminal behind other windows is as good as away once the idle time has passed too.
        assert!(!is_away(&facts(0, false, None)));
    }

    #[test]
    fn the_intervals_follow_the_table() {
        let cases = [
            // (facts, open chat, chat list)
            (facts(0, true, Some(10)), 4, 20),
            (facts(0, true, None), 8, 30),
            (facts(299, true, None), 8, 30),
            (facts(300, true, None), 30, 60),
            (facts(1799, true, None), 30, 60),
            (facts(1800, true, None), 120, 180),
        ];
        for (f, messages, chats) in cases {
            assert_eq!(messages_poll(&f), secs(messages), "{f:?}");
            assert_eq!(chats_refresh(&f), secs(chats), "{f:?}");
        }
    }

    #[test]
    fn a_conversation_ends_two_minutes_after_the_last_message() {
        assert_eq!(
            engagement(&facts(0, true, Some(119))),
            Engagement::Conversation
        );
        assert_eq!(engagement(&facts(0, true, Some(120))), Engagement::Active);
    }

    #[test]
    fn a_terminal_behind_other_windows_is_at_best_idle_but_a_conversation_is_still_followed() {
        assert_eq!(engagement(&facts(0, false, None)), Engagement::Idle);
        assert_eq!(engagement(&facts(40 * 60, false, None)), Engagement::Away);
        assert_eq!(
            engagement(&facts(0, false, Some(30))),
            Engagement::Conversation
        );
    }

    #[test]
    fn backoff_doubles_up_to_sixteen_times() {
        let waits: Vec<u64> = [0, 1, 2, 3, 4, 5, 50]
            .iter()
            .map(|&n| backoff(secs(8), n).as_secs())
            .collect();
        assert_eq!(waits, [8, 16, 32, 64, 128, 128, 128]);
    }

    #[test]
    fn a_schedule_is_due_after_its_interval_and_not_before() {
        let mut s = Schedule::new();
        s.set_last(ago(5));
        assert!(!s.due(secs(8)));
        s.set_last(ago(9));
        assert!(s.due(secs(8)));
    }

    #[test]
    fn failures_stretch_the_wait_and_success_restores_it() {
        let mut s = Schedule::new();
        s.failed(None);
        s.failed(None);
        assert_eq!(s.fails(), 2);
        s.set_last(ago(20));
        assert!(!s.due(secs(8)), "two failures: 32 s, not 8");
        s.set_last(ago(40));
        assert!(s.due(secs(8)));
        s.succeeded();
        assert_eq!(s.fails(), 0);
        s.set_last(ago(9));
        assert!(s.due(secs(8)));
    }

    #[test]
    fn refresh_soon_makes_it_due_at_once_and_starting_a_fetch_clears_it() {
        let mut s = Schedule::new();
        assert!(!s.due(secs(8)));
        s.refresh_soon();
        assert!(s.due(secs(8)));
        s.started();
        assert!(!s.due(secs(8)));
    }

    #[test]
    fn a_hold_off_from_the_service_beats_failures_and_refresh_soon() {
        let mut s = Schedule::new();
        s.failed(Some(secs(120)));
        s.refresh_soon();
        s.set_last(ago(100));
        assert!(s.held());
        assert!(!s.due(secs(4)), "asked to wait two minutes");
        s.succeeded();
        assert!(!s.held(), "a success ends the hold-off");
        s.refresh_soon();
        assert!(s.due(secs(4)));
    }

    #[test]
    fn a_hold_off_is_capped_and_a_short_one_expires() {
        let mut s = Schedule::new();
        s.failed(Some(secs(24 * 3600)));
        assert!(s.held());
        assert!(
            s.hold_until.expect("held") <= Instant::now() + MAX_HOLD + secs(1),
            "never held for longer than ten minutes"
        );
        let mut s = Schedule::new();
        s.failed(Some(Duration::ZERO));
        assert!(!s.held());
    }

    proptest! {
        /// Whatever the facts, polling stays within sane bounds.
        #[test]
        fn intervals_are_always_between_four_seconds_and_three_minutes(
            input in 0u64..100_000,
            focused in any::<bool>(),
            exchange in proptest::option::of(0u64..100_000),
        ) {
            let f = facts(input, focused, exchange);
            prop_assert!((secs(4)..=secs(120)).contains(&messages_poll(&f)));
            prop_assert!((secs(20)..=secs(180)).contains(&chats_refresh(&f)));
        }

        /// Longer without input never makes polling faster.
        #[test]
        fn more_quiet_never_polls_faster(
            a in 0u64..100_000,
            b in 0u64..100_000,
            focused in any::<bool>(),
            exchange in proptest::option::of(0u64..100_000),
        ) {
            let (short, long) = (a.min(b), a.max(b));
            let (fs, fl) = (facts(short, focused, exchange), facts(long, focused, exchange));
            prop_assert!(messages_poll(&fl) >= messages_poll(&fs));
            prop_assert!(chats_refresh(&fl) >= chats_refresh(&fs));
        }

        /// A more recent exchange never makes polling slower.
        #[test]
        fn a_fresher_exchange_never_polls_slower(
            a in 0u64..100_000,
            b in 0u64..100_000,
            input in 0u64..100_000,
            focused in any::<bool>(),
        ) {
            let (fresh, stale) = (a.min(b), a.max(b));
            let (ff, fs) = (facts(input, focused, Some(fresh)), facts(input, focused, Some(stale)));
            prop_assert!(messages_poll(&ff) <= messages_poll(&fs));
            prop_assert!(chats_refresh(&ff) <= chats_refresh(&fs));
        }

        /// The wait after failures never shrinks as failures accumulate, and never exceeds sixteen intervals.
        #[test]
        fn backoff_is_monotonic_and_bounded(base in 1u64..300, a in 0u32..100, b in 0u32..100) {
            let (few, many) = (a.min(b), a.max(b));
            prop_assert!(backoff(secs(base), few) <= backoff(secs(base), many));
            prop_assert!(backoff(secs(base), many) <= secs(base * 16));
        }
    }
}
