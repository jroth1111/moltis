---
description: "Moltis engineering guide for Claude/Codex agents: Rust architecture, testing, security, and release workflows"
alwaysApply: true
---

# CLAUDE.md

Rust version of openclaw ([docs](https://docs.openclaw.ai), [code](https://github.com/openclaw/openclaw)).
All code must have tests with high coverage. Always check for security.

## Cargo Features

Enable new feature flags **by default** in `crates/cli/Cargo.toml` (opt-out, not opt-in):
`default = ["foo", ...]` → `foo = ["moltis-gateway/foo"]`

## Workspace Dependencies

Add new crates to `[workspace.dependencies]` in root `Cargo.toml`, reference with `{ workspace = true }`.
Never add versions directly in crate `Cargo.toml`. Use latest stable crates.io version.

## Config Schema and Validation

When adding/renaming `MoltisConfig` fields (`crates/config/src/schema.rs`), also update
`build_schema_map()` in `validate.rs`. New enum variants for string-typed fields need `check_semantic_warnings()`.

## Rust Style and Idioms

- Traits for behaviour boundaries. Generics for hot paths, `dyn Trait` for runtime dispatch.
- Derive `Default` when all fields have sensible defaults.
- Concrete types (`struct`/`enum`) over `serde_json::Value` wherever shape is known.
- **Match on types, never strings.** Strings only at serialization/display boundaries.
- Prefer `From`/`Into`/`TryFrom`/`TryInto`. Ask before adding manual conversion paths.
- Prefer streaming over non-streaming API calls.
- Run independent async work concurrently (`tokio::join!`, `futures::join_all`). Never `block_on` in async context.
- **Forbidden:** `Mutex<()>` / `Arc<Mutex<()>>` — mutex must guard actual state.
- `anyhow::Result` for app errors, `thiserror` for library errors. Propagate with `?`.
- **Never `.unwrap()`/`.expect()` in production** (workspace lints deny). Use `?`, `ok_or_else`, `unwrap_or_default`, `unwrap_or_else(|e| e.into_inner())` for locks.
- `time` crate (workspace dep) for date/time — no epoch math or magic constants. `chrono` only if already imported.
- Crates over subprocesses. Subprocesses only when no mature crate exists.
- Guard clauses over nested `if`. Iterators/combinators over manual loops. `Cow<'_, str>` for conditional allocation.
- Small public API surfaces. `#[must_use]` where return values matter.

### Tracing and Metrics

All crates: `tracing` and `metrics` features, gated with `#[cfg(feature = "...")]`.
`tracing::instrument` on async fns. Record metrics at key points. See `docs/metrics-and-tracing.md`.

## Web UI Assets

Assets in `crates/web/src/assets/` (JS, CSS, HTML). Dev: serves from disk; release: `include_dir!` with versioned URLs.

- `biome check --write` after editing JS. **Tailwind classes** over inline `style="..."`.
- No HTML from JS — hidden elements in `index.html`, toggle visibility (Preact/HTM exceptions).
- Reuse `components.css`: `provider-btn`, `provider-btn-secondary`, `provider-btn-danger`. Match button heights/text sizes.
- **Rebuild Tailwind** after new classes: `cd crates/web/ui && npx tailwindcss -i input.css -o ../src/assets/style.css --minify`
- **Selection cards** (`.model-card`, `.backend-card` in `input.css`) over dropdowns. States: `.selected`, `.disabled`. Badges: `.recommended-badge`, `.tier-badge`.
- **Provider keys**: `~/.config/moltis/provider_keys.json` via `KeyStore` in `provider_setup.rs`. On new fields update: `ProviderConfig`, `available()`, `save_key()`.
- **Gon pattern**: server data at page load → `GonData` in `server.rs`/`build_gon_data()`. JS: `import * as gon from "./gon.js"`. Never inline `<script>` tags or build HTML in Rust.
- **Events**: `import { onEvent } from "./events.js"` (WebSocket). No `window.addEventListener`/`CustomEvent`.

## API Namespace Convention

Each UI tab gets its own namespace: REST `/api/<feature>/...` and RPC `<feature>.*`. Never merge features.

## Channel Message Handling

**Always respond to approved senders** — no silent failures. Error/fallback messages for LLM/transcription failures, unhandled types. Access control via allowlist/OTP.

## Authentication

Password + passkey (WebAuthn) in `crates/gateway/src/auth.rs`, routes in `auth_routes.rs`, middleware in `auth_middleware.rs`. Setup code printed to terminal on first run.
`RequireAuth` protects `/api/*` except `/api/auth/*` and `/api/gon`.
`CredentialStore`: argon2-hashed passwords, passkeys, API keys, sessions → JSON.
CLI: `moltis auth reset-password`, `moltis auth reset-identity`.

## Testing

**Every web UI change needs E2E tests.** Tests in `crates/web/ui/e2e/specs/` (Playwright), helpers in `e2e/helpers.js`.
Run: `cd crates/web/ui && npx playwright test [spec_path]`
Selectors: `getByRole()`/`getByText({ exact: true })`, shared helpers (`navigateAndWait`, `waitForWsConnected`, `watchPageErrors`), assert no JS errors, no `waitForTimeout()`.

## Code Quality and Validation

**Pinned nightly rustfmt** — never `cargo fmt` on stable.

| Task | Command |
|------|---------|
| Format Rust | `just format` / `cargo +nightly-2025-11-30 fmt --all` |
| Format check | `just format-check` / `cargo +nightly-2025-11-30 fmt --all -- --check` |
| Clippy | `cargo +nightly-2025-11-30 clippy -Z unstable-options --workspace --all-features --all-targets --timings -- -D warnings` |
| Clippy (no nvcc) | Same but without `--all-features` |
| Preflight | `just release-preflight` (fmt + clippy gates) |
| TOML | `taplo fmt` |
| JS | `biome check --write` |

**Pre-commit checklist:** no secrets (CRITICAL), format (Rust/TOML/JS), clippy, `just release-preflight`, `cargo test`, conventional commit message, no debug code or temp files.

**Local validation** — **always** run `./scripts/local-validate.sh <PR_NUMBER>` when a PR exists. Commands above must match what `local-validate.sh` runs.

Platform-specific (Darwin):
- macOS: `./scripts/build-swift-bridge.sh && ./scripts/generate-swift-project.sh && ./scripts/lint-swift.sh && xcodebuild -project apps/macos/Moltis.xcodeproj -scheme Moltis -configuration Release -destination "platform=macOS" -derivedDataPath apps/macos/.derivedData-local-validate build`
- iOS: `cargo run -p moltis-schema-export -- apps/ios/GraphQL/Schema/schema.graphqls && ./scripts/generate-ios-graphql.sh && ./scripts/generate-ios-project.sh && xcodebuild -project apps/ios/Moltis.xcodeproj -scheme Moltis -configuration Debug -destination "generic/platform=iOS" CODE_SIGNING_ALLOWED=NO build`

## Sandbox Architecture

Containers (Docker/Apple Container): `crates/tools/src/sandbox.rs` (trait + impls), `exec.rs` (ExecTool), `crates/cli/src/sandbox_commands.rs` (CLI), `crates/config/src/schema.rs` (config).
Pre-built images: deterministic hash tags from base + packages. Defaults in `default_sandbox_packages()`.
CLI: `moltis sandbox {list,build,remove,clean}`.

## Logging Levels

`error!` unrecoverable | `warn!` unexpected but recoverable | `info!` milestones | `debug!` diagnostics | `trace!` per-item verbose.
**Common mistake:** `warn!` for unconfigured providers — use `debug!` for expected "not configured" states.

## Security

- **WS Origin**: `server.rs` rejects cross-origin upgrades (403). Loopback variants equivalent.
- **SSRF**: `web_fetch.rs` blocks loopback/private/link-local/CGNAT IPs. Preserve on changes.
- **Secrets**: `secrecy::Secret<String>` for passwords/keys/tokens. `expose_secret()` only at consumption. Manual `Debug` with `[REDACTED]`. Scope `RwLock` guards in blocks. See `crates/oauth/src/types.rs`.
- **Never commit** passwords, credentials, `.env` with real values, or PII. Recovery: `git reset HEAD~1`, remove, re-commit. If pushed, rotate immediately.

## Data and Config Directories

- `moltis_config::config_dir()` (`~/.moltis/`): `moltis.toml`, `credentials.json`, `mcp-servers.json`.
- `moltis_config::data_dir()` (`~/.moltis/`): DBs, sessions, logs, memory files.
- **Never** `directories::BaseDirs` outside `moltis-config`. Never `std::env::current_dir()` for storage.
- Workspace files (`MEMORY.md`, `memory/*.md`) resolve relative to `data_dir()`. Gateway resolves once at startup.

## Database Migrations

sqlx migrations, each crate owns `migrations/`. See `docs/sqlite-migration.md`.

| Crate | Tables |
|-------|--------|
| `moltis-projects` | `projects` |
| `moltis-sessions` | `sessions`, `channel_sessions` |
| `moltis-cron` | `cron_jobs`, `cron_runs` |
| `moltis-gateway` | `auth_*`, `passkeys`, `api_keys`, `env_variables`, `message_log`, `channels` |
| `moltis-memory` | `files`, `chunks`, `embedding_cache`, `chunks_fts` |

New: `crates/<crate>/migrations/YYYYMMDDHHMMSS_description.sql` (use `IF NOT EXISTS`).
New crate: `run_migrations()` in `lib.rs`, call from `server.rs` in dependency order.

## Provider Implementation

All async — never `block_on`. Broad model lists (API errors handle unavailable). Check `../clawdbot/` for reference.
BYOM providers (OpenRouter, Ollama): require user config, don't hardcode models.

## Git Workflow

Conventional commits: `feat|fix|docs|style|refactor|test|chore(scope): description`
**No `Co-Authored-By` trailers.** Update `README.md` features list with `feat` commits.

- **Releases**: never overwrite tags. `[workspace.package].version` must match. Use `./scripts/prepare-release.sh <version> [date]`. Deploy template tags updated by CI.
- **Lockfile**: `cargo fetch` to sync (not `cargo update`). Verify: `cargo fetch --locked`. `local-validate.sh` auto-handles. `cargo update --workspace` for intentional upgrades only.
- **Changelog**: auto-generated via `git-cliff` (`cliff.toml`). No manual entries. Preview: `just changelog-unreleased`. CI enforces via `scripts/check-changelog-guard.sh`.
- **PR descriptions**: required `## Summary`, `## Validation` (`### Completed`/`### Remaining` with exact commands), `## Manual QA`.

## Documentation

`docs/src/` (mdBook), auto-deployed to docs.moltis.org. Update `SUMMARY.md` when adding pages. Preview: `cd docs && mdbook serve`.

## Session Completion

**Work is NOT complete until `git push` succeeds.**
1. File issues for remaining work (`bd create "Title" --type task --priority 2`)
2. Run quality gates
3. Update issue status, push: `git pull --rebase && bd sync && git push && git status`
4. Clean up stashes/branches, hand off context

Issue tracking: **bd (beads)** — `bd ready`, `bd close <id>`, `bd sync` (at session end). Full docs: `bd prime`.
Plans in `prompts/`. Session summaries: `prompts/session-YYYY-MM-DD-<topic>.md`.
