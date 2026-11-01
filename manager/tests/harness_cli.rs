//! Behavioral compatibility checks for the custom runner's libtest CLI.
//! Compile a tiny dependency-free executable containing the real runner;
//! these checks run once, not once per suite importing common.
mod common;

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    dir: PathBuf,
    binary: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "famulus-harness-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let binary = dir.join(format!("fixture{}", std::env::consts::EXE_SUFFIX));
        let fixture = Self { dir, binary };
        // common makes HOME hermetic: bypass rustup's PATH shim.
        let rustc = Path::new(env!("CARGO"))
            .with_file_name(format!("rustc{}", std::env::consts::EXE_SUFFIX));
        let output = common::command(rustc)
            .arg("--edition=2021")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/harness_cli.rs"))
            .arg("-o")
            .arg(&fixture.binary)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "rustc: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        fixture
    }

    fn run(&self, args: &[&str]) -> Output {
        common::command(&self.binary)
            // No MSYS keeper is needed for this fixture on Windows.
            .env("PI_FAMULUS_SHELL", "cmd.exe")
            .args(args)
            .output()
            .unwrap()
    }

    fn stdout(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn filters_exact_and_skip_select_the_same_tests_for_listing_and_execution() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.stdout(&["--list", "alpha"]),
        "alpha: test\nalphabet: test\n"
    );
    assert_eq!(
        fixture.stdout(&["--list", "--exact", "alpha"]),
        "alpha: test\n"
    );
    assert_eq!(
        fixture.stdout(&["--list", "alpha", "--skip", "alphabet"]),
        "alpha: test\n"
    );
    assert_eq!(
        fixture.stdout(&["--list", "alpha", "--skip=alphabet"]),
        "alpha: test\n"
    );
    assert_eq!(
        fixture.stdout(&["--list", "--exact", "alpha", "--skip", "alpha"]),
        ""
    );
    assert_eq!(
        fixture.stdout(&["--list", "--exact", "alpha", "--skip", "alph"]),
        "alpha: test\n"
    );
    assert_eq!(
        fixture.stdout(&["--list", "--format", "terse", "missing"]),
        ""
    );
    // nextest runs one exact test, with libtest's capture and thread flags.
    let output = fixture.stdout(&["--exact", "alpha", "--nocapture", "--test-threads", "1"]);
    assert!(output.contains("ran alpha\n"), "{output}");
    assert!(
        !output.contains("ran alphabet") && !output.contains("ran beta"),
        "{output}"
    );
    assert!(output.contains("1 passed; 0 failed"), "{output}");
    let output = fixture.stdout(&["alpha", "--skip=alphabet", "--test-threads=1", "-q"]);
    assert!(output.contains("ran alpha\n"), "{output}");
    assert!(
        !output.contains("ran alphabet") && !output.contains("ran beta"),
        "{output}"
    );
    assert!(output.contains("1 passed; 0 failed"), "{output}");
}

#[test]
fn failed_tests_keep_a_failing_exit_status_and_task_dispatch_bypasses_the_runner() {
    let fixture = Fixture::new();
    let output = fixture.run(&["--exact", "fails", "--nocapture"]);
    assert_eq!(output.status.code(), Some(101), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("0 passed; 1 failed"), "{stdout}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("fixture failure"));
    assert_eq!(
        fixture.stdout(&["taskkit", "echo", "hello", "world"]),
        "hello world\n"
    );
    assert_eq!(
        fixture.run(&["taskkit", "exit", "7"]).status.code(),
        Some(7)
    );
    assert_eq!(fixture.stdout(&["fixture-helper"]), "helper dispatched\n");
}

#[test]
fn ignored_only_is_empty_but_include_ignored_preserves_registered_tests() {
    let fixture = Fixture::new();
    assert_eq!(fixture.stdout(&["--list", "--ignored"]), "");
    assert_eq!(
        fixture.stdout(&["--list", "--format", "terse", "--ignored"]),
        ""
    );
    assert_eq!(
        fixture.stdout(&["--list", "--include-ignored"]),
        "alpha: test\nalphabet: test\nbeta: test\nfails: test\n"
    );
    let output = fixture.stdout(&["--ignored"]);
    assert!(output.contains("0 passed; 0 failed"), "{output}");
    assert!(!output.contains("ran "), "{output}");
    let output = fixture.stdout(&["--include-ignored", "--exact", "alpha"]);
    assert!(output.contains("ran alpha\n"), "{output}");
    assert!(output.contains("1 passed; 0 failed"), "{output}");
}

#[test]
fn terse_listing_preserves_the_full_inventory() {
    let fixture = Fixture::new();
    let inventory = "alpha: test\nalphabet: test\nbeta: test\nfails: test\n";
    assert_eq!(fixture.stdout(&["--list"]), inventory);
    assert_eq!(fixture.stdout(&["--list", "--format", "terse"]), inventory);
    assert_eq!(fixture.stdout(&["--list", "--format=terse"]), inventory);
}
