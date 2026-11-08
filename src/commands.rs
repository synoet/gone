//! What each subcommand does.

use crate::cli::{Cli, Command, DaemonArgs, ServiceCommand, StepSpec, WatchArgs};
use crate::daemon::{self, short, step_env};
use crate::events::{Logger, format_line, v};
use crate::exec::{self, Job, Output};
use crate::model::{Entry, EntryState, Outcome, Registry, RetryPolicy, Step, StepStatus};
use crate::store::Store;
use crate::time::{Ts, approx};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};

const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

pub fn main(cli: Cli, steps: Vec<StepSpec>) -> Result<i32> {
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
            .map(|s| Step {
                cmd: s.cmd,
                after: s.after,
                status: StepStatus::Pending,
                attempts: 0,
                last: None,
            })
            .collect(),
        registered_at: Ts::now(),
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
            if let Some(status) = step_text(e, i, now) {
                writeln!(out, "     {status}")?;
            }
        }
    }
    Ok(0)
}

fn state_text(e: &Entry, now: Ts) -> String {
    match e.state {
        EntryState::Watching => match daemon::presence(&e.dir) {
            Ok(daemon::Presence::Absent) => "watching, dir is gone: the daemon will act".into(),
            Ok(daemon::Presence::Present) => "watching".into(),
            Err(err) => format!("watching, cannot check dir: {err}"),
        },
        EntryState::Triggered { gone_at } => {
            format!("triggered {} ago", approx(now.since(gone_at)))
        }
        EntryState::Done { at, .. } => format!("done {} ago", approx(now.since(at))),
        EntryState::Failed { at, .. } => format!("FAILED {} ago", approx(now.since(at))),
    }
}

fn step_text(e: &Entry, i: usize, now: Ts) -> Option<String> {
    let s = &e.steps[i];
    let last = s.last.as_ref().map(|r| {
        let what = match (&r.error, r.exit_code) {
            (Some(err), _) => err.clone(),
            (None, Some(c)) => format!("exit {c}"),
            (None, None) => "ok".into(),
        };
        format!("{what}, took {}", approx(r.took))
    });
    let text = match &s.status {
        StepStatus::Pending => match (&e.state, e.due_at(i)) {
            (EntryState::Watching, _) => return last.map(|l| format!("last run: {l}")),
            (_, Some(due)) if due > now => format!("due in {}", approx(due.since(now))),
            _ => "due now".into(),
        },
        StepStatus::Running { pid, since } => {
            format!("running for {} (pid {pid})", approx(now.since(*since)))
        }
        StepStatus::Retry { next_at } => format!(
            "attempt {} failed: {}; retry in {}",
            s.attempts,
            last.unwrap_or_default(),
            approx(next_at.since(now))
        ),
        StepStatus::Ok { at } => format!(
            "ok {} ago ({})",
            approx(now.since(*at)),
            last.unwrap_or_default()
        ),
        StepStatus::Failed { at } => format!(
            "FAILED after {} attempt{} {} ago: {}",
            s.attempts,
            plural(s.attempts as usize),
            approx(now.since(*at)),
            last.unwrap_or_default()
        ),
    };
    let output = s
        .last
        .as_ref()
        .and_then(|r| r.output.as_ref())
        .filter(|_| !matches!(s.status, StepStatus::Pending | StepStatus::Running { .. }))
        .map(|p| format!("\n     output: {}", p.display()))
        .unwrap_or_default();
    Some(text + &output)
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

    let pid = std::process::id();
    let id = entry.id.clone();
    let now = Ts::now();
    let (entry, claimed) = store.update(|reg| {
        let e = reg
            .entries
            .iter_mut()
            .find(|e| e.id == id)
            .context("the registration changed meanwhile; try again")?;
        let claimed = e
            .claim_all(pid, now, exec::pid_alive)
            .map_err(anyhow::Error::msg)?;
        Ok((e.clone(), claimed))
    })?;
    let log = logger(store);
    let base = [
        ("name", v(entry.name.clone())),
        ("dir", v(entry.dir.display().to_string())),
        ("id", v(short(&entry.id))),
    ];
    log.event(
        "manual_run",
        &[
            base[0].clone(),
            base[1].clone(),
            base[2].clone(),
            ("steps", v(claimed.len())),
        ],
    );
    if claimed.is_empty() {
        println!("nothing to run: every step already succeeded");
        return Ok(0);
    }
    let env = step_env(&entry);
    let mut failed = 0;
    for i in claimed {
        let step = &entry.steps[i];
        eprintln!(
            "gone: step {}/{}: {}",
            i + 1,
            entry.steps.len(),
            step.cmd.display()
        );
        let job = Job {
            cmd: &step.cmd,
            cwd: &entry.cwd,
            env: &env,
            timeout: None,
            output: Output::Inherit,
        };
        let result = exec::run(&job, |_| {});
        let summary = match &result.error {
            Some(e) => e.clone(),
            None => "ok".into(),
        };
        eprintln!("gone: step {}: {summary} ({})", i + 1, approx(result.took));
        let mut fields = base.to_vec();
        fields.extend([
            ("step", v(i + 1)),
            ("program", v(step.cmd.program())),
            ("exit", result.exit_code.map(v).unwrap_or(Value::Null)),
            ("took", v(approx(result.took))),
            ("error", result.error.clone().map(v).unwrap_or(Value::Null)),
            ("manual", v(true)),
        ]);
        let done = Ts::now();
        let f = store.update(|reg| {
            Ok(reg
                .entries
                .iter_mut()
                .find(|e| e.id == id)
                .and_then(|e| e.finish(i, pid, result, &RetryPolicy::ONCE, done)))
        })?;
        match f {
            Some(f) => {
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
                if let Some(state) = f.concluded {
                    log.event(state.kind(), &base);
                }
            }
            None => {
                eprintln!("gone: the registration was replaced or forgotten meanwhile; stopping");
                return Ok(1);
            }
        }
    }
    Ok(if failed > 0 { 1 } else { 0 })
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
Documentation=file://{exe_dir}

[Service]
Type=simple
ExecStart={exe} daemon
Environment=GONE_STATE_DIR={state}
Restart=on-failure
RestartSec=5s
# SIGTERM goes to the daemon only; it waits for running steps (see
# --shutdown-grace), then systemd kills what is left after TimeoutStopSec.
KillMode=mixed
TimeoutStopSec=60s

[Install]
WantedBy=default.target
",
        exe = systemd_escape(&exe.display().to_string()),
        exe_dir = exe.parent().unwrap_or(Path::new("/")).display(),
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
