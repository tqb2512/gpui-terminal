use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use hosts::{Approval, KeyCheck, Profile, ProfileDraft, ProfileId, Repository, StoreError};
use ssh::{AuthMethod, Endpoint, Event, Input, PtySize, Target};
use terminal::{Snapshot, Terminal, TerminalInput};
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TabId(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptId {
    pub tab: TabId,
    serial: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromptId {
    pub attempt: AttemptId,
    serial: u64,
}

#[derive(Clone)]
pub struct ConnectionSpec {
    pub label: String,
    pub target: Target,
    pub auth: AuthMethod,
    pub profile_id: Option<ProfileId>,
}

pub struct Prompt {
    pub id: PromptId,
    pub kind: PromptKind,
}

pub enum PromptKind {
    Host {
        endpoint: Endpoint,
        fingerprint: String,
    },
    Password,
    KeyPath,
    Passphrase,
}

pub enum PromptResponse {
    Trust(bool),
    Secret(Zeroizing<String>),
    KeyPath(PathBuf),
}

pub struct ConnectionEvent {
    pub attempt: AttemptId,
    pub kind: ConnectionEventKind,
}

pub enum ConnectionEventKind {
    Prompt(Prompt),
    Connected { recent_error: Option<StoreError> },
    Redraw,
    Ended(Result<(), ManagerError>),
}

#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error(transparent)]
    Storage(#[from] StoreError),
    #[error(transparent)]
    Session(#[from] ssh::SessionError),
    #[error("Could not open the local connection data folder.")]
    Directory,
    #[error("The connection manager is unavailable.")]
    Unavailable,
    #[error("This tab is closed.")]
    MissingTab,
    #[error("This connection prompt is no longer active.")]
    StalePrompt,
    #[error("This response does not match the requested input.")]
    WrongResponse,
    #[error(
        "The SSH host key for {0} changed. Verify the change independently before forgetting the old key."
    )]
    ChangedKey(Endpoint),
    #[error("The trusted host key was forgotten; connect again to verify the host.")]
    Forgotten,
    #[error("The SSH terminal stopped processing input.")]
    TerminalInput,
}

#[derive(Clone)]
pub struct Connections {
    runtime: Handle,
    core: Arc<Mutex<Core>>,
    frames: Frames,
    events: mpsc::Sender<ConnectionEvent>,
}

impl Connections {
    pub async fn open(
        runtime: &Handle,
        directory: &Path,
    ) -> Result<(Self, mpsc::Receiver<ConnectionEvent>), ManagerError> {
        let directory = directory.to_owned();
        let repository = runtime
            .spawn_blocking(move || {
                std::fs::create_dir_all(&directory).map_err(|_| ManagerError::Directory)?;
                Ok::<_, ManagerError>(Repository::open(&directory.join("connections.sqlite3"))?)
            })
            .await
            .map_err(|_| ManagerError::Unavailable)??;
        let (events, receiver) = mpsc::channel(64);
        let core = Core::new(repository);
        Ok((
            Self {
                runtime: runtime.clone(),
                frames: core.frames.clone(),
                core: Arc::new(Mutex::new(core)),
                events,
            },
            receiver,
        ))
    }

    async fn work<T: Send + 'static>(
        &self,
        run: impl FnOnce(&mut Core) -> Result<T, ManagerError> + Send + 'static,
    ) -> Result<T, ManagerError> {
        work(&self.runtime, self.core.clone(), run).await
    }

    pub async fn start(&self, spec: ConnectionSpec) -> Result<TabId, ManagerError> {
        let this = self.clone();
        self.work(move |core| {
            let launch = core.insert_tab(spec)?;
            let tab = launch.id.tab;
            this.launch(launch);
            Ok(tab)
        })
        .await
    }

    pub async fn reconnect(&self, tab: TabId) -> Result<(), ManagerError> {
        let this = self.clone();
        self.work(move |core| {
            this.launch(core.reconnect(tab)?);
            Ok(())
        })
        .await
    }

    pub async fn close(&self, tab: TabId) -> Result<(), ManagerError> {
        self.work(move |core| core.close(tab)).await
    }

    pub async fn forget_host(&self, endpoint: Endpoint) -> Result<(), ManagerError> {
        let events = self.events.clone();
        self.work(move |core| core.forget_host(&endpoint))
            .await?
            .into_iter()
            .for_each(|attempt| {
                let events = events.clone();
                self.runtime.spawn(async move {
                    let kind = ConnectionEventKind::Ended(Err(ManagerError::Forgotten));
                    let _ = events.send(ConnectionEvent { attempt, kind }).await;
                });
            });
        Ok(())
    }

    pub async fn respond(
        &self,
        prompt: PromptId,
        response: PromptResponse,
    ) -> Result<(), ManagerError> {
        self.work(move |core| core.respond(prompt, response)).await
    }

    pub async fn list_profiles(&self, search: &str) -> Result<Vec<Profile>, ManagerError> {
        let search = search.to_owned();
        self.work(move |core| Ok(core.repository.list(&search)?))
            .await
    }

    pub async fn save_profile(
        &self,
        id: Option<ProfileId>,
        draft: ProfileDraft,
    ) -> Result<ProfileId, ManagerError> {
        self.work(move |core| match id {
            Some(id) => {
                core.repository.update(id, &draft)?;
                Ok(id)
            }
            None => Ok(core.repository.create(&draft)?),
        })
        .await
    }

    pub async fn delete_profile(&self, id: ProfileId) -> Result<(), ManagerError> {
        self.work(move |core| Ok(core.repository.delete(id)?)).await
    }

    fn frame(&self, tab: TabId) -> Option<Arc<TabFrame>> {
        self.frames.lock().ok()?.get(&tab).cloned()
    }

    pub fn input(&self, tab: TabId, input: TerminalInput) -> Result<(), InputError> {
        let frame = self.frame(tab).ok_or(InputError::Closed)?;
        if !frame.connected.load(Ordering::Acquire) {
            return Err(InputError::Closed);
        }
        frame.input(input)
    }

    pub fn snapshot(&self, tab: TabId) -> Option<Arc<Snapshot>> {
        self.frame(tab)?.read()
    }

    pub fn selected_text(&self, tab: TabId) -> Option<String> {
        self.frame(tab)?
            .latest
            .lock()
            .ok()?
            .as_ref()?
            .selected_text
            .clone()
    }

    fn launch(&self, mut launch: Launch) {
        let core = Arc::downgrade(&self.core);
        let events = self.events.clone();
        let runtime = self.runtime.clone();
        self.runtime.spawn(async move {
            let mut session = ssh::connect(
                &runtime, launch.spec.target, launch.spec.auth,
                PtySize { columns: 80, rows: 24, pixel_width: 0, pixel_height: 0 },
            );
            let writer = session.split_input();
            let attempt = launch.id;
            let mut emulator = TabEmulator::new(launch.frame, attempt, events.clone(), launch.cancel.clone());
            let mut pending = VecDeque::new();
            let mut clock = tokio::time::interval(Duration::from_millis(16));
            clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut dirty = false;
            loop {
                let event = tokio::select! {
                    () = launch.cancel.cancelled() => break,
                    event = session.recv() => event.unwrap_or(Event::Ended(Err(ssh::SessionError::Protocol))),
                    permit = writer.reserve(), if !pending.is_empty() => {
                        match (permit, pending.pop_front()) {
                            (Some(permit), Some(input)) if !launch.cancel.is_cancelled() => {
                                if permit.send(input).is_err() { Event::Ended(Err(ssh::SessionError::Protocol)) }
                                else { continue; }
                            }
                            _ => { pending.clear(); continue; }
                        }
                    }
                    input = launch.input.recv(), if pending.is_empty() => {
                        let Some(input) = input else { break; };
                        emulator.apply(input, &mut pending);
                        dirty = true;
                        continue;
                    }
                    _ = clock.tick(), if dirty => {
                        dirty = false;
                        emulator.publish();
                        continue;
                    }
                };
                if let Event::Output(bytes) = event {
                    match emulator.advance(&bytes) {
                        Ok(replies) if pending.len() < 256 => {
                            enqueue_bytes(&mut pending, &replies);
                            dirty = true;
                            continue;
                        }
                        Ok(_) | Err(_) => {
                            let kind = ConnectionEventKind::Ended(Err(ManagerError::TerminalInput));
                            session.cancel();
                            if let Some(state) = core.upgrade() {
                                let _ = work(&runtime, state, move |core| { core.end(attempt); Ok(()) }).await;
                            }
                            let _ = events.send(ConnectionEvent { attempt, kind }).await;
                            break;
                        }
                    }
                }
                let Some(state) = core.upgrade() else { break; };
                let result = work(&runtime, state, move |core| {
                    match core.on_event(attempt, event) {
                        Ok(kind) => Ok(kind),
                        Err(error) => {
                            core.end(attempt);
                            Ok(Some(ConnectionEventKind::Ended(Err(error))))
                        }
                    }
                }).await;
                let kind = match result {
                    Ok(Some(kind)) => kind,
                    Ok(None) => continue,
                    Err(error) => ConnectionEventKind::Ended(Err(error)),
                };
                let ended = matches!(kind, ConnectionEventKind::Ended(_));
                if ended { session.cancel(); }
                if ended || matches!(kind, ConnectionEventKind::Connected { .. }) {
                    emulator.publish();
                    dirty = false;
                }
                let sent = tokio::select! {
                    biased;
                    () = launch.cancel.cancelled() => break,
                    result = events.send(ConnectionEvent { attempt, kind }) => result,
                };
                if ended || sent.is_err() { break; }
            }
        });
    }
}

