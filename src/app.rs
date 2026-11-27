use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use eframe::egui;
use crate::config::Config;
use crate::icon_extract::IconRgba;
use crate::identity::{AppIdentity, AppInfo};
use crate::installed::InstalledApp;
use crate::restart::{Done, RestartError};

/// How often the running app list auto-refreshes while the main window is visible.
pub const AUTO_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// Semantic level of a status bar message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct StatusMessage {
    pub text: String,
    pub kind: StatusKind,
}

/// Application state, shared between the UI thread, tray event handlers and
/// restart worker threads. Never hold the lock across slow work.
pub struct AppState {
    pub apps: Vec<AppInfo>,
    pub config: Config,
    pub search_query: String,
    /// The app a restart/start job is running for. Only one job at a time —
    /// repeated clicks are ignored while this is set.
    pub restart_in_progress: Option<AppIdentity>,
    /// True while a background refresh of running apps is in flight.
    pub refreshing: bool,
    /// True only when the current refresh was triggered manually by clicking ↻.
    pub manual_refreshing: bool,
    /// Last time a scan of running apps started.
    pub last_refresh: Option<Instant>,
    /// Last status message shown at the bottom of the window.
    pub status_message: Option<StatusMessage>,
    /// Set by workers when running state changed; the UI thread resyncs the
    /// tray menu labels and clears it.
    pub tray_stale: bool,
    /// Icon textures loaded on the UI thread, keyed by `icon_extract::cache_key`.
    pub icon_cache: HashMap<String, egui::TextureHandle>,
    /// Session-wide cache of installed apps so reopening "+" is instantaneous.
    pub installed_cache: Option<Vec<InstalledApp>>,
    /// Icons extracted on worker threads waiting to be uploaded to GPU textures on the UI thread.
    pub pending_icons: Vec<(String, IconRgba)>,
    /// "添加应用"面板状态，None 表示面板关闭。
    pub add_panel: Option<AddPanelState>,
    /// Monotonic counter used to discard stale background scan updates when the
    /// add-app panel is closed and reopened.
    pub add_panel_seq: u64,
    /// Whether HKCU Run auto-start is enabled for the current executable.
    pub autostart: bool,
}

/// State for the "添加应用" picker panel.
pub struct AddPanelState {
    pub seq: u64,
    pub search: String,
    pub scanning: bool,
    /// Count of apps found so far, updated while scanning.
    pub scan_count: usize,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            apps: Vec::new(),
            config: Config::load(),
            search_query: String::new(),
            restart_in_progress: None,
            refreshing: false,
            manual_refreshing: false,
            last_refresh: None,
            status_message: None,
            tray_stale: false,
            icon_cache: HashMap::new(),
            installed_cache: None,
            pending_icons: Vec::new(),
            add_panel: None,
            add_panel_seq: 0,
            autostart: crate::autostart::is_enabled(),
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_status(&mut self, kind: StatusKind, text: impl Into<String>) {
        self.status_message = Some(StatusMessage {
            text: text.into(),
            kind,
        });
    }

    /// Favorites as (identity, display name), the input discovery needs.
    pub fn favorite_identities(&self) -> Vec<(AppIdentity, String)> {
        self.config
            .favorites
            .iter()
            .map(|f| (f.identity.clone(), f.display_name.clone()))
            .collect()
    }

    /// Install a discovery result. Split so callers on other threads can
    /// discover without holding the state lock.
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
                self.set_status(StatusKind::Error, format!("保存配置失败: {e}"));
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
                None => {
                    let mut info = AppInfo::new(fav.identity.clone(), fav.display_name.clone());
                    info.exe_path = fav.last_known_path.clone();
                    info
                }
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
            self.set_status(StatusKind::Error, format!("保存配置失败: {e}"));
        }
    }

    /// Toggle Windows login auto-start for the current executable and update status bar.
    pub fn set_autostart(&mut self, enable: bool) -> bool {
        match crate::autostart::set_enabled(enable) {
            Ok(()) => {
                self.autostart = enable;
                let msg = if enable {
                    "已开启开机自启（开机将静默启动到托盘）"
                } else {
                    "已关闭开机自启"
                };
                self.set_status(StatusKind::Success, msg);
                true
            }
            Err(e) => {
                self.autostart = crate::autostart::is_enabled();
                self.set_status(StatusKind::Error, format!("设置开机自启失败: {e}"));
                false
            }
        }
    }

    /// Guard: prevent operating on this process itself.
    pub fn is_self(&self, pids: &[u32]) -> bool {
        pids.iter().any(|&pid| crate::process::is_self(pid))
    }
}

