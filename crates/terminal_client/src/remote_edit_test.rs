use std::{fs, io, path::Path, sync::Arc, time::Duration};

use ssh::{AuthMethod, RemoteKind, RemoteMetadata, RemotePath, Target};
use tokio::{sync::watch, time::timeout};
use tokio_util::sync::CancellationToken;

use crate::{
    ConnectionSpec, EditPause, EditStatus, FileError,
    connections::Core,
    remote_edit::{
        EditCopy, EditSlots, RemoteVersion, WatchFile, edit_filename, run_watch, sample_local,
    },
};

#[path = "remote_edit_test_server.rs"]
mod server;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn replace(source: &Path, contents: &[u8]) -> io::Result<()> {
    let replacement = source.with_extension("next");
    fs::write(&replacement, contents)?;
    fs::rename(replacement, source)
}

#[tokio::test]
async fn edit_tracks_replace_and_save_during_upload() -> TestResult {
    let runtime = tokio::runtime::Handle::current();
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("edit.txt");
    fs::write(&path, b"a")?;
    let watched = Arc::new(WatchFile::new(&path)?);
    let baseline = sample_local(&watched, false)?.digest;
    replace(&path, b"b")?;
    let stop = CancellationToken::new();
    let (status, _) = watch::channel(EditStatus::Watching);
    let mut uploads = Vec::new();
    let mut ticks = 0;
    timeout(
        Duration::from_secs(3),
        run_watch(
            &runtime,
            &watched,
            (baseline, ()),
            (&CancellationToken::new(), &stop),
            &status,
            || {
                ticks += 1;
                if ticks == 7 {
                    stop.cancel();
                }
                std::future::ready(())
            },
            |(), mut snapshot| {
                std::future::ready((|| {
                    use std::io::Read as _;
                    let mut bytes = Vec::new();
                    if uploads.is_empty() {
                        replace(&path, b"c").map_err(|_| EditPause::Local)?;
                    }
                    snapshot
                        .file
                        .read_to_end(&mut bytes)
                        .map_err(|_| EditPause::Local)?;
                    uploads.push(bytes);
                    Ok(())
                })())
            },
        ),
    )
    .await?;
    assert_eq!(uploads, [b"b".to_vec(), b"c".to_vec()]);
    assert_eq!(*status.borrow(), EditStatus::Finished);
    Ok(())
}

#[test]
fn edit_refuses_remote_same_size_same_mtime_change() -> TestResult {
    let metadata = RemoteMetadata {
        kind: RemoteKind::File,
        size: Some(1),
        mtime: Some(1),
        mode: Some(0o644),
        uid: Some(1000),
        gid: Some(1000),
    };
    let directory = tempfile::tempdir()?;
    let local = directory.path().join("edit.txt");
    fs::write(&local, b"b")?;
    let original = RemoteVersion::new(metadata.clone(), b"a");
    let changed = RemoteVersion::new(metadata, b"z");
    assert_eq!(original.verify(&changed), Err(EditPause::Conflict));
    assert_eq!(fs::read(local)?, b"b");
    Ok(())
}

#[test]
fn edit_limits_duplicate_and_parallel_copies() -> TestResult {
    let mut core = Core::new(hosts::Repository::open(Path::new(":memory:"))?);
    let attempt = core
        .insert_tab(ConnectionSpec {
            label: "fixture".into(),
            target: Target::parse("ops@example.test")?,
            auth: AuthMethod::Password,
            profile_id: None,
        })?
        .id;
    let slots = Arc::new(EditSlots::default());
    let first_path = RemotePath::absolute("/edit-0.txt")?;
    let first = slots.reserve(attempt, first_path.clone())?;
    assert!(matches!(
        slots.reserve(attempt, first_path.clone()),
        Err(FileError::EditAlreadyOpen)
    ));
    let remaining = (1..4)
        .map(|index| {
            slots.reserve(
                attempt,
                RemotePath::absolute(&format!("/edit-{index}.txt"))?,
            )
        })
        .collect::<Result<Vec<_>, FileError>>()?;
    let fifth = RemotePath::absolute("/fifth.txt")?;
    assert!(matches!(
        slots.reserve(attempt, fifth.clone()),
        Err(FileError::EditLimit)
    ));
    drop(first);
    let _fifth = slots.reserve(attempt, fifth)?;
    assert_eq!(remaining.len(), 3);
    Ok(())
}

