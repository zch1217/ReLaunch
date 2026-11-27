use crate::identity::{AppIdentity, AppInfo};
use std::collections::{HashMap, HashSet};
use windows::{
    core::PWSTR,
    Win32::{
        Foundation::{CloseHandle, BOOL, FILETIME, HANDLE, HWND, LPARAM, WPARAM},
        Storage::Packaging::Appx::GetApplicationUserModelId,
        System::Threading::{
            GetProcessId, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
            PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetWindowLongW, GetWindowTextLengthW, GetWindowThreadProcessId,
            IsWindowVisible, SendMessageTimeoutW, GWL_EXSTYLE, SMTO_ABORTIFHUNG, SMTO_BLOCK,
            WM_NULL, WS_EX_TOOLWINDOW,
        },
    },
};

/// Discovers currently running desktop applications by enumerating visible windows.
///
/// Grouping rules:
/// - Packaged (MSIX) apps are grouped by AUMID. Helper processes without an explicit
///   AUMID are attached only when their process ancestry traces back to an explicit
///   AUMID ancestor (checked for PID reuse via creation times).
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
/// - MSIX: processes matching the target AUMID explicitly, plus unlabeled helper
///   processes whose verified ancestor chain leads to that AUMID.
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
        let mut creation_times = HashMap::new();
        if procs.iter().any(|p| p.aumid.is_none()) {
            for p in &procs {
                if let Some(ct) = query_process_creation_time(p.pid) {
                    creation_times.insert(p.pid, ct);
                }
            }
            for p in procs.iter().filter(|p| p.aumid.is_none()) {
                let mut cur = p.pid;
                let mut visited = HashSet::new();
                for _ in 0..64 {
                    let Some(&ppid) = parent_map.get(&cur) else {
                        break;
                    };
                    if ppid == 0 || ppid == cur || !visited.insert(ppid) {
                        break;
                    }
                    if !creation_times.contains_key(&ppid) {
                        if let Some(ct) = query_process_creation_time(ppid) {
                            creation_times.insert(ppid, ct);
                        }
                    }
                    cur = ppid;
                }
            }
        }
        let pids = select_msix_pids_for_aumid(aumid, &procs, &parent_map, &creation_times);
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

/// Verify that an on-disk executable matches the canonical Win32 `AppIdentity`.
pub(crate) fn exe_matches_win32_identity(
    exe_path: &str,
    exe_name: &str,
    product_name: &str,
) -> bool {
    let path = std::path::Path::new(exe_path);
    if !path.is_file() {
        return false;
    }
    if exe_path.to_lowercase().starts_with(&windows_dir()) {
        return false;
    }
    let file_ok = path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(exe_name));
    if !file_ok {
        return false;
    }
    let actual = read_product_name(exe_path).unwrap_or_else(|| exe_stem(exe_path));
    product_name_matches(&actual, product_name)
}

