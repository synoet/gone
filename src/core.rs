//! The daemon's decisions, with no IO: which directories to check, what a
//! check means, which steps to claim, and how to settle claims whose owner is
//! gone. `daemon.rs` is the shell around this that does the checks, runs the
//! processes and holds the locks; the property tests in
//! `tests/machines.rs` drive this same code with a simulated world.
//!
//! In-memory state here is only what may be lost on restart: each entry's
//! [`Presence`] belief, the next sweep time and the lifecycle. Who runs what
//! is read from the registry claims every time.

use crate::model::{Claim, Entry, EntryState, Finished, Owner, Registry, RetryPolicy, RunResult};
use crate::presence::{self, Notice, Presence, Probe};
use crate::time::{Dur, Ts};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lifecycle {
    /// Before the first pass: every entry is checked ("startup").
    Starting,
    Steady,
    /// Stopping: claim nothing new; exit once our own runs finish or at the
    /// deadline. Runs still going then keep going and are adopted by the next
    /// daemon.
    Draining {
        deadline: Ts,
    },
}

#[derive(Debug, Clone)]
pub struct Core {
    pub settle: Dur,
    pub sweep_interval: Dur,
    pub retry: RetryPolicy,
    pub me: Owner,
    pub lifecycle: Lifecycle,
    presence: HashMap<String, Presence>,
    next_sweep: Ts,
}

/// A directory to check this pass, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub id: String,
    pub dir: PathBuf,
    pub via: &'static str,
}

/// How a claim's process looks from outside its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimProbe {
    /// The attempt's process still holds its run lock.
    Running,
    /// It finished and left its exit status.
    Exited(RunResult),
    /// No process holds the lock and no status was left: it never started,
    /// or died without finishing (e.g. the machine went down).
    Lost,
}

/// Something worth a log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    Presence {
        id: String,
        via: &'static str,
        notice: Notice,
    },
    Triggered {
        id: String,
        gone_at: Ts,
    },
}

impl Core {
    pub fn new(me: Owner, settle: Dur, sweep_interval: Dur, retry: RetryPolicy) -> Core {
        Core {
            settle,
            sweep_interval,
            retry,
            me,
            lifecycle: Lifecycle::Starting,
            presence: HashMap::new(),
            next_sweep: Ts(0),
        }
    }

    pub fn presence(&self, id: &str) -> Option<&Presence> {
        self.presence.get(id)
    }

    /// Run a sweep at the next pass (events were lost or overflowed).
    pub fn force_sweep(&mut self) {
        self.next_sweep = Ts(0);
    }

    /// Which watched directories to check now. `touched` are paths from
    /// filesystem events since the last pass; an event on a path touches
    /// every entry at or below it (whole components, so `/a/b` does not touch
    /// `/a/bc`).
    pub fn plan_checks(&mut self, reg: &Registry, touched: &[PathBuf], now: Ts) -> Vec<Check> {
        self.presence.retain(|id, _| {
            reg.entries
                .iter()
                .any(|e| &e.id == id && e.state == EntryState::Watching)
        });
        let sweep = now >= self.next_sweep;
        if sweep {
            self.next_sweep = now.plus(self.sweep_interval);
        }
        let starting = self.lifecycle == Lifecycle::Starting;
        let mut out = Vec::new();
        for e in reg
            .entries
            .iter()
            .filter(|e| e.state == EntryState::Watching)
        {
            let state = self.presence.get(&e.id);
            let is_touched = touched_by(&e.dir, touched);
            if !presence::wants_check(state, is_touched, sweep) {
                continue;
            }
            let via = match state {
                _ if starting => "startup",
                None => "registered",
                Some(Presence::Settling { .. }) => "settling",
                Some(Presence::Unknown { .. }) => "retry_check",
                Some(Presence::Present) if is_touched => "inotify",
                Some(Presence::Present) => "sweep",
            };
            out.push(Check {
                id: e.id.clone(),
                dir: e.dir.clone(),
                via,
            });
        }
        out
    }

