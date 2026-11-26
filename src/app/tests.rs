use super::chat::{AUTOFILL_MAX_PAGES, AUTOFILL_MIN_MESSAGES, merge_messages};
use super::*;
use crate::demo::DemoBackend;
use crate::editor::Mode;
use crate::emoji;
use crate::render::ImageStore;
use ratatui::crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::collections::HashSet;
use teams_client::Reaction;

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

/// Tick until `until` holds. Background work runs on the calling thread in tests, so a handful of ticks is enough
/// and nothing waits for the clock.
fn pump(app: &mut App, until: impl Fn(&App) -> bool) {
    for _ in 0..50 {
        app.on_tick();
        if until(app) {
            return;
        }
    }
    panic!("condition not reached");
}

fn app() -> App {
    app_with(Arc::new(DemoBackend::instant()))
}

/// An app together with its stand-in clipboard and opener, which the test can read and fill.
fn app_and_fakes() -> (App, Fakes) {
    let (mut app, fakes) = App::for_test(Arc::new(DemoBackend::instant()));
    // Most of these tests exercise the vim keys; the standard keymap has its own (`standard_app`).
    app.set_keymap(Keymap::Vim);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    (app, fakes)
}

fn msg(id: &str, mins: i64) -> Msg {
    let t = Utc::now() - chrono::Duration::minutes(mins);
    Msg {
        root_id: None,
        id: id.into(),
        created: t,
        modified: t,
        edited: None,
        from_id: Some("u".into()),
        from_name: "U".into(),
        system: false,
        deleted: false,
        html: true,
        body: id.into(),
        subject: None,
        attachments: vec![],
        reactions: vec![],
        mentions: vec![],
        delivery: Delivery::Sent,
    }
}

#[test]
fn merge_upserts_sorts_and_keeps_local_messages_last() {
    let mut v = vec![msg("b", 5), msg("local-1", 0)];
    let fresh = merge_messages(&mut v, vec![msg("a", 10), msg("b", 5), msg("c", 1)], "me").fresh;
    assert_eq!(fresh, 2);
    assert_eq!(
        v.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["a", "b", "c", "local-1"]
    );
}

#[test]
fn loads_chats_and_selects_the_newest() {
    let app = app();
    assert!(!app.chats.items().is_empty());
    assert_eq!(
        app.current.as_deref(),
        Some(app.chats.items()[0].id.as_str())
    );
}

#[test]
fn navigation_moves_between_chats_and_panes() {
    let mut app = app();
    let first = app.current.clone();
    app.on_key(key('j'));
    assert_ne!(app.current, first);
    app.on_key(key('k'));
    assert_eq!(app.current, first);
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::Messages);
    app.on_key(key('h'));
    assert_eq!(app.focus, Focus::Chats);
}

#[test]
fn images_drawn_as_art_are_click_targets() {
    let mut app = app();
    let ada = app
        .chats
        .items()
        .iter()
        .find(|c| c.id == "demo-ada")
        .map(|c| c.id.clone())
        .expect("demo chat with an image");
    app.select_chat(&ada);
    // Draw until the demo image has arrived and been laid out.
    for _ in 0..50 {
        crate::ui::render_to_string(&mut app, 120, 40);
        app.on_tick();
        let arrived = app
            .images
            .store()
            .map
            .values()
            .any(|s| matches!(s, ImgState::Ready(_)));
        if arrived
            && app
                .state()
                .is_some_and(|s| !s.transcript.image_rows.is_empty())
        {
            break;
        }
    }
    crate::ui::render_to_string(&mut app, 120, 40);
    let st = app.state().unwrap();
    let (rows, url) = st.transcript.image_rows[0].clone();
    assert_eq!(url, "https://demo.invalid/plasma/1");
    assert!(rows.len() > 1, "art spans several rows");
    let screen_row = |line: usize| app.view.geometry.transcript.y + (line - st.scroll) as u16;
    assert_eq!(
        app.image_at(screen_row(rows.start)).as_deref(),
        Some(url.as_str())
    );
    assert_eq!(
        app.image_at(screen_row(rows.end - 1)).as_deref(),
        Some(url.as_str())
    );
    // The row above the art belongs to the message text, not the image.
    if rows.start > st.scroll {
        assert_eq!(app.image_at(screen_row(rows.start - 1)), None);
    }
    // Wheel events over the transcript are handled without opening anything.
    let wheel = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: app.view.geometry.transcript.x,
        row: app.view.geometry.transcript.y,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(wheel);
}

fn wheel(app: &mut App, kind: MouseEventKind, area: ratatui::layout::Rect) {
    app.on_mouse(MouseEvent {
        kind,
        column: area.x + 2,
        row: area.y + 1,
        modifiers: KeyModifiers::NONE,
    });
}

#[test]
fn the_wheel_scrolls_the_chat_list_without_opening_another_chat_until_the_selection_moves() {
    let (mut app, _) = app_and_fakes();
    // A short screen, so the list is longer than it.
    crate::ui::render_to_string(&mut app, 120, 14);
    let open = app.current.clone();
    let list = app.view.geometry.list_body;
    let top = app.view.list_offset;
    for _ in 0..3 {
        wheel(&mut app, MouseEventKind::ScrollDown, list);
        crate::ui::render_to_string(&mut app, 120, 14);
    }
    assert_eq!(app.current, open, "no other chat was opened");
    let open_at = app
        .visible_chats()
        .iter()
        .position(|&i| Some(&app.chats.items()[i].id) == open.as_ref())
        .unwrap();
    assert!(
        app.view.list_offset > open_at.max(top),
        "the list scrolled past the open chat, which it would otherwise keep on screen: {top} -> {}, open at {open_at}",
        app.view.list_offset
    );
    let scrolled = app.view.list_offset;
    // Frames go by and polls come in: it stays where it was put.
    for _ in 0..3 {
        app.on_tick();
        crate::ui::render_to_string(&mut app, 120, 14);
    }
    assert_eq!(app.view.list_offset, scrolled);
    wheel(&mut app, MouseEventKind::ScrollUp, list);
    crate::ui::render_to_string(&mut app, 120, 14);
    assert_eq!(app.view.list_offset, scrolled - 1, "and back up");
    // Choosing a chat with the keys shows it again.
    app.focus = Focus::Chats;
    app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    crate::ui::render_to_string(&mut app, 120, 14);
    assert_ne!(app.current, open);
    let vis = app.visible_chats();
    let at = vis
        .iter()
        .position(|&i| Some(&app.chats.items()[i].id) == app.current.as_ref())
        .unwrap();
    assert!(
        at >= app.view.list_offset,
        "the newly selected chat is on screen: {at} vs {}",
        app.view.list_offset
    );
}

#[test]
fn the_wheel_scrolls_the_conversation_and_leaves_the_selected_message_alone() {
    let (mut app, _) = app_and_fakes();
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    crate::ui::render_to_string(&mut app, 120, 20);
    let transcript = app.view.geometry.transcript;
    let (scroll, sel) = {
        let st = app.state().unwrap();
        (st.scroll, st.sel)
    };
    assert!(scroll > 0, "the conversation is longer than the screen");
    // An earlier message is the selected one (the layout itself follows when the last one is).
    app.state_mut().unwrap().sel = sel.saturating_sub(1);
    let sel = sel.saturating_sub(1);
    wheel(&mut app, MouseEventKind::ScrollUp, transcript);
    crate::ui::render_to_string(&mut app, 120, 20);
    let st = app.state().unwrap();
    assert!(st.scroll < scroll, "the view moved up");
    assert_eq!(st.sel, sel, "the selected message did not change");
    assert!(!st.follow);
    // Back to the bottom: following new messages again.
    for _ in 0..20 {
        wheel(&mut app, MouseEventKind::ScrollDown, transcript);
    }
    crate::ui::render_to_string(&mut app, 120, 20);
    let st = app.state().unwrap();
    assert!(st.follow);
    assert_eq!(st.sel, sel);
}

#[test]
fn a_reply_is_joined_to_the_message_it_answers_by_a_line_down_to_the_message_box() {
    let (mut app, _) = app_and_fakes();
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    crate::ui::render_to_string(&mut app, 120, 40);
    // Reply to the message two before the last.
    let target = app.state().unwrap().messages().len() - 3;
    app.state_mut().unwrap().sel = target;
    app.reply_to_selected();
    assert!(app.is_replying());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    let draw = |app: &mut App, terminal: &mut ratatui::Terminal<ratatui::backend::TestBackend>| {
        terminal
            .draw(|f| crate::ui::render(f, app))
            .unwrap()
            .buffer
            .clone()
    };
    let buffer = draw(&mut app, &mut terminal);

    let (t, editor) = (app.view.geometry.transcript, app.view.geometry.editor);
    let (x, right) = (t.x + 1, t.right() - 1);
    let st = app.state().unwrap();
    let first = t.y + (st.transcript.starts[target] - st.scroll) as u16;
    // The row above the message is its top edge.
    let top = first - 1;
    let edge: String = (x..=right)
        .map(|c| buffer[(c, top)].symbol().to_string())
        .collect();
    assert!(
        edge.starts_with('╭')
            && edge.ends_with('╮')
            && edge.chars().filter(|&c| c == '─').count() == edge.chars().count() - 2,
        "a top edge over the message: {edge:?}"
    );
    let column = |side: u16| -> Vec<String> {
        (first..=editor.y)
            .map(|y| buffer[(side, y)].symbol().to_string())
            .collect()
    };
    let left_side = column(x);
    assert_eq!(
        left_side.last().unwrap(),
        "┴",
        "joined to the box's top border: {left_side:?}"
    );
    assert!(
        left_side[..left_side.len() - 1]
            .iter()
            .all(|s| ["│", "┼", "├"].contains(&s.as_str())),
        "the left side goes all the way down: {left_side:?}"
    );
    // The blank row after the message closes the frame under it, and the right side ends there.
    let under = t.y + (st.transcript.ends[target] + 1 - st.scroll) as u16;
    let edge: String = (x..=right)
        .map(|c| buffer[(c, under)].symbol().to_string())
        .collect();
    assert!(
        edge.starts_with('├')
            && edge.ends_with('╯')
            && edge[3..edge.len() - 3].chars().all(|c| c == '─'),
        "an edge under the message: {edge:?}"
    );
    let right_side = column(right);
    let at_under = usize::from(under - first);
    assert!(
        right_side[..at_under].iter().all(|s| s == "│"),
        "the right side runs along the message: {right_side:?}"
    );
    assert!(
        right_side[at_under + 1..]
            .iter()
            .all(|s| !["│", "┼", "┴"].contains(&s.as_str())),
        "and no further: {right_side:?}"
    );
    let border = buffer[(editor.x, editor.y)].fg;
    assert!(
        (top..=editor.y).all(|y| buffer[(x, y)].fg == border)
            && (top..=under).all(|y| buffer[(right, y)].fg == border),
        "in the box's border colour"
    );
    assert!(
        (t.y..top).all(|y| buffer[(x, y)].symbol() == " " || buffer[(x, y)].symbol() == "─"),
        "nothing above the message"
    );
    // Answering the first message, scrolled so that it is above the screen: the line comes in from the top.
    app.cancel_reply();
    app.state_mut().unwrap().sel = 0;
    app.reply_to_selected();
    let past = app.state().unwrap().transcript.ends[0] + 1;
    app.state_mut().unwrap().scroll = past;
    app.state_mut().unwrap().follow = false;
    draw(&mut app, &mut terminal);
    assert!(
        app.state().unwrap().scroll >= past,
        "the first message is off the screen"
    );
    let buffer = draw(&mut app, &mut terminal);
    let top = (t.y..t.bottom())
        .find(|&y| buffer[(x, y)].symbol() != " ")
        .unwrap();
    assert_eq!(buffer[(x, top)].symbol(), "↑");

    // No reply, no line.
    app.cancel_reply();
    let buffer = draw(&mut app, &mut terminal);
    assert!((t.y..=editor.y).all(|y| !["╭", "│", "┴", "↑"].contains(&buffer[(x, y)].symbol())));
}

fn type_command(app: &mut App, text: &str) {
    app.on_key(key(':'));
    for c in text.chars() {
        app.on_key(key(c));
    }
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
}

#[test]
fn new_chat_with_one_match_opens_it_ready_to_compose() {
    let mut app = app();
    let before = app.chats.items().len();
    type_command(&mut app, "new knuth");
    pump(&mut app, |a| a.chats.items().len() > before);
    let chat = app
        .current_chat()
        .expect("the new chat is selected")
        .clone();
    assert_eq!(chat.title(app.me_id()), "Donald Knuth");
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(app.editor.mode(), Mode::Insert);
    // It has no messages yet but loads (as empty) rather than erroring.
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    assert!(app.state().unwrap().messages().is_empty());
}

#[test]
fn the_new_chat_picker_shows_each_person_s_availability_phone_and_office_readably() {
    let mut app = app();
    type_command(&mut app, "new a");
    pump(&mut app, |a| matches!(a.overlay, Overlay::People { .. }));
    let Overlay::People { people, .. } = app.overlay.clone() else {
        unreachable!()
    };
    // The picker's people are asked about: their availability comes in while it is open.
    pump(&mut app, |a| a.presence_of_id(&people[0].id).is_some());
    let screen = crate::ui::render_to_string(&mut app, 140, 50);
    let first = &people[0];
    let line_with = |text: &str| {
        screen
            .lines()
            .find(|l| l.contains(text))
            .unwrap_or_else(|| panic!("{text:?} is not drawn:\n{screen}"))
            .to_string()
    };
    let availability = crate::presence::describe(app.presence_of_id(&first.id).unwrap());
    assert!(line_with(&first.name).contains(&availability), "{screen}");
    assert!(line_with(&first.email).contains(&first.phone), "{screen}");
    assert!(line_with(&first.detail).contains(&first.office), "{screen}");

    // The selected person's details are drawn in a colour that reads on the highlight.
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 50)).unwrap();
    let buffer = terminal
        .draw(|f| crate::ui::render(f, &mut app))
        .unwrap()
        .buffer
        .clone();
    let (x, y) = (0..buffer.area.height)
        .find_map(|y| {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            row.find(&first.email)
                .map(|at| (row[..at].chars().count() as u16, y))
        })
        .expect("the email is drawn");
    let cell = &buffer[(x, y)];
    assert_eq!(
        (cell.fg, cell.bg),
        (crate::theme::TEXT, crate::theme::POPUP_SEL_BG),
        "the selected person's email"
    );
}

#[test]
fn new_chat_with_several_matches_offers_a_picker() {
    let mut app = app();
    type_command(&mut app, "new a");
    pump(&mut app, |a| matches!(a.overlay, Overlay::People { .. }));
    let Overlay::People { people, sel } = app.overlay.clone() else {
        unreachable!()
    };
    assert!(people.len() > 1 && sel == 0);
    app.on_key(key('j'));
    assert!(matches!(app.overlay, Overlay::People { sel: 1, .. }));
    let chosen = match &app.overlay {
        Overlay::People { people, sel } => people[*sel].clone(),
        _ => unreachable!(),
    };
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.overlay, Overlay::None);
    pump(&mut app, |a| {
        a.current_chat()
            .is_some_and(|c| c.members.iter().any(|m| m.id == chosen.id))
    });
}

#[test]
fn the_picker_offers_only_the_nine_it_can_show_and_jump_to() {
    let mut app = app();
    let people: Vec<teams_client::Contact> = (0..10)
        .map(|n| teams_client::Contact {
            id: format!("id{n}"),
            name: format!("Person {n}"),
            email: String::new(),
            detail: String::new(),
            ..Default::default()
        })
        .collect();
    app.on_event(Ev::People {
        query: "p".into(),
        result: Ok(people),
    });
    let Overlay::People { people, .. } = app.overlay.clone() else {
        panic!("a picker opens: {:?}", app.overlay)
    };
    assert_eq!(
        people.len(),
        9,
        "the tenth would be selectable but never drawn"
    );
}

#[test]
fn opening_a_chat_ends_a_filter_that_was_still_being_typed() {
    let mut app = app();
    pump(&mut app, |a| a.chats.loaded());
    app.filtering = true;
    app.filter = "zz".into();
    let chat = app.chats.items()[0].clone();
    app.show_chat(chat);
    assert!(app.filter.is_empty() && !app.filtering);
    // Typing goes to the message box, not a hidden filter.
    type_text(&mut app, "hello");
    assert_eq!(app.editor.text(), "hello");
    assert!(app.filter.is_empty());
}

#[test]
fn new_chat_nobody_found_says_so_and_empty_query_shows_usage() {
    let mut app = app();
    type_command(&mut app, "new zzzz");
    pump(&mut app, |a| {
        a.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.contains("no one found"))
    });
    type_command(&mut app, "new");
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, _)| m.contains("usage"))
    );
}

#[test]
fn a_chat_with_no_messages_survives_a_list_refresh() {
    let mut app = app();
    type_command(&mut app, "new knuth");
    pump(&mut app, |a| {
        a.current_chat()
            .is_some_and(|c| c.title(a.me_id()) == "Donald Knuth")
    });
    let id = app.current.clone().unwrap();
    // A server list that doesn't know the chat (as Teams/Graph wouldn't before the first message).
    app.on_event(Ev::Chats(Ok(app
        .chats
        .items()
        .iter()
        .filter(|c| c.id != id)
        .cloned()
        .collect())));
    assert!(
        app.chats.items().iter().any(|c| c.id == id),
        "pending chat dropped by refresh"
    );
    assert_eq!(app.current.as_deref(), Some(id.as_str()));
}

#[test]
fn a_pending_chat_keeps_its_sent_preview_until_it_becomes_an_ordinary_list_item() {
    let app = loaded_app();
    let mut chats = ChatList::new();
    let mut pending = app.chats.items()[0].clone();
    pending.id = "pending-chat".into();
    chats.remember(pending.clone());
    chats.remember(pending.clone());
    let sent = msg("sent preview", 0);
    chats.record_sent(&pending.id, &sent);
    chats.receive(vec![]);
    assert_eq!(chats.items().len(), 1);
    assert_eq!(
        chats.items()[0].preview.as_ref().unwrap().text,
        "sent preview"
    );
    chats.receive(vec![pending]);
    assert_eq!(
        chats.items().len(),
        1,
        "the service replaces the pending entry"
    );
    chats.receive(vec![]);
    assert!(
        chats.items().is_empty(),
        "an ordinary chat is no longer retained locally"
    );
}

#[test]
fn n_prefills_the_command_panel() {
    let mut app = app();
    app.on_key(key('n'));
    assert_eq!(app.cmdline.as_deref(), Some("new "));
}

#[test]
fn ctrl_w_in_the_command_panel_deletes_a_word_even_across_a_multi_byte_space() {
    let mut app = app();
    // A non-breaking space (U+00A0) is 2 bytes in UTF-8, and `char::is_whitespace` (which both `trim_end` and
    // `rfind` use) treats it as whitespace like any other. Stepping back by a fixed 1 byte, instead of by the
    // matched char's own length, would truncate the buffer on a byte that is not a char boundary and panic.
    app.cmdline = Some("new knuth\u{a0}ada".into());
    ctrl(&mut app, 'w');
    assert_eq!(app.cmdline.as_deref(), Some("new knuth\u{a0}"));
    // The trailing NBSP is itself whitespace, so `trim_end` eats it too and the next word back is "knuth".
    ctrl(&mut app, 'w');
    assert_eq!(app.cmdline.as_deref(), Some("new "));
    ctrl(&mut app, 'w');
    assert_eq!(app.cmdline.as_deref(), Some(""));
    // A three-byte space (U+3000, ideographic space) likewise does not panic.
    app.cmdline = Some("new\u{3000}ada".into());
    ctrl(&mut app, 'w');
    assert_eq!(app.cmdline.as_deref(), Some("new\u{3000}"));
}

/// A backend that fails in scripted ways, to test how the app copes.
struct Misbehaving {
    panic_on_chats: bool,
    /// Sends are refused outright (nothing was delivered) instead of failing in an unknown way.
    refuse_sends: bool,
    message_calls: std::sync::atomic::AtomicUsize,
}

impl Misbehaving {
    fn new(panic_on_chats: bool) -> Arc<Self> {
        Arc::new(Self {
            panic_on_chats,
            refuse_sends: false,
            message_calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn refusing_sends() -> Arc<Self> {
        Arc::new(Self {
            panic_on_chats: false,
            refuse_sends: true,
            message_calls: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

fn scripted_failure() -> Error {
    Error::Unexpected {
        operation: "scripted",
        detail: "this backend always fails",
    }
}

impl Backend for Misbehaving {
    fn label(&self) -> String {
        "test".into()
    }
    fn me(&self) -> Result<Me, Error> {
        Ok(Me {
            id: "me".into(),
            name: "Me".into(),
        })
    }
    fn chats(&self) -> Result<Vec<Chat>, Error> {
        assert!(!self.panic_on_chats, "boom: a bug inside the backend");
        Ok(vec![Chat {
            id: "c1".into(),
            kind: teams_client::ChatKind::Group,
            topic: Some("Test".into()),
            members: vec![],
            updated: Utc::now(),
            preview: None,
            last_read: None,
            web_url: None,
            favorite: None,
        }])
    }
    fn find_people(&self, _: &str) -> Result<Vec<Contact>, Error> {
        Ok(vec![])
    }
    fn open_chat(&self, _: &Contact) -> Result<Chat, Error> {
        Err(scripted_failure())
    }
    fn messages(&self, _: &str, _: Option<&str>) -> Result<Page, Error> {
        self.message_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(scripted_failure())
    }
    fn send(
        &self,
        _: &str,
        _: &str,
        _: &[teams_client::OutMention],
        _: Option<&teams_client::ReplyTarget>,
    ) -> Result<Msg, Error> {
        Err(if self.refuse_sends {
            Error::Status {
                operation: "send message",
                status: 403,
                message: "not allowed".into(),
            }
        } else {
            scripted_failure()
        })
    }
    fn serves_image(&self, _: &str) -> bool {
        false
    }
    fn image(&self, _: &str) -> Result<RgbImage, Error> {
        Err(scripted_failure())
    }
}

fn app_with(backend: Arc<dyn Backend>) -> App {
    let (mut app, _) = App::for_test(backend);
    app.set_keymap(Keymap::Vim);
    app.start();
    pump(&mut app, |a| a.chats.loaded() || a.chats.error().is_some());
    app
}

#[test]
fn outstanding_mentions_counts_unread_mentions_in_loaded_chats_only() {
    let mut app = app();
    assert_eq!(
        app.outstanding_mentions(),
        0,
        "nothing is loaded yet, so nothing can be counted"
    );
    // Not focused, so the chat is not marked read the moment it loads: the mention stays outstanding to check.
    app.set_focused(false);
    app.select_chat("demo-platform");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    assert_eq!(
        app.outstanding_mentions(),
        1,
        "the demo seeds exactly one unread mention of the signed-in user in this chat"
    );
    // Focused again, the next tick marks it read, and the mention is no longer outstanding.
    app.set_focused(true);
    pump(&mut app, |a| a.outstanding_mentions() == 0);
}

#[test]
fn a_panic_in_a_backend_call_becomes_an_error_and_clears_the_in_flight_flag() {
    let (mut app, _) = App::for_test(Misbehaving::new(true));
    app.start();
    pump(&mut app, |a| a.chats.error().is_some());
    assert!(
        !app.chats.is_fetching(),
        "the request is no longer marked as in flight"
    );
    assert!(
        app.chats.error().is_some_and(|e| e.contains("panicked")),
        "{:?}",
        app.chats.error()
    );
    // The app keeps working: the next refresh is allowed to run.
    app.refresh_chats();
    pump(&mut app, |a| !a.chats.is_fetching());
}

#[test]
fn a_failing_message_load_is_not_retried_on_every_tick() {
    let backend = Misbehaving::new(false);
    let mut app = app_with(Arc::clone(&backend) as Arc<dyn Backend>);
    for _ in 0..40 {
        app.on_tick();
    }
    assert_eq!(
        backend
            .message_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one attempt, then back off"
    );
    assert_eq!(app.state().map(|s| s.sched.fails()), Some(1));
    assert!(app.state().is_some_and(|s| !s.loaded && s.error.is_some()));
}

#[test]
fn chat_list_refresh_failures_are_visible_after_the_first_load_and_recovery_resets_them() {
    let mut app = app();
    app.on_event(Ev::Chats(Err(scripted_failure())));
    assert_eq!(app.chats.sched.fails(), 1);
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.contains("cannot refresh chats")),
        "{:?}",
        app.status
    );
    assert!(app.chats.loaded(), "the old list stays on screen");
    app.on_event(Ev::Chats(Ok(app.chats.items().to_vec())));
    assert_eq!(app.chats.sched.fails(), 0);
}

#[test]
fn describe_adds_the_sign_in_hint_only_when_signing_in_again_is_the_cure() {
    let expired = Error::Auth(teams_client::AuthError::NotSignedIn);
    assert!(describe(&expired).contains("vtteams logout"));
    let throttled = Error::Throttled {
        operation: "list chats",
        retry_after: None,
    };
    assert!(!describe(&throttled).contains("logout") && describe(&throttled).contains("throttled"));
    assert!(!describe(&scripted_failure()).contains("logout"));
}

#[test]
fn ctrl_w_is_handled_in_one_place_and_never_left_armed() {
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    let mut app = app();
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::Messages);
    // ^W j from the transcript goes to the editor; ^W h from the editor goes back to the chat list.
    app.on_key(ctrl('w'));
    app.on_key(key('j'));
    assert_eq!(app.focus, Focus::Editor);
    app.on_key(ctrl('w'));
    app.on_key(key('h'));
    assert_eq!(app.focus, Focus::Chats);
    // An unrecognised follow-up cancels it; the next key is an ordinary key.
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.on_key(ctrl('w'));
    app.on_key(key('x'));
    assert!(!app.ctrl_w, "the aborted ^W is not left armed");
    app.on_key(key('i'));
    assert_eq!(
        (app.focus, app.editor.mode()),
        (Focus::Editor, Mode::Insert),
        "the next key is an ordinary key"
    );
    app.on_key(key('z'));
    assert_eq!(
        app.editor.text(),
        "z",
        "and the editor did not swallow its first key"
    );
}

#[test]
fn pasted_text_goes_where_typing_would() {
    let mut app = app();
    app.on_key(key('i'));
    assert_eq!(app.focus, Focus::Editor);
    // The command panel wins over the editor while it is open.
    app.cmdline = Some(String::new());
    app.on_paste("new knuth\nignored second line");
    assert!(app.editor.is_empty());
    assert_eq!(app.cmdline.as_deref(), Some("new knuth"));
    app.cmdline = None;
    // A popup swallows it.
    app.overlay = Overlay::Help(0);
    app.on_paste("nope");
    assert!(app.editor.is_empty());
    app.overlay = Overlay::None;
    app.on_paste("hello");
    assert_eq!(app.editor.text(), "hello");
}

fn image_message(n: usize) -> Msg {
    let mut m = msg(&format!("m{n}"), (100 - n) as i64);
    m.body = format!(r#"<p>picture {n}</p><img src="https://demo.invalid/plasma/{n}" alt="p">"#);
    m
}

#[test]
fn only_images_near_the_viewport_are_queued_for_download() {
    let mut app = app();
    let id = app.current.clone().unwrap();
    let st = app.states.get_mut(&id).unwrap();
    st.edit_messages(|msgs| *msgs = (0..40).map(image_message).collect());
    st.loaded = true;
    crate::ui::render_to_string(&mut app, 100, 30);
    let queued = |n: usize| {
        app.images
            .queued()
            .contains(&format!("https://demo.invalid/plasma/{n}"))
    };
    assert!(queued(39), "the newest message is on screen");
    assert!(
        !queued(0),
        "a message far above the viewport is not fetched yet"
    );
    assert!(
        app.images.queued().len() < 20,
        "a bounded window, not all 40: {}",
        app.images.queued().len()
    );
}

#[test]
fn decoded_images_far_from_the_screen_are_evicted_and_visible_ones_kept() {
    let mut store = ImageStore::default();
    let ready = || ImgState::Ready(Arc::new(RgbImage::new(2, 2)));
    for n in 0..6 {
        store.set(&format!("u{n}"), ready());
    }
    store.begin("loading");
    let keep: HashSet<&str> = ["u0"].into_iter().collect();
    let before = store.version;
    store.evict(3, &keep);
    let left: Vec<&str> = (0..6)
        .map(|n| format!("u{n}"))
        .filter(|u| store.map.contains_key(u))
        .map(|u| if u == "u0" { "u0" } else { "other" })
        .collect();
    assert_eq!(left.len(), 3, "trimmed to the limit");
    assert!(
        store.map.contains_key("u0"),
        "an image in the protected set survives, however old"
    );
    assert!(
        store.map.contains_key("loading"),
        "a download in progress is never evicted"
    );
    assert!(
        store.version > before,
        "the transcript is told to re-render"
    );
}

#[test]
fn ctrl_h_and_ctrl_l_move_between_panes_from_anywhere() {
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    let mut app = app();
    app.on_key(ctrl('l'));
    assert_eq!(app.focus, Focus::Messages);
    app.on_key(ctrl('l'));
    assert_eq!(app.focus, Focus::Editor);
    app.on_key(ctrl('l'));
    assert_eq!(app.focus, Focus::Editor);
    // From insert mode: leaves for the chat list and the editor is back in normal mode, not stuck in insert.
    app.on_key(key('i'));
    assert_eq!(app.editor.mode(), Mode::Insert);
    app.on_key(ctrl('h'));
    assert_eq!(app.focus, Focus::Chats);
    assert_eq!(app.editor.mode(), Mode::Normal);
    // Already at the left edge: stays put.
    app.on_key(ctrl('h'));
    assert_eq!(app.focus, Focus::Chats);
}

#[test]
fn filter_narrows_the_chat_list() {
    let mut app = app();
    app.on_key(key('/'));
    for c in "grace".chars() {
        app.on_key(key(c));
    }
    let vis = app.visible_chats();
    assert_eq!(vis.len(), 1);
    assert!(
        app.chats.items()[vis[0]]
            .title(app.me_id())
            .contains("Grace")
    );
    assert_eq!(
        app.current_chat().map(|c| c.title(app.me_id())).as_deref(),
        Some("Grace Hopper")
    );
}

#[test]
fn composing_and_sending_shows_the_message_and_clears_the_editor() {
    let mut app = app();
    let id = app.current.clone().unwrap();
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.on_key(key('i'));
    assert_eq!(app.focus, Focus::Editor);
    for c in "hello **there**".chars() {
        app.on_key(key(c));
    }
    app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(app.editor.is_empty());
    let st = app.states.get(&id).unwrap();
    assert_eq!(st.messages().last().unwrap().delivery, Delivery::Sending);
    pump(&mut app, |a| {
        a.states
            .get(&id)
            .unwrap()
            .messages()
            .iter()
            .all(|m| m.delivery == Delivery::Sent)
    });
    let last = app.states.get(&id).unwrap().messages().last().unwrap();
    assert!(
        last.body.contains("<strong>there</strong>"),
        "{}",
        last.body
    );
}

#[test]
fn drafts_follow_their_chat() {
    let mut app = app();
    app.on_key(key('i'));
    app.on_key(key('x'));
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)); // normal
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)); // leave editor
    assert_eq!(app.focus, Focus::Messages);
    app.on_key(key('h'));
    app.on_key(key('j'));
    assert!(app.editor.is_empty(), "other chat has its own draft");
    app.on_key(key('k'));
    assert_eq!(app.editor.text(), "x");
}

/// Type `text` into the open chat's editor and send it.
fn compose_and_send(app: &mut App, text: &str) {
    app.on_key(key('i'));
    for c in text.chars() {
        app.on_key(key(c));
    }
    app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
}

fn last_message(app: &App) -> &Msg {
    app.state().unwrap().messages().last().unwrap()
}

#[test]
fn a_refused_send_gives_the_text_back_and_removes_the_pending_message() {
    let mut app = app_with(Misbehaving::refusing_sends());
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    compose_and_send(&mut app, "hello");
    assert!(app.editor.is_empty() && last_message(&app).delivery == Delivery::Sending);
    app.on_tick();
    assert_eq!(
        app.editor.text(),
        "hello",
        "nothing was delivered, so the text is back to edit"
    );
    assert!(
        app.state().unwrap().messages().is_empty(),
        "the pending message is gone"
    );
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.starts_with("send failed")),
        "{:?}",
        app.status
    );
}

