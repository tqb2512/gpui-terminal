//! Headless application core shared by the Windows workspace and tests.

mod connections;
mod file_transfer;

pub use connections::{
    AttemptId, ConnectionEvent, ConnectionEventKind, ConnectionSpec, Connections, InputError,
    ManagerError, Prompt, PromptId, PromptKind, PromptResponse, TabId,
};
pub use file_transfer::{AttemptFiles, FileError};

#[cfg(test)]
mod connections_test;
#[cfg(test)]
mod file_transfer_test;
