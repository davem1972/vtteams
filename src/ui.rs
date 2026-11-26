//! All drawing.

use crate::app::{App, EMOJI_COLUMNS, EMOJI_ROWS, EmojiTarget, Focus, Overlay, SignIn};
use crate::editor::{Keymap, Mode};
use crate::emoji;
use crate::highlight;
use crate::theme::*;
use crate::transcript::me_color;
use crate::view::{self, EditorRows, Geometry, Tab};
use chrono::{DateTime, Local, Utc};
use ratatui::Frame;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use teams_client::{Chat, ChatKind, Contact, same_emoji};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// One frame: bring the view state up to date, then draw it.
pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    prepare(app, frame.area());
    draw(frame, app);
}

/// Lay the screen out for `area` and bring everything drawing depends on up to date: the rectangles (which mouse
/// clicks are matched against), the wrapped transcript, the chat-list and editor scroll positions and the editor's
/// rows. This is the only place drawing state changes; [`draw`] only reads.
pub fn prepare(app: &mut App, area: Rect) {
    let chats_focused = app.focus == Focus::Chats;
    // What the banner has to say, from the clock as it is now: a note whose time is up leaves at the next frame.
    let now = Utc::now();
    let notes = app.banner_entries(now);
    // Only worth working out while there is an `@` to colour (a channel's people are found from its messages).
    let people = if app.editor.lines().iter().any(|l| l.contains('@')) {
        MentionPeople::new(app.mention_candidates().into_owned(), app.me_id())
    } else {
        MentionPeople::default()
    };
    let (editor, view) = (&app.editor, &mut app.view);
    let geometry = view::layout(
        area,
        chats_focused,
        |width, most| {
            view.banner = crate::banner::lines(&notes, width, most, now, &Local);
            view.banner.len()
        },
        |width| {
            refresh_editor_rows(
                editor,
                &mut view.editor_cells,
                &mut view.editor_cache,
                &mut view.editor_rows,
                width,
                &people,
            );
            view.editor_rows.lines.len()
        },
    );
    // `layout` may have given the banner fewer rows than it wanted (a short terminal), or none, or not asked at all
    // (one pane shown): what is kept is what has room.
    view.banner.truncate(geometry.banner.height as usize);
    view.geometry = geometry.clone();
    view.screen = area;

    if geometry.list_body != Rect::default() {
        let vis = app.visible_chats();
        let pinned = app.pinned_count(&vis);
        let height = geometry.list_body.height as usize;
        let mut offset = app.view.list_offset;
        // Scrolled with the wheel: it stays there until the selection moves.
        let follow = app.view.list_wheeled.as_ref() != Some(&app.current);
        if follow {
            app.view.list_wheeled = None;
        }
        // An open channel is not in this list: then the list stays where it is.
        if let Some(selected) = selected_position(app, &vis).filter(|_| follow) {
            if selected < offset {
                offset = selected;
            }
            // The divider takes a row, so how many chats fit depends on where the list starts.
            while selected >= offset + view::list_capacity(height, offset, pinned) {
                offset += 1;
            }
        }
        // Not so far down that the list ends short of the bottom.
        while offset > 0 && offset + view::list_capacity(height, offset - 1, pinned) > vis.len() {
            offset -= 1;
        }
        app.view.list_offset = offset.min(vis.len().saturating_sub(1));
        if app.on_teams_tab() {
            let rows = app.team_rows();
            let at = app.team_cursor_index(&rows);
            let pinned = crate::app::favorite_count(&rows);
            let follow = app.view.team_wheeled != Some(at);
            if follow {
                app.view.team_wheeled = None;
            }
            // Scrolled with the wheel, the list only has to stay within its rows; otherwise the cursor is kept on screen.
            let keep = if follow { at } else { app.view.team_offset };
            app.view.team_offset =
                view::teams_scroll(app.view.team_offset, keep, height, rows.len(), pinned);
        }
    }
    if geometry.conversation != Rect::default() {
        app.layout_transcript(
            geometry.transcript.width as usize,
            geometry.transcript.height as usize,
        );
        // Sticky scroll: the editor window only moves when the cursor leaves it.
        let rows = geometry.editor.height.saturating_sub(2) as usize;
        if rows > 0 {
            let view = &mut app.view;
            let mut top = view
                .editor_top
                .min(view.editor_rows.lines.len().saturating_sub(1));
            let cursor = view.editor_rows.cursor.0;
            if cursor < top {
                top = cursor;
            } else if cursor >= top + rows {
                top = cursor + 1 - rows;
            }
            view.editor_top = top;
        }
    }
    // After the transcript is laid out, and also when it is not on screen (which clears the plan).
    app.plan_graphics(&geometry);
    let links = plan_links(app, &geometry);
    app.view.links_before = std::mem::replace(&mut app.view.links, links);
}

/// The links of the open conversation to mark as terminal hyperlinks this frame (see `hyperlink.rs`): none when the
/// `hyperlinks` setting is off or anything is drawn over the transcript (a popup, an overlay, the mention list), only
/// those on screen with an address `hyperlink::target` accepts, and never under the labels over its first and last
/// rows.
fn plan_links(app: &App, g: &Geometry) -> Vec<crate::hyperlink::Spot> {
    let area = g.transcript;
    if !app.hyperlinks
        || app.overlay != Overlay::None
        || app.mention_suggestions().is_some()
        || area == Rect::default()
    {
        return Vec::new();
    }
    let Some(st) = app.state().filter(|st| st.loaded) else {
        return Vec::new();
    };
    let labels = hint_labels_of(app, g);
    let mut out = Vec::new();
    for (line, cols, url) in &st.transcript.link_cells {
        let Some(row) = line
            .checked_sub(st.scroll)
            .filter(|r| *r < usize::from(area.height))
        else {
            continue;
        };
        let Some(url) = crate::hyperlink::target(url) else {
            continue;
        };
        let y = area.y + row as u16;
        let left = usize::from(area.x);
        let mut pieces = vec![(
            left + cols.start,
            (left + cols.end).min(usize::from(area.right())),
        )];
        for label in labels.iter().filter(|l| l.rect.y == y) {
            let (from, to) = (usize::from(label.rect.x), usize::from(label.rect.right()));
            pieces = pieces
                .into_iter()
                .flat_map(|(a, b)| [(a, b.min(from)), (a.max(to), b)])
                .filter(|(a, b)| a < b)
                .collect();
        }
        for (a, b) in pieces {
            out.push(crate::hyperlink::Spot {
                x: a as u16,
                y,
                width: (b - a) as u16,
                url: url.clone(),
            });
        }
    }
    out
}

/// Where the open chat is among `visible` (0 when it is not shown); `None` when what is open is a channel, which is not
/// in this list at all.
fn selected_position(app: &App, visible: &[usize]) -> Option<usize> {
    if app.current_channel().is_some() {
        return None;
    }
    Some(
        visible
            .iter()
            .position(|&i| Some(&app.chats.items()[i].id) == app.current.as_ref())
            .unwrap_or(0),
    )
}

/// Draw the frame laid out by [`prepare`].
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let g = &app.view.geometry;
    frame.render_widget(Block::new().style(Style::new().bg(BG).fg(TEXT)), area);
    draw_header(frame, app, g.header);
    if g.list != Rect::default() {
        draw_chat_list(frame, app, g);
    }
    if g.conversation != Rect::default() {
        draw_conversation(frame, app, g);
    }
    draw_footer(frame, app, g);
    draw_mention_suggestions(frame, app, g.editor);

    match app.overlay.clone() {
        Overlay::Help(scroll) => draw_help(frame, area, app, scroll),
        Overlay::Links(links) => draw_links(frame, &links, area),
        Overlay::People { people, sel } => draw_people(frame, app, &people, sel, area),
        Overlay::Status { sel } => draw_status(frame, app, sel, area),
        Overlay::Emoji { target, query, sel } => draw_emoji(frame, app, &target, &query, sel, area),
        Overlay::SignIn { .. } => draw_sign_in(frame, app, area),
        Overlay::Search => draw_search(frame, app, area),
        Overlay::None => {}
    }
    // The pictures drawn with the terminal's graphics go over cells ratatui must leave alone while they are up; the
    // presenter writes them after this frame is flushed (`App::present_graphics`).
    let labels = hint_labels_of(app, g);
    for placement in app.gfx_plan() {
        for position in placement.rect.positions() {
            let Some(cell) = frame.buffer_mut().cell_mut(position) else {
                continue;
            };
            if placement.image.protocol == crate::gfx::Protocol::Kitty {
                // A Kitty picture is placed under the text, so the cells over it hold nothing but what is meant to show on
                // top of it: the labels, and blanks everywhere else (not the block art, which would cover the picture).
                if !labels.iter().any(|l| l.rect.contains(position)) {
                    cell.reset();
                    cell.set_bg(BG);
                }
            } else {
                // The other protocols write pixels into the cells; the frame leaves them alone, and what is to show on top
                // of the picture (the labels) is written again after it (`Placement::overlay`).
                cell.set_diff_option(CellDiffOption::Skip);
            }
        }
    }
    // Last, so each link's cell holds the text as drawn, with the picture cells already left alone.
    crate::hyperlink::mark(frame.buffer_mut(), &app.view.links, &app.view.links_before);
}

/// The people offered for the `@name` being typed, in a small list on top of the conversation just above the message box.
fn draw_mention_suggestions(frame: &mut Frame<'_>, app: &App, editor: Rect) {
    if editor == Rect::default() {
        return;
    }
    let Some(s) = app.mention_suggestions() else {
        return;
    };
    let widest = s.people.iter().map(|p| p.name.width()).max().unwrap_or(0);
    let width = (widest as u16 + 6)
        .clamp(16, editor.width.max(16))
        .min(editor.width);
    let height = s.people.len() as u16 + 2;
    let area = Rect {
        x: editor.x,
        y: editor.y.saturating_sub(height),
        width,
        height: height.min(editor.y + editor.height),
    };
    frame.render_widget(Clear, area);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(gradient(0.6)))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled("@ mention", Style::new().fg(gradient(0.3)).bold()),
            Span::raw(" "),
        ]))
        .style(Style::new().bg(PANEL_BG));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let room = inner.width.saturating_sub(3) as usize;
    let lines: Vec<Line<'_>> = s
        .people
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let (bg, mark) = if i == s.sel {
                (POPUP_SEL_BG, "▶")
            } else {
                (PANEL_BG, " ")
            };
            Line::from(vec![
                Span::styled(format!(" {mark} "), Style::new().fg(gradient(0.6)).bg(bg)),
                Span::styled(
                    truncate(&p.name, room),
                    Style::new().fg(person_color(&p.name)).bg(bg).bold(),
                ),
            ])
            .style(Style::new().bg(bg))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let [row0, row1] = Layout::vertical([Constraint::Length(1); 2]).areas(area);
    let mut title = vec![
        Span::raw(" "),
        Span::styled("⌨ ", Style::new().fg(gradient(0.9)).bold()),
    ];
    title.extend(gradient_spans(
        "v t t e a m s",
        app.header_phase,
        Style::new().bold(),
    ));
    title.push(Span::raw("  "));
    title.push(Span::styled(
        env!("VTTEAMS_BUILD_VERSION"),
        Style::new().fg(DIM),
    ));
    frame.render_widget(Paragraph::new(Line::from(title)), row0);

    let unread = app
        .chats
        .items()
        .iter()
        .filter(|c| app.is_unread(c))
        .count();
    let mut right: Vec<Span<'_>> = Vec::new();
    if app.is_busy() {
        right.push(Span::styled(
            format!("{} ", SPINNER[(app.tick / 2) % SPINNER.len()]),
            // The colour moves with the glyph, not between its steps: one change a step, not one a tick.
            Style::new()
                .fg(gradient(((app.tick / 2) % 20) as f32 / 20.0))
                .bold(),
        ));
    }
    let mentioned = app.outstanding_mentions();
    if mentioned > 0 {
        right.push(pill(&format!("@ {mentioned}"), MENTION_ME));
        right.push(Span::raw("  "));
    }
    if unread > 0 {
        right.push(pill(&format!("● {unread} unread"), ERR));
        right.push(Span::raw("  "));
    }
    right.push(Span::styled(app.backend.label(), Style::new().fg(DIM)));
    if let Some(me) = &app.me {
        right.push(Span::styled("  ·  ", Style::new().fg(DIM)));
        // How others see you (your own status), in front of your name.
        if let Some(mark) = app
            .own_presence()
            .and_then(|p| crate::presence::mark(app.presence_style(), p))
        {
            right.push(Span::styled(mark.glyph, Style::new().fg(mark.color)));
            right.push(Span::raw(" "));
        }
        right.push(Span::styled(
            me.name.clone(),
            Style::new().fg(me_color()).bold(),
        ));
    }
    right.push(Span::raw(" "));
    frame.render_widget(Paragraph::new(Line::from(right).right_aligned()), row0);

    let w = area.width as usize;
    let rule: Vec<Span<'_>> = (0..w)
        .map(|i| Span::styled("━", Style::new().fg(gradient(i as f32 / w.max(1) as f32))))
        .collect();
    frame.render_widget(Paragraph::new(Line::from(rule)), row1);
}

// ---------------------------------------------------------------------------
// Chat list
// ---------------------------------------------------------------------------

fn short_time(t: DateTime<Utc>) -> String {
    let t = t.with_timezone(&Local);
    let days = (Local::now().date_naive() - t.date_naive()).num_days();
    match days {
        ..=0 => t.format("%H:%M").to_string(),
        1 => "Yest".into(),
        2..=6 => t.format("%a").to_string(),
        _ => t.format("%b %-d").to_string(),
    }
}

/// How the name of someone the directory no longer lists (`Member::not_in_directory`) is drawn: dim and in italics, on
/// top of `base`, with `muted` as its colour.
pub fn unlisted_style(base: Style, muted: ratatui::style::Color) -> Style {
    base.fg(muted)
        .add_modifier(Modifier::ITALIC)
        .remove_modifier(Modifier::BOLD)
}

/// A chat's title as spans no wider than `max` (cut with "…" as [`truncate`] does), the names of people the directory no
/// longer lists in [`unlisted_style`] and the rest in `base`.
fn title_spans(
    chat: &Chat,
    me_id: &str,
    max: usize,
    base: Style,
    muted: ratatui::style::Color,
) -> Vec<Span<'static>> {
    let parts = chat.title_parts(me_id);
    let whole: String = parts.iter().map(|(t, _)| t.as_str()).collect();
    let mut room = truncate(&whole, max);
    let mut spans = Vec::new();
    for (text, unlisted) in parts {
        if room.is_empty() {
            break;
        }
        // The cut title is the whole one up to some point, then "…": each part takes its share of what is left.
        let take = if room.starts_with(text.as_str()) {
            text.len()
        } else {
            room.len()
        };
        let piece: String = room.drain(..take).collect();
        let style = if unlisted {
            unlisted_style(base, muted)
        } else {
            base
        };
        spans.push(Span::styled(piece, style));
    }
    spans
}

fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

