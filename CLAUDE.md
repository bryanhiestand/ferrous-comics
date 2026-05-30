# CLAUDE.md

Guidance for Claude Code (claude.ai/code) working in this repo.

## Commands

```bash
cargo build           # debug build
cargo build --release # release build
cargo clippy -- -D warnings  # lint (CI enforces -D warnings)
cargo fmt             # format
cargo fmt --check     # check formatting without writing
cargo test            # run tests
```

Run `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` before committing.

When changing function behavior or signature, update tests in `#[cfg(test)]` module. New functions need tests: happy path, error cases, edge cases.

## Architecture

Single-binary Rust CLI, five source files:

| File | Contents |
|---|---|
| `src/main.rs` | `main()`, module declarations, `LEGACY_HISTORY_FILE`, `COMIC_DIR` |
| `src/config.rs` | `Config` struct, `load_config`, `validate_config` |
| `src/db.rs` | `ComicRecord`, all redb DB functions, `cmd_dump` |
| `src/http.rs` | `Comic` struct, `fetch_comic`, `fetch_comic_by_num`, `download_image` |
| `src/email.rs` | `build_email`, `send_email`, `escape_html`, `format_date` |

**Flow:** `main()` → `load_config` → `fetch_comic` → `last_seen_num` → backfill candidates → for each: `is_seen` → `record_first_seen` → optionally `download_image` → `send_email` → `record_email_success`

**Config** loaded from `.env` via `dotenvy`, all vars prefixed `XKCD_`. `env_bool` trims whitespace before matching. Username and password must both be set or both unset — validated at startup.

**History** stored in `xkcd_comics.db` (redb). First run auto-migrates legacy `xkcd_history.txt`, renames to `xkcd_history.txt.migrated`.

**Email** uses `lettre` with STARTTLS (`smtp_starttls=true`) or direct TLS (`false`). Body always `MultiPart::alternative` (plain + HTML); when `mail_attachment=true` wrapped in `MultiPart::mixed`. HTML values run through `escape_html()` before interpolation.

**TLS**: `lettre` uses `rustls-tls` (not `native-tls`), no C/OpenSSL deps — required for cross-compilation in CI.

## Conventions

### Error handling

Every fallible op gets `.context("descriptive message")` so errors identify the failed operation, not just raw library error. Use `bail!` for validation failures with user-readable message. No custom error types — `anyhow` throughout.

### Logging

- `log::info!` — normal flow milestones (comic fetched, email sent, migration done, already-seen skip)
- `log::warn!` — recoverable per-comic failures that skip and continue (download failed, email failed, transient fetch error)
- `log::debug!` — verbose detail for debugging (`RUST_LOG=debug`)

### Safety invariant: mark-seen before email

`record_first_seen()` called immediately after `is_seen()` returns false, **before** download or email. Prevents duplicate emails when process crashes mid-run and cron retries — next run sees comic as recorded and skips. `record_email_success()` sets `email_sent=true` as final confirmation. Do not reorder.

### HTTP status codes

In `fetch_comic_by_num()`, 404 = comic intentionally doesn't exist (e.g. xkcd #404), returns `Ok(None)` — caller marks it seen to advance past it. Any other non-2xx = `Err` (transient — skip, let cron retry). Never treat 404 as transient.

### Testing

- HTTP calls: mock with `mockito::Server::new()` — never reach real xkcd in tests.
- DB tests: use `db::make_db()` → `(TempDir, Database)` for isolated, auto-cleaned databases.
- Email content: test via `build_email()` + `msg.formatted()` — no live SMTP needed.
- Cross-module test helpers are `#[cfg(test)] pub(crate) fn make_*()` at **module scope** (not inside `mod tests`) so other test modules can import them.

## Branching

Type prefixes, kebab-case descriptions (2–4 words):

| Prefix | Use for |
|---|---|
| `feat/` | New user-facing features |
| `fix/` | Bug fixes |
| `refactor/` | Code restructuring, no behavior change |
| `ci/` | CI/workflow-only changes |
| `chore/` | Maintenance: deps, docs, tooling |
| `release/vX.Y.Z` | Release branches |

**Never commit directly to `main`** — always branch + PR unless told otherwise.

PR titles MUST follow Conventional Commits (`feat:`, `fix:`, `refactor:`, `ci:`, `chore:`, `docs:`, `test:`, `perf:`, `build:`) regardless of branch prefix — enforced by `pr-title.yml`. Squash-merge only, so the PR title becomes the commit subject release-plz parses for version bumps.

## Opening PRs

Before opening any PR, launch subagent for adversarial review: skeptical reviewer, look for bugs, security issues, edge cases, correctness problems — not style. Fix real issues before pushing.

## CI / Release

- **CI** (`.github/workflows/ci.yml`): fmt, clippy, test on every push/PR.
- **PR title lint** (`.github/workflows/pr-title.yml`): enforces Conventional Commits on PR titles via `amannn/action-semantic-pull-request`.
- **release-plz** (`.github/workflows/release-plz.yml`): on push to `main`, runs `release-plz update` to bump `Cargo.toml`/`Cargo.lock` and append `CHANGELOG.md`. If a bump is warranted: builds `linux-amd64`, `linux-arm64`, `darwin-arm64` binaries via `cargo-zigbuild` + zig, then atomically commits `chore: release vX.Y.Z`, pushes, and creates tag + GitHub Release with binaries attached via `gh release create`. Build failure aborts the release — no partial state.
- `cliff.toml` is the single source of truth for changelog format — used by both the one-shot historical backfill (`git-cliff -o CHANGELOG.md`) and release-plz going forward.
- Do not hand-edit `Cargo.toml` `version`, `Cargo.lock`, or `CHANGELOG.md` — release-plz owns all three.
- All GitHub Actions refs pinned to commit SHAs.
