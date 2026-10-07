<!-- Model-visible texts of the task_output tool. Built into src/prompts.generated.ts by npm run prompts. -->

# description

Read output of a background task. Without a cursor, returns the tail of the output plus the current file pointer; pass the returned next_cursor as cursor for incremental reads. Not a way to wait: a running task wakes you when it finishes, so reply with no tool call instead of calling this again.

# snippet

Read background task output

# param: task_id

Task id, e.g. sh_a1b2c3d4

# param: cursor

Byte offset to read from (for incremental reads). Omit to read the tail of the output.

# param: max_bytes

Maximum bytes to return (default 65536)
# hint: still-running

The task is still running. Reply to the user now with no tool call; a <pi-famulus-wake> arrives when it finishes.

# error: no-new-output

No new output from {{taskId}} since your last read. {{hint}}
