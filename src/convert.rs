//! Format conversion at the edges of the pipeline.
//!
//! Incoming: Teams HTML (or plain text) -> Markdown, so everything downstream deals with one format.
//! Outgoing: the editor's Markdown -> the HTML Graph expects, with `@mentions` and `:emoji:` resolved.

use chrono::{DateTime, Local, Utc};
use teams_client::{CodeBlockStyle, Member, Msg, OutMention, clean, html_to_plain};

/// Very long messages are cut before layout so one huge paste cannot stall rendering.
const MAX_RENDERED_CHARS: usize = 20_000;
use crate::highlight;
use crate::inline_style::{self, Format};
use htmd::element_handler::Handlers;
use htmd::{Element, HtmlToMarkdown};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd, html};

// ---------------------------------------------------------------------------
// Incoming
// ---------------------------------------------------------------------------

/// The value of the attribute `name` of `el`, if it has one.
fn attr(el: &Element<'_>, name: &str) -> Option<String> {
    el.node.attr(name).map(|v| v.to_string())
}

/// `ms`, milliseconds since the epoch (as Teams writes a reply quote's own `itemid`, see [`quote_time`]'s caller),
/// as the signed-in user's local date and time. Absolute, not relative ("Today"/"Yesterday" as the transcript's
/// own date dividers use): a quoted message has no divider of its own next to it to lean on, and this is also
/// what the real Teams client shows there.
fn quote_time(ms: &str) -> Option<String> {
    let ms: i64 = ms.parse().ok()?;
    let utc: DateTime<Utc> = DateTime::from_timestamp_millis(ms)?;
    Some(
        utc.with_timezone(&Local)
            .format("%-m/%-d/%Y %H:%M")
            .to_string(),
    )
}

/// The text of a `<codeblock>` body as the service sends it back: tokens wrapped in highlighting spans
/// (`<span class="hljs-string">`), line breaks as `<br>` and spaces as `&nbsp;`. Tags are dropped, `<br>` becomes a
/// newline and `&nbsp;` a space; other entities are left for the HTML parser, which decodes them. Code never holds a
/// raw `<` (it is `&lt;`), so every `<` here starts a tag.
fn codeblock_text(inner: &str) -> String {
    let mut text = String::with_capacity(inner.len());
    let mut rest = inner.trim_matches(['\r', '\n']);
    while let Some(open) = rest.find('<') {
        text.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[open + 1..open + close].trim_end_matches('/').trim();
        rest = &rest[open + close + 1..];
        if tag.eq_ignore_ascii_case("br") {
            text.push('\n');
            // The service writes a line break after its `<br>` (`<br>\r\nQuit`); that one is formatting, not a line.
            rest = rest
                .strip_prefix("\r\n")
                .or_else(|| rest.strip_prefix('\n'))
                .unwrap_or(rest);
        }
    }
    text.push_str(rest);
    text.replace("&nbsp;", " ")
        .replace('\u{a0}', " ")
        .replace('\r', "")
}

/// Teams wraps snippets in `<codeblock class="Rust"><code>..</code></codeblock>` (Graph documents the same element);
/// rewrite to the `<pre><code>` form the generic converter turns into a fenced block with a language.
fn normalize_codeblocks(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("<codeblock") {
        out.push_str(&rest[..start]);
        let Some(tag_end) = rest[start..].find('>') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let tag = &rest[start..start + tag_end];
        let lang = tag
            .split("class=\"")
            .nth(1)
            .and_then(|c| c.split('"').next())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
            .replace(' ', "-");
        let after = start + tag_end + 1;
        let Some(close) = rest[after..].find("</codeblock>") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &rest[after..after + close];
        let inner = codeblock_text(inner);
        let inner = inner.as_str();
        if lang.is_empty() || lang == "plaintext" {
            out.push_str(&format!("<pre><code>{inner}</code></pre>"));
        } else {
            out.push_str(&format!(
                "<pre><code class=\"language-{lang}\">{inner}</code></pre>"
            ));
        }
        rest = &rest[after + close + "</codeblock>".len()..];
    }
    out.push_str(rest);
    out
}

/// The service stores a code block in a language it does not know (ours: `objectscript`) as a bare `<code>` between
/// paragraphs, lines separated by `<br>`; the Teams client draws that as a code block. Inline `<code>` has no line
/// breaks, so a `<code>` outside `<pre>` that holds a `<br>` is rewritten to the `<pre><code>` form the converter
/// turns into a fenced block.
/// The language a start tag's `class` names (`language-rust`, or `Rust` as `<codeblock>` has it), if any. Teams' own
/// "plain" names mean no language.
fn class_language(tag: &str) -> Option<String> {
    let at = tag.find("class=")? + "class=".len();
    let quote = tag[at..]
        .chars()
        .next()
        .filter(|c| matches!(c, '"' | '\''))?;
    let value = tag[at + 1..].split(quote).next()?;
    let word = value
        .split_whitespace()
        .find_map(|w| w.strip_prefix("language-"))
        .or_else(|| value.split_whitespace().next())?
        .to_ascii_lowercase();
    (!matches!(word.as_str(), "" | "plaintext" | "text" | "none")
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+#-_.".contains(c)))
    .then_some(word)
}

/// Rewrites every `<pre>` to `<pre><code class="language-x">text</code></pre>` with the markup and non-breaking
/// spaces of the stored form taken out (see [`codeblock_text`]), and drops the Teams client's marker paragraph before
/// a code card (`<p itemtype=".../CodeBlockEditor">&nbsp;</p>`). The language comes from the `pre`'s class, as the
/// client writes it, or from the `code` inside it, as other clients do.
fn normalize_pre(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut at = 0;
    while at < html.len() {
        let next_pre = lower[at..].find("<pre").map(|i| at + i);
        let next_marker = lower[at..].find("codeblockeditor").map(|i| at + i);
        // A marker paragraph: from the `<p` that holds the word to the end of that paragraph.
        if let Some(m) = next_marker.filter(|m| next_pre.is_none_or(|p| *m < p)) {
            if let Some(open) = lower[at..m].rfind("<p").map(|i| at + i)
                && lower[open + 2..].starts_with(['>', ' '])
                && let Some(close) = lower[m..].find("</p>")
                && !lower[open..m].contains('>')
            {
                out.push_str(&html[at..open]);
                at = m + close + "</p>".len();
                continue;
            }
            // Not a marker we recognise: leave the word alone.
            out.push_str(&html[at..m + "codeblockeditor".len()]);
            at = m + "codeblockeditor".len();
            continue;
        }
        let Some(start) = next_pre else { break };
        let Some(tag_len) = html[start..].find('>') else {
            out.push_str(&html[at..]);
            return out;
        };
        let tag_end = start + tag_len + 1;
        let Some(close) = lower[tag_end..].find("</pre>") else {
            out.push_str(&html[at..]);
            return out;
        };
        let inner = &html[tag_end..tag_end + close];
        let lang = class_language(&html[start..tag_end]).or_else(|| {
            let code = inner.to_ascii_lowercase().find("<code")?;
            let end = inner[code..].find('>')?;
            class_language(&inner[code..code + end])
        });
        out.push_str(&html[at..start]);
        let text = codeblock_text(inner);
        match lang {
            Some(lang) => out.push_str(&format!(
                "<pre><code class=\"language-{lang}\">{text}</code></pre>"
            )),
            None => out.push_str(&format!("<pre><code>{text}</code></pre>")),
        }
        at = tag_end + close + "</pre>".len();
    }
    out.push_str(&html[at..]);
    out
}

fn normalize_bare_code(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    // How many `<pre>` elements are open at the start of `rest`.
    let mut in_pre = 0usize;
    while let Some(start) = rest.find('<') {
        let tag_end = rest[start..]
            .find('>')
            .map_or(rest.len(), |e| start + e + 1);
        let tag = &rest[start..tag_end];
        let name = tag
            .trim_start_matches('<')
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '/'))
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match name.as_str() {
            "pre" => in_pre += 1,
            "/pre" => in_pre = in_pre.saturating_sub(1),
            "code" if in_pre == 0 && tag.ends_with('>') => {
                if let Some(close) = rest[tag_end..].find("</code>") {
                    let inner = &rest[tag_end..tag_end + close];
                    if inner.to_ascii_lowercase().contains("<br") {
                        out.push_str(&rest[..start]);
                        out.push_str(&format!(
                            "<pre><code>{}</code></pre>",
                            codeblock_text(inner)
                        ));
                        rest = &rest[tag_end + close + "</code>".len()..];
                        continue;
                    }
                }
            }
            _ => {}
        }
        out.push_str(&rest[..tag_end]);
        rest = &rest[tag_end..];
    }
    out.push_str(rest);
    out
}

/// Teams' own colour for a mention of the signed-in user (`theme::MENTION_ME` in the app; no ratatui dependency
/// here, so the tuple is repeated rather than shared).
const MENTION_ME_COLOR: (u8, u8, u8) = (243, 139, 168);
/// A mention of someone other than the signed-in user: the same value as `theme::LINK` (no constant of its own
/// there; this colour has no other use in the app). The real Teams client shows these as a plain blue,
/// link-styled name with a hover profile card (no popup here, just the colour).
const MENTION_OTHER_COLOR: (u8, u8, u8) = (137, 180, 250);
/// The same muted grey a message's own header uses for its timestamp (`theme::DIM`), for a reply quote's time
/// (see [`quote_time`]) so it reads the same way there.
const DIM_COLOR: (u8, u8, u8) = (88, 91, 112);

/// Teams' own highlight for an @mention: bold, in a colour that tells a mention of the signed-in user (red) from
/// a mention of anyone else (blue, matching how the real client colours it). `id` is the mentioned user's bare
/// id when it is known (Teams web rewrites the small index Teams' own markup carries into this before `Msg.body`
/// ever reaches the app, `teams_client::teams::parse::normalize_mentions`; Graph does not yet, so a Graph
/// message's `id` is still that small index and never matches `me_id`, falling back to the "someone else"
/// colour — not wrong, just not as precise as Teams web). Via `inline_style` rather than CommonMark's `**..**`,
/// so a name that happens to contain Markdown metacharacters still renders correctly.
fn mention_markup(name: &str, id: Option<&str>, me_id: &str) -> String {
    let colour = if id == Some(me_id) {
        MENTION_ME_COLOR
    } else {
        MENTION_OTHER_COLOR
    };
    let format = Format {
        bold: true,
        fg: Some(colour),
        ..Format::default()
    };
    inline_style::wrap(format, &format!("@{}", name.trim().trim_start_matches('@')))
}

