//! What the emoji selector offers, and how it is searched.
//!
//! Two lists feed it, built by one routine ([`merge`]). Reacting is limited to what the service accepts, so the reaction
//! list comes from the backend, and until that has arrived (or if it cannot be had) it is the six classic reactions, which
//! every service accepts. Writing a message can use any emoji, so its list is the reaction list followed by whatever else
//! the shortcode table has (until the service's list has arrived, just the table): everything that can be reacted with
//! can be written, and the picker is the same one.

use crate::convert::EMOJI;
use std::collections::HashMap;
use std::sync::OnceLock;
use teams_client::{EmojiInfo, emoji_key};

/// One emoji in the selector.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Its name: a shortcode such as `thumbsup`, or what the service calls it.
    pub name: String,
    pub emoji: String,
    /// Other names for the same emoji (`+1`), searchable but not listed again.
    pub aliases: Vec<String>,
    /// The name and the aliases in lower case, made once because every key typed in the search compares with them.
    lower: Vec<String>,
}

impl Entry {
    fn new(name: String, emoji: String, aliases: Vec<String>) -> Self {
        let lower = std::iter::once(&name)
            .chain(&aliases)
            .map(|n| n.to_lowercase())
            .collect();
        Self {
            name,
            emoji,
            aliases,
            lower,
        }
    }

    /// Another name for the emoji, unless it has that name already.
    fn add_alias(&mut self, word: String) {
        if self.name != word && !self.aliases.contains(&word) {
            self.lower.push(word.to_lowercase());
            self.aliases.push(word);
        }
    }
}

impl From<EmojiInfo> for Entry {
    fn from(info: EmojiInfo) -> Self {
        Self::new(info.name, info.emoji, info.aliases)
    }
}

/// The six classic reactions, which come first because they are what people react with most.
const FIRST: [&str; 6] = [
    "thumbsup",
    "heart",
    "laughing",
    "open_mouth",
    "slightly_frowning_face",
    "angry",
];

/// Every emoji that can go into a message: the classic reactions, then the rest of the shortcodes in their own order,
/// each emoji once.
pub fn catalog() -> &'static [Entry] {
    static CATALOG: OnceLock<Vec<Entry>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut entries: Vec<Entry> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        let ordered = FIRST
            .iter()
            .filter_map(|n| EMOJI.iter().find(|(name, _)| name == n))
            .chain(EMOJI.iter());
        for (name, emoji) in ordered {
            match at.get(&emoji_key(emoji)) {
                Some(&i) => entries[i].add_alias((*name).to_string()),
                None => {
                    at.insert(emoji_key(emoji), entries.len());
                    entries.push(Entry::new(
                        (*name).to_string(),
                        (*emoji).to_string(),
                        Vec::new(),
                    ));
                }
            }
        }
        entries
    })
}

/// The six reactions every service accepts.
pub fn classic() -> Vec<Entry> {
    catalog().iter().take(FIRST.len()).cloned().collect()
}

/// `list` with `more` added: an emoji the list has already keeps its place and name and gains the new names as words it
/// can be found by, and any other goes on the end, each emoji once.
fn merge(mut list: Vec<Entry>, more: impl IntoIterator<Item = EmojiInfo>) -> Vec<Entry> {
    // The service may offer thousands; looking each one up by key keeps this linear in their number.
    let mut at: HashMap<String, usize> = list
        .iter()
        .enumerate()
        .map(|(i, e)| (emoji_key(&e.emoji), i))
        .collect();
    for info in more {
        match at.get(&emoji_key(&info.emoji)) {
            Some(&i) => {
                for word in std::iter::once(info.name).chain(info.aliases) {
                    list[i].add_alias(word);
                }
            }
            None => {
                at.insert(emoji_key(&info.emoji), list.len());
                list.push(info.into());
            }
        }
    }
    list
}

/// The service's reaction list with the classic six first, each emoji once.
pub fn reactions(offered: Vec<EmojiInfo>) -> Vec<Entry> {
    merge(classic(), offered)
}

/// What can be written into a message: the reaction list, then the shortcode table's emoji that are not in it (and the
/// table's names as more words to find the others by).
pub fn insertable(offered: Vec<EmojiInfo>) -> Vec<Entry> {
    merge(
        reactions(offered),
        catalog().iter().map(|e| EmojiInfo {
            emoji: e.emoji.clone(),
            name: e.name.clone(),
            aliases: e.aliases.clone(),
        }),
    )
}

