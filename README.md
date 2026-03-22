# ferrous-comics

Emails the latest xkcd comic to the recipient specified in settings.

Checks xkcd's latest comic using the xkcd API at <https://xkcd.com/info.0.json>

If the latest comic is new, emails it to the configured recipient and records it
as seen. Optionally downloads comics to `./comics/`

## Installation

1. Clone this repo
1. Copy `example.env` to `.env` and fill in your SMTP credentials
1. `cargo build --release`
1. Run: `./target/release/ferrous-comics`
1. The binary will create `xkcd_history.txt` and (if `XKCD_DOWNLOAD=true`) a `comics/` directory
1. (optional) Install as a cron job

## Dependencies

* Rust / Cargo

## Configuration

All settings are read from environment variables (or a `.env` file) with the `XKCD_` prefix:

| Variable | Required | Default | Description |
|---|---|---|---|
| `XKCD_MAIL_TO` | yes | — | Recipient email address |
| `XKCD_MAIL_FROM` | yes | — | Sender email address |
| `XKCD_SMTP_SERVER` | yes | — | SMTP hostname |
| `XKCD_SMTP_PORT` | no | `587` | SMTP port |
| `XKCD_SMTP_STARTTLS` | no | `true` | Use STARTTLS |
| `XKCD_SMTP_USERNAME` | no | — | SMTP username |
| `XKCD_SMTP_PASSWORD` | no | — | SMTP password |
| `XKCD_DOWNLOAD` | no | `true` | Download comic image locally |
| `XKCD_MAIL_ATTACHMENT` | no | `false` | Attach image to email (requires `XKCD_DOWNLOAD=true`) |

Set `RUST_LOG=debug` for verbose output.

## Limitations

* Emails only the latest comic — will not backfill missed comics since last run
* Only accepts one recipient
* Must have file write access in the binary's working directory
* Only supports SMTP

## TODO

* Support comic backfill (one email per comic since last run)
* Make email body HTML prettier
* Test other email providers
