//! Browser-assisted sign-in: launch Edge/Chrome with a private profile, let the user sign in to the Teams web app,
//! and read that page's `localStorage` over the Chrome DevTools Protocol (where the web app keeps its MSAL cache,
//! encrypted at rest by current builds and decrypted in the page: `READ_STORAGE`).
//!
//! The debugging port is bound to loopback by the browser and exists only while a capture is running (up to thirty
//! minutes for an interactive sign-in, about thirty seconds for a silent renewal). It has no authentication: any
//! process on the computer, including another user's, can connect to it and read the signed-in page's storage. A
//! caller should tell the user not to sign in on a computer they share. (The browser's pipe transport would avoid the
//! port, but handing it inherited file descriptors needs `unsafe`, which this workspace forbids.)

use crate::error::{AuthError, BrowserError, Error, Result};
use crate::http;
use serde_json::{Map, Value, json};
use std::cell::Cell;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

const START_URL: &str = "https://teams.microsoft.com/";
/// One DevTools command must be answered within this long, however many unrelated events arrive meanwhile.
const CALL_DEADLINE: Duration = Duration::from_secs(15);
const PAGE_HOSTS: [&str; 3] = [
    "teams.microsoft.com",
    "teams.cloud.microsoft",
    "teams.microsoft.us",
];

/// Reads the page's `localStorage`, decrypting the entries MSAL.js v4 encrypts at rest, in the page itself with the
/// browser's own WebCrypto. An encrypted entry is `{id, nonce, data}`; its key comes from the session cookie
/// `msal.cache.encryption` (`{id, key}`, URL-encoded JSON): HKDF-SHA256 over that key with the entry's nonce as salt
/// and, as `info`, the client id named in the entry's key (else empty), then AES-256-GCM with a zero IV. An entry that
/// is not encrypted, or does not decrypt, is returned as it is.
const READ_STORAGE: &str = r#"(async () => {
  const b64 = s => {
    s = s.replace(/-/g, "+").replace(/_/g, "/");
    while (s.length % 4) s += "=";
    return Uint8Array.from(atob(s), c => c.charCodeAt(0));
  };
  let base = null;
  try {
    const prefix = "msal.cache.encryption=";
    const c = document.cookie.split(";").map(c => c.trim()).find(c => c.startsWith(prefix));
    if (c) {
      const k = JSON.parse(decodeURIComponent(c.slice(prefix.length)));
      base = { id: k.id, key: await crypto.subtle.importKey("raw", b64(k.key), "HKDF", false, ["deriveKey"]) };
    }
  } catch (e) {}
  const storage = {};
  let encrypted = 0, decrypted = 0;
  for (let i = 0; i < localStorage.length; i++) {
    const name = localStorage.key(i);
    const value = localStorage.getItem(name);
    storage[name] = value;
    let w;
    try { w = JSON.parse(value); } catch (e) { continue; }
    if (!w || typeof w !== "object" || typeof w.data !== "string" || typeof w.nonce !== "string" || !w.id) continue;
    encrypted++;
    if (!base || base.id !== w.id) continue;
    const parts = name.split("|");
    for (const context of [parts.length > 4 ? parts[4] : "", ""]) {
      try {
        const key = await crypto.subtle.deriveKey(
          { name: "HKDF", salt: b64(w.nonce), hash: "SHA-256", info: new TextEncoder().encode(context) },
          base.key, { name: "AES-GCM", length: 256 }, false, ["decrypt"]);
        const plain = await crypto.subtle.decrypt({ name: "AES-GCM", iv: new Uint8Array(12) }, key, b64(w.data));
        storage[name] = new TextDecoder().decode(plain);
        decrypted++;
        break;
      } catch (e) {}
    }
  }
  return JSON.stringify({ storage, encrypted, decrypted });
})()"#;

/// Which browser to drive and where it keeps its profile.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BrowserOptions {
    /// The browser executable; `None` finds Edge, Chrome or Chromium.
    pub executable: Option<PathBuf>,
    /// A directory reserved for this client's browser profile. It holds the Microsoft single-sign-on cookies, which
    /// is what makes later silent renewal possible, so keep it private to the user.
    pub profile_dir: PathBuf,
}

