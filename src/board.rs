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
    "Explore the repository as much as you need, then task_propose the tasks that achieve the goal (deps only \
where truly needed), then task_complete this task without a commit. Keep the critical path short: a small shared \
skeleton, wrap-up work split so its independent parts can start early, and helpers that several tasks need in \
one early task the others depend on. Exactly one worker plans; everyone else waits until the tasks appear.";
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
    /// Who reserved this open task while its deps were pending. It becomes their claim when the last dep is
    /// done, if they hold no other task then.
    pub reserved_by: Option<String>,
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

/// What `claim` did. A task whose deps are pending is reserved rather than refused: the intent is recorded on
/// the board instead of only in chat (observed in a real run: two workers drafted the same wrap-up task).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    Held,
    Reserved { waits: Vec<String> },
}

/// A reserved task that became its reserver's claim when its last dependency was completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoClaim {
    pub id: String,
    pub agent: String,
}

/// What a worker has not been shown yet: direct mentions and other workers' notes. Delivered mid-turn after
/// tool calls, or in the next nudge, whichever comes first.
#[derive(Debug, Clone, Default)]
pub struct Inbox {
    pub mentions: Vec<ChatMsg>,
    pub notes: Vec<Note>,
    /// Notes left out because there were more than fit; `note_search` finds them.
    pub older_notes: usize,
}

impl Inbox {
    pub fn is_empty(&self) -> bool {
        self.mentions.is_empty() && self.notes.is_empty()
    }
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
    Reserved {
        id: String,
        by: String,
    },
    AlreadyReserving {
        held: String,
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
            BoardError::Reserved { id, by } => write!(
                f,
                "{id} is reserved by {by}; it becomes theirs when its deps are done. Pick another task"
            ),
            BoardError::AlreadyReserving { held } => write!(
                f,
                "you already reserve {held}; one reservation at a time (task_release {held} drops it)"
            ),
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
    /// Last chat seq whose direct mentions each agent has been shown (mid-turn or in a nudge).
    delivered_chat: HashMap<String, u64>,
    /// Number of notes each agent has been shown.
    delivered_notes: HashMap<String, usize>,
}

/// Most notes one inbox delivery carries. A fresh worker joining a long run is pointed at `note_search` for
/// the rest instead of receiving them all.
const INBOX_NOTES: usize = 5;

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
            delivered_chat: HashMap::new(),
            delivered_notes: HashMap::new(),
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

    pub fn reservation_of(&self, agent: &str) -> Option<&Task> {
        self.tasks
            .iter()
            .find(|t| t.status == TaskStatus::Open && t.reserved_by.as_deref() == Some(agent))
    }

