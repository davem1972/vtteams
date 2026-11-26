//! A small, strict decoder for the Sixel streams encode.rs writes, for tests: it reads back what a stream draws so
//! that it can be compared with the picture it was made from (and a part of it with the matching rows of the whole).

use std::collections::HashMap;

/// What a Sixel stream draws, read back: the size it declares and the colour of every pixel it sets. A small, strict
/// decoder for the subset the encoder writes (raster attributes, palette definitions in RGB percent, `!` repeats, `$`
/// and `-`).
pub(crate) struct Decoded {
    pub size: (usize, usize),
    pub pixels: Vec<Option<[u8; 3]>>,
    pub palette: HashMap<usize, [u8; 3]>,
    pub bands: usize,
}

/// The picture being drawn by the decoder.
#[derive(Default)]
struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<Option<[u8; 3]>>,
}

impl Canvas {
    /// `n` columns of the sixel `bits` in `colour`, from column `*x` of the band starting at row `top`.
    fn draw(&mut self, bits: u8, n: usize, colour: [u8; 3], x: &mut usize, top: usize) {
        for _ in 0..n {
            for row in 0..6 {
                if bits & (1 << row) != 0 {
                    let y = top + row;
                    assert!(
                        *x < self.width && y < self.height,
                        "a pixel outside the declared size"
                    );
                    self.pixels[y * self.width + *x] = Some(colour);
                }
            }
            *x += 1;
        }
    }
}

fn number(data: &[u8], at: &mut usize) -> Option<usize> {
    let start = *at;
    while data.get(*at).is_some_and(u8::is_ascii_digit) {
        *at += 1;
    }
    (*at > start).then(|| {
        std::str::from_utf8(&data[start..*at])
            .unwrap()
            .parse()
            .unwrap()
    })
}

pub(crate) fn decode_sixel(stream: &[u8]) -> Decoded {
    assert!(stream.starts_with(b"\x1bP9;1;0q"), "DCS with square pixels");
    assert!(stream.ends_with(b"\x1b\\"), "string terminator");
    let data = &stream[b"\x1bP9;1;0q".len()..stream.len() - 2];
    let mut at = 0;
    let mut canvas = Canvas::default();
    let mut palette: HashMap<usize, [u8; 3]> = HashMap::new();
    let (mut colour, mut x, mut top, mut bands) = (0usize, 0usize, 0usize, 0usize);
    while at < data.len() {
        match data[at] {
            b'"' => {
                at += 1;
                let mut v = Vec::new();
                while let Some(n) = number(data, &mut at) {
                    v.push(n);
                    if data.get(at) == Some(&b';') {
                        at += 1;
                    }
                }
                assert_eq!(v.len(), 4, "raster attributes: Pan;Pad;Ph;Pv");
                assert_eq!((v[0], v[1]), (1, 1), "square pixels");
                canvas = Canvas {
                    width: v[2],
                    height: v[3],
                    pixels: vec![None; v[2] * v[3]],
                };
            }
            b'#' => {
                at += 1;
                colour = number(data, &mut at).expect("a register number");
                if data.get(at) == Some(&b';') {
                    at += 1;
                    assert_eq!(number(data, &mut at), Some(2), "RGB colour space");
                    let mut rgb = [0u8; 3];
                    for slot in &mut rgb {
                        assert_eq!(data.get(at), Some(&b';'));
                        at += 1;
                        let pct = number(data, &mut at).expect("a percentage");
                        assert!(pct <= 100);
                        *slot = ((pct * 255 + 50) / 100) as u8;
                    }
                    palette.insert(colour, rgb);
                }
            }
            b'!' => {
                at += 1;
                let n = number(data, &mut at).expect("a repeat count");
                let ch = data[at];
                assert!((63..=126).contains(&ch));
                at += 1;
                canvas.draw(ch - 63, n, palette[&colour], &mut x, top);
            }
            b'$' => {
                x = 0;
                at += 1;
            }
            b'-' => {
                x = 0;
                top += 6;
                bands += 1;
                at += 1;
            }
            ch @ 63..=126 => {
                canvas.draw(ch - 63, 1, palette[&colour], &mut x, top);
                at += 1;
            }
            other => panic!("unexpected byte {other:#x} at {at}"),
        }
    }
    Decoded {
        size: (canvas.width, canvas.height),
        pixels: canvas.pixels,
        palette,
        bands,
    }
}
