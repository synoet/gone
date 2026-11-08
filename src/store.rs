//! The state directory: `registry.json` (the registrations, rewritten
//! atomically under a lock), `events.jsonl` (the append-only action log),
//! `runs/` (captured output of daemon-run steps) and `daemon.lock`.

use crate::core::ClaimProbe;
use crate::exec::StepStatus;
use crate::model::Registry;
use crate::time::Ts;
use anyhow::{Context, Result, bail};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub struct Store {
    root: PathBuf,
}

/// Held for as long as the daemon runs; dropping it releases the lock.
pub struct DaemonLock {
    _file: File,
}

impl Store {
    /// `$GONE_STATE_DIR` is handled by the CLI; this is the fallback:
    /// `$XDG_STATE_HOME/gone`, else `~/.local/state/gone`.
    pub fn default_root() -> Result<PathBuf> {
        if let Some(x) = std::env::var_os("XDG_STATE_HOME").filter(|x| !x.is_empty()) {
            let x = PathBuf::from(x);
            if x.is_absolute() {
                return Ok(x.join("gone"));
            }
        }
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .context("neither XDG_STATE_HOME nor HOME is set; pass --state-dir")?;
        Ok(PathBuf::from(home).join(".local/state/gone"))
    }

    pub fn open(root: impl Into<PathBuf>) -> Result<Store> {
        let root = root.into();
        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()?.join(root)
        };
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
            .with_context(|| format!("creating state dir {}", root.display()))?;
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn registry_path(&self) -> PathBuf {
        self.root.join("registry.json")
    }

    pub fn events_path(&self) -> PathBuf {
        self.root.join("events.jsonl")
    }

    pub fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }

    /// The files of one attempt: `<id>-<gone_at ms>-step<N>-attempt<M>.*`
    /// under `runs/`. The trigger time is part of the name so a later manual
    /// re-run of the same registration never reads an earlier status.
    pub fn run_files(&self, id: &str, gone_at: Ts, step: usize, attempt: u32) -> RunFiles {
        let base = format!("{id}-{}-step{}-attempt{attempt}", gone_at.0, step + 1);
        let dir = self.runs_dir();
        RunFiles {
            lock: dir.join(format!("{base}.lock")),
            status: dir.join(format!("{base}.status")),
            output: dir.join(format!("{base}.log")),
        }
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join("registry.lock")
    }

    fn daemon_lock_path(&self) -> PathBuf {
        self.root.join("daemon.lock")
    }

    /// Reads the registry without locking (writers replace it atomically, so
    /// a reader sees either the old or the new file). Missing means empty.
    pub fn load(&self) -> Result<Registry> {
        let path = self.registry_path();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Registry::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        crate::migrate::parse(&text).with_context(|| {
            format!(
                "{} is not a valid registry (fix or move it aside)",
                path.display()
            )
        })
    }

    /// Read-modify-write under an exclusive lock, so concurrent `gone watch`
    /// calls and the daemon never lose each other's changes. The file is only
    /// rewritten if `f` changed something.
    pub fn update<T>(&self, f: impl FnOnce(&mut Registry) -> Result<T>) -> Result<T> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.lock_path())
            .context("opening registry lock")?;
        lock.lock().context("locking registry")?;
        let mut reg = self.load()?;
        let before = reg.clone();
        let out = f(&mut reg)?;
        if reg != before {
            self.save(&reg)?;
        }
        Ok(out)
    }

    fn save(&self, reg: &Registry) -> Result<()> {
        let path = self.registry_path();
        let tmp = self
            .root
            .join(format!(".registry.json.{}.tmp", std::process::id()));
        let mut json = serde_json::to_string_pretty(reg)?;
        json.push('\n');
        {
            let mut f = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("writing {}", tmp.display()))?;
            f.write_all(json.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    /// Takes the single-daemon lock, or fails if another daemon holds it.
    pub fn lock_daemon(&self, instance: &str) -> Result<DaemonLock> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(self.daemon_lock_path())
            .context("opening daemon lock")?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let pid = read_pid(&mut file);
                bail!(
                    "another gone daemon is already running{} for {}",
                    pid.map(|p| format!(" (pid {p})")).unwrap_or_default(),
                    self.root.display()
                );
            }
            Err(TryLockError::Error(e)) => return Err(e).context("locking daemon lock"),
        }
        file.set_len(0)?;
        file.rewind()?;
        writeln!(file, "{} {instance}", std::process::id())?;
        Ok(DaemonLock { _file: file })
    }

    /// The running daemon's pid, or `None` if no daemon holds the lock.
    pub fn daemon_pid(&self) -> Option<u32> {
        let mut file = File::open(self.daemon_lock_path()).ok()?;
        match file.try_lock() {
            Ok(()) => None,
            Err(TryLockError::WouldBlock) => Some(read_pid(&mut file).unwrap_or(0)),
            Err(TryLockError::Error(_)) => None,
        }
    }
}

