//! The shared board: tasks, chat, notes and the goal. A pure state machine with no IO.
//!
//! Callers pass time (`now`, unix ms) and git verdicts (whether a commit is merged) in as values, so every
//! rule can be pinned down by unit tests without processes or a network.

use std::collections::{HashMap, HashSet};
use std::fmt::{self, Write as _};

use serde::Serialize;

/// How many tasks one agent may hold at once. Prevents hoarding claims.
pub const MAX_CLAIMS_PER_AGENT: usize = 1;
/// A proposal whose title-token Jaccard similarity reaches this value is treated as a duplicate.
const DUPLICATE_THRESHOLD: f64 = 0.8;
pub const PLAN_TITLE: &str = "Plan: break the goal into tasks";
const PLAN_DETAIL: &str =
    "Explore the repository as much as you need, then task_propose the tasks that achieve \
the goal (deps only where truly needed), then task_complete this task without a commit. Exactly one worker \
plans; everyone else waits until the tasks appear.";
/// Maximum number of messages one `chat_read` returns.
const CHAT_PAGE: usize = 50;
/// Number of done tasks the board view lists in full; older ones are only counted.
const VIEW_DONE_TAIL: usize = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Open,
    Claimed,
    Done,
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TaskStatus::Open => "open",
            TaskStatus::Claimed => "claimed",
            TaskStatus::Done => "done",
        })
    }
}

/// One entry in a task's log (plan, progress, release, reopen, done).
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub at: i64,
    pub by: String,
    pub kind: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub detail: String,
    pub status: TaskStatus,
    pub proposed_by: String,
    pub claimed_by: Option<String>,
    pub deps: Vec<String>,
    pub log: Vec<Entry>,
    pub commit: Option<String>,
    pub summary: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Proposal input. `deps` names existing task ids (`T3`) or earlier items of the same batch (`#1`, 1-based).
#[derive(Debug, Clone, Default)]
pub struct NewTask {
    pub title: String,
    pub detail: String,
    pub deps: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMsg {
    pub seq: u64,
    pub at: i64,
    pub from: String,
    pub text: String,
    /// Mentioned agent ids. `@all` is stored as `"all"`.
    pub mentions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub id: String,
    pub at: i64,
    pub by: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    Active,
    Complete,
    Stopped,
}

#[derive(Debug, Clone, Serialize)]
pub struct Goal {
    pub text: String,
    pub status: GoalStatus,
    pub summary: Option<String>,
    pub closed_by: Option<String>,
}

/// The commit passed to `task_complete`, with the merge status the caller checked on the hub.
#[derive(Debug, Clone)]
pub struct CommitCheck {
    pub sha: String,
    pub merged: bool,
}

/// Refusal reasons shown verbatim to agents. Each one also says what to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardError {
    GoalClosed(GoalStatus),
    Empty(&'static str),
    UnknownTask(String),
    UnknownDeps(Vec<String>),
    NotOpen {
        id: String,
        status: TaskStatus,
        owner: Option<String>,
    },
    DepsPending {
        id: String,
        pending: Vec<String>,
    },
    AlreadyClaiming {
        held: String,
    },
    NotYours {
        id: String,
        owner: Option<String>,
    },
    NotDone(String),
    NotMerged {
        sha: String,
    },
    Duplicate {
        title: String,
        similar: Vec<(String, String)>,
    },
    OpenWork {
        open: Vec<String>,
        claimed: Vec<String>,
    },
    NothingDone,
}

impl fmt::Display for BoardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BoardError::GoalClosed(s) => write!(f, "the goal is {s:?}; no more board changes are accepted"),
            BoardError::Empty(field) => write!(f, "`{field}` must not be empty"),
            BoardError::UnknownTask(id) => write!(f, "no task {id}; call board_view for current ids"),
            BoardError::UnknownDeps(ids) => write!(
                f,
                "unknown deps {}: use existing ids (T3) or earlier items in this batch (#1)",
                ids.join(", ")
            ),
            BoardError::NotOpen { id, status, owner } => match owner {
                Some(o) => write!(f, "{id} is {status} by {o}; pick another task"),
                None => write!(f, "{id} is {status}; pick another task"),
            },
            BoardError::DepsPending { id, pending } => {
                write!(f, "{id} waits on {} which are not done yet", pending.join(", "))
            }
            BoardError::AlreadyClaiming { held } => write!(
                f,
                "you already hold {held}; finish it (task_complete) or give it back (task_release) first"
            ),
            BoardError::NotYours { id, owner } => match owner {
                Some(o) => write!(f, "{id} is claimed by {o}, not you"),
                None => write!(f, "{id} is not claimed by you"),
            },
            BoardError::NotDone(id) => write!(f, "{id} is not done, so it cannot be reopened"),
            BoardError::NotMerged { sha } => write!(
                f,
                "commit {sha} is not on colony-main. Push first: git pull --rebase origin colony-main && \
                 git push origin HEAD:colony-main, then pass the pushed `git rev-parse HEAD`"
            ),
            BoardError::Duplicate { title, similar } => {
                write!(f, "\"{title}\" looks like existing work:")?;
                for (id, t) in similar {
                    write!(f, " {id} \"{t}\";")?;
                }
                write!(f, " claim or extend that instead, or resend with force=true if it is really different")
            }
            BoardError::OpenWork { open, claimed } => write!(
                f,
                "work is still outstanding (open: {}; claimed: {}); finish or release it before goal_complete",
                list_or_none(open),
                list_or_none(claimed)
            ),
            BoardError::NothingDone => {
                write!(f, "no task is done yet; the goal cannot be complete")
            }
        }
    }
}