fn initials(name: &str) -> String {
    let words: Vec<&str> = name
        .split_whitespace()
        .filter(|w| w.chars().next().is_some_and(char::is_alphanumeric))
        .collect();
    match words.len() {
        0 => "?".into(),
        1 => words[0].chars().take(2).collect::<String>().to_uppercase(),
        _ => format!(
            "{}{}",
            words[0].chars().next().unwrap_or('?'),
            words[words.len() - 1].chars().next().unwrap_or('?')
        )
        .to_uppercase(),
    }
}

fn avatar(chat: &Chat, title: &str) -> Vec<Span<'static>> {
    let color = person_color(title);
    let text = match chat.kind {
        ChatKind::OneOnOne => initials(title),
        ChatKind::Meeting => "◷ ".to_string(),
        // Groups, and any kind of conversation a newer client library starts reporting.
        _ => "◈◈".to_string(),
    };
    let style = match chat.kind {
        ChatKind::OneOnOne => Style::new().fg(BG).bg(color).bold(),
        _ => Style::new().fg(color).bg(CHIP_BG).bold(),
    };
    vec![Span::styled(format!("{text:^4}"), style)]
}

/// What introduces a chat list preview: "You: " for your own message, and in a group the first name of whoever wrote
/// it. Nothing for a 1:1 chat (the title names the person), for a cached list (it has no preview text to introduce)
/// or when the sender is not known: a bare ": " in front of nothing says nothing.
fn preview_prefix(
    preview: Option<&teams_client::Preview>,
    kind: ChatKind,
    me_id: &str,
    cached: bool,
) -> String {
    match preview {
        Some(_) if cached => String::new(),
        Some(p) if p.from_id.as_deref() == Some(me_id) => "You: ".to_string(),
        Some(p) if kind != ChatKind::OneOnOne => p
            .from_name
            .as_deref()
            .and_then(|n| n.split_whitespace().next())
            .map(|first| format!("{first}: "))
            .unwrap_or_default(),
        _ => String::new(),
    }
}
fn draw_chat_list(frame: &mut Frame<'_>, app: &App, g: &Geometry) {
    let focused = app.focus == Focus::Chats;
    let (head, list) = (g.list_head, g.list_body);
    let vis = app.visible_chats();

    let labels = app.tab_labels(usize::from(head.width));
    let mut h = vec![Span::raw(" ")];
    if app.filtering || !app.filter.is_empty() {
        h.push(Span::styled("/", Style::new().fg(WARN).bold()));
        h.push(Span::styled(app.filter.clone(), Style::new().fg(TEXT)));
        if app.filtering {
            h.push(Span::styled("▏", Style::new().fg(WARN)));
        }
    } else if !labels.is_empty() {
        // Two tabs: each a padded label (see `view::tab_cells`, which is also what a click is matched against), the one
        // showing highlighted.
        h.clear();
        for (i, label) in labels.iter().enumerate() {
            if i > 0 {
                h.push(Span::raw(" "));
            }
            let active = label.tab == app.tab;
            let style = match (active, focused) {
                (true, true) => Style::new().fg(BG).bg(gradient(0.45)).bold(),
                (true, false) => Style::new().fg(TEXT).bg(CHIP_BG).bold(),
                (false, _) => Style::new().fg(SUBTLE),
            };
            h.push(Span::styled(format!(" {}", label.main), style));
            if !label.unread.is_empty() {
                h.push(Span::styled(
                    label.unread.clone(),
                    if active { style } else { Style::new().fg(ERR) },
                ));
            }
            h.push(Span::styled(" ", style));
        }
        if app.unread_only && app.tab == Tab::Chats {
            h.push(Span::raw(" "));
            h.push(pill("unread", WARN));
        }
    } else {
        h.push(Span::styled(
            "CHATS",
            Style::new()
                .fg(if focused { gradient(0.3) } else { SUBTLE })
                .bold(),
        ));
        h.push(Span::styled(
            format!("  {}", vis.len()),
            Style::new().fg(DIM),
        ));
        if app.unread_only {
            h.push(Span::raw(" "));
            h.push(pill("unread", WARN));
        }
    }
    // What the list shows is what the last session left, until the service has answered: say so, if there is room.
    let cached = if app.on_teams_tab() {
        app.teams.is_cached()
    } else {
        app.chats.is_cached()
    };
    if cached && !app.filtering && app.filter.is_empty() {
        let used: usize = h.iter().map(|s| s.content.width()).sum();
        if used + 8 <= usize::from(head.width) {
            h.push(Span::styled(
                " cached",
                Style::new().fg(DIM).add_modifier(Modifier::ITALIC),
            ));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(h)), head);

    if app.on_teams_tab() {
        draw_teams_list(frame, app, list, focused);
        return;
    }
    let w = list.width as usize;
    let me_id = app.me_id().to_string();

    if !app.chats.loaded() {
        let msg = match app.chats.error() {
            Some(e) => Line::from(Span::styled(format!(" {e}"), Style::new().fg(ERR))),
            None => Line::from(Span::styled(
                format!(
                    " {} loading chats…",
                    SPINNER[(app.tick / 2) % SPINNER.len()]
                ),
                Style::new().fg(SUBTLE),
            )),
        };
        frame.render_widget(
            Paragraph::new(msg).wrap(ratatui::widgets::Wrap { trim: true }),
            list,
        );
        return;
    }
    if vis.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " no chats match",
                Style::new().fg(DIM),
            ))),
            list,
        );
        return;
    }

    let pinned = app.pinned_count(&vis);
    let offset = app.view.list_offset;
    let cap = view::list_capacity(list.height as usize, offset, pinned);
    let sel_pos = selected_position(app, &vis);

    let mut lines: Vec<Line<'_>> = Vec::new();
    for (pos, &ci) in vis.iter().enumerate().skip(offset).take(cap) {
        // A thin rule under the last favourite, when the others follow it on screen.
        if pinned > 0 && pos == pinned && pos > offset {
            lines.push(Line::from(Span::styled(
                "─".repeat(w),
                Style::new().fg(CHIP_BG),
            )));
        }
        let chat = &app.chats.items()[ci];
        let selected = Some(pos) == sel_pos;
        let unread = app.is_unread(chat);
        let title = chat.title(&me_id);
        let bg = if selected { SEL_BG } else { BG };
        // Quiet text is dimmer than the highlight can carry, so it is lifted on the selected row.
        let muted = if selected { SUBTLE } else { DIM };
        let time = chat
            .preview
            .as_ref()
            .map(|p| short_time(p.created))
            .unwrap_or_default();

        let bar = if selected {
            Span::styled(
                "▌",
                Style::new()
                    .fg(if focused { gradient(0.5) } else { DIM })
                    .bg(bg),
            )
        } else {
            Span::styled(" ", Style::new().bg(bg))
        };
        let dot = if unread {
            Span::styled("●", Style::new().fg(ERR).bg(bg))
        } else {
            Span::styled(" ", Style::new().bg(bg))
        };
        // Columns the availability mark takes in front of every name (none when it is off or cannot be had).
        let mark_cols = app.presence_columns();
        let title_w = w.saturating_sub(1 + 4 + 1 + mark_cols + 1 + time.width() + 2);
        let title_style = Style::new()
            .fg(if unread { WHITE } else { TEXT })
            .bg(bg)
            .add_modifier(if unread {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });

        let mut l1 = vec![bar.clone()];
        l1.extend(avatar(chat, &title).into_iter().map(|mut s| {
            s.style = s.style.bg(if matches!(chat.kind, ChatKind::OneOnOne) {
                person_color(&title)
            } else if selected {
                // The chip would otherwise melt into the highlighted row.
                DIM
            } else {
                CHIP_BG
            });
            s
        }));
        l1.push(Span::styled(" ", Style::new().bg(bg)));
        if mark_cols > 0 {
            // How available a 1:1 chat's other person is, in front of the name; a blank of the same width for the
            // chats without one, so the names stay in line.
            l1.push(match presence_mark(app, chat) {
                Some(m) => Span::styled(m.glyph, Style::new().fg(m.color).bg(bg)),
                None => Span::styled(
                    " ".repeat(app.presence_style().width()),
                    Style::new().bg(bg),
                ),
            });
            l1.push(Span::styled(" ", Style::new().bg(bg)));
        }
        l1.extend(title_spans(chat, &me_id, title_w, title_style, muted));
        let used: usize = l1.iter().map(|s| s.content.width()).sum();
        let gap = w.saturating_sub(used + time.width() + 2);
        l1.push(Span::styled(" ".repeat(gap), Style::new().bg(bg)));
        l1.push(Span::styled(
            time,
            Style::new().fg(if unread { ERR } else { muted }).bg(bg),
        ));
        l1.push(dot);

        let prefix = preview_prefix(
            chat.preview.as_ref(),
            chat.kind,
            me_id.as_str(),
            app.chats.is_cached(),
        );
        let text = chat.preview.as_ref().map(|p| p.text.as_str()).unwrap_or("");
        let prev_w = w.saturating_sub(1 + 4 + 1 + mark_cols + 1);
        let preview = truncate(&format!("{prefix}{text}"), prev_w);
        // A favourite has a star under its avatar; the preview lines up under the name.
        let star = if chat.favorite.is_some() {
            Span::styled(" ★  ", Style::new().fg(WARN).bg(bg))
        } else {
            Span::styled("    ", Style::new().bg(bg))
        };
        let mut l2 = vec![bar, star, Span::styled(" ", Style::new().bg(bg))];
        l2.push(Span::styled(" ".repeat(mark_cols), Style::new().bg(bg)));
        l2.push(Span::styled(
            preview.clone(),
            Style::new().fg(if unread { SUBTLE } else { muted }).bg(bg),
        ));
        let used2: usize = l2.iter().map(|s| s.content.width()).sum();
        l2.push(Span::styled(
            " ".repeat(w.saturating_sub(used2)),
            Style::new().bg(bg),
        ));
        lines.push(Line::from(l1));
        lines.push(Line::from(l2));
    }
    frame.render_widget(Paragraph::new(lines), list);

    if vis.len() > cap {
        let mut state = ScrollbarState::new(vis.len() - cap).position(offset);
        let sb = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_symbol("▐")
            .thumb_style(Style::new().fg(DIM));
        frame.render_stateful_widget(sb, list, &mut state);
    }
}

const WHITE: ratatui::style::Color = ratatui::style::Color::Rgb(255, 255, 255);

/// One row of the teams list: what is in front of the name, the name (cut to what room is left), what follows it, and
/// what is at the right edge, on the row's background.
fn teams_row_line(
    bg: ratatui::style::Color,
    front: Vec<Span<'static>>,
    name: (String, Style),
    after: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Line<'static> {
    let fixed = |spans: &[Span<'_>]| spans.iter().map(|s| s.content.width()).sum::<usize>();
    let room = width
        .saturating_sub(fixed(&front) + fixed(&after) + fixed(&right) + 1)
        .max(1);
    let mut spans = front;
    spans.push(Span::styled(truncate(&name.0, room), name.1.bg(bg)));
    spans.extend(after);
    let used = fixed(&spans) + fixed(&right);
    spans.push(Span::styled(
        " ".repeat(width.saturating_sub(used)),
        Style::new().bg(bg),
    ));
    spans.extend(right);
    Line::from(spans)
}

/// The teams list: a team per row, under an open one its channels (see `app::Teams::rows`), one row each, so the row at
/// a screen row is `view::teams_row` for drawing, scrolling and clicking alike.
fn draw_teams_list(frame: &mut Frame<'_>, app: &App, list: Rect, focused: bool) {
    let teams = &app.teams;
    if !teams.loaded() {
        let line = match teams.error() {
            Some(e) => Line::from(Span::styled(format!(" {e}"), Style::new().fg(ERR))),
            None => Line::from(Span::styled(
                format!(
                    " {} loading teams…",
                    SPINNER[(app.tick / 2) % SPINNER.len()]
                ),
                Style::new().fg(SUBTLE),
            )),
        };
        frame.render_widget(
            Paragraph::new(line).wrap(ratatui::widgets::Wrap { trim: true }),
            list,
        );
        return;
    }
    let rows = app.team_rows();
    if rows.is_empty() {
        let text = if app.filter.is_empty() {
            " you are not in any team"
        } else {
            " no teams or channels match"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, Style::new().fg(DIM)))),
            list,
        );
        return;
    }
    let w = list.width as usize;
    let pinned = crate::app::favorite_count(&rows);
    let offset = app.view.team_offset;
    let cap = view::teams_capacity(list.height as usize, offset, pinned);
    let cursor = app.team_cursor_index(&rows);
    let open = app.current_channel().map(|o| o.id.clone());
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (pos, row) in rows.iter().enumerate().skip(offset).take(cap) {
        // The same thin rule the chat list draws under its last favourite, when the teams follow it on screen.
        if pinned > 0 && pos == pinned && pos > offset {
            lines.push(Line::from(Span::styled(
                "─".repeat(w),
                Style::new().fg(CHIP_BG),
            )));
        }
        lines.push(team_line(
            app,
            row,
            pos == cursor,
            open.as_deref(),
            focused,
            w,
        ));
    }
    frame.render_widget(Paragraph::new(lines), list);
    if rows.len() > cap {
        let mut state = ScrollbarState::new(rows.len() - cap).position(offset);
        let sb = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_symbol("▐")
            .thumb_style(Style::new().fg(DIM));
        frame.render_stateful_widget(sb, list, &mut state);
    }
}

