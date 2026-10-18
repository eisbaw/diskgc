//! The matcher, pure: segments and globs, whole patterns, examples; and
//! the spike's profile file (spike-profiles.yaml) as a configuration whose
//! every example holds.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::*;
use crate::config;
use crate::profile::{Body, Library, Origin, parse_profiles};

fn seg(text: &str) -> Segment {
    Pattern::compile(text, 1000)
        .segment(0)
        .expect("one segment")
        .clone()
}

fn m(segment: &str, name: &str) -> bool {
    seg(segment).matches(OsStr::new(name))
}

#[test]
fn a_literal_segment_matches_exactly_that_name() {
    assert!(m("node-compile-cache", "node-compile-cache"));
    for other in [
        "node-compile-cach",
        "node-compile-cache2",
        "Node-compile-cache",
        "",
    ] {
        assert!(!m("node-compile-cache", other), "{other:?}");
    }
    assert_eq!(
        seg("a.b").literal(),
        Some(OsStr::new("a.b")),
        "looked up, not listed"
    );
    assert_eq!(seg("a*").literal(), None);
}

#[test]
fn star_matches_any_run_within_one_name_also_an_empty_one_and_a_leading_dot() {
    for (pattern, name, want) in [
        ("*", "x", true),
        ("*", ".X11-unix", true),
        ("*", "", true),
        ("pytest-of-*", "pytest-of-mark", true),
        ("pytest-of-*", "pytest-of-", true),
        ("pytest-of-*", "pytest-of", false),
        ("pytest-of-*", "xpytest-of-mark", false),
        ("nix-shell-*-*", "nix-shell-1144586-3257819220", true),
        ("nix-shell-*-*", "nix-shell--", true),
        ("nix-shell-*-*", "nix-shell-1144586", false),
        ("nix-shell-*-*", "nix-shell.8raHjU", false),
        ("zephyr-*-run.*", "zephyr-h5e5-run.jTm9zGlH", true),
        ("zephyr-*-run.*", "zephyr-run.x", false),
        ("zephyr-*-run.*", "zephyr--run.", true),
        ("*.tar", "a.tar", true),
        ("*.tar", "a.tar.gz", false),
        ("a*a", "a", false),
        ("a*a", "aa", true),
        ("a*b*a", "aba", true),
        ("a*b*a", "aab", false),
        (".org.chromium.Chromium.*", "org.chromium.Chromium.x", false),
    ] {
        assert_eq!(m(pattern, name), want, "{pattern} vs {name:?}");
    }
}

#[test]
fn names_are_matched_as_bytes_not_as_text() {
    let latin1 = OsStr::from_bytes(b"pytest-of-m\xe6rk");
    assert!(seg("pytest-of-*").matches(latin1));
    assert!(!seg("pytest-of-mark").matches(latin1));
}

#[test]
fn a_pattern_matches_one_name_per_segment_never_more_or_fewer() {
    let p = Pattern::compile("nix-shell.*/pytest-of-*", 1000);
    assert_eq!(p.depth(), 2);
    fn names(path: &str) -> Vec<&OsStr> {
        path.split('/').map(OsStr::new).collect()
    }
    assert!(p.matches_names(&names("nix-shell.Ab1/pytest-of-mark")));
    assert!(!p.matches_names(&names("pytest-of-mark")));
    assert!(!p.matches_names(&names("nix-shell.Ab1/x/pytest-of-mark")));
    assert!(!p.matches_names(&names("nix-shell.Ab1/pytest-of-mark/pytest-3")));
    let uid = Pattern::compile("claude-{uid}/*/*", 4242);
    assert_eq!(uid.text(), "claude-4242/*/*");
    assert!(uid.matches_names(&names("claude-4242/p/s")));
    assert!(!uid.matches_names(&names("claude-1000/p/s")));
}

