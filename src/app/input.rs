//! Popup, pane and editor input handling. Global key precedence is in App::on_key.

use super::{
    App, EMOJI_COLUMNS, EMOJI_ROWS, EmojiTarget, Ev, Focus, MAX_EMOJI_QUERY, Overlay, open_image,
    open_url,
};
use crate::cadence;
use crate::clipboard::Via;
use crate::convert::message_markdown_in_full;
use crate::editor::{EditorAction, Keymap, Mode};
use crate::emoji;
use crate::render::ImgState;
use crate::sign_in_modal::{self, Button};
use crate::view::{self, Geometry, ListRow, Tab};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long after the sign-in popup opens by itself its keys and buttons are ignored.
const SIGN_IN_POPUP_GRACE: Duration = Duration::from_millis(700);
use teams_client::{Chat, Contact, Delivery, Msg, same_emoji};

/// Control without Alt: a shortcut. Control and Alt together are how Windows reports AltGr, which is how people on
/// many layouts type `@`, `€`, `{` and the like, so that combination is text, not a shortcut (the editor reads it
/// the same way).
pub(super) fn ctrl_shortcut(mods: KeyModifiers) -> bool {
    mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::ALT)
}

/// Whether a character key with these modifiers types a character where Alt alone is also a shortcut (the panes and
/// the emoji search): neither modifier, or both together (AltGr).
pub(super) fn types_text(mods: KeyModifiers) -> bool {
    mods.contains(KeyModifiers::CONTROL) == mods.contains(KeyModifiers::ALT)
}

impl App {
    /// Paste follows the same input precedence as typing, and is ignored while a popup owns the keyboard.
    pub fn on_paste(&mut self, text: &str) {
        self.last_input = Instant::now();
        // Not a send: the question before a first post is dropped, as it is by any key that does not send.
        self.post_asked = None;
        let first_line = text.lines().next().unwrap_or("");
        if self.overlay == Overlay::Search {
            self.search_paste(first_line);
        } else if self.overlay != Overlay::None {
            // A popup owns the keyboard; it has nowhere to put text.
        } else if let Some(c) = &mut self.cmdline {
            c.push_str(first_line);
        } else if self.filtering {
            self.filter.push_str(first_line);
        } else if self.focus == Focus::Editor {
            self.editor.paste(text);
        }
    }

