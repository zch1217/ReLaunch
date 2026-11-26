use windows::{
    Win32::{
        Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, Process32FirstW, Process32NextW,
                PROCESSENTRY32W, TH32CS_SNAPPROCESS,
            },
            Threading::{
                OpenProcess, TerminateProcess, WaitForMultipleObjects,
                PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
            },
        },
        UI::WindowsAndMessaging::{},
    },
};

#[derive(Debug)]
pub enum CloseResult {
    /// All processes in the tree exited cleanly.
    Exited,
    /// Access denied opening one or more processes (target may be elevated).
    AccessDenied,
    /// Other OS error.
    Error(String),
}

/// Kill all processes in `pids` immediately via TerminateProcess,
/// then wait up to 2 seconds for them to exit.
pub fn close_gracefully(pids: &[u32]) -> CloseResult {
    if pids.is_empty() {
        return CloseResult::Exited;
    }

    // 1. Open handles for terminate + wait.
    let mut handles: Vec<HANDLE> = Vec::new();
    for &pid in pids {
        unsafe {
            match OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid) {
                Ok(h) => handles.push(h),
                Err(e) if e.code() == windows::Win32::Foundation::E_ACCESSDENIED => {
                    for h in &handles {
                        let _ = CloseHandle(*h);
                    }
                    return CloseResult::AccessDenied;
                }
                // Process already exited between discovery and kill — nothing to do.
                Err(_) => {}
            }
        }
    }

    // 2. Terminate all.
    for &h in &handles {
        unsafe { let _ = TerminateProcess(h, 1); }
    }

    // 3. Wait up to 2 s for all to exit.
    let result = wait_all(&handles, 2000);

    for h in &handles {
        unsafe { let _ = CloseHandle(*h); }
    }

    result
}

/// Force-terminate all processes in `pids` via TerminateProcess.
/// Only called after explicit user confirmation.
pub fn force_kill(pids: &[u32]) -> Result<(), String> {
    let mut last_err: Option<String> = None;
    for &pid in pids {
        unsafe {
            match OpenProcess(PROCESS_TERMINATE, false, pid) {
                Ok(h) => {
                    if TerminateProcess(h, 1).is_err() {
                        last_err = Some(format!("TerminateProcess failed for PID {pid}"));
                    }
                    let _ = CloseHandle(h);
                }
                Err(e) => {
                    last_err = Some(format!("OpenProcess failed for PID {pid}: {e}"));
                }
            }
        }
    }
    if let Some(e) = last_err {
        Err(e)
    } else {
        Ok(())
    }
}

/// Returns true if any process in `pids` is still running.
pub fn is_running(pid: u32) -> bool {
    unsafe {
        match OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            Ok(h) => {
                use windows::Win32::System::Threading::WaitForSingleObject;
                let r = WaitForSingleObject(h, 0);
                let _ = CloseHandle(h);
                r != WAIT_OBJECT_0
            }
            Err(_) => false,
        }
    }
}

/// Collect the full process tree rooted at `root_pid` (root + all descendants).
pub fn collect_process_tree(root_pid: u32) -> Vec<u32> {
    let mut result = vec![root_pid];
    let snapshot = unsafe {
        match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return result,
        }
    };

    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    // Gather all (pid, parent_pid) pairs.
    let mut all: Vec<(u32, u32)> = Vec::new();
    unsafe {
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                all.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }

    // BFS to find all descendants.
    let mut queue = vec![root_pid];
    while let Some(parent) = queue.pop() {
        for &(pid, ppid) in &all {
            if ppid == parent && !result.contains(&pid) {
                result.push(pid);
                queue.push(pid);
            }
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

            let r = unsafe {
                WaitForMultipleObjects(chunk, true, remaining.min(timeout_ms))
            };

            if r == WAIT_OBJECT_0 {
                break; // All in this chunk exited.
            } else if r == WAIT_TIMEOUT {
                // Timeout after direct terminate — treat as exited (processes may be
                // zombie or protected; nothing more to do).
                break;
            } else {
                return CloseResult::Error(format!("WaitForMultipleObjects returned {:?}", r));
            }
        }
    }
    CloseResult::Exited
}
