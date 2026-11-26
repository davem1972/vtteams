//! The teams tab and the channels in it, driven through the keys a person presses, the mouse and the command panel.

use super::*;
use crate::view::{self, Tab};
use chrono::DateTime;
use ratatui::buffer::Buffer;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use teams_client::{AuthError, Channel, ChannelActivity, ChannelKind, Preview, Team};

const RELEASES: &str = "demo-chan-releases";
const GENERAL: &str = "demo-team-eng";

/// The demo's chats and teams, with what a test needs to know about what the app asked of it and a way to script what
/// it answers: the teams list (the demo's own when none is left), what the conversation list says about channels, and
/// whether it has teams at all.
struct Spy {
    inner: DemoBackend,
    can_teams: AtomicBool,
    can_sign_in_again: bool,
    answers: Mutex<VecDeque<Result<Vec<Team>, Error>>>,
    teams_calls: AtomicUsize,
    activity: Mutex<Option<Vec<ChannelActivity>>>,
    /// Chats `members` was asked about.
    members: Mutex<Vec<String>>,
    /// Everyone `presence` was asked about, over all asks.
    presence: Mutex<Vec<String>>,
    /// Chats `send` was called for.
    sent: Mutex<Vec<String>>,
    edits: AtomicUsize,
    favourited: AtomicUsize,
    /// (chat, message) of every reaction.
    reacted: Mutex<Vec<(String, String)>>,
    /// Reactions are refused (403).
    refuse_reactions: AtomicBool,
    /// An untitled group chat with nobody listed, which is what a chat asks the service for its people for.
    roster_probe: bool,
}

impl Spy {
    fn new() -> Self {
        Self {
            inner: DemoBackend::instant(),
            can_teams: AtomicBool::new(true),
            can_sign_in_again: false,
            answers: Mutex::new(VecDeque::new()),
            teams_calls: AtomicUsize::new(0),
            activity: Mutex::new(None),
            members: Mutex::new(Vec::new()),
            presence: Mutex::new(Vec::new()),
            sent: Mutex::new(Vec::new()),
            edits: AtomicUsize::new(0),
            favourited: AtomicUsize::new(0),
            reacted: Mutex::new(Vec::new()),
            refuse_reactions: AtomicBool::new(false),
            roster_probe: false,
        }
    }

    /// Over the demo that has no teams: the service itself does not have them.
    fn without_teams() -> Self {
        let spy = Self::new();
        spy.can_teams.store(false, Ordering::SeqCst);
        Self {
            inner: DemoBackend::without_teams(),
            ..spy
        }
    }

    fn answer(&self, answer: Result<Vec<Team>, Error>) {
        self.answers.lock().unwrap().push_back(answer);
    }

    fn teams_calls(&self) -> usize {
        self.teams_calls.load(Ordering::SeqCst)
    }

    fn sent(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }

    fn members_asked(&self) -> Vec<String> {
        self.members.lock().unwrap().clone()
    }
}

