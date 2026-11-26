//! A tiny scripted HTTP server for unit tests, so the request/response layer can be tested without the network.
//!
//! It serves one scripted [`Reply`] per incoming request, in order, on a loopback port, and records what it received.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// One scripted answer.
pub(crate) struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    /// A body that is not text (compressed data); sent instead of `body` when set.
    raw: Option<Vec<u8>>,
    /// Close the connection without answering (see [`Reply::hang_up`]).
    hang_up: bool,
    /// How long to wait, after the request has been read and recorded, before answering (see [`Reply::delayed`]).
    delay: std::time::Duration,
}

impl Reply {
    pub(crate) fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: String::new(),
            raw: None,
            hang_up: false,
            delay: std::time::Duration::ZERO,
        }
    }

    /// Answer only after `delay`, counted from the moment the request was read and recorded as seen (so a test that
    /// waits for the request to appear in [`Server::seen`] knows the answer is still to come). The server answers one
    /// request at a time, so requests behind this one wait too.
    #[cfg(feature = "teams")]
    pub(crate) fn delayed(mut self, delay: std::time::Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Take the request and close the connection without sending anything back: what a service does to a connection
    /// the client is still holding open after the service has given up on it. The request is recorded as seen.
    pub(crate) fn hang_up() -> Self {
        Self {
            hang_up: true,
            ..Self::status(0)
        }
    }

    pub(crate) fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub(crate) fn body(mut self, body: &str) -> Self {
        self.body = body.to_string();
        self
    }

    pub(crate) fn bytes(mut self, body: Vec<u8>) -> Self {
        self.raw = Some(body);
        self
    }

    pub(crate) fn json(self, body: &str) -> Self {
        self.header("Content-Type", "application/json").body(body)
    }
}

/// A request as the server saw it.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub(crate) method: String,
    pub(crate) path: String,
    /// Header names are lower-cased.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: String,
}

impl Seen {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == &name.to_ascii_lowercase())
            .map(|(_, v)| v.as_str())
    }
}

pub(crate) struct Server {
    origin: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Server {
    /// `http://127.0.0.1:<port>`
    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Start serving `replies`, one per request, in order. Requests beyond the script get a 500.
pub(crate) fn serve(replies: Vec<Reply>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let origin = format!("http://{}", listener.local_addr().expect("local addr"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let served_at = origin.clone();
    std::thread::spawn(move || {
        let mut replies = replies.into_iter();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            // A client that never finishes sending a request (a bug in a test, or in this server) must not hang the
            // suite waiting for bytes that are never coming; failing fast here is far shorter than the real client's
            // own 20-30 s request timeout, which would otherwise be what eventually ends it.
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request);
            let mut reply = replies
                .next()
                .unwrap_or_else(|| Reply::status(500).body("script exhausted"));
            if !reply.delay.is_zero() {
                std::thread::sleep(reply.delay);
            }
            if reply.hang_up {
                continue; // dropping the stream closes it with nothing written
            }
            reply.body = reply.body.replace("{origin}", &served_at);
            let payload = reply
                .raw
                .take()
                .unwrap_or_else(|| reply.body.clone().into_bytes());
            let mut head = format!(
                "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                reply.status,
                payload.len()
            );
            for (k, v) in &reply.headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("\r\n");
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&payload);
            let _ = stream.flush();
        }
    });
    Server { origin, seen }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Seen> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next()?.to_string(), parts.next()?.to_string());
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let (k, v) = h.split_once(':')?;
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    reader.read_exact(&mut body).ok()?;
    Some(Seen {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// A throwaway self-signed certificate for `127.0.0.1` (with that address as its subject alternative name, so the
/// only thing wrong with it is that nothing trusts its issuer) and its PKCS#8 key, made for these tests alone. Valid
/// for a century.
#[cfg(feature = "teams")]
const UNTRUSTED_CERT_DER_B64: &str = "MIIBbjCCARWgAwIBAgIUXZio62iEFvz6Ua/60wzqJf/BD9kwCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJMTI3LjAuMC4xMCAXDTI2MTExMzAwNDYyMVoYDzIxMjYxMDIwMDA0NjIxWjAUMRIwEAYDVQQDDAkxMjcuMC4wLjEwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASCbHeea1y3gvK4CK/8VCm7T+JLae0vhiLrKkBWIyrMnGBTwFcCq+G/EU4ZdMoegQ70aIV513+5+VIledrPCm+8o0MwQTAdBgNVHQ4EFgQUETrPIxjHuL4+R31i7O4fKucvVdkwDwYDVR0TAQH/BAUwAwEB/zAPBgNVHREECDAGhwR/AAABMAoGCCqGSM49BAMCA0cAMEQCIEurvARhmvzFBn7oJJHjRpmKMYbnETzdfZEcQCaiygT/AiBJgYCKj9d7ByFaC3ijgOmvEsP+twBDy6Rj2HbKpcKYKg==";
#[cfg(feature = "teams")]
const UNTRUSTED_KEY_DER_B64: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgnGLlZ4xBfT2LKgziP23aO+V5WdwW9ndlXifnduRxdz+hRANCAASCbHeea1y3gvK4CK/8VCm7T+JLae0vhiLrKkBWIyrMnGBTwFcCq+G/EU4ZdMoegQ70aIV513+5+VIledrPCm+8";

/// A TLS server on a loopback port presenting a certificate that no system trusts. The handle says whether any
/// application data ever arrived after the handshake: a client that distrusts the certificate ends the handshake with
/// an alert and never sends its request, so it should answer alse. For testing that TLS verification is on.
#[cfg(feature = "teams")]
pub(crate) fn untrusted_tls_server() -> (u16, std::thread::JoinHandle<bool>) {
    use base64::Engine;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let decode = |s: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .expect("the fixture is base64")
    };
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("protocol versions")
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(decode(UNTRUSTED_CERT_DER_B64))],
        PrivateKeyDer::try_from(decode(UNTRUSTED_KEY_DER_B64)).expect("a private key"),
    )
    .expect("a usable certificate");
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("an address").port();
    let handle = std::thread::spawn(move || {
        let Ok((tcp, _)) = listener.accept() else {
            return false;
        };
        let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let conn = rustls::ServerConnection::new(Arc::new(config)).expect("a server connection");
        let mut tls = rustls::StreamOwned::new(conn, tcp);
        let mut buf = [0u8; 64];
        matches!(tls.read(&mut buf), Ok(n) if n > 0)
    });
    (port, handle)
}
