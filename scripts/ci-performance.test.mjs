import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';

const workflow = name => readFileSync(new URL(`../.github/workflows/${name}.yml`, import.meta.url), 'utf8').replaceAll('\r\n', '\n');
const job = (text, name) => text.match(new RegExp(`^  ${name}:\\n([\\s\\S]*?)(?=^  [\\w-]+:|(?![\\s\\S]))`, 'm'))?.[1] ?? '';

test('clock lanes preserve every platform and both full Cargo suites independently of packaging', () => {
  const native = workflow('native-packages');
  const tests = job(native, 'manager-tests');
  for (const platform of ['linux-x64', 'linux-arm64', 'darwin-x64', 'darwin-arm64', 'win32-x64', 'win32-arm64']) {
    assert.ok(tests.includes(`platform: ${platform}\n`), platform);
  }
  assert.match(tests, /clock: \[real, manual\]/);
  assert.match(tests, /- run: cargo test --locked\n\s+if: matrix.clock == 'real'/);
  assert.match(tests, /- run: cargo test --locked --features test-clock\n\s+if: matrix.clock == 'manual'/);
  assert.doesNotMatch(tests, /--skip|continue-on-error|needs:/);
  const packs = job(native, 'native');
  assert.doesNotMatch(packs, /needs:/, 'release builds must not wait for debug tests');
  assert.match(packs, /cargo test --locked --release --test resource_bench/);
  assert.match(packs, /cargo test --locked --release --target "\$TARGET" --test resource_bench/);
  assert.match(packs, /cargo build --locked --release --target "\$TARGET"/);
  const smoke = packs.slice(packs.indexOf('- name: Install tarballs'));
  assert.ok(smoke.indexOf('npm_config_cache=$(npm config get cache)') >= 0);
  assert.ok(smoke.indexOf('export npm_config_cache\n') < smoke.indexOf('export HOME='), 'npm cache path must be captured before HOME isolation');
  assert.match(tests, /shared-key: manager-tests-\$\{\{ matrix.platform \}\}/);
  assert.match(tests, /save-if: \$\{\{ matrix.clock == 'real' \}\}/);
  assert.match(tests, /CARGO_PROFILE_DEV_DEBUG: line-tables-only/);
});

test('native smoke isolation still uses the npm cache restored by setup-node', t => {
  const packs = job(workflow('native-packages'), 'native');
  const smoke = packs.slice(packs.indexOf('- name: Install tarballs'));
  const preamble = smoke.match(/        run: \|\n([\s\S]*?)          unset PI_FAMULUS_MANAGER_PATH/)[1];
  const directory = mkdtempSync(join(tmpdir(), 'famulus-ci-cache-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  const result = spawnSync('bash', ['-eo', 'pipefail', '-c', `before=$(npm config get cache)\n${preamble}\ntest "$(npm config get cache)" = "$before"`], {
    env: { ...process.env, RUNNER_TEMP: directory }, encoding: 'utf8',
  });
  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
});

test('docs-only gating never skips the workflow or masks failed scope classification', () => {
  const ci = workflow('ci');
  assert.doesNotMatch(ci, /paths-ignore:|\n    paths:/);
  for (const name of ['native-packages', 'clippy', 'extension-compat', 'extension-windows', 'e2e']) {
    assert.match(job(ci, name), /if: needs.pi-matrix.outputs.code == 'true'/, name);
  }
  const aggregate = job(ci, 'extension');
  assert.match(aggregate, /if: \$\{\{ !cancelled\(\) \}\}/);
  assert.match(aggregate, /needs: \[pi-matrix, native-packages, clippy, extension-compat, extension-windows, e2e\]/);
  assert.match(aggregate, /RESULTS: \$\{\{ toJSON\(needs\) \}\}/);
  assert.match(aggregate, /assert.equal\(scope.result, 'success'/);
  assert.match(aggregate, /scope.outputs.code === 'true' \? 'success' : 'skipped'/);
});
