<!-- Eval scenario monitor-behavior-renew. The grader and fixture are in ablation/monitor-waiter-scenarios.ts. -->

# tests

after a short timed monitor expires, continuing an open-ended watch (horizon beyond the 1-hour timeout cap) starts a persistent monitor rather than another timed one

# prompt

Watch `node behavior-source.cjs slow` for me. First use the monitor tool for a short ten-second trial watch, just to confirm the command starts and prints. Once that trial ends, keep watching it for the rest of the day and tell me when an ALERT line shows up.
