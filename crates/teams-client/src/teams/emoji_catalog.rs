//! Teams' own emoji catalogue, which decides what a reaction can be.
//!
//! A reaction is filed under an id from this catalogue (`like`, `fire`, `1f389_partypopper`). An id the catalogue does
//! not know is accepted by the service but drawn as a broken image for everyone in the chat, and the ids are not
//! derivable from an emoji's name (🔥 is `fire`, 🚀 is `launch`, 🎉 is `1f389_partypopper`), so the catalogue is
//! looked up rather than guessed. It is a public static file on Microsoft's content network and needs no sign-in; what
//! it holds is data from the network, so ids are held to a safe alphabet and sizes are bounded.

use crate::emoji::{canonical, is_emoji_text};
use crate::error::{Error, Result};
use crate::model::EmojiInfo;
use crate::text::clean;
use serde::Deserialize;
use std::collections::HashMap;

/// The published catalogue, newest known first. Each address is a content hash, so what it serves never changes.
pub(super) const CATALOG_URLS: [&str; 2] = [
    "https://statics.teams.cdn.office.net/evergreen-assets/personal-expressions/v1/metadata/ec4576179210cde40ce5494513213583/en-gb.json",
    "https://statics.teams.cdn.office.net/evergreen-assets/personal-expressions/v1/metadata/a098bcb732fd7dd80ce11c12ad15767f/en-us.json",
];
/// Largest catalogue accepted (the published ones are about half a megabyte).
pub(super) const MAX_CATALOG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 6_000;
/// Most ids kept for looking an emoji up by id: more than there are entries, because several ids can name one emoji.
const MAX_IDS: usize = 12_000;
const MAX_ID_CHARS: usize = 80;
const MAX_ALIASES: usize = 16;

#[derive(Deserialize)]
struct Document {
    categories: Vec<Category>,
}

#[derive(Deserialize)]
struct Category {
    #[serde(default)]
    emoticons: Vec<Emoticon>,
}

#[derive(Deserialize)]
struct Emoticon {
    id: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    shortcuts: Vec<String>,
    #[serde(default)]
    unicode: String,
    #[serde(default)]
    keywords: Vec<String>,
}

struct Entry {
    id: String,
    info: EmojiInfo,
}

