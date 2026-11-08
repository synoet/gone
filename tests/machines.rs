//! Property tests of gone's state machines with proptest-state-machine.
//!
//! The reference model is the world: for two watched directories, whether
//! each is present, absent or erroring, since when it has been absent, what
//! is registered, and the clock. The system under test is the real daemon
//! core (`gone::core::Core`) and the real registry model, driven the way
//! `daemon.rs` drives them, against a simulated filesystem, simulated step
//! processes and a simulated clock. Nothing touches real folders, threads or
//! time.
//!
//! Every action is followed by one daemon pass. Invariants are asserted at
//! the moment things happen (trigger, spawn) and after every action.

use gone::core::{ClaimProbe, Core, Note};
use gone::migrate;
use gone::model::{
    Claim, Cmd, Entry, EntryState, Owner, REGISTRY_VERSION, Registry, RetryPolicy, RunResult,
    StepConfig,
};
use gone::presence::{Notice, Presence, Probe};
use gone::time::{Dur, Ts};
use proptest::prelude::*;
use proptest::test_runner::Config;
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest, prop_state_machine};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const SLOTS: usize = 2;
const SETTLE: u64 = 2_000;
const SWEEP: u64 = 10_000;
const RETRY: RetryPolicy = RetryPolicy {
    base: Dur(1_000),
    max: Dur(4_000),
    max_attempts: 3,
};
const MANUAL: Owner = Owner::Manual { pid: 7 };

fn slot_dir(slot: usize) -> PathBuf {
    PathBuf::from(format!("/w/slot{slot}"))
}

// ---------------------------------------------------------------- reference

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fs {
    Present,
    Absent,
    /// A check fails (EACCES, EIO, …).
    Error,
}

#[derive(Debug, Clone)]
pub struct World {
    now: u64,
    fs: [Fs; SLOTS],
    /// Since when the directory has been absent without interruption.
    absent_since: [Option<u64>; SLOTS],
    /// Last time an event about this slot was lost.
    lost_event_at: [Option<u64>; SLOTS],
    /// The registered step delays, if registered.
    registered: [Option<Vec<u64>>; SLOTS],
}

#[derive(Debug, Clone)]
pub enum Action {
    /// The directory changes; the inotify event is delivered or lost.
    SetDir {
        slot: usize,
        to: Fs,
        delivered: bool,
    },
    /// Time passes.
    Advance(u64),
    /// The step process running for this slot exits.
    Finish {
        slot: usize,
        ok: bool,
    },
    /// The step process vanishes without an exit status (a reboot).
    Lose {
        slot: usize,
    },
    /// The daemon dies and starts again: in-memory state is lost.
    Restart,
    /// `gone watch` with these step delays (register or replace).
    Watch {
        slot: usize,
        afters: Vec<u64>,
    },
    Forget {
        slot: usize,
    },
    ManualRun {
        slot: usize,
    },
    /// A late or duplicated result for this slot's last finished attempt
    /// arrives (a racing process): it must be ignored.
    Duplicate {
        slot: usize,
    },
}

fn delays() -> impl Strategy<Value = u64> {
    prop_oneof![
        4 => Just(0u64),
        4 => 0u64..20_000,
        // Absurd but accepted by `--after`: far-future due times must still
        // serialize and read back.
        1 => 0u64..=u64::MAX,
    ]
}

fn advance() -> impl Strategy<Value = u64> {
    prop_oneof![
        6 => 0u64..3_000,
        3 => 0u64..30_000,
        1 => 0u64..1_000_000_000,
    ]
}

pub struct Model;

impl ReferenceStateMachine for Model {
    type State = World;
    type Transition = Action;

    fn init_state() -> BoxedStrategy<World> {
        Just(World {
            now: 1_000_000,
            fs: [Fs::Present; SLOTS],
            absent_since: [None; SLOTS],
            lost_event_at: [None; SLOTS],
            registered: [None, None],
        })
        .boxed()
    }

