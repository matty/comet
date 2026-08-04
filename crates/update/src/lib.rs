//! comet-update — release checking and self-update, shared by the engine (the
//! background checker + `ApplyUpdate`), the CLI (`comet update`), and the UI
//! (the sidebar update strip + macOS bundle swap).
//!
//! Release layout (see `.github/workflows/release.yml` and `edge/src/install.sh`):
//! artifacts live in the `comet-native-releases` R2 bucket, served publicly at
//! `{releases_url}/releases/*`. `manifest.json` carries the latest version plus a
//! repository identity and a sha256 per artifact. Manifests from another fork,
//! and legacy metadata without repository provenance, are rejected.
//!
//! Install kinds and their update paths:
//! - **Managed** (`~/.comet-native/app/<ver>` + `current` symlink — the curl|sh
//!   installer): download the headless tarball into a new versioned dir, flip
//!   the symlink, restart the service. Same flow the installer script performs,
//!   natively.
//! - **MacApp** (running out of `Comet.app`): download the app tarball, swap the
//!   bundle directory, relaunch. Driven by the UI.
//! - **Unmanaged** (source builds, hand-copied binaries): report only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, bail};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::watch;

/// Public release distribution endpoint. It serves installer and signed release
/// metadata/artifacts only; local and LAN operation never depend on it.
pub const DEFAULT_RELEASES_URL: &str = "https://comet.zeron.sh";
/// Repository identity every official endpoint or intentional mirror must
/// publish in its release manifest.
pub const EXPECTED_RELEASE_REPOSITORY: &str = "matty/comet";

/// Resolve the optional release-distribution override from the process
/// environment. No removed runtime/edge variable participates in this lookup.
pub fn releases_url_from_env() -> String {
    releases_url_from(|key| std::env::var(key).ok())
}

fn releases_url_from(getenv: impl Fn(&str) -> Option<String>) -> String {
    getenv("COMET_RELEASES_URL")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASES_URL.to_string())
}

/// The version compiled into this binary (the workspace version).
pub const fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Background check cadence.
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
/// Retry sooner after a failed check (offline boot, transient distribution error).
const CHECK_RETRY: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// First check waits out engine boot (room joins, doc re-sync).
const CHECK_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(20);
/// While an auto-apply is deferred behind active sessions, re-probe idleness
/// this often.
const IDLE_RECHECK: std::time::Duration = std::time::Duration::from_secs(5 * 60);

// ---------------------------------------------------------------------------
// Release metadata
// ---------------------------------------------------------------------------

/// `{releases_url}/releases/manifest.json` — written by the release workflow.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// GitHub `owner/repository` that built these artifacts.
    #[serde(default)]
    pub repository: String,
    pub version: String,
    /// Artifact file name → checksum metadata.
    #[serde(default)]
    pub files: BTreeMap<String, FileMeta>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileMeta {
    #[serde(default)]
    pub sha256: Option<String>,
}

fn expected_release_sha256<'a>(manifest: &'a Manifest, file: &str) -> anyhow::Result<&'a str> {
    let metadata = manifest
        .files
        .get(file)
        .with_context(|| format!("release manifest is missing artifact metadata for {file}"))?;
    let checksum = metadata
        .sha256
        .as_deref()
        .with_context(|| format!("release manifest is missing SHA-256 for {file}"))?;
    if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("release manifest has an invalid SHA-256 for {file}");
    }
    Ok(checksum)
}

const STAGE_MARKER_FILE: &str = ".comet-release";

fn validated_release_version(manifest: &Manifest) -> anyhow::Result<&str> {
    if manifest.repository != EXPECTED_RELEASE_REPOSITORY {
        bail!(
            "release repository mismatch: expected {EXPECTED_RELEASE_REPOSITORY}, got {}",
            manifest.repository
        );
    }
    let version = manifest.version.trim();
    if version_parts(version).is_none() {
        bail!("release manifest has an invalid dotted-numeric version");
    }
    Ok(version)
}

fn stage_marker(manifest: &Manifest, file: &str) -> anyhow::Result<String> {
    let version = validated_release_version(manifest)?;
    let checksum = expected_release_sha256(manifest, file)?;
    Ok(format!(
        "repository={EXPECTED_RELEASE_REPOSITORY}\nversion={version}\nartifact={file}\nsha256={}\n",
        checksum.to_ascii_lowercase()
    ))
}

