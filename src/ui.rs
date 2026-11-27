use std::collections::HashSet;
use eframe::egui::{
    self, Color32, CornerRadius, CursorIcon, Margin, Sense, Stroke, StrokeKind, Vec2,
};
use crate::app::{
    begin_load_installed, begin_operation, begin_refresh, AddPanelState, AppState, ReLaunchApp,
    StatusKind, AUTO_REFRESH_INTERVAL,
};
use crate::identity::{AppIdentity, AppInfo};
use crate::theme::{self, Palette};

const ICON_TEX_OPTIONS: egui::TextureOptions = egui::TextureOptions {
    magnification: egui::TextureFilter::Linear,
    minification: egui::TextureFilter::Linear,
    wrap_mode: egui::TextureWrapMode::ClampToEdge,
    mipmap_mode: Some(egui::TextureFilter::Linear),
};

/// What the user did this frame that needs work outside the state lock.
#[derive(Default)]
struct FrameActions {
    /// 重启/启动 clicked for this app.
    run: Option<AppIdentity>,
    /// Favorites added/removed: tray menu must follow.
    tray_dirty: bool,
    /// Trigger background refresh of running apps.
    refresh: bool,
    /// "+" or rescan clicked: kick off installed-app enumeration for this sequence id.
    load_installed: Option<u64>,
    /// Auto-start checkbox toggled: keep tray menu in sync.
    autostart_sync: Option<bool>,
}

impl eframe::App for ReLaunchApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Closing the window hides it to the tray; 退出 in the tray menu quits.
        // Without a tray icon there is no way back, so then close for real.
        if self.tray_ok && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        // Sync visuals only when theme preference changed.
        let dark_mode = self.state.lock().unwrap().config.dark_mode;
        if self.applied_dark_mode != Some(dark_mode) {
            theme::apply_visuals(&ctx, dark_mode);
            self.applied_dark_mode = Some(dark_mode);
        }
        let p = Palette::for_mode(dark_mode);

        let mut actions = FrameActions::default();
        let tray_favorites = {
            let mut state = self.state.lock().unwrap();

            // Upload any icons extracted by background workers to GPU textures.
            let pending: Vec<_> = state.pending_icons.drain(..).collect();
            for (key, icon) in pending {
                if !state.icon_cache.contains_key(&key) {
                    let img = egui::ColorImage::from_rgba_unmultiplied(
                        [icon.width as usize, icon.height as usize],
                        &icon.rgba,
                    );
                    let tex = ctx.load_texture(&key, img, ICON_TEX_OPTIONS);
                    state.icon_cache.insert(key, tex);
                }
            }

            // 1. Top anchored header (title + toolbar + search).
            let header_resp = egui::Panel::top("relaunch_header")
                .frame(
                    egui::Frame::NONE
                        .fill(p.bg_header)
                        .inner_margin(Margin {
                            left: 16,
                            right: 16,
                            top: 12,
                            bottom: 10,
                        }),
                )
                .show_separator_line(false)
                .show_inside(ui, |ui| {
                    render_header(ui, &mut state, &mut actions, &p);
                });
            let hr = header_resp.response.rect;
            ctx.layer_painter(header_resp.response.layer_id).line_segment(
                [
                    egui::pos2(hr.left(), hr.bottom() - 0.5),
                    egui::pos2(hr.right(), hr.bottom() - 0.5),
                ],
                Stroke::new(1.0, p.divider),
            );

            // 2. Bottom anchored status bar.
            let footer_resp = egui::Panel::bottom("relaunch_status_bar")
                .frame(
                    egui::Frame::NONE
                        .fill(p.bg_header)
                        .inner_margin(Margin::symmetric(16, 8)),
                )
                .show_separator_line(false)
                .show_inside(ui, |ui| {
                    render_status_bar(ui, &mut state, &p);
                });
            let fr = footer_resp.response.rect;
            ctx.layer_painter(footer_resp.response.layer_id).line_segment(
                [
                    egui::pos2(fr.left(), fr.top() + 0.5),
                    egui::pos2(fr.right(), fr.top() + 0.5),
                ],
                Stroke::new(1.0, p.divider),
            );

            // 3. Scrollable main body (either main list or embedded add-app page).
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::NONE
                        .fill(p.bg_canvas)
                        .inner_margin(Margin::symmetric(14, 10)),
                )
                .show_inside(ui, |ui| {
                    if state.add_panel.is_some() {
                        render_add_view(ui, &mut state, &mut actions, &p);
                    } else {
                        render_app_list(ui, &mut state, &mut actions, &p);
                    }
                });

            if !state.refreshing
                && state.restart_in_progress.is_none()
                && state
                    .last_refresh
                    .map_or(true, |t| t.elapsed() >= AUTO_REFRESH_INTERVAL)
            {
                actions.refresh = true;
            }
            if state.tray_stale || actions.tray_dirty {
                state.tray_stale = false;
                Some((state.favorite_apps(), state.autostart))
            } else {
                None
            }
        };

        // Outside the lock: workers and the tray both take it themselves.
        if actions.refresh {
            begin_refresh(&self.state, &ctx);
        }
        if let Some(identity) = actions.run {
            begin_operation(&self.state, &ctx, &identity);
        }
        if let Some((favorites, autostart)) = tray_favorites {
            crate::tray::sync_menu(&favorites, autostart);
        } else if let Some(enabled) = actions.autostart_sync {
            crate::tray::sync_autostart(enabled);
        }
        if let Some(seq) = actions.load_installed {
            begin_load_installed(&self.state, &ctx, seq);
        }

        ctx.request_repaint_after(AUTO_REFRESH_INTERVAL);
    }
}