fn converter(me_id: &str) -> HtmlToMarkdown {
    // Owned so the `at` handler below, which `add_handler` requires to be `'static`, can hold it.
    let me_id = me_id.to_string();
    HtmlToMarkdown::builder()
        .skip_tags(vec![
            "script",
            "style",
            "noscript",
            "iframe",
            "svg",
            "attachment",
            "systemeventmessage",
        ])
        .add_handler(vec!["at"], move |h: &dyn Handlers, el: &Element<'_>| {
            let name = h.walk_children(el.node, el.context).content;
            Some(mention_markup(&name, attr(el, "id").as_deref(), &me_id).into())
        })
        // Formatting Markdown has no word for travels as markers in the text (see `inline_style`).
        .add_handler(vec!["u", "ins"], |h: &dyn Handlers, el: &Element<'_>| {
            let inner = h.walk_children(el.node, el.context).content;
            let format = Format {
                underline: true,
                ..Format::default()
            };
            Some(inline_style::wrap(format, &inner).into())
        })
        .add_handler(
            vec!["s", "strike", "del"],
            |h: &dyn Handlers, el: &Element<'_>| {
                let inner = h.walk_children(el.node, el.context).content;
                let format = Format {
                    strike: true,
                    ..Format::default()
                };
                Some(inline_style::wrap(format, &inner).into())
            },
        )
        .add_handler(vec!["mark"], |h: &dyn Handlers, el: &Element<'_>| {
            let inner = h.walk_children(el.node, el.context).content;
            let format = Format {
                bg: Some((255, 235, 59)),
                ..Format::default()
            };
            Some(inline_style::wrap(format, &inner).into())
        })
        .add_handler(
            vec!["span", "font"],
            |h: &dyn Handlers, el: &Element<'_>| {
                // A reply quote's own timestamp (`<blockquote itemtype=".../Reply">`): Teams writes it as an empty
                // `<span itemprop="time" itemid="<ms since the epoch>">`, with the real client filling in readable
                // text at render time rather than sending any; nothing else gives this element a reason to exist.
                if attr(el, "itemprop").as_deref() == Some("time") {
                    if let Some(t) = attr(el, "itemid").as_deref().and_then(quote_time) {
                        let format = Format {
                            fg: Some(DIM_COLOR),
                            ..Format::default()
                        };
                        return Some(format!("  {}", inline_style::wrap(format, &t)).into());
                    }
                    return h.fallback(el);
                }
                let inner = h.walk_children(el.node, el.context).content;
                let mut format = attr(el, "style")
                    .as_deref()
                    .map(inline_style::parse_style)
                    .unwrap_or_default();
                if let Some(colour) = attr(el, "color")
                    .as_deref()
                    .and_then(inline_style::parse_colour)
                {
                    format.fg = Some(colour);
                }
                Some(inline_style::wrap(format, &inner).into())
            },
        )
        .add_handler(vec!["emoji"], |_: &dyn Handlers, el: &Element<'_>| {
            Some(
                attr(el, "alt")
                    .as_deref()
                    .or(attr(el, "title").as_deref())
                    .unwrap_or("")
                    .to_string()
                    .into(),
            )
        })
        .add_handler(vec!["img"], |h: &dyn Handlers, el: &Element<'_>| {
            // Teams renders emoji/stickers as <img> pointing at its emoticon CDN; keep their text form.
            let is_emoji = attr(el, "itemtype")
                .as_deref()
                .is_some_and(|t| t.contains("Emoji"))
                || attr(el, "src")
                    .as_deref()
                    .is_some_and(|s| s.contains("/emoticons/"));
            if is_emoji && let Some(alt) = attr(el, "alt").as_deref().filter(|a| !a.is_empty()) {
                return Some(alt.to_string().into());
            }
            h.fallback(el)
        })
        .build()
}

/// Elements nested deeper than this are not chat content. The converter below recurses once per level, and a stack
/// overflow cannot be caught, so such a message is shown as plain text instead.
const MAX_HTML_DEPTH: usize = 100;

/// A message with more start tags than this is shown as plain text. The nesting check above reads tags the way a
/// person would; the HTML parser reads them differently (an empty comment `<!-->`, stray end tags), so it can be
/// fooled. The number of start tags cannot: nesting is never deeper than the count of tags that open an element, and
/// counting them needs no knowledge of how the parser pairs them.
const MAX_HTML_ELEMENTS: usize = 3_000;

/// Stack for the converter thread. A level of nesting costs well under 10 kB even in a debug build, so this holds
/// [`MAX_HTML_ELEMENTS`] levels many times over; the memory is reserved, not used, until a message needs it.
const CONVERT_STACK: usize = 64 * 1024 * 1024;

/// Elements that never have a closing tag.
const VOID_ELEMENTS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Whether `html` nests elements more than [`MAX_HTML_DEPTH`] deep. A single flat pass over the tags, so it cannot be
/// made to recurse itself; unclosed tags count as nested, which only matters for absurd input.
fn nests_too_deeply(html: &str) -> bool {
    let mut depth = 0usize;
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        rest = &rest[open + 1..];
        if let Some(comment) = rest.strip_prefix("!--") {
            let Some(end) = comment.find("-->") else {
                return false;
            };
            rest = &comment[end + 3..];
            continue;
        }
        let closing = rest.starts_with('/');
        let name_start = usize::from(closing);
        let name_len = rest[name_start..]
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(rest.len() - name_start);
        if name_len == 0 {
            continue; // a `<` that does not start a tag
        }
        let name = rest[name_start..name_start + name_len].to_ascii_lowercase();
        let Some(end) = rest.find('>') else {
            return false; // no tag can end after this point
        };
        let self_closing = rest[..end].ends_with('/');
        if closing {
            depth = depth.saturating_sub(1);
        } else if !self_closing && !VOID_ELEMENTS.contains(&name.as_str()) {
            depth += 1;
            if depth > MAX_HTML_DEPTH {
                return true;
            }
        }
        rest = &rest[end + 1..];
    }
    false
}

/// Whether `html` has more than `max` places where an element can start (`<` and a letter), counted without regard to
/// comments or end tags.
fn has_more_elements_than(html: &str, max: usize) -> bool {
    html.as_bytes()
        .windows(2)
        .filter(|w| w[0] == b'<' && w[1].is_ascii_alphabetic())
        .nth(max)
        .is_some()
}

/// Teams HTML -> Markdown.
///
/// The conversion runs on its own thread with a large stack and a panic in it is contained, so a message that defeats
/// the checks above still cannot take the interface down.
pub fn html_to_markdown(html: &str, me_id: &str) -> String {
    // Our own markers are added below; none may arrive from the network.
    let html = &inline_style::strip(&inline_style::strip_references(html));
    if nests_too_deeply(html) || has_more_elements_than(html, MAX_HTML_ELEMENTS) {
        return tidy(&html_to_plain(html));
    }
    let html = normalize_bare_code(&normalize_pre(&normalize_codeblocks(html)));
    let converted = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("vtteams-convert".into())
            .stack_size(CONVERT_STACK)
            .spawn_scoped(scope, || {
                converter(me_id).convert(&html).map_err(|e| e.to_string())
            })
            .map_err(|e| e.to_string())
            .and_then(|handle| {
                handle
                    .join()
                    .map_err(|_| "the converter panicked".to_string())
            })
    });
    let md = match converted {
        Ok(Ok(md)) => md,
        Ok(Err(e)) | Err(e) => {
            // Show the text without its formatting rather than nothing, and leave a trace for whoever wonders why.
            log::warn!(
                "could not convert a message from HTML ({} bytes, {e}); showing it as plain text",
                html.len()
            );
            html_to_plain(&html)
        }
    };
    tidy(&md)
}

