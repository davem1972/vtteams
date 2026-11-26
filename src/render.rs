//! Markdown -> styled terminal lines.
//!
//! A message body (already Markdown) is split into segments: prose goes through `termimad`, fenced code through the
//! syntax highlighters inside a bordered card, and images through the image-to-art renderers.

use crate::highlight;
use crate::imgart::{self, ImgMode};
use crate::inline_style;
use crate::termtext;
use crate::theme::*;
use image::RgbImage;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Never fetch or draw more than this many images per message.
pub const MAX_IMAGES_PER_MESSAGE: usize = 6;
/// Code blocks in one message that get syntax colour; later ones are shown uncoloured. Colouring a block costs tens
/// of microseconds plus a few microseconds per byte, and a message is cut at 20,000 characters, so this only bounds
/// the per-block overhead of a message made of hundreds of tiny blocks.
pub const MAX_COLOURED_BLOCKS: usize = 200;

#[derive(Clone)]
pub enum ImgState {
    Loading,
    Ready(Arc<RgbImage>),
    Failed(String),
    /// Content from outside the organisation, not loaded automatically. The URL is kept for retry logic.
    #[allow(dead_code)]
    Unverified(String),
}

/// Which fit of which picture: URL, glyph set, columns, rows.
type ArtKey = (String, ImgMode, usize, usize);

/// Fitted pictures kept at once; far more than fit on a screen, so only a long session with many widths reaches it.
const MAX_CACHED_ART: usize = 96;

/// Decoded (or pending) images keyed by URL, plus a counter bumped whenever anything changes.
#[derive(Default)]
pub struct ImageStore {
    pub map: HashMap<String, ImgState>,
    pub version: u64,
    /// URLs in the order they were first stored, oldest first; eviction walks it.
    order: VecDeque<String>,
    /// The art of each picture at the sizes it was drawn. Fitting glyphs to a picture costs tens of milliseconds, and
    /// a message is redrawn whenever anything near it changes (a spinner turning for another picture, a reaction),
    /// so the result is kept until the picture itself changes or goes.
    art: Mutex<HashMap<ArtKey, Arc<Vec<Line<'static>>>>>,
}

impl ImageStore {
    /// The art for `img` (the picture stored under `url`) at this size, fitted the first time it is asked for.
    pub fn art(
        &self,
        url: &str,
        img: &RgbImage,
        mode: ImgMode,
        cols: usize,
        rows: usize,
    ) -> Arc<Vec<Line<'static>>> {
        let key = (url.to_string(), mode, cols, rows);
        if let Some(art) = self.art_cache().get(&key) {
            return Arc::clone(art);
        }
        let started = std::time::Instant::now();
        let art = Arc::new(imgart::render(img, mode, cols, rows));
        log::debug!(
            "fitted {mode:?} art, up to {cols}x{rows} cells, for a {}x{} picture in {} ms",
            img.width(),
            img.height(),
            started.elapsed().as_millis()
        );
        let mut cache = self.art_cache();
        if cache.len() >= MAX_CACHED_ART {
            cache.clear();
        }
        cache.insert(key, Arc::clone(&art));
        art
    }

    // The cache only holds finished results, so it is valid even if a holder panicked.
    fn art_cache(&self) -> MutexGuard<'_, HashMap<ArtKey, Arc<Vec<Line<'static>>>>> {
        self.art.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record that `url` is being downloaded. Does not count as a change: nothing on screen differs yet.
    pub fn begin(&mut self, url: &str) {
        if self
            .map
            .insert(url.to_string(), ImgState::Loading)
            .is_none()
        {
            self.order.push_back(url.to_string());
        }
    }

    pub fn set(&mut self, url: &str, state: ImgState) {
        if self.map.insert(url.to_string(), state).is_none() {
            self.order.push_back(url.to_string());
        }
        self.art_cache().retain(|(u, ..), _| u != url);
        self.version += 1;
    }

    /// Forget the pictures that failed to load, so the next time one is near the screen it is fetched again. Returns
    /// how many there were. Unverified images are not retried here; they are only loaded when explicitly requested.
    pub fn forget_failed(&mut self) -> usize {
        let before = self.map.len();
        self.map.retain(|_, s| !matches!(s, ImgState::Failed(_)));
        let map = &self.map;
        self.order.retain(|url| map.contains_key(url));
        let forgotten = before - self.map.len();
        if forgotten > 0 {
            self.version += 1;
        }
        forgotten
    }

    /// Keep at most `max` decoded images by dropping the oldest ones that are not in `protect` (those on or near the
    /// screen). A dropped image is simply fetched again if it comes back into view.
    pub fn evict(&mut self, max: usize, protect: &HashSet<&str>) {
        let ready = self
            .map
            .values()
            .filter(|s| matches!(s, ImgState::Ready(_)))
            .count();
        let mut excess = ready.saturating_sub(max);
        if excess == 0 {
            return;
        }
        let mut dropped = false;
        let map = &mut self.map;
        self.order.retain(|url| {
            let droppable = excess > 0
                && !protect.contains(url.as_str())
                && matches!(map.get(url), Some(ImgState::Ready(_)));
            if droppable {
                map.remove(url);
                excess -= 1;
                dropped = true;
            }
            !droppable && map.contains_key(url)
        });
        // Over the limit with every picture protected, nothing goes and nothing differs: saying so would make the open
        // chat's messages with pictures be laid out again at every frame (`layout_transcript` asks each time) for as long
        // as the pictures stay on or near the screen.
        if !dropped {
            return;
        }
        let map = &self.map;
        self.art_cache()
            .retain(|(url, ..), _| map.contains_key(url));
        self.version += 1;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Prose(String),
    Code { lang: String, code: String },
    Image { url: String, alt: String },
}

pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    /// Destinations of the numbered links: `[1]` is `links[0]`.
    pub links: Vec<String>,
    /// Image URLs referenced, in order.
    pub images: Vec<String>,
    /// Rows (`start..end` into `lines`) occupied by each fetchable image, with its URL: what to load when the rows come
    /// near the screen, and the click targets once the image has arrived.
    pub image_rows: Vec<(std::ops::Range<usize>, String)>,
    /// Rows (`start..end` into `lines`) of each `[n]` line of the link list, with its full destination: a click on
    /// any of them opens it, since a terminal's own link detection sees only one row of a wrapped address.
    pub link_rows: Vec<(std::ops::Range<usize>, String)>,
    /// Where each link is drawn, for marking it as a terminal hyperlink: a line, the columns of it the link's text covers,
    /// and the destination. Both a label in the text and the rows of its `[n]` entry are here, one item per line.
    pub link_cells: Vec<LinkCells>,
}

/// A line of [`Rendered::lines`], the columns of it a link covers, and where the link goes.
pub type LinkCells = (usize, std::ops::Range<usize>, String);

// ---------------------------------------------------------------------------
// Segmenting
// ---------------------------------------------------------------------------

struct ImageRef {
    start: usize,
    end: usize,
    alt: String,
    src: String,
}

/// The next match `search` finds at or after `from`, keeping the answer in `cached` while it still lies ahead of
/// wherever this is asked next, so a run of calls that advance monotonically (as every caller here does) costs one
/// search, not one per call, even across a stretch with no match at all (`Some(None)`, cheaply reused once failed).
fn find_cached(
    cached: &mut Option<Option<usize>>,
    from: usize,
    search: impl FnOnce(usize) -> Option<usize>,
) -> Option<usize> {
    match *cached {
        Some(None) => None,
        Some(Some(at)) if at >= from => Some(at),
        _ => {
            let found = search(from);
            *cached = Some(found);
            found
        }
    }
}