    fn transitions(w: &World) -> BoxedStrategy<Action> {
        let slot = 0..SLOTS;
        let fs = prop_oneof![
            3 => Just(Fs::Absent),
            3 => Just(Fs::Present),
            1 => Just(Fs::Error)
        ];
        let mut options: Vec<(u32, BoxedStrategy<Action>)> = vec![
            (
                4,
                (slot.clone(), fs, prop::bool::weighted(0.8))
                    .prop_map(|(slot, to, delivered)| Action::SetDir {
                        slot,
                        to,
                        delivered,
                    })
                    .boxed(),
            ),
            (6, advance().prop_map(Action::Advance).boxed()),
            (
                3,
                (slot.clone(), prop::bool::weighted(0.7))
                    .prop_map(|(slot, ok)| Action::Finish { slot, ok })
                    .boxed(),
            ),
            (
                1,
                slot.clone().prop_map(|slot| Action::Lose { slot }).boxed(),
            ),
            (1, Just(Action::Restart).boxed()),
            (
                1,
                slot.clone()
                    .prop_map(|slot| Action::Forget { slot })
                    .boxed(),
            ),
            (
                1,
                slot.clone()
                    .prop_map(|slot| Action::ManualRun { slot })
                    .boxed(),
            ),
            (1, slot.prop_map(|slot| Action::Duplicate { slot }).boxed()),
        ];
        // `gone watch` needs the directory to exist: only offer it then.
        let present: Vec<usize> = (0..SLOTS).filter(|&i| w.fs[i] == Fs::Present).collect();
        if !present.is_empty() {
            options.push((
                2,
                (
                    prop::sample::select(present),
                    prop::collection::vec(delays(), 1..4),
                )
                    .prop_map(|(slot, afters)| Action::Watch { slot, afters })
                    .boxed(),
            ));
        }
        proptest::strategy::Union::new_weighted(options).boxed()
    }

    fn preconditions(w: &World, a: &Action) -> bool {
        match a {
            // `gone watch` refuses a directory that doesn't exist.
            Action::Watch { slot, .. } => w.fs[*slot] == Fs::Present,
            _ => true,
        }
    }

    fn apply(mut w: World, a: &Action) -> World {
        match a {
            Action::SetDir {
                slot,
                to,
                delivered,
            } => {
                w.fs[*slot] = *to;
                w.absent_since[*slot] = match to {
                    Fs::Absent => w.absent_since[*slot].or(Some(w.now)),
                    _ => None,
                };
                if !delivered {
                    w.lost_event_at[*slot] = Some(w.now);
                }
            }
            Action::Advance(ms) => w.now = w.now.saturating_add(*ms),
            Action::Watch { slot, afters } => w.registered[*slot] = Some(afters.clone()),
            Action::Forget { slot } => w.registered[*slot] = None,
            _ => {}
        }
        w
    }
}

// ---------------------------------------------------------------- the SUT

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proc {
    Running,
    Exited(bool),
    Lost,
}

type Key = (String, u32, usize, u32);

#[derive(Debug)]
struct Spawn {
    key: Key,
}

pub struct Sut {
    now: Ts,
    core: Core,
    instance: u32,
    reg: Registry,
    slots: [Option<String>; SLOTS],
    next_id: u32,
    procs: HashMap<Key, (Owner, Proc)>,
    spawns: Vec<Spawn>,
    dead: HashSet<String>,
    touched: Vec<PathBuf>,
    /// A `gone run` in progress: (slot, entry id).
    manual: Option<(usize, String)>,
    /// Every check result, per entry: (time, probe).
    probes: HashMap<String, Vec<(Ts, Probe)>>,
    /// Completed daemon passes.
    passes: u64,
    /// The last recorded attempt per entry, to replay as a stale result.
    last_finished: HashMap<String, (Claim, RunResult)>,
    /// What happened, for the coverage report (`GONE_PROPTEST_STATS=file`).
    stats: std::collections::BTreeMap<&'static str, u64>,
}