impl Backend for Spy {
    fn label(&self) -> String {
        self.inner.label()
    }
    fn me(&self) -> Result<Me, Error> {
        self.inner.me()
    }
    fn chats(&self) -> Result<Vec<Chat>, Error> {
        let mut chats = self.inner.chats()?;
        if self.roster_probe {
            chats.push(Chat {
                id: "spy-group".into(),
                kind: teams_client::ChatKind::Group,
                topic: Some("Roster probe".into()),
                members: vec![],
                updated: Utc::now() - chrono::Duration::days(300),
                preview: None,
                last_read: None,
                web_url: None,
                favorite: None,
            });
        }
        Ok(chats)
    }
    fn find_people(&self, q: &str) -> Result<Vec<Contact>, Error> {
        self.inner.find_people(q)
    }
    fn open_chat(&self, who: &Contact) -> Result<Chat, Error> {
        self.inner.open_chat(who)
    }
    fn messages(&self, id: &str, older: Option<&str>) -> Result<Page, Error> {
        // A chat or channel of a test's own making has nothing in it.
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
        self.sent.lock().unwrap().push(id.to_string());
        self.inner.send(id, html, m, reply_to)
    }
    fn can_edit(&self) -> bool {
        true
    }
    fn edit(
        &self,
        chat_id: &str,
        message_id: &str,
        html: &str,
        mentions: &[teams_client::OutMention],
    ) -> Result<(), Error> {
        self.edits.fetch_add(1, Ordering::SeqCst);
        self.inner.edit(chat_id, message_id, html, mentions)
    }
    fn can_favorite(&self) -> bool {
        true
    }
    fn set_favorite(&self, chat_id: &str, on: bool) -> Result<(), Error> {
        self.favourited.fetch_add(1, Ordering::SeqCst);
        self.inner.set_favorite(chat_id, on)
    }
    fn members(&self, chat_id: &str) -> Result<Vec<teams_client::Member>, Error> {
        self.members.lock().unwrap().push(chat_id.to_string());
        Ok(vec![teams_client::Member {
            not_in_directory: false,
            id: "u-ada".into(),
            name: "Ada Lovelace".into(),
        }])
    }
    fn can_presence(&self) -> bool {
        true
    }
    fn presence(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, teams_client::Presence>, Error> {
        self.presence.lock().unwrap().extend(ids.iter().cloned());
        self.inner.presence(ids)
    }
    fn marks_read(&self) -> bool {
        true
    }
    fn mark_read(
        &self,
        chat_id: &str,
        message_id: &str,
        arrived: DateTime<Utc>,
    ) -> Result<(), Error> {
        self.inner.mark_read(chat_id, message_id, arrived)
    }
    fn react(&self, chat_id: &str, message_id: &str, emoji: &str, on: bool) -> Result<(), Error> {
        self.reacted
            .lock()
            .unwrap()
            .push((chat_id.to_string(), message_id.to_string()));
        if self.refuse_reactions.load(Ordering::SeqCst) {
            return Err(Error::Status {
                operation: "react to message",
                status: 403,
                message: "not allowed here".into(),
            });
        }
        self.inner.react(chat_id, message_id, emoji, on)
    }
    fn reaction_emoji(&self) -> Result<Vec<EmojiInfo>, Error> {
        self.inner.reaction_emoji()
    }
    fn can_search_messages(&self) -> bool {
        true
    }
    fn search_messages(
        &self,
        chat_id: &str,
        query: &str,
        from: usize,
    ) -> Result<teams_client::SearchPage, Error> {
        self.inner.search_messages(chat_id, query, from)
    }
    fn can_sign_in_again(&self) -> bool {
        self.can_sign_in_again
    }
    fn can_list_teams(&self) -> bool {
        self.can_teams.load(Ordering::SeqCst)
    }
    fn teams(&self) -> Result<Vec<Team>, Error> {
        self.teams_calls.fetch_add(1, Ordering::SeqCst);
        match self.answers.lock().unwrap().pop_front() {
            Some(answer) => answer,
            None => self.inner.teams(),
        }
    }
    fn channel_activity(&self) -> Vec<ChannelActivity> {
        match &*self.activity.lock().unwrap() {
            Some(activity) => activity.clone(),
            None => self.inner.channel_activity(),
        }
    }
    fn serves_image(&self, url: &str) -> bool {
        self.inner.serves_image(url)
    }
    fn image(&self, url: &str) -> Result<RgbImage, Error> {
        self.inner.image(url)
    }
}

fn started(spy: &Arc<Spy>, keymap: Keymap) -> App {
    let (mut app, _) = App::for_test(spy.clone());
    app.set_keymap(keymap);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app
}

fn teams_app(keymap: Keymap) -> (App, Arc<Spy>) {
    let spy = Arc::new(Spy::new());
    let app = started(&spy, keymap);
    (app, spy)
}

fn alt(app: &mut App, c: char) {
    press(app, KeyCode::Char(c), KeyModifiers::ALT);
}

fn enter(app: &mut App) {
    press(app, KeyCode::Enter, KeyModifiers::NONE);
}

/// Show the teams tab the way a person of `app`'s keymap would, and wait for the list.
fn show_teams(app: &mut App) {
    if app.keymap == Keymap::Standard {
        alt(app, '2');
    } else {
        type_text(app, "gt");
    }
    assert_eq!(app.tab, Tab::Teams);
    pump(app, |a| a.teams.loaded());
}

/// Run `cmd` through the command panel the way a person would (`:` in vim, Ctrl-P in the standard keymap).
fn run(app: &mut App, cmd: &str) {
    if app.keymap == Keymap::Standard {
        ctrl(app, 'p');
    } else {
        app.on_key(key(':'));
    }
    type_text(app, cmd);
    enter(app);
}

fn down(app: &mut App) {
    if app.keymap == Keymap::Standard {
        press(app, KeyCode::Down, KeyModifiers::NONE);
    } else {
        app.on_key(key('j'));
    }
}

fn open_key(app: &mut App) {
    if app.keymap == Keymap::Standard {
        press(app, KeyCode::Right, KeyModifiers::NONE);
    } else {
        app.on_key(key('l'));
    }
}

fn back_key(app: &mut App) {
    if app.keymap == Keymap::Standard {
        press(app, KeyCode::Left, KeyModifiers::NONE);
    } else {
        app.on_key(key('h'));
    }
}

/// The rows of the teams list as text: `* Team › Channel` for a favourite, `v`/`>` for an open or closed team, two
/// spaces for a channel, `+N more` or `-N more` for the row that shows the rest.
fn names(app: &App) -> Vec<String> {
    let items = app.teams.items();
    app.team_rows()
        .iter()
        .map(|r| match *r {
            TeamRow::Favorite { team, channel } => {
                format!(
                    "* {}",
                    favorite_label(&items[team], &items[team].channels[channel])
                )
            }
            TeamRow::Team { team, open } => {
                format!("{} {}", if open { "v" } else { ">" }, items[team].name)
            }
            TeamRow::Channel { team, channel } => {
                format!("  {}", items[team].channels[channel].name)
            }
            TeamRow::More { hidden, open, .. } => {
                format!("  {}{hidden} more", if open { "-" } else { "+" })
            }
        })
        .collect()
}

fn cursor_name(app: &App) -> String {
    let rows = app.team_rows();
    names(app)[app.team_cursor_index(&rows)].trim().to_string()
}

/// Move the cursor down to the row that reads `name` (as `names` writes it), from the top.
fn move_to(app: &mut App, name: &str) {
    if app.keymap == Keymap::Standard {
        press(app, KeyCode::Home, KeyModifiers::NONE);
    } else {
        type_text(app, "gg");
    }
    for _ in 0..200 {
        if cursor_name(app) == name.trim() {
            return;
        }
        down(app);
    }
    panic!("no row {name:?} in {:?}", names(app));
}

/// Open `channel` of `team` with the keys: the team's row, Right or `l` to open it if it is not open, down to the
/// channel, Enter.
fn open_channel_by_keys(app: &mut App, team: &str, channel: &str) {
    if app.tab != Tab::Teams {
        show_teams(app);
    }
    app.focus = Focus::Chats;
    let closed = format!("> {team}");
    let open = format!("v {team}");
    if names(app).contains(&closed) {
        move_to(app, &closed);
        open_key(app);
    }
    assert!(names(app).contains(&open), "{:?}", names(app));
    move_to(app, channel);
    enter(app);
    pump(app, |a| a.state().is_some_and(|s| s.loaded));
}

fn frame(app: &mut App) -> String {
    crate::ui::render_to_string(app, 120, 40)
}

fn buffer(app: &mut App, width: u16, height: u16) -> Buffer {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal.draw(|f| crate::ui::render(f, app)).expect("draw");
    terminal.backend().buffer().clone()
}

fn text_row(text: &str, y: u16) -> String {
    text.lines().nth(usize::from(y)).unwrap_or("").to_string()
}

fn ago_min(minutes: u64) -> Instant {
    ago(minutes * 60)
}

// --- the tabs ---------------------------------------------------------------------------------------------------

#[test]
fn a_service_with_no_teams_has_no_tab_and_none_of_the_tab_keys_do_anything() {
    for keymap in [Keymap::Standard, Keymap::Vim] {
        let spy = Arc::new(Spy::without_teams());
        let mut app = started(&spy, keymap);
        let before = (app.current.clone(), app.focus);
        let head = frame(&mut app);
        let head_row = text_row(&head, app.view.geometry.list_head.y);
        assert!(
            head_row.contains("CHATS") && !head_row.contains("TEAMS"),
            "{keymap:?}: {head_row:?}"
        );
        assert!(app.tab_labels(40).is_empty());
        // The keys of the tab: Alt-1/Alt-2, `gt`/`gT`, the commands and a click where a label would be.
        alt(&mut app, '2');
        alt(&mut app, '1');
        app.focus = Focus::Chats;
        if keymap == Keymap::Vim {
            type_text(&mut app, "gtgT");
        }
        run(&mut app, "teams");
        assert!(
            app.status
                .as_ref()
                .is_some_and(|(m, _, err)| *err && m.contains("no teams")),
            "{keymap:?}: the command says so"
        );
        run(&mut app, "chats");
        let head = app.view.geometry.list_head;
        click(&mut app, head.x + 12, head.y);
        assert_eq!(app.tab, Tab::Chats, "{keymap:?}");
        assert_eq!(spy.teams_calls(), 0, "{keymap:?}: nothing asked for teams");
        assert_eq!(app.current, before.0, "{keymap:?}");
        assert!(!app.teams.loaded() && !app.teams_available());
    }
}

#[test]
fn a_service_with_teams_shows_both_tabs_and_marks_the_one_that_is_showing() {
    let (mut app, _) = teams_app(Keymap::Standard);
    let text = frame(&mut app);
    let head = app.view.geometry.list_head;
    let row = text_row(&text, head.y);
    // Chats are counted, and so is what is unread; the teams are not known until the tab has been shown.
    let chats = app.visible_chats().len();
    let unread = app
        .chats
        .items()
        .iter()
        .filter(|c| app.is_unread(c))
        .count();
    assert!(unread > 0, "the demo has an unread chat");
    assert!(
        row.contains(&format!("CHATS {chats} ●{unread}")) && row.contains("TEAMS"),
        "{row:?}"
    );
    assert!(
        !row.contains("TEAMS 2"),
        "no count before the list has come: {row:?}"
    );
    let cell_bg = |app: &mut App, tab: Tab| {
        let labels = app.tab_labels(usize::from(head.width));
        let widths: Vec<usize> = labels.iter().map(|l| l.width()).collect();
        let cells = view::tab_cells(&widths);
        let i = labels.iter().position(|l| l.tab == tab).unwrap();
        let x = head.x + cells[i].start as u16 + 1;
        buffer(app, 120, 40)[(x, head.y)].bg
    };
    assert_ne!(
        cell_bg(&mut app, Tab::Chats),
        crate::theme::BG,
        "the showing tab is highlighted"
    );
    assert_eq!(cell_bg(&mut app, Tab::Teams), crate::theme::BG);
    alt(&mut app, '2');
    pump(&mut app, |a| a.teams.loaded());
    assert_ne!(cell_bg(&mut app, Tab::Teams), crate::theme::BG);
    assert_eq!(cell_bg(&mut app, Tab::Chats), crate::theme::BG);
    // Now the teams are counted too, with what is unread in them (Incidents by its list, Releases by the activity).
    let row = text_row(&frame(&mut app), head.y);
    assert!(row.contains("TEAMS 2 ●2"), "{row:?}");
}

#[test]
fn the_tab_is_switched_by_key_by_click_and_by_command_in_both_keymaps() {
    for keymap in [Keymap::Standard, Keymap::Vim] {
        let (mut app, spy) = teams_app(keymap);
        frame(&mut app);
        // By key.
        if keymap == Keymap::Standard {
            alt(&mut app, '2');
        } else {
            app.focus = Focus::Chats;
            type_text(&mut app, "gt");
        }
        assert_eq!(app.tab, Tab::Teams, "{keymap:?}: key");
        assert_eq!(app.focus, Focus::Chats);
        if keymap == Keymap::Standard {
            alt(&mut app, '1');
        } else {
            type_text(&mut app, "gT");
        }
        assert_eq!(app.tab, Tab::Chats, "{keymap:?}: key back");
        // By click, on the label drawn.
        let head = app.view.geometry.list_head;
        let labels = app.tab_labels(usize::from(head.width));
        let widths: Vec<usize> = labels.iter().map(|l| l.width()).collect();
        let cells = view::tab_cells(&widths);
        click(&mut app, head.x + cells[1].start as u16 + 2, head.y);
        assert_eq!(app.tab, Tab::Teams, "{keymap:?}: click");
        frame(&mut app);
        let head = app.view.geometry.list_head;
        click(&mut app, head.x + cells[0].start as u16 + 2, head.y);
        assert_eq!(app.tab, Tab::Chats, "{keymap:?}: click back");
        // The padding between the labels is no label.
        click(&mut app, head.x + cells[0].end as u16, head.y);
        assert_eq!(app.tab, Tab::Chats);
        // By command.
        run(&mut app, "teams");
        assert_eq!(app.tab, Tab::Teams, "{keymap:?}: command");
        run(&mut app, "chats");
        assert_eq!(app.tab, Tab::Chats, "{keymap:?}: command back");
        assert!(spy.teams_calls() >= 1);
    }
}

#[test]
fn switching_tab_from_the_message_box_keeps_the_draft_and_leaves_vim_in_normal_mode() {
    let (mut app, _) = teams_app(Keymap::Standard);
    app.focus = Focus::Editor;
    type_text(&mut app, "half a thought");
    alt(&mut app, '2');
    assert_eq!((app.tab, app.focus), (Tab::Teams, Focus::Chats));
    assert_eq!(
        app.editor.text(),
        "half a thought",
        "typed text is not lost or changed"
    );
    alt(&mut app, '1');
    assert_eq!(app.editor.text(), "half a thought");

    // Vim: from insert mode, Esc and then the editor's own `:` line, as a person would.
    let (mut app, _) = teams_app(Keymap::Vim);
    app.focus = Focus::Editor;
    app.editor.set_mode(Mode::Insert);
    type_text(&mut app, "abc");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    app.on_key(key(':'));
    type_text(&mut app, "teams");
    enter(&mut app);
    assert_eq!((app.tab, app.focus), (Tab::Teams, Focus::Chats));
    assert_eq!(app.editor.mode(), Mode::Normal);
    assert_eq!(app.editor.text(), "abc");
}

#[test]
fn the_chats_tab_is_drawn_as_it_was_whether_or_not_there_are_tabs() {
    let spy = Arc::new(Spy::new());
    let mut with = started(&spy, Keymap::Standard);
    spy.can_teams.store(false, Ordering::SeqCst);
    let mut without = started(&spy, Keymap::Standard);
    // Both have the same chat open and read, so that what differs can only be the tabs.
    for app in [&mut with, &mut without] {
        pump(app, |a| a.state().is_some_and(|s| s.loaded));
        app.on_tick();
        app.on_tick();
    }
    // One service, which is made to have teams or not just before each of the two frames.
    spy.can_teams.store(true, Ordering::SeqCst);
    let with_frame = frame(&mut with);
    spy.can_teams.store(false, Ordering::SeqCst);
    let without_frame = frame(&mut without);
    spy.can_teams.store(true, Ordering::SeqCst);
    let list = with.view.geometry.list;
    let head_y = with.view.geometry.list_head.y;
    for y in 0..40u16 {
        if y == head_y {
            continue;
        }
        let cut = |s: &str| {
            text_row(s, y)
                .chars()
                .take(usize::from(list.width))
                .collect::<String>()
        };
        assert_eq!(
            cut(&with_frame),
            cut(&without_frame),
            "row {y} of the chat list"
        );
    }
    // And after a visit to the teams, the same list is back.
    alt(&mut with, '2');
    pump(&mut with, |a| a.teams.loaded());
    alt(&mut with, '1');
    let back = frame(&mut with);
    for y in 0..40u16 {
        if y == head_y {
            continue;
        }
        let cut = |s: &str| {
            text_row(s, y)
                .chars()
                .take(usize::from(list.width))
                .collect::<String>()
        };
        assert_eq!(cut(&back), cut(&without_frame), "row {y} after a visit");
    }
}

// --- the rows -------------------------------------------------------------------------------------------------

#[test]
fn teams_start_closed_and_a_team_lists_its_channels_in_order_with_a_row_for_the_rest() {
    for keymap in [Keymap::Standard, Keymap::Vim] {
        let (mut app, _) = teams_app(keymap);
        show_teams(&mut app);
        assert_eq!(
            names(&app),
            [
                "* Engineering › Incidents",
                "* Marketing › General",
                "> Engineering",
                "> Marketing"
            ],
            "{keymap:?}: the favourites, then the teams all closed (what is open is a chat); the archived team is not there"
        );
        assert_eq!(cursor_name(&app), "* Engineering › Incidents");
        move_to(&mut app, "> Engineering");
        open_key(&mut app);
        assert_eq!(
            names(&app)[2..],
            [
                "v Engineering",
                "  General",
                "  Releases",
                "  Incidents",
                "  Design review",
                "  Partners",
                "  Old experiments",
                "  +2 more",
                "> Marketing",
            ],
            "{keymap:?}: General, the pinned one, then the shown ones, then the row for the rest"
        );
        let text = frame(&mut app);
        let list = text
            .lines()
            .map(|l| l.chars().take(36).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            list.contains("▾ Engineering") && !list.contains('★'),
            "{keymap:?}: open, and a team is no longer marked as a favourite\n{list}"
        );
        assert!(
            list.contains("Marketing › General"),
            "{keymap:?}: the favourites section\n{list}"
        );
        assert!(
            !list.contains("Legacy"),
            "{keymap:?}: an archived team is not listed\n{list}"
        );
        assert!(
            list.contains("▸ Marketing muted"),
            "{keymap:?}: closed, muted\n{list}"
        );
        assert!(
            list.contains("Design review private"),
            "{keymap:?}: private\n{list}"
        );
        assert!(
            list.contains("Partners shared"),
            "{keymap:?}: shared\n{list}"
        );
        assert!(list.contains("+2 more channels"), "{keymap:?}\n{list}");
        let incidents: Vec<&str> = list.lines().filter(|l| l.contains("Incidents")).collect();
        assert_eq!(
            incidents.len(),
            2,
            "{keymap:?}: under its team and among the favourites"
        );
        assert!(
            incidents.iter().all(|l| l.contains('●')),
            "{keymap:?}: an unread bullet on each\n{list}"
        );
        let general: Vec<&str> = list.lines().filter(|l| l.contains("General")).collect();
        assert!(
            general.len() == 2 && general.iter().all(|l| !l.contains('●')),
            "{keymap:?}: and none on a read one\n{list}"
        );
    }
}

#[test]
fn archived_and_muted_rows_are_dimmed_and_the_rest_are_not() {
    let (mut app, _) = teams_app(Keymap::Standard);
    show_teams(&mut app);
    move_to(&mut app, "> Engineering");
    open_key(&mut app);
    move_to(&mut app, "> Marketing");
    open_key(&mut app);
    // Cursor off the rows being looked at: the highlight would hide their colour.
    press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    let text = frame(&mut app);
    let buf = buffer(&mut app, 120, 40);
    let fg_of = |needle: &str| {
        let (y, line) = text
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle} is drawn:\n{text}"));
        let col = line[..line.find(needle).unwrap()].chars().count();
        buf[(col as u16, y as u16)].fg
    };
    assert_eq!(
        fg_of("Old experiments"),
        crate::theme::DIM,
        "an archived channel"
    );
    assert_eq!(fg_of("Marketing muted"), crate::theme::DIM, "a muted team");
    assert!(
        !text.contains("Legacy Platform"),
        "an archived team is not drawn at all:\n{text}"
    );
    assert_eq!(
        fg_of("Design review"),
        crate::theme::TEXT,
        "an ordinary channel"
    );
}

