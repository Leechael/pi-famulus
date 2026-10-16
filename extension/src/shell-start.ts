/**
 * Windows Git Bash start-up failures (manager end_reasons, see
 * manager/src/shellstart.rs). Concurrent MSYS cold starts race while building
 * the shared mount table (git-for-windows/git#6368): a loser crashes or
 * prints `add_item ... failed`, or hangs with no output. The command never
 * ran, so the model must hear that, not "exit code -1073741819".
 */
export const SHELL_START_FAILED = "shell-start-failed";
export const SHELL_START_HUNG = "shell-start-hung";

/** The status line for a shell start-up failure, or undefined. */
export function shellStartStatus(endReason: string | undefined): string | undefined {
  if (endReason === SHELL_START_FAILED) {
    return "Git Bash failed to start (Windows runtime race, not your command); the command did not run. Retry it.";
  }
  if (endReason === SHELL_START_HUNG) {
    return (
      "Git Bash hung while starting (Windows runtime race, not your command) and was killed after 20s " +
      "with no output; the command most likely did not run. Retry it."
    );
  }
  return undefined;
}
