import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const read = path => readFileSync(new URL(`../${path}`, import.meta.url), 'utf8');

// npm shows extension/README.md (the package root). It is a committed copy of
// the root README so GitHub and npm render the same text.
test('extension/README.md is a byte-for-byte copy of the root README.md', () => {
  assert.equal(read('extension/README.md'), read('README.md'), 'run: cp README.md extension/README.md');
});

// npm resolves relative links against repository.directory ("extension"), so a
// link that works on GitHub's root page would 404 on npmjs.com.
test('README links are absolute', () => {
  const text = read('README.md');
  const inline = [...text.matchAll(/\]\(([^)\s]+)\)/g)].map(m => m[1]);
  // Reference definitions: "[label]: destination" at the start of a line.
  const reference = [...text.matchAll(/^ {0,3}\[[^\]]+\]:\s*<?([^\s>]+)>?/gm)].map(m => m[1]);
  const links = [...inline, ...reference];
  assert.ok(links.length > 0);
  for (const link of links) assert.match(link, /^(https?:\/\/|#|mailto:)/, `relative link in README: ${link}`);
});
