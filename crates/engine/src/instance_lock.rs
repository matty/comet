//! Single-instance lock — an exclusive advisory `flock` on `{data_dir}/engine.lock`
//! held for the engine's lifetime. Two engines sharing one data dir would race the
//! SQLite snapshots DB and the append-only run journals (WAL + `busy_timeout` guard
//! individual statements, not whole-file ownership), so the second instance must
//! fail fast with a clear error instead of corrupting state.
//!
//! The lock is taken in `EngineCore::assemble_with_identity` BEFORE any store is opened
//! and before the IPC port binds, which also closes the race where a headed app's
//! TCP probe sees no daemon during another instance's startup window.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::EngineError;

/// Held lock on the data dir. Dropping it (engine shutdown / process exit)
/// releases the advisory lock; a crash releases it too (kernel-owned).
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Acquire the exclusive lock, non-blocking. Errors with a descriptive
    /// message (including the holder's pid when readable) if another engine
    /// already owns this data dir.
    pub fn acquire(data_dir: &Path) -> Result<Self, EngineError> {
        let path = data_dir.join("engine.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Let status probes read the PID, but deny a second writer for the
            // engine's lifetime. Windows releases the share lock on crash.
            options.share_mode(1); // FILE_SHARE_READ
        }
        let mut file = options.open(&path).map_err(|error| {
            #[cfg(windows)]
            if matches!(error.raw_os_error(), Some(32 | 33)) {
                let holder = std::fs::read_to_string(&path).unwrap_or_default();
                let holder = holder.trim();
                return EngineError::AlreadyRunning {
                    data_dir: data_dir.display().to_string(),
                    pid: if holder.is_empty() {
                        "unknown".to_string()
                    } else {
                        holder.to_string()
                    },
                };
            }
            EngineError::Io(error)
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // Bounded EWOULDBLOCK retries: a fork→exec window in ANY process
            // that inherited the previous holder's fd (git scans, harness
            // spawns — fds are duplicated between fork and CLOEXEC-at-exec)
            // keeps the flock alive for a few milliseconds after release. A
            // real second engine holds it forever; transient artifacts clear
            // well within the budget.
            //
            // EINTR is unbounded and un-slept: a signal landing mid-syscall
            // is not evidence of anything, so it just retries the same
            // attempt rather than eating into the contention budget below.
            let mut retries = 40u32; // × 25ms = 1s budget
            loop {
                let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if rc == 0 {
                    break;
                }
                let errno = std::io::Error::last_os_error();
                match errno.raw_os_error() {
                    Some(libc::EINTR) => continue, // signal-interrupted: retry
                    Some(libc::EWOULDBLOCK) if retries > 0 => {
                        retries -= 1;
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                    Some(libc::EWOULDBLOCK) => {
                        let holder = std::fs::read_to_string(&path).unwrap_or_default();
                        let holder = holder.trim();
                        return Err(EngineError::AlreadyRunning {
                            data_dir: data_dir.display().to_string(),
                            pid: if holder.is_empty() {
                                "unknown".to_string()
                            } else {
                                holder.to_string()
                            },
                        });
                    }
                    // Anything else (ENOLCK, filesystem without flock, …) is an
                    // environment problem, not a second engine — surface it as-is.
                    _ => return Err(EngineError::Io(errno)),
                }
            }
        }

        // Best-effort pid stamp for the contention error message above.
        let _ = file.set_len(0);
        let _ = write!(file, "{}", std::process::id());
        let _ = file.flush();
        Ok(Self { _file: file })
    }

    /// Best-effort liveness probe: the pid stamped by the engine currently holding
    /// this data dir's lock, `None` when no engine is running (or the platform
    /// cannot test a lock without taking it). Used only for the informational
    /// `comet status` line — the real exclusivity guard is always `acquire`,
    /// never this — so a false "not running" costs a misleading status line,
    /// not a double-launch.
    ///
    /// A single non-blocking try, no contention-retry budget: a starting
    /// engine's transient fork-window artifact reads as "running", which is
    /// the safe direction while a real second engine is still plausible.
    /// But an attempt that fails for a reason that is *not* itself evidence
    /// of a holder (a signal interrupting the syscall, an unrelated handle a
    /// scanner or indexer left on the path, a stale byte-stamp nobody wrote)
    /// must not be reported as one — that was D88: `holder()` read whatever
    /// pid happened to be on disk the moment it failed to win the check,
    /// with nothing confirming the pid named a process that was still
    /// running. `pid_is_alive` is the confirmation step.
    pub fn holder(data_dir: &Path) -> Option<String> {
        let path = data_dir.join("engine.lock");
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .ok()?;
            loop {
                let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
                if rc == 0 {
                    // We took it: nothing is running. Closing the fd releases it, but
                    // unlock explicitly so the window is as small as possible.
                    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
                    return None;
                }
                match std::io::Error::last_os_error().raw_os_error() {
                    Some(libc::EINTR) => continue, // not evidence either way: retry
                    Some(libc::EWOULDBLOCK) => break,
                    // Anything else is an environment problem, not confirmed
                    // contention — do not read the pid off the back of it.
                    _ => return None,
                }
            }
            let pid = std::fs::read_to_string(&path).unwrap_or_default();
            let pid = pid.trim();
            pid_is_alive(pid).then(|| pid.to_string())
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let mut options = OpenOptions::new();
            options.read(true).write(true).share_mode(1);
            match options.open(&path) {
                Ok(_) => None,
                Err(error) if matches!(error.raw_os_error(), Some(32 | 33)) => {
                    let pid = std::fs::read_to_string(&path).unwrap_or_default();
                    let pid = pid.trim();
                    pid_is_alive(pid).then(|| pid.to_string())
                }
                Err(_) => None,
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            None
        }
    }
}

/// Best-effort check that `pid` currently names a running process on this
/// machine, used to confirm a lock-probe failure before reporting the pid it
/// found on disk as a live holder (see `InstanceLock::holder`'s doc comment).
/// An unparseable or empty pid is never alive by definition.
#[cfg(any(unix, windows))]
fn pid_is_alive(pid: &str) -> bool {
    let Ok(pid) = pid.parse::<u32>() else {
        return false;
    };
    // Real pids never reach the top half of u32 on either platform; reject
    // here rather than let the Unix cast below wrap into a negative
    // `pid_t`, which `kill` reinterprets as a process-group signal.
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    #[cfg(unix)]
    {
        // Signal 0 sends nothing; the kernel still validates the pid exists.
        // A same-user engine process is always signalable, so any failure
        // here (ESRCH: no such process; EPERM: exists but owned elsewhere)
        // is treated the same way — we did not confirm it, so it is not
        // reported as a holder.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        // SAFETY: the handle is checked for null, used only to prove it
        // opened, then closed.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            CloseHandle(handle);
            true
        }
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;

    #[test]
    fn holder_probe_reports_pid_without_disturbing_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(InstanceLock::holder(dir.path()), None, "unlocked dir");
        let lock = InstanceLock::acquire(dir.path()).expect("acquire");
        assert_eq!(
            InstanceLock::holder(dir.path()).as_deref(),
            Some(std::process::id().to_string().as_str()),
        );
        // The probe must not have stolen the lock from the holder.
        InstanceLock::acquire(dir.path()).expect_err("still held after probe");
        drop(lock);
        assert_eq!(InstanceLock::holder(dir.path()), None, "released");
    }

    #[test]
    fn second_acquire_fails_while_held_then_succeeds_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        let lock = InstanceLock::acquire(dir.path()).expect("first acquire");
        let err = InstanceLock::acquire(dir.path()).expect_err("second acquire must fail");
        let msg = err.to_string();
        assert!(msg.contains("already running"), "unexpected error: {msg}");
        assert!(
            msg.contains(&std::process::id().to_string()),
            "holder pid missing from error: {msg}"
        );
        drop(lock);
        InstanceLock::acquire(dir.path()).expect("acquire after release");
    }

    /// D88's post-drop race, reproduced without any timing dependency: a
    /// `holder()` that reads a pid off disk the moment it cannot *itself*
    /// win the exclusivity check is trusting a signal that does not
    /// uniquely mean "another engine holds this". An unrelated handle on
    /// the same path (an indexer, a virus scanner, or — per the debt row's
    /// own guess — a stale handle left over from a process that never
    /// really held `InstanceLock`) produces exactly that signal on Windows
    /// without any `InstanceLock` involved at all, and the pid left on disk
    /// (deliberately one no process can plausibly hold) is not verified
    /// before being reported. On Unix this same construction does not
    /// disturb `flock`, so the assertion holds there for a different,
    /// already-correct reason — the point of the test is the platform where
    /// it is not.
    #[test]
    fn holder_does_not_trust_a_stale_pid_it_cannot_verify_is_alive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine.lock");
        // A pid outside any real process id range on either platform.
        std::fs::write(&path, (u32::MAX - 1).to_string()).unwrap();
        let _unrelated = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        assert_eq!(
            InstanceLock::holder(dir.path()),
            None,
            "no InstanceLock is held, and the pid on disk cannot be confirmed alive"
        );
    }

    #[test]
    fn pid_is_alive_confirms_the_calling_process() {
        assert!(pid_is_alive(&std::process::id().to_string()));
    }

    #[test]
    fn pid_is_alive_rejects_a_pid_far_outside_any_real_range() {
        assert!(!pid_is_alive(&(u32::MAX - 1).to_string()));
    }

    #[test]
    fn pid_is_alive_rejects_unparseable_or_empty_input() {
        assert!(!pid_is_alive(""));
        assert!(!pid_is_alive("not-a-pid"));
    }

    #[test]
    fn pid_is_alive_rejects_zero() {
        assert!(!pid_is_alive("0"));
    }

    /// Guards the cast in the Unix arm: `pid as libc::pid_t` (an `i32`) wraps
    /// a `u32` above `i32::MAX` into a negative value, which `kill` reads as
    /// a process-*group* signal instead of a single-process existence check
    /// — a different, wrong question that could return 0 (success) for
    /// reasons unrelated to the pid we were asked about.
    #[test]
    fn pid_is_alive_rejects_a_pid_above_i32_max() {
        let above_i32_max = (i32::MAX as u32) + 1;
        assert!(!pid_is_alive(&above_i32_max.to_string()));
    }
}
