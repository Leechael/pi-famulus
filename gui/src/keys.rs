//! Window keystrokes as the TUI's key events. Every key `pi-famulus top`
//! handles is bound here and handed to the shared controller unchanged, so
//! each one does in the window what it does in the terminal. One exception:
//! Ctrl+C quits only on macOS, because on Linux and Windows it is the copy
//! chord. While a text input has the focus (filter, search, agent input) the
//! keys it types stay with it; its text goes to the controller as
//! `Action::Text`. Which of the bound keys still reach the controller while
//! an input has the focus has not been checked in the window (README, "Known
//! limits").

use nsh_core::prelude::*;
pub use pi_famulus::top::Input;

pub use crossterm::event::KeyCode;
use crossterm::event::{KeyEvent, KeyModifiers};

use crate::engine::Engine;

pub fn press(code: KeyCode) -> Input {
    Input::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn with(code: KeyCode, mods: KeyModifiers) -> Input {
    Input::Key(KeyEvent::new(code, mods))
}

/// Whether Ctrl+C quits, as it does in the terminal. Only on macOS, where
/// copy is Cmd+C; on Linux and Windows Ctrl+C is the copy chord and stays
/// with the system (`q` quits everywhere).
const CTRL_C_QUITS: bool = cfg!(target_os = "macos");

/// `(keystroke, key event)` for every key the TUI handles, except that
/// Ctrl+C is bound only where `ctrl_c_quits`.
fn table(ctrl_c_quits: bool) -> Vec<(String, Input)> {
    let mut out: Vec<(String, Input)> = Vec::new();
    for ch in 'a'..='z' {
        out.push((ch.to_string(), press(KeyCode::Char(ch))));
        out.push((format!("shift+{ch}"), with(KeyCode::Char(ch.to_ascii_uppercase()), KeyModifiers::SHIFT)));
    }
    for d in '0'..='9' {
        out.push((d.to_string(), press(KeyCode::Char(d))));
    }
    for (stroke, ch) in [("/", '/'), ("=", '='), ("-", '-'), ("space", ' ')] {
        out.push((stroke.into(), press(KeyCode::Char(ch))));
    }
    for (stroke, ch) in [("shift+/", '?'), ("shift+.", '>'), ("shift+,", '<'), ("shift+=", '+'), ("shift+7", '&')] {
        out.push((stroke.into(), with(KeyCode::Char(ch), KeyModifiers::SHIFT)));
    }
    out.push(("ctrl+x".into(), with(KeyCode::Char('x'), KeyModifiers::CONTROL)));
    if ctrl_c_quits {
        out.push(("ctrl+c".into(), with(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }
    for (stroke, code) in [
        ("enter", KeyCode::Enter),
        ("escape", KeyCode::Esc),
        ("tab", KeyCode::Tab),
        ("backspace", KeyCode::Backspace),
        ("up", KeyCode::Up),
        ("down", KeyCode::Down),
        ("left", KeyCode::Left),
        ("right", KeyCode::Right),
        ("pageup", KeyCode::PageUp),
        ("pagedown", KeyCode::PageDown),
        ("home", KeyCode::Home),
        ("end", KeyCode::End),
        ("f9", KeyCode::F(9)),
    ] {
        out.push((stroke.into(), press(code)));
    }
    out.push(("shift+tab".into(), with(KeyCode::BackTab, KeyModifiers::SHIFT)));
    out.push(("alt+enter".into(), with(KeyCode::Enter, KeyModifiers::ALT)));
    out
}

/// Binds every key for the current owner's lifetime.
pub fn bind(engine: &Engine) {
    for (stroke, input) in table(CTRL_C_QUITS) {
        let e = engine.clone();
        let opts = KeyOptions { repeat: true, ..Default::default() };
        on_key_with(&stroke, opts, move |_| e.send(input.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(table: &[(String, Input)], stroke: &str) -> Option<(KeyCode, KeyModifiers)> {
        table.iter().find(|(s, _)| s == stroke).map(|(_, input)| match input {
            Input::Key(key) => (key.code, key.modifiers),
            other => panic!("{stroke} is bound to {other:?}"),
        })
    }

    #[test]
    fn ctrl_c_quits_only_where_it_is_not_the_copy_chord() {
        let mac = table(true);
        assert_eq!(bound(&mac, "ctrl+c"), Some((KeyCode::Char('c'), KeyModifiers::CONTROL)));
        let elsewhere = table(false);
        assert_eq!(bound(&elsewhere, "ctrl+c"), None, "Ctrl+C is left to the system's copy");
        // Nothing else differs, and `q` quits everywhere.
        assert_eq!(elsewhere.len() + 1, mac.len());
        for t in [&mac, &elsewhere] {
            assert_eq!(bound(t, "q"), Some((KeyCode::Char('q'), KeyModifiers::NONE)));
            assert_eq!(bound(t, "ctrl+x"), Some((KeyCode::Char('x'), KeyModifiers::CONTROL)));
        }
        assert_eq!(CTRL_C_QUITS, cfg!(target_os = "macos"), "this build binds it on macOS only");
    }
}
