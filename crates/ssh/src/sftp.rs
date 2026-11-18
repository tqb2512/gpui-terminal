use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use russh::{ChannelMsg, client};
use russh_sftp::{
    client::{error::Error, rawsession::RawSftpSession},
    protocol::{FileAttributes, FileType, OpenFlags, Packet, StatusCode},
};
use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const POSIX_RENAME: &str = "posix-rename@openssh.com";
const CHUNK: usize = 16 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 10_000;
const MAX_PATH_BYTES: usize = 4096;

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
    #[error("The remote path or filename is not supported.")]
    InvalidPath,
    #[error("The remote file or folder no longer exists.")]
    NotFound,
    #[error("The remote destination already exists. Choose a different filename.")]
    AlreadyExists,
    #[error("The server denied access to the remote path.")]
    PermissionDenied,
    #[error("The server rejected the file operation. Check permissions and free space.")]
    Rejected,
    #[error("The server does not support this file operation.")]
    Unsupported,
    #[error("Only regular remote files are supported.")]
    NotRegularFile,
    #[error("Only regular remote directories can be browsed.")]
    NotDirectory,
    #[error("This folder has too many entries to display (maximum 10,000).")]
    DirectoryTooLarge,
    #[error("Could not preserve the remote file's permissions and ownership.")]
    PreserveMetadata,
    #[error("The server did not confirm the write. Check the remote file before retrying.")]
    OutcomeUnknown,
    #[error("A remote temporary file or handle may not have been cleaned up.")]
    CleanupFailed,
    #[error("The staged file is not ready to publish.")]
    Unsealed,
    #[error("The staged file has already been published.")]
    Published,
}

impl From<Error> for SftpError {
    fn from(error: Error) -> Self {
        match error {
            Error::Status(status) => match status.status_code {
                StatusCode::NoSuchFile => Self::NotFound,
                StatusCode::PermissionDenied => Self::PermissionDenied,
                StatusCode::Failure => Self::Rejected,
                StatusCode::NoConnection | StatusCode::ConnectionLost => Self::Disconnected,
                StatusCode::OpUnsupported => Self::Unsupported,
                _ => Self::Protocol,
            },
            Error::Timeout => Self::Timeout,
            Error::IO(_) => Self::Disconnected,
            _ => Self::Protocol,
        }
    }
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
    session: Arc<RawSftpSession>,
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
        let version = session.init().await?;
        if version.version != 3 {
            return Err(SftpError::Protocol);
        }

