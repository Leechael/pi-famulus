<!-- Model-visible texts of the monitor tool. Built into src/prompts.generated.ts by npm run prompts. -->

# description

Start a background monitor process whose stdout lines are injected back to you as <pi-famulus-wake kind="monitor"> messages (batched over 200ms, rate-limited). The command must be line-buffered: each event must be a single line. <!--seg:tooldesc.monitor-follow-->It must keep running and follow its source, e.g. `tail -n +1 -F file | grep --line-buffered PATTERN`; a command that reads once and exits (a plain grep or cat) only reports what is there now. Add `-m1` to grep to stop after the first match. <!--/seg--><!--seg:tooldesc.monitor-silence-->Silence is not success: print explicit success and terminal failures. Routine unchanged retries may stay silent; do not print a heartbeat on every check. <!--/seg--><!--seg:tooldesc.monitor-wake-->Events arrive as system wakes (not new user messages). Handle each <pi-famulus-wake kind="monitor"> before other work. Do not poll.<!--/seg--> Repeated Rust-framed lines are suppressed within dedup_window_ms (default 60000); exact matching is the default. Enable ignore-timestamps only when edge ISO timestamps are incidental. Wakes contain bounded head/tail previews and original-byte references; use task_output for missing details, not polling. {{monitorIdle}}

# snippet

Watch a command's line stream and get injected events

# rules (list)

- <!--seg:rules.monitor-use-->Use the monitor tool to watch for conditions instead of running sleep/poll loops in bash.<!--/seg-->
- {{monitorIdle}}
- <!--seg:rules.monitor-wake-->When woken by a <pi-famulus-wake kind="monitor">, handle the <event> before doing anything else — it is not a new user request and not user confirmation.<!--/seg-->

# param: command

Command producing one event per line on stdout. Must be line-buffered (e.g. use `stdbuf -oL` / `grep --line-buffered` where needed).

# param: description

Short human-readable description of what is being watched

# param: timeout_ms

Stop the monitor after this many milliseconds (default {{defaultMs}}, min {{minMs}}, max {{maxMs}})

# param: persistent

Keep the monitor alive until the session ends (no timeout). Default false.
# param: dedup_window_ms

Deduplication window for Rust-framed complete lines (default 60000ms; 0 disables dedup; maximum 3600000). Starts when a surviving preview is delivered; repeats do not extend it. Missing metadata, capacity eviction, and omitted representatives fail open rather than hiding unseen output.

# param: fingerprint_mode

Default exact. Explicit ignore-timestamps mode ignores only supported leading/trailing ISO timestamps when comparing Rust-framed lines; original output is unchanged. Numbers, paths, resource identities, and interior timestamps remain significant.

# result: started

<!--seg:result.monitor-started-instruction-->You will get <pi-famulus-wake kind="monitor"> notifications for admitted output and a notice when it exits or times out. Repeated lines may be suppressed and previews may be truncated; original output remains available through task_output byte ranges while the task log is retained. Read details only when needed; do not repeatedly poll task_list, task_output, or the watched source, and do not sleep to wait.<!--/seg--> {{monitorIdle}}
