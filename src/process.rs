use std::collections::{HashMap, HashSet};
use windows::Win32::{
    Foundation::{
        CloseHandle, GetLastError, ERROR_NOT_GUI_PROCESS, E_ACCESSDENIED, HANDLE, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    },
    System::Diagnostics::Debug::ReadProcessMemory,
    System::Threading::{
        OpenProcess, TerminateProcess, WaitForInputIdle, WaitForMultipleObjects,
        WaitForSingleObject, PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, PROCESS_VM_READ,
    },
};

use crate::identity::AppIdentity;

const TERMINATE_WAIT_TIMEOUT_MS: u32 = 5000;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023_u32 as i32;

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationProcess(
        process_handle: HANDLE,
        process_information_class: u32,
        process_information: *mut std::ffi::c_void,
        process_information_length: u32,
        return_length: *mut u32,
    ) -> i32;
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CloseResult {
    /// All targeted processes exited.
    Exited,
    /// Timed out waiting for one or more processes to exit after termination.
    Timeout,
    /// Access denied opening or terminating one or more processes (target may be elevated).
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
    let mut opened: Vec<(u32, OwnedHandle)> = Vec::new();
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
                Ok(h) => opened.push((pid, OwnedHandle(h))),
                Err(e) if e.code() == E_ACCESSDENIED => return CloseResult::AccessDenied,
                // Process already exited between discovery and kill — nothing to do.
                Err(_) => {}
            }
        }
    }

    if opened.is_empty() {
        return CloseResult::Exited;
    }

    // 2. Verify all opened handles against `identity` (keeping sibling handles open for MSIX ancestry).
    let open_map: HashMap<u32, HANDLE> = opened.iter().map(|(pid, oh)| (*pid, oh.0)).collect();
    let parent_map = if identity.is_msix() {
        Some(
            crate::discovery::snapshot_processes()
                .into_iter()
                .collect::<HashMap<u32, u32>>(),
        )
    } else {
        None
    };

    let keep: Vec<bool> = opened
        .iter()
        .map(|(pid, oh)| {
            let ok = crate::discovery::verify_handle_matches_identity(
                oh.0,
                identity,
                parent_map.as_ref(),
                &open_map,
            );
            if !ok {
                tracing::warn!("跳过身份不匹配的 PID {pid}（可能已退出或被复用）");
            }
            ok
        })
        .collect();

    let verified: Vec<(u32, OwnedHandle)> = opened
        .into_iter()
        .zip(keep)
        .filter_map(|(item, ok)| if ok { Some(item) } else { None })
        .collect();

    if verified.is_empty() {
        return CloseResult::Exited;
    }

    // 3. Terminate all verified handles and wait up to 5 s for all to exit.
    terminate_verified_handles(&verified, TERMINATE_WAIT_TIMEOUT_MS)
}

fn terminate_verified_handles(verified: &[(u32, OwnedHandle)], timeout_ms: u32) -> CloseResult {
    let mut term_errors = Vec::new();
    for (pid, oh) in verified {
        if let Err(e) = unsafe { TerminateProcess(oh.0, 1) } {
            tracing::warn!("TerminateProcess failed for PID {pid}: {e}");
            term_errors.push((*pid, oh.0, e));
        }
    }

    for (pid, h, e) in term_errors {
        // Windows returns ERROR_ACCESS_DENIED when TerminateProcess races with an
        // already-exiting process (e.g. a child exiting right after its parent).
        let already_exited = unsafe { WaitForSingleObject(h, 250) == WAIT_OBJECT_0 };
        if !already_exited {
            if e.code() == E_ACCESSDENIED {
                return CloseResult::AccessDenied;
            }
            return CloseResult::Error(format!("TerminateProcess failed for PID {pid}: {e}"));
        }
    }

    let raw_handles: Vec<HANDLE> = verified.iter().map(|(_, oh)| oh.0).collect();
    wait_all(&raw_handles, timeout_ms)
}

/// Guard: returns true when `pid` is this process itself.
pub fn is_self(pid: u32) -> bool {
    pid == std::process::id()
}

