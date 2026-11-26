//! A picture as the bytes a graphics protocol wants.
//!
//! Everything here is pure and runs on a worker thread (`app/gfx.rs`), never the UI thread: scaling a large picture and
//! quantising it for Sixel takes tens to hundreds of milliseconds.
//!
//! - **Kitty**: the picture as a PNG, sent in base64 chunks of at most 4096 bytes (`a=t`, transmit without showing, with
//!   `q=2` so the terminal never answers: an answer would arrive on stdin). Showing it is a separate, short command
//!   (`place`), so a picture that moves while scrolling is not sent again.
//! - **iTerm2**: the picture as a PNG in one OSC 1337 `File=` sequence.
//! - **Sixel**: our own encoder (`sixel`): median-cut palette of up to 256 colours, Floyd-Steinberg dithering when the
//!   picture has more colours than that, and run-length bands. There is no crate for this that is both small and
//!   self-contained (the pure-Rust ones bring a colour-science stack of about 35 crates for the same job).
//!
//! The picture is scaled to fit the rectangle of cells it will occupy, never larger than the source and never more than
//! [`MAX_SIDE_PX`] on a side, and a stream that comes out over [`MAX_STREAM_BYTES`] is made again from a smaller one.

use super::Protocol;
use crate::clipboard::base64;
use image::imageops::{FilterType, resize};
use image::{ExtendedColorType, ImageEncoder, RgbImage};
use std::io::Write;
use std::ops::Range;

/// No picture is sent larger than this on either side, whatever the terminal could show.
pub const MAX_SIDE_PX: u32 = 1600;
/// No encoded picture is larger than this many bytes (before a smaller one is tried). A stream goes through the
/// terminal's own parser at a few hundred kilobytes a second, a picture is made again whenever it is scrolled back in, and
/// what a stream holds is a picture too large to read at the size it is drawn at anyway; half a megabyte is already
/// several seconds on a slow link.
pub const MAX_STREAM_BYTES: usize = 512 * 1024;
/// How much base64 one Kitty graphics command carries (the protocol's own limit).
pub const KITTY_CHUNK: usize = 4096;
/// The most colours a Sixel palette is given. Terminals have at least 256 registers.
const SIXEL_COLOURS: usize = 256;
/// The size a Sixel is aimed at, in bytes (see [sixel_build]).
pub const SIXEL_TARGET_BYTES: usize = 128 * 1024;
/// How many times a stream that is too large is made again at 70 % of the size. Enough that the very worst picture (noise
/// that neither a palette of 32 colours nor PNG compression helps) is made small enough: at 8 steps the picture is under a
/// tenth of its side, far fewer pixels than [`MAX_STREAM_BYTES`] holds even uncompressed.
const SHRINKS: usize = 8;

/// A picture ready to be written.
pub struct Encoded {
    /// Unique among the pictures encoded in a session; what the presenter tells pictures apart by. The Kitty image id is
    /// derived from it.
    pub id: u64,
    pub protocol: Protocol,
    /// The size of the picture sent, in pixels (at most the rectangle it was made for).
    pub size: (u32, u32),
    /// What to write at the cursor (Sixel, iTerm2), or what transmits the picture to the terminal (Kitty).
    pub stream: Vec<u8>,
    /// Where the pieces of a Sixel stream are, so that a part of the picture can be written without making it again.
    pub layout: Option<SixelLayout>,
}

/// Only the facts about the stream: its bytes are far too long to print.
impl std::fmt::Debug for Encoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Encoded")
            .field("id", &self.id)
            .field("protocol", &self.protocol)
            .field("size", &self.size)
            .field("bytes", &self.stream.len())
            .finish()
    }
}

impl Encoded {
    /// The id the terminal knows a Kitty picture by.
    pub fn kitty_id(&self) -> u32 {
        kitty_id_of(self.id)
    }

    /// The rows of the picture, in its own pixels, that can be drawn when `wanted` is the part inside the screen: what
    /// the protocol can show of it, or `None` when it can show none of `wanted` (a partly visible picture in iTerm2,
    /// which cannot crop; a part too thin to hold a whole six-pixel band of a Sixel).
    ///
    /// A Sixel is drawn in whole bands of six rows, so the rows are rounded in: the first up to the next band, the last
    /// down to the one before. Up to five rows of the picture's edge, inside the cell at the cut, stay blank.
    pub fn visible_rows(&self, wanted: Range<u32>) -> Option<Range<u32>> {
        let height = self.size.1;
        let rows = wanted.start..wanted.end.min(height);
        if rows.start >= rows.end {
            return None;
        }
        let whole = rows.start == 0 && rows.end >= height;
        match self.protocol {
            Protocol::Sixel if whole => Some(0..height),
            Protocol::Sixel => {
                let bands = self.layout.as_ref()?.bands_within(&rows)?;
                Some(self.layout.as_ref()?.rows_of(&bands))
            }
            Protocol::Kitty => Some(rows),
            Protocol::Iterm => whole.then_some(0..height),
        }
    }

    /// What to write at the cursor to show `rows` (as given by [`visible_rows`](Self::visible_rows)) of a Sixel or
    /// iTerm2 picture: the whole stream, or for a part of a Sixel the header with its height changed, the bands of those
    /// rows and the terminator. No picture is made again.
    pub fn stream_for(&self, rows: &Range<u32>) -> std::borrow::Cow<'_, [u8]> {
        use std::borrow::Cow;
        match (&self.layout, self.protocol) {
            (Some(layout), Protocol::Sixel) if !(rows.start == 0 && rows.end >= self.size.1) => {
                Cow::Owned(layout.slice(&self.stream, rows))
            }
            _ => Cow::Borrowed(&self.stream),
        }
    }
}

/// Where the parts of a Sixel stream made by [`sixel_build`] are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SixelLayout {
    /// Pixel rows in the picture.
    pub height: usize,
    /// The digits of the height in the raster attributes, which a part of the picture needs changed.
    raster_height: Range<usize>,
    /// The offset of each band of six rows, and then of the terminator: one more than there are bands. The header (the
    /// introducer, the raster attributes and the palette) is everything before the first.
    bands: Vec<usize>,
}

impl SixelLayout {
    /// How many bands there are.
    pub fn band_count(&self) -> usize {
        self.bands.len().saturating_sub(1)
    }

    /// The bands that lie wholly inside the pixel `rows`, if there is one. The last band of the picture counts as whole
    /// when the rows reach the picture's end.
    pub fn bands_within(&self, rows: &Range<u32>) -> Option<Range<usize>> {
        let first = (rows.start as usize).div_ceil(6);
        let end = if rows.end as usize >= self.height {
            self.band_count()
        } else {
            rows.end as usize / 6
        };
        (first < end).then_some(first..end)
    }