#[test]
fn folding_a_team_and_showing_the_rest_work_with_the_keys_of_both_keymaps() {
    for keymap in [Keymap::Standard, Keymap::Vim] {
        let (mut app, _) = teams_app(keymap);
        show_teams(&mut app);
        move_to(&mut app, "> Engineering");
        open_key(&mut app);
        assert_eq!(names(&app).len(), 11, "{keymap:?}");
        // Back closes an open team; going back again does nothing.
        back_key(&mut app);
        assert_eq!(names(&app)[2], "> Engineering", "{keymap:?}");
        back_key(&mut app);
        assert_eq!(names(&app).len(), 4);
        // Forward on an open team steps into its first channel (General); back from there goes to the team.
        open_key(&mut app);
        open_key(&mut app);
        assert_eq!(cursor_name(&app), "General", "{keymap:?}");
        back_key(&mut app);
        assert_eq!(cursor_name(&app), "v Engineering", "{keymap:?}");
        // Enter closes an open team and opens a closed one.
        enter(&mut app);
        assert_eq!(names(&app).len(), 4, "{keymap:?}");
        enter(&mut app);
        assert_eq!(names(&app).len(), 11, "{keymap:?}");
        // The row for the rest lists them, and again hides them.
        move_to(&mut app, "+2 more");
        open_key(&mut app);
        assert_eq!(
            names(&app)[9..12].to_vec(),
            ["  -2 more", "  Random", "  Hackathon 2023"],
            "{keymap:?}: the rest follow it in the service's order"
        );
        assert_eq!(cursor_name(&app), "-2 more", "the cursor stays on the row");
        let text = frame(&mut app);
        assert!(text.contains("−2 more channels"));
        back_key(&mut app);
        assert_eq!(
            cursor_name(&app),
            "v Engineering",
            "back from the row goes to its team"
        );
        move_to(&mut app, "-2 more");
        enter(&mut app);
        assert_eq!(names(&app).len(), 11, "{keymap:?}");
        // The fold state is kept while another tab shows.
        alt_or_gt_back_and_forth(&mut app);
        assert_eq!(names(&app).len(), 11);
    }
}

fn alt_or_gt_back_and_forth(app: &mut App) {
    app.show_tab(Tab::Chats);
    app.show_tab(Tab::Teams);
}

#[test]
fn a_filter_lists_the_teams_and_channels_that_match_and_opens_their_team() {
    // The standard keymap: just typing.
    let (mut app, _) = teams_app(Keymap::Standard);
    show_teams(&mut app);
    type_text(&mut app, "rand");
    assert!(app.filtering && app.filter == "rand");
    assert_eq!(
        names(&app),
        ["v Engineering", "  Random"],
        "a hidden channel is found, and its team opens while filtering"
    );
    assert_eq!(app.editor.text(), "", "the letters did not start a message");
    let text = frame(&mut app);
    let head = text_row(&text, app.view.geometry.list_head.y);
    assert!(head.contains("/rand"), "{head:?}");
    // Backspace widens it again, Esc clears it, and the fold state the person had is back.
    for _ in 0..3 {
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    }
    assert_eq!(app.filter, "r");
    assert!(names(&app).len() > 2, "{:?}", names(&app));
    assert!(names(&app).contains(&"  Releases".to_string()));
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert!(app.filter.is_empty() && !app.filtering);
    assert_eq!(
        names(&app),
        [
            "* Engineering › Incidents",
            "* Marketing › General",
            "> Engineering",
            "> Marketing"
        ]
    );

    // The vim keymap: `/`, the text, Enter to leave the box with the result kept, Enter to open.
    let (mut app, _) = teams_app(Keymap::Vim);
    show_teams(&mut app);
    app.on_key(key('/'));
    type_text(&mut app, "campaigns");
    assert_eq!(names(&app), ["v Marketing", "  Campaigns"]);
    enter(&mut app);
    assert!(!app.filtering && app.filter == "campaigns");
    assert_eq!(
        cursor_name(&app),
        "v Marketing",
        "the cursor goes to the first match"
    );
    down(&mut app);
    enter(&mut app);
    assert_eq!(app.current.as_deref(), Some("demo-chan-campaigns"));
    // A team matched by its own name keeps its fold state.
    app.focus = Focus::Chats;
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    app.on_key(key('/'));
    type_text(&mut app, "engineering");
    assert_eq!(
        names(&app),
        ["* Engineering › Incidents", "> Engineering"],
        "a favourite matches by its team's name too and opens nothing"
    );
    // An archived team is not listed, so it is not found either.
    app.filter.clear();
    type_text(&mut app, "legacy");
    assert!(names(&app).is_empty());
    app.filter.clear();
    type_text(&mut app, "engineering");
    // Nothing matches: the list says so.
    type_text(&mut app, "zzz");
    assert!(names(&app).is_empty());
    assert!(frame(&mut app).contains("no teams or channels match"));
}

// --- scrolling and clicking --------------------------------------------------------------------------------

/// `n` teams of two channels each (General, whose id is the team's, and `Channel NN`) and, as favourites in this order,
/// the `Channel NN` of each team numbered in `favourites`.
fn long_list_with_favourites(spy: &Spy, n: usize, favourites: &[usize]) {
    let place = |id: &str| {
        favourites
            .iter()
            .position(|i| format!("c{i:02}") == id)
            .map(|p| u32::try_from(p + 1).unwrap())
    };
    let teams = (0..n)
        .map(|i| Team {
            id: format!("t{i:02}"),
            name: format!("Team {i:02}"),
            archived: false,
            muted: false,
            members: None,
            channels: vec![
                Channel {
                    id: format!("t{i:02}"),
                    name: "General".into(),
                    general: true,
                    kind: ChannelKind::Standard,
                    archived: false,
                    shown: true,
                    pinned: false,
                    muted: false,
                    followed: true,
                    favorite: None,
                    unread: false,
                    last: None,
                },
                Channel {
                    id: format!("c{i:02}"),
                    name: format!("Channel {i:02}"),
                    general: false,
                    kind: ChannelKind::Standard,
                    archived: false,
                    shown: true,
                    pinned: false,
                    muted: false,
                    followed: true,
                    favorite: place(&format!("c{i:02}")),
                    unread: false,
                    last: None,
                },
            ],
        })
        .collect();
    spy.answer(Ok(teams));
}

/// A tall list on a short screen: rows 0..cap are drawn, from `view.team_offset`.
fn long_app(keymap: Keymap) -> (App, Arc<Spy>) {
    long_app_with_favourites(keymap, &[])
}

/// The same with favourite channels (see `long_list_with_favourites`): the section and its divider on top of the tree.
fn long_app_with_favourites(keymap: Keymap, favourites: &[usize]) -> (App, Arc<Spy>) {
    let spy = Arc::new(Spy::new());
    long_list_with_favourites(&spy, 40, favourites);
    let mut app = started(&spy, keymap);
    show_teams(&mut app);
    crate::ui::render_to_string(&mut app, 100, 14);
    (app, spy)
}

