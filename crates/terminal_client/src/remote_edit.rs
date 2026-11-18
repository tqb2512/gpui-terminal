use std::{
    collections::HashSet,
    fs::{self, File},
    future::Future,
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use sha2::{Digest, Sha256};
use ssh::{PublishMode, RemoteKind, RemoteMetadata, RemotePath, SftpError};
use tokio::{io::AsyncWrite, runtime::Handle, sync::watch, time::sleep};
use tokio_util::sync::CancellationToken;

pub(crate) use crate::edit_file::WatchFile;
use crate::{
    AttemptFiles, AttemptId, FileError, ManagerError,
    file_transfer::{CHUNK, publish_remote},
};

const MAX_EDIT_BYTES: u64 = 32 * 1024 * 1024;
type ContentDigest = [u8; 32];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditStatus {
    Watching,
    Saving,
    Saved,
    Paused(EditPause),
    Finished,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum EditPause {
    #[error("The remote file changed. Your edits are in the local copy.")]
    Conflict,
    #[error("The SSH connection ended. Your edits are in the local copy.")]
    Disconnected,
    #[error("Could not verify or safely write the remote file. Your edits are in the local copy.")]
    Remote,
    #[error("Could not read a safe, complete local edit (maximum 32 MiB).")]
    Local,
    #[error("The server did not confirm the save. Check the remote file; the local copy was kept.")]
    OutcomeUnknown,
    #[error("Save-back stopped and a remote temporary file may not have been removed.")]
    Cleanup,
}

#[derive(Default)]
pub(crate) struct EditSlots(Mutex<HashSet<(AttemptId, RemotePath)>>);

impl EditSlots {
    pub(crate) fn reserve(
        self: &Arc<Self>,
        attempt: AttemptId,
        path: RemotePath,
    ) -> Result<EditSlot, FileError> {
        let key = (attempt, path);
        let mut slots = self.0.lock().map_err(|_| ManagerError::Unavailable)?;
        if slots.contains(&key) {
            return Err(FileError::EditAlreadyOpen);
        }
        if slots.len() >= 4 {
            return Err(FileError::EditLimit);
        }
        slots.insert(key.clone());
        Ok(EditSlot {
            slots: self.clone(),
            key,
        })
    }
}

pub(crate) struct EditSlot {
    slots: Arc<EditSlots>,
    key: (AttemptId, RemotePath),
}

impl Drop for EditSlot {
    fn drop(&mut self) {
        self.slots
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.key);
    }
}

pub(crate) struct EditCopy {
    directory: tempfile::TempDir,
    path: PathBuf,
    watched: Arc<WatchFile>,
}

impl EditCopy {
    pub(crate) fn create(root: &Path, name: &str) -> Result<Self, FileError> {
        fs::create_dir_all(root).map_err(|_| FileError::LocalWrite)?;
        if !fs::symlink_metadata(root).is_ok_and(|meta| meta.is_dir() && !meta.is_symlink()) {
            return Err(FileError::LocalWrite);
        }
        let directory = tempfile::Builder::new()
            .prefix("remote-")
            .tempdir_in(root)
            .map_err(|_| FileError::LocalWrite)?;
        let path = directory.path().join(name);
        let watched = Arc::new(WatchFile::new(&path)?);
        Ok(Self {
            directory,
            path,
            watched,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn keep(self) -> PathBuf {
        let _kept_directory = self.directory.keep();
        self.path
    }
}

#[must_use = "keep the prepared copy alive until the native picker responds"]
pub struct PreparedEdit {
    files: AttemptFiles,
    remote: RemotePath,
    version: RemoteVersion,
    copy: EditCopy,
    slot: EditSlot,
}

impl AttemptFiles {
    pub async fn prepare_edit(
        &self,
        remote: &RemotePath,
        edits_root: &Path,
    ) -> Result<PreparedEdit, FileError> {
        self.check_active()?;
        if !self.supports_atomic_replace() {
            return Err(SftpError::Unsupported.into());
        }
        let filename = edit_filename(remote.name().ok_or(FileError::EditType)?)?;
        let slot = self.edits.reserve(self.attempt(), remote.clone())?;
        let metadata = self.remote.metadata(remote).await?;
        validate_remote(&metadata)?;
        let root = edits_root.to_owned();
        let copy = tokio::task::spawn_blocking(move || EditCopy::create(&root, &filename))
            .await
            .map_err(|_| FileError::LocalWrite)??;
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(copy.path())
            .await
            .map_err(|_| FileError::LocalWrite)?;
        let mut output = DigestWriter::new(&mut file);
        self.copy_to_local(remote, &mut output, Some(MAX_EDIT_BYTES))
            .await?;
        let digest = output.finish();
        tokio::io::AsyncWriteExt::flush(&mut file)
            .await
            .map_err(|_| FileError::LocalWrite)?;
        drop(file);
        if self.remote.metadata(remote).await? != metadata {
            return Err(FileError::EditChanged);
        }
        self.check_active()?;
        Ok(PreparedEdit {
            files: self.clone(),
            remote: remote.clone(),
            version: RemoteVersion { metadata, digest },
            copy,
            slot,
        })
    }
}

impl PreparedEdit {
    pub fn is_active(&self) -> bool {
        !self.files.cancel.is_cancelled()
    }

    pub fn local_path(&self) -> &Path {
        self.copy.path()
    }

    pub fn start(self) -> ActiveEdit {
        let watched = self.copy.watched.clone();
        ActiveEdit {
            files: self.files,
            remote: self.remote,
            version: self.version,
            local: self.copy.keep(),
            watched,
            _slot: self.slot,
        }
    }
}

#[must_use = "run the edit watcher to save edits to the remote file"]
pub struct ActiveEdit {
    files: AttemptFiles,
    remote: RemotePath,
    version: RemoteVersion,
    local: PathBuf,
    watched: Arc<WatchFile>,
    _slot: EditSlot,
}

impl ActiveEdit {
    pub fn local_path(&self) -> &Path {
        &self.local
    }

    pub async fn run(
        self,
        runtime: &Handle,
        stop: CancellationToken,
        status: watch::Sender<EditStatus>,
    ) {
        let Self {
            files,
            remote,
            version,
            watched,
            _slot,
            ..
        } = self;
        run_watch(
            runtime,
            &watched,
            (version.digest, version),
            (&files.cancel, &stop),
            &status,
            || sleep(Duration::from_secs(1)),
            |mut version, snapshot| {
                let files = files.clone();
                let remote = remote.clone();
                let stop = stop.clone();
                async move {
                    save(&files, &remote, &mut version, snapshot, &stop).await?;
                    Ok(version)
                }
            },
        )
        .await;
    }
}

pub(crate) async fn run_watch<State, Tick, Upload>(
    runtime: &Handle,
    local: &Arc<WatchFile>,
    (mut saved, mut state): (ContentDigest, State),
    (disconnect, stop): (&CancellationToken, &CancellationToken),
    status: &watch::Sender<EditStatus>,
    mut tick: impl FnMut() -> Tick,
    mut upload: impl FnMut(State, Snapshot) -> Upload,
) where
    State: Send,
    Tick: Future<Output = ()> + Send,
    Upload: Future<Output = Result<State, EditPause>> + Send,
{
    let mut candidate = None;
    let end = loop {
        tokio::select! {
            biased;
            () = disconnect.cancelled() => break EditStatus::Paused(EditPause::Disconnected),
            () = stop.cancelled() => break EditStatus::Finished,
            () = tick() => {}
        }
        let watched = local.clone();
        let capture = candidate.is_some();
        let sample = runtime
            .spawn_blocking(move || sample_local(&watched, capture))
            .await;
        let sample = match sample {
            Ok(Ok(sample)) => sample,
            Ok(Err(FileError::LocalBusy)) => {
                candidate = None;
                continue;
            }
            _ => break EditStatus::Paused(EditPause::Local),
        };
        if disconnect.is_cancelled() {
            break EditStatus::Paused(EditPause::Disconnected);
        }
        if stop.is_cancelled() {
            break EditStatus::Finished;
        }
        if sample.digest == saved {
            candidate = None;
            continue;
        }
        if candidate != Some(sample.digest) {
            candidate = Some(sample.digest);
            continue;
        }
        let Some(file) = sample.snapshot else {
            break EditStatus::Paused(EditPause::Local);
        };
        status.send_replace(EditStatus::Saving);
        state = match upload(
            state,
            Snapshot {
                digest: sample.digest,
                file,
            },
        )
        .await
        {
            Ok(state) => state,
            Err(EditPause::Disconnected) if stop.is_cancelled() && !disconnect.is_cancelled() => {
                break EditStatus::Finished;
            }
            Err(reason) => break EditStatus::Paused(reason),
        };
        saved = sample.digest;
        candidate = None;
        status.send_replace(EditStatus::Saved);
    };
    status.send_replace(end);
}

pub(crate) struct LocalSample {
    pub(crate) digest: ContentDigest,
    snapshot: Option<File>,
}

pub(crate) struct Snapshot {
    digest: ContentDigest,
    pub(crate) file: File,
}

pub(crate) fn sample_local(watched: &WatchFile, snapshot: bool) -> Result<LocalSample, FileError> {
    let mut file = watched.open()?;
    let mut snapshot = snapshot
        .then(tempfile::tempfile)
        .transpose()
        .map_err(|_| FileError::LocalWrite)?;
    let mut digest = Sha256::new();
    let mut remaining = MAX_EDIT_BYTES;
    let mut buffer = [0; CHUNK];
    loop {
        let length = file.read(&mut buffer).map_err(|_| FileError::LocalRead)?;
        if length == 0 {
            break;
        }
        remaining = remaining
            .checked_sub(length as u64)
            .ok_or(FileError::EditTooLarge)?;
        let bytes = buffer.get(..length).ok_or(FileError::LocalRead)?;
        digest.update(bytes);
        if let Some(snapshot) = &mut snapshot {
            snapshot
                .write_all(bytes)
                .map_err(|_| FileError::LocalWrite)?;
        }
    }
    if let Some(snapshot) = &mut snapshot {
        snapshot.rewind().map_err(|_| FileError::LocalRead)?;
    }
    Ok(LocalSample {
        digest: digest.finalize().into(),
        snapshot,
    })
}

fn validate_remote(metadata: &RemoteMetadata) -> Result<(), FileError> {
    if metadata.kind != RemoteKind::File
        || metadata.uid.is_none()
        || metadata.gid.is_none()
        || !metadata
            .mode
            .is_some_and(|mode| mode & 0o7000 == 0 && mode & 0o222 != 0)
    {
        return Err(FileError::EditUnverifiable);
    }
    if metadata.size.is_some_and(|size| size > MAX_EDIT_BYTES) {
        return Err(FileError::EditTooLarge);
    }
    Ok(())
}

pub(crate) struct RemoteVersion {
    metadata: RemoteMetadata,
    digest: ContentDigest,
}

impl RemoteVersion {
    #[cfg(test)]
    pub(crate) fn new(metadata: RemoteMetadata, bytes: &[u8]) -> Self {
        Self {
            metadata,
            digest: Sha256::digest(bytes).into(),
        }
    }

    pub(crate) fn verify(&self, current: &Self) -> Result<(), EditPause> {
        if self.metadata != current.metadata || self.digest != current.digest {
            return Err(EditPause::Conflict);
        }
        Ok(())
    }

    async fn observe(files: &AttemptFiles, path: &RemotePath) -> Result<Self, EditPause> {
        let metadata = files.remote.metadata(path).await.map_err(remote_pause)?;
        validate_remote(&metadata).map_err(|_| EditPause::Remote)?;
        let mut output = DigestWriter::new(tokio::io::sink());
        files
            .copy_to_local(path, &mut output, Some(MAX_EDIT_BYTES))
            .await
            .map_err(file_pause)?;
        if files.remote.metadata(path).await.map_err(remote_pause)? != metadata {
            return Err(EditPause::Conflict);
        }
        Ok(Self {
            metadata,
            digest: output.finish(),
        })
    }
}

async fn save(
    files: &AttemptFiles,
    path: &RemotePath,
    version: &mut RemoteVersion,
    snapshot: Snapshot,
    stop: &CancellationToken,
) -> Result<(), EditPause> {
    files.check_active().map_err(file_pause)?;
    if stop.is_cancelled() {
        return Err(EditPause::Disconnected);
    }
    let mut stage = files.remote.stage_write(path).await.map_err(remote_pause)?;
    let result = async {
        let mut input = tokio::fs::File::from_std(snapshot.file);
        tokio::select! {
            biased;
            () = stop.cancelled() => return Err(EditPause::Disconnected),
            result = files.copy_to_remote(&mut input, &mut stage) => result.map_err(file_pause)?,
        }
        files.check_active().map_err(file_pause)?;
        stage
            .seal(Some(&version.metadata))
            .await
            .map_err(remote_pause)?;
        version.verify(&RemoteVersion::observe(files, path).await?)?;
        if stop.is_cancelled() {
            return Err(EditPause::Disconnected);
        }
        publish_remote(&mut stage, PublishMode::AtomicReplace, &files.cancel)
            .await
            .map_err(file_pause)
    }
    .await;
    if let Err(error) = result {
        if stage.abort().await.is_err() && error != EditPause::OutcomeUnknown {
            return Err(EditPause::Cleanup);
        }
        return Err(error);
    }
    let after = RemoteVersion::observe(files, path).await?;
    if after.digest != snapshot.digest {
        return Err(EditPause::Conflict);
    }
    *version = after;
    Ok(())
}

fn file_pause(error: FileError) -> EditPause {
    match error {
        FileError::AttemptEnded => EditPause::Disconnected,
        FileError::Remote(error) => remote_pause(error),
        FileError::EditTooLarge | FileError::EditUnverifiable => EditPause::Remote,
        _ => EditPause::Local,
    }
}

fn remote_pause(error: SftpError) -> EditPause {
    match error {
        SftpError::NotFound | SftpError::NotRegularFile => EditPause::Conflict,
        SftpError::Disconnected => EditPause::Disconnected,
        SftpError::OutcomeUnknown => EditPause::OutcomeUnknown,
        SftpError::CleanupFailed => EditPause::Cleanup,
        _ => EditPause::Remote,
    }
}

pub fn valid_windows_filename(name: &str) -> bool {
    if name.is_empty()
        || name.ends_with(['.', ' '])
        || name.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*'])
        || name.chars().any(char::is_control)
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    !(matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    }))
}

pub(crate) fn edit_filename(name: &str) -> Result<String, FileError> {
    if !valid_windows_filename(name) {
        return Err(FileError::EditType);
    }
    let extension = Path::new(name)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("txt")
        .to_ascii_lowercase();
    match extension.as_str() {
        "txt" | "log" | "md" | "toml" | "ini" | "cfg" | "conf" | "json" | "yaml" | "yml"
        | "xml" | "csv" | "tsv" | "pdf" | "docx" | "xlsx" | "png" | "jpg" => {
            Ok(format!("edit.{extension}"))
        }
        _ => Err(FileError::EditType),
    }
}

struct DigestWriter<W> {
    output: W,
    digest: Sha256,
}

impl<W> DigestWriter<W> {
    fn new(output: W) -> Self {
        Self {
            output,
            digest: Sha256::new(),
        }
    }

    fn finish(self) -> ContentDigest {
        self.digest.finalize().into()
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for DigestWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let length = std::task::ready!(Pin::new(&mut self.output).poll_write(cx, bytes))?;
        let bytes = bytes
            .get(..length)
            .ok_or(io::Error::other("invalid write length"))?;
        self.digest.update(bytes);
        Poll::Ready(Ok(length))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.output).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.output).poll_shutdown(cx)
    }
}
