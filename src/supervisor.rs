//! Supervisor: a process manager, not a conductor. It never decides who does what.
//!
//! It starts workers on a stagger (ramp), keeps the number of workers mid-turn within a limit, and when a
//! worker's turn ends either hands it the next turn with a digest of board changes or puts it to sleep
//! until there is work. On a usage limit it pauses everyone and halves concurrency, then raises it one
//! step at a time while turns keep succeeding.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::board::GoalStatus;
use crate::config::Config;
use crate::hub;
use crate::prompts::{self, Nudge};
use crate::server::Shared;
use crate::stream::{RateLimitInfo, StreamEvent, TurnResult};
use crate::worker::{self, SpawnSpec, WorkerHandle};

const MAX_RESTARTS: u32 = 3;
const MAX_ERRORS_IN_ROW: u32 = 3;
/// After this many consecutive turns that end without a claim, the worker sleeps until the board changes,
/// even if claimable tasks exist.
const MAX_IDLE_TURNS: u32 = 2;
const MAX_IDLE_BACKOFF: Duration = Duration::from_secs(15 * 60);
const MIN_LIMIT_BACKOFF: Duration = Duration::from_secs(60);
const MAX_PAUSE: Duration = Duration::from_secs(6 * 3600);
const SETUP_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const REPORT_EVERY_TICKS: u64 = 60;

#[derive(Debug)]
pub enum SupEvent {
    Ready {
        idx: usize,
        setup_note: Option<String>,
    },
    PrepFailed {
        idx: usize,
        error: String,
    },
    Stream {
        idx: usize,
        generation: u32,
        ev: StreamEvent,
    },
    Exited {
        idx: usize,
        generation: u32,
        code: Option<i32>,
    },
    Board(BoardSignal),
    Interrupt,
}

#[derive(Debug, Clone)]
pub enum BoardSignal {
    /// Work appeared or opened up (proposal, release, reopen, or a completion that unblocked deps).
    NewWork,
    /// Workers mentioned in chat. `"all"` means everyone.
    Mention(Vec<String>),
    GoalClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StopReason {
    Complete,
    Deadline,
    Budget,
    Interrupted,
    AllFailed,
}

impl StopReason {
    pub fn label(self) -> &'static str {
        match self {
            StopReason::Complete => "goal complete",
            StopReason::Deadline => "deadline reached",
            StopReason::Budget => "cost cap reached",
            StopReason::Interrupted => "interrupted by user",
            StopReason::AllFailed => "all workers failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Its ramp slot has not come up yet.
    Pending,
    /// Cloning the workspace and running setup.
    Preparing,
    /// Waiting for a free turn slot.
    Queued,
    Working,
    /// Nothing to do; waiting for the board to change.
    Idle,
    /// Stdin closed; waiting for the process to exit.
    Closing,
    Exited,
    Failed,
}

#[derive(Debug, Clone, Copy)]
enum Next {
    Kickoff,
    Nudge(Nudge),
}

struct Slot {
    id: String,
    phase: Phase,
    generation: u32,
    handle: Option<WorkerHandle>,
    session_id: String,
    ramp_at: Instant,
    next: Option<Next>,
    setup_note: Option<String>,
    turns: u32,
    idle_turns: u32,
    errors_in_row: u32,
    restarts: u32,
    cost_done: f64,
    cost_cur: f64,
    activity: String,
    idle_until: Option<Instant>,
    idle_backoff: Duration,
    seen_tasks: usize,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AgentRow {
    pub id: String,
    pub phase: Option<Phase>,
    pub claim: Option<String>,
    pub turns: u32,
    pub cost_usd: f64,
    pub activity: String,
    pub restarts: u32,
}

/// Run state served to the dashboard.
#[derive(Debug, Clone, Serialize, Default)]
pub struct RunStatus {
    pub run_id: String,
    pub repo: String,
    pub phase: String,
    pub started_ms: i64,
    pub deadline_ms: i64,
    pub max_active: usize,
    pub limit: usize,
    pub active: usize,
    pub queued: usize,
    pub paused_until_ms: Option<i64>,
    pub rate_limit: Option<RateLimitInfo>,
    pub total_cost_usd: f64,
    pub max_cost_usd: Option<f64>,
    pub hub_head: String,
    pub agents: Vec<AgentRow>,
}

pub struct RunCtx {
    pub cfg: Config,
    pub run_id: String,
    pub repo_label: String,
    pub run_dir: PathBuf,
    pub server_url: String,
    pub email: String,
    pub system_prompt: String,
}

impl RunCtx {
    fn workspace(&self, id: &str) -> PathBuf {
        self.run_dir.join("ws").join(id)
    }

