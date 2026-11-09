//! The terminal UI on a terminal Kelpy reaches only through a byte stream, such as an SSH session: what it draws goes
//! out as ANSI sequences, and the keys come in as the bytes the terminal sends. Nothing here touches the local
//! terminal, which on Windows crossterm would drive through the console API.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use kelpy_common::lock;
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthStr;

use crate::mark::Rgb;

/// Switches to the alternate screen with bracketed paste, cursor hidden.
pub const ENTER: &str = "\x1b[?1049h\x1b[?2004h\x1b[?25l\x1b[2J";
/// Gives the terminal back as it was.
pub const LEAVE: &str = "\x1b[0m\x1b[?2004l\x1b[?1049l\x1b[?25h";
/// Bracketed paste, cursor hidden, on the screen as it is: drawn inline.
pub const INLINE_ENTER: &str = "\x1b[?2004h\x1b[?25l";
/// Gives the terminal back after drawing inline.
pub const INLINE_LEAVE: &str = "\x1b[0m\x1b[?2004l\x1b[?25h";
/// Asks the terminal its background colour (OSC 11) and then its attributes (DA1), which every terminal answers: once
/// that answer is in, so is the first, if the terminal gives it.
pub const ASK_BACKGROUND: &str = "\x1b]11;?\x1b\\\x1b[c";

/// The terminal's answer to [`ASK_BACKGROUND`] in `bytes`, once all of it is there: the background, if it told, and
/// the bytes around the answer, such as keys typed meanwhile. None while incomplete.
pub fn background_answer(bytes: &[u8]) -> Option<(Option<Rgb>, Vec<u8>)> {
    let attributes = find(bytes, b"\x1b[?")?;
    let end = attributes + bytes[attributes..].iter().position(|&byte| byte == b'c')? + 1;
    let mut rest = [&bytes[..attributes], &bytes[end..]].concat();
    let Some(start) = find(&rest, b"\x1b]11;") else {
        return Some((None, rest));
    };
    let body = &rest[start + 5..];
    let (length, terminator) = match (
        body.iter().position(|&byte| byte == 0x07),
        find(body, b"\x1b\\"),
    ) {
        (Some(bell), Some(st)) if st < bell => (st, 2),
        (Some(bell), _) => (bell, 1),
        (None, Some(st)) => (st, 2),
        (None, None) => return Some((None, rest)),
    };
    let colour = std::str::from_utf8(&body[..length]).ok().and_then(rgb_spec);
    rest.drain(start..start + 5 + length + terminator);
    Some((colour, rest))
}

/// An X11 colour as terminals give it, `rgb:RRRR/GGGG/BBBB`, each part of one to four hex digits.
fn rgb_spec(spec: &str) -> Option<Rgb> {
    let channel = |part: &str| {
        if !(1..=4).contains(&part.len()) || !part.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(part, 16).ok()?;
        let most = (1u32 << (4 * part.len())) - 1;
        u8::try_from(value * 255 / most).ok()
    };
    match spec.strip_prefix("rgb:")?.split('/').collect::<Vec<_>>()[..] {
        [red, green, blue] => Some([channel(red)?, channel(green)?, channel(blue)?]),
        _ => None,
    }
}

/// A ratatui backend writing ANSI sequences to `out`, for a terminal of the size the other end last reported.
pub struct AnsiBackend<W: Write> {
    out: W,
    size: Arc<Mutex<Size>>,
    cursor: Position,
}

impl<W: Write> AnsiBackend<W> {
    pub fn new(out: W, size: Arc<Mutex<Size>>) -> Self {
        Self {
            out,
            size,
            cursor: Position::ORIGIN,
        }
    }

    /// Writes `bytes` as they are, and flushes them.
    pub fn write_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.out.flush()
    }

    fn go(&mut self, x: u16, y: u16) -> io::Result<()> {
        write!(self.out, "\x1b[{};{}H", y + 1, x + 1)
    }
}

