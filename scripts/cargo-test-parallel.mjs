#!/usr/bin/env node
// Run the manager's complete `cargo test` suite with independent test
// binaries overlapping, instead of cargo's one-binary-at-a-time loop.
//
//   node scripts/cargo-test-parallel.mjs [cargo test args...]
//
// Every test binary cargo would run is built by one `cargo test --no-run`
// with the same arguments and then executed exactly as cargo runs it (from
// the package root, no filters or skips). Any binary that exits unsuccessfully fails
// the run. The crate has no library target, so there are no doctests.
//
// Coverage is identical to `cargo test`; only scheduling differs:
// - ordinary binaries start longest-first, at most PI_FAMULUS_TEST_JOBS at a
//   time, each with libtest's own default thread count;
// - SOLO binaries then run one by one with the machine to themselves. They
//   either load every CPU on purpose (lifecycle_stress) or assert wall-clock
//   bounds that must not measure other suites' load (lifecycle_adversarial,
//   backpressure, timing_canary). tests/backpressure.rs was split out for exactly that
//   reason; overlapping it with anything reintroduces the problem;
// - each binary's output is buffered and printed whole when it finishes, in
//   a collapsible group on GitHub Actions, followed by a duration table.
//
// Windows defaults to one binary at a time: Git Bash start-up is racy under
// concurrent first launches (see tests/platform.rs), and its suites are short.

import { spawn, spawnSync } from 'node:child_process';
import { appendFileSync, closeSync, mkdtempSync, openSync, readFileSync, rmSync } from 'node:fs';
import { availableParallelism, tmpdir } from 'node:os';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const manager = join(dirname(fileURLToPath(import.meta.url)), '..', 'manager');

// Measured seconds of the slowest binaries in CI (darwin/linux, both clock
// lanes). Only an ordering hint: unknown binaries start after these.
const WEIGHT = {
  intercom: 180,
  lifecycle_adversarial: 80,
  lifecycle_stress: 30,
  'pi-famulus': 35,
  backpressure: 35,
  upgrade: 32,
  platform: 24,
  protocol: 20,
  observability: 18,
  messaging_relay: 18,
  build_script: 17,
  startup_retry: 10,
};
// Never overlapped with anything; see the header.
export const SOLO = ['lifecycle_stress', 'lifecycle_adversarial', 'backpressure', 'timing_canary'];

export function defaultJobs(platform = process.platform, cpus = availableParallelism()) {
  if (platform === 'win32') return 1;
  return Math.max(2, Math.min(3, cpus));
}

/** Test executables from `cargo --message-format=json` stdout lines. */
export function testBinaries(lines) {
  const out = [];
  for (const line of lines) {
    if (!line.startsWith('{')) continue;
    const msg = JSON.parse(line);
    if (msg.reason !== 'compiler-artifact' || !msg.profile?.test || !msg.executable) continue;
    out.push({ name: msg.target.name, kind: msg.target.kind.join(','), src: msg.target.src_path, executable: msg.executable });
  }
  return out;
}

/** Longest-first overlapping batch, then the SOLO binaries in SOLO order. */
export function schedule(binaries) {
  const weight = b => WEIGHT[b.name] ?? 0;
  const parallel = binaries.filter(b => !SOLO.includes(b.name)).sort((a, b) => weight(b) - weight(a) || a.name.localeCompare(b.name));
  const solo = SOLO.flatMap(name => binaries.filter(b => b.name === name));
  return { parallel, solo };
}

function build(args) {
  return new Promise((resolve, reject) => {
    // Diagnostics stay human-readable on stderr; artifacts arrive on stdout.
    const child = spawn('cargo', ['test', ...args, '--no-run', '--message-format=json-render-diagnostics'], {
      cwd: manager, stdio: ['ignore', 'pipe', 'inherit'],
    });
    let stdout = '';
    child.stdout.setEncoding('utf8').on('data', chunk => { stdout += chunk; });
    child.on('error', reject);
    child.on('close', code => (code === 0 ? resolve(testBinaries(stdout.split('\n'))) : reject(new Error(`cargo test --no-run exited ${code}`))));
  });
}

// Like cargo, give each test process inherited file descriptors and wait for
// the process itself, never for EOF: a daemon or task a test leaves behind
// can hold its stdout open indefinitely, and must not hang the run.
const logs = mkdtempSync(join(tmpdir(), 'cargo-test-parallel-'));
process.on('exit', () => rmSync(logs, { recursive: true, force: true }));

// A binary that outlives this (the slowest takes ~3 minutes today) is
// reported with its output so far plus a process snapshot, then killed, so
// the step fails with logs instead of hanging the job until it is cancelled
// (GitHub publishes no logs for a step cancelled mid-run).
export const TIMEOUT_S = Number(process.env.PI_FAMULUS_TEST_TIMEOUT_S) || 900;
const running = new Map();
// Set by main(): stream a timeout's diagnostics as soon as it fires.
let live = false;

function snapshot(pid) {
  const out = [];
  const capture = (title, cmd, args) => {
    const r = spawnSync(cmd, args, { encoding: 'utf8', timeout: 30_000, maxBuffer: 16 << 20 });
    if (r.error) return;
    out.push(`--- ${title}`, (r.stdout + r.stderr).split('\n').slice(0, 400).join('\n'));
  };
  if (process.platform !== 'win32') capture('processes', 'ps', ['-axo', 'pid,ppid,pgid,etime,stat,command']);
  // Stacks of every thread of the hung test process; needs no ptrace grant.
  if (process.platform === 'darwin') capture(`sample ${pid}`, 'sample', [String(pid), '3']);
  return out.join('\n');
}