    fn log_path(&self, id: &str) -> PathBuf {
        self.run_dir.join("agents").join(format!("{id}.jsonl"))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub reason: StopReason,
    pub total_cost_usd: f64,
    pub elapsed_secs: u64,
    pub turns: u32,
}

pub struct Supervisor {
    ctx: Arc<RunCtx>,
    app: Arc<Shared>,
    tx: mpsc::UnboundedSender<SupEvent>,
    rx: mpsc::UnboundedReceiver<SupEvent>,
    slots: Vec<Slot>,
    queue: VecDeque<usize>,
    limit: usize,
    streak: usize,
    paused_until: Option<Instant>,
    limit_backoff: Duration,
    last_warning_cut: Option<Instant>,
    rate_limit: Option<RateLimitInfo>,
    stopping: Option<StopReason>,
    kill_at: Option<Instant>,
    started: Instant,
    started_ms: i64,
    deadline: Instant,
    hub_head: String,
}

impl Supervisor {
    pub fn new(
        ctx: Arc<RunCtx>,
        app: Arc<Shared>,
        tx: mpsc::UnboundedSender<SupEvent>,
        rx: mpsc::UnboundedReceiver<SupEvent>,
    ) -> Self {
        let started = Instant::now();
        let cfg = &ctx.cfg;
        let slots = (0..cfg.workers)
            .map(|i| Slot {
                id: worker_id(i, cfg.workers),
                phase: Phase::Pending,
                generation: 0,
                handle: None,
                session_id: uuid::Uuid::new_v4().to_string(),
                ramp_at: started + Duration::from_secs(cfg.ramp_secs * i as u64),
                next: None,
                setup_note: None,
                turns: 0,
                idle_turns: 0,
                errors_in_row: 0,
                restarts: 0,
                cost_done: 0.0,
                cost_cur: 0.0,
                activity: String::new(),
                idle_until: None,
                idle_backoff: Duration::from_secs(cfg.idle_secs),
                seen_tasks: 0,
            })
            .collect();
        let deadline = started + Duration::from_secs_f64(cfg.hours * 3600.0);
        Supervisor {
            limit: cfg.max_active,
            app,
            tx,
            rx,
            slots,
            queue: VecDeque::new(),
            streak: 0,
            paused_until: None,
            limit_backoff: MIN_LIMIT_BACKOFF,
            last_warning_cut: None,
            rate_limit: None,
            stopping: None,
            kill_at: None,
            started,
            started_ms: chrono::Utc::now().timestamp_millis(),
            deadline,
            hub_head: String::new(),
            ctx,
        }
    }

    pub async fn run(mut self) -> Outcome {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut ticks: u64 = 0;
        loop {
            tokio::select! {
                ev = self.rx.recv() => match ev {
                    Some(ev) => self.on_event(ev),
                    None => break,
                },
                _ = tick.tick() => {
                    ticks += 1;
                    self.on_tick(ticks).await;
                }
            }
            self.pump().await;
            self.publish();
            if self.finished() {
                break;
            }
        }
        self.publish();
        Outcome {
            reason: self.stopping.unwrap_or(StopReason::AllFailed),
            total_cost_usd: self.total_cost(),
            elapsed_secs: self.started.elapsed().as_secs(),
            turns: self.slots.iter().map(|s| s.turns).sum(),
        }
    }