impl<W: Write> Backend for AnsiBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut next: Option<(u16, u16)> = None;
        let mut style: Option<(Color, Color, Modifier)> = None;
        for (x, y, cell) in content {
            if next != Some((x, y)) {
                self.go(x, y)?;
            }
            let wanted = (cell.fg, cell.bg, cell.modifier);
            if style != Some(wanted) {
                self.out
                    .write_all(sgr(cell.fg, cell.bg, cell.modifier).as_bytes())?;
                style = Some(wanted);
            }
            let symbol = cell.symbol();
            self.out.write_all(symbol.as_bytes())?;
            next = Some((x + symbol.width().max(1) as u16, y));
        }
        self.out.write_all(b"\x1b[0m")
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.out.write_all(b"\x1b[?25l")
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.out.write_all(b"\x1b[?25h")
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    /// New lines under the cursor; on the last row, the screen scrolls into the terminal's history.
    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.out.write_all("\n".repeat(n.into()).as_bytes())?;
        let last = lock(&self.size).height.saturating_sub(1);
        self.cursor.y = (self.cursor.y + n).min(last);
        Ok(())
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.cursor = position.into();
        self.go(self.cursor.x, self.cursor.y)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.out.write_all(b"\x1b[2J")
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        let sequence: &[u8] = match clear_type {
            ClearType::All => b"\x1b[2J",
            ClearType::AfterCursor => b"\x1b[0J",
            ClearType::BeforeCursor => b"\x1b[1J",
            ClearType::CurrentLine => b"\x1b[2K",
            ClearType::UntilNewLine => b"\x1b[0K",
        };
        self.out.write_all(sequence)
    }

    fn size(&self) -> io::Result<Size> {
        Ok(*lock(&self.size))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: *lock(&self.size),
            pixels: Size::default(),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// The SGR sequence for a cell's colours and modifiers, from the default style.
fn sgr(fg: Color, bg: Color, modifier: Modifier) -> String {
    let mut codes = vec!["0".to_owned()];
    for (flag, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::REVERSED, "7"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if modifier.contains(flag) {
            codes.push(code.to_owned());
        }
    }
    codes.extend(colour(fg, false));
    codes.extend(colour(bg, true));
    format!("\x1b[{}m", codes.join(";"))
}

fn colour(colour: Color, background: bool) -> Option<String> {
    let base = if background { 40 } else { 30 };
    let named = |index: u8, bright: bool| {
        Some(format!(
            "{}",
            base + u16::from(index) + if bright { 60 } else { 0 }
        ))
    };
    match colour {
        Color::Reset => None,
        Color::Black => named(0, false),
        Color::Red => named(1, false),
        Color::Green => named(2, false),
        Color::Yellow => named(3, false),
        Color::Blue => named(4, false),
        Color::Magenta => named(5, false),
        Color::Cyan => named(6, false),
        Color::Gray => named(7, false),
        Color::DarkGray => named(0, true),
        Color::LightRed => named(1, true),
        Color::LightGreen => named(2, true),
        Color::LightYellow => named(3, true),
        Color::LightBlue => named(4, true),
        Color::LightMagenta => named(5, true),
        Color::LightCyan => named(6, true),
        Color::White => named(7, true),
        Color::Rgb(r, g, b) => Some(format!("{};2;{r};{g};{b}", base + 8)),
        Color::Indexed(index) => Some(format!("{};5;{index}", base + 8)),
    }
}

/// Turns the bytes a terminal sends into key and paste events. Bytes that end in the middle of a sequence or a
/// character wait for the next ones; an Esc alone at the end of what arrived is the Esc key.
#[derive(Default)]
pub struct KeyParser {
    pending: Vec<u8>,
    /// Text being pasted, between the bracketed paste's start and end.
    paste: Option<Vec<u8>>,
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// The longest paste kept: the message box takes at most this much from one paste.
const MAX_PASTE: usize = 64 * 1024;

impl KeyParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.pending.extend_from_slice(bytes);
        let mut events = Vec::new();
        loop {
            if let Some(paste) = &mut self.paste {
                match find(&self.pending, PASTE_END) {
                    Some(end) => {
                        paste.extend(self.pending.drain(..end));
                        self.pending.drain(..PASTE_END.len());
                        let text = String::from_utf8_lossy(paste).into_owned();
                        self.paste = None;
                        events.push(Event::Paste(text));
                        continue;
                    }
                    None => {
                        // Keeps what could be the start of the end marker.
                        let keep = self.pending.len().min(PASTE_END.len() - 1);
                        let take = self.pending.len() - keep;
                        if paste.len() < MAX_PASTE {
                            paste.extend(self.pending.drain(..take));
                        } else {
                            self.pending.drain(..take);
                        }
                        return events;
                    }
                }
            }
            let Some((consumed, event)) = parse(&self.pending) else {
                return events;
            };
            self.pending.drain(..consumed);
            match event {
                Parsed::Key(code, modifiers) => {
                    events.push(Event::Key(KeyEvent::new(code, modifiers)))
                }
                Parsed::PasteStart => self.paste = Some(Vec::new()),
                Parsed::Nothing => {}
            }
        }
    }
}

