//! Reading older registry formats. `Store::load` reads the `version` field
//! and converts; the next write saves the current format.
//!
//! Version 1 kept run state on every step (`status`, `attempts`, `last`),
//! even while the entry was watching. Version 2 keeps it only inside the
//! trigger (see `model`).

use crate::model::{
    Claim, Cmd, Current, Entry, EntryState, Owner, Phase, REGISTRY_VERSION, Registry, RunResult,
    StepConfig, StepRecord,
};
use crate::time::{Dur, Ts};
use anyhow::{Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Parses a registry file of any known version.
pub fn parse(text: &str) -> Result<Registry> {
    #[derive(Deserialize)]
    struct Version {
        version: u32,
    }
    let Version { version } = serde_json::from_str(text)?;
    match version {
        1 => Ok(from_v1(serde_json::from_str(text)?)),
        REGISTRY_VERSION => Ok(serde_json::from_str(text)?),
        v => bail!("registry version {v}; this gone understands 1 to {REGISTRY_VERSION}"),
    }
}

#[derive(Deserialize)]
struct V1Registry {
    #[serde(default)]
    entries: Vec<V1Entry>,
}

#[derive(Deserialize)]
struct V1Entry {
    id: String,
    name: String,
    dir: PathBuf,
    cwd: PathBuf,
    #[serde(default)]
    env: BTreeMap<String, String>,
    steps: Vec<V1Step>,
    registered_at: Ts,
    #[serde(default)]
    state: V1State,
}

#[derive(Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum V1State {
    #[default]
    Watching,
    Triggered {
        gone_at: Ts,
    },
    Done {
        gone_at: Ts,
        at: Ts,
    },
    Failed {
        gone_at: Ts,
        at: Ts,
    },
}

#[derive(Deserialize)]
struct V1Step {
    cmd: Cmd,
    #[serde(default)]
    after: Dur,
    #[serde(default)]
    status: V1Status,
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    last: Option<RunResult>,
}

#[derive(Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum V1Status {
    #[default]
    Pending,
    Running {
        pid: u32,
        since: Ts,
    },
    Retry {
        next_at: Ts,
    },
    Ok {
        at: Ts,
    },
    Failed {
        at: Ts,
    },
}

fn from_v1(old: V1Registry) -> Registry {
    Registry {
        version: REGISTRY_VERSION,
        entries: old.entries.into_iter().map(entry_from_v1).collect(),
    }
}

fn record(s: &V1Step, at: Ts, ok: bool) -> StepRecord {
    let result = s.last.clone().unwrap_or(RunResult {
        at,
        ok,
        exit_code: None,
        error: (!ok).then(|| "failed (details lost in migration)".into()),
        took: Dur::ZERO,
        output: None,
    });
    StepRecord {
        attempts: s.attempts.max(1),
        result,
    }
}

