use std::{ops::Range, path::PathBuf};

use gpui::{
    App, Bounds, ClipboardItem, Context, ElementInputHandler, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyDownEvent, MouseButton, Pixels, Render, SharedString, TextRun, UTF16Selection,
    Window, canvas, div, fill, point, prelude::*, px, rgb, size,
};
use hosts::ProfileDraft;
use ssh::{AuthMethod, Target};
use unicode_segmentation::UnicodeSegmentation;
use zeroize::Zeroizing;

pub fn profile_draft(
    label: &str,
    target: &str,
    key: bool,
    path: &str,
) -> Result<ProfileDraft, String> {
    let label = label.trim();
    if label.is_empty() || label.chars().count() > 256 || label.chars().any(char::is_control) {
        return Err("Enter a host name between 1 and 256 characters.".into());
    }
    Ok(ProfileDraft {
        label: label.into(),
        target: Target::parse(target).map_err(|error| error.to_string())?,
        auth: auth_method(key, path),
    })
}

pub fn auth_method(key: bool, path: &str) -> AuthMethod {
    if key {
        AuthMethod::PrivateKey {
            path: (!path.trim().is_empty()).then(|| PathBuf::from(path.trim())),
        }
    } else {
        AuthMethod::Password
    }
}

struct FieldBuffer {
    text: Zeroizing<String>,
    selection: Range<usize>,
    marked: Option<Range<usize>>,
    secret: bool,
}

impl FieldBuffer {
    fn new(text: &str, secret: bool) -> Self {
        Self {
            text: Zeroizing::new(text.into()),
            selection: text.len()..text.len(),
            marked: None,
            secret,
        }
    }

    fn display(&self) -> String {
        if self.secret {
            "•".repeat(self.text.encode_utf16().count())
        } else {
            self.text.to_string()
        }
    }

    fn display_index(&self, index: usize) -> usize {
        if self.secret {
            self.text[..index].encode_utf16().count() * '•'.len_utf8()
        } else {
            index
        }
    }

    fn clear(&mut self) {
        self.text = Zeroizing::default();
        self.selection = 0..0;
        self.marked = None;
    }

    fn take(&mut self) -> Zeroizing<String> {
        let text = std::mem::take(&mut self.text);
        self.clear();
        text
    }

    fn replace(&mut self, range: Option<Range<usize>>, text: &str) -> Range<usize> {
        let range = range
            .or(self.marked.take())
            .unwrap_or_else(|| self.selection.clone());
        let limit = 8192usize.saturating_sub(self.text.len().saturating_sub(range.len()));
        let mut replacement = Zeroizing::new(String::with_capacity(self.text.len().min(8192)));
        replacement.push_str(&self.text[..range.start]);
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            if replacement.len() - range.start + ch.len_utf8() > limit {
                break;
            }
            replacement.push(ch);
        }
        let end = replacement.len();
        replacement.push_str(&self.text[range.end..]);
        self.text = replacement;
        self.selection = end..end;
        self.marked = None;
        range.start..end
    }

    fn byte_offset(&self, utf16: usize) -> usize {
        let mut units = 0;
        for (index, ch) in self.text.char_indices() {
            units += ch.len_utf16();
            if units > utf16 {
                return index;
            }
        }
        self.text.len()
    }

    fn utf16_range(&self, range: Range<usize>) -> Range<usize> {
        self.text[..range.start].encode_utf16().count()
            ..self.text[..range.end].encode_utf16().count()
    }
}

pub struct Field {
    focus: FocusHandle,
    label: SharedString,
    buffer: FieldBuffer,
}

pub struct FieldEdit;
impl EventEmitter<FieldEdit> for Field {}