    /// The pixel rows of the picture that `bands` draw.
    pub fn rows_of(&self, bands: &Range<usize>) -> Range<u32> {
        let from = (bands.start * 6).min(self.height);
        let to = (bands.end * 6).min(self.height);
        from as u32..to as u32
    }

    /// The stream for the pixel `rows` (band aligned, as [`Self::rows_of`] gives them) of `stream`, the one this layout
    /// describes.
    pub fn slice(&self, stream: &[u8], rows: &Range<u32>) -> Vec<u8> {
        let first = (rows.start as usize / 6).min(self.band_count());
        let end = (rows.end as usize)
            .div_ceil(6)
            .clamp(first, self.band_count());
        let height = (rows.end - rows.start) as usize;
        let mut out = Vec::with_capacity(self.bands[end] - self.bands[first] + self.bands[0] + 8);
        out.extend_from_slice(&stream[..self.raster_height.start]);
        let _ = write!(out, "{height}");
        out.extend_from_slice(&stream[self.raster_height.end..self.bands[0]]);
        out.extend_from_slice(&stream[self.bands[first]..self.bands[end]]);
        out.extend_from_slice(&stream[self.bands[self.band_count()]..]);
        out
    }
}

/// Why a picture was not encoded.
#[derive(Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// The picture could not be written as a PNG.
    Png,
    /// It was still over the size limit after being made smaller several times.
    TooLarge,
    /// The encoder itself failed unexpectedly (a bug).
    Panicked,
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Png => f.write_str("the picture could not be written as a PNG"),
            Self::TooLarge => f.write_str("the encoded picture stayed over the size limit"),
            Self::Panicked => f.write_str("the encoder failed unexpectedly; this is a bug"),
        }
    }
}

/// Encode `img` for `protocol`, to fit inside `max` pixels.
pub fn encode(
    img: &RgbImage,
    protocol: Protocol,
    max: (u32, u32),
    id: u64,
) -> Result<Encoded, EncodeError> {
    encode_capped(img, protocol, max, id, MAX_STREAM_BYTES)
}

/// [`encode`] with the byte limit given (the tests use small ones).
pub fn encode_capped(
    img: &RgbImage,
    protocol: Protocol,
    max: (u32, u32),
    id: u64,
    limit: usize,
) -> Result<Encoded, EncodeError> {
    let mut max = (max.0.clamp(1, MAX_SIDE_PX), max.1.clamp(1, MAX_SIDE_PX));
    for _ in 0..=SHRINKS {
        let size = target_size(img.dimensions(), max, protocol == Protocol::Sixel);
        let scaled = if size == img.dimensions() {
            img.clone()
        } else {
            resize(img, size.0, size.1, FilterType::Lanczos3)
        };
        let (stream, layout) = match protocol {
            Protocol::Sixel => {
                let (stream, layout) = sixel_build(&scaled, SIXEL_TARGET_BYTES);
                (stream, Some(layout))
            }
            Protocol::Kitty => (kitty_transmit(&png(&scaled)?, kitty_id_of(id)), None),
            Protocol::Iterm => (iterm_inline(&png(&scaled)?, size), None),
        };
        if stream.len() <= limit {
            return Ok(Encoded {
                id,
                protocol,
                size,
                stream,
                layout,
            });
        }
        max = (
            (f64::from(max.0) * 0.7).floor().max(1.0) as u32,
            (f64::from(max.1) * 0.7).floor().max(1.0) as u32,
        );
    }
    Err(EncodeError::TooLarge)
}

/// The id the terminal knows a Kitty picture by: a non-zero 32-bit number, exact for the first four billion pictures of a
/// session (ids come from a counter that starts at 1).
fn kitty_id_of(id: u64) -> u32 {
    (id as u32).max(1)
}

/// The size a `src` picture is drawn at inside `max`: scaled down to fit, aspect kept, never enlarged. For Sixel the
/// height is a whole number of 6-pixel bands (when it is at least two), so that no band hangs below the picture into the
/// next row of cells.
pub fn target_size(src: (u32, u32), max: (u32, u32), sixel: bool) -> (u32, u32) {
    let (sw, sh) = (f64::from(src.0.max(1)), f64::from(src.1.max(1)));
    let (mw, mh) = (f64::from(max.0.max(1)), f64::from(max.1.max(1)));
    let scale = (mw / sw).min(mh / sh).min(1.0);
    let mut w = (sw * scale).round().max(1.0);
    let mut h = (sh * scale).round().max(1.0);
    if sixel && h >= 12.0 {
        let whole = (h / 6.0).floor() * 6.0;
        w = (w * whole / h).round().max(1.0);
        h = whole;
    }
    (w.min(mw) as u32, h.min(mh) as u32)
}

