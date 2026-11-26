//! Pictures drawn with the terminal's own graphics protocol (Sixel, the Kitty graphics protocol, iTerm2's inline
//! images) where the terminal has one, and as block art (`imgart.rs`) everywhere else.
//!
//! The pieces, each testable without a terminal:
//!
//! - [`detect`]: what the environment and the terminal's replies say about the terminal, and the strict parser for those
//!   replies (terminal replies are untrusted input);
//! - [`probe`]: asking the terminal, once, before the event loop starts;
//! - [`encode`]: a picture to the bytes a protocol wants (the Sixel encoder is our own, on the `image` crate we already
//!   have);
//! - [`plan`]: where on the screen a picture may be drawn with graphics (pure rules about rectangles);
//! - [`present`]: writing what the plan says, once, and taking it away again when it no longer holds.
//!
//! Graphics are painted over cells, outside ratatui's diff. The cells a picture covers are marked
//! `CellDiffOption::Skip` in the frame, so ratatui leaves them alone while the picture is up, and the picture's block
//! art stays in the transcript underneath as the fallback for every moment the graphics are not drawn.

#[cfg(test)]
pub(crate) mod decoder;
pub mod detect;
pub mod encode;
pub mod plan;
pub mod present;
pub mod probe;
pub mod report;

use crate::imgart::ImgMode;

/// A terminal graphics protocol.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Protocol {
    Sixel,
    Kitty,
    Iterm,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sixel => "sixel",
            Self::Kitty => "kitty",
            Self::Iterm => "iterm",
        }
    }
}

/// The size of one character cell in pixels: what turns a rectangle of cells into a picture size.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CellPx {
    pub w: u16,
    pub h: u16,
}

impl CellPx {
    /// A cell size, if it is one a terminal could have: a few pixels to a few dozen wide, taller than a few, and no
    /// larger than a huge font on a huge display. Anything else is a misreading and is not used.
    pub fn new(w: u32, h: u32) -> Option<Self> {
        if (3..=128).contains(&w) && (6..=256).contains(&h) {
            Some(Self {
                w: w as u16,
                h: h as u16,
            })
        } else {
            None
        }
    }

    /// `10x20`: the form the `cell_pixels` setting takes.
    pub fn parse(text: &str) -> Option<Self> {
        let (w, h) = text.trim().split_once(['x', 'X'])?;
        let digits = |s: &str| {
            let s = s.trim();
            (!s.is_empty() && s.len() <= 4 && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse::<u32>().ok())
                .flatten()
        };
        Self::new(digits(w)?, digits(h)?)
    }

    pub fn label(self) -> String {
        format!("{}x{}", self.w, self.h)
    }
}

/// What the `images` setting asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageChoice {
    /// The terminal's own graphics when it has them, else block art.
    Auto,
    /// This protocol, whether or not the terminal was found to speak it.
    Protocol(Protocol),
    /// Art made of characters (or nothing), never graphics.
    Art(ImgMode),
}

impl ImageChoice {
    pub const DEFAULT: Self = Self::Auto;

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "sixel" => Some(Self::Protocol(Protocol::Sixel)),
            "kitty" => Some(Self::Protocol(Protocol::Kitty)),
            "iterm" | "iterm2" => Some(Self::Protocol(Protocol::Iterm)),
            other => ImgMode::parse(other).map(Self::Art),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Protocol(p) => p.label(),
            Self::Art(m) => m.label(),
        }
    }

    /// The glyph set for the block art: what shows while graphics are not drawn, and what a choice of art asks for.
    pub fn art_mode(self) -> ImgMode {
        match self {
            Self::Art(m) => m,
            Self::Auto | Self::Protocol(_) => ImgMode::DEFAULT,
        }
    }
}

/// The size of a cell assumed for Windows Terminal when neither it nor the person has said how big its cells are.
/// Windows Terminal's default font at 100 % scaling has cells of about 9 x 19 pixels and larger ones at higher scaling,
/// so this is on the small side on purpose: a picture drawn for cells smaller than the real ones leaves a little blank
/// space round it, while one drawn for cells larger than the real ones would cover the text beside it. A size the
/// terminal reports, or `cell_pixels`, always replaces it.
pub const ASSUMED_WINDOWS_TERMINAL_CELL: CellPx = CellPx { w: 9, h: 18 };

