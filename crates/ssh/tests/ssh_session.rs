use std::{
    collections::{HashMap, HashSet},
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
    time::Duration,
};

use russh::{
    Channel, ChannelId, Pty,
    keys::{Algorithm, HashAlg, PrivateKey, PublicKey, ssh_key::LineEnding},
    server::{self, Auth, ChannelOpenHandle, Msg, VerifiedPublicKey},
};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle as FileHandle, Name, OpenFlags, Packet, Status,
    StatusCode, Version,
};
use ssh::{
    AuthMethod, Event, Input, PtySize, PublishMode, RemoteKind, RemotePath, Session, SftpClient,
    SftpError, Target, connect,
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, mpsc},
    task::JoinHandle,
    time::timeout,
};
use zeroize::Zeroizing;

type TestResult = Result<(), Box<dyn std::error::Error>>;
const DEADLINE: Duration = Duration::from_secs(12);
const SIZE: PtySize = PtySize {
    columns: 80,
    rows: 24,
    pixel_width: 800,
    pixel_height: 480,
};

struct Fixture {
    task: JoinHandle<()>,
    target: Target,
    host_key: Arc<Mutex<PrivateKey>>,
    auth_calls: Arc<AtomicUsize>,
    sftp_requests: Arc<Semaphore>,
    files: Arc<Mutex<FixtureFiles>>,
    closed: mpsc::UnboundedReceiver<()>,
}