/// Verify an already-open process handle against `identity` before terminating,
/// preventing accidental termination when a PID is reused between discovery and kill.
///
/// Reuses an optional pre-captured `parent_map` and already-opened sibling handles
/// (`open_handles`) when walking MSIX helper ancestry.
pub(crate) fn verify_handle_matches_identity(
    handle: HANDLE,
    identity: &AppIdentity,
    parent_map: Option<&HashMap<u32, u32>>,
    open_handles: &HashMap<u32, HANDLE>,
) -> bool {
    match identity {
        AppIdentity::Msix { aumid } => {
            if let Some(proc_aumid) = query_aumid_handle(handle) {
                return proc_aumid.eq_ignore_ascii_case(aumid);
            }
            let path = query_exe_path_handle(handle);
            if let Some(path_aumid) = path.as_deref().and_then(aumid_from_windowsapps_exe) {
                return path_aumid.eq_ignore_ascii_case(aumid);
            }
            let target_family = aumid.split('!').next().unwrap_or(aumid);
            let in_family = query_package_family_handle(handle)
                .or_else(|| path.as_deref().and_then(package_family_from_exe_path))
                .is_some_and(|f| f.eq_ignore_ascii_case(target_family));
            if !in_family {
                return false;
            }

            let pid = unsafe { GetProcessId(handle) };
            let Some(mut cur_ctime) = query_process_creation_time_handle(handle) else {
                return false;
            };
            if pid == 0 {
                return false;
            }
            let owned_parent_map;
            let pmap = match parent_map {
                Some(m) => m,
                None => {
                    owned_parent_map = snapshot_processes().into_iter().collect();
                    &owned_parent_map
                }
            };

            let mut cur = pid;
            let mut visited = HashSet::new();
            for _ in 0..64 {
                let Some(&ppid) = pmap.get(&cur) else {
                    break;
                };
                if ppid == 0 || ppid == cur || !visited.insert(ppid) {
                    break;
                }
                let (anc_handle, owned) = if let Some(&h) = open_handles.get(&ppid) {
                    (h, false)
                } else {
                    let Ok(h) =
                        (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, ppid) })
                    else {
                        break;
                    };
                    (h, true)
                };
                let anc_info = (|| -> Option<(u64, Option<String>)> {
                    let anc_ctime = query_process_creation_time_handle(anc_handle)?;
                    if anc_ctime > cur_ctime {
                        return None;
                    }
                    let anc_aumid = query_aumid_handle(anc_handle).or_else(|| {
                        query_exe_path_handle(anc_handle)
                            .and_then(|p| aumid_from_windowsapps_exe(&p))
                    });
                    Some((anc_ctime, anc_aumid))
                })();
                if owned {
                    unsafe {
                        let _ = CloseHandle(anc_handle);
                    }
                }
                match anc_info {
                    None => break,
                    Some((_, Some(anc_aumid))) => {
                        return anc_aumid.eq_ignore_ascii_case(aumid);
                    }
                    Some((anc_ctime, None)) => {
                        cur = ppid;
                        cur_ctime = anc_ctime;
                    }
                }
            }
            false
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
    let mut cur_ctime = query_process_creation_time(pid)?;

    let mut cur = pid;
    let mut visited = HashSet::new();
    for _ in 0..64 {
        let Some(&ppid) = parent_map.get(&cur) else {
            break;
        };
        if ppid == 0 || ppid == cur || !visited.insert(ppid) {
            break;
        }
        let Some(anc_ctime) = query_process_creation_time(ppid) else {
            break;
        };
        if anc_ctime > cur_ctime {
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
            return None;
        }
        cur = ppid;
        cur_ctime = anc_ctime;
    }

    None
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
    creation_times: &HashMap<u32, u64>,
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
            if let (Some(&cur_ct), Some(&par_ct)) =
                (creation_times.get(&cur), creation_times.get(&ppid))
            {
                if par_ct > cur_ct {
                    break;
                }
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
        if resolved == Some(true) {
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
pub(crate) fn snapshot_processes() -> Vec<(u32, u32)> {
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
pub(crate) fn enumerate_desktop_windows() -> Vec<u32> {
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

        // Query all translation blocks in order, then fall back to common Unicode blocks.
        for lang_cp in query_translations(&buf) {
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

/// Extract all `(language << 16 | codepage)` pairs from `\VarFileInfo\Translation`,
/// appending common Unicode fallbacks if absent.
unsafe fn query_translations(buf: &[u8]) -> Vec<u32> {
    use windows::Win32::Storage::FileSystem::VerQueryValueW;
    use windows::core::PCWSTR;

    let mut out = Vec::new();
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
        let count = (vlen as usize) / 4;
        let pairs = std::slice::from_raw_parts(pval as *const u32, count);
        for &pair in pairs {
            let lang = pair & 0xFFFF;
            let cp = pair >> 16;
            let combined = (lang << 16) | cp;
            if !out.contains(&combined) {
                out.push(combined);
            }
        }
    }
    for fallback in [0x0409_04B0_u32, 0x0000_04B0_u32] {
        if !out.contains(&fallback) {
            out.push(fallback);
        }
    }
    out
}

/// Process creation time in 100-ns intervals since 1601-01-01, or None if unreadable.
pub(crate) fn query_process_creation_time(pid: u32) -> Option<u64> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let ctime = query_process_creation_time_handle(handle);
        let _ = CloseHandle(handle);
        ctime
    }
}

pub(crate) fn query_process_creation_time_handle(handle: HANDLE) -> Option<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user).ok()?;
    }
    Some(((creation.dwHighDateTime as u64) << 32) | (creation.dwLowDateTime as u64))
}