/// Return true if `pid` can be opened and has not yet exited.
pub(crate) fn is_process_alive(pid: u32) -> bool {
    let Ok(h) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        return false;
    };
    let _guard = OwnedHandle(h);
    unsafe { WaitForSingleObject(h, 0) == WAIT_TIMEOUT }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputIdleStatus {
    /// At least one live root GUI process is waiting for user input.
    GuiIdle,
    /// Only non-GUI (e.g. console) processes were observed.
    NonGuiOnly,
    /// Live GUI process(es) are still initializing or status could not be confirmed.
    StillInitializing,
}

/// Probe `WaitForInputIdle` on live processes in `pids` without mistaking
/// an already-exited process or a console process for an initialized GUI app.
pub(crate) fn probe_input_idle(pids: &[u32]) -> InputIdleStatus {
    let mut any_gui_idle = false;
    let mut any_gui_busy = false;
    let mut any_non_gui = false;

    for &pid in pids {
        let Ok(h) = (unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }) else {
            continue;
        };
        let _guard = OwnedHandle(h);

        let alive_before = unsafe { WaitForSingleObject(h, 0) == WAIT_TIMEOUT };
        if !alive_before {
            continue;
        }

        let res = unsafe { WaitForInputIdle(h, 50) };
        if res == 0 {
            // WaitForInputIdle can also return 0 if the target exits; re-check liveness.
            let alive_after = unsafe { WaitForSingleObject(h, 0) == WAIT_TIMEOUT };
            if alive_after {
                any_gui_idle = true;
            }
        } else if res == WAIT_TIMEOUT.0 {
            any_gui_busy = true;
        } else {
            let err = unsafe { GetLastError() };
            if err == ERROR_NOT_GUI_PROCESS {
                any_non_gui = true;
            } else if unsafe { WaitForSingleObject(h, 0) == WAIT_TIMEOUT } {
                any_gui_busy = true;
            }
        }
    }

    if any_gui_idle {
        InputIdleStatus::GuiIdle
    } else if any_gui_busy {
        InputIdleStatus::StillInitializing
    } else if any_non_gui {
        InputIdleStatus::NonGuiOnly
    } else {
        InputIdleStatus::StillInitializing
    }
}

/// Capture launch arguments and working directory from the primary live process
/// before terminating it, so direct EXE relaunches preserve original context.
pub(crate) fn capture_launch_context(
    pids: &[u32],
    fallback_exe: Option<&str>,
) -> crate::launcher::LaunchContext {
    let root_pids = crate::discovery::select_root_pids(pids);
    for pid in root_pids {
        let source_exe = crate::discovery::query_exe_path(pid)
            .or_else(|| fallback_exe.map(str::to_string));
        let args = query_process_command_line(pid)
            .and_then(|raw| extract_command_line_args(&raw, source_exe.as_deref()))
            .filter(|a| !looks_like_internal_child_args(a));
        let working_dir = query_process_working_dir(pid);
        if args.is_some() || working_dir.is_some() {
            return crate::launcher::LaunchContext {
                args,
                working_dir,
                source_exe,
            };
        }
    }
    crate::launcher::LaunchContext {
        args: None,
        working_dir: None,
        source_exe: fallback_exe.map(str::to_string),
    }
}

