//! Copy text to the user's clipboard from inside the TUI.
//!
//! Two routes, best first:
//! - a local clipboard tool (`pbcopy`, `wl-copy`, `xclip`, `xsel`, `clip.exe`)
//!   when the session is not remote;
//! - OSC 52, which asks the terminal itself to set the clipboard. It also
//!   works over SSH and inside tmux (with `set-clipboard on`), but some
//!   terminals ignore it, so it is the fallback rather than the only path.

use std::io::Write;
use std::process::Stdio;

/// Copy `text`; returns a short description of the route taken, for the UI.
pub fn copy(text: &str) -> Result<&'static str, String> {
    let remote = std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    if !remote {
        for (cmd, args) in native_tools() {
            if pipe_to(cmd, args, text).is_ok() {
                return Ok("clipboard");
            }
        }
    }
    osc52(text).map(|_| "terminal clipboard (OSC 52)").map_err(|e| format!("copy failed: {e}"))
}

fn native_tools() -> Vec<(&'static str, &'static [&'static str])> {
    let mut out: Vec<(&'static str, &'static [&'static str])> = Vec::new();
    if cfg!(target_os = "macos") {
        out.push(("pbcopy", &[]));
    } else if cfg!(windows) {
        out.push(("clip.exe", &[]));
    } else {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            out.push(("wl-copy", &[]));
        }
        if std::env::var_os("DISPLAY").is_some() {
            out.push(("xclip", &["-selection", "clipboard"]));
            out.push(("xsel", &["--clipboard", "--input"]));
        }
    }
    out
}

fn pipe_to(cmd: &str, args: &[&str], text: &str) -> std::io::Result<()> {
    let mut child = crate::sys::command_std(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("{cmd} exited with {status}")))
    }
}

fn osc52(text: &str) -> std::io::Result<()> {
    let seq = osc52_sequence(text, std::env::var_os("TMUX").is_some());
    let mut out = std::io::stdout();
    out.write_all(seq.as_bytes())?;
    out.flush()
}

/// `ESC ] 52 ; c ; <base64> BEL`, wrapped in tmux's DCS passthrough when
/// running under tmux so it reaches the outer terminal.
pub fn osc52_sequence(text: &str, tmux: bool) -> String {
    let body = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    if tmux {
        format!("\x1bPtmux;{}\x1b\\", body.replace('\x1b', "\x1b\x1b"))
    } else {
        body
    }
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "{input:?}");
        }
    }

    #[test]
    fn osc52_wraps_for_tmux() {
        assert_eq!(osc52_sequence("hi", false), "\x1b]52;c;aGk=\x07");
        assert_eq!(osc52_sequence("hi", true), "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\");
    }
}
