/// Enumerate installed desktop applications from Start menu shortcuts and
/// the MSIX app list. Used to populate the "add application" picker so the
/// user can favorite apps that are not currently running.

use std::collections::{HashMap, HashSet};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use serde::Deserialize;
use crate::identity::AppIdentity;

/// A candidate app the user might want to add to favorites.
#[derive(Debug, Clone)]
pub struct InstalledApp {
    pub display_name: String,
    pub identity: AppIdentity,
    /// EXE path, resolved at discovery time, used for icon extraction.
    pub exe_path: Option<String>,
    /// Custom icon path declared by a `.lnk` shortcut, when different from `exe_path`.
    pub icon_path: Option<String>,
}

/// Enumerate all installed apps: `Get-StartApps` + Start menu shortcuts.
/// `progress` is called each time a batch of new apps is found, with the
/// current running total. Blocking, runs ~0.5–2 s. Call on a worker thread.
pub fn enumerate_installed(progress: impl Fn(usize)) -> Vec<InstalledApp> {
    let mut apps = start_apps();
    progress(apps.len());

    let base_count = apps.len();
    let lnk_apps = start_menu_apps_with_progress(|n| progress(base_count + n));
    apps.extend(lnk_apps);

    // Sort by display name, with MSIX < Win32 < Manual within the same name.
    apps.sort_by(|a, b| {
        let name_ord = a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase());
        if name_ord != std::cmp::Ordering::Equal {
            return name_ord;
        }
        type_rank(&a.identity).cmp(&type_rank(&b.identity))
    });

    // MSIX entries take priority over non-MSIX entries with the same display name;
    // keep distinct identities separate (e.g. Office apps sharing a ProductName).
    let msix_names: HashSet<String> = apps
        .iter()
        .filter(|a| a.identity.is_msix())
        .map(|a| a.display_name.to_lowercase())
        .collect();
    let mut deduped: Vec<InstalledApp> = Vec::with_capacity(apps.len());
    let mut index_by_id: HashMap<AppIdentity, usize> = HashMap::new();
    for app in apps {
        if !app.identity.is_msix() && msix_names.contains(&app.display_name.to_lowercase()) {
            continue;
        }
        if let Some(&idx) = index_by_id.get(&app.identity) {
            if deduped[idx].icon_path.is_none() && app.icon_path.is_some() {
                deduped[idx].icon_path = app.icon_path;
            }
            continue;
        }
        index_by_id.insert(app.identity.clone(), deduped.len());
        deduped.push(app);
    }

    deduped
}

fn type_rank(id: &AppIdentity) -> u8 {
    match id {
        AppIdentity::Msix { .. } => 0,
        AppIdentity::Win32 { .. } => 1,
        AppIdentity::Manual { .. } => 2,
    }
}

// ---------------------------------------------------------------------------
// Get-StartApps (MSIX + registered Win32 apps)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StartApp {
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "AppID")]
    app_id: Option<String>,
}

fn start_apps() -> Vec<InstalledApp> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let output = std::process::Command::new("powershell.exe")
        .creation_flags(CREATE_NO_WINDOW)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; ConvertTo-Json -InputObject @(Get-StartApps) -Compress",
        ])
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    parse_start_apps_json(&text)
        .into_iter()
        .filter_map(|(name, app_id)| classify_start_app(&name, &app_id))
        .collect()
}

fn parse_start_apps_json(json: &str) -> Vec<(String, String)> {
    let json = json.trim();
    if json.is_empty() {
        return Vec::new();
    }

    let items: Vec<StartApp> = serde_json::from_str(json)
        .or_else(|_| serde_json::from_str::<StartApp>(json).map(|one| vec![one]))
        .unwrap_or_default();

    items
        .into_iter()
        .filter_map(|item| {
            let name = item.name?;
            let app_id = item.app_id?;
            if name.is_empty() || app_id.is_empty() {
                None
            } else {
                Some((name, app_id))
            }
        })
        .collect()
}

fn classify_start_app(name: &str, app_id: &str) -> Option<InstalledApp> {
    if app_id.contains('!') {
        return Some(InstalledApp {
            display_name: name.to_string(),
            identity: AppIdentity::Msix {
                aumid: app_id.to_string(),
            },
            exe_path: None,
            icon_path: None,
        });
    }

    let path = resolve_known_folder_path(app_id)?;
    if !is_exe_path(&path) || !Path::new(&path).is_file() {
        return None;
    }
    if path.to_lowercase().starts_with(&crate::discovery::windows_dir()) {
        return None;
    }
    if is_uninstaller(name, &path) || is_browser_proxy(&path) {
        return None;
    }

    Some(InstalledApp {
        display_name: name.to_string(),
        identity: crate::discovery::win32_identity(&path),
        exe_path: Some(path),
        icon_path: None,
    })
}