impl Field {
    pub fn new(label: &'static str, initial: &str, secret: bool, cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle().tab_stop(true),
            label: label.into(),
            buffer: FieldBuffer::new(initial, secret),
        }
    }

    pub fn text(&self) -> &str {
        &self.buffer.text
    }
    pub fn take_secret(&mut self, cx: &mut Context<Self>) -> Zeroizing<String> {
        let text = self.buffer.take();
        cx.notify();
        text
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.buffer.clear();
        cx.notify();
    }

    fn edited(&self, cx: &mut Context<Self>) {
        cx.emit(FieldEdit);
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let name = key.key.as_str();
        let range = self.buffer.selection.clone();
        let previous = self.buffer.text[..range.start]
            .grapheme_indices(true)
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        let next = self.buffer.text[range.end..]
            .graphemes(true)
            .next()
            .map_or(range.end, |item| range.end + item.len());
        if key.modifiers.control {
            match name {
                "a" => self.buffer.selection = 0..self.buffer.text.len(),
                "c" | "x" if !self.buffer.secret && !range.is_empty() => {
                    cx.write_to_clipboard(ClipboardItem::new_string(
                        self.buffer.text[range.clone()].to_owned(),
                    ));
                    if name == "x" {
                        self.buffer.replace(Some(range), "");
                        self.edited(cx);
                    }
                }
                "c" | "x" => {}
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        self.buffer.replace(None, &Zeroizing::new(text));
                        self.edited(cx);
                    }
                }
                _ => return,
            }
        } else {
            match name {
                "left" => {
                    let index = if range.is_empty() || key.modifiers.shift {
                        previous
                    } else {
                        range.start
                    };
                    self.buffer.selection = if key.modifiers.shift {
                        index..range.end
                    } else {
                        index..index
                    };
                }
                "right" => {
                    let index = if range.is_empty() || key.modifiers.shift {
                        next
                    } else {
                        range.end
                    };
                    self.buffer.selection = if key.modifiers.shift {
                        range.start..index
                    } else {
                        index..index
                    };
                }
                "home" => {
                    self.buffer.selection = if key.modifiers.shift {
                        0..range.end
                    } else {
                        0..0
                    }
                }
                "end" => {
                    let end = self.buffer.text.len();
                    self.buffer.selection = if key.modifiers.shift {
                        range.start..end
                    } else {
                        end..end
                    };
                }
                "backspace" | "delete" => {
                    let range = if !range.is_empty() {
                        range
                    } else if name == "backspace" {
                        previous..range.start
                    } else {
                        range.end..next
                    };
                    self.buffer.replace(Some(range), "");
                    self.edited(cx);
                }
                _ => return,
            }
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Focusable for Field {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Field {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = self.buffer.display();
        let selection = self.buffer.display_index(self.buffer.selection.start)
            ..self.buffer.display_index(self.buffer.selection.end);
        let entity = cx.entity();
        let focus = self.focus.clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0xb7c1d0))
                    .child(self.label.clone()),
            )
            .child(
                div()
                    .track_focus(&self.focus)
                    .w_full()
                    .on_key_down(cx.listener(Self::on_key))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|field, _, window, _| field.focus.focus(window)),
                    )
                    .border_1()
                    .border_color(rgb(0x525f71))
                    .focus(|style| style.border_color(rgb(0x9dbefa)))
                    .bg(rgb(0x20252d))
                    .rounded_sm()
                    .overflow_hidden()
                    .px_2()
                    .child(
                        canvas(
                            |_bounds, _, _| (),
                            move |bounds, (), window, cx| {
                                window.handle_input(
                                    &focus,
                                    ElementInputHandler::new(bounds, entity),
                                    cx,
                                );
                                let run = TextRun {
                                    len: text.len(),
                                    font: window.text_style().font(),
                                    color: rgb(0xe9ecf1).into(),
                                    background_color: None,
                                    underline: None,
                                    strikethrough: None,
                                };
                                let line = window.text_system().shape_line(
                                    text.into(),
                                    px(13.),
                                    &[run],
                                    None,
                                );
                                let left = line.x_for_index(selection.start);
                                let right = line.x_for_index(selection.end);
                                let x = bounds.origin.x
                                    - (right + px(3.) - bounds.size.width).max(px(0.));
                                if focus.is_focused(window) {
                                    let selected = Bounds::new(
                                        point(x + left, bounds.origin.y + px(5.)),
                                        size((right - left).max(px(1.)), px(18.)),
                                    );
                                    window.paint_quad(fill(selected, rgb(0x5979a4)));
                                }
                                if let Err(error) = line.paint(
                                    point(x, bounds.origin.y),
                                    bounds.size.height,
                                    window,
                                    cx,
                                ) {
                                    let _ = error;
                                }
                            },
                        )
                        .w_full()
                        .h(px(28.)),
                    ),
            )
    }
}

impl gpui::EntityInputHandler for Field {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let byte = self.buffer.byte_offset(range.start)..self.buffer.byte_offset(range.end);
        let utf16 = self.buffer.utf16_range(byte.clone());
        let result = if self.buffer.secret {
            "•".repeat(utf16.len())
        } else {
            self.buffer.text.get(byte)?.to_owned()
        };
        *actual = Some(utf16);
        Some(result)
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.buffer.utf16_range(self.buffer.selection.clone()),
            reversed: false,
        })
    }
    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.buffer
            .marked
            .clone()
            .map(|range| self.buffer.utf16_range(range))
    }
    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.buffer.marked = None;
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|range| self.buffer.byte_offset(range.start)..self.buffer.byte_offset(range.end));
        self.buffer.replace(range, text);
        self.edited(cx);
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _selection: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|range| self.buffer.byte_offset(range.start)..self.buffer.byte_offset(range.end));
        let marked = self.buffer.replace(range, text);
        self.buffer.marked = (!marked.is_empty()).then_some(marked);
        self.edited(cx);
    }
    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(bounds)
    }
    fn character_index_for_point(
        &mut self,
        _position: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_editor_validates_and_clears_secrets_on_cancel() {
        assert!(profile_draft("Build", "ops@host:0", false, "").is_err());
        assert!(profile_draft("Build", "ops@bad/host", true, "").is_err());
        let mut secret = FieldBuffer::new("", true);
        secret.replace(None, "fixture password");
        assert!(!secret.display().contains("fixture password"));
        secret.clear();
        assert!(secret.display().is_empty());
        assert!(secret.take().is_empty());
    }
}