/// The next occurrence of `needle` in `haystack` at or after `from`.
fn find_from(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    haystack[from..].find(needle).map(|rel| from + rel)
}

/// 1-based position of `dest` in `links`, appending it first if it is not already there.
fn link_number(links: &mut Vec<String>, dest: &str) -> usize {
    links.iter().position(|l| l == dest).unwrap_or_else(|| {
        links.push(dest.to_string());
        links.len() - 1
    }) + 1
}

/// First `![alt](src)` in `s`, including a surrounding `[...](link)` wrapper.
fn find_image(s: &str) -> Option<ImageRef> {
    let mut from = 0;
    // The next `](` is searched for once and kept while it lies ahead (see `number_links`). The closing paren search
    // is not cached the same way: unlike `number_links`, this function returns as soon as one image is found or ruled
    // out, so it is called at most once per invocation.
    let mut close_at: Option<Option<usize>> = None;
    while let Some(rel) = s[from..].find("![") {
        let start = from + rel;
        from = start + 2;
        let after_bang = start + 2;
        // No ]( after this point means no image can start after it.
        let close = find_cached(&mut close_at, after_bang, |from| find_from(s, "](", from))?;
        let alt = &s[after_bang..close];
        if alt.len() > MAX_ALT_BYTES || alt.contains('\n') {
            continue;
        }
        let src_start = close + 2;
        let paren = find_unescaped_close_paren(s, src_start)?;
        let inner = &s[src_start..paren];
        let src = inner
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches(['<', '>']);
        let src = unescape_destination(src);
        let mut end = paren + 1;
        let mut begin = start;
        // A wrapping link's own destination is not kept (an image wrapped in a link is just drawn as the image), but
        // its closing paren must still be found the same escape-aware way to land on the right `end`.
        if s[..start].ends_with('[')
            && s[end..].starts_with("](")
            && let Some(p) = find_unescaped_close_paren(s, end + 2)
        {
            begin = start - 1;
            end = p + 1;
        }
        return Some(ImageRef {
            start: begin,
            end,
            alt: alt.trim().to_string(),
            src,
        });
    }
    None
}

/// Longest alternative text an image reference may have; longer is prose that happens to contain `![`.
const MAX_ALT_BYTES: usize = 1_000;
pub fn split_segments(md: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut prose = String::new();
    // The fence travels with the open block, so a `~~~` line cannot close a `` ``` `` block or the reverse, and a
    // shorter run cannot close a longer one (code that itself contains a `` ``` `` line is fenced with four).
    let mut code: Option<(crate::fence::Fence, String, String)> = None;

    let flush = |prose: &mut String, out: &mut Vec<Segment>| {
        let t = prose.trim_matches('\n');
        if !t.trim().is_empty() {
            out.push(Segment::Prose(t.to_string()));
        }
        prose.clear();
    };

    for line in md.lines() {
        let closes = code
            .as_ref()
            .is_some_and(|&(open, ..)| crate::fence::fence_closes(line, open));
        if closes {
            if let Some((_, lang, body)) = code.take() {
                out.push(Segment::Code {
                    lang,
                    code: body.trim_end_matches('\n').to_string(),
                });
            }
            continue;
        }
        if code.is_none()
            && let Some((open, info)) = crate::fence::fence_marker(line)
        {
            flush(&mut prose, &mut out);
            code = Some((
                open,
                info.split_whitespace().next().unwrap_or("").to_string(),
                String::new(),
            ));
            continue;
        }
        if let Some((_, _, body)) = &mut code {
            body.push_str(line);
            body.push('\n');
            continue;
        }
        let mut rest = line;
        while let Some(img) = find_image(rest) {
            prose.push_str(&rest[..img.start]);
            flush(&mut prose, &mut out);
            if img.src.starts_with("http") {
                out.push(Segment::Image {
                    url: img.src,
                    alt: img.alt,
                });
            }
            rest = &rest[img.end..];
        }
        prose.push_str(rest);
        prose.push('\n');
    }
    // An unterminated fence (message cut off, or still being typed) is still code.
    if let Some((_, lang, body)) = code {
        out.push(Segment::Code {
            lang,
            code: body.trim_end_matches('\n').to_string(),
        });
    }
    flush(&mut prose, &mut out);
    out
}

/// Whether `text` reads as a web address, which a reader will take for the destination of the link.
fn looks_like_address(text: &str) -> bool {
    let t = text.trim_start().to_ascii_lowercase();
    t.starts_with("http://") || t.starts_with("https://") || t.starts_with("www.")
}

/// Whether the visible `text` and the destination `dest` are the same address, ignoring case, the scheme, a leading
/// `www.` and a trailing slash.
fn same_address(text: &str, dest: &str) -> bool {
    let plain = |s: &str| {
        let s = s.trim().to_ascii_lowercase();
        let s = s
            .strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
            .unwrap_or(&s);
        s.strip_prefix("www.")
            .unwrap_or(s)
            .trim_end_matches('/')
            .to_string()
    };
    plain(text) == plain(dest)
}

/// Whether `text` is the start of the address `dest` cut short with an ellipsis, the way a sender's client shortens a
/// long address it shows (Markdown's backslash escapes aside, and ignoring case).
fn shortened_address(text: &str, dest: &str) -> bool {
    let t = text.trim().replace('\\', "");
    let Some(start) = t.strip_suffix('…').or_else(|| t.strip_suffix("...")) else {
        return false;
    };
    let start = start.to_ascii_lowercase();
    !start.is_empty() && dest.to_ascii_lowercase().starts_with(&start)
}

/// Whether the label `text` of a link to `dest` may itself be made a link to `dest` in the terminal: anything but an
/// address that names a different one, which would send a reader somewhere other than what they read (that label is
/// still numbered, and the `[n]` list says where it really goes).
fn label_may_link(text: &str, dest: &str) -> bool {
    !looks_like_address(text) || same_address(text, dest) || shortened_address(text, dest)
}

/// Position of the first `)` in `md[from..]` that is not escaped with a backslash, or `None` if there is none.
/// htmd escapes every literal `(` or `)` inside a link destination as `\(` / `\)` (`escape_link_destination` in its
/// anchor handler), which Microsoft docs and Wikipedia URLs routinely contain, so the markdown syntax's own closing
/// paren for the link is the first *unescaped* `)`, not simply the first one.
fn find_unescaped_close_paren(md: &str, from: usize) -> Option<usize> {
    let bytes = md.as_bytes();
    let mut at = from;
    loop {
        let found = at + md[at..].find(')')?;
        if found > 0 && bytes[found - 1] == b'\\' {
            at = found + 1;
            continue;
        }
        return Some(found);
    }
}

/// Undo the escaping [`find_unescaped_close_paren`] accounts for, on a destination extracted up to it.
fn unescape_destination(dest: &str) -> String {
    if !dest.contains('\\') {
        return dest.to_string();
    }
    // CommonMark: a backslash before any ASCII punctuation stands for that character, in a destination as anywhere
    // (htmd writes `\(`, `\)` and `\&` there); before anything else it is a backslash.
    let mut out = String::with_capacity(dest.len());
    let mut chars = dest.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(&next) = chars.peek()
            && next.is_ascii_punctuation()
        {
            out.push(next);
            chars.next();
        } else {
            out.push(c);
        }
    }
    out
}

