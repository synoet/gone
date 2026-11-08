# gone

Run a registered teardown command when a watched directory is deleted.

You create a git worktree, something starts a local Docker stack in it, you
delete the worktree, and the stack keeps running: holding ports, subnets and
disk. Docker has no "stop when this folder goes away" setting and git has no
worktree-remove hook. `gone` fills that gap, and knows nothing about Docker or
git:

```sh
gone watch ~/wt/feature-x -- docker compose -p feature-x down
rm -rf ~/wt/feature-x        # ~10 s later: docker compose -p feature-x down
```

A small daemon (`gone daemon`, run by a systemd user unit) watches the
registered directories with inotify, and when one is really gone it runs that
directory's steps, once.

## The rule: teardown must not depend on the directory

By the time a step runs, the directory and everything in it are gone. The
command is captured when you register, and it must work without the
directory's files:

| works | does not work |
| --- | --- |
| `docker compose -p feature-x down` (finds containers by project label) | `docker compose -f ~/wt/feature-x/compose.yaml down` |
| `docker volume prune --all --force --filter label=com.docker.compose.project=feature-x` | `cd ~/wt/feature-x && make down` |
| `kill $(cat /run/user/1000/feature-x.pid)` (pid file outside the dir) | `./scripts/teardown.sh` (script inside the dir) |

`gone` enforces the part it can: steps never run inside the watched directory
(`--cwd` defaults to `$HOME` and may not be inside it), and they get a fixed,
explicit environment instead of whatever the daemon happens to have.

## Install

```sh
cargo install --path .           # puts gone in ~/.cargo/bin
gone service print               # prints a systemd user unit; installs nothing
```

To run the daemon under systemd (your call; `gone` never does this itself):

```sh
mkdir -p ~/.config/systemd/user
gone service print > ~/.config/systemd/user/gone.service
systemctl --user daemon-reload
systemctl --user enable --now gone.service
loginctl enable-linger "$USER"   # optional: keep it running while logged out
journalctl --user -u gone -f     # its log lines
```

The unit it prints looks like this (`ExecStart` and `GONE_STATE_DIR` are
filled in from the binary and state directory in use):

```ini
[Unit]
Description=gone: run teardown commands when watched directories are deleted

[Service]
Type=simple
ExecStart=/home/you/.cargo/bin/gone daemon
Environment=GONE_STATE_DIR=/home/you/.local/state/gone
Restart=on-failure
RestartSec=5s
# Steps outlive the daemon on purpose: on stop or crash they keep running,
# and the next daemon records their exit status instead of re-running them.
KillMode=process
TimeoutStopSec=60s

[Install]
WantedBy=default.target
```

Without systemd, any way of keeping `gone daemon` running works (a tmux pane,
a supervisor). Only one daemon runs per state directory; a second one exits
with an error. On filesystems where inotify misses changes (NFS and other
network mounts), run `gone daemon --no-inotify`: deletes are then noticed by
the sweep alone (`--sweep-interval`, default 3m).

## Usage

```text
gone watch <dir> [--name N] [--cwd DIR] [--env K[=V]]... STEP...
    STEP = [--after DUR] --shell CMD
         | [--after DUR] --exec '["prog","arg",...]'
         | [--after DUR] -- prog arg...        (last, takes the rest)
gone list [--json]
gone run <dir|name> [--dry-run]
gone forget <dir|name>
gone log [name] [-n N] [--json]
gone daemon [--settle 10s] [--sweep-interval 3m] [--max-attempts 6] ...
gone service print
```

### `gone watch`

Registers a directory (it must exist) with one or more steps. Running the
same `gone watch` again is a no-op, so callers can run it every time they
start something; running it with different steps, name, cwd or env replaces
the registration, and cancels any steps still pending from the old one.

- **Steps** run in the order given. `-- prog args...` and `--exec JSON` run a
  program directly, without a shell; `--shell CMD` runs `/bin/sh -c CMD`.
  Each step waits for the one before it to finish (succeed, or give up after
  its retries).