impl BrowserOptions {
    /// Use the first browser found, with its profile in `profile_dir`.
    pub fn new(profile_dir: impl Into<PathBuf>) -> Self {
        Self {
            executable: None,
            profile_dir: profile_dir.into(),
        }
    }

    /// Use a specific browser executable.
    #[must_use]
    pub fn with_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = Some(executable.into());
        self
    }
}

/// The browser to drive: an explicit path, else the first Edge/Chrome/Chromium found.
pub(crate) fn find_browser(explicit: Option<&Path>) -> Result<PathBuf, BrowserError> {
    if let Some(p) = explicit.filter(|p| !p.as_os_str().is_empty()) {
        return if p.is_file() {
            Ok(p.to_path_buf())
        } else {
            Err(BrowserError::MissingExecutable(p.to_path_buf()))
        };
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    for var in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            let base = PathBuf::from(base);
            candidates.push(base.join(r"Microsoft\Edge\Application\msedge.exe"));
            candidates.push(base.join(r"Google\Chrome\Application\chrome.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    for app in ["Microsoft Edge", "Google Chrome", "Chromium"] {
        candidates.push(PathBuf::from(format!(
            "/Applications/{app}.app/Contents/MacOS/{app}"
        )));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    for name in [
        "microsoft-edge",
        "microsoft-edge-stable",
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
    ] {
        if let Some(path) = std::env::var_os("PATH") {
            // A relative or empty entry means "the current directory", where a planted file could be mistaken for a browser.
            candidates.extend(
                std::env::split_paths(&path)
                    .filter(|d| d.is_absolute())
                    .map(|d| d.join(name)),
            );
        }
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or(BrowserError::NotFound)
}

fn connection(
    action: &'static str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> BrowserError {
    BrowserError::Connection {
        action,
        source: Box::new(source),
    }
}

struct Cdp {
    ws: WebSocket<TcpStream>,
    next: u64,
}

impl Cdp {
    fn open(port: u16, path: &str) -> Result<Self, BrowserError> {
        let stream = TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_secs(5),
        )
        .map_err(|e| connection("connect", e))?;
        // The sockets are plain blocking ones; a timeout keeps a wedged browser from hanging the caller.
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| connection("configure", e))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| connection("configure", e))?;
        let (ws, _) = tungstenite::client::client(format!("ws://127.0.0.1:{port}{path}"), stream)
            .map_err(|e| connection("handshake", e))?;
        Ok(Self { ws, next: 0 })
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, BrowserError> {
        self.next += 1;
        let id = self.next;
        let deadline = Instant::now() + CALL_DEADLINE;
        self.ws
            .send(Message::text(
                json!({ "id": id, "method": method, "params": params }).to_string(),
            ))
            .map_err(|e| connection("send", e))?;
        loop {
            if Instant::now() > deadline {
                return Err(connection(
                    "read",
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("no answer to {method}"),
                    ),
                ));
            }
            match self.ws.read().map_err(|e| connection("read", e))? {
                Message::Text(t) => {
                    let v: Value =
                        serde_json::from_str(t.as_str()).map_err(|e| connection("read", e))?;
                    if v["id"].as_u64() == Some(id) {
                        return match v.get("error") {
                            Some(e) => Err(BrowserError::Protocol {
                                method: method.to_string(),
                                message: e["message"].as_str().unwrap_or("error").to_string(),
                            }),
                            None => Ok(v["result"].clone()),
                        };
                    }
                }
                Message::Close(_) => {
                    return Err(connection(
                        "read",
                        std::io::Error::new(
                            std::io::ErrorKind::ConnectionAborted,
                            "connection closed",
                        ),
                    ));
                }
                _ => {}
            }
        }
    }
}

pub(crate) struct Browser {
    child: Child,
    port: u16,
    browser_path: String,
    /// Used for the `/json/list` polls, which go to this computer and never through a proxy.
    agent: ureq::Agent,
    /// What the last storage read found (entries, encrypted, decrypted), so the log notes only changes.
    last_counts: Cell<Option<(usize, u64, u64)>>,
}

impl Browser {
    /// Start the browser on the Teams web app with a persistent profile (so SSO survives between runs). A browser that
    /// exits at once is tried once more after a moment: one that was shutting down still held the profile.
    pub(crate) fn launch(exe: &Path, profile: &Path, headless: bool) -> Result<Self, BrowserError> {
        match Self::launch_once(exe, profile, headless) {
            Err(BrowserError::ExitedAtOnce { .. }) => {
                log::warn!("the browser exited at once; trying once more");
                std::thread::sleep(Duration::from_secs(2));
                Self::launch_once(exe, profile, headless)
            }
            other => other,
        }
    }

    fn launch_once(exe: &Path, profile: &Path, headless: bool) -> Result<Self, BrowserError> {
        crate::store::create_private_dir(profile).map_err(|source| BrowserError::Profile {
            path: profile.to_path_buf(),
            source,
        })?;
        let port_file = profile.join("DevToolsActivePort");
        close_leftover(profile);
        // What is left names a port nobody listens on, and would be trusted as the new browser's own if it stayed.
        if let Err(source) = std::fs::remove_file(&port_file)
            && source.kind() != std::io::ErrorKind::NotFound
        {
            return Err(BrowserError::Profile {
                path: port_file,
                source,
            });
        }
        let mut cmd = Command::new(exe);
        cmd.arg(format!("--user-data-dir={}", profile.display()))
            .args([
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
                // Without this, Teams' own sign-in can finish (the page shows a fully signed-in chat UI) while
                // `window.localStorage` of that same top-level page stays empty, confirmed live, 2026-11-09, on
                // Chromium on Linux: Chrome's own third-party storage partitioning isolates whatever cross-origin
                // step actually commits the MSAL cache from the top-level page `teams_storage` reads from. Not
                // needed on Edge (its default behaviour already let a real sign-in populate `localStorage`
                // normally), but harmless there and on Chrome too: an unknown or already-disabled feature name is
                // silently ignored rather than rejected.
                "--disable-features=ThirdPartyStoragePartitioning",
            ]);
        if headless {
            cmd.arg("--headless=new");
        }
        cmd.arg(START_URL)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(|source| BrowserError::Launch {
            path: exe.to_path_buf(),
            source,
        })?;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(BrowserError::ExitedAtOnce {
                    status,
                    profile: profile.to_path_buf(),
                });
            }
            if let Some((port, path)) = std::fs::read_to_string(&port_file)
                .ok()
                .and_then(|t| parse_port_file(&t))
            {
                return Ok(Self {
                    child,
                    port,
                    browser_path: path,
                    agent: http::local_agent(Duration::from_secs(5)),
                    last_counts: Cell::new(None),
                });
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        stop(&mut child);
        Err(BrowserError::NoDebugPort)
    }

    /// One look at the browser: has it gone, is there still a window, and does a Teams page have anything to read.
    /// Callers poll.
    pub(crate) fn poll(&mut self) -> Poll {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return Poll::Exited;
        }
        look(&self.agent, self.port, &self.last_counts)
    }

    /// Ask the browser to quit (so the profile is flushed to disk), then make sure it is gone.
    pub(crate) fn close(mut self) {
        if let Ok(mut cdp) = Cdp::open(self.port, &self.browser_path) {
            // The browser may drop the connection as it exits, so the answer is not interesting.
            let _ = cdp.call("Browser.close", json!({}));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        stop(&mut self.child);
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            stop(&mut self.child);
        }
    }
}