// ---------------------------------------------------------------------------
// Header & search bar
// ---------------------------------------------------------------------------

fn render_header(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut FrameActions,
    p: &Palette,
) {
    let in_add_mode = state.add_panel.is_some();

    if in_add_mode {
        let count_opt = if state.add_panel.as_ref().is_some_and(|panel| !panel.scanning) {
            state.installed_cache.as_ref().map(|a| a.len())
        } else {
            None
        };
        let scanning = state.add_panel.as_ref().is_some_and(|panel| panel.scanning);

        let mut close_panel = false;
        let mut rescan = false;
        ui.horizontal(|ui| {
            let back_btn = pill_back_button(ui, p);
            if back_btn.clicked() {
                close_panel = true;
            }
            ui.add_space(2.0);
            ui.label(theme::bold_text("添加应用", 16.0, p.text_primary));
            if let Some(total) = count_opt {
                count_badge(ui, total, p);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                theme_toggle_button(ui, state, p);

                // Rescan installed apps button.
                let (rect, resp) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
                if resp.hovered() && !scanning {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    ui.painter()
                        .rect_filled(rect, CornerRadius::same(8), p.icon_btn_hover);
                }
                let angle = if scanning {
                    ui.ctx().request_repaint();
                    (ui.input(|i| i.time) * 6.0) as f32
                } else {
                    0.0
                };
                let color = if scanning { p.accent } else { p.text_secondary };
                theme::paint_refresh(ui.painter(), rect.center(), 7.0, angle, color);
                if resp.on_hover_text("重新扫描已安装应用").clicked() && !scanning {
                    rescan = true;
                }
            });
        });

        ui.add_space(10.0);

        let esc_pressed = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if let Some(panel) = state.add_panel.as_mut() {
            render_search_box(ui, &mut panel.search, "搜索已安装应用…", p);
            if esc_pressed {
                if !panel.search.is_empty() {
                    panel.search.clear();
                } else {
                    close_panel = true;
                }
            }
        }
        if rescan {
            state.installed_cache = None;
            state.add_panel_seq += 1;
            let seq = state.add_panel_seq;
            if let Some(panel) = state.add_panel.as_mut() {
                panel.seq = seq;
                panel.scanning = true;
                panel.scan_count = 0;
            }
            actions.load_installed = Some(seq);
        }
        if close_panel {
            state.add_panel = None;
        }
    } else {
        // Ensure ReLaunch brand logo texture is loaded once.
        const LOGO_KEY: &str = "__relaunch_logo__";
        if !state.icon_cache.contains_key(LOGO_KEY) {
            let rgba = crate::icon::rgba(64);
            let img = egui::ColorImage::from_rgba_unmultiplied([64, 64], &rgba);
            let tex = ui
                .ctx()
                .load_texture(LOGO_KEY, img, ICON_TEX_OPTIONS);
            state.icon_cache.insert(LOGO_KEY.to_string(), tex);
        }

        ui.horizontal(|ui| {
            if let Some(tex) = state.icon_cache.get(LOGO_KEY) {
                ui.add(egui::Image::new(tex).fit_to_exact_size(Vec2::splat(22.0)));
            }
            ui.label(theme::bold_text("ReLaunch", 17.0, p.text_primary));

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                theme_toggle_button(ui, state, p);

                // Refresh running apps button.
                let idle = state.restart_in_progress.is_none() && !state.refreshing;
                let spinning = state.manual_refreshing;
                let (rect, resp) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
                if resp.hovered() && (idle || spinning) {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    ui.painter()
                        .rect_filled(rect, CornerRadius::same(8), p.icon_btn_hover);
                }
                let angle = if spinning {
                    ui.ctx().request_repaint();
                    (ui.input(|i| i.time) * 6.0) as f32
                } else {
                    0.0
                };
                let icon_color = if spinning {
                    p.accent
                } else if idle {
                    p.text_secondary
                } else {
                    p.text_muted
                };
                theme::paint_refresh(ui.painter(), rect.center(), 7.0, angle, icon_color);
                if resp.on_hover_text("刷新应用列表").clicked() && idle {
                    state.manual_refreshing = true;
                    actions.refresh = true;
                }

                // "+" Add app button with soft cobalt tint.
                let (add_rect, add_resp) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
                let add_bg = if add_resp.hovered() {
                    p.accent
                } else {
                    p.accent_soft
                };
                let add_fg = if add_resp.hovered() {
                    Color32::WHITE
                } else {
                    p.accent
                };
                ui.painter()
                    .rect_filled(add_rect, CornerRadius::same(8), add_bg);
                if !add_resp.hovered() {
                    ui.painter().rect_stroke(
                        add_rect,
                        CornerRadius::same(8),
                        Stroke::new(1.0, p.accent_soft_border),
                        StrokeKind::Inside,
                    );
                }
                if add_resp.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                }
                theme::paint_plus(ui.painter(), add_rect.center(), 5.5, add_fg);
                if add_resp.on_hover_text("添加应用到收藏").clicked() {
                    if state.installed_cache.is_some() {
                        state.add_panel = Some(AddPanelState {
                            seq: state.add_panel_seq,
                            search: String::new(),
                            scanning: false,
                            scan_count: 0,
                        });
                    } else {
                        state.add_panel_seq += 1;
                        let seq = state.add_panel_seq;
                        state.add_panel = Some(AddPanelState {
                            seq,
                            search: String::new(),
                            scanning: true,
                            scan_count: 0,
                        });
                        actions.load_installed = Some(seq);
                    }
                }

                autostart_toggle_button(ui, state, actions, p);
            });
        });

        ui.add_space(10.0);
        render_search_box(ui, &mut state.search_query, "搜索应用…", p);
        if !state.search_query.is_empty() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            state.search_query.clear();
        }
    }
}

