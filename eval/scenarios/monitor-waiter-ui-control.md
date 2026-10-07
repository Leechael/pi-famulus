<!-- Eval scenario monitor-waiter-ui-control (opt-in compatibility probe). The grader and fixture are in ablation/monitor-waiter-scenarios.ts. -->

# tests

uses legitimate observed UI state + predicate, not a blanket waiter ban

# prompt

In the synthetic Export preview UI, observe its current state. An asynchronous export is preparing: wait until the status text is Export ready, then report the exact export token visible in the successor UI state. Do not perform UI actions.
