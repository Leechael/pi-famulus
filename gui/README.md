# pi-famulus GUI (spike)

`pi-famulus top` in a native desktop window, built on [NSH](https://nsh.dev/docs/) 0.4.0. It
has the same screens, page flow and keys as the TUI. It is a spike, not a shipped component:
it is not part of the manager's workspace, CI, or npm packages.

```sh
cd gui
cargo run                      # AppKit on macOS, GTK 4 on Linux, Win32 on Windows
nsh package --target macos     # -> .nsh/out/pi-famulus.app (ad hoc signed)
```

`nsh package` needs the nsh CLI at the same version as the crates (0.4.0). The crate follows
nsh's Rust template: `src/lib.rs` holds `#[native_module]`, and `nsh.json` names the root (`App`).
`rust-toolchain.toml` pins Rust 1.99.1, because nsh-core 0.4.0 uses `isolate_lowest_one`,
which Rust 1.95 does not have as a stable API. The manager still builds with plain `stable`.

## Same behavior as the TUI, by construction

The TUI and the GUI drive one state machine, so they cannot drift apart:

- **One controller.** `manager/src/top/controller.rs` holds the session: state, collector,
  live agent link, copying, stopping. The TUI's `run` and this crate's `src/engine.rs` both
  drive a `Controller`. The GUI's engine runs it on its own thread and publishes a `Frame`
  per change.
- **Same keys.** `src/keys.rs` binds every key `top` handles and hands the controller the
  same `crossterm` key event the terminal would. Filter, output search and the agent input
  are native text fields; their text reaches the controller as `Action::Text`, which leaves
  the state as typing would.
- **Same pointer actions.** Clicks on rows, tabs, scope, sort, the column header, stream tabs,
  fold chevrons and the scrollbar send `top::Action`s. Each runs the code the TUI's mouse
  handling runs for that target. The wheel moves the selection, as in the terminal.
- **Same text.** Cells, the overview, work kinds, preview, help, the task info card, output
  search, and the agent thread come from the TUI's view functions in `top::ui`, at the size
  the window measures in character cells. Those functions also do the scroll, follow and
  match bookkeeping, so both frontends scroll and search the same way.
- **Same paging.** Like the terminal, the window renders only the rows in view.

## Verified (macOS, live daemon, ~4,100 retained tasks, 248 agents)

- Tree, agents and tasks tabs show the TUI's columns and values. Scope cycles through
  active, connected and all; tab counts and the `n/total` position update.
- A task detail shows the pinned info card, the `output` / `stderr` tabs, the line count and
  the footer keys. An agent detail shows its usage line and the live input with its facts.
- Clicks reach the controller (logged end to end): tabs, scope, rows, second click to open.
- The release build uses about 210 MB (the earlier spike, with a native row per task, used
  837 MB). CPU is 30–48% at the default 1 s refresh. The release TUI under the same load
  uses 22–42% CPU, so the cost is the shared collector, not the window.

## Not verified yet

- **Not every key.** The main keys were tried by hand in the window and behave as in the
  TUI. Not every key was covered. Stopping a task (`x`, then `y`) and sending to or aborting
  an agent were deliberately left untried, so no real process or agent was touched. Both
  run the TUI's code path, and stop asks for confirmation first.
- Linux and Windows.

## Known

- An agent whose live session has no entries shows `(no conversation yet)` once the live
  link connects, even though its transcript on disk has turns. This is the shared
  `apply_live` code, so the TUI does the same.
- `cargo build` pulls a newer tokio than `manager/Cargo.lock` pins, and that version
  deprecates `AsyncFd::new` (`manager/src/intercom/native/alias.rs`). The warning shows up
  only in the GUI build.
- `nsh package --target macos` builds a 46 MB ad hoc signed `.app` with the repo logo as its
  icon.
