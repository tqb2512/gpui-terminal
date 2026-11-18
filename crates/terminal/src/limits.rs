use alacritty_terminal::{
    Term,
    term::cell::Flags,
    vte::ansi::{
        Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, KeyboardModes,
        KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb,
        StandardCharset, TabulationClearMode,
    },
};
use unicode_width::UnicodeWidthChar;

use super::{Replies, TerminalError};

#[derive(Default)]
pub(super) enum ControlSequence {
    #[default]
    Text,
    Escape,
    Osc(usize),
}

impl ControlSequence {
    // VTE's std parser retains an entire OSC string before dispatch.
    pub(super) fn check(&mut self, bytes: &[u8]) -> Result<(), TerminalError> {
        for byte in bytes.iter().copied() {
            *self = match (&*self, byte) {
                (_, 0x1b) => Self::Escape,
                (Self::Escape, b']') => Self::Osc(0),
                (Self::Escape, 0..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f..=0xff) => Self::Escape,
                (Self::Osc(_), 0x07 | 0x18 | 0x1a) => Self::Text,
                (Self::Osc(8192), _) => return Err(TerminalError::ControlSequence),
                (Self::Osc(length), _) => Self::Osc(length + 1),
                _ => Self::Text,
            };
        }
        Ok(())
    }
}

pub(super) struct BoundedInput<'a> {
    term: &'a mut Term<Replies>,
    exceeded: bool,
}

impl<'a> BoundedInput<'a> {
    pub(super) fn new(term: &'a mut Term<Replies>) -> Self {
        Self {
            term,
            exceeded: false,
        }
    }

    pub(super) fn result(&self) -> Result<(), TerminalError> {
        if self.exceeded {
            Err(TerminalError::CombiningMarks)
        } else {
            Ok(())
        }
    }
}

// Keep normal ANSI handling in Alacritty; intercept only allocation into a single cell.
macro_rules! forward {
    ($($method:ident($($name:ident: $type:ty),*);)*) => {
        $(fn $method(&mut self, $($name: $type),*) {
            self.term.$method($($name),*);
        })*
    };
}

impl Handler for BoundedInput<'_> {
    fn input(&mut self, ch: char) {
        if self.exceeded {
            return;
        }
        if ch.width() == Some(0) {
            let grid = self.term.grid();
            let mut location = grid.cursor.point;
            if !grid.cursor.input_needs_wrap {
                location.column.0 = location.column.0.saturating_sub(1);
            }
            if grid[location].flags.contains(Flags::WIDE_CHAR_SPACER) {
                location.column.0 = location.column.0.saturating_sub(1);
            }
            if grid[location]
                .zerowidth()
                .is_some_and(|marks| marks.len() >= 16)
            {
                self.exceeded = true;
                return;
            }
        }
        self.term.input(ch);
    }

    forward! {
        set_title(title: Option<String>);
        set_cursor_style(style: Option<CursorStyle>);
        set_cursor_shape(shape: CursorShape);
        goto(line: i32, column: usize);
        goto_line(line: i32);
        goto_col(column: usize);
        insert_blank(count: usize);
        move_up(count: usize);
        move_down(count: usize);
        identify_terminal(intermediate: Option<char>);
        device_status(status: usize);
        move_forward(columns: usize);
        move_backward(columns: usize);
        move_down_and_cr(rows: usize);
        move_up_and_cr(rows: usize);
        put_tab(count: u16);
        backspace();
        carriage_return();
        linefeed();
        bell();
        substitute();
        newline();
        set_horizontal_tabstop();
        scroll_up(rows: usize);
        scroll_down(rows: usize);
        insert_blank_lines(rows: usize);
        delete_lines(rows: usize);
        erase_chars(count: usize);
        delete_chars(count: usize);
        move_backward_tabs(count: u16);
        move_forward_tabs(count: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_line(mode: LineClearMode);
        clear_screen(mode: ClearMode);
        clear_tabs(mode: TabulationClearMode);
        set_tabs(interval: u16);
        reset_state();
        reverse_index();
        terminal_attribute(attr: Attr);
        set_mode(mode: Mode);
        unset_mode(mode: Mode);
        report_mode(mode: Mode);
        set_private_mode(mode: PrivateMode);
        unset_private_mode(mode: PrivateMode);
        report_private_mode(mode: PrivateMode);
        set_scrolling_region(top: usize, bottom: Option<usize>);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(charset: CharsetIndex);
        configure_charset(charset: CharsetIndex, standard: StandardCharset);
        set_color(index: usize, color: Rgb);
        dynamic_color_sequence(prefix: String, index: usize, terminator: &str);
        reset_color(index: usize);
        decaln();
        push_title();
        pop_title();
        text_area_size_pixels();
        text_area_size_chars();
        report_keyboard_mode();
        push_keyboard_mode(mode: KeyboardModes);
        pop_keyboard_modes(count: u16);
        set_keyboard_mode(mode: KeyboardModes, behavior: KeyboardModesApplyBehavior);
        set_modify_other_keys(mode: ModifyOtherKeys);
        report_modify_other_keys();
    }
}