#[test]
fn a_send_with_an_unknown_outcome_is_not_offered_for_an_easy_resend() {
    let mut app = app_with(Misbehaving::new(false));
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    compose_and_send(&mut app, "hello");
    app.on_tick();
    assert!(
        app.editor.is_empty(),
        "the text is not put back, so it cannot be sent twice by habit"
    );
    let delivery = &last_message(&app).delivery;
    assert!(
        matches!(delivery, Delivery::Failed(why) if why.contains("not confirmed") && why.contains("check the chat")),
        "{delivery:?}"
    );
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.starts_with("send not confirmed")),
        "{:?}",
        app.status
    );
}

#[test]
fn a_refused_send_keeps_the_message_in_the_transcript_when_the_editor_is_busy() {
    let mut app = app_with(Misbehaving::refusing_sends());
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    compose_and_send(&mut app, "first");
    // Typing the next message before the answer arrives (the editor is still in insert mode).
    app.on_key(key('n'));
    app.on_tick();
    assert_eq!(
        app.editor.text(),
        "n",
        "what is being typed is not overwritten"
    );
    assert!(
        matches!(&last_message(&app).delivery, Delivery::Failed(why) if why.contains("not allowed") && !why.contains("not confirmed")),
        "{:?}",
        last_message(&app).delivery
    );
}

#[test]
fn a_late_send_refusal_preserves_the_editor_of_another_chat() {
    for draft in ["", "another chat's draft"] {
        let mut app = app_with(Misbehaving::refusing_sends());
        let original = app.current.clone().unwrap();
        app.editor.set_text("first message");
        app.send();
        let mut other = app.chats.items()[0].clone();
        other.id = "other-chat".into();
        app.chats.remember(other);
        app.select_chat("other-chat");
        app.editor.set_text(draft);
        app.on_tick();
        assert_eq!(app.editor.text(), draft);
        assert_eq!(app.sending, 0);
        assert!(matches!(
            app.states[&original].messages().last().unwrap().delivery,
            Delivery::Failed(_)
        ));
        app.select_chat(&original);
        assert!(
            app.editor.is_empty(),
            "the refused send is retained in the original transcript"
        );
    }
}

#[test]
fn cached_rendering_tracks_message_edits_and_optimistic_reaction_rollback() {
    let (mut app, _) = standard_app();
    let chat_id = app.current.clone().unwrap();
    let mut edited = last_message(&app).clone();
    edited.body = "original render sentinel".into();
    edited.modified += chrono::Duration::days(1);
    edited.reactions.clear();
    let update = |message| Ev::Messages {
        chat_id: chat_id.clone(),
        fetch: Fetch::Newest { quiet: true },
        result: Ok(Page {
            messages: vec![message],
            older: None,
        }),
    };
    app.on_event(update(edited.clone()));
    let screen = crate::ui::render_to_string(&mut app, 130, 40);
    assert!(screen.contains("original render sentinel"));
    edited.body = "edited render sentinel".into();
    edited.modified += chrono::Duration::seconds(1);
    app.on_event(update(edited.clone()));
    let screen = crate::ui::render_to_string(&mut app, 130, 40);
    assert!(screen.contains("edited render sentinel"));
    assert!(!screen.contains("original render sentinel"));

    app.apply_reaction(&chat_id, &edited.id, "👍", true);
    crate::ui::render_to_string(&mut app, 130, 40);
    let message_id = edited.id.clone();
    let transcript = |app: &App| {
        let st = app.state().unwrap();
        let index = st
            .messages()
            .iter()
            .position(|m| m.id == message_id)
            .unwrap();
        st.transcript.lines[st.transcript.starts[index]..=st.transcript.ends[index]]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(transcript(&app).contains("👍 1"));
    app.on_event(Ev::Reacted {
        chat_id,
        message_id: edited.id,
        emoji: "👍".into(),
        on: true,
        result: Err(scripted_failure()),
    });
    crate::ui::render_to_string(&mut app, 130, 40);
    assert!(!transcript(&app).contains("👍 1"));
    assert!(transcript(&app).contains("edited render sentinel"));
}

#[test]
fn yanking_copies_to_the_clipboard_and_p_pastes_what_another_program_copied() {
    let (mut app, fakes) = app_and_fakes();
    app.on_key(key('i'));
    for c in "mine".chars() {
        app.on_key(key(c));
    }
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    for c in "yiw".chars() {
        app.on_key(key(c));
    }
    assert_eq!(fakes.clipboard.get().as_deref(), Some("mine"));
    // Our own yank is not mistaken for something new, but another program's copy is.
    fakes.clipboard.put("theirs");
    app.on_key(key('p'));
    assert_eq!(
        app.editor.text(),
        "mtheirsine",
        "pasted after the cursor, which `yiw` left on the word's first letter"
    );
}

/// A clipboard that is the terminal's: it cannot be read and cannot confirm a copy.
#[derive(Debug, Default)]
struct ViaTerminal(std::sync::Mutex<Option<String>>);

impl Clipboard for ViaTerminal {
    fn get(&self) -> Option<String> {
        None
    }
    fn set(&self, text: &str) -> Result<crate::clipboard::Via, String> {
        *self.0.lock().unwrap() = Some(text.to_string());
        Ok(crate::clipboard::Via::Terminal)
    }
}

#[test]
fn a_copy_says_what_was_copied_and_when_it_went_through_the_terminal() {
    // A message yanked with the system clipboard, in the vim keymap.
    let (mut app, fakes) = app_and_fakes();
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.focus = Focus::Messages;
    app.on_key(key('y'));
    assert!(fakes.clipboard.get().is_some());
    let said = app.status.clone().unwrap().0;
    assert_eq!(said, "message copied as Markdown");

    // The same through the terminal: nothing can confirm it, and the wording says so.
    let terminal = Arc::new(ViaTerminal::default());
    app.set_clipboard(terminal.clone());
    app.on_key(key('y'));
    let said = app.status.clone().unwrap().0;
    assert!(said.contains("sent to the terminal"), "{said}");
    assert!(terminal.0.lock().unwrap().is_some());

    // Deliberate copies in the standard keymap say how much; vim's deletes do not announce themselves.
    let (mut standard, _) = App::for_test(Arc::new(DemoBackend::instant()));
    standard.set_keymap(Keymap::Standard);
    standard.start();
    pump(&mut standard, |a| a.chats.loaded());
    standard.focus = Focus::Editor;
    standard.editor.set_text("hello world");
    press(&mut standard, KeyCode::Char('a'), KeyModifiers::CONTROL);
    press(&mut standard, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(standard.status.clone().unwrap().0, "11 characters copied");

    // Through the terminal the clipboard cannot be read back, so Ctrl-V pastes the last copy made in vtteams.
    standard.set_clipboard(Arc::new(ViaTerminal::default()));
    press(&mut standard, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(
        standard
            .status
            .clone()
            .unwrap()
            .0
            .contains("sent to the terminal")
    );
    standard.editor.set_text("");
    press(&mut standard, KeyCode::Char('v'), KeyModifiers::CONTROL);
    assert_eq!(standard.editor.text(), "hello world");

    let (mut vim, _) = app_and_fakes();
    vim.focus = Focus::Editor;
    vim.editor.set_text("hello world");
    vim.status = None;
    vim.editor.set_mode(Mode::Normal);
    vim.on_key(key('d'));
    vim.on_key(key('d'));
    assert!(vim.status.is_none(), "{:?}", vim.status);
}

#[test]
fn a_clipboard_that_cannot_be_written_is_reported() {
    #[derive(Debug)]
    struct Broken;
    impl Clipboard for Broken {
        fn get(&self) -> Option<String> {
            None
        }
        fn set(&self, _: &str) -> Result<crate::clipboard::Via, String> {
            Err("no display".into())
        }
    }
    let mut app = app();
    app.clipboard = Arc::new(Broken);
    app.copy_to_clipboard("x", None);
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.contains("clipboard unavailable: no display")),
        "{:?}",
        app.status
    );
}

#[test]
fn links_are_handed_to_the_opener_and_non_web_links_are_refused() {
    let (mut app, fakes) = app_and_fakes();
    app.open_link("https://example.com/a");
    app.open_link("file:///etc/passwd");
    app.on_tick();
    assert_eq!(fakes.opener.opened(), ["https://example.com/a"]);
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.contains("non-web")),
        "{:?}",
        app.status
    );
}

#[test]
fn clicking_any_row_of_a_wrapped_link_opens_the_whole_address() {
    // The sender's client sends a shortened label for a long address; the link list under the message wraps the full
    // address over several rows, and a click on its last row opens all of it, not just what one row shows.
    let (mut app, fakes) = app_and_fakes();
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    let id = app.current.clone().expect("a chat is open");
    let url = "https://code.example.com/group/project/-/blob/main/src/main/module/Service/Dispatch.cls?blame=1&ref_type=heads#L72";
    let mut m = msg("long-link", -1);
    m.body = format!(
        r#"<p>See <a href="{url}">https://code.example.com/group/project/-/blob/main/src/ma…</a></p>"#
    );
    app.on_event(Ev::Messages {
        chat_id: id,
        fetch: Fetch::Newest { quiet: true },
        result: Ok(teams_client::Page {
            messages: vec![m],
            older: None,
        }),
    });
    crate::ui::render_to_string(&mut app, 100, 40);
    let st = app.state().unwrap();
    let (rows, target) = st
        .transcript
        .link_rows
        .iter()
        .find(|(_, u)| u == url)
        .cloned()
        .expect("the link has rows in the list");
    assert!(rows.len() > 1, "a long address wraps: {rows:?}");
    let last = rows.end - 1;
    assert!(
        last >= st.scroll,
        "the link is on screen: {last} {}",
        st.scroll
    );
    let row = app.view.geometry.transcript.y + (last - st.scroll) as u16;
    assert_eq!(target, url);
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: app.view.geometry.transcript.x + 10,
        row,
        modifiers: KeyModifiers::NONE,
    });
    app.on_tick();
    assert_eq!(fakes.opener.opened(), [url]);
}

/// Open a chat holding one message from someone else with `body` as its HTML.
fn app_showing(body: &str) -> (App, Fakes) {
    let (mut app, fakes) = app_and_fakes();
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    let id = app.current.clone().expect("a chat is open");
    let mut m = msg("with-links", -1);
    m.body = body.into();
    app.on_event(Ev::Messages {
        chat_id: id,
        fetch: Fetch::Newest { quiet: true },
        result: Ok(teams_client::Page {
            messages: vec![m],
            older: None,
        }),
    });
    (app, fakes)
}

/// The frame drawn at `width` x `height` (what ratatui diffs, so the link cells are as `hyperlink::mark` left them),
/// and every terminal hyperlink in it as (row, address, text).
fn frame_links(
    app: &mut App,
    width: u16,
    height: u16,
) -> (ratatui::buffer::Buffer, Vec<(u16, String, String)>) {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    let buffer = terminal
        .draw(|f| crate::ui::render(f, app))
        .unwrap()
        .buffer
        .clone();
    let mut links = Vec::new();
    for y in 0..height {
        for x in 0..width {
            let symbol = buffer[(x, y)].symbol();
            if let Some(rest) = symbol.strip_prefix("\x1b]8;;") {
                let (url, rest) = rest.split_once("\x1b\\").unwrap();
                let text = rest.strip_suffix("\x1b]8;;\x1b\\").unwrap();
                links.push((y, url.to_string(), text.to_string()));
            }
        }
    }
    (buffer, links)
}

#[test]
fn a_shortened_label_and_every_row_of_its_wrapped_entry_are_hyperlinks_to_the_whole_address() {
    let url = "https://code.example.com/group/project/-/blob/main/src/main/module/Service/Dispatch.cls?blame=1&ref_type=heads#L72";
    let label = "https://code.example.com/group/project/-/blob/main/src/ma…";
    let (mut app, _) = app_showing(&format!(r#"<p>See <a href="{url}">{label}</a></p>"#));
    app.hyperlinks = true;
    crate::ui::render_to_string(&mut app, 100, 40);
    let (buffer, links) = frame_links(&mut app, 100, 40);
    // The demo chat's own messages have links too; these are the new message's, and each opens the whole address.
    let links: Vec<_> = links
        .into_iter()
        .filter(|(_, u, _)| u.starts_with("https://code.example.com/"))
        .collect();
    assert!(
        links.iter().all(|(_, u, _)| u == url),
        "every mark opens the whole address: {links:?}"
    );
    assert!(
        links.iter().any(|(_, _, text)| text == label),
        "the label the sender's client shortened is itself a link: {links:?}"
    );
    let listed: String = links
        .iter()
        .filter(|(_, _, text)| text != label)
        .map(|(_, _, text)| text.trim_start_matches("[1] ").trim())
        .collect();
    assert_eq!(
        listed, url,
        "the rows of the [1] entry, together, are the whole address"
    );
    assert!(
        links.iter().filter(|(_, _, text)| text != label).count() > 1,
        "the entry wraps over several rows, each one marked: {links:?}"
    );
    assert_eq!(
        crate::output::first_control_cell(&buffer),
        None,
        "the frame's check lets these marks through"
    );
    // Off (the default, and what a dump uses): nothing is marked.
    app.hyperlinks = false;
    crate::ui::render_to_string(&mut app, 100, 40);
    assert!(frame_links(&mut app, 100, 40).1.is_empty());
}

#[test]
fn a_label_that_reads_as_a_different_address_is_not_made_a_link_but_its_entry_is() {
    let (mut app, _) = app_showing(
        r#"<p><a href="https://b.example/real">https://a.example/looks-safe</a> and <a href="https://e.example/doc">the doc</a></p>"#,
    );
    app.hyperlinks = true;
    crate::ui::render_to_string(&mut app, 100, 40);
    let (_, links) = frame_links(&mut app, 100, 40);
    assert!(
        !links.iter().any(|(_, _, text)| text.contains("a.example")),
        "a label naming another address must not lead elsewhere: {links:?}"
    );
    assert!(
        links
            .iter()
            .any(|(_, u, t)| u == "https://b.example/real" && t.contains("b.example/real"))
    );
    assert!(
        links
            .iter()
            .any(|(_, u, t)| u == "https://e.example/doc" && t == "the doc")
    );
}

#[test]
fn nothing_is_marked_while_a_popup_is_open_over_the_conversation() {
    let (mut app, _) = app_showing(r#"<p><a href="https://e.example/doc">the doc</a></p>"#);
    app.hyperlinks = true;
    crate::ui::render_to_string(&mut app, 100, 40);
    assert!(!frame_links(&mut app, 100, 40).1.is_empty());
    app.overlay = Overlay::Links(vec!["https://e.example/doc".into()]);
    crate::ui::render_to_string(&mut app, 100, 40);
    assert!(frame_links(&mut app, 100, 40).1.is_empty());
}

#[test]
fn someone_no_longer_in_the_directory_is_named_dim_and_in_italics_wherever_the_name_is_drawn() {
    let (mut app, _) = app_and_fakes();
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    let me = app.me_id().to_string();
    let other = app
        .current_chat()
        .and_then(|c| c.members.iter().find(|m| m.id != me))
        .map(|m| (m.id.clone(), m.name.clone()))
        .expect("a 1:1 chat with someone");
    let draw = |app: &mut App| {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| crate::ui::render(f, app))
            .unwrap()
            .buffer
            .clone()
    };
    // Where the name starts on screen, with whether it is drawn in italics there.
    let places = |buffer: &ratatui::buffer::Buffer| -> Vec<(u16, u16, bool)> {
        let mut out = Vec::new();
        for y in 0..buffer.area.height {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            if let Some(at) = row.find(other.1.as_str()) {
                let x = row[..at].chars().count() as u16;
                let italic = buffer[(x, y)]
                    .modifier
                    .contains(ratatui::style::Modifier::ITALIC);
                out.push((x, y, italic));
            }
        }
        out
    };
    app.presences.set_known(
        &other.0,
        teams_client::Presence::new(teams_client::Availability::Offline, false),
    );
    let shown = app.current_chat().cloned().unwrap();
    assert!(
        app.presence_of(&shown).is_some(),
        "their availability is known and shown"
    );
    let before = places(&draw(&mut app));
    assert!(
        before.len() >= 3,
        "list, header and messages name them: {before:?}"
    );
    assert!(before.iter().all(|(.., italic)| !italic), "{before:?}");
    assert!(!crate::ui::render_to_string(&mut app, 120, 40).contains("no longer in the directory"));

    app.chats.edit_items(|chats| {
        for c in chats.iter_mut() {
            for m in &mut c.members {
                if m.id == other.0 {
                    m.not_in_directory = true;
                }
            }
        }
    });
    let buffer = draw(&mut app);
    let after = places(&buffer);
    assert_eq!(
        after.iter().map(|(x, y, _)| (x, y)).collect::<Vec<_>>(),
        before.iter().map(|(x, y, _)| (x, y)).collect::<Vec<_>>(),
        "the same places"
    );
    assert!(after.iter().all(|(.., italic)| *italic), "{after:?}");
    assert!(crate::ui::render_to_string(&mut app, 120, 40).contains("no longer in the directory"));
    let chat = app.current_chat().cloned().unwrap();
    assert!(
        app.presence_of(&chat).is_none(),
        "no availability (it would only ever say offline)"
    );
}

#[test]
fn a_mention_of_a_full_name_is_coloured_whole_in_the_message_box_as_it_will_be_sent() {
    let (mut app, _) = app_and_fakes();
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    let me = app.me_id().to_string();
    let name = app
        .current_chat()
        .and_then(|c| c.members.iter().find(|m| m.id != me))
        .map(|m| m.name.clone())
        .expect("a 1:1 chat with someone");
    let (first, last) = name.split_once(' ').expect("a first and a last name");
    app.focus = Focus::Editor;
    app.editor.paste(&format!("hi @{name} and @Nobody Here"));
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    let buffer = terminal
        .draw(|f| crate::ui::render(f, &mut app))
        .unwrap()
        .buffer
        .clone();
    let area = app.view.geometry.editor;
    let (y, row) = (area.y..area.bottom())
        .map(|y| {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            (y, row)
        })
        .find(|(_, row)| row.contains("hi @"))
        .expect("the text is in the box");
    let style_at = |text: &str| {
        let x = row[..row.find(text).unwrap()].chars().count() as u16;
        (buffer[(x, y)].fg, buffer[(x, y)].modifier)
    };
    let at = style_at(&format!("@{first}"));
    assert_eq!(style_at(last), at, "the last name is part of the mention");
    assert_ne!(style_at(" and"), at, "the text after it is not");
    assert_ne!(
        style_at("Here"),
        at,
        "a name that is nobody in the chat is not a mention past its first word"
    );
    // What is coloured is what is sent.
    let sent = crate::convert::markdown_to_html_noting(
        &app.editor.text(),
        &app.mention_candidates(),
        &me,
        teams_client::CodeBlockStyle::default(),
    );
    assert!(sent.1.iter().any(|m| m.name == name), "{sent:?}");
}

#[test]
fn clicking_an_image_saves_it_and_opens_the_saved_file() {
    let (mut app, fakes) = app_and_fakes();
    app.select_chat("demo-ada");
    for _ in 0..50 {
        crate::ui::render_to_string(&mut app, 120, 40);
        app.on_tick();
        if app
            .state()
            .is_some_and(|s| !s.transcript.image_rows.is_empty())
            && app
                .images
                .store()
                .map
                .values()
                .any(|s| matches!(s, ImgState::Ready(_)))
        {
            break;
        }
    }
    crate::ui::render_to_string(&mut app, 120, 40);
    let st = app.state().unwrap();
    let (rows, _) = st.transcript.image_rows[0].clone();
    let row = app.view.geometry.transcript.y + (rows.start - st.scroll) as u16;
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: app.view.geometry.transcript.x,
        row,
        modifiers: KeyModifiers::NONE,
    });
    app.on_tick();
    let opened = fakes.opener.opened();
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert!(
        opened[0].starts_with("file://") && opened[0].ends_with(".png"),
        "{opened:?}"
    );
}

