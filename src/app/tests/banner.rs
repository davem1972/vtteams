//! The banner above the message box: people's status messages and out-of-office notices, through the real paths (the
//! presence request, the answer, the layout, the frame and the mouse), on the demo backend and a crowd of its own.

use super::*;
use crate::demo::ME_ID;
use crate::theme::{OUT_OF_OFFICE, SUBTLE};
use ratatui::style::Modifier;
use std::collections::HashMap;
use std::sync::Mutex;
use teams_client::{Availability, Chat, ChatKind, Member, Presence, StatusNote};

const CROWD: &str = "crowd-chat";
const CROWD_SIZE: usize = 60;

/// The demo's chats and one more: a group chat of you and [`CROWD_SIZE`] people, whose presence answers a test scripts.
/// It records everyone the app asks about.
struct Crowd {
    inner: DemoBackend,
    script: Mutex<HashMap<String, Presence>>,
    /// The ids of each request, in order.
    asks: Mutex<Vec<Vec<String>>>,
}

impl Crowd {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: DemoBackend::instant(),
            script: Mutex::new(HashMap::new()),
            asks: Mutex::new(Vec::new()),
        })
    }

    fn say(&self, id: &str, presence: Presence) {
        self.script.lock().unwrap().insert(id.to_string(), presence);
    }

    fn asked(&self) -> Vec<Vec<String>> {
        self.asks.lock().unwrap().clone()
    }
}

fn person(n: usize) -> String {
    format!("crowd-{n}")
}

impl Backend for Crowd {
    fn label(&self) -> String {
        self.inner.label()
    }
    fn me(&self) -> Result<Me, Error> {
        self.inner.me()
    }
    fn chats(&self) -> Result<Vec<Chat>, Error> {
        let mut chats = self.inner.chats()?;
        let mut members = vec![Member {
            not_in_directory: false,
            id: ME_ID.into(),
            name: "John Titor".into(),
        }];
        members.extend((0..CROWD_SIZE).map(|n| Member {
            not_in_directory: false,
            id: person(n),
            name: format!("Person {n}"),
        }));
        chats.push(Chat {
            id: CROWD.into(),
            kind: ChatKind::Group,
            topic: Some("The crowd".into()),
            members,
            updated: Utc::now() - chrono::Duration::days(300),
            preview: None,
            last_read: None,
            web_url: None,
            favorite: None,
        });
        Ok(chats)
    }
    fn find_people(&self, q: &str) -> Result<Vec<Contact>, Error> {
        self.inner.find_people(q)
    }
    fn open_chat(&self, who: &Contact) -> Result<Chat, Error> {
        self.inner.open_chat(who)
    }
    fn messages(&self, id: &str, older: Option<&str>) -> Result<Page, Error> {
        match self.inner.messages(id, older) {
            Err(Error::InvalidInput { .. }) => Ok(Page::default()),
            other => other,
        }
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
        true
    }
    fn presence(&self, ids: &[String]) -> Result<HashMap<String, Presence>, Error> {
        self.asks.lock().unwrap().push(ids.to_vec());
        let mut answer = self.inner.presence(ids)?;
        let script = self.script.lock().unwrap();
        for id in ids {
            if let Some(p) = script.get(id) {
                answer.insert(id.clone(), p.clone());
            }
        }
        Ok(answer)
    }
    fn serves_image(&self, url: &str) -> bool {
        self.inner.serves_image(url)
    }
    fn image(&self, url: &str) -> Result<RgbImage, Error> {
        self.inner.image(url)
    }
}

fn started(backend: &Arc<Crowd>) -> App {
    let (mut app, _) = App::for_test(backend.clone());
    app.set_keymap(Keymap::Standard);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app
}

/// Open `chat`, wait until its messages are there and let the people on screen who have not been asked about yet be
/// asked and answered (jobs run on the calling thread, the answer is delivered by a tick).
fn open(app: &mut App, chat: &str) {
    app.select_chat(chat);
    pump(app, |a| a.state().is_some_and(|s| s.loaded));
    app.presences.sched.set_last(ago(10));
    for _ in 0..4 {
        app.on_tick();
    }
    assert!(!app.presences.in_flight);
}

