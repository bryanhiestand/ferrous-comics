//! Self-update from GitHub Releases.
//!
//! Trust model: a release is trusted if it is the latest non-prerelease on the project's GitHub
//! repo, fetched over HTTPS. The downloaded binary must match the SHA-256 in the release's
//! `SHA256SUMS` asset (catches truncated/corrupted downloads and mismatched assets), and must run
//! and report the expected version before it replaces the current binary. Releases that bump the
//! major version are never installed automatically, because majors may carry one-way changes
//! (e.g. database migrations) that should be applied deliberately.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::http::USER_AGENT;

pub(crate) const GITHUB_API_BASE: &str =
    "https://api.github.com/repos/bryanhiestand/ferrous-comics";
pub(crate) const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECKSUMS_ASSET: &str = "SHA256SUMS";
/// Upper bound on any downloaded asset; release binaries are a few MiB.
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
/// Minimum time between automatic update checks.
pub(crate) const AUTO_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    /// `sha256:<hex>`, computed by GitHub at upload time (absent on very old assets).
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    /// A newer major version exists but was not installed.
    MajorAvailable(String),
    Installed {
        from: String,
        to: String,
    },
    /// Another update holds the lock; nothing was done.
    InProgress,
}

/// Release asset name for the platform this binary was built for.
pub(crate) fn asset_name() -> Option<&'static str> {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("ferrous-comics-linux-amd64")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("ferrous-comics-linux-arm64")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("ferrous-comics-darwin-arm64")
    } else {
        None
    }
}

