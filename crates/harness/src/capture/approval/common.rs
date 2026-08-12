use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::ffi::OsString;

use anyhow::{anyhow, bail};

use crate::capture::sanitize::has_windows_reparse_point;
use crate::capture::{CaptureConfig, CaptureOperation, CodexCaptureOperation, CodexRunScript};

pub(in crate::capture) const CLAUDE_APPROVAL_COMMAND: &str = "printf capture";
pub(in crate::capture) const CODEX_APPROVAL_COMMAND: &str = "echo capture";
pub(in crate::capture) const APPROVAL_MARKER_NAME: &str = "capture-marker.txt";
pub(in crate::capture) const APPROVAL_MARKER_CONTENT: &str = "capture\n";
pub(in crate::capture) const APPROVAL_MARKER_ADD_DIFF: &str = APPROVAL_MARKER_CONTENT;

pub fn claude_approval_prompt(cwd: &Path) -> String {
    let marker = cwd.join(APPROVAL_MARKER_NAME);
    format!(
        "Use Bash exactly once with input {{\"command\":{}}}. Wait for it to finish successfully. Then use Write exactly once with input {{\"file_path\":{},\"content\":{}}}.",
        serde_json::to_string(CLAUDE_APPROVAL_COMMAND).expect("static command serializes"),
        serde_json::to_string(&marker.display().to_string()).expect("path serializes"),
        serde_json::to_string(APPROVAL_MARKER_CONTENT).expect("static content serializes"),
    )
}

pub fn codex_approval_prompt(cwd: &Path) -> String {
    let marker = cwd.join(APPROVAL_MARKER_NAME);
    format!(
        "Run the exact command {} three separate times, then add exactly one file at {} containing exactly capture followed by one newline.",
        serde_json::to_string(CODEX_APPROVAL_COMMAND).expect("static command serializes"),
        serde_json::to_string(&marker.display().to_string()).expect("path serializes"),
    )
}

pub fn approval_marker_command(target: &Path) -> String {
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

pub fn approval_on_request_prompt(target: &Path) -> String {
    format!(
        "Run this exact command once and report success: {}",
        approval_marker_command(target)
    )
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
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, OPEN_EXISTING,
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
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            bail!("Codex on-request approval target identity could not be opened.");
        }
        // SAFETY: ownership of the newly opened handle transfers to `File`.
        let file = unsafe { std::fs::File::from_raw_handle(handle) };
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        // SAFETY: the live file handle and writable output pointer are valid for this call.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            bail!("Codex on-request approval target identity could not be read.");
        }
        // SAFETY: the successful call initialized the whole structure.
        let info = unsafe { info.assume_init() };
        Ok(DirectoryIdentity {
            canonical,
            volume_serial_number: info.dwVolumeSerialNumber,
            file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
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

pub(in crate::capture) fn validate_on_request_preflight(
    config: &CaptureConfig,
) -> anyhow::Result<Option<DirectoryIdentity>> {
    let CaptureOperation::Codex(CodexCaptureOperation::Run { request, script }) =
        &config.scenario.operation
    else {
        return Ok(None);
    };
    if !matches!(script, CodexRunScript::ApprovalOnRequest) {
        return Ok(None);
    }
    if request.runtime_mode != comet_proto::RuntimeMode::AutoAcceptEdits
        || request.sandbox != comet_proto::SandboxLevel::WorkspaceWrite
    {
        bail!("Codex on-request capture requires workspace-write/on-request runtime settings.");
    }
    let cwd = Path::new(&request.cwd);
    let cwd = std::fs::canonicalize(cwd).map_err(|_| {
        anyhow!("Codex on-request capture requires an accessible non-repository cwd.")
    })?;
    if repository_root(&cwd).is_some() {
        bail!("Codex on-request capture requires a non-repository, non-worktree cwd.");
    }
    let target = config
        .approval_target
        .as_deref()
        .ok_or_else(|| anyhow!("Codex on-request capture requires a validated approval target."))?;
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

pub(in crate::capture) fn validate_ordinary_approval_marker(cwd: &Path) -> anyhow::Result<()> {
    let marker = cwd.join(APPROVAL_MARKER_NAME);
    let metadata = std::fs::symlink_metadata(&marker)
        .map_err(|_| anyhow!("Codex approval marker was not created."))?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || has_windows_reparse_point(&metadata)
    {
        bail!("Codex approval marker was not a regular non-reparse file.");
    }
    let content = std::fs::read_to_string(marker)
        .map_err(|_| anyhow!("Codex approval marker could not be read."))?;
    if content != APPROVAL_MARKER_CONTENT {
        bail!("Codex approval marker did not contain the exact bounded content.");
    }
    Ok(())
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
pub(in crate::capture) fn file_identity(path: &Path) -> anyhow::Result<FileIdentity> {
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
    #[cfg(windows)]
    {
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
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            bail!("The trusted PowerShell executable identity could not be opened.");
        }
        // SAFETY: ownership of the newly opened handle transfers to `File`.
        let file = unsafe { std::fs::File::from_raw_handle(handle) };
        let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        // SAFETY: the live file handle and writable output pointer are valid for this call.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            bail!("The trusted PowerShell executable identity could not be read.");
        }
        // SAFETY: the successful call initialized the whole structure.
        let info = unsafe { info.assume_init() };
        Ok(FileIdentity {
            canonical,
            volume_serial_number: info.dwVolumeSerialNumber,
            file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        })
    }
    #[cfg(not(windows))]
    {
        Ok(FileIdentity { canonical })
    }
}

#[cfg(windows)]
pub(in crate::capture) fn canonical_protected_roots<'a>(
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
pub(in crate::capture) fn windows_protected_roots() -> anyhow::Result<Vec<PathBuf>> {
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
pub(in crate::capture) fn select_trusted_powershell(
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
