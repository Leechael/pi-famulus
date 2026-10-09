# ADR: Native npm packages and trusted publishing

## Status

Accepted. The user selected platform-specific npm packages instead of postinstall GitHub Release downloads.

## Context

The TypeScript extension requires a Rust manager. Users should install one npm package without a compiler, independent binary downloads, or mutations of their shared runtime state. Existing explicit manager overrides and source installations must continue to work.

## Options

| Option | Advantages | Costs |
|---|---|---|
| GitHub Release assets + postinstall download | One npm package; small root archive | Another network/auth/redirect/integrity path; install hooks; GitHub availability required |
| Native optional npm packages | npm selects the platform and verifies registry integrity; no install hooks | Six packages, trusted-publisher bindings, and synchronized versions |

## Decision

Publish `pi-famulus` plus `pi-famulus-{linux,darwin}-{x64,arm64}` and `pi-famulus-win32-x64`. The root declares exact-version optional dependencies; native packages declare `os` and `cpu`. Linux binaries use native-architecture static musl builds; macOS builds target macOS 13+; Windows x64 statically links the MSVC CRT. Other systems/architectures have no automatic native package. Windows ARM64 stays outside the release set until CI produces its artifact.

```
release tag on main
  -> validate versions/repository -> full CI
  -> five native cargo builds -> five native npm tarballs
  -> root npm tarball -> per-host installed-package smoke tests
  -> artifact validation -> publish natives -> publish root

npm install pi-famulus
  -> npm os/cpu selection + integrity verification
  -> exact-version native package
  -> executable file -> extension discovery / npm CLI
```

The extension searches explicit config, environment override, installed native npm package, home/bin, then PATH. Every selected candidate must be a regular executable file; native metadata must match the root version. The npm CLI runs only its package's native executable. Neither installation nor discovery copies executables into the user's state directory.

## State and invariants

Entity: release set (see [domain vocabulary](../../npm/CONTEXT.md)).

| State | Trigger | Source | Next state | Invariant |
|---|---|---|---|---|
| Draft | Stable tag/version/repository validation passes | Maintainer + validate job | Validated source | All six versions and actual GitHub identity agree; tag is on main ancestry |
| Validated source | Build, pack, tests, installed-package checks pass | Five native builders + full CI | Verified artifacts | Correct targets; Linux static and Windows static CRT; six actual tarballs contain required files |
| Validated source | Any build/test/package check fails | CI | Blocked | No publication is attempted |
| Verified artifacts | Dry-run publish requested | Manual workflow | Verified artifacts | No registry mutation or claim of OIDC authentication |
| Verified artifacts | Registry preflight fails | Publish job | Blocked | No mutation occurs before all candidate preflights pass |
| Verified artifacts | Real publication begins | Trusted publish job | Partial publication | Native calls/identical-byte verification precede root publication |
| Partial publication | Native or root publication fails | Registry / npm authorization | Blocked | Existing version bytes remain immutable; failure is visible |
| Partial publication | All native calls and final root call succeed | Publish job | Published | One complete release set is available |
| Blocked | Cause fixed; same candidate set revalidated | Maintainer retry | Verified artifacts | Already published bytes must have identical integrity |
| Blocked or Published | Different bytes proposed for an existing version | Maintainer / release validator | [rejected] | Registry integrity mismatch cannot be bypassed; create a new version |

Published is terminal for this release set. Installing or rolling back a client does not change its published bytes. A new version starts a separate Draft release set.

Consumers: npm uses package metadata; the extension and CLI share a native resolver; source tests use local development lock records; CI prepares/installs real tarballs; publishing validates those same artifacts. Historical eval harnesses and installed runtime records do not migrate through this flow.

Missing/invalid/omitted native dependencies permit explicit/manual discovery for the extension; otherwise it warns and degrades. The npm CLI exits with an actionable error. Unexpected infrastructure errors are not silently swallowed. Repeating package preparation uses a new inode, protecting macOS code-signing semantics.

## Consequences

The registry is the only binary transport. Version synchronization and six independent npm trust bindings are required. npm optional dependencies can be intentionally omitted; this is visible, not treated as a successful manager installation.

Publication uses GitHub OIDC, public provenance, and no npm token secret. The workflow is explicitly dispatched from main; npm trust must bind to the `npm` GitHub environment, whose external deployment policy permits only that branch. Tag ancestry validates build input, not workflow authority. Release serialization retains up to 100 pending runs with `queue: max`; arrivals exceeding that bound must be redispatched. Repository URLs must match the actual repository even while its external name still differs from the renamed product. npm's first authenticated publication and package-side trust settings cannot be bootstrapped by unauthenticated OIDC; they remain explicit setup steps in [the release guide](../releasing.md).

No automatic migration of old state is in scope. Windows is supported for the manager binary and named-pipe IPC (in-place upgrade remains Unix-only). Roll back by installing a previously published complete root version with its exact native dependencies; never rewrite an existing version's bytes. A retry may skip a version only when the candidate tarball integrity equals the registry's existing integrity; if rebuilding differs, release a new synchronized version instead.
