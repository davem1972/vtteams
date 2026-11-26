//! Searching the messages of the open chat, driven through the keys a person presses.

use super::*;
use chrono::DateTime;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use teams_client::{AuthError, SearchHit, SearchPage};

/// A backend for the search: the demo's chats, a chat that has as many older pages as a test asks for, and answers to
/// searches that a test writes (the demo's own search when none is left).
struct Scripted {
    inner: DemoBackend,
    can_search: bool,
    /// Whether it can sign in again while running (so that a refusal that means "signed out" would show as that).
    can_sign_in_again: bool,
    /// How many pages of ten messages `messages` makes up (page 0 is the newest); 0 means the demo's own messages.
    pages: usize,
    /// Fetching this older page (counting the first older one as 1) fails.
    fail_older_page: Option<usize>,
    base: DateTime<Utc>,
    answers: Mutex<VecDeque<Result<SearchPage, Error>>>,
    /// Every search asked of the backend: the text and where it started.
    asked: Mutex<Vec<(String, usize)>>,
    older_calls: AtomicUsize,
    /// Whether it has the demo's teams (the teams tab).
    has_teams: bool,
    /// Whether it can search several conversations at once (a team's channels).
    can_search_conversations: bool,
    /// Answers to searches of several conversations (the demo's own when none is left).
    team_answers: Mutex<VecDeque<Result<SearchPage, Error>>>,
    /// Every search of several conversations: the ids, the text and where it started.
    asked_conversations: Mutex<Vec<(Vec<String>, String, usize)>>,
}

impl Scripted {
    fn new(pages: usize) -> Self {
        Self {
            inner: DemoBackend::instant(),
            can_search: true,
            can_sign_in_again: false,
            pages,
            fail_older_page: None,
            base: Utc::now(),
            answers: Mutex::new(VecDeque::new()),
            asked: Mutex::new(Vec::new()),
            older_calls: AtomicUsize::new(0),
            has_teams: false,
            can_search_conversations: true,
            team_answers: Mutex::new(VecDeque::new()),
            asked_conversations: Mutex::new(Vec::new()),
        }
    }

    fn answer(&self, answer: Result<SearchPage, Error>) {
        self.answers.lock().unwrap().push_back(answer);
    }

    fn team_answer(&self, answer: Result<SearchPage, Error>) {
        self.team_answers.lock().unwrap().push_back(answer);
    }

    fn asked_conversations(&self) -> Vec<(Vec<String>, String, usize)> {
        self.asked_conversations.lock().unwrap().clone()
    }

    fn asked(&self) -> Vec<(String, usize)> {
        self.asked.lock().unwrap().clone()
    }

    fn older_calls(&self) -> usize {
        self.older_calls.load(Ordering::SeqCst)
    }

    fn sent(&self, n: usize) -> DateTime<Utc> {
        self.base - chrono::Duration::minutes(n as i64)
    }

    /// The message `n` steps back from the newest, as a hit.
    fn hit(&self, n: usize) -> SearchHit {
        SearchHit {
            message_id: format!("m{n}"),
            sent: self.sent(n),
            sender: "Someone".into(),
            preview: format!("message {n}"),
            conversation: String::new(),
            channel: String::new(),
        }
    }

    fn page_of(&self, hits: Vec<SearchHit>) -> Result<SearchPage, Error> {
        let total = hits.len() as u64;
        Ok(SearchPage {
            hits,
            next: None,
            total: Some(total),
        })
    }
}

