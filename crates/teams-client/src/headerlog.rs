//! Opt-in logging of selected *response* headers, for finding out in the field what a service says about its
//! connections (`Connection`, `Keep-Alive`, `Via`, an edge's request reference) when requests fail.
//!
//! Headers can carry credentials, so this is deliberately narrow:
//!
//! - Only response headers are looked at, through a hook on the HTTP agent. Request headers (the bearer token, the
//!   skypetoken) are never read by it.
//! - What is logged is chosen by a [`HeaderLog`] selector, and nothing is logged unless one was asked for.
//! - A header whose name suggests a credential (`authorization`, `set-cookie`, anything with `token`, `secret`,
//!   `password`, `session`, `key`, `csrf`, `sig`, `sas`, ... in it, see `WITHHELD`) is never printed, however it was
//!   selected: the line says `name=<withheld>`, which shows it was sent without showing it.
//! - Every other value is cleaned of control characters, single-spaced, has addresses (with or without a host),
//!   email addresses and GUIDs masked the way error text is for a log, has long opaque words (a token under a name
//!   nobody thought of) replaced, and is cut at a fixed length.

use crate::error::{Error, Result, masked};
use crate::text::clean;
use std::sync::Arc;
use ureq::http::{HeaderMap, Request, Response};
use ureq::middleware::{Middleware, MiddlewareNext};
use ureq::{Body, SendBody};

/// Most entries a selector may have, and the longest one.
const MAX_ENTRIES: usize = 50;
const MAX_ENTRY_CHARS: usize = 64;
/// Longest value logged, and the longest single word of it: a longer word is not a header value anybody reads, it is
/// a token or an id that the name did not warn about.
const MAX_VALUE_CHARS: usize = 200;
const MAX_WORD_CHARS: usize = 40;

/// Parts of a header name that mean its value must not be logged. A part is matched anywhere in the name, so `sig`
/// covers `signature` and `x-ms-sig`, `pass` covers `passcode`, and a name that merely contains one of these is
/// withheld too: a line that says less is the safe mistake.
const WITHHELD: [&str; 21] = [
    "auth",
    "token",
    "secret",
    "password",
    "passwd",
    "pwd",
    "pass",
    "credential",
    "cookie",
    "session",
    "key",
    "csrf",
    "xsrf",
    "jwt",
    "bearer",
    "sig",
    "sas",
    "ticket",
    "nonce",
    "otp",
    "secure",
];

/// Which response headers to log. Built from a selector with [`HeaderLog::parse`] and handed to
/// `TeamsBackend::with_header_log` or `GraphBackend::with_header_log`; every response the backend then receives
/// adds one line at `info` level (target `teams_client::headerlog`), such as
///
/// ```text
/// response headers: GET teams.microsoft.com -> 200 HTTP/1.1: connection=(absent), x-cache=CONFIG_NOCACHE
/// ```
///
/// That is a line for every request, so it is for a diagnostic session, not for leaving on.
///
/// # Selector syntax
///
/// A comma-separated list of header names, case-insensitive. `*` stands for any run of characters (`x-ms-*`,
/// `*cache*`), and a leading `!` excludes what matches (`x-*,!x-ms-latency`). A header is logged if some name selects
/// it and no `!` entry excludes it; at least one name that selects is required. A name without `*` that the response
/// lacks is reported as `(absent)`, because that is often the finding (`connection,keep-alive` on a service that sends
/// neither). Only letters, digits, `-`, `_`, `.` and `*` are allowed in a name.
#[derive(Clone, Debug)]
pub struct HeaderLog {
    include: Vec<String>,
    exclude: Vec<String>,
    /// Every line logged, for the tests (there is no logger to read them from).
    #[cfg(test)]
    captured: Arc<std::sync::Mutex<Vec<String>>>,
}

