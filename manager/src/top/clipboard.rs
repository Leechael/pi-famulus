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
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const HELPER_TIMEOUT: Duration = Duration::from_secs(2);

pub enum CopyRoute {
    Native,
    /// OSC 52 has no acknowledgement; the terminal may ignore the request.
    Osc52,
}

/// Copy `text` without blocking the UI; native helpers have a shared write/wait deadline.
pub async fn copy(text: &str) -> Result<CopyRoute, String> {
    let remote = std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    if !remote {
        for (cmd, args) in native_tools() {
            if pipe_to(cmd, args, text).await.is_ok() {
                return Ok(CopyRoute::Native);
            }
        }
    }
    osc52(text).map(|_| CopyRoute::Osc52).map_err(|e| format!("copy failed: {e}"))
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

async fn pipe_to(cmd: &str, args: &[&str], text: &str) -> std::io::Result<()> {
    let mut child = crate::sys::command_tokio(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let operation = async {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).await?;
        }
        let status = child.wait().await?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!("{cmd} exited with {status}")))
        }
    };
    let result = tokio::time::timeout(HELPER_TIMEOUT, operation)
        .await
        .unwrap_or_else(|_| Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "clipboard helper timed out")));
    if result.is_err() {
        // Also reap on write errors; kill_on_drop covers cancellation by the UI.
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    result
}

fn osc52(text: &str) -> std::io::Result<()> {
    let seq = osc52_sequence(text);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(seq.as_bytes())?;
    out.flush()
}

/// `ESC ] 52 ; c ; <base64> BEL`. Tmux handles this through `set-clipboard`;
/// DCS wrapping would instead require its non-default `allow-passthrough`.
pub fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
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

    #[cfg(unix)]
    #[tokio::test]
    async fn native_helper_receives_the_full_command() {
        let path = std::env::temp_dir().join(format!("famulus-clipboard-{}", uuid::Uuid::new_v4()));
        let text = "printf '你好'\necho second line\n";
        pipe_to("/bin/sh", &["-c", "cat > \"$1\"", "sh", path.to_str().unwrap()], text).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_helper_is_reaped_after_a_broken_pipe() {
        let path = std::env::temp_dir().join(format!("famulus-clipboard-pid-{}", uuid::Uuid::new_v4()));
        let text = "x".repeat(1024 * 1024);
        let result = pipe_to("/bin/sh", &["-c", "echo $$ > \"$1\"; exit 1", "sh", path.to_str().unwrap()], &text).await;
        assert!(result.is_err());
        let pid = std::fs::read_to_string(&path).unwrap().trim().parse::<u32>().unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(!crate::sys::pid_alive(pid), "failed clipboard helper {pid} was not reaped");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unresponsive_helper_has_a_deadline_while_writing() {
        let path = std::env::temp_dir().join(format!("famulus-clipboard-pid-{}", uuid::Uuid::new_v4()));
        let text = "x".repeat(1024 * 1024);
        let args = ["-c", "echo $$ > \"$1\"; exec sleep 4", "sh", path.to_str().unwrap()];
        let operation = pipe_to("/bin/sh", &args, &text);
        tokio::pin!(operation);
        // A blocked clipboard write must still let the event loop service keys.
        tokio::select! {
            result = &mut operation => panic!("helper returned before the responsiveness check: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        assert_eq!(operation.await.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        let pid = std::fs::read_to_string(&path).unwrap().trim().parse::<u32>().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(!crate::sys::pid_alive(pid), "timed-out clipboard helper {pid} was not reaped");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_that_consumes_input_but_never_exits_is_also_timed_out_and_reaped() {
        let path = std::env::temp_dir().join(format!("famulus-clipboard-pid-{}", uuid::Uuid::new_v4()));
        let args = ["-c", "echo $$ > \"$1\"; cat >/dev/null; exec sleep 4", "sh", path.to_str().unwrap()];
        let error = pipe_to("/bin/sh", &args, "echo hello").await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let pid = std::fs::read_to_string(&path).unwrap().trim().parse::<u32>().unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(!crate::sys::pid_alive(pid), "timed-out clipboard helper {pid} was not reaped");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_a_copy_kills_and_reaps_its_helper() {
        let path = std::env::temp_dir().join(format!("famulus-clipboard-pid-{}", uuid::Uuid::new_v4()));
        let helper_path = path.clone();
        let copy = tokio::spawn(async move {
            pipe_to("/bin/sh", &["-c", "echo $$ > \"$1\"; exec sleep 30", "sh", helper_path.to_str().unwrap()], "command").await
        });
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(pid) = text.trim().parse::<u32>() {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        copy.abort();
        assert!(copy.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), async {
            while crate::sys::pid_alive(pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("cancelled clipboard helper was not killed and reaped");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn osc52_does_not_require_tmux_passthrough() {
        assert_eq!(osc52_sequence("hi"), "\x1b]52;c;aGk=\x07");
    }
}
