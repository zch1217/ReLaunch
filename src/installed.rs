/// Enumerate installed desktop applications from Start menu shortcuts and
/// the MSIX app list. Used to populate the "add application" picker so the
/// user can favorite apps that are not currently running.

use crate::identity::AppIdentity;

/// A candidate app the user might want to add to favorites.
#[derive(Debug, Clone)]
pub struct InstalledApp {
    pub display_name: String,
    pub identity: AppIdentity,
    /// EXE path, resolved at discovery time, used for icon extraction.
    pub exe_path: Option<String>,
}

/// Enumerate all installed apps: MSIX packages + Start menu shortcuts.
/// `progress` is called each time a batch of new apps is found, with the
/// current running total. Blocking, runs ~0.5–2 s. Call on a worker thread.
pub fn enumerate_installed(progress: impl Fn(usize)) -> Vec<InstalledApp> {
    let mut apps = Vec::new();

    // 1. MSIX / AppX packages via Get-StartApps.
    apps.extend(msix_apps());
    progress(apps.len());

    // 2. Start menu .lnk files.
    let lnk_apps = start_menu_apps_with_progress(|n| progress(apps.len() + n));
    apps.extend(lnk_apps);

    // Deduplicate by display name. MSIX takes priority over Win32/Manual — the
    // .lnk in the Start menu and the MSIX AUMID for the same app have the same
    // name; we want the AUMID so we keep it.
    // Sort MSIX first, then Win32, then Manual within the same display name.
    apps.sort_by(|a, b| {
        let name_ord = a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase());
        if name_ord != std::cmp::Ordering::Equal {
            return name_ord;
        }
        // Same name: MSIX < Win32 < Manual so MSIX survives dedup.
        type_rank(&a.identity).cmp(&type_rank(&b.identity))
    });
    apps.dedup_by(|a, b| a.display_name.to_lowercase() == b.display_name.to_lowercase());
    apps
}

fn type_rank(id: &AppIdentity) -> u8 {
    match id {
        AppIdentity::Msix { .. } => 0,
        AppIdentity::Win32 { .. } => 1,
        AppIdentity::Manual { .. } => 2,
    }
}

// ---------------------------------------------------------------------------
// MSIX apps
// ---------------------------------------------------------------------------

fn msix_apps() -> Vec<InstalledApp> {
    // Use Get-StartApps via PowerShell — most reliable source of MSIX AUMIDs.
    msix_from_registry()
}

/// Read registered MSIX AUMIDs from
/// HKCU\Software\Microsoft\Windows\CurrentVersion\Search\Flighting\0\SearchBoxDisabled
/// – too fragile. Use PackageManager AppInfo instead.
/// Simpler: just read Get-StartApps output by shelling out once.
fn msix_from_registry() -> Vec<InstalledApp> {
    // Use PowerShell's Get-StartApps: fast and reliable.
    // Force UTF-8 output so non-ASCII app names survive the encoding round-trip.
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; Get-StartApps | ConvertTo-Json -Compress",
        ])
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }

    // Parse JSON array of {Name, AppID}.
    parse_start_apps_json(text)
}

fn parse_start_apps_json(json: &str) -> Vec<InstalledApp> {
    // Lightweight JSON parser — avoids pulling in serde_json for a simple array.
    // Expected shape: [{"Name":"...","AppID":"..."},...]  or single object.
    let json = json.trim();

    // Normalise to array.
    let items_text = if json.starts_with('[') {
        json[1..json.len() - 1].trim()
    } else {
        json
    };

    let mut result = Vec::new();
    // Split objects by "},{" and parse each.
    for obj in split_json_objects(items_text) {
        if let (Some(name), Some(appid)) = (extract_json_str(obj, "Name"), extract_json_str(obj, "AppID")) {
            if name.is_empty() || appid.is_empty() {
                continue;
            }
            // AppID containing '!' is an AUMID (e.g. "Claude_pzs8sxrjxfjjc!Claude").
            // Everything else that looks like an absolute path is a Win32 EXE.
            let identity = if appid.contains('!') {
                AppIdentity::Msix { aumid: appid.to_string() }
            } else if appid.contains('\\') || appid.contains('/') {
                // Absolute path — a Win32 EXE registered directly in Start Apps.
                AppIdentity::Manual { path: appid.to_string() }
            } else {
                // Unrecognised format — skip.
                continue;
            };
            result.push(InstalledApp {
                display_name: name.to_string(),
                identity,
                exe_path: None,
            });
        }
    }
    result
}

