//! The shared workspace (the role Gitea plays in Agensh). Each run gets one bare repository as its hub;
//! every worker works in its own clone of the hub and merges by pushing to `colony-main`.
//!
//! The hub rejects non-fast-forward pushes, so "first to merge wins; the other rebases and pushes again"
//! is enforced by git alone, with no harness code. The user's repository is read once at start, and the
//! only write is fetching one result branch into it at the end.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

pub const BRANCH: &str = "colony-main";

/// Git-ignored scratch directory in every worker clone.
pub const SCRATCH_DIR: &str = ".colony-tmp";

/// Trailers that AI tools append to commit messages (`Co-Authored-By: Claude …`, `🤖 Generated with …`).
/// As a rule, none of them may survive in colony's output. A `grep -E -i` pattern.
pub const AI_TRAILER_PATTERN: &str =
    "^[[:space:]]*co-authored-by:.*(claude|anthropic|copilot|openai|codex|gemini)\
|generated (with|by) .*(claude|copilot|codex|gemini)";

/// commit-msg hook for worker clones: deletes AI trailer lines and the blank lines they leave behind.
fn commit_msg_hook() -> String {
    format!(
        r#"#!/bin/sh
# colony: strip AI trailers.
f="$1"
grep -v -i -E '{AI_TRAILER_PATTERN}' "$f" \
  | awk '{{ l[NR] = $0 }} END {{ n = NR; while (n > 0 && l[n] ~ /^[[:space:]]*$/) n--; for (i = 1; i <= n; i++) print l[i] }}' \
  > "$f.colony" && mv "$f.colony" "$f"
"#
    )
}

/// pre-receive hook for the hub: the last line of defense, which also catches commits that skipped hooks
/// (`--no-verify`). For a new branch it only inspects commits unreachable from existing refs, so the user's
/// existing history is never checked.
fn pre_receive_hook() -> String {
    format!(
        r#"#!/bin/sh
# colony: reject commits that carry AI trailers.
zero=0000000000000000000000000000000000000000
bad=0
while read old new ref; do
  [ "$new" = "$zero" ] && continue
  if [ "$old" = "$zero" ]; then commits=$(git rev-list "$new" --not --all); else commits=$(git rev-list "$old..$new"); fi
  for c in $commits; do
    if git log -1 --format=%B "$c" | grep -q -i -E '{AI_TRAILER_PATTERN}'; then
      echo "colony: commit $c carries an AI trailer (e.g. Co-Authored-By: Claude). Remove that line from the message (git commit --amend for the last commit) and push again." >&2
      bad=1
    fi
  done
done
exit $bad
"#
    )
}

async fn install_hook(hooks_dir: &Path, name: &str, body: &str) -> Result<()> {
    tokio::fs::create_dir_all(hooks_dir).await?;
    let path = hooks_dir.join(name);
    tokio::fs::write(&path, body).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).await?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Hub {
    pub dir: PathBuf,
}

impl Hub {
    /// Creates the hub at `run_dir/hub.git`, starting from `repo`'s `base` commit. Returns the base sha.
    pub async fn create(repo: &Path, base: &str, run_dir: &Path) -> Result<(Hub, String)> {
        let sha = git(repo, &["rev-parse", "--verify", &format!("{base}^{{commit}}")])
            .await
            .with_context(|| format!("base commit `{base}` not found"))?;
        let dir = run_dir.join("hub.git");
        // A clone from a local path hardlinks objects, so even large repositories are fast.
        git(
            run_dir,
            &[
                "clone",
                "--bare",
                "--quiet",
                "--no-tags",
                &repo.to_string_lossy(),
                &dir.to_string_lossy(),
            ],
        )
        .await
        .context("failed to create the hub")?;
        let hub = Hub { dir };
        hub.git(&["update-ref", &format!("refs/heads/{BRANCH}"), &sha])
            .await?;
        hub.git(&["symbolic-ref", "HEAD", &format!("refs/heads/{BRANCH}")])
            .await?;
        hub.git(&["config", "receive.denyNonFastForwards", "true"])
            .await?;
        hub.git(&["config", "receive.denyDeletes", "true"]).await?;
        // Pin hooksPath to an absolute path so the hub's hooks run even under a global core.hooksPath.
        let hooks = hub.dir.join("hooks");
        install_hook(&hooks, "pre-receive", &pre_receive_hook()).await?;
        hub.git(&["config", "core.hooksPath", &hooks.to_string_lossy()])
            .await?;
        Ok((hub, sha))
    }