#[test]
fn the_cursor_stays_on_screen_and_what_is_drawn_is_what_scrolling_and_clicking_mean() {
    // Without favourites, and with three (so a section, and a divider below it, are part of what scrolls).
    for (keymap, favourites) in [
        (Keymap::Standard, vec![]),
        (Keymap::Vim, vec![]),
        (Keymap::Standard, vec![30, 5, 20]),
        (Keymap::Vim, vec![30, 5, 20]),
    ] {
        let (mut app, _) = long_app_with_favourites(keymap, &favourites);
        let height = usize::from(app.view.geometry.list_body.height);
        assert!(
            (5..40).contains(&height),
            "a list longer than the screen: {height}"
        );
        for step in 0..48 {
            let text = crate::ui::render_to_string(&mut app, 100, 14);
            let rows = app.team_rows();
            assert_eq!(rows.len(), 40 + favourites.len());
            let (at, offset) = (app.team_cursor_index(&rows), app.view.team_offset);
            let pinned = favorite_count(&rows);
            assert_eq!(pinned, favourites.len());
            let cap = view::teams_capacity(height, offset, pinned);
            assert_eq!(at, step.min(rows.len() - 1), "{keymap:?}");
            assert!(
                offset <= at && at < offset + cap,
                "{keymap:?} {favourites:?}: cursor {at} outside {offset}+{cap}"
            );
            // Every screen row shows what the model says is there (`view::teams_row`): a row, or the rule that is
            // drawn between the favourites and the teams only when both are on the screen.
            let body = app.view.geometry.list_body;
            let labels = names(&app);
            for r in 0..height {
                let shown: String = text_row(&text, body.y + r as u16)
                    .chars()
                    .take(usize::from(body.width))
                    .collect();
                match view::teams_row(offset, pinned, r) {
                    view::TeamsLine::Row(pos) => {
                        let Some(label) = labels.get(pos).filter(|_| pos < offset + cap) else {
                            continue;
                        };
                        let want = label.trim_start_matches(['v', '>', '*', ' ']).trim();
                        assert!(
                            shown.contains(want),
                            "{keymap:?} {favourites:?}: screen row {r} should show {want:?}, shows {shown:?}"
                        );
                    }
                    view::TeamsLine::Divider => {
                        if pinned < offset + cap {
                            assert!(
                                shown.starts_with("──────"),
                                "{keymap:?} {favourites:?}: screen row {r} is the rule, shows {shown:?}"
                            );
                        }
                    }
                }
            }
            down(&mut app);
        }
        // The end of the list: the last row is the last one drawn.
        if keymap == Keymap::Standard {
            press(&mut app, KeyCode::End, KeyModifiers::NONE);
        } else {
            app.on_key(key('G'));
        }
        crate::ui::render_to_string(&mut app, 100, 14);
        let rows = app.team_rows();
        assert_eq!(app.team_cursor_index(&rows), rows.len() - 1);
        let (offset, pinned) = (app.view.team_offset, favorite_count(&rows));
        assert_eq!(
            offset + view::teams_capacity(height, offset, pinned),
            rows.len(),
            "{keymap:?} {favourites:?}: ends at the bottom"
        );
    }
}

#[test]
fn a_click_on_a_row_goes_to_the_row_that_was_drawn_there_at_every_scroll_position() {
    for favourites in [vec![], vec![30, 5, 20]] {
        let (mut app, _) = long_app_with_favourites(Keymap::Standard, &favourites);
        for jump in [0usize, 2, 3, 7, 25, 39] {
            // Scroll by keys to where the cursor is `jump` rows down.
            app.tab = Tab::Teams;
            app.focus = Focus::Chats;
            press(&mut app, KeyCode::Home, KeyModifiers::NONE);
            for _ in 0..jump {
                down(&mut app);
            }
            crate::ui::render_to_string(&mut app, 100, 14);
            let height = usize::from(app.view.geometry.list_body.height);
            for r in 0..height {
                crate::ui::render_to_string(&mut app, 100, 14);
                app.focus = Focus::Chats;
                app.tab = Tab::Teams;
                let rows = app.team_rows();
                let (offset, body) = (app.view.team_offset, app.view.geometry.list_body);
                let pinned = favorite_count(&rows);
                let before = app.team_cursor.clone();
                let note = format!("{favourites:?} jump {jump}, screen row {r}, offset {offset}");
                match view::teams_row(offset, pinned, r) {
                    view::TeamsLine::Divider => {
                        // The rule is not a row: nothing happens.
                        let before_names = names(&app);
                        click(&mut app, body.x + 3, body.y + r as u16);
                        assert_eq!(app.team_cursor, before, "{note}");
                        assert_eq!(names(&app), before_names, "{note}");
                        assert_eq!(app.focus, Focus::Chats, "{note}");
                    }
                    view::TeamsLine::Row(pos) => {
                        let Some(&row) = rows.get(pos) else {
                            continue;
                        };
                        let want = app.row_key(&row);
                        click(&mut app, body.x + 3, body.y + r as u16);
                        // `click` redraws; the cursor is on the row that was drawn where the click was.
                        assert_eq!(app.team_cursor, Some(want), "{note}");
                        // Clicking a channel, or a favourite, opens it and moves to its messages; a team toggles.
                        if let TeamRow::Channel { team, channel }
                        | TeamRow::Favorite { team, channel } = row
                        {
                            assert_eq!(app.focus, Focus::Messages, "{note}");
                            assert_eq!(
                                app.current.as_deref(),
                                Some(app.teams.items()[team].channels[channel].id.as_str()),
                                "{note}"
                            );
                        }
                    }
                }
            }
        }
    }
    // Below the last row there is nothing to click.
    let (mut app, _) = teams_app(Keymap::Standard);
    show_teams(&mut app);
    crate::ui::render_to_string(&mut app, 120, 40);
    let before = (app.team_cursor.clone(), names(&app));
    let body = app.view.geometry.list_body;
    click(&mut app, body.x + 3, body.y + 20);
    assert_eq!((app.team_cursor.clone(), names(&app)), before);
}

#[test]
fn the_vim_keys_move_through_a_long_list_by_rows_and_by_half_pages() {
    let (mut app, _) = long_app(Keymap::Vim);
    let at = |app: &App| app.team_cursor_index(&app.team_rows());
    app.on_key(key('G'));
    assert_eq!(at(&app), 39);
    type_text(&mut app, "gg");
    assert_eq!(at(&app), 0);
    app.on_key(key('j'));
    app.on_key(key('j'));
    app.on_key(key('k'));
    assert_eq!(at(&app), 1);
    let half = (app.view.list_height() / 2).max(1);
    ctrl(&mut app, 'd');
    assert_eq!(at(&app), 1 + half);
    ctrl(&mut app, 'u');
    assert_eq!(at(&app), 1);
    // The wheel (or a trackpad) scrolls the list and leaves the cursor where it is, even off screen.
    crate::ui::render_to_string(&mut app, 120, 40);
    let list = app.view.geometry.list_body;
    let top = app.view.team_offset;
    for _ in 0..5 {
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: list.x + 2,
            row: list.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        crate::ui::render_to_string(&mut app, 120, 40);
    }
    assert_eq!(at(&app), 1, "the cursor did not move");
    assert!(
        app.view.team_offset > top + 1,
        "the list did: {top} -> {}",
        app.view.team_offset
    );
    // Moving the cursor brings it back into view.
    app.on_key(key('j'));
    crate::ui::render_to_string(&mut app, 120, 40);
    assert_eq!(at(&app), 2);
    assert!(app.view.team_offset <= 2, "{}", app.view.team_offset);
}

#[test]
fn standard_page_keys_move_by_a_screen_of_rows() {
    let (mut app, _) = long_app(Keymap::Standard);
    let at = |app: &App| app.team_cursor_index(&app.team_rows());
    let page = app.view.list_height();
    press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
    assert_eq!(
        at(&app),
        page,
        "one row each, so a page is the height of the list"
    );
    press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!(at(&app), 0);
    press(&mut app, KeyCode::End, KeyModifiers::NONE);
    assert_eq!(at(&app), 39);
}

// --- favourites ----------------------------------------------------------------------------------------------

fn chan(id: &str, name: &str, general: bool, favorite: Option<u32>) -> Channel {
    Channel {
        id: id.into(),
        name: name.into(),
        general,
        kind: ChannelKind::Standard,
        archived: false,
        shown: true,
        pinned: false,
        muted: false,
        followed: true,
        favorite,
        unread: false,
        last: None,
    }
}

fn team_of(id: &str, name: &str, archived: bool, channels: Vec<Channel>) -> Team {
    Team {
        id: id.into(),
        name: name.into(),
        archived,
        muted: false,
        members: Some(3),
        channels,
    }
}

/// Teams in the service's order Alpha, Beta, Old, whose favourites are, in the folder's order, Alpha's Plans, Beta's Ops,
/// Alpha's General (the team's root conversation) and then two channels of the archived team Old. Plans is unread.
fn favourite_teams() -> Vec<Team> {
    let mut plans = chan("p", "Plans", false, Some(1));
    plans.unread = true;
    let mut lore = chan("l", "Lore", false, Some(5));
    lore.unread = true;
    vec![
        team_of(
            "a",
            "Alpha",
            false,
            vec![chan("a", "General", true, Some(3)), plans],
        ),
        team_of(
            "b",
            "Beta",
            false,
            vec![
                chan("b", "General", true, None),
                chan("o", "Ops", false, Some(2)),
            ],
        ),
        team_of(
            "z",
            "Old",
            true,
            vec![chan("z", "General", true, Some(4)), lore],
        ),
    ]
}

/// An app on the teams tab with `teams` as the list.
fn app_with_teams(keymap: Keymap, teams: Vec<Team>) -> (App, Arc<Spy>) {
    let spy = Arc::new(Spy::new());
    spy.answer(Ok(teams));
    let mut app = started(&spy, keymap);
    show_teams(&mut app);
    (app, spy)
}

/// The rows of the list body as drawn, cut to the list's width.
fn list_lines(app: &mut App, height: u16) -> Vec<String> {
    let text = crate::ui::render_to_string(app, 120, height);
    let body = app.view.geometry.list_body;
    (0..body.height)
        .map(|r| {
            text_row(&text, body.y + r)
                .chars()
                .skip(usize::from(body.x))
                .take(usize::from(body.width))
                .collect()
        })
        .collect()
}

fn is_rule(line: &str) -> bool {
    line.trim_end().chars().count() > 5 && line.trim_end().chars().all(|c| c == '─')
}

