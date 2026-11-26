use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use eframe::egui;
use crate::config::Config;
use crate::identity::{AppIdentity, AppInfo};
use crate::restart::{Done, RestartError};

/// Application state, shared between the UI thread, tray event handlers and
/// restart worker threads. Never hold the lock across slow work.
pub struct AppState {
    pub apps: Vec<AppInfo>,
    pub config: Config,
    pub search_query: String,
    /// The app a restart/start job is running for. Only one job at a time —
    /// repeated clicks are ignored while this is set.
    pub restart_in_progress: Option<AppIdentity>,
    /// Last status message shown at the bottom of the window.
    pub status_message: Option<String>,
    /// Set by workers when running state changed; the UI thread resyncs the
    /// tray menu labels and clears it.
    pub tray_stale: bool,
    /// Icon textures loaded on the UI thread, keyed by exe path or AUMID.
    pub icon_cache: HashMap<String, egui::TextureHandle>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            apps: Vec::new(),
            config: Config::load(),
            search_query: String::new(),
            restart_in_progress: None,
            status_message: None,
            tray_stale: false,
            icon_cache: HashMap::new(),
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Favorites as (identity, display name), the input discovery needs.
    pub fn favorite_identities(&self) -> Vec<(AppIdentity, String)> {
        self.config
            .favorites
            .iter()
            .map(|f| (f.identity.clone(), f.display_name.clone()))
            .collect()
    }

    /// Refresh the running app list from the OS (blocking, ~0.1 s).
    pub fn refresh_apps(&mut self) {
        let apps = crate::discovery::discover_with_favorites(&self.favorite_identities());
        self.apply_discovery(apps);
    }

    /// Install a discovery result. Split from `refresh_apps` so callers on
    /// other threads can discover without holding the state lock.
    pub fn apply_discovery(&mut self, apps: Vec<AppInfo>) {
        self.apps = apps;

        // Remember where running favorites currently live. This is only a
        // fallback hint for launching when no installer entry can be found;
        // the identity, not this path, is what the favorite is keyed on.
        let mut changed = false;
        for fav in &mut self.config.favorites {
            let seen = self
                .apps
                .iter()
                .find(|a| a.identity == fav.identity)
                .and_then(|a| a.exe_path.clone());
            if seen.is_some() && seen != fav.last_known_path {
                fav.last_known_path = seen;
                changed = true;
            }
        }
        if changed {
            if let Err(e) = self.config.save() {
                self.status_message = Some(format!("保存配置失败: {e}"));
            }
        }
    }

    /// All favorites in config order, running or not. Favorites that are not
    /// running get a stub entry with `running == false` so they can be started.
    pub fn favorite_apps(&self) -> Vec<AppInfo> {
        self.config
            .favorites
            .iter()
            .map(|fav| match self.apps.iter().find(|a| a.identity == fav.identity) {
                Some(app) => {
                    let mut app = app.clone();
                    app.display_name = fav.display_name.clone();
                    app
                }
                None => AppInfo::new(fav.identity.clone(), fav.display_name.clone()),
            })
            .collect()
    }

    /// Search-filtered view for the main window: favorites first, then other
    /// running apps.
    pub fn visible_apps(&self) -> (Vec<AppInfo>, Vec<AppInfo>) {
        let query = self.search_query.to_lowercase();
        let matches = |name: &str| query.is_empty() || name.to_lowercase().contains(&query);

        let favorites: Vec<AppInfo> = self
            .favorite_apps()
            .into_iter()
            .filter(|a| matches(&a.display_name))
            .collect();

        let running: Vec<AppInfo> = self
            .apps
            .iter()
            .filter(|a| !self.config.is_favorite(&a.identity) && matches(&a.display_name))
            .cloned()
            .collect();

        (favorites, running)
    }

    /// User-facing name for `identity` (favorite name, else discovered name).
    pub fn name_of(&self, identity: &AppIdentity) -> String {
        self.config
            .favorites
            .iter()
            .find(|f| &f.identity == identity)
            .map(|f| f.display_name.clone())
            .or_else(|| {
                self.apps
                    .iter()
                    .find(|a| &a.identity == identity)
                    .map(|a| a.display_name.clone())
            })
            .unwrap_or_else(|| identity.display_name().to_string())
    }

    /// Toggle favorite status and persist immediately.
    pub fn toggle_favorite(&mut self, identity: &AppIdentity, display_name: &str) {
        if self.config.is_favorite(identity) {
            self.config.remove_favorite(identity);
        } else {
            let last_known_path = self
                .apps
                .iter()
                .find(|a| &a.identity == identity)
                .and_then(|a| a.exe_path.clone());
            self.config.add_favorite(crate::config::FavoriteEntry {
                display_name: display_name.to_string(),
                identity: identity.clone(),
                last_known_path,
            });
        }
        if let Err(e) = self.config.save() {
            self.status_message = Some(format!("保存配置失败: {e}"));
        }
    }

    /// Guard: prevent operating on this process itself.
    pub fn is_self(&self, pids: &[u32]) -> bool {
        pids.iter().any(|&pid| crate::process::is_self(pid))
    }

}

