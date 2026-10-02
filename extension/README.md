# pi-famulus

Subagent orchestration, auto-backgrounding bash, monitoring, and agent-to-agent communication for pi 1.0.0 or newer.

Workers inherit user/global and trusted-project pi configuration and resources, including codemode and MCP, then receive famulus communication tools. An explicit agent `tools` list remains an allowlist; omitting it inherits the user’s default tools. Parent-only CLI resource overrides are not copied.

After the first public release:

```sh
pi install npm:pi-famulus
```

npm installs the matching native manager automatically through an exact-version optional dependency. No Rust compiler, postinstall download, or separate manager installation is needed. Keep optional dependencies enabled.

Supported native packages:

| System | CPU | Package |
|---|---|---|
| Linux | x64 | `pi-famulus-linux-x64` |
| Linux | arm64 | `pi-famulus-linux-arm64` |
| macOS | Intel x64 | `pi-famulus-darwin-x64` |
| macOS | Apple Silicon arm64 | `pi-famulus-darwin-arm64` |
| Windows | x64 | `pi-famulus-win32-x64` |
| Windows | arm64 | `pi-famulus-win32-arm64` |

Linux binaries are statically linked with musl. macOS binaries target macOS 13 or newer; Windows binaries use MSVC. Your Node/pi runtime's requirements also apply. In-place manager upgrade remains Unix-only.

The npm package also exposes the `pi-famulus` CLI (`npx pi-famulus --help`). The extension's binary search order is:

1. Executable `managerPath` from configuration.
2. Executable `PI_FAMULUS_MANAGER_PATH` override.
3. The matching-version installed native npm package.
4. Executable `~/.pi/agent/pi-famulus/bin/pi-famulus` (or the equivalent beneath `PI_FAMULUS_HOME`).
5. Executable `pi-famulus` on `PATH`.

On Windows the file name in steps 4 and 5 (and inside the native package) is `pi-famulus.exe`.

Configuration and runtime state remain in `~/.pi/agent/pi-famulus`. Installing/updating the npm package does not move that state or copy a binary into the shared home. The npm CLI uses the package's native executable; use the extension configuration above when intentionally running a separately built manager.

If optional dependencies were omitted, reinstall with them enabled. The CLI reports an actionable error; the extension can still discover an explicitly installed manager and otherwise enters degraded mode. An arbitrary different-version native npm package is never substituted.

See the [project README](https://github.com/Leechael/pi-famulus#readme) for tools, settings, one-time migration, and source-build instructions; see the [CLI manual](https://github.com/Leechael/pi-famulus/blob/main/docs/cli.md) for operations.