#[test]
fn a_picture_opens_from_the_keyboard_when_its_message_has_nothing_else_to_open() {
    let (mut app, fakes) = app_and_fakes();
    app.select_chat("demo-ada");
    for _ in 0..50 {
        crate::ui::render_to_string(&mut app, 120, 40);
        app.on_tick();
        if app
            .state()
            .is_some_and(|s| !s.transcript.image_rows.is_empty())
            && app
                .images
                .store()
                .map
                .values()
                .any(|s| matches!(s, ImgState::Ready(_)))
        {
            break;
        }
    }
    crate::ui::render_to_string(&mut app, 120, 40);
    let (rows, _) = app.state().unwrap().transcript.image_rows[0].clone();
    let message = app
        .state()
        .unwrap()
        .transcript
        .starts
        .iter()
        .rposition(|&s| s <= rows.start)
        .unwrap();
    assert!(
        app.state().unwrap().transcript.links[message].is_empty(),
        "the demo's picture message has no links"
    );
    app.state_mut().unwrap().sel = message;
    app.focus = Focus::Messages;
    app.on_key(key('o'));
    app.on_tick();
    let opened = fakes.opener.opened();
    assert!(
        opened.len() == 1 && opened[0].starts_with("file://") && opened[0].ends_with(".png"),
        "{opened:?}"
    );
}
#[test]
fn a_click_on_the_command_panel_opens_it_and_the_panel_is_where_it_was_drawn() {
    let mut app = app();
    crate::ui::render_to_string(&mut app, 120, 40);
    let panel = app.view.geometry.panel;
    assert_eq!((panel.y, panel.height), (39, 1), "the bottom row");
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: panel.x + 3,
        row: panel.y,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(app.cmdline.as_deref(), Some(""));
}

fn click(app: &mut App, column: u16, row: u16) {
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    });
    app.on_tick();
    crate::ui::render_to_string(app, 120, 40);
}

#[test]
fn a_click_on_a_chat_opens_it_and_moves_to_its_messages() {
    let mut app = app();
    // A plain list, two rows a chat; the pinned list with its divider has a test of its own.
    app.pin_favorites = false;
    crate::ui::render_to_string(&mut app, 120, 40);
    let vis = app.visible_chats();
    assert!(vis.len() >= 3, "the demo has several chats");
    let body = app.view.geometry.list_body;
    // Each chat takes two rows: both rows of the third one open it, and the editor loses its insert mode.
    for row in [body.y + 4, body.y + 5] {
        app.focus = Focus::Chats;
        app.current = None;
        click(&mut app, body.x + 2, row);
        let id = app.chats.items()[vis[2]].id.clone();
        assert_eq!(app.current.as_deref(), Some(id.as_str()), "row {row}");
        assert_eq!(app.focus, Focus::Messages);
    }
    // Below the last chat there is nothing to open.
    let below = body.y + 2 * vis.len() as u16;
    if below < body.y + body.height {
        let before = app.current.clone();
        click(&mut app, body.x + 2, below);
        assert_eq!(app.current, before);
    }
}

#[test]
fn a_click_on_a_message_selects_it_without_moving_the_view() {
    let mut app = app();
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    crate::ui::render_to_string(&mut app, 120, 40);
    app.focus = Focus::Chats;
    let st = app.state().unwrap();
    assert!(st.transcript.starts.len() >= 3);
    let (scroll, last) = (st.scroll, st.messages().len() - 1);
    // A message that is fully on screen, other than the selected one.
    let target = (0..=last)
        .find(|&i| {
            i != st.sel
                && st.transcript.starts[i] >= scroll
                && st.transcript.ends[i] < scroll + app.view.transcript_height()
        })
        .expect("a second message on screen");
    let row = app.view.geometry.transcript.y + (st.transcript.ends[target] - scroll) as u16;
    let column = app.view.geometry.transcript.x + 2;
    click(&mut app, column, row);
    let st = app.state().unwrap();
    assert_eq!(st.sel, target);
    assert_eq!(st.scroll, scroll, "the view stays where it was");
    assert_eq!(app.focus, Focus::Messages);
    assert_eq!(st.follow, target == last);
}

#[test]
fn a_click_in_the_message_box_starts_typing_and_a_click_elsewhere_leaves_it() {
    let mut app = app();
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    crate::ui::render_to_string(&mut app, 120, 40);
    assert_ne!(app.focus, Focus::Editor);
    let editor = app.view.geometry.editor;
    click(&mut app, editor.x + 3, editor.y + 1);
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(app.editor.mode(), Mode::Insert);
    app.on_key(key('x'));
    assert_eq!(app.editor.text(), "x", "typing goes into the box");
    // The row of the open chat: clicking it again keeps the draft.
    let vis = app.visible_chats();
    let open = vis
        .iter()
        .position(|&i| Some(&app.chats.items()[i].id) == app.current.as_ref())
        .unwrap();
    let body = app.view.geometry.list_body;
    let row = body.y + 2 * (open - app.view.list_offset) as u16;
    click(&mut app, body.x + 2, row);
    assert_eq!(app.focus, Focus::Messages);
    assert_eq!(app.editor.mode(), Mode::Normal);
    assert_eq!(app.editor.text(), "x", "the draft is kept");
}

#[test]
fn drawing_does_not_change_what_the_app_would_do() {
    // `draw` takes a shared reference, so this is checked by the compiler; what remains is that preparing a frame
    // twice at the same size is idempotent.
    let mut app = app();
    crate::ui::render_to_string(&mut app, 120, 40);
    let first = (
        app.view.geometry.clone(),
        app.view.list_offset,
        app.view.editor_top,
    );
    crate::ui::render_to_string(&mut app, 120, 40);
    assert_eq!(
        first,
        (
            app.view.geometry.clone(),
            app.view.list_offset,
            app.view.editor_top
        )
    );
}

#[test]
fn the_selected_chat_stays_in_the_visible_part_of_the_list() {
    let mut app = app();
    crate::ui::render_to_string(&mut app, 100, 14); // room for only a few chats
    for _ in 0..app.chats.items().len() {
        app.on_key(key('j'));
        crate::ui::render_to_string(&mut app, 100, 14);
        let vis = app.visible_chats();
        let sel = vis
            .iter()
            .position(|&i| Some(&app.chats.items()[i].id) == app.current.as_ref())
            .unwrap();
        let cap = app.view.geometry.list_body.height as usize / 2;
        let off = app.view.list_offset;
        assert!(
            sel >= off && sel < off + cap.max(1),
            "selected {sel} outside {off}..{}",
            off + cap
        );
    }
}

fn ago(secs: u64) -> Instant {
    Instant::now()
        .checked_sub(Duration::from_secs(secs))
        .expect("the clock has run long enough")
}

/// An app whose open chat has loaded.
fn loaded_app() -> App {
    let mut app = app();
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app
}

/// Whether a tick fetched the open chat when it was last fetched `secs` ago (a fetch stamps its schedule).
fn polls_when_last_polled(app: &mut App, secs: u64) -> bool {
    let id = app.current.clone().unwrap();
    let stamp = ago(secs);
    app.states.get_mut(&id).unwrap().sched.set_last(stamp);
    app.on_tick();
    app.states[&id].sched.last() > stamp
}

#[test]
fn an_active_user_polls_every_eight_seconds() {
    let mut app = loaded_app();
    assert!(!polls_when_last_polled(&mut app, 5));
    assert!(polls_when_last_polled(&mut app, 9));
}

#[test]
fn an_idle_user_polls_every_thirty_seconds_and_input_brings_the_pace_back() {
    let mut app = loaded_app();
    app.last_input = ago(10 * 60);
    assert!(!polls_when_last_polled(&mut app, 20));
    assert!(polls_when_last_polled(&mut app, 40));
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)); // changes nothing, but it is input
    assert!(app.last_input.elapsed() < Duration::from_secs(1));
    assert!(polls_when_last_polled(&mut app, 9), "active again");
    assert!(!polls_when_last_polled(&mut app, 5));
}

#[test]
fn a_conversation_in_progress_is_polled_every_four_seconds_even_behind_other_windows() {
    let mut app = loaded_app();
    let id = app.current.clone().unwrap();
    app.states.get_mut(&id).unwrap().last_exchange = Some(ago(30));
    assert!(polls_when_last_polled(&mut app, 5));
    app.set_focused(false);
    assert!(
        polls_when_last_polled(&mut app, 5),
        "still following the conversation"
    );
    app.states.get_mut(&id).unwrap().last_exchange = Some(ago(300));
    assert!(
        !polls_when_last_polled(&mut app, 20),
        "unfocused and the conversation has gone quiet"
    );
}

#[test]
fn returning_to_the_terminal_fetches_at_once_but_staying_focused_does_not() {
    let mut app = loaded_app();
    let id = app.current.clone().unwrap();
    let stale = ago(1);
    app.chats.sched.set_last(stale);
    app.states.get_mut(&id).unwrap().sched.set_last(stale);
    app.set_focused(true); // already in front: nothing to catch up on
    app.on_tick();
    assert_eq!(app.chats.sched.last(), stale);
    assert_eq!(app.states[&id].sched.last(), stale);

    app.set_focused(false);
    app.on_tick();
    assert_eq!(
        app.chats.sched.last(),
        stale,
        "going behind other windows fetches nothing"
    );
    app.set_focused(true);
    app.on_tick();
    assert!(app.chats.sched.last() > stale, "the chat list is refreshed");
    assert!(
        app.states[&id].sched.last() > stale,
        "and so is the open chat"
    );
    assert!(
        app.last_input.elapsed() < Duration::from_secs(1),
        "coming back counts as being active"
    );
}

fn throttled(wait: Option<u64>) -> Error {
    Error::Throttled {
        operation: "read messages",
        retry_after: wait.map(Duration::from_secs),
    }
}

#[test]
fn only_a_throttled_answer_carries_a_wait_and_a_missing_hint_means_a_default() {
    assert_eq!(
        throttle_hold(&throttled(Some(90))),
        Some(Duration::from_secs(90))
    );
    assert_eq!(
        throttle_hold(&throttled(None)),
        Some(cadence::THROTTLED_WAIT)
    );
    assert_eq!(throttle_hold(&scripted_failure()), None);
}

#[test]
fn a_throttled_answer_holds_polling_for_as_long_as_the_service_asked() {
    let mut app = loaded_app();
    let id = app.current.clone().unwrap();
    app.on_event(Ev::Messages {
        chat_id: id.clone(),
        fetch: Fetch::Newest { quiet: false },
        result: Err(throttled(Some(90))),
    });
    assert!(app.states[&id].sched.held());
    // Failures alone would allow a retry after 16 s, and coming back to the terminal asks for one at once; neither
    // overrides what the service asked for.
    assert!(!polls_when_last_polled(&mut app, 60));
    app.set_focused(false);
    app.set_focused(true);
    assert!(!polls_when_last_polled(&mut app, 60));

    // Another kind of failure only backs off.
    let mut app = loaded_app();
    let id = app.current.clone().unwrap();
    app.on_event(Ev::Messages {
        chat_id: id.clone(),
        fetch: Fetch::Newest { quiet: false },
        result: Err(scripted_failure()),
    });
    assert!(!app.states[&id].sched.held());
    assert!(polls_when_last_polled(&mut app, 60));
}

#[test]
fn a_throttled_chat_list_waits_too() {
    let mut app = app();
    app.on_event(Ev::Chats(Err(throttled(None))));
    assert!(app.chats.sched.held());
    app.chats.sched.set_last(ago(120));
    app.on_tick();
    assert!(
        app.chats.sched.last() < Instant::now() - Duration::from_secs(100),
        "the list was not fetched while the hold-off lasts"
    );
    app.on_event(Ev::Chats(Ok(app.chats.items().to_vec())));
    assert!(!app.chats.sched.held(), "a success ends the hold-off");
}

#[test]
fn sending_or_receiving_makes_a_conversation_but_the_first_load_does_not() {
    let mut app = loaded_app();
    let id = app.current.clone().unwrap();
    assert!(
        app.states[&id].last_exchange.is_none(),
        "history is not a conversation"
    );

    app.on_event(Ev::Messages {
        chat_id: id.clone(),
        fetch: Fetch::Newest { quiet: false },
        result: Ok(Page {
            messages: vec![msg("new-from-someone", 0)],
            older: None,
        }),
    });
    assert!(app.states[&id].last_exchange.is_some());

    let mut app = loaded_app();
    let id = app.current.clone().unwrap();
    app.on_key(key('i'));
    app.on_key(key('x'));
    app.send();
    assert!(app.states[&id].last_exchange.is_some());
}

#[test]
#[ignore = "a measurement, run on demand"]
fn frame_cost_with_a_long_conversation_and_a_long_draft() {
    let mut app = app();
    let id = app.current.clone().unwrap();
    let st = app.states.get_mut(&id).unwrap();
    st.edit_messages(|msgs| {
        *msgs = (0..2000).map(|n| msg(&format!("m{n}"), 2000 - n)).collect();
        for m in msgs {
            m.body = concat!(
                "<p>some <b>bold</b> text, a <a href=\"https://example.com/page\">link</a>",
                " and enough words to wrap across a couple of lines in a narrow pane</p>"
            )
            .into();
        }
    });
    st.loaded = true;
    app.editor
        .set_text(&"a line of the draft with `code` and **bold**\n".repeat(300));
    let started = std::time::Instant::now();
    crate::ui::render_to_string(&mut app, 160, 50);
    let first = started.elapsed();
    let frames = 30;
    let started = std::time::Instant::now();
    for _ in 0..frames {
        crate::ui::render_to_string(&mut app, 160, 50);
    }
    let steady = started.elapsed() / frames;
    println!("first frame {first:?}, steady frame {steady:?}");
    assert!(
        steady < Duration::from_millis(50),
        "steady frame {steady:?}"
    );
}

// -- the standard keymap -------------------------------------------------------------

fn standard_app() -> (App, Fakes) {
    let (mut app, fakes) = App::for_test(Arc::new(DemoBackend::instant()));
    app.set_keymap(Keymap::Standard);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    (app, fakes)
}

fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    app.on_key(KeyEvent::new(code, mods));
}

/// Position of the open chat among `visible_chats()`.
fn selected_chat_position(app: &App) -> usize {
    app.visible_chats()
        .iter()
        .position(|&i| Some(&app.chats.items()[i].id) == app.current.as_ref())
        .unwrap()
}

#[test]
fn page_down_in_the_standard_keymap_moves_by_a_full_page_of_chats_not_a_full_page_of_rows() {
    let (mut app, _) = standard_app();
    // 14 rows tall gives an 8-row list body (`view::layout`): a full page of chats is 4 (two rows a chat), not 8.
    crate::ui::render_to_string(&mut app, 120, 14);
    app.focus = Focus::Chats;
    let first = app.chats.items()[app.visible_chats()[0]].id.clone();
    app.select_chat(&first);
    assert_eq!(selected_chat_position(&app), 0);
    press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    assert_eq!(
        selected_chat_position(&app),
        4,
        "a full page of chats (half of 8 rows), not 8 chats clamped to the end of a 6-chat list"
    );
}

fn ctrl(app: &mut App, c: char) {
    press(app, KeyCode::Char(c), KeyModifiers::CONTROL);
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        app.on_key(key(c));
    }
}

#[test]
fn typing_in_the_conversation_starts_a_message_and_enter_sends_it() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Messages;
    type_text(&mut app, "hi there");
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(app.editor.text(), "hi there");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    pump(&mut app, |a| a.sending == 0);
    assert!(app.editor.is_empty(), "sent");
    assert!(last_message(&app).body.contains("hi there"));
    // Shift-Enter adds a line instead.
    type_text(&mut app, "one");
    press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    type_text(&mut app, "two");
    assert_eq!(app.editor.text(), "one\ntwo");
    assert_eq!(app.sending, 0);
}

#[test]
fn with_enter_adding_a_line_shift_enter_and_ctrl_s_send_it() {
    let (mut app, _) = standard_app();
    app.set_enter_sends(false);
    app.focus = Focus::Messages;
    type_text(&mut app, "one");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    type_text(&mut app, "two");
    assert_eq!(app.editor.text(), "one\ntwo");
    assert_eq!(app.sending, 0, "a plain Enter only adds a line");
    press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
    pump(&mut app, |a| a.sending == 0);
    assert!(app.editor.is_empty(), "sent");
    assert!(last_message(&app).body.contains("one"));
    // The send that every terminal can deliver.
    type_text(&mut app, "again");
    ctrl(&mut app, 's');
    pump(&mut app, |a| a.sending == 0);
    assert!(app.editor.is_empty());
    assert!(last_message(&app).body.contains("again"));
}

#[test]
fn a_favourite_change_already_in_flight_is_not_restarted_by_a_second_press() {
    let (mut app, _) = standard_app();
    // Pick a chat that is not already a favourite, so the first press is unambiguously "add".
    let id = listed_ids(&app)
        .into_iter()
        .find(|id| {
            app.chats
                .items()
                .iter()
                .find(|c| &c.id == id)
                .unwrap()
                .favorite
                .is_none()
        })
        .unwrap();
    app.select_chat(&id);
    press(&mut app, KeyCode::F(7), KeyModifiers::NONE);
    let favorite = |app: &App| {
        app.chats
            .items()
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .favorite
    };
    assert!(favorite(&app).is_some(), "shown at once");
    // The answer has not been drained by on_tick yet, so a second press on the same chat must not fire another
    // request: the two would race a read-modify-write of one shared, service-side value.
    press(&mut app, KeyCode::F(7), KeyModifiers::NONE);
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.contains("still being saved")),
        "{:?}",
        app.status
    );
    assert!(favorite(&app).is_some(), "the first change was not undone");
    app.on_tick();
    assert!(
        app.status.as_ref().is_some_and(|(_, _, err)| !err),
        "the first change's own answer still lands: {:?}",
        app.status
    );
    // Free again: a further press now acts.
    press(&mut app, KeyCode::F(7), KeyModifiers::NONE);
    assert!(favorite(&app).is_none(), "taken out again");
}

#[test]
fn a_chat_list_refresh_that_drops_the_open_chat_does_not_switch_away_mid_draft() {
    let (mut app, _) = standard_app();
    let current = app.current.clone().unwrap();
    app.focus = Focus::Messages;
    type_text(&mut app, "still typing");
    assert_eq!(app.focus, Focus::Editor);
    // A refresh (the scheduled kind runs every 20-180 s on its own) that genuinely no longer lists the open chat
    // (the max_chats cap, or it was left or hidden elsewhere) must not pull the person away into sending this draft
    // to a different chat by mistake.
    let others: Vec<_> = app
        .chats
        .items()
        .iter()
        .filter(|c| c.id != current)
        .cloned()
        .collect();
    assert!(
        !others.is_empty(),
        "there is another chat it could switch to"
    );
    app.on_event(Ev::Chats(Ok(others.clone())));
    assert_eq!(
        app.current.as_deref(),
        Some(current.as_str()),
        "stayed on the chat with the open draft"
    );
    assert_eq!(app.editor.text(), "still typing");

    // Once there is nothing of theirs to lose, the next refresh is free to move on.
    app.editor.clear();
    app.focus = Focus::Chats;
    app.on_event(Ev::Chats(Ok(others)));
    assert_ne!(app.current.as_deref(), Some(current.as_str()));
}

#[test]
fn letters_in_the_chat_list_filter_instead_of_acting() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Chats;
    app.on_key(key('q'));
    assert!(!app.quit, "q is a letter here, not quit");
    assert!(app.filtering && app.filter == "q");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.filtering && app.filter.is_empty());
    type_text(&mut app, "ada");
    assert_eq!(app.filter, "ada");
    let shown: Vec<_> = app
        .visible_chats()
        .into_iter()
        .map(|i| app.chats.items()[i].id.clone())
        .collect();
    assert!(shown.contains(&"demo-ada".to_string()), "{shown:?}");
    assert!(
        shown.len() < app.chats.items().len(),
        "the list is narrowed"
    );
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!app.filtering, "Enter ends the filter");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Messages, "and then opens the chat");
}

#[test]
fn the_global_shortcuts_work_from_the_message_box() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    ctrl(&mut app, 'k');
    assert!(
        app.focus == Focus::Chats && app.filtering,
        "^k finds a chat"
    );
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    app.focus = Focus::Editor;
    ctrl(&mut app, 'n');
    assert_eq!(app.cmdline.as_deref(), Some("new "), "^n starts a chat");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    ctrl(&mut app, 'p');
    assert_eq!(
        app.cmdline.as_deref(),
        Some(""),
        "^p opens the command panel"
    );
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
    assert_eq!(app.overlay, Overlay::Help(0));
    app.on_key(key('x'));
    assert_eq!(app.overlay, Overlay::None);
    assert!(
        app.editor.is_empty(),
        "the key that closed the help was not typed"
    );
    press(&mut app, KeyCode::F(5), KeyModifiers::NONE);
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, _)| m.contains("refreshing"))
    );
}

#[test]
fn copy_cut_and_paste_use_the_system_clipboard() {
    let (mut app, fakes) = standard_app();
    app.focus = Focus::Editor;
    type_text(&mut app, "hello");
    ctrl(&mut app, 'a');
    ctrl(&mut app, 'c');
    assert_eq!(fakes.clipboard.get().as_deref(), Some("hello"));
    assert_eq!(app.editor.text(), "hello", "copy leaves the text");
    fakes.clipboard.put("");
    ctrl(&mut app, 'x');
    assert_eq!(fakes.clipboard.get().as_deref(), Some("hello"));
    assert!(app.editor.is_empty());
    fakes.clipboard.put("from elsewhere");
    type_text(&mut app, "x ");
    ctrl(&mut app, 'v');
    assert_eq!(app.editor.text(), "x from elsewhere");
    ctrl(&mut app, 'z');
    assert_eq!(app.editor.text(), "x ", "a paste is one step of undo");
    press(&mut app, KeyCode::Insert, KeyModifiers::SHIFT);
    assert_eq!(
        app.editor.text(),
        "x from elsewhere",
        "Shift-Insert pastes too"
    );
}

#[test]
fn pasting_with_an_empty_clipboard_says_so() {
    let (mut app, fakes) = standard_app();
    app.focus = Focus::Editor;
    fakes.clipboard.put("");
    ctrl(&mut app, 'v');
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.contains("clipboard"))
    );
    assert!(app.editor.is_empty());
}

#[test]
fn the_conversation_has_copy_quote_and_links_on_control_keys() {
    let (mut app, fakes) = standard_app();
    app.focus = Focus::Messages;
    let last = app.state().map(|s| s.messages().len() - 1).unwrap();
    app.state_mut().unwrap().sel = last;
    crate::ui::render_to_string(&mut app, 120, 40);
    ctrl(&mut app, 'c');
    assert!(
        fakes.clipboard.get().is_some_and(|t| t.contains("PR #42")),
        "^c copies the selected message"
    );
    ctrl(&mut app, 'o');
    pump(&mut app, |_| !fakes.opener.opened().is_empty());
    assert_eq!(fakes.opener.opened(), ["https://example.com/pr/42"]);
    ctrl(&mut app, 'r');
    assert_eq!(app.focus, Focus::Editor);
    assert!(app.is_replying(), "^r marks a reply pending");
    assert_eq!(
        app.editor.text(),
        "",
        "nothing is pre-filled; the backend builds the quote once it is sent"
    );
    assert_eq!(app.editor.mode(), Mode::Insert);
}

#[test]
fn escape_goes_back_one_pane_at_a_time() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    type_text(&mut app, "draft");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Messages);
    assert_eq!(app.editor.text(), "draft", "leaving keeps the draft");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Chats);
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Editor, "tab goes forward");
}

#[test]
fn vims_pane_keys_are_not_taken_in_the_standard_keymap() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    type_text(&mut app, "ab cd");
    ctrl(&mut app, 'h');
    assert_eq!(app.focus, Focus::Editor, "^h is not 'go left'");
    assert_eq!(
        app.editor.text(),
        "ab ",
        "it deletes a word, as ^backspace does in terminals"
    );
    ctrl(&mut app, 'l');
    assert_eq!(app.focus, Focus::Editor);
    ctrl(&mut app, 'w');
    assert!(!app.ctrl_w, "^w is not a window prefix");
}

#[test]
fn a_new_app_uses_the_standard_keymap() {
    let (app, _) = App::for_test(Arc::new(DemoBackend::instant()));
    assert_eq!(app.keymap, Keymap::Standard);
    assert_eq!(
        (app.editor.mode(), app.keymap),
        (Mode::Insert, Keymap::Standard)
    );
}

#[test]
fn the_vim_keymap_keeps_its_keys_and_gains_only_f1() {
    let (mut app, _) = app_and_fakes();
    app.on_key(key('i'));
    ctrl(&mut app, 'c');
    assert_eq!(app.editor.mode(), Mode::Normal, "^c is still Esc in vim");
    ctrl(&mut app, 'n');
    assert!(app.cmdline.is_none(), "^n is not 'new chat' in vim");
    press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
    assert_eq!(app.overlay, Overlay::Help(0));
}

#[test]
fn help_hints_and_prompts_follow_the_keymap() {
    let (mut standard, _) = standard_app();
    standard.focus = Focus::Editor;
    let screen = crate::ui::render_to_string(&mut standard, 150, 40);
    assert!(
        screen.contains("shift+⏎") && screen.contains("MESSAGE"),
        "{screen}"
    );
    assert!(!screen.contains("INSERT") && !screen.contains("NORMAL"));
    press(&mut standard, KeyCode::F(1), KeyModifiers::NONE);
    let help = crate::ui::render_to_string(&mut standard, 150, 50);
    assert!(
        help.contains("Message box") && help.contains("Everywhere"),
        "{help}"
    );
    assert!(!help.contains("Editor (vim)"));
    standard.on_key(key('x'));
    standard.focus = Focus::Chats;
    let chats = crate::ui::render_to_string(&mut standard, 150, 40);
    assert!(
        chats.contains("new chat") && chats.contains("^k"),
        "{chats}"
    );

    let (mut vim, _) = app_and_fakes();
    press(&mut vim, KeyCode::F(1), KeyModifiers::NONE);
    let help = crate::ui::render_to_string(&mut vim, 150, 50);
    assert!(
        help.contains("Editor (vim)") && !help.contains("Message box"),
        "{help}"
    );
}
// -- moving between panes with the arrow keys ------------------------------------------

#[test]
fn the_arrow_keys_walk_between_the_panes_in_the_standard_keymap() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Chats;
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Messages, "Right opens the conversation");
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Editor, "and goes on to the message box");
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(
        app.focus,
        Focus::Messages,
        "Left leaves an empty message box"
    );
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Chats, "and the conversation");
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(
        app.focus,
        Focus::Chats,
        "the chat list is the end of the line"
    );
}

