//! The registry's data model and the per-entry run state machine.
//!
//! ```text
//!   Watching ──trigger(gone_at)──▶ Triggered{done, current} ──last step finishes──▶ Finished
//!      ▲                            │   ▲                                            │
//!      └──── gone watch (new id) ───┘   └── current: Waiting{due_at} ⇄ Running{claim}│
//!      └──────────────────────────── gone watch (new id) ◀───────────────────────────┘
//! ```
//!
//! Run state exists only inside `Triggered` and `Finished`; the step
//! configuration (`StepConfig`) carries none. Steps run strictly in order, so
//! a trigger is "the steps already finished" plus "the one current step",
//! which is either waiting for its due time or claimed by exactly one owner.
//! A `Triggered` entry always has a current step (finishing the last one goes
//! straight to `Finished`), and a `Watching` entry has no run state at all.
//!
//! Every method takes the current time as an argument; nothing here reads the
//! clock or touches the filesystem.

use crate::time::{Dur, Ts};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const REGISTRY_VERSION: u32 = 2;

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
    /// claims and results from the old one are recognised and dropped.
    pub id: String,
    pub name: String,
    /// Absolute, canonical at registration time.
    pub dir: PathBuf,
    /// Where steps run. Never inside `dir`.
    pub cwd: PathBuf,
    /// The steps' whole environment, besides `GONE_DIR` and `GONE_NAME`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub steps: Vec<StepConfig>,
    pub registered_at: Ts,
    #[serde(default)]
    pub state: EntryState,
}

/// What `gone watch` was asked to run. No run state lives here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepConfig {
    pub cmd: Cmd,
    /// Delay after the delete before this step is due.
    #[serde(default, skip_serializing_if = "Dur::is_zero")]
    pub after: Dur,
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
pub enum EntryState {
    #[default]
    Watching,
    Triggered {
        gone_at: Ts,
        /// Steps `0..done.len()` have finished, in order.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        done: Vec<StepRecord>,
        /// Step `done.len()`.
        current: Current,
    },
    Finished {
        gone_at: Ts,
        at: Ts,
        steps: Vec<StepRecord>,
    },
}

/// The one step of a trigger that has not finished yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Current {
    /// Attempts of this step that already failed.
    #[serde(default)]
    pub failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<RunResult>,
    pub phase: Phase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Phase {
    /// Runs when `due_at` is reached (its delay, or a retry backoff).
    Waiting { due_at: Ts },
    /// Claimed by one owner, who is running it.
    Running { claim: Claim },
}

/// Who runs an attempt. The registry is the only record of this; whether the
/// attempt's process is still alive is a lock held by the process itself
/// (see `store::RunFiles`), not the owner's pid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub owner: Owner,
    pub since: Ts,
    /// 1-based attempt number (`failures + 1` when claimed).
    pub attempt: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Owner {
    /// A `gone daemon`; `instance` is random per daemon start, so a restarted
    /// daemon with a reused pid still knows the claim is not its own.
    Daemon { pid: u32, instance: String },
    /// A foreground `gone run`.
    Manual { pid: u32 },
}

/// A step that finished: succeeded, or failed its last allowed attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRecord {
    pub attempts: u32,
    pub result: RunResult,
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

