use crate::identity::{AppIdentity, AppInfo};
use std::collections::HashMap;
use windows::{
    core::PWSTR,
    Win32::{
        Foundation::{CloseHandle, BOOL, HWND, LPARAM},
        Storage::Packaging::Appx::GetApplicationUserModelId,
        System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetWindowLongW, GetWindowTextLengthW, GetWindowThreadProcessId,
            IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
        },
    },
};

/// Discovers currently running desktop applications by enumerating visible windows.
///
/// Grouping rules:
/// - Packaged (MSIX) apps are grouped by AUMID. All processes of the same package
///   family in the current session belong to the app, even with different EXE
///   names (Codex: codex.exe, codex-code-mode-host.exe, ...).
/// - Win32 apps are grouped by full EXE path. All processes with the same path in
///   the current session belong to the app (Electron / Chrome child processes).
///
/// Processes in other sessions (services, session 0) are never included.
pub fn discover_running_apps() -> Vec<AppInfo> {
    let self_pid = std::process::id();
    let my_session = session_of(self_pid);

    // Key: "msix:<aumid>" or "exe:<lowercase path>".
    let mut groups: HashMap<String, AppInfo> = HashMap::new();
    // Lookup tables for expanding groups to full process sets.
    let mut family_to_key: HashMap<String, String> = HashMap::new();
    let mut path_to_key: HashMap<String, String> = HashMap::new();

    // Step 1: seed groups from processes that own visible desktop windows.
    for pid in enumerate_desktop_windows() {
        if pid == self_pid {
            continue;
        }
        let Some(exe_path) = query_exe_path(pid) else { continue };
        let exe_lower = exe_path.to_lowercase();
        // OS components (ApplicationFrameHost, Settings, TextInputHost, ...) are not
        // apps the user wants to restart.
        if exe_lower.starts_with(&windows_dir()) {
            continue;
        }

        let (key, identity, display_name) = match query_aumid(pid) {
            Some(aumid) => {
                let display = read_product_name(&exe_path)
                    .unwrap_or_else(|| package_short_name(&aumid));
                let family = aumid.split('!').next().unwrap_or(&aumid).to_lowercase();
                let key = format!("msix:{}", aumid.to_lowercase());
                family_to_key.insert(family, key.clone());
                (key, AppIdentity::Msix { aumid }, display)
            }
            None => {
                let product = read_product_name(&exe_path)
                    .unwrap_or_else(|| exe_stem(&exe_path));
                let key = format!("exe:{exe_lower}");
                path_to_key.insert(exe_lower.clone(), key.clone());
                let identity = AppIdentity::Win32 {
                    exe_name: exe_stem_lower(&exe_path),
                    product_name: product.clone(),
                };
                (key, identity, product)
            }
        };

        let entry = groups.entry(key).or_insert_with(|| {
            let icon = crate::icon_extract::extract_exe_icon(&exe_path);
            let mut info = AppInfo::new(identity, display_name);
            info.exe_path = Some(exe_path.clone());
            info.icon = icon;
            info.running = true;
            info
        });
        if !entry.pids.contains(&pid) {
            entry.pids.push(pid);
        }
    }

    // Step 2: expand each group to every related process in the same session.
    for (pid, _ppid) in snapshot_processes() {
        if pid == self_pid || pid == 0 || session_of(pid) != my_session {
            continue;
        }
        let key = match query_package_family(pid) {
            Some(family) => family_to_key.get(&family.to_lowercase()),
            None => query_exe_path(pid).and_then(|p| path_to_key.get(&p.to_lowercase())),
        };
        if let Some(app) = key.and_then(|k| groups.get_mut(k)) {
            if !app.pids.contains(&pid) {
                app.pids.push(pid);
            }
        }
    }

    let mut apps: Vec<AppInfo> = groups.into_values().collect();
    for app in &mut apps {
        app.pids.sort_unstable();
    }
    apps.sort_by(|a, b| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()));
    apps
}

/// Re-discover `identity` right before an operation. None when not running.
///
/// Window-based discovery first (it groups exactly like the list the user saw);
/// if the app has no visible window — closed to the tray, or started minimized —
/// fall back to scanning processes by identity.
pub fn current_app(identity: &AppIdentity) -> Option<AppInfo> {
    discover_running_apps()
        .into_iter()
        .find(|a| &a.identity == identity)
        .or_else(|| find_by_identity(identity))
}

