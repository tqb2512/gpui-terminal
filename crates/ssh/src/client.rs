use std::{fs::File, io::Read, path::PathBuf, sync::Arc, time::Duration};

use russh::{
    Channel, ChannelMsg, client,
    keys::{self, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate},
};
use tokio::{
    io,
    net::TcpStream,
    runtime::Handle,
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::sftp::{self, SftpOpener, SftpRequests};
use crate::{AuthMethod, Target};

const CHUNK: usize = 16 * 1024;
const KEY_LIMIT: u64 = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SETUP_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PtySize {
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

pub enum Event {
    VerifyHost {
        key: Vec<u8>,
        fingerprint: String,
        reply: oneshot::Sender<bool>,
    },
    Password {
        reply: oneshot::Sender<Zeroizing<String>>,
    },
    KeyPath {
        reply: oneshot::Sender<PathBuf>,
    },
    Passphrase {
        reply: oneshot::Sender<Zeroizing<String>>,
    },
    Connected,
    Output(Vec<u8>),
    Ended(Result<(), SessionError>),
}

pub enum Input {
    Bytes(Vec<u8>),
    Resize(PtySize),
}

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(bytes) => f.debug_tuple("Bytes").field(&bytes.len()).finish(),
            Self::Resize(size) => f.debug_tuple("Resize").field(size).finish(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SendInputError {
    #[error("The SSH input queue is full. Try again.")]
    Full(Input),
    #[error("The SSH session is closed.")]
    Closed(Input),
    #[error("The SSH input exceeds the maximum chunk size.")]
    TooLarge(Input),
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("SSH connection cancelled.")]
    Cancelled,
    #[error("Could not reach the SSH server. Check the host and port.")]
    Tcp,
    #[error("The SSH server did not respond in time.")]
    Timeout,
    #[error("The SSH server identity was not approved.")]
    HostRejected,
    #[error("The SSH server supplied an unsupported identity.")]
    HostIdentity,
    #[error("The SSH server rejected the requested authentication method or credential.")]
    Authentication,
    #[error("Could not open or read the private key file.")]
    KeyRead,
    #[error("The private key is not supported or the passphrase was incorrect.")]
    KeyDecode,
    #[error("The SSH server refused the remote PTY or shell.")]
    Shell,
    #[error("The SSH server sent too much terminal output before accepting the shell.")]
    EarlyOutput,
    #[error("The SSH connection failed or was interrupted.")]
    Protocol,
    #[error("The private key could not be processed.")]
    KeyWorker,
}

impl From<russh::Error> for SessionError {
    fn from(error: russh::Error) -> Self {
        match error {
            russh::Error::UnknownKey => Self::HostRejected,
            _ => Self::Protocol,
        }
    }
}

pub struct Session {
    events: mpsc::Receiver<Event>,
    input: mpsc::Sender<Input>,
    cancel: CancellationToken,
    sftp: SftpOpener,
}

impl Session {
    pub fn sftp_opener(&self) -> SftpOpener {
        self.sftp.clone()
    }

    pub fn split_input(&self) -> InputSender {
        InputSender {
            input: self.input.clone(),
            cancel: self.cancel.clone(),
        }
    }

    pub async fn recv(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    pub fn try_send(&self, input: Input) -> Result<(), SendInputError> {
        if self.cancel.is_cancelled() {
            return Err(SendInputError::Closed(input));
        }
        if matches!(&input, Input::Bytes(bytes) if bytes.len() > CHUNK) {
            return Err(SendInputError::TooLarge(input));
        }
        self.input.try_send(input).map_err(|error| match error {
            mpsc::error::TrySendError::Full(input) => SendInputError::Full(input),
            mpsc::error::TrySendError::Closed(input) => SendInputError::Closed(input),
        })
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

#[derive(Clone)]
pub struct InputSender {
    input: mpsc::Sender<Input>,
    cancel: CancellationToken,
}

impl InputSender {
    pub async fn reserve(&self) -> Option<InputPermit<'_>> {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => None,
            permit = self.input.reserve() => permit.ok().map(InputPermit),
        }
    }
}

pub struct InputPermit<'a>(mpsc::Permit<'a, Input>);

impl InputPermit<'_> {
    pub fn send(self, input: Input) -> Result<(), SendInputError> {
        if matches!(&input, Input::Bytes(bytes) if bytes.len() > CHUNK) {
            return Err(SendInputError::TooLarge(input));
        }
        self.0.send(input);
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub fn connect(runtime: &Handle, target: Target, auth: AuthMethod, size: PtySize) -> Session {
    let (event_sender, events) = mpsc::channel(32);
    let (input, commands) = mpsc::channel(32);
    let cancel = CancellationToken::new();
    let sftp_cancel = cancel.child_token();
    let (sftp, sftp_requests) = sftp::requests(sftp_cancel.clone());
    runtime.spawn({
        let cancel = cancel.clone();
        async move {
            let result = {
                let _sftp_lifetime = sftp_cancel.drop_guard();
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    result = run(target, auth, size, commands, &event_sender, &cancel, sftp_requests) => result,
                }
            };
            tokio::select! {
                () = cancel.cancelled() => {}
                _ = event_sender.send(Event::Ended(result)) => {}
            }
        }
    });
    Session {
        events,
        input,
        cancel,
        sftp,
    }
}

struct TrustHandler {
    events: mpsc::Sender<Event>,
    cancel: CancellationToken,
    accepted_key: Option<Vec<u8>>,
}

impl client::Handler for TrustHandler {
    type Error = SessionError;

    async fn check_server_key(
        &mut self,
        identity: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = identity else {
            return Err(SessionError::HostIdentity);
        };
        let bytes = key.to_bytes().map_err(|_| SessionError::HostIdentity)?;
        if let Some(accepted_key) = &self.accepted_key {
            return Ok(accepted_key == &bytes);
        }
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let approved = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Err(SessionError::Cancelled),
            approved = ask(&self.events, |reply| Event::VerifyHost {
                key: bytes.clone(), fingerprint, reply,
            }) => approved?,
        };
        if approved {
            self.accepted_key = Some(bytes);
        }
        Ok(approved)
    }
}

async fn ask<T>(
    events: &mpsc::Sender<Event>,
    event: impl FnOnce(oneshot::Sender<T>) -> Event,
) -> Result<T, SessionError> {
    let (reply, response) = oneshot::channel();
    events
        .send(event(reply))
        .await
        .map_err(|_| SessionError::Cancelled)?;
    response.await.map_err(|_| SessionError::Cancelled)
}

async fn run(
    target: Target,
    auth: AuthMethod,
    size: PtySize,
    mut commands: mpsc::Receiver<Input>,
    events: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
    sftp_requests: SftpRequests,
) -> Result<(), SessionError> {
    let endpoint = target.endpoint();
    let mut socket = timeout(
        REQUEST_TIMEOUT,
        TcpStream::connect((endpoint.host(), endpoint.port())),
    )
    .await
    .map_err(|_| SessionError::Timeout)?
    .map_err(|_| SessionError::Tcp)?;
    let (stream, mut bridge) = io::duplex(64 * 1024);
    let io_cancel = cancel.child_token();
    let _cancel_io = io_cancel.clone().drop_guard();
    // Russh owns a spawned protocol task; this bridge also closes TCP if that task is waiting on a prompt or a full channel.
    tokio::spawn({
        let io_cancel = io_cancel.clone();
        async move {
            tokio::select! {
                biased;
                () = io_cancel.cancelled() => {}
                _ = io::copy_bidirectional(&mut socket, &mut bridge) => {}
            }
        }
    });
    let config = client::Config {
        window_size: 256 * 1024,
        channel_buffer_size: 16,
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    };
    let mut connection = timeout(
        SETUP_TIMEOUT,
        client::connect_stream(
            Arc::new(config),
            stream,
            TrustHandler {
                events: events.clone(),
                cancel: io_cancel,
                accepted_key: None,
            },
        ),
    )
    .await
    .map_err(|_| SessionError::Timeout)??;

    let authenticated = match auth {
        AuthMethod::Password => {
            let password = ask(events, |reply| Event::Password { reply }).await?;
            timeout(
                REQUEST_TIMEOUT,
                connection.authenticate_password(target.username(), password.as_str()),
            )
            .await
            .map_err(|_| SessionError::Timeout)??
        }
        AuthMethod::PrivateKey { path } => {
            let path = match path {
                Some(path) => path,
                None => ask(events, |reply| Event::KeyPath { reply }).await?,
            };
            let key = load_key(path, events).await?;
            let hash = timeout(REQUEST_TIMEOUT, connection.best_supported_rsa_hash())
                .await
                .map_err(|_| SessionError::Timeout)??
                .flatten();
            timeout(
                REQUEST_TIMEOUT,
                connection.authenticate_publickey(
                    target.username(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                ),
            )
            .await
            .map_err(|_| SessionError::Timeout)??
        }
    };
    if !authenticated.success() {
        return Err(SessionError::Authentication);
    }

    let mut channel = timeout(REQUEST_TIMEOUT, connection.channel_open_session())
        .await
        .map_err(|_| SessionError::Timeout)??;
    channel
        .request_pty(
            true,
            "xterm-256color",
            size.columns.max(1).into(),
            size.rows.max(1).into(),
            size.pixel_width.into(),
            size.pixel_height.into(),
            &[],
        )
        .await?;
    let mut early_output = Vec::new();
    expect_success(&mut channel, &mut early_output).await?;
    channel.request_shell(true).await?;
    expect_success(&mut channel, &mut early_output).await?;
    sftp_requests.set_ready();
    events
        .send(Event::Connected)
        .await
        .map_err(|_| SessionError::Cancelled)?;
    let (mut reader, writer) = channel.split();
    tokio::select! {
        () = sftp_requests.serve(&connection) => Err(SessionError::Cancelled),
        result = async {
            forward_output(events, &early_output).await?;
            drop(early_output);
            while let Some(message) = reader.wait().await {
                match message {
                    ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                        forward_output(events, &data).await?;
                    }
                    ChannelMsg::Close => return Ok(()),
                    _ => {}
                }
            }
            Err(SessionError::Protocol)
        } => result,
        result = async {
            while let Some(input) = commands.recv().await {
                match input {
                    Input::Bytes(bytes) => writer.data_bytes(bytes).await?,
                    Input::Resize(size) => writer.window_change(
                        size.columns.max(1).into(), size.rows.max(1).into(),
                        size.pixel_width.into(), size.pixel_height.into(),
                    ).await?,
                }
            }
            Ok(())
        } => result,
    }
}

async fn forward_output(events: &mpsc::Sender<Event>, bytes: &[u8]) -> Result<(), SessionError> {
    for chunk in bytes.chunks(CHUNK) {
        events
            .send(Event::Output(chunk.to_vec()))
            .await
            .map_err(|_| SessionError::Cancelled)?;
    }
    Ok(())
}

async fn expect_success(
    channel: &mut Channel<client::Msg>,
    early_output: &mut Vec<u8>,
) -> Result<(), SessionError> {
    timeout(REQUEST_TIMEOUT, async {
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Success) => return Ok(()),
                Some(ChannelMsg::Failure | ChannelMsg::Close) | None => {
                    return Err(SessionError::Shell);
                }
                Some(ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. }) => {
                    if data.len() > (64 * 1024usize).saturating_sub(early_output.len()) {
                        return Err(SessionError::EarlyOutput);
                    }
                    early_output.extend_from_slice(&data);
                }
                Some(_) => {}
            }
        }
    })
    .await
    .map_err(|_| SessionError::Timeout)?
}

async fn load_key(path: PathBuf, events: &mpsc::Sender<Event>) -> Result<PrivateKey, SessionError> {
    let (source, decoded) = tokio::task::spawn_blocking(move || {
        let mut source = Zeroizing::new(String::new());
        File::open(path)
            .and_then(|file| file.take(KEY_LIMIT + 1).read_to_string(&mut source))
            .map_err(|_| SessionError::KeyRead)?;
        if source.len() as u64 > KEY_LIMIT {
            return Err(SessionError::KeyRead);
        }
        let decoded = keys::decode_secret_key(&source, None);
        Ok((source, decoded))
    })
    .await
    .map_err(|_| SessionError::KeyWorker)??;
    match decoded {
        Ok(key) => Ok(key),
        Err(keys::Error::KeyIsEncrypted) => {
            let passphrase = ask(events, |reply| Event::Passphrase { reply }).await?;
            tokio::task::spawn_blocking(move || {
                keys::decode_secret_key(&source, Some(&passphrase))
                    .map_err(|_| SessionError::KeyDecode)
            })
            .await
            .map_err(|_| SessionError::KeyWorker)?
        }
        Err(_) => Err(SessionError::KeyDecode),
    }
}
