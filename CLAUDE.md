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

Always run `cargo fmt` and `cargo clippy -- -D warnings` before committing.

## Architecture

Single-binary Rust CLI (`src/main.rs`) that runs as a cron job. No modules — everything is in one file (~290 lines).

**Flow:** `main()` → `load_config` → `fetch_comic` → `is_seen` (exit if true) → optionally `download_image` → `send_email` → `record_seen`

**Config** is loaded from a `.env` file via `dotenvy`, all env vars prefixed `XKCD_`. `env_bool` trims whitespace before matching. Username and password must both be set or both unset — validated at startup.

**History** (`xkcd_history.txt`) is a plain text file with one comic number per line. Missing file is treated as empty (not an error).

**Email** uses `lettre` with STARTTLS (`smtp_starttls=true`) or direct TLS (`false`). The body is always `MultiPart::alternative` (plain + HTML); when `mail_attachment=true` it's wrapped in `MultiPart::mixed`. HTML values are run through `escape_html()` before interpolation.

**TLS**: both `reqwest` and `lettre` use `rustls-tls` (not `native-tls`), keeping the binary free of C/OpenSSL dependencies — required for cross-compilation in CI.

## Opening PRs

Before opening any pull request, launch a subagent to conduct an adversarial review of the changes. The subagent should act as a skeptical reviewer and look for bugs, security issues, edge cases, and correctness problems — not style preferences. Address any real issues before pushing and opening the PR.

## CI / Release

- **CI** (`.github/workflows/ci.yml`): runs fmt, clippy, test on every push/PR.
- **Release** (`.github/workflows/release.yml`): triggered by `v*` tags. Builds `linux-amd64`, `linux-arm64`, `darwin-arm64` from a single `ubuntu-latest` runner using `cargo-zigbuild` + zig. Attaches binaries to the GitHub release.
- All GitHub Actions references are pinned to commit SHAs.
