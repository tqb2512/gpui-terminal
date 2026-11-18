use std::{
    collections::HashMap,
    io,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    },
    time::Duration,
};

use russh::{
    Channel, ChannelId, Pty,
    keys::{Algorithm, PrivateKey},
    server::{self, Auth, ChannelOpenHandle, Msg},
};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Packet, Status, StatusCode, Version,
};
use ssh::{AuthMethod, Target};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, mpsc},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::{
    AttemptFiles, AttemptId, ConnectionEvent, ConnectionEventKind, ConnectionSpec, Connections,
    PromptKind, PromptResponse,
};

const FILE: &str = "/home/test/edit.txt";
type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

pub(super) struct EditServer {
    target: Target,
    state: Arc<State>,
    listener: JoinHandle<()>,
}

struct State {
    files: Mutex<HashMap<String, Vec<u8>>>,
    stall: AtomicBool,
    publishes: AtomicUsize,
    received: Semaphore,
    shutdown: CancellationToken,
}

impl EditServer {
    pub(super) async fn start() -> TestResult<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let target = Target::parse(&format!("test@{}", listener.local_addr()?))?;
        let state = Arc::new(State {
            files: Mutex::new(HashMap::from([(FILE.into(), b"a".to_vec())])),
            stall: AtomicBool::new(false),
            publishes: AtomicUsize::new(0),
            received: Semaphore::new(0),
            shutdown: CancellationToken::new(),
        });
        let config = Arc::new(server::Config {
            keys: vec![PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?],
            auth_rejection_time: Duration::ZERO,
            ..Default::default()
        });
        let listener = tokio::spawn({
            let state = state.clone();
            async move {
                while let Ok((socket, _)) = listener.accept().await {
                    let handler = Ssh {
                        state: state.clone(),
                        channels: HashMap::new(),
                    };
                    let config = config.clone();
                    tokio::spawn(async move {
                        if let Ok(session) = server::run_stream(config, socket, handler).await {
                            let _ = session.await;
                        }
                    });
                }
            }
        });
        Ok(Self {
            target,
            state,
            listener,
        })
    }

    pub(super) async fn connect(&self, root: &Path) -> TestResult<Connected> {
        let (connections, mut events) =
            Connections::open(&tokio::runtime::Handle::current(), root).await?;
        let tab = connections
            .start(ConnectionSpec {
                label: "loopback".into(),
                target: self.target.clone(),
                auth: AuthMethod::Password,
                profile_id: None,
            })
            .await?;
        let attempt = connect_attempt(&connections, &mut events, tab).await?;
        Ok(Connected {
            connections,
            events,
            original_attempt: attempt.attempt(),
            attempt,
        })
    }

    pub(super) fn change_remote(&self, bytes: &[u8]) -> TestResult<()> {
        self.state
            .files
            .lock()
            .map_err(|_| io::Error::other("fixture lock"))?
            .insert(FILE.into(), bytes.to_vec());
        Ok(())
    }

    pub(super) fn remote(&self) -> TestResult<Vec<u8>> {
        Ok(self
            .state
            .files
            .lock()
            .map_err(|_| io::Error::other("fixture lock"))?
            .get(FILE)
            .cloned()
            .ok_or(io::Error::other("remote file not found"))?)
    }

    pub(super) fn stall_publish(&self) {
        self.state.stall.store(true, SeqCst);
    }

    pub(super) fn publish_count(&self) -> usize {
        self.state.publishes.load(SeqCst)
    }

    pub(super) async fn publish_received(&self) -> TestResult<()> {
        self.state.received.acquire().await?.forget();
        Ok(())
    }
}

impl Drop for EditServer {
    fn drop(&mut self) {
        self.state.shutdown.cancel();
        self.listener.abort();
    }
}

pub(super) struct Connected {
    connections: Connections,
    events: mpsc::Receiver<ConnectionEvent>,
    pub(super) original_attempt: AttemptId,
    pub(super) attempt: AttemptFiles,
}

impl Connected {
    pub(super) async fn reconnect(&mut self) -> TestResult<()> {
        let tab = self.attempt.attempt().tab;
        self.connections.reconnect(tab).await?;
        self.attempt = connect_attempt(&self.connections, &mut self.events, tab).await?;
        Ok(())
    }
}

async fn connect_attempt(
    connections: &Connections,
    events: &mut mpsc::Receiver<ConnectionEvent>,
    tab: crate::TabId,
) -> TestResult<AttemptFiles> {
    timeout(Duration::from_secs(9), async {
        while let Some(event) = events.recv().await {
            if event.attempt.tab != tab {
                continue;
            }
            match event.kind {
                ConnectionEventKind::Prompt(prompt) => {
                    let response = match prompt.kind {
                        PromptKind::Host { .. } => PromptResponse::Trust(true),
                        PromptKind::Password => {
                            PromptResponse::Secret(Zeroizing::new("fixture".into()))
                        }
                        _ => return Err(io::Error::other("unexpected fixture prompt").into()),
                    };
                    connections.respond(prompt.id, response).await?;
                }
                ConnectionEventKind::Connected { .. } => {
                    return Ok(connections.files(event.attempt).await?);
                }
                ConnectionEventKind::Ended(Err(error)) => return Err(error.into()),
                _ => {}
            }
        }
        Err(io::Error::other("event feed closed").into())
    })
    .await?
}