    fn on_event(&mut self, ev: SupEvent) {
        match ev {
            SupEvent::Ready { idx, setup_note } => self.on_ready(idx, setup_note),
            SupEvent::PrepFailed { idx, error } => {
                log(&format!(
                    "{}: workspace preparation failed: {error}",
                    self.slots[idx].id
                ));
                self.slots[idx].phase = Phase::Failed;
                self.check_all_failed();
            }
            SupEvent::Stream { idx, generation, ev } => {
                if self.slots[idx].generation != generation {
                    return;
                }
                match ev {
                    StreamEvent::Init { .. } => {}
                    StreamEvent::Activity(a) => self.slots[idx].activity = a,
                    StreamEvent::RateLimit(info) => self.on_rate_limit(info),
                    StreamEvent::TurnEnd(res) => self.on_turn_end(idx, res),
                }
            }
            SupEvent::Exited {
                idx,
                generation,
                code,
            } => self.on_exited(idx, generation, code),
            SupEvent::Board(signal) => self.on_board(signal),
            SupEvent::Interrupt => {
                if self.stopping.is_some() {
                    log("second interrupt: killing the remaining workers now");
                    self.kill_at = Some(Instant::now());
                } else {
                    self.stop(StopReason::Interrupted);
                }
            }
        }
    }

    async fn on_tick(&mut self, ticks: u64) {
        let now = Instant::now();
        if self.stopping.is_none() {
            if now >= self.deadline {
                self.stop(StopReason::Deadline);
            } else if self
                .ctx
                .cfg
                .max_cost_usd
                .is_some_and(|max| self.total_cost() >= max)
            {
                self.stop(StopReason::Budget);
            }
        }
        if self.stopping.is_none() {
            for idx in 0..self.slots.len() {
                let s = &self.slots[idx];
                if s.phase == Phase::Pending && now >= s.ramp_at {
                    self.prepare(idx);
                } else if s.phase == Phase::Idle && s.idle_until.is_some_and(|t| now >= t) {
                    let slot = &mut self.slots[idx];
                    slot.idle_backoff = (slot.idle_backoff * 2).min(MAX_IDLE_BACKOFF);
                    self.enqueue(idx, Next::Nudge(Nudge::IdleCheck), false);
                }
            }
        }
        if self.paused_until.is_some_and(|p| now >= p) {
            self.paused_until = None;
            log(&format!("pause lifted; concurrency {}", self.limit));
        }
        if self.kill_at.is_some_and(|k| now >= k) {
            for s in &mut self.slots {
                if let Some(h) = s.handle.as_mut() {
                    h.kill();
                }
            }
        }
        if ticks % 5 == 1 {
            if let Ok(h) = self.app.hub.head().await {
                self.hub_head = h;
            }
        }
        if ticks.is_multiple_of(REPORT_EVERY_TICKS) && self.stopping.is_none() {
            self.report();
        }
    }

    /// Clones the workspace and runs setup in the background; `Ready` arrives when done.
    fn prepare(&mut self, idx: usize) {
        let slot = &mut self.slots[idx];
        slot.phase = Phase::Preparing;
        let id = slot.id.clone();
        let hub = self.app.hub.clone();
        let gate = self.app.gate.clone();
        let dest = self.ctx.workspace(&id);
        let setup = self.ctx.cfg.setup.clone();
        let email = self.ctx.email.clone();
        let setup_log = self.ctx.run_dir.join("agents").join(format!("{id}.setup.log"));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = async {
                hub.clone_workspace(&dest, &id, &email).await?;
                let mut note = None;
                if let Some(cmd) = setup {
                    let out = {
                        let _lease = gate.acquire(&id, "setup").await;
                        hub::run_shell(&dest, &cmd, SETUP_TIMEOUT, 400).await?
                    };
                    tokio::fs::write(&setup_log, &out.tail).await?;
                    if !out.ok {
                        let tail: Vec<&str> = out.tail.lines().rev().take(15).collect();
                        let tail: Vec<&str> = tail.into_iter().rev().collect();
                        note = Some(format!(
                            "workspace setup `{cmd}` failed (exit {:?}); fix or rerun it. Last lines:\n{}",
                            out.code,
                            tail.join("\n")
                        ));
                    }
                }
                anyhow::Ok(note)
            }
            .await;
            let _ = tx.send(match res {
                Ok(setup_note) => SupEvent::Ready { idx, setup_note },
                Err(e) => SupEvent::PrepFailed {
                    idx,
                    error: format!("{e:#}"),
                },
            });
        });
    }

