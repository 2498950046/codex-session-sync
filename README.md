# Codex Session Sync

Personal, self-hosted synchronization for Codex conversation data. The
desktop client is Tauri 2 + React + TypeScript, the local core and server are
Rust, and the server uses Axum, SQLite metadata, and filesystem object
storage.

## Current implementation

The development target is storage/protocol v4-only. Earlier synchronization
endpoints, untyped remote object transfer, and the old server Revision store
are not part of the build. Codex compatibility is still separate: both modern
`sqlite/*.db` and legacy `state_5.sqlite` homes are scanned, and active plus
archived rollout directories are supported.

v4 stores a provider/workspace-neutral immutable typed object graph. Rollouts
are normalized to fixed provider/workspace tokens before hashing; restore and
checkout materialize the current machine's provider and workspace afterward:

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
- Local snapshot list with labels, tags, pinning, compare, validation, semantic
  restore, and recoverable trash. Semantic restore preserves thread meaning,
  while current-machine provider, workspace paths, and rollout formatting are
  materialized locally.
- Remote namespace and Revision history with download, local restore, restore
  and publish, explicit Head rewind, current-Head deletion, and trash restore.
- Operation Recovery Points that automatically surface incomplete import and
  checkout/provider-sync journals, with incomplete points pinned above terminal records.
- Offline local Provider Sync rewrites rollout and SQLite provider metadata
  with preview, backups, rollback, and restart recovery. Provider identity is
  materialized per machine and does not create remote revisions.
- Repository shared/exclusive leases in the Tauri backend. Read-only history,
  validation, and sync operations can share a repository; trash changes and
  permanent deletion use an exclusive lease. The React busy flag is not the
  synchronization boundary.
- Local snapshot and server history recycle bins support restoring, permanently
  deleting one recovery point, or emptying the bin. Permanent deletion removes
  only objects that are globally unreachable; shared objects remain intact.
- Repository storage statistics: logical bytes, physical bytes, shared and
  exclusive references, trash protection, quarantine, and reclaimable bytes.

## v4 HTTP API

Public endpoints are `GET /health` and `GET /api/v4/info`. All data endpoints
require the configured Bearer token.

```text
GET/POST  /api/v4/namespaces
PATCH     /api/v4/namespaces/{id}
GET       /api/v4/namespaces/{id}/head
GET       /api/v4/namespaces/{id}/revisions
POST      /api/v4/namespaces/{id}/revisions/commit
POST      /api/v4/objects/missing
PUT/GET   /api/v4/objects/{kind}/{sha256}
POST      /api/v4/namespaces/{id}/history/truncations
GET       /api/v4/namespaces/{id}/trash
POST      /api/v4/namespaces/{id}/trash/{operation}/restore
POST      /api/v4/namespaces/{id}/trash/purge
GET       /api/v4/storage
GET       /api/v4/gc/plan
POST      /api/v4/gc/quarantine
```

Revision commits use `expectedHead` and `expectedNamespaceEpoch`. Snapshot
overlays are local-only and are never uploaded. The server
validates the complete Root/Descriptor/Manifest/Chunk/Attachment graph before
the SQLite Head compare-and-swap. History rewrites increment the namespace
epoch and move removed revisions into recoverable trash. Server GC uses a
persistent queue, rechecks global reachability immediately before quarantine,
and resumes pending entries after restart. A manual history purge advances its
selected unreachable objects through quarantine to permanent deletion; shared
or newly reachable objects are retained. Recently uploaded objects are excluded
from ordinary GC plans for a short safety grace period.

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
└─ index/source-objects-v4.json
```

The default repository is `~/.codex-session-sync`. It is independent from the
real Codex Home (`~/.codex`).

## Safety boundaries

Codex must be fully closed before snapshot creation, import, semantic restore,
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
`cd apps/desktop
npm install
npm run tauri -- dev`
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
