//! Headless RmlUi widget tree dumper: load one `.rml`, inject data-model
//! state, settle the layout and print the post-layout component tree as
//! text — the `shot` pipeline (`widgetshot.rs`) with a document-tree dump
//! instead of rasterization, for checking layout/overflow without a PNG.

use std::path::PathBuf;
use std::time::Duration;

use deerpunk_ui::{UiDirAssets, UiRuntime, drain_commands};

use crate::widgetshot::{base_for, inject_state_file, value};

const USAGE: &str = "\
usage: deerpunk-studio tree <file.rml> [options]

  <file.rml>       widget document (base = its nearest `ui` ancestor dir)
  --state <toml>   flat TOML table injected into the document's data model
  --max-depth <N>  nesting levels to print (default: unlimited)
  -o, --out <file> write the tree to a file (default: stdout)";

/// CLI entry: `deerpunk-studio tree <file.rml> [--state state.toml]
/// [--max-depth N] [-o out]`. Returns the process exit code (0 ok, 1 on a
/// usage or runtime error, with the reason on stderr).
pub fn run(args: &[String]) -> i32 {
    let opts = match parse_args(args) {
        Ok(opts) => opts,
        Err(reason) => {
            eprintln!("tree: {reason}\n{USAGE}");
            return 1;
        }
    };

    let base = base_for(&opts.rml);
    let rel = opts
        .rml
        .strip_prefix(&base)
        .map(|rel| {
            rel.components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect::<Vec<_>>()
                .join("/")
        })
        .unwrap_or_default();

    // Same headless runtime contract as `shot` (widgetshot.rs): RmlUi is
    // process-global, one runtime, context named "main" for widget Lua.
    let runtime = UiRuntime::new(Box::new(UiDirAssets::new(base.clone())), (1280, 720), "main");
    if !runtime.is_ready() {
        eprintln!("tree: no usable fonts under {} — need fonts/*.ttf", base.display());
        return 1;
    }
    let Some(doc) = runtime.load_document(&rel) else {
        eprintln!("tree: failed to load {rel} under {}", base.display());
        return 1;
    };
    doc.show();

    // Inject AFTER show(): the document's onshow Lua defaults apply first,
    // so explicit state wins.
    if let Some(path) = &opts.state {
        if let Err(reason) = inject_state_file(&doc, path) {
            eprintln!("tree: {reason}");
            return 1;
        }
    }

    // Same settle semantics as `shot`: RmlUi updates are wall-clock driven —
    // poll with sleeps so animations tick, stop once three consecutive
    // frames carried no change. Nothing is rasterized, so frames are polled
    // and dropped.
    let mut misses = 0u32;
    for _ in 0..30 {
        misses = if runtime.frame().is_some() { 0 } else { misses + 1 };
        if misses >= 3 {
            break;
        }
        // Lua cmd() output is meaningless headless — drop it every update.
        let _ = drain_commands();
        std::thread::sleep(Duration::from_millis(16));
    }

    let Some(tree) = doc.tree_text(opts.max_depth) else {
        eprintln!("tree: failed to dump the document tree");
        return 1;
    };
    match &opts.out {
        Some(path) => {
            if let Err(e) = std::fs::write(path, &tree) {
                eprintln!("tree: cannot write {}: {e}", path.display());
                return 1;
            }
            println!("{}", path.display());
        }
        None => print!("{tree}"),
    }
    0
}

// ----------------------------------------------------------------- options

/// Parsed `tree` command line.
struct TreeOpts {
    rml: PathBuf,
    state: Option<PathBuf>,
    /// `None` = unlimited nesting depth.
    max_depth: Option<u32>,
    /// `None` = print to stdout.
    out: Option<PathBuf>,
}

/// Hand-rolled parse: one positional plus `--state`, `--max-depth`,
/// `-o`/`--out`. Size (1280x720) and settle frames (30) reuse the `shot`
/// defaults.
fn parse_args(argv: &[String]) -> Result<TreeOpts, String> {
    let mut rml = None;
    let mut state = None;
    let mut max_depth = None;
    let mut out = None;
    let mut i = 0;
    while let Some(arg) = argv.get(i).map(String::as_str) {
        match arg {
            "--state" => state = Some(PathBuf::from(value(argv, &mut i, arg)?)),
            "--max-depth" => max_depth = Some(parse_max_depth(&value(argv, &mut i, arg)?)?),
            "-o" | "--out" => out = Some(PathBuf::from(value(argv, &mut i, arg)?)),
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}")),
            _ => {
                if rml.is_some() {
                    return Err(format!("unexpected argument {arg:?}"));
                }
                rml = Some(PathBuf::from(arg));
            }
        }
        i += 1;
    }
    Ok(TreeOpts {
        rml: rml.ok_or("missing <file.rml>")?,
        state,
        max_depth,
        out,
    })
}

/// Positive level count (`1` = document root only).
fn parse_max_depth(s: &str) -> Result<u32, String> {
    s.parse::<u32>()
        .ok()
        .filter(|&n| n >= 1)
        .ok_or_else(|| format!("invalid --max-depth {s:?} — want a positive level count"))
}
