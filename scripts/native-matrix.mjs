#!/usr/bin/env node
// Matrix rows for the manager suites and native packages, from the single
// list in .github/native-platforms.json.
//
//   node scripts/native-matrix.mjs <all-architectures: true|false>
//
// Prints `matrix=<json>` for $GITHUB_OUTPUT. With `false` (pull requests)
// only one architecture per OS runs: the platform code differs by OS
// (process groups, proc_pidinfo, job objects), not by CPU. Pushes to main,
// dispatches and releases pass `true` and get all six.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

export const platforms = () =>
  JSON.parse(readFileSync(new URL('../.github/native-platforms.json', import.meta.url), 'utf8'));

export function matrix(allArchitectures, rows = platforms()) {
  if (allArchitectures !== 'true' && allArchitectures !== 'false') {
    throw new Error(`all-architectures must be 'true' or 'false', got ${JSON.stringify(allArchitectures)}`);
  }
  const include = rows
    .filter(row => allArchitectures === 'true' || row.primary)
    .map(({ platform, runner, target }) => ({ platform, runner, target }));
  if (include.length === 0) throw new Error('no platforms selected');
  return { include };
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  console.log(`matrix=${JSON.stringify(matrix(process.argv[2]))}`);
}