async fn work<T: Send + 'static>(
    runtime: &Handle,
    core: Arc<Mutex<Core>>,
    run: impl FnOnce(&mut Core) -> Result<T, ManagerError> + Send + 'static,
) -> Result<T, ManagerError> {
    runtime
        .spawn_blocking(move || {
            let mut core = core.lock().map_err(|_| ManagerError::Unavailable)?;
            run(&mut core)
        })
        .await
        .map_err(|_| ManagerError::Unavailable)?
}

pub(crate) struct Core {
    repository: Repository,
    serial: u64,
    tabs: HashMap<TabId, Tab>,
    frames: Frames,
}

struct Tab {
    spec: ConnectionSpec,
    attempt: AttemptId,
    cancel: CancellationToken,
    established: bool,
    ended: bool,
    prompt: Option<(PromptId, Pending)>,
    failed_trust: Option<Endpoint>,
    frame: Arc<TabFrame>,
}

pub(crate) struct Launch {
    pub(crate) id: AttemptId,
    pub(crate) spec: ConnectionSpec,
    pub(crate) cancel: CancellationToken,
    pub(crate) frame: Arc<TabFrame>,
    pub(crate) input: mpsc::Receiver<TerminalInput>,
}

enum Pending {
    Trust {
        key: Vec<u8>,
        revision: i64,
        reply: oneshot::Sender<bool>,
    },
    Secret(oneshot::Sender<Zeroizing<String>>),
    Path(oneshot::Sender<PathBuf>),
}

