import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

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
  assert.match(tests, /shared-key: manager-tests-\$\{\{ matrix.platform \}\}/);
  assert.match(tests, /save-if: \$\{\{ matrix.clock == 'real' \}\}/);
  assert.match(tests, /CARGO_PROFILE_DEV_DEBUG: line-tables-only/);
});

test('docs-only gating never skips the workflow or masks failed scope classification', () => {
  const ci = workflow('ci');
  assert.doesNotMatch(ci, /paths-ignore:|\n    paths:/);
  for (const name of ['native-packages', 'clippy', 'extension-compat', 'extension-windows', 'e2e']) {
    assert.match(job(ci, name), /if: needs.pi-matrix.outputs.code == 'true'/, name);
  }
  const aggregate = job(ci, 'extension');
  assert.match(aggregate, /if: always\(\)/);
  assert.match(aggregate, /test "\$SCOPE_RESULT" = success/);
  assert.match(aggregate, /test "\$CODE" = true/);
  assert.match(aggregate, /test "\$COMPAT_RESULT" = success/);
  assert.match(aggregate, /test "\$COMPAT_RESULT" = skipped/);
});