/// Parses `X.Y.Z` or `vX.Y.Z`. Pre-release/build suffixes are rejected.
pub(crate) fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Finds the expected hash for `name` in `sha256sum`-format output (`<hex>  <name>` or
/// `<hex> *<name>`).
fn expected_sha256(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim_start();
        let file = file.strip_prefix('*').unwrap_or(file);
        (file == name && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

fn get(agent: &ureq::Agent, url: &str, what: &str) -> anyhow::Result<ureq::Response> {
    agent
        .get(url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => anyhow::anyhow!("{what}: HTTP {code}"),
            ureq::Error::Transport(t) => {
                anyhow::Error::new(t).context(format!("{what}: request failed"))
            }
        })
}

fn download(agent: &ureq::Agent, url: &str, what: &str) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    get(agent, url, what)?
        .into_reader()
        .take(MAX_DOWNLOAD_BYTES + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("{what}: failed to read body"))?;
    if buf.len() as u64 > MAX_DOWNLOAD_BYTES {
        bail!("{what}: exceeds {MAX_DOWNLOAD_BYTES} bytes");
    }
    Ok(buf)
}

/// Most recent releases, newest first. 100 is the API's page maximum and far more history
/// than is needed to find the newest release in the current major.
fn fetch_releases(agent: &ureq::Agent, api_base: &str) -> anyhow::Result<Vec<Release>> {
    let url = format!("{}/releases?per_page=100", api_base.trim_end_matches('/'));
    get(agent, &url, "listing releases")?
        .into_json::<Vec<Release>>()
        .context("failed to parse release list JSON")
}

type Version = (u64, u64, u64);

fn fmt_version(v: Version) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

/// Picks the release to install: the highest stable release newer than `cur` within the same
/// major (any major when `allow_major`). Also returns the highest newer release in a *different*
/// major, if any, so it can be reported. Drafts, pre-releases and unparseable tags are ignored.
fn select_release(
    releases: &[Release],
    cur: Version,
    allow_major: bool,
) -> (Option<(&Release, Version)>, Option<Version>) {
    let newer = releases
        .iter()
        .filter(|r| !r.draft && !r.prerelease)
        .filter_map(|r| parse_version(&r.tag_name).map(|v| (r, v)))
        .filter(|(_, v)| *v > cur);
    let mut same_major: Option<(&Release, Version)> = None;
    let mut any_major: Option<(&Release, Version)> = None;
    for (r, v) in newer {
        if v.0 == cur.0 && same_major.is_none_or(|(_, best)| v > best) {
            same_major = Some((r, v));
        }
        if any_major.is_none_or(|(_, best)| v > best) {
            any_major = Some((r, v));
        }
    }
    let other_major = any_major.map(|(_, v)| v).filter(|v| v.0 != cur.0);
    let target = if allow_major { any_major } else { same_major };
    (target, other_major)
}

/// Installs over `exe` the newest stable release that is newer than `current` and within the
/// current major version (or the newest overall when `allow_major` is set).
///
/// Updates of the same `exe` are serialized with a lock file, and the installed version is
/// re-read under the lock, so overlapping runs can neither downgrade the binary nor back up an
/// already-updated binary over `.prev`.
pub fn check_and_install(
    agent: &ureq::Agent,
    api_base: &str,
    current: &str,
    exe: &Path,
    allow_major: bool,
) -> anyhow::Result<Outcome> {
    let cur = parse_version(current)
        .with_context(|| format!("cannot parse current version {current:?}"))?;
    let releases = fetch_releases(agent, api_base)?;
    let (target, other_major) = select_release(&releases, cur, allow_major);
    let Some((release, latest)) = target else {
        return Ok(match other_major {
            Some(v) => Outcome::MajorAvailable(fmt_version(v)),
            None => Outcome::UpToDate,
        });
    };
    if let Some(v) = other_major.filter(|_| !allow_major) {
        log::info!(
            "ferrous-comics {} (new major version) is also available; it is not installed automatically",
            fmt_version(v)
        );
    }
    let latest_str = fmt_version(latest);

    let _lock = match lock_exe(exe)? {
        Some(lock) => lock,
        None => return Ok(Outcome::InProgress),
    };
    let installed = installed_version(exe)?;
    if installed >= latest {
        log::info!(
            "{} is already at {} (updated by another run)",
            exe.display(),
            fmt_version(installed)
        );
        return Ok(Outcome::UpToDate);
    }

    let name = asset_name().context("no release binary is published for this platform")?;
    let find = |n: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name == n)
            .with_context(|| format!("release {} has no {n} asset", release.tag_name))
    };
    let bin_asset = find(name)?;
    let sums_asset = find(CHECKSUMS_ASSET)?;

    let sums = download(
        agent,
        &sums_asset.browser_download_url,
        "downloading SHA256SUMS",
    )?;
    let sums = String::from_utf8(sums).context("SHA256SUMS is not valid UTF-8")?;
    let expected = expected_sha256(&sums, name)
        .with_context(|| format!("SHA256SUMS has no entry for {name}"))?;

    let bytes = download(
        agent,
        &bin_asset.browser_download_url,
        "downloading release binary",
    )?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        bail!("checksum mismatch for {name}: expected {expected}, got {actual}");
    }
    // Independent second opinion: GitHub's own upload-time digest for the asset.
    if let Some(digest) = &bin_asset.digest {
        match digest.strip_prefix("sha256:") {
            Some(hex) if hex.eq_ignore_ascii_case(&actual) => {}
            Some(hex) => bail!("checksum mismatch for {name}: GitHub reports {hex}, got {actual}"),
            None => log::debug!("ignoring unsupported asset digest {digest:?}"),
        }
    }

    install(exe, &bytes, &latest_str)?;
    Ok(Outcome::Installed {
        from: current.to_string(),
        to: latest_str,
    })
}

/// How long a `version` query (self-check or installed-version re-check) may take.
const SELF_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// Takes an exclusive, non-blocking lock on `<exe>.update.lock`. Returns `None` if another
/// process holds it. The lock is released when the returned file is dropped.
fn lock_exe(exe: &Path) -> anyhow::Result<Option<std::fs::File>> {
    let path = PathBuf::from(format!("{}.update.lock", exe.display()));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("failed to open update lock {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("failed to lock {}", path.display()))
        }
    }
}

/// Version of the binary currently on disk at `exe`. This may differ from the running
/// process if another run already updated it.
fn installed_version(exe: &Path) -> anyhow::Result<Version> {
    let reported = query_version(exe, SELF_CHECK_TIMEOUT)?;
    parse_version(&reported).with_context(|| format!("cannot parse installed version {reported:?}"))
}

