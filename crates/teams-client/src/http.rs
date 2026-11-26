//! Blocking HTTP plumbing shared by the backends: agent construction, response checking and error mapping, and
//! image download. Nothing here is public.

use crate::error::{Error, Result};
use crate::headerlog::HeaderLog;
use crate::record::{Exchange, ShapeRecorder};
use crate::text::clean;
use image::{ImageReader, Limits, RgbImage};
use serde_json::Value;
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ureq::Agent;
use ureq::config::Config;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

/// Largest image download accepted, in bytes.
const MAX_IMAGE_BYTES: u64 = 10_000_000;
/// A decoded image needs at least this many pixels on its shorter side and altogether to be worth showing. A thin
/// strip (a cropped toolbar, a banner) is fine; a dot or a one-pixel rule is not.
const MIN_IMAGE_SIDE_PX: u32 = 4;
const MIN_IMAGE_AREA_PX: u32 = 256;
/// Images are scaled down to fit within this many pixels altogether and this many on their longer side. Limiting the
/// area rather than a side keeps a screenshot of any shape as sharp as the memory allows (a tall phone screenshot is
/// not squeezed to a narrow column) while bounding what one picture costs: 1.7 million pixels is about 5 MB.
const MAX_IMAGE_AREA_PX: u64 = 1_700_000;
const MAX_IMAGE_SIDE_PX: u32 = 2400;
/// Hard limits applied while decoding, so a small file cannot expand into a huge bitmap (a decompression bomb).
const MAX_DECODE_DIMENSION: u32 = 8192;
const MAX_DECODE_ALLOC: u64 = 128 * 1024 * 1024;
const MAX_ERROR_CHARS: usize = 300;
/// Largest API answer accepted, in bytes after decompression.
const MAX_BODY_BYTES: u64 = 10 * 1024 * 1024;

type Response = ureq::http::Response<ureq::Body>;

/// How many times a read (a GET, which changes nothing) is sent again, on another connection, after the service turns
/// out to have closed the one it was sent on.
///
/// The agent keeps connections open between requests, and a service (or something between) closes one now and then
/// without the client being told until it next uses it: the request then fails at once with a reset or an end of
/// stream, before any answer. Seen live as "An existing connection was forcibly closed by the remote host (os error
/// 10054)" in a poll that failed after 13 ms, a few times a day. `ureq` tolerates a close only while it is reading a
/// body, not while sending the request or waiting for the answer's first bytes, and a poll that fails also doubles the
/// wait before the next one. Two repeats, not one, because the pool may hold more than one connection the service has
/// dropped; each failed attempt discards the connection it used, so the repeats end in a new one.
///
/// Only reads are repeated. A write whose connection drops may have been delivered, and sending it again would
/// duplicate it (`Error::delivery_unknown` tells the caller to check first).
const RECONNECTS: u32 = 2;

/// Whether `e` is the other end having closed the connection: a reset, an abort, a broken pipe or an end of stream
/// where an answer was expected. Not a refusal to connect, a name that does not resolve or a timeout: those are the
/// network being down, and trying again at once would not help.
fn connection_dropped(e: &ureq::Error) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, UnexpectedEof};
    matches!(
        e,
        ureq::Error::Io(io)
            if matches!(io.kind(), ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof)
    )
}

/// An agent that trusts the operating system's certificate store only (this works behind TLS-inspecting proxies).
/// 4xx/5xx responses are returned rather than raised, so their JSON error bodies can be read. Redirects are never
/// followed by the agent: credentials go only to hosts the caller has checked, and a redirect names a host nobody has
/// checked (and the client would forward every header except `Authorization`). A redirect answer is an error, except
/// for images, where [`fetch_image`] follows redirects itself and checks every hop.
pub(crate) fn agent(timeout: Duration) -> Agent {
    build_agent(timeout, None)
}

/// [`agent`], with the response headers `log` selects written to the log (see [`HeaderLog`]).
pub(crate) fn logging_agent(timeout: Duration, log: &Arc<HeaderLog>) -> Agent {
    build_agent(timeout, Some(log))
}

fn build_agent(timeout: Duration, log: Option<&Arc<HeaderLog>>) -> Agent {
    let builder = Config::builder()
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::Rustls)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .timeout_global(Some(timeout))
        .max_redirects(0)
        .http_status_as_error(false)
        .user_agent(concat!("teams-client/", env!("CARGO_PKG_VERSION")));
    let builder = match log {
        Some(log) => builder.middleware(HeaderLog::layer(log)),
        None => builder,
    };
    Agent::new_with_config(builder.build())
}

/// An agent for talking to a program on this computer over plain HTTP (the browser's DevTools port). It never uses a
/// proxy: ureq reads `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` from the environment and has no exemption for loopback, so
/// with one of those set and no `NO_PROXY` covering 127.0.0.1 every request would go to the proxy and fail.
#[cfg(feature = "teams")]
pub(crate) fn local_agent(timeout: Duration) -> Agent {
    let config = Config::builder()
        .proxy(None)
        .timeout_global(Some(timeout))
        .max_redirects(0)
        .http_status_as_error(false)
        .build();
    Agent::new_with_config(config)
}

/// The longest retry-after delay honoured, to prevent unbounded waits.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// The `Retry-After` header, when it is a number of seconds.
fn retry_after(resp: &Response) -> Option<Duration> {
    resp.headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
        .map(Duration::from_secs)
        .map(|d| d.min(MAX_RETRY_AFTER))
}

