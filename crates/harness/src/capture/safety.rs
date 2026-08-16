//! The pre-spawn fence: design §3.2's other half.
//!
//! Fence the environment before spawn; then whatever happens inside the fence is safe to record.
//! Every check in this file runs before a provider process exists, and nothing in it may read a
//! frame — a check that inspects a frame and aborts destroys evidence already paid for (that
//! class is deleted, not moved); a check here instead refuses to spawn at all when the
//! environment it is about to hand a provider is not the one it was told to expect.

use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::ffi::OsString;

use anyhow::{anyhow, bail};

use crate::capture::filesystem::has_windows_reparse_point;

// `pub(in crate::capture)`, not `pub(super)`: both provider prompt builders that read these
// constants — Claude's `claude_approval_prompt` and Codex's `codex_approval_prompt` — live in
// `capture::record::scenarios::{claude, codex}`, which need to read them from there.
// `validate_ordinary_approval_cwd` below also reads `APPROVAL_MARKER_NAME` for its own
// marker-absence check, so moving either constant into one provider's scenario module would
// make the fence and the other provider's prompt reach across a scenario boundary instead.
pub(in crate::capture) const APPROVAL_MARKER_NAME: &str = "capture-marker.txt";
pub(in crate::capture) const APPROVAL_MARKER_CONTENT: &str = "capture\n";

/// The literal command a marker-write approval asks the model to run, platform-specific. Stays
/// here rather than moving into either provider's scenario module: `approval_on_request_prompt`
/// (`record/scenarios/codex.rs`) embeds this command's own output verbatim into its prompt text,
/// so the two are more one shared primitive than one owning the other.
pub(in crate::capture) fn approval_marker_command(target: &Path) -> String {
    #[cfg(windows)]
    {
        let path = target
            .join("approval-marker.txt")
            .display()
            .to_string()
            .replace('\'', "''");
        format!(
            "powershell.exe -NoProfile -Command \"Set-Content -LiteralPath '{path}' -Value 'capture' -NoNewline\""
        )
    }
    #[cfg(not(windows))]
    {
        let path = target
            .join("approval-marker.txt")
            .display()
            .to_string()
            .replace('\'', "'\\''");
        format!("printf %s capture > '{path}'")
    }
}