/// Writes `bytes` next to `exe`, verifies the new binary runs and reports `expected_version`,
/// keeps a copy of the current binary at `<exe>.prev`, then atomically renames the new binary
/// over `exe`. The running process keeps executing the old code; the new version takes effect
/// on the next run. Callers must hold the lock from [`lock_exe`].
fn install(exe: &Path, bytes: &[u8], expected_version: &str) -> anyhow::Result<()> {
    use std::io::Write;

    let dir = exe
        .parent()
        .context("executable path has no parent directory")?;
    let file_name = exe
        .file_name()
        .context("executable path has no file name")?
        .to_string_lossy();
    let pid = std::process::id();
    let tmp = dir.join(format!(".{file_name}.update-{pid}"));
    let prev = PathBuf::from(format!("{}.prev", exe.display()));
    let prev_tmp = dir.join(format!(".{file_name}.prev-{pid}"));
    let mode = std::fs::metadata(exe)
        .with_context(|| format!("failed to stat {}", exe.display()))?
        .permissions();

    // create_new fails on any existing file or symlink, so a temp path is never followed.
    // Only files this call created are removed on failure.
    let create_new = |path: &Path| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("failed to create {}", path.display()))
    };
    let mut created_tmp = false;
    let mut created_prev_tmp = false;

    let result = (|| {
        let mut f = create_new(&tmp)?;
        created_tmp = true;
        f.write_all(bytes).context("failed to write new binary")?;
        f.sync_all().context("failed to sync new binary")?;
        drop(f);
        set_executable(&tmp, mode.clone())?;

        self_check(&tmp, expected_version, SELF_CHECK_TIMEOUT)?;

        // Back up via temp + rename so a failed copy never clobbers an existing good `.prev`.
        let mut backup = create_new(&prev_tmp)?;
        created_prev_tmp = true;
        let mut current = std::fs::File::open(exe)
            .with_context(|| format!("failed to open {}", exe.display()))?;
        std::io::copy(&mut current, &mut backup)
            .and_then(|_| backup.sync_all())
            .with_context(|| format!("failed to back up current binary to {}", prev.display()))?;
        drop(backup);
        std::fs::set_permissions(&prev_tmp, mode.clone())
            .with_context(|| format!("failed to set permissions on {}", prev_tmp.display()))?;
        std::fs::rename(&prev_tmp, &prev)
            .with_context(|| format!("failed to back up current binary to {}", prev.display()))?;
        std::fs::rename(&tmp, exe)
            .with_context(|| format!("failed to replace {}", exe.display()))?;
        Ok(())
    })();

    if result.is_err() {
        if created_tmp {
            let _ = std::fs::remove_file(&tmp);
        }
        if created_prev_tmp {
            let _ = std::fs::remove_file(&prev_tmp);
        }
    }
    result
}

/// Runs `<bin> version` and checks it exits successfully within `timeout` and reports
/// `expected_version`. A binary that hangs is killed rather than blocking the caller forever.
fn self_check(bin: &Path, expected_version: &str, timeout: Duration) -> anyhow::Result<()> {
    let reported = query_version(bin, timeout).context("downloaded binary failed self-check")?;
    if reported != expected_version {
        bail!(
            "downloaded binary failed self-check: reported version {reported:?}, expected {expected_version}"
        );
    }
    Ok(())
}

/// Runs `<bin> version` and returns the version it reports (second word of its output).
/// Fails if it doesn't exit successfully within `timeout`; a hanging binary is killed.
fn query_version(bin: &Path, timeout: Duration) -> anyhow::Result<String> {
    use std::process::{Command, Stdio};

    // A concurrent fork in another thread can briefly hold a just-closed write fd,
    // making exec fail with ETXTBSY; retry a few times.
    let mut attempt = 0;
    let mut child = loop {
        let spawned = Command::new(bin)
            .arg("version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 10 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            r => break r.with_context(|| format!("failed to run {}", bin.display()))?,
        }
    };

    let deadline = std::time::Instant::now() + timeout;

    // Read stdout on a separate thread so a descendant process that inherits the pipe and
    // keeps it open can't block us past the deadline. If that happens the thread is left
    // blocked; it holds no locks and ends when the process exits.
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(out) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let res = out.take(64 * 1024).read_to_string(&mut buf).map(|_| buf);
            let _ = tx.send(res);
        });
    } else {
        let _ = tx.send(Ok(String::new()));
    }

    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("failed to wait for {}", bin.display()))?
        {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "`{} version` did not finish within {timeout:?}",
                bin.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let stdout = match rx.recv_timeout(remaining) {
        Ok(res) => res.with_context(|| format!("failed to read output of {}", bin.display()))?,
        Err(_) => bail!(
            "`{} version` exited but its output did not close within {timeout:?}",
            bin.display()
        ),
    };
    if !status.success() {
        bail!("`{} version` exited with {status}", bin.display());
    }
    Ok(stdout
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string())
}

