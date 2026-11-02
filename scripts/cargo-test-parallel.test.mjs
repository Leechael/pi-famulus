import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { SOLO, defaultJobs, run, schedule, testBinaries } from './cargo-test-parallel.mjs';

const artifact = (name, kind, test, executable) => JSON.stringify({
  reason: 'compiler-artifact', target: { name, kind: [kind], src_path: `/m/${name}.rs` }, profile: { test }, executable,
});

test('collects exactly the test executables cargo built, including custom harnesses', () => {
  const lines = [
    '{"reason":"build-script-executed","package_id":"x"}',
    artifact('tokio', 'lib', false, null),
    artifact('pi-famulus', 'bin', false, '/t/debug/pi-famulus'), // the CARGO_BIN_EXE, not a test
    artifact('pi-famulus', 'bin', true, '/t/debug/deps/pi_famulus-1'),
    artifact('intercom', 'test', true, '/t/debug/deps/intercom-2'),
    artifact('platform', 'test', true, '/t/debug/deps/platform-3'),
    '{"reason":"build-finished","success":true}',
    '',
  ];
  assert.deepEqual(testBinaries(lines).map(b => [b.name, b.kind, b.executable]), [
    ['pi-famulus', 'bin', '/t/debug/deps/pi_famulus-1'],
    ['intercom', 'test', '/t/debug/deps/intercom-2'],
    ['platform', 'test', '/t/debug/deps/platform-3'],
  ]);
});

test('schedules every binary once: longest first, load-sensitive suites alone at the end', () => {
  const names = ['cli', 'timing_canary', 'backpressure', 'zzz_new', 'intercom', 'lifecycle_adversarial', 'pi-famulus'];
  const { parallel, solo } = schedule(names.map(name => ({ name })));
  assert.deepEqual(parallel.map(b => b.name), ['intercom', 'pi-famulus', 'cli', 'zzz_new']);
  assert.deepEqual(solo.map(b => b.name), ['lifecycle_adversarial', 'backpressure', 'timing_canary']);
  assert.equal(parallel.length + solo.length, names.length);
});

test('every suite that documents a wall-clock or CPU-saturation contract runs alone', () => {
  // backpressure was split from lifecycle_adversarial so its latency bound
  // never measures the CPU burners; timing_canary pins real durations.
  const header = readFileSync(new URL('../manager/tests/backpressure.rs', import.meta.url), 'utf8').replaceAll('\r\n', '\n');
  assert.match(header, /lifecycle_adversarial deliberately\n\/\/! saturates every CPU/);
  for (const name of ['lifecycle_adversarial', 'backpressure', 'timing_canary']) assert.ok(SOLO.includes(name), name);
});

test('overlaps binaries on Unix but keeps Windows sequential', () => {
  assert.equal(defaultJobs('win32', 4), 1);
  assert.equal(defaultJobs('darwin', 3), 3);
  assert.equal(defaultJobs('linux', 1), 2);
  assert.equal(defaultJobs('linux', 64), 3);
});

test('a process a test leaves behind holding stdout does not hang the run', { skip: process.platform === 'win32' }, async () => {
  // The "test binary" prints, leaves a 30s sleeper holding its stdout and
  // stderr, and exits 0 at once: as cargo does, finish on exit, not on EOF.
  const started = Date.now();
  const result = await run({ name: 'leaky', kind: 'test', src: 'x', executable: '/bin/sh' }, ['-c', 'echo before; sleep 30 & echo after']);
  assert.ok(Date.now() - started < 10_000, `waited ${Date.now() - started}ms for a leaked descendant`);
  assert.equal(result.code, 0);
  assert.match(result.output, /before\nafter\n/);
});

test('a failing or missing binary fails without hanging', async () => {
  const failed = await run({ name: 'f', kind: 'test', src: 'x', executable: process.execPath }, ['-e', 'console.log("out"); process.exit(101)']);
  assert.equal(failed.code, 101);
  assert.match(failed.output, /out/);
  const missing = await run({ name: 'm', kind: 'test', src: 'x', executable: '/nonexistent/test-binary' });
  assert.ok(missing.error);
});
