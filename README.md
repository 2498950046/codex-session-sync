# Codex Session Sync

A personal, self-hosted, Git-like synchronization system for Codex
conversations. The repository contains a cross-platform Tauri desktop client,
a Rust synchronization core, and an Axum server.

Phase 3A is complete: in addition to safe local snapshots and imports, the
server now provides authenticated namespaces, immutable revisions,
content-addressed object transfer, and atomic fast-forward head updates. The
desktop client does not connect to these remote APIs yet.

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

The server keeps large rollout content outside SQLite, validates every upload
while streaming it to disk, and atomically installs only objects whose byte
length and SHA-256 match the request. Revision manifests use deterministic JSON
and content-derived IDs. Namespace heads update through an immediate SQLite
transaction, so concurrent pushes cannot silently overwrite one another. All
conversation-data endpoints require a Bearer token; health and protocol info
are the only public endpoints.

The default local repository is `~/.codex-session-sync`:

```text
objects/sha256/   Immutable rollout content
objects/tmp/      In-progress streamed objects
index/            Disposable trusted-local source index
snapshots/        Snapshot manifests
backups/          Per-operation SQLite backups
journal/          Recoverable operation state
```

The server data directory contains:

```text
metadata.sqlite    Namespaces, immutable revision metadata, and heads
objects/sha256/    Immutable rollout objects
objects/tmp/       In-progress uploads, cleaned after cancellation/restart
revisions/sha256/  Canonical immutable revision manifests
revisions/tmp/     In-progress revision writes
```

## Run the desktop client

```powershell
cd apps/desktop
npm install
npm run tauri -- dev
```

Scan is always read-only. Fully exit Codex before selecting the confirmation
checkbox and using snapshot, import, or recovery actions.

## Run the synchronization server

`SYNC_SERVER_TOKEN` is required. Use a long random value and place the server
behind HTTPS before exposing it outside a trusted network.

```powershell
$env:SYNC_SERVER_TOKEN = "replace-with-a-long-random-token"
$env:SYNC_SERVER_DATA_DIR = "D:\codex-session-sync-data"
cargo run -p sync-server
```

Optional settings:

- `SYNC_SERVER_BIND` defaults to `127.0.0.1:8787`.
- `SYNC_SERVER_MAX_OBJECT_BYTES` defaults to 4 GiB.
- `SYNC_SERVER_MAX_MANIFEST_BYTES` defaults to 64 MiB.

Public endpoints are `GET /health` and `GET /api/v1/info`. Authenticated v1
endpoints provide namespace create/list/rename, batch missing-object lookup,
streaming object PUT/GET, namespace head lookup, revision GET, and atomic
revision commit with `expectedHead`. A stale commit receives
`409 head_mismatch` and the current head.

Phase 3B will add the desktop-side HTTP transport and push/pull orchestration;
for now these endpoints are exercised through automated API tests.

## Development

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings

cd apps/desktop
npm install
npm run check
npm run build
```

See `AGENTS.md` for the locked architecture, delivery order, and compatibility
rules.
