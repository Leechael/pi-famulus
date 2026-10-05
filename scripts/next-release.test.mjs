import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { CHANNELS, compareStable, distTagForVersion, readGitTags, resolveNextVersion } from './next-release.mjs';

test('patch/minor/major increment from package.json when nothing is published', () => {
  assert.deepEqual(resolveNextVersion({ channel: 'patch', packageVersion: '0.1.2' }), {
    version: '0.1.3', tag: 'v0.1.3', distTag: 'latest',
  });
  assert.deepEqual(resolveNextVersion({ channel: 'minor', packageVersion: '0.1.2' }), {
    version: '0.2.0', tag: 'v0.2.0', distTag: 'latest',
  });
  assert.deepEqual(resolveNextVersion({ channel: 'major', packageVersion: '0.1.2' }), {
    version: '1.0.0', tag: 'v1.0.0', distTag: 'latest',
  });
});

test('stable bump uses the highest published or tagged stable, not a stale package.json', () => {
  const got = resolveNextVersion({
    channel: 'patch',
    packageVersion: '0.1.2',
    registryVersions: ['0.1.2', '0.1.3'],
  });
  assert.deepEqual(got, { version: '0.1.4', tag: 'v0.1.4', distTag: 'latest' });
  const tagged = resolveNextVersion({
    channel: 'patch',
    packageVersion: '0.1.2',
    gitTags: ['v0.1.5'],
  });
  assert.deepEqual(tagged, { version: '0.1.6', tag: 'v0.1.6', distTag: 'latest' });
});

test('beta and nightly auto-detect the next prerelease on the following patch series', () => {
  assert.deepEqual(resolveNextVersion({ channel: 'beta', packageVersion: '0.1.2' }), {
    version: '0.1.3-beta.0', tag: 'v0.1.3-beta.0', distTag: 'beta',
  });
  assert.deepEqual(resolveNextVersion({
    channel: 'beta',
    packageVersion: '0.1.2',
    registryVersions: ['0.1.3-beta.0', '0.1.3-beta.2'],
  }), { version: '0.1.3-beta.3', tag: 'v0.1.3-beta.3', distTag: 'beta' });
  assert.deepEqual(resolveNextVersion({
    channel: 'nightly',
    packageVersion: '0.1.2',
    now: new Date('2026-10-06T01:02:03Z'),
  }), { version: '0.1.3-nightly.20261006', tag: 'v0.1.3-nightly.20261006', distTag: 'nightly' });
  assert.deepEqual(resolveNextVersion({
    channel: 'nightly',
    packageVersion: '0.1.2',
    gitTags: ['v0.1.3-nightly.20261006'],
    now: new Date('2026-10-06T12:00:00Z'),
  }), { version: '0.1.3-nightly.20261006.1', tag: 'v0.1.3-nightly.20261006.1', distTag: 'nightly' });
});

test('dist tags follow the version shape', () => {
  assert.equal(distTagForVersion('0.1.3'), 'latest');
  assert.equal(distTagForVersion('0.1.3-beta.0'), 'beta');
  assert.equal(distTagForVersion('0.1.3-nightly.20261006'), 'nightly');
});

test('unknown channels and non-stable package versions are rejected', () => {
  assert.throws(() => resolveNextVersion({ channel: 'rc', packageVersion: '0.1.2' }), /channel/);
  assert.throws(() => resolveNextVersion({ channel: 'patch', packageVersion: '0.1.2-beta.0' }), /stable/);
  assert.ok(CHANNELS.includes('beta'));
  assert.equal(compareStable('0.2.0', '0.1.9'), 1);
});

test('readGitTags fails closed when git cannot list tags', t => {
  const root = mkdtempSync(join(tmpdir(), 'famulus-not-git-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  assert.throws(() => readGitTags(root), /git tag/);
});
