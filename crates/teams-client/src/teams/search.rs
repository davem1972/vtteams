//! Searching the messages of one chat, or of several conversations at once (the channels of a team), on the server,
//! through the Substrate search service. See `docs/teams-web-protocol.md` ("Message search"): the request and the
//! answer's shape come from other open-source clients and were checked against a live tenant only in part (what was
//! and was not is listed there), and a read-only live probe is how to check the rest.
//!
//! What a person types is only ever the text of one quoted phrase, from which every double quote and backslash has been
//! taken out (they separate words, because that the service honours a backslash escape is not verified), inside a
//! query whose scope clause (these conversations, and not a deleted message) is fixed text of ours. A phrase cannot add
//! an operator, name another conversation or end its own quotes (see the tests, which read the query back the way the
//! service's language would). The conversation ids of a scope come from the caller and each is checked the same way
//! before it is put between quotes: one that could end them refuses the whole call. A hit the service labels as
//! belonging to a conversation outside the scope is left out. Nothing about the query, or about a hit's text, sender,
//! channel or conversation, reaches the log: only counts and timings.

use super::{Call, Headers, Parsed, TeamsBackend, guid, http, trusted_url};
use crate::error::{Error, Result};
use crate::model::{MAX_SEARCH_CONVERSATIONS, SearchHit, SearchPage};
use crate::text::{clean, id_tag};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{Value, json};
use std::time::Instant;

/// Where the search is posted. A single constant so that the other spelling the service is also said to answer on
/// (`/search/api/v2/query`) is a one-line change if a tenant needs it.
pub(super) const SEARCH_URL: &str = "https://substrate.office.com/searchservice/api/v2/query";

/// The field that limits a search to one conversation. Another client is said to use `clientconversationid` for the
/// same thing; only this one has been reported working against a live service.
const THREAD_FIELD: &str = "ClientThreadId";

/// What one request asks for: a page of this many hits (the service's limits differ between clients; 25 is safe).
const PAGE_SIZE: usize = 25;

/// The longest phrase and the longest chat id accepted, in characters.
const MAX_QUERY_CHARS: usize = 200;
const MAX_CHAT_ID_CHARS: usize = 200;

/// The longest preview and sender name kept, in characters.
const MAX_PREVIEW_CHARS: usize = 300;
const MAX_SENDER_CHARS: usize = 100;
const MAX_CHANNEL_CHARS: usize = 80;

const OPERATION: &str = "search messages";

/// What the person typed as the one phrase to look for. Whitespace (tabs and line breaks too) **and the double quote
/// and the backslash** separate words, and runs of them shrink to one space: that the service reads a backslash escape
/// inside a quoted phrase is not verified, so nothing is left to depend on it, at the cost of an exact search for
/// punctuation that is one of those two characters. Refused (and nothing sent) when nothing is left, when what is left
/// is longer than [`MAX_QUERY_CHARS`], or when it holds a control character that is not whitespace (a stray escape or
/// NUL says the text did not come from typing). The error names the reason, never the text.
fn normalise_phrase(query: &str) -> Result<String> {
    let invalid = |reason: &str| Error::InvalidInput {
        what: "search text",
        value: reason.to_string(),
    };
    let mut out = String::new();
    let mut length = 0usize;
    let mut pending_space = false;
    for c in query.chars() {
        if c.is_whitespace() || c == '"' || c == '\\' {
            pending_space = !out.is_empty();
            continue;
        }
        if c.is_control() {
            return Err(invalid("has a control character"));
        }
        if pending_space {
            out.push(' ');
            length += 1;
            pending_space = false;
        }
        out.push(c);
        length += 1;
        if length > MAX_QUERY_CHARS {
            return Err(invalid("too long"));
        }
    }
    if out.is_empty() {
        return Err(invalid("empty"));
    }
    Ok(out)
}

/// A chat id as it is put between quotes in the scope clause: refused when it could end them.
fn check_chat_id(chat_id: &str) -> Result<()> {
    let reason = if chat_id.is_empty() {
        "empty"
    } else if chat_id.chars().count() > MAX_CHAT_ID_CHARS {
        "too long"
    } else if chat_id
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        "has a quote, a backslash or a control character"
    } else {
        return Ok(());
    };
    Err(Error::InvalidInput {
        what: "chat id",
        value: reason.to_string(),
    })
}

/// The conversations a search covers, from what the caller gave: each id checked ([`check_chat_id`], all of them, so a
/// bad one anywhere refuses the call, even past the cap), none given refused, an id repeated (ASCII case aside) kept
/// once in the spelling of its first occurrence, and no more than [`MAX_SEARCH_CONVERSATIONS`] of them, the first ones.
fn search_scope_ids(ids: &[String]) -> Result<Vec<String>> {
    if ids.is_empty() {
        return Err(Error::InvalidInput {
            what: "chat id",
            value: "none given".to_string(),
        });
    }
    for id in ids {
        check_chat_id(id)?;
    }
    let mut scope: Vec<String> = Vec::new();
    for id in ids {
        if scope.len() == MAX_SEARCH_CONVERSATIONS {
            break;
        }
        if !scope.iter().any(|kept| kept.eq_ignore_ascii_case(id)) {
            scope.push(id.clone());
        }
    }
    Ok(scope)
}

/// `text` as one quoted string, its backslashes and quotes escaped so that nothing in it can end the quotes. Belt and
/// braces: [`normalise_phrase`] leaves neither character in what it returns, so there is nothing to escape in practice,
/// and a phrase that reached here some other way would still be one string if the service reads the escapes.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The query string: these conversations (one, or an OR of several in parentheses, which the service answers: seen
/// live with seven channels), this phrase, not a deleted message. Each of `scope` has been through [`check_chat_id`] (see
/// [`search_scope_ids`]) and `phrase` through [`normalise_phrase`].
fn query_string(scope: &[String], phrase: &str) -> String {
    let clause = |id: &String| format!("{THREAD_FIELD}:\"{id}\"");
    let conversations = match scope {
        [one] => clause(one),
        many => format!(
            "({})",
            many.iter().map(clause).collect::<Vec<_>>().join(" OR ")
        ),
    };
    format!(
        "{conversations} AND {} AND NOT (isClientSoftDeleted:TRUE)",
        quoted(phrase)
    )
}

/// The request body for one page, starting `from` results in.
fn request_body(scope: &[String], phrase: &str, from: usize) -> Value {
    json!({
        "cvid": guid(),
        "scenario": { "Name": "powerbar" },
        "EntityRequests": [{
            "entityType": "Message",
            "contentSources": ["Teams"],
            "fields": [
                "Extension_SkypeSpaces_ConversationPost_Extension_FromSkypeInternalId_String",
                "Extension_SkypeSpaces_ConversationPost_Extension_ThreadType_String"
            ],
            "propertySet": "Optimized",
            "query": {
                "queryString": query_string(scope, phrase),
                // For display only, but quoted and escaped all the same, in case the service reads it.
                "displayQueryString": quoted(phrase)
            },
            "from": from,
            "size": PAGE_SIZE
        }]
    })
}

/// Whether a flag the service sends is set (a boolean, or the word as text).
fn is_true(v: &Value) -> bool {
    v.as_bool()
        .unwrap_or_else(|| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("true")))
}

