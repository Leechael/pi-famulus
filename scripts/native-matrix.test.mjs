import test from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { matrix, platforms } from './native-matrix.mjs';

test('every OS keeps exactly one primary architecture', () => {
  const rows = platforms();
  const oses = [...new Set(rows.map(r => r.platform.split('-')[0]))];
  assert.deepEqual(oses, ['linux', 'darwin', 'win32']);
  for (const os of oses) {
    assert.equal(rows.filter(r => r.platform.startsWith(`${os}-`) && r.primary === true).length, 1, os);
  }
  for (const row of rows) assert.equal(typeof row.primary, 'boolean', row.platform);
});

test('all architectures selects every row; otherwise only primaries, without the flag', () => {
  assert.equal(matrix('true').include.length, platforms().length);
  const primaries = matrix('false').include;
  assert.deepEqual(primaries.map(r => r.platform), platforms().filter(r => r.primary).map(r => r.platform));
  for (const row of [...primaries, ...matrix('true').include]) assert.deepEqual(Object.keys(row), ['platform', 'runner', 'target']);
});

test('anything but the literal text true/false is rejected, never a silent full or empty matrix', () => {
  for (const bad of [true, false, '', 'TRUE', '1', undefined]) assert.throws(() => matrix(bad), /must be 'true' or 'false'/);
  assert.throws(() => matrix('false', [{ platform: 'x', runner: 'y', target: 'z', primary: false }]), /no platforms selected/);
});

test('the CLI prints one GITHUB_OUTPUT line and fails on bad input', () => {
  const cli = fileURLToPath(new URL('./native-matrix.mjs', import.meta.url));
  const ok = spawnSync(process.execPath, [cli, 'false'], { encoding: 'utf8' });
  assert.equal(ok.status, 0, ok.stderr);
  assert.match(ok.stdout, /^matrix=\{"include":\[.+\]\}\n$/);
  assert.notEqual(spawnSync(process.execPath, [cli, 'yes'], { encoding: 'utf8' }).status, 0);
});