impl Backend for Scripted {
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
        if self.pages == 0 {
            return self.inner.messages(id, older);
        }
        let page: usize = older.map_or(0, |c| c.parse().unwrap());
        if older.is_some() {
            self.older_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_older_page == Some(page) {
                return Err(scripted_failure());
            }
        }
        if page >= self.pages {
            return Ok(Page::default());
        }
        // Oldest first, so the highest "steps back" comes first.
        let messages = (0..10)
            .rev()
            .map(|i| {
                let n = page * 10 + i;
                let mut m = msg(&format!("m{n}"), 0);
                m.created = self.sent(n);
                m.modified = m.created;
                m.from_name = "Someone".into();
                m.body = format!("<p>message {n}</p>");
                m
            })
            .collect();
        Ok(Page {
            messages,
            older: (page + 1 < self.pages).then(|| (page + 1).to_string()),
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
    fn can_search_messages(&self) -> bool {
        self.can_search
    }
    fn can_sign_in_again(&self) -> bool {
        self.can_sign_in_again
    }
    fn search_messages(
        &self,
        chat_id: &str,
        query: &str,
        from: usize,
    ) -> Result<SearchPage, Error> {
        self.asked.lock().unwrap().push((query.to_string(), from));
        match self.answers.lock().unwrap().pop_front() {
            Some(answer) => answer,
            None => self.inner.search_messages(chat_id, query, from),
        }
    }
    fn can_search_conversations(&self) -> bool {
        self.can_search_conversations
    }
    fn search_conversations(
        &self,
        conversation_ids: &[String],
        query: &str,
        from: usize,
    ) -> Result<SearchPage, Error> {
        self.asked_conversations.lock().unwrap().push((
            conversation_ids.to_vec(),
            query.to_string(),
            from,
        ));
        match self.team_answers.lock().unwrap().pop_front() {
            Some(answer) => answer,
            None => self
                .inner
                .search_conversations(conversation_ids, query, from),
        }
    }
    fn can_list_teams(&self) -> bool {
        self.has_teams
    }
    fn teams(&self) -> Result<Vec<teams_client::Team>, Error> {
        self.inner.teams()
    }
    fn serves_image(&self, url: &str) -> bool {
        self.inner.serves_image(url)
    }
    fn image(&self, url: &str) -> Result<RgbImage, Error> {
        self.inner.image(url)
    }
}

/// An app on `backend` with `chat` open and loaded, focus on the conversation.
fn open_app(backend: &Arc<Scripted>, chat: &str, keymap: Keymap) -> App {
    let (mut app, _) = App::for_test(backend.clone());
    app.set_keymap(keymap);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.select_chat(chat);
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.focus = Focus::Messages;
    app
}

/// The demo's group chat (seven messages with an `e` in them, two with `join`), open in `keymap`.
fn platform(keymap: Keymap) -> (App, Arc<Scripted>) {
    let backend = Arc::new(Scripted::new(0));
    let app = open_app(&backend, "demo-platform", keymap);
    (app, backend)
}

/// A chat of `pages` pages of ten messages ("message 0" is the newest), open in the vim keymap.
fn history(pages: usize) -> (App, Arc<Scripted>) {
    let backend = Arc::new(Scripted::new(pages));
    let app = open_app(&backend, "demo-ada", Keymap::Vim);
    (app, backend)
}

fn enter(app: &mut App) {
    press(app, KeyCode::Enter, KeyModifiers::NONE);
}

/// Let the answer to a search the app asked for arrive.
fn settle(app: &mut App) {
    pump(app, |a| !a.search_pending());
}

fn row_ids(app: &App) -> Vec<String> {
    app.search_view()
        .expect("the search is open")
        .rows
        .iter()
        .map(|r| r.message_id.clone())
        .collect()
}

fn status(app: &App) -> String {
    app.search_view().expect("the search is open").status
}

fn selected_id(app: &App) -> String {
    app.selected_msg()
        .expect("a message is selected")
        .id
        .clone()
}

/// Open the search, type `text` and press Enter, like a person does (vim keys).
fn search_for(app: &mut App, text: &str) {
    app.on_key(key('/'));
    type_text(app, text);
    enter(app);
}

fn loaded_messages(app: &App) -> usize {
    app.state()
        .unwrap()
        .messages()
        .iter()
        .filter(|m| !m.system)
        .count()
}

fn ids_of_demo(app: &App, text: &str) -> Vec<String> {
    let mut found: Vec<&Msg> = app
        .state()
        .unwrap()
        .messages()
        .iter()
        .filter(|m| {
            !m.system
                && teams_client::html_to_plain(&m.body)
                    .to_lowercase()
                    .contains(text)
        })
        .collect();
    found.sort_by_key(|m| std::cmp::Reverse(m.created));
    found.iter().map(|m| m.id.clone()).collect()
}

// ---------------------------------------------------------------------------------------------------------------------
// Opening it
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn slash_opens_the_search_in_the_vim_conversation_but_is_just_a_character_in_the_message_box() {
    let (mut app, _) = platform(Keymap::Vim);
    app.on_key(key('/'));
    assert_eq!(app.overlay, Overlay::Search);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.overlay, Overlay::None);

    app.focus = Focus::Editor;
    app.editor.set_mode(Mode::Insert);
    app.on_key(key('/'));
    assert_eq!(app.overlay, Overlay::None, "typing a slash is not a search");
    assert_eq!(app.editor.text(), "/");
}

#[test]
fn ctrl_f_opens_the_search_from_the_conversation_and_the_message_box_in_the_standard_keymap() {
    let (mut app, _) = platform(Keymap::Standard);
    ctrl(&mut app, 'f');
    assert_eq!(app.overlay, Overlay::Search);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    app.focus = Focus::Editor;
    ctrl(&mut app, 'f');
    assert_eq!(app.overlay, Overlay::Search, "also while writing a message");
    assert_eq!(app.editor.text(), "", "and it typed nothing");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    app.focus = Focus::Chats;
    ctrl(&mut app, 'f');
    assert_eq!(
        app.overlay,
        Overlay::None,
        "the chat list has nothing to search"
    );

    // Not over another popup.
    app.focus = Focus::Editor;
    press(&mut app, KeyCode::F(2), KeyModifiers::NONE);
    assert!(matches!(app.overlay, Overlay::Emoji { .. }));
    ctrl(&mut app, 'f');
    assert!(
        matches!(app.overlay, Overlay::Emoji { .. }),
        "the emoji selector keeps the key"
    );
}

#[test]
fn nothing_opens_without_a_chat() {
    for keymap in [Keymap::Vim, Keymap::Standard] {
        let (mut app, _) = App::for_test(Arc::new(DemoBackend::instant()));
        app.set_keymap(keymap);
        assert!(app.current.is_none());
        app.focus = Focus::Messages;
        if keymap == Keymap::Vim {
            app.on_key(key('/'));
        } else {
            ctrl(&mut app, 'f');
        }
        assert_eq!(app.overlay, Overlay::None);
        assert!(app.status.as_ref().is_some_and(|(_, _, err)| *err));
        app.status = None;
        app.run_command("search anything");
        assert_eq!(app.overlay, Overlay::None);
        assert!(app.status.is_some(), "it says why");
    }
}

#[test]
fn the_search_command_opens_the_popup_with_the_text_and_asks_the_service() {
    let (mut app, backend) = platform(Keymap::Vim);
    type_command(&mut app, "search join");
    assert_eq!(app.overlay, Overlay::Search);
    assert_eq!(app.search_view().unwrap().query, "join");
    assert!(app.search_pending(), "it runs the search");
    settle(&mut app);
    assert_eq!(backend.asked(), [("join".to_string(), 0)]);
    assert!(status(&app).contains("on the server"), "{}", status(&app));

    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    type_command(&mut app, "search");
    assert_eq!(app.overlay, Overlay::Search);
    assert_eq!(app.search_view().unwrap().query, "");
    assert_eq!(backend.asked().len(), 1, "no text, nothing to ask");
}

// ---------------------------------------------------------------------------------------------------------------------
// While typing: the loaded messages, at once
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn typing_lists_the_loaded_messages_that_match_newest_first_and_sends_nothing() {
    let (mut app, backend) = platform(Keymap::Vim);
    app.on_key(key('/'));
    assert!(row_ids(&app).is_empty(), "nothing typed, nothing listed");

    type_text(&mut app, "JOIN");
    let expected = ids_of_demo(&app, "join");
    assert_eq!(
        expected.len(),
        2,
        "the demo has two messages about the join"
    );
    assert_eq!(
        row_ids(&app),
        expected,
        "newest first, whatever the case typed"
    );
    let view = app.search_view().unwrap();
    assert!(view.rows[0].sent > view.rows[1].sent);
    assert!(view.rows.iter().all(|r| !r.from_server));
    assert_eq!(
        view.status,
        "2 in the loaded messages; ⏎ searches the whole chat"
    );

    type_text(&mut app, " fix");
    assert_eq!(
        row_ids(&app),
        &expected[..1],
        "typing more narrows the list"
    );
    for _ in 0..4 {
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
    }
    assert_eq!(row_ids(&app), expected, "and erasing widens it again");
    assert!(backend.asked().is_empty(), "nothing went to the service");

    // Drawn: the date, the sender and the one-line preview.
    let screen = crate::ui::render_to_string(&mut app, 120, 30);
    assert!(screen.contains("Search this chat"), "{screen}");
    assert!(screen.contains("Margaret Hamilton"), "{screen}");
    assert!(screen.contains("take a look at the join fix"), "{screen}");
}

// ---------------------------------------------------------------------------------------------------------------------
// Enter: the service, in its order; Enter again: the hit
// ---------------------------------------------------------------------------------------------------------------------

/// The demo's answer for `e`: five a page, best match first, which is not by date.
fn demo_answer(backend: &Scripted, from: usize) -> SearchPage {
    backend
        .inner
        .search_messages("demo-platform", "e", from)
        .unwrap()
}

#[test]
fn enter_asks_the_service_and_lists_its_hits_in_its_order_then_the_loaded_ones_it_did_not_give() {
    let (mut app, backend) = platform(Keymap::Vim);
    let first = demo_answer(&backend, 0);
    let served: Vec<String> = first.hits.iter().map(|h| h.message_id.clone()).collect();
    let newest_first = {
        let mut hits = first.hits.clone();
        hits.sort_by_key(|h| std::cmp::Reverse(h.sent));
        hits.iter()
            .map(|h| h.message_id.clone())
            .collect::<Vec<_>>()
    };
    assert_ne!(served, newest_first, "the service's order is not by date");

    search_for(&mut app, "e");
    assert!(app.search_pending());
    assert!(status(&app).contains("searching"), "{}", status(&app));
    settle(&mut app);
    assert_eq!(backend.asked(), [("e".to_string(), 0)]);
    let view = app.search_view().unwrap();
    assert_eq!(
        view.status,
        "5 on the server (about 7) + 2 only in loaded messages; ↓ at the end loads more"
    );
    let ids = row_ids(&app);
    assert_eq!(ids.len(), 7);
    assert_eq!(
        &ids[..5],
        &served[..],
        "the service's hits come first, in its order"
    );
    let view = app.search_view().unwrap();
    assert!(view.rows[..5].iter().all(|r| r.from_server));
    assert!(view.rows[5..].iter().all(|r| !r.from_server));
    assert!(
        view.rows[5].sent > view.rows[6].sent,
        "the loaded matches after them are newest first"
    );
    assert_eq!(view.sel, 0);
    assert_eq!(view.enter, "open", "the list is current now");

    // The dates are shown, so the order is honest.
    let screen = crate::ui::render_to_string(&mut app, 120, 30);
    let year = chrono::Local::now().format("%Y-").to_string();
    assert!(screen.contains(&year), "{screen}");
}

#[test]
fn enter_again_opens_the_selected_hit_scrolled_into_view_and_leaves_the_follow_mode() {
    let (mut app, backend) = platform(Keymap::Vim);
    // A short terminal, so that the oldest message is not on screen until the search takes us there.
    let before = crate::ui::render_to_string(&mut app, 120, 16);
    assert!(!before.contains("Latency numbers"), "{before}");
    assert!(app.state().unwrap().follow);

    search_for(&mut app, "e");
    settle(&mut app);
    // The hit for the oldest real message, wherever the service ranks it (not first, so the cursor has to move).
    let hits = demo_answer(&backend, 0).hits;
    let at = hits
        .iter()
        .position(|h| h.preview.starts_with("Latency numbers"))
        .expect("the oldest message is a hit");
    assert!(at > 0, "it is not the first hit");
    let third = hits[at].clone();
    for _ in 0..at {
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    }
    enter(&mut app);

    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.focus, Focus::Messages);
    assert_eq!(selected_id(&app), third.message_id);
    assert!(
        !app.state().unwrap().follow,
        "the newest message is not followed any more"
    );
    let status = app.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
    assert!(
        status.starts_with(&format!("hit {} of ", at + 1)),
        "{status}"
    );
    let after = crate::ui::render_to_string(&mut app, 120, 16);
    assert!(after.contains("Latency numbers"), "{after}");
    assert_eq!(backend.asked().len(), 1, "opening a hit asks nothing more");
}

