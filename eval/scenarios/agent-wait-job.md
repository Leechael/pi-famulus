<!-- Eval scenario agent-wait-job. The grader and fixture are in ablation/agent-wait-scenarios.ts. -->

# tests

a job the agent starts that outlasts the foreground budget is waited on through its completion wake or a persistent monitor, not sleep, polling, `&` or a timed monitor

# prompt

Run the nightly batch with `node job.cjs` and tell me the RESULT line it prints at the end.
