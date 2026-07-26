# Codex Session Sync — Agent Guide

## Project Purpose

Build a personal, cross-platform desktop application that synchronizes Codex
conversation data between computers through a self-hosted server. The product
uses Git-like revisions and namespaces, but **does not** use Git to merge
Codex SQLite files.

## Locked Architecture

- Desktop UI: Tauri 2, React, TypeScript.
- Local synchronization core: Rust.
- Server: Rust and Axum.
- Server metadata database: SQLite.
- Object storage: local filesystem first; keep an adapter boundary for S3 later.
- Deployment: Docker Compose.
- Target platforms: Windows, macOS, Linux.

## Product Boundaries (v1)

- Single-user, self-hosted service.
- Codex must be fully closed before an import, export, sync, or namespace switch.
- A namespace is the synchronization unit and has a stable ID plus a renameable
  display name.
- Sync only conversation-related data; do not sync `auth.json`, API keys,
  configuration, plugins, skills, MCP configuration, logs, worktrees, or source code.
- Never upload raw API keys. Optional automatic namespace selection uses an
  HMAC fingerprint derived locally from the API key and server URL.
- Do not merge SQLite binary files. Export thread-level semantic bundles,
  merge those bundles, then import with guarded SQLite transactions.
- Different thread UUIDs can merge automatically. A divergent update to the
  same thread must become a user-resolved conflict in v1.

## Local Codex Compatibility Rules

- Discover both modern `~/.codex/sqlite/*.db` layouts and legacy
  `~/.codex/state_5.sqlite` layouts.
- Read rollout files from `sessions/**/rollout-*.jsonl` and
  `archived_sessions/**/rollout-*.jsonl`.
- Handle missing, empty, malformed, or unsupported rollout files without
  aborting the whole scan; return structured warnings instead.
- A local probe on 2026-07-23 found one zero-byte rollout among 342 active
  sessions. This is a supported skipped-file condition, not a fatal error.
- Any write must create a local backup and operation journal before changing
  session files or SQLite data.

## Delivery Order

1. Shared protocol models and read-only local scanner/exporter.
2. Tested local backup/import adapter.
3. Server object storage, namespaces, revisions, and fast-forward push/pull.
4. Tauri desktop GUI for scan, namespaces, and sync status.
5. Three-way merge and conflict UI.
6. API-key, provider, and path mappings.
7. Cross-platform packaging and compatibility test matrix.

## Development Rules

- Keep the Rust core independent of Tauri and HTTP details where possible.
- Treat server objects and revisions as immutable and SHA-256 verified.
- Use atomic filesystem replacement where possible; bridge filesystem and
  SQLite operations with a recoverable operation journal.
- Serialize every operation that can write or snapshot a normalized Codex Home
  through the desktop backend's per-home lease. The React busy state is not a
  synchronization boundary.
- Keep remote bearer tokens in the operating system credential backend. Remote
  profile JSON may contain the server URL and display metadata, but never the
  token itself.
- Treat `codex-dev.db` as a machine-local catalog: invalidate only rebuildable
  local catalog state after exact checkout. Preserve remote-host rows,
  automations, inbox data, feature state, and `thread_timeline_ledger`.
- `thread_timeline_ledger` is deliberately preserved but is not part of the
  synchronized semantic thread model in the current version.
- Prefer read-only inspection before any destructive or mutating action.
- Add unit tests for data parsing, hash validation, merge decisions, and
  unsupported-data handling before wiring UI behavior.
- Do not claim a local import is safe until backup, rollback, and validation
  paths have automated tests.

## Initial Verification Commands

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

For the Tauri UI (once initialized):

```powershell
cd apps/desktop
npm run check
npm run build
```

The native credential smoke test is ignored by default because it briefly
creates a uniquely named operating-system credential and then deletes it:

```powershell
cargo test -p codex-session-sync-desktop --lib remote_config::tests::system_credential_store_round_trip_uses_native_backend_and_cleans_up -- --ignored --exact
```

## Current Status

Phase 3B (desktop remote synchronization) is complete. The authenticated HTTP
transport, native credential storage, Push/Pull/exact namespace checkout
orchestration, durable Tracking reconciliation, and remote-sync GUI are wired.

Implemented:

- Rust workspace with `sync-core`, `sync-server`, and Tauri desktop members.
- Shared `ThreadBundle`, content-object, warning, and scan-report models.
- Shared namespace, object-query, revision, and commit protocol models with
  deterministic recursive JSON canonicalization and content-derived revision
  IDs.
- Read-only rollout discovery, fast first-record dashboard scans, full SHA-256
  export/import paths, SQLite thread metadata overlay, and structured
  skipped-file warnings.
- Tests for valid, empty, malformed JSON, and invalid UTF-8 rollouts.
- Tauri command boundary and React scan dashboard.
- Content-addressed local object storage and immutable snapshot manifests.
- Explicit closed-Codex safety confirmation for snapshot, import, and recovery.
- SHA-256 and byte-length validation before any import write.
- Per-operation SQLite online backups and atomic JSON operation journals.
- Transactional thread-row insertion and temporary-file rollout installation.
- Pre-write rejection of divergent content for an existing thread UUID.
- Automatic rollback after apply or post-import validation failure.
- Restart recovery for incomplete journals, with hash-guarded file cleanup.
- GUI actions for snapshot creation, validation, safe import, and recovery.
- Cross-platform live Codex Desktop/CLI process detection before every local
  write operation, with PID-level GUI status and no automatic termination.
- Background task manager with persisted task status, progress polling, and
  cooperative cancellation for scans, snapshots, validation, and import.
