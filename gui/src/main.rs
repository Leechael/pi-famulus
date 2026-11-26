//! Runs the GUI on the machine's native desktop backend.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
use nsh_platform_linux as platform;
#[cfg(target_os = "macos")]
use nsh_platform_macos as platform;
#[cfg(target_os = "windows")]
use nsh_platform_windows as platform;

use pi_famulus_gui::APP_ID;

fn main() -> impl std::process::Termination {
    env_logger::init();
    // Before any window: a second launch shows the first and exits here.
    pi_famulus_gui::instance::ensure_single();
    platform::run_app(
        // The id names the app's own settings directory; without it they
        // land in one shared by every NSH app that has no id (`default`).
        platform::AppSource::Rust(platform::Root::of::<pi_famulus_gui::App>().app_id(APP_ID)),
        platform::AppOptions {
            title: "pi-famulus".to_owned(),
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            app_id: APP_ID.to_owned(),
            size: (1280, 820),
            ..Default::default()
        },
    )
}