enum Parsed {
    Key(KeyCode, KeyModifiers),
    PasteStart,
    Nothing,
}

/// The first event in `bytes` and how many bytes it takes; `None` if more bytes are needed.
fn parse(bytes: &[u8]) -> Option<(usize, Parsed)> {
    let key = |code| Parsed::Key(code, KeyModifiers::NONE);
    let first = *bytes.first()?;
    match first {
        0x1b => {
            if bytes.len() == 1 {
                return Some((1, key(KeyCode::Esc)));
            }
            match bytes[1] {
                b'[' => csi(bytes),
                b'O' => {
                    let code = match bytes.get(2)? {
                        b'P' => KeyCode::F(1),
                        b'Q' => KeyCode::F(2),
                        b'R' => KeyCode::F(3),
                        b'S' => KeyCode::F(4),
                        b'A' => KeyCode::Up,
                        b'B' => KeyCode::Down,
                        b'C' => KeyCode::Right,
                        b'D' => KeyCode::Left,
                        b'H' => KeyCode::Home,
                        b'F' => KeyCode::End,
                        _ => return Some((3, Parsed::Nothing)),
                    };
                    Some((3, key(code)))
                }
                0x1b => Some((1, key(KeyCode::Esc))),
                // Alt and a key: the key alone, Alt being of no use here but with Enter, a new line.
                _ => parse(&bytes[1..]).map(|(used, parsed)| {
                    let parsed = match parsed {
                        Parsed::Key(KeyCode::Enter, _) => {
                            Parsed::Key(KeyCode::Enter, KeyModifiers::ALT)
                        }
                        parsed => parsed,
                    };
                    (used + 1, parsed)
                }),
            }
        }
        b'\r' | b'\n' => Some((1, key(KeyCode::Enter))),
        b'\t' => Some((1, key(KeyCode::Tab))),
        0x7f | 0x08 => Some((1, key(KeyCode::Backspace))),
        0x01..=0x1a => Some((
            1,
            Parsed::Key(
                KeyCode::Char((b'a' + first - 1) as char),
                KeyModifiers::CONTROL,
            ),
        )),
        0x00..=0x1f => Some((1, Parsed::Nothing)),
        _ => {
            let length = match first {
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => 1,
            };
            let character = bytes.get(..length)?;
            Some(match std::str::from_utf8(character) {
                Ok(text) => (length, key(KeyCode::Char(text.chars().next()?))),
                Err(_) => (1, Parsed::Nothing),
            })
        }
    }
}