impl Core {
    pub(crate) fn new(repository: Repository) -> Self {
        Self {
            repository,
            serial: 0,
            tabs: HashMap::new(),
            frames: Frames::default(),
        }
    }

    fn serial(&mut self) -> Result<u64, ManagerError> {
        self.serial = self
            .serial
            .checked_add(1)
            .ok_or(ManagerError::Unavailable)?;
        Ok(self.serial)
    }

    pub(crate) fn insert_tab(&mut self, spec: ConnectionSpec) -> Result<Launch, ManagerError> {
        let serial = self.serial()?;
        let attempt = AttemptId {
            tab: TabId(serial),
            serial,
        };
        self.replace(attempt, spec)
    }

    pub(crate) fn reconnect(&mut self, tab: TabId) -> Result<Launch, ManagerError> {
        let spec = self
            .tabs
            .get(&tab)
            .ok_or(ManagerError::MissingTab)?
            .spec
            .clone();
        let serial = self.serial()?;
        self.replace(AttemptId { tab, serial }, spec)
    }

    fn replace(
        &mut self,
        attempt: AttemptId,
        spec: ConnectionSpec,
    ) -> Result<Launch, ManagerError> {
        let cancel = CancellationToken::new();
        let (sender, input) = mpsc::channel(16);
        let frame = Arc::new(TabFrame {
            latest: Mutex::new(None),
            redraw_pending: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            sender,
        });
        self.frames
            .lock()
            .map_err(|_| ManagerError::Unavailable)?
            .insert(attempt.tab, frame.clone());
        let tab = Tab {
            spec: spec.clone(),
            attempt,
            cancel: cancel.clone(),
            established: false,
            ended: false,
            prompt: None,
            failed_trust: None,
            frame: frame.clone(),
        };
        self.tabs.insert(attempt.tab, tab);
        Ok(Launch {
            id: attempt,
            spec,
            cancel,
            frame,
            input,
        })
    }

