//! Writing to a channel: a new post (with an optional title), a reply in a post's thread, and editing a post. See
//! `docs/teams-web-protocol.md` ("Teams and channels"): a post is the same request a chat message is, with
//! `properties.subject` set; a reply goes to the same conversation with `;messageid=<post id>` after it (the `;` itself,
//! not `%3B`); an edit that does not repeat the title loses it. Checked on a throwaway channel; what was not (editing a
//! reply, formatting variants, attachments, mentions in an edit) is listed there and not done here. Person mentions go
//! in the way a chat message's do (`teams_mentions`); `@channel` and `@team` are not built.
//!
//! The same credentials and headers as a chat message (`TeamsBackend::send_message`), and the same failure rules: what
//! cannot have reached the service is [`Error::NotSent`] or a refusal, and everything that may have is
//! [`Error::delivery_unknown`]. Nothing about a post (its text, title, channel or thread) reaches the log: only that
//! one was made, and sizes.

use super::{
    ChannelPost, Delivery, Error, Msg, OutMention, Result, TeamsBackend, Utc, Value, clean, id_tag,
    is_guid, json, message_body, message_properties, teams_mentions, urlencode,
};
use chrono::TimeZone;

/// The longest title a post may have, in characters.
const MAX_SUBJECT_CHARS: usize = 200;
/// The longest channel id accepted, in characters.
const MAX_CHANNEL_ID_CHARS: usize = 200;
/// The longest post id (a time in milliseconds) accepted, in digits.
const MAX_MESSAGE_ID_DIGITS: usize = 20;

fn invalid(what: &'static str, reason: &str) -> Error {
    Error::InvalidInput {
        what,
        value: reason.to_string(),
    }
}

/// A channel's conversation id: `19:<name>@thread.tacv2` (or `@thread.skype` for a team's General channel), made of
/// letters, digits and `-_.`. Anything else is refused before it can reach a URL: a chat, a meeting or a stream is not a
/// place to post "to a channel", and an id holding `;` or `/` would name another thing than the one asked for.
fn check_channel_id(channel: &str) -> Result<()> {
    let reason = if channel.is_empty() {
        "empty"
    } else if channel.chars().count() > MAX_CHANNEL_ID_CHARS {
        "too long"
    } else {
        let name = channel
            .strip_prefix("19:")
            .and_then(|rest| {
                rest.strip_suffix("@thread.tacv2")
                    .or_else(|| rest.strip_suffix("@thread.skype"))
            })
            .unwrap_or("");
        if !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Ok(());
        }
        "is not a channel's id"
    };
    Err(invalid("channel id", reason))
}

/// A post's id, the time the service stored it in milliseconds: digits only.
fn check_message_id(what: &'static str, id: &str) -> Result<()> {
    if id.is_empty() || id.len() > MAX_MESSAGE_ID_DIGITS || !id.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(invalid(what, "not a message id"));
    }
    Ok(())
}

/// Most people one post may mention.
const MAX_MENTIONS: usize = 50;
/// The longest display name a mention may carry, in characters.
const MAX_MENTION_NAME_CHARS: usize = 200;

/// The mentions of a post or an edit, checked before anything is sent: each must be a person's object id (it goes into
/// an `8:orgid:` address), have a printable name of sensible length, a distinct `id` (the number the HTML's `<at>` tag
/// and `properties.mentions` agree on) and a tag in the HTML to belong to. There may be at most [`MAX_MENTIONS`].
/// The error says why and never what.
fn check_mentions(html: &str, mentions: &[OutMention]) -> Result<()> {
    if mentions.len() > MAX_MENTIONS {
        return Err(invalid("mention", "too many"));
    }
    for (i, m) in mentions.iter().enumerate() {
        if !is_guid(&m.user_id) {
            return Err(invalid("mention", "not a person's id"));
        }
        let name = m.name.trim();
        if name.is_empty() {
            return Err(invalid("mention", "no name"));
        }
        if name.chars().count() > MAX_MENTION_NAME_CHARS {
            return Err(invalid("mention", "name too long"));
        }
        if clean(name) != name {
            return Err(invalid("mention", "name has control characters"));
        }
        if mentions[..i].iter().any(|other| other.id == m.id) {
            return Err(invalid("mention", "the same number twice"));
        }
        if !html.contains(&format!("<at id=\"{}\">", m.id)) {
            return Err(invalid("mention", "not in the message"));
        }
    }
    Ok(())
}

