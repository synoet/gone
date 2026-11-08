# AGENTS.md

Notes for agents (and people) changing `gone`. Read `README.md` first for
what it does and why; its "How it decides a directory is gone" and "Steps,
claims and crashes" sections describe the state machines below.

## Layout

Pure (no IO, no clock: every function takes `now`):

- `src/presence.rs`: the per-directory belief `Present | Settling | Unknown`
  and `observe(state, probe, now, settle)`. In memory only.
- `src/model.rs`: registry types and the run state machine. Step config
  (`StepConfig`) has no run state; run state is only in
  `EntryState::Triggered { trigger, gone_at, done, current }` and
  `Finished`. `current.phase` is `Waiting { due_at }` or `Running { claim }`.
- `src/core.rs`: the daemon's decisions over a `&mut Registry`: which dirs
  to check, applying checks, which steps to claim, resolving claims whose
  owner is gone, lifecycle (`Starting | Steady | Draining`).
- `src/migrate.rs`: reads registry v1 (gone 0.1) into v2.
- `src/time.rs`: `Ts` (RFC 3339, clamped to year 9999) and `Dur` (`24h`).

IO:

- `src/daemon.rs`: the shell around `Core`: checks dirs, holds locks, runs
  steps in threads, logs. inotify watches are a cache rebuilt from the
  registry, not state.
- `src/exec.rs`: runs one attempt; `supervise` is `gone __supervise`, which
  holds the attempt's run lock while the step runs and writes its status.
- `src/store.rs`: the state dir. `Store::update` is the only way to write
  the registry (exclusive `flock`, temp file + fsync + rename). `RunFiles`
  are one attempt's `.lock`, `.status` and `.log`.
- `src/events.rs`, `src/cli.rs`, `src/commands.rs`: log, clap, subcommands.

Tests:

- `tests/machines.rs`: proptest-state-machine over `Core` + `model` with a
  simulated world. Read its module docs before changing the core.
- `tests/paths.rs`: path-shape properties (pure + real tempdirs).
- `tests/daemon.rs` + `tests/common/mod.rs`: end to end with the real binary.

## Invariants (the property tests check these; don't break them)

1. **No teardown while the dir is present, and an error never triggers.**
   Trigger only from `presence::observe` after a full settle of absent
   checks with nothing else in between. Only ENOENT/ENOTDIR are absent
   (`daemon::probe_dir`).
2. **A deletion triggers at most once.** Only `Entry::trigger` moves
   `Watching → Triggered`, inside `Store::update`. Re-arming is a new
   `gone watch` (new `id`).
3. **One owner per attempt, recorded only in the registry.** Claim under
   the lock (`Entry::claim` / `claim_manual`), finish only with the exact
   claim (`Entry::finish`). Don't add in-memory "who is running" maps; derive
   them (`Core::mine`, `Core::foreign`).
4. **Liveness is the run lock, not a pid.** The owner takes the attempt's
   run lock before claiming and keeps it until it has recorded the result;
   the supervisor inherits it. Lock held → running; free with a status →
   record it; free without → interrupted, retried.
5. **Steps outlive the daemon.** Stopping or crashing never kills running
   steps; the next daemon adopts them. Only the step timeout kills.
6. **A delayed step never runs early** (`due_at = gone_at + after`, or a
   retry backoff) and its due time is in the registry, so it survives
   restarts.
7. **Replace and forget cancel everything pending** of the old registration;
   results for an old `id` are dropped.
8. **The registry round-trips.** Anything written must read back identical;
   bump `REGISTRY_VERSION` and extend `migrate.rs` for incompatible changes.
9. **Logs never contain step arguments, env values or step output.** Log
   `Cmd::program()`, not `Cmd::display()`.
10. **Steps never run inside the watched directory** and get only
    `Entry::env` + `GONE_DIR`/`GONE_NAME`.
11. `gone service print` prints; nothing in `gone` installs or enables units.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

All three must pass before a commit. After touching `presence.rs`,
`model.rs` or `core.rs`, also run the deep property suite once:

```sh
PROPTEST_CASES=10000 cargo test --release --test machines --test paths
```

When proptest finds something, fix it and add a plain regression test next
to the code (see `time.rs` `far_future_times_clamp_and_still_round_trip`,
`model.rs` `triggers_in_the_same_millisecond_are_still_distinct`). The
integration tests use real timing with short durations (`tests/common`
`FAST`): wait on events or registry state (`env.wait_for`, `env.count`),
never on sleeps, and remember a marker file appears before the daemon
records the result.

## Conventions

- Small Conventional Commits (`feat:`, `fix:`, `test:`, `docs:`).
- Keep it generic: no Docker, git or herdr knowledge in the code. Examples
  in docs are fine.
- Local only: no remote, no publishing.
