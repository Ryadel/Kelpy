//! Kelpy's marks in the terminal: the kelpie, drawn in block characters from a map of its pixels, and the `kelpy_`
//! wordmark. Both need true colour; on a terminal with 16 colours the wordmark is drawn alone, in its own colours.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

/// A colour as red, green and blue.
pub type Rgb = [u8; 3];

/// Kelpy's turquoise: the mane, the wordmark's cursor, the glow behind the kelpie.
pub const TEAL: Rgb = [0x3F, 0xC0, 0xCA];

/// The kelpie at one of its sizes. Each is a map of its pixels, one character each (see [`pixel`]), drawn by hand
/// for its size: scaling one down would blur the eye and the mane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kelpie {
    /// 42 × 50 pixels in half blocks: 42 columns, 25 rows.
    Large,
    /// 30 × 35 pixels in half blocks: 30 columns, 18 rows.
    Medium,
    /// 28 × 17 pixels in quarter blocks: 14 columns, 9 rows. Without greys, which would blur the head this small.
    Small,
}

impl Kelpie {
    fn map(self) -> &'static str {
        match self {
            Self::Large => include_str!("../marks/large.txt"),
            Self::Medium => include_str!("../marks/medium.txt"),
            Self::Small => include_str!("../marks/small.txt"),
        }
    }

    /// The pixels a cell holds, across and down, and the block character for each set of them, indexed by the
    /// pixels in the text colour: one bit each, left to right and top to bottom.
    fn blocks(self) -> (usize, usize, &'static [&'static str]) {
        match self {
            Self::Large | Self::Medium => (1, 2, &[" ", "▀", "▄", "█"]),
            Self::Small => (2, 2, &QUARTERS),
        }
    }
}

/// The quarter blocks, indexed by the quarters in the text colour: top left 1, top right 2, bottom left 4, bottom
/// right 8.
const QUARTERS: [&str; 16] = [
    " ", "▘", "▝", "▀", "▖", "▌", "▞", "▛", "▗", "▚", "▐", "▜", "▄", "▙", "▟", "█",
];

/// The colour of a pixel of a map; `.` is none.
fn pixel(code: u8) -> Option<Rgb> {
    match code {
        b'k' => Some([0x11, 0x13, 0x12]),
        b'g' => Some([0x55, 0x56, 0x55]),
        b's' => Some([0x9D, 0x9D, 0x9D]),
        b't' => Some(TEAL),
        b'p' => Some([0x03, 0x87, 0x9A]),
        b'o' => Some([0xFF, 0xFF, 0xFF]),
        _ => None,
    }
}

/// The kelpie as drawn: inside a margin of empty pixels and, on a dark terminal, on a turquoise glow, which keeps
/// its black body apart from the background. A light terminal needs none.
#[derive(Debug, Clone, Copy)]
pub struct Mark {
    kelpie: Kelpie,
    /// Empty pixels around the kelpie, on each side.
    margin: usize,
    /// The background the glow is blended over; none without a glow.
    glow: Option<Rgb>,
    /// Drawn in the nearest colours of the 256-colour palette.
    indexed: bool,
}

impl Mark {
    pub fn new(kelpie: Kelpie, margin: usize) -> Self {
        Self {
            kelpie,
            margin,
            glow: None,
            indexed: false,
        }
    }

    /// The mark in the nearest colours of the 256-colour palette, for a terminal that shows no other.
    pub fn indexed(self) -> Self {
        Self {
            indexed: true,
            ..self
        }
    }

    /// The mark on its glow, over `background`.
    pub fn glow(self, background: Rgb) -> Self {
        Self {
            glow: Some(background),
            ..self
        }
    }

    /// The map's rows; each row a pixel per byte.
    fn rows(&self) -> Vec<&'static [u8]> {
        self.kelpie.map().lines().map(str::as_bytes).collect()
    }

    /// The columns and rows the mark takes: the map and its margin, made whole cells.
    pub fn size(&self) -> (u16, u16) {
        let rows = self.rows();
        let (across, down, _) = self.kelpie.blocks();
        let cells =
            |pixels: usize, per_cell: usize| (pixels + 2 * self.margin).div_ceil(per_cell) as u16;
        (cells(rows[0].len(), across), cells(rows.len(), down))
    }
}

