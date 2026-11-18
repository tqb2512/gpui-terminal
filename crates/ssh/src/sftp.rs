use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use russh::{ChannelMsg, client};
use russh_sftp::client::{error::Error, rawsession::RawSftpSession};
use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const POSIX_RENAME: &str = "posix-rename@openssh.com";

#[derive(Debug, thiserror::Error)]
pub enum SftpError {
    #[error("Connect to the host before opening Files.")]
    NotConnected,
    #[error("The server did not accept the SFTP subsystem.")]
    SubsystemUnavailable,
    #[error("The SSH connection for Files has ended.")]
    Disconnected,
    #[error("The SFTP server did not respond in time.")]
    Timeout,
    #[error("The SFTP server returned an unexpected response.")]
    Protocol,
}

type OpenReply = oneshot::Sender<Result<Arc<SftpClient>, SftpError>>;

#[derive(Clone)]
pub struct SftpOpener {
    sender: mpsc::Sender<OpenReply>,
    ready: Arc<AtomicBool>,
    cancel: CancellationToken,
}

impl SftpOpener {
    pub async fn open(&self) -> Result<Arc<SftpClient>, SftpError> {
        if self.cancel.is_cancelled() || self.sender.is_closed() {
            return Err(SftpError::Disconnected);
        }
        if !self.ready.load(Ordering::Acquire) {
            return Err(SftpError::NotConnected);
        }
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err(SftpError::Disconnected),
            result = async {
                let (reply, response) = oneshot::channel();
                self.sender.send(reply).await.map_err(|_| SftpError::Disconnected)?;
                response.await.map_err(|_| SftpError::Disconnected)?
            } => result,
        }
    }
}

pub(crate) struct SftpRequests {
    receiver: mpsc::Receiver<OpenReply>,
    ready: Arc<AtomicBool>,
}

pub(crate) fn requests(cancel: CancellationToken) -> (SftpOpener, SftpRequests) {
    let (sender, receiver) = mpsc::channel(4);
    let ready = Arc::new(AtomicBool::new(false));
    (
        SftpOpener {
            sender,
            ready: ready.clone(),
            cancel,
        },
        SftpRequests { receiver, ready },
    )
}

impl SftpRequests {
    pub(crate) fn set_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }

    pub(crate) async fn serve<H: client::Handler>(mut self, connection: &client::Handle<H>) {
        let mut client = None;
        while let Some(reply) = self.receiver.recv().await {
            if reply.is_closed() {
                continue;
            }
            let result = match &client {
                Some(client) => Ok(Arc::clone(client)),
                None => SftpClient::open(connection).await.map(Arc::new),
            };
            if let Ok(opened) = &result {
                client = Some(opened.clone());
            }
            // A closed receiver means this caller no longer needs the result.
            let _ = reply.send(result);
        }
    }
}

pub struct SftpClient {
    _session: RawSftpSession,
    atomic_replace: bool,
}

impl SftpClient {
    async fn open<H: client::Handler>(connection: &client::Handle<H>) -> Result<Self, SftpError> {
        let mut channel = timeout(OPEN_TIMEOUT, connection.channel_open_session())
            .await
            .map_err(|_| SftpError::Timeout)?
            .map_err(|error| match error {
                russh::Error::ChannelOpenFailure(_) => SftpError::SubsystemUnavailable,
                _ => SftpError::Disconnected,
            })?;
        let response = timeout(OPEN_TIMEOUT, async {
            channel
                .request_subsystem(true, "sftp")
                .await
                .map_err(|_| SftpError::Disconnected)?;
            match channel.wait().await {
                Some(ChannelMsg::Success) => Ok(()),
                Some(ChannelMsg::Failure) => Err(SftpError::SubsystemUnavailable),
                Some(ChannelMsg::Close | ChannelMsg::Eof) | None => Err(SftpError::Disconnected),
                Some(_) => Err(SftpError::Protocol),
            }
        })
        .await;
        // The stream owns the channel close even when negotiation is refused.
        let stream = channel.into_stream();
        response.map_err(|_| SftpError::Timeout)??;
        let session = RawSftpSession::new(stream);
        let version = session.init().await.map_err(|error| match error {
            Error::Timeout => SftpError::Timeout,
            _ => SftpError::Protocol,
        })?;
        if version.version != 3 {
            return Err(SftpError::Protocol);
        }

        Ok(Self {
            _session: session,
            atomic_replace: version
                .extensions
                .get(POSIX_RENAME)
                .is_some_and(|v| v == "1"),
        })
    }

    pub fn supports_atomic_replace(&self) -> bool {
        self.atomic_replace
    }
}
