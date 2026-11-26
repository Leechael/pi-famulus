//! The GUI's root component and its NSH module: `nsh run`, `nsh package`
//! and `cargo run` all mount `App`.

use nsh_core::prelude::*;

mod app;
mod engine;
pub mod instance;
mod keys;
mod theme;
mod tray;

pub use app::App;

/// The app's id, as in `nsh.json`: names its settings directory, its
/// per-user instance lock, and the desktop application.
pub const APP_ID: &str = "network.phala.pi-famulus.gui";

/// The crate's UI set (`nsh.json` `"rust": "."`).
#[native_module("pi-famulus-gui")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}