/// Collapse runs of blank lines and trim trailing spaces outside code fences; a fence's own blank lines are kept
/// exactly as sent, since two blank lines between (say) two Python function definitions are meaningful there.
pub fn tidy(md: &str) -> String {
    let md = clean(md);
    let mut out = String::with_capacity(md.len());
    let mut blank = 0;
    let mut fence: Option<crate::fence::Fence> = None;
    for line in md.lines() {
        let line = line.trim_end();
        match fence {
            Some(open) if crate::fence::fence_closes(line, open) => fence = None,
            None => {
                if let Some((open, _)) = crate::fence::fence_marker(line) {
                    fence = Some(open);
                }
            }
            Some(_) => {}
        }
        let narrowed;
        let line = if fence.is_some() {
            line
        } else {
            narrowed = narrow_list_marker(line);
            narrowed.as_str()
        };
        if line.is_empty() {
            if fence.is_none() {
                blank += 1;
                if blank > 1 {
                    continue;
                }
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

/// htmd pads list markers (`*   item`, `1.  item`); chat is narrow, so use a single space.
fn narrow_list_marker(line: &str) -> String {
    let body = line.trim_start();
    let indent = line.len() - body.len();
    let marker_len = if body.starts_with(['*', '-', '+']) {
        1
    } else {
        let digits = body.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && body[digits..].starts_with('.') {
            digits + 1
        } else {
            0
        }
    };
    if marker_len > 0 && body[marker_len..].starts_with("  ") {
        format!(
            "{}{} {}",
            &line[..indent],
            &body[..marker_len],
            body[marker_len..].trim_start()
        )
    } else {
        line.to_string()
    }
}

/// Escape characters that would be misread as Markdown in a plain-text message.
fn plain_to_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in inline_style::strip(text).replace("\r\n", "\n").chars() {
        if matches!(c, '*' | '_' | '`' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Strip style markers and neutralise Markdown metacharacters from a name the service sends before it is spliced into
/// Markdown generated here (an attachment or card name): `name` only ever went through `teams_client::clean`, which
/// filters control and bidi characters but not a marker `inline_style::strip` would remove, nor anything Markdown
/// gives meaning to. This is also used as a link label (`[name](url)`); `render::number_links`' bracket search has no
/// notion of an escaped `[`/`]` (unlike the parenthesis search `escape_link_destination` feeds), so a literal one in
/// the name, followed by the destination's own `](`, would still read as the start of a second, attacker-chosen link.
/// The ASCII brackets are swapped for fullwidth look-alikes instead, which keep the name legible but are invisible to
/// that search.
fn escape_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in inline_style::strip(name).chars() {
        match c {
            '*' | '_' | '`' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '[' => out.push('\u{FF3B}'),
            ']' => out.push('\u{FF3D}'),
            _ => out.push(c),
        }
    }
    out
}

/// Escape a literal `(` or `)` in a link destination we construct, the same way htmd escapes one in a destination it
/// converts from HTML (`escape_link_destination` in its anchor handler), so `render::number_links`' escape-aware
/// search for the closing paren (which has to cope with htmd's output either way) parses either source the same way.
/// A Microsoft-hosted attachment URL routinely contains a parenthesised duplicate-name suffix.
fn escape_link_destination(url: &str) -> String {
    if !url.contains(['(', ')']) {
        return url.to_string();
    }
    let mut escaped = String::with_capacity(url.len());
    for c in url.chars() {
        match c {
            '(' => escaped.push_str("\\("),
            ')' => escaped.push_str("\\)"),
            _ => escaped.push(c),
        }
    }
    escaped
}

/// Whether an attachment is a picture: Graph's own MIME-style `content_type` (`image/png`), or the bare extension
/// Teams web reports instead (`Attachment::content_type` is documented as "content type **or file extension**, as
/// the service reports it" for exactly this reason — a screenshot pasted into Teams web is uploaded to the
/// sender's OneDrive and arrives as a file attachment, `properties.files[].fileType`, not an inline `<img>`).
/// Limited to the formats `image`'s own enabled features (`Cargo.toml`) can actually decode, so this agrees with
/// what a successful fetch would produce.
fn is_image_attachment(content_type: &str) -> bool {
    content_type.starts_with("image/")
        || matches!(
            content_type.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "webp"
        )
}

/// The Markdown for one message, as it is drawn: subject, body, then file attachments, cut at
/// `MAX_RENDERED_CHARS` with a note saying so.
pub fn message_markdown(m: &Msg, me_id: &str) -> String {
    shorten(message_markdown_in_full(m, me_id))
}

/// `md` cut to `MAX_RENDERED_CHARS` characters, with a note. A cut that lands inside a fenced block closes the fence
/// first, or the note would be drawn as code.
fn shorten(md: String) -> String {
    if md.chars().count() <= MAX_RENDERED_CHARS {
        return md;
    }
    let mut cut: String = md.chars().take(MAX_RENDERED_CHARS).collect();
    let mut fence: Option<crate::fence::Fence> = None;
    for line in cut.lines() {
        match fence {
            Some(open) if crate::fence::fence_closes(line, open) => fence = None,
            None => fence = crate::fence::fence_marker(line).map(|(open, _)| open),
            Some(_) => {}
        }
    }
    if let Some(open) = fence {
        cut.push('\n');
        cut.push_str(&open.closer());
    }
    format!("{cut}\n\n*… message truncated*")
}

/// The whole Markdown of one message, however long: what copying it should give. Drawing uses `message_markdown`.
pub fn message_markdown_in_full(m: &Msg, me_id: &str) -> String {
    if m.deleted {
        return "*This message was deleted.*".into();
    }
    let mut md = String::new();
    if let Some(subject) = &m.subject {
        // One line, with nothing in it Markdown reads: the subject is sent by whoever wrote the message and is put
        // inside `**..**`, so a `**`, a line break or a fence marker in it would otherwise change the rest of the
        // message, and a `[text](address)` would be a link of their choosing.
        let subject = escape_name(subject)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !subject.is_empty() {
            md.push_str(&format!("**{subject}**\n\n"));
        }
    }
    let body = if m.html {
        html_to_markdown(&m.body, me_id)
    } else {
        plain_to_markdown(&m.body)
    };
    md.push_str(&body);
    // Every attachment but a card (handled separately below, by whichever backend sends one that way): a video,
    // PDF, Word document or source file, Graph's own "reference" shape or Teams web's bare file extension alike,
    // all the same except for whether it is a picture.
    for a in m
        .attachments
        .iter()
        .filter(|a| !a.content_type.contains("card"))
    {
        let name = if a.name.is_empty() {
            "attachment".to_string()
        } else {
            escape_name(&a.name)
        };
        let picture = is_image_attachment(&a.content_type);
        match &a.url {
            // A picture becomes a real image reference, so it is drawn as block art (or, on a host the backend does
            // not serve images from, the same "external, not loaded" placeholder any other untrusted picture gets)
            // instead of a plain file link.
            Some(u) if picture => {
                md.push_str(&format!("\n\n![{name}]({})", escape_link_destination(u)));
            }
            Some(u) => md.push_str(&format!("\n\n📎 [{name}]({})", escape_link_destination(u))),
            None => md.push_str(&format!("\n\n📎 {name}")),
        }
    }
    for a in m
        .attachments
        .iter()
        .filter(|a| a.content_type.contains("card"))
    {
        let name = if a.name.is_empty() {
            "card".to_string()
        } else {
            escape_name(&a.name)
        };
        md.push_str(&format!("\n\n▤ *{name} (interactive card, open in Teams)*"));
    }
    tidy(&md)
}

// ---------------------------------------------------------------------------
// Outgoing
// ---------------------------------------------------------------------------

/// `:shortcode:` names and their emoji: what typing `:name:` expands to, and what the emoji selector offers.
pub(crate) const EMOJI: &[(&str, &str)] = &[
    ("smile", "😄"),
    ("smiley", "😃"),
    ("rofl", "🤣"),
    ("innocent", "😇"),
    ("relieved", "😌"),
    ("sleeping", "😴"),
    ("confused", "😕"),
    ("grimacing", "😬"),
    ("hugging", "🤗"),
    ("nerd_face", "🤓"),
    ("partying_face", "🥳"),
    ("open_mouth", "😮"),
    ("slightly_frowning_face", "🙁"),
    ("raised_hands", "🙌"),
    ("handshake", "🤝"),
    ("point_up", "☝️"),
    ("point_right", "👉"),
    ("point_left", "👈"),
    ("v", "✌️"),
    ("fist", "👊"),
    ("zzz", "💤"),
    ("rainbow", "🌈"),
    ("grin", "😁"),
    ("joy", "😂"),
    ("laughing", "😆"),
    ("wink", "😉"),
    ("blush", "😊"),
    ("heart_eyes", "😍"),
    ("sunglasses", "😎"),
    ("thinking", "🤔"),
    ("neutral_face", "😐"),
    ("cry", "😢"),
    ("sob", "😭"),
    ("angry", "😠"),
    ("scream", "😱"),
    ("sweat_smile", "😅"),
    ("slightly_smiling_face", "🙂"),
    ("upside_down_face", "🙃"),
    ("facepalm", "🤦"),
    ("shrug", "🤷"),
    ("pray", "🙏"),
    ("clap", "👏"),
    ("wave", "👋"),
    ("thumbsup", "👍"),
    ("+1", "👍"),
    ("thumbsdown", "👎"),
    ("-1", "👎"),
    ("ok_hand", "👌"),
    ("muscle", "💪"),
    ("eyes", "👀"),
    ("heart", "❤️"),
    ("broken_heart", "💔"),
    ("fire", "🔥"),
    ("star", "⭐"),
    ("sparkles", "✨"),
    ("tada", "🎉"),
    ("rocket", "🚀"),
    ("bug", "🐛"),
    ("warning", "⚠️"),
    ("check", "✅"),
    ("white_check_mark", "✅"),
    ("x", "❌"),
    ("question", "❓"),
    ("100", "💯"),
    ("coffee", "☕"),
    ("pizza", "🍕"),
    ("beer", "🍺"),
    ("bulb", "💡"),
    ("memo", "📝"),
    ("lock", "🔒"),
    ("hourglass", "⌛"),
    ("crab", "🦀"),
];

/// Replace `:shortcode:` with the emoji, leaving code (fenced blocks and inline spans) untouched. A shortcode has to
/// stand on its own: `ns::x::fn` or `a:x:b` are not written with one.
pub fn expand_emoji(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<crate::fence::Fence> = None;
    for line in text.split_inclusive('\n') {
        if let Some(open) = fence {
            if crate::fence::fence_closes(line, open) {
                fence = None;
            }
            out.push_str(line);
        } else if let Some((open, _)) = crate::fence::fence_marker(line) {
            fence = Some(open);
            out.push_str(line);
        } else {
            expand_line(line, &mut out);
        }
    }
    out
}

/// One line of prose: the pieces between backticks alternate prose and inline code.
fn expand_line(line: &str, out: &mut String) {
    for (i, seg) in line.split('`').enumerate() {
        if i > 0 {
            out.push('`');
        }
        if i % 2 == 1 {
            out.push_str(seg);
            continue;
        }
        let mut rest = seg;
        // Whether what was written last is an emoji, which a shortcode may directly follow.
        let mut after_emoji = false;
        while let Some(open) = rest.find(':') {
            out.push_str(&rest[..open]);
            if open > 0 {
                after_emoji = false;
            }
            let tail = &rest[open + 1..];
            let before_ok = after_emoji
                || out
                    .chars()
                    .last()
                    .is_none_or(|c| !c.is_alphanumeric() && c != ':');
            let hit = before_ok
                .then(|| {
                    let close = tail.find(':')?;
                    let name = &tail[..close];
                    let after_ok = tail[close + 1..]
                        .chars()
                        .next()
                        .is_none_or(|c| !c.is_alphanumeric());
                    EMOJI
                        .iter()
                        .find(|(n, _)| *n == name && after_ok)
                        .map(|(_, e)| (*e, close))
                })
                .flatten();
            match hit {
                Some((e, close)) => {
                    out.push_str(e);
                    rest = &tail[close + 1..];
                    after_emoji = true;
                }
                None => {
                    out.push(':');
                    rest = tail;
                    after_emoji = false;
                }
            }
        }
        out.push_str(rest);
    }
}
/// Marks a typed `@name` while mentions are being matched against the roster, removed again (as a real mention or
/// put back as plain text) before the Markdown goes anywhere else. In the Supplementary Private Use Area-A, clear of
/// the Basic Multilingual Plane's Private Use Area that `inline_style`'s own markers reserve in full
/// (`inline_style::is_marker`, checked against these two in a test below): the two mechanisms must never be able to
/// mistake the other's marker for its own.
const MENTION_OPEN: char = '\u{F0000}';
const MENTION_CLOSE: char = '\u{F0001}';

/// `text` without a leading `prefix`, compared case-insensitively one character at a time, so the returned slice
/// always starts on a character boundary of `text` (lowercasing can change a string's length, so offsets measured on a
/// lowercased copy must never be applied to the original).
fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let mut chars = text.char_indices();
    for p in prefix.chars() {
        let (_, c) = chars.next()?;
        if !c.to_lowercase().eq(p.to_lowercase()) {
            return None;
        }
    }
    Some(chars.next().map_or("", |(i, _)| &text[i..]))
}

/// A display name as a mention carries it and as a typed `@name` is matched against it: the mention markers taken out
/// (a name must never be able to write one into the text) and every run of whitespace, line breaks and tabs included,
/// one space, so a name that arrived with a stray line break can still be typed.
pub(crate) fn mention_name(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(*c, MENTION_OPEN | MENTION_CLOSE))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A person a typed `@name` can mean: the name that is matched (a full display name, or a first name that only one
/// person has), who it is, and whether another person has the same name.
struct Candidate<'a> {
    name: String,
    member: &'a Member,
    ambiguous: bool,
}

/// Replace `@Name` references in one piece of prose (no code) with placeholders, recording each mention. A name that
/// two people have is not a mention of either: it stays as typed and is added to `ambiguous` (once).
fn scan_mentions(
    text: &str,
    cands: &[Candidate<'_>],
    out: &mut String,
    mentions: &mut Vec<OutMention>,
    ambiguous: &mut Vec<String>,
    (base, found): (usize, &mut Vec<std::ops::Range<usize>>),
) {
    let mut rest = text;
    'scan: while let Some(at) = rest.find('@') {
        let (before, tail) = rest.split_at(at);
        let boundary_ok = before.chars().last().is_none_or(|c| !c.is_alphanumeric());
        if boundary_ok {
            for Candidate {
                name,
                member,
                ambiguous: shared,
            } in cands
            {
                let Some(after) = strip_prefix_ignore_case(&tail[1..], name) else {
                    continue;
                };
                if after.chars().next().is_some_and(char::is_alphanumeric) {
                    continue; // "@Adam" is not a mention of "Ada"
                }
                if *shared {
                    // Whichever of them was meant, guessing could ping the wrong person; the shorter names that are
                    // also a prefix of this one are not tried either.
                    if !ambiguous
                        .iter()
                        .any(|n| n.to_lowercase() == name.to_lowercase())
                    {
                        ambiguous.push(name.clone());
                    }
                    break;
                }
                let id = match mentions.iter().find(|m| m.user_id == member.id) {
                    Some(m) => m.id,
                    None => {
                        let id = u32::try_from(mentions.len()).unwrap_or(u32::MAX);
                        mentions.push(OutMention {
                            id,
                            user_id: member.id.clone(),
                            name: mention_name(&member.name),
                        });
                        id
                    }
                };
                out.push_str(before);
                out.push(MENTION_OPEN);
                out.push_str(&id.to_string());
                out.push(MENTION_CLOSE);
                // Where "@Name" was, as a byte range of the whole text.
                let start = base + (text.len() - rest.len()) + at;
                found.push(start..base + (text.len() - after.len()));
                rest = after;
                continue 'scan;
            }
        }
        out.push_str(before);
        out.push('@');
        rest = &tail[1..];
    }
    out.push_str(rest);
}

/// Where, in one line of prose, an address is: a Markdown link's destination (the part of `[label](destination)`
/// after the bracket, parentheses inside it balanced, `\` escaping), and a bare `http://`, `https://` or `mailto:`
/// address up to the next space or `>`. Sorted, not overlapping, on character boundaries.
fn address_spans(s: &str) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut from = 0;
    while let Some(found) = s[from..].find("](") {
        let start = from + found + 2;
        let mut depth = 1;
        let mut end = s.len();
        let mut chars = s[start..].char_indices();
        while let Some((k, c)) = chars.next() {
            match c {
                '\\' => {
                    chars.next();
                }
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + k;
                        break;
                    }
                }
                _ => {}
            }
        }
        spans.push((start, end));
        from = end;
    }
    // Lower-casing ASCII letters keeps every byte offset.
    let lower = s.to_ascii_lowercase();
    for scheme in ["http://", "https://", "mailto:"] {
        let mut from = 0;
        while let Some(found) = lower[from..].find(scheme) {
            let start = from + found;
            let end = s[start..]
                .find(|c: char| c.is_whitespace() || c == '>')
                .map_or(s.len(), |k| start + k);
            if s[..start]
                .chars()
                .last()
                .is_none_or(|c| !c.is_alphanumeric())
            {
                spans.push((start, end));
            }
            from = end.max(start + scheme.len());
        }
    }
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}
/// Find `@Name` references to chat members. Returns the text with each replaced by a placeholder, and the mentions.
/// Fenced code and inline code are left alone.
///
/// A full name that two different people have (the same name, ignoring case, for different ids) is not a mention of
/// either: it is left as typed, and the third answer lists each such name that was typed, once.
fn extract_mentions(
    md: &str,
    members: &[Member],
    me_id: &str,
) -> (String, Vec<OutMention>, Vec<String>) {
    let (out, mentions, ambiguous, _) = find_mentions(md, members, me_id);
    (out, mentions, ambiguous)
}

/// Where in `md` the `@Name`s are that sending it would make mentions of `members` (byte ranges, in order): the one rule
/// the message box colours by, so what is drawn as a mention while typing is what is sent as one.
pub(crate) fn mention_ranges(
    md: &str,
    members: &[Member],
    me_id: &str,
) -> Vec<std::ops::Range<usize>> {
    find_mentions(md, members, me_id).3
}

/// [`extract_mentions`], also saying where each mention was found.
fn find_mentions(
    md: &str,
    members: &[Member],
    me_id: &str,
) -> (
    String,
    Vec<OutMention>,
    Vec<String>,
    Vec<std::ops::Range<usize>>,
) {
    // Candidates: full display names, plus unique first names. Longest first so "@Ada Lovelace" beats "@Ada".
    let names: Vec<String> = members.iter().map(|m| mention_name(&m.name)).collect();
    let mut cands: Vec<Candidate<'_>> = Vec::new();
    for (m, name) in members
        .iter()
        .zip(&names)
        .filter(|(m, name)| m.id != me_id && !name.is_empty())
    {
        let shared = members.iter().zip(&names).any(|(o, other)| {
            o.id != m.id && other.to_lowercase() == name.to_lowercase() && o.id != me_id
        });
        cands.push(Candidate {
            name: name.clone(),
            member: m,
            ambiguous: shared,
        });
        let first = name.split_whitespace().next().unwrap_or("");
        let unique = names
            .iter()
            .filter(|o| {
                o.split_whitespace().next().map(|f| f.to_lowercase()) == Some(first.to_lowercase())
            })
            .count()
            == 1;
        if unique && first != name {
            cands.push(Candidate {
                name: first.to_string(),
                member: m,
                ambiguous: false,
            });
        }
    }
    cands.sort_by_key(|c| std::cmp::Reverse(c.name.chars().count()));

    let mut out = String::with_capacity(md.len());
    let mut mentions: Vec<OutMention> = Vec::new();
    let mut ambiguous: Vec<String> = Vec::new();
    let mut found: Vec<std::ops::Range<usize>> = Vec::new();
    let mut fence: Option<crate::fence::Fence> = None;
    let mut line_at = 0;
    for line in md.split_inclusive('\n') {
        let line_start = line_at;
        line_at += line.len();
        if let Some(open) = fence
            && crate::fence::fence_closes(line, open)
        {
            fence = None;
            out.push_str(line);
            continue;
        }
        if fence.is_none()
            && let Some((open, _)) = crate::fence::fence_marker(line)
        {
            fence = Some(open);
            out.push_str(line);
            continue;
        }
        if fence.is_some() || !line.contains('@') {
            out.push_str(line);
            continue;
        }
        // The odd-numbered pieces between backticks are inline code.
        let mut piece_at = line_start;
        for (i, piece) in line.split('`').enumerate() {
            if i > 0 {
                out.push('`');
                piece_at += 1;
            }
            let piece_start = piece_at;
            piece_at += piece.len();
            if i % 2 == 1 {
                out.push_str(piece);
            } else {
                // An address is not a place to find people: a link's destination or a bare URL that contains `@Ada`
                // is copied as it is.
                let mut done = 0;
                for (from, to) in address_spans(piece) {
                    scan_mentions(
                        &piece[done..from],
                        &cands,
                        &mut out,
                        &mut mentions,
                        &mut ambiguous,
                        (piece_start + done, &mut found),
                    );
                    out.push_str(&piece[from..to]);
                    done = to;
                }
                scan_mentions(
                    &piece[done..],
                    &cands,
                    &mut out,
                    &mut mentions,
                    &mut ambiguous,
                    (piece_start + done, &mut found),
                );
            }
        }
    }
    (out, mentions, ambiguous, found)
}
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Replaces each fenced or indented code block in `events` with one raw `<codeblock>` element, the markup both
/// Graph and the Teams web service use for code (Graph documents it for sending and reading; see [`codeblock_html`]).
fn teams_code_blocks<'a>(
    events: impl Iterator<Item = Event<'a>>,
    style: CodeBlockStyle,
) -> Vec<Event<'a>> {
    let mut out = Vec::new();
    let mut open: Option<(String, String)> = None;
    for ev in events {
        match (&mut open, ev) {
            (None, Event::Start(Tag::CodeBlock(kind))) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                open = Some((lang, String::new()));
            }
            (Some((lang, code)), Event::End(TagEnd::CodeBlock)) => {
                out.push(Event::Html(codeblock_html(lang, code, style).into()));
                open = None;
            }
            (Some((_, code)), Event::Text(text)) => code.push_str(&text),
            (Some(_), _) => {}
            (None, other) => out.push(other),
        }
    }
    out
}