fn daemon(instance: u32) -> Owner {
    // The same pid every time: a restarted daemon may reuse its pid.
    Owner::Daemon {
        pid: 100,
        instance: format!("i{instance}"),
    }
}

fn result(ok: bool, now: Ts) -> RunResult {
    RunResult {
        at: now,
        ok,
        exit_code: Some(if ok { 0 } else { 1 }),
        error: (!ok).then(|| "exit status 1".into()),
        took: Dur(1),
        output: None,
    }
}

impl Sut {
    fn count(&mut self, what: &'static str) {
        *self.stats.entry(what).or_default() += 1;
    }

    fn new_core(instance: u32) -> Core {
        Core::new(daemon(instance), Dur(SETTLE), Dur(SWEEP), RETRY)
    }

    /// Writes the registry and reads it back, as the daemon does through
    /// `Store`: it must survive the round trip unchanged.
    fn persist(&mut self) {
        let json = serde_json::to_string_pretty(&self.reg).expect("registry serializes");
        let back = migrate::parse(&json).unwrap_or_else(|e| {
            panic!("registry does not read back: {e:#}\n{json}");
        });
        assert_eq!(back, self.reg, "registry changed in a round trip");
        assert_eq!(back.version, REGISTRY_VERSION);
        self.reg = back;
    }

    fn entry(&self, slot: usize) -> Option<&Entry> {
        let id = self.slots[slot].as_ref()?;
        self.reg.entries.iter().find(|e| &e.id == id)
    }