fn autostart_toggle_button(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut FrameActions,
    p: &Palette,
) {
    let enabled = state.autostart;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(76.0, 28.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }

    let (bg, border, dot_color, text_color) = if enabled {
        let stroke = Color32::from_rgba_unmultiplied(
            p.green_dot.r(),
            p.green_dot.g(),
            p.green_dot.b(),
            if resp.hovered() { 140 } else { 90 },
        );
        (p.green_soft, stroke, p.green_dot, p.green_text)
    } else {
        let fill = if resp.hovered() {
            p.icon_btn_hover
        } else {
            p.bg_input
        };
        (fill, p.border, p.stopped_dot, p.text_secondary)
    };

    ui.painter().rect_filled(rect, CornerRadius::same(8), bg);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );

    let dot_center = egui::pos2(rect.left() + 12.0, rect.center().y);
    ui.painter().circle_filled(dot_center, 3.2, dot_color);
    ui.painter().text(
        egui::pos2(rect.left() + 20.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        "开机自启",
        egui::FontId::proportional(12.0),
        text_color,
    );

    let tip = if enabled {
        "开机自启已开启（开机静默启动到系统托盘，点击关闭）"
    } else {
        "点击开启开机自启（开机静默启动到系统托盘）"
    };
    if resp.on_hover_text(tip).clicked() {
        let target = !enabled;
        state.set_autostart(target);
        actions.autostart_sync = Some(state.autostart);
    }
}

fn theme_toggle_button(ui: &mut egui::Ui, state: &mut AppState, p: &Palette) {
    let dark = state.config.dark_mode;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
    let bg = if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        ui.painter()
            .rect_filled(rect, CornerRadius::same(8), p.icon_btn_hover);
        p.icon_btn_hover
    } else {
        p.bg_header
    };
    theme::paint_theme_icon(
        ui.painter(),
        rect.center(),
        7.5,
        dark,
        p.text_secondary,
        bg,
    );
    let tip = if dark { "切换到浅色模式" } else { "切换到深色模式" };
    if resp.on_hover_text(tip).clicked() {
        state.config.dark_mode = !dark;
        if let Err(e) = state.config.save() {
            state.set_status(StatusKind::Error, format!("保存配置失败: {e}"));
        }
    }
}

fn pill_back_button(ui: &mut egui::Ui, p: &Palette) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(66.0, 28.0), Sense::click());
    let bg = if resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        p.icon_btn_hover
    } else {
        p.bg_input
    };
    ui.painter().rect_filled(rect, CornerRadius::same(7), bg);
    let arrow_center = egui::pos2(rect.left() + 16.0, rect.center().y);
    theme::paint_back(ui.painter(), arrow_center, 5.5, p.text_primary);
    ui.painter().text(
        egui::pos2(rect.left() + 28.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        "返回",
        egui::FontId::proportional(13.0),
        p.text_primary,
    );
    resp
}