fn split_json_objects(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                if depth == 0 { start = i; }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    parts.push(&s[start..=i]);
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' { i += 1; }
                    else if bytes[i] == b'"' { break; }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    parts
}

fn extract_json_str<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{}\":", key);
    let idx = obj.find(&needle)?;
    let after = obj[idx + needle.len()..].trim_start();
    if !after.starts_with('"') {
        return None;
    }
    let content = &after[1..];
    let mut end = 0;
    let bytes = content.as_bytes();
    while end < bytes.len() {
        if bytes[end] == b'\\' { end += 1; }
        else if bytes[end] == b'"' { break; }
        end += 1;
    }
    Some(&content[..end])
}

// ---------------------------------------------------------------------------
// Start menu .lnk enumeration
// ---------------------------------------------------------------------------

fn start_menu_apps_with_progress(progress: impl Fn(usize)) -> Vec<InstalledApp> {
    let mut dirs = Vec::new();
    if let Some(d) = dirs::data_dir() {
        let p = d.join(r"Microsoft\Windows\Start Menu\Programs");
        if p.is_dir() { dirs.push(p); }
    }
    let program_data = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
    let p = std::path::PathBuf::from(program_data).join(r"Microsoft\Windows\Start Menu\Programs");
    if p.is_dir() { dirs.push(p); }

    let mut apps = Vec::new();
    for dir in dirs {
        collect_lnks_progress(&dir, &mut apps, 0, &progress);
    }
    apps
}

fn collect_lnks_progress(dir: &std::path::Path, apps: &mut Vec<InstalledApp>, depth: u32, progress: &impl Fn(usize)) {
    if depth > 3 { return; }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_lnks_progress(&path, apps, depth + 1, progress);
        } else if path.extension().map(|e| e.eq_ignore_ascii_case("lnk")).unwrap_or(false) {
            if let Some(app) = lnk_to_installed(&path) {
                apps.push(app);
                progress(apps.len());
            }
        }
    }
}

fn lnk_to_installed(lnk_path: &std::path::Path) -> Option<InstalledApp> {
    use windows::Win32::{
        Storage::FileSystem::WIN32_FIND_DATAW,
        System::Com::{
            CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER,
            COINIT_APARTMENTTHREADED, STGM_READ,
        },
        UI::Shell::{IShellLinkW, ShellLink},
    };
    use windows::core::{Interface, PCWSTR};

    let lnk_str = lnk_path.to_str()?;
    // Name from the .lnk file stem.
    let stem = lnk_path.file_stem()?.to_string_lossy().to_string();

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let persist: IPersistFile = link.cast().ok()?;
        let wide: Vec<u16> = lnk_str.encode_utf16().chain([0]).collect();
        persist.Load(PCWSTR(wide.as_ptr()), STGM_READ).ok()?;

        let mut buf = vec![0u16; 1024];
        let mut find_data = WIN32_FIND_DATAW::default();
        link.GetPath(buf.as_mut_slice(), &mut find_data as *mut _, 0u32).ok()?;

        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let target = String::from_utf16_lossy(&buf[..len]);
        if target.is_empty() || !std::path::Path::new(&target).exists() {
            return None;
        }

        // Skip system executables.
        let sys = {
            let dir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            format!(r"{}\", dir.trim_end_matches('\\').to_lowercase())
        };
        if target.to_lowercase().starts_with(&sys) {
            return None;
        }

        let exe_name = std::path::Path::new(&target)
            .file_name()?.to_string_lossy().to_lowercase();
        let product_name = crate::discovery::read_product_name(&target)
            .unwrap_or_else(|| stem.clone());

        let identity = AppIdentity::Win32 {
            exe_name,
            product_name: product_name.clone(),
        };

        Some(InstalledApp {
            display_name: product_name,
            identity,
            exe_path: Some(target),
        })
    }
}
