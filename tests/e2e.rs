//! Runs the whole harness with three fake claudes (examples/fake_claude.rs). Everything but the LLM is
//! real: hub, clones and racing pushes, HTTP MCP, the supervisor's turn allocation, wake-ups and
//! usage-limit handling, and result branch export.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use colony::config::{Config, FileConfig, Overrides};
use colony::supervisor::StopReason;
use common::{git, init_repo};

/// Builds the fake claude and returns its path. Works even for invocations that do not build examples,
/// such as `cargo test --test e2e`.
fn fake_claude() -> PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = std::process::Command::new(cargo)
        .args(["build", "--quiet", "--example", "fake_claude"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("cargo runs");
    assert!(status.success(), "building fake_claude failed");
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().and_then(|deps| deps.parent()).unwrap();
    profile_dir.join("examples").join("fake_claude")
}

#[tokio::test(flavor = "multi_thread")]
async fn three_workers_split_the_goal_and_finish_it() {
    // Only this test uses it: ending w2's first turn on a usage limit exercises the pause/resume path.
    std::env::set_var("FAKE_LIMIT_AGENT", "w2");
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    init_repo(&repo);
    let overrides = Overrides {
        workers: Some(3),
        ramp_secs: Some(0),
        hours: Some(0.05),
        idle_secs: Some(1),
        state_dir: Some(tmp.path().join("state").to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut cfg = Config::resolve(
        "write three files".into(),
        repo.clone(),
        FileConfig::default(),
        overrides,
    )
    .unwrap();
    cfg.claude_bin = fake_claude().to_string_lossy().into_owned();

    let finished = tokio::time::timeout(Duration::from_secs(120), colony::run::run(cfg))
        .await
        .expect("run finishes in time")
        .expect("run succeeds");

    assert_eq!(finished.outcome.reason, StopReason::Complete);
    let branch = finished.branch.expect("result branch exported");
    for id in ["T2", "T3", "T4"] {
        let body = git(&repo, &["show", &format!("{branch}:{id}.txt")]);
        assert!(body.starts_with(&format!("{id} by w")), "{id}.txt: {body}");
    }
    assert_eq!(git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");

    let board: serde_json::Value =
        serde_json::from_slice(&std::fs::read(finished.run_dir.join("board.json")).unwrap()).unwrap();
    assert_eq!(board["goal"]["status"], "complete");
    let tasks = board["tasks"].as_array().unwrap();
    assert_eq!(
        tasks.len(),
        4,
        "one plan task + three planned tasks, no duplicate plans"
    );
    assert_eq!(tasks[3]["deps"][0], "T2", "dep given by title resolves to its id");
    let owners: std::collections::HashSet<&str> =
        tasks.iter().map(|t| t["claimed_by"].as_str().unwrap()).collect();
    assert!(
        owners.len() >= 2,
        "work should spread over workers, got {owners:?}"
    );
    assert_eq!(board["notes"].as_array().unwrap().len(), 3);

    let tools = std::fs::read_to_string(finished.run_dir.join("tools.jsonl")).unwrap();
    assert!(tools.contains("\"tool\":\"goal_complete\""));
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(finished.run_dir.join("summary.json")).unwrap()).unwrap();
    assert!(summary["status"]["limit"].as_u64().unwrap() >= 1);
    assert!(finished.outcome.total_cost_usd > 0.0);
}
