//! Native application menu bar: File → Open Project… (⌘O) and
//! File → Open Recent ▸ — the gpui analog of the egui frontend's
//! src/native_menu.rs.
//!
//! Mechanism (gpui-pre app menus, Zed-style declaration at startup):
//! - `App::set_menus` installs the bar; items carry boxed `Action`s. The
//!   platform bakes each item's key equivalent from the keymap, and a picked
//!   item reaches the app as `cx.dispatch_action(action)` — dispatched on the
//!   focused window first, then bubbled to global listeners, so handlers
//!   registered with `App::on_action` fire regardless of focus.
//! - `Entity<StudioState>` is created inside `Workspace::new` and only kept by
//!   child panels, so menu actions resolve it through the `StudioSlot` global,
//!   which is filled by `App::observe_new::<StudioState>` the moment the
//!   entity exists — no app.rs changes required.
//! - macOS bakes items into NSMenu at `set_menus` time, so the bar is rebuilt
//!   on recents changes (guarded by a diff, state notifies ~every pump tick).

use std::path::PathBuf;

use gpui_kit::*;

use crate::state::StudioState;

actions!(studio, [OpenProject, QuitStudio]);

/// Carries one recent project path to `StudioState::open_project`.
#[derive(Action, Clone, Debug, PartialEq)]
#[action(namespace = studio, no_json)]
pub struct OpenRecentProject(pub String);

const DIALOG_TITLE: &str = "Open game project folder (contains deerpunk.toml)";
const APP_MENU_NAME: &str = "deerpunk studio";

/// Registry the menu actions resolve the studio state through.
///
/// Menus are declared before any window exists, while `Workspace` builds its
/// `Entity<StudioState>` privately. `observe_new` bridges the gap: the first
/// `StudioState` ever constructed is captured here together with a
/// recents-change subscription that keeps the bar in sync.
#[derive(Default)]
struct StudioSlot {
    state: Option<Entity<StudioState>>,
    built_recents: Vec<String>,
}

impl Global for StudioSlot {}

pub fn init(cx: &mut App) {
    cx.set_global(StudioSlot::default());
    cx.bind_keys([
        KeyBinding::new("cmd-o", OpenProject, None),
        KeyBinding::new("cmd-q", QuitStudio, None),
    ]);
    cx.on_action::<OpenProject>(|_, cx| pick_and_open(cx));
    cx.on_action::<OpenRecentProject>(|action, cx| {
        let Some(state) = studio_state(cx) else {
            return;
        };
        state.update(cx, |state, _| state.open_project(PathBuf::from(&action.0)));
    });
    cx.on_action::<QuitStudio>(|_, cx| cx.quit());
    // The state entity does not exist yet when the bar is declared; capture it
    // the moment Workspace constructs it.
    cx.observe_new::<StudioState>(|_, _, cx| attach_state(cx));
    rebuild(cx, Vec::new());
}

fn studio_state(cx: &mut App) -> Option<Entity<StudioState>> {
    cx.global_mut::<StudioSlot>().state.clone()
}

fn attach_state(cx: &mut Context<StudioState>) {
    if studio_state(cx).is_some() {
        return; // Idempotent: only the first instance drives the menu.
    }
    let entity = cx.entity();
    cx.global_mut::<StudioSlot>().state = Some(entity.clone());
    cx.observe(&entity, |_, _, cx| sync_menu(cx)).detach();
    sync_menu(cx);
}

/// Rebuild the bar only when the recents list actually differs — state
/// notifies on every pump tick and NSMenu is rebuilt wholesale.
fn sync_menu(cx: &mut App) {
    let recents = studio_state(cx)
        .map(|state| state.update(cx, |state, _| state.cfg.recents.clone()))
        .unwrap_or_default();
    let changed = {
        let slot = cx.global_mut::<StudioSlot>();
        let changed = recents != slot.built_recents;
        if changed {
            slot.built_recents = recents.clone();
        }
        changed
    };
    if changed {
        rebuild(cx, recents);
    }
}

fn rebuild(cx: &mut App, recents: Vec<String>) {
    let mut file_items = vec![MenuItem::action("Open Project…", OpenProject)];
    if !recents.is_empty() {
        // egui keeps a disabled placeholder; per wave spec the submenu is
        // hidden until there is at least one recent project.
        file_items.push(MenuItem::separator());
        file_items.push(MenuItem::submenu(
            Menu::new("Open Recent").items(
                recents
                    .iter()
                    .map(|path| MenuItem::action(path.as_str(), OpenRecentProject(path.clone()))),
            ),
        ));
    }
    // On macOS the first menu doubles as the app menu, and set_menus replaces
    // whatever default bar the platform had — keep Quit reachable.
    cx.set_menus([
        Menu::new(APP_MENU_NAME).items([MenuItem::action("Quit", QuitStudio)]),
        Menu::new("File").items(file_items),
    ]);
}

/// ⌘O — async rfd folder picker on the foreground executor; the dialog does
/// not block the frame loop, `open_project` resumes on the main thread.
fn pick_and_open(cx: &mut App) {
    let Some(state) = studio_state(cx) else {
        return;
    };
    cx.spawn(async move |cx| {
        let Some(dir) = rfd::AsyncFileDialog::new()
            .set_title(DIALOG_TITLE)
            .pick_folder()
            .await
        else {
            return;
        };
        let path = dir.path().to_path_buf();
        cx.update(move |cx| state.update(cx, |state, _| state.open_project(path)));
    })
    .detach();
}
