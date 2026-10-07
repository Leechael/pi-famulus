<!-- Model-visible texts of the agent_message tool (child sessions). Built into src/prompts.generated.ts by npm run prompts. -->

# description

Message sibling subagents in your run, or list pending requests. "send" delivers to a running sibling. Sending to a finished sibling errors and does not resume it; resume with subagent({ action: "resume", run_id, child_id, message }). "broadcast" steers every running sibling in your run; "list" shows pending decision requests and recent traffic. Cross-run messaging is rejected.

# snippet

Message sibling subagents in your run
