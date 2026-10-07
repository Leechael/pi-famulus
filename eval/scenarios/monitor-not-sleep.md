<!-- Eval scenario monitor-not-sleep. The grader and fixture are in ablation/scenarios.ts. -->

# tests

waits for a condition event-driven (monitor, or a backgrounded tail -f | grep -m1), not with a sleep/poll loop

# prompt

A service in this directory is starting up; within about 20 seconds it will append a line containing READY to service.log. Wait until that line appears, then tell me the token on the READY line.
