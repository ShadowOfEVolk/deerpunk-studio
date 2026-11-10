//! Launch settings panel: run profiles + test scenarios editor, the gpui
//! analog of the egui launch-settings view (src/panels.rs
//! `launch_settings_sidebar` / `launch_settings_center` +
//! `profile_card`/`scenario_card`). Profiles load through the lib
//! `profiles.rs` API (`Profiles::load`, `.deerpunk/profiles.toml`);
//! scenarios come from `.deerpunk/scenarios.toml` via
//! `StudioState::load_scenarios`. Unlike the egui read-only v1, the fields
//! here are editable in memory — the active profile/scenario feed
//! `StudioState::run_instance`; there is no save API in the lib, so edits
//! stay session-local and the .toml files remain the storage format.

use std::collections::{BTreeMap, HashMap};

use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::separator::Separator;
use gpui_kit::component::theme::ActiveTheme;
use gpui_kit::component::{Sizable, Size, h_flex, v_flex};
use gpui_kit::*;

use crate::state::Scenario;
use deerpunk_studio::profiles::{Profiles, RunProfile};

use crate::fa;
use crate::state::StudioState;
use crate::theme;

/// Compact editor row text (files/console panels use 12.5/11).
const ROW_FONT_SIZE: f32 = 12.5;
const SECTION_FONT_SIZE: f32 = 11.0;
/// Fixed label column of a field row.
const LABEL_WIDTH: f32 = 112.;

pub struct LaunchSettingsPanel {
    focus_handle: FocusHandle,
    state: Entity<StudioState>,
    /// Field key (`p/<id>/<field>` / `s/<id>/<field>`) → input state. Inputs
    /// are created lazily in render (they need `Window`), seeded from the
    /// current profiles/scenarios values and kept across frames.
    fields: HashMap<SharedString, Entity<InputState>>,
    /// Fingerprint of the profiles/scenarios last mirrored into `fields`.
    /// A change our own edit handlers didn't pre-apply (project open or
    /// reload) wipes the inputs so they re-seed from state.
    sync_key: String,
}

/// Which state field an input writes back into (captured per input).
#[derive(Clone)]
enum EditTarget {
    Profile { id: String, field: ProfileField },
    Scenario { id: String, field: ScenarioField },
}

#[derive(Clone, Copy, PartialEq)]
enum ProfileField {
    Label,
    EngineProfile,
    GameProfile,
    EngineFeatures,
    GameFeatures,
    Env,
}

#[derive(Clone, Copy, PartialEq)]
enum ScenarioField {
    Label,
    Args,
}

impl LaunchSettingsPanel {
    pub fn new(state: Entity<StudioState>, cx: &mut Context<Self>) -> Self {
        let sync_key = content_key(
            state.read(cx).project.as_ref().map(|p| p.root.as_path()),
            &state.read(cx).profiles,
            &state.read(cx).scenarios,
        );
        cx.observe(&state, |this, _, cx| this.sync_fields(cx))
            .detach();
        Self {
            focus_handle: cx.focus_handle(),
            state,
            fields: HashMap::new(),
            sync_key,
        }
    }

    /// Re-seed the inputs when the profiles/scenarios content changed behind
    /// our back (project open / reload). Our own edit handlers refresh
    /// `sync_key` after each write ([`Self::apply_edit`]), so typing never
    /// lands here and never wipes the inputs mid-keystroke.
    fn sync_fields(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let key = content_key(
            state.project.as_ref().map(|p| p.root.as_path()),
            &state.profiles,
            &state.scenarios,
        );
        if key == self.sync_key {
            return;
        }
        self.sync_key = key;
        self.fields.clear();
        cx.notify();
    }