/// Text from the service as one short line: no private-use characters (the service wraps the matched words in two of
/// them, and this crate's callers use others for their own markers), no controls, runs of whitespace as one space.
fn one_line(text: &str, max_chars: usize) -> String {
    let kept: String = text
        .chars()
        .filter(|c| !matches!(c, '\u{E000}'..='\u{F8FF}'))
        .collect();
    clean(&kept)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

/// `text` without the `<c0>`..`<c9>` and `</c0>`..`</c9>` tags a highlighted summary wraps matched words in.
fn without_highlight_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let tag_len = ["<c", "</c"].iter().find_map(|open| {
            let mut after = tail.strip_prefix(open)?.chars();
            let digit = after.next()?;
            (digit.is_ascii_digit() && after.next() == Some('>')).then_some(open.len() + 2)
        });
        match tag_len {
            Some(n) => rest = &tail[n..],
            None => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The message id (digits, the form the rest of this crate uses) from `InternetMessageId`, which one client's
/// fixtures have as a number and another's as a string.
fn message_id(v: &Value) -> Option<String> {
    let id = match v {
        Value::Number(n) => n.as_u64()?.to_string(),
        Value::String(s) => s.clone(),
        _ => return None,
    };
    (!id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

/// When the message was sent: ISO 8601, with or without an offset (none means UTC).
fn sent_time(v: &Value) -> Option<DateTime<Utc>> {
    let s = v.as_str()?;
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|n| n.and_utc())
        })
}

/// Who sent it, from `From` or `Sender`, whichever has a name: a person's `EmailAddress.Name`, a bare `Name` or
/// `DisplayName`, or the text itself. Empty when the service says nothing.
fn sender_name(source: &Value) -> String {
    ["From", "Sender"]
        .iter()
        .find_map(|key| {
            let node = &source[*key];
            let name = node["EmailAddress"]["Name"]
                .as_str()
                .or_else(|| node["Name"].as_str())
                .or_else(|| node["DisplayName"].as_str())
                .or_else(|| node.as_str())?;
            let name = one_line(name, MAX_SENDER_CHARS);
            (!name.is_empty()).then_some(name)
        })
        .unwrap_or_default()
}

/// The chat a result says it belongs to: `Source.ClientThreadId`, else `Source.ClientConversationId` (which may carry a
/// `;messageid=...` suffix, cut off here). `None` when it says neither.
fn result_chat(source: &Value) -> Option<&str> {
    ["ClientThreadId", "ClientConversationId"]
        .iter()
        .find_map(|key| {
            let id = source[*key].as_str()?.split(';').next()?;
            (!id.is_empty()).then_some(id)
        })
}

/// One result -> a hit. A result without a usable message id or time is unreadable (counted by the caller), not
/// fatal: the rest of the page is still good. A result that names a conversation outside `scope` (ASCII case aside) is
/// `Skipped`, whatever else it holds: the query asked for those conversations, so the service's answer is not trusted
/// to have kept to them. One that names none cannot be checked: with one conversation in the scope it is kept, and is
/// in that one; with several, which of them it is cannot be known and it is unreadable for this scope (`Malformed`).
/// The hit's conversation is the scope's own spelling of the id. `Id` is the search hit's own id and says nothing about
/// the message.
fn parse_hit(result: &Value, scope: &[String]) -> Parsed<SearchHit> {
    let source = &result["Source"];
    if !source.is_object() {
        return Parsed::Malformed;
    }
    let conversation = match (result_chat(source), scope) {
        (Some(label), _) => match scope.iter().find(|id| id.eq_ignore_ascii_case(label)) {
            Some(id) => id.clone(),
            None => return Parsed::Skipped,
        },
        (None, [only]) => only.clone(),
        (None, _) => return Parsed::Malformed,
    };
    let (Some(message_id), Some(sent)) = (
        message_id(&source["InternetMessageId"]),
        sent_time(&source["DateTimeSent"]),
    ) else {
        return Parsed::Malformed;
    };
    let preview = [
        &source["Preview"],
        &result["HitHighlightedSummary"],
        &source["HitHighlightedSummary"],
    ]
    .iter()
    .filter_map(|v| v.as_str())
    .map(|text| one_line(&without_highlight_tags(text), MAX_PREVIEW_CHARS))
    .find(|text| !text.is_empty())
    .unwrap_or_default();
    Parsed::Item(SearchHit {
        message_id,
        sent,
        sender: sender_name(source),
        preview,
        conversation,
        channel: source["ChannelName"]
            .as_str()
            .map(|name| one_line(name, MAX_CHANNEL_CHARS))
            .unwrap_or_default(),
    })
}

/// The one result set a search asks for, out of the answer, after checking that the service did not say it could not
/// read the query (which would look like "no hits" and be wrong).
fn result_set(answer: &Value) -> Result<&Value> {
    let unexpected = |detail| Error::Unexpected {
        operation: OPERATION,
        detail,
    };
    let sets = answer["EntitySets"]
        .as_array()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| unexpected("no entity sets in the answer"))?;
    if is_true(&answer["Properties"]["HasParseException"]) {
        return Err(unexpected("the service could not read the query"));
    }
    for set in sets {
        // `IsPartial` is NOT an error: the service sets it on ordinary answers (live, 2026-11-19: true on a search
        // that found its message and on one that found nothing, with no parse exception), so reading it as "answered
        // only in part" made every search fail. What would look like "no hits" and be wrong is a query it could not
        // read, which is checked here.
        if is_true(&set["Properties"]["HasParseException"]) {
            return Err(unexpected("the service could not read the query"));
        }
        for rs in set["ResultSets"].as_array().into_iter().flatten() {
            if is_true(&rs["Properties"]["HasParseException"]) {
                return Err(unexpected("the service could not read the query"));
            }
        }
    }
    sets[0]["ResultSets"]
        .as_array()
        .and_then(|rs| rs.first())
        .ok_or_else(|| unexpected("no result set in the answer"))
}

/// A count the service gives as a number or as numeric text.
fn count(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}

impl TeamsBackend {
    /// Search one chat for `query`, a page of results starting `from` results in. The query is made into one quoted
    /// phrase (see the module notes); the Substrate token only ever goes to Substrate (or, in tests, a local stand-in).
    ///
    /// Results are in the service's own order (relevance), and a message sent a moment ago may not be indexed yet.
    pub(super) fn search_chat(
        &self,
        chat_id: &str,
        query: &str,
        from: usize,
    ) -> Result<SearchPage> {
        self.search_scope(std::slice::from_ref(&chat_id.to_string()), query, from)
    }

    /// Search several conversations (a team's channels, say) for `query`: the same call as [`Self::search_chat`] over
    /// an OR of the conversations (see [`search_scope_ids`] for what is refused, dropped and capped), the hits each
    /// naming its conversation. Seen live: seven channels, one answer, newest first.
    pub(super) fn search_scope(
        &self,
        conversation_ids: &[String],
        query: &str,
        from: usize,
    ) -> Result<SearchPage> {
        let scope = search_scope_ids(conversation_ids)?;
        let phrase = normalise_phrase(query)?;
        if !(self.search_url.starts_with("https://substrate.office.com/")
            || trusted_url(&self.search_url))
        {
            return Err(Error::UntrustedUrl {
                url: self.search_url.clone(),
            });
        }
        let body = request_body(&scope, &phrase, from);
        let call = Call {
            agent: &self.api,
            recorder: self.recorder.as_deref(),
            operation: OPERATION,
            method: "POST",
            url: &self.search_url,
            body: Some(&body),
        };
        let started = Instant::now();
        let answer = http::call_json(&call, &mut |renewed| -> Result<Headers> {
            Ok(vec![
                (
                    "Authorization",
                    format!("Bearer {}", self.substrate_token(renewed)?),
                ),
                ("Origin", "https://teams.microsoft.com".to_string()),
                ("Referer", "https://teams.microsoft.com/".to_string()),
                ("Accept", "application/json".to_string()),
            ])
        })?;
        self.watch_search(&answer);
        let set = result_set(&answer)?;
        let results: &[Value] = match set["Results"].as_array() {
            Some(results) => results,
            // A result set with nothing in it may leave the list out, but only one that says so (a total of 0) is
            // believed: an answer that has neither is not the shape this reads.
            None if count(&set["Total"]) == Some(0) => &[],
            None => {
                return Err(Error::Unexpected {
                    operation: OPERATION,
                    detail: "no results in the answer",
                });
            }
        };
        // Results the service labels as another conversation's are counted as they are left out (`Parsed::Skipped` is
        // only ever that here).
        let other_chat = std::cell::Cell::new(0usize);
        let hits = self.collect(OPERATION, results, |result| {
            let parsed = parse_hit(result, &scope);
            if matches!(parsed, Parsed::Skipped) {
                other_chat.set(other_chat.get() + 1);
            }
            parsed
        });
        let other_chat = other_chat.get();
        if other_chat > 0 {
            // The count is in the log line below: a warning's text is fixed.
            self.auth.warn(Error::Unexpected {
                operation: OPERATION,
                detail: "some search results belonged to another chat and were left out",
            });
        }
        let page = SearchPage {
            next: (is_true(&set["MoreResultsAvailable"]) && !results.is_empty())
                .then(|| from.saturating_add(results.len())),
            total: count(&set["Total"]),
            hits,
        };
        // The one conversation of a single-conversation search is named (as a tag); of several, only how many.
        let covered = match scope.as_slice() {
            [only] => format!("chat {}", id_tag(only)),
            many => format!("{} conversation(s)", many.len()),
        };
        log::debug!(
            "{OPERATION}: {covered} from {from}: {} result(s), {} read, {other_chat} from another chat left out, about {} in all, more: {}, {} ms",
            results.len(),
            page.hits.len(),
            page.total
                .map_or_else(|| "(not said)".to_string(), |t| t.to_string()),
            page.next.is_some(),
            started.elapsed().as_millis()
        );
        Ok(page)
    }