/// What one look at a browser found (see [`Browser::poll`]).
pub(crate) enum Poll {
    /// A Teams page with a `localStorage` to read, and that page's origin.
    Storage(String, Map<String, Value>),
    /// The browser is there but there is nothing to read yet: the page is still loading or still at the sign-in
    /// screen, or the browser did not answer this time.
    Waiting,
    /// The browser answered and has no window open at all.
    NoWindow,
    /// The browser process has ended.
    Exited,
}

/// Look at the browser on `port`: its tabs (`/json/list`), then the `localStorage` of the first Teams page that has any.
fn look(agent: &ureq::Agent, port: u16, last_counts: &Cell<Option<(usize, u64, u64)>>) -> Poll {
    const MAX_DEVTOOLS_LIST_BYTES: u64 = 1024 * 1024;
    let Some(targets) = agent
        .get(&format!("http://127.0.0.1:{port}/json/list"))
        .call()
        .ok()
        .and_then(|mut resp| {
            crate::http::body_json::<Value>("list browser tabs", &mut resp, MAX_DEVTOOLS_LIST_BYTES)
                .ok()
        })
    else {
        return Poll::Waiting;
    };
    let Some(targets) = targets.as_array() else {
        return Poll::Waiting;
    };
    if !targets.iter().any(|t| t["type"].as_str() == Some("page")) {
        return Poll::NoWindow;
    }
    for t in targets {
        let Ok(page) = url::Url::parse(t["url"].as_str().unwrap_or("")) else {
            continue;
        };
        if t["type"].as_str() != Some("page")
            || page.scheme() != "https"
            || !PAGE_HOSTS.contains(&page.host_str().unwrap_or(""))
        {
            continue;
        }
        let Some(path) = t["webSocketDebuggerUrl"]
            .as_str()
            .and_then(|u| u.split_once(&format!("127.0.0.1:{port}")))
            .map(|(_, p)| p.to_string())
        else {
            continue;
        };
        let Ok(mut cdp) = Cdp::open(port, &path) else {
            continue;
        };
        let Ok(r) = cdp.call(
            "Runtime.evaluate",
            json!({ "expression": READ_STORAGE, "returnByValue": true, "awaitPromise": true }),
        ) else {
            continue;
        };
        let Some(read) = r["result"]["value"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
        else {
            continue;
        };
        let counts = (
            read["storage"].as_object().map_or(0, Map::len),
            read["encrypted"].as_u64().unwrap_or(0),
            read["decrypted"].as_u64().unwrap_or(0),
        );
        if last_counts.replace(Some(counts)) != Some(counts) {
            log::debug!(
                "sign-in page storage: {} entries, {} encrypted, {} decrypted",
                counts.0,
                counts.1,
                counts.2
            );
        }
        if let Some(map) = read["storage"].as_object().filter(|m| !m.is_empty()) {
            return Poll::Storage(
                format!("https://{}", page.host_str().unwrap_or("")),
                map.clone(),
            );
        }
    }
    Poll::Waiting
}

/// A browser from an earlier run may still be open on the profile: vtteams was killed (its window closed, say) while one was
/// running, and the browser carries on without it. It holds the profile, so the new one would exit at once ("profile in
/// use"). Ask it to quit through the debugging port it wrote into the profile, and wait for it to go. Returns whether
/// there was one to ask.
fn close_leftover(profile: &Path) -> bool {
    let Some((port, path)) = std::fs::read_to_string(profile.join("DevToolsActivePort"))
        .ok()
        .and_then(|t| parse_port_file(&t))
    else {
        return false;
    };
    let Ok(mut cdp) = Cdp::open(port, &path) else {
        return false; // nothing listens there any more
    };
    log::warn!("a browser from an earlier run is still open on the profile; asking it to quit");
    // It may drop the connection as it exits, so the answer is not interesting.
    let _ = cdp.call("Browser.close", json!({}));
    drop(cdp);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(300),
        )
        .is_err()
        {
            // The port is closed; give it a moment to let go of the profile.
            std::thread::sleep(Duration::from_millis(500));
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    true
}

/// Kill the browser and collect its exit status, so no zombie process is left behind. Failures mean it is already gone.
fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// `DevToolsActivePort` is two lines: the port, then the browser websocket path.
fn parse_port_file(text: &str) -> Option<(u16, String)> {
    let mut lines = text.lines();
    let port = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    path.starts_with("/devtools/browser/")
        .then(|| (port, path.to_string()))
}

/// How many polls in a row may find the browser running with no window at all before the sign-in is given up. A
/// browser that has only just started can show none for a moment; one whose last window was closed shows none for good.
const NO_WINDOW_POLLS: u32 = 10;

/// Poll every `every` until `ready` accepts a Teams page's localStorage, `timeout` passes
/// ([`AuthError::TimedOut`]), or the browser is gone ([`BrowserError::Closed`]): it has exited, or has had no window
/// for [`NO_WINDOW_POLLS`] polls in a row. Someone who closes the sign-in window is not left waiting out the timeout,
/// with every other request failing meanwhile.
fn wait_for_sign_in<T>(
    timeout: Duration,
    every: Duration,
    mut poll: impl FnMut() -> Poll,
    mut ready: impl FnMut(&str, &Map<String, Value>) -> Option<T>,
) -> Result<T> {
    let deadline = Instant::now() + timeout;
    let mut without_window = 0;
    while Instant::now() < deadline {
        match poll() {
            Poll::Storage(origin, storage) => {
                without_window = 0;
                if let Some(found) = ready(&origin, &storage) {
                    return Ok(found);
                }
            }
            Poll::Waiting => without_window = 0,
            Poll::NoWindow => {
                without_window += 1;
                if without_window >= NO_WINDOW_POLLS {
                    return Err(BrowserError::Closed.into());
                }
            }
            Poll::Exited => return Err(BrowserError::Closed.into()),
        }
        std::thread::sleep(every);
    }
    Err(Error::Auth(AuthError::TimedOut))
}

/// Run the browser until `ready` accepts the Teams page's localStorage (or `timeout` passes, or the browser is closed
/// by whoever is signing in), then close it.
pub(crate) fn capture<T>(
    options: &BrowserOptions,
    headless: bool,
    timeout: Duration,
    ready: impl FnMut(&str, &Map<String, Value>) -> Option<T>,
) -> Result<T> {
    let exe = find_browser(options.executable.as_deref())?;
    let mut browser = Browser::launch(&exe, &options.profile_dir, headless)?;
    let result = wait_for_sign_in(timeout, Duration::from_secs(1), || browser.poll(), ready);
    browser.close();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_devtools_port_file() {
        assert_eq!(
            parse_port_file("51234\n/devtools/browser/abc-123\n"),
            Some((51234, "/devtools/browser/abc-123".into()))
        );
        assert_eq!(parse_port_file("51234\n"), None);
        assert_eq!(parse_port_file("x\n/devtools/browser/a"), None);
    }

    /// A stand-in for a browser left open: accepts one DevTools connection, notes the commands it gets, and when it
    /// is told to close stops listening, as a browser that quits does.
    fn leftover_browser() -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let mut commands = Vec::new();
            while let Ok(Message::Text(t)) = ws.read() {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                commands.push(v["method"].as_str().unwrap_or("").to_string());
                let _ = ws.send(Message::text(
                    json!({"id": v["id"], "result": {}}).to_string(),
                ));
                if v["method"] == "Browser.close" {
                    break;
                }
            }
            drop(ws);
            drop(listener); // the port closes: the browser is gone
            commands
        });
        (port, handle)
    }

    fn profile_with_port_file(name: &str, content: &str) -> crate::testdir::TestDir {
        let dir = crate::testdir::TestDir::new(name);
        std::fs::write(dir.join("DevToolsActivePort"), content).unwrap();
        dir
    }

    #[test]
    fn a_browser_left_open_on_the_profile_is_asked_to_quit_before_a_new_one_starts() {
        let (port, server) = leftover_browser();
        let profile =
            profile_with_port_file("leftover", &format!("{port}\n/devtools/browser/abc-123\n"));
        assert!(close_leftover(&profile));
        assert_eq!(server.join().unwrap(), ["Browser.close"]);
    }

    #[test]
    fn nothing_is_asked_when_no_browser_answers_or_the_file_is_not_a_browser_port() {
        // A port file whose browser is long gone, one that is not a port file, and none at all.
        let gone = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        for content in [
            format!("{gone}\n/devtools/browser/abc\n"),
            "garbage".to_string(),
            "12345\n/somewhere/else\n".to_string(),
        ] {
            let profile = profile_with_port_file("nobody", &content);
            assert!(!close_leftover(&profile), "{content:?}");
        }
        assert!(!close_leftover(Path::new("no-such-profile-directory")));
    }

    /// A stand-in for a browser's DevTools port. It answers `/json/list` with `targets(port)`, and every command on a
    /// page's socket with `evaluated` as the result of `Runtime.evaluate`. It serves `connections` connections.
    fn fake_devtools(
        targets: impl Fn(u16) -> Value + Send + 'static,
        evaluated: Option<String>,
        connections: usize,
    ) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for _ in 0..connections {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut head = [0u8; 16];
                let mut seen = 0;
                for _ in 0..100 {
                    seen = stream.peek(&mut head).unwrap_or(0);
                    if seen == head.len() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                if head[..seen].starts_with(b"GET /json/list") {
                    let mut request = [0u8; 4096];
                    let _ = stream.read(&mut request);
                    let body = targets(port).to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                } else if let Ok(mut ws) = tungstenite::accept(stream) {
                    while let Ok(Message::Text(t)) = ws.read() {
                        let v: Value = serde_json::from_str(t.as_str()).unwrap();
                        let reply =
                            json!({"id": v["id"], "result": {"result": {"value": evaluated}}});
                        let _ = ws.send(Message::text(reply.to_string()));
                    }
                }
            }
        });
        port
    }

    fn teams_page(port: u16) -> Value {
        json!([
            {"type": "service_worker", "url": "https://teams.cloud.microsoft/serviceworker.js"},
            {"type": "page", "url": "https://teams.cloud.microsoft/",
             "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}/devtools/page/ABC")}
        ])
    }

    fn look_at(port: u16) -> Poll {
        look(
            &http::local_agent(Duration::from_secs(5)),
            port,
            &Cell::new(None),
        )
    }

    #[test]
    fn a_teams_page_with_storage_is_read_through_devtools() {
        let value =
            json!({"storage": {"key": "value"}, "encrypted": 2, "decrypted": 2}).to_string();
        let port = fake_devtools(teams_page, Some(value), 2);
        match look_at(port) {
            Poll::Storage(origin, storage) => {
                assert_eq!(origin, "https://teams.cloud.microsoft");
                assert_eq!(storage["key"], "value");
            }
            _ => panic!("expected the storage to be read"),
        }
    }

    #[test]
    fn a_page_with_no_storage_yet_or_an_unreadable_answer_is_just_waiting() {
        let empty = json!({"storage": {}, "encrypted": 0, "decrypted": 0}).to_string();
        let port = fake_devtools(teams_page, Some(empty), 2);
        assert!(matches!(look_at(port), Poll::Waiting), "nothing stored yet");
        let port = fake_devtools(teams_page, Some("not json".into()), 2);
        assert!(
            matches!(look_at(port), Poll::Waiting),
            "an unreadable result"
        );
        let port = fake_devtools(
            |port| {
                json!([{"type": "page", "url": "https://login.microsoftonline.com/x",
                        "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}/devtools/page/L")}])
            },
            None,
            1,
        );
        assert!(
            matches!(look_at(port), Poll::Waiting),
            "a window still at the sign-in screen is not a Teams page, and not a closed window either"
        );
    }

    #[test]
    fn a_browser_with_no_window_open_is_told_apart_from_one_that_is_loading() {
        let port = fake_devtools(
            |_| json!([{"type": "service_worker", "url": "https://teams.cloud.microsoft/serviceworker.js"}]),
            None,
            1,
        );
        assert!(matches!(look_at(port), Poll::NoWindow));
    }

    #[test]
    fn a_browser_that_does_not_answer_is_waiting_not_closed() {
        let gone = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        assert!(matches!(look_at(gone), Poll::Waiting));
    }

    fn quick<T>(
        timeout_ms: u64,
        poll: impl FnMut() -> Poll,
        ready: impl FnMut(&str, &Map<String, Value>) -> Option<T>,
    ) -> Result<T> {
        wait_for_sign_in(
            Duration::from_millis(timeout_ms),
            Duration::from_millis(1),
            poll,
            ready,
        )
    }

    fn is_closed(e: &Error) -> bool {
        matches!(e, Error::Browser(BrowserError::Closed))
    }

    #[test]
    fn closing_the_sign_in_window_ends_the_wait_instead_of_running_out_the_clock() {
        let started = Instant::now();
        let err = quick(60_000, || Poll::Exited, |_, _| None::<()>).unwrap_err();
        assert!(is_closed(&err), "{err:?}");
        let err = quick(60_000, || Poll::NoWindow, |_, _| None::<()>).unwrap_err();
        assert!(is_closed(&err), "{err:?}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "neither waited for the one-minute timeout"
        );
    }

    #[test]
    fn a_brief_moment_without_a_window_does_not_end_the_wait() {
        let mut polls = 0;
        let found = quick(
            60_000,
            || {
                polls += 1;
                // Nearly enough empty polls to give up, a poll that finds the browser busy (which resets the
                // count), nearly enough again, then the page.
                match polls {
                    n if n < NO_WINDOW_POLLS => Poll::NoWindow,
                    n if n == NO_WINDOW_POLLS => Poll::Waiting,
                    n if n < 2 * NO_WINDOW_POLLS => Poll::NoWindow,
                    _ => Poll::Storage("https://teams.cloud.microsoft".into(), Map::new()),
                }
            },
            |origin, _| Some(origin.to_string()),
        )
        .unwrap();
        assert_eq!(found, "https://teams.cloud.microsoft");
    }

    #[test]
    fn a_wait_with_nothing_to_find_times_out() {
        let err = quick(30, || Poll::Waiting, |_, _| None::<()>).unwrap_err();
        assert!(matches!(err, Error::Auth(AuthError::TimedOut)), "{err:?}");
    }

    #[test]
    fn a_stale_port_file_that_cannot_be_removed_is_an_error_not_trusted() {
        // A directory in the place of the file cannot be removed with `remove_file`.
        let profile = crate::testdir::TestDir::new("stale");
        std::fs::create_dir_all(profile.join("DevToolsActivePort")).unwrap();
        let err = Browser::launch_once(Path::new("no-such-browser"), &profile, true).err();
        assert!(matches!(err, Some(BrowserError::Profile { .. })), "{err:?}");
    }

    /// The in-page script that decrypts the MSAL cache, run under Node (which has the same WebCrypto) against known
    /// answers produced by a separate implementation (Node's `crypto` module: HKDF-SHA256 with the entry's nonce as
    /// salt and its client id or nothing as info, AES-256-GCM with a zero IV). Skipped where Node is not installed.
    #[test]
    fn the_in_page_script_decrypts_what_msal_encrypts_and_leaves_everything_else_alone() {
        const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
        const CLIENT: &str = "11111111-2222-3333-4444-555555555555";
        const PLAIN_1: &str = r#"{"credentialType":"RefreshToken","secret":"not-a-real-token","clientId":"11111111-2222-3333-4444-555555555555","homeAccountId":"oid-0001.tenant-0001"}"#;
        const PLAIN_2: &str = r#"{"credentialType":"IdToken","secret":"also-not-real","clientId":"11111111-2222-3333-4444-555555555555","homeAccountId":"oid-0001.tenant-0001"}"#;
        const NONCE_1: &str = "oKGio6SlpqeoqaqrrK2urw";
        const DATA_1: &str = "OufHCk0ISCFvXlFnZKHhwNRCI80a_jmwBhLdmac2O-fsxNIAuKqT6lpL2XricdH-47-F-kRfML_uYCDlw7K53WL9i-enCbzU0GOkaiYZISkzpv_taN4_WHwjBTZHOKK567LVBasAca9Z-eZzDBRdhUXhG8NucOAznh9n8MBAb6WCpphBynkC0D0976OJNIFlgC8ze2Ut3haQMn6k2lqQZJUYcCFjpA";
        const NONCE_2: &str = "sLGys7S1tre4ubq7vL2-vw";
        const DATA_2: &str = "1ZOzj5Wx5CIP3NwuXpUbmMxskz9GVY0xXiXg98HpPWhFaCUgX1FkKcK0jzLZm__DeKhyMDW0YkJE4_0ewKuF2ZWsthpaG924vT11Lj3wlHXwKpMzZsMnU_GrOXoWTpFAf2ICCFcr8-K3wyy7vVAcS4GixoRLZY4Kz5ovIS2GbrKWcg945Hw8Eff1-gAQjaSB60XWRGnbKw0_-G5sQSw";

        let node_runs = Command::new("node")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !node_runs {
            // The check cannot run without Node. CI always has it, and there a missing Node is a failure rather
            // than a silent pass.
            assert!(
                std::env::var_os("CI").is_none(),
                "node is required in CI for this test"
            );
            return;
        }
        let entry = |kind: &str| {
            format!("msal.2|oid-0001.tenant-0001|login.windows.net|{kind}|{CLIENT}|||")
        };
        let wrapper = |id: &str, nonce: &str, data: &str| {
            json!({"id": id, "nonce": nonce, "data": data, "lastUpdatedAt": "1"}).to_string()
        };
        let mut tampered = DATA_1.to_string();
        tampered.replace_range(10..11, if &tampered[10..11] == "A" { "B" } else { "A" });
        let items = json!({
            entry("refreshtoken"): wrapper("the-id", NONCE_1, DATA_1),
            entry("idtoken"): wrapper("the-id", NONCE_2, DATA_2),
            entry("accesstoken"): wrapper("another-cookie", NONCE_1, DATA_1),
            entry("tampered"): wrapper("the-id", NONCE_1, &tampered),
            "plain": r#"{"credentialType":"AccessToken"}"#,
            "not-json": "hello",
        });
        let cookie = format!(
            "other=1; msal.cache.encryption={}; last=2",
            // URL-encoded the way the page stores it.
            json!({"id": "the-id", "key": KEY})
                .to_string()
                .replace('"', "%22")
                .replace(':', "%3A")
                .replace(',', "%2C")
        );
        // `defineProperty`, not assignment: recent Node versions have a `localStorage` global of their own.
        let harness = format!(
            "const SCRIPT = {script};\nconst items = {items};\n\
             Object.defineProperty(globalThis, 'document', {{ value: {{ cookie: {cookie} }}, configurable: true }});\n\
             Object.defineProperty(globalThis, 'localStorage', {{ configurable: true, value: {{ \
             get length() {{ return Object.keys(items).length; }}, key: (i) => Object.keys(items)[i], \
             getItem: (k) => (k in items ? items[k] : null) }} }});\n\
             (async () => console.log(await eval(SCRIPT)))();\n",
            script = serde_json::to_string(READ_STORAGE).unwrap(),
            cookie = serde_json::to_string(&cookie).unwrap(),
            items = items,
        );
        let dir = crate::testdir::TestDir::new("read-storage");
        let path = dir.join("script.js");
        std::fs::write(&path, harness).unwrap();
        let out = Command::new("node").arg(&path).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let result: Value =
            serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
        let storage = &result["storage"];
        assert_eq!(
            storage[entry("refreshtoken")],
            PLAIN_1,
            "decrypted with the client id as context"
        );
        assert_eq!(
            storage[entry("idtoken")],
            PLAIN_2,
            "decrypted with no context"
        );
        assert_eq!(
            storage[entry("accesstoken")],
            wrapper("another-cookie", NONCE_1, DATA_1),
            "an entry written under a different cookie is left as it was"
        );
        assert_eq!(
            storage[entry("tampered")],
            wrapper("the-id", NONCE_1, &tampered),
            "so is one that fails to decrypt"
        );
        assert_eq!(
            storage["plain"], r#"{"credentialType":"AccessToken"}"#,
            "an unencrypted entry passes through"
        );
        assert_eq!(storage["not-json"], "hello");
        assert_eq!(result["encrypted"], 4);
        assert_eq!(result["decrypted"], 2);
    }

    #[test]
    fn a_missing_explicit_browser_is_a_typed_error() {
        let err = find_browser(Some(Path::new("definitely/not/a/browser.exe"))).unwrap_err();
        assert!(matches!(err, BrowserError::MissingExecutable(_)));
    }
}
