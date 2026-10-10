import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { execFileSync } from 'node:child_process';

export const REPOSITORY = 'Leechael/pi-famulus';
// win32-arm64 is not required: CI does not build it, and publish.yml only
// downloads artifacts from its own run. Add it back with a producer.
// npm package names use `win` for Windows. Unscoped `*-win32-*` names have
// tripped npm's spam detector and blocked first publish.
export const PLATFORMS = [
  ['linux', 'x64', 'x86_64-unknown-linux-musl'],
  ['linux', 'arm64', 'aarch64-unknown-linux-musl'],
  ['darwin', 'x64', 'x86_64-apple-darwin'],
  ['darwin', 'arm64', 'aarch64-apple-darwin'],
  ['win32', 'x64', 'x86_64-pc-windows-msvc'],
].map(([os, arch, target]) => {
  const packageOs = os === 'win32' ? 'win' : os;
  return {
    os, arch, target, id: `${os}-${arch}`, name: `pi-famulus-${packageOs}-${arch}`, directory: `npm/${os}-${arch}`,
    // CreateProcess needs an explicit extension for lpApplicationName; this package ships `.exe`.
    binary: os === 'win32' ? 'bin/pi-famulus.exe' : 'bin/pi-famulus',
  };
});

function isUtcCalendarDay(yyyymmdd) {
  const y = Number(yyyymmdd.slice(0, 4));
  const m = Number(yyyymmdd.slice(4, 6));
  const d = Number(yyyymmdd.slice(6, 8));
  const dt = new Date(Date.UTC(y, m - 1, d));
  return dt.getUTCFullYear() === y && dt.getUTCMonth() === m - 1 && dt.getUTCDate() === d;
}

export function validateTag(tag) {
  const m = /^(v(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*))(?:-beta\.(?:0|[1-9]\d*)|-nightly\.(\d{8})(?:\.(?:0|[1-9]\d*))?)?$/.exec(tag ?? '');
  assert.ok(m, 'release tag must be vX.Y.Z, vX.Y.Z-beta.N, or vX.Y.Z-nightly.YYYYMMDD[.N] (no shell syntax)');
  if (m[2]) assert.ok(isUtcCalendarDay(m[2]), `nightly date ${m[2]} is not a real UTC calendar day`);
  return tag.slice(1);
}

export function validateMetadata(root, { tag, repository = REPOSITORY } = {}) {
  assert.equal(repository, REPOSITORY, 'repository must match the canonical GitHub repository');
  const read = directory => JSON.parse(readFileSync(join(root, directory, 'package.json'), 'utf8'));
  const pkg = read('extension');
  const version = tag === undefined ? pkg.version : validateTag(tag);
  validateTag(`v${version}`);
  const packages = [{ name: 'pi-famulus', directory: 'extension', metadata: pkg }, ...PLATFORMS.map(p => ({ ...p, metadata: read(p.directory) }))];
  for (const p of packages) {
    const m = p.metadata;
    assert.equal(m.name, p.name, `package name: ${p.directory}`);
    assert.equal(m.version, version, `version mismatch: ${p.name}`);
    assert.equal(m.repository?.type, 'git', `repository type: ${p.name}`);
    assert.equal(m.repository?.url, `https://github.com/${repository}`, `repository URL mismatch: ${p.name}`);
    assert.equal(m.repository?.directory, p.directory, `repository directory: ${p.name}`);
    for (const hook of ['preinstall', 'install', 'postinstall']) assert.ok(!m.scripts?.[hook], `install hook forbidden: ${p.name}`);
    if (p.os) {
      // `./bin/pi-famulus` is the OS-agnostic subpath resolvers ask for.
      assert.equal(m.main, `./${p.binary}`, `native main: ${p.name}`);
      assert.deepEqual(m.exports, { './package.json': './package.json', './bin/pi-famulus': `./${p.binary}` }, `native exports: ${p.name}`);
      assert.deepEqual(m.os, [p.os], `native os: ${p.name}`);
      assert.deepEqual(m.cpu, [p.arch], `native cpu: ${p.name}`);
      assert.ok(Array.isArray(m.files) && m.files.includes(p.binary) && m.files.every(f => [p.binary, 'README.md'].includes(f)), `native files must whitelist binary and optional README: ${p.name}`);
      for (const key of ['dependencies', 'optionalDependencies', 'peerDependencies', 'devDependencies']) assert.equal(Object.keys(m[key] ?? {}).length, 0, `native dependencies forbidden: ${p.name}`);
    }
  }
  assert.deepEqual(pkg.files, ['src', 'bin', 'README.md'], 'root files contract');
  assert.deepEqual(pkg.bin, { 'pi-famulus': './bin/pi-famulus.js' }, 'root CLI contract');
  assert.deepEqual(pkg.optionalDependencies, Object.fromEntries(PLATFORMS.map(p => [p.name, version])), 'root optional native dependencies must match version exactly');
  const cargo = readFileSync(join(root, 'manager/Cargo.toml'), 'utf8').match(/\[package\]([\s\S]*?)(?=\n\[|$)/)?.[1];
  assert.equal(cargo?.match(/^version\s*=\s*"([^"]+)"/m)?.[1], version, 'Cargo version mismatch');
  const managerClient = readFileSync(join(root, 'extension/src/manager-client.ts'), 'utf8');
  assert.equal(managerClient.match(/^const EXTENSION_VERSION = "([^"]+)";$/m)?.[1], version, 'extension src version mismatch');
  return packages;
}

export function validateGitTag(root, tag) {
  validateTag(tag);
  const git = (...args) => execFileSync('git', args, { cwd: root, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
  const tagged = git('rev-parse', '--verify', `refs/tags/${tag}^{commit}`);
  assert.equal(git('rev-parse', 'HEAD'), tagged, 'release tag must be the checked-out commit');
  try { git('rev-parse', '--verify', 'refs/remotes/origin/main^{commit}'); }
  catch (cause) { throw new Error('origin/main is missing or invalid; fetch origin/main before release validation', { cause }); }
  try { git('merge-base', '--is-ancestor', tagged, 'refs/remotes/origin/main'); }
  catch (error) {
    if (error.status === 1) throw new Error('release tagged commit must be an ancestor of origin/main', { cause: error });
    throw error;
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const args = process.argv.slice(2);
    // --tag validates package metadata against the computed release tag. The
    // git tag itself is created after npm publish, so require it only when
    // callers pass --require-git-tag.
    const tagIdx = args.indexOf('--tag');
    const requireIdx = args.indexOf('--require-git-tag');
    const requireGit = requireIdx >= 0;
    const skip = new Set([tagIdx, requireIdx].filter(i => i >= 0));
    if (tagIdx >= 0) skip.add(tagIdx + 1);
    const rest = args.filter((_, i) => !skip.has(i));
    assert.ok(rest.length === 0 && (tagIdx < 0 || typeof args[tagIdx + 1] === 'string'), 'usage: validate-release.mjs [--tag vX.Y.Z] [--require-git-tag]');
    assert.ok(!requireGit || tagIdx >= 0, '--require-git-tag needs --tag');
    const tag = tagIdx >= 0 ? args[tagIdx + 1] : undefined;
    const packages = validateMetadata(process.cwd(), { tag, repository: process.env.GITHUB_REPOSITORY ?? REPOSITORY });
    if (requireGit) validateGitTag(process.cwd(), tag);
    console.log(`Validated all ${packages.length} packages${tag ? ` for ${tag}` : ''}`);
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