/// A random GUID, for tying a code card's marker to its `pre`. Not a secret: it only has to differ between blocks.
fn guid() -> String {
    use std::hash::{BuildHasher, Hasher};
    let state = std::collections::hash_map::RandomState::new();
    let part = |salt: u8| {
        let mut h = state.build_hasher();
        h.write_u8(salt);
        h.finish()
    };
    let (a, b) = (part(1), part(2));
    format!(
        "{:08x}-{:04x}-4{:03x}-{:x}{:03x}-{:012x}",
        a >> 32,
        (a >> 16) & 0xffff,
        a & 0xfff,
        8 + (b >> 60) % 4,
        (b >> 48) & 0xfff,
        b & 0xffff_ffff_ffff
    )
}

/// The name the Teams client's language menu uses for a fence label, when the label is a common short form of it
/// (`ts`, `py`, `sh`, ...); any other label is returned as it is. The card's header shows the language only for the
/// names in the menu, so `ts` would otherwise read "Text".
fn teams_language(label: &str) -> &str {
    match label {
        "js" => "javascript",
        "ts" | "tsx" => "typescript",
        "py" => "python",
        "yml" => "yaml",
        "sh" | "zsh" => "shell",
        "golang" => "go",
        "cs" | "c#" => "csharp",
        "c++" | "cc" | "cxx" => "cpp",
        "docker" => "dockerfile",
        "bat" | "batch" | "cmd" => "dos",
        "gql" => "graphql",
        "kt" => "kotlin",
        "objc" | "objective-c" => "objectivec",
        "ps1" => "powershell",
        "rb" => "ruby",
        "rs" => "rust",
        "vb" | "vb.net" => "vbnet",
        "vbs" => "vbscript",
        "md" => "markdown",
        "text" | "txt" => "plaintext",
        other => other,
    }
}

/// Longest code that is coloured for Teams, and longest coloured body sent (the markup of the colours counts).
const MAX_COLOURED_CODE: usize = 8_000;
const MAX_CODE_BODY: usize = 20_000;

/// `code` as the body of a Teams code block: text escaped, spaces `&nbsp;`, lines separated by `<br>`, and each of
/// `ranges` (sorted, not overlapping, on character boundaries) wrapped in `<span class="hljs-..">`. A span never
/// crosses a line: highlight.js closes and reopens it, and so does this.
fn code_body(code: &str, ranges: &[highlight::ClassRange]) -> String {
    let mut body = String::with_capacity(code.len() + code.len() / 4);
    let mut open: Option<&str> = None;
    let mut next = 0;
    for (i, c) in code.char_indices() {
        while ranges.get(next).is_some_and(|r| r.end <= i) {
            next += 1;
        }
        let class = ranges
            .get(next)
            .filter(|r| r.start <= i && i < r.end && c != '\n')
            .map(|r| r.class);
        if open != class {
            if open.is_some() {
                body.push_str("</span>");
            }
            if let Some(class) = class {
                // The class is what the Teams client writes; the colour is ours (see `highlight::class_colour`).
                match highlight::class_colour(class) {
                    Some(colour) => body.push_str(&format!(
                        "<span class=\"hljs-{class}\" style=\"color:{colour}\">"
                    )),
                    None => body.push_str(&format!("<span class=\"hljs-{class}\">")),
                }
            }
            open = class;
        }
        match c {
            '&' => body.push_str("&amp;"),
            '<' => body.push_str("&lt;"),
            '>' => body.push_str("&gt;"),
            ' ' => body.push_str("&nbsp;"),
            '\t' => body.push_str("&nbsp;&nbsp;&nbsp;&nbsp;"),
            '\r' => {}
            '\n' => body.push_str("<br>"),
            c => body.push(c),
        }
    }
    if open.is_some() {
        body.push_str("</span>");
    }
    body
}

/// A code block as the service wants it, for the fence''s first word `lang` and the code `code`.
///
/// [`CodeBlockStyle::Codeblock`] is `<codeblock class="Json"><code>..</code></codeblock>`, which Graph documents both
/// for sending (`class="plaintext"` without a language) and as read back, where the service has turned line breaks
/// into `<br>` and spaces into `&nbsp;` (and wrapped tokens in `hljs-*` spans).
///
/// [`CodeBlockStyle::EditorCard`] is what the Teams web client writes, seen in a message it sent and confirmed by
/// sending the same structure: an empty marker paragraph and a `<pre class="language-rust">` that name each other with
/// a GUID. The client draws that as a card with the language in its header, line numbers and a copy button, and the
/// service keeps it as it is. The `<codeblock>` form is not kept: the service rewrites it to a bare `<code>` and drops
/// the class, so the language is lost and the client draws a plain box. The client adds the colour spans (`hljs-*`)
/// itself when it composes; they are not needed to be drawn as a card and are not added here.
///
/// Either way the language is the fence''s first word, as typed (lower-case): a language Teams does not know, such as
/// `objectscript`, is carried for vtteams readers, and the client shows it as "Text".
fn codeblock_html(lang: &str, code: &str, style: CodeBlockStyle) -> String {
    let lang: String = lang
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '#' | '-' | '_' | '.'))
        .take(32)
        .collect::<String>()
        .to_ascii_lowercase();
    let lang = if lang.is_empty() {
        "plaintext"
    } else {
        teams_language(&lang)
    };
    let code = code.replace("\r\n", "\n");
    let code = code.strip_suffix('\n').unwrap_or(&code);
    // Colour is for the card only, and only for code of a size that cannot swell the message.
    let ranges = if style == CodeBlockStyle::EditorCard
        && lang != "plaintext"
        && code.len() <= MAX_COLOURED_CODE
    {
        highlight::hljs_ranges(code, lang)
    } else {
        Vec::new()
    };
    let mut body = code_body(code, &ranges);
    if body.len() > MAX_CODE_BODY {
        body = code_body(code, &[]);
    }
    match style {
        CodeBlockStyle::EditorCard => {
            let id = guid();
            format!(
                "<p itemtype=\"http://schema.skype.com/CodeBlockEditor\" id=\"x_codeBlockEditor-{id}\">&nbsp;</p>\
                 <pre class=\"language-{lang}\" itemid=\"codeBlockEditor-{id}\"><code>{body}</code></pre>"
            )
        }
        _ => format!("<codeblock class=\"{lang}\"><code>{body}</code></codeblock>"),
    }
}
/// The tests' form of [`markdown_to_html_noting`], for the many that have no use for the names left as text.
#[cfg(test)]
pub fn markdown_to_html(
    md: &str,
    members: &[Member],
    me_id: &str,
    style: CodeBlockStyle,
) -> (String, Vec<OutMention>) {
    let (html, mentions, _) = markdown_to_html_noting(md, members, me_id, style);
    (html, mentions)
}

