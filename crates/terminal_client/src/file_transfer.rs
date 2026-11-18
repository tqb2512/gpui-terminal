use std::{
    fs::OpenOptions,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use ssh::{PublishMode, RemoteEntry, RemotePath, SftpClient, SftpError, StagedRemoteWrite};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
};
use tokio_util::sync::CancellationToken;

use crate::{AttemptId, ManagerError, remote_edit::EditSlots};

pub(crate) const CHUNK: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("This SSH connection attempt is no longer active.")]
    AttemptEnded,
    #[error(transparent)]
    Remote(#[from] SftpError),
    #[error(transparent)]
    Manager(#[from] ManagerError),
    #[error("The local destination already exists. Choose a different filename.")]
    LocalExists,
    #[error("Select a regular local file, not a link, folder, or device.")]
    NotRegular,
    #[error("Could not read the local file. Check that it still exists and is readable.")]
    LocalRead,
    #[error("Could not write the local destination. Check available space and permissions.")]
    LocalWrite,
    #[error("Could not clean up an unfinished local download.")]
    LocalCleanup,
    #[error("The local file is temporarily missing or in use.")]
    LocalBusy,
    #[error("This remote filename is not supported by Open with. Use Download instead.")]
    EditType,
    #[error("Open with supports files up to 32 MiB. Use Download for larger files.")]
    EditTooLarge,
    #[error("This file already has an active local copy.")]
    EditAlreadyOpen,
    #[error("Finish editing one of the four active copies before opening another.")]
    EditLimit,
    #[error("The remote file changed while the local copy was being prepared.")]
    EditChanged,
    #[error("The original file could not be verified for automatic save-back.")]
    EditUnverifiable,
}

#[derive(Clone)]
pub struct AttemptFiles {
    attempt: AttemptId,
    pub(crate) remote: Arc<SftpClient>,
    pub(crate) cancel: CancellationToken,
    pub(crate) edits: Arc<EditSlots>,
}

impl AttemptFiles {
    pub(crate) fn new(
        attempt: AttemptId,
        remote: Arc<SftpClient>,
        cancel: CancellationToken,
        edits: Arc<EditSlots>,
    ) -> Self {
        Self {
            attempt,
            remote,
            cancel,
            edits,
        }
    }

    pub fn attempt(&self) -> AttemptId {
        self.attempt
    }

    pub fn supports_atomic_replace(&self) -> bool {
        self.remote.supports_atomic_replace()
    }

    pub async fn home(&self) -> Result<RemotePath, FileError> {
        self.check_active()?;
        let path = self.remote.home().await?;
        self.check_active()?;
        Ok(path)
    }

    pub async fn list(&self, path: &RemotePath) -> Result<Vec<RemoteEntry>, FileError> {
        self.check_active()?;
        let entries = self.remote.list(path).await?;
        self.check_active()?;
        Ok(entries)
    }

    pub async fn download(&self, remote: &RemotePath, destination: &Path) -> Result<(), FileError> {
        self.check_active()?;
        let mut stage = LocalDownload::create(destination).await?;
        let result = self.copy_to_local(remote, &mut stage.file, None).await;
        if let Err(error) = result {
            stage.abort().await?;
            return Err(error);
        }
        stage.publish(&self.cancel).await
    }

    pub async fn upload(&self, source: &Path, remote: &RemotePath) -> Result<(), FileError> {
        self.check_active()?;
        if !self.remote.supports_create_only() {
            return Err(SftpError::NoSafeUpload.into());
        }
        let mut source = open_regular(source).await?;
        let mut stage = self.remote.stage_write(remote).await?;
        let result = async {
            self.copy_to_remote(&mut source, &mut stage).await?;
            self.check_active()?;
            stage.seal(None).await?;
            publish_remote(&mut stage, PublishMode::CreateOnly, &self.cancel).await
        }
        .await;
        finish_remote(stage, result).await
    }

    pub(crate) fn check_active(&self) -> Result<(), FileError> {
        check_active(&self.cancel)
    }

    pub(crate) async fn copy_to_local(
        &self,
        path: &RemotePath,
        output: &mut (impl tokio::io::AsyncWrite + Unpin),
        limit: Option<u64>,
    ) -> Result<(), FileError> {
        self.check_active()?;
        let mut reader = self.remote.open_read(path).await?;
        let result: Result<(), FileError> = async {
            let mut buffer = [0; CHUNK];
            let mut remaining = limit.unwrap_or(u64::MAX);
            loop {
                let length = while_active(&self.cancel, reader.read(&mut buffer)).await??;
                if length == 0 {
                    return Ok(());
                }
                remaining = remaining
                    .checked_sub(length as u64)
                    .ok_or(FileError::EditTooLarge)?;
                let bytes = buffer.get(..length).ok_or(SftpError::Protocol)?;
                while_active(&self.cancel, output.write_all(bytes))
                    .await?
                    .map_err(|_| FileError::LocalWrite)?;
            }
        }
        .await;
        let closed = reader.close().await;
        if closed.is_err() && !self.cancel.is_cancelled() {
            return Err(SftpError::CleanupFailed.into());
        }
        result?;
        closed?;
        Ok(())
    }

    pub(crate) async fn copy_to_remote(
        &self,
        input: &mut File,
        stage: &mut StagedRemoteWrite,
    ) -> Result<(), FileError> {
        let mut buffer = [0; CHUNK];
        loop {
            let length = while_active(&self.cancel, input.read(&mut buffer))
                .await?
                .map_err(|_| FileError::LocalRead)?;
            if length == 0 {
                return Ok(());
            }
            let bytes = buffer.get(..length).ok_or(SftpError::Protocol)?;
            while_active(&self.cancel, stage.write_all(bytes)).await??;
        }
    }
}

fn check_active(cancel: &CancellationToken) -> Result<(), FileError> {
    if cancel.is_cancelled() {
        Err(FileError::AttemptEnded)
    } else {
        Ok(())
    }
}

async fn while_active<T>(
    cancel: &CancellationToken,
    work: impl Future<Output = T>,
) -> Result<T, FileError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(FileError::AttemptEnded),
        result = work => Ok(result),
    }
}

