//! What is written to the terminal, and how: a frame goes out in one write, and a frame that changes nothing goes out as
//! nothing at all.
//!
//! The loop used to make about five writes a frame, each flushed on its own (begin a synchronized update, hide the
//! cursor, ratatui's cell updates, the pictures, end the update), about every 60 ms whether or not anything had changed.
//! On a slow link those arrive as separate packets, so the terminal could show a half-built frame (the update's begin and
//! end were not in the packets that held its content), the cursor visibly hopped, and an idle program sent a few dozen
//! bytes fifteen times a second. Now:
//!
//! - [`Display::frame`] draws into ratatui's buffers as before, but the backend it draws through ([`FrameBackend`]) writes
//!   nowhere: the cell updates are collected in memory and what ratatui asks of the cursor (shown or hidden, where) is
//!   only noted. The pictures' bytes (`Presenter::stage`) are collected next to them.
//! - Whether anything changed is read from those bytes: no cell updates, no picture bytes and the cursor and its shape
//!   where the terminal already has them means no write at all, not even the update markers.
//! - Otherwise everything goes out as one `write_all` and one flush: begin update, hide the cursor (when it is shown),
//!   the cells, the pictures, the cursor's position and showing, its shape if that changed, end update. A frame that
//!   changed only the cursor writes just that (a move, a show or a hide).
//! - The bytes are only written once the frame is complete, so a failure while the frame is built leaves the terminal
//!   exactly as it was (no update begun and left open), and the presenter's idea of what pictures are up changes only if the
//!   write succeeded.
//!
//! Rendering still runs every iteration of the loop (about half a millisecond); only what it makes of the terminal's
//! output is skipped.

use crate::app::App;
use crate::gfx::present::Presenter;
use crate::ui;
use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::{Buffer, Cell};
use ratatui::crossterm::cursor::{Hide, MoveTo, SetCursorStyle, Show};
use ratatui::crossterm::queue;
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::layout::{Position, Rect, Size};
use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// A frame buffer that has grown past this is given back to the allocator after the frame, so that one picture does not
/// keep a megabyte for the rest of the session.
const KEEP_CAPACITY: usize = 1 << 20;

/// At least this many bytes, and nothing but ASCII, on Windows go out through a handle of the console's own that takes a
/// whole buffer in one call (see [`TerminalOut`]).
const WHOLE_WRITE_FROM: usize = 16 * 1024;

/// How the terminal draws the cursor: a bar while typing, a block otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Bar,
    Block,
}

impl CursorShape {
    fn command(self) -> SetCursorStyle {
        match self {
            Self::Bar => SetCursorStyle::SteadyBar,
            Self::Block => SetCursorStyle::SteadyBlock,
        }
    }
}

// ---------------------------------------------------------------------------
// The backend that writes nowhere
// ---------------------------------------------------------------------------

/// Bytes ratatui's crossterm backend writes, kept where the display can take them.
#[derive(Clone, Debug, Default)]
struct Collected(Rc<RefCell<Vec<u8>>>);

impl Write for Collected {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// ratatui's crossterm backend, with two differences. Cell updates and clears go into memory instead of to the terminal
/// (and a draw with no changed cell adds nothing, where the crossterm backend adds a reset of the colours), and the cursor
/// is not written at all: showing, hiding and moving it are noted, for [`Display`] to write once the frame is complete.
#[derive(Debug)]
pub struct FrameBackend {
    inner: CrosstermBackend<Collected>,
    /// A size to report instead of asking the terminal (for tests, which have no terminal).
    fixed: Option<Size>,
    /// Make the size unreadable, to see a draw fail (tests).
    #[cfg(test)]
    size_fails: std::cell::Cell<bool>,
    shown: bool,
    at: Option<Position>,
}

impl FrameBackend {
    fn new(fixed: Option<Size>, cells: Collected) -> Self {
        Self {
            inner: CrosstermBackend::new(cells),
            fixed,
            #[cfg(test)]
            size_fails: std::cell::Cell::new(false),
            shown: true,
            at: None,
        }
    }

    /// What the frame drawn last wants of the cursor: shown, and where.
    fn wanted_cursor(&self) -> (bool, Option<Position>) {
        (self.shown, self.at)
    }
}

impl Backend for FrameBackend {
    type Error = std::io::Error;

    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut content = content.peekable();
        if content.peek().is_none() {
            return Ok(());
        }
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> std::io::Result<()> {
        self.shown = false;
        Ok(())
    }

