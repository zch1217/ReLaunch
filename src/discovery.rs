use crate::identity::{AppIdentity, AppInfo};
use std::collections::{HashMap, HashSet};
use windows::{
    core::PWSTR,
    Win32::{
        Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM},
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
    let mut aumid_to_key: HashMap<String, String> = HashMap::new();
    let mut family_to_keys: HashMap<String, Vec<String>> = HashMap::new();
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

        let (key, identity, display_name) = match query_aumid(pid).or_else(|| aumid_from_windowsapps_exe(&exe_path)) {
            Some(aumid) => {
                let display = read_product_name(&exe_path)
                    .unwrap_or_else(|| package_short_name(&aumid));
                let family = aumid.split('!').next().unwrap_or(&aumid).to_lowercase();
                let key = format!("msix:{}", aumid.to_lowercase());
                aumid_to_key.insert(aumid.to_lowercase(), key.clone());
                let keys = family_to_keys.entry(family).or_default();
                if !keys.contains(&key) {
                    keys.push(key.clone());
                }
                (key, AppIdentity::Msix { aumid }, display)
            }
            None => {
                let identity = win32_identity(&exe_path);
                let display = identity.display_name().to_string();
                let key = format!("exe:{exe_lower}");
                path_to_key.insert(exe_lower.clone(), key.clone());
                (key, identity, display)
            }
        };

        let entry = groups.entry(key).or_insert_with(|| {
            let icon = crate::icon_extract::extract_app_icon(&identity, Some(&exe_path));
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
    let snapshot = snapshot_processes();
    let parent_map: HashMap<u32, u32> = snapshot.iter().copied().collect();
    let mut pid_to_key: HashMap<u32, String> = HashMap::new();
    for (key, info) in &groups {
        for &pid in &info.pids {
            pid_to_key.insert(pid, key.clone());
        }
    }

    for (pid, _ppid) in snapshot {
        if pid == self_pid || pid == 0 || session_of(pid) != my_session {
            continue;
        }
        let key: Option<String> = if let Some(family) = query_package_family(pid) {
            let exe_path = query_exe_path(pid);
            let proc_aumid = query_aumid(pid)
                .or_else(|| exe_path.as_deref().and_then(aumid_from_windowsapps_exe));
            resolve_msix_group_key(
                pid,
                &family,
                proc_aumid.as_deref(),
                &aumid_to_key,
                &family_to_keys,
                &pid_to_key,
                &parent_map,
            )
        } else if let Some(p) = query_exe_path(pid) {
            if let Some(k) = path_to_key.get(&p.to_lowercase()) {
                Some(k.clone())
            } else if !family_to_keys.is_empty() {
                if let Some(family) = package_family_from_exe_path(&p) {
                    let proc_aumid = aumid_from_windowsapps_exe(&p);
                    resolve_msix_group_key(
                        pid,
                        &family,
                        proc_aumid.as_deref(),
                        &aumid_to_key,
                        &family_to_keys,
                        &pid_to_key,
                        &parent_map,
                    )
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some(ref k) = key {
            if let Some(app) = groups.get_mut(k) {
                if !app.pids.contains(&pid) {
                    app.pids.push(pid);
                }
                pid_to_key.insert(pid, k.clone());
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
    let snapshot = snapshot_processes();

    if let AppIdentity::Msix { aumid } = identity {
        let target_family = aumid.split('!').next().unwrap_or(aumid);
        let parent_map: HashMap<u32, u32> = snapshot.iter().copied().collect();
        let mut procs = Vec::new();
        for &(pid, _) in &snapshot {
            if pid == self_pid || pid == 0 || session_of(pid) != my_session {
                continue;
            }
            let exe_path = query_exe_path(pid);
            let family = query_package_family(pid)
                .or_else(|| exe_path.as_deref().and_then(package_family_from_exe_path));
            let Some(family) = family else { continue };
            if !family.eq_ignore_ascii_case(target_family) {
                continue;
            }
            let proc_aumid = query_aumid(pid)
                .or_else(|| exe_path.as_deref().and_then(aumid_from_windowsapps_exe));
            procs.push(MsixProcInfo {
                pid,
                aumid: proc_aumid,
                family,
            });
        }
        let pids = select_msix_pids_for_aumid(aumid, &procs, &parent_map);
        if pids.is_empty() {
            return None;
        }
        let exe_path = pids.iter().find_map(|&pid| query_exe_path(pid));
        let mut info = AppInfo::new(identity.clone(), identity.display_name().to_string());
        info.exe_path = exe_path;
        info.running = true;
        info.pids = pids;
        return Some(info);
    }

    // Product names are read from disk; cache them per path for this scan.
    let mut product_cache: HashMap<String, String> = HashMap::new();
    let mut pids = Vec::new();
    let mut exe_path: Option<String> = None;

    for (pid, _ppid) in snapshot {
        if pid == self_pid || pid == 0 || session_of(pid) != my_session {
            continue;
        }
        let matched = match identity {
            AppIdentity::Msix { .. } => unreachable!(),
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
                    let actual = product_cache
                        .entry(lower)
                        .or_insert_with(|| read_product_name(&path).unwrap_or_else(|| exe_stem(&path)));
                    product_name_matches(actual, product_name)
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
pub(crate) fn windows_dir() -> String {
    let dir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    format!(r"{}\", dir.trim_end_matches('\\').to_lowercase())
}

/// Strict Win32 ProductName match:
/// - Reject empty strings.
/// - Exact case-insensitive equality matches.
/// - Otherwise strips at most one trailing whitespace-separated version token
///   (e.g. `1.2.3`, `v2.0`, `2026`) and compares exact equality.
pub(crate) fn product_name_matches(actual: &str, expected: &str) -> bool {
    let a = actual.trim();
    let e = expected.trim();
    if a.is_empty() || e.is_empty() {
        return false;
    }
    if a.eq_ignore_ascii_case(e) {
        return true;
    }
    let na = normalize_product_name(a);
    let ne = normalize_product_name(e);
    !na.is_empty() && na == ne
}

fn normalize_product_name(s: &str) -> String {
    let trimmed = s.trim();
    if let Some((prefix, suffix)) = trimmed.rsplit_once(char::is_whitespace) {
        let prefix = prefix.trim_end();
        let suffix = suffix.trim();
        let ver = suffix
            .strip_prefix('v')
            .or_else(|| suffix.strip_prefix('V'))
            .unwrap_or(suffix);
        let is_version = !prefix.is_empty()
            && !ver.is_empty()
            && ver.as_bytes()[0].is_ascii_digit()
            && ver.bytes().all(|b| b.is_ascii_digit() || b == b'.');
        if is_version {
            return prefix.to_lowercase();
        }
    }
    trimmed.to_lowercase()
}

/// Verify an already-open process handle against `identity` before terminating,
/// preventing accidental termination when a PID is reused between discovery and kill.
pub(crate) fn handle_matches_identity(handle: HANDLE, identity: &AppIdentity) -> bool {
    match identity {
        AppIdentity::Msix { aumid } => {
            if let Some(proc_aumid) = query_aumid_handle(handle) {
                return proc_aumid.eq_ignore_ascii_case(aumid);
            }
            let target_family = aumid.split('!').next().unwrap_or(aumid);
            if let Some(path) = query_exe_path_handle(handle) {
                if let Some(path_aumid) = aumid_from_windowsapps_exe(&path) {
                    return path_aumid.eq_ignore_ascii_case(aumid);
                }
                return query_package_family_handle(handle)
                    .or_else(|| package_family_from_exe_path(&path))
                    .is_some_and(|f| f.eq_ignore_ascii_case(target_family));
            }
            query_package_family_handle(handle)
                .is_some_and(|f| f.eq_ignore_ascii_case(target_family))
        }
        AppIdentity::Win32 {
            exe_name,
            product_name,
        } => {
            let Some(path) = query_exe_path_handle(handle) else {
                return false;
            };
            if path.to_lowercase().starts_with(&windows_dir()) {
                return false;
            }
            let file_ok = std::path::Path::new(&path)
                .file_name()
                .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(exe_name));
            if !file_ok {
                return false;
            }
            let actual = read_product_name(&path).unwrap_or_else(|| exe_stem(&path));
            product_name_matches(&actual, product_name)
        }
        AppIdentity::Manual { path } => {
            query_exe_path_handle(handle).is_some_and(|p| p.eq_ignore_ascii_case(path))
        }
    }
}

fn resolve_msix_group_key(
    pid: u32,
    family: &str,
    proc_aumid: Option<&str>,
    aumid_to_key: &HashMap<String, String>,
    family_to_keys: &HashMap<String, Vec<String>>,
    pid_to_key: &HashMap<u32, String>,
    parent_map: &HashMap<u32, u32>,
) -> Option<String> {
    if let Some(aumid) = proc_aumid {
        return aumid_to_key.get(&aumid.to_lowercase()).cloned();
    }
    let fam_lower = family.to_lowercase();
    let keys = family_to_keys.get(&fam_lower)?;

    let mut cur = pid;
    let mut visited = HashSet::new();
    for _ in 0..64 {
        let Some(&ppid) = parent_map.get(&cur) else {
            break;
        };
        if ppid == 0 || ppid == cur || !visited.insert(ppid) {
            break;
        }
        if let Some(anc_key) = pid_to_key.get(&ppid) {
            if keys.contains(anc_key) {
                return Some(anc_key.clone());
            }
        }
        if let Some(anc_aumid) = query_aumid(ppid)
            .or_else(|| query_exe_path(ppid).and_then(|p| aumid_from_windowsapps_exe(&p)))
        {
            let anc_fam = anc_aumid.split('!').next().unwrap_or(&anc_aumid);
            if anc_fam.eq_ignore_ascii_case(family) {
                return aumid_to_key.get(&anc_aumid.to_lowercase()).cloned();
            }
        }
        cur = ppid;
    }

    if keys.len() == 1 {
        Some(keys[0].clone())
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MsixProcInfo {
    pub(crate) pid: u32,
    pub(crate) aumid: Option<String>,
    pub(crate) family: String,
}

pub(crate) fn select_msix_pids_for_aumid(
    target_aumid: &str,
    procs: &[MsixProcInfo],
    parent_map: &HashMap<u32, u32>,
) -> Vec<u32> {
    let target_family = target_aumid.split('!').next().unwrap_or(target_aumid);
    let mut direct = HashSet::new();
    let mut sibling = HashSet::new();
    let mut unlabeled = Vec::new();

    for proc in procs {
        if !proc.family.eq_ignore_ascii_case(target_family) {
            continue;
        }
        match &proc.aumid {
            Some(a) if a.eq_ignore_ascii_case(target_aumid) => {
                direct.insert(proc.pid);
            }
            Some(_) => {
                sibling.insert(proc.pid);
            }
            None => {
                unlabeled.push(proc.pid);
            }
        }
    }

    let mut selected: Vec<u32> = direct.iter().copied().collect();
    for pid in unlabeled {
        let mut cur = pid;
        let mut visited = HashSet::new();
        let mut resolved = None;
        for _ in 0..64 {
            let Some(&ppid) = parent_map.get(&cur) else {
                break;
            };
            if ppid == 0 || ppid == cur || !visited.insert(ppid) {
                break;
            }
            if direct.contains(&ppid) {
                resolved = Some(true);
                break;
            }
            if sibling.contains(&ppid) {
                resolved = Some(false);
                break;
            }
            cur = ppid;
        }
        if resolved.unwrap_or_else(|| sibling.is_empty()) {
            selected.push(pid);
        }
    }

    selected.sort_unstable();
    selected.dedup();
    selected
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
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let name = query_package_family_handle(handle);
        let _ = CloseHandle(handle);
        name
    }
}

fn query_package_family_handle(handle: HANDLE) -> Option<String> {
    use windows::Win32::Storage::Packaging::Appx::GetPackageFamilyName;
    unsafe {
        let mut len: u32 = 0;
        let _ = GetPackageFamilyName(handle, &mut len, PWSTR::null());
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        let result = GetPackageFamilyName(handle, &mut len, PWSTR(buf.as_mut_ptr()));
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
pub(crate) fn query_exe_path(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let path = query_exe_path_handle(handle);
        let _ = CloseHandle(handle);
        path
    }
}

pub(crate) fn query_exe_path_handle(handle: HANDLE) -> Option<String> {
    unsafe {
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
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
        let aumid = query_aumid_handle(handle);
        let _ = CloseHandle(handle);
        aumid
    }
}

fn query_aumid_handle(handle: HANDLE) -> Option<String> {
    unsafe {
        let mut len: u32 = 0;
        // First call to get the required buffer length.
        let _ = GetApplicationUserModelId(handle, &mut len, PWSTR::null());
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        let result = GetApplicationUserModelId(handle, &mut len, PWSTR(buf.as_mut_ptr()));
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

/// Build the canonical Win32 `AppIdentity` from an executable path.
pub(crate) fn win32_identity(exe_path: &str) -> AppIdentity {
    let product_name = read_product_name(exe_path).unwrap_or_else(|| exe_stem(exe_path));
    AppIdentity::Win32 {
        exe_name: exe_stem_lower(exe_path),
        product_name,
    }
}

pub(crate) fn exe_stem(path: &str) -> String {
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

// ---------------------------------------------------------------------------
// WindowsApps path fallback for unpackaged-looking MSIX processes
// ---------------------------------------------------------------------------

/// Resolve `"Family!AppId"` for an executable under `WindowsApps\<PackageFullName>\...`
/// by reading the package's `AppxManifest.xml`.
pub(crate) fn aumid_from_windowsapps_exe(exe_path: &str) -> Option<String> {
    let (pkg_root, full_name, rel_exe) = split_windowsapps_path(exe_path)?;
    let family = package_family_from_full_name(full_name)?;
    let manifest_path = std::path::Path::new(pkg_root).join("AppxManifest.xml");
    let xml = std::fs::read_to_string(manifest_path).ok()?;
    let app_id = parse_manifest_app_id(&xml, rel_exe)?;
    Some(format!("{family}!{app_id}"))
}

fn package_family_from_exe_path(exe_path: &str) -> Option<String> {
    let (_, full_name, _) = split_windowsapps_path(exe_path)?;
    package_family_from_full_name(full_name)
}

/// Split `...\WindowsApps\<PackageFullName>\<rel_exe>` into
/// `(package_root, package_full_name, rel_exe)`.
fn split_windowsapps_path(exe_path: &str) -> Option<(&str, &str, &str)> {
    let bytes = exe_path.as_bytes();
    let mut segs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b'\\' || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b'\\' && bytes[i] != b'/' {
            i += 1;
        }
        segs.push((start, i));
    }

    let idx = segs
        .iter()
        .position(|&(s, e)| exe_path[s..e].eq_ignore_ascii_case("WindowsApps"))?;
    let &(pkg_start, pkg_end) = segs.get(idx + 1)?;
    let full_name = &exe_path[pkg_start..pkg_end];
    let pkg_root = &exe_path[..pkg_end];
    let rel_exe = exe_path[pkg_end..].trim_start_matches(['\\', '/']);
    if full_name.is_empty() || rel_exe.is_empty() {
        return None;
    }
    Some((pkg_root, full_name, rel_exe))
}

fn package_family_from_full_name(full_name: &str) -> Option<String> {
    use windows::Win32::{
        Foundation::ERROR_SUCCESS,
        Storage::Packaging::Appx::PackageFamilyNameFromFullName,
    };
    use windows::core::PCWSTR;

    let wide: Vec<u16> = full_name.encode_utf16().chain([0]).collect();
    unsafe {
        let mut len: u32 = 0;
        let _ = PackageFamilyNameFromFullName(PCWSTR(wide.as_ptr()), &mut len, PWSTR::null());
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize];
        let err = PackageFamilyNameFromFullName(
            PCWSTR(wide.as_ptr()),
            &mut len,
            PWSTR(buf.as_mut_ptr()),
        );
        if err != ERROR_SUCCESS {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let family = String::from_utf16_lossy(&buf[..end]);
        if family.is_empty() {
            None
        } else {
            Some(family)
        }
    }
}

fn parse_manifest_app_id(xml: &str, rel_exe: &str) -> Option<String> {
    fn norm(s: &str) -> String {
        s.replace('/', "\\").trim_start_matches('\\').to_lowercase()
    }

    let target = norm(rel_exe);
    let bytes = xml.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if xml[i..].starts_with("<!--") {
            if let Some(end_rel) = xml[i + 4..].find("-->") {
                i += 4 + end_rel + 3;
            } else {
                break;
            }
            continue;
        }
        if xml[i..].starts_with("<Application")
            && bytes.get(i + 12).is_some_and(|b| b.is_ascii_whitespace())
        {
            let mut j = i + 12;
            let mut quote: Option<u8> = None;
            while j < bytes.len() {
                let b = bytes[j];
                if let Some(q) = quote {
                    if b == q {
                        quote = None;
                    }
                } else if b == b'"' || b == b'\'' {
                    quote = Some(b);
                } else if b == b'>' {
                    break;
                }
                j += 1;
            }
            if j >= bytes.len() {
                break;
            }
            let tag = &xml[i + 12..j];
            if let (Some(id), Some(exe)) = (
                extract_xml_attr(tag, "Id"),
                extract_xml_attr(tag, "Executable"),
            ) {
                if norm(exe) == target {
                    return Some(id.to_string());
                }
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }
    None
}

fn extract_xml_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let attr_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'>'
            && bytes[i] != b'/'
        {
            i += 1;
        }
        if i == attr_start {
            i += 1;
            continue;
        }
        let attr_name = &tag[attr_start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) != Some(&b'=') {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let Some(&q) = bytes.get(i) else { break };
        if q != b'"' && q != b'\'' {
            continue;
        }
        i += 1;
        let val_start = i;
        while i < bytes.len() && bytes[i] != q {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let val = &tag[val_start..i];
        i += 1;
        if attr_name.eq_ignore_ascii_case(name) {
            return Some(val);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_windowsapps_path_cases() {
        assert_eq!(
            split_windowsapps_path(
                r"C:\Program Files\WindowsApps\Claude_6.28899.6.0_x64__pzs8sxrjxfjjc\app\claude.exe"
            ),
            Some((
                r"C:\Program Files\WindowsApps\Claude_6.28899.6.0_x64__pzs8sxrjxfjjc",
                "Claude_6.28899.6.0_x64__pzs8sxrjxfjjc",
                r"app\claude.exe"
            ))
        );
        assert_eq!(
            split_windowsapps_path(
                "C:/Program Files/WindowsApps/Pkg_1.0_x64__abc/app/app.exe"
            ),
            Some((
                "C:/Program Files/WindowsApps/Pkg_1.0_x64__abc",
                "Pkg_1.0_x64__abc",
                "app/app.exe"
            ))
        );
        assert_eq!(
            split_windowsapps_path(
                r"D:\测试目录\WindowsApps\Pkg_1.0_x64__abc\bin\run.exe"
            ),
            Some((
                r"D:\测试目录\WindowsApps\Pkg_1.0_x64__abc",
                "Pkg_1.0_x64__abc",
                r"bin\run.exe"
            ))
        );
        assert_eq!(
            split_windowsapps_path(r"C:\Program Files\App\app.exe"),
            None
        );
    }

    #[test]
    fn package_family_from_full_name_cases() {
        assert_eq!(
            package_family_from_full_name("Claude_6.28899.6.0_x64__pzs8sxrjxfjjc"),
            Some("Claude_pzs8sxrjxfjjc".to_string())
        );
        assert_eq!(
            package_family_from_full_name("OpenAI.Codex_26.1120.6357.0_x64__2p2nqsd0c76g0"),
            Some("OpenAI.Codex_2p2nqsd0c76g0".to_string())
        );
        assert_eq!(package_family_from_full_name("not-a-valid-package"), None);
    }

    #[test]
    fn parse_manifest_app_id_cases() {
        let xml = r#"
<Package>
  <!-- <Application Id="Commented" Executable="app\Claude.exe" /> -->
  <Applications>
    <Application Id="SshAskpass" Executable="app\resources\claude-ssh-askpass.exe" EntryPoint="Windows.FullTrustApplication" />
    <Application Id="Claude" Executable="app\Claude.exe" EntryPoint="Windows.FullTrustApplication">
    </Application>
    <Application
      Id="App"
      Executable="app/ChatGPT.exe"
      EntryPoint="Windows.FullTrustApplication" />
  </Applications>
</Package>
"#;
        assert_eq!(
            parse_manifest_app_id(xml, r"app\claude.exe"),
            Some("Claude".to_string())
        );
        assert_eq!(
            parse_manifest_app_id(xml, r"app\ChatGPT.exe"),
            Some("App".to_string())
        );
        assert_eq!(
            parse_manifest_app_id(xml, r"app\other.exe"),
            None
        );
    }

    #[test]
    fn product_name_matching_cases() {
        assert!(product_name_matches("Visual Studio Code", "visual studio code"));
        assert!(product_name_matches("Foo 1.2.3", "Foo 1.2.4"));
        assert!(product_name_matches("Foo v2.0", "Foo"));
        assert!(product_name_matches("Foo 2026", "Foo"));

        assert!(!product_name_matches("", "Foo"));
        assert!(!product_name_matches("Foo", ""));
        assert!(!product_name_matches("Code", "Visual Studio Code"));
        assert!(!product_name_matches("Visual Studio Code", "Code"));
        assert!(!product_name_matches("App", "SuperApp"));
        assert!(!product_name_matches("Foo Pro", "Foo"));
    }

    #[test]
    fn select_msix_pids_multi_app_package() {
        let family = "Suite_abc".to_string();
        // Single-app package: unlabeled helper without labeled ancestor still belongs.
        let single_procs = vec![
            MsixProcInfo {
                pid: 100,
                aumid: Some("Suite_abc!App1".to_string()),
                family: family.clone(),
            },
            MsixProcInfo {
                pid: 101,
                aumid: None,
                family: family.clone(),
            },
        ];
        assert_eq!(
            select_msix_pids_for_aumid("Suite_abc!App1", &single_procs, &HashMap::new()),
            vec![100, 101]
        );

        // Multi-app package: App1 and App2 run together, each with unlabeled helpers.
        let multi_procs = vec![
            MsixProcInfo {
                pid: 10,
                aumid: Some("Suite_abc!App1".to_string()),
                family: family.clone(),
            },
            MsixProcInfo {
                pid: 11,
                aumid: None,
                family: family.clone(),
            },
            MsixProcInfo {
                pid: 20,
                aumid: Some("Suite_abc!App2".to_string()),
                family: family.clone(),
            },
            MsixProcInfo {
                pid: 21,
                aumid: None,
                family: family.clone(),
            },
            MsixProcInfo {
                pid: 30,
                aumid: None,
                family,
            },
        ];
        let mut parents = HashMap::new();
        parents.insert(11, 10);
        parents.insert(21, 20);
        parents.insert(30, 1); // unrelated orphan

        assert_eq!(
            select_msix_pids_for_aumid("Suite_abc!App1", &multi_procs, &parents),
            vec![10, 11]
        );
        assert_eq!(
            select_msix_pids_for_aumid("Suite_abc!App2", &multi_procs, &parents),
            vec![20, 21]
        );
    }
}
