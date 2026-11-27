use crate::identity::AppIdentity;
use windows::{
    core::PCWSTR,
    Win32::{
        UI::Shell::ShellExecuteW,
        UI::WindowsAndMessaging::SW_SHOWNORMAL,
    },
};

/// A resolved launch entry — computed fresh on every restart attempt, never cached.
pub enum LaunchEntry {
    /// Launch via AUMID (MSIX / packaged apps).
    Aumid(String),
    /// Launch a .lnk shortcut as-is (keeps its arguments and working directory,
    /// e.g. Squirrel's `Update.exe --processStart App.exe`).
    Shortcut(String),
    /// Launch via resolved absolute EXE path.
    ExePath(String),
}

impl LaunchEntry {
    pub fn describe(&self) -> String {
        match self {
            LaunchEntry::Aumid(a) => format!("AUMID {a}"),
            LaunchEntry::Shortcut(p) => format!("快捷方式 {p}"),
            LaunchEntry::ExePath(p) => format!("EXE {p}"),
        }
    }
}

/// Attempt to resolve how to launch the application identified by `identity`.
///
/// `path_hint` is the EXE path of the running instance (when running) or the
/// last path the app was seen at. It is only a fallback — installer-maintained
/// entries win, because after an update they point at the new version.
///
/// Returns `None` if no valid entry can be found — in that case the caller
/// must NOT close the running application.
pub fn resolve_launch_entry(identity: &AppIdentity, path_hint: Option<&str>) -> Option<LaunchEntry> {
    match identity {
        // AUMID is independent of the versioned install directory, but verify it is still registered.
        AppIdentity::Msix { aumid } => {
            if is_aumid_registered(aumid) {
                Some(LaunchEntry::Aumid(aumid.clone()))
            } else {
                None
            }
        }
        AppIdentity::Win32 { exe_name, product_name } => {
            resolve_win32_entry(exe_name, product_name, path_hint)
        }
        AppIdentity::Manual { path } => {
            if std::path::Path::new(path).exists() {
                Some(LaunchEntry::ExePath(path.clone()))
            } else {
                None
            }
        }
    }
}

/// Verify that an MSIX/Store AUMID is currently registered in `shell:AppsFolder`.
pub(crate) fn is_aumid_registered(aumid: &str) -> bool {
    if aumid.is_empty() || !aumid.contains('!') {
        return false;
    }
    use windows::Win32::UI::Shell::{IShellItem, SHCreateItemFromParsingName};

    let _com = crate::com::ComGuard::init_sta();
    let parsing_name = format!("shell:AppsFolder\\{aumid}");
    let wide: Vec<u16> = parsing_name.encode_utf16().chain([0]).collect();
    let item: windows::core::Result<IShellItem> =
        unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) };
    item.is_ok()
}

fn resolve_win32_entry(exe_name: &str, product_name: &str, path_hint: Option<&str>) -> Option<LaunchEntry> {
    // Resolution order (re-evaluated every call, never cached):
    // 1. App Paths registry (HKCU, then HKLM)
    // 2. Start menu shortcuts (per-user, then all-users)
    // 3. Desktop shortcuts (per-user, then public)
    // 4. Running / last-known EXE path, if the file still exists
    if let Some(path) = try_app_paths_registry(exe_name) {
        return Some(LaunchEntry::ExePath(path));
    }
    for dir in shortcut_dirs() {
        if let Some(lnk) = scan_lnk_dir(&dir, exe_name, product_name, 0) {
            return Some(LaunchEntry::Shortcut(lnk));
        }
    }
    if let Some(hint) = path_hint {
        let p = std::path::Path::new(hint);
        let name_matches = p
            .file_name()
            .map(|n| n.to_string_lossy().eq_ignore_ascii_case(exe_name))
            .unwrap_or(false);
        if name_matches && p.exists() {
            return Some(LaunchEntry::ExePath(hint.to_string()));
        }
    }
    None
}

