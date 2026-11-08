//! End-to-end: the real binary, a real daemon, real directories in a tempdir.

mod common;

use common::Env;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);
/// Long enough for a duplicate run to show up if there were going to be one.
const QUIET: Duration = Duration::from_millis(1500);

#[test]
fn delete_runs_the_command_once() {
    let env = Env::new();
    let wt = env.dir("wt");
    env.watch(&wt, &["--shell", &env.mark_step("m")]);
    let _d = env.daemon(&[]);

    std::fs::remove_dir_all(&wt).unwrap();
    env.wait_for(WAIT, "the step to run", || env.runs("m") == 1);
    env.stays(QUIET, "a single run", || env.runs("m") == 1);

    assert_eq!(
        std::fs::read_to_string(env.marker("m")).unwrap(),
        "wt\n",
        "GONE_NAME is passed"
    );
    assert_eq!(env.entry_state(0), "done");
    assert_eq!(env.count("triggered"), 1);
    let list = env.ok(&["list"]);
    assert!(list.contains("[done"), "{list}");
    assert!(list.contains("ok "), "{list}");
}

#[test]
fn move_away_counts_as_deleted() {
    let env = Env::new();
    let wt = env.dir("wt");
    env.watch(&wt, &["--shell", &env.mark_step("m")]);
    let _d = env.daemon(&[]);

    std::fs::rename(&wt, env.root().join("elsewhere")).unwrap();
    env.wait_for(WAIT, "the step to run", || env.runs("m") == 1);
    env.stays(QUIET, "a single run", || env.runs("m") == 1);
}

#[test]
fn deleting_an_ancestor_counts() {
    let env = Env::new();
    let wt = env.dir("repo/worktrees/wt");
    env.watch(&wt, &["--shell", &env.mark_step("m")]);
    let _d = env.daemon(&[]);

    std::fs::remove_dir_all(env.root().join("w/repo")).unwrap();
    env.wait_for(WAIT, "the step to run", || env.runs("m") == 1);
    env.stays(QUIET, "a single run", || env.runs("m") == 1);
}

#[test]
fn moving_an_ancestor_away_counts() {
    let env = Env::new();
    let wt = env.dir("repo/worktrees/wt");
    env.watch(&wt, &["--shell", &env.mark_step("m")]);
    let _d = env.daemon(&[]);

    // Nothing inside `worktrees/` changes: only the move of `repo` itself.
    std::fs::rename(env.root().join("w/repo"), env.root().join("w/repo-old")).unwrap();
    env.wait_for(WAIT, "the step to run", || env.runs("m") == 1);
    assert!(
        env.events()
            .iter()
            .any(|e| e["event"] == "dir_gone" && e["via"] == "inotify")
    );
}

#[test]
fn recreated_within_settle_is_cancelled() {
    let env = Env::new();
    let wt = env.dir("wt");
    env.watch(&wt, &["--shell", &env.mark_step("m")]);
    let _d = env.daemon(&["--settle", "1500ms"]);

    std::fs::remove_dir_all(&wt).unwrap();
    env.wait_for(WAIT, "the delete to be noticed", || {
        env.count("dir_gone") == 1
    });
    std::fs::create_dir(&wt).unwrap();
    env.wait_for(WAIT, "the cancel", || env.count("cancelled") == 1);
    env.stays(Duration::from_millis(2000), "nothing running", || {
        env.runs("m") == 0
    });
    assert_eq!(env.entry_state(0), "watching");

    // Still armed: a real delete later runs it.
    std::fs::remove_dir_all(&wt).unwrap();
    env.wait_for(WAIT, "the step to run", || env.runs("m") == 1);
}

#[test]
fn deleted_while_daemon_was_down_is_caught_on_startup() {
    let env = Env::new();
    let wt = env.dir("wt");
    env.watch(&wt, &["--shell", &env.mark_step("m")]);
    std::fs::remove_dir_all(&wt).unwrap();
    assert_eq!(env.runs("m"), 0);

    let _d = env.daemon(&[]);
    env.wait_for(WAIT, "the step to run", || env.runs("m") == 1);
    assert!(
        env.events()
            .iter()
            .any(|e| e["event"] == "dir_gone" && e["via"] == "startup")
    );
}