fn render_search_box(ui: &mut egui::Ui, query: &mut String, hint: &str, p: &Palette) {
    let box_h = 34.0;
    let width = ui.available_width();
    let (box_rect, _) = ui.allocate_exact_size(Vec2::new(width, box_h), Sense::hover());

    // Placeholder background painted behind the embedded TextEdit.
    let bg_idx = ui.painter().add(egui::Shape::Noop);

    let mut inner_rect = box_rect.shrink2(Vec2::new(10.0, 5.0));
    let search_center = egui::pos2(box_rect.left() + 17.0, box_rect.center().y);
    theme::paint_search(ui.painter(), search_center, 5.8, p.text_muted);

    inner_rect.min.x += 20.0;
    if !query.is_empty() {
        inner_rect.max.x -= 22.0;
    }

    let mut edit_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let te_resp = edit_ui.add(
        egui::TextEdit::singleline(query)
            .frame(egui::Frame::NONE)
            .hint_text(egui::RichText::new(hint).color(p.text_muted))
            .desired_width(inner_rect.width()),
    );

    if !query.is_empty() {
        let clear_rect = egui::Rect::from_center_size(
            egui::pos2(box_rect.right() - 16.0, box_rect.center().y),
            Vec2::splat(20.0),
        );
        let clear_resp = ui.interact(clear_rect, ui.id().with(("search_clear", hint)), Sense::click());
        if clear_resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            ui.painter()
                .circle_filled(clear_rect.center(), 9.0, p.icon_btn_hover);
        }
        theme::paint_close(ui.painter(), clear_rect.center(), 3.8, p.text_secondary);
        if clear_resp.on_hover_text("清空搜索").clicked() {
            query.clear();
        }
    }

    let focused = te_resp.has_focus();
    let (fill, stroke) = if focused {
        (p.bg_input_focus, Stroke::new(1.5, p.accent))
    } else if box_rect.contains(ui.input(|i| i.pointer.hover_pos().unwrap_or(egui::Pos2::ZERO))) {
        (p.bg_input, Stroke::new(1.0, p.border_hover))
    } else {
        (p.bg_input, Stroke::new(1.0, p.border))
    };
    ui.painter().set(
        bg_idx,
        egui::Shape::Vec(vec![
            egui::Shape::rect_filled(box_rect, CornerRadius::same(8), fill),
            egui::Shape::rect_stroke(box_rect, CornerRadius::same(8), stroke, StrokeKind::Inside),
        ]),
    );
}

// ---------------------------------------------------------------------------
// Main app list
// ---------------------------------------------------------------------------

fn render_app_list(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut FrameActions,
    p: &Palette,
) {
    let (favorites, running) = state.visible_apps();

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if !favorites.is_empty() {
                section_header(ui, "收藏", favorites.len(), p);
                ui.add_space(6.0);
                for app in &favorites {
                    render_app_row(ui, state, app, actions, p);
                    ui.add_space(6.0);
                }
                if !running.is_empty() {
                    ui.add_space(8.0);
                }
            }

            if !running.is_empty() {
                section_header(ui, "正在运行", running.len(), p);
                ui.add_space(6.0);
                for app in &running {
                    render_app_row(ui, state, app, actions, p);
                    ui.add_space(6.0);
                }
            }

            if favorites.is_empty() && running.is_empty() {
                let msg = if state.search_query.is_empty() {
                    "当前没有正在运行的桌面应用"
                } else {
                    "没有匹配的应用"
                };
                ui.add_space(48.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new(msg).size(13.5).color(p.text_muted));
                });
            }
        });
}

fn section_header(ui: &mut egui::Ui, title: &str, count: usize, p: &Palette) {
    ui.horizontal(|ui| {
        ui.add_space(2.0);
        ui.label(theme::bold_text(title, 12.5, p.text_secondary));
        count_badge(ui, count, p);
    });
}

fn count_badge(ui: &mut egui::Ui, count: usize, p: &Palette) {
    let text = count.to_string();
    let width = (text.len() as f32 * 7.0 + 12.0).max(20.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(9), p.bg_input);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(11.5),
        p.text_secondary,
    );
}

