//! Posting to a channel, replying in a post's thread and editing a post, driven through the keys a person presses in
//! both keymaps, against the demo's channels with a backend that records what it was asked to do.

use super::*;
use std::collections::VecDeque;
use std::sync::Mutex;
use teams_client::{ChannelPost, Team};

const RELEASES: &str = "demo-chan-releases";
const GENERAL: &str = "demo-team-eng";
const QUESTION: &str = "Post to Engineering › Releases? Everyone in the channel will see it.";

/// A post or reply the backend was asked to make.
#[derive(Clone, Debug, PartialEq)]
struct Posted {
    channel: String,
    html: String,
    subject: Option<String>,
    thread: Option<String>,
}

/// An edit of a post the backend was asked to make.
#[derive(Clone, Debug, PartialEq)]
struct PostEdit {
    channel: String,
    id: String,
    subject: Option<String>,
    html: String,
}

/// The demo, recording what it is asked to post, edit and send, and failing the calls a test scripts to fail.
struct Poster {
    inner: DemoBackend,
    posts: Mutex<Vec<Posted>>,
    post_edits: Mutex<Vec<PostEdit>>,
    /// (chat, the message replied to) of every chat send.
    sent: Mutex<Vec<(String, Option<String>)>>,
    /// Messages of chats edited with the chat call.
    chat_edits: Mutex<Vec<String>>,
    failures: Mutex<VecDeque<Error>>,
    /// The mentions of every post or reply, and of every edit of a post, in the order they were asked for.
    post_mentions: Mutex<Vec<Vec<Mention>>>,
    edit_mentions: Mutex<Vec<Vec<Mention>>>,
    /// Messages added to the Releases channel's first page, for a test to give it senders the demo's lack.
    planted: Mutex<Vec<Msg>>,
    /// Conversations `members` was asked about.
    members_asked: Mutex<Vec<String>>,
    /// The HTML and mentions of every chat send.
    sent_with: Mutex<Vec<(String, Vec<Mention>)>>,
    /// People the Platform chat has on top of its own.
    extra_in_platform: Mutex<Vec<teams_client::Member>>,
}

/// A mention as a test reads it: the number in the HTML, the person's id and the name.
type Mention = (u32, String, String);

fn mention_list(mentions: &[teams_client::OutMention]) -> Vec<Mention> {
    mentions
        .iter()
        .map(|m| (m.id, m.user_id.clone(), m.name.clone()))
        .collect()
}

impl Poster {
    fn new(inner: DemoBackend) -> Self {
        Self {
            inner,
            post_mentions: Mutex::new(Vec::new()),
            edit_mentions: Mutex::new(Vec::new()),
            planted: Mutex::new(Vec::new()),
            members_asked: Mutex::new(Vec::new()),
            sent_with: Mutex::new(Vec::new()),
            extra_in_platform: Mutex::new(Vec::new()),
            posts: Mutex::new(Vec::new()),
            post_edits: Mutex::new(Vec::new()),
            sent: Mutex::new(Vec::new()),
            chat_edits: Mutex::new(Vec::new()),
            failures: Mutex::new(VecDeque::new()),
        }
    }

    fn posts(&self) -> Vec<Posted> {
        self.posts.lock().unwrap().clone()
    }

    fn post_edits(&self) -> Vec<PostEdit> {
        self.post_edits.lock().unwrap().clone()
    }

    /// The next post or edit of a post fails with `e`.
    fn fail_next(&self, e: Error) {
        self.failures.lock().unwrap().push_back(e);
    }
}

