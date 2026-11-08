//! The action log: one JSON object per line in `events.jsonl`, appended with
//! a single `write` so concurrent writers never interleave lines. The daemon
//! also echoes each line to stderr in `key=value` form (journald picks it up).
//!
//! Fields are names, paths, step numbers, program names, exit codes and our
//! own error messages: never a step's arguments, its environment or its
//! output, any of which may hold secrets.

use crate::time::Ts;
use serde_json::{Map, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

pub struct Logger {
    path: PathBuf,
    echo: bool,
}

/// Keeps call sites short: `log.event("x", &[("name", v(&name)), ...])`.
pub fn v(x: impl Into<Value>) -> Value {
    x.into()
}

impl Logger {
    pub fn new(path: PathBuf, echo: bool) -> Logger {
        Logger { path, echo }
    }

    pub fn event(&self, event: &str, fields: &[(&str, Value)]) {
        let mut obj = Map::new();
        obj.insert("ts".into(), Value::String(Ts::now().rfc3339()));
        obj.insert("event".into(), Value::String(event.into()));
        for (k, val) in fields {
            if !val.is_null() {
                obj.insert((*k).into(), val.clone());
            }
        }
        let obj = Value::Object(obj);
        let mut line = obj.to_string();
        line.push('\n');
        let written = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = written {
            eprintln!("gone: cannot write {}: {e}", self.path.display());
        }
        if self.echo {
            eprintln!("{}", format_line(&obj));
        }
    }
}

/// `2026-11-08T18:40:05Z triggered  name=wt dir=/w/wt` — the `gone log` form.
pub fn format_line(v: &Value) -> String {
    let Some(obj) = v.as_object() else {
        return v.to_string();
    };
    let ts = obj
        .get("ts")
        .and_then(Value::as_str)
        .and_then(|s| Ts::parse(s).ok())
        .map(Ts::short)
        .unwrap_or_else(|| "?".into());
    let event = obj.get("event").and_then(Value::as_str).unwrap_or("?");
    let mut out = format!("{ts} {event:<13}");
    for (k, val) in obj {
        if k == "ts" || k == "event" {
            continue;
        }
        let s = match val {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let needs_quotes = s.is_empty() || s.contains(|c: char| c.is_whitespace() || c == '"');
        if needs_quotes {
            out.push_str(&format!(" {k}={}", Value::String(s)));
        } else {
            out.push_str(&format!(" {k}={s}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_json_and_format_readably() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.jsonl");
        let log = Logger::new(path.clone(), false);
        log.event(
            "triggered",
            &[
                ("name", v("wt")),
                ("dir", v("/w/my wt")),
                ("step", v(2)),
                ("skip", Value::Null),
            ],
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["event"], "triggered");
        assert!(parsed.get("skip").is_none());
        let line = format_line(&parsed);
        assert!(
            line.ends_with(r#"triggered     name=wt dir="/w/my wt" step=2"#),
            "{line}"
        );
    }
}
