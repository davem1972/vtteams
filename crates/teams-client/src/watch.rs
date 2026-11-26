//! Noticing when a service starts sending something new.
//!
//! The chat services are undocumented or change without notice, and their answers are read as loose JSON, so a new or
//! retyped field would otherwise go unseen until something stops working. A [`Watch`] remembers which field names (with
//! their JSON types) it has seen on each kind of object in this session and logs the first appearance of any other.
//! The first sighting of a kind is the baseline (`debug`); a name that shows up later is a change in the protocol
//! (`info`). Only names, JSON types and short protocol tokens (such as a message type) are logged, never values.

use crate::lock;
use crate::record::mask_value;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// Names remembered per kind; a service sending endless distinct names cannot grow the memory without bound.
const MAX_NAMES: usize = 2_000;
/// Longest name or token logged.
const MAX_LEN: usize = 60;
/// Most new names reported in one record.
const MAX_REPORTED: usize = 40;
/// Longest masked shape logged for an unreadable item.
const MAX_SHAPE: usize = 600;

#[derive(Default)]
pub(crate) struct Watch {
    seen: Mutex<BTreeMap<&'static str, BTreeSet<String>>>,
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Text from the service, made safe and short enough for a log line.
fn tame(s: &str) -> String {
    crate::clean(s)
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_LEN)
        .collect()
}

impl Watch {
    /// Record what has not been seen yet in `entries` for `kind`. Returns whether this is the first sighting of the
    /// kind and the new entries, in order.
    fn note(
        &self,
        kind: &'static str,
        entries: impl IntoIterator<Item = String>,
    ) -> Option<(bool, Vec<String>)> {
        let mut seen = lock(&self.seen);
        let first = !seen.contains_key(kind);
        let known = seen.entry(kind).or_default();
        let mut fresh = Vec::new();
        for entry in entries {
            if known.len() < MAX_NAMES && known.insert(entry.clone()) {
                fresh.push(entry);
            }
        }
        // The very first sighting is reported even when empty, so the baseline is on record.
        (first || !fresh.is_empty()).then_some((first, fresh))
    }

    fn report(kind: &str, noted: Option<(bool, Vec<String>)>, what: &str) {
        let Some((first, mut fresh)) = noted else {
            return;
        };
        fresh.sort();
        let more = fresh.len().saturating_sub(MAX_REPORTED);
        fresh.truncate(MAX_REPORTED);
        let list = fresh.join(", ");
        let more = if more > 0 {
            format!(" (and {more} more)")
        } else {
            String::new()
        };
        if first {
            log::debug!("{kind}: {what} seen: {list}{more}");
        } else {
            log::info!("{kind}: new {what} since the start of this session: {list}{more}");
        }
    }

    /// Note the fields (name and JSON type) of `objects`, which are all of one `kind`.
    pub(crate) fn fields<'a>(
        &self,
        kind: &'static str,
        objects: impl IntoIterator<Item = &'a Value>,
    ) {
        if !log::log_enabled!(log::Level::Info) {
            return;
        }
        let entries = objects
            .into_iter()
            .filter_map(Value::as_object)
            .flat_map(|o| {
                o.iter()
                    .map(|(name, v)| format!("{}: {}", tame(name), json_type(v)))
            });
        Self::report(kind, self.note(kind, entries.collect::<Vec<_>>()), "fields");
    }

    /// Note the values of a small closed set of protocol tokens, such as a message type.
    pub(crate) fn tokens<'a>(&self, kind: &'static str, tokens: impl IntoIterator<Item = &'a str>) {
        if !log::log_enabled!(log::Level::Info) {
            return;
        }
        let entries: Vec<String> = tokens.into_iter().map(tame).collect();
        Self::report(kind, self.note(kind, entries), "values");
    }

    #[cfg(test)]
    fn field_entries(&self, kind: &'static str, objects: &[Value]) -> Option<(bool, Vec<String>)> {
        let entries: Vec<String> = objects
            .iter()
            .filter_map(Value::as_object)
            .flat_map(|o| {
                o.iter()
                    .map(|(name, v)| format!("{}: {}", tame(name), json_type(v)))
            })
            .collect();
        self.note(kind, entries)
    }
}