/// Read the raw command line of `pid` via `NtQueryInformationProcess(ProcessCommandLineInformation)`.
pub(crate) fn query_process_command_line(pid: u32) -> Option<String> {
    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *const u16,
    }

    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()? };
    let _guard = OwnedHandle(h);

    let mut buf = vec![0u8; 2048];
    let mut ret_len: u32 = 0;
    let mut status = unsafe {
        NtQueryInformationProcess(
            h,
            60, // ProcessCommandLineInformation
            buf.as_mut_ptr() as _,
            buf.len() as u32,
            &mut ret_len,
        )
    };
    if (status == STATUS_INFO_LENGTH_MISMATCH || status == STATUS_BUFFER_TOO_SMALL)
        && ret_len > buf.len() as u32
        && ret_len <= 65536
    {
        buf.resize(ret_len as usize, 0);
        status = unsafe {
            NtQueryInformationProcess(
                h,
                60,
                buf.as_mut_ptr() as _,
                buf.len() as u32,
                &mut ret_len,
            )
        };
    }
    if status < 0 || buf.len() < std::mem::size_of::<UnicodeString>() {
        return None;
    }

    let us = unsafe { &*(buf.as_ptr() as *const UnicodeString) };
    let byte_len = us.length as usize;
    if byte_len == 0 || byte_len % 2 != 0 || us.buffer.is_null() {
        return None;
    }
    let base_addr = buf.as_ptr() as usize;
    let str_addr = us.buffer as usize;
    let offset = str_addr.checked_sub(base_addr)?;
    if offset.checked_add(byte_len)? > buf.len() {
        return None;
    }
    let slice = unsafe { std::slice::from_raw_parts(us.buffer, byte_len / 2) };
    let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    let text = String::from_utf16_lossy(&slice[..end]).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Read the current working directory of `pid` from its PEB (handling WoW64 32-bit processes).
pub(crate) fn query_process_working_dir(pid: u32) -> Option<String> {
    let h = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid).ok()? };
    let _guard = OwnedHandle(h);

    let bytes = read_peb_current_directory_bytes(h)?;
    let raw = decode_utf16_le_bytes(&bytes);
    let normalized = normalize_dir_path(&raw);
    if !normalized.is_empty() && std::path::Path::new(&normalized).is_dir() {
        Some(normalized)
    } else {
        None
    }
}

fn read_peb_current_directory_bytes(handle: HANDLE) -> Option<Vec<u8>> {
    #[repr(C)]
    #[derive(Default)]
    struct ProcessBasicInformation {
        reserved1: usize,
        peb_base_address: usize,
        reserved2: [usize; 2],
        unique_process_id: usize,
        reserved3: usize,
    }

    #[cfg(target_pointer_width = "64")]
    {
        let mut wow64_peb: usize = 0;
        let mut ret_len: u32 = 0;
        let st_wow = unsafe {
            NtQueryInformationProcess(
                handle,
                26, // ProcessWow64Information
                &mut wow64_peb as *mut usize as _,
                std::mem::size_of::<usize>() as u32,
                &mut ret_len,
            )
        };
        if st_wow >= 0 && wow64_peb != 0 {
            return read_cwd_from_peb32(handle, wow64_peb);
        }

        let mut pbi = ProcessBasicInformation::default();
        let st = unsafe {
            NtQueryInformationProcess(
                handle,
                0, // ProcessBasicInformation
                &mut pbi as *mut ProcessBasicInformation as _,
                std::mem::size_of::<ProcessBasicInformation>() as u32,
                &mut ret_len,
            )
        };
        if st < 0 || pbi.peb_base_address == 0 {
            return None;
        }
        read_cwd_from_peb64(handle, pbi.peb_base_address)
    }

    #[cfg(target_pointer_width = "32")]
    {
        let mut pbi = ProcessBasicInformation::default();
        let mut ret_len: u32 = 0;
        let st = unsafe {
            NtQueryInformationProcess(
                handle,
                0,
                &mut pbi as *mut ProcessBasicInformation as _,
                std::mem::size_of::<ProcessBasicInformation>() as u32,
                &mut ret_len,
            )
        };
        if st < 0 || pbi.peb_base_address == 0 {
            return None;
        }
        read_cwd_from_peb32(handle, pbi.peb_base_address)
    }
}

fn read_remote_bytes(handle: HANDLE, addr: usize, size: usize) -> Option<Vec<u8>> {
    if addr == 0 || size == 0 || size > 65536 {
        return None;
    }
    let mut buf = vec![0u8; size];
    let mut read: usize = 0;
    unsafe {
        ReadProcessMemory(
            handle,
            addr as *const std::ffi::c_void,
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            size,
            Some(&mut read),
        )
        .ok()?;
    }
    if read == size { Some(buf) } else { None }
}

