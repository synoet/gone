//! What each subcommand does.

use crate::cli::{Cli, Command, DaemonArgs, ServiceCommand, StepSpec, WatchArgs};
use crate::core::{self, ClaimProbe};
use crate::daemon::{self, short, step_env};
use crate::events::{Logger, format_line, v};
use crate::exec::Supervised;
use crate::exec::{self, Job, Output};
use crate::model::{
    Entry, EntryState, Outcome, Owner, Phase, Registry, RetryPolicy, RunResult, StepConfig,
    StepRecord,
};
use crate::presence::Probe;
use crate::store::Store;
use crate::time::{Ts, approx};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

pub fn main(cli: Cli, steps: Vec<StepSpec>) -> Result<i32> {
    if let Command::Supervise {
        status,
        lock_fd,
        argv,
    } = &cli.command
    {
        return Ok(exec::supervise(status, *lock_fd, argv));
    }
    let root = match &cli.state_dir {
        Some(d) => d.clone(),
        None => Store::default_root()?,
    };
    let store = Store::open(root)?;
    match cli.command {
        Command::Watch(args) => watch(&store, args, steps),
        Command::List { json } => list(&store, json),
        Command::Run { target, dry_run } => run(&store, &target, dry_run),
        Command::Forget { target } => forget(&store, &target),
        Command::Log { name, lines, json } => log(&store, name.as_deref(), lines, json),
        Command::Daemon(args) => run_daemon(&store, &args),
        Command::Service {
            command: ServiceCommand::Print,
        } => service_print(&store),
        Command::Supervise { .. } => unreachable!("handled above"),
    }
}

fn logger(store: &Store) -> Logger {
    Logger::new(store.events_path(), false)
}

fn watch(store: &Store, args: WatchArgs, steps: Vec<StepSpec>) -> Result<i32> {
    let dir = std::fs::canonicalize(&args.dir)
        .with_context(|| format!("cannot watch {}", args.dir.display()))?;
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    if dir.parent().is_none() {
        bail!("refusing to watch /");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cwd = match args.cwd.or(home.clone()) {
        Some(c) => std::fs::canonicalize(&c).with_context(|| format!("--cwd {}", c.display()))?,
        None => bail!("HOME is not set; pass --cwd"),
    };
    if cwd.starts_with(&dir) {
        bail!(
            "--cwd {} is inside the watched directory; steps run after it is deleted, \
             so give a cwd outside it (default: $HOME)",
            cwd.display()
        );
    }
    let name = match args.name {
        Some(n) if n.trim().is_empty() => bail!("--name must not be empty"),
        Some(n) => n,
        None => dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string()),
    };

    let mut env = BTreeMap::new();
    env.insert(
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_else(|_| DEFAULT_PATH.into()),
    );
    if let Some(h) = &home {
        env.insert("HOME".to_string(), h.display().to_string());
    }
    for spec in &args.env {
        let (k, val) = match spec.split_once('=') {
            Some((k, val)) => (k.to_string(), val.to_string()),
            None => {
                let val = std::env::var(spec)
                    .with_context(|| format!("--env {spec}: not set in the current environment"))?;
                (spec.clone(), val)
            }
        };
        if k.is_empty() || k.contains('\0') {
            bail!("--env {spec}: bad variable name");
        }
        if k == "GONE_DIR" || k == "GONE_NAME" {
            bail!("--env {k}: set by gone itself");
        }
        env.insert(k, val);
    }

    let entry = Entry {
        id: new_id(),
        name: name.clone(),
        dir: dir.clone(),
        cwd,
        env,
        steps: steps
            .into_iter()
            .map(|s| StepConfig {
                cmd: s.cmd,
                after: s.after,
            })
            .collect(),
        registered_at: Ts::now(),
        triggers: 0,
        state: EntryState::Watching,
    };
    let nsteps = entry.steps.len();

    enum Change {
        Added,
        Unchanged,
        Replaced(EntryState),
    }
    let new = entry.clone();
    let change = store.update(move |reg| {
        if let Some(e) = reg.entries.iter_mut().find(|e| e.dir == new.dir) {
            if e.state == EntryState::Watching && e.same_config(&new) {
                return Ok(Change::Unchanged);
            }
            let old = std::mem::replace(e, new);
            Ok(Change::Replaced(old.state))
        } else {
            reg.entries.push(new);
            Ok(Change::Added)
        }
    })?;
    let log = logger(store);
    let fields = [
        ("name", v(name.clone())),
        ("dir", v(dir.display().to_string())),
        ("id", v(short(&entry.id))),
        ("steps", v(nsteps)),
    ];
    match change {
        Change::Added => {
            log.event("registered", &fields);
            println!(
                "watching {} as {name} ({nsteps} step{})",
                dir.display(),
                plural(nsteps)
            );
        }
        Change::Unchanged => {
            println!("already watching {} as {name} (unchanged)", dir.display());
        }
        Change::Replaced(old) => {
            log.event("replaced", &fields);
            println!(
                "watching {} as {name} ({nsteps} step{}, replaced the previous registration)",
                dir.display(),
                plural(nsteps)
            );
            if matches!(old, EntryState::Triggered { .. }) {
                eprintln!(
                    "note: steps still pending from the previous registration were cancelled"
                );
            }
        }
    }
    if store.daemon_pid().is_none() {
        eprintln!(
            "note: the gone daemon is not running; start `gone daemon` (see `gone service print`)"
        );
    }
    Ok(0)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn new_id() -> String {
    use std::io::Read;
    let mut buf = [0u8; 8];
    let random = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf));
    if random.is_err() {
        let t = Ts::now().0 ^ (u64::from(std::process::id()) << 32);
        buf = t.to_le_bytes();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Lexically absolute: joins onto cwd and folds `.`/`..`, without touching
/// the filesystem (the directory may be gone).
fn absolutize(p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(p)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Finds a registration by name, or by directory (gone or not).
fn resolve<'a>(reg: &'a Registry, target: &str) -> Result<&'a Entry> {
    let by_name: Vec<&Entry> = reg.entries.iter().filter(|e| e.name == target).collect();
    let looks_like_path = target.contains('/') || target == "." || target == "..";
    if by_name.len() == 1 && !looks_like_path {
        return Ok(by_name[0]);
    }
    let lexical = absolutize(Path::new(target));
    let canonical = std::fs::canonicalize(&lexical).ok();
    if let Some(e) = reg
        .entries
        .iter()
        .find(|e| e.dir == lexical || Some(&e.dir) == canonical.as_ref())
    {
        return Ok(e);
    }
    match by_name.len() {
        0 => bail!("no registration named or at {target:?} (see `gone list`)"),
        1 => Ok(by_name[0]),
        _ => {
            let dirs: Vec<String> = by_name
                .iter()
                .map(|e| e.dir.display().to_string())
                .collect();
            bail!(
                "{target:?} names several registrations; give the directory: {}",
                dirs.join(", ")
            )
        }
    }
}

