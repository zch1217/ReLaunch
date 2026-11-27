use std::time::{Duration, Instant};

use crate::identity::AppIdentity;
use crate::launcher::{launch, launch_with_context, resolve_launch_entry, LaunchEntry};
use crate::process::{terminate_processes, CloseResult, InputIdleStatus};

#[derive(Debug, PartialEq, Eq)]
pub enum RestartError {
    /// No valid launch entry found — original process was NOT touched.
    NoLaunchEntry,
    /// OS denied access (target likely running elevated).
    AccessDenied,
    /// Existing processes did not finish exiting within the timeout — launch aborted.
    CloseTimeout,
    /// Launch failed after close succeeded.
    LaunchFailed(String),
    /// Launch command was accepted, but no new process appeared in time.
    NotDetected,
    /// A new process appeared, but exited before completing initialization.
    ExitedEarly,
    /// A new process appeared and stayed alive, but did not finish initializing in time.
    InitTimeout,
    /// Other error.
    Other(String),
}

/// Full restart flow for a single application.
///
/// Invariants:
/// - Validate launch entry BEFORE terminating: if `NoLaunchEntry`, the app is untouched.
/// - Only terminate PIDs freshly discovered for `identity` (never stale caller PIDs).
/// - Confirm all target processes have exited BEFORE launching the new instance.
pub fn restart(identity: &AppIdentity, path_hint: Option<&str>) -> Result<(), RestartError> {
    // 1. Re-discover live processes right before terminating.
    let live = crate::discovery::current_app(identity);
    let hint = live
        .as_ref()
        .and_then(|a| a.exe_path.as_deref())
        .or(path_hint);

    // 2. Resolve launch entry BEFORE killing. No entry -> leave the app alone.
    let entry = resolve_launch_entry(identity, hint).ok_or(RestartError::NoLaunchEntry)?;

    // Capture original args and working directory for direct EXE launches before closing.
    let launch_ctx = if matches!(entry, LaunchEntry::ExePath(_)) {
        live.as_ref().map(|app| {
            crate::process::capture_launch_context(&app.pids, app.exe_path.as_deref())
        })
    } else {
        None
    };

    // 3. Terminate every live process of the app and confirm full exit.
    let mut killed_pids = Vec::new();
    let mut current = live;
    for _pass in 0..2 {
        let Some(app) = current.take() else {
            break;
        };
        if app.pids.is_empty() {
            break;
        }
        match terminate_processes(identity, &app.pids) {
            CloseResult::Exited => {
                killed_pids.extend(app.pids);
            }
            CloseResult::AccessDenied => return Err(RestartError::AccessDenied),
            CloseResult::Timeout => return Err(RestartError::CloseTimeout),
            CloseResult::Error(e) => return Err(RestartError::Other(e)),
        }
        current = crate::discovery::find_by_identity(identity);
    }
    if current.is_some_and(|app| !app.pids.is_empty()) {
        return Err(RestartError::CloseTimeout);
    }

    // 4. Launch the entry validated in step 2.
    launch_with_context(&entry, launch_ctx.as_ref())
        .map_err(|e| RestartError::LaunchFailed(e.to_string()))?;

    // 5. Confirm a new process appeared AND completed initialization.
    wait_for_app_ready(identity, &killed_pids)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ReadyConfig {
    pub appear_timeout: Duration,
    pub total_timeout: Duration,
    pub poll_interval: Duration,
    pub visible_stability: Duration,
    pub background_stability: Duration,
    pub reappear_grace: Duration,
}

impl Default for ReadyConfig {
    fn default() -> Self {
        Self {
            appear_timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(15),
            poll_interval: Duration::from_millis(200),
            visible_stability: Duration::from_millis(300),
            background_stability: Duration::from_millis(1200),
            reappear_grace: Duration::from_millis(1500),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ReadySample {
    alive_new_pids_non_empty: bool,
    responsive_visible_window: bool,
    responsive_any_window: bool,
    input_idle: InputIdleStatus,
    is_gui_app: bool,
}

struct ReadyTracker {
    cfg: ReadyConfig,
    appear_deadline: Instant,
    total_deadline: Instant,
    seen_any: bool,
    last_alive_at: Option<Instant>,
    visible_ready_since: Option<Instant>,
    bg_ready_since: Option<Instant>,
    nongui_alive_since: Option<Instant>,
}

impl ReadyTracker {
    fn new(started_at: Instant, cfg: ReadyConfig) -> Self {
        Self {
            cfg,
            appear_deadline: started_at + cfg.appear_timeout,
            total_deadline: started_at + cfg.total_timeout,
            seen_any: false,
            last_alive_at: None,
            visible_ready_since: None,
            bg_ready_since: None,
            nongui_alive_since: None,
        }
    }

    fn step(&mut self, now: Instant, sample: ReadySample) -> Option<Result<(), RestartError>> {
        if !self.seen_any {
            if !sample.alive_new_pids_non_empty {
                if now >= self.appear_deadline {
                    return Some(Err(RestartError::NotDetected));
                }
                return None;
            }
            self.seen_any = true;
            self.last_alive_at = Some(now);
        } else if !sample.alive_new_pids_non_empty {
            self.visible_ready_since = None;
            self.bg_ready_since = None;
            self.nongui_alive_since = None;
            let last = self.last_alive_at.unwrap_or(now);
            if now.saturating_duration_since(last) >= self.cfg.reappear_grace
                || now >= self.total_deadline
            {
                return Some(Err(RestartError::ExitedEarly));
            }
            return None;
        }

        self.last_alive_at = Some(now);

        // Fast path: responsive visible desktop window sustained briefly.
        if sample.responsive_visible_window {
            let since = *self.visible_ready_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= self.cfg.visible_stability {
                return Some(Ok(()));
            }
        } else {
            self.visible_ready_since = None;
        }

        // Background / tray / UWP GUI path: responsive hidden window or WaitForInputIdle(0).
        let bg_gui_ready =
            sample.responsive_any_window || sample.input_idle == InputIdleStatus::GuiIdle;
        if bg_gui_ready {
            let since = *self.bg_ready_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= self.cfg.background_stability {
                return Some(Ok(()));
            }
        } else {
            self.bg_ready_since = None;
        }

        // Console / non-GUI path: only when the PE itself is not a GUI subsystem binary.
        if !sample.is_gui_app && sample.input_idle == InputIdleStatus::NonGuiOnly {
            let since = *self.nongui_alive_since.get_or_insert(now);
            if now.saturating_duration_since(since) >= self.cfg.background_stability {
                return Some(Ok(()));
            }
        } else {
            self.nongui_alive_since = None;
        }

        if now >= self.total_deadline {
            Some(Err(RestartError::InitTimeout))
        } else {
            None
        }
    }
}

/// Poll until a new process of `identity` appears and completes initialization.
pub fn wait_for_app_ready(identity: &AppIdentity, old_pids: &[u32]) -> Result<(), RestartError> {
    wait_for_app_ready_with_config(identity, old_pids, ReadyConfig::default())
}

pub(crate) fn wait_for_app_ready_with_config(
    identity: &AppIdentity,
    old_pids: &[u32],
    cfg: ReadyConfig,
) -> Result<(), RestartError> {
    let mut tracker = ReadyTracker::new(Instant::now(), cfg);
    loop {
        let sample = sample_app_readiness(identity, old_pids);
        if let Some(outcome) = tracker.step(Instant::now(), sample) {
            return outcome;
        }
        std::thread::sleep(cfg.poll_interval);
    }
}

fn sample_app_readiness(identity: &AppIdentity, old_pids: &[u32]) -> ReadySample {
    let Some(app) = crate::discovery::find_by_identity(identity) else {
        return ReadySample {
            alive_new_pids_non_empty: false,
            responsive_visible_window: false,
            responsive_any_window: false,
            input_idle: InputIdleStatus::StillInitializing,
            is_gui_app: true,
        };
    };

    let new_pids: Vec<u32> = app
        .pids
        .into_iter()
        .filter(|p| !old_pids.contains(p) && crate::process::is_process_alive(*p))
        .collect();
    if new_pids.is_empty() {
        return ReadySample {
            alive_new_pids_non_empty: false,
            responsive_visible_window: false,
            responsive_any_window: false,
            input_idle: InputIdleStatus::StillInitializing,
            is_gui_app: true,
        };
    }

    let root_new_pids = crate::discovery::select_root_pids(&new_pids);
    let responsive_visible_window = crate::discovery::has_responsive_visible_window(&new_pids);
    let responsive_any_window = if responsive_visible_window {
        true
    } else {
        crate::discovery::has_responsive_any_window(&new_pids)
    };
    let input_idle = crate::process::probe_input_idle(&root_new_pids);
    let is_gui_app = identity.is_msix()
        || app
            .exe_path
            .as_deref()
            .and_then(crate::discovery::is_gui_subsystem)
            .unwrap_or(true);

    ReadySample {
        alive_new_pids_non_empty: true,
        responsive_visible_window,
        responsive_any_window,
        input_idle,
        is_gui_app,
    }
}

/// What `run` ended up doing.
#[derive(Debug, Clone, Copy)]
pub enum Done {
    Restarted,
    Started,
}

/// Restart the app if it is running right now, otherwise start it.
///
/// The decision is made here, at execution time, not from whatever the UI or
/// tray menu showed when it was built — a tray menu that says "启动" for an app
/// that has since started must still do the right thing.
pub fn run(identity: &AppIdentity, path_hint: Option<&str>) -> Result<Done, RestartError> {
    if crate::discovery::current_app(identity).is_some() {
        restart(identity, path_hint).map(|_| Done::Restarted)
    } else {
        start(identity, path_hint).map(|_| Done::Started)
    }
}

/// Start an app that is not running and confirm it completes initialization.
pub fn start(identity: &AppIdentity, path_hint: Option<&str>) -> Result<(), RestartError> {
    let entry = resolve_launch_entry(identity, path_hint).ok_or(RestartError::NoLaunchEntry)?;
    launch(&entry).map_err(|e| RestartError::LaunchFailed(e.to_string()))?;
    wait_for_app_ready(identity, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn test_cfg() -> ReadyConfig {
        ReadyConfig {
            appear_timeout: Duration::from_millis(500),
            total_timeout: Duration::from_millis(2000),
            poll_interval: Duration::from_millis(50),
            visible_stability: Duration::from_millis(200),
            background_stability: Duration::from_millis(600),
            reappear_grace: Duration::from_millis(400),
        }
    }

    #[test]
    fn ready_tracker_not_detected_when_never_appears() {
        let t0 = Instant::now();
        let mut tracker = ReadyTracker::new(t0, test_cfg());
        let empty = ReadySample {
            alive_new_pids_non_empty: false,
            responsive_visible_window: false,
            responsive_any_window: false,
            input_idle: InputIdleStatus::StillInitializing,
            is_gui_app: true,
        };
        assert_eq!(tracker.step(t0 + Duration::from_millis(200), empty), None);
        assert_eq!(
            tracker.step(t0 + Duration::from_millis(500), empty),
            Some(Err(RestartError::NotDetected))
        );
    }

    #[test]
    fn ready_tracker_exited_early_before_init_and_during_stability() {
        let t0 = Instant::now();
        let mut tracker = ReadyTracker::new(t0, test_cfg());
        let alive_unready = ReadySample {
            alive_new_pids_non_empty: true,
            responsive_visible_window: false,
            responsive_any_window: false,
            input_idle: InputIdleStatus::StillInitializing,
            is_gui_app: true,
        };
        let gone = ReadySample {
            alive_new_pids_non_empty: false,
            responsive_visible_window: false,
            responsive_any_window: false,
            input_idle: InputIdleStatus::StillInitializing,
            is_gui_app: true,
        };
        assert_eq!(
            tracker.step(t0 + Duration::from_millis(100), alive_unready),
            None
        );
        assert_eq!(tracker.step(t0 + Duration::from_millis(200), gone), None);
        assert_eq!(
            tracker.step(t0 + Duration::from_millis(500), gone),
            Some(Err(RestartError::ExitedEarly))
        );

        // Crash during visible stability window must not report Ok(())
        let mut tracker2 = ReadyTracker::new(t0, test_cfg());
        let visible = ReadySample {
            alive_new_pids_non_empty: true,
            responsive_visible_window: true,
            responsive_any_window: true,
            input_idle: InputIdleStatus::GuiIdle,
            is_gui_app: true,
        };
        assert_eq!(tracker2.step(t0 + Duration::from_millis(100), visible), None);
        assert_eq!(tracker2.step(t0 + Duration::from_millis(150), gone), None);
        assert_eq!(
            tracker2.step(t0 + Duration::from_millis(550), gone),
            Some(Err(RestartError::ExitedEarly))
        );
    }

    #[test]
    fn ready_tracker_bootstrapper_handoff_succeeds() {
        let t0 = Instant::now();
        let mut tracker = ReadyTracker::new(t0, test_cfg());
        let bootstrap = ReadySample {
            alive_new_pids_non_empty: true,
            responsive_visible_window: false,
            responsive_any_window: false,
            input_idle: InputIdleStatus::StillInitializing,
            is_gui_app: true,
        };
        let gone = ReadySample {
            alive_new_pids_non_empty: false,
            responsive_visible_window: false,
            responsive_any_window: false,
            input_idle: InputIdleStatus::StillInitializing,
            is_gui_app: true,
        };
        let real_app = ReadySample {
            alive_new_pids_non_empty: true,
            responsive_visible_window: true,
            responsive_any_window: true,
            input_idle: InputIdleStatus::GuiIdle,
            is_gui_app: true,
        };

        assert_eq!(tracker.step(t0 + Duration::from_millis(50), bootstrap), None);
        assert_eq!(tracker.step(t0 + Duration::from_millis(100), gone), None);
        assert_eq!(tracker.step(t0 + Duration::from_millis(250), real_app), None);
        assert_eq!(
            tracker.step(t0 + Duration::from_millis(450), real_app),
            Some(Ok(()))
        );
    }

    #[test]
    fn ready_tracker_init_timeout_for_hung_gui_or_unready_nongui_gui_binary() {
        let t0 = Instant::now();
        let mut tracker = ReadyTracker::new(t0, test_cfg());
        let hung = ReadySample {
            alive_new_pids_non_empty: true,
            responsive_visible_window: false,
            responsive_any_window: false,
            input_idle: InputIdleStatus::StillInitializing,
            is_gui_app: true,
        };
        assert_eq!(tracker.step(t0 + Duration::from_millis(100), hung), None);
        assert_eq!(
            tracker.step(t0 + Duration::from_millis(2000), hung),
            Some(Err(RestartError::InitTimeout))
        );

        // GUI binary reporting NonGuiOnly before creating its message queue must NOT succeed.
        let mut gui_as_nongui = ReadyTracker::new(t0, test_cfg());
        let early_gui = ReadySample {
            alive_new_pids_non_empty: true,
            responsive_visible_window: false,
            responsive_any_window: false,
            input_idle: InputIdleStatus::NonGuiOnly,
            is_gui_app: true,
        };
        assert_eq!(
            gui_as_nongui.step(t0 + Duration::from_millis(100), early_gui),
            None
        );
        assert_eq!(
            gui_as_nongui.step(t0 + Duration::from_millis(900), early_gui),
            None
        );
        assert_eq!(
            gui_as_nongui.step(t0 + Duration::from_millis(2000), early_gui),
            Some(Err(RestartError::InitTimeout))
        );
    }

    #[test]
    fn ready_tracker_background_gui_and_cui_paths_succeed() {
        let t0 = Instant::now();
        let mut tray_tracker = ReadyTracker::new(t0, test_cfg());
        let tray_app = ReadySample {
            alive_new_pids_non_empty: true,
            responsive_visible_window: false,
            responsive_any_window: false,
            input_idle: InputIdleStatus::GuiIdle,
            is_gui_app: true,
        };
        assert_eq!(
            tray_tracker.step(t0 + Duration::from_millis(100), tray_app),
            None
        );
        assert_eq!(
            tray_tracker.step(t0 + Duration::from_millis(400), tray_app),
            None
        );
        assert_eq!(
            tray_tracker.step(t0 + Duration::from_millis(700), tray_app),
            Some(Ok(()))
        );

        let mut cui_tracker = ReadyTracker::new(t0, test_cfg());
        let cui_app = ReadySample {
            alive_new_pids_non_empty: true,
            responsive_visible_window: false,
            responsive_any_window: false,
            input_idle: InputIdleStatus::NonGuiOnly,
            is_gui_app: false,
        };
        assert_eq!(
            cui_tracker.step(t0 + Duration::from_millis(100), cui_app),
            None
        );
        assert_eq!(
            cui_tracker.step(t0 + Duration::from_millis(700), cui_app),
            Some(Ok(()))
        );
    }

    struct TempExeChild {
        child: Option<Child>,
        exe_copy: std::path::PathBuf,
    }

    impl Drop for TempExeChild {
        fn drop(&mut self) {
            if let Some(mut c) = self.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            let _ = std::fs::remove_file(&self.exe_copy);
        }
    }

    #[test]
    fn wait_for_app_ready_live_console_process_and_early_exit() {
        let src_cmd =
            std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into());
        let exe_copy = std::env::temp_dir()
            .join(format!("relaunch_ready_test_{}.exe", std::process::id()));
        std::fs::copy(&src_cmd, &exe_copy).expect("copy cmd.exe");
        let id = AppIdentity::Manual {
            path: exe_copy.to_string_lossy().into_owned(),
        };

        let cfg = ReadyConfig {
            appear_timeout: Duration::from_millis(500),
            total_timeout: Duration::from_millis(2000),
            poll_interval: Duration::from_millis(50),
            visible_stability: Duration::from_millis(150),
            background_stability: Duration::from_millis(250),
            reappear_grace: Duration::from_millis(200),
        };

        // Case 1: short-lived process exits immediately -> ExitedEarly or NotDetected.
        let mut short_child = Command::new(&exe_copy)
            .args(["/C", "exit", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn short cmd");
        let short_res = wait_for_app_ready_with_config(&id, &[], cfg);
        let _ = short_child.wait();
        assert!(
            matches!(
                short_res,
                Err(RestartError::ExitedEarly) | Err(RestartError::NotDetected)
            ),
            "unexpected result for short-lived process: {short_res:?}"
        );

        // Case 2: live console process stays alive -> Ok(()).
        let live_child = Command::new(&exe_copy)
            .args(["/C", "pause"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("spawn live cmd");
        let _guard = TempExeChild {
            child: Some(live_child),
            exe_copy: exe_copy.clone(),
        };
        let live_res = wait_for_app_ready_with_config(&id, &[], cfg);
        assert_eq!(live_res, Ok(()));
    }
}