fn read_cwd_from_peb32(handle: HANDLE, peb32_addr: usize) -> Option<Vec<u8>> {
    let pp_bytes = read_remote_bytes(handle, peb32_addr.checked_add(0x10)?, 4)?;
    let pp = u32::from_le_bytes(pp_bytes.try_into().ok()?) as usize;
    let us_bytes = read_remote_bytes(handle, pp.checked_add(0x24)?, 8)?;
    let len = u16::from_le_bytes([us_bytes[0], us_bytes[1]]) as usize;
    let buf_ptr = u32::from_le_bytes([us_bytes[4], us_bytes[5], us_bytes[6], us_bytes[7]]) as usize;
    if len == 0 || len % 2 != 0 || len > 65536 || buf_ptr == 0 {
        return None;
    }
    read_remote_bytes(handle, buf_ptr, len)
}

#[cfg(target_pointer_width = "64")]
fn read_cwd_from_peb64(handle: HANDLE, peb64_addr: usize) -> Option<Vec<u8>> {
    let pp_bytes = read_remote_bytes(handle, peb64_addr.checked_add(0x20)?, 8)?;
    let pp = u64::from_le_bytes(pp_bytes.try_into().ok()?) as usize;
    let us_bytes = read_remote_bytes(handle, pp.checked_add(0x38)?, 16)?;
    let len = u16::from_le_bytes([us_bytes[0], us_bytes[1]]) as usize;
    let buf_ptr = u64::from_le_bytes(us_bytes[8..16].try_into().ok()?) as usize;
    if len == 0 || len % 2 != 0 || len > 65536 || buf_ptr == 0 {
        return None;
    }
    read_remote_bytes(handle, buf_ptr, len)
}

fn decode_utf16_le_bytes(bytes: &[u8]) -> String {
    let u16s: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let end = u16s.iter().position(|&c| c == 0).unwrap_or(u16s.len());
    String::from_utf16_lossy(&u16s[..end])
}

fn normalize_dir_path(s: &str) -> String {
    let trimmed = s.trim_end_matches('\0').trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let stripped = trimmed.trim_end_matches(['\\', '/']);
    if stripped.len() == 2 && stripped.as_bytes()[1] == b':' {
        format!("{stripped}\\")
    } else if stripped.is_empty() {
        trimmed.to_string()
    } else {
        stripped.to_string()
    }
}

/// Strip the leading executable path from a Windows command line string, returning
/// only the argument tail if non-empty.
pub(crate) fn extract_command_line_args(
    raw_cmdline: &str,
    exe_path: Option<&str>,
) -> Option<String> {
    let trimmed = raw_cmdline.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    let rest = if let Some(after_open) = trimmed.strip_prefix('"') {
        match after_open.find('"') {
            Some(end_quote) => &after_open[end_quote + 1..],
            None => return None,
        }
    } else if let Some(exe) = exe_path {
        let exe_trimmed = exe.trim();
        if !exe_trimmed.is_empty()
            && trimmed.len() >= exe_trimmed.len()
            && trimmed.is_char_boundary(exe_trimmed.len())
            && trimmed[..exe_trimmed.len()].eq_ignore_ascii_case(exe_trimmed)
            && trimmed[exe_trimmed.len()..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace)
        {
            &trimmed[exe_trimmed.len()..]
        } else {
            match trimmed.find(char::is_whitespace) {
                Some(ws) => &trimmed[ws..],
                None => return None,
            }
        }
    } else {
        match trimmed.find(char::is_whitespace) {
            Some(ws) => &trimmed[ws..],
            None => return None,
        }
    };

    let args = rest.trim();
    if args.is_empty() { None } else { Some(args.to_string()) }
}