fn list(store: &Store, json: bool) -> Result<i32> {
    let reg = store.load()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&reg)?);
        return Ok(0);
    }
    let out = &mut std::io::stdout().lock();
    match store.daemon_pid() {
        Some(pid) => writeln!(out, "daemon: running (pid {pid})")?,
        None => writeln!(
            out,
            "daemon: NOT running (start `gone daemon`; see `gone service print`)"
        )?,
    }
    if reg.entries.is_empty() {
        writeln!(
            out,
            "no registrations (add one with `gone watch DIR -- COMMAND...`)"
        )?;
        return Ok(0);
    }
    let now = Ts::now();
    for e in &reg.entries {
        writeln!(out)?;
        writeln!(
            out,
            "{}  [{}]  {}",
            e.name,
            state_text(e, now),
            e.dir.display()
        )?;
        for (i, s) in e.steps.iter().enumerate() {
            let when = if s.after.is_zero() {
                "now".to_string()
            } else {
                format!("+{}", s.after)
            };
            writeln!(out, "  {}. {when:<6} {}", i + 1, s.cmd.display())?;
            if let Some(status) = step_text(store, e, i, now) {
                writeln!(out, "     {status}")?;
            }
        }
    }
    Ok(0)
}

fn state_text(e: &Entry, now: Ts) -> String {
    match &e.state {
        EntryState::Watching => match daemon::probe_dir(&e.dir) {
            Probe::Absent => "watching, dir is gone: the daemon will act".into(),
            Probe::Present => "watching".into(),
            Probe::Error(err) => format!("watching, cannot check dir: {err}"),
        },
        EntryState::Triggered { gone_at, .. } => {
            format!("triggered {} ago", approx(now.since(*gone_at)))
        }
        EntryState::Finished { at, .. } => {
            let kind = if e.state.kind() == "done" {
                "done"
            } else {
                "FAILED"
            };
            format!("{kind} {} ago", approx(now.since(*at)))
        }
    }
}

