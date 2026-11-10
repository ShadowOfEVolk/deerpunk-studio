//! Persistent studio settings (`studio.toml` in the OS config dir):
//! last opened project + recents list.

use std::path::PathBuf;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StudioConfig {
    pub last_project: Option<String>,
    #[serde(default)]
    pub recents: Vec<String>,
}

impl StudioConfig {
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("deerpunk-studio")
            .join("studio.toml")
    }

    pub fn load() -> StudioConfig {
        match std::fs::read_to_string(Self::path()) {
            Ok(text) => toml::from_str(&text).unwrap_or_default(),
            Err(_) => StudioConfig::default(),
        }
    }

    pub fn save(&self) {
        if let Some(dir) = Self::path().parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = toml::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), text);
        }
    }

    /// Record an opened project: becomes `last_project`, deduped at recents head.
    pub fn record_open(&mut self, path: &str) {
        self.last_project = Some(path.to_string());
        self.recents.retain(|p| p != path);
        self.recents.insert(0, path.to_string());
        self.recents.truncate(10);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_open_dedupes_and_pins_last() {
        let mut cfg = StudioConfig::default();
        cfg.record_open("/games/a");
        cfg.record_open("/games/b");
        cfg.record_open("/games/a");
        assert_eq!(cfg.last_project.as_deref(), Some("/games/a"));
        assert_eq!(cfg.recents, vec!["/games/a", "/games/b"]);
        assert!(cfg.recents.len() <= 10);
    }

    #[test]
    fn toml_roundtrip() {
        let mut cfg = StudioConfig::default();
        cfg.record_open("/games/ohmyblock");
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: StudioConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.last_project.as_deref(), Some("/games/ohmyblock"));
        assert_eq!(back.recents.len(), 1);
    }
}