/// Reject command lines that clearly belong to Chromium/Electron internal child
/// processes rather than user-facing startup flags.
pub(crate) fn looks_like_internal_child_args(args: &str) -> bool {
    let lower = args.to_ascii_lowercase();
    [
        "--type=renderer",
        "--type=gpu-process",
        "--type=utility",
        "--type=crashpad-handler",
        "--type=zygote",
        "--type=broker",
        "--type=ppapi",
        "--mojo-platform-channel-handle=",
        "--service-pipe-token=",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
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

    #[test]
    fn terminate_handles_reports_access_denied_without_waiting_full_timeout() {
        let guard = spawn_pause_cmd();
        let pid = guard.0.id();
        // Open without PROCESS_TERMINATE so TerminateProcess fails with E_ACCESSDENIED.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.expect("OpenProcess");
        let verified = vec![(pid, OwnedHandle(handle))];
        let started = std::time::Instant::now();
        let res = terminate_verified_handles(&verified, 5000);
        assert_eq!(res, CloseResult::AccessDenied);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(1500),
            "should report AccessDenied promptly without waiting full timeout"
        );
    }

    #[test]
    fn terminate_handles_ignores_error_on_already_exited_process() {
        let cmd = std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into());
        let mut child = Command::new(cmd)
            .args(["/C", "exit", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn cmd.exe");
        let pid = child.id();
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.expect("OpenProcess");
        let _ = child.wait();
        let verified = vec![(pid, OwnedHandle(handle))];
        assert_eq!(terminate_verified_handles(&verified, 500), CloseResult::Exited);
    }

    #[test]
    fn extract_command_line_args_cases() {
        assert_eq!(
            extract_command_line_args(
                r#""C:\Program Files\App\app.exe" --minimized --profile "Default""#,
                None
            ),
            Some(r#"--minimized --profile "Default""#.to_string())
        );
        assert_eq!(
            extract_command_line_args(
                r#"C:\Tools\app.exe /flag value"#,
                Some(r#"C:\Tools\app.exe"#)
            ),
            Some("/flag value".to_string())
        );
        assert_eq!(
            extract_command_line_args(
                r#"C:\Program Files\My App\app.exe --port 8080"#,
                Some(r#"C:\Program Files\My App\app.exe"#)
            ),
            Some("--port 8080".to_string())
        );
        assert_eq!(
            extract_command_line_args(
                r#""C:\Program Files\App\app.exe""#,
                Some(r#"C:\Program Files\App\app.exe"#)
            ),
            None
        );
        assert_eq!(
            extract_command_line_args(r#"C:\Tools\app.exe"#, Some(r#"C:\Tools\app.exe"#)),
            None
        );
    }

    #[test]
    fn internal_child_args_filtered() {
        assert!(looks_like_internal_child_args(
            "--type=renderer --lang=en-US"
        ));
        assert!(looks_like_internal_child_args(
            "--type=gpu-process --mojo-platform-channel-handle=1234"
        ));
        assert!(!looks_like_internal_child_args(
            "--minimized --config \"C:\\App\\cfg.json\""
        ));
    }

    #[test]
    fn capture_launch_context_reads_real_64bit_and_32bit_process() {
        let temp_dir = std::env::temp_dir();
        let temp_norm = normalize_dir_path(&temp_dir.to_string_lossy());
        let sys_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());

        let mut candidates = vec![format!(r"{sys_root}\System32\cmd.exe")];
        let wow64_cmd = format!(r"{sys_root}\SysWOW64\cmd.exe");
        if std::path::Path::new(&wow64_cmd).is_file() {
            candidates.push(wow64_cmd);
        }

        for cmd in candidates {
            let child = Command::new(&cmd)
                .args(["/C", "pause"])
                .current_dir(&temp_dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .expect("spawn cmd.exe");
            let guard = ChildGuard(child);
            let pid = guard.0.id();

            let ctx = capture_launch_context(&[pid], Some(&cmd));
            assert_eq!(ctx.args.as_deref(), Some("/C pause"), "cmd={cmd}");
            assert!(
                ctx.working_dir
                    .as_deref()
                    .is_some_and(|w| w.eq_ignore_ascii_case(&temp_norm)),
                "cmd={cmd}, got={:?}, expected={}",
                ctx.working_dir,
                temp_norm
            );
        }
    }
}