/// Read a body of at most `max` bytes *after* decompression. The HTTP client's own limit counts the bytes on the wire,
/// so a small gzip answer could otherwise expand without bound.
fn read_decoded(operation: &'static str, body: &mut ureq::Body, max: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let too_large = Error::TooLarge {
        operation,
        limit_bytes: max,
    };
    let mut bytes = Vec::new();
    // The wire limit stays as well: it stops a slow trickle of compressed data early.
    let mut reader = body.with_config().limit(max).reader().take(max + 1);
    match reader.read_to_end(&mut bytes) {
        Ok(_) if bytes.len() as u64 > max => Err(too_large),
        Ok(_) => Ok(bytes),
        Err(e)
            if e.get_ref()
                .and_then(|inner| inner.downcast_ref::<ureq::Error>())
                .is_some_and(|inner| matches!(inner, ureq::Error::BodyExceedsLimit(_))) =>
        {
            Err(too_large)
        }
        Err(e) => Err(Error::transport(operation, e)),
    }
}

/// Read a response body as text.
pub(crate) fn body_text(operation: &'static str, resp: &mut Response) -> Result<String> {
    let bytes = read_decoded(operation, resp.body_mut(), MAX_BODY_BYTES)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Read and parse a response body as JSON with a size cap.
pub(crate) fn body_json<T: serde::de::DeserializeOwned>(
    operation: &'static str,
    resp: &mut Response,
    max_bytes: u64,
) -> Result<T> {
    let bytes = read_decoded(operation, resp.body_mut(), max_bytes)?;
    serde_json::from_slice(&bytes).map_err(|e| Error::decode(operation, e))
}

/// Read a response body and turn an error status into the matching [`Error`]. Success returns the body text.
pub(crate) fn check(operation: &'static str, resp: &mut Response) -> Result<String> {
    let status = resp.status().as_u16();
    let wait = retry_after(resp);
    let text = body_text(operation, resp)?;
    if status >= 300 {
        return Err(status_error(operation, status, wait, &text));
    }
    Ok(text)
}

/// For OAuth endpoints, whose 4xx answers are `error` documents the caller interprets itself: fails for throttling
/// (429) and server errors (5xx), which are not an answer about the credentials, and lets everything else through.
pub(crate) fn transient_failure(operation: &'static str, resp: &mut Response) -> Result<()> {
    let status = resp.status().as_u16();
    if status == 429 || status >= 500 {
        check(operation, resp).map(|_| ())
    } else {
        Ok(())
    }
}

/// Parse a response body as JSON; an empty body is `null`.
pub(crate) fn parse_json(operation: &'static str, text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(text).map_err(|e| Error::decode(operation, e))
}

/// An error status, with the service's own explanation when its body has one (`{"error":{"message"}}` as Graph
/// sends, or `{"message"}` as chatsvc does), otherwise the start of the body. 429 is [`Error::Throttled`].
pub(crate) fn status_error(
    operation: &'static str,
    status: u16,
    retry_after: Option<Duration>,
    body: &str,
) -> Error {
    if status == 429 {
        return Error::Throttled {
            operation,
            retry_after,
        };
    }
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let message = v["error"]["message"]
        .as_str()
        .or(v["message"].as_str())
        .unwrap_or(body.trim());
    Error::Status {
        operation,
        status,
        message: clean(message).chars().take(MAX_ERROR_CHARS).collect(),
    }
}

/// Request headers for one attempt.
pub(crate) type Headers = Vec<(&'static str, String)>;

/// What one JSON call needs. Trust checks (is this URL one our credentials may go to?) are the caller's job.
pub(crate) struct Call<'a> {
    pub(crate) agent: &'a Agent,
    pub(crate) recorder: Option<&'a ShapeRecorder>,
    pub(crate) operation: &'static str,
    pub(crate) method: &'a str,
    pub(crate) url: &'a str,
    pub(crate) body: Option<&'a Value>,
}

fn with_headers<B>(mut rb: ureq::RequestBuilder<B>, headers: &Headers) -> ureq::RequestBuilder<B> {
    for (name, value) in headers {
        rb = rb.header(*name, value.as_str());
    }
    rb
}

/// Send a JSON request and parse the JSON answer. `headers_for(renewed)` supplies the headers for each attempt; after a
/// 401 the call is repeated once with `renewed = true`, so the caller can force fresh credentials. Error statuses
/// become typed errors (see [`status_error`]), and every exchange is handed to the recorder when there is one.
pub(crate) fn call_json(
    call: &Call<'_>,
    headers_for: &mut dyn FnMut(bool) -> Result<Headers>,
) -> Result<Value> {
    // Only these combinations are sent; anything else is a mistake in the caller, not a request to guess at.
    if !matches!(
        (call.method, call.body.is_some()),
        ("GET", false) | ("POST" | "PUT" | "DELETE", _)
    ) {
        return Err(Error::InvalidInput {
            what: "HTTP method for this request",
            value: call.method.to_string(),
        });
    }
    let mut renewed = false;
    let mut reconnects = 0;
    loop {
        // Renewing credentials can fail in ways that read as "the request may have got through" (a dropped connection
        // to the token endpoint); nothing has been written yet, so say so.
        let headers = headers_for(renewed).map_err(Error::not_sent)?;
        let started = Instant::now();
        let sent = match (call.method, call.body) {
            ("POST", Some(body)) => {
                with_headers(call.agent.post(call.url), &headers).send_json(body)
            }
            ("POST", None) => with_headers(call.agent.post(call.url), &headers).send_empty(),
            ("PUT", Some(body)) => with_headers(call.agent.put(call.url), &headers).send_json(body),
            ("PUT", None) => with_headers(call.agent.put(call.url), &headers).send_empty(),
            // Some services take the reaction to remove as a body on DELETE.
            ("DELETE", Some(body)) => {
                with_headers(call.agent.delete(call.url).force_send_body(), &headers)
                    .send_json(body)
            }
            ("DELETE", None) => with_headers(call.agent.delete(call.url), &headers).call(),
            _ => with_headers(call.agent.get(call.url), &headers).call(),
        };
        let mut resp = match sent {
            Ok(resp) => resp,
            // A read on a connection the service had already closed: see `RECONNECTS`.
            Err(e) if call.method == "GET" && reconnects < RECONNECTS && connection_dropped(&e) => {
                reconnects += 1;
                // At info, not debug: it is rare (a few a day) and is the evidence of a dropped connection, which
                // would otherwise be invisible at the default level because the repeat succeeds.
                log::info!(
                    "{}: GET {}: the service had closed the connection (after {} ms); sending the read again on a new one ({reconnects} of {RECONNECTS})",
                    call.operation,
                    host_of(call.url),
                    started.elapsed().as_millis(),
                );
                continue;
            }
            Err(e) => {
                record(call, None, "");
                let err = Error::transport(call.operation, e);
                log::warn!(
                    "{}: {} {} failed after {} ms: {}",
                    call.operation,
                    call.method,
                    host_of(call.url),
                    started.elapsed().as_millis(),
                    err.for_log()
                );
                return Err(err);
            }
        };
        let status = resp.status().as_u16();
        let wait = retry_after(&resp);
        let text = body_text(call.operation, &mut resp)?;
        record(call, Some(status), &text);
        log_exchange(call, status, started, text.len(), renewed);
        if status == 401 && !renewed {
            renewed = true;
            continue;
        }
        if status >= 300 {
            return Err(status_error(call.operation, status, wait, &text));
        }
        return parse_json(call.operation, &text);
    }
}

#[cfg(feature = "teams")]
/// Like [`check`], but also hands the exchange to the recorder first (for endpoints that do not go through
/// [`call_json`]).
pub(crate) fn check_recorded(call: &Call<'_>, resp: &mut Response) -> Result<String> {
    let status = resp.status().as_u16();
    let wait = retry_after(resp);
    let text = body_text(call.operation, resp)?;
    record(call, Some(status), &text);
    if status >= 300 {
        return Err(status_error(call.operation, status, wait, &text));
    }
    Ok(text)
}

/// Download a public file, with no credentials, refusing one larger than `max` bytes.
#[cfg(feature = "teams")]
pub(crate) fn fetch_public_text(
    agent: &Agent,
    operation: &'static str,
    url: &str,
    max: u64,
) -> Result<String> {
    let started = Instant::now();
    let mut resp = agent
        .get(url)
        .header("Accept", "application/json")
        .call()
        .map_err(|e| Error::transport(operation, e))?;
    let status = resp.status().as_u16();
    if status >= 300 {
        return Err(status_error(operation, status, retry_after(&resp), ""));
    }
    let bytes = read_decoded(operation, resp.body_mut(), max)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    log::debug!(
        "{operation}: GET {} -> {status}, {} bytes in {} ms",
        host_of(url),
        text.len(),
        started.elapsed().as_millis()
    );
    Ok(text)
}

/// The host of `url`, for log lines: the rest of a URL can carry identifiers and query values.
fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| "(unparsable url)".into())
}