impl Widget for Mark {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let rows = self.rows();
        let (across, down, blocks) = self.kelpie.blocks();
        let (columns, height) = self.size();
        let pixel_at = |x: usize, y: usize| {
            let (x, y) = (x.checked_sub(self.margin)?, y.checked_sub(self.margin)?);
            pixel(*rows.get(y)?.get(x)?)
        };
        let area = area.intersection(buf.area);
        let colour = |value: Rgb| {
            if self.indexed {
                Color::Indexed(crate::theme::indexed(value))
            } else {
                rgb(value)
            }
        };
        for row in 0..height.min(area.height) {
            for column in 0..columns.min(area.width) {
                let back = self.glow.and_then(|background| {
                    glow(
                        (f32::from(column) + 0.5) / f32::from(columns),
                        (f32::from(row) + 0.5) / f32::from(height),
                        background,
                    )
                });
                let pixels: Vec<Option<Rgb>> = (0..down)
                    .flat_map(|dy| (0..across).map(move |dx| (dx, dy)))
                    .map(|(dx, dy)| {
                        pixel_at(
                            usize::from(column) * across + dx,
                            usize::from(row) * down + dy,
                        )
                        .or(back)
                    })
                    .collect();
                let cell = &mut buf[(area.x + column, area.y + row)];
                let Some((front, behind)) = two_colours(&pixels, back) else {
                    if let Some(back) = back {
                        cell.set_symbol(" ").set_bg(colour(back));
                    }
                    continue;
                };
                let bits = pixels
                    .iter()
                    .enumerate()
                    .filter(|(_, colour)| **colour == Some(front))
                    .fold(0, |bits, (index, _)| bits | 1 << index);
                cell.set_symbol(blocks[bits]).set_fg(colour(front));
                if let Some(behind) = behind {
                    cell.set_bg(colour(behind));
                }
            }
        }
    }
}

/// The two colours a cell is drawn in, of the pixels it holds: in front, the one most of them have other than the
/// background, the first of them on a tie; behind, the one most of the others have, if any. None when it shows only
/// the background. A cell shows two colours at most: a third takes the one behind.
fn two_colours(pixels: &[Option<Rgb>], back: Option<Rgb>) -> Option<(Rgb, Option<Rgb>)> {
    let count = |colour: Option<Rgb>| pixels.iter().filter(|&&other| other == colour).count();
    // The first of the most frequent: `max_by_key` keeps the last of equals, so the pixels are walked backwards.
    let most = |skip: &dyn Fn(Option<Rgb>) -> bool| {
        pixels
            .iter()
            .rev()
            .copied()
            .filter(|&colour| !skip(colour))
            .max_by_key(|&colour| count(colour))
    };
    let front = most(&|colour| colour.is_none() || colour == back)??;
    let behind = most(&|colour| colour == Some(front)).flatten();
    Some((front, behind))
}

/// The glow at a point of the mark, `x` and `y` from 0 to 1 across and down, blended over `background`: as the
/// website's, 20% turquoise in the middle, 6% at 60% of the way to the edge, none at the edge. None where too faint
/// to show.
fn glow(x: f32, y: f32, background: Rgb) -> Option<Rgb> {
    let distance = (2.0 * x - 1.0).hypot(2.0 * y - 1.0);
    let alpha = if distance < 0.6 {
        0.2 - 0.14 * distance / 0.6
    } else {
        0.06 * ((1.0 - distance) / 0.4).max(0.0)
    };
    (alpha >= 0.012).then(|| {
        std::array::from_fn(|index| {
            let (base, teal) = (f32::from(background[index]), f32::from(TEAL[index]));
            (base + (teal - base) * alpha).round() as u8
        })
    })
}

pub fn rgb([red, green, blue]: Rgb) -> Color {
    Color::Rgb(red, green, blue)
}

/// The wordmark `kelpy_`, on a grid of half cells: each pixel a column wide and half a row high, twelve pixels (six
/// rows) in all, stems two pixels wide. The underscore is its cursor, on the baseline.
#[rustfmt::skip]
const GLYPHS: [(char, [&str; 12]); 6] = [
    ('k', ["##....", "##....", "##..##", "##.##.", "####..", "###...", "####..", "##.##.", "##..##", "......", "......", "......"]),
    ('e', ["......", "......", ".####.", "##..##", "##..##", "######", "##....", "##....", ".#####", "......", "......", "......"]),
    ('l', ["##", "##", "##", "##", "##", "##", "##", "##", "##", "..", "..", ".."]),
    ('p', ["......", "......", "#####.", "##..##", "##..##", "##..##", "##..##", "##..##", "#####.", "##....", "##....", "......"]),
    ('y', ["......", "......", "##..##", "##..##", "##..##", "##..##", "##..##", "##..##", ".#####", "....##", "#####.", "......"]),
    ('_', ["......", "......", "......", "......", "......", "......", "......", "......", "######", "......", "......", "......"]),
];

/// The wordmark: its letters in `ink`, its cursor in `cursor`.
pub struct Wordmark {
    pub ink: Color,
    pub cursor: Color,
}

impl Wordmark {
    /// The columns and rows it takes.
    pub const SIZE: (u16, u16) = (37, 6);
}

