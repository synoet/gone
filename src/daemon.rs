//! The background service: the IO shell around [`crate::core::Core`].
//!
//! Each pass: reload the registry if it changed; keep inotify watches on
//! every ancestor of every watched directory (a cache, rebuilt from the
//! registry); check the directories `Core` asks for and feed the results
//! back; settle claims left by an earlier daemon or a `gone run`; claim and
//! start due steps. A pass runs when an event or a step result arrives, when
//! a settle deadline or due time comes, and at least every tick.
//!
//! Each attempt runs under `gone __supervise` in its own process group,
//! holding the attempt's run lock (see `exec`). The daemon never kills
//! running steps when it stops: they finish on their own and the next daemon
//! records their exit status.

use crate::core::{ClaimProbe, Core, Lifecycle, Note};
use crate::events::{Logger, v};
use crate::exec::{self, Job, Output, Supervised};
use crate::model::{Claim, Entry, Outcome, Owner, Registry, RetryPolicy, RunResult};
use crate::presence::{Notice, Probe};
use crate::store::{RunFiles, Store};
use crate::time::{Dur, Ts, approx};
use anyhow::{Context, Result};
use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, SystemTime};

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
    /// On SIGTERM/SIGINT, how long to wait for our running steps to finish
    /// (so we record their results) before exiting and leaving them running.
    pub shutdown_grace: Duration,
    /// Off: notice deletes by sweeping only.
    pub inotify: bool,
}

enum Msg {
    Fs(notify::Result<notify::Event>),
    Done {
        id: String,
        claim: Claim,
        result: RunResult,
        files: RunFiles,
        lock: File,
    },
}

/// One check of a watched directory. Absent only on ENOENT/ENOTDIR;
/// anything else (EACCES, EIO, …) is an error, which never triggers.
pub fn probe_dir(path: &Path) -> Probe {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Probe::Present,
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::NotADirectory =>
        {
            Probe::Absent
        }
        Err(e) => Probe::Error(e.to_string()),
    }
}

struct Daemon<'a> {
    store: &'a Store,
    cfg: &'a Config,
    log: Logger,
    exe: PathBuf,
    tx: Sender<Msg>,
    watcher: RecommendedWatcher,
    core: Core,
    reg: Registry,
    reg_sig: Option<(u64, u64, SystemTime)>,
    reg_error: Option<String>,
    /// Cache: watched path → (dev, ino) of the directory the watch is on.
    watched: HashMap<PathBuf, (u64, u64)>,
    watch_errors: HashSet<PathBuf>,
    /// Log dedupe only: foreign claims we already said we are waiting on.
    adopted_logged: HashSet<(String, u32)>,
    touched: Vec<PathBuf>,
}

pub fn new_instance() -> String {
    use std::io::Read;
    let mut buf = [0u8; 4];
    let _ = File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf));
    let mix = u32::from_le_bytes(buf) ^ (Ts::now().0 as u32) ^ std::process::id().rotate_left(16);
    format!("{mix:08x}")
}

pub fn run(store: &Store, cfg: &Config) -> Result<()> {
    let instance = new_instance();
    let _lock = store.lock_daemon(&instance)?;
    let term = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        // A second signal while draining exits at once.
        signal_hook::flag::register_conditional_shutdown(sig, 1, term.clone())?;
        signal_hook::flag::register(sig, term.clone())?;
    }
    let (tx, rx) = mpsc::channel();
    let fs_tx = tx.clone();
    let watcher = notify::recommended_watcher(move |res| {
        let _ = fs_tx.send(Msg::Fs(res));
    })
    .context("starting inotify")?;
    let exe = std::env::current_exe().context("finding the gone binary")?;
    let me = Owner::Daemon {
        pid: std::process::id(),
        instance: instance.clone(),
    };
    let mut d = Daemon {
        store,
        cfg,
        log: Logger::new(store.events_path(), true),
        exe,
        tx,
        watcher,
        core: Core::new(
            me,
            Dur::from_std(cfg.settle),
            Dur::from_std(cfg.sweep_interval),
            cfg.retry,
        ),
        reg: Registry::default(),
        reg_sig: None,
        reg_error: None,
        watched: HashMap::new(),
        watch_errors: HashSet::new(),
        adopted_logged: HashSet::new(),
        touched: Vec::new(),
    };
    d.log.event(
        "daemon_start",
        &[
            ("pid", v(std::process::id())),
            ("instance", v(instance)),
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
        if term.load(Ordering::Relaxed) && !matches!(d.core.lifecycle, Lifecycle::Draining { .. }) {
            let mine = d.core.mine(&d.reg).len();
            d.log.event("daemon_stopping", &[("running", v(mine))]);
            d.core.drain(Ts::now(), Dur::from_std(cfg.shutdown_grace));
        }
        d.pass();
        if d.core.should_exit(&d.reg, Ts::now()) {
            break;
        }
    }
    let left = d.core.mine(&d.reg).len();
    d.log.event(
        "daemon_stop",
        &[("pid", v(std::process::id())), ("left_running", v(left))],
    );
    Ok(())
}