    pub(crate) fn close(&mut self, tab: TabId) -> Result<(), ManagerError> {
        self.tabs.remove(&tab).ok_or(ManagerError::MissingTab)?;
        self.frames
            .lock()
            .map_err(|_| ManagerError::Unavailable)?
            .remove(&tab);
        Ok(())
    }

    pub(crate) fn forget_host(
        &mut self,
        endpoint: &Endpoint,
    ) -> Result<Vec<AttemptId>, ManagerError> {
        // This runs under the same serialized lock as replies and connection transitions.
        self.repository.forget_key(endpoint)?;
        Ok(self
            .tabs
            .values_mut()
            .filter_map(|tab| {
                if tab.established || tab.ended || tab.spec.target.endpoint() != endpoint {
                    return None;
                }
                tab.cancel.cancel();
                tab.ended = true;
                tab.prompt = None;
                Some(tab.attempt)
            })
            .collect())
    }

    pub(crate) fn respond(
        &mut self,
        prompt: PromptId,
        response: PromptResponse,
    ) -> Result<(), ManagerError> {
        let tab = self
            .tabs
            .get_mut(&prompt.attempt.tab)
            .ok_or(ManagerError::StalePrompt)?;
        let (id, pending) = tab.prompt.as_ref().ok_or(ManagerError::StalePrompt)?;
        if *id != prompt || tab.ended {
            return Err(ManagerError::StalePrompt);
        }
        if !matches!(
            (pending, &response),
            (Pending::Trust { .. }, PromptResponse::Trust(_))
                | (Pending::Secret(_), PromptResponse::Secret(_))
                | (Pending::Path(_), PromptResponse::KeyPath(_))
        ) {
            return Err(ManagerError::WrongResponse);
        }
        let (_, pending) = tab.prompt.take().ok_or(ManagerError::StalePrompt)?;
        match (pending, response) {
            (
                Pending::Trust {
                    key,
                    revision,
                    reply,
                },
                PromptResponse::Trust(allow),
            ) => {
                if reply.is_closed() {
                    return Err(ManagerError::StalePrompt);
                }
                if allow {
                    let endpoint = tab.spec.target.endpoint();
                    match self.repository.approve_key(endpoint, &key, revision)? {
                        Approval::Approved => {}
                        Approval::Changed => {
                            tab.failed_trust = Some(endpoint.clone());
                            let _ = reply.send(false);
                            return Err(ManagerError::ChangedKey(endpoint.clone()));
                        }
                        Approval::Stale => {
                            let _ = reply.send(false);
                            return Err(ManagerError::StalePrompt);
                        }
                    }
                }
                reply.send(allow).map_err(|_| ManagerError::StalePrompt)
            }
            (Pending::Secret(reply), PromptResponse::Secret(secret)) => {
                reply.send(secret).map_err(|_| ManagerError::StalePrompt)
            }
            (Pending::Path(reply), PromptResponse::KeyPath(path)) => {
                reply
                    .send(path.clone())
                    .map_err(|_| ManagerError::StalePrompt)?;
                tab.spec.auth = AuthMethod::PrivateKey { path: Some(path) };
                Ok(())
            }
            _ => Err(ManagerError::WrongResponse),
        }
    }