    /// A worker workspace. Fetches only `colony-main` and records the worker as the commit author.
    pub async fn clone_workspace(&self, dest: &Path, worker: &str, email: &str) -> Result<()> {
        let parent = dest.parent().context("workspace path has no parent directory")?;
        tokio::fs::create_dir_all(parent).await?;
        git(
            parent,
            &[
                "clone",
                "--quiet",
                "--single-branch",
                "--branch",
                BRANCH,
                &self.dir.to_string_lossy(),
                &dest.to_string_lossy(),
            ],
        )
        .await
        .with_context(|| format!("failed to clone the workspace for {worker}"))?;
        git(dest, &["config", "user.name", &format!("colony {worker}")]).await?;
        git(dest, &["config", "user.email", email]).await?;
        // If setup (husky, etc.) changes core.hooksPath this hook won't run; the hub's pre-receive then
        // catches the trailer instead.
        install_hook(&dest.join(".git").join("hooks"), "commit-msg", &commit_msg_hook()).await?;
        // Scratch space inside the clone. Writes outside it are denied, and a real run's workers otherwise
        // put their probes in /tmp.
        let exclude = dest.join(".git").join("info").join("exclude");
        let mut text = tokio::fs::read_to_string(&exclude).await.unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("/{SCRATCH_DIR}/\n"));
        tokio::fs::create_dir_all(exclude.parent().context("exclude path has no parent")?).await?;
        tokio::fs::write(&exclude, text).await?;
        Ok(())
    }

    pub async fn head(&self) -> Result<String> {
        self.git(&["rev-parse", &format!("refs/heads/{BRANCH}")]).await
    }

    /// Accepts short shas too. Returns the full sha if the commit is on the hub and is `colony-main` or one
    /// of its ancestors.
    pub async fn merged_sha(&self, commit: &str) -> Result<Option<String>> {
        let commit = commit.trim();
        if commit.is_empty() || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let Ok(full) = self
            .git(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{commit}^{{commit}}"),
            ])
            .await
        else {
            return Ok(None);
        };
        let status = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args([
                "merge-base",
                "--is-ancestor",
                &full,
                &format!("refs/heads/{BRANCH}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;
        Ok(status.success().then_some(full))
    }

    /// Fetches the result into the user's repository as a branch. Leaves the working tree and current
    /// branch untouched.
    pub async fn export(&self, repo: &Path, branch: &str) -> Result<()> {
        git(
            repo,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                &self.dir.to_string_lossy(),
                &format!("refs/heads/{BRANCH}:refs/heads/{branch}"),
            ],
        )
        .await
        .with_context(|| format!("failed to fetch result branch {branch}"))?;
        Ok(())
    }

    async fn git(&self, args: &[&str]) -> Result<String> {
        git(&self.dir, args).await
    }
}

/// Resets the verify workspace to the hub's latest commit. Ignored files (node_modules, etc.) are kept.
pub async fn sync_to_hub(ws: &Path) -> Result<String> {
    git(ws, &["fetch", "--quiet", "origin", BRANCH]).await?;
    git(ws, &["reset", "--quiet", "--hard", &format!("origin/{BRANCH}")]).await?;
    git(ws, &["clean", "--quiet", "-fd"]).await?;
    git(ws, &["rev-parse", "HEAD"]).await
}

pub async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub async fn repo_root(path: &Path) -> Result<PathBuf> {
    let root = git(path, &["rev-parse", "--show-toplevel"])
        .await
        .with_context(|| format!("{} is not a git repository", path.display()))?;
    Ok(PathBuf::from(root))
}

pub async fn is_dirty(repo: &Path) -> Result<bool> {
    Ok(!git(repo, &["status", "--porcelain", "--untracked-files=no"])
        .await?
        .is_empty())
}

pub async fn user_email(repo: &Path) -> String {
    git(repo, &["config", "user.email"])
        .await
        .unwrap_or_else(|_| "colony@localhost".into())
}

/// Result of one shell command. Output is stdout followed by stderr, truncated to the tail.
#[derive(Debug, Clone)]
pub struct ShellOutcome {
    pub ok: bool,
    pub code: Option<i32>,
    pub tail: String,
}

pub async fn run_shell(cwd: &Path, cmd: &str, timeout: Duration, tail_lines: usize) -> Result<ShellOutcome> {
    let child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to run `{cmd}`"))?;
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(out) => out?,
        Err(_) => {
            return Ok(ShellOutcome {
                ok: false,
                code: None,
                tail: format!("timed out after {}s", timeout.as_secs()),
            })
        }
    };
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(tail_lines)..].join("\n");
    Ok(ShellOutcome {
        ok: out.status.success(),
        code: out.status.code(),
        tail,
    })
}
