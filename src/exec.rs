//! Runs one attempt of a step.
//!
//! Steps run without a shell (argv) or with `/bin/sh -c`, in a cleared
//! environment, with stdin from /dev/null. When the attempt has run files
//! (always, for the daemon and `gone run`), the step runs under
//! `gone __supervise`: a tiny parent that inherits the attempt's run lock,
//! holds it until the step exits, and then writes the exit status next to it.
//! That is what lets a daemon that crashed and restarted tell an attempt that
//! is still running, from one that finished (and how), from one that was lost
//! — instead of re-running it.

use crate::model::{Cmd, RunResult};
use crate::store::RunFiles;
use crate::time::{Dur, Ts};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub enum Output {
    /// Share the caller's stdout/stderr (manual `gone run`).
    Inherit,
    /// Capture stdout+stderr into this file (the daemon).
    File(PathBuf),
}

/// Run under `gone __supervise`, holding `lock` (already locked by the
/// owner) for the attempt's lifetime.
pub struct Supervised<'a> {
    pub exe: &'a Path,
    pub files: &'a RunFiles,
    pub lock: &'a File,
}

pub struct Job<'a> {
    pub cmd: &'a Cmd,
    pub cwd: &'a Path,
    pub env: &'a BTreeMap<String, String>,
    pub timeout: Option<Duration>,
    pub output: Output,
    /// Its own process group (the daemon, so a timeout can stop everything
    /// it started and a terminal's Ctrl-C does not reach it). `gone run`
    /// keeps the terminal's group so Ctrl-C stops the step.
    pub own_group: bool,
    pub supervised: Option<Supervised<'a>>,
}

const KILL_GRACE: Duration = Duration::from_secs(5);

/// The exit status a supervisor leaves behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepStatus {
    pub started: Ts,
    pub ended: Ts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_error: Option<String>,
}

impl StepStatus {
    pub fn to_result(&self, output: Option<PathBuf>) -> RunResult {
        let error = if let Some(e) = &self.spawn_error {
            Some(e.clone())
        } else if let Some(sig) = self.signal {
            Some(format!("killed by signal {sig}"))
        } else {
            match self.exit_code {
                Some(0) => None,
                Some(c) => Some(format!("exit status {c}")),
                None => Some("no exit status".into()),
            }
        };
        RunResult {
            at: self.started,
            ok: error.is_none(),
            exit_code: self.exit_code,
            error,
            took: self.ended.since(self.started),
            output,
        }
    }
}

