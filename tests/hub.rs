//! Hub git semantics: the first push wins and the loser must rebase. Merge checks and export.

mod common;

use colony::hub::{Hub, BRANCH};
use common::{commit_file, git, git_ok, init_repo};

#[tokio::test]
async fn first_push_wins_and_merge_checks_follow_the_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let run = tmp.path().join("run");
    std::fs::create_dir_all(&run).unwrap();

    let (hub, sha) = Hub::create(&repo, "HEAD", &run).await.unwrap();
    assert_eq!(sha, base);
    assert_eq!(hub.head().await.unwrap(), base);

    let (a, b) = (run.join("ws/w1"), run.join("ws/w2"));
    hub.clone_workspace(&a, "w1", "me@example.com").await.unwrap();
    hub.clone_workspace(&b, "w2", "me@example.com").await.unwrap();
    assert_eq!(git(&a, &["config", "user.name"]), "colony w1");
    std::fs::create_dir_all(a.join(".colony-tmp")).unwrap();
    std::fs::write(a.join(".colony-tmp/probe.mjs"), "1").unwrap();
    assert_eq!(
        git(&a, &["status", "--porcelain"]),
        "",
        "scratch files are ignored"
    );

    let sha_a = commit_file(&a, "a.txt", "a");
    let sha_b = commit_file(&b, "b.txt", "b");
    assert!(git_ok(&a, &["push", "-q", "origin", &format!("HEAD:{BRANCH}")]));
    assert!(
        !git_ok(&b, &["push", "-q", "origin", &format!("HEAD:{BRANCH}")]),
        "a non-fast-forward push must be rejected"
    );
    assert!(
        !git_ok(
            &b,
            &["push", "-q", "--force", "origin", &format!("HEAD:{BRANCH}")]
        ),
        "force push must be rejected too"
    );

    assert_eq!(
        hub.merged_sha(&sha_a[..10]).await.unwrap(),
        Some(sha_a.clone()),
        "short sha resolves"
    );
    assert_eq!(
        hub.merged_sha(&sha_b).await.unwrap(),
        None,
        "unpushed commit is not merged"
    );
    assert_eq!(hub.merged_sha("not-a-sha").await.unwrap(), None);

    git(&b, &["pull", "-q", "--rebase", "origin", BRANCH]);
    assert!(git_ok(&b, &["push", "-q", "origin", &format!("HEAD:{BRANCH}")]));
    let rebased = git(&b, &["rev-parse", "HEAD"]);
    assert_eq!(hub.merged_sha(&rebased).await.unwrap(), Some(rebased.clone()));
    assert_eq!(
        hub.merged_sha(&sha_a).await.unwrap(),
        Some(sha_a),
        "ancestors stay merged"
    );

    hub.export(&repo, "colony/test").await.unwrap();
    assert_eq!(git(&repo, &["rev-parse", "colony/test"]), rebased);
    assert_eq!(
        git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main",
        "user's checkout is untouched"
    );
    assert!(!repo.join("a.txt").exists(), "user's working tree is untouched");
}

#[tokio::test]
async fn ai_trailers_are_stripped_on_commit_and_rejected_on_push() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    // Trailers in the user's existing history must not block worker pushes.
    std::fs::write(repo.join("old.txt"), "old").unwrap();
    git(&repo, &["add", "old.txt"]);
    git(
        &repo,
        &[
            "commit",
            "-q",
            "-m",
            "old\n\nCo-Authored-By: Claude Opus <noreply@anthropic.com>",
        ],
    );
    let run = tmp.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    let (hub, _) = Hub::create(&repo, "HEAD", &run).await.unwrap();
    let ws = run.join("ws/w1");
    hub.clone_workspace(&ws, "w1", "me@example.com").await.unwrap();

    std::fs::write(ws.join("a.txt"), "a").unwrap();
    git(&ws, &["add", "a.txt"]);
    let msg = "Add a\n\nKeeps this line.\n\nCo-Authored-By: Claude Haiku 5.5 <noreply@anthropic.com>\n\
               🤖 Generated with [Claude Code](https://claude.com/claude-code)\n";
    git(&ws, &["commit", "-q", "-m", msg]);
    assert_eq!(
        git(&ws, &["log", "-1", "--format=%B"]),
        "Add a\n\nKeeps this line."
    );
    assert!(git_ok(&ws, &["push", "-q", "origin", &format!("HEAD:{BRANCH}")]));

    std::fs::write(ws.join("b.txt"), "b").unwrap();
    git(&ws, &["add", "b.txt"]);
    git(
        &ws,
        &[
            "commit",
            "-q",
            "--no-verify",
            "-m",
            "Add b\n\nCo-authored-by: Claude <noreply@anthropic.com>",
        ],
    );
    assert!(
        !git_ok(&ws, &["push", "-q", "origin", &format!("HEAD:{BRANCH}")]),
        "a trailer that skipped the commit hook is refused by the hub"
    );
    git(&ws, &["commit", "-q", "--amend", "-m", "Add b"]);
    assert!(git_ok(&ws, &["push", "-q", "origin", &format!("HEAD:{BRANCH}")]));
}
