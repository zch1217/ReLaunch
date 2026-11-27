use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use crate::identity::AppIdentity;

/// A favorited application entry. The identity is the stable anchor;
/// last_known_path is a fallback hint only and is never used as the primary key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FavoriteEntry {
    pub display_name: String,
    pub identity: AppIdentity,
    /// Fallback hint — updated at runtime but never used as a launch source unless
    /// all other resolution paths fail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_known_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub favorites: Vec<FavoriteEntry>,
    #[serde(default)]
    pub dark_mode: bool,
}

impl Config {
    pub fn config_path() -> PathBuf {
        let base = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("ReLaunch").join("config.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Atomic write: temp file → rename.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn add_favorite(&mut self, entry: FavoriteEntry) {
        // Avoid duplicates by identity.
        if !self.favorites.iter().any(|f| f.identity == entry.identity) {
            self.favorites.push(entry);
        }
    }

    pub fn remove_favorite(&mut self, identity: &AppIdentity) {
        self.favorites.retain(|f| &f.identity != identity);
    }

    pub fn is_favorite(&self, identity: &AppIdentity) -> bool {
        self.favorites.iter().any(|f| &f.identity == identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn favorites_add_dedup_remove_and_json_roundtrip() {
        let mut cfg = Config::default();
        let msix = AppIdentity::Msix {
            aumid: "OpenAI.Codex_2p2nqsd0c76g0!App".to_string(),
        };
        let win32 = AppIdentity::Win32 {
            exe_name: "code.exe".to_string(),
            product_name: "Visual Studio Code".to_string(),
        };

        cfg.add_favorite(FavoriteEntry {
            display_name: "Codex".to_string(),
            identity: msix.clone(),
            last_known_path: None,
        });
        cfg.add_favorite(FavoriteEntry {
            display_name: "Codex Duplicate".to_string(),
            identity: msix.clone(),
            last_known_path: Some(r"C:\Dummy\codex.exe".to_string()),
        });
        cfg.add_favorite(FavoriteEntry {
            display_name: "VS Code".to_string(),
            identity: win32.clone(),
            last_known_path: Some(r"C:\Program Files\Microsoft VS Code\Code.exe".to_string()),
        });
        cfg.dark_mode = true;

        assert_eq!(cfg.favorites.len(), 2);
        assert!(cfg.is_favorite(&msix));
        assert!(cfg.is_favorite(&win32));

        let json = serde_json::to_string_pretty(&cfg).expect("serialize config");
        let parsed: Config = serde_json::from_str(&json).expect("deserialize config");
        assert!(parsed.dark_mode);
        assert_eq!(parsed.favorites.len(), 2);
        assert_eq!(parsed.favorites[0].display_name, "Codex");
        assert_eq!(parsed.favorites[1].identity, win32);

        cfg.remove_favorite(&msix);
        assert!(!cfg.is_favorite(&msix));
        assert_eq!(cfg.favorites.len(), 1);
    }
}