/// Runs the job to completion.
pub fn run(job: &Job) -> RunResult {
    let started = Instant::now();
    let at = Ts::now();
    let output_path = match &job.output {
        Output::File(p) => Some(p.clone()),
        Output::Inherit => None,
    };
    let fail = |error: String| RunResult {
        at,
        ok: false,
        exit_code: None,
        error: Some(error),
        took: Dur::from_std(started.elapsed()),
        output: output_path.clone(),
    };

    let argv = job.cmd.argv();
    if argv.first().is_none_or(|p| p.is_empty()) {
        return fail("empty command".into());
    }
    let mut command = match &job.supervised {
        Some(s) => {
            let mut c = Command::new(s.exe);
            c.arg("__supervise")
                .arg("--status")
                .arg(&s.files.status)
                .arg("--lock-fd")
                .arg(s.lock.as_raw_fd().to_string())
                .arg("--")
                .args(&argv);
            let fd = s.lock.as_raw_fd();
            // SAFETY: only fcntl, which is async-signal-safe, between fork
            // and exec: the child keeps the locked fd open across exec.
            unsafe {
                c.pre_exec(move || {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            c
        }
        None => {
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]);
            c
        }
    };
    command
        .env_clear()
        .envs(job.env)
        .current_dir(job.cwd)
        .stdin(Stdio::null());
    if job.own_group {
        command.process_group(0);
    }
    if let Output::File(path) = &job.output {
        let file = match open_output(path).and_then(|f| Ok((f.try_clone()?, f))) {
            Ok(f) => f,
            Err(e) => return fail(format!("cannot open output file: {e}")),
        };
        command.stdout(file.0).stderr(file.1);
    }
    if !job.cwd.is_dir() {
        return fail(format!("cwd {} does not exist", job.cwd.display()));
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return fail(format!("cannot run {:?}: {e}", job.cmd.program())),
    };
    let pid = child.id();

    let mut timed_out = false;
    let mut kill_at: Option<Instant> = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(e) => break Err(e),
        }
        if let Some(limit) = job.timeout
            && !timed_out
            && started.elapsed() >= limit
        {
            timed_out = true;
            stop(pid, job.own_group, libc::SIGTERM);
            kill_at = Some(Instant::now() + KILL_GRACE);
        }
        if let Some(t) = kill_at
            && Instant::now() >= t
        {
            stop(pid, job.own_group, libc::SIGKILL);
            kill_at = None;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let status = match status {
        Ok(s) => s,
        Err(e) => return fail(format!("waiting for the step: {e}")),
    };
    if timed_out {
        return fail(format!(
            "timed out after {}",
            Dur::from_std(job.timeout.unwrap_or_default())
        ));
    }
    match &job.supervised {
        Some(s) => match s.files.read_status() {
            Some(st) => st.to_result(output_path),
            None => fail(format!(
                "the step's supervisor exited without a status ({})",
                describe(status)
            )),
        },
        None => direct_result(status, at, Dur::from_std(started.elapsed()), output_path),
    }
}

fn direct_result(status: ExitStatus, at: Ts, took: Dur, output: Option<PathBuf>) -> RunResult {
    let error = (!status.success()).then(|| describe(status));
    RunResult {
        at,
        ok: error.is_none(),
        exit_code: status.code(),
        error,
        took,
        output,
    }
}

fn describe(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(c), _) => format!("exit status {c}"),
        (None, Some(s)) => format!("killed by signal {s}"),
        _ => "unknown exit".into(),
    }
}

fn stop(pid: u32, group: bool, sig: i32) {
    if group {
        kill_group(pid, sig);
    } else if pid > 1 {
        // SAFETY: plain syscall.
        unsafe {
            libc::kill(pid as i32, sig);
        }
    }
}

/// `gone __supervise --status PATH --lock-fd N -- ARGV...`: runs ARGV,
/// holding fd N (the attempt's locked run-lock file) until it exits, then
/// writes the exit status to PATH. Returns the exit code to use.
pub fn supervise(status: &Path, lock_fd: i32, argv: &[OsString]) -> i32 {
    // Keep the lock to ourselves: the step must not inherit it, or a
    // background process it leaves behind would look like a running step.
    // SAFETY: plain fcntl/write on an fd we were handed.
    unsafe {
        libc::fcntl(lock_fd, libc::F_SETFD, libc::FD_CLOEXEC);
        let pid = format!("{}\n", std::process::id());
        libc::ftruncate(lock_fd, 0);
        libc::pwrite(lock_fd, pid.as_ptr().cast(), pid.len(), 0);
    }
    // A terminal's Ctrl-C reaches the step (same group under `gone run`);
    // we survive it to record what happened to the step.
    let caught = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, caught);

    let started = Ts::now();
    let st = match argv.split_first() {
        None => StepStatus {
            started,
            ended: Ts::now(),
            exit_code: None,
            signal: None,
            spawn_error: Some("empty command".into()),
        },
        Some((prog, args)) => match Command::new(prog).args(args).status() {
            Ok(s) => StepStatus {
                started,
                ended: Ts::now(),
                exit_code: s.code(),
                signal: s.signal(),
                spawn_error: None,
            },
            Err(e) => StepStatus {
                started,
                ended: Ts::now(),
                exit_code: None,
                signal: None,
                spawn_error: Some(format!("cannot run {:?}: {e}", prog.to_string_lossy())),
            },
        },
    };
    if let Err(e) = write_atomic(status, &serde_json::to_vec(&st).unwrap_or_default()) {
        eprintln!("gone: cannot write step status {}: {e}", status.display());
    }
    match (st.exit_code, st.signal) {
        (Some(c), _) => c,
        (None, Some(s)) => 128 + s,
        _ => 127,
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("status.tmp");
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(tmp, path)
}