    /// The lazily-created input for `key`, seeded from `value`. The first
    /// creation installs the Change handler that parses the text back into
    /// the studio state ([`Self::apply_edit`]).
    fn field(
        &mut self,
        key: SharedString,
        value: &str,
        target: EditTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(input) = self.fields.get(&key) {
            return input.clone();
        }
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(field_placeholder(&target)));
        input.update(cx, |state, cx| {
            state.set_value(value.to_string(), window, cx)
        });
        cx.subscribe_in(
            &input,
            window,
            move |this, input, event: &InputEvent, _window, cx| {
                let InputEvent::Change = event else {
                    return;
                };
                let text = input.read(cx).value().to_string();
                this.apply_edit(&target, &text, cx);
            },
        )
        .detach();
        self.fields.insert(key, input.clone());
        input
    }

    /// Parse one field's raw text and write it into `StudioState` — the
    /// inverse of the seeds in the card builders. Refreshes `sync_key` so
    /// the observe-driven re-seed treats this write as ours.
    fn apply_edit(&mut self, target: &EditTarget, text: &str, cx: &mut Context<Self>) {
        let text = text.trim();
        self.state.update(cx, |state, _| match target {
            EditTarget::Profile { id, field } => {
                if let Some(profile) = state.profiles.map.get_mut(id) {
                    match field {
                        ProfileField::Label => profile.label = text.to_string(),
                        ProfileField::EngineProfile => profile.engine_profile = text.to_string(),
                        ProfileField::GameProfile => profile.game_profile = text.to_string(),
                        ProfileField::EngineFeatures => profile.engine_features = split_list(text),
                        ProfileField::GameFeatures => profile.game_features = split_list(text),
                        ProfileField::Env => profile.env = parse_env(text),
                    }
                }
            }
            EditTarget::Scenario { id, field } => {
                if let Some(scenario) = state.scenarios.iter_mut().find(|s| &s.id == id) {
                    match field {
                        ScenarioField::Label => scenario.label = text.to_string(),
                        ScenarioField::Args => scenario.args = split_args(text),
                    }
                }
            }
        });
        let state = self.state.read(cx);
        self.sync_key = content_key(
            state.project.as_ref().map(|p| p.root.as_path()),
            &state.profiles,
            &state.scenarios,
        );
    }

    /// One profile card: id + active mark, then the editable fields (egui
    /// `profile_card`, with inputs instead of read-only kv rows).
    fn profile_card(
        &mut self,
        id: &str,
        profile: &RunProfile,
        active: bool,
        mono_font: &SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let target = |field: ProfileField| EditTarget::Profile {
            id: id.to_string(),
            field,
        };
        let label = self.field(
            pkey(id, "label"),
            &profile.label,
            target(ProfileField::Label),
            window,
            cx,
        );
        let engine_profile = self.field(
            pkey(id, "engine_profile"),
            &profile.engine_profile,
            target(ProfileField::EngineProfile),
            window,
            cx,
        );
        let game_profile = self.field(
            pkey(id, "game_profile"),
            &profile.game_profile,
            target(ProfileField::GameProfile),
            window,
            cx,
        );
        let engine_features = self.field(
            pkey(id, "engine_features"),
            &profile.engine_features.join(", "),
            target(ProfileField::EngineFeatures),
            window,
            cx,
        );
        let game_features = self.field(
            pkey(id, "game_features"),
            &profile.game_features.join(", "),
            target(ProfileField::GameFeatures),
            window,
            cx,
        );
        let env = self.field(
            pkey(id, "env"),
            &env_text(&profile.env),
            target(ProfileField::Env),
            window,
            cx,
        );

        v_flex()
            .gap_1()
            .p_2()
            .rounded_sm()
            .bg(theme::CARD)
            .border_1()
            .border_color(theme::BORDER)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .font_family(mono_font.clone())
                            .text_size(px(ROW_FONT_SIZE))
                            .text_color(theme::TEXT_DIM)
                            .child(id.to_string()),
                    )
                    .children(active.then(|| active_mark())),
            )
            .child(field_row("Label", &label, mono_font))
            .child(field_row("Engine profile", &engine_profile, mono_font))
            .child(field_row("Game profile", &game_profile, mono_font))
            .child(field_row("Engine features", &engine_features, mono_font))
            .child(field_row("Game features", &game_features, mono_font))
            .child(field_row("Env", &env, mono_font))
            .into_any_element()
    }

    /// One scenario card: id + active mark, then the editable label/args
    /// (egui `scenario_card`; args are extra engine CLI args).
    fn scenario_card(
        &mut self,
        scenario: &Scenario,
        active: bool,
        mono_font: &SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let target = |field: ScenarioField| EditTarget::Scenario {
            id: scenario.id.clone(),
            field,
        };
        let label = self.field(
            skey(&scenario.id, "label"),
            &scenario.label,
            target(ScenarioField::Label),
            window,
            cx,
        );
        let args = self.field(
            skey(&scenario.id, "args"),
            &scenario.args.join(" "),
            target(ScenarioField::Args),
            window,
            cx,
        );

        v_flex()
            .gap_1()
            .p_2()
            .rounded_sm()
            .bg(theme::CARD)
            .border_1()
            .border_color(theme::BORDER)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .font_family(mono_font.clone())
                            .text_size(px(ROW_FONT_SIZE))
                            .text_color(theme::TEXT_DIM)
                            .child(scenario.id.clone()),
                    )
                    .children(active.then(|| active_mark())),
            )
            .child(field_row("Label", &label, mono_font))
            .child(field_row("Args", &args, mono_font))
            .into_any_element()
    }
}

