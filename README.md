# gone

## What this is

`gone` runs a command you registered when a directory is deleted.

```sh
gone watch ~/wt/feature-x -- docker compose -p feature-x down
rm -rf ~/wt/feature-x        # ~10 s later: docker compose -p feature-x down
```

A small daemon watches the registered directories. When one is really gone,
meaning still missing after a short settle delay, it runs that directory's
steps once.

## Why you might want it

Some things outlive the directory they were started from. You make a git
worktree, something starts a Docker stack or a dev server in it, you delete the
worktree, and the stack keeps running, holding ports and disk. Docker has no
"stop when this folder goes away" setting, and git has no hook for removing a
worktree.

`gone` fills that gap without knowing anything about Docker or git. Register
the teardown when you start something, and it runs when the folder disappears,
however that happens: `rm -rf`, `git worktree remove`, a tool's cleanup.

## Usage

```sh
cargo install --path .
gone service print > ~/.config/systemd/user/gone.service
systemctl --user enable --now gone.service
```

`gone service print` writes the systemd user unit that keeps the daemon
running; `gone` never installs it itself. Any other way of keeping
`gone daemon` running works too.

```sh
gone watch <dir> [--name N] [--after DUR] -- <command...>   # register (re-running it is a no-op)
gone list                                                   # registrations and their state
gone run <dir|name> [--dry-run]                             # run a teardown now
gone forget <dir|name>                                      # stop watching
gone log                                                    # what happened
```

The command is saved when you register, and by the time it runs the directory
is gone, so it must not need the directory's files: `docker compose -p
feature-x down` works, `cd ~/wt/feature-x && make down` doesn't. `--after 24h`
delays a step, for example to delete volumes a day later.

Agents: load the skill in [`skills/gone/SKILL.md`](skills/gone/SKILL.md). It
covers every flag, the environment steps get, retries, crash behavior and
`gone list` states. Working on `gone` itself: see [`AGENTS.md`](AGENTS.md).
