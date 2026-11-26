//! One window per user. A second launch asks the first to show its window
//! and exits.
//!
//! macOS already does this for an app bundle opened from the Finder or with
//! `open`, but not for the binary run directly; NSH 0.4 has no option for
//! it and creates its GTK application `NON_UNIQUE`. So the app does it
//! itself, in two files in a per-user directory ([`dir`]):
//!
//! - `instance.lock`, on which the first instance holds an OS file lock. The
//!   lock ends with the process, so a crashed instance never blocks the next
//!   launch; the file itself may stay.
//! - `instance.sock` (Unix), where the first instance listens. A second
//!   launch finds the lock held, sends `show` there and exits.
//!
//! The instance is per user, not per pi-famulus home: a second launch with
//! another `PI_FAMULUS_HOME` shows the window that is already open, on the
//! home it was started with.

use std::fs::{File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use tokio::sync::watch;

const LOCK_FILE: &str = "instance.lock";
#[cfg(unix)]
const SOCK_FILE: &str = "instance.sock";
/// How long a second launch waits for the first to answer: it may be between
/// taking the lock and listening.
const ANSWER_WAIT: Duration = Duration::from_secs(3);

/// The running instance. Dropping it lets the next launch be the first.
pub struct Instance {
    _lock: File,
    #[cfg(unix)]
    listener: unix::Listening,
}

pub enum Claim {
    /// No other instance runs: this is it, until the value is dropped.
    First(Instance),
    /// Another instance holds the lock. `shown`: it confirmed the request
    /// to show its window.
    Second { shown: bool },
}

/// The per-user directory of the lock and socket; `None` when the
/// environment names no place for one.
pub fn dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(target_os = "macos") {
        Some(var("HOME")?.join("Library/Caches").join(crate::APP_ID))
    } else if cfg!(windows) {
        Some(var("LOCALAPPDATA")?.join(crate::APP_ID))
    } else {
        var("XDG_RUNTIME_DIR")
            .or_else(|| Some(var("HOME")?.join(".cache")))
            .map(|base| base.join(crate::APP_ID))
    }
}

/// Becomes the instance of `dir`, or asks the one that is to show its
/// window. `on_show` runs in the first instance, on a thread of its own,
/// each time a later launch asks.
pub fn claim(dir: &Path, on_show: impl Fn() + Send + 'static) -> io::Result<Claim> {
    claim_within(dir, on_show, ANSWER_WAIT)
}

fn claim_within(dir: &Path, on_show: impl Fn() + Send + 'static, wait: Duration) -> io::Result<Claim> {
    create_private_dir(dir)?;
    let lock = File::options().create(true).truncate(false).write(true).open(dir.join(LOCK_FILE))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(Claim::Second { shown: ask_to_show(dir, wait) }),
        Err(TryLockError::Error(e)) => return Err(e),
    }
    #[cfg(unix)]
    let listener = unix::listen(&dir.join(SOCK_FILE), on_show)?;
    #[cfg(not(unix))]
    let _ = on_show;
    Ok(Claim::First(Instance {
        _lock: lock,
        #[cfg(unix)]
        listener,
    }))
}

