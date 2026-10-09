import test from 'node:test';
import assert from 'node:assert/strict';
import { classifyCommitLookup, classifyReleaseLookup, ensureGitHubTagAndRelease } from './github-release.mjs';

// ENTITY: GitHub release tag (publish job, after npm)
// STATE            | TRIGGER                      | SOURCE     | NEXT            | INVARIANT
// absent           | commits 422 No commit found  | lookup     | creating        | do not abort; create refs/tags
// absent           | commits 404                  | lookup     | creating        | same as 422-missing
// absent           | commits 422 other / 401/5xx  | lookup     | [rejected]      | fail closed; not "missing"
// present@sha      | commits 200 matching sha     | lookup     | present@sha     | idempotent, no retarget
// present@other    | commits 200 different sha    | lookup     | [rejected]      | never move the tag
//
// Incident: https://github.com/Leechael/pi-famulus/actions/runs/37938377337
// GET /commits/v0.1.4 returned 422, not 404, after npm 0.1.4 had published.
const INCIDENT_422 = {
  message: 'No commit found for SHA: v0.1.4',
  documentation_url: 'https://docs.github.com/rest/commits/commits#get-a-commit',
  status: '422',
};
const SHA = '364c36302f6e26f700964dcf905f08be874d47a7';
const OTHER_SHA = 'a79fd8df7a6073e8d5117433f8f2df28996c3793';
const TAG = 'v0.1.4';
const REPO = 'Leechael/pi-famulus';

function response(status, body) {
  return {
    status,
    ok: status >= 200 && status < 300,
    json: async () => body,
  };
}

test('commits 422 No commit found for SHA is a missing tag, not a hard error', () => {
  assert.equal(classifyCommitLookup(422, INCIDENT_422), 'missing');
  assert.equal(classifyCommitLookup('422', INCIDENT_422), 'missing');
  assert.equal(classifyCommitLookup(404, { message: 'Not Found', status: '404' }), 'missing');
  assert.equal(classifyCommitLookup(200, { sha: SHA }), 'exists');
});

test('other commit lookup failures stay errors so 401/5xx/validation never create a tag', () => {
  assert.equal(classifyCommitLookup(422, { message: 'Validation Failed', status: '422' }), 'error');
  assert.equal(classifyCommitLookup(422, { message: 'No commit found for SHA' }), 'error');
  assert.equal(classifyCommitLookup(422, {}), 'error');
  assert.equal(classifyCommitLookup(401, { message: 'Bad credentials' }), 'error');
  assert.equal(classifyCommitLookup(403, { message: 'Resource not accessible by integration' }), 'error');
  assert.equal(classifyCommitLookup(500, { message: 'Internal Server Error' }), 'error');
});

test('release lookup treats only HTTP 404 as missing', () => {
  assert.equal(classifyReleaseLookup(200, { tag_name: TAG }), 'exists');
  assert.equal(classifyReleaseLookup(404, { message: 'Not Found', status: '404' }), 'missing');
  assert.equal(classifyReleaseLookup(422, INCIDENT_422), 'error');
  assert.equal(classifyReleaseLookup(500, { message: 'Internal Server Error' }), 'error');
});

function mockGithub({ commits, releases }) {
  const commitQueue = [...commits];
  const releaseQueue = [...releases];
  const lookups = [];
  const fetchImpl = async (url, init) => {
    lookups.push({ url: String(url), authorization: init?.headers?.Authorization });
    const u = String(url);
    if (u.includes('/commits/')) {
      const next = commitQueue.shift();
      assert.ok(next, `unexpected commits lookup: ${u}`);
      return response(next.status, next.body);
    }
    if (u.includes('/releases/tags/')) {
      const next = releaseQueue.shift();
      assert.ok(next, `unexpected release lookup: ${u}`);
      return response(next.status, next.body);
    }
    throw new Error(`unexpected url ${u}`);
  };
  return { fetchImpl, lookups, commitQueue, releaseQueue };
}

async function runEnsure(github, extra = {}) {
  const commands = [];
  const sleeps = [];
  await ensureGitHubTagAndRelease({
    tag: TAG,
    sha: SHA,
    repository: REPO,
    token: 'test-token',
    apiUrl: 'https://api.github.com',
    fetchImpl: github.fetchImpl,
    run: (command, args) => {
      commands.push([command, args]);
      return '';
    },
    sleep: async ms => {
      sleeps.push(ms);
    },
    ...extra,
  });
  return { commands, sleeps, lookups: github.lookups };
}

