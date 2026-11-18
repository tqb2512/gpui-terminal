//! GPUI presentation for terminal sessions.

#![cfg(windows)]

use std::{cell::Cell, ops::Range, rc::Rc, sync::Arc};

use gpui::{
    App, Bounds, ClipboardItem, Context, ElementInputHandler, EventEmitter, FocusHandle, Focusable,
    FontStyle, FontWeight, IntoElement, KeyBinding, KeyDownEvent, MouseButton, Pixels, Point,
    Render, TextRun, UTF16Selection, UnderlineStyle, Window, actions, canvas, div, fill, font,
    point, prelude::*, px, rgb, size,
};
use terminal::{BACKGROUND, CursorShape, KeyStroke, Selection, Snapshot, TerminalInput};

actions!(terminal, [CopySelection, PasteClipboard]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-shift-c", CopySelection, Some("Terminal")),
        KeyBinding::new("ctrl-shift-v", PasteClipboard, Some("Terminal")),
    ]);
}

#[derive(Clone, Copy, PartialEq)]
pub struct Viewport {
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl From<Viewport> for TerminalInput {
    fn from(viewport: Viewport) -> Self {
        Self::Resize {
            columns: viewport.columns,
            rows: viewport.rows,
            pixel_width: viewport.pixel_width,
            pixel_height: viewport.pixel_height,
        }
    }
}

pub enum ViewEvent {
    Input(TerminalInput),
    Resize(Viewport),
}

pub struct TerminalView {
    frame: Option<Arc<Snapshot>>,
    focus: FocusHandle,
    geometry: Rc<Cell<Option<Geometry>>>,
    drag_start: Option<(u16, u16)>,
    composing: bool,
}

impl EventEmitter<ViewEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TerminalView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            frame: None,
            focus: cx.focus_handle().tab_stop(true),
            geometry: Rc::default(),
            drag_start: None,
            composing: false,
        }
    }

    pub fn set_frame(&mut self, frame: Option<Arc<Snapshot>>, cx: &mut Context<Self>) {
        self.frame = frame;
        cx.notify();
    }

    pub fn reset(&mut self) {
        self.drag_start = None;
        self.composing = false;
    }

    pub fn viewport(&self) -> Option<Viewport> {
        self.geometry.get().map(Geometry::viewport)
    }

    fn emit(input: TerminalInput, cx: &mut Context<Self>) {
        cx.emit(ViewEvent::Input(input));
    }

    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let mods = key.modifiers;
        if mods.control && key.key == "tab" {
            if mods.shift {
                window.focus_prev();
            } else {
                window.focus_next();
            }
            cx.stop_propagation();
            return;
        }
        if key.key_char.is_some() && (mods.control == mods.alt) {
            return;
        }
        if key.key == "space" && !mods.control && !mods.alt {
            return;
        }
        Self::emit(
            TerminalInput::Key(KeyStroke {
                key: if mods.control && key.key == "space" {
                    " ".into()
                } else {
                    key.key.clone()
                },
                text: key.key_char.clone(),
                control: mods.control,
                alt: mods.alt,
                shift: mods.shift,
            }),
            cx,
        );
        cx.stop_propagation();
    }

    fn coordinates(&self, position: Point<Pixels>) -> Option<(u16, u16)> {
        let geometry = self.geometry.get()?;
        let viewport = geometry.viewport();
        Some((
            ((position.y - geometry.bounds.top()) / geometry.row_height)
                .floor()
                .max(0.) as u16,
            (((position.x - geometry.bounds.left()) / geometry.cell_width)
                .floor()
                .max(0.) as u16)
                .min(viewport.columns.saturating_sub(1)),
        ))
    }
}

#[derive(Clone, Copy)]
struct Geometry {
    bounds: Bounds<Pixels>,
    cell_width: Pixels,
    row_height: Pixels,
    scale: f32,
}