    fn on_ready(&mut self, idx: usize, setup_note: Option<String>) {
        if self.stopping.is_some() || self.slots[idx].phase != Phase::Preparing {
            self.slots[idx].phase = Phase::Exited;
            return;
        }
        self.slots[idx].setup_note = setup_note;
        if let Err(e) = self.spawn_process(idx, false) {
            log(&format!("{}: failed to start: {e:#}", self.slots[idx].id));
            self.slots[idx].phase = Phase::Failed;
            self.check_all_failed();
            return;
        }
        log(&format!("{} started", self.slots[idx].id));
        self.enqueue(idx, Next::Kickoff, false);
    }

    fn spawn_process(&mut self, idx: usize, resume: bool) -> anyhow::Result<()> {
        let ctx = self.ctx.clone();
        let slot = &mut self.slots[idx];
        slot.generation += 1;
        let cwd = ctx.workspace(&slot.id);
        let spec = SpawnSpec {
            cfg: &ctx.cfg,
            agent: &slot.id,
            run_id: &ctx.run_id,
            cwd: &cwd,
            session_id: &slot.session_id,
            resume,
            server_url: &ctx.server_url,
            system_prompt: &ctx.system_prompt,
            log_path: ctx.log_path(&slot.id),
        };
        slot.handle = Some(worker::spawn(&spec, idx, slot.generation, self.tx.clone())?);
        Ok(())
    }

    fn enqueue(&mut self, idx: usize, next: Next, front: bool) {
        let slot = &mut self.slots[idx];
        slot.next = Some(next);
        slot.phase = Phase::Queued;
        slot.idle_until = None;
        if front {
            self.queue.push_front(idx);
        } else {
            self.queue.push_back(idx);
        }
    }

    /// Hands free turn slots to queued workers. Each message is built from the board at send time, so it
    /// is never stale.
    async fn pump(&mut self) {
        if self.stopping.is_some() || self.paused_until.is_some() {
            return;
        }
        while self.active() < self.limit {
            let Some(idx) = self.queue.pop_front() else { break };
            if self.slots[idx].phase != Phase::Queued {
                continue;
            }
            let Some(next) = self.slots[idx].next.take() else {
                continue;
            };
            let time_left = self.deadline.saturating_duration_since(Instant::now());
            let text = {
                let board = self.app.board();
                let slot = &self.slots[idx];
                let text = match next {
                    Next::Kickoff => prompts::kickoff(
                        &slot.id,
                        &board.goal().text,
                        time_left,
                        slot.setup_note.as_deref(),
                    ),
                    Next::Nudge(n) => prompts::nudge(&board, &slot.id, slot.seen_tasks, time_left, n),
                };
                self.slots[idx].seen_tasks = board.tasks().len();
                text
            };
            let slot = &mut self.slots[idx];
            let Some(handle) = slot.handle.as_mut() else {
                continue;
            };
            match handle.send(&text).await {
                Ok(()) => slot.phase = Phase::Working,
                // The process is already gone; the pending Exited event handles the restart.
                Err(e) => log(&format!("{}: failed to send message: {e}", slot.id)),
            }
        }
    }

