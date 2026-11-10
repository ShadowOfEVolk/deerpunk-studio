//! gamedir asset scan: a flat, classified index of every file under the
//! game directory. No file contents are read — kind comes from path/extension.

use std::path::{Path, PathBuf};
use std::time::Instant;
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssetKind {
    GameInfo,
    StartupScript,
    Texture,
    Material,
    Shader,
    Prefab,
    Entity,
    Config,
    Map,
    MapRegion,
    DataPack,
    Sound,
    Ui,
    Font,
    Binary,
    Log,
    Doc,
    Other,
}

impl AssetKind {
    pub fn label(self) -> &'static str {
        match self {
            AssetKind::GameInfo => "Game info",
            AssetKind::StartupScript => "Startup script",
            AssetKind::Texture => "Textures",
            AssetKind::Material => "Materials",
            AssetKind::Shader => "Shaders",
            AssetKind::Prefab => "Prefabs",
            AssetKind::Entity => "Entities",
            AssetKind::Config => "Config",
            AssetKind::Map => "Maps",
            AssetKind::MapRegion => "Map regions",
            AssetKind::DataPack => "DataPack",
            AssetKind::Sound => "Sounds",
            AssetKind::Ui => "UI",
            AssetKind::Font => "Fonts",
            AssetKind::Binary => "Binaries",
            AssetKind::Log => "Logs",
            AssetKind::Doc => "Docs",
            AssetKind::Other => "Other",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssetEntry {
    /// Path relative to gamedir, e.g. `textures/blocks/stone.png`.
    pub rel: PathBuf,
    pub kind: AssetKind,
    pub size: u64,
    /// DataPack id for files under `datapacks/<pack>/…`.
    pub pack: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub entries: Vec<AssetEntry>,
    /// Non-fatal walk errors (unreadable dirs etc.).
    pub errors: Vec<String>,
}

/// Scan a gamedir synchronously (fast: metadata only). Generated `cache/`
/// is skipped; everything else is indexed.
pub fn scan_gamedir(gamedir: &Path) -> ScanResult {
    let mut out = ScanResult::default();
    for entry in WalkDir::new(gamedir)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !(e.depth() == 1 && e.file_name() == "cache"))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                out.errors.push(e.to_string());
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = match entry.path().strip_prefix(gamedir) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let kind = classify(&rel);
        let pack = rel
            .components()
            .nth(1)
            .filter(|_| rel.components().next().map(|c| c.as_os_str()) == Some("datapacks".as_ref()))
            .and_then(|c| c.as_os_str().to_str())
            .map(str::to_string);
        out.entries.push(AssetEntry { rel, kind, size, pack });
    }
    out.entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

fn classify(rel: &Path) -> AssetKind {
    let name = rel
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let comps: Vec<&str> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let top = comps.first().copied().unwrap_or("");
    let depth = comps.len();

    if depth == 1 && name == "gameinfo.txt" {
        return AssetKind::GameInfo;
    }
    if depth == 1 && ext == "rc" {
        return AssetKind::StartupScript;
    }
    if matches!(ext, "png" | "jpg" | "jpeg" | "tga" | "ktx2" | "dds") {
        return AssetKind::Texture;
    }
    if ext == "wgsl" || top == "shaders" {
        return AssetKind::Shader;
    }
    if name.ends_with(".mat.json") {
        return AssetKind::Material;
    }
    if ext == "prefab" {
        return AssetKind::Prefab;
    }
    if top == "entities" {
        return AssetKind::Entity;
    }
    if (top == "config" && ext == "json") || (top == "configs" && ext == "cfg") {
        return AssetKind::Config;
    }
    if top == "maps" {
        if ext == "ohr" {
            return AssetKind::MapRegion;
        }
        return AssetKind::Map;
    }
    if top == "datapacks" {
        if matches!(ext, "ogg" | "wav" | "mp3" | "flac") {
            return AssetKind::Sound;
        }
        return AssetKind::DataPack;
    }
    if matches!(ext, "ogg" | "wav" | "mp3" | "flac") {
        return AssetKind::Sound;
    }
    if top == "ui" {
        return if matches!(ext, "ttf" | "otf") {
            AssetKind::Font
        } else {
            AssetKind::Ui
        };
    }
    if matches!(ext, "ttf" | "otf") {
        return AssetKind::Font;
    }
    if matches!(ext, "dll" | "dylib" | "so") {
        return AssetKind::Binary;
    }
    if top == "logs" {
        return AssetKind::Log;
    }
    if matches!(ext, "md" | "txt") {
        return AssetKind::Doc;
    }
    AssetKind::Other
}

/// Human-readable byte size, `1.4 MB` style.
pub fn fmt_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = size as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Wall-clock duration wrapper for scan timing in the UI status line.
pub fn scan_timed(gamedir: &Path) -> (ScanResult, std::time::Duration) {
    let t = Instant::now();
    (scan_gamedir(gamedir), t.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, rel: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, b"x").unwrap();
    }

