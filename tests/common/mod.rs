// Each test file uses a different subset of these helpers, so some are unused in each crate.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

pub fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("git runs")
        .status
        .success()
}

/// A repository with one commit.
pub fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "tester"]);
    git(dir, &["config", "user.email", "tester@example.com"]);
    std::fs::write(dir.join("README.md"), "# sample\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

pub fn commit_file(ws: &Path, name: &str, body: &str) -> String {
    std::fs::write(ws.join(name), body).unwrap();
    git(ws, &["add", name]);
    git(ws, &["commit", "-q", "-m", name]);
    git(ws, &["rev-parse", "HEAD"])
}
