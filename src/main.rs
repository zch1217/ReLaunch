#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod com;
mod config;
mod discovery;
mod icon;
mod icon_extract;
mod installed;
mod identity;
mod launcher;
mod process;
mod restart;
mod theme;
mod tray;
mod ui;

use eframe::egui;
use tracing_subscriber::EnvFilter;

fn main() -> eframe::Result<()> {
    // Initialise logging. Set RUST_LOG=debug to see verbose output.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let args: Vec<String> = std::env::args().collect();

    // Diagnostic mode: print what discovery sees and exit. Read-only.
    if args.iter().any(|a| a == "--list") {
        let t0 = std::time::Instant::now();
        let apps = discovery::discover_with_favorites(&config_favorites());
        let t_discover = t0.elapsed();
        for app in apps {
            let t = std::time::Instant::now();
            let entry = launcher::resolve_launch_entry(&app.identity, app.exe_path.as_deref())
                .map(|e| e.describe())
                .unwrap_or_else(|| "<未找到启动入口>".to_string());
            println!(
                "{}\n    身份: {:?}\n    进程: {:?}\n    启动入口: {} ({} ms)",
                app.display_name, app.identity, app.pids, entry, t.elapsed().as_millis()
            );
        }
        println!("识别耗时 {} ms，总耗时 {} ms", t_discover.as_millis(), t0.elapsed().as_millis());
        return Ok(());
    }

    // Diagnostic mode: print enumerated installed apps and exit. Read-only.
    if args.iter().any(|a| a == "--installed") {
        let t0 = std::time::Instant::now();
        let apps = installed::enumerate_installed(|_| {});
        let count = apps.len();
        for app in apps {
            println!(
                "{}\n    身份: {:?}\n    路径: {:?}",
                app.display_name, app.identity, app.exe_path
            );
        }
        println!("共 {count} 个已安装应用，耗时 {} ms", t0.elapsed().as_millis());
        return Ok(());
    }

    // Test mode: restart one running app by display name, without the GUI.
    if let Some(pos) = args.iter().position(|a| a == "--restart") {
        let Some(name) = args.get(pos + 1) else {
            eprintln!("用法: relaunch --restart <应用名>");
            std::process::exit(2);
        };
        let Some(app) = discovery::discover_with_favorites(&config_favorites())
            .into_iter()
            .find(|a| a.display_name.eq_ignore_ascii_case(name))
        else {
            eprintln!("没有找到正在运行的应用: {name}");
            std::process::exit(1);
        };
        println!("重启 {}，当前进程: {:?}", app.display_name, app.pids);
        match restart::restart(&app.identity, app.exe_path.as_deref()) {
            Ok(()) => println!("重启指令已完成"),
            Err(e) => {
                eprintln!("重启失败: {e:?}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // Single-instance guard via a named mutex.
    // If the mutex already exists the app is already running; bring it to focus
    // unless started silently for background tray startup.
    let start_minimized = args.iter().any(|a| a == "--minimized" || a == "--autostart");

    #[cfg(target_os = "windows")]
    {
        use windows::core::PCWSTR;
        use windows::Win32::System::Threading::CreateMutexW;
        let name: Vec<u16> = "ReLaunch_SingleInstance\0".encode_utf16().collect();
        let mutex = unsafe { CreateMutexW(None, true, PCWSTR(name.as_ptr())) };
        if let Ok(m) = mutex {
            use windows::Win32::Foundation::ERROR_ALREADY_EXISTS;
            let last_error = unsafe { windows::Win32::Foundation::GetLastError() };
            if last_error == ERROR_ALREADY_EXISTS {
                if !start_minimized {
                    focus_existing_instance();
                }
                let _ = m;
                return Ok(());
            }
            // Intentionally leak the handle so it lives for the process lifetime.
            let _ = m; // handle stays open for the process lifetime
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ReLaunch")
            .with_inner_size([520.0, 640.0])
            .with_min_inner_size([380.0, 440.0])
            .with_visible(!start_minimized)
            .with_icon(load_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "ReLaunch",
        options,
        Box::new(move |cc| Ok(Box::new(app::ReLaunchApp::new(cc, start_minimized)))),
    )
}

#[cfg(target_os = "windows")]
fn focus_existing_instance() {
    use windows::Win32::{
        Foundation::{BOOL, HWND, LPARAM},
        UI::WindowsAndMessaging::{
            EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsIconic, SetForegroundWindow,
            ShowWindow, SW_RESTORE, SW_SHOW,
        },
    };

    struct Search {
        exe: String,
        found: Option<HWND>,
    }

    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = &mut *(lparam.0 as *mut Search);
        let mut title = [0u16; 32];
        let len = GetWindowTextW(hwnd, &mut title);
        if len <= 0 || String::from_utf16_lossy(&title[..len as usize]) != "ReLaunch" {
            return BOOL(1);
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 || pid == std::process::id() {
            return BOOL(1);
        }

        let matches_exe = discovery::query_exe_path(pid)
            .and_then(|p| {
                std::path::Path::new(&p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_lowercase() == search.exe)
            })
            .unwrap_or(false);
        if !matches_exe {
            return BOOL(1);
        }

        search.found = Some(hwnd);
        BOOL(0)
    }

    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
        .unwrap_or_else(|| "relaunch.exe".to_string());
    let mut search = Search { exe, found: None };
    unsafe {
        let _ = EnumWindows(Some(callback), LPARAM(&mut search as *mut Search as isize));
        if let Some(hwnd) = search.found {
            let cmd = if IsIconic(hwnd).as_bool() {
                SW_RESTORE
            } else {
                SW_SHOW
            };
            let _ = ShowWindow(hwnd, cmd);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

fn load_icon() -> egui::IconData {
    egui::IconData {
        rgba: icon::rgba(64),
        width: 64,
        height: 64,
    }
}

/// Favorites from the saved config, as (identity, display name).
fn config_favorites() -> Vec<(identity::AppIdentity, String)> {
    config::Config::load()
        .favorites
        .into_iter()
        .map(|f| (f.identity, f.display_name))
        .collect()
}
