//! GPUI presentation for terminal sessions.

#![cfg(windows)]

use gpui::{App, IntoElement, RenderOnce, Window, div, prelude::*, rgb};

#[derive(IntoElement)]
pub struct TerminalView;

impl RenderOnce for TerminalView {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x181b20))
            .text_color(rgb(0xe9ecf1))
            .child("No active session")
    }
}
