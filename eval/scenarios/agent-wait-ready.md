<!-- Eval scenario agent-wait-ready. The grader and fixture are in ablation/agent-wait-scenarios.ts. -->

# tests

a long-running server the agent starts (it never exits, so no completion wake comes) is seen ready through a persistent monitor before the follow-up, not through sleep, polling, a timed monitor or `&`

# prompt

Start the dev server with `node server.cjs` and leave it running. Once it is up, run `node smoke.cjs` and tell me what it prints.