/// A title as it is sent: line breaks and tabs become spaces, other control characters and bidi marks are dropped, the
/// ends are trimmed. Refused when nothing is left or more than [`MAX_SUBJECT_CHARS`] is. The error says why, never what.
fn normalise_subject(subject: &str) -> Result<String> {
    let spaced: String = subject
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let cleaned = clean(&spaced);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return Err(invalid("post subject", "empty"));
    }
    if trimmed.chars().count() > MAX_SUBJECT_CHARS {
        return Err(invalid("post subject", "too long"));
    }
    Ok(trimmed.to_string())
}

impl TeamsBackend {
    /// [`Backend::post_to_channel`](crate::Backend::post_to_channel).
    pub(super) fn post_channel(&self, channel: &str, post: &ChannelPost<'_>) -> Result<Msg> {
        check_channel_id(channel)?;
        if let Some(root) = post.thread {
            check_message_id("thread id", root)?;
        }
        let subject = match (post.subject, post.thread) {
            (Some(_), Some(_)) => return Err(invalid("post subject", "a reply has none")),
            (Some(subject), None) => Some(normalise_subject(subject)?),
            (None, _) => None,
        };
        if post.html.trim().is_empty() {
            return Err(invalid("message", "empty"));
        }
        check_mentions(post.html, post.mentions)?;
        let live = self.live(None).map_err(Error::not_sent)?;
        let (me, name) = self.auth.me();
        // The mentions are built by the function a chat message's are, so the two stay identical. No quote (a thread
        // needs none), no files.
        let (content, mention_json) = teams_mentions(post.html, post.mentions);
        let mut body = message_body(channel, &me, &name, &content);
        body["properties"] = message_properties(
            mention_json.as_deref().unwrap_or("[]"),
            subject.as_deref().unwrap_or(""),
        );
        let conversation = match post.thread {
            Some(root) => format!("{};messageid={root}", urlencode(channel)),
            None => urlencode(channel),
        };
        let url = format!(
            "{}/v1/users/ME/conversations/{conversation}/messages",
            live.chat_base
        );
        log::debug!(
            "post to channel {}: {}{}, {} bytes of HTML, {} mention(s)",
            id_tag(channel),
            if post.thread.is_some() {
                "a reply"
            } else {
                "a post"
            },
            if subject.is_some() {
                " with a subject"
            } else {
                ""
            },
            post.html.len(),
            post.mentions.len()
        );
        let v = self.send_message(&url, &body)?;
        let arrived = v["OriginalArrivalTime"]
            .as_i64()
            .unwrap_or_else(|| Utc::now().timestamp_millis());
        let created = Utc
            .timestamp_millis_opt(arrived)
            .single()
            .unwrap_or_else(Utc::now);
        Ok(Msg {
            id: arrived.to_string(),
            created,
            modified: created,
            edited: None,
            from_id: Some(me),
            from_name: clean(&name),
            system: false,
            deleted: false,
            html: true,
            body: content,
            subject,
            attachments: Vec::new(),
            reactions: Vec::new(),
            mentions: post.mentions.iter().map(|m| m.user_id.clone()).collect(),
            delivery: Delivery::Sent,
            root_id: post.thread.map(str::to_string),
        })
    }

