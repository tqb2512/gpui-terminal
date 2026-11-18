//! Terminal session state and emulation.

use std::sync::{Arc, Mutex};

pub use alacritty_terminal::vte::ansi::CursorShape;
use alacritty_terminal::{
    Term,
    event::{Event, EventListener},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point, Side},
    selection::{Selection as GridSelection, SelectionType},
    term::{Config, Osc52, TermMode, cell::Flags, color::Colors},
    vte::ansi::{Color, NamedColor, Processor},
};

const PALETTE: [u32; 16] = [
    0x181b20, 0xd96b69, 0x92c781, 0xf0c674, 0x85aef2, 0xc49ae8, 0x7ccacf, 0xd3d8e0, 0x626d7c,
    0xff8986, 0xb2e19d, 0xffdb8a, 0xa5c6ff, 0xe2b8ff, 0x9de5ea, 0xe9ecf1,
];
pub const BACKGROUND: u32 = 0x181b20;
const FOREGROUND: u32 = 0xe9ecf1;

pub struct Terminal {
    grid: Term<Replies>,
    parser: Processor,
    replies: Replies,
}

#[derive(Clone, Copy)]
struct Size {
    columns: usize,
    rows: usize,
}

impl Size {
    fn new(columns: u16, rows: u16) -> Self {
        Self {
            columns: usize::from(columns.clamp(2, 512)),
            rows: usize::from(rows.clamp(1, 256)),
        }
    }
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

#[derive(Clone, Default)]
struct Replies(Arc<Mutex<Vec<u8>>>);

impl EventListener for Replies {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event
            && let Ok(mut replies) = self.0.lock()
        {
            replies.extend_from_slice(text.as_bytes());
        }
    }
}

#[derive(Clone, Copy)]
pub struct Selection {
    pub start: (u16, u16),
    pub end: (u16, u16),
}

#[derive(Default)]
pub struct KeyStroke {
    pub key: String,
    pub text: Option<String>,
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
}

pub enum TerminalInput {
    Key(KeyStroke),
    Paste(String),
    Resize {
        columns: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    },
    Scroll(i32),
    Select(Option<Selection>),
}

pub struct Snapshot {
    pub columns: usize,
    pub rows: usize,
    pub cells: Vec<Cell>,
    pub cursor: Option<Cursor>,
    pub scroll_offset: usize,
    pub selected_text: Option<String>,
}

pub struct Cell {
    pub text: String,
    pub width: u8,
    pub foreground: u32,
    pub background: u32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub selected: bool,
}

#[derive(Clone, Copy)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
    pub shape: CursorShape,
    pub color: u32,
}