/// The structure of `v` with every value masked as `--record-shapes` does it (letters, digits and values reduced to
/// character classes, secrets redacted), cut short: enough to see which fields an unreadable item had.
pub(crate) fn masked(v: &Value) -> String {
    let text = mask_value(v).to_string();
    match text.char_indices().nth(MAX_SHAPE) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_first_sighting_is_the_baseline_and_only_later_names_are_news() {
        let w = Watch::default();
        let a = json!({"id": "1", "content": "secret words", "n": 3});
        let (first, fresh) = w
            .field_entries("message", std::slice::from_ref(&a))
            .unwrap();
        assert!(first);
        assert_eq!(
            fresh,
            ["content: string", "id: string", "n: number"].map(String::from)
        );
        assert!(
            w.field_entries("message", std::slice::from_ref(&a))
                .is_none(),
            "nothing new"
        );
        let b = json!({"id": "2", "reactions": [], "n": "3"});
        let (first, fresh) = w.field_entries("message", &[b]).unwrap();
        assert!(!first);
        assert_eq!(fresh, ["n: string", "reactions: array"].map(String::from));
    }

    #[test]
    fn kinds_are_kept_apart_and_values_never_appear() {
        let w = Watch::default();
        let (_, fresh) = w
            .field_entries("a", &[json!({"x": "VALUE-THAT-MUST-NOT-APPEAR"})])
            .unwrap();
        assert_eq!(fresh, ["x: string"]);
        let (first, _) = w.field_entries("b", &[json!({"x": 1})]).unwrap();
        assert!(first, "another kind has its own baseline");
    }

    #[test]
    fn a_name_is_one_line() {
        assert_eq!(tame("two\nlines\tand a tab"), "twolinesand a tab");
    }
    #[test]
    fn hostile_names_are_made_safe_short_and_bounded() {
        let w = Watch::default();
        let long = "n".repeat(500);
        let obj = json!({ long: 1, "esc\u{1b}[31m": 2 });
        let (_, fresh) = w.field_entries("k", &[obj]).unwrap();
        assert!(
            fresh.iter().all(|e| e.chars().count() <= MAX_LEN + 10),
            "{fresh:?}"
        );
        assert!(fresh.iter().all(|e| !e.contains('\u{1b}')));
        let many: Vec<Value> = (0..MAX_NAMES + 50)
            .map(|i| json!({ format!("field{i}"): 1 }))
            .collect();
        w.field_entries("flood", &many);
        assert!(lock(&w.seen)["flood"].len() <= MAX_NAMES);
    }

    #[test]
    fn tokens_are_noted_like_fields() {
        let w = Watch::default();
        let (first, fresh) = w
            .note("type", ["Text", "RichText/Html", "Text"].map(String::from))
            .unwrap();
        assert!(first);
        assert_eq!(fresh, ["Text", "RichText/Html"]);
        let (first, fresh) = w
            .note("type", ["ThreadActivity/AddMember".to_string()])
            .unwrap();
        assert!(!first);
        assert_eq!(fresh, ["ThreadActivity/AddMember"]);
    }

    #[test]
    fn a_masked_shape_has_the_structure_and_none_of_the_content() {
        let shape =
            masked(&json!({"id": "19:abc@thread.v2", "lastMessage": {}, "content": "hello world"}));
        assert!(shape.contains("lastMessage"), "{shape}");
        assert!(
            !shape.contains("hello") && !shape.contains("abc"),
            "{shape}"
        );
        let huge = masked(&json!({"k": "x".repeat(5_000)}));
        assert!(huge.chars().count() <= MAX_SHAPE + 3);
    }
}
