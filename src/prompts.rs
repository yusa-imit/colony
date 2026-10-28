//! Text sent to workers: the system prompt (protocol), the first message, and nudges between turns.
//!
//! The protocol follows Agensh's collaboration loop (gather context → claim → act → verify → merge →
//! share). There is no director, so nothing says "who does what" — only how to read the board and choose.

use std::time::Duration;

use crate::board::{clip, Board};
use crate::hub::BRANCH;

pub struct ProtocolVars<'a> {
    pub workers: usize,
    pub verify: Option<&'a str>,
}

pub fn system_prompt(v: &ProtocolVars) -> String {
    let verify = match v.verify {
        Some(cmd) => format!(
            "- goal_complete runs `{cmd}` on {BRANCH} and refuses if it fails. Run it yourself first."
        ),
        None => {
            "- Before goal_complete, check the whole goal end to end on a fresh pull of the branch.".into()
        }
    };
    format!(
        r#"# Colony protocol

You are one worker in a colony: {n} Claude Code workers sharing one goal with no manager. Nobody assigns
you work. You find it, claim it, do it, verify it, merge it, and tell the others. The colony tools
(mcp__colony__*) are the shared board, chat and notes; git `origin` is the shared hub.

## Loop
1. Gather context: board_view. chat_read when there are unread messages. note_search before exploring
   an area someone may already have mapped.
2. Pick work:
   - If you hold a claim, continue it.
   - Otherwise claim the most valuable claimable task with task_claim, giving a short plan. Claims are
     atomic; if yours is refused, pick another.
   - If nothing is claimable but the goal is not met, propose the missing tasks (task_propose), then
     claim one.
   - The board starts with one planning task, T1. If it is open, claim it and plan: propose 5-15
     tasks, each roughly 15-60 minutes of work for one worker, independent where possible, with deps
     only where truly needed; then task_complete T1 (no commit) and claim one of your tasks. If another
     worker holds T1, end your turn; you will be woken when the tasks appear.
3. Act in your workspace (your own clone). Keep changes scoped to your task. Commit in small, coherent
   commits.
4. Verify before merging: run the checks relevant to your change. Wrap heavy commands (dependency
   install, build, typecheck, full test runs) as `colony gate -- <command>`; it queues them so the
   machine is not overloaded.
5. Merge: `git pull --rebase origin {BRANCH}`, re-run checks if the rebase brought changes near yours,
   then `git push origin HEAD:{BRANCH}`. A rejected push means someone merged first: pull --rebase and
   push again. Never force-push. On a rebase conflict, edit the files to keep both sides' intent, then
   `git add <files>` and `git rebase --continue` (no editor opens; GIT_EDITOR is already set).
6. task_complete with a summary and the pushed commit (`git rev-parse HEAD` after a successful push).
   It is refused unless that commit is on {BRANCH}.
7. Share: note_add for findings others can reuse (conventions, commands, gotchas, where things live).
   chat_post for coordination; write @w07 to address a worker, @all for everyone.

## Rules
- Commands run unattended under an allow list that matches on the command's first word. Never prefix a
  command with environment assignments (`FOO=1 git ...`) or wrap it in `sh -c`/`env`; such commands are
  denied. A denied command stays denied for the whole session, so find another way instead of retrying.
- Hold at most one claim. If you are blocked or the task is wrong, task_release with the reason
  (task_progress logs partial progress), then move on.
- {BRANCH} must stay green. If it is broken, fixing it outranks everything; say so in chat.
- Do not redo others' work: read the board and notes first; when tasks overlap, settle it in chat.
- If a done task is broken or incomplete, task_reopen it with evidence. That is how the colony verifies.
- There is no human to ask and no manager to wait for. Decide, and record your reasoning in the task log.
- Do not spawn subagents.
- When every task is done and you have verified the whole goal on {BRANCH}, call goal_complete.
  It is refused while tasks are open or claimed.
{verify}
- Write chat, notes and task text in the language of the goal.
- End your turn when you finish a task or have to wait. The harness wakes you with board changes;
  never sleep or poll in a loop.
"#,
        n = v.workers,
    )
}