    fn on_turn_end(&mut self, idx: usize, res: TurnResult) {
        let slot = &mut self.slots[idx];
        slot.turns += 1;
        slot.cost_cur = res.total_cost_usd;
        if res.limited {
            log(&format!(
                "{}: turn ended on a usage limit: {}",
                slot.id,
                crate::board::clip(&res.text, 120)
            ));
            self.on_limited(None);
            if self.stopping.is_some() {
                self.close(idx);
            } else {
                self.enqueue(idx, Next::Nudge(Nudge::AfterLimit), true);
            }
            return;
        }
        if res.is_error {
            slot.errors_in_row += 1;
            log(&format!(
                "{}: turn error ({}): {}",
                slot.id,
                res.subtype,
                crate::board::clip(&res.text, 160)
            ));
        } else {
            slot.errors_in_row = 0;
            self.on_success();
        }
        if self.stopping.is_some() {
            self.close(idx);
            return;
        }
        if self.slots[idx].errors_in_row >= MAX_ERRORS_IN_ROW {
            log(&format!(
                "{}: {} errors in a row; giving up",
                self.slots[idx].id, MAX_ERRORS_IN_ROW
            ));
            self.fail(idx);
            return;
        }
        self.after_turn(idx);
    }

    /// Decides between another turn and sleep. A worker holding a claim or mentioned in chat goes again.
    fn after_turn(&mut self, idx: usize) {
        let (has_claim, claimable, mentioned) = {
            let b = self.app.board();
            let id = &self.slots[idx].id;
            (
                b.claim_of(id).is_some(),
                b.claimable().len(),
                !b.unread(id).mentions.is_empty(),
            )
        };
        let slot = &mut self.slots[idx];
        slot.idle_turns = if has_claim { 0 } else { slot.idle_turns + 1 };
        if has_claim || mentioned || (claimable > 0 && slot.idle_turns <= MAX_IDLE_TURNS) {
            slot.idle_backoff = Duration::from_secs(self.ctx.cfg.idle_secs);
            self.enqueue(idx, Next::Nudge(Nudge::Continue), false);
        } else {
            slot.phase = Phase::Idle;
            slot.idle_until = Some(Instant::now() + slot.idle_backoff);
        }
    }

    fn on_success(&mut self) {
        self.streak += 1;
        self.limit_backoff = MIN_LIMIT_BACKOFF;
        let max = self.ctx.cfg.max_active;
        if self.limit < max && self.streak >= self.limit * 2 {
            self.limit += 1;
            self.streak = 0;
            log(&format!("concurrency limit up: {}/{max}", self.limit));
        }
    }

    fn on_rate_limit(&mut self, info: RateLimitInfo) {
        if info.is_rejected() {
            self.on_limited(info.resets_at);
        } else if info.is_warning() {
            let now = Instant::now();
            if self
                .last_warning_cut
                .is_none_or(|t| now.duration_since(t) > Duration::from_secs(60))
                && self.limit > 1
            {
                self.last_warning_cut = Some(now);
                self.limit -= 1;
                self.streak = 0;
                log(&format!(
                    "usage warning: concurrency limit down to {}",
                    self.limit
                ));
            }
        }
        self.rate_limit = Some(info);
    }

    /// Usage limit hit. If already paused, treat it as the same incident and cut only once.
    fn on_limited(&mut self, resets_at: Option<i64>) {
        let now = Instant::now();
        if self.paused_until.is_some_and(|p| p > now) {
            return;
        }
        self.streak = 0;
        self.limit = (self.limit / 2).max(1);
        let now_s = chrono::Utc::now().timestamp();
        let wait = match resets_at {
            Some(t) if t > now_s => Duration::from_secs((t - now_s) as u64).min(MAX_PAUSE),
            _ => {
                let w = self.limit_backoff;
                self.limit_backoff = (self.limit_backoff * 2).min(MAX_IDLE_BACKOFF);
                w
            }
        };
        self.paused_until = Some(now + wait);
        log(&format!(
            "usage limit: pausing {}s, resuming at concurrency {}",
            wait.as_secs(),
            self.limit
        ));
    }

