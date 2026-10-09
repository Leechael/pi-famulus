import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';

// Exercise the Windows launch path in an isolated Node process on every OS.
// The fake npm CLI is actually executed: accidentally selecting npx fails.
test('Windows npmSync ignores npx npm_execpath and preserves literal argv without a shell', t => {
  const root = mkdtempSync(join(tmpdir(), 'famulus npm % & '));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const npm = join(root, 'npm-cli.js');
  const npx = join(root, 'npx-cli.js');
  writeFileSync(npm, 'console.log(JSON.stringify(process.argv.slice(2)))');
  writeFileSync(npx, 'process.stderr.write("npx was invoked instead of npm"); process.exit(99)');
  const args = ['pack', '--pack-destination', join(root, 'packs & %PATH%')];
  const script = `
    import { npmSync } from ${JSON.stringify(new URL('./prepare-native.mjs', import.meta.url).href)};
    Object.defineProperty(process, 'platform', { value: 'win32' });
    process.stdout.write(npmSync(${JSON.stringify(args)}, { encoding: 'utf8' }));
  `;
  for (const npm_execpath of [npx, npm]) {
    const result = execFileSync(process.execPath, ['--input-type=module', '-e', script], {
      encoding: 'utf8', env: { ...process.env, npm_execpath, PATH: root },
    });
    assert.deepEqual(JSON.parse(result), args);
  }
});
