# Long-running, quiet monitors

**Date:** 2026-11-02  
**Status:** Discussion in [#107](https://github.com/Leechael/pi-famulus/issues/107). Nothing implemented. Open questions are in §5.  
**Scope (likely):** `extension/src/monitor.ts`, `monitor-output.ts`, `notify.ts`, `extension/prompts/tools/monitor.md`, possibly `manager/` framing (§3.2 option B), `docs/tools.md`, `docs/design.md` §4.4

---

## 1. Problem

The monitor tool is used heavily to watch production logs while shipping features. Two limits get in the way of that use.

### 1.1 Monitors die every 5 minutes

- `timeout_ms` defaults to 300000 (`DEFAULT_TIMEOUT_MS` in `extension/src/monitor.ts`). The max is 3600000. `persistent: true` removes the timeout, but models rarely pass it, so in practice the default wins.
- At expiry, `MonitorRegistry.timeout()` stops the process and sends `[Monitor timed out — re-arm if needed.]` as a normal wake. That starts a turn.
- The model then spends that turn re-creating the same monitor. That costs:
  - **A turn and its tokens**, every 5 minutes, for no new information.
  - **A coverage gap.** Lines written between the stop and the re-arm are missed if the new command is `tail -F` from the end. With `tail -n +1`, the whole file is replayed instead.
  - **Lost dedup state.** The new monitor has a new task id and an empty fingerprint map, so lines that were suppressed come back once.
  - **Churn in `/tasks` and the fleet line.** A deploy watch turns into a chain of `mon_*` ids.

Why the timeout exists: it is the guard against a monitor the model forgot. The leak is already bounded, though. Monitors are tied to the session (zero-connection cleanup in the manager), they show in the fleet line, and `/tasks` can stop them. So the timeout guards against a few minutes to hours of an idle `tail` process, and it costs a turn every 5 minutes for the monitors that are wanted.

### 1.2 Every admitted line wakes the model

- Each batch that passes dedup and the rate limiter becomes a `<pi-famulus-wake kind="monitor">`. When the agent is idle, that triggers a turn (`NotifyCenter.notifyMonitorEvent`).
- For production logs, most output needs no action: routine INFO, healthy requests, retries that recover. The only filter today is the command itself (`grep --line-buffered ERROR`). That works, but:
  - The model must guess the pattern up front. Too wide and every line wakes it. Too narrow and it misses the first odd line.
  - Filtered-out lines are gone from the wake. When an ERROR does wake the agent, the lines that led up to it are not referenced; the model has to go and find them.
  - The tool prompt says "Silence is not success: print explicit success and terminal failures." That is right for a build or a test run. For a long production watch it pushes models to print "still OK" lines, each of which wakes them.
- Dedup (`dedup_window_ms`) only handles identical lines. It does not handle lines that differ but don't matter.

The goal: a monitor can watch for a long time, quietly, and wake the agent only when something needs action, with the quiet output still recorded and easy to reach.

---

## 2. What already exists to build on

- **Passive delivery.** `NotifyCenter.notify(..., { passive: true })` appends a notice without starting a turn (used for a `grep -m1` exit within 2s of an event, and for output of a monitor the model stopped). `quiet` monitor events are coalesced and delivered passively.
- **Byte-range references.** Wakes already carry `cursor-start`/`cursor-end` and a `task_output` hint. Quiet lines can be referenced the same way without being shown.
- **Rust line framing** (`manager/MONITOR_OUTPUT.md`). Rust sees every complete line in full, with fingerprints and byte ranges, before preview truncation. That is the right place to match a pattern against the whole line.
- **TUI channels that are not model context:** toasts, the fleet line and `/tasks` details. These can show "monitor alive, N lines, 0 alerts" to the human without spending model tokens.

---

## 3. Options

### 3.1 Lifetime (problem 1)

**A. Raise the default.** Default `timeout_ms` to the max (1h), or make `persistent` the default. Smallest change. Still wakes the model once an hour, and still loses coverage on re-arm.

**B. Expiry stops quietly.** Keep the timeout but deliver the timeout notice passively (no turn). The model sees it the next time it is woken for another reason. Fixes the turn cost, not the gap: a monitor the user still wanted is now silently gone.

**C. Lease renewed by session activity.** The monitor lives while the session is alive and someone is using it. The timer counts from the last user message or turn in the session, not from monitor start. A monitor dies only after, say, 2h with no activity at all. A deploy watch survives as long as the person is working; a forgotten one in an abandoned session still goes away. The manager's zero-connection cleanup stays the hard backstop.

**D. Explicit end time.** Add `until` (ISO time or duration, for example "until the deploy window closes at 18:00"). The model states the intended lifetime instead of picking a number of ms. Combines with C: `until` overrides the lease.

**E. Session-scoped by default with a count cap.** Drop the time default. Instead, cap concurrent monitors per session (for example 8) and show them clearly in the fleet line. Starting one past the cap fails with a list of the running ones, so the model has to stop one. Bounds the leak by count, not time.

**Leaning:** C + D. The lease removes the 5-minute re-arm without letting monitors outlive their use. `until` covers "watch this for the next 2 hours". Keep the timeout notice as a normal wake only when it was explicit (`until` or `timeout_ms` given). The lease expiring is passive.

### 3.2 Quiet output (problem 2)

**A. Wake filter in the extension.** New params, for example:

```
monitor({ command, description,
          wake_on?: string,     // regex; lines that match wake the agent
          quiet_on?: string })  // regex; lines that match never wake
```

Lines that don't wake are counted and kept as byte ranges. The next wake (or the exit/timeout notice) carries `quiet-lines="1234"` and a range, so `task_output` can read the lead-up on demand. The TS side only has 500-scalar previews, so a match past the preview would be missed.

**B. Wake filter in Rust.** Same params, but the manager evaluates the regex on the full line during framing and marks each record `wake: true|false`. Exact, no preview blind spot, and cheap. Needs a protocol bump (`MonitorOutputRecord` v2 or an optional field) and the `regex` crate in the daemon. Older managers fall back to "everything wakes" (fail open).

**C. Command-side signal.** The command decides: for example stdout lines wake and stderr lines are recorded only, or lines starting with a marker (`::quiet::`) are recorded only. Most flexible (a script can apply any logic), but models have to learn a convention, and plain `tail | grep` loses its stderr.

**D. Digest, not wake.** Quiet lines feed a periodic summary (for example every 10 min: "1.2k lines, 0 matches for wake_on") shown in the TUI only, never in model context. Answers the human's question "is it still watching?" without breaking silence.

**E. Triage by a cheap subagent.** A small model reads quiet batches and decides whether to wake the main agent. Matches the project's "cheap model does the work" idea and catches things a regex can't (a new kind of warning). Costs tokens and latency on every batch, and a wrong "no" hides a real problem. Better as a later, opt-in mode.

**Leaning:** B (A as a fallback when the manager is older), plus D for the human. Revise the prompt: for a long watch, filter with `wake_on`, do not print heartbeat or "still OK" lines; the TUI shows liveness.

### 3.3 Prompt changes regardless of the above

- `tooldesc.monitor-silence` is aimed at builds and tests. Add a line for production watches: silence is the expected state, wake only on lines that need action.
- The timeout instruction "re-arm if needed" invites a re-arm every time. With a lease, it can say what ended it and when.

---

## 4. Risks

- **Silent failure of the watched source.** A quiet monitor whose `tail` lost the file (rotation without `-F`, dead SSH, expired kubectl token) looks the same as a healthy quiet one. Mitigations: wake on monitor exit (already the case); optional `stale_after` that wakes when *no* line at all arrives for N minutes. This is the inverse of the heartbeat: the source should keep talking, the model should not.
- **Over-filtering.** A narrow `wake_on` hides the first instance of a new failure. The quiet-line count and range on every wake make the gap visible, and D gives the human a view.
- **Token growth from passive notices.** Passive notices still enter context. Quiet lines must be counted and referenced, never appended as text.
- **Eval coverage.** The monitor compatibility evals (`eval/`) measure whether models re-arm, poll or reply. New params need new scenarios: does the model pick `wake_on` for a log watch, and does it skip heartbeats.

---

## 5. Open questions

1. Lifetime: is a session-activity lease (C) the right default, or is plain "persistent by default + count cap" (E) simpler and good enough?
2. Should a lease expiry wake the agent at all, or only show a toast and a passive notice?
3. Filter in Rust (exact, protocol change) or TS first (fast to ship, preview blind spot)?
4. Filter shape: `wake_on`/`quiet_on` regex params, or a command-side convention (stderr = quiet), or both?
5. Is `stale_after` (wake when the source goes quiet for too long) wanted in the first cut?
6. Should quiet-line liveness show in the fleet line (`1 monitor · 12k lines · 0 alerts`) or only in `/tasks` details?
7. How should a wake present the lead-up: a `quiet-lines` count and range only, or also the last few quiet lines before the matching line (like `grep -B3`)?
