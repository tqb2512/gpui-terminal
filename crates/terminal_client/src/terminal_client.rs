//! Headless application core shared by the Windows workspace and tests.

mod connections;

pub use connections::{
    AttemptId, ConnectionEvent, ConnectionEventKind, ConnectionSpec, Connections, ManagerError,
    Prompt, PromptId, PromptKind, PromptResponse, TabId,
};

#[cfg(test)]
mod connections_test;
