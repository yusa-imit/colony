//! Text sent to workers: the system prompt (protocol and goal), the first message, nudges between turns, and
//! mid-turn inbox deliveries.
//!
//! The protocol follows Agensh's collaboration loop (gather context → claim → act → verify → merge →
//! share). There is no director, so nothing says "who does what" — only how to read the board and choose.

use std::time::Duration;

use crate::board::{clip, Board, Inbox};
use crate::hub::BRANCH;

pub struct ProtocolVars<'a> {
    pub workers: usize,
    pub verify: Option<&'a str>,
    /// The goal goes into the system prompt, which is identical for every worker and every fresh session, so
    /// it is cached once. Repeating it in each `board_view` cost about 20% of a real run.
    pub goal: &'a str,
}

pub fn system_prompt(v: &ProtocolVars) -> String {
    let (check_main, verify) = match v.verify {
        Some(cmd) => (
            "To check colony-main as a whole, call `verify` instead of running the full chain\n   \
             yourself: it runs on the hub's latest commit and caches the result per commit for everyone."
                .to_string(),
            format!(
                "- goal_complete runs `{cmd}` on {BRANCH} and refuses if it fails. Run `verify` first (it is the \
                 same command)."
            ),
        ),
        None => (
            "No verify command is configured for this run, so check colony-main yourself when it matters."
                .to_string(),
            "- Before goal_complete, check the whole goal end to end on a fresh pull of the branch.".into(),
        ),
    };
    format!(
        r#"# Colony protocol

You are one worker in a colony: {n} Claude Code workers sharing one goal with no manager. Nobody assigns
you work. You find it, claim it, do it, verify it, merge it, and tell the others. The colony tools
(mcp__colony__*) are the shared board, chat and notes; git `origin` is the shared hub. The goal is at the end
of this prompt.

## Loop
1. Gather context: board_view. chat_read when there are unread messages. note_search before exploring
   an area someone may already have mapped.
2. Pick work:
   - If you hold a claim, continue it.
   - Otherwise claim the most valuable claimable task with task_claim, giving a short plan. Claims are
     atomic; if yours is refused, pick another.
   - If the task you are best placed to do still waits on deps, task_claim reserves it: it becomes yours
     automatically when its deps are done (if you hold no other task then) and you are woken. Prepare
     locally if that helps, but push only once it is yours. One reservation at a time.
   - If nothing is claimable but the goal is not met, propose the missing tasks (task_propose), then
     claim one. Any worker may propose work; the plan is not only the planner's.
   - The board starts with one planning task, T1. If it is open, claim it and plan: propose 5-15
     tasks, each roughly 15-60 minutes of work for one worker, independent where possible, with deps
     only where truly needed. Keep the critical path short: make any shared skeleton task as small as
     possible, and split wrap-up work (docs, final checks) so the parts that do not need everything can
     start early. Put helpers that several tasks need into one early task and make those tasks depend on
     it, instead of splitting by name alone. State each task's acceptance check in its detail. Then
     task_complete T1 (no commit) and end your turn. If another worker holds T1, end your turn; you will
     be woken when the tasks appear.
3. Act in your workspace (your own clone). Keep changes scoped to your task. Commit in small, coherent
   commits.
4. Verify before merging: run the checks relevant to your change. Wrap heavy commands (dependency
   install, build, typecheck, full test runs) as `colony gate -- <command>`; it queues them so the
   machine is not overloaded. {check_main}
5. Merge: `git pull --rebase origin {BRANCH}`, re-run checks if the rebase brought changes near yours,
   then `git push origin HEAD:{BRANCH}`. A rejected push means someone merged first: pull --rebase and
   push again. Never force-push. On a rebase conflict, edit the files to keep both sides' intent, then
   `git add <files>` and `git rebase --continue` (no editor opens; GIT_EDITOR is already set).
6. Share: note_add for findings others can reuse (conventions, commands, gotchas, where things live,
   approaches that failed). chat_post for coordination: write @w07 to reach one worker, even mid-task;
   @all is an announcement the others read at their next turn, and it wakes nobody.
7. task_complete with a summary and the pushed commit (`git rev-parse HEAD` after a successful push).
   It is refused unless that commit is on {BRANCH}. Then end your turn.

## Rules
- One task per turn: after task_complete, end your turn. Your next turn starts with a fresh board digest,
  often in a fresh session, so whatever is worth keeping belongs in a note, the task log or git.
- Commands run unattended under an allow list that matches on the command's first word. Never prefix a
  command with environment assignments (`FOO=1 git ...`) or wrap it in `sh -c`/`env`; such commands are
  denied. A denied command stays denied for the whole session, so find another way instead of retrying.
- Create and change files with the Write/Edit tools, not shell redirection or heredocs (`>`,
  `cat > f <<EOF` are denied). Delete files with `git rm` or `git clean -f <path>`. Writes outside your
  clone are denied: put scratch files (probes, generated inputs) in `.colony-tmp/` inside your clone,
  which git ignores.
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
- Messages addressed to you and new notes can appear after a tool result while you work. Act on them when
  they change your task.
- End your turn when you finish a task or have to wait. The harness wakes you with board changes;
  never sleep or poll in a loop.

# Goal

{goal}
"#,
        n = v.workers,
        goal = v.goal.trim(),
    )
}

