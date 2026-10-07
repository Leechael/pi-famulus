<!-- Model-visible texts of the subagent tool. Built into src/prompts.generated.ts by npm run prompts. -->

# description

Run subagents in parallel (tasks) or sequentially (chain with {previous}/{outputs.<label>} interpolation). By default the call waits up to a foreground budget (default 45s); longer runs continue in the background. <!--seg:tooldesc.subagent-wakes-->Each child that finishes while others are still running wakes you with <pi-famulus-wake kind="subagent-handover"> (its prompt and result). The whole run wakes you with <pi-famulus-wake kind="subagent-done">. Never poll or sleep to wait. <!--/seg--><!--seg:tooldesc.subagent-overrun-->A subagent still running past its timeout_ms is not stopped; it wakes you with <pi-famulus-wake kind="subagent-overrun"> so you can extend, steer, or interrupt it. <!--/seg-->Use action=list/get/status/interrupt/resume/steer/extend to manage existing runs.

# snippet

Fan out subagents in parallel or sequence them in a chain

# rules (list)

- <!--seg:rules.subagent-handover-->When a <pi-famulus-wake kind="subagent-handover"> arrives, read <prompt> and <result> immediately and continue: subagent({action:"resume", run_id, child_id, message}) for that child, or agent_message to steer children that are still running. Do not wait for the rest of the run.<!--/seg-->
- <!--seg:rules.subagent-no-poll-->Subagent runs that exceed the foreground budget continue in the background; you are notified per finished child and again when the run completes — do not poll.<!--/seg-->
- <!--seg:rules.subagent-failed-child-->A failed subagent does not fail the whole run; inspect per-subagent sections in the result.<!--/seg-->
- <!--seg:rules.subagent-wakes-not-user--><pi-famulus-wake> is a system wake, not a user reply. kind=subagent-handover is one child; kind=subagent-done is the whole run.<!--/seg-->

# param: tasks

Subagents to run in parallel

# param: tasks.agent

Agent definition name (default: worker)

# param: tasks.prompt

Task prompt for this subagent

# param: tasks.name

Display name (default: agent name + ordinal)

# param: work_kind

Explicit planned workload class for machine-wide admission (default: other)

# param: chain

Steps to run sequentially (always awaited)

# param: chain.agent

Agent definition name (default: worker)

# param: chain.prompt

Prompt for this step; {previous} and {outputs.<label>} interpolate earlier results

# param: chain.label

Label for referencing this step's output later

# param: async

Return immediately with a run_id; completion arrives via notification

# param: concurrency

Max parallel subagents for tasks (default 4)

# param: fail_fast

Cancel not-yet-started subagents on first failure (already-started ones finish)

# param: model

Model override for all subagents: fuzzy ("haiku"), qualified ("provider/id"), optionally with ":<thinking>" suffix ({{thinkingLevels}}; an unknown suffix is dropped with a warning when the base resolves; an unknown base still errors). Default: current model. Use action:"models" to list selectable values.

# param: timeout_ms

Time budget per subagent turn in ms (default 1800000, max {{maxMs}}). <!--seg:tooldesc.subagent-timeout-overrun-->Passing it does not stop the subagent: you get <pi-famulus-wake kind="subagent-overrun"> and choose extend, steer, or interrupt. With resume: the resumed turn's budget. With extend: the new budget from now.<!--/seg-->

# param: action

Manage an existing run (or list selectable models) instead of starting a new one

# param: run_id

Target run for action

# param: child_id

Target child (id or name) for steer/interrupt/resume/extend

# param: message

Message content for steer/resume
# result: backgrounded

Started {{count}} subagent(s) in run {{runId}}. {{reason}}
<!--seg:result.subagent-bg-instruction-->While others are still running, each finished subagent arrives as <pi-famulus-wake kind="subagent-handover"> with that child's prompt and result. Read it and continue: subagent({action:"resume", run_id, child_id, message}) for that child, or agent_message to steer the ones still running. Do not wait for the whole run. Do not poll. <pi-famulus-wake kind="subagent-done"> arrives when every subagent in the run has finished. <!--/seg-->Use subagent({action:"get", run_id:"{{runId}}"}) if you need the full record.

# result: extended

Extended subagent {{name}} ({{childId}}) in run {{runId}}: the next <pi-famulus-wake kind="subagent-overrun"> comes in {{next}} if it is still running. Its result arrives as a wake when it finishes; do not poll.

# result: extended.hard-ceiling

The configured hard ceiling (hardTimeoutMs) is not moved: it stops this subagent in {{remaining}}.

# result: resumed

Resumed subagent {{name}} ({{childId}}) in run {{runId}}. {{queued}}You will be notified via <pi-famulus-wake kind="subagent-handover"> if others are still running, otherwise via <pi-famulus-wake kind="subagent-done"> when it completes. Do not poll.

# error: resume-queued

subagent {{name}} ({{childId}}) is already queued and starts when a subagent slot frees; its result arrives as a wake when it finishes. Do not resume it again.
