//! Error types.
//!
//! Each layer owns a small error type ([`StoreError`], [`AuthError`], [`BrowserError`]); [`Error`] is what the
//! backends return and wraps those layers plus the HTTP failures every operation can hit. Variants carry structured
//! fields (a status code, the operation being attempted, the failing path) so callers can decide what to do without
//! parsing messages, and dependency errors are kept as [`std::error::Error::source`] so their types never become
//! part of this crate's API.

use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

/// A dependency's error, kept as a `source` so the dependency's type stays out of this crate's public API.
pub type Source = Box<dyn std::error::Error + Send + Sync>;

/// What the backends return.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A failed backend operation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The request never got an answer: DNS, TLS, connection, timeout or a broken body.
    #[error("{operation}: request failed: {source}")]
    Transport {
        /// What was being attempted, e.g. `"list chats"`.
        operation: &'static str,
        /// The underlying failure.
        #[source]
        source: Source,
    },

    /// The service answered with an error status.
    #[error("{operation}: HTTP {status}: {message}")]
    Status {
        /// What was being attempted, e.g. `"send message"`.
        operation: &'static str,
        /// The HTTP status code (401, 403, 404, 429, ...).
        status: u16,
        /// The service's own explanation, truncated.
        message: String,
    },

    /// The service is throttling us (HTTP 429). Retry later, honouring `retry_after` when it is given.
    #[error("{operation}: throttled by the service{}", retry_hint(*retry_after))]
    Throttled {
        /// What was being attempted.
        operation: &'static str,
        /// How long the service asked us to wait, if it said.
        retry_after: Option<Duration>,
    },

    /// The service answered, but the body could not be read as the expected document.
    #[error("{operation}: unreadable response: {source}")]
    Decode {
        /// What was being attempted.
        operation: &'static str,
        /// The underlying failure.
        #[source]
        source: Source,
    },

    /// The service answered with a well-formed document that lacks something this client needs.
    #[error("{operation}: unexpected response: {detail}")]
    Unexpected {
        /// What was being attempted.
        operation: &'static str,
        /// What was missing or wrong.
        detail: &'static str,
    },

    /// The caller asked for something this backend, or this account's current state, does not support. Not a
    /// failure from the service: nothing was sent, or nothing was sent *because* of this.
    #[error("{operation}: not supported: {detail}")]
    Unsupported {
        /// What was being attempted.
        operation: &'static str,
        /// Why not.
        detail: &'static str,
    },

    /// A response or upload is larger than this client will read, decided locally before (or instead of) finishing
    /// the transfer.
    #[error("{operation}: larger than the {limit_bytes}-byte limit")]
    TooLarge {
        /// What was being attempted.
        operation: &'static str,
        /// The limit that was exceeded.
        limit_bytes: u64,
    },

    /// A request followed more redirects than this client allows before giving up.
    #[error("{operation}: too many redirects")]
    TooManyRedirects {
        /// What was being attempted.
        operation: &'static str,
    },

    /// A URL that would have received credentials is not on the allow-list for them (for instance a paging link
    /// in a response that points somewhere else).
    #[error("refusing to send credentials to {url}")]
    UntrustedUrl {
        /// The rejected URL.
        url: String,
    },

    /// Content from outside the signed-in user's organisation, not loaded automatically (can be loaded explicitly).
    /// A SharePoint attachment whose tenant differs from the signed-in user's, or one whose tenant cannot be verified.
    #[error("{operation}: content from outside your organisation")]
    UnverifiedContent {
        /// What was being attempted, e.g. `"fetch image"`.
        operation: &'static str,
        /// The URL (for logging and explicit-load decisions).
        url: String,
    },

    /// An argument the caller supplied cannot be used.
    #[error("{what} is not usable: {value:?}")]
    InvalidInput {
        /// Which argument, e.g. `"contact id"`.
        what: &'static str,
        /// The offending value.
        value: String,
    },

    /// Getting ready to send failed before anything was written (renewing credentials, building the request), with
    /// the failure inside. Only failures that would otherwise read as "the request may have been delivered" are
    /// wrapped like this (see [`Error::delivery_unknown`]); any other kind of failure is left as it was.
    #[error(transparent)]
    NotSent(Box<Error>),

    /// Signing in, or staying signed in, failed.
    #[error(transparent)]
    Auth(#[from] AuthError),

    /// Driving the browser for sign-in failed.
    #[error(transparent)]
    Browser(#[from] BrowserError),

    /// Reading or writing stored credentials failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl Error {
    /// The HTTP status, when the service answered with one.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => Some(*status),
            Self::NotSent(inner) => inner.status(),
            _ => None,
        }
    }

    /// Mark a failure that happened *before* the request was written (renewing credentials, building headers): the
    /// service never saw it, so it must not read as "may have been delivered". An error that already says the request
    /// was not delivered (a refusal, an authentication failure) is returned as it was.
    #[cfg(any(feature = "graph", feature = "teams"))]
    pub(crate) fn not_sent(self) -> Self {
        if self.delivery_unknown() {
            Self::NotSent(Box::new(self))
        } else {
            self
        }
    }

    /// The error as a line for a log: its usual text, except that what the error was about is left out, because it
    /// can carry identifiers or content (a URL or value, a decoder quoting the document it could not read, a transport
    /// error naming the address). The service's own explanation of a failure is kept, since "HTTP 500" alone says
    /// nothing about why, but with addresses, GUIDs and email addresses in it replaced (it is the service's text about
    /// its own fault, already cleaned and cut to a few hundred characters when the error was made).
    #[must_use]
    pub fn for_log(&self) -> String {
        match self {
            Error::UntrustedUrl { .. } => "refusing to send credentials to an untrusted URL".into(),
            Error::UnverifiedContent { operation, .. } => {
                format!("{operation}: content from outside your organisation")
            }
            Error::InvalidInput { what, .. } => format!("{what} is not usable"),
            Error::Status {
                operation,
                status,
                message,
            } => {
                if message.trim().is_empty() {
                    format!("{operation}: HTTP {status} (the service gave no explanation)")
                } else {
                    format!("{operation}: HTTP {status}: {}", masked(message))
                }
            }
            Error::Decode { operation, .. } => format!("{operation}: unreadable response"),
            Error::Transport { operation, source } => {
                format!(
                    "{operation}: request failed: {}",
                    without_urls(&source.to_string())
                )
            }
            Error::Auth(AuthError::Rejected { code, .. }) => {
                format!("sign-in rejected ({})", short_code(code))
            }
            Error::Auth(AuthError::Expired { code, renewal }) => format!(
                "sign-in expired ({}) and could not be renewed silently: {}",
                short_code(code),
                renewal.for_log()
            ),
            Error::Browser(BrowserError::Protocol { method, .. }) => {
                format!("DevTools {} failed", short_code(method))
            }
            Error::NotSent(inner) => format!("not sent: {}", inner.for_log()),
            other => other.to_string(),
        }
    }

    /// Whether the remedy is for the user to sign in again.
    #[must_use]
    pub fn needs_sign_in(&self) -> bool {
        match self {
            Self::NotSent(inner) => inner.needs_sign_in(),
            Self::Auth(a) => a.needs_sign_in(),
            other => other.status() == Some(401),
        }
    }

    /// For a call that sends something: whether the request may have reached the service even though the call failed
    /// (the connection dropped, the answer could not be read, or the service's own infrastructure failed after
    /// accepting the request, such as a gateway timeout). The caller should check before sending again, because a
    /// retry can duplicate it. A refusal that the service itself issued (a 4xx, or throttling) means it was not
    /// delivered: the service looked at the request and said no before doing anything with it.
    #[must_use]
    pub fn delivery_unknown(&self) -> bool {
        match self {
            Self::Transport { .. }
            | Self::Decode { .. }
            | Self::Unexpected { .. }
            | Self::TooLarge { .. }
            | Self::TooManyRedirects { .. } => true,
            Self::Status { status, .. } => *status >= 500 || *status == 408,
            // Failed before the request was written: it never reached the service.
            Self::NotSent(_) => false,
            Self::Throttled { .. }
            | Self::Unsupported { .. }
            | Self::UntrustedUrl { .. }
            | Self::UnverifiedContent { .. }
            | Self::InvalidInput { .. }
            | Self::Auth(_)
            | Self::Browser(_)
            | Self::Store(_) => false,
        }
    }

    /// Whether trying again later may succeed without anyone doing anything: the network failed, or the service is
    /// throttling or erroring.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Transport { .. } | Self::Throttled { .. } => true,
            Self::Status { status, .. } => *status >= 500,
            Self::NotSent(inner) => inner.is_transient(),
            _ => false,
        }
    }

    #[cfg(any(feature = "graph", feature = "teams"))]
    pub(crate) fn transport(
        operation: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Transport {
            operation,
            source: Box::new(source),
        }
    }

    #[cfg(any(feature = "graph", feature = "teams"))]
    pub(crate) fn decode(
        operation: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Decode {
            operation,
            source: Box::new(source),
        }
    }
}

