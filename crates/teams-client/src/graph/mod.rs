//! The Microsoft Graph backend: delegated permissions, device-code sign-in, `chats` and `users` endpoints.
//!
//! Sign in with [`GraphAuth`], keep the [`GraphTokens`] in a [`CredentialStore`], and hand both to
//! [`GraphBackend::new`].

mod auth;

pub use auth::{
    DEFAULT_CLIENT_ID, DEFAULT_TENANT, DeviceCodePrompt, GraphAuth, GraphConfig, GraphTokens,
    SCOPES,
};

use crate::emoji::reaction_emoji;
use crate::error::{AuthError, Error, Result};
use crate::headerlog::HeaderLog;
use crate::http::{self, Call, Headers, urlencode};
use crate::lock;
use crate::model::{
    Attachment, Backend, Chat, ChatKind, Contact, Delivery, Me, Member, Msg, OutMention, Page,
    Preview, Reaction, ReplyTarget,
};
use crate::record::ShapeRecorder;
use crate::shares;
use crate::store::CredentialStore;
use crate::text::{clean, html_to_plain};
use crate::watch::{self, Watch};
use chrono::{DateTime, Utc};
use image::RgbImage;
use serde_json::{Value, json};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BASE: &str = "https://graph.microsoft.com/v1.0";
const DEFAULT_MAX_CHATS: usize = 100;
const PAGE_SIZE: usize = 50;
/// How long one API request, and one picture download, may take altogether.
const API_TIMEOUT: Duration = Duration::from_secs(30);
const CDN_TIMEOUT: Duration = Duration::from_secs(20);

/// Whether `url` is content hosted by Graph, which is the only place this backend sends its bearer token.
fn is_media_url(url: &str) -> bool {
    url::Url::parse(url)
        .is_ok_and(|u| u.scheme() == "https" && u.host_str() == Some("graph.microsoft.com"))
}

/// Chats over Microsoft Graph.
pub struct GraphBackend {
    auth: GraphAuth,
    tokens: Mutex<GraphTokens>,
    grant_failed: Mutex<bool>,
    store: Arc<dyn CredentialStore>,
    warnings: Mutex<Vec<Error>>,
    api: ureq::Agent,
    cdn: ureq::Agent,
    /// `https://graph.microsoft.com/v1.0`, replaceable so tests can talk to a local server.
    base: String,
    recorder: Option<Arc<ShapeRecorder>>,
    max_chats: usize,
    /// Notices fields and values the service has not sent before (logged, names only).
    watch: Watch,
}

impl fmt::Debug for GraphBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GraphBackend")
            .field("auth", &self.auth)
            .field("grant_failed", &self.grant_failed)
            .finish_non_exhaustive()
    }
}

impl GraphBackend {
    /// A backend using `tokens`. Renewed tokens are saved to `store`; a failure to save is reported through
    /// [`Backend::take_warnings`] rather than failing the call that triggered the renewal.
    #[must_use]
    pub fn new(auth: GraphAuth, tokens: GraphTokens, store: Arc<dyn CredentialStore>) -> Self {
        Self {
            auth,
            tokens: Mutex::new(tokens),
            grant_failed: Mutex::new(false),
            store,
            warnings: Mutex::new(Vec::new()),
            api: http::agent(API_TIMEOUT),
            cdn: http::agent(CDN_TIMEOUT),
            base: BASE.to_string(),
            recorder: None,
            max_chats: DEFAULT_MAX_CHATS,
            watch: Watch::default(),
        }
    }

    /// Internal implementation for fetching images, with `skip_tenant_check` bypassing the tenant verification for
    /// explicitly requested content from outside the signed-in user's organization.
    fn fetch_image_impl(&self, url: &str, skip_tenant_check: bool) -> Result<RgbImage> {
        // Only Graph-hosted content is fetched: the bearer token must never go to a host named inside message
        // content, and unauthenticated fetches of sender-chosen URLs would leak our IP address.
        url::Url::parse(url).map_err(|_| Error::InvalidInput {
            what: "image url",
            value: url.to_string(),
        })?;
        // A pasted or shared picture is on the sender's OneDrive, not a Graph-hosted URL directly: resolved via
        // Graph's own Shares API (shares.rs, shared with the Teams web backend) to a download URL that is itself
        // already authenticated, fetched with no credential of ours. There is no list of hosts to check it against
        // (Graph chooses it), so `shares::is_download_url` refuses what is never a public download: not https, an
        // address instead of a name, `localhost`, a user name or an unusual port, for each redirect hop too. Only pictures
        // from the signed-in user's own tenant are auto-loaded; those from other tenants (or whose tenant cannot be
        // verified) are refused unless explicitly requested.
        if shares::is_sharepoint_url(url) {
            let v = self.request(
                "resolve shared file",
                "GET",
                &shares::shared_driveitem_url(&self.base, url),
                None,
            )?;
            if !skip_tenant_check {
                let content_tenant = shares::tenant_id(&v);
                let my_tenant = lock(&self.tokens).tenant_id();
                // A missing tenant ID on either side is unverified and not loaded automatically. The signed-in user's
                // tenant should always be present (it is a JWT claim), but a missing one there is treated the same way:
                // no download, so a missing or unreadable access token cannot silently weaken the check.
                if !shares::same_tenant(content_tenant.as_deref(), my_tenant.as_deref()) {
                    return Err(Error::UnverifiedContent {
                        operation: "fetch image",
                        url: url.to_string(),
                    });
                }
            }
            let download_url = shares::download_url(&v)?;
            return http::fetch_image(&self.cdn, &download_url, "", &shares::is_download_url);
        }
        if !self.serves_image(url) {
            return Err(Error::UntrustedUrl {
                url: url.to_string(),
            });
        }
        http::fetch_image(
            &self.cdn,
            url,
            &format!("Bearer {}", self.token(None)?),
            &|u| u.starts_with(&self.base) || is_media_url(u),
        )
    }