fn render_app_row(
    ui: &mut egui::Ui,
    state: &mut AppState,
    app: &AppInfo,
    actions: &mut FrameActions,
    p: &Palette,
) {
    const ICON_BOX: f32 = 32.0;

    let identity = &app.identity;
    let is_self = state.is_self(&app.pids);
    let busy_any = state.restart_in_progress.is_some();
    let busy_this = state.restart_in_progress.as_ref() == Some(identity);
    let is_favorite = state.config.is_favorite(identity);
    let dark = state.config.dark_mode;

    let cache_key = crate::icon_extract::cache_key(identity, app.exe_path.as_deref());
    if !state.icon_cache.contains_key(&cache_key) {
        if let Some(icon) = &app.icon {
            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                [icon.width as usize, icon.height as usize],
                &icon.rgba,
            );
            let handle = ui
                .ctx()
                .load_texture(&cache_key, color_image, ICON_TEX_OPTIONS);
            state.icon_cache.insert(cache_key.clone(), handle);
        }
    }

    let mut path_status: Option<(StatusKind, String)> = None;
    let bg_idx = ui.painter().add(egui::Shape::Noop);
    let card_resp = egui::Frame::NONE
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            let card_avail_w = ui.available_width();
            ui.set_min_width(card_avail_w);
            let show_path_actions = card_avail_w >= 420.0 && app.exe_path.is_some();
            let right_w = if show_path_actions { 162.0 } else { 104.0 };

            ui.horizontal(|ui| {
                // 1. Icon (or monogram fallback).
                let (icon_rect, _) =
                    ui.allocate_exact_size(Vec2::splat(ICON_BOX), Sense::hover());
                if let Some(tex) = state.icon_cache.get(&cache_key) {
                    let img_rect = egui::Rect::from_center_size(icon_rect.center(), Vec2::splat(28.0));
                    egui::Image::new(tex).paint_at(ui, img_rect);
                } else {
                    theme::paint_monogram(ui.painter(), icon_rect, &app.display_name, dark);
                }

                ui.add_space(4.0);

                // 2. Name + status indicator + directory path (constrained so right controls never clip).
                let text_w = (ui.available_width() - right_w - 8.0).max(60.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(text_w, ICON_BOX + 4.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_max_width(text_w);
                        ui.add(
                            egui::Label::new(theme::bold_text(
                                &app.display_name,
                                14.0,
                                p.text_primary,
                            ))
                            .truncate(),
                        );
                        ui.horizontal(|ui| {
                            let (dot_rect, _) =
                                ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                            let (dot_c, status_text, text_c) = if app.running {
                                (p.green_dot, "运行中", p.green_text)
                            } else {
                                (p.stopped_dot, "未运行", p.text_muted)
                            };
                            ui.painter().circle_filled(dot_rect.center(), 3.2, dot_c);
                            ui.label(egui::RichText::new(status_text).size(12.0).color(text_c));

                            if let Some(exe) = app.exe_path.as_deref() {
                                if let Some(msg) = render_path_segment(ui, exe, p) {
                                    path_status = Some(msg);
                                }
                            }
                        });
                    },
                );

                // 3. Right actions: [copy, folder,] favorite star + restart/start pill.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if is_self {
                        let (r, _) = ui.allocate_exact_size(Vec2::new(64.0, 28.0), Sense::hover());
                        ui.painter().rect_filled(r, CornerRadius::same(7), p.bg_input);
                        ui.painter().text(
                            r.center(),
                            egui::Align2::CENTER_CENTER,
                            "自身",
                            egui::FontId::proportional(12.5),
                            p.text_muted,
                        );
                    } else if busy_this {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(14.0));
                            ui.label(
                                egui::RichText::new("处理中")
                                    .size(12.5)
                                    .color(p.text_secondary),
                            );
                        });
                    } else {
                        let clicked = action_pill_button(ui, app.running, !busy_any, p);
                        if clicked {
                            actions.run = Some(identity.clone());
                        }
                    }

                    // Star button.
                    let (star_rect, star_resp) =
                        ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
                    if star_resp.hovered() {
                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                        ui.painter().rect_filled(
                            star_rect,
                            CornerRadius::same(7),
                            p.icon_btn_hover,
                        );
                    }
                    let star_color = if is_favorite {
                        p.star_active
                    } else if star_resp.hovered() {
                        p.text_secondary
                    } else {
                        p.star_idle
                    };
                    theme::paint_star(
                        ui.painter(),
                        star_rect.center(),
                        7.0,
                        is_favorite,
                        star_color,
                    );
                    let star_tip = if is_favorite { "取消收藏" } else { "收藏" };
                    if star_resp.on_hover_text(star_tip).clicked() {
                        state.toggle_favorite(identity, &app.display_name);
                        actions.tray_dirty = true;
                    }

                    if show_path_actions {
                        if let Some(exe) = app.exe_path.as_deref() {
                            if let Some(msg) = path_action_buttons(ui, exe, p) {
                                path_status = Some(msg);
                            }
                        }
                    }
                });
            });
        });

    if let Some((kind, msg)) = path_status {
        state.set_status(kind, msg);
    }

    let card_rect = card_resp.response.rect;
    let hovered = ui.rect_contains_pointer(card_rect);
    let (fill, stroke) = if hovered {
        (p.bg_card_hover, Stroke::new(1.0, p.border_hover))
    } else {
        (p.bg_card, Stroke::new(1.0, p.border))
    };
    ui.painter().set(
        bg_idx,
        egui::Shape::Vec(vec![
            egui::Shape::rect_filled(card_rect, CornerRadius::same(10), fill),
            egui::Shape::rect_stroke(
                card_rect,
                CornerRadius::same(10),
                stroke,
                StrokeKind::Inside,
            ),
        ]),
    );
}