/// One log record per exchange: operation, method, host, status, time and size. Successes are `trace`; an expired
/// credential that will be renewed and other refusals are `debug`; throttling and server errors are `warn`.
fn log_exchange(call: &Call<'_>, status: u16, started: Instant, bytes: usize, renewed: bool) {
    let level = match status {
        200..=299 => log::Level::Trace,
        429 | 500.. => log::Level::Warn,
        _ => log::Level::Debug,
    };
    if log::log_enabled!(level) {
        log::log!(
            level,
            "{}: {} {} -> {status} in {} ms, {bytes} bytes{}",
            call.operation,
            call.method,
            host_of(call.url),
            started.elapsed().as_millis(),
            if renewed {
                " (after renewing credentials)"
            } else {
                ""
            }
        );
    }
}

/// Hand an exchange to the recorder, if one is attached.
pub(crate) fn record(call: &Call<'_>, status: Option<u16>, response: &str) {
    if let Some(recorder) = call.recorder {
        recorder.record(&Exchange {
            operation: call.operation,
            method: call.method,
            url: call.url,
            status,
            request: call.body,
            response,
        });
    }
}

/// Most redirects an image download follows.
const MAX_IMAGE_REDIRECTS: usize = 3;

/// Download and decode an image, sending `authorization` as the `Authorization` header. A redirect is followed only
/// to a URL `allowed` accepts (the same rule that admitted the first URL), and the header goes only to such URLs.
pub(crate) fn fetch_image(
    agent: &Agent,
    url: &str,
    authorization: &str,
    allowed: &dyn Fn(&str) -> bool,
) -> Result<RgbImage> {
    const OPERATION: &str = "fetch image";
    if !allowed(url) {
        log::debug!("{OPERATION}: refused to fetch from {}", host_of(url));
        return Err(Error::UntrustedUrl {
            url: url.to_string(),
        });
    }
    let started = Instant::now();
    let mut url = url.to_string();
    let mut hops = 0;
    let mut reconnects = 0;
    let resp = loop {
        let mut req = agent.get(&url).header(
            "Accept",
            "image/png,image/jpeg,image/webp,image/gif;q=0.9,*/*;q=0.1",
        );
        // Empty means there is deliberately no credential for this host (a pre-authenticated URL Graph's Shares
        // API resolved, say): send nothing at all, rather than a header with nothing in it.
        if !authorization.is_empty() {
            req = req.header("Authorization", authorization);
        }
        let resp = match req.call() {
            Ok(resp) => resp,
            // The same read, sent again on a new connection: see `RECONNECTS`. Still the URL that was checked.
            Err(e) if reconnects < RECONNECTS && connection_dropped(&e) => {
                reconnects += 1;
                log::info!(
                    "{OPERATION}: GET {}: the service had closed the connection; sending the read again on a new one ({reconnects} of {RECONNECTS})",
                    host_of(&url),
                );
                continue;
            }
            Err(e) => return Err(Error::transport(OPERATION, e)),
        };
        let (status, wait) = (resp.status().as_u16(), retry_after(&resp));
        if (300..400).contains(&status) {
            let next = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .and_then(|l| url::Url::parse(&url).ok()?.join(l).ok())
                .map(String::from)
                .ok_or_else(|| status_error(OPERATION, status, wait, ""))?;
            hops += 1;
            if hops > MAX_IMAGE_REDIRECTS {
                return Err(Error::TooManyRedirects {
                    operation: OPERATION,
                });
            }
            if !allowed(&next) {
                log::debug!("{OPERATION}: refused a redirect to {}", host_of(&next));
                return Err(Error::UntrustedUrl { url: next });
            }
            url = next;
            continue;
        }
        if status >= 400 {
            return Err(status_error(OPERATION, status, wait, ""));
        }
        break resp;
    };
    let mut resp = resp;
    let bytes = read_decoded(OPERATION, resp.body_mut(), MAX_IMAGE_BYTES)?;
    let downloaded = started.elapsed();
    let img = decode_image(&bytes);
    log::debug!(
        "{OPERATION}: {} sent {} bytes in {} ms; {}",
        host_of(&url),
        bytes.len(),
        downloaded.as_millis(),
        match &img {
            Ok(i) => format!(
                "decoded to {}x{} in {} ms",
                i.width(),
                i.height(),
                (started.elapsed() - downloaded).as_millis()
            ),
            Err(e) => format!("not decoded: {}", e.for_log()),
        }
    );
    img
}