impl Terminal {
    pub fn new(columns: u16, rows: u16) -> Self {
        let config = Config {
            osc52: Osc52::Disabled,
            ..Config::default()
        };
        let replies = Replies::default();
        Self {
            grid: Term::new(config, &Size::new(columns, rows), replies.clone()),
            parser: Processor::new(),
            replies,
        }
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.grid, bytes);
    }

    pub fn take_replies(&self) -> Vec<u8> {
        std::mem::take(
            &mut *self
                .replies
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    pub fn resize(&mut self, columns: u16, rows: u16) {
        self.grid.resize(Size::new(columns, rows));
    }

    pub fn snapshot(&self) -> Snapshot {
        let content = self.grid.renderable_content();
        let cursor = content.cursor;
        let cursor_row = (cursor.point.line.0 + content.display_offset as i32) as usize;
        let rows = self.grid.screen_lines();
        let cursor = (cursor.shape != CursorShape::Hidden && cursor_row < rows).then_some(Cursor {
            row: cursor_row,
            column: cursor.point.column.0,
            shape: cursor.shape,
            color: color(Color::Named(NamedColor::Cursor), content.colors),
        });
        let cells = content
            .display_iter
            .map(|indexed| {
                let cell = indexed.cell;
                let flags = cell.flags;
                let width = if flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    0
                } else if flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
                let text = if flags.contains(Flags::HIDDEN)
                    || width == 0
                    || (cell.c == ' ' && cell.zerowidth().is_none())
                {
                    String::new()
                } else {
                    std::iter::once(cell.c)
                        .chain(cell.zerowidth().into_iter().flatten().copied())
                        .collect()
                };
                let (mut fg, mut bg) = (
                    color(cell.fg, content.colors),
                    color(cell.bg, content.colors),
                );
                if flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                Cell {
                    text,
                    width,
                    foreground: fg,
                    background: bg,
                    bold: flags.contains(Flags::BOLD),
                    italic: flags.contains(Flags::ITALIC),
                    underline: flags.intersects(Flags::ALL_UNDERLINES),
                    selected: content
                        .selection
                        .is_some_and(|range| range.contains(indexed.point)),
                }
            })
            .collect();
        Snapshot {
            columns: self.grid.columns(),
            rows,
            cells,
            cursor,
            scroll_offset: content.display_offset,
            selected_text: self.selected_text(),
        }
    }

    pub fn encode_key(&self, key: &KeyStroke) -> Option<Vec<u8>> {
        let cursor_prefix = if self.grid.mode().contains(TermMode::APP_CURSOR) {
            b"\x1bO"
        } else {
            b"\x1b["
        };
        let arrow = match key.key.as_str() {
            "up" => Some(b'A'),
            "down" => Some(b'B'),
            "right" => Some(b'C'),
            "left" => Some(b'D'),
            "home" => Some(b'H'),
            "end" => Some(b'F'),
            _ => None,
        };
        let modifier = 1 + u8::from(key.shift) + 2 * u8::from(key.alt) + 4 * u8::from(key.control);
        if let Some(arrow) = arrow {
            return Some(if modifier == 1 {
                [cursor_prefix.as_slice(), &[arrow]].concat()
            } else {
                format!("\x1b[1;{modifier}{}", char::from(arrow)).into_bytes()
            });
        }
        let sequence = match key.key.as_str() {
            "enter" if self.grid.mode().contains(TermMode::LINE_FEED_NEW_LINE) => {
                b"\r\n".as_slice()
            }
            "enter" => b"\r",
            "backspace" => b"\x7f",
            "tab" if key.shift => b"\x1b[Z",
            "tab" => b"\t",
            "escape" => b"\x1b",
            "insert" => b"\x1b[2~",
            "delete" => b"\x1b[3~",
            "pageup" => b"\x1b[5~",
            "pagedown" => b"\x1b[6~",
            "f1" => b"\x1bOP",
            "f2" => b"\x1bOQ",
            "f3" => b"\x1bOR",
            "f4" => b"\x1bOS",
            "f5" => b"\x1b[15~",
            "f6" => b"\x1b[17~",
            "f7" => b"\x1b[18~",
            "f8" => b"\x1b[19~",
            "f9" => b"\x1b[20~",
            "f10" => b"\x1b[21~",
            "f11" => b"\x1b[23~",
            "f12" => b"\x1b[24~",
            _ if key.control && key.key.len() == 1 => {
                let byte = key.key.as_bytes().first()?.to_ascii_uppercase();
                let code = match byte {
                    b'@'..=b'_' => byte & 0x1f,
                    b' ' => 0,
                    b'?' => 127,
                    _ => return None,
                };
                return Some(if key.alt { vec![27, code] } else { vec![code] });
            }
            _ => key.text.as_deref()?.as_bytes(),
        };
        Some(if key.alt {
            [b"\x1b", sequence].concat()
        } else {
            sequence.to_vec()
        })
    }

    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        let clean = text.replace("\r\n", "\n");
        let clean: String = clean
            .chars()
            .filter(|ch| !ch.is_control() || matches!(ch, '\t' | '\n' | '\r'))
            .collect();
        if self.grid.mode().contains(TermMode::BRACKETED_PASTE) {
            [b"\x1b[200~", clean.as_bytes(), b"\x1b[201~"].concat()
        } else {
            clean.replace('\n', "\r").into_bytes()
        }
    }

    pub fn scroll(&mut self, lines: i32) {
        self.grid
            .scroll_display(Scroll::Delta(lines.clamp(-10_000, 10_000)));
    }

    pub fn select(&mut self, selection: Option<Selection>) {
        let point = |(row, column): (u16, u16)| {
            let row = usize::from(row).min(self.grid.screen_lines().saturating_sub(1));
            let col = usize::from(column).min(self.grid.columns().saturating_sub(1));
            Point::new(
                Line(row as i32 - self.grid.grid().display_offset() as i32),
                Column(col),
            )
        };
        self.grid.selection = selection.map(|range| {
            let (start, end) = (point(range.start), point(range.end));
            let (start, end) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            let mut selection = GridSelection::new(SelectionType::Simple, start, Side::Left);
            selection.update(end, Side::Right);
            selection
        });
    }

    pub fn selected_text(&self) -> Option<String> {
        self.grid.selection_to_string()
    }
}