fn resolve_known_folder_path(app_id: &str) -> Option<String> {
    if !app_id.starts_with('{') {
        return if app_id.contains('\\') || app_id.contains('/') {
            Some(app_id.to_string())
        } else {
            None
        };
    }

    let end = app_id.find('}')?;
    let guid = app_id[1..end].to_ascii_uppercase();
    let rest = app_id[end + 1..].trim_start_matches(['\\', '/']);

    let sys_root = || {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
    };
    let base: PathBuf = match guid.as_str() {
        "6D809377-6AF0-444B-8957-A3773F02200E" => std::env::var_os("ProgramW6432")
            .or_else(|| std::env::var_os("ProgramFiles"))
            .map(PathBuf::from)?,
        "7C5A40EF-A0FB-4BFC-874A-C0F2E0B9FA8E" => {
            std::env::var_os("ProgramFiles(x86)").map(PathBuf::from)?
        }
        "905E63B6-C1BF-494E-B29C-65B732D3D21A" => {
            std::env::var_os("ProgramFiles").map(PathBuf::from)?
        }
        "F38BF404-1D43-42F2-9305-67DE0B28FC23" => sys_root(),
        "1AC14E77-02E7-4E5D-B744-2EB1AE5198B7" => sys_root().join("System32"),
        "D65231B0-B2F1-4857-A4CE-A8E7C6EA7D27" => sys_root().join("SysWOW64"),
        "F1B32785-6FBA-4FCF-9D55-7B8E7F157091" => dirs::data_local_dir()?,
        "3EB685DB-65F9-4CF6-A03A-E3EF65729F3D" => dirs::data_dir()?,
        "5E6C858F-0E22-4760-9AFE-EA3317B67173" => dirs::home_dir()?,
        "62AB5D82-FDC1-4DC3-A9DD-070D1D495D97" => {
            std::env::var_os("ProgramData").map(PathBuf::from)?
        }
        _ => return None,
    };

    let full = if rest.is_empty() {
        base
    } else {
        base.join(rest)
    };
    Some(full.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// Start menu .lnk enumeration
// ---------------------------------------------------------------------------

fn start_menu_apps_with_progress(progress: impl Fn(usize)) -> Vec<InstalledApp> {
    let mut dirs = Vec::new();
    if let Some(d) = dirs::data_dir() {
        let p = d.join(r"Microsoft\Windows\Start Menu\Programs");
        if p.is_dir() {
            dirs.push(p);
        }
    }
    let program_data = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
    let p = PathBuf::from(program_data).join(r"Microsoft\Windows\Start Menu\Programs");
    if p.is_dir() {
        dirs.push(p);
    }

    let mut apps = Vec::new();
    for dir in dirs {
        collect_lnks_progress(&dir, &mut apps, 0, &progress);
    }
    apps
}

fn collect_lnks_progress(
    dir: &Path,
    apps: &mut Vec<InstalledApp>,
    depth: u32,
    progress: &impl Fn(usize),
) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_lnks_progress(&path, apps, depth + 1, progress);
        } else if path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("lnk"))
            .unwrap_or(false)
        {
            if let Some(app) = lnk_to_installed(&path) {
                apps.push(app);
                progress(apps.len());
            }
        }
    }
}

fn lnk_to_installed(lnk_path: &Path) -> Option<InstalledApp> {
    let lnk_str = lnk_path.to_str()?;
    let stem = lnk_path.file_stem()?.to_string_lossy().to_string();
    let info = crate::launcher::read_shortcut_info(lnk_str)?;
    let target = info.target;
    let args = info.args;

    let target_path = Path::new(&target);
    if !target_path.exists() {
        return None;
    }
    let sys = crate::discovery::windows_dir();
    if target.to_lowercase().starts_with(&sys) {
        return None;
    }

    let is_squirrel_update = target_path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("update.exe"));
    let exe = if is_squirrel_update {
        let proc_exe = parse_process_start(&args)?;
        let update_dir = target_path.parent()?;
        resolve_squirrel_exe(update_dir, &proc_exe)?
    } else {
        target
    };

    if !is_exe_path(&exe) || !Path::new(&exe).is_file() {
        return None;
    }
    if exe.to_lowercase().starts_with(&sys) {
        return None;
    }
    if is_uninstaller(&stem, &exe) || is_browser_proxy(&exe) {
        return None;
    }

    let (display_name, identity, exe_path, icon_path) =
        if let Some(aumid) = crate::discovery::aumid_from_windowsapps_exe(&exe) {
            (stem, AppIdentity::Msix { aumid }, None, None)
        } else {
            let icon_override = info.icon_path.filter(|p| !p.eq_ignore_ascii_case(&exe));
            (
                stem,
                crate::discovery::win32_identity(&exe),
                Some(exe),
                icon_override,
            )
        };

    Some(InstalledApp {
        display_name,
        identity,
        exe_path,
        icon_path,
    })
}

pub(crate) fn resolve_squirrel_exe(update_dir: &Path, proc_exe: &str) -> Option<String> {
    let mut app_dirs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(update_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join(proc_exe).is_file() {
                if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) {
                    app_dirs.push(name);
                }
            }
        }
    }
    if let Some(best) = pick_latest_app_dir(&app_dirs) {
        return Some(update_dir.join(best).join(proc_exe).to_string_lossy().into_owned());
    }
    let fallback = update_dir.join(proc_exe);
    if fallback.is_file() {
        Some(fallback.to_string_lossy().into_owned())
    } else {
        None
    }
}