    fn on_board(&mut self, signal: BoardSignal) {
        if self.stopping.is_some() {
            return;
        }
        let wake: Vec<usize> = match &signal {
            BoardSignal::GoalClosed => {
                self.stop(StopReason::Complete);
                return;
            }
            BoardSignal::NewWork => (0..self.slots.len()).collect(),
            BoardSignal::Mention(ids) => {
                let all = ids.iter().any(|i| i == "all");
                (0..self.slots.len())
                    .filter(|&i| all || ids.contains(&self.slots[i].id))
                    .collect()
            }
        };
        for idx in wake {
            if self.slots[idx].phase == Phase::Idle {
                self.slots[idx].idle_backoff = Duration::from_secs(self.ctx.cfg.idle_secs);
                self.slots[idx].idle_turns = 0;
                self.enqueue(idx, Next::Nudge(Nudge::Woken), false);
            }
        }
    }

    fn on_exited(&mut self, idx: usize, generation: u32, code: Option<i32>) {
        if self.slots[idx].generation != generation {
            return;
        }
        let slot = &mut self.slots[idx];
        slot.handle = None;
        slot.cost_done += slot.cost_cur;
        slot.cost_cur = 0.0;
        match slot.phase {
            Phase::Failed => {}
            Phase::Closing | Phase::Exited => slot.phase = Phase::Exited,
            _ if self.stopping.is_some() => slot.phase = Phase::Exited,
            _ => {
                if slot.restarts >= MAX_RESTARTS {
                    log(&format!(
                        "{}: exited again (code {code:?}); restart limit reached",
                        slot.id
                    ));
                    self.fail(idx);
                    return;
                }
                slot.restarts += 1;
                log(&format!(
                    "{}: exited unexpectedly (code {code:?}); resuming its session",
                    slot.id
                ));
                match self.spawn_process(idx, true) {
                    Ok(()) => self.enqueue(idx, Next::Nudge(Nudge::Restarted), false),
                    Err(e) => {
                        log(&format!("{}: restart failed: {e:#}", self.slots[idx].id));
                        self.fail(idx);
                    }
                }
            }
        }
    }

    /// Gives up on a worker and releases its claim so another worker can take it.
    fn fail(&mut self, idx: usize) {
        let id = self.slots[idx].id.clone();
        let released = {
            let mut b = self.app.board();
            let held = b.claim_of(&id).map(|t| t.id.clone());
            held.filter(|t| {
                b.release(&id, t, "worker failed; released by the harness", now_ms())
                    .is_ok()
            })
        };
        if released.is_some() {
            self.on_board(BoardSignal::NewWork);
        }
        let slot = &mut self.slots[idx];
        slot.phase = Phase::Failed;
        if let Some(h) = slot.handle.as_mut() {
            h.close();
        }
        self.check_all_failed();
    }

    fn check_all_failed(&mut self) {
        if self.stopping.is_none() && self.slots.iter().all(|s| s.phase == Phase::Failed) {
            self.stop(StopReason::AllFailed);
        }
    }

    fn close(&mut self, idx: usize) {
        let slot = &mut self.slots[idx];
        match slot.handle.as_mut() {
            Some(h) => {
                h.close();
                if slot.phase != Phase::Failed {
                    slot.phase = Phase::Closing;
                }
            }
            None if slot.phase != Phase::Failed => slot.phase = Phase::Exited,
            None => {}
        }
    }

    /// Stops the run by closing every stdin: in-flight turns finish and the processes exit on their own.
    /// Anything still running after the grace period is killed.
    fn stop(&mut self, reason: StopReason) {
        if self.stopping.is_some() {
            return;
        }
        self.stopping = Some(reason);
        let grace = match reason {
            StopReason::Complete => self.ctx.cfg.grace_secs.min(60),
            _ => self.ctx.cfg.grace_secs,
        };
        self.kill_at = Some(Instant::now() + Duration::from_secs(grace));
        if reason != StopReason::Complete {
            self.app.board().stop(reason.label());
        }
        self.queue.clear();
        for idx in 0..self.slots.len() {
            match self.slots[idx].phase {
                Phase::Pending | Phase::Preparing => self.slots[idx].phase = Phase::Exited,
                Phase::Exited | Phase::Failed => {}
                _ => self.close(idx),
            }
        }
        log(&format!(
            "stopping ({}): waiting up to {grace}s for in-flight turns",
            reason.label()
        ));
    }