    /// Note the structure of a search answer (names and types only), so that a service that starts sending something
    /// new shows up in the log.
    fn watch_search(&self, answer: &Value) {
        let sets: Vec<&Value> = answer["EntitySets"]
            .as_array()
            .into_iter()
            .flatten()
            .collect();
        let result_sets: Vec<&Value> = sets
            .iter()
            .flat_map(|s| s["ResultSets"].as_array().into_iter().flatten())
            .collect();
        let results: Vec<&Value> = result_sets
            .iter()
            .flat_map(|rs| rs["Results"].as_array().into_iter().flatten())
            .collect();
        self.watch.fields("search answer", [answer]);
        self.watch.fields("search entity set", sets.iter().copied());
        self.watch
            .fields("search result set", result_sets.iter().copied());
        self.watch.fields("search result", results.iter().copied());
        self.watch
            .fields("search result source", results.iter().map(|r| &r["Source"]));
    }
}

#[cfg(test)]
mod tests {
    use super::super::auth::TeamsAuth;
    use super::super::tests::{ADA, ME, backend};
    use super::super::{BrowserOptions, TeamsSession, lock, now};
    use super::*;
    use crate::error::AuthError;
    use crate::model::Backend;
    use crate::store::MemoryStore;
    use crate::testserver::{Reply, Server, serve};
    use std::sync::Arc;

    const CHAT: &str = "19:abc123@thread.v2";

    /// A backend whose Substrate token is fresh and whose search is `server`'s.
    fn searcher(server: &Server) -> TeamsBackend {
        let mut b = backend(server);
        b.search_url = server.url("/searchservice/api/v2/query");
        *lock(&b.substrate) = Some(("substrate-token".into(), now() + 3600));
        b
    }

    /// An answer holding `results`.
    fn answer(results: &[Value], total: u64, more: bool) -> String {
        json!({"EntitySets": [{"ResultSets": [
            {"Total": total, "MoreResultsAvailable": more, "Results": results}
        ]}]})
        .to_string()
    }

    fn hit(id: Value, sent: &str) -> Value {
        json!({"Id": "search-hit-id", "Source": {
            "InternetMessageId": id, "DateTimeSent": sent, "Preview": "see you at noon",
            "From": {"EmailAddress": {"Name": "Ada Lovelace", "Address": "ada@x.test"}}}})
    }

    /// What a query language with quoted strings and backslash escapes reads out of `q`: the text outside every quoted
    /// string, each quoted string replaced by `#`, and the strings themselves, unescaped. A string left open panics.
    fn lex(q: &str) -> (String, Vec<String>) {
        let (mut outside, mut strings) = (String::new(), Vec::new());
        let mut chars = q.chars();
        while let Some(c) = chars.next() {
            if c != '"' {
                outside.push(c);
                continue;
            }
            let mut s = String::new();
            loop {
                match chars.next().expect("a quoted string that never ends") {
                    '\\' => s.push(chars.next().expect("an escape with nothing after it")),
                    '"' => break,
                    other => s.push(other),
                }
            }
            outside.push('#');
            strings.push(s);
        }
        (outside, strings)
    }

    const SKELETON: &str = "ClientThreadId:# AND # AND NOT (isClientSoftDeleted:TRUE)";