impl EventEmitter<PanelEvent> for LaunchSettingsPanel {}

impl Focusable for LaunchSettingsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BasePanel for LaunchSettingsPanel {
    fn panel_name(&self) -> &'static str {
        "LaunchSettingsPanel"
    }
}

impl Panel for LaunchSettingsPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Launch"
    }
}

impl Render for LaunchSettingsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Snapshot the data the cards render from: the field-creating calls
        // below need `&mut self` + `window`, so nothing may keep borrowing
        // the state.
        let (project_root, profiles, scenarios, active_profile, active_scenario) = {
            let state = self.state.read(cx);
            (
                state.project.as_ref().map(|p| p.root.clone()),
                state
                    .profiles
                    .order
                    .iter()
                    .filter_map(|id| state.profiles.map.get(id).cloned().map(|p| (id.clone(), p)))
                    .collect::<Vec<_>>(),
                state.scenarios.clone(),
                state.active_profile.clone(),
                state.active_scenario.clone(),
            )
        };
        let mono_font = cx.theme().mono_font_family.clone();

        let mut content = v_flex().p_2().gap_2();

        // ── header: heading + .toml paths (egui launch_settings_center) ──
        content = content.child(
            h_flex()
                .items_baseline()
                .gap_2()
                .child(
                    div()
                        .text_size(px(16.))
                        .text_color(theme::TEXT)
                        .child("Launch settings"),
                )
                .child(
                    div()
                        .text_size(px(SECTION_FONT_SIZE))
                        .text_color(theme::MUTED)
                        .child(format!(
                            "{} profiles · {} scenarios",
                            profiles.len(),
                            scenarios.len()
                        )),
                ),
        );
        content = content.child(match &project_root {
            Some(root) => v_flex()
                .gap_0p5()
                .child(dim_line(
                    &root.join(".deerpunk/profiles.toml").display().to_string(),
                ))
                .child(dim_line(
                    &root.join(".deerpunk/scenarios.toml").display().to_string(),
                )),
            None => v_flex().child(dim_line("no project open — showing built-in defaults")),
        });
        content = content.child(Separator::horizontal());

        // ── profiles: click-to-activate rows + editable cards ────────────
        content = content.child(section_label("PROFILES"));
        for (id, profile) in &profiles {
            let selected = *id == active_profile;
            let pick = {
                let id = id.clone();
                move |state: &mut StudioState| state.active_profile = id.clone()
            };
            content = content
                .child(pick_row(
                    format!("launch-profile-row-{id}"),
                    fa::GEAR,
                    profile.label.clone(),
                    selected,
                    pick,
                    cx,
                ))
                .child(self.profile_card(id, profile, selected, &mono_font, window, cx));
        }

        // ── scenarios: click-to-activate rows + editable cards ───────────
        content = content.child(section_label("SCENARIOS"));
        for scenario in &scenarios {
            let selected = scenario.id == active_scenario;
            let pick = {
                let id = scenario.id.clone();
                move |state: &mut StudioState| state.active_scenario = id.clone()
            };
            content = content
                .child(pick_row(
                    format!("launch-scenario-row-{}", scenario.id),
                    fa::CHEVRON_DOWN,
                    scenario.label.clone(),
                    selected,
                    pick,
                    cx,
                ))
                .child(self.scenario_card(scenario, selected, &mono_font, window, cx));
        }

        // ── footer: storage note (the lib API is load-only) ──────────────
        content = content.child(Separator::horizontal()).child(dim_line(
            "loaded from .deerpunk/*.toml — field edits are session-local",
        ));

        div().size_full().overflow_hidden().child(
            div()
                .id("launch-settings-scroll")
                .size_full()
                .overflow_scroll()
                .child(content),
        )
    }
}

