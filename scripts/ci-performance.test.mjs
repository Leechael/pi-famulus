import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';

const workflow = name => readFileSync(new URL(`../.github/workflows/${name}.yml`, import.meta.url), 'utf8').replaceAll('\r\n', '\n');
const job = (text, name) => text.match(new RegExp(`^  ${name}:\\n([\\s\\S]*?)(?=^  [\\w-]+:|(?![\\s\\S]))`, 'm'))?.[1] ?? '';

test('one complete simulated-clock suite per platform, independent of packaging', () => {
  const native = workflow('native-packages');
  const tests = job(native, 'manager-tests');
  // Rows (runner, target) come from .github/native-platforms.json via the
  // platforms job; no include entries can re-add excluded platforms.
  assert.match(tests, /needs: platforms\n/);
  assert.match(tests, /matrix: \$\{\{ fromJSON\(needs\.platforms\.outputs\.matrix\) \}\}/);
  assert.doesNotMatch(tests, /^\s+include:/m);
  // The real-clock rerun of the whole suite is gone: waits are simulated,
  // and the real-time canaries (Home::new_real) run inside this suite.
  assert.doesNotMatch(tests, /clock: \[|matrix\.clock/);
  assert.match(tests, /- run: node \.\.\/scripts\/cargo-test-parallel\.mjs --locked --features test-clock\n\s+if: runner.os == 'Linux'/);
  assert.match(tests, /- run: cargo test --locked --features test-clock\n\s+if: runner.os != 'Linux'/);
  assert.equal((tests.match(/(cargo test|cargo-test-parallel\.mjs) --locked/g) ?? []).length, 2, 'exactly one suite command per OS family');
  assert.match(tests, /if: format\('\{0\}', inputs.manager-tests\) == 'true'/);
  assert.match(tests, /timeout-minutes: 45/);
  assert.doesNotMatch(tests, /--skip|continue-on-error/);
  assert.deepEqual([...tests.matchAll(/needs: (.+)/g)].map(m => m[1]), ['platforms'], 'suites wait only for the platform list');
  const packs = job(native, 'native');
  assert.deepEqual([...packs.matchAll(/needs: (.+)/g)].map(m => m[1]), ['platforms'], 'release builds must not wait for debug tests');
  assert.match(packs, /cargo test --locked --release --test resource_bench/);
  assert.match(packs, /cargo test --locked --release --target "\$TARGET" --test resource_bench/);
  assert.match(packs, /cargo build --locked --release --target "\$TARGET"/);
  const smoke = packs.slice(packs.indexOf('- name: Install tarballs'));
  assert.ok(smoke.indexOf('npm_config_cache=$(npm config get cache)') >= 0);
  assert.ok(smoke.indexOf('export npm_config_cache\n') < smoke.indexOf('export HOME='), 'npm cache path must be captured before HOME isolation');
  assert.match(tests, /shared-key: manager-tests-\$\{\{ matrix.platform \}\}/);
  assert.match(tests, /CARGO_PROFILE_DEV_DEBUG: line-tables-only/);
});

test('portable typechecks are not repeated but every installed pi version is checked', () => {
  const ci = workflow('ci');
  const compat = job(ci, 'extension-compat');
  const checks = compat.split(/(?=^      - )/m).filter(step => step.startsWith('      - run: npx tsc --noEmit'));
  assert.equal(checks.length, 2, 'locked source and installed compatibility phases');
  assert.match(checks[0], /if: matrix.pi-version == '1.0.0'/);
  assert.doesNotMatch(checks[1], /\n        if:/, 'every selected pi version must be typechecked');
  assert.match(job(ci, 'e2e'), /- run: npm run typecheck\n        working-directory: eval\n        if: matrix.os == 'ubuntu-latest'/);
  assert.match(job(ci, 'extension-windows'), /- run: npx tsc --noEmit/);
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

test('scope gating never skips the workflow and routes each family by its own area', () => {
  const ci = workflow('ci');
  assert.doesNotMatch(ci, /paths-ignore:|\n    paths:/);
  const gates = {
    'native-packages': "if: needs.pi-matrix.outputs.code == 'true'",
    clippy: "if: needs.pi-matrix.outputs.rust == 'true'",
    'extension-compat': "if: needs.pi-matrix.outputs.extension == 'true'",
    'extension-windows': "if: needs.pi-matrix.outputs.extension == 'true'",
    e2e: "if: needs.pi-matrix.outputs.rust == 'true' || needs.pi-matrix.outputs.extension == 'true' || needs.pi-matrix.outputs.eval == 'true'",
  };
  for (const [name, gate] of Object.entries(gates)) assert.ok(job(ci, name).includes(`    ${gate}\n`), name);
  const packages = job(ci, 'native-packages');
  assert.match(packages, /manager-tests: \$\{\{ needs.pi-matrix.outputs.rust == 'true' \}\}/);
  assert.match(packages, /packages: \$\{\{ needs.pi-matrix.outputs.rust == 'true' \|\| needs.pi-matrix.outputs.extension == 'true' \}\}/);
  const aggregate = job(ci, 'extension');
  assert.match(aggregate, /if: \$\{\{ !cancelled\(\) \}\}/);
  assert.match(aggregate, /needs: \[pi-matrix, native-packages, clippy, extension-compat, extension-windows, e2e\]/);
  assert.match(aggregate, /RESULTS: \$\{\{ toJSON\(needs\) \}\}/);
  assert.match(aggregate, /assert.equal\(scope.result, 'success'/);
});