/// Return true if any PID in `pids` owns a visible, titled, non-tool desktop window
/// that responds to a synchronous `WM_NULL` probe within 500 ms.
pub(crate) fn has_responsive_visible_window(pids: &[u32]) -> bool {
    let hwnds = collect_windows_for_pids(pids, true);
    any_window_responsive(&hwnds)
}

/// Return true if any PID in `pids` owns any top-level window (including hidden/tray)
/// that responds to a synchronous `WM_NULL` probe within 500 ms.
pub(crate) fn has_responsive_any_window(pids: &[u32]) -> bool {
    let hwnds = collect_windows_for_pids(pids, false);
    any_window_responsive(&hwnds)
}

fn collect_windows_for_pids(pids: &[u32], visible_desktop_only: bool) -> Vec<HWND> {
    if pids.is_empty() {
        return Vec::new();
    }
    struct Search<'a> {
        pids: &'a [u32],
        visible_desktop_only: bool,
        hwnds: Vec<HWND>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = &mut *(lparam.0 as *mut Search<'_>);
        if search.visible_desktop_only {
            if !IsWindowVisible(hwnd).as_bool() || GetWindowTextLengthW(hwnd) == 0 {
                return BOOL(1);
            }
            let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            if ex_style & WS_EX_TOOLWINDOW.0 != 0 {
                return BOOL(1);
            }
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != 0 && search.pids.contains(&pid) {
            search.hwnds.push(hwnd);
        }
        BOOL(1)
    }

    let mut search = Search {
        pids,
        visible_desktop_only,
        hwnds: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut search as *mut Search<'_> as isize));
    }
    search.hwnds
}

fn any_window_responsive(hwnds: &[HWND]) -> bool {
    for &hwnd in hwnds {
        let mut res: usize = 0;
        let ok = unsafe {
            SendMessageTimeoutW(
                hwnd,
                WM_NULL,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                500,
                Some(&mut res),
            )
        };
        if ok.0 != 0 {
            return true;
        }
    }
    false
}

/// Parse the PE header on disk to determine whether `exe_path` targets the
/// Windows GUI subsystem (`IMAGE_SUBSYSTEM_WINDOWS_GUI == 2`).
pub(crate) fn is_gui_subsystem(exe_path: &str) -> Option<bool> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(exe_path).ok()?;
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos).ok()?;
    if &dos[0..2] != b"MZ" {
        return None;
    }
    let e_lfanew = u32::from_le_bytes([dos[0x3C], dos[0x3D], dos[0x3E], dos[0x3F]]);
    if e_lfanew < 64 {
        return None;
    }

    file.seek(SeekFrom::Start(e_lfanew as u64)).ok()?;
    // 4 (PE\0\0) + 20 (COFF) + 72 (Optional Header through Subsystem at offset 68..70)
    let mut pe = [0u8; 96];
    file.read_exact(&mut pe).ok()?;
    if &pe[0..4] != b"PE\0\0" {
        return None;
    }
    let magic = u16::from_le_bytes([pe[24], pe[25]]);
    if magic != 0x10B && magic != 0x20B {
        return None;
    }
    let subsystem = u16::from_le_bytes([pe[92], pe[93]]);
    Some(subsystem == 2)
}

