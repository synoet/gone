//! The background service.
//!
//! One loop, woken by inotify events, step completions, or a short tick:
//!
//! 1. reload the registry if the file changed (no restart needed);
//! 2. keep an inotify watch on every ancestor of every watched directory,
//!    re-adding watches when an ancestor is re-created;
//! 3. check watched directories that an event touched, that are new, or (on
//!    every sweep) all of them; a directory that is really absent (ENOENT /
//!    ENOTDIR, never a permission error) starts settling;
//! 4. after the settle delay, re-check: back → cancelled, still gone →
//!    triggered (persisted, with the time it was first seen gone);
//! 5. claim and run due steps, one at a time per entry, in worker threads;
//! 6. record results, scheduling retries with backoff.
//!
//! Everything that must survive a restart (triggered entries, delayed steps,
//! retries) lives in the registry; only settling is in memory, and a restart
//! simply sees the directory missing again and settles afresh.

use crate::events::{Logger, v};
use crate::exec::{self, Job, Output};
use crate::model::{Entry, EntryState, Outcome, RetryPolicy, RunResult};
use crate::store::Store;
use crate::time::{Dur, Ts, approx};
use anyhow::{Context, Result};
use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone)]
pub struct Config {
    /// How long a directory must stay gone before its steps run.
    pub settle: Duration,
    /// How often every watched directory is re-checked regardless of events.
    pub sweep_interval: Duration,
    /// Upper bound on how long the loop sleeps.
    pub tick: Duration,
    pub retry: RetryPolicy,
    /// A step running longer than this is killed and counts as failed.
    pub step_timeout: Option<Duration>,
    /// On SIGTERM/SIGINT, how long to wait for running steps before killing them.
    pub shutdown_grace: Duration,
}

enum Msg {
    Fs(notify::Result<notify::Event>),
    Done {
        id: String,
        step: usize,
        result: RunResult,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Present,
    Absent,
}

/// Absent only on ENOENT/ENOTDIR. Anything else (EACCES, EIO, …) is an
/// error, and an error never triggers a teardown.
pub fn presence(path: &Path) -> std::io::Result<Presence> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(Presence::Present),
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::NotADirectory =>
        {
            Ok(Presence::Absent)
        }
        Err(e) => Err(e),
    }
}

struct Settling {
    since: Ts,
    deadline: Ts,
}

struct Daemon<'a> {
    store: &'a Store,
    cfg: &'a Config,
    log: Logger,
    pid: u32,
    tx: Sender<Msg>,
    watcher: RecommendedWatcher,
    reg: Vec<Entry>,
    reg_sig: Option<(u64, u64, SystemTime)>,
    reg_error: Option<String>,
    /// Watched path → (dev, ino) of the directory the watch is on.
    watched: HashMap<PathBuf, (u64, u64)>,
    watch_errors: HashSet<PathBuf>,
    check_errors: HashMap<String, String>,
    known: HashSet<String>,
    settling: HashMap<String, Settling>,
    /// Entry id → (step, child pid once spawned).
    running: HashMap<String, usize>,
    children: Arc<Mutex<HashMap<String, u32>>>,
    touched: Vec<PathBuf>,
    force_sweep: bool,
    last_sweep: Instant,
    first_pass: bool,
}

