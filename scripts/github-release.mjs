import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { validateTag } from './validate-release.mjs';

const API_VERSION = '2022-11-28';
const ATTEMPTS = 5;
const RETRY_MS = 2000;

export function classifyCommitLookup(status, body) {
  const code = Number(status);
  if (code === 200) return 'exists';
  if (code === 404) return 'missing';
  const message = typeof body?.message === 'string' ? body.message : '';
  // GitHub's commits API returns 422 (not 404) for an unknown tag/ref.
  // Only this exact missing-ref message is "not found"; other 422s fail closed.
  if (code === 422 && message.startsWith('No commit found for SHA:')) return 'missing';
  return 'error';
}

export function classifyReleaseLookup(status) {
  const code = Number(status);
  if (code === 200) return 'exists';
  if (code === 404) return 'missing';
  return 'error';
}

function apiUrl(base) {
  return (base || 'https://api.github.com').replace(/\/$/, '');
}

async function githubGet(fetchImpl, { base, token, endpoint }) {
  const response = await fetchImpl(`${base}/${endpoint}`, {
    headers: {
      Accept: 'application/vnd.github+json',
      'X-GitHub-Api-Version': API_VERSION,
      Authorization: `Bearer ${token}`,
    },
  });
  let body = null;
  try { body = await response.json(); } catch { body = null; }
  return { status: response.status, body };
}

function lookupError(endpoint, status, body) {
  const detail = body ? `\n${JSON.stringify(body)}` : '';
  return new Error(`GitHub API GET ${endpoint} failed with HTTP ${status}:${detail}`);
}

export async function ensureGitHubTagAndRelease({
  tag,
  sha,
  repository,
  token,
  apiUrl: rawApiUrl = 'https://api.github.com',
  fetchImpl = fetch,
  run = execFileSync,
  sleep = ms => new Promise(resolve => setTimeout(resolve, ms)),
  attempts = ATTEMPTS,
} = {}) {
  validateTag(tag);
  assert.match(sha ?? '', /^[0-9a-f]{40}$/i, 'release sha must be a 40-character commit');
  assert.match(repository ?? '', /^[^/]+\/[^/]+$/, 'repository must be owner/name');
  assert.ok(token, 'GH_TOKEN is required');
  const base = apiUrl(rawApiUrl);
  const commitEndpoint = `repos/${repository}/commits/${encodeURIComponent(tag)}`;
  const releaseEndpoint = `repos/${repository}/releases/tags/${encodeURIComponent(tag)}`;

  let tagReady = false;
  for (let attempt = 1; attempt <= attempts; attempt++) {
    const { status, body } = await githubGet(fetchImpl, { base, token, endpoint: commitEndpoint });
    const kind = classifyCommitLookup(status, body);
    if (kind === 'exists') {
      const tagSha = typeof body?.sha === 'string' && body.sha ? body.sha : '';
      if (!tagSha) throw new Error(`GitHub commit lookup for ${tag} returned no sha`);
      if (tagSha !== sha) throw new Error(`Tag ${tag} already exists at ${tagSha}, expected ${sha}`);
      console.log(`Tag ${tag} already points to ${sha}`);
      tagReady = true;
      break;
    }
    if (kind === 'error') throw lookupError(commitEndpoint, status, body);
    if (attempt === attempts) throw new Error(`Could not create or verify tag ${tag} after retries`);
    try {
      run('gh', ['api', '--method', 'POST', `repos/${repository}/git/refs`, '-f', `ref=refs/tags/${tag}`, '-f', `sha=${sha}`], { stdio: 'inherit' });
    } catch {
      console.error(`Tag creation attempt ${attempt} failed; checking again before retrying`);
    }
    await sleep(RETRY_MS);
  }
  if (!tagReady) throw new Error(`Could not create or verify tag ${tag} after retries`);

  const releaseArgs = ['release', 'create', tag, '--verify-tag', '--generate-notes', '--title', tag];
  if (tag.includes('-beta.') || tag.includes('-nightly.')) releaseArgs.push('--prerelease');

  let releaseReady = false;
  for (let attempt = 1; attempt <= attempts; attempt++) {
    const { status, body } = await githubGet(fetchImpl, { base, token, endpoint: releaseEndpoint });
    const kind = classifyReleaseLookup(status, body);
    if (kind === 'exists') {
      console.log(`GitHub Release ${tag} already exists`);
      releaseReady = true;
      break;
    }
    if (kind === 'error') throw lookupError(releaseEndpoint, status, body);
    if (attempt === attempts) throw new Error(`Could not create or verify GitHub Release ${tag} after retries`);
    try {
      run('gh', releaseArgs, { stdio: 'inherit' });
    } catch {
      console.error(`Release creation attempt ${attempt} failed; checking again before retrying`);
    }
    await sleep(RETRY_MS);
  }
  if (!releaseReady) throw new Error(`Could not create or verify GitHub Release ${tag} after retries`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    await ensureGitHubTagAndRelease({
      tag: process.env.RELEASE_TAG,
      sha: process.env.RELEASE_SHA,
      repository: process.env.GITHUB_REPOSITORY,
      token: process.env.GH_TOKEN,
      apiUrl: process.env.GITHUB_API_URL,
    });
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