impl Backend for Poster {
    fn label(&self) -> String {
        self.inner.label()
    }
    fn me(&self) -> Result<Me, Error> {
        self.inner.me()
    }
    fn chats(&self) -> Result<Vec<Chat>, Error> {
        let mut chats = self.inner.chats()?;
        let extra = self.extra_in_platform.lock().unwrap().clone();
        for c in chats.iter_mut().filter(|c| c.id == "demo-platform") {
            c.members.extend(extra.clone());
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
        let mut page = self.inner.messages(id, older)?;
        if older.is_none() && id == RELEASES {
            page.messages.extend(self.planted.lock().unwrap().clone());
        }
        Ok(page)
    }
    fn members(&self, chat_id: &str) -> Result<Vec<teams_client::Member>, Error> {
        self.members_asked.lock().unwrap().push(chat_id.to_string());
        self.inner.members(chat_id)
    }
    fn send(
        &self,
        id: &str,
        html: &str,
        m: &[teams_client::OutMention],
        reply_to: Option<&teams_client::ReplyTarget>,
    ) -> Result<Msg, Error> {
        self.sent
            .lock()
            .unwrap()
            .push((id.to_string(), reply_to.map(|t| t.id.clone())));
        self.sent_with
            .lock()
            .unwrap()
            .push((html.to_string(), mention_list(m)));
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
        self.chat_edits.lock().unwrap().push(message_id.to_string());
        self.inner.edit(chat_id, message_id, html, mentions)
    }
    fn can_post_to_channels(&self) -> bool {
        self.inner.can_post_to_channels()
    }
    fn post_to_channel(&self, channel: &str, post: &ChannelPost<'_>) -> Result<Msg, Error> {
        self.posts.lock().unwrap().push(Posted {
            channel: channel.to_string(),
            html: post.html.to_string(),
            subject: post.subject.map(String::from),
            thread: post.thread.map(String::from),
        });
        self.post_mentions
            .lock()
            .unwrap()
            .push(mention_list(post.mentions));
        if let Some(e) = self.failures.lock().unwrap().pop_front() {
            return Err(e);
        }
        self.inner.post_to_channel(channel, post)
    }
    fn can_edit_posts(&self) -> bool {
        self.inner.can_edit_posts()
    }
    fn edit_post(
        &self,
        channel: &str,
        message_id: &str,
        subject: Option<&str>,
        html: &str,
        mentions: &[teams_client::OutMention],
    ) -> Result<(), Error> {
        self.post_edits.lock().unwrap().push(PostEdit {
            channel: channel.to_string(),
            id: message_id.to_string(),
            subject: subject.map(String::from),
            html: html.to_string(),
        });
        self.edit_mentions
            .lock()
            .unwrap()
            .push(mention_list(mentions));
        if let Some(e) = self.failures.lock().unwrap().pop_front() {
            return Err(e);
        }
        self.inner
            .edit_post(channel, message_id, subject, html, mentions)
    }
    fn can_list_teams(&self) -> bool {
        true
    }
    fn teams(&self) -> Result<Vec<Team>, Error> {
        self.inner.teams()
    }
    fn channel_activity(&self) -> Vec<teams_client::ChannelActivity> {
        self.inner.channel_activity()
    }
    fn serves_image(&self, url: &str) -> bool {
        self.inner.serves_image(url)
    }
    fn image(&self, url: &str) -> Result<RgbImage, Error> {
        self.inner.image(url)
    }
}

fn poster_app(keymap: Keymap) -> (App, Arc<Poster>) {
    poster_app_over(keymap, DemoBackend::instant())
}

fn poster_app_over(keymap: Keymap, demo: DemoBackend) -> (App, Arc<Poster>) {
    let poster = Arc::new(Poster::new(demo));
    let (mut app, _) = App::for_test(poster.clone());
    app.set_keymap(keymap);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    (app, poster)
}

/// Show the teams tab and open the channel `id` (the keys that get there are the teams tests' business).
fn open(app: &mut App, id: &str) {
    if app.keymap == Keymap::Standard {
        press(app, KeyCode::Char('2'), KeyModifiers::ALT);
    } else {
        app.focus = Focus::Chats;
        type_text(app, "gt");
    }
    pump(app, |a| a.teams.loaded());
    app.open_channel(id, false);
    pump(app, |a| a.state().is_some_and(|s| s.loaded));
    assert_eq!(app.current.as_deref(), Some(id));
}

/// Type `text` into the message box the way a person of the keymap does.
fn compose(app: &mut App, text: &str) {
    app.focus = Focus::Editor;
    if app.keymap == Keymap::Vim {
        app.editor.set_mode(Mode::Insert);
    }
    type_text(app, text);
}

/// The key that sends from the message box in `app`'s keymap, pressed by someone who has read the question first: a
/// question still being asked is taken to have been on screen for a second.
fn send(app: &mut App) {
    if let Some((_, at)) = &mut app.post_asked {
        *at = (*at).min(ago(1));
    }
    send_at_once(app);
}

/// The same key, pressed with no time for the question to have been read (what a held-down key does).
fn send_at_once(app: &mut App) {
    if app.keymap == Keymap::Standard {
        press(app, KeyCode::Enter, KeyModifiers::NONE);
    } else {
        ctrl(app, 's');
    }
}

fn reply_key(app: &mut App) {
    app.focus = Focus::Messages;
    if app.keymap == Keymap::Standard {
        ctrl(app, 'r');
    } else {
        app.on_key(key('r'));
    }
}

fn edit_key(app: &mut App) {
    app.focus = Focus::Messages;
    if app.keymap == Keymap::Standard {
        press(app, KeyCode::F(4), KeyModifiers::NONE);
    } else {
        app.on_key(key('e'));
    }
}

/// A command typed in the command panel (Ctrl-P in the standard keymap, `:` in vim).
fn run(app: &mut App, cmd: &str) {
    if app.keymap == Keymap::Standard {
        ctrl(app, 'p');
    } else {
        app.focus = Focus::Messages;
        app.on_key(key(':'));
    }
    type_text(app, cmd);
    press(app, KeyCode::Enter, KeyModifiers::NONE);
}

/// Esc until the keyboard has left the message box (vim needs two from insert mode).
fn leave_box(app: &mut App) {
    for _ in 0..3 {
        if app.focus != Focus::Editor {
            return;
        }
        press(app, KeyCode::Esc, KeyModifiers::NONE);
    }
}

fn status(app: &App) -> String {
    app.status
        .as_ref()
        .map(|(m, _, _)| m.clone())
        .unwrap_or_default()
}

fn frame(app: &mut App) -> String {
    crate::ui::render_to_string(app, 140, 50)
}

/// The id of the loaded message whose text holds `text`.
fn id_of(app: &App, text: &str) -> String {
    app.state()
        .unwrap()
        .messages()
        .iter()
        .find(|m| m.body.contains(text))
        .unwrap_or_else(|| panic!("no message with {text:?}"))
        .id
        .clone()
}

fn select(app: &mut App, id: &str) {
    let st = app.state_mut().unwrap();
    let at = st.position_of(id).expect("the message is loaded");
    st.select_at(at);
    app.focus = Focus::Messages;
}

const BOTH: [Keymap; 2] = [Keymap::Standard, Keymap::Vim];

// --- the first post to a channel asks ----------------------------------------------------------------------------

#[test]
fn the_first_post_to_a_channel_asks_and_the_next_send_posts_and_it_is_not_asked_again() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        compose(&mut app, "first words");
        send(&mut app);
        let said = status(&app);
        assert!(said.contains(QUESTION), "{keymap:?}: {said}");
        if keymap == Keymap::Standard {
            assert!(said.ends_with("Press Enter again to post."), "{said}");
        }
        assert!(
            poster.posts().is_empty(),
            "{keymap:?}: nothing was sent by the question"
        );
        assert_eq!(app.sending, 0);
        assert_eq!(app.editor.text(), "first words", "the text is kept");
        assert!(
            app.state()
                .unwrap()
                .messages()
                .iter()
                .all(|m| !m.id.starts_with("local-"))
        );

        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(
            poster.posts(),
            [Posted {
                channel: RELEASES.into(),
                html: "<p>first words</p>".into(),
                subject: None,
                thread: None
            }],
            "{keymap:?}"
        );
        assert_eq!(app.editor.text(), "");

        // Not asked again for this channel.
        compose(&mut app, "second words");
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(poster.posts().len(), 2, "{keymap:?}");
        assert!(!status(&app).contains("Everyone in the channel"));
    }
}

#[test]
fn another_channel_asks_again_and_so_does_a_wait_or_any_other_key() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        compose(&mut app, "words");
        send(&mut app);
        // A key that is not a send drops the question.
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        send(&mut app);
        assert!(poster.posts().is_empty(), "{keymap:?}: asked again");
        assert!(status(&app).contains(QUESTION));
        // So does waiting too long.
        app.post_asked = Some((RELEASES.to_string(), ago(11)));
        send(&mut app);
        assert!(
            poster.posts().is_empty(),
            "{keymap:?}: the answer came late"
        );
        assert!(status(&app).contains(QUESTION));
        // The answer to a question about one channel is not an answer for another.
        app.post_asked = Some((GENERAL.to_string(), Instant::now()));
        send(&mut app);
        assert!(poster.posts().is_empty(), "{keymap:?}");
        // At last.
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(poster.posts().len(), 1, "{keymap:?}");

        // Another channel is asked about on its own.
        open(&mut app, GENERAL);
        compose(&mut app, "hello general");
        send(&mut app);
        assert_eq!(poster.posts().len(), 1, "{keymap:?}");
        assert!(
            status(&app).contains("Post to Engineering › General?"),
            "{}",
            status(&app)
        );
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(poster.posts().len(), 2);
        assert_eq!(poster.posts()[1].channel, GENERAL);
    }
}

fn ago_ms(ms: u64) -> Instant {
    Instant::now()
        .checked_sub(Duration::from_millis(ms))
        .expect("the clock has run long enough")
}

#[test]
fn a_send_that_comes_at_once_after_the_question_is_not_its_answer_and_a_held_key_never_is() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        compose(&mut app, "words");
        send_at_once(&mut app);
        assert!(status(&app).contains(QUESTION), "{keymap:?}");
        // A key held down: the same send over and over with nothing between, far more than enough to pass the gap in
        // key presses even if not in time.
        for _ in 0..200 {
            send_at_once(&mut app);
        }
        assert!(poster.posts().is_empty(), "{keymap:?}: a repeat posted");
        assert!(
            status(&app).contains("too quickly"),
            "{keymap:?}: {}",
            status(&app)
        );
        assert_eq!(app.editor.text(), "words", "the text is kept");
        assert!(app.post_asked.is_some(), "the question is still asked");
        // The person lets go, reads, and presses again.
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(poster.posts().len(), 1, "{keymap:?}");
        assert!(!status(&app).contains("too quickly"), "{}", status(&app));
    }
}

