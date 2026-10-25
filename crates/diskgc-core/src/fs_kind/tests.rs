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
    for profile in config.library.profiles() {
        if profile.origin != Origin::User {
            continue;
        }
        let Body::Source(source) = &profile.body else {
            continue;
        };
        if Find::of(source, 1000).is_none() {
            continue;
        }
        checked += 1;
    }
    let failures = crate::profile::examples::failures(&config.library);
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
    let Some(Body::Source(_)) = library.get("x").map(|p| &p.body) else {
        panic!("x is a source");
    };
    let failures = crate::profile::examples::failures(&library);
    assert_eq!(failures.len(), 2, "{failures:?}");
    assert!(failures[0].contains("/tmp/b-1"), "{failures:?}");
    assert!(failures[1].contains("/tmp/a-2"), "{failures:?}");
}

fn mount(
    id: u64,
    parent_id: u64,
    at: &str,
    fs_type: &str,
    fs_root: &str,
) -> crate::host::MountEntry {
    crate::host::MountEntry {
        id,
        parent_id,
        dev: crate::host::Dev {
            major: 0,
            minor: 42,
        },
        fs_root: fs_root.into(),
        mount_point: at.into(),
        fs_type: fs_type.into(),
        source: "src".into(),
        read_only: false,
    }
}

/// A root is judged by its path and its mount's mountinfo line before
/// anything is opened (docs/safety.md, Before the walk): inside systemd's
/// PrivateTmp by a name on its path, or by its mount's root in its
/// filesystem (a bind mount of a `systemd-private-*` directory elsewhere,
/// task-68's QA finding); on a network filesystem; or with no mount.
#[test]
fn a_root_is_judged_by_its_path_and_its_mounts_line_first() {
    use super::judge::{RootRefused, resolve_root};
    use crate::engine::Reason;
    use crate::resolve::screen_mount_chain;
    use crate::resolve::tests::Scripted;
    let mounts = [
        mount(1, 0, "/", "ext4", "/"),
        mount(30, 1, "/tmp", "tmpfs", "/"),
        mount(
            31,
            1,
            "/srv/bound",
            "tmpfs",
            "/systemd-private-abc-x.service-y/tmp",
        ),
        mount(32, 1, "/mnt/nas", "nfs4", "/export"),
        mount(33, 1, "/home/me/remote", "fuse.sshfs", "/"),
        mount(34, 1, "/srv/plain", "tmpfs", "/srv-data"),
    ];
    // Any openat2 or statx fails the test: a refusal touches nothing.
    let judged = |root: &str, mounts: &[crate::host::MountEntry]| {
        let host = Scripted::never_opened(mounts.to_vec());
        match resolve_root(&host, Path::new(root), mounts) {
            Ok(Err(refused)) => refused,
            Ok(Ok(_)) => panic!("{root}: resolved"),
            Err(stop) => panic!("{root}: {stop}"),
        }
    };
    for local in ["/tmp", "/srv/plain/x"] {
        assert!(!path_in_private_tmp(Path::new(local)), "{local}");
        assert_eq!(
            screen_mount_chain(&mounts, Path::new(local)),
            Ok(()),
            "{local}"
        );
    }
    for private in [
        "/tmp/systemd-private-abc-x.service-y/tmp",
        "/srv/bound",
        "/srv/bound/sub",
    ] {
        assert!(
            matches!(judged(private, &mounts), RootRefused::PrivateTmp(_)),
            "{private}"
        );
    }
    for network in ["/mnt/nas", "/mnt/nas/scratch", "/home/me/remote/x"] {
        match judged(network, &mounts) {
            RootRefused::Skip(skip) => assert_eq!(skip.reason, Reason::UnsupportedFs, "{network}"),
            RootRefused::PrivateTmp(why) => panic!("{network}: {why}"),
        }
    }
    match judged("/tmp", &[]) {
        RootRefused::Skip(skip) => assert_eq!(skip.reason, Reason::UnsafePath),
        RootRefused::PrivateTmp(why) => panic!("{why}"),
    }
}

/// The one test for systemd's PrivateTmp names: the prefix, as bytes, at
/// the start of a name only.
#[test]
fn private_tmp_names_are_known_by_their_prefix() {
    assert!(is_private_tmp(OsStr::new(
        "systemd-private-abc-cups.service-x1"
    )));
    assert!(!is_private_tmp(OsStr::new("my-systemd-private-x")));
    assert!(!is_private_tmp(OsStr::new("systemd-privat")));
    assert!(path_in_private_tmp(Path::new(
        "/tmp/systemd-private-a/tmp/x"
    )));
    assert!(!path_in_private_tmp(Path::new("/tmp/a/tmp/x")));
}
