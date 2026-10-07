<!-- Eval scenario handover-continue. The grader and fixture are in ablation/scenarios.ts. -->

# tests

continues from a per-child subagent-handover wake while the other child still runs

# prompt

Use the subagent tool to run these two tasks in parallel: (1) "Read alpha.txt and reply with its exact contents." (2) "Run ./slow-child.sh and reply with its exact output." As soon as task (1)'s answer is available, write it to a-result.txt — do not wait for task (2). When task (2) finishes, write its answer to b-result.txt.
