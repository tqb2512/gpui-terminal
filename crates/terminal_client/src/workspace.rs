use gpui::{Context, IntoElement, Render, Window};
use terminal_view::TerminalView;

pub(crate) struct Workspace;

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        TerminalView
    }
}