fn action_pill_button(ui: &mut egui::Ui, running: bool, enabled: bool, p: &Palette) -> bool {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(64.0, 28.0), sense);
    let hovered = enabled && resp.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }

    let (label, fill, border, text_color) = if running {
        if !enabled {
            ("重启", p.bg_input, Some(p.border), p.text_muted)
        } else if hovered {
            ("重启", p.accent, None, Color32::WHITE)
        } else {
            (
                "重启",
                p.accent_soft,
                Some(p.accent_soft_border),
                p.accent_soft_text,
            )
        }
    } else if !enabled {
        ("启动", p.bg_input, Some(p.border), p.text_muted)
    } else if hovered {
        ("启动", p.accent_hover, None, Color32::WHITE)
    } else {
        ("启动", p.accent, None, Color32::WHITE)
    };

    ui.painter().rect_filled(rect, CornerRadius::same(7), fill);
    if let Some(bc) = border {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(7),
            Stroke::new(1.0, bc),
            StrokeKind::Inside,
        );
    }
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::new(13.0, egui::FontFamily::Name("bold".into())),
        text_color,
    );
    enabled && resp.clicked()
}

// ---------------------------------------------------------------------------
// Embedded "Add Application" view
// ---------------------------------------------------------------------------

fn render_add_view(
    ui: &mut egui::Ui,
    state: &mut AppState,
    actions: &mut FrameActions,
    p: &Palette,
) {
    let running_ids: HashSet<AppIdentity> = state
        .apps
        .iter()
        .filter(|a| a.running)
        .map(|a| a.identity.clone())
        .collect();
    let dark = state.config.dark_mode;

    let Some(panel) = state.add_panel.as_ref() else {
        return;
    };

    if panel.scanning || state.installed_cache.is_none() {
        let n = panel.scan_count;
        ui.add_space(60.0);
        ui.vertical_centered(|ui| {
            ui.add(egui::Spinner::new().size(22.0));
            ui.add_space(10.0);
            let msg = if n > 0 {
                format!("正在扫描已安装应用，已发现 {n} 个…")
            } else {
                "正在扫描已安装应用…".to_string()
            };
            ui.label(egui::RichText::new(msg).size(13.5).color(p.text_secondary));
        });
        return;
    }

    let Some(apps) = state.installed_cache.as_ref() else {
        return;
    };

    let query = panel.search.to_lowercase();
    let filtered: Vec<_> = apps
        .iter()
        .filter(|a| query.is_empty() || a.display_name.to_lowercase().contains(&query))
        .collect();

    if filtered.is_empty() {
        ui.add_space(48.0);
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new("没有匹配的已安装应用")
                    .size(13.5)
                    .color(p.text_muted),
            );
        });
        return;
    }

    let mut clicked_toggle: Option<(String, AppIdentity, Option<String>)> = None;
    let mut path_status: Option<(StatusKind, String)> = None;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for app in &filtered {
                let already = state.config.is_favorite(&app.identity);
                let is_running = running_ids.contains(&app.identity);
                let key = crate::icon_extract::cache_key_for(
                    &app.identity,
                    app.exe_path.as_deref(),
                    app.icon_path.as_deref(),
                );
                let cached_tex = state.icon_cache.get(&key).cloned();

                let bg_idx = ui.painter().add(egui::Shape::Noop);
                let card_resp = egui::Frame::NONE
                    .inner_margin(Margin::symmetric(12, 9))
                    .show(ui, |ui| {
                        let card_avail_w = ui.available_width();
                        ui.set_min_width(card_avail_w);
                        let show_path_actions = card_avail_w >= 420.0 && app.exe_path.is_some();
                        let right_w = if show_path_actions { 134.0 } else { 76.0 };

                        ui.horizontal(|ui| {
                            const ICON_BOX: f32 = 28.0;

                            let (icon_rect, _) =
                                ui.allocate_exact_size(Vec2::splat(ICON_BOX), Sense::hover());
                            if let Some(tex) = cached_tex {
                                let img_rect = egui::Rect::from_center_size(
                                    icon_rect.center(),
                                    Vec2::splat(26.0),
                                );
                                egui::Image::new(&tex).paint_at(ui, img_rect);
                            } else {
                                theme::paint_monogram(
                                    ui.painter(),
                                    icon_rect,
                                    &app.display_name,
                                    dark,
                                );
                            }

                            ui.add_space(4.0);

                            let text_w = (ui.available_width() - right_w - 8.0).max(60.0);
                            ui.allocate_ui_with_layout(
                                Vec2::new(text_w, ICON_BOX + 6.0),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    ui.set_max_width(text_w);
                                    ui.add(
                                        egui::Label::new(theme::bold_text(
                                            &app.display_name,
                                            13.5,
                                            p.text_primary,
                                        ))
                                        .truncate(),
                                    );
                                    ui.horizontal(|ui| {
                                        let (tag, tag_bg, tag_fg) = match &app.identity {
                                            AppIdentity::Msix { .. } => {
                                                ("MSIX", p.msix_bg, p.msix_fg)
                                            }
                                            AppIdentity::Win32 { .. } => {
                                                ("Win32", p.win32_bg, p.win32_fg)
                                            }
                                            AppIdentity::Manual { .. } => {
                                                ("手动", p.win32_bg, p.win32_fg)
                                            }
                                        };
                                        tag_pill(ui, tag, tag_bg, tag_fg);
                                        if is_running {
                                            let (dot_r, _) = ui.allocate_exact_size(
                                                Vec2::splat(8.0),
                                                Sense::hover(),
                                            );
                                            ui.painter().circle_filled(
                                                dot_r.center(),
                                                3.0,
                                                p.green_dot,
                                            );
                                            ui.label(
                                                egui::RichText::new("运行中")
                                                    .size(11.5)
                                                    .color(p.green_text),
                                            );
                                        }
                                        if let Some(exe) = app.exe_path.as_deref() {
                                            if let Some(msg) = render_path_segment(ui, exe, p) {
                                                path_status = Some(msg);
                                            }
                                        }
                                    });
                                },
                            );

                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let (btn_rect, btn_resp) = ui.allocate_exact_size(
                                        Vec2::new(72.0, 26.0),
                                        Sense::click(),
                                    );
                                    let hovered = btn_resp.hovered();
                                    if hovered {
                                        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                                    }
                                    let (lbl, fill, border, fg) = if already {
                                        if hovered {
                                            ("取消收藏", p.danger_soft, Some(p.danger), p.danger)
                                        } else {
                                            ("✓ 已收藏", p.green_soft, None, p.green_text)
                                        }
                                    } else if hovered {
                                        ("+ 收藏", p.accent, None, Color32::WHITE)
                                    } else {
                                        (
                                            "+ 收藏",
                                            p.accent_soft,
                                            Some(p.accent_soft_border),
                                            p.accent_soft_text,
                                        )
                                    };
                                    ui.painter().rect_filled(
                                        btn_rect,
                                        CornerRadius::same(7),
                                        fill,
                                    );
                                    if let Some(bc) = border {
                                        ui.painter().rect_stroke(
                                            btn_rect,
                                            CornerRadius::same(7),
                                            Stroke::new(1.0, bc),
                                            StrokeKind::Inside,
                                        );
                                    }
                                    ui.painter().text(
                                        btn_rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        lbl,
                                        egui::FontId::proportional(12.5),
                                        fg,
                                    );
                                    if btn_resp.clicked() {
                                        clicked_toggle = Some((
                                            app.display_name.clone(),
                                            app.identity.clone(),
                                            app.exe_path.clone(),
                                        ));
                                    }

                                    if show_path_actions {
                                        if let Some(exe) = app.exe_path.as_deref() {
                                            if let Some(msg) = path_action_buttons(ui, exe, p) {
                                                path_status = Some(msg);
                                            }
                                        }
                                    }
                                },
                            );
                        });
                    });

                let card_rect = card_resp.response.rect;
                let hovered = ui.rect_contains_pointer(card_rect);
                let (fill, stroke) = if hovered {
                    (p.bg_card_hover, Stroke::new(1.0, p.border_hover))
                } else {
                    (p.bg_card, Stroke::new(1.0, p.border))
                };
                ui.painter().set(
                    bg_idx,
                    egui::Shape::Vec(vec![
                        egui::Shape::rect_filled(card_rect, CornerRadius::same(10), fill),
                        egui::Shape::rect_stroke(
                            card_rect,
                            CornerRadius::same(10),
                            stroke,
                            StrokeKind::Inside,
                        ),
                    ]),
                );
                ui.add_space(6.0);
            }
        });

    if let Some((kind, msg)) = path_status {
        state.set_status(kind, msg);
    }

    if let Some((name, id, exe_path)) = clicked_toggle {
        if state.config.is_favorite(&id) {
            state.config.remove_favorite(&id);
            if let Err(e) = state.config.save() {
                state.set_status(StatusKind::Error, format!("保存配置失败: {e}"));
            } else {
                state.set_status(StatusKind::Info, format!("已将 {name} 移出收藏"));
            }
        } else {
            state.config.add_favorite(crate::config::FavoriteEntry {
                display_name: name.clone(),
                identity: id,
                last_known_path: exe_path,
            });
            if let Err(e) = state.config.save() {
                state.set_status(StatusKind::Error, format!("保存配置失败: {e}"));
            } else {
                state.set_status(StatusKind::Success, format!("已将 {name} 加入收藏"));
            }
        }
        actions.tray_dirty = true;
    }
}

