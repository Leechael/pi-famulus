# pi-famulus

A pi extension for subagent orchestration, auto-backgrounding bash, monitoring tasks, and agent-to-agent communication. Process management lives in a standalone Rust daemon, `pi-famulus` (machine-wide singleton, session-isolated, exits with the last pi).

## Architecture

```
pi extension (extension/, TypeScript)        pi-famulus (manager/, Rust)
├─ bash override: foreground budget →        ├─ spawn/wait/stop/output engine
│  auto-background                           ├─ session_id namespacing
├─ subagent: parallel tasks / serial chain   ├─ output duality (ring + full log)
├─ monitor: command output → event stream    └─ lifecycle: 0 connections, 5s →
├─ agent_message / contact_supervisor           kill tasks and exit
└─ NotifyCenter: single injection point
   for all async events
```

Design doc (wire protocol, state machines, interface contracts): [docs/design.md](docs/design.md).

## Install

After the first public npm release:

```bash
pi install npm:pi-famulus
```

npm installs the matching exact-version native manager automatically: Linux and macOS x64/arm64, and Windows x64. Windows ARM64 is not in the release set until CI produces it. No Rust compiler, postinstall download, or separate manager install is required. Linux builds are static musl binaries; macOS builds target macOS 13+; Windows x64 builds use MSVC with a statically linked CRT (Node/pi runtime requirements also apply). In-place upgrade is Unix-only. Keep optional dependencies enabled. The npm package also exposes `pi-famulus` on its npm bin path (`npx pi-famulus --help`).

## Documentation

- [Architecture and design](docs/design.md)
- [Tool reference](docs/tools.md): tool parameters, agent definitions, terminal UI
- [Configuration](docs/configuration.md): `config.json`, timeouts, manager binary discovery, degraded startup
- [Manager CLI](docs/cli.md)
- [Machine-wide agent capacity](docs/global-capacity.md)
- [Development](docs/development.md): source build, tests, name transition, package conflicts
- [Releasing](docs/releasing.md)
