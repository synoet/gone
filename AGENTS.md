# AGENTS.md

Notes for agents (and people) changing `gone`. Read `README.md` first for
what it does and why.

## Layout

- `src/model.rs`: registry types and the state machine (trigger, claim,
  finish, retry, conclude). Pure: every method takes `now: Ts`; nothing here
  reads the clock or touches the filesystem. Most logic changes belong here,
  with a unit test.
- `src/store.rs`: the state dir. `Store::update` is the only way to write
  the registry (exclusive `flock` on `registry.lock`, then temp file + fsync
  + rename). `Store::load` reads without the lock.
- `src/daemon.rs`: the loop: reload registry, sync inotify watches on
  ancestors, detect, settle, schedule, record results.
- `src/exec.rs`: runs one step (cleared env, own process group, timeout).
- `src/events.rs`: `events.jsonl` and its `key=value` rendering.
- `src/cli.rs`: clap definitions; `watch_steps` rebuilds the order of
  `--after/--shell/--exec/--` from clap's argument indices.
- `src/commands.rs`: one function per subcommand, plus the systemd unit text.
- `src/time.rs`: `Ts` (RFC 3339 in JSON) and `Dur` (`24h` in JSON).
- `tests/daemon.rs` + `tests/common/mod.rs`: end-to-end tests against the
  real binary and daemon in temp dirs.

## Invariants (don't break these)

1. **A registration triggers at most once.** Only `Entry::trigger` moves
   `watching → triggered`, and only inside `Store::update`. Re-arming is a
   new `gone watch`, which gets a new `id`.
2. **A step runs in one place.** Owners claim steps (`claim`, `claim_all`)
   under the lock and `finish` only accepts the owner's pid. Results for a
   replaced or forgotten registration (different `id`) are dropped.
3. **Only ENOENT/ENOTDIR mean gone** (`daemon::presence`). Never treat a
   permission error, EIO, or a failed watch as a delete.
4. **Settle before acting**, and cancel if the directory is back.
5. **Anything that must survive a restart lives in the registry** (trigger
   time, delayed steps, retry times). In-memory daemon state (settling,
   watches) must be safe to lose.
6. **Logs never contain step arguments, env values or step output.** Log
   `Cmd::program()`, not `Cmd::display()`.
7. **Steps never run inside the watched directory** and never inherit the
   daemon's environment: only `Entry::env` + `GONE_DIR`/`GONE_NAME`.
8. `gone service print` prints; nothing in `gone` installs or enables units.

Changing the registry format: bump `REGISTRY_VERSION` only for incompatible
changes; prefer `#[serde(default)]` fields.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

All three must pass before a commit. The integration tests take a few
seconds; they use real timing with short durations (`tests/common/mod.rs`
`FAST`), so when adding one, wait on events or registry state
(`env.wait_for`, `env.count`), not on sleeps, and remember that a marker
file appears before the daemon records the result.

## Conventions

- Small Conventional Commits (`feat:`, `fix:`, `test:`, `docs:`).
- Keep it generic: no Docker, git or herdr knowledge in the code. Examples
  in docs are fine.
- Local only: no remote, no publishing.