    fn end(&mut self, attempt: AttemptId) {
        if let Some(tab) = self
            .tabs
            .get_mut(&attempt.tab)
            .filter(|tab| tab.attempt == attempt)
        {
            tab.ended = true;
            tab.established = false;
            tab.frame.connected.store(false, Ordering::Release);
            tab.prompt = None;
        }
    }

    pub(crate) fn on_event(
        &mut self,
        attempt: AttemptId,
        event: Event,
    ) -> Result<Option<ConnectionEventKind>, ManagerError> {
        let serial = self.serial;
        let Some(tab) = self
            .tabs
            .get_mut(&attempt.tab)
            .filter(|tab| tab.attempt == attempt && !tab.ended)
        else {
            return Ok(None);
        };
        let endpoint = tab.spec.target.endpoint();
        let (kind, pending) = match event {
            Event::VerifyHost {
                key,
                fingerprint,
                reply,
            } => match self.repository.check_key(endpoint, &key)? {
                KeyCheck::Match => {
                    reply.send(true).map_err(|_| ManagerError::StalePrompt)?;
                    return Ok(None);
                }
                KeyCheck::Changed => {
                    let _ = reply.send(false);
                    return Err(ManagerError::ChangedKey(endpoint.clone()));
                }
                KeyCheck::Unknown { revision } => (
                    PromptKind::Host {
                        endpoint: endpoint.clone(),
                        fingerprint,
                    },
                    Pending::Trust {
                        key,
                        revision,
                        reply,
                    },
                ),
            },
            Event::Password { reply } => (PromptKind::Password, Pending::Secret(reply)),
            Event::Passphrase { reply } => (PromptKind::Passphrase, Pending::Secret(reply)),
            Event::KeyPath { reply } => (PromptKind::KeyPath, Pending::Path(reply)),
            Event::Connected => {
                tab.established = true;
                tab.frame.connected.store(true, Ordering::Release);
                let recent_error = tab
                    .spec
                    .profile_id
                    .and_then(|id| self.repository.record_connected(id).err());
                return Ok(Some(ConnectionEventKind::Connected { recent_error }));
            }
            Event::Output(_) => return Ok(None),
            Event::Ended(result) => {
                let result = match tab.failed_trust.take() {
                    Some(endpoint) => Err(ManagerError::ChangedKey(endpoint)),
                    None => result.map_err(ManagerError::from),
                };
                self.end(attempt);
                return Ok(Some(ConnectionEventKind::Ended(result)));
            }
        };
        self.serial = serial.checked_add(1).ok_or(ManagerError::Unavailable)?;
        let id = PromptId {
            attempt,
            serial: self.serial,
        };
        tab.prompt = Some((id, pending));
        Ok(Some(ConnectionEventKind::Prompt(Prompt { id, kind })))
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

type Frames = Arc<Mutex<HashMap<TabId, Arc<TabFrame>>>>;

pub(crate) struct TabFrame {
    latest: Mutex<Option<Arc<Snapshot>>>,
    redraw_pending: AtomicBool,
    connected: AtomicBool,
    sender: mpsc::Sender<TerminalInput>,
}

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("The SSH tab is not connected.")]
    Closed,
    #[error("The terminal is busy. Try the input again.")]
    QueueFull,
    #[error("The paste exceeds 1 MiB. Paste a smaller section.")]
    PasteTooLarge,
}