    /// Shortcuts of the standard keymap that work in every pane, even while typing a message: Ctrl-K finds a chat,
    /// Ctrl-N starts one, Ctrl-P opens the command panel, F5 refreshes. True if the key was one of them.
    pub(super) fn on_standard_global_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('k') if ctrl && !alt => {
                self.focus = Focus::Chats;
                self.filtering = true;
                self.filter.clear();
            }
            KeyCode::Char('n') if ctrl && !alt => self.cmdline = Some("new ".into()),
            KeyCode::Char('p') if ctrl && !alt => self.cmdline = Some(String::new()),
            KeyCode::Char('e') if ctrl && !alt => self.open_emoji_selector(),
            // Ctrl-F searches the open chat from the conversation or the message box; in the chat list it is nothing,
            // but in the teams list it searches the channel that is open.
            KeyCode::Char('f')
                if ctrl
                    && !alt
                    && (self.focus != Focus::Chats
                        || (self.on_teams_tab() && self.current_channel().is_some())) =>
            {
                self.open_search(None)
            }
            KeyCode::F(5) => self.run_command("refresh"),
            _ => return false,
        }
        true
    }

    /// Open the emoji selector for where the user is: in the message box choosing an emoji inserts it, in the
    /// conversation it reacts to the selected message.
    pub fn open_emoji_selector(&mut self) {
        let target = match self.focus {
            Focus::Editor => EmojiTarget::Insert,
            Focus::Messages => {
                let me = self.me_id().to_string();
                let Some(chat_id) = self.current.clone() else {
                    return;
                };
                let Some(m) = self
                    .selected_msg()
                    .filter(|m| !m.system && !m.deleted && m.delivery == Delivery::Sent)
                else {
                    self.set_status("select a sent message to react to", true);
                    return;
                };
                if self.reactions.is_pending(&chat_id, &m.id) {
                    self.set_status(
                        "the last reaction to this message is still being sent",
                        true,
                    );
                    return;
                }
                EmojiTarget::React {
                    chat_id,
                    message_id: m.id.clone(),
                    mine: m
                        .reactions
                        .iter()
                        .filter(|r| r.users.iter().any(|u| u.eq_ignore_ascii_case(&me)))
                        .map(|r| r.emoji.clone())
                        .collect(),
                }
            }
            Focus::Chats => {
                self.set_status("open a conversation to react or to write in", true);
                return;
            }
        };
        // The same list serves both: what can be reacted with can be written too.
        if self
            .reactions
            .start_catalog(matches!(target, EmojiTarget::React { .. }))
        {
            self.spawn_call(|b| b.reaction_emoji(), Ev::ReactionEmoji);
        }
        self.overlay = Overlay::Emoji {
            target,
            query: String::new(),
            sel: 0,
        };
    }

    /// What the selector lists for `target`: to write into a message, what can be reacted with and then the shortcodes
    /// the service has no reaction for; to react, only what the service accepts.
    pub fn emoji_entries(&self, target: &EmojiTarget) -> &[emoji::Entry] {
        match target {
            EmojiTarget::Insert => self.reactions.insert_entries(),
            EmojiTarget::React { .. } => self.reactions.entries(),
        }
    }

    pub(super) fn on_emoji_key(
        &mut self,
        key: KeyEvent,
        target: EmojiTarget,
        mut query: String,
        mut sel: usize,
    ) {
        let entries = self.emoji_entries(&target);
        let hits = emoji::search(entries, &query);
        let last = hits.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => {
                self.overlay = Overlay::None;
                return;
            }
            KeyCode::Enter => {
                if let Some(entry) = hits.get(sel).map(|e| (*e).clone()) {
                    self.overlay = Overlay::None;
                    self.choose_emoji(target, entry);
                    return;
                }
            }
            KeyCode::Left | KeyCode::BackTab => sel = sel.saturating_sub(1),
            KeyCode::Right | KeyCode::Tab => sel = (sel + 1).min(last),
            KeyCode::Up => sel = sel.saturating_sub(EMOJI_COLUMNS),
            KeyCode::Down => sel = (sel + EMOJI_COLUMNS).min(last),
            // The service's list is over a thousand long: a window at a time.
            KeyCode::PageUp => sel = sel.saturating_sub(EMOJI_COLUMNS * EMOJI_ROWS),
            KeyCode::PageDown => sel = (sel + EMOJI_COLUMNS * EMOJI_ROWS).min(last),
            KeyCode::Home => sel = 0,
            KeyCode::End => sel = last,
            KeyCode::Backspace => {
                query.pop();
                sel = 0;
            }
            KeyCode::Char(c)
                if types_text(key.modifiers) && query.chars().count() < MAX_EMOJI_QUERY =>
            {
                query.push(c);
                sel = 0;
            }
            _ => {}
        }
        self.overlay = Overlay::Emoji { target, query, sel };
    }

    pub(super) fn choose_emoji(&mut self, target: EmojiTarget, entry: emoji::Entry) {
        match target {
            EmojiTarget::Insert => self.editor.paste(&entry.emoji),
            EmojiTarget::React {
                chat_id,
                message_id,
                mine,
            } => {
                let on = !mine.iter().any(|m| same_emoji(m, &entry.emoji));
                if !self.reactions.begin(chat_id.clone(), message_id.clone()) {
                    self.set_status(
                        "the last reaction to this message is still being sent",
                        true,
                    );
                    return;
                }
                self.apply_reaction(&chat_id, &message_id, &entry.emoji, on);
                let emoji = entry.emoji;
                let (chat, message, sent) = (chat_id.clone(), message_id.clone(), emoji.clone());
                self.spawn_action(
                    move |b| b.react(&chat, &message, &sent, on),
                    move |result| Ev::Reacted {
                        chat_id,
                        message_id,
                        emoji,
                        on,
                        result,
                    },
                );
            }
        }
    }

    /// Show a reaction at once, before the service has confirmed it (the next poll replaces it with the truth).
    pub(super) fn apply_reaction(
        &mut self,
        chat_id: &str,
        message_id: &str,
        emoji: &str,
        on: bool,
    ) {
        let me = self.me_id().to_string();
        if let Some(st) = self.states.get_mut(chat_id) {
            st.apply_reaction(message_id, emoji, on, &me);
        }
    }

    /// The key after `^W`: move to another pane, as in vim.
    pub(super) fn window_command(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('h' | 'H') => self.pane_left(),
            KeyCode::Char('l' | 'L') => self.pane_right(),
            KeyCode::Char('j' | 'J') if self.focus == Focus::Messages => self.pane_right(),
            KeyCode::Char('k' | 'K') if self.focus == Focus::Editor => self.focus = Focus::Messages,
            _ => {}
        }
    }

    /// Chat list. Leaving the editor drops it back to normal mode so it never comes back stuck in insert.
    pub(super) fn pane_left(&mut self) {
        if self.focus == Focus::Editor {
            self.editor.set_mode(Mode::Normal);
        }
        self.focus = Focus::Chats;
    }

    /// Chats -> transcript -> editor (the right-hand side is the conversation and its compose box).
    pub(super) fn pane_right(&mut self) {
        match self.focus {
            Focus::Chats if self.current.is_some() => self.focus = Focus::Messages,
            Focus::Messages => self.focus = Focus::Editor,
            _ => {}
        }
    }

    /// Keys in the sign-in popup. For a moment after it opens by itself they are ignored: the person may be typing.
    pub(super) fn on_sign_in_key(&mut self, key: KeyEvent, since: Instant) {
        // F6 is never typed by accident; the other keys are, so they wait.
        if since.elapsed() < SIGN_IN_POPUP_GRACE && key.code != KeyCode::F(6) {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter | KeyCode::F(6) => self.press_sign_in_button(Button::SignIn),
            KeyCode::Char('c') => self.press_sign_in_button(Button::Copy),
            KeyCode::Esc | KeyCode::Char('q') if !ctrl => self.press_sign_in_button(Button::Close),
            _ => {}
        }
    }

    pub(super) fn press_sign_in_button(&mut self, button: Button) {
        match button {
            Button::SignIn => self.sign_in_again(),
            Button::Copy => {
                let text = sign_in_modal::details(&self.sign_in);
                self.copy_to_clipboard(&text, Some("sign-in details copied"));
            }
            Button::Close => self.overlay = Overlay::None,
        }
    }

    pub fn on_mouse(&mut self, m: MouseEvent) {
        self.last_input = Instant::now();
        // Not a send: the question before a first post is dropped, as it is by any key that does not send.
        self.post_asked = None;
        self.post_armed = None;
        if let Overlay::SignIn { since } = self.overlay {
            let g = &self.view.geometry;
            let area = Rect::new(0, 0, g.header.width, g.panel.bottom());
            if matches!(m.kind, MouseEventKind::Down(MouseButton::Left))
                && since.elapsed() >= SIGN_IN_POPUP_GRACE
                && let Some(button) =
                    sign_in_modal::plan(area, &self.sign_in).button_at(m.column, m.row)
            {
                self.press_sign_in_button(button);
            }
            return;
        }
        if self.overlay != Overlay::None || self.cmdline.is_some() {
            return;
        }
        let g = &self.view.geometry;
        let inside = |r: Rect| Geometry::contains(r, m.column, m.row);
        let (panel, transcript, list) = (inside(g.panel), inside(g.transcript), inside(g.list));
        let (list_body, editor) = (inside(g.list_body), inside(g.editor));
        let list_head = inside(g.list_head);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if panel && !self.filtering => {
                self.cmdline = Some(String::new())
            }
            MouseEventKind::Down(MouseButton::Left) if list_head => self.click_tab(m.column),
            MouseEventKind::Down(MouseButton::Left) if list_body && self.on_teams_tab() => {
                self.click_team_row(m.row)
            }
            MouseEventKind::Down(MouseButton::Left) if list_body => self.click_chat_list(m.row),
            MouseEventKind::Down(MouseButton::Left) if transcript => self.click_transcript(m.row),
            MouseEventKind::Down(MouseButton::Left) if editor && self.current.is_some() => {
                self.click_editor(m.column, m.row)
            }
            MouseEventKind::ScrollUp if transcript => self.wheel_transcript(-3),
            MouseEventKind::ScrollDown if transcript => self.wheel_transcript(3),
            MouseEventKind::ScrollUp if list => self.wheel_list(-1),
            MouseEventKind::ScrollDown if list => self.wheel_list(1),
            _ => {}
        }
    }

    /// The image drawn as art at screen row `row`, if there is one.
    pub(super) fn image_at(&self, row: u16) -> Option<String> {
        let st = self.state()?;
        let line = st.scroll + row.checked_sub(self.view.geometry.transcript.y)? as usize;
        st.transcript
            .image_rows
            .iter()
            .find(|(rows, _)| rows.contains(&line))
            .map(|(_, url)| url.clone())
    }

    /// Move focus to `pane` the way a click does. Leaving the editor drops it to normal mode (as `pane_left` does), and
    /// clicking into it starts typing, so the vim keymap does not need `i` first.
    pub(super) fn focus_pane(&mut self, pane: Focus) {
        if self.focus == Focus::Editor && pane != Focus::Editor {
            self.editor.set_mode(Mode::Normal);
        }
        if pane == Focus::Editor {
            self.editor.set_mode(Mode::Insert);
        }
        self.focus = pane;
    }

    /// A click in the message box focuses it and puts the text cursor where the click was (the nearest character of the
    /// row clicked; past the end of a row, its end). A click on the border, or before the drawn rows are known to be the
    /// text's current ones, only focuses.
    pub(super) fn click_editor(&mut self, column: u16, row: u16) {
        self.focus_pane(Focus::Editor);
        let area = self.view.geometry.editor;
        // Inside the border, and the text starts one column in from it.
        let (Some(rel_row), Some(rel_col)) =
            (row.checked_sub(area.y + 1), column.checked_sub(area.x + 2))
        else {
            return;
        };
        if rel_row + 1 >= area.height {
            return;
        }
        let rows = &self.view.editor_rows;
        if rows.version != self.editor.version() || rows.spans.is_empty() {
            return;
        }
        let target = (self.view.editor_top + usize::from(rel_row)).min(rows.spans.len() - 1);
        if let Some(place) = rows.char_at(target, usize::from(rel_col)) {
            self.editor.move_to(place, false);
            self.editor_goal = None;
        }
    }

    /// A click on a chat in the list opens it and moves to its messages. Each chat takes two rows.
    pub(super) fn click_chat_list(&mut self, row: u16) {
        let body = self.view.geometry.list_body;
        let Some(rel) = row.checked_sub(body.y) else {
            return;
        };
        let vis = self.visible_chats();
        let pinned = self.pinned_count(&vis);
        // The divider under the favourites is not a chat.
        let ListRow::Chat(pos) = view::list_row(self.view.list_offset, pinned, usize::from(rel))
        else {
            return;
        };
        let Some(&index) = vis.get(pos) else {
            return;
        };
        let id = self.chats.items()[index].id.clone();
        self.filtering = false;
        self.select_chat(&id);
        self.focus_pane(Focus::Messages);
    }

    /// The full destination of the `[n]` link-list entry drawn at screen row `row`, if there is one.
    pub(super) fn link_at(&self, row: u16) -> Option<String> {
        let st = self.state()?;
        let line = st.scroll + row.checked_sub(self.view.geometry.transcript.y)? as usize;
        st.transcript
            .link_rows
            .iter()
            .find(|(rows, _)| rows.contains(&line))
            .map(|(_, url)| url.clone())
    }

    /// A click on a picture drawn as art opens it in the browser, and a click on an entry of a message's link list opens
    /// that link in full; any click on a message also selects it and focuses the conversation. The view does not move:
    /// what was clicked is already on screen.
    pub(super) fn click_transcript(&mut self, row: u16) {
        if let Some(url) = self.image_at(row) {
            self.open_picture(&url);
        } else if let Some(url) = self.link_at(row) {
            self.open_link(&url);
        }
        let Some(line) = row
            .checked_sub(self.view.geometry.transcript.y)
            .map(usize::from)
        else {
            return;
        };
        self.focus_pane(Focus::Messages);
        let Some(st) = self.state_mut() else { return };
        let line = st.scroll + line;
        let Some(i) = st.transcript.starts.iter().rposition(|&s| s <= line) else {
            return;
        };
        if st.transcript.ends.get(i).is_some_and(|&e| line <= e) {
            let last = st.messages().len().saturating_sub(1);
            st.sel = i.min(last);
            st.follow = st.follow && st.sel == last;
        }
    }

    /// Open the loaded picture `url` full size in the browser.
    pub(super) fn open_picture(&mut self, url: &str) {
        if let Some(ImgState::Ready(img)) = self.images.store().map.get(url) {
            let img = Arc::clone(img);
            let opener = Arc::clone(&self.opener);
            let dir = self.image_dir.clone();
            self.spawn_task(move || {
                match dir.and_then(|dir| open_image(opener.as_ref(), &dir, &img)) {
                    Ok(()) => Ev::Notice {
                        text: "opened the image in your browser".into(),
                        error: false,
                    },
                    Err(e) => Ev::Notice {
                        text: format!("cannot open image: {e}"),
                        error: true,
                    },
                }
            });
        }
    }

    pub(super) fn open_link(&mut self, url: &str) {
        let url = url.to_string();
        let opener = Arc::clone(&self.opener);
        self.spawn_task(move || match open_url(opener.as_ref(), &url) {
            Ok(()) => Ev::Notice {
                text: format!("opened {url}"),
                error: false,
            },
            Err(e) => Ev::Notice {
                text: format!("cannot open link: {e}"),
                error: true,
            },
        });
    }

    pub(super) fn on_cmdline_key(&mut self, key: KeyEvent) {
        let Some(buf) = self.cmdline.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.cmdline = None,
            KeyCode::Enter => {
                let cmd = self.cmdline.take().unwrap_or_default();
                self.run_command(cmd.trim());
            }
            KeyCode::Backspace => {
                if buf.pop().is_none() {
                    self.cmdline = None;
                }
            }
            KeyCode::Char('u') if ctrl_shortcut(key.modifiers) => buf.clear(),
            KeyCode::Char('w') if ctrl_shortcut(key.modifiers) => {
                let trimmed = buf.trim_end();
                // `i` is the byte offset of the whitespace char `rfind` matched; step past it by its own
                // length rather than assuming one byte, or a multi-byte space (NBSP, U+3000, ...) leaves
                // `truncate` an off-char-boundary byte count, which panics.
                let kept = trimmed.rfind(char::is_whitespace).map_or(0, |i| {
                    i + trimmed[i..].chars().next().map_or(1, char::len_utf8)
                });
                buf.truncate(kept);
            }
            KeyCode::Char(c) if !ctrl_shortcut(key.modifiers) => buf.push(c),
            _ => {}
        }
    }

    /// Search the directory for `query` (name or email) and offer the matches.
    pub(super) fn find_people(&mut self, query: &str) {
        let query = query.trim().to_string();
        if query.is_empty() {
            self.set_status("usage: :new <name or email>", true);
            return;
        }
        self.lookups += 1;
        self.set_status(format!("searching for '{query}'…"), false);
        let asked = query.clone();
        self.spawn_action(
            move |b| b.find_people(&asked),
            move |result| Ev::People { query, result },
        );
    }

    pub(super) fn open_contact(&mut self, who: Contact) {
        self.lookups += 1;
        self.set_status(format!("opening a chat with {}…", who.name), false);
        self.spawn_action(move |b| b.open_chat(&who), Ev::Opened);
    }

    /// Put a chat in front of the user and start composing in it. Chats the server doesn't list yet are remembered.
    pub(super) fn show_chat(&mut self, chat: Chat) {
        let id = chat.id.clone();
        self.chats.remember(chat);
        // Make sure the list is not hiding it, and that typing is not still going to a filter nobody can see.
        self.filter.clear();
        self.filtering = false;
        self.unread_only = false;
        // `select_chat` only cancels an edit when it actually switches chats; this can reopen the chat already open
        // (`:new` to someone already on screen), which must still clear a stale edit rather than let the box look
        // like a fresh compose field while still holding another message's text and `self.editing`.
        self.cancel_edit(false);
        self.cancel_reply();
        self.cancel_subject();
        self.select_chat(&id);
        self.focus = Focus::Editor;
        self.editor.set_mode(Mode::Insert);
        self.status = None;
    }

    pub(super) fn on_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.filtering = false;
            }
            KeyCode::Enter => self.filtering = false,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Down | KeyCode::Tab => self.move_list(1),
            KeyCode::Up | KeyCode::BackTab => self.move_list(-1),
            KeyCode::Char(c) if !ctrl_shortcut(key.modifiers) => self.filter.push(c),
            _ => {}
        }
        self.keep_selection_visible();
    }

    /// Move one row through whichever list the left pane shows (the wheel and the arrows of a filter).
    pub(super) fn move_list(&mut self, delta: isize) {
        if self.on_teams_tab() {
            self.teams_move(delta);
        } else {
            self.move_chat(delta);
        }
    }

    /// After the filter changes, keep the selection on something that matches.
    pub(super) fn keep_selection_visible(&mut self) {
        if self.on_teams_tab() {
            self.teams_keep_cursor();
            return;
        }
        let vis = self.visible_chats();
        if !vis
            .iter()
            .any(|&i| Some(&self.chats.items()[i].id) == self.current.as_ref())
        {
            self.goto_chat(false);
        }
    }

    pub fn run_command(&mut self, cmd: &str) {
        let cmd = cmd.trim().trim_start_matches(':').trim_start();
        let (name, arg) = cmd
            .split_once(char::is_whitespace)
            .map_or((cmd, ""), |(n, a)| (n, a.trim()));
        match name {
            "q" | "quit" | "q!" | "qa" | "wq" | "x" => self.quit = true,
            "w" | "send" => self.send(),
            "signin" | "login" => self.sign_in_again(),
            "status" => self.run_status_command(arg),
            "subject" => self.run_subject_command(arg),
            "chats" => self.show_tab(Tab::Chats),
            "teams" => self.show_tab(Tab::Teams),
            "r" | "refresh" | "e" if !self.sign_in.is_online() => {
                self.set_status("the sign-in has expired: press F6 to sign in again", true);
            }
            "r" | "refresh" | "e" => {
                // Asking again does not make a service that said "wait" answer sooner; it makes it say so again.
                let wait = [
                    Some(&self.chats.sched),
                    self.current
                        .as_ref()
                        .and_then(|id| self.states.get(id))
                        .map(|st| &st.sched),
                    self.on_teams_tab().then_some(&self.teams.sched),
                ]
                .into_iter()
                .flatten()
                .filter_map(cadence::Schedule::held_for)
                .max();
                if let Some(left) = wait {
                    self.set_status(
                        format!(
                            "the service asked us to wait; vtteams refreshes by itself in {} s",
                            left.as_secs() + 1
                        ),
                        true,
                    );
                } else {
                    // Asking to refresh also means "try the pictures that failed again".
                    self.images.retry_failed();
                    self.refresh_chats();
                    if self.on_teams_tab() {
                        self.refresh_teams(true);
                    }
                    if let Some(id) = self.current.clone() {
                        self.load_messages(&id, false);
                    }
                    self.set_status("refreshing…", false);
                }
            }
            "images" | "img" => self.images_command(arg),
            "new" | "chat" | "dm" => self.find_people(arg),
            "search" | "find" => self.open_search(Some(arg)),
            "unread" if self.on_teams_tab() => self.set_status(
                "the unread filter is for chats; switch to the CHATS tab",
                true,
            ),
            "unread" => self.unread_only = !self.unread_only,
            "open" | "web" => self.open_current_in_web(),
            "help" | "h" | "?" => self.overlay = Overlay::Help(0),
            "" => {}
            other => self.set_status(format!("not a command: {other}"), true),
        }
    }

    pub(super) fn open_current_in_web(&mut self) {
        if self.current_channel().is_some() {
            self.set_status("channels have no web link here yet", true);
        } else if let Some(url) = self.current_chat().and_then(|c| c.web_url.clone()) {
            self.open_link(&url);
        } else {
            self.set_status("this chat has no web link", true);
        }
    }

    pub(super) fn on_chats_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let was_g = std::mem::take(&mut self.pending_g);
        // `gt` and `gT` switch between the tabs (when there are tabs: otherwise they are nothing).
        if was_g && !ctrl && matches!(key.code, KeyCode::Char('t' | 'T')) && self.teams_available()
        {
            self.switch_tab();
            return;
        }
        if self.on_teams_tab() {
            self.on_teams_key(key, was_g);
            return;
        }
        let half = (self.view.list_height() / 4).max(1) as isize;
        match key.code {
            KeyCode::Char('j') | KeyCode::Down if !ctrl => self.move_chat(1),
            KeyCode::Char('k') | KeyCode::Up if !ctrl => self.move_chat(-1),
            KeyCode::Char('n') if ctrl => self.move_chat(1),
            KeyCode::Char('p') if ctrl => self.move_chat(-1),
            KeyCode::Char('d') if ctrl => self.move_chat(half),
            KeyCode::Char('u') if ctrl => self.move_chat(-half),
            KeyCode::Char('g') if !ctrl => {
                if was_g {
                    self.goto_chat(false);
                } else {
                    self.pending_g = true;
                }
            }
            KeyCode::Char('G') => self.goto_chat(true),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => {
                if self.current.is_some() {
                    self.focus = Focus::Messages;
                }
            }
            KeyCode::Char('i') | KeyCode::Char('a') => {
                if self.current.is_some() {
                    self.focus = Focus::Editor;
                    self.editor.set_mode(Mode::Insert);
                }
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                self.filter.clear();
            }
            KeyCode::Esc if !self.filter.is_empty() => self.filter.clear(),
            KeyCode::Char('U') => self.unread_only = !self.unread_only,
            KeyCode::Char('f') => self.toggle_favorite(),
            KeyCode::Char('F') => self.toggle_pinned(),
            KeyCode::Char('n') if !ctrl => self.cmdline = Some("new ".into()),
            KeyCode::Char('r') => self.run_command("refresh"),
            KeyCode::Char('o') => self.open_current_in_web(),
            KeyCode::Char('I') => self.cycle_images(),
            KeyCode::Char(':') => self.cmdline = Some(String::new()),
            KeyCode::Char('?') => self.overlay = Overlay::Help(0),
            KeyCode::Char('q') => self.quit = true,
            _ => {}
        }
    }

    /// Chat list in the standard keymap: arrows and paging to move, Enter to open, and typing starts a filter, so no
    /// letter does anything surprising.
    pub(super) fn on_chats_key_standard(&mut self, key: KeyEvent) {
        if self.on_teams_tab() {
            self.on_teams_key_standard(key);
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Two rows a chat (`view::list_row`), so a screen of rows is only half that many chats; the vim keymap's
        // Ctrl-D/Ctrl-U halve this again for a half-page.
        let page = (self.view.list_height() / 2).max(1) as isize;
        match key.code {
            KeyCode::Down => self.move_chat(1),
            KeyCode::Up => self.move_chat(-1),
            KeyCode::PageDown => self.move_chat(page),
            KeyCode::PageUp => self.move_chat(-page),
            KeyCode::Home => self.goto_chat(false),
            KeyCode::End => self.goto_chat(true),
            KeyCode::Enter | KeyCode::Right | KeyCode::Tab => {
                if self.current.is_some() {
                    self.focus = Focus::Messages;
                }
            }
            KeyCode::Esc if !self.filter.is_empty() => self.filter.clear(),
            KeyCode::Char('u') if ctrl && !alt => self.unread_only = !self.unread_only,
            KeyCode::Char(c) if types_text(key.modifiers) => {
                self.filtering = true;
                self.filter.clear();
                self.filter.push(c);
                self.keep_selection_visible();
            }
            _ => {}
        }
    }

    /// Conversation in the standard keymap: arrows pick a message, Enter or typing writes one, Ctrl-C copies the
    /// message, Ctrl-R quotes it, Ctrl-O opens its links.
    pub(super) fn on_messages_key_standard(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let h = self.view.transcript_height().max(2);
        match key.code {
            KeyCode::Down => self.move_msg(1),
            KeyCode::Up => self.move_msg(-1),
            KeyCode::PageDown => self.scroll_by(h as isize - 1),
            KeyCode::PageUp => self.scroll_by(-(h as isize - 1)),
            KeyCode::End => self.goto_msg(true),
            KeyCode::Home => self.goto_msg(false),
            KeyCode::Enter | KeyCode::Tab | KeyCode::Right => self.focus = Focus::Editor,
            KeyCode::Esc | KeyCode::Left | KeyCode::Backspace | KeyCode::BackTab => {
                self.focus = Focus::Chats
            }
            KeyCode::Char('c') if ctrl && !alt => self.yank_selected(),
            KeyCode::Char('r') if ctrl && !alt => self.reply_to_selected(),
            KeyCode::Char('o') if ctrl && !alt => self.open_links_of_selected(),
            KeyCode::Char('l') if ctrl && !alt => self.load_unverified_images(),
            // The next and previous hit of the last search.
            KeyCode::F(3) => self.step_hit(!key.modifiers.contains(KeyModifiers::SHIFT)),
            // Typing anywhere in the conversation starts a message, as in a chat window.
            KeyCode::Char(_) if types_text(key.modifiers) => {
                self.focus = Focus::Editor;
                self.on_editor_key(key);
            }
            _ => {}
        }
    }

    pub(super) fn on_messages_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let was_g = std::mem::take(&mut self.pending_g);
        let h = self.view.transcript_height().max(2);
        match key.code {
            KeyCode::Char('j') | KeyCode::Down if !ctrl => self.move_msg(1),
            KeyCode::Char('k') | KeyCode::Up if !ctrl => self.move_msg(-1),
            KeyCode::Char('d') if ctrl => self.scroll_by((h / 2) as isize),
            KeyCode::Char('u') if ctrl => self.scroll_by(-((h / 2) as isize)),
            KeyCode::Char('f') if ctrl => self.scroll_by(h as isize - 1),
            KeyCode::Char('b') if ctrl => self.scroll_by(-(h as isize - 1)),
            KeyCode::PageDown => self.scroll_by(h as isize - 1),
            KeyCode::PageUp => self.scroll_by(-(h as isize - 1)),
            KeyCode::Char('e') if ctrl => self.scroll_lines(1),
            KeyCode::Char('y') if ctrl => self.scroll_lines(-1),
            KeyCode::Char('g') if !ctrl => {
                if was_g {
                    self.goto_msg(false);
                } else {
                    self.pending_g = true;
                }
            }
            KeyCode::Char('G') | KeyCode::End => self.goto_msg(true),
            KeyCode::Home => self.goto_msg(false),
            KeyCode::Char('i') | KeyCode::Char('a') | KeyCode::Enter => {
                self.focus = Focus::Editor;
                self.editor.set_mode(Mode::Insert);
            }
            KeyCode::Char('r') => self.reply_to_selected(),
            KeyCode::Char('e') if !ctrl => self.edit_selected(),
            KeyCode::Char('+') => self.open_emoji_selector(),
            KeyCode::Char('/') => self.open_search(None),
            KeyCode::Char('n') if !ctrl => self.step_hit(true),
            KeyCode::Char('N') => self.step_hit(false),
            KeyCode::F(3) => self.step_hit(!key.modifiers.contains(KeyModifiers::SHIFT)),
            KeyCode::Char('f') => self.toggle_favorite(),
            KeyCode::Char('F') => self.toggle_pinned(),
            KeyCode::Char('y') => self.yank_selected(),
            KeyCode::Char('o') => self.open_links_of_selected(),
            KeyCode::Char('L') => self.load_unverified_images(),
            KeyCode::Char('O') => self.open_current_in_web(),
            KeyCode::Char('I') => self.cycle_images(),
            KeyCode::Char('R') => self.run_command("refresh"),
            KeyCode::Char('h')
            | KeyCode::Left
            | KeyCode::Esc
            | KeyCode::Backspace
            | KeyCode::BackTab => self.focus = Focus::Chats,
            KeyCode::Tab | KeyCode::Right => self.focus = Focus::Editor,
            KeyCode::Char(':') => self.cmdline = Some(String::new()),
            KeyCode::Char('?') => self.overlay = Overlay::Help(0),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('w') if ctrl => self.ctrl_w = true,
            _ => {}
        }
    }

    pub(super) fn move_msg(&mut self, delta: isize) {
        let Some(id) = self.current.clone() else {
            return;
        };
        let Some(st) = self.states.get_mut(&id) else {
            return;
        };
        if st.messages().is_empty() {
            return;
        }
        let last = st.messages().len() - 1;
        let next = (st.sel as isize + delta).clamp(0, last as isize) as usize;
        st.sel = next;
        st.follow = next == last && delta > 0 || (st.follow && next == last);
        st.reveal = true;
        let at_top = next == 0 && delta < 0;
        if at_top {
            self.load_messages(&id, true);
        }
    }

    pub(super) fn goto_msg(&mut self, last: bool) {
        let Some(id) = self.current.clone() else {
            return;
        };
        let Some(st) = self.states.get_mut(&id) else {
            return;
        };
        if st.messages().is_empty() {
            return;
        }
        if last {
            st.sel = st.messages().len() - 1;
            st.follow = true;
        } else {
            st.sel = 0;
            st.follow = false;
        }
        st.reveal = true;
        if !last {
            self.load_messages(&id, true);
        }
    }

    /// Scroll the viewport by lines and move the selection to the message nearest the middle of the screen.
    pub(super) fn scroll_by(&mut self, delta: isize) {
        let h = self.view.transcript_height();
        let Some(st) = self.state_mut() else { return };
        let max = st.transcript.lines.len().saturating_sub(h);
        st.scroll = (st.scroll as isize + delta).clamp(0, max as isize) as usize;
        st.follow = st.scroll == max;
        let mid = st.scroll + h / 2;
        if let Some(i) = st.transcript.starts.iter().rposition(|&s| s <= mid) {
            st.sel = i;
        }
        if st.follow {
            st.sel = st.messages().len().saturating_sub(1);
        }
        let at_top = st.scroll == 0;
        let id = self.current.clone();
        if at_top && let Some(id) = id {
            self.load_messages(&id, true);
        }
    }

    /// The wheel (or a two-finger slide on a trackpad) over the conversation: the view moves, the selected message stays
    /// what it was. At the bottom it follows new messages again; at the top older ones are loaded.
    pub(super) fn wheel_transcript(&mut self, delta: isize) {
        let h = self.view.transcript_height();
        let Some(st) = self.state_mut() else { return };
        let max = st.transcript.lines.len().saturating_sub(h);
        st.scroll = (st.scroll as isize + delta).clamp(0, max as isize) as usize;
        st.follow = st.scroll == max;
        let at_top = st.scroll == 0;
        let id = self.current.clone();
        if at_top && let Some(id) = id {
            self.load_messages(&id, true);
        }
    }

    /// The wheel over the chat or teams list: the list moves, the selection (and so the open chat) does not, and it stays
    /// where it was scrolled to until the selection moves (`ui::prepare`).
    pub(super) fn wheel_list(&mut self, delta: isize) {
        if self.on_teams_tab() {
            let rows = self.team_rows();
            let at = self.team_cursor_index(&rows);
            self.view.team_wheeled = Some(at);
            let last = rows.len().saturating_sub(1) as isize;
            self.view.team_offset =
                (self.view.team_offset as isize + delta * 2).clamp(0, last) as usize;
        } else {
            let last = self.visible_chats().len().saturating_sub(1) as isize;
            self.view.list_wheeled = Some(self.current.clone());
            self.view.list_offset =
                (self.view.list_offset as isize + delta).clamp(0, last) as usize;
        }
    }

    pub(super) fn scroll_lines(&mut self, delta: isize) {
        let h = self.view.transcript_height();
        let Some(st) = self.state_mut() else { return };
        let max = st.transcript.lines.len().saturating_sub(h);
        st.scroll = (st.scroll as isize + delta).clamp(0, max as isize) as usize;
        st.follow = false;
    }

    pub(super) fn selected_msg(&self) -> Option<&Msg> {
        let st = self.state()?;
        st.messages().get(st.sel)
    }

    pub(super) fn yank_selected(&mut self) {
        let Some(m) = self.selected_msg() else { return };
        // The whole message, not the shortened one that is drawn.
        let md = crate::inline_style::strip(&message_markdown_in_full(m, self.me_id()));
        self.copy_to_clipboard(&md, Some("message copied as Markdown"));
    }

    /// Put `text` on the clipboard and remember it for pasting inside vtteams. `announce` is what to say on success; a
    /// failure is always said. A copy through the terminal cannot be confirmed, so it is described as sent.
    pub(super) fn copy_to_clipboard(&mut self, text: &str, announce: Option<&str>) {
        self.last_yank = Some(text.to_string());
        match self.clipboard.set(text) {
            Ok(via) => {
                let Some(what) = announce else { return };
                let message = match via {
                    Via::System => what.to_string(),
                    Via::Terminal => format!("{what} (sent to the terminal's clipboard)"),
                };
                self.set_status(message, false);
            }
            Err(e) => self.set_status(format!("clipboard unavailable: {e}"), true),
        }
    }

    pub(super) fn open_links_of_selected(&mut self) {
        let Some(st) = self.state() else { return };
        let links = st.transcript.links.get(st.sel).cloned().unwrap_or_default();
        // A message with a picture and nothing to follow opens the picture, the way clicking it does.
        let picture = match (
            st.transcript.starts.get(st.sel),
            st.transcript.ends.get(st.sel),
        ) {
            (Some(&first), Some(&last)) => st
                .transcript
                .image_rows
                .iter()
                .find(|(rows, _)| rows.start >= first && rows.start <= last)
                .map(|(_, url)| url.clone()),
            _ => None,
        };
        match links.len() {
            0 if picture.is_some() => {
                if let Some(url) = picture {
                    self.open_picture(&url);
                }
            }
            0 => self.set_status("no links in this message", false),
            1 => self.open_link(&links[0]),
            _ => self.overlay = Overlay::Links(links.into_iter().take(9).collect()),
        }
    }

    pub(super) fn load_unverified_images(&mut self) {
        let Some(st) = self.state() else { return };
        let (Some(&first), Some(&last)) = (
            st.transcript.starts.get(st.sel),
            st.transcript.ends.get(st.sel),
        ) else {
            return;
        };
        let unverified: Vec<String> = st
            .transcript
            .image_rows
            .iter()
            .filter(|(rows, _)| rows.start >= first && rows.start <= last)
            .filter(|(_, url)| {
                matches!(
                    self.images.store().map.get(url),
                    Some(ImgState::Unverified(_))
                )
            })
            .map(|(_, url)| url.clone())
            .collect();
        if unverified.is_empty() {
            self.set_status("no unverified images in this message", false);
            return;
        }
        let count = unverified.len();
        for url in unverified {
            self.explicit_loads.insert(url);
        }
        let msg = if count == 1 {
            "loading 1 image".to_string()
        } else {
            format!("loading {count} images")
        };
        self.set_status(msg, false);
        // The images will be re-queued on the next layout_transcript call.
    }

    /// Whether `key` moves up or down in the message box by a row of the screen, and how: `(-1 or 1, extend the
    /// selection)`. The arrows, with Shift to select (standard keymap); in vim, the arrows in insert mode and in normal
    /// mode with nothing pending, and `j`/`k` in normal mode with nothing pending (anything else, an operator waiting
    /// for its motion or a visual selection, keeps the editor's own lines).
    fn row_step(&self, key: &KeyEvent, vim: bool, normal_idle: bool) -> Option<(isize, bool)> {
        let mods = key.modifiers;
        if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
            return None;
        }
        let shift = mods.contains(KeyModifiers::SHIFT);
        let allowed = !vim || normal_idle || self.editor.mode() == Mode::Insert;
        match key.code {
            KeyCode::Up if allowed => Some((-1, shift && !vim)),
            KeyCode::Down if allowed => Some((1, shift && !vim)),
            KeyCode::Char('k') if vim && normal_idle && !shift => Some((-1, false)),
            KeyCode::Char('j') if vim && normal_idle && !shift => Some((1, false)),
            _ => None,
        }
    }

    /// Move the cursor in the message box `delta` rows of the screen up (`-1`) or down (`1`), keeping to the column it
    /// began at however short the rows in between are. False when there is nowhere to go that way (the first or last
    /// row, where the editor goes to the start or end of the text) or the drawn rows are not the text's current ones
    /// (typing came in before a frame was drawn): the editor then moves by lines as it always has.
    pub(super) fn move_editor_by_rows(&mut self, delta: isize, extend: bool) -> bool {
        let rows = &self.view.editor_rows;
        if rows.version != self.editor.version() || rows.spans.is_empty() {
            return false;
        }
        let (row, col) = rows.cursor;
        let Some(target) = row
            .checked_add_signed(delta)
            .filter(|t| *t < rows.spans.len())
        else {
            return false;
        };
        let here = self.editor.cursor();
        let goal = match self.editor_goal {
            Some((x, at)) if at == here => x,
            _ => col,
        };
        let Some(place) = rows.char_at(target, goal) else {
            return false;
        };
        self.editor.move_to(place, extend);
        self.editor_goal = Some((goal, self.editor.cursor()));
        true
    }

    pub(super) fn on_editor_key(&mut self, key: KeyEvent) {
        // A list of people offered for the `@name` being typed takes its own keys first.
        if self.on_mention_key(&key) {
            return;
        }
        self.on_editor_key_unfiltered(key);
        self.settle_mention();
    }

    fn on_editor_key_unfiltered(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let vim = self.keymap == Keymap::Vim;
        let normal_idle = vim
            && self.editor.mode() == Mode::Normal
            && self.editor.pending().is_empty()
            && self.editor.command_line().is_none();

        if normal_idle {
            match key.code {
                KeyCode::Tab => {
                    self.focus = Focus::Chats;
                    return;
                }
                KeyCode::Char('w') if ctrl => {
                    self.ctrl_w = true;
                    return;
                }
                KeyCode::Char('?') => {
                    self.overlay = Overlay::Help(0);
                    return;
                }
                KeyCode::Char('p') | KeyCode::Char('P') if !ctrl => {
                    self.sync_register_from_clipboard()
                }
                _ => {}
            }
        }
        // An empty message box has nothing for Left to move over, so in the standard keymap it goes back to the
        // conversation, as it does from the conversation to the chat list.
        if !vim
            && key.code == KeyCode::Left
            && key.modifiers == KeyModifiers::NONE
            && self.editor.lines().iter().all(String::is_empty)
        {
            self.focus = Focus::Messages;
            return;
        }
        // Up in an empty message box edits the last message you sent, as in Teams and Slack.
        if !vim
            && key.code == KeyCode::Up
            && key.modifiers == KeyModifiers::NONE
            && self.editing.is_none()
            && self.editor.lines().iter().all(String::is_empty)
        {
            self.edit_last_own();
            return;
        }
        // Up and Down go by rows of the screen, so a long line that wraps can be walked through; the editor only knows
        // lines of the text.
        if let Some((rows, extend)) = self.row_step(&key, vim, normal_idle)
            && self.move_editor_by_rows(rows, extend)
        {
            return;
        }
        // In vim, Ctrl-C leaves insert/visual mode like Esc (it must never kill the app mid-sentence); in the
        // standard keymap it copies.
        let key = if vim && ctrl && key.code == KeyCode::Char('c') {
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        } else {
            key
        };
        match self.editor.handle_key(key) {
            EditorAction::None => {}
            EditorAction::Send => self.send(),
            EditorAction::Escape => {
                if self.editing.is_some() {
                    self.cancel_edit(true);
                } else {
                    self.cancel_reply();
                    // The title is a draft like the text: it stays while there is text, and goes with the last of it.
                    if self.editor.is_empty() {
                        self.cancel_subject();
                    }
                    self.focus = Focus::Messages;
                }
            }
            EditorAction::Yanked(text) => {
                if !text.is_empty() {
                    // Ctrl-C and Ctrl-X are deliberate copies and say so; vim's deletes copy too, and stay quiet.
                    let said = (self.keymap == Keymap::Standard)
                        .then(|| format!("{} characters copied", text.chars().count()));
                    self.copy_to_clipboard(&text, said.as_deref());
                }
            }
            EditorAction::Command(cmd) => self.run_command(cmd.trim()),
            EditorAction::Paste => match self.clipboard.get().or_else(|| self.last_yank.clone()) {
                Some(text) if !text.is_empty() => self.editor.paste(&text),
                _ => self.set_status("the clipboard is empty", true),
            },
        }
    }

    /// If something else was copied to the system clipboard since our last yank, make `p` paste that.
    pub(super) fn sync_register_from_clipboard(&mut self) {
        let Some(text) = self.clipboard.get() else {
            return;
        };
        if text.is_empty() || self.last_yank.as_deref() == Some(text.as_str()) {
            return;
        }
        let linewise = text.ends_with('\n');
        self.editor
            .set_register(text.trim_end_matches('\n').to_string(), linewise);
    }
}