/// What finishing an attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Retry { next_at: Ts },
    GaveUp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub step: usize,
    pub outcome: Outcome,
    pub attempt: u32,
    /// `Some(all_ok)` when this attempt finished the whole trigger.
    pub concluded: Option<bool>,
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

    /// The argv to execute: the program itself, or `/bin/sh -c SCRIPT`.
    pub fn argv(&self) -> Vec<String> {
        match self {
            Cmd::Argv(a) => a.clone(),
            Cmd::Shell(s) => vec!["/bin/sh".into(), "-c".into(), s.clone()],
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

impl StepRecord {
    pub fn ok(&self) -> bool {
        self.result.ok
    }
}

impl EntryState {
    /// `watching`, `triggered`, `done` (every step succeeded) or `failed`.
    pub fn kind(&self) -> &'static str {
        match self {
            EntryState::Watching => "watching",
            EntryState::Triggered { .. } => "triggered",
            EntryState::Finished { steps, .. } if steps.iter().all(StepRecord::ok) => "done",
            EntryState::Finished { .. } => "failed",
        }
    }

    pub fn gone_at(&self) -> Option<Ts> {
        match *self {
            EntryState::Watching => None,
            EntryState::Triggered { gone_at, .. } | EntryState::Finished { gone_at, .. } => {
                Some(gone_at)
            }
        }
    }
}

impl Owner {
    pub fn pid(&self) -> u32 {
        match *self {
            Owner::Daemon { pid, .. } | Owner::Manual { pid } => pid,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Owner::Daemon { pid, .. } => format!("daemon pid {pid}"),
            Owner::Manual { pid } => format!("gone run pid {pid}"),
        }
    }
}

impl Entry {
    /// Same registration (what `gone watch` was asked for), ignoring state.
    pub fn same_config(&self, other: &Entry) -> bool {
        self.name == other.name
            && self.dir == other.dir
            && self.cwd == other.cwd
            && self.env == other.env
            && self.steps == other.steps
    }

    fn waiting(&self, i: usize, gone_at: Ts) -> Current {
        Current {
            failures: 0,
            last: None,
            phase: Phase::Waiting {
                due_at: gone_at.plus(self.steps[i].after),
            },
        }
    }

    /// A fresh trigger: step 0 waiting for its delay. An entry with no steps
    /// (never created by `gone watch`) finishes at once.
    fn start(&mut self, gone_at: Ts) {
        self.state = if self.steps.is_empty() {
            EntryState::Finished {
                gone_at,
                at: gone_at,
                steps: Vec::new(),
            }
        } else {
            EntryState::Triggered {
                gone_at,
                done: Vec::new(),
                current: self.waiting(0, gone_at),
            }
        };
    }

    /// Watching → triggered. Returns false (and changes nothing) from any
    /// other state, which is what keeps a deletion from running twice.
    pub fn trigger(&mut self, gone_at: Ts) -> bool {
        if self.state != EntryState::Watching {
            return false;
        }
        self.start(gone_at);
        true
    }

    /// The current step and its state, if triggered.
    pub fn current(&self) -> Option<(usize, &Current)> {
        match &self.state {
            EntryState::Triggered { done, current, .. } => Some((done.len(), current)),
            _ => None,
        }
    }

    /// The claim on the current step, if someone holds one.
    pub fn claim_held(&self) -> Option<(usize, &Claim)> {
        match self.current()? {
            (
                i,
                Current {
                    phase: Phase::Running { claim },
                    ..
                },
            ) => Some((i, claim)),
            _ => None,
        }
    }

    /// When the current step becomes due, if it is waiting.
    pub fn next_due(&self) -> Option<(usize, Ts)> {
        match self.current()? {
            (
                i,
                Current {
                    phase: Phase::Waiting { due_at },
                    ..
                },
            ) => Some((i, *due_at)),
            _ => None,
        }
    }

    /// The step to run now, if any: the current step, once it is due.
    pub fn runnable(&self, now: Ts) -> Option<(usize, u32)> {
        let (i, due) = self.next_due()?;
        let (_, cur) = self.current()?;
        (due <= now).then_some((i, cur.failures + 1))
    }

    /// Claims step `i` attempt `attempt` for `owner`, if that is exactly what
    /// is runnable now.
    pub fn claim(&mut self, i: usize, attempt: u32, owner: Owner, now: Ts) -> Option<Claim> {
        if self.runnable(now) != Some((i, attempt)) {
            return None;
        }
        self.set_running(owner, now)
    }

    fn set_running(&mut self, owner: Owner, now: Ts) -> Option<Claim> {
        let EntryState::Triggered { current, .. } = &mut self.state else {
            return None;
        };
        let claim = Claim {
            owner,
            since: now,
            attempt: current.failures + 1,
        };
        current.phase = Phase::Running {
            claim: claim.clone(),
        };
        Some(claim)
    }

    /// For a manual `gone run`: claims the current step now, ignoring its
    /// delay, starting a fresh trigger unless one is in progress. Fails if the
    /// current step is already claimed (by anyone; the caller recovers dead
    /// claims first).
    pub fn claim_manual(&mut self, owner: Owner, now: Ts) -> Result<(usize, Claim), String> {
        if let Some((i, claim)) = self.claim_held() {
            return Err(format!(
                "step {} is already being run by {}",
                i + 1,
                claim.owner.describe()
            ));
        }
        if !matches!(self.state, EntryState::Triggered { .. }) {
            self.start(now);
        }
        let i = self
            .current()
            .map(|(i, _)| i)
            .ok_or_else(|| "nothing to run".to_string())?;
        let claim = self
            .set_running(owner, now)
            .ok_or_else(|| "nothing to run".to_string())?;
        Ok((i, claim))
    }

    /// Records an attempt. `claim` must be the claim currently held (the
    /// registration was replaced, forgotten or recovered meanwhile if not, and
    /// then nothing changes).
    pub fn finish(
        &mut self,
        claim: &Claim,
        result: RunResult,
        policy: &RetryPolicy,
        now: Ts,
    ) -> Option<Finished> {
        let (step, held) = self.claim_held()?;
        if held != claim {
            return None;
        }
        let nsteps = self.steps.len();
        let next_after = self.steps.get(step + 1).map(|s| s.after);
        let EntryState::Triggered {
            gone_at,
            done,
            current,
        } = &mut self.state
        else {
            unreachable!("claim_held implies Triggered")
        };
        let gone_at = *gone_at;
        let attempt = claim.attempt;
        let (outcome, finished_step) = if result.ok {
            (Outcome::Ok, true)
        } else if attempt >= policy.max_attempts {
            (Outcome::GaveUp, true)
        } else {
            let next_at = now.plus(policy.backoff(attempt));
            current.failures = attempt;
            current.last = Some(result.clone());
            current.phase = Phase::Waiting { due_at: next_at };
            (Outcome::Retry { next_at }, false)
        };
        let mut concluded = None;
        if finished_step {
            done.push(StepRecord {
                attempts: attempt,
                result,
            });
            if done.len() < nsteps {
                *current = Current {
                    failures: 0,
                    last: None,
                    phase: Phase::Waiting {
                        due_at: gone_at.plus(next_after.unwrap_or_default()),
                    },
                };
            } else {
                let steps = std::mem::take(done);
                concluded = Some(steps.iter().all(StepRecord::ok));
                self.state = EntryState::Finished {
                    gone_at,
                    at: now,
                    steps,
                };
            }
        }
        Some(Finished {
            step,
            outcome,
            attempt,
            concluded,
        })
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

    fn daemon() -> Owner {
        Owner::Daemon {
            pid: 1,
            instance: "a".into(),
        }
    }

    fn entry(afters: &[u64]) -> Entry {
        Entry {
            id: "id1".into(),
            name: "wt".into(),
            dir: "/w/wt".into(),
            cwd: "/".into(),
            env: BTreeMap::new(),
            steps: afters
                .iter()
                .map(|a| StepConfig {
                    cmd: Cmd::Shell(format!("step after {a}")),
                    after: Dur(*a),
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
        let mut e = entry(&[0]);
        assert_eq!(e.runnable(Ts(100)), None, "watching entries run nothing");
        assert!(e.trigger(Ts(100)));
        assert!(!e.trigger(Ts(200)), "second trigger must be refused");
        assert_eq!(e.state.gone_at(), Some(Ts(100)));
    }

    #[test]
    fn claimed_step_cannot_be_claimed_again() {
        let mut e = entry(&[0]);
        e.trigger(Ts(0));
        assert!(e.claim(0, 1, daemon(), Ts(0)).is_some());
        assert!(e.claim(0, 1, daemon(), Ts(0)).is_none());
        assert!(e.claim_manual(Owner::Manual { pid: 2 }, Ts(0)).is_err());
        assert_eq!(e.runnable(Ts(0)), None);
    }

    #[test]
    fn delayed_step_waits_for_its_time() {
        let mut e = entry(&[0, 10_000]);
        e.trigger(Ts(1000));
        let c = e.claim(0, 1, daemon(), Ts(1000)).unwrap();
        e.finish(&c, result(true), &POLICY, Ts(1500)).unwrap();
        assert_eq!(e.runnable(Ts(10_999)), None);
        assert_eq!(e.next_due(), Some((1, Ts(11_000))));
        let c = e.claim(1, 1, daemon(), Ts(11_000)).unwrap();
        let f = e.finish(&c, result(true), &POLICY, Ts(11_100)).unwrap();
        assert_eq!((f.outcome, f.concluded), (Outcome::Ok, Some(true)));
        assert_eq!(e.state.kind(), "done");
    }

    #[test]
    fn failures_retry_with_capped_backoff_then_give_up() {
        let mut e = entry(&[0, 0]);
        e.trigger(Ts(0));
        let c = e.claim(0, 1, daemon(), Ts(0)).unwrap();
        let f = e.finish(&c, result(false), &POLICY, Ts(0)).unwrap();
        assert_eq!(f.outcome, Outcome::Retry { next_at: Ts(1000) });
        assert_eq!(e.runnable(Ts(999)), None, "backoff not elapsed");
        assert!(
            e.claim(0, 1, daemon(), Ts(1000)).is_none(),
            "attempt 1 is over"
        );
        let c = e.claim(0, 2, daemon(), Ts(1000)).unwrap();
        let f = e.finish(&c, result(false), &POLICY, Ts(1000)).unwrap();
        assert_eq!(f.outcome, Outcome::Retry { next_at: Ts(3000) });
        let c = e.claim(0, 3, daemon(), Ts(3000)).unwrap();
        let f = e.finish(&c, result(false), &POLICY, Ts(3000)).unwrap();
        assert_eq!(
            (f.outcome, f.attempt, f.concluded),
            (Outcome::GaveUp, 3, None)
        );
        let c = e.claim(1, 1, daemon(), Ts(3000)).unwrap();
        let f = e.finish(&c, result(true), &POLICY, Ts(3001)).unwrap();
        assert_eq!(f.concluded, Some(false));
        assert_eq!(e.state.kind(), "failed");
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
    fn finish_with_a_stale_claim_is_ignored() {
        let mut e = entry(&[0]);
        e.trigger(Ts(0));
        let c = e.claim(0, 1, daemon(), Ts(0)).unwrap();
        let mut other = c.clone();
        other.owner = Owner::Manual { pid: 9 };
        assert_eq!(e.finish(&other, result(true), &POLICY, Ts(1)), None);
        assert!(e.claim_held().is_some());
        assert!(e.finish(&c, result(true), &POLICY, Ts(1)).is_some());
        assert_eq!(
            e.finish(&c, result(true), &POLICY, Ts(2)),
            None,
            "only once"
        );
    }

    #[test]
    fn manual_run_claims_ignoring_delay_and_restarts_finished_entries() {
        let m = Owner::Manual { pid: 9 };
        let mut e = entry(&[0, 86_400_000]);
        let (i, c) = e.claim_manual(m.clone(), Ts(10)).unwrap();
        assert_eq!(i, 0);
        e.finish(&c, result(true), &RetryPolicy::ONCE, Ts(11))
            .unwrap();
        let (i, c) = e.claim_manual(m.clone(), Ts(12)).unwrap();
        assert_eq!(i, 1, "the day-long delay does not apply to a manual run");
        let f = e
            .finish(&c, result(false), &RetryPolicy::ONCE, Ts(13))
            .unwrap();
        assert_eq!((f.outcome, f.concluded), (Outcome::GaveUp, Some(false)));
        // A finished entry can be run again by hand: a fresh trigger.
        let (i, _) = e.claim_manual(m, Ts(20)).unwrap();
        assert_eq!(i, 0);
        assert_eq!(e.state.gone_at(), Some(Ts(20)));
    }

    #[test]
    fn same_config_ignores_state() {
        let a = entry(&[0]);
        let mut b = a.clone();
        b.id = "other".into();
        b.trigger(Ts(5));
        assert!(a.same_config(&b));
        b.steps[0].after = Dur(1);
        assert!(!a.same_config(&b));
    }

    #[test]
    fn registry_json_is_readable_and_round_trips() {
        let mut e = entry(&[0, 86_400_000]);
        e.trigger(Ts::parse("2026-11-08T18:00:00Z").unwrap());
        e.claim(0, 1, daemon(), Ts::parse("2026-11-08T18:00:00Z").unwrap());
        let reg = Registry {
            version: REGISTRY_VERSION,
            entries: vec![e],
        };
        let json = serde_json::to_string_pretty(&reg).unwrap();
        for want in [
            r#""after": "1d""#,
            r#""gone_at": "2026-11-08T18:00:00.000Z""#,
            r#""kind": "running""#,
            r#""instance": "a""#,
        ] {
            assert!(json.contains(want), "{want} missing in {json}");
        }
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
        assert_eq!(
            Cmd::Shell("a | b".into()).argv(),
            ["/bin/sh", "-c", "a | b"]
        );
    }
}