#[test]
fn edit_discards_unlaunched_copy_but_keeps_launched_copy() -> TestResult {
    let directory = tempfile::tempdir()?;
    let name = edit_filename("Report.TXT")?;
    let unopened = EditCopy::create(directory.path(), &name)?;
    let path = unopened.path().to_owned();
    fs::write(&path, b"abandoned")?;
    drop(unopened);
    assert!(!path.exists());

    let launched = EditCopy::create(directory.path(), &name)?;
    let path = launched.path().to_owned();
    fs::write(&path, b"local edit")?;
    assert_eq!(launched.keep(), path);
    assert_eq!(fs::read(path)?, b"local edit");
    Ok(())
}

#[tokio::test]
async fn edit_loopback_preserves_both_copies_on_a_content_conflict() -> TestResult {
    let host = server::EditServer::start().await?;
    let directory = tempfile::tempdir()?;
    let files = host.connect(directory.path()).await?;
    let path = RemotePath::absolute("/home/test/edit.txt")?;
    let prepared = files.attempt.prepare_edit(&path, directory.path()).await?;
    let local = prepared.local_path().to_owned();
    let edit = prepared.start();
    host.change_remote(b"z")?;
    replace(&local, b"b")?;
    let (status, result) = watch::channel(EditStatus::Watching);
    timeout(
        Duration::from_secs(9),
        edit.run(
            &tokio::runtime::Handle::current(),
            CancellationToken::new(),
            status,
        ),
    )
    .await?;
    assert_eq!(*result.borrow(), EditStatus::Paused(EditPause::Conflict));
    assert_eq!(host.remote()?, b"z");
    assert_eq!(fs::read(local)?, b"b");
    assert_eq!(host.publish_count(), 0);
    Ok(())
}

#[tokio::test]
async fn edit_loopback_disconnect_after_publish_is_unknown_and_not_replayed() -> TestResult {
    let host = server::EditServer::start().await?;
    let directory = tempfile::tempdir()?;
    let mut files = host.connect(directory.path()).await?;
    let path = RemotePath::absolute("/home/test/edit.txt")?;
    let prepared = files.attempt.prepare_edit(&path, directory.path()).await?;
    let local = prepared.local_path().to_owned();
    replace(&local, b"b")?;
    let (status, result) = watch::channel(EditStatus::Watching);
    host.stall_publish();
    let runtime = tokio::runtime::Handle::current();
    let edit = prepared.start();
    let worker = tokio::spawn(async move {
        edit.run(&runtime, CancellationToken::new(), status).await;
    });
    timeout(Duration::from_secs(9), host.publish_received()).await??;
    assert_eq!(host.remote()?, b"b");
    files.reconnect().await?;
    timeout(Duration::from_secs(9), worker).await??;
    assert_eq!(
        *result.borrow(),
        EditStatus::Paused(EditPause::OutcomeUnknown)
    );
    assert_eq!(host.publish_count(), 1);
    assert_eq!(fs::read(local)?, b"b");
    assert_ne!(files.attempt.attempt(), files.original_attempt);
    Ok(())
}

