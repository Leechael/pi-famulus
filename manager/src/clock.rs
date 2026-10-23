//! The daemon's time source for its own timers: the 5s idle grace, the 2s
//! SIGKILL escalation (stop reaper and graceful shutdown), and the fallback
//! leftover-group poll.
//!
//! Normal builds: a thin wrapper over `tokio::time::sleep`.
//!
//! With the `test-clock` cargo feature and `PI_FAMULUS_TEST_CLOCK=manual` in the
//! daemon's environment, these timers run on a manual clock instead: virtual
//! time starts at 0 and only moves when a client sends the debug request
//! `clock_advance {ms}` (see `daemon::handle_clock`). Everything else stays
//! on real time: child processes, `timeout_ms`, the hello timeout, record
//! timestamps. Black-box tests can then step through the 5s/2s lifecycle
//! deterministically, without waiting for it.
//!
//! Every sleep carries a label. `clock_status` lists the pending ones, so a
//! test first waits until the timer it means is armed, then advances. This
//! avoids the race of advancing before the daemon has scheduled the timer.
//!
//! Why not tokio's test-util (`pause`/`advance`)? It only works inside one
//! current-thread runtime, which the black-box tests cannot reach. Its
//! auto-advance also jumps timers while the runtime waits on real child
//! processes, which would make the kill grace fire instantly.

use std::time::Duration;

#[cfg(feature = "test-clock")]
pub use manual::ClockStatus;

#[derive(Clone, Default)]
pub struct Clock {
    #[cfg(feature = "test-clock")]
    manual: Option<std::sync::Arc<manual::Manual>>,
}

impl Clock {
    /// The clock the daemon should use: manual only when built with
    /// `test-clock` *and* started with `PI_FAMULUS_TEST_CLOCK=manual`.
    pub fn from_env() -> Clock {
        #[cfg(feature = "test-clock")]
        {
            if std::env::var("PI_FAMULUS_TEST_CLOCK").as_deref() == Ok("manual") {
                return Clock {
                    manual: Some(std::sync::Arc::new(manual::Manual::default())),
                };
            }
        }
        Clock::default()
    }

    #[cfg(all(test, feature = "test-clock"))]
    pub(crate) fn manual_for_test() -> Clock {
        Clock {
            manual: Some(std::sync::Arc::new(manual::Manual::default())),
        }
    }

    /// Sleep for `d` on this clock. `label` names the timer in
    /// `clock_status` (manual clock only).
    #[allow(unused_variables)]
    pub async fn sleep(&self, label: &'static str, d: Duration) {
        #[cfg(feature = "test-clock")]
        if let Some(m) = &self.manual {
            return m.sleep(label, d).await;
        }
        tokio::time::sleep(d).await
    }

    /// Current time on this clock, in ms: wall time (epoch) normally, the
    /// virtual time on the manual clock. Deadlines of this clock's timers
    /// (e.g. a stop's kill grace) are kept in these units.
    pub fn now_ms(&self) -> u64 {
        #[cfg(feature = "test-clock")]
        if let Some(m) = &self.manual {
            return m.now_ms();
        }
        crate::proto::now_ms()
    }

    /// Continue the manual clock's virtual time after an in-place upgrade
    /// (the new image starts a fresh clock). No-op on the real clock.
    #[allow(unused_variables)]
    #[cfg_attr(windows, allow(dead_code))]
    pub fn resume_at(&self, now_ms: u64) {
        #[cfg(feature = "test-clock")]
        if let Some(m) = &self.manual {
            m.set_now_ms(now_ms);
        }
    }

    #[cfg(feature = "test-clock")]
    pub fn manual(&self) -> Option<&manual::Manual> {
        self.manual.as_deref()
    }
}

#[cfg(feature = "test-clock")]
mod manual {
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::Notify;

    #[derive(Default)]
    pub struct Manual {
        state: Mutex<State>,
    }

    #[derive(Default)]
    struct State {
        now_ms: u64,
        next_id: u64,
        sleepers: BTreeMap<u64, Sleeper>,
    }

