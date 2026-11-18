//! Headless application core shared by the Windows workspace and tests.

mod connections;
mod file_transfer;
mod remote_edit;
#[cfg(test)]
mod remote_edit_test;

pub use connections::{
    AttemptId, ConnectionEvent, ConnectionEventKind, ConnectionSpec, Connections, InputError,
    ManagerError, Prompt, PromptId, PromptKind, PromptResponse, TabId,
};
pub use file_transfer::{AttemptFiles, FileError};
pub use remote_edit::{ActiveEdit, EditPause, EditStatus, PreparedEdit};

#[cfg(test)]
mod connections_test;
#[cfg(test)]
mod file_transfer_test;