#[test]
fn edit_rejects_unsafe_local_extension() -> TestResult {
    for name in [
        "launch.lnk",
        "program.exe",
        "open.url",
        "run.ps1",
        "note.txt:stream",
        "note.txt.",
        "note.txt ",
        "a/b.txt",
        "a\\b.txt",
        "NUL.txt",
    ] {
        assert!(
            matches!(edit_filename(name), Err(FileError::EditType)),
            "{name}"
        );
    }
    assert_eq!(edit_filename("service.TOML")?, "edit.toml");
    assert_eq!(edit_filename(".bashrc")?, "edit.txt");
    Ok(())
}

#[tokio::test]
async fn edit_stops_on_disconnect_without_replaying() -> TestResult {
    let runtime = tokio::runtime::Handle::current();
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("edit.txt");
    fs::write(&path, b"a")?;
    let watched = Arc::new(WatchFile::new(&path)?);
    let baseline = sample_local(&watched, false)?.digest;
    replace(&path, b"b")?;
    let disconnect = CancellationToken::new();
    let (status, _) = watch::channel(EditStatus::Watching);
    let mut ticks = 0;
    let mut uploads = 0;
    timeout(
        Duration::from_secs(3),
        run_watch(
            &runtime,
            &watched,
            (baseline, ()),
            (&disconnect, &CancellationToken::new()),
            &status,
            || {
                ticks += 1;
                if ticks == 2 {
                    disconnect.cancel();
                }
                std::future::ready(())
            },
            |(), _| {
                uploads += 1;
                std::future::ready(Ok(()))
            },
        ),
    )
    .await?;
    assert_eq!(uploads, 0);
    assert_eq!(
        *status.borrow(),
        EditStatus::Paused(EditPause::Disconnected)
    );
    assert_eq!(fs::read(path)?, b"b");
    Ok(())
}

#[cfg(any(unix, windows))]
#[test]
fn edit_rejects_local_reparse_point() -> TestResult {
    let directory = tempfile::tempdir()?;
    let target = directory.path().join("target");
    let link = directory.path().join("edit.txt");
    fs::write(&target, b"must not read")?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &link)?;
    #[cfg(windows)]
    if let Err(error) = std::os::windows::fs::symlink_file(target, &link) {
        // Windows requires Developer Mode or elevated symlink privileges.
        if error.raw_os_error() == Some(1314) {
            return Ok(());
        }
        return Err(error.into());
    }
    assert!(sample_local(&WatchFile::new(&link)?, false).is_err());
    Ok(())
}

#[cfg(any(unix, windows))]
#[test]
fn edit_does_not_follow_a_replaced_parent_directory() -> TestResult {
    let directory = tempfile::tempdir()?;
    let original = directory.path().join("original");
    let other = directory.path().join("other");
    fs::create_dir(&original)?;
    fs::create_dir(&other)?;
    fs::write(original.join("edit.txt"), b"local copy")?;
    fs::write(other.join("edit.txt"), b"unrelated data")?;
    let watched = WatchFile::new(&original.join("edit.txt"))?;
    let expected = sample_local(&watched, false)?.digest;
    fs::rename(&original, directory.path().join("moved"))?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(&other, &original)?;
    #[cfg(windows)]
    if let Err(error) = std::os::windows::fs::symlink_dir(&other, &original) {
        if error.raw_os_error() == Some(1314) {
            return Ok(());
        }
        return Err(error.into());
    }
    assert!(sample_local(&watched, false).is_ok_and(|sample| sample.digest == expected));
    Ok(())
}

#[cfg(windows)]
#[test]
fn edit_skips_partial_saves_while_a_writer_keeps_the_file_open() -> TestResult {
    use std::io::Write as _;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("edit.txt");
    fs::write(&path, b"original")?;
    let watched = WatchFile::new(&path)?;
    let mut writer = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)?;
    writer.write_all(b"partial")?;
    for _ in 0..2 {
        assert!(matches!(
            sample_local(&watched, false),
            Err(FileError::LocalBusy)
        ));
    }
    writer.write_all(b" complete")?;
    drop(writer);
    assert!(sample_local(&watched, true).is_ok());
    Ok(())
}