/// Check HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{exe_name}
fn try_app_paths_registry(exe_name: &str) -> Option<String> {
    let key_path = format!(
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{}",
        exe_name
    );
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ,
        REG_VALUE_TYPE,
    };

    unsafe {
        let wide_key: Vec<u16> = key_path.encode_utf16().chain([0]).collect();
        let mut hkey = windows::Win32::System::Registry::HKEY::default();
        let opened = [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE].into_iter().any(|root| {
            RegOpenKeyExW(root, PCWSTR(wide_key.as_ptr()), 0, KEY_READ, &mut hkey).is_ok()
        });
        if !opened {
            return None;
        }

        let mut data = vec![0u16; 1024];
        let mut data_size = (data.len() * 2) as u32;
        let mut vtype = REG_VALUE_TYPE::default();
        let wide_name: Vec<u16> = vec![0u16]; // empty → default value
        let result = RegQueryValueExW(
            hkey,
            PCWSTR(wide_name.as_ptr()),
            None,
            Some(&mut vtype),
            Some(data.as_mut_ptr() as _),
            Some(&mut data_size),
        );
        let _ = RegCloseKey(hkey);

        if result.is_err() {
            return None;
        }

        let len = (data_size / 2).saturating_sub(1) as usize;
        // App Paths values are sometimes quoted.
        let path = String::from_utf16_lossy(&data[..len]).trim_matches('"').to_string();
        if !path.is_empty() && std::path::Path::new(&path).exists() {
            Some(path)
        } else {
            None
        }
    }
}

/// Directories that may hold shortcuts to installed apps, most specific first.
fn shortcut_dirs() -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |p: std::path::PathBuf| {
        if p.is_dir() {
            out.push(p.to_string_lossy().into_owned());
        }
    };
    // Per-user Start menu lives under Roaming AppData.
    if let Some(d) = dirs::data_dir() {
        push(d.join(r"Microsoft\Windows\Start Menu\Programs"));
    }
    let program_data = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
    push(std::path::PathBuf::from(&program_data).join(r"Microsoft\Windows\Start Menu\Programs"));
    if let Some(d) = dirs::desktop_dir() {
        push(d);
    }
    let public = std::env::var("PUBLIC").unwrap_or_else(|_| r"C:\Users\Public".into());
    push(std::path::PathBuf::from(public).join("Desktop"));
    out
}

/// Walk `dir` (max 3 levels) for a .lnk that launches `exe_name`.
/// Returns the path of the .lnk itself.
///
/// A shortcut matches when either
/// - its target file name equals `exe_name` and the target still exists (a stale
///   shortcut left behind by an update is skipped), and the target's ProductName,
///   if present, agrees with `product_name`; or
/// - its target is a launcher stub (Squirrel `Update.exe --processStart X.exe`)
///   whose arguments name `exe_name`.
fn scan_lnk_dir(dir: &str, exe_name: &str, product_name: &str, depth: u32) -> Option<String> {
    if depth > 3 {
        return None;
    }
    let mut subdirs = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
            continue;
        }
        let is_lnk = path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("lnk"))
            .unwrap_or(false);
        if !is_lnk {
            continue;
        }
        let Some(lnk) = path.to_str() else { continue };
        let Some((target, args)) = read_shortcut(lnk) else { continue };
        if lnk_matches(&target, &args, exe_name, product_name) {
            return Some(lnk.to_string());
        }
    }
    // Files in this directory take precedence over subdirectories.
    for sub in subdirs {
        let Some(sub) = sub.to_str() else { continue };
        if let Some(found) = scan_lnk_dir(sub, exe_name, product_name, depth + 1) {
            return Some(found);
        }
    }
    None
}

fn lnk_matches(target: &str, args: &str, exe_name: &str, product_name: &str) -> bool {
    let target_path = std::path::Path::new(target);
    if !target_path.exists() {
        return false;
    }
    let target_file = target_path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    if target_file == exe_name {
        let actual = crate::discovery::read_product_name(target)
            .unwrap_or_else(|| crate::discovery::exe_stem(target));
        return crate::discovery::product_name_matches(&actual, product_name);
    }

    // Launcher stub: the real EXE is named in the arguments.
    args.to_lowercase()
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '/'))
        .any(|tok| tok == exe_name)
}

pub(crate) struct ShortcutInfo {
    pub target: String,
    pub args: String,
    pub icon_path: Option<String>,
}

/// Read a .lnk's target path and arguments via IShellLink.
pub(crate) fn read_shortcut(lnk_path: &str) -> Option<(String, String)> {
    read_shortcut_info(lnk_path).map(|s| (s.target, s.args))
}

