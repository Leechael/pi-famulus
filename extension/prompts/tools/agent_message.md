<!-- Model-visible texts of the agent_message tool. Built into src/prompts.generated.ts by npm run prompts. -->

# description

Message your subagents. "send" steers or queues a running child, and starts a new turn on an idle one. <!--seg:tooldesc.agent-message-no-resume-->An idle child (warm or parked) continues with send or subagent resume; its prompt cache stays warm only for the time a wake reports. <!--/seg-->"reply" answers a child's pending decision request; "broadcast" steers every running child of a run (to = run_id); "list" shows children, pending decision requests, and recent traffic.

# snippet

Send/reply/broadcast messages to subagents

# param: action

"send" a message to a child, "reply" to a pending decision request, "broadcast" to all running children of a run, "list" pending requests and recent traffic.

# param: to

Target child_id or name ("send"/"reply"). For "broadcast" from the parent session: run_id. Child sessions broadcast to their own run and must omit this.

# param: message

Message text.

# param: delivery

"steer" (default) delivers to a running child after its current tool call/turn finishes; "queue" delivers after its current turn. Sending to an idle child starts a new turn.
# error: finished-child

Child {{childId}} ({{name}}) has finished ({{status}}), and this host cannot start another turn. Use subagent({ action: "resume", run_id: "{{runId}}", child_id: "{{childId}}", message: "..." }).
