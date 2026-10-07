<!-- Model-visible texts of the contact_supervisor tool (child sessions). Built into src/prompts.generated.ts by npm run prompts. -->

# description

Contact the supervisor (parent agent). Use reason "need_decision" when you are blocked and need the parent to decide — the call blocks until the parent replies (10 minute timeout, after which you must decide yourself). Use reason "progress_update" for a fire-and-forget status note.

# snippet

Ask the parent agent for a decision or report progress

# param: reason

"need_decision" blocks until the supervisor (parent agent) replies; "progress_update" is fire-and-forget.

# param: message

Message for the supervisor.
