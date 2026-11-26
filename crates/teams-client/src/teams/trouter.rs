//! Trouter, Teams' push-notification service: a long-lived WebSocket that other services push events to, so the
//! app can notice a new message, reaction or edit sooner than its own poll interval. The lifecycle (register →
//! socket.io v0.9 handshake → WebSocket dial → authenticate → register four app transports → ping every 30 s →
//! reconnect on any failure) was verified live against a real tenant with a throwaway probe before this was
//! written; see `docs/teams-web-protocol.md`. Behaviour follows `YourSandwich/mautrix-teams`'s
//! `pkg/msteams/trouter.go` (AGPL; read for behaviour only, nothing of its code is here).
//!
//! This is purely a hint mechanism: a connection failure here never surfaces as an error to a caller, and nothing
//! here duplicates the existing poll-and-merge path. [`TeamsBackend::connect_push`] starts one background thread
//! holding only a [`Weak`] reference to the backend, so it cannot keep it alive; the thread notices the backend is
//! gone (or never comes up at all while signed out) and simply stops mattering, the same way a straggler thread is
//! already accepted at quit time (see `AGENTS.md`, "Background work"). Token renewal goes through
//! [`TeamsBackend::live`], the exact same mutex-guarded path every other request already shares.

use super::{Live, TeamsBackend};
use crate::error::{Error, Result};
use crate::http::{self, Call};
use crate::lock;
use crate::model::PushHint;
use crate::record::ShapeRecorder;
use crate::watch::Watch;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, StreamOwned};
use rustls_platform_verifier::BuilderVerifierExt;
use serde_json::{Value, json};
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

/// Milliseconds since the epoch, for the `con_num` query field (a wall-clock-ish counter the service does not
/// seem to validate strictly; it only needs to look plausible and change between the handshake and the dial).
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// The one fixed entry point; everything else (the regional host, the per-connection session path, the registrar)
/// comes back in its answer. Kept on [`TeamsBackend`] as `trouter_host`, not used directly, so tests can replace
/// it with a local server.
pub(super) const TROUTER_HOST: &str = "https://go.trouter.teams.microsoft.com/";
/// Used only if the registration answer did not name one (seen live as `communications.svc.cloud.microsoft`'s own
/// path; this is the older host an earlier reference implementation used).
const REGISTRAR_FALLBACK: &str = "https://teams.microsoft.com/registrar/prod/V2/registrations";
/// A fixed tracing blob the service expects in every query string; an empty `cv` was refused with a bare 400,
/// confirmed live, so these two values are worth getting exactly right rather than guessed at again.
const TC_CV: &str = "2024.23.01.2";
const TC_V: &str = "27/24070119616";
/// How often to ping an idle connection.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// How long one `read()` blocks before giving the loop a chance to notice the backend is gone or that a ping is
/// due. Short enough that neither takes long to notice; long enough not to busy-loop.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Reconnect backoff: doubles each consecutive failure, capped around two minutes.
const RECONNECT_BASE: Duration = Duration::from_secs(2);
/// A connection that stayed up this long was working: when it ends, the next attempt starts from the shortest wait
/// again instead of continuing the count of failures from before it.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);
/// A connection that has received nothing at all (not even the service's own heartbeat) for this long is taken for
/// dead and replaced. A socket whose other end vanished without closing it reads as quiet forever, and the pings
/// written to it are accepted by the operating system for a long time.
const SILENCE_LIMIT: Duration = Duration::from_secs(300);
/// How long the whole TCP connection to the push host may take, across every address its name resolves to.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one write may block. Shorter than [`PING_INTERVAL`], so a peer that stops reading is noticed before the
/// next ping is due rather than holding the loop for good.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait between checks when the sign-in is lost, before attempting to connect.
const SIGN_IN_CHECK_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(5)
};
/// The largest WebSocket message, and the largest single frame, that is accepted: far more than any push the service
/// has been seen to send, and far less than the library's own defaults (64 MiB and 16 MiB), which a server could fill
/// to make this process hold that much memory.
const MAX_SOCKET_BYTES: usize = 1024 * 1024;
/// The most hints kept waiting for the app to ask for them. A hint only says "poll that chat soon", so more than this
/// say nothing the first few did not; the oldest are dropped, the newest kept.
const MAX_QUEUED_HINTS: usize = 256;
/// The longest id in a request frame that is echoed back in an acknowledgement (a `u64` has 20 digits).
const MAX_ACK_ID_DIGITS: usize = 20;

type Socket = WebSocket<StreamOwned<ClientConnection, TcpStream>>;

/// Add `hint` to the queue the app drains: an identical one already waiting is not added again, and when the queue is
/// full the oldest is dropped to make room.
pub(super) fn queue_hint(queue: &mut Vec<PushHint>, hint: PushHint) {
    if queue.contains(&hint) {
        return;
    }
    if queue.len() >= MAX_QUEUED_HINTS {
        let excess = queue.len() + 1 - MAX_QUEUED_HINTS;
        queue.drain(..excess);
    }
    queue.push(hint);
}

/// The WebSocket limits for the push connection (see [`MAX_SOCKET_BYTES`]).
fn socket_config() -> tungstenite::protocol::WebSocketConfig {
    tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_SOCKET_BYTES))
        .max_frame_size(Some(MAX_SOCKET_BYTES))
}

/// A TCP connection to `host:port` that gives up after `timeout` in all (not for each address the name resolves to),
/// with the read and write timeouts the connection loop relies on already set.
fn connect_tcp(host: &str, port: u16, timeout: Duration) -> std::io::Result<TcpStream> {
    use std::net::ToSocketAddrs;
    let tcp = connect_first((host, port).to_socket_addrs()?, timeout, |addr, left| {
        TcpStream::connect_timeout(addr, left)
    })?;
    tcp.set_read_timeout(Some(READ_TIMEOUT))?;
    tcp.set_write_timeout(Some(WRITE_TIMEOUT))?;
    Ok(tcp)
}