impl Geometry {
    fn viewport(self) -> Viewport {
        Viewport {
            columns: ((self.bounds.size.width / self.cell_width).floor() as u16).clamp(2, 512),
            rows: ((self.bounds.size.height / self.row_height).floor() as u16).clamp(1, 256),
            pixel_width: (f32::from(self.bounds.size.width) * self.scale).round() as u16,
            pixel_height: (f32::from(self.bounds.size.height) * self.scale).round() as u16,
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let frame = self.frame.clone();
        let geometry = self.geometry.clone();
        let focus = self.focus.clone();
        let entity = cx.weak_entity();
        div()
            .size_full()
            .overflow_hidden()
            .bg(rgb(BACKGROUND))
            .px_2()
            .py_1()
            .track_focus(&self.focus)
            .key_context("Terminal")
            .on_action(cx.listener(|view, _: &CopySelection, _window, cx| {
                if let Some(text) = view
                    .frame
                    .as_ref()
                    .and_then(|frame| frame.selected_text.clone())
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }))
            .on_action(cx.listener(|_, _: &PasteClipboard, _window, cx| {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    Self::emit(TerminalInput::Paste(text), cx);
                }
            }))
            .on_key_down(cx.listener(Self::key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &gpui::MouseDownEvent, window, cx| {
                    view.focus.focus(window);
                    view.drag_start = view.coordinates(event.position);
                    Self::emit(TerminalInput::Select(None), cx);
                }),
            )
            .on_mouse_move(
                cx.listener(|view, event: &gpui::MouseMoveEvent, _window, cx| {
                    if let Some(start) = view.drag_start
                        && let Some(end) = view.coordinates(event.position)
                    {
                        Self::emit(TerminalInput::Select(Some(Selection { start, end })), cx);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _, _window, _| view.drag_start = None),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, _, _window, _| view.drag_start = None),
            )
            .on_scroll_wheel(
                cx.listener(|_, event: &gpui::ScrollWheelEvent, _window, cx| {
                    let lines = (event.delta.pixel_delta(px(18.)).y / px(18.)).round() as i32;
                    if lines != 0 {
                        Self::emit(TerminalInput::Scroll(lines), cx);
                    }
                }),
            )
            .child(
                canvas(
                    |_bounds, _, _| (),
                    move |bounds, (), window, cx| {
                        let Some(view) = entity.upgrade() else {
                            return;
                        };
                        window.handle_input(&focus, ElementInputHandler::new(bounds, view), cx);
                        let style = font("Consolas");
                        let font_size = px(13.);
                        let run = TextRun {
                            len: 1,
                            font: style.clone(),
                            color: rgb(0xe9ecf1).into(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        let reference =
                            window
                                .text_system()
                                .shape_line("M".into(), font_size, &[run], None);
                        let current = Geometry {
                            bounds,
                            cell_width: reference.width.max(px(1.)),
                            row_height: px(18.),
                            scale: window.scale_factor(),
                        };
                        let changed = geometry.replace(Some(current)).map(Geometry::viewport)
                            != Some(current.viewport());
                        if changed {
                            window.defer(cx, move |_, cx| {
                                let _ = entity.update(cx, |_, cx| {
                                    cx.emit(ViewEvent::Resize(current.viewport()))
                                });
                            });
                        }
                        if let Some(frame) = frame {
                            paint_frame(window, cx, current, &frame, style, font_size);
                        }
                    },
                )
                .size_full(),
            )
    }
}

fn paint_frame(
    window: &mut Window,
    cx: &mut App,
    geometry: Geometry,
    frame: &Snapshot,
    style: gpui::Font,
    font_size: Pixels,
) {
    let viewport = geometry.viewport();
    for (index, cell) in frame.cells.iter().enumerate() {
        let row = index / frame.columns;
        let column = index % frame.columns;
        if row >= usize::from(viewport.rows) || column >= usize::from(viewport.columns) {
            continue;
        }
        let origin = geometry.bounds.origin
            + point(
                geometry.cell_width * column as f32,
                geometry.row_height * row as f32,
            );
        let bounds = Bounds::new(origin, size(geometry.cell_width, geometry.row_height));
        let cursor = frame
            .cursor
            .filter(|at| at.row == row && at.column == column);
        let mut foreground = cell.foreground;
        let mut background = if cell.selected {
            0x3e608d
        } else {
            cell.background
        };
        if let Some(cursor) = cursor
            && cursor.shape == CursorShape::Block
        {
            foreground = BACKGROUND;
            background = cursor.color;
        }
        if background != BACKGROUND {
            window.paint_quad(fill(bounds, rgb(background)));
        }
        if let Some(cursor) = cursor {
            match cursor.shape {
                CursorShape::Beam => window.paint_quad(fill(
                    Bounds::new(origin, size(px(2.), geometry.row_height)),
                    rgb(cursor.color),
                )),
                CursorShape::Underline => window.paint_quad(fill(
                    Bounds::new(
                        origin + point(px(0.), geometry.row_height - px(2.)),
                        size(geometry.cell_width, px(2.)),
                    ),
                    rgb(cursor.color),
                )),
                _ => {}
            }
        }
        if cell.text.is_empty() {
            continue;
        }
        let run = TextRun {
            len: cell.text.len(),
            font: gpui::Font {
                weight: if cell.bold {
                    FontWeight::BOLD
                } else {
                    FontWeight::NORMAL
                },
                style: if cell.italic {
                    FontStyle::Italic
                } else {
                    FontStyle::Normal
                },
                ..style.clone()
            },
            color: rgb(foreground).into(),
            background_color: None,
            underline: cell.underline.then_some(UnderlineStyle {
                color: None,
                thickness: px(1.),
                wavy: false,
            }),
            strikethrough: None,
        };
        let line =
            window
                .text_system()
                .shape_line(cell.text.clone().into(), font_size, &[run], None);
        if line.paint(origin, geometry.row_height, window, cx).is_err() {
            window.paint_quad(fill(
                Bounds::new(origin + point(px(2.), px(7.)), size(px(3.), px(3.))),
                rgb(foreground),
            ));
        }
    }
}

impl gpui::EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }
    fn marked_text_range(
        &self,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.composing.then_some(0..0)
    }
    fn unmark_text(&mut self, _window: &mut Window, _: &mut Context<Self>) {
        self.composing = false;
    }
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composing = false;
        if !text.is_empty() {
            Self::emit(
                TerminalInput::Key(KeyStroke {
                    text: Some(text.into()),
                    ..KeyStroke::default()
                }),
                cx,
            );
        }
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
        self.composing = !text.is_empty();
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let geometry = self.geometry.get()?;
        let (row, column) = self
            .frame
            .as_ref()
            .and_then(|frame| frame.cursor)
            .map_or((0, 0), |cursor| (cursor.row, cursor.column));
        Some(Bounds::new(
            bounds.origin
                + point(
                    geometry.cell_width * column as f32,
                    geometry.row_height * row as f32,
                ),
            size(geometry.cell_width, geometry.row_height),
        ))
    }
    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}