/// Refresh the running app list on a background thread.
/// Ignored if a refresh is already in flight.
pub fn begin_refresh(state: &Arc<Mutex<AppState>>, ctx: &egui::Context) {
    {
        let mut s = state.lock().unwrap();
        if s.refreshing {
            return;
        }
        s.refreshing = true;
        s.last_refresh = Some(Instant::now());
    }
    ctx.request_repaint();

    let bg_state = state.clone();
    let ctx = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("relaunch-refresh".into())
        .spawn(move || {
            let _com = crate::com::ComGuard::init_sta();
            let favorites = bg_state.lock().unwrap().favorite_identities();
            let apps = crate::discovery::discover_with_favorites(&favorites);

            // Also warm icons for favorites (including non-running ones).
            let fav_targets: Vec<(String, AppIdentity, Option<String>)> = {
                let s = bg_state.lock().unwrap();
                s.config
                    .favorites
                    .iter()
                    .filter_map(|fav| {
                        let key = crate::icon_extract::cache_key(
                            &fav.identity,
                            fav.last_known_path.as_deref(),
                        );
                        if s.icon_cache.contains_key(&key) {
                            None
                        } else {
                            Some((key, fav.identity.clone(), fav.last_known_path.clone()))
                        }
                    })
                    .collect()
            };
            let mut fav_icons = Vec::new();
            for (key, id, path) in fav_targets {
                if let Some(ic) = crate::icon_extract::extract_app_icon(&id, path.as_deref()) {
                    fav_icons.push((key, ic));
                }
            }

            {
                let mut s = bg_state.lock().unwrap();
                s.apply_discovery(apps);
                s.pending_icons.extend(fav_icons);
                s.refreshing = false;
                s.manual_refreshing = false;
                s.tray_stale = true;
            }
            ctx.request_repaint();
        });
    if let Err(e) = spawned {
        let mut s = state.lock().unwrap();
        s.refreshing = false;
        s.manual_refreshing = false;
        s.set_status(StatusKind::Error, format!("无法刷新应用列表: {e}"));
    }
}