#[test]
fn registry_changes_are_picked_up_without_restart() {
    let env = Env::new();
    let _d = env.daemon(&[]);
    let wt = env.dir("later");
    env.watch(&wt, &["--shell", &env.mark_step("m")]);
    env.wait_for(WAIT, "the new watch", || {
        env.events().iter().any(|e| e["event"] == "registered")
    });
    // Give the daemon a moment to add the watches for the new entry.
    std::thread::sleep(Duration::from_millis(300));
    std::fs::remove_dir_all(&wt).unwrap();
    env.wait_for(WAIT, "the step to run", || env.runs("m") == 1);
}

#[test]
fn delayed_step_runs_after_its_delay_and_survives_a_restart() {
    let env = Env::new();
    let wt = env.dir("wt");
    env.watch(
        &wt,
        &[
            "--shell",
            &env.mark_step("now"),
            "--after",
            "3s",
            "--shell",
            &env.mark_step("later"),
        ],
    );
    let d = env.daemon(&[]);
    std::fs::remove_dir_all(&wt).unwrap();
    let deleted = Instant::now();
    // Wait for the result to be recorded, not just the marker: a crash between
    // the two would (correctly) re-run the step as interrupted.
    env.wait_for(WAIT, "the first step", || env.count("step_ok") == 1);
    assert_eq!(env.runs("now"), 1);
    assert_eq!(env.runs("later"), 0);
    let list = env.ok(&["list"]);
    assert!(list.contains("due in"), "{list}");

    // A crash, not a clean stop: the delay must come from the registry.
    d.crash();
    let _d = env.daemon(&[]);
    env.wait_for(WAIT, "the delayed step", || env.runs("later") == 1);
    let elapsed = deleted.elapsed();
    assert!(
        elapsed >= Duration::from_secs(3),
        "ran after {elapsed:?}, before its 3s delay"
    );
    env.stays(QUIET, "single runs", || {
        env.runs("now") == 1 && env.runs("later") == 1
    });
    assert_eq!(env.entry_state(0), "done");
}

#[test]
fn failing_command_retries_then_reports() {
    let env = Env::new();
    let wt = env.dir("wt");
    let step = format!("{}; exit 3", env.mark_step("m"));
    env.watch(&wt, &["--shell", &step]);
    let _d = env.daemon(&[]);

    std::fs::remove_dir_all(&wt).unwrap();
    env.wait_for(WAIT, "the entry to fail", || env.entry_state(0) == "failed");
    assert_eq!(env.runs("m"), 3, "--max-attempts 3");
    let failures: Vec<_> = env
        .events()
        .into_iter()
        .filter(|e| e["event"] == "step_failed")
        .collect();
    assert_eq!(failures.len(), 3);
    assert_eq!(failures[0]["exit"], 3);
    assert!(failures[0].get("retry_in").is_some());
    assert_eq!(failures[2]["gave_up"], true);
    assert_eq!(env.count("failed"), 1);

    let list = env.ok(&["list"]);
    assert!(list.contains("[FAILED"), "{list}");
    assert!(list.contains("FAILED after 3 attempts"), "{list}");
    assert!(list.contains("exit status 3"), "{list}");
    let log = env.ok(&["log"]);
    assert!(log.contains("gave_up=true"), "{log}");
}

#[test]
fn flaky_command_succeeds_on_retry() {
    let env = Env::new();
    let wt = env.dir("wt");
    let m = env.marker("m");
    // Fails the first time, succeeds the second.
    let step = format!("test -e '{0}' && exit 0; touch '{0}'; exit 1", m.display());
    env.watch(&wt, &["--shell", &step]);
    let _d = env.daemon(&[]);

    std::fs::remove_dir_all(&wt).unwrap();
    env.wait_for(WAIT, "the entry to finish", || env.entry_state(0) == "done");
    assert_eq!(env.count("step_failed"), 1);
    assert_eq!(env.count("step_ok"), 1);
}

