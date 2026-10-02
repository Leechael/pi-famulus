import assert from 'node:assert/strict';
import { chmodSync, copyFileSync, mkdirSync, mkdtempSync, renameSync, rmSync, statSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { PLATFORMS, validateMetadata } from './validate-release.mjs';

export function prepareNative(root, platform, destination) {
  const p = PLATFORMS.find(p => p.id === platform);
  assert.ok(p, `unsupported platform: ${platform}`);
  const packages = validateMetadata(root);
  const version = packages[0].metadata.version;
  const exeName = p.os === 'win32' ? 'pi-famulus.exe' : 'pi-famulus';
  const binary = join(root, 'manager/target', p.target, 'release', exeName);
  let stat;
  try { stat = statSync(binary); } catch { throw new Error(`missing native binary: ${binary}`); }
  assert.ok(stat.isFile() && stat.size > 0 && (p.os === 'win32' || (stat.mode & 0o111)), `native binary must be a nonempty executable: ${binary}`);
  const output = execFileSync(binary, ['--version'], { encoding: 'utf8', timeout: 10_000 }).trim();
  assert.match(output, new RegExp(`^pi-famulus ${version.replaceAll('.', '\\.')}([+][\\w.-]+)?$`), 'native binary version must match package version');
  const binDir = join(root, p.directory, 'bin');
  mkdirSync(binDir, { recursive: true });
  // A new inode is essential on macOS: overwriting a previously executed
  // Mach-O file in place can invalidate its code-signing cache.
  // npm package always exposes `bin/pi-famulus` (no .exe) so the resolver is OS-agnostic.
  const stage = mkdtempSync(join(binDir, '.stage-'));
  try {
    const stagedBinary = join(stage, 'pi-famulus');
    copyFileSync(binary, stagedBinary);
    if (p.os !== 'win32') chmodSync(stagedBinary, 0o755);
    renameSync(stagedBinary, join(binDir, 'pi-famulus'));
  } finally {
    rmSync(stage, { recursive: true, force: true });
  }
  destination = resolve(destination);
  mkdirSync(destination, { recursive: true });
  const [pack] = JSON.parse(execFileSync('npm', ['pack', '--json', '--pack-destination', destination], { cwd: join(root, p.directory), encoding: 'utf8' }));
  assert.equal(pack.name, p.name);
  assert.equal(pack.version, version);
  assert.ok(pack.files.some(f => f.path === 'bin/pi-famulus' && f.size > 0), 'packed artifact must contain native binary');
  return join(destination, pack.filename);
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    assert.equal(process.argv.length, 4, 'usage: prepare-native.mjs <os-arch> <pack-destination>');
    console.log(prepareNative(process.cwd(), process.argv[2], process.argv[3]));
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