fn parent_dir(exe_path: Option<&str>) -> Option<&str> {
    let p = exe_path?;
    std::path::Path::new(p)
        .parent()
        .and_then(|d| d.to_str())
        .filter(|s| !s.is_empty())
        .or(Some(p))
}

fn open_in_explorer(exe_path: &str) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{exe_path}\""))
        .spawn();
}

/// Render selectable directory path with hover tooltip and right-click context menu.
fn render_path_segment(
    ui: &mut egui::Ui,
    exe_path: &str,
    p: &Palette,
) -> Option<(StatusKind, String)> {
    let dir = parent_dir(Some(exe_path))?;
    ui.label(egui::RichText::new("·").size(11.5).color(p.text_muted));
    let resp = ui
        .add(
            egui::Label::new(egui::RichText::new(dir).size(11.5).color(p.text_muted))
                .truncate()
                .selectable(true),
        )
        .on_hover_text(format!("{dir}\n拖拽选中复制 · 右键打开或复制"));

    let mut status_out: Option<(StatusKind, String)> = None;
    resp.context_menu(|ui| {
        if ui.button("复制目录路径").clicked() {
            ui.ctx().copy_text(dir.to_string());
            status_out = Some((StatusKind::Success, format!("已复制目录: {dir}")));
            ui.close();
        }
        if ui.button("复制程序路径").clicked() {
            ui.ctx().copy_text(exe_path.to_string());
            status_out = Some((StatusKind::Success, format!("已复制路径: {exe_path}")));
            ui.close();
        }
        if ui.button("在资源管理器中打开").clicked() {
            open_in_explorer(exe_path);
            ui.close();
        }
    });
    status_out
}