#[test]
fn the_favourites_are_the_first_rows_as_team_arrow_channel_in_the_folders_order_above_a_divider() {
    for keymap in [Keymap::Standard, Keymap::Vim] {
        let (mut app, _) = app_with_teams(keymap, favourite_teams());
        assert_eq!(
            names(&app),
            [
                "* Alpha › Plans",
                "* Beta › Ops",
                "* Alpha › General",
                "> Alpha",
                "> Beta"
            ],
            "{keymap:?}: the folder's order (not the service's), the team's root as its General, nothing of Old"
        );
        let lines = list_lines(&mut app, 30);
        assert!(
            lines[0].contains("Alpha › Plans") && lines[0].contains('●'),
            "{lines:?}"
        );
        assert!(
            lines[1].contains("Beta › Ops") && !lines[1].contains('●'),
            "{lines:?}"
        );
        assert!(lines[2].contains("Alpha › General"), "{lines:?}");
        assert!(is_rule(&lines[3]), "{keymap:?}: the divider: {lines:?}");
        assert!(lines[4].contains("▸ Alpha"), "{lines:?}");
        assert!(lines[5].contains("▸ Beta"), "{lines:?}");
        assert_eq!(
            lines.iter().filter(|l| is_rule(l)).count(),
            1,
            "{keymap:?}: one divider"
        );
        // The same thin rule the chat list draws between its favourites and the others.
        let tab = text_row(
            &crate::ui::render_to_string(&mut app, 120, 30),
            app.view.geometry.list_head.y,
        );
        assert!(
            tab.contains("TEAMS 2 ●1"),
            "{keymap:?}: Old is not counted, in rows or in unread: {tab:?}"
        );
    }
}

#[test]
fn the_demo_has_favourites_in_two_teams_one_of_them_a_general_channel() {
    let (mut app, _) = teams_app(Keymap::Standard);
    show_teams(&mut app);
    let lines = list_lines(&mut app, 30);
    assert!(lines[0].contains("Engineering › Incidents"), "{lines:?}");
    assert!(lines[1].contains("Marketing › General"), "{lines:?}");
    assert!(is_rule(&lines[2]), "{lines:?}");
    assert!(lines[3].contains("▸ Engineering"), "{lines:?}");
    assert!(
        lines.iter().all(|l| !l.contains("Legacy")),
        "the archived team is not listed: {lines:?}"
    );
}

#[test]
fn no_favourite_means_no_section_and_no_divider_and_turning_them_off_takes_them_away() {
    // None to begin with.
    let mut teams = favourite_teams();
    for c in teams.iter_mut().flat_map(|t| &mut t.channels) {
        c.favorite = None;
    }
    let (mut app, _) = app_with_teams(Keymap::Standard, teams);
    assert_eq!(names(&app), ["> Alpha", "> Beta"]);
    let lines = list_lines(&mut app, 30);
    assert!(
        lines.iter().all(|l| !is_rule(l) && !l.contains('›')),
        "{lines:?}"
    );

    // The `favorites` setting is off, and F8 (every keymap, `F` in vim's list) brings the section and takes it away.
    for keymap in [Keymap::Standard, Keymap::Vim] {
        let (mut app, _) = app_with_teams(keymap, favourite_teams());
        app.pin_favorites = false;
        assert_eq!(names(&app), ["> Alpha", "> Beta"], "{keymap:?}");
        let lines = list_lines(&mut app, 30);
        assert!(
            lines.iter().all(|l| !is_rule(l) && !l.contains('›')),
            "{keymap:?}: {lines:?}"
        );
        press(&mut app, KeyCode::F(8), KeyModifiers::NONE);
        assert!(app.pin_favorites);
        assert_eq!(names(&app).len(), 5, "{keymap:?}");
        assert!(list_lines(&mut app, 30).iter().any(|l| is_rule(l)));
        // Off again, from the list's own key where there is one; the cursor was on a favourite and is on a row still.
        app.focus = Focus::Chats;
        if keymap == Keymap::Vim {
            app.on_key(key('F'));
        } else {
            press(&mut app, KeyCode::F(8), KeyModifiers::NONE);
        }
        assert!(!app.pin_favorites, "{keymap:?}");
        assert_eq!(names(&app), ["> Alpha", "> Beta"], "{keymap:?}");
        assert!(app.team_cursor_index(&app.team_rows()) < 2);
    }
}

#[test]
fn a_favourite_opens_with_enter_right_or_a_click_without_unfolding_its_team() {
    for keymap in [Keymap::Standard, Keymap::Vim] {
        for how in ["enter", "open key", "click"] {
            let (mut app, _) = app_with_teams(keymap, favourite_teams());
            crate::ui::render_to_string(&mut app, 120, 30);
            let body = app.view.geometry.list_body;
            // The second favourite: Beta's Ops.
            match how {
                "click" => click(&mut app, body.x + 3, body.y + 1),
                _ => {
                    down(&mut app);
                    if how == "enter" {
                        enter(&mut app)
                    } else {
                        open_key(&mut app)
                    }
                }
            }
            let note = format!("{keymap:?} {how}");
            assert_eq!(app.current.as_deref(), Some("o"), "{note}");
            assert_eq!(app.focus, Focus::Messages, "{note}");
            assert_eq!(
                app.current_channel()
                    .map(|o| (o.team.as_str(), o.name.as_str())),
                Some(("Beta", "Ops")),
                "{note}"
            );
            assert_eq!(
                app.team_cursor,
                Some(RowKey::Favorite("o".into())),
                "{note}: the cursor stays on the row that was used"
            );
            assert_eq!(
                names(&app),
                [
                    "* Alpha › Plans",
                    "* Beta › Ops",
                    "* Alpha › General",
                    "> Alpha",
                    "> Beta"
                ],
                "{note}: no team unfolded"
            );
            assert!(frame(&mut app).contains("Beta › Ops"), "{note}: the title");
        }
    }
    // A favourite is not part of the tree: back does not fold or move anything.
    let (mut app, _) = app_with_teams(Keymap::Standard, favourite_teams());
    let before = (names(&app), app.team_cursor_index(&app.team_rows()));
    back_key(&mut app);
    assert_eq!(
        (names(&app), app.team_cursor_index(&app.team_rows())),
        before
    );
    // Nor does a click on the divider do anything.
    crate::ui::render_to_string(&mut app, 120, 30);
    let body = app.view.geometry.list_body;
    let (current, cursor) = (app.current.clone(), app.team_cursor.clone());
    click(&mut app, body.x + 3, body.y + 3);
    assert_eq!(
        (app.current.clone(), app.team_cursor.clone()),
        (current, cursor)
    );
    assert_eq!(names(&app).len(), 5);
}

#[test]
fn a_channel_that_is_a_favourite_and_in_an_open_team_is_marked_open_in_both_places() {
    let (mut app, _) = app_with_teams(Keymap::Standard, favourite_teams());
    move_to(&mut app, "> Alpha");
    open_key(&mut app);
    // Opened from the tree, then the cursor goes to the last row so that neither of the two rows has it.
    move_to(&mut app, "Plans");
    enter(&mut app);
    app.focus = Focus::Chats;
    press(&mut app, KeyCode::End, KeyModifiers::NONE);
    let lines = list_lines(&mut app, 30);
    let marked: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with('▎') && l.contains("Plans"))
        .collect();
    assert_eq!(
        marked.len(),
        2,
        "among the favourites and under the team: {lines:?}"
    );
    assert!(marked.iter().any(|l| l.contains("Alpha › Plans")));
    // Nothing else is marked.
    assert_eq!(
        lines.iter().filter(|l| l.starts_with('▎')).count(),
        2,
        "{lines:?}"
    );
}

#[test]
fn a_filter_matches_a_favourite_by_its_team_and_channel_text_and_a_match_opens_no_team() {
    for keymap in [Keymap::Standard, Keymap::Vim] {
        let (mut app, _) = app_with_teams(keymap, favourite_teams());
        if keymap == Keymap::Vim {
            app.on_key(key('/'));
        }
        type_text(&mut app, "beta › o");
        assert_eq!(
            names(&app),
            ["* Beta › Ops"],
            "{keymap:?}: the text as it is drawn; the team below matches nothing"
        );
        // The first Enter leaves the filter box with the result kept, the second opens the row.
        enter(&mut app);
        assert!(!app.filtering && app.filter == "beta › o", "{keymap:?}");
        enter(&mut app);
        assert_eq!(app.current.as_deref(), Some("o"), "{keymap:?}");
        app.focus = Focus::Chats;
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(
            names(&app),
            [
                "* Alpha › Plans",
                "* Beta › Ops",
                "* Alpha › General",
                "> Alpha",
                "> Beta"
            ],
            "{keymap:?}: Beta is still closed"
        );
        // A team's name matches its favourites and the team; the team keeps its fold state.
        app.filter.clear();
        app.filtering = true;
        type_text(&mut app, "alpha");
        assert_eq!(
            names(&app),
            ["* Alpha › Plans", "* Alpha › General", "> Alpha"],
            "{keymap:?}"
        );
        // A channel's name opens its team and shows only it, below the favourite that matched.
        app.filter.clear();
        type_text(&mut app, "plans");
        assert_eq!(
            names(&app),
            ["* Alpha › Plans", "v Alpha", "  Plans"],
            "{keymap:?}"
        );
        // With the favourites off, only the tree matches.
        app.pin_favorites = false;
        assert_eq!(names(&app), ["v Alpha", "  Plans"], "{keymap:?}");
    }
}

#[test]
fn archived_teams_are_hidden_everywhere_and_their_data_stays_loaded() {
    let (mut app, _) = app_with_teams(Keymap::Standard, favourite_teams());
    // Not in the rows, not as a favourite, not in the count of teams or of what is unread.
    assert!(
        names(&app)
            .iter()
            .all(|n| !n.contains("Old") && !n.contains("Lore"))
    );
    let text = frame(&mut app);
    let list = list_lines(&mut app, 40).join("\n");
    assert!(!list.contains("Old") && !list.contains("Lore"), "{list}");
    let head = text_row(&text, app.view.geometry.list_head.y);
    assert!(head.contains("TEAMS 2 ●1"), "{head:?}");
    // Not found by the filter, by team or channel name.
    for q in ["old", "lore", "General"] {
        app.filter.clear();
        app.filtering = true;
        type_text(&mut app, q);
        assert!(
            names(&app)
                .iter()
                .all(|n| !n.contains("Old") && !n.contains("Lore")),
            "{q}: {:?}",
            names(&app)
        );
    }
    app.filter.clear();
    app.filtering = false;
    // But the data is there: it is a view that hides them.
    assert_eq!(app.teams.items().len(), 3);
    assert!(app.teams.channel("l").is_some());
    // The tab counts only the teams that are shown, wherever the unread ones are.
    assert_eq!(app.tab_labels(60)[1].main, "TEAMS 2");

    // An open channel whose team is archived (it was archived while open) keeps its title.
    let (mut app, _) = app_with_teams(Keymap::Standard, favourite_teams());
    app.open_channel("p", false);
    let mut later = favourite_teams();
    later[0].archived = true;
    app.on_event(Ev::Teams(Ok(later)));
    assert!(!names(&app).iter().any(|n| n.contains("Alpha")));
    assert_eq!(
        app.current_channel()
            .map(|o| (o.team.as_str(), o.name.as_str())),
        Some(("Alpha", "Plans"))
    );
    assert!(frame(&mut app).contains("Alpha › Plans"));
}