pub fn run(store: &Store, cfg: &Config) -> Result<()> {
    let _lock = store.lock_daemon()?;
    let term = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        // A second signal while shutting down exits at once.
        signal_hook::flag::register_conditional_shutdown(sig, 1, term.clone())?;
        signal_hook::flag::register(sig, term.clone())?;
    }
    let (tx, rx) = mpsc::channel();
    let fs_tx = tx.clone();
    let watcher = notify::recommended_watcher(move |res| {
        let _ = fs_tx.send(Msg::Fs(res));
    })
    .context("starting inotify")?;

    let mut d = Daemon {
        store,
        cfg,
        log: Logger::new(store.events_path(), true),
        pid: std::process::id(),
        tx,
        watcher,
        reg: Vec::new(),
        reg_sig: None,
        reg_error: None,
        watched: HashMap::new(),
        watch_errors: HashSet::new(),
        check_errors: HashMap::new(),
        known: HashSet::new(),
        settling: HashMap::new(),
        running: HashMap::new(),
        children: Arc::default(),
        touched: Vec::new(),
        force_sweep: false,
        last_sweep: Instant::now(),
        first_pass: true,
    };
    d.log.event(
        "daemon_start",
        &[
            ("pid", v(d.pid)),
            ("state_dir", v(store.root().display().to_string())),
            ("settle", v(Dur::from_std(cfg.settle).to_string())),
            ("sweep", v(Dur::from_std(cfg.sweep_interval).to_string())),
        ],
    );
    d.prune_run_logs();
    if let Err(e) = d.watcher.watch(store.root(), RecursiveMode::NonRecursive) {
        d.log.event(
            "watch_error",
            &[
                ("path", v(store.root().display().to_string())),
                ("error", v(e.to_string())),
            ],
        );
    }

    let mut stop_deadline: Option<Instant> = None;
    loop {
        let mut msgs = Vec::new();
        match rx.recv_timeout(d.sleep_for()) {
            Ok(m) => msgs.push(m),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        msgs.extend(rx.try_iter());
        for m in msgs {
            d.handle(m);
        }
        let stopping = term.load(Ordering::Relaxed);
        if stopping && stop_deadline.is_none() {
            d.log
                .event("daemon_stopping", &[("running", v(d.running.len()))]);
            stop_deadline = Some(Instant::now() + cfg.shutdown_grace);
        }
        d.pass(!stopping);
        if let Some(deadline) = stop_deadline {
            if d.running.is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                d.kill_running();
                break;
            }
        }
    }
    d.log.event("daemon_stop", &[("pid", v(d.pid))]);
    Ok(())
}