    fn entry_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.reg.entries.iter_mut().find(|e| e.id == id)
    }

    fn slot_of(&self, id: &str) -> Option<usize> {
        self.slots.iter().position(|s| s.as_deref() == Some(id))
    }

    fn probe(&mut self, w: &World, id: &str) -> Probe {
        let slot = self.slot_of(id).expect("checks only registered entries");
        let p = match w.fs[slot] {
            Fs::Present => Probe::Present,
            Fs::Absent => Probe::Absent,
            Fs::Error => Probe::Error("EACCES".into()),
        };
        self.probes
            .entry(id.to_string())
            .or_default()
            .push((self.now, p.clone()));
        p
    }

    /// Starts a process for a claim; asserts the at-most-once rules.
    fn spawn(&mut self, id: &str, step: usize, claim: &Claim) {
        assert!(
            !self.dead.contains(id),
            "a replaced or forgotten registration ran"
        );
        let e = self.reg.entries.iter().find(|e| e.id == id).unwrap();
        let gone_at = e.state.gone_at().unwrap();
        let trigger = e.state.trigger().unwrap();
        let delayed = e.steps[step].after.0 > 0;
        let manual = claim.owner == MANUAL;
        if !manual {
            let earliest = gone_at.plus(e.steps[step].after);
            assert!(
                self.now >= earliest,
                "step {step} ran at {:?}, before its delay ended at {earliest:?}",
                self.now
            );
        }
        for s in self
            .spawns
            .iter()
            .filter(|s| s.key.0 == id && s.key.1 == trigger && s.key.2 == step)
        {
            let state = self.procs[&s.key].1;
            assert!(
                matches!(state, Proc::Exited(false) | Proc::Lost),
                "step {step} of one deletion started again while an earlier \
                 attempt is {state:?}"
            );
        }
        let key = (id.to_string(), trigger, step, claim.attempt);
        assert!(
            !self.procs.contains_key(&key),
            "attempt {key:?} spawned twice"
        );
        self.procs
            .insert(key.clone(), (claim.owner.clone(), Proc::Running));
        self.count(if manual {
            "spawn_manual"
        } else {
            "spawn_daemon"
        });
        if claim.attempt > 1 {
            self.count("spawn_retry");
        }
        if delayed && !manual {
            self.count("spawn_delayed");
        }
        self.spawns.push(Spawn { key });
    }

    fn key_of(&self, id: &str) -> Option<(Key, Claim)> {
        let e = self.reg.entries.iter().find(|e| e.id == id)?;
        let (i, claim) = e.claim_held()?;
        Some((
            (id.to_string(), e.state.trigger()?, i, claim.attempt),
            claim.clone(),
        ))
    }

    /// One daemon pass, as in `daemon.rs`.
    fn pass(&mut self, w: &World) {
        let now = self.now;
        let touched = std::mem::take(&mut self.touched);
        let checks = self.core.plan_checks(&self.reg, &touched, now);
        let results: Vec<_> = checks
            .into_iter()
            .map(|c| {
                let p = self.probe(w, &c.id);
                (c, p)
            })
            .collect();
        let notes = self.core.apply_checks(&mut self.reg, results, now);
        for n in &notes {
            match n {
                Note::Triggered { id, gone_at } => {
                    self.check_trigger(w, id, *gone_at);
                    self.count("trigger");
                }
                Note::Presence { notice, .. } => self.count(match notice {
                    Notice::Gone { .. } => "settle_started",
                    Notice::Cancelled => "settle_cancelled",
                    Notice::CheckError {
                        interrupted_settle: true,
                        ..
                    } => "settle_interrupted_by_error",
                    Notice::CheckError { .. } => "check_error",
                    Notice::Recovered => "check_recovered",
                }),
            }
        }
        self.persist();

        for (id, _, claim) in self.core.foreign(&self.reg) {
            let (key, _) = self.key_of(&id).unwrap();
            let probe = match self.procs.get(&key) {
                // A live `gone run` holds its run lock until it records.
                _ if claim.owner == MANUAL && self.manual.as_ref().is_some_and(|m| m.1 == id) => {
                    ClaimProbe::Running
                }
                Some((_, Proc::Running)) => ClaimProbe::Running,
                Some((_, Proc::Exited(ok))) => ClaimProbe::Exited(result(*ok, now)),
                Some((_, Proc::Lost)) | None => ClaimProbe::Lost,
            };
            let what = match &probe {
                ClaimProbe::Running => "foreign_claim_waited",
                ClaimProbe::Exited(_) => "orphan_adopted_with_status",
                ClaimProbe::Lost => "orphan_lost_retried",
            };
            if self
                .core
                .resolve(&mut self.reg, &id, &claim, probe, now)
                .is_some()
                || what == "foreign_claim_waited"
            {
                self.count(what);
            }
            self.persist();
        }

        for (id, step, attempt) in self.core.due(&self.reg, now) {
            if let Some((_, claim)) = self.core.claim(&mut self.reg, &id, step, attempt, now) {
                self.spawn(&id, step, &claim);
            }
            self.persist();
        }
        self.core.end_pass();
        self.passes += 1;
    }

    /// A trigger must follow a full settle of absent checks, with no present
    /// or failed check in between, on a directory that is really gone.
    fn check_trigger(&self, w: &World, id: &str, gone_at: Ts) {
        let slot = self.slot_of(id).unwrap();
        assert_eq!(
            w.fs[slot],
            Fs::Absent,
            "teardown started while the dir is {:?}",
            w.fs[slot]
        );
        assert!(
            self.now.0 >= gone_at.0 + SETTLE,
            "triggered {}ms after first seen gone, settle is {SETTLE}ms",
            self.now.0 - gone_at.0
        );
        // The trigger comes from the final unbroken run of absent checks,
        // which must have started exactly at gone_at.
        let probes = &self.probes[id];
        let run_start = probes
            .iter()
            .rposition(|(_, p)| *p != Probe::Absent)
            .map_or(0, |i| i + 1);
        let run = &probes[run_start..];
        assert!(
            run.first().is_some_and(|(t, _)| *t == gone_at),
            "trigger with gone_at {gone_at:?} does not match the absent checks {:?}",
            &probes[run_start.saturating_sub(2)..]
        );
        // With every event delivered, the daemon saw the whole absence.
        let lost = w.lost_event_at[slot].is_some_and(|t| t >= gone_at.0);
        if !lost {
            let since = w.absent_since[slot].unwrap();
            assert!(
                since <= gone_at.0,
                "gone_at {gone_at:?} is before the dir actually went at {since}"
            );
        }
    }

    /// The process running for `slot` ends; its owner records it if alive.
    fn end_proc(&mut self, slot: usize, how: Proc) {
        let Some(id) = self.slots[slot].clone() else {
            return;
        };
        let Some((key, claim)) = self.key_of(&id) else {
            return;
        };
        let Some((owner, state)) = self.procs.get_mut(&key) else {
            return;
        };
        if *state != Proc::Running {
            return;
        }
        *state = how;
        let owner = owner.clone();
        let now = self.now;
        let r = result(how == Proc::Exited(true), now);
        self.last_finished
            .insert(id.clone(), (claim.clone(), r.clone()));
        if owner == self.core.me {
            let f = self.core.finish(&mut self.reg, &id, &claim, r, now);
            assert!(f.is_some(), "our own claim must be ours to finish");
        } else if owner == MANUAL && self.manual.as_ref().is_some_and(|m| m.1 == id) {
            let e = self.entry_mut(&id).unwrap();
            e.finish(&claim, r, &RetryPolicy::ONCE, now).unwrap();
            // `gone run` goes on to the next step, ignoring its delay.
            if matches!(e.state, EntryState::Triggered { .. }) {
                let (i, c) = e.claim_manual(MANUAL, now).unwrap();
                self.spawn(&id, i, &c);
            } else {
                self.manual = None;
            }
        }
        // An earlier daemon's orphan just exits; the next pass records it.
        self.persist();
    }

    fn watch(&mut self, slot: usize, afters: &[u64]) {
        let e = Entry {
            id: format!("id{}", self.next_id),
            name: format!("slot{slot}"),
            dir: slot_dir(slot),
            cwd: "/home/u".into(),
            env: Default::default(),
            steps: afters
                .iter()
                .enumerate()
                .map(|(i, a)| StepConfig {
                    cmd: Cmd::Argv(vec![format!("step{i}")]),
                    after: Dur(*a),
                })
                .collect(),
            registered_at: self.now,
            triggers: 0,
            state: EntryState::Watching,
        };
        self.next_id += 1;
        let old = self.entry(slot).cloned();
        match old {
            Some(o) if o.state == EntryState::Watching && o.same_config(&e) => return,
            Some(o) => {
                if matches!(o.state, EntryState::Triggered { .. }) {
                    self.count("replace_cancelled_pending");
                }
                self.dead.insert(o.id.clone());
                let pos = self.reg.entries.iter().position(|x| x.id == o.id).unwrap();
                self.reg.entries[pos] = e.clone();
                if self.manual.as_ref().is_some_and(|m| m.1 == o.id) {
                    self.manual = None;
                }
            }
            None => self.reg.entries.push(e.clone()),
        }
        self.slots[slot] = Some(e.id);
        self.persist();
    }

    fn forget(&mut self, slot: usize) {
        if let Some(id) = self.slots[slot].take() {
            self.reg.entries.retain(|e| e.id != id);
            self.dead.insert(id.clone());
            if self.manual.as_ref().is_some_and(|m| m.1 == id) {
                self.manual = None;
            }
            self.persist();
        }
    }

    fn manual_run(&mut self, slot: usize) {
        if self.manual.is_some() {
            return;
        }
        let Some(id) = self.slots[slot].clone() else {
            return;
        };
        let now = self.now;
        // As `gone run`: a claim whose process is gone is recorded first; a
        // running one makes it refuse.
        if let Some((key, claim)) = self.key_of(&id) {
            let probe = match self.procs.get(&key).map(|p| p.1) {
                Some(Proc::Running) => return,
                Some(Proc::Exited(ok)) => ClaimProbe::Exited(result(ok, now)),
                Some(Proc::Lost) | None => ClaimProbe::Lost,
            };
            let e = self.entry_mut(&id).unwrap();
            gone::core::resolve(e, &claim, probe, &RetryPolicy::ONCE, now).unwrap();
        }
        let e = self.entry_mut(&id).unwrap();
        let (i, claim) = e
            .claim_manual(MANUAL, now)
            .expect("nothing holds a claim now");
        self.spawn(&id, i, &claim);
        self.manual = Some((slot, id));
        self.persist();
    }
}

