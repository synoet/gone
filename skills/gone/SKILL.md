---
name: gone
description: Register a teardown with `gone` so something you start (a Docker stack, a dev server, a tunnel) is cleaned up when its directory is deleted. Use when starting long-lived processes or containers tied to a worktree or checkout, or when asked to check, run or remove a registered teardown.
---

# gone

`gone watch <dir> -- <command>` registers a teardown. The `gone daemon` runs it
once when `<dir>` is really gone. Register every time you start the thing:
re-running the same `gone watch` is a no-op.

## The one rule: the teardown must not need the directory

By the time a step runs, the directory and its files are gone. The command is
saved at registration.

| Works | Doesn't work |
| --- | --- |
| `docker compose -p feature-x down` (finds containers by project label) | `docker compose -f ~/wt/feature-x/compose.yaml down` |
| `docker volume prune --all --force --filter label=com.docker.compose.project=feature-x` | `cd ~/wt/feature-x && make down` |
| `kill $(cat /run/user/1000/feature-x.pid)` (pid file outside the dir) | `./scripts/teardown.sh` (a script inside the dir) |

Steps never run inside the watched directory. `--cwd` defaults to `$HOME` and
can't point inside it. Teardowns should be safe to run twice (see Crashes).

## Registering

```text
gone watch <dir> [--name N] [--cwd DIR] [--env K[=V]]... STEP...
    STEP = [--after DUR] --shell CMD
         | [--after DUR] --exec '["prog","arg",...]'
         | [--after DUR] -- prog arg...        (last; takes the rest)
```

- The directory must exist when you register.
- Steps run in order, each after the previous one finishes (succeeds, or gives
  up after its retries).
- `-- prog args...` and `--exec JSON` run a program directly. `--shell CMD`
  runs `/bin/sh -c CMD`.
- `--after DUR` delays the step after it, counted from when the directory was
  found gone: `30s`, `10m`, `24h`, `2d`, `1h30m`. Delays survive a daemon
  restart.
- **Environment:** steps get a cleared environment holding `PATH` and `HOME`
  as they were at `gone watch` time, each `--env KEY=VALUE` (`--env KEY`
  copies the current value), and `GONE_DIR` / `GONE_NAME`. Rootless Docker or
  a remote engine needs `--env DOCKER_HOST` or `--env XDG_RUNTIME_DIR`.
- `--name` labels the registration (default: the directory's basename).
- **Re-registering:** a different set of steps, name, cwd or env replaces the
  registration and cancels its pending steps. So a worktree re-created at the
  same path can't have its new stack's volumes deleted by an old delayed step.

Example: stop a stack right away, and delete its volumes a day later:

```sh
gone watch "$WORKTREE" --name feature-x \
  --shell 'docker compose -p "$GONE_NAME" down --remove-orphans' \
  --after 24h \
  -- docker volume prune --all --force --filter label=com.docker.compose.project=feature-x
```

## Checking and running

- `gone list [--json]` shows each registration and its state:
  - `watching`;
  - `triggered` (steps running or scheduled);
  - `done` (every step succeeded);
  - `FAILED` (every step finished, and at least one gave up).

  Each step shows its last result and the path of its output log.
- `gone run <dir|name> --dry-run` prints the commands, cwd and full environment
  without running anything. Use it to check a teardown before relying on it.
  Without `--dry-run`, it runs the steps now in the foreground, ignoring delays
  and retries, and refuses a step the daemon is running. Ctrl-C stops the
  step and re-arms the registration.
- `gone forget <dir|name>` stops watching without running anything.
- `gone log [name] [-n N] [--json]` prints one line per action. Logs never
  include a step's arguments, environment or output; output goes to a file
  under the state directory's `runs/`.

## When it decides a directory is gone

- inotify watches every ancestor, so deleting or moving the directory or any
  parent of it is seen at once.
- A sweep at startup and every 3 minutes (`--sweep-interval`) catches deletes
  made while the daemon was down, or events that were missed.
- "Gone" means the lookup failed with `ENOENT` or `ENOTDIR`. Any other error
  (permission, I/O) never triggers a teardown.
- A teardown starts only after the directory was missing on every check for a
  full settle delay (`--settle`, default 10s). If it comes back in that time,
  the teardown is cancelled and the registration stays armed.
- On NFS or other network filesystems, run `gone daemon --no-inotify`, which
  uses the sweep alone.

## Failures and crashes

- **A failing step** (non-zero exit, failure to start, or longer than
  `--step-timeout`, 15m) is retried with backoff: 30s doubling up to 30m, 6
  attempts. Then it's marked failed and the next step runs.
- **The daemon stops or crashes mid-step:** the step keeps running (the unit
  uses `KillMode=process`), and the next daemon records its real exit status.
  Nothing runs twice.
- **The step's own process dies with no exit status** (a reboot, an
  out-of-memory kill): the step is retried. Only then can a step run again
  after it may already have done its work, which is why teardowns should be
  idempotent. `docker compose down` is.

## Daemon

- Run it under systemd:
  - `gone service print > ~/.config/systemd/user/gone.service`;
  - `systemctl --user enable --now gone.service`;
  - optionally `loginctl enable-linger "$USER"`, to keep it running while
    logged out.

  Its log is `journalctl --user -u gone -f`.
- Only one daemon runs per state directory.
- State lives in `$GONE_STATE_DIR`, else `$XDG_STATE_HOME/gone`, else
  `~/.local/state/gone`. `registry.json` is written atomically under a lock, so
  concurrent `gone watch` calls are safe.
- Never install or enable the unit for someone without asking: it's a
  long-running process on their machine.