/// A clickable sidebar-style row (egui sidebar lists): icon + label, accent
/// tint when selected. `pick` applies the selection to the studio state.
fn pick_row(
    id: String,
    icon: char,
    label: String,
    selected: bool,
    pick: impl Fn(&mut StudioState) + 'static,
    cx: &mut Context<LaunchSettingsPanel>,
) -> AnyElement {
    let on_click = cx.listener(move |this, _, _, cx| {
        this.state.update(cx, |state, cx| {
            pick(state);
            cx.notify();
        });
    });
    h_flex()
        .id(id)
        .cursor_pointer()
        .px_2()
        .py_0p5()
        .rounded_sm()
        .gap_1p5()
        .hover(|s| s.bg(theme::HOVER))
        .on_click(on_click)
        .child(
            div()
                .text_color(if selected {
                    theme::ACCENT
                } else {
                    theme::FAINT
                })
                .child(fa::sized(icon, px(10.))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .text_size(px(ROW_FONT_SIZE))
                .text_color(if selected { theme::ACCENT } else { theme::TEXT })
                .child(label),
        )
        .into_any_element()
}

/// Small accent "active" badge (egui `theme::badge(ui, "active", ACCENT)`).
fn active_mark() -> Div {
    div()
        .text_size(px(SECTION_FONT_SIZE))
        .text_color(theme::ACCENT)
        .child("active")
}

/// Label + input row.
fn field_row(name: &str, input: &Entity<InputState>, mono_font: &SharedString) -> Div {
    h_flex()
        .gap_2()
        .items_center()
        .child(
            div()
                .w(px(LABEL_WIDTH))
                .flex_shrink_0()
                .text_size(px(SECTION_FONT_SIZE))
                .text_color(theme::MUTED)
                .child(name.to_string()),
        )
        .child(
            Input::new(input)
                .with_size(Size::Small)
                .flex_1()
                .min_w_0()
                .font_family(mono_font.clone())
                .text_size(px(ROW_FONT_SIZE)),
        )
}

fn section_label(text: &str) -> Div {
    div()
        .text_size(px(SECTION_FONT_SIZE))
        .text_color(theme::MUTED)
        .child(text.to_string())
}

fn dim_line(text: &str) -> Div {
    div()
        .text_size(px(SECTION_FONT_SIZE))
        .text_color(theme::TEXT_DIM)
        .child(text.to_string())
}

/// Input key for a profile field.
fn pkey(id: &str, field: &str) -> SharedString {
    format!("p/{id}/{field}").into()
}

/// Input key for a scenario field.
fn skey(id: &str, field: &str) -> SharedString {
    format!("s/{id}/{field}").into()
}

/// Empty-input hint describing the expected format.
fn field_placeholder(target: &EditTarget) -> &'static str {
    match target {
        EditTarget::Profile {
            field: ProfileField::EngineFeatures | ProfileField::GameFeatures,
            ..
        } => "space separated",
        EditTarget::Profile {
            field: ProfileField::Env,
            ..
        } => "KEY=VALUE …",
        EditTarget::Scenario {
            field: ScenarioField::Args,
            ..
        } => "engine args…",
        _ => "label",
    }
}

/// Fingerprint of everything the inputs mirror: project root + profiles +
/// scenarios. A change our own edits didn't pre-apply re-seeds the fields.
fn content_key(
    root: Option<&std::path::Path>,
    profiles: &Profiles,
    scenarios: &[Scenario],
) -> String {
    format!(
        "{:?}|{:?}|{:?}",
        root.map(|p| p.to_string_lossy()),
        profiles,
        scenarios
    )
}