/// The files of one attempt of one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFiles {
    /// Locked by the owner before it claims, and by the step's supervisor for
    /// as long as the step runs: held means running.
    pub lock: PathBuf,
    /// The step's exit status, written by its supervisor when it ends.
    pub status: PathBuf,
    /// Captured stdout+stderr (daemon runs).
    pub output: PathBuf,
}

impl RunFiles {
    /// Locks this attempt for the caller, before it claims it in the
    /// registry. Fails if a process already holds it.
    pub fn acquire(&self) -> Result<File> {
        if let Some(dir) = self.lock.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&self.lock)
            .with_context(|| format!("opening {}", self.lock.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => bail!("this attempt is already running"),
            Err(TryLockError::Error(e)) => return Err(e).context("locking run lock"),
        }
        match fs::remove_file(&self.status) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("clearing an old status"),
        }
        Ok(file)
    }

    pub fn read_status(&self) -> Option<StepStatus> {
        let bytes = fs::read(&self.status).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// The supervisor's pid (also its process group), while it runs.
    pub fn pid(&self) -> Option<u32> {
        read_pid(&mut File::open(&self.lock).ok()?)
    }

    /// What became of an attempt whose owner is not us.
    pub fn probe(&self) -> ClaimProbe {
        let held = match File::open(&self.lock) {
            Ok(f) => matches!(f.try_lock(), Err(TryLockError::WouldBlock)),
            Err(_) => false,
        };
        if held {
            return ClaimProbe::Running;
        }
        match self.read_status() {
            Some(st) => ClaimProbe::Exited(st.to_result(Some(self.output.clone()))),
            None => ClaimProbe::Lost,
        }
    }

    /// Removes the lock and status once the result is recorded (the output
    /// log stays, for `gone list`).
    pub fn cleanup(&self) {
        let _ = fs::remove_file(&self.status);
        let _ = fs::remove_file(&self.lock);
    }
}

fn read_pid(file: &mut File) -> Option<u32> {
    let mut s = String::new();
    file.rewind().ok()?;
    file.read_to_string(&mut s).ok()?;
    s.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Cmd, Entry, EntryState, StepConfig};
    use crate::time::Dur;

    fn entry(name: &str) -> Entry {
        Entry {
            id: name.into(),
            name: name.into(),
            dir: format!("/w/{name}").into(),
            cwd: "/".into(),
            env: Default::default(),
            steps: vec![StepConfig {
                cmd: Cmd::Argv(vec!["true".into()]),
                after: Dur::ZERO,
            }],
            registered_at: Ts(0),
            state: EntryState::Watching,
        }
    }

    #[test]
    fn missing_registry_is_empty_and_update_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("s")).unwrap();
        assert!(store.load().unwrap().entries.is_empty());
        store
            .update(|r| {
                r.entries.push(entry("a"));
                Ok(())
            })
            .unwrap();
        assert_eq!(store.load().unwrap().entries[0].name, "a");
        let leftovers: Vec<_> = fs::read_dir(store.root())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn corrupt_registry_is_reported_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        fs::write(store.registry_path(), "{ not json").unwrap();
        assert!(store.load().is_err());
        assert!(store.update(|_| Ok(())).is_err());
        assert_eq!(
            fs::read_to_string(store.registry_path()).unwrap(),
            "{ not json"
        );
    }

    #[test]
    fn concurrent_updates_from_threads_all_land() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let store = Store::open(root).unwrap();
                    store
                        .update(|r| {
                            r.entries.push(entry(&format!("e{i}")));
                            Ok(())
                        })
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            Store::open(&root).unwrap().load().unwrap().entries.len(),
            16
        );
    }

    #[test]
    fn daemon_lock_is_exclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        assert_eq!(store.daemon_pid(), None);
        let lock = store.lock_daemon("i1").unwrap();
        assert_eq!(store.daemon_pid(), Some(std::process::id()));
        assert!(store.lock_daemon("i2").is_err());
        drop(lock);
        assert_eq!(store.daemon_pid(), None);
    }

    #[test]
    fn run_files_tell_running_from_exited_from_lost() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let rf = store.run_files("abc", Ts(5), 0, 1);
        assert!(rf.lock.ends_with("runs/abc-5-step1-attempt1.lock"));
        assert_eq!(rf.probe(), ClaimProbe::Lost, "never started");
        let lock = rf.acquire().unwrap();
        assert_eq!(rf.probe(), ClaimProbe::Running);
        assert!(rf.acquire().is_err(), "one holder at a time");
        drop(lock);
        assert_eq!(rf.probe(), ClaimProbe::Lost, "died without a status");
        let st = StepStatus {
            started: Ts(10),
            ended: Ts(20),
            exit_code: Some(0),
            signal: None,
            spawn_error: None,
        };
        fs::write(&rf.status, serde_json::to_vec(&st).unwrap()).unwrap();
        assert!(matches!(rf.probe(), ClaimProbe::Exited(r) if r.ok));
        // Acquiring for a new run clears a stale status.
        let _l = rf.acquire().unwrap();
        assert!(rf.read_status().is_none());
    }
}
