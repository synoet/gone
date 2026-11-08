//! Test harness: a temp tree with a state dir, watched dirs and markers, the
//! real `gone` binary, and a real daemon process with fast timings.

#![allow(dead_code)]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_gone");

/// Fast timings so tests finish quickly; settle is long enough to beat a
/// scripted rmdir+mkdir, short enough not to slow everything down.
pub const FAST: &[&str] = &[
    "--settle",
    "300ms",
    "--sweep-interval",
    "1s",
    "--tick",
    "50ms",
    "--retry-base",
    "200ms",
    "--retry-max",
    "1s",
    "--max-attempts",
    "3",
    "--shutdown-grace",
    "2s",
];

pub struct Env {
    pub tmp: tempfile::TempDir,
}

impl Env {
    pub fn new() -> Env {
        let tmp = tempfile::Builder::new()
            .prefix("gone-test-")
            .tempdir()
            .unwrap();
        for d in ["state", "w", "markers", "home"] {
            std::fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
        Env { tmp }
    }

    pub fn root(&self) -> &Path {
        self.tmp.path()
    }

    pub fn state(&self) -> PathBuf {
        self.root().join("state")
    }

    /// Creates (and returns) a directory under `w/`.
    pub fn dir(&self, rel: &str) -> PathBuf {
        let p = self.root().join("w").join(rel);
        std::fs::create_dir_all(&p).unwrap();
        // Something inside, like a real worktree.
        std::fs::write(p.join("compose.yaml"), "services: {}\n").unwrap();
        p
    }

    pub fn marker(&self, name: &str) -> PathBuf {
        self.root().join("markers").join(name)
    }

    /// Lines written to a marker (one per run of a marker step).
    pub fn runs(&self, name: &str) -> usize {
        std::fs::read_to_string(self.marker(name))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// A shell step that appends a line to `markers/<name>`.
    pub fn mark_step(&self, name: &str) -> String {
        format!("echo \"$GONE_NAME\" >> '{}'", self.marker(name).display())
    }

    pub fn cmd(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env("GONE_STATE_DIR", self.state())
            .env("HOME", self.root().join("home"))
            .env_remove("XDG_STATE_HOME");
        c
    }

    pub fn gone(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    /// Runs gone and asserts success, returning stdout.
    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.gone(args);
        assert!(
            out.status.success(),
            "gone {args:?} failed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    pub fn watch(&self, dir: &Path, rest: &[&str]) -> String {
        let d = dir.display().to_string();
        let mut args = vec!["watch", d.as_str()];
        args.extend(rest);
        self.ok(&args)
    }

    pub fn daemon(&self, extra: &[&str]) -> Daemon {
        let mut args = vec!["daemon"];
        for pair in FAST.chunks(2) {
            if !extra.contains(&pair[0]) {
                args.extend(pair);
            }
        }
        args.extend(extra);
        let child = self
            .cmd()
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let d = Daemon { child: Some(child) };
        let start = self.events().len();
        self.wait_for(Duration::from_secs(10), "daemon ready", || {
            self.events()[start.min(self.events().len())..]
                .iter()
                .any(|e| e["event"] == "ready")
        });
        d
    }

    pub fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(self.state().join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("event lines are JSON"))
            .collect()
    }

    pub fn count(&self, event: &str) -> usize {
        self.events().iter().filter(|e| e["event"] == event).count()
    }

    pub fn registry(&self) -> Value {
        let text = std::fs::read_to_string(self.state().join("registry.json")).unwrap();
        serde_json::from_str(&text).expect("registry is valid JSON")
    }

    pub fn entry_state(&self, i: usize) -> String {
        self.registry()["entries"][i]["state"]["kind"]
            .as_str()
            .unwrap()
            .to_string()
    }

    pub fn wait_for(&self, timeout: Duration, what: &str, mut cond: impl FnMut() -> bool) {
        let start = Instant::now();
        while !cond() {
            if start.elapsed() > timeout {
                panic!(
                    "timed out after {timeout:?} waiting for {what}\nevents:\n{}",
                    self.ok(&["log", "-n", "0"])
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Waits a while and asserts `cond` stays true the whole time.
    pub fn stays(&self, d: Duration, what: &str, cond: impl Fn() -> bool) {
        let start = Instant::now();
        while start.elapsed() < d {
            assert!(
                cond(),
                "{what} stopped holding\n{}",
                self.ok(&["log", "-n", "0"])
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

pub struct Daemon {
    child: Option<Child>,
}

impl Daemon {
    /// SIGTERM and wait: a clean stop.
    pub fn stop(mut self) {
        if let Some(mut c) = self.child.take() {
            // SAFETY: plain syscall on our own child.
            unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
            c.wait().unwrap();
        }
    }

    /// SIGKILL: a crash.
    pub fn crash(mut self) {
        if let Some(mut c) = self.child.take() {
            c.kill().unwrap();
            c.wait().unwrap();
        }
    }

    pub fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.as_mut().and_then(|c| c.try_wait().unwrap())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