/// Signals a whole process group (a daemon-run step's pid is its pgid).
pub fn kill_group(pgid: u32, sig: i32) {
    if pgid > 1 {
        // SAFETY: plain syscall; a negative pid addresses the process group.
        unsafe {
            libc::kill(-(pgid as i32), sig);
        }
    }
}

fn open_output(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("GONE_NAME".to_string(), "wt".to_string()),
        ])
    }

    fn job<'a>(cmd: &'a Cmd, env: &'a BTreeMap<String, String>, out: PathBuf) -> Job<'a> {
        Job {
            cmd,
            cwd: Path::new("/"),
            env,
            timeout: Some(Duration::from_secs(10)),
            output: Output::File(out),
            own_group: true,
            supervised: None,
        }
    }

    #[test]
    fn runs_with_only_the_given_env() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("o.log");
        let env = env();
        let cmd = Cmd::Shell("echo \"$GONE_NAME ${HOME:-nohome} $(pwd)\"".into());
        let r = run(&job(&cmd, &env, out.clone()));
        assert!(r.ok, "{r:?}");
        assert_eq!(std::fs::read_to_string(out).unwrap(), "wt nohome /\n");
    }

    #[test]
    fn argv_is_not_shell_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("o.log");
        let env = env();
        let cmd = Cmd::Argv(vec!["echo".into(), "$GONE_NAME; rm -rf /".into()]);
        assert!(run(&job(&cmd, &env, out.clone())).ok);
        assert_eq!(
            std::fs::read_to_string(out).unwrap(),
            "$GONE_NAME; rm -rf /\n"
        );
    }

    #[test]
    fn reports_exit_codes_missing_programs_and_timeouts() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env();
        let cmd = Cmd::Shell("exit 3".into());
        let r = run(&job(&cmd, &env, tmp.path().join("a")));
        assert_eq!(
            (r.ok, r.exit_code, r.error.as_deref()),
            (false, Some(3), Some("exit status 3"))
        );

        let cmd = Cmd::Argv(vec!["definitely-not-a-program-gone".into()]);
        let r = run(&job(&cmd, &env, tmp.path().join("b")));
        assert!(!r.ok);
        assert!(
            r.error
                .unwrap()
                .starts_with("cannot run \"definitely-not-a-program-gone\"")
        );

        let cmd = Cmd::Shell("sleep 30".into());
        let mut j = job(&cmd, &env, tmp.path().join("c"));
        j.timeout = Some(Duration::from_millis(200));
        let r = run(&j);
        assert_eq!(r.error.as_deref(), Some("timed out after 200ms"));
        assert!(r.took < Dur(5000));
    }

    #[test]
    fn status_maps_to_results() {
        let st = |exit_code, signal, spawn_error: Option<&str>| StepStatus {
            started: Ts(1000),
            ended: Ts(1500),
            exit_code,
            signal,
            spawn_error: spawn_error.map(String::from),
        };
        let r = st(Some(0), None, None).to_result(None);
        assert!(r.ok && r.took == Dur(500) && r.at == Ts(1000));
        assert_eq!(
            st(Some(2), None, None).to_result(None).error.as_deref(),
            Some("exit status 2")
        );
        assert_eq!(
            st(None, Some(9), None).to_result(None).error.as_deref(),
            Some("killed by signal 9")
        );
        assert!(!st(None, None, Some("nope")).to_result(None).ok);
    }
}
