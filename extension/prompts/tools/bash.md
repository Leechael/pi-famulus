<!-- Model-visible texts of the bash tool. Built into src/prompts.generated.ts by npm run prompts. -->

# description

Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last {{maxLines}} lines or {{maxKb}}KB (whichever is hit first). <!--seg:tooldesc.bash-autobg-->Foreground commands that exceed the foreground budget are automatically moved to the background; you will be notified when they complete. <!--/seg-->Optionally provide a timeout in seconds (hard kill limit), or run_in_background to background immediately.

# snippet

Execute bash commands (ls, grep, find, etc.)

# rules (list)

- <!--seg:rules.pi-env-->You can inspect PI_* environment variables for current model and session details.<!--/seg-->
- <!--seg:rules.bash-no-poll-->Long-running bash commands are moved to the background automatically; do not poll or sleep to wait for them.<!--/seg--> End your turn (a reply with no tool call) and resume from the task wake when it arrives.

# param: command

The bash command to execute

# param: timeout

Hard kill timeout in seconds (optional, no default timeout)

# param: run_in_background

Start the command in the background and return immediately. You will be notified when it completes.
# result: backgrounded

Command "{{command}}" moved to background (task_id: {{taskId}}). Output: {{outputPath}}.<!--seg:result.bash-bg-instruction-->
You will be notified when it completes, even if other commands are still running. Do not poll or sleep: reply to the user now with no tool call, and continue from the <pi-famulus-wake kind="task"> when it arrives.<!--/seg-->

# error: bare-sleep

Refusing to run a bare sleep/idle-loop command: {{command}}.<!--seg:result.sleep-block-advice--> Do not sleep to wait for background work; completion is delivered via notification. Use the monitor tool to watch for a condition, or run_in_background for long commands.<!--/seg-->
