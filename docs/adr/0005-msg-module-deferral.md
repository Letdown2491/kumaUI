# ADR-0005: The MSG protocol module waits for the second socket verb

Status: accepted (2026-09-30)

## Context

The MSG CLI's socket protocol exists only as string literals and substring
dispatch in `main.rs`: one socket verb (launcher-toggle); volume verbs act
locally via wpctl. An architecture review (2026-09-30) proposed extracting a
`msg.rs` module (request enum, accept loop, client sender, reply type) now.

## Decision

Defer the module until a second socket verb actually arrives (e.g.
`msg settings`). With one verb, the module would be a pass-through shim: its
interface would be as wide as its implementation.

## Consequences

- The substring dispatch and hand-synced verb list in `main.rs` are
  deliberate, not an oversight. Don't "fix" them before the trigger.
- When the second verb lands, all wire parsing and dispatch concentrates in
  `msg.rs`, and the CLI becomes a verb → request mapping, the same
  concentration argument that justifies ADR-0003.
