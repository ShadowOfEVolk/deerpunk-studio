//! deerpunk-studio — toolkit-agnostic core of the deerpunk game-project
//! studio. UI frontends live in separate crates (`crates/studio-gpui` owns
//! the GPUI binary `deerpunk-studio`); this lib holds the shared domain:
//! project/model state, net protocol, PTY runner, asset scanning, viewers
//! support types and the headless `shot` renderer.

pub mod ansi;
pub mod assets;
pub mod config;
pub mod diagnostics;
pub mod gameinfo;
pub mod mcp;
pub mod net;
pub mod profiles;
pub mod project;
pub mod protocol;
pub mod pty;
pub mod runner;
pub mod widgetshot;
