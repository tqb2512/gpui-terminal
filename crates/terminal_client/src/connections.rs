use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use hosts::{Approval, KeyCheck, Profile, ProfileDraft, ProfileId, Repository, StoreError};
use ssh::{AuthMethod, Endpoint, Event, PtySize, Target};
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
    Output(Vec<u8>),
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
}

#[derive(Clone)]
pub struct Connections {
    runtime: Handle,
    core: Arc<Mutex<Core>>,
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
        Ok((
            Self {
                runtime: runtime.clone(),
                core: Arc::new(Mutex::new(Core::new(repository))),
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

    fn launch(&self, launch: Launch) {
        let core = Arc::downgrade(&self.core);
        let events = self.events.clone();
        let runtime = self.runtime.clone();
        self.runtime.spawn(async move {
            let mut session = ssh::connect(
                &runtime, launch.spec.target, launch.spec.auth,
                PtySize { columns: 80, rows: 24, pixel_width: 0, pixel_height: 0 },
            );
            let attempt = launch.id;
            loop {
                let event = tokio::select! {
                    biased;
                    () = launch.cancel.cancelled() => break,
                    event = session.recv() => event.unwrap_or(Event::Ended(Err(ssh::SessionError::Protocol))),
                };
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
}

struct Tab {
    spec: ConnectionSpec,
    attempt: AttemptId,
    cancel: CancellationToken,
    established: bool,
    ended: bool,
    prompt: Option<(PromptId, Pending)>,
    failed_trust: Option<Endpoint>,
}

pub(crate) struct Launch {
    pub(crate) id: AttemptId,
    pub(crate) spec: ConnectionSpec,
    pub(crate) cancel: CancellationToken,
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
        Ok(self.replace(attempt, spec))
    }

    pub(crate) fn reconnect(&mut self, tab: TabId) -> Result<Launch, ManagerError> {
        let spec = self
            .tabs
            .get(&tab)
            .ok_or(ManagerError::MissingTab)?
            .spec
            .clone();
        let serial = self.serial()?;
        Ok(self.replace(AttemptId { tab, serial }, spec))
    }

    fn replace(&mut self, attempt: AttemptId, spec: ConnectionSpec) -> Launch {
        let cancel = CancellationToken::new();
        let tab = Tab {
            spec: spec.clone(),
            attempt,
            cancel: cancel.clone(),
            established: false,
            ended: false,
            prompt: None,
            failed_trust: None,
        };
        self.tabs.insert(attempt.tab, tab);
        Launch {
            id: attempt,
            spec,
            cancel,
        }
    }

    pub(crate) fn close(&mut self, tab: TabId) -> Result<(), ManagerError> {
        self.tabs.remove(&tab).ok_or(ManagerError::MissingTab)?;
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
                let recent_error = tab
                    .spec
                    .profile_id
                    .and_then(|id| self.repository.record_connected(id).err());
                return Ok(Some(ConnectionEventKind::Connected { recent_error }));
            }
            Event::Output(output) => return Ok(Some(ConnectionEventKind::Output(output))),
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
