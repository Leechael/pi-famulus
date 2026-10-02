# Native npm release context

The packages are one release set: users install the root extension and receive the matching native manager without a compiler or separate transport.

## Language

**Root package**: `pi-famulus`, the pi extension and npm CLI that declares four exact-version optional dependencies.
_Avoid_: using "manager package" for the root.

**Native package**: One os/cpu-specific package containing the executable built for that platform.
_Avoid_: "download installer"; there is no install hook.

**Release set**: The root and native tarballs sharing one stable version and source commit.
_Avoid_: "release" when referring to only one platform's archive.

**Published version**: Immutable registry bytes for a package name/version, identified by tarball integrity.
_Avoid_: "replace" or "overwrite" for a version already published.

**Trust binding**: npm's package-side authorization of the actual GitHub owner/repository, workflow filename, and required `npm` environment. The environment's external deployment policy allows only the `main` branch.
_Avoid_: treating workflow permissions or a dry run as proof that this remote binding exists.

## Customer invariants

1. Only Linux/macOS x64/arm64 native packages are selected, at exactly the root version.
2. Installation and discovery never move runtime history or overwrite a shared-home manager.
3. A non-executable/directory/malformed native candidate never shadows a usable configured or manual executable.
4. Published version bytes are never overwritten or silently accepted with differing integrity.
5. The root publish call follows successful native publication/identical-byte verification for the release set.
6. CI permissions and dry runs never imply that remote npm trust was configured or exercised.

Release transitions, invalid-input rejection, rollback, and consumers are recorded in [the ADR](../docs/decisions/npm-native-packages.md); actual setup and release operations are in [the release guide](../docs/releasing.md).
