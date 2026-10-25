import { spawn, type ChildProcess } from "node:child_process";
import { join } from "node:path";
import { createInterface } from "node:readline";

export interface ProcessExitObserver {
  /** Identifies the process captured before the operation under test. */
  identity: string;
  waitForExit(): Promise<boolean>;
  dispose(): void;
}

type SpawnObserver = (pid: number, timeoutMs: number) => ChildProcess;

function spawnObserver(pid: number, timeoutMs: number): ChildProcess {
  if (process.platform === "win32") {
    // GetProcessById alone is lazy: force Handle before acknowledging readiness.
    // Retaining that handle pins this incarnation and prevents PID reuse.
    const script = `
$ErrorActionPreference = 'Stop'
$target = [System.Diagnostics.Process]::GetProcessById(${pid})
try {
  $null = $target.Handle
  [Console]::Out.WriteLine('ready:' + $target.Id + ':' + $target.StartTime.ToUniversalTime().ToString('O'))
  [Console]::Out.Flush()
  if ($target.WaitForExit(${timeoutMs})) { exit 0 } else { exit 2 }
} finally { $target.Dispose() }
`;
    const powershell = join(process.env.SystemRoot ?? "C:\\Windows", "System32", "WindowsPowerShell", "v1.0", "powershell.exe");
    return spawn(powershell, ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", script], { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
  }
  // POSIX keeps the existing ESRCH-based oracle, in a bounded observer process.
  const script = `
const pid = Number(process.argv[1]);
const timeoutMs = Number(process.argv[2]);
process.kill(pid, 0);
console.log('ready:' + pid);
const deadline = Date.now() + timeoutMs;
const check = () => {
  try { process.kill(pid, 0); } catch (error) {
    if (error.code === 'ESRCH') process.exit(0);
    throw error;
  }
  if (Date.now() >= deadline) process.exit(2);
  setTimeout(check, 100);
};
check();
`;
  return spawn(process.execPath, ["-e", script, String(pid), String(timeoutMs)], { stdio: ["ignore", "pipe", "pipe"] });
}

/** Arm before killing the manager; never reopen the Windows target by PID. */
export async function observeProcessExit(
  pid: number,
  timeoutMs = 20_000,
  start: SpawnObserver = spawnObserver,
): Promise<ProcessExitObserver> {
  if (!Number.isSafeInteger(pid) || pid <= 0 || !Number.isSafeInteger(timeoutMs) || timeoutMs <= 0) {
    throw new Error("process observer requires a positive PID and timeout");
  }
  const child = start(pid, timeoutMs);
  let stderr = "";
  let failure = "";
  const watchdog = setTimeout(() => {
    failure = `observer did not finish within ${timeoutMs + 8000}ms`;
    child.kill();
  }, timeoutMs + 8000);
  child.stderr?.on("data", (chunk) => { stderr = (stderr + String(chunk)).slice(-4000); });
  // Store errors as data until waitForExit is called, avoiding an unhandled
  // rejection if another e2e assertion fails before it consumes this result.
  const completed = new Promise<{ code: number | null; signal: string | null }>((resolve) => {
    child.once("error", (error) => { failure = error.message; });
    child.once("close", (code, signal) => {
      clearTimeout(watchdog);
      resolve({ code, signal });
    });
  });
  const lines = createInterface({ input: child.stdout! });
  try {
    const identity = await new Promise<string>((resolve, reject) => {
      const timer = setTimeout(() => { child.kill(); reject(new Error(`process observer readiness timed out for PID ${pid}`)); }, 8000);
      lines.once("line", (line) => {
        clearTimeout(timer);
        if (line !== `ready:${pid}` && !line.startsWith(`ready:${pid}:`)) reject(new Error(`invalid process observer readiness: ${line}`));
        else resolve(line.slice("ready:".length));
      });
      void completed.then(() => {
        clearTimeout(timer);
        reject(new Error(`process observer exited before readiness for PID ${pid}: ${failure} ${stderr}`));
      });
    });
    return {
      identity,
      async waitForExit() {
        const { code, signal } = await completed;
        if (!failure && code === 0) return true;
        if (!failure && code === 2) return false;
        throw new Error(`process observer failed for ${identity}: exit=${code} signal=${signal} ${failure} ${stderr}`);
      },
      dispose() { lines.close(); child.kill(); },
    };
  } catch (error) {
    lines.close();
    child.kill();
    throw error;
  }
}
