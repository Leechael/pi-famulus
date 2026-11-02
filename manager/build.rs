//! Embed the commit this binary is built from as `PI_FAMULUS_GIT_SHA`, so that
//! `status` and upgrade logs can tell two builds of the same version apart.
//! Outside a git checkout it is "unknown".

use std::path::PathBuf;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

fn main() {
    println!("cargo:rerun-if-env-changed=PI_FAMULUS_GIT_SHA");
    let sha = std::env::var("PI_FAMULUS_GIT_SHA")
        .ok()
        .map(|sha| sha.trim().to_string())
        .filter(|sha| !sha.is_empty())
        .unwrap_or_else(|| {
            let mut sha =
                git(&["rev-parse", "--short=10", "HEAD"]).unwrap_or_else(|| "unknown".into());
            // Uncommitted changes are invisible in HEAD; mark a dirty build so it
            // does not look identical to the clean build of the same commit (the
            // very case this marker exists to disambiguate).
            if sha != "unknown" && git(&["diff-index", "--quiet", "HEAD", "--"]).is_none() {
                sha.push_str("-dirty");
            }
            sha
        });
    println!("cargo:rustc-env=PI_FAMULUS_GIT_SHA={sha}");
    // Rebuild when HEAD moves: HEAD itself (branch switch) and the ref it
    // names (a commit). Worktrees keep HEAD in their own git dir and refs in
    // the common one.
    if let (Some(dir), Some(common)) = (
        git(&["rev-parse", "--git-dir"]),
        git(&["rev-parse", "--git-common-dir"]),
    ) {
        let (dir, common) = (PathBuf::from(dir), PathBuf::from(common));
        println!("cargo:rerun-if-changed={}", dir.join("HEAD").display());
        // Cargo treats a missing watched file as perpetually stale. Fresh
        // repositories have no packed-refs; packing removes loose refs.
        let packed = common.join("packed-refs");
        if packed.exists() {
            println!("cargo:rerun-if-changed={}", packed.display());
        }
        if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
            let mut reference = common.join(r);
            // A packed-only branch can become loose via update-ref without
            // changing HEAD or packed-refs. Watch its nearest existing parent
            // until the loose ref exists, rather than silently missing that
            // transition. Packing a watched loose ref invalidates it as well.
            while !reference.exists() && reference.pop() {}
            println!("cargo:rerun-if-changed={}", reference.display());
        }
    }
}
