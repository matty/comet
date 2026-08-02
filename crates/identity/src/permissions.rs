use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct AtomicWriteError {
    source: io::Error,
    committed: bool,
}

impl AtomicWriteError {
    fn before_commit(source: io::Error) -> Self {
        Self {
            source,
            committed: false,
        }
    }

    fn after_commit(source: io::Error) -> Self {
        Self {
            source,
            committed: true,
        }
    }

    pub fn committed(&self) -> bool {
        self.committed
    }
}

impl std::fmt::Display for AtomicWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for AtomicWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub(crate) fn write_private_atomic(path: &Path, contents: &[u8]) -> Result<(), AtomicWriteError> {
    let temporary = temporary_path(path);
    let result = (|| {
        let mut file = open_private(&temporary).map_err(AtomicWriteError::before_commit)?;
        file.write_all(contents)
            .map_err(AtomicWriteError::before_commit)?;
        file.sync_all().map_err(AtomicWriteError::before_commit)?;
        drop(file);
        replace_file(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn write_private_atomic_new(
    path: &Path,
    contents: &[u8],
) -> Result<bool, AtomicWriteError> {
    let temporary = temporary_path(path);
    let result = (|| {
        let mut file = open_private(&temporary).map_err(AtomicWriteError::before_commit)?;
        file.write_all(contents)
            .map_err(AtomicWriteError::before_commit)?;
        file.sync_all().map_err(AtomicWriteError::before_commit)?;
        drop(file);
        match std::fs::hard_link(&temporary, path) {
            Ok(()) => {
                sync_parent(path).map_err(AtomicWriteError::after_commit)?;
                Ok(true)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(AtomicWriteError::before_commit(error)),
        }
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

pub(crate) fn read_private(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = open_private_for_read(path)?;
    let metadata = file.metadata()?;
    if !is_regular_file(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private path is not a regular file",
        ));
    }
    verify_open_file_permissions(path, &file, &metadata)?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    Ok(contents)
}

#[cfg(windows)]
fn is_regular_file(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_type().is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

#[cfg(not(windows))]
fn is_regular_file(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_file()
}

#[cfg(unix)]
fn verify_open_file_permissions(
    path: &Path,
    _file: &std::fs::File,
    metadata: &std::fs::Metadata,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    if mode == 0o600 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} must have mode 0600, found {mode:04o}", path.display()),
        ))
    }
}

#[cfg(windows)]
fn verify_open_file_permissions(
    path: &Path,
    file: &std::fs::File,
    _metadata: &std::fs::Metadata,
) -> io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    let mut actual: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: the file handle is live and actual points to output storage.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut actual,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let expected = private_security_descriptor()?;
    let actual_sddl = dacl_sddl(actual);
    let expected_sddl = dacl_sddl(expected);
    // SAFETY: GetSecurityInfo allocates actual with LocalAlloc.
    unsafe { LocalFree(actual.cast::<c_void>()) };
    // SAFETY: expected was allocated by the SDDL conversion API.
    unsafe { LocalFree(expected.cast::<c_void>()) };
    let actual_sddl = actual_sddl?;
    let expected_sddl = expected_sddl?;
    if actual_sddl == expected_sddl {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} does not have the private Comet DACL", path.display()),
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn verify_open_file_permissions(
    _path: &Path,
    _file: &std::fs::File,
    _metadata: &std::fs::Metadata,
) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn open_private_for_read(path: &Path) -> io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(windows)]
fn open_private_for_read(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, OPEN_EXISTING,
    };

    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: path is a live NUL-terminated UTF-16 buffer for this call.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: ownership of the newly opened handle transfers to File.
        Ok(unsafe { std::fs::File::from_raw_handle(handle) })
    }
}

#[cfg(not(any(unix, windows)))]
fn open_private_for_read(path: &Path) -> io::Result<std::fs::File> {
    std::fs::File::open(path)
}

fn temporary_path(path: &Path) -> PathBuf {
    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("tmp-{}-{sequence}", std::process::id()))
}

