//! Run profiles (`.deerpunk/profiles.toml` in the project folder): named
//! launch configurations that drive the pre_run pipeline — build profiles,
//! cargo features and env for the engine process.

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct RunProfile {
    pub label: String,
    /// Cargo profile for the engine build (`debug` / `release`).
    pub engine_profile: String,
    /// Cargo profile for the game lib build.
    pub game_profile: String,
    /// Engine cargo features (full list, e.g. `["full-kira", "puffin"]`).
    pub engine_features: Vec<String>,
    /// Game lib cargo features.
    pub game_features: Vec<String>,
    /// Extra env vars for the engine process.
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Profiles {
    pub order: Vec<String>,
    pub map: BTreeMap<String, RunProfile>,
}

impl Profiles {
    /// Built-in single `dev` profile (mirrors run.sh defaults: debug + puffin).
    pub fn default_profiles() -> Profiles {
        let dev = RunProfile {
            label: "Dev".into(),
            engine_profile: "debug".into(),
            game_profile: "debug".into(),
            engine_features: vec!["full-kira".into(), "puffin".into()],
            game_features: vec!["puffin".into()],
            env: BTreeMap::new(),
        };
        Profiles {
            order: vec!["dev".into()],
            map: BTreeMap::from([("dev".into(), dev)]),
        }
    }

    /// Load `.deerpunk/profiles.toml`; missing file or missing sections
    /// yield the built-in `dev` profile (debug/debug, puffin like run.sh).
    pub fn load(project_root: &Path) -> Profiles {
        let path = project_root.join(".deerpunk/profiles.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default_profiles();
        };
        let Ok(value) = toml::from_str::<toml::Value>(&text) else {
            return Self::default_profiles();
        };
        let Some(table) = value.get("profile").and_then(|v| v.as_table()) else {
            return Self::default_profiles();
        };

        let mut out = Profiles {
            order: Vec::new(),
            map: BTreeMap::new(),
        };
        for (id, value) in table {
            let label = value
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .to_string();
            let engine_profile = value
                .get("engine_profile")
                .and_then(|v| v.as_str())
                .unwrap_or("debug")
                .to_string();
            let game_profile = value
                .get("game_profile")
                .and_then(|v| v.as_str())
                .unwrap_or("debug")
                .to_string();
            let engine_features = strings(value.get("engine_features"));
            let game_features = strings(value.get("game_features"));
            let mut env = BTreeMap::new();
            if let Some(t) = value.get("env").and_then(|v| v.as_table()) {
                for (k, v) in t {
                    if let Some(s) = v.as_str() {
                        env.insert(k.clone(), s.to_string());
                    }
                }
            }
            out.order.push(id.clone());
            out.map.insert(
                id.clone(),
                RunProfile {
                    label,
                    engine_profile,
                    game_profile,
                    engine_features,
                    game_features,
                    env,
                },
            );
        }
        if out.map.is_empty() {
            Self::default_profiles()
        } else {
            out
        }
    }

    pub fn get(&self, id: &str) -> Option<&RunProfile> {
        self.map.get(id)
    }

    pub fn first(&self) -> Option<(&String, &RunProfile)> {
        let id = self.order.first()?;
        self.map.get(id).map(|p| (id, p))
    }
}

impl Default for Profiles {
    fn default() -> Self {
        Self::default_profiles()
    }
}

fn strings(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_root(tag: &str, body: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("studio_profiles_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".deerpunk")).unwrap();
        if let Some(body) = body {
            std::fs::write(dir.join(".deerpunk/profiles.toml"), body).unwrap();
        }
        dir
    }

    #[test]
    fn missing_file_yields_default_dev() {
        let dir = write_root("missing", None);
        let profiles = Profiles::load(&dir);
        assert_eq!(profiles.order, vec!["dev"]);
        let dev = profiles.get("dev").unwrap();
        assert_eq!(dev.engine_profile, "debug");
        assert!(dev.engine_features.contains(&"puffin".into()));
        assert!(dev.engine_features.contains(&"full-kira".into()));
    }

    #[test]
    fn parses_profile_sections() {
        let dir = write_root(
            "full",
            Some(
                "[profile.dev]\nlabel = \"Dev\"\nengine_profile = \"debug\"\ngame_profile = \"debug\"\nengine_features = [\"full-kira\", \"puffin\"]\ngame_features = [\"puffin\"]\n\n[profile.tracy]\nlabel = \"Tracy\"\nengine_features = [\"full-kira\", \"tracy\"]\ngame_features = [\"tracy\"]\n\n[profile.tracy.env]\nTRACY = \"1\"\n",
            ),
        );
        let profiles = Profiles::load(&dir);
        assert_eq!(profiles.order, vec!["dev", "tracy"]);
        let tracy = profiles.get("tracy").unwrap();
        assert_eq!(tracy.label, "Tracy");
        assert!(tracy.engine_features.contains(&"tracy".into()));
        assert!(!tracy.engine_features.contains(&"puffin".into()));
        assert_eq!(tracy.env.get("TRACY").map(String::as_str), Some("1"));
    }

    #[test]
    fn empty_file_yields_default() {
        let dir = write_root("empty", Some("[other]\nx = 1\n"));
        let profiles = Profiles::load(&dir);
        assert_eq!(profiles.order, vec!["dev"]);
    }
}
