//! `deerpunk.toml` — game project settings at the project root.
//!
//! Known sections:
//! ```toml
//! [project]
//! name = "ohmyblock"
//!
//! [engine]
//! gamedir = "./gamedir"   # relative to this file
//!
//! [launch]
//! commands = []
//! ```
//! Unknown keys are preserved as dotted extras so the UI can show them.

use std::path::{Path, PathBuf};

pub type Error = String;

#[derive(Debug, Clone)]
pub struct Project {
    /// Directory containing `deerpunk.toml`.
    pub root: PathBuf,
    /// Raw file contents for the raw view.
    pub raw: String,
    pub name: String,
    /// `[engine].gamedir` resolved to an absolute path (root-joined).
    pub gamedir: PathBuf,
    /// `[engine].path` — engine repo directory, default `../deerpunk`.
    pub engine_dir: PathBuf,
    /// `[launch].gamelibs` — cdylib crates to build and install.
    pub gamelibs: Vec<GameLib>,
    pub launch_commands: Vec<String>,
    /// Unknown `key = value` pairs (section-prefixed), in file order.
    pub extras: Vec<(String, String)>,
}

/// One gamelib built and installed into the gamedir.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GameLib {
    /// Cargo package name, e.g. `ohmyblock-server`.
    pub package: String,
    /// cdylib artifact base name, e.g. `ohmyblock_amd64`.
    pub artifact: String,
    /// Install slot and CLI mapping: `server` | `client`.
    pub slot: String,
}

