//! System tray icon and menu.
//!
//! Tray and menu events are handled directly in their event handlers, which
//! tray-icon/muda invoke synchronously on the main thread while it pumps
//! window messages. They must NOT depend on egui frame updates: Windows does
//! not repaint a hidden window, so `update()` never runs while ReLaunch sits
//! in the tray.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

use crate::app::{begin_operation, AppState};
use crate::identity::{AppIdentity, AppInfo};

#[derive(Clone)]
enum TrayAction {
    OpenMainWindow,
    Exit,
    /// Restart if running, start if not — decided when the job executes.
    Run(AppIdentity),
}

/// Main-thread-only tray objects (TrayIcon and MenuItem are not Send).
struct TrayUi {
    icon: TrayIcon,
    /// One menu item per favorite, so labels can be updated in place.
    app_items: Vec<(MenuItem, AppIdentity)>,
    /// Favorites the current menu was built for; a change means rebuild.
    built_for: Vec<AppIdentity>,
    last_refresh: Option<Instant>,
}

thread_local! {
    static TRAY: RefCell<Option<TrayUi>> = const { RefCell::new(None) };
}

/// Menu item id -> action. Written and read on the main thread only, but the
/// event handler closure must be Send + Sync, hence the Mutex.
fn actions() -> &'static Mutex<HashMap<String, TrayAction>> {
    static ACTIONS: OnceLock<Mutex<HashMap<String, TrayAction>>> = OnceLock::new();
    ACTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Create the tray icon and hook up its event handlers. Call on the main thread.
/// `hwnd` is the main window, used to show it again while it is hidden.
pub fn install(
    state: Arc<Mutex<AppState>>,
    ctx: egui::Context,
    hwnd: Option<isize>,
) -> anyhow::Result<()> {
    let icon = tray_icon::Icon::from_rgba(crate::icon::rgba(32), 32, 32)?;
    let tray = TrayIconBuilder::new()
        .with_tooltip("ReLaunch")
        .with_icon(icon)
        .with_menu_on_left_click(false)
        .build()?;
    TRAY.with(|t| {
        *t.borrow_mut() = Some(TrayUi {
            icon: tray,
            app_items: Vec::new(),
            built_for: Vec::new(),
            last_refresh: None,
        })
    });
    sync_menu(&state.lock().unwrap().favorite_apps());

    let (menu_state, menu_ctx) = (state.clone(), ctx.clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let action = actions().lock().unwrap().get(&event.id.0).cloned();
        match action {
            Some(TrayAction::OpenMainWindow) => show_main_window(hwnd, &menu_ctx),
            Some(TrayAction::Exit) => exit_app(),
            Some(TrayAction::Run(identity)) => begin_operation(&menu_state, &menu_ctx, &identity),
            None => {}
        }
    }));

    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| match event {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } => show_main_window(hwnd, &ctx),
        // The cursor reached the icon: refresh running state so the menu that
        // may open next says 重启/启动 correctly.
        TrayIconEvent::Enter { .. } => refresh_on_hover(&state),
        _ => {}
    }));
    Ok(())
}

/// Bring the menu in line with `favorites`: rebuild if the set of favorites
/// changed, otherwise just update the 重启/启动 labels in place.
/// Main thread only; a no-op if the tray was not installed.
pub fn sync_menu(favorites: &[AppInfo]) {
    TRAY.with(|t| {
        let mut guard = t.borrow_mut();
        let Some(tray) = guard.as_mut() else { return };
        let ids: Vec<AppIdentity> = favorites.iter().map(|a| a.identity.clone()).collect();
        if ids == tray.built_for {
            for (item, identity) in &tray.app_items {
                if let Some(app) = favorites.iter().find(|a| &a.identity == identity) {
                    item.set_text(label_for(app));
                }
            }
        } else {
            rebuild(tray, favorites);
            tray.built_for = ids;
        }
    });
}

fn rebuild(tray: &mut TrayUi, favorites: &[AppInfo]) {
    let menu = Menu::new();
    let mut acts = actions().lock().unwrap();
    acts.clear();
    tray.app_items.clear();

    let _ = menu.append(&MenuItem::new("ReLaunch", false, None));
    let _ = menu.append(&PredefinedMenuItem::separator());

    if favorites.is_empty() {
        let _ = menu.append(&MenuItem::new("还没有收藏的应用", false, None));
    }
    for app in favorites {
        let item = MenuItem::new(label_for(app), true, None);
        acts.insert(item.id().0.clone(), TrayAction::Run(app.identity.clone()));
        let _ = menu.append(&item);
        tray.app_items.push((item, app.identity.clone()));
    }

    let _ = menu.append(&PredefinedMenuItem::separator());
    let open = MenuItem::new("打开主界面", true, None);
    acts.insert(open.id().0.clone(), TrayAction::OpenMainWindow);
    let _ = menu.append(&open);
    let exit = MenuItem::new("退出", true, None);
    acts.insert(exit.id().0.clone(), TrayAction::Exit);
    let _ = menu.append(&exit);

    tray.icon.set_menu(Some(Box::new(menu)));
}

fn label_for(app: &AppInfo) -> String {
    if app.running {
        format!("↻ 重启 {}", app.display_name)
    } else {
        format!("▶ 启动 {}", app.display_name)
    }
}

/// Re-check which favorites are running, at most every 2 seconds.
fn refresh_on_hover(state: &Arc<Mutex<AppState>>) {
    let due = TRAY.with(|t| {
        let mut guard = t.borrow_mut();
        let Some(tray) = guard.as_mut() else { return false };
        let due = tray
            .last_refresh
            .map_or(true, |at| at.elapsed() >= Duration::from_secs(2));
        if due {
            tray.last_refresh = Some(Instant::now());
        }
        due
    });
    if !due {
        return;
    }
    // Discovery (~0.1 s) runs without holding the state lock.
    let favorites = state.lock().unwrap().favorite_identities();
    let apps = crate::discovery::discover_with_favorites(&favorites);
    let favs = {
        let mut s = state.lock().unwrap();
        s.apply_discovery(apps);
        s.favorite_apps()
    };
    // Labels only: rebuilding would re-enter the tray window procedure that is
    // currently dispatching this event.
    TRAY.with(|t| {
        if let Some(tray) = t.borrow().as_ref() {
            for (item, identity) in &tray.app_items {
                if let Some(app) = favs.iter().find(|a| &a.identity == identity) {
                    item.set_text(label_for(app));
                }
            }
        }
    });
}

/// Show and focus the main window, even while it is hidden in the tray.
pub fn show_main_window(hwnd: Option<isize>, ctx: &egui::Context) {
    if let Some(h) = hwnd {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
        };
        let hwnd = HWND(h as *mut _);
        unsafe {
            let _ = ShowWindow(hwnd, if IsIconic(hwnd).as_bool() { SW_RESTORE } else { SW_SHOW });
            let _ = SetForegroundWindow(hwnd);
        }
    }
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.request_repaint();
}

/// Remove the tray icon (so no ghost icon lingers) and end the process.
/// Hidden rather than dropped: this runs inside the tray window's own message
/// handler, and dropping the TrayIcon would destroy that window mid-dispatch.
fn exit_app() -> ! {
    TRAY.with(|t| {
        if let Some(tray) = t.borrow().as_ref() {
            let _ = tray.icon.set_visible(false);
        }
    });
    std::process::exit(0);
}
