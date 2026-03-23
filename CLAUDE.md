# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build           # debug build
cargo build --release # release build
cargo clippy -- -D warnings  # lint (CI enforces -D warnings)
cargo fmt             # format
cargo fmt --check     # check formatting without writing
cargo test            # run tests
```

Always run `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo test` before committing.

When changing any function's behavior or signature, update the corresponding tests in the `#[cfg(test)]` module. When adding new functions, add tests covering the happy path, error cases, and edge cases.

## Architecture

Single-binary Rust CLI split across five source files:

| File | Contents |
|---|---|
| `src/main.rs` | `main()`, module declarations, `LEGACY_HISTORY_FILE`, `COMIC_DIR` |
| `src/config.rs` | `Config` struct, `load_config`, `validate_config` |
| `src/db.rs` | `ComicRecord`, all redb DB functions, `cmd_dump` |
| `src/http.rs` | `Comic` struct, `fetch_comic`, `fetch_comic_by_num`, `download_image` |
| `src/email.rs` | `build_email`, `send_email`, `escape_html`, `format_date` |

**Flow:** `main()` → `load_config` → `fetch_comic` → `last_seen_num` → backfill candidates → for each: `is_seen` → `record_first_seen` → optionally `download_image` → `send_email` → `record_email_success`

**Config** is loaded from a `.env` file via `dotenvy`, all env vars prefixed `XKCD_`. `env_bool` trims whitespace before matching. Username and password must both be set or both unset — validated at startup.

**History** is stored in `xkcd_comics.db` (redb embedded database). On first run, a legacy `xkcd_history.txt` is automatically migrated and renamed to `xkcd_history.txt.migrated`.

**Email** uses `lettre` with STARTTLS (`smtp_starttls=true`) or direct TLS (`false`). The body is always `MultiPart::alternative` (plain + HTML); when `mail_attachment=true` it's wrapped in `MultiPart::mixed`. HTML values are run through `escape_html()` before interpolation.

**TLS**: `lettre` uses `rustls-tls` (not `native-tls`), keeping the binary free of C/OpenSSL dependencies — required for cross-compilation in CI.

## Conventions

### Error handling

Every fallible operation gets `.context("descriptive message")` chained on it so errors surfaced in logs always identify the operation that failed, not just the raw library error. Use `bail!` for validation failures with a user-readable message. No custom error types — `anyhow` throughout.

### Logging

- `log::info!` — normal flow milestones (comic fetched, email sent, migration done, already-seen skip)
- `log::warn!` — recoverable per-comic failures that skip and continue (download failed, email failed, transient fetch error)
- `log::debug!` — verbose detail useful when debugging (`RUST_LOG=debug`)

### Safety invariant: mark-seen before email

`record_first_seen()` is called immediately after `is_seen()` returns false, **before** download or email. This prevents duplicate emails when the process crashes mid-run and cron retries — the next run sees the comic as already recorded and skips it. `record_email_success()` then sets `email_sent=true` as a final confirmation. Do not reorder these steps.

### HTTP status codes

In `fetch_comic_by_num()`, a 404 means the comic intentionally doesn't exist (e.g. xkcd #404) and returns `Ok(None)` — the caller marks it seen to advance past it. Any other non-2xx status is an `Err` (transient failure — skip and let cron retry). Never treat 404 as a transient error.

### Testing

- HTTP calls: mock with `mockito::Server::new()` — never reach real xkcd in tests.
- DB tests: use `db::make_db()` → `(TempDir, Database)` for isolated, auto-cleaned databases.
- Email content: test via `build_email()` + `msg.formatted()` — no live SMTP needed.
- Cross-module test helpers are `#[cfg(test)] pub(crate) fn make_*()` at **module scope** (not inside `mod tests`) so other test modules can import them.

## Branching

Use type prefixes with kebab-case descriptions (2–4 words):

| Prefix | Use for |
|---|---|
| `feat/` | New user-facing features |
| `fix/` | Bug fixes |
| `refactor/` | Code restructuring, no behavior change |
| `ci/` | CI/workflow-only changes |
| `chore/` | Maintenance: deps, docs, tooling |
| `release/vX.Y.Z` | Release branches |

**Never commit directly to `main`** — always use a branch + PR unless explicitly told otherwise.

## Opening PRs

Before opening any pull request, launch a subagent to conduct an adversarial review of the changes. The subagent should act as a skeptical reviewer and look for bugs, security issues, edge cases, and correctness problems — not style preferences. Address any real issues before pushing and opening the PR.

## CI / Release

- **CI** (`.github/workflows/ci.yml`): runs fmt, clippy, test on every push/PR.
- **Release** (`.github/workflows/release.yml`): triggered by `v*` tags. Builds `linux-amd64`, `linux-arm64`, `darwin-arm64` from a single `ubuntu-latest` runner using `cargo-zigbuild` + zig. Attaches binaries to the GitHub release.
- All GitHub Actions references are pinned to commit SHAs.
