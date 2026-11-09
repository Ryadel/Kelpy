//! The terminal UI's colours, by what they mean: the same names on a dark terminal and a light one, in any colour,
//! in the 256 of the common palette, or in the 16 of the terminal's own; and what the terminal says it shows.

use ratatui::style::Color;

use crate::mark::{Mark, Rgb, rgb};

/// A dark terminal's background, as Kelpy assumes it until the terminal says.
pub const TERMINAL: Rgb = [0x10, 0x13, 0x14];
/// A light terminal's background, as Kelpy assumes it until the terminal says.
const LIGHT_TERMINAL: Rgb = [0xFF, 0xFF, 0xFF];
/// The side panel's background on a dark terminal.
pub const PANEL: Rgb = [0x15, 0x1B, 0x1D];

/// How many colours a terminal shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colours {
    /// Any: the marks as drawn, with their glow.
    True,
    /// The 256 of the common palette: the marks in its nearest colours, without the glow, which would band.
    Indexed,
    /// The 16 of the terminal's own palette: no marks.
    Basic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub colours: Colours,
    /// Whether the terminal's background is dark.
    pub dark: bool,
    /// The terminal's background, as it said, else as assumed.
    pub background: Rgb,
}

impl Default for Theme {
    fn default() -> Self {
        Self::new(Colours::True, true)
    }
}

impl Theme {
    pub fn new(colours: Colours, dark: bool) -> Self {
        Self {
            colours,
            dark,
            background: if dark { TERMINAL } else { LIGHT_TERMINAL },
        }
    }

    /// The colour for a dark terminal or a light one, or one with 16 colours.
    fn pick(self, dark: Rgb, light: Rgb, basic: Color) -> Color {
        let wanted = if self.dark { dark } else { light };
        match self.colours {
            Colours::True => rgb(wanted),
            Colours::Indexed => Color::Indexed(indexed(wanted)),
            Colours::Basic => basic,
        }
    }

    /// Whether the marks show: not with 16 colours.
    pub fn marks(self) -> bool {
        self.colours != Colours::Basic
    }

    /// `mark` as this terminal shows it, over `under`, or the terminal's background: on its glow where the
    /// background is dark and any colour shows; in the palette's colours where only 256 do.
    pub fn paint(self, mark: Mark, under: Option<Rgb>) -> Mark {
        match self.colours {
            Colours::True if self.dark => mark.glow(under.unwrap_or(self.background)),
            Colours::Indexed => mark.indexed(),
            _ => mark,
        }
    }

    pub fn text(self) -> Color {
        self.pick([0xD7, 0xDC, 0xDD], [0x1A, 0x1D, 0x1E], Color::Reset)
    }

    /// Titles and the administrator's messages.
    pub fn strong(self) -> Color {
        self.pick([0xFF, 0xFF, 0xFF], [0x00, 0x00, 0x00], Color::Reset)
    }

    /// What explains, and the keys.
    pub fn dim(self) -> Color {
        self.pick([0x6B, 0x75, 0x79], [0x6A, 0x72, 0x75], Color::DarkGray)
    }

    /// The message box's placeholder: lighter than `dim`, to be read at a glance.
    pub fn placeholder(self) -> Color {
        self.pick([0x8B, 0x96, 0x99], [0x7D, 0x86, 0x89], Color::DarkGray)
    }

    /// Kelpy's turquoise; petrol on a light terminal, for contrast.
    pub fn accent(self) -> Color {
        self.pick(crate::mark::TEAL, [0x03, 0x87, 0x9A], Color::Cyan)
    }

    pub fn good(self) -> Color {
        self.pick([0x7C, 0xCF, 0x8A], [0x2F, 0x8A, 0x3E], Color::Green)
    }

    pub fn bad(self) -> Color {
        self.pick([0xEE, 0x80, 0x80], [0xB3, 0x26, 0x1E], Color::Red)
    }

    /// What waits for the administrator, and full access.
    pub fn warn(self) -> Color {
        self.pick([0xE6, 0xC3, 0x6A], [0x9A, 0x6B, 0x00], Color::Yellow)
    }

    /// Behind a chip in `warn`: the same yellow on both backgrounds, with dark text.
    pub fn warn_chip(self) -> Color {
        self.pick([0xE6, 0xC3, 0x6A], [0xE6, 0xC3, 0x6A], Color::Yellow)
    }

    /// Text on `accent` or `warn_chip`.
    pub fn on_chip(self) -> Color {
        self.pick([0x0B, 0x11, 0x12], [0xFF, 0xFF, 0xFF], Color::Black)
    }

    /// Behind the message box and the administrator's messages.
    pub fn surface(self) -> Color {
        self.pick([0x18, 0x23, 0x26], [0xEE, 0xF3, 0xF4], Color::Reset)
    }