fn entry_from_v1(e: V1Entry) -> Entry {
    let steps: Vec<StepConfig> = e
        .steps
        .iter()
        .map(|s| StepConfig {
            cmd: s.cmd.clone(),
            after: s.after,
        })
        .collect();
    let records = |upto: usize| -> Vec<StepRecord> {
        e.steps[..upto]
            .iter()
            .map(|s| match s.status {
                V1Status::Ok { at } => record(s, at, true),
                V1Status::Failed { at } => record(s, at, false),
                _ => record(s, e.registered_at, false),
            })
            .collect()
    };
    let triggers = u32::from(!matches!(e.state, V1State::Watching));
    let state = match e.state {
        V1State::Watching => EntryState::Watching,
        V1State::Done { gone_at, at } | V1State::Failed { gone_at, at } => EntryState::Finished {
            trigger: 1,
            gone_at,
            at,
            steps: records(e.steps.len()),
        },
        V1State::Triggered { gone_at } => {
            // v1 also ran steps in order: finished ones form a prefix.
            let i = e
                .steps
                .iter()
                .position(|s| !matches!(s.status, V1Status::Ok { .. } | V1Status::Failed { .. }))
                .unwrap_or(e.steps.len());
            if i == e.steps.len() {
                EntryState::Finished {
                    trigger: 1,
                    gone_at,
                    at: gone_at,
                    steps: records(i),
                }
            } else {
                let s = &e.steps[i];
                let phase = match s.status {
                    V1Status::Retry { next_at } => Phase::Waiting { due_at: next_at },
                    // A v1 daemon held it. Its instance can't match a v2
                    // daemon, so the new daemon recovers the claim.
                    V1Status::Running { pid, since } => Phase::Running {
                        claim: Claim {
                            owner: Owner::Daemon {
                                pid,
                                instance: "v1".into(),
                            },
                            since,
                            attempt: s.attempts + 1,
                        },
                    },
                    _ => Phase::Waiting {
                        due_at: gone_at.plus(s.after),
                    },
                };
                EntryState::Triggered {
                    trigger: 1,
                    gone_at,
                    done: records(i),
                    current: Current {
                        failures: s.attempts,
                        last: s.last.clone(),
                        phase,
                    },
                }
            }
        }
    };
    Entry {
        id: e.id,
        name: e.name,
        dir: e.dir,
        cwd: e.cwd,
        env: e.env,
        steps,
        registered_at: e.registered_at,
        triggers,
        state,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Written by gone 0.1.0 (registry version 1): one watching entry, one
    /// done, one with step 1 done and step 2 waiting a day, one mid-run.
    const V1: &str = r#"{
  "version": 1,
  "entries": [
    {
      "id": "aaaaaaaaaaaaaaaa", "name": "watching", "dir": "/w/a", "cwd": "/home/u",
      "env": {"PATH": "/usr/bin:/bin"},
      "steps": [{"cmd": {"argv": ["true"]}, "status": {"kind": "pending"}, "attempts": 0}],
      "registered_at": "2026-11-08T18:00:00.000Z",
      "state": {"kind": "watching"}
    },
    {
      "id": "bbbbbbbbbbbbbbbb", "name": "done", "dir": "/w/b", "cwd": "/home/u",
      "steps": [{"cmd": {"shell": "echo hi"}, "status": {"kind": "ok", "at": "2026-11-08T18:01:00.000Z"}, "attempts": 1,
                 "last": {"at": "2026-11-08T18:00:59.000Z", "ok": true, "exit_code": 0, "took": "25ms"}}],
      "registered_at": "2026-11-08T18:00:00.000Z",
      "state": {"kind": "done", "gone_at": "2026-11-08T18:00:50.000Z", "at": "2026-11-08T18:01:00.000Z"}
    },
    {
      "id": "cccccccccccccccc", "name": "delayed", "dir": "/w/c", "cwd": "/home/u",
      "steps": [
        {"cmd": {"argv": ["true"]}, "status": {"kind": "ok", "at": "2026-11-08T18:01:00.000Z"}, "attempts": 1,
         "last": {"at": "2026-11-08T18:01:00.000Z", "ok": true, "exit_code": 0, "took": "1s"}},
        {"cmd": {"argv": ["true"]}, "after": "1d", "status": {"kind": "pending"}, "attempts": 0}
      ],
      "registered_at": "2026-11-08T18:00:00.000Z",
      "state": {"kind": "triggered", "gone_at": "2026-11-08T18:00:50.000Z"}
    },
    {
      "id": "dddddddddddddddd", "name": "running", "dir": "/w/d", "cwd": "/home/u",
      "steps": [{"cmd": {"argv": ["false"]}, "status": {"kind": "running", "pid": 4242, "since": "2026-11-08T18:02:00.000Z"}, "attempts": 1,
                 "last": {"at": "2026-11-08T18:01:00.000Z", "ok": false, "exit_code": 1, "error": "exit status 1", "took": "1s"}}],
      "registered_at": "2026-11-08T18:00:00.000Z",
      "state": {"kind": "triggered", "gone_at": "2026-11-08T18:00:50.000Z"}
    }
  ]
}"#;

    fn ts(s: &str) -> Ts {
        Ts::parse(s).unwrap()
    }

    #[test]
    fn v1_registry_still_loads() {
        let reg = parse(V1).unwrap();
        assert_eq!(reg.version, REGISTRY_VERSION);
        let [a, b, c, d] = &reg.entries[..] else {
            panic!("{reg:?}")
        };
        assert_eq!(a.state, EntryState::Watching);
        assert_eq!(a.steps[0].cmd, Cmd::Argv(vec!["true".into()]));
        assert_eq!(a.env["PATH"], "/usr/bin:/bin");

        assert_eq!(b.state.kind(), "done");

        assert_eq!(c.next_due(), Some((1, ts("2026-11-09T18:00:50.000Z"))));
        assert_eq!(c.steps[1].after, Dur(86_400_000));

        let (i, claim) = d.claim_held().unwrap();
        assert_eq!((i, claim.attempt), (0, 2));
        assert!(matches!(&claim.owner, Owner::Daemon { pid: 4242, instance } if instance == "v1"));
        let (_, cur) = d.current().unwrap();
        assert_eq!(cur.failures, 1);

        // And it writes back as the current version.
        let json = serde_json::to_string(&reg).unwrap();
        assert_eq!(parse(&json).unwrap(), reg);
    }

    #[test]
    fn unknown_versions_are_refused() {
        assert!(parse(r#"{"version": 99, "entries": []}"#).is_err());
        assert!(parse(r#"{"entries": []}"#).is_err());
    }
}