/// The entries of `list` whose name or alias contains `query` (ignoring case and a leading or trailing `:`), those that
/// start with it first. An empty query matches everything, in list order.
pub fn search<'a>(list: &'a [Entry], query: &str) -> Vec<&'a Entry> {
    let q = query.trim().trim_matches(':').to_lowercase();
    let mut starts = Vec::new();
    let mut contains = Vec::new();
    for entry in list {
        if q.is_empty() || entry.lower.iter().any(|n| n.starts_with(&q)) {
            starts.push(entry);
        } else if entry.lower.iter().any(|n| n.contains(&q)) {
            contains.push(entry);
        }
    }
    starts.extend(contains);
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(emoji: &str, name: &str, aliases: &[&str]) -> EmojiInfo {
        EmojiInfo {
            emoji: emoji.into(),
            name: name.into(),
            aliases: aliases.iter().map(|a| (*a).into()).collect(),
        }
    }

    #[test]
    fn the_catalogue_leads_with_the_classic_reactions_and_lists_each_emoji_once() {
        let names: Vec<&str> = catalog().iter().take(6).map(|e| e.name.as_str()).collect();
        assert_eq!(names, FIRST);
        let all = catalog();
        for (i, a) in all.iter().enumerate() {
            assert!(!a.name.is_empty() && !a.emoji.is_empty());
            for b in &all[i + 1..] {
                assert!(
                    emoji_key(&a.emoji) != emoji_key(&b.emoji),
                    "{} and {} are the same emoji",
                    a.name,
                    b.name
                );
            }
        }
        assert!(all.len() > 50);
    }

    #[test]
    fn aliases_are_found_but_not_listed_again() {
        let thumbs = catalog().iter().find(|e| e.name == "thumbsup").unwrap();
        assert!(thumbs.aliases.iter().any(|a| a == "+1"));
        assert_eq!(
            search(catalog(), "+1").first().map(|e| e.name.as_str()),
            Some("thumbsup")
        );
        assert_eq!(catalog().iter().filter(|e| e.emoji == "👍").count(), 1);
    }

    #[test]
    fn a_search_matches_names_that_start_with_it_before_those_that_contain_it() {
        let hits: Vec<&str> = search(catalog(), "smile")
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(hits.first(), Some(&"smile"));
        assert!(hits.contains(&"sweat_smile"), "contains");
        let start_of_contains = hits.iter().position(|n| *n == "sweat_smile").unwrap();
        assert!(hits.iter().position(|n| *n == "smiley").unwrap() < start_of_contains);
    }

    #[test]
    fn colons_and_case_do_not_matter_and_an_empty_search_lists_everything() {
        let all = catalog();
        assert_eq!(
            search(all, ":TADA:").first().map(|e| e.name.as_str()),
            Some("tada")
        );
        assert_eq!(search(all, "  ").len(), all.len());
        assert!(search(all, "zzzzzz-nothing").is_empty());
    }

    #[test]
    fn the_services_list_keeps_the_classic_six_first_and_adds_only_what_it_offers() {
        let list = reactions(vec![
            info("🍻", "Clinking beer mugs", &["beer", "cheers"]),
            info("👍", "Like", &["yes"]),
            info("🔥", "Fire", &[]),
        ]);
        let emoji: Vec<&str> = list.iter().map(|e| e.emoji.as_str()).collect();
        assert_eq!(emoji, ["👍", "❤️", "😆", "😮", "🙁", "😠", "🍻", "🔥"]);
        assert_eq!(
            search(&list, "yes").first().map(|e| e.emoji.as_str()),
            Some("👍"),
            "the service's words find the classic ones too"
        );
        assert_eq!(
            search(&list, "cheers").first().map(|e| e.emoji.as_str()),
            Some("🍻")
        );
        assert!(
            search(&list, "tada").is_empty(),
            "an emoji the service has no reaction for is not offered"
        );
    }

    #[test]
    fn a_service_list_of_thousands_is_merged_quickly() {
        let offered: Vec<EmojiInfo> = (0..6_000u32)
            .map(|n| EmojiInfo {
                emoji: char::from_u32(0x1_0000 + n).unwrap().to_string(),
                name: format!("emoji {n}"),
                aliases: vec![],
            })
            .collect();
        let started = std::time::Instant::now();
        let list = reactions(offered);
        assert_eq!(list.len(), 6 + 6_000);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn what_can_be_written_is_what_can_be_reacted_with_and_then_the_shortcodes_the_service_has_no_reaction_for()
     {
        let offered = vec![
            info("🦃", "Turkey", &["thanksgiving"]),
            info("🍕", "Pizza slice", &["food"]),
        ];
        let reacting = reactions(offered.clone());
        let writing = insertable(offered);
        // The same list first, in the same order.
        assert_eq!(
            writing
                .iter()
                .take(reacting.len())
                .map(|e| e.emoji.as_str())
                .collect::<Vec<_>>(),
            reacting
                .iter()
                .map(|e| e.emoji.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            search(&writing, "turkey").len(),
            1,
            "the service's emoji can be written"
        );
        // Then the table's own: an emoji only it has, and its names for ones the service has too.
        assert!(
            search(&reacting, "smile").is_empty() && !search(&writing, "smile").is_empty(),
            "a shortcode with no reaction can be written, not reacted with"
        );
        assert_eq!(
            search(&writing, "pizza").first().map(|e| e.name.as_str()),
            Some("Pizza slice"),
            "found by the table's name, listed under the service's"
        );
        for (i, a) in writing.iter().enumerate() {
            assert!(
                writing[i + 1..]
                    .iter()
                    .all(|b| emoji_key(&a.emoji) != emoji_key(&b.emoji)),
                "{} is listed twice",
                a.name
            );
        }
        // Without a service list it is the table, as it always was.
        assert_eq!(insertable(vec![]), catalog().to_vec());
    }

    #[test]
    fn without_a_service_list_only_the_classic_six_are_offered() {
        assert_eq!(classic().len(), 6);
        assert_eq!(reactions(vec![]), classic());
    }
}
