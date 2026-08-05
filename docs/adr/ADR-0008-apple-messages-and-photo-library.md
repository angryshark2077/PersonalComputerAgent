# ADR-0008: Apple Messages text and Photo Library originals

## Status

Accepted for the self-use macOS product.

## Decision

- `communication.messages` is Owner-controlled, reads every iMessage/SMS conversation, and emits text only. Initial collection covers the previous seven days. A durable local row cursor continues from the last committed source row after restart or an extended offline period.
- Apple attributed message bodies are decoded inside the signed Swift Platform Bridge. Rust owns the read-only database query, event creation, cursor, Outbox, and Cloud sync.
- `photos.library` is Owner-controlled and uses PhotoKit through the Swift Platform Bridge. It exports original images and videos together with capture time, dimensions, duration, MIME type, original filename, and album names.
- Photo collection initially covers the previous seven days and then polls for future assets. Local originals remain in the private PhotoSpool until the Cloud manifest and private R2 object are verified as completed.
- Completed PhotoSpool media files are deleted locally; a small completed manifest remains to prevent repeat export after restart. Private R2 photo objects are not included in the seven-day screenshot retention job and are retained permanently.
- Dashboard reads require an Owner session and receive only short-lived signed R2 URLs.

## Boundaries

- Swift does not access the Cloud API, business database, Outbox, or retention state.
- Apple Messages attachments are disabled; no attachment bytes or metadata are collected.
- Partial Photo Library authorization is treated as insufficient because the requested scope is the full seven-day history plus future originals.
- WeChat remains constrained to direct chats and groups of at most 15 members even though Apple Messages permits larger conversations.
