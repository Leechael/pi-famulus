<!-- Texts of harness guards that stop a run instead of a tool. Built into src/prompts.generated.ts by npm run prompts. -->

# error: idle-turn

Not run. pi-famulus ended this run: {{count}} turns in a row made only no-op or status calls ({{calls}}). Such calls do not wait for anything or yield control. Background tasks, monitors and messages from other sessions resume you with a new message when there is news. When no actionable work remains, reply with no tool call.

# error: idle-run

Not run. pi-famulus ended this run: {{count}} turns in this run made only no-op or status calls ({{calls}}). Such calls do not wait for anything or yield control. Background tasks, monitors and messages from other sessions resume you with a new message when there is news. When no actionable work remains, reply with no tool call.

# error: wait-sleep

Not run: this command waits in bash ({{what}}). Waiting in bash holds your turn and sees nothing new in between. To wait for a condition (a process you started, a file, a marker line), start a monitor with persistent:true whose command prints only when the condition is met or fails; it wakes you. If you are waiting for a background task, end your turn: its wake resumes you. Rerun the rest of the command without the wait if you still need it.

# error: wait-detach

Not run: this command starts a process in the background ({{what}}) outside any monitor, so its output and its exit never reach you. Run that process as the monitor command with persistent:true (for example `./run.sh 2>&1 | grep --line-buffered -E 'ALERT|ERROR|done'`), or run it in the foreground: pi moves a long command to the background by itself and wakes you when it finishes.

# error: poll-output

Not run: {{id}} is still running and nothing new has arrived for it since you last looked. Its wake resumes you when there is news (a monitor event, or the command's exit). End your turn now: reply with no tool call. If nothing would wake you (a command that keeps running, like a server, with no monitor on it), start a monitor for what you are waiting for first.

# error: poll-list

Not run: {{count}} background item(s) are still running and no wake or message has arrived since your last task_list. Each one wakes you when there is news. End your turn now: reply with no tool call. If nothing would wake you (a command that keeps running, like a server, with no monitor on it), start a monitor for what you are waiting for first.
