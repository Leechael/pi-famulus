/**
 * Windows Git Bash start-up failures (manager end_reasons, see
 * manager/src/shellstart.rs). Concurrent MSYS cold starts race while building
 * the shared mount table (git-for-windows/git#6368). These reasons are
 * inferred: a nested shell can print the same fatal text after the command
 * has already had side effects, and a silent builtin-only command looks like
 * a hung start. Say that, and do not tell the model the command did not run.
 */
export const SHELL_START_FAILED = "shell-start-failed";
export const SHELL_START_HUNG = "shell-start-hung";

/** The status line for a shell start-up failure, or undefined. */
export function shellStartStatus(endReason: string | undefined): string | undefined {
  if (endReason === SHELL_START_FAILED) {
    return "Git Bash reported a Windows runtime startup failure; the command may have partially run. Check for side effects before retrying.";
  }
  if (endReason === SHELL_START_HUNG) {
    return (
      "Git Bash showed no output and only the shell process for 20s, so it was killed. " +
      "This is usually a hung start, but a silent builtin-only command looks the same and may have been running. " +
      "Check for side effects before retrying."
    );
  }
  return undefined;
}