    /// Record the shape of every exchange (see [`ShapeRecorder`]).
    #[must_use]
    pub fn with_recorder(mut self, recorder: Arc<ShapeRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// Log the response headers `log` selects, one line per response (see [`HeaderLog`]), for diagnosing connection
    /// problems. Request headers are never logged, and credential-bearing response headers are withheld.
    #[must_use]
    pub fn with_header_log(mut self, log: HeaderLog) -> Self {
        let log = Arc::new(log);
        self.api = http::logging_agent(API_TIMEOUT, &log);
        self.cdn = http::logging_agent(CDN_TIMEOUT, &log);
        self.auth = self.auth.with_header_log(&log);
        self
    }

    /// How many recent chats [`Backend::chats`] returns at most (default 100, at least 1). Graph serves 50 per page,
    /// so this many chats cost `ceil(n / 50)` requests.
    #[must_use]
    pub fn with_max_chats(mut self, max: usize) -> Self {
        self.max_chats = max.max(1);
        self
    }

    #[cfg(test)]
    fn with_base(mut self, base: &str) -> Self {
        self.base = base.to_string();
        self
    }

    fn warn(&self, e: Error) {
        log::warn!("{}", e.for_log());
        lock(&self.warnings).push(e);
    }

    /// A valid access token, renewing under the lock when the current one is about to expire. `stale` is the token a
    /// caller just had refused with a 401, when this is a forced retry; if the cache has already moved past it
    /// (another caller renewed while this one waited for the lock), nothing more needs to happen; renewing again
    /// regardless, as a bare `force: bool` would, serves every concurrent 401 its own grant and rotates the refresh
    /// token once per caller instead of once per actual expiry.
    fn token(&self, stale: Option<&str>) -> Result<String> {
        if *lock(&self.grant_failed) {
            return Err(Error::Auth(AuthError::NotSignedIn));
        }
        let mut tokens = lock(&self.tokens);
        let outdated = match stale {
            Some(s) => tokens.access_token() == s,
            None => !tokens.fresh(),
        };
        if outdated {
            match self.auth.refresh(&tokens) {
                Ok(renewed) => {
                    if let Err(e) = renewed.save(self.store.as_ref()) {
                        self.warn(e.into());
                    }
                    *tokens = renewed;
                }
                Err(e) => {
                    if let Error::Auth(ref auth_err) = e
                        && auth_err.is_grant_problem()
                    {
                        *lock(&self.grant_failed) = true;
                    }
                    return Err(e);
                }
            }
        }
        Ok(tokens.access_token().to_string())
    }

    fn request(
        &self,
        operation: &'static str,
        method: &str,
        url: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        // The bearer token only ever goes to Graph, even if a paging link in a response said otherwise.
        if !url.starts_with(&self.base) {
            return Err(Error::UntrustedUrl {
                url: url.to_string(),
            });
        }
        let call = Call {
            agent: &self.api,
            recorder: self.recorder.as_deref(),
            operation,
            method,
            url,
            body,
        };
        let mut used_token: Option<String> = None;
        http::call_json(&call, &mut |renewed| -> Result<Headers> {
            let stale = renewed.then_some(used_token.as_deref()).flatten();
            let token = self.token(stale)?;
            used_token = Some(token.clone());
            let mut headers = vec![("Authorization", format!("Bearer {token}"))];
            if url.contains("$search=") {
                // Directory $search is an "advanced query"; Graph insists on this header for it.
                headers.push(("ConsistencyLevel", "eventual".to_string()));
            }
            Ok(headers)
        })
    }
    /// Keep the parseable items of `items`; if some could not be read, say so once as a warning, because that usually
    /// means the service changed its format and the list would otherwise just look shorter.
    fn parse_all<T>(
        &self,
        operation: &'static str,
        items: &[Value],
        parse: impl Fn(&Value) -> Option<T>,
    ) -> Vec<T> {
        let mut parsed = Vec::new();
        let mut unreadable = 0usize;
        for item in items {
            match parse(item) {
                Some(t) => parsed.push(t),
                None => {
                    unreadable += 1;
                    if unreadable <= 3 {
                        log::warn!(
                            "{operation}: unreadable item, shape {}",
                            watch::masked(item)
                        );
                    }
                }
            }
        }
        log::debug!(
            "{operation}: {} item(s), {} read, {unreadable} unreadable",
            items.len(),
            parsed.len()
        );
        if unreadable > 0 {
            self.warn(Error::Unexpected { operation, detail: "some items in the answer could not be read; the service format may have changed" });
        }
        parsed
    }
}

fn time(v: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v.as_str()?)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Sanitised optional string.
fn os(v: &Value) -> Option<String> {
    v.as_str().map(clean)
}

fn s(v: &Value) -> String {
    clean(v.as_str().unwrap_or(""))
}

fn parse_chat(v: &Value) -> Option<Chat> {
    let id = v["id"].as_str()?.to_string();
    let kind = match v["chatType"].as_str() {
        Some("oneOnOne") => ChatKind::OneOnOne,
        Some("meeting") => ChatKind::Meeting,
        _ => ChatKind::Group,
    };
    let members = v["members"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|m| Member {
                    not_in_directory: false,
                    id: s(&m["userId"]),
                    name: s(&m["displayName"]),
                })
                .collect()
        })
        .unwrap_or_default();
    let p = &v["lastMessagePreview"];
    let preview = time(&p["createdDateTime"]).map(|created| {
        let text = if p["isDeleted"].as_bool() == Some(true) {
            "message deleted".to_string()
        } else if p["body"]["contentType"].as_str() == Some("html") {
            html_to_plain(p["body"]["content"].as_str().unwrap_or(""))
        } else {
            p["body"]["content"].as_str().unwrap_or("").to_string()
        };
        Preview {
            created,
            from_id: os(&p["from"]["user"]["id"]),
            from_name: os(&p["from"]["user"]["displayName"]),
            text: clean(&text)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            // A message from an application has no user; Graph does not mark service notices otherwise.
            automated: false,
        }
    });
    // Graph's `lastUpdatedDateTime` is when the chat itself last changed (renamed, members added), which is not when
    // someone last wrote in it; the newest message is. A chat is as recent as the later of the two, so an active chat
    // that was never renamed is not ordered by the day it was created.
    let updated = time(&v["lastUpdatedDateTime"])
        .into_iter()
        .chain(preview.as_ref().map(|p| p.created))
        .max()
        .unwrap_or_default();
    Some(Chat {
        id,
        kind,
        topic: os(&v["topic"]),
        members,
        updated,
        preview,
        last_read: time(&v["viewpoint"]["lastMessageReadDateTime"]),
        web_url: v["webUrl"].as_str().map(String::from),
        favorite: None,
    })
}