/// Find a running app by identity alone, without requiring a visible window.
/// Used for favorites that live in the system tray (WeChat, Feishu, CC Switch...).
///
/// - MSIX: every process of the same package family.
/// - Win32: every process whose file name equals `exe_name` and whose
///   ProductName (when present) agrees with `product_name`.
///
/// Only processes in the current session are considered; never this process.
pub fn find_by_identity(identity: &AppIdentity) -> Option<AppInfo> {
    let self_pid = std::process::id();
    let my_session = session_of(self_pid);
    let sys_dir = windows_dir();

    // Product names are read from disk; cache them per path for this scan.
    let mut product_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut pids = Vec::new();
    let mut exe_path: Option<String> = None;

    for (pid, _ppid) in snapshot_processes() {
        if pid == self_pid || pid == 0 || session_of(pid) != my_session {
            continue;
        }
        let matched = match identity {
            AppIdentity::Msix { aumid } => {
                let family = aumid.split('!').next().unwrap_or(aumid);
                query_package_family(pid)
                    .map(|f| f.eq_ignore_ascii_case(family))
                    .unwrap_or(false)
            }
            AppIdentity::Win32 { exe_name, product_name } => {
                let Some(path) = query_exe_path(pid) else { continue };
                let lower = path.to_lowercase();
                let file_ok = std::path::Path::new(&lower)
                    .file_name()
                    .map(|n| n.to_string_lossy() == exe_name.as_str())
                    .unwrap_or(false);
                if !file_ok || lower.starts_with(&sys_dir) {
                    false
                } else {
                    let pn = product_cache
                        .entry(lower)
                        .or_insert_with(|| read_product_name(&path));
                    match pn {
                        Some(pn) => {
                            let (pn, want) = (pn.to_lowercase(), product_name.to_lowercase());
                            pn.contains(&want) || want.contains(&pn)
                        }
                        None => true,
                    }
                }
            }
            AppIdentity::Manual { path } => query_exe_path(pid)
                .map(|p| p.eq_ignore_ascii_case(path))
                .unwrap_or(false),
        };
        if matched {
            pids.push(pid);
            if exe_path.is_none() {
                exe_path = query_exe_path(pid);
            }
        }
    }

    if pids.is_empty() {
        return None;
    }
    pids.sort_unstable();
    let mut info = AppInfo::new(identity.clone(), identity.display_name().to_string());
    info.exe_path = exe_path;
    info.running = true;
    info.pids = pids;
    Some(info)
}

/// Window-based discovery plus any `favorites` that are running without a
/// visible window. Favorites found this way take the given display name.
pub fn discover_with_favorites(favorites: &[(AppIdentity, String)]) -> Vec<AppInfo> {
    let mut apps = discover_running_apps();
    for (identity, name) in favorites {
        if apps.iter().any(|a| &a.identity == identity) {
            continue;
        }
        if let Some(mut info) = find_by_identity(identity) {
            info.display_name = name.clone();
            apps.push(info);
        }
    }
    apps.sort_by(|a, b| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()));
    apps
}

/// Lowercase Windows directory with trailing backslash, e.g. `c:\windows\`.
fn windows_dir() -> String {
    let dir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    format!(r"{}\", dir.trim_end_matches('\\').to_lowercase())
}

/// "OpenAI.Codex_2p2nqsd0c76g0!App" -> "Codex"
fn package_short_name(aumid: &str) -> String {
    let family = aumid.split('!').next().unwrap_or(aumid);
    let name = family.rsplit_once('_').map(|(n, _)| n).unwrap_or(family);
    name.rsplit('.').next().unwrap_or(name).to_string()
}

/// All (pid, parent_pid) pairs currently on the system.
fn snapshot_processes() -> Vec<(u32, u32)> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                out.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Terminal-services session of `pid`; `u32::MAX` if unknown.
fn session_of(pid: u32) -> u32 {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let mut session = u32::MAX;
    unsafe {
        if ProcessIdToSessionId(pid, &mut session).is_err() {
            return u32::MAX;
        }
    }
    session
}

/// Package family name of a packaged process, or None for unpackaged processes.
fn query_package_family(pid: u32) -> Option<String> {
    use windows::Win32::Storage::Packaging::Appx::GetPackageFamilyName;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut len: u32 = 0;
        let _ = GetPackageFamilyName(handle, &mut len, PWSTR::null());
        if len == 0 {
            let _ = CloseHandle(handle);
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        let result = GetPackageFamilyName(handle, &mut len, PWSTR(buf.as_mut_ptr()));
        let _ = CloseHandle(handle);
        if result.is_ok() {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let name = String::from_utf16_lossy(&buf[..end]);
            if name.is_empty() { None } else { Some(name) }
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Window enumeration
// ---------------------------------------------------------------------------

/// Returns the set of unique PIDs that own visible, meaningful desktop windows.
fn enumerate_desktop_windows() -> Vec<u32> {
    let mut pids: Vec<u32> = Vec::new();

    unsafe {
        let _ = EnumWindows(
            Some(enum_windows_callback),
            LPARAM(&mut pids as *mut Vec<u32> as isize),
        );
    }

    pids.sort_unstable();
    pids.dedup();
    pids
}

unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let pids = &mut *(lparam.0 as *mut Vec<u32>);

    // Must be visible.
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }

    // Must have a non-empty title.
    if GetWindowTextLengthW(hwnd) == 0 {
        return BOOL(1);
    }

    // Skip tool windows (tray items, overlays, tooltips).
    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    if ex_style & WS_EX_TOOLWINDOW.0 != 0 {
        return BOOL(1);
    }

    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));

    if pid != 0 {
        pids.push(pid);
    }

    BOOL(1) // continue enumeration
}