pub struct Machine;

impl StateMachineTest for Machine {
    type SystemUnderTest = Sut;
    type Reference = Model;

    fn init_test(w: &World) -> Sut {
        let mut s = Sut {
            now: Ts(w.now),
            core: Sut::new_core(0),
            instance: 0,
            reg: Registry::default(),
            slots: [None, None],
            next_id: 0,
            procs: HashMap::new(),
            spawns: Vec::new(),
            dead: HashSet::new(),
            touched: Vec::new(),
            manual: None,
            probes: HashMap::new(),
            passes: 0,
            stats: Default::default(),
            last_finished: HashMap::new(),
        };
        s.pass(w);
        s
    }

    fn apply(mut s: Sut, w: &World, a: Action) -> Sut {
        s.now = Ts(w.now);
        match a {
            Action::SetDir {
                slot, delivered, ..
            } => {
                if delivered {
                    // An event on the parent directory, naming the slot.
                    s.touched.push(slot_dir(slot));
                }
            }
            Action::Advance(_) => {}
            Action::Finish { slot, ok } => s.end_proc(slot, Proc::Exited(ok)),
            Action::Lose { slot } => s.end_proc(slot, Proc::Lost),
            Action::Restart => {
                if !s.core.mine(&s.reg).is_empty() {
                    s.count("restart_mid_step");
                }
                // The new daemon knows nothing of the old one's checks:
                // settling starts over (a restart can delay, never cause, a
                // teardown). Mark the break in every check log.
                for log in s.probes.values_mut() {
                    log.push((s.now, Probe::Error("<daemon restart>".into())));
                }
                s.instance += 1;
                s.core = Sut::new_core(s.instance);
                s.touched.clear();
            }
            Action::Watch { slot, afters } => s.watch(slot, &afters),
            Action::Forget { slot } => s.forget(slot),
            Action::ManualRun { slot } => s.manual_run(slot),
            Action::Duplicate { slot } => {
                if let Some(id) = s.slots[slot].clone()
                    && let Some((claim, r)) = s.last_finished.get(&id).cloned()
                {
                    let before = s.reg.clone();
                    let held = s.key_of(&id).map(|(_, c)| c);
                    if held.as_ref() != Some(&claim) {
                        let now = s.now;
                        let f = s.core.finish(&mut s.reg, &id, &claim, r, now);
                        assert_eq!(f, None, "a stale result was accepted");
                        assert_eq!(s.reg, before, "a stale result changed the registry");
                        s.count("stale_result_rejected");
                    }
                }
            }
        }
        s.pass(w);
        s
    }

