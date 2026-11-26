//! What the terminal is sent for a frame: counted per kind of frame, and checked against the way the loop used to send it.
//!
//! `OldPath` is the loop's frame as it was before the output was gathered into one write (a synchronized-update begin,
//! a cursor hide, ratatui's draw, the pictures through their own handle, a synchronized-update end, each flushed on its
//! own), kept as the reference the new path is compared with and as the baseline the measurement prints. `Term` is a
//! small terminal that reads what was sent, so that two ways of sending can be compared by the screen they leave.

use super::*;
use crate::app::Focus;
use crate::demo::DemoBackend;
use crate::editor::{Keymap, Mode};
use crate::gfx::probe::Facts;
use crate::gfx::{CellPx, ImageChoice, Protocol};
use crate::imgart::ImgMode;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::crossterm::execute;
use std::io::LineWriter;
use std::sync::Arc;

const CELL: CellPx = CellPx { w: 10, h: 20 };
const BSU: &str = "\x1b[?2026h";
const ESU: &str = "\x1b[?2026l";
const HIDE: &str = "\x1b[?25l";
const SHOW: &str = "\x1b[?25h";
const SIXEL_START: &str = "\x1bP9;1;0q";

// ---------------------------------------------------------------------------
// Counting
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Tally {
    writes: usize,
    flushes: usize,
    bytes: usize,
    /// Everything that arrived, in order: what the terminal would have been sent.
    data: Vec<u8>,
}

/// The far end of the pipe: counts the calls made to it and keeps the bytes.
#[derive(Clone, Default)]
struct Counting(Rc<RefCell<Tally>>);

impl Write for Counting {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut t = self.0.borrow_mut();
        t.writes += 1;
        t.bytes += buf.len();
        t.data.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flushes += 1;
        Ok(())
    }
}

/// What reached the far end up to now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Stats {
    /// Calls to `write` (what would be system calls).
    writes: usize,
    /// Calls to `flush`.
    flushes: usize,
    bytes: usize,
}

impl Stats {
    fn since(self, before: Stats) -> Stats {
        Stats {
            writes: self.writes - before.writes,
            flushes: self.flushes - before.flushes,
            bytes: self.bytes - before.bytes,
        }
    }
}

impl Counting {
    fn stats(&self) -> Stats {
        let t = self.0.borrow();
        Stats {
            writes: t.writes,
            flushes: t.flushes,
            bytes: t.bytes,
        }
    }

    fn data(&self) -> Vec<u8> {
        self.0.borrow().data.clone()
    }
}

/// A pipe that refuses a write that holds `fail_on`, while it is armed (and says nothing is written then).
#[derive(Clone, Default)]
struct Flaky {
    pipe: Counting,
    fail_on: Rc<RefCell<Option<Vec<u8>>>>,
}

impl Write for Flaky {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(pattern) = self.fail_on.borrow().as_ref()
            && buf.windows(pattern.len()).any(|w| w == pattern.as_slice())
        {
            return Err(std::io::Error::other("the link went away"));
        }
        self.pipe.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.pipe.flush()
    }
}

// ---------------------------------------------------------------------------
// The two ways of sending a frame
// ---------------------------------------------------------------------------

/// One way of sending frames, over a counted pipe.
trait Path {
    fn frame(&mut self, app: &mut App);
    fn resize(&mut self, width: u16, height: u16);
    /// What has reached the pipe so far, in calls and bytes.
    fn stats(&self) -> Stats;
    /// Every byte sent so far.
    fn sent(&self) -> Vec<u8>;
}

fn shape_of(app: &App) -> CursorShape {
    if app.editor.mode() == Mode::Insert {
        CursorShape::Bar
    } else {
        CursorShape::Block
    }
}

/// The standard output as Rust gives it: line-buffered with a kilobyte, so what reaches the system is its chunks.
#[derive(Clone)]
struct Stdoutish(Rc<RefCell<LineWriter<Counting>>>);

impl Write for Stdoutish {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

/// The loop's frame before the output was gathered: begin update, hide the cursor, draw, pictures through a handle of
/// their own, end update, and the cursor's shape when the mode changed.
struct OldPath {
    terminal: Terminal<CrosstermBackend<Stdoutish>>,
    out: Stdoutish,
    pictures_out: Counting,
    presenter: Presenter,
    cursor_mode: Option<Mode>,
    pipe: Counting,
}

impl OldPath {
    fn new(width: u16, height: u16) -> Self {
        colours_on();
        let pipe = Counting::default();
        let out = Stdoutish(Rc::new(RefCell::new(LineWriter::with_capacity(
            1024,
            pipe.clone(),
        ))));
        let terminal = Terminal::with_options(
            CrosstermBackend::new(out.clone()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, width, height)),
            },
        )
        .unwrap();
        Self {
            terminal,
            out,
            pictures_out: pipe.clone(),
            presenter: Presenter::default(),
            cursor_mode: None,
            pipe,
        }
    }
}

impl Path for OldPath {
    fn frame(&mut self, app: &mut App) {
        execute!(self.out, BeginSynchronizedUpdate).unwrap();
        self.terminal.hide_cursor().unwrap();
        self.terminal.draw(|f| crate::ui::render(f, app)).unwrap();
        app.present_graphics(&mut self.presenter, &mut self.pictures_out)
            .unwrap();
        execute!(self.out, EndSynchronizedUpdate).unwrap();
        let mode = app.editor.mode();
        if self.cursor_mode != Some(mode) {
            let style = if mode == Mode::Insert {
                SetCursorStyle::SteadyBar
            } else {
                SetCursorStyle::SteadyBlock
            };
            execute!(self.out, style).unwrap();
            self.cursor_mode = Some(mode);
        }
    }

    /// A new terminal on the same output, after clearing the screen as a resize does. ratatui's own `resize` asks
    /// the terminal where its cursor is, which needs a real terminal and fails (`ENOTTY`) where tests run without
    /// one, as they do in CI on Unix.
    fn resize(&mut self, width: u16, height: u16) {
        execute!(
            self.out,
            ratatui::crossterm::terminal::Clear(ratatui::crossterm::terminal::ClearType::All)
        )
        .unwrap();
        self.terminal = Terminal::with_options(
            CrosstermBackend::new(self.out.clone()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, width, height)),
            },
        )
        .unwrap();
    }

    fn stats(&self) -> Stats {
        self.pipe.stats()
    }

    fn sent(&self) -> Vec<u8> {
        self.pipe.data()
    }
}

/// The frame as it is sent now.
struct NewPath {
    display: Display<Counting>,
    pipe: Counting,
}

impl NewPath {
    fn new(width: u16, height: u16) -> Self {
        colours_on();
        let pipe = Counting::default();
        Self {
            display: Display::fixed(pipe.clone(), width, height).unwrap(),
            pipe,
        }
    }
}

impl Path for NewPath {
    fn frame(&mut self, app: &mut App) {
        self.display.frame(app, shape_of(app)).unwrap();
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.display.set_size(width, height);
    }

