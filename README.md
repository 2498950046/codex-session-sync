# Codex Session Sync

A personal, self-hosted, Git-like synchronization system for Codex
conversations. The repository contains a cross-platform Tauri desktop client,
a Rust synchronization core, and an Axum server.

Phase 1 is complete: the project can perform read-only discovery and export of
local Codex sessions into normalized `ThreadBundle` records. The next phase is
the local backup, operation-journal, and safe import adapter.

## Repository layout

```text
apps/desktop/       Tauri 2 + React desktop GUI
apps/sync-server/   Personal Axum synchronization server
crates/sync-core/   Shared models and local Codex adapters
```

## Safety

The phase 1 scanner opens Codex databases read-only and never modifies
`CODEX_HOME`. Empty or malformed rollout files are returned as structured
warnings instead of aborting the scan.

## Development

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo check --workspace

cd apps/desktop
npm install
npm run build
```

See `AGENTS.md` for the locked architecture, delivery order, and compatibility
rules.