#[test]
fn left_edits_a_message_that_has_text_instead_of_leaving_it() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    type_text(&mut app, "ab");
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(app.editor.cursor(), (0, 1), "it moved the cursor");
    press(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Editor, "still the message box");
    // A modifier never counts as navigation, even on an empty box.
    app.editor.clear();
    press(&mut app, KeyCode::Left, KeyModifiers::CONTROL);
    assert_eq!(app.focus, Focus::Editor);
}

#[test]
fn right_goes_from_the_conversation_to_the_message_box_in_the_vim_keymap_too() {
    let (mut app, _) = app_and_fakes();
    app.select_chat("demo-ada");
    app.focus = Focus::Chats;
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Messages);
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(
        app.editor.mode(),
        Mode::Normal,
        "vim keeps its modes: this only moved the focus"
    );
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(
        app.focus,
        Focus::Editor,
        "in vim Left is a motion, not navigation"
    );
}

#[test]
fn the_footer_tells_you_about_the_arrows() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Chats;
    let chats = crate::ui::render_to_string(&mut app, 150, 36);
    assert!(chats.contains("→ ⏎  open"), "{chats}");
    app.focus = Focus::Messages;
    let conversation = crate::ui::render_to_string(&mut app, 150, 36);
    assert!(
        conversation.contains("→ ⏎  write") && conversation.contains("←  chats"),
        "{conversation}"
    );
}
// -- the emoji selector --------------------------------------------------------------

fn emoji_overlay(app: &App) -> Option<(&EmojiTarget, &str, usize)> {
    match &app.overlay {
        Overlay::Emoji { target, query, sel } => Some((target, query.as_str(), *sel)),
        _ => None,
    }
}

fn select_last_message(app: &mut App) {
    let last = app.state().map(|s| s.messages().len() - 1).unwrap();
    app.state_mut().unwrap().sel = last;
}

fn reactions_of_last_message(app: &App) -> Vec<(String, usize)> {
    last_message(app)
        .reactions
        .iter()
        .map(|r| (r.emoji.clone(), r.count))
        .collect()
}

#[test]
fn choosing_an_emoji_in_the_conversation_reacts_at_once_and_choosing_it_again_takes_it_back() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Messages;
    select_last_message(&mut app);
    ctrl(&mut app, 'e');
    assert!(matches!(
        emoji_overlay(&app),
        Some((EmojiTarget::React { .. }, "", 0))
    ));
    // Only the classic six until the service's own list has arrived.
    pump(&mut app, |a| a.reactions.catalog_loaded());
    type_text(&mut app, "tada");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(
        reactions_of_last_message(&app),
        [("🎉".to_string(), 1)],
        "shown before the service answers"
    );
    pump(&mut app, |a| {
        a.status
            .as_ref()
            .is_some_and(|(m, _, err)| !err && m.contains("reacted"))
    });
    // The next read from the service still has it.
    app.run_command("refresh");
    pump(&mut app, |a| a.state().is_some_and(|s| !s.is_fetching()));
    assert_eq!(reactions_of_last_message(&app), [("🎉".to_string(), 1)]);

    ctrl(&mut app, 'e');
    let Some((EmojiTarget::React { mine, .. }, ..)) = emoji_overlay(&app) else {
        panic!("no selector");
    };
    assert_eq!(
        mine,
        &["🎉".to_string()],
        "the selector knows which one is yours"
    );
    type_text(&mut app, "tada");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(reactions_of_last_message(&app).is_empty(), "taken back");
    pump(&mut app, |a| {
        a.status
            .as_ref()
            .is_some_and(|(m, _, _)| m.contains("took back"))
    });
}

#[test]
fn a_second_person_s_reaction_is_kept_when_yours_is_added_and_removed() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Messages;
    // The demo's first message from Ada carries a thumbs-up from one other person.
    let with_reaction = app
        .state()
        .unwrap()
        .messages()
        .iter()
        .position(|m| m.reactions.iter().any(|r| r.emoji == "👍"))
        .expect("a message with a reaction");
    app.state_mut().unwrap().sel = with_reaction;
    let count = |app: &App| app.state().unwrap().messages()[with_reaction].reactions[0].count;
    let before = count(&app);
    ctrl(&mut app, 'e');
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // the first emoji is the thumbs-up
    assert_eq!(count(&app), before + 1);
    pump(&mut app, |a| !a.reactions.has_pending());
    ctrl(&mut app, 'e');
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(count(&app), before, "only yours was taken back");
}

#[test]
fn reactions_offer_only_what_the_service_accepts_but_a_message_can_hold_any_emoji() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Messages;
    select_last_message(&mut app);
    ctrl(&mut app, 'e');
    pump(&mut app, |a| a.reactions.catalog_loaded());
    type_text(&mut app, "pizza");
    let Overlay::Emoji { target, query, .. } = app.overlay.clone() else {
        panic!("no selector");
    };
    assert!(
        emoji::search(app.emoji_entries(&target), &query).is_empty(),
        "the service has no reaction for it"
    );
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    app.focus = Focus::Editor;
    ctrl(&mut app, 'e');
    type_text(&mut app, "pizza");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.editor.text(), "🍕", "but it can be written");
}

/// A service list as long as Teams' own (over a thousand entries) with a turkey and a tree far down it.
fn long_service_list() -> Vec<teams_client::EmojiInfo> {
    (0..1_500u32)
        .map(|n| teams_client::EmojiInfo {
            emoji: match n {
                550 => "🦃".to_string(),
                1_250 => "🎄".to_string(),
                _ => char::from_u32(0x2_0000 + n).unwrap().to_string(),
            },
            name: match n {
                550 => "Turkey".to_string(),
                1_250 => "Xmas tree".to_string(),
                _ => format!("Thing {n}"),
            },
            aliases: if n == 1_250 {
                vec!["christmas".to_string()]
            } else {
                Vec::new()
            },
        })
        .collect()
}

#[test]
fn a_long_emoji_list_says_how_long_it_is_pages_with_pgup_and_pgdn_and_is_searched_by_name() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Messages;
    select_last_message(&mut app);
    ctrl(&mut app, 'e');
    pump(&mut app, |a| a.reactions.catalog_loaded());
    app.reactions.receive_catalog(long_service_list());
    let total = emoji_total(&app);
    assert_eq!(total, 6 + 1_500);

    let screen = crate::ui::render_to_string(&mut app, 100, 40);
    assert!(
        screen.contains(&format!("React · {total}")),
        "the title counts the list:\n{screen}"
    );

    // A window at a time, to the ends, and back.
    let page = EMOJI_COLUMNS * crate::app::EMOJI_ROWS;
    let at = |app: &App| emoji_overlay(app).map(|(_, _, sel)| sel).unwrap();
    press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    assert_eq!(at(&app), page);
    press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    assert_eq!(at(&app), 2 * page);
    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!(at(&app), page);
    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!(at(&app), 0, "not before the first");
    for _ in 0..total / page + 2 {
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    }
    assert_eq!(at(&app), total - 1, "not past the last");

    // Searching is how a particular one is found, by its name or one of its words.
    press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    for (word, emoji) in [("turkey", "🦃"), ("christmas", "🎄"), ("xmas", "🎄")] {
        type_text(&mut app, word);
        let screen = crate::ui::render_to_string(&mut app, 100, 40);
        assert!(
            screen.contains(&format!("React · 1 of {total}")) && screen.contains(emoji),
            "{word}:\n{screen}"
        );
        for _ in 0..word.len() {
            press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        }
    }
    let screen = crate::ui::render_to_string(&mut app, 100, 40);
    assert!(screen.contains(&format!("React · {total}")), "{screen}");
}

#[test]
fn the_message_box_picker_loads_and_offers_the_same_list_as_reacting_so_a_turkey_can_be_written() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    // Opened only to write: until the service's list arrives it is the shortcode table, and opening it asks for the list.
    ctrl(&mut app, 'e');
    let table = emoji_total(&app);
    assert_eq!(table, crate::emoji::catalog().len());
    pump(&mut app, |a| a.reactions.catalog_loaded());
    app.reactions.receive_catalog(long_service_list());

    // The same list as reacting, with the table's other emoji after it.
    let (target, ..) = emoji_overlay(&app).unwrap();
    let writing: Vec<String> = app
        .emoji_entries(target)
        .iter()
        .map(|e| e.emoji.clone())
        .collect();
    let reacting: Vec<String> = app
        .reactions
        .entries()
        .iter()
        .map(|e| e.emoji.clone())
        .collect();
    assert!(
        writing.len() > reacting.len(),
        "and the shortcodes that have no reaction"
    );
    assert_eq!(writing[..reacting.len()], reacting[..]);
    let screen = crate::ui::render_to_string(&mut app, 100, 40);
    assert!(
        screen.contains(&format!("Insert emoji · {}", writing.len())),
        "{screen}"
    );

    // Found by name or keyword, and written.
    type_text(&mut app, "turkey");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.editor.text(), "🦃");
    ctrl(&mut app, 'e');
    type_text(&mut app, "christmas");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.editor.text(), "🦃🎄");
}

#[test]
fn a_list_that_cannot_be_had_is_only_worth_a_word_to_someone_reacting() {
    let (mut app, _) = standard_app();
    // Writing a message: the shortcode table is there, so a failed load says nothing.
    app.focus = Focus::Editor;
    ctrl(&mut app, 'e');
    app.status = None;
    app.on_event(Ev::ReactionEmoji(Err(Error::Unsupported {
        operation: "list reaction emoji",
        detail: "this backend has no list of emoji",
    })));
    assert!(app.status.is_none(), "{:?}", app.status);
    assert_eq!(emoji_total(&app), crate::emoji::catalog().len());
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    // Reacting: only the classic six are left, and that is said.
    app.focus = Focus::Messages;
    select_last_message(&mut app);
    ctrl(&mut app, 'e');
    app.on_event(Ev::ReactionEmoji(Err(Error::Unsupported {
        operation: "list reaction emoji",
        detail: "this backend has no list of emoji",
    })));
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.contains("only the classic reactions")),
        "{:?}",
        app.status
    );
}

fn emoji_total(app: &App) -> usize {
    let (target, ..) = emoji_overlay(app).expect("the selector is open");
    app.emoji_entries(target).len()
}

#[test]
fn the_classic_six_are_offered_at_once_and_the_service_list_replaces_them_when_it_arrives() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Messages;
    select_last_message(&mut app);
    assert_eq!(app.reactions.entries().len(), 6);
    ctrl(&mut app, 'e');
    let Overlay::Emoji { target, .. } = app.overlay.clone() else {
        panic!("no selector");
    };
    assert_eq!(app.emoji_entries(&target).len(), 6);
    pump(&mut app, |a| a.reactions.catalog_loaded());
    assert!(app.emoji_entries(&target).len() > 6);
    assert_eq!(app.emoji_entries(&target)[0].emoji, "👍", "classic first");
}
#[test]
fn mail_links_open_without_the_senders_parameters_and_odd_links_are_refused() {
    let rec = opener::Recorder::default();
    open_url(
        &rec,
        "mailto:ada@example.com?subject=hi&attach=C:\\secret.txt",
    )
    .unwrap();
    open_url(&rec, "https://example.com/a?b=c#d").unwrap();
    assert_eq!(
        rec.opened(),
        ["mailto:ada@example.com", "https://example.com/a?b=c#d"]
    );
    for bad in [
        "https://example.com/a b",
        "https://example.com/\u{1b}[2J",
        "file:///etc/passwd",
        "javascript:alert(1)",
    ] {
        assert!(open_url(&rec, bad).is_err(), "{bad:?}");
    }
    assert_eq!(rec.opened().len(), 2);
}
#[test]
fn a_message_still_being_sent_cannot_be_reacted_to_and_neither_can_one_with_a_reaction_in_flight() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Messages;
    select_last_message(&mut app);
    let id = app.current.clone().unwrap();
    // A message of ours that the service has not confirmed yet.
    let last = app.states[&id].messages().len() - 1;
    app.state_mut()
        .unwrap()
        .edit_messages(|msgs| msgs[last].delivery = Delivery::Sending);
    ctrl(&mut app, 'e');
    assert_eq!(app.overlay, Overlay::None);
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.contains("sent message"))
    );
    app.state_mut()
        .unwrap()
        .edit_messages(|msgs| msgs[last].delivery = Delivery::Sent);

    // One reaction at a time per message: the second waits for the answer to the first.
    ctrl(&mut app, 'e');
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    ctrl(&mut app, 'e');
    assert_eq!(app.overlay, Overlay::None, "the first is still in flight");
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.contains("still being sent"))
    );
    pump(&mut app, |a| !a.reactions.has_pending());
    ctrl(&mut app, 'e');
    assert!(matches!(app.overlay, Overlay::Emoji { .. }));
}

#[test]
fn who_reacted_is_compared_without_regard_to_case() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Messages;
    select_last_message(&mut app);
    let id = app.current.clone().unwrap();
    let me = app.me_id().to_string();
    let last = app.states[&id].messages().len() - 1;
    app.state_mut().unwrap().edit_messages(|msgs| {
        msgs[last].reactions = vec![Reaction {
            emoji: "👍".into(),
            count: 1,
            users: vec![me.to_uppercase()],
            names: vec!["You".into()],
        }]
    });
    ctrl(&mut app, 'e');
    let Some((EmojiTarget::React { mine, .. }, ..)) = emoji_overlay(&app) else {
        panic!("no selector");
    };
    assert_eq!(
        mine,
        &["👍".to_string()],
        "the service''s spelling of my id differs only in case"
    );
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE); // takes it back
    assert!(reactions_of_last_message(&app).is_empty());
}

#[test]
fn a_manual_refresh_does_not_ask_a_service_that_said_to_wait() {
    let mut app = loaded_app();
    app.on_event(Ev::Chats(Err(throttled(Some(60)))));
    app.run_command("refresh");
    assert!(!app.chats.is_fetching(), "nothing was fetched");
    let status = app.status.clone().map(|(m, _, _)| m).unwrap_or_default();
    assert!(status.contains("asked us to wait"), "{status}");
}

#[test]
fn scrolling_back_in_a_chat_the_service_asked_us_to_wait_on_does_not_ask_it_again() {
    let mut app = loaded_app();
    let id = app.current.clone().expect("a chat is open");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.states
        .get_mut(&id)
        .unwrap()
        .sched
        .failed(Some(Duration::from_secs(60)));
    // Someone asked for older messages: refused, and told why.
    assert!(!app.load_messages(&id, true));
    let status = app.status.take().map(|(m, _, _)| m).unwrap_or_default();
    assert!(status.contains("asked us to wait"), "{status}");
    // What nobody asked for is refused quietly.
    assert!(!app.start_message_fetch(&id, Fetch::Older { automatic: true }));
    assert!(!app.start_message_fetch(&id, Fetch::Newest { quiet: true }));
    assert!(app.status.is_none(), "{:?}", app.status);
    // Once the wait is over it works again.
    app.states.get_mut(&id).unwrap().sched.succeeded();
    assert!(app.start_message_fetch(&id, Fetch::Newest { quiet: false }));
}

#[test]
fn a_chat_list_that_failed_to_load_the_first_time_is_tried_again_on_its_schedule() {
    let (mut app, _) = App::for_test(Arc::new(DemoBackend::instant()));
    app.on_event(Ev::Chats(Err(scripted_failure())));
    assert!(!app.chats.loaded());
    app.chats.sched.set_last(ago(10 * 60));
    app.on_tick();
    assert!(app.chats.is_fetching(), "a retry was started");
}

#[test]
fn a_failed_first_fetch_of_who_is_signed_in_is_retried_on_its_schedule() {
    // Without `me` known, `me_id()` is empty, which silently breaks reactions, editing and mention/self checks for
    // the rest of the session (see `App::fetch_me`): a flaky first answer must not be left unretried.
    let (mut app, _) = App::for_test(Arc::new(DemoBackend::instant()));
    app.on_event(Ev::Me(Err(scripted_failure())));
    assert!(app.me.is_none());
    assert!(
        !app.me_fetching,
        "the failed attempt cleared its in-flight flag"
    );
    app.on_tick();
    assert!(!app.me_fetching, "too soon to retry");
    app.me_sched.set_last(ago(10 * 60));
    app.on_tick();
    assert!(app.me_fetching, "a retry was started");
    app.on_tick(); // the demo backend's answer, queued by the retry, is drained on the next tick
    assert!(app.me.is_some());
    assert!(!app.me_fetching);
}

#[test]
fn automatic_paging_back_waits_for_the_current_request_and_a_failure_releases_it() {
    let mut app = open_eventful(usize::MAX);
    let id = app.current.clone().unwrap();
    app.states.get_mut(&id).unwrap().older = Some("next".into());
    let manual = Fetch::Older { automatic: false };
    assert!(app.start_message_fetch(&id, manual));
    assert!(!app.start_message_fetch(&id, Fetch::Older { automatic: true }));
    app.on_event(Ev::Messages {
        chat_id: id.clone(),
        fetch: manual,
        result: Err(scripted_failure()),
    });
    assert!(!app.states[&id].is_fetching());
}

fn demo_app() -> (App, Arc<DemoBackend>) {
    let demo = Arc::new(DemoBackend::instant());
    let (mut app, _) = App::for_test(demo.clone());
    app.set_keymap(Keymap::Standard);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    (app, demo)
}

fn ticks(app: &mut App, n: usize) {
    for _ in 0..n {
        app.on_tick();
    }
}

#[test]
fn a_push_hint_for_the_open_chat_makes_it_fetch_sooner_than_its_own_schedule_would() {
    let (mut app, demo) = demo_app();
    let open = app.current.clone().unwrap();
    // Nothing due on its own: both schedules were just touched.
    app.states
        .get_mut(&open)
        .unwrap()
        .sched
        .set_last(Instant::now());
    app.chats.sched.set_last(Instant::now());
    demo.queue_push(PushHint::Chat(open.clone()));
    app.on_tick();
    assert!(
        app.states[&open].is_fetching(),
        "the named chat fetches at once"
    );
    assert!(!app.chats.is_fetching(), "the list was left alone");
}

#[test]
fn a_push_hint_for_a_chat_not_open_this_session_makes_the_list_fetch_sooner() {
    let (mut app, demo) = demo_app();
    let open = app.current.clone().unwrap();
    app.states
        .get_mut(&open)
        .unwrap()
        .sched
        .set_last(Instant::now());
    app.chats.sched.set_last(Instant::now());
    demo.queue_push(PushHint::Chat("not-open-this-session".to_string()));
    app.on_tick();
    assert!(
        !app.states[&open].is_fetching(),
        "the open chat was left alone"
    );
    assert!(
        app.chats.is_fetching(),
        "the list fetches instead, since the chat has no state of its own yet"
    );
}

#[test]
fn an_unknown_push_hint_makes_both_the_open_chat_and_the_list_fetch_sooner() {
    let (mut app, demo) = demo_app();
    let open = app.current.clone().unwrap();
    app.states
        .get_mut(&open)
        .unwrap()
        .sched
        .set_last(Instant::now());
    app.chats.sched.set_last(Instant::now());
    demo.queue_push(PushHint::Unknown);
    app.on_tick();
    assert!(app.states[&open].is_fetching());
    assert!(app.chats.is_fetching());
}

#[test]
fn a_chat_that_is_read_is_marked_read_on_the_service_once_per_newest_message() {
    let (mut app, demo) = demo_app();
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    ticks(&mut app, 5);
    let newest = app.state().unwrap().messages().last().unwrap().id.clone();
    assert_eq!(demo.read_marks(), [("demo-ada".to_string(), newest)]);
    ticks(&mut app, 5);
    assert_eq!(demo.read_marks().len(), 1, "not again for the same message");

    // Something new arrives while the chat is open: it is read too.
    let id = app.current.clone().unwrap();
    app.on_event(Ev::Messages {
        chat_id: id,
        fetch: Fetch::Newest { quiet: false },
        result: Ok(Page {
            messages: vec![msg("9999", 0)],
            older: None,
        }),
    });
    ticks(&mut app, 3);
    let marks = demo.read_marks();
    assert_eq!(marks.len(), 2);
    assert_eq!(marks[1].1, "9999");
}

#[test]
fn nothing_is_marked_read_while_the_terminal_is_in_the_background_or_the_person_is_away() {
    let (mut app, demo) = demo_app();
    app.set_focused(false);
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    ticks(&mut app, 5);
    assert!(
        demo.read_marks().is_empty(),
        "the terminal is in the background"
    );
    app.set_focused(true);
    app.last_input = ago(2 * 60 * 60);
    ticks(&mut app, 5);
    assert!(
        demo.read_marks().is_empty(),
        "no one has touched it for two hours"
    );
    app.last_input = Instant::now();
    ticks(&mut app, 3);
    assert_eq!(demo.read_marks().len(), 1);
}

#[test]
fn a_failed_mark_is_not_retried_at_once_and_does_not_bother_the_user() {
    let (mut app, demo) = demo_app();
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    ticks(&mut app, 3);
    assert_eq!(demo.read_marks().len(), 1);
    app.status = None;
    app.on_event(Ev::MarkedRead {
        chat_id: "demo-ada".into(),
        result: Err(scripted_failure()),
    });
    assert!(app.status.is_none(), "logged, not shown");
    let id = app.current.clone().unwrap();
    app.on_event(Ev::Messages {
        chat_id: id,
        fetch: Fetch::Newest { quiet: false },
        result: Ok(Page {
            messages: vec![msg("9998", 0)],
            older: None,
        }),
    });
    ticks(&mut app, 5);
    assert_eq!(demo.read_marks().len(), 1, "the pause is not over");
    app.read.set_paused_until(ago(1));
    ticks(&mut app, 3);
    assert_eq!(demo.read_marks().len(), 2);
}

#[test]
fn a_chat_the_service_already_has_as_read_is_not_marked_again() {
    let (mut app, demo) = demo_app();
    // The service's read marker is already past everything in the chat.
    app.chats.edit_items(|items| {
        items
            .iter_mut()
            .find(|c| c.id == "demo-ada")
            .unwrap()
            .last_read = Some(Utc::now() + chrono::Duration::days(1));
    });
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    ticks(&mut app, 5);
    assert!(demo.read_marks().is_empty());
}
#[test]
fn the_help_can_be_scrolled_on_a_short_terminal_and_any_other_key_closes_it() {
    let (mut app, _) = standard_app();
    crate::ui::render_to_string(&mut app, 120, 24);
    press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
    let top = crate::ui::render_to_string(&mut app, 120, 24);
    assert!(
        top.contains("Everywhere") && !top.contains("Message box"),
        "{top}"
    );
    press(&mut app, KeyCode::End, KeyModifiers::NONE);
    let bottom = crate::ui::render_to_string(&mut app, 120, 24);
    assert!(
        bottom.contains("Message box") && bottom.contains("any other key closes it"),
        "{bottom}"
    );
    assert!(!bottom.contains("Everywhere"));
    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    assert_eq!(app.overlay, Overlay::Help(0));
    // The scroll position cannot run past the end.
    let most =
        crate::ui::help_lines(Keymap::Standard).len() - crate::ui::help_rows(Keymap::Standard, 24);
    for _ in 0..100 {
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    }
    assert_eq!(app.overlay, Overlay::Help(most));
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.overlay, Overlay::Help(most - 1));
    press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
    assert_eq!(app.overlay, Overlay::None);
}
#[test]
fn the_emoji_picker_keeps_the_selected_emoji_in_view_on_a_short_terminal() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    ctrl(&mut app, 'e');
    // Go down to the last emoji of the list; the grid has far more rows than a 14-row terminal can show.
    press(&mut app, KeyCode::End, KeyModifiers::NONE);
    let last = emoji::catalog().last().unwrap().emoji.clone();
    let screen = crate::ui::render_to_string(&mut app, 100, 14);
    assert!(
        screen.contains(&last),
        "the selected emoji is on screen:\n{screen}"
    );
    assert!(screen.contains("type to search"), "{screen}");
}
#[test]
fn an_emoji_with_a_presentation_selector_keeps_it_when_the_message_box_is_drawn() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    app.editor.set_text("a ☝\u{FE0F} ❤\u{FE0F} 👨\u{200D}💻 b");
    crate::ui::render_to_string(&mut app, 100, 30);
    let drawn: String = app
        .view
        .editor_rows
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(
        drawn.contains("☝\u{FE0F}")
            && drawn.contains("❤\u{FE0F}")
            && drawn.contains("👨\u{200D}💻"),
        "{drawn:?}"
    );
}
/// A service whose sign-in has lapsed: the lists fail until `sign_in_again` succeeds.
struct Lapsed {
    inner: DemoBackend,
    signed_in: std::sync::atomic::AtomicBool,
    can_sign_in_again: bool,
    sign_in_works: bool,
    fetches: std::sync::atomic::AtomicUsize,
    /// The presence service refuses what it is asked (a tenant that does not grant its scope), whatever the sign-in.
    presence_refused: std::sync::atomic::AtomicBool,
}

