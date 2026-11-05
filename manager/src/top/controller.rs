//! The `top` session without a screen: state, the collector, the live agent
//! link, copying and stopping. The TUI (`top::run`) and the desktop GUI both
//! drive one of these, so every key and action behaves the same in both.
//!
//! A frontend loops: draw from [`Controller::state`], await either its own
//! input (handed to [`Controller::input`]) or [`Controller::wait`], then call
//! [`Controller::settle`] before drawing again.

use super::collector::{CollectView, Collector, Tick};
use super::live::{LiveCmd, LiveMsg};
use super::model::TopState;
use super::{apply_latest, apply_live, dispatch, sync_live, CollectorCmd, CopyJob, Input, Lease};
use crate::client;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

pub struct Controller {
    state: TopState,
    home: PathBuf,
    cmd_tx: mpsc::UnboundedSender<CollectorCmd>,
    tick_rx: mpsc::Receiver<Result<Tick, String>>,
    live_tx: mpsc::UnboundedSender<LiveMsg>,
    live_rx: mpsc::UnboundedReceiver<LiveMsg>,
    live_slot: Option<(String, mpsc::UnboundedSender<LiveCmd>)>,
    copy_job: Option<CopyJob>,
    collector: tokio::task::JoinHandle<()>,
    _lease: Lease,
}

impl Controller {
    /// Starts the collector and the supervision lease; needs a tokio
    /// runtime. `focus` opens that node's detail once it appears.
    pub fn start(
        home: &Path,
        interval_ms: u64,
        session: Option<String>,
        cwd: Option<String>,
        focus: Option<String>,
    ) -> Controller {
        let mut state = TopState::new(interval_ms, session, cwd);
        state.focus = focus;
        state.message = Some("loading…".into());

        // The live view is `serve` with a screen: it supervises the manager
        // until it exits.
        let lease_home = home.to_path_buf();
        let lease = Lease(tokio::spawn(async move {
            client::hold_supervision(&lease_home, |_| {}).await;
        }));

        let (live_tx, live_rx) = mpsc::unbounded_channel::<LiveMsg>();
        let (tick_tx, tick_rx) = mpsc::channel::<Result<Tick, String>>(1);
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<CollectorCmd>();
        let home_buf = home.to_path_buf();
        let start_interval = state.interval_ms.max(100);
        let start_view = CollectView::from_state(&state);
        let collector = tokio::spawn(async move {
            let mut collector = Collector::new(home_buf);
            let mut interval = tokio::time::interval(Duration::from_millis(start_interval));
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut paused = false;
            let mut view = start_view;
            loop {
                tokio::select! {
                    _ = interval.tick(), if !paused => {
                        let _ = tick_tx.send(collector.tick(&view).await).await;
                        if collector.disk_stale() {
                            collector.refresh_disk().await;
                            let _ = tick_tx.send(collector.tick(&view).await).await;
                        }
                    }
                    Some(cmd) = cmd_rx.recv() => match cmd {
                        CollectorCmd::View(v) => view = v,
                        CollectorCmd::SetInterval(ms) => {
                            interval = tokio::time::interval(Duration::from_millis(ms.max(100)));
                            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
                        }
                        CollectorCmd::Pause(p) => paused = p,
                        CollectorCmd::Refresh => {
                            let _ = tick_tx.send(collector.tick(&view).await).await;
                        }
                        CollectorCmd::Tails => {
                            let _ = tick_tx.send(Ok(collector.tails(&view))).await;
                        }
                    },
                    else => break,
                }
            }
        });

        Controller {
            state,
            home: home.to_path_buf(),
            cmd_tx,
            tick_rx,
            live_tx,
            live_rx,
            live_slot: None,
            copy_job: None,
            collector,
            _lease: lease,
        }
    }

    pub fn state(&self) -> &TopState {
        &self.state
    }

    /// For the frontend's own drawing bookkeeping (scroll offsets, hit
    /// rectangles); behavior changes go through [`Controller::input`].
    pub fn state_mut(&mut self) -> &mut TopState {
        &mut self.state
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Handles one input. `Ok(true)` means quit.
    pub async fn input(&mut self, input: Input) -> Result<bool, String> {
        dispatch(&mut self.state, input, &self.home, &self.cmd_tx).await
    }

    /// Waits for the next thing that changes the screen without input: a
    /// collector tick, a live agent message, a finished copy, or (while
    /// something visible animates) the next spinner frame.
    pub async fn wait(&mut self) {
        let anim = self.state.wants_anim();
        tokio::select! {
            Some(res) = self.tick_rx.recv() => apply_latest(&mut self.state, &mut self.tick_rx, res),
            Some(msg) = self.live_rx.recv() => apply_live(&mut self.state, msg),
            result = async { (&mut self.copy_job.as_mut().unwrap().task).await }, if self.copy_job.is_some() => {
                let job = self.copy_job.take().unwrap();
                let notice = result.unwrap_or_else(|e| format!("copy failed: {e}"));
                job.apply(&mut self.state, notice);
            }
            _ = tokio::time::sleep(Duration::from_millis(80)), if anim => {}
        }
    }

    /// Applies whatever else arrived and starts the work inputs asked for
    /// (copies, the live agent link, messages to it). Call before drawing.
    pub fn settle(&mut self) {
        while let Ok(res) = self.tick_rx.try_recv() {
            apply_latest(&mut self.state, &mut self.tick_rx, res);
        }
        while let Ok(msg) = self.live_rx.try_recv() {
            apply_live(&mut self.state, msg);
        }
        if let Some(request) = self.state.pending_copy.take() {
            self.copy_job = Some(CopyJob::start(request));
        }
        sync_live(&mut self.state, &self.home, &mut self.live_slot, &self.live_tx);
        for (op, text) in std::mem::take(&mut self.state.pending_live) {
            let sent = self
                .live_slot
                .as_ref()
                .is_some_and(|(_, tx)| tx.send(LiveCmd::Send { op, text: text.clone() }).is_ok());
            if !sent {
                if let Some(composer) = self.state.composer.as_mut() {
                    if composer.input.is_empty() {
                        composer.input = text;
                    } else {
                        composer.input.push('\n');
                        composer.input.push_str(&text);
                    }
                }
                self.state.message = Some("the live session is gone; the message was put back".into());
            }
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        if let Some((_, tx)) = self.live_slot.take() {
            let _ = tx.send(LiveCmd::Stop);
        }
        self.collector.abort();
    }
}