impl TabFrame {
    pub(crate) fn read(&self) -> Option<Arc<Snapshot>> {
        let latest = self.latest.lock().ok()?;
        self.redraw_pending.store(false, Ordering::Release);
        latest.clone()
    }

    pub(crate) fn input(&self, input: TerminalInput) -> Result<(), InputError> {
        if matches!(&input, TerminalInput::Paste(text) if text.len() > 1024 * 1024)
            || matches!(&input, TerminalInput::Key(key) if key.text.as_ref().is_some_and(|text| text.len() > 1024 * 1024))
        {
            return Err(InputError::PasteTooLarge);
        }
        self.sender.try_send(input).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => InputError::QueueFull,
            mpsc::error::TrySendError::Closed(_) => InputError::Closed,
        })
    }
}

pub(crate) struct TabEmulator {
    terminal: Terminal,
    pub(crate) frame: Arc<TabFrame>,
    attempt: AttemptId,
    events: mpsc::Sender<ConnectionEvent>,
    cancel: CancellationToken,
}

impl TabEmulator {
    pub(crate) fn new(
        frame: Arc<TabFrame>,
        attempt: AttemptId,
        events: mpsc::Sender<ConnectionEvent>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            terminal: Terminal::new(80, 24),
            frame,
            attempt,
            events,
            cancel,
        }
    }

    pub(crate) fn advance(&mut self, bytes: &[u8]) -> Result<Vec<u8>, ManagerError> {
        self.terminal.advance(bytes);
        Ok(self.terminal.take_replies())
    }

    fn apply(&mut self, input: TerminalInput, pending: &mut VecDeque<Input>) {
        match input {
            TerminalInput::Key(key) => {
                if let Some(bytes) = self.terminal.encode_key(&key) {
                    enqueue_bytes(pending, &bytes);
                }
            }
            TerminalInput::Paste(text) => {
                enqueue_bytes(pending, &self.terminal.encode_paste(&text))
            }
            TerminalInput::Resize {
                columns,
                rows,
                pixel_width,
                pixel_height,
            } => {
                let columns = columns.clamp(2, 512);
                let rows = rows.clamp(1, 256);
                self.terminal.resize(columns, rows);
                pending.push_back(Input::Resize(PtySize {
                    columns,
                    rows,
                    pixel_width,
                    pixel_height,
                }));
            }
            TerminalInput::Scroll(lines) => self.terminal.scroll(lines),
            TerminalInput::Select(range) => self.terminal.select(range),
        }
    }

    pub(crate) fn publish(&self) {
        let frame = Arc::new(self.terminal.snapshot());
        let Ok(mut latest) = self.frame.latest.lock() else {
            return;
        };
        *latest = Some(frame);
        if self.frame.redraw_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        drop(latest);
        let event = ConnectionEvent {
            attempt: self.attempt,
            kind: ConnectionEventKind::Redraw,
        };
        if let Err(mpsc::error::TrySendError::Full(event)) = self.events.try_send(event) {
            let cancel = self.cancel.clone();
            let events = self.events.clone();
            tokio::spawn(async move {
                tokio::select! {
                    () = cancel.cancelled() => {}
                    _ = events.send(event) => {}
                }
            });
        }
    }
}

fn enqueue_bytes(pending: &mut VecDeque<Input>, bytes: &[u8]) {
    pending.extend(
        bytes
            .chunks(16 * 1024)
            .map(|chunk| Input::Bytes(chunk.to_vec())),
    );
}