    fn hostile_phrases() -> Vec<String> {
        let mut phrases: Vec<String> = [
            "plain words",
            "x\" OR ClientThreadId:\"other",
            "\\\" OR 1",
            "\\",
            "\"",
            "a\\\\\"b",
            "OR",
            "AND NOT",
            "NOT",
            "*",
            "wild*card?",
            "(a OR b)",
            ") OR (",
            "ClientThreadId:\"someone-elses-chat\"",
            "ClientThreadId:other",
            "isClientSoftDeleted:TRUE",
            "a:b:c",
            "日本語 émoji 🎉 \u{202E}bidi",
            "tab\tand\nnewline",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        phrases.push("\"".repeat(200));
        phrases.push("\\".repeat(200));
        phrases.push("\\\"".repeat(100));
        phrases.push("\" OR \"".repeat(25));
        // A fixed pseudo-random walk over the characters that matter.
        let alphabet: Vec<char> = "\"\\ ()*:ORANDT\u{e9}\u{1F389}-_".chars().collect();
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..300 {
            let mut s = String::new();
            for _ in 0..(1 + state % 60) {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                s.push(alphabet[(state % alphabet.len() as u64) as usize]);
            }
            phrases.push(s);
        }
        phrases
    }

    #[test]
    fn no_phrase_can_change_the_scope_or_leave_its_quotes() {
        // Words are what is left after whitespace, quotes and backslashes are taken as separators.
        let words = |s: &str| {
            s.split(|c: char| c.is_whitespace() || c == '"' || c == '\\')
                .filter(|w| !w.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut refused = 0;
        for raw in hostile_phrases() {
            let Ok(phrase) = normalise_phrase(&raw) else {
                // Only what is nothing but quotes, backslashes and whitespace is refused.
                assert_eq!(words(&raw), "", "refused though it has words: {raw:?}");
                refused += 1;
                continue;
            };
            let body = request_body(&[CHAT.to_string()], &phrase, 0);
            let request = &body["EntityRequests"][0]["query"];
            let (outside, strings) = lex(request["queryString"].as_str().unwrap());
            assert_eq!(outside, SKELETON, "for {raw:?}");
            assert_eq!(strings, [CHAT.to_string(), phrase.clone()], "for {raw:?}");
            let (outside, strings) = lex(request["displayQueryString"].as_str().unwrap());
            assert_eq!(outside, "#", "for {raw:?}");
            assert_eq!(strings, std::slice::from_ref(&phrase), "for {raw:?}");
            // The person's words, and nothing of theirs that could end a string.
            assert_eq!(phrase, words(&raw), "for {raw:?}");
        }
        assert!(
            refused >= 3,
            "the lists hold phrases of nothing but quotes and backslashes"
        );
    }

    #[test]
    fn no_quote_or_backslash_of_the_persons_ever_reaches_the_query() {
        for raw in hostile_phrases() {
            let Ok(phrase) = normalise_phrase(&raw) else {
                continue;
            };
            assert!(!phrase.contains('"') && !phrase.contains('\\'), "{raw:?}");
            let body = request_body(&[CHAT.to_string()], &phrase, 0);
            let query = &body["EntityRequests"][0]["query"];
            let q = query["queryString"].as_str().unwrap();
            // Exactly the four quotes of the two strings we write, and no backslash at all.
            assert_eq!(q.matches('"').count(), 4, "{raw:?}");
            assert!(!q.contains('\\'), "{raw:?}");
            let display = query["displayQueryString"].as_str().unwrap();
            assert_eq!(display.matches('"').count(), 2, "{raw:?}");
            assert!(!display.contains('\\'), "{raw:?}");
        }
    }

    #[test]
    fn the_quoting_helper_still_escapes_for_a_service_that_reads_escapes() {
        // Belt and braces: the phrase it is given never holds either character, but if one did, it would be escaped.
        let text = "a\"b\\c";
        assert_eq!(quoted(text), "\"a\\\"b\\\\c\"");
        let (outside, strings) = lex(&quoted(text));
        assert_eq!((outside.as_str(), strings), ("#", vec![text.to_string()]));
    }

    #[test]
    fn the_lexer_used_above_does_notice_a_phrase_that_gets_out() {
        // The escaping, not this reader, is what is under test; check the reader sees a break-out when there is one.
        let broken = format!(
            "ClientThreadId:\"{CHAT}\" AND \"x\" OR ClientThreadId:\"other\" AND NOT (isClientSoftDeleted:TRUE)"
        );
        let (outside, strings) = lex(&broken);
        assert_ne!(outside, SKELETON);
        assert_ne!(strings.len(), 2);
    }

    #[test]
    fn a_search_posts_one_scoped_phrase_with_the_substrate_token_and_nothing_else_of_ours() {
        let server = serve(vec![Reply::status(200).json(&answer(&[], 0, false))]);
        let b = searcher(&server);
        b.search_messages(CHAT, "  quarterly   report ", 0).unwrap();
        let seen = &server.seen()[0];
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.path, "/searchservice/api/v2/query");
        assert_eq!(seen.header("authorization"), Some("Bearer substrate-token"));
        for ours in ["authentication", "x-skypetoken", "x-ms-client-version"] {
            assert_eq!(seen.header(ours), None, "{ours} must not go to Substrate");
        }
        let body: Value = serde_json::from_str(&seen.body).unwrap();
        assert!(
            body["cvid"].as_str().is_some_and(super::super::is_guid),
            "{body}"
        );
        assert_eq!(body["scenario"], json!({"Name": "powerbar"}));
        let entity = &body["EntityRequests"][0];
        assert_eq!(entity["entityType"], "Message");
        assert_eq!(entity["contentSources"], json!(["Teams"]));
        assert_eq!(entity["propertySet"], "Optimized");
        assert_eq!(
            entity["fields"],
            json!([
                "Extension_SkypeSpaces_ConversationPost_Extension_FromSkypeInternalId_String",
                "Extension_SkypeSpaces_ConversationPost_Extension_ThreadType_String"
            ])
        );
        assert_eq!(
            entity["query"]["queryString"],
            format!(
                "ClientThreadId:\"{CHAT}\" AND \"quarterly report\" AND NOT (isClientSoftDeleted:TRUE)"
            )
        );
        assert_eq!(
            entity["query"]["displayQueryString"],
            "\"quarterly report\""
        );
        assert_eq!(entity["from"], json!(0));
        assert_eq!(entity["size"], json!(25));
        assert_eq!(server.seen().len(), 1);
    }

    #[test]
    fn a_hostile_phrase_reaches_the_wire_without_its_quotes_and_backslashes() {
        let server = serve(vec![Reply::status(200).json(&answer(&[], 0, false))]);
        let b = searcher(&server);
        let nasty = "x\" OR ClientThreadId:\"other\\";
        b.search_messages(CHAT, nasty, 0).unwrap();
        let body: Value = serde_json::from_str(&server.seen()[0].body).unwrap();
        let q = body["EntityRequests"][0]["query"]["queryString"]
            .as_str()
            .unwrap();
        assert_eq!(
            q,
            format!(
                "ClientThreadId:\"{CHAT}\" AND \"x OR ClientThreadId: other\" AND NOT (isClientSoftDeleted:TRUE)"
            )
        );
        let (outside, strings) = lex(q);
        assert_eq!(outside, SKELETON);
        assert_eq!(strings[1], "x OR ClientThreadId: other");
    }

    #[test]
    fn tabs_line_breaks_quotes_and_backslashes_are_all_just_spaces_between_words() {
        assert_eq!(normalise_phrase("a\tb\r\n c\u{a0}d").unwrap(), "a b c d");
        assert_eq!(normalise_phrase("  \t x \n ").unwrap(), "x");
        assert_eq!(normalise_phrase("\"a\"\\\\\"b\" c\\").unwrap(), "a b c");
        // The length rule counts what is left, not what was typed.
        let padded = format!("{}a", "\"\\ ".repeat(500));
        assert_eq!(normalise_phrase(&padded).unwrap(), "a");
        let words = "ab\" ".repeat(70); // 70 words, 209 characters once joined
        assert!(matches!(
            normalise_phrase(&words),
            Err(Error::InvalidInput { value, .. }) if value == "too long"
        ));
    }

    #[test]
    fn text_or_a_chat_id_that_cannot_be_used_is_refused_without_a_request() {
        let server = serve(vec![]);
        let b = searcher(&server);
        let long = "x".repeat(MAX_QUERY_CHARS + 1);
        for query in [
            "",
            "   ",
            "\t\n",
            "\"",
            "\\",
            " \" \\ \"\" \\\\ ",
            &long,
            "bad\u{0}nul",
            "esc\u{1b}[31m",
            "del\u{7f}",
        ] {
            assert!(
                matches!(
                    b.search_messages(CHAT, query, 0),
                    Err(Error::InvalidInput {
                        what: "search text",
                        ..
                    })
                ),
                "{query:?}"
            );
        }
        let too_long_id = format!("19:{}@thread.v2", "a".repeat(MAX_CHAT_ID_CHARS));
        for chat in [
            "",
            "19:a\"b@thread.v2",
            "19:a\\b@thread.v2",
            "19:a\nb@thread.v2",
            "19:a\u{0}b",
            &too_long_id,
        ] {
            assert!(
                matches!(
                    b.search_messages(chat, "report", 0),
                    Err(Error::InvalidInput {
                        what: "chat id",
                        ..
                    })
                ),
                "{chat:?}"
            );
        }
        assert!(
            server.seen().is_empty(),
            "nothing was sent: {:?}",
            server.seen()
        );
        // The longest allowed ones are not refused.
        let edge = "x".repeat(MAX_QUERY_CHARS);
        assert!(normalise_phrase(&edge).is_ok());
        assert!(check_chat_id(&"a".repeat(MAX_CHAT_ID_CHARS)).is_ok());
        for real in [
            "19:abc@thread.v2",
            &format!("19:{ADA}_{ME}@unq.gbl.spaces"),
            "48:notes",
        ] {
            assert!(check_chat_id(real).is_ok(), "{real}");
        }
    }

    #[test]
    fn the_error_for_refused_text_does_not_repeat_the_text() {
        let err = normalise_phrase("secret\u{0}words").unwrap_err();
        assert!(!err.to_string().contains("secret"), "{err}");
        assert!(!err.for_log().contains("secret"), "{}", err.for_log());
    }

    #[test]
    fn the_token_is_not_sent_to_an_unexpected_host() {
        let server = serve(vec![]);
        let mut b = searcher(&server);
        b.search_url = "https://evil.example/searchservice/api/v2/query".into();
        assert!(matches!(
            b.search_messages(CHAT, "report", 0),
            Err(Error::UntrustedUrl { .. })
        ));
        assert!(server.seen().is_empty());
        // The real address is accepted as a place for the token (checked without sending anything).
        assert!(SEARCH_URL.starts_with("https://substrate.office.com/"));
    }

    #[test]
    fn from_goes_to_the_service_and_the_next_page_starts_after_what_came() {
        let ids = [1_793_000_000_001_u64, 1_793_000_000_002, 1_793_000_000_003];
        let results: Vec<Value> = ids
            .iter()
            .map(|id| hit(json!(id), "2026-11-10T09:12:33Z"))
            .collect();
        let server = serve(vec![
            Reply::status(200).json(&answer(&results, 41, true)),
            Reply::status(200).json(&answer(&results, 41, false)),
            Reply::status(200).json(&answer(&[], 41, true)),
        ]);
        let b = searcher(&server);
        let page = b.search_messages(CHAT, "report", 50).unwrap();
        assert_eq!(page.hits.len(), 3);
        assert_eq!(page.next, Some(53));
        assert_eq!(page.total, Some(41));
        let body: Value = serde_json::from_str(&server.seen()[0].body).unwrap();
        assert_eq!(body["EntityRequests"][0]["from"], json!(50));
        // The last page says there is no more, and a page with nothing in it ends the walk even if the service claims
        // otherwise.
        let last = b.search_messages(CHAT, "report", 0).unwrap();
        assert_eq!(last.next, None);
        let empty = b.search_messages(CHAT, "report", 75).unwrap();
        assert!(empty.hits.is_empty());
        assert_eq!(empty.next, None);
    }

    #[test]
    fn the_next_page_counts_results_the_service_sent_even_if_some_could_not_be_read() {
        let results = [
            hit(json!(1_793_000_000_001_u64), "2026-11-10T09:12:33Z"),
            json!({"Source": {"DateTimeSent": "2026-11-10T09:12:33Z"}}),
        ];
        let server = serve(vec![Reply::status(200).json(&answer(&results, 9, true))]);
        let b = searcher(&server);
        let page = b.search_messages(CHAT, "report", 10).unwrap();
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.next, Some(12));
    }

    /// A hit that says (in `source_field`) which chat it belongs to.
    fn labelled(n: u64, source_field: &str, chat: &str) -> Value {
        let mut h = hit(json!(n), "2026-11-10T09:12:33Z");
        h["Source"][source_field] = json!(chat);
        h
    }

    #[test]
    fn a_hit_that_names_another_chat_is_left_out_counted_and_warned_about_once() {
        let upper = CHAT.to_ascii_uppercase();
        let results = [
            labelled(1_793_000_000_001, "ClientThreadId", CHAT), // ours
            labelled(1_793_000_000_002, "ClientThreadId", "19:other@thread.v2"), // another chat
            hit(json!(1_793_000_000_003_u64), "2026-11-10T09:12:33Z"), // says nothing: kept
            labelled(
                1_793_000_000_004,
                "ClientConversationId",
                &format!("{CHAT};messageid=1793000000004"),
            ), // ours, with a suffix
            labelled(
                1_793_000_000_005,
                "ClientConversationId",
                "19:other@thread.v2;messageid=1793000000005",
            ), // another chat, with a suffix
            labelled(1_793_000_000_006, "ClientThreadId", &upper), // ours, in other case
            // The thread field wins over the conversation field when both are there.
            {
                let mut h = labelled(1_793_000_000_007, "ClientThreadId", "19:other@thread.v2");
                h["Source"]["ClientConversationId"] = json!(CHAT);
                h
            },
            labelled(1_793_000_000_008, "ClientThreadId", ""), // an empty label says nothing: kept
        ];
        let server = serve(vec![Reply::status(200).json(&answer(&results, 40, true))]);
        let b = searcher(&server);
        let page = b.search_messages(CHAT, "report", 10).unwrap();
        let ids: Vec<&str> = page.hits.iter().map(|h| h.message_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "1793000000001",
                "1793000000003",
                "1793000000004",
                "1793000000006",
                "1793000000008"
            ]
        );
        // The next page starts after everything the service sent, not after what was kept.
        assert_eq!(page.next, Some(18));
        assert_eq!(page.total, Some(40));
        match b.take_warnings().as_slice() {
            [Error::Unexpected { operation, detail }] => {
                assert_eq!(*operation, "search messages");
                assert!(detail.contains("another chat"), "{detail}");
            }
            other => panic!("one warning, for all of them: {other:?}"),
        }
    }

