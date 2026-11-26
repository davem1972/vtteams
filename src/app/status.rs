//! Your own availability: telling the service whether you are at the keyboard (so it shows you as available, and as
//! away when you are idle, as the Teams app does) and choosing a status outright (the dialog, `:status`).
//!
//! Best effort and quiet, like the presence of other people: a failure to report is said once and then backs off, never
//! read as a lost sign-in (`Ev::Reported` and `Ev::StatusSet` are not in `Ev::sign_in_error`), and `--dump` reports and
//! sets nothing.

use super::{App, Ev, Overlay, describe_side};
use crate::cadence;
use crate::status::{AwayAfter, CHOICES, Reporter, parse_choice};
use std::time::Instant;
use teams_client::{Availability, Error, Presence};

/// What the program does about your own availability.
pub(super) struct Own {
    /// Whether to report whether you are at the keyboard (the `activity` setting).
    report: bool,
    away_after: AwayAfter,
    pub(super) reporter: Reporter,
    in_flight: bool,
    /// A status is being set.
    setting: bool,
    /// The service cannot take reports at all.
    unsupported: bool,
    /// A failure to report has been said, once.
    pub(super) told: bool,
}

impl Own {
    pub(super) fn new() -> Self {
        Self {
            report: false,
            away_after: AwayAfter::default(),
            reporter: Reporter::default(),
            in_flight: false,
            setting: false,
            unsupported: false,
            told: false,
        }
    }
}

/// The dialog's entry for `choice`: how it is called.
fn name_of(choice: Option<Availability>) -> &'static str {
    CHOICES
        .iter()
        .find(|(_, c)| *c == choice)
        .map_or("Status", |(name, _)| name)
}

impl App {
    /// Whether to tell the service you are at the keyboard (the `activity` setting), and when you count as away.
    pub fn set_activity(&mut self, report: bool, away_after: AwayAfter) {
        self.own.report = report;
        self.own.away_after = away_after;
    }

    pub fn away_after(&self) -> AwayAfter {
        self.own.away_after
    }

    /// Whether your own status can be chosen here: the service takes it, and this is not a read-only look (`--dump`).
    pub fn can_set_status(&self) -> bool {
        self.backend.can_set_status() && !self.read_only
    }

    /// Report whether you are at the keyboard, if it is time to (called from the tick).
    pub(super) fn report_activity_if_due(&mut self, facts: &cadence::Facts) {
        if !self.own.report || self.own.unsupported || self.own.in_flight || !self.can_set_status()
        {
            return;
        }
        let active = self.own.away_after.is_active(facts.since_input);
        let now = Instant::now();
        if !self.own.reporter.should_report(active, now) {
            return;
        }
        self.own.reporter.started(now);
        self.own.in_flight = true;
        log::debug!(
            "activity: telling the service you are {}",
            if active { "active" } else { "idle" }
        );
        self.spawn_call(
            move |b| b.report_activity(active),
            move |result| Ev::Reported { active, result },
        );
    }

    pub(super) fn on_reported(&mut self, active: bool, result: Result<(), Error>) {
        self.own.in_flight = false;
        match result {
            Ok(()) => self.own.reporter.succeeded(active, Instant::now()),
            Err(e) => {
                log::warn!("activity: could not report: {}", e.for_log());
                self.own.reporter.failed();
                if matches!(e, Error::Unsupported { .. }) {
                    self.own.unsupported = true;
                } else if !self.own.told {
                    self.own.told = true;
                    self.set_status(
                        format!(
                            "others cannot see that you are online: {}",
                            describe_side(&e)
                        ),
                        true,
                    );
                }
            }
        }
    }

    /// Choose your status (`None`: let it follow whether you are around again).
    pub(super) fn choose_status(&mut self, choice: Option<Availability>) {
        if !self.can_set_status() {
            self.set_status("this service cannot set your status from here", true);
            return;
        }
        if self.own.setting {
            self.set_status("still setting the last status", true);
            return;
        }
        self.own.setting = true;
        self.spawn_action(
            move |b| b.set_status(choice),
            move |result| Ev::StatusSet { choice, result },
        );
    }

    pub(super) fn on_status_set(
        &mut self,
        choice: Option<Availability>,
        result: Result<(), Error>,
    ) {
        self.own.setting = false;
        match result {
            Ok(()) => {
                // Shown at once; the next answer from the service is what it really is.
                if let (Some(a), Some(me)) = (choice, self.me.as_ref().map(|m| m.id.clone())) {
                    self.presences.set_known(&me, Presence::new(a, false));
                }
                self.presences.refresh_soon();
                let text = match choice {
                    Some(_) => format!("status: {}", name_of(choice)),
                    None => "status reset: it follows whether you are around".to_string(),
                };
                self.set_status(text, false);
            }
            Err(e) => {
                log::warn!("status: could not set it: {}", e.for_log());
                self.set_status(
                    format!("cannot set your status: {}", describe_side(&e)),
                    true,
                );
            }
        }
    }

