//! Plain-text helpers for text that arrives from the network.

/// Strip characters that must never reach a terminal from remote text: C0/C1 controls (so no escape sequences,
/// OSC-52 clipboard writes, ...) other than newline and tab, and bidi overrides and marks (including the Arabic letter\n/// mark, U+061C) that can disguise text.
#[must_use]
pub fn clean(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            if c == '\n' || c == '\t' {
                return true;
            }
            !(c.is_control() || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}'))
        })
        .collect()
}

/// A short tag for an opaque identifier (a chat id, an object id) to put in a log line: its first few characters, the
/// host-like part after an `@`, and its length. It tells ids apart without recording them whole.
#[must_use]
pub fn id_tag(id: &str) -> String {
    let line = |s: &str, max: usize| -> String {
        clean(s)
            .chars()
            .filter(|c| !c.is_control())
            .take(max)
            .collect()
    };
    let head = line(id, 6);
    match id.rsplit_once('@') {
        Some((_, domain)) => format!("{head}..@{}({})", line(domain, 40), id.len()),
        None => format!("{head}..({})", id.len()),
    }
}

/// Plain text with tags stripped and entities decoded; block boundaries become spaces and whitespace is collapsed.
/// Suitable for one-line previews. The result has been through [`clean`].
#[must_use]
pub fn html_to_plain(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut tag = String::new();
    let mut chars = html.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                // A `<` only starts a tag if it is followed by an ASCII letter or `/` (for closing tags); otherwise it is text.
                if chars
                    .peek()
                    .is_some_and(|&next| next.is_ascii_alphabetic() || next == '/')
                {
                    in_tag = true;
                    tag.clear();
                } else {
                    out.push(c);
                }
            }
            '>' if in_tag => {
                in_tag = false;
                let name = tag
                    .trim_start_matches('/')
                    .split(|c: char| c.is_whitespace() || c == '/')
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if matches!(
                    name.as_str(),
                    "br" | "p" | "div" | "li" | "blockquote" | "tr" | "pre" | "codeblock"
                ) {
                    out.push(' ');
                } else if (name == "emoji" || name == "img")
                    && let Some(alt) = tag.split("alt=\"").nth(1).and_then(|a| a.split('"').next())
                {
                    out.push_str(alt);
                }
            }
            _ if in_tag => tag.push(c),
            _ => out.push(c),
        }
    }
    // Entities such as &#27; decode to control characters, so sanitise after decoding.
    clean(&decode_entities(&out))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// One line of plain text for a message body, for a preview. `html` says how the body is written: HTML has its tags