impl std::error::Error for BoardError {}

fn list_or_none(ids: &[String]) -> String {
    if ids.is_empty() {
        "none".into()
    } else {
        ids.join(", ")
    }
}

/// Unread-chat summary; the supervisor uses it to build the next turn's message.
#[derive(Debug, Clone, Default)]
pub struct Unread {
    pub count: usize,
    pub mentions: Vec<ChatMsg>,
}

#[derive(Debug)]
pub struct Board {
    goal: Goal,
    tasks: Vec<Task>,
    chat: Vec<ChatMsg>,
    notes: Vec<Note>,
    /// Last chat seq each agent has read.
    cursors: HashMap<String, u64>,
}

impl Board {
    pub fn new(goal: impl Into<String>) -> Self {
        Board {
            goal: Goal {
                text: goal.into(),
                status: GoalStatus::Active,
                summary: None,
                closed_by: None,
            },
            tasks: Vec::new(),
            chat: Vec::new(),
            notes: Vec::new(),
            cursors: HashMap::new(),
        }
    }

    /// A board seeded with planning task T1. Without it, workers that saw an empty board at the same time
    /// each decomposed the goal and produced two copies of the plan (observed in real runs). Claims are
    /// atomic, so exactly one worker plans.
    pub fn seeded(goal: impl Into<String>) -> Self {
        let mut b = Board::new(goal);
        let plan = NewTask {
            title: PLAN_TITLE.into(),
            detail: PLAN_DETAIL.into(),
            deps: Vec::new(),
        };
        b.propose("colony", vec![plan], true, 0)
            .expect("an empty active board accepts the plan task");
        b
    }

    pub fn goal(&self) -> &Goal {
        &self.goal
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn task(&self, id: &str) -> Option<&Task> {
        self.index(id).map(|i| &self.tasks[i])
    }

    pub fn claim_of(&self, agent: &str) -> Option<&Task> {
        self.tasks
            .iter()
            .find(|t| t.status == TaskStatus::Claimed && t.claimed_by.as_deref() == Some(agent))
    }

    /// Tasks that can be claimed right now: open, with every dependency done.
    pub fn claimable(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Open && self.pending_deps(t).is_empty())
            .collect()
    }