/// Compact "open folder" and "copy directory" icon buttons (rendered in RTL order).
fn path_action_buttons(
    ui: &mut egui::Ui,
    exe_path: &str,
    p: &Palette,
) -> Option<(StatusKind, String)> {
    let mut status_out: Option<(StatusKind, String)> = None;

    let (folder_rect, folder_resp) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
    if folder_resp.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        ui.painter()
            .rect_filled(folder_rect, CornerRadius::same(7), p.icon_btn_hover);
    }
    let folder_color = if folder_resp.hovered() {
        p.text_primary
    } else {
        p.text_muted
    };
    theme::paint_folder(ui.painter(), folder_rect.center(), 6.2, folder_color);
    if folder_resp.on_hover_text("在资源管理器中打开").clicked() {
        open_in_explorer(exe_path);
    }

    if let Some(dir) = parent_dir(Some(exe_path)) {
        let (copy_rect, copy_resp) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
        if copy_resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            ui.painter()
                .rect_filled(copy_rect, CornerRadius::same(7), p.icon_btn_hover);
        }
        let copy_color = if copy_resp.hovered() {
            p.text_primary
        } else {
            p.text_muted
        };
        theme::paint_copy(ui.painter(), copy_rect.center(), 5.8, copy_color);
        if copy_resp.on_hover_text("复制目录路径").clicked() {
            ui.ctx().copy_text(dir.to_string());
            status_out = Some((StatusKind::Success, format!("已复制目录: {dir}")));
        }
    }

    status_out
}

fn tag_pill(ui: &mut egui::Ui, label: &str, bg: Color32, fg: Color32) {
    let w = (label.len() as f32 * 6.8 + 12.0).max(36.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 17.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(4), bg);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(11.0),
        fg,
    );
}

// ---------------------------------------------------------------------------
// Bottom status bar
// ---------------------------------------------------------------------------

fn render_status_bar(ui: &mut egui::Ui, state: &mut AppState, p: &Palette) {
    let running_count = state.apps.iter().filter(|a| a.running).count();
    let fav_count = state.config.favorites.len();
    let summary = format!("{running_count} 运行中 · {fav_count} 收藏");

    ui.horizontal(|ui| {
        let mut clear_status = false;
        let has_toast = state.status_message.is_some();

        let (dot_color, text_color, text) = match &state.status_message {
            Some(msg) => match msg.kind {
                StatusKind::Success => (p.green_dot, p.text_primary, msg.text.as_str()),
                StatusKind::Error => (p.danger, p.danger, msg.text.as_str()),
                StatusKind::Info => (p.accent, p.text_primary, msg.text.as_str()),
            },
            None => (
                if running_count > 0 { p.green_dot } else { p.stopped_dot },
                p.text_secondary,
                "就绪",
            ),
        };

        let (dot_r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
        ui.painter().circle_filled(dot_r.center(), 3.5, dot_color);

        let right_reserve = summary.len() as f32 * 6.4 + if has_toast { 34.0 } else { 14.0 };
        let max_msg_w = (ui.available_width() - right_reserve).max(60.0);
        ui.allocate_ui_with_layout(
            Vec2::new(max_msg_w, 18.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_max_width(max_msg_w);
                ui.add(
                    egui::Label::new(egui::RichText::new(text).size(12.5).color(text_color))
                        .truncate(),
                );
            },
        );

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(summary)
                    .size(12.0)
                    .color(p.text_secondary),
            );
            if has_toast {
                let (x_rect, x_resp) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
                if x_resp.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    ui.painter()
                        .circle_filled(x_rect.center(), 8.0, p.icon_btn_hover);
                }
                theme::paint_close(ui.painter(), x_rect.center(), 3.2, p.text_muted);
                if x_resp.on_hover_text("清除提示").clicked() {
                    clear_status = true;
                }
            }
        });

        if clear_status {
            state.status_message = None;
        }
    });
}