/// The destination of an inline link whose parentheses hold `inside`, or `None` when that is not a link destination:
/// one token (optionally in `<>`), then optionally a title in quotes or parentheses, and the token must be an address
/// with a scheme (`https:`, `mailto:`, `tel:`, ...). Anything else between `](` and `)` is just text that happens to
/// follow a bracket, such as an index and a call (`handlers[i](e)`) or a remark (`[WIP](do not merge)`).
fn link_destination(inside: &str) -> Option<String> {
    let inside = inside.trim();
    let token = inside.split_whitespace().next()?;
    let title = inside[token.len()..].trim();
    let quoted = |open: char, close: char| {
        title.len() >= 2 && title.starts_with(open) && title.ends_with(close)
    };
    if !(title.is_empty() || quoted('"', '"') || quoted('\'', '\'') || quoted('(', ')')) {
        return None;
    }
    let dest = unescape_destination(token.trim_matches(['<', '>']));
    let scheme = dest.split_once(':')?.0;
    (scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
    .then_some(dest)
}

/// Replace inline links `[text](dest)` with `text[n]`, appending each distinct destination to `links`.
/// termimad would otherwise print the raw Markdown. Bare-URL links collapse to the URL.
#[cfg(test)]
pub fn number_links(md: &str, links: &mut Vec<String>) -> String {
    number_links_marked(md, links, false)
}

/// Replace inline links `[text](dest)` with `text [n]` (see `number_links`), with `mark` also putting a link marker ([`inline_style::Format::link`]) around each label that may
/// be a terminal hyperlink, so [`inline_style::apply_linked`] can say where it was laid out.
fn number_links_marked(md: &str, links: &mut Vec<String>, mark: bool) -> String {
    let link_label = |text: &str, n: usize, dest: &str| {
        if mark && label_may_link(text, dest) {
            inline_style::wrap(
                inline_style::Format {
                    link: u16::try_from(n).ok(),
                    ..inline_style::Format::default()
                },
                text,
            )
        } else {
            text.to_string()
        }
    };
    let mut out = String::with_capacity(md.len());
    // Everything before `done` has been dealt with. The positions of the next `](` and `)` are looked for once and
    // kept while they still lie ahead, so a run of `[` with no link after it costs one search, not one per bracket.
    let mut done = 0;
    let mut close_at: Option<Option<usize>> = None;
    let mut paren_at: Option<Option<usize>> = None;
    while let Some(rel) = md[done..].find('[') {
        let open = done + rel;
        let text_start = open + 1;
        let Some(close) = find_cached(&mut close_at, text_start, |from| find_from(md, "](", from))
        else {
            break; // no link can start after this point
        };
        let dest_start = close + 2;
        let Some(end) = find_cached(&mut paren_at, dest_start, |from| {
            find_unescaped_close_paren(md, from)
        }) else {
            break;
        };
        let text = &md[text_start..close];
        if !(text.len() < 400 && !text.contains('[') && !text.contains("\n\n")) {
            out.push_str(&md[done..=open]);
            done = text_start;
            continue;
        }
        let Some(dest) = link_destination(&md[dest_start..end]) else {
            // Not a link at all (`handlers[i](e)`, `[WIP](do not merge)`): left exactly as written, and the search
            // goes on from just after the bracket, so nothing up to the next parenthesis is swallowed.
            out.push_str(&md[done..=open]);
            done = text_start;
            continue;
        };
        out.push_str(&md[done..open]);
        let text = text.replace('\n', " ");
        let ok = ["http://", "https://", "mailto:"]
            .iter()
            .any(|p| dest.starts_with(p));
        // Text that looks like an address but names a different one is numbered like any other label, so the real
        // destination is what the link list (and the status line on opening) shows.
        if ok
            && !text.trim().is_empty()
            && !(looks_like_address(&text) && same_address(&text, &dest))
        {
            let n = link_number(links, &dest);
            // A space before the number, not `text[n]` glued together: when `text` is itself a bare URL
            // (as it is for `[https://a](https://b)`, the mismatch case above), gluing the number onto it
            // makes it read, and get matched by a terminal's own URL detector, as part of the address.
            out.push_str(&format!("{} [{n}]", link_label(&text, n, &dest)));
        } else if ok {
            // Link text is the URL itself (or empty): keep it visible and openable.
            let n = link_number(links, &dest);
            let shown = if text.trim().is_empty() { &dest } else { &text };
            out.push_str(&link_label(shown, n, &dest));
        } else {
            out.push_str(&text);
        }
        done = end + 1;
    }
    out.push_str(&md[done..]);
    out
}
// ---------------------------------------------------------------------------
// Line helpers
// ---------------------------------------------------------------------------

pub fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| s.content.width()).sum()
}

/// Split a styled line into rows no wider than `width` columns.
pub fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    if line_width(line) <= width {
        return vec![line.clone()];
    }
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in &line.spans {
        let mut buf = String::new();
        // By grapheme cluster, not by `char`: a variation selector or combining mark can change the width of the
        // base character it follows (a heart alone is narrow, heart + U+FE0F is drawn wide), so measuring each `char`
        // on its own and summing undercounts the cluster and wraps a little later than the terminal actually will.
        for cluster in span.content.graphemes(true) {
            let w = cluster.width();
            // Only break a row that already holds something: a cluster wider than `width` on its own (a ZWJ
            // sequence can be, since `unicode-width` sums its parts rather than collapsing it) must still go
            // somewhere, and breaking before it when the row is empty would insert a blank row ahead of it instead.
            if used > 0 && used + w > width {
                if !buf.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut buf), span.style));
                }
                rows.push(Line::from(std::mem::take(&mut cur)));
                used = 0;
            }
            buf.push_str(cluster);
            used += w;
        }
        if !buf.is_empty() {
            cur.push(Span::styled(buf, span.style));
        }
    }
    rows.push(Line::from(cur));
    rows
}

fn pad_to(spans: &mut Vec<Span<'static>>, used: usize, width: usize, style: Style) {
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), style));
    }
}

// ---------------------------------------------------------------------------
// Code card
// ---------------------------------------------------------------------------

