# Codex Session Sync

A personal, self-hosted, Git-like synchronization system for Codex
conversations. The repository contains a cross-platform Tauri desktop client,
a Rust synchronization core, and an Axum server.

Phase 2 is complete: the project can scan Codex sessions, create immutable
content-addressed local snapshots, validate their objects, and import new
threads with automatic SQLite backup, rollback, and crash-recovery journals.

## Repository layout

```text
apps/desktop/       Tauri 2 + React desktop GUI
apps/sync-server/   Personal Axum synchronization server
crates/sync-core/   Shared models and local Codex adapters
```

## Safety

The scanner opens Codex databases read-only. Snapshot creation, import, and
recovery require explicit confirmation that Codex is fully closed. Imports
validate all SHA-256 objects before writing, reject divergent updates to an
existing thread UUID, create a database backup, and re-scan the target before
marking the journal complete.

The default local repository is `~/.codex-session-sync`:

```text
objects/sha256/   Immutable rollout content
snapshots/        Snapshot manifests
backups/          Per-operation SQLite backups
journal/          Recoverable operation state
```

## Run the desktop client

```powershell
cd apps/desktop
npm install
npm run tauri -- dev
```

Scan is always read-only. Fully exit Codex before selecting the confirmation
checkbox and using snapshot, import, or recovery actions.

## Development

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo check --workspace

cd apps/desktop
npm install
npm run check
npm run build
```

See `AGENTS.md` for the locked architecture, delivery order, and compatibility
rules.
