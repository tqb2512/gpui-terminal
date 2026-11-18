use std::{collections::HashMap, future::Future, path::PathBuf, sync::Arc};

use gpui::{
    AnyElement, App, ClickEvent, Context, ElementId, Entity, Focusable, IntoElement, KeyDownEvent,
    Render, SharedString, Subscription, Task, Window, div, prelude::*, px, rgb,
};
use hosts::{Profile, ProfileId};
use ssh::{AuthMethod, Endpoint, Target};
use terminal::Snapshot;
use terminal_client::{
    AttemptId, ConnectionEvent, ConnectionEventKind, ConnectionSpec, Connections, ManagerError,
    PromptId, PromptKind, PromptResponse, TabId,
};
use terminal_view::{TerminalView, ViewEvent, Viewport};

use crate::{
    files_view::{FilesView, edits_root},
    profile_form::{Field, FieldEdit, auth_method, profile_draft},
};

pub(crate) struct Workspace {
    connections: Option<Connections>,
    profiles: Vec<Profile>,
    search: Entity<Field>,
    editor: Option<Editor>,
    tabs: Vec<Tab>,
    active: Option<TabId>,
    terminal: Entity<TerminalView>,
    confirm: Option<Confirm>,
    message: Option<String>,
    file_warning: Option<String>,
    file_warnings: tokio::sync::mpsc::Sender<String>,
    hosts_visible: bool,
    pending: HashMap<u64, Task<()>>,
    serial: u64,
    _startup: Task<()>,
    _file_warnings: Task<()>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy)]
enum EditorMode {
    Quick,
    New,
    Edit(ProfileId),
}

struct Editor {
    mode: EditorMode,
    label: Entity<Field>,
    address: Entity<Field>,
    path: Entity<Field>,
    key: bool,
}

struct Tab {
    attempt: AttemptId,
    label: String,
    endpoint: Endpoint,
    status: String,
    connected: bool,
    finished: bool,
    prompt: Option<(PromptId, FormPrompt)>,
    frame: Option<Arc<Snapshot>>,
    viewport: Option<Viewport>,
    files: Option<Entity<FilesView>>,
    showing_files: bool,
}

enum FormPrompt {
    Host {
        endpoint: Endpoint,
        fingerprint: String,
    },
    Password(Entity<Field>),
    PrivateKey(Entity<Field>),
    Passphrase(Entity<Field>),
}

impl FormPrompt {
    fn field(&self) -> Option<&Entity<Field>> {
        match self {
            Self::Host { .. } => None,
            Self::Password(field) | Self::PrivateKey(field) | Self::Passphrase(field) => {
                Some(field)
            }
        }
    }
}

#[derive(Clone)]
enum Confirm {
    Delete(ProfileId, String),
    Forget(Endpoint),
}