    fn show_cursor(&mut self) -> std::io::Result<()> {
        self.shown = true;
        Ok(())
    }

    fn get_cursor_position(&mut self) -> std::io::Result<Position> {
        if self.fixed.is_some() {
            return Ok(self.at.unwrap_or_default());
        }
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> std::io::Result<()> {
        self.at = Some(position.into());
        Ok(())
    }

    fn clear(&mut self) -> std::io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> std::io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> std::io::Result<()> {
        self.inner.append_lines(n)
    }

    fn size(&self) -> std::io::Result<Size> {
        #[cfg(test)]
        if self.size_fails.get() {
            return Err(std::io::Error::other("the terminal's size cannot be read"));
        }
        match self.fixed {
            Some(size) => Ok(size),
            None => self.inner.size(),
        }
    }

    fn window_size(&mut self) -> std::io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Assembling a frame
// ---------------------------------------------------------------------------

/// What the terminal holds of the cursor, as far as what was written to it says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OnScreen {
    shown: bool,
    /// Where, if known: the cell updates and pictures move it, and then it is only known again once it has been put.
    at: Option<Position>,
    shape: Option<CursorShape>,
}

impl OnScreen {
    /// A terminal just taken over: the cursor is shown, and nothing else is known.
    const START: Self = Self {
        shown: true,
        at: None,
        shape: None,
    };
}

/// What a frame wants the cursor to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Wanted {
    shown: bool,
    at: Option<Position>,
    shape: CursorShape,
}

fn command(buf: &mut Vec<u8>, c: impl ratatui::crossterm::Command) {
    // Writing into memory cannot fail.
    let _ = queue!(buf, c);
}

/// Put one frame's bytes into `frame` (emptied first): what has to be written to take the terminal from `now` to a
/// screen showing `cells` and `pictures` with the cursor as `wanted`. Nothing in `frame` means there is nothing to write.
/// Returns what the terminal then holds of the cursor.
fn assemble(
    frame: &mut Vec<u8>,
    cells: &[u8],
    pictures: &[u8],
    now: OnScreen,
    wanted: Wanted,
) -> OnScreen {
    frame.clear();
    let shown = wanted.shown;
    let mut at = now.at;
    let drawn = !cells.is_empty() || !pictures.is_empty();
    if drawn {
        command(frame, BeginSynchronizedUpdate);
        if now.shown {
            command(frame, Hide);
        }
        frame.extend_from_slice(cells);
        frame.extend_from_slice(pictures);
        // The cells and pictures moved the cursor; it is wherever the last of them left it until it is put.
        at = None;
    }
    if shown {
        if let Some(to) = wanted.at
            && (drawn || at != Some(to))
        {
            command(frame, MoveTo(to.x, to.y));
            at = Some(to);
        }
        // After the move, so that it is never shown at the old place. A cursor that was shown was hidden for the cells.
        if drawn || !now.shown {
            command(frame, Show);
        }
    } else if now.shown && !drawn {
        command(frame, Hide);
    }
    if now.shape != Some(wanted.shape) {
        command(frame, wanted.shape.command());
    }
    if drawn {
        command(frame, EndSynchronizedUpdate);
    }
    OnScreen {
        shown,
        at: if shown { at } else { None },
        shape: Some(wanted.shape),
    }
}

// ---------------------------------------------------------------------------
// The display
// ---------------------------------------------------------------------------

/// How long each step of one frame took, for the record of a slow one: `render` is all our own work (laying out, drawing
/// into ratatui's buffer), the rest of `draw` is ratatui comparing it with the last frame and encoding the difference,
/// `graphics` is working out the pictures' bytes and `write` the one write to the terminal, which is where a slow link
/// shows.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameParts {
    /// The whole of `terminal.draw`: asking the size, `render`, the difference with the last frame, encoding it.
    pub draw: Duration,
    pub render: Duration,
    pub graphics: Duration,
    pub write: Duration,
}

impl FrameParts {
    /// The steps in milliseconds, for the log.
    pub fn describe(&self, bytes: usize) -> String {
        format!(
            "render {} ms, diff {} ms, graphics {} ms, one write of {bytes} bytes {} ms",
            self.render.as_millis(),
            self.draw.saturating_sub(self.render).as_millis(),
            self.graphics.as_millis(),
            self.write.as_millis()
        )
    }
}

/// What a frame did.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sent {
    /// Bytes written: nothing for a frame that changed nothing.
    pub bytes: usize,
    pub parts: FrameParts,
}

