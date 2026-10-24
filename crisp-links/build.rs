//! Works out the version string the binary reports (`crisp-links --version`).
//!
//! Priority:
//! 1. `PIPEWIRE_CRISP_VOCALS_VERSION` in the environment. Release builds set this to the release version.
//! 2. `git describe` on a checkout:
//!    - exactly on a tag:          `0.2.0`
//!    - commits past a tag:        `0.2.0+3.gabc1234`
//!    - uncommitted changes:       `...+dirty` (added to the end)
//! 3. No tags or no git (e.g. an AUR tarball): `0.0.0+g<hash>`, or `0.0.0` without git.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

fn git_version() -> Option<String> {
    // `v0.2.0-3-gabc1234`
    let described = git(&["describe", "--tags", "--long", "--match", "v[0-9]*"])?;
    // `status` refreshes the index first, so a file that was edited and then restored reads as clean.
    // (`describe --dirty` can report stale results for that case.)
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    let mut parts = described.rsplitn(3, '-');
    let hash = parts.next()?;
    let count: u32 = parts.next()?.parse().ok()?;
    let tag = parts.next()?.strip_prefix('v')?;

    let mut meta = Vec::new();
    if count > 0 {
        meta.push(format!("{count}.{hash}"));
    }
    if dirty {
        meta.push("dirty".to_string());
    }
    Some(if meta.is_empty() { tag.to_string() } else { format!("{tag}+{}", meta.join(".")) })
}

fn untagged_version() -> String {
    match git(&["rev-parse", "--short", "HEAD"]) {
        Some(hash) => format!("0.0.0+g{hash}"),
        None => "0.0.0".to_string(),
    }
}

/// Re-run this script when the checked-out commit, its branch, tags, or any tracked file change
/// (`dirty` depends on the contents of tracked files, not just on commits). The repo is a workspace,
/// so tracked files are watched from the repo root, not just this crate's directory.
fn watch_git() {
    for p in ["HEAD", "packed-refs", "index", "refs/tags"] {
        if let Some(path) = git(&["rev-parse", "--git-path", p]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let Some(branch_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&["rev-parse", "--git-path", &branch_ref]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    if let (Some(top), Some(files)) = (git(&["rev-parse", "--show-toplevel"]), git(&["ls-files", "--full-name"])) {
        for file in files.lines() {
            println!("cargo:rerun-if-changed={top}/{file}");
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=PIPEWIRE_CRISP_VOCALS_VERSION");
    watch_git();

    let version = std::env::var("PIPEWIRE_CRISP_VOCALS_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(git_version)
        .unwrap_or_else(untagged_version);
    println!("cargo:rustc-env=PIPEWIRE_CRISP_VOCALS_VERSION={version}");
}