fn result_text(r: &RunResult) -> String {
    let what = match (&r.error, r.exit_code) {
        (Some(err), _) => err.clone(),
        (None, Some(c)) => format!("exit {c}"),
        (None, None) => "ok".into(),
    };
    format!("{what}, took {}", approx(r.took))
}

fn record_text(r: &StepRecord, now: Ts) -> String {
    let ago = approx(now.since(r.result.at.plus(r.result.took)));
    let mut s = if r.ok() {
        format!("ok {ago} ago ({})", result_text(&r.result))
    } else {
        format!(
            "FAILED after {} attempt{} {ago} ago: {}",
            r.attempts,
            plural(r.attempts as usize),
            result_text(&r.result)
        )
    };
    if let Some(p) = &r.result.output {
        s.push_str(&format!("\n     output: {}", p.display()));
    }
    s
}

fn step_text(store: &Store, e: &Entry, i: usize, now: Ts) -> Option<String> {
    match &e.state {
        EntryState::Watching => None,
        EntryState::Finished { steps, .. } => steps.get(i).map(|r| record_text(r, now)),
        EntryState::Triggered {
            trigger,
            gone_at,
            done,
            current,
        } => {
            if let Some(r) = done.get(i) {
                return Some(record_text(r, now));
            }
            if i > done.len() {
                let due = gone_at.plus(e.steps[i].after);
                return Some(if due > now {
                    format!("pending, due in {} at the earliest", approx(due.since(now)))
                } else {
                    format!("pending, after step {}", done.len() + 1)
                });
            }
            Some(match &current.phase {
                Phase::Waiting { due_at } => {
                    let when = if *due_at > now {
                        format!("in {}", approx(due_at.since(now)))
                    } else {
                        "now".into()
                    };
                    match &current.last {
                        Some(last) if current.failures > 0 => format!(
                            "attempt {} failed: {}; retry {when}",
                            current.failures,
                            result_text(last)
                        ),
                        _ => format!("due {when}"),
                    }
                }
                Phase::Running { claim } => {
                    let files = store.run_files(&e.id, *trigger, i, claim.attempt);
                    match files.probe() {
                        ClaimProbe::Running => format!(
                            "running for {} (attempt {}, {})",
                            approx(now.since(claim.since)),
                            claim.attempt,
                            claim.owner.describe()
                        ),
                        _ => format!(
                            "attempt {} ended while its owner ({}) was away; \
                             the daemon will record it",
                            claim.attempt,
                            claim.owner.describe()
                        ),
                    }
                }
            })
        }
    }
}