    #[test]
    fn a_page_of_our_own_chats_hits_raises_no_warning() {
        let results = [
            labelled(1_793_000_000_001, "ClientThreadId", CHAT),
            hit(json!(1_793_000_000_002_u64), "2026-11-10T09:12:33Z"),
        ];
        let server = serve(vec![Reply::status(200).json(&answer(&results, 2, false))]);
        let b = searcher(&server);
        assert_eq!(b.search_messages(CHAT, "report", 0).unwrap().hits.len(), 2);
        assert!(b.take_warnings().is_empty());
    }

    #[test]
    fn a_page_of_nothing_but_other_chats_hits_still_pages_on() {
        let results = [
            labelled(1_793_000_000_001, "ClientThreadId", "19:other@thread.v2"),
            labelled(1_793_000_000_002, "ClientThreadId", "19:other@thread.v2"),
        ];
        let server = serve(vec![Reply::status(200).json(&answer(&results, 9, true))]);
        let b = searcher(&server);
        let page = b.search_messages(CHAT, "report", 4).unwrap();
        assert!(page.hits.is_empty());
        assert_eq!(page.next, Some(6), "the service has more, and where");
    }

    #[test]
    fn a_hit_is_read_from_a_numeric_or_a_text_id_and_its_markers_and_sender_are_tidied() {
        let results = [
            // A number, the time without an offset, the summary with the service's highlight tags and markers.
            json!({"Id": "h1", "HitHighlightedSummary": "<c0>quarterly</c0> \u{e000}report\u{e001}\n  is a < b ready",
                   "Source": {"InternetMessageId": 1_793_000_000_001_u64, "DateTimeSent": "2026-11-10T09:12:33.250",
                              "Sender": {"EmailAddress": {"Name": "Ada\u{202e} Lovelace"}}}}),
            // A text id, a plain Preview with markers, a name given directly.
            json!({"Source": {"InternetMessageId": "1793000000002", "DateTimeSent": "2026-11-10T10:00:00+01:00",
                              "Preview": "\u{e000}Hello\u{e001} \u{1b}[31mworld", "From": {"Name": "Grace"}}}),
            // No sender at all.
            json!({"Source": {"InternetMessageId": "1793000000003", "DateTimeSent": "2026-11-10T11:00:00Z"}}),
        ];
        let server = serve(vec![Reply::status(200).json(&answer(&results, 3, false))]);
        let b = searcher(&server);
        let page = b.search_messages(CHAT, "report", 0).unwrap();
        let got: Vec<(&str, &str, &str, String)> = page
            .hits
            .iter()
            .map(|h| {
                (
                    h.message_id.as_str(),
                    h.sender.as_str(),
                    h.preview.as_str(),
                    h.sent.to_rfc3339(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (
                    "1793000000001",
                    "Ada Lovelace",
                    "quarterly report is a < b ready",
                    "2026-11-10T09:12:33.250+00:00".to_string()
                ),
                (
                    "1793000000002",
                    "Grace",
                    "Hello [31mworld",
                    "2026-11-10T09:00:00+00:00".to_string()
                ),
                (
                    "1793000000003",
                    "",
                    "",
                    "2026-11-10T11:00:00+00:00".to_string()
                ),
            ]
        );
    }

    #[test]
    fn the_search_hits_own_id_is_never_taken_for_the_message_id() {
        let results =
            [json!({"Id": "1793000000009", "Source": {"DateTimeSent": "2026-11-10T09:12:33Z"}})];
        let server = serve(vec![Reply::status(200).json(&answer(&results, 1, false))]);
        let b = searcher(&server);
        let page = b.search_messages(CHAT, "report", 0).unwrap();
        assert!(page.hits.is_empty(), "{page:?}");
    }

    #[test]
    fn results_without_a_usable_id_or_time_are_left_out_and_reported_once() {
        let good = |n: u64| hit(json!(n), "2026-11-10T09:12:33Z");
        let results = [
            good(1_793_000_000_001),
            hit(json!("<abc@mail.example>"), "2026-11-10T09:12:33Z"), // an email style id
            hit(json!(12.5), "2026-11-10T09:12:33Z"),                 // not a whole number
            hit(json!("12 3"), "2026-11-10T09:12:33Z"),
            hit(json!(""), "2026-11-10T09:12:33Z"),
            hit(json!(1_793_000_000_002_u64), "yesterday"), // no readable time
            json!({"Source": {"InternetMessageId": 1_793_000_000_003_u64}}), // no time at all
            json!({"Id": "x"}),                             // no source
            json!("not even an object"),
            good(1_793_000_000_004),
        ];
        let server = serve(vec![Reply::status(200).json(&answer(&results, 10, false))]);
        let b = searcher(&server);
        let page = b.search_messages(CHAT, "report", 0).unwrap();
        let ids: Vec<&str> = page.hits.iter().map(|h| h.message_id.as_str()).collect();
        assert_eq!(ids, ["1793000000001", "1793000000004"]);
        assert!(
            matches!(
                b.take_warnings().as_slice(),
                [Error::Unexpected {
                    operation: "search messages",
                    ..
                }]
            ),
            "one warning for all the unreadable results"
        );
    }

    #[test]
    fn an_answer_that_could_not_read_the_query_is_an_error_not_no_hits() {
        let parse_set = json!({"EntitySets": [{"Properties": {"HasParseException": true},
                                               "ResultSets": [{"Total": 0, "Results": []}]}]});
        let parse_result = json!({"EntitySets": [{"ResultSets": [
            {"Total": 0, "Results": [], "Properties": {"HasParseException": true}}]}]});
        let parse_top = json!({"Properties": {"HasParseException": true},
                               "EntitySets": [{"ResultSets": [{"Total": 0, "Results": []}]}]});
        let second_unreadable = json!({"EntitySets": [
            {"ResultSets": [{"Total": 0, "Results": []}]},
            {"Properties": {"HasParseException": true}, "ResultSets": []}]});
        for bad in [parse_set, parse_result, parse_top, second_unreadable] {
            let server = serve(vec![Reply::status(200).json(&bad.to_string())]);
            let b = searcher(&server);
            assert!(
                matches!(
                    b.search_messages(CHAT, "report", 0),
                    Err(Error::Unexpected {
                        operation: "search messages",
                        ..
                    })
                ),
                "{bad}"
            );
        }
        // A parse-exception flag that is present and false is fine, and so is `IsPartial` either way: the live service
        // sets it on answers that found their message and on ones that found nothing.
        for partial in [json!(false), json!(true), json!("True")] {
            let fine = json!({"EntitySets": [{"IsPartial": partial, "Properties": {"HasParseException": false},
                                              "ResultSets": [{"Total": 0, "Results": []}]}]});
            let server = serve(vec![Reply::status(200).json(&fine.to_string())]);
            assert!(
                searcher(&server)
                    .search_messages(CHAT, "report", 0)
                    .unwrap()
                    .hits
                    .is_empty(),
                "{fine}"
            );
        }
    }

    #[test]
    fn a_partial_answer_with_a_hit_still_gives_the_hit() {
        let answer = json!({"EntitySets": [{"IsPartial": true, "Properties": {"HasParseException": false},
            "ResultSets": [{"Total": 1, "MoreResultsAvailable": false, "Results": [
                labelled(1_793_000_000_001, "ClientThreadId", CHAT)]}]}]});
        let server = serve(vec![Reply::status(200).json(&answer.to_string())]);
        let page = searcher(&server)
            .search_messages(CHAT, "report", 0)
            .unwrap();
        assert_eq!(page.hits.len(), 1);
        assert_eq!(page.hits[0].message_id, "1793000000001");
    }

    #[test]
    fn an_answer_without_the_expected_structure_is_an_error() {
        for bad in [
            "{}",
            "null",
            r#"{"EntitySets": []}"#,
            r#"{"EntitySets": "no"}"#,
            r#"{"EntitySets": [{}]}"#,
            r#"{"EntitySets": [{"ResultSets": []}]}"#,
            r#"{"EntitySets": [{"ResultSets": [{"Total": 4}]}]}"#,
            r#"{"EntitySets": [{"ResultSets": [{"Results": {"a": 1}}]}]}"#,
        ] {
            let server = serve(vec![Reply::status(200).json(bad)]);
            assert!(
                matches!(
                    searcher(&server).search_messages(CHAT, "report", 0),
                    Err(Error::Unexpected { .. })
                ),
                "{bad}"
            );
        }
        // A result set that says it has nothing may leave the list out.
        let server = serve(vec![
            Reply::status(200).json(r#"{"EntitySets": [{"ResultSets": [{"Total": 0}]}]}"#),
        ]);
        let page = searcher(&server)
            .search_messages(CHAT, "report", 0)
            .unwrap();
        assert_eq!((page.hits.len(), page.next, page.total), (0, None, Some(0)));
    }

    #[test]
    fn the_total_may_be_text_and_may_be_missing() {
        let server = serve(vec![
            Reply::status(200)
                .json(r#"{"EntitySets": [{"ResultSets": [{"Total": "17", "Results": []}]}]}"#),
            Reply::status(200).json(r#"{"EntitySets": [{"ResultSets": [{"Results": []}]}]}"#),
        ]);
        let b = searcher(&server);
        assert_eq!(
            b.search_messages(CHAT, "report", 0).unwrap().total,
            Some(17)
        );
        assert_eq!(b.search_messages(CHAT, "report", 0).unwrap().total, None);
    }

    #[test]
    fn refusals_and_throttling_are_typed_like_every_other_call() {
        let server = serve(vec![
            Reply::status(403).json(r#"{"error":{"message":"not allowed"}}"#),
            Reply::status(429).header("Retry-After", "7").body(""),
            Reply::status(500).body("boom"),
        ]);
        let b = searcher(&server);
        match b.search_messages(CHAT, "report", 0) {
            Err(Error::Status {
                operation: "search messages",
                status: 403,
                message,
            }) => assert_eq!(message, "not allowed"),
            other => panic!("{other:?}"),
        }
        match b.search_messages(CHAT, "report", 0) {
            Err(Error::Throttled {
                operation: "search messages",
                retry_after: Some(d),
            }) => assert_eq!(d.as_secs(), 7),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            b.search_messages(CHAT, "report", 0),
            Err(Error::Status { status: 500, .. })
        ));
    }

    /// A backend whose token renewals go to `server` too, so a refused token can be renewed in a test.
    fn searcher_that_can_renew(server: &Server) -> TeamsBackend {
        let mut b = searcher(server);
        b.auth = TeamsAuth::new(
            TeamsSession::for_test(ME, "Me"),
            Arc::new(MemoryStore::new()),
            BrowserOptions::new("unused-profile"),
        )
        .with_endpoints(server.origin(), &server.url("/v1.0/authz"));
        b
    }

    #[test]
    fn a_refused_token_is_renewed_once_and_a_second_refusal_is_the_answer() {
        let server = serve(vec![
            Reply::status(401).body("expired"),
            Reply::status(200).json(r#"{"access_token":"renewed-substrate","expires_in":3600}"#),
            Reply::status(401).json(r#"{"message":"still no"}"#),
        ]);
        let b = searcher_that_can_renew(&server);
        assert!(matches!(
            b.search_messages(CHAT, "report", 0),
            Err(Error::Status { status: 401, .. })
        ));
        let seen = server.seen();
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen[0].header("authorization"),
            Some("Bearer substrate-token")
        );
        assert_eq!(seen[2].path, "/searchservice/api/v2/query");
        assert_eq!(
            seen[2].header("authorization"),
            Some("Bearer renewed-substrate")
        );
    }

    #[test]
    fn a_tenant_that_withholds_the_search_scope_fails_the_search_alone() {
        let server = serve(vec![Reply::status(400).json(
            r#"{"error":"invalid_scope","error_description":"AADSTS70011: the scope is not valid"}"#,
        )]);
        let b = searcher_that_can_renew(&server);
        *lock(&b.substrate) = None;
        match b.search_messages(CHAT, "report", 0) {
            Err(Error::Auth(AuthError::Rejected { code, .. })) => assert_eq!(code, "invalid_scope"),
            other => panic!("{other:?}"),
        }
        let seen = server.seen();
        assert_eq!(
            seen.len(),
            1,
            "only the renewal was asked, no browser, no search"
        );
        assert!(
            seen[0].path.ends_with("/oauth2/v2.0/token"),
            "{}",
            seen[0].path
        );
    }

    #[test]
    fn only_the_teams_backend_can_search() {
        let server = serve(vec![]);
        assert!(searcher(&server).can_search_messages());
        assert!(searcher(&server).can_search_conversations());
    }

    // --- several conversations at once (the channels of a team) -----------------------------------------------------

    const CHAN_A: &str = "19:aaa@thread.tacv2";
    const CHAN_B: &str = "19:bbb@thread.tacv2";
    const CHAN_C: &str = "19:ccc@thread.tacv2";

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    /// The text outside the quoted strings of a query over `n` conversations.
    fn skeleton(n: usize) -> String {
        if n == 1 {
            return SKELETON.to_string();
        }
        let ors = vec!["ClientThreadId:#"; n].join(" OR ");
        format!("({ors}) AND # AND NOT (isClientSoftDeleted:TRUE)")
    }

    /// A hit in `conversation` (as `ClientThreadId`), in the channel called `channel`.
    fn in_channel(n: u64, conversation: &str, channel: &str) -> Value {
        let mut h = labelled(n, "ClientThreadId", conversation);
        h["Source"]["ChannelName"] = json!(channel);
        h
    }

    #[test]
    fn a_scope_of_one_is_the_old_query_and_a_scope_of_three_is_an_or_of_the_three() {
        let server = serve(vec![
            Reply::status(200).json(&answer(&[], 0, false)),
            Reply::status(200).json(&answer(&[], 0, false)),
        ]);
        let b = searcher(&server);
        b.search_conversations(&ids(&[CHAN_A]), " quarterly  report", 0)
            .unwrap();
        b.search_conversations(&ids(&[CHAN_A, CHAN_B, CHAN_C]), "quarterly report", 25)
            .unwrap();
        let seen = server.seen();
        let query = |i: usize| -> String {
            let body: Value = serde_json::from_str(&seen[i].body).unwrap();
            body["EntityRequests"][0]["query"]["queryString"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(
            query(0),
            format!(
                "ClientThreadId:\"{CHAN_A}\" AND \"quarterly report\" AND NOT (isClientSoftDeleted:TRUE)"
            )
        );
        assert_eq!(
            query(1),
            format!(
                "(ClientThreadId:\"{CHAN_A}\" OR ClientThreadId:\"{CHAN_B}\" OR ClientThreadId:\"{CHAN_C}\") AND \"quarterly report\" AND NOT (isClientSoftDeleted:TRUE)"
            )
        );
        let (outside, strings) = lex(&query(1));
        assert_eq!(outside, skeleton(3));
        assert_eq!(strings, [CHAN_A, CHAN_B, CHAN_C, "quarterly report"]);
        let body: Value = serde_json::from_str(&seen[1].body).unwrap();
        assert_eq!(body["EntityRequests"][0]["from"], json!(25));
        assert_eq!(
            seen[1].header("authorization"),
            Some("Bearer substrate-token")
        );
        // The ordinary single-chat call is the same call with a scope of one.
        assert_eq!(skeleton(1), SKELETON);
    }

    #[test]
    fn no_phrase_can_change_a_scope_of_several_conversations_or_leave_its_quotes() {
        let scope = ids(&[CHAN_A, CHAN_B, CHAN_C]);
        for raw in hostile_phrases() {
            let Ok(phrase) = normalise_phrase(&raw) else {
                continue;
            };
            let body = request_body(&scope, &phrase, 0);
            let request = &body["EntityRequests"][0]["query"];
            let (outside, strings) = lex(request["queryString"].as_str().unwrap());
            assert_eq!(outside, skeleton(3), "for {raw:?}");
            assert_eq!(
                strings,
                [CHAN_A, CHAN_B, CHAN_C, phrase.as_str()],
                "for {raw:?}"
            );
            let q = request["queryString"].as_str().unwrap();
            assert_eq!(q.matches('"').count(), 8, "{raw:?}");
            assert!(!q.contains('\\'), "{raw:?}");
        }
    }

    #[test]
    fn a_hostile_conversation_id_anywhere_in_the_scope_refuses_the_whole_call_and_sends_nothing() {
        let server = serve(vec![]);
        let b = searcher(&server);
        let too_long = format!("19:{}@thread.v2", "a".repeat(MAX_CHAT_ID_CHARS));
        let hostile = [
            "x\" OR ClientThreadId:\"other",
            "a\"b",
            "a\\b",
            "19:a\nb@thread.v2",
            "19:a\u{0}b",
            "19:a\u{1b}[31mb",
            "",
            &too_long,
        ];
        for bad in hostile {
            // First, last, and past the cap, where it would have been dropped without a check.
            let mut first = ids(&[bad, CHAN_A]);
            let mut last = ids(&[CHAN_A, CHAN_B, bad]);
            let mut past_the_cap: Vec<String> = (0..MAX_SEARCH_CONVERSATIONS + 5)
                .map(|n| format!("19:ok{n}@thread.tacv2"))
                .collect();
            past_the_cap.push(bad.to_string());
            for scope in [&mut first, &mut last, &mut past_the_cap] {
                assert!(
                    matches!(
                        b.search_conversations(scope, "report", 0),
                        Err(Error::InvalidInput {
                            what: "chat id",
                            ..
                        })
                    ),
                    "{bad:?}"
                );
            }
        }
        assert!(
            server.seen().is_empty(),
            "nothing was sent: {:?}",
            server.seen()
        );
    }

    #[test]
    fn an_empty_scope_is_refused_and_so_is_text_that_cannot_be_used() {
        let server = serve(vec![]);
        let b = searcher(&server);
        assert!(matches!(
            b.search_conversations(&[], "report", 0),
            Err(Error::InvalidInput {
                what: "chat id",
                ..
            })
        ));
        for query in [
            "",
            "  ",
            "\"",
            "bad\u{0}nul",
            &"x".repeat(MAX_QUERY_CHARS + 1),
        ] {
            assert!(
                matches!(
                    b.search_conversations(&ids(&[CHAN_A, CHAN_B]), query, 0),
                    Err(Error::InvalidInput {
                        what: "search text",
                        ..
                    })
                ),
                "{query:?}"
            );
        }
        assert!(server.seen().is_empty());
    }

    #[test]
    fn repeated_ids_are_searched_once_and_no_more_than_fifty_are() {
        let upper = CHAN_B.to_ascii_uppercase();
        assert_eq!(
            search_scope_ids(&ids(&[CHAN_A, CHAN_B, CHAN_A, &upper, CHAN_C])).unwrap(),
            ids(&[CHAN_A, CHAN_B, CHAN_C]),
            "the first spelling is kept"
        );
        let many: Vec<String> = (0..MAX_SEARCH_CONVERSATIONS + 20)
            .map(|n| format!("19:c{n}@thread.tacv2"))
            .collect();
        let scope = search_scope_ids(&many).unwrap();
        assert_eq!(scope.len(), MAX_SEARCH_CONVERSATIONS);
        assert_eq!(
            scope[..],
            many[..MAX_SEARCH_CONVERSATIONS],
            "the first ones"
        );
        assert_eq!(
            search_scope_ids(&many[..MAX_SEARCH_CONVERSATIONS])
                .unwrap()
                .len(),
            MAX_SEARCH_CONVERSATIONS,
            "exactly the cap is all kept"
        );
        // Repeats do not use up the room.
        let mut with_repeats = many.clone();
        with_repeats.splice(1..1, many[..30].iter().cloned());
        assert_eq!(
            search_scope_ids(&with_repeats).unwrap()[..],
            many[..MAX_SEARCH_CONVERSATIONS]
        );
        // And it is what reaches the wire.
        let server = serve(vec![Reply::status(200).json(&answer(&[], 0, false))]);
        let b = searcher(&server);
        b.search_conversations(&many, "report", 0).unwrap();
        let body: Value = serde_json::from_str(&server.seen()[0].body).unwrap();
        let q = body["EntityRequests"][0]["query"]["queryString"]
            .as_str()
            .unwrap();
        assert_eq!(
            q.matches("ClientThreadId:").count(),
            MAX_SEARCH_CONVERSATIONS
        );
        assert!(q.contains("19:c49@thread.tacv2") && !q.contains("19:c50@thread.tacv2"));
    }

    #[test]
    fn hits_from_two_channels_name_their_conversation_and_channel() {
        let upper = CHAN_B.to_ascii_uppercase();
        let results = [
            in_channel(1_793_000_000_001, CHAN_A, "General"),
            // A reply in a thread: the label carries the root post's id.
            in_channel(
                1_793_000_000_002,
                &format!("{CHAN_B};messageid=1793000000000"),
                "Releases",
            ),
            // Another spelling of the id: the caller's own spelling comes back, so a caller can compare.
            in_channel(1_793_000_000_003, &upper, "Releases"),
            // The conversation field, when the thread field is not there.
            {
                let mut h = labelled(
                    1_793_000_000_004,
                    "ClientConversationId",
                    &format!("{CHAN_A};messageid=1793000000001"),
                );
                h["Source"]["ChannelName"] = json!("General");
                h
            },
            // No channel name given: an empty one.
            labelled(1_793_000_000_005, "ClientThreadId", CHAN_B),
        ];
        let server = serve(vec![Reply::status(200).json(&answer(&results, 5, false))]);
        let b = searcher(&server);
        let page = b
            .search_conversations(&ids(&[CHAN_A, CHAN_B]), "report", 0)
            .unwrap();
        let got: Vec<(&str, &str, &str)> = page
            .hits
            .iter()
            .map(|h| {
                (
                    h.message_id.as_str(),
                    h.conversation.as_str(),
                    h.channel.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("1793000000001", CHAN_A, "General"),
                ("1793000000002", CHAN_B, "Releases"),
                ("1793000000003", CHAN_B, "Releases"),
                ("1793000000004", CHAN_A, "General"),
                ("1793000000005", CHAN_B, ""),
            ]
        );
        assert!(b.take_warnings().is_empty());
    }

    #[test]
    fn a_hit_outside_the_scope_is_left_out_counted_and_warned_about_once() {
        let results = [
            in_channel(1_793_000_000_001, CHAN_A, "General"),
            in_channel(1_793_000_000_002, "19:elsewhere@thread.tacv2", "Elsewhere"),
            in_channel(
                1_793_000_000_003,
                "19:elsewhere@thread.tacv2;messageid=1793000000000",
                "Elsewhere",
            ),
            in_channel(1_793_000_000_004, CHAN_C, "Releases"),
        ];
        let server = serve(vec![Reply::status(200).json(&answer(&results, 40, true))]);
        let b = searcher(&server);
        let page = b
            .search_conversations(&ids(&[CHAN_A, CHAN_C]), "report", 10)
            .unwrap();
        let kept: Vec<&str> = page.hits.iter().map(|h| h.message_id.as_str()).collect();
        assert_eq!(kept, ["1793000000001", "1793000000004"]);
        assert_eq!(page.next, Some(14), "the next page counts what was sent");
        assert_eq!(page.total, Some(40));
        match b.take_warnings().as_slice() {
            [Error::Unexpected { operation, detail }] => {
                assert_eq!(*operation, "search messages");
                assert!(detail.contains("another chat"), "{detail}");
            }
            other => panic!("one warning, for both: {other:?}"),
        }
    }

    #[test]
    fn a_hit_with_no_label_is_kept_in_a_scope_of_one_and_unreadable_in_a_scope_of_several() {
        let unlabelled = hit(json!(1_793_000_000_001_u64), "2026-11-10T09:12:33Z");
        let server = serve(vec![
            Reply::status(200).json(&answer(std::slice::from_ref(&unlabelled), 1, false)),
            Reply::status(200).json(&answer(
                &[
                    unlabelled.clone(),
                    in_channel(1_793_000_000_002, CHAN_B, "Releases"),
                ],
                2,
                false,
            )),
        ]);
        let b = searcher(&server);
        let one = b
            .search_conversations(&ids(&[CHAN_A]), "report", 0)
            .unwrap();
        assert_eq!(one.hits.len(), 1);
        assert_eq!(one.hits[0].conversation, CHAN_A);
        assert!(b.take_warnings().is_empty());
        let two = b
            .search_conversations(&ids(&[CHAN_A, CHAN_B]), "report", 0)
            .unwrap();
        let kept: Vec<&str> = two.hits.iter().map(|h| h.message_id.as_str()).collect();
        assert_eq!(
            kept,
            ["1793000000002"],
            "which channel it is cannot be known"
        );
        assert!(
            matches!(
                b.take_warnings().as_slice(),
                [Error::Unexpected {
                    operation: "search messages",
                    ..
                }]
            ),
            "counted with the unreadable ones, one warning"
        );
    }

    #[test]
    fn a_partial_answer_over_several_conversations_is_accepted_and_the_channel_name_is_tidied() {
        let long = "x".repeat(300);
        let mut named = in_channel(
            1_793_000_000_001,
            CHAN_A,
            "Gen\u{e000}eral\u{1b}[31m\n  tab\u{202e}bed",
        );
        named["Source"]["TeamName"] = json!("Some team");
        let mut huge = in_channel(1_793_000_000_002, CHAN_B, &long);
        huge["Source"]["ChannelName"] = json!(long);
        let mut odd = in_channel(1_793_000_000_003, CHAN_B, "unused");
        odd["Source"]["ChannelName"] = json!(12);
        let answer = json!({"EntitySets": [{"IsPartial": true, "Properties": {"HasParseException": false},
            "ResultSets": [{"Total": 3, "MoreResultsAvailable": false,
                            "Results": [named, huge, odd]}]}]});
        let server = serve(vec![Reply::status(200).json(&answer.to_string())]);
        let page = searcher(&server)
            .search_conversations(&ids(&[CHAN_A, CHAN_B]), "report", 0)
            .unwrap();
        assert_eq!(page.hits.len(), 3);
        assert_eq!(page.hits[0].channel, "General[31m tabbed");
        assert_eq!(page.hits[1].channel.chars().count(), MAX_CHANNEL_CHARS);
        assert_eq!(page.hits[2].channel, "", "not text, so nothing");
    }
}