pub(in crate::capture) fn repository_root(start: &Path) -> Option<&Path> {
    start
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::capture) struct DirectoryIdentity {
    canonical: PathBuf,
    #[cfg(windows)]
    volume_serial_number: u32,
    #[cfg(windows)]
    file_index: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

/// The Windows file-identity primitive both [`directory_identity`] and [`file_identity`] need:
/// open `canonical` without acquiring share locks, read back its volume serial number and file
/// index, error using `what` to name what failed. `flags` is the one legitimate difference
/// between the two callers (`FILE_FLAG_BACKUP_SEMANTICS` for a directory handle, `0` for a
/// regular file) — everything else was duplicated verbatim between them before this, the worst
/// shape for an `unsafe` block: a fix to one copy is invisible in the other.
#[cfg(windows)]
fn windows_handle_identity(canonical: &Path, flags: u32, what: &str) -> anyhow::Result<(u32, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING,
    };
    let wide: Vec<u16> = canonical.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a live, NUL-terminated UTF-16 path for this call.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        bail!("{what} identity could not be opened.");
    }
    // SAFETY: ownership of the newly opened handle transfers to `File`.
    let file = unsafe { std::fs::File::from_raw_handle(handle) };
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: the live file handle and writable output pointer are valid for this call.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        bail!("{what} identity could not be read.");
    }
    // SAFETY: the successful call initialized the whole structure.
    let info = unsafe { info.assume_init() };
    Ok((
        info.dwVolumeSerialNumber,
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

fn directory_identity(path: &Path) -> anyhow::Result<DirectoryIdentity> {
    let link_metadata = std::fs::symlink_metadata(path).map_err(|_| {
        anyhow!("Codex on-request approval target must remain an accessible empty directory.")
    })?;
    if link_metadata.file_type().is_symlink() || has_windows_reparse_point(&link_metadata) {
        bail!("Codex on-request approval target must not be a symbolic link.");
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| {
        anyhow!("Codex on-request approval target must remain an accessible empty directory.")
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|_| {
        anyhow!("Codex on-request approval target must remain an accessible empty directory.")
    })?;
    if !metadata.is_dir() {
        bail!("Codex on-request approval target must remain an empty directory.");
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
        let (volume_serial_number, file_index) = windows_handle_identity(
            &canonical,
            FILE_FLAG_BACKUP_SEMANTICS,
            "Codex on-request approval target",
        )?;
        Ok(DirectoryIdentity {
            canonical,
            volume_serial_number,
            file_index,
        })
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(DirectoryIdentity {
            canonical,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

pub(in crate::capture) fn require_empty_approval_target(
    target: &Path,
    expected_identity: Option<&DirectoryIdentity>,
) -> anyhow::Result<DirectoryIdentity> {
    let identity = directory_identity(target)?;
    if expected_identity.is_some_and(|expected| expected != &identity) {
        bail!("Codex on-request approval target changed identity before approval.");
    }
    let mut entries = std::fs::read_dir(target).map_err(|_| {
        anyhow!("Codex on-request approval target must remain an accessible empty directory.")
    })?;
    if entries.next().is_some() {
        bail!("Codex on-request approval target must remain empty before approval.");
    }
    Ok(identity)
}

/// The `approval-on-request` fence: a non-repository, non-worktree `cwd`, and an
/// `approval_target` that stays empty, identity-stable, and isolated from both `cwd` and the
/// system temp tree. Returns `Ok(None)` when `approval_target` is absent.
///
/// An older assertion here checked `request.runtime_mode`/`request.sandbox` stayed
/// `AutoAcceptEdits`/`WorkspaceWrite` after `normalize_run_request`, guarding against a
/// caller-built `RunRequest` disagreeing with the CLI arguments. It is dropped, not ported:
/// `record::scenarios::codex::approval_on_request_request` is the only builder of that request
/// now and hardcodes both fields, so `normalize_run_request` can only ever escalate `sandbox` for
/// a linked worktree on a slash-named branch — a shape this function already rejects via
/// `repository_root`. The condition is structurally unreachable now, not merely re-checked. The
/// other half — the `RuntimeMode -> SandboxLevel` mapping itself regressing, independent of any
/// cwd — is restored as a narrower debug assertion in `approval_on_request_request`.
pub(in crate::capture) fn validate_on_request_preflight(
    cwd: &Path,
    approval_target: Option<&Path>,
) -> anyhow::Result<Option<DirectoryIdentity>> {
    let Some(target) = approval_target else {
        return Ok(None);
    };
    let cwd = std::fs::canonicalize(cwd).map_err(|_| {
        anyhow!("Codex on-request capture requires an accessible non-repository cwd.")
    })?;
    if repository_root(&cwd).is_some() {
        bail!("Codex on-request capture requires a non-repository, non-worktree cwd.");
    }
    let identity = directory_identity(target)?;
    if identity.canonical.starts_with(&cwd) || cwd.starts_with(&identity.canonical) {
        bail!("Codex on-request approval target must remain isolated from the cwd.");
    }
    let temp = std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
    if identity.canonical.starts_with(temp) {
        bail!("Codex on-request approval target must remain outside the system temporary tree.");
    }
    if target.join(".git").is_file() {
        bail!("Codex on-request approval target must not be a linked worktree.");
    }
    require_empty_approval_target(target, Some(&identity))?;
    Ok(Some(identity))
}

pub(in crate::capture) fn validate_ordinary_approval_cwd(
    cwd: &Path,
    expected: Option<&DirectoryIdentity>,
    require_marker_absent: bool,
) -> anyhow::Result<DirectoryIdentity> {
    let identity = directory_identity(cwd)
        .map_err(|_| anyhow!("Codex approval capture cwd identity could not be validated."))?;
    if expected.is_some_and(|expected| expected != &identity) {
        bail!("Codex approval capture cwd changed identity during the scenario.");
    }
    let marker = cwd.join(APPROVAL_MARKER_NAME);
    if require_marker_absent && std::fs::symlink_metadata(&marker).is_ok() {
        bail!("Codex approval marker must be absent before file approval.");
    }
    Ok(identity)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::capture) struct FileIdentity {
    pub(in crate::capture) canonical: PathBuf,
    #[cfg(windows)]
    volume_serial_number: u32,
    #[cfg(windows)]
    file_index: u64,
}

#[cfg(windows)]
pub(super) fn file_identity(path: &Path) -> anyhow::Result<FileIdentity> {
    let link_metadata = std::fs::symlink_metadata(path)
        .map_err(|_| anyhow!("The trusted PowerShell executable could not be inspected."))?;
    if !link_metadata.file_type().is_file()
        || link_metadata.file_type().is_symlink()
        || has_windows_reparse_point(&link_metadata)
    {
        bail!("The trusted PowerShell executable must be a regular file.");
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| anyhow!("The trusted PowerShell executable could not be resolved."))?;
    let (volume_serial_number, file_index) =
        windows_handle_identity(&canonical, 0, "The trusted PowerShell executable")?;
    Ok(FileIdentity {
        canonical,
        volume_serial_number,
        file_index,
    })
}

#[cfg(windows)]
pub(super) fn canonical_protected_roots<'a>(
    roots: impl IntoIterator<Item = Option<&'a Path>>,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut canonical = Vec::new();
    for root in roots.into_iter().flatten() {
        let root = std::fs::canonicalize(root).map_err(|_| {
            anyhow!("A configured Windows system root could not be validated for capture.")
        })?;
        if !canonical.contains(&root) {
            canonical.push(root);
        }
    }
    if canonical.is_empty() {
        bail!("Windows system installation roots could not be found for approval capture.");
    }
    Ok(canonical)
}

#[cfg(windows)]
pub(super) fn windows_protected_roots() -> anyhow::Result<Vec<PathBuf>> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, FOLDERID_Windows, SHGetKnownFolderPath,
    };
    if usize::BITS != 64 {
        bail!("Windows approval capture is only reviewed for 64-bit hosts.");
    }
    let mut known = Vec::new();
    for folder in [
        &FOLDERID_Windows,
        &FOLDERID_ProgramFiles,
        &FOLDERID_ProgramFilesX86,
    ] {
        let mut raw = std::ptr::null_mut();
        // SAFETY: the known-folder GUID is static and `raw` is a writable out pointer.
        let status = unsafe { SHGetKnownFolderPath(folder, 0, std::ptr::null_mut(), &mut raw) };
        if status >= 0 && !raw.is_null() {
            let mut len = 0;
            // SAFETY: successful API output is a NUL-terminated UTF-16 allocation.
            while unsafe { *raw.add(len) } != 0 {
                len += 1;
            }
            // SAFETY: the allocation contains `len` initialized code units.
            known.push(PathBuf::from(OsString::from_wide(unsafe {
                std::slice::from_raw_parts(raw, len)
            })));
        }
        // SAFETY: SHGetKnownFolderPath documents CoTaskMemFree ownership for every nonnull
        // output, including a buffer returned alongside failure.
        if !raw.is_null() {
            unsafe { CoTaskMemFree(raw.cast()) };
        }
    }
    if known.len() != 3 {
        bail!("Windows protected installation roots could not be resolved for approval capture.");
    }
    let roots = canonical_protected_roots([
        Some(known[0].as_path()),
        Some(known[1].as_path()),
        Some(known[2].as_path()),
    ])?;
    if roots.len() != 3 {
        bail!("Windows protected installation roots resolved inconsistently.");
    }
    Ok(roots)
}

#[cfg(windows)]
pub(super) fn select_trusted_powershell(
    candidates: &[PathBuf],
    protected_roots: &[PathBuf],
    forbidden_roots: &[PathBuf],
) -> anyhow::Result<FileIdentity> {
    candidates
        .iter()
        .filter_map(|candidate| file_identity(candidate).ok())
        .find(|identity| {
            protected_roots
                .iter()
                .any(|root| identity.canonical.starts_with(root))
                && !forbidden_roots
                    .iter()
                    .any(|root| identity.canonical.starts_with(root))
        })
        .ok_or_else(|| {
            anyhow!(
                "Codex approval capture requires PowerShell from a protected Windows system root."
            )
        })
}

#[cfg(windows)]
pub(in crate::capture) fn resolve_trusted_powershell(
    cwd: &Path,
    raw_root: &Path,
) -> anyhow::Result<FileIdentity> {
    let mut paths = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Some(path) = crate::shell_env::system_path() {
        paths.extend(std::env::split_paths(path));
    }
    let candidates: Vec<_> = paths
        .into_iter()
        .map(|dir| dir.join("pwsh.exe"))
        .filter(|path| path.is_file())
        .collect();
    let protected_roots = windows_protected_roots()?;
    let mut forbidden_roots = vec![
        std::fs::canonicalize(cwd)
            .map_err(|_| anyhow!("Codex approval capture cwd could not be validated."))?,
    ];
    forbidden_roots
        .push(std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir()));
    if let Some(home) = crate::home_dir().and_then(|path| std::fs::canonicalize(path).ok()) {
        forbidden_roots.push(home);
    }
    if let Ok(path) = std::path::absolute(raw_root) {
        forbidden_roots.push(std::fs::canonicalize(&path).unwrap_or(path));
    }
    // These roots establish the capture trust boundary. ACL ownership inference is deliberately
    // avoided; every observed launcher is still reopened and compared by file identity.
    select_trusted_powershell(&candidates, &protected_roots, &forbidden_roots)
}

#[cfg(not(windows))]
pub(in crate::capture) fn resolve_trusted_powershell(
    _cwd: &Path,
    _raw_root: &Path,
) -> anyhow::Result<FileIdentity> {
    bail!(
        "Codex approval capture has no observed safe Unix launcher contract. Review real evidence and update the design before retrying."
    )
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use std::collections::BTreeSet;
    #[cfg(not(windows))]
    use std::path::Path;

    #[cfg(windows)]
    use super::APPROVAL_MARKER_NAME;
    use crate::capture::record;
    use crate::capture::test_support::{
        config, fixture_path, isolated_approval_target, isolated_tempdir,
    };

    #[cfg(windows)]
    #[test]
    fn codex_approval_trusted_roots_must_exist_and_canonicalize() {
        let root = tempfile::tempdir().unwrap();
        assert!(super::canonical_protected_roots([None, None, None]).is_err());
        assert!(
            super::canonical_protected_roots([
                Some(root.path().join("missing").as_path()),
                None,
                None,
            ])
            .is_err()
        );
        let roots =
            super::canonical_protected_roots([Some(root.path()), Some(root.path()), None]).unwrap();
        assert_eq!(
            roots.len(),
            1,
            "canonical root aliases must be deduplicated"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn codex_approval_launcher_remains_fail_closed_without_unix_evidence() {
        let error =
            super::resolve_trusted_powershell(Path::new("/"), Path::new("/tmp/raw")).unwrap_err();
        assert!(error.to_string().contains("safe Unix launcher"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn codex_approval_cwd_identity_and_marker_are_rechecked() {
        let parent = tempfile::tempdir().unwrap();
        let cwd = parent.path().join("cwd");
        std::fs::create_dir(&cwd).unwrap();
        let identity = super::validate_ordinary_approval_cwd(&cwd, None, true).unwrap();
        std::fs::write(cwd.join(APPROVAL_MARKER_NAME), "capture\n").unwrap();
        assert!(super::validate_ordinary_approval_cwd(&cwd, Some(&identity), true).is_err());
        std::fs::remove_file(cwd.join(APPROVAL_MARKER_NAME)).unwrap();
        std::fs::remove_dir(&cwd).unwrap();
        std::fs::create_dir(&cwd).unwrap();
        assert!(super::validate_ordinary_approval_cwd(&cwd, Some(&identity), true).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn codex_approval_cwd_rejects_directory_reparse_points() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let link = root.path().join("cwd-link");
        std::fs::create_dir(&target).unwrap();
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to construct a test-owned directory junction"
        );
        assert!(super::validate_ordinary_approval_cwd(&link, None, true).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn codex_approval_windows_api_roots_are_available_and_canonical() {
        assert_eq!(usize::BITS, 64, "32-bit Windows must fail closed");
        let roots = super::windows_protected_roots().unwrap();
        assert!(!roots.is_empty());
        assert_eq!(roots.iter().collect::<BTreeSet<_>>().len(), roots.len());
        assert!(roots.iter().all(|root| root.is_absolute() && root.is_dir()));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn codex_approval_precreated_marker_fails_before_spawn_or_reply() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join(APPROVAL_MARKER_NAME), "capture\n").unwrap();
        let mut cfg = config("approval", fixture_path("fake-codex"), "codex", raw.path());
        cfg.cwd = Some(cwd.path().into());
        let error = record(cfg).await.unwrap_err();
        assert!(
            error.to_string().contains("marker must be absent"),
            "{error}"
        );
        assert!(raw.path().read_dir().unwrap().next().is_none());
    }

    #[tokio::test]
    async fn codex_on_request_preflight_rejects_repository_and_linked_worktree_cwds() {
        for linked in [false, true] {
            let raw = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            if linked {
                std::fs::write(cwd.path().join(".git"), "gitdir: unused").unwrap();
            } else {
                std::fs::create_dir(cwd.path().join(".git")).unwrap();
            }
            let Some(target) = isolated_approval_target("comet-onrequest-target-") else {
                return;
            };
            let mut cfg = config(
                "approval-on-request",
                fixture_path("fake-codex"),
                "codex",
                raw.path(),
            );
            cfg.cwd = Some(cwd.path().into());
            cfg.approval_target = Some(target.path().into());
            let error = match record(cfg).await {
                Ok(_) => panic!("repository cwd must fail before spawn"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("non-repository"), "{error}");
        }
    }

    #[tokio::test]
    async fn codex_on_request_preflight_rechecks_target_emptiness_before_spawn() {
        let raw = tempfile::tempdir().unwrap();
        let cwd = isolated_tempdir("comet-onrequest-cwd-");
        let Some(target) = isolated_approval_target("comet-onrequest-target-") else {
            return;
        };
        std::fs::write(target.path().join("appeared-after-config.txt"), "hostile").unwrap();
        let mut cfg = config(
            "approval-on-request",
            fixture_path("fake-codex"),
            "codex",
            raw.path(),
        );
        cfg.cwd = Some(cwd.path().into());
        cfg.approval_target = Some(target.path().into());
        let error = match record(cfg).await {
            Ok(_) => panic!("nonempty target must fail before spawn"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("empty"), "{error}");
    }
}