export function run(binary, args = [], timeoutS = TIMEOUT_S) {
  return new Promise(resolve => {
    const started = Date.now();
    const path = join(logs, `${binary.name}-${binary.kind}.log`);
    const fd = openSync(path, 'w');
    // The environment cargo itself gives a test process at run time.
    const env = { ...process.env, CARGO_MANIFEST_DIR: manager, CARGO_PKG_NAME: 'pi-famulus' };
    let child;
    let timer;
    let timedOut = false;
    let diagnostics = '';
    let done = false;
    const finish = (code, signal, error) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      running.delete(binary);
      closeSync(fd);
      let output = readFileSync(path, 'utf8');
      if (timedOut) output += `\n===== timed out after ${timeoutS}s; state before the kill:\n${diagnostics}\n`;
      resolve({ ...binary, code, signal, error, timedOut, seconds: (Date.now() - started) / 1000, output });
    };
    try {
      child = spawn(binary.executable, args, { cwd: manager, env, stdio: ['ignore', fd, fd] });
    } catch (error) {
      finish(null, null, error);
      return;
    }
    running.set(binary, started);
    timer = setTimeout(() => {
      timedOut = true;
      diagnostics = snapshot(child.pid);
      // Print now: if the process cannot be reaped, finish() may come late.
      if (live) process.stdout.write(`\n===== ${binary.name}: timed out after ${timeoutS}s; output so far:\n${readFileSync(path, 'utf8')}\n${diagnostics}\n`);
      child.kill('SIGKILL');
      // A process stuck in the kernel may never report exit; do not wait on it.
      setTimeout(() => finish(null, 'SIGKILL'), 30_000).unref();
    }, timeoutS * 1000);
    child.once('error', error => finish(null, null, error));
    child.once('exit', (code, signal) => finish(code, signal));
  });
}

const actions = process.env.GITHUB_ACTIONS === 'true';
const label = b => `${b.kind === 'bin' ? 'unittests' : 'tests'} ${relative(manager, b.src).replaceAll('\\', '/')}`;
const passed = r => r.code === 0 && !r.signal && !r.error && !r.timedOut;

function report(r) {
  const why = r.timedOut ? `timed out after ${TIMEOUT_S}s` : r.error?.message ?? (r.signal ? `signal ${r.signal}` : `exit ${r.code}`);
  const status = passed(r) ? 'ok' : `FAILED (${why})`;
  const title = `${label(r)}: ${status} in ${r.seconds.toFixed(1)}s`;
  // Failures stay expanded so the log opens on them.
  if (actions && passed(r)) process.stdout.write(`::group::${title}\n${r.output}\n::endgroup::\n`);
  else process.stdout.write(`\n===== ${title}\n${r.output}\n`);
}

async function pool(items, jobs, each) {
  const results = [];
  let next = 0;
  await Promise.all(Array.from({ length: Math.min(jobs, items.length) }, async () => {
    while (next < items.length) {
      const result = await each(items[next++]);
      report(result);
      results.push(result);
    }
  }));
  return results;
}

async function main(args) {
  live = true;
  const jobs = Number(process.env.PI_FAMULUS_TEST_JOBS) || defaultJobs();
  const binaries = await build(args);
  if (binaries.length === 0) throw new Error('cargo reported no test binaries');
  const { parallel, solo } = schedule(binaries);
  console.log(`Running ${parallel.length} test binaries, ${jobs} at a time; then ${solo.map(b => b.name).join(', ') || 'nothing'} alone, one by one.`);
  const started = Date.now();
  // Output is printed per binary on completion; say what is still running.
  const heartbeat = setInterval(() => {
    const now = Date.now();
    const busy = [...running].map(([b, t]) => `${b.name} ${Math.round((now - t) / 1000)}s`).join(', ');
    console.log(`[${Math.round((now - started) / 1000)}s] still running: ${busy}`);
  }, 120_000);
  const results = [...await pool(parallel, jobs, b => run(b)), ...await pool(solo, 1, b => run(b))];
  clearInterval(heartbeat);
  const wall = (Date.now() - started) / 1000;
  const failed = results.filter(r => !passed(r));

  const rows = [...results].sort((a, b) => b.seconds - a.seconds)
    .map(r => `| \`${label(r)}\` | ${passed(r) ? 'ok' : '**FAILED**'} | ${r.seconds.toFixed(1)} |`);
  const summary = [`### cargo test ${args.join(' ')}`.trimEnd(), '',
    `${results.length} binaries, ${failed.length} failed, ${wall.toFixed(1)}s wall (${jobs} parallel).`, '',
    '| Binary | Result | Seconds |', '|---|---|---:|', ...rows, ''].join('\n');
  console.log(`\n${summary}`);
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, `${summary}\n`);
  if (failed.length) {
    console.error(`\nFailed: ${failed.map(label).join(', ')}`);
    process.exitCode = 1;
  }
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  main(process.argv.slice(2)).catch(error => { console.error(error); process.exitCode = 1; });
}
