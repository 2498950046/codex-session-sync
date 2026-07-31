# Codex Session Sync

Personal, self-hosted synchronization for Codex conversation data. The
desktop client is Tauri 2 + React + TypeScript, the local core and server are
Rust, and the server uses Axum, SQLite metadata, and filesystem object
storage.

## Current implementation

The development target is storage/protocol v2-only. v1 synchronization
endpoints, untyped remote object transfer, and the old server Revision store
are not part of the build. Codex compatibility is still separate: both modern
`sqlite/*.db` and legacy `state_5.sqlite` homes are scanned, and active plus
archived rollout directories are supported.

v2 stores an immutable typed object graph:

```text
Revision Root
  └─ Thread Descriptor
       ├─ Whole object
       └─ Chunk Manifest ── Chunk objects
```

Objects are addressed by `(kind, sha256)`. Roots and structured objects are
canonical JSON whose content hash is their immutable identity. SQLite files
are never merged as binary files; thread-level semantic bundles are checked
out through guarded backups, journals, transactions, validation, and
rollback.

## Desktop features

- IDEA-style version graph on Sync and Snapshot & Recovery pages.
- Local snapshot list with labels, tags, pinning, compare, validation, exact
  restore, and recoverable trash.
- Remote namespace and Revision history with download, local restore, restore
  and publish, explicit Head rewind, current-Head deletion, and trash restore.
- Operation Recovery Points that automatically surface incomplete import and
  checkout journals, with incomplete points pinned above terminal records.
- Repository shared/exclusive leases in the Tauri backend. Read-only history,
  validation, and sync operations can share a repository; trash and GC use an
  exclusive lease. The React busy flag is not the synchronization boundary.
- Local reachability GC that protects active snapshots, snapshot trash,
  cached remote Revision Roots, and non-terminal journals. GC first moves
  objects to quarantine and never permanently deletes them automatically.
- Repository storage statistics: logical bytes, physical bytes, shared and
  exclusive references, trash protection, quarantine, and reclaimable bytes.

## v2 HTTP API

Public endpoints are `GET /health` and `GET /api/v2/info`. All data endpoints
require the configured Bearer token.

```text
GET/POST  /api/v2/namespaces
PATCH     /api/v2/namespaces/{id}
GET       /api/v2/namespaces/{id}/head
GET       /api/v2/namespaces/{id}/revisions
POST      /api/v2/namespaces/{id}/revisions/commit
POST      /api/v2/objects/missing
PUT/GET   /api/v2/objects/{kind}/{sha256}
POST      /api/v2/namespaces/{id}/history/truncations
GET       /api/v2/namespaces/{id}/trash
POST      /api/v2/namespaces/{id}/trash/{operation}/restore
GET       /api/v2/storage
GET       /api/v2/gc/plan
POST      /api/v2/gc/quarantine
```

Revision commits use `expectedHead` and `expectedNamespaceEpoch`. The server
validates the complete Root/Descriptor/Manifest/Chunk/Attachment graph before
the SQLite Head compare-and-swap. History rewrites increment the namespace
epoch and move removed revisions into recoverable trash. Server GC uses a
persistent queue, rechecks global reachability immediately before quarantine,
and resumes pending entries after restart. Recently uploaded objects are
excluded from plans for a short safety grace period.

## Local repository layout

```text
.codex-session-sync/
├─ objects/{whole,chunks,chunk-manifests,threads,revision-roots}/sha256/
├─ objects/tmp/
├─ snapshots/
├─ metadata/snapshots/
├─ backups/
├─ journal/
├─ trash/snapshots/
├─ trash/gc/
├─ quarantine/
└─ index/source-objects-v2.json
```

The default repository is `~/.codex-session-sync`. It is independent from the
real Codex Home (`~/.codex`).

## Safety boundaries

Codex must be fully closed before snapshot creation, import, exact restore,
Push, Pull, conflict resolution, namespace switching, or quarantine cleanup.
The backend performs a fresh cross-platform process check and refuses to
terminate Codex automatically. Every write to a real Codex Home creates a
backup and operation journal first, then validates the result and can roll
back or recover after a crash.

Raw API keys are never uploaded or returned over IPC. Remote bearer tokens are
stored only in the operating-system credential backend. Namespace auto-
selection stores only a local HMAC fingerprint bound to the server URL.

## Run

Desktop development:

```powershell
cd apps/desktop
npm install
npm run tauri -- dev
```

Server development:

```powershell
$env:SYNC_SERVER_TOKEN = "replace-with-a-long-random-token"
$env:SYNC_SERVER_DATA_DIR = "D:\codex-session-sync-data"
cargo run -p sync-server
```

Docker Compose files are under `deploy/server`. The sample direct-IP HTTP
profile is for temporary trusted-network testing only; put the service behind
HTTPS before using sensitive or long-term Internet traffic.

## Verification

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings

cd apps/desktop
npm run check
npm run build
npm test -- --run
```

All automated local-write and HTTP integration tests use temporary Codex
Homes and temporary server data. They do not modify the current machine's
real `C:\Users\24989\.codex` directory.