// --- opening a channel ------------------------------------------------------------------------------------

#[test]
fn a_channel_opens_like_a_chat_with_a_title_of_its_own_and_asks_no_one_about_its_people() {
    for keymap in [Keymap::Standard, Keymap::Vim] {
        let spy = Arc::new(Spy {
            roster_probe: true,
            ..Spy::new()
        });
        let mut app = started(&spy, keymap);
        // A control: a chat with no people listed does make the app ask for them, so the silence below means something.
        app.select_chat("spy-group");
        pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
        assert_eq!(
            spy.members_asked(),
            ["spy-group"],
            "{keymap:?}: the chat's people were asked for"
        );

        open_channel_by_keys(&mut app, "Engineering", "Releases");
        assert_eq!(app.current.as_deref(), Some(RELEASES), "{keymap:?}");
        assert_eq!(app.focus, Focus::Messages);
        assert!(app.current_chat().is_none());
        assert_eq!(
            app.current_channel().map(|o| o.name.as_str()),
            Some("Releases")
        );
        // Plenty of ticks: the roster and presence paths all run.
        for _ in 0..5 {
            app.on_tick();
        }
        assert_eq!(
            spy.members_asked(),
            ["spy-group"],
            "{keymap:?}: a channel has no roster to ask for"
        );
        assert!(
            !spy.presence
                .lock()
                .unwrap()
                .iter()
                .any(|id| id == RELEASES || id.starts_with("demo-chan")),
            "{keymap:?}: nobody asked about the channel"
        );
        let text = frame(&mut app);
        let title = app.view.geometry.title;
        let right_of_list = |y: u16| -> String {
            text_row(&text, y)
                .chars()
                .skip(usize::from(title.x))
                .collect()
        };
        let line = right_of_list(title.y);
        assert!(
            line.contains("Engineering › Releases"),
            "{keymap:?}: {line:?}"
        );
        assert!(
            right_of_list(title.y + 1).contains("48 members"),
            "{keymap:?}"
        );
        for word in ["available", "busy", "away", "offline", "direct", "group"] {
            assert!(
                !line.contains(word),
                "{keymap:?}: a presence or a chat kind in a channel title: {line:?}"
            );
        }
        // Its messages are drawn like a chat's, and nobody's status message or out-of-office notice is shown over its box.
        assert!(text.contains("Release 4.2 is out"), "{keymap:?}\n{text}");
        assert!(app.banner_entries(Utc::now()).is_empty(), "{keymap:?}");
        assert_eq!(
            app.view.geometry.banner,
            Rect::default(),
            "{keymap:?}: no banner in a channel\n{text}"
        );
        // The channel is polled by its id, like the open chat.
        let id = RELEASES.to_string();
        app.states.get_mut(&id).unwrap().sched.set_last(ago(100));
        app.on_tick();
        assert!(app.states[&id].sched.last() > ago(10), "{keymap:?}: polled");
    }
}

#[test]
fn an_open_channel_is_not_moved_off_by_a_refresh_of_the_chat_list_and_leaves_the_chat_list_alone() {
    let (mut app, _) = teams_app(Keymap::Standard);
    open_channel_by_keys(&mut app, "Engineering", "Releases");
    app.focus = Focus::Messages;
    // A refresh of the chat list does not carry the person away from a conversation that is not in it.
    let chats: Vec<Chat> = app.chats.items().to_vec();
    app.on_event(Ev::Chats(Ok(chats)));
    app.on_tick();
    assert_eq!(app.current.as_deref(), Some(RELEASES));
    // The chat list, with the channel open, has no row marked as the open one and keeps its scroll position (on a
    // screen with room for two chats of the six, scrolled one down).
    alt(&mut app, '1');
    app.view.list_offset = 1;
    let text = crate::ui::render_to_string(&mut app, 100, 10);
    assert_eq!(
        app.view.list_offset, 1,
        "an open channel does not pull the list to the first chat"
    );
    let body = app.view.geometry.list_body;
    for y in body.y..body.y + body.height {
        assert!(
            !text_row(&text, y).starts_with('▌'),
            "no chat is marked open while a channel is: {}",
            text_row(&text, y)
        );
    }
    assert_eq!(app.current.as_deref(), Some(RELEASES));
    // Opening a chat from the list leaves the channel for good: everything about it is gone from the open chat.
    app.focus = Focus::Chats;
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert!(app.current_channel().is_none() && app.current_chat().is_some());
}

#[test]
fn opening_a_channel_unfolds_its_team_and_the_open_one_is_marked_when_the_cursor_is_elsewhere() {
    let (mut app, _) = teams_app(Keymap::Standard);
    open_channel_by_keys(&mut app, "Engineering", "Releases");
    alt(&mut app, '1');
    alt(&mut app, '2');
    assert_eq!(
        cursor_name(&app),
        "Releases",
        "the cursor is where the open channel is"
    );
    press(&mut app, KeyCode::Home, KeyModifiers::NONE);
    let text = frame(&mut app);
    // The open channel has its own bar (the conversation's gutter is the same glyph, so look at the list's edge).
    assert!(
        text.lines()
            .any(|l| l.starts_with('▎') && l.contains("Releases")),
        "{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.starts_with('▌') && l.contains("Engineering")),
        "and the cursor row has its own: {text}"
    );
}

#[test]
fn a_later_list_does_not_unfold_what_the_person_folded() {
    let (mut app, _) = teams_app(Keymap::Standard);
    open_channel_by_keys(&mut app, "Engineering", "Releases");
    app.focus = Focus::Chats;
    move_to(&mut app, "v Engineering");
    enter(&mut app);
    assert_eq!(names(&app)[2], "> Engineering");
    app.on_event(Ev::Teams(Ok(demo_list())));
    assert_eq!(names(&app)[2], "> Engineering");
    assert!(app.current_channel().is_some());
}

fn demo_list() -> Vec<Team> {
    DemoBackend::instant().teams().unwrap()
}

#[test]
fn a_channel_that_drops_out_of_the_list_stays_open_with_its_title() {
    let (mut app, _) = teams_app(Keymap::Standard);
    open_channel_by_keys(&mut app, "Engineering", "Releases");
    let mut gone = demo_list();
    gone[0].channels.retain(|c| c.id != RELEASES);
    app.on_event(Ev::Teams(Ok(gone)));
    assert_eq!(app.current.as_deref(), Some(RELEASES));
    assert!(
        app.current_channel().is_some(),
        "its title and its refusals stay"
    );
    assert!(frame(&mut app).contains("Engineering › Releases"));
    app.set_status("", false);
    app.focus = Focus::Editor;
    type_text(&mut app, "still not sent");
    enter(&mut app);
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, _)| m.contains("not available"))
    );
}

// --- posts and replies ----------------------------------------------------------------------------------

