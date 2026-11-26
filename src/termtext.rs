//! Markdown -> styled terminal lines.
//!
//! `termimad` lays Markdown out for a given width (wrapping, lists, tables, code blocks, quotes) and emits
//! ANSI; `ansi_to_lines` turns that into ratatui lines so it can live inside our own scrolling view.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use termimad::crossterm::style::Color as TColor;
use termimad::{Alignment, FmtText, MadSkin};

fn tc(r: u8, g: u8, b: u8) -> TColor {
    TColor::Rgb { r, g, b }
}

/// A skin that matches the dashboard palette (Catppuccin-flavoured, for dark backgrounds).
fn skin() -> MadSkin {
    let mut skin = MadSkin::default_dark();
    let text = tc(205, 214, 244);
    skin.paragraph.set_fg(text);
    skin.bold.set_fg(tc(255, 255, 255));
    skin.italic.set_fg(tc(203, 166, 247));
    skin.strikeout.set_fg(tc(108, 112, 134));
    skin.inline_code.set_fgbg(tc(249, 226, 175), tc(49, 50, 68));
    skin.code_block.set_fgbg(tc(166, 227, 161), tc(30, 30, 46));
    skin.quote_mark.set_fg(tc(148, 226, 213));
    skin.bullet.set_fg(tc(250, 179, 135));
    skin.horizontal_rule.set_fg(tc(88, 91, 112));
    skin.table.set_fg(tc(147, 153, 178));
    let heading = [
        tc(137, 180, 250),
        tc(245, 194, 231),
        tc(148, 226, 213),
        tc(249, 226, 175),
    ];
    for (i, h) in skin.headers.iter_mut().enumerate() {
        h.set_fg(heading[i.min(heading.len() - 1)]);
        h.align = Alignment::Left;
    }
    skin
}

/// termimad's emphasis rules are looser than CommonMark's, which shows up as three glitches: a stray `*` (`2*4 and 3 * 5`,
/// `*.rs and *.toml`) opens an italic run that swallows the text up to the next `*`, `_italic_` / `__bold__` are not
/// recognised at all, and a backslash-escaped `_` (`%AI\_Service`, written by htmd or `plain_to_markdown` to stop a
/// literal underscore in someone's text from being misread, though termimad was never going to) is not unescaped the
/// way `\*` is, so the backslash itself would reach the screen. pulldown-cmark follows CommonMark, so let it say which
/// delimiters really are emphasis: those are rewritten to `*` / `**`, every other `*` is escaped, `\_` is unescaped,
/// and code, list bullets and rules are left alone.
fn normalize_emphasis(md: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag};
    if !md.contains(['*', '_']) {
        return md.to_string();
    }
    let mut delims = std::collections::HashSet::new();
    let mut verbatim: Vec<std::ops::Range<usize>> = Vec::new();
    for (event, range) in Parser::new_ext(md, Options::ENABLE_TABLES).into_offset_iter() {
        match event {
            // The element's range spans from its opening delimiter to the end of its closing one.
            Event::Start(Tag::Emphasis) => delims.extend([range.start, range.end - 1]),
            Event::Start(Tag::Strong) => {
                delims.extend([range.start, range.start + 1, range.end - 2, range.end - 1])
            }
            Event::Code(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::Rule
            | Event::Start(Tag::CodeBlock(_) | Tag::HtmlBlock) => verbatim.push(range),
            // A `*` bullet is the first character of its item.
            Event::Start(Tag::Item) => verbatim.push(range.start..range.start + 1),
            _ => {}
        }
    }
    // One flag per byte, so each character is checked in one step however many ranges there are.
    let mut inside = vec![false; md.len()];
    for range in &verbatim {
        inside[range.start.min(md.len())..range.end.min(md.len())].fill(true);
    }
    let mut out = String::with_capacity(md.len() + 8);
    let mut chars = md.char_indices();
    while let Some((i, c)) = chars.next() {
        if inside[i] {
            out.push(c);
        } else if c == '\\' {
            // Already escaped: keep the pair as written, with one exception. htmd (reading a Teams
            // message) and `plain_to_markdown` both escape every literal `_`, on the assumption that
            // whatever renders the Markdown will strip the backslash back off, the way termimad does
            // for `\*`. termimad never gives `_` any meaning (see the module doc) and, unlike `*`, does
            // not treat `\_` as an escape sequence either: it shows the backslash. So `%AI\_Service`
            // would otherwise reach the screen with a literal backslash in it; drop it here instead,
            // since termimad was never going to read the underscore as emphasis regardless.
            match chars.next() {
                Some((_, '_')) => out.push('_'),
                Some((_, next)) => {
                    out.push(c);
                    out.push(next);
                }
                None => out.push(c),
            }
        } else if delims.contains(&i) {
            out.push('*');
        } else if c == '*' {
            out.push_str("\\*");
        } else {
            out.push(c);
        }
    }
    out
}