impl Lapsed {
    fn new(can_sign_in_again: bool, sign_in_works: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: DemoBackend::instant(),
            signed_in: std::sync::atomic::AtomicBool::new(true),
            can_sign_in_again,
            sign_in_works,
            fetches: std::sync::atomic::AtomicUsize::new(0),
            presence_refused: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn refuse_presence(&self) {
        self.presence_refused
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The service stops accepting the sign-in.
    fn lapse(&self) {
        self.signed_in
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    fn fetches(&self) -> usize {
        self.fetches.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn gate(&self) -> Result<(), Error> {
        self.fetches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.signed_in.load(std::sync::atomic::Ordering::SeqCst) {
            Ok(())
        } else {
            Err(Error::Auth(teams_client::AuthError::NotSignedIn))
        }
    }
}

impl Backend for Lapsed {
    fn label(&self) -> String {
        self.inner.label()
    }
    fn me(&self) -> Result<Me, Error> {
        self.inner.me()
    }
    fn chats(&self) -> Result<Vec<Chat>, Error> {
        self.gate()?;
        self.inner.chats()
    }
    fn find_people(&self, q: &str) -> Result<Vec<Contact>, Error> {
        self.inner.find_people(q)
    }
    fn open_chat(&self, who: &Contact) -> Result<Chat, Error> {
        self.inner.open_chat(who)
    }
    fn messages(&self, id: &str, older: Option<&str>) -> Result<Page, Error> {
        self.gate()?;
        self.inner.messages(id, older)
    }
    fn send(
        &self,
        id: &str,
        html: &str,
        m: &[teams_client::OutMention],
        reply_to: Option<&teams_client::ReplyTarget>,
    ) -> Result<Msg, Error> {
        self.inner.send(id, html, m, reply_to)
    }
    fn can_presence(&self) -> bool {
        self.presence_refused
            .load(std::sync::atomic::Ordering::SeqCst)
    }
    fn can_set_status(&self) -> bool {
        self.presence_refused
            .load(std::sync::atomic::Ordering::SeqCst)
    }
    fn set_status(&self, _: Option<teams_client::Availability>) -> Result<(), Error> {
        Err(Error::Auth(teams_client::AuthError::NotSignedIn))
    }
    fn report_activity(&self, _: bool) -> Result<(), Error> {
        Err(Error::Auth(teams_client::AuthError::NotSignedIn))
    }
    fn presence(
        &self,
        _ids: &[String],
    ) -> Result<std::collections::HashMap<String, teams_client::Presence>, Error> {
        // What a 401 for a scope the tenant does not grant looks like to the app.
        Err(Error::Auth(teams_client::AuthError::NotSignedIn))
    }
    fn can_sign_in_again(&self) -> bool {
        self.can_sign_in_again
    }
    fn sign_in_again(&self) -> Result<(), Error> {
        if self.sign_in_works {
            self.signed_in
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        } else {
            Err(Error::Auth(teams_client::AuthError::TimedOut))
        }
    }
    fn serves_image(&self, url: &str) -> bool {
        self.inner.serves_image(url)
    }
    fn image(&self, url: &str) -> Result<RgbImage, Error> {
        self.inner.image(url)
    }
}

/// A service whose list names the people of a group or meeting chat only when asked, as Teams does for a chat that has a
/// name of its own.
struct Unrostered {
    inner: DemoBackend,
    asked: std::sync::atomic::AtomicUsize,
}

impl Backend for Unrostered {
    fn label(&self) -> String {
        self.inner.label()
    }
    fn me(&self) -> Result<Me, Error> {
        self.inner.me()
    }
    fn chats(&self) -> Result<Vec<Chat>, Error> {
        let mut chats = self.inner.chats()?;
        for chat in &mut chats {
            if chat.kind != teams_client::ChatKind::OneOnOne && chat.topic.is_some() {
                chat.members.clear();
            }
        }
        Ok(chats)
    }
    fn members(&self, chat_id: &str) -> Result<Vec<teams_client::Member>, Error> {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let chats = self.inner.chats()?;
        chats
            .into_iter()
            .find(|c| c.id == chat_id)
            .map(|c| c.members)
            .ok_or(Error::Unsupported {
                operation: "read chat members",
                detail: "unknown chat",
            })
    }
    fn find_people(&self, q: &str) -> Result<Vec<Contact>, Error> {
        self.inner.find_people(q)
    }
    fn open_chat(&self, who: &Contact) -> Result<Chat, Error> {
        self.inner.open_chat(who)
    }
    fn messages(&self, id: &str, older: Option<&str>) -> Result<Page, Error> {
        self.inner.messages(id, older)
    }
    fn send(
        &self,
        id: &str,
        html: &str,
        m: &[teams_client::OutMention],
        reply_to: Option<&teams_client::ReplyTarget>,
    ) -> Result<Msg, Error> {
        self.inner.send(id, html, m, reply_to)
    }
    fn serves_image(&self, url: &str) -> bool {
        self.inner.serves_image(url)
    }
    fn image(&self, url: &str) -> Result<RgbImage, Error> {
        self.inner.image(url)
    }
}

#[test]
fn a_named_group_chat_shows_its_people_under_the_title_once_the_roster_is_asked_for() {
    let backend = Arc::new(Unrostered {
        inner: DemoBackend::instant(),
        asked: std::sync::atomic::AtomicUsize::new(0),
    });
    let (mut app, _) = App::for_test(backend.clone());
    app.set_keymap(Keymap::Standard);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.select_chat("demo-platform");
    let before = crate::ui::render_to_string(&mut app, 120, 30);
    assert!(
        !before.contains("4 people"),
        "no roster yet, so nothing to list:\n{before}"
    );
    pump(&mut app, |a| {
        a.current_chat().is_some_and(|c| !a.attendees(c).is_empty())
    });
    let screen = crate::ui::render_to_string(&mut app, 120, 30);
    let line = screen
        .lines()
        .find(|l| l.contains("people"))
        .expect("the attendee line");
    assert!(
        line.contains("Ada") && line.contains("4 people"),
        "{line}\n{screen}"
    );
    assert!(
        !line.contains(&app.me.as_ref().unwrap().name),
        "you are not listed among the others: {line}"
    );
    // Asked once, however many ticks follow, since everyone has a name.
    for _ in 0..10 {
        app.on_tick();
    }
    assert_eq!(backend.asked.load(std::sync::atomic::Ordering::SeqCst), 1);
}

fn lapsed_app(backend: &Arc<Lapsed>) -> App {
    lapsed_app_and_fakes(backend).0
}

fn lapsed_app_and_fakes(backend: &Arc<Lapsed>) -> (App, Fakes) {
    let (mut app, fakes) = App::for_test(backend.clone());
    app.set_keymap(Keymap::Standard);
    app.start();
    (app, fakes)
}

/// An app on the demo backend that has loaded its chats and who it is, with the bell in `mode`.
fn bell_app(mode: crate::bell::BellMode) -> App {
    let mut app = app();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.set_bell_mode(mode);
    assert!(!app.take_bell(), "what was there at the start is not news");
    app
}

/// The chat list as it is, with the newest message of one chat replaced by one that has just arrived from `from`.
fn list_with_a_new_message_from(app: &App, chat_id: &str, from: &str) -> Vec<Chat> {
    let mut items = app.chats.items().to_vec();
    let chat = items.iter_mut().find(|c| c.id == chat_id).expect("chat");
    let preview = chat.preview.as_mut().expect("a preview");
    preview.created = Utc::now() + chrono::Duration::minutes(1);
    preview.from_id = Some(from.to_string());
    preview.automated = false;
    items
}

#[test]
fn a_new_message_from_someone_else_rings_the_bell_once_and_your_own_does_not() {
    use crate::bell::BellMode;
    let mut app = bell_app(BellMode::Always);
    let list = list_with_a_new_message_from(&app, "demo-grace", "someone");
    app.on_event(Ev::Chats(Ok(list.clone())));
    assert!(app.take_bell());
    assert!(!app.take_bell(), "once");
    // The same list again is not news.
    app.on_event(Ev::Chats(Ok(list)));
    assert!(!app.take_bell());
    // Nor is a message you wrote yourself, on another device say.
    let own = list_with_a_new_message_from(&app, "demo-ada", app.me_id());
    app.on_event(Ev::Chats(Ok(own)));
    assert!(!app.take_bell());
}

#[test]
fn the_bell_is_silent_unless_asked_for_and_in_background_mode_only_while_the_terminal_is_behind() {
    use crate::bell::BellMode;
    let mut app = bell_app(BellMode::Off);
    let list = list_with_a_new_message_from(&app, "demo-grace", "someone");
    app.on_event(Ev::Chats(Ok(list)));
    assert!(!app.take_bell(), "off");

    let mut app = bell_app(BellMode::Background);
    app.set_focused(true);
    let list = list_with_a_new_message_from(&app, "demo-grace", "someone");
    app.on_event(Ev::Chats(Ok(list)));
    assert!(!app.take_bell(), "the terminal is in front");
    app.set_focused(false);
    let list = list_with_a_new_message_from(&app, "demo-ada", "someone");
    app.on_event(Ev::Chats(Ok(list)));
    assert!(app.take_bell(), "the terminal is behind other windows");
}

#[test]
fn a_new_message_in_the_open_chat_rings_when_its_poll_finds_it_and_a_chat_just_opened_does_not() {
    use crate::bell::BellMode;
    let mut app = bell_app(BellMode::Always);
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    let id = app.current.clone().expect("a chat is open");
    let page = |m: Msg| Ev::Messages {
        chat_id: id.clone(),
        fetch: Fetch::Newest { quiet: true },
        result: Ok(teams_client::Page {
            messages: vec![m],
            older: None,
        }),
    };
    // Someone else's message arrives.
    app.on_event(page(msg("new-from-them", -2)));
    assert!(app.take_bell());
    // The same one found again, and one of your own, do not ring.
    app.on_event(page(msg("new-from-them", -2)));
    let mut mine = msg("new-from-me", -3);
    mine.from_id = Some(app.me_id().to_string());
    app.on_event(page(mine));
    assert!(!app.take_bell());
    // A chat opened for the first time only sets the starting point, however recent its messages are.
    let other = app
        .chats
        .items()
        .iter()
        .map(|c| c.id.clone())
        .find(|c| *c != id)
        .expect("another chat");
    app.select_chat(&other);
    app.on_event(Ev::Messages {
        chat_id: other,
        fetch: Fetch::Newest { quiet: false },
        result: Ok(teams_client::Page {
            messages: vec![msg("just-arrived-before-i-looked", -1)],
            older: None,
        }),
    });
    assert!(!app.take_bell());
}

/// The text of a long single line that wraps over several rows of the message box.
fn long_line() -> String {
    "word ".repeat(70)
}

#[test]
fn up_and_down_walk_the_rows_of_one_long_wrapped_line_and_keep_the_column() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    app.editor.set_text(&long_line());
    crate::ui::render_to_string(&mut app, 100, 40);
    let spans = app.view.editor_rows.spans.clone();
    assert!(spans.len() >= 3, "the line wraps: {} rows", spans.len());
    assert!(
        spans.iter().all(|s| s.line == 0),
        "it is one line of the text"
    );
    let end = app.editor.cursor();
    assert_eq!(end.0, 0);
    let last = spans.len() - 1;

    // Up goes one drawn row, to the same column of it; the text has still only the one line.
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    let up = app.editor.cursor();
    assert_eq!(up.0, 0);
    let here_in_row = end.1 - spans[last].first;
    assert_eq!(
        up.1,
        spans[last - 1].first + here_in_row,
        "{end:?} -> {up:?}"
    );
    crate::ui::render_to_string(&mut app, 100, 40);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert!(app.editor.cursor().1 < up.1);
    // Back down to where it started (the column was kept), then once more: nowhere further, the end of the text.
    crate::ui::render_to_string(&mut app, 100, 40);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    crate::ui::render_to_string(&mut app, 100, 40);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.editor.cursor(), end);
    crate::ui::render_to_string(&mut app, 100, 40);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.editor.cursor(), end, "already on the last row");
    // Up from the first row goes to the start of the text, as it did.
    for _ in 0..last {
        crate::ui::render_to_string(&mut app, 100, 40);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    }
    crate::ui::render_to_string(&mut app, 100, 40);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.editor.cursor(), (0, 0));
    assert_eq!(app.editor.text(), long_line(), "nothing was typed");
}

#[test]
fn shift_with_up_or_down_selects_across_the_rows_of_a_wrapped_line() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    app.editor.set_text(&long_line());
    crate::ui::render_to_string(&mut app, 100, 40);
    let end = app.editor.cursor();
    press(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
    // From one row above (the same column) to the old cursor; the selection's end is inclusive.
    let (start, finish, linewise) = app.editor.selection().expect("a selection");
    assert!(!linewise);
    assert_eq!((start.0, finish.0), (0, 0));
    assert!(start.1 < end.1, "{start:?} {finish:?} {end:?}");
    assert_eq!(finish.1 + 1, end.1);
    // Plain Down ends the selection.
    crate::ui::render_to_string(&mut app, 100, 40);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert!(app.editor.selection().is_none());
}

#[test]
fn text_with_several_lines_still_moves_a_line_at_a_time_and_a_changed_text_falls_back_to_lines() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    app.editor
        .set_text("first line\nsecond line is longer\nthird");
    crate::ui::render_to_string(&mut app, 100, 40);
    assert_eq!(app.editor.cursor(), (2, 5));
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.editor.cursor(), (1, 5));
    crate::ui::render_to_string(&mut app, 100, 40);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.editor.cursor(), (0, 5));
    // Typing before a frame is drawn leaves the layout out of date: the editor moves by lines, and does not guess.
    app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.editor.cursor().0, 1);
}

#[test]
fn in_vim_the_arrows_in_insert_mode_and_j_k_in_normal_mode_walk_the_rows_too() {
    let (mut app, _) = app_and_fakes();
    app.focus = Focus::Editor;
    app.editor.set_text(&long_line());
    app.editor.set_mode(Mode::Insert);
    crate::ui::render_to_string(&mut app, 100, 40);
    let end = app.editor.cursor();
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    let up = app.editor.cursor();
    assert!(up.0 == 0 && up.1 < end.1, "{end:?} -> {up:?}");
    // Normal mode: k and j.
    app.editor.set_mode(Mode::Normal);
    crate::ui::render_to_string(&mut app, 100, 40);
    let normal_at = app.editor.cursor();
    app.on_key(key('k'));
    let k = app.editor.cursor();
    assert!(k.0 == 0 && k.1 < normal_at.1, "{normal_at:?} -> {k:?}");
    crate::ui::render_to_string(&mut app, 100, 40);
    app.on_key(key('j'));
    assert_eq!(
        app.editor.cursor(),
        normal_at,
        "j returns to the column k left"
    );
    // With an operator waiting, the motion is the editor's own, by lines: `dk` on the only line of the text has no
    // line above to take, and a row move would instead have deleted from the cursor to the row above.
    crate::ui::render_to_string(&mut app, 100, 40);
    app.on_key(key('d'));
    app.on_key(key('k'));
    assert_eq!(app.editor.text(), long_line());
}

/// An app on the demo backend whose first availability lookup is due.
fn presence_app() -> App {
    let mut app = app();
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.presences.sched.set_last(ago(10));
    app
}

#[test]
fn availability_is_asked_for_and_drawn_beside_one_to_one_chats_and_in_the_header() {
    let mut app = presence_app();
    pump(&mut app, |a| !a.presences.known.is_empty());
    let screen = crate::ui::render_to_string(&mut app, 120, 40);
    // The open chat is Ada's, who is available: in the header with words...
    let header = screen
        .lines()
        .find(|l| l.contains("Ada Lovelace") && l.contains("direct"))
        .unwrap_or_else(|| panic!("a header line:\n{screen}"));
    assert!(
        header.contains("🟢") && header.contains("Available"),
        "{header}"
    );
    // ...and in front of the name of each 1:1 chat in the list whose person the service said something about (the demo's
    // other people are in group chats, which have none). The header has the name first, so a line with the mark before
    // the name is a list row.
    for (mark, name) in [("🟢", "Ada Lovelace"), ("🔴", "Grace Hopper")] {
        let row = screen
            .lines()
            .find(|l| matches!((l.find(mark), l.find(name)), (Some(m), Some(n)) if m < n))
            .unwrap_or_else(|| panic!("{mark} in front of {name} in the list:\n{screen}"));
        assert!(
            row.find(mark).unwrap() < 40,
            "in the list, not the conversation: {row}"
        );
    }
    // Every name starts in the same column, with a mark or without (cells, so a wide mark counts for two: the dump
    // writes the second one as a space).
    let column = |name: &str| {
        screen
            .lines()
            .find(|l| l.find(name).is_some_and(|i| l[..i].chars().count() < 20))
            .map(|l| l[..l.find(name).unwrap()].chars().count())
            .unwrap_or_else(|| panic!("{name} in the list:\n{screen}"))
    };
    let with_mark = column("Ada Lovelace");
    assert_eq!(column("Grace Hopper"), with_mark);
    assert_eq!(
        column("Platform Team"),
        with_mark,
        "a group chat has a blank in the mark's place"
    );
    // A group chat has no one person to show.
    let chat = app
        .chats
        .items()
        .iter()
        .find(|c| c.kind == teams_client::ChatKind::Group);
    if let Some(group) = chat {
        assert!(app.presence_of(group).is_none());
    }
}

/// An app on a demo backend we keep a handle to, reporting that you are at the keyboard (`away_after`).
fn reporting_app(away_after: crate::status::AwayAfter) -> (App, Arc<DemoBackend>) {
    let backend = Arc::new(DemoBackend::instant());
    let (mut app, _) = App::for_test(backend.clone());
    app.set_keymap(Keymap::Standard);
    app.set_activity(true, away_after);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    (app, backend)
}

#[test]
fn being_at_the_keyboard_is_reported_and_going_idle_is_said_once_and_coming_back_again() {
    use crate::status::AwayAfter;
    use std::time::Duration;
    let (mut app, backend) = reporting_app(AwayAfter::Minutes(5));
    pump(&mut app, |_| backend.activity_reports() == [true]);
    // Nothing more while nothing changes.
    for _ in 0..20 {
        app.on_tick();
    }
    assert_eq!(backend.activity_reports(), [true]);
    // Ten minutes without a key: idle, said once.
    app.own.reporter.rewind(Duration::from_secs(10));
    app.last_input = ago(600);
    pump(&mut app, |_| backend.activity_reports() == [true, false]);
    app.own.reporter.rewind(Duration::from_secs(600));
    for _ in 0..20 {
        app.on_tick();
    }
    assert_eq!(
        backend.activity_reports(),
        [true, false],
        "idle is not repeated"
    );
    // A key: back.
    app.last_input = Instant::now();
    app.own.reporter.rewind(Duration::from_secs(10));
    pump(&mut app, |_| {
        backend.activity_reports() == [true, false, true]
    });
}

#[test]
fn never_away_reports_active_however_long_nothing_is_pressed() {
    use crate::status::AwayAfter;
    use std::time::Duration;
    let (mut app, backend) = reporting_app(AwayAfter::Never);
    app.last_input = ago(3 * 24 * 3600);
    pump(&mut app, |_| backend.activity_reports() == [true]);
    app.own.reporter.rewind(Duration::from_secs(130));
    pump(&mut app, |_| backend.activity_reports() == [true, true]);
    assert!(!backend.activity_reports().contains(&false));
}

#[test]
fn nothing_is_reported_when_it_is_switched_off_or_the_program_only_looks() {
    use crate::status::AwayAfter;
    let (mut app, backend) = reporting_app(AwayAfter::Minutes(5));
    pump(&mut app, |_| !backend.activity_reports().is_empty());
    let (mut off, backend_off) = reporting_app(AwayAfter::Minutes(5));
    off.set_activity(false, AwayAfter::Minutes(5));
    let (mut dump, backend_dump) = reporting_app(AwayAfter::Minutes(5));
    dump.read_only = true;
    for _ in 0..30 {
        off.on_tick();
        dump.on_tick();
    }
    // The first tick of `reporting_app` may have reported before the switch; what matters is that nothing grows.
    let before = (
        backend_off.activity_reports(),
        backend_dump.activity_reports(),
    );
    off.last_input = ago(3600);
    dump.last_input = ago(3600);
    for _ in 0..30 {
        off.on_tick();
        dump.on_tick();
    }
    assert_eq!(
        (
            backend_off.activity_reports(),
            backend_dump.activity_reports()
        ),
        before
    );
    assert!(!dump.can_set_status(), "a dump cannot set a status either");
}

#[test]
fn the_dialog_sets_a_status_which_is_shown_in_the_header_and_reset_gives_it_up() {
    use crate::status::AwayAfter;
    let (mut app, backend) = reporting_app(AwayAfter::Minutes(5));
    press(&mut app, KeyCode::F(9), KeyModifiers::NONE);
    assert!(matches!(app.overlay, Overlay::Status { .. }));
    let screen = crate::ui::render_to_string(&mut app, 120, 40);
    for expected in [
        "Set your status",
        "Do not disturb",
        "Reset status",
        "Away after 5 minutes",
        // The ends of the help lines, which a box too narrow for them cuts off.
        "8 or a: away time",
        "away_after keeps it",
    ] {
        assert!(screen.contains(expected), "{expected}:\n{screen}");
    }
    // The away time is the last row of the same list: the line right under "Reset status".
    let rows: Vec<&str> = screen.lines().collect();
    let reset = rows
        .iter()
        .position(|l| l.contains("Reset status"))
        .unwrap();
    assert!(rows[reset + 1].contains("Away after 5 minutes"), "{screen}");
    // Also in a small terminal, and with the longest timeout (a longer line).
    app.run_command("status away-after 1440");
    let narrow = crate::ui::render_to_string(&mut app, 80, 24);
    for expected in ["away_after keeps it", "Away after 1440 minutes"] {
        assert!(narrow.contains(expected), "{expected}:\n{narrow}");
    }
    app.run_command("status away-after 5");
    // Down to Busy, Enter.
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.overlay, Overlay::None);
    pump(&mut app, |_| {
        backend.chosen_status() == Some(teams_client::Availability::Busy)
    });
    pump(&mut app, |a| {
        a.own_presence()
            .is_some_and(|p| p.availability == teams_client::Availability::Busy)
    });
    let header = crate::ui::render_to_string(&mut app, 120, 40)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    assert!(
        header.contains("🔴"),
        "your own status is in the header: {header}"
    );
    // The command does the same, and the number keys jump.
    app.run_command("status dnd");
    pump(&mut app, |_| {
        backend.chosen_status() == Some(teams_client::Availability::DoNotDisturb)
    });
    app.run_command("status reset");
    pump(&mut app, |_| backend.chosen_status().is_none());
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, ..)| m.contains("reset")),
        "{:?}",
        app.status
    );
    press(&mut app, KeyCode::F(9), KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('2'), KeyModifiers::NONE);
    pump(&mut app, |_| {
        backend.chosen_status() == Some(teams_client::Availability::Busy)
    });
}

#[test]
fn the_away_time_is_stepped_in_the_dialog_without_leaving_it_or_setting_a_status() {
    use crate::status::AwayAfter::{Minutes, Never};
    let (mut app, backend) = reporting_app(Minutes(5));
    pump(&mut app, |_| !backend.activity_reports().is_empty());
    press(&mut app, KeyCode::F(9), KeyModifiers::NONE);
    // `a` steps it from anywhere in the dialog; the dialog stays open and no status is chosen.
    press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    assert_eq!(app.away_after(), Minutes(10));
    assert!(matches!(app.overlay, Overlay::Status { .. }));
    // Down to its row (the eighth), then the arrows change it either way.
    for _ in 0..7 {
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    }
    assert_eq!(app.overlay, Overlay::Status { sel: 7 });
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    assert_eq!(app.away_after(), Minutes(15));
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(app.away_after(), Minutes(5));
    // One step the short way round is never away; the dialog says so, and Enter on the row steps on (and wraps).
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(app.away_after(), Never);
    let screen = crate::ui::render_to_string(&mut app, 100, 30);
    assert!(screen.contains("Never away"), "{screen}");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.away_after(), Minutes(5));
    // `8` jumps to the row and steps it.
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    press(&mut app, KeyCode::Char('8'), KeyModifiers::NONE);
    assert_eq!(app.away_after(), Minutes(10));
    assert_eq!(app.overlay, Overlay::Status { sel: 7 });
    assert_eq!(backend.chosen_status(), None, "nothing was set as a status");
    // Escape closes it, and what was chosen holds: with never, idle is no longer reported.
    for _ in 0..4 {
        press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
    }
    assert_eq!(app.away_after(), Never);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.overlay, Overlay::None);
    app.last_input = ago(3600);
    app.own.reporter.rewind(std::time::Duration::from_secs(130));
    pump(&mut app, |_| {
        backend.activity_reports().last() == Some(&true)
    });
    assert!(!backend.activity_reports().contains(&false));
}

#[test]
fn the_status_command_changes_how_long_before_away_for_the_session_and_says_how_it_is_used() {
    use crate::status::AwayAfter;
    let (mut app, _) = reporting_app(AwayAfter::Minutes(5));
    app.run_command("status away-after 20");
    assert_eq!(app.away_after(), AwayAfter::Minutes(20));
    app.run_command("status away-after never");
    assert_eq!(app.away_after(), AwayAfter::Never);
    app.run_command("status away-after soon");
    assert_eq!(
        app.away_after(),
        AwayAfter::Never,
        "a bad one changes nothing"
    );
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| m.contains("usage") && *err),
        "{:?}",
        app.status
    );
    app.run_command("status sleepy");
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, ..)| m.contains("usage"))
    );
}

#[test]
fn a_service_that_refuses_reports_and_choices_is_said_once_and_is_not_a_lost_sign_in() {
    use crate::status::AwayAfter;
    let backend = Lapsed::new(true, true);
    backend.refuse_presence();
    let mut app = lapsed_app(&backend);
    app.set_activity(true, AwayAfter::Minutes(5));
    pump(&mut app, |a| a.chats.loaded());
    pump(&mut app, |a| a.own.told);
    assert_eq!(app.sign_in, SignIn::Fine);
    app.run_command("status busy");
    pump(&mut app, |a| {
        a.status
            .as_ref()
            .is_some_and(|(m, _, err)| m.contains("cannot set your status") && *err)
    });
    assert_eq!(app.sign_in, SignIn::Fine);
}

#[test]
fn availability_turned_off_is_neither_asked_for_nor_drawn() {
    let mut app = presence_app();
    app.set_presence_style(crate::presence::PresenceStyle::Off);
    for _ in 0..20 {
        app.on_tick();
    }
    assert!(app.presences.known.is_empty() && !app.presences.in_flight);
    let screen = crate::ui::render_to_string(&mut app, 120, 40);
    assert!(!screen.contains("🟢") && !screen.contains("🔴"), "{screen}");
}

#[test]
fn a_refused_presence_request_is_said_once_and_never_read_as_a_lost_sign_in() {
    let backend = Lapsed::new(true, true);
    backend.refuse_presence();
    let mut app = lapsed_app(&backend);
    pump(&mut app, |a| a.chats.loaded());
    app.presences.sched.set_last(ago(10));
    pump(&mut app, |a| a.presences.told);
    assert_eq!(app.sign_in, SignIn::Fine, "a side call is not the sign-in");
    let status = app.status.clone().map(|(m, _, _)| m).unwrap_or_default();
    assert!(status.contains("availability not shown"), "{status}");
    assert_eq!(app.presences.sched.fails(), 1);
    // It is not asked again straight away, and the failure is not announced a second time.
    app.status = None;
    for _ in 0..30 {
        app.on_tick();
    }
    assert!(!app.presences.in_flight);
    assert!(
        !app.status
            .as_ref()
            .is_some_and(|(m, ..)| m.contains("availability"))
    );
}

