# Development

## Source build and local trial

Use this to run a separately built manager, or to work on the project.

```bash
# 1. Build and install the manager at ~/.pi/agent/pi-famulus/bin/
cd manager && cargo build --release
mkdir -p ~/.pi/agent/pi-famulus/bin
# Atomic replace (new inode). In-place `cp` onto an existing binary breaks
# macOS code-signing and the next run dies with SIGKILL / "killed".
install -m 755 target/release/pi-famulus ~/.pi/agent/pi-famulus/bin/pi-famulus
#    Subsequent same-name upgrades: the same `install` line is enough.
#    A running daemon upgrades itself in place within seconds (same pid,
#    running work kept); `pi-famulus upgrade` does it now and reports the result.

# 2. Load the extension with this manager (local trial recommended first)
PI_FAMULUS_MANAGER_PATH="$HOME/.pi/agent/pi-famulus/bin/pi-famulus" \
  pi -e /path/to/pi-famulus/extension
# For keeps: `pi install <source>`
```

An installed native npm manager takes precedence over home/bin and PATH. To test a separately built manager, set `PI_FAMULUS_MANAGER_PATH` as above, or set `managerPath` in `~/.pi/agent/pi-famulus/config.json` to its absolute path. An executable `managerPath` takes precedence over the environment override; update or clear it when using the latter. The full lookup order is in [configuration.md](configuration.md#manager-binary-discovery).

## Tests

```bash
cd manager && cargo test                        # Rust: unit + adversarial + protocol + observability
cd manager && cargo test --features test-clock  # same suites on a manual clock (fast)
cd extension && npx tsc --noEmit && npx vitest run
PI_FAMULUS_INTEG=1 npx vitest run tests/integration/real-manager.test.ts  # TS ↔ real daemon
```

Manual acceptance checklist: [docs/testing-guide.md](docs/testing-guide.md).

Manual acceptance checklist: [testing-guide.md](testing-guide.md). Model-facing prompt evals: [eval/README.md](../eval/README.md).

## Releasing

CI, five-package publishing, and the one-time npm Trusted Publisher setup: [releasing.md](releasing.md).

The npm page for `pi-famulus` shows `extension/README.md`. That file must be a byte-for-byte copy of the root `README.md`; `node --test scripts/*.test.mjs` fails when they differ. After editing the root README run `cp README.md extension/README.md`. Links in the README are absolute GitHub URLs because npm resolves relative links against `repository.directory` (`extension`), not the repository root.

## One-time name transition

The rename to `pi-famulus` is a breaking installation change, not a hot upgrade of a previous installation. Wait for work to finish or stop it, close the sessions using that installation, and wait for its daemon to exit. Reinstall via npm or the source-build path above, migrate **configuration only** to `~/.pi/agent/pi-famulus/config.json` (update explicit paths and environment overrides), then reopen sessions. Do not move the runtime state/history tree: records contain absolute output and transcript paths that a directory move does not rewrite. Keep previous history separately if needed. Later compatible upgrades under the same name and home use the in-place upgrade described in [design.md](design.md); when changing installation method or binary location, restart the sessions rather than assuming an in-place upgrade across different paths.

## Conflicts with other packages

The older `pi-subagents` package also registers a `subagent` tool. Either `pi remove pi-subagents`, or test with `pi -ne -e ./extension` (`-ne` also suppresses your other extensions).