fn code_card(lang: &str, code: &str, width: usize, coloured: bool) -> Vec<Line<'static>> {
    let width = width.max(12);
    let inner_max = width - 4;
    // Teams drops the tag of a language it does not know (ObjectScript), so an untagged block may be one.
    let lang = if lang.trim().is_empty() {
        highlight::guess_language(code).unwrap_or(lang)
    } else {
        lang
    };
    let mut body: Vec<Line<'static>> = Vec::new();
    let lines = if coloured {
        highlight::highlight(code, lang)
    } else {
        highlight::uncoloured(code)
    };
    for l in lines {
        body.extend(wrap_line(&l, inner_max));
    }
    let label = highlight::language_label(lang);
    let content_w = body
        .iter()
        .map(line_width)
        .max()
        .unwrap_or(0)
        .max(label.width() + 2);
    let inner = content_w.min(inner_max);
    let border = Style::new().fg(CODE_BORDER);
    let bg = Style::new().bg(CODE_BG);

    let mut out = Vec::with_capacity(body.len() + 2);
    // ╭─ rust ───────╮
    let fill = (inner + 2).saturating_sub(label.width() + 3);
    out.push(Line::from(vec![
        Span::styled("╭─ ", border),
        Span::styled(
            label,
            Style::new().fg(CODE_LABEL).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {}╮", "─".repeat(fill)), border),
    ]));
    for l in body {
        let used = line_width(&l);
        // A line the highlighter tinted (an added or removed line of a diff) is tinted to its end.
        let line_bg = l
            .spans
            .last()
            .and_then(|s| s.style.bg)
            .map_or(bg, |c| Style::new().bg(c));
        let mut spans = vec![Span::styled("│", border), Span::styled(" ", line_bg)];
        spans.extend(l.spans.into_iter().map(|mut s| {
            if s.style.bg.is_none() {
                s.style = s.style.bg(CODE_BG);
            }
            if s.style.fg.is_none() {
                s.style = s.style.fg(CODE_FG);
            }
            s
        }));
        pad_to(&mut spans, used, inner, line_bg);
        spans.push(Span::styled(" ", line_bg));
        spans.push(Span::styled("│", border));
        out.push(Line::from(spans));
    }
    out.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner + 2)),
        border,
    )));
    out
}

// ---------------------------------------------------------------------------
// Whole bodies
// ---------------------------------------------------------------------------

pub struct Ctx<'a> {
    pub images: &'a ImageStore,
    pub mode: ImgMode,
    /// Spinner frame for pending images (kept stable across renders so the cache is not invalidated per tick).
    pub spinner: &'a str,
    /// Whether an image URL is loaded automatically: only content hosted by the service we are signed in to is (see
    /// `Backend::serves_image`). Any other image is listed as a link, because fetching a URL the sender chose would
    /// reveal our IP address.
    pub fetchable: &'a dyn Fn(&str) -> bool,
    /// The active keymap, for keymap-specific help text.
    pub keymap: crate::editor::Keymap,
}