    /// Tasks that can be claimed right now: open, with every dependency done.
    pub fn claimable(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Open && self.pending_deps(t).is_empty())
            .collect()
    }

    /// Ids of `claimable()`. Callers compare it before and after a change to tell whether work opened up,
    /// which is the only reason to wake idle workers.
    pub fn claimable_ids(&self) -> Vec<String> {
        self.claimable().into_iter().map(|t| t.id.clone()).collect()
    }

    pub fn is_closable(&self) -> bool {
        self.check_closable().is_ok()
    }

    /// The goal's first non-empty line without heading marks. The full text is in every worker's system
    /// prompt; repeating it in each `board_view` cost about 20% of a real run whose goal was a 25k-character
    /// document.
    pub fn goal_title(&self) -> String {
        let line = self
            .goal
            .text
            .lines()
            .map(|l| l.trim().trim_start_matches('#').trim())
            .find(|l| !l.is_empty())
            .unwrap_or("");
        clip(line, 120)
    }

    /// Whether `agent` has added a note since it last claimed `id`. `task_complete` reminds workers who have
    /// not, as Agensh does after a turn that published nothing to shared context.
    pub fn noted_since_claim(&self, agent: &str, id: &str) -> bool {
        let Some(claimed_at) = self.task(id).and_then(|t| {
            t.log
                .iter()
                .rev()
                .find(|e| e.kind == "plan" && e.by == agent)
                .map(|e| e.at)
        }) else {
            return false;
        };
        self.notes.iter().any(|n| n.by == agent && n.at >= claimed_at)
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
                reserved_by: None,
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

    /// Claims an open task, or reserves it when its deps are still pending.
    pub fn claim(&mut self, by: &str, id: &str, plan: &str, now: i64) -> Result<Claim, BoardError> {
        self.ensure_active()?;
        let i = self
            .index(id)
            .ok_or_else(|| BoardError::UnknownTask(id.to_string()))?;
        let task = &self.tasks[i];
        let pending = self.pending_deps(task);
        if task.status == TaskStatus::Open && !pending.is_empty() {
            return self.reserve(i, by, plan, pending, now);
        }
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
        if task.status != TaskStatus::Open {
            return Err(BoardError::NotOpen {
                id: id.to_string(),
                status: task.status,
                owner: task.claimed_by.clone(),
            });
        }
        self.take(i, by, plan.trim(), now);
        Ok(Claim::Held)
    }

    fn reserve(
        &mut self,
        i: usize,
        by: &str,
        plan: &str,
        waits: Vec<String>,
        now: i64,
    ) -> Result<Claim, BoardError> {
        let id = self.tasks[i].id.clone();
        match self.tasks[i].reserved_by.as_deref() {
            Some(other) if other != by => {
                return Err(BoardError::Reserved {
                    id,
                    by: other.to_string(),
                })
            }
            Some(_) => return Ok(Claim::Reserved { waits }),
            None => {}
        }
        if let Some(held) = self.reservation_of(by) {
            return Err(BoardError::AlreadyReserving {
                held: held.id.clone(),
            });
        }
        let task = &mut self.tasks[i];
        task.reserved_by = Some(by.to_string());
        task.updated_at = now;
        task.log.push(Entry {
            at: now,
            by: by.to_string(),
            kind: "reserve",
            text: plan.trim().to_string(),
        });
        Ok(Claim::Reserved { waits })
    }

    fn take(&mut self, i: usize, by: &str, plan: &str, now: i64) {
        let task = &mut self.tasks[i];
        task.status = TaskStatus::Claimed;
        task.claimed_by = Some(by.to_string());
        task.reserved_by = None;
        task.updated_at = now;
        task.log.push(Entry {
            at: now,
            by: by.to_string(),
            kind: "plan",
            text: plan.to_string(),
        });
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

    /// Gives back a claim, or drops a reservation.
    pub fn release(&mut self, by: &str, id: &str, reason: &str, now: i64) -> Result<(), BoardError> {
        self.ensure_active()?;
        if let Some(i) = self.index(id).filter(|&i| {
            let t = &self.tasks[i];
            t.status == TaskStatus::Open && t.reserved_by.as_deref() == Some(by)
        }) {
            self.unreserve(i, by, reason, now);
            return Ok(());
        }
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

    /// Marks the caller's task done. Reserved tasks whose last dep this was become their reservers' claims
    /// (returned, so the reservers can be woken), or lapse when the reserver already holds another task.
    pub fn complete(
        &mut self,
        by: &str,
        id: &str,
        summary: &str,
        commit: Option<CommitCheck>,
        now: i64,
    ) -> Result<Vec<AutoClaim>, BoardError> {
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
        Ok(self.resolve_reservations(now))
    }

    fn resolve_reservations(&mut self, now: i64) -> Vec<AutoClaim> {
        let ready: Vec<(usize, String)> = (0..self.tasks.len())
            .filter_map(|i| {
                let t = &self.tasks[i];
                let r = t.reserved_by.clone()?;
                (t.status == TaskStatus::Open && self.pending_deps(t).is_empty()).then_some((i, r))
            })
            .collect();
        let mut auto = Vec::new();
        for (i, agent) in ready {
            match self.claim_of(&agent).map(|t| t.id.clone()) {
                None => {
                    let plan = self.tasks[i]
                        .log
                        .iter()
                        .rev()
                        .find(|e| e.kind == "reserve" && e.by == agent)
                        .map(|e| e.text.clone())
                        .unwrap_or_default();
                    self.take(i, &agent, &plan, now);
                    auto.push(AutoClaim {
                        id: self.tasks[i].id.clone(),
                        agent,
                    });
                }
                Some(held) => {
                    let reason = format!("reservation lapsed: {agent} holds {held}");
                    self.unreserve(i, &agent, &reason, now);
                }
            }
        }
        auto
    }

    fn unreserve(&mut self, i: usize, by: &str, reason: &str, now: i64) {
        let task = &mut self.tasks[i];
        task.reserved_by = None;
        task.updated_at = now;
        task.log.push(Entry {
            at: now,
            by: by.to_string(),
            kind: "unreserve",
            text: reason.trim().to_string(),
        });
    }

    /// Drops every reservation `agent` holds. The harness calls it for a worker that has failed.
    pub fn drop_reservations(&mut self, agent: &str, now: i64) {
        let held: Vec<usize> = (0..self.tasks.len())
            .filter(|&i| self.tasks[i].reserved_by.as_deref() == Some(agent))
            .collect();
        for i in held {
            self.unreserve(i, agent, "worker failed; dropped by the harness", now);
        }
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
        // The cursor is left alone: moving it to this message would mark every unread message from others as
        // read (observed in a real run: 29 deliveries lost). Own messages are skipped when reading instead.
        self.chat.push(msg.clone());
        Ok(msg)
    }

    /// Returns other agents' messages after the cursor and moves the cursor to the end. When there are too
    /// many, returns the most recent ones and reports how many were skipped.
    pub fn read_chat(&mut self, agent: &str) -> (Vec<ChatMsg>, usize) {
        let cursor = self.cursors.get(agent).copied().unwrap_or(0);
        let unread: Vec<&ChatMsg> = self
            .chat
            .iter()
            .filter(|m| m.seq > cursor && m.from != agent)
            .collect();
        let skipped = unread.len().saturating_sub(CHAT_PAGE);
        let page = unread.into_iter().skip(skipped).cloned().collect();
        self.cursors.insert(agent.to_string(), self.chat.len() as u64);
        self.delivered_chat
            .insert(agent.to_string(), self.chat.len() as u64);
        (page, skipped)
    }

    fn is_direct(m: &ChatMsg, agent: &str) -> bool {
        m.from != agent && m.mentions.iter().any(|x| x == agent)
    }

    /// Direct mentions of `agent` it has not been shown yet. `@all` is an announcement, read at turn start,
    /// so it does not count.
    pub fn pending_direct(&self, agent: &str) -> usize {
        let seen = self.delivered_chat.get(agent).copied().unwrap_or(0);
        self.chat
            .iter()
            .filter(|m| m.seq > seen && Self::is_direct(m, agent))
            .count()
    }

    /// Returns what `agent` has not been shown yet (direct mentions, other workers' notes) and marks it
    /// shown. Unlike `read_chat` it leaves the read cursor alone, so chat still counts as unread.
    pub fn take_inbox(&mut self, agent: &str) -> Inbox {
        let seen = self.delivered_chat.get(agent).copied().unwrap_or(0);
        let mentions: Vec<ChatMsg> = self
            .chat
            .iter()
            .filter(|m| m.seq > seen && Self::is_direct(m, agent))
            .cloned()
            .collect();
        self.delivered_chat
            .insert(agent.to_string(), self.chat.len() as u64);
        let from = self.delivered_notes.get(agent).copied().unwrap_or(0);
        let fresh: Vec<&Note> = self.notes[from.min(self.notes.len())..]
            .iter()
            .filter(|n| n.by != agent)
            .collect();
        let older_notes = fresh.len().saturating_sub(INBOX_NOTES);
        let notes = fresh.into_iter().skip(older_notes).cloned().collect();
        self.delivered_notes.insert(agent.to_string(), self.notes.len());
        Inbox {
            mentions,
            notes,
            older_notes,
        }
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
        let _ = writeln!(
            s,
            "GOAL [{:?}]: {} (full text: your system prompt)",
            self.goal.status,
            self.goal_title()
        );
        let _ = writeln!(s, "HUB colony-main @ {}", short(hub_head));
        match self.claim_of(agent) {
            Some(t) => {
                let _ = writeln!(s, "YOU ({agent}) hold {} \"{}\"", t.id, t.title);
            }
            None => {
                let _ = writeln!(s, "YOU ({agent}) hold no task");
            }
        }
        if let Some(t) = self.reservation_of(agent) {
            let _ = writeln!(
                s,
                "YOU reserve {} \"{}\" (waits {}; becomes yours when they are done)",
                t.id,
                t.title,
                self.pending_deps(t).join(",")
            );
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
                    let mut tag = if pending.is_empty() {
                        "open".to_string()
                    } else {
                        format!("open, waits {}", pending.join(","))
                    };
                    if let Some(r) = &t.reserved_by {
                        tag.push_str(&format!(", reserved {r}"));
                    }
                    tag
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
        assert!(matches!(b.claim("w2", "T2", "", 1), Ok(Claim::Reserved { .. })));
        assert_eq!(b.claimable().len(), 1, "a reserved task is not claimable");
        assert!(b.claim_of("w2").is_none(), "a reservation is not a claim");
        b.claim("w1", "T1", "", 1).unwrap();
        b.complete("w1", "T1", "done", merged("abc"), 2).unwrap();
        assert_eq!(b.claim_of("w2").map(|t| t.id.as_str()), Some("T2"));
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
    fn posting_keeps_unread_messages_from_others() {
        // Observed in a real run: w2 posted while w1's "I take T22" was unread, and never saw it.
        let mut b = Board::new("g");
        b.post("w1", "@all I take T22", 1).unwrap();
        b.post("w2", "T19 merged", 2).unwrap();
        let u = b.unread("w2");
        assert_eq!(u.count, 1, "w1's message must stay unread for w2");
        assert_eq!(u.mentions[0].text, "@all I take T22");
        let (page, _) = b.read_chat("w2");
        let texts: Vec<&str> = page.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["@all I take T22"], "own posts are not read back");
    }

    #[test]
    fn view_shows_goal_title_not_body() {
        let body = "detail line that must not be repeated on every board_view";
        let b = Board::new(format!("\n# Ship the utility package\n\n{body}\n"));
        let v = b.render_view("w1", "");
        assert!(v.contains("GOAL [Active]: Ship the utility package"), "{v}");
        assert!(!v.contains(body), "{v}");
    }

    #[test]
    fn noted_since_claim_tracks_notes_after_the_claim() {
        let mut b = Board::new("g");
        b.propose("w1", vec![nt("a")], false, 0).unwrap();
        b.add_note("w1", "old", "before the claim", vec![], 5).unwrap();
        b.claim("w1", "T1", "", 10).unwrap();
        assert!(!b.noted_since_claim("w1", "T1"));
        b.add_note("w2", "other", "someone else's", vec![], 11).unwrap();
        assert!(!b.noted_since_claim("w1", "T1"));
        b.add_note("w1", "found", "a reusable fact", vec![], 12).unwrap();
        assert!(b.noted_since_claim("w1", "T1"));
    }

    #[test]
    fn claimable_ids_grow_only_when_work_opens() {
        let mut b = Board::new("g");
        b.propose("w1", vec![nt("a"), nt_deps("b", &["#1"])], false, 0)
            .unwrap();
        assert_eq!(b.claimable_ids(), vec!["T1"]);
        b.claim("w1", "T1", "", 0).unwrap();
        assert!(b.claimable_ids().is_empty());
        b.complete("w1", "T1", "done", None, 0).unwrap();
        assert_eq!(b.claimable_ids(), vec!["T2"]);
        assert!(!b.is_closable());
        b.claim("w2", "T2", "", 0).unwrap();
        b.complete("w2", "T2", "done", None, 0).unwrap();
        assert!(b.is_closable());
    }

    #[test]
    fn inbox_delivers_direct_mentions_and_notes_once() {
        let mut b = Board::new("g");
        b.post("w1", "@all announcement", 1).unwrap();
        b.post("w1", "@w2 leave the shared helper to me", 2).unwrap();
        b.post("w3", "@w1 unrelated", 3).unwrap();
        b.add_note("w3", "samples", "shared inputs in fixtures.rs", vec![], 4)
            .unwrap();
        b.add_note("w2", "mine", "w2's own note", vec![], 5).unwrap();
        assert_eq!(b.pending_direct("w2"), 1);
        let inbox = b.take_inbox("w2");
        let texts: Vec<&str> = inbox.mentions.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["@w2 leave the shared helper to me"],
            "direct mentions only"
        );
        let notes: Vec<&str> = inbox.notes.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(notes, vec!["samples"], "others' notes only");
        assert!(b.take_inbox("w2").is_empty(), "each item is delivered once");
        assert_eq!(b.pending_direct("w2"), 0);
        assert_eq!(b.unread("w2").count, 3, "delivery does not mark chat as read");
        b.post("w1", "@w2 second", 6).unwrap();
        b.read_chat("w2");
        assert!(
            b.take_inbox("w2").mentions.is_empty(),
            "reading chat counts as delivery"
        );
    }

    #[test]
    fn reserve_blocked_task_and_claim_it_when_deps_finish() {
        let mut b = Board::new("g");
        b.propose("w1", vec![nt("impl"), nt_deps("docs", &["#1"])], false, 0)
            .unwrap();
        b.claim("w2", "T1", "", 0).unwrap();
        assert_eq!(
            b.claim("w1", "T2", "write the README", 1).unwrap(),
            Claim::Reserved {
                waits: vec!["T1".into()]
            }
        );
        assert_eq!(b.reservation_of("w1").map(|t| t.id.as_str()), Some("T2"));
        assert!(b
            .render_view("w3", "")
            .contains("T2 [open, waits T1, reserved w1] docs"));
        assert_eq!(
            b.claim("w3", "T2", "", 2),
            Err(BoardError::Reserved {
                id: "T2".into(),
                by: "w1".into()
            })
        );
        let auto = b.complete("w2", "T1", "done", None, 3).unwrap();
        assert_eq!(
            auto,
            vec![AutoClaim {
                id: "T2".into(),
                agent: "w1".into()
            }]
        );
        assert_eq!(b.claim_of("w1").map(|t| t.id.as_str()), Some("T2"));
        assert!(b.reservation_of("w1").is_none());
    }

    #[test]
    fn reserve_is_one_per_agent_and_lapses_when_the_reserver_is_busy() {
        let mut b = Board::new("g");
        b.propose(
            "w1",
            vec![
                nt("base"),
                nt_deps("x", &["#1"]),
                nt_deps("y", &["#1"]),
                nt("other"),
            ],
            false,
            0,
        )
        .unwrap();
        b.claim("w2", "T1", "", 0).unwrap();
        b.claim("w1", "T2", "", 0).unwrap();
        assert_eq!(
            b.claim("w1", "T3", "", 0),
            Err(BoardError::AlreadyReserving { held: "T2".into() })
        );
        b.claim("w1", "T4", "", 0).unwrap();
        let auto = b.complete("w2", "T1", "done", None, 1).unwrap();
        assert!(auto.is_empty(), "w1 holds T4, so its reservation lapses");
        let t2 = b.task("T2").unwrap();
        assert_eq!((t2.status, t2.reserved_by.as_deref()), (TaskStatus::Open, None));
        assert!(b.claimable_ids().contains(&"T2".to_string()));
    }

    #[test]
    fn reservation_can_be_released_and_is_dropped_for_failed_workers() {
        let mut b = Board::new("g");
        b.propose(
            "w1",
            vec![nt("base"), nt_deps("x", &["#1"]), nt_deps("y", &["#1"])],
            false,
            0,
        )
        .unwrap();
        b.claim("w1", "T2", "", 0).unwrap();
        b.release("w1", "T2", "changed my mind", 1).unwrap();
        assert!(b.task("T2").unwrap().reserved_by.is_none());
        b.claim("w3", "T3", "", 2).unwrap();
        b.drop_reservations("w3", 3);
        assert!(b.task("T3").unwrap().reserved_by.is_none());
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