pub(crate) async fn publish_remote(
    stage: &mut StagedRemoteWrite,
    mode: PublishMode,
    cancel: &CancellationToken,
) -> Result<(), FileError> {
    check_active(cancel)?;
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(SftpError::OutcomeUnknown.into()),
        result = stage.publish(mode) => result,
    };
    Ok(result?)
}

pub(crate) async fn finish_remote(
    stage: StagedRemoteWrite,
    result: Result<(), FileError>,
) -> Result<(), FileError> {
    match result {
        Ok(()) => Ok(()),
        Err(error) => match stage.abort().await {
            Ok(()) => Err(error),
            Err(cleanup) => {
                if matches!(error, FileError::Remote(SftpError::OutcomeUnknown)) {
                    Err(error)
                } else {
                    Err(cleanup.into())
                }
            }
        },
    }
}

pub(crate) async fn open_regular(path: &Path) -> Result<File, FileError> {
    if path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|ext| {
            ["lnk", "url", "website", "scf"]
                .iter()
                .any(|shortcut| ext.eq_ignore_ascii_case(shortcut))
        })
    {
        return Err(FileError::NotRegular);
    }
    let path = path.to_owned();
    let file = tokio::task::spawn_blocking(move || open_regular_file(&path))
        .await
        .map_err(|_| FileError::LocalRead)??;
    Ok(File::from_std(file))
}

pub(crate) fn open_regular_file(path: &Path) -> Result<std::fs::File, FileError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | 0x0200_0000);
    }
    let file = options.open(path).map_err(|error| {
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::ELOOP) {
            return FileError::NotRegular;
        }
        #[cfg(windows)]
        if matches!(error.raw_os_error(), Some(32 | 33)) {
            return FileError::LocalBusy;
        }
        if error.kind() == io::ErrorKind::NotFound {
            FileError::LocalBusy
        } else {
            FileError::LocalRead
        }
    })?;
    let metadata = file.metadata().map_err(|_| FileError::LocalRead)?;
    if !metadata.is_file() {
        return Err(FileError::NotRegular);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(FileError::NotRegular);
        }
    }
    Ok(file)
}
pub(crate) struct LocalDownload {
    pub(crate) file: File,
    stage: tempfile::TempPath,
    destination: PathBuf,
}

impl LocalDownload {
    pub(crate) async fn create(destination: &Path) -> Result<Self, FileError> {
        let destination = destination.to_owned();
        tokio::task::spawn_blocking(move || {
            let directory = destination.parent().ok_or(FileError::LocalWrite)?;
            let stage = tempfile::Builder::new()
                .prefix(".gpui-sftp-")
                .tempfile_in(directory)
                .map_err(|_| FileError::LocalWrite)?;
            let (file, stage) = stage.into_parts();
            Ok(Self {
                file: File::from_std(file),
                stage,
                destination,
            })
        })
        .await
        .map_err(|_| FileError::LocalWrite)?
    }

    pub(crate) async fn publish(mut self, cancel: &CancellationToken) -> Result<(), FileError> {
        if self.file.flush().await.is_err() {
            self.abort().await?;
            return Err(FileError::LocalWrite);
        }
        let cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            drop(self.file);
            if cancel.is_cancelled() {
                self.stage.close().map_err(|_| FileError::LocalCleanup)?;
                return Err(FileError::AttemptEnded);
            }
            if let Err(error) = self.stage.persist_noclobber(self.destination) {
                error.path.close().map_err(|_| FileError::LocalCleanup)?;
                return Err(if error.error.kind() == io::ErrorKind::AlreadyExists {
                    FileError::LocalExists
                } else {
                    FileError::LocalWrite
                });
            }
            Ok(())
        })
        .await
        .map_err(|_| FileError::LocalWrite)?
    }

    async fn abort(self) -> Result<(), FileError> {
        tokio::task::spawn_blocking(move || {
            drop(self.file);
            self.stage.close().map_err(|_| FileError::LocalCleanup)
        })
        .await
        .map_err(|_| FileError::LocalCleanup)?
    }
}
