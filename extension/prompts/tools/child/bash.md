<!-- Model-visible texts of the bash tool (child sessions). Built into src/prompts.generated.ts by npm run prompts. -->

# description

Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last {{maxLines}} lines or {{maxKb}}KB (whichever is hit first). The command runs to completion (or the optional timeout in seconds, after which it is killed). There is no background execution inside subagents.

# snippet

Execute bash commands (ls, grep, find, etc.)

# param: command

The bash command to execute

# param: timeout

Hard kill timeout in seconds (optional, no default timeout)
# error: bare-sleep

Refusing to run a bare sleep/idle-loop command: {{command}}. Sleeping to wait for work is never useful: run the actual command directly, or report back to the supervisor if you are blocked.
