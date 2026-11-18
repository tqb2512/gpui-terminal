use std::time::Duration;

use tokio::{io::AsyncWriteExt, time::timeout};
use tokio_util::sync::CancellationToken;

use crate::{FileError, file_transfer::LocalDownload};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[tokio::test]
async fn download_keeps_existing_destination() -> TestResult {
    let directory = tempfile::tempdir()?;
    let destination = directory.path().join("report.txt");
    let mut stage = LocalDownload::create(&destination).await?;
    stage.file.write_all(b"new").await?;
    std::fs::write(&destination, b"old")?;
    assert!(matches!(
        stage.publish(&CancellationToken::new()).await,
        Err(FileError::LocalExists)
    ));
    assert_eq!(std::fs::read(destination)?, b"old");
    assert_eq!(std::fs::read_dir(directory.path())?.count(), 1);
    Ok(())
}

#[tokio::test]
async fn transfer_cancellation_keeps_original_and_ends_promptly() -> TestResult {
    let directory = tempfile::tempdir()?;
    let destination = directory.path().join("report.txt");
    let mut stage = LocalDownload::create(&destination).await?;
    stage.file.write_all(b"partial").await?;
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        timeout(Duration::from_secs(2), stage.publish(&cancel)).await?,
        Err(FileError::AttemptEnded)
    ));
    assert!(!destination.exists());
    assert_eq!(std::fs::read_dir(directory.path())?.count(), 0);
    Ok(())
}
