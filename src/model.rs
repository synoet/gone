//! The registry's data model and its state machine.
//!
//! An entry is `watching` until its directory is confirmed gone, then
//! `triggered` while its steps run, then `done` (every step succeeded) or
//! `failed` (every step finished and at least one gave up). Only a fresh
//! `gone watch` puts it back to `watching`.
//!
//! A step is `pending` until due, `running` while an owner process (the daemon
//! or a manual `gone run`) holds it, `retry` after a failed attempt, and `ok`
//! or `failed` when finished. Owners claim a step inside the registry lock, so
//! a step never runs in two places, and an entry triggers only from
//! `watching`, so it never triggers twice.
//!
//! All methods take the current time as an argument; nothing here reads the
//! clock.

use crate::time::{Dur, Ts};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const REGISTRY_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry {
            version: REGISTRY_VERSION,
            entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Random per registration; a replaced registration gets a new one, so
    /// results from the old one are recognised and dropped.
    pub id: String,
    pub name: String,
    /// Absolute, canonical at registration time.
    pub dir: PathBuf,
    /// Where steps run. Never inside `dir`.
    pub cwd: PathBuf,
    /// The steps' whole environment, besides `GONE_DIR` and `GONE_NAME`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub steps: Vec<Step>,
    pub registered_at: Ts,
    #[serde(default)]
    pub state: EntryState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryState {
    #[default]
    Watching,
    Triggered {
        gone_at: Ts,
    },
    Done {
        gone_at: Ts,
        at: Ts,
    },
    Failed {
        gone_at: Ts,
        at: Ts,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub cmd: Cmd,
    /// Delay after the delete before this step is due.
    #[serde(default, skip_serializing_if = "Dur::is_zero")]
    pub after: Dur,
    #[serde(default)]
    pub status: StepStatus,
    /// Attempts made in the current trigger.
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<RunResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cmd {
    /// Run directly, no shell.
    Argv(Vec<String>),
    /// Run with `/bin/sh -c`.
    Shell(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepStatus {
    #[default]
    Pending,
    Running {
        pid: u32,
        since: Ts,
    },
    Retry {
        next_at: Ts,
    },
    Ok {
        at: Ts,
    },
    Failed {
        at: Ts,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResult {
    pub at: Ts,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub took: Dur,
    /// The step's captured stdout+stderr, when the daemon ran it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub base: Dur,
    pub max: Dur,
    /// Total attempts per step, including the first.
    pub max_attempts: u32,
}

impl RetryPolicy {
    /// One attempt, no retries: what a manual `gone run` uses.
    pub const ONCE: RetryPolicy = RetryPolicy {
        base: Dur::ZERO,
        max: Dur::ZERO,
        max_attempts: 1,
    };

    /// Delay before the next attempt after `failed` failures: base, 2×base,
    /// 4×base, … capped at `max`.
    pub fn backoff(&self, failed: u32) -> Dur {
        let shift = failed.saturating_sub(1).min(32);
        Dur(self.base.0.saturating_mul(1u64 << shift).min(self.max.0))
    }
}

/// What finishing an attempt did to the step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Retry { next_at: Ts },
    GaveUp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub outcome: Outcome,
    pub attempt: u32,
    /// Set when this attempt finished the whole entry.
    pub concluded: Option<EntryState>,
}

impl Cmd {
    /// The program name only: what logs show, so arguments (which may hold
    /// secrets) never reach a log line.
    pub fn program(&self) -> &str {
        match self {
            Cmd::Argv(a) => a.first().map(String::as_str).unwrap_or(""),
            Cmd::Shell(_) => "sh",
        }
    }

    /// The full command, shell-quoted, for `list` and `run --dry-run`.
    pub fn display(&self) -> String {
        match self {
            Cmd::Argv(a) => a
                .iter()
                .map(|s| shell_quote(s))
                .collect::<Vec<_>>()
                .join(" "),
            Cmd::Shell(s) => format!("sh -c {}", shell_quote(s)),
        }
    }
}

pub fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%^".contains(&b));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

impl StepStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, StepStatus::Ok { .. } | StepStatus::Failed { .. })
    }
}

impl EntryState {
    pub fn kind(&self) -> &'static str {
        match self {
            EntryState::Watching => "watching",
            EntryState::Triggered { .. } => "triggered",
            EntryState::Done { .. } => "done",
            EntryState::Failed { .. } => "failed",
        }
    }

    pub fn gone_at(&self) -> Option<Ts> {
        match *self {
            EntryState::Watching => None,
            EntryState::Triggered { gone_at }
            | EntryState::Done { gone_at, .. }
            | EntryState::Failed { gone_at, .. } => Some(gone_at),
        }
    }
}

impl Entry {
    /// Same registration (what `gone watch` was asked for), ignoring runtime
    /// state: re-registering an identical, still-watching entry is a no-op.
    pub fn same_config(&self, other: &Entry) -> bool {
        self.name == other.name
            && self.dir == other.dir
            && self.cwd == other.cwd
            && self.env == other.env
            && self.steps.len() == other.steps.len()
            && self
                .steps
                .iter()
                .zip(&other.steps)
                .all(|(a, b)| a.cmd == b.cmd && a.after == b.after)
    }

    /// Watching → triggered. Returns false (and changes nothing) from any
    /// other state, which is what keeps a registration from running twice.
    pub fn trigger(&mut self, gone_at: Ts) -> bool {
        if self.state != EntryState::Watching {
            return false;
        }
        self.start(gone_at);
        true
    }

    fn start(&mut self, gone_at: Ts) {
        self.state = EntryState::Triggered { gone_at };
        for s in &mut self.steps {
            s.status = StepStatus::Pending;
            s.attempts = 0;
        }
    }

    pub fn due_at(&self, i: usize) -> Option<Ts> {
        let gone_at = match self.state {
            EntryState::Triggered { gone_at } => gone_at,
            _ => return None,
        };
        match self.steps.get(i)?.status {
            StepStatus::Pending => Some(gone_at.plus(self.steps[i].after)),
            StepStatus::Retry { next_at } => Some(next_at),
            _ => None,
        }
    }

    /// The step to run now, if any. Steps go in order: a step waits until
    /// every earlier step has finished (succeeded or given up), so a delayed
    /// step never overtakes the one before it.
    pub fn next_runnable(&self, now: Ts) -> Option<usize> {
        let i = self.steps.iter().position(|s| !s.status.is_terminal())?;
        match self.due_at(i) {
            Some(due) if due <= now => Some(i),
            _ => None,
        }
    }

    /// When the next step becomes due (for scheduling and `list`).
    pub fn next_due(&self) -> Option<(usize, Ts)> {
        let i = self.steps.iter().position(|s| !s.status.is_terminal())?;
        self.due_at(i).map(|t| (i, t))
    }

    /// Claims step `i` for `pid` if it is the runnable step now.
    pub fn claim(&mut self, i: usize, pid: u32, now: Ts) -> bool {
        if self.next_runnable(now) != Some(i) {
            return false;
        }
        self.steps[i].status = StepStatus::Running { pid, since: now };
        true
    }

    /// For a manual `gone run`: claims every unfinished step at once for
    /// `pid` (starting a fresh trigger unless one is in progress). Fails if
    /// any step is being run by a live owner.
    pub fn claim_all(
        &mut self,
        pid: u32,
        now: Ts,
        alive: impl Fn(u32) -> bool,
    ) -> Result<Vec<usize>, String> {
        for (i, s) in self.steps.iter().enumerate() {
            if let StepStatus::Running { pid: owner, .. } = s.status
                && alive(owner)
            {
                return Err(format!("step {} is already running (pid {owner})", i + 1));
            }
        }
        if !matches!(self.state, EntryState::Triggered { .. }) {
            self.start(now);
        }
        let mut claimed = Vec::new();
        for (i, s) in self.steps.iter_mut().enumerate() {
            if !matches!(s.status, StepStatus::Ok { .. }) {
                s.status = StepStatus::Running { pid, since: now };
                claimed.push(i);
            }
        }
        Ok(claimed)
    }

    /// Records an attempt of step `i` held by `pid`. Returns `None` if `pid`
    /// does not hold it (the registration was replaced, forgotten or
    /// recovered meanwhile), in which case nothing changes.
    pub fn finish(
        &mut self,
        i: usize,
        pid: u32,
        result: RunResult,
        policy: &RetryPolicy,
        now: Ts,
    ) -> Option<Finished> {
        let step = self.steps.get_mut(i)?;
        match step.status {
            StepStatus::Running { pid: owner, .. } if owner == pid => {}
            _ => return None,
        }
        step.attempts += 1;
        let attempt = step.attempts;
        let outcome = if result.ok {
            step.status = StepStatus::Ok { at: now };
            Outcome::Ok
        } else if attempt >= policy.max_attempts {
            step.status = StepStatus::Failed { at: now };
            Outcome::GaveUp
        } else {
            let next_at = now.plus(policy.backoff(attempt));
            step.status = StepStatus::Retry { next_at };
            Outcome::Retry { next_at }
        };
        step.last = Some(result);
        let concluded = self.conclude(now);
        Some(Finished {
            outcome,
            attempt,
            concluded,
        })
    }

    /// Triggered → done/failed once every step has finished.
    fn conclude(&mut self, now: Ts) -> Option<EntryState> {
        let EntryState::Triggered { gone_at } = self.state else {
            return None;
        };
        if !self.steps.iter().all(|s| s.status.is_terminal()) {
            return None;
        }
        let all_ok = self
            .steps
            .iter()
            .all(|s| matches!(s.status, StepStatus::Ok { .. }));
        self.state = if all_ok {
            EntryState::Done { gone_at, at: now }
        } else {
            EntryState::Failed { gone_at, at: now }
        };
        Some(self.state.clone())
    }

    /// Running steps whose owner is no longer alive (a crashed daemon or a
    /// killed `gone run`). Each is finished as a failed, interrupted attempt,
    /// so the normal retry policy decides what happens next.
    pub fn stale_steps(&self, alive: impl Fn(u32) -> bool) -> Vec<(usize, u32)> {
        self.steps
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match s.status {
                StepStatus::Running { pid, .. } if !alive(pid) => Some((i, pid)),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: RetryPolicy = RetryPolicy {
        base: Dur(1000),
        max: Dur(5000),
        max_attempts: 3,
    };

    fn entry(steps: &[(u64, &str)]) -> Entry {
        Entry {
            id: "id1".into(),
            name: "wt".into(),
            dir: "/w/wt".into(),
            cwd: "/".into(),
            env: BTreeMap::new(),
            steps: steps
                .iter()
                .map(|(after, cmd)| Step {
                    cmd: Cmd::Shell(cmd.to_string()),
                    after: Dur(*after),
                    status: StepStatus::Pending,
                    attempts: 0,
                    last: None,
                })
                .collect(),
            registered_at: Ts(0),
            state: EntryState::Watching,
        }
    }

    fn result(ok: bool) -> RunResult {
        RunResult {
            at: Ts(0),
            ok,
            exit_code: Some(if ok { 0 } else { 1 }),
            error: None,
            took: Dur(1),
            output: None,
        }
    }

    #[test]
    fn triggers_only_once() {
        let mut e = entry(&[(0, "a")]);
        assert_eq!(
            e.next_runnable(Ts(100)),
            None,
            "watching entries run nothing"
        );
        assert!(e.trigger(Ts(100)));
        assert!(!e.trigger(Ts(200)), "second trigger must be refused");
        assert_eq!(e.state, EntryState::Triggered { gone_at: Ts(100) });
    }

    #[test]
    fn claimed_step_cannot_be_claimed_again() {
        let mut e = entry(&[(0, "a")]);
        e.trigger(Ts(0));
        assert!(e.claim(0, 7, Ts(0)));
        assert!(!e.claim(0, 8, Ts(0)));
        assert_eq!(e.next_runnable(Ts(0)), None);
    }

    #[test]
    fn delayed_step_waits_for_its_time_and_its_predecessor() {
        let mut e = entry(&[(0, "down"), (10_000, "volumes")]);
        e.trigger(Ts(1000));
        assert_eq!(e.next_runnable(Ts(1000)), Some(0));
        assert!(e.claim(0, 1, Ts(1000)));
        // Step 2 is not due, and step 1 is still running anyway.
        assert_eq!(e.next_runnable(Ts(20_000)), None);
        e.finish(0, 1, result(true), &POLICY, Ts(1500)).unwrap();
        assert_eq!(e.next_runnable(Ts(10_999)), None);
        assert_eq!(e.next_due(), Some((1, Ts(11_000))));
        assert_eq!(e.next_runnable(Ts(11_000)), Some(1));
        assert!(e.claim(1, 1, Ts(11_000)));
        let f = e.finish(1, 1, result(true), &POLICY, Ts(11_100)).unwrap();
        assert_eq!(f.outcome, Outcome::Ok);
        assert_eq!(
            f.concluded,
            Some(EntryState::Done {
                gone_at: Ts(1000),
                at: Ts(11_100)
            })
        );
    }

    #[test]
    fn failures_retry_with_capped_backoff_then_give_up() {
        let mut e = entry(&[(0, "flaky"), (0, "after")]);
        e.trigger(Ts(0));
        assert!(e.claim(0, 1, Ts(0)));
        let f = e.finish(0, 1, result(false), &POLICY, Ts(0)).unwrap();
        assert_eq!(f.outcome, Outcome::Retry { next_at: Ts(1000) });
        assert_eq!(e.next_runnable(Ts(999)), None, "backoff not elapsed");
        assert!(e.claim(0, 1, Ts(1000)));
        let f = e.finish(0, 1, result(false), &POLICY, Ts(1000)).unwrap();
        assert_eq!(f.outcome, Outcome::Retry { next_at: Ts(3000) });
        assert!(e.claim(0, 1, Ts(3000)));
        let f = e.finish(0, 1, result(false), &POLICY, Ts(3000)).unwrap();
        assert_eq!(f.outcome, Outcome::GaveUp);
        assert_eq!(f.attempt, 3);
        assert_eq!(f.concluded, None, "the next step still runs");
        assert_eq!(e.next_runnable(Ts(3000)), Some(1));
        assert!(e.claim(1, 1, Ts(3000)));
        let f = e.finish(1, 1, result(true), &POLICY, Ts(3001)).unwrap();
        assert!(matches!(f.concluded, Some(EntryState::Failed { .. })));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = RetryPolicy {
            base: Dur(30_000),
            max: Dur(600_000),
            max_attempts: 99,
        };
        let got: Vec<u64> = (1..=7).map(|n| p.backoff(n).0 / 1000).collect();
        assert_eq!(got, [30, 60, 120, 240, 480, 600, 600]);
        assert_eq!(p.backoff(200), Dur(600_000));
    }

    #[test]
    fn finish_from_a_non_owner_is_ignored() {
        let mut e = entry(&[(0, "a")]);
        e.trigger(Ts(0));
        e.claim(0, 1, Ts(0));
        assert_eq!(e.finish(0, 2, result(true), &POLICY, Ts(1)), None);
        assert!(matches!(
            e.steps[0].status,
            StepStatus::Running { pid: 1, .. }
        ));
    }

    #[test]
    fn stale_running_steps_are_found_and_retried() {
        let mut e = entry(&[(0, "a")]);
        e.trigger(Ts(0));
        e.claim(0, 42, Ts(0));
        assert!(e.stale_steps(|_| true).is_empty());
        assert_eq!(e.stale_steps(|p| p != 42), vec![(0, 42)]);
        let f = e.finish(0, 42, result(false), &POLICY, Ts(5)).unwrap();
        assert!(matches!(f.outcome, Outcome::Retry { .. }));
    }

    #[test]
    fn manual_run_claims_everything_and_refuses_live_owners() {
        let mut e = entry(&[(0, "a"), (86_400_000, "b")]);
        let got = e.claim_all(9, Ts(10), |_| true).unwrap();
        assert_eq!(got, vec![0, 1]);
        assert_eq!(e.state, EntryState::Triggered { gone_at: Ts(10) });
        assert!(e.claim_all(10, Ts(11), |_| true).is_err());
        // Both steps finish after one attempt each.
        for i in got {
            e.finish(i, 9, result(true), &RetryPolicy::ONCE, Ts(12))
                .unwrap();
        }
        assert!(matches!(e.state, EntryState::Done { .. }));
        // A done entry can be run again by hand.
        assert_eq!(e.claim_all(9, Ts(20), |_| false).unwrap(), vec![0, 1]);
    }

    #[test]
    fn manual_run_failure_is_final() {
        let mut e = entry(&[(0, "a")]);
        e.claim_all(9, Ts(0), |_| true).unwrap();
        let f = e
            .finish(0, 9, result(false), &RetryPolicy::ONCE, Ts(1))
            .unwrap();
        assert_eq!(f.outcome, Outcome::GaveUp);
        assert!(matches!(e.state, EntryState::Failed { .. }));
    }

    #[test]
    fn same_config_ignores_runtime_state() {
        let a = entry(&[(0, "a")]);
        let mut b = a.clone();
        b.id = "other".into();
        b.trigger(Ts(5));
        assert!(a.same_config(&b));
        b.steps[0].after = Dur(1);
        assert!(!a.same_config(&b));
    }

    #[test]
    fn registry_json_is_readable_and_round_trips() {
        let mut e = entry(&[(0, "a"), (86_400_000, "b")]);
        e.trigger(Ts::parse("2026-11-08T18:00:00Z").unwrap());
        let reg = Registry {
            version: REGISTRY_VERSION,
            entries: vec![e],
        };
        let json = serde_json::to_string_pretty(&reg).unwrap();
        assert!(json.contains(r#""after": "1d""#), "{json}");
        assert!(
            json.contains(r#""gone_at": "2026-11-08T18:00:00.000Z""#),
            "{json}"
        );
        assert!(json.contains(r#""shell": "a""#), "{json}");
        let back: Registry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reg);
    }

    #[test]
    fn display_quotes_only_when_needed() {
        let c = Cmd::Argv(vec![
            "docker".into(),
            "compose".into(),
            "-p".into(),
            "it's here".into(),
        ]);
        assert_eq!(c.display(), r"docker compose -p 'it'\''s here'");
        assert_eq!(c.program(), "docker");
        assert_eq!(Cmd::Shell("a | b".into()).display(), "sh -c 'a | b'");
        assert_eq!(Cmd::Shell("a | b".into()).program(), "sh");
    }
}