impl Workspace {
    pub fn new(runtime: tokio::runtime::Handle, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| Field::new("Search saved hosts", "", false, cx));
        let terminal = cx.new(TerminalView::new);
        let (file_warnings, mut warnings) = tokio::sync::mpsc::channel(16);
        let warning_task = cx.spawn(async move |weak, cx| {
            while let Some(warning) = warnings.recv().await {
                if weak
                    .update(cx, |view, cx| {
                        view.file_warning = Some(warning);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let subscriptions = vec![
            cx.subscribe(&search, |_, _, _: &FieldEdit, cx| cx.notify()),
            cx.subscribe(&terminal, |view, _, event: &ViewEvent, cx| {
                view.on_terminal(event, cx)
            }),
        ];
        let startup = cx.spawn(async move |weak, cx| {
            let result = match std::env::var_os("LOCALAPPDATA") {
                Some(path) => {
                    Connections::open(&runtime, &PathBuf::from(path).join("GPUI Terminal Client"))
                        .await
                }
                None => Err(ManagerError::Directory),
            };
            match result {
                Ok((manager, mut events)) => {
                    if weak
                        .update(cx, |workspace, cx| {
                            workspace.connections = Some(manager);
                            workspace.message = None;
                            workspace.refresh(cx);
                            cx.notify();
                        })
                        .is_err()
                    {
                        return;
                    }
                    while let Some(event) = events.recv().await {
                        if weak
                            .update(cx, |workspace, cx| workspace.on_event(event, cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                Err(error) => {
                    let _ = weak.update(cx, |workspace, cx| {
                        workspace.message = Some(error.to_string());
                        cx.notify();
                    });
                }
            }
        });
        Self {
            connections: None,
            profiles: Vec::new(),
            search,
            editor: None,
            tabs: Vec::new(),
            active: None,
            terminal,
            confirm: None,
            message: Some("Opening local connections…".into()),
            file_warning: None,
            file_warnings,
            hosts_visible: true,
            pending: HashMap::new(),
            serial: 0,
            _startup: startup,
            _file_warnings: warning_task,
            _subscriptions: subscriptions,
        }
    }

    fn run<T: 'static, F: Future<Output = Result<T, ManagerError>> + 'static>(
        &mut self,
        call: F,
        cx: &mut Context<Self>,
        success: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        self.serial = self.serial.wrapping_add(1);
        let serial = self.serial;
        let task = cx.spawn(async move |weak, cx| {
            let result = call.await;
            let _ = weak.update(cx, |workspace, cx| {
                if let Some(task) = workspace.pending.remove(&serial) {
                    task.detach();
                }
                match result {
                    Ok(value) => success(workspace, value, cx),
                    Err(error) => workspace.message = Some(error.to_string()),
                }
                cx.notify();
            });
        });
        self.pending.insert(serial, task);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(manager) = self.connections.clone() {
            self.run(
                async move { manager.list_profiles("").await },
                cx,
                |workspace, profiles, _| {
                    workspace.profiles = profiles;
                },
            );
        }
    }

    fn on_event(&mut self, event: ConnectionEvent, cx: &mut Context<Self>) {
        let Some(manager) = self.connections.clone() else {
            return;
        };
        let id = event.attempt.tab;
        if manager.current_attempt(id) != Some(event.attempt) {
            return;
        }
        if let ConnectionEventKind::Starting { label, endpoint } = event.kind {
            let state = Tab {
                attempt: event.attempt,
                label,
                endpoint,
                status: "Connecting…".into(),
                connected: false,
                finished: false,
                prompt: None,
                frame: None,
                viewport: None,
                files: None,
                showing_files: false,
            };
            if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.attempt.tab == id) {
                if let Some(field) = tab.prompt.as_ref().and_then(|(_, prompt)| prompt.field()) {
                    field.update(cx, Field::clear);
                }
                *tab = state;
            } else {
                self.tabs.push(state);
                self.select_tab(id, cx);
            }
            if self.active == Some(id) {
                self.terminal.update(cx, |terminal, cx| {
                    terminal.reset();
                    terminal.set_frame(None, cx);
                });
            }
            cx.notify();
            return;
        }
        let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.attempt == event.attempt)
        else {
            return;
        };
        match event.kind {
            ConnectionEventKind::Prompt(prompt) => {
                if tab.finished {
                    return;
                }
                let kind = match prompt.kind {
                    PromptKind::Host {
                        endpoint,
                        fingerprint,
                    } => FormPrompt::Host {
                        endpoint,
                        fingerprint,
                    },
                    PromptKind::Password => {
                        FormPrompt::Password(cx.new(|cx| Field::new("Password", "", true, cx)))
                    }
                    PromptKind::KeyPath => FormPrompt::PrivateKey(
                        cx.new(|cx| Field::new("Private key file path", "", false, cx)),
                    ),
                    PromptKind::Passphrase => FormPrompt::Passphrase(
                        cx.new(|cx| Field::new("Key passphrase", "", true, cx)),
                    ),
                };
                tab.prompt = Some((prompt.id, kind));
                tab.status = "Waiting for your response".into();
            }
            ConnectionEventKind::Connected { recent_error } => {
                if tab.finished {
                    return;
                }
                tab.connected = true;
                tab.prompt = None;
                tab.status = "Connected".into();
                tab.frame = manager.snapshot(id);
                if let Some(error) = recent_error {
                    self.message = Some(error.to_string());
                }
                if self.active == Some(id) {
                    self.sync_terminal(cx);
                }
                self.refresh(cx);
            }
            ConnectionEventKind::Redraw => {
                tab.frame = manager.snapshot(id);
                if self.active == Some(id) {
                    self.sync_terminal(cx);
                }
            }
            ConnectionEventKind::Ended(result) => {
                if let Some(files) = &tab.files {
                    files.update(cx, FilesView::disconnect);
                }
                if let Some(field) = tab
                    .prompt
                    .take()
                    .as_ref()
                    .and_then(|(_, prompt)| prompt.field())
                {
                    field.update(cx, Field::clear);
                }
                tab.finished = true;
                tab.connected = false;
                tab.status = match result {
                    Ok(()) => "Disconnected".into(),
                    Err(error) => error.to_string(),
                };
            }
            ConnectionEventKind::Starting { .. } => {}
        }
        cx.notify();
    }

    fn select_tab(&mut self, id: TabId, cx: &mut Context<Self>) {
        self.active = Some(id);
        self.editor = None;
        self.confirm = None;
        self.terminal.update(cx, |terminal, _| terminal.reset());
        self.sync_terminal(cx);
        cx.notify();
    }

    fn focus_active(&self, window: &mut Window, cx: &App) {
        let files = self.active.and_then(|id| {
            let tab = self
                .tabs
                .iter()
                .find(|tab| tab.attempt.tab == id && tab.showing_files)?;
            tab.files.as_ref()
        });
        if let Some(files) = files {
            files.focus_handle(cx).focus(window);
        } else {
            self.terminal.focus_handle(cx).focus(window);
        }
    }

    fn show_files(&mut self, id: TabId, show: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.attempt.tab == id) else {
            return;
        };
        if show && tab.files.is_none() {
            let Some(connections) = self.connections.clone().filter(|_| tab.connected) else {
                return;
            };
            tab.files = Some(cx.new(|cx| {
                FilesView::new(
                    tab.attempt,
                    connections,
                    tab.label.clone(),
                    self.file_warnings.clone(),
                    cx,
                )
            }));
        }
        tab.showing_files = show;
        self.focus_active(window, cx);
        cx.notify();
    }

    fn sync_terminal(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self
            .active
            .and_then(|id| self.tabs.iter().find(|tab| tab.attempt.tab == id))
        {
            self.terminal
                .update(cx, |terminal, cx| terminal.set_frame(tab.frame.clone(), cx));
        }
        if let Some(viewport) = self.terminal.read(cx).viewport() {
            self.resize(viewport);
        }
    }

    fn resize(&mut self, viewport: Viewport) {
        let Some(tab) = self
            .active
            .and_then(|id| self.tabs.iter_mut().find(|tab| tab.attempt.tab == id))
        else {
            return;
        };
        if tab.connected
            && tab.viewport != Some(viewport)
            && let Some(manager) = &self.connections
            && manager.input(tab.attempt.tab, viewport.into()).is_ok()
        {
            tab.viewport = Some(viewport);
        }
    }

    fn on_terminal(&mut self, event: &ViewEvent, cx: &mut Context<Self>) {
        match event {
            ViewEvent::Resize(viewport) => self.resize(*viewport),
            ViewEvent::Input(input) => {
                if let (Some(id), Some(manager)) = (self.active, &self.connections)
                    && self
                        .tabs
                        .iter()
                        .any(|tab| tab.attempt.tab == id && tab.connected)
                    && let Err(error) = manager.input(id, input.clone())
                {
                    self.message = Some(error.to_string());
                    cx.notify();
                }
            }
        }
    }

    fn editor(&mut self, mode: EditorMode, window: &mut Window, cx: &mut Context<Self>) {
        let profile = match mode {
            EditorMode::Edit(id) => self.profiles.iter().find(|profile| profile.id == id),
            _ => None,
        };
        let initial_label = profile.map_or("", |profile| profile.draft.label.as_str());
        let initial_address = profile
            .map(|profile| {
                format!(
                    "{}@{}",
                    profile.draft.target.username(),
                    profile.draft.target.endpoint()
                )
            })
            .unwrap_or_default();
        let (key, path) = match profile.map(|profile| &profile.draft.auth) {
            Some(AuthMethod::PrivateKey { path }) => {
                (true, path.as_ref().and_then(|p| p.to_str()).unwrap_or(""))
            }
            _ => (false, ""),
        };
        let label = cx.new(|cx| Field::new("Display name", initial_label, false, cx));
        let address = cx.new(|cx| {
            Field::new(
                "Address (user@host or user@host:port)",
                &initial_address,
                false,
                cx,
            )
        });
        let path = cx.new(|cx| Field::new("Private key file path (optional)", path, false, cx));
        match mode {
            EditorMode::Quick => address.focus_handle(cx).focus(window),
            _ => label.focus_handle(cx).focus(window),
        }
        self.editor = Some(Editor {
            mode,
            label,
            address,
            path,
            key,
        });
        self.confirm = None;
        self.message = None;
        cx.notify();
    }

    fn submit_editor(&mut self, cx: &mut Context<Self>) {
        let (Some(editor), Some(manager)) = (&self.editor, self.connections.clone()) else {
            return;
        };
        let address = editor.address.read(cx).text();
        let path = editor.path.read(cx).text();
        match editor.mode {
            EditorMode::Quick => match Target::parse(address) {
                Ok(target) => self.start(
                    ConnectionSpec {
                        label: target.endpoint().host().into(),
                        target,
                        auth: auth_method(editor.key, path),
                        profile_id: None,
                    },
                    cx,
                ),
                Err(error) => {
                    self.message = Some(error.to_string());
                    cx.notify();
                }
            },
            mode => match profile_draft(editor.label.read(cx).text(), address, editor.key, path) {
                Ok(draft) => {
                    let id = match mode {
                        EditorMode::Edit(id) => Some(id),
                        _ => None,
                    };
                    self.run(
                        async move { manager.save_profile(id, draft).await },
                        cx,
                        |workspace, _, cx| {
                            workspace.editor = None;
                            workspace.message = None;
                            workspace.refresh(cx);
                        },
                    );
                }
                Err(error) => {
                    self.message = Some(error);
                    cx.notify();
                }
            },
        }
    }

    fn start(&mut self, spec: ConnectionSpec, cx: &mut Context<Self>) {
        if let Some(manager) = self.connections.clone() {
            self.run(
                async move { manager.start(spec).await },
                cx,
                |workspace, _, _| {
                    workspace.message = None;
                },
            );
        }
    }

    fn connect_profile(&mut self, id: ProfileId, cx: &mut Context<Self>) {
        if let Some(profile) = self.profiles.iter().find(|profile| profile.id == id) {
            let draft = profile.draft.clone();
            self.start(
                ConnectionSpec {
                    label: draft.label,
                    target: draft.target,
                    auth: draft.auth,
                    profile_id: Some(id),
                },
                cx,
            );
        }
    }

    fn respond(
        &mut self,
        id: PromptId,
        approval: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active != Some(id.attempt.tab) {
            return;
        }
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.attempt == id.attempt) else {
            return;
        };
        let Some(manager) = self.connections.clone() else {
            return;
        };
        let Some(prompt) = take_matching(&mut tab.prompt, id) else {
            return;
        };
        let response = match &prompt {
            FormPrompt::Host { .. } => PromptResponse::Trust(approval.unwrap_or(false)),
            FormPrompt::PrivateKey(field) => {
                let text = field.read(cx).text().trim();
                if text.is_empty() {
                    tab.prompt = Some((id, prompt));
                    self.message = Some("Enter the path to a private key file.".into());
                    cx.notify();
                    return;
                }
                PromptResponse::KeyPath(PathBuf::from(text))
            }
            FormPrompt::Password(field) | FormPrompt::Passphrase(field) => {
                PromptResponse::Secret(field.update(cx, Field::take_secret))
            }
        };
        tab.status = "Connecting…".into();
        self.message = None;
        self.terminal.focus_handle(cx).focus(window);
        self.run(
            async move { manager.respond(id, response).await },
            cx,
            |_, _, _| {},
        );
        cx.notify();
    }

    fn remove(&mut self, id: TabId, cx: &mut Context<Self>) {
        let Some(manager) = self.connections.clone() else {
            return;
        };
        let Some(index) = self.tabs.iter().position(|tab| tab.attempt.tab == id) else {
            return;
        };
        let removed = self.tabs.remove(index);
        if let Some(files) = &removed.files {
            if files.read(cx).has_recovery() {
                self.message = edits_root()
                    .map(|path| format!("Local edit copies are kept in {}.", path.display()));
            }
            files.update(cx, FilesView::disconnect);
        }
        if let Some(field) = removed
            .prompt
            .as_ref()
            .and_then(|(_, prompt)| prompt.field())
        {
            field.update(cx, Field::clear);
        }
        if self.active == Some(id) {
            self.active = None;
            if let Some(next) = self.tabs.get(index.min(self.tabs.len().saturating_sub(1))) {
                self.select_tab(next.attempt.tab, cx);
            } else {
                self.terminal
                    .update(cx, |terminal, cx| terminal.set_frame(None, cx));
            }
        }
        self.run(async move { manager.close(id).await }, cx, |_, _, _| {});
        cx.notify();
    }

    fn reconnect(&mut self, id: TabId, cx: &mut Context<Self>) {
        if let Some(manager) = self.connections.clone() {
            self.run(
                async move { manager.reconnect(id).await },
                cx,
                |workspace, _, _| {
                    workspace.message = None;
                },
            );
        }
    }

    fn apply_confirmation(&mut self, cx: &mut Context<Self>) {
        let (Some(confirm), Some(manager)) = (self.confirm.take(), self.connections.clone()) else {
            return;
        };
        match confirm {
            Confirm::Delete(id, _) => {
                self.run(
                    async move { manager.delete_profile(id).await },
                    cx,
                    |workspace, _, cx| {
                        workspace.editor = None;
                        workspace.refresh(cx);
                    },
                );
            }
            Confirm::Forget(endpoint) => {
                self.run(
                    async move { manager.forget_host(endpoint).await },
                    cx,
                    |workspace, _, _| {
                        workspace.message = Some(
                            "Host key forgotten. Reconnect and verify the new fingerprint.".into(),
                        );
                    },
                );
            }
        }
    }

    pub(crate) fn button<View: 'static>(
        cx: &Context<View>,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        clicked: impl Fn(&mut View, &mut Window, &mut Context<View>) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .tab_stop(true)
            .cursor_pointer()
            .px_2()
            .py_1()
            .rounded_sm()
            .border_1()
            .border_color(rgb(0x525f71))
            .bg(rgb(0x272e38))
            .hover(|style| style.bg(rgb(0x354154)))
            .focus(|style| style.border_color(rgb(0x9dbefa)))
            .on_click(cx.listener(move |workspace, _: &ClickEvent, window, cx| {
                clicked(workspace, window, cx)
            }))
            .child(label.into())
            .into_any_element()
    }

    fn sidebar(&self, cx: &Context<Self>) -> AnyElement {
        let search = self.search.read(cx).text().trim().to_lowercase();
        let mut list = div()
            .id("saved-profiles")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2();
        let mut count = 0;
        for (index, profile) in self.profiles.iter().enumerate() {
            let draft = &profile.draft;
            if ![
                draft.label.as_str(),
                draft.target.username(),
                draft.target.endpoint().host(),
            ]
            .iter()
            .any(|text| text.to_lowercase().contains(&search))
            {
                continue;
            }
            count += 1;
            let id = profile.id;
            let label = draft.label.clone();
            list = list.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .bg(rgb(0x222831))
                    .rounded_sm()
                    .child(
                        div()
                            .truncate()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(label.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(rgb(0xb7c1d0))
                            .child(format!(
                                "{}@{}",
                                draft.target.username(),
                                draft.target.endpoint()
                            )),
                    )
                    .when(profile.last_connected.is_some(), |row| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(rgb(0xa9c8aa))
                                .child("Recently connected"),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_1()
                            .child(Self::button(
                                cx,
                                ("connect-profile", index),
                                "Connect",
                                move |view, _, cx| view.connect_profile(id, cx),
                            ))
                            .child(Self::button(
                                cx,
                                ("edit-profile", index),
                                "Edit",
                                move |view, window, cx| {
                                    view.editor(EditorMode::Edit(id), window, cx)
                                },
                            ))
                            .child(Self::button(
                                cx,
                                ("delete-profile", index),
                                "Delete",
                                move |view, _, cx| {
                                    view.confirm = Some(Confirm::Delete(id, label.clone()));
                                    view.editor = None;
                                    cx.notify();
                                },
                            )),
                    ),
            );
        }
        if count == 0 {
            list = list.child(if search.is_empty() {
                "No saved hosts. Add a host to connect again later."
            } else {
                "No matching hosts."
            });
        }
        div()
            .w(px(208.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .bg(rgb(0x1c2128))
            .border_r_1()
            .border_color(rgb(0x39434f))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child("Saved hosts"),
            )
            .child(self.search.clone())
            .when(self.connections.is_some(), |side| {
                side.child(Self::button(
                    cx,
                    "new-host",
                    "New host",
                    |view, window, cx| view.editor(EditorMode::New, window, cx),
                ))
            })
            .child(list)
            .into_any_element()
    }

    fn editor_panel(&self, editor: &Editor, cx: &Context<Self>) -> AnyElement {
        let (title, action) = match editor.mode {
            EditorMode::Quick => ("Quick Connect", "Connect"),
            EditorMode::New => ("New host", "Save host"),
            EditorMode::Edit(_) => ("Edit host", "Save changes"),
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .max_w(px(480.))
            .child(
                div()
                    .text_base()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(title),
            )
            .when(!matches!(editor.mode, EditorMode::Quick), |column| {
                column.child(editor.label.clone())
            })
            .child(editor.address.clone())
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .child(Self::button(
                        cx,
                        "password-method",
                        if editor.key {
                            "Password"
                        } else {
                            "Password (selected)"
                        },
                        |view, _, cx| {
                            if let Some(editor) = &mut view.editor {
                                editor.key = false;
                                cx.notify();
                            }
                        },
                    ))
                    .child(Self::button(
                        cx,
                        "key-method",
                        if editor.key {
                            "Private key (selected)"
                        } else {
                            "Private key"
                        },
                        |view, _, cx| {
                            if let Some(editor) = &mut view.editor {
                                editor.key = true;
                                cx.notify();
                            }
                        },
                    )),
            )
            .when(editor.key, |column| column.child(editor.path.clone()))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(Self::button(cx, "submit-editor", action, |view, _, cx| {
                        view.submit_editor(cx)
                    }))
                    .child(Self::button(
                        cx,
                        "cancel-editor",
                        "Cancel",
                        |view, _, cx| {
                            view.editor = None;
                            cx.notify();
                        },
                    )),
            )
            .into_any_element()
    }

    fn prompt_panel(&self, id: PromptId, prompt: &FormPrompt, cx: &Context<Self>) -> AnyElement {
        let mut column = div().flex().flex_col().gap_2().max_w(px(560.));
        match prompt {
            FormPrompt::Host {
                endpoint,
                fingerprint,
            } => {
                column = column
                    .child(div().text_base().font_weight(gpui::FontWeight::SEMIBOLD).child("Verify SSH host"))
                    .child(endpoint.to_string())
                    .child(div().font_family("Consolas").text_xs().child(fingerprint.clone()))
                    .child("Verify this fingerprint with your administrator or another trusted source before approving.")
                    .child(div().flex().flex_wrap().gap_2()
                        .child(Self::button(cx, "approve-key", "Approve fingerprint", move |view, window, cx| view.respond(id, Some(true), window, cx)))
                        .child(Self::button(cx, "deny-key", "Reject", move |view, window, cx| view.respond(id, Some(false), window, cx))));
            }
            _ => {
                column = column.children(prompt.field().cloned()).child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(Self::button(
                            cx,
                            "send-credential",
                            "Continue",
                            move |view, window, cx| view.respond(id, None, window, cx),
                        ))
                        .child(Self::button(
                            cx,
                            "cancel-credential",
                            "Cancel connection",
                            move |view, _, cx| view.remove(id.attempt.tab, cx),
                        )),
                );
            }
        }
        column.into_any_element()
    }

    fn confirm_panel(&self, confirm: &Confirm, cx: &Context<Self>) -> AnyElement {
        let (text, action) = match confirm {
            Confirm::Delete(_, label) => (
                format!(
                    "Delete “{label}” from your saved hosts? Open tabs and host trust are not affected."
                ),
                "Delete host",
            ),
            Confirm::Forget(endpoint) => (
                format!(
                    "Forget the trusted key for {endpoint}? Do this only after independently confirming a key change. Pending connections will stop."
                ),
                "Forget host key",
            ),
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .max_w(px(480.))
            .child(text)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(Self::button(
                        cx,
                        "confirm-host-action",
                        action,
                        |view, _, cx| view.apply_confirmation(cx),
                    ))
                    .child(Self::button(
                        cx,
                        "cancel-host-action",
                        "Cancel",
                        |view, _, cx| {
                            view.confirm = None;
                            cx.notify();
                        },
                    )),
            )
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = self.hosts_visible && window.viewport_size().width >= px(540.);
        let active = self
            .active
            .and_then(|id| self.tabs.iter().find(|tab| tab.attempt.tab == id));
        let forms = ["editor", "confirm", "prompt"];
        let panel = if let Some(editor) = &self.editor {
            Some((forms[0], self.editor_panel(editor, cx)))
        } else if let Some(confirm) = &self.confirm {
            Some((forms[1], self.confirm_panel(confirm, cx)))
        } else {
            active
                .and_then(|tab| tab.prompt.as_ref())
                .map(|(id, prompt)| (forms[2], self.prompt_panel(*id, prompt, cx)))
        };
        let mut strip = div()
            .id("ssh-tabs")
            .overflow_x_scroll()
            .flex()
            .gap_1()
            .p_1()
            .bg(rgb(0x1c2128))
            .border_b_1()
            .border_color(rgb(0x39434f));
        for (index, tab) in self.tabs.iter().enumerate() {
            let id = tab.attempt.tab;
            let label = if tab.prompt.is_some() {
                format!("{} · response needed", tab.label)
            } else {
                tab.label.clone()
            };
            strip = strip.child(
                div()
                    .flex()
                    .gap_1()
                    .flex_shrink_0()
                    .when(self.active == Some(id), |row| row.bg(rgb(0x3e536e)))
                    .child(Self::button(
                        cx,
                        ("select-tab", index),
                        label,
                        move |view, window, cx| {
                            view.select_tab(id, cx);
                            view.focus_active(window, cx);
                        },
                    ))
                    .child(Self::button(
                        cx,
                        ("close-tab", index),
                        "Close",
                        move |view, _, cx| view.remove(id, cx),
                    )),
            );
        }
        let toolbar = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .p_2()
            .bg(rgb(0x20252d))
            .when(self.connections.is_some(), |toolbar| {
                toolbar
                    .child(Self::button(
                        cx,
                        "quick-connect",
                        "Quick Connect",
                        |view, window, cx| view.editor(EditorMode::Quick, window, cx),
                    ))
                    .child(Self::button(
                        cx,
                        "toggle-saved-hosts",
                        "Saved hosts",
                        |view, window, cx| {
                            if window.viewport_size().width >= px(540.) {
                                view.hosts_visible = !view.hosts_visible;
                            } else {
                                view.hosts_visible = true;
                                view.active = None;
                                view.editor = None;
                                view.confirm = None;
                            }
                            cx.notify();
                        },
                    ))
            });
        let body = if let Some((id, panel)) = panel {
            div()
                .id(id)
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p_3()
                .child(panel)
                .into_any_element()
        } else if let Some(tab) = active {
            let id = tab.attempt.tab;
            let endpoint = tab.endpoint.clone();
            let contents = tab
                .files
                .as_ref()
                .filter(|_| tab.showing_files)
                .map_or_else(
                    || self.terminal.clone().into_any_element(),
                    |files| files.clone().into_any_element(),
                );
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .p_1()
                        .bg(rgb(0x20252d))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0xb7c1d0))
                                .child(tab.status.clone()),
                        )
                        .when(tab.connected || tab.files.is_some(), |row| {
                            row.child(Self::button(
                                cx,
                                "switch-terminal",
                                "Terminal",
                                move |view, window, cx| view.show_files(id, false, window, cx),
                            ))
                            .child(Self::button(
                                cx,
                                "switch-files",
                                "Files",
                                move |view, window, cx| view.show_files(id, true, window, cx),
                            ))
                        })
                        .when(tab.finished, |row| {
                            row.child(Self::button(
                                cx,
                                "reconnect",
                                "Reconnect",
                                move |view, _, cx| view.reconnect(id, cx),
                            ))
                            .child(Self::button(
                                cx,
                                "forget-key",
                                "Forget host key…",
                                move |view, _, cx| {
                                    view.confirm = Some(Confirm::Forget(endpoint.clone()));
                                    cx.notify();
                                },
                            ))
                        }),
                )
                .child(div().flex_1().min_h_0().child(contents))
                .into_any_element()
        } else if !sidebar && window.viewport_size().width < px(540.) && self.hosts_visible {
            div()
                .flex_1()
                .min_h_0()
                .child(self.sidebar(cx))
                .into_any_element()
        } else {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p_3()
                .text_color(rgb(0xb7c1d0))
                .child("No active session. Open Quick Connect or choose a saved host.")
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x181b20))
            .text_color(rgb(0xe9ecf1))
            .text_sm()
            .on_key_down(|event: &KeyDownEvent, window, cx: &mut App| {
                if event.keystroke.key == "tab" {
                    if event.keystroke.modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    cx.stop_propagation();
                }
            })
            .child(toolbar)
            .when_some(self.message.clone(), |root, message| {
                root.child(div().px_2().py_1().bg(rgb(0x303744)).child(message))
            })
            .when_some(self.file_warning.clone(), |root, warning| {
                root.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .bg(rgb(0x303744))
                        .child(div().flex_1().min_w_0().child(warning))
                        .child(Self::button(
                            cx,
                            "dismiss-file-warning",
                            "Dismiss",
                            |view, _, cx| {
                                view.file_warning = None;
                                cx.notify();
                            },
                        )),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .when(sidebar, |row| row.child(self.sidebar(cx)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .when(!self.tabs.is_empty(), |right| right.child(strip))
                            .child(body),
                    ),
            )
    }
}

fn take_matching<I: PartialEq, T>(slot: &mut Option<(I, T)>, id: I) -> Option<T> {
    if slot.as_ref().is_some_and(|(candidate, _)| *candidate == id) {
        slot.take().map(|(_, value)| value)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_submit_uses_its_own_id_after_tab_switch() {
        let mut visible = Some((2, "other tab"));
        assert_eq!(take_matching(&mut visible, 1), None);
        assert_eq!(visible, Some((2, "other tab")));
        assert_eq!(take_matching(&mut visible, 2), Some("other tab"));
        assert_eq!(take_matching(&mut visible, 2), None);
    }
}
