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
use russh_sftp::protocol::{StatusCode, Version};
use ssh::{AuthMethod, Event, Input, PtySize, Session, SftpError, Target, connect};
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
    closed: mpsc::UnboundedReceiver<()>,
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
                tokio::spawn(russh_sftp::server::run(channel.into_stream(), Files));
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

struct Files;

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
    let (closed_tx, closed) = mpsc::unbounded_channel();
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    let public = client_key.public_key().clone();
    let task = tokio::spawn({
        let host_key = host_key.clone();
        let auth_calls = auth_calls.clone();
        let sftp_requests = sftp_requests.clone();
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