/// A control sequence, `ESC [` parameters and a final byte.
fn csi(bytes: &[u8]) -> Option<(usize, Parsed)> {
    let end = bytes[2..]
        .iter()
        .position(|byte| (0x40..=0x7e).contains(byte))?
        + 2;
    let used = end + 1;
    if bytes[..used] == *PASTE_START {
        return Some((used, Parsed::PasteStart));
    }
    let parameters = std::str::from_utf8(&bytes[2..end]).unwrap_or_default();
    let first: u8 = parameters
        .split(';')
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let code = match bytes[end] {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'Z' => return Some((used, Parsed::Key(KeyCode::BackTab, KeyModifiers::SHIFT))),
        b'~' => match first {
            1 | 7 => KeyCode::Home,
            2 => KeyCode::Insert,
            3 => KeyCode::Delete,
            4 | 8 => KeyCode::End,
            5 => KeyCode::PageUp,
            6 => KeyCode::PageDown,
            11..=15 => KeyCode::F(first - 10),
            17..=21 => KeyCode::F(first - 11),
            23 | 24 => KeyCode::F(first - 12),
            _ => return Some((used, Parsed::Nothing)),
        },
        _ => return Some((used, Parsed::Nothing)),
    };
    Some((used, Parsed::Key(code, KeyModifiers::NONE)))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::widgets::Paragraph;

    fn keys(parser: &mut KeyParser, bytes: &[u8]) -> Vec<KeyCode> {
        parser
            .feed(bytes)
            .into_iter()
            .filter_map(|event| match event {
                Event::Key(key) => Some(key.code),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_bytes_of_a_terminal_become_keys() {
        let mut parser = KeyParser::default();
        assert_eq!(
            keys(&mut parser, "hé\r\t\x7f".as_bytes()),
            [
                KeyCode::Char('h'),
                KeyCode::Char('é'),
                KeyCode::Enter,
                KeyCode::Tab,
                KeyCode::Backspace
            ]
        );
        assert_eq!(
            keys(&mut parser, b"\x1b[A\x1bOQ\x1b[13~\x1b[5~\x1b[Z"),
            [
                KeyCode::Up,
                KeyCode::F(2),
                KeyCode::F(3),
                KeyCode::PageUp,
                KeyCode::BackTab
            ]
        );
        // Ctrl+Y, and a sequence or a character cut between two reads.
        let events = parser.feed(b"\x19");
        assert!(
            matches!(&events[..], [Event::Key(key)] if key.code == KeyCode::Char('y') && key.modifiers == KeyModifiers::CONTROL)
        );
        // Alt+Enter, a new line in the message: Alt is kept with Enter only.
        let events = parser.feed(b"\x1b\r\x1bx");
        assert!(matches!(&events[..], [Event::Key(enter), Event::Key(x)]
                if enter.code == KeyCode::Enter && enter.modifiers == KeyModifiers::ALT
                    && x.code == KeyCode::Char('x') && x.modifiers == KeyModifiers::NONE));
        assert!(keys(&mut parser, b"\x1b[").is_empty());
        assert_eq!(keys(&mut parser, b"B"), [KeyCode::Down]);
        assert!(keys(&mut parser, &"é".as_bytes()[..1]).is_empty());
        assert_eq!(
            keys(&mut parser, &"é".as_bytes()[1..]),
            [KeyCode::Char('é')]
        );
        // Esc alone.
        assert_eq!(keys(&mut parser, b"\x1b"), [KeyCode::Esc]);
    }

    #[test]
    fn the_background_is_read_from_the_terminals_answer_and_keys_typed_meanwhile_are_kept() {
        let answer = b"x\x1b]11;rgb:1010/1313/1414\x1b\\\x1b[?62;22cy";
        assert_eq!(background_answer(&answer[..20]), None);
        assert_eq!(
            background_answer(answer),
            Some((Some([0x10, 0x13, 0x14]), b"xy".to_vec()))
        );
        // Two digits, ended by BEL.
        assert_eq!(
            background_answer(b"\x1b]11;rgb:ff/fd/f0\x07\x1b[?1;2c")
                .unwrap()
                .0,
            Some([0xFF, 0xFD, 0xF0])
        );
        // A terminal that does not tell its background answers the second question only.
        assert_eq!(background_answer(b"\x1b[?6c"), Some((None, Vec::new())));
        // What is not an X11 colour is no colour, whatever its length.
        assert_eq!(rgb_spec("rgb:ffff/0/8"), Some([0xFF, 0, 0x88]));
        for wrong in [
            "rgb:fffffffff/0/0",
            "rgb:ff/ff",
            "rgb:ff/ff/ff/ff",
            "rgb:+f/0/0",
            "rgb://",
            "#ffffff",
        ] {
            assert_eq!(rgb_spec(wrong), None, "{wrong}");
        }
    }

    #[test]
    fn a_bracketed_paste_is_one_event() {
        let mut parser = KeyParser::default();
        assert!(parser.feed(b"\x1b[200~line one\r\nline").is_empty());
        let events = parser.feed(b" two\x1b[201~x");
        assert!(matches!(&events[0], Event::Paste(text) if text == "line one\r\nline two"));
        assert!(matches!(&events[1], Event::Key(key) if key.code == KeyCode::Char('x')));
    }

    #[test]
    fn a_frame_is_written_as_ansi_for_the_size_reported() {
        let size = Arc::new(Mutex::new(Size::new(20, 3)));
        let mut written = Vec::new();
        {
            let mut terminal =
                Terminal::new(AnsiBackend::new(&mut written, Arc::clone(&size))).unwrap();
            terminal
                .draw(|frame| frame.render_widget(Paragraph::new("kelpy_"), frame.area()))
                .unwrap();
            assert_eq!(terminal.size().unwrap(), Size::new(20, 3));
        }
        let text = String::from_utf8(written).unwrap();
        assert!(text.contains("\x1b[1;1H"), "{text:?}");
        assert!(text.contains('k') && text.contains('_'), "{text:?}");
        assert_eq!(
            sgr(Color::Rgb(63, 192, 201), Color::Reset, Modifier::BOLD),
            "\x1b[0;1;38;2;63;192;201m"
        );
        assert_eq!(
            sgr(Color::DarkGray, Color::Red, Modifier::empty()),
            "\x1b[0;90;41m"
        );
    }
}
