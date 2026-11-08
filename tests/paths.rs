//! Path-shape properties.
//!
//! Pure: which registrations an event touches, for nested watched dirs and
//! names that are prefixes of each other, in any script.
//! Real filesystem (fewer cases): every way of spelling a directory —
//! trailing slash, `.`/`..`, a symlink, unicode, bytes that are not UTF-8 —
//! registers the same canonical directory, re-registering through another
//! spelling is a no-op, and `forget` finds it by any spelling.

mod common;

use common::Env;
use gone::core::{ancestors, touched_by};
use proptest::prelude::*;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

/// Names chosen to collide: prefixes of each other, unicode, dots, spaces.
fn name() -> impl Strategy<Value = String> {
    prop::sample::select(vec![
        "a", "ab", "a b", "é", "e\u{301}", "日本", "日", "..a", ".x", "wt-1", "wt-10", "🙂",
    ])
    .prop_map(String::from)
}

fn rel_path() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(name(), 1..5)
}

fn join(base: &Path, parts: &[String]) -> PathBuf {
    parts.iter().fold(base.to_path_buf(), |p, c| p.join(c))
}

fn cases(n: u32) -> ProptestConfig {
    ProptestConfig {
        cases: std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|c| c.parse().ok())
            .map_or(n, |c: u32| c.min(n * 40)),
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(cases(512))]

    /// An event on path P concerns exactly the watched dirs at or below P,
    /// component by component (so /w/a never touches /w/ab).
    #[test]
    fn events_touch_exactly_the_dirs_below_them(
        dirs in prop::collection::vec(rel_path(), 1..6),
        event in rel_path(),
        cut in 0usize..5,
    ) {
        let root = Path::new("/w");
        let dirs: Vec<PathBuf> = dirs.iter().map(|d| join(root, d)).collect();
        let event = join(root, &event[..event.len().min(cut + 1)]);
        for d in &dirs {
            let expected = d.components().count() >= event.components().count()
                && d.components().zip(event.components()).all(|(a, b)| a == b);
            prop_assert_eq!(touched_by(d, std::slice::from_ref(&event)), expected, "dir {:?} event {:?}", d, event);
            // Watching the ancestors is what makes such an event visible.
            if expected && d != &event {
                prop_assert!(ancestors(d).any(|a| a == event.as_path()));
            }
        }
    }
}

proptest! {
    #![proptest_config(cases(24))]

    /// Every spelling of a directory registers the same canonical path.
    #[test]
    fn every_spelling_registers_the_same_dir(
        parts in rel_path(),
        nested in rel_path(),
        form in 0usize..4,
    ) {
        let env = Env::new();
        let base = env.root().join("w");
        let dir = join(&base, &parts);
        std::fs::create_dir_all(&dir).unwrap();
        let canonical = std::fs::canonicalize(&dir).unwrap();
        // A second watched dir nested inside the first.
        let inner = join(&dir, &nested);
        std::fs::create_dir_all(&inner).unwrap();
        let link = env.root().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();

        let spellings: Vec<OsString> = vec![
            dir.clone().into_os_string(),
            {
                let mut s = dir.clone().into_os_string();
                s.push("/");
                s
            },
            dir.join(".").join("..").join(dir.file_name().unwrap()).into_os_string(),
            link.clone().into_os_string(),
        ];
        let first = &spellings[form];
        let out = env.cmd().arg("watch").arg(first).args(["--", "true"]).output().unwrap();
        prop_assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let out = env.cmd().arg("watch").arg(&inner).args(["--", "true"]).output().unwrap();
        prop_assert!(out.status.success());

        let reg = env.registry();
        let entries = reg["entries"].as_array().unwrap();
        prop_assert_eq!(entries.len(), 2);
        prop_assert_eq!(entries[0]["dir"].as_str().unwrap(), canonical.to_str().unwrap());
        prop_assert_eq!(entries[0]["name"].as_str().unwrap(), parts.last().unwrap().as_str());

        // Any other spelling of the same dir is the same registration.
        for other in &spellings {
            let out = env.cmd().arg("watch").arg(other).args(["--", "true"]).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            prop_assert!(text.contains("unchanged"), "{:?}: {}", other, text);
        }
        prop_assert_eq!(env.registry()["entries"].as_array().unwrap().len(), 2);
        let out = env.cmd().arg("forget").arg(&spellings[(form + 1) % 4]).output().unwrap();
        prop_assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let left = env.registry();
        let left = left["entries"].as_array().unwrap();
        prop_assert_eq!(left.len(), 1, "forget removed the wrong one");
        let inner_canonical = std::fs::canonicalize(&inner).unwrap();
        prop_assert_eq!(left[0]["dir"].as_str().unwrap(), inner_canonical.to_str().unwrap());
    }

    /// A directory whose name is not UTF-8 is refused with a clear message,
    /// and the registry is left as it was.
    #[test]
    fn non_utf8_names_are_refused_cleanly(bytes in prop::collection::vec(0x80u8..=0xff, 1..6)) {
        let env = Env::new();
        let ok = env.dir("fine");
        env.watch(&ok, &["--", "true"]);
        let before = std::fs::read(env.state().join("registry.json")).unwrap();
        let mut name = b"bad-".to_vec();
        name.extend(&bytes);
        let bad = env.root().join("w").join(OsString::from_vec(name));
        prop_assume!(std::str::from_utf8(bad.as_os_str().as_encoded_bytes()).is_err());
        std::fs::create_dir_all(&bad).unwrap();
        let out = env.cmd().arg("watch").arg(&bad).args(["--", "true"]).output().unwrap();
        prop_assert_eq!(out.status.code(), Some(2));
        let err = String::from_utf8_lossy(&out.stderr);
        prop_assert!(err.contains("not valid UTF-8"), "{}", err);
        prop_assert_eq!(std::fs::read(env.state().join("registry.json")).unwrap(), before);
    }
}