#[test]
fn the_gap_is_measured_from_the_question_and_the_window_still_ends_it() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        compose(&mut app, "words");
        send_at_once(&mut app);
        // Just short of the gap: refused (and asked afresh).
        app.post_asked = Some((RELEASES.to_string(), ago_ms(300)));
        send_at_once(&mut app);
        assert!(poster.posts().is_empty(), "{keymap:?}: 300 ms is too soon");
        // Past it: the answer.
        app.post_asked = Some((RELEASES.to_string(), ago_ms(500)));
        send_at_once(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(poster.posts().len(), 1, "{keymap:?}: 500 ms is an answer");
        // The ten seconds still end the question.
        open(&mut app, GENERAL);
        compose(&mut app, "hello");
        send_at_once(&mut app);
        app.post_asked = Some((GENERAL.to_string(), ago(11)));
        send_at_once(&mut app);
        assert_eq!(poster.posts().len(), 1, "{keymap:?}: too late");
        assert!(status(&app).contains("Post to Engineering › General?"));
    }
}

#[test]
fn a_mouse_event_or_a_paste_drops_the_question_so_the_next_send_asks_again() {
    use ratatui::crossterm::event::{MouseEvent, MouseEventKind};
    for keymap in BOTH {
        for between in ["nothing", "mouse", "paste"] {
            let (mut app, poster) = poster_app(keymap);
            open(&mut app, RELEASES);
            compose(&mut app, "words");
            send(&mut app);
            assert!(app.post_asked.is_some());
            match between {
                "mouse" => app.on_mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 1,
                    row: 1,
                    modifiers: KeyModifiers::NONE,
                }),
                "paste" => app.on_paste("more"),
                _ => {}
            }
            send(&mut app);
            pump(&mut app, |a| a.sending == 0);
            let posted = poster.posts().len();
            if between == "nothing" {
                assert_eq!(posted, 1, "{keymap:?}: the control posts");
            } else {
                assert_eq!(posted, 0, "{keymap:?}: after {between} it asked again");
                assert!(status(&app).contains(QUESTION), "{keymap:?} {between}");
            }
        }
    }
}

#[test]
fn a_reply_is_asked_about_too_and_once_asked_replies_and_posts_go_straight_out() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        let post = id_of(&app, "Tagged and published");
        select(&mut app, &post);
        reply_key(&mut app);
        compose(&mut app, "a reply");
        send(&mut app);
        assert!(poster.posts().is_empty(), "{keymap:?}");
        assert!(
            status(&app).contains(QUESTION),
            "{keymap:?}: {}",
            status(&app)
        );
        assert!(app.is_replying(), "the reply target is kept while asking");
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(poster.posts().len(), 1);
        // A new post after a reply is not asked about.
        compose(&mut app, "a post");
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(poster.posts().len(), 2, "{keymap:?}");
    }
}

// --- the subject ---------------------------------------------------------------------------------------------------

#[test]
fn a_post_takes_the_subject_set_with_the_command_and_spends_it() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        assert_eq!(app.pending_subject(), None);
        run(&mut app, "subject  Weekly   plan ");
        assert_eq!(app.pending_subject(), Some("Weekly plan"), "{keymap:?}");
        assert!(
            frame(&mut app).contains("SUBJECT: Weekly plan"),
            "{keymap:?}: the pill"
        );
        // Shown by the command alone.
        app.status = None;
        run(&mut app, "subject");
        assert!(status(&app).contains("Weekly plan"), "{}", status(&app));
        assert_eq!(app.pending_subject(), Some("Weekly plan"));

        compose(&mut app, "the body");
        send(&mut app);
        assert_eq!(
            app.pending_subject(),
            Some("Weekly plan"),
            "kept by the question"
        );
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(poster.posts()[0].subject.as_deref(), Some("Weekly plan"));
        assert_eq!(poster.posts()[0].thread, None);
        assert_eq!(app.pending_subject(), None, "spent");
        assert!(!frame(&mut app).contains("SUBJECT:"), "{keymap:?}");

        // The next post has none.
        compose(&mut app, "no title");
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(poster.posts()[1].subject, None, "{keymap:?}");
        // The confirmed post shows with its title.
        let titled = app
            .state()
            .unwrap()
            .messages()
            .iter()
            .any(|m| m.subject.as_deref() == Some("Weekly plan"));
        assert!(titled, "{keymap:?}");
    }
}

#[test]
fn the_subject_is_cleared_by_the_command_and_by_escape_from_an_empty_box_not_one_with_text() {
    for keymap in BOTH {
        let (mut app, _) = poster_app(keymap);
        open(&mut app, RELEASES);
        run(&mut app, "subject Plan");
        run(&mut app, "subject -");
        assert_eq!(app.pending_subject(), None, "{keymap:?}");

        run(&mut app, "subject Plan");
        compose(&mut app, "draft words");
        leave_box(&mut app);
        assert_eq!(app.focus, Focus::Messages);
        assert_eq!(
            app.pending_subject(),
            Some("Plan"),
            "{keymap:?}: the title is a draft with the text"
        );
        assert_eq!(app.editor.text(), "draft words");

        app.focus = Focus::Editor;
        app.editor.clear();
        leave_box(&mut app);
        assert_eq!(app.pending_subject(), None, "{keymap:?}: an empty box");

        // Switching to another conversation drops it.
        run(&mut app, "subject Plan");
        app.select_chat("demo-ada");
        assert_eq!(app.pending_subject(), None, "{keymap:?}");
    }
}

#[test]
fn a_subject_is_refused_in_a_chat_while_replying_and_when_unusable_and_a_reply_is_refused_while_one_is_set()
 {
    for keymap in BOTH {
        let (mut app, _) = poster_app(keymap);
        // In a chat.
        app.select_chat("demo-ada");
        run(&mut app, "subject Plan");
        assert_eq!(app.pending_subject(), None, "{keymap:?}");
        assert!(
            status(&app).contains("a post in a channel"),
            "{}",
            status(&app)
        );

        open(&mut app, RELEASES);
        // Too long, and nothing.
        run(&mut app, &format!("subject {}", "x".repeat(201)));
        assert_eq!(app.pending_subject(), None, "{keymap:?}");
        assert!(status(&app).contains("at most 200"), "{}", status(&app));
        run(&mut app, "subject");
        assert!(status(&app).contains("no subject"), "{}", status(&app));

        // While replying.
        let post = id_of(&app, "Tagged and published");
        select(&mut app, &post);
        reply_key(&mut app);
        assert!(app.is_replying());
        run(&mut app, "subject Plan");
        assert_eq!(app.pending_subject(), None, "{keymap:?}");
        assert!(
            status(&app).contains("a reply has no subject"),
            "{}",
            status(&app)
        );
        // `:subject -` is always allowed.
        run(&mut app, "subject -");
        assert!(
            app.is_replying(),
            "clearing a subject does not stop a reply"
        );

        // And replying while one is set.
        app.cancel_reply();
        run(&mut app, "subject Plan");
        select(&mut app, &post);
        reply_key(&mut app);
        assert!(!app.is_replying(), "{keymap:?}");
        assert!(
            status(&app).contains("a reply has no subject"),
            "{}",
            status(&app)
        );
        assert_eq!(app.pending_subject(), Some("Plan"), "nothing was lost");
    }
}