#[test]
fn f6_on_a_working_sign_in_asks_first_and_a_second_press_means_it() {
    let backend = Lapsed::new(true, true);
    let mut app = lapsed_app(&backend);
    pump(&mut app, |a| a.chats.loaded());
    assert_eq!(app.sign_in, SignIn::Fine);

    press(&mut app, KeyCode::F(6), KeyModifiers::NONE);
    assert_eq!(
        app.sign_in,
        SignIn::Fine,
        "no browser for an accidental key"
    );
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, ..)| m.contains("press F6 again")),
        "{:?}",
        app.status
    );
    press(&mut app, KeyCode::F(6), KeyModifiers::NONE);
    assert!(
        matches!(app.sign_in, SignIn::Waiting { .. }),
        "the second press goes ahead"
    );
    pump(&mut app, |a| a.sign_in == SignIn::Fine);

    // The question is forgotten once answered: asking again starts over.
    press(&mut app, KeyCode::F(6), KeyModifiers::NONE);
    assert_eq!(app.sign_in, SignIn::Fine);
    // And a window that has run out does not count.
    app.sign_in_asked = Instant::now().checked_sub(Duration::from_secs(60));
    press(&mut app, KeyCode::F(6), KeyModifiers::NONE);
    assert_eq!(app.sign_in, SignIn::Fine, "too late: it asks again");
}

#[test]
fn a_lapsed_sign_in_stops_the_fetching_says_so_and_signing_in_again_resumes_it_with_the_draft_intact()
 {
    let backend = Lapsed::new(true, true);
    let mut app = lapsed_app(&backend);
    pump(&mut app, |a| a.chats.loaded());
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.focus = Focus::Editor;
    app.editor.set_text("half a thought");
    // The next fetch finds the sign-in gone.
    backend.lapse();
    app.run_command("refresh");
    pump(&mut app, |a| matches!(a.sign_in, SignIn::Expired { .. }));
    let screen = crate::ui::render_to_string(&mut app, 120, 30);
    assert!(screen.contains("Press F6 to sign in again"), "{screen}");

    // Nothing more is asked of the service while the sign-in is gone.
    let before = backend.fetches();
    for _ in 0..30 {
        app.on_tick();
    }
    assert_eq!(backend.fetches(), before);
    app.run_command("refresh");
    assert_eq!(backend.fetches(), before, "not even a manual refresh");

    press(&mut app, KeyCode::F(6), KeyModifiers::NONE);
    assert!(matches!(app.sign_in, SignIn::Waiting { .. }));
    pump(&mut app, |a| a.sign_in == SignIn::Fine);
    assert_eq!(
        app.overlay,
        Overlay::None,
        "the popup goes when the sign-in is back"
    );
    pump(&mut app, |a| a.chats.loaded());
    assert_eq!(app.editor.text(), "half a thought");
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, ..)| m.contains("signed in again"))
    );
    assert!(backend.fetches() > before, "fetching resumed");
}

#[test]
fn scrolling_to_the_top_while_signed_out_does_not_ask_the_service_for_an_older_page() {
    let backend = Lapsed::new(true, true);
    let mut app = lapsed_app(&backend);
    pump(&mut app, |a| a.chats.loaded());
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    // The demo backend never offers an older page on its own; give this chat one to fetch, as a real chat
    // usually has, so the bug (a scroll-triggered fetch skipping the sign-in gate every other fetch path honours)
    // has something to actually dispatch.
    app.states.get_mut("demo-ada").unwrap().older = Some("cursor".into());
    backend.lapse();
    app.run_command("refresh");
    pump(&mut app, |a| matches!(a.sign_in, SignIn::Expired { .. }));

    let before = backend.fetches();
    app.goto_msg(false); // Home: jump to the top of the conversation
    assert_eq!(
        backend.fetches(),
        before,
        "reaching the top of the conversation while signed out must not dispatch a real request"
    );
}

#[test]
fn a_sign_in_that_does_not_finish_leaves_the_banner_and_can_be_tried_again() {
    let backend = Lapsed::new(true, false);
    backend.lapse();
    let mut app = lapsed_app(&backend);
    pump(&mut app, |a| matches!(a.sign_in, SignIn::Expired { .. }));
    press(&mut app, KeyCode::F(6), KeyModifiers::NONE);
    pump(&mut app, |a| a.sign_in.failure().is_some());
    assert!(matches!(app.sign_in, SignIn::Expired { .. }));
    let screen = crate::ui::render_to_string(&mut app, 120, 30);
    assert!(
        screen.contains("Sign-in failed") && screen.contains("F6 to try again"),
        "{screen}"
    );
    press(&mut app, KeyCode::F(6), KeyModifiers::NONE);
    assert!(matches!(app.sign_in, SignIn::Waiting { .. }), "again");
}

fn after_the_grace_period(app: &mut App) {
    if let Overlay::SignIn { .. } = app.overlay {
        app.overlay = Overlay::SignIn {
            since: Instant::now() - Duration::from_secs(5),
        };
    }
}

#[test]
fn a_retry_failing_again_does_not_rearm_the_popups_grace_period() {
    let backend = Lapsed::new(true, false);
    backend.lapse();
    let mut app = lapsed_app(&backend);
    pump(&mut app, |a| matches!(a.sign_in, SignIn::Expired { .. }));
    after_the_grace_period(&mut app);
    press(&mut app, KeyCode::F(6), KeyModifiers::NONE);
    assert!(matches!(app.sign_in, SignIn::Waiting { .. }));
    pump(&mut app, |a| a.sign_in.failure().is_some());
    assert!(matches!(app.sign_in, SignIn::Expired { .. }));
    // The popup was already open, showing the previous attempt; reopening it after this new failure must not rearm
    // the "ignore accidental keys" grace period, or reacting to the failure right away gets silently dropped.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        matches!(app.sign_in, SignIn::Waiting { .. }),
        "Enter right after the failure must not be swallowed by a freshly rearmed grace period"
    );
}

#[test]
fn a_lost_sign_in_opens_a_popup_with_the_reason_that_can_be_copied_and_closed() {
    let backend = Lapsed::new(true, false);
    backend.lapse();
    let (mut app, fakes) = lapsed_app_and_fakes(&backend);
    pump(&mut app, |a| matches!(a.sign_in, SignIn::Expired { .. }));
    assert!(
        matches!(app.overlay, Overlay::SignIn { .. }),
        "it opens by itself"
    );
    let screen = crate::ui::render_to_string(&mut app, 120, 40);
    assert!(
        screen.contains("Your Teams sign-in has expired")
            && screen.contains("Sign in again")
            && screen.contains("Copy details")
            && screen.contains("Close"),
        "{screen}"
    );

    // Keys typed as it opens are not taken as answers (someone may be typing a message).
    app.on_key(key('c'));
    app.on_key(key('q'));
    assert!(matches!(app.overlay, Overlay::SignIn { .. }));
    assert!(fakes.clipboard.get().is_none());
    after_the_grace_period(&mut app);

    // Copy puts the whole story on the clipboard, and the popup stays.
    app.on_key(key('c'));
    let copied = fakes.clipboard.get().expect("copied");
    assert!(copied.contains("Why: not signed in"), "{copied}");
    assert!(matches!(app.overlay, Overlay::SignIn { .. }));

    // Esc closes it; the banner stays.
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.overlay, Overlay::None);
    assert!(!app.sign_in.is_online());

    // Signing in again that fails brings it back, with what went wrong.
    press(&mut app, KeyCode::F(6), KeyModifiers::NONE);
    pump(&mut app, |a| a.sign_in.failure().is_some());
    assert!(matches!(app.overlay, Overlay::SignIn { .. }));
    let screen = crate::ui::render_to_string(&mut app, 120, 40);
    assert!(
        screen.contains("did not work") && screen.contains("The last attempt failed"),
        "{screen}"
    );
}

#[test]
fn the_popups_buttons_can_be_clicked() {
    let backend = Lapsed::new(true, true);
    backend.lapse();
    let (mut app, fakes) = lapsed_app_and_fakes(&backend);
    pump(&mut app, |a| matches!(a.sign_in, SignIn::Expired { .. }));
    crate::ui::render_to_string(&mut app, 120, 40);
    let g = app.view.geometry.clone();
    let plan = crate::sign_in_modal::plan(
        Rect::new(0, 0, g.header.width, g.panel.bottom()),
        &app.sign_in,
    );
    let click_on = |app: &mut App, i: usize| {
        let r = plan.buttons[i].1;
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: r.x + 1,
            row: r.y,
            modifiers: KeyModifiers::NONE,
        });
    };
    // Too soon after it opened by itself: ignored.
    click_on(&mut app, 2);
    assert!(matches!(app.overlay, Overlay::SignIn { .. }));
    after_the_grace_period(&mut app);
    // Clicking beside the buttons does nothing.
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    assert!(matches!(app.overlay, Overlay::SignIn { .. }));
    click_on(&mut app, 1);
    assert!(fakes.clipboard.get().is_some());
    click_on(&mut app, 0);
    assert!(matches!(app.sign_in, SignIn::Waiting { .. }));
    pump(&mut app, |a| a.sign_in == SignIn::Fine);
    assert_eq!(app.overlay, Overlay::None, "signing in again closes it");
}

#[test]
fn a_lost_sign_in_reads_short_in_the_panes_and_points_at_f6() {
    let expired = Error::Auth(teams_client::AuthError::Expired {
        code: "invalid_grant".into(),
        renewal: Box::new(Error::Auth(teams_client::AuthError::RenewalPaused)),
    });
    assert_eq!(
        describe_running(&expired),
        "signed out: press F6 to sign in again"
    );
    assert!(describe_running(&scripted_failure()).len() > 5);
}

#[test]
fn a_service_that_cannot_sign_in_again_keeps_the_old_advice() {
    let backend = Lapsed::new(false, false);
    backend.lapse();
    let mut app = lapsed_app(&backend);
    for _ in 0..10 {
        app.on_tick();
    }
    assert_eq!(app.sign_in, SignIn::Fine);
    press(&mut app, KeyCode::F(6), KeyModifiers::NONE);
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.contains("cannot sign in again"))
    );
    assert!(
        describe(&Error::Auth(teams_client::AuthError::NotSignedIn)).contains("vtteams logout")
    );
}
#[test]
fn a_scheduled_refresh_of_what_is_showing_is_silent_and_what_was_asked_for_is_not() {
    let mut app = loaded_app();
    let id = app.current.clone().unwrap();
    assert!(!app.is_busy());

    // The poll the schedule starts: in flight, but nothing to see.
    app.states
        .get_mut(&id)
        .unwrap()
        .sched
        .set_last(ago(10 * 60));
    app.chats.sched.set_last(ago(10 * 60));
    app.on_tick();
    assert!(
        app.states[&id].is_fetching() || app.chats.is_fetching(),
        "something was fetched"
    );
    assert!(!app.is_busy(), "and nothing is announced");
    assert!(!app.states[&id].shows_loading());
    pump(&mut app, |a| a.settled());

    // The same fetches when asked for, or for a chat that is not yet showing anything, are announced.
    app.refresh_chats();
    assert!(app.is_busy());
    pump(&mut app, |a| a.settled());
    assert!(app.load_messages(&id, false));
    assert!(app.is_busy() && app.states[&id].shows_loading());
    pump(&mut app, |a| a.settled());
    let fresh = ChatState::new();
    assert!(
        !fresh.loaded && !fresh.shows_loading(),
        "nothing in flight yet"
    );
    app.states.get_mut(&id).unwrap().loaded = false;
    app.states
        .get_mut(&id)
        .unwrap()
        .sched
        .set_last(ago(10 * 60));
    app.on_tick();
    assert!(app.is_busy(), "a first load is announced");
}

#[test]
fn settled_waits_for_quiet_fetches_including_a_chat_that_is_no_longer_selected() {
    let mut app = loaded_app();
    let id = app.current.clone().unwrap();
    assert!(app.start_message_fetch(&id, Fetch::Newest { quiet: true }));
    assert!(!app.is_busy(), "quiet work has no spinner");
    assert!(!app.settled(), "the message request is still in flight");
    let other = app
        .chats
        .items()
        .iter()
        .find(|c| c.id != id)
        .unwrap()
        .id
        .clone();
    app.select_chat(&other);
    assert!(!app.is_busy());
    assert!(
        !app.settled(),
        "switching chats does not complete the old request"
    );
    pump(&mut app, App::settled);
    app.refresh_chats_as(true);
    assert!(!app.is_busy());
    assert!(!app.settled(), "a quiet list refresh also counts");
    pump(&mut app, App::settled);
}
#[test]
fn the_key_hints_for_a_message_always_name_reacting_and_help_even_on_a_narrow_terminal() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Messages;
    for width in [60, 80, 100, 140] {
        let screen = crate::ui::render_to_string(&mut app, width, 30);
        let hints = screen
            .lines()
            .rev()
            .find(|l| l.contains("react"))
            .unwrap_or_else(|| panic!("no react hint at {width} columns:\n{screen}"));
        assert!(
            hints.contains("^e") && hints.contains("F1"),
            "{width}: {hints}"
        );
        assert!(
            hints.chars().count() <= usize::from(width),
            "{width}: {hints}"
        );
    }
    // The vim keymap too.
    app.set_keymap(Keymap::Vim);
    for width in [60, 100] {
        let screen = crate::ui::render_to_string(&mut app, width, 30);
        let hints = screen
            .lines()
            .rev()
            .find(|l| l.contains("react"))
            .unwrap_or("");
        assert!(
            hints.contains(" e ") && hints.contains("react"),
            "{width}:\n{screen}"
        );
    }
}

#[test]
fn in_the_vim_keymap_plus_reacts_to_the_selected_message_as_f2_does() {
    let (mut app, _) = standard_app();
    app.set_keymap(Keymap::Vim);
    app.focus = Focus::Messages;
    select_last_message(&mut app);
    app.on_key(key('+'));
    assert!(matches!(
        emoji_overlay(&app),
        Some((EmojiTarget::React { .. }, "", 0))
    ));
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    assert!(matches!(
        emoji_overlay(&app),
        Some((EmojiTarget::React { .. }, ..))
    ));
}

#[test]
fn choosing_an_emoji_in_the_message_box_inserts_it_where_the_cursor_is() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    type_text(&mut app, "well done ");
    ctrl(&mut app, 'e');
    assert!(matches!(
        emoji_overlay(&app),
        Some((EmojiTarget::Insert, ..))
    ));
    type_text(&mut app, "fire");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.editor.text(), "well done 🔥");
    assert_eq!(app.focus, Focus::Editor);
    type_text(&mut app, "!");
    assert_eq!(
        app.editor.text(),
        "well done 🔥!",
        "typing carries on after it"
    );
}

#[test]
fn an_emoji_replaces_the_selection_and_cancelling_changes_nothing() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    type_text(&mut app, "abc");
    ctrl(&mut app, 'a');
    ctrl(&mut app, 'e');
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.editor.text(), "abc");
    ctrl(&mut app, 'e');
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.editor.text(), "👍", "the selection was replaced");
}

#[test]
fn the_selector_searches_moves_and_scrolls() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    ctrl(&mut app, 'e');
    press(&mut app, KeyCode::Right, KeyModifiers::NONE);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(
        emoji_overlay(&app).map(|(_, _, sel)| sel),
        Some(1 + crate::app::EMOJI_COLUMNS)
    );
    type_text(&mut app, "ro");
    assert_eq!(
        emoji_overlay(&app).map(|(_, q, sel)| (q, sel)),
        Some(("ro", 0)),
        "typing starts again at the first match"
    );
    press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(emoji_overlay(&app).map(|(_, q, _)| q), Some("r"));
    press(&mut app, KeyCode::End, KeyModifiers::NONE);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        !app.editor.is_empty(),
        "the last match was chosen without running off the end"
    );
    // Nothing matches: Enter does nothing and the selector stays.
    ctrl(&mut app, 'e');
    type_text(&mut app, "zzzzzz");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(emoji_overlay(&app).is_some());
    let drawn = crate::ui::render_to_string(&mut app, 120, 40);
    assert!(
        drawn.contains("Insert emoji") && drawn.contains("nothing matches"),
        "{drawn}"
    );
}

#[test]
fn the_selector_is_drawn_with_its_purpose_and_the_name_under_the_cursor() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Messages;
    select_last_message(&mut app);
    ctrl(&mut app, 'e');
    let drawn = crate::ui::render_to_string(&mut app, 120, 40);
    assert!(
        drawn.contains("React") && drawn.contains(":thumbsup:") && drawn.contains("esc cancel"),
        "{drawn}"
    );
    assert!(drawn.contains('👍'));
}

/// A chat whose newest pages hold only events (joins, calls), with real messages `real_after` pages further back.
struct Eventful {
    inner: DemoBackend,
    real_after: usize,
    /// An older page at or past this one fails instead of returning events, to test what an automatic autofill
    /// request's own failure does.
    fail_from: Option<usize>,
}

impl Eventful {
    fn event(id: usize, page: usize) -> Msg {
        let t = Utc::now() - chrono::Duration::minutes((page * 100 + id) as i64);
        Msg {
            root_id: None,
            id: format!("{page}-{id}"),
            created: t,
            modified: t,
            edited: None,
            from_id: None,
            from_name: String::new(),
            system: true,
            deleted: false,
            html: false,
            body: "chat updated".into(),
            subject: None,
            attachments: vec![],
            reactions: vec![],
            mentions: vec![],
            delivery: Delivery::Sent,
        }
    }
}

impl Backend for Eventful {
    fn label(&self) -> String {
        self.inner.label()
    }
    fn me(&self) -> Result<Me, Error> {
        self.inner.me()
    }
    fn chats(&self) -> Result<Vec<Chat>, Error> {
        self.inner.chats()
    }
    fn find_people(&self, q: &str) -> Result<Vec<Contact>, Error> {
        self.inner.find_people(q)
    }
    fn open_chat(&self, who: &Contact) -> Result<Chat, Error> {
        self.inner.open_chat(who)
    }
    fn messages(&self, _id: &str, older: Option<&str>) -> Result<Page, Error> {
        let page: usize = older.map_or(0, |c| c.parse().unwrap());
        if self.fail_from.is_some_and(|f| page >= f) {
            return Err(Error::Unexpected {
                operation: "test fetch",
                detail: "failed",
            });
        }
        let mut messages: Vec<Msg> = (0..12).map(|i| Self::event(i, page)).collect();
        messages.reverse();
        if page == self.real_after {
            for (i, m) in messages.iter_mut().enumerate().take(10) {
                m.system = false;
                m.from_id = Some("someone".into());
                m.from_name = "Someone".into();
                m.html = false;
                m.body = format!("real {i}");
            }
        }
        Ok(Page {
            messages,
            older: Some((page + 1).to_string()),
        })
    }
    fn send(
        &self,
        id: &str,
        html: &str,
        m: &[teams_client::OutMention],
        reply_to: Option<&teams_client::ReplyTarget>,
    ) -> Result<Msg, Error> {
        self.inner.send(id, html, m, reply_to)
    }
    fn serves_image(&self, url: &str) -> bool {
        self.inner.serves_image(url)
    }
    fn image(&self, url: &str) -> Result<RgbImage, Error> {
        self.inner.image(url)
    }
}

fn open_eventful(real_after: usize) -> App {
    let (mut app, _) = App::for_test(Arc::new(Eventful {
        inner: DemoBackend::instant(),
        real_after,
        fail_from: None,
    }));
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    // Let any pages the app asks for by itself arrive.
    for _ in 0..40 {
        app.on_tick();
    }
    app
}

#[test]
fn a_chat_whose_newest_page_is_only_events_goes_back_until_there_is_something_to_read() {
    let app = open_eventful(2);
    let st = app.state().unwrap();
    assert!(
        st.messages().iter().filter(|m| !m.system).count() >= AUTOFILL_MIN_MESSAGES,
        "{} messages, {} of them events",
        st.messages().len(),
        st.messages().iter().filter(|m| m.system).count()
    );
    assert_eq!(st.messages().len(), 36, "three pages, and no more");
    assert!(st.follow, "still following the newest");
    assert_eq!(st.sel, st.messages().len() - 1);
}

#[test]
fn going_back_on_its_own_stops_after_a_few_pages() {
    let app = open_eventful(usize::MAX);
    let st = app.state().unwrap();
    assert_eq!(
        st.messages().len(),
        12 * (1 + usize::from(AUTOFILL_MAX_PAGES)),
        "a chat of nothing but events is not read to the end"
    );
    assert!(st.older.is_some(), "the user can still go further back");
}

#[test]
fn a_failed_automatic_autofill_page_stays_quiet_instead_of_showing_an_error() {
    let (mut app, _) = App::for_test(Arc::new(Eventful {
        inner: DemoBackend::instant(),
        real_after: usize::MAX,
        fail_from: Some(1),
    }));
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    for _ in 0..10 {
        app.on_tick();
    }
    let st = app.state().unwrap();
    assert!(
        !st.shows_loading(),
        "the failed autofill page must not leave the spinner on"
    );
    assert!(
        !app.is_busy(),
        "no request is still pretending to be in flight"
    );
    assert!(
        app.status.is_none(),
        "a background page nobody asked for must not surface an error: {:?}",
        app.status
    );
}

#[test]
fn a_chat_with_enough_messages_is_not_read_further_back() {
    let (mut app, _) = App::for_test(Arc::new(DemoBackend::instant()));
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    assert_eq!(app.state().unwrap().automatic_pages(), 0);
}

#[test]
fn events_in_a_row_sit_on_adjacent_lines() {
    let mut app = open_eventful(usize::MAX);
    crate::ui::render_to_string(&mut app, 120, 40);
    let st = app.state().unwrap();
    let t = &st.transcript;
    let events: Vec<usize> = t.starts.iter().copied().take(5).collect();
    assert_eq!(
        events.windows(2).map(|w| w[1] - w[0]).collect::<Vec<_>>(),
        [1, 1, 1, 1].to_vec(),
        "no blank line between them"
    );
}
#[test]
fn a_reaction_the_service_refuses_is_taken_back_and_the_reason_shown() {
    /// The demo, except that it cannot react (the trait's default).
    struct NoReactions(DemoBackend);
    impl Backend for NoReactions {
        fn label(&self) -> String {
            self.0.label()
        }
        fn me(&self) -> Result<Me, Error> {
            self.0.me()
        }
        fn chats(&self) -> Result<Vec<Chat>, Error> {
            self.0.chats()
        }
        fn find_people(&self, q: &str) -> Result<Vec<Contact>, Error> {
            self.0.find_people(q)
        }
        fn open_chat(&self, who: &Contact) -> Result<Chat, Error> {
            self.0.open_chat(who)
        }
        fn messages(&self, id: &str, older: Option<&str>) -> Result<Page, Error> {
            self.0.messages(id, older)
        }
        fn send(
            &self,
            id: &str,
            html: &str,
            m: &[teams_client::OutMention],
            reply_to: Option<&teams_client::ReplyTarget>,
        ) -> Result<Msg, Error> {
            self.0.send(id, html, m, reply_to)
        }
        fn serves_image(&self, url: &str) -> bool {
            self.0.serves_image(url)
        }
        fn image(&self, url: &str) -> Result<RgbImage, Error> {
            self.0.image(url)
        }
    }
    let (mut app, _) = App::for_test(Arc::new(NoReactions(DemoBackend::instant())));
    app.set_keymap(Keymap::Standard);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.focus = Focus::Messages;
    select_last_message(&mut app);
    ctrl(&mut app, 'e');
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        reactions_of_last_message(&app),
        [("👍".to_string(), 1)],
        "shown at once"
    );
    pump(&mut app, |a| {
        a.status.as_ref().is_some_and(|(_, _, err)| *err)
    });
    assert!(
        reactions_of_last_message(&app).is_empty(),
        "and withdrawn when it failed"
    );
    let status = app.status.as_ref().map(|(m, ..)| m.clone()).unwrap();
    assert!(
        status.contains("reaction failed") && status.contains("cannot react"),
        "{status}"
    );
}

#[test]
fn there_is_nothing_to_react_to_on_a_system_message_or_in_the_chat_list() {
    let (mut app, _) = standard_app();
    app.select_chat("demo-sprint");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.focus = Focus::Messages;
    select_last_message(&mut app);
    assert!(
        last_message(&app).system,
        "the demo meeting chat ends with an event"
    );
    ctrl(&mut app, 'e');
    assert_eq!(app.overlay, Overlay::None);
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.contains("select a sent message"))
    );
    app.focus = Focus::Chats;
    ctrl(&mut app, 'e');
    assert_eq!(app.overlay, Overlay::None);
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.contains("open a conversation"))
    );
}

#[test]
fn f2_opens_the_selector_in_the_vim_keymap_for_the_editor_and_for_the_conversation() {
    let (mut app, _) = app_and_fakes();
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.focus = Focus::Messages;
    select_last_message(&mut app);
    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    assert!(matches!(
        emoji_overlay(&app),
        Some((EmojiTarget::React { .. }, ..))
    ));
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(reactions_of_last_message(&app), [("👍".to_string(), 1)]);

    app.on_key(key('i'));
    assert_eq!(app.focus, Focus::Editor);
    type_text(&mut app, "ship it ");
    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    type_text(&mut app, "rocket");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.editor.text(), "ship it 🚀");
    assert_eq!(app.editor.mode(), Mode::Insert, "still typing");
}

#[test]
fn unverified_images_are_not_loaded_automatically() {
    struct UnverifiedBackend(DemoBackend);
    impl Backend for UnverifiedBackend {
        fn label(&self) -> String {
            self.0.label()
        }
        fn me(&self) -> Result<Me, Error> {
            self.0.me()
        }
        fn chats(&self) -> Result<Vec<Chat>, Error> {
            self.0.chats()
        }
        fn find_people(&self, q: &str) -> Result<Vec<Contact>, Error> {
            self.0.find_people(q)
        }
        fn open_chat(&self, who: &Contact) -> Result<Chat, Error> {
            self.0.open_chat(who)
        }
        fn messages(&self, id: &str, older: Option<&str>) -> Result<Page, Error> {
            self.0.messages(id, older)
        }
        fn send(
            &self,
            id: &str,
            html: &str,
            m: &[teams_client::OutMention],
            reply_to: Option<&teams_client::ReplyTarget>,
        ) -> Result<Msg, Error> {
            self.0.send(id, html, m, reply_to)
        }
        fn serves_image(&self, _: &str) -> bool {
            true
        }
        fn image(&self, url: &str) -> Result<RgbImage, Error> {
            Err(Error::UnverifiedContent {
                operation: "fetch image",
                url: url.to_string(),
            })
        }
    }
    let mut app = app_with(Arc::new(UnverifiedBackend(DemoBackend::instant())));
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    // Render to trigger image queueing and downloading.
    for _ in 0..10 {
        crate::ui::render_to_string(&mut app, 120, 40);
        app.on_tick();
    }
    // Check that the image is marked as unverified, not failed.
    let unverified_count = app
        .images
        .store()
        .map
        .values()
        .filter(|s| matches!(s, ImgState::Unverified(_)))
        .count();
    assert_eq!(
        unverified_count, 1,
        "unverified image should be in Unverified state"
    );
    // Check that the image is not queued for re-download.
    assert_eq!(
        app.images.queued().len(),
        0,
        "unverified images should not be re-queued automatically"
    );
}

