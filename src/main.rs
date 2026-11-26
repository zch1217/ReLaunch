mod app;
mod config;
mod discovery;
mod icon;
mod icon_extract;
mod identity;
mod launcher;
mod process;
mod restart;
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
        match restart::restart(&app.identity, &app.pids) {
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
    // instead of launching a second instance. (TODO: signal the existing window.)
    #[cfg(target_os = "windows")]
    {
        use windows::core::PCWSTR;
        use windows::Win32::System::Threading::CreateMutexW;
        let name: Vec<u16> = "ReLaunch_SingleInstance\0".encode_utf16().collect();
        let mutex = unsafe {
            CreateMutexW(None, true, PCWSTR(name.as_ptr()))
        };
        if let Ok(m) = mutex {
            use windows::Win32::Foundation::ERROR_ALREADY_EXISTS;
            let last_error = unsafe { windows::Win32::Foundation::GetLastError() };
            if last_error == ERROR_ALREADY_EXISTS {
                // Another instance is running — exit silently.
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
            .with_inner_size([480.0, 600.0])
            .with_min_inner_size([360.0, 400.0])
            .with_icon(load_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "ReLaunch",
        options,
        Box::new(|cc| Ok(Box::new(app::ReLaunchApp::new(cc)))),
    )
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