    /// The service has been told you are here before this: say it again after a renewed sign-in.
    pub(super) fn forget_reported_activity(&mut self) {
        self.own.reporter.forget();
    }

    /// Open the dialog for choosing your status (and, on its last row, how long before you show as away).
    pub(super) fn open_status_dialog(&mut self) {
        if !self.can_set_status() {
            self.set_status("this service cannot set your status from here", true);
            return;
        }
        let current = self.own_presence().map(|p| p.availability);
        let sel = CHOICES
            .iter()
            .position(|(_, c)| match (c, current) {
                (Some(Availability::Away), Some(Availability::AvailableIdle)) => true,
                (Some(a), Some(now)) => *a == now,
                _ => false,
            })
            .unwrap_or(0);
        self.overlay = Overlay::Status { sel };
    }

    /// Whether the dialog entry `index` is how you are shown now.
    pub fn status_choice_is_current(&self, index: usize) -> bool {
        let (Some((_, choice)), Some(own)) = (CHOICES.get(index), self.own_presence()) else {
            return false;
        };
        match choice {
            Some(Availability::Away) => matches!(
                own.availability,
                Availability::Away | Availability::AvailableIdle
            ),
            Some(a) => *a == own.availability,
            None => false,
        }
    }

    pub(super) fn on_status_key(&mut self, key: ratatui::crossterm::event::KeyEvent, sel: usize) {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // The row after the choices is the away time: it is stepped through, and the dialog stays open.
        let away_row = CHOICES.len();
        let last = away_row;
        let step = |app: &mut Self, longer: bool| {
            let now = app.own.away_after;
            app.own.away_after = if longer { now.longer() } else { now.shorter() };
        };
        match key.code {
            KeyCode::Char('a') => return step(self, true),
            KeyCode::Right | KeyCode::Char('l') if sel == away_row => return step(self, true),
            KeyCode::Left | KeyCode::Char('h') if sel == away_row => return step(self, false),
            KeyCode::Enter if sel == away_row => return step(self, true),
            KeyCode::Char('8') => {
                self.overlay = Overlay::Status { sel: away_row };
                return step(self, true);
            }
            _ => {}
        }
        let pick = match key.code {
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => Err((sel + 1).min(last)),
            KeyCode::Char('n') if ctrl => Err((sel + 1).min(last)),
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => Err(sel.saturating_sub(1)),
            KeyCode::Char('p') if ctrl => Err(sel.saturating_sub(1)),
            KeyCode::Enter => Ok(Some(sel)),
            KeyCode::Char(c @ '1'..='7') => Ok(Some(c as usize - '1' as usize)),
            KeyCode::Esc | KeyCode::Char('q') => Ok(None),
            _ => Err(sel),
        };
        match pick {
            Err(sel) => self.overlay = Overlay::Status { sel },
            Ok(chosen) => {
                self.overlay = Overlay::None;
                if let Some((_, choice)) = chosen.and_then(|i| CHOICES.get(i)) {
                    self.choose_status(*choice);
                }
            }
        }
    }

    /// `:status`: the dialog, or `:status busy`, `:status reset`, `:status away-after 15`.
    pub(super) fn run_status_command(&mut self, arg: &str) {
        const USAGE: &str = "usage: :status [available|busy|dnd|brb|away|offline|reset | away-after <minutes>|never]";
        let arg = arg.trim();
        if arg.is_empty() {
            self.open_status_dialog();
        } else if let Some(rest) = arg
            .strip_prefix("away-after")
            .or_else(|| arg.strip_prefix("away_after"))
        {
            match AwayAfter::parse(rest) {
                Some(after) => {
                    self.own.away_after = after;
                    let text = match after {
                        AwayAfter::Never => {
                            "you are reported as at the keyboard all the time (until you quit)"
                                .to_string()
                        }
                        AwayAfter::Minutes(_) => format!(
                            "away after {} without a key (until you quit)",
                            after.describe()
                        ),
                    };
                    self.set_status(text, false);
                }
                None => self.set_status(USAGE, true),
            }
        } else if let Some(choice) = parse_choice(arg) {
            self.choose_status(choice);
        } else {
            self.set_status(USAGE, true);
        }
    }
}