/// One row of the teams list as drawn. The cursor row has the highlight; the open channel has a bar and its own colour
/// even when the cursor is elsewhere.
fn team_line(
    app: &App,
    row: &crate::app::TeamRow,
    cursor: bool,
    open: Option<&str>,
    focused: bool,
    width: usize,
) -> Line<'static> {
    use crate::app::TeamRow;
    let items = app.teams.items();
    let bg = if cursor { SEL_BG } else { BG };
    let bar = |is_open: bool| {
        if cursor {
            Span::styled(
                "▌",
                Style::new()
                    .fg(if focused { gradient(0.5) } else { DIM })
                    .bg(bg),
            )
        } else if is_open {
            Span::styled("▎", Style::new().fg(ACCENT).bg(bg))
        } else {
            Span::styled(" ", Style::new().bg(bg))
        }
    };
    // Quiet text is lifted on the highlighted row, as in the chat list.
    let dim = if cursor { SUBTLE } else { DIM };
    let bullet = |n: usize, with_count: bool| -> Vec<Span<'static>> {
        if n == 0 {
            return Vec::new();
        }
        let text = if with_count {
            format!("●{n}")
        } else {
            "●".to_string()
        };
        vec![Span::styled(text, Style::new().fg(ERR).bg(bg))]
    };
    match *row {
        TeamRow::Favorite { team, channel } => {
            // `Team › Channel`, flush left: not part of the tree below, so it has no fold arrow to line up with.
            let t = &items[team];
            let c = &t.channels[channel];
            let is_open = open == Some(c.id.as_str());
            let unread = app.channel_unread(c);
            let style = if is_open {
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else if c.archived || c.muted {
                Style::new().fg(dim)
            } else if unread {
                Style::new().fg(WHITE).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(TEXT)
            };
            teams_row_line(
                bg,
                vec![bar(is_open), Span::styled(" ", Style::new().bg(bg))],
                (crate::app::favorite_label(t, c), style),
                Vec::new(),
                bullet(usize::from(unread), false),
                width,
            )
        }
        TeamRow::Team {
            team,
            open: unfolded,
        } => {
            let t = &items[team];
            let unread = app.team_unread(t);
            let style = if t.archived || t.muted {
                Style::new().fg(dim)
            } else if unread > 0 {
                Style::new().fg(WHITE).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(TEXT).add_modifier(Modifier::BOLD)
            };
            let mut after = Vec::new();
            if t.muted {
                after.push(Span::styled(" muted", Style::new().fg(dim).bg(bg)));
            }
            teams_row_line(
                bg,
                vec![
                    bar(false),
                    Span::styled(
                        if unfolded { "▾ " } else { "▸ " },
                        Style::new().fg(dim).bg(bg),
                    ),
                ],
                (t.name.clone(), style),
                after,
                bullet(unread, true),
                width,
            )
        }
        TeamRow::Channel { team, channel } => {
            let t = &items[team];
            let c = &t.channels[channel];
            let is_open = open == Some(c.id.as_str());
            let unread = app.channel_unread(c);
            let style = if is_open {
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else if c.archived || t.archived || c.muted {
                Style::new().fg(dim)
            } else if unread {
                Style::new().fg(WHITE).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(TEXT)
            };
            let mut after = Vec::new();
            // Words, not a lock: a glyph that some fonts draw one column wide and others two would put the pane's edge
            // out of line.
            match c.kind {
                teams_client::ChannelKind::Private => {
                    after.push(Span::styled(" private", Style::new().fg(dim).bg(bg)))
                }
                teams_client::ChannelKind::Shared => {
                    after.push(Span::styled(" shared", Style::new().fg(dim).bg(bg)))
                }
                _ => {}
            }
            teams_row_line(
                bg,
                vec![bar(is_open), Span::styled("    ", Style::new().bg(bg))],
                (c.name.clone(), style),
                after,
                bullet(usize::from(unread), false),
                width,
            )
        }
        TeamRow::More {
            hidden,
            open: shown,
            ..
        } => teams_row_line(
            bg,
            vec![bar(false), Span::styled("    ", Style::new().bg(bg))],
            (
                if shown {
                    format!("−{hidden} more channels")
                } else {
                    format!("+{hidden} more channels")
                },
                Style::new().fg(dim).add_modifier(Modifier::ITALIC),
            ),
            Vec::new(),
            Vec::new(),
            width,
        ),
    }
}

// ---------------------------------------------------------------------------
// Conversation
// ---------------------------------------------------------------------------

fn draw_conversation(frame: &mut Frame<'_>, app: &App, g: &Geometry) {
    let border = Style::new().fg(DIM);
    let area = g.conversation_body;
    let rule: Vec<Line<'_>> = (0..g.divider.height)
        .map(|_| Line::from(Span::styled("│", border)))
        .collect();
    frame.render_widget(Paragraph::new(rule), g.divider);

    let Some(chat) = app.current_chat().cloned() else {
        // A channel is opened like a chat but has a title of its own and no people to show.
        if let Some(channel) = app.current_channel() {
            draw_channel_title(frame, app, channel, g.title);
            draw_transcript(frame, app, g);
            draw_editor(frame, app, g.editor);
            draw_reply_connector(frame, app, g);
            return;
        }
        let msg = if app.chats.loaded() {
            "Select a chat"
        } else {
            ""
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, Style::new().fg(DIM))).centered()),
            Rect {
                y: area.y + area.height / 2,
                height: 1,
                ..area
            },
        );
        return;
    };

    draw_chat_title(frame, app, &chat, g.title);
    draw_transcript(frame, app, g);
    draw_banner(frame, app, g.banner);
    draw_editor(frame, app, g.editor);
    draw_reply_connector(frame, app, g);
}

/// The status messages and out-of-office notices of the people in the open chat, above the message box. The rows were
/// made by [`prepare`] (the same ones the rectangle's height was decided from); this only draws them.
fn draw_banner(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if area == Rect::default() {
        return;
    }
    frame.render_widget(Paragraph::new(app.view.banner.clone()), area);
}

/// The mark for how available the other person of a 1:1 `chat` is, if it is known and shown.
fn presence_mark(app: &App, chat: &Chat) -> Option<crate::presence::Mark> {
    crate::presence::mark(app.presence_style(), app.presence_of(chat)?)
}

/// The line of people under a group or meeting chat's title: as many of `names` (everyone but you) as fit in `width`
/// columns, then how many more there are and how many people the chat has (`total`, you included).
fn attendee_line(names: &[&str], total: usize, width: usize) -> String {
    let count = format!("{total} people");
    for shown in (1..=names.len()).rev() {
        let rest = names.len() - shown;
        let more = if rest > 0 {
            format!(" +{rest} more")
        } else {
            String::new()
        };
        let line = format!(" {}{more} · {count}", names[..shown].join(", "));
        if line.width() <= width {
            return line;
        }
    }
    truncate(&format!(" {count}"), width)
}

fn draw_chat_title(frame: &mut Frame<'_>, app: &App, chat: &Chat, area: Rect) {
    let me_id = app.me_id();
    let title = chat.title(me_id);
    let (kind, color) = match chat.kind {
        ChatKind::OneOnOne => ("direct", person_color(&title)),
        ChatKind::Meeting => ("meeting", WARN),
        _ => ("group", ACCENT),
    };
    let mut l0 = vec![Span::raw(" ")];
    l0.extend(title_spans(
        chat,
        me_id,
        usize::MAX,
        Style::new().fg(color).bold(),
        DIM,
    ));
    l0.extend([Span::raw("  "), pill(kind, color)]);
    // Said in words in a 1:1 chat, whose title is just that name.
    if chat.kind == ChatKind::OneOnOne
        && chat
            .members
            .iter()
            .any(|m| m.id != me_id && m.not_in_directory)
    {
        l0.push(Span::styled(
            "  no longer in the directory",
            unlisted_style(Style::new(), DIM),
        ));
    }
    // How available the other person is, for a 1:1 chat: the mark and then what it means in words.
    if let Some(presence) = app.presence_of(chat)
        && let Some(mark) = crate::presence::mark(app.presence_style(), presence)
    {
        l0.push(Span::raw("  "));
        l0.push(Span::styled(mark.glyph, Style::new().fg(mark.color)));
        l0.push(Span::styled(
            format!(" {}", crate::presence::describe(presence)),
            Style::new().fg(DIM),
        ));
    }
    if let Some(st) = app.state()
        && st.shows_loading()
    {
        l0.push(Span::styled(
            format!("  {} loading", SPINNER[(app.tick / 2) % SPINNER.len()]),
            Style::new().fg(SUBTLE),
        ));
    }
    let people = app.attendees(chat);
    let names: Vec<&str> = people
        .iter()
        .filter(|m| m.id != me_id && !m.name.is_empty())
        .map(|m| m.name.as_str())
        .collect();
    let sub = if chat.kind == ChatKind::OneOnOne || people.is_empty() {
        String::new()
    } else {
        attendee_line(&names, people.len(), area.width as usize)
    };
    let l1 = Line::from(Span::styled(sub, Style::new().fg(DIM)));
    frame.render_widget(Paragraph::new(vec![Line::from(l0), l1]), area);
}

/// The title of an open channel: `Team › Channel`, what kind it is, and how many people the team has when that is known.
/// There is no presence and no roster for a channel.
fn draw_channel_title(frame: &mut Frame<'_>, app: &App, channel: &crate::app::Opened, area: Rect) {
    use teams_client::ChannelKind;
    let mut l0 = vec![
        Span::raw(" "),
        Span::styled(channel.team.clone(), Style::new().fg(SUBTLE)),
        Span::styled(" › ", Style::new().fg(DIM)),
        Span::styled(channel.name.clone(), Style::new().fg(ACCENT).bold()),
        Span::raw("  "),
        pill("channel", ACCENT),
    ];
    match channel.kind {
        ChannelKind::Private => {
            l0.push(Span::raw(" "));
            l0.push(pill("private", WARN));
        }
        ChannelKind::Shared => {
            l0.push(Span::raw(" "));
            l0.push(pill("shared", WARN));
        }
        _ => {}
    }
    if channel.archived {
        l0.push(Span::raw(" "));
        l0.push(pill("archived", DIM));
    }
    if let Some(st) = app.state()
        && st.shows_loading()
    {
        l0.push(Span::styled(
            format!("  {} loading", SPINNER[(app.tick / 2) % SPINNER.len()]),
            Style::new().fg(SUBTLE),
        ));
    }
    let sub = channel
        .members
        .map(|n| {
            format!(
                " {n} {} in the team",
                if n == 1 { "member" } else { "members" }
            )
        })
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(l0),
            Line::from(Span::styled(sub, Style::new().fg(DIM))),
        ]),
        area,
    );
}

/// Whether the transcript draws its two hints over its own rows: the one about older messages on the first row, and the
/// "latest" label on the last.
pub fn transcript_hints(st: &crate::app::ChatState, height: usize) -> (bool, bool) {
    let max_scroll = st.transcript.lines.len().saturating_sub(height);
    (
        st.older.is_some() && st.scroll == 0,
        !st.follow && st.scroll < max_scroll,
    )
}

/// A label the transcript draws over its own rows.
#[derive(Clone, Debug)]
pub struct HintLabel {
    /// The cells its text takes.
    pub rect: Rect,
    pub text: &'static str,
    pub style: Style,
}

/// The labels drawn over the first and last rows of the transcript in `text_area` (the one about older messages, then the
/// "latest" one). The drawing and the pictures drawn with graphics both ask, so that what a picture lets show on top of
/// it is exactly what is drawn.
pub fn transcript_hint_labels(st: &crate::app::ChatState, text_area: Rect) -> Vec<HintLabel> {
    let (older, latest) = transcript_hints(st, usize::from(text_area.height));
    let mut out = Vec::new();
    if latest {
        let text = " ↓ G latest ";
        let width = text.width() as u16;
        out.push(HintLabel {
            rect: Rect {
                x: text_area.x + text_area.width.saturating_sub(width + 1),
                y: text_area.y + text_area.height.saturating_sub(1),
                width,
                height: 1,
            },
            text,
            style: Style::new().fg(BG).bg(gradient(0.6)).bold(),
        });
    }
    if older {
        let text = " ↑ gg loads older messages ";
        out.push(HintLabel {
            rect: Rect {
                width: (text.width() as u16).min(text_area.width),
                height: 1,
                ..text_area
            },
            text,
            style: Style::new().fg(SUBTLE),
        });
    }
    out
}

/// The labels over the open chat's transcript, as drawn: none when it shows no messages.
pub fn hint_labels_of(app: &App, g: &Geometry) -> Vec<HintLabel> {
    match app.state() {
        Some(st) if st.loaded && !st.messages().is_empty() && g.transcript != Rect::default() => {
            transcript_hint_labels(st, g.transcript)
        }
        _ => Vec::new(),
    }
}

fn draw_transcript(frame: &mut Frame<'_>, app: &App, g: &Geometry) {
    let (text_area, bar_area) = (g.transcript, g.scrollbar);
    let focused = app.focus == Focus::Messages;
    let spinner = SPINNER[(app.tick / 2) % SPINNER.len()];
    let Some(st) = app.state() else { return };

    if !st.loaded {
        let line = match &st.error {
            Some(e) => Line::from(Span::styled(format!(" {e}"), Style::new().fg(ERR))),
            None => Line::from(Span::styled(
                format!(" {spinner} loading messages…"),
                Style::new().fg(SUBTLE),
            )),
        };
        frame.render_widget(
            Paragraph::new(line).wrap(ratatui::widgets::Wrap { trim: true }),
            text_area,
        );
        return;
    }
    if st.messages().is_empty() {
        frame.render_widget(
            Paragraph::new(
                Line::from(Span::styled(
                    if app.current_channel().is_some() {
                        "No posts in this channel yet."
                    } else {
                        "No messages yet. Press i to say hello."
                    },
                    Style::new().fg(DIM),
                ))
                .centered(),
            ),
            Rect {
                y: text_area.y + text_area.height / 2,
                height: 1,
                ..text_area
            },
        );
        return;
    }

    let t = &st.transcript;
    let h = text_area.height as usize;
    let sel_range = (t.starts.get(st.sel).copied(), t.ends.get(st.sel).copied());
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(h);
    for (i, l) in t.lines.iter().enumerate().skip(st.scroll).take(h) {
        let in_sel = focused && matches!(sel_range, (Some(s), Some(e)) if i >= s && i <= e);
        lines.push(if in_sel {
            tint(l.clone(), MSG_SEL_BG, text_area.width as usize)
        } else {
            l.clone()
        });
    }
    frame.render_widget(Paragraph::new(lines), text_area);

    let max_scroll = t.lines.len().saturating_sub(h);
    if max_scroll > 0 {
        let mut state = ScrollbarState::new(max_scroll).position(st.scroll);
        let sb = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .track_style(Style::new().fg(CHIP_BG))
            .thumb_symbol("█")
            .thumb_style(Style::new().fg(gradient(0.6)));
        frame.render_stateful_widget(sb, bar_area, &mut state);
    }
    for label in transcript_hint_labels(st, text_area) {
        frame.render_widget(
            Paragraph::new(Span::styled(label.text, label.style)),
            label.rect,
        );
    }
}
// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

type Cell = (char, Style);

fn mode_style(mode: Mode) -> (&'static str, ratatui::style::Color) {
    match mode {
        Mode::Normal => ("NORMAL", LINK),
        Mode::Insert => ("INSERT", OK),
        Mode::Visual => ("VISUAL", ACCENT),
        Mode::VisualLine => ("V-LINE", ACCENT),
    }
}

#[cfg(test)]
thread_local! {
    /// How many places `Closer::find` has compared a pattern at, on this thread (so a test can say the work is linear).
    static POSITIONS_LOOKED_AT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Finds where a closing pattern starts, at or after a given place in a line of characters. It remembers the last
/// answer, so an opener that has no closer, or several that reach the same one, cost one scan between them rather than
/// one each: the answer for a later start is the remembered one while that is still ahead, and "nothing" stays nothing.
struct Closer<'a> {
    chars: &'a [char],
    pat: Vec<char>,
    /// The place the last scan began and what it found.
    last: Option<(usize, Option<usize>)>,
}

impl<'a> Closer<'a> {
    fn new(chars: &'a [char], pat: &str) -> Self {
        Self {
            chars,
            pat: pat.chars().collect(),
            last: None,
        }
    }

    /// The first place at or after `from` where the pattern starts.
    fn find(&mut self, from: usize) -> Option<usize> {
        if let Some((began, found)) = self.last
            && from >= began
            && found.is_none_or(|at| at >= from)
        {
            return found;
        }
        let size = self.pat.len();
        let found = (from..self.chars.len().saturating_sub(size - 1)).find(|&at| {
            #[cfg(test)]
            POSITIONS_LOOKED_AT.with(|n| n.set(n.get() + 1));
            self.chars[at..at + size] == self.pat[..]
        });
        self.last = Some((from, found));
        found
    }
}

