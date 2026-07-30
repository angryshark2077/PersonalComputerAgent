use pca_domain::{AgentStatus, BridgeStatus};

use crate::RuntimeError;

/// The canonical local lifecycle state, transitioned only through fixed edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeStateMachine {
    agent_status: AgentStatus,
    bridge_status: BridgeStatus,
}

impl RuntimeStateMachine {
    /// Starts before local health is established and before the Bridge connects.
    #[must_use]
    pub const fn starting() -> Self {
        Self {
            agent_status: AgentStatus::Initializing,
            bridge_status: BridgeStatus::Disconnected,
        }
    }

    #[must_use]
    pub const fn agent_status(self) -> AgentStatus {
        self.agent_status
    }

    #[must_use]
    pub const fn bridge_status(self) -> BridgeStatus {
        self.bridge_status
    }

    /// Advances the agent lifecycle through its explicit transition table.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested transition is not a legal lifecycle edge.
    pub fn transition_agent(&mut self, next: AgentStatus) -> Result<(), RuntimeError> {
        if agent_transition_is_legal(self.agent_status, next) {
            self.agent_status = next;
            Ok(())
        } else {
            Err(RuntimeError::IllegalAgentTransition {
                from: self.agent_status,
                to: next,
            })
        }
    }

    /// Advances the Bridge lifecycle through its explicit transition table.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested transition is not a legal lifecycle edge.
    pub fn transition_bridge(&mut self, next: BridgeStatus) -> Result<(), RuntimeError> {
        if bridge_transition_is_legal(self.bridge_status, next) {
            self.bridge_status = next;
            Ok(())
        } else {
            Err(RuntimeError::IllegalBridgeTransition {
                from: self.bridge_status,
                to: next,
            })
        }
    }
}

const fn agent_transition_is_legal(from: AgentStatus, to: AgentStatus) -> bool {
    matches!(
        (from, to),
        (
            AgentStatus::Initializing,
            AgentStatus::Unpaired
                | AgentStatus::Degraded
                | AgentStatus::Repair
                | AgentStatus::Stopped,
        ) | (
            AgentStatus::Unpaired,
            AgentStatus::WaitingPermission
                | AgentStatus::Running
                | AgentStatus::Updating
                | AgentStatus::Repair
                | AgentStatus::Stopped,
        ) | (
            AgentStatus::WaitingPermission,
            AgentStatus::Unpaired
                | AgentStatus::Running
                | AgentStatus::Degraded
                | AgentStatus::Updating
                | AgentStatus::Repair
                | AgentStatus::Stopped,
        ) | (
            AgentStatus::Running,
            AgentStatus::WaitingPermission
                | AgentStatus::Degraded
                | AgentStatus::Sleeping
                | AgentStatus::Updating
                | AgentStatus::Repair
                | AgentStatus::Stopped,
        ) | (
            AgentStatus::Degraded,
            AgentStatus::Unpaired
                | AgentStatus::WaitingPermission
                | AgentStatus::Running
                | AgentStatus::Sleeping
                | AgentStatus::Updating
                | AgentStatus::Repair
                | AgentStatus::Stopped,
        ) | (
            AgentStatus::Sleeping,
            AgentStatus::Unpaired
                | AgentStatus::Running
                | AgentStatus::Degraded
                | AgentStatus::Stopped,
        ) | (
            AgentStatus::Updating,
            AgentStatus::Initializing | AgentStatus::Repair | AgentStatus::Stopped,
        ) | (
            AgentStatus::Repair,
            AgentStatus::Initializing | AgentStatus::Stopped
        )
    )
}

const fn bridge_transition_is_legal(from: BridgeStatus, to: BridgeStatus) -> bool {
    matches!(
        (from, to),
        (
            BridgeStatus::Disconnected,
            BridgeStatus::Handshaking | BridgeStatus::Stopped
        ) | (
            BridgeStatus::Handshaking,
            BridgeStatus::Ready
                | BridgeStatus::Degraded
                | BridgeStatus::Incompatible
                | BridgeStatus::Disconnected
                | BridgeStatus::Stopped,
        ) | (
            BridgeStatus::Ready,
            BridgeStatus::Degraded | BridgeStatus::Disconnected | BridgeStatus::Stopped,
        ) | (
            BridgeStatus::Degraded | BridgeStatus::Incompatible,
            BridgeStatus::Handshaking | BridgeStatus::Disconnected | BridgeStatus::Stopped,
        )
    )
}
