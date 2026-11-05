//! Runs the GUI on the machine's native desktop backend.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
use nsh_platform_linux as platform;
#[cfg(target_os = "macos")]
use nsh_platform_macos as platform;
#[cfg(target_os = "windows")]
use nsh_platform_windows as platform;

mod app;

fn main() -> impl std::process::Termination {
    env_logger::init();
    platform::run_app(
        platform::AppSource::Rust(platform::Root::of::<app::App>()),
        platform::AppOptions {
            title: "pi-famulus".to_owned(),
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            app_id: "network.phala.pi-famulus".to_owned(),
            size: (900, 560),
            ..Default::default()
        },
    )
}
