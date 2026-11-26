//! Pictures drawn with the terminal's graphics protocol, driven through whole frames against the demo's pictures.
//!
//! A real terminal cannot be used here, so the frames go to ratatui's test backend, whose screen only changes where
//! ratatui writes (which is what makes cells left alone visible), and what would be written to the terminal for the
//! pictures is recorded in a buffer.

use super::*;
use crate::gfx::present::Presenter;
use crate::gfx::probe::Facts;
use crate::gfx::{CellPx, ImageChoice, Protocol};
use ratatui::backend::TestBackend;
use ratatui::buffer::CellDiffOption;

const CELL: CellPx = CellPx { w: 10, h: 20 };

fn facts(cell: Option<CellPx>) -> Facts {
    Facts {
        cell,
        why: "a test",
        ..Facts::NONE
    }
}

/// A terminal that shows what ratatui writes, and the pictures that would be written over it.
struct Screen {
    terminal: ratatui::Terminal<TestBackend>,
    pictures: Presenter,
    /// The cells the last frame asked ratatui to leave alone.
    skipped: Vec<(u16, u16)>,
}

impl Screen {
    fn new(width: u16, height: u16) -> Self {
        Self {
            terminal: ratatui::Terminal::new(TestBackend::new(width, height)).unwrap(),
            pictures: Presenter::default(),
            skipped: Vec::new(),
        }
    }