fn png(img: &RgbImage) -> Result<Vec<u8>, EncodeError> {
    use image::codecs::png::{CompressionType, FilterType as Filter, PngEncoder};
    let mut out = Vec::new();
    PngEncoder::new_with_quality(&mut out, CompressionType::Fast, Filter::Adaptive)
        .write_image(
            img.as_raw(),
            img.width(),
            img.height(),
            ExtendedColorType::Rgb8,
        )
        .map_err(|_| EncodeError::Png)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Kitty
// ---------------------------------------------------------------------------

/// The commands that give the terminal a PNG under `image_id` without showing it: one APC string per 4096 bytes of base64,
/// `m=1` on every one but the last. `q=2` on the first asks the terminal to say nothing back, errors included.
pub fn kitty_transmit(png: &[u8], image_id: u32) -> Vec<u8> {
    let data = base64(png);
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(KITTY_CHUNK).collect();
    let mut out = Vec::with_capacity(data.len() + chunks.len() * 40);
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            let _ = write!(out, "\x1b_Ga=t,t=d,f=100,i={image_id},q=2,m={more};");
        } else {
            let _ = write!(out, "\x1b_Gm={more};");
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// Show the picture sent as `image_id` at the cursor, with the given placement id (so it can be moved or taken away by id). With
/// `keep_cursor` the cursor stays where it is (`C=1`).
pub fn kitty_place(
    image_id: u32,
    placement_id: u32,
    keep_cursor: bool,
    crop: Option<Crop>,
) -> Vec<u8> {
    let crop = crop.map_or_else(String::new, |c| {
        format!(",x={},y={},w={},h={}", c.x, c.y, c.width, c.height)
    });
    // `z=-1`: under the text, which is drawn over the picture (the labels of the transcript), and still over the cells'
    // backgrounds (only below -1073741824 does a picture go under those).
    format!(
        "\x1b_Ga=p,i={image_id},p={placement_id},q=2{}{crop},z=-1\x1b\\",
        if keep_cursor { ",C=1" } else { "" }
    )
    .into_bytes()
}

/// The part of a picture a Kitty placement shows, in the picture's own pixels (`x`, `y`, `w` and `h` of the command): the
/// terminal draws just that rectangle, at its own size, from the data it already holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Take the given placement of the picture away; with `free` the terminal also forgets the picture's data.
pub fn kitty_delete(image_id: u32, placement_id: u32, free: bool) -> Vec<u8> {
    format!(
        "\x1b_Ga=d,d={},i={image_id},p={placement_id},q=2\x1b\\",
        if free { 'I' } else { 'i' }
    )
    .into_bytes()
}

/// Take away every picture (placements and data) this program has put in the terminal, the panic hook's cleanup.
pub const KITTY_DELETE_ALL: &[u8] = b"\x1b_Ga=d,d=A,q=2\x1b\\";

// ---------------------------------------------------------------------------
// iTerm2
// ---------------------------------------------------------------------------

/// iTerm2's inline image: `OSC 1337 ; File=inline=1;... : base64 ST`. The size is given in pixels, which is what the
/// picture was made for, and the aspect is kept in case the terminal rounds.
pub fn iterm_inline(png: &[u8], size: (u32, u32)) -> Vec<u8> {
    format!(
        "\x1b]1337;File=inline=1;size={};width={}px;height={}px;preserveAspectRatio=1:{}\x1b\\",
        png.len(),
        size.0,
        size.1,
        base64(png)
    )
    .into_bytes()
}

// ---------------------------------------------------------------------------
// Sixel
// ---------------------------------------------------------------------------

/// One distinct colour (at 5 bits a channel) in the picture: how many pixels, and the sum of their real values.
#[derive(Clone, Copy)]
struct Bin {
    key: u16,
    n: u32,
    sum: [u32; 3],
}

/// The 15-bit colour of a pixel: 5 bits a channel.
fn key15(p: [u8; 3]) -> usize {
    (usize::from(p[0] >> 3) << 10) | (usize::from(p[1] >> 3) << 5) | usize::from(p[2] >> 3)
}

/// The 5-bit channel of a key: 0 red, 1 green, 2 blue.
fn channel(key: u16, c: usize) -> u16 {
    (key >> (10 - 5 * c)) & 31
}

/// The middle of a key's colour cell, as 8-bit values.
fn centre(key: usize) -> [i32; 3] {
    let at = |c: usize| (((key >> (10 - 5 * c)) & 31) as i32) * 8 + 4;
    [at(0), at(1), at(2)]
}

/// A palette of at most `limit` colours for `img` and each pixel's index into it. A picture with no more colours than that
/// (at 5 bits a channel) keeps them all and is not dithered; otherwise the colours are cut into `limit` boxes by median
/// cut, each box becomes its average, and pixels are mapped with Floyd-Steinberg error diffusion.
fn quantise(img: &RgbImage, limit: usize, dither: i32) -> (Vec<[u8; 3]>, Vec<u8>) {
    let mut histogram = vec![[0u32; 4]; 1 << 15];
    for p in img.pixels() {
        let h = &mut histogram[key15(p.0)];
        h[0] += 1;
        for c in 0..3 {
            h[c + 1] += u32::from(p.0[c]);
        }
    }
    let mut bins: Vec<Bin> = histogram
        .iter()
        .enumerate()
        .filter(|(_, h)| h[0] > 0)
        .map(|(key, h)| Bin {
            key: key as u16,
            n: h[0],
            sum: [h[1], h[2], h[3]],
        })
        .collect();
    let average = |bins: &[Bin]| {
        let n: u64 = bins.iter().map(|b| u64::from(b.n)).sum::<u64>().max(1);
        let mut out = [0u8; 3];
        for (c, slot) in out.iter_mut().enumerate() {
            let sum: u64 = bins.iter().map(|b| u64::from(b.sum[c])).sum();
            *slot = ((sum + n / 2) / n).min(255) as u8;
        }
        out
    };

    if bins.len() <= limit {
        let palette: Vec<[u8; 3]> = bins
            .iter()
            .map(|b| average(std::slice::from_ref(b)))
            .collect();
        let mut index_of = vec![0u8; 1 << 15];
        for (i, b) in bins.iter().enumerate() {
            index_of[usize::from(b.key)] = i as u8;
        }
        let indices = img.pixels().map(|p| index_of[key15(p.0)]).collect();
        return (palette, indices);
    }

    // Median cut over the bins: boxes are ranges of `bins`, split along their widest channel at the pixel median.
    let mut boxes: Vec<(usize, usize)> = vec![(0, bins.len())];
    let extent = |bins: &[Bin]| {
        let mut lo = [31u16; 3];
        let mut hi = [0u16; 3];
        for b in bins {
            for c in 0..3 {
                let v = channel(b.key, c);
                lo[c] = lo[c].min(v);
                hi[c] = hi[c].max(v);
            }
        }
        [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]]
    };
    let weight = |(a, b): (usize, usize), bins: &[Bin]| -> u64 {
        if b - a < 2 {
            return 0;
        }
        let range = extent(&bins[a..b]).into_iter().max().unwrap_or(0);
        let n: u64 = bins[a..b].iter().map(|x| u64::from(x.n)).sum();
        u64::from(range) * n
    };
    while boxes.len() < limit {
        let Some((at, _)) = boxes
            .iter()
            .enumerate()
            .map(|(i, r)| (i, weight(*r, &bins)))
            .filter(|(_, w)| *w > 0)
            .max_by_key(|(_, w)| *w)
        else {
            break;
        };
        let (a, b) = boxes[at];
        let widths = extent(&bins[a..b]);
        let widest = (0..3).max_by_key(|c| widths[*c]).unwrap_or(0);
        bins[a..b].sort_unstable_by_key(|x| (channel(x.key, widest), x.key));
        let total: u64 = bins[a..b].iter().map(|x| u64::from(x.n)).sum();
        let mut seen = 0u64;
        let mut cut = a + 1;
        for (i, x) in bins[a..b].iter().enumerate() {
            seen += u64::from(x.n);
            if seen * 2 >= total {
                cut = a + i + 1;
                break;
            }
        }
        let cut = cut.clamp(a + 1, b - 1);
        boxes[at] = (a, cut);
        boxes.push((cut, b));
    }
    let palette: Vec<[u8; 3]> = boxes.iter().map(|&(a, b)| average(&bins[a..b])).collect();

    // Nearest palette entry for each 15-bit colour, worked out the first time it is needed.
    let mut nearest = vec![u16::MAX; 1 << 15];
    let mut lookup = |key: usize| -> u8 {
        if nearest[key] == u16::MAX {
            let c = centre(key);
            let best = palette
                .iter()
                .enumerate()
                .min_by_key(|(_, p)| {
                    let d = |i: usize| i64::from(c[i] - i32::from(p[i]));
                    2 * d(0) * d(0) + 4 * d(1) * d(1) + 3 * d(2) * d(2)
                })
                .map_or(0, |(i, _)| i);
            nearest[key] = best as u16;
        }
        nearest[key] as u8
    };

    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut indices = vec![0u8; w * h];
    // Error carried to the next pixel and to the next row, in sixteenths of a level, a channel each.
    let mut this_row = vec![[0i32; 3]; w + 2];
    let mut next_row = vec![[0i32; 3]; w + 2];
    for y in 0..h {
        for x in 0..w {
            let p = img.get_pixel(x as u32, y as u32).0;
            let mut v = [0i32; 3];
            for c in 0..3 {
                v[c] = (i32::from(p[c]) + this_row[x + 1][c] / 16).clamp(0, 255);
            }
            let index = lookup(key15([v[0] as u8, v[1] as u8, v[2] as u8]));
            indices[y * w + x] = index;
            let chosen = palette[usize::from(index)];
            for c in 0..3 {
                let err = (v[c] - i32::from(chosen[c])).clamp(-48, 48) * dither / 8;
                this_row[x + 2][c] += err * 7;
                next_row[x][c] += err * 3;
                next_row[x + 1][c] += err * 5;
                next_row[x + 2][c] += err;
            }
        }
        std::mem::swap(&mut this_row, &mut next_row);
        next_row.iter_mut().for_each(|e| *e = [0; 3]);
    }
    (palette, indices)
}

/// A repeat in Sixel data: `!n c` for a run longer than three, else the character itself.
fn run(out: &mut Vec<u8>, ch: u8, n: usize) {
    if n > 3 {
        let _ = write!(out, "!{n}");
        out.push(ch);
    } else {
        out.extend(std::iter::repeat_n(ch, n));
    }
}

/// `img` as a Sixel image: `ESC P 9 ; 1 ; 0 q`, raster attributes, the palette, then the six-pixel bands, each colour of
/// a band as one run-length-coded row; `ESC \` ends it.
///
/// `9` is the pixel aspect ratio 1:1 (Windows Terminal draws a different aspect when it is left at 0); the `1` after it
/// leaves pixels no colour is drawn on as they were.
#[cfg(test)]
pub fn sixel(img: &RgbImage) -> Vec<u8> {
    sixel_within(img, SIXEL_TARGET_BYTES)
}

/// [`sixel`] with the size aimed at: the palette is made smaller (256, 128, 64, 32 colours) until the stream is no larger
/// than `target`, or is as small as that goes. A terminal takes a Sixel's bytes through the console and the terminal's own
/// parser at a few hundred kilobytes a second, so what a picture costs on the screen is mostly its size; a photograph
/// at 256 colours is a quarter of a megabyte, at 64 about half that, and looks the same at the size it is drawn.
#[cfg(test)]
pub fn sixel_within(img: &RgbImage, target: usize) -> Vec<u8> {
    sixel_build(img, target).0
}

/// A Sixel for `img` aimed at `target` bytes (see `SIXEL_TARGET_BYTES`), with where the parts of the stream are.
pub fn sixel_build(img: &RgbImage, target: usize) -> (Vec<u8>, SixelLayout) {
    let mut best = sixel_made(img, SIXEL_COLOURS, 8);
    for colours in [128, 64, 32] {
        if best.0.len() <= target {
            break;
        }
        best = sixel_made(img, colours, 8);
    }
    best
}

/// [sixel] with the palette size and the dithering strength (in eighths, 0 to 8) given.
#[cfg(test)]
pub fn sixel_with(img: &RgbImage, colours: usize, dither: i32) -> Vec<u8> {
    sixel_made(img, colours, dither).0
}

fn sixel_made(img: &RgbImage, colours: usize, dither: i32) -> (Vec<u8>, SixelLayout) {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let (palette, indices) = quantise(img, colours, dither);
    let mut out = Vec::with_capacity(w * h / 4 + 4096);
    let _ = write!(out, "\x1bP9;1;0q\"1;1;{w};");
    let height_at = out.len();
    let _ = write!(out, "{h}");
    let raster_height = height_at..out.len();
    let mut bands = Vec::with_capacity(h.div_ceil(6) + 1);
    for (i, c) in palette.iter().enumerate() {
        let pct = |v: u8| (u32::from(v) * 100 + 127) / 255;
        let _ = write!(out, "#{i};2;{};{};{}", pct(c[0]), pct(c[1]), pct(c[2]));
    }
    let mut scratch = vec![0u8; palette.len() * w];
    let mut used = vec![false; palette.len()];
    for band in 0..h.div_ceil(6) {
        // The header is everything before the first band; #0 in its palette is not the start of one.
        bands.push(out.len());
        let top = band * 6;
        for rows in 0..(h - top).min(6) {
            let bit = 1u8 << rows;
            let row = (top + rows) * w;
            for x in 0..w {
                let c = usize::from(indices[row + x]);
                scratch[c * w + x] |= bit;
                used[c] = true;
            }
        }
        let mut first = true;
        for c in 0..palette.len() {
            if !used[c] {
                continue;
            }
            used[c] = false;
            let cells = &mut scratch[c * w..(c + 1) * w];
            if !first {
                out.push(b'$');
            }
            first = false;
            let _ = write!(out, "#{c}");
            let start = cells.iter().position(|b| *b != 0).unwrap_or(0);
            let end = cells.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
            if start > 0 {
                run(&mut out, b'?', start);
            }
            let mut x = start;
            while x < end {
                let bits = cells[x];
                let mut n = 1;
                while x + n < end && cells[x + n] == bits {
                    n += 1;
                }
                run(&mut out, 63 + bits, n);
                x += n;
            }
            cells.fill(0);
        }
        out.push(b'-');
    }
    bands.push(out.len());
    out.extend_from_slice(b"\x1b\\");
    (
        out,
        SixelLayout {
            height: h,
            raster_height,
            bands,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::decoder::{Decoded, decode_sixel};

    /// A synthetic picture: smooth colour in both directions, so it has far more than 256 colours.
    fn gradient(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([
                (x * 255 / w.max(2).saturating_sub(1).max(1)) as u8,
                (y * 255 / h.max(2).saturating_sub(1).max(1)) as u8,
                ((x + y) * 255 / (w + h).max(2).saturating_sub(2).max(1)) as u8,
            ])
        })
    }

    /// Four flat colours, so that a Sixel palette can hold them exactly.
    fn flat(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| {
            image::Rgb(match (x < w / 2, y < h / 2) {
                (true, true) => [200, 30, 30],
                (false, true) => [30, 200, 30],
                (true, false) => [30, 30, 200],
                (false, false) => [240, 240, 80],
            })
        })
    }

    fn mean_error(img: &RgbImage, decoded: &Decoded) -> f64 {
        let mut total = 0u64;
        for (i, p) in img.pixels().enumerate() {
            let got = decoded.pixels[i].expect("every pixel of the picture is drawn");
            for (a, b) in p.0.iter().zip(got) {
                total += u64::from(a.abs_diff(b));
            }
        }
        total as f64 / (img.pixels().len() * 3) as f64
    }

    #[test]
    fn a_picture_with_few_colours_comes_back_exactly_from_its_sixel() {
        let img = flat(30, 18);
        let sixel = sixel(&img);
        let decoded = decode_sixel(&sixel);
        assert_eq!(decoded.size, (30, 18));
        assert_eq!(
            decoded.palette.len(),
            4,
            "one register for each colour, no more"
        );
        for (i, p) in img.pixels().enumerate() {
            let got = decoded.pixels[i].expect("drawn");
            for c in 0..3 {
                assert!(
                    p.0[c].abs_diff(got[c]) <= 2,
                    "pixel {i}: {:?} vs {got:?}",
                    p.0
                );
            }
        }
    }

    #[test]
    fn a_smooth_picture_is_cut_to_256_colours_and_stays_close() {
        let img = gradient(96, 60);
        let sixel = sixel(&img);
        let decoded = decode_sixel(&sixel);
        assert_eq!(decoded.size, (96, 60));
        assert!(
            decoded.palette.len() <= 256,
            "{} colours",
            decoded.palette.len()
        );
        assert!(
            decoded.palette.len() > 64,
            "a gradient needs many of them: {}",
            decoded.palette.len()
        );
        let error = mean_error(&img, &decoded);
        assert!(error < 6.0, "mean error per channel {error:.2} levels");
    }

    #[test]
    fn the_sixel_has_one_band_for_each_six_rows_and_only_stream_characters() {
        for h in [6u32, 12, 13, 30, 31] {
            let img = flat(16, h);
            let sixel = sixel(&img);
            let decoded = decode_sixel(&sixel);
            assert_eq!(decoded.bands, (h as usize).div_ceil(6), "height {h}");
            let text = std::str::from_utf8(&sixel).expect("ASCII only");
            assert!(
                text.bytes().all(|b| b == 0x1b || (0x20..0x7f).contains(&b)),
                "no control character but ESC"
            );
            assert_eq!(
                text.matches('\x1b').count(),
                2,
                "one introducer, one terminator"
            );
        }
        let one = sixel(&RgbImage::from_pixel(1, 1, image::Rgb([10, 20, 30])));
        assert_eq!(decode_sixel(&one).size, (1, 1));
    }

    #[test]
    fn a_large_stream_is_made_smaller_with_fewer_colours_and_a_small_one_keeps_them_all() {
        let mut seed = 5u32;
        let noisy = RgbImage::from_fn(120, 120, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            image::Rgb([(seed >> 8) as u8, (seed >> 16) as u8, (seed >> 24) as u8])
        });
        let full = sixel_within(&noisy, usize::MAX);
        let small = sixel_within(&noisy, full.len() / 3);
        assert!(
            small.len() < full.len() / 2,
            "{} vs {}",
            small.len(),
            full.len()
        );
        assert!(decode_sixel(&small).palette.len() <= 128);
        assert!(
            decode_sixel(&full).palette.len() > 128,
            "the first try keeps 256"
        );
        // A picture already under the target is not touched.
        let flat = flat(40, 24);
        assert_eq!(
            sixel_within(&flat, SIXEL_TARGET_BYTES),
            sixel_with(&flat, SIXEL_COLOURS, 8)
        );
        // However small the aim, the ladder ends.
        let tiny = sixel_within(&noisy, 1);
        assert!(decode_sixel(&tiny).palette.len() <= 32);
    }

    /// The rows `from..to` of a decoded Sixel, as its pixels.
    fn rows_of(decoded: &Decoded, from: usize, to: usize) -> Vec<Option<[u8; 3]>> {
        let w = decoded.size.0;
        decoded.pixels[from * w..to * w].to_vec()
    }

    #[test]
    fn a_part_of_a_sixel_is_its_header_and_the_bands_of_those_rows_with_nothing_made_again() {
        let img = gradient(40, 60);
        let e = encode(&img, Protocol::Sixel, (1000, 1000), 1).unwrap();
        assert_eq!(e.size, (40, 60));
        let full = decode_sixel(&e.stream);
        let layout = e.layout.as_ref().unwrap();
        assert_eq!(layout.band_count(), 10);
        // Rows 14..45 hold the whole bands 3..7, which are the rows 18..42: the first rounded in, the last rounded down.
        let rows = e.visible_rows(14..45).unwrap();
        assert_eq!(rows, 18..42);
        let part = e.stream_for(&rows);
        let decoded = decode_sixel(&part);
        assert_eq!(
            decoded.size,
            (40, 24),
            "the height in the raster attributes follows"
        );
        assert_eq!(decoded.bands, 4);
        assert_eq!(
            decoded.pixels,
            rows_of(&full, 18, 42),
            "exactly those rows of the whole picture"
        );
        assert_eq!(
            decoded.palette, full.palette,
            "the palette is the whole one"
        );
        assert!(part.len() < e.stream.len());
        // Cut at the top only, at the bottom only, and an edge that is already on a band.
        let rows = e.visible_rows(13..60).unwrap();
        assert_eq!(rows, 18..60);
        assert_eq!(
            decode_sixel(&e.stream_for(&rows)).pixels,
            rows_of(&full, 18, 60)
        );
        let rows = e.visible_rows(0..31).unwrap();
        assert_eq!(rows, 0..30);
        assert_eq!(
            decode_sixel(&e.stream_for(&rows)).pixels,
            rows_of(&full, 0, 30)
        );
        assert_eq!(e.visible_rows(12..36).unwrap(), 12..36);
        // The whole picture is the stream as it was made, not a copy.
        let all = e.visible_rows(0..60).unwrap();
        assert_eq!(all, 0..60);
        assert!(matches!(e.stream_for(&all), std::borrow::Cow::Borrowed(_)));
        assert_eq!(
            e.visible_rows(0..600).unwrap(),
            0..60,
            "rows past the end are the end"
        );
        // Too thin to hold a whole band, or nothing at all.
        for thin in [7..11, 0..5, 13..18, 55..59, 30..30, 70..80] {
            assert_eq!(e.visible_rows(thin.clone()), None, "{thin:?}");
        }
    }

    #[test]
    fn the_short_last_band_of_a_small_sixel_counts_when_the_picture_ends() {
        let img = gradient(40, 8);
        let e = encode(&img, Protocol::Sixel, (1000, 1000), 1).unwrap();
        assert_eq!(e.size, (40, 8), "too short for whole bands, so as it is");
        let full = decode_sixel(&e.stream);
        let rows = e.visible_rows(2..8).unwrap();
        assert_eq!(rows, 6..8);
        let part = decode_sixel(&e.stream_for(&rows));
        assert_eq!(part.size, (40, 2));
        assert_eq!(part.pixels, rows_of(&full, 6, 8));
        assert_eq!(e.visible_rows(0..6).unwrap(), 0..6);
        assert_eq!(
            e.visible_rows(0..7),
            Some(0..6),
            "not the second band: it is not all inside"
        );
    }

    #[test]
    fn kitty_crops_to_the_pixel_row_and_iterm_cannot_crop() {
        let img = gradient(40, 60);
        let kitty = encode(&img, Protocol::Kitty, (1000, 1000), 1).unwrap();
        assert_eq!(kitty.visible_rows(13..47), Some(13..47));
        assert_eq!(kitty.visible_rows(0..600), Some(0..60));
        assert_eq!(kitty.visible_rows(5..5), None);
        let iterm = encode(&img, Protocol::Iterm, (1000, 1000), 2).unwrap();
        assert_eq!(iterm.visible_rows(0..60), Some(0..60));
        assert_eq!(iterm.visible_rows(0..600), Some(0..60));
        assert_eq!(
            iterm.visible_rows(1..60),
            None,
            "no part of an iTerm2 picture"
        );
        assert_eq!(iterm.visible_rows(0..59), None);
    }

    #[test]
    fn a_noisy_picture_still_decodes_within_its_declared_size() {
        let mut seed = 12345u32;
        let img = RgbImage::from_fn(40, 31, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            image::Rgb([(seed >> 8) as u8, (seed >> 16) as u8, (seed >> 24) as u8])
        });
        let decoded = decode_sixel(&sixel(&img));
        assert_eq!(decoded.size, (40, 31));
        assert!(decoded.pixels.iter().all(Option::is_some));
        assert!(decoded.palette.len() <= 256);
    }

    #[test]
    fn a_picture_is_scaled_down_to_the_rectangle_and_never_up() {
        assert_eq!(target_size((1000, 500), (400, 400), false), (400, 200));
        assert_eq!(target_size((500, 1000), (400, 400), false), (200, 400));
        assert_eq!(
            target_size((100, 50), (400, 400), false),
            (100, 50),
            "never enlarged"
        );
        assert_eq!(target_size((1, 1), (400, 400), false), (1, 1));
        // Sixel: whole bands, and the width follows so the shape is kept.
        let (w, h) = target_size((1000, 500), (400, 400), true);
        assert_eq!(h % 6, 0);
        assert!((192..=200).contains(&h) && w <= 400, "{w}x{h}");
        assert_eq!(
            target_size((100, 7), (400, 400), true),
            (100, 7),
            "too short for whole bands"
        );
        for src in [(3000, 17), (17, 3000), (1600, 1600), (5, 5), (4000, 3000)] {
            for max in [(1, 1), (10, 10), (333, 217), (1600, 1600)] {
                for sixel in [false, true] {
                    let (w, h) = target_size(src, max, sixel);
                    assert!(
                        w >= 1 && h >= 1 && w <= max.0 && h <= max.1,
                        "{src:?} in {max:?}: {w}x{h}"
                    );
                }
            }
        }
    }

    fn decode_png(png: &[u8]) -> RgbImage {
        image::load_from_memory_with_format(png, image::ImageFormat::Png)
            .unwrap()
            .to_rgb8()
    }

    /// The base64 of every chunk of a Kitty transmission, checked as it goes.
    fn kitty_payload(stream: &[u8]) -> (String, Vec<String>) {
        let text = std::str::from_utf8(stream).expect("ASCII");
        let commands: Vec<&str> = text.split("\x1b\\").filter(|c| !c.is_empty()).collect();
        let mut payload = String::new();
        let mut controls = Vec::new();
        for (i, command) in commands.iter().enumerate() {
            let body = command
                .strip_prefix("\x1b_G")
                .expect("an APC graphics command");
            let (control, data) = body.split_once(';').expect("control;data");
            assert!(data.len() <= KITTY_CHUNK, "chunk of {} bytes", data.len());
            assert!(
                data.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='),
                "base64 only"
            );
            let last = i + 1 == commands.len();
            assert!(
                control.contains(if last { "m=0" } else { "m=1" }),
                "chunk {i}: {control}"
            );
            if !last {
                assert_eq!(data.len(), KITTY_CHUNK, "every chunk but the last is full");
            }
            payload.push_str(data);
            controls.push(control.to_string());
        }
        (payload, controls)
    }

    #[test]
    fn a_kitty_transmission_is_chunked_and_decodes_back_to_the_picture() {
        let img = gradient(150, 100);
        let encoded = encode(&img, Protocol::Kitty, (1000, 1000), 7).unwrap();
        assert_eq!(encoded.size, (150, 100));
        let (payload, controls) = kitty_payload(&encoded.stream);
        assert!(controls.len() > 1, "a gradient does not fit one chunk");
        assert!(
            controls[0].starts_with("a=t,t=d,f=100,i=7,q=2,m="),
            "{}",
            controls[0]
        );
        for later in &controls[1..] {
            assert!(
                later.starts_with("m="),
                "later chunks carry only m: {later}"
            );
        }
        let png = base64_decode(&payload);
        assert_eq!(decode_png(&png), img, "PNG is lossless");
    }

    #[test]
    fn a_kitty_picture_that_fits_one_chunk_is_one_command_and_commands_are_exact() {
        let img = RgbImage::from_pixel(4, 4, image::Rgb([1, 2, 3]));
        let encoded = encode(&img, Protocol::Kitty, (100, 100), 3).unwrap();
        let text = String::from_utf8(encoded.stream).unwrap();
        assert_eq!(text.matches("\x1b_G").count(), 1);
        assert!(text.starts_with("\x1b_Ga=t,t=d,f=100,i=3,q=2,m=0;"));
        assert_eq!(
            kitty_place(3, 1, true, None),
            b"\x1b_Ga=p,i=3,p=1,q=2,C=1,z=-1\x1b\\"
        );
        assert_eq!(
            kitty_place(3, 1, false, None),
            b"\x1b_Ga=p,i=3,p=1,q=2,z=-1\x1b\\"
        );
        let crop = Crop {
            x: 0,
            y: 40,
            width: 300,
            height: 80,
        };
        assert_eq!(
            kitty_place(3, 1, true, Some(crop)),
            b"\x1b_Ga=p,i=3,p=1,q=2,C=1,x=0,y=40,w=300,h=80,z=-1\x1b\\"
        );
        assert_eq!(
            kitty_delete(3, 1, false),
            b"\x1b_Ga=d,d=i,i=3,p=1,q=2\x1b\\"
        );
        assert_eq!(kitty_delete(3, 1, true), b"\x1b_Ga=d,d=I,i=3,p=1,q=2\x1b\\");
        assert!(
            [
                kitty_place(3, 1, true, None),
                kitty_delete(3, 1, true),
                KITTY_DELETE_ALL.to_vec()
            ]
            .iter()
            .all(|c| std::str::from_utf8(c).unwrap().contains("q=2")),
            "no command asks the terminal to answer: an answer would arrive as typing"
        );
    }

    #[test]
    fn an_iterm_picture_is_one_sequence_with_its_size_and_a_png() {
        let img = gradient(64, 40);
        let encoded = encode(&img, Protocol::Iterm, (1000, 1000), 1).unwrap();
        let text = String::from_utf8(encoded.stream).unwrap();
        let body = text
            .strip_prefix("\x1b]1337;File=")
            .and_then(|t| t.strip_suffix("\x1b\\"))
            .expect("OSC 1337 File= ... ST");
        let (args, data) = body.split_once(':').expect("args:data");
        assert!(
            args.contains("inline=1")
                && args.contains("width=64px")
                && args.contains("height=40px"),
            "{args}"
        );
        let png = base64_decode(data);
        assert!(args.contains(&format!("size={}", png.len())));
        assert_eq!(decode_png(&png), img);
        assert_eq!(text.matches('\x1b').count(), 2);
    }

    /// Standard base64, for reading back what was written (the writer is `clipboard::base64`).
    fn base64_decode(text: &str) -> Vec<u8> {
        let value = |c: u8| match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            other => panic!("not base64: {other}"),
        };
        let mut out = Vec::new();
        for quad in text.as_bytes().chunks(4) {
            assert_eq!(quad.len(), 4);
            let pad = quad.iter().filter(|c| **c == b'=').count();
            let n = quad
                .iter()
                .take(4 - pad)
                .fold(0u32, |n, c| (n << 6) | u32::from(value(*c)))
                << (6 * pad);
            let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
            out.extend_from_slice(&bytes[..3 - pad]);
        }
        out
    }

    #[test]
    fn a_large_picture_is_sent_at_the_size_of_its_rectangle_not_its_own() {
        let img = gradient(1200, 800);
        let encoded = encode(&img, Protocol::Kitty, (300, 200), 1).unwrap();
        assert_eq!(encoded.size, (300, 200));
        let encoded = encode(&img, Protocol::Kitty, (5000, 5000), 1).unwrap();
        assert_eq!(encoded.size, (1200, 800), "never above the source");
        let big = RgbImage::from_pixel(3000, 2000, image::Rgb([9, 9, 9]));
        let encoded = encode(&big, Protocol::Iterm, (4000, 4000), 1).unwrap();
        assert!(
            encoded.size.0 <= MAX_SIDE_PX && encoded.size.1 <= MAX_SIDE_PX,
            "never above {MAX_SIDE_PX}: {:?}",
            encoded.size
        );
    }

    /// The worst picture for every protocol: noise, which no palette and no compression helps, `side` pixels square (the
    /// largest picture there is would take half a minute in a debug build; see the ignored test below for that).
    fn check_noise_ends_up_inside_the_hard_cap_and_still_decodes(side: u32) {
        assert_eq!(MAX_STREAM_BYTES, 512 * 1024);
        let mut seed = 12_345u32;
        let noise = RgbImage::from_fn(side, side, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            image::Rgb([(seed >> 8) as u8, (seed >> 16) as u8, (seed >> 24) as u8])
        });
        for protocol in [Protocol::Sixel, Protocol::Kitty, Protocol::Iterm] {
            let encoded = encode(&noise, protocol, (side, side), 1)
                .unwrap_or_else(|e| panic!("{protocol:?}: {e}"));
            assert!(
                encoded.stream.len() <= 512 * 1024,
                "{protocol:?}: {} bytes",
                encoded.stream.len()
            );
            assert!(
                encoded.size.0 < side,
                "{protocol:?}: it had to be made smaller: {:?}",
                encoded.size
            );
            match protocol {
                Protocol::Sixel => {
                    let decoded = decode_sixel(&encoded.stream);
                    assert_eq!(
                        decoded.size,
                        (encoded.size.0 as usize, encoded.size.1 as usize)
                    );
                    assert!(
                        decoded.pixels.iter().all(Option::is_some),
                        "every pixel drawn"
                    );
                }
                Protocol::Kitty => {
                    let text = String::from_utf8(encoded.stream.clone()).unwrap();
                    let data: String = text
                        .split("\x1b\\")
                        .filter(|c| !c.is_empty())
                        .map(|c| c.split_once(';').expect("a header and its data").1)
                        .collect();
                    let png = base64_decode(&data);
                    assert_eq!(decode_png(&png).dimensions(), encoded.size);
                }
                Protocol::Iterm => {
                    let text = String::from_utf8(encoded.stream.clone()).unwrap();
                    let data = text
                        .strip_suffix("\x1b\\")
                        .and_then(|t| t.split_once(':'))
                        .expect("OSC 1337 File= ... ST")
                        .1;
                    assert_eq!(decode_png(&base64_decode(data)).dimensions(), encoded.size);
                }
            }
        }
    }

    #[test]
    fn noise_ends_up_inside_the_hard_cap_and_still_decodes() {
        check_noise_ends_up_inside_the_hard_cap_and_still_decodes(480);
    }

    /// `cargo test --release noise_at_the_largest_size -- --ignored`: the same at the largest picture there is.
    #[test]
    #[ignore = "half a minute in a debug build"]
    fn noise_at_the_largest_size_ends_up_inside_the_hard_cap_and_still_decodes() {
        check_noise_ends_up_inside_the_hard_cap_and_still_decodes(MAX_SIDE_PX);
    }

    #[test]
    fn a_stream_over_the_byte_limit_is_made_again_smaller_and_then_given_up() {
        let mut seed = 99u32;
        let noise = RgbImage::from_fn(300, 300, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            image::Rgb([(seed >> 8) as u8, (seed >> 16) as u8, (seed >> 24) as u8])
        });
        let limit = 60_000;
        let first = encode_capped(&noise, Protocol::Kitty, (300, 300), 1, usize::MAX).unwrap();
        assert!(
            first.stream.len() > limit,
            "the premise: noise does not compress"
        );
        let encoded = encode_capped(&noise, Protocol::Kitty, (300, 300), 1, limit).unwrap();
        assert!(encoded.stream.len() <= limit);
        assert!(
            encoded.size.0 < 300,
            "it was made smaller: {:?}",
            encoded.size
        );
        assert_eq!(
            encode_capped(&noise, Protocol::Kitty, (300, 300), 1, 100).unwrap_err(),
            EncodeError::TooLarge
        );
    }

    /// `cargo test --release sixel_slice_cost -- --ignored --nocapture`: how big a stream is when part of a picture is
    /// drawn, and what making that part costs.
    #[test]
    #[ignore = "measurement"]
    fn sixel_slice_cost() {
        let (w, h) = (444u32, 438u32);
        let mut seed = 7u32;
        let img = RgbImage::from_fn(w, h, |x, y| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = ((seed >> 24) as f64 - 128.0) * 0.06;
            let (fx, fy) = (f64::from(x) / 60.0, f64::from(y) / 50.0);
            let v = (fx.sin() + (fy * 1.3).sin() + ((fx + fy) * 0.8).sin()) / 3.0;
            let c = |p: f64| (128.0 + 100.0 * (v * 3.0 + p).sin() + n).clamp(0.0, 255.0) as u8;
            image::Rgb([c(0.0), c(2.0), c(4.0)])
        });
        let e = encode(&img, Protocol::Sixel, (1000, 1000), 1).unwrap();
        println!(
            "whole picture {}x{}: {} bytes",
            e.size.0,
            e.size.1,
            e.stream.len()
        );
        for (name, rows) in [
            ("top three quarters", 0..320u32),
            ("the middle half", 110..330),
            ("the bottom quarter", 330..438),
        ] {
            let visible = e.visible_rows(rows.clone()).unwrap();
            let started = std::time::Instant::now();
            let mut bytes = 0;
            for _ in 0..1000 {
                bytes = e.stream_for(&visible).len();
            }
            println!(
                "{name} (rows {visible:?}): {bytes} bytes, {} us to make",
                started.elapsed().as_micros() / 1000
            );
        }
    }

    #[test]
    #[ignore = "measurement"]
    fn sixel_size_by_dither() {
        let (w, h) = (444u32, 438u32);
        let mut seed = 7u32;
        let img = RgbImage::from_fn(w, h, |x, y| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = ((seed >> 24) as f64 - 128.0) * 0.06;
            let (fx, fy) = (f64::from(x) / 60.0, f64::from(y) / 50.0);
            let v = (fx.sin() + (fy * 1.3).sin() + ((fx + fy) * 0.8).sin()) / 3.0;
            let c = |p: f64| (128.0 + 100.0 * (v * 3.0 + p).sin() + n).clamp(0.0, 255.0) as u8;
            image::Rgb([c(0.0), c(2.0), c(4.0)])
        });
        for colours in [256usize, 128, 64] {
            for dither in [8, 4, 2, 0] {
                let s = sixel_with(&img, colours, dither);
                let d = decode_sixel(&s);
                println!(
                    "colours {colours:>3} dither {dither}/8: {:>7} bytes, mean error {:.2}",
                    s.len(),
                    mean_error(&img, &d)
                );
            }
        }
    }

    /// Writes one picture in each of Sixel and Kitty graphics (and iTerm2's form) for looking at in a real terminal:
    /// `cargo test --release write_sample_pictures -- --ignored`, then `cat` the files it names.
    #[test]
    #[ignore = "writes files for trying the protocols by hand"]
    fn write_sample_pictures() {
        // A colour wheel on a dark ground with a bright frame and a few sharp lines, so smooth colour, edges and exact
        // placement are all easy to judge.
        let (w, h) = (360u32, 240u32);
        let img = RgbImage::from_fn(w, h, |x, y| {
            let (dx, dy) = (f64::from(x) - 180.0, f64::from(y) - 120.0);
            let r = (dx * dx + dy * dy).sqrt();
            if x < 3 || y < 3 || x >= w - 3 || y >= h - 3 {
                image::Rgb([255, 255, 255])
            } else if r < 100.0 {
                let angle = dy.atan2(dx) / std::f64::consts::TAU + 0.5;
                let (red, green, blue) = hue(angle);
                let k = 1.0 - (r / 100.0) * 0.6;
                image::Rgb([(red * k) as u8, (green * k) as u8, (blue * k) as u8])
            } else if (x / 20 + y / 20) % 2 == 0 {
                image::Rgb([30, 30, 46])
            } else {
                image::Rgb([24, 24, 37])
            }
        });
        let dir = std::env::temp_dir().join("vtteams-graphics-samples");
        std::fs::create_dir_all(&dir).unwrap();
        let sixel_file = dir.join("sample.sixel");
        std::fs::write(&sixel_file, sixel(&img)).unwrap();
        // The Kitty file transmits the picture and shows it where the cursor is (a=p without C=1 moves the cursor below).
        let kitty = encode(&img, Protocol::Kitty, (w, h), 4242).unwrap();
        let mut shown = kitty.stream.clone();
        shown.extend(kitty_place(kitty.kitty_id(), 1, false, None));
        shown.extend_from_slice(b"\n");
        let kitty_file = dir.join("sample.kitty");
        std::fs::write(&kitty_file, shown).unwrap();
        let iterm = encode(&img, Protocol::Iterm, (w, h), 1).unwrap();
        let mut shown = iterm.stream;
        shown.extend_from_slice(b"\n");
        let iterm_file = dir.join("sample.iterm");
        std::fs::write(&iterm_file, shown).unwrap();
        // What the Sixel draws, read back by the test decoder and saved as a PNG, to compare with what a terminal shows.
        let decoded = decode_sixel(&std::fs::read(&sixel_file).unwrap());
        let readback = RgbImage::from_fn(w, h, |x, y| {
            image::Rgb(decoded.pixels[(y * w + x) as usize].unwrap_or([255, 0, 255]))
        });
        readback.save(dir.join("sample-sixel-decoded.png")).unwrap();
        println!("wrote {}", sixel_file.display());
        println!("wrote {}", kitty_file.display());
        println!("wrote {}", iterm_file.display());
    }

    fn hue(t: f64) -> (f64, f64, f64) {
        let h = t.rem_euclid(1.0) * 6.0;
        let f = h - h.floor();
        let (r, g, b) = match h as u32 {
            0 => (1.0, f, 0.0),
            1 => (1.0 - f, 1.0, 0.0),
            2 => (0.0, 1.0, f),
            3 => (0.0, 1.0 - f, 1.0),
            4 => (f, 0.0, 1.0),
            _ => (1.0, 0.0, 1.0 - f),
        };
        (r * 255.0, g * 255.0, b * 255.0)
    }
}
