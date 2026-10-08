<!-- Fixed texts inside <pi-famulus-wake> messages. The XML structure and data fields are built in src/wake.ts. -->

# lead-in

<!--seg:wake.lead-in-->System wake — not a new user message. Handle this <pi-famulus-wake> before other work.<!--/seg-->

# handover: changed-active

Its result arrives as a new wake when it finishes.

# done: changed-active

The run is active again; another subagent-done arrives when it finishes.

# overrun: summary

{{name}} has run {{elapsed}} in this turn, past its {{budget}} budget, and is still running{{shell}}.

# overrun: summary-shell

; it is waiting on a shell command that has run {{elapsed}}

# overrun: actions

give it more time with subagent({ action: "extend", run_id: "{{runId}}", child_id: "{{childId}}", timeout_ms: <ms from now> }); redirect it with agent_message({ action: "send", to: "{{childId}}", message: "<instruction>" }); or stop it with subagent({ action: "interrupt", run_id: "{{runId}}", child_id: "{{childId}}" }).

# overrun: options

It has not been stopped. Extend only when progress or a known, still-useful silent workload justifies more time within the user's timing constraints; a child shell timeout is not evidence of progress or, by itself, a reason to extend past the requested timeframe. Choose one: {{actions}} If you do none of these, it keeps running and the next reminder is scheduled in {{next}}; its result arrives as usual when it finishes.

# overrun: options-ceiling

It has not been stopped yet, but the configured hard ceiling stops it in {{ceiling}}, and extend does not move that ceiling. Choose one: {{actions}} {{noAction}} Its result, or the interruption, arrives as a wake.

# overrun: ceiling-no-action-reminder

If you do none of these, it keeps running and the next reminder is scheduled in {{next}}, until the hard ceiling stops it in {{ceiling}}.

# overrun: ceiling-no-action

If you do none of these, it keeps running until the hard ceiling stops it.

# request: reply-with

agent_message { action: "reply", to: "{{from}}", message: "<your decision>" }
