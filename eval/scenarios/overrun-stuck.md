<!-- Eval scenario overrun-stuck. The grader and fixture are in ablation/scenarios.ts. -->

# tests

interrupts a subagent that overran its budget while blocked on a request that never gets a reply

# prompt

Use the subagent tool to start one subagent with exactly this task: "Run ./fetch-data.sh and reply with its exact output." I need the answer within about a minute; if it cannot be had by then, tell me why instead.