/// Restart (if running) or start (if not) `identity` on a worker thread.
/// Returns immediately. The worker writes its outcome straight into `state`
/// (not via the UI loop), so it completes even while the window is hidden.
/// Ignored while another job is in progress.
pub fn begin_operation(state: &Arc<Mutex<AppState>>, ctx: &egui::Context, identity: &AppIdentity) {
    let (name, path_hint) = {
        let mut s = state.lock().unwrap();
        if s.restart_in_progress.is_some() {
            return;
        }
        let name = s.name_of(identity);
        let hint = s
            .config
            .favorites
            .iter()
            .find(|f| &f.identity == identity)
            .and_then(|f| f.last_known_path.clone());
        s.restart_in_progress = Some(identity.clone());
        s.status_message = Some(format!("正在处理 {name}…"));
        (name, hint)
    };
    ctx.request_repaint();

    let worker_state = state.clone();
    let ctx = ctx.clone();
    let identity = identity.clone();
    let spawned = std::thread::Builder::new()
        .name("relaunch-op".into())
        .spawn(move || {
            // ShellExecute and IShellLink want COM on the calling thread.
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
                );
            }
            let result = crate::restart::run(&identity, path_hint.as_deref());
            let message = describe_outcome(&name, &result);

            let favorites = worker_state.lock().unwrap().favorite_identities();
            let apps = crate::discovery::discover_with_favorites(&favorites);
            {
                let mut s = worker_state.lock().unwrap();
                s.apply_discovery(apps);
                s.restart_in_progress = None;
                s.status_message = Some(message);
                s.tray_stale = true;
            }
            ctx.request_repaint();
        });
    if let Err(e) = spawned {
        let mut s = state.lock().unwrap();
        s.restart_in_progress = None;
        s.status_message = Some(format!("无法创建后台任务: {e}"));
    }
}

fn describe_outcome(name: &str, result: &Result<Done, RestartError>) -> String {
    match result {
        Ok(Done::Restarted) => format!("已重启 {name}"),
        Ok(Done::Started) => format!("已启动 {name}"),
        Err(RestartError::NoLaunchEntry) => {
            format!("找不到 {name} 的启动入口，已取消操作（原程序未被关闭）")
        }
        Err(RestartError::AccessDenied) => {
            format!("{name} 需要管理员权限，请以管理员身份运行 ReLaunch")
        }
        Err(RestartError::NotDetected) => {
            format!("已发出启动指令，但 10 秒内没有检测到 {name} 的新进程")
        }
        Err(RestartError::LaunchFailed(e)) => format!("启动 {name} 失败: {e}"),
        Err(RestartError::Other(e)) => format!("操作 {name} 失败: {e}"),
    }
}

/// Top-level eframe app struct.
pub struct ReLaunchApp {
    pub state: Arc<Mutex<AppState>>,
    /// True when the tray icon exists, so closing the window can hide to tray.
    pub tray_ok: bool,
}

impl ReLaunchApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);
        let state = Arc::new(Mutex::new(AppState::new()));

        let hwnd = main_hwnd(cc);
        let tray_ok = match crate::tray::install(state.clone(), cc.egui_ctx.clone(), hwnd) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("托盘初始化失败: {e}");
                false
            }
        };

        // Kick off initial app discovery in the background so the window opens
        // immediately without waiting ~0.1-3 s for the scan to finish.
        {
            let bg_state = state.clone();
            let ctx = cc.egui_ctx.clone();
            let _ = std::thread::Builder::new()
                .name("relaunch-init-refresh".into())
                .spawn(move || {
                    unsafe {
                        let _ = windows::Win32::System::Com::CoInitializeEx(
                            None,
                            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
                        );
                    }
                    let favorites = bg_state.lock().unwrap().favorite_identities();
                    let apps = crate::discovery::discover_with_favorites(&favorites);
                    {
                        let mut s = bg_state.lock().unwrap();
                        s.apply_discovery(apps);
                        s.tray_stale = true;
                    }
                    ctx.request_repaint();
                });
        }

        Self { state, tray_ok }
    }
}

/// Native handle of the main window, for showing it while hidden in the tray.
fn main_hwnd(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// egui's bundled fonts have no CJK glyphs. Load a system Chinese font as a
/// fallback (read from the Windows font directory at startup, not bundled, so
/// the EXE stays small). Latin text keeps egui's default font.
fn install_cjk_font(ctx: &eframe::egui::Context) {
    use eframe::egui::{FontData, FontDefinitions, FontFamily};

    let fonts_dir = std::path::PathBuf::from(
        std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()),
    )
    .join("Fonts");
    // Microsoft YaHei UI first (Windows 10/11), then older fallbacks.
    let candidates = ["msyh.ttc", "Deng.ttf", "simhei.ttf", "simsun.ttc"];
    let Some(bytes) = candidates
        .iter()
        .find_map(|name| std::fs::read(fonts_dir.join(name)).ok())
    else {
        tracing::warn!("未找到中文字体，中文将无法显示");
        return;
    };

    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".to_owned(), FontData::from_owned(bytes));
    // Append as fallback — egui's built-in fonts (including the icon glyphs)
    // stay at the front and are tried first. CJK fills in what they miss.
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("cjk".to_owned());
    }
    ctx.set_fonts(fonts);
}
