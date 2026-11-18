use std::{
    future::Future,
    path::{Path, PathBuf},
};

use gpui::{
    Context, FocusHandle, Focusable, IntoElement, PathPromptOptions, Render, Task, Window, div,
    prelude::*,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use ssh::{RemoteEntry, RemoteKind, RemotePath};
use terminal_client::{ActiveEdit, AttemptFiles, AttemptId, Connections, EditStatus};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use windows::Win32::Foundation::HWND;

use crate::{
    windows_open_with::{OpenWithOutcome, choose_application, mark_remote_attachment},
    workspace::Workspace,
};

#[derive(Clone, Default)]
pub(crate) struct Request(CancellationToken);

impl Request {
    pub(crate) fn invalidate(&self) {
        self.0.cancel();
    }

    pub(crate) fn is_current(&self) -> bool {
        !self.0.is_cancelled()
    }
}

pub(crate) fn accept_opened<T, U>(
    outcome: OpenWithOutcome,
    copy: T,
    start: impl FnOnce(T) -> U,
) -> Option<U> {
    match outcome {
        OpenWithOutcome::Opened => Some(start(copy)),
        OpenWithOutcome::Cancelled => None,
    }
}

pub(crate) struct FilesView {
    attempt: AttemptId,
    connections: Connections,
    request: Request,
    focus: FocusHandle,
    files: Option<AttemptFiles>,
    directory: Option<RemotePath>,
    entries: Vec<RemoteEntry>,
    selected: Option<(RemotePath, RemoteKind)>,
    pending: Option<Task<()>>,
    message: Option<String>,
    edits: Vec<EditRow>,
    has_recovery: bool,
    opening_edit: bool,
}

struct EditRow {
    name: String,
    path: PathBuf,
    stop: CancellationToken,
    status: watch::Receiver<EditStatus>,
    _listen: Task<()>,
}

impl Drop for EditRow {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Drop for FilesView {
    fn drop(&mut self) {
        self.request.invalidate();
    }
}

impl Focusable for FilesView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl FilesView {
    pub(crate) fn new(
        attempt: AttemptId,
        connections: Connections,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            attempt,
            connections,
            request: Request::default(),
            focus: cx.focus_handle(),
            files: None,
            directory: None,
            entries: Vec::new(),
            selected: None,
            pending: None,
            message: None,
            edits: Vec::new(),
            has_recovery: false,
            opening_edit: false,
        };
        view.load(None, cx);
        view
    }

    pub(crate) fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.request.invalidate();
        self.message = Some("SSH disconnected. Automatic saves have stopped.".into());
        cx.notify();
    }

    pub(crate) fn has_recovery(&self) -> bool {
        self.has_recovery || self.opening_edit
    }

    fn run<T: 'static>(
        &mut self,
        operation: impl Future<Output = Result<T, String>> + 'static,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        if self.pending.is_some() || !self.request.is_current() {
            return;
        }
        self.message = None;
        let attempt = self.attempt;
        let connections = self.connections.clone();
        let request = self.request.clone();
        self.pending = Some(cx.spawn(async move |weak, cx| {
            let result = operation.await;
            let _ = weak.update(cx, |view, cx| {
                if let Some(task) = view.pending.take() {
                    task.detach();
                }
                view.opening_edit = false;
                if !request.is_current()
                    || connections.current_attempt(attempt.tab) != Some(attempt)
                {
                    cx.notify();
                    return;
                }
                match result {
                    Ok(value) => done(view, value, cx),
                    Err(error) => view.message = Some(error),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn load(&mut self, path: Option<RemotePath>, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        let connections = self.connections.clone();
        let runtime = connections.runtime().clone();
        let attempt = self.attempt;
        self.run(
            async move {
                runtime
                    .spawn(async move {
                        let files = connections.files(attempt).await.map_err(text)?;
                        let path = match path {
                            Some(path) => path,
                            None => files.home().await.map_err(text)?,
                        };
                        let mut entries = files.list(&path).await.map_err(text)?;
                        entries.sort_by(|left, right| {
                            let directory =
                                |entry: &RemoteEntry| entry.kind != RemoteKind::Directory;
                            (directory(left), &left.name).cmp(&(directory(right), &right.name))
                        });
                        Ok((files, path, entries))
                    })
                    .await
                    .map_err(text)?
            },
            cx,
            |view, (files, path, entries), _| {
                view.files = Some(files);
                view.directory = Some(path);
                view.entries = entries;
                view.selected = None;
            },
        );
    }

    fn upload(&mut self, cx: &mut Context<Self>) {
        let (Some(files), Some(directory)) = (self.files.clone(), self.directory.clone()) else {
            return;
        };
        if self.pending.is_some() || !self.request.is_current() {
            return;
        }
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Upload a file".into()),
        });
        let runtime = self.connections.runtime().clone();
        let request = self.request.clone();
        self.run(
            async move {
                let Some(source) = picker
                    .await
                    .map_err(text)?
                    .map_err(text)?
                    .and_then(|mut paths| paths.pop())
                else {
                    return Ok(false);
                };
                if !request.is_current() {
                    return Ok(false);
                }
                let name = source
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or("This filename is not supported.")?;
                let remote = directory.child(name).map_err(text)?;
                runtime
                    .spawn(async move { files.upload(&source, &remote).await })
                    .await
                    .map_err(text)?
                    .map_err(text)?;
                Ok(true)
            },
            cx,
            |view, uploaded, cx| {
                if uploaded {
                    view.load(view.directory.clone(), cx);
                    view.message = Some("Uploaded.".into());
                }
            },
        );
    }

    fn download(&mut self, cx: &mut Context<Self>) {
        let (Some(files), Some((remote, RemoteKind::File))) =
            (self.files.clone(), self.selected.clone())
        else {
            return;
        };
        if self.pending.is_some() || !self.request.is_current() {
            return;
        }
        let suggestion = remote
            .name()
            .filter(|name| terminal_client::valid_windows_filename(name))
            .unwrap_or("download");
        let picker = cx.prompt_for_new_path(Path::new(""), Some(suggestion));
        let request = self.request.clone();
        let runtime = self.connections.runtime().clone();
        self.run(
            async move {
                let Some(destination) = picker.await.map_err(text)?.map_err(text)? else {
                    return Ok(false);
                };
                if !request.is_current() {
                    return Ok(false);
                }
                runtime
                    .spawn(async move { files.download(&remote, &destination).await })
                    .await
                    .map_err(text)?
                    .map_err(text)?;
                Ok(true)
            },
            cx,
            |view, downloaded, _| {
                if downloaded {
                    view.message = Some("Downloaded.".into());
                }
            },
        );
    }

    fn open_with(&mut self, window: &Window, cx: &mut Context<Self>) {
        let (Some(files), Some((remote, RemoteKind::File))) =
            (self.files.clone(), self.selected.clone())
        else {
            return;
        };
        if self.pending.is_some() || !self.request.is_current() {
            return;
        }
        let (Ok(parent), Some(root)) = (HasWindowHandle::window_handle(window), edits_root())
        else {
            self.message = Some("Windows could not prepare an editable copy.".into());
            cx.notify();
            return;
        };
        let RawWindowHandle::Win32(parent) = parent.as_raw() else {
            return;
        };
        let hwnd = HWND(parent.hwnd.get() as *mut core::ffi::c_void);
        let request = self.request.clone();
        let manager = self.connections.clone();
        let attempt = self.attempt;
        let runtime = manager.runtime().clone();
        let name = remote.name().unwrap_or_default().to_owned();
        self.opening_edit = true;
        self.run(
            async move {
                let prepared = runtime
                    .spawn(async move {
                        let prepared = files.prepare_edit(&remote, &root).await.map_err(text)?;
                        let path = prepared.local_path().to_owned();
                        tokio::task::spawn_blocking(move || mark_remote_attachment(&path))
                            .await
                            .map_err(text)?
                            .map_err(text)?;
                        Ok::<_, String>(prepared)
                    })
                    .await
                    .map_err(text)??;
                if !request.is_current()
                    || manager.current_attempt(attempt.tab) != Some(attempt)
                    || !prepared.is_active()
                {
                    return Ok(None);
                }
                let outcome = choose_application(prepared.local_path(), hwnd).map_err(text)?;
                Ok(accept_opened(outcome, prepared, |copy| {
                    (name, copy.start())
                }))
            },
            cx,
            |view, opened, cx| {
                if let Some((name, edit)) = opened {
                    view.watch_edit(name, edit, cx);
                }
            },
        );
    }

    fn watch_edit(&mut self, name: String, edit: ActiveEdit, cx: &mut Context<Self>) {
        self.has_recovery = true;
        let stop = CancellationToken::new();
        let path = edit.local_path().to_owned();
        let (publish_status, status) = watch::channel(EditStatus::Watching);
        let runtime = self.connections.runtime().clone();
        runtime.spawn({
            let runtime = runtime.clone();
            let stop = stop.clone();
            async move { edit.run(&runtime, stop, publish_status).await }
        });
        let mut changes = status.clone();
        let listen = cx.spawn(async move |weak, cx| {
            while changes.changed().await.is_ok() {
                if weak.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        self.edits.push(EditRow {
            name,
            path,
            stop,
            status,
            _listen: listen,
        });
    }
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

pub(crate) fn edits_root() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("GPUI Terminal Client")
            .join("edits"),
    )
}

impl Render for FilesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = self.pending.is_none() && self.request.is_current();
        let directory = self.directory.clone();
        let parent = directory.as_ref().and_then(RemotePath::parent);
        let mut tools = div().flex().flex_wrap().gap_1();
        if ready {
            if let Some(parent) = parent {
                tools = tools.child(Workspace::button(cx, "up", "Up", move |view, _, cx| {
                    view.load(Some(parent.clone()), cx);
                }));
            }
            tools = tools.child(Workspace::button(
                cx,
                "refresh",
                "Refresh",
                |view, _, cx| {
                    view.load(view.directory.clone(), cx);
                },
            ));
            if directory.is_some() {
                tools = tools.child(Workspace::button(
                    cx,
                    "upload",
                    "Upload…",
                    |view, _, cx| view.upload(cx),
                ));
            }
        }
        let mut list = div()
            .id("remote-files")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1();
        if let Some(directory) = &directory {
            for (index, entry) in self.entries.iter().enumerate() {
                let Ok(path) = directory.child(&entry.name) else {
                    continue;
                };
                let active = self
                    .selected
                    .as_ref()
                    .is_some_and(|(selected, _)| selected == &path);
                let kind = entry.kind;
                let suffix = match kind {
                    RemoteKind::Directory => "Folder".into(),
                    RemoteKind::File => entry
                        .size
                        .map_or("File".into(), |size| format!("{size} bytes")),
                    RemoteKind::Symlink => "Link".into(),
                    RemoteKind::Other => "Special".into(),
                };
                let label = format!(
                    "{}{} — {}",
                    if active { "Selected: " } else { "" },
                    entry.name,
                    suffix
                );
                list = list.child(Workspace::button(
                    cx,
                    ("remote-entry", index),
                    label,
                    move |view, _, cx| {
                        if view.pending.is_some() || !view.request.is_current() {
                            return;
                        }
                        if kind == RemoteKind::Directory {
                            view.load(Some(path.clone()), cx);
                        } else {
                            view.selected = Some((path.clone(), kind));
                            cx.notify();
                        }
                    },
                ));
            }
            if self.entries.is_empty() && ready {
                list = list.child("This folder is empty.");
            }
        }
        if ready && matches!(self.selected, Some((_, RemoteKind::File))) {
            tools = tools.child(Workspace::button(
                cx,
                "download",
                "Download…",
                |view, _, cx| view.download(cx),
            ));
            if self
                .files
                .as_ref()
                .is_some_and(AttemptFiles::supports_atomic_replace)
            {
                tools = tools.child(Workspace::button(
                    cx,
                    "open-with",
                    "Open with…",
                    |view, window, cx| view.open_with(window, cx),
                ));
            } else {
                tools = tools.child(
                    div()
                        .px_1()
                        .child("Open with requires server support for atomic replacement."),
                );
            }
        }
        let mut edits = div()
            .id("local-edits")
            .max_h(gpui::px(110.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1();
        for (index, row) in self.edits.iter().enumerate() {
            let status = match *row.status.borrow() {
                EditStatus::Watching => "Watching for local saves".into(),
                EditStatus::Saving => "Saving to remote…".into(),
                EditStatus::Saved => "Saved to remote".into(),
                EditStatus::Paused(reason) => reason.to_string(),
                EditStatus::Finished => "Editing finished; local copy kept".into(),
            };
            let path = row.path.clone();
            edits = edits.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(format!("{} — {status}", row.name))
                    .child(Workspace::button(
                        cx,
                        ("show-copy", index),
                        "Show local copy",
                        move |_, _, cx| {
                            cx.reveal_path(&path);
                        },
                    ))
                    .child(Workspace::button(
                        cx,
                        ("finish-edit", index),
                        "Finish editing",
                        move |view, _, cx| {
                            if index < view.edits.len() {
                                view.edits.remove(index);
                                view.message = edits_root()
                                    .map(|path| format!("Local copy kept in {}.", path.display()));
                                cx.notify();
                            }
                        },
                    )),
            );
        }
        div()
            .track_focus(&self.focus)
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .child(
                directory
                    .as_ref()
                    .map_or("Remote files", RemotePath::as_str)
                    .to_owned(),
            )
            .child(tools)
            .when(
                self.pending.is_some() && self.request.is_current(),
                |body| body.child("Working…"),
            )
            .when_some(self.message.clone(), |body, message| body.child(message))
            .child(list)
            .child(edits)
    }
}