    pub fn propose(
        &mut self,
        by: &str,
        items: Vec<NewTask>,
        force: bool,
        now: i64,
    ) -> Result<Vec<String>, BoardError> {
        self.ensure_active()?;
        if items.is_empty() {
            return Err(BoardError::Empty("tasks"));
        }
        let base = self.tasks.len();
        let mut resolved: Vec<Vec<String>> = Vec::with_capacity(items.len());
        let mut unknown = Vec::new();
        for (i, item) in items.iter().enumerate() {
            if item.title.trim().is_empty() {
                return Err(BoardError::Empty("title"));
            }
            let mut deps = Vec::new();
            for d in &item.deps {
                let d = d.trim();
                if let Some(n) = d.strip_prefix('#').and_then(|n| n.parse::<usize>().ok()) {
                    // Only earlier items can be referenced, so cycles are impossible.
                    if n >= 1 && n <= i {
                        deps.push(task_id(base + n));
                        continue;
                    }
                } else if self.index(d).is_some() {
                    deps.push(d.to_string());
                    continue;
                } else if let Some(j) = items.iter().take(i).position(|x| x.title.trim() == d) {
                    // Models often name dependencies by title instead of id (observed in real runs), so
                    // titles of earlier batch items and existing tasks are accepted too.
                    deps.push(task_id(base + j + 1));
                    continue;
                } else if let Some(t) = self.tasks.iter().find(|t| t.title == d) {
                    deps.push(t.id.clone());
                    continue;
                }
                unknown.push(d.to_string());
            }
            resolved.push(deps);
        }
        if !unknown.is_empty() {
            return Err(BoardError::UnknownDeps(unknown));
        }
        // Items in the same batch are not compared with each other. One worker's plan shares title
        // boilerplate ("… implement it and pub use from lib.rs"), and a valid batch was rejected on that
        // alone (observed in real runs).
        if !force {
            for item in &items {
                let similar: Vec<(String, String)> = self
                    .tasks
                    .iter()
                    .filter(|t| {
                        t.status != TaskStatus::Done
                            && similarity(&t.title, &item.title) >= DUPLICATE_THRESHOLD
                    })
                    .map(|t| (t.id.clone(), t.title.clone()))
                    .collect();
                if !similar.is_empty() {
                    return Err(BoardError::Duplicate {
                        title: item.title.clone(),
                        similar,
                    });
                }
            }
        }
        let mut ids = Vec::with_capacity(items.len());
        for (item, deps) in items.into_iter().zip(resolved) {
            let id = task_id(self.tasks.len() + 1);
            self.tasks.push(Task {
                id: id.clone(),
                title: item.title.trim().to_string(),
                detail: item.detail.trim().to_string(),
                status: TaskStatus::Open,
                proposed_by: by.to_string(),
                claimed_by: None,
                deps,
                log: Vec::new(),
                commit: None,
                summary: None,
                created_at: now,
                updated_at: now,
            });
            ids.push(id);
        }
        Ok(ids)
    }

