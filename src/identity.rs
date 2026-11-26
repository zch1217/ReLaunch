use serde::{Deserialize, Serialize};

/// Stable identity for an application — survives version/path changes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppIdentity {
    /// MSIX / packaged app identified by Application User Model ID.
    Msix { aumid: String },
    /// Classic Win32 EXE identified by filename + product name from version resource.
    Win32 {
        exe_name: String,   // lowercase, e.g. "notepad++.exe"
        product_name: String, // from VS_VERSION_INFO ProductName
    },
    /// Fallback: user-specified absolute path.
    Manual { path: String },
}

impl AppIdentity {
    pub fn display_name(&self) -> &str {
        match self {
            AppIdentity::Msix { aumid } => aumid.as_str(),
            AppIdentity::Win32 { product_name, .. } => product_name.as_str(),
            AppIdentity::Manual { path } => path.as_str(),
        }
    }

    pub fn is_msix(&self) -> bool {
        matches!(self, AppIdentity::Msix { .. })
    }
}

/// Runtime information about a discovered application.
#[derive(Debug, Clone)]
pub struct AppInfo {
    pub identity: AppIdentity,
    /// User-visible display name (from identity or window title).
    pub display_name: String,
    /// Current executable path (resolved at discovery time, not persisted).
    pub exe_path: Option<String>,
    /// Whether the app is currently running.
    pub running: bool,
    /// All PIDs belonging to this application's process tree.
    pub pids: Vec<u32>,
    /// Icon data as RGBA bytes (width × height × 4), 32×32 pixels.
    pub icon: Option<Vec<u8>>,
}

impl AppInfo {
    pub fn new(identity: AppIdentity, display_name: String) -> Self {
        Self {
            identity,
            display_name,
            exe_path: None,
            running: false,
            pids: Vec::new(),
            icon: None,
        }
    }
}
