//! A frontend that links the library but is not the `pi-famulus` binary (the
//! desktop GUI; here, this test binary) never runs its own executable as the
//! daemon: that executable does not understand `daemon`, so "starting the
//! daemon" would launch another copy of the program, again on every retry.
//! It attaches to a manager that runs, and starts one only from the real
//! binary it declared.

#![cfg(unix)]

mod common;

use common::*;
use pi_famulus::daemon_binary::{self, DaemonBinary};
use pi_famulus::top::controller::Controller;
use pi_famulus::top::model::TopState;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Drives the controller as a frontend does until `done` holds or `limit`
/// passes; returns whether it held.
async fn drive(ctl: &mut Controller, limit: Duration, done: impl Fn(&TopState) -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        ctl.settle();
        if done(ctl.state()) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        let _ = tokio::time::timeout(Duration::from_millis(100), ctl.wait()).await;
    }
}

/// Long enough for several supervision attempts (one a second) and many
/// collector ticks; then nothing was started for `home`.
async fn assert_nothing_started(ctl: &mut Controller, home: &Home, why: &str) {
    drive(ctl, Duration::from_secs(3), |_| false).await;
    assert!(!ctl.state().header.live, "{why}: nothing is running");
    // `spawn_daemon` opens manager.log before it launches anything, so its
    // absence shows no start was even attempted.
    assert!(
        !home.path.join("manager.log").exists(),
        "{why}: a daemon start was attempted"
    );
    assert!(
        !home.path.join("manager.spawn.lock").exists(),
        "{why}: the spawn lock was taken"
    );
    assert!(
        !home.sock().exists() && home.pidfile_pid().is_none(),
        "{why}"
    );
    assert!(
        daemon_pids_for(&home.path).is_empty(),
        "{why}: a process was started for this home"
    );
}

fn live(state: &TopState) -> bool {
    state.header.live && state.header.pid.is_some()
}

// One test: the declaration is per process, so the steps run in order.
#[test]
fn a_library_frontend_never_runs_itself_as_the_daemon() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // Nothing declared: attach only, and connect to a manager that
        // something else starts later.
        let home = Home::new_real("attach-only");
        let mut ctl = Controller::start(&home.path, 100, None, None, None);
        assert_nothing_started(&mut ctl, &home, "no daemon binary declared").await;
        let _daemon = tokio::task::block_in_place(|| home.start_daemon());
        assert!(
            drive(&mut ctl, Duration::from_secs(20), live).await,
            "the frontend did not connect to the manager that started later"
        );
        assert_eq!(
            ctl.state().header.pid,
            home.pidfile_pid(),
            "it shows that manager"
        );
        drop(ctl);

        // Declaring this very executable is refused, through a symlink too.
        let home = Home::new_real("attach-self");
        let this = std::env::current_exe().unwrap();
        let link: PathBuf = home.path.join("link-to-this-program");
        std::os::unix::fs::symlink(&this, &link).unwrap();
        for own in [this, link] {
            daemon_binary::declare(Some(DaemonBinary::At(own)));
            let mut ctl = Controller::start(&home.path, 100, None, None, None);
            assert_nothing_started(&mut ctl, &home, "its own executable declared").await;
        }

        // The real binary declared: the frontend starts exactly one manager
        // for the home, as `pi-famulus top` does, and attaches to it.
        daemon_binary::declare(Some(DaemonBinary::At(PathBuf::from(BIN))));
        let mut ctl = Controller::start(&home.path, 100, None, None, None);
        assert!(
            drive(&mut ctl, Duration::from_secs(30), live).await,
            "the frontend did not start the declared daemon binary; manager.log: {}",
            std::fs::read_to_string(home.path.join("manager.log")).unwrap_or_default()
        );
        let started = home.pidfile_pid().expect("the started manager's pid file");
        assert_eq!(ctl.state().header.pid, Some(started));
        // A second frontend on the same home attaches to that manager.
        let mut second = Controller::start(&home.path, 100, None, None, None);
        assert!(drive(&mut second, Duration::from_secs(20), live).await);
        drive(&mut ctl, Duration::from_secs(2), |_| false).await;
        assert_eq!(
            second.state().header.pid,
            Some(started),
            "both show the one manager"
        );
        assert_eq!(
            home.pidfile_pid(),
            Some(started),
            "no successor replaced it"
        );
        assert_eq!(
            daemon_pids_for(&home.path),
            vec![started],
            "exactly one daemon for the home"
        );
        daemon_binary::declare(None);
    });
}