    fn stats(&self) -> Stats {
        self.pipe.stats()
    }

    fn sent(&self) -> Vec<u8> {
        self.pipe.data()
    }
}

// ---------------------------------------------------------------------------
// A terminal that reads what was sent
// ---------------------------------------------------------------------------

/// Colours and attributes a cell is drawn in, as the escape sequences said them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Pen {
    fg: Option<String>,
    bg: Option<String>,
    flags: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Glyph {
    text: String,
    pen: Pen,
}

impl Glyph {
    fn blank() -> Self {
        Self {
            text: " ".into(),
            pen: Pen::default(),
        }
    }
}

/// Enough of a terminal to tell what two streams of bytes leave on the screen: positions, text, colours and attributes
/// of cells, the cursor (place, shown, shape), save and restore of the cursor, and the synchronized update (which it
/// counts the misuse of). Pictures (device control, application and operating system strings) are skipped, not drawn.
struct Term {
    width: usize,
    height: usize,
    cells: Vec<Glyph>,
    x: usize,
    y: usize,
    pen: Pen,
    saved: Option<(usize, usize, Pen)>,
    shown: bool,
    shape: Option<u32>,
    /// Inside a synchronized update.
    sync: bool,
    /// Updates begun inside one, or ended outside one.
    misuse: usize,
}

impl Term {
    fn new(width: u16, height: u16) -> Self {
        let (width, height) = (usize::from(width), usize::from(height));
        Self {
            width,
            height,
            cells: vec![Glyph::blank(); width * height],
            x: 0,
            y: 0,
            pen: Pen::default(),
            saved: None,
            shown: true,
            shape: None,
            sync: false,
            misuse: 0,
        }
    }

    /// The window changed size: what was on the screen is gone, as it is for a terminal that is cleared on resize.
    fn resize(&mut self, width: u16, height: u16) {
        let (shown, shape, sync, misuse) = (self.shown, self.shape, self.sync, self.misuse);
        *self = Self::new(width, height);
        self.shown = shown;
        self.shape = shape;
        self.sync = sync;
        self.misuse = misuse;
    }

