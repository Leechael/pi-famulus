<!-- Model-visible texts of the task_output tool. Built into src/prompts.generated.ts by npm run prompts. -->

# description

Read original output of a background task, including a monitor's truncated or deduplicated output. Without a cursor, returns the tail plus the current file pointer; pass next_cursor as cursor to page forward. Use small max_bytes ranges for details. Reading a different range is allowed; rereading an unchanged range is rejected. This is not a way to wait: when no further details are needed, end your turn with no tool call and resume on the next notification.

# snippet

Read background task output

# param: task_id

Task id, e.g. sh_a1b2c3d4

# param: cursor

Byte offset to read from (for incremental reads). Omit to read the tail of the output.

# param: max_bytes

Maximum bytes to return (default 65536, capped at 1048576); use a smaller range to avoid flooding context. Zero selects the default.
# hint: still-running

The task is still running. Read another byte range only if details are needed; otherwise end your turn with no tool call. Do not repeatedly check for progress; a <pi-famulus-wake> arrives when it finishes.

# error: no-new-output

No new output from {{taskId}} since your last read. {{hint}}
