//! Local runtime ownership primitives for the macOS S1A agent.

#![forbid(unsafe_code)]

mod crash_marker;
mod heartbeat;
mod paths;
mod single_instance;
mod state;

use std::{fmt, io, path::PathBuf};

use pca_domain::{AgentStatus, BridgeStatus};

pub use crash_marker::CrashMarkerGuard;
pub use heartbeat::LocalHeartbeatWriter;
pub use paths::RuntimePaths;
pub use single_instance::SingleInstanceGuard;
pub use state::RuntimeStateMachine;

/// Errors returned by local runtime ownership primitives.
#[derive(Debug)]
pub enum RuntimeError {
    /// Another process owns the operating-system file lock.
    AlreadyRunning,
    /// An agent lifecycle transition is not permitted by the fixed state table.
    IllegalAgentTransition { from: AgentStatus, to: AgentStatus },
    /// A Bridge lifecycle transition is not permitted by the fixed state table.
    IllegalBridgeTransition {
        from: BridgeStatus,
        to: BridgeStatus,
    },
    /// A runtime path is unsafe to use for a sensitive local artifact.
    UnsafePath { path: PathBuf, reason: &'static str },
    /// A filesystem operation failed.
    Io {
        operation: &'static str,
        source: io::Error,
    },
    /// Runtime status could not be encoded as canonical JSON.
    Serialization(serde_json::Error),
}

impl RuntimeError {
    pub(crate) fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning => formatter.write_str("the agent is already running"),
            Self::IllegalAgentTransition { from, to } => {
                write!(formatter, "illegal agent transition: {from:?} -> {to:?}")
            }
            Self::IllegalBridgeTransition { from, to } => {
                write!(formatter, "illegal Bridge transition: {from:?} -> {to:?}")
            }
            Self::UnsafePath { path, reason } => {
                write!(
                    formatter,
                    "unsafe runtime path {}: {reason}",
                    path.display()
                )
            }
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::Serialization(error) => write!(formatter, "serialize runtime status: {error}"),
        }
    }
}

impl std::error::Error for RuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}