/// Inline Markdown colouring for one prose line of the editor.
fn md_cells(line: &str) -> Vec<Cell> {
    let chars: Vec<char> = line.chars().collect();
    let base = Style::new().fg(TEXT);
    let mut cells: Vec<Cell> = chars.iter().map(|&c| (c, base)).collect();
    let trimmed = line.trim_start();
    let indent = chars.len() - trimmed.chars().count();

    let paint = |cells: &mut Vec<Cell>, from: usize, to: usize, f: &dyn Fn(Style) -> Style| {
        for c in cells.iter_mut().take(to.min(chars.len())).skip(from) {
            c.1 = f(c.1);
        }
    };

    if trimmed.starts_with('#') && trimmed.trim_start_matches('#').starts_with(' ') {
        paint(&mut cells, 0, chars.len(), &|s| {
            s.fg(LINK).add_modifier(Modifier::BOLD)
        });
        return cells;
    }
    if trimmed.starts_with('>') {
        paint(&mut cells, 0, chars.len(), &|s| {
            s.fg(SUBTLE).add_modifier(Modifier::ITALIC)
        });
        paint(&mut cells, indent, indent + 1, &|s| s.fg(CODE_LABEL));
        return cells;
    }
    for marker in ["- ", "* ", "+ "] {
        if trimmed.starts_with(marker) {
            paint(&mut cells, indent, indent + 1, &|s| {
                s.fg(WARN).add_modifier(Modifier::BOLD)
            });
        }
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && trimmed[digits..].starts_with(". ") {
        paint(&mut cells, indent, indent + digits + 1, &|s| {
            s.fg(WARN).add_modifier(Modifier::BOLD)
        });
    }

    // Inline spans, scanned left to right. A search for a closer that found nothing is not repeated for each opener
    // after it: every pattern has a `Closer` that remembers its last answer.
    let (mut tick, mut bold, mut tilde) = (
        Closer::new(&chars, "`"),
        Closer::new(&chars, "**"),
        Closer::new(&chars, "~~"),
    );
    let (mut star, mut under) = (Closer::new(&chars, "*"), Closer::new(&chars, "_"));
    let (mut bracket, mut paren, mut colon) = (
        Closer::new(&chars, "]("),
        Closer::new(&chars, ")"),
        Closer::new(&chars, ":"),
    );
    let mut i = 0;
    while i < chars.len() {
        let next = chars.get(i + 1).copied();
        let pair = match (chars[i], next) {
            ('*', Some('*')) => Some(&mut bold),
            ('~', Some('~')) => Some(&mut tilde),
            _ => None,
        };
        let single = match chars[i] {
            '*' => Some(&mut star),
            '_' => Some(&mut under),
            _ => None,
        };
        if chars[i] == '`'
            && let Some(j) = tick.find(i + 1)
        {
            paint(&mut cells, i, j + 1, &|s| s.fg(WARN).bg(CODE_BG));
            i = j + 1;
        } else if let Some(closer) = pair
            && let Some(j) = closer.find(i + 2)
        {
            let strike = chars[i] == '~';
            paint(&mut cells, i, j + 2, &|s| {
                if strike {
                    s.fg(DIM).add_modifier(Modifier::CROSSED_OUT)
                } else {
                    s.fg(WHITE).add_modifier(Modifier::BOLD)
                }
            });
            i = j + 2;
        } else if let Some(closer) = single
            && next.is_some_and(|c| !c.is_whitespace())
            && (i == 0 || !chars[i - 1].is_alphanumeric())
            && let Some(j) = closer.find(i + 1)
        {
            paint(&mut cells, i, j + 1, &|s| {
                s.fg(ACCENT).add_modifier(Modifier::ITALIC)
            });
            i = j + 1;
        } else if chars[i] == '['
            && let Some(j) = bracket.find(i + 1)
            && let Some(k) = paren.find(j + 2)
        {
            paint(&mut cells, i, j + 1, &|s| {
                s.fg(LINK).add_modifier(Modifier::UNDERLINED)
            });
            paint(&mut cells, j + 1, k + 1, &|s| s.fg(DIM));
            i = k + 1;
        } else if chars[i] == '@' && (i == 0 || !chars[i - 1].is_alphanumeric()) {
            let end = (i + 1..chars.len())
                .find(|&j| !(chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '.'))
                .unwrap_or(chars.len());
            if end > i + 1 {
                paint(&mut cells, i, end, &|s| {
                    s.fg(CODE_LABEL).add_modifier(Modifier::BOLD)
                });
            }
            i = end.max(i + 1);
        } else if chars[i] == ':'
            && let Some(j) = colon.find(i + 1)
            && j > i + 1
            && j - i < 24
            && chars[i + 1..j]
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-'))
        {
            paint(&mut cells, i, j + 1, &|s| s.fg(WARN));
            i = j + 1;
        } else {
            i += 1;
        }
    }
    cells
}

/// The people of the open chat as the message box colours mentions of them, and a fingerprint of who they are (0 for
/// nobody), which the editor's caches are keyed on so a roster that arrives later colours the text again.
#[derive(Debug, Default)]
pub struct MentionPeople {
    members: Vec<teams_client::Member>,
    me: String,
    key: u64,
}

impl MentionPeople {
    pub fn new(members: Vec<teams_client::Member>, me: &str) -> Self {
        use std::hash::{Hash, Hasher};
        if members.is_empty() {
            return Self::default();
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        me.hash(&mut h);
        for m in &members {
            (&m.id, &m.name).hash(&mut h);
        }
        Self {
            members,
            me: me.to_string(),
            key: h.finish().max(1),
        }
    }

    /// For each line of `lines`, the character ranges of the `@Name`s sending would make mentions
    /// (`convert::mention_ranges`).
    fn ranges(&self, lines: &[String]) -> Vec<Vec<std::ops::Range<usize>>> {
        let mut out = vec![Vec::new(); lines.len()];
        if self.members.is_empty() || !lines.iter().any(|l| l.contains('@')) {
            return out;
        }
        let text = lines.join("\n");
        let mut starts = Vec::with_capacity(lines.len());
        let mut at = 0;
        for l in lines {
            starts.push(at);
            at += l.len() + 1;
        }
        for r in crate::convert::mention_ranges(&text, &self.members, &self.me) {
            let row = starts.partition_point(|&s| s <= r.start) - 1;
            let line = &lines[row];
            let (from, to) = (r.start - starts[row], (r.end - starts[row]).min(line.len()));
            out[row].push(line[..from].chars().count()..line[..to].chars().count());
        }
        out
    }
}

/// Highlighted cells for every buffer line, with fenced code coloured by the real highlighters, and the mentions in
/// `mentions` (character ranges for each line) drawn whole, as they will be sent.
fn editor_cells(lines: &[String], mentions: &[Vec<std::ops::Range<usize>>]) -> Vec<Vec<Cell>> {
    let mut out: Vec<Vec<Cell>> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let l = &lines[i];
        if let Some((open, info)) = crate::fence::fence_marker(l) {
            let lang = info.to_string();
            let fence_style = Style::new().fg(CODE_BORDER);
            out.push(
                l.chars()
                    .map(|c| {
                        (
                            c,
                            if c == '`' || c == '~' {
                                fence_style
                            } else {
                                Style::new().fg(CODE_LABEL).add_modifier(Modifier::BOLD)
                            },
                        )
                    })
                    .collect(),
            );
            let start = i + 1;
            let end = (start..lines.len())
                .find(|&j| crate::fence::fence_closes(&lines[j], open))
                .unwrap_or(lines.len());
            let code = lines[start..end].join("\n");
            let hl = highlight::highlight(&code, &lang);
            for (k, src) in lines[start..end].iter().enumerate() {
                // Tabs were expanded by the highlighter; map by text so columns stay aligned with the buffer.
                let coloured = hl.get(k).filter(|l| {
                    !src.contains('\t')
                        && l.spans
                            .iter()
                            .map(|s| s.content.as_ref())
                            .collect::<String>()
                            == *src
                });
                match coloured {
                    Some(line) => {
                        let mut cells = Vec::new();
                        for s in &line.spans {
                            let mut st = s.style;
                            st.bg = Some(CODE_BG);
                            cells.extend(s.content.chars().map(|c| (c, st)));
                        }
                        out.push(cells);
                    }
                    None => out.push(
                        src.chars()
                            .map(|c| (c, Style::new().fg(CODE_FG).bg(CODE_BG)))
                            .collect(),
                    ),
                }
            }
            i = end;
            if end < lines.len() {
                out.push(lines[end].chars().map(|c| (c, fence_style)).collect());
                i += 1;
            }
        } else {
            let mut cells = md_cells(l);
            for r in mentions.get(i).into_iter().flatten() {
                for cell in cells.iter_mut().take(r.end).skip(r.start) {
                    cell.1 = Style::new().fg(CODE_LABEL).add_modifier(Modifier::BOLD);
                }
            }
            out.push(cells);
            i += 1;
        }
    }
    out
}

/// The column width to draw for each character in `chars`: a whole grapheme cluster's width on its first character,
/// zero on the rest of the cluster. A variation selector or combining mark can change the width of the base character
/// it follows (`❤` alone is narrow, `❤️`, heart plus U+FE0F, is drawn wide), so measuring each `char` on its own and
/// summing, as [`char::width`] does one at a time, undercounts the cluster and leaves wrapping and the cursor column
/// a little short of where the terminal will actually draw them.
fn grapheme_col_widths(chars: &[char]) -> Vec<usize> {
    // Printable ASCII is one column a character and never joins its neighbours into a cluster.
    if chars.iter().all(|c| (' '..='~').contains(c)) {
        return vec![1; chars.len()];
    }
    let text: String = chars.iter().collect();
    let mut widths = vec![0; chars.len()];
    let mut i = 0;
    for cluster in text.graphemes(true) {
        widths[i] = cluster.width();
        i += cluster.chars().count();
    }
    widths
}

/// Whether the character at `col` of line `row` (whose length is `len`) is inside `sel`, the editor's selection.
fn in_selection(sel: Option<crate::editor::Selection>, row: usize, col: usize, len: usize) -> bool {
    match sel {
        Some(((sr, sc), (er, ec), linewise)) => {
            if row < sr || row > er {
                false
            } else if linewise {
                true
            } else {
                (row > sr || col >= sc) && (row < er || col <= ec) && col < len.max(1)
            }
        }
        None => false,
    }
}

/// What a line of the editor is drawn with besides its text: where the cursor stands in it (a character index) and the
/// selection.
#[derive(Clone, Copy)]
struct Marks {
    cursor: Option<usize>,
    selection: Option<crate::editor::Selection>,
}

/// One line of the text wrapped to `width` columns. The rows (what each shows, and the cells drawn), and where the
/// cursor falls among them (row within the line, column) when `marks` has one that is on a character or just after the
/// last.
fn layout_line(
    r: usize,
    line_cells: &[Cell],
    width: usize,
    marks: Marks,
) -> (
    Vec<Line<'static>>,
    Vec<view::RowSpan>,
    Option<(usize, usize)>,
) {
    let len = line_cells.len();
    let col_widths = {
        let chars: Vec<char> = line_cells.iter().map(|&(c, _)| c).collect();
        grapheme_col_widths(&chars)
    };
    let selected = |col: usize| in_selection(marks.selection, r, col, len);
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut row_spans: Vec<view::RowSpan> = Vec::new();
    let mut cursor = None;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    // What the row being built shows: the first character and the column each one starts at.
    let mut first = 0usize;
    let mut cols: Vec<usize> = Vec::new();
    for col in 0..=len {
        let (ch, mut style) = line_cells
            .get(col)
            .copied()
            .unwrap_or((' ', Style::new().fg(TEXT)));
        // Out of range only for the sentinel `col == len` (one past the last character, for a cursor or
        // selection at the end of the line), which always takes one column. Decided before anything wraps: a
        // line that exactly fills the row has no use for the sentinel, and wrapping for it would add an empty row.
        if col == len && !marks.cursor.is_some_and(|c| c >= len) && !selected(col) {
            break;
        }
        let cw = col_widths.get(col).copied().unwrap_or(1);
        if used + cw > width {
            rows.push(Line::from(std::mem::take(&mut spans)));
            row_spans.push(view::RowSpan {
                line: r,
                first,
                cols: std::mem::take(&mut cols),
                ends_line: false,
            });
            first = col;
            used = 0;
        }
        if marks.cursor == Some(col) {
            cursor = Some((rows.len(), used));
        }
        if selected(col) {
            style = style.add_modifier(Modifier::REVERSED);
        }
        // A character that takes no room (the emoji selector, a joiner, a combining mark) belongs to the one before it:
        // drawn on its own it would be dropped, and the emoji would lose its presentation. A character in the style of
        // the one before it joins that span too: a span for each character would be an allocation for each.
        match spans.last_mut() {
            Some(last) if cw == 0 && col < len => last.content.to_mut().push(ch),
            Some(last) if last.style == style => last.content.to_mut().push(ch),
            _ => spans.push(Span::styled(ch.to_string(), style)),
        }
        if col < len {
            if cw == 0 && !cols.is_empty() {
                cols.push(cols[cols.len() - 1]);
            } else {
                cols.push(used);
            }
        }
        used += cw;
    }
    rows.push(Line::from(spans));
    row_spans.push(view::RowSpan {
        line: r,
        first,
        cols,
        ends_line: true,
    });
    (rows, row_spans, cursor)
}

/// Bring `rows` up to date with the editor as it is now, wrapped to `width` columns, without redoing what has not
/// changed. `cells` holds the colouring for the editor's version; `cache` the rows of each line as they are without a
/// cursor or a selection, for a version and width, and what `rows` was last made for. A frame in which none of the
/// text, width, cursor and selection changed leaves `rows` alone. When only the cursor or the selection did, the lines
/// they touch are laid out again and the rest are copied from the cache. When the text or the width did, every line is.
fn refresh_editor_rows(
    ed: &crate::editor::Editor,
    cells: &mut (u64, Vec<Vec<Cell>>),
    cache: &mut view::RowsCache,
    rows: &mut EditorRows,
    width: usize,
    people: &MentionPeople,
) {
    let width = width.max(4);
    let text_version = ed.version();
    // The colouring depends on the people too: the caches are keyed on both.
    let version = if people.key == 0 {
        text_version
    } else {
        text_version ^ people.key.rotate_left(17)
    };
    let (cursor, selection) = (ed.cursor(), ed.selection());
    let key = view::RowsKey {
        version,
        width,
        cursor,
        selection,
    };
    if cache.made_for.as_ref() == Some(&key) {
        return;
    }
    if cells.0 != version {
        *cells = (
            version,
            editor_cells(ed.lines(), &people.ranges(ed.lines())),
        );
    }
    if cache.plain_for != Some((version, width)) {
        let bare = Marks {
            cursor: None,
            selection: None,
        };
        cache.plain = cells
            .1
            .iter()
            .enumerate()
            .map(|(r, line)| {
                let (lines, spans, _) = layout_line(r, line, width, bare);
                view::PlainRows { lines, spans }
            })
            .collect();
        cache.plain_for = Some((version, width));
        #[cfg(test)]
        {
            cache.plain_builds += 1;
        }
    }
    #[cfg(test)]
    {
        cache.assemblies += 1;
    }
    let (cr, cc) = cursor;
    let touched =
        |r: usize| r == cr || selection.is_some_and(|((sr, _), (er, _), _)| r >= sr && r <= er);
    let mut lines: Vec<Line<'static>> =
        Vec::with_capacity(cache.plain.iter().map(|p| p.lines.len()).sum());
    let mut spans: Vec<view::RowSpan> = Vec::with_capacity(lines.capacity());
    let mut at = (0usize, 0usize);
    for (r, (line, plain)) in cells.1.iter().zip(&cache.plain).enumerate() {
        if touched(r) {
            let marks = Marks {
                cursor: (r == cr).then_some(cc),
                selection,
            };
            let (l, s, found) = layout_line(r, line, width, marks);
            if let Some((row, col)) = found {
                at = (lines.len() + row, col);
            }
            lines.extend(l);
            spans.extend(s);
        } else {
            lines.extend(plain.lines.iter().cloned());
            spans.extend(plain.spans.iter().cloned());
        }
    }
    if lines.is_empty() {
        lines.push(Line::default());
        spans.push(view::RowSpan {
            ends_line: true,
            ..view::RowSpan::default()
        });
    }
    *rows = EditorRows {
        lines,
        cursor: at,
        spans,
        version: text_version,
    };
    cache.made_for = Some(key);
}

/// The editor's text wrapped to `width` columns and coloured, made from nothing but `cache` (the colouring of the
/// editor's version): no rows are kept between calls. What [`refresh_editor_rows`] keeps is checked against this.
#[cfg(test)]
fn build_editor_rows(
    ed: &crate::editor::Editor,
    cache: &mut (u64, Vec<Vec<Cell>>),
    width: usize,
) -> EditorRows {
    let mut rows = EditorRows::default();
    refresh_editor_rows(
        ed,
        cache,
        &mut view::RowsCache::default(),
        &mut rows,
        width,
        &MentionPeople::default(),
    );
    rows
}

/// What the message box is for, and its colour: the label of its pill when focused.
fn editor_label(app: &App) -> (&'static str, ratatui::style::Color) {
    // The standard keymap has no modes, so there is nothing to name but what the box is for.
    let in_thread = app.is_replying() && app.current_channel().is_some();
    if app.is_editing() {
        ("EDITING", WARN)
    } else if in_thread {
        ("REPLYING IN THREAD", ACCENT)
    } else if app.is_replying() {
        ("REPLYING", ACCENT)
    } else if app.keymap == Keymap::Standard {
        ("MESSAGE", OK)
    } else {
        mode_style(app.editor.mode())
    }
}

/// The colour of the message box's border: what it is for while it has the focus, dim otherwise.
fn editor_border_color(app: &App) -> ratatui::style::Color {
    if app.focus == Focus::Editor {
        editor_label(app).1
    } else {
        DIM
    }
}

/// While a reply is being written, the message replied to is framed in the message box's border colour and the frame's
/// left side runs on down into the box: a top edge on the row above the message (`╭─…─╮`; the corners go on its first
/// row when nothing is above it on screen), `│` on both sides of the message (the left in the column between the sender
/// bar and the text, the right in the free column at the edge), an edge on the blank row below it (`├─…─╯`), and the
/// left side going on through the messages after it and the banner (`┼` across a day or "new" divider) to a `┴` on the
/// box's top border. When the message starts above the screen its sides come in from the top with `↑`; when it is
/// below it, or not loaded, nothing is drawn. The labels over the transcript's rows and the cells of a picture are left
/// alone.
fn draw_reply_connector(frame: &mut Frame<'_>, app: &App, g: &Geometry) {
    let (Some(target), Some(st)) = (app.reply_target_id(), app.state()) else {
        return;
    };
    let Some(i) = st.messages().iter().position(|m| m.id == target) else {
        return;
    };
    let (Some(&first), area) = (st.transcript.starts.get(i), g.transcript) else {
        return;
    };
    if area.width < 8
        || area.height == 0
        || g.editor.height == 0
        || first >= st.scroll + usize::from(area.height)
    {
        return;
    }
    let (left, right) = (area.x + 1, area.right() - 1);
    if left <= g.editor.x || right + 1 >= g.editor.right() {
        return;
    }
    let colour = editor_border_color(app);
    let labels = hint_labels_of(app, g);
    let buffer = frame.buffer_mut();
    let mut put = |x: u16, y: u16, symbol: &str| {
        if labels.iter().any(|l| l.rect.contains((x, y).into())) {
            return;
        }
        if let Some(cell) = buffer.cell_mut((x, y))
            && cell.diff_option != CellDiffOption::Skip
        {
            let crossing = symbol == "│" && cell.symbol() == "─";
            cell.set_symbol(if crossing { "┼" } else { symbol });
            cell.set_fg(colour);
        }
    };
    let blank = |line: usize| {
        st.transcript
            .lines
            .get(line)
            .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
    };
    // Where the sides start: the row above the message when it is on screen and blank, else its own first row.
    let sides_from = if first >= st.scroll {
        let row = area.y + (first - st.scroll) as u16;
        let blank_above = first > st.scroll && blank(first - 1);
        let top = if blank_above { row - 1 } else { row };
        put(left, top, "╭");
        put(right, top, "╮");
        if blank_above {
            for x in left + 1..right {
                put(x, top, "─");
            }
        }
        top + 1
    } else {
        put(left, area.y, "↑");
        area.y + 1
    };
    // The message's last row on screen, if any of it is; the right side goes down to it.
    let last = st
        .transcript
        .ends
        .get(i)
        .filter(|&&e| e >= st.scroll)
        .map(|&e| area.y + (e - st.scroll).min(usize::from(area.height) - 1) as u16);
    // The edge under the message, on the blank row that follows it, closes the frame: the left side goes on through it
    // to the box, the right one ends there.
    let under = st
        .transcript
        .ends
        .get(i)
        .map(|&e| e + 1)
        .filter(|&l| l >= st.scroll && l < st.scroll + usize::from(area.height) && blank(l))
        .map(|l| area.y + (l - st.scroll) as u16)
        .filter(|&y| y < g.editor.y);
    if first < st.scroll && last.is_some() {
        put(right, area.y, "↑");
    }
    for y in sides_from..g.editor.y {
        if Some(y) == under {
            put(left, y, "├");
            for x in left + 1..right {
                put(x, y, "─");
            }
            put(right, y, "╯");
        } else {
            put(left, y, "│");
            if last.is_some_and(|l| y <= l) {
                put(right, y, "│");
            }
        }
    }
    put(left, g.editor.y, "┴");
}
fn draw_editor(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows = &app.view.editor_rows;
    let focused = app.focus == Focus::Editor;
    let in_thread = app.is_replying() && app.current_channel().is_some();
    let (label, color) = editor_label(app);
    let border_color = editor_border_color(app);
    let (cr, cc) = app.editor.cursor();

    let mut title = vec![Span::styled("─ ", Style::new().fg(border_color))];
    if focused {
        title.push(pill(label, color));
    } else {
        title.push(Span::styled(
            if app.is_editing() {
                "editing"
            } else if in_thread {
                "replying in thread"
            } else if app.is_replying() {
                "replying"
            } else {
                "message"
            },
            Style::new().fg(SUBTLE),
        ));
    }
    let pending = app.editor.pending();
    if focused && !pending.is_empty() {
        title.push(Span::styled(
            format!(" {pending} "),
            Style::new().fg(WARN).bold(),
        ));
    }
    title.push(Span::styled(" ", Style::new().fg(border_color)));
    // What this box will do besides send: the post being replied to, or the title the next post will have, or the
    // title an edit keeps. Cut to what the border has room for.
    let used: usize = title.iter().map(|s| s.content.width()).sum();
    let room = usize::from(area.width).saturating_sub(used + 14 + 4);
    let thread_label = if in_thread && !app.is_editing() {
        app.thread_label()
    } else {
        None
    };
    if let Some(name) = thread_label {
        if room >= 8 {
            title.push(Span::styled(
                format!("{} ", truncate(&name, room)),
                Style::new().fg(SUBTLE),
            ));
        }
    } else if app.is_keeping_subject() {
        title.push(pill("SUBJECT kept", WARN));
        title.push(Span::styled(" ", Style::new().fg(border_color)));
    } else if let Some(subject) = app.pending_subject() {
        let text = if room >= 20 {
            format!("SUBJECT: {}", truncate(subject, room.saturating_sub(9)))
        } else {
            "SUBJECT".to_string()
        };
        title.push(pill(&text, OK));
        title.push(Span::styled(" ", Style::new().fg(border_color)));
    }

    let mut right = vec![Span::styled(
        format!(" {}:{} ", cr + 1, cc + 1),
        Style::new().fg(DIM),
    )];
    right.push(Span::styled("─", Style::new().fg(border_color)));
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border_color))
        .title(Line::from(title))
        .title(Line::from(right).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let h = inner.height as usize;
    if h == 0 {
        return; // a terminal too short to show any of the editor
    }
    let top = app.view.editor_top;
    if app.editor.is_empty() && app.current_channel().is_some() && !app.can_post_here() {
        // Said whether or not the box has the keyboard: nothing typed here can be sent yet.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " Posting in channels is not available yet",
                Style::new().fg(DIM),
            ))),
            inner,
        );
    } else if app.editor.is_empty() && !focused {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                if app.keymap == Keymap::Standard && app.enter_sends {
                    " Message — start typing; ⏎ sends, shift+⏎ adds a line"
                } else if app.keymap == Keymap::Standard {
                    " Message — start typing; ⏎ adds a line, ^s or shift+⏎ sends"
                } else {
                    " Message — press i to type, ⏎ in normal mode sends"
                },
                Style::new().fg(DIM),
            ))),
            inner,
        );
    } else {
        let visible: Vec<Line<'_>> = rows
            .lines
            .iter()
            .skip(top)
            .take(h)
            .map(|l| {
                let mut spans = vec![Span::raw(" ")];
                spans.extend(l.spans.iter().cloned());
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(visible), inner);
    }
    if focused && app.cmdline.is_none() {
        let x = inner.x + 1 + rows.cursor.1 as u16;
        let y = inner.y + rows.cursor.0.saturating_sub(top) as u16;
        if x < inner.x + inner.width && y < inner.y + inner.height {
            frame.set_cursor_position(Position { x, y });
        }
    }
}

