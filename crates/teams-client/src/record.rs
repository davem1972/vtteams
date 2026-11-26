//! An opt-in recorder of the *shape* of every API exchange, for working out what a real service sends without
//! recording anything personal.
//!
//! A recorded exchange keeps the operation, method, status, the structure of the request and response JSON, and the
//! URL path with its identifiers masked. Strings are reduced to character classes (digits `9`, lower case `a`, upper
//! case `A`, punctuation kept), so a chat id like `19:ab12@thread.v2` becomes `99:aa99@aaaaaa.a9`: formats and lengths
//! are visible, content is not. Values under keys that look like secrets (`token`, `secret`, `password`, ...) are
//! replaced outright. Attach a recorder with the backends' `with_recorder`, then share the directory it wrote.

use crate::error::StoreError;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Strings longer than this are cut (their original length is noted).
const MAX_SHAPE_STRING: usize = 120;
/// Arrays keep this many masked elements and note how many more there were.
const MAX_SHAPE_ITEMS: usize = 3;

/// One request/response pair, as the HTTP layer saw it.
pub(crate) struct Exchange<'a> {
    pub(crate) operation: &'static str,
    pub(crate) method: &'a str,
    pub(crate) url: &'a str,
    /// `None` when no answer arrived.
    pub(crate) status: Option<u16>,
    pub(crate) request: Option<&'a Value>,
    /// The response body as text.
    pub(crate) response: &'a str,
}

/// Writes one JSON file per exchange into a directory. Share the directory to show what a service really sends.
#[derive(Debug)]
pub struct ShapeRecorder {
    dir: PathBuf,
    next: AtomicUsize,
    failure: Mutex<Option<StoreError>>,
}

impl ShapeRecorder {
    /// A recorder writing into `dir`, which is created if needed.
    ///
    /// # Errors
    /// [`StoreError`] if the directory cannot be created.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let dir = dir.into();
        crate::store::create_private_dir(&dir).map_err(|source| StoreError::Io {
            action: "create",
            path: dir.clone(),
            source,
        })?;
        // Numbering continues after what an earlier run left in the directory; files are never overwritten.
        let used = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()?
                    .split_once('-')?
                    .0
                    .parse::<usize>()
                    .ok()
            })
            .max()
            .unwrap_or(0);
        Ok(Self {
            dir,
            next: AtomicUsize::new(used + 1),
            failure: Mutex::new(None),
        })
    }

    /// The directory being written.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The first failure to write a file since the last call, if any (recording never fails an API call).
    pub fn take_failure(&self) -> Option<StoreError> {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    pub(crate) fn record(&self, e: &Exchange<'_>) {
        let (path, query) = split_url(e.url);
        let response = serde_json::from_str::<Value>(e.response).map_or_else(
            |_| Value::String(mask_string(e.response)),
            |v| mask_value(&v),
        );
        let doc = json!({
            "operation": e.operation,
            "method": e.method,
            "host": host_of(e.url),
            "path": mask_path(path),
            "query_keys": query.split('&').filter(|p| !p.is_empty()).map(|p| p.split('=').next().unwrap_or("")).collect::<Vec<_>>(),
            "status": e.status,
            "request": e.request.map(mask_value),
            "response": response,
        });
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let name: String = e
            .operation
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let file = self.dir.join(format!("{n:03}-{name}.json"));
        // The directory is made again if it has disappeared since the recorder started (cleaned up, or on a drive that
        // was reconnected), so a recording in progress is not lost.
        let written = serde_json::to_vec_pretty(&doc)
            .map_err(|err| StoreError::Encode {
                what: "a recorded exchange",
                source: Box::new(err),
            })
            .and_then(|bytes| {
                crate::store::create_private_dir(&self.dir).map_err(|source| StoreError::Io {
                    action: "create",
                    path: self.dir.clone(),
                    source,
                })?;
                write_new(&file, &bytes).map_err(|source| StoreError::Io {
                    action: "write",
                    path: file.clone(),
                    source,
                })
            });
        if let Err(err) = written {
            let mut slot = self
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.get_or_insert(err);
        }
    }
}

/// Create `path`, failing if it exists (so an existing file, or a link planted in a shared directory, is never written
/// through), readable by the current user only on Unix.
fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

fn split_url(url: &str) -> (&str, &str) {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path_and_query = after_scheme.find('/').map_or("", |i| &after_scheme[i..]);
    path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""))
}

fn host_of(url: &str) -> &str {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    after_scheme.split('/').next().unwrap_or("")
}