/// Enumerate installed apps and extract missing icons in the background,
/// flushing batches progressively to `pending_icons`.
pub fn begin_load_installed(state: &Arc<Mutex<AppState>>, ctx: &egui::Context, seq: u64) {
    let st = state.clone();
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new()
        .name("relaunch-enum-installed".into())
        .spawn(move || {
            let _com = crate::com::ComGuard::init_sta();
            let st_prog = st.clone();
            let ctx_prog = ctx.clone();
            let apps = crate::installed::enumerate_installed(move |n| {
                if let Some(panel) = st_prog.lock().unwrap().add_panel.as_mut() {
                    if panel.seq == seq {
                        panel.scan_count = n;
                    }
                }
                ctx_prog.request_repaint();
            });

            let missing: Vec<(String, AppIdentity, Option<String>, Option<String>)> = {
                let mut s = st.lock().unwrap();
                if s.add_panel_seq != seq {
                    return;
                }
                let mut seen_keys = HashSet::new();
                let missing = apps
                    .iter()
                    .filter_map(|app| {
                        let key = crate::icon_extract::cache_key_for(
                            &app.identity,
                            app.exe_path.as_deref(),
                            app.icon_path.as_deref(),
                        );
                        if s.icon_cache.contains_key(&key) || !seen_keys.insert(key.clone()) {
                            None
                        } else {
                            Some((
                                key,
                                app.identity.clone(),
                                app.exe_path.clone(),
                                app.icon_path.clone(),
                            ))
                        }
                    })
                    .collect();
                s.installed_cache = Some(apps);
                if let Some(panel) = s.add_panel.as_mut() {
                    if panel.seq == seq {
                        panel.scanning = false;
                    }
                }
                missing
            };
            ctx.request_repaint();

            let mut batch: Vec<(String, IconRgba)> = Vec::with_capacity(12);
            let mut last_flush = Instant::now();
            for (key, id, exe_path, icon_path) in missing {
                if st.lock().unwrap().add_panel_seq != seq {
                    return;
                }
                if let Some(icon) = crate::icon_extract::extract_app_icon_with_override(
                    &id,
                    exe_path.as_deref(),
                    icon_path.as_deref(),
                ) {
                    batch.push((key, icon));
                }
                if batch.len() >= 12
                    || (!batch.is_empty() && last_flush.elapsed() >= Duration::from_millis(80))
                {
                    let mut s = st.lock().unwrap();
                    if s.add_panel_seq != seq {
                        return;
                    }
                    s.pending_icons.append(&mut batch);
                    drop(s);
                    last_flush = Instant::now();
                    ctx.request_repaint();
                }
            }
            if !batch.is_empty() {
                let mut s = st.lock().unwrap();
                if s.add_panel_seq == seq {
                    s.pending_icons.append(&mut batch);
                }
                drop(s);
                ctx.request_repaint();
            }
        });
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
        s.set_status(StatusKind::Info, format!("正在处理 {name}…"));
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
            let _com = crate::com::ComGuard::init_sta();
            let result = crate::restart::run(&identity, path_hint.as_deref());
            let message = describe_outcome(&name, &result);

            let favorites = worker_state.lock().unwrap().favorite_identities();
            let apps = crate::discovery::discover_with_favorites(&favorites);
            {
                let mut s = worker_state.lock().unwrap();
                s.apply_discovery(apps);
                s.last_refresh = Some(Instant::now());
                s.restart_in_progress = None;
                s.status_message = Some(message);
                s.tray_stale = true;
            }
            ctx.request_repaint();
        });
    if let Err(e) = spawned {
        let mut s = state.lock().unwrap();
        s.restart_in_progress = None;
        s.set_status(StatusKind::Error, format!("无法创建后台任务: {e}"));
    }
}

fn describe_outcome(name: &str, result: &Result<Done, RestartError>) -> StatusMessage {
    match result {
        Ok(Done::Restarted) => StatusMessage {
            text: format!("已重启 {name}"),
            kind: StatusKind::Success,
        },
        Ok(Done::Started) => StatusMessage {
            text: format!("已启动 {name}"),
            kind: StatusKind::Success,
        },
        Err(RestartError::NoLaunchEntry) => StatusMessage {
            text: format!("找不到 {name} 的启动入口，已取消操作（原程序未被关闭）"),
            kind: StatusKind::Error,
        },
        Err(RestartError::AccessDenied) => StatusMessage {
            text: format!("{name} 需要管理员权限，请以管理员身份运行 ReLaunch"),
            kind: StatusKind::Error,
        },
        Err(RestartError::CloseTimeout) => StatusMessage {
            text: format!("等待 {name} 进程退出超时，已取消启动以避免旧进程残留"),
            kind: StatusKind::Error,
        },
        Err(RestartError::NotDetected) => StatusMessage {
            text: format!("已发出启动指令，但 10 秒内没有检测到 {name} 的新进程"),
            kind: StatusKind::Error,
        },
        Err(RestartError::ExitedEarly) => StatusMessage {
            text: format!("已启动 {name}，但新进程在完成初始化前退出"),
            kind: StatusKind::Error,
        },
        Err(RestartError::InitTimeout) => StatusMessage {
            text: format!("已检测到 {name} 的新进程，但超时内未完成初始化"),
            kind: StatusKind::Error,
        },
        Err(RestartError::LaunchFailed(e)) => StatusMessage {
            text: format!("启动 {name} 失败: {e}"),
            kind: StatusKind::Error,
        },
        Err(RestartError::Other(e)) => StatusMessage {
            text: format!("操作 {name} 失败: {e}"),
            kind: StatusKind::Error,
        },
    }
}

/// Top-level eframe app struct.
pub struct ReLaunchApp {
    pub state: Arc<Mutex<AppState>>,
    /// True when the tray icon exists, so closing the window can hide to tray.
    pub tray_ok: bool,
    /// Dark mode state last applied to `egui::Context` visuals.
    pub applied_dark_mode: Option<bool>,
}

