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

/// Captured launch parameters and working directory from a live Win32 process,
/// applied when restarting directly via `LaunchEntry::ExePath`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchContext {
    pub args: Option<String>,
    pub working_dir: Option<String>,
    pub source_exe: Option<String>,
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
            if std::path::Path::new(path).is_file() {
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
    // 1. App Paths registry (HKCU, then HKLM), verified against Win32 identity
    // 2. Start menu shortcuts (per-user, then all-users)
    // 3. Desktop shortcuts (per-user, then public)
    // 4. Running / last-known EXE path, verified against Win32 identity
    if let Some(path) = try_app_paths_registry(exe_name, product_name) {
        return Some(LaunchEntry::ExePath(path));
    }
    for dir in shortcut_dirs() {
        if let Some(lnk) = scan_lnk_dir(&dir, exe_name, product_name, 0) {
            return Some(LaunchEntry::Shortcut(lnk));
        }
    }
    if let Some(hint) = path_hint {
        if crate::discovery::exe_matches_win32_identity(hint, exe_name, product_name) {
            return Some(LaunchEntry::ExePath(hint.to_string()));
        }
    }
    None
}

/// Check HKCU then HKLM `SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{exe_name}`,
/// verifying that the target EXE matches `exe_name` and `product_name`.
fn try_app_paths_registry(exe_name: &str, product_name: &str) -> Option<String> {
    let key_path = format!(
        "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{}",
        exe_name
    );
    use windows::Win32::System::Registry::{
        RegOpenKeyExW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ,
    };

    let wide_key: Vec<u16> = key_path.encode_utf16().chain([0]).collect();
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        let mut hkey = windows::Win32::System::Registry::HKEY::default();
        if unsafe { RegOpenKeyExW(root, PCWSTR(wide_key.as_ptr()), 0, KEY_READ, &mut hkey) }.is_err()
        {
            continue;
        }
        let Some(raw) = query_registry_default_string(hkey) else {
            continue;
        };
        let Some(path) = parse_app_paths_value(&raw) else {
            continue;
        };
        if crate::discovery::exe_matches_win32_identity(&path, exe_name, product_name) {
            return Some(path);
        }
    }
    None
}

fn query_registry_default_string(hkey: windows::Win32::System::Registry::HKEY) -> Option<String> {
    use windows::Win32::{
        Foundation::ERROR_MORE_DATA,
        System::Registry::{RegCloseKey, RegQueryValueExW, REG_EXPAND_SZ, REG_SZ, REG_VALUE_TYPE},
    };

    let wide_name: Vec<u16> = vec![0u16]; // empty -> default value
    let mut data = vec![0u16; 1024];
    let mut data_size = (data.len() * 2) as u32;
    let mut vtype = REG_VALUE_TYPE::default();

    let mut result = unsafe {
        RegQueryValueExW(
            hkey,
            PCWSTR(wide_name.as_ptr()),
            None,
            Some(&mut vtype),
            Some(data.as_mut_ptr() as _),
            Some(&mut data_size),
        )
    };
    if result == ERROR_MORE_DATA && data_size > 0 && data_size <= 131_072 {
        data.resize((data_size as usize).div_ceil(2), 0);
        data_size = (data.len() * 2) as u32;
        result = unsafe {
            RegQueryValueExW(
                hkey,
                PCWSTR(wide_name.as_ptr()),
                None,
                Some(&mut vtype),
                Some(data.as_mut_ptr() as _),
                Some(&mut data_size),
            )
        };
    }
    unsafe {
        let _ = RegCloseKey(hkey);
    }

    if result.is_err() || (vtype != REG_SZ && vtype != REG_EXPAND_SZ) {
        return None;
    }

    let max_u16 = ((data_size as usize) / 2).min(data.len());
    let slice = &data[..max_u16];
    let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    let text = String::from_utf16_lossy(&slice[..len]);
    if vtype == REG_EXPAND_SZ {
        expand_env_strings(&text)
    } else {
        Some(text)
    }
}

