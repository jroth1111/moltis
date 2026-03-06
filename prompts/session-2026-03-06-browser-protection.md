# Browser Protection Session Summary

## Summary

- Replaced one-shot Patchright probing plus HTML/cookie mirroring with a persistent Patchright session backend for protected sites.
- Added typed navigation outcomes and backend identity to browser responses.
- Centralized protection trigger policy and stored per-session Patchright launch identity in the pool so backend switching reuses the actual launched browser.
- Updated the live real-sites suite to assert `navigate` plus follow-up `snapshot`, `get_title`, and `get_url` on the same returned session.

## Commits

- `4d155d92a` `refactor(browser): switch protected sessions to patchright`
- `e222765b4` `test(browser): validate protected sessions in live suite`

## Validation

- `cargo check -p moltis-browser --quiet`
  - exit `0`
- `cargo test -p moltis-browser --lib --quiet`
  - exit `0`
  - `117 passed, 0 failed, 8 ignored`
- `cargo test -p moltis-browser --test real_sites_test --no-run --quiet`
  - exit `0`
- `CHROME='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' MOLTIS_DATA_DIR=$(mktemp -d) cargo test -p moltis-browser --test real_sites_test test_woolworths_navigation -- --nocapture --test-threads=1`
  - exit `0`
  - Woolworths passed on Patchright after broadening generic challenge triggers
- `CHROME='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' MOLTIS_DATA_DIR=$(mktemp -d) cargo test -p moltis-browser --test real_sites_test -- --nocapture --test-threads=1`
  - exit `0`
  - `5 passed`
  - Summary:
    - Google PASS (`chromiumoxide`)
    - Woolworths PASS (`patchright`)
    - Coles PASS (`patchright`)
    - Realestate PASS (`patchright`)

## Remaining Worktree Changes

- Unrelated user changes remain uncommitted in:
  - `crates/browser/src/container.rs`
  - `crates/tools/src/branch_session.rs`
  - `crates/tools/src/sandbox_pool.rs`
  - `crates/tools/src/session_state.rs`
  - `crates/tools/src/tool_selector.rs`
  - `crates/tools/src/web_fetch.rs`

## Follow-up Improvements

- Added browser-side telemetry models in `crates/browser/src/telemetry.rs` for owned fingerprint and behavior measurement.
- Added deterministic probe tests covering:
  - `BrowserManager` identity + behavior capture
  - `PatchrightSession` identity + behavior capture
- Added DOM text sanitization in `crates/browser/src/snapshot.rs` to strip invisible Unicode prompt-injection characters before browser text is returned to the agent.
- Validation for the follow-up work:
  - `cargo check -p moltis-browser --quiet`
  - `cargo test -p moltis-browser --lib --quiet`
  - `cargo test -p moltis-browser --test real_sites_test --no-run --quiet`
- Added request-sequence telemetry in `crates/browser/src/telemetry.rs`:
  - typed `RequestSequenceEvent` and `RequestSequenceSummary`
  - owned probe routes that preserve `run_id` across page and fetch requests
  - backend coverage for both `BrowserManager` and `PatchrightSession`
- Validation for the request-sequence step:
  - `cargo test -p moltis-browser --lib telemetry::tests:: -- --nocapture`
    - exit `0`
    - `6 passed`
  - `cargo test -p moltis-browser --lib --quiet`
    - exit `0`
    - `125 passed, 0 failed, 8 ignored`
  - `cargo test -p moltis-browser --test real_sites_test --no-run --quiet`
    - exit `0`
  - `CHROME='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' MOLTIS_DATA_DIR=$(mktemp -d) cargo test -p moltis-browser --test real_sites_test -- --nocapture --test-threads=1`
    - exit `0`
    - `5 passed`
    - Summary:
      - Google PASS (`chromiumoxide`)
      - Woolworths PASS (`patchright`)
      - Coles PASS (`patchright`)
      - Realestate PASS (`patchright`)
- Added probe baseline drift utilities in `crates/browser/src/telemetry.rs`:
  - typed probe run profile/evidence models
  - drift issue classifications for profile, identity, and request-sequence changes
  - configurable timing thresholds for mean/max request-gap drift
- Validation for the drift step:
  - `cargo check -p moltis-browser --quiet`
    - exit `0`
  - `cargo test -p moltis-browser --lib telemetry::tests:: -- --nocapture`
    - exit `0`
    - `9 passed`
  - `cargo test -p moltis-browser --lib --quiet`
    - exit `0`
    - `128 passed, 0 failed, 8 ignored`
  - `cargo test -p moltis-browser --test real_sites_test --no-run --quiet`
    - exit `0`
  - `CHROME='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' MOLTIS_DATA_DIR=$(mktemp -d) cargo test -p moltis-browser --test real_sites_test -- --nocapture --test-threads=1`
    - exit `0`
    - `5 passed`
    - Summary:
      - Google PASS (`chromiumoxide`)
      - Woolworths PASS (`patchright`)
      - Coles PASS (`patchright`)
      - Realestate PASS (`patchright`)
