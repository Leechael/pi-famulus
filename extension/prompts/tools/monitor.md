<!-- Model-visible texts of the monitor tool. Built into src/prompts.generated.ts by npm run prompts. -->

# description

Start a background monitor process whose stdout lines are injected back to you as <pi-famulus-wake kind="monitor"> messages (batched over 200ms, rate-limited). The command must be line-buffered: each event must be a single line. <!--seg:tooldesc.monitor-follow-->It must keep running and follow its source, e.g. `tail -n +1 -F file | grep --line-buffered PATTERN`; a command that reads once and exits (a plain grep or cat) only reports what is there now. Add `-m1` to grep to stop after the first match. <!--/seg--><!--seg:tooldesc.monitor-silence-->Silence is not success: write the command so failures also produce lines (e.g. grep for both success and error patterns). <!--/seg--><!--seg:tooldesc.monitor-wake-->Events arrive as system wakes (not new user messages). Handle each <pi-famulus-wake kind="monitor"> before other work. Do not poll.<!--/seg--> {{monitorIdle}}

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
# result: started

<!--seg:result.monitor-started-instruction-->You will get a <pi-famulus-wake kind="monitor"> for each event, and a notice when it exits or times out. Do not poll it (task_list, task_output, or reading what it watches) or sleep.<!--/seg--> {{monitorIdle}}