fn color(color: Color, colors: &Colors) -> u32 {
    let rgb = |r: u8, g: u8, b: u8| u32::from_be_bytes([0, r, g, b]);
    let index = match color {
        Color::Spec(value) => return rgb(value.r, value.g, value.b),
        Color::Named(value) => value as usize,
        Color::Indexed(value) => usize::from(value),
    };
    if let Some(value) = colors[index] {
        return rgb(value.r, value.g, value.b);
    }
    match index {
        0..=15 => PALETTE[index],
        16..=231 => {
            let index = index - 16;
            let channel = |value| {
                if value == 0 {
                    0
                } else {
                    (55 + 40 * value) as u8
                }
            };
            rgb(
                channel(index / 36),
                channel(index / 6 % 6),
                channel(index % 6),
            )
        }
        232..=255 => {
            let gray = (8 + 10 * (index - 232)) as u8;
            rgb(gray, gray, gray)
        }
        257 | 268 => BACKGROUND,
        259..=266 => PALETTE[index - 259],
        _ => FOREGROUND,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_utf8_and_escape_sequences_render_in_order() {
        let mut term = Terminal::new(8, 2);
        term.advance(b"\x1b[38;2;12;");
        term.advance(b"34;56m\xe7");
        term.advance(b"\x95\x8c");
        let frame = term.snapshot();
        assert_eq!(frame.cells[0].text, "界");
        assert_eq!(frame.cells[0].foreground, 0x0c2238);
        assert_eq!(frame.cells[0].width, 2);
        assert_eq!(frame.cursor.map(|cursor| cursor.column), Some(2));
    }

    #[test]
    fn history_is_bounded_and_copy_excludes_cell_padding() {
        let mut term = Terminal::new(12, 2);
        for line in 0..10_100 {
            term.advance(format!("{line:05}\r\n").as_bytes());
        }
        term.scroll(i32::MAX);
        let frame = term.snapshot();
        assert_eq!(frame.scroll_offset, 10_000);
        assert_eq!(
            frame
                .cells
                .iter()
                .take(5)
                .map(|cell| cell.text.as_str())
                .collect::<String>(),
            "00099"
        );
        term.select(Some(Selection {
            start: (0, 0),
            end: (0, 11),
        }));
        assert_eq!(term.selected_text().as_deref(), Some("00099"));
    }

    #[test]
    fn key_and_paste_follow_terminal_modes() {
        let mut term = Terminal::new(80, 24);
        let up = KeyStroke {
            key: "up".into(),
            ..Default::default()
        };
        assert_eq!(term.encode_key(&up).as_deref(), Some(b"\x1b[A".as_slice()));
        assert_eq!(term.encode_paste("a\nb"), b"a\rb");
        term.advance(b"\x1b[?1h\x1b[?2004h");
        assert_eq!(term.encode_key(&up).as_deref(), Some(b"\x1bOA".as_slice()));
        assert_eq!(term.encode_paste("a\nb"), b"\x1b[200~a\nb\x1b[201~");
    }
}
