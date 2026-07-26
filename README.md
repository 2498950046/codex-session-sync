# Codex Session Sync

A personal, self-hosted, Git-like synchronization system for Codex
conversations. The repository contains a cross-platform Tauri desktop client,
a Rust synchronization core, and an Axum server.

Phase 3B is implemented: the desktop client can securely store a server token,
manage remote namespaces, and perform Push, Pull, and exact namespace checkout
through the authenticated server API. Remote operations run in the Tauri Rust
backend; the React webview never receives the stored token or talks to the
server directly.

## Repository layout

```text
apps/desktop/       Tauri 2 + React desktop GUI
apps/sync-server/   Personal Axum synchronization server
crates/sync-core/   Shared models and local Codex adapters
```

## Safety

The dashboard scanner opens Codex databases read-only and reads only rollout
metadata instead of hashing every complete file. Snapshot creation, import,
recovery, Push, Pull, and namespace switching require explicit confirmation
that Codex is fully closed and a live cross-platform process check. The backend
also serializes write operations per normalized Codex Home, so another IPC
request cannot bypass the GUI's busy state. Snapshot creation hashes each
changed rollout while copying it once into the object store. Imports validate
all SHA-256 objects before writing, reject divergent updates to an existing
thread UUID, create a database backup, and re-scan the target before marking
the journal complete.

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

Server tokens are stored by the native credential backend on Windows, macOS,
and Linux. Ordinary remote profile data is stored separately as JSON without
the token. A Pull or namespace switch records its target Tracking update in the
durable checkout journal. Local replacement, Tracking compare-and-swap, and
the active-namespace binding can therefore be reconciled after a crash instead
of leaving the checked-out conversations under the wrong namespace.
An unfinished checkout journal blocks another checkout for the same Codex Home
in that local synchronization repository. If a `LocalApplied` journal no longer
matches the live conversations or its Tracking compare-and-swap conflicts,
recovery refuses to overwrite either side and leaves the journal for explicit
resolution.

Exact checkout backs up every affected thread database and the rebuildable
`codex-dev.db` local thread catalog before replacing session directories. It
invalidates only the local catalog rows and full-scan watermark; Codex rebuilds
that directory from the newly checked-out state database on its next start.
Unrelated automation, inbox, feature, and timeline tables are preserved.

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
checkbox and using snapshot, import, recovery, Push, Pull, or namespace-switch
actions. In the GUI:

1. Add the synchronization server URL and Bearer token, then save and verify.
2. Create or select a namespace.
3. Use Push to initialize an empty namespace or publish local changes.
4. Use Pull when the active namespace has advanced remotely.
5. Use namespace switch only after reviewing and accepting the exact local
   replacement confirmation. A recoverable backup is always created first.

Push never force-updates a remote namespace. If its Head is ahead of local
Tracking and the thread content differs, the client rejects Push and requires
Pull. Different thread UUIDs merge automatically; divergent edits to the same
UUID return a conflict without changing local conversations or advancing
Tracking.
If a prior Push reached the server but crashed before updating local Tracking,
retrying Push recognizes semantically identical remote content and atomically
repairs Tracking plus the active-namespace binding.

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

The desktop HTTP client rejects redirects, bounds JSON and error responses,
streams large objects, and verifies object length and SHA-256 before
installation.

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

The native credential-store smoke test is intentionally ignored during normal
test runs because it briefly creates an isolated operating-system credential.
It uses a unique UUID and deletes the credential before succeeding:

```powershell
cargo test -p codex-session-sync-desktop --lib remote_config::tests::system_credential_store_round_trip_uses_native_backend_and_cleans_up -- --ignored --exact
```

All automated checkout and synchronization tests use temporary Codex homes and
temporary server data. Development and verification do not modify the current
machine's real Codex conversation data.

See `AGENTS.md` for the locked architecture, delivery order, and compatibility
rules.