#[test]
fn ctrl_l_loads_unverified_images_in_standard_keymap() {
    struct UnverifiedBackend(DemoBackend);
    impl Backend for UnverifiedBackend {
        fn label(&self) -> String {
            self.0.label()
        }
        fn me(&self) -> Result<Me, Error> {
            self.0.me()
        }
        fn chats(&self) -> Result<Vec<Chat>, Error> {
            self.0.chats()
        }
        fn find_people(&self, q: &str) -> Result<Vec<Contact>, Error> {
            self.0.find_people(q)
        }
        fn open_chat(&self, who: &Contact) -> Result<Chat, Error> {
            self.0.open_chat(who)
        }
        fn messages(&self, id: &str, older: Option<&str>) -> Result<Page, Error> {
            self.0.messages(id, older)
        }
        fn send(
            &self,
            id: &str,
            html: &str,
            m: &[teams_client::OutMention],
            reply_to: Option<&teams_client::ReplyTarget>,
        ) -> Result<Msg, Error> {
            self.0.send(id, html, m, reply_to)
        }
        fn serves_image(&self, _: &str) -> bool {
            true
        }
        fn image(&self, _: &str) -> Result<RgbImage, Error> {
            // Always return unverified for this test.
            Err(Error::UnverifiedContent {
                operation: "fetch image",
                url: "https://demo.invalid/test".to_string(),
            })
        }
    }
    let (mut app, _) = App::for_test(Arc::new(UnverifiedBackend(DemoBackend::instant())));
    app.set_keymap(Keymap::Standard);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.focus = Focus::Messages;
    // Render to trigger image download.
    for _ in 0..10 {
        crate::ui::render_to_string(&mut app, 120, 40);
        app.on_tick();
    }
    // Wait for image to be marked as unverified.
    pump(&mut app, |a| {
        a.images
            .store()
            .map
            .values()
            .any(|s| matches!(s, ImgState::Unverified(_)))
    });
    // Select the last message which has the image.
    select_last_message(&mut app);
    // Use Ctrl-L to load.
    ctrl(&mut app, 'l');
    // The status should indicate loading or that images were queued.
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, _)| m.contains("loading") || m.contains("image")),
        "status should confirm action: {:?}",
        app.status
    );
}

/// A backend for which every picture is from outside the organisation: `image` always refuses, as the real backends
/// do for another tenant, and `image_from_outside` (the explicit-consent call) serves it and records the request.
struct OutsideBackend {
    inner: DemoBackend,
    explicit: Mutex<Vec<String>>,
}

impl Backend for OutsideBackend {
    fn label(&self) -> String {
        self.inner.label()
    }
    fn me(&self) -> Result<Me, Error> {
        self.inner.me()
    }
    fn chats(&self) -> Result<Vec<Chat>, Error> {
        self.inner.chats()
    }
    fn find_people(&self, q: &str) -> Result<Vec<Contact>, Error> {
        self.inner.find_people(q)
    }
    fn open_chat(&self, who: &Contact) -> Result<Chat, Error> {
        self.inner.open_chat(who)
    }
    fn messages(&self, id: &str, older: Option<&str>) -> Result<Page, Error> {
        self.inner.messages(id, older)
    }
    fn send(
        &self,
        id: &str,
        html: &str,
        m: &[teams_client::OutMention],
        reply_to: Option<&teams_client::ReplyTarget>,
    ) -> Result<Msg, Error> {
        self.inner.send(id, html, m, reply_to)
    }
    fn serves_image(&self, url: &str) -> bool {
        self.inner.serves_image(url)
    }
    fn image(&self, url: &str) -> Result<RgbImage, Error> {
        Err(Error::UnverifiedContent {
            operation: "fetch image",
            url: url.to_string(),
        })
    }
    fn image_from_outside(&self, url: &str) -> Result<RgbImage, Error> {
        self.explicit.lock().unwrap().push(url.to_string());
        self.inner.image(url)
    }
}

/// Open a chat whose pictures are all from outside, draw it until they are placeholders, and select the message
/// holding the first one. Returns the backend, that picture's URL, and every other unverified picture's.
fn outside_pictures(keymap: Keymap) -> (App, Arc<OutsideBackend>, String, Vec<String>) {
    let backend = Arc::new(OutsideBackend {
        inner: DemoBackend::instant(),
        explicit: Mutex::new(Vec::new()),
    });
    let (mut app, _) = App::for_test(backend.clone());
    app.set_keymap(keymap);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    let chat = app
        .chats
        .items()
        .iter()
        .find(|c| {
            backend
                .inner
                .messages(&c.id, None)
                .is_ok_and(|p| p.messages.iter().any(|m| m.body.contains("<img")))
        })
        .map(|c| c.id.clone())
        .expect("a demo chat with a picture");
    app.select_chat(&chat);
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.focus = Focus::Messages;
    for _ in 0..20 {
        crate::ui::render_to_string(&mut app, 120, 60);
        app.on_tick();
    }
    let unverified: Vec<String> = app
        .images
        .store()
        .map
        .iter()
        .filter(|(_, s)| matches!(s, ImgState::Unverified(_)))
        .map(|(url, _)| url.clone())
        .collect();
    assert!(!unverified.is_empty(), "the pictures became placeholders");
    let st = app.state().unwrap();
    let (rows, first) = st
        .transcript
        .image_rows
        .iter()
        .find(|(_, url)| unverified.contains(url))
        .map(|(rows, url)| (rows.clone(), url.clone()))
        .expect("a placeholder is drawn");
    let message = (0..st.transcript.starts.len())
        .find(|&i| st.transcript.starts[i] <= rows.start && rows.start <= st.transcript.ends[i])
        .expect("the placeholder belongs to a message");
    app.state_mut().unwrap().sel = message;
    let others = unverified.into_iter().filter(|u| *u != first).collect();
    (app, backend, first, others)
}

#[test]
fn an_outside_picture_loads_only_after_the_load_key_and_only_that_one() {
    for (keymap, press_load) in [
        (
            Keymap::Standard,
            (|app: &mut App| ctrl(app, 'l')) as fn(&mut App),
        ),
        (Keymap::Vim, |app: &mut App| app.on_key(key('L'))),
    ] {
        let (mut app, backend, first, others) = outside_pictures(keymap);
        // Scrolling over and redrawing never asks for it explicitly.
        for _ in 0..10 {
            crate::ui::render_to_string(&mut app, 120, 60);
            app.on_tick();
        }
        assert!(
            backend.explicit.lock().unwrap().is_empty(),
            "{keymap:?}: nothing outside is loaded without the key"
        );
        press_load(&mut app);
        for _ in 0..20 {
            crate::ui::render_to_string(&mut app, 120, 60);
            app.on_tick();
        }
        assert_eq!(
            *backend.explicit.lock().unwrap(),
            vec![first.clone()],
            "{keymap:?}: exactly the selected message's picture, once, through the explicit call"
        );
        assert!(
            !matches!(
                app.images.store().map.get(&first),
                Some(ImgState::Unverified(_))
            ),
            "{keymap:?}: the chosen picture is no longer a placeholder"
        );
        for other in &others {
            assert!(
                matches!(
                    app.images.store().map.get(other),
                    Some(ImgState::Unverified(_))
                ),
                "{keymap:?}: a picture in another message stays unloaded"
            );
        }
        // Choosing one picture is not consent for others: open another chat with a picture of its own.
        let current = app.current.clone();
        let elsewhere = app
            .chats
            .items()
            .iter()
            .map(|c| c.id.clone())
            .filter(|id| Some(id) != current.as_ref())
            .find(|id| {
                backend
                    .inner
                    .messages(id, None)
                    .is_ok_and(|p| p.messages.iter().any(|m| m.body.contains("<img")))
            })
            .expect("a second demo chat with a picture");
        app.select_chat(&elsewhere);
        pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
        for _ in 0..20 {
            crate::ui::render_to_string(&mut app, 120, 60);
            app.on_tick();
        }
        let placeholders = app
            .images
            .store()
            .map
            .iter()
            .filter(|(url, s)| **url != first && matches!(s, ImgState::Unverified(_)))
            .count();
        assert!(
            placeholders > 0,
            "{keymap:?}: the other chat's picture is a placeholder"
        );
        assert_eq!(
            *backend.explicit.lock().unwrap(),
            vec![first.clone()],
            "{keymap:?}: nothing else was fetched through the explicit call"
        );
    }
}

mod fuzz {
    use super::*;
    use proptest::prelude::*;

    /// Keys a person might press anywhere in the app. Keys whose effect is outside the app (opening a browser) are
    /// harmless here because the test app records them instead.
    fn any_key() -> impl Strategy<Value = KeyEvent> {
        let plain = prop::sample::select(
            "abcdefghijklmnopqrstuvwxyzABCDEGHIJKLMNOPRSUVWXYZ0123456789 :/?-_.,*`\"'~#>[]{}()"
                .chars()
                .collect::<Vec<_>>(),
        )
        .prop_map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        let ctrl = prop::sample::select("hjklwdunpsvbfcakxzyroe".chars().collect::<Vec<_>>())
            .prop_map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        // What the standard keymap adds: Shift and Ctrl with the movement keys, Shift-Enter, function keys.
        let modified = (
            prop::sample::select(vec![
                KeyCode::Enter,
                KeyCode::Left,
                KeyCode::Right,
                KeyCode::Up,
                KeyCode::Down,
                KeyCode::Home,
                KeyCode::End,
                KeyCode::Backspace,
                KeyCode::Delete,
                KeyCode::Insert,
                KeyCode::BackTab,
                KeyCode::F(1),
                KeyCode::F(2),
                KeyCode::F(5),
            ]),
            prop::sample::select(vec![
                KeyModifiers::SHIFT,
                KeyModifiers::CONTROL,
                KeyModifiers::ALT,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ]),
        )
            .prop_map(|(code, mods)| KeyEvent::new(code, mods));
        let named = prop::sample::select(vec![
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Tab,
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
        ])
        .prop_map(|code| KeyEvent::new(code, KeyModifiers::NONE));
        // Alt-1 and Alt-2 pick the tab of the left pane (when there is one), so the teams list and a channel are reached.
        let tabs = prop::sample::select(vec!['1', '2'])
            .prop_map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT));
        prop_oneof![8 => plain, 2 => ctrl, 3 => named, 3 => modified, 1 => tabs]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// Whatever is typed, the app keeps its invariants and every frame can be drawn at any terminal size.
        #[test]
        fn any_key_sequence_leaves_the_app_consistent_and_drawable(
            keys in prop::collection::vec(any_key(), 0..80),
            width in 10u16..160,
            height in 6u16..60,
            standard in any::<bool>(),
        ) {
            let (mut app, _) = App::for_test(Arc::new(DemoBackend::instant()));
            app.set_keymap(if standard { Keymap::Standard } else { Keymap::Vim });
            app.start();
            for (n, key) in keys.into_iter().enumerate() {
                app.on_key(key);
                if app.quit {
                    break;
                }
                app.on_tick();
                if n % 7 == 0 {
                    crate::ui::render_to_string(&mut app, width, height);
                }
            }
            crate::ui::render_to_string(&mut app, width, height);
            if let Some(id) = &app.current {
                prop_assert!(
                    app.chats.items().iter().any(|c| &c.id == id)
                        || app.current_channel().is_some_and(|c| &c.id == id),
                    "the open chat, or the open channel, exists"
                );
            }
            prop_assert!(app.sending <= 64 && app.lookups <= 64, "in-flight counters stay small");
            let (row, col) = app.editor.cursor();
            let lines = app.editor.lines();
            prop_assert!(row < lines.len() && col <= lines[row].chars().count(), "editor cursor is inside the text");
        }

        /// Pasting any text is safe wherever the focus is.
        #[test]
        fn pasting_anything_is_safe(text in "\\PC{0,200}", focus in 0u8..3) {
            let mut app = app();
            match focus {
                0 => {}
                1 => app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                _ => app.on_key(key('i')),
            }
            app.on_paste(&text);
            crate::ui::render_to_string(&mut app, 100, 30);
        }
    }
}

// --- editing your own messages ----------------------------------------------------------------------------------

/// A demo-backend app in the given keymap whose open chat ends with a message of ours, "first words", selected.
fn app_with_my_message(keymap: Keymap) -> App {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    type_text(&mut app, "first words");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    pump(&mut app, |a| a.sending == 0);
    app.set_keymap(keymap);
    app.focus = Focus::Messages;
    select_last_message(&mut app);
    assert!(last_message(&app).body.contains("first words"));
    app
}

fn save_edit(app: &mut App) {
    if app.keymap == Keymap::Vim {
        ctrl(app, 's');
    } else {
        press(app, KeyCode::Enter, KeyModifiers::NONE);
    }
    pump(app, |a| !a.is_editing());
}

#[test]
fn e_in_the_vim_keymap_puts_my_message_in_the_box_and_enter_saves_it_marked_as_edited() {
    let mut app = app_with_my_message(Keymap::Vim);
    app.editor.set_text("half-typed draft");
    app.on_key(key('e'));
    assert!(app.is_editing());
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(app.editor.mode(), Mode::Insert);
    assert_eq!(app.editor.text(), "first words");
    assert!(app.status.as_ref().unwrap().0.contains("Enter saves"));
    let screen = crate::ui::render_to_string(&mut app, 120, 40);
    assert!(
        screen.contains("EDITING") && screen.contains("save"),
        "{screen}"
    );

    app.editor.set_text("first words, corrected");
    save_edit(&mut app);
    let m = last_message(&app);
    assert!(m.body.contains("first words, corrected"), "{}", m.body);
    assert!(m.edited.is_some(), "it is marked edited");
    assert_eq!(app.editor.text(), "half-typed draft", "the draft is back");
    assert_eq!(app.focus, Focus::Messages);
    assert_eq!(app.status.as_ref().unwrap().0, "message edited");
    let screen = crate::ui::render_to_string(&mut app, 120, 40);
    assert!(screen.contains("(edited"), "{screen}");
    // The service's own copy, read again, agrees (the demo backend keeps the edit).
    app.run_command("refresh");
    pump(&mut app, |a| a.settled());
    assert!(last_message(&app).body.contains("corrected") && last_message(&app).edited.is_some());
}

#[test]
fn editing_a_message_with_icon_font_glyphs_says_they_will_not_be_kept() {
    let mut app = app_with_my_message(Keymap::Standard);
    // U+F015 is a Font Awesome / Nerd Font icon, in the private-use area that vtteams' own style markers occupy.
    app.state_mut()
        .unwrap()
        .edit_messages(|ms| ms.last_mut().unwrap().body = "home \u{F015} sweet".into());
    app.on_key(KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE));
    assert!(app.is_editing());
    let (status, _, is_warning) = app.status.clone().unwrap();
    assert!(
        status.contains("private-use") && status.contains("lost on saving") && is_warning,
        "{status}"
    );
    assert_eq!(
        app.editor.text(),
        "home sweet",
        "the glyph is not in the box"
    );
}

#[test]
fn esc_gives_up_an_edit_without_touching_the_message_and_returns_the_draft() {
    let mut app = app_with_my_message(Keymap::Standard);
    app.editor.set_text("my draft");
    app.on_key(key('e')); // standard: a letter in the conversation starts a message, it is not a command
    assert!(!app.is_editing());
    app.editor.set_text("my draft");
    app.focus = Focus::Messages;
    press(&mut app, KeyCode::F(4), KeyModifiers::NONE);
    assert!(app.is_editing());
    assert_eq!(app.editor.text(), "first words");
    type_text(&mut app, " and more");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.is_editing());
    assert_eq!(app.editor.text(), "my draft");
    assert_eq!(app.focus, Focus::Messages);
    assert!(last_message(&app).edited.is_none());
    assert!(!last_message(&app).body.contains("and more"));
}

#[test]
fn an_edit_that_changes_nothing_or_empties_the_message_is_not_sent() {
    let mut app = app_with_my_message(Keymap::Standard);
    press(&mut app, KeyCode::F(4), KeyModifiers::NONE);
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!app.is_editing(), "unchanged: nothing to save");
    assert_eq!(app.status.as_ref().unwrap().0, "no change");
    assert!(last_message(&app).edited.is_none());

    press(&mut app, KeyCode::F(4), KeyModifiers::NONE);
    app.editor.clear();
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.is_editing(), "still editing");
    assert!(app.status.as_ref().unwrap().0.contains("cannot be empty"));
}

#[test]
fn only_my_own_sent_messages_can_be_edited() {
    let mut app = app_with_my_message(Keymap::Vim);
    // Someone else's message: the first one in the chat.
    app.state_mut().unwrap().sel = 0;
    assert_ne!(
        app.state().unwrap().messages()[0].from_id.as_deref(),
        Some(app.me_id())
    );
    app.on_key(key('e'));
    assert!(!app.is_editing());
    assert!(app.status.as_ref().unwrap().0.contains("your own"));
    // An event, and a message still being sent.
    let mut system = last_message(&app).clone();
    system.system = true;
    assert!(app.why_not_editable_for_test(&system).is_some());
    let mut sending = last_message(&app).clone();
    sending.delivery = Delivery::Sending;
    assert!(app.why_not_editable_for_test(&sending).is_some());
    assert!(app.why_not_editable_for_test(last_message(&app)).is_none());
}

#[test]
fn up_in_an_empty_message_box_edits_the_last_message_i_sent_but_not_with_text_in_it() {
    let mut app = app_with_my_message(Keymap::Standard);
    app.focus = Focus::Editor;
    type_text(&mut app, "abc");
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert!(
        !app.is_editing(),
        "with text in the box Up moves the cursor"
    );
    app.editor.clear();
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert!(app.is_editing());
    assert_eq!(app.editor.text(), "first words");
    // In the vim keymap Up is not a shortcut.
    let mut vim = app_with_my_message(Keymap::Vim);
    vim.focus = Focus::Editor;
    press(&mut vim, KeyCode::Up, KeyModifiers::NONE);
    assert!(!vim.is_editing());
}

#[test]
fn edit_last_own_refuses_without_moving_the_selection_when_already_editing() {
    let mut app = app_with_my_message(Keymap::Standard);
    // A second message of mine, so "the last message I sent" differs from the one selected below.
    app.focus = Focus::Editor;
    type_text(&mut app, "second message");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    pump(&mut app, |a| a.sending == 0);
    app.focus = Focus::Messages;

    // Select "first words", the earlier of my two messages, and start editing it.
    let first_words = app
        .state()
        .unwrap()
        .messages()
        .iter()
        .position(|m| m.body.contains("first words"))
        .unwrap();
    app.state_mut().unwrap().sel = first_words;
    press(&mut app, KeyCode::F(4), KeyModifiers::NONE);
    assert!(app.is_editing());
    let sel_before = app.state().unwrap().sel;

    app.edit_last_own();
    assert!(app.status.as_ref().unwrap().0.contains("already editing"));
    assert_eq!(
        app.state().unwrap().sel,
        sel_before,
        "a refused edit must not jump the transcript to a different message"
    );
}

#[test]
fn leaving_the_chat_gives_up_the_edit_and_the_draft_stays_with_its_own_chat() {
    let mut app = app_with_my_message(Keymap::Standard);
    app.editor.set_text("draft for ada");
    press(&mut app, KeyCode::F(4), KeyModifiers::NONE);
    assert!(app.is_editing());
    app.select_chat("demo-grace");
    assert!(!app.is_editing());
    assert_eq!(
        app.editor.text(),
        "",
        "the other chat has no draft, and not the edit text"
    );
    app.select_chat("demo-ada");
    assert_eq!(app.editor.text(), "draft for ada");
}

#[test]
fn reopening_the_chat_already_open_gives_up_a_stale_edit_instead_of_hiding_it() {
    let mut app = app_with_my_message(Keymap::Standard);
    press(&mut app, KeyCode::F(4), KeyModifiers::NONE);
    assert!(app.is_editing());
    type_text(&mut app, " extra");
    let chat = app.current_chat().unwrap().clone();
    // `:new` to a contact whose chat is already open reopens it through `show_chat`, the same as `Ev::Opened` would;
    // `select_chat` alone does not notice, since the id does not change.
    app.show_chat(chat);
    assert!(
        !app.is_editing(),
        "show_chat must not leave a stale edit active behind a box that now looks like a fresh compose field"
    );
    assert_eq!(
        app.editor.text(),
        "",
        "no draft for this chat, and not the edit text"
    );
    assert!(!last_message(&app).body.contains("extra"));
}

#[test]
fn replying_while_editing_is_refused_instead_of_corrupting_the_edit() {
    let mut app = app_with_my_message(Keymap::Standard);
    press(&mut app, KeyCode::F(4), KeyModifiers::NONE);
    assert!(app.is_editing());
    let before = app.editor.text();
    app.reply_to_selected();
    assert!(app.is_editing(), "the edit is still in progress");
    assert_eq!(
        app.editor.text(),
        before,
        "no quote was appended to the edit"
    );
    assert!(app.status.as_ref().unwrap().0.contains("already editing"));
}

#[test]
fn sending_a_reply_quotes_the_selected_message() {
    let (mut app, _) = standard_app();
    select_last_message(&mut app);
    let quoted = last_message(&app).clone();
    app.reply_to_selected();
    assert!(app.is_replying());
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(
        app.editor.text(),
        "",
        "nothing is pre-filled; the backend builds the quote once it is sent"
    );
    type_text(&mut app, "sounds good");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        !app.is_replying(),
        "the reply target is spent once the send is dispatched"
    );
    pump(&mut app, |a| a.sending == 0);
    let sent = last_message(&app);
    assert!(
        sent.body.contains("schema.skype.com/Reply"),
        "{:?}",
        sent.body
    );
    assert!(sent.body.contains(&quoted.from_name), "{:?}", sent.body);
}

#[test]
fn escape_cancels_a_pending_reply_without_sending_it() {
    let (mut app, _) = standard_app();
    select_last_message(&mut app);
    app.reply_to_selected();
    assert!(app.is_replying());
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!app.is_replying());
    assert_eq!(app.focus, Focus::Messages);
}

#[test]
fn switching_chats_cancels_a_pending_reply() {
    let (mut app, _) = standard_app();
    select_last_message(&mut app);
    app.reply_to_selected();
    assert!(app.is_replying());
    app.select_chat("demo-grace");
    assert!(
        !app.is_replying(),
        "a reply target from the chat left behind must not attach to whatever is sent next"
    );
}

#[test]
fn a_stale_edit_confirmation_does_not_undo_a_newer_one_sent_after_cancelling() {
    let mut app = app_with_my_message(Keymap::Standard);
    let chat_id = app.current.clone().unwrap();
    let message_id = last_message(&app).id.clone();

    // Attempt one is dispatched (token 1) and its confirmation is left sitting unprocessed, as it would be while
    // still in flight on a worker thread, then the user gives up on it.
    press(&mut app, KeyCode::F(4), KeyModifiers::NONE);
    app.editor.set_text("edit one");
    app.finish_edit();
    assert!(app.is_editing(), "still saving the first attempt");
    app.cancel_edit(false);
    assert!(!app.is_editing());

    // Attempt two edits the same message again (token 2) and is confirmed first.
    press(&mut app, KeyCode::F(4), KeyModifiers::NONE);
    app.editor.set_text("edit two");
    app.finish_edit();
    app.on_edited(
        chat_id.clone(),
        message_id.clone(),
        "edit two".into(),
        2,
        Ok(()),
    );
    assert!(!app.is_editing(), "the confirmed edit ends the attempt");
    assert_eq!(last_message(&app).body, "edit two");

    // Attempt one's confirmation, for an edit abandoned locally, arrives after attempt two's.
    app.on_edited(chat_id, message_id, "edit one".into(), 1, Ok(()));
    assert_eq!(
        last_message(&app).body,
        "edit two",
        "a stale confirmation for the cancelled first attempt must not revert the newer edit"
    );
}

/// A backend that sends but refuses every edit.
struct RefusesEdits(DemoBackend);

impl Backend for RefusesEdits {
    fn label(&self) -> String {
        self.0.label()
    }
    fn me(&self) -> Result<Me, Error> {
        self.0.me()
    }
    fn chats(&self) -> Result<Vec<Chat>, Error> {
        self.0.chats()
    }
    fn find_people(&self, q: &str) -> Result<Vec<Contact>, Error> {
        self.0.find_people(q)
    }
    fn open_chat(&self, who: &Contact) -> Result<Chat, Error> {
        self.0.open_chat(who)
    }
    fn messages(&self, id: &str, older: Option<&str>) -> Result<Page, Error> {
        self.0.messages(id, older)
    }
    fn send(
        &self,
        id: &str,
        html: &str,
        m: &[teams_client::OutMention],
        reply_to: Option<&teams_client::ReplyTarget>,
    ) -> Result<Msg, Error> {
        self.0.send(id, html, m, reply_to)
    }
    fn can_edit(&self) -> bool {
        true
    }
    fn edit(&self, _: &str, _: &str, _: &str, _: &[teams_client::OutMention]) -> Result<(), Error> {
        Err(Error::Status {
            operation: "edit message",
            status: 403,
            message: "not yours".into(),
        })
    }
    fn serves_image(&self, url: &str) -> bool {
        self.0.serves_image(url)
    }
    fn image(&self, url: &str) -> Result<RgbImage, Error> {
        self.0.image(url)
    }
}

