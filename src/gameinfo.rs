//! `gameinfo.txt` parsing — mirrors deerpunk `engine-host/gameinfo.rs` so the
//! studio shows exactly what the engine would load: `key value` / `key "value"`
//! lines, `//` comments, first two tokens per line, keys lowercased.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Parsed game descriptor. `gamefolder` is always the gamedir directory name.
#[derive(Debug, Clone, Default)]
pub struct GameInfo {
    pub gamefolder: String,
    pub basedir: String,
    pub title: String,
    /// Relative to gamedir (e.g. `game.ico`).
    pub icon: String,
    pub startmap: String,
    pub trainmap: String,
    pub gamedll: String,
    pub version: String,
    pub extras: HashMap<String, String>,
}

impl GameInfo {
    /// Parse `gameinfo.txt` inside `gamedir`. Missing file or parse failure
    /// yields folder-name defaults (same as the engine), never an error.
    pub fn from_gamedir(gamedir: &Path) -> Self {
        let gamefolder = gamedir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("game")
            .to_string();

        let mut info = Self::defaults(&gamefolder);

        let gi = gamedir.join("gameinfo.txt");
        let Ok(text) = std::fs::read_to_string(&gi) else {
            return info;
        };
        apply_kv(&mut info, &parse_kv(&text));
        info
    }

    /// Window title: `title`, or `gamefolder` if empty.
    pub fn window_title(&self) -> &str {
        if self.title.is_empty() {
            &self.gamefolder
        } else {
            &self.title
        }
    }

    /// Absolute icon path under `gamedir`, if the file exists.
    pub fn icon_path(&self, gamedir: &Path) -> Option<PathBuf> {
        let rel = if self.icon.is_empty() {
            "game.ico"
        } else {
            self.icon.as_str()
        };
        let path = gamedir.join(rel);
        path.is_file().then_some(path)
    }

    fn defaults(gamefolder: &str) -> Self {
        Self {
            gamefolder: gamefolder.to_string(),
            basedir: gamefolder.to_string(),
            title: gamefolder.to_string(),
            icon: "game.ico".into(),
            ..Default::default()
        }
    }
}

fn apply_kv(info: &mut GameInfo, kv: &HashMap<String, String>) {
    // Folder name wins for gamefolder (Xash FWGS convention).
    let folder = info.gamefolder.clone();
    for (k, v) in kv {
        match k.as_str() {
            "basedir" => info.basedir = v.clone(),
            "title" => info.title = v.clone(),
            "icon" => info.icon = v.clone(),
            "startmap" => info.startmap = v.clone(),
            "trainmap" => info.trainmap = v.clone(),
            "gamedll" => info.gamedll = v.clone(),
            "version" => info.version = v.clone(),
            // Ignored by the engine — directory name wins.
            "gamedir" => {}
            _ => {
                info.extras.insert(k.clone(), v.clone());
            }
        }
    }
    info.gamefolder = folder;
}

/// Parse Xash gameinfo lines: `key value` / `key "value"`, `//` comments.
pub fn parse_kv(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        let tokens = tokenize(line);
        if tokens.len() < 2 {
            continue;
        }
        out.insert(tokens[0].to_ascii_lowercase(), tokens[1].clone());
    }
    out
}

fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let c = bytes[i] as char;
        if c == '"' {
            in_quotes = !in_quotes;
            i += 1;
            continue;
        }
        if !in_quotes && c == '/' && bytes[i + 1] == b'/' {
            return &line[..i];
        }
        i += 1;
    }
    line
}

fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if in_quotes {
                    tokens.push(std::mem::take(&mut current));
                    in_quotes = false;
                } else {
                    in_quotes = true;
                }
            }
            ' ' | '\t' if !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            '\\' if in_quotes => {
                if let Some(n) = chars.next() {
                    current.push(n);
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() || in_quotes {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quoted_and_comments() {
        let kv = parse_kv(
            r#"
// generated
basedir		"valve"
title		"Dark Demo" // trailing
version		1
gamedir		"ignored"
"#,
        );
        assert_eq!(kv.get("basedir").map(String::as_str), Some("valve"));
        assert_eq!(kv.get("title").map(String::as_str), Some("Dark Demo"));
        assert_eq!(kv.get("version").map(String::as_str), Some("1"));
        assert_eq!(kv.get("gamedir").map(String::as_str), Some("ignored"));
        assert_eq!(kv.get("title").map(String::as_str), Some("Dark Demo"));
    }

    #[test]
    fn unknown_keys_go_to_extras() {
        let kv = parse_kv("title \"T\"\ndllpath \"dlls/client\"\n");
        assert_eq!(kv.get("dllpath").map(String::as_str), Some("dlls/client"));
    }

    #[test]
    fn gamefolder_from_path_not_file() {
        let dir = std::env::temp_dir().join("studio_gi_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("gameinfo.txt"), "gamedir \"other\"\ntitle \"T\"\n").unwrap();
        let info = GameInfo::from_gamedir(&dir);
        assert_eq!(info.gamefolder, "studio_gi_test");
        assert_eq!(info.title, "T");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = std::env::temp_dir().join("studio_gi_missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let info = GameInfo::from_gamedir(&dir);
        assert_eq!(info.title, "studio_gi_missing");
        assert_eq!(info.icon, "game.ico");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
