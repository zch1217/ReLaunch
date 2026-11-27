use crate::identity::AppIdentity;
use crate::launcher::{launch, resolve_launch_entry};
use crate::process::{terminate_processes, CloseResult};

#[derive(Debug)]
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
    launch(&entry).map_err(|e| RestartError::LaunchFailed(e.to_string()))?;

    // 5. Confirm a new process actually appeared (not one of the killed PIDs).
    if wait_for_new_process(identity, &killed_pids) {
        Ok(())
    } else {
        Err(RestartError::NotDetected)
    }
}

/// Time allowed for the relaunched app to show up. Squirrel-style launchers
/// (Update.exe -> App.exe) need a moment.
const START_DETECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Poll until a process of `identity` exists that is not in `old_pids`.
pub fn wait_for_new_process(identity: &AppIdentity, old_pids: &[u32]) -> bool {
    let deadline = std::time::Instant::now() + START_DETECT_TIMEOUT;
    loop {
        if let Some(app) = crate::discovery::find_by_identity(identity) {
            if app.pids.iter().any(|p| !old_pids.contains(p)) {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
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

/// Start an app that is not running and confirm a process appeared.
pub fn start(identity: &AppIdentity, path_hint: Option<&str>) -> Result<(), RestartError> {
    let entry = resolve_launch_entry(identity, path_hint).ok_or(RestartError::NoLaunchEntry)?;
    launch(&entry).map_err(|e| RestartError::LaunchFailed(e.to_string()))?;
    if wait_for_new_process(identity, &[]) {
        Ok(())
    } else {
        Err(RestartError::NotDetected)
    }
}
