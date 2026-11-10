//! Instance lifecycle: pre_run pipeline (build game libs → install into the
//! gamedir → build engine) and the engine process under a PTY. All command
//! output is streamed to the console via `Event::Console`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use crate::profiles::RunProfile;
use crate::project::GameLib;
use crate::pty::PtySession;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Building,
    Running,
    Stopping,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Ok,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Step {
    pub name: String,
    pub state: StepState,
}

/// Fixed pre_run pipeline step names (shared with the UI).
pub const STEP_NAMES: [&str; 4] = [
    "build game libs",
    "install game libs",
    "build engine",
    "launch engine",
];

pub enum Event {
    Console(Vec<u8>),
    State(RunState),
    Step(usize, StepState),
    /// The engine process exited.
    Exited(i32),
    /// Studio-generated console line.
    Note(String),
}

pub struct InstancePlan {
    pub project_root: PathBuf,
    pub gamedir: PathBuf,
    pub engine_dir: PathBuf,
    pub profile: RunProfile,
    pub gamelibs: Vec<GameLib>,
    /// Extra engine CLI args from the active scenario.
    pub extra_args: Vec<String>,
}

enum Cmd {
    Start(InstancePlan),
    Stop,
    Input(String),
}

pub struct Runner {
    cmd_tx: Sender<Cmd>,
    ev_rx: Receiver<Event>,
}

impl Runner {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ev_tx, ev_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("instance-runner".into())
            .spawn(move || run(cmd_rx, ev_tx))
            .expect("spawn instance-runner thread");
        Self { cmd_tx, ev_rx }
    }

    pub fn start(&self, plan: InstancePlan) {
        let _ = self.cmd_tx.send(Cmd::Start(plan));
    }

    pub fn stop(&self) {
        let _ = self.cmd_tx.send(Cmd::Stop);
    }

    /// Send a console command line to the running engine.
    pub fn input(&self, line: String) {
        let _ = self.cmd_tx.send(Cmd::Input(line));
    }

    pub fn poll(&self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(ev) = self.ev_rx.try_recv() {
            out.push(ev);
        }
        out
    }
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