    /// [`Backend::edit_post`](crate::Backend::edit_post): the edit request a chat message gets, with the title repeated
    /// in `properties.subject` when the post has one, since an edit that does not repeat it removes it, and in
    /// `properties.mentions` when it mentions anyone (an edit with none sends no `mentions`, like a chat's).
    pub(super) fn edit_channel_post(
        &self,
        channel: &str,
        message_id: &str,
        subject: Option<&str>,
        html: &str,
        mentions: &[OutMention],
    ) -> Result<()> {
        check_channel_id(channel)?;
        check_message_id("message id", message_id)?;
        let subject = subject.map(normalise_subject).transpose()?;
        if html.trim().is_empty() {
            return Err(invalid("message", "empty"));
        }
        check_mentions(html, mentions)?;
        let (content, mention_json) = teams_mentions(html, mentions);
        let mut properties = serde_json::Map::new();
        if let Some(subject) = subject {
            properties.insert("subject".to_string(), json!(subject));
        }
        if let Some(mention_json) = mention_json {
            properties.insert("mentions".to_string(), json!(mention_json));
        }
        let properties = (!properties.is_empty()).then_some(Value::Object(properties));
        self.put_edit("edit post", channel, message_id, &content, properties)
    }

    /// The edit request: `PUT` to the message's own address with `skypeeditedid` naming it, answered by an empty body
    /// (what the Teams client sends, also seen in other open-source clients). `properties`, when there is any, is what
    /// the edit restates (the mentions of a chat message, the title of a channel post). Used by `edit` for chats too.
    pub(super) fn put_edit(
        &self,
        operation: &'static str,
        conversation: &str,
        message_id: &str,
        content: &str,
        properties: Option<Value>,
    ) -> Result<()> {
        // Message ids are the time the service stored them, in milliseconds.
        if message_id.is_empty() || !message_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::InvalidInput {
                what: "message id",
                value: message_id.to_string(),
            });
        }
        let live = self.live(None).map_err(Error::not_sent)?;
        let (_, name) = self.auth.me();
        let mut body = json!({
            "content": content,
            "messagetype": "RichText/Html",
            "contenttype": "Text",
            "skypeeditedid": message_id,
            "imdisplayname": name,
        });
        if let Some(properties) = properties {
            body["properties"] = properties;
        }
        let url = format!(
            "{}/v1/users/ME/conversations/{}/messages/{message_id}",
            live.chat_base,
            urlencode(conversation)
        );
        self.request(operation, "PUT", &url, Some(&body))
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{ME, backend};
    use super::*;
    use crate::Backend as _;
    use crate::testserver::{Reply, serve};

    const CHANNEL: &str = "19:abc-DEF_1.2@thread.tacv2";
    const CHANNEL_PATH: &str =
        "/api/chatsvc/amer/v1/users/ME/conversations/19%3Aabc-DEF_1.2@thread.tacv2";
    const ROOT: &str = "1793000000000";
    const ANSWER: &str = r#"{"OriginalArrivalTime": 1793095260000}"#;

    fn posted(server: &crate::testserver::Server, n: usize) -> Value {
        serde_json::from_str(&server.seen()[n].body).unwrap()
    }

    #[test]
    fn a_post_with_a_subject_is_a_chat_send_to_the_channel_with_the_subject_in_its_properties() {
        let server = serve(vec![Reply::status(201).json(ANSWER)]);
        let b = backend(&server);
        assert!(b.can_post_to_channels());
        let sent = b
            .post_to_channel(
                CHANNEL,
                &ChannelPost::new("<p>hello team</p>").with_subject("Design review"),
            )
            .unwrap();
        let seen = server.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "POST");
        assert_eq!(seen[0].path, format!("{CHANNEL_PATH}/messages"));
        let body = posted(&server, 0);
        assert_eq!(body["conversationid"], CHANNEL);
        assert_eq!(body["content"], "<p>hello team</p>");
        assert_eq!(body["messagetype"], "RichText/Html");
        assert_eq!(body["from"], format!("8:orgid:{ME}"));
        let props = &body["properties"];
        assert_eq!(props["subject"], "Design review");
        assert_eq!(props["mentions"], "[]");
        assert_eq!(props["formatVariant"], "TEAMS");
        assert_eq!(props["cards"], "[]");
        assert_eq!(props["files"], "[]");
        assert_eq!(props["links"], "[]");
        assert!(props["qtdMsgs"].is_null(), "a post quotes nothing");
        assert_eq!(
            (
                sent.id.as_str(),
                sent.subject.as_deref(),
                sent.root_id.as_deref()
            ),
            ("1793095260000", Some("Design review"), None)
        );
        assert_eq!(sent.body, "<p>hello team</p>");
        assert_eq!(sent.from_id.as_deref(), Some(ME));
        assert_eq!(sent.delivery, Delivery::Sent);
    }

    #[test]
    fn a_post_with_no_subject_sends_an_empty_one_like_a_chat_message() {
        let server = serve(vec![Reply::status(201).json(ANSWER)]);
        let sent = backend(&server)
            .post_to_channel(CHANNEL, &ChannelPost::new("<p>hi</p>"))
            .unwrap();
        assert_eq!(posted(&server, 0)["properties"]["subject"], "");
        assert_eq!(sent.subject, None);
    }

    #[test]
    fn a_reply_goes_to_the_thread_with_a_literal_semicolon_and_no_quote_or_subject() {
        let server = serve(vec![Reply::status(201).json(ANSWER)]);
        let sent = backend(&server)
            .post_to_channel(CHANNEL, &ChannelPost::new("<p>agreed</p>").in_thread(ROOT))
            .unwrap();
        let seen = server.seen();
        assert_eq!(
            seen[0].path,
            format!("{CHANNEL_PATH};messageid={ROOT}/messages"),
            "the `;` itself, not %3B"
        );
        assert!(!seen[0].path.contains("%3B"), "{}", seen[0].path);
        let body = posted(&server, 0);
        assert_eq!(
            body["conversationid"], CHANNEL,
            "the body names the channel; the thread is in the address"
        );
        assert_eq!(body["content"], "<p>agreed</p>", "no reply quote");
        assert!(body["properties"]["qtdMsgs"].is_null());
        assert_eq!(body["properties"]["subject"], "");
        assert_eq!(sent.root_id.as_deref(), Some(ROOT));
        assert_eq!(sent.subject, None);
    }

    #[test]
    fn a_post_carries_the_ic3_bearer_and_the_genuine_clients_other_headers() {
        let server = serve(vec![Reply::status(201).json(ANSWER)]);
        backend(&server)
            .post_to_channel(CHANNEL, &ChannelPost::new("<p>hi</p>"))
            .unwrap();
        let seen = &server.seen()[0];
        assert_eq!(seen.header("authorization"), Some("Bearer ic3-token"));
        for (name, value) in [
            ("x-ms-request-priority", "0"),
            ("behavioroverride", "redirectAs404"),
            ("x-ms-test-user", "False"),
            ("x-ms-migration", "True"),
            ("dnt", "1"),
        ] {
            assert_eq!(seen.header(name), Some(value), "{name}");
        }
        for absent in ["authentication", "origin", "x-ms-client-version"] {
            assert!(seen.header(absent).is_none(), "{absent}");
        }
    }

    #[test]
    fn what_cannot_be_posted_is_refused_before_any_request() {
        let server = serve(vec![]);
        let b = backend(&server);
        let long_subject = "s".repeat(MAX_SUBJECT_CHARS + 1);
        let refused: Vec<(&str, &str, ChannelPost<'_>)> = vec![
            ("empty channel", "", ChannelPost::new("<p>x</p>")),
            ("a chat", "19:x@thread.v2", ChannelPost::new("<p>x</p>")),
            ("the notes", "48:notes", ChannelPost::new("<p>x</p>")),
            (
                "a semicolon in the channel",
                "19:a;messageid=1@thread.tacv2",
                ChannelPost::new("<p>x</p>"),
            ),
            (
                "a slash in the channel",
                "19:a/b@thread.tacv2",
                ChannelPost::new("<p>x</p>"),
            ),
            ("no name", "19:@thread.tacv2", ChannelPost::new("<p>x</p>")),
            (
                "empty thread",
                CHANNEL,
                ChannelPost::new("<p>x</p>").in_thread(""),
            ),
            (
                "letters in the thread",
                CHANNEL,
                ChannelPost::new("<p>x</p>").in_thread("12a"),
            ),
            (
                "an address in the thread",
                CHANNEL,
                ChannelPost::new("<p>x</p>").in_thread("1;messageid=2"),
            ),
            (
                "a long thread",
                CHANNEL,
                ChannelPost::new("<p>x</p>").in_thread("123456789012345678901"),
            ),
            (
                "a subject on a reply",
                CHANNEL,
                ChannelPost::new("<p>x</p>")
                    .with_subject("Title")
                    .in_thread(ROOT),
            ),
            (
                "a blank subject",
                CHANNEL,
                ChannelPost::new("<p>x</p>").with_subject("  \t "),
            ),
            (
                "a subject of controls",
                CHANNEL,
                ChannelPost::new("<p>x</p>").with_subject("\u{1b}\u{7}"),
            ),
            (
                "a long subject",
                CHANNEL,
                ChannelPost::new("<p>x</p>").with_subject(&long_subject),
            ),
            ("no text", CHANNEL, ChannelPost::new("  \n")),
        ];
        for (why, channel, post) in refused {
            let result = b.post_to_channel(channel, &post);
            assert!(
                matches!(result, Err(Error::InvalidInput { .. })),
                "{why}: {result:?}"
            );
        }
        assert!(server.seen().is_empty(), "nothing was sent");
    }

    #[test]
    fn a_refusal_names_the_reason_and_never_the_text() {
        let server = serve(vec![]);
        let b = backend(&server);
        let e = b
            .post_to_channel(
                CHANNEL,
                &ChannelPost::new("<p>x</p>")
                    .with_subject("SECRET-TITLE")
                    .in_thread(ROOT),
            )
            .unwrap_err();
        assert!(!e.to_string().contains("SECRET"), "{e}");
        let e = b
            .post_to_channel("19:SECRET;x@thread.tacv2", &ChannelPost::new("<p>x</p>"))
            .unwrap_err();
        assert!(!e.to_string().contains("SECRET"), "{e}");
    }

    #[test]
    fn a_subject_is_trimmed_and_cleaned_and_two_hundred_characters_is_the_limit() {
        let server = serve(vec![
            Reply::status(201).json(ANSWER),
            Reply::status(201).json(ANSWER),
        ]);
        let b = backend(&server);
        let sent = b
            .post_to_channel(
                CHANNEL,
                &ChannelPost::new("<p>x</p>").with_subject("  Plan\u{1b}[31m\nB\u{202e}  "),
            )
            .unwrap();
        assert_eq!(sent.subject.as_deref(), Some("Plan[31m B"));
        assert_eq!(posted(&server, 0)["properties"]["subject"], "Plan[31m B");
        let exact = "é".repeat(MAX_SUBJECT_CHARS);
        b.post_to_channel(CHANNEL, &ChannelPost::new("<p>x</p>").with_subject(&exact))
            .unwrap();
        assert_eq!(
            posted(&server, 1)["properties"]["subject"].as_str(),
            Some(exact.as_str())
        );
    }

    #[test]
    fn a_refused_post_is_not_delivered_and_an_unconfirmed_one_may_have_been() {
        for (reply, delivered) in [
            (Reply::status(400).json(r#"{"message":"no"}"#), false),
            (
                Reply::status(403).json(r#"{"message":"not allowed"}"#),
                false,
            ),
            (Reply::status(404).json(r#"{"message":"gone"}"#), false),
            (
                Reply::status(429).header("Retry-After", "7").body(""),
                false,
            ),
            (Reply::status(500).json(r#"{"message":"boom"}"#), true),
            (Reply::status(504).json(r#"{"message":"slow"}"#), true),
            (Reply::hang_up(), true),
        ] {
            let server = serve(vec![reply]);
            let b = backend(&server);
            let e = b
                .post_to_channel(CHANNEL, &ChannelPost::new("<p>x</p>").in_thread(ROOT))
                .unwrap_err();
            assert_eq!(e.delivery_unknown(), delivered, "{e}");
            assert_eq!(
                server.seen().len(),
                1,
                "a write is never sent again by the library: {e}"
            );
        }
        let server = serve(vec![Reply::status(429).header("Retry-After", "7").body("")]);
        assert!(matches!(
            backend(&server).post_to_channel(CHANNEL, &ChannelPost::new("<p>x</p>")),
            Err(Error::Throttled { .. })
        ));
    }

    #[test]
    fn editing_a_post_repeats_its_subject_and_a_post_without_one_is_edited_like_a_chat_message() {
        let server = serve(vec![
            Reply::status(200).body(""),
            Reply::status(200).body(""),
        ]);
        let b = backend(&server);
        assert!(b.can_edit_posts());
        b.edit_post(CHANNEL, ROOT, Some(" Design review "), "<p>new</p>", &[])
            .unwrap();
        b.edit_post(CHANNEL, ROOT, None, "<p>new again</p>", &[])
            .unwrap();
        let seen = server.seen();
        for s in &seen {
            assert_eq!(s.method, "PUT");
            assert_eq!(s.path, format!("{CHANNEL_PATH}/messages/{ROOT}"));
        }
        let with = posted(&server, 0);
        assert_eq!(with["skypeeditedid"], ROOT);
        assert_eq!(with["content"], "<p>new</p>");
        assert_eq!(with["properties"]["subject"], "Design review");
        let without = posted(&server, 1);
        assert!(without["properties"].is_null(), "{without}");
        let mut keys: Vec<&str> = without
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "content",
                "contenttype",
                "imdisplayname",
                "messagetype",
                "skypeeditedid"
            ],
            "exactly what an edit in a chat sends"
        );
    }

    #[test]
    fn an_edit_of_a_post_that_cannot_be_sent_is_refused_before_any_request() {
        let server = serve(vec![]);
        let b = backend(&server);
        for (channel, id, subject, html) in [
            ("19:x@thread.v2", ROOT, None, "<p>x</p>"),
            (CHANNEL, "", None, "<p>x</p>"),
            (CHANNEL, "12x", None, "<p>x</p>"),
            (CHANNEL, ROOT, Some(" "), "<p>x</p>"),
            (CHANNEL, ROOT, None, " "),
        ] {
            assert!(
                matches!(
                    b.edit_post(channel, id, subject, html, &[]),
                    Err(Error::InvalidInput { .. })
                ),
                "{channel} {id} {subject:?} {html:?}"
            );
        }
        assert!(server.seen().is_empty());
    }

    const ADA: &str = "11111111-2222-3333-4444-555555555555";
    const GRACE: &str = "66666666-7777-8888-9999-aaaaaaaaaaaa";
    const TWO: &str =
        "<p><at id=\"0\">Ada Lovelace</at> and <at id=\"1\">Grace Hopper</at>, hello</p>";

    fn two() -> Vec<OutMention> {
        vec![
            OutMention {
                id: 0,
                user_id: ADA.to_string(),
                name: "Ada Lovelace".to_string(),
            },
            OutMention {
                id: 1,
                user_id: GRACE.to_string(),
                name: "Grace Hopper".to_string(),
            },
        ]
    }

    fn mentions_sent(body: &Value) -> Vec<Value> {
        let text = body["properties"]["mentions"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn a_post_with_two_mentions_is_a_chat_message_with_two_mentions() {
        let server = serve(vec![Reply::status(201).json(ANSWER)]);
        let people = two();
        let sent = backend(&server)
            .post_to_channel(
                CHANNEL,
                &ChannelPost::new(TWO)
                    .with_subject("Review")
                    .with_mentions(&people),
            )
            .unwrap();
        let body = posted(&server, 0);
        let span = |id: u32| {
            format!(
                "<span itemscope=\"\" itemtype=\"http://schema.skype.com/Mention\" itemid=\"{id}\">"
            )
        };
        let content = body["content"].as_str().unwrap();
        assert_eq!(
            content,
            format!(
                "<p>{}Ada Lovelace</span> and {}Grace Hopper</span>, hello</p>",
                span(0),
                span(1)
            )
        );
        assert!(!content.contains("<at"), "{content}");
        let list = mentions_sent(&body);
        assert_eq!(list.len(), 2);
        for (i, (id, name)) in [(ADA, "Ada Lovelace"), (GRACE, "Grace Hopper")]
            .iter()
            .enumerate()
        {
            assert_eq!(list[i]["@type"], "http://schema.skype.com/Mention");
            assert_eq!(list[i]["itemid"], i as u64);
            assert_eq!(list[i]["mri"], format!("8:orgid:{id}"));
            assert_eq!(list[i]["mentionType"], "person");
            assert_eq!(list[i]["displayName"], *name);
        }
        assert_eq!(body["properties"]["subject"], "Review");
        assert_eq!(sent.mentions, vec![ADA.to_string(), GRACE.to_string()]);
        assert_eq!(sent.body, content);
    }

    #[test]
    fn a_reply_carries_mentions_like_a_post_does() {
        let server = serve(vec![Reply::status(201).json(ANSWER)]);
        let people = [OutMention {
            id: 0,
            user_id: GRACE.to_string(),
            name: "Grace Hopper".to_string(),
        }];
        let sent = backend(&server)
            .post_to_channel(
                CHANNEL,
                &ChannelPost::new("<p><at id=\"0\">Grace Hopper</at> agreed</p>")
                    .in_thread(ROOT)
                    .with_mentions(&people),
            )
            .unwrap();
        let seen = server.seen();
        assert_eq!(
            seen[0].path,
            format!("{CHANNEL_PATH};messageid={ROOT}/messages")
        );
        let body = posted(&server, 0);
        assert!(
            body["content"]
                .as_str()
                .unwrap()
                .starts_with("<p><span itemscope=\"\" itemtype=\"http://schema.skype.com/Mention\" itemid=\"0\">Grace Hopper</span>")
        );
        let list = mentions_sent(&body);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["mri"], format!("8:orgid:{GRACE}"));
        assert_eq!(list[0]["itemid"], 0);
        assert_eq!(sent.mentions, vec![GRACE.to_string()]);
        assert_eq!(sent.root_id.as_deref(), Some(ROOT));
    }

    #[test]
    fn an_edit_with_a_mention_restates_it_beside_the_subject_and_one_without_sends_none() {
        let server = serve(vec![
            Reply::status(200).body(""),
            Reply::status(200).body(""),
            Reply::status(200).body(""),
        ]);
        let b = backend(&server);
        let people = two();
        b.edit_post(CHANNEL, ROOT, Some("Review"), TWO, &people)
            .unwrap();
        b.edit_post(CHANNEL, ROOT, None, TWO, &people).unwrap();
        b.edit_post(CHANNEL, ROOT, Some("Review"), "<p>nobody</p>", &[])
            .unwrap();
        let with_subject = posted(&server, 0);
        assert_eq!(with_subject["skypeeditedid"], ROOT);
        assert_eq!(with_subject["properties"]["subject"], "Review");
        let list = mentions_sent(&with_subject);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1]["mri"], format!("8:orgid:{GRACE}"));
        assert_eq!(list[1]["itemid"], 1);
        assert!(
            with_subject["content"]
                .as_str()
                .unwrap()
                .contains("itemid=\"1\">Grace Hopper</span>")
        );
        let without_subject = posted(&server, 1);
        assert!(without_subject["properties"]["subject"].is_null());
        assert_eq!(mentions_sent(&without_subject).len(), 2);
        let none = posted(&server, 2);
        assert_eq!(none["properties"]["subject"], "Review");
        assert!(
            none["properties"]["mentions"].is_null(),
            "no mentions key when there are none: {none}"
        );
    }

    #[test]
    fn a_hostile_or_inconsistent_mention_is_refused_before_any_request() {
        let server = serve(vec![]);
        let b = backend(&server);
        let one = |id: u32, user: &str, name: &str| OutMention {
            id,
            user_id: user.to_string(),
            name: name.to_string(),
        };
        let html = "<p><at id=\"0\">Ada</at></p>";
        let many: Vec<OutMention> = (0..=MAX_MENTIONS as u32)
            .map(|i| one(i, ADA, "Ada"))
            .collect();
        let many_html: String = (0..=MAX_MENTIONS)
            .map(|i| format!("<at id=\"{i}\">Ada</at>"))
            .collect();
        let long = "n".repeat(MAX_MENTION_NAME_CHARS + 1);
        let cases: Vec<(&str, &str, Vec<OutMention>)> = vec![
            (
                "an id that is not a guid",
                html,
                vec![one(0, "ada@x.test", "Ada")],
            ),
            (
                "an id that tries an address",
                html,
                vec![one(0, &format!("{ADA}/../x"), "Ada")],
            ),
            ("an empty id", html, vec![one(0, "", "Ada")]),
            ("a blank name", html, vec![one(0, ADA, "  ")]),
            (
                "a name of controls",
                html,
                vec![one(0, ADA, "A\u{1b}[31mda")],
            ),
            (
                "a name with a bidi mark",
                html,
                vec![one(0, ADA, "Ada\u{202e}")],
            ),
            ("a long name", html, vec![one(0, ADA, &long)]),
            (
                "the same number twice",
                "<p><at id=\"0\">Ada</at><at id=\"0\">Grace</at></p>",
                vec![one(0, ADA, "Ada"), one(0, GRACE, "Grace")],
            ),
            (
                "a mention the message lacks",
                "<p>Ada</p>",
                vec![one(0, ADA, "Ada")],
            ),
            (
                "a mention whose number is not in the message",
                html,
                vec![one(7, ADA, "Ada")],
            ),
            ("too many", many_html.as_str(), many),
        ];
        for (why, html, people) in &cases {
            let post = b.post_to_channel(CHANNEL, &ChannelPost::new(html).with_mentions(people));
            assert!(
                matches!(post, Err(Error::InvalidInput { .. })),
                "post, {why}: {post:?}"
            );
            let reply = b.post_to_channel(
                CHANNEL,
                &ChannelPost::new(html).in_thread(ROOT).with_mentions(people),
            );
            assert!(
                matches!(reply, Err(Error::InvalidInput { .. })),
                "reply, {why}: {reply:?}"
            );
            let edit = b.edit_post(CHANNEL, ROOT, None, html, people);
            assert!(
                matches!(edit, Err(Error::InvalidInput { .. })),
                "edit, {why}: {edit:?}"
            );
        }
        assert!(server.seen().is_empty(), "nothing was sent");
        let e = b
            .post_to_channel(
                CHANNEL,
                &ChannelPost::new("<p><at id=\"0\">SECRET-NAME</at></p>").with_mentions(&[one(
                    0,
                    "SECRET-ID",
                    "SECRET-NAME",
                )]),
            )
            .unwrap_err();
        assert!(!e.to_string().contains("SECRET"), "{e}");
    }

    #[test]
    fn a_backend_that_cannot_post_says_so_and_changes_nothing() {
        struct Nothing;
        impl crate::Backend for Nothing {
            fn label(&self) -> String {
                String::new()
            }
            fn me(&self) -> Result<crate::Me> {
                Err(Error::Unsupported {
                    operation: "x",
                    detail: "x",
                })
            }
            fn chats(&self) -> Result<Vec<crate::Chat>> {
                Ok(vec![])
            }
            fn find_people(&self, _: &str) -> Result<Vec<crate::Contact>> {
                Ok(vec![])
            }
            fn open_chat(&self, _: &crate::Contact) -> Result<crate::Chat> {
                Err(Error::Unsupported {
                    operation: "x",
                    detail: "x",
                })
            }
            fn messages(&self, _: &str, _: Option<&str>) -> Result<crate::Page> {
                Ok(crate::Page::default())
            }
            fn send(
                &self,
                _: &str,
                _: &str,
                _: &[crate::OutMention],
                _: Option<&crate::ReplyTarget>,
            ) -> Result<Msg> {
                Err(Error::Unsupported {
                    operation: "x",
                    detail: "x",
                })
            }
            fn serves_image(&self, _: &str) -> bool {
                false
            }
            fn image(&self, _: &str) -> Result<crate::image::RgbImage> {
                Err(Error::Unsupported {
                    operation: "x",
                    detail: "x",
                })
            }
        }
        assert!(!Nothing.can_post_to_channels());
        assert!(!Nothing.can_edit_posts());
        assert!(matches!(
            Nothing.post_to_channel(CHANNEL, &ChannelPost::new("<p>x</p>")),
            Err(Error::Unsupported { .. })
        ));
        assert!(matches!(
            Nothing.edit_post(CHANNEL, ROOT, None, "<p>x</p>", &[]),
            Err(Error::Unsupported { .. })
        ));
    }
}