// --- a reply in a thread ----------------------------------------------------------------------------------------

#[test]
fn a_reply_goes_to_the_thread_of_the_post_even_from_one_of_its_replies_and_quotes_nothing() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        let post = id_of(&app, "Tagged and published");
        let reply = id_of(&app, "The notes are on the wiki");
        // From the post.
        select(&mut app, &post);
        reply_key(&mut app);
        assert!(
            app.is_replying() && app.focus == Focus::Editor,
            "{keymap:?}"
        );
        let text = frame(&mut app);
        assert!(text.contains("REPLYING IN THREAD"), "{keymap:?}\n{text}");
        assert!(
            text.contains("Release 4.2 is out"),
            "{keymap:?}: the pill names the post's title"
        );
        assert_eq!(app.editor.text(), "", "nothing is pre-filled or quoted");
        compose(&mut app, "from the post");
        send(&mut app);
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(poster.posts()[0].thread.as_deref(), Some(post.as_str()));
        assert_eq!(poster.posts()[0].subject, None);
        assert_eq!(poster.posts()[0].html, "<p>from the post</p>");
        assert!(!app.is_replying(), "spent");

        // From a reply: the thread is the post's, not the reply's.
        select(&mut app, &reply);
        reply_key(&mut app);
        compose(&mut app, "from a reply");
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(
            poster.posts()[1].thread.as_deref(),
            Some(post.as_str()),
            "{keymap:?}: the root, not the reply's own id"
        );

        // The sent replies are in the thread when the service's copy comes: the new message has the post as its root.
        let sent = app
            .state()
            .unwrap()
            .messages()
            .iter()
            .filter(|m| m.body.contains("from the post") || m.body.contains("from a reply"))
            .map(|m| m.root_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(sent, [Some(post.clone()), Some(post)], "{keymap:?}");
    }
}

#[test]
fn the_arrow_keys_pick_the_message_to_reply_to_in_a_channel() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        app.focus = Focus::Messages;
        // The newest is Ada's reply to the post; three up is the Linus reply to the first post, two up is the post
        // "Release 4.2 is out", and one up is yours.
        let key = |app: &mut App| {
            if keymap == Keymap::Standard {
                press(app, KeyCode::Up, KeyModifiers::NONE);
            } else {
                app.on_key(super::key('k'));
            }
        };
        frame(&mut app);
        key(&mut app);
        frame(&mut app);
        key(&mut app);
        frame(&mut app);
        let post = id_of(&app, "Tagged and published");
        assert_eq!(app.selected_msg().map(|m| m.id.clone()), Some(post.clone()));
        reply_key(&mut app);
        assert!(app.is_replying(), "{keymap:?}: {}", status(&app));
        compose(&mut app, "by the arrows");
        send(&mut app);
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(poster.posts()[0].thread.as_deref(), Some(post.as_str()));
    }
}

#[test]
fn a_reply_never_carries_a_subject_even_if_one_is_somehow_set() {
    // The keys never leave both set (`:subject` refuses while replying and replying refuses while one is set), so this
    // sets it behind their back: the guard is in the send itself.
    let (mut app, poster) = poster_app(Keymap::Standard);
    open(&mut app, RELEASES);
    let post = id_of(&app, "Tagged and published");
    select(&mut app, &post);
    reply_key(&mut app);
    app.subject = Some("Plan".into());
    compose(&mut app, "a reply");
    send(&mut app);
    send(&mut app);
    pump(&mut app, |a| a.sending == 0);
    assert_eq!(poster.posts()[0].subject, None);
    assert_eq!(poster.posts()[0].thread.as_deref(), Some(post.as_str()));
}

#[test]
fn the_pill_names_the_sender_of_a_post_without_a_title_and_a_reply_to_an_unloaded_post_names_that_post()
 {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, GENERAL);
        let rota = id_of(&app, "Does anyone still have");
        select(&mut app, &rota);
        reply_key(&mut app);
        let text = frame(&mut app);
        assert!(
            text.contains("REPLYING IN THREAD") && text.contains("Linus Torvalds"),
            "{keymap:?}\n{text}"
        );
        app.cancel_reply();

        // A reply to a post that is not loaded still has a thread to go to; the pill has no name for it.
        let orphan = id_of(&app, "Following up on the earlier thread");
        select(&mut app, &orphan);
        reply_key(&mut app);
        assert!(app.is_replying(), "{keymap:?}");
        compose(&mut app, "late reply");
        send(&mut app);
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(
            poster.posts()[0].thread.as_deref(),
            Some("an-older-post-that-is-not-loaded"),
            "{keymap:?}"
        );
    }
}

#[test]
fn what_cannot_be_replied_to_is_refused_with_the_box_untouched() {
    for keymap in BOTH {
        let (mut app, _) = poster_app(keymap);
        open(&mut app, GENERAL);
        // An event.
        let event = app
            .state()
            .unwrap()
            .messages()
            .iter()
            .find(|m| m.system)
            .unwrap()
            .id
            .clone();
        select(&mut app, &event);
        reply_key(&mut app);
        assert!(!app.is_replying(), "{keymap:?}");
        assert!(
            status(&app).contains("nothing to reply to"),
            "{}",
            status(&app)
        );
        assert_eq!(app.focus, Focus::Messages);

        // A deleted message.
        let mut gone = app
            .state()
            .unwrap()
            .messages()
            .iter()
            .find(|m| m.body.contains("It is in the Platform Team chat"))
            .unwrap()
            .clone();
        gone.id = "1999999999999".into();
        gone.deleted = true;
        let me = app.me_id().to_string();
        app.state_mut().unwrap().receive_page(
            Fetch::Newest { quiet: true },
            Page {
                messages: vec![gone],
                older: None,
            },
            &me,
        );
        select(&mut app, "1999999999999");
        reply_key(&mut app);
        assert!(!app.is_replying(), "{keymap:?}");
        assert!(status(&app).contains("was deleted"), "{}", status(&app));

        // A message still being sent has no id to reply to.
        open_and_post_unanswered(&mut app);
        let local = app
            .state()
            .unwrap()
            .messages()
            .iter()
            .find(|m| m.id.starts_with("local-"))
            .unwrap()
            .id
            .clone();
        select(&mut app, &local);
        reply_key(&mut app);
        assert!(!app.is_replying(), "{keymap:?}");
        assert!(
            status(&app).contains("not been sent yet"),
            "{}",
            status(&app)
        );
    }
}

/// A post sent but not yet answered (no tick has delivered the answer), so its message is still a `local-` one.
fn open_and_post_unanswered(app: &mut App) {
    compose(app, "still sending");
    send(app);
    send(app);
    assert_eq!(app.sending, 1);
}

