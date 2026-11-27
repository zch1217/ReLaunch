use std::collections::HashSet;
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{
        OpenProcess, TerminateProcess, WaitForMultipleObjects, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    },
};

use crate::identity::AppIdentity;

const TERMINATE_WAIT_TIMEOUT_MS: u32 = 5000;

#[derive(Debug, PartialEq, Eq)]
pub enum CloseResult {
    /// All targeted processes exited.
    Exited,
    /// Timed out waiting for one or more processes to exit after termination.
    Timeout,
    /// Access denied opening one or more processes (target may be elevated).
    AccessDenied,
    /// Other OS error.
    Error(String),
}

/// Forcefully terminate all processes belonging to `identity` via `TerminateProcess`
/// (intentional design for quick restarts of tray/Electron/MSIX apps), verifying
/// each open process handle against `identity` before terminating to prevent PID-reuse
/// mis-kills, then wait up to 5 seconds for every verified process to exit.
pub fn terminate_processes(identity: &AppIdentity, pids: &[u32]) -> CloseResult {
    if pids.is_empty() {
        return CloseResult::Exited;
    }

    // 1. Open handles for identity check, terminate, and wait.
    let mut seen = HashSet::new();
    let mut handles: Vec<HANDLE> = Vec::new();
    for &pid in pids {
        if pid == 0 || is_self(pid) || !seen.insert(pid) {
            continue;
        }
        unsafe {
            match OpenProcess(
                PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            ) {
                Ok(h) => {
                    if crate::discovery::handle_matches_identity(h, identity) {
                        handles.push(h);
                    } else {
                        tracing::warn!("跳过身份不匹配的 PID {pid}（可能已退出或被复用）");
                        let _ = CloseHandle(h);
                    }
                }
                Err(e) if e.code() == windows::Win32::Foundation::E_ACCESSDENIED => {
                    for opened in &handles {
                        let _ = CloseHandle(*opened);
                    }
                    return CloseResult::AccessDenied;
                }
                // Process already exited between discovery and kill — nothing to do.
                Err(_) => {}
            }
        }
    }

    if handles.is_empty() {
        return CloseResult::Exited;
    }

    // 2. Terminate all verified handles.
    for &h in &handles {
        unsafe {
            let _ = TerminateProcess(h, 1);
        }
    }

    // 3. Wait up to 5 s for all to exit.
    let result = wait_all(&handles, TERMINATE_WAIT_TIMEOUT_MS);

    for h in &handles {
        unsafe {
            let _ = CloseHandle(*h);
        }
    }

    result
}

/// Guard: returns true when `pid` is this process itself.
pub fn is_self(pid: u32) -> bool {
    pid == std::process::id()
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Wait for all handles to signal (process exit). Returns CloseResult.
fn wait_all(handles: &[HANDLE], timeout_ms: u32) -> CloseResult {
    if handles.is_empty() {
        return CloseResult::Exited;
    }
    // WaitForMultipleObjects accepts at most MAXIMUM_WAIT_OBJECTS (64) handles.
    // For the common case of < 64 processes, one call suffices.
    // For larger trees, poll in chunks.
    let chunks: Vec<&[HANDLE]> = handles.chunks(64).collect();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);

    for chunk in chunks {
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .unwrap_or_default()
                .as_millis() as u32;

            let r = unsafe { WaitForMultipleObjects(chunk, true, remaining.min(timeout_ms)) };

            if r == WAIT_OBJECT_0 {
                break; // All in this chunk exited.
            } else if r == WAIT_TIMEOUT {
                return CloseResult::Timeout;
            } else {
                return CloseResult::Error(format!("WaitForMultipleObjects returned {:?}", r));
            }
        }
    }
    CloseResult::Exited
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn spawn_pause_cmd() -> ChildGuard {
        let cmd = std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into());
        let child = Command::new(cmd)
            .args(["/C", "pause"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn cmd.exe");
        ChildGuard(child)
    }

    #[test]
    fn terminate_processes_empty_and_self_guard() {
        let self_id = AppIdentity::Manual {
            path: std::env::current_exe().unwrap().to_string_lossy().into_owned(),
        };
        assert_eq!(terminate_processes(&self_id, &[]), CloseResult::Exited);
        assert_eq!(
            terminate_processes(&self_id, &[std::process::id()]),
            CloseResult::Exited
        );
    }

    #[test]
    fn terminate_processes_verifies_identity_before_kill() {
        let mut guard = spawn_pause_cmd();
        let pid = guard.0.id();
        let actual_path = crate::discovery::query_exe_path(pid).expect("query cmd.exe path");

        // Mismatched identity must NOT terminate the process.
        let wrong_id = AppIdentity::Manual {
            path: r"C:\NonExistent\definitely_not_cmd.exe".to_string(),
        };
        assert_eq!(terminate_processes(&wrong_id, &[pid]), CloseResult::Exited);
        assert!(
            guard.0.try_wait().unwrap().is_none(),
            "mismatched identity should not kill live process"
        );

        // Matching identity must terminate it cleanly.
        let right_id = AppIdentity::Manual { path: actual_path };
        assert_eq!(terminate_processes(&right_id, &[pid]), CloseResult::Exited);
        assert!(
            guard.0.try_wait().unwrap().is_some(),
            "matching identity should terminate target process"
        );
    }

    #[test]
    fn wait_all_returns_timeout_for_running_process() {
        let guard = spawn_pause_cmd();
        let pid = guard.0.id();
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.expect("OpenProcess");
        let res = wait_all(&[handle], 50);
        unsafe {
            let _ = CloseHandle(handle);
        }
        assert_eq!(res, CloseResult::Timeout);
    }
}