struct Ssh {
    state: Arc<State>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl server::Handler for Ssh {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if (user, password) == ("test", "fixture") {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        id: ChannelId,
        _: &str,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: &[(Pty, u32)],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(id)
    }

    async fn shell_request(
        &mut self,
        id: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.channels.remove(&id);
        session.channel_success(id)
    }

    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if name == "sftp"
            && let Some(channel) = self.channels.remove(&id)
        {
            session.channel_success(id)?;
            tokio::spawn(russh_sftp::server::run(
                channel.into_stream(),
                Files(self.state.clone()),
            ));
            return Ok(());
        }
        session.channel_failure(id)
    }
}

struct Files(Arc<State>);

fn success(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: String::new(),
        language_tag: String::new(),
    }
}

fn read_path<'a>(bytes: &mut &'a [u8]) -> Option<&'a str> {
    let (length, rest) = bytes.split_first_chunk::<4>()?;
    let (path, rest) = rest.split_at_checked(u32::from_be_bytes(*length) as usize)?;
    *bytes = rest;
    std::str::from_utf8(path).ok()
}

impl russh_sftp::server::Handler for Files {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(&mut self, _: u32, _: HashMap<String, String>) -> Result<Version, Self::Error> {
        let mut version = Version::new();
        version
            .extensions
            .insert("posix-rename@openssh.com".into(), "1".into());
        Ok(version)
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        assert_eq!(path, ".");
        Ok(Name {
            id,
            files: vec![File::new("/home/test", FileAttributes::default())],
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let files = self.0.files.lock().map_err(|_| StatusCode::Failure)?;
        let (mode, size) = if path == "/home/test" {
            (0o040700, 0)
        } else {
            (
                0o100600,
                files.get(&path).ok_or(StatusCode::NoSuchFile)?.len() as u64,
            )
        };
        Ok(Attrs {
            id,
            attrs: FileAttributes {
                permissions: Some(mode),
                size: Some(size),
                uid: Some(1000),
                gid: Some(1000),
                atime: Some(1),
                mtime: Some(1),
                ..Default::default()
            },
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        self.lstat(id, handle).await
    }

    async fn open(
        &mut self,
        id: u32,
        path: String,
        flags: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let mut files = self.0.files.lock().map_err(|_| StatusCode::Failure)?;
        if flags.contains(OpenFlags::CREATE | OpenFlags::EXCLUDE) && !files.contains_key(&path) {
            files.insert(path.clone(), Vec::new());
        } else if !flags.contains(OpenFlags::READ) || !files.contains_key(&path) {
            return Err(StatusCode::NoSuchFile);
        }
        Ok(Handle { id, handle: path })
    }

    async fn close(&mut self, id: u32, _: String) -> Result<Status, Self::Error> {
        Ok(success(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let files = self.0.files.lock().map_err(|_| StatusCode::Failure)?;
        let file = files.get(&handle).ok_or(StatusCode::NoSuchFile)?;
        let offset = usize::try_from(offset).map_err(|_| StatusCode::Failure)?;
        if offset >= file.len() {
            return Err(StatusCode::Eof);
        }
        let data = file
            .get(offset..(offset + len as usize).min(file.len()))
            .ok_or(StatusCode::Failure)?
            .to_vec();
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let mut files = self.0.files.lock().map_err(|_| StatusCode::Failure)?;
        let file = files.get_mut(&handle).ok_or(StatusCode::NoSuchFile)?;
        assert_eq!(offset, file.len() as u64);
        file.extend(data);
        Ok(success(id))
    }

    async fn remove(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        self.0
            .files
            .lock()
            .map_err(|_| StatusCode::Failure)?
            .remove(&path)
            .ok_or(StatusCode::NoSuchFile)?;
        Ok(success(id))
    }

    async fn extended(
        &mut self,
        id: u32,
        request: String,
        data: Vec<u8>,
    ) -> Result<Packet, Self::Error> {
        assert_eq!(request, "posix-rename@openssh.com");
        let mut bytes = data.as_slice();
        let old = read_path(&mut bytes).ok_or(StatusCode::BadMessage)?;
        let new = read_path(&mut bytes).ok_or(StatusCode::BadMessage)?;
        {
            let mut files = self.0.files.lock().map_err(|_| StatusCode::Failure)?;
            let contents = files.remove(old).ok_or(StatusCode::NoSuchFile)?;
            files.insert(new.to_owned(), contents);
        }
        self.0.publishes.fetch_add(1, SeqCst);
        self.0.received.add_permits(1);
        if self.0.stall.load(SeqCst) {
            self.0.shutdown.cancelled().await;
        }
        Ok(success(id).into())
    }
}
