//! Emoji for reaction names that both services share, and the keys a reaction is sent under.

/// The six classic reactions: the word both services use, and its emoji.
pub(crate) const CLASSIC: [(&str, &str); 6] = [
    ("like", "👍"),
    ("heart", "❤️"),
    ("laugh", "😆"),
    ("surprised", "😮"),
    ("sad", "🙁"),
    ("angry", "😠"),
];

/// Most characters a reaction is shown with. The longest real emoji sequences are about ten code points.
const MAX_REACTION_CHARS: usize = 16;

/// A reaction from the network, made safe to draw: no control characters, bidi marks or whitespace, and no more than
/// [`MAX_REACTION_CHARS`] characters. The joiner (U+200D) and variation selectors stay, because sequences need them.
pub(crate) fn shown(reaction: &str) -> String {
    crate::text::clean(reaction)
        .chars()
        .filter(|&c| !c.is_whitespace() && c != '\u{061C}')
        .take(MAX_REACTION_CHARS)
        .collect()
}

/// The emoji for a classic reaction name (`like`, `heart`, ...); other names are shown as they are written (made safe
/// to draw, see [`shown`]).
pub(crate) fn reaction_emoji(kind: &str) -> String {
    CLASSIC
        .iter()
        .find(|(key, _)| *key == kind)
        .map_or_else(|| shown(kind), |(_, emoji)| (*emoji).to_string())
}

/// The code points of `emoji`, except that a lone symbol from the Basic Multilingual Plane drops its emoji-presentation
/// selector (U+FE0F), so `❤` and `❤️` are the same emoji.
fn code_points(emoji: &str) -> Vec<char> {
    let chars: Vec<char> = emoji.chars().collect();
    match chars[..] {
        [c, '\u{FE0F}'] if (c as u32) <= 0xFFFF => vec![c],
        _ => chars,
    }
}

/// An emoji reduced to what identifies it, so spellings that differ only in the emoji-presentation selector (`❤` and
/// `❤️`) are equal: a key for sets and maps of emoji.
#[must_use]
pub fn canonical(emoji: &str) -> String {
    code_points(emoji).into_iter().collect()
}

/// Whether two strings are the same emoji, ignoring the emoji-presentation selector some spell out (`❤` and `❤️`).
pub fn same_emoji(a: &str, b: &str) -> bool {
    code_points(a) == code_points(b)
}

/// The classic reaction word for `emoji`, if it is one of the six.
pub(crate) fn classic_key(emoji: &str) -> Option<&'static str> {
    CLASSIC
        .iter()
        .find(|(_, classic)| same_emoji(classic, emoji))
        .map(|(key, _)| *key)
}

/// Something that can be sent as a reaction: not empty, short, and free of control characters, bidi marks and spaces.
#[cfg(feature = "teams")]
pub(crate) fn is_emoji_text(emoji: &str) -> bool {
    !emoji.is_empty() && shown(emoji) == emoji
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_names_become_emoji_and_anything_else_is_kept() {
        assert_eq!(reaction_emoji("like"), "👍");
        assert_eq!(reaction_emoji("angry"), "😠");
        assert_eq!(reaction_emoji("custom"), "custom");
    }

    #[test]
    fn a_reaction_from_the_network_cannot_carry_controls_bidi_marks_or_length() {
        assert_eq!(reaction_emoji("\u{1b}[2J"), "[2J");
        assert_eq!(reaction_emoji("a\u{202E}b\u{2067}c"), "abc");
        assert_eq!(reaction_emoji("a b\nc"), "abc");
        assert_eq!(reaction_emoji(&"x".repeat(1_000)).chars().count(), 16);
        assert_eq!(
            shown("👨\u{200D}💻"),
            "👨\u{200D}💻",
            "sequences keep their joiner"
        );
        assert_eq!(shown("❤\u{FE0F}"), "❤\u{FE0F}");
    }

    #[test]
    fn an_emoji_is_the_same_with_or_without_its_presentation_selector() {
        assert!(same_emoji("❤", "❤️"));
        assert!(same_emoji("👍", "👍"));
        assert!(!same_emoji("👍", "👎"));
        assert!(!same_emoji("", "👍"));
        // Longer sequences keep every code point.
        assert!(!same_emoji("🏳️‍🌈", "🏳‍🌈"));
    }
}