// ---------------------------------------------------------------------------
// Footer and overlays
// ---------------------------------------------------------------------------

fn standard_hints(focus: Focus, enter_sends: bool) -> Vec<(&'static str, &'static str)> {
    match focus {
        Focus::Chats => vec![
            ("↑↓", "move"),
            ("→ ⏎", "open"),
            ("type", "filter"),
            ("F7", "favourite"),
            ("^n", "new chat"),
            ("^k", "find"),
            ("F8", "pin favs"),
            ("F1", "help"),
            ("^q", "quit"),
        ],
        // Most useful first: on a narrow terminal the later ones are the ones left out (see `fit_hints`).
        Focus::Messages => vec![
            ("↑↓", "message"),
            ("^f", "search"),
            ("^e", "react"),
            ("F4", "edit"),
            ("^r", "quote reply"),
            ("→ ⏎", "write"),
            ("^c", "copy"),
            ("^o", "links"),
            ("^l", "load unverified"),
            ("←", "chats"),
            ("PgUp/PgDn", "scroll"),
            ("F1", "help"),
        ],
        Focus::Editor if enter_sends => vec![
            ("⏎", "send"),
            ("shift+⏎", "new line"),
            ("^c ^x ^v", "copy cut paste"),
            ("^z ^y", "undo redo"),
            ("^e", "emoji"),
            ("^f", "search"),
            ("esc", "back"),
            ("F1", "help"),
        ],
        // Enter adds a line; ^s is the send that works in every terminal.
        Focus::Editor => vec![
            ("^s shift+⏎", "send"),
            ("⏎", "new line"),
            ("^c ^x ^v", "copy cut paste"),
            ("^z ^y", "undo redo"),
            ("^e", "emoji"),
            ("^f", "search"),
            ("esc", "back"),
            ("F1", "help"),
        ],
    }
}

/// Columns one key hint takes: its key in a chip, then its label and two spaces.
fn hint_width(&(key, label): &(&str, &str)) -> usize {
    key.width() + 2 + 1 + label.width() + 2
}

/// The hints that fit in `width` columns, whole ones only, taken from the front (the list is in order of usefulness).
/// A last hint for the help key (`F1` or `?`) is kept whenever there is room for it, so the way to the full list of keys
/// is always on screen.
fn fit_hints(
    mut hints: Vec<(&'static str, &'static str)>,
    width: usize,
) -> Vec<(&'static str, &'static str)> {
    let help = hints
        .last()
        .copied()
        .filter(|(k, _)| matches!(*k, "F1" | "?"));
    if help.is_some() {
        hints.pop();
    }
    let reserved = help.as_ref().map_or(0, hint_width);
    // One column of margin at the left.
    let mut room = width.saturating_sub(1 + reserved);
    let mut shown = Vec::new();
    for hint in hints {
        let cost = hint_width(&hint);
        if cost > room {
            break;
        }
        room -= cost;
        shown.push(hint);
    }
    shown.extend(help);
    shown
}

/// The key hints for where the keyboard is: the chat list's, with the tab keys added when there are tabs, or the teams
/// list's own; and in a channel, without the keys for what cannot be done there yet (replying and editing).
fn hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let standard = app.keymap == Keymap::Standard;
    if app.focus == Focus::Chats && app.on_teams_tab() {
        return if standard {
            let mut hints = vec![
                ("↑↓", "move"),
                ("→ ⏎", "open / fold"),
                ("←", "fold"),
                ("type", "filter"),
            ];
            // Ctrl-F searches the channel that is open.
            if app.current_channel().is_some() {
                hints.push(("^f", "search"));
            }
            hints.extend([("alt+1", "chats"), ("F5", "refresh"), ("F1", "help")]);
            hints
        } else {
            vec![
                ("j/k", "move"),
                ("l ⏎", "open / fold"),
                ("h", "fold"),
                ("/", "filter"),
                ("gt", "chats"),
                ("i", "compose"),
                ("r", "refresh"),
                ("?", "help"),
                ("q", "quit"),
            ]
        };
    }
    let mut hints = base_hints(app);
    if app.focus == Focus::Chats && app.teams_available() {
        // Where the tab keys sit among the others: after the filter.
        let at = hints
            .iter()
            .position(|(_, l)| *l == "filter")
            .map_or(0, |i| i + 1);
        hints.insert(
            at,
            if standard {
                ("alt+2", "teams")
            } else {
                ("gt", "teams")
            },
        );
    }
    if app.focus == Focus::Messages && app.current_channel().is_some() {
        if app.can_post_here() {
            for (_, label) in &mut hints {
                if matches!(*label, "reply" | "quote reply") {
                    *label = "reply in thread";
                }
            }
        } else {
            hints.retain(|(_, label)| !matches!(*label, "edit" | "reply" | "quote reply"));
        }
    }
    hints
}