fn expand_env_strings(input: &str) -> Option<String> {
    use windows::Win32::System::Environment::ExpandEnvironmentStringsW;

    let wide: Vec<u16> = input.encode_utf16().chain([0]).collect();
    let needed = unsafe { ExpandEnvironmentStringsW(PCWSTR(wide.as_ptr()), None) };
    if needed == 0 || needed > 65536 {
        return None;
    }
    let mut buf = vec![0u16; needed as usize];
    let written = unsafe { ExpandEnvironmentStringsW(PCWSTR(wide.as_ptr()), Some(&mut buf)) };
    if written == 0 || written > needed {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

pub(crate) fn parse_app_paths_value(raw: &str) -> Option<String> {
    let cleaned = raw.trim().trim_matches('"').trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
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
    if !target_path.is_file() {
        return false;
    }
    let target_file = target_path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    if target_file == exe_name {
        return crate::discovery::exe_matches_win32_identity(target, exe_name, product_name);
    }

    if target_file == "update.exe" {
        let Some(proc_exe) = crate::installed::parse_process_start(args) else {
            return false;
        };
        if !proc_exe.eq_ignore_ascii_case(exe_name) {
            return false;
        }
        let Some(update_dir) = target_path.parent() else {
            return false;
        };
        let Some(squirrel_exe) = crate::installed::resolve_squirrel_exe(update_dir, &proc_exe)
        else {
            return false;
        };
        return crate::discovery::exe_matches_win32_identity(
            &squirrel_exe,
            exe_name,
            product_name,
        );
    }

    // Other launcher stub: require `exe_name` in arguments and verify product identity.
    let has_exe_token = args
        .to_lowercase()
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '/'))
        .any(|tok| tok == exe_name);
    if !has_exe_token {
        return false;
    }
    if target
        .to_lowercase()
        .starts_with(&crate::discovery::windows_dir())
    {
        return false;
    }

    let arg_file_match = split_arg_tokens(args).into_iter().any(|tok| {
        std::path::Path::new(&tok)
            .file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(exe_name))
            && crate::discovery::exe_matches_win32_identity(&tok, exe_name, product_name)
    });
    if arg_file_match {
        return true;
    }
    if let Some(sibling) = target_path.parent().map(|d| d.join(exe_name)) {
        if let Some(s) = sibling.to_str() {
            if crate::discovery::exe_matches_win32_identity(s, exe_name, product_name) {
                return true;
            }
        }
    }
    crate::discovery::read_product_name(target)
        .is_some_and(|actual| crate::discovery::product_name_matches(&actual, product_name))
}

fn split_arg_tokens(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = args.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        if c == '"' {
            chars.next();
            let mut tok = String::new();
            for ch in chars.by_ref() {
                if ch == '"' {
                    break;
                }
                tok.push(ch);
            }
            if !tok.is_empty() {
                out.push(tok);
            }
        } else {
            let mut tok = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() {
                    break;
                }
                tok.push(ch);
                chars.next();
            }
            if !tok.is_empty() {
                out.push(tok);
            }
        }
    }
    out
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

/// Resolve the working directory to pass when launching `exe_path` directly.
///
/// Preserves the captured working directory unless it pointed at the old
/// executable's own directory and the launch target moved (e.g. an update).
pub(crate) fn resolve_launch_working_dir(
    exe_path: &str,
    ctx: Option<&LaunchContext>,
) -> Option<String> {
    fn norm_dir(p: &std::path::Path) -> String {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_lowercase()
    }

    let exe_dir = std::path::Path::new(exe_path)
        .parent()
        .filter(|p| p.is_dir());

    if let Some(cwd) = ctx.and_then(|c| c.working_dir.as_deref()) {
        let cwd_path = std::path::Path::new(cwd);
        if cwd_path.is_dir() {
            if let (Some(src_exe), Some(target_dir)) =
                (ctx.and_then(|c| c.source_exe.as_deref()), exe_dir)
            {
                if let Some(src_dir) = std::path::Path::new(src_exe).parent() {
                    if norm_dir(cwd_path) == norm_dir(src_dir)
                        && norm_dir(target_dir) != norm_dir(src_dir)
                    {
                        return Some(target_dir.to_string_lossy().into_owned());
                    }
                }
            }
            return Some(cwd.to_string());
        }
    }

    exe_dir.map(|p| p.to_string_lossy().into_owned())
}

/// Actually launch an application given a resolved `LaunchEntry`.
pub fn launch(entry: &LaunchEntry) -> anyhow::Result<()> {
    launch_with_context(entry, None)
}