#[test]
fn escape_closes_the_search_and_changes_nothing_else() {
    let (mut app, _) = platform(Keymap::Vim);
    let (sel, follow) = {
        let st = app.state().unwrap();
        (st.sel, st.follow)
    };
    search_for(&mut app, "e");
    settle(&mut app);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.focus, Focus::Messages);
    let st = app.state().unwrap();
    assert_eq!((st.sel, st.follow), (sel, follow));
    assert!(!app.has_search_hits());
}

#[test]
fn moving_down_past_the_last_row_fetches_the_next_page_once() {
    let (mut app, backend) = platform(Keymap::Vim);
    search_for(&mut app, "e");
    settle(&mut app);
    for _ in 0..6 {
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    }
    assert_eq!(
        app.search_view().unwrap().sel,
        6,
        "the last of the seven rows"
    );
    assert_eq!(
        backend.asked().len(),
        1,
        "moving within the list asks nothing"
    );

    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert!(app.search_pending());
    assert!(status(&app).contains("loading more"), "{}", status(&app));
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    settle(&mut app);
    assert_eq!(
        backend.asked(),
        [("e".to_string(), 0), ("e".to_string(), 5)],
        "the next page starts where the service said, once"
    );
    let view = app.search_view().unwrap();
    assert_eq!(
        view.status,
        "7 on the server (about 7) + 0 only in loaded messages"
    );
    assert_eq!(view.rows.len(), 7);
    assert!(view.rows.iter().all(|r| r.from_server));
    assert_eq!(
        view.sel, 5,
        "the selection lands on the first row that arrived"
    );

    // Nothing more to fetch: going down at the end just stays there.
    for _ in 0..4 {
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    }
    settle(&mut app);
    assert_eq!(backend.asked().len(), 2);
}

// ---------------------------------------------------------------------------------------------------------------------
// Answers nobody wants any more
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn an_answer_for_text_that_was_typed_over_is_ignored() {
    let (mut app, backend) = platform(Keymap::Vim);
    search_for(&mut app, "e");
    type_text(&mut app, "x");
    assert!(
        !app.search_pending(),
        "what was asked is no longer waited for"
    );
    app.on_tick();
    assert_eq!(backend.asked().len(), 1);
    assert_eq!(
        app.search_server_rows_held(),
        0,
        "the stale answer was thrown away"
    );
    let view = app.search_view().unwrap();
    assert_eq!(view.query, "ex");
    assert!(view.rows.iter().all(|r| !r.from_server));
    assert!(!view.status.contains("on the server"), "{}", view.status);
}

#[test]
fn an_answer_that_arrives_after_the_popup_was_closed_is_ignored() {
    let (mut app, _) = platform(Keymap::Vim);
    search_for(&mut app, "e");
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    app.on_tick();
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.search_server_rows_held(), 0);
    assert!(app.search_view().is_none());
}

#[test]
fn an_answer_for_a_chat_that_was_left_is_ignored_even_in_the_next_popup() {
    let (mut app, _) = platform(Keymap::Vim);
    search_for(&mut app, "e");
    app.select_chat("demo-grace");
    assert_eq!(
        app.overlay,
        Overlay::None,
        "leaving the chat closes the search"
    );
    // The next chat's own search is open when the old answer arrives (no tick has delivered it yet).
    app.focus = Focus::Messages;
    app.on_key(key('/'));
    assert_eq!(app.overlay, Overlay::Search);
    app.on_tick();
    assert_eq!(
        app.search_server_rows_held(),
        0,
        "it was about another chat"
    );
    assert!(row_ids(&app).is_empty());
}

// ---------------------------------------------------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn a_service_that_cannot_search_is_not_asked_again_and_the_loaded_messages_are_searched() {
    let (mut app, backend) = platform(Keymap::Vim);
    backend.answer(Err(Error::Unsupported {
        operation: "search messages",
        detail: "no search here",
    }));
    search_for(&mut app, "join");
    settle(&mut app);
    let loaded = loaded_messages(&app);
    assert_eq!(
        status(&app),
        format!("searched the {loaded} loaded messages; older ones are not searched")
    );
    assert_eq!(row_ids(&app).len(), 2, "the loaded matches are still there");
    enter(&mut app);
    assert_eq!(app.overlay, Overlay::None, "Enter opens the hit now");

    search_for(&mut app, "join");
    assert_eq!(
        app.overlay,
        Overlay::None,
        "and straight away, without asking"
    );
    assert_eq!(backend.asked().len(), 1, "the service was asked only once");
}

#[test]
fn a_backend_without_search_gets_the_loaded_messages_and_enter_opens_the_hit() {
    let mut backend = Scripted::new(0);
    backend.can_search = false;
    let backend = Arc::new(backend);
    let mut app = open_app(&backend, "demo-platform", Keymap::Vim);
    app.on_key(key('/'));
    type_text(&mut app, "join");
    let loaded = loaded_messages(&app);
    assert_eq!(
        status(&app),
        format!("searched the {loaded} loaded messages; older ones are not searched")
    );
    let expected = ids_of_demo(&app, "join");
    enter(&mut app);
    assert!(!app.search_pending());
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(selected_id(&app), expected[0]);
    assert!(backend.asked().is_empty());
}

#[test]
fn the_demo_with_search_switched_off_falls_back_to_the_loaded_messages() {
    let (mut app, _) = App::for_test(Arc::new(DemoBackend::without_search()));
    app.set_keymap(Keymap::Standard);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.select_chat("demo-platform");
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    app.focus = Focus::Messages;
    ctrl(&mut app, 'f');
    type_text(&mut app, "join");
    assert!(
        status(&app).contains("older ones are not searched"),
        "{}",
        status(&app)
    );
    enter(&mut app);
    assert!(!app.search_pending());
    assert_eq!(app.overlay, Overlay::None, "Enter opened the hit");
    assert!(app.has_search_hits());
}

#[test]
fn a_failed_search_says_so_in_the_popup_and_keeps_the_loaded_matches() {
    let (mut app, backend) = platform(Keymap::Vim);
    backend.answer(Err(Error::Status {
        operation: "search messages",
        status: 500,
        message: "the service is unwell".into(),
    }));
    search_for(&mut app, "join");
    settle(&mut app);
    let view = app.search_view().unwrap();
    assert!(view.status.starts_with("search failed:"), "{}", view.status);
    assert!(view.status_is_error);
    assert_eq!(view.rows.len(), 2);
    assert_eq!(app.overlay, Overlay::Search, "the popup stays");
    enter(&mut app);
    assert_eq!(
        app.overlay,
        Overlay::None,
        "and a loaded hit can still be opened"
    );
}

