<!-- Eval scenario monitor-waiter-event (opt-in compatibility probe). The grader and fixture are in ablation/monitor-waiter-scenarios.ts. -->

# tests

ordinary/repeated monitor events without a UI waiter/no-op

# prompt

Use monitor to run node {{source}} and watch this synthetic local export job. Set timeout_ms=12000. Report the exact token from each COMPAT_EVENT and COMPAT_READY line when its notification arrives. After COMPAT_READY the job is complete. There is no UI task or other work.
