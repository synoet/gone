//! The state directory: `registry.json` (the registrations, rewritten
//! atomically under a lock), `events.jsonl` (the append-only action log),
//! `runs/` (captured output of daemon-run steps) and `daemon.lock`.

use crate::model::{REGISTRY_VERSION, Registry};
use anyhow::{Context, Result, bail};
use std::fs::{self, File, OpenOptions, TryLockError};
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
        let reg: Registry = serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is not a valid registry (fix or move it aside)",
                path.display()
            )
        })?;
        if reg.version != REGISTRY_VERSION {
            bail!(
                "{} has version {}, this gone understands {REGISTRY_VERSION}",
                path.display(),
                reg.version
            );
        }
        Ok(reg)
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
    pub fn lock_daemon(&self) -> Result<DaemonLock> {
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
        writeln!(file, "{}", std::process::id())?;
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

fn read_pid(file: &mut File) -> Option<u32> {
    let mut s = String::new();
    file.rewind().ok()?;
    file.read_to_string(&mut s).ok()?;
    s.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Cmd, Entry, EntryState, Step, StepStatus};
    use crate::time::{Dur, Ts};

    fn entry(name: &str) -> Entry {
        Entry {
            id: name.into(),
            name: name.into(),
            dir: format!("/w/{name}").into(),
            cwd: "/".into(),
            env: Default::default(),
            steps: vec![Step {
                cmd: Cmd::Argv(vec!["true".into()]),
                after: Dur::ZERO,
                status: StepStatus::Pending,
                attempts: 0,
                last: None,
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
        let lock = store.lock_daemon().unwrap();
        assert_eq!(store.daemon_pid(), Some(std::process::id()));
        assert!(store.lock_daemon().is_err());
        drop(lock);
        assert_eq!(store.daemon_pid(), None);
    }
}