fn run(cmd_rx: Receiver<Cmd>, ev: Sender<Event>) {
    loop {
        // Idle: block until a run request; other commands are meaningless here.
        match cmd_rx.recv() {
            Ok(Cmd::Start(plan)) => {
                pipeline(&plan, &cmd_rx, &ev);
            }
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

enum Outcome {
    Ok,
    Failed,
    Stopped,
}

/// Execute the whole pipeline; returns when the engine exits or the pipeline
/// fails/is stopped.
fn pipeline(plan: &InstancePlan, cmd_rx: &Receiver<Cmd>, ev: &Sender<Event>) {
    let _ = ev.send(Event::State(RunState::Building));
    let _ = ev.send(Event::Note(format!(
        "[studio] profile `{}` — engine {}, game {}",
        plan.profile.label, plan.profile.engine_profile, plan.profile.game_profile
    )));

    let mut steps = vec![
        Step {
            name: "build game libs".into(),
            state: StepState::Pending,
        },
        Step {
            name: "install game libs".into(),
            state: StepState::Pending,
        },
        Step {
            name: "build engine".into(),
            state: StepState::Pending,
        },
        Step {
            name: "launch engine".into(),
            state: StepState::Pending,
        },
    ];

    match step_build_gamelibs(plan, cmd_rx, ev, &mut steps, 0) {
        Outcome::Ok => {}
        Outcome::Failed => return,
        Outcome::Stopped => return stop_idle(ev),
    }
    match step_install_gamelibs(plan, ev, &mut steps, 1) {
        Outcome::Ok => {}
        Outcome::Failed => return,
        Outcome::Stopped => return stop_idle(ev),
    }
    match step_build_engine(plan, cmd_rx, ev, &mut steps, 2) {
        Outcome::Ok => {}
        Outcome::Failed => return,
        Outcome::Stopped => return stop_idle(ev),
    }
    step_launch_engine(plan, cmd_rx, ev, &mut steps, 3);
}

fn stop_idle(ev: &Sender<Event>) {
    let _ = ev.send(Event::Note("[studio] run aborted".to_string()));
    let _ = ev.send(Event::State(RunState::Idle));
}

fn set_step(steps: &mut [Step], ev: &Sender<Event>, idx: usize, state: StepState) {
    if let Some(s) = steps.get_mut(idx) {
        s.state = state.clone();
        let _ = ev.send(Event::Step(idx, state));
    }
}

fn step_build_gamelibs(
    plan: &InstancePlan,
    cmd_rx: &Receiver<Cmd>,
    ev: &Sender<Event>,
    steps: &mut [Step],
    idx: usize,
) -> Outcome {
    if plan.gamelibs.is_empty() {
        set_step(steps, ev, idx, StepState::Ok);
        let _ = ev.send(Event::Note(
            "[studio] no [launch].gamelibs in deerpunk.toml — skipping game build".to_string(),
        ));
        return Outcome::Ok;
    }
    set_step(steps, ev, idx, StepState::Running);
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&plan.project_root).arg("build");
    for lib in &plan.gamelibs {
        cmd.args(["-p", &lib.package]);
    }
    apply_profile(&mut cmd, &plan.profile.game_profile);
    if !plan.profile.game_features.is_empty() {
        cmd.arg("--features")
            .arg(plan.profile.game_features.join(","));
    }
    build_outcome(
        pump(&mut cmd, cmd_rx, ev),
        steps,
        ev,
        idx,
        "game build failed",
    )
}

fn step_install_gamelibs(
    plan: &InstancePlan,
    ev: &Sender<Event>,
    steps: &mut [Step],
    idx: usize,
) -> Outcome {
    set_step(steps, ev, idx, StepState::Running);
    let (lib_prefix, lib_suffix) = lib_names();
    let profile = profile_dir(&plan.profile.game_profile);
    for lib in &plan.gamelibs {
        let src = plan
            .project_root
            .join("target")
            .join(profile)
            .join(format!("{lib_prefix}{}{lib_suffix}", lib.artifact));
        let dst = plan
            .gamedir
            .join("dlls")
            .join(&lib.slot)
            .join(format!("{}{}", lib.artifact, lib_suffix));
        if !src.is_file() {
            let _ = ev.send(Event::Note(format!(
                "[studio] missing artifact {}",
                src.display()
            )));
            set_step(steps, ev, idx, StepState::Failed("artifact missing".into()));
            let _ = ev.send(Event::State(RunState::Failed("game build failed".into())));
            return Outcome::Failed;
        }
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::copy(&src, &dst) {
            let _ = ev.send(Event::Note(format!(
                "[studio] copy {} → {}: {e}",
                src.display(),
                dst.display()
            )));
            set_step(steps, ev, idx, StepState::Failed("copy failed".into()));
            let _ = ev.send(Event::State(RunState::Failed("install failed".into())));
            return Outcome::Failed;
        }
        let _ = ev.send(Event::Note(format!(
            "[studio] installed {} → {}",
            src.display(),
            dst.display()
        )));
    }
    set_step(steps, ev, idx, StepState::Ok);
    Outcome::Ok
}

fn step_build_engine(
    plan: &InstancePlan,
    cmd_rx: &Receiver<Cmd>,
    ev: &Sender<Event>,
    steps: &mut [Step],
    idx: usize,
) -> Outcome {
    set_step(steps, ev, idx, StepState::Running);
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&plan.engine_dir)
        .arg("build")
        .arg("--no-default-features")
        .arg("--features")
        .arg(plan.profile.engine_features.join(","));
    apply_profile(&mut cmd, &plan.profile.engine_profile);
    build_outcome(
        pump(&mut cmd, cmd_rx, ev),
        steps,
        ev,
        idx,
        "engine build failed",
    )
}

fn build_outcome(
    result: PumpResult,
    steps: &mut [Step],
    ev: &Sender<Event>,
    idx: usize,
    fail_msg: &str,
) -> Outcome {
    match result {
        PumpResult::Exited(0) => {
            set_step(steps, ev, idx, StepState::Ok);
            Outcome::Ok
        }
        PumpResult::Exited(code) => {
            set_step(
                steps,
                ev,
                idx,
                StepState::Failed(format!("exit code {code}")),
            );
            let _ = ev.send(Event::State(RunState::Failed(fail_msg.to_string())));
            Outcome::Failed
        }
        PumpResult::Stopped => Outcome::Stopped,
    }
}

fn step_launch_engine(
    plan: &InstancePlan,
    cmd_rx: &Receiver<Cmd>,
    ev: &Sender<Event>,
    steps: &mut [Step],
    idx: usize,
) {
    set_step(steps, ev, idx, StepState::Running);
    if !plan.engine_dir.join("Cargo.toml").is_file() {
        set_step(
            steps,
            ev,
            idx,
            StepState::Failed(format!(
                "engine repo not found: {}",
                plan.engine_dir.display()
            )),
        );
        let _ = ev.send(Event::State(RunState::Failed("engine repo missing".into())));
        return;
    }

    // Launch like run.sh does: `cargo run` from the engine repo (resolves the
    // SDL2 @rpath correctly; a direct binary exec does not).
    let mut cmd = Command::new("cargo");
    cmd.current_dir(&plan.engine_dir)
        .arg("run")
        .arg("--no-default-features")
        .arg("--features")
        .arg(plan.profile.engine_features.join(","));
    apply_profile(&mut cmd, &plan.profile.engine_profile);
    cmd.arg("--")
        .arg("--gamedir")
        .arg(&plan.gamedir)
        .arg("--svlib")
        .arg(lib_path_for(plan, "server"))
        .arg("--cllib")
        .arg(lib_path_for(plan, "client"))
        .args(&plan.extra_args);

    // Same PATH handling as run.sh: gamelib loader dirs first.
    let engine_target = plan
        .engine_dir
        .join("target")
        .join(profile_dir(&plan.profile.engine_profile));
    let prepend = format!(
        "{}:{}:",
        plan.gamedir.join("cl_dlls").display(),
        engine_target.display()
    );
    let path_env = match std::env::var("PATH") {
        Ok(p) => format!("{prepend}{p}"),
        Err(_) => prepend.trim_end_matches(':').to_string(),
    };
    cmd.env("PATH", path_env);
    for (k, v) in &plan.profile.env {
        cmd.env(k, v);
    }

    let _ = ev.send(Event::State(RunState::Running));
    match pump(&mut cmd, cmd_rx, ev) {
        PumpResult::Exited(0) => {
            set_step(steps, ev, idx, StepState::Ok);
            let _ = ev.send(Event::Exited(0));
            let _ = ev.send(Event::State(RunState::Idle));
        }
        PumpResult::Exited(code) => {
            set_step(
                steps,
                ev,
                idx,
                StepState::Failed(format!("exit code {code}")),
            );
            let _ = ev.send(Event::Exited(code));
            let _ = ev.send(Event::State(RunState::Failed(format!(
                "engine exited with {code}"
            ))));
        }
        PumpResult::Stopped => {
            set_step(steps, ev, idx, StepState::Ok);
            let _ = ev.send(Event::Note("[studio] instance stopped".to_string()));
            let _ = ev.send(Event::State(RunState::Idle));
        }
    }
}

enum PumpResult {
    Exited(i32),
    Stopped,
}

/// Spawn `cmd` under a PTY and pump its output to the console until exit or
/// Stop. Console input (`Cmd::Input`) is forwarded only for the engine.
fn pump(cmd: &mut Command, cmd_rx: &Receiver<Cmd>, ev: &Sender<Event>) -> PumpResult {
    let mut session = match PtySession::spawn(cmd) {
        Ok(s) => s,
        Err(e) => {
            let _ = ev.send(Event::Note(format!("[studio] {e}")));
            return PumpResult::Exited(1);
        }
    };

    let mut buf = [0u8; 4096];
    let mut stopped = false;
    let mut stop_deadline: Option<Instant> = None;
    loop {
        match session.read(&mut buf) {
            Ok(0) => {
                crate::pty::debug_log("pty read: EOF");
                // An EOF on the master is not proof the child exited (the
                // slave can be closed independently); only trust the wait
                // status. Keep pumping until the child is really gone.
                if session
                    .child
                    .try_wait()
                    .map(|s| s.is_some())
                    .unwrap_or(false)
                {
                    let status = session.child.wait();
                    crate::pty::debug_log(&format!("pump exit after EOF: {status:?}"));
                    return finish_pump(exit_code(&status), stopped);
                }
            }
            Ok(n) => {
                let _ = ev.send(Event::Console(buf[..n].to_vec()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                crate::pty::debug_log(&format!("pty read error: {e}"));
                let _ = ev.send(Event::Note(format!("[studio] pty read: {e}")));
                let status = session.child.wait();
                crate::pty::debug_log(&format!("pump exit after read error: {status:?}"));
                return finish_pump(exit_code(&status), stopped);
            }
        }

        // Commands.
        loop {
            match cmd_rx.try_recv() {
                Ok(Cmd::Stop) => {
                    if !stopped {
                        stopped = true;
                        let _ = ev.send(Event::State(RunState::Stopping));
                        session.terminate();
                        stop_deadline = Some(Instant::now() + Duration::from_secs(3));
                    }
                }
                Ok(Cmd::Input(line)) => {
                    if let Err(e) = session.send_line(&line) {
                        let _ = ev.send(Event::Note(format!("[studio] stdin: {e}")));
                    }
                }
                Ok(Cmd::Start(_)) => {} // ignored while busy
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        if let Some(deadline) = stop_deadline {
            if Instant::now() >= deadline {
                session.kill();
                stop_deadline = None;
            }
        }

        if session
            .child
            .try_wait()
            .map(|s| s.is_some())
            .unwrap_or(false)
        {
            let status = session.child.wait();
            crate::pty::debug_log(&format!("pump exit: {status:?}"));
            return finish_pump(exit_code(&status), stopped);
        }

        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A stop (SIGTERM/SIGKILL from the UI) is a deliberate stop, not a crash.
fn finish_pump(code: i32, stopped: bool) -> PumpResult {
    if stopped {
        PumpResult::Stopped
    } else {
        PumpResult::Exited(code)
    }
}

/// Exit code for `Event::Exited`; signal deaths collapse to -1.
fn exit_code(status: &std::io::Result<std::process::ExitStatus>) -> i32 {
    status.as_ref().ok().and_then(|s| s.code()).unwrap_or(-1)
}

fn apply_profile(cmd: &mut Command, profile: &str) {
    if profile != "debug" {
        cmd.arg("--profile").arg(profile);
    }
}

/// Cargo target directory name for a profile name (`debug`/`release`/other).
fn profile_dir(profile: &str) -> &str {
    profile
}

fn lib_names() -> (&'static str, &'static str) {
    if cfg!(target_os = "macos") {
        ("lib", ".dylib")
    } else if cfg!(target_os = "linux") {
        ("lib", ".so")
    } else {
        ("", ".dll")
    }
}

/// gamedir path of an installed gamelib by slot (`server` / `client`).
fn lib_path_for(plan: &InstancePlan, slot: &str) -> PathBuf {
    let (_, lib_suffix) = lib_names();
    let artifact = plan
        .gamelibs
        .iter()
        .find(|l| l.slot == slot)
        .map(|l| l.artifact.clone())
        .unwrap_or_default();
    plan.gamedir
        .join("dlls")
        .join(slot)
        .join(format!("{artifact}{lib_suffix}"))
}