impl HeaderLog {
    /// Parse a selector (see the type's documentation for the syntax).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidInput`] naming the entry that is not usable, or the selector as a whole when it selects
    /// nothing.
    pub fn parse(selector: &str) -> Result<Self> {
        let (mut include, mut exclude) = (Vec::new(), Vec::new());
        for raw in selector.split(',') {
            let entry = raw.trim();
            if entry.is_empty() {
                continue;
            }
            let (excluded, name) = entry
                .strip_prefix('!')
                .map_or((false, entry), |n| (true, n.trim()));
            let usable = !name.is_empty()
                && name.chars().count() <= MAX_ENTRY_CHARS
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '*'));
            if !usable || include.len() + exclude.len() >= MAX_ENTRIES {
                return Err(Error::InvalidInput {
                    what: "header name to log (letters, digits, - _ . and * only; at most 50 entries)",
                    value: entry.chars().take(MAX_ENTRY_CHARS).collect(),
                });
            }
            let name = name.to_ascii_lowercase();
            if excluded {
                exclude.push(name);
            } else if !include.contains(&name) {
                include.push(name);
            }
        }
        if include.is_empty() {
            return Err(Error::InvalidInput {
                what: "header selector (it names no header to log)",
                value: selector.chars().take(MAX_ENTRY_CHARS).collect(),
            });
        }
        Ok(Self {
            include,
            exclude,
            #[cfg(test)]
            captured: Arc::default(),
        })
    }

    /// Whether the header called `name` (lower case) is one to log.
    fn selects(&self, name: &str) -> bool {
        self.include.iter().any(|p| glob(p, name)) && !self.exclude.iter().any(|p| glob(p, name))
    }

    /// The text of one log line for a response: which request it answers, and the selected headers.
    fn describe(
        &self,
        method: &str,
        host: &str,
        status: u16,
        version: &str,
        headers: &HeaderMap,
    ) -> String {
        let mut items: Vec<String> = Vec::new();
        for (name, value) in headers {
            let name = name.as_str();
            if !self.selects(name) {
                continue;
            }
            if WITHHELD.iter().any(|part| name.contains(part)) {
                items.push(format!("{name}=<withheld>"));
            } else {
                items.push(format!(
                    "{name}={}",
                    render(&String::from_utf8_lossy(value.as_bytes()))
                ));
            }
        }
        for name in &self.include {
            let named = !name.contains('*') && !self.exclude.iter().any(|p| glob(p, name));
            if named && !headers.contains_key(name.as_str()) {
                items.push(format!("{name}=(absent)"));
            }
        }
        let shown = if items.is_empty() {
            "none of the selected headers are present".to_string()
        } else {
            items.join(", ")
        };
        format!("{method} {host} -> {status} {version}: {shown}")
    }

    /// The hook to put on an agent: logs the selected headers of each response, and changes nothing else.
    pub(crate) fn layer(log: &Arc<Self>) -> impl Middleware {
        Logging(Arc::clone(log))
    }

    /// Every line logged so far.
    #[cfg(test)]
    pub(crate) fn captured(&self) -> Vec<String> {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

struct Logging(Arc<HeaderLog>);

impl Middleware for Logging {
    fn handle(
        &self,
        request: Request<SendBody<'_>>,
        next: MiddlewareNext<'_>,
    ) -> std::result::Result<Response<Body>, ureq::Error> {
        // Only the method and the host of the request are read, never its headers or its address beyond the host.
        let method = request.method().as_str().to_string();
        let host = request.uri().host().unwrap_or("?").to_string();
        let response = next.handle(request)?;
        let line = self.0.describe(
            &method,
            &host,
            response.status().as_u16(),
            &format!("{:?}", response.version()),
            response.headers(),
        );
        log::info!("response headers: {line}");
        #[cfg(test)]
        self.0
            .captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(line);
        Ok(response)
    }
}

/// A header value made safe for a log line: no control characters, one line, addresses, email addresses and GUIDs
/// masked, no word long enough to be a token, and a length limit.
fn render(value: &str) -> String {
    let text = masked(&clean(value))
        .split(' ')
        .map(|word| {
            if word.chars().count() > MAX_WORD_CHARS {
                "<long value>"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        "(empty)".to_string()
    } else if text.chars().count() > MAX_VALUE_CHARS {
        format!(
            "{}…",
            text.chars().take(MAX_VALUE_CHARS).collect::<String>()
        )
    } else {
        text
    }
}

/// Whether `text` matches `pattern`, where `*` in the pattern stands for any run of characters (none included).
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut resume) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            resume = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ureq::http::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_bytes(value.as_bytes()).unwrap(),
            );
        }
        map
    }

    fn line(selector: &str, pairs: &[(&str, &str)]) -> String {
        HeaderLog::parse(selector).unwrap().describe(
            "GET",
            "teams.microsoft.com",
            200,
            "HTTP/1.1",
            &headers(pairs),
        )
    }

    #[test]
    fn a_selector_is_names_with_wildcards_and_exclusions_in_any_case() {
        let log = HeaderLog::parse(" Connection , X-MS-* ,!x-ms-latency, keep-alive,,connection ")
            .unwrap();
        for selected in ["connection", "keep-alive", "x-ms-ref", "x-ms-", "x-ms-a-b"] {
            assert!(log.selects(selected), "{selected}");
        }
        for not_selected in [
            "x-ms-latency",
            "date",
            "x-msx",
            "connections",
            "content-length",
        ] {
            assert!(!log.selects(not_selected), "{not_selected}");
        }
        assert!(HeaderLog::parse("*").unwrap().selects("anything"));
        assert!(HeaderLog::parse("*cache*").unwrap().selects("x-cache"));
        assert!(HeaderLog::parse("a*b*c").unwrap().selects("a-xb-yc"));
        assert!(!HeaderLog::parse("a*b*c").unwrap().selects("a-xb-y"));
    }

    #[test]
    fn a_selector_that_selects_nothing_or_names_nonsense_is_refused() {
        for bad in [
            "",
            " , ,",
            "!set-cookie",
            "x y",
            "a:b",
            "x/y",
            "é",
            &"a".repeat(65),
        ] {
            assert!(
                matches!(HeaderLog::parse(bad), Err(Error::InvalidInput { .. })),
                "{bad:?}"
            );
        }
        let many = (0..51)
            .map(|i| format!("h{i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(HeaderLog::parse(&many).is_err(), "too many entries");
        assert!(
            HeaderLog::parse(
                &(0..50)
                    .map(|i| format!("h{i}"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
            .is_ok()
        );
    }

    #[test]
    fn the_line_says_which_response_it_is_and_lists_what_was_selected() {
        assert_eq!(
            line(
                "x-cache,date",
                &[
                    ("X-Cache", "CONFIG_NOCACHE"),
                    ("Date", "Wed, 18 Nov 2026 23:29:37 GMT"),
                    ("Server", "x")
                ]
            ),
            "GET teams.microsoft.com -> 200 HTTP/1.1: x-cache=CONFIG_NOCACHE, date=Wed, 18 Nov 2026 23:29:37 GMT"
        );
    }

    /// The absence of `Connection` and `Keep-Alive` is the finding that prompted this, so it has to show.
    #[test]
    fn a_named_header_the_response_lacks_is_reported_absent_and_a_wildcard_is_not() {
        let shown = line("connection,keep-alive,x-ms-*,!keep-alive", &[("Date", "d")]);
        assert!(shown.ends_with(": connection=(absent)"), "{shown}");
        assert!(
            line("x-ms-*", &[("Date", "d")]).ends_with("none of the selected headers are present")
        );
    }

    #[test]
    fn credentials_are_never_printed_however_they_were_selected() {
        let secrets = [
            ("Set-Cookie", "session=SECRET-COOKIE-VALUE; Path=/"),
            ("Authorization", "Bearer SECRET-BEARER"),
            ("Authentication", "skypetoken=SECRET-SKYPE"),
            ("Proxy-Authenticate", "Basic realm=SECRET-REALM"),
            ("WWW-Authenticate", "Bearer error=SECRET-ERROR"),
            ("X-Ms-Refresh-Token", "SECRET-REFRESH"),
            ("X-Client-Secret", "SECRET-SECRET"),
            ("X-Api-Key", "SECRET-KEY"),
            ("X-Session-Id", "SECRET-SESSION"),
            ("X-Password-Hint", "SECRET-PASSWORD"),
            // One for each of the other parts of a name that mean a credential, with no other part in the name, so
            // dropping any one of them from `WITHHELD` shows here.
            ("X-Csrf-Value", "SECRET-CSRF"),
            ("X-Xsrf-Value", "SECRET-XSRF"),
            ("X-Jwt-Value", "SECRET-JWT"),
            ("X-Bearer-Value", "SECRET-BEARER2"),
            ("X-Sig-Value", "SECRET-SIG"),
            ("X-Sas-Value", "SECRET-SAS"),
            ("X-Ticket-Value", "SECRET-TICKET"),
            ("X-Nonce-Value", "SECRET-NONCE"),
            ("X-Pass-Value", "SECRET-PASS"),
            ("X-Pwd-Value", "SECRET-PWD"),
            ("X-Otp-Value", "SECRET-OTP"),
            ("X-Secure-Value", "SECRET-SECURE"),
        ];
        for selector in [
            "*",
            "set-cookie,authorization,authentication,proxy-authenticate,www-authenticate,x-*",
        ] {
            let shown = line(selector, &secrets);
            assert!(!shown.contains("SECRET"), "{selector}: {shown}");
            assert_eq!(
                shown.matches("=<withheld>").count(),
                secrets.len(),
                "{selector}: {shown}"
            );
        }
        // Withheld means the value, not the fact that the header came: an operator can still see it did.
        assert!(line("set-cookie", &[("Set-Cookie", "x=1")]).contains("set-cookie=<withheld>"));
    }

    #[test]
    fn values_are_one_clean_masked_line_of_bounded_length() {
        let shown = line(
            "x-note",
            &[(
                "X-Note",
                "see https://login.example/path?code=SECRET1 mail bob@example.com id 0123abcd-0123-4567-89ab-0123456789ab  \t eyJhbGciOiJSUzI1NiIsInR5cCI6IkpXVCJ9.payload.signature-padding-to-be-long",
            )],
        );
        for leaked in ["SECRET1", "login.example", "bob@", "0123abcd", "eyJhbGci"] {
            assert!(!shown.contains(leaked), "{leaked}: {shown}");
        }
        assert!(
            shown.contains("<url>") && shown.contains("<email>") && shown.contains("<id>"),
            "{shown}"
        );
        assert!(shown.contains("<long value>"), "{shown}");
        assert!(!shown.contains("  ") && !shown.contains('\t'), "{shown}");
        // Text that disguises what is read (a bidi override, a tab) cannot reach whoever tails the log, and a long
        // value is cut. The `http` crate itself refuses an ESC or other control byte in a value, so none can arrive.
        let evil = line("x-note", &[("X-Note", "a\tb\u{202e}c")]);
        assert!(
            !evil.contains('\u{202e}') && !evil.contains('\t'),
            "{evil:?}"
        );
        let long = line("x-note", &[("X-Note", &"word ".repeat(100))]);
        assert!(long.chars().count() < 300 && long.ends_with('…'), "{long}");
        assert!(line("x-note", &[("X-Note", "")]).ends_with("x-note=(empty)"));
    }

    /// A `Location` or `Link` header often has no host, only a path and a query that can hold a code or a signature.
    #[test]
    fn a_relative_address_in_a_value_is_masked_like_a_full_one() {
        let shown = line(
            "location,link,x-next,x-rel,x-dot",
            &[
                ("Location", "/signin/callback?code=SECRET1&state=x"),
                ("Link", "</items?page=SECRET2>; rel=\"next\""),
                ("X-Next", "go to /a/SECRET3 now"),
                ("X-Rel", "a/b?c=SECRET4"),
                ("X-Dot", "../up/SECRET5"),
            ],
        );
        assert!(!shown.contains("SECRET"), "{shown}");
        assert_eq!(shown.matches("<url>").count(), 5, "{shown}");
        // Ordinary words with a slash or a question mark in them are not addresses.
        let plain = line(
            "x-type,x-ask",
            &[
                ("X-Type", "text/html; charset=utf-8"),
                ("X-Ask", "what? no"),
            ],
        );
        assert!(
            plain.contains("text/html") && plain.contains("what?"),
            "{plain}"
        );
    }

    #[test]
    fn a_header_sent_twice_is_listed_twice_and_a_value_that_is_not_text_does_not_break_the_line() {
        let mut map = headers(&[("Via", "1.1 a"), ("Via", "1.1 b")]);
        map.append(
            HeaderName::from_static("x-binary"),
            HeaderValue::from_bytes(&[0xff, 0xfe, b'o', b'k']).unwrap(),
        );
        let shown = HeaderLog::parse("via,x-binary")
            .unwrap()
            .describe("GET", "h", 200, "HTTP/1.1", &map);
        assert!(shown.contains("via=1.1 a, via=1.1 b"), "{shown}");
        assert!(shown.contains("x-binary="), "{shown}");
    }
}
