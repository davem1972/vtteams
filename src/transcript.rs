//! Lays a chat's messages out as one long list of styled lines (header, body, reactions, separators).

use crate::convert::message_markdown;
use crate::imgart::ImgMode;
use crate::render::{self, Ctx, LinkCells};
use crate::theme::*;
use chrono::{DateTime, Local, NaiveDate, Utc};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::HashMap;
use std::ops::Range;
use teams_client::{Delivery, Msg, html_to_plain, preview_text};
use unicode_width::UnicodeWidthStr;

/// Width of the gutter bar ("▎ ") in front of every message line.
pub const GUTTER: usize = 2;

#[derive(Default)]
pub struct Transcript {
    pub lines: Vec<Line<'static>>,
    /// First and last line (inclusive) of each message, by message index.
    pub starts: Vec<usize>,
    pub ends: Vec<usize>,
    /// Links of each message, by message index.
    pub links: Vec<Vec<String>>,
    /// Lines occupied by each fetchable image, with its URL (absolute line numbers): what to fetch when it nears the
    /// screen, and the click targets once it has arrived.
    pub image_rows: Vec<(Range<usize>, String)>,
    /// Lines of each `[n]` entry of a message's link list, with its full destination (absolute line numbers).
    pub link_rows: Vec<(Range<usize>, String)>,
    /// Where each link is drawn (absolute line, columns, destination), for marking it as a terminal hyperlink.
    pub link_cells: Vec<LinkCells>,
    /// What this was built from; rebuild when it changes.
    pub key: Option<TranscriptKey>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TranscriptKey {
    pub width: usize,
    pub version: u64,
    pub images: u64,
    pub mode: ImgMode,
    pub spinner: usize,
    /// The local date the labels (Today, Yesterday) were made on: a new day rebuilds even when no message changed.
    pub day: chrono::NaiveDate,
    /// The unread marker position: a change rebuilds to move the "new" divider.
    pub unread_marker: Option<chrono::DateTime<chrono::Utc>>,
    /// The chat's people the directory no longer lists, whose names are drawn dim and in italics: a change redraws.
    pub unlisted: std::collections::BTreeSet<String>,
}

pub struct CacheEntry {
    key: u64,
    width: usize,
    header: bool,
    mode: ImgMode,
    image_version: u64,
    has_images: bool,
    spinner: usize,
    lines: Vec<Line<'static>>,
    links: Vec<String>,
    images: Vec<String>,
    /// Relative to the first line of the message.
    image_rows: Vec<(Range<usize>, String)>,
    link_rows: Vec<(Range<usize>, String)>,
    link_cells: Vec<LinkCells>,
}

/// One rendered message: its lines plus what lives in them.
struct Laid {
    lines: Vec<Line<'static>>,
    links: Vec<String>,
    images: Vec<String>,
    image_rows: Vec<(Range<usize>, String)>,
    link_rows: Vec<(Range<usize>, String)>,
    link_cells: Vec<LinkCells>,
}

pub type Cache = HashMap<String, CacheEntry>;

/// What is known about the people of a chat beyond its messages: names by id (the roster, earlier senders), and who the
/// directory no longer lists.
pub struct People<'a> {
    pub names: &'a HashMap<String, String>,
    pub unlisted: &'a std::collections::BTreeSet<String>,
}

pub fn me_color() -> ratatui::style::Color {
    OK
}

fn sender_color(m: &Msg, me_id: &str) -> ratatui::style::Color {
    if m.from_id.as_deref() == Some(me_id) {
        me_color()
    } else {
        person_color(&m.from_name)
    }
}

fn local(t: DateTime<Utc>) -> DateTime<Local> {
    t.with_timezone(&Local)
}

pub fn day_label(d: NaiveDate) -> String {
    let today = Local::now().date_naive();
    match (today - d).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => d.format("%A").to_string(),
        _ => d.format("%A, %B %-d").to_string(),
    }
}

fn centered(
    text: &str,
    width: usize,
    fill: &str,
    style: Style,
    fill_style: Style,
) -> Line<'static> {
    let tw = text.width() + 2;
    let left = width.saturating_sub(tw) / 2;
    let right = width.saturating_sub(tw + left);
    Line::from(vec![
        Span::styled(fill.repeat(left), fill_style),
        Span::styled(format!(" {text} "), style),
        Span::styled(fill.repeat(right), fill_style),
    ])
}

fn gutter(color: ratatui::style::Color) -> Span<'static> {
    Span::styled("▎ ", Style::new().fg(color))
}

/// How much of the post a reply names is shown in its marker.
const REPLY_PREVIEW_COLUMNS: usize = 40;

/// `text` cut to `max` columns, with an ellipsis when something was left out.
fn clip(text: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if text.width() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// The line above a reply in a channel that says what it answers: the post's sender and the start of its title or text
/// when the post is among the messages loaded, else only that it is a reply. `None` for a message that is not a reply.
/// No request is made for a post that is not loaded.
fn reply_note(m: &Msg, posts: &HashMap<&str, &Msg>, me_id: &str) -> Option<String> {
    let root = m.root_id.as_deref()?;
    let Some(post) = posts.get(root).filter(|p| p.id != m.id) else {
        return Some("↳ reply in a thread".to_string());
    };
    let who = if post.from_id.as_deref() == Some(me_id) {
        "you".to_string()
    } else if post.from_name.is_empty() {
        "Unknown".to_string()
    } else {
        post.from_name.clone()
    };
    let said = post
        .subject
        .as_deref()
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if post.deleted {
                String::new()
            } else {
                preview_text(&post.body, post.html)
            }
        });
    let said = crate::inline_style::strip(&said);
    Some(if said.is_empty() {
        format!("↳ reply to {who}")
    } else {
        format!("↳ reply to {who}: {}", clip(&said, REPLY_PREVIEW_COLUMNS))
    })
}

