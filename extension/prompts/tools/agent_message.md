<!-- Model-visible texts of the agent_message tool. Built into src/prompts.generated.ts by npm run prompts. -->

# description

Message your subagents. "send" steers or queues a running child. <!--seg:tooldesc.agent-message-no-resume-->Sending to a finished child errors (it does not resume) and points at subagent({ action: "resume", run_id, child_id, message }). <!--/seg-->"reply" answers a child's pending decision request; "broadcast" steers every running child of a run (to = run_id); "list" shows children, pending decision requests, and recent traffic.

# snippet

Send/reply/broadcast messages to subagents

# param: action

"send" a message to a child, "reply" to a pending decision request, "broadcast" to all running children of a run, "list" pending requests and recent traffic.

# param: to

Target child_id or name ("send"/"reply"). For "broadcast" from the parent session: run_id. Child sessions broadcast to their own run and must omit this.

# param: message

Message text.

# param: delivery

"steer" (default) delivers to a running child after its current tool call/turn finishes; "queue" delivers after its current turn. Sending to a finished child is an error — resume with subagent({action:"resume"}).
# error: finished-child

Child {{childId}} ({{name}}) has finished ({{status}}). <!--seg:result.agent-message-use-resume-->agent_message does not resume children. Use subagent({ action: "resume", run_id: "{{runId}}", child_id: "{{childId}}", message: "..." }).<!--/seg-->