pub(crate) fn parse_process_start(args: &str) -> Option<String> {
    let mut chars = args.chars().peekable();
    let mut expect_value = false;
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let token = if c == '"' {
            chars.next();
            let mut tok = String::new();
            for ch in chars.by_ref() {
                if ch == '"' {
                    break;
                }
                tok.push(ch);
            }
            tok
        } else {
            let mut tok = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_whitespace() {
                    break;
                }
                tok.push(ch);
                chars.next();
            }
            tok
        };
        if expect_value {
            return if token.is_empty() { None } else { Some(token) };
        }
        if token.eq_ignore_ascii_case("--processStart") {
            expect_value = true;
        }
    }
    None
}

fn pick_latest_app_dir(names: &[String]) -> Option<String> {
    names
        .iter()
        .filter_map(|name| {
            let rest = name
                .strip_prefix("app-")
                .or_else(|| name.strip_prefix("APP-"))?;
            if rest.is_empty() {
                return None;
            }
            let ver: Option<Vec<u32>> = rest.split('.').map(|s| s.parse::<u32>().ok()).collect();
            Some((ver?, name))
        })
        .max_by(|(va, _), (vb, _)| va.cmp(vb))
        .map(|(_, name)| name.clone())
}

fn is_exe_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

fn is_browser_proxy(exe_path: &str) -> bool {
    let file = Path::new(exe_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        file.as_str(),
        "chrome_proxy.exe" | "msedge_proxy.exe" | "brave_proxy.exe"
    )
}

fn is_uninstaller(display_name: &str, exe_path: &str) -> bool {
    let exe_file = Path::new(exe_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let name_lower = display_name.to_lowercase();
    exe_file.starts_with("unins")
        || exe_file.contains("uninstall")
        || name_lower.contains("uninstall")
        || display_name.contains("卸载")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_start_apps_array_and_single_and_escapes() {
        let arr = r#"[{"Name":"A\u0026B","AppID":"C:\\X\\a.exe"},{"Name":"","AppID":"x"}]"#;
        let parsed = parse_start_apps_json(arr);
        assert_eq!(parsed, vec![("A&B".to_string(), r"C:\X\a.exe".to_string())]);

        let single = r#"{"Name":"Solo","AppID":"Pub.Solo_123!App"}"#;
        assert_eq!(
            parse_start_apps_json(single),
            vec![("Solo".to_string(), "Pub.Solo_123!App".to_string())]
        );
    }

    #[test]
    fn classify_msix_and_skip_autogenerated() {
        let msix = classify_start_app("Foo", "Pub.Foo_abc!App").expect("should classify MSIX");
        assert!(matches!(
            msix.identity,
            AppIdentity::Msix { ref aumid } if aumid == "Pub.Foo_abc!App"
        ));
        assert!(msix.exe_path.is_none());

        assert!(classify_start_app(
            "Auto",
            "Microsoft.AutoGenerated.{023BD127-5ED5-F28E-F86C-0B454ADD5CEA}"
        )
        .is_none());
    }

    #[test]
    fn resolve_known_folder_windows_and_unknown() {
        let resolved = resolve_known_folder_path(
            r"{F38BF404-1D43-42F2-9305-67DE0B28FC23}\notepad.exe",
        )
        .expect("known Windows GUID should resolve");
        let sys_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let expected = format!(r"{}\notepad.exe", sys_root.trim_end_matches('\\'));
        assert!(resolved.eq_ignore_ascii_case(&expected), "{resolved} != {expected}");

        assert!(resolve_known_folder_path(
            r"{00000000-0000-0000-0000-000000000000}\foo.exe"
        )
        .is_none());
    }

    #[test]
    fn parse_process_start_cases() {
        assert_eq!(
            parse_process_start(r#"--processStart "Discord.exe""#),
            Some("Discord.exe".to_string())
        );
        assert_eq!(
            parse_process_start("--processStart Slack.exe --process-start-args x"),
            Some("Slack.exe".to_string())
        );
        assert_eq!(parse_process_start(""), None);
    }

    #[test]
    fn pick_latest_app_dir_semver_numeric() {
        let dirs = vec![
            "app-1.0.9".to_string(),
            "app-1.0.10".to_string(),
            "packages".to_string(),
        ];
        assert_eq!(pick_latest_app_dir(&dirs), Some("app-1.0.10".to_string()));
    }

    #[test]
    fn browser_proxy_filtered() {
        assert!(is_browser_proxy(
            r"C:\Program Files\Google\Chrome\Application\chrome_proxy.exe"
        ));
        assert!(is_browser_proxy(
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge_proxy.exe"
        ));
        assert!(!is_browser_proxy(
            r"C:\Program Files\Google\Chrome\Application\chrome.exe"
        ));
        assert!(!is_browser_proxy(r"E:\Clash Verge\clash-verge.exe"));
    }
}