/// The first of `addrs` that `connect` reaches, where each attempt is given only what is left of `timeout`, so the
/// whole takes about `timeout` however many addresses there are.
fn connect_first<T>(
    addrs: impl IntoIterator<Item = std::net::SocketAddr>,
    timeout: Duration,
    mut connect: impl FnMut(&std::net::SocketAddr, Duration) -> std::io::Result<T>,
) -> std::io::Result<T> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    for addr in addrs {
        let left = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        match connect(&addr, left) {
            Ok(connected) => return Ok(connected),
            Err(e) => last = Some(e),
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    Err(last.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::TimedOut, "no address answered in time")
    }))
}

/// The WebSocket handshake over `stream`, with this connection's size limits.
fn open_socket<S: std::io::Read + std::io::Write + Send + Sync + 'static>(
    req: tungstenite::http::Request<()>,
    stream: S,
) -> Result<WebSocket<S>> {
    tungstenite::client::client_with_config(req, stream, Some(socket_config()))
        .map(|(ws, _)| ws)
        .map_err(|e| Error::transport("dial the push channel", e))
}

/// Whether `url` is one of Trouter's own hosts: `trouter_host` itself (the fixed entry point, or in a test the
/// loopback server standing in for it), or a `pub-ent-<region>-<n>-{t,f}` regional host the registration answer
/// named: the only hosts the skypetoken goes to for this connection.
fn is_trouter_url(url: &str, trouter_host: &str) -> bool {
    url.starts_with(trouter_host)
        || url::Url::parse(url).is_ok_and(|u| {
            u.scheme() == "https"
                && u.host_str()
                    .is_some_and(|h| h.ends_with(".trouter.teams.microsoft.com"))
        })
}

/// Whether `url` is a registrar host actually seen or documented for this flow (or, in a test, the loopback
/// server standing in for every host in this connection); the skypetoken and the spaces token both go there over
/// https only, so a value merely *named inside* the registration answer is checked against this small, explicit
/// list of https hosts rather than trusted outright (the same caution `Error::UntrustedUrl` already applies to
/// paging links and `authz`-named hosts elsewhere in this backend).
fn is_registrar_url(url: &str, trouter_host: &str) -> bool {
    url.starts_with(trouter_host)
        || url::Url::parse(url).is_ok_and(|u| {
            u.scheme() == "https"
                && u.host_str().is_some_and(|h| {
                    h == "teams.microsoft.com" || h == "communications.svc.cloud.microsoft"
                })
        })
}

/// A client-chosen identifier for this connection; not cryptographically random, which does not matter here (it
/// names nothing secret, only this one connection's own endpoint).
fn endpoint_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    (std::process::id(), thread::current().id(), Instant::now()).hash(&mut h);
    let a = h.finish();
    let mut h2 = DefaultHasher::new();
    (a, "trouter-endpoint").hash(&mut h2);
    let b = h2.finish();
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (a >> 32) as u32,
        (a >> 16) as u16,
        a as u16 & 0x0fff,
        (b >> 48) as u16 & 0x3fff | 0x8000,
        b & 0xffff_ffff_ffff
    )
}

/// What the registration answer gave us to work with.
pub(super) struct Registration {
    /// Bare host for the handshake and the WebSocket dial.
    socketio: String,
    /// This connection's own `v4/f/<sr>/` path, used to build the registrar's transport path (not for the
    /// client's own outbound connection, which uses `socketio` instead).
    surl: String,
    registrar_url: String,
    connect_params: Value,
    ccid: String,
}

pub(super) fn register(
    agent: &ureq::Agent,
    recorder: Option<&ShapeRecorder>,
    trouter_host: &str,
    endpoint: &str,
    skype: &str,
) -> Result<Registration> {
    let url = format!("{trouter_host}v4/a?epid={}", http::urlencode(endpoint));
    let call = Call {
        agent,
        recorder,
        operation: "register with the push channel",
        method: "POST",
        url: &url,
        body: None,
    };
    let info = http::call_json(&call, &mut |_| {
        Ok(vec![("X-Skypetoken", skype.to_string())])
    })?;
    let socketio = info["socketio"]
        .as_str()
        .filter(|u| is_trouter_url(u, trouter_host))
        .ok_or(Error::Unexpected {
            operation: "register with the push channel",
            detail: "no usable socketio host in the answer",
        })?
        .to_string();
    let surl = info["surl"].as_str().unwrap_or("").to_string();
    if surl.is_empty() {
        return Err(Error::Unexpected {
            operation: "register with the push channel",
            detail: "no connection path in the answer",
        });
    }
    let registrar_url = info["registrarUrl"]
        .as_str()
        .filter(|u| is_registrar_url(u, trouter_host))
        .unwrap_or(REGISTRAR_FALLBACK)
        .to_string();
    Ok(Registration {
        socketio,
        surl,
        registrar_url,
        connect_params: info["connectparams"].clone(),
        ccid: info["ccid"].as_str().unwrap_or("").to_string(),
    })
}

