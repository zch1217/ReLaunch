use eframe::egui;
use crate::app::{begin_operation, AppState, ReLaunchApp};
use crate::identity::{AppIdentity, AppInfo};

/// What the user did this frame that needs work outside the state lock.
#[derive(Default)]
struct FrameActions {
    /// 重启/启动 clicked for this app.
    run: Option<AppIdentity>,
    /// Favorites added/removed or list refreshed: tray menu must follow.
    tray_dirty: bool,
}

impl eframe::App for ReLaunchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Closing the window hides it to the tray; 退出 in the tray menu quits.
        // Without a tray icon there is no way back, so then close for real.
        if self.tray_ok && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        let mut actions = FrameActions::default();
        let tray_favorites = {
            let mut state = self.state.lock().unwrap();
            egui::CentralPanel::default().show(ctx, |ui| {
                render_toolbar(ui, &mut state, &mut actions);
                ui.separator();
                render_app_list(ui, &mut state, &mut actions);
                ui.separator();
                render_status_bar(ui, &state);
            });
            if state.tray_stale || actions.tray_dirty {
                state.tray_stale = false;
                Some(state.favorite_apps())
            } else {
                None
            }
        };

        // Outside the lock: the job and the tray both take it themselves.
        if let Some(identity) = actions.run {
            begin_operation(&self.state, ctx, &identity);
        }
        if let Some(favorites) = tray_favorites {
            crate::tray::sync_menu(&favorites);
        }
    }
}

fn render_toolbar(ui: &mut egui::Ui, state: &mut AppState, actions: &mut FrameActions) {
    ui.horizontal(|ui| {
        ui.heading("ReLaunch");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("⚙").on_hover_text("设置").clicked() {
                // TODO: open settings panel
            }
            let idle = state.restart_in_progress.is_none();
            if ui
                .add_enabled(idle, egui::Button::new("↻"))
                .on_hover_text("刷新应用列表")
                .clicked()
            {
                state.refresh_apps();
                actions.tray_dirty = true;
            }
            ui.add(
                egui::TextEdit::singleline(&mut state.search_query)
                    .hint_text("搜索应用…")
                    .desired_width(160.0),
            );
        });
    });
}

fn render_app_list(ui: &mut egui::Ui, state: &mut AppState, actions: &mut FrameActions) {
    let (favorites, running) = state.visible_apps();

    egui::ScrollArea::vertical().show(ui, |ui| {
        if !favorites.is_empty() {
            ui.label(egui::RichText::new("收藏").strong());
            ui.separator();
            for app in &favorites {
                render_app_row(ui, state, app, actions);
            }
            ui.add_space(8.0);
        }

        if !running.is_empty() {
            ui.label(egui::RichText::new("正在运行").strong());
            ui.separator();
            for app in &running {
                render_app_row(ui, state, app, actions);
            }
        }

        if favorites.is_empty() && running.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label("没有找到运行中的应用");
            });
        }
    });
}

fn render_app_row(
    ui: &mut egui::Ui,
    state: &mut AppState,
    app: &AppInfo,
    actions: &mut FrameActions,
) {
    const ICON_SIZE: f32 = 20.0;
    let identity = &app.identity;
    let is_self = state.is_self(&app.pids);
    let busy_any = state.restart_in_progress.is_some();
    let busy_this = state.restart_in_progress.as_ref() == Some(identity);
    let is_favorite = state.config.is_favorite(identity);

    // Ensure the icon texture is loaded (lazy, one-time per exe path / AUMID).
    let cache_key: String = app
        .exe_path
        .clone()
        .unwrap_or_else(|| identity.display_name().to_string());

    if !state.icon_cache.contains_key(&cache_key) {
        if let Some(rgba) = &app.icon {
            let color_image = egui::ColorImage::from_rgba_unmultiplied([32, 32], rgba);
            let handle = ui.ctx().load_texture(
                &cache_key,
                color_image,
                egui::TextureOptions::LINEAR,
            );
            state.icon_cache.insert(cache_key.clone(), handle);
        }
    }

    ui.horizontal(|ui| {
        // App icon (or spacer when none).
        if let Some(tex) = state.icon_cache.get(&cache_key) {
            let img = egui::Image::new(tex)
                .fit_to_exact_size(egui::vec2(ICON_SIZE, ICON_SIZE))
                .sense(egui::Sense::hover());
            ui.add(img);
        } else {
            ui.allocate_space(egui::vec2(ICON_SIZE, ICON_SIZE));
        }

        let star = if is_favorite { "★" } else { "☆" };
        let star_tip = if is_favorite { "取消收藏" } else { "收藏" };
        if ui.small_button(star).on_hover_text(star_tip).clicked() {
            state.toggle_favorite(identity, &app.display_name);
            actions.tray_dirty = true;
        }

        let status = if app.running { "● 运行中" } else { "○ 未运行" };
        ui.label(&app.display_name);
        ui.weak(status);

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if is_self {
                ui.weak("(自身)");
                return;
            }
            if busy_this {
                ui.add(egui::Spinner::new());
                ui.weak("处理中…");
                return;
            }
            let label = if app.running { "重启" } else { "启动" };
            if ui.add_enabled(!busy_any, egui::Button::new(label)).clicked() {
                actions.run = Some(identity.clone());
            }
        });
    });
}

fn render_status_bar(ui: &mut egui::Ui, state: &AppState) {
    let running_count = state.apps.iter().filter(|a| a.running).count();
    ui.horizontal(|ui| match &state.status_message {
        Some(msg) => {
            ui.label(msg);
        }
        None => {
            ui.label(format!("共 {running_count} 个运行中的应用"));
        }
    });
}