#[cfg(test)]
thread_local! {
    /// How many messages have been laid out (not taken from the cache) on this thread, so a test can say a frame did none.
    static LAID_OUT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many messages this thread has laid out so far.
#[cfg(test)]
pub fn laid_out() -> usize {
    LAID_OUT.with(std::cell::Cell::get)
}

/// Most people named beside one reaction; the rest are counted in "+N".
const REACTORS_SHOWN: usize = 3;

/// Who reacted with `r`, as drawn after its chip: "You" first when the signed-in user is among them, then the others by
/// first name (the full name when two of them share a first name), at most [`REACTORS_SHOWN`], then "+N" for everyone
/// else (named but not shown, named by id only and not known yet, or counted by the service without being named).
/// Names come from `Reaction::names` (the backend's), else from `known` (the chat's people and the senders the app has
/// loaded). Empty when nobody can be named, since "+2" beside a chip that already says 2 tells nothing.
fn reactor_names(
    r: &teams_client::Reaction,
    me_id: &str,
    known: &HashMap<String, String>,
) -> String {
    let mut you = false;
    let mut named: Vec<(&str, &str)> = Vec::new();
    let mut unknown = 0;
    for (i, id) in r.users.iter().enumerate() {
        if id == me_id {
            you = true;
            continue;
        }
        let name = r
            .names
            .get(i)
            .map(String::as_str)
            .filter(|n| !n.trim().is_empty())
            .or_else(|| {
                known
                    .get(id)
                    .map(String::as_str)
                    .filter(|n| !n.trim().is_empty())
            });
        match name {
            Some(n) if !named.iter().any(|(other, _)| other == id) => named.push((id, n)),
            Some(_) => {}
            None => unknown += 1,
        }
    }
    let first = |n: &str| n.split_whitespace().next().unwrap_or(n).to_string();
    let mut shown: Vec<String> = Vec::new();
    if you {
        shown.push("You".to_string());
    }
    for (id, name) in &named {
        let f = first(name);
        let shared = named.iter().any(|(other, n)| other != id && first(n) == f);
        shown.push(if shared { (*name).to_string() } else { f });
    }
    if shown.is_empty() {
        return String::new();
    }
    let rest = shown.len().saturating_sub(REACTORS_SHOWN)
        + unknown
        + r.count.saturating_sub(r.users.len());
    shown.truncate(REACTORS_SHOWN);
    let list = shown.join(", ");
    if rest > 0 {
        format!("{list} +{rest}")
    } else {
        list
    }
}
fn render_message(
    m: &Msg,
    me_id: &str,
    width: usize,
    header: bool,
    reply: Option<&str>,
    ctx: &Ctx<'_>,
    people: &People<'_>,
) -> Laid {
    let known_names = people.names;
    let unlisted = m
        .from_id
        .as_ref()
        .is_some_and(|id| people.unlisted.contains(id));
    #[cfg(test)]
    LAID_OUT.with(|n| n.set(n.get() + 1));
    if m.system {
        let text = if m.html {
            html_to_plain(&m.body)
        } else {
            m.body.clone()
        };
        let text = if text.trim().is_empty() {
            "system event".to_string()
        } else {
            text
        };
        let line = centered(
            &text,
            width,
            " ",
            Style::new().fg(SUBTLE).add_modifier(Modifier::ITALIC),
            Style::new(),
        );
        return Laid {
            lines: vec![line],
            links: vec![],
            images: vec![],
            image_rows: vec![],
            link_rows: vec![],
            link_cells: vec![],
        };
    }
    let color = sender_color(m, me_id);
    let mut out: Vec<Line<'static>> = Vec::new();
    if header {
        let me = m.from_id.as_deref() == Some(me_id);
        let name = if me {
            "You".to_string()
        } else if m.from_name.is_empty() {
            "Unknown".into()
        } else {
            m.from_name.clone()
        };
        let mut spans = vec![
            gutter(color),
            Span::styled(
                name,
                if unlisted {
                    // Someone the directory no longer lists: dim and in italics, as in the chat list.
                    crate::ui::unlisted_style(Style::new(), DIM)
                } else {
                    Style::new().fg(color).add_modifier(Modifier::BOLD)
                },
            ),
        ];
        // Who is on the left; when it was sent, and when it was edited if it was, at the right edge (two spaces after
        // the name when the line is too narrow for that). A message that @mentions the signed-in user gets a
        // bright "@" badge immediately before the time, so it stands out in a chat list scan the way Teams' own
        // notification does.
        let mentioned = m.mentions_me(me_id);
        let badge = if mentioned { "@ " } else { "" };
        let sent = local(m.created).format("%H:%M").to_string();
        let edited = m
            .edited
            .filter(|_| !m.deleted)
            .map(|at| format!("(edited {})", local(at).format("%H:%M")));
        let right_width =
            badge.width() + sent.width() + edited.as_ref().map_or(0, |e| 2 + e.width());
        let used: usize = spans.iter().map(|s| s.content.width()).sum();
        // One column is left free at the edge, so the last digit does not touch the scrollbar.
        spans.push(Span::raw(
            " ".repeat(width.saturating_sub(used + right_width + 1).max(2)),
        ));
        if mentioned {
            spans.push(Span::styled(
                badge,
                Style::new().fg(MENTION_ME).add_modifier(Modifier::BOLD),
            ));
        }
        spans.push(Span::styled(sent, Style::new().fg(DIM)));
        if let Some(note) = edited {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                note,
                Style::new().fg(DIM).add_modifier(Modifier::ITALIC),
            ));
        }
        out.push(Line::from(spans));
    }
    let body_w = width.saturating_sub(GUTTER + 1);
    // A reply in a channel says which post it answers, above its text and whether or not it has a header.
    if let Some(note) = reply {
        out.push(Line::from(vec![
            gutter(color),
            Span::styled(
                clip(note, body_w),
                Style::new().fg(DIM).add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
    let md = message_markdown(m, me_id);
    let rendered = render::render_body(&md, body_w, ctx);
    let deleted = m.deleted;
    // Body lines follow the header (if any), one output line per rendered line.
    let body_start = out.len();
    let body = rendered.lines;
    for l in body {
        let mut spans = vec![gutter(color)];
        spans.extend(l.spans.into_iter().map(|mut s| {
            if deleted {
                s.style = s.style.fg(DIM).add_modifier(Modifier::ITALIC);
            }
            s
        }));
        out.push(Line::from(spans));
    }
    if !m.reactions.is_empty() {
        // Each reaction is a chip and the people beside it; rows break between reactions, never inside one, and a
        // reaction too wide for a row on its own has its names cut with "…". The last column stays free, as it does for
        // the text (the frame around a message being replied to runs down it).
        let body_w = width.saturating_sub(GUTTER + 1);
        let mut rows: Vec<(Vec<Span<'static>>, usize)> = vec![(Vec::new(), 0)];
        for r in &m.reactions {
            let chip = format!(" {} {} ", r.emoji, r.count);
            let chip_w = chip.width();
            let names = reactor_names(r, me_id, known_names);
            let names = if names.is_empty() {
                names
            } else {
                clip(&format!(" {names}"), body_w.saturating_sub(chip_w))
            };
            let piece_w = chip_w + names.width();
            if rows
                .last()
                .is_some_and(|(_, used)| *used > 0 && used + 2 + piece_w > body_w)
            {
                rows.push((Vec::new(), 0));
            }
            let (row, used) = rows.last_mut().expect("rows starts with one row");
            if *used > 0 {
                row.push(Span::raw("  "));
                *used += 2;
            }
            row.push(Span::styled(chip, Style::new().fg(TEXT).bg(CHIP_BG)));
            if !names.is_empty() {
                row.push(Span::styled(names, Style::new().fg(DIM)));
            }
            *used += piece_w;
        }
        for (row, _) in rows {
            let mut spans = vec![gutter(color)];
            spans.extend(row);
            out.push(Line::from(spans));
        }
    }
    match &m.delivery {
        Delivery::Sent => {}
        Delivery::Sending => {
            out.push(Line::from(vec![
                gutter(color),
                Span::styled(format!("{} sending…", ctx.spinner), Style::new().fg(WARN)),
            ]));
        }
        Delivery::Failed(e) => {
            let e: String = e.chars().take(body_w.saturating_sub(10)).collect();
            out.push(Line::from(vec![
                gutter(color),
                Span::styled(format!("✗ not sent: {e}"), Style::new().fg(ERR)),
            ]));
        }
    }
    let image_rows = rendered
        .image_rows
        .into_iter()
        .map(|(r, url)| (r.start + body_start..r.end + body_start, url))
        .collect();
    let link_rows = rendered
        .link_rows
        .into_iter()
        .map(|(r, url)| (r.start + body_start..r.end + body_start, url))
        .collect();
    // The body's lines are drawn after the gutter.
    let link_cells = rendered
        .link_cells
        .into_iter()
        .map(|(line, cols, url)| {
            (
                line + body_start,
                cols.start + GUTTER..cols.end + GUTTER,
                url,
            )
        })
        .collect();
    Laid {
        lines: out,
        links: rendered.links,
        images: rendered.images,
        image_rows,
        link_rows,
        link_cells,
    }
}

/// A fingerprint of everything about `m` that affects how it is drawn: body, times, flags, sender, subject, attachments,
/// each reaction with its count, users and names, and delivery state, and `reply`, the line saying which post it answers
/// (which depends on a message other than `m`: the post's sender and text). The per-message render cache is keyed by it,
/// so a change to any of those redraws the message. Equal messages have equal keys within one run; the value is never stored.
/// `reactor_display` is the formatted display text for each reaction's reactors, so a name arriving later changes the key.
fn message_key(m: &Msg, reply: Option<&str>, reactor_display: &[String], unlisted: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    reply.hash(&mut h);
    unlisted.hash(&mut h);
    m.root_id.hash(&mut h);
    m.id.hash(&mut h);
    m.created.timestamp_millis().hash(&mut h);
    m.modified.timestamp_millis().hash(&mut h);
    m.edited.map(|t| t.timestamp_millis()).hash(&mut h);
    m.from_id.hash(&mut h);
    m.from_name.hash(&mut h);
    (m.system, m.deleted, m.html).hash(&mut h);
    m.body.hash(&mut h);
    m.subject.hash(&mut h);
    for a in &m.attachments {
        (&a.name, &a.content_type, &a.url).hash(&mut h);
    }
    for (r, disp) in m.reactions.iter().zip(reactor_display.iter()) {
        (&r.emoji, r.count, &r.users, &r.names, disp).hash(&mut h);
    }
    match &m.delivery {
        Delivery::Sent => 0u8.hash(&mut h),
        Delivery::Sending => 1u8.hash(&mut h),
        Delivery::Failed(why) => (2u8, why).hash(&mut h),
    }
    h.finish()
}

/// Build the transcript. Rows of every fetchable image are recorded in `Transcript::image_rows`; deciding which of
/// them to download is the caller's job (it knows what is near the screen). `ctx` must describe the same image mode,
/// images and spinner frame as `key`, which is what decides whether cached entries are still good.
pub fn build(
    msgs: &[Msg],
    me_id: &str,
    unread_marker: Option<DateTime<Utc>>,
    key: TranscriptKey,
    ctx: &Ctx<'_>,
    cache: &mut Cache,
    known_names: &HashMap<String, String>,
) -> Transcript {
    let mut t = Transcript::default();
    let mut last_day: Option<NaiveDate> = None;
    let mut marker_shown = false;
    let mut prev: Option<&Msg> = None;
    // Where a reply finds the post it answers.
    let posts: HashMap<&str, &Msg> = msgs
        .iter()
        .filter(|m| m.root_id.is_none())
        .map(|m| (m.id.as_str(), m))
        .collect();

    for m in msgs {
        let day = local(m.created).date_naive();
        if last_day != Some(day) {
            if !t.lines.is_empty() {
                t.lines.push(Line::default());
            }
            t.lines.push(centered(
                &day_label(day),
                key.width,
                "─",
                Style::new().fg(SUBTLE).add_modifier(Modifier::BOLD),
                Style::new().fg(DIM),
            ));
            t.lines.push(Line::default());
            last_day = Some(day);
            prev = None;
        }
        if !marker_shown
            && unread_marker.is_some_and(|u| m.created > u)
            && m.from_id.as_deref() != Some(me_id)
            && !m.system
        {
            if prev.is_some() {
                t.lines.push(Line::default());
            }
            t.lines.push(centered(
                "new",
                key.width,
                "─",
                Style::new().fg(ERR).add_modifier(Modifier::BOLD),
                Style::new().fg(ERR),
            ));
            t.lines.push(Line::default());
            marker_shown = true;
            prev = None;
        } else if prev.is_some_and(|p| !(p.system && m.system)) {
            // Events in a row (joins, calls, settings) sit together instead of a blank line apart.
            t.lines.push(Line::default());
        }

        // An edited message, or one that @mentions the signed-in user, always has a header: that is where the
        // note, or the badge, says so.
        let header = (m.edited.is_some() && !m.deleted)
            || m.mentions_me(me_id)
            || prev.is_none_or(|p| {
                p.system || p.from_id != m.from_id || (m.created - p.created).num_minutes() >= 5
            });
        let note = reply_note(m, &posts, me_id);
        let reactor_display: Vec<String> = m
            .reactions
            .iter()
            .map(|r| reactor_names(r, me_id, known_names))
            .collect();
        let unlisted = m
            .from_id
            .as_ref()
            .is_some_and(|id| key.unlisted.contains(id));
        let ck = message_key(m, note.as_deref(), &reactor_display, unlisted);
        let reuse = cache.get(&m.id).filter(|e| {
            e.key == ck
                && e.width == key.width
                && e.header == header
                && e.mode == key.mode
                && (!e.has_images || e.image_version == key.images)
                && (e.spinner == key.spinner || !(e.has_images || m.delivery == Delivery::Sending))
        });
        let laid = match reuse {
            Some(e) => Laid {
                lines: e.lines.clone(),
                links: e.links.clone(),
                images: e.images.clone(),
                image_rows: e.image_rows.clone(),
                link_rows: e.link_rows.clone(),
                link_cells: e.link_cells.clone(),
            },
            None => {
                let laid = render_message(
                    m,
                    me_id,
                    key.width,
                    header,
                    note.as_deref(),
                    ctx,
                    &People {
                        names: known_names,
                        unlisted: &key.unlisted,
                    },
                );
                cache.insert(
                    m.id.clone(),
                    CacheEntry {
                        key: ck,
                        width: key.width,
                        header,
                        mode: key.mode,
                        image_version: key.images,
                        has_images: !laid.images.is_empty(),
                        spinner: key.spinner,
                        lines: laid.lines.clone(),
                        links: laid.links.clone(),
                        images: laid.images.clone(),
                        image_rows: laid.image_rows.clone(),
                        link_rows: laid.link_rows.clone(),
                        link_cells: laid.link_cells.clone(),
                    },
                );
                laid
            }
        };
        let first = t.lines.len();
        t.starts.push(first);
        t.lines.extend(laid.lines);
        t.ends.push(t.lines.len().saturating_sub(1));
        t.links.push(laid.links);
        t.image_rows.extend(
            laid.image_rows
                .into_iter()
                .map(|(r, url)| (r.start + first..r.end + first, url)),
        );
        t.link_rows.extend(
            laid.link_rows
                .into_iter()
                .map(|(r, url)| (r.start + first..r.end + first, url)),
        );
        t.link_cells.extend(
            laid.link_cells
                .into_iter()
                .map(|(line, cols, url)| (line + first, cols, url)),
        );
        prev = Some(m);
    }
    t.key = Some(key);
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::ImageStore;
    use chrono::Duration;
    use teams_client::{Delivery, Msg};

    fn msg(id: &str, who: &str, mins_ago: i64, html: &str) -> Msg {
        let t = Utc::now() - Duration::minutes(mins_ago);
        Msg {
            root_id: None,
            id: id.into(),
            created: t,
            modified: t,
            edited: None,
            from_id: Some(who.into()),
            from_name: who.into(),
            system: false,
            deleted: false,
            html: true,
            body: html.into(),
            subject: None,
            attachments: vec![],
            reactions: vec![],
            mentions: vec![],
            delivery: Delivery::Sent,
        }
    }

    fn key(width: usize) -> TranscriptKey {
        TranscriptKey {
            width,
            version: 1,
            images: 0,
            mode: ImgMode::DEFAULT,
            spinner: 0,
            day: chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            unread_marker: None,
            unlisted: Default::default(),
        }
    }

    fn ctx<'a>(images: &'a ImageStore, fetchable: &'a dyn Fn(&str) -> bool) -> Ctx<'a> {
        Ctx {
            images,
            mode: ImgMode::DEFAULT,
            spinner: "⠋",
            fetchable,
            keymap: crate::editor::Keymap::default(),
        }
    }

    fn text(l: &Line<'_>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn reaction(emoji: &str, count: usize, people: &[(&str, &str)]) -> teams_client::Reaction {
        teams_client::Reaction {
            emoji: emoji.into(),
            count,
            users: people.iter().map(|(id, _)| (*id).to_string()).collect(),
            names: people.iter().map(|(_, n)| (*n).to_string()).collect(),
        }
    }

    fn known(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(i, n)| ((*i).to_string(), (*n).to_string()))
            .collect()
    }

    /// The lines under one message with `reactions`, laid out `width` wide.
    fn reaction_lines(
        reactions: Vec<teams_client::Reaction>,
        width: usize,
        people: &HashMap<String, String>,
        cache: &mut Cache,
    ) -> Vec<Line<'static>> {
        let mut m = msg("1", "ada", 1, "<p>hello</p>");
        // One fixed time, so two builds of the same message differ only in what the test changes.
        let at = chrono::TimeZone::timestamp_opt(&Utc, 1_800_000_000, 0).unwrap();
        m.created = at;
        m.modified = at;
        m.reactions = reactions;
        let t = build(
            &[m],
            "me",
            None,
            key(width),
            &ctx(&ImageStore::default(), &|_| false),
            cache,
            people,
        );
        t.lines
            .into_iter()
            .filter(|l| l.spans.iter().any(|s| s.style.bg == Some(CHIP_BG)))
            .collect()
    }

    #[test]
    fn who_reacted_is_named_you_first_by_first_name_and_the_rest_counted() {
        let r = |people: &[(&str, &str)], count| {
            reactor_names(
                &reaction("👍", count, people),
                "me",
                &known(&[("g", "Grace Hopper")]),
            )
        };
        // You first, then the backend's name, then a name the app knows, and two the service counted but did not name.
        assert_eq!(
            r(&[("a", "Ada Lovelace"), ("me", ""), ("g", "")], 5),
            "You, Ada, Grace +2"
        );
        // Two people with one first name are told apart by their full names.
        assert_eq!(
            r(&[("a", "Ada Lovelace"), ("b", "Ada Byron")], 2),
            "Ada Lovelace, Ada Byron"
        );
        // At most three names; the others, and the people known by id only, are counted.
        assert_eq!(
            r(
                &[
                    ("a", "Ada L"),
                    ("b", "Bob M"),
                    ("c", "Cy N"),
                    ("d", "Di O"),
                    ("x", "")
                ],
                5
            ),
            "Ada, Bob, Cy +2"
        );
        // Nobody to name: nothing beside the chip (its count already says how many).
        assert_eq!(r(&[("x", ""), ("y", "")], 2), "");
        assert_eq!(r(&[], 3), "");
        // A person really called "You" is not mistaken for the signed-in user.
        assert_eq!(r(&[("y", "You Know Who")], 1), "You");
        assert_eq!(r(&[("y", "You Know Who"), ("me", "")], 2), "You, You");
    }

    #[test]
    fn the_names_are_drawn_dim_after_their_chip_and_rows_break_between_reactions_only() {
        let reactions = vec![
            reaction(
                "👍",
                3,
                &[("me", ""), ("a", "Ada Lovelace"), ("g", "Grace Hopper")],
            ),
            reaction("❤", 1, &[("l", "Linus Torvalds")]),
            reaction(
                "😂",
                5,
                &[
                    ("a", "Ada Lovelace"),
                    ("b", "Bob Martin"),
                    ("c", "Cy Young"),
                ],
            ),
        ];
        let wide = reaction_lines(reactions.clone(), 120, &HashMap::new(), &mut Cache::new());
        assert_eq!(wide.len(), 1, "one row when they fit");
        let row = text(&wide[0]);
        assert!(row.contains(" 👍 3  You, Ada, Grace"), "{row:?}");
        assert!(row.contains(" ❤ 1  Linus"), "{row:?}");
        assert!(row.contains(" 😂 5  Ada, Bob, Cy +2"), "{row:?}");
        let dim: Vec<String> = wide[0]
            .spans
            .iter()
            .filter(|s| s.style.fg == Some(DIM))
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(dim, [" You, Ada, Grace", " Linus", " Ada, Bob, Cy +2"]);
        assert!(wide[0].spans.iter().any(|s| s.content.contains("👍")
            && s.style.bg == Some(CHIP_BG)
            && s.style.fg == Some(TEXT)));

        let narrow = reaction_lines(reactions, 32, &HashMap::new(), &mut Cache::new());
        assert!(
            narrow.len() >= 2,
            "{:?}",
            narrow.iter().map(text).collect::<Vec<_>>()
        );
        for line in &narrow {
            assert!(
                line.width() <= 32,
                "{:?} is wider than the pane",
                text(line)
            );
        }
        // Every reaction's names stay on its chip's row, whole.
        let all: Vec<String> = narrow.iter().map(text).collect();
        for whole in [
            " 👍 3  You, Ada, Grace",
            " ❤ 1  Linus",
            " 😂 5  Ada, Bob, Cy +2",
        ] {
            assert!(
                all.iter().any(|l| l.contains(whole)),
                "{whole:?} not whole in {all:?}"
            );
        }
    }

    #[test]
    fn a_reaction_too_wide_for_a_row_has_its_names_cut_and_keeps_its_chip() {
        let long = reaction(
            "👍",
            2,
            &[
                ("a", "Bartholomew-Maximilian Featherstonehaugh"),
                ("b", "Constantinople Underwood"),
            ],
        );
        let lines = reaction_lines(vec![long], 24, &HashMap::new(), &mut Cache::new());
        assert_eq!(lines.len(), 1);
        assert!(lines[0].width() <= 24, "{:?}", text(&lines[0]));
        assert!(text(&lines[0]).contains(" 👍 2 "), "{:?}", text(&lines[0]));
        assert!(text(&lines[0]).ends_with('…'), "{:?}", text(&lines[0]));
    }

    #[test]
    fn a_name_that_arrives_later_redraws_the_message() {
        let mut cache = Cache::new();
        let r = || vec![reaction("👍", 1, &[("g", "")])];
        let before = reaction_lines(r(), 80, &HashMap::new(), &mut cache);
        assert!(!text(&before[0]).contains("Grace"));
        let after = reaction_lines(r(), 80, &known(&[("g", "Grace Hopper")]), &mut cache);
        assert!(
            text(&after[0]).contains("Grace"),
            "the cached message was not drawn again: {:?}",
            text(&after[0])
        );
    }
    #[test]
    fn groups_consecutive_messages_and_indexes_them() {
        let msgs = vec![
            msg("1", "ada", 3, "<p>one</p>"),
            msg("2", "ada", 2, "<p>two</p>"),
            msg("3", "bob", 1, "<p>three</p>"),
        ];
        let t = build(
            &msgs,
            "me",
            None,
            key(60),
            &ctx(&ImageStore::default(), &|_| false),
            &mut Cache::new(),
            &HashMap::new(),
        );
        assert_eq!(t.starts.len(), 3);
        let all: Vec<String> = t.lines.iter().map(text).collect();
        assert_eq!(
            all.iter().filter(|l| l.contains("ada")).count(),
            1,
            "same sender within 5 minutes shares one header: {all:?}"
        );
        assert_eq!(all.iter().filter(|l| l.contains("bob")).count(), 1);
        assert!(all.iter().any(|l| l.contains("Today")));
        assert!(t.starts[0] < t.starts[1] && t.ends[1] < t.starts[2]);
    }

    #[test]
    fn an_edited_message_says_so_on_its_header_even_in_the_middle_of_a_run_and_a_reaction_is_not_an_edit()
     {
        let mut msgs = vec![
            msg("1", "ada", 3, "<p>one</p>"),
            msg("2", "ada", 2, "<p>two</p>"),
            msg("3", "ada", 1, "<p>three</p>"),
        ];
        msgs[1].edited = Some(Utc::now());
        // A change that is not an edit (a reaction moves `modified`) gets no note and no header.
        msgs[2].modified = Utc::now();
        let t = build(
            &msgs,
            "me",
            None,
            key(60),
            &ctx(&ImageStore::default(), &|_| false),
            &mut Cache::new(),
            &HashMap::new(),
        );
        let all: Vec<String> = t.lines.iter().map(text).collect();
        let noted: Vec<_> = all.iter().filter(|l| l.contains("(edited")).collect();
        assert_eq!(noted.len(), 1, "{all:?}");
        // The note is on a header line, with the sender at the left and the time sent and the note at the right edge,
        // one column short of it.
        assert!(
            noted[0].starts_with("▎ ada") && noted[0].ends_with(")") && noted[0].width() == 59,
            "{noted:?}"
        );
        let sent_at = noted[0].find('(').unwrap();
        assert!(
            noted[0][..sent_at]
                .trim_end()
                .ends_with(|c: char| c.is_ascii_digit()),
            "{noted:?}"
        );
        // Every header has its time at the right edge, edited or not.
        let plain: Vec<_> = all
            .iter()
            .filter(|l| l.contains("ada") && !l.contains("(edited"))
            .collect();
        assert!(
            plain
                .iter()
                .all(|l| l.width() == 59 && l.ends_with(|c: char| c.is_ascii_digit())),
            "{plain:?}"
        );
        // Too narrow for that, the time and the note follow the name instead of being cut off.
        let narrow = build(
            &msgs,
            "me",
            None,
            key(22),
            &ctx(&ImageStore::default(), &|_| false),
            &mut Cache::new(),
            &HashMap::new(),
        );
        let line = narrow
            .lines
            .iter()
            .map(text)
            .find(|l| l.contains("(edited"))
            .unwrap();
        assert!(line.starts_with("▎ ada  "), "{line:?}");
        // The run is split only there: a header for the first message and one for the edited one.
        assert_eq!(
            all.iter().filter(|l| l.contains("ada")).count(),
            2,
            "{all:?}"
        );
        let header_at = all.iter().position(|l| l.contains("(edited")).unwrap();
        assert_eq!(
            header_at, t.starts[1],
            "the header is the first line of its message"
        );
        // A deleted message is not called edited.
        msgs[1].deleted = true;
        let t = build(
            &msgs,
            "me",
            None,
            key(60),
            &ctx(&ImageStore::default(), &|_| false),
            &mut Cache::new(),
            &HashMap::new(),
        );
        assert!(!t.lines.iter().map(text).any(|l| l.contains("(edited")));
    }

    #[test]
    fn a_message_mentioning_me_gets_an_at_badge_on_its_own_header() {
        let mut msgs = vec![
            msg("1", "ada", 2, "<p>one</p>"),
            msg("2", "ada", 1, "<p>two</p>"),
        ];
        msgs[1].mentions = vec!["me".to_string()];
        let t = build(
            &msgs,
            "me",
            None,
            key(60),
            &ctx(&ImageStore::default(), &|_| false),
            &mut Cache::new(),
            &HashMap::new(),
        );
        let all: Vec<String> = t.lines.iter().map(text).collect();
        let headers: Vec<_> = all.iter().filter(|l| l.contains("ada")).collect();
        // Mentioning forces its own header, the same way an edit does, so the badge always has a place.
        assert_eq!(headers.len(), 2, "{all:?}");
        assert!(!headers[0].contains('@'), "{headers:?}");
        assert!(headers[1].contains("@ "), "{headers:?}");
        let badge = t
            .lines
            .iter()
            .find_map(|l| l.spans.iter().find(|s| s.content.as_ref() == "@ "))
            .expect("the badge is its own span");
        assert_eq!(badge.style.fg, Some(crate::theme::MENTION_ME));
        // A message mentioning someone else gets no badge.
        let mut not_me = msgs.clone();
        not_me[1].mentions = vec!["grace".to_string()];
        let t2 = build(
            &not_me,
            "me",
            None,
            key(60),
            &ctx(&ImageStore::default(), &|_| false),
            &mut Cache::new(),
            &HashMap::new(),
        );
        assert!(
            !t2.lines.iter().map(text).any(|l| l.contains('@')),
            "{:?}",
            t2.lines.iter().map(text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn unread_marker_is_inserted_before_first_new_message() {
        let msgs = vec![
            msg("1", "ada", 30, "<p>old</p>"),
            msg("2", "ada", 10, "<p>new one</p>"),
        ];
        let marker = Some(Utc::now() - Duration::minutes(20));
        let t = build(
            &msgs,
            "me",
            marker,
            key(60),
            &ctx(&ImageStore::default(), &|_| false),
            &mut Cache::new(),
            &HashMap::new(),
        );
        let all: Vec<String> = t.lines.iter().map(text).collect();
        let new_at = all
            .iter()
            .position(|l| l.contains(" new "))
            .expect("marker");
        assert!(new_at < t.starts[1] && new_at > t.ends[0]);
    }

    #[test]
    fn images_the_backend_serves_are_recorded_for_fetching_but_external_ones_are_not() {
        let msgs = vec![msg(
            "1",
            "ada",
            1,
            r#"<img src="https://graph.microsoft.com/v1.0/chats/19:a/messages/1/hostedContents/x/$value" alt="a"><img src="https://tracker.example/p.png" alt="b">"#,
        )];
        let serves = |u: &str| u.starts_with("https://graph.microsoft.com/");
        let t = build(
            &msgs,
            "me",
            None,
            key(60),
            &ctx(&ImageStore::default(), &serves),
            &mut Cache::new(),
            &HashMap::new(),
        );
        let urls: Vec<&str> = t.image_rows.iter().map(|(_, u)| u.as_str()).collect();
        assert_eq!(
            urls,
            ["https://graph.microsoft.com/v1.0/chats/19:a/messages/1/hostedContents/x/$value"]
        );
        assert_eq!(
            t.links[0],
            vec!["https://tracker.example/p.png".to_string()]
        );
    }

    #[test]
    fn cached_entries_are_reused_until_the_message_changes() {
        let msgs = vec![msg("1", "ada", 1, "<p>hello</p>")];
        let mut cache = Cache::new();
        let imgs = ImageStore::default();
        build(
            &msgs,
            "me",
            None,
            key(60),
            &ctx(&imgs, &|_| false),
            &mut cache,
            &HashMap::new(),
        );
        assert_eq!(cache.len(), 1);
        let mut edited = msgs.clone();
        edited[0].body = "<p>hello world, now longer</p>".into();
        let t = build(
            &edited,
            "me",
            None,
            key(60),
            &ctx(&imgs, &|_| false),
            &mut cache,
            &HashMap::new(),
        );
        assert!(t.lines.iter().any(|l| text(l).contains("now longer")));
    }

    #[test]
    fn the_cache_key_changes_with_everything_that_is_drawn() {
        use teams_client::Reaction;
        let base = msg("1", "ada", 1, "<p>hello</p>");
        let key = message_key(&base, None, &[], false);
        assert_eq!(
            key,
            message_key(&base.clone(), None, &[], false),
            "equal messages have equal keys"
        );
        let mut m = base.clone();
        m.reactions = vec![Reaction {
            emoji: "👍".into(),
            count: 1,
            users: vec![],
            names: vec![],
        }];
        let one = message_key(&m, None, &[String::new()], false);
        assert_ne!(one, key, "a first reaction");
        m.reactions[0].count = 2;
        assert_ne!(
            message_key(&m, None, &[String::new()], false),
            one,
            "the same reaction from a second person"
        );
        for change in [
            |m: &mut Msg| m.body.push('!'),
            |m: &mut Msg| m.deleted = true,
            |m: &mut Msg| m.subject = Some("s".into()),
            |m: &mut Msg| m.root_id = Some("post".into()),
            |m: &mut Msg| m.delivery = Delivery::Sending,
            |m: &mut Msg| m.delivery = Delivery::Failed("x".into()),
            |m: &mut Msg| m.from_name = "someone else".into(),
        ] {
            let mut changed = base.clone();
            change(&mut changed);
            assert_ne!(message_key(&changed, None, &[], false), key);
        }
        // What the reply line says depends on another message (the post): it is part of the key.
        assert_ne!(
            message_key(&base, Some("↳ reply to Ada: hi"), &[], false),
            key
        );
        assert_ne!(
            message_key(&base, Some("↳ reply to Ada: hi"), &[], false),
            message_key(&base, Some("↳ reply to Ada: hello"), &[], false)
        );
    }

    fn reply(id: &str, who: &str, mins_ago: i64, post: &str, html: &str) -> Msg {
        let mut m = msg(id, who, mins_ago, html);
        m.root_id = Some(post.into());
        m
    }

    #[test]
    fn a_reply_names_its_post_above_its_text_and_a_post_keeps_its_title() {
        let mut post = msg("p1", "ada", 9, "<p>We ship on Friday if the tests pass</p>");
        post.subject = Some("Release plan".into());
        let msgs = vec![
            post,
            reply("r1", "grace", 8, "p1", "<p>Tests pass here</p>"),
            // The post is not among the loaded messages.
            reply("r2", "grace", 7, "gone", "<p>and on the other branch</p>"),
        ];
        let t = build(
            &msgs,
            "me",
            None,
            key(80),
            &ctx(&ImageStore::default(), &|_| false),
            &mut Cache::new(),
            &HashMap::new(),
        );
        let all: Vec<String> = t.lines.iter().map(text).collect();
        // The title is a bold line directly under the post's header.
        let head = all.iter().position(|l| l.contains("ada")).unwrap();
        assert!(all[head + 1].contains("Release plan"), "{all:?}");
        let title_style = t.lines[head + 1]
            .spans
            .iter()
            .find(|s| s.content.contains("Release plan"))
            .unwrap()
            .style;
        assert!(title_style.add_modifier.contains(Modifier::BOLD));
        // The reply names the post by its sender and title; the note comes before the reply's text.
        let note = all
            .iter()
            .position(|l| l.contains("↳ reply to ada: Release plan"))
            .unwrap_or_else(|| panic!("{all:?}"));
        let said = all
            .iter()
            .position(|l| l.contains("Tests pass here"))
            .unwrap();
        assert!(note < said && note > t.starts[1] - 1, "{all:?}");
        // One whose post is not loaded still says it is a reply, and nothing is fetched to find out more.
        assert!(
            all.iter().any(|l| l.contains("↳ reply in a thread")),
            "{all:?}"
        );
        // A post that is not a reply has no such line.
        assert_eq!(all.iter().filter(|l| l.contains('↳')).count(), 2, "{all:?}");
    }

    #[test]
    fn two_replies_in_a_row_from_one_sender_each_keep_their_marker_and_a_post_without_a_title_is_quoted_by_its_text()
     {
        let msgs = vec![
            msg("p1", "ada", 9, "<p>Anyone seen the build?</p>"),
            reply("r1", "grace", 8, "p1", "<p>one</p>"),
            reply("r2", "grace", 7, "p1", "<p>two</p>"),
        ];
        let t = build(
            &msgs,
            "me",
            None,
            key(80),
            &ctx(&ImageStore::default(), &|_| false),
            &mut Cache::new(),
            &HashMap::new(),
        );
        let all: Vec<String> = t.lines.iter().map(text).collect();
        assert_eq!(
            all.iter()
                .filter(|l| l.contains("↳ reply to ada: Anyone seen the build?"))
                .count(),
            2,
            "{all:?}"
        );
        // Grouped under one header as always, but never without the marker.
        assert_eq!(all.iter().filter(|l| l.contains("grace")).count(), 1);
    }

    #[test]
    fn a_reply_is_redrawn_when_the_post_it_names_changes() {
        let mut cache = Cache::new();
        let imgs = ImageStore::default();
        let mut msgs = vec![
            msg("p1", "ada", 9, "<p>first words</p>"),
            reply("r1", "grace", 8, "p1", "<p>reply</p>"),
        ];
        let draw = |msgs: &[Msg], cache: &mut Cache| -> String {
            build(
                msgs,
                "me",
                None,
                key(80),
                &ctx(&imgs, &|_| false),
                cache,
                &HashMap::new(),
            )
            .lines
            .iter()
            .map(text)
            .collect::<Vec<_>>()
            .join("\n")
        };
        assert!(draw(&msgs, &mut cache).contains("reply to ada: first words"));
        // Only the post changes; the reply's own fields are the same, but what its marker says is not.
        msgs[0].body = "<p>other words</p>".into();
        let again = draw(&msgs, &mut cache);
        assert!(again.contains("reply to ada: other words"), "{again}");
        assert!(!again.contains("first words"));
    }
}