// --- failures ------------------------------------------------------------------------------------------------------

fn refusal() -> Error {
    Error::Status {
        operation: "send message",
        status: 403,
        message: "not allowed here".into(),
    }
}

fn unconfirmed() -> Error {
    Error::Status {
        operation: "send message",
        status: 500,
        message: "boom".into(),
    }
}

#[test]
fn a_refused_post_gives_back_its_text_and_subject_and_a_refused_reply_its_text_and_target() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        run(&mut app, "subject Plan");
        compose(&mut app, "words");
        send(&mut app);
        poster.fail_next(refusal());
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(poster.posts().len(), 1);
        assert_eq!(app.editor.text(), "words", "{keymap:?}: the text is back");
        assert_eq!(
            app.pending_subject(),
            Some("Plan"),
            "{keymap:?}: so is the title"
        );
        assert!(status(&app).contains("send failed"), "{}", status(&app));
        assert!(
            app.state()
                .unwrap()
                .messages()
                .iter()
                .all(|m| !m.id.starts_with("local-")),
            "no provisional message is left"
        );
        // And it can be sent as it was.
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(poster.posts().len(), 2);
        assert_eq!(poster.posts()[1].subject.as_deref(), Some("Plan"));
        assert_eq!(poster.posts()[1].html, poster.posts()[0].html);

        // A reply.
        let post = id_of(&app, "Tagged and published");
        select(&mut app, &post);
        reply_key(&mut app);
        compose(&mut app, "re");
        poster.fail_next(refusal());
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(app.editor.text(), "re", "{keymap:?}");
        assert!(app.is_replying(), "{keymap:?}: the thread is back");
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(
            poster.posts().last().unwrap().thread.as_deref(),
            Some(post.as_str())
        );
    }
}

#[test]
fn an_unconfirmed_post_stays_in_the_transcript_and_gives_back_nothing() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        run(&mut app, "subject Plan");
        compose(&mut app, "maybe sent");
        send(&mut app);
        poster.fail_next(unconfirmed());
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(
            app.editor.text(),
            "",
            "{keymap:?}: not offered for an easy resend"
        );
        assert_eq!(app.pending_subject(), None, "{keymap:?}");
        assert!(
            status(&app).contains("send not confirmed"),
            "{}",
            status(&app)
        );
        let kept = app.state().unwrap().messages().iter().any(|m| {
            matches!(&m.delivery, Delivery::Failed(why)
                if why.contains("not confirmed") && why.contains("check the chat before sending again"))
        });
        assert!(kept, "{keymap:?}");
    }
}

// --- editing a post --------------------------------------------------------------------------------------------------

/// Post `text` (with the subject, if any) to the open channel and wait for it; returns its id.
fn post_mine(app: &mut App, subject: Option<&str>, text: &str) -> String {
    if let Some(subject) = subject {
        run(app, &format!("subject {subject}"));
    }
    compose(app, text);
    send(app);
    if app.sending == 0 {
        // The question was asked.
        send(app);
    }
    pump(app, |a| a.sending == 0);
    id_of(app, text)
}

#[test]
fn editing_your_own_post_repeats_its_subject_and_a_post_without_one_repeats_nothing() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        let titled = post_mine(&mut app, Some("Plan"), "my post");
        let plain = post_mine(&mut app, None, "my plain post");

        select(&mut app, &titled);
        edit_key(&mut app);
        assert!(app.is_editing(), "{keymap:?}: {}", status(&app));
        assert!(app.is_keeping_subject());
        assert!(
            frame(&mut app).contains("SUBJECT kept"),
            "{keymap:?}: the pill says so"
        );
        assert!(app.editor.text().contains("my post"));
        type_text(&mut app, " more");
        send(&mut app);
        pump(&mut app, |a| !a.is_editing());
        let edits = poster.post_edits();
        assert_eq!(edits.len(), 1, "{keymap:?}");
        assert_eq!(edits[0].channel, RELEASES);
        assert_eq!(edits[0].id, titled);
        assert_eq!(edits[0].subject.as_deref(), Some("Plan"), "{keymap:?}");
        assert!(edits[0].html.contains("more"));
        // The demo behaves like the service: the title survives only because it was repeated.
        let kept = app
            .state()
            .unwrap()
            .messages()
            .iter()
            .find(|m| m.id == titled)
            .unwrap()
            .clone();
        assert!(kept.edited.is_some());

        select(&mut app, &plain);
        edit_key(&mut app);
        assert!(app.is_editing());
        assert!(!app.is_keeping_subject());
        assert!(!frame(&mut app).contains("SUBJECT kept"));
        type_text(&mut app, " too");
        send(&mut app);
        pump(&mut app, |a| !a.is_editing());
        assert_eq!(poster.post_edits()[1].subject, None, "{keymap:?}");
        assert!(
            poster.chat_edits.lock().unwrap().is_empty(),
            "never the chat call"
        );
    }
}

#[test]
fn a_reply_and_someone_elses_post_are_not_edited() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        // The demo's reply of yours.
        let mine = id_of(&app, "Great work, everyone");
        select(&mut app, &mine);
        edit_key(&mut app);
        assert!(!app.is_editing(), "{keymap:?}");
        assert!(
            status(&app).contains("editing a reply in a channel is not supported yet"),
            "{}",
            status(&app)
        );
        assert_eq!(app.focus, Focus::Messages);

        let theirs = id_of(&app, "Tagged and published");
        select(&mut app, &theirs);
        edit_key(&mut app);
        assert!(!app.is_editing());
        assert!(
            status(&app).contains("you can only edit your own messages"),
            "{}",
            status(&app)
        );
        assert!(poster.post_edits().is_empty(), "{keymap:?}");

        // Up in an empty box (standard) picks your last message, which here is the reply.
        if keymap == Keymap::Standard {
            app.focus = Focus::Editor;
            app.editor.clear();
            press(&mut app, KeyCode::Up, KeyModifiers::NONE);
            assert!(!app.is_editing());
            assert!(status(&app).contains("a reply"), "{}", status(&app));
        }
    }
}

#[test]
fn a_failed_edit_of_a_post_is_said_and_keeps_the_edit_open() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        let mine = post_mine(&mut app, Some("Plan"), "my post");
        select(&mut app, &mine);
        edit_key(&mut app);
        type_text(&mut app, "!");
        poster.fail_next(refusal());
        send(&mut app);
        pump(&mut app, |a| {
            a.status
                .as_ref()
                .is_some_and(|(m, _, _)| m.contains("edit failed"))
        });
        assert!(app.is_editing(), "{keymap:?}");
        assert_eq!(poster.post_edits().len(), 1);
        assert!(status(&app).contains("edit failed"), "{}", status(&app));
    }
}

// --- what stays as it was ---------------------------------------------------------------------------------------

