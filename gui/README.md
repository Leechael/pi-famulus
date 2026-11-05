# pi-famulus GUI (spike)

A native desktop window that shows the same task list as `pi-famulus top`, built on
[NSH](https://nsh.dev/docs/) 0.4.0. It is a spike, not a shipped component: it is not
part of the manager's workspace, CI, or npm packages.

```sh
cd gui
cargo run          # AppKit on macOS, GTK 4 on Linux, Win32 on Windows
```

`rust-toolchain.toml` pins Rust 1.99.1, because nsh-core 0.4.0 uses `isolate_lowest_one`,
which Rust 1.95 does not have as a stable API. The manager still builds with plain `stable`.

## How it is wired

- `manager` now has a library target (`src/lib.rs`), and `main.rs` imports it. The GUI
  depends on `pi-famulus` by path and reuses `top::collector::Collector` directly. Nothing in
  `top::{model,collector,live}` depends on ratatui.
- `spawn` runs on NSH's engine thread and owns the signals. `background` is `tokio::spawn`
  on NSH's own multi-thread runtime, which is built with `enable_all`. The collector's IPC
  and disk reads run there. The collector moves into each tick and comes back with the
  result, so no lock is needed. See `src/app.rs`.

## What the spike showed (macOS, debug build, real home with ~4100 tasks)

- It works: the daemon is reached, the list refreshes every second (ages tick, new tasks
  appear), and idle CPU stays under 1%.
- RSS is about 685 MB. `each` creates a native view for every one of the ~4100 rows, and
  nothing is virtualized. A real GUI needs NSH's list component or windowing before it can
  show the full history.
- `cargo build` pulls a newer tokio than `manager/Cargo.lock` pins, and that version
  deprecates `AsyncFd::new` (`manager/src/intercom/native/alias.rs`). The warning shows up
  only in the GUI build.
- An unbundled binary is not a macOS app. Screenshots and other tooling need an `.app`
  bundle, which `nsh package --target macos` produces.

## Not done

Detail view, agents tab, stop/copy actions, and testing on Linux or Windows.