- Import cancellation reports rollback progress and restores the local backup
  before the task can finish.
- GUI scan results use a compact dashboard projection; raw SQLite records stay
  in the Rust core. Completed task results are claimed once and then released
  from the task manager to prevent completion-time memory spikes.
- Dashboard scans no longer hash complete rollout files. Snapshot creation
  streams each changed rollout once while simultaneously copying and hashing,
  eliminating the previous pre-hash pass.
- Normal snapshots maintain a disposable trusted-local source index using
  canonical path, byte length, and modification time. Unchanged sources reuse
  immutable objects without re-hashing; malformed/stale index data falls back
  to a full stream. Explicit validation and imports still hash every object.
- Automated tests covering successful import, corrupt objects, conflicts,
  transactional rollback, restart recovery, process classification, scan
  cancellation, and import cancellation.
- Server filesystem object storage with streaming SHA-256/length/size checks,
  atomic immutable installation, idempotent concurrent writes, cancellation
  cleanup, and stale temporary-file cleanup after restart.
- Canonical immutable revision storage with full hash validation on reads and
  tamper detection.
- SQLite metadata migration, namespace create/list/rename, revision metadata,
  restart persistence, and `BEGIN IMMEDIATE` fast-forward head compare-and-swap.
- Required single-user Bearer authentication with constant-time token compare;
  only health and protocol information are public.
- Axum v1 APIs for namespaces, missing-object queries, streaming object
  upload/download, namespace heads, revision reads, and revision commits using
  `expectedHead`.
- In-process API tests covering unauthorized zero-write behavior, hash/length/
  size rejection, object idempotency, missing objects, first commit,
  fast-forward, idempotent retries, concurrent stale-head rejection, hidden
  orphan revisions, and server restart persistence.
- Client tracking SQLite keyed by Codex home, remote, and namespace, with
  generation compare-and-swap and a separate active-namespace binding.
- Authenticated desktop HTTP client with redirect rejection, bounded JSON and
  error responses, streamed object transfer, and download length/hash
  verification before immutable installation.
- Remote profiles stored as token-free JSON and bearer tokens stored through
  native Windows Credential Manager, macOS Keychain, or persistent Linux
  keyring backends. An ignored native-backend smoke test verifies round-trip
  storage and cleanup.
- GUI workflows for remote profile creation/verification, namespace creation,
  selection and rename, plus Push, Pull, and exact namespace switching.
- Tauri backend leases keyed by normalized Codex Home cover direct and job
  snapshot, import, recovery, Push, Pull, and namespace-switch entry points.
  Concurrent writes to one home are rejected while different homes can run in
  parallel; RAII release covers completion, failure, and cancellation.
- Pure three-way thread-set planning that merges independent UUID changes and
  reports modify/modify and delete/modify conflicts without writing.
- Snapshot/Revision conversion with machine-local database paths removed from
  the remote semantic view.
- Streaming installation of downloaded objects into the local content store,
  with length/hash verification, cancellation cleanup, and atomic immutable
  installation.
- Exact local checkout with staged rollout directories, online backups of all
  affected thread databases, same-filesystem directory swaps, post-apply
  validation, durable journals, rollback, and restart recovery.
- Checkout journals persist the intended remote, namespace, expected Tracking
  generation, and target revision. After local apply, Tracking CAS and the
  active-namespace binding commit in one SQLite transaction before the journal
  becomes complete. Another checkout using the same repository and Home is
  blocked while a non-terminal journal exists. Restart recovery may restore a
  pre-apply state, but a `LocalApplied` journal is never automatically rolled
  back: it completes only when the live semantic hashes match and Tracking
  reconciles; otherwise recovery preserves both live data and Tracking and
  reports explicit action.
- Push atomically updates Tracking and the active namespace after a successful
  commit. If the server commit outlives the client process, a retry accepts only
  semantically identical remote thread content before repairing local Tracking.
- Exact checkout also backs up `codex-dev.db`, removes only the rebuildable
  local-host thread catalog rows, resets its full-scan watermark, and increments
  its catalog revision. Remote-host catalogs and non-rebuildable tables,
  including `thread_timeline_ledger`, remain local and untouched.
- Discovery and round-trip preservation of direct SQLite child records whose
  foreign keys reference `threads`.
- Frontend type-check, production build, and browser visual verification.
- Real loopback HTTP integration coverage, including an A Push → B checkout →
  B Push → A Pull merge flow across two temporary Codex homes.
- Original cross-platform app icon and generated Tauri platform icon set.
- Rust formatting, Clippy with warnings denied, full workspace tests, Tauri
  backend check, frontend type-check/build, server health smoke test, and a
  read-only scan against the current machine's real Codex data.

Environment note:

- Direct Cargo sparse-index connections to `index.crates.io` repeatedly timed
  out while direct HTTP requests worked. With user approval, this project uses
  a project-local `.cargo/config.toml` source replacement for
  `sparse+https://rsproxy.cn/index/`. Do not copy this setting into global Cargo
  configuration without separate approval.

Latest real-data read-only scan:

- 418 valid threads.
- 984,811,738 rollout bytes.
- 2 discovered thread databases.
- 1 skipped zero-byte rollout warning.

All server tests use temporary data directories, and all automated local-write
and remote-sync tests use temporary Codex homes. The current machine's real
Codex data has not been modified by development or verification.

Next phase: expose the existing three-way conflict model in the GUI and add
explicit user resolution for divergent changes to the same thread UUID. After
that, add optional API-key/provider/path mappings and namespace-selection
automation. Keep fingerprints local/HMAC-derived, retain manual overrides, and
do not add force-push or silent same-thread conflict resolution.