#[test]
fn replies_say_which_post_they_answer_and_posts_keep_their_titles() {
    let (mut app, _) = teams_app(Keymap::Standard);
    open_channel_by_keys(&mut app, "Engineering", "General");
    let text = crate::ui::render_to_string(&mut app, 120, 70);
    let lines: Vec<&str> = text.lines().collect();
    let at = |needle: &str| {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} is drawn:\n{text}"))
    };
    // The post's title is the line under its header (the header is the line with the sender that is not the event).
    let head = lines
        .iter()
        .position(|l| l.contains("Margaret Hamilton") && !l.contains("added"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(lines[head + 1].contains("Welcome to Engineering"), "{text}");
    assert!(
        at("↳ reply to Margaret Hamilton: Welcome to Engineering") > head,
        "the reply names the post's sender and title"
    );
    // A post with no title is quoted by its text.
    assert!(lines[at("↳ reply to Linus Torvalds: Does anyone still")].contains('↳'));
    // A reply whose post is not loaded says only that it is a reply, and nothing was asked for to find out more.
    at("↳ reply in a thread");
    assert!(text.contains("Following up on the earlier thread"));
    // The marker comes before the reply's own words.
    assert!(at("↳ reply to Margaret Hamilton: Welcome") < at("Thanks, read it"));
    // A system event in the feed is drawn as one.
    assert!(text.contains("added Ada Lovelace to the team"));
}

// --- nothing is sent from a channel --------------------------------------------------------------------------

#[test]
fn nothing_is_sent_replied_to_or_edited_in_a_channel_and_the_text_stays() {
    for keymap in [Keymap::Standard, Keymap::Vim] {
        let (mut app, spy) = teams_app(keymap);
        open_channel_by_keys(&mut app, "Engineering", "Releases");
        let sel = app.state().unwrap().sel;
        let said = |app: &App, what: &str| {
            app.status
                .as_ref()
                .is_some_and(|(m, _, err)| *err && m.contains(what))
        };

        // Typing is allowed; sending is refused by every key that sends, and the text stays.
        app.focus = Focus::Editor;
        if keymap == Keymap::Vim {
            app.editor.set_mode(Mode::Insert);
        }
        type_text(&mut app, "a post");
        let sends: Vec<KeyEvent> = if keymap == Keymap::Standard {
            vec![
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            ]
        } else {
            vec![
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            ]
        };
        for send in sends {
            app.status = None;
            app.on_key(send);
            app.on_tick();
            assert!(
                said(&app, "posting in channels is not available"),
                "{keymap:?}: {send:?}: {:?}",
                app.status
            );
            assert_eq!(app.editor.text(), "a post", "{keymap:?}: the text stays");
        }
        if keymap == Keymap::Vim {
            press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            for cmd in ["w", "send"] {
                app.status = None;
                run(&mut app, cmd);
                assert!(said(&app, "posting in channels"), "{keymap:?}: :{cmd}");
            }
        }
        assert_eq!(app.editor.text(), "a post");
        assert_eq!(app.sending, 0);
        assert!(
            spy.sent().is_empty(),
            "{keymap:?}: nothing reached the backend: {:?}",
            spy.sent()
        );
        assert!(
            app.state()
                .unwrap()
                .messages()
                .iter()
                .all(|m| !m.id.starts_with("local-"))
        );

        // A reply, an edit of your own message (the demo's last message there is yours), and favouriting.
        app.focus = Focus::Messages;
        for (keys, what) in [
            (
                if keymap == Keymap::Standard {
                    "ctrl-r"
                } else {
                    "r"
                },
                "replying in channels",
            ),
            (
                if keymap == Keymap::Standard {
                    "f4"
                } else {
                    "e"
                },
                "editing in channels",
            ),
            (
                if keymap == Keymap::Standard {
                    "f7"
                } else {
                    "f"
                },
                "not favourites",
            ),
        ] {
            app.status = None;
            app.focus = Focus::Messages;
            match keys {
                "ctrl-r" => ctrl(&mut app, 'r'),
                "f4" => press(&mut app, KeyCode::F(4), KeyModifiers::NONE),
                "f7" => press(&mut app, KeyCode::F(7), KeyModifiers::NONE),
                other => app.on_key(key(other.chars().next().unwrap())),
            }
            assert!(said(&app, what), "{keymap:?}: {keys}: {:?}", app.status);
            assert!(
                !app.is_replying() && !app.is_editing(),
                "{keymap:?}: {keys}"
            );
            assert_eq!(
                app.focus,
                Focus::Messages,
                "{keymap:?}: {keys}: the focus did not move into the box"
            );
        }
        if keymap == Keymap::Standard {
            // Up in an empty box edits your last message in a chat; here it must not move the selection either.
            app.focus = Focus::Editor;
            app.editor.clear();
            press(&mut app, KeyCode::Up, KeyModifiers::NONE);
            assert!(!app.is_editing());
        }
        assert_eq!(
            app.state().unwrap().sel,
            sel,
            "{keymap:?}: the selection stayed"
        );
        assert_eq!(spy.edits.load(Ordering::SeqCst), 0);
        assert_eq!(spy.favourited.load(Ordering::SeqCst), 0);

        // The box says why, with or without the keyboard.
        app.editor.clear();
        app.focus = Focus::Messages;
        assert!(frame(&mut app).contains("Posting in channels is not available yet"));
    }
}

#[test]
fn a_chat_still_sends_after_a_channel_was_open_and_the_channel_keeps_its_own_draft() {
    let (mut app, spy) = teams_app(Keymap::Standard);
    open_channel_by_keys(&mut app, "Engineering", "Releases");
    app.focus = Focus::Editor;
    type_text(&mut app, "for the channel");
    // Back to the chats: the draft is the channel's, not the chat's.
    alt(&mut app, '1');
    app.select_chat("demo-ada");
    assert_eq!(app.editor.text(), "");
    app.focus = Focus::Editor;
    type_text(&mut app, "hello ada");
    enter(&mut app);
    pump(&mut app, |a| a.sending == 0);
    assert_eq!(spy.sent(), ["demo-ada"], "an ordinary chat sends as ever");
    // Open the channel again: its draft is back, still not sent.
    alt(&mut app, '2');
    open_channel_by_keys(&mut app, "Engineering", "Releases");
    assert_eq!(app.editor.text(), "for the channel");
    assert_eq!(spy.sent(), ["demo-ada"]);
}

#[test]
fn reacting_and_searching_work_in_a_channel_and_a_refused_reaction_is_taken_back() {
    let (mut app, spy) = teams_app(Keymap::Standard);
    open_channel_by_keys(&mut app, "Engineering", "Releases");
    app.focus = Focus::Messages;
    let message = app.selected_msg().unwrap().id.clone();
    let mine = |app: &App| {
        app.selected_msg()
            .unwrap()
            .reactions
            .iter()
            .any(|r| r.users.iter().any(|u| u == "demo-me"))
    };
    // Refused: shown at once, then taken back, with the reason.
    spy.refuse_reactions.store(true, Ordering::SeqCst);
    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    assert!(matches!(app.overlay, Overlay::Emoji { .. }));
    enter(&mut app);
    assert!(mine(&app), "shown before the service has answered");
    pump(&mut app, |a| !a.reactions.is_pending(RELEASES, &message));
    assert!(!mine(&app), "taken back");
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.contains("reaction failed"))
    );
    // Accepted: it went to the channel's message. (The poll the refusal asked for answers first.)
    app.on_tick();
    app.on_tick();
    spy.refuse_reactions.store(false, Ordering::SeqCst);
    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    enter(&mut app);
    pump(&mut app, |a| !a.reactions.is_pending(RELEASES, &message));
    assert_eq!(
        spy.reacted.lock().unwrap().last(),
        Some(&(RELEASES.to_string(), message.clone())),
        "the reaction went to the channel's message"
    );
    assert!(
        mine(&app),
        "{:?} / {:?}",
        app.selected_msg().unwrap().reactions,
        app.status
    );
    // Search: the same popup, over the channel's messages.
    ctrl(&mut app, 'f');
    assert_eq!(app.overlay, Overlay::Search);
    type_text(&mut app, "tagged");
    enter(&mut app);
    pump(&mut app, |a| !a.search_pending());
    let rows = app.search_view().expect("the search is open").rows.len();
    assert!(rows >= 1, "the post that says it was tagged is found");
}

// --- unread -----------------------------------------------------------------------------------------------

fn unread_of(app: &App, id: &str) -> bool {
    let (_, ch) = app.teams.channel(id).unwrap();
    app.channel_unread(ch)
}

/// How many chats are unread, and whether the header says so.
fn chats_unread(app: &mut App) -> usize {
    let n = app
        .chats
        .items()
        .iter()
        .filter(|c| app.is_unread(c))
        .count();
    let header = text_row(&frame(app), 0);
    assert_eq!(
        header.contains(&format!("● {n} unread")),
        n > 0,
        "{header:?}"
    );
    n
}

#[test]
fn what_the_chat_list_says_about_channels_is_merged_over_the_teams_list_on_every_tick() {
    let (mut app, spy) = teams_app(Keymap::Standard);
    show_teams(&mut app);
    // The list itself called the releases channel read; the activity (which the app polls anyway) says it is not.
    assert!(unread_of(&app, RELEASES), "the activity made it unread");
    assert!(
        unread_of(&app, "demo-chan-incidents"),
        "the list's own word"
    );
    assert!(!unread_of(&app, GENERAL));
    // A newer state replaces both the flag and the newest message.
    let at = Utc::now();
    *spy.activity.lock().unwrap() = Some(vec![
        ChannelActivity {
            id: "demo-chan-design".into(),
            unread: true,
            last: Some(Preview {
                created: at,
                from_id: Some("u-ada".into()),
                from_name: Some("Ada Lovelace".into()),
                text: "new mock-ups".into(),
                automated: false,
            }),
        },
        ChannelActivity {
            id: RELEASES.into(),
            unread: false,
            last: None,
        },
    ]);
    app.on_tick();
    assert!(unread_of(&app, "demo-chan-design"));
    assert_eq!(
        app.teams
            .channel("demo-chan-design")
            .unwrap()
            .1
            .last
            .as_ref()
            .unwrap()
            .text,
        "new mock-ups"
    );
    assert!(!unread_of(&app, RELEASES), "the new word wins");
    // It shows: a bullet on the team (a count), on the channel and in the tab label.
    move_to(&mut app, "> Engineering");
    open_key(&mut app);
    let text = frame(&mut app);
    // The list's own columns, and a little of the divider.
    let list: Vec<String> = text.lines().map(|l| l.chars().take(40).collect()).collect();
    let row = |name: &str| {
        list.iter()
            .find(|l| l.contains(name))
            .unwrap_or_else(|| panic!("{name}"))
            .clone()
    };
    assert!(
        row("▾ Engineering").contains("●2"),
        "{}",
        row("▾ Engineering")
    );
    assert!(
        row("Design review").contains('●'),
        "{}",
        row("Design review")
    );
    assert!(!row("Releases").contains('●'), "{}", row("Releases"));
    assert!(row("TEAMS").contains("TEAMS 2 ●2"), "{}", row("TEAMS"));
}

#[test]
fn reading_a_channel_clears_it_here_and_on_the_service_and_a_newer_message_makes_it_unread_again() {
    let (mut app, spy) = teams_app(Keymap::Standard);
    show_teams(&mut app);
    // The chat that was open has been read here by now.
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.on_tick();
    app.on_tick();
    let chats_before = chats_unread(&mut app);
    assert!(chats_before > 0 && unread_of(&app, RELEASES));
    open_channel_by_keys(&mut app, "Engineering", "Releases");
    for _ in 0..3 {
        app.on_tick();
    }
    assert!(
        !unread_of(&app, RELEASES),
        "open and loaded: read here at once"
    );
    let newest = app.state().unwrap().messages().last().unwrap().id.clone();
    assert!(
        spy.inner
            .read_marks()
            .contains(&(RELEASES.to_string(), newest)),
        "and the service was told: {:?}",
        spy.inner.read_marks()
    );
    // The service's own word catches up (a chat list that says it is read) and the channel stays read.
    *spy.activity.lock().unwrap() = Some(vec![ChannelActivity {
        id: RELEASES.into(),
        unread: false,
        last: app.teams.channel(RELEASES).unwrap().1.last.clone(),
    }]);
    app.on_tick();
    assert!(!unread_of(&app, RELEASES));
    let marks = spy.inner.read_marks().len();
    for _ in 0..4 {
        app.on_tick();
    }
    assert_eq!(
        spy.inner.read_marks().len(),
        marks,
        "told once for each newest message, not on every tick"
    );
    // Something newer arrives and the activity says so: unread again... while it is open it is read at once.
    let later = Utc::now() + chrono::Duration::minutes(5);
    *spy.activity.lock().unwrap() = Some(vec![ChannelActivity {
        id: RELEASES.into(),
        unread: true,
        last: Some(Preview {
            created: later,
            from_id: Some("u-ada".into()),
            from_name: Some("Ada".into()),
            text: "one more thing".into(),
            automated: false,
        }),
    }]);
    // Leave the channel for a chat first, so that it is not open to read it.
    app.focus = Focus::Chats;
    app.select_chat("demo-ada");
    app.on_tick();
    assert!(
        unread_of(&app, RELEASES),
        "a message newer than the one read"
    );
    // The header's pill counts chats, and channels were never in it.
    assert_eq!(chats_unread(&mut app), chats_before);
}

