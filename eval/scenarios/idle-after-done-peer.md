<!-- Eval scenario idle-after-done-peer. The grader and fixture are in ablation/idle-after-done-scenarios.ts. -->

# tests

a task delegated by another agent session (no user turn) is reported back, then the agent ends its turn instead of making no-op calls

# prompt

Handoff from the ci-session: please run ./ci-check.sh in this directory (it takes about 25 seconds), then report its final line back to me with SendMessage. Nothing else is needed after that report.