/// Markdown -> HTML for Graph and Teams. Single newlines become `<br>` (chat semantics), `@Name` becomes `<at>` tags,
/// and raw HTML in the input is escaped rather than passed through.
///
/// Also returns the names that were typed as `@name` but belong to more than one person (each once): those are left as
/// plain text, so the caller can say so. Any mention marker in the typed text is taken out first, so the text can never
/// write one.
pub fn markdown_to_html_noting(
    md: &str,
    members: &[Member],
    me_id: &str,
    style: CodeBlockStyle,
) -> (String, Vec<OutMention>, Vec<String>) {
    let md = expand_emoji(md.trim());
    let md = md.replace([MENTION_OPEN, MENTION_CLOSE], "");
    let (md, mentions, ambiguous) = extract_mentions(&md, members, me_id);
    let parser = Parser::new_ext(
        &md,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS,
    )
    .map(|ev| match ev {
        Event::SoftBreak => Event::HardBreak,
        // Raw HTML in what the user typed is text, not markup: `Vec<String>` must arrive as written, and a pasted
        // `<script>` must not be sent as a tag. Rich formatting comes from Markdown, never from raw HTML.
        Event::Html(s) | Event::InlineHtml(s) => Event::Text(s),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, teams_code_blocks(parser, style).into_iter());
    let mut out = out.trim().to_string();
    for m in &mentions {
        let marker = format!("{MENTION_OPEN}{}{MENTION_CLOSE}", m.id);
        out = out.replace(
            &marker,
            &format!("<at id=\"{}\">{}</at>", m.id, escape_html(&m.name)),
        );
    }
    (out, mentions, ambiguous)
}

#[cfg(test)]
mod tests {
    use super::*;
    use teams_client::Attachment;

    #[test]
    fn mention_markers_are_disjoint_from_inline_style_markers() {
        assert!(!inline_style::is_marker(MENTION_OPEN));
        assert!(!inline_style::is_marker(MENTION_CLOSE));
    }

    fn member(id: &str, name: &str) -> Member {
        Member {
            not_in_directory: false,
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn a_literal_paren_in_a_link_destination_is_escaped() {
        assert_eq!(escape_link_destination("https://x/y"), "https://x/y");
        assert_eq!(
            escape_link_destination(r"https://x/aa365247(v=vs.85)"),
            r"https://x/aa365247\(v=vs.85\)"
        );
    }

    #[test]
    fn an_attachment_url_with_parentheses_round_trips_through_the_link_parser() {
        let m = Msg {
            root_id: None,
            id: "1".into(),
            created: chrono::Utc::now(),
            modified: chrono::Utc::now(),
            edited: None,
            from_id: None,
            from_name: String::new(),
            system: false,
            deleted: false,
            html: false,
            body: "see attached".into(),
            subject: None,
            attachments: vec![Attachment {
                name: "Report (1).pdf".into(),
                content_type: "reference".into(),
                url: Some("https://x/Report(1).pdf".into()),
            }],
            reactions: vec![],
            mentions: vec![],
            delivery: teams_client::Delivery::Sent,
        };
        let md = message_markdown(&m, "me");
        assert!(md.contains(r"\(1\)"), "the destination is escaped: {md:?}");
        let mut links = Vec::new();
        let out = crate::render::number_links(&md, &mut links);
        assert_eq!(
            links,
            ["https://x/Report(1).pdf"],
            "the real, unescaped destination is what the link list shows"
        );
        assert!(
            !out.contains('\\'),
            "the escaping is parser-internal, never shown: {out:?}"
        );
    }

    #[test]
    fn a_long_message_is_cut_for_drawing_but_not_for_copying_and_the_cut_closes_a_fence() {
        let mut m = msg_with_attachment(Attachment {
            name: "x".into(),
            content_type: "reference".into(),
            url: None,
        });
        m.attachments.clear();
        // Prose, then a fenced block that is still open when the cut comes.
        m.html = true;
        m.body = format!(
            "<p>intro</p><pre><code>{}</code></pre>",
            "let a = 1;\n".repeat(3000)
        );
        let shown = message_markdown(&m, "me");
        let whole = message_markdown_in_full(&m, "me");
        assert!(whole.chars().count() > MAX_RENDERED_CHARS);
        assert!(
            !whole.contains("truncated"),
            "copying gives the message as it is"
        );
        assert!(
            shown.ends_with("\n```\n\n*… message truncated*"),
            "{:?}",
            &shown[shown.len() - 60..]
        );
        // Cut inside prose: no fence is invented.
        m.html = false;
        m.body = "word ".repeat(10_000);
        let shown = message_markdown(&m, "me");
        assert!(shown.ends_with("word \n\n*… message truncated*") && !shown.contains("```"));
    }

    fn msg_with_attachment(a: Attachment) -> Msg {
        Msg {
            root_id: None,
            id: "1".into(),
            created: chrono::Utc::now(),
            modified: chrono::Utc::now(),
            edited: None,
            from_id: None,
            from_name: String::new(),
            system: false,
            deleted: false,
            html: false,
            body: "see attached".into(),
            subject: None,
            attachments: vec![a],
            reactions: vec![],
            mentions: vec![],
            delivery: teams_client::Delivery::Sent,
        }
    }

    /// The subject is the sender's text inside `**..**`: it stays one bold line whatever it holds.
    #[test]
    fn a_subject_is_one_escaped_line_that_cannot_change_the_rest_of_the_message() {
        let mut m = msg_with_attachment(Attachment {
            name: "x".into(),
            content_type: "reference".into(),
            url: None,
        });
        m.attachments.clear();
        m.body = "the body".into();
        m.subject =
            Some("Hi **bold**\n```\n  [a](https://evil.example)\r\n~~~ rest\t\tof _it_".into());
        for md in [
            message_markdown(&m, "me"),
            message_markdown_in_full(&m, "me"),
        ] {
            let (subject, body) = md
                .split_once("\n\n")
                .expect("a subject, a blank line, the body");
            assert_eq!(body, "the body", "{md:?}");
            assert!(
                !subject.contains('\n') && subject.starts_with("**") && subject.ends_with("**"),
                "{subject:?}"
            );
            assert_eq!(
                subject,
                "**Hi \\*\\*bold\\*\\* \\`\\`\\` \u{FF3B}a\u{FF3D}(https://evil.example) ~~~ rest of \\_it\\_**"
            );
            assert!(
                md.lines().all(|l| crate::fence::fence_marker(l).is_none()),
                "no line is a fence: {md:?}"
            );
            let mut links = Vec::new();
            crate::render::number_links(&md, &mut links);
            assert!(links.is_empty(), "the subject made a link: {links:?}");
        }
        // Nothing left to show means no empty bold marker either.
        m.subject = Some(" \n\t ".into());
        assert_eq!(message_markdown(&m, "me"), "the body");
    }

    fn underlined(text: &str) -> String {
        inline_style::wrap(
            inline_style::Format {
                underline: true,
                ..Default::default()
            },
            text,
        )
    }

    #[test]
    fn a_picture_teams_web_reports_as_a_bare_extension_becomes_an_image_reference() {
        // Teams web uploads a pasted screenshot to the sender's OneDrive and reports it as a file attachment with
        // `fileType` a bare extension ("png"), not a MIME type; Graph instead calls any file "reference" and never
        // says "image" at all. Both must still end up as a picture, not a dropped or paperclip-linked attachment.
        for content_type in ["png", "PNG", "jpg", "jpeg", "gif", "webp", "image/png"] {
            let m = msg_with_attachment(Attachment {
                name: "Screenshot.png".into(),
                content_type: content_type.into(),
                url: Some("https://x/shot.png".into()),
            });
            let md = message_markdown(&m, "me");
            assert!(
                md.contains("![Screenshot.png](https://x/shot.png)"),
                "{content_type}: {md:?}"
            );
        }
    }

    #[test]
    fn a_non_picture_attachment_teams_web_reports_as_a_bare_extension_is_still_shown() {
        // The same bare-extension shape, but a type that is not a picture (a video, a PDF, a Word document, a
        // source file): still rendered, as a file link, not silently dropped for not matching Graph's "reference".
        for (content_type, url) in [
            ("pdf", "https://x/report.pdf"),
            ("docx", "https://x/notes.docx"),
            ("mp4", "https://x/clip.mp4"),
            ("rs", "https://x/main.rs"),
        ] {
            let m = msg_with_attachment(Attachment {
                name: "file".into(),
                content_type: content_type.into(),
                url: Some(url.into()),
            });
            let md = message_markdown(&m, "me");
            assert!(
                md.contains(&format!("📎 [file]({url})")),
                "{content_type}: {md:?}"
            );
            assert!(!md.contains("![file]"), "not treated as a picture: {md:?}");
        }
    }

    #[test]
    fn an_attachment_name_cannot_carry_a_style_marker() {
        let m = msg_with_attachment(Attachment {
            name: format!("evil{}name", underlined("1")),
            content_type: "reference".into(),
            url: Some("https://x/f".into()),
        });
        let md = message_markdown(&m, "me");
        assert!(
            !md.chars().any(|c| matches!(c as u32, 0xE000..=0xF8FF)),
            "a marker from the name must not reach the rendered Markdown: {md:?}"
        );
    }

    #[test]
    fn an_attachment_name_cannot_forge_a_second_link() {
        let m = msg_with_attachment(Attachment {
            name: "legit](http://evil.example)[hidden".into(),
            content_type: "reference".into(),
            url: Some("https://x/real".into()),
        });
        let md = message_markdown(&m, "me");
        let mut links = Vec::new();
        let out = crate::render::number_links(&md, &mut links);
        assert_eq!(
            links,
            ["https://x/real"],
            "the attachment name must not be read as Markdown link syntax: {out:?}"
        );
    }

    #[test]
    fn a_card_name_cannot_carry_a_style_marker_or_break_out_of_its_italics() {
        let m = msg_with_attachment(Attachment {
            name: format!("evil{}* actual italics", underlined("1")),
            content_type: "application/vnd.microsoft.card.adaptive".into(),
            url: None,
        });
        let md = message_markdown(&m, "me");
        assert!(
            !md.chars().any(|c| matches!(c as u32, 0xE000..=0xF8FF)),
            "a marker from the card name must not reach the rendered Markdown: {md:?}"
        );
        assert!(md.contains(r"\*"), "a literal asterisk is escaped: {md:?}");
    }

    /// Run `f` on a thread with a 1 MiB stack, the size of the main thread on Windows.
    fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn deeply_nested_html_is_shown_as_text_and_never_overflows_the_stack() {
        for (open, close) in [
            ("<div>", "</div>"),
            ("<b>", "</b>"),
            ("<blockquote>", "</blockquote>"),
            ("<ul><li>", "</li></ul>"),
            ("<b>", ""),
        ] {
            let html = format!("{}deep text{}", open.repeat(5000), close.repeat(5000));
            let md = on_small_stack(move || html_to_markdown(&html, "me"));
            assert!(md.contains("deep text"), "{open}: {md:?}");
        }
    }

    #[test]
    fn nesting_the_parser_sees_but_the_checker_does_not_cannot_overflow_the_stack() {
        // An empty comment (`<!-->`) ends at once in HTML, but the checker reads on to the next `-->` and skips
        // everything between; stray end tags (`</x>`) lower its count without closing anything in the parser.
        let hidden = format!(
            "<!-->{}deep text{}-->",
            "<div>".repeat(2_900),
            "</div>".repeat(2_900)
        );
        let stray = format!(
            "{}deep text",
            format!("{}</x>{}", "<div>".repeat(100), "</x>".repeat(100)).repeat(29)
        );
        for html in [hidden, stray] {
            assert!(
                !nests_too_deeply(&html),
                "the checker is fooled, as intended by this input"
            );
            let md = on_small_stack(move || html_to_markdown(&html, "me"));
            assert!(md.contains("deep text"), "{md:?}");
        }
    }

    #[test]
    fn a_message_with_thousands_of_elements_is_shown_as_text_whatever_their_shape() {
        let html = format!("<!-->{}deep text-->", "<div>".repeat(MAX_HTML_ELEMENTS + 1));
        assert!(has_more_elements_than(&html, MAX_HTML_ELEMENTS));
        let md = on_small_stack(move || html_to_markdown(&html, "me"));
        assert!(md.contains("deep text"), "{md:?}");
        // Closing tags and text do not count; ordinary messages are far below the limit.
        assert!(!has_more_elements_than(&"</p>a < b ".repeat(5_000), 10));
        assert!(!has_more_elements_than("<p>one</p><p>two</p>", 2));
        assert!(has_more_elements_than("<p>one</p><p>two</p>", 1));
    }
    #[test]
    fn nesting_checks_ignore_void_elements_comments_and_stray_angle_brackets() {
        let flat = format!(
            "{}<!-- <div><div> -->a < b <br><img src=x>{}",
            "<p>".repeat(3),
            "</p>".repeat(3)
        );
        assert!(!nests_too_deeply(&flat));
        assert!(!nests_too_deeply(&"<br>".repeat(10_000)));
        assert!(
            !nests_too_deeply(&"<a".repeat(10_000)),
            "no closing bracket anywhere"
        );
        assert!(
            !nests_too_deeply(&"<p>x</p>".repeat(10_000)),
            "siblings are not nesting"
        );
        assert!(nests_too_deeply(&"<div>".repeat(MAX_HTML_DEPTH + 1)));
        assert!(!nests_too_deeply(&"<div>".repeat(MAX_HTML_DEPTH)));
    }

    #[test]
    fn ordinary_nesting_still_converts_to_markdown() {
        let html = format!("{}<b>bold</b>{}", "<div>".repeat(30), "</div>".repeat(30));
        assert!(html_to_markdown(&html, "me").contains("**bold**"));
    }

    #[test]
    fn html_basics_become_markdown() {
        let md = html_to_markdown(
            "<p>Hello <b>world</b> and <i>you</i></p><ul><li>one</li><li>two</li></ul>",
            "me",
        );
        assert!(md.contains("Hello **world** and *you*"), "{md}");
        assert!(
            md.lines().any(|l| l.starts_with('*') && l.ends_with("one")),
            "{md}"
        );
    }

    #[test]
    fn mentions_and_emoji_survive() {
        let md = html_to_markdown(
            r#"<p><at id="0">Ada Lovelace</at> check this <emoji id="smile" alt="😄" title="Smile"></emoji></p>"#,
            "me",
        );
        assert!(
            md.contains(&mention_markup("Ada Lovelace", Some("0"), "me")),
            "{md}"
        );
        assert!(md.contains('😄'), "{md}");
    }

    #[test]
    fn a_reply_quotes_own_time_is_shown_as_a_readable_local_date_and_time() {
        // Exactly as seen live, 2026-11-06: Teams' own reply quote leaves the timestamp as an empty `<span>`,
        // the real client filling in readable text at render time rather than sending any; vtteams has to do
        // the same, from the millisecond epoch time in `itemid`.
        let ms: i64 = 1_793_979_227_035;
        let html = format!(
            "<blockquote itemscope itemtype=\"http://schema.skype.com/Reply\" itemid=\"{ms}\">\
             <strong itemprop=\"mri\" itemid=\"8:orgid:x\">Ada Lovelace</strong>\
             <span itemprop=\"time\" itemid=\"{ms}\"></span>\
             <p itemprop=\"preview\">No I mean about 30 fixes and changes for AI hub!</p></blockquote>\n\
             <p>I know that's motherhood and apple pie</p>"
        );
        let md = html_to_markdown(&html, "me");
        let expected = chrono::DateTime::from_timestamp_millis(ms)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%-m/%-d/%Y %H:%M")
            .to_string();
        assert!(md.contains("Ada Lovelace"), "{md}");
        assert!(md.contains(&expected), "{md}");
        assert!(
            md.find("Ada Lovelace").unwrap() < md.find(&expected).unwrap(),
            "the time comes after the name, matching the real client: {md}"
        );
        assert!(md.contains("No I mean about 30 fixes"), "{md}");
    }

    #[test]
    fn a_reply_quotes_time_span_with_an_unreadable_itemid_is_dropped_not_an_error() {
        let html = "<blockquote itemscope itemtype=\"http://schema.skype.com/Reply\" itemid=\"x\">\
             <strong itemprop=\"mri\" itemid=\"8:orgid:x\">Ada Lovelace</strong>\
             <span itemprop=\"time\" itemid=\"not a number\"></span>\
             <p itemprop=\"preview\">hi</p></blockquote>";
        let md = html_to_markdown(html, "me");
        assert!(md.contains("Ada Lovelace") && md.contains("hi"), "{md}");
    }

    #[test]
    fn a_mention_of_the_signed_in_user_is_coloured_differently_from_one_of_someone_else() {
        let self_markup = mention_markup("Name", Some("me"), "me");
        let other_markup = mention_markup("Name", Some("ada"), "me");
        assert_ne!(
            self_markup, other_markup,
            "same name, different id: only the colour tells them apart"
        );
        let unresolved_markup = mention_markup("Name", None, "me");
        assert_eq!(
            other_markup, unresolved_markup,
            "no id (or one that doesn't match) is treated the same as someone else's, never as the signed-in \
             user's by accident"
        );
    }

    #[test]
    fn teams_codeblock_becomes_fenced_code_with_language() {
        let md = html_to_markdown(
            "<p>see</p><codeblock class=\"Rust\"><code>fn main() {\n    println!(\"a &lt; b\");\n}</code></codeblock>",
            "me",
        );
        assert!(md.contains("```rust"), "{md}");
        assert!(md.contains("a < b"), "{md}");
        assert!(md.contains("fn main() {\n    println!"), "{md}");
    }

    #[test]
    fn pre_code_language_class_is_kept() {
        let md = html_to_markdown(
            r#"<pre><code class="language-python">print(1)</code></pre>"#,
            "me",
        );
        assert!(md.contains("```python") && md.contains("print(1)"), "{md}");
    }

    #[test]
    fn hosted_images_become_markdown_images() {
        let md = html_to_markdown(
            r#"<p><img src="https://graph.microsoft.com/v1.0/chats/19:a/messages/1/hostedContents/abc/$value" alt="image"></p>"#,
            "me",
        );
        assert!(md.contains("![image](https://graph.microsoft.com/v1.0/chats/19:a/messages/1/hostedContents/abc/$value)"), "{md}");
    }

    #[test]
    fn emoji_images_use_alt_text() {
        let md = html_to_markdown(
            r#"<img src="https://statics.teams.cdn.office.net/evergreen-assets/skype/v2/emoticons/laugh.png" alt="😆">"#,
            "me",
        );
        assert_eq!(md, "😆");
    }

    #[test]
    fn terminal_control_sequences_never_survive() {
        let evil = "<p>hi &#27;]52;c;AAAA&#7; \u{1b}[2J there \u{202E}reversed</p>";
        for out in [html_to_markdown(evil, "me"), html_to_plain(evil)] {
            assert!(
                !out.chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t'),
                "{out:?}"
            );
            assert!(!out.contains('\u{202E}'));
            assert!(out.contains("hi") && out.contains("there"));
        }
    }

    #[test]
    fn list_markers_are_narrowed_outside_code() {
        let md = tidy("*   one\n1.  two\n```\n*   keep\n```");
        assert_eq!(md, "* one\n1. two\n```\n*   keep\n```");
    }

    #[test]
    fn plain_text_is_escaped_not_interpreted() {
        assert_eq!(plain_to_markdown("a*b_c"), "a\\*b\\_c");
    }

    #[test]
    fn shortcodes_expand_except_in_code() {
        assert_eq!(
            expand_emoji("hi :smile: and `:smile:` and :nope: 12:30:45"),
            "hi 😄 and `:smile:` and :nope: 12:30:45"
        );
    }

    #[test]
    fn shortcodes_are_left_alone_inside_fenced_code() {
        let text =
            "before :smile:\n```objectscript\nset x = :smile:\n```\nafter :smile:\n~~~\n:x:\n~~~\n";
        assert_eq!(
            expand_emoji(text),
            "before 😄\n```objectscript\nset x = :smile:\n```\nafter 😄\n~~~\n:x:\n~~~\n"
        );
        // A fence that is never closed runs to the end, as Markdown reads it.
        assert_eq!(expand_emoji("```\n:smile:"), "```\n:smile:");
    }

    #[test]
    fn a_shortcode_must_stand_on_its_own() {
        // Scope operators and similar punctuation in prose are not shortcodes.
        for plain in [
            "call ns::x::fn() here",
            "Foo::x::bar",
            "a:x:b",
            "https://example.com:100:/",
        ] {
            assert_eq!(expand_emoji(plain), plain, "{plain}");
        }
        assert_eq!(expand_emoji(":x:"), "❌");
        assert_eq!(expand_emoji("(:smile:)"), "(😄)");
        assert_eq!(
            expand_emoji(":smile::smile:"),
            "😄😄",
            "shortcodes written back to back"
        );
        assert_eq!(expand_emoji("done :x:, then :smile:."), "done ❌, then 😄.");
    }

    #[test]
    fn markdown_to_html_keeps_line_breaks_and_code() {
        let (html, _) = markdown_to_html(
            "line one\nline **two**\n\n```rust\nlet x = 1 < 2;\n```",
            &[],
            "me",
            CodeBlockStyle::default(),
        );
        assert!(
            html.contains("line one<br />\nline <strong>two</strong>"),
            "{html}"
        );
        assert!(
            html.contains(
                "<codeblock class=\"rust\"><code>let&nbsp;x&nbsp;=&nbsp;1&nbsp;&lt;&nbsp;2;</code></codeblock>"
            ),
            "{html}"
        );
    }

    #[test]
    fn code_blocks_are_sent_in_the_teams_form() {
        // No language: Graph's documented example uses "plaintext". Indentation and line breaks survive as
        // `&nbsp;` and `<br>`, as the service itself sends them; markup in the code is text.
        let (html, _) = markdown_to_html(
            "```\nif a {\n  b<c>&d\n}\n```",
            &[],
            "me",
            CodeBlockStyle::default(),
        );
        assert_eq!(
            html,
            "<codeblock class=\"plaintext\"><code>if&nbsp;a&nbsp;{<br>&nbsp;&nbsp;b&lt;c&gt;&amp;d<br>}</code></codeblock>"
        );
        // A language Teams does not know is carried as typed (lower-cased), for readers that colour it; the info
        // string's extra words and any markup in it are dropped.
        let (html, _) = markdown_to_html(
            "```ObjectScript title=\"x\"\nset x=1\n```",
            &[],
            "me",
            CodeBlockStyle::default(),
        );
        assert!(
            html.starts_with("<codeblock class=\"objectscript\">"),
            "{html}"
        );
        let (html, _) = markdown_to_html(
            "```a\"><script>\nx\n```",
            &[],
            "me",
            CodeBlockStyle::default(),
        );
        assert!(html.starts_with("<codeblock class=\"ascript\">"), "{html}");
        // Indented code is a block too; inline code is not.
        let (html, _) = markdown_to_html(
            "text\n\n    indented\n\n`inline`",
            &[],
            "me",
            CodeBlockStyle::default(),
        );
        assert!(
            html.contains("<codeblock class=\"plaintext\"><code>indented</code>"),
            "{html}"
        );
        assert!(
            html.contains("<code>inline</code>")
                && !html.contains("<codeblock class=\"plaintext\"><code>inline")
        );
    }

    #[test]
    fn a_code_block_the_service_stored_as_a_bare_code_element_is_still_a_block() {
        // What Teams stored for a ```objectscript block we sent: the `<codeblock>` and its class are gone, the
        // lines are `<br>` followed by a line break, and spaces are `&nbsp;`.
        let html = "<p>So we're using this sort of construct:</p>\r\n\n<code>Set&nbsp;err&nbsp;=&nbsp;$$$ERROR($$$AIFoo,baz)<br>\r\nQuit&nbsp;err</code>";
        let md = html_to_markdown(html, "me");
        assert_eq!(
            md,
            "So we're using this sort of construct:\n\n```\nSet err = $$$ERROR($$$AIFoo,baz)\nQuit err\n```"
        );
        // Inline code, with or without other markup, is left alone; so is code inside a `<pre>`.
        let md = html_to_markdown(
            "<p>use <code>Set x=1</code> and <code>Quit</code> here</p>",
            "me",
        );
        assert_eq!(md, "use `Set x=1` and `Quit` here");
        let md = html_to_markdown("<pre><code>a<br>b</code></pre><p><code>c</code></p>", "me");
        assert!(md.contains("```") && md.contains("`c`"), "{md}");
        // Blank lines in the code are kept: `<br><br>` is one empty line.
        let md = html_to_markdown("<code>a<br>\r\n<br>\r\nb</code>", "me");
        assert!(md.contains("a\n\nb"), "{md:?}");
        // A tag-looking line of code stays code.
        let md = html_to_markdown("<code>x &lt;b&gt;<br>y</code>", "me");
        assert!(md.contains("x <b>\ny"), "{md:?}");
    }

    /// A coloured span as the card should have it.
    fn hl(class: &str, text: &str) -> String {
        format!(
            "<span class=\"hljs-{class}\" style=\"color:{}\">{text}</span>",
            highlight::class_colour(class).unwrap()
        )
    }

    fn card_ids(html: &str) -> (String, String) {
        let marker = html.split("id=\"x_codeBlockEditor-").nth(1).unwrap();
        let pre = html.split("itemid=\"codeBlockEditor-").nth(1).unwrap();
        (
            marker.split('"').next().unwrap().to_string(),
            pre.split('"').next().unwrap().to_string(),
        )
    }

    #[test]
    fn the_teams_web_service_gets_the_card_the_teams_client_writes() {
        let (html, _) = markdown_to_html(
            "```Rust\nfn a<T>() {\n  1 & 2\n}\n```\n\n```\nplain\n```",
            &[],
            "me",
            CodeBlockStyle::EditorCard,
        );
        let (marker_id, pre_id) = card_ids(&html);
        assert_eq!(marker_id, pre_id, "the marker and the pre name each other");
        assert!(
            pre_id.len() == 36 && pre_id.matches('-').count() == 4,
            "{pre_id}"
        );
        assert!(html.contains(
            "<p itemtype=\"http://schema.skype.com/CodeBlockEditor\" id=\"x_codeBlockEditor-"
        ));
        assert!(
            html.contains("<pre class=\"language-rust\" itemid=\"codeBlockEditor-"),
            "{html}"
        );
        assert!(
            html.contains(
                &format!(
                    "{}&nbsp;{}&lt;{}&gt;()&nbsp;{{<br>&nbsp;&nbsp;{}&nbsp;&amp;&nbsp;{}<br>}}</code></pre>",
                    hl("keyword", "fn"),
                    hl("function", "a"),
                    hl("type", "T"),
                    hl("literal", "1"),
                    hl("literal", "2")
                )
            ),
            "{html}"
        );
        // Short labels become the menu's names, so the card's header can show them.
        let (short, _) = markdown_to_html(
            "```ts\nlet a\n```\n```py\nb\n```",
            &[],
            "me",
            CodeBlockStyle::EditorCard,
        );
        assert!(
            short.contains("language-typescript") && short.contains("language-python"),
            "{short}"
        );
        // No language: what the client calls plain text. Two blocks, two different GUIDs.
        assert!(html.contains("<pre class=\"language-plaintext\""), "{html}");
        let second = html.rsplit("itemid=\"codeBlockEditor-").next().unwrap();
        assert_ne!(second.split('"').next().unwrap(), pre_id);
        assert!(!html.contains("<codeblock"));
    }

    /// The body of the first card in `html`.
    fn card_body(html: &str) -> &str {
        let pre = html.split("<pre").nth(1).unwrap();
        pre.split("<code>")
            .nth(1)
            .unwrap()
            .split("</code>")
            .next()
            .unwrap()
    }

    #[test]
    fn a_card_is_coloured_with_the_classes_the_teams_client_uses() {
        let (html, _) = markdown_to_html(
            "```rust\nfn main() {\n    let x = \"hi\"; // c\n}\n```",
            &[],
            "me",
            CodeBlockStyle::EditorCard,
        );
        let body = card_body(&html);
        // The classes the Teams client wrote for `fn main() {}` (see the stored message in the tests below), each
        // with its colour.
        assert!(
            body.starts_with(&format!(
                "{}&nbsp;{}()&nbsp;{{<br>",
                hl("keyword", "fn"),
                hl("function", "main")
            )),
            "{body}"
        );
        assert!(body.contains(&hl("string", "\"hi\"")), "{body}");
        assert!(body.contains(&hl("comment", "//&nbsp;c")), "{body}");
        // Spans balance and none runs across a line.
        assert_eq!(
            body.matches("<span").count(),
            body.matches("</span>").count()
        );
        for line in body.split("<br>") {
            assert_eq!(
                line.matches("<span").count(),
                line.matches("</span>").count(),
                "{line}"
            );
        }
        // Take the markup away and the code is what was typed.
        let mut text = String::new();
        let mut in_tag = false;
        for c in body.replace("<br>", "\n").chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => text.push(c),
                _ => {}
            }
        }
        assert_eq!(
            text.replace("&nbsp;", " "),
            "fn main() {\n    let x = \"hi\"; // c\n}"
        );
    }

    #[test]
    fn objectscript_is_coloured_for_teams_by_the_same_highlighter_that_colours_it_here() {
        let source = "ClassMethod Hello() As %Status\n{\n    Set x = $Get(^Counts(\"a\"), 0) + 1 // c\n    Write \"hi\", !\n    Quit $$$OK\n}";
        let (html, _) = markdown_to_html(
            &format!("```objectscript\n{source}\n```"),
            &[],
            "me",
            CodeBlockStyle::EditorCard,
        );
        let body = card_body(&html);
        for class in ["keyword", "string", "comment"] {
            assert!(
                body.contains(&format!("<span class=\"hljs-{class}\" style=\"color:#")),
                "{class}: {body}"
            );
        }
        // The language survives in the `pre` (Teams drops comments and attributes elsewhere but keeps this), so the
        // message read back is an ObjectScript block again, to be coloured here by the same highlighter.
        assert_eq!(
            html_to_markdown(&html, "me"),
            format!("```objectscript\n{source}\n```")
        );
        assert!(
            html.contains("<pre class=\"language-objectscript\""),
            "{html}"
        );
    }

    #[test]
    fn code_that_nothing_colours_or_is_too_big_or_goes_to_graph_has_no_colour_spans() {
        for (src, style) in [
            (
                "```nosuchlanguage\nSet x = 1\n```",
                CodeBlockStyle::EditorCard,
            ),
            ("```\nfn main() {}\n```", CodeBlockStyle::EditorCard),
            ("```rust\nfn main() {}\n```", CodeBlockStyle::Codeblock),
        ] {
            let (html, _) = markdown_to_html(src, &[], "me", style);
            assert!(!html.contains("hljs-"), "{src}: {html}");
        }
        let big = format!("```rust\n{}\n```", "let x = 1;\n".repeat(2_000));
        let (html, _) = markdown_to_html(&big, &[], "me", CodeBlockStyle::EditorCard);
        assert!(
            !html.contains("hljs-") && html.contains("let&nbsp;x"),
            "too big to colour"
        );
    }
    #[test]
    fn a_card_we_send_reads_back_as_the_same_fenced_block() {
        let source = "text\n\n```objectscript\nSet x = $Get(^A)\n  Quit x\n```";
        let (html, _) = markdown_to_html(source, &[], "me", CodeBlockStyle::EditorCard);
        assert_eq!(
            html_to_markdown(&html, "me"),
            "text\n\n```objectscript\nSet x = $Get(^A)\n  Quit x\n```"
        );
    }

    #[test]
    fn a_card_as_the_teams_client_stored_it_reads_as_a_fenced_block_without_its_marker() {
        // Verbatim from a message the Teams web client sent (ids shortened), including its colour spans.
        let html = "<p>vtteams real rust test</p>\r\n<p>&nbsp;</p>\r\n<p></p>\r\n<p itemtype=\"http://schema.skype.com/CodeBlockEditor\" id=\"x_codeBlockEditor-fc54925b\">\r\n&nbsp;</p>\r\n<pre class=\"language-rust language-was-manually-selected\" itemid=\"codeBlockEditor-fc54925b\"><code><span class=\"hljs-keyword\">fn</span>&nbsp;<span class=\"hljs-function\">main</span>()&nbsp;{<br>}</code></pre>";
        let md = html_to_markdown(html, "me");
        assert!(md.ends_with("```rust\nfn main() {\n}\n```"), "{md:?}");
        assert!(!md.contains("hljs") && !md.contains('\u{a0}'), "{md:?}");
        // The card with a language Teams does not know, and plain text, as the client stores them.
        let md = html_to_markdown(
            "<p>x</p><p itemtype=\"http://schema.skype.com/CodeBlockEditor\" id=\"x_codeBlockEditor-1\">&nbsp;</p><pre class=\"language-objectscript language-was-manually-selected\" itemid=\"codeBlockEditor-1\"><code>Set&nbsp;x&nbsp;=&nbsp;1<br>Quit&nbsp;x</code></pre>",
            "me",
        );
        assert!(
            md.contains("```objectscript\nSet x = 1\nQuit x\n```"),
            "{md:?}"
        );
        let md = html_to_markdown(
            "<pre class=\"language-plaintext\"><code>a<br>b</code></pre>",
            "me",
        );
        assert!(md.contains("```\na\nb\n```"), "{md:?}");
    }

    #[test]
    fn preformatted_text_keeps_its_indentation_and_other_markup_in_it_is_dropped() {
        let md = html_to_markdown(
            "<pre>$ git log\n    indented &lt;b&gt;\n<b>bold</b> end</pre>",
            "me",
        );
        assert!(
            md.contains("$ git log\n    indented <b>\nbold end"),
            "{md:?}"
        );
        // Older forms still read: language on the `code`, as other clients write it.
        let md = html_to_markdown(
            "<pre><code class=\"language-python\">print(1)</code></pre>",
            "me",
        );
        assert!(md.contains("```python\nprint(1)\n```"), "{md:?}");
        // A `pre` that is never closed is left to the converter.
        let md = html_to_markdown("<p>a</p><pre>unclosed", "me");
        assert!(md.contains("unclosed"), "{md:?}");
    }
    #[test]
    fn a_code_block_survives_the_round_trip_through_the_service_form() {
        let source = "```json\n{\n  \"a\": [1, 2],\n\t\"b\": \"x < y\"\n}\n```";
        let (html, _) = markdown_to_html(source, &[], "me", CodeBlockStyle::default());
        let md = html_to_markdown(&html, "me");
        assert!(md.contains("```json"), "{md}");
        assert!(
            md.contains("{\n  \"a\": [1, 2],\n    \"b\": \"x < y\"\n}"),
            "{md}"
        );
    }

    #[test]
    fn a_code_block_as_the_service_sends_it_is_plain_code() {
        // Graph's documented read-back: empty paragraphs around it, highlighting spans, `<br>` and `&nbsp;`.
        let md = html_to_markdown(
            "<p>&nbsp;</p>\n\n<codeblock class=\"Json\"><code>{<br> &nbsp;&nbsp; <span class=\"hljs-function\">\"body\"</span>:&nbsp;{<br> &nbsp;&nbsp; <span class=\"hljs-function\">\"a\"</span>:&nbsp;<span class=\"hljs-string\">\"&lt;x&gt;\"</span><br> &nbsp;&nbsp; }<br>}</code></codeblock>\n<p>&nbsp;</p>",
            "me",
        );
        assert!(md.contains("```json"), "{md}");
        assert!(
            md.contains("{\n    \"body\": {\n    \"a\": \"<x>\"\n    }\n}"),
            "{md}"
        );
        assert!(!md.contains("hljs") && !md.contains('\u{a0}'), "{md:?}");
        // No language, or "plaintext": a fence without a tag.
        for class in ["", "plaintext"] {
            let md = html_to_markdown(
                &format!("<codeblock class=\"{class}\"><code>Hello&nbsp;world</code></codeblock>"),
                "me",
            );
            assert!(md.contains("```\nHello world\n```"), "{class}: {md}");
        }
    }

    #[test]
    fn raw_html_and_generic_types_are_sent_as_text() {
        let (html, _) = markdown_to_html(
            "Use Vec<String> and <script>alert(1)</script> or <b>bold</b>",
            &[],
            "me",
            CodeBlockStyle::default(),
        );
        assert!(html.contains("Vec&lt;String&gt;"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;")
                && html.contains("&lt;b&gt;bold&lt;/b&gt;"),
            "{html}"
        );
        assert!(!html.contains("<script") && !html.contains("<b>"), "{html}");
        let (html, _) =
            markdown_to_html("<div>\nblock\n</div>", &[], "me", CodeBlockStyle::default());
        assert!(!html.contains("<div>"), "{html}");
        // Markdown formatting still works.
        assert!(
            markdown_to_html(
                "**bold** and `<code>`",
                &[],
                "me",
                CodeBlockStyle::default()
            )
            .0
            .contains("<strong>bold</strong>")
        );
    }

    #[test]
    fn mentions_match_names_whose_lowercase_has_a_different_length() {
        // "İ" lowercases to two characters ("i" + a combining dot), so offsets measured on a lowercased copy do not match the original text.
        let members = [member("u1", "İlker Demir")];
        let (html, mentions) = markdown_to_html(
            "@İlker Demir, hello",
            &members,
            "me",
            CodeBlockStyle::default(),
        );
        assert_eq!(mentions.len(), 1, "{html}");
        assert!(
            html.contains("<at id=\"0\">İlker Demir</at>, hello"),
            "{html}"
        );
        let members = [member("u2", "Zoë Ångström")];
        let (html, mentions) = markdown_to_html(
            "@zoë ÅNGSTRÖM hi",
            &members,
            "me",
            CodeBlockStyle::default(),
        );
        assert_eq!(mentions.len(), 1, "case-insensitive: {html}");
        assert!(html.contains("<at id=\"0\">Zoë Ångström</at> hi"), "{html}");
    }

    #[test]
    fn mentions_are_not_resolved_inside_inline_code() {
        let members = [member("u1", "Ada Lovelace")];
        let (html, mentions) = markdown_to_html(
            "ask `@Ada` or @Ada",
            &members,
            "me",
            CodeBlockStyle::default(),
        );
        assert_eq!(mentions.len(), 1);
        assert!(
            html.contains("<code>@Ada</code>") && html.contains("<at id=\"0\">Ada Lovelace</at>"),
            "{html}"
        );
    }

    #[test]
    fn mentions_resolve_to_at_tags() {
        let members = [
            member("me", "Me Myself"),
            member("u1", "Ada Lovelace"),
            member("u2", "Grace Hopper"),
        ];
        let (html, mentions) = markdown_to_html(
            "hey @Ada Lovelace and @grace, ping @nobody",
            &members,
            "me",
            CodeBlockStyle::default(),
        );
        assert_eq!(mentions.len(), 2);
        assert!(html.contains("<at id=\"0\">Ada Lovelace</at>"), "{html}");
        assert!(html.contains("<at id=\"1\">Grace Hopper</at>,"), "{html}");
        assert!(html.contains("@nobody"), "{html}");
    }

    #[test]
    fn an_at_sign_in_an_address_is_not_a_mention_and_the_address_is_not_changed() {
        let members = [member("u1", "Ada Lovelace")];
        let send = |md: &str| markdown_to_html(md, &members, "me", CodeBlockStyle::default());
        // A link's destination and a bare address (a profile page named for someone, say) are not people.
        let (html, mentions) =
            send("[her page](https://example.com/@Ada/notes) and https://example.com/@Ada");
        assert!(mentions.is_empty(), "{html}");
        assert!(
            html.contains(r#"href="https://example.com/@Ada/notes""#),
            "the destination arrives as written: {html}"
        );
        assert!(html.contains("https://example.com/@Ada"), "{html}");
        // A destination with parentheses in it is skipped whole.
        let (html, mentions) = send(
            "[x](https://example.com/a_(b)/@Ada) and <https://example.com/@Ada> and mailto:x@Ada.example",
        );
        assert!(mentions.is_empty(), "{html}");
        // Beside them, a real mention still works, including the label of a link and text right after one.
        let (html, mentions) = send("thanks @Ada, see [the page](https://example.com/@Ada) @Ada");
        assert_eq!(mentions.len(), 1, "one person, mentioned twice: {html}");
        assert_eq!(html.matches(r#"<at id="0">Ada Lovelace</at>"#).count(), 2);
        assert!(
            html.contains(r#"href="https://example.com/@Ada""#),
            "{html}"
        );
    }

    #[test]
    fn mentions_are_not_resolved_inside_code_fences_or_emails() {
        let members = [member("u1", "Ada Lovelace")];
        let (html, mentions) = markdown_to_html(
            "mail ada@Ada.example\n```\n@Ada\n```",
            &members,
            "me",
            CodeBlockStyle::default(),
        );
        assert!(mentions.is_empty(), "{html}");
    }

    #[test]
    fn code_that_contains_a_fence_line_is_one_block_for_tidying_and_for_mentions() {
        // A four-backtick block holding a three-backtick line, as htmd writes it.
        let md = "````\n```\n\n\n*   x\n```\n@Ada\n````\nafter\n\n\nmore";
        let tidy = tidy(md);
        assert!(
            tidy.contains("```\n\n\n*   x\n```"),
            "blank lines and list markers inside the block are untouched: {tidy:?}"
        );
        assert!(
            tidy.ends_with("after\n\nmore"),
            "outside it they are tidied: {tidy:?}"
        );
        let members = [member("u1", "Ada Lovelace")];
        let (_, mentions) = markdown_to_html(md, &members, "me", CodeBlockStyle::default());
        assert!(
            mentions.is_empty(),
            "an @name inside the block is not a mention"
        );
    }

    #[test]
    fn first_name_uniqueness_is_case_insensitive() {
        let members = [member("a", "Ada Lovelace"), member("b", "ada smith")];
        let (html, mentions) =
            markdown_to_html("@ada hi", &members, "me", CodeBlockStyle::Codeblock);
        assert!(
            mentions.is_empty(),
            "first name not unique case-insensitively: {html}"
        );
        let (html, mentions) = markdown_to_html(
            "@Ada Lovelace hi",
            &members,
            "me",
            CodeBlockStyle::Codeblock,
        );
        assert_eq!(mentions.len(), 1);
        assert!(html.contains("<at id=\"0\">Ada Lovelace</at>"), "{html}");
    }

    #[test]
    fn numeric_references_to_markers_are_stripped() {
        let md = html_to_markdown("<p>&#xE001;x&#xE000;</p>", "me");
        assert!(
            !md.chars().any(inline_style::is_marker),
            "hex with semicolons: {md:?}"
        );

        let md = html_to_markdown("<p>&#57345;x</p>", "me");
        assert!(
            !md.chars().any(inline_style::is_marker),
            "decimal 57345 (0xE001): {md:?}"
        );

        let md = html_to_markdown("<p>&#xe001x</p>", "me");
        assert!(
            !md.chars().any(inline_style::is_marker),
            "hex without semicolon: {md:?}"
        );

        let md = html_to_markdown("<p>&#x0000E001;x</p>", "me");
        assert!(
            !md.chars().any(inline_style::is_marker),
            "hex with leading zeros: {md:?}"
        );

        let md = html_to_markdown(
            r#"<p><img alt="&#xE001;a" src="https://x/y.png"></p>"#,
            "me",
        );
        assert!(
            !md.chars().any(inline_style::is_marker),
            "in an attribute: {md:?}"
        );

        let md = html_to_markdown("<p>&&#xE001;#xE001;x</p>", "me");
        assert!(
            !md.chars().any(inline_style::is_marker),
            "bypass attempt: {md:?}"
        );
    }

    #[test]
    fn other_numeric_references_remain() {
        let md = html_to_markdown("<p>&#x41;</p>", "me");
        assert!(md.contains('A'), "&#x41; becomes A: {md:?}");
    }

    #[test]
    fn legitimate_markers_remain() {
        let md = html_to_markdown(r#"<p><span style="color:#c00">red</span></p>"#, "me");
        assert!(
            md.chars().any(inline_style::is_marker),
            "colored span produces markers: {md:?}"
        );
    }

    #[test]
    fn unterminated_codeblock_does_not_duplicate_text() {
        let md = html_to_markdown("<p>hello <codeblock", "me");
        assert_eq!(
            md.matches("hello").count(),
            1,
            "text before unterminated codeblock must appear once: {md:?}"
        );
    }

    #[test]
    fn unterminated_pre_does_not_duplicate_text() {
        let md = html_to_markdown("<p>world <pre", "me");
        assert_eq!(
            md.matches("world").count(),
            1,
            "text before unterminated pre must appear once: {md:?}"
        );
    }

    #[test]
    fn unterminated_code_does_not_duplicate_text() {
        let md = html_to_markdown("<p>test <code", "me");
        assert_eq!(
            md.matches("test").count(),
            1,
            "text before unterminated code must appear once: {md:?}"
        );
    }

    #[test]
    fn unclosed_codeblock_class_does_not_duplicate_text() {
        let md = html_to_markdown(r#"<p>example <codeblock class="x""#, "me");
        assert_eq!(
            md.matches("example").count(),
            1,
            "text before unclosed codeblock class must appear once: {md:?}"
        );
    }
}