    /// Feeds check results through the presence machine; triggers entries
    /// whose directory stayed gone for the settle delay.
    pub fn apply_checks(
        &mut self,
        reg: &mut Registry,
        results: Vec<(Check, Probe)>,
        now: Ts,
    ) -> Vec<Note> {
        let mut notes = Vec::new();
        for (check, probe) in results {
            let Some(e) = reg
                .entries
                .iter_mut()
                .find(|e| e.id == check.id && e.state == EntryState::Watching)
            else {
                self.presence.remove(&check.id);
                continue;
            };
            let o = presence::observe(self.presence.get(&e.id), probe, now, self.settle);
            if let Some(notice) = o.notice {
                notes.push(Note::Presence {
                    id: e.id.clone(),
                    via: check.via,
                    notice,
                });
            }
            match o.next {
                Some(p) => {
                    self.presence.insert(e.id.clone(), p);
                }
                None => {
                    self.presence.remove(&e.id);
                }
            }
            if let Some(gone_at) = o.trigger
                && e.trigger(gone_at)
            {
                notes.push(Note::Triggered {
                    id: e.id.clone(),
                    gone_at,
                });
            }
        }
        notes
    }

    /// Steps due now that we should claim, as `(entry id, step, attempt)`.
    /// Nothing while draining.
    pub fn due(&self, reg: &Registry, now: Ts) -> Vec<(String, usize, u32)> {
        if matches!(self.lifecycle, Lifecycle::Draining { .. }) {
            return Vec::new();
        }
        reg.entries
            .iter()
            .filter_map(|e| e.runnable(now).map(|(i, a)| (e.id.clone(), i, a)))
            .collect()
    }

    /// Claims a due step for this daemon. Returns the entry (for running it)
    /// and the claim, or `None` if it is no longer due exactly so.
    pub fn claim(
        &self,
        reg: &mut Registry,
        id: &str,
        step: usize,
        attempt: u32,
        now: Ts,
    ) -> Option<(Entry, Claim)> {
        let e = reg.entries.iter_mut().find(|e| e.id == id)?;
        let claim = e.claim(step, attempt, self.me.clone(), now)?;
        Some((e.clone(), claim))
    }

    /// Records a finished attempt of one of our claims.
    pub fn finish(
        &self,
        reg: &mut Registry,
        id: &str,
        claim: &Claim,
        result: RunResult,
        now: Ts,
    ) -> Option<Finished> {
        let e = reg.entries.iter_mut().find(|e| e.id == id)?;
        e.finish(claim, result, &self.retry, now)
    }

    /// Claims held by us (derived from the registry, the only record).
    pub fn mine(&self, reg: &Registry) -> Vec<(String, usize, Claim)> {
        self.claims(reg, true)
    }

    /// Claims held by anyone else: an earlier daemon, or a `gone run`.
    pub fn foreign(&self, reg: &Registry) -> Vec<(String, usize, Claim)> {
        self.claims(reg, false)
    }

    fn claims(&self, reg: &Registry, mine: bool) -> Vec<(String, usize, Claim)> {
        reg.entries
            .iter()
            .filter_map(|e| {
                let (i, c) = e.claim_held()?;
                ((c.owner == self.me) == mine).then(|| (e.id.clone(), i, c.clone()))
            })
            .collect()
    }

    /// Settles a claim we don't own, once its process is known to be done.
    pub fn resolve(
        &self,
        reg: &mut Registry,
        id: &str,
        claim: &Claim,
        probe: ClaimProbe,
        now: Ts,
    ) -> Option<Finished> {
        let e = reg.entries.iter_mut().find(|e| e.id == id)?;
        resolve(e, claim, probe, &self.retry, now)
    }

    /// Called after each pass.
    pub fn end_pass(&mut self) {
        if self.lifecycle == Lifecycle::Starting {
            self.lifecycle = Lifecycle::Steady;
        }
    }

    pub fn drain(&mut self, now: Ts, grace: Dur) {
        if !matches!(self.lifecycle, Lifecycle::Draining { .. }) {
            self.lifecycle = Lifecycle::Draining {
                deadline: now.plus(grace),
            };
        }
    }

