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

## The daemon

Like `pi-famulus top`, the window attaches to the daemon of its home (`PI_FAMULUS_HOME`, or
the default) and starts one when none runs. It goes through the same client code as `top`, so
the daemon's lock and socket still guarantee one daemon per home.

The GUI is not the daemon binary, so it never runs its own executable as the daemon. It looks
for the installed `pi-famulus` binary and starts that. The first of these that is an
executable file, and is not the GUI itself, wins:

1. `managerPath` in `<home>/config.json`
2. `PI_FAMULUS_MANAGER_PATH`
3. The native npm package of this platform, `pi-famulus-<linux|darwin|win>-<x64|arm64>`
   (`bin/pi-famulus`, `bin/pi-famulus.exe` on Windows):
   - in pi's npm tree, `~/.pi/agent/npm/node_modules/<package>`: the standard install;
   - in the extension's cache, `~/.cache/pi-famulus/native/<version>/<os>-<arch>/node_modules/<package>`
     (`$XDG_CACHE_HOME` if set; `%LOCALAPPDATA%` on Windows), for the version this GUI was
     built from.
4. `<home>/bin/pi-famulus`
5. `pi-famulus` in each directory of `PATH`
6. `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin` (not on Windows)

Steps 1 to 5 are the extension's order ([configuration](../docs/configuration.md#manager-binary-discovery)).
Step 3 differs in how it gets there: the extension walks up from its own files and insists
on its own exact version; the GUI looks in the two places such an install puts the package
and takes the version it finds in pi's tree. `~/.pi/agent` is always pi's default agent
directory, even when `PI_FAMULUS_HOME` points elsewhere. Step 6 is there because an app
opened from the Finder or a desktop launcher does not get your shell's `PATH`.

The daemon that gets started is the installed binary, so it can be a different build from
the library this GUI was compiled with. When the daemon's version differs, the title shows
both: `<daemon version> · this window is built from <its own>`.

When nothing is live, the header says why, and the window keeps retrying once a second:

- `daemon not running; pi-famulus binary not found`: nothing is started. The window shows
  what is on disk and attaches as soon as a daemon runs. The binary is looked for again
  every 2 seconds.
- `daemon not reachable; starting it`: a binary was found and the daemon is not answering
  yet. If this stays, look at `<home>/manager.log`.

`RUST_LOG=info` prints the binary that was found.

## One window per user

Starting the app a second time brings the first window to the front (out of the menu bar
too) and exits. macOS does this by itself for an app bundle opened from the Finder, but not
for the binary run directly, and NSH 0.4 has no option for it (its GTK application is
`NON_UNIQUE`), so `src/instance.rs` does it:

- The first instance holds an OS file lock on `instance.lock` and listens on `instance.sock`,
  both in a per-user directory: `~/Library/Caches/network.phala.pi-famulus.gui` on macOS,
  `$XDG_RUNTIME_DIR/network.phala.pi-famulus.gui` (or `~/.cache/...`) on Linux.
- A second launch finds the lock held, sends `show` to the socket and exits: with status 0
  when the first instance confirmed, 1 when it did not answer within 3 seconds.
- The lock ends with the process. After a crash the files stay, the lock does not, and the
  next launch replaces the dead socket.

The instance is per user, not per home. A second launch with a different `PI_FAMULUS_HOME`
shows the window that is already open, on the home it was started with, and exits. Quit the
first to look at another home.

On Windows only the lock is used: a second launch exits and the first window is not brought
forward.

## Same behavior as the TUI, by construction

The TUI and the GUI drive one state machine, so they cannot drift apart:

- **One controller.** `manager/src/top/controller.rs` holds the session: state, collector,
  live agent link, copying, stopping. The TUI's `run` and this crate's `src/engine.rs` both
  drive a `Controller`. The GUI's engine runs it on its own thread and publishes a `Frame`
  per change.
- **Same keys.** `src/keys.rs` binds every key `top` handles and hands the controller the
  same `crossterm` key event the terminal would. One exception: Ctrl+C quits only on macOS.
  On Linux and Windows it is the copy chord and is left to the system; `q` quits everywhere. Filter, output search and the agent input
  are native text fields; their text reaches the controller as `Action::Text`, which leaves
  the state as typing would.
- **Same pointer actions.** Clicks on rows, tabs, scope, sort, the column header, stream tabs,
  fold chevrons and the scrollbar send `top::Action`s. Each runs the code the TUI's mouse
  handling runs for that target. The wheel moves the selection, as in the terminal; one
  wheel event moves at most 64 steps.
