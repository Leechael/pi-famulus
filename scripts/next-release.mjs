import assert from 'node:assert/strict';
import { readFileSync, appendFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { execFileSync } from 'node:child_process';
import { validateTag } from './validate-release.mjs';

export const CHANNELS = ['beta', 'nightly', 'patch', 'minor', 'major'];
export const REGISTRY = 'https://registry.npmjs.org';
export const ROOT_PACKAGE = 'pi-famulus';

const STABLE = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;

export function distTagForVersion(version) {
  if (version.includes('-beta.')) return 'beta';
  if (version.includes('-nightly.')) return 'nightly';
  return 'latest';
}

export function parseStable(version) {
  const m = STABLE.exec(version);
  assert.ok(m, `stable version required, got ${version}`);
  return { major: Number(m[1]), minor: Number(m[2]), patch: Number(m[3]) };
}

function cmpParts(a, b) {
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
  }
  return 0;
}

export function compareStable(a, b) {
  const pa = parseStable(a);
  const pb = parseStable(b);
  return cmpParts([pa.major, pa.minor, pa.patch], [pb.major, pb.minor, pb.patch]);
}

export function bumpStable(version, channel) {
  const { major, minor, patch } = parseStable(version);
  if (channel === 'major') return `${major + 1}.0.0`;
  if (channel === 'minor') return `${major}.${minor + 1}.0`;
  if (channel === 'patch') return `${major}.${minor}.${patch + 1}`;
  throw new Error(`stable bump requires patch|minor|major, got ${channel}`);
}

function maxStable(versions) {
  const stables = versions.filter(v => STABLE.test(v));
  if (stables.length === 0) return null;
  return stables.reduce((a, b) => (compareStable(a, b) >= 0 ? a : b));
}

function takenSet(registryVersions, gitTags) {
  const out = new Set(registryVersions);
  for (const tag of gitTags) {
    if (tag.startsWith('v')) out.add(tag.slice(1));
  }
  return out;
}

function utcDay(now) {
  const d = new Date(now);
  const y = d.getUTCFullYear();
  const m = String(d.getUTCMonth() + 1).padStart(2, '0');
  const day = String(d.getUTCDate()).padStart(2, '0');
  return `${y}${m}${day}`;
}

export function resolveNextVersion({
  channel,
  packageVersion,
  registryVersions = [],
  gitTags = [],
  now = new Date(),
} = {}) {
  assert.ok(CHANNELS.includes(channel), `channel must be one of ${CHANNELS.join(', ')}`);
  assert.match(packageVersion ?? '', STABLE, 'package.json version must be stable X.Y.Z');
  const taken = takenSet(registryVersions, gitTags);
  const publishedMax = maxStable([...registryVersions, ...[...taken]]);
  const base = publishedMax && compareStable(publishedMax, packageVersion) > 0 ? publishedMax : packageVersion;

  if (channel === 'patch' || channel === 'minor' || channel === 'major') {
    const next = bumpStable(base, channel);
    if (taken.has(next)) {
      throw new Error(`${next} is already published or tagged; refuse to reuse a stable version`);
    }
    return { version: next, tag: `v${next}`, distTag: 'latest' };
  }

  const series = bumpStable(base, 'patch');
  if (channel === 'beta') {
    const re = new RegExp(`^${series.replaceAll('.', '\\.')}-beta\\.(0|[1-9]\\d*)$`);
    let n = 0;
    for (const v of taken) {
      const m = re.exec(v);
      if (m) n = Math.max(n, Number(m[1]) + 1);
    }
    const version = `${series}-beta.${n}`;
    return { version, tag: `v${version}`, distTag: 'beta' };
  }

  const day = utcDay(now);
  const prefix = `${series}-nightly.${day}`;
  let version = prefix;
  if (taken.has(version)) {
    let k = 1;
    while (taken.has(`${prefix}.${k}`)) k += 1;
    version = `${prefix}.${k}`;
  }
  return { version, tag: `v${version}`, distTag: 'nightly' };
}

export async function readRegistryVersions(name = ROOT_PACKAGE, fetchImpl = fetch) {
  const response = await fetchImpl(`${REGISTRY}/${name}`, { signal: AbortSignal.timeout(30_000) });
  if (response.status === 404) return [];
  assert.ok(response.ok, `registry lookup failed (HTTP ${response.status}) for ${name}`);
  const body = await response.json();
  return Object.keys(body.versions ?? {});
}

export function readGitTags(root) {
  try {
    const out = execFileSync('git', ['tag', '--list', 'v*'], {
      cwd: root,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return out.split('\n').map(s => s.trim()).filter(Boolean);
  } catch (cause) {
    throw new Error('git tag --list failed; refusing to compute a version without the tag set', { cause });
  }
}

export function readPackageVersion(root) {
  return JSON.parse(readFileSync(resolve(root, 'extension/package.json'), 'utf8')).version;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const args = process.argv.slice(2);
    const channelIdx = args.indexOf('--channel');
    assert.ok(channelIdx >= 0 && args[channelIdx + 1], 'usage: next-release.mjs --channel <beta|nightly|patch|minor|major>');
    const channel = args[channelIdx + 1];
    const root = process.cwd();
    const packageVersion = readPackageVersion(root);
    const gitTags = readGitTags(root);
    const registryVersions = await readRegistryVersions();
    const resolved = resolveNextVersion({ channel, packageVersion, registryVersions, gitTags });
    validateTag(resolved.tag);
    const lines = `version=${resolved.version}\ntag=${resolved.tag}\ndist_tag=${resolved.distTag}\n`;
    process.stdout.write(lines);
    if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, lines);
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
