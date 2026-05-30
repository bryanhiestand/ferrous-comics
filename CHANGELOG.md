# Changelog

All notable changes to this project are documented here.


## [1.4.6] - 2026-05-30

### Bug Fixes
- Upgrade lettre to 0.11.22 (RUSTSEC-2026-0141) ([#35](https://github.com/bryanhiestand/ferrous-comics/pull/35))
- Re-release pending lettre upgrade as v1.4.5 ([#43](https://github.com/bryanhiestand/ferrous-comics/pull/43))
- Skip v1.4.5 (tag reserved by deleted immutable release) ([#44](https://github.com/bryanhiestand/ferrous-comics/pull/44))
- Bump version to 1.4.6 (v1.4.5 tag is burned) ([#45](https://github.com/bryanhiestand/ferrous-comics/pull/45))


## [1.4.4] - 2026-05-09

### Bug Fixes
- Clear RUSTSEC advisories (rustls-webpki, rand) (#30)

## [1.4.3] - 2026-03-25

### Other
- Restore timestamps in log output (#27)

## [1.4.2] - 2026-03-24

### Other
- Reduce binary size 3.3 MiB -> 2.0 MiB via dep trimming (#26)

## [1.4.1] - 2026-03-24

### Other
- Replace softprops/action-gh-release with gh release upload (#25)

## [1.4.0] - 2026-03-24

### Other
- Add CI badge to README
- Split main.rs into config, db, http, and email modules (#17)
- Document branching convention and update Architecture section (#18)
- Add Conventions section to CLAUDE.md (#19)
- Add MIT license (#20)
- Add version subcommand with git commit hash (#21)
- Upgrade to Rust edition 2024 (#22)
- Remove stale TODO and SMTP limitation from README (#23)
- Reduce binary size and speed up CI tooling installs (#24)

## [1.3.0] - 2026-03-22

### Other
- Fix dump subcommand requiring full SMTP config (#11)
- Add comprehensive test harness (34 tests) (#15)
- Support multiple recipients in XKCD_MAIL_TO (#12)
- Improve HTML email template (#13)
- Add comic backfill: email missed comics since last run (#14)
- Fix backfill delivering only 1 comic on fresh install (#16)

## [1.2.0] - 2026-03-22

### Other
- Add darwin-arm64 cross-compile check to CI (#5)
- Replace reqwest with ureq (#6)
- Sync version to 1.1.2, auto-track in USER_AGENT, add bump-version workflow (#7)
- Fix YAML syntax error in bump-version workflow (#8)
- Fix multiline BODY breaking YAML in bump-version workflow (#9)
- Drop gh pr create from bump-version workflow
- Delete stale release branch before pushing in bump-version workflow
- Auto-patch version from git tag in release workflow (#10)

## [1.1.2] - 2026-03-22

### Other
- Fix darwin-arm64 cross-compilation (CoreFoundation) — v1.1.1 (#4)

## [1.1.0] - 2026-03-22

### Other
- Rust rewrite of xkcd_checker
- Replace xkcd_history.txt with redb embedded database (v1.1.0) (#3)