fn stage_marker_matches(directory: &Path, expected: &str) -> bool {
    std::fs::read_to_string(directory.join(STAGE_MARKER_FILE)).is_ok_and(|value| value == expected)
}

/// Artifact-name platform pair — `uname`-style strings matching the packaging
/// scripts: `linux-x86_64`, `linux-aarch64`, `macos-arm64`.
pub fn platform_key() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let arch = match (os, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64",
        (_, arch) => arch,
    };
    (os, arch)
}

/// `comet-<ver>-<os>-<arch>.tar.gz` — the headless/CLI tarball (Linux CI builds).
pub fn headless_artifact(version: &str) -> String {
    let (os, arch) = platform_key();
    format!("comet-{version}-{os}-{arch}.tar.gz")
}

/// `comet-<ver>-macos-<arch>-app.tar.gz` — the packaged `Comet.app` bundle.
pub fn mac_app_artifact(version: &str) -> String {
    let (_, arch) = platform_key();
    format!("comet-{version}-macos-{arch}-app.tar.gz")
}

/// Strictly-newer dotted-numeric compare (`0.1.10` > `0.1.9` > `0.1`).
/// Unparseable versions never count as newer — malformed release metadata must
/// not trigger an update loop.
pub fn version_newer(latest: &str, current: &str) -> bool {
    match (version_parts(latest), version_parts(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn version_parts(version: &str) -> Option<Vec<u64>> {
    let parts: Vec<u64> = version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.parse().ok())
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then_some(parts)
}

fn parse_manifest(bytes: &[u8]) -> anyhow::Result<Manifest> {
    let mut manifest: Manifest = serde_json::from_slice(bytes).context("parsing manifest.json")?;
    if manifest.repository.trim().is_empty() {
        bail!("release repository is missing; expected {EXPECTED_RELEASE_REPOSITORY}");
    }
    if manifest.repository != EXPECTED_RELEASE_REPOSITORY {
        bail!(
            "release repository mismatch: expected {EXPECTED_RELEASE_REPOSITORY}, got {}",
            manifest.repository
        );
    }
    let version = manifest.version.trim();
    if version_parts(version).is_none() {
        bail!("manifest.json has an invalid dotted-numeric version");
    }
    manifest.version = version.to_string();
    Ok(manifest)
}

/// Fetch the newest release metadata. Repository provenance is mandatory; a
/// missing, malformed, or mismatched manifest is never replaced with legacy
/// unprovenanced version-only metadata.
pub async fn fetch_latest(releases_url: &str) -> anyhow::Result<Manifest> {
    let base = releases_url.trim_end_matches('/');
    let client = http_client()?;
    let manifest_url = format!("{base}/releases/manifest.json");
    let bytes = client
        .get(&manifest_url)
        .send()
        .await
        .context("fetching manifest.json")?
        .error_for_status()
        .context("fetching manifest.json")?
        .bytes()
        .await
        .context("reading manifest.json")?;
    parse_manifest(&bytes)
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("comet/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building http client")
}

// ---------------------------------------------------------------------------
// Install-kind detection
// ---------------------------------------------------------------------------

/// How this binary was installed — decides the update path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// `~/.comet-native/app/<ver>/comet` behind the `current` symlink
    /// (curl|sh installer / a previous `comet update`).
    Managed { app_root: PathBuf },
    /// Running out of a macOS `.app` bundle.
    MacApp { bundle: PathBuf },
    /// Source build or hand-copied binary — updates are report-only.
    Unmanaged,
}

pub fn detect_install() -> InstallKind {
    let Ok(exe) = std::env::current_exe() else {
        return InstallKind::Unmanaged;
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    detect_install_from(&exe, home.as_deref())
}

fn detect_install_from(exe: &Path, home: Option<&Path>) -> InstallKind {
    if let Some(home) = home {
        // `current_exe` resolves the `current` symlink to the versioned dir.
        let app_root = home.join(".comet-native").join("app");
        if exe.starts_with(&app_root) {
            return InstallKind::Managed { app_root };
        }
    }
    for ancestor in exe.ancestors() {
        if ancestor.extension().is_some_and(|ext| ext == "app")
            && exe.starts_with(ancestor.join("Contents").join("MacOS"))
        {
            return InstallKind::MacApp {
                bundle: ancestor.to_path_buf(),
            };
        }
    }
    InstallKind::Unmanaged
}

// ---------------------------------------------------------------------------
// Download + verify
// ---------------------------------------------------------------------------

/// Stream `{releases_url}/releases/<file>` to `dest`, verifying the manifest sha256 when
/// present. Writes through a `.partial` sidecar so an interrupted download never
/// leaves a plausible-looking artifact behind.
pub async fn download_release_file(
    releases_url: &str,
    manifest: &Manifest,
    file: &str,
    dest: &Path,
) -> anyhow::Result<()> {
    let url = format!("{}/releases/{file}", releases_url.trim_end_matches('/'));
    let expected = expected_release_sha256(manifest, file)?;
    let partial = dest.with_extension("partial");
    let resp = http_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    let mut out = tokio::fs::File::create(&partial)
        .await
        .with_context(|| format!("creating {}", partial.display()))?;
    let mut hasher = Sha256::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading download stream")?;
        hasher.update(&chunk);
        out.write_all(&chunk).await.context("writing download")?;
    }
    out.flush().await.ok();
    drop(out);
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        tokio::fs::remove_file(&partial).await.ok();
        bail!("checksum mismatch for {file}: expected {expected}, got {actual}");
    }
    tokio::fs::rename(&partial, dest)
        .await
        .with_context(|| format!("moving {} into place", dest.display()))?;
    Ok(())
}

fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Managed (symlink) installs — the daemon/VPS path
// ---------------------------------------------------------------------------

/// Download + unpack the headless tarball into `app_root/<ver>` (idempotent —
/// an already-staged version is reused). Returns the versioned dir.
pub async fn stage_headless(
    releases_url: &str,
    manifest: &Manifest,
    app_root: &Path,
) -> anyhow::Result<PathBuf> {
    let version = validated_release_version(manifest)?;
    let dest = app_root.join(version);
    let file = headless_artifact(version);
    let marker = stage_marker(manifest, &file)?;
    if dest.join("comet").exists() {
        if stage_marker_matches(&dest, &marker) {
            return Ok(dest);
        }
        bail!(
            "existing staged release {} is unverified; remove it before retrying",
            dest.display()
        );
    }
    let stage = app_root.join(format!(".stage-{version}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).with_context(|| format!("creating {}", stage.display()))?;
    let result = async {
        let tarball = stage.join(&file);
        download_release_file(releases_url, manifest, &file, &tarball).await?;
        let unpacked = stage.join("unpacked");
        std::fs::create_dir_all(&unpacked)?;
        // Tarball root is the versioned stage dir (see scripts/package-linux.sh);
        // strip it exactly as install.sh does.
        run(
            "tar",
            &[
                "-xzf",
                &tarball.to_string_lossy(),
                "-C",
                &unpacked.to_string_lossy(),
                "--strip-components=1",
            ],
        )?;
        if !unpacked.join("comet").is_file() {
            bail!("tarball {file} did not contain a comet binary");
        }
        std::fs::write(unpacked.join(STAGE_MARKER_FILE), &marker)
            .context("writing release verification marker")?;
        match std::fs::rename(&unpacked, &dest) {
            Ok(()) => {}
            // Lost a race with another stager — the staged copy is equivalent.
            Err(_) if dest.join("comet").exists() => {}
            Err(err) => {
                return Err(err).with_context(|| format!("moving {} into place", dest.display()));
            }
        }
        Ok(dest.clone())
    }
    .await;
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// Atomically repoint `app_root/current` at `app_root/<ver>` (symlink to a temp
/// name, then rename over — never a window with no `current`).
pub fn apply_headless(app_root: &Path, version: &str) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let target = app_root.join(version);
        if !target.join("comet").exists() {
            bail!("{} is not a staged install", target.display());
        }
        let tmp = app_root.join(format!(".current-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&target, &tmp).context("creating current symlink")?;
        std::fs::rename(&tmp, app_root.join("current")).context("swapping current symlink")?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (app_root, version);
        bail!("managed installs are unix-only");
    }
}

/// Restart the installed engine service (the same units `comet daemon` and the
/// curl|sh installer manage). Called after a symlink swap so the running daemon
/// picks up the new binary.
pub fn restart_service() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        let output = std::process::Command::new("id").arg("-u").output()?;
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        run(
            "launchctl",
            &["kickstart", "-k", &format!("gui/{uid}/sh.zeron.comet")],
        )
    } else {
        run("systemctl", &["--user", "restart", "comet-native.service"])
    }
}

// ---------------------------------------------------------------------------
// macOS app-bundle installs — the desktop path
// ---------------------------------------------------------------------------

/// Download + unpack the app tarball into `{data_dir}/updates/<ver>/Comet.app`
/// (idempotent). Returns the staged bundle path.
pub async fn stage_mac_app(
    releases_url: &str,
    manifest: &Manifest,
    data_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let version = validated_release_version(manifest)?;
    let dir = data_dir.join("updates").join(version);
    let staged = dir.join("Comet.app");
    let file = mac_app_artifact(version);
    let marker = stage_marker(manifest, &file)?;
    if staged.join("Contents/MacOS/comet").exists() {
        if stage_marker_matches(&dir, &marker) {
            return Ok(staged);
        }
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("removing unverified stage {}", dir.display()))?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tarball = dir.join(&file);
    download_release_file(releases_url, manifest, &file, &tarball).await?;
    run(
        "tar",
        &[
            "-xzf",
            &tarball.to_string_lossy(),
            "-C",
            &dir.to_string_lossy(),
        ],
    )?;
    std::fs::remove_file(&tarball).ok();
    if !staged.join("Contents/MacOS/comet").exists() {
        bail!("app tarball {file} did not contain Comet.app");
    }
    std::fs::write(dir.join(STAGE_MARKER_FILE), marker)
        .context("writing release verification marker")?;
    Ok(staged)
}

/// Swap the installed bundle for the staged one: `ditto` the staged copy next to
/// the target (metadata-preserving, cross-volume safe), then two renames — the
/// old bundle is restored if the second rename fails.
pub fn apply_mac_app(staged: &Path, bundle: &Path) -> anyhow::Result<()> {
    let parent = bundle
        .parent()
        .context("app bundle has no parent directory")?;
    let name = bundle
        .file_name()
        .context("app bundle has no name")?
        .to_string_lossy();
    let pid = std::process::id();
    let fresh = parent.join(format!(".{name}.new-{pid}"));
    let old = parent.join(format!(".{name}.old-{pid}"));
    let _ = std::fs::remove_dir_all(&fresh);
    run(
        "ditto",
        &[&staged.to_string_lossy(), &fresh.to_string_lossy()],
    )?;
    std::fs::rename(bundle, &old).context("moving the current app aside")?;
    if let Err(err) = std::fs::rename(&fresh, bundle) {
        let _ = std::fs::rename(&old, bundle);
        let _ = std::fs::remove_dir_all(&fresh);
        return Err(err).context("installing the new app bundle");
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// Detached relauncher: waits for THIS process to exit, then `open`s the bundle.
/// (Opening before exit would race the single-instance engine lock and the IPC
/// port.) The caller quits the app after this returns.
pub fn relaunch_app_after_exit(bundle: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let pid = std::process::id();
        let script = format!(
            "while /bin/kill -0 {pid} 2>/dev/null; do sleep 0.2; done; /usr/bin/open \"{}\"",
            bundle.display()
        );
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        if let Err(err) = command.spawn() {
            tracing::error!(error = %err, "failed to spawn the relauncher");
        }
    }
    #[cfg(not(unix))]
    let _ = bundle;
}

// ---------------------------------------------------------------------------
// Engine-side checker
// ---------------------------------------------------------------------------

/// What the engine reports over the `UpdateStatus` stream. Version facts only —
/// download/apply progress is owned by whoever drives the update (UI or CLI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub current_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default)]
    pub update_available: bool,
    /// Epoch ms of the last successful check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UpdateStatus {
    fn initial() -> Self {
        Self {
            current_version: current_version().to_string(),
            latest_version: None,
            update_available: false,
            checked_at: None,
            error: None,
        }
    }
}

/// `COMET_AUTO_UPDATE=1|true|yes` — headless daemons apply updates themselves.
fn auto_update_enabled() -> bool {
    std::env::var("COMET_AUTO_UPDATE")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// "Nothing would be interrupted by a restart right now" — wired by the engine
/// to its live-run and open-terminal registries. `None` = no gate.
pub type QuiescentCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// Background release checker: polls `{releases_url}/releases` on a 6h cadence and
/// publishes [`UpdateStatus`] over a watch channel (the `UpdateStatus` RPC
/// stream). Managed installs with `COMET_AUTO_UPDATE` set stage + apply + service
/// restart on their own — but only in a quiet window: while `quiescent` reports
/// activity, the apply defers and re-probes every [`IDLE_RECHECK`].
#[derive(Clone)]
pub struct Updater {
    releases_url: String,
    status_tx: Arc<watch::Sender<UpdateStatus>>,
    quiescent: Option<QuiescentCheck>,
}

impl Updater {
    /// Spawn the check loop (must run on a tokio runtime).
    pub fn spawn(releases_url: String, quiescent: Option<QuiescentCheck>) -> Self {
        let (status_tx, _) = watch::channel(UpdateStatus::initial());
        let updater = Self {
            releases_url,
            status_tx: Arc::new(status_tx),
            quiescent,
        };
        let for_loop = updater.clone();
        tokio::spawn(async move { for_loop.check_loop().await });
        updater
    }

    pub fn watch(&self) -> watch::Receiver<UpdateStatus> {
        self.status_tx.subscribe()
    }

    fn quiescent_now(&self) -> bool {
        self.quiescent.as_ref().is_none_or(|check| check())
    }

    async fn check_loop(&self) {
        tokio::time::sleep(CHECK_INITIAL_DELAY).await;
        loop {
            let ok = self.check_once().await;
            if ok
                && self.status_tx.borrow().update_available
                && auto_update_enabled()
                && let InstallKind::Managed { .. } = detect_install()
            {
                self.auto_apply_when_idle().await;
            }
            tokio::time::sleep(if ok { CHECK_INTERVAL } else { CHECK_RETRY }).await;
        }
    }

    /// Sessions must never die to an update: pre-stage the download now
    /// (harmless while busy), wait for a quiet window (no live runs, no open
    /// terminals), then apply — which re-fetches the manifest (so a long defer
    /// lands on whatever is newest) and reuses the staged dir, keeping the
    /// idle→restart gap to well under a second.
    async fn auto_apply_when_idle(&self) {
        if let InstallKind::Managed { app_root } = detect_install() {
            match fetch_latest(&self.releases_url).await {
                Ok(manifest) if version_newer(&manifest.version, current_version()) => {
                    if let Err(err) = stage_headless(&self.releases_url, &manifest, &app_root).await
                    {
                        tracing::warn!(error = %err, "auto-update staging failed");
                        return;
                    }
                }
                Ok(_) => return,
                Err(err) => {
                    tracing::warn!(error = %err, "auto-update staging fetch failed");
                    return;
                }
            }
        }
        let mut deferred = false;
        while !self.quiescent_now() {
            if !deferred {
                deferred = true;
                tracing::info!("auto-update deferred: sessions or terminals active");
            }
            tokio::time::sleep(IDLE_RECHECK).await;
        }
        match self.apply().await {
            Ok(version) => {
                tracing::info!(%version, "auto-update applied; service restarting")
            }
            Err(err) => tracing::warn!(error = %err, "auto-update failed"),
        }
    }

    /// One check; returns false on fetch failure (retry sooner).
    async fn check_once(&self) -> bool {
        match fetch_latest(&self.releases_url).await {
            Ok(manifest) => {
                let status = UpdateStatus {
                    current_version: current_version().to_string(),
                    update_available: version_newer(&manifest.version, current_version()),
                    latest_version: Some(manifest.version),
                    checked_at: Some(now_ms()),
                    error: None,
                };
                if status.update_available {
                    tracing::info!(
                        latest = status.latest_version.as_deref().unwrap_or(""),
                        current = %status.current_version,
                        "update available"
                    );
                }
                self.status_tx.send_replace(status);
                true
            }
            Err(err) => {
                tracing::debug!(error = %err, "update check failed");
                self.status_tx
                    .send_modify(|s| s.error = Some(format!("{err:#}")));
                false
            }
        }
    }

    /// Stage + apply the newest release on THIS device (managed installs only),
    /// then restart the service after a short delay so the caller's RPC reply
    /// flushes before systemd/launchd kills this process.
    pub async fn apply(&self) -> anyhow::Result<String> {
        let InstallKind::Managed { app_root } = detect_install() else {
            bail!(
                "this install is not update-managed — the desktop app updates from its UI; \
                 source builds update via git"
            );
        };
        let manifest = fetch_latest(&self.releases_url).await?;
        if !version_newer(&manifest.version, current_version()) {
            bail!("already up to date ({})", current_version());
        }
        stage_headless(&self.releases_url, &manifest, &app_root).await?;
        apply_headless(&app_root, &manifest.version)?;
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            if let Err(err) = restart_service() {
                tracing::warn!(error = %err, "service restart failed — restart the engine to finish the update");
            }
        });
        Ok(manifest.version)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releases_url_is_independent_of_removed_edge_variables() {
        let lookup = |key: &str| match key {
            "COMET_RELEASES_URL" => Some("https://downloads.example".to_string()),
            concat!("COMET_", "EDGE_URL") => Some("https://must-not-be-read.example".to_string()),
            _ => None,
        };

        assert_eq!(releases_url_from(lookup), "https://downloads.example");
    }

    #[test]
    fn releases_url_defaults_when_override_is_missing_or_blank() {
        assert_eq!(releases_url_from(|_| None), DEFAULT_RELEASES_URL);
        assert_eq!(
            releases_url_from(|key| (key == "COMET_RELEASES_URL").then(|| "  ".to_string())),
            DEFAULT_RELEASES_URL
        );
    }

    #[tokio::test]
    async fn unreachable_distribution_only_changes_update_status() {
        let updater = Updater::spawn("http://127.0.0.1:1".to_string(), None);
        let ok = updater.check_once().await;
        let status = updater.watch().borrow().clone();

        assert!(!ok);
        assert_eq!(status.current_version, current_version());
        assert!(!status.update_available);
        assert!(status.latest_version.is_none());
        assert!(status.error.is_some());
    }

    #[test]
    fn version_compare() {
        assert!(version_newer("0.1.1", "0.1.0"));
        assert!(version_newer("0.2.0", "0.1.9"));
        assert!(version_newer("0.1.10", "0.1.9"));
        assert!(version_newer("v0.1.1", "0.1.0"));
        assert!(version_newer("0.1.0.1", "0.1.0"));
        assert!(!version_newer("0.1.0", "0.1.0"));
        assert!(!version_newer("0.1.0", "0.1.1"));
        // Garbage never counts as newer.
        assert!(!version_newer("", "0.1.0"));
        assert!(!version_newer("nightly", "0.1.0"));
    }

    #[test]
    fn install_kind_detection() {
        assert_eq!(
            detect_install_from(
                Path::new("/home/u/.comet-native/app/0.1.1/comet"),
                Some(Path::new("/home/u")),
            ),
            InstallKind::Managed {
                app_root: PathBuf::from("/home/u/.comet-native/app")
            }
        );
        assert_eq!(
            detect_install_from(
                Path::new("/Applications/Comet.app/Contents/MacOS/comet"),
                Some(Path::new("/Users/u")),
            ),
            InstallKind::MacApp {
                bundle: PathBuf::from("/Applications/Comet.app")
            }
        );
        // A path merely containing `.app` without the bundle layout is not a bundle.
        assert_eq!(
            detect_install_from(Path::new("/tmp/foo.app/comet"), None),
            InstallKind::Unmanaged
        );
        assert_eq!(
            detect_install_from(
                Path::new("/src/target/release/comet"),
                Some(Path::new("/home/u"))
            ),
            InstallKind::Unmanaged
        );
    }

    #[test]
    fn artifact_names_match_packaging() {
        let (os, arch) = platform_key();
        assert!(headless_artifact("0.2.0").starts_with("comet-0.2.0-"));
        assert_eq!(
            headless_artifact("0.2.0"),
            format!("comet-0.2.0-{os}-{arch}.tar.gz")
        );
        assert!(mac_app_artifact("0.2.0").ends_with("-app.tar.gz"));
    }

    #[test]
    fn manifest_parses_with_and_without_files() {
        let full = parse_manifest(
            br#"{"repository":"matty/comet","version":"0.1.1","files":{"comet-0.1.1-linux-x86_64.tar.gz":{"sha256":"abc"}}}"#,
        )
        .unwrap();
        assert_eq!(full.version, "0.1.1");
        assert_eq!(
            full.files["comet-0.1.1-linux-x86_64.tar.gz"]
                .sha256
                .as_deref(),
            Some("abc")
        );
        let bare = parse_manifest(br#"{"repository":"matty/comet","version":"0.1.1"}"#).unwrap();
        assert!(bare.files.is_empty());
    }

    #[test]
    fn manifest_rejects_missing_or_wrong_repository() {
        let missing = parse_manifest(br#"{"version":"0.1.1"}"#).unwrap_err();
        assert!(missing.to_string().contains("release repository"));

        let wrong = parse_manifest(br#"{"repository":"someone/other-comet","version":"0.1.1"}"#)
            .unwrap_err();
        assert!(wrong.to_string().contains("someone/other-comet"));
        assert!(wrong.to_string().contains("matty/comet"));
    }

    #[test]
    fn manifest_rejects_non_numeric_or_path_versions() {
        for version in [
            "../../evil",
            "1.2 3",
            "nightly",
            "1..2",
            "18446744073709551616.1",
        ] {
            let json =
                format!(r#"{{"repository":"matty/comet","version":"{version}","files":{{}}}}"#);
            assert!(
                parse_manifest(json.as_bytes()).is_err(),
                "accepted {version}"
            );
        }
    }

    #[test]
    fn selected_artifact_requires_valid_sha256() {
        let mut manifest =
            parse_manifest(br#"{"repository":"matty/comet","version":"1.2.3","files":{}}"#)
                .unwrap();
        let file = "comet-1.2.3-linux-x86_64.tar.gz";

        assert!(expected_release_sha256(&manifest, file).is_err());
        manifest
            .files
            .insert(file.to_string(), FileMeta { sha256: None });
        assert!(expected_release_sha256(&manifest, file).is_err());
        manifest.files.get_mut(file).unwrap().sha256 = Some("not-a-checksum".into());
        assert!(expected_release_sha256(&manifest, file).is_err());
        manifest.files.get_mut(file).unwrap().sha256 = Some("a".repeat(64));
        assert_eq!(
            expected_release_sha256(&manifest, file).unwrap(),
            "a".repeat(64)
        );
    }

    #[tokio::test]
    async fn missing_checksum_creates_no_staged_artifact() {
        let manifest =
            parse_manifest(br#"{"repository":"matty/comet","version":"1.2.3","files":{}}"#)
                .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("release.tar.gz");

        let error = download_release_file(
            "http://127.0.0.1:1",
            &manifest,
            "comet-1.2.3-linux-x86_64.tar.gz",
            &destination,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("missing artifact metadata"));
        assert!(!destination.exists());
        assert!(!destination.with_extension("partial").exists());
    }

    #[tokio::test]
    async fn missing_checksum_does_not_create_install_staging() {
        let manifest =
            parse_manifest(br#"{"repository":"matty/comet","version":"1.2.3","files":{}}"#)
                .unwrap();
        let temp = tempfile::tempdir().unwrap();

        assert!(
            stage_headless("http://127.0.0.1:1", &manifest, temp.path())
                .await
                .is_err()
        );
        assert!(std::fs::read_dir(temp.path()).unwrap().next().is_none());

        assert!(
            stage_mac_app("http://127.0.0.1:1", &manifest, temp.path())
                .await
                .is_err()
        );
        assert!(!temp.path().join("updates/1.2.3").exists());
    }

    #[tokio::test]
    async fn existing_headless_stage_requires_metadata_and_matching_marker() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("1.2.3");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("comet"), "unverified").unwrap();

        let missing =
            parse_manifest(br#"{"repository":"matty/comet","version":"1.2.3","files":{}}"#)
                .unwrap();
        assert!(
            stage_headless("http://127.0.0.1:1", &missing, temp.path())
                .await
                .is_err()
        );

        let file = headless_artifact("1.2.3");
        let valid = parse_manifest(
            format!(
                r#"{{"repository":"matty/comet","version":"1.2.3","files":{{"{file}":{{"sha256":"{}"}}}}}}"#,
                "a".repeat(64)
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(
            stage_headless("http://127.0.0.1:1", &valid, temp.path())
                .await
                .is_err()
        );
        std::fs::write(destination.join(".comet-release"), "wrong marker").unwrap();
        assert!(
            stage_headless("http://127.0.0.1:1", &valid, temp.path())
                .await
                .is_err()
        );
        std::fs::write(
            destination.join(STAGE_MARKER_FILE),
            stage_marker(&valid, &file).unwrap(),
        )
        .unwrap();
        assert_eq!(
            stage_headless("http://127.0.0.1:1", &valid, temp.path())
                .await
                .unwrap(),
            destination
        );
    }

    #[tokio::test]
    async fn existing_mac_stage_requires_metadata_and_matching_marker() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("updates/1.2.3/Comet.app/Contents/MacOS");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("comet"), "unverified").unwrap();

        let missing =
            parse_manifest(br#"{"repository":"matty/comet","version":"1.2.3","files":{}}"#)
                .unwrap();
        assert!(
            stage_mac_app("http://127.0.0.1:1", &missing, temp.path())
                .await
                .is_err()
        );

        let file = mac_app_artifact("1.2.3");
        let valid = parse_manifest(
            format!(
                r#"{{"repository":"matty/comet","version":"1.2.3","files":{{"{file}":{{"sha256":"{}"}}}}}}"#,
                "a".repeat(64)
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(
            stage_mac_app("http://127.0.0.1:1", &valid, temp.path())
                .await
                .is_err()
        );
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("comet"), "unverified").unwrap();
        std::fs::write(
            temp.path().join("updates/1.2.3/.comet-release"),
            "wrong marker",
        )
        .unwrap();
        assert!(
            stage_mac_app("http://127.0.0.1:1", &valid, temp.path())
                .await
                .is_err()
        );
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("comet"), "verified").unwrap();
        std::fs::write(
            temp.path().join("updates/1.2.3").join(STAGE_MARKER_FILE),
            stage_marker(&valid, &file).unwrap(),
        )
        .unwrap();
        assert_eq!(
            stage_mac_app("http://127.0.0.1:1", &valid, temp.path())
                .await
                .unwrap(),
            temp.path().join("updates/1.2.3/Comet.app")
        );
    }

    #[tokio::test]
    async fn provenance_failure_does_not_fallback_to_latest_txt() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = requests.clone();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let Ok(Ok((mut socket, _))) =
                    tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
                        .await
                else {
                    break;
                };
                server_requests.fetch_add(1, Ordering::SeqCst);
                let mut request = [0_u8; 2048];
                let read = socket.read(&mut request).await.unwrap();
                let path = String::from_utf8_lossy(&request[..read]);
                let body = if path.contains("/releases/manifest.json") {
                    r#"{"repository":"someone/other-comet","version":"9.9.9"}"#
                } else {
                    "9.9.9"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let error = fetch_latest(&format!("http://{address}"))
            .await
            .unwrap_err();
        server.await.unwrap();

        assert!(error.to_string().contains("release repository mismatch"));
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[cfg(unix)]
    #[test]
    fn headless_symlink_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path().join("app");
        for ver in ["0.1.0", "0.1.1"] {
            std::fs::create_dir_all(app_root.join(ver)).unwrap();
            std::fs::write(app_root.join(ver).join("comet"), ver).unwrap();
        }
        apply_headless(&app_root, "0.1.0").unwrap();
        assert_eq!(
            std::fs::read_link(app_root.join("current")).unwrap(),
            app_root.join("0.1.0")
        );
        // Swap over an existing symlink.
        apply_headless(&app_root, "0.1.1").unwrap();
        assert_eq!(
            std::fs::read_link(app_root.join("current")).unwrap(),
            app_root.join("0.1.1")
        );
        // Unstaged version refuses.
        assert!(apply_headless(&app_root, "0.2.0").is_err());
    }
}