        Ok(Self {
            session: Arc::new(session),
            atomic_replace: version
                .extensions
                .get(POSIX_RENAME)
                .is_some_and(|v| v == "1"),
        })
    }

    pub fn supports_atomic_replace(&self) -> bool {
        self.atomic_replace
    }

    pub async fn home(&self) -> Result<RemotePath, SftpError> {
        let names = self.session.realpath(".").await?.files;
        match names.as_slice() {
            [entry] => RemotePath::absolute(&entry.filename),
            _ => Err(SftpError::Protocol),
        }
    }

    pub async fn metadata(&self, path: &RemotePath) -> Result<RemoteMetadata, SftpError> {
        Ok(self.session.lstat(path.as_str()).await?.attrs.into())
    }

    pub async fn list(&self, path: &RemotePath) -> Result<Vec<RemoteEntry>, SftpError> {
        if self.metadata(path).await?.kind != RemoteKind::Directory {
            return Err(SftpError::NotDirectory);
        }
        let handle = self.session.opendir(path.as_str()).await?.handle;
        let result = self.read_directory(path, &handle).await;
        let closed = self.session.close(handle).await;
        if closed.is_err() {
            return Err(SftpError::CleanupFailed);
        }
        result
    }

    async fn read_directory(
        &self,
        path: &RemotePath,
        handle: &str,
    ) -> Result<Vec<RemoteEntry>, SftpError> {
        let mut entries = Vec::new();
        let mut count = 0;
        loop {
            let batch = match self.session.readdir(handle).await {
                Ok(batch) if !batch.files.is_empty() => batch.files,
                Ok(_) => return Err(SftpError::Protocol),
                Err(Error::Status(status)) if status.status_code == StatusCode::Eof => {
                    return Ok(entries);
                }
                Err(error) => return Err(error.into()),
            };
            for entry in batch {
                count += 1;
                if count > MAX_DIRECTORY_ENTRIES {
                    return Err(SftpError::DirectoryTooLarge);
                }
                if path.child(&entry.filename).is_ok() {
                    entries.push(RemoteEntry {
                        name: entry.filename,
                        kind: RemoteKind::from(&entry.attrs),
                        size: entry.attrs.size,
                    });
                }
            }
        }
    }

    pub async fn open_read(&self, path: &RemotePath) -> Result<RemoteReader, SftpError> {
        if self.metadata(path).await?.kind != RemoteKind::File {
            return Err(SftpError::NotRegularFile);
        }
        let handle = self
            .session
            .open(path.as_str(), OpenFlags::READ, FileAttributes::default())
            .await?
            .handle;
        let attrs = self.session.fstat(&handle).await;
        let error = match attrs {
            Ok(attrs) if RemoteKind::from(&attrs.attrs) == RemoteKind::File => {
                return Ok(RemoteReader {
                    session: self.session.clone(),
                    handle,
                    offset: 0,
                });
            }
            Ok(_) => SftpError::NotRegularFile,
            Err(error) => error.into(),
        };
        if self.session.close(handle).await.is_err() {
            return Err(SftpError::CleanupFailed);
        }
        Err(error)
    }

    pub async fn stage_write(&self, path: &RemotePath) -> Result<StagedRemoteWrite, SftpError> {
        let parent = path.parent().ok_or(SftpError::InvalidPath)?;
        if self.metadata(&parent).await?.kind != RemoteKind::Directory {
            return Err(SftpError::NotDirectory);
        }
        let temporary = parent.child(&format!(".gpui-sftp-{:032x}.tmp", rand::random::<u128>()))?;
        let result = self
            .session
            .open(
                temporary.as_str(),
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
                FileAttributes {
                    permissions: Some(0o600),
                    ..Default::default()
                },
            )
            .await;
        let handle = match result {
            Ok(handle) => handle.handle,
            Err(error @ Error::Status(_)) => return Err(error.into()),
            Err(_) => return Err(SftpError::CleanupFailed),
        };
        Ok(StagedRemoteWrite {
            session: self.session.clone(),
            handle: Some(handle),
            offset: 0,
            temporary,
            destination: path.clone(),
            atomic_replace: self.atomic_replace,
            state: WriteState::Writing,
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RemotePath(String);

impl RemotePath {
    pub fn absolute(path: &str) -> Result<Self, SftpError> {
        let rest = path.strip_prefix('/').ok_or(SftpError::InvalidPath)?;
        if path.len() > MAX_PATH_BYTES
            || (!rest.is_empty() && !rest.split('/').all(valid_component))
        {
            return Err(SftpError::InvalidPath);
        }
        Ok(Self(path.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn child(&self, name: &str) -> Result<Self, SftpError> {
        let prefix = self.0.trim_end_matches('/');
        if !valid_component(name) || prefix.len() + 1 + name.len() > MAX_PATH_BYTES {
            return Err(SftpError::InvalidPath);
        }
        Ok(Self(format!("{prefix}/{name}")))
    }

    pub fn parent(&self) -> Option<Self> {
        if self.0 == "/" {
            return None;
        }
        let (parent, _) = self.0.rsplit_once('/')?;
        Some(Self(
            if parent.is_empty() { "/" } else { parent }.to_owned(),
        ))
    }

    pub fn name(&self) -> Option<&str> {
        let (_, name) = self.0.rsplit_once('/')?;
        (!name.is_empty()).then_some(name)
    }
}

fn valid_component(name: &str) -> bool {
    !matches!(name, "" | "." | "..")
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteKind {
    Directory,
    File,
    Symlink,
    Other,
}

impl From<&FileAttributes> for RemoteKind {
    fn from(attrs: &FileAttributes) -> Self {
        match attrs.file_type() {
            FileType::Dir => Self::Directory,
            FileType::File => Self::File,
            FileType::Symlink => Self::Symlink,
            FileType::Other => Self::Other,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteEntry {
    pub name: String,
    pub kind: RemoteKind,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteMetadata {
    pub kind: RemoteKind,
    pub size: Option<u64>,
    pub mtime: Option<u32>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
}

impl From<FileAttributes> for RemoteMetadata {
    fn from(attrs: FileAttributes) -> Self {
        Self {
            kind: RemoteKind::from(&attrs),
            size: attrs.size,
            mtime: attrs.mtime,
            mode: attrs.permissions.map(|mode| mode & 0o7777),
            uid: attrs.uid,
            gid: attrs.gid,
        }
    }
}

#[must_use = "close the remote reader when finished"]
pub struct RemoteReader {
    session: Arc<RawSftpSession>,
    handle: String,
    offset: u64,
}

impl RemoteReader {
    pub async fn read(&mut self, buffer: &mut [u8]) -> Result<usize, SftpError> {
        let length = buffer.len().min(CHUNK);
        if length == 0 {
            return Ok(0);
        }
        let result = self
            .session
            .read(&self.handle, self.offset, length as u32)
            .await;
        let data = match result {
            Ok(data) if !data.data.is_empty() && data.data.len() <= length => data.data,
            Ok(_) => return Err(SftpError::Protocol),
            Err(Error::Status(status)) if status.status_code == StatusCode::Eof => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        let size = data.len();
        self.offset = self
            .offset
            .checked_add(size as u64)
            .ok_or(SftpError::Protocol)?;
        buffer
            .get_mut(..size)
            .ok_or(SftpError::Protocol)?
            .copy_from_slice(&data);
        Ok(size)
    }

    pub async fn close(self) -> Result<(), SftpError> {
        self.session.close(self.handle).await?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishMode {
    CreateOnly,
    AtomicReplace,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum WriteState {
    Writing,
    Sealed,
    Unknown,
    Published,
}

#[must_use = "publish or abort the staged remote write"]
pub struct StagedRemoteWrite {
    session: Arc<RawSftpSession>,
    handle: Option<String>,
    offset: u64,
    temporary: RemotePath,
    destination: RemotePath,
    atomic_replace: bool,
    state: WriteState,
}

impl StagedRemoteWrite {
    pub async fn write_all(&mut self, bytes: &[u8]) -> Result<(), SftpError> {
        let handle = self.handle.as_ref().ok_or(SftpError::Unsealed)?;
        for chunk in bytes.chunks(CHUNK) {
            let end = self
                .offset
                .checked_add(chunk.len() as u64)
                .ok_or(SftpError::Protocol)?;
            self.session
                .write(handle, self.offset, chunk.to_vec())
                .await?;
            self.offset = end;
        }
        Ok(())
    }

    pub async fn seal(&mut self, preserve: Option<&RemoteMetadata>) -> Result<(), SftpError> {
        let handle = self.handle.as_ref().ok_or(SftpError::Unsealed)?;
        if let Some(expected) = preserve {
            let (Some(mode), Some(uid), Some(gid)) = (expected.mode, expected.uid, expected.gid)
            else {
                return Err(SftpError::PreserveMetadata);
            };
            let actual: RemoteMetadata = self.session.fstat(handle).await?.attrs.into();
            if (actual.uid, actual.gid) != (Some(uid), Some(gid)) {
                self.session
                    .fsetstat(
                        handle,
                        FileAttributes {
                            uid: Some(uid),
                            gid: Some(gid),
                            ..Default::default()
                        },
                    )
                    .await?;
            }
            if actual.mode != Some(mode) || (actual.uid, actual.gid) != (Some(uid), Some(gid)) {
                self.session
                    .fsetstat(
                        handle,
                        FileAttributes {
                            permissions: Some(mode),
                            ..Default::default()
                        },
                    )
                    .await?;
            }
            let after: RemoteMetadata = self.session.fstat(handle).await?.attrs.into();
            if (after.mode, after.uid, after.gid) != (Some(mode), Some(uid), Some(gid)) {
                return Err(SftpError::PreserveMetadata);
            }
        }
        let handle = self.handle.take().ok_or(SftpError::Unsealed)?;
        self.session.close(handle).await?;
        self.state = WriteState::Sealed;
        Ok(())
    }

    pub async fn publish(&mut self, mode: PublishMode) -> Result<(), SftpError> {
        match self.state {
            WriteState::Writing => return Err(SftpError::Unsealed),
            WriteState::Unknown => return Err(SftpError::OutcomeUnknown),
            WriteState::Published => return Err(SftpError::Published),
            WriteState::Sealed => {}
        }
        if mode == PublishMode::AtomicReplace && !self.atomic_replace {
            return Err(SftpError::Unsupported);
        }
        let old = self.temporary.as_str();
        let new = self.destination.as_str();
        // If the future is stopped mid-request, never attempt to publish the same stage again.
        self.state = WriteState::Unknown;
        let result = match mode {
            PublishMode::CreateOnly => self.session.rename(old, new).await,
            PublishMode::AtomicReplace => {
                let mut request = Vec::with_capacity(8 + old.len() + new.len());
                for path in [old, new] {
                    request.extend_from_slice(&(path.len() as u32).to_be_bytes());
                    request.extend_from_slice(path.as_bytes());
                }
                match self.session.extended(POSIX_RENAME, request).await {
                    Ok(Packet::Status(status)) if status.status_code == StatusCode::Ok => {
                        Ok(status)
                    }
                    Ok(Packet::Status(status)) => Err(status.into()),
                    Ok(_) => Err(Error::UnexpectedPacket),
                    Err(error) => Err(error),
                }
            }
        };
        match result {
            Ok(_) => {
                self.state = WriteState::Published;
                Ok(())
            }
            Err(Error::Status(status))
                if matches!(
                    status.status_code,
                    StatusCode::NoSuchFile
                        | StatusCode::PermissionDenied
                        | StatusCode::Failure
                        | StatusCode::OpUnsupported
                ) =>
            {
                self.state = WriteState::Sealed;
                // SFTP v3 has no separate status code for an existing destination.
                if mode == PublishMode::CreateOnly
                    && status.status_code == StatusCode::Failure
                    && self.session.lstat(new).await.is_ok()
                {
                    return Err(SftpError::AlreadyExists);
                }
                Err(Error::Status(status).into())
            }
            Err(_) => Err(SftpError::OutcomeUnknown),
        }
    }

    pub async fn abort(self) -> Result<(), SftpError> {
        if self.state == WriteState::Published {
            return Ok(());
        }
        let closed = match self.handle {
            Some(handle) => self.session.close(handle).await.is_ok(),
            None => true,
        };
        let removed = match self.session.remove(self.temporary.as_str()).await {
            Ok(_) => true,
            Err(Error::Status(status)) if status.status_code == StatusCode::NoSuchFile => true,
            Err(_) => false,
        };
        if closed && removed {
            Ok(())
        } else {
            Err(SftpError::CleanupFailed)
        }
    }
}