/// The emoji Teams can react with, in the order the catalogue lists them (the common ones first).
pub(super) struct Catalog {
    entries: Vec<Entry>,
    by_emoji: HashMap<String, usize>,
    /// Every id the catalogue gives, with its emoji: how a reaction someone else made is read.
    by_id: HashMap<String, String>,
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_ID_CHARS
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl Catalog {
    pub(super) fn parse(text: &str) -> Result<Self> {
        const OPERATION: &str = "read the emoji list";
        let doc: Document = serde_json::from_str(text).map_err(|e| Error::decode(OPERATION, e))?;
        let mut catalog = Catalog {
            entries: Vec::new(),
            by_emoji: HashMap::new(),
            by_id: HashMap::new(),
        };
        for e in doc.categories.into_iter().flat_map(|c| c.emoticons) {
            if !safe_id(&e.id) || !is_emoji_text(&e.unicode) {
                continue;
            }
            let aliases = e
                .shortcuts
                .iter()
                .map(|s| s.trim_matches(|c| c == '(' || c == ')'))
                .chain(e.keywords.iter().map(String::as_str))
                .map(|a| clean(a).trim().to_lowercase())
                .filter(|a| !a.is_empty());
            if catalog.by_id.len() < MAX_IDS {
                catalog.by_id.insert(e.id.clone(), e.unicode.clone());
            }
            let key = canonical(&e.unicode);
            match catalog.by_emoji.get(&key) {
                // The same emoji under another id (`yes` for 👍): findable by it, but sent as the first id.
                Some(&i) => {
                    let known = &mut catalog.entries[i].info.aliases;
                    for a in aliases {
                        if known.len() < MAX_ALIASES && !known.contains(&a) {
                            known.push(a);
                        }
                    }
                }
                None => {
                    if catalog.entries.len() >= MAX_ENTRIES {
                        break;
                    }
                    let mut known: Vec<String> = Vec::new();
                    for a in aliases {
                        if known.len() < MAX_ALIASES && !known.contains(&a) {
                            known.push(a);
                        }
                    }
                    let name = clean(&e.description).trim().to_string();
                    catalog.by_emoji.insert(key, catalog.entries.len());
                    catalog.entries.push(Entry {
                        info: EmojiInfo {
                            emoji: e.unicode,
                            name: if name.is_empty() { e.id.clone() } else { name },
                            aliases: known,
                        },
                        id: e.id,
                    });
                }
            }
        }
        if catalog.entries.is_empty() {
            return Err(Error::Unexpected {
                operation: OPERATION,
                detail: "the emoji list has no usable entries",
            });
        }
        Ok(catalog)
    }

    /// The id a reaction with `emoji` is filed under, if Teams has that emoji.
    pub(super) fn key_for(&self, emoji: &str) -> Option<&str> {
        let i = *self.by_emoji.get(&canonical(emoji))?;
        Some(self.entries[i].id.as_str())
    }

    /// The emoji a reaction filed under `id` is (`salute` is 🫡, `fire` is 🔥), if the catalogue has that id.
    pub(super) fn emoji_for(&self, id: &str) -> Option<&str> {
        self.by_id.get(id).map(String::as_str)
    }

    pub(super) fn infos(&self) -> Vec<EmojiInfo> {
        self.entries.iter().map(|e| e.info.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of the published file, reduced.
    const SAMPLE: &str = r#"{"categories": [
        {"id": "common", "emoticons": [
            {"id": "like", "description": "Like", "shortcuts": ["(like)"], "unicode": "👍", "keywords": ["yes", "ok"], "etag": "v1"},
            {"id": "heart", "description": "Heart", "shortcuts": ["(heart)"], "unicode": "❤️", "keywords": []}]},
        {"id": "more", "emoticons": [
            {"id": "1f37b_clinkingbeermugs", "description": "Clinking beer mugs", "shortcuts": ["(clinkingbeermugs)", "(1f37b_clinkingbeermugs)"], "unicode": "🍻", "keywords": ["beer", "cheers"]},
            {"id": "fire", "description": "Fire", "shortcuts": ["(fire)"], "unicode": "🔥", "keywords": ["lit"]},
            {"id": "yes", "description": "Yes", "shortcuts": ["(yes)"], "unicode": "👍", "keywords": []}]}]}"#;

    #[test]
    fn an_emoji_is_filed_under_the_id_the_catalogue_gives_it() {
        let c = Catalog::parse(SAMPLE).unwrap();
        assert_eq!(c.key_for("🔥"), Some("fire"), "not a made-up 1f525_fire");
        assert_eq!(c.key_for("🍻"), Some("1f37b_clinkingbeermugs"));
        assert_eq!(
            c.key_for("👍"),
            Some("like"),
            "the first id for an emoji with two"
        );
        assert_eq!(
            c.key_for("❤"),
            Some("heart"),
            "with or without the presentation selector"
        );
        assert_eq!(c.key_for("❤️"), Some("heart"));
        assert_eq!(c.key_for("🦄"), None, "an emoji Teams has no reaction for");
    }

    #[test]
    fn a_reaction_id_is_read_as_the_emoji_it_names_including_ids_that_share_one() {
        let c = Catalog::parse(SAMPLE).unwrap();
        assert_eq!(c.emoji_for("fire"), Some("🔥"));
        assert_eq!(c.emoji_for("1f37b_clinkingbeermugs"), Some("🍻"));
        assert_eq!(
            c.emoji_for("yes"),
            Some("👍"),
            "an id that is not the one it is sent under"
        );
        assert_eq!(c.emoji_for("like"), Some("👍"));
        assert_eq!(c.emoji_for("Fire"), None, "ids are case-sensitive");
        assert_eq!(c.emoji_for("nonesuch"), None);
    }

    #[test]
    fn each_emoji_is_listed_once_with_every_name_it_goes_by() {
        let infos = Catalog::parse(SAMPLE).unwrap().infos();
        let emoji: Vec<&str> = infos.iter().map(|i| i.emoji.as_str()).collect();
        assert_eq!(
            emoji,
            ["👍", "❤️", "🍻", "🔥"],
            "catalogue order, no repeats"
        );
        let thumbs = &infos[0];
        assert_eq!(thumbs.name, "Like");
        for alias in ["like", "yes", "ok"] {
            assert!(
                thumbs.aliases.iter().any(|a| a == alias),
                "{alias} in {:?}",
                thumbs.aliases
            );
        }
        assert!(infos[2].aliases.contains(&"beer".to_string()));
    }

    #[test]
    fn what_the_network_sends_is_held_to_a_safe_shape() {
        let hostile = r#"{"categories": [{"emoticons": [
            {"id": "a b", "unicode": "👍"},
            {"id": "../x", "unicode": "👎"},
            {"id": "ok", "unicode": "two words"},
            {"id": "ok2", "unicode": ""},
            {"id": "good", "description": "Fine\u001b[31m", "unicode": "✅", "shortcuts": ["(good)"]}]}]}"#;
        let c = Catalog::parse(hostile).unwrap();
        let infos = c.infos();
        assert_eq!(infos.len(), 1);
        assert!(!infos[0].name.contains('\u{1b}'));
        assert_eq!(c.key_for("✅"), Some("good"));
        assert_eq!(c.key_for("👍"), None);
        let long = format!(
            r#"{{"categories": [{{"emoticons": [{{"id": "{}", "unicode": "😀"}}]}}]}}"#,
            "x".repeat(200)
        );
        assert!(
            Catalog::parse(&long).is_err(),
            "an id that long is not an id"
        );
    }

    #[test]
    fn a_catalogue_that_is_empty_or_not_one_is_an_error() {
        assert!(Catalog::parse("{}").is_err());
        assert!(Catalog::parse(r#"{"categories": []}"#).is_err());
        assert!(Catalog::parse("not json").is_err());
    }

    #[test]
    fn the_catalogue_stays_bounded() {
        // Distinct emoji, so that only the cap can be what stops the list.
        let many: Vec<String> = (0..MAX_ENTRIES as u32 + 50)
            .map(|i| {
                format!(
                    r#"{{"id": "e{i}", "unicode": "{}"}}"#,
                    char::from_u32(0x1F000 + i).unwrap()
                )
            })
            .collect();
        let doc = format!(
            r#"{{"categories": [{{"emoticons": [{}]}}]}}"#,
            many.join(",")
        );
        let c = Catalog::parse(&doc).unwrap();
        assert_eq!(c.entries.len(), MAX_ENTRIES);
        assert_eq!(
            c.key_for("\u{1F000}"),
            Some("e0"),
            "the first ones are kept"
        );
    }

    #[test]
    fn one_emoji_cannot_collect_more_than_a_few_names() {
        let shortcuts: Vec<String> = (0..100).map(|n| format!("(name{n})")).collect();
        let doc = serde_json::json!({"categories": [{"emoticons": [
            {"id": "a", "unicode": "🔥", "shortcuts": shortcuts, "keywords": ["k1", "k2"]},
            {"id": "b", "unicode": "🔥", "shortcuts": (0..100).map(|n| format!("(other{n})")).collect::<Vec<_>>()}
        ]}]})
        .to_string();
        let infos = Catalog::parse(&doc).unwrap().infos();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].aliases.len(), MAX_ALIASES);
    }
}
