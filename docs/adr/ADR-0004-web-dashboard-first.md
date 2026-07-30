# ADR-0004: Web Dashboard is the Primary Product UI

Status: Accepted  
Date: 2026-07-30

## Decision

The local macOS bundle remains headless in normal operation. Setup/Repair UI is limited to pairing, permissions, repair and update failure. Daily product work happens in Web Dashboard.

## Consequences

- Agent startup cannot wait for local UI.
- Cloud query models and commands are first-class.
- Local UI must not become a second product or second configuration fact source.
