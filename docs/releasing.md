# npm releases

The main extension and native manager packages form one versioned release. The packaging design is recorded in [the ADR](decisions/npm-native-packages.md).

## Supported outputs

| Package | Rust target | GitHub-hosted builder |
|---|---|---|
| `pi-famulus-linux-x64` | `x86_64-unknown-linux-musl` | `ubuntu-24.04` |
| `pi-famulus-linux-arm64` | `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` |
| `pi-famulus-darwin-x64` | `x86_64-apple-darwin` | `macos-15-intel` |
| `pi-famulus-darwin-arm64` | `aarch64-apple-darwin` | `macos-15` |
| `pi-famulus-win-x64` | `x86_64-pc-windows-msvc` | `windows-latest` |
| `pi-famulus` | TypeScript extension + JS CLI/resolver | `ubuntu-24.04` |

Linux artifacts are statically linked with musl. macOS builds explicitly target macOS 13+. Windows x64 builds use the MSVC toolchain with the CRT linked statically (`manager/.cargo/config.toml`, `target.x86_64-pc-windows-msvc` `+crt-static`). The package job sets `RUSTFLAGS` only on Linux, so that cargo config is what the Windows release binary is linked with. Before packing, CI inspects the actual PE dependencies with `dumpbin` and rejects VC runtime DLL imports; the hosted runner's installed redistributables must not hide a dependency. Node/pi's runtime requirements still apply. In-place upgrade (`exec` handover) remains Unix-only; on Windows, replace the binary and restart the manager.

CI builds the Unix targets on native-architecture hosts and `win32-x64` on `windows-latest`, runs the ordinary and (Unix) manual-clock Rust suites, and verifies actual root/native npm tarball installation on Unix hosts and Windows x64. Windows ARM64 is not part of the required release set until a CI producer exists. Installation smoke tests explicitly select the pi version pinned in the extension lockfile rather than an unbounded latest peer. Installed-package tests use an isolated HOME with no manager override and check both the CLI and the extension's real TypeScript resolver. CI also runs extension typecheck/full tests, real-manager integration, and the free faux-model eval/unit/grader suites. These do not replace the [required real-model baseline gate](../eval/BASELINES.md).

## Exact release metadata

Keep these at the same stable `X.Y.Z`:

- `extension/package.json` version and its exact optional dependency versions.
- All `npm/*/package.json` versions.
- `manager/Cargo.toml` version and the own-package Cargo lock record.
- The extension lockfile's root metadata and first-party native-package version records.

Third-party dependency records need not change for a version bump. For source development, the extension lockfile resolves native packages to their local metadata directories; CI uses full `npm ci` and verifies built packages independently. Do not globally omit optional dependencies: TypeScript and Rollup also require their platform-specific optional bindings. The published root tarball does **not** contain this lockfile: consumers resolve the exact native versions from npm.

```sh
node scripts/validate-release.mjs
node --test scripts/*.test.mjs
```

`validate-release.mjs` checks versions, native names/targets, exports/files, no install hooks, and repository identity. `--tag vX.Y.Z` compares that metadata to a computed release tag; add `--require-git-tag` only when the git tag must already exist (publish creates the tag after npm). `prepare-native.mjs <os-arch> <artifact-directory>` requires the compiled release under `manager/target/<target>/release/pi-famulus`, verifies its version, atomically installs it into the ignored native package bin directory, and packs a real tarball. Generated binaries and `dist/` are not committed.

## Trusted Publishers: repository configuration

The workflow is `.github/workflows/publish.yml`. It uses GitHub-hosted runners, Node 24, an explicit npm upgrade/check for npm >=11.5.1, and **publish-job-only** `id-token: write` plus `contents: read`. It does not use npm token secrets, login, or `npm whoami` as an OIDC check. Tokens and `.npmrc` contents must never be printed.

All five package manifests specify the exact public repository URL:

```text
https://github.com/Leechael/pi-famulus
```

The GitHub repository has been renamed to `Leechael/pi-famulus`. Package metadata, the canonical release validator, and **all five npm trust bindings** must use this exact current identity. GitHub redirects from the previous repository name do not replace the matching OIDC identity.