/// Gives the new binary the old binary's permissions, making sure the owner can execute it.
#[cfg(unix)]
fn set_executable(path: &Path, perms: std::fs::Permissions) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = perms.mode() | 0o100;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .context("failed to mark new binary executable")
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _perms: std::fs::Permissions) -> anyhow::Result<()> {
    bail!("self-update is only supported on Unix")
}

/// Path of the running executable, with symlinks resolved so the real file is replaced.
pub fn current_exe() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().context("failed to locate running executable")?;
    exe.canonicalize()
        .with_context(|| format!("failed to resolve {}", exe.display()))
}

/// True when `stamp` is missing or older than `interval`.
pub(crate) fn check_due(stamp: &Path, interval: Duration) -> bool {
    match std::fs::metadata(stamp).and_then(|m| m.modified()) {
        Ok(modified) => SystemTime::now()
            .duration_since(modified)
            .map(|age| age >= interval)
            .unwrap_or(true), // stamp in the future (clock change) — check anyway
        Err(_) => true,
    }
}

/// Throttled, best-effort update used after a normal run when `XKCD_AUTO_UPDATE` is enabled.
/// The stamp file is touched before checking, so a persistently failing check is retried at
/// most once per interval. If the stamp can't be written (e.g. the database directory is
/// missing), the check still runs, unthrottled, so recovery from a broken release is not
/// blocked by the same fault.
pub fn auto_update(agent: &ureq::Agent, stamp: &Path) -> anyhow::Result<()> {
    let exe = current_exe()?;
    if let Some(outcome) = throttled_check(agent, GITHUB_API_BASE, CURRENT_VERSION, &exe, stamp)? {
        report(outcome);
    }
    Ok(())
}

/// Returns `None` when the check isn't due yet.
fn throttled_check(
    agent: &ureq::Agent,
    api_base: &str,
    current: &str,
    exe: &Path,
    stamp: &Path,
) -> anyhow::Result<Option<Outcome>> {
    if !check_due(stamp, AUTO_CHECK_INTERVAL) {
        log::debug!("update check not due yet ({})", stamp.display());
        return Ok(None);
    }
    if let Err(e) = std::fs::write(stamp, b"") {
        log::warn!(
            "failed to write update stamp {} ({e}); checking anyway",
            stamp.display()
        );
    }

    check_and_install(agent, api_base, current, exe, false).map(Some)
}

