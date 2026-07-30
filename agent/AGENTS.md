# Agent-specific rules

- Rust agentd owns runtime state.
- Collector and Provider code emits Events through EventSink only.
- Do not call Cloud from Collector crates.
- Blocking work must be isolated and bounded.
- Secrets are references only.