/// Keep path words (`v1`, `users`, `ME`, `messages`) and mask everything else, which is an identifier.
fn mask_path(path: &str) -> String {
    path.split('/')
        .map(|seg| {
            if is_word(seg) {
                seg.to_string()
            } else {
                mask_string(seg)
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A word: letters, optionally one trailing digit (`v1`).
fn is_word(s: &str) -> bool {
    let body = s.strip_suffix(|c: char| c.is_ascii_digit()).unwrap_or(s);
    !body.is_empty() && body.chars().all(|c| c.is_ascii_alphabetic()) && s.len() <= 24
}

/// Reduce a string to its character classes, keeping ASCII punctuation. JSON embedded in a string is masked
/// structurally (so its field names survive) rather than character by character.
pub(crate) fn mask_string(s: &str) -> String {
    let trimmed = s.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(v) = serde_json::from_str::<Value>(s)
    {
        return format!("<json {}>", mask_value(&v));
    }
    let mut out: String = s
        .chars()
        .take(MAX_SHAPE_STRING)
        .map(|c| match c {
            '0'..='9' => '9',
            'a'..='z' => 'a',
            'A'..='Z' => 'A',
            c if c.is_whitespace() => ' ',
            c if c.is_ascii_punctuation() => c,
            _ => 'x',
        })
        .collect();
    if s.chars().count() > MAX_SHAPE_STRING {
        out.push_str(&format!("…(length {})", s.chars().count()));
    }
    out
}

fn secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "authorization",
        "cookie",
        "credential",
    ]
    .iter()
    .any(|w| k.contains(w))
}

/// A key worth keeping: a plain field name. Anything with long digit runs or odd characters is data used as a key.
fn plain_key(key: &str) -> bool {
    key.len() <= 40
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@' | '$'))
        && key.chars().filter(char::is_ascii_digit).count() <= 2
}

/// The structure of `v` with its content masked.
pub(crate) fn mask_value(v: &Value) -> Value {
    match v {
        Value::Null | Value::Bool(_) => v.clone(),
        Value::Number(n) => Value::String(format!("<number, {} chars>", n.to_string().len())),
        Value::String(s) => Value::String(mask_string(s)),
        Value::Array(items) => {
            let mut out: Vec<Value> = items.iter().take(MAX_SHAPE_ITEMS).map(mask_value).collect();
            if items.len() > MAX_SHAPE_ITEMS {
                out.push(Value::String(format!(
                    "<{} more>",
                    items.len() - MAX_SHAPE_ITEMS
                )));
            }
            Value::Array(out)
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, val) in map {
                let key = if plain_key(k) {
                    k.clone()
                } else {
                    mask_string(k)
                };
                let masked = if secret_key(k) {
                    Value::String(format!("<redacted, {} chars>", val.to_string().len()))
                } else {
                    mask_value(val)
                };
                out.insert(key, masked);
            }
            Value::Object(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_keep_their_format_but_not_their_content() {
        assert_eq!(mask_string("19:ab12@thread.v2"), "99:aa99@aaaaaa.a9");
        assert_eq!(mask_string("Ada Lovelace"), "Aaa Aaaaaaaa");
        assert_eq!(mask_string("é"), "x");
        let long = "a".repeat(500);
        assert!(
            mask_string(&long).ends_with("(length 500)")
                && mask_string(&long).chars().count() < 160
        );
    }

    #[test]
    fn json_inside_a_string_keeps_its_field_names() {
        let masked = mask_string(r#"[{"key":"1f37b_beer","users":[{"mri":"8:orgid:abc"}]}]"#);
        assert!(
            masked.starts_with("<json ")
                && masked.contains("\"key\"")
                && masked.contains("\"mri\""),
            "{masked}"
        );
        assert!(
            !masked.contains("beer") && !masked.contains("orgid:abc"),
            "{masked}"
        );
    }

    #[test]
    fn values_have_their_structure_and_no_content() {
        let v = json!({"id": "1793095260000", "from": "8:orgid:0a1b", "content": "<p>secret plans</p>", "n": 12, "ok": true,
                       "items": [1, 2, 3, 4, 5], "access_token": "eyJabc.def.ghi"});
        let m = mask_value(&v);
        let text = m.to_string();
        assert!(
            !text.contains("plans") && !text.contains("eyJ") && !text.contains("0a1b"),
            "{text}"
        );
        assert_eq!(m["id"], "9999999999999");
        assert_eq!(m["ok"], true);
        assert_eq!(
            m["items"].as_array().unwrap().len(),
            4,
            "three kept plus a count"
        );
        assert!(m["access_token"].as_str().unwrap().starts_with("<redacted"));
        assert!(m["n"].as_str().unwrap().starts_with("<number"));
    }

    #[test]
    fn data_used_as_object_keys_is_masked_but_field_names_are_not() {
        let m = mask_value(&json!({"messages": {"1793095260000": 1}, "backwardLink": "x"}));
        let keys: Vec<&str> = m.as_object().unwrap().keys().map(String::as_str).collect();
        assert!(keys.contains(&"backwardLink") && keys.contains(&"messages"));
        assert!(
            m["messages"]
                .as_object()
                .unwrap()
                .keys()
                .all(|k| !k.contains("1793")),
            "{m}"
        );
    }

    #[test]
    fn paths_keep_words_and_mask_identifiers() {
        assert_eq!(
            mask_path("/api/chatsvc/amer/v1/users/ME/conversations/19%3Aab12@thread.v2/messages"),
            "/api/chatsvc/amer/v1/users/ME/conversations/99%9Aaa99@aaaaaa.a9/messages"
        );
    }

    #[test]
    fn a_recorder_writes_one_masked_file_per_exchange() {
        let dir = crate::testdir::TestDir::new("shapes");
        let rec = ShapeRecorder::new(&*dir).unwrap();
        let body = json!({"content": "hello there"});
        rec.record(&Exchange {
            operation: "send message",
            method: "POST",
            url: "https://teams.microsoft.com/api/chatsvc/amer/v1/users/ME/conversations/19:abc@thread.v2/messages?view=x&pageSize=50",
            status: Some(200),
            request: Some(&body),
            response: r#"{"OriginalArrivalTime": 1793095260000}"#,
        });
        let file = std::fs::read_dir(&dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let doc: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(
            (doc["status"].as_u64(), doc["method"].as_str()),
            (Some(200), Some("POST"))
        );
        assert_eq!(doc["query_keys"], json!(["view", "pageSize"]));
        let text = doc.to_string();
        assert!(
            !text.contains("hello") && !text.contains("abc@") && !text.contains("1793095260000"),
            "{text}"
        );
        assert!(rec.take_failure().is_none());
    }

    fn one_exchange(rec: &ShapeRecorder) {
        rec.record(&Exchange {
            operation: "list chats",
            method: "GET",
            url: "https://teams.microsoft.com/x",
            status: Some(200),
            request: None,
            response: "{}",
        });
    }

    #[test]
    fn a_later_run_continues_the_numbering_and_never_overwrites_a_file() {
        let dir = crate::testdir::TestDir::new("shapes-again");
        let first = ShapeRecorder::new(&*dir).unwrap();
        one_exchange(&first);
        let before = std::fs::read(dir.join("001-list-chats.json")).unwrap();
        let second = ShapeRecorder::new(&*dir).unwrap();
        one_exchange(&second);
        assert!(dir.join("002-list-chats.json").exists());
        assert_eq!(
            std::fs::read(dir.join("001-list-chats.json")).unwrap(),
            before
        );
        // A file that appears under the next name (another process) is refused, not replaced.
        std::fs::write(dir.join("003-list-chats.json"), "mine").unwrap();
        one_exchange(&second);
        assert_eq!(
            std::fs::read_to_string(dir.join("003-list-chats.json")).unwrap(),
            "mine"
        );
        assert!(second.take_failure().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_the_directory_is_not_written_through_and_files_are_private() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = crate::testdir::TestDir::new("shapes-link");
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "keep").unwrap();
        symlink(&victim, dir.join("001-list-chats.json")).unwrap();
        let rec = ShapeRecorder::new(&*dir).unwrap();
        one_exchange(&rec);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        let mode = std::fs::metadata(dir.join("002-list-chats.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
    #[test]
    fn a_directory_removed_while_recording_is_made_again() {
        let dir = crate::testdir::TestDir::new("shapes-gone");
        let rec = ShapeRecorder::new(&*dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        rec.record(&Exchange {
            operation: "list chats",
            method: "GET",
            url: "https://teams.microsoft.com/api/x",
            status: Some(200),
            request: None,
            response: "{}",
        });
        assert!(rec.take_failure().is_none());
        assert_eq!(std::fs::read_dir(&*dir).unwrap().count(), 1);
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn masked_strings_contain_only_character_classes_and_punctuation(s in "[ -~]{0,100}") {
            prop_assume!(!(s.trim_start().starts_with('{') || s.trim_start().starts_with('[')));
            let masked = mask_string(&s);
            prop_assert!(masked.chars().all(|c| matches!(c, 'a' | 'A' | '9' | ' ' | 'x') || c.is_ascii_punctuation()));
            prop_assert_eq!(masked.chars().count(), s.chars().count());
        }

        #[test]
        fn masking_any_json_value_never_panics_and_never_keeps_long_digit_runs(s in any::<String>(), n in any::<i64>()) {
            let v = json!({"text": s, "number": n, "list": [s, n], "nested": {"deep": s}});
            let masked = mask_value(&v).to_string();
            prop_assert!(!masked.contains("0000000000"), "{masked}");
        }
    }
}