/// Lay out `md` for `width` columns and return styled lines.
pub fn render_markdown(md: &str, width: usize) -> Vec<Line<'static>> {
    // crossterm drops colour codes when stdout is not a terminal (tests, `--dump`, pipes). We parse the codes
    // ourselves rather than print them, so always emit them.
    termimad::crossterm::style::force_color_output(true);
    let skin = skin();
    let md = normalize_emphasis(md);
    let text = FmtText::from(&skin, &md, Some(width.max(10)));
    ansi_to_lines(&text.to_string())
}

// ---------------------------------------------------------------------------
// ANSI SGR parsing
// ---------------------------------------------------------------------------

fn basic_color(n: u32, bright: bool) -> Color {
    match (n, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        _ => Color::White,
    }
}

/// Reads the colour that follows a 38/48 code: `5;n` (256-colour) or `2;r;g;b` (truecolour).
fn extended_color(params: &[u32], i: &mut usize) -> Option<Color> {
    match params.get(*i + 1)? {
        5 => {
            let n = *params.get(*i + 2)?;
            *i += 2;
            Some(Color::Indexed(n as u8))
        }
        2 => {
            let (r, g, b) = (
                *params.get(*i + 2)?,
                *params.get(*i + 3)?,
                *params.get(*i + 4)?,
            );
            *i += 4;
            Some(Color::Rgb(r as u8, g as u8, b as u8))
        }
        _ => None,
    }
}