/// Launch an application given a resolved `LaunchEntry`, applying `ctx`
/// (original command-line arguments and working directory) for direct EXEs.
pub fn launch_with_context(
    entry: &LaunchEntry,
    ctx: Option<&LaunchContext>,
) -> anyhow::Result<()> {
    match entry {
        LaunchEntry::Aumid(aumid) => launch_aumid(aumid),
        LaunchEntry::Shortcut(path) => launch_shell_item(path, None, None),
        LaunchEntry::ExePath(path) => {
            let args = ctx
                .and_then(|c| c.args.as_deref())
                .map(str::trim)
                .filter(|a| !a.is_empty());
            let working_dir = resolve_launch_working_dir(path, ctx);
            launch_shell_item(path, args, working_dir.as_deref())
        }
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

fn launch_shell_item(
    path: &str,
    args: Option<&str>,
    working_dir: Option<&str>,
) -> anyhow::Result<()> {
    let wide_path: Vec<u16> = path.encode_utf16().chain([0]).collect();
    let wide_open: Vec<u16> = "open\0".encode_utf16().collect();
    let wide_args: Option<Vec<u16>> = args
        .filter(|a| !a.is_empty())
        .map(|a| a.encode_utf16().chain([0]).collect());
    let wide_dir: Option<Vec<u16>> = working_dir
        .filter(|d| !d.is_empty())
        .map(|d| d.encode_utf16().chain([0]).collect());

    unsafe {
        let result = ShellExecuteW(
            None,
            PCWSTR(wide_open.as_ptr()),
            PCWSTR(wide_path.as_ptr()),
            wide_args
                .as_ref()
                .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr())),
            wide_dir
                .as_ref()
                .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr())),
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

    #[test]
    fn resolve_win32_entry_verifies_product_identity_on_path_hint() {
        let exe = std::env::current_exe().expect("current_exe");
        let exe_str = exe.to_string_lossy().into_owned();
        let exe_name = exe.file_name().unwrap().to_string_lossy().to_lowercase();
        let product = crate::discovery::read_product_name(&exe_str)
            .unwrap_or_else(|| crate::discovery::exe_stem(&exe_str));

        // Same exe_name but mismatched product_name must be rejected.
        assert!(resolve_win32_entry(
            &exe_name,
            "Definitely Mismatched Product",
            Some(&exe_str)
        )
        .is_none());

        // Matching exe_name and product_name resolves to ExePath.
        let entry = resolve_win32_entry(&exe_name, &product, Some(&exe_str));
        assert!(matches!(entry, Some(LaunchEntry::ExePath(_))));
    }

    #[test]
    fn parse_app_paths_value_cases() {
        assert_eq!(
            parse_app_paths_value(r#"  "C:\Program Files\App\app.exe"  "#),
            Some(r"C:\Program Files\App\app.exe".to_string())
        );
        assert_eq!(
            parse_app_paths_value(r"C:\Tools\tool.exe"),
            Some(r"C:\Tools\tool.exe".to_string())
        );
        assert_eq!(parse_app_paths_value("  \"\"  "), None);
    }

    #[test]
    fn resolve_launch_working_dir_cases() {
        let temp_dir = std::env::temp_dir();
        let exe = std::env::current_exe().expect("current_exe");
        let exe_str = exe.to_string_lossy().into_owned();
        let exe_dir = exe.parent().unwrap().to_string_lossy().into_owned();
        let sys_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let sys32 = format!(r"{sys_root}\System32");

        // Preserves custom user working directory.
        let ctx_user_cwd = LaunchContext {
            args: Some("--flag".to_string()),
            working_dir: Some(temp_dir.to_string_lossy().into_owned()),
            source_exe: Some(exe_str.clone()),
        };
        assert_eq!(
            resolve_launch_working_dir(&exe_str, Some(&ctx_user_cwd)),
            Some(temp_dir.to_string_lossy().into_owned())
        );

        // If old CWD equaled old install dir and target EXE moved, use new EXE's dir.
        let ctx_old_install_cwd = LaunchContext {
            args: None,
            working_dir: Some(sys32.clone()),
            source_exe: Some(format!(r"{sys32}\old_app.exe")),
        };
        assert_eq!(
            resolve_launch_working_dir(&exe_str, Some(&ctx_old_install_cwd)),
            Some(exe_dir.clone())
        );

        // Missing/invalid CWD falls back to target EXE's parent directory.
        let ctx_bad_cwd = LaunchContext {
            args: None,
            working_dir: Some(r"C:\NonExistent\Dir_12345".to_string()),
            source_exe: None,
        };
        assert_eq!(
            resolve_launch_working_dir(&exe_str, Some(&ctx_bad_cwd)),
            Some(exe_dir)
        );
    }
}