fn plain() -> (App, Arc<Crowd>) {
    let backend = Crowd::new();
    let app = started(&backend);
    (app, backend)
}

fn note(text: &str, in_minutes: Option<i64>) -> StatusNote {
    StatusNote::new(text)
        .expect("a note")
        .expiring(in_minutes.map(|m| Utc::now() + chrono::Duration::minutes(m)))
}

fn screen(app: &mut App) -> String {
    crate::ui::render_to_string(app, 120, 40)
}

/// The text of the banner's rows as drawn, from the frame.
fn banner_rows(app: &App, frame: &str) -> Vec<String> {
    let b = app.view.geometry.banner;
    (b.y..b.bottom())
        .map(|y| {
            frame
                .lines()
                .nth(usize::from(y))
                .unwrap_or("")
                .chars()
                .skip(usize::from(b.x))
                .take(usize::from(b.width))
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[test]
fn a_status_message_shows_just_above_the_message_box_in_a_one_to_one_chat() {
    let (mut app, _) = plain();
    open(&mut app, "demo-ada");
    let frame = screen(&mut app);
    let g = &app.view.geometry;
    assert_eq!(g.banner.height, 1, "{frame}");
    assert_eq!(g.banner.bottom(), g.editor.y, "directly above the box");
    assert_eq!(
        g.transcript.bottom(),
        g.banner.y,
        "directly below the messages"
    );
    assert_eq!(
        banner_rows(&app, &frame),
        [" Ada Lovelace: Working on the Analytical Engine, ping me here"],
        "plain text: the markup the service sent is gone\n{frame}"
    );
}

#[test]
fn a_meeting_with_two_people_out_of_office_and_a_note_shows_two_rows_and_how_many_more() {
    let (mut app, backend) = plain();
    open(&mut app, "demo-sprint");
    let frame = screen(&mut app);
    assert_eq!(app.view.geometry.banner.height, 2, "{frame}");
    let rows = banner_rows(&app, &frame);
    assert_eq!(rows[0], " Alan Turing is out of office", "{frame}");
    assert!(
        rows[1].starts_with(" Katherine Johnson is out of office until ")
            && rows[1].contains(": Back on Monday")
            && rows[1].ends_with(" +1 more"),
        "the second out-of-office person, and the status message left as a count: {rows:?}"
    );
    // Nobody else's chat is asked about for it: the roster of the open chat is.
    assert!(
        backend
            .asked()
            .iter()
            .flatten()
            .any(|id| id == "u-katherine" || id == "u-alan"),
        "the members were asked about"
    );
}

#[test]
fn a_note_whose_time_is_up_is_not_shown_and_leaves_with_the_clock() {
    let (mut app, _) = plain();
    open(&mut app, "demo-ada");
    let ada = || Presence::new(Availability::Available, false).with_note(note("Lunch", Some(-1)));
    // An answer that is already out of date by the time it is shown.
    app.presences.known.insert("u-ada".into(), ada());
    let frame = screen(&mut app);
    assert_eq!(app.view.geometry.banner, Rect::default(), "{frame}");
    assert!(!frame.contains("Lunch"));
    // A note with time left is shown, and the same answer read later in the day is gone: the clock decides.
    app.presences.known.insert(
        "u-ada".into(),
        Presence::new(Availability::Available, false).with_note(note("Lunch", Some(30))),
    );
    assert!(screen(&mut app).contains("Ada Lovelace: Lunch"));
    let soon = Utc::now() + chrono::Duration::minutes(10);
    let later = Utc::now() + chrono::Duration::minutes(31);
    assert_eq!(app.banner_entries(soon).len(), 1);
    assert!(app.banner_entries(later).is_empty());
    // Out of office whose reply has ended is not shown as out of office, whatever the calendar flag says.
    app.presences.known.insert(
        "u-ada".into(),
        Presence::new(Availability::Away, true).with_out_of_office_note(note("Away", Some(10))),
    );
    assert_eq!(app.banner_entries(soon).len(), 1);
    assert!(app.banner_entries(later).is_empty());
}

#[test]
fn the_banner_is_gone_when_the_chat_is_switched_and_back_when_it_is_reopened() {
    let (mut app, _) = plain();
    open(&mut app, "demo-ada");
    assert!(screen(&mut app).contains("Ada Lovelace: Working"));
    // Grace has nothing to say.
    open(&mut app, "demo-grace");
    let frame = screen(&mut app);
    assert_eq!(app.view.geometry.banner, Rect::default(), "{frame}");
    assert!(!frame.contains("Working on the"), "{frame}");
    open(&mut app, "demo-ada");
    assert!(screen(&mut app).contains("Ada Lovelace: Working"));
}

#[test]
fn you_are_not_in_it_and_with_presence_off_there_is_none() {
    let (mut app, backend) = plain();
    backend.say(
        ME_ID,
        Presence::new(Availability::Available, false)
            .with_note(note("My own note", None))
            .with_out_of_office_note(note("My own away note", None)),
    );
    backend.say(
        &person(1),
        Presence::new(Availability::Available, false).with_note(note("Hello from one", None)),
    );
    open(&mut app, CROWD);
    let frame = screen(&mut app);
    assert!(frame.contains("Person 1: Hello from one"), "{frame}");
    assert!(!frame.contains("My own"), "{frame}");
    assert!(
        app.banner_entries(Utc::now())
            .iter()
            .all(|e| e.name != "John Titor")
    );
    // The same answers, with presence switched off: nothing is asked and nothing is shown.
    let asked = backend.asked().len();
    app.set_presence_style(crate::presence::PresenceStyle::Off);
    app.presences.sched.set_last(ago(100));
    for _ in 0..10 {
        app.on_tick();
    }
    let frame = screen(&mut app);
    assert_eq!(backend.asked().len(), asked, "nothing asked");
    assert_eq!(app.view.geometry.banner, Rect::default(), "{frame}");
    assert!(!frame.contains("Hello from one"), "{frame}");
}

#[test]
fn a_big_group_chat_asks_about_forty_of_its_people_and_shows_only_those() {
    let (mut app, backend) = plain();
    for n in [3, 39, 40, 55] {
        backend.say(
            &person(n),
            Presence::new(Availability::Available, false)
                .with_note(note(&format!("note {n}"), None)),
        );
    }
    open(&mut app, CROWD);
    let asked = backend.asked();
    let last = asked.last().unwrap();
    let people: Vec<&String> = last.iter().filter(|id| id.starts_with("crowd-")).collect();
    assert_eq!(people.len(), 40, "{last:?}");
    assert!(
        (0..40).all(|n| last.contains(&person(n))),
        "the first forty, in the roster's order"
    );
    assert!(!last.contains(&person(40)));
    assert_eq!(
        last.iter().filter(|id| *id == ME_ID).count(),
        1,
        "you, once"
    );
    let names: Vec<String> = app
        .banner_entries(Utc::now())
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(
        names,
        ["Person 3", "Person 39"],
        "people past the forty were not asked about, so nothing of theirs is shown"
    );
}

#[test]
fn hostile_note_text_is_drawn_as_plain_text_without_a_control_character() {
    let (mut app, _) = plain();
    open(&mut app, "demo-ada");
    // A note as the library would never give it: the app draws nothing from the network raw.
    let mut nasty = StatusNote::new("x").unwrap();
    nasty.text = "\u{1b}]52;c;QUJD\u{7} \u{202E}evil\u{2066} \u{E001}\u{E002} [link](https://x.test) **bold** <b>tag</b>\r\n\t\u{9b}31m"
        .to_string();
    app.presences.known.insert(
        "u-ada".into(),
        Presence::new(Availability::Available, false).with_note(nasty),
    );
    let frame = screen(&mut app);
    let rows = banner_rows(&app, &frame);
    assert!(!rows.is_empty(), "{frame}");
    let row = rows.join("\n");
    assert!(row.contains("evil"), "{row:?}");
    // Markdown and markup are drawn as the characters they are: not a link, not bold, not a tag.
    assert!(
        row.contains("[link](https://x.test)") && row.contains("**bold**"),
        "{row:?}"
    );
    for c in frame.chars().filter(|c| *c != '\n') {
        assert!(
            !c.is_control()
                && !matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{E000}'..='\u{F8FF}'),
            "{c:?} reached the screen"
        );
    }
    // And through the whole path to the terminal: nothing in what is written is an escape that was not ratatui's own
    // (the ANSI renderer writes styles and nothing from the text).
    let ansi = crate::ui::render_to_ansi(&mut app, 120, 40);
    assert!(!ansi.contains("52;c;QUJD\u{7}") && !ansi.contains('\u{7}'));
}

#[test]
fn what_is_drawn_is_what_is_clicked_with_the_banner_in_place() {
    let (mut app, _) = plain();
    open(&mut app, "demo-sprint");
    app.focus = Focus::Messages;
    screen(&mut app);
    let g = app.view.geometry.clone();
    assert_eq!(g.banner.height, 2);
    // A click in the message box focuses it and a click on the transcript selects the message drawn at that row.
    click(&mut app, g.editor.x + 3, g.editor.y + 1);
    assert_eq!(app.focus, Focus::Editor);
    app.focus = Focus::Messages;
    let st = app.state().unwrap();
    let (starts, scroll) = (st.transcript.starts.clone(), st.scroll);
    let target = starts.len() - 1;
    let row = g.transcript.y + (starts[target] - scroll) as u16;
    click(&mut app, g.transcript.x + 2, row);
    assert_eq!(app.state().unwrap().sel, target);
    // A click on the banner is not a click on the message box or the messages.
    let (sel, focus) = (app.state().unwrap().sel, app.focus);
    click(&mut app, g.banner.x + 3, g.banner.y);
    assert_eq!((app.state().unwrap().sel, app.focus), (sel, focus));
    // Typing still lands in the box, which draws just below the banner.
    click(&mut app, g.editor.x + 3, g.editor.y + 1);
    type_text(&mut app, "hi");
    let frame = screen(&mut app);
    let box_row = frame
        .lines()
        .nth(usize::from(g.editor.y) + 1)
        .unwrap_or_default();
    assert!(box_row.contains("hi"), "{box_row:?}");
}

#[test]
fn the_banner_has_its_own_colours_red_for_out_of_office_and_grey_for_a_status_message() {
    let (mut app, _) = plain();
    open(&mut app, "demo-sprint");
    let terminal_backend = ratatui::backend::TestBackend::new(120, 40);
    let mut terminal = ratatui::Terminal::new(terminal_backend).expect("test terminal");
    terminal
        .draw(|f| crate::ui::render(f, &mut app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let b = app.view.geometry.banner;
    assert_eq!(b.height, 2);
    // Both rows here are out-of-office people; the status message is a count at the end of the second.
    for y in b.y..b.bottom() {
        let used: Vec<_> = (b.x..b.right())
            .map(|x| &buffer[(x, y)])
            .filter(|c| c.symbol() != " ")
            .collect();
        assert!(!used.is_empty());
        assert!(
            used.iter().all(|c| c.fg == OUT_OF_OFFICE),
            "an out-of-office row is red: {:?}",
            used.iter().map(|c| (c.symbol(), c.fg)).collect::<Vec<_>>()
        );
    }
    // A status message on its own is grey, with its words in italics.
    open(&mut app, "demo-ada");
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|f| crate::ui::render(f, &mut app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let b = app.view.geometry.banner;
    assert_eq!(b.height, 1);
    let cells: Vec<_> = (b.x..b.right())
        .map(|x| &buffer[(x, b.y)])
        .filter(|c| c.symbol() != " ")
        .collect();
    assert!(
        cells.iter().all(|c| c.fg == SUBTLE),
        "a status message is grey"
    );
    assert!(
        cells.last().unwrap().modifier.contains(Modifier::ITALIC),
        "its words are italic"
    );
}

#[test]
fn the_banner_takes_rows_from_the_transcript_but_not_its_width_so_wrapping_is_kept() {
    let (mut app, _) = plain();
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    screen(&mut app);
    let before = app.view.geometry.clone();
    assert_eq!(before.banner, Rect::default(), "nothing is known yet");
    open(&mut app, "demo-ada");
    screen(&mut app);
    let after = app.view.geometry.clone();
    assert_eq!(after.banner.height, 1);
    assert_eq!(after.transcript.width, before.transcript.width);
    assert_eq!(after.transcript.height + 1, before.transcript.height);
}