impl Widget for Wordmark {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        let mut x = area.x;
        for (letter, glyph) in GLYPHS {
            let colour = if letter == '_' { self.cursor } else { self.ink };
            let width = glyph[0].len() as u16;
            for (row, pair) in glyph.chunks(2).enumerate().take(area.height.into()) {
                for (column, (top, bottom)) in pair[0].bytes().zip(pair[1].bytes()).enumerate() {
                    let symbol = match (top, bottom) {
                        (b'#', b'#') => "█",
                        (b'#', _) => "▀",
                        (_, b'#') => "▄",
                        _ => continue,
                    };
                    let column = x + column as u16;
                    if column < area.right() {
                        buf[(column, area.y + row as u16)]
                            .set_symbol(symbol)
                            .set_fg(colour);
                    }
                }
            }
            x += width + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KELPIES: [Kelpie; 3] = [Kelpie::Large, Kelpie::Medium, Kelpie::Small];

    #[test]
    fn each_map_has_its_size_and_only_known_pixels() {
        for (kelpie, size) in KELPIES.into_iter().zip([(42, 50), (30, 35), (28, 17)]) {
            let rows = Mark::new(kelpie, 0).rows();
            assert_eq!((rows[0].len(), rows.len()), size, "{kelpie:?}");
            for row in rows {
                assert_eq!(row.len(), size.0, "{kelpie:?}");
                assert!(
                    row.iter()
                        .all(|&code| code == b'.' || pixel(code).is_some())
                );
            }
        }
    }

    #[test]
    fn a_mark_takes_its_map_and_margin_in_whole_cells() {
        assert_eq!(Mark::new(Kelpie::Large, 4).size(), (50, 29));
        assert_eq!(Mark::new(Kelpie::Medium, 0).size(), (30, 18));
        assert_eq!(Mark::new(Kelpie::Medium, 1).size(), (32, 19));
        assert_eq!(Mark::new(Kelpie::Small, 2).size(), (16, 11));
        let (columns, rows) = Wordmark::SIZE;
        let width: usize = GLYPHS.iter().map(|(_, glyph)| glyph[0].len() + 1).sum();
        assert_eq!((usize::from(columns), usize::from(rows)), (width - 1, 6));
    }

    #[test]
    fn the_glow_is_brightest_in_the_middle_and_gone_at_the_edge() {
        let background = [0x10, 0x13, 0x14];
        assert_eq!(glow(0.5, 0.5, background), Some([0x19, 0x36, 0x38]));
        assert!(glow(0.5, 0.1, background).unwrap()[1] < 0x36);
        assert_eq!(glow(0.0, 0.0, background), None);
        assert_eq!(glow(0.5, 0.0, background), None);
    }

    #[test]
    fn a_cell_shows_its_two_most_frequent_colours_the_mark_before_the_background() {
        let (black, white, teal, back) = ([0; 3], [255; 3], TEAL, [9; 3]);
        // Mostly background: the mark still goes in front.
        assert_eq!(
            two_colours(
                &[Some(back), Some(back), Some(back), Some(black)],
                Some(back)
            ),
            Some((black, Some(back)))
        );
        // Three colours: the third takes the one behind, the background here.
        assert_eq!(
            two_colours(
                &[Some(back), Some(back), Some(black), Some(teal)],
                Some(back)
            ),
            Some((black, Some(back)))
        );
        assert_eq!(
            two_colours(&[Some(white), Some(teal), Some(teal), None], None),
            Some((teal, Some(white)))
        );
        assert_eq!(two_colours(&[None, None], None), None);
        assert_eq!(two_colours(&[Some(back), Some(back)], Some(back)), None);
    }

    /// What `widget`, `size` cells big, draws on a blank screen.
    fn drawn(widget: impl Widget, (width, height): (u16, u16)) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        widget.render(buf.area, &mut buf);
        buf
    }

    #[test]
    fn on_a_dark_terminal_the_glow_fills_the_middle_and_on_a_light_one_nothing_does() {
        let mark = Mark::new(Kelpie::Small, 2);
        let light = drawn(mark, mark.size());
        let dark = drawn(mark.glow([0x10, 0x13, 0x14]), mark.size());
        // The margin: empty on a light terminal, and in the corners on a dark one.
        assert_eq!(light[(0, 5)], Default::default());
        assert_eq!(dark[(0, 0)], Default::default());
        assert!(matches!(dark[(1, 5)].bg, Color::Rgb(..)));
        // The single white eye is drawn at each size.
        let white = Color::Rgb(255, 255, 255);
        for kelpie in KELPIES {
            let mark = Mark::new(kelpie, 1).glow([0x10, 0x13, 0x14]);
            let buf = drawn(mark, mark.size());
            assert!(
                buf.content()
                    .iter()
                    .any(|cell| cell.fg == white || cell.bg == white),
                "{kelpie:?}"
            );
        }
    }

    #[test]
    fn the_wordmark_draws_its_letters_and_cursor_and_is_cut_by_a_smaller_area() {
        let wordmark = || Wordmark {
            ink: Color::White,
            cursor: Color::Cyan,
        };
        let buf = drawn(wordmark(), Wordmark::SIZE);
        let text: Vec<String> = (0..6)
            .map(|row| (0..37).map(|column| buf[(column, row)].symbol()).collect())
            .collect();
        assert_eq!(text[0], format!("██{}██{}", " ".repeat(12), " ".repeat(21)));
        assert_eq!(buf[(36, 4)].symbol(), "▀");
        assert_eq!(buf[(36, 4)].fg, Color::Cyan);
        // Drawn into fewer columns and rows than it takes, it stays inside them.
        let mut small = Buffer::empty(Rect::new(0, 0, 37, 6));
        wordmark().render(Rect::new(0, 0, 10, 2), &mut small);
        assert!((0..37).all(|column| small[(column, 3)].symbol() == " "));
        assert!((10..37).all(|column| small[(column, 0)].symbol() == " "));
    }
}