The publish job requires the **`npm` GitHub environment**. In repository Settings → Environments → npm, use **Selected branches and tags**, with exactly one **Branch** rule named `main` and no tag rules. This external policy blocks a non-main workflow from acquiring the trusted environment even if someone edits its in-file guards. Protect `main` against unreviewed changes as part of repository access policy. Bind each package separately; do not leave npm's Environment field blank:

| npm setting | Value |
|---|---|
| Provider | GitHub Actions |
| Owner / organization | `Leechael` |
| Repository | `pi-famulus` |
| Workflow filename | `publish.yml` (not the path or display name) |
| Environment | `npm` (required) |
| Publish permission | Allow direct `npm publish` |

## Required one-time npm setup

Repository configuration does not create npm packages or their trusted-publisher bindings. All packages in the release set must first exist in npm and be controlled by the intended maintainer. This repository cannot assert that those remote settings are configured just because the workflow passes a dry run.

1. Confirm the public license/ownership and all release-set npm names before the first public release.
2. Merge the release code only after required review/testing, including the real-model baseline gate or an explicit maintainer waiver.
3. Verify the `npm` environment's main-only deployment policy. Run `publish.yml` via **workflow_dispatch** from **main**, selecting a channel (`patch` / `minor` / `major` / `beta` / `nightly`) and **checking** `dry_run` (the UI default is now unchecked and would publish for real). The workflow computes the next version from npm, git tags, and `package.json`. The resulting artifacts are `npm-root` and `npm-<os>-<arch>`.
4. Download all `.tgz` files from that exact run. Authenticate interactively with `npm login` in a maintainer-controlled terminal, then bootstrap the native tarballs **first** and the root tarball **last**, with `npm publish <file.tgz> --access public`. Do not publish placeholders, source-only native packages, or different bytes under the same version. Local interactive bootstrap does not automatically produce GitHub OIDC provenance.
5. Configure the table above under each package's npm Access / Trusted publishing settings. With current npm, the equivalent authenticated commands are:

```sh
for package in \
  pi-famulus-linux-x64 pi-famulus-linux-arm64 \
  pi-famulus-darwin-x64 pi-famulus-darwin-arm64 \
  pi-famulus-win-x64 \
  pi-famulus
do
  npm trust github "$package" --file publish.yml \
    --repository Leechael/pi-famulus --environment npm --allow-publish --yes \
    --registry https://registry.npmjs.org
  npm trust list "$package" --registry https://registry.npmjs.org
done
```

Use npm's website if your npm version lacks `npm trust`. Refresh and verify the saved bindings; do not merely assume form submission proved authentication. After verifying OIDC on a subsequent version, restrict token publishing as appropriate for the maintainer's npm account policy.

The repository's `npm` GitHub environment was created and its single `main` branch-only deployment rule verified through GitHub's API. This is not npm-side authorization. At implementation time local `npm whoami` returned 401 and the root package lookup returned 404. No public package was created or published, and no npm trust binding was changed. Those observations are setup status only: `whoami` returning 401 inside an OIDC-only job is expected and must not block publishing.

## Subsequent tokenless releases

`publish.yml` does not take a version or tag. Dispatch from **main** and choose a channel. The workflow UI defaults to `patch` with `dry_run` unchecked (a real publish); first-time setup and artifact review still require an explicit dry run.

| Channel | Version | npm dist-tag |
|---|---|---|
| `patch` / `minor` / `major` | next stable `X.Y.Z` | `latest` |
| `beta` | `{next-patch}-beta.N` | `beta` |
| `nightly` | `{next-patch}-nightly.YYYYMMDD` (`.N` if that day already exists) | `nightly` |

The next version is the bump of `max(package.json, npm published stables, git tags vX.Y.Z)`. Prerelease counters come from existing npm versions and git tags. You never type the number.

CI rewrites release metadata in the job workspace (package manifests, Cargo, `EXTENSION_VERSION`, lockfiles) with `scripts/bump-release.mjs` so packed tarballs carry that version. After a real stable publish, the `record-version` job commits that same rewrite to `main` (see below), so source shows the last released version. npm remains the source of truth for the next increment.

Publication is explicitly dispatched from **main**; publishing a GitHub release does not itself trigger npm publication. A tag-triggered workflow runs under a tag ref, not main, and is deliberately incompatible with the main-only environment gate.