/// The terminal's screen: ratatui's buffers, the pictures on it, what its cursor is doing, and where frames are written.
pub struct Display<W: Write> {
    terminal: Terminal<FrameBackend>,
    /// The cell updates of the frame being drawn, written by the backend.
    cells: Rc<RefCell<Vec<u8>>>,
    out: W,
    pictures: Presenter,
    on_screen: OnScreen,
    /// The bytes of the frame being sent, and of its pictures: kept between frames so that none allocates.
    frame: Vec<u8>,
    pics: Vec<u8>,
    /// A write failed, so the terminal may not hold what ratatui thinks it does: draw everything again.
    redraw: bool,
    warned_control: bool,
}

impl<W: Write> Display<W> {
    /// A display of the terminal this process runs in, written to through `out`. Nothing is written.
    pub fn new(out: W) -> std::io::Result<Self> {
        Self::over(out, None)
    }

    /// A display of a screen `width` x `height`, for tests: no terminal is asked its size.
    #[cfg(test)]
    pub fn fixed(out: W, width: u16, height: u16) -> std::io::Result<Self> {
        Self::over(out, Some(Size { width, height }))
    }

    fn over(out: W, size: Option<Size>) -> std::io::Result<Self> {
        let cells = Collected::default();
        Ok(Self {
            cells: Rc::clone(&cells.0),
            terminal: Terminal::new(FrameBackend::new(size, cells))?,
            out,
            pictures: Presenter::default(),
            on_screen: OnScreen::START,
            frame: Vec::new(),
            pics: Vec::new(),
            redraw: false,
            warned_control: false,
        })
    }

    /// The screen's size now, asked of the terminal.
    pub fn size(&self) -> std::io::Result<Size> {
        self.terminal.size()
    }

    /// Change the size a test display reports; the next frame draws all of it again.
    #[cfg(test)]
    pub fn set_size(&mut self, width: u16, height: u16) {
        self.terminal.backend_mut().fixed = Some(Size { width, height });
    }

    /// Make the terminal's size unreadable (or readable again), so that the draw of the next frame fails (tests).
    #[cfg(test)]
    pub fn make_size_fail(&mut self, fails: bool) {
        self.terminal.backend().size_fails.set(fails);
    }

    /// How many pictures the terminal is thought to show.
    #[cfg(test)]
    pub fn pictures_up(&self) -> usize {
        self.pictures.showing()
    }

    /// Take down what is in the terminal's own layer, for leaving.
    pub fn teardown(&mut self) {
        self.pictures.teardown(&mut self.out);
    }

    /// Draw one frame of `app` and write it: one write and one flush, or nothing if nothing changed.
    pub fn frame(&mut self, app: &mut App, shape: CursorShape) -> std::io::Result<Sent> {
        let mut parts = FrameParts::default();
        if self.redraw {
            self.redraw = false;
            let size = self.terminal.size()?;
            self.terminal.resize(Rect::from(size))?;
        }
        let drawing = Instant::now();
        let mut rendering = Duration::ZERO;
        let drawn = self.terminal.draw(|f| {
            let started = Instant::now();
            ui::render(f, app);
            rendering = started.elapsed();
        });
        parts.draw = drawing.elapsed();
        parts.render = rendering;
        match drawn {
            // Nothing has been written for this frame, so no update is left open and the terminal is as it was.
            Err(e) => {
                self.cells.borrow_mut().clear();
                return Err(e);
            }
            // Nothing the frame holds may be a control character: ratatui writes a cell's text to the terminal as it is,
            // so one would be an escape sequence. Remote text is cleaned long before this; this is the tripwire if that
            // ever fails. Logged once (position and code point only, never the text around it).
            Ok(done) if !self.warned_control => {
                if let Some((x, y, code)) = first_control_cell(done.buffer) {
                    self.warned_control = true;
                    log::warn!(
                        "a control character (U+{code:04X}) is in the frame at column {x}, row {y}: some text was not cleaned"
                    );
                }
            }
            Ok(_) => {}
        }

        // The pictures that go over the cells the frame left alone, as bytes for the same write.
        let staging = Instant::now();
        self.pics.clear();
        let staged = app.stage_graphics(&mut self.pictures, &mut self.pics);
        parts.graphics = staging.elapsed();

        let (shown, at) = self.terminal.backend().wanted_cursor();
        let wanted = Wanted { shown, at, shape };
        let next = assemble(
            &mut self.frame,
            &self.cells.borrow(),
            &self.pics,
            self.on_screen,
            wanted,
        );
        self.cells.borrow_mut().clear();
        if self.frame.is_empty() {
            self.keep_small();
            return Ok(Sent { bytes: 0, parts });
        }

        let writing = Instant::now();
        let written = self
            .out
            .write_all(&self.frame)
            .and_then(|()| self.out.flush());
        parts.write = writing.elapsed();
        let bytes = self.frame.len();
        self.keep_small();
        match written {
            Ok(()) => {
                self.on_screen = next;
                if staged > 0 {
                    app.graphics_written(&mut self.pictures, staged, parts.write);
                    log::trace!(
                        "graphics: {staged} bytes written, {} pictures up",
                        self.pictures.showing()
                    );
                }
                Ok(Sent { bytes, parts })
            }
            Err(e) => {
                // How much of it got there is not known: the terminal may be inside an update, with part of a frame, a
                // picture that is not up and a cursor anywhere. The pictures are written again, and the cells.
                self.pictures.discard();
                self.on_screen = OnScreen::START;
                self.redraw = true;
                Err(e)
            }
        }
    }

