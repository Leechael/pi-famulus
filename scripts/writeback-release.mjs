import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { bumpRelease } from './bump-release.mjs';
import { validateTag } from './validate-release.mjs';

const STABLE = /^\d+\.\d+\.\d+$/;
const SEMVER = /^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z.-]+)?$/;
const NUMERIC = /^\d+$/;

// SemVer 2.0.0 section 11 precedence: -1, 0 or 1. Build metadata is ignored;
// a prerelease sorts before its release; prerelease identifiers compare
// left to right, numeric ones numerically and below alphanumeric ones, and a
// shorter list sorts first when all shared identifiers are equal.
export function compareSemver(a, b) {
  const [pa, pb] = [a, b].map(v => {
    const m = SEMVER.exec(v);
    if (!m) throw new Error(`not a SemVer version: ${v}`);
    return { core: m.slice(1, 4).map(Number), pre: m[4]?.split('.') ?? [] };
  });
  for (let i = 0; i < 3; i++) if (pa.core[i] !== pb.core[i]) return pa.core[i] < pb.core[i] ? -1 : 1;
  if (!pa.pre.length || !pb.pre.length) return Math.sign(pb.pre.length - pa.pre.length);
  for (let i = 0; i < Math.min(pa.pre.length, pb.pre.length); i++) {
    const [x, y] = [pa.pre[i], pb.pre[i]];
    if (x === y) continue;
    const [nx, ny] = [NUMERIC.test(x), NUMERIC.test(y)];
    if (nx && ny) return Number(x) < Number(y) ? -1 : 1;
    if (nx !== ny) return nx ? -1 : 1;
    return x < y ? -1 : 1;
  }
  return Math.sign(pa.pre.length - pb.pre.length);
}

// Decide whether a published version should be written back to the source
// tree. Only stable releases are recorded on main, and only when they have
// higher SemVer precedence than what main already carries: a re-run is a
// no-op and a late job never moves main backwards (a prerelease on main,
// such as 0.2.0-beta.1, still outranks an older stable like 0.1.9).
export function writebackDecision(current, release) {
  if (!STABLE.test(release)) return { write: false, reason: `${release} is a prerelease; main only records stable versions` };
  const order = compareSemver(release, current);
  if (order > 0) return { write: true, reason: `${current} -> ${release}` };
  if (order < 0) return { write: false, reason: `main already has newer ${current}` };
  return { write: false, reason: `main already has ${release}` };
}

export function writebackRelease(root, release) {
  validateTag(`v${release}`);
  const current = JSON.parse(readFileSync(join(root, 'extension/package.json'), 'utf8')).version;
  const decision = writebackDecision(current, release);
  if (decision.write) bumpRelease(root, release);
  return decision;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    assert.ok(process.argv.length === 3, 'usage: writeback-release.mjs <version>');
    const decision = writebackRelease(process.cwd(), process.argv[2]);
    console.log(`${decision.write ? 'write' : 'skip'}: ${decision.reason}`);
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