/// The first message of a session. `fresh` marks a session started after the previous one finished its task.
pub fn kickoff(agent: &str, time_left: Duration, setup_note: Option<&str>, fresh: bool) -> String {
    let mut s = format!(
        "You are colony worker {agent}. Time budget left: {}.\n",
        fmt_duration(time_left)
    );
    if fresh {
        s.push_str(
            "This is a fresh session: your previous one ended with its task. The board, notes, task logs \
             and git hold everything the colony knows; nothing else carried over.\n",
        );
    }
    s.push_str(&format!(
        "The goal is in your system prompt. Your current directory is your own clone; `origin` is the hub, \
         integration branch `{BRANCH}`.\n"
    ));
    if let Some(note) = setup_note {
        s.push_str(&format!("\nNote: {note}\n"));
    }
    s.push_str(
        "\nWrite every task title, chat message and note in the same language as the goal.\
         \nStart with board_view.",
    );
    s
}

/// The next message for a worker whose turn ended, with a short digest of what changed on the board.
/// `inbox` is what the worker has not been shown yet (taken from the board by the caller).
pub fn nudge(
    board: &Board,
    agent: &str,
    seen_tasks: usize,
    time_left: Duration,
    reason: Nudge,
    inbox: &Inbox,
) -> String {
    let mut s = String::from("[colony] ");
    s.push_str(match reason {
        Nudge::Continue => "Turn ended; continue the loop.",
        Nudge::Woken => "The board changed while you were idle.",
        Nudge::IdleCheck => {
            "Nothing has been claimable for a while. If you can see work the goal still needs, propose it. \
             Otherwise end your turn; do not re-run checks others already ran (`verify` caches results per \
             commit)."
        }
        Nudge::AllDone => {
            "Every task on the board is done. Check the whole goal on colony-main (`verify` if configured), \
             then call goal_complete. If something is missing, propose it instead."
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
    if let Some(t) = board.reservation_of(agent) {
        s.push_str(&format!(
            "You reserve {} \"{}\"; it becomes yours when its deps are done.\n",
            t.id,
            clip(&t.title, 80)
        ));
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
    for line in inbox_lines(inbox) {
        s.push_str(&line);
        s.push('\n');
    }
    let unread = board.unread(agent);
    if unread.count > 0 {
        s.push_str(&format!("Unread chat: {}\n", unread.count));
        let announcements = unread
            .mentions
            .iter()
            .filter(|m| !m.mentions.iter().any(|x| x == agent))
            .rev()
            .take(3);
        for m in announcements {
            s.push_str(&format!("  {} → all: {}\n", m.from, clip(&m.text, 160)));
        }
    }
    s
}

/// Text for a mid-turn delivery, or `None` when there is nothing to deliver.
pub fn render_inbox(inbox: &Inbox) -> Option<String> {
    if inbox.is_empty() {
        return None;
    }
    let mut s = String::from("[colony] While you were working:\n");
    for line in inbox_lines(inbox) {
        s.push_str(&line);
        s.push('\n');
    }
    Some(s)
}

fn inbox_lines(inbox: &Inbox) -> Vec<String> {
    let mut out: Vec<String> = inbox
        .mentions
        .iter()
        .map(|m| format!("  {} → you: {}", m.from, clip(&m.text, 300)))
        .collect();
    out.extend(inbox.notes.iter().map(|n| {
        format!(
            "  Note {} by {}: {} — {}",
            n.id,
            n.by,
            clip(&n.title, 80),
            clip(&n.body, 200)
        )
    }));
    if inbox.older_notes > 0 {
        out.push(format!("  (+{} older notes: note_search)", inbox.older_notes));
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nudge {
    Continue,
    Woken,
    IdleCheck,
    /// Every task is done; this worker is the one asked to check the goal and close it.
    AllDone,
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
    use crate::board::{Inbox, NewTask};

    fn alpha_board() -> Board {
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
        b
    }

    #[test]
    fn nudge_lists_new_tasks_mentions_and_announcements() {
        let mut b = alpha_board();
        b.post("w1", "@w2 can you take T1?", 0).unwrap();
        b.post("w3", "@all T9 merged", 0).unwrap();
        let inbox = b.take_inbox("w2");
        let n = nudge(&b, "w2", 0, Duration::from_secs(3 * 3600), Nudge::Woken, &inbox);
        assert!(n.contains("New tasks: T1 \"alpha work\""), "{n}");
        assert!(n.contains("w1 → you: @w2 can you take T1?"), "{n}");
        assert!(n.contains("w3 → all: @all T9 merged"), "{n}");
        assert!(n.contains("Time left: 3h00m"), "{n}");
        assert!(!n.contains("WRAP UP"), "{n}");
        let late = nudge(
            &b,
            "w2",
            1,
            Duration::from_secs(600),
            Nudge::Continue,
            &Inbox::default(),
        );
        assert!(late.contains("WRAP UP") && !late.contains("New tasks"), "{late}");
        assert!(
            !late.contains("can you take T1"),
            "a delivered mention is not repeated: {late}"
        );
    }

    #[test]
    fn nudge_shows_reservation_and_all_done() {
        let mut b = alpha_board();
        b.propose(
            "w1",
            vec![NewTask {
                title: "docs".into(),
                deps: vec!["T1".into()],
                ..Default::default()
            }],
            false,
            0,
        )
        .unwrap();
        b.claim("w2", "T2", "", 0).unwrap();
        let n = nudge(
            &b,
            "w2",
            2,
            Duration::from_secs(3600),
            Nudge::Continue,
            &Inbox::default(),
        );
        assert!(n.contains("You reserve T2 \"docs\""), "{n}");
        let done = nudge(
            &b,
            "w2",
            2,
            Duration::from_secs(3600),
            Nudge::AllDone,
            &Inbox::default(),
        );
        assert!(done.contains("goal_complete"), "{done}");
    }

    #[test]
    fn renders_inbox() {
        let mut b = alpha_board();
        assert!(render_inbox(&b.take_inbox("w2")).is_none());
        b.post("w1", "@w2 leave the shared helper to me", 0).unwrap();
        b.add_note("w3", "samples", "shared inputs live in fixtures.rs", vec![], 0)
            .unwrap();
        let text = render_inbox(&b.take_inbox("w2")).unwrap();
        assert!(
            text.contains("w1 → you: @w2 leave the shared helper to me"),
            "{text}"
        );
        assert!(
            text.contains("N1 by w3: samples — shared inputs live in fixtures.rs"),
            "{text}"
        );
    }

    fn vars(goal: &str) -> ProtocolVars<'_> {
        ProtocolVars {
            workers: 4,
            verify: Some("pnpm test"),
            goal,
        }
    }

    #[test]
    fn system_prompt_carries_the_goal() {
        let p = system_prompt(&vars("# Ship the package\n\nevery detail"));
        assert!(p.contains("# Goal\n\n# Ship the package\n\nevery detail"), "{p}");
        let k = kickoff("w1", Duration::from_secs(3600), None, false);
        assert!(!k.contains("every detail") && k.contains("system prompt"), "{k}");
    }

    #[test]
    fn system_prompt_mentions_verify_and_planning_rules() {
        let p = system_prompt(&vars("g"));
        assert!(p.contains("4 Claude Code workers"));
        assert!(p.contains("`pnpm test`"));
        for rule in [
            "One task per turn",
            "critical path",
            "reserves it",
            ".colony-tmp/",
            "Write/Edit",
            "verify",
            "@all",
        ] {
            assert!(p.contains(rule), "missing `{rule}`");
        }
        let fresh = kickoff("w1", Duration::from_secs(3600), None, true);
        assert!(fresh.contains("fresh session"), "{fresh}");
    }
}