#[test]
fn a_refusal_of_the_search_is_never_read_as_a_lost_sign_in() {
    for refusal in [
        Error::Auth(AuthError::NotSignedIn),
        Error::Status {
            operation: "search messages",
            status: 401,
            message: "no".into(),
        },
        Error::Auth(AuthError::Rejected {
            code: "invalid_grant".into(),
            description: "withheld".into(),
        }),
    ] {
        // A service that can sign in again, which is the case where a refusal read as "signed out" would show.
        let mut scripted = Scripted::new(0);
        scripted.can_sign_in_again = true;
        let backend = Arc::new(scripted);
        let mut app = open_app(&backend, "demo-platform", Keymap::Vim);
        backend.answer(Err(refusal));
        search_for(&mut app, "join");
        settle(&mut app);
        assert!(app.sign_in.is_online(), "the sign-in is untouched");
        assert_eq!(
            app.overlay,
            Overlay::Search,
            "no sign-in popup took its place"
        );
        let view = app.search_view().unwrap();
        assert!(view.status.starts_with("search failed:"), "{}", view.status);
        assert!(!view.status.contains("F6"), "{}", view.status);
        assert!(
            app.chats.error().is_none() && app.state().unwrap().error.is_none(),
            "nothing else was changed"
        );
    }
}

#[test]
fn an_unreadable_answer_is_an_error_not_no_hits_and_a_throttled_one_shows_the_wait() {
    let (mut app, backend) = platform(Keymap::Vim);
    backend.answer(Err(Error::Unexpected {
        operation: "search messages",
        detail: "the answer could not be read",
    }));
    search_for(&mut app, "zzz");
    settle(&mut app);
    let view = app.search_view().unwrap();
    assert!(
        view.status_is_error && view.status.contains("could not be read"),
        "{}",
        view.status
    );
    assert!(!view.status.contains("nothing found"));

    let (mut app, backend) = platform(Keymap::Vim);
    backend.answer(Err(Error::Throttled {
        operation: "search messages",
        retry_after: Some(Duration::from_secs(30)),
    }));
    search_for(&mut app, "zzz");
    settle(&mut app);
    assert!(status(&app).contains("retry in 30s"), "{}", status(&app));
}

#[test]
fn nothing_found_says_so() {
    let (mut app, _) = platform(Keymap::Vim);
    search_for(&mut app, "zzzzzz");
    settle(&mut app);
    assert_eq!(
        status(&app),
        "nothing found on the server or in the loaded messages"
    );
}

// ---------------------------------------------------------------------------------------------------------------------
// Going to a hit that is not loaded
// ---------------------------------------------------------------------------------------------------------------------

/// Search `history(..)` for a message at `steps_back` and open the hit: the seek has begun when this returns.
fn open_hit_of(app: &mut App, backend: &Scripted, hit: SearchHit) {
    backend.answer(backend.page_of(vec![hit]));
    search_for(app, "message");
    settle(app);
    // The loaded ten are listed after the service's hit; select the service's.
    assert!(app.search_view().unwrap().rows[0].from_server);
    enter(app);
}

fn wait_for_the_seek(app: &mut App) {
    for _ in 0..300 {
        app.on_tick();
        if app.state().is_some_and(|s| s.seek.is_none()) {
            return;
        }
    }
    panic!("the seek never ended");
}

#[test]
fn a_hit_older_than_the_loaded_pages_is_reached_by_fetching_older_pages_quietly() {
    let (mut app, backend) = history(5);
    assert_eq!(app.state().unwrap().messages().len(), 10);
    open_hit_of(&mut app, &backend, backend.hit(25));

    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.focus, Focus::Messages);
    assert!(app.state().unwrap().seek.is_some(), "the hunt has begun");
    assert!(app.state().unwrap().is_fetching());
    assert_eq!(
        app.status.as_ref().map(|s| s.0.as_str()),
        Some("looking for the message… (page 1)")
    );
    assert!(!app.is_busy(), "no spinner for it");

    wait_for_the_seek(&mut app);
    assert_eq!(
        backend.older_calls(),
        2,
        "two older pages hold messages 10-29"
    );
    assert_eq!(selected_id(&app), "m25");
    assert!(!app.state().unwrap().follow);
    assert_eq!(
        app.state().unwrap().automatic_pages(),
        0,
        "the automatic budget is untouched"
    );
    assert_eq!(
        app.status.as_ref().map(|s| s.0.as_str()),
        Some("hit 1 of 11")
    );
    // And it is on screen.
    let screen = crate::ui::render_to_string(&mut app, 100, 24);
    assert!(screen.contains("message 25"), "{screen}");
}

#[test]
fn a_hit_the_pages_do_not_hold_is_reported_once_they_are_older_than_it() {
    let (mut app, backend) = history(5);
    let mut ghost = backend.hit(25);
    ghost.message_id = "ghost".into();
    open_hit_of(&mut app, &backend, ghost);
    wait_for_the_seek(&mut app);
    // Page 2 held messages 20-29: older than the hit's time, and it was not in them. Page 3 is not asked for.
    assert_eq!(backend.older_calls(), 2);
    let (text, _, err) = app.status.clone().unwrap();
    assert_eq!(
        text,
        "could not reach that message; scroll up to load older ones"
    );
    assert!(err);
    assert_eq!(
        app.state().unwrap().messages().len(),
        30,
        "what arrived stays"
    );
}

#[test]
fn a_hunt_gives_up_after_sixty_pages() {
    let (mut app, backend) = history(200);
    let mut ghost = backend.hit(0);
    ghost.message_id = "ghost".into();
    ghost.sent = backend.sent(100_000);
    open_hit_of(&mut app, &backend, ghost);
    wait_for_the_seek(&mut app);
    assert_eq!(backend.older_calls(), 60);
    assert_eq!(app.state().unwrap().messages().len(), 610);
    assert_eq!(
        app.status.as_ref().map(|s| s.0.as_str()),
        Some("could not reach that message; scroll up to load older ones")
    );
}

#[test]
fn a_hunt_with_no_older_pages_ends_at_once() {
    let (mut app, backend) = history(1);
    let mut ghost = backend.hit(0);
    ghost.message_id = "ghost".into();
    ghost.sent = backend.sent(500);
    open_hit_of(&mut app, &backend, ghost);
    assert!(app.state().unwrap().seek.is_none());
    assert_eq!(backend.older_calls(), 0);
    assert!(
        app.status
            .as_ref()
            .is_some_and(|s| s.0.starts_with("could not reach"))
    );
}

#[test]
fn a_page_that_fails_ends_the_hunt_and_says_why() {
    let backend = Arc::new({
        let mut b = Scripted::new(5);
        b.fail_older_page = Some(2);
        b
    });
    let mut app = open_app(&backend, "demo-ada", Keymap::Vim);
    open_hit_of(&mut app, &backend, backend.hit(25));
    wait_for_the_seek(&mut app);
    let (text, _, err) = app.status.clone().unwrap();
    assert!(text.starts_with("could not reach that message:"), "{text}");
    assert!(err);
    assert!(app.sign_in.is_online());
}

#[test]
fn a_poll_in_flight_is_not_disturbed_by_the_hunt_which_waits_for_it() {
    let (mut app, backend) = history(5);
    backend.answer(backend.page_of(vec![backend.hit(25)]));
    search_for(&mut app, "message");
    settle(&mut app);
    let chat = app.current.clone().unwrap();
    assert!(app.start_message_fetch(&chat, Fetch::Newest { quiet: true }));
    enter(&mut app);
    assert_eq!(backend.older_calls(), 0, "the slot was taken");
    assert!(app.state().unwrap().seek.is_some());
    wait_for_the_seek(&mut app);
    assert_eq!(selected_id(&app), "m25");
    assert_eq!(backend.older_calls(), 2);
}

