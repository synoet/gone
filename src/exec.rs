//! Runs one step: no shell for argv steps, a cleared environment, its own
//! process group (so a timeout or shutdown can stop everything it started),
//! and stdin from /dev/null.

use crate::model::{Cmd, RunResult};
use crate::time::{Dur, Ts};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub enum Output {
    /// Share the caller's stdout/stderr (manual `gone run`).
    Inherit,
    /// Capture stdout+stderr into this file (the daemon).
    File(PathBuf),
}

pub struct Job<'a> {
    pub cmd: &'a Cmd,
    pub cwd: &'a Path,
    pub env: &'a BTreeMap<String, String>,
    pub timeout: Option<Duration>,
    pub output: Output,
}

const KILL_GRACE: Duration = Duration::from_secs(5);

/// Runs the job to completion. `on_spawn` gets the child's pid (which is
/// also its process group id) as soon as it exists.
pub fn run(job: &Job, on_spawn: impl FnOnce(u32)) -> RunResult {
    let started = Instant::now();
    let at = Ts::now();
    let fail = |error: String, output: Option<PathBuf>| RunResult {
        at,
        ok: false,
        exit_code: None,
        error: Some(error),
        took: Dur::from_std(started.elapsed()),
        output,
    };

    let mut command = match job.cmd {
        Cmd::Argv(argv) if !argv.is_empty() => {
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]);
            c
        }
        Cmd::Argv(_) => return fail("empty argv".into(), None),
        Cmd::Shell(script) => {
            let mut c = Command::new("/bin/sh");
            c.arg("-c").arg(script);
            c
        }
    };
    command
        .env_clear()
        .envs(job.env)
        .current_dir(job.cwd)
        .stdin(Stdio::null())
        .process_group(0);

    let output_path = match &job.output {
        Output::Inherit => None,
        Output::File(path) => {
            let file = match open_output(path) {
                Ok(f) => f,
                Err(e) => return fail(format!("cannot open output file: {e}"), None),
            };
            let err = match file.try_clone() {
                Ok(f) => f,
                Err(e) => return fail(format!("cannot open output file: {e}"), None),
            };
            command.stdout(file).stderr(err);
            Some(path.clone())
        }
    };

    if !job.cwd.is_dir() {
        return fail(
            format!("cwd {} does not exist", job.cwd.display()),
            output_path,
        );
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return fail(
                format!("cannot run {:?}: {e}", job.cmd.program()),
                output_path,
            );
        }
    };
    let pgid = child.id();
    on_spawn(pgid);

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
            kill_group(pgid, libc::SIGTERM);
            kill_at = Some(Instant::now() + KILL_GRACE);
        }
        if let Some(t) = kill_at
            && Instant::now() >= t
        {
            kill_group(pgid, libc::SIGKILL);
            kill_at = None;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let took = Dur::from_std(started.elapsed());
    let status = match status {
        Ok(s) => s,
        Err(e) => return fail(format!("waiting for child: {e}"), output_path),
    };
    let error = if timed_out {
        Some(format!(
            "timed out after {}",
            Dur::from_std(job.timeout.unwrap_or_default())
        ))
    } else if let Some(sig) = status.signal() {
        Some(format!("killed by signal {sig}"))
    } else if !status.success() {
        Some(format!("exit status {}", status.code().unwrap_or(-1)))
    } else {
        None
    };
    RunResult {
        at,
        ok: error.is_none(),
        exit_code: status.code(),
        error,
        took,
        output: output_path,
    }
}

/// Signals a step's whole process group (the step's pid is its pgid).
pub fn kill_group(pgid: u32, sig: i32) {
    if pgid > 1 {
        // SAFETY: plain syscall; a negative pid addresses the process group.
        unsafe {
            libc::kill(-(pgid as i32), sig);
        }
    }
}

/// True if `pid` exists (EPERM counts: it exists but isn't ours).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 only checks for existence.
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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
        }
    }

    #[test]
    fn runs_with_only_the_given_env() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("o.log");
        let env = env();
        let cmd = Cmd::Shell("echo \"$GONE_NAME ${HOME:-nohome} $(pwd)\"".into());
        let r = run(&job(&cmd, &env, out.clone()), |_| {});
        assert!(r.ok, "{r:?}");
        assert_eq!(std::fs::read_to_string(out).unwrap(), "wt nohome /\n");
    }

    #[test]
    fn argv_is_not_shell_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("o.log");
        let env = env();
        let cmd = Cmd::Argv(vec!["echo".into(), "$GONE_NAME; rm -rf /".into()]);
        let r = run(&job(&cmd, &env, out.clone()), |_| {});
        assert!(r.ok);
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
        let r = run(&job(&cmd, &env, tmp.path().join("a")), |_| {});
        assert_eq!(
            (r.ok, r.exit_code, r.error.as_deref()),
            (false, Some(3), Some("exit status 3"))
        );

        let cmd = Cmd::Argv(vec!["definitely-not-a-program-gone".into()]);
        let r = run(&job(&cmd, &env, tmp.path().join("b")), |_| {});
        assert!(!r.ok);
        assert!(
            r.error
                .unwrap()
                .starts_with("cannot run \"definitely-not-a-program-gone\"")
        );

        let cmd = Cmd::Shell("sleep 30".into());
        let mut j = job(&cmd, &env, tmp.path().join("c"));
        j.timeout = Some(Duration::from_millis(200));
        let r = run(&j, |_| {});
        assert_eq!(r.error.as_deref(), Some("timed out after 200ms"));
        assert!(r.took < Dur(5000));
    }
}