    /// Behind the side panel and the header.
    pub fn panel(self) -> Color {
        self.pick(PANEL, [0xF4, 0xF7, 0xF8], Color::Reset)
    }

    /// The panel's background as the glow of its mark is blended over, on a dark terminal.
    pub fn panel_under(self) -> Option<Rgb> {
        self.dark.then_some(PANEL)
    }

    /// Behind a chip that is not lit.
    pub fn chip(self) -> Color {
        self.pick([0x26, 0x30, 0x33], [0xE1, 0xE8, 0xEA], Color::DarkGray)
    }
}

/// The colour of the 256-colour palette nearest `colour`: of its 6 x 6 x 6 cube from 16 and its 24 greys from 232.
pub fn indexed([red, green, blue]: Rgb) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |value: u8| match value {
        ..48 => 0,
        48..115 => 1,
        _ => (value - 35) / 40,
    };
    let distance = |[r, g, b]: Rgb| {
        [(r, red), (g, green), (b, blue)]
            .into_iter()
            .map(|(a, b)| (i32::from(a) - i32::from(b)).pow(2))
            .sum::<i32>()
    };
    let (r, g, b) = (level(red), level(green), level(blue));
    let cube = [
        LEVELS[usize::from(r)],
        LEVELS[usize::from(g)],
        LEVELS[usize::from(b)],
    ];
    let average = (u16::from(red) + u16::from(green) + u16::from(blue)) / 3;
    let grey_index = (average.saturating_sub(3) / 10).min(23) as u8;
    let grey = 8 + 10 * grey_index;
    if distance([grey; 3]) < distance(cube) {
        232 + grey_index
    } else {
        16 + 36 * r + 6 * g + b
    }
}

/// How many colours the terminal shows, from what its environment tells: `variable` reads it. Terminals that show any
/// colour say so in `COLORTERM`, or are known by name; over SSH and in tmux that is often lost, and `TERM` names the
/// 256-colour palette at best. Windows' consoles all show any colour.
pub fn colours_from(variable: impl Fn(&str) -> Option<String>, windows: bool) -> Colours {
    let read = |name: &str| variable(name).filter(|value| !value.is_empty());
    let term = read("TERM").unwrap_or_default();
    let known = [
        "-direct",
        "kitty",
        "alacritty",
        "foot",
        "ghostty",
        "wezterm",
    ];
    if windows
        || matches!(read("COLORTERM").as_deref(), Some("truecolor" | "24bit"))
        || matches!(
            read("TERM_PROGRAM").as_deref(),
            Some("iTerm.app" | "WezTerm" | "vscode" | "ghostty")
        )
        || known.iter().any(|name| term.contains(name))
    {
        Colours::True
    } else if term.contains("256") {
        Colours::Indexed
    } else {
        Colours::Basic
    }
}

/// Whether a background is dark: its lightness as perceived, under half.
pub fn is_dark([red, green, blue]: Rgb) -> bool {
    let luminance = 0.2126 * f32::from(red) + 0.7152 * f32::from(green) + 0.0722 * f32::from(blue);
    luminance < 128.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_colours_shown_come_from_the_environment() {
        let with = |pairs: &'static [(&str, &str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        assert_eq!(
            colours_from(with(&[("COLORTERM", "truecolor")]), false),
            Colours::True
        );
        assert_eq!(
            colours_from(with(&[("TERM", "xterm-kitty")]), false),
            Colours::True
        );
        assert_eq!(
            colours_from(with(&[("TERM", "xterm-256color")]), false),
            Colours::Indexed
        );
        assert_eq!(
            colours_from(with(&[("TERM", "tmux-256color")]), false),
            Colours::Indexed
        );
        assert_eq!(
            colours_from(with(&[("TERM", "linux")]), false),
            Colours::Basic
        );
        assert_eq!(colours_from(with(&[]), true), Colours::True);
    }

    #[test]
    fn a_colour_takes_the_nearest_of_the_256() {
        assert_eq!(indexed([0, 0, 0]), 16);
        assert_eq!(indexed([255, 255, 255]), 231);
        assert_eq!(indexed([0x11, 0x13, 0x12]), 233);
        // Kelpy's turquoise: 0x5f/0xaf/0xd7 is nearer than any grey.
        assert_eq!(indexed(crate::mark::TEAL), 74);
        assert_eq!(indexed([0x80, 0x80, 0x80]), 244);
    }

    #[test]
    fn dark_and_light_backgrounds_are_told_apart() {
        assert!(is_dark(TERMINAL));
        assert!(is_dark([0x30, 0x0A, 0x24]));
        assert!(!is_dark(LIGHT_TERMINAL));
        assert!(!is_dark([0xFD, 0xF6, 0xE3]));
    }
}