/// The query string every one of this connection's socket.io requests carries: `connectparams` copied in verbatim
/// (short-lived signed fields the registration answer handed back), a fixed tracing blob, and a counter.
fn common_query(reg: &Registration, endpoint: &str, con_num: u64) -> String {
    let tc = format!(r#"{{"cv":"{TC_CV}","ua":"TeamsCDL","hr":"","v":"{TC_V}"}}"#);
    let mut q = vec![
        ("v".to_string(), "v4".to_string()),
        ("tc".to_string(), tc),
        ("con_num".to_string(), format!("{}_{con_num}", now_ms())),
        ("epid".to_string(), endpoint.to_string()),
        ("auth".to_string(), "true".to_string()),
        ("timeout".to_string(), "40".to_string()),
    ];
    if let Some(obj) = reg.connect_params.as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                q.push((k.clone(), s.to_string()));
            }
        }
    }
    if !reg.ccid.is_empty() {
        q.push(("ccid".to_string(), reg.ccid.clone()));
    }
    q.into_iter()
        .map(|(k, v)| format!("{}={}", http::urlencode(&k), http::urlencode(&v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `GET {socketio}/socket.io/1/?...`; the body is socket.io's own `<sid>:<heartbeat>:<close>:<transports>`, not
/// JSON, so this does not go through `call_json`.
pub(super) fn handshake(
    agent: &ureq::Agent,
    reg: &Registration,
    endpoint: &str,
    skype: &str,
) -> Result<String> {
    let url = format!(
        "{}/socket.io/1/?{}",
        reg.socketio.trim_end_matches('/'),
        common_query(reg, endpoint, 1)
    );
    let mut resp = agent
        .get(&url)
        .header("X-Skypetoken", skype)
        .call()
        .map_err(|e| Error::transport("push channel handshake", e))?;
    let text = http::check("push channel handshake", &mut resp)?;
    let sid = text.split(':').next().unwrap_or("").to_string();
    if sid.is_empty() {
        return Err(Error::Unexpected {
            operation: "push channel handshake",
            detail: "no session id in the answer",
        });
    }
    Ok(sid)
}

fn dial(reg: &Registration, endpoint: &str, sid: &str, skype: &str) -> Result<Socket> {
    let ws_url = format!(
        "{}/socket.io/1/websocket/{sid}?{}",
        reg.socketio
            .trim_end_matches('/')
            .replacen("https://", "wss://", 1),
        common_query(reg, endpoint, 2)
    );
    let u = url::Url::parse(&ws_url).map_err(|e| Error::transport("dial the push channel", e))?;
    let host = u
        .host_str()
        .ok_or(Error::Unexpected {
            operation: "dial the push channel",
            detail: "no host in the dial URL",
        })?
        .to_string();
    let port = u.port_or_known_default().unwrap_or(443);
    let tcp = connect_tcp(&host, port, CONNECT_TIMEOUT)
        .map_err(|e| Error::transport("dial the push channel", e))?;
    // The OS's own certificate validator decides trust (Windows chain validation, including the Disallowed
    // Certificates store; macOS `Security.framework`), the same as `http::agent`'s `RootCerts::PlatformVerifier`
    // uses for every other request — one TLS stack and one trust policy for the whole backend.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls_config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::transport("dial the push channel", e))?
        .with_platform_verifier()
        .map_err(|e| Error::transport("dial the push channel", e))?
        .with_no_client_auth();
    let server_name = ServerName::try_from(host.clone())
        .map_err(|e| Error::transport("dial the push channel", e))?;
    let conn = ClientConnection::new(Arc::new(tls_config), server_name)
        .map_err(|e| Error::transport("dial the push channel", e))?;
    let tls = StreamOwned::new(conn, tcp);
    let mut req = ws_url
        .into_client_request()
        .map_err(|e| Error::transport("dial the push channel", e))?;
    let value = skype
        .parse()
        .map_err(|e| Error::transport("dial the push channel", e))?;
    req.headers_mut().insert("X-Skypetoken", value);
    open_socket(req, tls)
}

/// Sent once, on the socket.io `1` (connect) frame.
fn auth_frame(live: &Live) -> String {
    format!(
        r#"5:::{{"name":"user.authenticate","args":[{{"headers":{{"Authorization":"Bearer {}"}}}}]}}"#,
        live.spaces_token
    )
}
const ACTIVITY_FRAME: &str = r#"5:::{"name":"user.activity","args":[{"state":"active"}]}"#;

/// Register this connection as reachable by each of the four apps the real Teams web client registers (seen
/// live). Best effort: a failure here is logged and does not end the connection — the socket itself still works
/// for receiving pushes either way; it only means some other service's own, separate attempt to reach this
/// endpoint by name might not find it.
fn register_transports(
    agent: &ureq::Agent,
    recorder: Option<&ShapeRecorder>,
    reg: &Registration,
    endpoint: &str,
    skype: &str,
    spaces: &str,
) {
    for (app_id, template, path) in [
        (
            "NextGenCalling",
            "DesktopNgc_2.3:SkypeNgc",
            "NGCallManagerWin",
        ),
        ("SkypeSpacesWeb", "SkypeSpacesWeb_2.3", "SkypeSpacesWeb"),
        (
            "SkypeSpacesCallAgent",
            "SkypeSpacesCallAgent_2.3",
            "callAgent",
        ),
        ("TeamsCDLWebWorker", "TeamsCDLWebWorker_2.1", ""),
    ] {
        let full_path = if path.is_empty() {
            reg.surl.clone()
        } else {
            format!("{}{path}", reg.surl)
        };
        let reg_id = if path.is_empty() {
            endpoint.to_string()
        } else {
            endpoint_id()
        };
        let body = json!({
            "clientDescription": {
                "appId": app_id, "aesKey": "", "languageId": "en-US",
                "platform": "edge", "templateKey": template,
                "platformUIVersion": super::DEFAULT_CLIENT_VERSION,
            },
            "registrationId": reg_id, "nodeId": "",
            "transports": { "TROUTER": [{ "context": "", "path": full_path, "ttl": 86400 }] },
        });
        let call = Call {
            agent,
            recorder,
            operation: "register a push transport",
            method: "POST",
            url: &reg.registrar_url,
            body: Some(&body),
        };
        let skype = skype.to_string();
        let spaces = spaces.to_string();
        let result = http::call_json(&call, &mut |_| {
            Ok(vec![
                ("X-Skypetoken", skype.clone()),
                ("Authorization", format!("Bearer {spaces}")),
            ])
        });
        if let Err(e) = result {
            log::debug!(
                "registering {app_id} with the push channel: {}",
                e.for_log()
            );
        }
    }
}

/// What came out of handling one frame that is not the connect (`1`) frame: a reply to send right back (an ack or
/// a heartbeat), and/or a hint to queue. Pure, so it is tested with plain strings and no socket.
fn handle_frame(frame: &str, watch: &Watch) -> (Option<String>, Option<PushHint>) {
    let Some(kind) = frame.chars().next() else {
        return (None, None);
    };
    match kind {
        '2' => (Some("2::".to_string()), None),
        '3' => {
            let after = frame.strip_prefix("3:").unwrap_or("");
            // The id is echoed back inside JSON, so only what a number looks like is: anything else is not
            // acknowledged at all (the hint, if any, is still handled).
            let reply = after
                .split(':')
                .next()
                .filter(|id| {
                    !id.is_empty()
                        && id.len() <= MAX_ACK_ID_DIGITS
                        && id.bytes().all(|b| b.is_ascii_digit())
                })
                .map(|id| format!("3:::{{\"id\":{id},\"status\":200,\"body\":\"\"}}"));
            let body: Value = frame
                .splitn(4, ':')
                .nth(3)
                .and_then(|rest| serde_json::from_str(rest).ok())
                .unwrap_or(Value::Null);
            let url = body["url"].as_str().unwrap_or("");
            let hint = if url.ends_with("/messaging") {
                Some(extract_chat_id(&body, watch))
            } else {
                // Not yet recognised (a typing indicator is believed to arrive this way too, among others, but
                // its shape is unconfirmed): note the request's own "kind" (the last path segment, e.g.
                // "messaging" -- never the full path, which carries this connection's own session id) as a small
                // closed set, the same discipline `Watch::tokens` already applies to a message type, and the
                // envelope's field names, so a future live run teaches us what else this channel carries.
                if !url.is_empty() {
                    watch.tokens("push request kind", [trailing_id(url)]);
                }
                watch.fields("push request", [&body]);
                None
            };
            (reply, hint)
        }
        '5' => {
            let id = frame
                .strip_prefix("5:")
                .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
                .filter(|d| !d.is_empty());
            let reply = id.map(|id| format!("6:{id}::"));
            let name = frame
                .splitn(4, ':')
                .nth(3)
                .and_then(|rest| serde_json::from_str::<Value>(rest).ok())
                .and_then(|v| v["name"].as_str().map(str::to_string));
            if let Some(name) = &name {
                // A small closed set of event names (seen so far: "trouter.connected", "trouter.message_loss"),
                // the same discipline `Watch::tokens` already applies to a message type.
                watch.tokens("push event name", [name.as_str()]);
            }
            // Named explicitly: the service is telling us there is a gap it cannot resolve over the socket, which
            // is exactly why the existing poll-based cadence stays authoritative and this is only ever a hint.
            let hint = name.map(|_| PushHint::Unknown);
            (reply, hint)
        }
        _ => (None, None),
    }
}

/// The last path segment of `s`, with any query string dropped: `conversationLink` was seen live as a full URL
/// (`.../conversations/<id>`), while `conversationid` was a bare id with nothing to strip; applying this to both
/// uniformly is harmless for the one that needs no trimming. A reply in a channel is believed to be named
/// `<channel id>;messageid=<post id>` (not verified: `docs/teams-web-protocol.md`, "Teams and channels"), written with
/// the semicolon or as `%3B`; the suffix is cut, so the hint names the channel, which is what is polled.
fn trailing_id(s: &str) -> &str {
    let last = s
        .rsplit('/')
        .next()
        .unwrap_or(s)
        .split('?')
        .next()
        .unwrap_or(s);
    let cut = last
        .find(';')
        .into_iter()
        .chain(last.to_ascii_lowercase().find("%3b"))
        .min()
        .unwrap_or(last.len());
    &last[..cut]
}

/// A chat id from a `messaging` push's envelope. **Confirmed live** (2026-11-06, `vtteams.log`'s `Watch` output):
/// the outer frame is itself a small REST-request envelope, `{"id":..,"method":..,"url":..,"headers":{..},
/// "body":"<json string>"}`, not an object with the change already in it — `body` is a *string* and must be
/// decoded a second time before anything inside it is visible. That decoded body is itself a Microsoft-style
/// change-notification envelope, **also confirmed live**: `resourceType`/`resourceLink`/`resource`/`type`/`time`,
/// plus `isactive`/`mentiontypes`; `resource` is a nested object, one level deeper than the decoded body itself,
/// and is itself an ordinary message-shaped object (the same family as `teams::parse::parse_msg` already reads).
/// **Confirmed live, a third time**: a reaction's `resource` carried `conversationLink` but not `conversationid`
/// (the two are not equally reliable, hence trying both); extracting the trailing id out of it correctly named
/// the chat rather than falling back to `PushHint::Unknown`. An unrecognised shape still broadens to `Unknown`
/// rather than being dropped, and every level inspected here is noted through `watch` (field names only).
fn extract_chat_id(envelope: &Value, watch: &Watch) -> PushHint {
    let decoded: Value = envelope["body"]
        .as_str()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null);
    if !decoded.is_null() {
        watch.fields("push message body", [&decoded]);
        // `type`/`resourceType` are a small, closed set of protocol-level labels (seen so far: "EventMessage",
        // "NewMessage"), the same discipline `Watch::tokens` already applies to a message type -- this is the
        // one signal that would tell a typing indicator or a roster change apart from an ordinary message or
        // reaction, which `push message body`'s field *names* alone cannot (the field is there either way).
        for field in ["type", "resourceType"] {
            if let Some(v) = decoded[field].as_str() {
                watch.tokens("push message type", [v]);
            }
        }
    }
    let resource = &decoded["resource"];
    if !resource.is_null() {
        watch.fields("push message resource", [resource]);
        if let Some(v) = resource["type"].as_str() {
            watch.tokens("push message resource type", [v]);
        }
    }
    for path in [
        &resource["conversationid"],
        &resource["conversationLink"],
        &decoded["conversationid"],
        &decoded["conversationLink"],
    ] {
        if let Some(id) = path.as_str().filter(|s| !s.is_empty()) {
            return PushHint::Chat(trailing_id(id).to_string());
        }
    }
    watch.fields("push message", [envelope]);
    PushHint::Unknown
}

fn send(ws: &mut Socket, text: &str) -> Result<()> {
    ws.send(Message::text(text))
        .map_err(|e| Error::transport("push channel", e))
}

/// Connect once and pump frames until the connection fails or `weak` can no longer be upgraded (the backend was
/// dropped, meaning a clean, deliberate stop rather than a failure to reconnect from).
fn connect_and_pump(weak: &Weak<TeamsBackend>) -> Result<()> {
    let Some(backend) = weak.upgrade() else {
        return Ok(());
    };
    let live = backend.live(None)?;
    let endpoint = endpoint_id();
    let reg = register(
        &backend.api,
        backend.recorder.as_deref(),
        &backend.trouter_host,
        &endpoint,
        &live.skype_token,
    )?;
    let sid = handshake(&backend.api, &reg, &endpoint, &live.skype_token)?;
    let mut ws = dial(&reg, &endpoint, &sid, &live.skype_token)?;
    drop(backend);
    let mut registered_transports = false;
    let mut last_ping = Instant::now();
    let mut last_heard = Instant::now();
    loop {
        if weak.upgrade().is_none() {
            return Ok(());
        }
        if last_heard.elapsed() > SILENCE_LIMIT {
            return Err(Error::Unexpected {
                operation: "push channel",
                detail: "the service has been silent too long; reconnecting",
            });
        }
        if last_ping.elapsed() > PING_INTERVAL {
            send(&mut ws, r#"5:::{"name":"ping"}"#)?;
            last_ping = Instant::now();
        }
        let msg = match ws.read() {
            Ok(m) => m,
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(Error::transport("push channel", e)),
        };
        last_heard = Instant::now();
        let Message::Text(frame) = msg else { continue };
        let frame = frame.as_str();
        if frame.starts_with('1') {
            let Some(backend) = weak.upgrade() else {
                return Ok(());
            };
            send(&mut ws, &auth_frame(&live))?;
            send(&mut ws, ACTIVITY_FRAME)?;
            if !registered_transports {
                registered_transports = true;
                register_transports(
                    &backend.api,
                    backend.recorder.as_deref(),
                    &reg,
                    &endpoint,
                    &live.skype_token,
                    &live.spaces_token,
                );
            }
            continue;
        }
        let Some(backend) = weak.upgrade() else {
            return Ok(());
        };
        let (reply, hint) = handle_frame(frame, &backend.watch);
        if let Some(r) = reply {
            send(&mut ws, &r)?;
        }
        if let Some(h) = hint {
            queue_hint(&mut lock(&backend.push_queue), h);
        }
    }
}

/// How long to wait before the next reconnect attempt: doubles each consecutive failure, capped at 64x the base
/// (a little over two minutes).
fn reconnect_delay(fails: u32) -> Duration {
    RECONNECT_BASE.saturating_mul(1u32 << fails.min(6))
}

/// The count of consecutive failures after a connection attempt that ended after `up_for`: a connection that was
/// healthy for a while ends the run of failures (this one counts as the first), a quick failure extends it.
fn failures_after(fails: u32, up_for: Duration) -> u32 {
    if up_for >= HEALTHY_AFTER {
        1
    } else {
        fails.saturating_add(1)
    }
}

/// Whether the push thread should wait instead of connecting: while the sign-in is lost, do not attempt to connect
/// (which would trigger a renewal).
fn should_wait_for_sign_in(weak: &Weak<TeamsBackend>) -> bool {
    weak.upgrade()
        .is_some_and(|backend| backend.auth.sign_in_lost())
}

/// Runs until `weak` can no longer be upgraded, reconnecting with backoff on any failure. Spawned once by
/// [`TeamsBackend::connect_push`]; never touches `App` or sign-in state directly, only `TeamsBackend::live`, the
/// same renewal path every other request already shares.
fn run(weak: Weak<TeamsBackend>) {
    let mut fails: u32 = 0;
    while weak.upgrade().is_some() {
        // While the sign-in is lost, do not attempt to connect (which would trigger a renewal); instead wait and
        // check again. When sign_in_again succeeds and calls adopt, sign_in_lost() becomes false and the push
        // thread reconnects automatically.
        if should_wait_for_sign_in(&weak) {
            thread::sleep(SIGN_IN_CHECK_INTERVAL);
            continue;
        }
        let started = Instant::now();
        match connect_and_pump(&weak) {
            Ok(()) => return,
            Err(e) => {
                log::debug!("push channel connection ended: {}", e.for_log());
                fails = failures_after(fails, started.elapsed());
            }
        }
        if weak.upgrade().is_none() {
            return;
        }
        thread::sleep(reconnect_delay(fails));
    }
}

impl TeamsBackend {
    /// Start the push connection in the background, if [`with_push`](Self::with_push) (the default) and this has
    /// not already been called. Never blocks and never fails visibly: while signed out, or if a tenant refuses
    /// this entirely, `TeamsBackend::live` simply keeps failing and the thread backs off and keeps retrying; the
    /// existing poll-based cadence works regardless.
    pub fn connect_push(self: &Arc<Self>) {
        if !self.push_enabled {
            return;
        }
        if self.push_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        if thread::Builder::new()
            .name("teams-trouter".to_string())
            .spawn(move || run(weak))
            .is_err()
        {
            log::warn!("could not start the push channel thread; staying poll-based");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::Watch;

    #[test]
    fn a_connection_that_worked_for_a_while_starts_the_back_off_over() {
        let quick = Duration::from_secs(3);
        let long = HEALTHY_AFTER + Duration::from_secs(1);
        // Connections that fail at once pile up delays...
        let mut fails = 0;
        for _ in 0..8 {
            fails = failures_after(fails, quick);
        }
        assert_eq!(reconnect_delay(fails), RECONNECT_BASE * 64);
        // ...but one that was up for a while, ending after hours of use, waits the shortest time before the retry.
        assert_eq!(failures_after(fails, long), 1);
        assert_eq!(
            reconnect_delay(failures_after(fails, long)),
            RECONNECT_BASE * 2
        );
        // The wait is still bounded when failures never stop.
        assert_eq!(reconnect_delay(u32::MAX), RECONNECT_BASE * 64);
    }

    #[test]
    fn the_silence_limit_is_longer_than_a_ping_so_a_quiet_but_healthy_connection_is_kept() {
        assert!(SILENCE_LIMIT >= PING_INTERVAL * 4);
        assert!(HEALTHY_AFTER > READ_TIMEOUT);
    }

    #[test]
    fn a_heartbeat_is_echoed_with_no_hint() {
        let watch = Watch::default();
        let (reply, hint) = handle_frame("2::", &watch);
        assert_eq!(reply, Some("2::".to_string()));
        assert_eq!(hint, None);
    }

    /// The id is put into a JSON reply as it is, so it has to look like a number or nothing is sent back.
    #[test]
    fn a_request_id_that_is_not_digits_is_never_echoed() {
        let watch = Watch::default();
        let long = "9".repeat(MAX_ACK_ID_DIGITS + 1);
        let longest = "9".repeat(MAX_ACK_ID_DIGITS);
        for bad in [
            "",
            "1,\"status\":500",
            "1}{",
            "-1",
            "1.5",
            "x",
            "\u{661}",
            long.as_str(),
        ] {
            let frame = format!("3:{bad}::{{\"url\":\"/v4/f/x/other\"}}");
            let (reply, hint) = handle_frame(&frame, &watch);
            assert_eq!(reply, None, "{bad:?}");
            assert_eq!(hint, None);
        }
        let (reply, _) = handle_frame(&format!("3:{longest}::{{}}"), &watch);
        assert_eq!(
            reply,
            Some(format!(
                "3:::{{\"id\":{longest},\"status\":200,\"body\":\"\"}}"
            ))
        );
        // A bad id still lets the hint through: only the acknowledgement is dropped.
        let frame = messaging_frame(9, &json!({"conversationid": "19:abc@thread.v2"}))
            .replacen("3:9::", "3:9x::", 1);
        let (reply, hint) = handle_frame(&frame, &watch);
        assert_eq!(reply, None);
        assert_eq!(hint, Some(PushHint::Chat("19:abc@thread.v2".into())));
    }

    #[test]
    fn the_hint_queue_is_bounded_keeps_the_newest_and_collapses_repeats() {
        let mut queue = Vec::new();
        for _ in 0..1000 {
            queue_hint(&mut queue, PushHint::Unknown);
            queue_hint(&mut queue, PushHint::Chat("19:same@thread.v2".into()));
        }
        assert_eq!(queue.len(), 2, "identical hints are one: {queue:?}");
        let mut queue = Vec::new();
        for i in 0..(MAX_QUEUED_HINTS * 3) {
            queue_hint(&mut queue, PushHint::Chat(format!("19:{i}@thread.v2")));
        }
        assert_eq!(queue.len(), MAX_QUEUED_HINTS);
        let last = MAX_QUEUED_HINTS * 3 - 1;
        assert_eq!(
            queue.last(),
            Some(&PushHint::Chat(format!("19:{last}@thread.v2"))),
            "the newest is kept"
        );
        assert_eq!(
            queue.first(),
            Some(&PushHint::Chat(format!(
                "19:{}@thread.v2",
                last + 1 - MAX_QUEUED_HINTS
            ))),
            "the oldest are the ones dropped"
        );
    }

    /// A server that sends a frame past the limit makes the read fail, instead of this process holding it.
    #[test]
    fn an_oversized_frame_is_refused_by_the_socket() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let _ = ws.send(Message::text("5".repeat(MAX_SOCKET_BYTES + 1)));
            let _ = ws.send(Message::text("ok"));
            // Keep the connection open until the client has finished reading.
            thread::sleep(Duration::from_millis(500));
        });
        let tcp = connect_tcp("127.0.0.1", addr.port(), CONNECT_TIMEOUT).unwrap();
        let req = format!("ws://127.0.0.1:{}/socket", addr.port())
            .into_client_request()
            .unwrap();
        let mut ws = open_socket(req, tcp).unwrap();
        let err = loop {
            match ws.read() {
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                other => break other,
            }
        };
        assert!(
            matches!(err, Err(tungstenite::Error::Capacity(_))),
            "{err:?}"
        );
        // The server may still be writing the rest of the frame; closing this end lets it stop.
        drop(ws);
        server.join().unwrap();
    }

    #[test]
    fn a_message_within_the_limit_still_arrives() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            ws.send(Message::text("5".repeat(MAX_SOCKET_BYTES / 2)))
                .unwrap();
            thread::sleep(Duration::from_millis(300));
        });
        let tcp = connect_tcp("127.0.0.1", addr.port(), CONNECT_TIMEOUT).unwrap();
        let req = format!("ws://127.0.0.1:{}/socket", addr.port())
            .into_client_request()
            .unwrap();
        let mut ws = open_socket(req, tcp).unwrap();
        let got = loop {
            match ws.read() {
                Err(tungstenite::Error::Io(_)) => continue,
                other => break other.unwrap(),
            }
        };
        assert_eq!(got.len(), MAX_SOCKET_BYTES / 2);
        server.join().unwrap();
    }

    #[test]
    fn the_tcp_connection_has_a_deadline_and_the_timeouts_the_loop_relies_on() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let tcp = connect_tcp("127.0.0.1", port, CONNECT_TIMEOUT).unwrap();
        assert_eq!(tcp.read_timeout().unwrap(), Some(READ_TIMEOUT));
        assert_eq!(tcp.write_timeout().unwrap(), Some(WRITE_TIMEOUT));
        assert!(WRITE_TIMEOUT < PING_INTERVAL && CONNECT_TIMEOUT < SILENCE_LIMIT);
        // A port nothing listens on is an error, not a hang.
        drop(listener);
        assert!(connect_tcp("127.0.0.1", port, Duration::from_secs(5)).is_err());
    }

    /// An address that never answers (the attempt runs out its allowance) must not leave the next one a fresh full
    /// allowance each: the whole is bounded by the one timeout.
    #[test]
    fn the_connect_deadline_is_for_all_addresses_together() {
        let addrs: Vec<std::net::SocketAddr> = (1..=5)
            .map(|i| format!("192.0.2.{i}:443").parse().unwrap())
            .collect();
        let mut allowances = Vec::new();
        let started = Instant::now();
        let result: std::io::Result<()> =
            connect_first(addrs, Duration::from_millis(200), |_, left| {
                allowances.push(left);
                // Each address uses some of its allowance before it fails.
                thread::sleep(left.min(Duration::from_millis(80)));
                Err(std::io::ErrorKind::TimedOut.into())
            });
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "{:?}",
            started.elapsed()
        );
        assert!(allowances[0] <= Duration::from_millis(200));
        assert!(
            allowances
                .iter()
                .skip(1)
                .all(|a| *a < Duration::from_millis(200)),
            "{allowances:?}"
        );
        // The first address that answers ends it, and none is tried when there is no address at all.
        let mut tried = 0;
        assert_eq!(
            connect_first(
                [
                    "192.0.2.1:1".parse().unwrap(),
                    "192.0.2.2:1".parse().unwrap()
                ],
                Duration::from_secs(1),
                |addr, _| {
                    tried += 1;
                    if addr.ip().to_string().ends_with(".2") {
                        Ok(7)
                    } else {
                        Err(std::io::ErrorKind::ConnectionRefused.into())
                    }
                }
            )
            .unwrap(),
            7
        );
        assert_eq!(tried, 2);
        assert!(connect_first::<()>(Vec::new(), Duration::from_secs(1), |_, _| Ok(())).is_err());
    }

    #[test]
    fn a_request_frame_is_always_acked_whether_or_not_its_body_is_understood() {
        let watch = Watch::default();
        let frame = r#"3:7::{"id":7,"url":"/v4/f/x/other","body":""}"#;
        let (reply, hint) = handle_frame(frame, &watch);
        assert_eq!(
            reply,
            Some(r#"3:::{"id":7,"status":200,"body":""}"#.to_string())
        );
        assert_eq!(hint, None, "only a messaging url produces a hint");
    }

    /// A `3` request frame shaped as it was confirmed live (`vtteams.log`'s `Watch` output, 2026-11-06): a small
    /// REST-request envelope whose own `body` field is a JSON *string*, not an object.
    fn messaging_frame(id: u64, inner: &serde_json::Value) -> String {
        let envelope = json!({
            "id": id,
            "url": "/v4/f/x/messaging",
            "method": "POST",
            "headers": {},
            "body": inner.to_string(),
        });
        format!("3:{id}::{envelope}")
    }

    #[test]
    fn a_messaging_request_names_the_chat_from_conversationid_in_the_decoded_body() {
        let watch = Watch::default();
        let frame = messaging_frame(9, &json!({"conversationid": "19:abc@thread.v2"}));
        let (reply, hint) = handle_frame(&frame, &watch);
        assert!(reply.is_some(), "still acked");
        assert_eq!(hint, Some(PushHint::Chat("19:abc@thread.v2".to_string())));
    }

    #[test]
    fn a_messaging_request_takes_the_trailing_id_out_of_a_conversation_link_url() {
        let watch = Watch::default();
        let frame = messaging_frame(
            9,
            &json!({"conversationLink": "https://teams.microsoft.com/v1/users/ME/conversations/19:abc@thread.v2"}),
        );
        let (_, hint) = handle_frame(&frame, &watch);
        assert_eq!(hint, Some(PushHint::Chat("19:abc@thread.v2".to_string())));
    }

    /// The decoded body's real shape, confirmed live 2026-11-06: a change-notification envelope with the
    /// conversation details one level deeper, under `resource`.
    #[test]
    fn a_messaging_request_names_the_chat_from_conversationid_nested_under_resource() {
        let watch = Watch::default();
        let frame = messaging_frame(
            9,
            &json!({
                "type": "EventMessage",
                "resourceType": "NewMessage",
                "resourceLink": "/v1/users/ME/conversations/19:abc@thread.v2/messages/123",
                "time": "2026-11-06T02:54:38Z",
                "isactive": true,
                "mentiontypes": "",
                "resource": {"conversationid": "19:abc@thread.v2"},
            }),
        );
        let (_, hint) = handle_frame(&frame, &watch);
        assert_eq!(hint, Some(PushHint::Chat("19:abc@thread.v2".to_string())));
    }

    #[test]
    fn a_messaging_request_with_an_unrecognised_decoded_body_still_broadens_to_unknown() {
        let watch = Watch::default();
        let frame = messaging_frame(9, &json!({"somethingElse": true}));
        let (_, hint) = handle_frame(&frame, &watch);
        assert_eq!(hint, Some(PushHint::Unknown));
    }

    #[test]
    fn a_messaging_request_whose_body_is_not_json_at_all_still_broadens_to_unknown() {
        let watch = Watch::default();
        let frame = r#"3:9::{"id":9,"url":"/v4/f/x/messaging","body":"not json"}"#;
        let (reply, hint) = handle_frame(frame, &watch);
        assert!(reply.is_some(), "still acked");
        assert_eq!(hint, Some(PushHint::Unknown));
    }

    #[test]
    fn a_named_sequential_event_is_acked_and_broadens_to_unknown() {
        let watch = Watch::default();
        let frame = r#"5:3+::{"name":"trouter.message_loss"}"#;
        let (reply, hint) = handle_frame(frame, &watch);
        assert_eq!(reply, Some("6:3::".to_string()));
        assert_eq!(hint, Some(PushHint::Unknown));
    }

    #[test]
    fn an_unknown_frame_type_is_ignored() {
        let watch = Watch::default();
        let (reply, hint) = handle_frame("0", &watch);
        assert_eq!(reply, None);
        assert_eq!(hint, None);
    }

    #[test]
    fn reconnect_delay_doubles_up_to_a_cap() {
        assert_eq!(reconnect_delay(0), Duration::from_secs(2));
        assert_eq!(reconnect_delay(1), Duration::from_secs(4));
        assert_eq!(reconnect_delay(6), Duration::from_secs(128));
        assert_eq!(
            reconnect_delay(20),
            Duration::from_secs(128),
            "stays capped"
        );
    }

    #[test]
    fn trailing_id_strips_everything_but_the_last_path_segment_and_any_query() {
        assert_eq!(trailing_id("19:abc@thread.v2"), "19:abc@thread.v2");
        assert_eq!(
            trailing_id("https://teams.microsoft.com/v1/users/ME/conversations/19:abc@thread.v2"),
            "19:abc@thread.v2"
        );
        assert_eq!(
            trailing_id("https://teams.microsoft.com/conversations/19:abc@thread.v2?view=x"),
            "19:abc@thread.v2"
        );
    }

    #[test]
    fn a_reply_in_a_channel_names_the_channel_not_the_thread() {
        // Both spellings of the separator, in a link and in a bare id.
        assert_eq!(
            trailing_id("19:abc@thread.tacv2;messageid=1700000000000"),
            "19:abc@thread.tacv2"
        );
        assert_eq!(
            trailing_id(
                "https://teams.microsoft.com/v1/users/ME/conversations/19:abc@thread.tacv2%3Bmessageid%3D1700000000000"
            ),
            "19:abc@thread.tacv2"
        );
        assert_eq!(
            trailing_id("https://x.example/conversations/19:abc@thread.tacv2%3bmessageid=5?view=y"),
            "19:abc@thread.tacv2"
        );
        let watch = Watch::default();
        let frame = messaging_frame(
            9,
            &json!({"resource": {"conversationLink":
                "https://teams.microsoft.com/v1/users/ME/conversations/19:abc@thread.tacv2;messageid=1700000000000"}}),
        );
        let (_, hint) = handle_frame(&frame, &watch);
        assert_eq!(
            hint,
            Some(PushHint::Chat("19:abc@thread.tacv2".to_string()))
        );
        let frame = messaging_frame(
            9,
            &json!({"resource": {"conversationid": "19:abc@thread.tacv2;messageid=1700000000000"}}),
        );
        let (_, hint) = handle_frame(&frame, &watch);
        assert_eq!(
            hint,
            Some(PushHint::Chat("19:abc@thread.tacv2".to_string()))
        );
    }

    #[test]
    fn trouter_hosts_are_recognised_and_nothing_else_is() {
        assert!(is_trouter_url(
            "https://go.trouter.teams.microsoft.com/v4/a",
            TROUTER_HOST
        ));
        assert!(is_trouter_url(
            "https://pub-ent-usnc-17-f.trouter.teams.microsoft.com:3443/v4/f/x/",
            TROUTER_HOST
        ));
        assert!(!is_trouter_url("https://evil.example/v4/a", TROUTER_HOST));
        assert!(!is_trouter_url(
            "https://trouter.teams.microsoft.com.evil.example/",
            TROUTER_HOST
        ));
        assert!(
            !is_trouter_url(
                "http://pub-ent-usnc-17-f.trouter.teams.microsoft.com/v4/a",
                TROUTER_HOST
            ),
            "http scheme is refused for production hosts"
        );
        assert!(
            is_trouter_url("http://127.0.0.1:9/v4/a", "http://127.0.0.1:9/"),
            "a test's own loopback host stands in for the real one"
        );
    }

    #[test]
    fn registrar_hosts_are_recognised_and_nothing_else_is() {
        assert!(is_registrar_url(
            "https://communications.svc.cloud.microsoft/registrar/noam/v3/registrations",
            TROUTER_HOST
        ));
        assert!(is_registrar_url(
            "https://teams.microsoft.com/registrar/prod/V2/registrations",
            TROUTER_HOST
        ));
        assert!(!is_registrar_url(
            "https://evil.example/registrar",
            TROUTER_HOST
        ));
        assert!(
            !is_registrar_url("http://teams.microsoft.com/registrar", TROUTER_HOST),
            "http scheme is refused for production hosts"
        );
        assert!(
            !is_registrar_url(
                "http://communications.svc.cloud.microsoft/registrar",
                TROUTER_HOST
            ),
            "http scheme is refused for production hosts"
        );
        assert!(
            is_registrar_url("http://127.0.0.1:9/registrar", "http://127.0.0.1:9/"),
            "a test's own loopback host stands in for the real one"
        );
    }

    #[test]
    fn endpoint_ids_look_distinct_each_time() {
        assert_ne!(endpoint_id(), endpoint_id());
    }

    #[test]
    fn the_push_socket_is_not_dialled_into_a_server_whose_certificate_nothing_trusts() {
        let (port, server) = crate::testserver::untrusted_tls_server();
        let reg = Registration {
            socketio: format!("https://127.0.0.1:{port}"),
            surl: String::new(),
            registrar_url: String::new(),
            connect_params: json!({}),
            ccid: String::new(),
        };
        let err = dial(&reg, "endpoint", "session", "skype-token")
            .expect_err("a certificate nothing trusts must be refused");
        assert!(matches!(err, Error::Transport { .. }), "{err:?}");
        assert!(
            !server.join().unwrap(),
            "no upgrade request, and so no token, reached a server the system does not trust"
        );
    }

    #[test]
    fn while_the_sign_in_is_lost_the_push_thread_asks_for_no_credentials_and_connects_once_it_is_back()
     {
        use crate::teams::tests::backend;
        use crate::testserver::{Reply, serve};
        use std::sync::atomic::Ordering;

        let server = serve((0..4).map(|_| Reply::status(500).body("no")).collect());
        let mut b = backend(&server);
        b.trouter_host = format!("{}/", server.origin());
        let b = Arc::new(b);
        b.auth.lose_sign_in_for_test();
        let weak = Arc::downgrade(&b);
        let thread = thread::spawn(move || run(weak));
        // Many of its waits go by: it never asks for credentials (which would start a renewal) or calls anyone.
        thread::sleep(SIGN_IN_CHECK_INTERVAL * 10);
        assert_eq!(
            b.live_calls.load(Ordering::SeqCst),
            0,
            "no credentials asked for while signed out"
        );
        assert!(server.seen().is_empty(), "nothing sent while signed out");
        // Signed in again: the next wait ends and it connects (the registration request arrives).
        b.auth.restore_sign_in_for_test();
        let deadline = Instant::now() + Duration::from_secs(10);
        while server.seen().is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(b.live_calls.load(Ordering::SeqCst) > 0);
        assert!(
            !server.seen().is_empty(),
            "it connects once the sign-in is good again"
        );
        // Dropping the backend ends the thread at its next look (it holds only a weak reference).
        drop(b);
        let _ = thread;
    }

    #[test]
    fn the_push_thread_waits_only_while_the_sign_in_is_lost() {
        use crate::teams::tests::backend;
        use crate::testserver::serve;
        let server = serve(vec![]);
        let b = Arc::new(backend(&server));
        let weak = Arc::downgrade(&b);
        assert!(!should_wait_for_sign_in(&weak));
        b.auth.lose_sign_in_for_test();
        assert!(should_wait_for_sign_in(&weak));
        drop(b);
        assert!(
            !should_wait_for_sign_in(&weak),
            "a backend that is gone is not waited for"
        );
    }
}
