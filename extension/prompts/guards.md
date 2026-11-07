<!-- Texts of harness guards that stop a run instead of a tool. Built into src/prompts.generated.ts by npm run prompts. -->

# error: idle-turn

Not run. pi-famulus ended this run: {{count}} turns in a row made only no-op or status calls ({{calls}}). Such calls do not wait for anything or yield control. Background tasks, monitors and messages from other sessions resume you with a new message when there is news. When no actionable work remains, reply with no tool call.

# error: idle-run

Not run. pi-famulus ended this run: {{count}} turns in this run made only no-op or status calls ({{calls}}). Such calls do not wait for anything or yield control. Background tasks, monitors and messages from other sessions resume you with a new message when there is news. When no actionable work remains, reply with no tool call.