    /// Give back the memory a very large frame took.
    fn keep_small(&mut self) {
        for buf in [&mut self.frame, &mut self.pics] {
            buf.clear();
            if buf.capacity() > KEEP_CAPACITY {
                buf.shrink_to(KEEP_CAPACITY / 4);
            }
        }
        let mut cells = self.cells.borrow_mut();
        if cells.capacity() > KEEP_CAPACITY {
            cells.shrink_to(KEEP_CAPACITY / 4);
        }
    }
}

/// The first cell of `buffer` whose text holds a control character, as (column, row, code point). A terminal hyperlink
/// made by `hyperlink::mark` (and only one of exactly that form, with an address that passes its check) is not one.
pub fn first_control_cell(buffer: &Buffer) -> Option<(u16, u16, u32)> {
    let width = usize::from(buffer.area.width.max(1));
    buffer.content.iter().enumerate().find_map(|(i, cell)| {
        if crate::hyperlink::is_ours(cell.symbol()) {
            return None;
        }
        let c = cell.symbol().chars().find(|c| c.is_control())?;
        let x = u16::try_from(i % width).ok()?.saturating_add(buffer.area.x);
        let y = u16::try_from(i / width).ok()?.saturating_add(buffer.area.y);
        Some((x, y, u32::from(c)))
    })
}

// ---------------------------------------------------------------------------
// The terminal itself
// ---------------------------------------------------------------------------

/// Whether a write of `bytes` should go through the console's whole-buffer handle: a big one that is all ASCII.
/// That handle hands the bytes to the console in the console's own code page, which is only the same text for ASCII; the
/// other way of writing (the standard output) converts to UTF-16 first, in pieces of a few kilobytes.
#[cfg_attr(not(windows), allow(dead_code))]
fn fits_whole_write(bytes: &[u8]) -> bool {
    bytes.len() >= WHOLE_WRITE_FROM && bytes.is_ascii()
}

/// The terminal as a writer: the standard output. On Windows a large frame that is all ASCII (a picture's bytes) goes
/// through a second handle to the console instead, which takes a whole buffer in one call where the standard output
/// hands a console over in pieces of a few kilobytes, one call each, and a Sixel is a few hundred kilobytes. Every other
/// frame, and any text that is not ASCII, goes through the standard output, which converts text to what the console
/// draws whatever its code page is (the second handle would not). Either way the stream is in order: the standard output
/// is flushed before the other handle is used, and each write is flushed by [`Display`].
#[derive(Debug)]
pub struct TerminalOut {
    stdout: std::io::Stdout,
    #[cfg(windows)]
    whole: Option<std::fs::File>,
}

impl TerminalOut {
    pub fn open() -> Self {
        Self {
            stdout: std::io::stdout(),
            #[cfg(windows)]
            whole: {
                use std::os::windows::io::AsHandle;
                std::io::stdout()
                    .as_handle()
                    .try_clone_to_owned()
                    .ok()
                    .map(std::fs::File::from)
            },
        }
    }
}

impl Write for TerminalOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stdout.write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        #[cfg(windows)]
        if let Some(whole) = &mut self.whole
            && fits_whole_write(buf)
        {
            self.stdout.flush()?;
            return whole.write_all(buf);
        }
        self.stdout.write_all(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stdout.flush()
    }
}

#[cfg(test)]
mod tests;
