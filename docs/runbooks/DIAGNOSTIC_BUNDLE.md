# Diagnostic Bundle

Included by default:

- app/agent/bridge versions
- OS/arch
- protocol version
- permission snapshot
- collector/provider health
- migration history
- sanitized structured logs
- crash marker
- outbox counts
- error codes

Excluded by default:

- screenshots
- message body
- WeChat KeyMaterial
- device token
- Bridge secret
- Cookie/password/form data
- full absolute user paths
- raw database copies

The user must explicitly initiate export. Redaction tests are required.
