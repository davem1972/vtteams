//! Your own availability: what can be chosen, when you count as away, and when the service is told whether you are
//! around. Pure, so it is tested without a terminal or a clock.
//!
//! The service works out away from what a program reports (at the keyboard, or not); a person can also choose a
//! status outright (busy, do not disturb, ...), which then holds until it is reset.

use crate::cadence;
use std::time::{Duration, Instant};
use teams_client::Availability;

/// How long without a key before you count as away (the `away_after` setting), or never.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AwayAfter {
    Minutes(u32),
    /// Never away: always reported as active while the program is running and signed in.
    Never,
}

impl Default for AwayAfter {
    fn default() -> Self {
        Self::Minutes(5)
    }
}

/// The most that is accepted (a day): longer is the same as never, which has its own word.
const MAX_MINUTES: u32 = 24 * 60;

impl AwayAfter {
    /// A number of minutes (`5`, `10m`, `10 min`), or `never` (also `off`, `no`).
    pub fn parse(value: &str) -> Option<Self> {
        let v = value.trim().to_ascii_lowercase();
        if matches!(v.as_str(), "never" | "off" | "no" | "none") {
            return Some(Self::Never);
        }
        let digits = v
            .trim_end_matches("minutes")
            .trim_end_matches("minute")
            .trim_end_matches("mins")
            .trim_end_matches("min")
            .trim_end_matches('m')
            .trim();
        let n: u32 = digits.parse().ok()?;
        (1..=MAX_MINUTES).contains(&n).then_some(Self::Minutes(n))
    }

    /// The setting's own spelling.
    pub fn label(self) -> String {
        match self {
            Self::Minutes(n) => n.to_string(),
            Self::Never => "never".to_string(),
        }
    }

    /// The values the dialog steps through, shortest first.
    pub const PRESETS: [Self; 6] = [
        Self::Minutes(5),
        Self::Minutes(10),
        Self::Minutes(15),
        Self::Minutes(30),
        Self::Minutes(60),
        Self::Never,
    ];

    /// The next longer preset, wrapping from never round to the shortest. A value between presets (set in the file)
    /// goes to the first preset above it.
    pub fn longer(self) -> Self {
        Self::PRESETS
            .iter()
            .copied()
            .find(|p| p.minutes() > self.minutes())
            .unwrap_or(Self::PRESETS[0])
    }

    /// The next shorter preset, wrapping from the shortest round to never.
    pub fn shorter(self) -> Self {
        Self::PRESETS
            .iter()
            .rev()
            .copied()
            .find(|p| p.minutes() < self.minutes())
            .unwrap_or(Self::Never)
    }

    /// Minutes, with never as the longest of all, so values can be compared.
    fn minutes(self) -> u32 {
        match self {
            Self::Minutes(n) => n,
            Self::Never => u32::MAX,
        }
    }

    /// As the dialog's row says it: "Away after 5 minutes", "Never away".
    pub fn row_text(self) -> String {
        match self {
            Self::Minutes(_) => format!("Away after {}", self.describe()),
            Self::Never => "Never away".to_string(),
        }
    }

    /// In words, for a status line.
    pub fn describe(self) -> String {
        match self {
            Self::Minutes(1) => "1 minute".to_string(),
            Self::Minutes(n) => format!("{n} minutes"),
            Self::Never => "never".to_string(),
        }
    }

    /// Whether someone who last pressed a key `since_input` ago counts as at the keyboard.
    pub fn is_active(self, since_input: Duration) -> bool {
        match self {
            Self::Minutes(n) => since_input < Duration::from_secs(u64::from(n) * 60),
            Self::Never => true,
        }
    }
}

/// What the status dialog offers, in order: how it is called, and what it sets (`None` gives the choice up, so that
/// the service works it out again from whether you are around).
pub const CHOICES: [(&str, Option<Availability>); 7] = [
    ("Available", Some(Availability::Available)),
    ("Busy", Some(Availability::Busy)),
    ("Do not disturb", Some(Availability::DoNotDisturb)),
    ("Be right back", Some(Availability::BeRightBack)),
    ("Appear away", Some(Availability::Away)),
    ("Appear offline", Some(Availability::Offline)),
    ("Reset status", None),
];

