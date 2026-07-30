# Codex Session Sync

## Storage protocol v2

New local snapshots use a typed, content-addressed graph. Small rollouts are
stored as immutable whole objects; rollouts of 8 MiB or more use fixed 4 MiB
chunks plus a canonical chunk manifest. Thread metadata is stored once as an
immutable descriptor and the snapshot root contains only thread/descriptor
references. Existing v1 snapshots and `/api/v1` objects remain readable.

The server advertises protocol 2 capabilities and exposes authenticated typed
object transfer at `/api/v2/objects/{kind}/{sha256}` plus the batched
`/api/v2/objects/missing` query. Typed objects are separated by kind even when
their byte hashes match. Metadata schema v2 includes rebuildable object and
edge tables for reachability and future server-side maintenance.

Local GC is deliberately two-phase: `plan_local_gc` recomputes reachability
from authoritative v2 roots and `quarantine_local_gc_plan` revalidates the
plan before moving unreachable immutable objects into `trash/gc/<operation>`.
It never permanently deletes data. Legacy repository optimization remains an
explicit user operation; application startup does not migrate or reclaim old
objects automatically.

A personal, self-hosted, Git-like synchronization system for Codex
conversations. The repository contains a cross-platform Tauri desktop client,
a Rust synchronization core, and an Axum server.

Phase 6 is implemented: the desktop client can securely store a server token,
manage remote namespaces, perform Push, Pull, and exact namespace checkout,
explicitly resolve divergent changes to the same thread UUID, and select a
namespace from local API-key/provider/Codex-Home mappings. Remote operations
and identity detection run in the Tauri Rust backend; the React webview never
receives the stored token, a raw API key, or direct server access.

## Repository layout

```text
apps/desktop/       Tauri 2 + React desktop GUI
apps/sync-server/   Personal Axum synchronization server
crates/sync-core/   Shared models and local Codex adapters
```

## Safety

The dashboard scanner opens Codex databases read-only and reads only rollout
metadata instead of hashing every complete file. Snapshot creation, import,
recovery, Push, Pull, conflict resolution, and namespace switching require
explicit confirmation that Codex is fully closed and a live cross-platform
process check. The backend
also serializes write operations per normalized Codex Home, so another IPC
request cannot bypass the GUI's busy state. Snapshot creation hashes each
changed rollout while copying it once into the object store. Imports validate
all SHA-256 objects before writing, reject divergent updates to an existing
thread UUID, create a database backup, and re-scan the target before marking
the journal complete.

An empty-rollout warning can be cleaned from the compatibility panel only
after Codex is confirmed closed. The backend revalidates that the selected
path is a regular zero-byte `rollout-*.jsonl` inside the selected Codex Home's
session directories, then moves it into the local repository's recoverable
`quarantine/empty-rollouts` directory and triggers a fresh scan. Non-empty,
symlinked, renamed, or out-of-home paths are rejected.

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

Optional automatic namespace selection reads only the selected Codex Home's
`config.toml`, the explicit `OPENAI_API_KEY`/`api_key` fields in `auth.json`,
and a configured provider `env_key`. The raw API key is never saved, returned
to React, or uploaded. The backend stores only a local HMAC-SHA256 fingerprint
derived from the key and the normalized synchronization-server URL in
`config/namespace-mappings-v1.json`. Mapping rules can combine API-key,
provider, and exact normalized Codex-Home conditions; API-key matches have the
highest weight, followed by provider and path. Equal best matches that point to
different namespaces are reported as ambiguous instead of choosing silently.

Clicking a namespace while automatic selection is enabled creates a local
manual override for that remote and Codex Home. **恢复自动选择** clears it.
Automatic selection changes only the target highlighted in the GUI: it never
starts checkout, Pull, Push, or any local write on its own.

Conflict choices are bound to immutable fingerprints derived from the base,
local, and remote semantic thread hashes. The backend re-scans the local Codex
Home and re-reads the current remote Head before accepting a choice. If any
version or the conflict set changed while the GUI was open, resolution is
rejected and Pull must be run again. Accepted choices are backed up and checked
out locally first, then published with an ordinary Head compare-and-swap; there
is no force-push path. Publishing becomes non-cancellable after the guarded
local checkout completes. If publishing still fails, the resolved local state
and its tracked remote parent remain safe and an ordinary Push can retry it.

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
5. If Pull reports same-thread conflicts, compare the common base, local, and
   remote summaries, choose the local or remote result for every item, then
   select **应用选择并完成合并**. A deleted side is also an explicit choice.
6. Use namespace switch only after reviewing and accepting the exact local
   replacement confirmation. A recoverable backup is always created first.
7. Optionally enable automatic namespace selection and create local rules for
   the current API key, provider, Codex Home, or a combination of them.

Push never force-updates a remote namespace. If its Head is ahead of local
Tracking and the thread content differs, the client rejects Push and requires
Pull. Different thread UUIDs merge automatically. Divergent edits or
delete/modify changes to the same UUID open the conflict workbench and never
change local conversations or advance Tracking until every item has an
explicit, still-valid choice.
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

### Temporary public HTTP deployment with Docker

The deployment files under `deploy/server` publish TCP port `8787` on every
server interface, so a desktop client can connect to
`http://SERVER_PUBLIC_IP:8787`. This mode is intended only for temporary
testing: the Bearer token and synchronized conversation content are not
encrypted on the public network. Move the service behind HTTPS before using
real or sensitive conversation data.

On the Linux server, copy or clone this repository and run:

```bash
cd /opt/codex-session-sync/deploy/server
cp .env.example .env
openssl rand -hex 32
nano .env
docker compose up -d --build
docker compose ps
docker compose logs --tail=100 sync-server
curl http://127.0.0.1:8787/health
```

Put the generated 64-character hexadecimal value after
`SYNC_SERVER_TOKEN=` in `.env`. If a host or cloud firewall is enabled, allow
inbound TCP port `8787`. The Compose port mapping is deliberately
`0.0.0.0:8787:8787`, and persistent server data is stored in the Docker named
volume `codex-session-sync-data`.

From another computer, verify the public endpoint:

```bash
curl http://SERVER_PUBLIC_IP:8787/health
```

Then configure the desktop profile with
`http://SERVER_PUBLIC_IP:8787` and the exact token from `.env`. Routine
operations are:

```bash
cd /opt/codex-session-sync/deploy/server
docker compose logs --tail=100 -f sync-server
docker compose restart sync-server
docker compose up -d --build
```

Do not delete the `codex-session-sync-data` volume during upgrades. Switching
to a domain later requires only an HTTPS reverse proxy and changing the
desktop server URL; the stored namespace data remains in the same volume.

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

For repeatable browser-only visual QA, run `npm run dev` and open either
`http://127.0.0.1:1420/?preview=conflict` for the conflict workbench or
`http://127.0.0.1:1420/?preview=mapping` for namespace mappings. These mocks
are loaded only by the Vite development build and are removed from production
bundles.

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
