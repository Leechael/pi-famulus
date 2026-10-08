<!-- Eval scenario still-running-continue. The grader and fixture are in ablation/scenarios.ts. -->

# tests

continues from one task's wake while another background task is still running

# prompt

Run ./quick.sh and ./slow.sh, each in its own bash tool call, leaving stdout unredirected. Do not arrange file writes inside those commands or delegate them; write each result file yourself in a separate tool call after that script completes. As soon as quick.sh finishes, write its output line to quick.txt — do not wait for slow.sh. When slow.sh finishes, write its output line to slow.txt.