// ---------------------------------------------------------------------------
// Process information
// ---------------------------------------------------------------------------

/// Returns the full path of the executable for `pid`, or None on failure.
fn query_exe_path(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(handle);
        if result.is_ok() && len > 0 {
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        } else {
            None
        }
    }
}

/// Returns the Application User Model ID for `pid` if it is a packaged (MSIX) app.
fn query_aumid(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut len: u32 = 0;
        // First call to get the required buffer length.
        let _ = GetApplicationUserModelId(handle, &mut len, PWSTR::null());
        if len == 0 {
            let _ = CloseHandle(handle);
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        let result = GetApplicationUserModelId(handle, &mut len, PWSTR(buf.as_mut_ptr()));
        let _ = CloseHandle(handle);
        if result.is_ok() && len > 0 {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(len as usize);
            let aumid = String::from_utf16_lossy(&buf[..end]).to_string();
            if aumid.is_empty() { None } else { Some(aumid) }
        } else {
            None
        }
    }
}

/// Reads the `ProductName` field from a PE version resource.
/// Returns None if the resource is absent or unreadable.
pub fn read_product_name(exe_path: &str) -> Option<String> {
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };
    use windows::core::PCWSTR;

    let wide: Vec<u16> = exe_path.encode_utf16().chain([0]).collect();

    unsafe {
        let mut dummy: u32 = 0;
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), Some(&mut dummy));
        if size == 0 {
            return None;
        }

        let mut buf = vec![0u8; size as usize];
        if GetFileVersionInfoW(PCWSTR(wide.as_ptr()), 0, size, buf.as_mut_ptr() as _).is_err() {
            return None;
        }

        // Query the language-neutral translation block first, then fall back to 040904B0 (English/Unicode).
        for lang_cp in [query_first_translation(&buf).unwrap_or(0x040904B0_u32)] {
            let lang = (lang_cp >> 16) as u16;
            let cp   = (lang_cp & 0xFFFF) as u16;
            let subblock: Vec<u16> = format!("\\StringFileInfo\\{lang:04X}{cp:04X}\\ProductName\0")
                .encode_utf16()
                .collect();
            let mut pval: *mut std::ffi::c_void = std::ptr::null_mut();
            let mut vlen: u32 = 0;
            if VerQueryValueW(
                buf.as_ptr() as _,
                PCWSTR(subblock.as_ptr()),
                &mut pval,
                &mut vlen,
            )
            .as_bool()
                && vlen > 0
                && !pval.is_null()
            {
                let slice = std::slice::from_raw_parts(pval as *const u16, vlen as usize);
                // Strip trailing NUL.
                let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
                let name = String::from_utf16_lossy(&slice[..end]).trim().to_string();
                if !name.is_empty() {
                    return Some(name);
                }
            }
        }
        None
    }
}

/// Extract the first language+codepage value from \VarFileInfo\Translation.
unsafe fn query_first_translation(buf: &[u8]) -> Option<u32> {
    use windows::Win32::Storage::FileSystem::VerQueryValueW;
    use windows::core::PCWSTR;

    let subblock: Vec<u16> = "\\VarFileInfo\\Translation\0".encode_utf16().collect();
    let mut pval: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut vlen: u32 = 0;
    if VerQueryValueW(
        buf.as_ptr() as _,
        PCWSTR(subblock.as_ptr()),
        &mut pval,
        &mut vlen,
    )
    .as_bool()
        && vlen >= 4
        && !pval.is_null()
    {
        // Translation entries are pairs of u16 (language, codepage).
        let pair = *(pval as *const u32);
        // Swap: LOWORD is language, HIWORD is codepage — compose as (lang << 16 | cp).
        let lang = (pair & 0xFFFF) as u32;
        let cp   = (pair >> 16) as u32;
        Some((lang << 16) | cp)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn exe_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

fn exe_stem_lower(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| path.to_lowercase())
}
