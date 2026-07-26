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
```

For the Tauri UI (once initialized):

```powershell
cd apps/desktop
npm run check
npm run build
```

## Current Status

Phase 2 is complete.

Implemented:

- Rust workspace with `sync-core`, `sync-server`, and Tauri desktop members.
- Shared `ThreadBundle`, content-object, warning, and scan-report models.
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
- Axum health endpoint skeleton.
- Frontend type-check, production build, and browser visual verification.
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

Phase 2 automated write tests use temporary Codex homes. The current machine's
real Codex data has not been modified by development or verification.

Next implementation phase: server object storage, namespaces, immutable
revisions, and fast-forward push/pull. Keep server writes disabled until their
hash validation and authorization paths have automated tests.