    fn feed(&mut self, bytes: &[u8]) {
        let text = std::str::from_utf8(bytes).expect("what is written is text");
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => self.escape(&mut chars),
                c if c.is_control() => {}
                c => self.print(c),
            }
        }
    }

    fn escape(&mut self, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        match chars.next() {
            Some('[') => {
                let mut params = String::new();
                let mut inter = String::new();
                for c in chars.by_ref() {
                    match c {
                        '0'..='9' | ';' | '?' | ':' => params.push(c),
                        ' ' => inter.push(c),
                        '@'..='~' => {
                            self.csi(&params, &inter, c);
                            break;
                        }
                        _ => break,
                    }
                }
            }
            Some('7') => self.saved = Some((self.x, self.y, self.pen.clone())),
            Some('8') => {
                if let Some((x, y, pen)) = self.saved.clone() {
                    (self.x, self.y, self.pen) = (x, y, pen);
                }
            }
            // A string (a picture, a title): up to its terminator.
            Some(']' | 'P' | '_' | '^' | 'X') => {
                while let Some(c) = chars.next() {
                    match c {
                        '\x1b' => {
                            chars.next();
                            break;
                        }
                        '\x07' => break,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn csi(&mut self, params: &str, inter: &str, last: char) {
        let private = params.starts_with('?');
        let nums: Vec<u32> = params
            .trim_start_matches('?')
            .split(';')
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        match (last, private) {
            ('H', false) => {
                let row = nums.first().copied().unwrap_or(1).max(1) as usize;
                let col = nums.get(1).copied().unwrap_or(1).max(1) as usize;
                self.y = (row - 1).min(self.height.saturating_sub(1));
                self.x = (col - 1).min(self.width.saturating_sub(1));
            }
            ('m', false) => self.sgr(&nums),
            ('h' | 'l', true) => {
                let on = last == 'h';
                match nums[0] {
                    25 => self.shown = on,
                    2026 => {
                        if on == self.sync {
                            self.misuse += 1;
                        }
                        self.sync = on;
                    }
                    _ => {}
                }
            }
            ('J', false) => {
                let from = match nums[0] {
                    2 | 3 => 0,
                    _ => self.y * self.width + self.x,
                };
                for cell in &mut self.cells[from..] {
                    *cell = Glyph::blank();
                }
            }
            ('K', false) => {
                let (from, to) = match nums[0] {
                    2 => (self.y * self.width, (self.y + 1) * self.width),
                    _ => (self.y * self.width + self.x, (self.y + 1) * self.width),
                };
                for cell in &mut self.cells[from..to] {
                    *cell = Glyph::blank();
                }
            }
            ('q', false) if inter == " " => self.shape = Some(nums[0]),
            _ => {}
        }
    }

    fn sgr(&mut self, nums: &[u32]) {
        let mut it = nums.iter().copied();
        let colour = |it: &mut dyn Iterator<Item = u32>| -> String {
            match it.next() {
                Some(2) => format!(
                    "{},{},{}",
                    it.next().unwrap_or(0),
                    it.next().unwrap_or(0),
                    it.next().unwrap_or(0)
                ),
                Some(5) => format!("i{}", it.next().unwrap_or(0)),
                other => format!("?{other:?}"),
            }
        };
        while let Some(n) = it.next() {
            match n {
                0 => self.pen = Pen::default(),
                1 => self.pen.flags |= 1,
                2 => self.pen.flags |= 2,
                3 => self.pen.flags |= 4,
                4 => self.pen.flags |= 8,
                7 => self.pen.flags |= 16,
                9 => self.pen.flags |= 32,
                22 => self.pen.flags &= !3,
                23 => self.pen.flags &= !4,
                24 => self.pen.flags &= !8,
                27 => self.pen.flags &= !16,
                29 => self.pen.flags &= !32,
                30..=37 | 90..=97 => self.pen.fg = Some(n.to_string()),
                38 => self.pen.fg = Some(colour(&mut it)),
                39 => self.pen.fg = None,
                40..=47 | 100..=107 => self.pen.bg = Some(n.to_string()),
                48 => self.pen.bg = Some(colour(&mut it)),
                49 => self.pen.bg = None,
                58 => {
                    colour(&mut it);
                }
                _ => {}
            }
        }
    }

    fn print(&mut self, c: char) {
        use unicode_width::UnicodeWidthChar;
        let width = c.width().unwrap_or(0);
        if width == 0 {
            // Joins the cell before it (a joiner, a variation selector, a combining mark).
            let mut at = self.y * self.width + self.x.saturating_sub(1);
            if self.cells[at].text.is_empty() && at > 0 {
                at -= 1;
            }
            self.cells[at].text.push(c);
            return;
        }
        if self.x + width > self.width {
            return;
        }
        let at = self.y * self.width + self.x;
        self.cells[at] = Glyph {
            text: c.to_string(),
            pen: self.pen.clone(),
        };
        if width == 2 {
            self.cells[at + 1] = Glyph {
                text: String::new(),
                pen: self.pen.clone(),
            };
        }
        self.x += width;
    }

    /// The text of the screen, a line to a row.
    fn text(&self) -> String {
        self.cells
            .chunks(self.width)
            .map(|row| row.iter().map(|g| g.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Where two terminals differ, in words; `None` if they show the same.
    fn difference(&self, other: &Term) -> Option<String> {
        if (self.width, self.height) != (other.width, other.height) {
            return Some("different sizes".into());
        }
        for (i, (a, b)) in self.cells.iter().zip(&other.cells).enumerate() {
            if a != b {
                return Some(format!(
                    "cell ({}, {}): {a:?} against {b:?}",
                    i % self.width,
                    i / self.width
                ));
            }
        }
        if (self.x, self.y) != (other.x, other.y) && self.shown {
            return Some(format!(
                "cursor at ({}, {}) against ({}, {})",
                self.x, self.y, other.x, other.y
            ));
        }
        if self.shown != other.shown {
            return Some(format!(
                "cursor shown {} against {}",
                self.shown, other.shown
            ));
        }
        if self.shape != other.shape {
            return Some(format!(
                "cursor shape {:?} against {:?}",
                self.shape, other.shape
            ));
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Scenes
// ---------------------------------------------------------------------------

fn pump(app: &mut App, until: impl Fn(&App) -> bool) {
    for _ in 0..50 {
        app.on_tick();
        if until(app) {
            return;
        }
    }
    panic!("condition not reached");
}

/// Colour codes are written whatever the environment says: crossterm leaves them out when NO_COLOR is set, until the
/// first time anything (the Markdown renderer, which needs them to read) turns them on for the whole process, and that
/// would make what a frame writes depend on which test ran before.
fn colours_on() {
    ratatui::crossterm::style::force_color_output(true);
}

/// An app on the demo's chat with a picture in it, set to draw pictures with `choice`.
fn demo_app(choice: ImageChoice) -> App {
    colours_on();
    let (mut app, _) = App::for_test(Arc::new(DemoBackend::instant()));
    app.start();
    pump(&mut app, |a| a.chats.loaded() && a.me.is_some());
    app.set_images(
        choice,
        Facts {
            cell: Some(CELL),
            why: "a test",
            ..Facts::NONE
        },
    );
    app.select_chat("demo-ada");
    app
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// The mouse wheel over the conversation: `up` is back in time.
fn wheel(app: &mut App, up: bool) {
    let t = app.view.geometry.transcript;
    app.on_mouse(MouseEvent {
        kind: if up {
            MouseEventKind::ScrollUp
        } else {
            MouseEventKind::ScrollDown
        },
        column: t.x + 2,
        row: t.y + 2,
        modifiers: KeyModifiers::NONE,
    });
}

/// One step of the animation clock, as the loop's tick makes it.
fn animate(app: &mut App) {
    app.tick += 1;
    app.animate();
}

/// A frame, and what it cost.
fn measure(path: &mut impl Path, app: &mut App) -> Stats {
    let before = path.stats();
    path.frame(app);
    path.stats().since(before)
}

/// A frame, and the bytes it sent.
fn bytes_of(path: &mut impl Path, app: &mut App) -> String {
    let mark = path.sent().len();
    path.frame(app);
    String::from_utf8(path.sent()[mark..].to_vec()).unwrap()
}

/// Frames and ticks until the picture is in the plan, then a few more so that everything is as it will stay.
/// Returns what the frame in which the picture first went out cost.
fn until_the_picture_is_up(path: &mut impl Path, app: &mut App) -> Stats {
    for _ in 0..80 {
        let cost = measure(path, app);
        if !app.gfx_plan().is_empty() {
            for _ in 0..3 {
                path.frame(app);
            }
            return cost;
        }
        app.on_tick();
    }
    panic!("the picture never came up");
}

/// Frames until one changes nothing on the new path's pipe, then the app is as it will stay.
fn settle(path: &mut impl Path, app: &mut App) {
    for _ in 0..30 {
        if measure(path, app).bytes <= 60 && measure(path, app).bytes <= 60 {
            return;
        }
        app.on_tick();
    }
    panic!("the screen never settled");
}

/// Resize until the conversation is `width` columns wide, then `rows` tall, as the picture tests do.
fn give_the_conversation(path: &mut impl Path, app: &mut App, width: u16, rows: u16) {
    for columns in 80..200 {
        path.resize(columns, 60);
        path.frame(app);
        if app.view.geometry.transcript.width == width {
            break;
        }
    }
    assert_eq!(app.view.geometry.transcript.width, width);
    let columns = app.view.screen.width;
    for height in 14..80 {
        path.resize(columns, height);
        path.frame(app);
        if app.view.geometry.transcript.height == rows {
            return;
        }
    }
    panic!("no height gives {rows} rows of conversation");
}

/// Scroll back until a label falls over the picture, giving the bytes and the cost of the frame that did it.
fn until_a_label_is_over_the_picture(path: &mut impl Path, app: &mut App) -> (String, Stats) {
    for _ in 0..40 {
        wheel(app, true);
        let before = path.stats();
        let text = bytes_of(path, app);
        if app
            .gfx_plan()
            .first()
            .is_some_and(|g| !g.overlay.is_empty())
        {
            return (text, path.stats().since(before));
        }
    }
    panic!("the label never fell over the picture");
}

/// The cost of each kind of frame, as `(what, what it cost)`.
fn scenes<P: Path>(mut path: impl FnMut(u16, u16) -> P) -> Vec<(&'static str, Stats)> {
    let mut found = Vec::new();

    // Nothing changes, and then the clock moves on.
    {
        let mut p = path(140, 60);
        let mut app = demo_app(ImageChoice::Protocol(Protocol::Sixel));
        until_the_picture_is_up(&mut p, &mut app);
        let idle = measure(&mut p, &mut app);
        found.push(("idle frame (nothing changed)", idle));
        let mut quiet = None;
        let mut moved = None;
        for _ in 0..200 {
            animate(&mut app);
            let cost = measure(&mut p, &mut app);
            if cost.bytes > idle.bytes + 5 {
                moved.get_or_insert(cost);
            } else {
                quiet.get_or_insert(cost);
            }
            if moved.is_some() && quiet.is_some() {
                break;
            }
        }
        found.push((
            "tick that changes no cell",
            quiet.expect("a tick that changes nothing"),
        ));
        found.push((
            "animation step (header colours)",
            moved.expect("a tick that changes the header"),
        ));
    }

    // A key typed into the message box.
    {
        let mut p = path(140, 60);
        let mut app = demo_app(ImageChoice::Art(ImgMode::Blocks));
        for _ in 0..4 {
            p.frame(&mut app);
            app.on_tick();
        }
        app.focus = Focus::Editor;
        app.on_key(key(KeyCode::Char('h')));
        p.frame(&mut app);
        app.on_key(key(KeyCode::Char('e')));
        found.push(("typing one character", measure(&mut p, &mut app)));
    }

    // One notch of the wheel over a long conversation.
    {
        let mut p = path(140, 24);
        let mut app = demo_app(ImageChoice::Art(ImgMode::Blocks));
        app.select_chat("demo-platform");
        for _ in 0..4 {
            p.frame(&mut app);
            app.on_tick();
        }
        wheel(&mut app, true);
        p.frame(&mut app);
        wheel(&mut app, true);
        found.push(("scrolling the conversation", measure(&mut p, &mut app)));
    }

    // Opening another chat.
    {
        let mut p = path(140, 60);
        let mut app = demo_app(ImageChoice::Art(ImgMode::Blocks));
        for _ in 0..4 {
            p.frame(&mut app);
            app.on_tick();
        }
        app.select_chat("demo-grace");
        app.on_tick();
        found.push(("opening a chat", measure(&mut p, &mut app)));
    }

    // A picture going out, in each protocol.
    for (name, protocol) in [
        ("a Sixel picture appears", Protocol::Sixel),
        ("a Kitty picture appears", Protocol::Kitty),
    ] {
        let mut p = path(140, 60);
        let mut app = demo_app(ImageChoice::Protocol(protocol));
        let cost = until_the_picture_is_up(&mut p, &mut app);
        found.push((name, cost));
    }

    // A label falls over a Sixel picture.
    {
        let mut p = path(140, 60);
        let mut app = demo_app(ImageChoice::Protocol(Protocol::Sixel));
        until_the_picture_is_up(&mut p, &mut app);
        give_the_conversation(&mut p, &mut app, 70, 22);
        let mut cost = None;
        for _ in 0..40 {
            wheel(&mut app, true);
            let c = measure(&mut p, &mut app);
            if app
                .gfx_plan()
                .first()
                .is_some_and(|g| !g.overlay.is_empty())
            {
                cost = Some(c);
                break;
            }
        }
        found.push((
            "a hint label over a Sixel picture",
            cost.expect("the label never fell over the picture"),
        ));
    }
    found
}

fn print_scenes(title: &str, rows: &[(&'static str, Stats)]) {
    println!("\n== {title}");
    println!(
        "{:<36} {:>7} {:>8} {:>9}",
        "frame", "writes", "flushes", "bytes"
    );
    for (what, s) in rows {
        println!("{what:<36} {:>7} {:>8} {:>9}", s.writes, s.flushes, s.bytes);
    }
}

/// What an idle app costs over one second (17 loop iterations of 60 ms, with the clock moving as the loop moves it):
/// the calls made, the bytes, and how many frames wrote anything.
fn an_idle_second<P: Path>(
    path: impl FnOnce(u16, u16) -> P,
    focused: bool,
    busy: bool,
) -> (Stats, usize) {
    let mut p = path(140, 60);
    let mut app = if busy {
        let (mut app, _) = App::for_test(Arc::new(DemoBackend::instant()));
        app.start();
        app
    } else {
        demo_app(ImageChoice::Art(ImgMode::Blocks))
    };
    for _ in 0..4 {
        p.frame(&mut app);
        if !busy {
            app.on_tick();
        }
    }
    app.set_focused(focused);
    for _ in 0..3 {
        p.frame(&mut app);
    }
    app.tick = 0;
    app.animate();
    p.frame(&mut app);
    let before = p.stats();
    let mut written = 0;
    for _ in 0..17 {
        animate(&mut app);
        written += usize::from(measure(&mut p, &mut app).bytes > 0);
    }
    (p.stats().since(before), written)
}

#[test]
#[ignore = "a measurement, run on demand: cargo test --bin vtteams output_cost -- --ignored --nocapture"]
fn output_cost_by_kind_of_frame() {
    let old = scenes(OldPath::new);
    print_scenes(
        "before: separate flushed writes (writes are what reaches the system through a 1 KiB line buffer)",
        &old,
    );
    let new = scenes(NewPath::new);
    print_scenes(
        "after: one write and one flush a frame, none for a frame that changes nothing",
        &new,
    );
    println!(
        "\n== one second (17 frames of 60 ms), nothing happening, both with the throttled animation"
    );
    println!(
        "{:<38} {:>7} {:>8} {:>8} {:>8}",
        "", "writes", "flushes", "bytes", "frames"
    );
    for (what, focused, busy) in [
        ("focused, idle", true, false),
        ("focused, loading (spinner)", true, true),
        ("terminal behind other windows", false, false),
    ] {
        let (o, ow) = an_idle_second(OldPath::new, focused, busy);
        let (n, nw) = an_idle_second(NewPath::new, focused, busy);
        println!(
            "{:<38} {:>7} {:>8} {:>8} {:>8}",
            format!("old path, {what}"),
            o.writes,
            o.flushes,
            o.bytes,
            ow
        );
        println!(
            "{:<38} {:>7} {:>8} {:>8} {:>8}",
            format!("new path, {what}"),
            n.writes,
            n.flushes,
            n.bytes,
            nw
        );
    }
}

// ---------------------------------------------------------------------------
// One write a frame, none for an empty one
// ---------------------------------------------------------------------------

#[test]
fn an_app_nothing_happens_to_writes_nothing_at_all() {
    let mut p = NewPath::new(140, 60);
    let mut app = demo_app(ImageChoice::Art(ImgMode::Blocks));
    for _ in 0..4 {
        p.frame(&mut app);
        app.on_tick();
    }
    settle(&mut p, &mut app);
    let before = p.stats();
    for _ in 0..25 {
        p.frame(&mut app);
    }
    assert_eq!(
        p.stats().since(before),
        Stats::default(),
        "no write, no flush, not even the update's markers or the cursor"
    );
}

#[test]
fn a_changed_cell_is_one_write_between_the_markers_with_the_cursor_hidden_and_shown_inside() {
    let mut p = NewPath::new(140, 60);
    let mut app = demo_app(ImageChoice::Art(ImgMode::Blocks));
    app.set_keymap(Keymap::Standard);
    for _ in 0..4 {
        p.frame(&mut app);
        app.on_tick();
    }
    settle(&mut p, &mut app);
    // The first key is typed into the message box, where the cursor is shown.
    app.focus = Focus::Editor;
    app.on_key(key(KeyCode::Char('z')));
    p.frame(&mut app);
    settle(&mut p, &mut app);

    app.on_key(key(KeyCode::Char('q')));
    let before = p.stats();
    let text = bytes_of(&mut p, &mut app);
    let cost = p.stats().since(before);
    assert_eq!((cost.writes, cost.flushes), (1, 1), "{text:?}");
    assert!(text.starts_with(BSU), "{text:?}");
    assert!(text.ends_with(ESU), "{text:?}");
    let (hide, cell, show, end) = (
        text.find(HIDE)
            .expect("the cursor is hidden while cells are written"),
        text.find('q').expect("the cell"),
        text.rfind(SHOW).expect("and shown again"),
        text.rfind(ESU).unwrap(),
    );
    assert!(
        BSU.len() <= hide && hide < cell && cell < show && show < end,
        "{text:?}"
    );
    assert!(
        text[show..end].contains('H') || text[cell..show].contains('H'),
        "the cursor is put where the text goes: {text:?}"
    );
    assert_eq!(text.matches(BSU).count(), 1);
    assert_eq!(text.matches(ESU).count(), 1);
    assert!(
        text.len() < 400,
        "one character is a few bytes, not a screen: {}",
        text.len()
    );
}

#[test]
fn a_key_that_moves_the_cursor_costs_one_small_write() {
    let mut p = NewPath::new(140, 60);
    let mut app = demo_app(ImageChoice::Art(ImgMode::Blocks));
    app.set_keymap(Keymap::Standard);
    app.focus = Focus::Editor;
    for c in "hello world".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    for _ in 0..4 {
        p.frame(&mut app);
        app.on_tick();
    }
    settle(&mut p, &mut app);
    app.on_key(key(KeyCode::Left));
    let before = p.stats();
    let text = bytes_of(&mut p, &mut app);
    let cost = p.stats().since(before);
    assert_eq!((cost.writes, cost.flushes), (1, 1), "{text:?}");
    // The cursor goes where the text cursor is, shown once, inside one update with the little that changed.
    assert_eq!(text.matches(SHOW).count(), 1, "{text:?}");
    assert_eq!(text.matches(BSU).count(), 1, "{text:?}");
    assert!(text.len() < 200, "{text:?}");
}

#[test]
fn a_frame_with_a_picture_has_the_picture_between_the_markers_in_the_same_write() {
    for protocol in [Protocol::Sixel, Protocol::Kitty, Protocol::Iterm] {
        let mut p = NewPath::new(140, 60);
        let mut app = demo_app(ImageChoice::Protocol(protocol));
        let mut seen = None;
        for _ in 0..80 {
            let before = p.stats();
            let text = bytes_of(&mut p, &mut app);
            let cost = p.stats().since(before);
            assert!(
                cost.writes <= 1 && cost.flushes <= 1,
                "{protocol:?}: {cost:?}"
            );
            if !app.gfx_plan().is_empty() {
                seen = Some((text, cost));
                break;
            }
            app.on_tick();
        }
        let (text, cost) = seen.expect("the picture was never planned");
        assert_eq!((cost.writes, cost.flushes), (1, 1), "{protocol:?}");
        assert!(text.starts_with(BSU) && text.ends_with(ESU), "{protocol:?}");
        let start = match protocol {
            Protocol::Sixel => "\x1bP",
            Protocol::Kitty => "\x1b_G",
            Protocol::Iterm => "\x1b]1337;File=",
        };
        let at = text.find(start).expect("the picture's own bytes");
        assert!(text.find(BSU).unwrap() < at && at < text.rfind(ESU).unwrap());
        assert!(text.len() > 10_000, "{protocol:?}: {} bytes", text.len());
        assert_eq!(text.matches(BSU).count(), 1, "{protocol:?}");
        assert_eq!(app.gfx_plan().len(), 1);
    }
}

#[test]
fn a_hint_label_over_a_picture_follows_the_picture_in_the_same_write() {
    let mut p = NewPath::new(140, 60);
    let mut app = demo_app(ImageChoice::Protocol(Protocol::Sixel));
    until_the_picture_is_up(&mut p, &mut app);
    give_the_conversation(&mut p, &mut app, 70, 22);
    let (text, cost) = until_a_label_is_over_the_picture(&mut p, &mut app);
    assert_eq!((cost.writes, cost.flushes), (1, 1));
    let overlay: String = app.gfx_plan()[0]
        .overlay
        .iter()
        .map(|c| c.symbol.as_str())
        .collect();
    let picture = text
        .find("\x1bP")
        .expect("the picture is sent again with the label");
    let label = text[picture..].find(&overlay).expect("the label's cells");
    assert!(text.starts_with(BSU) && text.ends_with(ESU));
    assert!(label > 0, "{overlay:?} comes after the picture");
    assert_eq!(text.matches(BSU).count(), 1);
    assert_eq!(text.matches(ESU).count(), 1);
}

#[test]
fn a_draw_that_fails_writes_nothing_and_leaves_no_update_open() {
    let pipe = Counting::default();
    let mut display = Display::fixed(pipe.clone(), 140, 60).unwrap();
    let mut app = demo_app(ImageChoice::Art(ImgMode::Blocks));
    display.frame(&mut app, CursorShape::Block).unwrap();
    let before = pipe.stats();
    display.make_size_fail(true);
    app.on_key(key(KeyCode::Char('z')));
    assert!(display.frame(&mut app, CursorShape::Block).is_err());
    assert_eq!(
        pipe.stats(),
        before,
        "an error while the frame is built writes nothing: no update begun, so none left open"
    );
    // It works again when the terminal can be asked, and the screen is complete.
    display.make_size_fail(false);
    let sent = display.frame(&mut app, CursorShape::Block).unwrap();
    assert!(sent.bytes > 0);
    let text = String::from_utf8(pipe.data()).unwrap();
    let mut term = Term::new(140, 60);
    term.feed(text.as_bytes());
    assert!(!term.sync && term.misuse == 0);
    assert!(
        term.text().contains('z'),
        "the key typed during the failure is on the screen"
    );
}

#[test]
fn a_write_that_fails_is_not_taken_for_pictures_that_are_up_and_the_next_frame_sends_them_again() {
    let flaky = Flaky::default();
    let mut display = Display::fixed(flaky.clone(), 140, 60).unwrap();
    let mut app = demo_app(ImageChoice::Protocol(Protocol::Sixel));
    // Everything that carries a Sixel is refused.
    *flaky.fail_on.borrow_mut() = Some(SIXEL_START.as_bytes().to_vec());
    let mut failed = false;
    for _ in 0..80 {
        match display.frame(&mut app, CursorShape::Block) {
            Ok(_) => {}
            Err(_) => {
                failed = true;
                break;
            }
        }
        app.on_tick();
    }
    assert!(failed, "the picture never came to be written");
    assert_eq!(
        display.pictures_up(),
        0,
        "the write failed, so the picture is not up"
    );
    // The link is back: the same picture is sent again, and now it is up.
    *flaky.fail_on.borrow_mut() = None;
    let mark = flaky.pipe.data().len();
    let sent = display.frame(&mut app, CursorShape::Block).unwrap();
    assert!(sent.bytes > 10_000, "{} bytes", sent.bytes);
    let text = String::from_utf8(flaky.pipe.data()[mark..].to_vec()).unwrap();
    assert!(text.contains(SIXEL_START), "sent again");
    assert!(text.starts_with(BSU) && text.ends_with(ESU));
    assert_eq!(display.pictures_up(), 1);
    // And now it is not sent again.
    let before = flaky.pipe.stats();
    display.frame(&mut app, CursorShape::Block).unwrap();
    assert_eq!(flaky.pipe.stats(), before);
}

#[test]
fn the_screen_is_drawn_again_in_full_after_a_write_that_failed() {
    let flaky = Flaky::default();
    let mut display = Display::fixed(flaky.clone(), 100, 30).unwrap();
    let mut app = demo_app(ImageChoice::Art(ImgMode::Blocks));
    display.frame(&mut app, CursorShape::Block).unwrap();
    // Anything written now fails.
    *flaky.fail_on.borrow_mut() = Some(b"z".to_vec());
    app.on_key(key(KeyCode::Char('z')));
    assert!(display.frame(&mut app, CursorShape::Block).is_err());
    *flaky.fail_on.borrow_mut() = None;
    let mark = flaky.pipe.data().len();
    display.frame(&mut app, CursorShape::Block).unwrap();
    let text = String::from_utf8(flaky.pipe.data()[mark..].to_vec()).unwrap();
    assert!(
        text.len() > 2_000,
        "the whole screen again, not just the difference: {} bytes",
        text.len()
    );
}

// ---------------------------------------------------------------------------
// Assembling
// ---------------------------------------------------------------------------

fn at(x: u16, y: u16) -> Option<Position> {
    Some(Position { x, y })
}

fn assembled(cells: &str, pictures: &str, now: OnScreen, wanted: Wanted) -> (String, OnScreen) {
    let mut frame = Vec::new();
    let next = assemble(
        &mut frame,
        cells.as_bytes(),
        pictures.as_bytes(),
        now,
        wanted,
    );
    (String::from_utf8(frame).unwrap(), next)
}

const SHOWN_AT_1_2: OnScreen = OnScreen {
    shown: true,
    at: Some(Position { x: 1, y: 2 }),
    shape: Some(CursorShape::Bar),
};

fn wants(shown: bool, to: Option<Position>, shape: CursorShape) -> Wanted {
    Wanted {
        shown,
        at: to,
        shape,
    }
}

#[test]
fn nothing_drawn_and_the_cursor_where_it_is_is_nothing() {
    let (frame, next) = assembled(
        "",
        "",
        SHOWN_AT_1_2,
        wants(true, at(1, 2), CursorShape::Bar),
    );
    assert_eq!(frame, "");
    assert_eq!(next, SHOWN_AT_1_2);
    // A hidden cursor that stays hidden, wherever it is asked to be.
    let hidden = OnScreen {
        shown: false,
        at: None,
        shape: Some(CursorShape::Block),
    };
    let (frame, next) = assembled("", "", hidden, wants(false, at(5, 5), CursorShape::Block));
    assert_eq!(frame, "");
    assert_eq!(next, hidden);
}

#[test]
fn a_drawn_frame_is_bracketed_and_the_cursor_goes_out_and_comes_back_inside_it() {
    let (frame, next) = assembled(
        "CELLS",
        "PICTURES",
        SHOWN_AT_1_2,
        wants(true, at(7, 3), CursorShape::Bar),
    );
    assert_eq!(
        frame,
        format!("{BSU}{HIDE}CELLSPICTURES\x1b[4;8H{SHOW}{ESU}")
    );
    assert_eq!(next.at, at(7, 3));
    // The same cursor place is put again after cells, which moved it.
    let (frame, _) = assembled(
        "C",
        "",
        SHOWN_AT_1_2,
        wants(true, at(1, 2), CursorShape::Bar),
    );
    assert_eq!(frame, format!("{BSU}{HIDE}C\x1b[3;2H{SHOW}{ESU}"));
}

#[test]
fn a_drawn_frame_with_no_cursor_neither_hides_nor_shows_one_that_is_already_hidden() {
    let hidden = OnScreen {
        shown: false,
        at: None,
        shape: Some(CursorShape::Block),
    };
    let (frame, next) = assembled("C", "", hidden, wants(false, None, CursorShape::Block));
    assert_eq!(frame, format!("{BSU}C{ESU}"));
    assert!(!next.shown);
    // One that was shown goes away, once, at the start.
    let (frame, next) = assembled("C", "", SHOWN_AT_1_2, wants(false, None, CursorShape::Bar));
    assert_eq!(frame, format!("{BSU}{HIDE}C{ESU}"));
    assert!(!next.shown);
}

#[test]
fn a_cursor_that_changes_alone_writes_only_what_changed() {
    let (frame, _) = assembled(
        "",
        "",
        SHOWN_AT_1_2,
        wants(true, at(4, 0), CursorShape::Bar),
    );
    assert_eq!(frame, "\x1b[1;5H");
    let (frame, _) = assembled("", "", SHOWN_AT_1_2, wants(false, None, CursorShape::Bar));
    assert_eq!(frame, HIDE);
    let gone = OnScreen {
        shown: false,
        at: None,
        shape: Some(CursorShape::Bar),
    };
    let (frame, next) = assembled("", "", gone, wants(true, at(2, 2), CursorShape::Bar));
    assert_eq!(
        frame,
        format!("\x1b[3;3H{SHOW}"),
        "moved before it is shown"
    );
    assert_eq!(next.at, at(2, 2));
    let (frame, _) = assembled(
        "",
        "",
        SHOWN_AT_1_2,
        wants(true, at(1, 2), CursorShape::Block),
    );
    assert_eq!(frame, "\x1b[2 q", "only the shape");
}

#[test]
fn the_cursors_shape_is_written_when_it_changes_and_not_otherwise() {
    let (first, next) = assembled("", "", OnScreen::START, wants(true, None, CursorShape::Bar));
    assert!(first.contains("\x1b[6 q"), "{first:?}");
    assert_eq!(next.shape, Some(CursorShape::Bar));
    let (again, _) = assembled("", "", next, wants(true, None, CursorShape::Bar));
    assert_eq!(again, "");
    let (block, _) = assembled("", "", next, wants(true, None, CursorShape::Block));
    assert_eq!(block, "\x1b[2 q");
}

#[test]
fn pictures_alone_make_a_frame() {
    let (frame, _) = assembled(
        "",
        "P",
        SHOWN_AT_1_2,
        wants(true, at(1, 2), CursorShape::Bar),
    );
    assert_eq!(frame, format!("{BSU}{HIDE}P\x1b[3;2H{SHOW}{ESU}"));
}

// ---------------------------------------------------------------------------
// The animation
// ---------------------------------------------------------------------------

/// A settled app on a new path, with the pipe to count in.
fn settled() -> (NewPath, App) {
    let mut p = NewPath::new(140, 60);
    let mut app = demo_app(ImageChoice::Art(ImgMode::Blocks));
    for _ in 0..4 {
        p.frame(&mut app);
        app.on_tick();
    }
    settle(&mut p, &mut app);
    (p, app)
}

fn non_empty_frames(p: &mut NewPath, app: &mut App, ticks: usize) -> usize {
    (0..ticks)
        .filter(|_| {
            animate(app);
            measure(p, app).bytes > 0
        })
        .count()
}

#[test]
fn most_ticks_leave_an_idle_frame_empty_and_the_header_moves_a_few_times_a_second() {
    let (mut p, mut app) = settled();
    app.tick = 0;
    app.animate();
    p.frame(&mut app);
    let moved = non_empty_frames(&mut p, &mut app, 17);
    assert!(
        (1..=5).contains(&moved),
        "{moved} frames of the header in one second (17 ticks of 60 ms)"
    );
    // Over ten seconds, however it falls, never faster than about four a second.
    let moved = non_empty_frames(&mut p, &mut app, 170);
    assert!(moved <= 45, "{moved} frames in ten seconds");
}

#[test]
fn a_header_nobody_can_see_does_not_move() {
    // The terminal behind other windows.
    let (mut p, mut app) = settled();
    app.set_focused(false);
    app.tick = 0;
    app.animate();
    p.frame(&mut app);
    settle(&mut p, &mut app);
    assert_eq!(
        non_empty_frames(&mut p, &mut app, 200),
        0,
        "an unfocused terminal is sent nothing while nothing happens"
    );
    // In front again, it moves once more.
    app.set_focused(true);
    assert!(non_empty_frames(&mut p, &mut app, 40) > 0);

    // The person away from the keyboard.
    let (mut p, mut app) = settled();
    let Some(long_ago) = Instant::now().checked_sub(Duration::from_secs(45 * 60)) else {
        // A computer started less than 45 minutes ago has no such moment on its clock.
        return;
    };
    app.set_last_input(long_ago);
    app.tick = 0;
    app.animate();
    p.frame(&mut app);
    settle(&mut p, &mut app);
    assert_eq!(non_empty_frames(&mut p, &mut app, 200), 0);
}

#[test]
fn what_a_person_does_is_drawn_at_once_whether_or_not_the_header_moves() {
    let (mut p, mut app) = settled();
    app.set_focused(false);
    settle(&mut p, &mut app);
    app.on_key(key(KeyCode::Char('x')));
    assert!(
        measure(&mut p, &mut app).bytes > 0,
        "a key typed is on screen in the next frame"
    );
    settle(&mut p, &mut app);
    app.select_chat("demo-grace");
    app.on_tick();
    assert!(
        measure(&mut p, &mut app).bytes > 0,
        "so is a chat that was opened"
    );
}

#[test]
fn the_clock_does_not_run_faster_than_the_loop_does() {
    let (_, mut app) = settled();
    let (before, started) = (app.tick, Instant::now());
    for _ in 0..500 {
        app.on_tick();
    }
    // One step at most for each 50 ms that passed (and the one the first wake-up may be owed), however often it woke.
    let allowed = started.elapsed().as_millis() / 50 + 1;
    let moved = (app.tick - before) as u128;
    assert!(
        moved <= allowed,
        "500 wake-ups in {:?} moved the clock {moved} steps",
        started.elapsed()
    );
}
#[test]
fn a_spinner_moves_every_other_tick_and_nothing_else_does_between() {
    // Before the first list has come, the app is busy: spinners in the header and the list.
    let (mut app, _) = App::for_test(Arc::new(DemoBackend::instant()));
    app.start();
    app.set_keymap(Keymap::Standard);
    assert!(app.is_busy());
    let mut p = NewPath::new(140, 60);
    p.frame(&mut app);
    p.frame(&mut app);
    let mut moves = Vec::new();
    for _ in 0..16 {
        animate(&mut app);
        moves.push(measure(&mut p, &mut app).bytes > 0);
    }
    // The first step 0 -> 1 keeps the glyph (tick / 2): moves are only on every other tick, and a step is 120 ms at most
    // eight times a second.
    assert_eq!(moves.iter().filter(|m| **m).count(), 8, "{moves:?}");
    for pair in moves.chunks(2) {
        assert_eq!(pair, [false, true], "{moves:?}");
    }
}

// ---------------------------------------------------------------------------
// What the terminal is left showing
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

fn random_key(r: &mut Rng) -> KeyEvent {
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    match r.below(30) {
        0..=11 => key(KeyCode::Char(
            "abcdefghijklmnopqrstuvwxyz ,.:/@+#"
                .chars()
                .nth(r.below(34))
                .unwrap_or('a'),
        )),
        12 => key(KeyCode::Enter),
        13 => key(KeyCode::Esc),
        14 => key(KeyCode::Tab),
        15 => key(KeyCode::Up),
        16 => key(KeyCode::Down),
        17 => key(KeyCode::Left),
        18 => key(KeyCode::Right),
        19 => key(KeyCode::PageUp),
        20 => key(KeyCode::PageDown),
        21 => key(KeyCode::Home),
        22 => key(KeyCode::End),
        23 => key(KeyCode::Backspace),
        24 => key(KeyCode::F(1)),
        25 => ctrl('k'),
        26 => ctrl('n'),
        27 => ctrl('f'),
        28 => key(KeyCode::F(2)),
        _ => key(KeyCode::Delete),
    }
}

const SIZES: [(u16, u16); 6] = [
    (140, 60),
    (100, 30),
    (80, 24),
    (200, 50),
    (60, 20),
    (120, 40),
];
const CHATS: [&str; 5] = [
    "demo-ada",
    "demo-grace",
    "demo-platform",
    "demo-linus",
    "demo-sprint",
];

/// The old way and the new way of sending the same frames leave the same screen: cells, colours, cursor.
fn compare_the_two_ways(seed: u64, choice: ImageChoice, keymap: Keymap, steps: usize) {
    let mut rng = Rng(seed);
    let (mut width, mut height) = SIZES[0];
    let mut old = OldPath::new(width, height);
    let mut new = NewPath::new(width, height);
    let mut app = demo_app(choice);
    app.set_keymap(keymap);
    let (mut old_term, mut new_term) = (Term::new(width, height), Term::new(width, height));
    let (mut old_read, mut new_read) = (0, 0);
    for step in 0..steps {
        let did = match rng.below(12) {
            0..=4 => {
                let k = random_key(&mut rng);
                app.on_key(k);
                format!("key {k:?}")
            }
            5 => {
                let up = rng.below(2) == 0;
                wheel(&mut app, up);
                format!("wheel up={up}")
            }
            6 | 7 => {
                let by = 1 + rng.below(4);
                for _ in 0..by {
                    animate(&mut app);
                }
                format!("{by} ticks of the clock")
            }
            8 => {
                app.on_tick();
                "a tick".to_string()
            }
            9 => {
                let (w, h) = SIZES[rng.below(SIZES.len())];
                // Only a change of size is a resize: the terminal says nothing of a window left as it was.
                if (w, h) != (width, height) {
                    (width, height) = (w, h);
                    old.resize(w, h);
                    new.resize(w, h);
                    old_term.resize(w, h);
                    new_term.resize(w, h);
                }
                format!("resize to {w}x{h}")
            }
            10 => {
                let focused = rng.below(2) == 0;
                app.set_focused(focused);
                format!("focus {focused}")
            }
            _ => {
                let id = CHATS[rng.below(CHATS.len())];
                app.select_chat(id);
                app.on_tick();
                format!("open {id}")
            }
        };
        old.frame(&mut app);
        let before = new.stats();
        new.frame(&mut app);
        let cost = new.stats().since(before);
        assert!(
            cost.writes <= 1 && cost.flushes <= 1,
            "seed {seed} step {step} ({did}): {cost:?}"
        );
        assert_eq!(
            cost.writes, cost.flushes,
            "seed {seed} step {step} ({did}): a write is flushed, nothing else is"
        );
        let (o, n) = (old.sent(), new.sent());
        let (old_from, new_from) = (old_read, new_read);
        old_term.feed(&o[old_read..]);
        new_term.feed(&n[new_read..]);
        (old_read, new_read) = (o.len(), n.len());
        assert!(
            !new_term.sync && new_term.misuse == 0 && old_term.misuse == 0,
            "seed {seed} step {step} ({did}): an update left open or begun twice"
        );
        if let Some(why) = old_term.difference(&new_term) {
            panic!(
                "seed {seed} step {step} ({did}) at {width}x{height}: {why}\nold sent {:?}\nnew sent {:?}\nold:\n{}\nnew:\n{}",
                String::from_utf8_lossy(&o[old_from..][..300.min(o.len() - old_from)]),
                String::from_utf8_lossy(&n[new_from..][..300.min(n.len() - new_from)]),
                old_term.text(),
                new_term.text()
            );
        }
    }
}

#[test]
fn one_write_a_frame_leaves_the_screen_the_separate_writes_did() {
    let choices = [
        ImageChoice::Protocol(Protocol::Sixel),
        ImageChoice::Protocol(Protocol::Kitty),
        ImageChoice::Art(ImgMode::Blocks),
        ImageChoice::Protocol(Protocol::Iterm),
        ImageChoice::Protocol(Protocol::Sixel),
        ImageChoice::Art(ImgMode::Octant),
    ];
    for (seed, choice) in choices.into_iter().enumerate() {
        let keymap = if seed % 2 == 0 {
            Keymap::Standard
        } else {
            Keymap::Vim
        };
        compare_the_two_ways(seed as u64 + 1, choice, keymap, 160);
    }
}

#[test]
fn the_terminal_reader_reads_what_ratatui_writes() {
    // A check of `Term` against the buffer ratatui drew: its text is the screen's.
    let mut p = NewPath::new(100, 30);
    let mut app = demo_app(ImageChoice::Art(ImgMode::Blocks));
    for _ in 0..4 {
        p.frame(&mut app);
        app.on_tick();
    }
    let mut term = Term::new(100, 30);
    term.feed(&p.sent());
    let shown = term.text();
    assert!(
        shown.contains("vtteams") || shown.contains("v t t e a m s"),
        "{shown}"
    );
    assert!(shown.contains("Ada"), "{shown}");
    assert!(!term.sync && term.misuse == 0);
}

// ---------------------------------------------------------------------------
// The pieces
// ---------------------------------------------------------------------------

#[test]
fn the_frame_buffers_are_reused_and_a_huge_one_is_given_back() {
    let mut display = Display::fixed(Counting::default(), 100, 30).unwrap();
    let mut app = demo_app(ImageChoice::Art(ImgMode::Blocks));
    display.frame(&mut app, CursorShape::Block).unwrap();
    let kept = display.frame.capacity();
    assert!(kept > 0, "the first frame's buffer is kept for the next");
    for _ in 0..5 {
        app.on_key(key(KeyCode::Char('a')));
        display.frame(&mut app, CursorShape::Block).unwrap();
    }
    assert_eq!(display.frame.capacity(), kept, "and not allocated again");
    display.frame.reserve(4 * KEEP_CAPACITY);
    display.pics.reserve(4 * KEEP_CAPACITY);
    display.cells.borrow_mut().reserve(4 * KEEP_CAPACITY);
    display.keep_small();
    assert!(display.frame.capacity() <= KEEP_CAPACITY);
    assert!(display.pics.capacity() <= KEEP_CAPACITY);
    assert!(display.cells.borrow().capacity() <= KEEP_CAPACITY);
}

#[test]
fn a_big_ascii_frame_may_use_the_whole_buffer_handle_and_nothing_else_may() {
    let big = vec![b'a'; WHOLE_WRITE_FROM];
    assert!(fits_whole_write(&big));
    assert!(
        !fits_whole_write(&big[..WHOLE_WRITE_FROM - 1]),
        "a small one"
    );
    let mut text = big.clone();
    text.extend_from_slice("\u{2501}".as_bytes());
    assert!(!fits_whole_write(&text), "text the console must convert");
}

#[test]
fn a_slow_frame_is_described_by_where_its_time_went() {
    let ms = Duration::from_millis;
    let parts = FrameParts {
        draw: ms(100),
        render: ms(4),
        graphics: ms(5),
        write: ms(6),
    };
    assert_eq!(
        parts.describe(1234),
        "render 4 ms, diff 96 ms, graphics 5 ms, one write of 1234 bytes 6 ms"
    );
    // A draw that did not reach its render has no render time and never a negative rest.
    let failed = FrameParts {
        draw: ms(1),
        render: ms(3),
        ..Default::default()
    };
    assert!(failed.describe(0).contains("diff 0 ms"));
}

#[test]
fn a_control_character_in_a_cell_is_found_and_clean_text_is_not() {
    use ratatui::style::Style;
    let area = Rect::new(2, 3, 10, 2);
    let mut buffer = Buffer::empty(area);
    buffer.set_string(2, 3, "hello wide \u{e9}\u{4e16}", Style::new());
    assert_eq!(
        first_control_cell(&buffer),
        None,
        "ordinary text, including accented and wide"
    );
    // `set_string` drops control characters itself, so put one into a cell directly, as a bug elsewhere could.
    buffer[(5, 4)].set_symbol("\u{1b}");
    assert_eq!(first_control_cell(&buffer), Some((5, 4, 0x1b)));
    let mut other = Buffer::empty(area);
    other[(2, 3)].set_symbol("\u{9d}");
    assert_eq!(
        first_control_cell(&other),
        Some((2, 3, 0x9d)),
        "a C1 control counts too"
    );
}

#[test]
fn the_backend_notes_the_cursor_and_writes_cells_only_when_there_are_some() {
    let cells = Collected::default();
    let shared = Rc::clone(&cells.0);
    let mut backend = FrameBackend::new(
        Some(Size {
            width: 10,
            height: 3,
        }),
        cells,
    );
    backend.draw(std::iter::empty()).unwrap();
    assert!(
        shared.borrow().is_empty(),
        "no changed cell, no bytes (not even the resets)"
    );
    backend.hide_cursor().unwrap();
    backend.show_cursor().unwrap();
    backend.set_cursor_position((4, 1)).unwrap();
    backend.flush().unwrap();
    assert!(shared.borrow().is_empty(), "the cursor is only noted");
    assert_eq!(backend.wanted_cursor(), (true, at(4, 1)));
    let cell = Cell::new("x");
    backend.draw(std::iter::once((2u16, 1u16, &cell))).unwrap();
    assert!(String::from_utf8_lossy(&shared.borrow()).contains('x'));
}