/// Decode image bytes within fixed dimension and memory limits, and scale them down to fit in 1.7 million pixels and
/// 2400 on a side (see [`scaled_size`]).
pub(crate) fn decode_image(bytes: &[u8]) -> Result<RgbImage> {
    const OPERATION: &str = "decode image";
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| Error::decode(OPERATION, e))?;
    reader.limits(limits);
    let img = reader.decode().map_err(|e| Error::decode(OPERATION, e))?;
    if img.width().min(img.height()) < MIN_IMAGE_SIDE_PX
        || img.width() * img.height() < MIN_IMAGE_AREA_PX
    {
        return Err(Error::Unexpected {
            operation: OPERATION,
            detail: "image too small to show",
        });
    }
    let (w, h) = scaled_size(img.width(), img.height());
    if (w, h) == (img.width(), img.height()) {
        return Ok(img.to_rgb8());
    }
    Ok(img.thumbnail(w, h).to_rgb8())
}

/// The size an image of `w` x `h` pixels is kept at: unchanged if it fits, else scaled down (keeping its shape) until
/// it does.
pub(crate) fn scaled_size(w: u32, h: u32) -> (u32, u32) {
    let (w, h) = (w.max(1), h.max(1));
    let by_area = (MAX_IMAGE_AREA_PX as f64 / (f64::from(w) * f64::from(h))).sqrt();
    let by_side = f64::from(MAX_IMAGE_SIDE_PX) / f64::from(w.max(h));
    let scale = by_area.min(by_side).min(1.0);
    if scale >= 1.0 {
        return (w, h);
    }
    (
        ((f64::from(w) * scale).floor() as u32).max(1),
        ((f64::from(h) * scale).floor() as u32).max(1),
    )
}

