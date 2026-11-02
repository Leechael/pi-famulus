import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { matchesGlob } from 'node:path';
import { runInNewContext } from 'node:vm';
import { spawnSync } from 'node:child_process';

const read = path => readFileSync(new URL(`../${path}`, import.meta.url), 'utf8').replace(/\r\n/g, '\n');
const actionPath = '.github/actions/ci-scope/action.yml';

// Evaluate the actual YAML expressions, not a copied predicate. The operators
// here are JS-compatible; Actions additionally permits hyphens in property names.
function expression(text, key) {
  const match = text.match(new RegExp(`^\\s*${key}: \\$\\{\\{ (.+?) \\}\\}\\s*$`, 'm'));
  assert.ok(match, `missing expression: ${key}`);
  return match[1];
}
const evaluate = (source, context) => runInNewContext(
  source.replace(/\.([A-Za-z_]\w*(?:-\w+)+)/g, "['$1']"), context, { timeout: 100 },
);

// Patterns of one paths-filter filter, e.g. `code:` or `rust:`.
function filterPatterns(action, name) {
  const block = action.match(new RegExp(`^ {10}${name}:\\n((?: {12}- '[^']+'\\n)+)`, 'm'))?.[1];
  assert.ok(block, `missing filter: ${name}`);
  return [...block.matchAll(/'([^']+)'/g)].map(match => match[1]);
}

// Replay the declared filter against literal boundary cases. These patterns use
// only portable *, **, and literal paths; no picomatch-only syntax is needed.
// The exact allowlist assertion also prevents an untested new skip category.
test('only the reviewed documentation allowlist can skip code checks', () => {
  const action = read(actionPath);
  const patterns = filterPatterns(action, 'code');
  assert.deepEqual(patterns, [
    '!README.md',
    '!README_CN.md',
    '!docs/**',
    '!extension/README.md',
    '!npm/**/README.md',
    '!npm/CONTEXT.md',
    '!manager/TESTING.md',
    '!manager/MONITOR_OUTPUT.md',
    '!eval/*.md',
    '!eval/ablation/fixtures/README.md',
  ]);
  assert.match(action, /predicate-quantifier: every/);
  const code = files => files.some(file => patterns.every(pattern => !matchesGlob(file, pattern.slice(1))));
  const docs = [
    'README.md', 'README_CN.md', 'docs/guide.md', 'docs/nested/guide.md',
    'extension/README.md', 'npm/README.md', 'npm/linux-x64/README.md',
    'npm/nested/package/README.md', 'npm/CONTEXT.md', 'manager/TESTING.md',
    'manager/MONITOR_OUTPUT.md', 'eval/README.md', 'eval/NOTES.md',
    'eval/ablation/fixtures/README.md',
  ];
  for (const path of docs) assert.equal(code([path]), false, path);
  const executable = [
    'extension/prompts/default.md', 'extension/prompts/nested/README.md',
    'eval/scenarios/scenario.md', 'eval/scenarios/nested/README.md',
    'eval/ablation/README.md', 'eval/ablation/fixtures/prompt.md',
    'README_OTHER.md', 'CONTEXT.md', 'manager/README.md', 'npm/pkg/CONTEXT.md',
    'docs-other/guide.md', 'extension/src/index.ts', 'manager/src/main.rs',
    'eval/package-lock.json', 'npm/pkg/package.json', '.github/workflows/ci.yml',
    '.github/pi-compat.json', '.github/actions/ci-scope/action.yml', 'scripts/readme-sync.test.mjs',
  ];
  for (const path of executable) {
    assert.equal(code([path]), true, path);
    assert.equal(code([...docs, path]), true, `mixed docs/code: ${path}`);
  }
  assert.equal(code(docs), false);
  assert.equal(code([]), false);
});