    pub fn claim(&mut self, by: &str, id: &str, plan: &str, now: i64) -> Result<(), BoardError> {
        self.ensure_active()?;
        let i = self
            .index(id)
            .ok_or_else(|| BoardError::UnknownTask(id.to_string()))?;
        let held: Vec<&Task> = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Claimed && t.claimed_by.as_deref() == Some(by))
            .collect();
        if held.len() >= MAX_CLAIMS_PER_AGENT {
            return Err(BoardError::AlreadyClaiming {
                held: held[0].id.clone(),
            });
        }
        let task = &self.tasks[i];
        if task.status != TaskStatus::Open {
            return Err(BoardError::NotOpen {
                id: id.to_string(),
                status: task.status,
                owner: task.claimed_by.clone(),
            });
        }
        let pending = self.pending_deps(task);
        if !pending.is_empty() {
            return Err(BoardError::DepsPending {
                id: id.to_string(),
                pending,
            });
        }
        let task = &mut self.tasks[i];
        task.status = TaskStatus::Claimed;
        task.claimed_by = Some(by.to_string());
        task.updated_at = now;
        task.log.push(Entry {
            at: now,
            by: by.to_string(),
            kind: "plan",
            text: plan.trim().to_string(),
        });
        Ok(())
    }

    pub fn progress(&mut self, by: &str, id: &str, text: &str, now: i64) -> Result<(), BoardError> {
        self.ensure_active()?;
        let i = self.owned_index(by, id)?;
        let task = &mut self.tasks[i];
        task.updated_at = now;
        task.log.push(Entry {
            at: now,
            by: by.to_string(),
            kind: "progress",
            text: text.trim().to_string(),
        });
        Ok(())
    }

    pub fn release(&mut self, by: &str, id: &str, reason: &str, now: i64) -> Result<(), BoardError> {
        self.ensure_active()?;
        let i = self.owned_index(by, id)?;
        let task = &mut self.tasks[i];
        task.status = TaskStatus::Open;
        task.claimed_by = None;
        task.updated_at = now;
        task.log.push(Entry {
            at: now,
            by: by.to_string(),
            kind: "release",
            text: reason.trim().to_string(),
        });
        Ok(())
    }

    pub fn complete(
        &mut self,
        by: &str,
        id: &str,
        summary: &str,
        commit: Option<CommitCheck>,
        now: i64,
    ) -> Result<(), BoardError> {
        self.ensure_active()?;
        if summary.trim().is_empty() {
            return Err(BoardError::Empty("summary"));
        }
        let i = self.owned_index(by, id)?;
        if let Some(c) = &commit {
            if !c.merged {
                return Err(BoardError::NotMerged { sha: c.sha.clone() });
            }
        }
        let task = &mut self.tasks[i];
        task.status = TaskStatus::Done;
        task.commit = commit.map(|c| c.sha);
        task.summary = Some(summary.trim().to_string());
        task.updated_at = now;
        task.log.push(Entry {
            at: now,
            by: by.to_string(),
            kind: "done",
            text: summary.trim().to_string(),
        });
        Ok(())
    }

    /// Reopens another agent's done task that is broken or incomplete. This is the peer-review path.
    pub fn reopen(&mut self, by: &str, id: &str, reason: &str, now: i64) -> Result<(), BoardError> {
        self.ensure_active()?;
        if reason.trim().is_empty() {
            return Err(BoardError::Empty("reason"));
        }
        let i = self
            .index(id)
            .ok_or_else(|| BoardError::UnknownTask(id.to_string()))?;
        let task = &mut self.tasks[i];
        if task.status != TaskStatus::Done {
            return Err(BoardError::NotDone(id.to_string()));
        }
        task.status = TaskStatus::Open;
        task.claimed_by = None;
        task.updated_at = now;
        task.log.push(Entry {
            at: now,
            by: by.to_string(),
            kind: "reopen",
            text: reason.trim().to_string(),
        });
        Ok(())
    }

    pub fn post(&mut self, from: &str, text: &str, now: i64) -> Result<ChatMsg, BoardError> {
        if text.trim().is_empty() {
            return Err(BoardError::Empty("text"));
        }
        let msg = ChatMsg {
            seq: self.chat.len() as u64 + 1,
            at: now,
            from: from.to_string(),
            text: text.trim().to_string(),
            mentions: parse_mentions(text),
        };
        self.chat.push(msg.clone());
        // An agent's own message counts as read.
        self.cursors.insert(from.to_string(), msg.seq);
        Ok(msg)
    }

    /// Returns messages after the cursor and moves the cursor to the end. When there are too many, returns
    /// the most recent ones and reports how many were skipped.
    pub fn read_chat(&mut self, agent: &str) -> (Vec<ChatMsg>, usize) {
        let cursor = self.cursors.get(agent).copied().unwrap_or(0);
        let unread: Vec<&ChatMsg> = self.chat.iter().filter(|m| m.seq > cursor).collect();
        let skipped = unread.len().saturating_sub(CHAT_PAGE);
        let page = unread.into_iter().skip(skipped).cloned().collect();
        self.cursors.insert(agent.to_string(), self.chat.len() as u64);
        (page, skipped)
    }

    /// Counts unread messages and collects those mentioning the agent, without moving the cursor.
    pub fn unread(&self, agent: &str) -> Unread {
        let cursor = self.cursors.get(agent).copied().unwrap_or(0);
        let mut out = Unread::default();
        for m in self.chat.iter().filter(|m| m.seq > cursor && m.from != agent) {
            out.count += 1;
            if m.mentions.iter().any(|x| x == agent || x == "all") {
                out.mentions.push(m.clone());
            }
        }
        out
    }

    pub fn add_note(
        &mut self,
        by: &str,
        title: &str,
        body: &str,
        tags: Vec<String>,
        now: i64,
    ) -> Result<String, BoardError> {
        if title.trim().is_empty() {
            return Err(BoardError::Empty("title"));
        }
        let id = format!("N{}", self.notes.len() + 1);
        self.notes.push(Note {
            id: id.clone(),
            at: now,
            by: by.to_string(),
            title: title.trim().to_string(),
            body: body.trim().to_string(),
            tags: tags
                .into_iter()
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
                .collect(),
        });
        Ok(id)
    }

    /// Ranked by word overlap. An empty query returns the latest notes.
    pub fn search_notes(&self, query: &str, limit: usize) -> Vec<&Note> {
        let terms = tokens(query);
        if terms.is_empty() {
            return self.notes.iter().rev().take(limit).collect();
        }
        let mut scored: Vec<(usize, &Note)> = self
            .notes
            .iter()
            .filter_map(|n| {
                let hay = tokens(&format!("{} {} {}", n.title, n.body, n.tags.join(" ")));
                let hits = terms.iter().filter(|t| hay.contains(*t)).count();
                (hits > 0).then_some((hits, n))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.at.cmp(&a.1.at)));
        scored.into_iter().take(limit).map(|(_, n)| n).collect()
    }

    /// Only checks the preconditions for declaring the goal complete (no outstanding work, at least one
    /// done task). Split from `close_goal` so the cheap check runs before the verify command.
    pub fn check_closable(&self) -> Result<(), BoardError> {
        self.ensure_active()?;
        let open: Vec<String> = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Open)
            .map(|t| t.id.clone())
            .collect();
        let claimed: Vec<String> = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Claimed)
            .map(|t| t.id.clone())
            .collect();
        if !open.is_empty() || !claimed.is_empty() {
            return Err(BoardError::OpenWork { open, claimed });
        }
        if !self.tasks.iter().any(|t| t.status == TaskStatus::Done) {
            return Err(BoardError::NothingDone);
        }
        Ok(())
    }

    pub fn close_goal(&mut self, by: &str, summary: &str) -> Result<(), BoardError> {
        self.check_closable()?;
        self.goal.status = GoalStatus::Complete;
        self.goal.summary = Some(summary.trim().to_string());
        self.goal.closed_by = Some(by.to_string());
        Ok(())
    }

    /// Called when the harness stops on a deadline, budget or interrupt. A completed goal stays complete.
    pub fn stop(&mut self, reason: &str) {
        if self.goal.status == GoalStatus::Active {
            self.goal.status = GoalStatus::Stopped;
            self.goal.summary = Some(reason.to_string());
        }
    }

    /// The text agents get from `board_view`. Fewer tokens than JSON, and it reads as is.
    pub fn render_view(&self, agent: &str, hub_head: &str) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "GOAL [{:?}]: {}", self.goal.status, self.goal.text);
        let _ = writeln!(s, "HUB colony-main @ {}", short(hub_head));
        match self.claim_of(agent) {
            Some(t) => {
                let _ = writeln!(s, "YOU ({agent}) hold {} \"{}\"", t.id, t.title);
            }
            None => {
                let _ = writeln!(s, "YOU ({agent}) hold no task");
            }
        }
        let claimable = self.claimable();
        if !claimable.is_empty() {
            let list: Vec<String> = claimable
                .iter()
                .map(|t| format!("{} {}", t.id, t.title))
                .collect();
            let _ = writeln!(s, "CLAIMABLE NOW ({}): {}", claimable.len(), list.join(" | "));
        }
        let count = |st| self.tasks.iter().filter(|t| t.status == st).count();
        let done = count(TaskStatus::Done);
        let _ = writeln!(
            s,
            "TASKS open {} · claimed {} · done {}",
            count(TaskStatus::Open),
            count(TaskStatus::Claimed),
            done
        );
        let hidden_done = done.saturating_sub(VIEW_DONE_TAIL);
        if hidden_done > 0 {
            let _ = writeln!(
                s,
                "  ({hidden_done} older done tasks hidden; task_get <id> for any task)"
            );
        }
        let mut done_seen = 0;
        for t in &self.tasks {
            let tag = match t.status {
                TaskStatus::Open => {
                    let pending = self.pending_deps(t);
                    if pending.is_empty() {
                        "open".to_string()
                    } else {
                        format!("open, waits {}", pending.join(","))
                    }
                }
                TaskStatus::Claimed => format!("claimed {}", t.claimed_by.as_deref().unwrap_or("?")),
                TaskStatus::Done => {
                    done_seen += 1;
                    if done_seen <= hidden_done {
                        continue;
                    }
                    match &t.commit {
                        Some(c) => format!("done {} {}", t.claimed_by.as_deref().unwrap_or("?"), short(c)),
                        None => format!("done {}", t.claimed_by.as_deref().unwrap_or("?")),
                    }
                }
            };
            let _ = writeln!(s, "  {} [{}] {}", t.id, tag, t.title);
        }
        let unread = self.unread(agent);
        if unread.count > 0 {
            let _ = writeln!(s, "CHAT {} unread (chat_read to see them)", unread.count);
            for m in unread.mentions.iter().rev().take(5) {
                let _ = writeln!(s, "  #{} {} → you: {}", m.seq, m.from, clip(&m.text, 200));
            }
        }
        if !self.notes.is_empty() {
            let latest: Vec<String> = self
                .notes
                .iter()
                .rev()
                .take(8)
                .map(|n| format!("{} {}", n.id, n.title))
                .collect();
            let _ = writeln!(
                s,
                "NOTES {} (note_search to read): {}",
                self.notes.len(),
                latest.join(" | ")
            );
        }
        s
    }

    pub fn render_task(&self, id: &str) -> Result<String, BoardError> {
        let t = self
            .task(id)
            .ok_or_else(|| BoardError::UnknownTask(id.to_string()))?;
        let mut s = String::new();
        let _ = writeln!(s, "{} [{}] {}", t.id, t.status, t.title);
        if let Some(o) = &t.claimed_by {
            let _ = writeln!(s, "owner: {o}");
        }
        let _ = writeln!(s, "proposed by: {}", t.proposed_by);
        if !t.deps.is_empty() {
            let _ = writeln!(s, "deps: {}", t.deps.join(", "));
        }
        if let Some(c) = &t.commit {
            let _ = writeln!(s, "commit: {c}");
        }
        if !t.detail.is_empty() {
            let _ = writeln!(s, "\n{}", t.detail);
        }
        if !t.log.is_empty() {
            let _ = writeln!(s, "\nlog:");
            for e in &t.log {
                let _ = writeln!(s, "  [{}] {}: {}", e.kind, e.by, e.text);
            }
        }
        Ok(s)
    }

    /// Snapshot for the dashboard.
    pub fn snapshot(&self) -> serde_json::Value {
        let chat_tail = self.chat.len().saturating_sub(200);
        serde_json::json!({
            "goal": self.goal,
            "tasks": self.tasks,
            "chat": &self.chat[chat_tail..],
            "notes": self.notes,
        })
    }

    fn ensure_active(&self) -> Result<(), BoardError> {
        match self.goal.status {
            GoalStatus::Active => Ok(()),
            s => Err(BoardError::GoalClosed(s)),
        }
    }

    fn index(&self, id: &str) -> Option<usize> {
        let n: usize = id.trim().strip_prefix('T')?.parse().ok()?;
        (n >= 1 && n <= self.tasks.len()).then(|| n - 1)
    }

    fn owned_index(&self, by: &str, id: &str) -> Result<usize, BoardError> {
        let i = self
            .index(id)
            .ok_or_else(|| BoardError::UnknownTask(id.to_string()))?;
        let t = &self.tasks[i];
        if t.status != TaskStatus::Claimed || t.claimed_by.as_deref() != Some(by) {
            return Err(BoardError::NotYours {
                id: id.to_string(),
                owner: t.claimed_by.clone(),
            });
        }
        Ok(i)
    }

    fn pending_deps(&self, t: &Task) -> Vec<String> {
        t.deps
            .iter()
            .filter(|d| self.task(d).is_some_and(|x| x.status != TaskStatus::Done))
            .cloned()
            .collect()
    }
}