#[test]
fn chats_send_reply_and_edit_with_the_chat_calls_and_ask_nothing() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        app.select_chat("demo-ada");
        pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
        compose(&mut app, "hello ada");
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(
            poster.sent.lock().unwrap().clone(),
            [("demo-ada".to_string(), None)],
            "{keymap:?}: sent at once, no question"
        );
        assert!(poster.posts().is_empty());
        assert!(!status(&app).contains("Everyone in the channel"));

        // A reply quotes the message.
        let other = app
            .state()
            .unwrap()
            .messages()
            .iter()
            .find(|m| !m.system && m.from_id.as_deref() != Some(app.me_id()))
            .unwrap()
            .id
            .clone();
        select(&mut app, &other);
        reply_key(&mut app);
        assert!(app.is_replying());
        assert!(
            !frame(&mut app).contains("REPLYING IN THREAD"),
            "{keymap:?}"
        );
        compose(&mut app, "a quote reply");
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(
            poster.sent.lock().unwrap()[1].1.as_deref(),
            Some(other.as_str())
        );

        // An edit uses the chat call.
        let mine = id_of(&app, "hello ada");
        select(&mut app, &mine);
        edit_key(&mut app);
        assert!(app.is_editing(), "{keymap:?}");
        assert!(!app.is_keeping_subject());
        type_text(&mut app, "!");
        send(&mut app);
        pump(&mut app, |a| !a.is_editing());
        assert_eq!(poster.chat_edits.lock().unwrap().clone(), [mine]);
        assert!(poster.post_edits().is_empty(), "{keymap:?}");
    }
}

#[test]
fn a_service_that_cannot_post_keeps_the_refusals_and_the_box_says_so() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app_over(keymap, DemoBackend::without_posting());
        open(&mut app, RELEASES);
        assert!(!app.can_post_here());
        assert!(
            frame(&mut app).contains("Posting in channels is not available yet"),
            "{keymap:?}"
        );
        compose(&mut app, "no");
        send(&mut app);
        assert!(
            status(&app).contains("posting in channels is not available yet"),
            "{keymap:?}: {}",
            status(&app)
        );
        assert!(!status(&app).contains("Everyone in the channel"));
        assert_eq!(app.editor.text(), "no");
        assert!(poster.posts().is_empty());
        let mine = id_of(&app, "Great work, everyone");
        select(&mut app, &mine);
        reply_key(&mut app);
        assert!(
            status(&app).contains("replying in channels is not available"),
            "{}",
            status(&app)
        );
        assert!(!app.is_replying());
        edit_key(&mut app);
        assert!(
            status(&app).contains("editing in channels is not available"),
            "{}",
            status(&app)
        );
        run(&mut app, "subject Plan");
        assert!(
            status(&app).contains("posting in channels is not available"),
            "{}",
            status(&app)
        );
        assert_eq!(app.pending_subject(), None);
        // No hints for what cannot be done.
        app.focus = Focus::Messages;
        let hints = frame(&mut app);
        assert!(!hints.contains("reply in thread"), "{keymap:?}");
    }
}

#[test]
fn the_message_box_hints_and_placeholder_of_a_channel_that_takes_posts() {
    for keymap in BOTH {
        let (mut app, _) = poster_app(keymap);
        open(&mut app, RELEASES);
        app.focus = Focus::Messages;
        app.editor.clear();
        let text = frame(&mut app);
        assert!(
            !text.contains("Posting in channels is not available"),
            "{keymap:?}\n{text}"
        );
        assert!(text.contains("Message"), "{keymap:?}");
        assert!(
            text.contains("reply in thread"),
            "{keymap:?}: the footer\n{text}"
        );
        assert!(text.contains("edit"), "{keymap:?}");
    }
}

// --- what is drawn while it is sent --------------------------------------------------------------------------------

#[test]
fn a_pending_post_shows_its_title_and_a_pending_reply_says_which_post_it_answers() {
    for keymap in BOTH {
        let (mut app, _) = poster_app(keymap);
        open(&mut app, RELEASES);
        run(&mut app, "subject Weekly plan");
        compose(&mut app, "the plan body");
        send(&mut app);
        send(&mut app);
        assert_eq!(app.sending, 1, "{keymap:?}: not answered yet");
        let text = frame(&mut app);
        assert!(text.contains("Weekly plan"), "{keymap:?}\n{text}");
        assert!(text.contains("the plan body"), "{keymap:?}");
        pump(&mut app, |a| a.sending == 0);

        let marker = "↳ reply to Margaret Hamilton: Release 4.2 is out";
        let before = frame(&mut app).matches(marker).count();
        let post = id_of(&app, "Tagged and published");
        select(&mut app, &post);
        reply_key(&mut app);
        compose(&mut app, "pending reply body");
        send(&mut app);
        assert_eq!(app.sending, 1);
        let text = frame(&mut app);
        assert!(text.contains("pending reply body"), "{keymap:?}\n{text}");
        assert_eq!(
            text.matches(marker).count(),
            before + 1,
            "{keymap:?}: the pending reply names its post\n{text}"
        );
    }
}

// --- mentions ------------------------------------------------------------------------------------------------------

/// A message in the Releases channel from `who` (an id, or none for a service), `ago` minutes old.
fn planted_msg(id: u64, ago: i64, who: Option<&str>, name: &str) -> Msg {
    let t = chrono::Utc::now() - chrono::Duration::minutes(ago);
    Msg {
        root_id: None,
        id: id.to_string(),
        created: t,
        modified: t,
        edited: None,
        from_id: who.map(String::from),
        from_name: name.to_string(),
        system: false,
        deleted: false,
        html: true,
        body: "<p>planted</p>".into(),
        subject: None,
        attachments: vec![],
        reactions: vec![],
        mentions: vec![],
        delivery: Delivery::Sent,
    }
}

/// The names the open box offers for what has been typed, in order.
fn offered(app: &App) -> Vec<String> {
    app.mention_suggestions()
        .map(|s| s.people.into_iter().map(|p| p.name).collect())
        .unwrap_or_default()
}

/// Send what is in the box, answering the first-post question.
fn send_confirmed(app: &mut App) {
    send(app);
    if app.sending == 0 {
        send(app);
    }
    pump(app, |a| a.sending == 0);
}

const RELEASE_POSTERS: [&str; 3] = ["Ada Lovelace", "Margaret Hamilton", "Linus Torvalds"];

#[test]
fn an_at_in_a_channel_offers_the_people_who_posted_there_newest_first_and_never_you() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        compose(&mut app, "hello @");
        assert_eq!(offered(&app), RELEASE_POSTERS, "{keymap:?}");
        let ids: Vec<String> = app
            .mention_suggestions()
            .unwrap()
            .people
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, ["u-ada", "u-margaret", "u-linus"], "{keymap:?}");
        assert!(!ids.iter().any(|i| i == app.me_id()), "not yourself");
        assert!(
            frame(&mut app).contains("Margaret Hamilton"),
            "{keymap:?}: the list is drawn above the box"
        );
        // Typing narrows it, on any word of a name.
        type_text(&mut app, "ha");
        assert_eq!(offered(&app), ["Margaret Hamilton"], "{keymap:?}");
        // Somebody who has not posted in this channel is not offered, however well the name matches.
        app.editor.clear();
        type_text(&mut app, "@grace");
        assert!(offered(&app).is_empty(), "{keymap:?}");
        assert!(
            poster.members_asked.lock().unwrap().is_empty(),
            "{keymap:?}: a channel's people are never asked for"
        );
    }
}