/// Read a .lnk's target path, arguments, and optional custom icon file via IShellLink.
pub(crate) fn read_shortcut_info(lnk_path: &str) -> Option<ShortcutInfo> {
    use windows::Win32::{
        Storage::FileSystem::WIN32_FIND_DATAW,
        System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ},
        UI::Shell::{IShellLinkW, ShellLink},
    };
    use windows::core::Interface;

    fn wide_to_string(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    let _com = crate::com::ComGuard::init_sta();
    unsafe {

        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let persist: IPersistFile = link.cast().ok()?;
        let wide: Vec<u16> = lnk_path.encode_utf16().chain([0]).collect();
        persist.Load(PCWSTR(wide.as_ptr()), STGM_READ).ok()?;

        let mut target = vec![0u16; 1024];
        let mut find_data = WIN32_FIND_DATAW::default();
        link.GetPath(target.as_mut_slice(), &mut find_data as *mut _, 0u32).ok()?;
        let target = wide_to_string(&target);
        if target.is_empty() {
            // Advertised (MSI) shortcuts have no plain target path.
            return None;
        }

        let mut args = vec![0u16; 2048];
        let args = if link.GetArguments(args.as_mut_slice()).is_ok() {
            wide_to_string(&args)
        } else {
            String::new()
        };

        let mut icon_buf = vec![0u16; 1024];
        let mut icon_index: i32 = 0;
        let icon_path = if link
            .GetIconLocation(icon_buf.as_mut_slice(), &mut icon_index)
            .is_ok()
            && icon_index == 0
        {
            let p = wide_to_string(&icon_buf);
            let path = std::path::Path::new(&p);
            let ext_ok = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("ico") || e.eq_ignore_ascii_case("exe"));
            if !p.is_empty() && ext_ok && path.is_file() {
                Some(p)
            } else {
                None
            }
        } else {
            None
        };

        Some(ShortcutInfo {
            target,
            args,
            icon_path,
        })
    }
}

/// Actually launch an application given a resolved `LaunchEntry`.
pub fn launch(entry: &LaunchEntry) -> anyhow::Result<()> {
    match entry {
        LaunchEntry::Aumid(aumid) => launch_aumid(aumid),
        LaunchEntry::Shortcut(path) | LaunchEntry::ExePath(path) => launch_exe(path),
    }
}

fn launch_aumid(aumid: &str) -> anyhow::Result<()> {
    match activate_aumid(aumid) {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::warn!("IApplicationActivationManager 激活 {aumid} 失败，回退 ShellExecute: {e}");
            launch_aumid_shell(aumid)
        }
    }
}

fn activate_aumid(aumid: &str) -> anyhow::Result<()> {
    use windows::Win32::{
        System::Com::{CoCreateInstance, CLSCTX_LOCAL_SERVER},
        UI::Shell::{ApplicationActivationManager, IApplicationActivationManager, AO_NONE},
    };

    let _com = crate::com::ComGuard::init_sta();
    let wide_aumid: Vec<u16> = aumid.encode_utf16().chain([0]).collect();
    unsafe {
        let mgr: IApplicationActivationManager =
            CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER)?;
        mgr.ActivateApplication(PCWSTR(wide_aumid.as_ptr()), PCWSTR::null(), AO_NONE)?;
    }
    Ok(())
}

fn launch_aumid_shell(aumid: &str) -> anyhow::Result<()> {
    let target = format!("shell:AppsFolder\\{aumid}");
    let wide_target: Vec<u16> = target.encode_utf16().chain([0]).collect();
    let wide_open: Vec<u16> = "open\0".encode_utf16().collect();

    unsafe {
        let result = ShellExecuteW(
            None,
            PCWSTR(wide_open.as_ptr()),
            PCWSTR(wide_target.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
        if result.0 as usize > 32 {
            Ok(())
        } else {
            anyhow::bail!("ShellExecuteW failed for AUMID {aumid}: result={}", result.0 as usize)
        }
    }
}

fn launch_exe(path: &str) -> anyhow::Result<()> {
    let wide_path: Vec<u16> = path.encode_utf16().chain([0]).collect();
    let wide_open: Vec<u16> = "open\0".encode_utf16().collect();

    unsafe {
        let result = ShellExecuteW(
            None,
            PCWSTR(wide_open.as_ptr()),
            PCWSTR(wide_path.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
        if result.0 as usize > 32 {
            Ok(())
        } else {
            anyhow::bail!("ShellExecuteW failed for path {path}: result={}", result.0 as usize)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_manager_available() {
        use windows::Win32::{
            System::Com::{CoCreateInstance, CLSCTX_LOCAL_SERVER},
            UI::Shell::{ApplicationActivationManager, IApplicationActivationManager},
        };
        let _com = crate::com::ComGuard::init_sta();
        unsafe {
            let mgr: windows::core::Result<IApplicationActivationManager> =
                CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER);
            assert!(mgr.is_ok(), "CoCreateInstance failed: {mgr:?}");
        }
    }

    #[test]
    fn unregistered_aumid_is_rejected() {
        assert!(!is_aumid_registered(""));
        assert!(!is_aumid_registered("NoBang"));
        assert!(!is_aumid_registered("NonExistent.FakeApp_0000000000000!App"));
        assert!(resolve_launch_entry(
            &AppIdentity::Msix {
                aumid: "NonExistent.FakeApp_0000000000000!App".to_string(),
            },
            None,
        )
        .is_none());
    }
}