#[cfg(unix)]
fn open_private(path: &Path) -> io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
fn open_private(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL};

    let descriptor = private_security_descriptor()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: path and security attributes remain valid for the duration of the call.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    // SAFETY: the descriptor was allocated by the SDDL conversion API.
    unsafe { LocalFree(descriptor.cast()) };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: ownership of the newly opened handle transfers to File.
        Ok(unsafe { std::fs::File::from_raw_handle(handle) })
    }
}

#[cfg(not(any(unix, windows)))]
fn open_private(path: &Path) -> io::Result<std::fs::File> {
    use std::fs::OpenOptions;

    OpenOptions::new().write(true).create_new(true).open(path)
}

#[cfg(unix)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), AtomicWriteError> {
    std::fs::rename(source, destination).map_err(AtomicWriteError::before_commit)?;
    sync_parent(destination).map_err(AtomicWriteError::after_commit)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<(), AtomicWriteError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: both pointers refer to NUL-terminated UTF-16 buffers for this call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(AtomicWriteError::before_commit(io::Error::last_os_error()))
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn replace_file(source: &Path, destination: &Path) -> Result<(), AtomicWriteError> {
    std::fs::rename(source, destination).map_err(AtomicWriteError::before_commit)
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn private_security_descriptor() -> io::Result<windows_sys::Win32::Security::PSECURITY_DESCRIPTOR> {
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = null_mut();
    // SAFETY: token points to valid storage and the pseudo process handle is valid.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }

    let result = (|| {
        let mut required = 0;
        // SAFETY: a null buffer is the documented size-query form.
        unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut required) };
        let mut buffer = vec![0u8; required as usize];
        // SAFETY: buffer is sized by the preceding query and remains live.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: TOKEN_USER is the requested structure at the buffer start.
        let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
        let mut sid_string = null_mut();
        // SAFETY: user SID comes from GetTokenInformation and output is valid storage.
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_string) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid = wide_ptr_to_string(sid_string);
        // SAFETY: ConvertSidToStringSidW allocates with LocalAlloc.
        unsafe { LocalFree(sid_string.cast::<c_void>()) };

        let sddl: Vec<u16> = format!("D:P(A;;FA;;;SY)(A;;FA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: SDDL is NUL-terminated and descriptor points to valid storage.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(descriptor)
    })();
    // SAFETY: token was initialized by OpenProcessToken.
    unsafe { CloseHandle(token) };
    result
}

#[cfg(windows)]
fn dacl_sddl(descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR) -> io::Result<String> {
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW;
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

    let mut value = null_mut();
    // SAFETY: descriptor is valid and value points to output storage.
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            1,
            DACL_SECURITY_INFORMATION,
            &mut value,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let result = wide_ptr_to_string(value);
    // SAFETY: the conversion API allocates value with LocalAlloc.
    unsafe { LocalFree(value.cast::<c_void>()) };
    Ok(result)
}

#[cfg(windows)]
fn wide_ptr_to_string(value: *const u16) -> String {
    let mut length = 0;
    // SAFETY: value is a NUL-terminated string returned by a Windows API.
    while unsafe { *value.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: length was computed within the NUL-terminated allocation.
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(value, length) })
}

#[cfg(all(test, windows))]
pub(crate) fn has_protected_dacl_for_test(path: &Path) -> io::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetFileSecurityW, GetSecurityDescriptorControl,
        SE_DACL_PROTECTED,
    };

    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut required = 0;
    // SAFETY: null buffer is the documented size-query form.
    unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION,
            null_mut(),
            0,
            &mut required,
        )
    };
    let mut descriptor = vec![0u8; required as usize];
    // SAFETY: descriptor is sized by the preceding query.
    if unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION,
            descriptor.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: descriptor contains a valid security descriptor.
    if unsafe {
        GetSecurityDescriptorControl(descriptor.as_mut_ptr().cast(), &mut control, &mut revision)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(control & SE_DACL_PROTECTED != 0)
}