test('force and non-diff events always require code checks; PRs use their full diff', () => {
  const action = read(actionPath);
  assert.match(action, /force:\n(?:[^\n]*\n)*?    default: 'false'/);
  assert.match(action, /using: composite/);
  assert.match(action, /uses: dorny\/paths-filter@[0-9a-f]{40} # v3/);
  const output = expression(action, 'value');
  const filterIf = expression(action, 'if');
  const base = expression(action, 'base');
  for (const event of ['push', 'pull_request', 'workflow_dispatch', 'schedule', 'workflow_call', 'release']) {
    for (const force of ['false', 'true']) {
      for (const changed of ['false', 'true']) {
        const filtered = force === 'false' && ['push', 'pull_request'].includes(event);
        const context = {
          inputs: { force },
          github: { event_name: event, event: { before: 'previous-push-sha' } },
          steps: { changes: { outputs: { code: filtered ? changed : undefined } } },
        };
        assert.equal(evaluate(filterIf, context), filtered, `${event}, force=${force}`);
        assert.equal(evaluate(output, context), filtered ? changed : 'true', `${event}, force=${force}, changes=${changed}`);
        assert.equal(evaluate(base, context), event === 'push' ? 'previous-push-sha' : '');
      }
    }
  }
});

test('component areas select only their own families; anything else is shared and selects all', () => {
  const action = read(actionPath);
  const code = filterPatterns(action, 'code');
  const shared = filterPatterns(action, 'shared');
  const areas = Object.fromEntries(['rust', 'extension', 'eval'].map(name => [name, filterPatterns(action, name)]));
  assert.deepEqual(areas, { rust: ['manager/**'], extension: ['extension/**'], eval: ['eval/**'] });
  // `changes` uses every-quantifier negations; `areas` the default some-quantifier.
  const every = (patterns, files) => files.some(file => patterns.every(pattern => !matchesGlob(file, pattern.slice(1))));
  const some = (patterns, files) => files.some(file => patterns.some(pattern => matchesGlob(file, pattern)));
  const classify = files => {
    const isCode = every(code, files);
    const isShared = every(shared, files);
    const pick = name => isCode && (some(areas[name], files) || isShared);
    return { code: isCode, rust: pick('rust'), extension: pick('extension'), eval: pick('eval') };
  };
  const none = { code: true, rust: false, extension: false, eval: false };
  const all = { code: true, rust: true, extension: true, eval: true };
  const cases = [
    [['manager/src/main.rs'], { ...none, rust: true }],
    [['manager/tests/intercom.rs', 'manager/Cargo.lock'], { ...none, rust: true }],
    [['extension/src/index.ts'], { ...none, extension: true }],
    [['extension/prompts/default.md'], { ...none, extension: true }],
    [['eval/lib/process-exit-observer.ts'], { ...none, eval: true }],
    [['eval/scenarios/scenario.md'], { ...none, eval: true }],
    [['manager/src/main.rs', 'eval/e2e/faux.test.ts'], { ...none, rust: true, eval: true }],
    // Shared infrastructure fails closed: every family runs.
    [['.github/workflows/ci.yml'], all],
    [['.github/pi-compat.json'], all],
    [['.github/actions/ci-scope/action.yml'], all],
    [['scripts/install-pi-compat.sh'], all],
    [['scripts/cargo-test-parallel.mjs'], all],
    [['npm/linux-x64/package.json'], all],
    [['LICENSE'], all],
    [['.gitignore'], all],
    [['assets/logo.png'], all],
    [['extension/src/index.ts', 'README.md'], { ...none, extension: true }],
    [['docs/guide.md', 'manager/src/main.rs'], { ...none, rust: true }],
    // Docs-only: nothing.
    [['README.md', 'docs/x.md', 'manager/TESTING.md'], { code: false, rust: false, extension: false, eval: false }],
  ];
  for (const [files, expected] of cases) assert.deepEqual(classify(files), expected, files.join(', '));
});

test('component outputs are explicit strings, forced for non-diff events and docs-only aware', () => {
  const action = read(actionPath);
  for (const area of ['rust', 'extension', 'eval']) {
    const block = action.slice(action.indexOf(`  ${area}:\n    description:`));
    const output = expression(block, 'value');
    for (const event of ['push', 'pull_request', 'workflow_dispatch', 'schedule', 'workflow_call']) {
      for (const force of ['false', 'true']) {
        const filtered = force === 'false' && ['push', 'pull_request'].includes(event);
        for (const code of ['false', 'true']) {
          for (const own of ['false', 'true']) {
            for (const shared of ['false', 'true']) {
              const context = {
                inputs: { force },
                github: { event_name: event },
                steps: filtered
                  ? { changes: { outputs: { code, shared } }, areas: { outputs: { [area]: own } } }
                  : { changes: { outputs: {} }, areas: { outputs: {} } },
              };
              const expected = !filtered || (code === 'true' && (own === 'true' || shared === 'true')) ? 'true' : 'false';
              assert.equal(evaluate(output, context), expected, `${area} ${event} force=${force} code=${code} own=${own} shared=${shared}`);
            }
          }
        }
      }
    }
  }
  const areasStep = action.slice(action.indexOf('      id: areas'));
  assert.equal(expression(areasStep, 'if'), expression(action, 'if'), 'both filter steps run under the same condition');
  assert.doesNotMatch(areasStep.slice(0, areasStep.indexOf('filters:')), /predicate-quantifier/);
});

test('docs-only changes still validate README synchronization and links', () => {
  const action = read(actionPath);
  const docsStep = action.slice(action.indexOf('    - name: Check README'));
  assert.match(docsStep, /shell: bash/);
  assert.match(docsStep, /run: node --test scripts\/readme-sync\.test\.mjs/);
  for (const code of ['false', 'true', undefined]) {
    assert.equal(evaluate(expression(docsStep, 'if'), { steps: { changes: { outputs: { code } } } }), code === 'false');
  }
});

test('upstream drift passes scope through pi-matrix and gates only the expensive drift job', () => {
  const workflow = read('.github/workflows/upstream-drift.yml');
  assert.match(workflow, /permissions:\n  contents: read\n  pull-requests: read/);
  const matrix = workflow.slice(workflow.indexOf('  pi-matrix:'), workflow.indexOf('\n  drift:'));
  assert.match(matrix, /extension: \$\{\{ steps\.scope\.outputs\.extension \}\}/);
  assert.ok(matrix.indexOf('uses: actions/checkout@') < matrix.indexOf('uses: ./.github/actions/ci-scope'));
  assert.match(matrix, /- id: scope\n        uses: \.\/\.github\/actions\/ci-scope/);
  const drift = workflow.slice(workflow.indexOf('\n  drift:'));
  assert.match(drift, /needs: pi-matrix/);
  // Drift concerns pi and the extension only (shared changes select it too).
  for (const extension of ['false', 'true', undefined]) {
    assert.equal(evaluate(expression(drift, 'if'), { needs: { 'pi-matrix': { outputs: { code: 'true', extension } } } }), extension === 'true');
  }
});

for (const file of ['ci.yml', 'upstream-drift.yml']) {
test(`${file} only coalesces PR runs; main, schedule, and dispatch never share a group`, () => {
  const workflow = read(`.github/workflows/${file}`);
  const group = workflow.match(/^  group: (.+)$/m)?.[1];
  assert.ok(group);
  const cancel = expression(workflow, 'cancel-in-progress');
  const render = github => group.replace(/\$\{\{ (.+?) \}\}/g, (_, source) => evaluate(source, { github }));
  const pr = { workflow: file, event_name: 'pull_request', event: { pull_request: { number: 42 } }, ref: 'refs/pull/42/merge', run_id: 100 };
  assert.equal(render(pr), render({ ...pr, run_id: 101 }));
  assert.notEqual(render(pr), render({ ...pr, event: { pull_request: { number: 43 } } }));
  assert.equal(evaluate(cancel, { github: pr }), true);
  assert.equal(evaluate(cancel, { github: { ...pr, ref: 'refs/heads/main' } }), false);
  for (const event of ['push', 'schedule', 'workflow_dispatch', 'workflow_call', 'release']) {
    for (const ref of ['refs/heads/main', 'refs/heads/feature']) {
      const github = { workflow: file, event_name: event, event: {}, ref, run_id: 100 };
      assert.equal(evaluate(cancel, { github }), false, `${event} on ${ref}`);
      assert.notEqual(render(github), render({ ...github, run_id: 101 }), `${event} must not discard pending runs`);
    }
  }
});
}

function job(workflow, name) {
  const match = workflow.match(new RegExp(`^  ${name}:\\n([\\s\\S]*?)(?=^  [\\w-]+:|$(?![\\s\\S]))`, 'm'));
  assert.ok(match, `missing job: ${name}`);
  return match[1];
}

test('CI shares scope, forces explicit build inputs, and gates all expensive jobs', () => {
  const workflow = read('.github/workflows/ci.yml');
  assert.match(workflow, /pull-requests: read/);
  const matrix = job(workflow, 'pi-matrix');
  assert.match(matrix, /code: \$\{\{ steps\.scope\.outputs\.code \}\}/);
  assert.match(matrix, /uses: \.\/\.github\/actions\/ci-scope/);
  assert.ok(matrix.indexOf('uses: actions/checkout@') < matrix.indexOf('uses: ./.github/actions/ci-scope'));
  for (const [ref, version, expected] of [['', '', false], ['release-ref', '', true], ['', '1.2.3', true]]) {
    assert.equal(evaluate(expression(matrix, 'force'), { inputs: { ref, version } }), expected);
  }
  for (const area of ['rust', 'extension', 'eval']) {
    assert.match(matrix, new RegExp(`${area}: \\$\\{\\{ steps\\.scope\\.outputs\\.${area} \\}\\}`));
  }
  const wants = {
    'native-packages': o => o.code,
    clippy: o => o.rust,
    'extension-compat': o => o.extension,
    'extension-windows': o => o.extension,
    e2e: o => o.rust || o.extension || o.eval,
  };
  const combos = [];
  for (const code of [false, true]) for (const rust of [false, true]) for (const extension of [false, true]) for (const evalArea of [false, true]) {
    if (!code && (rust || extension || evalArea)) continue;
    combos.push({ code, rust, extension, eval: evalArea });
  }
  for (const [name, want] of Object.entries(wants)) {
    const block = job(workflow, name);
    assert.match(block, /needs: pi-matrix/);
    const gate = block.match(/^    if: (.+)$/m)?.[1]?.replace(/^\$\{\{ (.+) \}\}$/, '$1');
    assert.ok(gate, `missing gate: ${name}`);
    for (const combo of combos) {
      const outputs = Object.fromEntries(Object.entries(combo).map(([k, v]) => [k, String(v)]));
      assert.equal(evaluate(gate, { needs: { 'pi-matrix': { outputs } } }), want(combo), `${name} ${JSON.stringify(combo)}`);
    }
    // A missing output never selects a family.
    assert.equal(evaluate(gate, { needs: { 'pi-matrix': { outputs: {} } } }), false, `${name} with no outputs`);
  }
  const packages = job(workflow, 'native-packages');
  for (const combo of combos) {
    const outputs = Object.fromEntries(Object.entries(combo).map(([k, v]) => [k, String(v)]));
    const ctx = { needs: { 'pi-matrix': { outputs } } };
    assert.equal(evaluate(expression(packages, 'manager-tests'), ctx), combo.rust, `manager-tests ${JSON.stringify(combo)}`);
    assert.equal(evaluate(expression(packages, 'packages'), ctx), combo.rust || combo.extension, `packages ${JSON.stringify(combo)}`);
  }
});

test('required extension check gates every CI family and does not prolong cancelled runs', () => {
  const block = job(read('.github/workflows/ci.yml'), 'extension');
  const families = ['native-packages', 'clippy', 'extension-compat', 'extension-windows', 'e2e'];
  const dependencies = block.match(/^    needs: \[([^\]]+)\]/m)?.[1].split(/,\s*/);
  assert.deepEqual(dependencies, ['pi-matrix', ...families]);
  assert.match(block, /RESULTS: \$\{\{ toJSON\(needs\) \}\}/);
  const gate = expression(block, 'if');
  for (const status of ['success', 'failure', 'cancelled']) {
    assert.equal(evaluate(gate, {
      cancelled: () => status === 'cancelled',
      failure: () => status === 'failure',
      success: () => status === 'success',
      always: () => true,
    }), status !== 'cancelled', `aggregate scheduling after ${status}`);
  }
  // Remove YAML block indentation so the actual inline Node heredoc closes at
  // column zero, exactly as it does when Actions executes the run step.
  const script = block.match(/        run: \|\n((?:          [^\n]*\n)+)/)?.[1].replace(/^          /gm, '');
  assert.ok(script, 'missing required-check shell script');
  const want = o => ({
    'native-packages': o.code,
    clippy: o.rust,
    'extension-compat': o.extension,
    'extension-windows': o.extension,
    e2e: o.rust || o.extension || o.eval,
  });
  const fixture = scope => ({
    'pi-matrix': { result: 'success', outputs: Object.fromEntries(Object.entries(scope).map(([k, v]) => [k, String(v)])) },
    ...Object.fromEntries(Object.entries(want(scope)).map(([name, run]) => [name, { result: run ? 'success' : 'skipped' }])),
  });
  const scopes = [];
  for (const code of [false, true]) for (const rust of [false, true]) for (const extension of [false, true]) for (const evalArea of [false, true]) {
    if (!code && (rust || extension || evalArea)) continue;
    scopes.push({ code, rust, extension, eval: evalArea });
  }
  const cases = [];
  for (const scope of scopes) {
    const label = JSON.stringify(scope);
    cases.push([`${label}: exactly the selected families ran`, fixture(scope), true]);
    for (const [name, run] of Object.entries(want(scope))) {
      const rejected = run ? ['failure', 'cancelled', 'skipped'] : ['failure', 'cancelled', 'success'];
      for (const result of rejected) {
        const results = fixture(scope);
        results[name].result = result;
        cases.push([`${label}: ${name} ${result}`, results, false]);
      }
      const missing = fixture(scope);
      delete missing[name];
      cases.push([`${label}: missing ${name}`, missing, false]);
    }
  }
  const base = { code: true, rust: true, extension: false, eval: false };
  for (const bad of [
    undefined,
    { result: 'failure', outputs: fixture(base)['pi-matrix'].outputs },
    { result: 'cancelled', outputs: fixture(base)['pi-matrix'].outputs },
    { result: 'skipped', outputs: fixture(base)['pi-matrix'].outputs },
    { result: 'success' },
    { result: 'success', outputs: {} },
    { result: 'success', outputs: { code: 'true' } },
    { result: 'success', outputs: { ...fixture(base)['pi-matrix'].outputs, rust: '' } },
    { result: 'success', outputs: { ...fixture(base)['pi-matrix'].outputs, eval: true } },
    // A docs-only scope that still names a component is contradictory.
    { result: 'success', outputs: { code: 'false', rust: 'true', extension: 'false', eval: 'false' } },
  ]) {
    const results = fixture(base);
    results['pi-matrix'] = bad;
    cases.push([`invalid scope ${JSON.stringify(bad)}`, results, false]);
  }
  for (const [label, results, success] of cases) {
    const result = spawnSync('bash', ['--noprofile', '--norc', '-eo', 'pipefail', '-c', script], {
      env: { ...process.env, RESULTS: JSON.stringify(results) },
      encoding: 'utf8',
    });
    assert.ifError(result.error);
    assert.equal(result.status === 0, success, `${label}: ${result.stderr}`);
  }
});