/// A status named on the command line (`:status busy`): the choice it stands for, or `None` for a word that is not
/// one. (`Some(None)` is the reset.)
pub fn parse_choice(word: &str) -> Option<Option<Availability>> {
    use Availability::{Available, Away, BeRightBack, Busy, DoNotDisturb, Offline};
    Some(match word.trim().to_ascii_lowercase().as_str() {
        "available" | "online" => Some(Available),
        "busy" => Some(Busy),
        "dnd" | "donotdisturb" | "do-not-disturb" => Some(DoNotDisturb),
        "brb" | "berightback" | "be-right-back" => Some(BeRightBack),
        "away" => Some(Away),
        "offline" | "invisible" => Some(Offline),
        "reset" | "auto" | "clear" => None,
        _ => return None,
    })
}

/// How often to say again that you are active, so the service keeps showing it.
const KEEPALIVE: Duration = Duration::from_secs(120);
/// The soonest one report follows another, whatever has changed.
const MIN_GAP: Duration = Duration::from_secs(5);
/// After a failure the wait is this, doubled for each one in a row (see [`cadence::backoff`]).
const RETRY_BASE: Duration = Duration::from_secs(30);

/// When to tell the service whether you are around: when it changes, and now and then while you are.
#[derive(Debug, Default)]
pub struct Reporter {
    /// What the service was last told, and when.
    sent: Option<(bool, Instant)>,
    /// When a report was last started.
    tried: Option<Instant>,
    fails: u32,
}

impl Reporter {
    /// Whether to report `active` now.
    pub fn should_report(&self, active: bool, now: Instant) -> bool {
        let wait = if self.fails == 0 {
            MIN_GAP
        } else {
            cadence::backoff(RETRY_BASE, self.fails - 1)
        };
        if self
            .tried
            .is_some_and(|at| now.saturating_duration_since(at) < wait)
        {
            return false;
        }
        match self.sent {
            None => true,
            Some((was, at)) => was != active || (active && now.duration_since(at) >= KEEPALIVE),
        }
    }

    /// A report is being sent.
    pub fn started(&mut self, now: Instant) {
        self.tried = Some(now);
    }

    /// The service has been told `active`.
    pub fn succeeded(&mut self, active: bool, now: Instant) {
        self.sent = Some((active, now));
        self.fails = 0;
    }

    pub fn failed(&mut self) {
        self.fails = self.fails.saturating_add(1);
    }

    /// Whatever the service knows is no longer to be relied on (the sign-in was renewed, say): say it again.
    pub fn forget(&mut self) {
        self.sent = None;
    }

    #[cfg(test)]
    pub fn fails(&self) -> u32 {
        self.fails
    }