pub(crate) fn report(outcome: Outcome) {
    match outcome {
        Outcome::UpToDate => log::info!("ferrous-comics {CURRENT_VERSION} is up to date"),
        Outcome::MajorAvailable(v) => log::info!(
            "ferrous-comics {v} is available but is a new major version; review the changelog and run `ferrous-comics update --allow-major` to install it"
        ),
        Outcome::InProgress => log::info!("another update is in progress; skipping"),
        Outcome::Installed { from, to } => {
            log::info!("updated ferrous-comics {from} -> {to}; takes effect on the next run")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new().build()
    }

    // ── parse_version ─────────────────────────────────────────────────────────

    #[test]
    fn parse_version_accepts_plain_and_v_prefixed() {
        assert_eq!(parse_version("1.5.0"), Some((1, 5, 0)));
        assert_eq!(parse_version("v10.0.23"), Some((10, 0, 23)));
    }

    #[test]
    fn parse_version_rejects_malformed() {
        for s in ["", "1.5", "1.5.0.1", "1.5.0-rc1", "v", "a.b.c", "1..0"] {
            assert_eq!(parse_version(s), None, "{s:?}");
        }
    }

    // ── expected_sha256 ───────────────────────────────────────────────────────

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    #[test]
    fn expected_sha256_finds_exact_name() {
        let sums =
            format!("{HASH_A}  ferrous-comics-linux-arm64\n{HASH_B} *ferrous-comics-linux-amd64\n");
        assert_eq!(
            expected_sha256(&sums, "ferrous-comics-linux-amd64"),
            Some(HASH_B.to_ascii_lowercase())
        );
        assert_eq!(
            expected_sha256(&sums, "ferrous-comics-linux-arm64"),
            Some(HASH_A.to_string())
        );
    }

    #[test]
    fn expected_sha256_rejects_missing_and_malformed() {
        let sums = format!("{HASH_A}  ferrous-comics-linux-amd64.sig\nnothex  ferrous-comics\n");
        assert_eq!(expected_sha256(&sums, "ferrous-comics-linux-amd64"), None);
        assert_eq!(expected_sha256(&sums, "ferrous-comics"), None);
        assert_eq!(expected_sha256("", "x"), None);
    }

    // ── check_due ─────────────────────────────────────────────────────────────

    #[test]
    fn check_due_when_stamp_missing_or_stale() {
        let dir = tempfile::TempDir::new().unwrap();
        let stamp = dir.path().join("stamp");
        assert!(check_due(&stamp, Duration::from_secs(60)));
        std::fs::write(&stamp, b"").unwrap();
        assert!(!check_due(&stamp, Duration::from_secs(3600)));
        assert!(check_due(&stamp, Duration::ZERO));
    }

    // ── check_and_install ─────────────────────────────────────────────────────

    /// A stand-in release binary: a shell script that answers `version` like the real one.
    fn fake_binary(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho \"ferrous-comics {version} (test)\"\n").into_bytes()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        exe: PathBuf,
        server: mockito::ServerGuard,
    }

    /// The "currently installed" binary in fixtures: answers `version` with 1.5.0.
    fn old_binary() -> Vec<u8> {
        fake_binary("1.5.0")
    }

    fn write_executable(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn fixture() -> Fixture {
        let dir = tempfile::TempDir::new().unwrap();
        let exe = dir.path().join("ferrous-comics");
        write_executable(&exe, &old_binary());
        Fixture {
            _dir: dir,
            exe,
            server: mockito::Server::new(),
        }
    }

    fn mock_release(f: &mut Fixture, tag: &str, binary: &[u8], sums_hash: Option<&str>) {
        let digest = format!("sha256:{}", sha256_hex(binary));
        mock_release_with_digest(f, tag, binary, sums_hash, Some(&digest));
    }

    fn mock_release_with_digest(
        f: &mut Fixture,
        tag: &str,
        binary: &[u8],
        sums_hash: Option<&str>,
        digest: Option<&str>,
    ) {
        let name = asset_name().unwrap_or("ferrous-comics-unsupported-platform");
        let base = f.server.url();
        let mut bin_asset = serde_json::json!({
            "name": name, "browser_download_url": format!("{base}/dl/bin")
        });
        if let Some(d) = digest {
            bin_asset["digest"] = d.into();
        }
        let mut assets = vec![bin_asset];
        if let Some(hash) = sums_hash {
            assets.push(serde_json::json!({
                "name": "SHA256SUMS", "browser_download_url": format!("{base}/dl/sums")
            }));
            f.server
                .mock("GET", "/dl/sums")
                .with_body(format!("{hash}  {name}\n"))
                .create();
        }
        let release = serde_json::json!({ "tag_name": tag, "assets": assets });
        mock_release_list(f, vec![release]);
        f.server.mock("GET", "/dl/bin").with_body(binary).create();
    }

    fn mock_release_list(f: &mut Fixture, releases: Vec<serde_json::Value>) {
        f.server
            .mock("GET", "/releases")
            .match_query(mockito::Matcher::UrlEncoded(
                "per_page".into(),
                "100".into(),
            ))
            .with_body(serde_json::Value::Array(releases).to_string())
            .create();
    }

    #[test]
    fn up_to_date_when_latest_not_newer() {
        for tag in ["v1.5.0", "v1.4.9"] {
            let mut f = fixture();
            mock_release(&mut f, tag, b"", None);
            let out = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap();
            assert_eq!(out, Outcome::UpToDate, "{tag}");
            assert_eq!(std::fs::read(&f.exe).unwrap(), old_binary());
        }
    }

    #[test]
    fn major_not_installed_without_opt_in() {
        let mut f = fixture();
        mock_release(&mut f, "v2.0.0", b"", None);
        let out = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap();
        assert_eq!(out, Outcome::MajorAvailable("2.0.0".into()));
        assert_eq!(std::fs::read(&f.exe).unwrap(), old_binary());
    }

    #[cfg(unix)]
    #[test]
    fn installs_verified_minor_update_and_keeps_prev() {
        if asset_name().is_none() {
            return; // no release asset for this platform
        }
        let mut f = fixture();
        let bin = fake_binary("1.6.0");
        mock_release(&mut f, "v1.6.0", &bin, Some(&sha256_hex(&bin)));

        let out = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap();

        assert_eq!(
            out,
            Outcome::Installed {
                from: "1.5.0".into(),
                to: "1.6.0".into()
            }
        );
        assert_eq!(std::fs::read(&f.exe).unwrap(), bin);
        let prev = PathBuf::from(format!("{}.prev", f.exe.display()));
        assert_eq!(std::fs::read(prev).unwrap(), old_binary());
        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(f.exe.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".update-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn installs_major_with_opt_in() {
        if asset_name().is_none() {
            return; // no release asset for this platform
        }
        let mut f = fixture();
        let bin = fake_binary("2.0.0");
        mock_release(&mut f, "v2.0.0", &bin, Some(&sha256_hex(&bin)));
        let out = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, true).unwrap();
        assert!(matches!(out, Outcome::Installed { .. }));
        assert_eq!(std::fs::read(&f.exe).unwrap(), bin);
    }

    #[test]
    fn rejects_checksum_mismatch() {
        if asset_name().is_none() {
            return; // no release asset for this platform
        }
        let mut f = fixture();
        let bin = fake_binary("1.6.0");
        mock_release(&mut f, "v1.6.0", &bin, Some(&sha256_hex(b"something else")));
        let err = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        assert_eq!(std::fs::read(&f.exe).unwrap(), old_binary());
    }

    #[test]
    fn rejects_github_digest_mismatch() {
        if asset_name().is_none() {
            return; // no release asset for this platform
        }
        let mut f = fixture();
        let bin = fake_binary("1.6.0");
        let wrong = format!("sha256:{}", sha256_hex(b"something else"));
        mock_release_with_digest(
            &mut f,
            "v1.6.0",
            &bin,
            Some(&sha256_hex(&bin)),
            Some(&wrong),
        );
        let err = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap_err();
        assert!(format!("{err:#}").contains("GitHub reports"), "{err:#}");
        assert_eq!(std::fs::read(&f.exe).unwrap(), old_binary());
    }

    #[cfg(unix)]
    #[test]
    fn installs_when_github_digest_absent() {
        if asset_name().is_none() {
            return; // no release asset for this platform
        }
        let mut f = fixture();
        let bin = fake_binary("1.6.0");
        mock_release_with_digest(&mut f, "v1.6.0", &bin, Some(&sha256_hex(&bin)), None);
        let out = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap();
        assert!(matches!(out, Outcome::Installed { .. }));
    }

    #[test]
    fn rejects_release_without_checksums() {
        if asset_name().is_none() {
            return; // no release asset for this platform
        }
        let mut f = fixture();
        mock_release(&mut f, "v1.6.0", &fake_binary("1.6.0"), None);
        let err = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap_err();
        assert!(format!("{err:#}").contains("SHA256SUMS"), "{err:#}");
        assert_eq!(std::fs::read(&f.exe).unwrap(), old_binary());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_binary_reporting_wrong_version() {
        if asset_name().is_none() {
            return; // no release asset for this platform
        }
        let mut f = fixture();
        let bin = fake_binary("1.5.9"); // checksum matches, but it isn't the advertised release
        mock_release(&mut f, "v1.6.0", &bin, Some(&sha256_hex(&bin)));
        let err = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap_err();
        assert!(format!("{err:#}").contains("self-check"), "{err:#}");
        assert_eq!(std::fs::read(&f.exe).unwrap(), old_binary());
        assert!(!PathBuf::from(format!("{}.prev", f.exe.display())).exists());
    }

    #[test]
    fn api_error_is_reported() {
        let mut f = fixture();
        f.server
            .mock("GET", "/releases")
            .match_query(mockito::Matcher::Any)
            .with_status(403)
            .create();
        let err = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap_err();
        assert!(format!("{err:#}").contains("HTTP 403"), "{err:#}");
    }

    #[test]
    fn unparseable_and_prerelease_tags_are_ignored() {
        let mut f = fixture();
        mock_release_list(
            &mut f,
            vec![
                serde_json::json!({ "tag_name": "nightly", "assets": [] }),
                serde_json::json!({ "tag_name": "v1.6.0-rc1", "assets": [] }),
                serde_json::json!({ "tag_name": "v1.7.0", "prerelease": true, "assets": [] }),
                serde_json::json!({ "tag_name": "v1.8.0", "draft": true, "assets": [] }),
            ],
        );
        let out = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap();
        assert_eq!(out, Outcome::UpToDate);
    }

    // ── self_check ────────────────────────────────────────────────────────────

    #[cfg(unix)]
    fn script(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("candidate");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn self_check_accepts_matching_version() {
        let dir = tempfile::TempDir::new().unwrap();
        let bin = script(dir.path(), "echo 'ferrous-comics 1.6.0 (abc)'");
        self_check(&bin, "1.6.0", Duration::from_secs(10)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn self_check_kills_hanging_binary() {
        let dir = tempfile::TempDir::new().unwrap();
        let bin = script(dir.path(), "sleep 30");
        let start = std::time::Instant::now();
        let err = self_check(&bin, "1.6.0", Duration::from_millis(300)).unwrap_err();
        assert!(format!("{err:#}").contains("did not finish"), "{err:#}");
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn self_check_rejects_nonzero_exit() {
        let dir = tempfile::TempDir::new().unwrap();
        let bin = script(dir.path(), "echo 'ferrous-comics 1.6.0 (abc)'; exit 1");
        assert!(self_check(&bin, "1.6.0", Duration::from_secs(10)).is_err());
    }

    // ── select_release ────────────────────────────────────────────────────────

    fn rel(tag: &str) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            assets: vec![],
        }
    }

    #[test]
    fn select_prefers_newest_in_current_major_when_newer_major_is_latest() {
        let releases = [rel("v2.0.0"), rel("v1.7.0"), rel("v1.6.0"), rel("v1.4.0")];
        let (target, other) = select_release(&releases, (1, 5, 0), false);
        assert_eq!(target.map(|(_, v)| v), Some((1, 7, 0)));
        assert_eq!(other, Some((2, 0, 0)));
    }

    #[test]
    fn select_with_allow_major_takes_newest_overall() {
        let releases = [rel("v1.7.0"), rel("v2.1.0"), rel("v2.0.0")];
        let (target, other) = select_release(&releases, (1, 5, 0), true);
        assert_eq!(target.map(|(_, v)| v), Some((2, 1, 0)));
        assert_eq!(other, Some((2, 1, 0)));
    }

    #[test]
    fn select_nothing_newer() {
        let releases = [rel("v1.5.0"), rel("v1.4.0")];
        assert!(matches!(
            select_release(&releases, (1, 5, 0), false),
            (None, None)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn installs_in_major_fix_when_newer_major_is_latest() {
        if asset_name().is_none() {
            return;
        }
        let mut f = fixture();
        let bin = fake_binary("1.6.0");
        let name = asset_name().unwrap();
        let base = f.server.url();
        f.server.mock("GET", "/dl/bin").with_body(&bin).create();
        f.server
            .mock("GET", "/dl/sums")
            .with_body(format!("{}  {name}\n", sha256_hex(&bin)))
            .create();
        // v2.0.0 is the newest release overall; only v1.6.0 is in the current major.
        let v20 = serde_json::json!({ "tag_name": "v2.0.0", "assets": [] });
        let v16 = serde_json::json!({ "tag_name": "v1.6.0", "assets": [
            { "name": name, "browser_download_url": format!("{base}/dl/bin") },
            { "name": "SHA256SUMS", "browser_download_url": format!("{base}/dl/sums") },
        ]});
        mock_release_list(&mut f, vec![v20, v16]);

        let out = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap();
        assert_eq!(
            out,
            Outcome::Installed {
                from: "1.5.0".into(),
                to: "1.6.0".into()
            }
        );
    }

    // ── concurrency / backup safety ───────────────────────────────────────────

    #[test]
    fn skips_when_another_update_holds_lock() {
        let mut f = fixture();
        let bin = fake_binary("1.6.0");
        mock_release(&mut f, "v1.6.0", &bin, Some(&sha256_hex(&bin)));
        let _held = lock_exe(&f.exe).unwrap().expect("lock is free");
        let out = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap();
        assert_eq!(out, Outcome::InProgress);
        assert_eq!(std::fs::read(&f.exe).unwrap(), old_binary());
    }

    #[cfg(unix)]
    #[test]
    fn skips_when_installed_binary_already_updated() {
        let mut f = fixture();
        // Another run already replaced the binary with 1.6.0, but this process is still 1.5.0.
        write_executable(&f.exe, &fake_binary("1.6.0"));
        let bin = fake_binary("1.6.0");
        mock_release(&mut f, "v1.6.0", &bin, Some(&sha256_hex(&bin)));
        let out = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap();
        assert_eq!(out, Outcome::UpToDate);
        assert!(!PathBuf::from(format!("{}.prev", f.exe.display())).exists());
    }

    #[cfg(unix)]
    #[test]
    fn install_refuses_planted_symlink_at_backup_temp_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let exe = dir.path().join("ferrous-comics");
        write_executable(&exe, &old_binary());
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"precious").unwrap();
        let planted = dir
            .path()
            .join(format!(".ferrous-comics.prev-{}", std::process::id()));
        std::os::unix::fs::symlink(&victim, &planted).unwrap();

        assert!(install(&exe, &fake_binary("1.6.0"), "1.6.0").is_err());

        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        assert!(
            planted.symlink_metadata().is_ok(),
            "planted file must not be removed"
        );
        assert_eq!(std::fs::read(&exe).unwrap(), old_binary());
    }

    #[cfg(unix)]
    #[test]
    fn self_check_not_blocked_by_descendant_holding_stdout() {
        let dir = tempfile::TempDir::new().unwrap();
        // Prints the version, then leaves a background process holding stdout open.
        let bin = script(dir.path(), "echo 'ferrous-comics 1.6.0 (abc)'; sleep 30 &");
        let start = std::time::Instant::now();
        let res = self_check(&bin, "1.6.0", Duration::from_millis(500));
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "blocked on inherited stdout"
        );
        let err = res.unwrap_err();
        assert!(format!("{err:#}").contains("did not close"), "{err:#}");
    }

    #[test]
    fn throttled_check_runs_and_stamps_once_per_interval() {
        let mut f = fixture();
        mock_release_list(&mut f, vec![]);
        let stamp = f.exe.with_file_name("db.update-check");
        let first = throttled_check(&agent(), &f.server.url(), "1.5.0", &f.exe, &stamp).unwrap();
        assert_eq!(first, Some(Outcome::UpToDate));
        assert!(stamp.exists());
        let second = throttled_check(&agent(), &f.server.url(), "1.5.0", &f.exe, &stamp).unwrap();
        assert_eq!(
            second, None,
            "second check within the interval must be skipped"
        );
    }

    #[test]
    fn throttled_check_still_runs_when_stamp_unwritable() {
        let mut f = fixture();
        let list = f
            .server
            .mock("GET", "/releases")
            .match_query(mockito::Matcher::Any)
            .with_body("[]")
            .expect(1)
            .create();
        // Parent directory doesn't exist, so the stamp can't be written.
        let stamp = f.exe.with_file_name("missing-dir").join("db.update-check");
        let out = throttled_check(&agent(), &f.server.url(), "1.5.0", &f.exe, &stamp).unwrap();
        assert_eq!(out, Some(Outcome::UpToDate));
        list.assert();
    }
}
