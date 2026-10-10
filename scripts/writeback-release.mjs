import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { bumpRelease } from './bump-release.mjs';
import { validateTag } from './validate-release.mjs';

const STABLE = /^(\d+)\.(\d+)\.(\d+)$/;

// Decide whether a published version should be written back to the source
// tree. Only stable releases are recorded on main, and only when they are
// strictly newer than what main already carries: a re-run is a no-op and a
// late job never moves main backwards.
export function writebackDecision(current, release) {
  const next = STABLE.exec(release);
  if (!next) return { write: false, reason: `${release} is a prerelease; main only records stable versions` };
  const cur = STABLE.exec(current);
  if (!cur) return { write: true, reason: `main carries non-stable ${current}` };
  for (let i = 1; i <= 3; i++) {
    const delta = Number(next[i]) - Number(cur[i]);
    if (delta > 0) return { write: true, reason: `${current} -> ${release}` };
    if (delta < 0) return { write: false, reason: `main already has newer ${current}` };
  }
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
