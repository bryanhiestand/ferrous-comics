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

fn fetch_latest(agent: &ureq::Agent, api_base: &str) -> anyhow::Result<Release> {
    let url = format!("{}/releases/latest", api_base.trim_end_matches('/'));
    get(agent, &url, "fetching latest release")?
        .into_json::<Release>()
        .context("failed to parse latest release JSON")
}

/// Checks the latest release and installs it over `exe` if it is newer and within the current
/// major version (or any newer version when `allow_major` is set).
pub fn check_and_install(
    agent: &ureq::Agent,
    api_base: &str,
    current: &str,
    exe: &Path,
    allow_major: bool,
) -> anyhow::Result<Outcome> {
    let cur = parse_version(current)
        .with_context(|| format!("cannot parse current version {current:?}"))?;
    let release = fetch_latest(agent, api_base)?;
    let latest = parse_version(&release.tag_name)
        .with_context(|| format!("cannot parse release tag {:?}", release.tag_name))?;
    let latest_str = format!("{}.{}.{}", latest.0, latest.1, latest.2);

    if latest <= cur {
        log::debug!("latest release {latest_str} is not newer than {current}");
        return Ok(Outcome::UpToDate);
    }
    if latest.0 != cur.0 && !allow_major {
        return Ok(Outcome::MajorAvailable(latest_str));
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

/// How long the downloaded binary's `version` self-check may take before it is killed.
const SELF_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// Writes `bytes` next to `exe`, verifies the new binary runs and reports `expected_version`,
/// keeps a copy of the current binary at `<exe>.prev`, then atomically renames the new binary
/// over `exe`. The running process keeps executing the old code; the new version takes effect
/// on the next run.
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

    let result = (|| {
        // create_new: never follow a pre-existing file or symlink at the temp path.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        f.write_all(bytes).context("failed to write new binary")?;
        f.sync_all().context("failed to sync new binary")?;
        drop(f);
        set_executable(&tmp, mode.clone())?;

        self_check(&tmp, expected_version, SELF_CHECK_TIMEOUT)?;

        // Back up via temp + rename so a failed copy never clobbers an existing good `.prev`.
        std::fs::copy(exe, &prev_tmp)
            .with_context(|| format!("failed to back up current binary to {}", prev.display()))?;
        std::fs::rename(&prev_tmp, &prev)
            .with_context(|| format!("failed to back up current binary to {}", prev.display()))?;
        std::fs::rename(&tmp, exe)
            .with_context(|| format!("failed to replace {}", exe.display()))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&prev_tmp);
    }
    result
}

/// Runs `<bin> version` and checks it exits successfully within `timeout` and reports
/// `expected_version`. A binary that hangs is killed rather than blocking the caller forever.
fn self_check(bin: &Path, expected_version: &str, timeout: Duration) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};

    // A concurrent fork in another thread can briefly hold our just-closed write fd,
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
            r => break r.context("failed to run downloaded binary")?,
        }
    };

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .context("failed to wait for downloaded binary")?
        {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "downloaded binary failed self-check: `version` did not finish within {timeout:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_string(&mut stdout)
            .context("failed to read downloaded binary output")?;
    }
    let reported = stdout.split_whitespace().nth(1).unwrap_or_default();
    if !status.success() || reported != expected_version {
        bail!(
            "downloaded binary failed self-check (exit {status}, reported version {reported:?}, expected {expected_version})"
        );
    }
    Ok(())
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
/// most once per interval.
pub fn auto_update(agent: &ureq::Agent, stamp: &Path) -> anyhow::Result<()> {
    if !check_due(stamp, AUTO_CHECK_INTERVAL) {
        log::debug!("update check not due yet ({})", stamp.display());
        return Ok(());
    }
    std::fs::write(stamp, b"")
        .with_context(|| format!("failed to write update stamp {}", stamp.display()))?;

    let exe = current_exe()?;
    report(check_and_install(
        agent,
        GITHUB_API_BASE,
        CURRENT_VERSION,
        &exe,
        false,
    )?);
    Ok(())
}

pub(crate) fn report(outcome: Outcome) {
    match outcome {
        Outcome::UpToDate => log::info!("ferrous-comics {CURRENT_VERSION} is up to date"),
        Outcome::MajorAvailable(v) => log::info!(
            "ferrous-comics {v} is available but is a new major version; review the changelog and run `ferrous-comics update --allow-major` to install it"
        ),
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

    fn fixture() -> Fixture {
        let dir = tempfile::TempDir::new().unwrap();
        let exe = dir.path().join("ferrous-comics");
        std::fs::write(&exe, b"old binary").unwrap();
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
        f.server
            .mock("GET", "/releases/latest")
            .with_body(serde_json::json!({ "tag_name": tag, "assets": assets }).to_string())
            .create();
        f.server.mock("GET", "/dl/bin").with_body(binary).create();
    }

    #[test]
    fn up_to_date_when_latest_not_newer() {
        for tag in ["v1.5.0", "v1.4.9"] {
            let mut f = fixture();
            mock_release(&mut f, tag, b"", None);
            let out = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap();
            assert_eq!(out, Outcome::UpToDate, "{tag}");
            assert_eq!(std::fs::read(&f.exe).unwrap(), b"old binary");
        }
    }

    #[test]
    fn major_not_installed_without_opt_in() {
        let mut f = fixture();
        mock_release(&mut f, "v2.0.0", b"", None);
        let out = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap();
        assert_eq!(out, Outcome::MajorAvailable("2.0.0".into()));
        assert_eq!(std::fs::read(&f.exe).unwrap(), b"old binary");
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
        assert_eq!(std::fs::read(prev).unwrap(), b"old binary");
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
        assert_eq!(std::fs::read(&f.exe).unwrap(), b"old binary");
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
        assert_eq!(std::fs::read(&f.exe).unwrap(), b"old binary");
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
        assert_eq!(std::fs::read(&f.exe).unwrap(), b"old binary");
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
        assert_eq!(std::fs::read(&f.exe).unwrap(), b"old binary");
        assert!(!PathBuf::from(format!("{}.prev", f.exe.display())).exists());
    }

    #[test]
    fn api_error_is_reported() {
        let mut f = fixture();
        f.server
            .mock("GET", "/releases/latest")
            .with_status(403)
            .create();
        let err = check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).unwrap_err();
        assert!(format!("{err:#}").contains("HTTP 403"), "{err:#}");
    }

    #[test]
    fn unparseable_tag_is_error() {
        let mut f = fixture();
        mock_release(&mut f, "nightly", b"", None);
        assert!(check_and_install(&agent(), &f.server.url(), "1.5.0", &f.exe, false).is_err());
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
}