    fn setup(tag: &str) -> PathBuf {
        let gamedir = std::env::temp_dir().join(format!("studio_scan_{tag}"));
        let _ = std::fs::remove_dir_all(&gamedir);
        touch(&gamedir, "gameinfo.txt");
        touch(&gamedir, "gamedir.rc");
        touch(&gamedir, "textures/blocks/stone.png");
        touch(&gamedir, "materials/voxel.mat.json");
        touch(&gamedir, "shaders/voxel.wgsl");
        touch(&gamedir, "shaders/inc/common.wgsl");
        touch(&gamedir, "prefabs/player.prefab");
        touch(&gamedir, "entities/chicken/chicken.json");
        touch(&gamedir, "config/cvars_catalog.json");
        touch(&gamedir, "configs/config.cfg");
        touch(&gamedir, "maps/test/level.json");
        touch(&gamedir, "maps/test/regions/r.0.0.ohr");
        touch(&gamedir, "datapacks/ohmyblock/pack.toml");
        touch(&gamedir, "datapacks/ohmyblock/blocks/01_stone.json");
        touch(&gamedir, "datapacks/ohmyblock/blocks/stone.png");
        touch(&gamedir, "datapacks/ohmyblock/sounds/ambient/cave.ogg");
        touch(&gamedir, "ui/fonts/main.ttf");
        touch(&gamedir, "ui/styles/main.json");
        touch(&gamedir, "dlls/client/client.dylib");
        touch(&gamedir, "logs/latest.log");
        touch(&gamedir, "cache/generated.bin");
        touch(&gamedir, "readme.md");
        gamedir
    }

    fn find<'a>(res: &'a ScanResult, rel: &str) -> &'a AssetEntry {
        res.entries
            .iter()
            .find(|e| e.rel == PathBuf::from(rel))
            .unwrap_or_else(|| panic!("missing {rel}"))
    }

    #[test]
    fn classifies_all_kinds() {
        let gamedir = setup("kinds");
        let res = scan_gamedir(&gamedir);
        assert!(res.errors.is_empty());
        let cases: [(&str, AssetKind); 18] = [
            ("gameinfo.txt", AssetKind::GameInfo),
            ("gamedir.rc", AssetKind::StartupScript),
            ("textures/blocks/stone.png", AssetKind::Texture),
            ("materials/voxel.mat.json", AssetKind::Material),
            ("shaders/voxel.wgsl", AssetKind::Shader),
            ("shaders/inc/common.wgsl", AssetKind::Shader),
            ("prefabs/player.prefab", AssetKind::Prefab),
            ("entities/chicken/chicken.json", AssetKind::Entity),
            ("config/cvars_catalog.json", AssetKind::Config),
            ("configs/config.cfg", AssetKind::Config),
            ("maps/test/level.json", AssetKind::Map),
            ("maps/test/regions/r.0.0.ohr", AssetKind::MapRegion),
            ("datapacks/ohmyblock/pack.toml", AssetKind::DataPack),
            ("datapacks/ohmyblock/blocks/01_stone.json", AssetKind::DataPack),
            ("datapacks/ohmyblock/blocks/stone.png", AssetKind::Texture),
            ("datapacks/ohmyblock/sounds/ambient/cave.ogg", AssetKind::Sound),
            ("ui/fonts/main.ttf", AssetKind::Font),
            ("logs/latest.log", AssetKind::Log),
        ];
        for (rel, kind) in cases {
            assert_eq!(find(&res, rel).kind, kind, "kind mismatch for {rel}");
        }
        assert_eq!(find(&res, "dlls/client/client.dylib").kind, AssetKind::Binary);
        assert_eq!(find(&res, "ui/styles/main.json").kind, AssetKind::Ui);
        assert_eq!(find(&res, "readme.md").kind, AssetKind::Doc);
        // cache/ skipped entirely.
        assert!(res.entries.iter().all(|e| !e.rel.starts_with("cache")));
        // datapack tag follows the pack folder.
        assert_eq!(find(&res, "datapacks/ohmyblock/blocks/01_stone.json").pack.as_deref(), Some("ohmyblock"));
        assert_eq!(find(&res, "textures/blocks/stone.png").pack, None);
        let _ = std::fs::remove_dir_all(&gamedir);
    }

    #[test]
    fn sort_is_stable_and_size_fmt() {
        let gamedir = setup("sort");
        let res = scan_gamedir(&gamedir);
        let mut sorted = res.entries.clone();
        sorted.sort_by(|a, b| a.rel.cmp(&b.rel));
        assert_eq!(res.entries, sorted);
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(2048), "2.0 KB");
        assert_eq!(fmt_size(3 * 1024 * 1024 + 100 * 1024), "3.1 MB");
        let _ = std::fs::remove_dir_all(&gamedir);
    }
}
