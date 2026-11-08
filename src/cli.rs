//! Command-line interface.

use crate::model::Cmd;
use crate::time::Dur;
use clap::error::ErrorKind;
use clap::{ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "gone",
    version,
    about = "Run a registered teardown command when a watched directory is deleted",
    long_about = "Run a registered teardown command when a watched directory is deleted.\n\n\
        Register a directory with `gone watch`, keep `gone daemon` running (see \
        `gone service print`), and when the directory is deleted or moved away the \
        daemon runs its steps.\n\n\
        The directory's files are gone by then, so a step must not need them: \
        `docker compose -p NAME down` works (it finds containers by label), \
        `docker compose -f DIR/compose.yaml down` does not."
)]
pub struct Cli {
    /// State directory (registry and logs). Default: $XDG_STATE_HOME/gone,
    /// else ~/.local/state/gone.
    #[arg(long, global = true, env = "GONE_STATE_DIR", value_name = "DIR")]
    pub state_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Register (or replace) a directory and its teardown steps.
    #[command(after_help = WATCH_HELP)]
    Watch(WatchArgs),
    /// Show registrations: directory, name, steps, state, last result.
    List {
        /// Print the registry as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run a registration's steps now, in the foreground, ignoring delays.
    Run {
        /// Registered directory or name.
        target: String,
        /// Print what would run (command, cwd, env) and run nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove a registration without running anything.
    Forget {
        /// Registered directory or name.
        target: String,
    },
    /// Show the action log.
    Log {
        /// Only events for this name.
        name: Option<String>,
        /// How many recent lines to show (0 = all).
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Print raw JSON lines.
        #[arg(long)]
        json: bool,
    },
    /// Run the watcher in the foreground (what the systemd unit runs).
    Daemon(DaemonArgs),
    /// Systemd integration.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Internal: runs one step attempt, holding its run lock, and records
    /// its exit status (see `exec::supervise`).
    #[command(name = "__supervise", hide = true)]
    Supervise {
        #[arg(long)]
        status: PathBuf,
        #[arg(long)]
        lock_fd: i32,
        #[arg(last = true, required = true)]
        argv: Vec<OsString>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Print a systemd user unit for `gone daemon` (installs nothing).
    Print,
}

const WATCH_HELP: &str = "\
Steps:
  Each step is `--shell CMD`, `--exec JSON_ARGV`, or `-- ARGV...` (argv form
  last, since it takes the rest of the line). Argv steps run without a shell.
  `--after DUR` delays the step that follows it, counted from the delete.
  Steps run in order; each waits for the one before it to finish.

Environment:
  Steps run with a cleared environment: PATH and HOME as they are now, any
  --env, plus GONE_DIR and GONE_NAME. They run in --cwd (default $HOME),
  never in the watched directory.

Examples:
  gone watch ~/wt/feature-x -- docker compose -p feature-x down
  gone watch ~/wt/feature-x --name feature-x \\
      --shell 'docker compose -p \"$GONE_NAME\" down' \\
      --after 24h -- docker volume prune --all --force \\
          --filter label=com.docker.compose.project=feature-x

Re-running the same `gone watch` is a no-op; changing it replaces the
registration (and cancels any steps still pending from the old one).";

#[derive(Debug, Args)]
pub struct WatchArgs {
    /// The directory to watch. Must exist.
    pub dir: PathBuf,
    /// Name for list/log and $GONE_NAME (default: the directory's basename).
    #[arg(long)]
    pub name: Option<String>,
    /// Working directory for the steps (default: $HOME). Not inside DIR.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
    /// Extra environment for the steps: KEY=VALUE, or KEY to copy it from
    /// the current environment.
    #[arg(long = "env", value_name = "KEY[=VALUE]")]
    pub env: Vec<String>,
    /// Delay the next step until DUR after the delete (e.g. 30s, 10m, 24h).
    #[arg(long, value_name = "DUR", value_parser = Dur::parse)]
    pub after: Vec<Dur>,
    /// A step run with `/bin/sh -c CMD`.
    #[arg(long, value_name = "CMD")]
    pub shell: Vec<String>,
    /// A step given as a JSON array of strings, run without a shell.
    #[arg(long, value_name = "JSON")]
    pub exec: Vec<String>,
    /// The last step: a program and its arguments, run without a shell.
    #[arg(last = true, value_name = "ARGV")]
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct DaemonArgs {
    /// How long a directory must stay gone before its steps run.
    #[arg(long, default_value = "10s", value_parser = Dur::parse)]
    pub settle: Dur,
    /// How often to re-check every watched directory, events or not.
    #[arg(long, default_value = "3m", value_parser = Dur::parse)]
    pub sweep_interval: Dur,
    /// Delay before the first retry of a failed step (doubles each time).
    #[arg(long, default_value = "30s", value_parser = Dur::parse)]
    pub retry_base: Dur,
    /// Longest delay between retries.
    #[arg(long, default_value = "30m", value_parser = Dur::parse)]
    pub retry_max: Dur,
    /// Attempts per step, including the first, before giving up.
    #[arg(long, default_value_t = 6, value_parser = clap::value_parser!(u32).range(1..))]
    pub max_attempts: u32,
    /// Kill a step that runs longer than this (0 = no limit).
    #[arg(long, default_value = "15m", value_parser = Dur::parse)]
    pub step_timeout: Dur,
    /// On SIGTERM, how long to wait for running steps.
    #[arg(long, default_value = "30s", value_parser = Dur::parse)]
    pub shutdown_grace: Dur,
    /// Don't use inotify; notice deletes only by sweeping (for filesystems
    /// where inotify misses changes, e.g. NFS).
    #[arg(long)]
    pub no_inotify: bool,
    /// Longest the loop sleeps between passes.
    #[arg(long, default_value = "1s", value_parser = Dur::parse, hide = true)]
    pub tick: Dur,
}

/// A step as given on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepSpec {
    pub cmd: Cmd,
    pub after: Dur,
}

pub struct Parsed {
    pub cli: Cli,
    /// For `watch`: the steps, in command-line order, with their delays.
    pub steps: Vec<StepSpec>,
}

pub fn parse_from<I, T>(args: I) -> Result<Parsed, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = Cli::command().try_get_matches_from(args)?;
    let cli = Cli::from_arg_matches(&matches)?;
    let steps = match matches.subcommand() {
        Some(("watch", sub)) => watch_steps(sub).map_err(|msg| {
            Cli::command()
                .find_subcommand_mut("watch")
                .expect("watch subcommand")
                .error(ErrorKind::ValueValidation, msg)
        })?,
        _ => Vec::new(),
    };
    Ok(Parsed { cli, steps })
}

enum Item {
    After(Dur),
    Step(Cmd),
}

/// Rebuilds the order of `--after`/`--shell`/`--exec`/`--` from their
/// positions, so each `--after` binds to the step that follows it.
fn watch_steps(m: &ArgMatches) -> Result<Vec<StepSpec>, String> {
    let mut items: Vec<(usize, Item)> = Vec::new();
    if let (Some(vals), Some(idx)) = (m.get_many::<Dur>("after"), m.indices_of("after")) {
        items.extend(idx.zip(vals).map(|(i, d)| (i, Item::After(*d))));
    }
    if let (Some(vals), Some(idx)) = (m.get_many::<String>("shell"), m.indices_of("shell")) {
        for (i, s) in idx.zip(vals) {
            if s.trim().is_empty() {
                return Err("--shell needs a non-empty command".into());
            }
            items.push((i, Item::Step(Cmd::Shell(s.clone()))));
        }
    }
    if let (Some(vals), Some(idx)) = (m.get_many::<String>("exec"), m.indices_of("exec")) {
        for (i, s) in idx.zip(vals) {
            let argv: Vec<String> = serde_json::from_str(s)
                .map_err(|e| format!("--exec wants a JSON array of strings ({e}): {s}"))?;
            check_argv(&argv, "--exec")?;
            items.push((i, Item::Step(Cmd::Argv(argv))));
        }
    }
    if let (Some(vals), Some(mut idx)) = (m.get_many::<String>("argv"), m.indices_of("argv")) {
        let argv: Vec<String> = vals.cloned().collect();
        check_argv(&argv, "--")?;
        items.push((
            idx.next().unwrap_or(usize::MAX),
            Item::Step(Cmd::Argv(argv)),
        ));
    }
    items.sort_by_key(|(i, _)| *i);

    let mut steps = Vec::new();
    let mut pending: Option<Dur> = None;
    for (_, item) in items {
        match item {
            Item::After(d) => {
                if pending.replace(d).is_some() {
                    return Err(
                        "two --after in a row: each --after delays the one step after it".into(),
                    );
                }
            }
            Item::Step(cmd) => steps.push(StepSpec {
                cmd,
                after: pending.take().unwrap_or(Dur::ZERO),
            }),
        }
    }
    if pending.is_some() {
        return Err("--after must be followed by a step (--shell, --exec or -- ARGV)".into());
    }
    if steps.is_empty() {
        return Err("no steps: give --shell CMD, --exec JSON or -- ARGV...".into());
    }
    Ok(steps)
}

fn check_argv(argv: &[String], flag: &str) -> Result<(), String> {
    match argv.first() {
        None => Err(format!("{flag}: empty command")),
        Some(p) if p.is_empty() => Err(format!("{flag}: empty program name")),
        Some(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps(args: &[&str]) -> Result<Vec<StepSpec>, String> {
        let mut all = vec!["gone", "watch", "/tmp/x"];
        all.extend(args);
        parse_from(all).map(|p| p.steps).map_err(|e| e.to_string())
    }

    fn argv(a: &[&str]) -> Cmd {
        Cmd::Argv(a.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn single_argv_step() {
        let got = steps(&[
            "--", "docker", "compose", "-p", "x", "down", "--after", "5s",
        ])
        .unwrap();
        assert_eq!(
            got,
            vec![StepSpec {
                cmd: argv(&["docker", "compose", "-p", "x", "down", "--after", "5s"]),
                after: Dur::ZERO
            }],
            "everything after -- belongs to the command"
        );
    }

    #[test]
    fn after_binds_to_the_next_step() {
        let got = steps(&[
            "--shell",
            "first",
            "--after",
            "24h",
            "--exec",
            r#"["docker","volume","rm","v"]"#,
            "--after",
            "1h30m",
            "--",
            "last",
            "arg",
        ])
        .unwrap();
        assert_eq!(
            got,
            vec![
                StepSpec {
                    cmd: Cmd::Shell("first".into()),
                    after: Dur::ZERO
                },
                StepSpec {
                    cmd: argv(&["docker", "volume", "rm", "v"]),
                    after: Dur(86_400_000)
                },
                StepSpec {
                    cmd: argv(&["last", "arg"]),
                    after: Dur(5_400_000)
                },
            ]
        );
    }

    #[test]
    fn options_can_go_anywhere_before_the_argv() {
        let p = parse_from([
            "gone", "watch", "--name", "n", "/tmp/x", "--env", "A=1", "--after", "2s", "--", "true",
        ])
        .unwrap();
        let Command::Watch(w) = p.cli.command else {
            panic!()
        };
        assert_eq!(w.name.as_deref(), Some("n"));
        assert_eq!(w.env, ["A=1"]);
        assert_eq!(p.steps[0].after, Dur(2000));
    }

    #[test]
    fn rejects_bad_step_lists() {
        assert!(steps(&[]).unwrap_err().contains("no steps"));
        assert!(
            steps(&["--shell", "x", "--after", "1h"])
                .unwrap_err()
                .contains("must be followed")
        );
        assert!(
            steps(&["--after", "1h", "--after", "2h", "--shell", "x"])
                .unwrap_err()
                .contains("two --after")
        );
        assert!(
            steps(&["--after", "soon", "--shell", "x"])
                .unwrap_err()
                .contains("invalid duration")
        );
        assert!(
            steps(&["--exec", "docker down"])
                .unwrap_err()
                .contains("JSON array")
        );
        assert!(
            steps(&["--exec", "[]"])
                .unwrap_err()
                .contains("empty command")
        );
        assert!(steps(&["--shell", "  "]).unwrap_err().contains("non-empty"));
    }

    #[test]
    fn other_subcommands_parse() {
        let p = parse_from(["gone", "run", "wt", "--dry-run"]).unwrap();
        assert!(matches!(p.cli.command, Command::Run { dry_run: true, .. }));
        let p = parse_from(["gone", "--state-dir", "/s", "log", "-n", "5", "wt"]).unwrap();
        assert_eq!(p.cli.state_dir, Some(PathBuf::from("/s")));
        assert!(matches!(p.cli.command, Command::Log { lines: 5, .. }));
        let p = parse_from(["gone", "daemon", "--settle", "500ms"]).unwrap();
        let Command::Daemon(d) = p.cli.command else {
            panic!()
        };
        assert_eq!((d.settle, d.max_attempts), (Dur(500), 6));
        assert!(parse_from(["gone", "daemon", "--max-attempts", "0"]).is_err());
        assert!(parse_from(["gone", "service", "print"]).is_ok());
    }

    #[test]
    fn clap_definition_is_consistent() {
        Cli::command().debug_assert();
    }
}
