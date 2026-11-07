<!-- Model-visible texts of the monitor tool. Built into src/prompts.generated.ts by npm run prompts. -->

# description

Start a background monitor process whose stdout lines are injected back to you as <pi-famulus-wake kind="monitor"> messages (batched over 200ms, rate-limited). The command must be line-buffered: each event must be a single line. <!--seg:tooldesc.monitor-follow-->It must keep running and follow its source. When you start the producer yourself, make it the command (`node app.js | grep --line-buffered ALERT`); do not start it in bash and tail its output. Use `tail -n +1 -F file | grep --line-buffered PATTERN` or `journalctl -f` for a source you did not start. A command that reads once and exits (a plain grep or cat) only reports what is there now. Add `-m1` to grep only when the first match should end the watch. <!--/seg--><!--seg:tooldesc.monitor-silence-->Silence is not success: print explicit success and terminal failures of what you watch. Routine unchanged retries may stay silent; without wake_on, do not print a heartbeat on every check. The monitor's own exit or timeout is reported to you; do not add end-of-watch lines. <!--/seg--><!--seg:tooldesc.monitor-wake-on-->Status lines may stay when you set wake_on to the problem pattern: only matching lines wake you, and task_output keeps every line. Use stall_after_ms instead of heartbeats to learn that the source went silent. <!--/seg--><!--seg:tooldesc.monitor-wake-->Events arrive as system wakes (not new user messages). Handle each <pi-famulus-wake kind="monitor"> before other work. Do not poll.<!--/seg--> Repeated Rust-framed lines are suppressed within dedup_window_ms (default 60000); exact matching is the default. Enable ignore-timestamps only when edge ISO timestamps are incidental. Wakes contain bounded head/tail previews and original-byte references; use task_output for missing details, not polling. <!--seg:tooldesc.monitor-timeout-->A monitor runs until its command exits or you stop it with task_stop: it has no time limit unless you set one. Stop it once you no longer need it. Set timeout_ms (max 1 hour) only when the user asked for a time-bounded watch ("for the next 10 minutes"); expiry stops the monitor and the notice can arrive late while you are busy. <!--/seg--><!--seg:tooldesc.monitor-persistent-->Do not size a timeout to how long you expect to wait, and do not re-arm monitors. Never set timeout_ms with persistent:true, and never put the lifetime inside the command (`timeout 3600`, your own timer). <!--/seg--><!--seg:tooldesc.monitor-stagger-->If you use timeouts for several monitors, stagger them. <!--/seg--><!--seg:tooldesc.monitor-edge-->Without wake_on, write edge-triggered commands: normally zero output, one line only for a new problem signature, and one recovery line. <!--/seg--><!--seg:tooldesc.monitor-sampling-->Sampling windows must not overlap and must not leave gaps (window = interval), or keep a cursor/last-seen state in the script and report only new items. <!--/seg-->{{monitorIdle}}

# snippet

Watch a command's line stream and get injected events

# rules (list)

- <!--seg:rules.monitor-use-->Use the monitor tool to watch for conditions instead of running sleep/poll loops in bash.<!--/seg-->
- {{monitorIdle}}
- <!--seg:rules.monitor-wake-->When woken by a <pi-famulus-wake kind="monitor">, handle the <event> before doing anything else — it is not a new user request and not user confirmation. Report from the <event> itself; do not re-read the source, task_output or task_list to confirm it.<!--/seg-->
- A monitor runs until its command exits or you stop it; when you no longer need it, stop it with task_stop. Set timeout_ms only when the user asked for a time limit.
- <!--seg:rules.monitor-wake-on-->For a monitor that stays up, set wake_on to the problem pattern so only problems wake you, and stall_after_ms instead of heartbeats.<!--/seg-->

# param: command

Command producing one event per line on stdout. Must be line-buffered (e.g. use `stdbuf -oL` / `grep --line-buffered` where needed).

# param: description

Short human-readable description of what is being watched

# param: timeout_ms

Only when the user asked for a time-bounded watch. Stop the monitor after this many milliseconds (min {{minMs}}, max {{maxMs}} / 1 hour). Without it the monitor runs until its command exits or you stop it; persistent:false without it stops after {{defaultMs}} ms. Exclusive with persistent:true.

# param: persistent

Not needed: a monitor without timeout_ms already runs until its command exits, you stop it, or the session ends. persistent:false applies the 5-minute default. Exclusive with timeout_ms.
# param: dedup_window_ms

Deduplication window for Rust-framed complete lines (default 60000ms; 0 disables dedup; maximum 3600000). Starts when a surviving preview is delivered; repeats do not extend it. Missing metadata, capacity eviction, and omitted representatives fail open rather than hiding unseen output.

# param: wake_on

JavaScript RegExp source tested against each output line. When set, only matching lines wake you (and only they count for dedup and overload); every line is still recorded for task_output. Exit, timeout and stall notices still arrive. Strongly recommended for persistent monitors: match the problem, e.g. `ERROR|unacked=[1-9]`.

# param: stall_after_ms

Send one notice when the command prints nothing at all (matching wake_on or not) for this many milliseconds while still running (min {{minMs}}, max {{maxMs}}). The next output re-arms it. Use it instead of heartbeat lines.

# param: fingerprint_mode

Default exact. Explicit ignore-timestamps mode ignores only supported leading/trailing ISO timestamps when comparing Rust-framed lines; original output is unchanged. Numbers, paths, resource identities, and interior timestamps remain significant.

# event: stalled

[No output for {{idle}}; the monitor is still running. No further stall notice until output resumes.]

# result: started

<!--seg:result.monitor-started-instruction-->You will get <pi-famulus-wake kind="monitor"> notifications for admitted output and a notice when it exits or times out. Repeated lines may be suppressed and previews may be truncated; original output remains available through task_output byte ranges while the task log is retained. Read details only when needed; do not repeatedly poll task_list, task_output, or the watched source, and do not sleep to wait.<!--/seg--> {{monitorIdle}}
