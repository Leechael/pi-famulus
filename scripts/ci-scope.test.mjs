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

// Replay the declared filter against literal boundary cases. These patterns use
// only portable *, **, and literal paths; no picomatch-only syntax is needed.
// The exact allowlist assertion also prevents an untested new skip category.
test('only the reviewed documentation allowlist can skip code checks', () => {
  const action = read(actionPath);
  const patterns = [...action.matchAll(/^\s+- '(![^']+)'$/gm)].map(match => match[1]);
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
  assert.match(matrix, /code: \$\{\{ steps\.scope\.outputs\.code \}\}/);
  assert.ok(matrix.indexOf('uses: actions/checkout@') < matrix.indexOf('uses: ./.github/actions/ci-scope'));
  assert.match(matrix, /- id: scope\n        uses: \.\/\.github\/actions\/ci-scope/);
  const drift = workflow.slice(workflow.indexOf('\n  drift:'));
  assert.match(drift, /needs: pi-matrix/);
  for (const code of ['false', 'true', undefined]) {
    assert.equal(evaluate(expression(drift, 'if'), { needs: { 'pi-matrix': { outputs: { code } } } }), code === 'true');
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
  for (const name of ['native-packages', 'clippy', 'extension-compat', 'extension-windows', 'e2e']) {
    const block = job(workflow, name);
    assert.match(block, /needs: pi-matrix/);
    const gate = block.match(/^    if: (.+)$/m)?.[1]?.replace(/^\$\{\{ (.+) \}\}$/, '$1');
    assert.ok(gate, `missing gate: ${name}`);
    for (const code of ['false', 'true', undefined]) {
      assert.equal(evaluate(gate, { needs: { 'pi-matrix': { outputs: { code } } } }), code === 'true', name);
    }
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
  const fixture = (code, result) => ({
    'pi-matrix': { result: 'success', outputs: { code } },
    ...Object.fromEntries(families.map(name => [name, { result }])),
  });
  const cases = [
    ['docs-only: all families skipped', fixture('false', 'skipped'), true],
    ['code: all families succeeded', fixture('true', 'success'), true],
  ];
  for (const [code, expected, rejected] of [
    ['true', 'success', ['failure', 'cancelled', 'skipped']],
    ['false', 'skipped', ['failure', 'cancelled', 'success']],
  ]) {
    for (const name of families) {
      for (const result of rejected) {
        const results = fixture(code, expected);
        results[name].result = result;
        cases.push([`code=${code}: ${name} ${result}`, results, false]);
      }
      const missing = fixture(code, expected);
      delete missing[name];
      cases.push([`code=${code}: missing ${name}`, missing, false]);
    }
    for (const scope of [
      undefined,
      { result: 'failure', outputs: { code } },
      { result: 'cancelled', outputs: { code } },
      { result: 'skipped', outputs: { code } },
      { result: 'success' },
      { result: 'success', outputs: {} },
      { result: 'success', outputs: { code: '' } },
      { result: 'success', outputs: { code: true } },
    ]) {
      const results = fixture(code, expected);
      results['pi-matrix'] = scope;
      cases.push([`code=${code}: invalid scope ${JSON.stringify(scope)}`, results, false]);
    }
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