#[test]
fn switching_chats_ends_a_hunt() {
    let (mut app, backend) = history(5);
    open_hit_of(&mut app, &backend, backend.hit(25));
    assert!(app.state().unwrap().seek.is_some());
    app.select_chat("demo-grace");
    for _ in 0..5 {
        app.on_tick();
    }
    assert!(app.states["demo-ada"].seek.is_none());
    assert_eq!(
        backend.older_calls(),
        1,
        "the page already on its way arrived, and no more were asked for"
    );
}

#[test]
fn a_new_search_ends_a_hunt() {
    let (mut app, backend) = history(5);
    open_hit_of(&mut app, &backend, backend.hit(25));
    assert!(app.state().unwrap().seek.is_some());
    search_for(&mut app, "something else");
    assert!(app.state().unwrap().seek.is_none());
    settle(&mut app);
    for _ in 0..5 {
        app.on_tick();
    }
    assert_eq!(backend.older_calls(), 1);
}

// ---------------------------------------------------------------------------------------------------------------------
// Stepping through the hits
// ---------------------------------------------------------------------------------------------------------------------

/// Search the platform chat for `e`, remember the seven rows and open the first.
fn open_first_of_seven(app: &mut App) -> Vec<String> {
    search_for(app, "e");
    settle(app);
    let ids = row_ids(app);
    assert_eq!(ids.len(), 7);
    enter(app);
    assert_eq!(app.overlay, Overlay::None);
    ids
}

#[test]
fn n_and_capital_n_step_through_the_hits_and_wrap() {
    let (mut app, _) = platform(Keymap::Vim);
    let ids = open_first_of_seven(&mut app);
    assert_eq!(selected_id(&app), ids[0]);
    assert!(app.has_search_hits());

    app.on_key(key('n'));
    assert_eq!(selected_id(&app), ids[1]);
    assert_eq!(
        app.status.as_ref().map(|s| s.0.as_str()),
        Some("hit 2 of 7")
    );
    app.on_key(key('N'));
    assert_eq!(selected_id(&app), ids[0]);
    app.on_key(key('N'));
    assert_eq!(
        selected_id(&app),
        ids[6],
        "back from the first goes to the last"
    );
    assert_eq!(
        app.status.as_ref().map(|s| s.0.as_str()),
        Some("hit 7 of 7")
    );
    app.on_key(key('n'));
    assert_eq!(
        selected_id(&app),
        ids[0],
        "on from the last goes to the first"
    );
    assert_eq!(
        app.status.as_ref().map(|s| s.0.as_str()),
        Some("hit 1 of 7")
    );
}

#[test]
fn f3_and_shift_f3_step_through_the_hits_in_the_standard_keymap() {
    let (mut app, _) = platform(Keymap::Standard);
    app.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    type_text(&mut app, "e");
    enter(&mut app);
    settle(&mut app);
    let ids = row_ids(&app);
    enter(&mut app);
    assert_eq!(selected_id(&app), ids[0]);

    press(&mut app, KeyCode::F(3), KeyModifiers::NONE);
    assert_eq!(selected_id(&app), ids[1]);
    press(&mut app, KeyCode::F(3), KeyModifiers::SHIFT);
    assert_eq!(selected_id(&app), ids[0]);
    press(&mut app, KeyCode::F(3), KeyModifiers::SHIFT);
    assert_eq!(selected_id(&app), ids[6]);
    press(&mut app, KeyCode::F(3), KeyModifiers::NONE);
    assert_eq!(selected_id(&app), ids[0]);
}

#[test]
fn the_stepping_keys_do_nothing_without_hits() {
    let (mut app, _) = platform(Keymap::Vim);
    let before = app.state().unwrap().sel;
    for code in [KeyCode::Char('n'), KeyCode::Char('N'), KeyCode::F(3)] {
        press(&mut app, code, KeyModifiers::NONE);
    }
    assert_eq!(app.state().unwrap().sel, before);
    assert!(app.status.is_none(), "{:?}", app.status);
    assert!(!app.has_search_hits());
}

#[test]
fn the_hits_are_forgotten_when_the_chat_changes_or_a_new_search_runs() {
    let (mut app, _) = platform(Keymap::Vim);
    open_first_of_seven(&mut app);
    assert!(app.has_search_hits());
    app.select_chat("demo-grace");
    app.select_chat("demo-platform");
    assert!(
        !app.has_search_hits(),
        "they belonged to the chat that was left"
    );
    let sel = app.state().unwrap().sel;
    app.focus = Focus::Messages;
    app.on_key(key('n'));
    assert_eq!(app.state().unwrap().sel, sel);

    open_first_of_seven(&mut app);
    assert!(app.has_search_hits());
    search_for(&mut app, "join");
    assert!(!app.has_search_hits(), "a new search replaces them");
}

// ---------------------------------------------------------------------------------------------------------------------
// What reaches the screen, and limits
// ---------------------------------------------------------------------------------------------------------------------

#[test]
fn text_from_the_service_never_carries_a_control_or_bidi_character_to_the_screen() {
    let (mut app, backend) = platform(Keymap::Vim);
    backend.answer(Ok(SearchPage {
        hits: vec![SearchHit {
            message_id: "m1".into(),
            sent: Utc::now(),
            sender: "Ev\u{1b}]0;title\u{7}il\u{202e}".into(),
            preview: "\u{202e}hello\u{1b}[2J \u{9b}31mthere\u{200f}\r\n and\u{e000} done".into(),
            conversation: String::new(),
            channel: String::new(),
        }],
        next: None,
        total: Some(1),
    }));
    search_for(&mut app, "hello");
    settle(&mut app);
    let screen = crate::ui::render_to_string(&mut app, 120, 30);
    assert!(screen.contains("hello"), "{screen}");
    assert!(screen.contains("and done"), "{screen}");
    for c in screen.chars() {
        assert!(
            c == '\n'
                || !(c.is_control()
                    || matches!(
                        c,
                        '\u{202a}'..='\u{202e}' | '\u{200e}' | '\u{200f}' | '\u{e000}'
                    )),
            "{c:?} reached the screen:\n{screen}"
        );
    }
}

#[test]
fn the_text_searched_for_is_cut_at_two_hundred_characters_however_it_arrives() {
    let (mut app, _) = platform(Keymap::Vim);
    app.on_key(key('/'));
    for _ in 0..250 {
        app.on_key(key('a'));
    }
    assert_eq!(app.search_view().unwrap().query.chars().count(), 200);
    app.on_paste(&"b".repeat(50));
    assert_eq!(app.search_view().unwrap().query.chars().count(), 200);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    app.on_key(key('/'));
    app.on_paste(&"c".repeat(300));
    assert_eq!(app.search_view().unwrap().query, "c".repeat(200));
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);

    app.run_command(&format!("search {}", "d".repeat(300)));
    assert_eq!(app.search_view().unwrap().query, "d".repeat(200));
}

#[test]
fn the_popup_follows_the_messages_that_arrive_while_it_is_open() {
    let (mut app, _) = platform(Keymap::Vim);
    app.on_key(key('/'));
    type_text(&mut app, "brand new");
    assert!(row_ids(&app).is_empty());
    let chat = app.current.clone().unwrap();
    app.states.get_mut(&chat).unwrap().edit_messages(|msgs| {
        let mut m = msg("fresh", 0);
        m.body = "<p>a brand new message</p>".into();
        msgs.push(m);
    });
    app.on_tick();
    assert_eq!(row_ids(&app), ["fresh"]);
}

/// The row of key hints at the foot of the screen (above the command panel).
fn footer_keys(app: &mut App) -> String {
    let screen = crate::ui::render_to_string(app, 140, 30);
    screen.lines().rev().nth(1).unwrap_or("").to_string()
}