/// Choose root PIDs from `pids`, preferring visible-desktop-window owners and
/// earliest creation times.
pub(crate) fn select_root_pids(pids: &[u32]) -> Vec<u32> {
    let mut unique = Vec::new();
    let mut pid_set = HashSet::new();
    for &pid in pids {
        if pid != 0 && pid_set.insert(pid) {
            unique.push(pid);
        }
    }
    if unique.is_empty() {
        return Vec::new();
    }

    let parent_map: HashMap<u32, u32> = snapshot_processes().into_iter().collect();
    let mut roots: Vec<u32> = unique
        .iter()
        .copied()
        .filter(|&pid| match parent_map.get(&pid) {
            Some(&ppid) if ppid != pid && pid_set.contains(&ppid) => {
                match (
                    query_process_creation_time(ppid),
                    query_process_creation_time(pid),
                ) {
                    (Some(par_ct), Some(child_ct)) => par_ct > child_ct,
                    _ => false,
                }
            }
            _ => true,
        })
        .collect();
    if roots.is_empty() {
        roots = unique;
    }

    let desktop_pids: HashSet<u32> = enumerate_desktop_windows().into_iter().collect();
    roots.sort_by_key(|&pid| {
        (
            !desktop_pids.contains(&pid),
            query_process_creation_time(pid).unwrap_or(u64::MAX),
            pid,
        )
    });
    roots
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
        // Single-app package: unlabeled helper IS selected when ancestor traces to direct,
        // NOT selected when no labeled ancestor exists (no package-family fallback).
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
            MsixProcInfo {
                pid: 102,
                aumid: None,
                family: family.clone(),
            },
        ];
        let mut single_parents = HashMap::new();
        single_parents.insert(101, 100);
        single_parents.insert(102, 1); // no labeled ancestor
        assert_eq!(
            select_msix_pids_for_aumid(
                "Suite_abc!App1",
                &single_procs,
                &single_parents,
                &HashMap::new()
            ),
            vec![100, 101]
        );

        // Unlabeled helpers alone (no direct AUMID) must return empty.
        let unlabeled_only = vec![MsixProcInfo {
            pid: 200,
            aumid: None,
            family: family.clone(),
        }];
        assert_eq!(
            select_msix_pids_for_aumid(
                "Suite_abc!App1",
                &unlabeled_only,
                &HashMap::new(),
                &HashMap::new()
            ),
            Vec::<u32>::new()
        );

        // Multi-app package: multi-hop ancestry, orphan exclusion, and PID-reuse creation time guard.
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
                pid: 12,
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
                family: family.clone(),
            },
            MsixProcInfo {
                pid: 40,
                aumid: None,
                family,
            },
        ];
        let mut parents = HashMap::new();
        parents.insert(11, 10);
        parents.insert(12, 11); // multi-hop: 12 -> 11 -> 10
        parents.insert(21, 20);
        parents.insert(30, 1); // unrelated orphan
        parents.insert(40, 10); // claims parent 10, but creation time shows PID reuse

        let mut ctimes = HashMap::new();
        ctimes.insert(10, 500);
        ctimes.insert(11, 510);
        ctimes.insert(12, 520);
        ctimes.insert(40, 400); // older than parent 10 -> PID 10 was reused

        assert_eq!(
            select_msix_pids_for_aumid("Suite_abc!App1", &multi_procs, &parents, &ctimes),
            vec![10, 11, 12]
        );
        assert_eq!(
            select_msix_pids_for_aumid("Suite_abc!App2", &multi_procs, &parents, &ctimes),
            vec![20, 21]
        );
    }

    #[test]
    fn exe_matches_win32_identity_cases() {
        let exe = std::env::current_exe().expect("current_exe");
        let exe_str = exe.to_string_lossy();
        let exe_name = exe.file_name().unwrap().to_string_lossy().to_lowercase();
        let product = read_product_name(&exe_str).unwrap_or_else(|| exe_stem(&exe_str));

        assert!(exe_matches_win32_identity(&exe_str, &exe_name, &product));
        assert!(!exe_matches_win32_identity(
            &exe_str,
            &exe_name,
            "Definitely Wrong Product Name"
        ));
        assert!(!exe_matches_win32_identity(
            &exe_str,
            "other_name.exe",
            &product
        ));

        let dir_str = exe.parent().unwrap().to_string_lossy();
        assert!(!exe_matches_win32_identity(&dir_str, &exe_name, &product));
        assert!(!exe_matches_win32_identity(
            r"C:\NonExistent\missing_app.exe",
            "missing_app.exe",
            "Missing App"
        ));

        let cmd = std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into());
        if std::path::Path::new(&cmd).is_file() {
            let cmd_product = read_product_name(&cmd).unwrap_or_else(|| "cmd".into());
            assert!(!exe_matches_win32_identity(&cmd, "cmd.exe", &cmd_product));
        }
    }

    #[test]
    fn is_gui_subsystem_detects_console_and_gui() {
        let cmd = std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into());
        assert_eq!(is_gui_subsystem(&cmd), Some(false));

        let explorer = format!(
            "{}\\explorer.exe",
            std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())
        );
        if std::path::Path::new(&explorer).is_file() {
            assert_eq!(is_gui_subsystem(&explorer), Some(true));
        }

        assert_eq!(is_gui_subsystem(r"C:\NonExistent\missing.exe"), None);
    }
}