fn base_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    // While a message is edited the box is for that and nothing else.
    if app.is_editing() && app.focus == Focus::Editor {
        return if app.keymap == Keymap::Standard {
            vec![
                ("⏎", "save"),
                ("esc", "cancel"),
                ("^z", "undo"),
                ("F1", "help"),
            ]
        } else {
            vec![("^s", "save"), ("esc esc", "cancel"), ("F1", "help")]
        };
    }
    if app.keymap == Keymap::Standard {
        let mut hints = standard_hints(app.focus, app.enter_sends);
        // After a hit was opened, F3 steps through the hits: worth a place near the front while they are there.
        if app.focus == Focus::Messages && app.has_search_hits() {
            hints.insert(1, ("F3", "next hit"));
        }
        return hints;
    }
    match app.focus {
        Focus::Chats => vec![
            ("j/k", "move"),
            ("gg/G", "ends"),
            ("/", "filter"),
            ("U", "unread"),
            ("f F", "favourite/pin"),
            ("⏎", "open"),
            ("n", "new chat"),
            ("i", "compose"),
            ("^l", "right"),
            ("r", "refresh"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Focus::Messages => {
            let mut hints = vec![
                ("j/k", "message"),
                ("/", "search"),
                ("e", "edit"),
                ("+", "react"),
                ("r", "reply"),
                ("i", "compose"),
                ("y", "copy"),
                ("o", "links"),
                ("L", "load unverified"),
                ("^h", "chats"),
                ("^d/^u", "scroll"),
                ("gg/G", "ends"),
                ("I", "images"),
                ("?", "help"),
            ];
            // After a hit was opened, n and N step through the hits: worth a place near the front while they are there.
            if app.has_search_hits() {
                hints.insert(2, ("n/N", "next hit"));
            }
            hints
        }
        Focus::Editor => match app.editor.mode() {
            Mode::Normal => vec![
                ("i/a/o", "insert"),
                ("hjkl wb", "move"),
                ("d c y p", "edit"),
                ("u", "undo"),
                ("v", "visual"),
                ("⏎", "send"),
                ("esc", "back"),
            ],
            Mode::Insert => vec![
                ("esc", "normal"),
                ("^s ^⏎", "send"),
                ("⏎", "newline"),
                ("^w", "del word"),
                ("^u", "del line"),
                ("F2", "emoji"),
                ("^h", "chats"),
            ],
            Mode::Visual | Mode::VisualLine => vec![
                ("hjkl", "extend"),
                ("d", "delete"),
                ("y", "yank"),
                ("c", "change"),
                ("esc", "cancel"),
            ],
        },
    }
}

/// The always-visible input on the bottom row. It runs commands such as `:new …`.
fn draw_command_panel(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let bar = Style::new().bg(PANEL_BG);
    let line = match &app.cmdline {
        Some(cmd) => Line::from(vec![
            Span::styled(" ❯ ", Style::new().fg(WARN).bold()),
            Span::styled(":", Style::new().fg(WARN).bold()),
            Span::styled(cmd.clone(), Style::new().fg(TEXT)),
            Span::styled("▏", Style::new().fg(WARN)),
        ]),
        None => Line::from(vec![
            Span::styled(" ❯ ", Style::new().fg(DIM)),
            Span::styled(
                if app.keymap == Keymap::Standard {
                    "press ^p or click here for commands  ·  ^n starts a chat  ·  F1 shows the keys"
                } else {
                    "press : or click here for commands  ·  :new <name or email> starts a chat  ·  :help"
                },
                Style::new().fg(DIM),
            ),
        ]),
    };
    frame.render_widget(Paragraph::new(line).style(bar), area);
}

fn draw_footer(frame: &mut Frame<'_>, app: &App, g: &Geometry) {
    let (top, keys, panel) = (g.status, g.keys, g.panel);
    draw_command_panel(frame, app, panel);
    if let Some(c) = app
        .editor
        .command_line()
        .filter(|_| app.focus == Focus::Editor && app.cmdline.is_none())
    {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" :", Style::new().fg(WARN).bold()),
                Span::styled(c.to_string(), Style::new().fg(TEXT)),
                Span::styled("▏", Style::new().fg(WARN)),
            ])),
            top,
        );
    } else if !app.sign_in.is_online() {
        // The one thing to do about it, kept in view until it is done; the message box is untouched.
        let text = match (&app.sign_in, app.sign_in.failure()) {
            (SignIn::Waiting { .. }, _) => {
                " Finish signing in in your browser window; vtteams carries on by itself. What you are typing is kept."
                    .to_string()
            }
            (_, Some(_)) => " Sign-in failed. Press F6 to try again; what you are typing is kept.".to_string(),
            _ => " Your Teams sign-in has expired. Press F6 to sign in again; what you are typing is kept."
                .to_string(),
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate(&text, top.width as usize),
                Style::new().fg(WARN).bold(),
            ))),
            top,
        );
    } else if let Some((msg, _, err)) = &app.status {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {msg}"),
                Style::new().fg(if *err { ERR } else { OK }),
            ))),
            top,
        );
    } else if app.focus == Focus::Messages
        && let Some(st) = app.state()
        && st.messages().get(st.sel).is_some()
    {
        let n = st.transcript.links.get(st.sel).map_or(0, Vec::len);
        let text = format!(
            " message {}/{}{}",
            st.sel + 1,
            st.messages().len(),
            if n > 0 {
                format!("  ·  {n} link{}", if n == 1 { "" } else { "s" })
            } else {
                String::new()
            }
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, Style::new().fg(DIM)))),
            top,
        );
    }

    let mut spans = vec![Span::raw(" ")];
    for (k, label) in fit_hints(hints(app), keys.width as usize) {
        spans.push(chip(k));
        spans.push(Span::styled(format!(" {label}  "), Style::new().fg(SUBTLE)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), keys);
}

fn popup(frame: &mut Frame<'_>, area: Rect, w: u16, h: u16, title: &str) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    let r = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    frame.render_widget(Clear, r);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(gradient(0.6)))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(title.to_string(), Style::new().fg(gradient(0.3)).bold()),
            Span::raw(" "),
        ]))
        .style(Style::new().bg(PANEL_BG));
    let inner = block.inner(r);
    frame.render_widget(block, r);
    inner
}