    fn finished(&self) -> bool {
        self.slots
            .iter()
            .all(|s| matches!(s.phase, Phase::Exited | Phase::Failed))
    }

    fn active(&self) -> usize {
        self.slots.iter().filter(|s| s.phase == Phase::Working).count()
    }

    fn total_cost(&self) -> f64 {
        self.slots.iter().map(|s| s.cost_done + s.cost_cur).sum()
    }

    fn report(&self) {
        let b = self.app.board();
        let count = |st| b.tasks().iter().filter(|t| t.status == st).count();
        use crate::board::TaskStatus::*;
        log(&format!(
            "tasks open {} · claimed {} · done {} | active {}/{} | ${:.2} | {} left",
            count(Open),
            count(Claimed),
            count(Done),
            self.active(),
            self.limit,
            self.total_cost(),
            prompts::fmt_duration(self.deadline.saturating_duration_since(Instant::now())),
        ));
    }

    fn publish(&self) {
        let claims: Vec<Option<String>> = {
            let b = self.app.board();
            self.slots
                .iter()
                .map(|s| b.claim_of(&s.id).map(|t| t.id.clone()))
                .collect()
        };
        let goal_done = self.app.board().goal().status == GoalStatus::Complete;
        let phase = match (self.stopping, self.finished()) {
            (None, _) => "running".to_string(),
            (Some(r), false) => format!("stopping: {}", r.label()),
            (Some(r), true) => format!("finished: {}", r.label()),
        };
        let deadline_ms =
            self.started_ms + self.deadline.saturating_duration_since(self.started).as_millis() as i64;
        let paused_until_ms = self.paused_until.map(|p| {
            chrono::Utc::now().timestamp_millis()
                + p.saturating_duration_since(Instant::now()).as_millis() as i64
        });
        let status = RunStatus {
            run_id: self.ctx.run_id.clone(),
            repo: self.ctx.repo_label.clone(),
            phase: if goal_done && self.stopping.is_none() {
                "complete".into()
            } else {
                phase
            },
            started_ms: self.started_ms,
            deadline_ms,
            max_active: self.ctx.cfg.max_active,
            limit: self.limit,
            active: self.active(),
            queued: self.slots.iter().filter(|s| s.phase == Phase::Queued).count(),
            paused_until_ms,
            rate_limit: self.rate_limit.clone(),
            total_cost_usd: self.total_cost(),
            max_cost_usd: self.ctx.cfg.max_cost_usd,
            hub_head: self.hub_head.clone(),
            agents: self
                .slots
                .iter()
                .zip(claims)
                .map(|(s, claim)| AgentRow {
                    id: s.id.clone(),
                    phase: Some(s.phase),
                    claim,
                    turns: s.turns,
                    cost_usd: s.cost_done + s.cost_cur,
                    activity: s.activity.clone(),
                    restarts: s.restarts,
                })
                .collect(),
        };
        if let Ok(mut s) = self.app.status.lock() {
            *s = status;
        }
    }
}

/// `w1`…`w9`; with ten or more workers, zero-padded (`w01`…) so ids sort and mention unambiguously.
pub fn worker_id(i: usize, total: usize) -> String {
    let width = total.to_string().len().max(1);
    format!("w{:0width$}", i + 1)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn log(msg: &str) {
    eprintln!("[{}] {msg}", chrono::Local::now().format("%H:%M:%S"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_ids_are_zero_padded_by_team_size() {
        assert_eq!(worker_id(0, 3), "w1");
        assert_eq!(worker_id(6, 32), "w07");
        assert_eq!(worker_id(31, 32), "w32");
    }
}