    pub fn should_exit(&self, reg: &Registry, now: Ts) -> bool {
        match self.lifecycle {
            Lifecycle::Draining { deadline } => now >= deadline || self.mine(reg).is_empty(),
            _ => false,
        }
    }

    /// When the next pass must happen at the latest (a settle deadline or a
    /// step coming due), if sooner than the caller's tick.
    pub fn next_wakeup(&self, reg: &Registry) -> Option<Ts> {
        let settles = self.presence.values().filter_map(|p| match p {
            Presence::Settling { deadline, .. } => Some(*deadline),
            _ => None,
        });
        let dues = reg
            .entries
            .iter()
            .filter_map(|e| e.next_due().map(|(_, t)| t));
        settles.chain(dues).chain([self.next_sweep]).min()
    }
}

/// Whether an event on any of `touched` concerns `dir` (it is `dir` or an
/// ancestor of it, compared by whole path components).
pub fn touched_by(dir: &Path, touched: &[PathBuf]) -> bool {
    touched.iter().any(|p| dir.starts_with(p))
}

/// Every proper ancestor of `dir`, nearest first: the paths to watch.
pub fn ancestors(dir: &Path) -> impl Iterator<Item = &Path> {
    dir.ancestors().skip(1)
}

/// Settles a claim whose owner is gone, from what its process left behind.
/// `Running` changes nothing; `Exited` records the real result (no re-run);
/// `Lost` records an interrupted attempt, retried under `policy`.
pub fn resolve(
    e: &mut Entry,
    claim: &Claim,
    probe: ClaimProbe,
    policy: &RetryPolicy,
    now: Ts,
) -> Option<Finished> {
    let result = match probe {
        ClaimProbe::Running => return None,
        ClaimProbe::Exited(r) => r,
        ClaimProbe::Lost => RunResult {
            at: claim.since,
            ok: false,
            exit_code: None,
            error: Some(format!(
                "interrupted: {} stopped and the step left no exit status",
                claim.owner.describe()
            )),
            took: now.since(claim.since),
            output: None,
        },
    };
    e.finish(claim, result, policy, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Cmd, StepConfig};

    fn me() -> Owner {
        Owner::Daemon {
            pid: 1,
            instance: "i1".into(),
        }
    }

    fn reg(dirs: &[&str]) -> Registry {
        Registry {
            version: crate::model::REGISTRY_VERSION,
            entries: dirs
                .iter()
                .enumerate()
                .map(|(n, d)| Entry {
                    id: format!("e{n}"),
                    name: format!("e{n}"),
                    dir: d.into(),
                    cwd: "/".into(),
                    env: Default::default(),
                    steps: vec![StepConfig {
                        cmd: Cmd::Argv(vec!["true".into()]),
                        after: Dur::ZERO,
                    }],
                    registered_at: Ts(0),
                    state: EntryState::Watching,
                })
                .collect(),
        }
    }

    fn core() -> Core {
        Core::new(
            me(),
            Dur(1000),
            Dur(60_000),
            RetryPolicy {
                base: Dur(100),
                max: Dur(1000),
                max_attempts: 2,
            },
        )
    }

    #[test]
    fn events_touch_whole_components_only() {
        let t = |p: &str| vec![PathBuf::from(p)];
        assert!(touched_by(Path::new("/a/b"), &t("/a/b")));
        assert!(touched_by(Path::new("/a/b"), &t("/a")));
        assert!(!touched_by(Path::new("/a/bc"), &t("/a/b")));
        assert!(!touched_by(Path::new("/a"), &t("/a/b")));
        let anc: Vec<_> = ancestors(Path::new("/a/b/c")).collect();
        assert_eq!(anc, [Path::new("/a/b"), Path::new("/a"), Path::new("/")]);
    }

    #[test]
    fn startup_checks_everything_then_only_what_is_touched() {
        let mut c = core();
        let r = reg(&["/w/a", "/w/b"]);
        let checks = c.plan_checks(&r, &[], Ts(0));
        assert_eq!(
            checks.iter().map(|c| c.via).collect::<Vec<_>>(),
            ["startup", "startup"]
        );
        let mut r2 = r.clone();
        let res = checks.into_iter().map(|c| (c, Probe::Present)).collect();
        c.apply_checks(&mut r2, res, Ts(0));
        c.end_pass();
        assert!(c.plan_checks(&r2, &[], Ts(10)).is_empty());
        let checks = c.plan_checks(&r2, &[PathBuf::from("/w/b")], Ts(20));
        assert_eq!(checks.len(), 1);
        assert_eq!((checks[0].id.as_str(), checks[0].via), ("e1", "inotify"));
        // The sweep comes round on its own.
        let checks = c.plan_checks(&r2, &[], Ts(60_000));
        assert_eq!(checks.len(), 2);
        assert!(checks.iter().all(|c| c.via == "sweep"));
    }

    #[test]
    fn delete_settles_then_triggers_and_claims_flow_through_the_registry() {
        let mut c = core();
        let mut r = reg(&["/w/a"]);
        let ch = c.plan_checks(&r, &[], Ts(0));
        c.apply_checks(
            &mut r,
            ch.into_iter().map(|c| (c, Probe::Absent)).collect(),
            Ts(0),
        );
        c.end_pass();
        assert_eq!(r.entries[0].state, EntryState::Watching);
        let ch = c.plan_checks(&r, &[], Ts(1000));
        assert_eq!(ch[0].via, "settling");
        let notes = c.apply_checks(
            &mut r,
            ch.into_iter().map(|c| (c, Probe::Absent)).collect(),
            Ts(1000),
        );
        assert!(notes.contains(&Note::Triggered {
            id: "e0".into(),
            gone_at: Ts(0)
        }));
        let due = c.due(&r, Ts(1000));
        assert_eq!(due, [("e0".to_string(), 0, 1)]);
        let (_, claim) = c.claim(&mut r, "e0", 0, 1, Ts(1000)).unwrap();
        assert_eq!(c.mine(&r).len(), 1);
        assert!(c.foreign(&r).is_empty());

        // After a restart the claim is foreign; an exit status settles it.
        let mut c2 = Core::new(
            Owner::Daemon {
                pid: 1,
                instance: "i2".into(),
            },
            c.settle,
            c.sweep_interval,
            c.retry,
        );
        assert!(c2.mine(&r).is_empty());
        let (id, _, held) = c2.foreign(&r).pop().unwrap();
        assert_eq!(held, claim);
        assert_eq!(
            c2.resolve(&mut r, &id, &held, ClaimProbe::Running, Ts(2000)),
            None
        );
        let ok = RunResult {
            at: Ts(1000),
            ok: true,
            exit_code: Some(0),
            error: None,
            took: Dur(5),
            output: None,
        };
        let f = c2
            .resolve(&mut r, &id, &held, ClaimProbe::Exited(ok), Ts(3000))
            .unwrap();
        assert_eq!(f.concluded, Some(true));
        c2.end_pass();
        c2.drain(Ts(3000), Dur(1000));
        assert!(c2.should_exit(&r, Ts(3000)));
    }

    #[test]
    fn lost_claims_become_failed_attempts() {
        let mut r = reg(&["/w/a"]);
        r.entries[0].trigger(Ts(0));
        let claim = r.entries[0].claim(0, 1, me(), Ts(0)).unwrap();
        let c = core();
        let f = resolve(
            &mut r.entries[0],
            &claim,
            ClaimProbe::Lost,
            &c.retry,
            Ts(10),
        )
        .unwrap();
        assert!(matches!(f.outcome, crate::model::Outcome::Retry { .. }));
        let (_, cur) = r.entries[0].current().unwrap();
        assert!(
            cur.last
                .as_ref()
                .unwrap()
                .error
                .as_ref()
                .unwrap()
                .starts_with("interrupted")
        );
    }

    #[test]
    fn draining_claims_nothing() {
        let mut c = core();
        let mut r = reg(&["/w/a"]);
        r.entries[0].trigger(Ts(0));
        assert_eq!(c.due(&r, Ts(0)).len(), 1);
        c.drain(Ts(0), Dur(10));
        assert!(c.due(&r, Ts(0)).is_empty());
    }
}