#[test]
fn the_search_is_in_the_help_and_in_the_hints_and_the_stepping_hint_comes_with_the_hits() {
    for (keymap, in_help, hint, step_hint) in [
        (Keymap::Vim, "/  :search x", " / ", "n/N"),
        (Keymap::Standard, "^f  :search x", "^f", "F3"),
    ] {
        let help: String = crate::ui::help_lines(keymap)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(help.contains(in_help), "{help}");
        assert!(help.contains("F3"), "{help}");
        let (mut app, _) = platform(keymap);
        let keys = footer_keys(&mut app);
        assert!(keys.contains(hint) && keys.contains("search"), "{keys}");
        assert!(!keys.contains(step_hint), "no hits yet: {keys}");
        // After a hit is opened.
        app.on_key(KeyEvent::new(
            KeyCode::Char(if keymap == Keymap::Vim { '/' } else { 'f' }),
            if keymap == Keymap::Vim {
                KeyModifiers::NONE
            } else {
                KeyModifiers::CONTROL
            },
        ));
        type_text(&mut app, "join");
        enter(&mut app);
        settle(&mut app);
        enter(&mut app);
        let keys = footer_keys(&mut app);
        assert!(keys.contains(step_hint), "{keys}");
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Searching all of a team from one of its channels
// ---------------------------------------------------------------------------------------------------------------------

const ENG: &str = "demo-team-eng";
const RELEASES: &str = "demo-chan-releases";

/// A backend with the demo's teams (Engineering's General and Releases have messages), `pages` as for `Scripted::new`.
fn teams_backend(pages: usize) -> Arc<Scripted> {
    let mut b = Scripted::new(pages);
    b.has_teams = true;
    Arc::new(b)
}

/// An app with the channel `id` open and loaded, the keyboard on its messages and the teams tab in front.
fn channel_open(backend: &Arc<Scripted>, id: &str, keymap: Keymap) -> App {
    let (mut app, _) = App::for_test(backend.clone());
    app.set_keymap(keymap);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.show_tab(crate::view::Tab::Teams);
    pump(&mut app, |a| a.teams.loaded());
    app.open_channel(id, false);
    pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    assert_eq!(app.focus, Focus::Messages);
    app
}

/// Open the popup the way a person of `app`'s keymap does.
fn open_popup(app: &mut App) {
    if app.keymap == Keymap::Standard {
        ctrl(app, 'f');
    } else {
        app.on_key(key('/'));
    }
    assert_eq!(app.overlay, Overlay::Search);
}

fn press_tab(app: &mut App) {
    press(app, KeyCode::Tab, KeyModifiers::NONE);
}

/// Open the popup, switch it to the team, type `text` and ask the service.
fn search_team_for(app: &mut App, text: &str) {
    open_popup(app);
    press_tab(app);
    type_text(app, text);
    enter(app);
    settle(app);
}

fn screen_of(app: &mut App) -> String {
    crate::ui::render_to_string(app, 130, 30)
}

/// (conversation, message id) of every row.
fn row_places(app: &App) -> Vec<(String, String)> {
    app.search_view()
        .expect("the search is open")
        .rows
        .iter()
        .map(|r| (r.conversation.clone(), r.message_id.clone()))
        .collect()
}

/// A hit of a team search, `n` steps back, in `conversation`.
fn team_hit(backend: &Scripted, n: usize, conversation: &str, channel: &str) -> SearchHit {
    SearchHit {
        conversation: conversation.into(),
        channel: channel.into(),
        ..backend.hit(n)
    }
}

#[test]
fn tab_switches_a_channel_search_between_the_channel_and_its_team_in_both_keymaps() {
    for keymap in [Keymap::Vim, Keymap::Standard] {
        let backend = teams_backend(0);
        let mut app = channel_open(&backend, RELEASES, keymap);
        open_popup(&mut app);
        let view = app.search_view().unwrap();
        assert_eq!(view.title, "Search Engineering › Releases", "{keymap:?}");
        assert_eq!(view.toggle.as_deref(), Some("tab: all of Engineering"));
        assert!(!view.show_channel);
        let screen = screen_of(&mut app);
        assert!(screen.contains("Search Engineering › Releases"), "{screen}");
        assert!(screen.contains("tab: all of Engineering"), "{screen}");

        type_text(&mut app, "wiki");
        assert_eq!(app.search_view().unwrap().rows.len(), 1);
        press_tab(&mut app);
        let view = app.search_view().unwrap();
        assert_eq!(view.title, "Search all of Engineering");
        assert_eq!(view.toggle.as_deref(), Some("tab: only this channel"));
        assert!(view.show_channel);
        assert_eq!(view.query, "wiki", "what was typed stays");
        assert_eq!(view.enter, "search all of Engineering");
        assert_eq!(
            view.status, "1 in this channel's loaded messages; ⏎ searches all of Engineering",
            "while typing, only the open channel's loaded messages are matched"
        );
        let screen = screen_of(&mut app);
        assert!(screen.contains("Search all of Engineering"), "{screen}");
        assert!(screen.contains("tab: only this channel"), "{screen}");
        assert!(screen.contains("#Releases"), "{screen}");

        press(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
        let view = app.search_view().unwrap();
        assert_eq!(view.title, "Search Engineering › Releases");
        assert_eq!(view.enter, "search the whole channel");
        assert!(backend.asked().is_empty() && backend.asked_conversations().is_empty());
    }
}

#[test]
fn there_is_no_toggle_in_a_chat_or_where_the_team_is_not_known_or_the_backend_cannot() {
    // A chat.
    let (mut app, _) = platform(Keymap::Standard);
    open_popup(&mut app);
    let before = (app.search_view().unwrap().title, status(&app));
    assert_eq!(before.0, "Search this chat");
    assert_eq!(app.search_view().unwrap().toggle, None);
    press_tab(&mut app);
    assert_eq!((app.search_view().unwrap().title, status(&app)), before);
    assert!(!app.search_view().unwrap().show_channel);
    assert!(!screen_of(&mut app).contains("tab:"));

    // A channel, on a backend that cannot search several conversations.
    let backend = Arc::new({
        let mut b = Scripted::new(0);
        b.has_teams = true;
        b.can_search_conversations = false;
        b
    });
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    open_popup(&mut app);
    assert_eq!(app.search_view().unwrap().title, "Search this chat");
    assert_eq!(app.search_view().unwrap().toggle, None);
    press_tab(&mut app);
    assert!(!app.search_view().unwrap().show_channel);
    type_text(&mut app, "wiki");
    enter(&mut app);
    settle(&mut app);
    assert!(backend.asked_conversations().is_empty());
    assert_eq!(backend.asked().len(), 1);

    // A channel the teams list no longer has: its team is not known.
    let backend = teams_backend(0);
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    app.teams.receive(Vec::new());
    assert!(app.current_channel().is_some(), "it is still the open one");
    open_popup(&mut app);
    assert_eq!(app.search_view().unwrap().toggle, None);
    press_tab(&mut app);
    assert!(!app.search_view().unwrap().show_channel);
}

#[test]
fn enter_in_the_team_scope_asks_for_every_channel_of_the_team_the_open_one_first() {
    for keymap in [Keymap::Vim, Keymap::Standard] {
        let backend = teams_backend(0);
        let mut app = channel_open(&backend, RELEASES, keymap);
        search_team_for(&mut app, "wiki");
        let asked = backend.asked_conversations();
        assert_eq!(asked.len(), 1, "{keymap:?}");
        let all = [
            RELEASES,
            ENG,
            "demo-chan-incidents",
            "demo-chan-design",
            "demo-chan-partners",
            "demo-chan-old",
            "demo-chan-random",
            "demo-chan-hack",
        ];
        assert_eq!(
            asked[0].0,
            all.map(String::from),
            "all of them, the archived too, this one first"
        );
        assert_eq!((asked[0].1.as_str(), asked[0].2), ("wiki", 0));
        assert!(backend.asked().is_empty(), "not the one-channel search");

        // Hits from two channels, each with its channel's name, and the newer of the two is the open channel's.
        let view = app.search_view().unwrap();
        let rows: Vec<(&str, &str, bool)> = view
            .rows
            .iter()
            .map(|r| (r.conversation.as_str(), r.channel.as_str(), r.from_server))
            .collect();
        assert_eq!(
            rows,
            [(ENG, "General", true), (RELEASES, "Releases", true)],
            "the service's order, and the open channel's copy is not listed twice"
        );
        assert_eq!(
            view.status,
            "2 on the server (about 2) + 0 only in this channel's loaded messages"
        );
        let screen = screen_of(&mut app);
        assert!(screen.contains("#General"), "{screen}");
        assert!(screen.contains("#Releases"), "{screen}");
        assert!(screen.contains("The rota is on the wiki"), "{screen}");
    }
}

#[test]
fn the_same_message_id_in_two_channels_is_two_rows() {
    let backend = teams_backend(0);
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    backend.team_answer(backend.page_of(vec![
        team_hit(&backend, 3, ENG, "General"),
        team_hit(&backend, 3, RELEASES, "Releases"),
    ]));
    search_team_for(&mut app, "message");
    assert_eq!(
        row_places(&app)[..2],
        [
            (ENG.to_string(), "m3".to_string()),
            (RELEASES.to_string(), "m3".to_string())
        ]
    );
}

#[test]
fn a_hit_naming_a_conversation_outside_the_team_is_not_listed() {
    let backend = teams_backend(0);
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    backend.team_answer(backend.page_of(vec![
        team_hit(&backend, 1, "19:evil@thread.tacv2", "Evil"),
        team_hit(&backend, 2, "", "Nowhere"),
        team_hit(&backend, 3, &ENG.to_ascii_uppercase(), ""),
    ]));
    search_team_for(&mut app, "zzz");
    assert_eq!(
        row_places(&app),
        [(ENG.to_string(), "m3".to_string())],
        "only the channel of the team, in the list's own spelling of its id"
    );
    assert_eq!(
        app.search_view().unwrap().rows[0].channel,
        "General",
        "the list's name when the service gives none"
    );
}

#[test]
fn enter_on_a_hit_in_another_channel_opens_that_channel_and_selects_the_message() {
    for keymap in [Keymap::Vim, Keymap::Standard] {
        let backend = teams_backend(0);
        let mut app = channel_open(&backend, RELEASES, keymap);
        // From the CHATS tab: opening a channel does not take the left pane to the teams.
        app.show_tab(crate::view::Tab::Chats);
        app.focus = Focus::Messages;
        search_team_for(&mut app, "wiki");
        let ids = row_places(&app);
        assert_eq!(ids[0].0, ENG, "the first hit is in the other channel");
        enter(&mut app);
        wait_for_the_seek(&mut app);

        assert_eq!(app.overlay, Overlay::None);
        assert_eq!(app.current.as_deref(), Some(ENG), "{keymap:?}");
        assert_eq!(app.current_channel().unwrap().name, "General");
        assert_eq!(app.tab, crate::view::Tab::Chats, "the tab stays as it was");
        assert_eq!(app.focus, Focus::Messages);
        assert_eq!(selected_id(&app), ids[0].1);
        assert_eq!(
            app.status.as_ref().map(|s| s.0.as_str()),
            Some("hit 1 of 2")
        );
        assert!(app.has_search_hits());
    }
}

#[test]
fn a_hit_in_the_open_channel_is_opened_where_it_is() {
    let backend = teams_backend(0);
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    search_team_for(&mut app, "wiki");
    press(&mut app, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(row_places(&app)[1].0, RELEASES);
    let id = row_places(&app)[1].1.clone();
    let loads = backend.older_calls();
    enter(&mut app);
    assert_eq!(app.current.as_deref(), Some(RELEASES));
    assert_eq!(selected_id(&app), id);
    assert_eq!(backend.older_calls(), loads);
}

#[test]
fn a_hit_in_another_channel_that_is_older_than_what_is_loaded_is_sought_with_older_pages() {
    let backend = teams_backend(5);
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    backend.team_answer(backend.page_of(vec![team_hit(&backend, 27, ENG, "General")]));
    search_team_for(&mut app, "message");
    assert_eq!(row_places(&app)[0], (ENG.to_string(), "m27".to_string()));
    enter(&mut app);
    assert_eq!(app.current.as_deref(), Some(ENG));
    wait_for_the_seek(&mut app);
    assert_eq!(
        backend.older_calls(),
        2,
        "two older pages of the other channel hold messages 10-29"
    );
    assert_eq!(selected_id(&app), "m27");
    assert_eq!(
        app.status.as_ref().map(|s| s.0.as_str()),
        Some("hit 1 of 11")
    );
    assert!(!app.state().unwrap().follow);
    let screen = screen_of(&mut app);
    assert!(screen.contains("message 27"), "{screen}");
}

#[test]
fn n_and_f3_step_through_the_hits_of_a_team_search_across_channels() {
    for keymap in [Keymap::Vim, Keymap::Standard] {
        let backend = teams_backend(0);
        let mut app = channel_open(&backend, RELEASES, keymap);
        search_team_for(&mut app, "wiki");
        let places = row_places(&app);
        assert_eq!(
            [places[0].0.as_str(), places[1].0.as_str()],
            [ENG, RELEASES]
        );
        enter(&mut app);
        wait_for_the_seek(&mut app);
        assert_eq!(app.current.as_deref(), Some(ENG));
        let (next, previous): (KeyEvent, KeyEvent) = if keymap == Keymap::Vim {
            (key('n'), key('N'))
        } else {
            (
                KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::F(3), KeyModifiers::SHIFT),
            )
        };

        app.on_key(next);
        wait_for_the_seek(&mut app);
        assert_eq!(app.current.as_deref(), Some(RELEASES), "{keymap:?}");
        assert_eq!(selected_id(&app), places[1].1);
        assert_eq!(
            app.status.as_ref().map(|s| s.0.as_str()),
            Some("hit 2 of 2")
        );
        assert!(
            app.has_search_hits(),
            "still steppable in the other channel"
        );
        app.on_key(next);
        wait_for_the_seek(&mut app);
        assert_eq!(app.current.as_deref(), Some(ENG), "it wraps");
        assert_eq!(selected_id(&app), places[0].1);
        app.on_key(previous);
        wait_for_the_seek(&mut app);
        assert_eq!(app.current.as_deref(), Some(RELEASES));
        assert_eq!(selected_id(&app), places[1].1);
    }
}

#[test]
fn switching_channels_by_hand_forgets_the_hits_of_a_team_search() {
    let backend = teams_backend(0);
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    search_team_for(&mut app, "wiki");
    enter(&mut app);
    wait_for_the_seek(&mut app);
    assert!(app.has_search_hits());
    app.open_channel(RELEASES, false);
    assert!(!app.has_search_hits());
}

#[test]
fn stepping_to_a_channel_the_list_no_longer_has_says_so_and_stays_where_it_is() {
    let backend = teams_backend(0);
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    search_team_for(&mut app, "wiki");
    enter(&mut app);
    wait_for_the_seek(&mut app);
    assert_eq!(app.current.as_deref(), Some(ENG));
    app.teams.receive(Vec::new());
    app.on_key(key('n'));
    assert_eq!(app.current.as_deref(), Some(ENG));
    assert!(
        app.status
            .as_ref()
            .is_some_and(|(m, _, err)| *err && m.contains("not in the teams list")),
        "{:?}",
        app.status
    );
    assert!(app.has_search_hits());
}

#[test]
fn an_answer_for_the_team_is_dropped_when_the_popup_went_back_to_the_channel() {
    let backend = teams_backend(0);
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    backend.team_answer(backend.page_of(vec![team_hit(&backend, 5, ENG, "General")]));
    open_popup(&mut app);
    press_tab(&mut app);
    type_text(&mut app, "wiki");
    enter(&mut app);
    assert!(app.search_pending());
    // Back to the channel before the answer arrives.
    press_tab(&mut app);
    assert!(!app.search_pending(), "what was asked is not wanted");
    for _ in 0..5 {
        app.on_tick();
    }
    assert_eq!(app.search_server_rows_held(), 0, "the answer was dropped");
    let view = app.search_view().unwrap();
    assert!(!view.show_channel);
    assert!(view.rows.iter().all(|r| !r.from_server));
    // And going to the team again does not bring it back: it is a new question.
    press_tab(&mut app);
    assert!(
        app.search_view()
            .unwrap()
            .rows
            .iter()
            .all(|r| !r.from_server)
    );
    assert_eq!(
        app.search_view().unwrap().enter,
        "search all of Engineering"
    );
}

#[test]
fn an_answer_for_the_team_is_dropped_when_the_chat_is_switched_or_the_popup_is_closed() {
    let backend = teams_backend(0);
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    backend.team_answer(backend.page_of(vec![team_hit(&backend, 5, ENG, "General")]));
    open_popup(&mut app);
    press_tab(&mut app);
    type_text(&mut app, "wiki");
    enter(&mut app);
    assert!(app.search_pending());
    app.select_chat("demo-ada");
    for _ in 0..5 {
        app.on_tick();
    }
    assert_eq!(app.overlay, Overlay::None);
    assert_eq!(app.search_server_rows_held(), 0);

    // A popup closed, and a newer one asked the same thing: the older answer is not the newer one's.
    let backend = teams_backend(0);
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    backend.team_answer(backend.page_of(vec![team_hit(&backend, 1, ENG, "stale")]));
    backend.team_answer(backend.page_of(vec![team_hit(&backend, 2, ENG, "fresh")]));
    open_popup(&mut app);
    press_tab(&mut app);
    type_text(&mut app, "message");
    enter(&mut app);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    open_popup(&mut app);
    press_tab(&mut app);
    type_text(&mut app, "message");
    enter(&mut app);
    settle(&mut app);
    let ids: Vec<String> = row_ids(&app);
    assert_eq!(ids, ["m2"], "{ids:?}");
}

#[test]
fn a_failed_team_search_is_said_in_the_popup_and_a_service_that_cannot_ends_the_toggle() {
    let backend = teams_backend(0);
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    backend.team_answer(Err(scripted_failure()));
    search_team_for(&mut app, "wiki");
    assert!(
        status(&app).starts_with("search failed:"),
        "{}",
        status(&app)
    );
    assert!(app.sign_in.is_online());
    assert!(app.search_view().unwrap().show_channel, "still the team");

    backend.team_answer(Err(Error::Unsupported {
        operation: "search messages",
        detail: "no",
    }));
    type_text(&mut app, "x");
    enter(&mut app);
    settle(&mut app);
    let view = app.search_view().unwrap();
    assert_eq!(view.title, "Search this chat", "back to the channel alone");
    assert_eq!(view.toggle, None);
    assert!(!view.show_channel);
    assert_eq!(backend.asked_conversations().len(), 2);
    // Not offered again this session.
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    open_popup(&mut app);
    assert_eq!(app.search_view().unwrap().toggle, None);
}

#[test]
fn the_search_command_searches_the_channel_alone() {
    let backend = teams_backend(0);
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    type_command(&mut app, "search wiki");
    settle(&mut app);
    assert_eq!(backend.asked(), [("wiki".to_string(), 0)]);
    assert!(backend.asked_conversations().is_empty());
    let view = app.search_view().unwrap();
    assert!(!view.show_channel);
    assert_eq!(view.title, "Search Engineering › Releases");
}

#[test]
fn a_team_of_more_channels_than_a_search_covers_says_so() {
    let backend = teams_backend(0);
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    let channel = |n: usize| teams_client::Channel {
        id: if n == 0 {
            RELEASES.to_string()
        } else {
            format!("big-{n}")
        },
        name: format!("Channel {n}"),
        general: n == 1,
        kind: teams_client::ChannelKind::Standard,
        archived: false,
        shown: true,
        pinned: false,
        muted: false,
        followed: false,
        favorite: None,
        unread: false,
        last: None,
    };
    app.teams.receive(vec![teams_client::Team {
        id: "big".into(),
        name: "Big".into(),
        archived: false,
        muted: false,
        members: None,
        channels: (1..60).chain([0]).map(channel).collect(),
    }]);
    search_team_for(&mut app, "wiki");
    let asked = backend.asked_conversations();
    assert_eq!(asked[0].0.len(), 60, "the app gives them all");
    assert_eq!(asked[0].0[0], RELEASES, "the open one first");
    assert!(
        status(&app).ends_with("; only 50 of its 60 channels are searched"),
        "{}",
        status(&app)
    );
}

#[test]
fn ctrl_f_from_the_teams_list_searches_the_open_channel_and_slash_there_still_filters() {
    let backend = teams_backend(0);
    let mut app = channel_open(&backend, RELEASES, Keymap::Standard);
    press(&mut app, KeyCode::Left, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Chats);
    assert!(app.on_teams_tab());
    ctrl(&mut app, 'f');
    assert_eq!(app.overlay, Overlay::Search);
    assert_eq!(
        app.search_view().unwrap().title,
        "Search Engineering › Releases"
    );
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Chats);
    // The hint says so while a channel is open.
    assert!(
        footer_keys(&mut app).contains("^f"),
        "{}",
        footer_keys(&mut app)
    );

    // With no channel open there is nothing to search.
    let mut app = started_on_teams(&backend, Keymap::Standard);
    assert!(app.current_channel().is_none());
    ctrl(&mut app, 'f');
    assert_eq!(app.overlay, Overlay::None);
    assert!(!footer_keys(&mut app).contains("^f"));

    // The vim keymap's `/` in the teams list is still the filter.
    let mut app = channel_open(&backend, RELEASES, Keymap::Vim);
    press(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
    assert_eq!(app.focus, Focus::Chats);
    app.on_key(key('/'));
    assert_eq!(app.overlay, Overlay::None);
    assert!(app.filtering);
}

/// An app whose teams tab is showing and which has nothing open.
fn started_on_teams(backend: &Arc<Scripted>, keymap: Keymap) -> App {
    let (mut app, _) = App::for_test(backend.clone());
    app.set_keymap(keymap);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.current = None;
    app.show_tab(crate::view::Tab::Teams);
    pump(&mut app, |a| a.teams.loaded());
    assert_eq!(app.focus, Focus::Chats);
    app
}

#[test]
fn the_help_of_both_keymaps_names_the_toggle() {
    for keymap in [Keymap::Vim, Keymap::Standard] {
        let help: String = crate::ui::help_lines(keymap)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(help.contains("tab (in the search)"), "{keymap:?}");
        assert!(help.contains("all of its team"), "{keymap:?}");
    }
}

#[test]
fn the_demo_searches_the_channels_of_a_team_unless_it_is_told_not_to() {
    let demo = DemoBackend::instant();
    assert!(demo.can_search_conversations());
    let ids = [ENG.to_string(), RELEASES.to_string(), "unknown".to_string()];
    let page = demo.search_conversations(&ids, "wiki", 0).unwrap();
    let found: Vec<(&str, &str)> = page
        .hits
        .iter()
        .map(|h| (h.conversation.as_str(), h.channel.as_str()))
        .collect();
    assert_eq!(found, [(ENG, "General"), (RELEASES, "Releases")]);
    let none = DemoBackend::without_search();
    assert!(!none.can_search_conversations());
    assert!(matches!(
        none.search_conversations(&ids, "wiki", 0),
        Err(Error::Unsupported { .. })
    ));
}
