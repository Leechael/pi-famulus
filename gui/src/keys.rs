//! Window keystrokes as the TUI's key events. Every key `pi-famulus top`
//! handles is bound here and handed to the shared controller unchanged, so
//! each one does in the window what it does in the terminal. While a text
//! input has the focus (filter, search, agent input) the keys it types stay
//! with it; its text goes to the controller as `Action::Text`.

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

/// `(keystroke, key event)` for every key the TUI handles.
fn table() -> Vec<(String, Input)> {
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
    for (stroke, ch) in [("ctrl+c", 'c'), ("ctrl+x", 'x')] {
        out.push((stroke.into(), with(KeyCode::Char(ch), KeyModifiers::CONTROL)));
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
    for (stroke, input) in table() {
        let e = engine.clone();
        let opts = KeyOptions { repeat: true, ..Default::default() };
        on_key_with(&stroke, opts, move |_| e.send(input.clone()));
    }
}