fn task_id(n: usize) -> String {
    format!("T{n}")
}

fn parse_mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | ':' | ';' | '(' | ')')) {
        if let Some(name) = word.strip_prefix('@') {
            let name = name.trim_end_matches(|c: char| !c.is_alphanumeric());
            let is_worker =
                name.len() > 1 && name.starts_with('w') && name[1..].chars().all(|c| c.is_ascii_digit());
            if (is_worker || name == "all") && !out.iter().any(|x| x == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Set of lowercase words. Hangul (and other CJK) characters are `is_alphanumeric`, so they form words as is.
/// Single-character words are dropped.
fn tokens(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 2)
        .collect()
}

fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (tokens(a), tokens(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(&b).count() as f64;
    let union = a.union(&b).count() as f64;
    inter / union
}

pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

pub fn clip(s: &str, max: usize) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if flat.chars().count() <= max {
        flat
    } else {
        let mut out: String = flat.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nt(title: &str) -> NewTask {
        NewTask {
            title: title.into(),
            ..Default::default()
        }
    }

    fn nt_deps(title: &str, deps: &[&str]) -> NewTask {
        NewTask {
            title: title.into(),
            deps: deps.iter().map(|d| d.to_string()).collect(),
            ..Default::default()
        }
    }

    fn merged(sha: &str) -> Option<CommitCheck> {
        Some(CommitCheck {
            sha: sha.into(),
            merged: true,
        })
    }

    #[test]
    fn propose_assigns_sequential_ids_and_resolves_batch_deps() {
        let mut b = Board::new("g");
        let ids = b
            .propose(
                "w1",
                vec![nt("set up router"), nt_deps("add login page", &["#1"])],
                false,
                0,
            )
            .unwrap();
        assert_eq!(ids, vec!["T1", "T2"]);
        assert_eq!(b.task("T2").unwrap().deps, vec!["T1"]);
        let more = b
            .propose("w2", vec![nt_deps("write e2e tests", &["T2"])], false, 0)
            .unwrap();
        assert_eq!(more, vec!["T3"]);
    }

    #[test]
    fn propose_rejects_forward_or_unknown_deps() {
        let mut b = Board::new("g");
        let err = b
            .propose("w1", vec![nt_deps("a task", &["#2"]), nt("b task")], false, 0)
            .unwrap_err();
        assert_eq!(err, BoardError::UnknownDeps(vec!["#2".into()]));
        let err = b
            .propose("w1", vec![nt_deps("a task", &["T9"])], false, 0)
            .unwrap_err();
        assert_eq!(err, BoardError::UnknownDeps(vec!["T9".into()]));
        assert!(
            b.tasks().is_empty(),
            "rejected batch must not leave partial tasks"
        );
    }

    #[test]
    fn propose_flags_duplicates_unless_forced() {
        let mut b = Board::new("g");
        b.propose("w1", vec![nt("add dark mode toggle to settings page")], false, 0)
            .unwrap();
        let err = b
            .propose(
                "w2",
                vec![nt("Add dark-mode toggle to the settings page")],
                false,
                0,
            )
            .unwrap_err();
        assert!(matches!(err, BoardError::Duplicate { .. }));
        assert!(b
            .propose(
                "w2",
                vec![nt("Add dark-mode toggle to the settings page")],
                true,
                0
            )
            .is_ok());
    }

    #[test]
    fn batch_with_shared_boilerplate_is_not_a_duplicate() {
        let mut b = Board::new("g");
        let ids = b
            .propose(
                "w1",
                vec![
                    nt("slugify を src/slug.rs に実装し lib.rs から pub use"),
                    nt("title_case を src/title.rs に実装し lib.rs から pub use"),
                ],
                false,
                0,
            )
            .unwrap();
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn deps_may_name_tasks_by_title() {
        let mut b = Board::new("g");
        b.propose("w1", vec![nt("schema")], false, 0).unwrap();
        b.propose(
            "w1",
            vec![nt("api"), nt_deps("docs", &["api", "schema"])],
            false,
            0,
        )
        .unwrap();
        assert_eq!(b.task("T3").unwrap().deps, vec!["T2", "T1"]);
    }

    #[test]
    fn seeded_board_has_one_claimable_plan_task() {
        let mut b = Board::seeded("g");
        assert_eq!(b.claimable().len(), 1);
        assert_eq!(b.task("T1").unwrap().title, PLAN_TITLE);
        b.claim("w1", "T1", "plan", 0).unwrap();
        assert!(
            b.claimable().is_empty(),
            "others see nothing to do while w1 plans"
        );
        b.propose("w1", vec![nt("alpha work")], false, 0).unwrap();
        b.complete("w1", "T1", "planned", None, 0).unwrap();
        assert_eq!(b.claimable().len(), 1);
    }

    #[test]
    fn claim_is_exclusive_and_one_per_agent() {
        let mut b = Board::new("g");
        b.propose("w1", vec![nt("alpha work"), nt("beta work")], false, 0)
            .unwrap();
        b.claim("w1", "T1", "plan", 1).unwrap();
        assert_eq!(
            b.claim("w2", "T1", "plan", 2).unwrap_err(),
            BoardError::NotOpen {
                id: "T1".into(),
                status: TaskStatus::Claimed,
                owner: Some("w1".into())
            }
        );
        assert_eq!(
            b.claim("w1", "T2", "plan", 2).unwrap_err(),
            BoardError::AlreadyClaiming { held: "T1".into() }
        );
        b.claim("w2", "T2", "plan", 2).unwrap();
    }

    #[test]
    fn claim_waits_for_deps() {
        let mut b = Board::new("g");
        b.propose(
            "w1",
            vec![nt("schema"), nt_deps("api on schema", &["#1"])],
            false,
            0,
        )
        .unwrap();
        assert_eq!(
            b.claim("w2", "T2", "", 1).unwrap_err(),
            BoardError::DepsPending {
                id: "T2".into(),
                pending: vec!["T1".into()]
            }
        );
        assert_eq!(b.claimable().len(), 1);
        b.claim("w1", "T1", "", 1).unwrap();
        b.complete("w1", "T1", "done", merged("abc"), 2).unwrap();
        b.claim("w2", "T2", "", 3).unwrap();
    }

    #[test]
    fn complete_requires_owner_and_merged_commit() {
        let mut b = Board::new("g");
        b.propose("w1", vec![nt("alpha work")], false, 0).unwrap();
        b.claim("w1", "T1", "", 1).unwrap();
        assert!(matches!(
            b.complete("w2", "T1", "s", None, 2),
            Err(BoardError::NotYours { .. })
        ));
        let unmerged = Some(CommitCheck {
            sha: "abc".into(),
            merged: false,
        });
        assert_eq!(
            b.complete("w1", "T1", "s", unmerged, 2).unwrap_err(),
            BoardError::NotMerged { sha: "abc".into() }
        );
        b.complete("w1", "T1", "s", merged("abc"), 2).unwrap();
        assert_eq!(b.task("T1").unwrap().status, TaskStatus::Done);
        assert_eq!(b.task("T1").unwrap().commit.as_deref(), Some("abc"));
        assert!(
            b.claim_of("w1").is_none(),
            "completing frees the agent to claim again"
        );
    }

    #[test]
    fn release_and_reopen_return_tasks_to_open() {
        let mut b = Board::new("g");
        b.propose("w1", vec![nt("alpha work")], false, 0).unwrap();
        b.claim("w1", "T1", "", 1).unwrap();
        b.release("w1", "T1", "blocked", 2).unwrap();
        assert_eq!(b.task("T1").unwrap().status, TaskStatus::Open);
        b.claim("w2", "T1", "", 3).unwrap();
        b.complete("w2", "T1", "s", None, 4).unwrap();
        assert_eq!(
            b.reopen("w3", "T1", "", 5).unwrap_err(),
            BoardError::Empty("reason")
        );
        b.reopen("w3", "T1", "tests fail on main", 5).unwrap();
        let t = b.task("T1").unwrap();
        assert_eq!((t.status, t.claimed_by.as_deref()), (TaskStatus::Open, None));
    }

    #[test]
    fn chat_cursor_and_mentions() {
        let mut b = Board::new("g");
        b.post("w1", "hi @w2 and @all.", 1).unwrap();
        b.post("w3", "unrelated", 2).unwrap();
        let u = b.unread("w2");
        assert_eq!(u.count, 2);
        assert_eq!(u.mentions.len(), 1);
        assert_eq!(u.mentions[0].mentions, vec!["w2", "all"]);
        let (page, skipped) = b.read_chat("w2");
        assert_eq!((page.len(), skipped), (2, 0));
        assert_eq!(b.unread("w2").count, 0);
        assert_eq!(b.unread("w1").count, 1, "own messages count as read");
    }

    #[test]
    fn notes_search_ranks_by_overlap() {
        let mut b = Board::new("g");
        b.add_note(
            "w1",
            "vitest config",
            "run with pnpm vitest",
            vec!["test".into()],
            1,
        )
        .unwrap();
        b.add_note("w2", "router layout", "routes live in src/routes", vec![], 2)
            .unwrap();
        let hits = b.search_notes("how to run vitest", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "N1");
        assert_eq!(
            b.search_notes("", 5)[0].id,
            "N2",
            "empty query lists latest first"
        );
    }

    #[test]
    fn goal_closes_only_when_everything_is_done() {
        let mut b = Board::new("g");
        assert_eq!(b.check_closable().unwrap_err(), BoardError::NothingDone);
        b.propose("w1", vec![nt("alpha work"), nt("beta work")], false, 0)
            .unwrap();
        b.claim("w1", "T1", "", 1).unwrap();
        assert_eq!(
            b.close_goal("w1", "x").unwrap_err(),
            BoardError::OpenWork {
                open: vec!["T2".into()],
                claimed: vec!["T1".into()]
            }
        );
        b.complete("w1", "T1", "s", None, 2).unwrap();
        b.claim("w1", "T2", "", 3).unwrap();
        b.complete("w1", "T2", "s", None, 4).unwrap();
        b.close_goal("w1", "all good").unwrap();
        assert_eq!(b.goal().status, GoalStatus::Complete);
        assert_eq!(
            b.propose("w2", vec![nt("late")], false, 5).unwrap_err(),
            BoardError::GoalClosed(GoalStatus::Complete)
        );
    }

    #[test]
    fn stop_keeps_a_completed_goal() {
        let mut b = Board::new("g");
        b.stop("deadline");
        assert_eq!(b.goal().status, GoalStatus::Stopped);
        let mut c = Board::new("g");
        c.propose("w1", vec![nt("alpha work")], false, 0).unwrap();
        c.claim("w1", "T1", "", 0).unwrap();
        c.complete("w1", "T1", "s", None, 0).unwrap();
        c.close_goal("w1", "ok").unwrap();
        c.stop("deadline");
        assert_eq!(c.goal().status, GoalStatus::Complete);
    }

    #[test]
    fn view_shows_claim_claimable_and_mentions() {
        let mut b = Board::new("ship it");
        b.propose("w1", vec![nt("schema"), nt_deps("api", &["#1"])], false, 0)
            .unwrap();
        b.claim("w1", "T1", "", 0).unwrap();
        b.post("w2", "@w1 ping", 0).unwrap();
        let v = b.render_view("w1", "0123456789abcdef");
        assert!(v.contains("HUB colony-main @ 01234567"), "{v}");
        assert!(v.contains("YOU (w1) hold T1"), "{v}");
        assert!(v.contains("T2 [open, waits T1] api"), "{v}");
        assert!(v.contains("w2 → you: @w1 ping"), "{v}");
        assert!(
            !v.contains("CLAIMABLE NOW"),
            "nothing is claimable while T1 is held: {v}"
        );
    }
}
