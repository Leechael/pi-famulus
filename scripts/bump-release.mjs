import assert from 'node:assert/strict';
import { readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { PLATFORMS, validateTag } from './validate-release.mjs';

function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'));
}

function writeJson(path, value) {
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);
}

export function bumpRelease(root, version) {
  validateTag(`v${version}`);
  const extensionPkg = join(root, 'extension/package.json');
  const pkg = readJson(extensionPkg);
  pkg.version = version;
  pkg.optionalDependencies = Object.fromEntries(PLATFORMS.map(p => [p.name, version]));
  writeJson(extensionPkg, pkg);

  for (const p of PLATFORMS) {
    const path = join(root, p.directory, 'package.json');
    const native = readJson(path);
    native.version = version;
    writeJson(path, native);
  }

  const cargoPath = join(root, 'manager/Cargo.toml');
  const cargo = readFileSync(cargoPath, 'utf8');
  const bumpedCargo = cargo.replace(/(\[package\][\s\S]*?^version\s*=\s*")[^"]+(")/m, `$1${version}$2`);
  assert.ok(bumpedCargo !== cargo || cargo.includes(`version = "${version}"`), 'Cargo.toml [package] version not updated');
  writeFileSync(cargoPath, bumpedCargo);

  const lockPath = join(root, 'manager/Cargo.lock');
  const lock = readFileSync(lockPath, 'utf8');
  const bumpedLock = lock.replace(
    /(\[\[package\]\]\nname = "pi-famulus"\nversion = ")[^"]+(")/,
    `$1${version}$2`,
  );
  assert.ok(bumpedLock !== lock || lock.includes(`name = "pi-famulus"\nversion = "${version}"`), 'Cargo.lock pi-famulus version not updated');
  writeFileSync(lockPath, bumpedLock);

  const clientPath = join(root, 'extension/src/manager-client.ts');
  const client = readFileSync(clientPath, 'utf8');
  const bumpedClient = client.replace(
    /^(const EXTENSION_VERSION = ")[^"]+(";)$/m,
    `$1${version}$2`,
  );
  assert.ok(bumpedClient !== client || client.includes(`const EXTENSION_VERSION = "${version}";`), 'EXTENSION_VERSION not updated');
  writeFileSync(clientPath, bumpedClient);

  const npmLockPath = join(root, 'extension/package-lock.json');
  const npmLock = readJson(npmLockPath);
  npmLock.version = version;
  if (npmLock.packages?.['']) {
    npmLock.packages[''].version = version;
    if (npmLock.packages[''].optionalDependencies) {
      npmLock.packages[''].optionalDependencies = Object.fromEntries(PLATFORMS.map(p => [p.name, version]));
    }
  }
  for (const p of PLATFORMS) {
    const key = `../npm/${p.id}`;
    if (npmLock.packages?.[key]) npmLock.packages[key].version = version;
  }
  writeJson(npmLockPath, npmLock);
  return version;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const version = process.argv[2];
    assert.ok(process.argv.length === 3, 'usage: bump-release.mjs <version>');
    bumpRelease(process.cwd(), version);
    console.log(`Bumped all release metadata to ${version}`);
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