impl Daemon<'_> {
    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Fs(Ok(ev)) => {
                if ev.need_rescan() {
                    self.core.force_sweep();
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
                self.core.force_sweep();
            }
            Msg::Done {
                id,
                claim,
                result,
                files,
                lock,
            } => {
                self.record(&id, &claim, result);
                files.cleanup();
                drop(lock);
            }
        }
    }

    fn sleep_for(&self) -> Duration {
        let now = Ts::now();
        let next = self
            .core
            .next_wakeup(&self.reg)
            .map(|t| Duration::from_millis(t.0.saturating_sub(now.0)))
            .unwrap_or(self.cfg.tick);
        next.min(self.cfg.tick).max(Duration::from_millis(5))
    }

    fn pass(&mut self) {
        let starting = self.core.lifecycle == Lifecycle::Starting;
        self.reload();
        self.sync_watches();
        self.check_dirs();
        self.settle_foreign_claims();
        self.start_due();
        self.core.end_pass();
        if starting {
            self.log.event(
                "ready",
                &[
                    ("entries", v(self.reg.entries.len())),
                    ("watches", v(self.watched.len())),
                ],
            );
        }
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
        if sig.is_some() && sig == self.reg_sig {
            return;
        }
        match self.store.load() {
            Ok(reg) => {
                self.reg = reg;
                self.reg_sig = sig;
                self.reg_error = None;
            }
            Err(e) => self.registry_error(&e),
        }
    }

    fn registry_error(&mut self, e: &anyhow::Error) {
        let msg = format!("{e:#}");
        if self.reg_error.as_ref() != Some(&msg) {
            self.log
                .event("registry_error", &[("error", v(msg.clone()))]);
            self.reg_error = Some(msg);
        }
    }

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.reg.entries.iter().find(|e| e.id == id)
    }

    fn fields(&self, id: &str) -> Vec<(&'static str, Value)> {
        match self.entry(id) {
            Some(e) => vec![
                ("name", v(e.name.clone())),
                ("dir", v(e.dir.display().to_string())),
                ("id", v(short(id))),
            ],
            None => vec![("id", v(short(id)))],
        }
    }

    fn sync_watches(&mut self) {
        if !self.cfg.inotify {
            return;
        }
        let mut want: HashSet<PathBuf> = HashSet::new();
        for e in self
            .reg
            .entries
            .iter()
            .filter(|e| e.state == crate::model::EntryState::Watching)
        {
            for a in crate::core::ancestors(&e.dir) {
                if !want.insert(a.to_path_buf()) {
                    break; // the rest of this chain is already in
                }
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

    fn check_dirs(&mut self) {
        let now = Ts::now();
        let touched = std::mem::take(&mut self.touched);
        let checks = self.core.plan_checks(&self.reg, &touched, now);
        if checks.is_empty() {
            return;
        }
        let results: Vec<_> = checks
            .into_iter()
            .map(|c| {
                let p = probe_dir(&c.dir);
                (c, p)
            })
            .collect();
        let core = &mut self.core;
        let notes = match self
            .store
            .update(|reg| Ok(core.apply_checks(reg, results, now)))
        {
            Ok(n) => n,
            Err(e) => {
                self.registry_error(&e);
                return;
            }
        };
        self.reload();
        let settle = Dur::from_std(self.cfg.settle).to_string();
        for note in notes {
            match note {
                Note::Presence { id, via, notice } => {
                    let mut f = self.fields(&id);
                    let event = match notice {
                        Notice::Gone { .. } => {
                            f.push(("via", v(via)));
                            f.push(("settle", v(settle.clone())));
                            "dir_gone"
                        }
                        Notice::Cancelled => {
                            f.push(("reason", v("dir is back")));
                            "cancelled"
                        }
                        Notice::CheckError {
                            error,
                            interrupted_settle,
                        } => {
                            f.push(("error", v(error)));
                            if interrupted_settle {
                                f.push(("settle", v("restarts on the next absent check")));
                            }
                            "check_error"
                        }
                        Notice::Recovered => "check_ok",
                    };
                    self.log.event(event, &f);
                }
                Note::Triggered { id, gone_at } => {
                    let mut f = self.fields(&id);
                    let steps = self.entry(&id).map(|e| e.steps.len()).unwrap_or(0);
                    f.push(("steps", v(steps)));
                    f.push(("gone_at", v(gone_at.rfc3339())));
                    self.log.event("triggered", &f);
                }
            }
        }
    }

    /// Claims held by an earlier daemon or by a `gone run`: wait while the
    /// attempt runs, then record its exit status, or an interrupted attempt
    /// if it left none.
    fn settle_foreign_claims(&mut self) {
        let now = Ts::now();
        for (id, step, claim) in self.core.foreign(&self.reg) {
            let Some(trigger) = self.entry(&id).and_then(|e| e.state.trigger()) else {
                continue;
            };
            let files = self.store.run_files(&id, trigger, step, claim.attempt);
            let probe = files.probe();
            let mut fields = self.fields(&id);
            fields.extend([
                ("step", v(step + 1)),
                ("attempt", v(claim.attempt)),
                ("owner", v(claim.owner.describe())),
            ]);
            if probe == ClaimProbe::Running {
                if self.adopted_logged.insert((id.clone(), claim.attempt)) {
                    self.log.event("step_adopted", &fields);
                }
                self.enforce_timeout(&claim, &files, now);
                continue;
            }
            let (event, result) = match &probe {
                ClaimProbe::Exited(r) => ("step_recovered", Some(r.clone())),
                _ => ("step_interrupted", None),
            };
            let core = &self.core;
            match self
                .store
                .update(|reg| Ok(core.resolve(reg, &id, &claim, probe, now)))
            {
                Ok(Some(f)) => {
                    self.log.event(event, &fields);
                    self.log_outcome(&id, &f, result.as_ref());
                    files.cleanup();
                }
                Ok(None) => {}
                Err(e) => self.registry_error(&e),
            }
            self.adopted_logged.remove(&(id.clone(), claim.attempt));
            self.reload();
        }
    }

    /// The step timeout also applies to an adopted daemon-run attempt: TERM
    /// its process group once over time, KILL five seconds later.
    fn enforce_timeout(&self, claim: &Claim, files: &RunFiles, now: Ts) {
        let (Some(limit), Owner::Daemon { .. }) = (self.cfg.step_timeout, &claim.owner) else {
            return;
        };
        let over = now.since(claim.since);
        let limit = Dur::from_std(limit);
        if over < limit {
            return;
        }
        if let Some(pgid) = files.pid() {
            let sig = if over.0 >= limit.0 + 5000 {
                libc::SIGKILL
            } else {
                libc::SIGTERM
            };
            exec::kill_group(pgid, sig);
        }
    }

    fn start_due(&mut self) {
        let now = Ts::now();
        for (id, step, attempt) in self.core.due(&self.reg, now) {
            let Some(trigger) = self.entry(&id).and_then(|e| e.state.trigger()) else {
                continue;
            };
            let files = self.store.run_files(&id, trigger, step, attempt);
            let lock = match files.acquire() {
                Ok(l) => l,
                Err(e) => {
                    let mut f = self.fields(&id);
                    f.extend([("step", v(step + 1)), ("error", v(format!("{e:#}")))]);
                    self.log.event("claim_error", &f);
                    continue;
                }
            };
            let core = &self.core;
            let claimed = self
                .store
                .update(|reg| Ok(core.claim(reg, &id, step, attempt, now)));
            match claimed {
                Ok(Some((entry, claim))) => self.start(entry, step, claim, files, lock),
                Ok(None) => {}
                Err(e) => self.registry_error(&e),
            }
            self.reload();
        }
    }

    fn start(&mut self, e: Entry, step: usize, claim: Claim, files: RunFiles, lock: File) {
        let mut f = self.fields(&e.id);
        f.extend([
            ("step", v(step + 1)),
            ("attempt", v(claim.attempt)),
            ("program", v(e.steps[step].cmd.program())),
        ]);
        self.log.event("step_start", &f);
        let tx = self.tx.clone();
        let timeout = self.cfg.step_timeout;
        let exe = self.exe.clone();
        std::thread::spawn(move || {
            let env = step_env(&e);
            let result = exec::run(&Job {
                cmd: &e.steps[step].cmd,
                cwd: &e.cwd,
                env: &env,
                timeout,
                output: Output::File(files.output.clone()),
                own_group: true,
                supervised: Some(Supervised {
                    exe: &exe,
                    files: &files,
                    lock: &lock,
                }),
            });
            let _ = tx.send(Msg::Done {
                id: e.id.clone(),
                claim,
                result,
                files,
                lock,
            });
        });
    }

    fn record(&mut self, id: &str, claim: &Claim, result: RunResult) {
        let now = Ts::now();
        let core = &self.core;
        let r = result.clone();
        match self
            .store
            .update(|reg| Ok(core.finish(reg, id, claim, r, now)))
        {
            Ok(Some(f)) => self.log_outcome(id, &f, Some(&result)),
            Ok(None) => {
                let mut f = self.fields(id);
                f.push(("reason", v("registration was replaced or forgotten")));
                self.log.event("result_dropped", &f);
            }
            Err(e) => self.registry_error(&e),
        }
        self.reload();
    }

    fn log_outcome(&self, id: &str, f: &crate::model::Finished, result: Option<&RunResult>) {
        let mut fields = self.fields(id);
        fields.extend([("step", v(f.step + 1)), ("attempt", v(f.attempt))]);
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
        if let Some(all_ok) = f.concluded {
            self.log
                .event(if all_ok { "done" } else { "failed" }, &self.fields(id));
        }
    }

    fn prune_run_logs(&self) {
        let Ok(dir) = std::fs::read_dir(self.store.runs_dir()) else {
            return;
        };
        let cutoff = SystemTime::now() - Duration::from_secs(30 * 86_400);
        for f in dir.flatten() {
            let is_log = f.path().extension().is_some_and(|x| x == "log");
            if is_log
                && f.metadata()
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