fn image_lines(url: &str, alt: &str, width: usize, ctx: &Ctx<'_>) -> Vec<Line<'static>> {
    let dim = Style::new().fg(DIM);
    let caption = |text: String| Line::from(Span::styled(text, dim));
    if ctx.mode == ImgMode::Off {
        return vec![caption(format!(
            "🖼  {}",
            if alt.is_empty() { "image" } else { alt }
        ))];
    }
    match ctx.images.map.get(url) {
        Some(ImgState::Ready(img)) => {
            let (cols, rows) = imgart::budget(img, width);
            let mut lines = ctx.images.art(url, img, ctx.mode, cols, rows).to_vec();
            if !alt.is_empty() && alt != "image" && !lines.is_empty() {
                lines.push(caption(format!("↑ {alt}")));
            }
            lines
        }
        Some(ImgState::Failed(why)) => vec![caption(format!("🖼  image unavailable ({why})"))],
        Some(ImgState::Unverified(_)) => {
            let key = match ctx.keymap {
                crate::editor::Keymap::Vim => "L",
                crate::editor::Keymap::Standard => "Ctrl-L",
            };
            vec![caption(format!(
                "🖼  from outside your organisation, not loaded ({key} to load)"
            ))]
        }
        _ => vec![caption(format!("{} loading image…", ctx.spinner))],
    }
}

/// Render a Markdown message body for `width` columns.
pub fn render_body(md: &str, width: usize, ctx: &Ctx<'_>) -> Rendered {
    let width = width.max(16);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut links = Vec::new();
    let mut images = Vec::new();
    let mut image_rows = Vec::new();
    let mut link_cells: Vec<LinkCells> = Vec::new();
    let mut code_blocks = 0usize;
    let mut images_beyond_cap = 0usize;
    for seg in split_segments(md) {
        match seg {
            Segment::Prose(p) => {
                let p = number_links_marked(&p, &mut links, true);
                let (laid, runs) = inline_style::apply_linked(termtext::render_markdown(&p, width));
                let first = lines.len();
                link_cells.extend(runs.into_iter().filter_map(|(row, cols, n)| {
                    let url = links.get(usize::from(n).checked_sub(1)?)?;
                    Some((first + row, cols, url.clone()))
                }));
                lines.extend(laid);
            }
            Segment::Code { lang, code } => {
                // Colouring is work done on the UI thread for text a sender chose, so a message with a great many
                // blocks gets colour for the first few only.
                let coloured = code_blocks < MAX_COLOURED_BLOCKS;
                code_blocks += 1;
                lines.extend(code_card(
                    &lang,
                    &inline_style::strip(&code),
                    width,
                    coloured,
                ));
            }
            Segment::Image { url, alt } => {
                if !(ctx.fetchable)(&url) {
                    // Not auto-loaded: a remote image URL chosen by the sender would reveal our IP address.
                    let n = link_number(&mut links, &url);
                    let what = if alt.is_empty() {
                        "image"
                    } else {
                        alt.as_str()
                    };
                    lines.push(Line::from(Span::styled(
                        format!("🖼  external {what} [{n}] (not loaded)"),
                        Style::new().fg(DIM),
                    )));
                } else if images.len() < MAX_IMAGES_PER_MESSAGE {
                    let start = lines.len();
                    lines.extend(image_lines(&url, &alt, width, ctx));
                    image_rows.push((start..lines.len(), url.clone()));
                    images.push(url);
                } else {
                    images_beyond_cap += 1;
                }
            }
        }
    }
    if images_beyond_cap > 0 {
        lines.push(Line::from(Span::styled(
            if images_beyond_cap == 1 {
                "🖼  1 more picture not shown".to_string()
            } else {
                format!("🖼  {images_beyond_cap} more pictures not shown")
            },
            Style::new().fg(DIM),
        )));
    }
    // termimad leaves trailing blank rows after the last paragraph. A row of a picture's block art can have empty
    // text too (a flat-coloured cell is the glyph ' ', `imgart::render_fit`), carried only by its background colour,
    // so a row is blank here only when it has neither: otherwise the bottom of the picture gets trimmed away.
    while lines.last().is_some_and(|l| {
        l.spans
            .iter()
            .all(|s| s.content.trim().is_empty() && s.style.bg.is_none())
    }) {
        lines.pop();
    }
    for (rows, _) in &mut image_rows {
        rows.end = rows.end.min(lines.len());
    }
    link_cells.retain(|(line, ..)| *line < lines.len());
    let mut link_rows = Vec::new();
    let dim = Style::new().fg(DIM);
    for (i, url) in links.iter().enumerate() {
        // The whole address, wrapped under its number rather than cut: it is what is opened, so it is what is shown.
        let label = format!("[{}] ", i + 1);
        let indent = label.width();
        let room = width.saturating_sub(indent).max(1);
        let start = lines.len();
        for (n, piece) in split_by_width(url, room).into_iter().enumerate() {
            let lead = if n == 0 {
                Span::styled(
                    label.clone(),
                    Style::new().fg(LINK).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw(" ".repeat(indent))
            };
            link_cells.push((lines.len(), indent..indent + piece.width(), url.clone()));
            lines.push(Line::from(vec![lead, Span::styled(piece, dim)]));
        }
        link_rows.push((start..lines.len(), url.clone()));
    }
    Rendered {
        lines,
        links,
        images,
        image_rows,
        link_rows,
        link_cells,
    }
}

/// `s` cut into pieces no wider than `room` columns (at least one character each), by grapheme cluster.
fn split_by_width(s: &str, room: usize) -> Vec<String> {
    let mut pieces = vec![String::new()];
    let mut used = 0;
    for g in s.graphemes(true) {
        let w = g.width();
        let last = pieces.last_mut().expect("pieces starts with one");
        if used > 0 && used + w > room {
            pieces.push(g.to_string());
            used = w;
        } else {
            last.push_str(g);
            used += w;
        }
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn wrap_line_treats_a_variation_selector_emoji_as_one_wide_cluster() {
        // A plain heart is narrow; heart + U+FE0F (request emoji presentation) is drawn wide by any terminal that
        // honours it. Measuring each character on its own and summing, as the old code did, undercounts the
        // cluster and lets a row grow wider than asked for once it is actually drawn.
        let line = Line::from(vec![Span::raw("\u{2764}\u{FE0F}ab")]);
        let rows = wrap_line(&line, 2);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(
            text(&rows[0]),
            "\u{2764}\u{FE0F}",
            "fills the row on its own"
        );
        assert_eq!(text(&rows[1]), "ab");
    }

    #[test]
    fn wrap_line_does_not_insert_a_blank_row_before_a_cluster_wider_than_the_wrap_width() {
        // A ZWJ family emoji is one grapheme cluster, but `unicode-width` sums each person's own width rather than
        // collapsing the sequence, so the cluster alone is wider than this narrow wrap width.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        let line = Line::from(vec![Span::raw(format!("{family}ab"))]);
        let rows = wrap_line(&line, 2);
        assert_eq!(
            text(&rows[0]),
            family,
            "the oversized cluster starts the first row, with no blank row ahead of it: {rows:?}"
        );
        assert_eq!(text(&rows[1]), "ab");
        assert_eq!(rows.len(), 2, "{rows:?}");
    }

    #[test]
    fn segments_split_prose_code_and_images() {
        let md = "Intro\n\n![pic](https://x/a.png)\n\n```rust\nfn x() {}\n```\nOutro";
        let segs = split_segments(md);
        assert_eq!(segs.len(), 4, "{segs:?}");
        assert!(matches!(&segs[0], Segment::Prose(p) if p == "Intro"));
        assert!(
            matches!(&segs[1], Segment::Image { url, alt } if url == "https://x/a.png" && alt == "pic")
        );
        assert!(
            matches!(&segs[2], Segment::Code { lang, code } if lang == "rust" && code == "fn x() {}")
        );
        assert!(matches!(&segs[3], Segment::Prose(p) if p == "Outro"));
    }

    #[test]
    fn code_that_contains_a_fence_line_stays_one_block_until_a_fence_as_long_as_its_own() {
        // htmd fences code that contains a ``` line with four backticks.
        let segs = split_segments("````\n```\nx\n```\n````\nafter");
        assert_eq!(segs.len(), 2, "{segs:?}");
        assert!(
            matches!(&segs[0], Segment::Code { code, .. } if code == "```\nx\n```"),
            "{:?}",
            segs[0]
        );
        assert!(matches!(&segs[1], Segment::Prose(p) if p == "after"));
        // A line with a language is another block opening, never the end of this one.
        let segs = split_segments("```\nfirst\n```rust\nstill code\n```\nafter");
        assert!(
            matches!(&segs[0], Segment::Code { code, .. } if code == "first\n```rust\nstill code"),
            "{segs:?}"
        );
    }

    #[test]
    fn an_image_src_with_escaped_parentheses_is_not_truncated_at_the_first_one() {
        let md = r"before![pic](https://x/a\(1\).png)after";
        let segs = split_segments(md);
        assert_eq!(segs.len(), 3, "{segs:?}");
        assert!(matches!(&segs[0], Segment::Prose(p) if p == "before"));
        assert!(
            matches!(&segs[1], Segment::Image { url, .. } if url == "https://x/a(1).png"),
            "{:?}",
            segs[1]
        );
        assert!(matches!(&segs[2], Segment::Prose(p) if p == "after"));
    }

    #[test]
    fn an_image_wrapped_in_a_link_with_escaped_parentheses_in_its_destination_is_fully_consumed() {
        // The wrapper's own destination is not kept (the image is drawn, not a link), but its closing paren still
        // has to be the real one so nothing of it leaks into the surrounding prose.
        let md = r"before[![pic](https://x/a.png)](https://x/aa365247\(v=vs.85\))after";
        let segs = split_segments(md);
        assert_eq!(segs.len(), 3, "{segs:?}");
        assert!(matches!(&segs[0], Segment::Prose(p) if p == "before"));
        assert!(
            matches!(&segs[1], Segment::Image { url, .. } if url == "https://x/a.png"),
            "{:?}",
            segs[1]
        );
        assert!(
            matches!(&segs[2], Segment::Prose(p) if p == "after"),
            "{:?}",
            segs[2]
        );
    }

    #[test]
    fn art_is_fitted_once_per_size_and_dropped_with_its_picture() {
        let mut store = ImageStore::default();
        let img = Arc::new(RgbImage::from_pixel(32, 32, image::Rgb([200, 30, 30])));
        store.set("u", ImgState::Ready(Arc::clone(&img)));
        let a = store.art("u", &img, ImgMode::Blocks, 20, 5);
        assert!(
            Arc::ptr_eq(&a, &store.art("u", &img, ImgMode::Blocks, 20, 5)),
            "asking again does not fit again"
        );
        assert!(
            !Arc::ptr_eq(&a, &store.art("u", &img, ImgMode::Blocks, 21, 5)),
            "another width is another fit"
        );
        assert!(
            !Arc::ptr_eq(&a, &store.art("u", &img, ImgMode::Sextant, 20, 5)),
            "and so is another glyph set"
        );
        store.set("u", ImgState::Ready(Arc::clone(&img)));
        assert!(
            !Arc::ptr_eq(&a, &store.art("u", &img, ImgMode::Blocks, 20, 5)),
            "a picture stored again is fitted again"
        );
        store.evict(0, &HashSet::new());
        assert!(store.art_cache().is_empty(), "art goes with its picture");
    }

    #[test]
    fn evicting_changes_the_version_only_when_a_picture_goes() {
        let ready = || ImgState::Ready(Arc::new(RgbImage::from_pixel(2, 2, image::Rgb([9, 9, 9]))));
        let mut store = ImageStore::default();
        for n in 0..4 {
            store.set(&format!("u{n}"), ready());
        }
        let all: HashSet<&str> = ["u0", "u1", "u2", "u3"].into();
        // Within the limit, and over it with every picture protected: nothing is dropped and nothing is different, so
        // nothing that is laid out from the store has to be laid out again.
        let before = store.version;
        store.evict(4, &all);
        store.evict(2, &all);
        assert_eq!(store.version, before, "nothing went");
        assert_eq!(store.map.len(), 4);
        // One that is not protected goes, the oldest first, and that is a change.
        let kept: HashSet<&str> = ["u2", "u3"].into();
        store.evict(3, &kept);
        assert_eq!(store.version, before + 1);
        assert!(!store.map.contains_key("u0") && store.map.contains_key("u1"));
        // And the next time round, with nothing over the limit, nothing changes again.
        store.evict(3, &kept);
        assert_eq!(store.version, before + 1);
    }

    #[test]
    fn redrawing_a_message_with_a_picture_reuses_its_art() {
        let mut store = ImageStore::default();
        let img = Arc::new(RgbImage::from_pixel(64, 64, image::Rgb([20, 120, 200])));
        store.set("https://demo.invalid/p", ImgState::Ready(img));
        let serves = |u: &str| u.starts_with("https://demo.invalid/");
        let draw = |spinner: &str| {
            let ctx = Ctx {
                images: &store,
                mode: ImgMode::Blocks,
                spinner,
                fetchable: &serves,
                keymap: crate::editor::Keymap::default(),
            };
            render_body("![p](https://demo.invalid/p)", 60, &ctx).lines
        };
        let first = draw("a");
        let again = draw("b");
        assert_eq!(first, again);
        assert_eq!(store.art_cache().len(), 1, "one fit served both draws");
    }

    #[test]
    fn a_uniformly_coloured_picture_is_not_mistaken_for_trailing_blank_rows() {
        let mut store = ImageStore::default();
        let img = Arc::new(RgbImage::from_pixel(64, 64, image::Rgb([80, 80, 80])));
        store.set("https://demo.invalid/p", ImgState::Ready(Arc::clone(&img)));
        let serves = |u: &str| u.starts_with("https://demo.invalid/");
        let ctx = Ctx {
            images: &store,
            mode: ImgMode::Blocks,
            spinner: "-",
            fetchable: &serves,
            keymap: crate::editor::Keymap::default(),
        };
        let (cols, rows) = imgart::budget(&img, 60);
        let art_rows = store
            .art("https://demo.invalid/p", &img, ImgMode::Blocks, cols, rows)
            .len();
        let rendered = render_body("![](https://demo.invalid/p)", 60, &ctx);
        assert_eq!(
            rendered.image_rows[0].0.len(),
            art_rows,
            "every row of art is a flat colour (no visible glyph text) and must survive the trailing-blank trim"
        );
        assert_eq!(rendered.lines.len(), art_rows);
    }

    #[test]
    fn images_inside_fences_are_code() {
        let segs = split_segments("```\n![not](https://x/y.png)\n```");
        assert_eq!(
            segs,
            vec![Segment::Code {
                lang: String::new(),
                code: "![not](https://x/y.png)".into()
            }]
        );
    }

    #[test]
    fn unterminated_fence_is_still_code() {
        let segs = split_segments("```python\nprint(1)");
        assert_eq!(
            segs,
            vec![Segment::Code {
                lang: "python".into(),
                code: "print(1)".into()
            }]
        );
    }

    #[test]
    fn links_are_numbered_and_listed() {
        let mut links = Vec::new();
        let out = number_links(
            "see [docs](https://a.example/d) and [again](https://a.example/d), [b](https://b.example)",
            &mut links,
        );
        assert_eq!(out, "see docs [1] and again [1], b [2]");
        assert_eq!(links.len(), 2);
    }

    #[test]
    fn brackets_followed_by_parentheses_that_are_not_a_link_are_left_as_written() {
        for text in [
            "handlers[i](e) and more",
            "[WIP](do not merge)",
            "a[0](x) then b[1](y z)",
            "empty [text]() stays",
            "see [this] (but not that)",
        ] {
            let mut links = Vec::new();
            assert_eq!(number_links(text, &mut links), text, "{text}");
            assert!(links.is_empty(), "{text}");
        }
        // And a real link after such text is still found, with nothing in between swallowed.
        let mut links = Vec::new();
        let out = number_links("a[0](x) then [t](https://e.example)", &mut links);
        assert_eq!(out, "a[0](x) then t [1]");
        assert_eq!(links, ["https://e.example"]);
    }

    #[test]
    fn a_link_may_carry_a_title_and_one_to_an_address_other_than_the_web_shows_just_its_text() {
        let mut links = Vec::new();
        assert_eq!(
            number_links(
                r#"[t](https://e.example "A title") [f](ftp://h/f) [x](<https://b.example>)"#,
                &mut links
            ),
            "t [1] f x [2]"
        );
        assert_eq!(links, ["https://e.example", "https://b.example"]);
    }

    #[test]
    fn a_destination_with_escaped_parentheses_is_not_truncated_at_the_first_one() {
        // htmd escapes every literal `(`/`)` in a destination as `\(`/`\)` (checked against htmd 0.5.6's
        // `escape_link_destination`), which a Microsoft docs or Wikipedia URL routinely contains.
        let mut links = Vec::new();
        let out = number_links(
            r"see [docs](https://a.example/aa365247\(v=vs.85\)) now",
            &mut links,
        );
        assert_eq!(out, "see docs [1] now", "no stray ) is left in the text");
        assert_eq!(links, ["https://a.example/aa365247(v=vs.85)"], "unescaped");
    }

    #[test]
    fn a_label_that_looks_like_an_address_but_names_another_is_numbered() {
        let mut links = Vec::new();
        let out = number_links(
            "[https://www.microsoft.com/login](https://evil.example/login) and [www.a.example/](https://a.example)",
            &mut links,
        );
        assert_eq!(
            out,
            "https://www.microsoft.com/login [1] and www.a.example/"
        );
        assert_eq!(
            links,
            vec![
                "https://evil.example/login".to_string(),
                "https://a.example".to_string()
            ],
            "the real destination is listed; spelling variants of the same address are not a mismatch"
        );
    }
    #[test]
    fn brackets_without_links_cost_time_in_proportion_to_their_number() {
        // 20,000 characters of the shapes that make a naive scan start over at every bracket.
        let started = std::time::Instant::now();
        for shape in ["[", "![", "[a](", "![a]( ", "[a] ", "![a\nb"] {
            let hostile = shape.repeat(20_000 / shape.len());
            let mut links = Vec::new();
            let out = number_links(&hostile, &mut links);
            assert!(out.len() >= hostile.len() / 2);
            let _ = find_image(&hostile);
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn links_and_images_are_still_found_after_a_long_run_of_brackets() {
        let md = format!(
            "{}![pic](https://a.example/p.png) and [t](https://b.example)",
            "[".repeat(5_000)
        );
        let image = find_image(&md).unwrap();
        assert_eq!(image.src, "https://a.example/p.png");
        let mut links = Vec::new();
        let out = number_links(&md, &mut links);
        assert!(out.ends_with("and t [2]"), "{out}");
        assert_eq!(links, ["https://a.example/p.png", "https://b.example"]);
    }
    #[test]
    fn bare_url_links_stay_readable() {
        let mut links = Vec::new();
        let out = number_links("[https://a.example](https://a.example)", &mut links);
        assert_eq!(out, "https://a.example");
        assert_eq!(links, vec!["https://a.example".to_string()]);
    }

    #[test]
    fn a_diff_tints_its_added_and_removed_lines_to_the_end_and_leaves_the_rest() {
        let card = code_card(
            "diff",
            "diff --git a/x b/x\n@@ -1 +1 @@\n context\n-old line\n+new line",
            60,
            true,
        );
        let bg_of = |row: usize| -> Vec<Option<ratatui::style::Color>> {
            card[row].spans.iter().map(|s| s.style.bg).collect()
        };
        let text =
            |row: usize| -> String { card[row].spans.iter().map(|s| s.content.as_ref()).collect() };
        let find = |needle: &str| (0..card.len()).find(|&r| text(r).contains(needle)).unwrap();
        let (context, removed, added) = (find("context"), find("-old"), find("+new"));
        // Context stays on the card colour; removed and added each have their own tint, which reaches the right edge.
        assert!(
            bg_of(context)
                .iter()
                .skip(1)
                .take(bg_of(context).len() - 2)
                .all(|b| *b == Some(CODE_BG))
        );
        let tint = |row: usize| bg_of(row)[bg_of(row).len() - 2];
        assert_ne!(tint(removed), Some(CODE_BG));
        assert_ne!(tint(added), Some(CODE_BG));
        assert_ne!(tint(removed), tint(added));
        assert_eq!(bg_of(removed)[1], tint(removed), "from the left edge too");
    }

    #[test]
    fn code_cards_have_equal_width_rows_and_a_language_label() {
        let card = code_card("rust", "fn main() {\n    println!(\"hello\");\n}", 60, true);
        assert!(text(&card[0]).contains("rust"));
        let w = line_width(&card[0]);
        assert!(
            card.iter().all(|l| line_width(l) == w),
            "{:?}",
            card.iter().map(text).collect::<Vec<_>>()
        );
        assert_eq!(card.len(), 5);
    }

    #[test]
    fn long_code_lines_wrap_inside_the_card() {
        let card = code_card("", &"x".repeat(100), 30, true);
        assert!(card.iter().all(|l| line_width(l) <= 30));
        assert!(card.len() > 3);
    }

    #[test]
    fn a_destination_s_backslash_escapes_of_punctuation_are_taken_out_and_any_other_backslash_kept()
    {
        let mut links = Vec::new();
        let out = number_links(
            r"[file](https://e.example/a\(b\)?x=1\&y=2\#L7) [w](https://e.example/C:\dir\_x)",
            &mut links,
        );
        assert_eq!(
            links,
            [
                "https://e.example/a(b)?x=1&y=2#L7",
                r"https://e.example/C:\dir_x",
            ],
            "{out}"
        );
    }

    #[test]
    fn a_long_link_is_listed_in_full_over_several_rows_under_its_number() {
        let store = ImageStore::default();
        let ctx = Ctx {
            images: &store,
            mode: ImgMode::DEFAULT,
            spinner: "⠋",
            fetchable: &|_| false,
            keymap: crate::editor::Keymap::default(),
        };
        let url = "https://code.example.com/group/project/-/blob/main/src/main/module/Service/Dispatch.cls?blame=1&ref_type=heads#L72";
        let r = render_body(
            &format!("See [the file]({url}) and [a short one](https://e.example/x)."),
            40,
            &ctx,
        );
        assert_eq!(r.link_rows.len(), 2, "{:?}", r.link_rows);
        let (rows, dest) = &r.link_rows[0];
        assert_eq!(dest, url);
        assert!(rows.len() > 1, "{rows:?}");
        let shown: Vec<String> = r.lines[rows.clone()].iter().map(text).collect();
        assert!(shown[0].starts_with("[1] "), "{shown:?}");
        assert!(
            shown[1..].iter().all(|l| l.starts_with("    ")),
            "continuations line up under the address: {shown:?}"
        );
        let whole: String = shown.iter().map(|l| &l[4..]).collect();
        assert_eq!(
            whole, url,
            "every character of the address is drawn, none cut"
        );
        assert!(r.lines.iter().all(|l| line_width(l) <= 40));
        let (rows, dest) = &r.link_rows[1];
        assert_eq!((rows.len(), dest.as_str()), (1, "https://e.example/x"));
        assert_eq!(text(&r.lines[rows.start]), "[2] https://e.example/x");
    }

    #[test]
    fn body_renders_everything_within_width() {
        let store = ImageStore::default();
        let ctx = Ctx {
            images: &store,
            mode: ImgMode::DEFAULT,
            spinner: "⠋",
            fetchable: &|u| u.starts_with("https://demo.invalid/"),
            keymap: crate::editor::Keymap::default(),
        };
        let md = "# Title\n\nSome **bold** [link](https://e.example/x) text.\n\n```objectscript\nWrite \"hi\",!\n```\n\n![img](https://demo.invalid/plasma/1)\n\n| a | b |\n|---|---|\n| 1 | 2 |";
        let r = render_body(md, 50, &ctx);
        let all: Vec<String> = r.lines.iter().map(text).collect();
        let joined = all.join("\n");
        assert!(
            joined.contains("Title")
                && joined.contains("ObjectScript")
                && joined.contains("loading image"),
            "{joined}"
        );
        assert!(joined.contains("[1] https://e.example/x"), "{joined}");
        assert_eq!(r.images, vec!["https://demo.invalid/plasma/1".to_string()]);
        for l in &r.lines {
            assert!(line_width(l) <= 50, "too wide: {:?}", text(l));
        }
        assert!(!joined.contains('\x1b'));
    }

    fn colours_in(lines: &[Line<'_>]) -> usize {
        let set: HashSet<String> = lines
            .iter()
            .flat_map(|l| l.spans.iter().filter_map(|s| s.style.fg))
            .map(|c| format!("{c:?}"))
            .collect();
        set.len()
    }

    fn body_lines(html: &str) -> Vec<Line<'static>> {
        let store = ImageStore::default();
        let ctx = Ctx {
            images: &store,
            mode: ImgMode::DEFAULT,
            spinner: "⠋",
            fetchable: &|_| false,
            keymap: crate::editor::Keymap::default(),
        };
        render_body(&crate::convert::html_to_markdown(html, "me"), 100, &ctx).lines
    }

    /// The spans whose text contains `needle`.
    fn spans_with<'a>(lines: &'a [Line<'static>], needle: &str) -> Vec<&'a Span<'static>> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.content.contains(needle))
            .collect()
    }

    #[test]
    fn underline_strikethrough_colour_and_highlight_from_teams_are_drawn() {
        // As the Teams client stored a message with each of them (the text is the same; only the ids are not here).
        let html = "<p>This <i>italic</i> is <strong>bold</strong> a <span style=\"color:#B6424C\">red </span>test, <s>strikethrough </s>and <u>underline</u></p>\r\n<ul>\r\n<li>bullet 1</li><li>bullet 2</li></ul>\r\n<blockquote>\r\n<p>Block quote</p>\r\n</blockquote>\r\n<p>This is <span style=\"background-color:#9DD9DB\">highlighted blue</span></p>";
        let lines = body_lines(html);
        let all: String = lines.iter().map(text).collect::<Vec<_>>().join("\n");
        assert!(
            all.contains("a red test, strikethrough and underline"),
            "{all}"
        );
        assert!(
            !all.chars().any(|c| ('\u{E000}'..='\u{F8FF}').contains(&c)),
            "no marker is left in what is drawn: {all:?}"
        );
        let struck = spans_with(&lines, "strikethrough");
        assert!(struck[0].style.add_modifier.contains(Modifier::CROSSED_OUT));
        let under = spans_with(&lines, "underline");
        assert!(under[0].style.add_modifier.contains(Modifier::UNDERLINED));
        let red = spans_with(&lines, "red");
        let Some(ratatui::style::Color::Rgb(r, g, b)) = red[0].style.fg else {
            panic!("red has a colour: {:?}", red[0])
        };
        assert!(r > g && r > b, "reddish: {r},{g},{b}");
        let marked = spans_with(&lines, "highlighted blue");
        assert_eq!(
            marked[0].style.bg,
            Some(ratatui::style::Color::Rgb(0x9D, 0xD9, 0xDB))
        );
        // Dark text on the light highlight.
        let Some(ratatui::style::Color::Rgb(r, g, b)) = marked[0].style.fg else {
            panic!("the highlighted text has a colour")
        };
        assert!(
            u16::from(r) + u16::from(g) + u16::from(b) < 300,
            "{r},{g},{b}"
        );
        // The words around them carry none of it.
        let plain = spans_with(&lines, "This is");
        assert!(plain[0].style.bg.is_none());
        assert!(!plain[0].style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn a_self_mention_and_another_persons_mention_are_coloured_differently() {
        // `body_lines` resolves "me" as the signed-in user's id (see its own `html_to_markdown(html, "me")`).
        let html = r#"<p><at id="me">Me</at> and <at id="ada">Ada</at> should both come</p>"#;
        let lines = body_lines(html);
        let mine = spans_with(&lines, "@Me");
        assert_eq!(mine[0].style.fg, Some(crate::theme::MENTION_ME));
        assert!(mine[0].style.add_modifier.contains(Modifier::BOLD));
        let other = spans_with(&lines, "@Ada");
        assert_eq!(
            other[0].style.fg,
            Some(crate::theme::LINK),
            "someone else's mention has no colour of its own; it's the same blue as a link"
        );
        assert!(other[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_reply_quotes_time_is_shown_dim_like_a_header_timestamp() {
        let ms: i64 = 1_793_979_227_035;
        let html = format!(
            "<blockquote itemscope itemtype=\"http://schema.skype.com/Reply\" itemid=\"{ms}\">\
             <strong itemprop=\"mri\" itemid=\"8:orgid:x\">Ada Lovelace</strong>\
             <span itemprop=\"time\" itemid=\"{ms}\"></span>\
             <p itemprop=\"preview\">hi</p></blockquote>"
        );
        let lines = body_lines(&html);
        let expected = chrono::DateTime::from_timestamp_millis(ms)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%-m/%-d/%Y %H:%M")
            .to_string();
        let time = spans_with(&lines, &expected);
        // Not exactly `theme::DIM`: this text goes through `inline_style::apply`, which lightens any colour
        // that falls under its contrast floor against the page -- `DIM`'s own raw value does, the same way a
        // sender's own dark colour would. The header's timestamp is a plain `Style`, never passed through that,
        // so it alone keeps the unadjusted, slightly dimmer value.
        let Some(ratatui::style::Color::Rgb(r, g, b)) = time[0].style.fg else {
            panic!("the quote's own time has a colour: {:?}", time[0])
        };
        assert!(
            r < 140 && g < 140 && b < 150 && b >= r && b >= g,
            "a muted blue-grey, not a bright or warm colour: {r},{g},{b}"
        );
    }

    #[test]
    fn a_sender_cannot_draw_with_our_markers_and_copied_text_has_none() {
        let forged = "<p>before \u{E005}hidden\u{E000} after</p>";
        let lines = body_lines(forged);
        let all: String = lines.iter().map(text).collect::<Vec<_>>().join("\n");
        assert_eq!(all.trim(), "before hidden after");
        assert!(
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .all(|s| s.style.bg.is_none()
                    && !s.style.add_modifier.contains(Modifier::UNDERLINED))
        );
        // Copying a message as Markdown gives Markdown, not private characters.
        let md = crate::convert::html_to_markdown("<p>an <u>underlined</u> word</p>", "me");
        assert!(md.chars().any(|c| ('\u{E000}'..='\u{F8FF}').contains(&c)));
        assert_eq!(inline_style::strip(&md), "an underlined word");
    }
    #[test]
    fn only_the_first_blocks_of_a_message_are_coloured() {
        let store = ImageStore::default();
        let ctx = Ctx {
            images: &store,
            mode: ImgMode::DEFAULT,
            spinner: "⠋",
            fetchable: &|u| u.starts_with("https://demo.invalid/"),
            keymap: crate::editor::Keymap::default(),
        };
        let block = "```rust\nfn main() { let x = \"a\"; }\n```\n\n";
        let r = render_body(&block.repeat(MAX_COLOURED_BLOCKS + 10), 60, &ctx);
        let card_len = 3 + 1; // header, one code row, footer, and the blank row between cards
        let first = &r.lines[..card_len - 1];
        let last = &r.lines[r.lines.len() - (card_len - 1)..];
        assert!(
            colours_in(first) > colours_in(last),
            "the last block is shown uncoloured"
        );
        assert_eq!(
            r.lines.iter().filter(|l| text(l).contains("╭─")).count(),
            MAX_COLOURED_BLOCKS + 10,
            "every block is still shown"
        );
    }

    #[test]
    fn a_message_of_many_tree_sitter_blocks_renders_quickly() {
        let store = ImageStore::default();
        let ctx = Ctx {
            images: &store,
            mode: ImgMode::DEFAULT,
            spinner: "⠋",
            fetchable: &|u| u.starts_with("https://demo.invalid/"),
            keymap: crate::editor::Keymap::default(),
        };
        let md = "```toml\nx = 1\n```\n\n".repeat(500)
            + &"```objectscript\nWrite 1\n```\n\n".repeat(500);
        let started = std::time::Instant::now();
        let r = render_body(&md, 60, &ctx);
        let took = started.elapsed();
        assert_eq!(
            r.lines.iter().filter(|l| text(l).contains("╭─")).count(),
            1000
        );
        // The highlighter is built once and shared (building one costs about 200 ms), and only the first
        // `MAX_COLOURED_BLOCKS` blocks are coloured, so a thousand blocks render in a fraction of a second.
        assert!(took < std::time::Duration::from_secs(10), "{took:?}");
    }

    #[test]
    fn images_beyond_the_cap_show_a_placeholder() {
        let store = ImageStore::default();
        let ctx = Ctx {
            images: &store,
            mode: ImgMode::Blocks,
            spinner: "⠋",
            fetchable: &|_| true,
            keymap: crate::editor::Keymap::default(),
        };
        let images: Vec<String> = (0..8)
            .map(|i| format!("https://example.com/image{i}.png"))
            .collect();
        let mut md = String::new();
        for url in &images {
            md.push_str(&format!("![alt]({url})\n\n"));
        }
        let r = render_body(&md, 80, &ctx);
        let lines_text = r
            .lines
            .iter()
            .map(|l| text(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            r.images,
            images[..MAX_IMAGES_PER_MESSAGE],
            "the first six are drawn"
        );
        assert!(
            lines_text.contains("2 more pictures not shown"),
            "the two beyond the cap are said, not dropped: {lines_text:?}"
        );
        // One beyond the cap reads as one.
        let seven: String = images[..7]
            .iter()
            .map(|u| format!("![alt]({u})\n\n"))
            .collect();
        let r = render_body(&seven, 80, &ctx);
        let text_of = |r: &Rendered| {
            r.lines
                .iter()
                .map(|l| text(l))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            text_of(&r).contains("1 more picture not shown"),
            "{:?}",
            text_of(&r)
        );
        // At the cap, nothing is said.
        let six: String = images[..6]
            .iter()
            .map(|u| format!("![alt]({u})\n\n"))
            .collect();
        assert!(!text_of(&render_body(&six, 80, &ctx)).contains("not shown"));
    }
}