#[test]
fn a_deleted_a_system_an_unnamed_and_a_serviced_message_give_no_one_to_mention() {
    for keymap in BOTH {
        let demo = DemoBackend::instant();
        let (mut app, poster) = poster_app_over(keymap, demo);
        let mut deleted = planted_msg(9_000_001, 5, Some("u-grace"), "Grace Hopper");
        deleted.deleted = true;
        let mut event = planted_msg(9_000_002, 4, Some("u-katherine"), "Katherine Johnson");
        event.system = true;
        let unnamed = planted_msg(9_000_003, 3, Some("u-alan"), "  ");
        let service = planted_msg(9_000_004, 2, None, "Release Bot");
        // The newest message of the channel is yours: it adds nobody.
        let mine = planted_msg(9_000_005, 1, Some(app.me_id()), "John Titor");
        *poster.planted.lock().unwrap() = vec![deleted, event, unnamed, service, mine];
        open(&mut app, RELEASES);
        compose(&mut app, "@");
        assert_eq!(offered(&app), RELEASE_POSTERS, "{keymap:?}");
        // What the list and the send both start from has the same three, whatever either filters again.
        let ids: Vec<String> = app
            .mention_candidates()
            .iter()
            .map(|m| m.id.clone())
            .collect();
        assert_eq!(ids, ["u-ada", "u-margaret", "u-linus"], "{keymap:?}");
    }
}

#[test]
fn tab_or_enter_chooses_a_poster_and_writes_the_full_name_without_sending() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        compose(&mut app, "cc @lin");
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "cc @Linus Torvalds ", "{keymap:?}");
        assert!(app.mention_suggestions().is_none());
        app.editor.clear();
        type_text(&mut app, "@mar");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "@Margaret Hamilton ", "{keymap:?}");
        assert_eq!(app.sending, 0, "choosing is not sending");
        assert!(poster.posts().is_empty());
        // Down and Up move through the list.
        app.editor.clear();
        type_text(&mut app, "@");
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "@Margaret Hamilton ", "{keymap:?}");
    }
}

#[test]
fn a_post_a_reply_and_an_edit_carry_the_mentions_typed_in_them() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        let margaret = (0, "u-margaret".to_string(), "Margaret Hamilton".to_string());
        let both = vec![
            (0, "u-linus".to_string(), "Linus Torvalds".to_string()),
            (1, "u-ada".to_string(), "Ada Lovelace".to_string()),
        ];

        // A new post, mentioning one person.
        compose(&mut app, "@marg");
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        type_text(&mut app, "please review");
        send(&mut app);
        send(&mut app);
        assert_eq!(app.sending, 1);
        // Provisionally it shows the mention the way a chat's does: the name, with its `<at>` in the message.
        let pending = app
            .state()
            .unwrap()
            .messages()
            .iter()
            .find(|m| m.id.starts_with("local-"))
            .unwrap()
            .clone();
        assert!(
            pending.body.contains("<at id=\"0\">Margaret Hamilton</at>"),
            "{keymap:?}: {}",
            pending.body
        );
        assert_eq!(pending.mentions, ["u-margaret"]);
        let text = frame(&mut app);
        assert!(text.contains("@Margaret Hamilton"), "{keymap:?}\n{text}");
        pump(&mut app, |a| a.sending == 0);
        assert_eq!(poster.posts().len(), 1);
        assert!(
            poster.posts()[0]
                .html
                .contains("<at id=\"0\">Margaret Hamilton</at> please review")
        );
        assert_eq!(poster.posts()[0].thread, None);
        assert_eq!(
            poster.post_mentions.lock().unwrap()[0],
            vec![margaret.clone()]
        );

        // A reply to someone's post, mentioning two.
        let post = id_of(&app, "Tagged and published");
        select(&mut app, &post);
        reply_key(&mut app);
        compose(&mut app, "@linus");
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        type_text(&mut app, "and @ada");
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        type_text(&mut app, "thanks");
        send_confirmed(&mut app);
        let posts = poster.posts();
        assert_eq!(posts.len(), 2, "{keymap:?}");
        assert_eq!(posts[1].thread.as_deref(), Some(post.as_str()));
        assert_eq!(poster.post_mentions.lock().unwrap()[1], both, "{keymap:?}");

        // Editing the first post loads the mention back as typed, and saving sends it again.
        let mine = id_of(&app, "please review");
        select(&mut app, &mine);
        edit_key(&mut app);
        assert!(app.is_editing(), "{keymap:?}: {}", status(&app));
        let loaded = app.editor.text();
        assert!(
            loaded.contains("@Margaret Hamilton") && !loaded.contains('*') && !loaded.contains('<'),
            "{keymap:?}: {loaded}"
        );
        type_text(&mut app, " today");
        send(&mut app);
        pump(&mut app, |a| !a.is_editing());
        assert_eq!(poster.post_edits().len(), 1, "{keymap:?}");
        assert!(
            poster.post_edits()[0]
                .html
                .contains("<at id=\"0\">Margaret Hamilton</at>")
        );
        assert_eq!(poster.edit_mentions.lock().unwrap()[0], [margaret]);
        let edited = app
            .state()
            .unwrap()
            .messages()
            .iter()
            .find(|m| m.id == mine)
            .unwrap()
            .clone();
        assert_eq!(edited.mentions, ["u-margaret"], "{keymap:?}");

        // Taking the mention out of the text edits it to none.
        select(&mut app, &mine);
        edit_key(&mut app);
        app.editor.set_text("no one now");
        send(&mut app);
        pump(&mut app, |a| !a.is_editing());
        assert_eq!(poster.edit_mentions.lock().unwrap()[1], [], "{keymap:?}");
        assert!(poster.members_asked.lock().unwrap().is_empty());
    }
}

#[test]
fn an_at_name_that_is_not_a_poster_or_is_you_is_sent_as_plain_text() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        compose(&mut app, "hello @Grace Hopper and @John Titor and @Nobody");
        send_confirmed(&mut app);
        let html = &poster.posts()[0].html;
        assert!(
            html.contains("@Grace Hopper") && html.contains("@John Titor") && !html.contains("<at"),
            "{keymap:?}: {html}"
        );
        assert_eq!(poster.post_mentions.lock().unwrap()[0], [], "{keymap:?}");
    }
}

const TWIN_NOTE: &str = "two people are called Ada Lovelace: the mention was not sent as a mention";