    /// One frame: what ratatui draws, then what the presenter writes for the pictures. Returns the latter as text.
    fn frame(&mut self, app: &mut App) -> String {
        let done = self.terminal.draw(|f| crate::ui::render(f, app)).unwrap();
        let area = done.area;
        self.skipped = (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| done.buffer[(x, y)].diff_option == CellDiffOption::Skip)
            .collect();
        let mut out = Vec::new();
        app.present_graphics(&mut self.pictures, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// The text of a rectangle of the screen as the terminal now has it.
    fn text(&self, rect: Rect) -> String {
        let buf = self.terminal.backend().buffer();
        (rect.y..rect.bottom())
            .map(|y| {
                (rect.x..rect.right())
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// An app on the demo's chat with a picture in it, set to draw pictures with `choice` where the cell is `cell`.
fn picture_app(choice: ImageChoice, cell: Option<CellPx>) -> App {
    let (mut app, _) = App::for_test(Arc::new(DemoBackend::instant()));
    app.set_keymap(Keymap::Vim);
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.set_images(choice, facts(cell));
    app.select_chat("demo-ada");
    app
}

/// Frames and ticks until the picture has arrived (and, with graphics, been encoded). Everything written on the way
/// is returned.
fn until_the_picture_is_up(app: &mut App, screen: &mut Screen) -> String {
    let mut written = String::new();
    for _ in 0..60 {
        written.push_str(&screen.frame(app));
        app.on_tick();
        let arrived = app
            .state()
            .is_some_and(|s| !s.transcript.image_rows.is_empty())
            && app
                .images
                .store()
                .map
                .values()
                .any(|s| matches!(s, ImgState::Ready(_)));
        if arrived && (app.graphics().is_none() || !app.gfx_plan().is_empty()) {
            // One more frame, so that what the last tick changed is drawn.
            written.push_str(&screen.frame(app));
            return written;
        }
    }
    panic!("the picture never came up");
}

const SIXEL_START: &str = "\x1bP9;1;0q";

#[test]
fn a_ready_picture_wholly_on_screen_is_drawn_with_the_protocol_over_cells_ratatui_leaves_alone() {
    let mut app = picture_app(ImageChoice::Protocol(Protocol::Sixel), Some(CELL));
    let mut screen = Screen::new(140, 60);
    let art_only = {
        // The frame before the picture has arrived: block art would not be there either, only the placeholder.
        screen.frame(&mut app)
    };
    assert_eq!(art_only, "", "nothing is written while there is no picture");
    let written = until_the_picture_is_up(&mut app, &mut screen);
    assert_eq!(
        written.matches(SIXEL_START).count(),
        1,
        "one picture, written once"
    );
    let plan = app.gfx_plan();
    assert_eq!(plan.len(), 1);
    let rect = plan[0].rect;
    let transcript = app.view.geometry.transcript;
    assert!(
        rect.x >= transcript.x
            && rect.right() <= transcript.right()
            && rect.y >= transcript.y
            && rect.bottom() <= transcript.bottom(),
        "{rect:?} inside {transcript:?}"
    );
    // It is the rectangle the block art takes: the same number of cells, so nothing moves when one replaces the other.
    let (cols, rows) = crate::imgart::art_cells(
        match app.images.store().map.get("https://demo.invalid/plasma/1") {
            Some(ImgState::Ready(img)) => img,
            _ => panic!("the demo picture"),
        },
        usize::from(transcript.width) - crate::transcript::GUTTER - 1,
    );
    assert_eq!(
        (usize::from(rect.width), usize::from(rect.height)),
        (cols, rows)
    );
    assert_eq!(
        screen.skipped.len(),
        usize::from(rect.width) * usize::from(rect.height),
        "exactly the picture's cells are left alone"
    );
    assert!(
        screen
            .skipped
            .iter()
            .all(|&(x, y)| rect.contains((x, y).into()))
    );
    // It was written where it goes, with the cursor saved around it.
    let cup = format!("\x1b[{};{}H{SIXEL_START}", rect.y + 1, rect.x + 1);
    assert!(written.contains(&cup), "positioned at its top left");
    assert!(written.contains("\x1b7") && written.ends_with("\x1b8"));
}

#[test]
fn a_picture_that_has_not_moved_is_not_written_again_frame_after_frame() {
    let mut app = picture_app(ImageChoice::Protocol(Protocol::Sixel), Some(CELL));
    let mut screen = Screen::new(140, 60);
    until_the_picture_is_up(&mut app, &mut screen);
    for _ in 0..5 {
        app.on_tick();
        assert_eq!(screen.frame(&mut app), "", "nothing to write");
    }
    assert_eq!(screen.pictures.showing(), 1);
}

#[test]
fn the_cells_a_picture_covers_keep_what_they_showed_while_it_is_up() {
    let mut app = picture_app(ImageChoice::Protocol(Protocol::Sixel), Some(CELL));
    let mut screen = Screen::new(140, 60);
    // First as block art: this is what the screen shows in the rectangle before and after.
    let mut art_app = picture_app(ImageChoice::Art(crate::imgart::ImgMode::Blocks), None);
    let mut art_screen = Screen::new(140, 60);
    until_the_picture_is_up(&mut art_app, &mut art_screen);
    until_the_picture_is_up(&mut app, &mut screen);
    let rect = app.gfx_plan()[0].rect;
    let art = art_screen.text(rect);
    assert!(
        art.chars().any(|c| c != ' '),
        "block art fills the rectangle"
    );
    // The screen the test backend shows under the picture is the art ratatui drew before the picture was ready.
    assert_eq!(
        screen.text(rect),
        art,
        "ratatui did not write over the picture's cells"
    );
}

#[test]
fn with_art_or_nothing_detected_nothing_is_ever_written_for_pictures() {
    for (choice, cell) in [
        (ImageChoice::Art(crate::imgart::ImgMode::Blocks), Some(CELL)),
        (ImageChoice::Art(crate::imgart::ImgMode::Octant), Some(CELL)),
        (ImageChoice::Auto, Some(CELL)),
        (ImageChoice::Protocol(Protocol::Sixel), None),
    ] {
        let mut app = picture_app(choice, cell);
        let mut screen = Screen::new(140, 60);
        let written = until_the_picture_is_up(&mut app, &mut screen);
        assert_eq!(written, "", "{choice:?}");
        assert!(app.gfx_plan().is_empty());
        assert!(
            screen.skipped.is_empty(),
            "no cell is left alone for a picture that is not there"
        );
    }
}

#[test]
fn auto_uses_what_was_detected_and_falls_back_to_blocks() {
    let mut app = picture_app(ImageChoice::Auto, Some(CELL));
    app.set_images(
        ImageChoice::Auto,
        Facts {
            detected: Some(Protocol::Sixel),
            ..facts(Some(CELL))
        },
    );
    let mut screen = Screen::new(140, 60);
    let written = until_the_picture_is_up(&mut app, &mut screen);
    assert!(written.contains(SIXEL_START), "a detected protocol is used");
    assert_eq!(app.images_label(), "auto (sixel)");
    // Detected, but the cell size is not known: the pictures stay block art, and the status says why.
    app.set_images(
        ImageChoice::Auto,
        Facts {
            detected: Some(Protocol::Sixel),
            ..facts(None)
        },
    );
    assert!(app.graphics().is_none());
    assert!(
        app.images_label().contains("cell size is not known"),
        "{}",
        app.images_label()
    );
}

#[test]
fn a_popup_takes_the_pictures_off_and_closing_it_puts_them_back() {
    let mut app = picture_app(ImageChoice::Protocol(Protocol::Sixel), Some(CELL));
    let mut screen = Screen::new(140, 60);
    until_the_picture_is_up(&mut app, &mut screen);
    assert_eq!(screen.pictures.showing(), 1);
    for overlay in [
        Overlay::Help(0),
        Overlay::Status { sel: 0 },
        Overlay::Search,
    ] {
        app.overlay = overlay.clone();
        let written = screen.frame(&mut app);
        assert!(app.gfx_plan().is_empty(), "no picture under {overlay:?}");
        assert!(!written.contains(SIXEL_START));
        assert!(screen.skipped.is_empty(), "the cells are ratatui's again");
        assert_eq!(screen.pictures.showing(), 0);
        app.overlay = Overlay::None;
        let written = screen.frame(&mut app);
        assert_eq!(
            written.matches(SIXEL_START).count(),
            1,
            "back after {overlay:?}"
        );
    }
}

#[test]
fn a_mention_list_over_the_transcript_takes_the_pictures_off() {
    let mut app = picture_app(ImageChoice::Protocol(Protocol::Sixel), Some(CELL));
    let mut screen = Screen::new(140, 60);
    until_the_picture_is_up(&mut app, &mut screen);
    app.on_key(key('i'));
    app.on_key(key('@'));
    assert!(
        app.mention_suggestions().is_some(),
        "the premise: typing @ offers people"
    );
    screen.frame(&mut app);
    assert!(app.gfx_plan().is_empty());
}

#[test]
fn switching_chat_or_resizing_clears_the_pictures_and_writes_them_again_where_they_belong() {
    let mut app = picture_app(ImageChoice::Protocol(Protocol::Kitty), Some(CELL));
    let mut screen = Screen::new(140, 60);
    let first = until_the_picture_is_up(&mut app, &mut screen);
    assert!(first.contains("a=t,t=d,f=100"), "sent to the terminal");
    assert!(first.contains("a=p,i="), "and placed");
    // Another chat: the picture's placement is deleted, and its data stays in the terminal.
    let other = app
        .chats
        .items()
        .iter()
        .map(|c| c.id.clone())
        .find(|id| id != "demo-ada")
        .unwrap();
    app.select_chat(&other);
    let gone = screen.frame(&mut app);
    assert!(
        gone.contains("a=d,d=i,i="),
        "the placement is taken away: {gone:?}"
    );
    assert!(!gone.contains("a=t"));
    assert_eq!(screen.pictures.showing(), 0);
    // Back again: placed without being sent again.
    app.select_chat("demo-ada");
    for _ in 0..4 {
        app.on_tick();
    }
    let back = screen.frame(&mut app);
    assert!(
        back.contains("a=p,i=") && !back.contains("a=t,t=d"),
        "{back:?}"
    );
    // A resized screen is written again at its new place.
    screen.terminal.backend_mut().resize(150, 60);
    let resized = screen.frame(&mut app);
    assert!(
        resized.contains("a=p,i="),
        "placed again after a resize: {resized:?}"
    );
}

#[test]
fn a_sixel_that_scrolls_away_is_repainted_by_ratatui_not_by_us() {
    let mut app = picture_app(ImageChoice::Protocol(Protocol::Sixel), Some(CELL));
    let mut screen = Screen::new(140, 60);
    until_the_picture_is_up(&mut app, &mut screen);
    let rect = app.gfx_plan()[0].rect;
    let other = app
        .chats
        .items()
        .iter()
        .map(|c| c.id.clone())
        .find(|id| id != "demo-ada")
        .unwrap();
    app.select_chat(&other);
    let written = screen.frame(&mut app);
    assert_eq!(written, "", "nothing needs to be sent to take a Sixel away");
    assert!(
        screen.skipped.is_empty(),
        "so ratatui paints those cells again"
    );
    assert_eq!(screen.pictures.showing(), 0);
    let _ = rect;
}

#[test]
fn encoding_happens_on_a_worker_after_the_frame_and_block_art_shows_until_then() {
    let mut app = picture_app(ImageChoice::Protocol(Protocol::Sixel), Some(CELL));
    let mut screen = Screen::new(140, 60);
    // Until the decoded picture is in the store the frames only wait for it.
    for _ in 0..60 {
        screen.frame(&mut app);
        assert_eq!(app.queued_encodings(), 0, "nothing to encode yet");
        app.on_tick();
        if app
            .images
            .store()
            .map
            .values()
            .any(|s| matches!(s, ImgState::Ready(_)))
        {
            break;
        }
    }
    app.on_tick();
    // The frame that first sees the decoded picture asks for the encoding: it does not make it, and draws block art.
    let written = screen.frame(&mut app);
    assert_eq!(app.queued_encodings(), 1, "asked for");
    assert_eq!(
        app.timelines(),
        1,
        "its way to the screen is being timed for the log"
    );
    assert!(app.gfx_plan().is_empty() && written.is_empty() && screen.skipped.is_empty());
    // While it waits the loop is told to look again soon, so that the answer is drawn within milliseconds of arriving.
    assert!(
        app.poll_wait() < std::time::Duration::from_millis(60),
        "{:?}",
        app.poll_wait()
    );
    // The tick starts it on a worker (inline in tests); the next tick reads the answer; the frame after shows it.
    app.on_tick();
    assert!(
        app.poll_wait() < std::time::Duration::from_millis(60),
        "in flight"
    );
    assert_eq!(app.queued_encodings(), 0, "handed to a worker");
    assert!(
        app.gfx_plan().is_empty(),
        "the plan only changes when a frame is drawn"
    );
    app.on_tick();
    assert!(
        app.poll_wait() >= std::time::Duration::from_millis(60),
        "idle again once the answer is in"
    );
    // The very next frame, with no other change, has the picture on the screen.
    assert!(screen.frame(&mut app).contains(SIXEL_START));
    assert_eq!(
        app.timelines(),
        0,
        "and the timing is done once it is written"
    );
}

#[test]
fn encoding_on_a_thread_of_its_own_reaches_the_screen_with_real_workers_too() {
    // The same, with the real pool and real threads: the encoding does not queue behind backend calls, and its answer
    // is drawn without anything else happening (no key, no event).
    let demo: Arc<dyn Backend> = Arc::new(DemoBackend::instant());
    let scratch = crate::testdir::TestDir::reserve("test-images-real-workers");
    let desktop = Desktop {
        clipboard: Arc::new(clipboard::Memory::default()),
        opener: Arc::new(opener::Recorder::default()),
        image_dir: Ok(scratch.to_path_buf()),
        open_dwell: Duration::ZERO,
    };
    let mut app = App::with_parts(Arc::clone(&demo), Workers::new(&demo), desktop);
    app.scratch = Some(scratch);
    app.set_keymap(Keymap::Vim);
    app.start();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !(app.chats.loaded() && app.me.is_some()) {
        assert!(Instant::now() < deadline, "the chats never loaded");
        app.on_tick();
        std::thread::sleep(Duration::from_millis(5));
    }
    app.set_images(ImageChoice::Protocol(Protocol::Sixel), facts(Some(CELL)));
    app.select_chat("demo-ada");
    let mut screen = Screen::new(140, 60);
    let mut written = String::new();
    while !written.contains(SIXEL_START) {
        assert!(
            Instant::now() < deadline,
            "the picture never reached the screen"
        );
        written.push_str(&screen.frame(&mut app));
        app.on_tick();
        std::thread::sleep(app.poll_wait());
    }
    assert_eq!(app.gfx_plan().len(), 1);
}

#[test]
fn the_images_command_and_key_choose_between_graphics_and_art() {
    let mut app = picture_app(ImageChoice::Auto, None);
    app.set_images(
        ImageChoice::Auto,
        Facts {
            detected: Some(Protocol::Sixel),
            ..facts(Some(CELL))
        },
    );
    assert!(app.graphics().is_some());
    app.run_command("images blocks");
    assert!(app.graphics().is_none());
    assert_eq!(app.img_mode, crate::imgart::ImgMode::Blocks);
    app.run_command("images kitty");
    assert_eq!(app.graphics().map(|g| g.protocol), Some(Protocol::Kitty));
    assert_eq!(app.status.as_ref().unwrap().0, "images: kitty");
    app.run_command("images auto");
    assert_eq!(app.graphics().map(|g| g.protocol), Some(Protocol::Sixel));
    app.run_command("images png");
    assert!(
        app.status
            .as_ref()
            .unwrap()
            .0
            .starts_with("usage: :images auto|sixel|kitty|iterm")
    );
    // The key goes graphics -> octant -> sextant -> blocks -> ascii -> off -> graphics.
    app.run_command("images auto");
    let mut seen = Vec::new();
    for _ in 0..6 {
        app.cycle_images();
        seen.push(app.images_label());
    }
    assert_eq!(
        seen.iter().map(String::as_str).collect::<Vec<_>>(),
        [
            "octant",
            "sextant 🬗",
            "blocks ▚",
            "ascii @#",
            "off",
            "auto (sixel)"
        ]
    );
    // Where the terminal has no graphics, the cycle is the one it always was.
    let mut plain = picture_app(ImageChoice::Auto, None);
    plain.cycle_images();
    assert_eq!(plain.images_label(), "ascii @#");
}

#[test]
fn a_protocol_in_the_settings_file_reaches_the_screen() {
    // The path a person takes: config.toml -> settings -> what start-up learns -> the app -> the frame.
    let file = crate::settings::FileValues::parse("images = \"sixel\"\ncell_pixels = \"10x20\"\n")
        .unwrap();
    let settings = crate::settings::resolve(&[], &|_| None, &file, "config.toml").unwrap();
    let learned = crate::gfx::probe::learn(
        settings.images,
        &crate::gfx::detect::TermEnv::default(),
        settings.cell_pixels,
        None,
        (140, 60),
        |_| panic!("a protocol chosen by name, with a cell size given, asks the terminal nothing"),
    );
    let (mut app, _) = App::for_test(Arc::new(DemoBackend::instant()));
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.set_images(settings.images, learned);
    app.select_chat("demo-ada");
    let mut screen = Screen::new(140, 60);
    let written = until_the_picture_is_up(&mut app, &mut screen);
    assert!(written.contains(SIXEL_START), "Sixel for a visible picture");
    // And a popup stops them, as above.
    app.overlay = Overlay::Help(0);
    assert!(!screen.frame(&mut app).contains(SIXEL_START));
}

// ---------------------------------------------------------------------------
// A picture partly inside the viewport is cut, not given up.
// ---------------------------------------------------------------------------

use crate::gfx::decoder::decode_sixel;

/// Worked out from the transcript's own numbers, not from the planner: where the open chat's picture is on the screen.
struct Where {
    /// First screen row and number of rows of the picture that are inside the free rows (0 rows when none).
    top: u16,
    rows: u16,
    /// Rows of the picture cut off above and below.
    cut_top: usize,
    cut_bottom: usize,
    /// The rows of the picture, in pixels, that a Sixel can draw of those (whole bands of six).
    sixel_rows: Option<std::ops::Range<u32>>,
}

fn where_is_the_picture(app: &App, picture_height: u32) -> Option<Where> {
    let st = app.state()?;
    let (rows, url) = st.transcript.image_rows.first()?.clone();
    let Some(ImgState::Ready(img)) = app.images.store().map.get(&url) else {
        return None;
    };
    let area = app.view.geometry.transcript;
    let art = crate::imgart::art_cells(img, usize::from(area.width) - crate::transcript::GUTTER - 1)
        .1 as i64;
    let height = i64::from(area.height);
    // The labels over the first and last row do not cut the picture.
    let (low, high) = (0, height);
    let top = rows.start as i64 - st.scroll as i64;
    let (inside_top, inside_bottom) = (top.max(low), (top + art).min(high));
    if inside_bottom <= inside_top {
        return None;
    }
    let ch = i64::from(CELL.h);
    let first = (((inside_top - top) * ch + 5) / 6 * 6) as u32;
    let end = {
        let px = (inside_bottom - top) * ch;
        if px >= i64::from(picture_height) {
            picture_height
        } else {
            (px / 6 * 6) as u32
        }
    };
    Some(Where {
        top: area.y + inside_top as u16,
        rows: (inside_bottom - inside_top) as u16,
        cut_top: (inside_top - top) as usize,
        cut_bottom: (top + art - inside_bottom) as usize,
        sixel_rows: (first < end).then_some(first..end),
    })
}

/// The Sixel in what a frame wrote, if it wrote one.
fn sixel_in(written: &str) -> Option<&str> {
    let start = written.find(SIXEL_START)?;
    let end = written[start..].find("\x1b\\")? + start + 2;
    Some(&written[start..end])
}

/// The demo's picture is 480 x 270 pixels, and a picture is never enlarged.
const PICTURE_HEIGHT: u32 = 270;

/// Resize the screen until the conversation has `rows` rows.
fn give_the_conversation(app: &mut App, screen: &mut Screen, rows: u16) {
    let width = screen.terminal.backend().buffer().area.width;
    for height in 14..80 {
        screen.terminal.backend_mut().resize(width, height);
        screen.frame(app);
        if app.view.geometry.transcript.height == rows {
            return;
        }
    }
    panic!("no screen height gives {rows} rows of conversation");
}

/// An app on the demo's picture with `extra` messages after it, so that the chat can be scrolled past the picture.
fn scrolled_app(choice: ImageChoice, rows: u16) -> (App, Screen) {
    let mut app = picture_app(choice, Some(CELL));
    let mut screen = Screen::new(140, 60);
    until_the_picture_is_up(&mut app, &mut screen);
    let id = app.current.clone().unwrap();
    app.states
        .get_mut(&id)
        .unwrap()
        .edit_messages(|m| m.extend((0..60).map(|n| msg(&format!("extra{n}"), 0))));
    give_the_conversation(&mut app, &mut screen, rows);
    screen.frame(&mut app);
    (app, screen)
}

/// Sweep the chat from its newest message up past the picture, a line at a time, and check every frame against
/// what the transcript says. Returns which kinds of frame were seen: (cut at the top, whole, cut at the bottom, both).
fn sweep_sixel(app: &mut App, screen: &mut Screen) -> (bool, bool, bool, bool) {
    let (mut top, mut whole, mut bottom, mut both) = (false, false, false, false);
    let mut previous: Option<(
        Rect,
        std::ops::Range<u32>,
        Vec<crate::gfx::plan::OverlayCell>,
    )> = None;
    for step in 0..400 {
        app.scroll_by(-1);
        let written = screen.frame(app);
        let expected = where_is_the_picture(app, PICTURE_HEIGHT);
        let plan = app.gfx_plan();
        match (
            expected.as_ref().and_then(|e| e.sixel_rows.clone()),
            plan.first(),
        ) {
            (Some(rows), Some(p)) => {
                let e = expected.as_ref().unwrap();
                assert_eq!(p.rows, rows, "step {step}: the rows of the picture");
                assert_eq!(
                    (p.rect.y, p.rect.height),
                    (e.top, e.rows),
                    "step {step}: the cells"
                );
                assert_eq!(
                    screen.skipped.len(),
                    usize::from(p.rect.width) * usize::from(p.rect.height),
                    "step {step}: exactly the cells drawn are left alone"
                );
                let changed =
                    previous.as_ref() != Some(&(p.rect, p.rows.clone(), p.overlay.clone()));
                assert_eq!(
                    !written.is_empty(),
                    changed,
                    "step {step}: written exactly when the slice changed"
                );
                if changed {
                    let sent = sixel_in(&written)
                        .unwrap_or_else(|| panic!("step {step}: no Sixel written"));
                    let part = decode_sixel(sent.as_bytes());
                    let full = decode_sixel(&p.image.stream);
                    let (from, to) = (rows.start as usize, rows.end as usize);
                    assert_eq!(
                        part.size,
                        (full.size.0, to - from),
                        "step {step}: the declared size"
                    );
                    let w = full.size.0;
                    assert_eq!(
                        part.pixels[..],
                        full.pixels[from * w..to * w],
                        "step {step}: the pixels are exactly those rows of the whole picture"
                    );
                }
                previous = Some((p.rect, p.rows.clone(), p.overlay.clone()));
                top |= e.cut_top > 0 && e.cut_bottom == 0;
                bottom |= e.cut_bottom > 0 && e.cut_top == 0;
                both |= e.cut_top > 0 && e.cut_bottom > 0;
                whole |= e.cut_top == 0 && e.cut_bottom == 0;
            }
            (None, None) => {
                assert!(
                    screen.skipped.is_empty(),
                    "step {step}: nothing is left alone"
                );
                if previous.take().is_some() {
                    assert!(
                        !written.contains(SIXEL_START),
                        "step {step}: a Sixel that left needs nothing written"
                    );
                }
            }
            (want, got) => panic!("step {step}: expected {want:?}, the plan has {got:?}"),
        }
    }
    (top, whole, bottom, both)
}

#[test]
fn a_sixel_picture_scrolling_through_the_viewport_is_cut_at_the_top_then_whole_then_cut_at_the_bottom()
 {
    let (mut app, mut screen) = scrolled_app(ImageChoice::Protocol(Protocol::Sixel), 19);
    let (top, whole, bottom, _) = sweep_sixel(&mut app, &mut screen);
    assert!(
        top && whole && bottom,
        "top {top}, whole {whole}, bottom {bottom}"
    );
}

#[test]
fn a_sixel_picture_taller_than_the_viewport_is_cut_at_both_ends() {
    // About eleven rows of conversation for a picture of seventeen.
    let (mut app, mut screen) = scrolled_app(ImageChoice::Protocol(Protocol::Sixel), 10);
    assert!(
        usize::from(app.view.geometry.transcript.height) < 17,
        "the premise: {} rows",
        app.view.geometry.transcript.height
    );
    let (top, whole, bottom, both) = sweep_sixel(&mut app, &mut screen);
    assert!(both, "cut at both ends at some point");
    assert!(
        top && bottom && !whole,
        "top {top}, whole {whole}, bottom {bottom}"
    );
}

#[test]
fn a_picture_that_exactly_fills_the_free_rows_is_drawn_whole() {
    // A conversation one row taller than the picture, scrolled back: the last row is the "latest" label's, and the
    // seventeen rows above it are exactly the picture's.
    let (mut app, mut screen) = scrolled_app(ImageChoice::Protocol(Protocol::Sixel), 18);
    for _ in 0..400 {
        app.scroll_by(-1);
        screen.frame(&mut app);
        let st = app.state().unwrap();
        if st.scroll == st.transcript.image_rows[0].0.start {
            break;
        }
    }
    let p = &app.gfx_plan()[0];
    assert_eq!(p.rect.height, 17, "all seventeen rows");
    assert_eq!(p.rows, 0..p.image.size.1, "all of the picture");
    assert_eq!(screen.skipped.len(), usize::from(p.rect.width) * 17);
    assert_eq!(screen.pictures.showing(), 1);
}
#[test]
fn a_whole_picture_is_written_as_the_stream_it_was_encoded_as() {
    let mut app = picture_app(ImageChoice::Protocol(Protocol::Sixel), Some(CELL));
    let mut screen = Screen::new(140, 60);
    let written = until_the_picture_is_up(&mut app, &mut screen);
    let p = &app.gfx_plan()[0];
    assert!(written.contains(std::str::from_utf8(&p.image.stream).unwrap()));
}

#[test]
fn a_partly_visible_picture_under_a_popup_or_mention_list_is_not_drawn() {
    let (mut app, mut screen) = scrolled_app(ImageChoice::Protocol(Protocol::Sixel), 19);
    // Scroll until it is cut at the top.
    for _ in 0..400 {
        app.scroll_by(-1);
        screen.frame(&mut app);
        if let Some(p) = app.gfx_plan().first()
            && p.rows.start > 0
        {
            break;
        }
    }
    assert!(
        app.gfx_plan().first().is_some_and(|p| p.rows.start > 0),
        "the premise: cut at the top"
    );
    app.overlay = Overlay::Help(0);
    let written = screen.frame(&mut app);
    assert!(app.gfx_plan().is_empty());
    assert!(screen.skipped.is_empty());
    assert!(!written.contains(SIXEL_START));
    app.overlay = Overlay::None;
    let written = screen.frame(&mut app);
    assert!(
        app.gfx_plan().first().is_some_and(|p| p.rows.start > 0),
        "back, cut as before"
    );
    assert!(written.contains(SIXEL_START));
    app.on_key(key('i'));
    app.on_key(key('@'));
    assert!(app.mention_suggestions().is_some());
    screen.frame(&mut app);
    assert!(
        app.gfx_plan().is_empty(),
        "no graphics under the mention list"
    );
}

#[test]
fn scrolling_one_line_changes_the_slice_and_standing_still_writes_nothing() {
    let (mut app, mut screen) = scrolled_app(ImageChoice::Protocol(Protocol::Sixel), 19);
    for _ in 0..400 {
        app.scroll_by(-1);
        screen.frame(&mut app);
        if app.gfx_plan().first().is_some_and(|p| p.rows.start > 0) {
            break;
        }
    }
    let before = (app.gfx_plan()[0].rect, app.gfx_plan()[0].rows.clone());
    for _ in 0..4 {
        assert_eq!(screen.frame(&mut app), "", "standing still");
    }
    app.scroll_by(-1);
    let written = screen.frame(&mut app);
    let after = (app.gfx_plan()[0].rect, app.gfx_plan()[0].rows.clone());
    assert_ne!(before, after, "the slice changed");
    assert_eq!(
        written.matches(SIXEL_START).count(),
        1,
        "and was written once"
    );
    assert_eq!(screen.frame(&mut app), "");
}

#[test]
fn a_cut_picture_that_leaves_the_viewport_is_taken_off() {
    let (mut app, mut screen) = scrolled_app(ImageChoice::Protocol(Protocol::Sixel), 19);
    // Up until it has been seen, then on until it is gone below.
    let mut seen = false;
    for _ in 0..600 {
        app.scroll_by(-1);
        let written = screen.frame(&mut app);
        if app.gfx_plan().is_empty() {
            if seen {
                assert_eq!(written, "", "a Sixel that is gone needs nothing written");
                assert_eq!(screen.pictures.showing(), 0);
                assert!(screen.skipped.is_empty(), "ratatui paints its cells again");
                return;
            }
        } else {
            seen = true;
        }
    }
    panic!("the picture never left");
}

#[test]
fn a_kitty_picture_is_cut_with_a_source_rectangle_and_nothing_is_sent_again() {
    let (mut app, mut screen) = scrolled_app(ImageChoice::Protocol(Protocol::Kitty), 19);
    let mut cuts = Vec::new();
    for _ in 0..400 {
        app.scroll_by(-1);
        let written = screen.frame(&mut app);
        assert!(
            !written.contains("a=t"),
            "the data is not sent again: {written:?}"
        );
        let Some(p) = app.gfx_plan().first() else {
            if !cuts.is_empty() {
                break;
            }
            continue;
        };
        let (height, width) = (p.image.size.1, p.image.size.0);
        let whole = p.rows == (0..height);
        if !written.is_empty() {
            let place = written.find("a=p,i=").expect("placed");
            let tail = &written[place..];
            if whole {
                assert!(
                    !tail.contains(",x="),
                    "a whole picture has no crop: {tail:?}"
                );
            } else {
                let crop = format!(
                    ",x=0,y={},w={width},h={}",
                    p.rows.start,
                    p.rows.end - p.rows.start
                );
                assert!(tail.contains(&crop), "{crop} in {tail:?}");
                cuts.push(p.rows.clone());
            }
        }
    }
    assert!(
        cuts.len() >= 2,
        "cut at both edges as it scrolled through: {cuts:?}"
    );
    assert!(cuts.iter().any(|r| r.start > 0), "a cut at the top");
    assert!(cuts.iter().any(|r| r.start == 0), "a cut at the bottom");
    // Kitty crops to the pixel row, not to a band.
    assert!(cuts.iter().any(|r| r.start % 6 != 0 || r.end % 6 != 0));
}

#[test]
fn iterm_cannot_crop_so_a_partly_visible_picture_stays_block_art_there() {
    let (mut app, mut screen) = scrolled_app(ImageChoice::Protocol(Protocol::Iterm), 19);
    let (mut partly, mut whole) = (0, 0);
    for _ in 0..400 {
        app.scroll_by(-1);
        let written = screen.frame(&mut app);
        match (
            where_is_the_picture(&app, PICTURE_HEIGHT),
            app.gfx_plan().first(),
        ) {
            (Some(w), None) if w.cut_top > 0 || w.cut_bottom > 0 => {
                partly += 1;
                assert!(screen.skipped.is_empty(), "block art is ratatui's");
                assert!(!written.contains("1337"));
            }
            (Some(_), Some(p)) => {
                whole += 1;
                assert_eq!(p.rows, 0..p.image.size.1);
            }
            _ => {}
        }
    }
    assert!(partly > 0 && whole > 0, "partly {partly}, whole {whole}");
}

// ---------------------------------------------------------------------------
// The labels over the transcript's first and last row do not cut a picture.
// ---------------------------------------------------------------------------

/// Resize the screen until the conversation is `width` columns wide.
fn give_the_width(app: &mut App, screen: &mut Screen, width: u16) {
    for columns in 80..200 {
        screen.terminal.backend_mut().resize(columns, 60);
        screen.frame(app);
        if app.view.geometry.transcript.width == width {
            return;
        }
    }
    panic!("no screen width gives {width} columns of conversation");
}

/// The picture of a conversation 70 columns wide, where "G latest" at the right edge falls over the picture's right side,
/// with the conversation `rows` tall and scrolled to the newest message.
fn app_with_a_label_over_the_picture(choice: ImageChoice, rows: u16) -> (App, Screen) {
    let mut app = picture_app(choice, Some(CELL));
    let mut screen = Screen::new(140, 60);
    give_the_width(&mut app, &mut screen, 70);
    until_the_picture_is_up(&mut app, &mut screen);
    give_the_conversation(&mut app, &mut screen, rows);
    screen.frame(&mut app);
    (app, screen)
}

/// The cells of the "G latest" label, from the screen's geometry, and the part of them that is inside `rect`.
fn latest_label_inside(app: &App, rect: Rect) -> Vec<(u16, u16, char)> {
    let area = app.view.geometry.transcript;
    let text = " ↓ G latest ";
    let x0 = area.x + area.width - 13;
    let y = area.y + area.height - 1;
    text.chars()
        .enumerate()
        .map(|(i, c)| (x0 + i as u16, y, c))
        .filter(|(x, y, _)| rect.contains((*x, *y).into()))
        .collect()
}

/// Scroll up from the newest message until the picture is on the label's row, giving the frame that did it.
fn until_the_label_is_over_the_picture(app: &mut App, screen: &mut Screen) -> String {
    for _ in 0..40 {
        app.scroll_by(-1);
        let written = screen.frame(app);
        if let Some(p) = app.gfx_plan().first()
            && !latest_label_inside(app, p.rect).is_empty()
        {
            return written;
        }
    }
    let st = app.state().unwrap();
    panic!(
        "the label never fell over the picture: picture rows {:?}, scroll {}, viewport {:?}, plan {:?}, lines {}",
        st.transcript.image_rows[0].0,
        st.scroll,
        app.view.geometry.transcript,
        app.gfx_plan().first().map(|p| (p.rect, p.rows.clone())),
        st.transcript.lines.len()
    );
}

#[test]
fn the_latest_label_over_a_sixel_is_written_over_it_afterwards_and_the_picture_stays_hi_res() {
    let (mut app, mut screen) =
        app_with_a_label_over_the_picture(ImageChoice::Protocol(Protocol::Sixel), 22);
    let first = app.gfx_plan()[0].clone();
    assert!(
        first.overlay.is_empty(),
        "at the newest message there is no label"
    );
    assert_eq!(screen.frame(&mut app), "");

    let written = until_the_label_is_over_the_picture(&mut app, &mut screen);
    let p = app.gfx_plan()[0].clone();
    let want = latest_label_inside(&app, p.rect);
    assert!(!want.is_empty());
    // The picture is drawn on the rows it occupies, including the one with the label: no cut for the label.
    let transcript = app.view.geometry.transcript;
    assert_eq!(
        p.rect.bottom(),
        transcript.bottom(),
        "down to the label's row (cut there by the viewport, not the label)"
    );
    // Every cell of the label inside the picture is rewritten, in order, in the label's own style.
    assert_eq!(
        p.overlay.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>(),
        want.iter().map(|(x, y, _)| (*x, *y)).collect::<Vec<_>>()
    );
    assert_eq!(
        p.overlay
            .iter()
            .map(|c| c.symbol.clone())
            .collect::<String>(),
        want.iter().map(|(_, _, c)| *c).collect::<String>()
    );
    let bg = match crate::theme::gradient(0.6) {
        ratatui::style::Color::Rgb(r, g, b) => (r, g, b),
        other => panic!("{other:?}"),
    };
    assert!(
        p.overlay
            .iter()
            .all(|c| c.bold && c.bg == bg && c.fg == Some((17, 17, 27)))
    );
    // Written after the picture, in the same write, with the cursor put back after it.
    let sixel_end = written.find("\x1b\\").unwrap() + 2;
    let after = &written[sixel_end..];
    let text: String = want.iter().map(|(_, _, c)| *c).collect();
    assert!(written.find(SIXEL_START).unwrap() < written.find(&text).unwrap());
    assert!(
        after.contains(&text) && after.ends_with("\x1b8"),
        "{after:?}"
    );
    // The cells are the picture's: the frame leaves them alone, the label's included.
    assert_eq!(
        screen.skipped.len(),
        usize::from(p.rect.width) * usize::from(p.rect.height)
    );
    // Standing still writes nothing, label or not.
    assert_eq!(screen.frame(&mut app), "");
    assert_eq!(screen.frame(&mut app), "");

    // Back at the newest message the label is gone: the picture is written again without it.
    for _ in 0..40 {
        app.scroll_by(1);
        let written = screen.frame(&mut app);
        if app.view.geometry.transcript.height > 0 && app.state().is_some_and(|s| s.follow) {
            let p = app.gfx_plan().first().expect("the picture is still there");
            assert!(p.overlay.is_empty(), "no label, no overlay");
            assert!(
                written.contains(SIXEL_START),
                "the cells of the label are the picture's again"
            );
            assert!(
                !written.contains(&text),
                "and nothing of the label: {written:?}"
            );
            return;
        }
    }
    panic!("never back at the newest message");
}

#[test]
fn with_no_label_over_the_picture_nothing_but_the_picture_is_written() {
    let (mut app, mut screen) =
        app_with_a_label_over_the_picture(ImageChoice::Protocol(Protocol::Sixel), 22);
    // The picture is at the top of what is shown and the label is on the last row, a long way off.
    let id = app.current.clone().unwrap();
    let _ = id;
    let written = {
        // One scroll step moves the picture; the label is not over it (the picture ends well above the last row).
        app.scroll_by(-1);
        screen.frame(&mut app)
    };
    let p = app.gfx_plan()[0].clone();
    if latest_label_inside(&app, p.rect).is_empty() {
        assert!(p.overlay.is_empty());
        assert!(
            written.ends_with("\x1b\\\x1b8"),
            "the picture and the restored cursor, nothing between: {written:?}"
        );
    }
}

#[test]
fn a_label_over_a_kitty_picture_is_the_frames_own_text_over_a_picture_placed_under_it() {
    let (mut app, mut screen) =
        app_with_a_label_over_the_picture(ImageChoice::Protocol(Protocol::Kitty), 22);
    let written = until_the_label_is_over_the_picture(&mut app, &mut screen);
    let p = app.gfx_plan()[0].clone();
    assert!(
        p.overlay.is_empty(),
        "nothing is written over a Kitty picture: it is under the text"
    );
    assert!(
        written.contains("a=p,i=") && written.contains(",z=-1"),
        "placed under the text: {written:?}"
    );
    assert!(
        !written.contains("latest"),
        "the label comes from the frame, not from the presenter"
    );
    assert!(screen.skipped.is_empty(), "the frame owns the cells");
    // The cells under the picture are blanks, except the label, which the frame draws as usual.
    let want = latest_label_inside(&app, p.rect);
    let buf = screen.terminal.backend().buffer();
    for y in p.rect.y..p.rect.bottom() {
        for x in p.rect.x..p.rect.right() {
            let expected = want
                .iter()
                .find(|(lx, ly, _)| (*lx, *ly) == (x, y))
                .map_or(' ', |(_, _, c)| *c);
            assert_eq!(buf[(x, y)].symbol(), expected.to_string(), "cell {x},{y}");
        }
    }
    // The label going away is the frame's business too: nothing is sent for it.
    for _ in 0..40 {
        app.scroll_by(1);
        let written = screen.frame(&mut app);
        assert!(!written.contains("a=t"), "no data sent again");
        if app.state().is_some_and(|s| s.follow) {
            let buf = screen.terminal.backend().buffer();
            let p = app.gfx_plan().first().expect("still there").clone();
            for y in p.rect.y..p.rect.bottom() {
                for x in p.rect.x..p.rect.right() {
                    assert_eq!(buf[(x, y)].symbol(), " ", "no label left behind at {x},{y}");
                }
            }
            return;
        }
    }
    panic!("never back at the newest message");
}

#[test]
fn the_older_messages_label_never_cuts_a_picture_either() {
    // A tall screen, so that the whole chat and its picture are in view at the top.
    let mut app = picture_app(ImageChoice::Protocol(Protocol::Sixel), Some(CELL));
    let mut screen = Screen::new(140, 70);
    until_the_picture_is_up(&mut app, &mut screen);
    app.scroll_by(-1000);
    screen.frame(&mut app);
    assert_eq!(app.state().unwrap().scroll, 0);
    let st = app.state().unwrap();
    let rows = st.transcript.image_rows[0].0.clone();
    assert!(
        rows.start >= 1,
        "the first line of a chat is its date, so a picture is never on the label's row"
    );
    let before = app.gfx_plan()[0].clone();
    let id = app.current.clone().unwrap();
    app.states.get_mut(&id).unwrap().older = Some("more".into());
    let written = screen.frame(&mut app);
    let after = app.gfx_plan()[0].clone();
    assert_eq!(
        (before.rect, before.rows.clone()),
        (after.rect, after.rows.clone()),
        "not cut for the label"
    );
    assert!(
        after.overlay.is_empty(),
        "the label does not reach the picture"
    );
    assert_eq!(written, "", "so nothing is written for it");
}
