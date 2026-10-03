import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, readdirSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { npmSync } from './prepare-native.mjs';
import { validateMetadata, validateTag, REPOSITORY } from './validate-release.mjs';
import { distTagForVersion } from './next-release.mjs';

const REGISTRY = 'https://registry.npmjs.org';

// Validate the actual downloaded tarballs, not just their filenames or source manifests.
export function validateArtifacts(root, directory, options) {
  const packages = validateMetadata(root, options);
  const filenames = readdirSync(directory).filter(f => f.endsWith('.tgz')).sort();
  assert.deepEqual(filenames, packages.map(p => `${p.name}-${p.metadata.version}.tgz`).sort(), `artifact set must contain exactly all ${packages.length} release packages`);
  return [...packages.slice(1), packages[0]].map(p => {
    const artifact = join(directory, `${p.name}-${p.metadata.version}.tgz`);
    const unpack = path => execFileSync('tar', ['-xOzf', artifact, `package/${path}`], { maxBuffer: 32 * 1024 * 1024, stdio: ['ignore', 'pipe', 'pipe'] });
    assert.deepEqual(JSON.parse(unpack('package.json')), p.metadata, `artifact metadata mismatch: ${p.name}`);
    for (const path of p.os ? [p.binary] : ['bin/pi-famulus.js', 'src/native-manager.js', 'src/config.ts', 'README.md']) assert.ok(unpack(path).length, `artifact missing ${path}: ${p.name}`);
    const integrity = `sha512-${createHash('sha512').update(readFileSync(artifact)).digest('base64')}`;
    return { ...p, artifact, integrity };
  });
}

export async function publishPackages(root, directory, { tag, dryRun = true, repository = REPOSITORY, fetchImpl = fetch, run = execFileSync } = {}) {
  validateTag(tag);
  const candidates = validateArtifacts(root, directory, { tag, repository });
  const lookup = async path => {
    const response = await fetchImpl(`${REGISTRY}/${path}`, { signal: AbortSignal.timeout(30_000) });
    if (response.status === 404) return null;
    assert.ok(response.ok, `registry preflight failed (HTTP ${response.status}) for ${path}; no publish attempted`);
    return response.json();
  };
  const pending = [];
  // Resolve all availability and integrity checks before the first mutating operation.
  for (const p of candidates) {
    if (!dryRun) {
      assert.ok(await lookup(p.name), `${p.name} is not available on npm: first publish must be performed interactively, then configure its trusted publisher (publish.yml, environment npm)`);
      const existing = await lookup(`${p.name}/${p.metadata.version}`);
      if (existing) {
        assert.equal(existing.dist?.integrity, p.integrity, `registry integrity differs for ${p.name}@${p.metadata.version}; refusing to skip or overwrite`);
        console.log(`Already published with identical integrity: ${p.name}@${p.metadata.version}`);
        continue;
      }
    }
    pending.push(p);
  }
  for (const p of pending) {
    const distTag = distTagForVersion(p.metadata.version);
    console.log(`${dryRun ? 'Dry run' : 'Publish'}: ${p.name}@${p.metadata.version} --tag ${distTag}`);
    // Tests inject only the execution boundary, not a different npm launch path.
    const args = ['publish', p.artifact, '--access', 'public', '--provenance', '--registry', REGISTRY, '--tag', distTag, ...(dryRun ? ['--dry-run'] : [])];
    npmSync(args, { cwd: root, stdio: 'inherit' }, run);
  }
  if (dryRun) console.log('Dry run validates packaging only; it does NOT prove OIDC authentication or trusted-publisher bindings.');
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const [directory, tag, mode] = process.argv.slice(2);
    assert.ok(process.argv.length === 5 && ['--dry-run', '--publish'].includes(mode), 'usage: publish-packages.mjs <artifact-directory> vX.Y.Z <--dry-run|--publish>');
    await publishPackages(process.cwd(), resolve(directory), { tag, dryRun: mode === '--dry-run', repository: process.env.GITHUB_REPOSITORY ?? REPOSITORY });
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