#[test]
fn a_name_two_people_have_in_a_channel_is_left_as_text_and_the_person_is_told() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        *poster.planted.lock().unwrap() = vec![planted_msg(
            9_000_010,
            1,
            Some("u-ada-twin"),
            "Ada Lovelace",
        )];
        open(&mut app, RELEASES);
        compose(&mut app, "@ada");
        // Both are still offered, so the person can see why.
        let names = offered(&app);
        assert_eq!(
            names.iter().filter(|n| *n == "Ada Lovelace").count(),
            2,
            "{keymap:?}: {names:?}"
        );
        app.editor.clear();
        compose(&mut app, "hi @Ada Lovelace and @Margaret Hamilton thanks");
        send_confirmed(&mut app);
        let post = &poster.posts()[0];
        assert!(
            post.html
                .contains("@Ada Lovelace and <at id=\"0\">Margaret Hamilton</at>"),
            "{keymap:?}: {}",
            post.html
        );
        assert_eq!(
            poster.post_mentions.lock().unwrap()[0],
            [(0, "u-margaret".to_string(), "Margaret Hamilton".to_string())],
            "{keymap:?}: only the one nobody shares a name with"
        );
        assert_eq!(status(&app), TWIN_NOTE, "{keymap:?}");

        // Only the shared name typed: no mention at all. A send with no such name says nothing of it.
        compose(&mut app, "ping @Ada Lovelace");
        send_confirmed(&mut app);
        assert!(!poster.posts()[1].html.contains("<at"), "{keymap:?}");
        assert_eq!(status(&app), TWIN_NOTE);
        app.status = None;
        compose(&mut app, "no names here");
        send_confirmed(&mut app);
        assert!(
            !status(&app).contains("two people"),
            "{keymap:?}: {}",
            status(&app)
        );
    }
}

#[test]
fn a_name_only_one_person_has_in_a_channel_is_still_a_mention() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        compose(&mut app, "hi @Ada Lovelace thanks");
        send_confirmed(&mut app);
        assert!(
            poster.posts()[0]
                .html
                .contains("<at id=\"0\">Ada Lovelace</at>"),
            "{keymap:?}"
        );
        assert!(!status(&app).contains("two people"), "{}", status(&app));
    }
}

#[test]
fn a_name_two_people_have_in_a_chat_is_left_as_text_and_the_person_is_told() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        *poster.extra_in_platform.lock().unwrap() = vec![teams_client::Member {
            not_in_directory: false,
            id: "u-ada-twin".into(),
            name: "Ada Lovelace".into(),
        }];
        app.refresh_soon();
        pump(&mut app, |a| {
            a.chats
                .items()
                .iter()
                .any(|c| c.id == "demo-platform" && c.members.iter().any(|m| m.id == "u-ada-twin"))
        });
        app.select_chat("demo-platform");
        pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
        compose(&mut app, "@ada");
        assert_eq!(
            offered(&app)
                .iter()
                .filter(|n| *n == "Ada Lovelace")
                .count(),
            2,
            "{keymap:?}: both are offered"
        );
        app.editor.clear();
        compose(&mut app, "hi @Ada Lovelace and @Linus Torvalds thanks");
        send(&mut app);
        assert_eq!(status(&app), TWIN_NOTE, "{keymap:?}");
        pump(&mut app, |a| a.sending == 0);
        let (html, mentions) = poster.sent_with.lock().unwrap()[0].clone();
        assert!(
            html.contains("@Ada Lovelace and <at id=\"0\">Linus Torvalds</at>"),
            "{keymap:?}: {html}"
        );
        assert_eq!(
            mentions,
            [(0, "u-linus".to_string(), "Linus Torvalds".to_string())],
            "{keymap:?}"
        );
        // With the twin gone, the name is Ada's again.
        poster.extra_in_platform.lock().unwrap().clear();
        app.refresh_soon();
        pump(&mut app, |a| {
            a.chats
                .items()
                .iter()
                .any(|c| c.id == "demo-platform" && c.members.iter().all(|m| m.id != "u-ada-twin"))
        });
        compose(&mut app, "hi @Ada Lovelace thanks");
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        assert!(
            poster.sent_with.lock().unwrap()[1]
                .0
                .contains("<at id=\"0\">Ada Lovelace</at>"),
            "{keymap:?}"
        );
    }
}

#[test]
fn a_names_stray_whitespace_and_marker_characters_never_reach_what_is_typed_or_sent() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        *poster.planted.lock().unwrap() = vec![planted_msg(
            9_000_011,
            1,
            Some("u-grace"),
            "Grace\n\t Hopper\u{F0000}\u{F0001}",
        )];
        open(&mut app, RELEASES);
        compose(&mut app, "@grace");
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "@Grace Hopper ", "{keymap:?}");
        type_text(&mut app, "hello");
        send_confirmed(&mut app);
        let html = &poster.posts()[0].html;
        assert!(
            html.contains("<at id=\"0\">Grace Hopper</at> hello"),
            "{keymap:?}: {html}"
        );
        assert!(
            html.chars()
                .all(|c| !('\u{F0000}'..='\u{F0001}').contains(&c)),
            "{keymap:?}: no marker in {html:?}"
        );
    }
}

#[test]
fn a_chat_members_stray_whitespace_and_marker_characters_never_reach_what_is_typed_or_sent() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        *poster.extra_in_platform.lock().unwrap() = vec![teams_client::Member {
            not_in_directory: false,
            id: "u-grace".into(),
            name: " Grace\n\t Hopper\u{F0000}\u{F0001} ".into(),
        }];
        app.refresh_soon();
        pump(&mut app, |a| {
            a.chats
                .items()
                .iter()
                .any(|c| c.id == "demo-platform" && c.members.iter().any(|m| m.id == "u-grace"))
        });
        app.select_chat("demo-platform");
        pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
        compose(&mut app, "@grace");
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.editor.text(), "@Grace Hopper ", "{keymap:?}");
        type_text(&mut app, "hello");
        send(&mut app);
        pump(&mut app, |a| a.sending == 0);
        let (html, mentions) = poster.sent_with.lock().unwrap()[0].clone();
        assert!(
            html.contains("<at id=\"0\">Grace Hopper</at> hello"),
            "{keymap:?}: {html}"
        );
        assert_eq!(
            mentions,
            [(0, "u-grace".to_string(), "Grace Hopper".to_string())],
            "{keymap:?}"
        );
    }
}

#[test]
fn a_mention_marker_typed_or_pasted_into_the_box_is_not_a_mention() {
    for keymap in BOTH {
        let (mut app, poster) = poster_app(keymap);
        open(&mut app, RELEASES);
        compose(&mut app, "hi @Margaret Hamilton thanks");
        app.on_paste(" \u{F0000}0\u{F0001} more");
        send_confirmed(&mut app);
        let html = &poster.posts()[0].html;
        assert_eq!(html.matches("<at").count(), 1, "{keymap:?}: {html}");
        assert!(
            html.chars()
                .all(|c| !('\u{F0000}'..='\u{F0001}').contains(&c)),
            "{keymap:?}: {html:?}"
        );
    }
}

#[test]
fn a_chats_completion_and_mentions_are_unchanged() {
    for keymap in BOTH {
        let (mut app, _poster) = poster_app(keymap);
        app.select_chat("demo-platform");
        pump(&mut app, |a| a.state().is_some_and(|s| s.loaded));
        compose(&mut app, "@");
        let names = offered(&app);
        assert_eq!(names.len(), 3, "{keymap:?}: {names:?}");
        let mut sorted = names.clone();
        sorted.sort_by_key(|n| n.to_lowercase());
        assert_eq!(names, sorted, "{keymap:?}: a chat's list is by name");
        assert!(!names.iter().any(|n| n == "John Titor"));
    }
}
