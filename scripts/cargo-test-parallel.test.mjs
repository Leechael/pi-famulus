import test from 'node:test';
import assert from 'node:assert/strict';
import { SERIAL, defaultJobs, schedule, testBinaries } from './cargo-test-parallel.mjs';

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

test('schedules every binary once: longest first, real-time canaries alone at the end', () => {
  const names = ['cli', 'timing_canary', 'backpressure', 'zzz_new', 'intercom', 'pi-famulus'];
  const { parallel, serial } = schedule(names.map(name => ({ name })));
  assert.deepEqual(parallel.map(b => b.name), ['intercom', 'backpressure', 'pi-famulus', 'cli', 'zzz_new']);
  assert.deepEqual(serial.map(b => b.name), ['timing_canary']);
  assert.ok(SERIAL.has('timing_canary'));
});

test('overlaps binaries on Unix but keeps Windows sequential', () => {
  assert.equal(defaultJobs('win32', 4), 1);
  assert.equal(defaultJobs('darwin', 3), 3);
  assert.equal(defaultJobs('linux', 1), 2);
  assert.equal(defaultJobs('linux', 64), 4);
});