- **`--after DUR`** delays the step right after it, counted from when the
  directory was found gone: `30s`, `10m`, `24h`, `2d`, `1h30m`. Delayed steps
  are stored in the registry, so they survive a daemon restart.
- **Environment**: steps run with a cleared environment containing `PATH` and
  `HOME` as they were at `gone watch` time, each `--env KEY=VALUE` (or
  `--env KEY` to copy the current value), and `GONE_DIR` / `GONE_NAME`.
- **`--cwd`** is where steps run; default `$HOME`; never inside the watched
  directory.
- **`--name`** labels the registration in `list` and `log` and is passed as
  `GONE_NAME` (default: the directory's basename).

### Example: a docker compose stack

When the stack starts:

```sh
gone watch "$WORKTREE" --name feature-x \
  -- docker compose -p feature-x down --remove-orphans
```

Stop containers right away, delete the project's volumes a day later (in
case the delete was a mistake and you want the data back):

```sh
gone watch "$WORKTREE" --name feature-x \
  --shell 'docker compose -p "$GONE_NAME" down --remove-orphans' \
  --after 24h \
  -- docker volume prune --all --force --filter label=com.docker.compose.project=feature-x
```

Rootless Docker or a remote engine needs its environment passed through:
`--env DOCKER_HOST` or `--env XDG_RUNTIME_DIR`.

If the worktree is re-created at the same path and the stack started again,
the second `gone watch` replaces the registration, so the old pending
volume step cannot delete the new stack's volumes.

### `gone list`

```text
daemon: running (pid 41237)

feature-x  [triggered 2m ago]  /home/you/wt/feature-x
  1. now    sh -c 'docker compose -p "$GONE_NAME" down --remove-orphans'
     ok 2m ago (exit 0, took 3s)
     output: /home/you/.local/state/gone/runs/1f0c…-t1-step1-attempt1.log
  2. +1d    docker volume prune --all --force --filter label=com.docker.compose.project=feature-x
     due in 23h57m
```

States: `watching`, `triggered` (steps running or scheduled), `done` (every
step succeeded), `FAILED` (every step finished and at least one gave up). A
running step shows who runs it (the daemon or a `gone run`); a failed one its
attempts and last error. `--json` prints the registry.

### `gone run`

Runs a registration's steps now, in the foreground with output on your
terminal, ignoring `--after` delays and retries. `--dry-run` prints the
commands, cwd and full environment and runs nothing. Useful to check a
teardown before you rely on it.

A manual run is a trigger like a delete: on a watching or finished entry it
starts a fresh one, on a triggered entry it continues from the current step
(it refuses while the daemon is running that step). Ctrl-C stops the step,
records it, and puts the registration back to `watching`, so the remaining
steps are not left to the daemon.

### `gone log`

One line per action, with structured fields:

```text
2026-11-08T18:56:26Z dir_gone      name=feature-x dir=/home/you/wt/feature-x id=1f0c2a9e via=inotify settle=10s
2026-11-08T18:56:36Z triggered     name=feature-x dir=/home/you/wt/feature-x id=1f0c2a9e steps=2
2026-11-08T18:56:36Z step_start    name=feature-x ... step=1 attempt=1 program=sh
2026-11-08T18:56:39Z step_ok       name=feature-x ... step=1 attempt=1 exit=0 took=3s
```

Logs carry names, paths, step numbers, the program name, exit codes and
gone's own error messages, never a step's arguments, environment or output
(those may hold secrets). A step's output goes to its own file under
`runs/`, shown by `gone list`. `--json` prints the raw JSON lines.

## How it decides a directory is gone

- inotify watches every **ancestor** of each registered directory, so
  deleting or moving away the directory itself, or any parent of it, is seen
  immediately. Watches follow registry changes without a restart, and are
  re-added when an ancestor is re-created.
- A **sweep** re-checks every directory at startup and every few minutes
  (`--sweep-interval`, default 3m), so deletes while the daemon was down, or
  missed or overflowed events, are still caught.
- A check says **gone** only if looking the path up fails with `ENOENT` (or
  `ENOTDIR`, when an ancestor became a file). Any other failure (EACCES, EIO,
  …) is an **error**: logged as `check_error`, and it never triggers.

