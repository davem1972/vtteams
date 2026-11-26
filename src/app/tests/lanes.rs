//! What a person is waiting on does not queue behind background calls: the app on the real two-lane pool, with every
//! thread of one lane held by a call that does not return.
//!
//! These tests use real threads, so they wait for answers with generous deadlines and never for a fixed time; a held
//! lane is released through a gate the test opens, not a sleep.

use super::*;
use crate::workers::testing::{BACKGROUND_LANE, Gate, PATIENCE, occupy};
use std::sync::atomic::{AtomicBool, Ordering};

/// An app on the real pool over `backend`, in the standard keymap with the demo's first conversation open and nothing
/// left in flight.
fn real_app_on(backend: Arc<dyn Backend>) -> App {
    let scratch = crate::testdir::TestDir::reserve("test-images-lanes");
    let desktop = Desktop {
        clipboard: Arc::new(clipboard::Memory::default()),
        opener: Arc::new(opener::Recorder::default()),
        image_dir: Ok(scratch.to_path_buf()),
        open_dwell: Duration::ZERO,
    };
    let mut app = App::with_parts(Arc::clone(&backend), Workers::new(&backend), desktop);
    app.scratch = Some(scratch);
    app.set_keymap(Keymap::Standard);
    app.start();
    until(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.select_chat("demo-ada");
    until(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    until(&mut app, App::settled);
    app
}

fn real_app() -> App {
    real_app_on(Arc::new(DemoBackend::instant()))
}

/// Tick (answers arrive through ticks) until `done`, or fail after [`PATIENCE`].
fn until(app: &mut App, done: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        app.on_tick();
        if done(app) {
            return;
        }
        assert!(Instant::now() < deadline, "the condition was never reached");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn send_a_message(app: &mut App, text: &str) {
    app.focus = Focus::Editor;
    type_text(app, text);
    press(app, KeyCode::Enter, KeyModifiers::NONE);
}

#[test]
fn a_send_is_answered_while_every_background_thread_is_held() {
    let mut app = real_app();
    let gate = Arc::new(Gate::default());
    occupy(&app.workers, Lane::Background, BACKGROUND_LANE, &gate);
    // A poll goes into the background queue, behind the eight.
    app.refresh_chats();
    assert!(app.chats.is_fetching());

    send_a_message(&mut app, "hello from the interactive lane");
    assert_eq!(app.sending, 1);
    let asked = Instant::now();
    until(&mut app, |a| a.sending == 0);
    eprintln!("send answered in {:?} with the pool held", asked.elapsed());

    let sent = last_message(&app);
    assert!(sent.body.contains("hello from the interactive lane"));
    assert_eq!(
        sent.delivery,
        Delivery::Sent,
        "the answer arrived and was applied"
    );
    assert!(
        app.chats.is_fetching(),
        "the poll is still waiting behind the held threads, so the send did not get its answer by sharing their queue"
    );
    assert!(!app.settled(), "the held jobs and the poll are in flight");

    gate.open();
    until(&mut app, App::settled);
    assert!(
        !app.chats.is_fetching(),
        "the poll ran once a thread was free"
    );
}

/// What the review measured: how long a send takes to be answered when the background pool is occupied for 250 ms.
/// `cargo test --release send_latency -- --ignored --nocapture`
#[test]
#[ignore = "a measurement"]
fn send_latency_with_the_background_pool_occupied_for_a_quarter_of_a_second() {
    let mut app = real_app();
    let gate = Arc::new(Gate::default());
    occupy(&app.workers, Lane::Background, BACKGROUND_LANE, &gate);
    let release = Arc::clone(&gate);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        release.open();
    });
    send_a_message(&mut app, "measured");
    let asked = Instant::now();
    until(&mut app, |a| a.sending == 0);
    eprintln!("send latency: {:?}", asked.elapsed());
}

#[test]
fn opening_a_chat_loads_its_first_page_while_every_background_thread_is_held() {
    let mut app = real_app();
    let other = app
        .chats
        .items()
        .iter()
        .map(|c| c.id.clone())
        .find(|id| Some(id.as_str()) != app.current.as_deref())
        .expect("the demo has another chat");
    let gate = Arc::new(Gate::default());
    occupy(&app.workers, Lane::Background, BACKGROUND_LANE, &gate);
    app.select_chat(&other);
    until(&mut app, |a| a.state().is_some_and(|s| s.loaded));
    gate.open();
}

#[test]
fn a_new_chat_is_found_and_opened_while_every_background_thread_is_held() {
    let mut app = real_app();
    let gate = Arc::new(Gate::default());
    occupy(&app.workers, Lane::Background, BACKGROUND_LANE, &gate);
    app.find_people("ada");
    assert!(app.lookups > 0);
    until(&mut app, |a| a.lookups == 0);
    gate.open();
}

#[test]
fn a_favourite_is_saved_while_every_background_thread_is_held() {
    let mut app = real_app();
    let gate = Arc::new(Gate::default());
    occupy(&app.workers, Lane::Background, BACKGROUND_LANE, &gate);
    assert_eq!(app.workers.in_flight_in(Lane::Interactive), 0);
    app.toggle_favorite();
    // The interactive lane has it done while the background threads are still held (polls may queue behind them
    // meanwhile). Whether the job is still running at this instant is a race, so it is not asserted: the proof that it
    // went to the interactive lane is that its answer is taken in below although every background thread is held.
    until(&mut app, |a| a.workers.in_flight_in(Lane::Interactive) == 0);
    // The answer was taken in (the job sends it before it is counted as finished): the chat can be changed again.
    app.on_tick();
    app.status = None;
    app.toggle_favorite();
    assert!(
        app.status
            .as_ref()
            .is_none_or(|(text, ..)| !text.contains("still being saved")),
        "{:?}",
        app.status
    );
    gate.open();
}

#[test]
fn a_background_poll_does_not_wait_behind_a_held_interactive_lane() {
    let mut app = real_app();
    let gate = Arc::new(Gate::default());
    occupy(&app.workers, Lane::Interactive, 2, &gate);
    app.refresh_chats();
    until(&mut app, |a| !a.chats.is_fetching());
    gate.open();
}

#[test]
fn signing_in_again_never_takes_a_thread_of_the_interactive_lane() {
    let backend = Lapsed::new(true, true);
    let mut app = real_app_on(backend.clone());
    backend.lapse();
    let gate = Arc::new(Gate::default());
    // Both interactive threads are held, so a sign-in sent there would never run.
    occupy(&app.workers, Lane::Interactive, 2, &gate);
    // The first press on a sign-in that looks fine only asks for a second.
    app.sign_in_again();
    app.sign_in_again();
    let signed_in = |b: &Lapsed| b.signed_in.load(Ordering::SeqCst);
    let deadline = Instant::now() + PATIENCE;
    while !signed_in(&backend) {
        app.on_tick();
        assert!(
            Instant::now() < deadline,
            "the sign-in did not run: it queued behind the interactive lane"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    gate.open();
}

#[test]
fn a_panic_in_either_lane_becomes_an_error_and_the_lane_keeps_working() {
    let mut app = real_app();
    for lane in [Lane::Interactive, Lane::Background] {
        app.status = None;
        app.spawn_call_in(
            lane,
            |_| -> Result<(), Error> { panic!("a call that fails") },
            |result| Ev::Notice {
                text: match result {
                    Err(Error::Unexpected { operation, .. }) => format!("contained: {operation}"),
                    other => format!("unexpected answer: {other:?}"),
                },
                error: true,
            },
        );
        until(&mut app, |a| a.status.is_some());
        let said = app.status.as_ref().map(|(text, ..)| text.clone());
        assert_eq!(
            said.as_deref(),
            Some("contained: background task"),
            "{lane:?}"
        );

        // The same lane still runs the next call.
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        app.status = None;
        app.spawn_call_in(
            lane,
            move |_| {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            },
            |_: Result<(), Error>| Ev::Notice {
                text: "next call ran".into(),
                error: false,
            },
        );
        until(&mut app, |a| a.status.is_some());
        assert!(ran.load(Ordering::SeqCst), "{lane:?}");
    }
}

#[test]
fn settled_waits_for_a_held_job_in_either_lane() {
    let mut app = real_app();
    assert!(app.settled());
    for lane in [Lane::Interactive, Lane::Background] {
        let gate = Arc::new(Gate::default());
        occupy(&app.workers, lane, 1, &gate);
        app.on_tick();
        assert!(!app.settled(), "{lane:?} has a job in flight");
        gate.open();
        until(&mut app, App::settled);
    }
}

#[test]
fn what_a_person_watches_loading_goes_to_the_interactive_lane_and_the_rest_does_not() {
    assert_eq!(Fetch::Newest { quiet: false }.lane(), Lane::Interactive);
    assert_eq!(Fetch::Older { automatic: false }.lane(), Lane::Interactive);
    assert_eq!(Fetch::Newest { quiet: true }.lane(), Lane::Background);
    assert_eq!(Fetch::Older { automatic: true }.lane(), Lane::Background);
    assert_eq!(Fetch::Seek.lane(), Lane::Background);
}