/// Comma/space separated features text → list (inverse of the `", "`
/// seeds).
fn split_list(text: &str) -> Vec<String> {
    text.split([',', ' ', '\t'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Engine args text → args list: split on whitespace. An arg containing a
/// space must come from the .toml file directly (this editor can't
/// re-create the quoting).
fn split_args(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

/// `KEY=VALUE` items separated by spaces → env map (items without `=` are
/// dropped).
fn parse_env(text: &str) -> BTreeMap<String, String> {
    text.split_whitespace()
        .filter_map(|item| item.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Env map → the `KEY=VALUE …` seed text for the env input.
fn env_text(env: &BTreeMap<String, String>) -> String {
    env.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::state::Scenario;
    use deerpunk_studio::profiles::Profiles;

    use super::{
        EditTarget, ProfileField, ScenarioField, content_key, env_text, field_placeholder,
        parse_env, split_args, split_list,
    };

    /// The feature seeds round-trip: `", "` in, whitespace/comma split out.
    #[test]
    fn feature_lists_round_trip() {
        assert_eq!(split_list("full-kira, puffin"), vec!["full-kira", "puffin"]);
        assert_eq!(split_list("  a  b "), vec!["a", "b"]);
        assert_eq!(split_list("a, , b,"), vec!["a", "b"]);
        assert!(split_list("").is_empty());
        assert_eq!(vec!["full-kira", "puffin"].join(", "), "full-kira, puffin");
    }

    /// Engine args split on whitespace; empty text → no args.
    #[test]
    fn args_split_on_whitespace() {
        assert_eq!(
            split_args("--inspector-host 0.0.0.0 --inspector-port 9000"),
            vec!["--inspector-host", "0.0.0.0", "--inspector-port", "9000"]
        );
        assert_eq!(split_args(""), Vec::<String>::new());
    }

    /// Env parses `KEY=VALUE` items; entries without `=` are dropped.
    #[test]
    fn env_parses_key_value_items() {
        let mut expected = BTreeMap::new();
        expected.insert("TRACY".to_string(), "1".to_string());
        expected.insert("RUST_LOG".to_string(), "debug".to_string());
        assert_eq!(parse_env("TRACY=1 RUST_LOG=debug"), expected);
        assert!(parse_env("NOEQUALS").is_empty());
        assert_eq!(env_text(&expected), "RUST_LOG=debug TRACY=1");
        assert_eq!(env_text(&BTreeMap::new()), "");
    }

    /// Placeholders describe the expected per-field format.
    #[test]
    fn placeholders_describe_format() {
        let profile = |field| EditTarget::Profile {
            id: "dev".into(),
            field,
        };
        let scenario = |field| EditTarget::Scenario {
            id: "default".into(),
            field,
        };
        assert_eq!(
            field_placeholder(&profile(ProfileField::Env)),
            "KEY=VALUE …"
        );
        assert_eq!(
            field_placeholder(&profile(ProfileField::EngineFeatures)),
            "space separated"
        );
        assert_eq!(
            field_placeholder(&scenario(ScenarioField::Args)),
            "engine args…"
        );
        assert_eq!(field_placeholder(&profile(ProfileField::Label)), "label");
    }

    /// The content fingerprint tracks profile/scenario edits (it decides
    /// whether the inputs are re-seeded from state).
    #[test]
    fn content_key_tracks_state_content() {
        let scenarios = |args: &[&str]| -> Vec<Scenario> {
            vec![Scenario {
                id: "default".into(),
                label: "Default".into(),
                args: args.iter().map(|s| s.to_string()).collect(),
            }]
        };
        let profiles = Profiles::default();
        let key = content_key(None, &profiles, &scenarios(&[]));

        // Same content → same key (steady-state observes are cheap).
        assert_eq!(content_key(None, &profiles, &scenarios(&[])), key);
        // A scenario arg edit → different key.
        assert_ne!(content_key(None, &profiles, &scenarios(&["--run"])), key);
        // A project root change → different key.
        let path = std::path::Path::new("/tmp/game");
        assert_ne!(content_key(Some(path), &profiles, &scenarios(&[])), key);
    }
}
