#![cfg(not(windows))]

use std::{io, process::Command};

#[test]
fn reports_unsupported_platform() -> io::Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_terminal_client")).output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Windows"));
    Ok(())
}
