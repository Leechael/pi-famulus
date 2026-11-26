//! The window's settings, set from the menu bar and kept across launches:
//! appearance (system, light, dark) and tray mode.

use nsh_core::desktop::MenuRole;
use nsh_core::element::desktop::{set_application_menu, MenuItem};
use nsh_core::prelude::*;
use nsh_core::storage::Storage;

use crate::tray;

const APPEARANCE: &str = "appearance";
const TRAY: &str = "tray";

fn load_appearance() -> Appearance {
    match Storage::get_item(APPEARANCE).as_deref() {
        Some("light") => Appearance::Light,
        Some("dark") => Appearance::Dark,
        _ => Appearance::System,
    }
}

fn appearance_key(a: Appearance) -> &'static str {
    match a {
        Appearance::Light => "light",
        Appearance::Dark => "dark",
        _ => "system",
    }
}

/// Applies the saved settings, keeps them as they change, and sets the
/// menu bar that changes them. Call once, in the root component.
pub fn install() {
    let appearance = signal(load_appearance());
    let tray_on = signal(tray::AVAILABLE && Storage::get_item(TRAY).as_deref() != Some("off"));

    effect(move || {
        let a = appearance.get();
        set_color_scheme(a);
        let _ = Storage::set_item(APPEARANCE, appearance_key(a));
    });
    effect(move || {
        let on = tray_on.get();
        tray::set_enabled(on);
        let _ = Storage::set_item(TRAY, if on { "on" } else { "off" });
    });
    effect(move || set_application_menu(menu(appearance, tray_on)));
}

fn menu(appearance: Signal<Appearance>, tray_on: Signal<bool>) -> Vec<MenuItem> {
    let now = appearance.get();
    let pick = |label: &str, a: Appearance| {
        MenuItem::new(label).checked(now == a).on_select(move || appearance.set(a))
    };
    let mut view = vec![MenuItem::submenu(
        "Appearance",
        [pick("System", Appearance::System), pick("Light", Appearance::Light), pick("Dark", Appearance::Dark)],
    )];
    if tray::AVAILABLE {
        let on = tray_on.get();
        view.push(MenuItem::separator());
        view.push(MenuItem::new("Minimize to Menu Bar").checked(on).on_select(move || tray_on.set(!on)));
    }
    vec![
        MenuItem::submenu(
            "pi-famulus",
            [
                MenuItem::role(MenuRole::About),
                MenuItem::separator(),
                MenuItem::role(MenuRole::Hide),
                MenuItem::role(MenuRole::HideOthers),
                MenuItem::role(MenuRole::ShowAll),
                MenuItem::separator(),
                MenuItem::role(MenuRole::Quit),
            ],
        ),
        MenuItem::submenu(
            "Edit",
            [
                MenuItem::role(MenuRole::Undo),
                MenuItem::role(MenuRole::Redo),
                MenuItem::separator(),
                MenuItem::role(MenuRole::Cut),
                MenuItem::role(MenuRole::Copy),
                MenuItem::role(MenuRole::Paste),
                MenuItem::role(MenuRole::SelectAll),
            ],
        ),
        MenuItem::submenu("View", view),
        MenuItem::submenu(
            "Window",
            [
                // Into the tray in tray mode, else to the Dock.
                MenuItem::new("Minimize").shortcut("mod+m").on_select(tray::minimize),
                MenuItem::role(MenuRole::Zoom),
                MenuItem::role(MenuRole::ToggleFullScreen),
                MenuItem::separator(),
                MenuItem::role(MenuRole::Close),
            ],
        ),
    ]
}
