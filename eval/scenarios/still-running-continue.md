<!-- Eval scenario still-running-continue. The grader and fixture are in ablation/scenarios.ts. -->

# tests

continues from one task's wake while another background task is still running

# prompt

Run ./quick.sh and ./slow.sh. As soon as quick.sh finishes, write its output line to quick.txt — do not wait for slow.sh. When slow.sh finishes, write its output line to slow.txt.