impl Daemon<'_> {
    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Fs(Ok(ev)) => {
                if ev.need_rescan() {
                    self.force_sweep = true;
                }
                let relevant = !matches!(
                    ev.kind,
                    EventKind::Access(_)
                        | EventKind::Create(_)
                        | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Metadata(_))
                );
                if relevant {
                    self.touched.extend(ev.paths);
                }
            }
            Msg::Fs(Err(e)) => {
                self.log
                    .event("watch_error", &[("error", v(e.to_string()))]);
                self.force_sweep = true;
            }
            Msg::Done { id, step, result } => self.record(id, step, result),
        }
    }

    fn sleep_for(&self) -> Duration {
        let now = Ts::now();
        let mut next = self.cfg.tick;
        let mut consider = |t: Ts| {
            let d = Duration::from_millis(t.0.saturating_sub(now.0));
            next = next.min(d);
        };
        for s in self.settling.values() {
            consider(s.deadline);
        }
        for e in &self.reg {
            if let Some((_, t)) = e.next_due() {
                consider(t);
            }
        }
        next.max(Duration::from_millis(5))
    }

    /// One pass of the loop (see the module docs).
    fn pass(&mut self, schedule: bool) {
        self.reload();
        self.recover_stale();
        self.sync_watches();
        self.detect();
        self.settle();
        if schedule {
            self.schedule();
        }
        if self.first_pass {
            self.log.event(
                "ready",
                &[
                    ("entries", v(self.reg.len())),
                    ("watches", v(self.watched.len())),
                ],
            );
        }
        self.first_pass = false;
    }

    fn reload(&mut self) {
        let path = self.store.registry_path();
        let sig = std::fs::metadata(&path).ok().map(|m| {
            (
                m.ino(),
                m.len(),
                m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            )
        });
        if sig == self.reg_sig && !self.first_pass {
            return;
        }
        match self.store.load() {
            Ok(reg) => {
                self.reg = reg.entries;
                self.reg_sig = sig;
                self.reg_error = None;
                let ids: HashSet<&String> = self.reg.iter().map(|e| &e.id).collect();
                self.known.retain(|id| ids.contains(id));
                self.settling.retain(|id, _| ids.contains(id));
                self.check_errors.retain(|id, _| ids.contains(id));
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if self.reg_error.as_ref() != Some(&msg) {
                    self.log
                        .event("registry_error", &[("error", v(msg.clone()))]);
                    self.reg_error = Some(msg);
                }
            }
        }
    }

    /// Steps marked running by an owner that no longer exists: a previous
    /// daemon that crashed or was killed, or an interrupted `gone run`.
    fn recover_stale(&mut self) {
        let me = self.pid;
        let stale: Vec<(String, usize, u32)> = self
            .reg
            .iter()
            .flat_map(|e| {
                let mine = self.running.get(&e.id).copied();
                e.stale_steps(|p| p != me && exec::pid_alive(p))
                    .into_iter()
                    .filter(move |&(i, p)| !(p == me && mine == Some(i)))
                    .map(|(i, p)| (e.id.clone(), i, p))
                    .collect::<Vec<_>>()
            })
            .collect();
        if stale.is_empty() {
            return;
        }
        let policy = self.cfg.retry;
        let now = Ts::now();
        let res = self.store.update(|reg| {
            let mut out = Vec::new();
            for (id, i, pid) in &stale {
                let Some(e) = reg.entries.iter_mut().find(|e| &e.id == id) else {
                    continue;
                };
                let result = RunResult {
                    at: now,
                    ok: false,
                    exit_code: None,
                    error: Some(format!("interrupted (owner pid {pid} is gone)")),
                    took: Dur::ZERO,
                    output: None,
                };
                if let Some(f) = e.finish(*i, *pid, result, &policy, now) {
                    out.push((e.name.clone(), e.dir.clone(), id.clone(), *i, f));
                }
            }
            Ok(out)
        });
        match res {
            Ok(list) => {
                for (name, dir, id, i, f) in list {
                    self.log.event(
                        "step_interrupted",
                        &[
                            ("name", v(name.clone())),
                            ("dir", v(dir.display().to_string())),
                            ("id", v(short(&id))),
                            ("step", v(i + 1)),
                        ],
                    );
                    self.log_outcome(&name, &dir, &id, i, &f, None);
                }
            }
            Err(e) => self
                .log
                .event("registry_error", &[("error", v(format!("{e:#}")))]),
        }
        self.reload();
    }

    fn sync_watches(&mut self) {
        let mut want: HashSet<PathBuf> = HashSet::new();
        for e in self.reg.iter().filter(|e| e.state == EntryState::Watching) {
            let mut p = e.dir.parent();
            while let Some(a) = p {
                if !want.insert(a.to_path_buf()) {
                    break; // the rest of this chain is already in
                }
                p = a.parent();
            }
        }
        let gone: Vec<PathBuf> = self
            .watched
            .keys()
            .filter(|p| !want.contains(*p))
            .cloned()
            .collect();
        for p in gone {
            let _ = self.watcher.unwatch(&p);
            self.watched.remove(&p);
        }
        for p in want {
            let id = std::fs::metadata(&p)
                .ok()
                .filter(|m| m.is_dir())
                .map(|m| (m.dev(), m.ino()));
            let current = self.watched.get(&p).copied();
            if id == current {
                continue;
            }
            if current.is_some() {
                let _ = self.watcher.unwatch(&p);
                self.watched.remove(&p);
            }
            let Some(id) = id else { continue };
            match self.watcher.watch(&p, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.watched.insert(p.clone(), id);
                    self.watch_errors.remove(&p);
                }
                Err(e) => {
                    if self.watch_errors.insert(p.clone()) {
                        self.log.event(
                            "watch_error",
                            &[
                                ("path", v(p.display().to_string())),
                                ("error", v(e.to_string())),
                            ],
                        );
                    }
                }
            }
        }
    }

    fn detect(&mut self) {
        let sweep = self.force_sweep || self.last_sweep.elapsed() >= self.cfg.sweep_interval;
        if sweep {
            self.last_sweep = Instant::now();
            self.force_sweep = false;
        }
        let touched = std::mem::take(&mut self.touched);
        let now = Ts::now();
        let mut found = Vec::new();
        for e in self.reg.iter().filter(|e| e.state == EntryState::Watching) {
            if self.settling.contains_key(&e.id) {
                continue;
            }
            let via = if self.first_pass {
                "startup"
            } else if !self.known.contains(&e.id) {
                "registered"
            } else if touched.iter().any(|p| e.dir.starts_with(p)) {
                "inotify"
            } else if sweep {
                "sweep"
            } else {
                continue;
            };
            self.known.insert(e.id.clone());
            match presence(&e.dir) {
                Ok(Presence::Present) => {
                    self.check_errors.remove(&e.id);
                }
                Ok(Presence::Absent) => {
                    self.check_errors.remove(&e.id);
                    found.push((e.id.clone(), e.name.clone(), e.dir.clone(), via));
                }
                Err(err) => {
                    let msg = err.to_string();
                    if self.check_errors.get(&e.id) != Some(&msg) {
                        self.log.event(
                            "check_error",
                            &[
                                ("name", v(e.name.clone())),
                                ("dir", v(e.dir.display().to_string())),
                                ("error", v(msg.clone())),
                            ],
                        );
                        self.check_errors.insert(e.id.clone(), msg);
                    }
                }
            }
        }
        let settle = Dur::from_std(self.cfg.settle);
        for (id, name, dir, via) in found {
            self.log.event(
                "dir_gone",
                &[
                    ("name", v(name)),
                    ("dir", v(dir.display().to_string())),
                    ("id", v(short(&id))),
                    ("via", v(via)),
                    ("settle", v(settle.to_string())),
                ],
            );
            self.settling.insert(
                id,
                Settling {
                    since: now,
                    deadline: now.plus(settle),
                },
            );
        }
    }

    fn settle(&mut self) {
        let now = Ts::now();
        let ids: Vec<String> = self.settling.keys().cloned().collect();
        for id in ids {
            let Some(e) = self.reg.iter().find(|e| e.id == id) else {
                self.settling.remove(&id);
                continue;
            };
            if e.state != EntryState::Watching {
                self.settling.remove(&id);
                continue;
            }
            let (name, dir) = (e.name.clone(), e.dir.clone());
            let fields = |extra: Vec<(&'static str, Value)>| {
                let mut f = vec![
                    ("name", v(name.clone())),
                    ("dir", v(dir.display().to_string())),
                    ("id", v(short(&id))),
                ];
                f.extend(extra);
                f
            };
            match presence(&dir) {
                Ok(Presence::Present) => {
                    self.settling.remove(&id);
                    self.log
                        .event("cancelled", &fields(vec![("reason", v("dir is back"))]));
                    continue;
                }
                Err(_) => continue, // unknown: keep waiting, never trigger on an error
                Ok(Presence::Absent) => {}
            }
            let s = &self.settling[&id];
            if now < s.deadline {
                continue;
            }
            let since = s.since;
            self.settling.remove(&id);
            let res = self.store.update(|reg| {
                Ok(reg
                    .entries
                    .iter_mut()
                    .find(|e| e.id == id)
                    .map(|e| (e.trigger(since), e.steps.len())))
            });
            match res {
                Ok(Some((true, steps))) => {
                    self.log
                        .event("triggered", &fields(vec![("steps", v(steps))]));
                }
                Ok(_) => {}
                Err(e) => self
                    .log
                    .event("registry_error", &[("error", v(format!("{e:#}")))]),
            }
            self.reload();
        }
    }

    fn schedule(&mut self) {
        let now = Ts::now();
        let due: Vec<(String, usize)> = self
            .reg
            .iter()
            .filter(|e| !self.running.contains_key(&e.id))
            .filter_map(|e| e.next_runnable(now).map(|i| (e.id.clone(), i)))
            .collect();
        for (id, i) in due {
            let pid = self.pid;
            let claimed = self.store.update(|reg| {
                Ok(reg
                    .entries
                    .iter_mut()
                    .find(|e| e.id == id)
                    .and_then(|e| e.claim(i, pid, now).then(|| e.clone())))
            });
            let entry = match claimed {
                Ok(Some(e)) => e,
                Ok(None) => continue,
                Err(e) => {
                    self.log
                        .event("registry_error", &[("error", v(format!("{e:#}")))]);
                    continue;
                }
            };
            self.start(entry, i);
        }
        self.reload();
    }

    fn start(&mut self, e: Entry, i: usize) {
        let step = &e.steps[i];
        let attempt = step.attempts + 1;
        let output =
            self.store
                .runs_dir()
                .join(format!("{}-step{}-attempt{attempt}.log", e.id, i + 1));
        self.log.event(
            "step_start",
            &[
                ("name", v(e.name.clone())),
                ("dir", v(e.dir.display().to_string())),
                ("id", v(short(&e.id))),
                ("step", v(i + 1)),
                ("attempt", v(attempt)),
                ("program", v(step.cmd.program())),
            ],
        );
        self.running.insert(e.id.clone(), i);
        let tx = self.tx.clone();
        let children = self.children.clone();
        let timeout = self.cfg.step_timeout;
        std::thread::spawn(move || {
            let env = step_env(&e);
            let job = Job {
                cmd: &e.steps[i].cmd,
                cwd: &e.cwd,
                env: &env,
                timeout,
                output: Output::File(output),
            };
            let id = e.id.clone();
            let result = exec::run(&job, |pgid| {
                children.lock().unwrap().insert(id.clone(), pgid);
            });
            children.lock().unwrap().remove(&id);
            let _ = tx.send(Msg::Done {
                id,
                step: i,
                result,
            });
        });
    }

    fn record(&mut self, id: String, step: usize, result: RunResult) {
        self.running.remove(&id);
        let pid = self.pid;
        let policy = self.cfg.retry;
        let now = Ts::now();
        let res = self.store.update(|reg| {
            Ok(reg.entries.iter_mut().find(|e| e.id == id).and_then(|e| {
                let (name, dir) = (e.name.clone(), e.dir.clone());
                e.finish(step, pid, result.clone(), &policy, now)
                    .map(|f| (name, dir, f))
            }))
        });
        match res {
            Ok(Some((name, dir, f))) => self.log_outcome(&name, &dir, &id, step, &f, Some(&result)),
            Ok(None) => self.log.event(
                "result_dropped",
                &[
                    ("id", v(short(&id))),
                    ("step", v(step + 1)),
                    ("reason", v("registration was replaced or forgotten")),
                ],
            ),
            Err(e) => self
                .log
                .event("registry_error", &[("error", v(format!("{e:#}")))]),
        }
        self.reload();
    }

    fn log_outcome(
        &self,
        name: &str,
        dir: &Path,
        id: &str,
        step: usize,
        f: &crate::model::Finished,
        result: Option<&RunResult>,
    ) {
        let mut fields = vec![
            ("name", v(name)),
            ("dir", v(dir.display().to_string())),
            ("id", v(short(id))),
            ("step", v(step + 1)),
            ("attempt", v(f.attempt)),
        ];
        if let Some(r) = result {
            fields.push(("exit", r.exit_code.map(v).unwrap_or(Value::Null)));
            fields.push(("took", v(approx(r.took))));
            if let Some(err) = &r.error {
                fields.push(("error", v(err.clone())));
            }
        }
        match &f.outcome {
            Outcome::Ok => self.log.event("step_ok", &fields),
            Outcome::Retry { next_at } => {
                fields.push(("retry_in", v(approx(next_at.since(Ts::now())))));
                self.log.event("step_failed", &fields);
            }
            Outcome::GaveUp => {
                fields.push(("gave_up", v(true)));
                self.log.event("step_failed", &fields);
            }
        }
        if let Some(state) = &f.concluded {
            self.log.event(
                state.kind(),
                &[
                    ("name", v(name)),
                    ("dir", v(dir.display().to_string())),
                    ("id", v(short(id))),
                ],
            );
        }
    }

    fn kill_running(&mut self) {
        for (id, pgid) in self.children.lock().unwrap().iter() {
            self.log.event(
                "step_killed",
                &[("id", v(short(id))), ("reason", v("daemon shutdown"))],
            );
            exec::kill_group(*pgid, libc::SIGTERM);
        }
    }

    fn prune_run_logs(&self) {
        let Ok(dir) = std::fs::read_dir(self.store.runs_dir()) else {
            return;
        };
        let cutoff = SystemTime::now() - Duration::from_secs(30 * 86_400);
        for f in dir.flatten() {
            if f.metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t < cutoff)
            {
                let _ = std::fs::remove_file(f.path());
            }
        }
    }
}

/// The step's complete environment: what was captured at registration plus
/// `GONE_DIR` and `GONE_NAME`.
pub fn step_env(e: &Entry) -> BTreeMap<String, String> {
    let mut env = e.env.clone();
    env.insert("GONE_DIR".into(), e.dir.display().to_string());
    env.insert("GONE_NAME".into(), e.name.clone());
    env
}

pub fn short(id: &str) -> String {
    id.chars().take(8).collect()
}
