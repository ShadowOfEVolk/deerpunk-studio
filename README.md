# deerpunk-studio

A native Rust desktop IDE (GPUI) for games built on the **deerpunk** engine (sibling repository). It wraps the whole edit → build → run → inspect loop in one window: open a game project, launch the engine with one click, and watch live inspector data, console output and profiling while the game runs.

![deerpunk-studio — Files view](docs/screens/studio_files.png)

## Features

- **Project model** — opens a game folder containing a `deerpunk.toml` (`[project]`, `[engine].gamedir`, `[engine].path`, `[launch].gamelibs`), parses `gameinfo.txt`, scans and indexes the gamedir assets.
- **Files view** — hierarchical file browser over the project root and gamedir, with tabbed asset viewers. Viewers live in a declarative registry (`register("ext=…/magic=…/bin=…", render)`: extension lists, hex magic-byte prefixes, and a binary/text content sniff, first match wins) instead of hardcoded `if ext == …` chains: any text file previews as monospace text, textures decode in-app (png/jpg/bmp/tga/webp/…, plus magic-byte matching for extensionless files), svg renders via the vector loader, and RmlUi widgets (`.rml`) get a **live preview** — the engine's `deerpunk-ui` crate loads the document with the same `UiDirAssets` resolver as in-game (RCSS sheets, `file://` textures, fonts, data-model templates) and the resulting draw list is rasterized into a texture shown right in the viewer — on the GPU via the engine's own wgpu `UiPass` (offscreen device, the game's `shaders/ui.wgsl`), with the in-app software rasterizer kept as fallback (no adapter / shader / runtime error); the headless `shot` PNG path stays CPU-only.
- **Cvars view** — browses the engine cvars catalog (`config/cvars_catalog.json`), grouped and filterable.
- **Live inspector** — JSON-RPC over WebSocket to the running engine's inspector server (default `127.0.0.1:9000`), auto-connects on project open; each collection gets its own tab in the left rail with a single-line entity list (filterable sidebar), and clicking an entity summons a shared **Inspector** center tab with the component/JSON detail. Refresh interval is configurable.
- **Console** — bottom PTY panel with ANSI colors and stdin input, fed by the launch pipeline.
- **Launch profiles & scenarios** — `.deerpunk/profiles.toml` (build profiles, cargo features, env) and `.deerpunk/scenarios.toml` (extra engine CLI args), switchable from the toolbar.
- **Play button** — one-shot pipeline: build gamelibs → install them into the gamedir → build the engine → `cargo run … -- --gamedir … --svlib … --cllib …`, all output streamed to the console.
- **Built-in Puffin profiler** — live flame-graph panel fed by the game's puffin server (default `:8585`), auto-connecting.
![deerpunk-studio — Puffin profiler](docs/screens/studio_profiler.jpg)
- **MCP for AI agents** — optional `deerpunk-mcp` companion server exposing the engine inspector collections over streamable HTTP (see below).
- Native macOS **File menu** (recent projects) via GPUI's native menu support (`App::set_menus`).

![deerpunk-studio — Cvars view](docs/screens/studio_cvars.png)

## Build & run

```sh
cargo build --release
./target/release/deerpunk-studio /path/to/game-project
```

Note: the studio depends on the sibling engine checkout — `deerpunk-ui` / `deerpunk-engine-common` are path dependencies on `../deerpunk` (first build also compiles RmlUi via cmake, so it needs cmake + libclang).

Launching from inside a game folder with no arguments opens that folder automatically (`deerpunk.toml` present), otherwise the last project. Passing `--run` starts the game instance immediately on open.

## Usage walkthrough

1. **Open a project** — pass the project folder on the command line, drop it onto the window, or use the File menu. The studio reads `deerpunk.toml` (engine path + gamedir), loads `gameinfo.txt` and scans assets in the background; the status bar shows the file count and whether the gamedir is valid.
2. **Press Play** — the toolbar Play button runs the pipeline (gamelibs → install → engine build → launch). Progress of each step and all engine output appear in the bottom console; you can type into the engine's stdin there.
3. **Inspect live state** — with the engine running, the inspector connection auto-connects (`127.0.0.1:9000` by default, configurable per scenario). Live collections appear as tabs in the left rail; pick a refresh interval in the status bar or refresh manually.
4. **Profile** — if the engine was launched with the `puffin` feature, the built-in Puffin panel shows the live flame graph from `:8585`.
5. **Tune & relaunch** — switch launch profiles (debug/release, cargo features, env) and scenarios (extra engine args) from the toolbar; both live in `.deerpunk/` in the project folder.

### Widget screenshots (`shot`)

Headless RmlUi widget rendering for layout iteration — no window, no running engine:

```sh
deerpunk-studio shot path/to/gamedir/ui/widgets/loading_screen.rml --size 1280x720 -o loading.png
deerpunk-studio shot …/loading_screen.rml --state state.toml   # inject data-model fields
```

`--state` takes a flat TOML file (`title = "…"`, `progress = 0.7`, `visible = true`) injected into the document's `data-model` fields. The widget loads through the same `UiDirAssets` resolver as in-game (RCSS, `file://` textures, fonts), settles for a few frames, and the draw list is software-rasterized straight to a PNG — deliberately CPU-only for deterministic output without a GPU (the live preview in the app uses the GPU path).

## MCP for AI agents

The optional `deerpunk-mcp` crate (`crates/deerpunk-mcp`) ships an MCP server binary that exposes the engine's live inspector collections to AI agents over streamable HTTP:

- Endpoint: `http://<LAN-IP>:9100/mcp`
- `GET /status` — server/engine status probe
- Tools: `inspector_status`, `collections_list`, `collection_items`

The studio's left rail shows an MCP icon; clicking it opens a modal with the server status, the available tools, the machine's LAN IP, and instructions for connecting agents.

Generic MCP client config (point `url` at your machine's LAN address so agents on other devices can reach it):

```json
{
  "mcpServers": {
    "deerpunk": {
      "url": "http://192.168.1.20:9100/mcp"
    }
  }
}
```

Claude Desktop style:

```json
{
  "mcpServers": {
    "deerpunk": {
      "type": "http",
      "url": "http://192.168.1.20:9100/mcp"
    }
  }
}
```

## Development

Layout: this crate is the toolkit-agnostic core (project model, net protocol, PTY runner, asset scanning, `shot` renderer); the GPUI frontend lives in `crates/studio-gpui` and produces the `deerpunk-studio` binary.

```sh
cargo test --workspace    # core unit tests + headless GPUI snapshot tests
```