#[test]
fn a_refused_edit_keeps_the_text_in_the_box_and_the_message_as_it_was() {
    let (mut app, _) = App::for_test(Arc::new(RefusesEdits(DemoBackend::instant())));
    app.set_keymap(Keymap::Standard);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.focus = Focus::Editor;
    type_text(&mut app, "first words");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    pump(&mut app, |a| a.sending == 0);
    app.focus = Focus::Messages;
    select_last_message(&mut app);
    press(&mut app, KeyCode::F(4), KeyModifiers::NONE);
    app.editor.set_text("changed my mind");
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    pump(&mut app, |a| {
        a.status
            .as_ref()
            .is_some_and(|(m, ..)| m.contains("edit failed"))
    });
    assert!(
        app.is_editing(),
        "still editing, so it can be tried again or given up"
    );
    assert_eq!(app.editor.text(), "changed my mind");
    assert!(last_message(&app).body.contains("first words") && last_message(&app).edited.is_none());
    // Enter again is allowed (the first attempt is over).
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    pump(&mut app, |a| {
        a.status
            .as_ref()
            .is_some_and(|(m, ..)| m.contains("edit failed"))
    });
}

// --- favourites ---------------------------------------------------------------------------------------------------

fn listed_ids(app: &App) -> Vec<String> {
    app.visible_chats()
        .iter()
        .map(|&i| app.chats.items()[i].id.clone())
        .collect()
}

#[test]
fn favourites_are_pinned_above_the_rest_in_their_own_order_and_the_others_stay_newest_first() {
    let mut app = app();
    let ids = listed_ids(&app);
    // The demo's favourites are the moon chat (1) and Ada (2); neither is the newest.
    assert_eq!(&ids[..2], ["demo-moon", "demo-ada"], "{ids:?}");
    assert_eq!(app.pinned_count(&app.visible_chats()), 2);
    let others: Vec<&String> = ids[2..].iter().collect();
    let recency: Vec<String> = {
        let mut rest: Vec<&teams_client::Chat> = app
            .chats
            .items()
            .iter()
            .filter(|c| c.favorite.is_none())
            .collect();
        rest.sort_by_key(|c| std::cmp::Reverse(c.updated));
        rest.iter().map(|c| c.id.clone()).collect()
    };
    assert_eq!(others, recency.iter().collect::<Vec<_>>());
    // Not pinned: one list, newest first, no divider.
    app.pin_favorites = false;
    let flat = listed_ids(&app);
    assert_eq!(app.pinned_count(&app.visible_chats()), 0);
    let mut by_recency: Vec<&teams_client::Chat> = app.chats.items().iter().collect();
    by_recency.sort_by_key(|c| std::cmp::Reverse(c.updated));
    assert_eq!(
        flat,
        by_recency.iter().map(|c| c.id.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn the_list_shows_a_star_on_favourites_and_a_thin_divider_below_them_and_clicks_skip_the_divider() {
    let mut app = app();
    let screen = crate::ui::render_to_string(&mut app, 120, 40);
    let rows: Vec<&str> = screen.lines().collect();
    let list_width = app.view.geometry.list_body.width as usize;
    let divider = rows
        .iter()
        .position(|l| l.trim_start().starts_with(&"─".repeat(list_width.min(10))))
        .expect("a divider");
    assert_eq!(
        rows.iter().filter(|l| l.contains('★')).count(),
        2,
        "one star for each favourite: {screen}"
    );
    // Two chats of two rows above it, and the stars sit on their second rows.
    let top = app.view.geometry.list_body.y as usize;
    assert_eq!(divider, top + 4, "{screen}");
    assert!(rows[top + 1].contains('★') && rows[top + 3].contains('★'));
    // A click on the divider opens nothing; a click on the row under it opens the first of the others.
    let before = app.current.clone();
    let x = app.view.geometry.list_body.x + 2;
    click(&mut app, x, (top + 4) as u16);
    assert_eq!(app.current, before);
    click(&mut app, x, (top + 5) as u16);
    let first_other = listed_ids(&app)[2].clone();
    assert_eq!(app.current.as_deref(), Some(first_other.as_str()));
    // And one above it opens the second favourite.
    click(&mut app, x, (top + 2) as u16);
    assert_eq!(app.current.as_deref(), Some("demo-ada"));
}

#[test]
fn moving_through_the_list_follows_what_is_drawn() {
    let mut app = app();
    app.select_chat("demo-moon");
    app.focus = Focus::Chats;
    app.on_key(key('j'));
    assert_eq!(
        app.current.as_deref(),
        Some("demo-ada"),
        "the second favourite"
    );
    app.on_key(key('j'));
    assert_eq!(
        app.current.as_deref(),
        Some(listed_ids(&app)[2].as_str()),
        "then the newest of the others, across the divider"
    );
}

#[test]
fn f_favourites_the_open_chat_at_once_and_again_takes_it_out() {
    let mut app = app();
    let other = listed_ids(&app)[2].clone();
    app.select_chat(&other);
    app.focus = Focus::Chats;
    app.on_key(key('f'));
    let fav = |app: &App, id: &str| {
        app.chats
            .items()
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .favorite
    };
    assert_eq!(
        fav(&app, &other),
        Some(3),
        "after the last favourite, before the service has answered"
    );
    assert_eq!(
        listed_ids(&app)[2],
        other,
        "it moved under the other favourites"
    );
    pump(&mut app, |a| {
        a.status.as_ref().is_some_and(|(m, ..)| m.contains("added"))
    });
    // The service's own list agrees.
    app.run_command("refresh");
    pump(&mut app, |a| a.settled());
    assert_eq!(fav(&app, &other), Some(3));
    app.on_key(key('f'));
    assert_eq!(fav(&app, &other), None);
    pump(&mut app, |a| {
        a.status
            .as_ref()
            .is_some_and(|(m, ..)| m.contains("taken out"))
    });
    app.run_command("refresh");
    pump(&mut app, |a| a.settled());
    assert_eq!(fav(&app, &other), None);
}

#[test]
fn f7_and_f8_work_in_the_standard_keymap_from_any_pane() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Editor;
    let id = app.current.clone().unwrap();
    let was = app.current_chat().unwrap().favorite;
    press(&mut app, KeyCode::F(7), KeyModifiers::NONE);
    assert_ne!(app.current_chat().unwrap().favorite, was, "toggled");
    assert_eq!(app.editor.text(), "", "nothing was typed");
    assert_eq!(app.current.as_deref(), Some(id.as_str()));
    assert!(app.pin_favorites);
    press(&mut app, KeyCode::F(8), KeyModifiers::NONE);
    assert!(!app.pin_favorites);
    assert!(
        app.status
            .as_ref()
            .unwrap()
            .0
            .contains("with the other chats")
    );
    press(&mut app, KeyCode::F(8), KeyModifiers::NONE);
    assert!(app.pin_favorites);
    // `F` in the vim keymap does the same as F8.
    app.set_keymap(Keymap::Vim);
    app.focus = Focus::Chats;
    app.on_key(key('F'));
    assert!(!app.pin_favorites);
}

struct RefusesFavourites(DemoBackend);

impl Backend for RefusesFavourites {
    fn label(&self) -> String {
        self.0.label()
    }
    fn me(&self) -> Result<Me, Error> {
        self.0.me()
    }
    fn chats(&self) -> Result<Vec<Chat>, Error> {
        self.0.chats()
    }
    fn find_people(&self, q: &str) -> Result<Vec<Contact>, Error> {
        self.0.find_people(q)
    }
    fn open_chat(&self, who: &Contact) -> Result<Chat, Error> {
        self.0.open_chat(who)
    }
    fn messages(&self, id: &str, older: Option<&str>) -> Result<Page, Error> {
        self.0.messages(id, older)
    }
    fn send(
        &self,
        id: &str,
        html: &str,
        m: &[teams_client::OutMention],
        reply_to: Option<&teams_client::ReplyTarget>,
    ) -> Result<Msg, Error> {
        self.0.send(id, html, m, reply_to)
    }
    fn can_favorite(&self) -> bool {
        true
    }
    fn set_favorite(&self, _: &str, _: bool) -> Result<(), Error> {
        Err(Error::Status {
            operation: "change favourites",
            status: 403,
            message: "no".into(),
        })
    }
    fn serves_image(&self, url: &str) -> bool {
        self.0.serves_image(url)
    }
    fn image(&self, url: &str) -> Result<RgbImage, Error> {
        self.0.image(url)
    }
}

#[test]
fn a_refused_favourite_puts_the_chat_back_where_it_was_and_says_why() {
    let (mut app, _) = App::for_test(Arc::new(RefusesFavourites(DemoBackend::instant())));
    app.set_keymap(Keymap::Vim);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    let before = listed_ids(&app);
    let other = before[2].clone();
    app.select_chat(&other);
    app.focus = Focus::Chats;
    app.on_key(key('f'));
    pump(&mut app, |a| {
        a.status
            .as_ref()
            .is_some_and(|(m, ..)| m.contains("could not change"))
    });
    assert_eq!(listed_ids(&app), before, "back in its place");
    assert_eq!(app.current_chat().unwrap().favorite, None);
    // Unfavouriting one that was a favourite is put back too.
    app.select_chat("demo-moon");
    app.on_key(key('f'));
    pump(&mut app, |a| {
        a.chats
            .items()
            .iter()
            .find(|c| c.id == "demo-moon")
            .unwrap()
            .favorite
            == Some(1)
    });
    assert_eq!(listed_ids(&app), before);
}

#[test]
fn a_service_without_favourites_says_so() {
    let (mut app, _) = App::for_test(Lapsed::new(false, false));
    app.set_keymap(Keymap::Vim);
    app.start();
    pump(&mut app, |a| a.chats.loaded());
    app.focus = Focus::Chats;
    app.on_key(key('f'));
    assert!(app.status.as_ref().unwrap().0.contains("no favourites"));
}

fn refused_sign_in() -> Error {
    Error::Auth(teams_client::AuthError::NotSignedIn)
}

#[test]
fn a_side_call_that_is_refused_does_not_declare_the_whole_sign_in_lost() {
    let backend = Lapsed::new(true, true);
    let mut app = lapsed_app(&backend);
    pump(&mut app, |a| a.chats.loaded());
    // Each of these needs a permission of its own, which a tenant can withhold while chats still work.
    app.on_event(Ev::Image {
        url: "https://example.invalid/a.png".into(),
        result: Err(refused_sign_in()),
    });
    app.on_event(Ev::People {
        query: "bob".into(),
        result: Err(refused_sign_in()),
    });
    app.on_event(Ev::ReactionEmoji(Err(refused_sign_in())));
    app.on_event(Ev::MarkedRead {
        chat_id: "demo-ada".into(),
        result: Err(refused_sign_in()),
    });
    app.on_event(Ev::Warning(refused_sign_in()));
    assert_eq!(app.sign_in, SignIn::Fine, "still signed in");
    assert_eq!(app.overlay, Overlay::None, "no popup");
    let said = app.status.as_ref().map(|(m, ..)| m.clone()).unwrap();
    assert!(
        !said.contains("signed out") && !said.contains("F6"),
        "a failed side call must not say the session ended: {said}"
    );
    // A call that does use the main credentials still does.
    app.on_event(Ev::Chats(Err(refused_sign_in())));
    assert!(matches!(app.sign_in, SignIn::Expired { .. }));
}

#[test]
fn a_push_hint_for_a_chat_visited_earlier_but_not_open_makes_the_list_fetch_sooner() {
    let (mut app, demo) = demo_app();
    let first = app.current.clone().unwrap();
    let other = app
        .chats
        .items()
        .iter()
        .map(|c| c.id.clone())
        .find(|id| *id != first)
        .unwrap();
    // Visit another chat, so that it keeps a state of its own, then come back.
    app.select_chat(&other);
    app.select_chat(&first);
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    assert!(app.states.contains_key(&other), "it has a state");
    app.states
        .get_mut(&first)
        .unwrap()
        .sched
        .set_last(Instant::now());
    app.chats.sched.set_last(Instant::now());
    demo.queue_push(PushHint::Chat(other));
    app.on_tick();
    assert!(
        app.chats.is_fetching(),
        "nothing polls a chat that is not open, so the list has to hear about it"
    );
    assert!(
        !app.states[&first].is_fetching(),
        "the open chat was left alone"
    );
}

#[test]
fn a_refused_send_does_not_overwrite_a_reply_started_in_the_meantime() {
    let mut app = app_with(Misbehaving::refusing_sends());
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    compose_and_send(&mut app, "hello");
    // The box is empty and the answer has not arrived: choose a message to answer next.
    app.replying = Some(super::reply::Replying {
        target: teams_client::ReplyTarget {
            id: "42".into(),
            from_id: Some("ada".into()),
            from_name: "Ada".into(),
            preview: "the newer reply".into(),
        },
    });
    app.on_tick();
    assert!(
        app.editor.is_empty(),
        "the refused text did not land in the box"
    );
    assert_eq!(
        app.replying.as_ref().map(|r| r.target.id.as_str()),
        Some("42"),
        "the reply that was started afterwards is still pending"
    );
    assert!(
        matches!(&last_message(&app).delivery, Delivery::Failed(_)),
        "the refused message stays in the transcript to be copied from"
    );
}

#[test]
fn a_reply_to_your_own_message_quotes_you_by_name_not_as_you() {
    let mut app = app_with_my_message(Keymap::Standard);
    app.reply_to_selected();
    let target = &app.replying.as_ref().expect("replying").target;
    let me = app.me.as_ref().unwrap().name.clone();
    assert_eq!(target.from_name, me);
    assert_ne!(target.from_name, "You");
}

/// AltGr as Windows reports it: Control and Alt together.
fn altgr(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL | KeyModifiers::ALT)
}

#[test]
fn a_character_typed_with_altgr_reaches_every_place_text_is_typed() {
    let (mut app, _) = standard_app();

    // The command panel (`:new user@example.com` on a German layout).
    app.cmdline = Some(String::new());
    app.on_key(altgr('@'));
    app.on_key(altgr('€'));
    assert_eq!(app.cmdline.as_deref(), Some("@€"));
    // ...where Control alone is still a shortcut, with or without AltGr's help for other letters.
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(app.cmdline.as_deref(), Some(""), "Ctrl-U clears");
    app.cmdline = None;

    // The chat filter.
    app.filtering = true;
    app.filter.clear();
    app.on_key(altgr('@'));
    assert_eq!(app.filter, "@");
    app.filtering = false;
    app.filter.clear();

    // Typing in the chat list starts a filter, and in the conversation starts a message.
    app.focus = Focus::Chats;
    app.on_key(altgr('@'));
    assert!(app.filtering && app.filter == "@");
    app.filtering = false;
    app.filter.clear();
    app.focus = Focus::Messages;
    app.on_key(altgr('@'));
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(app.editor.text(), "@");

    // The emoji search.
    app.editor.set_text("");
    app.focus = Focus::Editor;
    ctrl(&mut app, 'e');
    app.on_key(altgr('@'));
    assert!(
        matches!(emoji_overlay(&app), Some((_, "@", _))),
        "{:?}",
        emoji_overlay(&app).map(|(_, q, _)| q.to_string())
    );
}

#[test]
fn control_or_alt_alone_still_does_not_type_a_character_in_the_panes() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Chats;
    app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
    assert!(
        !app.filtering,
        "Alt-x is a shortcut, not a letter to filter by"
    );
    app.focus = Focus::Messages;
    app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
    assert_eq!(app.focus, Focus::Messages, "nor does it start a message");
}

#[test]
fn a_dump_never_marks_a_chat_read_on_the_service() {
    let (mut app, demo) = demo_app();
    app.read_only = true;
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    ticks(&mut app, 8);
    assert!(
        demo.read_marks().is_empty(),
        "looking at a chat for a screenshot must not change what other devices show as unread"
    );
}

/// A standard-keymap app in the demo's group chat, with the message box focused.
fn mention_app() -> App {
    let (mut app, _) = standard_app();
    app.select_chat("demo-platform");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.focus = Focus::Editor;
    app
}

#[test]
fn typing_an_at_offers_the_people_of_the_chat_and_enter_chooses_instead_of_sending() {
    let mut app = mention_app();
    type_text(&mut app, "see @");
    let all = app.mention_suggestions().expect("everyone is offered");
    assert_eq!(all.people.len(), 3, "{all:?}");
    assert!(
        all.people.iter().all(|p| p.id != app.me_id()),
        "not yourself"
    );
    type_text(&mut app, "mar");
    let s = app.mention_suggestions().expect("narrowed");
    assert_eq!(s.people.len(), 1, "{s:?}");
    assert!(s.people[0].name.starts_with("Margaret"), "{s:?}");
    let name = s.people[0].name.clone();
    let before = app.states["demo-platform"].messages().len();
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(app.editor.text(), format!("see @{name} "));
    assert_eq!(app.sending, 0, "choosing is not sending");
    assert_eq!(app.states["demo-platform"].messages().len(), before);
    assert!(app.mention_suggestions().is_none(), "done");
    // Sent as a real mention of her.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    pump(&mut app, |a| a.sending == 0);
    let sent = app.states["demo-platform"]
        .messages()
        .last()
        .unwrap()
        .clone();
    assert!(sent.body.contains("<at"), "{}", sent.body);
}

#[test]
fn up_and_down_move_through_the_offered_people_and_tab_chooses_the_one_marked() {
    let mut app = mention_app();
    type_text(&mut app, "@");
    let first = app.mention_suggestions().unwrap();
    assert_eq!(first.sel, 0);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    let second = app.mention_suggestions().unwrap();
    assert_eq!(second.sel, 1);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    press(&mut app, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.mention_suggestions().unwrap().sel, 2, "wraps round");
    let want = app.mention_suggestions().unwrap().people[2].name.clone();
    press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.editor.text(), format!("@{want} "));
}

#[test]
fn esc_closes_the_list_without_leaving_the_message_box_and_it_stays_closed_for_that_at() {
    let mut app = mention_app();
    type_text(&mut app, "@ad");
    assert!(app.mention_suggestions().is_some());
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Editor);
    assert!(app.mention_suggestions().is_none());
    type_text(&mut app, "a");
    assert!(app.mention_suggestions().is_none(), "still closed");
    // A different @ is offered again.
    type_text(&mut app, " and @li");
    assert!(app.mention_suggestions().is_some());
}

#[test]
fn an_email_address_or_code_is_not_a_mention_and_nothing_matching_offers_nothing() {
    let mut app = mention_app();
    type_text(&mut app, "mail ada@ex");
    assert!(app.mention_suggestions().is_none(), "an address");
    app.editor.clear();
    type_text(&mut app, "`@ad");
    assert!(app.mention_suggestions().is_none(), "inline code");
    app.editor.clear();
    type_text(&mut app, "```\n@ad");
    assert!(app.mention_suggestions().is_none(), "a fenced block");
    app.editor.clear();
    type_text(&mut app, "@zzzz");
    assert!(app.mention_suggestions().is_none(), "nobody called that");
    // Enter then does what it always did.
    press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
    pump(&mut app, |a| a.sending == 0);
}

#[test]
fn the_offered_people_are_drawn_above_the_message_box() {
    let mut app = mention_app();
    type_text(&mut app, "@");
    let screen = crate::ui::render_to_string(&mut app, 120, 30);
    assert!(screen.contains("@ mention"), "{screen}");
    let name = app.mention_suggestions().unwrap().people[0].name.clone();
    assert!(screen.contains(&name), "{screen}");
}

#[test]
fn in_vim_the_list_is_offered_only_while_typing_and_esc_closes_it_before_it_leaves_insert() {
    let (mut app, _) = app_and_fakes();
    app.select_chat("demo-platform");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.focus = Focus::Editor;
    app.editor.set_mode(crate::editor::Mode::Insert);
    type_text(&mut app, "@mar");
    assert!(app.mention_suggestions().is_some());
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.editor.mode(), crate::editor::Mode::Insert);
    assert!(app.mention_suggestions().is_none());
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.editor.mode(), crate::editor::Mode::Normal);
}

#[test]
fn a_click_in_the_message_box_puts_the_cursor_where_it_was_clicked() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Messages;
    app.editor.set_text("hello world\nsecond line");
    crate::ui::render_to_string(&mut app, 120, 40);
    let editor = app.view.geometry.editor;
    let (x0, y0) = (editor.x + 2, editor.y + 1);
    // On the 'w' of the first line.
    click(&mut app, x0 + 6, y0);
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(app.editor.cursor(), (0, 6));
    // The second line, between its letters.
    click(&mut app, x0 + 3, y0 + 1);
    assert_eq!(app.editor.cursor(), (1, 3));
    // Beyond the end of a line: after its last character.
    click(&mut app, x0 + 40, y0);
    assert_eq!(app.editor.cursor(), (0, 11));
    // Typing lands at the click.
    click(&mut app, x0 + 5, y0);
    app.on_key(key('!'));
    assert_eq!(app.editor.text(), "hello! world\nsecond line");
}

#[test]
fn a_click_in_a_wrapped_line_of_the_message_box_lands_on_the_row_clicked() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Messages;
    let long = "abcdefghij ".repeat(30);
    app.editor.set_text(long.trim_end());
    crate::ui::render_to_string(&mut app, 60, 30);
    let editor = app.view.geometry.editor;
    assert!(app.view.editor_rows.spans.len() > 2, "the line wraps");
    let second = app.view.editor_rows.spans[1].first;
    click(&mut app, editor.x + 2, editor.y + 2);
    assert_eq!(app.editor.cursor(), (0, second), "start of the second row");
}

#[test]
fn a_click_on_the_border_of_the_message_box_only_focuses_it() {
    let (mut app, _) = standard_app();
    app.focus = Focus::Messages;
    app.editor.set_text("hello");
    crate::ui::render_to_string(&mut app, 120, 40);
    let editor = app.view.geometry.editor;
    let before = app.editor.cursor();
    click(&mut app, editor.x + 4, editor.y);
    assert_eq!(app.focus, Focus::Editor);
    assert_eq!(app.editor.cursor(), before);
}

/// [`Eventful`], noting which chats an older page was asked for.
struct CountingOlder {
    inner: Eventful,
    older_reads: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Backend for CountingOlder {
    fn label(&self) -> String {
        self.inner.label()
    }
    fn me(&self) -> Result<Me, Error> {
        self.inner.me()
    }
    fn chats(&self) -> Result<Vec<Chat>, Error> {
        self.inner.chats()
    }
    fn find_people(&self, q: &str) -> Result<Vec<Contact>, Error> {
        self.inner.find_people(q)
    }
    fn open_chat(&self, who: &Contact) -> Result<Chat, Error> {
        self.inner.open_chat(who)
    }
    fn messages(&self, id: &str, older: Option<&str>) -> Result<Page, Error> {
        if older.is_some() {
            self.older_reads.lock().unwrap().push(id.to_string());
        }
        self.inner.messages(id, older)
    }
    fn send(
        &self,
        id: &str,
        html: &str,
        m: &[teams_client::OutMention],
        reply_to: Option<&teams_client::ReplyTarget>,
    ) -> Result<Msg, Error> {
        self.inner.send(id, html, m, reply_to)
    }
    fn serves_image(&self, url: &str) -> bool {
        self.inner.serves_image(url)
    }
    fn image(&self, url: &str) -> Result<RgbImage, Error> {
        self.inner.image(url)
    }
}

#[test]
fn automatic_older_pages_stop_when_chat_is_no_longer_open() {
    let older_reads = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (mut app, _) = App::for_test(Arc::new(CountingOlder {
        inner: Eventful {
            inner: DemoBackend::instant(),
            real_after: usize::MAX,
            // The first automatic page fails, so nothing is in flight when the person moves on.
            fail_from: Some(1),
        },
        older_reads: Arc::clone(&older_reads),
    }));
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.select_chat("demo-ada");
    pump(&mut app, |a| {
        a.state().is_some_and(|s| s.loaded && !s.is_fetching())
    });
    app.select_chat("demo-bob");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    older_reads.lock().unwrap().clear();
    // An automatic page for the chat left behind arrives late (as one in flight would): it is taken in, but it does
    // not ask for another.
    let events: Vec<Msg> = (0..12).map(|i| Eventful::event(i, 3)).collect();
    app.on_event(Ev::Messages {
        chat_id: "demo-ada".into(),
        fetch: Fetch::Older { automatic: true },
        result: Ok(Page {
            messages: events,
            older: Some("4".into()),
        }),
    });
    for _ in 0..10 {
        app.on_tick();
    }
    let reads = older_reads.lock().unwrap();
    assert!(
        !reads.iter().any(|id| id == "demo-ada"),
        "no more automatic pages for a chat that is no longer open: {reads:?}"
    );
}
#[test]
fn reopening_a_now_read_chat_clears_the_new_divider() {
    let (mut app, _) = app_and_fakes();
    let ada_id = "demo-ada".to_string();
    app.select_chat(&ada_id);
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    let marker_when_unread = app
        .chats
        .items()
        .iter()
        .find(|c| c.id == ada_id)
        .and_then(|c| c.last_read);
    assert!(
        marker_when_unread.is_some(),
        "chat should have a read marker"
    );
    if let Some(st) = app.states.get(&ada_id) {
        assert_eq!(
            st.unread_marker, marker_when_unread,
            "marker should be set when opening unread chat"
        );
    }
    let me_id = app.me_id().to_string();
    app.state_mut().unwrap().edit_messages(|msgs| {
        for m in msgs {
            if m.from_id.as_ref().is_some_and(|from| from != &me_id) {
                m.modified = Utc::now() - chrono::Duration::hours(2);
                m.created = m.modified;
            }
        }
    });
    app.read.mark_local(ada_id.clone(), Utc::now());
    app.select_chat("demo-bob");
    app.select_chat(&ada_id);
    if let Some(st) = app.states.get(&ada_id) {
        assert_eq!(
            st.unread_marker, None,
            "the unread marker must be cleared when reopening a now-read chat"
        );
    }
}

#[test]
fn unread_marker_is_part_of_transcript_key() {
    use crate::transcript::TranscriptKey;
    let key1 = TranscriptKey {
        width: 80,
        version: 1,
        images: 0,
        mode: crate::imgart::ImgMode::Blocks,
        spinner: 0,
        day: chrono::Utc::now().date_naive(),
        unread_marker: Some(Utc::now() - chrono::Duration::hours(1)),
        unlisted: Default::default(),
    };
    let key2 = TranscriptKey {
        unread_marker: None,
        ..key1.clone()
    };
    assert_ne!(
        key1, key2,
        "transcript key must differ when unread marker changes"
    );
}

mod banner;
mod gfx;
mod lanes;
mod posting;
mod search;
mod teams;
