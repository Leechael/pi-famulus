import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync, chmodSync, statSync, cpSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';
import { PLATFORMS, validateMetadata, validateTag, validateGitTag } from './validate-release.mjs';
import { prepareNative } from './prepare-native.mjs';
import { publishPackages } from './publish-packages.mjs';
import { bumpRelease } from './bump-release.mjs';

const repository = 'Leechael/pi-famulus';
const version = '0.1.0';
function fixture(t, fixtureVersion = version) {
  const root = mkdtempSync(join(tmpdir(), 'famulus-release-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const put = (path, value) => { mkdirSync(join(root, path, '..'), { recursive: true }); writeFileSync(join(root, path), typeof value === 'string' ? value : JSON.stringify(value)); };
  const repo = directory => ({ type: 'git', url: `https://github.com/${repository}`, directory });
  put('extension/package.json', { name: 'pi-famulus', version: fixtureVersion, type: 'module', repository: repo('extension'), files: ['src', 'bin', 'README.md'], bin: { 'pi-famulus': './bin/pi-famulus.js' }, optionalDependencies: Object.fromEntries(PLATFORMS.map(p => [p.name, fixtureVersion])) });
  put('extension/src/config.ts', 'export const config = true;');
  put('extension/src/manager-client.ts', `const EXTENSION_VERSION = "${fixtureVersion}";\n`);
  put('extension/src/native-manager.js', 'export const native = true;');
  put('extension/bin/pi-famulus.js', '#!/usr/bin/env node\nconsole.log("pi-famulus");');
  put('extension/README.md', 'Test package');
  put('manager/Cargo.toml', `[package]\nname = "pi-famulus"\nversion = "${fixtureVersion}"\n`);
  for (const p of PLATFORMS) put(`${p.directory}/package.json`, { name: p.name, version: fixtureVersion, main: './bin/pi-famulus', exports: { './package.json': './package.json', './bin/pi-famulus': './bin/pi-famulus' }, files: ['bin/pi-famulus'], os: [p.os], cpu: [p.arch], repository: repo(p.directory) });
  return { root, put };
}
function packAll(t, fixtureVersion = version) {
  const f = fixture(t, fixtureVersion);
  for (const p of PLATFORMS) {
    const binary = join(f.root, 'manager', 'target', p.target, 'release', 'pi-famulus');
    f.put(binary.slice(f.root.length + 1), `#!/bin/sh\necho pi-famulus ${fixtureVersion}+fixture\n`);
    chmodSync(binary, 0o755);
    prepareNative(f.root, p.id, join(f.root, 'dist'));
  }
  execFileSync('npm', ['pack', '--json', '--pack-destination', join(f.root, 'dist')], { cwd: join(f.root, 'extension'), stdio: 'pipe' });
  return f;
}
const response = (status, body) => ({ status, ok: status >= 200 && status < 300, json: async () => body });
// actionlint validates YAML. This guard follows each shell scalar's parent
// indentation rather than assuming a fixed column, scalar style, or final LF.
function workflowRunBodies(text) {
  const lines = text.split('\n');
  const bodies = [];
  for (let i = 0; i < lines.length; i++) {
    const run = lines[i].match(/^( *)(- +)?run: *(.*)$/);
    if (!run) continue;
    const keyIndent = run[1].length + (run[2]?.length ?? 0);
    bodies.push(run[3]);
    while (i + 1 < lines.length) {
      const next = lines[i + 1];
      if (next.trim() && next.match(/^ */)[0].length <= keyIndent) break;
      bodies.push(next);
      i++;
    }
  }
  return bodies.join('\n');
}

function assertPublishingAuthority(publish) {
  assert.match(publish, /^concurrency:\n  group: npm-release\n  queue: max\n  cancel-in-progress: false\n/m, 'workflow-level release queue must retain pending runs');
  const validate = publish.match(/^  validate:\n([\s\S]*?)(?=^  [\w-]+:|(?![\s\S]))/m)?.[1] ?? '';
  assert.match(validate, /^    if: github\.ref == 'refs\/heads\/main'$/m, 'validate job must require main');
}

test('bump-release writes every versioned manifest including prerelease', t => {
  const { root, put } = fixture(t);
  put('manager/Cargo.lock', '[[package]]\nname = "pi-famulus"\nversion = "0.1.0"\n');
  put('extension/package-lock.json', {
    name: 'pi-famulus',
    version: '0.1.0',
    lockfileVersion: 3,
    packages: {
      '': { name: 'pi-famulus', version: '0.1.0', optionalDependencies: Object.fromEntries(PLATFORMS.map(p => [p.name, '0.1.0'])) },
      ...Object.fromEntries(PLATFORMS.map(p => [`../npm/${p.id}`, { name: p.name, version: '0.1.0' }])),
    },
  });
  bumpRelease(root, '0.1.3-beta.0');
  assert.equal(JSON.parse(readFileSync(join(root, 'extension/package.json'))).version, '0.1.3-beta.0');
  assert.equal(JSON.parse(readFileSync(join(root, 'extension/package.json'))).optionalDependencies['pi-famulus-linux-x64'], '0.1.3-beta.0');
  assert.equal(JSON.parse(readFileSync(join(root, 'npm/linux-x64/package.json'))).version, '0.1.3-beta.0');
  assert.match(readFileSync(join(root, 'manager/Cargo.toml'), 'utf8'), /version = "0\.1\.3-beta\.0"/);
  assert.match(readFileSync(join(root, 'manager/Cargo.lock'), 'utf8'), /name = "pi-famulus"\nversion = "0\.1\.3-beta\.0"/);
  assert.match(readFileSync(join(root, 'extension/src/manager-client.ts'), 'utf8'), /EXTENSION_VERSION = "0\.1\.3-beta\.0"/);
  const lock = JSON.parse(readFileSync(join(root, 'extension/package-lock.json')));
  assert.equal(lock.version, '0.1.3-beta.0');
  assert.equal(lock.packages[''].version, '0.1.3-beta.0');
  assert.deepEqual(lock.packages[''].optionalDependencies, Object.fromEntries(PLATFORMS.map(p => [p.name, '0.1.3-beta.0'])));
  for (const p of PLATFORMS) assert.equal(lock.packages[`../npm/${p.id}`].version, '0.1.3-beta.0');
  validateMetadata(root, { tag: 'v0.1.3-beta.0', repository });
});

test('publication guards cannot be satisfied by comments or other jobs', () => {
  const publish = readFileSync(new URL('../.github/workflows/publish.yml', import.meta.url), 'utf8');
  assertPublishingAuthority(publish);
  assert.throws(() => assertPublishingAuthority(publish.replace('  queue: max', '  # queue: max')), /release queue/);
  assert.throws(() => assertPublishingAuthority(publish.replace("    if: github.ref == 'refs/heads/main'", "    # if: github.ref == 'refs/heads/main'")), /validate job/);
  const movedGuard = publish.replace("    if: github.ref == 'refs/heads/main'\n", '').replace('  publish:\n', "  publish:\n    if: github.ref == 'refs/heads/main'\n");
  assert.throws(() => assertPublishingAuthority(movedGuard), /validate job/);
});

function assertExtensionSourceInstall(ci) {
  const job = ci.match(/^  extension:\n([\s\S]*?)(?=^  [\w-]+:|(?![\s\S]))/m)?.[1] ?? '';
  assert.ok(job.includes('      - run: npm ci\n      - run: npx tsc --noEmit\n'), 'extension source job must install full dependencies before typecheck');
}

test('extension source install guard cannot be satisfied by other jobs', () => {
  const ci = readFileSync(new URL('../.github/workflows/ci.yml', import.meta.url), 'utf8');
  assertExtensionSourceInstall(ci);
  const missingSourceInstall = ci.replace('      - run: npm ci\n      - run: npx tsc --noEmit\n', '      - run: npx tsc --noEmit\n');
  assert.ok(missingSourceInstall.includes('- run: npm ci\n'), 'other jobs still install dependencies');
  assert.throws(() => assertExtensionSourceInstall(missingSourceInstall), /extension source job/);
});

test('shell-expression guard covers blank lines, indentation and scalar styles', () => {
  for (const scalar of ['|', '|-', '>', '>+']) {
    for (const indent of [8, 10, 12]) {
      const workflow = `jobs:\n  publish:\n    steps:\n${' '.repeat(indent - 4)}- run: ${scalar}\n${' '.repeat(indent)}echo safe\n\n${' '.repeat(indent)}echo \${{ inputs.tag }}`;
      assert.match(workflowRunBodies(workflow), /\$\{\{/, `guard must cover ${scalar} at ${indent} spaces after a blank line`);
    }
  }
  assert.match(workflowRunBodies('      - run: echo "${{ inputs.tag }}"'), /\$\{\{/);
  assert.ok(!/\$\{\{/.test(workflowRunBodies('      - run: echo safe\n        env:\n          TAG: ${{ inputs.tag }}')));
});

test('explicit empty release tags are rejected by metadata and the actual CLI', t => {
  const { root } = fixture(t);
  assert.throws(() => validateMetadata(root, { tag: '' }), /release tag must/);
  assert.throws(() => execFileSync(process.execPath, [fileURLToPath(new URL('./validate-release.mjs', import.meta.url)), '--tag', ''], { cwd: root, stdio: 'pipe' }), /release tag must/);
});

test('native files whitelist cannot ship an entire bin directory', t => {
  const { root, put } = fixture(t);
  const path = 'npm/linux-x64/package.json';
  const pkg = JSON.parse(readFileSync(join(root, path)));
  put(path, { ...pkg, files: ['bin'] });
  assert.throws(() => validateMetadata(root), /native files/);
});

test('actual checkout and CLI accept the renamed GitHub repository and reject the old identity', () => {
  const root = fileURLToPath(new URL('../', import.meta.url));
  const cli = fileURLToPath(new URL('./validate-release.mjs', import.meta.url));
  assert.equal(validateMetadata(root, { repository: 'Leechael/pi-famulus' }).length, 5);
  const output = execFileSync(process.execPath, [cli], {
    cwd: root, encoding: 'utf8', env: { ...process.env, GITHUB_REPOSITORY: 'Leechael/pi-famulus' }, stdio: 'pipe',
  });
  assert.match(output, /Validated all five packages/);
  assert.throws(() => validateMetadata(root, { repository: 'Leechael/pi-better-subagents' }), /canonical GitHub repository/);
  assert.throws(() => execFileSync(process.execPath, [cli], {
    cwd: root, encoding: 'utf8', env: { ...process.env, GITHUB_REPOSITORY: 'Leechael/pi-better-subagents' }, stdio: 'pipe',
  }), /canonical GitHub repository/);
});

test('release checkout paths with spaces and percent signs run the actual CLI regression', t => {
  const { root } = fixture(t);
  const spaced = mkdtempSync(join(tmpdir(), 'famulus checkout % '));
  t.after(() => rmSync(spaced, { recursive: true, force: true }));
  cpSync(root, spaced, { recursive: true });
  cpSync(new URL('.', import.meta.url), join(spaced, 'scripts'), { recursive: true });
  const env = { ...process.env };
  delete env.NODE_TEST_CONTEXT;
  const output = execFileSync(process.execPath, ['--test', '--test-name-pattern=^actual checkout and CLI', join(spaced, 'scripts/release.test.mjs')], { env, encoding: 'utf8', stdio: 'pipe' });
  assert.match(output, /actual checkout and CLI accept the renamed GitHub repository/);
});

test('all five packed packages include the approved MIT license', t => {
  const source = fileURLToPath(new URL('../', import.meta.url));
  const license = readFileSync(join(source, 'LICENSE'), 'utf8');
  assert.match(license, /^MIT License\n/);
  const { root, put } = fixture(t);
  const destination = join(root, 'licensed-packs');
  mkdirSync(destination);
  for (const p of validateMetadata(source)) {
    const packageLicense = readFileSync(join(source, p.directory, 'LICENSE'), 'utf8');
    assert.equal(packageLicense, license, `same license: ${p.name}`);
    assert.equal(p.metadata.license, 'MIT');
    put(`${p.directory}/package.json`, p.metadata);
    put(`${p.directory}/LICENSE`, packageLicense);
    if (p.os) {
      put(`${p.directory}/bin/pi-famulus`, '#!/bin/sh\necho license-pack-fixture\n');
      chmodSync(join(root, p.directory, 'bin/pi-famulus'), 0o755);
    }
    const [pack] = JSON.parse(execFileSync('npm', ['pack', '--json', '--pack-destination', destination], { cwd: join(root, p.directory), encoding: 'utf8', stdio: 'pipe' }));
    const packedLicense = execFileSync('tar', ['-xOzf', join(destination, pack.filename), 'package/LICENSE'], { encoding: 'utf8', stdio: 'pipe' });
    assert.equal(packedLicense, license, `license included despite files whitelist: ${p.name}`);
  }
});

test('release versions, literal repository and all four metadata contracts', t => {
  const { root, put } = fixture(t);
  assert.equal(validateMetadata(root, { tag: 'v0.1.0', repository }).length, 5);
  for (const bad of ['v01.1.0', '0.1.0', 'v1.2.3;echo pwn', 'v1.2.3\n', 'v1.2.3-beta', 'v1.2.3-nightly', 'v1.2.3-rc.1']) assert.throws(() => validateTag(bad));
  assert.equal(validateTag('v1.2.3-beta.0'), '1.2.3-beta.0');
  assert.equal(validateTag('v1.2.3-nightly.20261006'), '1.2.3-nightly.20261006');
  assert.equal(validateTag('v1.2.3-nightly.20261006.1'), '1.2.3-nightly.20261006.1');
  assert.throws(() => validateTag('v1.2.3-nightly.20250231'), /calendar day/);
  assert.throws(() => validateTag('v1.2.3-nightly.20251301'), /calendar day/);
  assert.throws(() => validateTag('v1.2.3-nightly.20251131'), /calendar day/);
  assert.throws(() => validateMetadata(root, { tag: 'v0.2.0', repository }), /version/);
  assert.throws(() => validateMetadata(root, { repository: 'other/repo' }), /repository/);
  const path = 'npm/linux-x64/package.json';
  const pkg = JSON.parse(readFileSync(join(root, path)));
  put(path, { ...pkg, scripts: { postinstall: 'bad' } });
  assert.throws(() => validateMetadata(root), /hook/);
  put(path, { ...pkg, main: './wrong' });
  assert.throws(() => validateMetadata(root), /main/);
  put(path, { ...pkg, version: '0.2.0' });
  assert.throws(() => validateMetadata(root), /version/);
  put(path, { ...pkg, exports: {} });
  assert.throws(() => validateMetadata(root), /exports/);
  put(path, { ...pkg, cpu: ['arm64'] });
  assert.throws(() => validateMetadata(root), /cpu/);
  put(path, { ...pkg, dependencies: { unexpected: '*' } });
  assert.throws(() => validateMetadata(root), /dependencies/);
  put(path, pkg);
  put('manager/Cargo.toml', '[package]\nversion = "0.2.0"\n');
  assert.throws(() => validateMetadata(root), /Cargo version/);
});

test('tag must be the checked-out commit and an ancestor of main (real git)', t => {
  const { root, put } = fixture(t);
  const git = (...args) => execFileSync('git', args, { cwd: root, stdio: 'pipe' });
  git('init', '-b', 'main'); git('config', 'user.email', 'test@example.com'); git('config', 'user.name', 'Test');
  git('add', '.'); git('commit', '-m', 'fixture'); git('tag', 'v0.1.0');
  git('update-ref', 'refs/remotes/origin/main', 'HEAD');
  validateGitTag(root, 'v0.1.0');
  put('unrelated', 'next'); git('add', '.'); git('commit', '-m', 'next');
  assert.throws(() => validateGitTag(root, 'v0.1.0'), /checked-out/);
  git('tag', 'v0.2.0');
  assert.throws(() => validateGitTag(root, 'v0.2.0'), /ancestor/);
  git('update-ref', '-d', 'refs/remotes/origin/main');
  assert.throws(() => validateGitTag(root, 'v0.2.0'), /origin\/main.*missing|fetch.*origin\/main/);
});

test('prepare fails for missing/nonexecutable/wrong-version binary; real npm tarball contains binary', t => {
  const { root, put } = fixture(t);
  const p = PLATFORMS[0];
  const path = `manager/target/${p.target}/release/pi-famulus`;
  assert.throws(() => prepareNative(root, p.id, join(root, 'dist')), /binary/);
  put(path, '#!/bin/sh\necho pi-famulus 0.2.0\n');
  assert.throws(() => prepareNative(root, p.id, join(root, 'dist')), /executable/);
  chmodSync(join(root, path), 0o755);
  assert.throws(() => prepareNative(root, p.id, join(root, 'dist')), /version/);
  put(path, '#!/bin/sh\necho pi-famulus 0.1.0+fixture\n');
  const artifact = prepareNative(root, p.id, join(root, 'dist'));
  assert.match(execFileSync('tar', ['-tzf', artifact], { encoding: 'utf8' }), /package\/bin\/pi-famulus/);
});

test('re-preparing a native package atomically replaces its executable inode', t => {
  const { root, put } = fixture(t);
  const p = PLATFORMS[0];
  const source = `manager/target/${p.target}/release/pi-famulus`;
  put(source, '#!/bin/sh\necho pi-famulus 0.1.0+first\n');
  chmodSync(join(root, source), 0o755);
  prepareNative(root, p.id, join(root, 'dist'));
  const installed = join(root, p.directory, 'bin/pi-famulus');
  const previous = statSync(installed).ino;
  put(source, '#!/bin/sh\necho pi-famulus 0.1.0+second\n');
  prepareNative(root, p.id, join(root, 'dist'));
  assert.notEqual(statSync(installed).ino, previous, 'new inode avoids macOS code-signing breakage');
  assert.match(readFileSync(installed, 'utf8'), /\+second/);
});

test('dry run uses all real packed candidates, natives first/root last, and never contacts registry', async t => {
  const { root } = packAll(t);
  const calls = [];
  await publishPackages(root, join(root, 'dist'), { tag: 'v0.1.0', repository, dryRun: true, fetchImpl: () => { throw Error('network forbidden'); }, run: (...args) => calls.push(args) });
  assert.equal(calls.length, 5);
  assert.match(calls.at(-1)[1][1], /pi-famulus-0.1.0.tgz$/);
  for (const [, args] of calls) {
    assert.ok(args.includes('--dry-run'));
    assert.ok(args.includes('--provenance'));
    assert.ok(args.includes('--access'));
    assert.equal(args[args.indexOf('--tag') + 1], 'latest');
  }
});

test('real release preflights all names before publish; bootstrap absence actionable', async t => {
  const { root } = packAll(t);
  const calls = [];
  const lookups = [];
  const missingName = PLATFORMS.at(-1).name;
  const fetchImpl = async url => {
    lookups.push(url);
    return response(url.endsWith(`/${missingName}`) || url.endsWith('/0.1.0') ? 404 : 200, {});
  };
  await assert.rejects(publishPackages(root, join(root, 'dist'), { tag: 'v0.1.0', dryRun: false, fetchImpl, run: (...args) => calls.push(args) }), /first publish.*trusted publisher \(publish\.yml, environment npm\)/i);
  assert.ok(lookups.includes(`https://registry.npmjs.org/${PLATFORMS[0].name}`), 'earlier package name exists');
  assert.ok(lookups.includes(`https://registry.npmjs.org/${missingName}`), 'a later package name is missing');
  assert.equal(calls.length, 0, 'no earlier package is published before the full preflight');
});

test('partial retry skips only byte-identical integrity; mismatch/network errors never publish', async t => {
  const { root } = packAll(t);
  const calls = [];
  const { createHash } = await import('node:crypto');
  const integrity = createHash('sha512').update(readFileSync(join(root, 'dist', 'pi-famulus-linux-x64-0.1.0.tgz'))).digest('base64');
  const fetchImpl = async url => url.endsWith('/0.1.0') ? (url.includes('linux-x64') ? response(200, { dist: { integrity: `sha512-${integrity}` } }) : response(404, {})) : response(200, {});
  await publishPackages(root, join(root, 'dist'), { tag: 'v0.1.0', dryRun: false, fetchImpl, run: (...args) => calls.push(args) });
  assert.equal(calls.length, 4);
  assert.match(calls.at(-1)[1][1], /pi-famulus-0.1.0.tgz$/);
  await assert.rejects(publishPackages(root, join(root, 'dist'), { tag: 'v0.1.0', dryRun: false, fetchImpl: async () => response(200, { dist: { integrity: 'sha512-other' } }), run: () => assert.fail('publish') }), /integrity/);
  await assert.rejects(publishPackages(root, join(root, 'dist'), { tag: 'v0.1.0', dryRun: false, fetchImpl: async () => response(503, {}), run: () => assert.fail('publish') }), /503/);
});

test('missing artifacts fail before any publication', async t => {
  const { root } = packAll(t);
  const opts = { tag: 'v0.1.0', dryRun: true, run: () => assert.fail('publish') };
  rmSync(join(root, 'dist', 'pi-famulus-darwin-x64-0.1.0.tgz'));
  await assert.rejects(publishPackages(root, join(root, 'dist'), opts), /artifact/);
});

test('tampered metadata and omitted native binary in real tarballs fail closed', async t => {
  const { root, put } = packAll(t);
  const directory = 'npm/linux-x64';
  const pkg = JSON.parse(readFileSync(join(root, directory, 'package.json')));
  const repack = () => execFileSync('npm', ['pack', '--json', '--pack-destination', join(root, 'dist')], { cwd: join(root, directory), stdio: 'pipe' });
  const opts = { tag: 'v0.1.0', dryRun: true, run: () => assert.fail('publish') };
  put(`${directory}/package.json`, { ...pkg, main: './tampered' });
  repack();
  put(`${directory}/package.json`, pkg);
  await assert.rejects(publishPackages(root, join(root, 'dist'), opts), /artifact metadata/);
  rmSync(join(root, directory, 'bin/pi-famulus'));
  repack();
  await assert.rejects(publishPackages(root, join(root, 'dist'), opts), /package\/bin\/pi-famulus/);
});

test('real npm publish dry-run validates all five packed artifacts without publication', async t => {
  // npm rejects dry-run republication of versions that already exist on the
  // registry, so the real-registry smoke test must pack a version below the
  // first real release that can never be published.
  const { root } = packAll(t, '0.0.1');
  await publishPackages(root, join(root, 'dist'), { tag: 'v0.0.1', dryRun: true, fetchImpl: () => assert.fail('registry lookup'), run: (command, args, options) => {
    assert.ok(args.includes('--dry-run'));
    // 0.0.1 is below the latest published version, so an explicit throwaway
    // dist-tag avoids npm's implicit-latest restriction; production releases
    // never use this version or tag.
    execFileSync(command, [...args, '--tag', 'famulus-smoke'], { ...options, stdio: 'pipe' });
  } });
});

test('workflow literal security, release graph and four host/target contracts', () => {
  const workflow = name => readFileSync(new URL(`../.github/workflows/${name}.yml`, import.meta.url), 'utf8');
  const native = workflow('native-packages');
  for (const p of PLATFORMS) { assert.ok(native.includes(`platform: ${p.id}`)); assert.ok(native.includes(`target: ${p.target}`)); }
  for (const runner of ['ubuntu-24.04', 'ubuntu-24.04-arm', 'macos-15-intel', 'macos-15']) assert.ok(native.includes(`runner: ${runner}\n`));
  assert.ok(native.includes('cargo test --locked\n'));
  assert.ok(native.includes('cargo test --locked --features test-clock'));
  assert.ok(native.includes('resolveManagerPath(DEFAULT_CONFIG'));
  assert.ok(native.includes("packages['node_modules/@earendil-works/pi-coding-agent'].version"));
  assert.ok(native.includes('"@earendil-works/pi-coding-agent@$pi_version"'));
  assert.ok(!native.includes('id-token:'));
  const ci = workflow('ci');
  assert.ok(!ci.includes('npm ci --omit=optional'), 'source installs must retain TypeScript/Rollup native optional bindings');
  assertExtensionSourceInstall(ci);
  assert.ok(ci.includes('npm run test:graders'));
  assert.ok(ci.includes('/tmp/eval-*'));
  const publish = workflow('publish');
  assert.ok(publish.includes('needs: [validate, tests]'));
  assert.ok(publish.includes('uses: ./.github/workflows/ci.yml'));
  assert.ok(publish.includes('cancel-in-progress: false'));
  assertPublishingAuthority(publish);
  assert.ok(!/^  release:/m.test(publish), 'tag-triggered workflows cannot use the main-only publishing environment');
  assert.ok(publish.includes('default: true'));
  assert.ok(publish.includes('type: choice'));
  for (const channel of ['beta', 'nightly', 'patch', 'minor', 'major']) {
    assert.match(publish, new RegExp(`^          - ${channel}$`, 'm'), channel);
  }
  assert.ok(publish.includes('scripts/next-release.mjs'));
  assert.ok(publish.includes('scripts/bump-release.mjs'));
  assert.ok(!publish.includes('Existing release tag'));
  assert.equal((publish.match(/id-token: write/g) ?? []).length, 1);
  assert.equal((publish.match(/^    environment: npm$/gm) ?? []).length, 1);
  assert.ok(!/NPM_TOKEN|NODE_AUTH_TOKEN|npm whoami|npm login/.test(publish));
  const runBodies = workflowRunBodies(publish);
  assert.ok(!/\$\{\{[^}]+\}\}/.test(runBodies));
});