impl Fixture {
    fn file_state(&self) -> io::Result<std::sync::MutexGuard<'_, FixtureFiles>> {
        self.files
            .lock()
            .map_err(|_| io::Error::other("fixture lock poisoned"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Server {
    client_key: PublicKey,
    auth_calls: Arc<AtomicUsize>,
    closed: mpsc::UnboundedSender<()>,
    behavior: Behavior,
    sftp_requests: Arc<Semaphore>,
    channels: HashMap<ChannelId, Channel<Msg>>,
    sftp_channels: HashSet<ChannelId>,
    files: Arc<Mutex<FixtureFiles>>,
}

#[derive(Clone, Copy, Default)]
enum Behavior {
    #[default]
    Echo,
    EarlyOutput,
    HalfClose,
    Sftp,
    StallSftp,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.closed.send(());
    }
}

impl server::Handler for Server {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        self.auth_calls.fetch_add(1, SeqCst);
        Ok(if user == "test" && password == "fixture password" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        self.auth_calls.fetch_add(1, SeqCst);
        Ok(if user == "test" && key.public_key == &self.client_key {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if matches!(self.behavior, Behavior::Sftp) {
            self.channels.insert(channel.id(), channel);
        }
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if name == "sftp" {
            self.sftp_requests.add_permits(1);
            if matches!(self.behavior, Behavior::StallSftp) {
                return Ok(());
            }
            if let Some(channel) = self.channels.remove(&channel_id) {
                self.sftp_channels.insert(channel_id);
                session.channel_success(channel_id)?;
                tokio::spawn(russh_sftp::server::run(
                    channel.into_stream(),
                    Files {
                        state: self.files.clone(),
                        listed: 0,
                    },
                ));
                return Ok(());
            }
        }
        session.channel_failure(channel_id)
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.channels.remove(&channel);
        self.sftp_channels.remove(&channel);
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        cols: u32,
        rows: u32,
        width: u32,
        height: u32,
        _modes: &[(Pty, u32)],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if (term, cols, rows, width, height) == ("xterm-256color", 80, 24, 800, 480) {
            if matches!(self.behavior, Behavior::EarlyOutput) {
                session.data(channel, &b"early "[..])?;
            }
            session.channel_success(channel)
        } else {
            session.channel_failure(channel)
        }
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        session.data(channel, &b"ready"[..])?;
        if matches!(self.behavior, Behavior::HalfClose) {
            session.eof(channel)?;
        }
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        cols: u32,
        rows: u32,
        width: u32,
        height: u32,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        session.data(
            channel,
            format!("{cols}x{rows}:{width}x{height}").into_bytes(),
        )
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if self.sftp_channels.contains(&channel) {
            return Ok(());
        }
        if matches!(self.behavior, Behavior::HalfClose) {
            assert_eq!(data, b"after EOF");
            return session.close(channel);
        }
        session.data(channel, data.to_vec())
    }
}

#[derive(Default)]
struct FixtureFiles {
    contents: HashMap<String, Vec<u8>>,
    modes: HashMap<String, u32>,
    entry_count: usize,
    stall_rename: bool,
    rename_calls: usize,
    extended_renames: usize,
}

struct Files {
    state: Arc<Mutex<FixtureFiles>>,
    listed: usize,
}

fn attributes(mode: u32, size: u64) -> FileAttributes {
    FileAttributes {
        permissions: Some(mode),
        size: Some(size),
        uid: Some(1000),
        gid: Some(1000),
        atime: Some(1),
        mtime: Some(1),
        ..Default::default()
    }
}

fn ok_status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: String::new(),
        language_tag: String::new(),
    }
}

impl russh_sftp::server::Handler for Files {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        assert_eq!(version, 3);
        let mut response = Version::new();
        response
            .extensions
            .insert("posix-rename@openssh.com".into(), "1".into());
        Ok(response)
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        assert_eq!(path, ".");
        Ok(Name {
            id,
            files: vec![File::new("/home/test", FileAttributes::default())],
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<FileHandle, Self::Error> {
        assert_eq!(path, "/home/test");
        self.listed = 0;
        Ok(FileHandle { id, handle: path })
    }

    async fn readdir(&mut self, id: u32, _handle: String) -> Result<Name, Self::Error> {
        let state = self.state.lock().map_err(|_| StatusCode::Failure)?;
        let end = (self.listed + 100).min(state.entry_count);
        if self.listed >= end {
            return Err(StatusCode::Eof);
        }
        let files = (self.listed..end)
            .map(|index| File::new(format!("file-{index}"), attributes(0o100644, 0)))
            .collect();
        self.listed = end;
        Ok(Name { id, files })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let attrs = match path.as_str() {
            "/home/test" => attributes(0o040755, 0),
            "/home/test/link" => attributes(0o120777, 0),
            "/home/test/special" => attributes(0o010644, 0),
            _ => {
                let state = self.state.lock().map_err(|_| StatusCode::Failure)?;
                let bytes = state.contents.get(&path).ok_or(StatusCode::NoSuchFile)?;
                attributes(
                    0o100000 | state.modes.get(&path).copied().unwrap_or(0o644),
                    bytes.len() as u64,
                )
            }
        };
        Ok(Attrs { id, attrs })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        self.lstat(id, handle).await
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: OpenFlags,
        attrs: FileAttributes,
    ) -> Result<FileHandle, Self::Error> {
        let mut state = self.state.lock().map_err(|_| StatusCode::Failure)?;
        if flags.bits() == (OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE).bits() {
            assert_eq!(attrs.permissions, Some(0o600));
            if state.contents.contains_key(&filename) {
                return Err(StatusCode::Failure);
            }
            state.contents.insert(filename.clone(), Vec::new());
            state.modes.insert(filename.clone(), 0o600);
        } else if flags.bits() != OpenFlags::READ.bits() || !state.contents.contains_key(&filename)
        {
            return Err(StatusCode::NoSuchFile);
        }
        Ok(FileHandle {
            id,
            handle: filename,
        })
    }

    async fn close(&mut self, id: u32, _handle: String) -> Result<Status, Self::Error> {
        Ok(ok_status(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        handle: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let mut state = self.state.lock().map_err(|_| StatusCode::Failure)?;
        let mode = attrs.permissions.ok_or(StatusCode::Failure)?;
        *state.modes.get_mut(&handle).ok_or(StatusCode::NoSuchFile)? = mode;
        Ok(ok_status(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        assert!(len <= 16 * 1024);
        let state = self.state.lock().map_err(|_| StatusCode::Failure)?;
        let bytes = state.contents.get(&handle).ok_or(StatusCode::NoSuchFile)?;
        let offset = usize::try_from(offset).map_err(|_| StatusCode::Failure)?;
        if offset >= bytes.len() {
            return Err(StatusCode::Eof);
        }
        let data = bytes
            .get(offset..(offset + len as usize).min(bytes.len()))
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
        assert!(data.len() <= 16 * 1024);
        let mut state = self.state.lock().map_err(|_| StatusCode::Failure)?;
        let bytes = state
            .contents
            .get_mut(&handle)
            .ok_or(StatusCode::NoSuchFile)?;
        assert_eq!(offset, bytes.len() as u64);
        bytes.extend(data);
        Ok(ok_status(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let mut state = self.state.lock().map_err(|_| StatusCode::Failure)?;
        state
            .contents
            .remove(&filename)
            .ok_or(StatusCode::NoSuchFile)?;
        state.modes.remove(&filename);
        Ok(ok_status(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let stall = {
            let mut state = self.state.lock().map_err(|_| StatusCode::Failure)?;
            state.rename_calls += 1;
            if state.contents.contains_key(&newpath) {
                return Err(StatusCode::Failure);
            }
            let file = state
                .contents
                .remove(&oldpath)
                .ok_or(StatusCode::NoSuchFile)?;
            if let Some(mode) = state.modes.remove(&oldpath) {
                state.modes.insert(newpath.clone(), mode);
            }
            state.contents.insert(newpath, file);
            state.stall_rename
        };
        if stall {
            return std::future::pending().await;
        }
        Ok(ok_status(id))
    }

    async fn extended(
        &mut self,
        id: u32,
        request: String,
        data: Vec<u8>,
    ) -> Result<Packet, Self::Error> {
        if request != "posix-rename@openssh.com" {
            return Err(StatusCode::OpUnsupported);
        }
        fn read_path<'a>(bytes: &mut &'a [u8]) -> Option<&'a str> {
            let (length, rest) = bytes.split_first_chunk::<4>()?;
            let (path, rest) = rest.split_at_checked(u32::from_be_bytes(*length) as usize)?;
            *bytes = rest;
            std::str::from_utf8(path).ok()
        }
        let mut bytes = data.as_slice();
        let old = read_path(&mut bytes).ok_or(StatusCode::BadMessage)?;
        let new = read_path(&mut bytes).ok_or(StatusCode::BadMessage)?;
        assert!(bytes.is_empty());
        let mut state = self.state.lock().map_err(|_| StatusCode::Failure)?;
        let contents = state.contents.remove(old).ok_or(StatusCode::NoSuchFile)?;
        let mode = state.modes.remove(old).ok_or(StatusCode::NoSuchFile)?;
        state.contents.insert(new.to_owned(), contents);
        state.modes.insert(new.to_owned(), mode);
        state.extended_renames += 1;
        Ok(ok_status(id).into())
    }
}

async fn fixture() -> Result<(Fixture, PrivateKey), Box<dyn std::error::Error>> {
    fixture_with(Behavior::default()).await
}

async fn fixture_with(
    behavior: Behavior,
) -> Result<(Fixture, PrivateKey), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let target = Target::parse(&format!("test@{}", listener.local_addr()?))?;
    let host_key = Arc::new(Mutex::new(PrivateKey::random(
        &mut rand::rng(),
        Algorithm::Ed25519,
    )?));
    let auth_calls = Arc::new(AtomicUsize::new(0));
    let sftp_requests = Arc::new(Semaphore::new(0));
    let files = Arc::new(Mutex::new(FixtureFiles::default()));
    let (closed_tx, closed) = mpsc::unbounded_channel();
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    let public = client_key.public_key().clone();
    let task = tokio::spawn({
        let host_key = host_key.clone();
        let auth_calls = auth_calls.clone();
        let sftp_requests = sftp_requests.clone();
        let files = files.clone();
        async move {
            while let Ok((socket, _)) = listener.accept().await {
                let key = match host_key.lock() {
                    Ok(key) => key.clone(),
                    Err(_) => break,
                };
                let config = Arc::new(server::Config {
                    keys: vec![key],
                    auth_rejection_time: Duration::ZERO,
                    ..Default::default()
                });
                let server = Server {
                    client_key: public.clone(),
                    auth_calls: auth_calls.clone(),
                    closed: closed_tx.clone(),
                    behavior,
                    sftp_requests: sftp_requests.clone(),
                    channels: HashMap::new(),
                    sftp_channels: HashSet::new(),
                    files: files.clone(),
                };
                tokio::spawn(async move {
                    if let Ok(session) = server::run_stream(config, socket, server).await {
                        let _ = session.await;
                    }
                });
            }
        }
    });
    Ok((
        Fixture {
            task,
            target,
            host_key,
            auth_calls,
            sftp_requests,
            files,
            closed,
        },
        client_key,
    ))
}

impl Fixture {
    fn connect(&self, auth: AuthMethod) -> Session {
        connect(
            &tokio::runtime::Handle::current(),
            self.target.clone(),
            auth,
            SIZE,
        )
    }
}

async fn next(session: &mut Session) -> Result<Event, Box<dyn std::error::Error>> {
    Ok(timeout(DEADLINE, session.recv())
        .await?
        .ok_or(io::Error::other("session closed early"))?)
}

async fn trust(
    session: &mut Session,
    approve: bool,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let Event::VerifyHost {
        key,
        fingerprint,
        reply,
    } = next(session).await?
    else {
        return Err(io::Error::other("host verification must come first").into());
    };
    let expected = PublicKey::from_bytes(&key)?
        .fingerprint(HashAlg::Sha256)
        .to_string();
    assert_eq!(fingerprint, expected);
    reply
        .send(approve)
        .map_err(|_| io::Error::other("trust prompt was cancelled"))?;
    Ok(key)
}

async fn password(session: &mut Session) -> TestResult {
    respond_password(session).await?;
    connected(session).await
}

async fn respond_password(session: &mut Session) -> TestResult {
    let Event::Password { reply } = next(session).await? else {
        return Err(io::Error::other("expected password prompt").into());
    };
    reply
        .send(Zeroizing::new("fixture password".into()))
        .map_err(|_| io::Error::other("password prompt was cancelled"))?;
    Ok(())
}

#[tokio::test]
async fn setup_preserves_output_sent_before_request_reply() -> TestResult {
    let (server, _) = fixture_with(Behavior::EarlyOutput).await?;
    let mut session = server.connect(AuthMethod::Password);
    trust(&mut session, true).await?;
    respond_password(&mut session).await?;
    assert!(matches!(next(&mut session).await?, Event::Connected));
    assert_eq!(output(&mut session, 11).await?, b"early ready");
    Ok(())
}

#[tokio::test]
async fn remote_eof_leaves_client_writes_open_until_close() -> TestResult {
    let (server, _) = fixture_with(Behavior::HalfClose).await?;
    let mut session = server.connect(AuthMethod::Password);
    trust(&mut session, true).await?;
    password(&mut session).await?;
    assert!(
        timeout(Duration::from_millis(100), session.recv())
            .await
            .is_err()
    );
    session.try_send(Input::Bytes(b"after EOF".to_vec()))?;
    assert!(matches!(next(&mut session).await?, Event::Ended(Ok(()))));
    Ok(())
}

async fn connected(session: &mut Session) -> TestResult {
    assert!(matches!(next(session).await?, Event::Connected));
    assert_eq!(output(session, 5).await?, b"ready");
    Ok(())
}

async fn output(session: &mut Session, size: usize) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    while bytes.len() < size {
        if let Event::Output(chunk) = next(session).await? {
            bytes.extend(chunk);
        } else {
            return Err(io::Error::other("expected terminal bytes").into());
        }
    }
    Ok(bytes)
}

#[tokio::test]
async fn sftp_opens_once_without_reauthentication() -> TestResult {
    let (server, _) = fixture_with(Behavior::Sftp).await?;
    let mut session = server.connect(AuthMethod::Password);
    let opener = session.sftp_opener();
    assert!(matches!(opener.open().await, Err(SftpError::NotConnected)));
    trust(&mut session, true).await?;
    password(&mut session).await?;

    let first = timeout(DEADLINE, opener.open()).await??;
    let second = timeout(DEADLINE, opener.open()).await??;
    assert!(Arc::ptr_eq(&first, &second));
    assert!(first.supports_atomic_replace());
    assert_eq!(server.auth_calls.load(SeqCst), 1);
    assert_eq!(server.sftp_requests.available_permits(), 1);
    session.try_send(Input::Bytes(b"still live".to_vec()))?;
    assert_eq!(output(&mut session, 10).await?, b"still live");
    Ok(())
}

#[tokio::test]
async fn sftp_refusal_preserves_shell() -> TestResult {
    let (server, _) = fixture().await?;
    let mut session = server.connect(AuthMethod::Password);
    trust(&mut session, true).await?;
    password(&mut session).await?;

    let opener = session.sftp_opener();
    for _ in 0..2 {
        assert!(matches!(
            timeout(DEADLINE, opener.open()).await?,
            Err(SftpError::SubsystemUnavailable)
        ));
    }
    assert_eq!(server.sftp_requests.available_permits(), 2);
    session.try_send(Input::Bytes(b"still live".to_vec()))?;
    assert_eq!(output(&mut session, 10).await?, b"still live");
    Ok(())
}

#[tokio::test]
async fn sftp_opener_expires_with_session() -> TestResult {
    let (mut server, _) = fixture_with(Behavior::StallSftp).await?;
    let mut session = server.connect(AuthMethod::Password);
    trust(&mut session, true).await?;
    password(&mut session).await?;

    let opener = session.sftp_opener();
    let pending = tokio::spawn({
        let opener = opener.clone();
        async move { opener.open().await }
    });
    timeout(DEADLINE, server.sftp_requests.acquire())
        .await??
        .forget();
    session.try_send(Input::Bytes(b"still live".to_vec()))?;
    assert_eq!(output(&mut session, 10).await?, b"still live");
    session.cancel();
    assert!(matches!(
        timeout(DEADLINE, pending).await??,
        Err(SftpError::Disconnected)
    ));
    assert!(matches!(opener.open().await, Err(SftpError::Disconnected)));
    assert_eq!(timeout(DEADLINE, server.closed.recv()).await?, Some(()));
    Ok(())
}

async fn connected_files(
    server: &Fixture,
) -> Result<(Session, Arc<SftpClient>), Box<dyn std::error::Error>> {
    let mut session = server.connect(AuthMethod::Password);
    trust(&mut session, true).await?;
    password(&mut session).await?;
    let files = timeout(DEADLINE, session.sftp_opener().open()).await??;
    Ok((session, files))
}

#[test]
fn remote_paths_do_not_reinterpret_invalid_children() -> TestResult {
    assert_eq!(RemotePath::absolute("/")?.parent(), None);
    let home = RemotePath::absolute("/home/ops")?;
    assert_eq!(home.child("résumé.txt")?.as_str(), "/home/ops/résumé.txt");
    for invalid in ["", ".", "..", "a/b", "a\\b", "\nname"] {
        assert!(matches!(home.child(invalid), Err(SftpError::InvalidPath)));
    }
    assert!(matches!(
        RemotePath::absolute("/a//b"),
        Err(SftpError::InvalidPath)
    ));
    Ok(())
}

#[tokio::test]
async fn sftp_remote_listing_is_bounded() -> TestResult {
    let (server, _) = fixture_with(Behavior::Sftp).await?;
    let (_session, files) = connected_files(&server).await?;
    let home = files.home().await?;
    assert_eq!(home.as_str(), "/home/test");
    server.file_state()?.entry_count = 10_001;
    assert!(matches!(
        timeout(DEADLINE, files.list(&home)).await?,
        Err(SftpError::DirectoryTooLarge)
    ));
    server.file_state()?.entry_count = 10_000;
    assert_eq!(timeout(DEADLINE, files.list(&home)).await??.len(), 10_000);
    Ok(())
}

#[tokio::test]
async fn sftp_create_only_keeps_late_destination() -> TestResult {
    let (server, _) = fixture_with(Behavior::Sftp).await?;
    let (_session, files) = connected_files(&server).await?;
    let path = files.home().await?.child("report.txt")?;
    let mut stage = files.stage_write(&path).await?;
    stage.write_all(b"new").await?;
    stage.seal(None).await?;
    server
        .file_state()?
        .contents
        .insert(path.as_str().into(), b"old".to_vec());
    assert!(matches!(
        stage.publish(PublishMode::CreateOnly).await,
        Err(SftpError::AlreadyExists)
    ));
    stage.abort().await?;
    let state = server.file_state()?;
    assert_eq!(state.contents.len(), 1);
    assert_eq!(
        state.contents.get(path.as_str()).map(Vec::as_slice),
        Some(b"old".as_slice())
    );
    Ok(())
}

#[tokio::test]
async fn sftp_publish_timeout_is_unknown() -> TestResult {
    let (server, _) = fixture_with(Behavior::Sftp).await?;
    let (_session, files) = connected_files(&server).await?;
    let path = files.home().await?.child("report.txt")?;
    let mut stage = files.stage_write(&path).await?;
    stage.write_all(b"new").await?;
    stage.seal(None).await?;
    server.file_state()?.stall_rename = true;
    for _ in 0..2 {
        assert!(matches!(
            timeout(DEADLINE, stage.publish(PublishMode::CreateOnly)).await?,
            Err(SftpError::OutcomeUnknown)
        ));
    }
    let state = server.file_state()?;
    assert_eq!(state.rename_calls, 1);
    assert_eq!(
        state.contents.get(path.as_str()).map(Vec::as_slice),
        Some(b"new".as_slice())
    );
    Ok(())
}

#[tokio::test]
async fn sftp_streams_regular_files_without_following_special_entries() -> TestResult {
    let (server, _) = fixture_with(Behavior::Sftp).await?;
    let (_session, files) = connected_files(&server).await?;
    let home = files.home().await?;
    for (name, kind) in [
        ("link", RemoteKind::Symlink),
        ("special", RemoteKind::Other),
    ] {
        let path = home.child(name)?;
        assert_eq!(files.metadata(&path).await?.kind, kind);
        assert!(matches!(
            files.open_read(&path).await,
            Err(SftpError::NotRegularFile)
        ));
    }
    let path = home.child("regular")?;
    let expected = vec![42; 32 * 1024 + 9];
    let mut stage = files.stage_write(&path).await?;
    stage.write_all(&expected).await?;
    stage.seal(None).await?;
    stage.publish(PublishMode::CreateOnly).await?;
    let mut reader = files.open_read(&path).await?;
    let mut actual = Vec::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let size = reader.read(&mut buffer).await?;
        if size == 0 {
            break;
        }
        actual.extend_from_slice(&buffer[..size]);
    }
    reader.close().await?;
    assert_eq!(actual, expected);
    Ok(())
}

#[tokio::test]
async fn sftp_aborted_partial_upload_keeps_original() -> TestResult {
    let (server, _) = fixture_with(Behavior::Sftp).await?;
    let (_session, files) = connected_files(&server).await?;
    let path = files.home().await?.child("report.txt")?;
    server
        .file_state()?
        .contents
        .insert(path.as_str().into(), b"old".to_vec());
    let mut stage = files.stage_write(&path).await?;
    stage.write_all(b"partial").await?;
    timeout(DEADLINE, stage.abort()).await??;

    let state = server.file_state()?;
    assert_eq!(state.contents.len(), 1);
    assert_eq!(
        state.contents.get(path.as_str()).map(Vec::as_slice),
        Some(b"old".as_slice())
    );
    assert_eq!(state.rename_calls, 0);
    Ok(())
}

#[tokio::test]
async fn sftp_atomic_publish_preserves_remote_mode_and_replaces_file() -> TestResult {
    let (server, _) = fixture_with(Behavior::Sftp).await?;
    let (_session, files) = connected_files(&server).await?;
    let path = files.home().await?.child("report.txt")?;
    server
        .file_state()?
        .contents
        .insert(path.as_str().into(), b"old".to_vec());
    let metadata = files.metadata(&path).await?;
    let mut stage = files.stage_write(&path).await?;
    stage.write_all(b"saved").await?;
    stage.seal(Some(&metadata)).await?;
    stage.publish(PublishMode::AtomicReplace).await?;

    {
        let state = server.file_state()?;
        assert_eq!(state.contents.len(), 1);
        assert_eq!(
            state.contents.get(path.as_str()).map(Vec::as_slice),
            Some(b"saved".as_slice())
        );
        assert_eq!(state.extended_renames, 1);
        assert_eq!(state.rename_calls, 0);
    }
    let after = files.metadata(&path).await?;
    assert_eq!(
        (after.mode, after.uid, after.gid),
        (metadata.mode, metadata.uid, metadata.gid)
    );
    Ok(())
}

#[tokio::test]
async fn password_requires_approved_host_before_auth() -> TestResult {
    let (server, _) = fixture().await?;
    let mut session = server.connect(AuthMethod::Password);
    let Event::VerifyHost { reply, .. } = next(&mut session).await? else {
        return Err(io::Error::other("expected trust before authentication").into());
    };
    assert!(
        timeout(Duration::from_millis(100), session.recv())
            .await
            .is_err()
    );
    assert_eq!(server.auth_calls.load(SeqCst), 0);
    assert!(reply.send(true).is_ok());
    password(&mut session).await?;
    assert_eq!(server.auth_calls.load(SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn key_login_opens_remote_pty() -> TestResult {
    let (server, key) = fixture().await?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("key");
    std::fs::write(&path, key.to_openssh(LineEnding::LF)?.as_bytes())?;
    let mut session = server.connect(AuthMethod::PrivateKey { path: None });
    trust(&mut session, true).await?;
    let Event::KeyPath { reply } = next(&mut session).await? else {
        return Err(io::Error::other("expected key path prompt").into());
    };
    assert!(reply.send(path).is_ok());
    connected(&mut session).await
}

#[tokio::test]
async fn encrypted_key_requests_passphrase_after_verification() -> TestResult {
    let (server, key) = fixture().await?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("key");
    let encrypted = key.encrypt(&mut rand::rng(), b"fixture passphrase")?;
    std::fs::write(&path, encrypted.to_openssh(LineEnding::LF)?.as_bytes())?;
    let mut session = server.connect(AuthMethod::PrivateKey { path: Some(path) });
    trust(&mut session, true).await?;
    let Event::Passphrase { reply } = next(&mut session).await? else {
        return Err(io::Error::other("expected passphrase prompt").into());
    };
    assert!(
        reply
            .send(Zeroizing::new("fixture passphrase".into()))
            .is_ok()
    );
    connected(&mut session).await
}

#[tokio::test]
async fn rejected_host_never_authenticates() -> TestResult {
    let (server, _) = fixture().await?;
    let mut session = server.connect(AuthMethod::Password);
    trust(&mut session, false).await?;
    assert!(matches!(next(&mut session).await?, Event::Ended(Err(_))));
    assert_eq!(server.auth_calls.load(SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn rotated_host_key_never_authenticates() -> TestResult {
    let (server, _) = fixture().await?;
    let mut first = server.connect(AuthMethod::Password);
    let pin = trust(&mut first, true).await?;
    password(&mut first).await?;
    first.cancel();
    *server
        .host_key
        .lock()
        .map_err(|_| io::Error::other("key lock poisoned"))? =
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    let mut second = server.connect(AuthMethod::Password);
    let Event::VerifyHost { key, reply, .. } = next(&mut second).await? else {
        return Err(io::Error::other("expected verification for the rotated key").into());
    };
    assert_ne!(key, pin);
    assert!(reply.send(key == pin).is_ok());
    assert!(matches!(next(&mut second).await?, Event::Ended(Err(_))));
    assert_eq!(server.auth_calls.load(SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn closing_pending_session_releases_socket() -> TestResult {
    let (mut server, _) = fixture().await?;
    let mut session = server.connect(AuthMethod::Password);
    let Event::VerifyHost { reply, .. } = next(&mut session).await? else {
        return Err(io::Error::other("expected pending trust prompt").into());
    };
    session.cancel();
    assert!(timeout(DEADLINE, server.closed.recv()).await?.is_some());
    assert!(reply.send(true).is_err());
    assert_eq!(server.auth_calls.load(SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn two_sessions_forward_independent_io_and_resize() -> TestResult {
    let (mut server, _) = fixture().await?;
    let mut first = server.connect(AuthMethod::Password);
    let mut second = server.connect(AuthMethod::Password);
    trust(&mut first, true).await?;
    trust(&mut second, true).await?;
    password(&mut first).await?;
    password(&mut second).await?;
    first.try_send(Input::Resize(PtySize {
        columns: 100,
        rows: 30,
        ..SIZE
    }))?;
    second.try_send(Input::Bytes(b"second".to_vec()))?;
    assert_eq!(
        output(&mut first, b"100x30:800x480".len()).await?,
        b"100x30:800x480"
    );
    assert_eq!(output(&mut second, 6).await?, b"second");
    first.cancel();
    assert!(timeout(DEADLINE, server.closed.recv()).await?.is_some());
    second.try_send(Input::Bytes(b"alive".to_vec()))?;
    assert_eq!(output(&mut second, 5).await?, b"alive");
    Ok(())
}

#[tokio::test]
async fn backpressure_keeps_byte_order_and_allows_cancel() -> TestResult {
    use ssh::SendInputError;
    let (mut server, _) = fixture().await?;
    let mut session = server.connect(AuthMethod::Password);
    trust(&mut session, true).await?;
    password(&mut session).await?;
    let mut sent = Vec::new();
    let mut retry = None;
    for byte in 0..=255u8 {
        match session.try_send(Input::Bytes(vec![byte])) {
            Ok(()) => sent.push(byte),
            Err(SendInputError::Full(input)) => {
                assert!(matches!(&input, Input::Bytes(bytes) if bytes == &[byte]));
                retry = Some(input);
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let retry = retry.ok_or(io::Error::other("bounded input queue never filled"))?;
    assert_eq!(output(&mut session, sent.len()).await?, sent);
    session.try_send(retry)?;
    assert_eq!(output(&mut session, 1).await?, [sent.len() as u8]);
    for _ in 0..64 {
        if session
            .try_send(Input::Bytes(vec![b'a'; 16 * 1024]))
            .is_err()
        {
            break;
        }
    }
    session.cancel();
    assert!(timeout(DEADLINE, server.closed.recv()).await?.is_some());
    Ok(())
}