    /// Make everything that happened `by` earlier than it did, so a test need not wait for a gap to pass.
    #[cfg(test)]
    pub fn rewind(&mut self, by: Duration) {
        self.tried = self.tried.map(|t| t - by);
        self.sent = self.sent.map(|(active, t)| (active, t - by));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn the_away_timeout_takes_minutes_or_never_and_nothing_else() {
        for (text, parsed) in [
            ("5", AwayAfter::Minutes(5)),
            (" 10m ", AwayAfter::Minutes(10)),
            ("15 min", AwayAfter::Minutes(15)),
            ("1 minute", AwayAfter::Minutes(1)),
            ("1440", AwayAfter::Minutes(1440)),
            ("Never", AwayAfter::Never),
            ("off", AwayAfter::Never),
        ] {
            assert_eq!(AwayAfter::parse(text), Some(parsed), "{text}");
        }
        for bad in ["", "0", "1441", "-5", "five", "5 hours", "1.5", "m"] {
            assert_eq!(AwayAfter::parse(bad), None, "{bad:?}");
        }
        assert_eq!(AwayAfter::default(), AwayAfter::Minutes(5));
        for a in [AwayAfter::Minutes(7), AwayAfter::Never] {
            assert_eq!(AwayAfter::parse(&a.label()), Some(a));
        }
        assert_eq!(AwayAfter::Minutes(1).describe(), "1 minute");
        assert_eq!(AwayAfter::Minutes(5).describe(), "5 minutes");
    }

    #[test]
    fn the_dialog_steps_through_the_presets_in_both_directions_and_round_again() {
        use AwayAfter::{Minutes, Never};
        let mut at = Minutes(5);
        let mut seen = vec![at];
        for _ in 0..6 {
            at = at.longer();
            seen.push(at);
        }
        assert_eq!(
            seen,
            [
                Minutes(5),
                Minutes(10),
                Minutes(15),
                Minutes(30),
                Minutes(60),
                Never,
                Minutes(5)
            ]
        );
        assert_eq!(Minutes(5).shorter(), Never, "round the other way");
        assert_eq!(Never.shorter(), Minutes(60));
        assert_eq!(Minutes(10).shorter(), Minutes(5));
        // A value set in the file that is not a preset goes to the one above, or below.
        assert_eq!(Minutes(7).longer(), Minutes(10));
        assert_eq!(Minutes(7).shorter(), Minutes(5));
        assert_eq!(Minutes(2).longer(), Minutes(5));
        assert_eq!(Minutes(500).longer(), Never);
        assert_eq!(Minutes(500).shorter(), Minutes(60));
        assert_eq!(Minutes(5).row_text(), "Away after 5 minutes");
        assert_eq!(Never.row_text(), "Never away");
    }

    #[test]
    fn at_the_keyboard_until_the_timeout_and_always_when_never_away() {
        let five = AwayAfter::Minutes(5);
        assert!(five.is_active(secs(0)) && five.is_active(secs(299)));
        assert!(!five.is_active(secs(300)));
        assert!(AwayAfter::Never.is_active(secs(10 * 24 * 3600)));
    }

    #[test]
    fn the_choices_are_the_six_of_the_teams_menu_and_the_reset_and_all_can_be_set() {
        assert_eq!(CHOICES.len(), 7);
        assert_eq!(CHOICES[6], ("Reset status", None));
        for (name, choice) in CHOICES {
            if let Some(a) = choice {
                assert!(a.is_choosable(), "{name}");
            }
        }
        for (word, choice) in [
            ("busy", Some(Availability::Busy)),
            ("DND", Some(Availability::DoNotDisturb)),
            ("brb", Some(Availability::BeRightBack)),
            ("away", Some(Availability::Away)),
            ("offline", Some(Availability::Offline)),
            ("available", Some(Availability::Available)),
        ] {
            assert_eq!(parse_choice(word), Some(choice), "{word}");
        }
        assert_eq!(parse_choice("reset"), Some(None));
        assert_eq!(parse_choice("sleepy"), None);
    }

    #[test]
    fn a_report_is_made_at_first_when_the_state_changes_and_every_two_minutes_while_active() {
        let t0 = Instant::now();
        let mut r = Reporter::default();
        assert!(r.should_report(true, t0), "never told anyone yet");
        r.started(t0);
        r.succeeded(true, t0);
        assert!(!r.should_report(true, t0 + secs(30)), "nothing has changed");
        assert!(!r.should_report(true, t0 + secs(119)));
        assert!(
            r.should_report(true, t0 + secs(120)),
            "still here: say so again"
        );
        // A change goes at once (once the minimum gap is over), and being idle is said once, not repeated.
        assert!(
            !r.should_report(false, t0 + secs(2)),
            "too soon after the last"
        );
        assert!(r.should_report(false, t0 + secs(10)));
        r.started(t0 + secs(10));
        r.succeeded(false, t0 + secs(10));
        assert!(!r.should_report(false, t0 + secs(600)), "idle is said once");
        assert!(
            r.should_report(true, t0 + secs(600)),
            "back at the keyboard"
        );
    }

    #[test]
    fn failures_back_off_and_forgetting_says_it_again() {
        let t0 = Instant::now();
        let mut r = Reporter::default();
        r.started(t0);
        r.failed();
        assert!(
            !r.should_report(true, t0 + secs(10)),
            "wait after a failure"
        );
        assert!(r.should_report(true, t0 + secs(31)));
        r.started(t0 + secs(31));
        r.failed();
        assert!(!r.should_report(true, t0 + secs(31 + 59)), "twice as long");
        assert!(r.should_report(true, t0 + secs(31 + 61)));
        assert_eq!(r.fails(), 2);
        r.succeeded(true, t0 + secs(100));
        assert_eq!(r.fails(), 0);
        assert!(!r.should_report(true, t0 + secs(110)));
        r.forget();
        assert!(r.should_report(true, t0 + secs(110)));
    }
}
