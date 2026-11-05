//! The GUI's root component and its NSH module: `nsh run`, `nsh package`
//! and `cargo run` all mount `App`.

use nsh_core::prelude::*;

mod app;
mod engine;
mod keys;

pub use app::App;

/// The crate's UI set (`nsh.json` `"rust": "."`).
#[native_module("pi-famulus-gui")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}
