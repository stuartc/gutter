//! Stamps the binary with `git describe` so two builds of the same crate version
//! can be told apart, falling back to `CARGO_PKG_VERSION` where there is no git
//! or no checkout (release tarballs, `cargo install` from a package).
//!
//! The stamp describes the tree as it stood when `HEAD` last moved, or since the
//! last `cargo clean`: cargo caches this script's output and reruns it only when
//! a watched path changes, and the only watched paths are `HEAD` and the branch
//! ref. Editing a source file does not refresh it, so the `-dirty` suffix is
//! best-effort in both directions — a tree that has since been dirtied may be
//! stamped clean, and one stamped `-dirty` keeps that suffix after it is cleaned.
//! `HEAD` alone is not enough to watch: it is a symref that moves on checkout,
//! while an ordinary commit moves `refs/heads/<branch>`, so both are watched. A
//! branch whose ref is packed has no loose file to watch; naming it anyway costs
//! a rerun of this script per build, which is cheaper than reporting a commit
//! that has moved on.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let ours = describes_this_crate();
    let stamp = ours.then(git_describe).flatten().unwrap_or_else(|| {
        std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string())
    });
    println!("cargo:rustc-env=GUTTER_VERSION={stamp}");

    if !ours {
        return;
    }
    let Some(git_dir) = git_path("--absolute-git-dir") else {
        return;
    };
    let head = git_dir.join("HEAD");
    // A linked worktree has a HEAD of its own but shares refs with the main
    // checkout, so the ref it names lives under the common dir, not this one.
    if let (Some(reference), Some(common)) = (head_ref(&head), git_path("--git-common-dir")) {
        println!("cargo:rerun-if-changed={}", common.join(reference).display());
    }
    println!("cargo:rerun-if-changed={}", head.display());
}

/// Whether the checkout git finds from here is this crate's own. Unpacking the
/// source inside an unrelated repository would otherwise stamp that repository's
/// tag. A linked worktree answers with its own root, which is the manifest dir
/// there too, so those still describe.
fn describes_this_crate() -> bool {
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .ok()
        .and_then(|dir| PathBuf::from(dir).canonicalize().ok());
    match (git_path("--show-toplevel"), manifest) {
        (Some(top), Some(dir)) => top == dir,
        _ => false,
    }
}

fn git_describe() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--tags", "--dirty", "--always"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// One of `git rev-parse`'s directory answers, made absolute. `None` outside a
/// checkout. Asking git rather than joining `.git` onto the manifest dir keeps
/// worktrees and submodules — where `.git` is a file pointing elsewhere — working.
fn git_path(flag: &str) -> Option<PathBuf> {
    let out = Command::new("git").args(["rev-parse", flag]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    // `--git-common-dir` answers relatively when the cwd is the checkout root.
    let path = path.canonicalize().ok()?;
    path.is_dir().then_some(path)
}

/// The ref path a symbolic `HEAD` names, e.g. `refs/heads/main`. `None` for a
/// detached HEAD, which holds a raw object id that only changes with `HEAD` itself.
fn head_ref(head: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(head).ok()?;
    Some(contents.trim().strip_prefix("ref: ")?.to_string())
}