fn apply_sgr(style: &mut Style, params: &[u32]) {
    let mut i = 0;
    while i < params.len() {
        match params[i] {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            2 => *style = style.add_modifier(Modifier::DIM),
            3 => *style = style.add_modifier(Modifier::ITALIC),
            4 => *style = style.add_modifier(Modifier::UNDERLINED),
            7 => *style = style.add_modifier(Modifier::REVERSED),
            9 => *style = style.add_modifier(Modifier::CROSSED_OUT),
            22 => *style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => *style = style.remove_modifier(Modifier::ITALIC),
            24 => *style = style.remove_modifier(Modifier::UNDERLINED),
            27 => *style = style.remove_modifier(Modifier::REVERSED),
            29 => *style = style.remove_modifier(Modifier::CROSSED_OUT),
            n @ 30..=37 => style.fg = Some(basic_color(n - 30, false)),
            n @ 90..=97 => style.fg = Some(basic_color(n - 90, true)),
            n @ 40..=47 => style.bg = Some(basic_color(n - 40, false)),
            n @ 100..=107 => style.bg = Some(basic_color(n - 100, true)),
            39 => style.fg = None,
            49 => style.bg = None,
            38 => {
                if let Some(c) = extended_color(params, &mut i) {
                    style.fg = Some(c);
                }
            }
            48 => {
                if let Some(c) = extended_color(params, &mut i) {
                    style.bg = Some(c);
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Convert a string containing ANSI SGR escapes into ratatui lines. Non-colour escapes are dropped.
pub fn ansi_to_lines(s: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut style = Style::default();
    for raw in s.split('\n') {
        let raw = raw.trim_end_matches('\r');
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut buf = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                buf.push(c);
                continue;
            }
            if chars.peek() != Some(&'[') {
                continue;
            }
            chars.next();
            let mut body = String::new();
            let mut fin = None;
            for ch in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&ch) {
                    fin = Some(ch);
                    break;
                }
                body.push(ch);
            }
            if fin == Some('m') {
                if !buf.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buf), style));
                }
                let params: Vec<u32> = if body.is_empty() {
                    vec![0]
                } else {
                    body.split([';', ':'])
                        .map(|p| p.parse().unwrap_or(0))
                        .collect()
                };
                apply_sgr(&mut style, &params);
            }
        }
        if !buf.is_empty() {
            spans.push(Span::styled(buf, style));
        }
        lines.push(Line::from(spans));
    }
    // A trailing newline in the input produces one empty final line; drop it.
    if lines.last().is_some_and(|l| l.spans.is_empty()) {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    #[test]
    fn emphasis_of_a_hostile_message_is_normalised_in_linear_time() {
        // Thousands of code spans, each of which the normaliser must step over, between underscores.
        let hostile = "_a `x` ".repeat(20_000 / 7);
        let started = std::time::Instant::now();
        let out = normalize_emphasis(&hostile);
        assert!(out.contains("`x`"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn parses_basic_and_truecolor_sgr() {
        let lines = ansi_to_lines("\x1b[1;38;2;10;20;30mhi\x1b[0m plain\n\x1b[48;5;200mx\x1b[0m");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].content, "hi");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(10, 20, 30)));
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[0].spans[1].content, " plain");
        assert_eq!(lines[0].spans[1].style, Style::default());
        assert_eq!(lines[1].spans[0].style.bg, Some(Color::Indexed(200)));
    }

    #[test]
    fn style_carries_across_lines_and_unknown_escapes_are_dropped() {
        let lines = ansi_to_lines("\x1b[3mone\ntwo\x1b[2K\x1b[0m");
        assert!(
            lines[1].spans[0]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert_eq!(text(&lines[1]), "two");
    }

    #[test]
    fn renders_markdown_structure_within_the_width() {
        let md = "# Title\n\nSome **bold** text that is long enough to need wrapping when the width is narrow.\n\n* one\n* two\n\n> quoted\n\n```\ncode here\n```";
        let lines = render_markdown(md, 30);
        let joined: Vec<String> = lines.iter().map(text).collect();
        let all = joined.join("\n");
        assert!(
            all.contains("Title")
                && all.contains("bold")
                && all.contains("code here")
                && all.contains("quoted"),
            "{all}"
        );
        assert!(
            all.contains('•') || all.contains('*'),
            "bullets rendered: {all}"
        );
        for l in &joined {
            assert!(
                l.chars().count() <= 30,
                "line too wide ({}): {l:?}",
                l.chars().count()
            );
        }
        // Escape codes must not leak through as text.
        assert!(!all.contains('\x1b'));
    }

    /// (text, italic, bold) for every span, so tests can check what is emphasised.
    fn runs(md: &str) -> Vec<(String, bool, bool)> {
        render_markdown(md, 80)
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| {
                (
                    s.content.to_string(),
                    s.style.add_modifier.contains(Modifier::ITALIC),
                    s.style.add_modifier.contains(Modifier::BOLD),
                )
            })
            .collect()
    }

    fn italic_text(md: &str) -> String {
        runs(md)
            .into_iter()
            .filter(|(_, i, _)| *i)
            .map(|(t, _, _)| t)
            .collect()
    }

    fn plain(md: &str) -> String {
        runs(md).into_iter().map(|(t, _, _)| t).collect()
    }

    #[test]
    fn real_emphasis_still_works() {
        assert_eq!(italic_text("an *italic* word"), "italic");
        assert_eq!(italic_text("***both*** end"), "both");
        assert!(
            runs("***both***")
                .iter()
                .any(|(t, i, b)| t == "both" && *i && *b)
        );
        assert_eq!(italic_text("snake_case_name stays plain"), "");
    }

    #[test]
    fn stray_asterisks_do_not_start_runaway_italics() {
        for md in [
            "a * b * c and 2*4 and 3 * 5",
            "globs like *.rs and *.toml here",
            "*never closed and a lone * here",
        ] {
            assert_eq!(italic_text(md), "", "{md:?}");
        }
        // And the asterisks are still shown.
        assert_eq!(
            plain("a * b * c and 2*4 and 3 * 5"),
            "a * b * c and 2*4 and 3 * 5"
        );
        assert_eq!(
            plain("globs like *.rs and *.toml here"),
            "globs like *.rs and *.toml here"
        );
    }

    #[test]
    fn underscore_emphasis_is_recognised() {
        assert_eq!(italic_text("_under_ and __strong__"), "under");
        assert!(
            runs("__strong__")
                .iter()
                .any(|(t, _, b)| t == "strong" && *b)
        );
        assert_eq!(plain("_under_ and __strong__"), "under and strong");
    }

    #[test]
    fn parenthesised_underscore_notes_are_italic() {
        // The shape LLM output commonly uses for asides: `_(note)_`.
        assert_eq!(italic_text("_(searching the web)_"), "(searching the web)");
        assert_eq!(
            italic_text("Done. _(3 results, 1.2s)_ Next line."),
            "(3 results, 1.2s)"
        );
        assert_eq!(plain("Done. _(3 results)_"), "Done. (3 results)");
    }

    #[test]
    fn code_bullets_rules_and_author_escapes_are_left_alone() {
        assert_eq!(plain("`2*3*4` and `_x_`"), "2*3*4 and _x_");
        assert!(
            plain("```\nlet a = 2*3*4;\nlet b = _x_;\n```").contains("2*3*4")
                && plain("```\nlet b = _x_;\n```").contains("_x_")
        );
        let bullets = render_markdown("* one\n* two *em*\n\n---\n\n***", 40);
        let text: Vec<String> = bullets.iter().map(text).collect();
        assert!(
            text.iter().filter(|l| l.contains('•')).count() == 2,
            "{text:?}"
        );
        assert_eq!(italic_text("* one\n* two *em*"), "em");
        assert_eq!(
            plain("escaped \\*lit\\* and a\\*b"),
            "escaped *lit* and a*b"
        );
    }

    #[test]
    fn a_backslash_escaped_underscore_shows_no_backslash() {
        // htmd and `plain_to_markdown` escape every literal `_`; termimad does not unescape `\_` the
        // way it does `\*`, so without this the backslash itself would reach the screen.
        assert_eq!(
            plain(r"%AI\_Service and %AI\_Keys"),
            "%AI_Service and %AI_Keys"
        );
        // A code span or block is someone's literal text and is left exactly as written.
        assert_eq!(plain(r"`a\_b`"), r"a\_b");
        assert!(plain("```\nlet a = x\\_y;\n```").contains(r"x\_y"));
    }
    #[test]
    fn headings_are_styled_not_plain() {
        let lines = render_markdown("# Hello", 40);
        let span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("Hello"))
            .expect("heading text");
        assert!(span.style.fg.is_some(), "heading should be coloured");
    }
}