fn run(store: &Store, target: &str, dry_run: bool) -> Result<i32> {
    let reg = store.load()?;
    let entry = resolve(&reg, target)?.clone();
    if dry_run {
        println!(
            "dry run: would run {} step{} for {} ({}), nothing is executed",
            entry.steps.len(),
            plural(entry.steps.len()),
            entry.name,
            entry.dir.display()
        );
        println!("cwd: {}", entry.cwd.display());
        println!("env (the whole environment):");
        for (k, val) in step_env(&entry) {
            println!("  {k}={val}");
        }
        for (i, s) in entry.steps.iter().enumerate() {
            let delay = if s.after.is_zero() {
                String::new()
            } else {
                format!(
                    "  (when deleted: +{} after the delete; `run` ignores the delay)",
                    s.after
                )
            };
            println!("{}. {}{delay}", i + 1, s.cmd.display());
        }
        return Ok(0);
    }

    // Ctrl-C reaches the step (it shares our process group); we stay alive
    // to record what happened and stop.
    let interrupted = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, interrupted.clone())?;
    let exe = std::env::current_exe().context("finding the gone binary")?;
    let owner = Owner::Manual {
        pid: std::process::id(),
    };
    let id = entry.id.clone();
    let log = logger(store);
    let base = |e: &Entry| {
        vec![
            ("name", v(e.name.clone())),
            ("dir", v(e.dir.display().to_string())),
            ("id", v(short(&e.id))),
        ]
    };
    log.event("manual_run", &base(&entry));
    let mut failed = 0;
    loop {
        let now = Ts::now();
        let reg = store.load()?;
        let Some(e) = reg.entries.iter().find(|e| e.id == id) else {
            eprintln!("gone: the registration was replaced or forgotten; stopping");
            return Ok(1);
        };
        // A claim left by someone who is gone: record it first.
        if let Some((i, claim)) = e.claim_held() {
            let trigger = e.state.trigger().unwrap_or(0);
            let files = store.run_files(&id, trigger, i, claim.attempt);
            match files.probe() {
                ClaimProbe::Running => bail!(
                    "step {} is being run by {}; try again when it is done",
                    i + 1,
                    claim.owner.describe()
                ),
                probe => {
                    let claim = claim.clone();
                    store.update(|reg| {
                        if let Some(e) = reg.entries.iter_mut().find(|e| e.id == id) {
                            core::resolve(e, &claim, probe, &RetryPolicy::ONCE, now);
                        }
                        Ok(())
                    })?;
                    files.cleanup();
                    continue;
                }
            }
        }
        // What claim_manual will produce, so the run lock can be taken first.
        let (trigger, step, attempt) = match e.current() {
            Some((i, cur)) => (e.state.trigger().unwrap_or(0), i, cur.failures + 1),
            None => (e.triggers + 1, 0, 1),
        };
        let files = store.run_files(&id, trigger, step, attempt);
        let lock = files.acquire()?;
        let owner2 = owner.clone();
        let (entry, claim) = store.update(|reg| {
            let e = reg
                .entries
                .iter_mut()
                .find(|e| e.id == id)
                .context("the registration was replaced or forgotten")?;
            let (i, claim) = e.claim_manual(owner2, now).map_err(anyhow::Error::msg)?;
            if (e.state.trigger(), i, claim.attempt) != (Some(trigger), step, attempt) {
                bail!("the registration changed meanwhile; try again");
            }
            Ok((e.clone(), claim))
        })?;
        let cmd = &entry.steps[step].cmd;
        eprintln!(
            "gone: step {}/{}: {}",
            step + 1,
            entry.steps.len(),
            cmd.display()
        );
        let env = step_env(&entry);
        let result = exec::run(&Job {
            cmd,
            cwd: &entry.cwd,
            env: &env,
            timeout: None,
            output: Output::Inherit,
            own_group: false,
            supervised: Some(Supervised {
                exe: &exe,
                files: &files,
                lock: &lock,
            }),
        });
        let summary = result.error.clone().unwrap_or_else(|| "ok".into());
        eprintln!(
            "gone: step {}: {summary} ({})",
            step + 1,
            approx(result.took)
        );
        let mut fields = base(&entry);
        fields.extend([
            ("step", v(step + 1)),
            ("program", v(cmd.program())),
            ("exit", result.exit_code.map(v).unwrap_or(Value::Null)),
            ("took", v(approx(result.took))),
            ("error", result.error.clone().map(v).unwrap_or(Value::Null)),
            ("manual", v(true)),
        ]);
        let stop = interrupted.load(Ordering::Relaxed);
        let done = Ts::now();
        let f = store.update(|reg| {
            let Some(e) = reg.entries.iter_mut().find(|e| e.id == id) else {
                return Ok(None);
            };
            let f = e.finish(&claim, result, &RetryPolicy::ONCE, done);
            if stop && f.is_some() {
                // Don't leave the rest of an interrupted manual run to the
                // daemon: re-arm, so only a real delete runs it.
                e.state = EntryState::Watching;
            }
            Ok(f)
        })?;
        files.cleanup();
        drop(lock);
        let Some(f) = f else {
            eprintln!("gone: the registration was replaced or forgotten meanwhile; stopping");
            return Ok(1);
        };
        if f.outcome != Outcome::Ok {
            failed += 1;
        }
        log.event(
            if f.outcome == Outcome::Ok {
                "step_ok"
            } else {
                "step_failed"
            },
            &fields,
        );
        if stop {
            log.event("manual_run_interrupted", &base(&entry));
            eprintln!("gone: interrupted; the registration is watching again");
            return Ok(130);
        }
        if let Some(all_ok) = f.concluded {
            log.event(if all_ok { "done" } else { "failed" }, &base(&entry));
            return Ok(if failed > 0 { 1 } else { 0 });
        }
    }
}

fn forget(store: &Store, target: &str) -> Result<i32> {
    let reg = store.load()?;
    let e = resolve(&reg, target)?;
    let id = e.id.clone();
    let removed = store.update(|reg| {
        let pos = reg.entries.iter().position(|e| e.id == id);
        Ok(pos.map(|p| reg.entries.remove(p)))
    })?;
    let Some(e) = removed else {
        bail!("the registration changed meanwhile; try again");
    };
    logger(store).event(
        "forgotten",
        &[
            ("name", v(e.name.clone())),
            ("dir", v(e.dir.display().to_string())),
            ("id", v(short(&e.id))),
        ],
    );
    println!("forgot {} ({})", e.name, e.dir.display());
    Ok(0)
}

