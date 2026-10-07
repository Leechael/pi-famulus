<!-- Eval scenario resume-finished. The grader and fixture are in ablation/scenarios.ts. -->

# tests

resumes a finished subagent via subagent({action:"resume"}) (an agent_message attempt first is recorded, not failed)

# prompt

Use the subagent tool to run one subagent with the task: "Pick a fruit name, write it to fruit.txt, and reply with just that fruit name." After it finishes, ask that same subagent (continue its existing conversation — do not start a new subagent) to append the word done to fruit.txt on a new line.