/// `text` with every address in it replaced by `<url>`: anything containing `://`, and a relative one, which a
/// `Location` or `Link` header or an error message can carry without a host (`/path?code=...`, `</next?page=2>;`, a
/// path starting with `/` or `./`, or any word with a query string).
fn without_urls(text: &str) -> String {
    text.split_whitespace()
        .map(|word| if is_address(word) { "<url>" } else { word })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `word` is an address: one with a scheme, or a relative one (see [`without_urls`]).
fn is_address(word: &str) -> bool {
    if word.contains("://") {
        return true;
    }
    let bare = word.trim_start_matches(['<', '(', '"', '\'']);
    let rooted = bare.len() > 1
        && (bare.starts_with('/') || bare.starts_with("./") || bare.starts_with("../"));
    let query = bare.split_once('?').is_some_and(|(_, q)| q.contains('='));
    rooted || query
}

/// A service's explanation of a failure for a log: single-spaced, with each address replaced by `<url>`, each email
/// address by `<email>` and each GUID (a user, chat or request id in the usual 8-4-4-4-12 form) by `<id>`.
pub(crate) fn masked(text: &str) -> String {
    without_urls(text)
        .split(' ')
        .map(|word| {
            if word.contains('@') && word != "<url>" {
                "<email>".to_string()
            } else {
                mask_guids(word)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `word` with every 8-4-4-4-12 hexadecimal GUID in it replaced by `<id>`.
fn mask_guids(word: &str) -> String {
    let is_guid = |s: &[u8]| {
        s.len() == 36
            && s.iter().enumerate().all(|(i, b)| match i {
                8 | 13 | 18 | 23 => *b == b'-',
                _ => b.is_ascii_hexdigit(),
            })
    };
    let bytes = word.as_bytes();
    let mut out = String::with_capacity(word.len());
    let mut i = 0;
    while i < word.len() {
        if i + 36 <= bytes.len() && is_guid(&bytes[i..i + 36]) {
            out.push_str("<id>");
            i += 36;
        } else if let Some(c) = word[i..].chars().next() {
            out.push(c);
            i += c.len_utf8();
        } else {
            break;
        }
    }
    out
}

/// An error code or command name from the network, safe and short enough for a log line.
fn short_code(code: &str) -> String {
    crate::text::clean(code)
        .chars()
        .filter(|c| !c.is_whitespace())
        .take(40)
        .collect()
}

fn retry_hint(after: Option<Duration>) -> String {
    after.map_or_else(String::new, |d| format!(" (retry in {}s)", d.as_secs()))
}

/// Signing in or renewing a sign-in failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    /// There are no stored credentials, or they do not belong to this configuration.
    #[error("not signed in")]
    NotSignedIn,

    /// The identity provider refused the request (bad or expired grant, consent missing, ...).
    #[error("sign-in rejected ({code}): {description}")]
    Rejected {
        /// The OAuth error code, e.g. `invalid_grant`.
        code: String,
        /// The provider's description, first line only.
        description: String,
    },

    /// The stored sign-in was rejected and renewing it through the browser profile also failed.
    #[error("sign-in expired ({code}) and could not be renewed silently: {renewal}")]
    Expired {
        /// The OAuth error code of the original rejection.
        code: String,
        /// Why the silent renewal failed.
        #[source]
        renewal: Box<Error>,
    },

    /// A silent renewal failed a moment ago, so another is not being attempted yet.
    #[error("silent renewal is paused after a recent failure")]
    RenewalPaused,

    /// A sign-in is waiting in the browser; requests fail at once until it is finished.
    #[error("a sign-in is waiting in the browser")]
    SignInInProgress,

    /// The user did not finish signing in before the deadline.
    #[error("timed out waiting for the sign-in to finish")]
    TimedOut,

    /// The browser profile is signed in as someone other than the stored user.
    #[error("the browser profile is signed in as a different user")]
    WrongAccount,
}

impl AuthError {
    /// Whether the user has to sign in again, as opposed to a misconfiguration or something transient.
    #[must_use]
    pub fn needs_sign_in(&self) -> bool {
        matches!(
            self,
            Self::NotSignedIn | Self::Expired { .. } | Self::WrongAccount
        ) || self.is_grant_problem()
    }

    /// Whether the rejection means the stored grant is no longer good (expired, revoked, consent withdrawn), which a
    /// fresh interactive or silent sign-in can fix. Other rejections, like `invalid_request` or
    /// `unauthorized_client`, are configuration problems that signing in again will not cure.
    #[must_use]
    pub fn is_grant_problem(&self) -> bool {
        matches!(self, Self::Rejected { code, .. } if is_grant_code(code))
    }
}

/// OAuth error codes that mean the stored grant is no longer good.
pub(crate) fn is_grant_code(code: &str) -> bool {
    matches!(
        code,
        "invalid_grant" | "interaction_required" | "login_required" | "consent_required"
    )
}

/// Driving the browser used for the Teams web sign-in failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BrowserError {
    /// No Edge, Chrome or Chromium was found in the usual places.
    #[error("no Edge, Chrome or Chromium installation found")]
    NotFound,

    /// The configured browser executable does not exist.
    #[error("browser not found at {}", .0.display())]
    MissingExecutable(PathBuf),

    /// The browser could not be started.
    #[error("cannot start {}: {source}", path.display())]
    Launch {
        /// The executable.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// The browser exited immediately, typically because another instance already owns the profile.
    #[error("the browser exited at once ({status}); is another window using the profile {}?", profile.display())]
    ExitedAtOnce {
        /// How it exited.
        status: ExitStatus,
        /// The profile directory it was given.
        profile: PathBuf,
    },

    /// The browser started but never opened its DevTools debugging port.
    #[error("the browser did not open a debugging port")]
    NoDebugPort,

    /// The browser was closed (its window shut, or the process ended) before the sign-in finished.
    #[error("the browser was closed before the sign-in finished")]
    Closed,

    /// The profile directory could not be created, or a file in it that must go before a launch could not be removed.
    #[error("cannot prepare the browser profile at {}: {source}", path.display())]
    Profile {
        /// The directory, or the file in it.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// Talking to the browser over DevTools failed.
    #[error("DevTools {action}: {source}")]
    Connection {
        /// What was being attempted, e.g. `"handshake"`.
        action: &'static str,
        /// The underlying failure.
        #[source]
        source: Source,
    },

    /// The browser answered a DevTools command with an error.
    #[error("DevTools {method}: {message}")]
    Protocol {
        /// The command.
        method: String,
        /// The browser's message.
        message: String,
    },
}

/// Reading or writing stored credentials failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The storage location could not be read or written.
    #[error("cannot {action} {}: {source}", path.display())]
    Io {
        /// `"read"`, `"write"`, `"remove"`, ...
        action: &'static str,
        /// The file involved.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// Stored credentials exist but are not in a readable format.
    #[error("stored credentials are unreadable: {source}")]
    Corrupt {
        /// The underlying failure.
        #[source]
        source: Source,
    },

    /// A document this client built itself could not be turned into JSON. Not expected to happen: these are our own
    /// typed values, not something read from disk or the network.
    #[error("could not encode {what} as JSON: {source}")]
    Encode {
        /// What was being encoded, e.g. `"credentials"`.
        what: &'static str,
        /// The underlying failure.
        #[source]
        source: Source,
    },

    /// Stored credentials were written in a format this build does not know.
    #[error(
        "stored credentials use format {found}, but this build reads up to {supported}; update the application or sign in again"
    )]
    UnsupportedVersion {
        /// The version in the stored document.
        found: u32,
        /// The newest version this build reads.
        supported: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io() -> Source {
        Box::new(std::io::Error::other("e"))
    }

    #[test]
    fn a_log_line_never_carries_what_a_decoder_quoted_or_an_address() {
        let secret = "Zaphod-Beeblebrox-9b1c";
        let errors = [
            Error::Decode {
                operation: "read messages",
                source: Box::new(std::io::Error::other(format!(
                    "invalid type: string \"{secret}\""
                ))),
            },
            Error::Transport {
                operation: "read messages",
                source: Box::new(std::io::Error::other(format!(
                    "bad uri: https://example.test/chats/{secret}?x=1"
                ))),
            },
            Error::Auth(AuthError::Rejected {
                code: "invalid_grant".into(),
                description: format!("user {secret} is disabled"),
            }),
            Error::Auth(AuthError::Expired {
                code: "invalid_grant".into(),
                renewal: Box::new(Error::Browser(BrowserError::Protocol {
                    method: "Runtime.evaluate".into(),
                    message: format!("page of {secret} failed"),
                })),
            }),
        ];
        for e in &errors {
            let line = e.for_log();
            assert!(!line.contains(secret), "{line}");
            assert!(!line.contains("example.test/chats"), "{line}");
            assert!(!line.contains('\n'), "{line}");
        }
        assert!(
            errors[2].for_log().contains("invalid_grant"),
            "the code is kept"
        );
        assert!(errors[1].for_log().contains("<url>"));
    }

    #[test]
    fn a_log_line_gives_the_services_own_explanation_with_identifiers_masked() {
        let status = |message: &str| Error::Status {
            operation: "look up names",
            status: 500,
            message: message.to_string(),
        };
        // The wording of a timeout is exactly what makes the line useful.
        assert_eq!(
            status("The operation didn't complete within the allowed timeout of '00:00:10'.")
                .for_log(),
            "look up names: HTTP 500: The operation didn't complete within the allowed timeout of '00:00:10'."
        );
        // Addresses, email addresses and GUIDs in it are not written down.
        let line = status(
            "no such chat 19:abc for ada@contoso.com at https://x.example/a?b=1; user 8:orgid:0a1b2c3d-0000-4000-8000-0123456789ab gone (id=0A1B2C3D-0000-4000-8000-0123456789AB)",
        )
        .for_log();
        assert_eq!(
            line,
            "look up names: HTTP 500: no such chat 19:abc for <email> at <url> user 8:orgid:<id> gone (id=<id>)"
        );
        // A line stays a line, and no explanation is said so. (The length is capped when the error is made, by
        // `http::status_error`, which has a test of its own.)
        assert!(!status("a\nb").for_log().contains('\n'));
        assert_eq!(
            status("  ").for_log(),
            "look up names: HTTP 500 (the service gave no explanation)"
        );
    }
    #[test]
    fn only_failures_that_may_have_reached_the_service_are_delivery_unknown() {
        let status = |status| Error::Status {
            operation: "send",
            status,
            message: String::new(),
        };
        let unknown = [
            Error::Transport {
                operation: "send",
                source: io(),
            },
            Error::Decode {
                operation: "send",
                source: io(),
            },
            Error::Unexpected {
                operation: "send",
                detail: "no message in the answer",
            },
            // The service's own infrastructure can fail after it has accepted the request (a gateway timeout,
            // say), so these are not a refusal: resending can duplicate the message.
            status(500),
            status(502),
            status(503),
            status(504),
            status(408),
        ];
        assert!(unknown.iter().all(Error::delivery_unknown), "{unknown:?}");
        let known_not_delivered = [
            status(403),
            status(404),
            status(409),
            Error::Throttled {
                operation: "send",
                retry_after: None,
            },
            Error::UntrustedUrl { url: String::new() },
            Error::InvalidInput {
                what: "x",
                value: String::new(),
            },
            Error::Auth(AuthError::NotSignedIn),
        ];
        assert!(
            !known_not_delivered.iter().any(Error::delivery_unknown),
            "{known_not_delivered:?}"
        );
    }

    #[cfg(any(feature = "graph", feature = "teams"))]
    #[test]
    fn a_failure_before_the_request_was_written_is_not_delivery_unknown() {
        let status = |status| Error::Status {
            operation: "send",
            status,
            message: String::new(),
        };
        // These would all read as "may have been delivered" if the request had been written.
        for failure in [
            Error::Transport {
                operation: "renew",
                source: io(),
            },
            Error::Decode {
                operation: "renew",
                source: io(),
            },
            Error::Unexpected {
                operation: "renew",
                detail: "no token in the answer",
            },
            status(503),
        ] {
            assert!(failure.delivery_unknown());
            let transient = failure.is_transient();
            let text = failure.to_string();
            let marked = failure.not_sent();
            assert!(matches!(marked, Error::NotSent(_)), "{marked:?}");
            assert!(!marked.delivery_unknown(), "{marked:?}");
            assert_eq!(
                marked.is_transient(),
                transient,
                "retrying is as sensible as before"
            );
            assert_eq!(marked.to_string(), text, "the reason reads as it did");
            assert!(marked.for_log().starts_with("not sent: "));
        }
        // What already says "not delivered" is left exactly as it was.
        for failure in [
            status(401),
            status(403),
            Error::Throttled {
                operation: "renew",
                retry_after: None,
            },
            Error::Auth(AuthError::NotSignedIn),
        ] {
            assert!(matches!(failure.not_sent(), e if !matches!(e, Error::NotSent(_))));
        }
        let marked_401 = status(401).not_sent();
        assert_eq!(marked_401.status(), Some(401));
        assert!(marked_401.needs_sign_in());
    }

    #[test]
    fn classification_helpers_agree_with_each_other() {
        let throttled = Error::Throttled {
            operation: "x",
            retry_after: Some(Duration::from_secs(3)),
        };
        assert!(throttled.is_transient() && !throttled.needs_sign_in());
        assert!(throttled.to_string().contains("retry in 3s"), "{throttled}");
        assert!(
            Error::Status {
                operation: "x",
                status: 503,
                message: String::new()
            }
            .is_transient()
        );
        assert!(
            !Error::Status {
                operation: "x",
                status: 404,
                message: String::new()
            }
            .is_transient()
        );
        assert!(
            Error::Status {
                operation: "x",
                status: 401,
                message: String::new()
            }
            .needs_sign_in()
        );
        let config_problem = Error::Auth(AuthError::Rejected {
            code: "invalid_request".into(),
            description: String::new(),
        });
        assert!(
            !config_problem.needs_sign_in(),
            "signing in again does not fix a misconfiguration"
        );
        let dead_grant = Error::Auth(AuthError::Rejected {
            code: "invalid_grant".into(),
            description: String::new(),
        });
        assert!(dead_grant.needs_sign_in());
    }
}