Per directory the daemon keeps one belief, in memory:

```text
            absent                        absent, settle delay over
 present ─────────▶ settling{since} ───────────────────────────▶ teardown (gone_at = since)
    ▲   ◀─present── │       │
    │  (cancelled)  │       └──error──▶ unknown{error}
    └────present────┴──────────────────── │ ──absent──▶ settling{since: now}
```

The rule: **a teardown starts only after the directory was seen absent on
every check for a full settle delay (`--settle`, default 10s), with no check
in between that saw it present or failed.** If it comes back (a worktree
re-created at the same path), the teardown is cancelled and the
registration stays armed. An error during settling interrupts it and the
next absent check starts the settle over, so an error can delay a teardown
by one settle delay but never cause one, and nothing waits forever. A daemon
restart forgets settling the same way (it starts over).

## Steps, claims and crashes

Once triggered, a registration's run state lives in the registry:

```text
 watching ──trigger──▶ triggered{done, current} ──last step finishes──▶ finished (done | FAILED)
    ▲                       current: waiting{due_at} ⇄ running{claim}
    └──────── gone watch (a new registration) ◀─────────────────────────────┘
```

Steps run in order; `current` is the one unfinished step, waiting for its
due time (its `--after`, or a retry backoff) or **claimed** by exactly one
owner: the daemon (pid + a per-start instance id) or a `gone run`. A
registration triggers at most once; only a new `gone watch` re-arms it.

Each attempt runs under a small supervisor (`gone __supervise`) that holds
the attempt's run lock for as long as the step runs and writes its exit
status when it ends. So whether an attempt is running is never guessed from
a pid:

- **The daemon stops or crashes mid-step:** the step keeps running (the
  unit uses `KillMode=process`). The next daemon finds the claim, sees the
  lock held, waits, and records the step's real exit status. Nothing runs
  twice.
- **The step's process dies too, without an exit status** (a reboot, an OOM
  kill of the group): the next daemon finds the lock free and no status, and
  records an **interrupted** attempt, retried like any failure. This is the
  only case where a step can run again after it may already have done its
  work, so teardown commands should still be safe to run twice
  (`docker compose down` is).

## Failures and retries

A step that exits non-zero, can't start, or exceeds `--step-timeout`
(default 15m) is retried with exponential backoff: `--retry-base` (30s),
doubling up to `--retry-max` (30m), for `--max-attempts` (6) attempts in
total. Then it is marked failed and the next step runs. Failures and retries
show in `gone list` and `gone log`.

## State

Everything lives in `$GONE_STATE_DIR`, else `$XDG_STATE_HOME/gone`, else
`~/.local/state/gone` (all private, mode 0700/0600):

- `registry.json`: the registrations, pretty-printed JSON with readable
  timestamps (`2026-11-08T18:56:36.000Z`) and durations (`1d`), and a
  `version` (now 2; version 1 files from gone 0.1 are read and converted on
  the next write). Every write
  takes an exclusive lock and replaces the file atomically (write a temp
  file, fsync, rename), so concurrent `gone watch` calls and the daemon never
  lose each other's changes and a reader never sees half a file.
- `events.jsonl`: the action log.
- `runs/`: per attempt, `<id>-t<trigger>-step<N>-attempt<M>.log` (output of
  daemon-run steps, pruned after 30 days) and, while it runs, `.lock` and
  `.status`.
- `daemon.lock`: held by the running daemon.

## Development

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

- `tests/daemon.rs`: the real binary and a real daemon against temp
  directories with short timings.
- `tests/machines.rs`: proptest-state-machine over the pure daemon core and
  registry model (a simulated world: checks, lost events, step processes,
  restarts, replace, forget, manual runs).
- `tests/paths.rs`: path shapes (trailing slash, `.`/`..`, symlinks, unicode,
  non-UTF-8, nested dirs).

`PROPTEST_CASES=10000 cargo test --release --test machines` for a deep run
(the default is 256 cases). See `AGENTS.md`.