impl ReLaunchApp {
    pub fn new(cc: &eframe::CreationContext<'_>, start_minimized: bool) -> Self {
        install_fonts(&cc.egui_ctx);
        let state = Arc::new(Mutex::new(AppState::new()));
        let dark_mode = state.lock().unwrap().config.dark_mode;
        crate::theme::apply_visuals(&cc.egui_ctx, dark_mode);

        let hwnd = main_hwnd(cc);
        let tray_ok = match crate::tray::install(state.clone(), cc.egui_ctx.clone(), hwnd) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("托盘初始化失败: {e}");
                false
            }
        };
        if start_minimized && !tray_ok {
            crate::tray::show_main_window(hwnd, &cc.egui_ctx);
        }

        // Kick off initial app discovery in the background so the window opens
        // immediately without waiting ~0.1-3 s for the scan to finish.
        begin_refresh(&state, &cc.egui_ctx);

        Self {
            state,
            tray_ok,
            applied_dark_mode: Some(dark_mode),
        }
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

/// Load Windows native regular + bold fonts (Segoe UI for Latin, Microsoft YaHei UI
/// / fallback for CJK) so titles and card headings have crisp semibold/bold weights.
fn install_fonts(ctx: &eframe::egui::Context) {
    use eframe::egui::{FontData, FontDefinitions, FontFamily};

    fn ttc_face_index(bytes: &[u8]) -> u32 {
        if bytes.len() >= 12 && &bytes[0..4] == b"ttcf" {
            let num_fonts = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
            if num_fonts >= 2 {
                return 1;
            }
        }
        0
    }

    let fonts_dir = std::path::PathBuf::from(
        std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()),
    )
    .join("Fonts");

    let mut fonts = FontDefinitions::default();
    let default_prop = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();

    let cjk_candidates = ["msyh.ttc", "Deng.ttf", "simhei.ttf", "simsun.ttc"];
    if let Some((name, bytes)) = cjk_candidates
        .iter()
        .find_map(|&name| std::fs::read(fonts_dir.join(name)).ok().map(|b| (name, b)))
    {
        let mut data = FontData::from_owned(bytes);
        if name.eq_ignore_ascii_case("msyh.ttc") {
            data.index = ttc_face_index(&data.font);
        }
        fonts.font_data.insert("cjk".to_owned(), Arc::new(data));
    } else {
        tracing::warn!("未找到中文字体，中文将无法显示");
    }

    if let Ok(bytes) = std::fs::read(fonts_dir.join("segoeui.ttf")) {
        fonts
            .font_data
            .insert("segoe_regular".to_owned(), Arc::new(FontData::from_owned(bytes)));
    }
    if let Ok(bytes) = std::fs::read(fonts_dir.join("segoeuib.ttf")) {
        fonts
            .font_data
            .insert("segoe_bold".to_owned(), Arc::new(FontData::from_owned(bytes)));
    }
    if let Ok(bytes) = std::fs::read(fonts_dir.join("msyhbd.ttc")) {
        let mut data = FontData::from_owned(bytes);
        data.index = ttc_face_index(&data.font);
        fonts.font_data.insert("cjk_bold".to_owned(), Arc::new(data));
    }

    // Proportional: Segoe UI -> CJK -> egui built-in fallback.
    let mut prop_order = Vec::new();
    if fonts.font_data.contains_key("segoe_regular") {
        prop_order.push("segoe_regular".to_owned());
    }
    if fonts.font_data.contains_key("cjk") {
        prop_order.push("cjk".to_owned());
    }
    prop_order.extend(default_prop.iter().cloned());
    fonts.families.insert(FontFamily::Proportional, prop_order);

    // Monospace: keep egui's monospace first, append CJK as fallback.
    if fonts.font_data.contains_key("cjk") {
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .push("cjk".to_owned());
    }

    // Bold family: Segoe UI Bold -> YaHei UI Bold -> CJK regular -> egui default.
    let mut bold_order = Vec::new();
    for key in ["segoe_bold", "cjk_bold", "cjk"] {
        if fonts.font_data.contains_key(key) {
            bold_order.push(key.to_owned());
        }
    }
    bold_order.extend(default_prop);
    fonts
        .families
        .insert(FontFamily::Name("bold".into()), bold_order);

    ctx.set_fonts(fonts);
}