/// Minimal percent-encoding for a path segment or query value (Teams chat ids contain `:` and `@`).
pub(crate) fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'@' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::{Reply, serve};

    #[test]
    fn a_failure_to_prepare_the_request_reads_as_not_sent_and_is_never_a_possible_delivery() {
        let agent = agent(Duration::from_secs(1));
        let body = serde_json::json!({});
        let call = Call {
            agent: &agent,
            recorder: None,
            operation: "send message",
            method: "POST",
            url: "http://127.0.0.1:9/never-contacted",
            body: Some(&body),
        };
        // The token endpoint dropped the connection while credentials were being renewed.
        let err = call_json(&call, &mut |_| {
            Err(Error::transport(
                "renew credentials",
                std::io::Error::other("connection reset"),
            ))
        })
        .unwrap_err();
        assert!(matches!(err, Error::NotSent(_)), "{err:?}");
        assert!(!err.delivery_unknown(), "nothing was written");
        assert!(err.is_transient());
        // A sign-in that has simply gone stays what it was.
        let err = call_json(&call, &mut |_| {
            Err(Error::Auth(crate::error::AuthError::NotSignedIn))
        })
        .unwrap_err();
        assert!(matches!(err, Error::Auth(_)), "{err:?}");
    }

    #[cfg(feature = "teams")]
    #[test]
    fn the_api_agent_refuses_a_server_whose_certificate_nothing_trusts() {
        // The one test that puts a TLS handshake through the agent: a server presenting a self-signed certificate
        // (for exactly the address asked for, so that nothing but the untrusted issuer is wrong with it) must be
        // refused before any request is sent.
        let (port, server) = crate::testserver::untrusted_tls_server();
        let err = agent(Duration::from_secs(5))
            .get(&format!("https://127.0.0.1:{port}/"))
            .header("Authorization", "Bearer must-not-be-sent")
            .call()
            .expect_err("an untrusted certificate must fail the request");
        assert!(!err.to_string().is_empty());
        assert!(
            !server.join().unwrap(),
            "the request, with its credentials, never reached the server"
        );
    }

    /// The control for the test above: the same server, asked by a client that does not verify, does receive the
    /// request, so the refusal there is the certificate check and nothing else (a broken fixture would also refuse).
    #[cfg(feature = "teams")]
    #[test]
    fn the_server_with_the_untrusted_certificate_works_for_a_client_that_does_not_check_it() {
        let (port, server) = crate::testserver::untrusted_tls_server();
        let config = Config::builder()
            .tls_config(
                TlsConfig::builder()
                    .provider(TlsProvider::Rustls)
                    .disable_verification(true)
                    .build(),
            )
            .timeout_global(Some(Duration::from_secs(5)))
            .build();
        let _ = Agent::new_with_config(config)
            .get(&format!("https://127.0.0.1:{port}/"))
            .call();
        assert!(server.join().unwrap(), "the request reached the server");
    }

    #[cfg(feature = "teams")]
    #[test]
    fn the_local_agent_never_uses_a_proxy_whatever_the_environment_says() {
        // `ureq` would read HTTP_PROXY and friends for the ordinary agent; the one for the browser's loopback port
        // must be built without any, so a proxy in the environment cannot capture the DevTools requests.
        assert!(
            local_agent(Duration::from_secs(1))
                .config()
                .proxy()
                .is_none()
        );
    }

    #[test]
    fn chat_ids_are_path_encoded() {
        assert_eq!(
            urlencode("19:abc@unq.gbl.spaces"),
            "19%3Aabc@unq.gbl.spaces"
        );
    }

    #[test]
    fn error_messages_come_from_either_service_shape_or_the_raw_body() {
        let msg = |body: &str| match status_error("op", 400, None, body) {
            Error::Status { message, .. } => message,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            msg(r#"{"error":{"code":"x","message":"graph says no"}}"#),
            "graph says no"
        );
        assert_eq!(msg(r#"{"message":"chatsvc says no"}"#), "chatsvc says no");
        assert_eq!(msg("  plain text body "), "plain text body");
        assert_eq!(msg(&"x".repeat(1000)).chars().count(), MAX_ERROR_CHARS);
        assert!(!msg("bad\u{1b}[31m").contains('\u{1b}'));
    }

    #[test]
    fn throttling_is_its_own_error_and_carries_retry_after() {
        let server = serve(vec![
            Reply::status(429)
                .header("Retry-After", "7")
                .body("slow down"),
            Reply::status(500).body(r#"{"message":"boom"}"#),
        ]);
        let agent = agent(Duration::from_secs(5));
        let mut resp = agent.get(&server.url("/a")).call().unwrap();
        match check("op", &mut resp) {
            Err(Error::Throttled {
                operation: "op",
                retry_after: Some(d),
            }) => assert_eq!(d, Duration::from_secs(7)),
            other => panic!("{other:?}"),
        }
        let mut resp = agent.get(&server.url("/b")).call().unwrap();
        let err = check("op", &mut resp).unwrap_err();
        assert!(
            matches!(err, Error::Status { status: 500, .. }) && err.is_transient(),
            "{err:?}"
        );
    }

    fn make_call<'a>(agent: &'a Agent, url: &'a str, body: Option<&'a Value>) -> Call<'a> {
        Call {
            agent,
            recorder: None,
            operation: "op",
            method: if body.is_some() { "POST" } else { "GET" },
            url,
            body,
        }
    }

    #[test]
    fn a_request_that_is_not_a_known_method_is_refused_instead_of_sent_as_a_get() {
        let server = serve(vec![Reply::status(200).body("{}")]);
        let agent = agent(Duration::from_secs(5));
        let url = server.url("/x");
        let body = serde_json::json!({});
        for (method, with_body) in [("PATCH", false), ("GET", true), ("", false)] {
            let call = Call {
                agent: &agent,
                recorder: None,
                operation: "op",
                method,
                url: &url,
                body: with_body.then_some(&body),
            };
            let result = call_json(&call, &mut |_| Ok(Vec::new()));
            assert!(
                matches!(result, Err(Error::InvalidInput { .. })),
                "{method:?} {with_body}"
            );
        }
        assert!(server.seen().is_empty(), "nothing was sent");
    }

    #[test]
    fn an_image_address_the_caller_does_not_accept_is_not_requested_with_the_credential() {
        let server = serve(vec![Reply::status(200).body("x")]);
        let agent = agent(Duration::from_secs(5));
        let err = fetch_image(&agent, &server.url("/i.png"), "secret", &|_| false).unwrap_err();
        assert!(matches!(err, Error::UntrustedUrl { .. }), "{err:?}");
        assert!(server.seen().is_empty());
    }
    #[test]
    fn a_401_is_retried_once_with_renewed_headers_and_a_second_401_is_an_error() {
        let server = serve(vec![
            Reply::status(401).json(r#"{"message":"expired"}"#),
            Reply::status(200).json(r#"{"ok":true}"#),
            Reply::status(401).body(""),
            Reply::status(401).body(""),
        ]);
        let agent = agent(Duration::from_secs(5));
        let url = server.url("/x");
        let call = make_call(&agent, &url, None);
        let mut seen_renewed = Vec::new();
        let v = call_json(&call, &mut |renewed| {
            seen_renewed.push(renewed);
            Ok(vec![(
                "Authorization",
                format!("Bearer attempt-{}", seen_renewed.len()),
            )])
        })
        .unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(seen_renewed, [false, true]);
        let seen = server.seen();
        assert_eq!(
            (
                seen[0].header("authorization"),
                seen[1].header("authorization")
            ),
            (Some("Bearer attempt-1"), Some("Bearer attempt-2"))
        );
        let err = call_json(&call, &mut |_| Ok(vec![])).unwrap_err();
        assert!(
            matches!(err, Error::Status { status: 401, .. }) && err.needs_sign_in(),
            "{err:?}"
        );
    }

    #[test]
    fn post_bodies_are_sent_as_json_and_header_failures_stop_the_call() {
        let server = serve(vec![Reply::status(200).json("{}")]);
        let agent = agent(Duration::from_secs(5));
        let body = serde_json::json!({"a": 1});
        let url = server.url("/post");
        let call = make_call(&agent, &url, Some(&body));
        call_json(&call, &mut |_| Ok(vec![("X-Test", "yes".into())])).unwrap();
        let seen = &server.seen()[0];
        assert_eq!(
            (seen.method.as_str(), seen.header("x-test")),
            ("POST", Some("yes"))
        );
        assert!(
            seen.header("content-type")
                .is_some_and(|c| c.starts_with("application/json"))
        );
        assert_eq!(serde_json::from_str::<Value>(&seen.body).unwrap(), body);
        let failing = call_json(&call, &mut |_| {
            Err(Error::InvalidInput {
                what: "credentials",
                value: String::new(),
            })
        });
        assert!(matches!(failing, Err(Error::InvalidInput { .. })));
        assert_eq!(server.seen().len(), 1, "no request was sent");
    }

    #[test]
    fn every_exchange_goes_to_the_recorder_masked() {
        let dir = crate::testdir::TestDir::new("call-shapes");
        let recorder = ShapeRecorder::new(&*dir).unwrap();
        let server = serve(vec![
            Reply::status(404).json(r#"{"message":"no chat 19:abc@thread.v2"}"#),
        ]);
        let agent = agent(Duration::from_secs(5));
        let url =
            server.url("/api/chatsvc/amer/v1/users/ME/conversations/19:abc@thread.v2/messages");
        let mut call = make_call(&agent, &url, None);
        call.recorder = Some(&recorder);
        assert!(call_json(&call, &mut |_| Ok(vec![])).is_err());
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        let text = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(
            text.contains("\"status\": 404") && !text.contains("abc@thread"),
            "{text}"
        );
    }

    #[test]
    fn an_oversized_image_download_is_its_own_error() {
        let server = serve(vec![
            Reply::status(200).body(&"x".repeat(MAX_IMAGE_BYTES as usize + 1)),
        ]);
        let agent = agent(Duration::from_secs(30));
        match fetch_image(&agent, &server.url("/big.png"), "Bearer t", &|_| true) {
            Err(Error::TooLarge { limit_bytes, .. }) => {
                assert_eq!(limit_bytes, MAX_IMAGE_BYTES)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn api_calls_never_follow_redirects_so_credentials_stay_on_the_checked_host() {
        let elsewhere = serve(vec![Reply::status(200).json("{}")]);
        let server = serve(vec![
            Reply::status(302).header("Location", &elsewhere.url("/other")),
        ]);
        let agent = agent(Duration::from_secs(5));
        let url = server.url("/x");
        let call = make_call(&agent, &url, None);
        let err = call_json(&call, &mut |_| {
            Ok(vec![("Authentication", "skypetoken=secret".into())])
        })
        .unwrap_err();
        assert!(matches!(err, Error::Status { status: 302, .. }), "{err:?}");
        assert!(
            elsewhere.seen().is_empty(),
            "nothing was sent to the redirect target"
        );
    }

    #[test]
    fn image_redirects_go_only_where_the_caller_allows_and_only_a_few_hops() {
        let agent = agent(Duration::from_secs(5));
        let target = serve(vec![Reply::status(200).body("not an image")]);
        let refused = serve(vec![
            Reply::status(302).header("Location", &target.url("/img")),
        ]);
        let err = fetch_image(&agent, &refused.url("/a"), "Bearer t", &|u| {
            !u.starts_with(target.origin())
        })
        .unwrap_err();
        assert!(matches!(err, Error::UntrustedUrl { .. }), "{err:?}");
        assert!(
            target.seen().is_empty(),
            "the refused hop was never requested"
        );

        let allowed = serve(vec![
            Reply::status(302).header("Location", "/relative"),
            Reply::status(302).header("Location", &target.url("/img")),
        ]);
        let err = fetch_image(&agent, &allowed.url("/a"), "Bearer t", &|_| true).unwrap_err();
        assert!(
            matches!(err, Error::Decode { .. }),
            "followed to the end, where the body is not an image: {err:?}"
        );
        assert_eq!(
            allowed.seen()[1].path,
            "/relative",
            "relative locations resolve"
        );
        assert_eq!(
            target.seen()[0].header("authorization"),
            Some("Bearer t"),
            "an allowed hop carries the header"
        );

        let looping = serve(
            (0..6)
                .map(|_| Reply::status(302).header("Location", "/again"))
                .collect(),
        );
        let err = fetch_image(&agent, &looping.url("/a"), "Bearer t", &|_| true).unwrap_err();
        assert!(matches!(err, Error::TooManyRedirects { .. }), "{err:?}");
    }

    #[test]
    fn empty_bodies_are_null_and_bad_json_is_a_decode_error() {
        assert_eq!(parse_json("op", "  ").unwrap(), Value::Null);
        assert!(matches!(
            parse_json("op", "{nope"),
            Err(Error::Decode {
                operation: "op",
                ..
            })
        ));
    }

    #[test]
    fn tiny_or_undecodable_images_are_rejected_with_typed_errors() {
        assert!(matches!(
            decode_image(b"not an image"),
            Err(Error::Decode { .. })
        ));
        let mut png = Cursor::new(Vec::new());
        RgbImage::new(4, 4)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        assert!(matches!(
            decode_image(png.get_ref()),
            Err(Error::Unexpected { .. })
        ));
    }

    #[test]
    fn images_over_the_dimension_limit_are_refused_but_the_limit_itself_is_fine() {
        // Both are valid PNGs and tiny on the wire (solid colour); only the declared dimensions differ.
        let png = |w: u32, h: u32| {
            let mut out = Cursor::new(Vec::new());
            RgbImage::new(w, h)
                .write_to(&mut out, image::ImageFormat::Png)
                .unwrap();
            out.into_inner()
        };
        assert!(
            decode_image(&png(MAX_DECODE_DIMENSION, 20)).is_ok(),
            "exactly at the limit decodes"
        );
        match decode_image(&png(MAX_DECODE_DIMENSION + 1, 20)) {
            Err(Error::Decode {
                operation: "decode image",
                source,
            }) => assert!(
                source.to_string().to_lowercase().contains("limit"),
                "{source}"
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn normal_images_are_scaled_down() {
        let mut png = Cursor::new(Vec::new());
        RgbImage::new(1600, 400)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let img = decode_image(png.get_ref()).unwrap();
        assert_eq!(
            (img.width(), img.height()),
            (1600, 400),
            "a wide strip within the budget keeps every pixel"
        );
    }

    #[test]
    fn big_images_are_scaled_by_area_so_tall_and_wide_ones_keep_their_shape() {
        assert_eq!(scaled_size(2000, 439), (2000, 439));
        assert_eq!(scaled_size(1066, 1356), (1066, 1356));
        let (w, h) = scaled_size(1080, 2400);
        assert!(
            u64::from(w) * u64::from(h) <= MAX_IMAGE_AREA_PX && w > 800,
            "{w}x{h}"
        );
        assert!(
            (f64::from(w) / f64::from(h) - 1080.0 / 2400.0).abs() < 0.01,
            "{w}x{h} keeps the shape"
        );
        let (w, h) = scaled_size(4000, 3000);
        assert!(u64::from(w) * u64::from(h) <= MAX_IMAGE_AREA_PX);
        // An extreme strip is held to the side limit.
        assert_eq!(scaled_size(8000, 100), (2400, 30));
        assert_eq!(scaled_size(200, 6000), (80, 2400));
        assert_eq!(scaled_size(0, 0), (1, 1));
    }

    #[test]
    fn thin_strips_and_small_crops_are_kept_but_dots_and_rules_are_not() {
        let png = |w: u32, h: u32| {
            let mut out = Cursor::new(Vec::new());
            RgbImage::new(w, h)
                .write_to(&mut out, image::ImageFormat::Png)
                .unwrap();
            out.into_inner()
        };
        for (w, h) in [(400, 12), (12, 400), (64, 8), (300, 120)] {
            let img = decode_image(&png(w, h)).unwrap();
            assert_eq!((img.width(), img.height()), (w, h));
        }
        for (w, h) in [(400, 2), (3, 400), (10, 10), (1, 1)] {
            assert!(decode_image(&png(w, h)).is_err(), "{w}x{h}");
        }
    }

    use proptest::prelude::*;

    fn percent_decode(s: &str) -> Vec<u8> {
        let bytes = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                out.push(
                    u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap(), 16)
                        .unwrap(),
                );
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        out
    }

    proptest! {
        #[test]
        fn urlencode_round_trips_and_emits_only_safe_characters(s in any::<String>()) {
            let encoded = urlencode(&s);
            prop_assert!(encoded.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '@' | '%')));
            prop_assert_eq!(percent_decode(&encoded), s.as_bytes());
        }

        #[test]
        fn error_bodies_are_always_summarised_without_control_characters(body in any::<String>(), status in 400u16..600) {
            prop_assume!(status != 429, "429 is Throttled, tested separately");
            match status_error("op", status, None, &body) {
                Error::Status { message, .. } => {
                    prop_assert!(message.chars().count() <= MAX_ERROR_CHARS);
                    prop_assert!(!message.chars().any(|c| c.is_control() && c != '\n' && c != '\t'));
                }
                other => prop_assert!(false, "{other:?}"),
            }
        }

        #[test]
        fn arbitrary_bytes_are_never_a_panic_when_decoded_as_an_image(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
            let _ = decode_image(&bytes);
        }
    }

    /// A gzip answer of `megabytes` MB of zeros: a few dozen kB on the wire.
    fn gzip_of_zeros(megabytes: usize) -> Vec<u8> {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        let chunk = vec![0u8; 1024 * 1024];
        for _ in 0..megabytes {
            enc.write_all(&chunk).unwrap();
        }
        enc.finish().unwrap()
    }

    fn compressed(megabytes: usize) -> Reply {
        let body = gzip_of_zeros(megabytes);
        assert!(
            body.len() < 200_000,
            "the test answer must be small on the wire"
        );
        Reply::status(200)
            .header("Content-Encoding", "gzip")
            .bytes(body)
    }

    #[test]
    fn body_json_respects_the_size_cap() {
        let server = serve(vec![compressed(50)]);
        let agent = agent(Duration::from_secs(20));
        let mut resp = agent.get(&server.url("/big")).call().unwrap();
        let err = body_json::<Value>("test", &mut resp, 1024 * 1024).unwrap_err();
        assert!(
            matches!(&err, Error::TooLarge { limit_bytes, .. } if *limit_bytes == 1024 * 1024),
            "{err:?}"
        );
    }

    #[cfg(feature = "teams")]
    #[test]
    fn a_small_gzip_answer_cannot_expand_past_the_limit_for_text_downloads() {
        let server = serve(vec![compressed(50), Reply::status(200).body("fine")]);
        let agent = agent(Duration::from_secs(20));
        let err = fetch_public_text(&agent, "test", &server.url("/big"), 1024 * 1024).unwrap_err();
        assert!(
            matches!(&err, Error::TooLarge { limit_bytes, .. } if *limit_bytes == 1024 * 1024),
            "{err:?}"
        );
        assert_eq!(
            fetch_public_text(&agent, "test", &server.url("/ok"), 1024 * 1024).unwrap(),
            "fine",
            "an ordinary answer still works"
        );
    }

    #[test]
    fn a_small_gzip_answer_cannot_expand_past_the_limit_for_api_answers() {
        let server = serve(vec![compressed(40)]);
        let agent = agent(Duration::from_secs(20));
        let mut resp = agent.get(&server.url("/api")).call().unwrap();
        let err = check("test", &mut resp).unwrap_err();
        assert!(
            matches!(&err, Error::TooLarge { limit_bytes, .. } if *limit_bytes == MAX_BODY_BYTES),
            "{err:?}"
        );
    }

    #[test]
    fn a_small_gzip_answer_cannot_expand_past_the_limit_for_images() {
        let server = serve(vec![compressed(40)]);
        let agent = agent(Duration::from_secs(20));
        let err = fetch_image(&agent, &server.url("/img"), "token", &|_| true).unwrap_err();
        assert!(
            matches!(&err, Error::TooLarge { limit_bytes, .. } if *limit_bytes == MAX_IMAGE_BYTES),
            "{err:?}"
        );
    }

    fn call<'a>(agent: &'a Agent, method: &'static str, url: &'a str) -> Call<'a> {
        Call {
            agent,
            recorder: None,
            operation: "read messages",
            method,
            url,
            body: None,
        }
    }

    /// A service closes a connection the client is still holding open in its pool, and the client finds out only
    /// when it next uses it: seen live as "An existing connection was forcibly closed by the remote host (os error
    /// 10054)" a few times a day, from a poll that failed after 13 ms (a new connection takes longer than that to
    /// set up) on a connection that had carried a request every four seconds before it.
    #[test]
    fn a_read_is_repeated_when_the_service_had_closed_the_connection_it_was_sent_on() {
        let server = serve(vec![
            Reply::hang_up(),
            Reply::status(200).json(r#"{"ok":true}"#),
        ]);
        let agent = agent(Duration::from_secs(5));
        let url = server.url("/messages");
        let answer = call_json(&call(&agent, "GET", &url), &mut |_| Ok(Vec::new())).unwrap();
        assert_eq!(answer["ok"], true);
        assert_eq!(server.seen().len(), 2, "the read was sent again");
    }

    #[test]
    fn a_read_is_repeated_a_bounded_number_of_times() {
        let hang_ups = (0..=RECONNECTS + 1).map(|_| Reply::hang_up()).collect();
        let server = serve(hang_ups);
        let agent = agent(Duration::from_secs(5));
        let url = server.url("/messages");
        let err = call_json(&call(&agent, "GET", &url), &mut |_| Ok(Vec::new())).unwrap_err();
        assert!(matches!(err, Error::Transport { .. }), "{err:?}");
        assert_eq!(
            server.seen().len(),
            1 + RECONNECTS as usize,
            "the first try and no more than the allowed repeats"
        );
    }

    /// Repeating a read changes nothing, but a write that may have been delivered must not be sent again behind the
    /// caller's back (`Error::delivery_unknown` is how the caller finds out and decides).
    #[test]
    fn a_write_is_never_repeated_after_a_dropped_connection() {
        for method in ["POST", "PUT", "DELETE"] {
            let server = serve(vec![Reply::hang_up(), Reply::status(200).json("{}")]);
            let agent = agent(Duration::from_secs(5));
            let url = server.url("/messages");
            let body = serde_json::json!({ "content": "hello" });
            let mut write = call(&agent, method, &url);
            write.operation = "send message";
            write.body = Some(&body);
            let err = call_json(&write, &mut |_| Ok(Vec::new())).unwrap_err();
            assert!(
                matches!(err, Error::Transport { .. }) && err.delivery_unknown(),
                "{method}: {err:?}"
            );
            assert_eq!(server.seen().len(), 1, "{method}: sent once");
        }
    }

    /// Only a connection the other end dropped is worth another try: a service that answers, even to refuse, is
    /// not asked twice, and neither is a host that cannot be reached at all.
    #[test]
    fn an_answer_or_an_unreachable_host_is_not_repeated() {
        let server = serve(vec![Reply::status(500).json(r#"{"message":"boom"}"#)]);
        let agent = agent(Duration::from_secs(5));
        let url = server.url("/messages");
        let err = call_json(&call(&agent, "GET", &url), &mut |_| Ok(Vec::new())).unwrap_err();
        assert!(matches!(err, Error::Status { status: 500, .. }), "{err:?}");
        assert_eq!(server.seen().len(), 1);
        let err = call_json(
            &call(&agent, "GET", "http://127.0.0.1:9/never"),
            &mut |_| Ok(Vec::new()),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Transport { .. }), "{err:?}");
    }

    /// The hook reads the response, nothing else: the request goes out exactly as it would have, its credentials
    /// are not in the log, and a credential-bearing response header is acknowledged without its value.
    #[test]
    fn a_logging_agent_logs_the_selected_response_headers_and_changes_nothing_else() {
        let server = serve(vec![
            Reply::status(200)
                .header("X-Cache", "CONFIG_NOCACHE")
                .header("Set-Cookie", "sid=SECRET-COOKIE")
                .json(r#"{"ok":true}"#),
        ]);
        let log = HeaderLog::parse("connection,keep-alive,set-cookie,x-cache").unwrap();
        let agent = logging_agent(Duration::from_secs(5), &Arc::new(log.clone()));
        let url = server.url("/messages");
        let answer = call_json(&call(&agent, "GET", &url), &mut |_| {
            Ok(vec![("Authorization", "Bearer REQUEST-SECRET".to_string())])
        })
        .unwrap();
        assert_eq!(answer["ok"], true, "the answer comes through untouched");
        let seen = server.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].header("authorization"),
            Some("Bearer REQUEST-SECRET")
        );
        let lines = log.captured();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("GET 127.0.0.1 -> 200 HTTP/1.1: "),
            "{}",
            lines[0]
        );
        for wanted in [
            "x-cache=CONFIG_NOCACHE",
            "set-cookie=<withheld>",
            "connection=close",
            "keep-alive=(absent)",
        ] {
            assert!(lines[0].contains(wanted), "{wanted}: {}", lines[0]);
        }
        assert!(!lines[0].contains("SECRET"), "{}", lines[0]);
    }

    #[test]
    fn a_picture_is_fetched_again_when_the_service_had_closed_the_connection() {
        let mut png = Cursor::new(Vec::new());
        RgbImage::new(32, 32)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let server = serve(vec![
            Reply::hang_up(),
            Reply::status(200).bytes(png.into_inner()),
        ]);
        let agent = agent(Duration::from_secs(5));
        let img = fetch_image(&agent, &server.url("/i.png"), "", &|_| true).unwrap();
        assert_eq!((img.width(), img.height()), (32, 32));
        assert_eq!(server.seen().len(), 2);
    }

    #[test]
    fn unbounded_retry_after_is_capped() {
        let server = serve(vec![
            Reply::status(429)
                .header("Retry-After", "18446744073709551615")
                .body("slow down"),
        ]);
        let agent = agent(Duration::from_secs(5));
        let mut resp = agent.get(&server.url("/a")).call().unwrap();
        match check("op", &mut resp) {
            Err(Error::Throttled {
                operation: "op",
                retry_after: Some(d),
            }) => assert_eq!(d, MAX_RETRY_AFTER),
            other => panic!("{other:?}"),
        }
    }
}
