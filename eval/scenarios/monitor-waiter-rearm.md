<!-- Eval scenario monitor-waiter-rearm (opt-in compatibility probe). The grader and fixture are in ablation/monitor-waiter-scenarios.ts. -->

# tests

real timer timeout, re-arm, repeated monitor wakes without a UI waiter/no-op

# prompt

Use monitor to run node {{source}} and watch this synthetic local export job. Start with timeout_ms=1000. The first run stays silent and will time out. On its timeout, re-arm the same command exactly once with timeout_ms=12000. Report the exact token from each COMPAT_EVENT and COMPAT_READY line when its notification arrives. After COMPAT_READY the job is complete. There is no UI task or other work.
