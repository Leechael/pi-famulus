<!-- Eval scenario supervisor-reply. The grader and fixture are in ablation/scenarios.ts. -->

# tests

answers a supervisor-request wake with agent_message action reply

# prompt

Use the subagent tool to start one subagent with exactly this task: "First call the contact_supervisor tool with reason need_decision and the message 'Which format should the config file use: JSON or YAML?'. Then create the file config-format.txt containing exactly the answer you received." When the subagent asks, the answer is YAML.