// --- a side call -----------------------------------------------------------------------------------------

#[test]
fn a_failed_teams_list_is_shown_in_the_tab_and_never_read_as_a_lost_sign_in() {
    for failure in [
        Error::Status {
            operation: "list teams",
            status: 401,
            message: "unauthorised".into(),
        },
        Error::Auth(AuthError::NotSignedIn),
        Error::Status {
            operation: "list teams",
            status: 500,
            message: "oops".into(),
        },
    ] {
        let spy = Arc::new(Spy {
            can_sign_in_again: true,
            ..Spy::new()
        });
        spy.answer(Err(failure));
        let mut app = started(&spy, Keymap::Standard);
        alt(&mut app, '2');
        pump(&mut app, |a| a.teams.error().is_some());
        assert!(app.sign_in.is_online(), "the sign-in is untouched");
        assert_eq!(app.overlay, Overlay::None, "no sign-in popup");
        let shown = frame(&mut app);
        assert!(!shown.contains("signed out"), "{shown}");
        let list_head = app.view.geometry.list_body;
        let body: String = (list_head.y..list_head.y + 3)
            .map(|y| text_row(&shown, y))
            .collect();
        let said = app.teams.error().unwrap().to_string();
        let start: String = said.chars().take(12).collect();
        assert!(
            body.contains(&start),
            "the error is in the tab: {body:?} / {said:?}"
        );
        assert!(app.teams_available() && !app.teams.loaded());
        // The chat list and the open chat go on as ever.
        assert!(app.chats.error().is_none());
        // Asked again, it comes: back-off, not a verdict.
        spy.answer(Ok(demo_list()));
        app.teams.sched.set_last(ago(10 * 60));
        pump(&mut app, |a| a.teams.loaded());
        assert!(app.teams.error().is_none());
    }
}

#[test]
fn a_refusal_that_the_service_means_for_teams_alone_does_not_stop_the_messages() {
    let spy = Arc::new(Spy {
        can_sign_in_again: true,
        ..Spy::new()
    });
    spy.answer(Err(Error::Status {
        operation: "list teams",
        status: 401,
        message: String::new(),
    }));
    let mut app = started(&spy, Keymap::Standard);
    alt(&mut app, '2');
    pump(&mut app, |a| a.teams.error().is_some());
    app.select_chat("demo-ada");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    assert!(app.state().unwrap().error.is_none());
}

#[test]
fn a_service_that_cannot_list_teams_for_this_account_hides_the_tab_and_is_not_asked_again() {
    let spy = Arc::new(Spy::new());
    spy.answer(Err(Error::Unsupported {
        operation: "list teams",
        detail: "this account has no teams",
    }));
    let mut app = started(&spy, Keymap::Standard);
    alt(&mut app, '2');
    pump(&mut app, |a| !a.teams_available());
    assert_eq!(app.tab, Tab::Chats, "the tab it was on is gone");
    assert!(app.tab_labels(40).is_empty());
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, _)| m.contains("this account has no teams"))
    );
    let head = text_row(&frame(&mut app), app.view.geometry.list_head.y);
    assert!(!head.contains("TEAMS"), "{head:?}");
    let calls = spy.teams_calls();
    app.teams.sched.refresh_soon();
    for _ in 0..5 {
        app.on_tick();
    }
    alt(&mut app, '2');
    run(&mut app, "teams");
    run(&mut app, "refresh");
    assert_eq!(spy.teams_calls(), calls, "never again");
    assert_eq!(app.tab, Tab::Chats);
}

#[test]
fn a_service_that_asks_us_to_wait_is_not_asked_before_the_wait_is_over() {
    let (mut app, spy) = teams_app(Keymap::Standard);
    show_teams(&mut app);
    assert_eq!(spy.teams_calls(), 1);
    spy.answer(Err(Error::Throttled {
        operation: "list teams",
        retry_after: Some(Duration::from_secs(120)),
    }));
    // F5 on the tab asks again; the service says wait.
    app.teams.sched.set_last(ago_min(1));
    press(&mut app, KeyCode::F(5), KeyModifiers::NONE);
    pump(&mut app, |a| a.teams.sched.fails() == 1);
    assert_eq!(spy.teams_calls(), 2);
    assert!(app.teams.loaded(), "what was loaded is kept");
    assert!(app.teams.sched.held());
    // Neither a due schedule, a refresh-now, a command nor the key asks before the wait is over.
    app.teams.sched.refresh_soon();
    app.teams.sched.set_last(ago_min(60));
    for _ in 0..3 {
        app.on_tick();
    }
    run(&mut app, "refresh");
    press(&mut app, KeyCode::F(5), KeyModifiers::NONE);
    assert_eq!(spy.teams_calls(), 2);
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, _)| m.contains("asked us to wait"))
    );
}

// --- when the list is asked for ----------------------------------------------------------------------------

#[test]
fn the_list_is_asked_for_the_first_time_the_tab_is_shown_and_not_before() {
    let (mut app, spy) = teams_app(Keymap::Standard);
    app.teams.sched.refresh_soon();
    for _ in 0..10 {
        app.on_tick();
    }
    assert_eq!(spy.teams_calls(), 0, "nobody has looked at the tab");
    alt(&mut app, '2');
    assert!(app.is_busy(), "asked for by someone: the spinner shows");
    pump(&mut app, |a| a.teams.loaded());
    assert_eq!(spy.teams_calls(), 1);
    alt(&mut app, '1');
    alt(&mut app, '2');
    app.on_tick();
    assert_eq!(spy.teams_calls(), 1, "showing it again does not ask again");
}

#[test]
fn a_loaded_list_is_refreshed_every_ten_minutes_while_shown_and_every_half_hour_otherwise() {
    let (mut app, spy) = teams_app(Keymap::Standard);
    show_teams(&mut app);
    assert_eq!(spy.teams_calls(), 1);
    // In front: not at nine minutes, at eleven; and quietly (no spinner, nothing said).
    app.teams.sched.set_last(ago_min(9));
    app.on_tick();
    assert_eq!(spy.teams_calls(), 1);
    app.teams.sched.set_last(ago_min(11));
    app.on_tick();
    assert_eq!(spy.teams_calls(), 2);
    assert!(
        !app.is_busy(),
        "the scheduled refresh of a loaded list is quiet"
    );
    assert!(app.teams.is_fetching(), "though it is in flight");
    app.on_tick();
    assert!(!app.teams.is_fetching());
    // Not in front: eleven minutes is not enough, half an hour and a bit is.
    alt(&mut app, '1');
    app.teams.sched.set_last(ago_min(11));
    app.on_tick();
    assert_eq!(spy.teams_calls(), 2);
    app.teams.sched.set_last(ago_min(31));
    app.on_tick();
    assert_eq!(spy.teams_calls(), 3);
}

#[test]
fn refresh_asks_again_at_once_only_while_the_tab_is_in_front_and_shows_that_it_did() {
    for keymap in [Keymap::Standard, Keymap::Vim] {
        let (mut app, spy) = teams_app(keymap);
        show_teams(&mut app);
        app.teams.sched.set_last(ago(30));
        if keymap == Keymap::Standard {
            press(&mut app, KeyCode::F(5), KeyModifiers::NONE);
        } else {
            app.on_key(key('r'));
        }
        assert_eq!(spy.teams_calls(), 2, "{keymap:?}: refresh on the tab");
        assert!(
            app.is_busy(),
            "{keymap:?}: someone asked: the spinner shows"
        );
        pump(&mut app, |a| a.teams.loaded() && !a.teams.is_fetching());
        // On the chats tab refresh is about the chats.
        app.show_tab(Tab::Chats);
        run(&mut app, "refresh");
        assert_eq!(
            spy.teams_calls(),
            2,
            "{keymap:?}: not asked while the chats show"
        );
    }
}

#[test]
fn a_first_load_that_fails_is_tried_again_after_a_short_wait_that_doubles() {
    let (mut app, spy) = {
        let spy = Arc::new(Spy::new());
        spy.answer(Err(Error::Status {
            operation: "list teams",
            status: 503,
            message: "busy".into(),
        }));
        spy.answer(Err(Error::Status {
            operation: "list teams",
            status: 503,
            message: "busy".into(),
        }));
        let app = started(&spy, Keymap::Standard);
        (app, spy)
    };
    alt(&mut app, '2');
    pump(&mut app, |a| a.teams.sched.fails() == 1);
    assert_eq!(spy.teams_calls(), 1);
    // After one failure the wait is a minute (30 s doubled): not at 50 s, at 70 s.
    app.teams.sched.set_last(ago(50));
    app.on_tick();
    assert_eq!(spy.teams_calls(), 1);
    app.teams.sched.set_last(ago(70));
    pump(&mut app, |a| a.teams.sched.fails() == 2);
    assert_eq!(spy.teams_calls(), 2);
    // After two it is two minutes.
    app.teams.sched.set_last(ago(100));
    app.on_tick();
    assert_eq!(spy.teams_calls(), 2);
    app.teams.sched.set_last(ago(130));
    pump(&mut app, |a| a.teams.loaded());
    assert_eq!(spy.teams_calls(), 3);
}

#[test]
fn a_refresh_that_fails_keeps_the_list_and_says_so_once() {
    let (mut app, spy) = teams_app(Keymap::Standard);
    show_teams(&mut app);
    let rows = names(&app);
    for _ in 0..2 {
        spy.answer(Err(Error::Status {
            operation: "list teams",
            status: 502,
            message: "bad gateway".into(),
        }));
    }
    app.teams.sched.set_last(ago_min(11));
    pump(&mut app, |a| a.teams.sched.fails() == 1);
    assert_eq!(names(&app), rows, "the list is as it was");
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, _)| m.contains("teams not refreshed"))
    );
    app.status = None;
    app.teams.sched.set_last(ago_min(60));
    pump(&mut app, |a| a.teams.sched.fails() == 2);
    assert!(
        app.status.is_none(),
        "a streak of failures is said once, not at every try"
    );
}

#[test]
fn nothing_is_asked_for_while_the_sign_in_is_lost() {
    let (mut app, spy) = teams_app(Keymap::Standard);
    app.sign_in = SignIn::Expired {
        reason: "gone".into(),
        failure: None,
    };
    alt(&mut app, '2');
    app.teams.sched.refresh_soon();
    for _ in 0..3 {
        app.on_tick();
    }
    assert_eq!(spy.teams_calls(), 0);
}