/// The popup for an expired sign-in: the story, and buttons for the three things to do about it.
fn draw_sign_in(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let plan = crate::sign_in_modal::plan(area, &app.sign_in);
    popup(frame, area, plan.outer.width, plan.outer.height, "Sign in");
    let lines: Vec<Line<'_>> = plan
        .lines
        .iter()
        .map(|(kind, text)| {
            use crate::sign_in_modal::Kind;
            let style = match kind {
                Kind::Heading => Style::new().fg(WARN).bold(),
                Kind::Plain => Style::new().fg(TEXT),
                Kind::Label => Style::new().fg(SUBTLE).bold(),
                Kind::Detail => Style::new().fg(SUBTLE),
                Kind::Failure => Style::new().fg(ERR),
            };
            Line::from(Span::styled(text.clone(), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), plan.text);
    let waiting = matches!(app.sign_in, SignIn::Waiting { .. });
    for (button, rect, label) in &plan.buttons {
        let style = match button {
            crate::sign_in_modal::Button::SignIn if waiting => Style::new().fg(DIM).bg(CHIP_BG),
            crate::sign_in_modal::Button::SignIn => Style::new().fg(BG).bg(gradient(0.5)).bold(),
            _ => Style::new().fg(TEXT).bg(CHIP_BG),
        };
        frame.render_widget(
            Paragraph::new(Span::styled(format!(" {label}"), style)).style(style),
            *rect,
        );
    }
}

fn draw_links(frame: &mut Frame<'_>, links: &[String], area: Rect) {
    let w = links.iter().map(|l| l.width()).max().unwrap_or(20).min(100) as u16 + 8;
    let inner = popup(frame, area, w, links.len() as u16 + 4, "Open link");
    let mut lines: Vec<Line<'_>> = links
        .iter()
        .enumerate()
        .map(|(i, l)| {
            Line::from(vec![
                Span::raw(" "),
                chip(&(i + 1).to_string()),
                Span::styled(
                    format!(" {}", truncate(l, inner.width.saturating_sub(7) as usize)),
                    Style::new().fg(LINK),
                ),
            ])
        })
        .collect();
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        " press a number, any other key cancels",
        Style::new().fg(DIM),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The emoji selector: a search line, a grid of the matches, and the name of the one under the cursor.
fn draw_emoji(
    frame: &mut Frame<'_>,
    app: &App,
    target: &EmojiTarget,
    query: &str,
    sel: usize,
    area: Rect,
) {
    const CELL: usize = 4;
    const MAX_ROWS: usize = EMOJI_ROWS;
    let entries = app.emoji_entries(target);
    let hits = emoji::search(entries, query);
    let rows_total = hits.len().div_ceil(EMOJI_COLUMNS).max(1);
    // The popup is the grid plus seven rows (borders, search, name, hint, blanks) and is kept off the screen's edge;
    // on a short terminal fewer grid rows are shown and the window follows the selection.
    let fit = usize::from(area.height.saturating_sub(2 + 7)).max(1);
    let shown = rows_total.min(MAX_ROWS).min(fit);
    let sel = sel.min(hits.len().saturating_sub(1));
    let sel_row = sel / EMOJI_COLUMNS;
    let first_row = if sel_row < shown {
        0
    } else {
        sel_row + 1 - shown
    };
    let (what, mine): (&str, &[String]) = match target {
        EmojiTarget::Insert => ("Insert emoji", &[]),
        EmojiTarget::React { mine, .. } => ("React", mine.as_slice()),
    };
    // How long the list is, so a long one is known to be long (the service's has over a thousand), and how much of it
    // what has been typed leaves.
    let title = if query.trim().trim_matches(':').is_empty() {
        format!("{what} · {}", entries.len())
    } else {
        format!("{what} · {} of {}", hits.len(), entries.len())
    };
    let width = (EMOJI_COLUMNS * CELL + 4) as u16;
    let inner = popup(frame, area, width, shown as u16 + 7, &title);
    let mut lines: Vec<Line<'_>> = vec![
        Line::from(vec![
            Span::styled(" Search ", Style::new().fg(SUBTLE)),
            Span::styled(query.to_string(), Style::new().fg(TEXT)),
            Span::styled("▏", Style::new().fg(WARN)),
        ]),
        Line::default(),
    ];
    if hits.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" nothing matches '{query}'"),
            Style::new().fg(DIM),
        )));
    }
    for row in first_row..first_row + shown {
        let cells: Vec<Span<'_>> = (0..EMOJI_COLUMNS)
            .filter_map(|col| {
                let i = row * EMOJI_COLUMNS + col;
                let entry = hits.get(i)?;
                let pad = 2usize.saturating_sub(entry.emoji.width());
                let mut style = Style::new().fg(TEXT);
                if mine.iter().any(|m| same_emoji(m, &entry.emoji)) {
                    style = style.fg(OK).add_modifier(Modifier::UNDERLINED);
                }
                if i == sel {
                    style = style.bg(POPUP_SEL_BG).add_modifier(Modifier::BOLD);
                }
                Some(Span::styled(
                    format!(" {}{} ", entry.emoji, " ".repeat(pad)),
                    style,
                ))
            })
            .collect();
        if !cells.is_empty() {
            lines.push(Line::from(cells));
        }
    }
    lines.push(Line::default());
    let name = match hits.get(sel) {
        Some(entry) => {
            let yours = mine.iter().any(|m| same_emoji(m, &entry.emoji));
            format!(
                " :{}:{}",
                entry.name,
                if yours {
                    "   you reacted with this: ⏎ takes it back"
                } else {
                    ""
                }
            )
        }
        None => String::new(),
    };
    lines.push(Line::from(Span::styled(name, Style::new().fg(WARN))));
    lines.push(Line::from(Span::styled(
        " type to search · ⏎ choose · esc cancel",
        Style::new().fg(DIM),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The last `max` columns of `s`, for a line being typed that is longer than its box.
fn tail(s: &str, max: usize) -> String {
    let mut width = 0;
    let mut kept: Vec<char> = Vec::new();
    for c in s.chars().rev() {
        let cw = c.width().unwrap_or(0);
        if width + cw > max {
            break;
        }
        width += cw;
        kept.push(c);
    }
    kept.into_iter().rev().collect()
}

/// `text` as spans, with the parts that match `query` (ignoring case) picked out.
fn highlight_matches(text: &str, query: &str, base: Style) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut at = 0;
    let lit = base
        .fg(WARN)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    for m in crate::app::find_matches(text, query) {
        if m.start > at {
            spans.push(Span::styled(
                chars[at..m.start].iter().collect::<String>(),
                base,
            ));
        }
        spans.push(Span::styled(
            chars[m.start..m.end].iter().collect::<String>(),
            lit,
        ));
        at = m.end;
    }
    if at < chars.len() {
        spans.push(Span::styled(chars[at..].iter().collect::<String>(), base));
    }
    spans
}

/// The message search: the text being searched for, what it matched and how it was found, and the rows, newest or best
/// first as the service has them.
fn draw_search(frame: &mut Frame<'_>, app: &App, area: Rect) {
    use crate::app::{search_list_rows, search_time};
    let Some(view) = app.search_view() else {
        return;
    };
    let capacity = search_list_rows(area.height);
    let shown = view.rows.len().clamp(3, capacity.max(3));
    let first = if view.sel < shown {
        0
    } else {
        view.sel + 1 - shown
    };
    let window: Vec<&crate::app::SearchRow> = view.rows.iter().skip(first).take(shown).collect();
    let sender_width = window
        .iter()
        .map(|r| r.sender.width())
        .max()
        .unwrap_or(0)
        .clamp(6, 22);
    // A team search says which channel each hit is in, `#channel`, between the sender and the text.
    let channel_width = if view.show_channel {
        window
            .iter()
            .map(|r| r.channel.width() + usize::from(!r.channel.is_empty()))
            .max()
            .unwrap_or(0)
            .clamp(1, 20)
    } else {
        0
    };
    let inner = popup(frame, area, 110, shown as u16 + 5, &view.title);
    let width = usize::from(inner.width);
    let mut lines: Vec<Line<'_>> = vec![
        Line::from(vec![
            Span::styled(" / ", Style::new().fg(WARN).bold()),
            Span::styled(
                tail(view.query, width.saturating_sub(5)),
                Style::new().fg(TEXT),
            ),
            Span::styled("▏", Style::new().fg(WARN)),
        ]),
        Line::from(Span::styled(
            format!(" {}", truncate(&view.status, width.saturating_sub(2))),
            Style::new().fg(if view.status_is_error { ERR } else { SUBTLE }),
        )),
    ];
    for (i, r) in window.iter().enumerate() {
        let selected = first + i == view.sel;
        let (bg, mark) = if selected {
            (POPUP_SEL_BG, "▶")
        } else {
            (PANEL_BG, " ")
        };
        let channel_room = if channel_width > 0 {
            channel_width + 2
        } else {
            0
        };
        let room = width.saturating_sub(3 + 16 + 2 + sender_width + 2 + channel_room + 1);
        let sender = truncate(&r.sender, sender_width);
        let pad = " ".repeat(sender_width.saturating_sub(sender.width()));
        let preview = truncate(&r.preview, room);
        let mut spans = vec![
            Span::styled(format!(" {mark} "), Style::new().fg(gradient(0.6)).bg(bg)),
            Span::styled(search_time(r.sent), Style::new().fg(SUBTLE).bg(bg)),
            Span::styled("  ", Style::new().bg(bg)),
            Span::styled(
                format!("{sender}{pad}"),
                Style::new().fg(person_color(&r.sender)).bg(bg).bold(),
            ),
            Span::styled("  ", Style::new().bg(bg)),
        ];
        if channel_width > 0 {
            let name = if r.channel.is_empty() {
                String::new()
            } else {
                truncate(&format!("#{}", r.channel), channel_width)
            };
            let pad = " ".repeat(channel_width.saturating_sub(name.width()));
            spans.push(Span::styled(
                format!("{name}{pad}"),
                Style::new().fg(SUBTLE).bg(bg),
            ));
            spans.push(Span::styled("  ", Style::new().bg(bg)));
        }
        spans.extend(highlight_matches(
            &preview,
            view.query.trim(),
            Style::new().fg(TEXT).bg(bg),
        ));
        lines.push(Line::from(spans).style(Style::new().bg(bg)));
    }
    for _ in window.len()..shown {
        lines.push(Line::default());
    }
    let toggle = view
        .toggle
        .as_ref()
        .map_or_else(String::new, |t| format!(" · {t}"));
    lines.push(Line::from(Span::styled(
        format!(" ⏎ {} · ↑↓ PgUp PgDn move{toggle} · esc closes", view.enter),
        Style::new().fg(DIM),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_people(frame: &mut Frame<'_>, app: &App, people: &[Contact], sel: usize, area: Rect) {
    let shown = &people[..people.len().min(crate::app::MAX_PEOPLE_SHOWN)];
    let rows: Vec<Vec<Line<'static>>> = shown
        .iter()
        .enumerate()
        .map(|(i, p)| person_rows(app, p, i, i == sel))
        .collect();
    let widest = rows.iter().flatten().map(|l| l.width()).max().unwrap_or(40);
    let w = (widest + 4).clamp(50, 120) as u16;
    let height = rows.iter().map(Vec::len).sum::<usize>() as u16 + 4;
    let inner = popup(frame, area, w, height, "Start a chat with…");
    let room = usize::from(inner.width);
    let mut lines: Vec<Line<'_>> = Vec::new();
    for (i, person) in rows.into_iter().enumerate() {
        let bg = if i == sel { POPUP_SEL_BG } else { PANEL_BG };
        for line in person {
            lines.push(fit_line(line, room, bg));
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        " ↑↓ / j k choose  ·  1-9 jump  ·  ⏎ open  ·  esc cancel",
        Style::new().fg(DIM),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The rows of one person in the `:new` picker: the name with how available they are and their status message, then
/// their email and phone, then their job and office. A row with nothing to say is left out. The selected person's
/// details are in the normal text colour, the others' in the lighter grey: the dim one is unreadable on the highlight.
fn person_rows(app: &App, p: &Contact, i: usize, selected: bool) -> Vec<Line<'static>> {
    let (bg, mark) = if selected {
        (POPUP_SEL_BG, "▶")
    } else {
        (PANEL_BG, " ")
    };
    let detail = Style::new().fg(if selected { TEXT } else { SUBTLE }).bg(bg);
    let who = if p.name.is_empty() {
        p.email.clone()
    } else {
        p.name.clone()
    };
    let mut first = vec![
        Span::styled(format!(" {mark} "), Style::new().fg(gradient(0.6)).bg(bg)),
        chip(&(i + 1).to_string()),
        Span::styled(
            format!(" {who}"),
            Style::new().fg(person_color(&who)).bg(bg).bold(),
        ),
    ];
    if let Some(presence) = app.presence_of_id(&p.id) {
        first.push(Span::styled("   ", Style::new().bg(bg)));
        if let Some(m) = crate::presence::mark(app.presence_style(), presence) {
            first.push(Span::styled(m.glyph, Style::new().fg(m.color).bg(bg)));
            first.push(Span::styled(" ", Style::new().bg(bg)));
        }
        first.push(Span::styled(crate::presence::describe(presence), detail));
        let note = presence
            .out_of_office_note
            .as_ref()
            .filter(|_| presence.out_of_office)
            .or(presence.note.as_ref());
        if let Some(note) = note {
            first.push(Span::styled(
                format!("  ·  {}", note.text),
                detail.add_modifier(Modifier::ITALIC),
            ));
        }
    }
    let joined = |parts: &[&str]| {
        parts
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("  ·  ")
    };
    let mut rows = vec![Line::from(first)];
    for text in [
        joined(&[&p.email, &p.phone]),
        joined(&[&p.detail, &p.office]),
    ] {
        if !text.is_empty() {
            rows.push(Line::from(Span::styled(format!("       {text}"), detail)));
        }
    }
    rows
}

/// `line` cut to `room` columns (with "…") and padded to the full width in `bg`, so a highlighted row is one solid bar.
fn fit_line(line: Line<'static>, room: usize, bg: ratatui::style::Color) -> Line<'static> {
    let mut used = 0;
    let mut spans = Vec::new();
    for span in line.spans {
        let w = span.content.width();
        if used + w <= room {
            used += w;
            spans.push(span);
        } else {
            let cut = truncate(&span.content, room - used);
            used += cut.width();
            spans.push(Span::styled(cut, span.style));
            break;
        }
    }
    spans.push(Span::styled(
        " ".repeat(room.saturating_sub(used)),
        Style::new().bg(bg),
    ));
    Line::from(spans)
}
/// The dialog for choosing your own status (F9, `:status`).
fn draw_status(frame: &mut Frame<'_>, app: &App, sel: usize, area: Rect) {
    use crate::presence::{PresenceStyle, mark};
    use crate::status::CHOICES;
    // The same marks as beside the chats; a one-column dot where presence is switched off.
    let style = match app.presence_style() {
        PresenceStyle::Off => PresenceStyle::Dot,
        s => s,
    };
    let mut lines: Vec<Line<'_>> = Vec::new();
    for (i, (name, choice)) in CHOICES.iter().enumerate() {
        let (bg, pointer) = if i == sel {
            (POPUP_SEL_BG, "▶")
        } else {
            (PANEL_BG, " ")
        };
        let glyph = match choice {
            Some(a) => mark(style, &teams_client::Presence::new(*a, false)).map_or_else(
                || Span::styled(" ".repeat(style.width()), Style::new().bg(bg)),
                |m| Span::styled(m.glyph, Style::new().fg(m.color).bg(bg)),
            ),
            None => Span::styled(
                format!("↺{}", " ".repeat(style.width().saturating_sub(1))),
                Style::new().fg(SUBTLE).bg(bg),
            ),
        };
        let current = if app.status_choice_is_current(i) {
            " ✓"
        } else {
            "  "
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {pointer} "),
                Style::new().fg(gradient(0.6)).bg(bg),
            ),
            chip(&(i + 1).to_string()),
            Span::styled(" ", Style::new().bg(bg)),
            glyph,
            Span::styled(
                format!(" {name}"),
                Style::new().fg(TEXT).bg(bg).add_modifier(if i == sel {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Span::styled(current, Style::new().fg(OK).bg(bg)),
        ]));
    }
    // The last row of the list is not a status: how long without a key before you show as away. It is stepped
    // through (← → or ⏎, or `a` from anywhere) and the dialog stays open.
    let (bg, pointer) = if sel == CHOICES.len() {
        (POPUP_SEL_BG, "▶")
    } else {
        (PANEL_BG, " ")
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {pointer} "),
            Style::new().fg(gradient(0.6)).bg(bg),
        ),
        chip("8"),
        Span::styled(" ◷ ", Style::new().fg(SUBTLE).bg(bg)),
        Span::styled(
            app.away_after().row_text(),
            Style::new()
                .fg(TEXT)
                .bg(bg)
                .add_modifier(if sel == CHOICES.len() {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::styled("  ◂ ▸", Style::new().fg(SUBTLE).bg(bg)),
    ]));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        " ↑↓ choose · 1-7 set · 8 or a: away time · esc closes",
        Style::new().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        " The away time lasts until you quit; away_after keeps it",
        Style::new().fg(DIM),
    )));
    // As wide as its longest line needs (and the borders and a space each side), and as tall as it has lines and a
    // blank one under them: a fixed size is what clipped the help text.
    let widest = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let inner = popup(
        frame,
        area,
        (widest + 4).max(40),
        lines.len() as u16 + 3,
        "Set your status",
    );
    frame.render_widget(Paragraph::new(lines), inner);
}

type HelpSection = (&'static str, &'static [(&'static str, &'static str)]);

const VIM_HELP: [HelpSection; 4] = [
    (
        "Chats",
        &[
            ("j k / ↑ ↓", "move"),
            ("gg  G", "first / last"),
            ("^d ^u", "jump"),
            ("⏎ l →", "open conversation"),
            ("i", "compose in this chat"),
            ("/", "filter (esc clears)"),
            ("U", "unread only"),
            (
                "f  F7",
                "favourite / unfavourite the chat (a star; it moves under the divider at the top)",
            ),
            ("F  F8", "pin favourites on top / list them with the others"),
            (
                "n  :new <name|email>",
                "start a chat with someone not in the list",
            ),
            ("r", "refresh"),
            ("F6", "sign in again when it has expired (any pane)"),
            (
                "F9  :status",
                "set your own status: available, busy, do not disturb ... (any pane)",
            ),
            ("o", "open chat in Teams web"),
        ],
    ),
    (
        "Teams (a tab beside CHATS when the service has teams)",
        &[
            (
                "gt  gT  :teams  :chats",
                "switch tab (a click on a tab does too)",
            ),
            ("j k  gg G  ^d ^u", "move through the teams and channels"),
            (
                "Team › Channel",
                "your favourite channels, on top under a divider; F  F8 show or hide them",
            ),
            ("l ⏎ →", "open a team, a channel, or +N more channels"),
            ("h ←", "close a team, or go from a channel to its team"),
            ("/", "filter teams and channels (a match opens its team)"),
            ("r", "refresh (the list is asked for every 10 minutes)"),
            (
                "(in a channel)",
                "post (the first asks once), reply in a thread, edit; :subject x titles a post",
            ),
        ],
    ),
    (
        "Conversation",
        &[
            ("j k", "select message"),
            ("^d ^u ^f ^b", "scroll"),
            ("^e ^y", "scroll a line"),
            ("gg  G", "oldest (loads more) / latest"),
            (
                "/  :search x",
                "search this chat (⏎ asks the service, ⏎ again opens)",
            ),
            (
                "tab (in the search)",
                "in a channel: all of its team / only this channel",
            ),
            ("n  N  F3  shift+F3", "next / previous hit of the search"),
            (
                "r",
                "reply (quotes message; in a channel: in the post's thread, nothing quoted)",
            ),
            ("y", "copy message as Markdown"),
            ("o", "open link (1-9 if several), or the picture"),
            (
                "L",
                "load unverified images (from outside your organisation)",
            ),
            (
                "e  F4",
                "edit one of your messages (⏎ or ^s saves, esc cancels)",
            ),
            ("+  F2", "react with an emoji (again to take it back)"),
            (
                "f  F  F7  F8",
                "favourite / unfavourite the chat, pin favourites (as in the chat list)",
            ),
            ("I", "cycle image renderer"),
            ("i a ⏎ → tab", "compose"),
            ("h ← esc", "back to chats"),
        ],
    ),
    (
        "Editor (vim)",
        &[
            ("i a I A o O", "enter insert mode"),
            ("h j k l w b e 0 $ gg G f t", "motions, counts"),
            ("d c y + motion / iw i\" i( ip", "operators, text objects"),
            ("dd cc yy p P x J ~ r .", "line ops, paste, repeat"),
            ("u  ^r", "undo / redo"),
            ("v  V", "visual / visual line"),
            ("F2", "insert an emoji"),
            ("⏎ (normal)  ^s", "send message"),
            (
                "^⏎  shift+⏎  alt+⏎  (insert)",
                "send without leaving insert mode",
            ),
            ("esc (normal)", "back to conversation"),
            (
                ":w :q :new x :images x :open",
                "commands (also in the bottom panel: press : or click it)",
            ),
            (
                "(above the message box)",
                "what the people in the open chat say: status messages (grey), out of office (red); presence = off hides them",
            ),
        ],
    ),
];

const STANDARD_HELP: [HelpSection; 5] = [
    (
        "Everywhere",
        &[
            ("^k", "find a chat (type to filter, esc clears)"),
            ("^n", "start a chat with someone: name or email"),
            ("^p", "commands: refresh, open (in Teams web), images ..."),
            (
                "^e  F2",
                "emoji: insert in the message box, react in the conversation",
            ),
            ("F5", "refresh"),
            (
                "F7",
                "favourite / unfavourite the open chat (favourites sit at the top under a divider)",
            ),
            ("F8", "pin favourites on top / list them with the others"),
            ("F6", "sign in again when the sign-in has expired"),
            (
                "F9  :status",
                "set your own status: available, busy, do not disturb ...",
            ),
            ("F1", "this help"),
            ("^q", "quit"),
        ],
    ),
    (
        "Chats",
        &[
            ("↑ ↓  PgUp PgDn  Home End", "move"),
            ("⏎  →  tab", "open conversation (← comes back)"),
            ("(type)", "filter the list"),
            ("^u", "unread only"),
        ],
    ),
    (
        "Teams (a tab beside CHATS when the service has teams)",
        &[
            (
                "alt+1  alt+2  :chats  :teams",
                "switch tab (a click on a tab does too)",
            ),
            (
                "↑ ↓  PgUp PgDn  Home End",
                "move through the teams and channels",
            ),
            (
                "Team › Channel",
                "your favourite channels, on top under a divider; F8 shows or hides them",
            ),
            ("⏎  →  tab", "open a team, a channel, or +N more channels"),
            ("←", "close a team, or go from a channel to its team"),
            (
                "(type)",
                "filter teams and channels (a match opens its team)",
            ),
            ("F5", "refresh (the list is asked for every 10 minutes)"),
            ("^f", "search the open channel (tab: all of its team)"),
            (
                "(in a channel)",
                "post (the first asks once), reply in a thread, edit; :subject x titles a post",
            ),
        ],
    ),
    (
        "Conversation",
        &[
            ("↑ ↓  PgUp PgDn  Home End", "select message / scroll"),
            ("⏎  →  tab  (type)", "write a message"),
            (
                "^f  :search x",
                "search this chat (also from the message box; ⏎ asks the service)",
            ),
            (
                "tab (in the search)",
                "in a channel: all of its team / only this channel",
            ),
            ("F3  shift+F3", "next / previous hit of the search"),
            (
                "^r",
                "reply (quotes the message; in a channel: in the post's thread, nothing quoted)",
            ),
            ("^c", "copy the message as Markdown"),
            ("^o", "open its links (1-9 if several), or the picture"),
            (
                "^l",
                "load unverified images (from outside your organisation)",
            ),
            ("^e  F2", "react with an emoji (again to take it back)"),
            (
                "F4",
                "edit the selected message, if it is yours (⏎ saves, esc cancels)",
            ),
            ("esc  ←", "back to chats"),
        ],
    ),
    (
        "Message box",
        &[
            ("⏎  or  ^s", "send (or save, while editing)"),
            ("↑ (empty box)", "edit your last message"),
            (
                "shift+⏎  ^⏎  ^j",
                "new line (⏎ inside an open ``` block too)",
            ),
            ("shift + ← → ↑ ↓", "select;  ^a selects everything"),
            ("^c  ^x  ^v  shift+ins", "copy, cut, paste"),
            ("^z  ^y  ^shift+z", "undo, redo"),
            ("tab  shift+tab", "indent / unindent"),
            ("^e", "insert an emoji (type to search)"),
            ("^← →  ^bksp  ^del", "move / delete by word"),
            ("home end  ^home ^end", "line / whole message"),
            ("esc", "clear the selection, then back to the conversation"),
            ("←  (when empty)", "back to the conversation"),
            (
                "(above the message box)",
                "what the people in the open chat say: status messages (grey), out of office (red); presence = off hides them",
            ),
        ],
    ),
];

/// The lines of the help for `keymap`: its sections, then a few lines on writing and closing.
/// [`help_lines_for`] with Enter sending, which is what the standard keymap does unless the `enter` setting says otherwise.
pub fn help_lines(keymap: Keymap) -> Vec<Line<'static>> {
    help_lines_for(keymap, true)
}

/// The lines of the help for `keymap` when a plain Enter sends (`enter_sends`) or adds a line (standard keymap only).
pub fn help_lines_for(keymap: Keymap, enter_sends: bool) -> Vec<Line<'static>> {
    let (sections, footer): (&[HelpSection], &str) = match keymap {
        Keymap::Vim => (
            &VIM_HELP,
            " ^h / ^l move between panes from anywhere (Tab and ^w h,l also work).   q or :q quits.",
        ),
        Keymap::Standard => (
            &STANDARD_HELP,
            " Started with --keymap standard (vim keys: --keymap vim).   esc goes back one pane.",
        ),
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (title, items) in sections {
        lines.push(Line::from(Span::styled(
            format!(" {title}"),
            Style::new().fg(gradient(0.2)).bold(),
        )));
        for &(k, d) in *items {
            // With Enter adding a line, the message box swaps the two rows that name Enter.
            let (k, d) = match (keymap, enter_sends, k) {
                (Keymap::Standard, false, "⏎  or  ^s") => ("shift+⏎  ^⏎  or  ^s", d),
                (Keymap::Standard, false, "shift+⏎  ^⏎  ^j") => ("⏎  ^j", "new line"),
                _ => (k, d),
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{k:<32}"), Style::new().fg(WARN)),
                Span::styled(d.to_string(), Style::new().fg(TEXT)),
            ]));
        }
        lines.push(Line::default());
    }
    lines.push(Line::from(Span::styled(" Write Markdown: **bold**, `code`, ```lang fences```, @Name mentions (typing @ offers the chat's people, or in a channel those who have posted there: Up/Down, Tab or Enter, Esc), :emoji: — sent as Teams HTML.", Style::new().fg(SUBTLE))));
    lines.push(Line::from(Span::styled(footer, Style::new().fg(SUBTLE))));
    lines.push(Line::from(Span::styled(
        " ↑ ↓ PgUp PgDn scroll this help; any other key closes it.",
        Style::new().fg(SUBTLE),
    )));
    lines
}

/// How many rows of help fit on a terminal `height` rows high (the popup keeps a row of margin and a border).
pub fn help_rows(keymap: Keymap, height: u16) -> usize {
    help_lines(keymap)
        .len()
        .min(usize::from(height.saturating_sub(4)))
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, app: &App, scroll: usize) {
    let lines = help_lines_for(app.keymap, app.enter_sends);
    let inner = popup(frame, area, 100, lines.len() as u16 + 2, "Keys");
    // Short terminals show a window onto the help; the scroll position is kept within it.
    let most = lines.len().saturating_sub(usize::from(inner.height));
    let top = u16::try_from(scroll.min(most)).unwrap_or(u16::MAX);
    frame.render_widget(Paragraph::new(lines).scroll((top, 0)), inner);
}

fn sgr_color(c: ratatui::style::Color, fg: bool) -> Option<String> {
    use ratatui::style::Color as C;
    let base = if fg { 30 } else { 40 };
    Some(match c {
        C::Reset => return None,
        C::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", if fg { 38 } else { 48 }),
        C::Indexed(n) => format!("{};5;{n}", if fg { 38 } else { 48 }),
        C::Black => base.to_string(),
        C::Red => (base + 1).to_string(),
        C::Green => (base + 2).to_string(),
        C::Yellow => (base + 3).to_string(),
        C::Blue => (base + 4).to_string(),
        C::Magenta => (base + 5).to_string(),
        C::Cyan => (base + 6).to_string(),
        C::Gray => (base + 7).to_string(),
        C::DarkGray => (base + 60).to_string(),
        C::LightRed => (base + 61).to_string(),
        C::LightGreen => (base + 62).to_string(),
        C::LightYellow => (base + 63).to_string(),
        C::LightBlue => (base + 64).to_string(),
        C::LightMagenta => (base + 65).to_string(),
        C::LightCyan => (base + 66).to_string(),
        C::White => (base + 67).to_string(),
    })
}

/// Render one frame with 24-bit colour escapes, so `--dump --ansi` shows the real thing in any truecolor terminal.
pub fn render_to_ansi(app: &mut App, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal.draw(|f| render(f, app)).expect("draw");
    let buf = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            let mut codes: Vec<String> = vec!["0".into()];
            let m = cell.modifier;
            for (flag, code) in [
                (Modifier::BOLD, "1"),
                (Modifier::DIM, "2"),
                (Modifier::ITALIC, "3"),
                (Modifier::UNDERLINED, "4"),
                (Modifier::REVERSED, "7"),
                (Modifier::CROSSED_OUT, "9"),
            ] {
                if m.contains(flag) {
                    codes.push(code.into());
                }
            }
            codes.extend(sgr_color(cell.fg, true));
            codes.extend(sgr_color(cell.bg, false));
            out.push_str(&format!("\x1b[{}m{}", codes.join(";"), cell.symbol()));
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

/// Render one frame to plain text (used by `--dump` and tests).
pub fn render_to_string(app: &mut App, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal.draw(|f| render(f, app)).expect("draw");
    let buf = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod editor_tests;
#[cfg(test)]
mod frame_cost;

#[cfg(test)]
mod tests {
    #[test]
    fn a_group_preview_with_no_known_sender_has_no_bare_colon() {
        use chrono::Utc;
        let said = |from_id: Option<&str>, from_name: Option<&str>| teams_client::Preview {
            text: "hello".into(),
            created: Utc::now(),
            from_id: from_id.map(String::from),
            from_name: from_name.map(String::from),
            automated: false,
        };
        let me = "me-id";
        let group = ChatKind::Group;
        assert_eq!(
            preview_prefix(
                Some(&said(Some("a"), Some("Ada Lovelace"))),
                group,
                me,
                false
            ),
            "Ada: "
        );
        assert_eq!(
            preview_prefix(Some(&said(Some(me), Some("Me"))), group, me, false),
            "You: "
        );
        // Nobody to introduce: no ": " in front of the text, whether the name is absent or blank.
        assert_eq!(
            preview_prefix(Some(&said(None, None)), group, me, false),
            ""
        );
        assert_eq!(
            preview_prefix(Some(&said(Some("a"), Some("  "))), group, me, false),
            ""
        );
        assert_eq!(
            preview_prefix(
                Some(&said(Some("a"), Some("Ada"))),
                ChatKind::OneOnOne,
                me,
                false
            ),
            ""
        );
        assert_eq!(
            preview_prefix(Some(&said(Some("a"), Some("Ada"))), group, me, true),
            ""
        );
        assert_eq!(preview_prefix(None, group, me, false), "");
    }
    use super::*;
    use crate::editor::Editor;

    #[test]
    fn the_attendee_line_names_as_many_people_as_fit_and_counts_the_rest() {
        let names = ["Ada Lovelace", "Grace Hopper", "Alan Turing"];
        assert_eq!(
            attendee_line(&names, 4, 80),
            " Ada Lovelace, Grace Hopper, Alan Turing · 4 people"
        );
        assert_eq!(
            attendee_line(&names, 4, 46),
            " Ada Lovelace, Grace Hopper +1 more · 4 people"
        );
        assert_eq!(
            attendee_line(&names, 4, 40),
            " Ada Lovelace +2 more · 4 people"
        ); // Nothing fits but the count: the count alone, and never wider than the line.
        assert_eq!(attendee_line(&names, 4, 20), " 4 people");
        assert_eq!(attendee_line(&names, 4, 11), " 4 people");
        assert!(attendee_line(&names, 4, 5).width() <= 5);
        // People the directory has not named yet are only counted.
        assert_eq!(attendee_line(&[], 6, 40), " 6 people");
    }

    #[test]
    fn the_matches_in_a_search_row_are_picked_out_and_nothing_else_changes() {
        let spans = highlight_matches("Join the join fix", "JOIN", Style::new());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            text, "Join the join fix",
            "the text is the same, only styled"
        );
        let lit: Vec<&str> = spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(
            lit,
            ["Join", "join"],
            "as typed in the text, whatever the case typed"
        );
        let plain = highlight_matches("nothing here", "zzz", Style::new());
        assert!(
            plain
                .iter()
                .all(|s| !s.style.add_modifier.contains(Modifier::UNDERLINED))
        );
    }

    #[test]
    fn a_long_line_being_typed_shows_its_end() {
        assert_eq!(tail("abcdef", 4), "cdef");
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(
            tail("ab\u{4e2d}", 3),
            "b\u{4e2d}",
            "a wide character is two columns"
        );
    }

    #[test]
    fn plain_ascii_each_takes_one_column() {
        assert_eq!(grapheme_col_widths(&['a', 'b', 'c']), [1, 1, 1]);
    }

    #[test]
    fn a_variation_selector_makes_its_base_character_wide_not_zero() {
        // A plain heart is narrow; the same heart followed by U+FE0F (request emoji presentation) is drawn wide by
        // any terminal that honours it. Measuring the two characters separately and summing (1 + 0) undercounts it.
        let heart_and_selector = ['\u{2764}', '\u{FE0F}'];
        assert_eq!(grapheme_col_widths(&heart_and_selector), [2, 0]);
    }

    #[test]
    fn a_zero_width_joiner_sequence_is_one_wide_cluster() {
        // Man + ZWJ + laptop: one rendered glyph, conventionally drawn as a single wide cell by terminals that
        // support it, not the sum of each code point's own width.
        let family = ['\u{1F468}', '\u{200D}', '\u{1F4BB}'];
        let widths = grapheme_col_widths(&family);
        assert_eq!(widths[0], "\u{1F468}\u{200D}\u{1F4BB}".width());
        assert_eq!(&widths[1..], [0, 0]);
    }

    #[test]
    fn each_drawn_row_knows_what_it_shows_so_a_move_can_go_by_rows_of_the_screen() {
        let mut ed = Editor::with_keymap(Keymap::Standard);
        // One line of 25 characters wrapped at 10 columns, then a short line, then an empty one.
        ed.set_text("abcdefghijklmnopqrstuvwxy\nzz\n");
        let mut cache = (u64::MAX, Vec::new());
        let rows = build_editor_rows(&ed, &mut cache, 10);
        assert_eq!(rows.version, ed.version());
        let shape: Vec<(usize, usize, usize, bool)> = rows
            .spans
            .iter()
            .map(|s| (s.line, s.first, s.cols.len(), s.ends_line))
            .collect();
        assert_eq!(
            shape,
            [
                (0, 0, 10, false),
                (0, 10, 10, false),
                (0, 20, 5, true),
                (1, 0, 2, true),
                (2, 0, 0, true)
            ]
        );
        assert_eq!(rows.spans.len(), rows.lines.len());
        // The column of a character is where it is drawn in its row; into the middle of a row, the same column of
        // another row is the same distance in.
        assert_eq!(rows.char_at(0, 4), Some((0, 4)));
        assert_eq!(rows.char_at(1, 4), Some((0, 14)));
        assert_eq!(rows.char_at(2, 4), Some((0, 24)));
        // Past the end of a row that goes on, the last character of it; past the end of a line, after its last
        // character (the cursor can stand there); an empty line has only its start.
        assert_eq!(rows.char_at(1, 40), Some((0, 19)));
        assert_eq!(rows.char_at(2, 40), Some((0, 25)));
        assert_eq!(rows.char_at(3, 40), Some((1, 2)));
        assert_eq!(rows.char_at(4, 7), Some((2, 0)));
        assert_eq!(rows.char_at(9, 0), None, "no such row");
    }

    #[test]
    fn a_wide_character_is_reached_at_the_column_it_starts_at() {
        let mut ed = Editor::with_keymap(Keymap::Standard);
        ed.set_text("a😀b😀c");
        let mut cache = (u64::MAX, Vec::new());
        let rows = build_editor_rows(&ed, &mut cache, 80);
        // a, then an emoji two columns wide, b, another, c: they start at columns 0, 1, 3, 4, 6.
        assert_eq!(rows.spans[0].cols, [0, 1, 3, 4, 6]);
        assert_eq!(
            rows.char_at(0, 2),
            Some((0, 1)),
            "inside the emoji: the emoji"
        );
        assert_eq!(rows.char_at(0, 3), Some((0, 2)));
        assert_eq!(rows.char_at(0, 5), Some((0, 3)));
    }

    #[test]
    fn a_line_that_exactly_fills_the_row_does_not_leave_an_empty_row_after_it() {
        let mut ed = Editor::with_keymap(Keymap::Standard);
        // 8 columns of text, an 8-column box; the cursor is at the end of the second line.
        ed.set_text("abcdefgh\nxy");
        let mut cache = (u64::MAX, Vec::new());
        let rows = build_editor_rows(&ed, &mut cache, 8);
        assert_eq!(
            rows.lines.len(),
            2,
            "the full line, then the next: {:?}",
            rows.lines
        );
        // With the cursor at the end of the full line it needs a cell of its own, so that one wraps (as a cursor must).
        let mut ed = Editor::with_keymap(Keymap::Standard);
        ed.set_text("abcdefgh");
        let mut cache = (u64::MAX, Vec::new());
        let rows = build_editor_rows(&ed, &mut cache, 8);
        assert_eq!(rows.lines.len(), 2);
        assert_eq!(rows.cursor, (1, 0));
    }

    #[test]
    fn the_cursor_after_a_variation_selector_emoji_lands_two_columns_in_not_one() {
        let mut ed = Editor::with_keymap(Keymap::Standard);
        ed.set_text("\u{2764}\u{FE0F}x");
        // Cursor at the end, after all three characters (the emoji's two code points, then "x").
        let mut cache = (u64::MAX, Vec::new());
        let rows = build_editor_rows(&ed, &mut cache, 80);
        assert_eq!(rows.cursor, (0, 3), "{:?}", rows.cursor);
    }
}