    struct Sleeper {
        label: &'static str,
        deadline_ms: u64,
        wake: Arc<Notify>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PendingTimer {
        pub label: String,
        pub due_in_ms: u64,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ClockStatus {
        pub now_ms: u64,
        pub pending: Vec<PendingTimer>,
    }

    /// Removes the sleeper when the sleeping future completes or is dropped
    /// (e.g. the idle timer task aborted by a new hello).
    struct Registration<'a> {
        clock: &'a Manual,
        id: u64,
    }

    impl Drop for Registration<'_> {
        fn drop(&mut self) {
            self.clock.state.lock().unwrap().sleepers.remove(&self.id);
        }
    }

    impl Manual {
        pub async fn sleep(&self, label: &'static str, d: Duration) {
            let wake = Arc::new(Notify::new());
            let (id, deadline) = {
                let mut st = self.state.lock().unwrap();
                let id = st.next_id;
                st.next_id += 1;
                let deadline = st.now_ms + d.as_millis() as u64;
                st.sleepers.insert(
                    id,
                    Sleeper {
                        label,
                        deadline_ms: deadline,
                        wake: wake.clone(),
                    },
                );
                (id, deadline)
            };
            let _reg = Registration { clock: self, id };
            loop {
                // Arm the notification before checking, so an advance
                // between the check and the await is not lost.
                let notified = wake.notified();
                if self.state.lock().unwrap().now_ms >= deadline {
                    return;
                }
                notified.await;
            }
        }

        pub fn now_ms(&self) -> u64 {
            self.state.lock().unwrap().now_ms
        }

        #[cfg_attr(windows, allow(dead_code))]
        pub fn set_now_ms(&self, ms: u64) {
            self.state.lock().unwrap().now_ms = ms;
        }

        /// Move virtual time forward and wake every timer now due.
        pub fn advance(&self, ms: u64) -> ClockStatus {
            let due: Vec<Arc<Notify>> = {
                let mut st = self.state.lock().unwrap();
                st.now_ms += ms;
                let now = st.now_ms;
                st.sleepers
                    .values()
                    .filter(|s| s.deadline_ms <= now)
                    .map(|s| s.wake.clone())
                    .collect()
            };
            for w in due {
                w.notify_one();
            }
            self.status()
        }

        pub fn status(&self) -> ClockStatus {
            let st = self.state.lock().unwrap();
            ClockStatus {
                now_ms: st.now_ms,
                pending: st
                    .sleepers
                    .values()
                    .map(|s| PendingTimer {
                        label: s.label.to_string(),
                        due_in_ms: s.deadline_ms.saturating_sub(st.now_ms),
                    })
                    .collect(),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn sleepers_wake_only_when_virtual_time_passes() {
            let m = Arc::new(Manual::default());
            let m2 = m.clone();
            let t = tokio::spawn(async move { m2.sleep("idle", Duration::from_secs(5)).await });
            while m.status().pending.is_empty() {
                tokio::task::yield_now().await;
            }
            assert_eq!(m.status().pending[0].label, "idle");
            assert_eq!(m.status().pending[0].due_in_ms, 5000);
            m.advance(4999);
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(!t.is_finished(), "woke before its deadline");
            assert_eq!(m.status().pending[0].due_in_ms, 1);
            m.advance(1);
            tokio::time::timeout(Duration::from_secs(1), t).await.unwrap().unwrap();
            assert!(m.status().pending.is_empty(), "finished sleepers are removed");
        }

        #[tokio::test]
        async fn dropped_sleepers_are_removed() {
            let m = Arc::new(Manual::default());
            let m2 = m.clone();
            let t = tokio::spawn(async move { m2.sleep("idle", Duration::from_secs(5)).await });
            while m.status().pending.is_empty() {
                tokio::task::yield_now().await;
            }
            t.abort();
            let _ = t.await;
            assert!(m.status().pending.is_empty());
        }
    }
}
