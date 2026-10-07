<!-- Eval scenario overrun-progressing. The grader and fixture are in ablation/scenarios.ts. -->

# tests

lets a subagent that overran its budget but is visibly progressing finish, instead of interrupting it

# prompt

Use the subagent tool to start one subagent with exactly this task: "Run ./build.sh and reply with the exact final line it prints." When it has the answer, write that line to build-result.txt.