/// stripped and its reply quotes (`<blockquote>`) left out, since a preview of a reply is what was said, not what it
/// answers; anything else is text as it is, in which a `<` is just a character. The result has been through [`clean`]
/// and has its whitespace collapsed.
#[must_use]
pub fn preview_text(body: &str, html: bool) -> String {
    if html {
        html_to_plain(&without_quotes(body))
    } else {
        clean(body).split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// `html` without its `<blockquote>` elements and what is inside them (nested ones too). An opening tag that is never
/// closed leaves the rest of the text out: it is all inside the quote.
fn without_quotes(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut depth = 0usize;
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        if depth == 0 {
            out.push_str(&rest[..open]);
        }
        let tag_and_after = &rest[open..];
        let Some(close) = tag_and_after.find('>') else {
            // No end to this tag: it is plain text, as `html_to_plain` will treat it.
            if depth == 0 {
                out.push_str(tag_and_after);
            }
            return out;
        };
        let tag = &tag_and_after[..=close];
        let inside = tag[1..].trim_start_matches('/');
        let name = inside
            .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .next()
            .unwrap_or("");
        if name.eq_ignore_ascii_case("blockquote") {
            if tag[1..].starts_with('/') {
                depth = depth.saturating_sub(1);
            } else {
                depth += 1;
            }
        } else if depth == 0 {
            out.push_str(tag);
        }
        rest = &tag_and_after[close + 1..];
    }
    if depth == 0 {
        out.push_str(rest);
    }
    out
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        match tail.find(';').filter(|&semi| semi <= 10) {
            Some(semi) => {
                let ent = &tail[1..semi];
                let decoded = match ent {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    _ => ent
                        .strip_prefix("#x")
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                        .and_then(char::from_u32),
                };
                match decoded {
                    Some(c) => {
                        out.push(c);
                        rest = &tail[semi + 1..];
                    }
                    None => {
                        out.push('&');
                        rest = &tail[1..];
                    }
                }
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_tag_is_one_short_line_whatever_the_id_holds() {
        let tag = id_tag("ab\ncd\u{1b}[31m@evil\nhost.example");
        assert!(!tag.contains(['\n', '\r', '\u{1b}']), "{tag:?}");
        let long = id_tag(&format!("19:abc@{}", "d".repeat(500)));
        assert!(long.chars().count() < 80, "{long}");
    }
    #[test]
    fn an_id_tag_tells_ids_apart_without_giving_them_away() {
        let one_on_one = "19:11111111-2222-3333-4444-555555555555_66666666-7777-8888-9999-000000000000@unq.gbl.spaces";
        let tag = id_tag(one_on_one);
        assert_eq!(tag, "19:111..@unq.gbl.spaces(91)");
        assert!(!tag.contains("2222"), "{tag}");
        assert_eq!(id_tag("19:abc@thread.v2"), "19:abc..@thread.v2(16)");
        assert_eq!(id_tag("x"), "x..(1)");
        assert_eq!(id_tag(""), "..(0)");
        assert_eq!(id_tag("19:a@evil\u{1b}[31m"), "19:a@e..@evil[31m(14)");
    }

    #[test]
    fn plain_preview_strips_tags_and_entities() {
        assert_eq!(
            html_to_plain("<p>a &amp; b</p><p>c&nbsp;&lt;d&gt; &#65;</p>"),
            "a & b c <d> A"
        );
    }

    #[test]
    fn a_preview_of_a_reply_is_what_was_said_not_what_it_answers() {
        let reply = concat!(
            r#"<blockquote itemscope itemtype="http://schema.skype.com/Reply" itemid="1">"#,
            r#"<strong itemprop="mri" itemid="x">Ada</strong><p itemprop="preview">the earlier words</p></blockquote>"#,
            "<p>sounds good</p>"
        );
        assert_eq!(preview_text(reply, true), "sounds good");
        let nested = "<BLOCKQUOTE>a<blockquote>b</blockquote>c</BLOCKQUOTE>after";
        assert_eq!(preview_text(nested, true), "after");
        assert_eq!(
            preview_text("<p>no quote</p>", true),
            "no quote",
            "ordinary markup is unaffected"
        );
        assert_eq!(
            preview_text("before<blockquote>never closed", true),
            "before",
            "an unclosed quote hides only what is inside it"
        );
    }

    #[test]
    fn a_plain_text_body_is_not_read_as_markup() {
        assert_eq!(
            preview_text("if a < b and c > d\n  and more", false),
            "if a < b and c > d and more"
        );
        assert_eq!(preview_text("a\u{1b}[31mb", false), "a[31mb");
    }

    #[test]
    fn emoji_and_images_keep_their_alt_text() {
        assert_eq!(
            html_to_plain(r#"hi <emoji id="smile" alt="😄"></emoji> <img src="x" alt="pic">"#),
            "hi 😄 pic"
        );
    }

    #[test]
    fn entities_cannot_smuggle_control_characters() {
        let out = html_to_plain("a&#27;[31mred&#x1b;]52;c;Zm9v&#7;b");
        assert!(!out.chars().any(char::is_control), "{out:?}");
    }

    #[test]
    fn clean_drops_controls_and_bidi_overrides_but_keeps_newlines_and_tabs() {
        assert_eq!(
            clean("a\u{1b}[31mb\u{202E}c\u{200F}d\ne\tf"),
            "a[31mbcd\ne\tf"
        );
        // The Arabic letter mark is a bidi control too, and invisible.
        assert_eq!(clean("ab\u{061C}c"), "abc");
    }

    #[test]
    fn a_less_than_sign_that_does_not_start_a_tag_is_text() {
        assert_eq!(html_to_plain("I <3 Mondays"), "I <3 Mondays");
        assert_eq!(html_to_plain("a < b"), "a < b");
        assert_eq!(html_to_plain("x<"), "x<");
        assert_eq!(html_to_plain("<"), "<");
        assert_eq!(html_to_plain("a<p>c</p>"), "a c", "real tags still work");
    }

    /// Found by a property test: a `<` is the start of a tag only before an ASCII letter or `/`. Before a letter of
    /// another alphabet (here U+00C2) it is text, and so is what follows it.
    #[test]
    fn a_less_than_sign_before_a_non_ascii_letter_is_text_not_a_tag() {
        let s = "<\u{c2}\u{ba}";
        assert_eq!(html_to_plain(s), s);
        assert_eq!(html_to_plain("a <\u{c2}\u{ba} b"), "a <\u{c2}\u{ba} b");
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn clean_leaves_no_controls_or_bidi_overrides_and_is_idempotent(s in any::<String>()) {
            let cleaned = clean(&s);
            let bad = |c: char| {
                (c.is_control() && c != '\n' && c != '\t')
                    || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}')
            };
            prop_assert!(!cleaned.chars().any(bad));
            prop_assert_eq!(clean(&cleaned), cleaned);
        }

        #[test]
        fn html_to_plain_accepts_anything_and_returns_single_spaced_clean_text(s in any::<String>()) {
            let plain = html_to_plain(&s);
            prop_assert!(!plain.chars().any(char::is_control));
            prop_assert!(!plain.contains("  "));
            prop_assert_eq!(plain.trim(), plain.as_str());
        }

        #[test]
        fn markup_never_survives_as_tags_for_simple_documents(words in prop::collection::vec("[a-z]{1,8}", 0..8)) {
            let html = format!("<p>{}</p>", words.join("</b> <b>"));
            prop_assert_eq!(html_to_plain(&html), words.join(" "));
        }

        #[test]
        fn text_without_any_tag_start_stays_itself(s in "[^<]*|<[^A-Za-z/]*") {
            // Text that has no `<` followed by a letter stays itself, modulo whitespace collapsing and entity decoding.
            let result = html_to_plain(&s);
            let expected = clean(&decode_entities(&s))
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            prop_assert_eq!(result, expected);
        }
    }
}