impl Project {
    /// Load and parse `deerpunk.toml` from `root`.
    pub fn load(root: &Path) -> Result<Project, Error> {
        let toml_path = root.join("deerpunk.toml");
        let raw = std::fs::read_to_string(&toml_path)
            .map_err(|e| format!("cannot read {}: {e}", toml_path.display()))?;
        let table: toml::Value = toml::from_str(&raw)
            .map_err(|e| format!("cannot parse {}: {e}", toml_path.display()))?;

        let name = table
            .get("project")
            .and_then(|p| p.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let gamedir_raw = table
            .get("engine")
            .and_then(|e| e.get("gamedir"))
            .and_then(|v| v.as_str())
            .unwrap_or("./gamedir")
            .to_string();
        let gamedir = resolve_relative(root, &gamedir_raw);

        let engine_raw = table
            .get("engine")
            .and_then(|e| e.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("../deerpunk")
            .to_string();
        let engine_dir = resolve_relative(root, &engine_raw);

        let mut gamelibs = Vec::new();
        if let Some(items) = table
            .get("launch")
            .and_then(|l| l.get("gamelibs"))
            .and_then(|v| v.as_array())
        {
            for item in items {
                match item.clone().try_into::<GameLib>() {
                    Ok(lib) => gamelibs.push(lib),
                    Err(_) => return Err(
                        "deerpunk.toml: bad [launch].gamelibs entry (need package, artifact, slot)"
                            .into(),
                    ),
                }
            }
        }

        let launch_commands = table
            .get("launch")
            .and_then(|l| l.get("commands"))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let mut extras = Vec::new();
        if let Some(t) = table.as_table() {
            for (section, value) in t {
                if matches!(section.as_str(), "project" | "engine" | "launch") {
                    if let Some(inner) = value.as_table() {
                        for (k, v) in inner {
                            let known = (section == "project" && k == "name")
                                || (section == "engine" && k == "gamedir")
                                || (section == "launch" && k == "commands");
                            if !known {
                                push_extras(&mut extras, &format!("{section}.{k}"), v);
                            }
                        }
                    }
                    continue;
                }
                push_extras(&mut extras, section, value);
            }
        }

        Ok(Project {
            root: root.to_path_buf(),
            raw,
            name,
            gamedir,
            engine_dir,
            gamelibs,
            launch_commands,
            extras,
        })
    }

    /// True when `gamedir` exists and contains `gameinfo.txt`.
    pub fn gamedir_valid(&self) -> bool {
        self.gamedir.join("gameinfo.txt").is_file()
    }
}

fn push_extras(out: &mut Vec<(String, String)>, prefix: &str, value: &toml::Value) {
    match value {
        toml::Value::Table(t) => {
            for (k, v) in t {
                push_extras(out, &format!("{prefix}.{k}"), v);
            }
        }
        toml::Value::String(s) => out.push((prefix.to_string(), s.clone())),
        other => out.push((prefix.to_string(), other.to_string())),
    }
}

fn resolve_relative(root: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_root(tag: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("studio_proj_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("gamedir")).unwrap();
        std::fs::write(dir.join("gamedir/gameinfo.txt"), "title \"Test\"\n").unwrap();
        std::fs::write(dir.join("deerpunk.toml"), body).unwrap();
        dir
    }

    #[test]
    fn parses_known_sections() {
        let dir = write_root(
            "full",
            "[project]\nname = \"ohmyblock\"\n\n[engine]\ngamedir = \"./gamedir\"\n\n[launch]\ncommands = []\n",
        );
        let p = Project::load(&dir).unwrap();
        assert_eq!(p.name, "ohmyblock");
        assert_eq!(p.gamedir, dir.join("gamedir"));
        assert!(p.gamedir_valid());
        assert!(p.launch_commands.is_empty());
        assert!(p.extras.is_empty());
    }

    #[test]
    fn missing_sections_get_defaults() {
        let dir = write_root("sparse", "[project]\nname = \"x\"\n");
        let p = Project::load(&dir).unwrap();
        assert_eq!(p.gamedir, dir.join("./gamedir"));
        assert_eq!(p.engine_dir, dir.join("../deerpunk"));
        assert!(p.gamelibs.is_empty());
        assert!(p.launch_commands.is_empty());
    }

    #[test]
    fn parses_gamelibs_and_engine_path() {
        let dir = write_root(
            "gamelibs",
            "[engine]\ngamedir = \"./gamedir\"\npath = \"../deerpunk\"\n\n[launch]\n[[launch.gamelibs]]\npackage = \"ohmyblock-server\"\nartifact = \"ohmyblock_amd64\"\nslot = \"server\"\n\n[[launch.gamelibs]]\npackage = \"ohmyblock-client\"\nartifact = \"client_amd64\"\nslot = \"client\"\n",
        );
        let p = Project::load(&dir).unwrap();
        assert_eq!(p.engine_dir, dir.join("../deerpunk"));
        assert_eq!(p.gamelibs.len(), 2);
        assert_eq!(p.gamelibs[0].package, "ohmyblock-server");
        assert_eq!(p.gamelibs[0].slot, "server");
        assert_eq!(p.gamelibs[1].artifact, "client_amd64");
    }

    #[test]
    fn bad_gamelib_entry_is_error() {
        let dir = write_root(
            "badgamelib",
            "[launch]\n[[launch.gamelibs]]\npackage = \"x\"\n",
        );
        assert!(Project::load(&dir).is_err());
    }

    #[test]
    fn unknown_keys_survive() {
        let dir = write_root(
            "extra",
            "[project]\nname = \"x\"\n\n[tools]\nfoo = \"bar\"\n\n[engine]\ngamedir = \"/tmp\"\neditor = true\n",
        );
        let p = Project::load(&dir).unwrap();
        assert_eq!(p.gamedir, PathBuf::from("/tmp"));
        assert!(p.extras.contains(&("tools.foo".into(), "bar".into())));
        assert!(p.extras.contains(&("engine.editor".into(), "true".into())));
        assert!(!p.extras.iter().any(|(k, _)| k == "engine.gamedir"));
    }

    #[test]
    fn missing_toml_is_error() {
        let dir = std::env::temp_dir().join("studio_proj_absent");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Project::load(&dir).is_err());
    }
}