- **Same text.** Cells, the overview, work kinds, preview, help, the task info card, output
  search, and the agent thread come from the TUI's view functions in `top::ui`, at the size
  the window measures in character cells. Those functions also do the scroll, follow and
  match bookkeeping, so both frontends scroll and search the same way.
- **Same paging.** Like the terminal, the window renders only the rows in view.

## Window settings (menu bar, kept across launches)

Settings are kept under the app's id, `network.phala.pi-famulus.gui` (on macOS,
`~/Library/Application Support/network.phala.pi-famulus.gui/storage.json`). Earlier builds
had no id and wrote to the `default` directory that NSH apps without an id share; settings
saved there are not carried over.

- **View → Appearance:** System, Light or Dark. Each color is a dark/light pair: the window's
  own colors have hand-picked light versions, and the TUI's colors get the same hue with the
  lightness turned over (`light` and `flip` in `src/app.rs`).
- **View → Minimize to Menu Bar** (macOS, on by default): the app gets an icon in the menu
  bar. Minimizing (the yellow button, ⌘M, Window → Minimize) hides the window there and drops
  the Dock icon; clicking the icon brings the window back. NSH 0.4 has no status item, so
  `src/tray.rs` uses AppKit directly. Off, minimizing goes to the Dock as usual.

## Verified (macOS, live daemon, ~4,100 retained tasks, 248 agents)

- Tree, agents and tasks tabs show the TUI's columns and values. Scope cycles through
  active, connected and all; tab counts and the `n/total` position update.
- A task detail shows the pinned info card, the `output` / `stderr` tabs, the line count and
  the footer keys. An agent detail shows its usage line and the live input with its facts.
- Clicks reach the controller (logged end to end): tabs, scope, rows, second click to open.
- Appearance switches between System, Light and Dark at once and is kept. In tray mode the
  menu bar icon shows; the yellow button, ⌘M and Window → Minimize hide the window and the
  Dock icon; the icon and reopening the app bring them back (checked by hand).
- The release build uses about 210 MB (the earlier spike, with a native row per task, used
  837 MB). CPU is 30–48% at the default 1 s refresh. The release TUI under the same load
  uses 22–42% CPU, so the cost is the shared collector, not the window.

If the session behind the window stops (its thread ends without a quit), the header shows
`session stopped · this data is stale` and the status line says to quit and reopen. The last
data stays readable; nothing on it responds.

## Not verified yet

- **Not every key.** The main keys were tried by hand in the window and behave as in the
  TUI. Not every key was covered. Stopping a task (`x`, then `y`) and sending to or aborting
  an agent were deliberately left untried, so no real process or agent was touched. Both
  run the TUI's code path, and stop asks for confirmation first.
- Linux and Windows.

## Known limits

Found by reading the code, not by running it. Each needs checking by hand at the window:

- **Keys while a text field has the focus.** NSH's default binding mode suppresses most key
  bindings in a focused input. In the filter, the output search and the agent input, esc-esc
  (abort), alt-enter (queue) and the scroll keys probably do not reach the controller.
- **The wheel** may drag the table's scroll view as well as move the selection.
- **Keyboard layouts other than US.** Keys are bound by the US keystroke (`shift+/` for `?`,
  `shift+7` for `&`, and so on).
- **Hidden in the menu bar**, the app keeps its 1 s refresh and its supervision lease on the
  daemon, and there is no Quit while it is hidden.
- **Linux** needs GTK 4.14 or newer (Ubuntu 24.04+) and has not been built. **Windows** has
  not been built. The code that only those systems compile (the window-to-front call, the
  lock-only instance check on Windows) has never been compiled.
- **Bringing the window forward on a second launch** is tested up to the request reaching
  the first instance, not by watching the window.
- **A daemon that refuses this build's hello** (a build too far apart) is retried once a
  second for as long as the window is open, and each retry starts the installed binary,
  which exits at once on the running daemon's lock. Nothing is damaged; it is one
  short-lived process a second, and the header stays on `daemon not reachable`.
- **Ctrl+X** (stop the selected task) is still bound on Linux and Windows, where it is the
  cut chord.
- The "preview needs a taller terminal" message says "terminal" in the window too. It shows
  when the window is under 20 rows (about 340 points) tall.

## Known

- An agent whose live session has no entries shows `(no conversation yet)` once the live
  link connects, even though its transcript on disk has turns. This is the shared
  `apply_live` code, so the TUI does the same.
- `cargo build` pulls a newer tokio than `manager/Cargo.lock` pins, and that version
  deprecates `AsyncFd::new` (`manager/src/intercom/native/alias.rs`). The warning shows up
  only in the GUI build.
- `nsh package --target macos` builds a 46 MB ad hoc signed `.app` with the repo logo as its
  icon.