test('incident 422 after npm publish creates the GitHub tag then the release', async () => {
  const github = mockGithub({
    commits: [
      { status: 422, body: INCIDENT_422 },
      { status: 200, body: { sha: SHA } },
    ],
    releases: [
      { status: 404, body: { message: 'Not Found', status: '404' } },
      { status: 200, body: { id: 1, tag_name: TAG } },
    ],
  });
  const { commands, sleeps, lookups } = await runEnsure(github);
  assert.equal(lookups[0].url, `https://api.github.com/repos/${REPO}/commits/${TAG}`);
  assert.equal(lookups[0].authorization, 'Bearer test-token');
  assert.deepEqual(commands[0], ['gh', ['api', '--method', 'POST', `repos/${REPO}/git/refs`, '-f', `ref=refs/tags/${TAG}`, '-f', `sha=${SHA}`]]);
  assert.deepEqual(commands[1], ['gh', ['release', 'create', TAG, '--verify-tag', '--generate-notes', '--title', TAG]]);
  assert.equal(commands.length, 2);
  assert.deepEqual(sleeps, [2000, 2000]);
});

test('existing tag at the expected peeled commit sha is idempotent', async () => {
  const github = mockGithub({
    commits: [{ status: 200, body: { sha: SHA } }],
    releases: [{ status: 200, body: { id: 1, tag_name: TAG } }],
  });
  const { commands, sleeps } = await runEnsure(github);
  assert.equal(commands.length, 0);
  assert.equal(sleeps.length, 0);
});

test('existing tag at a different sha fails closed and never moves the tag', async () => {
  const github = mockGithub({
    commits: [{ status: 200, body: { sha: OTHER_SHA } }],
    releases: [],
  });
  await assert.rejects(runEnsure(github), /already exists at .* expected/);
});

test('generic 422 and 5xx commit lookups fail closed without creating a tag', async () => {
  for (const commits of [
    [{ status: 422, body: { message: 'Validation Failed', status: '422' } }],
    [{ status: 500, body: { message: 'Internal Server Error' } }],
    [{ status: 401, body: { message: 'Bad credentials' } }],
  ]) {
    const github = mockGithub({ commits, releases: [] });
    const commands = [];
    await assert.rejects(ensureGitHubTagAndRelease({
      tag: TAG, sha: SHA, repository: REPO, token: 'test-token',
      fetchImpl: github.fetchImpl,
      run: (...args) => {
        commands.push(args);
        return '';
      },
      sleep: async () => {},
    }), /failed with HTTP|Unexpected tag lookup/);
    assert.equal(commands.length, 0);
  }
});

test('POST that loses a create race still succeeds if the next lookup matches', async () => {
  const github = mockGithub({
    commits: [
      { status: 404, body: { message: 'Not Found', status: '404' } },
      { status: 200, body: { sha: SHA } },
    ],
    releases: [{ status: 200, body: { id: 1, tag_name: TAG } }],
  });
  const commands = [];
  await ensureGitHubTagAndRelease({
    tag: TAG, sha: SHA, repository: REPO, token: 'test-token',
    fetchImpl: github.fetchImpl,
    run: (command, args) => {
      commands.push([command, args]);
      throw new Error('Reference already exists');
    },
    sleep: async () => {},
  });
  assert.equal(commands.length, 1);
  assert.equal(commands[0][1][0], 'api');
});

test('still-missing tag after retries fails without pretending the tag exists', async () => {
  const github = mockGithub({
    commits: Array.from({ length: 5 }, () => ({ status: 422, body: INCIDENT_422 })),
    releases: [],
  });
  const commands = [];
  await assert.rejects(runEnsure(github, {
    run: (command, args) => {
      commands.push([command, args]);
      return '';
    },
  }), /Could not create or verify tag/);
  assert.equal(commands.length, 4);
});

test('beta and nightly GitHub Releases are prereleases; stables are not', async () => {
  const cases = [
    ['v0.1.5-beta.0', true],
    ['v0.1.5-nightly.20261009', true],
    ['v0.1.4', false],
  ];
  for (const [tag, prerelease] of cases) {
    const github = mockGithub({
      commits: [{ status: 200, body: { sha: SHA } }],
      releases: [
        { status: 404, body: { message: 'Not Found', status: '404' } },
        { status: 200, body: { id: 1, tag_name: tag } },
      ],
    });
    const commands = [];
    await ensureGitHubTagAndRelease({
      tag, sha: SHA, repository: REPO, token: 'test-token',
      fetchImpl: github.fetchImpl,
      run: (command, args) => {
        commands.push(args);
        return '';
      },
      sleep: async () => {},
    });
    assert.equal(commands[0].includes('--prerelease'), prerelease, tag);
  }
});