    fn teardown(s: Sut, _: World) {
        if let Ok(path) = std::env::var("GONE_PROPTEST_STATS") {
            use std::io::Write;
            let line = serde_json::to_string(&s.stats).unwrap();
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(f, "{line}").unwrap();
        }
    }

    fn check_invariants(s: &Sut, w: &World) {
        // Registry and model agree on what is registered.
        for slot in 0..SLOTS {
            let e = s.entry(slot);
            assert_eq!(e.is_some(), w.registered[slot].is_some(), "slot {slot}");
            if let (Some(e), Some(afters)) = (e, &w.registered[slot]) {
                let got: Vec<u64> = e.steps.iter().map(|c| c.after.0).collect();
                assert_eq!(&got, afters);
            }
        }
        for e in &s.reg.entries {
            assert!(!s.dead.contains(&e.id), "a forgotten registration is back");
            // Representation invariants the types can't express.
            match &e.state {
                EntryState::Triggered { done, .. } => assert!(done.len() < e.steps.len()),
                EntryState::Finished { steps, .. } => assert_eq!(steps.len(), e.steps.len()),
                EntryState::Watching => {}
            }
            // A pass claims everything due: nothing due is left waiting,
            // including delayed steps after a restart.
            assert_eq!(e.runnable(s.now), None, "a due step was not started: {e:?}");
            // Settling never outlives its deadline: the pass at or after the
            // deadline triggers (or cancels).
            if let Some(Presence::Settling { deadline, .. }) = s.core.presence(&e.id) {
                assert!(*deadline > s.now, "settling past its deadline");
            }
        }
        // Refinement: the daemon's belief about each directory is exactly
        // what its own check log implies.
        for e in s
            .reg
            .entries
            .iter()
            .filter(|e| e.state == EntryState::Watching)
        {
            let log = s.probes.get(&e.id).map(Vec::as_slice).unwrap_or(&[]);
            let expected = match log.last() {
                None => None,
                Some((_, Probe::Error(m))) if m == "<daemon restart>" => None,
                Some((_, Probe::Present)) => Some(Presence::Present),
                Some((_, Probe::Error(m))) => Some(Presence::Unknown { error: m.clone() }),
                Some((_, Probe::Absent)) => {
                    let start = log
                        .iter()
                        .rposition(|(_, p)| *p != Probe::Absent)
                        .map_or(0, |i| i + 1);
                    let since = log[start].0;
                    Some(Presence::Settling {
                        since,
                        deadline: since.plus(Dur(SETTLE)),
                    })
                }
            };
            assert_eq!(
                s.core.presence(&e.id),
                expected.as_ref(),
                "check log: {:?}",
                &log[log.len().saturating_sub(4)..]
            );
        }
        // The sweep: a delete nobody was told about is still noticed within
        // one sweep interval, and acted on one settle later.
        for slot in 0..SLOTS {
            let (Some(e), Some(since)) = (s.entry(slot), w.absent_since[slot]) else {
                continue;
            };
            if e.state != EntryState::Watching || e.registered_at.0 > since {
                continue;
            }
            if s.now.0 >= since.saturating_add(SWEEP) {
                assert!(
                    matches!(s.core.presence(&e.id), Some(Presence::Settling { .. })),
                    "dir gone since {since}, now {}, still not settling: {:?}",
                    s.now.0,
                    s.core.presence(&e.id)
                );
            }
        }
        // The daemon's view of its own runs, derived from the registry,
        // matches the processes it actually started.
        for (id, _, claim) in s.core.mine(&s.reg) {
            let (key, _) = s.key_of(&id).unwrap();
            let (owner, state) = &s.procs[&key];
            assert_eq!(
                owner, &s.core.me,
                "the daemon claims a run it did not start: {claim:?}"
            );
            assert_eq!(*state, Proc::Running, "our run ended but is still claimed");
        }
        // No orphan whose process already ended is left unrecorded after a
        // pass (only a live `gone run` may hold a claim on an ended process).
        for (id, _, claim) in s.core.foreign(&s.reg) {
            let (key, _) = s.key_of(&id).unwrap();
            let live_manual = claim.owner == MANUAL && s.manual.as_ref().is_some_and(|m| m.1 == id);
            if !live_manual {
                assert_eq!(
                    s.procs.get(&key).map(|p| p.1),
                    Some(Proc::Running),
                    "an orphaned attempt that ended was not recorded: {claim:?}"
                );
            }
        }
        // Each spawn is accounted for.
        assert!(s.spawns.iter().all(|sp| s.procs.contains_key(&sp.key)));
        assert!(s.passes > 0);
    }
}

prop_state_machine! {
    #![proptest_config(Config {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(256),
        max_shrink_iters: 20_000,
        failure_persistence: None,
        .. Config::default()
    })]

    #[test]
    fn daemon_core_state_machine(sequential 1..60 => Machine);
}