fn log(store: &Store, name: Option<&str>, lines: usize, json: bool) -> Result<i32> {
    let file = match std::fs::File::open(store.events_path()) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no events yet");
            return Ok(0);
        }
        Err(e) => return Err(e.into()),
    };
    let mut keep: std::collections::VecDeque<(String, Option<Value>)> = Default::default();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Option<Value> = serde_json::from_str(&line).ok();
        if let Some(n) = name
            && parsed
                .as_ref()
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                != Some(n)
        {
            continue;
        }
        keep.push_back((line, parsed));
        if lines > 0 && keep.len() > lines {
            keep.pop_front();
        }
    }
    let out = &mut std::io::stdout().lock();
    for (raw, parsed) in keep {
        match (json, parsed) {
            (false, Some(p)) => writeln!(out, "{}", format_line(&p))?,
            _ => writeln!(out, "{raw}")?,
        }
    }
    Ok(0)
}

fn run_daemon(store: &Store, a: &DaemonArgs) -> Result<i32> {
    let cfg = daemon::Config {
        settle: a.settle.std(),
        sweep_interval: a
            .sweep_interval
            .std()
            .max(std::time::Duration::from_millis(100)),
        tick: a.tick.std().max(std::time::Duration::from_millis(10)),
        retry: RetryPolicy {
            base: a.retry_base,
            max: a.retry_max.max(a.retry_base),
            max_attempts: a.max_attempts,
        },
        step_timeout: (!a.step_timeout.is_zero()).then(|| a.step_timeout.std()),
        shutdown_grace: a.shutdown_grace.std(),
        inotify: !a.no_inotify,
    };
    daemon::run(store, &cfg)?;
    Ok(0)
}

fn service_print(store: &Store) -> Result<i32> {
    let exe = std::env::current_exe().context("finding the gone binary")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    print!("{}", unit_text(&exe, store.root()));
    eprintln!(
        "\n# To install (gone does not do this for you):\n\
         #   gone service print > ~/.config/systemd/user/gone.service\n\
         #   systemctl --user daemon-reload && systemctl --user enable --now gone.service\n\
         #   loginctl enable-linger \"$USER\"   # optional: keep it running while logged out"
    );
    Ok(0)
}

pub fn unit_text(exe: &Path, state_dir: &Path) -> String {
    format!(
        "\
[Unit]
Description=gone: run teardown commands when watched directories are deleted

[Service]
Type=simple
ExecStart={exe} daemon
Environment=GONE_STATE_DIR={state}
Restart=on-failure
RestartSec=5s
# Steps outlive the daemon on purpose: on stop or crash they keep running,
# and the next daemon records their exit status instead of re-running them.
KillMode=process
TimeoutStopSec=60s

[Install]
WantedBy=default.target
",
        exe = systemd_escape(&exe.display().to_string()),
        state = systemd_escape(&state_dir.display().to_string()),
    )
}

fn systemd_escape(s: &str) -> String {
    if s.contains(|c: char| c.is_whitespace() || c == '"' || c == '\\') {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.replace('%', "%%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Dur;

    #[test]
    fn absolutize_folds_dots_without_the_filesystem() {
        assert_eq!(
            absolutize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert!(absolutize(Path::new("x")).is_absolute());
    }

    #[test]
    fn unit_runs_the_daemon_with_the_state_dir() {
        let u = unit_text(
            Path::new("/opt/bin/gone"),
            Path::new("/home/u/.local/state/gone"),
        );
        assert!(u.contains("ExecStart=/opt/bin/gone daemon\n"), "{u}");
        assert!(
            u.contains("Environment=GONE_STATE_DIR=/home/u/.local/state/gone\n"),
            "{u}"
        );
        assert!(u.contains("WantedBy=default.target"));
        let u = unit_text(Path::new("/my bin/gone"), Path::new("/s"));
        assert!(u.contains("ExecStart=\"/my bin/gone\" daemon"), "{u}");
    }

    #[test]
    fn ids_are_16_hex_chars_and_differ() {
        let (a, b) = (new_id(), new_id());
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn dur_is_used_for_delays() {
        // Guard against accidentally showing ms where the README promises units.
        assert_eq!(format!("+{}", Dur(86_400_000)), "+1d");
    }
}
