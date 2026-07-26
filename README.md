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

The dashboard scanner opens Codex databases read-only and reads only rollout
metadata instead of hashing every complete file. Snapshot creation, import, and
recovery require explicit confirmation that Codex is fully closed and a live
cross-platform process check. Snapshot creation hashes each changed rollout
while copying it once into the object store. Imports validate all SHA-256
objects before writing, reject divergent updates to an existing thread UUID,
create a database backup, and re-scan the target before marking the journal
complete.

Long-running operations run as cancellable background tasks. Scan, snapshot,
and validation stop at the next safe checkpoint. Cancelling an import switches
the task into rollback and restores the SQLite backup before it closes. Recovery
cannot be interrupted once started.

The desktop dashboard receives only a compact scan summary and up to eight
thread previews. Full SQLite records stay in the Rust core for export/import
work and are never retained in the task manager or React state.

Normal snapshots use a disposable local source index keyed by rollout path,
size, and modification time to reuse already-created immutable objects. A
missing, malformed, or stale index safely falls back to streaming and hashing
the source again. Explicit snapshot validation and every import always perform
full SHA-256 verification and never trust this index.

The default local repository is `~/.codex-session-sync`:

```text
objects/sha256/   Immutable rollout content
objects/tmp/      In-progress streamed objects
index/            Disposable trusted-local source index
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