#[cfg(unix)]
impl Drop for Instance {
    fn drop(&mut self) {
        self.listener.stop();
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

#[cfg(unix)]
fn ask_to_show(dir: &Path, wait: Duration) -> bool {
    unix::ask(&dir.join(SOCK_FILE), wait)
}

/// Without a socket the second launch only steps aside.
#[cfg(not(unix))]
fn ask_to_show(_dir: &Path, _wait: Duration) -> bool {
    false
}

#[cfg(unix)]
mod unix {
    use std::io::{self, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const SHOW: &[u8] = b"show\n";
    const SHOWN: &[u8] = b"ok\n";
    const IO_WAIT: Duration = Duration::from_secs(2);

    pub struct Listening {
        path: PathBuf,
        stopped: Arc<AtomicBool>,
    }

    impl Listening {
        /// Ends the accept loop and removes the socket.
        pub fn stop(&self) {
            self.stopped.store(true, Ordering::SeqCst);
            // Wake the accept loop so it sees the flag.
            let _ = UnixStream::connect(&self.path);
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Listens at `path`. Only the lock holder calls this, so a socket file
    /// already there is a dead instance's and is replaced.
    pub fn listen(path: &Path, on_show: impl Fn() + Send + 'static) -> io::Result<Listening> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        std::thread::Builder::new().name("single-instance".into()).spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(IO_WAIT));
                let _ = stream.set_write_timeout(Some(IO_WAIT));
                let mut request = [0u8; SHOW.len()];
                if stream.read_exact(&mut request).is_ok() && request == SHOW {
                    on_show();
                    let _ = stream.write_all(SHOWN);
                }
            }
        })?;
        Ok(Listening { path: path.to_path_buf(), stopped })
    }

    /// Asks the instance listening at `path` to show its window; whether it
    /// confirmed within `wait`.
    pub fn ask(path: &Path, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            if let Ok(mut stream) = UnixStream::connect(path) {
                let _ = stream.set_read_timeout(Some(IO_WAIT));
                let _ = stream.set_write_timeout(Some(IO_WAIT));
                let mut reply = [0u8; SHOWN.len()];
                if stream.write_all(SHOW).is_ok() && stream.read_exact(&mut reply).is_ok() && reply == SHOWN {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

// ── this process ────────────────────────────────────────────────────────────

static THIS: OnceLock<Option<Instance>> = OnceLock::new();
static SHOW_REQUESTS: OnceLock<watch::Sender<u64>> = OnceLock::new();

fn show_requests() -> &'static watch::Sender<u64> {
    SHOW_REQUESTS.get_or_init(|| watch::channel(0).0)
}

/// Counts the requests to show the window; the root component watches it
/// and shows the window on each change.
pub fn show_requested() -> watch::Receiver<u64> {
    show_requests().subscribe()
}

/// Makes this process the user's one instance, or, when another already
/// runs, asks that one to show its window and exits. Called before the
/// window exists (`main`) and again by the root component, for hosts that
/// do not run this crate's `main`; only the first call does anything.
pub fn ensure_single() {
    THIS.get_or_init(|| {
        let Some(dir) = dir() else {
            log::warn!("single instance: no per-user directory; not enforced");
            return None;
        };
        match claim(&dir, || show_requests().send_modify(|n| *n += 1)) {
            Ok(Claim::First(instance)) => Some(instance),
            Ok(Claim::Second { shown }) => {
                if !shown {
                    eprintln!("pi-famulus is already running, but it did not answer the request to show its window");
                }
                std::process::exit(i32::from(!shown));
            }
            Err(e) => {
                log::warn!("single instance: {}: {e}; not enforced", dir.display());
                None
            }
        }
    });
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Dir {
            // Short: a Unix socket path is limited to about 100 bytes.
            let path = std::env::temp_dir().join(format!("pfg-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            Dir(path)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn counter() -> (Arc<AtomicUsize>, impl Fn() + Send + 'static) {
        let shows = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&shows);
        (shows, move || {
            count.fetch_add(1, Ordering::SeqCst);
        })
    }

    fn first(claim: io::Result<Claim>) -> Instance {
        match claim.unwrap() {
            Claim::First(instance) => instance,
            Claim::Second { .. } => panic!("another instance holds the lock"),
        }
    }

    #[test]
    fn a_second_launch_shows_the_first_and_steps_aside_until_the_first_is_gone() {
        let dir = Dir::new("second");
        let (shows, on_show) = counter();
        let instance = first(claim(&dir.0, on_show));
        assert_eq!(shows.load(Ordering::SeqCst), 0);

        for launch in 1..=2 {
            let (never, on_show) = counter();
            match claim(&dir.0, on_show).unwrap() {
                Claim::Second { shown } => assert!(shown, "the first instance confirmed the request"),
                Claim::First(_) => panic!("launch {launch} became a second instance beside the first"),
            }
            assert_eq!(shows.load(Ordering::SeqCst), launch, "the first instance was asked to show its window");
            assert_eq!(never.load(Ordering::SeqCst), 0, "the second launch's own callback never runs");
        }

        drop(instance);
        assert!(!dir.0.join(SOCK_FILE).exists(), "the socket goes with the instance");
        let (shows, on_show) = counter();
        let next = first(claim(&dir.0, on_show));
        match claim(&dir.0, || {}).unwrap() {
            Claim::Second { shown } => assert!(shown),
            Claim::First(_) => panic!("two instances at once"),
        }
        assert_eq!(shows.load(Ordering::SeqCst), 1, "the new first instance answers now");
        drop(next);
    }

    #[test]
    fn what_a_crashed_instance_left_behind_does_not_block_the_next_launch() {
        let dir = Dir::new("stale");
        create_private_dir(&dir.0).unwrap();
        // A crash leaves the lock file (unlocked: the lock died with the
        // process) and a socket file nobody listens on.
        std::fs::write(dir.0.join(LOCK_FILE), b"").unwrap();
        drop(std::os::unix::net::UnixListener::bind(dir.0.join(SOCK_FILE)).unwrap());
        assert!(dir.0.join(SOCK_FILE).exists());

        let (shows, on_show) = counter();
        let instance = first(claim(&dir.0, on_show));
        match claim(&dir.0, || {}).unwrap() {
            Claim::Second { shown } => assert!(shown, "the stale socket was replaced by a live one"),
            Claim::First(_) => panic!("two instances at once"),
        }
        assert_eq!(shows.load(Ordering::SeqCst), 1);
        drop(instance);
    }

    #[test]
    fn a_first_instance_that_does_not_answer_still_keeps_the_second_out() {
        let dir = Dir::new("mute");
        create_private_dir(&dir.0).unwrap();
        // The lock is held, but nothing listens (an instance that is stuck).
        let held = File::options().create(true).truncate(false).write(true).open(dir.0.join(LOCK_FILE)).unwrap();
        held.try_lock().unwrap();
        match claim_within(&dir.0, || {}, Duration::from_millis(200)).unwrap() {
            Claim::Second { shown } => assert!(!shown, "nobody confirmed"),
            Claim::First(_) => panic!("the lock was held"),
        }
        drop(held);
        drop(first(claim(&dir.0, || {})));
    }

    #[cfg(unix)]
    #[test]
    fn the_directory_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new("mode");
        drop(first(claim(&dir.0, || {})));
        assert_eq!(std::fs::metadata(&dir.0).unwrap().permissions().mode() & 0o777, 0o700);
    }
}