/// A protocol that can be used right now: the terminal speaks it (or the person said it does) and the size of a cell
/// is known, which is what a picture's size in pixels comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Graphics {
    pub protocol: Protocol,
    pub cell: CellPx,
}

/// What can be used given the `images` setting, what detection found, and the cell size. `None` means block art.
pub fn resolve(
    choice: ImageChoice,
    detected: Option<Protocol>,
    cell: Option<CellPx>,
) -> Option<Graphics> {
    let protocol = match choice {
        ImageChoice::Auto => detected?,
        ImageChoice::Protocol(p) => p,
        ImageChoice::Art(_) => return None,
    };
    Some(Graphics {
        protocol,
        cell: cell?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_value_of_the_setting_parses_and_art_values_keep_their_meaning() {
        assert_eq!(ImageChoice::parse("auto"), Some(ImageChoice::Auto));
        assert_eq!(
            ImageChoice::parse(" SIXEL "),
            Some(ImageChoice::Protocol(Protocol::Sixel))
        );
        assert_eq!(
            ImageChoice::parse("kitty"),
            Some(ImageChoice::Protocol(Protocol::Kitty))
        );
        assert_eq!(
            ImageChoice::parse("iterm"),
            Some(ImageChoice::Protocol(Protocol::Iterm))
        );
        for (word, mode) in [
            ("blocks", ImgMode::Blocks),
            ("sextant", ImgMode::Sextant),
            ("octant", ImgMode::Octant),
            ("ascii", ImgMode::Ascii),
            ("off", ImgMode::Off),
        ] {
            assert_eq!(ImageChoice::parse(word), Some(ImageChoice::Art(mode)));
            assert_eq!(ImageChoice::Art(mode).art_mode(), mode);
        }
        assert_eq!(ImageChoice::parse("png"), None);
        assert_eq!(ImageChoice::parse(""), None);
        assert_eq!(ImageChoice::DEFAULT, ImageChoice::Auto);
        assert_eq!(ImageChoice::Auto.art_mode(), ImgMode::Blocks);
    }

    #[test]
    fn a_cell_size_is_what_a_terminal_could_have() {
        assert_eq!(CellPx::parse("10x20"), Some(CellPx { w: 10, h: 20 }));
        assert_eq!(CellPx::parse(" 9 X 19 "), Some(CellPx { w: 9, h: 19 }));
        for bad in [
            "", "10", "x20", "10x", "0x0", "1x1", "10x5", "999x20", "10x999", "-10x20", "10.5x20",
            "10x20x30",
        ] {
            assert_eq!(CellPx::parse(bad), None, "{bad:?}");
        }
        assert_eq!(
            CellPx::new(10, 20).map(CellPx::label).as_deref(),
            Some("10x20")
        );
    }

    #[test]
    fn graphics_need_a_protocol_and_a_cell_size() {
        let cell = CellPx::new(10, 20);
        let sixel = Some(Protocol::Sixel);
        assert_eq!(
            resolve(ImageChoice::Auto, None, cell),
            None,
            "nothing detected"
        );
        assert_eq!(
            resolve(ImageChoice::Auto, sixel, None),
            None,
            "no cell size"
        );
        assert_eq!(
            resolve(ImageChoice::Auto, sixel, cell).map(|g| g.protocol),
            sixel
        );
        assert_eq!(
            resolve(ImageChoice::Protocol(Protocol::Kitty), None, cell).map(|g| g.protocol),
            Some(Protocol::Kitty),
            "forcing needs no detection"
        );
        assert_eq!(
            resolve(ImageChoice::Protocol(Protocol::Kitty), None, None),
            None
        );
        for mode in [ImgMode::Blocks, ImgMode::Off] {
            assert_eq!(
                resolve(ImageChoice::Art(mode), sixel, cell),
                None,
                "art never uses graphics"
            );
        }
    }
}