#[test]
fn concurrent_watch_writers_do_not_corrupt_the_registry() {
    let env = Env::new();
    let n = 24;
    let dirs: Vec<_> = (0..n).map(|i| env.dir(&format!("wt{i}"))).collect();
    let children: Vec<_> = dirs
        .iter()
        .map(|d| {
            env.cmd()
                .args(["watch", &d.display().to_string(), "--", "true"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    let reg = env.registry();
    let entries = reg["entries"].as_array().unwrap();
    assert_eq!(entries.len(), n);
    let mut names: Vec<_> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), n);
    assert_eq!(env.count("registered"), n);
}

#[test]
fn dry_run_runs_nothing() {
    let env = Env::new();
    let wt = env.dir("wt");
    env.watch(
        &wt,
        &[
            "--shell",
            &env.mark_step("m"),
            "--after",
            "1d",
            "--",
            "touch",
            &env.marker("argv").display().to_string(),
        ],
    );
    let before = std::fs::read(env.state().join("registry.json")).unwrap();

    let out = env.ok(&["run", "wt", "--dry-run"]);
    assert!(out.contains("nothing is executed"), "{out}");
    assert!(out.contains("GONE_NAME=wt"), "{out}");
    assert!(out.contains("+1d after the delete"), "{out}");
    assert!(out.contains("touch "), "{out}");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(env.runs("m"), 0);
    assert!(!env.marker("argv").exists());
    assert_eq!(
        std::fs::read(env.state().join("registry.json")).unwrap(),
        before
    );
}

#[test]
fn manual_run_runs_every_step_now() {
    let env = Env::new();
    let wt = env.dir("wt");
    env.watch(
        &wt,
        &[
            "--shell",
            &env.mark_step("a"),
            "--after",
            "1d",
            "--shell",
            &env.mark_step("b"),
        ],
    );
    env.ok(&["run", &wt.display().to_string()]);
    assert_eq!((env.runs("a"), env.runs("b")), (1, 1));
    assert_eq!(env.entry_state(0), "done");

    // A failing manual run exits non-zero and is recorded.
    let wt2 = env.dir("wt2");
    env.watch(&wt2, &["--", "false"]);
    let out = env.gone(&["run", "wt2"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(env.entry_state(1), "failed");
}

#[test]
fn permission_error_is_not_a_delete() {
    let env = Env::new();
    let wt = env.dir("locked/wt");
    env.watch(&wt, &["--shell", &env.mark_step("m")]);
    let _d = env.daemon(&[]);
    let locked = env.root().join("w/locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Can't stat through a 000 dir unless we're root, where the test means nothing.
    if std::fs::symlink_metadata(&wt).is_ok() {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipping: running as root");
        return;
    }
    env.wait_for(WAIT, "the check error", || env.count("check_error") >= 1);
    env.stays(QUIET, "nothing running", || env.runs("m") == 0);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(env.count("dir_gone"), 0);
    assert_eq!(env.entry_state(0), "watching");
}

#[test]
fn re_registering_replaces_pending_steps() {
    let env = Env::new();
    let wt = env.dir("wt");
    let steps = [
        "--shell",
        &env.mark_step("now"),
        "--after",
        "2s",
        "--shell",
        &env.mark_step("later"),
    ];
    env.watch(&wt, &steps);
    let _d = env.daemon(&[]);
    std::fs::remove_dir_all(&wt).unwrap();
    env.wait_for(WAIT, "the first step", || env.runs("now") == 1);

    // The worktree comes back and the stack is started again.
    let wt = env.dir("wt");
    let out = env.watch(&wt, &steps);
    assert!(out.contains("replaced"), "{out}");
    env.stays(
        Duration::from_millis(3000),
        "the old delayed step not running",
        || env.runs("later") == 0,
    );
    assert_eq!(env.entry_state(0), "watching");
}

#[test]
fn watch_is_idempotent() {
    let env = Env::new();
    let wt = env.dir("wt");
    env.watch(&wt, &["--", "true"]);
    let out = env.watch(&wt, &["--", "true"]);
    assert!(out.contains("unchanged"), "{out}");
    let out = env.watch(&wt, &["--", "true", "x"]);
    assert!(out.contains("replaced"), "{out}");
    assert_eq!(env.registry()["entries"].as_array().unwrap().len(), 1);
}

#[test]
fn forget_removes_without_running() {
    let env = Env::new();
    let wt = env.dir("wt");
    env.watch(&wt, &["--shell", &env.mark_step("m")]);
    let _d = env.daemon(&[]);
    env.ok(&["forget", "wt"]);
    std::thread::sleep(Duration::from_millis(300));
    std::fs::remove_dir_all(&wt).unwrap();
    env.stays(QUIET, "nothing running", || env.runs("m") == 0);
    assert!(env.registry()["entries"].as_array().unwrap().is_empty());
    assert!(env.gone(&["forget", "wt"]).status.code() == Some(2));
}

#[test]
fn only_one_daemon_runs() {
    let env = Env::new();
    let _d = env.daemon(&[]);
    let out = env.gone(&["daemon"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already running"));
}

#[test]
fn crash_mid_step_is_adopted_not_rerun() {
    let env = Env::new();
    let wt = env.dir("wt");
    let step = format!("{}; sleep 2", env.mark_step("m"));
    env.watch(&wt, &["--shell", &step]);
    let d = env.daemon(&[]);
    std::fs::remove_dir_all(&wt).unwrap();
    env.wait_for(WAIT, "the step to start", || env.runs("m") == 1);
    d.crash();
    // The step keeps running without the daemon; the next one adopts it and
    // records its real exit status when it ends.
    let _d = env.daemon(&[]);
    env.wait_for(WAIT, "the step to be recorded", || {
        env.entry_state(0) == "done"
    });
    assert_eq!(env.count("step_adopted"), 1);
    assert_eq!(env.count("step_recovered"), 1);
    assert_eq!(env.count("step_interrupted"), 0);
    assert_eq!(env.runs("m"), 1, "not re-run");
}

#[test]
fn lost_step_is_retried() {
    let env = Env::new();
    let wt = env.dir("wt");
    let m = env.marker("m");
    // First attempt hangs; the retry finishes at once.
    let step = format!(
        "{}; [ \"$(wc -l < '{}')\" -gt 1 ] || sleep 30",
        env.mark_step("m"),
        m.display()
    );
    env.watch(&wt, &["--shell", &step]);
    let d = env.daemon(&[]);
    std::fs::remove_dir_all(&wt).unwrap();
    env.wait_for(WAIT, "the step to start", || env.runs("m") == 1);
    d.crash();
    // Now the step dies too, without leaving a status (as on a reboot).
    let pgid = env.step_pgid();
    // SAFETY: plain syscall on the test's own process group.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
    let _d = env.daemon(&[]);
    env.wait_for(WAIT, "the retry to finish", || env.entry_state(0) == "done");
    assert_eq!(env.count("step_interrupted"), 1);
    assert_eq!(env.runs("m"), 2, "the lost attempt is retried");
}

#[test]
fn periodic_sweep_catches_a_delete_without_events() {
    let env = Env::new();
    let gone = env.dir("gone");
    let kept = env.dir("kept");
    env.watch(&gone, &["--shell", &env.mark_step("gone")]);
    env.watch(&kept, &["--shell", &env.mark_step("kept")]);
    // No inotify at all: only the sweep can notice.
    let _d = env.daemon(&["--no-inotify", "--sweep-interval", "1s"]);
    std::fs::remove_dir_all(&gone).unwrap();
    env.wait_for(WAIT, "the sweep to catch it", || env.runs("gone") == 1);
    assert!(
        env.events()
            .iter()
            .any(|e| e["event"] == "dir_gone" && e["via"] == "sweep")
    );
    env.stays(
        Duration::from_millis(2500),
        "the present dir left alone",
        || env.runs("kept") == 0,
    );
}

#[test]
fn watch_rejects_a_cwd_inside_the_dir() {
    let env = Env::new();
    let wt = env.dir("wt");
    let d = wt.display().to_string();
    let out = env.gone(&["watch", &d, "--cwd", &d, "--", "true"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("inside the watched directory"));
}

#[test]
fn steps_get_only_the_captured_env() {
    let env = Env::new();
    let wt = env.dir("wt");
    let out = env.marker("env");
    let step = format!("env | sort > '{}'", out.display());
    env.cmd()
        .args([
            "watch",
            &wt.display().to_string(),
            "--env",
            "EXTRA=1",
            "--shell",
            &step,
        ])
        .env("SECRET_TOKEN", "nope")
        .output()
        .unwrap();
    env.ok(&["run", "wt"]);
    let got = std::fs::read_to_string(out).unwrap();
    let keys: Vec<&str> = got
        .lines()
        .filter_map(|l| l.split('=').next())
        .filter(|k| !["PWD", "SHLVL", "_", "OLDPWD"].contains(k))
        .collect();
    assert_eq!(
        keys,
        ["EXTRA", "GONE_DIR", "GONE_NAME", "HOME", "PATH"],
        "{got}"
    );
    // And nothing secret or argument-like reaches the log.
    let log = std::fs::read_to_string(env.state().join("events.jsonl")).unwrap();
    assert!(
        !log.contains("nope") && !log.contains("env | sort"),
        "{log}"
    );
}

#[test]
fn stopped_daemon_leaves_the_step_running_for_the_next_one() {
    let env = Env::new();
    let wt = env.dir("wt");
    let step = format!("sleep 1.5; {}", env.mark_step("m"));
    env.watch(&wt, &["--shell", &step]);
    let d = env.daemon(&["--shutdown-grace", "100ms"]);
    std::fs::remove_dir_all(&wt).unwrap();
    env.wait_for(WAIT, "the step to start", || env.count("step_start") == 1);
    // A clean stop does not wait for, or kill, the running step.
    d.stop();
    let stop = env
        .events()
        .into_iter()
        .rfind(|e| e["event"] == "daemon_stop")
        .unwrap();
    assert_eq!(stop["left_running"], 1);
    env.wait_for(WAIT, "the step to finish on its own", || env.runs("m") == 1);
    let _d = env.daemon(&[]);
    env.wait_for(WAIT, "the next daemon to record it", || {
        env.entry_state(0) == "done"
    });
    assert_eq!(env.count("step_recovered"), 1);
    assert_eq!(env.runs("m"), 1);
}

#[test]
fn interrupted_manual_run_rearms_the_registration() {
    use std::os::unix::process::CommandExt;
    let env = Env::new();
    let wt = env.dir("wt");
    let later = env.mark_step("later");
    env.watch(
        &wt,
        &["--shell", "sleep 30", "--after", "1d", "--shell", &later],
    );
    // Like a terminal: gone run and its step share a process group, and
    // Ctrl-C goes to the whole group.
    let mut child = env
        .cmd()
        .args(["run", "wt"])
        .process_group(0)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    env.wait_for(WAIT, "the step to start", || {
        std::fs::read_dir(env.state().join("runs"))
            .map(|d| {
                d.flatten()
                    .any(|f| f.path().extension().is_some_and(|x| x == "lock"))
            })
            .unwrap_or(false)
            && env.entry_state(0) == "triggered"
    });
    std::thread::sleep(Duration::from_millis(200));
    // SAFETY: plain syscall on the child's own process group.
    unsafe { libc::kill(-(child.id() as i32), libc::SIGINT) };
    assert_eq!(child.wait().unwrap().code(), Some(130));
    assert_eq!(
        env.entry_state(0),
        "watching",
        "the rest is not left to the daemon"
    );
    assert_eq!(env.count("manual_run_interrupted"), 1);
    assert_eq!(env.runs("later"), 0);
}
