//! Regression: Git metadata must invalidate the embedded SHA without making
//! unchanged Cargo builds dirty when packed-refs or the loose ref is absent.
//! Uses tiny dependency-free Cargo fixtures, never this checkout's Git state.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};

mod common;

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    // common makes HOME hermetic. Use Cargo's actual toolchain binaries,
    // not rustup's PATH shims, which need the developer's ~/.rustup settings.
    let cargo = Path::new(env!("CARGO"));
    let executable = if program == "cargo" {
        cargo
    } else {
        Path::new(program)
    };
    let output = common::command(executable)
        .env("RUSTC", cargo.with_file_name(format!("rustc{}", std::env::consts::EXE_SUFFIX)))
        .args(args)
        .current_dir(dir)
        .env_remove("PI_FAMULUS_GIT_SHA")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8(run(dir, "git", args).stdout)
        .unwrap()
        .trim()
        .to_owned()
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "famulus-build-script-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::create_dir(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"git-stamp-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            dir.join("src/main.rs"),
            "fn main() { println!(\"{}\", env!(\"PI_FAMULUS_GIT_SHA\")); }\n",
        )
        .unwrap();
        fs::write(dir.join("build.rs"), include_str!("../build.rs")).unwrap();
        fs::write(dir.join(".gitignore"), "/target\n/Cargo.lock\n").unwrap();
        Self(dir)
    }

    fn init(&self) {
        git(&self.0, &["init", "-b", "main"]);
        git(
            &self.0,
            &["config", "user.email", "build-test@example.invalid"],
        );
        git(&self.0, &["config", "user.name", "Build test"]);
        git(&self.0, &["config", "commit.gpgsign", "false"]);
        git(&self.0, &["add", "."]);
        git(&self.0, &["commit", "-m", "fixture"]);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn build(dir: &Path, fresh: bool) {
    let output = run(
        dir,
        "cargo",
        &["build", "--offline", "--message-format=json"],
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let artifact = stdout
        .lines()
        .find(|line| line.contains("\"kind\":[\"bin\"]") && line.contains("compiler-artifact"))
        .expect("fixture binary artifact");
    assert!(
        artifact.contains(&format!("\"fresh\":{fresh}")),
        "expected fresh={fresh}: {artifact}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stamp(dir: &Path) -> String {
    let binary = dir
        .join("target/debug")
        .join(format!("git-stamp-fixture{}", std::env::consts::EXE_SUFFIX));
    String::from_utf8(run(dir, binary.to_str().unwrap(), &[]).stdout)
        .unwrap()
        .trim()
        .to_owned()
}

fn assert_head(dir: &Path) {
    assert_eq!(stamp(dir), git(dir, &["rev-parse", "--short=10", "HEAD"]));
}

#[test]
fn fresh_builds_survive_missing_packed_and_loose_refs_and_detect_ref_updates() {
    let fixture = Fixture::new();
    fixture.init();
    let dir = &fixture.0;
    assert!(!dir.join(".git/packed-refs").exists());
    build(dir, false);
    assert_head(dir);
    build(dir, true);

    git(dir, &["pack-refs", "--all", "--prune"]);
    assert!(!dir.join(".git/refs/heads/main").exists());
    build(dir, false);
    build(dir, true);
    assert_head(dir);

    // update-ref creates a loose ref without touching symbolic HEAD or packed-refs.
    let tree = git(dir, &["rev-parse", "HEAD^{tree}"]);
    let next = git(dir, &["commit-tree", &tree, "-p", "HEAD", "-m", "next"]);
    git(dir, &["update-ref", "refs/heads/main", &next]);
    build(dir, false);
    assert_head(dir);
    build(dir, true);

    git(dir, &["checkout", "--detach", "HEAD~1"]);
    build(dir, false);
    assert_head(dir);
    build(dir, true);
}

#[test]
fn linked_worktree_tracks_common_refs_and_its_own_head() {
    let fixture = Fixture::new();
    fixture.init();
    let worktree = fixture.0.join("linked");
    git(
        &fixture.0,
        &[
            "worktree",
            "add",
            "-b",
            "linked",
            worktree.to_str().unwrap(),
        ],
    );
    build(&worktree, false);
    assert_head(&worktree);
    build(&worktree, true);
    git(
        &worktree,
        &["commit", "--allow-empty", "-m", "worktree change"],
    );
    build(&worktree, false);
    assert_head(&worktree);
    build(&worktree, true);
}

#[test]
fn source_archive_builds_stay_fresh_without_git() {
    let fixture = Fixture::new();
    build(&fixture.0, false);
    assert_eq!(stamp(&fixture.0), "unknown");
    build(&fixture.0, true);
}