test('PRs run one architecture per OS; main pushes, dispatches and releases run every platform', () => {
  const ci = read('.github/workflows/ci.yml');
  const native = read('.github/workflows/native-packages.yml');
  const all = expression(job(ci, 'native-packages'), 'all-architectures');
  const managerExclude = expression(job(native, 'manager-tests'), 'exclude');
  const e2eExclude = expression(job(ci, 'e2e'), 'exclude');
  const fromJSON = text => JSON.parse(text);
  const platforms = ['linux-x64', 'linux-arm64', 'darwin-x64', 'darwin-arm64', 'win32-x64', 'win32-arm64'];
  const oses = ['macos-latest', 'ubuntu-latest', 'ubuntu-24.04-arm', 'windows-latest'];
  for (const event of ['pull_request', 'push', 'workflow_dispatch', 'workflow_call', 'schedule']) {
    const pr = event === 'pull_request';
    const allArchitectures = evaluate(all, { github: { event_name: event } });
    assert.equal(allArchitectures, !pr, event);
    const excluded = evaluate(managerExclude, { fromJSON, inputs: { 'all-architectures': allArchitectures } }).map(e => e.platform);
    const ran = platforms.filter(p => !excluded.includes(p));
    assert.deepEqual(ran, pr ? ['linux-x64', 'darwin-arm64', 'win32-x64'] : platforms, `manager ${event}`);
    const e2eRan = oses.filter(os => !evaluate(e2eExclude, { fromJSON, github: { event_name: event } }).some(e => e.os === os));
    assert.deepEqual(e2eRan, pr ? ['macos-latest', 'ubuntu-latest', 'windows-latest'] : oses, `e2e ${event}`);
  }
  // Every OS keeps one manager suite and one e2e on PRs; packages are never excluded.
  assert.doesNotMatch(job(native, 'native'), /exclude:/);
  assert.doesNotMatch(job(native, 'root-package'), /exclude:/);
});
