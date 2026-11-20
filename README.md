# ferrous-comics

<img src="assets/social-preview.svg" alt="ferrous-comics — xkcd, delivered to your inbox"/>

[![CI](https://github.com/bryanhiestand/ferrous-comics/actions/workflows/ci.yml/badge.svg)](https://github.com/bryanhiestand/ferrous-comics/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Emails the latest xkcd comic to the recipient specified in settings.

Checks xkcd's latest comic using the xkcd API at <https://xkcd.com/info.0.json>

If the latest comic is new, emails it to the configured recipient and records it
as seen. Optionally downloads comics to `./comics/`

## Installation

1. Clone this repo
1. Copy `example.env` to `.env` and fill in your SMTP credentials
1. `cargo build --release`
1. Run: `./target/release/ferrous-comics`
1. The binary will create `xkcd_comics.db` and (if `XKCD_DOWNLOAD=true`) a `comics/` directory. If `xkcd_history.txt` exists from a previous version it will be automatically migrated and renamed to `xkcd_history.txt.migrated`
1. (optional) Install as a cron job

## Dependencies

* Rust / Cargo

## Configuration

All settings are read from environment variables (or a `.env` file) with the `XKCD_` prefix:

| Variable | Required | Default | Description |
|---|---|---|---|
| `XKCD_MAIL_TO` | yes | — | Recipient email address (comma-separated for multiple) |
| `XKCD_MAIL_FROM` | yes | — | Sender email address |
| `XKCD_SMTP_SERVER` | yes | — | SMTP hostname |
| `XKCD_SMTP_PORT` | no | `587` | SMTP port |
| `XKCD_SMTP_STARTTLS` | no | `true` | Use STARTTLS |
| `XKCD_SMTP_USERNAME` | no | — | SMTP username |
| `XKCD_SMTP_PASSWORD` | no | — | SMTP password |
| `XKCD_DOWNLOAD` | no | `true` | Download comic image locally |
| `XKCD_MAIL_ATTACHMENT` | no | `false` | Attach image to email (requires `XKCD_DOWNLOAD=true`) |
| `XKCD_DB_PATH` | no | `xkcd_comics.db` | Path to the comic history database |
| `XKCD_BACKFILL_LIMIT` | no | `5` | Max comics to send per run when catching up after a missed day; `0` to disable backfill |
| `XKCD_AUTO_UPDATE` | no | `false` | Check for and install new releases (same major version only) at most once a day; see [Updating](#updating) |

Set `RUST_LOG=debug` for verbose output.

## Database

Comic history is stored in `xkcd_comics.db` (a [redb](https://github.com/cberner/redb) embedded database). Each record is a JSON object:

```json
{"num":3222,"first_seen_utc":1742563200,"image_downloaded":true,"email_sent":true,"email_sent_utc":1742563205}
```

Fields:

| Field | Description |
|---|---|
| `num` | xkcd comic number |
| `first_seen_utc` | Unix timestamp when the comic was first noticed (`0` = imported from legacy file) |
| `image_downloaded` | Whether the image was successfully saved to `comics/` |
| `email_sent` | Whether the email was successfully sent |
| `email_sent_utc` | Unix timestamp of the successful send, or `null` |

Databases created by older releases (redb 2.x file format) are upgraded automatically on first run, and a copy of the original file is kept as `xkcd_comics.db.v2.bak`. Older releases can't open the upgraded database. To roll back, restore the backup over `xkcd_comics.db`; any comics recorded after the upgrade will be treated as new and emailed again. Delete the backup once you no longer need to roll back.

### Check the version

```bash
ferrous-comics version
# → ferrous-comics 1.3.0 (abc1234)
```

### Updating

Prebuilt binaries (linux-amd64, linux-arm64, darwin-arm64) can update themselves from GitHub Releases:

```bash
ferrous-comics update                 # install the latest release within the current major version
ferrous-comics update --allow-major   # also allow a new major version
```

With `XKCD_AUTO_UPDATE=true`, a normal run checks for updates at most once every 24 hours. The check happens after the comics are processed, and also when the run fails, so a broken release can still be replaced by a fixed one. The check time is tracked in `<XKCD_DB_PATH>.update-check`. New major versions are never installed automatically; they are logged so you can read the changelog and run `update --allow-major` yourself.

How an update is installed:

1. The release list is fetched from the GitHub API over HTTPS, and the newest stable release within the current major version is picked. So 1.x installs keep getting 1.x fixes even after a 2.0 is published.
2. The binary is verified against the release's `SHA256SUMS` and against the SHA-256 digest GitHub records for the asset.
3. The new binary is run with `version` to check that it works and reports the expected version.
4. It is renamed over the current binary, and the previous binary is kept as `<binary>.prev`. To roll back, move `.prev` back into place. If the update changed the database format (see the changelog), the previous binary can't read the upgraded database, so restore the database backup as well.

The new version takes effect on the next run. The binary's directory must be writable by the user running it. A `<binary>.update.lock` file stops overlapping updates. Under the lock, the version of the binary on disk is checked again, so a slower run can't downgrade a binary that another run has just updated.

Trust model: updating trusts whoever can publish releases to this GitHub repository. The checksums catch corrupted or mismatched downloads, but they come from the same release, so they don't protect against a compromised account or release workflow. Leave auto-update off if that matters for your setup.

### Inspect the database

Dump all records as newline-delimited JSON:

```bash
ferrous-comics dump
# or during development:
make dump
```

Pretty-print a single record by comic number (requires `jq`):

```bash
ferrous-comics dump | jq 'select(.num == 3222)'
```

List comics where the email was not sent:

```bash
ferrous-comics dump | jq 'select(.email_sent == false)'
```

Count total comics seen:

```bash
ferrous-comics dump | wc -l
```

Convert `first_seen_utc` to a readable date (requires `jq` and `date`):

```bash
ferrous-comics dump | jq '.first_seen_utc' | xargs -I{} date -r {}
```

## Limitations

* Must have file write access in the binary's working directory