#[test]
fn examples_match_below_a_fixed_root_or_below_any_root() {
    let patterns = [Pattern::compile("pytest-of-*", 1000)];
    let tmp = [Path::new("/tmp"), Path::new("/var/tmp")];
    let fixed = Some(&tmp[..]);
    assert!(path_matches(
        fixed,
        &patterns,
        Path::new("/tmp/pytest-of-mark")
    ));
    assert!(path_matches(
        fixed,
        &patterns,
        Path::new("/var/tmp/pytest-of-ci")
    ));
    assert!(!path_matches(
        fixed,
        &patterns,
        Path::new("/home/pytest-of-mark")
    ));
    assert!(!path_matches(fixed, &patterns, Path::new("/tmp")));
    assert!(!path_matches(
        fixed,
        &patterns,
        Path::new("/tmpx/pytest-of-mark")
    ));
    assert!(!path_matches(
        fixed,
        &patterns,
        Path::new("tmp/pytest-of-mark")
    ));
    assert!(!path_matches(
        fixed,
        &patterns,
        Path::new("/tmp/../tmp/pytest-of-m")
    ));
    // roots: required: any directory above will do, but not none.
    assert!(path_matches(None, &patterns, Path::new("/a/b/pytest-of-x")));
    assert!(!path_matches(None, &patterns, Path::new("/pytest-of-x")));
    assert!(!path_matches(
        None,
        &patterns,
        Path::new("/a/pytest-of-x/y")
    ));
}

/// A `?`, `[`, `]` or `\` in a pattern is refused at load: reserved, so a
/// later glob syntax cannot change what an existing pattern matches.
#[test]
fn reserved_glob_bytes_are_refused_at_load() {
    for bad in ["tmp.??????", "tmp.[a-z]*", "a\\*b", "x]"] {
        let yaml = format!(
            "- {{name: x, find: {{roots: [/tmp], paths: [\"{}\"], type: dir}}, action: delete, \
             examples: {{match: [/tmp/a]}}}}",
            bad.replace('\\', "\\\\")
        );
        let specs = parse_profiles(&yaml).unwrap_or_else(|e| panic!("{bad}: {e}"));
        let error = Library::with_user_profiles(&specs)
            .expect_err(bad)
            .to_string();
        assert!(error.contains("reserved"), "{bad}: {error}");
    }
}

/// The spike's profile file loads as a configuration (static rules, the
/// command actions included), and every example of every `fs` source
/// holds for uid 1000.
#[test]
fn the_spike_profiles_load_and_their_examples_hold() {
    let text = include_str!("spike-profiles.yaml");
    let config = config::parse(Path::new("spike-profiles.yaml"), text.as_bytes())
        .unwrap_or_else(|e| panic!("{e}"));
    let mut checked = 0;
    let mut failures = Vec::new();
    for profile in config.library.profiles() {
        let Body::Source(source) = &profile.body else {
            continue;
        };
        if Find::of(source, 1000).is_none() {
            continue;
        }
        checked += 1;
        failures.extend(
            example_failures(source, 1000)
                .into_iter()
                .map(|f| format!("{}: {f}", profile.name)),
        );
    }
    assert!(failures.is_empty(), "{failures:#?}");
    // Every source the file defines is of kind fs and was checked.
    let user_sources = config
        .library
        .profiles()
        .filter(|p| p.origin == Origin::User && matches!(p.body, Body::Source(_)))
        .count();
    assert!(checked > 0);
    assert_eq!(
        checked, user_sources,
        "a source of the file was not checked"
    );
}

/// The examples check bites: a `match` the patterns miss and a `no_match`
/// they find are both reported.
#[test]
fn a_wrong_example_is_reported() {
    let yaml = "- {name: x, find: {roots: [/tmp], paths: [\"a-*\"], type: dir}, action: delete, \
                examples: {match: [/tmp/b-1, /tmp/a-1], no_match: [/tmp/a-2]}}";
    let library = Library::with_user_profiles(&parse_profiles(yaml).expect("parses"))
        .unwrap_or_else(|e| panic!("{e}"));
    let Some(Body::Source(source)) = library.get("x").map(|p| &p.body) else {
        panic!("x is a source");
    };
    let failures = example_failures(source, 1000);
    assert_eq!(failures.len(), 2, "{failures:?}");
    assert!(failures[0].contains("/tmp/b-1"), "{failures:?}");
    assert!(failures[1].contains("/tmp/a-2"), "{failures:?}");
}