fn parse_msg(v: &Value) -> Option<Msg> {
    let id = v["id"].as_str()?.to_string();
    let created = time(&v["createdDateTime"])?;
    let from = &v["from"]["user"];
    let app_name = v["from"]["application"]["displayName"].as_str();
    let html = v["body"]["contentType"].as_str() != Some("text");
    let mut reactions: Vec<Reaction> = Vec::new();
    for r in v["reactions"].as_array().into_iter().flatten() {
        // A reaction that does not say what it is cannot be shown as anything; inventing one (a thumbs-up) would put
        // words in someone's mouth.
        let Some(kind) = r["reactionType"].as_str().filter(|k| !k.trim().is_empty()) else {
            continue;
        };
        let emoji = reaction_emoji(kind);
        let who = os(&r["user"]["user"]["id"]);
        let name = s(&r["user"]["user"]["displayName"]);
        match reactions.iter_mut().find(|x| x.emoji == emoji) {
            Some(x) => {
                x.count += 1;
                x.users.extend(who.clone());
                if who.is_some() {
                    x.names.push(name);
                }
            }
            None => reactions.push(Reaction {
                emoji,
                count: 1,
                users: who.iter().cloned().collect(),
                names: if who.is_some() {
                    vec![name]
                } else {
                    Vec::new()
                },
            }),
        }
    }
    // An edit cannot happen before the message existed; see the same guard and its live-confirmed reason in
    // `teams::parse::parse_msg`.
    let edited = time(&v["lastEditedDateTime"]).filter(|t| *t > created);
    Some(Msg {
        id,
        created,
        modified: time(&v["lastModifiedDateTime"]).unwrap_or(created),
        edited,
        from_id: os(&from["id"]),
        from_name: clean(from["displayName"].as_str().or(app_name).unwrap_or("")),
        system: v["messageType"].as_str().is_some_and(|t| t != "message"),
        deleted: !v["deletedDateTime"].is_null(),
        html,
        body: s(&v["body"]["content"]),
        subject: os(&v["subject"]).filter(|x| !x.is_empty()),
        attachments: v["attachments"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|a| Attachment {
                name: s(&a["name"]),
                content_type: s(&a["contentType"]),
                url: os(&a["contentUrl"]).filter(|u| !u.is_empty()),
            })
            .collect(),
        reactions,
        // Documented, not verified live: a chatMessage's own top-level `mentions`, separate from the HTML body.
        mentions: v["mentions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| os(&m["mentioned"]["user"]["id"]))
            .collect(),
        delivery: Delivery::Sent,
        root_id: None,
    })
}

impl Backend for GraphBackend {
    fn label(&self) -> String {
        "Microsoft Graph".into()
    }

    fn me(&self) -> Result<Me> {
        let v = self.request(
            "read profile",
            "GET",
            &format!("{}/me?$select=id,displayName", self.base),
            None,
        )?;
        let id = s(&v["id"]);
        if id.is_empty() {
            return Err(Error::Unexpected {
                operation: "read profile",
                detail: "the profile has no id",
            });
        }
        Ok(Me {
            id,
            name: s(&v["displayName"]),
        })
    }

    fn chats(&self) -> Result<Vec<Chat>> {
        const OPERATION: &str = "list chats";
        let wanted = self.max_chats;
        let first = wanted.min(PAGE_SIZE);
        let mut url = format!(
            "{}/me/chats?$expand=members,lastMessagePreview&$orderby=lastMessagePreview/createdDateTime%20desc&$top={first}",
            self.base
        );
        let mut chats: Vec<Chat> = Vec::new();
        // The list is a window onto recent activity, not an archive: stop at the configured size, and after as many
        // pages as that size can need (a service that keeps answering with empty pages and a next link ends here).
        let mut pages_left = wanted.div_ceil(first.max(1)) + 5;
        while chats.len() < wanted && pages_left > 0 {
            pages_left -= 1;
            let v = self.request(OPERATION, "GET", &url, None)?;
            let items = v["value"].as_array().map_or(&[][..], Vec::as_slice);
            self.watch.fields("chat list answer", [&v]);
            self.watch.fields("chat", items);
            self.watch.tokens(
                "chat type",
                items.iter().filter_map(|c| c["chatType"].as_str()),
            );
            chats.extend(self.parse_all(OPERATION, items, parse_chat));
            match v["@odata.nextLink"].as_str() {
                Some(next) => url = next.to_string(),
                None => break,
            }
        }
        chats.sort_by_key(|c| std::cmp::Reverse(c.updated));
        chats.truncate(wanted);
        Ok(chats)
    }
    fn find_people(&self, query: &str) -> Result<Vec<Contact>> {
        let q: String = query
            .trim()
            .chars()
            .filter(|c| !matches!(c, '"' | '\\'))
            .collect();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let search = urlencode(&format!("\"displayName:{q}\" OR \"mail:{q}\""));
        let url = format!(
            "{}/users?$search={search}&$top=10&$select=id,displayName,mail,jobTitle,department,businessPhones,mobilePhone,officeLocation",
            self.base
        );
        let v = self.request("search people", "GET", &url, None)?;
        Ok(v["value"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|u| {
                let detail = [u["jobTitle"].as_str(), u["department"].as_str()]
                    .into_iter()
                    .flatten()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(Contact {
                    id: u["id"].as_str()?.to_string(),
                    name: s(&u["displayName"]),
                    email: s(&u["mail"]),
                    detail: clean(&detail),
                    // Graph's documented user properties: the first work number, else the mobile one.
                    phone: u["businessPhones"]
                        .as_array()
                        .and_then(|a| a.iter().find_map(Value::as_str))
                        .map(clean)
                        .filter(|p| !p.is_empty())
                        .unwrap_or_else(|| s(&u["mobilePhone"])),
                    office: s(&u["officeLocation"]),
                })
            })
            .collect())
    }

    fn open_chat(&self, who: &Contact) -> Result<Chat> {
        // The id goes into an OData URL below, so it must be a plain GUID.
        if who.id.is_empty() || !who.id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Err(Error::InvalidInput {
                what: "contact id",
                value: who.id.clone(),
            });
        }
        let me = self.me()?;
        let member = |id: &str| {
            json!({
                "@odata.type": "#microsoft.graph.aadUserConversationMember",
                "roles": ["owner"],
                "user@odata.bind": format!("{}/users('{id}')", self.base)
            })
        };
        // Graph returns the existing chat when there already is a one-on-one chat with this person.
        let body = json!({ "chatType": "oneOnOne", "members": [member(&me.id), member(&who.id)] });
        let v = self.request(
            "open chat",
            "POST",
            &format!("{}/chats", self.base),
            Some(&body),
        )?;
        let mut chat = parse_chat(&v).ok_or(Error::Unexpected {
            operation: "open chat",
            detail: "the new chat has no id",
        })?;
        chat.members = vec![
            Member {
                not_in_directory: false,
                id: me.id,
                name: me.name,
            },
            Member {
                not_in_directory: false,
                id: who.id.clone(),
                name: who.name.clone(),
            },
        ];
        if chat.preview.is_none() {
            chat.updated = Utc::now();
        }
        Ok(chat)
    }

    fn messages(&self, chat_id: &str, older: Option<&str>) -> Result<Page> {
        const OPERATION: &str = "read messages";
        let url = match older {
            Some(cursor) if cursor.starts_with(&self.base) => cursor.to_string(),
            Some(cursor) => {
                return Err(Error::UntrustedUrl {
                    url: cursor.to_string(),
                });
            }
            None => format!(
                "{}/chats/{}/messages?$top=50",
                self.base,
                urlencode(chat_id)
            ),
        };
        let v = self.request(OPERATION, "GET", &url, None)?;
        let items = v["value"].as_array().map_or(&[][..], Vec::as_slice);
        self.watch.fields("messages answer", [&v]);
        self.watch.fields("message", items);
        self.watch
            .fields("message body", items.iter().map(|m| &m["body"]));
        self.watch.tokens(
            "message type",
            items.iter().filter_map(|m| m["messageType"].as_str()),
        );
        let mut messages = self.parse_all(OPERATION, items, parse_msg);
        messages.sort_by_key(|m| m.created);
        Ok(Page {
            messages,
            older: v["@odata.nextLink"].as_str().map(String::from),
        })
    }

    fn send(
        &self,
        chat_id: &str,
        html: &str,
        mentions: &[OutMention],
        // Not implemented: Graph's own reply microformat has not been verified live (see `AGENTS.md`, "Not verified against a live service or real terminal").
        // Sending the message without a quote, rather than refusing, keeps a reply usable on this backend.
        _reply_to: Option<&ReplyTarget>,
    ) -> Result<Msg> {
        let mentions: Vec<Value> = mentions
            .iter()
            .map(|m| {
                json!({
                    "id": m.id,
                    "mentionText": m.name,
                    "mentioned": { "user": { "id": m.user_id, "displayName": m.name, "userIdentityType": "aadUser" } }
                })
            })
            .collect();
        let body =
            json!({ "body": { "contentType": "html", "content": html }, "mentions": mentions });
        let v = self.request(
            "send message",
            "POST",
            &format!("{}/chats/{}/messages", self.base, urlencode(chat_id)),
            Some(&body),
        )?;
        parse_msg(&v).ok_or(Error::Unexpected {
            operation: "send message",
            detail: "Graph accepted the message but returned no message",
        })
    }

    fn react(&self, chat_id: &str, message_id: &str, emoji: &str, on: bool) -> Result<()> {
        const OPERATION: &str = "react to message";
        // Message ids are the time the service stored them, in milliseconds.
        if message_id.is_empty() || !message_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::InvalidInput {
                what: "message id",
                value: message_id.to_string(),
            });
        }
        // Graph's documented reactions are the six classic ones.
        let Some(reaction) = crate::emoji::classic_key(emoji) else {
            return Err(Error::InvalidInput {
                what: "reaction (Graph supports 👍 ❤️ 😆 😮 🙁 😠)",
                value: emoji.to_string(),
            });
        };
        let action = if on { "setReaction" } else { "unsetReaction" };
        let url = format!(
            "{}/chats/{}/messages/{}/{action}",
            self.base,
            urlencode(chat_id),
            urlencode(message_id)
        );
        self.request(
            OPERATION,
            "POST",
            &url,
            Some(&json!({ "reactionType": reaction })),
        )
        .map(|_| ())
    }

    fn reaction_emoji(&self) -> Result<Vec<crate::model::EmojiInfo>> {
        Ok(crate::emoji::CLASSIC
            .iter()
            .map(|(word, emoji)| crate::model::EmojiInfo {
                emoji: (*emoji).to_string(),
                name: (*word).to_string(),
                aliases: Vec::new(),
            })
            .collect())
    }

    fn serves_image(&self, url: &str) -> bool {
        is_media_url(url) || shares::is_sharepoint_url(url)
    }

    fn image(&self, url: &str) -> Result<RgbImage> {
        self.fetch_image_impl(url, false)
    }

    fn image_from_outside(&self, url: &str) -> Result<RgbImage> {
        self.fetch_image_impl(url, true)
    }

    fn take_warnings(&self) -> Vec<Error> {
        let mut warnings = std::mem::take(&mut *lock(&self.warnings));
        warnings.extend(
            self.recorder
                .as_ref()
                .and_then(|r| r.take_failure())
                .map(Error::from),
        );
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::StoreError;
    use crate::store::MemoryStore;
    use crate::testserver::{Reply, Server, serve};

    fn backend(server: &Server) -> GraphBackend {
        shares::allow_download_origin_for_tests(server.origin());
        GraphBackend::new(
            GraphAuth::new(GraphConfig::default()),
            GraphTokens::for_test("test-token"),
            Arc::new(MemoryStore::new()),
        )
        .with_base(server.origin())
    }

    fn backend_with_tenant(server: &Server, tenant: &str) -> GraphBackend {
        shares::allow_download_origin_for_tests(server.origin());
        // Create a JWT with the given tenant ID in the tid claim.
        let payload = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            format!(r#"{{"tid": "{}"}}"#, tenant).as_bytes(),
        );
        let token = format!("header.{}.signature", payload);
        GraphBackend::new(
            GraphAuth::new(GraphConfig::default()),
            GraphTokens::for_test(&token),
            Arc::new(MemoryStore::new()),
        )
        .with_base(server.origin())
    }

    fn expired_tokens() -> GraphTokens {
        GraphTokens::expired_for_test("expired")
    }

    fn backend_with_auth(server: &Server, tokens: GraphTokens) -> GraphBackend {
        shares::allow_download_origin_for_tests(server.origin());
        GraphBackend::new(
            GraphAuth::new(GraphConfig::default()).with_login_base(server.origin()),
            tokens,
            Arc::new(MemoryStore::new()),
        )
        .with_base(server.origin())
    }

    #[test]
    fn revoked_grant_stops_retrying_the_token_endpoint() {
        let server =
            serve(vec![Reply::status(400).json(
                r#"{"error":"invalid_grant","error_description":"Token expired"}"#,
            )]);
        let b = backend_with_auth(&server, expired_tokens());
        let e1 = b.chats().unwrap_err();
        assert!(e1.needs_sign_in());
        let e2 = b.chats().unwrap_err();
        assert!(e2.needs_sign_in());
        assert_eq!(server.seen().len(), 1, "only one token request was sent");
    }

    #[test]
    fn transient_token_endpoint_failure_is_retried() {
        let server = serve(vec![
            Reply::status(503).body("Service Unavailable"),
            Reply::status(200).json(
                r#"{"access_token":"renewed","expires_in":3600,"refresh_token":"new-refresh"}"#,
            ),
            Reply::status(200).json(r#"{"value":[]}"#),
        ]);
        let b = backend_with_auth(&server, expired_tokens());
        let e = b.chats().unwrap_err();
        assert!(
            !e.needs_sign_in(),
            "503 is transient, not a sign-in problem"
        );
        let chats = b.chats().unwrap();
        assert_eq!(chats.len(), 0);
        assert_eq!(
            server.seen().len(),
            3,
            "token request failed, then succeeded, then list chats"
        );
    }

    #[test]
    fn message_search_is_not_supported_and_nothing_is_sent_for_it() {
        let server = serve(vec![]);
        let b = backend(&server);
        // Graph takes the trait's own answer: no search, said as `Unsupported`, with no request.
        assert!(!b.can_search_messages());
        assert!(matches!(
            b.search_messages("19:x@thread.v2", "report", 0),
            Err(Error::Unsupported {
                operation: "search messages",
                ..
            })
        ));
        assert!(!b.can_search_conversations());
        assert!(matches!(
            b.search_conversations(&["19:x@thread.v2".to_string()], "report", 0),
            Err(Error::Unsupported {
                operation: "search messages",
                ..
            })
        ));
        assert!(server.seen().is_empty());
    }

    #[test]
    fn a_forced_renewal_does_nothing_once_another_caller_has_already_renewed_the_stale_token() {
        let server = serve(vec![]);
        let b = backend(&server);
        let stale = lock(&b.tokens).access_token().to_string();
        let renewed = GraphTokens::for_test("renewed-while-waiting-for-the-lock");
        *lock(&b.tokens) = renewed.clone();
        let got = b
            .token(Some(&stale))
            .expect("the cache has already moved past the stale token");
        assert_eq!(
            got,
            renewed.access_token(),
            "the token another caller already renewed is returned as is"
        );
        assert!(
            server.seen().is_empty(),
            "no request was sent: nothing needed renewing"
        );
    }

    #[test]
    fn a_sharepoint_attachment_is_also_fetchable() {
        let server = serve(vec![]);
        let b = backend(&server);
        assert!(b.serves_image("https://contoso-my.sharepoint.com/personal/bob/doc.png"));
    }

    #[test]
    fn a_sharepoint_attachment_from_the_same_tenant_is_resolved_and_downloaded() {
        let server = serve(vec![
            Reply::status(200).json(
                &json!({
                    "@microsoft.graph.downloadUrl": "{origin}/dl",
                    "sharepointIds": { "tenantId": "my-tenant" }
                })
                .to_string(),
            ),
            Reply::status(200).body("not an image"),
        ]);
        let b = backend_with_tenant(&server, "my-tenant");
        let url = "https://contoso-my.sharepoint.com/personal/bob/doc.png";
        let err = b.image(url).unwrap_err();
        assert!(
            matches!(err, Error::Decode { .. }),
            "reached and tried to decode the resolved download: {err:?}"
        );
        let seen = server.seen();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].path.contains("/shares/u!"), "{}", seen[0].path);
        assert!(
            seen[0]
                .header("authorization")
                .unwrap()
                .starts_with("Bearer ")
        );
        assert_eq!(seen[1].path, "/dl");
        assert!(
            seen[1].header("authorization").is_none(),
            "the pre-authenticated download gets no credential of ours"
        );
    }

    #[test]
    fn a_sharepoint_attachment_from_another_tenant_is_refused() {
        let server = serve(vec![
            Reply::status(200).json(
                &json!({
                    "@microsoft.graph.downloadUrl": "https://external.example/dl",
                    "sharepointIds": { "tenantId": "different-tenant" }
                })
                .to_string(),
            ),
        ]);
        let b = backend_with_tenant(&server, "my-tenant");
        let url = "https://external-my.sharepoint.com/personal/attacker/malicious.png";
        let err = b.image(url).unwrap_err();
        assert!(
            matches!(
                err,
                Error::UnverifiedContent {
                    operation: "fetch image",
                    ..
                }
            ),
            "should refuse: {err:?}"
        );
        let seen = server.seen();
        assert_eq!(seen.len(), 1, "should not have downloaded");
    }

    /// Logging response headers is switched on once for the backend and has to reach every agent it makes requests
    /// with: the API one (resolving the shared file) and the picture one (downloading it).
    #[test]
    fn header_logging_covers_the_api_and_picture_agents_and_never_the_credentials() {
        let server = serve(vec![
            Reply::status(200).header("X-Cache", "HIT").json(
                &json!({
                    "@microsoft.graph.downloadUrl": "{origin}/dl",
                    "sharepointIds": { "tenantId": "my-tenant" }
                })
                .to_string(),
            ),
            Reply::status(200)
                .header("X-Cache", "MISS")
                .body("not an image"),
        ]);
        let log = HeaderLog::parse("x-cache,authorization").unwrap();
        let b = backend_with_tenant(&server, "my-tenant").with_header_log(log.clone());
        let _ = b.image("https://contoso-my.sharepoint.com/personal/bob/doc.png");
        let lines = log.captured();
        assert_eq!(lines.len(), 2, "one for each agent: {lines:?}");
        assert!(lines[0].contains("x-cache=HIT"), "{}", lines[0]);
        assert!(lines[1].contains("x-cache=MISS"), "{}", lines[1]);
        assert!(lines.iter().all(|l| !l.contains("Bearer")), "{lines:?}");
    }

    #[test]
    fn a_picture_from_another_tenant_is_downloaded_only_when_the_user_asks_for_it() {
        let server = serve(vec![
            Reply::status(200).json(
                &json!({
                    "@microsoft.graph.downloadUrl": "{origin}/dl",
                    "sharepointIds": { "tenantId": "different-tenant" }
                })
                .to_string(),
            ),
            Reply::status(200).body("not an image"),
        ]);
        let b = backend_with_tenant(&server, "my-tenant");
        let url = "https://external-my.sharepoint.com/personal/someone/pic.png";
        let err = b.image_from_outside(url).unwrap_err();
        assert!(
            matches!(err, Error::Decode { .. }),
            "the explicit call reached the download: {err:?}"
        );
        let seen = server.seen();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].path, "/dl");
        assert!(
            seen[1].header("authorization").is_none(),
            "an outside download still gets no credential of ours"
        );
    }

    #[test]
    fn a_sharepoint_attachment_with_no_tenant_id_is_refused() {
        let server =
            serve(vec![
                Reply::status(200).json(
                    &json!({ "@microsoft.graph.downloadUrl": "https://external.example/dl" })
                        .to_string(),
                ),
            ]);
        let b = backend_with_tenant(&server, "my-tenant");
        let url = "https://unknown-my.sharepoint.com/personal/x/pic.png";
        let err = b.image(url).unwrap_err();
        assert!(
            matches!(err, Error::UnverifiedContent { .. }),
            "missing tenant ID should be unverified: {err:?}"
        );
    }

    /// Graph chooses the download URL and nothing checks it against a list, so it has to be what a public download
    /// is: https, a DNS name, no user name, the ordinary port. Anything else is refused before a request is made.
    #[test]
    fn a_download_url_that_is_not_a_public_https_name_is_never_requested() {
        let elsewhere = serve(vec![Reply::status(200).body("not an image")]);
        for target in shares::refused_download_targets(elsewhere.origin()) {
            let server = serve(vec![
                Reply::status(200).json(
                    &json!({
                        "@microsoft.graph.downloadUrl": target,
                        "sharepointIds": { "tenantId": "my-tenant" }
                    })
                    .to_string(),
                ),
            ]);
            let b = backend_with_tenant(&server, "my-tenant");
            let err = b
                .image("https://contoso-my.sharepoint.com/personal/bob/doc.png")
                .unwrap_err();
            assert!(
                matches!(err, Error::UntrustedUrl { .. }),
                "{target}: {err:?}"
            );
            assert_eq!(server.seen().len(), 1, "{target}: only the lookup was sent");
        }
        assert!(
            elsewhere.seen().is_empty(),
            "nothing reached the other server"
        );
    }

    /// The same rule for every hop: a download that redirects somewhere it should not go is not followed there.
    #[test]
    fn a_download_that_redirects_to_something_that_is_not_a_public_https_name_is_not_followed() {
        let elsewhere = serve(vec![Reply::status(200).body("not an image")]);
        for target in shares::refused_download_targets(elsewhere.origin()) {
            let server = serve(vec![
                Reply::status(200).json(
                    &json!({
                        "@microsoft.graph.downloadUrl": "{origin}/dl",
                        "sharepointIds": { "tenantId": "my-tenant" }
                    })
                    .to_string(),
                ),
                Reply::status(302).header("Location", &target),
            ]);
            let b = backend_with_tenant(&server, "my-tenant");
            let err = b
                .image("https://contoso-my.sharepoint.com/personal/bob/doc.png")
                .unwrap_err();
            assert!(
                matches!(err, Error::UntrustedUrl { .. }),
                "{target}: {err:?}"
            );
            assert_eq!(
                server.seen().len(),
                2,
                "{target}: the lookup and the first hop"
            );
        }
        assert!(
            elsewhere.seen().is_empty(),
            "no hop reached the other server"
        );
    }

    #[test]
    fn parses_a_chat() {
        let v = json!({
            "id": "19:abc@unq.gbl.spaces", "chatType": "oneOnOne", "topic": null,
            "lastUpdatedDateTime": "2026-10-27T10:00:00Z",
            "members": [{"userId": "u1", "displayName": "Ada"}, {"userId": "me", "displayName": "Me"}],
            "viewpoint": {"lastMessageReadDateTime": "2026-10-27T09:00:00Z"},
            "lastMessagePreview": {"createdDateTime": "2026-10-27T10:00:00Z", "isDeleted": false,
              "from": {"user": {"id": "u1", "displayName": "Ada"}},
              "body": {"contentType": "html", "content": "<p>Hello <b>there</b></p>"}}
        });
        let c = parse_chat(&v).unwrap();
        assert_eq!(c.kind, ChatKind::OneOnOne);
        assert_eq!(c.title("me"), "Ada");
        assert!(c.is_unread("me"));
        assert!(!c.is_unread("u1"));
        assert_eq!(c.preview.unwrap().text, "Hello there");
    }

    #[test]
    fn a_chat_is_as_recent_as_its_newest_message_not_only_its_last_rename() {
        let chat = |id: &str, updated: &str, newest: &str| {
            json!({"id": id, "chatType": "group", "topic": id,
                "lastUpdatedDateTime": updated,
                "lastMessagePreview": {"createdDateTime": newest, "isDeleted": false,
                    "from": {"user": {"id": "u1", "displayName": "Ada"}},
                    "body": {"contentType": "text", "content": "hi"}}})
        };
        // Created long ago and never renamed, but written in this morning.
        let active = parse_chat(&chat(
            "active",
            "2025-01-01T00:00:00Z",
            "2026-10-27T09:00:00Z",
        ))
        .unwrap();
        // Renamed recently, quiet since.
        let renamed = parse_chat(&chat(
            "renamed",
            "2026-10-20T00:00:00Z",
            "2026-10-01T00:00:00Z",
        ))
        .unwrap();
        assert!(
            active.updated > renamed.updated,
            "{:?} {:?}",
            active.updated,
            renamed.updated
        );
        assert_eq!(
            renamed.updated.to_rfc3339(),
            "2026-10-20T00:00:00+00:00",
            "a rename later than the last message still counts"
        );
        // Either alone is used when the other is missing.
        let no_preview =
            json!({"id": "n", "chatType": "group", "lastUpdatedDateTime": "2026-10-02T00:00:00Z"});
        assert_eq!(
            parse_chat(&no_preview).unwrap().updated.to_rfc3339(),
            "2026-10-02T00:00:00+00:00"
        );
    }

    #[test]
    fn parses_a_message_with_reactions_and_mentions() {
        let v = json!({
            "id": "1", "messageType": "message", "createdDateTime": "2026-10-27T10:00:00Z",
            "from": {"user": {"id": "u1", "displayName": "Ada"}},
            "body": {"contentType": "html", "content": "<at id=\"0\">Bob</at> hi"},
            "reactions": [{"reactionType": "like"}, {"reactionType": "like"}, {"reactionType": "heart"}],
            "attachments": [{"name": "spec.docx", "contentType": "reference", "contentUrl": "https://x/spec.docx"}],
            "mentions": [{"id": 0, "mentionText": "Bob", "mentioned": {"user": {"id": "u2", "displayName": "Bob"}}}]
        });
        let m = parse_msg(&v).unwrap();
        assert_eq!(m.from_name, "Ada");
        assert!(m.html && !m.system && !m.deleted);
        assert_eq!(m.reactions.len(), 2);
        assert_eq!(m.reactions[0].count, 2);
        assert_eq!(m.attachments[0].name, "spec.docx");
        assert_eq!(m.mentions, vec!["u2".to_string()]);
        assert!(m.mentions_me("u2") && !m.mentions_me("u1"));
    }

    #[test]
    fn a_reaction_without_a_type_is_left_out_not_shown_as_a_thumbs_up() {
        let v = json!({
            "id": "1", "messageType": "message", "createdDateTime": "2026-10-27T10:00:00Z",
            "from": {"user": {"id": "u1", "displayName": "Ada"}},
            "body": {"contentType": "text", "content": "hi"},
            "reactions": [{"user": {"user": {"id": "u2"}}}, {"reactionType": ""}, {"reactionType": "heart"}]
        });
        let m = parse_msg(&v).unwrap();
        assert_eq!(m.reactions.len(), 1, "{:?}", m.reactions);
        assert_eq!(m.reactions[0].count, 1);
        assert_ne!(m.reactions[0].emoji, "👍");
    }

    #[test]
    fn an_edit_time_before_the_message_was_created_is_not_a_real_edit() {
        let v = json!({"id": "1", "messageType": "message", "createdDateTime": "2026-10-27T10:00:00Z",
            "lastEditedDateTime": "2026-10-27T09:59:00Z",
            "from": {"user": {"id": "u1", "displayName": "Ada"}},
            "body": {"contentType": "html", "content": "<p>hi</p>"}});
        let m = parse_msg(&v).unwrap();
        assert_eq!(m.edited, None, "{:?}", m.edited);
        assert_eq!(m.modified, m.created);
    }

    #[test]
    fn system_messages_are_flagged() {
        let v = json!({"id": "2", "messageType": "systemEventMessage", "createdDateTime": "2026-10-27T10:00:00Z",
                       "body": {"contentType": "html", "content": "<systemEventMessage/>"}});
        assert!(parse_msg(&v).unwrap().system);
    }

    #[test]
    fn only_graph_hosted_media_counts() {
        assert!(is_media_url(
            "https://graph.microsoft.com/v1.0/chats/x/hostedContents/1/$value"
        ));
        assert!(!is_media_url("https://graph.microsoft.com.evil.example/x"));
        assert!(!is_media_url("http://graph.microsoft.com/x"));
    }

    #[test]
    fn a_reaction_is_set_or_unset_by_its_classic_name() {
        let server = serve(vec![
            Reply::status(204),
            Reply::status(204),
            Reply::status(204),
        ]);
        let b = backend(&server);
        b.react("19:x@thread.v2", "1700", "👍", true).unwrap();
        b.react("19:x@thread.v2", "1700", "❤️", true).unwrap();
        b.react("19:x@thread.v2", "1700", "👍", false).unwrap();
        let seen = server.seen();
        assert!(seen.iter().all(|s| s.method == "POST"));
        assert!(
            seen[0]
                .path
                .ends_with("/chats/19%3Ax@thread.v2/messages/1700/setReaction"),
            "{}",
            seen[0].path
        );
        assert!(
            seen[2].path.ends_with("/messages/1700/unsetReaction"),
            "{}",
            seen[2].path
        );
        let reaction = |i: usize| {
            serde_json::from_str::<Value>(&seen[i].body).unwrap()["reactionType"].clone()
        };
        assert_eq!(
            (reaction(0), reaction(1), reaction(2)),
            (json!("like"), json!("heart"), json!("like"))
        );
        for refused in ["🎉", "two words", ""] {
            assert!(
                matches!(
                    b.react("19:x@thread.v2", "1700", refused, true),
                    Err(Error::InvalidInput { .. })
                ),
                "{refused:?}"
            );
        }
        assert_eq!(server.seen().len(), 3, "refused before any request");
    }

    #[test]
    fn the_backend_serves_only_graph_hosted_images() {
        let server = serve(vec![]);
        let b = backend(&server);
        assert!(b.serves_image("https://graph.microsoft.com/v1.0/chats/x/hostedContents/1/$value"));
        assert!(!b.serves_image("https://tracker.example/p.png"));
    }

    #[test]
    fn requests_carry_the_bearer_token_and_chats_come_back_newest_first() {
        let page = json!({"value": [
            {"id": "old", "chatType": "group", "lastUpdatedDateTime": "2026-10-01T00:00:00Z"},
            {"id": "new", "chatType": "group", "lastUpdatedDateTime": "2026-10-02T00:00:00Z"}
        ]});
        let server = serve(vec![Reply::status(200).json(&page.to_string())]);
        let chats = backend(&server).chats().unwrap();
        assert_eq!(
            chats.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["new", "old"]
        );
        let seen = server.seen();
        assert_eq!(
            (seen[0].method.as_str(), seen[0].header("authorization")),
            ("GET", Some("Bearer test-token"))
        );
        assert!(seen[0].path.starts_with("/me/chats?"), "{}", seen[0].path);
    }

    #[test]
    fn people_search_sends_the_consistency_header_and_encodes_the_query() {
        let server = serve(vec![Reply::status(200).json(r#"{"value":[{"id":"u1","displayName":"Ada Lovelace","mail":"ada@x.test","jobTitle":"Analyst","department":"Engines"}]}"#)]);
        let people = backend(&server).find_people("Ada \"Lovelace\"").unwrap();
        assert_eq!(
            (
                people[0].id.as_str(),
                people[0].email.as_str(),
                people[0].detail.as_str()
            ),
            ("u1", "ada@x.test", "Analyst, Engines")
        );
        let seen = server.seen();
        assert_eq!(seen[0].header("consistencylevel"), Some("eventual"));
        assert!(
            seen[0].path.contains("%22displayName%3AAda%20Lovelace%22"),
            "quotes in the query are dropped, the rest encoded: {}",
            seen[0].path
        );
        assert!(
            seen[0]
                .path
                .contains("businessPhones,mobilePhone,officeLocation"),
            "{}",
            seen[0].path
        );
    }

    #[test]
    fn a_person_found_has_the_first_work_number_else_the_mobile_one_and_an_office() {
        let server = serve(vec![Reply::status(200).json(
            r#"{"value":[
                {"id":"u1","displayName":"Ada","businessPhones":["+1 555 0100","+1 555 0109"],"mobilePhone":"+1 555 0101","officeLocation":"Building 4"},
                {"id":"u2","displayName":"Grace","businessPhones":[],"mobilePhone":"+1 555 0102"},
                {"id":"u3","displayName":"Alan"}
            ]}"#,
        )]);
        let people = backend(&server).find_people("a").unwrap();
        let shown: Vec<(&str, &str)> = people
            .iter()
            .map(|p| (p.phone.as_str(), p.office.as_str()))
            .collect();
        assert_eq!(
            shown,
            [("+1 555 0100", "Building 4"), ("+1 555 0102", ""), ("", "")]
        );
    }

    #[test]
    fn service_errors_are_typed_with_the_services_own_message() {
        let server = serve(vec![
            Reply::status(404).json(r#"{"error":{"code":"NotFound","message":"Chat not found"}}"#),
            Reply::status(429).header("Retry-After", "2").body(""),
        ]);
        let b = backend(&server);
        match b.messages("19:x", None) {
            Err(Error::Status {
                operation: "read messages",
                status: 404,
                message,
            }) => assert_eq!(message, "Chat not found"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            b.chats(),
            Err(Error::Throttled {
                operation: "list chats",
                retry_after: Some(_)
            })
        ));
    }

    #[test]
    fn paging_links_pointing_elsewhere_are_refused_without_a_request() {
        let server = serve(vec![]);
        let err = backend(&server)
            .messages("19:x", Some("https://evil.example/steal?token=1"))
            .unwrap_err();
        assert!(matches!(err, Error::UntrustedUrl { .. }), "{err:?}");
        assert!(server.seen().is_empty(), "nothing was sent");
    }

    #[test]
    fn unreadable_items_produce_a_warning_not_a_silently_shorter_list() {
        let page = json!({"value": [{"id": "ok", "chatType": "group"}, {"no_id": true}]});
        let server = serve(vec![Reply::status(200).json(&page.to_string())]);
        let b = backend(&server);
        assert_eq!(b.chats().unwrap().len(), 1);
        let warnings = b.take_warnings();
        assert!(
            matches!(
                warnings.as_slice(),
                [Error::Unexpected {
                    operation: "list chats",
                    ..
                }]
            ),
            "{warnings:?}"
        );
        assert!(b.take_warnings().is_empty());
    }

    #[test]
    fn a_profile_without_an_id_is_an_error() {
        let server = serve(vec![Reply::status(200).json(r#"{"displayName":"Nobody"}"#)]);
        assert!(matches!(
            backend(&server).me(),
            Err(Error::Unexpected {
                operation: "read profile",
                ..
            })
        ));
    }

    #[test]
    fn opening_a_chat_posts_both_members_and_returns_the_chat_with_names() {
        let server = serve(vec![
            Reply::status(200).json(r#"{"id":"me-id","displayName":"Me"}"#),
            Reply::status(201).json(r#"{"id":"19:new@unq.gbl.spaces","chatType":"oneOnOne"}"#),
        ]);
        let who = Contact {
            id: "abcd-1234".into(),
            name: "Ada".into(),
            email: String::new(),
            detail: String::new(),
            ..Default::default()
        };
        let chat = backend(&server).open_chat(&who).unwrap();
        assert_eq!(chat.id, "19:new@unq.gbl.spaces");
        assert_eq!(
            chat.members
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["Me", "Ada"]
        );
        let body: Value = serde_json::from_str(&server.seen()[1].body).unwrap();
        assert_eq!(body["chatType"], "oneOnOne");
        assert!(
            body["members"][1]["user@odata.bind"]
                .as_str()
                .unwrap()
                .ends_with("/users('abcd-1234')")
        );
    }

    #[test]
    fn opening_a_chat_rejects_ids_that_are_not_guids_before_any_request() {
        let server = serve(vec![]);
        let who = Contact {
            id: "x') or ('1'='1".into(),
            name: String::new(),
            email: String::new(),
            detail: String::new(),
            ..Default::default()
        };
        assert!(matches!(
            backend(&server).open_chat(&who),
            Err(Error::InvalidInput {
                what: "contact id",
                ..
            })
        ));
        assert!(server.seen().is_empty());
    }

    fn numbered_chats(range: std::ops::Range<u32>) -> Value {
        Value::Array(
            range
                .map(|n| json!({"id": format!("c{n}"), "chatType": "group", "lastUpdatedDateTime": format!("2026-10-01T00:{:02}:00Z", n % 60)}))
                .collect(),
        )
    }

    #[test]
    fn the_chat_list_follows_next_links_until_it_has_the_configured_number() {
        let page1 =
            json!({"value": numbered_chats(0..50), "@odata.nextLink": "{origin}/me/chats?page=2"});
        let page2 = json!({"value": numbered_chats(50..100)});
        let server = serve(vec![
            Reply::status(200).json(&page1.to_string()),
            Reply::status(200).json(&page2.to_string()),
        ]);
        let chats = backend(&server).with_max_chats(60).chats().unwrap();
        assert_eq!(chats.len(), 60, "trimmed to the configured size");
        let seen = server.seen();
        assert_eq!(seen.len(), 2);
        assert!(
            seen[0].path.contains("$top=50") && seen[1].path.starts_with("/me/chats?page=2"),
            "{:?}",
            seen.iter().map(|s| &s.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_service_that_keeps_sending_empty_pages_and_a_next_link_is_not_followed_forever() {
        let endless = |_: usize| {
            Reply::status(200).json(
                &json!({"value": [], "@odata.nextLink": "{origin}/me/chats?page=again"})
                    .to_string(),
            )
        };
        let server = serve((0..40).map(endless).collect());
        let chats = backend(&server).with_max_chats(50).chats().unwrap();
        assert!(chats.is_empty());
        // 50 chats need one page of 50; a few more are allowed, not forty.
        assert!(server.seen().len() <= 7, "{} requests", server.seen().len());
    }

    #[test]
    fn a_reaction_names_a_message_by_its_numeric_id_only() {
        let server = serve(vec![]);
        let b = backend(&server);
        for id in ["../x", "", "17/../../me", "1 7"] {
            assert!(
                matches!(
                    b.react("19:x@thread.v2", id, "👍", true),
                    Err(Error::InvalidInput { .. })
                ),
                "{id:?}"
            );
        }
        assert!(server.seen().is_empty());
    }
    #[test]
    fn a_small_list_size_is_asked_for_directly_in_one_request() {
        let server = serve(vec![
            Reply::status(200).json(&json!({"value": numbered_chats(0..50)}).to_string()),
        ]);
        let chats = backend(&server).with_max_chats(10).chats().unwrap();
        assert_eq!(chats.len(), 10);
        assert!(
            server.seen()[0].path.contains("$top=10"),
            "{}",
            server.seen()[0].path
        );
        assert_eq!(server.seen().len(), 1);
    }

    #[test]
    fn a_recorder_sees_graph_exchanges_masked() {
        let dir = crate::testdir::TestDir::new("graph-shapes");
        let server = serve(vec![
            Reply::status(200).json(r#"{"id":"u-1234","displayName":"Ada Lovelace"}"#),
        ]);
        let recorder = Arc::new(ShapeRecorder::new(&*dir).unwrap());
        backend(&server).with_recorder(recorder).me().unwrap();
        let text = std::fs::read_to_string(
            std::fs::read_dir(&dir)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(
            text.contains("displayName")
                && !text.contains("Lovelace")
                && !text.contains("test-token"),
            "{text}"
        );
    }

    /// A backend whose sign-in server is `server` too, with its tokens kept in `store`.
    fn backend_with_store(server: &Server, store: Arc<dyn CredentialStore>) -> GraphBackend {
        GraphBackend::new(
            GraphAuth::new(GraphConfig::default()).with_login_base(server.origin()),
            GraphTokens::for_test("test-token"),
            store,
        )
        .with_base(server.origin())
    }

    const RENEWED: &str =
        r#"{"access_token":"renewed-token","refresh_token":"rotated-refresh","expires_in":3600}"#;

    #[test]
    fn a_401_renews_the_token_retries_with_it_and_keeps_the_rotated_one() {
        let server = serve(vec![
            Reply::status(401).json(r#"{"error":{"message":"expired"}}"#),
            Reply::status(200).json(RENEWED),
            Reply::status(200).json(r#"{"value":[]}"#),
        ]);
        let store = Arc::new(MemoryStore::new());
        let b = backend_with_store(&server, store.clone());
        let page = b.messages("19:abc", None).unwrap();
        assert!(page.messages.is_empty());
        let seen = server.seen();
        assert_eq!(
            seen.len(),
            3,
            "{:?}",
            seen.iter().map(|s| &s.path).collect::<Vec<_>>()
        );
        assert_eq!(seen[0].header("authorization"), Some("Bearer test-token"));
        assert_eq!(seen[1].method, "POST");
        assert!(
            seen[1].path.ends_with("/oauth2/v2.0/token"),
            "{}",
            seen[1].path
        );
        assert!(
            seen[1].body.contains("grant_type=refresh_token")
                && seen[1].body.contains("refresh_token=test-refresh"),
            "the stored refresh token was redeemed: {}",
            seen[1].body
        );
        assert_eq!(
            seen[2].header("authorization"),
            Some("Bearer renewed-token")
        );
        let saved = GraphTokens::load(store.as_ref()).unwrap().expect("saved");
        assert_eq!(saved.access_token(), "renewed-token");
        assert_eq!(
            saved.refresh_token(),
            Some("rotated-refresh"),
            "the rotated refresh token is the one kept"
        );
        assert!(b.take_warnings().is_empty());
    }

    /// A store that cannot be written.
    struct FullDisk;

    impl CredentialStore for FullDisk {
        fn load(&self) -> Result<Option<Vec<u8>>, StoreError> {
            Ok(None)
        }
        fn save(&self, _: &[u8]) -> Result<(), StoreError> {
            Err(StoreError::Io {
                action: "write",
                path: std::path::PathBuf::from("graph-token.json"),
                source: std::io::Error::other("disk full"),
            })
        }
        fn clear(&self) -> Result<bool, StoreError> {
            Ok(false)
        }
    }

    #[test]
    fn a_renewed_token_that_cannot_be_saved_still_serves_the_request_and_is_reported_as_a_warning()
    {
        let server = serve(vec![
            Reply::status(401).json(r#"{"error":{"message":"expired"}}"#),
            Reply::status(200).json(RENEWED),
            Reply::status(200).json(r#"{"value":[]}"#),
        ]);
        let b = backend_with_store(&server, Arc::new(FullDisk));
        assert!(
            b.messages("19:abc", None).is_ok(),
            "the request itself went through"
        );
        let warnings = b.take_warnings();
        assert!(
            matches!(warnings.as_slice(), [Error::Store(StoreError::Io { .. })]),
            "{warnings:?}"
        );
        assert!(b.take_warnings().is_empty(), "reported once");
        // And the renewed token is the one in use from now on, saved or not.
        assert_eq!(lock(&b.tokens).access_token(), "renewed-token");
    }

    fn message_json(id: &str, text: &str) -> Value {
        json!({"id": id, "messageType": "message", "createdDateTime": "2026-10-27T10:00:00Z",
            "from": {"user": {"id": "me", "displayName": "Me"}},
            "body": {"contentType": "html", "content": text}})
    }

    #[test]
    fn sending_posts_the_html_and_the_mentions_and_returns_the_stored_message() {
        let server =
            serve(vec![Reply::status(201).json(
                &message_json("1700000000001", "<p>hello</p>").to_string(),
            )]);
        let b = backend(&server);
        let mentions = [OutMention {
            id: 0,
            user_id: "u2".into(),
            name: "Ada".into(),
        }];
        let quoted = ReplyTarget {
            id: "1".into(),
            from_id: None,
            from_name: "Bo".into(),
            preview: "earlier".into(),
        };
        let sent = b
            .send(
                "19:abc@unq.gbl.spaces",
                "<p>hello <at id=\"0\">Ada</at></p>",
                &mentions,
                Some(&quoted),
            )
            .unwrap();
        assert_eq!(sent.id, "1700000000001");
        assert_eq!(sent.from_name, "Me");
        let seen = server.seen();
        assert_eq!(seen[0].method, "POST");
        assert!(
            seen[0]
                .path
                .ends_with("/chats/19%3Aabc@unq.gbl.spaces/messages"),
            "{}",
            seen[0].path
        );
        assert_eq!(seen[0].header("authorization"), Some("Bearer test-token"));
        let body: Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(body["body"]["contentType"], "html");
        assert_eq!(
            body["body"]["content"],
            "<p>hello <at id=\"0\">Ada</at></p>"
        );
        assert_eq!(body["mentions"][0]["id"], 0);
        assert_eq!(body["mentions"][0]["mentionText"], "Ada");
        assert_eq!(body["mentions"][0]["mentioned"]["user"]["id"], "u2");
        assert!(
            !seen[0].body.contains("earlier"),
            "Graph has no reply quote, so the message goes without one rather than being refused"
        );
    }

    #[test]
    fn a_send_the_service_refuses_is_a_typed_refusal_that_was_not_delivered() {
        let server = serve(vec![
            Reply::status(403).json(r#"{"error":{"message":"not a member"}}"#),
        ]);
        let err = backend(&server)
            .send("19:abc", "<p>x</p>", &[], None)
            .unwrap_err();
        assert!(matches!(err, Error::Status { status: 403, .. }), "{err:?}");
        assert!(
            !err.delivery_unknown(),
            "a refusal means it was not delivered"
        );
    }

    #[test]
    fn messages_come_back_oldest_first_with_the_link_to_the_next_page_and_that_link_is_followed() {
        let server = serve(vec![
            Reply::status(200).json(
                &json!({"value": [message_json("2", "<p>b</p>"), {
                    "id": "1", "messageType": "message", "createdDateTime": "2026-10-27T09:00:00Z",
                    "from": {"user": {"id": "u1", "displayName": "Ada"}},
                    "body": {"contentType": "html", "content": "<p>a</p>"}}],
                    "@odata.nextLink": "{origin}/chats/19%3Aabc/messages?$skiptoken=page2"})
                .to_string(),
            ),
            Reply::status(200)
                .json(&json!({"value": [message_json("0", "<p>older</p>")]}).to_string()),
        ]);
        let b = backend(&server);
        let first = b.messages("19:abc", None).unwrap();
        let ids: Vec<&str> = first.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["1", "2"], "sorted by when they were created");
        let next = first.older.expect("a link to the older page");
        let second = b.messages("19:abc", Some(&next)).unwrap();
        assert_eq!(second.messages.len(), 1);
        assert!(second.older.is_none());
        let seen = server.seen();
        assert!(
            seen[0].path.contains("/chats/19%3Aabc/messages?$top=50"),
            "{}",
            seen[0].path
        );
        assert!(
            seen[1].path.contains("$skiptoken=page2"),
            "{}",
            seen[1].path
        );
    }
}