```sh
gh workflow run publish.yml --ref main -f channel=patch -f dry_run=true
# After reviewing the artifacts and trust bindings:
gh workflow run publish.yml --ref main -f channel=patch -f dry_run=false
```

`dry_run=true` builds/tests and invokes real `npm publish --dry-run`; `dry_run=false` explicitly requests publication. Repeating `patch` after a successful publish computes the next patch; repeating it after only a dry run computes the same unpublished version again.

A dry run proves package validity, **not** OIDC authentication or npm-side trust. For real publication, safe diagnostics assert OIDC request credentials are present without logging them. Successful OIDC/provenance must be verified from the publishing logs and npm metadata. Re-running an identical bootstrap version may skip every publish call; that is **not** an OIDC authentication test. Verify on a subsequent new version.

Native packages publish before the root. The script preflights all registry names/versions before its first mutation. Existing versions are skipped only if their SHA-512 tarball integrity equals the candidate; a mismatch or registry error stops publication. Releases are serialized with GitHub's `queue: max`: one active run and up to 100 pending runs. Active publication is not auto-cancelled. GitHub cancels additional arrivals beyond that queue limit; operators must inspect and explicitly redispatch those requests. npm versions cannot be overwritten. If a partial retry rebuilds different artifacts, fail closed and publish a new synchronized version rather than bypassing the integrity check.

After a real npm publish, the same job creates the GitHub tag at the validated commit and a generated GitHub Release (`scripts/github-release.mjs`). Tag existence is resolved through the commits API so annotated tags peel to their commit SHA (`git/ref/tags` returns the tag-object SHA). A missing tag is HTTP 404 **or** the commits-API 422 `No commit found for SHA:`; other 4xx/5xx responses fail closed and never retarget an existing tag. Retries reuse this run's tag and SHA. A later dispatch computes a new version from npm, so a failed GitHub tag step must be recovered by re-running that same job (after this helper is on the job's SHA) or by creating that exact tag/release manually — not by dispatching a new channel bump.

### Version write-back to `main`

The `record-version` job runs only when `publish` succeeded (so npm, the tag, and the GitHub Release exist) and `dry_run` was false. It checks out the **latest** `main` (not the tagged commit), runs `scripts/writeback-release.mjs <version>`, validates the result with `validate-release.mjs --tag`, and pushes `chore(release): vX.Y.Z` as `github-actions[bot]` using the job's `GITHUB_TOKEN` (`contents: write`; no npm credentials).

- Stable versions only. `beta` and `nightly` releases never change `main`.
- Idempotent and monotonic: if `main` already has that version or a newer one, the script prints `skip` and nothing is committed.
- Main moved since the tag: the job edits only version fields, so nothing is rebased. If the push is rejected it fetches, resets to the new `main`, redoes the edit, and pushes once more; a second rejection fails the job. Re-running the job is safe.
- `main` must allow `github-actions[bot]` to push directly. If branch protection or a ruleset is added later, this job will fail after a successful publish; the release itself is unaffected. Either exempt the bot or switch to a PR-based flow.
- Pushes made with `GITHUB_TOKEN` do not trigger workflows, so `ci.yml` does not run on the release commit. It changes only version fields that the release run just built and tested at that version.
- Keep the versioned-file list in `bump-release.mjs` and the carrier list in `scripts/release.test.mjs` in sync; the test fails if a file is left on the old version.

Verify a completed release with `npm view <package>@X.Y.Z version dist.integrity`, the provenance link, and clean installs on the four supported platforms. Roll back by installing a previously complete root version; its exact optional dependencies select the corresponding native build.

## References

- [npm Trusted Publishers: The Complete Guide](https://leechael.org/posts/2025/npm-trusted-publishers-the-complete-guide/)
- [Official npm trusted publishing documentation](https://docs.npmjs.com/trusted-publishers/)
- [Official npm trust commands](https://docs.npmjs.com/cli/v11/commands/npm-trust/)
- [GitHub environment deployment restrictions](https://docs.github.com/en/actions/how-tos/deploy/configure-and-manage-deployments/manage-environments)
- [GitHub concurrency queues](https://github.blog/changelog/2026-05-07-github-actions-concurrency-groups-now-allow-larger-queues/)
