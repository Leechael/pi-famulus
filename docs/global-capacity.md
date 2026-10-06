# Machine-wide agent capacity

The manager owns `maxAgents` in `<home>/config.json` (default 8). Change it at runtime with `pi-famulus config set max-agents 12`; the manager reads the persisted value at admission time, so the change applies immediately without a restart. `pi-famulus config get max-agents` reads the same setting. `status` reports agent slots used/total. Changes are appended to the daemon event stream as `capacity.changed`.

Admission is a request/response protocol, not a boolean semaphore: `acquire_agent(child_id)` returns `granted` or `rejection` (currently `global_capacity`); release is `release_agent(child_id)`. The permit is owned by `(session_id, child_id)`. Re-acquiring the same child is idempotent (including resume). Release is idempotent. On session disconnect, permits owned by that session are reaped. Daemon restart resets permits. The extension retains its per-session `maxConcurrentChildren` local ceiling; both local and machine-wide ceilings must allow a child to start. On global-capacity rejection, the extension backs off and retries.

The rejection field is extensible for stacked scheduling work, including per-work-kind limits and queued admissions that wake a waiting parent. Those are deferred; this protocol does not yet enqueue/wake jobs.

Deferred resource budgets: **CPU tokens** (impact: CPU-bound agents can still saturate the host despite an agent-count cap; trigger: add measured CPU-aware scheduling) and **per-provider in-flight limits** (impact: a provider may still receive a burst from concurrent agents; trigger: provider-aware dispatch/rate limiting). Neither is implemented here.