pub fn kickoff(agent: &str, goal: &str, time_left: Duration, setup_note: Option<&str>) -> String {
    let mut s = format!(
        "You are colony worker {agent}. Time budget left: {}.\n\nGOAL:\n{goal}\n\n\
         Your current directory is your own clone; `origin` is the hub, integration branch `{BRANCH}`.\n",
        fmt_duration(time_left)
    );
    if let Some(note) = setup_note {
        s.push_str(&format!("\nNote: {note}\n"));
    }
    s.push_str(
        "\nWrite every task title, chat message and note in the same language as the GOAL text above.\
         \nStart with board_view.",
    );
    s
}

/// The next message for a worker whose turn ended, with a short digest of what changed on the board.
pub fn nudge(board: &Board, agent: &str, seen_tasks: usize, time_left: Duration, reason: Nudge) -> String {
    let mut s = String::from("[colony] ");
    s.push_str(match reason {
        Nudge::Continue => "Turn ended; continue the loop.",
        Nudge::Woken => "The board changed while you were idle.",
        Nudge::IdleCheck => {
            "Still nothing claimable. Help verify: pull the branch, run the checks, review recently done \
             tasks (task_reopen if broken), or propose missing work. If the goal is truly met, goal_complete."
        }
        Nudge::AfterLimit => "Your last turn hit a usage limit and was paused. Resume where you left off.",
        Nudge::Restarted => "Your process was restarted. Re-orient with board_view, then continue.",
    });
    s.push_str(&format!(" Time left: {}.", fmt_duration(time_left)));
    if time_left < Duration::from_secs(15 * 60) {
        s.push_str(
            " WRAP UP: finish and push only working changes, or task_release with notes on what remains.",
        );
    }
    s.push('\n');
    match board.claim_of(agent) {
        Some(t) => s.push_str(&format!("You hold {} \"{}\".\n", t.id, clip(&t.title, 80))),
        None => s.push_str("You hold no task.\n"),
    }
    let fresh: Vec<String> = board
        .tasks()
        .iter()
        .skip(seen_tasks)
        .map(|t| format!("{} \"{}\"", t.id, clip(&t.title, 60)))
        .collect();
    if !fresh.is_empty() {
        s.push_str(&format!("New tasks: {}\n", fresh.join(", ")));
    }
    let claimable = board.claimable().len();
    if claimable > 0 {
        s.push_str(&format!("Claimable now: {claimable}\n"));
    }
    let unread = board.unread(agent);
    if unread.count > 0 {
        s.push_str(&format!("Unread chat: {}\n", unread.count));
        for m in unread.mentions.iter().rev().take(3) {
            s.push_str(&format!("  {} → you: {}\n", m.from, clip(&m.text, 160)));
        }
    }
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nudge {
    Continue,
    Woken,
    IdleCheck,
    AfterLimit,
    Restarted,
}

pub fn fmt_duration(d: Duration) -> String {
    let m = d.as_secs() / 60;
    if m >= 60 {
        format!("{}h{:02}m", m / 60, m % 60)
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::NewTask;

    #[test]
    fn nudge_lists_new_tasks_and_mentions() {
        let mut b = Board::new("g");
        b.propose(
            "w1",
            vec![NewTask {
                title: "alpha work".into(),
                ..Default::default()
            }],
            false,
            0,
        )
        .unwrap();
        b.post("w1", "@w2 can you take T1?", 0).unwrap();
        let n = nudge(&b, "w2", 0, Duration::from_secs(3 * 3600), Nudge::Woken);
        assert!(n.contains("New tasks: T1 \"alpha work\""), "{n}");
        assert!(n.contains("w1 → you: @w2 can you take T1?"), "{n}");
        assert!(n.contains("Time left: 3h00m"), "{n}");
        assert!(!n.contains("WRAP UP"), "{n}");
        let late = nudge(&b, "w2", 1, Duration::from_secs(600), Nudge::Continue);
        assert!(late.contains("WRAP UP") && !late.contains("New tasks"), "{late}");
    }

    #[test]
    fn system_prompt_mentions_verify_command() {
        let p = system_prompt(&ProtocolVars {
            workers: 4,
            verify: Some("pnpm test"),
        });
        assert!(p.contains("4 Claude Code workers"));
        assert!(p.contains("`pnpm test`"));
    }
}
