//! In-app sign-in recovery. A failure reason belongs only to an expired sign-in.

use teams_client::Error;

/// Longest text kept about one problem; the popup shows it whole, so this is only a bound on what a service can send.
const MAX_DETAIL: usize = 1500;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SignIn {
    /// Requests may use the current sign-in.
    Fine,
    /// Requests wait for another attempt. `reason` is why the sign-in was lost; only a failed attempt has a `failure`
    /// to show besides.
    Expired {
        reason: String,
        failure: Option<String>,
    },
    /// A recovery browser is already open. A second request must not open another one.
    Waiting { reason: String },
}

/// What an error says, as plain text for the popup: nothing a terminal would act on, and bounded.
fn detail(error: &Error) -> String {
    teams_client::clean(&error.to_string())
        .chars()
        .take(MAX_DETAIL)
        .collect()
}

impl SignIn {
    pub fn is_online(&self) -> bool {
        matches!(self, Self::Fine)
    }

    /// Why the last attempt to sign in again failed.
    pub fn failure(&self) -> Option<&str> {
        match self {
            Self::Expired { failure, .. } => failure.as_deref(),
            _ => None,
        }
    }

    /// Why the sign-in was lost (the error that ended it), when it is.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Expired { reason, .. } | Self::Waiting { reason } => Some(reason),
            Self::Fine => None,
        }
    }

    pub(super) fn note_lost(&mut self, error: &Error, can_sign_in: bool) -> bool {
        if self.is_online() && error.needs_sign_in() && can_sign_in {
            *self = Self::Expired {
                reason: detail(error),
                failure: None,
            };
            true
        } else {
            false
        }
    }

    pub(super) fn begin(&mut self) -> bool {
        if matches!(self, Self::Waiting { .. }) {
            return false;
        }
        *self = Self::Waiting {
            reason: self.reason().unwrap_or_default().to_string(),
        };
        true
    }

    pub(super) fn succeed(&mut self) {
        *self = Self::Fine;
    }

    pub(super) fn fail(&mut self, error: &Error) {
        *self = Self::Expired {
            reason: self.reason().unwrap_or_default().to_string(),
            failure: Some(detail(error)),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(status: u16) -> Error {
        Error::Status {
            operation: "fetch chats",
            status,
            message: "refused".into(),
        }
    }

    #[test]
    fn only_a_recoverable_lost_sign_in_stops_polling_and_pending_recovery_is_not_restarted() {
        let mut sign_in = SignIn::Fine;
        assert!(!sign_in.note_lost(&refused(500), true));
        assert!(!sign_in.note_lost(&refused(401), false));
        assert!(sign_in.is_online());
        assert!(sign_in.note_lost(&refused(401), true));
        assert!(!sign_in.is_online());
        assert!(sign_in.begin());
        assert!(!sign_in.begin());
        assert!(!sign_in.note_lost(&refused(401), true));
        assert!(matches!(sign_in, SignIn::Waiting { .. }));
    }

    #[test]
    fn the_reason_the_sign_in_was_lost_survives_the_attempts_to_recover_it() {
        let mut sign_in = SignIn::Fine;
        assert!(sign_in.reason().is_none());
        assert!(sign_in.note_lost(&refused(401), true));
        let reason = sign_in.reason().unwrap().to_string();
        assert!(reason.contains("401"), "{reason}");
        assert!(sign_in.begin());
        assert_eq!(sign_in.reason(), Some(reason.as_str()));
        sign_in.fail(&refused(403));
        assert_eq!(sign_in.reason(), Some(reason.as_str()));
        assert!(sign_in.failure().unwrap().contains("403"));
        sign_in.succeed();
        assert!(sign_in.reason().is_none());
    }

    #[test]
    fn a_failure_reason_belongs_only_to_the_failed_attempt() {
        let mut sign_in = SignIn::Waiting {
            reason: String::new(),
        };
        sign_in.fail(&Error::Status {
            operation: "sign in",
            status: 401,
            message: "x".repeat(3000),
        });
        assert_eq!(sign_in.failure().unwrap().chars().count(), MAX_DETAIL);
        assert!(sign_in.begin());
        assert!(sign_in.failure().is_none());
        sign_in.succeed();
        assert!(sign_in.is_online());
        assert!(sign_in.failure().is_none());
    }

    #[test]
    fn what_a_service_said_is_shown_without_control_characters() {
        let mut sign_in = SignIn::Fine;
        let evil = Error::Status {
            operation: "fetch chats",
            status: 401,
            message: "bad \u{1b}[2J \u{202E}text".into(),
        };
        assert!(sign_in.note_lost(&evil, true));
        let shown = sign_in.reason().unwrap();
        assert!(
            !shown.chars().any(|c| c.is_control() || c == '\u{202E}'),
            "{shown:?}"
        );
    }
}
