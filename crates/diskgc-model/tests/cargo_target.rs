//! The `cargo-target` kind up to its reclaim protocol (diskgc-core
//! `cargo_target`; docs/safety.md, `cargo-target`) on ModelHost: discovery,
//! the lock allowlist and lock sets (split build directories), layouts and
//! the last-build measure, eligibility, and the Cargo process heuristics.
//! Each rule has a test that fails when the rule is removed; ModelHost also
//! fails a test on any call the safety rules forbid.

mod common;

use std::path::{Path, PathBuf};

use common::host;
use diskgc_core::cargo_target::{
    DISCOVERY_DEPTH, Discovery, Eligible, Layout, LockSet, Target, discover, eligibility,
};
use diskgc_core::engine::{Reason, Rules, Skip};
use diskgc_core::inuse::{Checker, REUSE_LIMIT};
use diskgc_core::profile::IdleFor;
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;

const ME: u32 = 1000;
const DAY: std::time::Duration = std::time::Duration::from_secs(86_400);

/// A classic profile directory (Cargo 1.99.1 after a build): all three
/// lock files, `.fingerprint/`, `build/`, `deps/`, `incremental/`, the
/// binary.
const CLASSIC: &str = r#"
        .cargo-lock: {}
        .cargo-build-lock: {}
        .cargo-artifact-lock: {}
        .fingerprint: {p-1: {bin-p: {}}}
        build: {.: {}}
        deps: {p-1: {size: 1M}, p-1.d: {}}
        examples: {.: {}}
        incremental: {p-2: {s-1: {f: {}}}}
        p: {size: 1M}
        p.d: {}"#;

/// The new build-dir layout (the 2026-10-16 nightly's default): no
/// `.fingerprint/`, no `deps/`; `build/<pkg>/<hash>/{fingerprint,out}`.
const PER_UNIT: &str = r#"
        .cargo-lock: {}
        .cargo-build-lock: {}
        .cargo-artifact-lock: {}
        build: {p: {"1727f8a9": {fingerprint: {bin-p: {}}, out: {p-1727f8a9: {size: 1M}}}}}
        examples: {.: {}}
        incremental: {p-2: {s-1: {f: {}}}}
        p: {size: 1M}
        p.d: {}"#;

/// A model whose clock is two days past every time in the tree (an event
/// at the first call advances it), so everything is idle for a day unless
/// a test gives it a later time. `/tmp/w/p/target` holds a classic debug
/// profile; `tree` adds tree keys, `rest` top-level keys (processes,
/// further events).
fn model(tree: &str, rest: &str) -> ModelHost {
    host(&yaml(tree, rest))
}

/// The YAML of [`model`].
fn yaml(tree: &str, rest: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}}
tree:
  /bin/sh: {{}}
  /opt/runner: {{}}
  /tmp/w/p:
    Cargo.toml: {{}}
    src: {{main.rs: {{}}}}
    target:
      CACHEDIR.TAG: {{cachedir_tag: true}}
      debug:{CLASSIC}
{tree}
events:
  - {{at: {{call: 1, when: before}}, advance: 2d}}
{rest}"#
    )
}

/// The `/tmp` mount line of [`yaml`].
const TMP_MOUNT: &str = "  - {id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 1}";

/// `yaml` with `line` instead of the `/tmp` mount line.
fn remount(yaml: &str, line: &str) -> String {
    assert!(yaml.contains(TMP_MOUNT));
    yaml.replace(TMP_MOUNT, line)
}

/// `yaml` with the mount `line` added after `/tmp`'s.
fn with_mount(yaml: &str, line: &str) -> String {
    remount(yaml, &format!("{TMP_MOUNT}\n{line}"))
}

fn roots(paths: &[&str]) -> Vec<PathBuf> {
    paths.iter().map(PathBuf::from).collect()
}

fn found(host: &ModelHost, at: &[&str]) -> Discovery {
    discover(host, &roots(at), None).unwrap_or_else(|e| panic!("discover: {e}"))
}

fn paths(d: &Discovery) -> Vec<String> {
    d.targets
        .iter()
        .map(|t| t.path.display().to_string())
        .collect()
}

/// The rules of a `profile:cargo-target` tier: its `checkouts` setting
/// comes from the built-in profile, as in the executor.
fn rules() -> Rules<'static> {
    common::cargo_target_rules(&[ME])
}

/// Eligibility of `target` with the idle requirement `idle` (days).
fn judged(host: &ModelHost, target: &Target, idle_days: Option<u32>) -> Result<Eligible, Skip> {
    let rules = rules();
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    eligibility(
        host,
        target,
        &rules,
        idle_days.map(|d| IdleFor(DAY * d)),
        &mut checker,
        None,
    )
    .unwrap_or_else(|e| panic!("eligibility: {e}"))
}

/// The single target found under `/tmp/w`, judged with `idle_for: 1d`.
fn only_target(host: &ModelHost) -> Result<Eligible, Skip> {
    let d = found(host, &["/tmp/w"]);
    assert_eq!(d.targets.len(), 1, "{d:#?}");
    judged(host, &d.targets[0], Some(1))
}

fn skip(result: Result<Eligible, Skip>) -> Skip {
    result.expect_err("expected a skip")
}

#[track_caller]
fn assert_skip(result: Result<Eligible, Skip>, reason: Reason, needle: &str) -> Skip {
    let skip = skip(result);
    assert_eq!(skip.reason, reason, "{skip}");
    assert!(
        skip.to_string().contains(needle),
        "{needle:?} not in: {skip}"
    );
    skip
}

// ---- Discovery ----

/// Discovery finds a target by a valid tag plus a profile directory with a
/// lock file; it walks hidden directories (agent worktrees), never
/// descends into a found target, follows no symlink, crosses no mount,
/// stops at the depth limit, and needs the tag's signature.
#[test]
fn discovery_finds_targets_also_in_hidden_directories_and_nothing_else() {
    let mount = r#"  - {id: 31, at: /tmp/w/mnt, fs: tmpfs, dev: "0:43", size: 1G, parent: 30}"#;
    let yaml = r#"  /tmp/w/.claude/worktrees/agent-1/target:
      CACHEDIR.TAG: {cachedir_tag: true}
      debug: {.cargo-lock: {}, deps: {.: {}}}
  /tmp/w/p/target/debug/build/x/out/nested/target:
      CACHEDIR.TAG: {cachedir_tag: true}
      debug: {.cargo-lock: {}}
  /tmp/w/untagged/target:
      CACHEDIR.TAG: {}
      debug: {.cargo-lock: {}}
  /tmp/w/no-lock/target:
      CACHEDIR.TAG: {cachedir_tag: true}
      debug: {deps: {.: {}}}
  /tmp/w/link: {symlink: /tmp/w/p}
  /tmp/w/mnt/q/target:
      CACHEDIR.TAG: {cachedir_tag: true}
      debug: {.cargo-lock: {}}
  /tmp/w/a/b/c/d/e/target:
      CACHEDIR.TAG: {cachedir_tag: true}
      debug: {.cargo-lock: {}}
  /tmp/w/a/b/c/d/e/f/target:
      CACHEDIR.TAG: {cachedir_tag: true}
      debug: {.cargo-lock: {}}"#;
    let model = host(&with_mount(&self::yaml(yaml, ""), mount));
    let d = found(&model, &["/tmp/w"]);
    assert_eq!(
        paths(&d),
        [
            "/tmp/w/.claude/worktrees/agent-1/target",
            "/tmp/w/a/b/c/d/e/target",
            "/tmp/w/p/target",
        ],
        "{d:#?}"
    );
    assert_eq!(DISCOVERY_DEPTH, 6, "the fixture's depths assume 6");
    assert!(d.refused.is_empty() && d.duplicates.is_empty(), "{d:#?}");
    // Nothing inside a found target was listed, and the mount was not
    // entered.
    let listed: Vec<PathBuf> = model
        .trace()
        .into_iter()
        .filter(|c| c.op == Op::ReadDir)
        .filter_map(|c| c.path)
        .collect();
    for never in [
        "/tmp/w/p/target/debug/build",
        "/tmp/w/mnt/q",
        "/tmp/w/a/b/c/d/e/f/target",
    ] {
        assert!(
            !listed.iter().any(|p| p.starts_with(never)),
            "{never} was listed: {listed:#?}"
        );
    }
}

/// Overlapping roots (one inside the other, or given twice) find each
/// target once, under the outermost root that reached it (roots are walked
/// outermost first); the others are reported as duplicates.
#[test]
fn overlapping_roots_find_each_target_once() {
    let model = model("", "");
    let d = found(&model, &["/tmp/w/p", "/tmp/w", "/tmp/w/p/target"]);
    assert_eq!(paths(&d), ["/tmp/w/p/target"], "{d:#?}");
    assert_eq!(d.targets[0].root, PathBuf::from("/tmp/w"));
    assert_eq!(
        d.duplicates,
        [
            PathBuf::from("/tmp/w/p/target"),
            PathBuf::from("/tmp/w/p/target")
        ]
    );
}

/// A root on a network filesystem is refused (`unsupported_fs`) without
/// opening or statting it; a missing root is `missing`, a symlinked one
/// `unsafe_path`.
#[test]
fn a_root_on_a_network_filesystem_is_refused_unread() {
    let text = with_mount(
        &yaml(
            "  /tmp/w/l: {symlink: /tmp/w/p}\n  /net/src/p/target/CACHEDIR.TAG: {cachedir_tag: true}",
            "",
        ),
        "  - {id: 40, at: /net, fs: nfs4, dev: \"0:50\", size: 1T, parent: 1}",
    );
    let model = host(&text);
    let d = found(&model, &["/net/src", "/tmp/w/nope", "/tmp/w/l"]);
    assert!(d.targets.is_empty(), "{d:#?}");
    let reasons: Vec<(Reason, String)> = d
        .refused
        .iter()
        .map(|s| (s.reason, s.path.display().to_string()))
        .collect();
    assert_eq!(
        reasons,
        [
            (Reason::UnsupportedFs, "/net/src".to_owned()),
            (Reason::Missing, "/tmp/w/nope".to_owned()),
            (Reason::UnsafePath, "/tmp/w/l".to_owned()),
        ]
    );
    assert!(d.refused[0].detail.contains("nfs4"), "{}", d.refused[0]);
    assert!(
        model.trace().iter().all(|c| {
            !(matches!(c.op, Op::ReadDir | Op::Openat2 | Op::Statx)
                && c.path.as_deref().is_some_and(|p| p.starts_with("/net")))
        }),
        "the network filesystem was touched: {:#?}",
        model.trace()
    );
}

#[test]
fn cargo_discovery_refuses_fuse_subtypes_before_opening_the_root() {
    for fs in ["fuse.sshfs", "fuseblk.ntfs-3g", "fuse.fuse-overlayfs"] {
        let text = with_mount(
            &yaml("  /net/src/p/target/CACHEDIR.TAG: {cachedir_tag: true}", ""),
            &format!("  - {{id: 40, at: /net, fs: {fs}, dev: \"0:50\", size: 1T, parent: 1}}"),
        );
        let model = host(&text);
        let discovery = found(&model, &["/net/src"]);
        assert!(discovery.targets.is_empty(), "{fs}: {discovery:#?}");
        assert_eq!(discovery.refused.len(), 1, "{fs}: {discovery:#?}");
        assert_eq!(discovery.refused[0].reason, Reason::UnsupportedFs, "{fs}");
        assert!(
            model.trace().iter().all(|call| {
                !(matches!(call.op, Op::Openat2 | Op::Statx | Op::ReadDir)
                    && call
                        .path
                        .as_deref()
                        .is_some_and(|path| path.starts_with("/net")))
            }),
            "{fs}: {:#?}",
            model.trace()
        );
    }
}

#[test]
fn cargo_discovery_never_enters_private_tmp_below_a_root() {
    let private = "/tmp/w/systemd-private-abc-cups.service-x1";
    let tree = format!("  {private}/p/target/CACHEDIR.TAG: {{cachedir_tag: true}}");
    let first = model(&tree, "");
    let discovery = found(&first, &["/tmp/w"]);
    assert_eq!(paths(&discovery), ["/tmp/w/p/target"], "{discovery:#?}");
    assert!(
        first.trace().iter().all(|call| !call
            .path
            .as_deref()
            .is_some_and(|path| path.starts_with(private))),
        "PrivateTmp was touched: {:#?}",
        first.trace()
    );

    let root = format!("{private}/p");
    let model = model(&tree, "");
    let discovery = found(&model, &[&root]);
    assert!(discovery.targets.is_empty(), "{discovery:#?}");
    assert_eq!(discovery.refused[0].reason, Reason::UnsafePath);
    assert!(model.trace().iter().all(|call| {
        !call
            .path
            .as_deref()
            .is_some_and(|path| path.starts_with(private))
    }));
}

#[test]
fn cargo_discovery_does_not_stat_a_mount_below_a_root_or_profile() {
    for at in ["/tmp/w/dead", "/tmp/w/p/target/debug/build"] {
        let yaml = with_mount(
            &yaml(&format!("  {at}/f: {{}}"), ""),
            &format!(
                "  - {{id: 41, at: {at}, fs: fuse.sshfs, dev: \"0:51\", size: 1G, parent: 30}}"
            ),
        );
        let model = host(&yaml);
        let discovery = found(&model, &["/tmp/w"]);
        assert!(
            model.trace().iter().all(|call| !call
                .path
                .as_deref()
                .is_some_and(|path| path.starts_with(at))),
            "{at} was touched: {:#?}",
            model.trace()
        );
        if at.ends_with("/dead") {
            assert_eq!(paths(&discovery), ["/tmp/w/p/target"]);
        } else {
            assert!(discovery.targets.is_empty(), "{discovery:#?}");
            assert_eq!(discovery.refused[0].reason, Reason::ContainsMount);
        }
    }
}

/// A profile directory is recognised by any `.cargo-*lock*` name, so a
/// target whose only lock file has a name diskgc does not know is found
/// (and then skipped as `unsupported_layout`, not silently missed); also
/// two levels down, below a target triple.
#[test]
fn profile_directories_are_found_by_any_lock_name_and_below_a_triple() {
    let model = model(
        r#"  /tmp/w/q/target:
      CACHEDIR.TAG: {cachedir_tag: true}
      x86_64-unknown-linux-gnu: {release: {.cargo-lock: {}, deps: {.: {}}}}
      future: {.cargo-new-lock: {}}"#,
        "",
    );
    let d = found(&model, &["/tmp/w/q"]);
    let target = &d.targets[0];
    let rels: Vec<String> = target
        .inspected
        .profiles
        .iter()
        .map(|p| p.rel.display().to_string())
        .collect();
    assert_eq!(rels, ["future", "x86_64-unknown-linux-gnu/release"]);
    assert_eq!(
        target.inspected.profiles[1].lock_set(),
        LockSet::Whole,
        "{target:#?}"
    );
    assert_skip(
        judged(&model, target, Some(1)),
        Reason::UnsupportedLayout,
        "/tmp/w/q/target/future/.cargo-new-lock",
    );
}

// ---- Lock allowlist and lock sets (AC #1) ----

/// AC #1: a profile directory holding an unknown `.cargo-*lock*` name next
/// to the known ones is `unsupported_layout`, naming the file; without it
/// the same target is eligible.
#[test]
fn an_unknown_lock_name_is_unsupported_layout() {
    let clean = model("", "");
    only_target(&clean).expect("the control target is eligible");
    let model = model("  /tmp/w/p/target/debug/.cargo-shared-lock: {}", "");
    let skip = assert_skip(
        only_target(&model),
        Reason::UnsupportedLayout,
        "an unknown Cargo lock file name",
    );
    assert_eq!(
        skip.path,
        PathBuf::from("/tmp/w/p/target/debug/.cargo-shared-lock")
    );
}

/// The measured whole lock sets pass; the two halves of a split build
/// directory (1.99.1 and the nightly with `CARGO_BUILD_BUILD_DIR`) are
/// `unsupported_layout`, each on its own: nothing pairs them, and they
/// would have to be locked together.
#[test]
fn split_build_directory_halves_are_unsupported_whole_lock_sets_pass() {
    let target = |name: &str, locks: &str, rest: &str| {
        format!(
            "  /tmp/w/{name}/target:\n      CACHEDIR.TAG: {{cachedir_tag: true}}\n      debug: {{{locks}{rest}}}\n"
        )
    };
    let classic = ", .fingerprint: {.: {}}, deps: {.: {}}";
    let yaml = [
        target("v190", ".cargo-lock: {}", classic),
        target("check", ".cargo-lock: {}, .cargo-build-lock: {}", classic),
        target(
            "build",
            ".cargo-lock: {}, .cargo-build-lock: {}, .cargo-artifact-lock: {}",
            classic,
        ),
        target("build-half", ".cargo-build-lock: {}", classic),
        target(
            "artifact-half",
            ".cargo-lock: {}, .cargo-artifact-lock: {}",
            ", examples: {.: {}}, p: {}",
        ),
        target("odd", ".cargo-artifact-lock: {}", classic),
    ]
    .concat();
    let model = model(&yaml, "");
    let d = found(&model, &["/tmp/w"]);
    let by_name = |name: &str| {
        d.targets
            .iter()
            .find(|t| t.path == Path::new("/tmp/w").join(name).join("target"))
            .unwrap_or_else(|| panic!("{name} not found: {d:#?}"))
    };
    for whole in ["v190", "check", "build"] {
        let t = by_name(whole);
        assert_eq!(t.inspected.profiles[0].lock_set(), LockSet::Whole);
        judged(&model, t, Some(1)).unwrap_or_else(|s| panic!("{whole}: {s}"));
    }
    for (half, set, needle) in [
        (
            "build-half",
            LockSet::SplitBuildDir,
            "build directory's half",
        ),
        (
            "artifact-half",
            LockSet::SplitArtifactDir,
            "target directory's half",
        ),
        ("odd", LockSet::Unmeasured, "no measured Cargo makes"),
    ] {
        let t = by_name(half);
        assert_eq!(t.inspected.profiles[0].lock_set(), set, "{half}");
        assert_skip(
            judged(&model, t, Some(1)),
            Reason::UnsupportedLayout,
            needle,
        );
    }
}

// ---- Layouts, last build, idleness ----

/// Both layouts are recognised and an idle target of each is eligible; a
/// profile holding something but no fingerprints in a known place is an
/// unknown layout; one holding only lock files is `empty`.
#[test]
fn both_layouts_are_eligible_unknown_ones_are_not_and_lock_only_is_empty() {
    let model = model(
        &format!(
            r#"  /tmp/w/nightly/target:
      CACHEDIR.TAG: {{cachedir_tag: true}}
      debug:{PER_UNIT}
  /tmp/w/strange/target:
      CACHEDIR.TAG: {{cachedir_tag: true}}
      debug: {{.cargo-lock: {{}}, p: {{}}, examples: {{.: {{}}}}}}
  /tmp/w/empty/target:
      CACHEDIR.TAG: {{cachedir_tag: true}}
      debug: {{.cargo-lock: {{}}, .cargo-build-lock: {{}}}}
      release: {{.cargo-lock: {{}}}}"#
        ),
        "",
    );
    let d = found(&model, &["/tmp/w"]);
    let get = |name: &str| {
        d.targets
            .iter()
            .find(|t| t.path == Path::new("/tmp/w").join(name).join("target"))
            .unwrap_or_else(|| panic!("{name}: {d:#?}"))
    };
    assert_eq!(
        get("p").inspected.profiles[0].layout(),
        Some(Layout::Classic)
    );
    assert_eq!(
        get("nightly").inspected.profiles[0].layout(),
        Some(Layout::PerUnit)
    );
    for ok in ["p", "nightly"] {
        let e = judged(&model, get(ok), Some(1)).unwrap_or_else(|s| panic!("{ok}: {s}"));
        assert!(e.bytes >= 2 << 20, "{ok}: {e:?}");
    }
    assert_skip(
        judged(&model, get("strange"), Some(1)),
        Reason::UnsupportedLayout,
        "an unknown layout",
    );
    assert_skip(
        judged(&model, get("empty"), Some(1)),
        Reason::Empty,
        "nothing but lock files",
    );
}

/// The last build is the newest time of anything in the profile
/// directories, on both layouts: a recent file deep in a classic
/// `incremental/` or in a per-unit `build/<pkg>/<hash>/fingerprint/` makes
/// the target not idle; without a requirement it is eligible and reports
/// that time.
#[test]
fn the_last_build_is_the_newest_change_anywhere_in_the_profiles_on_both_layouts() {
    let recent = "{mtime: 2026-10-17T02:00:00Z}";
    for (layout, profile, deep) in [
        ("classic", CLASSIC, "debug/incremental/p-2/s-1/new"),
        (
            "per-unit",
            PER_UNIT,
            "debug/build/p/1727f8a9/fingerprint/new",
        ),
    ] {
        let model = model(
            &format!(
                "  /tmp/w/q/target:\n      CACHEDIR.TAG: {{cachedir_tag: true}}\n      debug:{profile}\n  /tmp/w/q/target/{deep}: {recent}"
            ),
            "",
        );
        let d = found(&model, &["/tmp/w/q"]);
        assert_skip(
            judged(&model, &d.targets[0], Some(1)),
            Reason::NotIdle,
            "idle_for 86400 s",
        );
        let e = judged(&model, &d.targets[0], None).unwrap_or_else(|s| panic!("{layout}: {s}"));
        assert_eq!(
            e.last_build.map(|t| t.secs),
            Some(1_792_202_400),
            "{layout}: 2026-10-17T02:00:00Z"
        );
    }
}

/// An age diskgc cannot tell is never idle: a filesystem that does not
/// report mtimes gives `unsupported_fs`, a last build in the future
/// `future_time`, under `filter:idle-*` alike.
#[test]
fn an_unknown_age_is_never_idle() {
    let no_mtime = remount(
        &yaml("", ""),
        "  - {id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 1, statx_missing: [mtime]}",
    );
    let model_a = host(&no_mtime);
    assert_skip(
        only_target(&model_a),
        Reason::UnsupportedFs,
        "an unknown age is never idle",
    );
    let future = model(
        "  /tmp/w/p/target/debug/deps/later: {mtime: 2027-01-01T00:00:00Z}",
        "",
    );
    assert_skip(only_target(&future), Reason::FutureTime, "lies after now");
}

// ---- Keep markers, ownership, changes ----

/// A keep marker keeps the target: between the root and the target, in
/// the target, in a directory directly in it, or in what would be
/// deleted. One deeper in a part the kind never deletes (`target/doc/x/`)
/// does not.
#[test]
fn keep_markers_keep_the_target() {
    for marker in [
        "/tmp/w/.diskgc-keep",
        "/tmp/w/p/.diskgc-keep",
        "/tmp/w/p/target/.diskgc-keep",
        "/tmp/w/p/target/doc/.diskgc-keep",
        "/tmp/w/p/target/debug/.diskgc-keep",
        "/tmp/w/p/target/debug/.fingerprint/p-1/.diskgc-keep",
    ] {
        let model = model(&format!("  {marker}: {{}}"), "");
        let skip = assert_skip(only_target(&model), Reason::Kept, "");
        assert_eq!(skip.path, PathBuf::from(marker));
    }
    let model = model("  /tmp/w/p/target/doc/x/.diskgc-keep: {}", "");
    only_target(&model).expect("a marker inside target/doc/x keeps only that");
}

/// Every entry that would be deleted, and the lock files and directories
/// the kind relies on, must be the user's.
#[test]
fn another_users_entry_or_lock_file_is_not_owner() {
    for path in [
        "/tmp/w/p/target/debug/deps/p-1",
        "/tmp/w/p/target/debug/.cargo-lock",
        "/tmp/w/p/target/debug",
    ] {
        let model = model(
            "",
            &format!(
                "  - {{at: {{op: mountinfo, nth: 1, when: after}}, chown: {{path: {path}, uid: 1001}}}}"
            ),
        );
        let skip = assert_skip(only_target(&model), Reason::NotOwner, "1001");
        assert_eq!(skip.path, PathBuf::from(path));
    }
}

/// A profile or lock file created after discovery, or the target replaced,
/// is `changed` at the check (race test 5's state, in the model).
#[test]
fn a_new_profile_or_a_replaced_target_after_discovery_is_changed() {
    // Discovery resolves its root (mountinfo #1); the check reaches the
    // target again from the root (mountinfo #2).
    for event in [
        "  - {at: {op: mountinfo, nth: 2, when: before}, create: /tmp/w/p/target/release/.cargo-lock}",
        "  - {at: {op: mountinfo, nth: 2, when: before}, replace: /tmp/w/p/target/debug/.cargo-lock}",
        "  - {at: {op: mountinfo, nth: 2, when: before}, rename: {from: /tmp/w/p/target, to: /tmp/w/p/old}}",
    ] {
        let model = model("", event);
        let d = found(&model, &["/tmp/w"]);
        let skip = skip(judged(&model, &d.targets[0], Some(1)));
        assert!(
            matches!(skip.reason, Reason::Changed | Reason::Missing),
            "{event}: {skip}"
        );
    }
}

/// What the walks refuse refuses the target: a nested Cargo target inside
/// an entry (the structural recogniser) is `contains_special`, a mount in
/// the target `contains_mount`.
#[test]
fn walk_refusals_and_mounts_refuse_the_target() {
    let model = model(
        "  /tmp/w/p/target/debug/build/x/out/t:\n      CACHEDIR.TAG: {}\n      debug: {.cargo-lock: {}}",
        "",
    );
    assert_skip(only_target(&model), Reason::ContainsSpecial, "Cargo lock");
    let mounted = host(&with_mount(
        &yaml("  /tmp/w/p/target/tmp: {.: {}}", ""),
        "  - {id: 31, at: /tmp/w/p/target/tmp, fs: tmpfs, dev: \"0:43\", size: 1G, parent: 30}",
    ));
    let d = found(&mounted, &["/tmp/w"]);
    assert!(d.targets.is_empty(), "{d:#?}");
    assert_eq!(d.refused[0].reason, Reason::ContainsMount, "{d:#?}");
}

/// Build scripts legitimately clone sources into a target (here
/// `build/<pkg>/out/vendor/` holding `.git`, a `.git` file of a worktree,
/// `.hg` and `.svn`), and the target stays eligible: the built-in
/// `cargo-target` says `checkouts: allow` (task-80; these rules take it
/// from the library, no code exempts the Cargo kind).
#[test]
fn a_vcs_checkout_inside_a_target_leaves_it_eligible() {
    let model = model(
        "  /tmp/w/p/target/debug/build/x/out/vendor:\n      .git: {HEAD: {}, objects: {.: {}}}\n      \
         wt: {.git: {}}\n      hg: {.hg: {store: {.: {}}}}\n      svn: {.svn: {wc.db: {}}}",
        "",
    );
    let e = only_target(&model).unwrap_or_else(|s| panic!("{s}"));
    assert!(e.bytes >= 2 << 20, "{e:?}");
}

// ---- In use (AC #3): each rule on its own ----

/// A process line with the readable defaults a model process needs (an
/// exe, a mapping, an fd and an environment: an empty one reads as
/// unreadable, as on Linux).
fn process(pid: u32, exe: &str, cwd: &str, environ: &[&str], fd: &str) -> String {
    let env: Vec<String> = environ.iter().map(|e| format!("\"{e}\"")).collect();
    format!(
        "  - {{pid: {pid}, exe: {exe}, cwd: {cwd}, maps: [{exe}], fds: [{{path: {fd}}}], \
         environ: [HOME=/home/x{}{}]}}\n",
        if env.is_empty() { "" } else { ", " },
        env.join(", ")
    )
}

/// The target is `in_use` and the evidence names `pid` with `needle`.
#[track_caller]
fn in_use_by(model: &ModelHost, pid: u32, needle: &str) {
    let skip = assert_skip(only_target(model), Reason::InUse, needle);
    assert!(
        skip.detail.contains(&format!("pid {pid} (")),
        "pid {pid} not named: {skip}"
    );
}

fn with_processes(tree: &str, processes: &[String]) -> ModelHost {
    model(tree, &format!("processes:\n{}", processes.concat()))
}

/// A bystander with nothing in the target does not make it in use: the
/// control for every rule below.
#[test]
fn a_process_with_nothing_in_the_target_leaves_it_eligible() {
    let model = with_processes(
        "",
        &[process(
            42,
            "/bin/sh",
            "/tmp/w",
            &["CARGO_MANIFEST_DIR=/tmp/w/other", "TMPDIR=/tmp/w/pp/target"],
            "/bin/sh",
        )],
    );
    only_target(&model).expect("eligible");
}

/// Rule 1: an open fd on a lock file (or a directory of the target) is a
/// use by identity.
#[test]
fn an_fd_on_a_lock_file_is_in_use() {
    let model = with_processes(
        "",
        &[process(
            42,
            "/bin/sh",
            "/",
            &[],
            "/tmp/w/p/target/debug/.cargo-lock",
        )],
    );
    in_use_by(&model, 42, "/tmp/w/p/target/debug/.cargo-lock open as fd");
}

/// Rule 2: a working directory or an executable inside the target, also in
/// parts the walks do not read (`target/doc/...`, `target/tmp/...`).
#[test]
fn a_cwd_or_exe_inside_the_target_is_in_use() {
    let tree = "  /tmp/w/p/target/doc/x/y: {.: {}}\n  /tmp/w/p/target/tmp/t/bin: {}";
    let cwd = with_processes(
        tree,
        &[process(
            42,
            "/bin/sh",
            "/tmp/w/p/target/doc/x/y",
            &[],
            "/bin/sh",
        )],
    );
    in_use_by(&cwd, 42, "works inside the target");
    let exe = with_processes(
        tree,
        &[process(
            43,
            "/tmp/w/p/target/tmp/t/bin",
            "/",
            &[],
            "/bin/sh",
        )],
    );
    in_use_by(&exe, 43, "runs an executable inside the target");
}

/// AC #3, a running test binary: its executable is an entry the walk
/// recorded (matched by identity) and a path inside the target.
#[test]
fn a_running_test_binary_is_in_use() {
    let model = with_processes(
        "",
        &[process(
            42,
            "/tmp/w/p/target/debug/deps/p-1",
            "/",
            &[],
            "/bin/sh",
        )],
    );
    let skip = assert_skip(only_target(&model), Reason::InUse, "as its executable");
    assert!(
        skip.detail.contains("runs an executable inside the target"),
        "{skip}"
    );
}

/// AC #3, rule 3: a Cargo process (also the Nix wrapper's name) working
/// in the project directory or below it; the same process elsewhere, and
/// another command there, do not count.
#[test]
fn a_cargo_process_in_the_project_directory_is_in_use() {
    for (exe, cwd) in [
        ("/opt/cargo", "/tmp/w/p"),
        ("/opt/.cargo-wrapped", "/tmp/w/p/src"),
        ("/opt/cargo-nextest", "/tmp/w/p"),
    ] {
        let model = with_processes(
            &format!("  {exe}: {{}}"),
            &[process(42, exe, cwd, &[], "/bin/sh")],
        );
        in_use_by(
            &model,
            42,
            "is a Cargo process working in the project directory /tmp/w/p",
        );
    }
    for (exe, cwd) in [("/opt/cargo", "/tmp/w"), ("/opt/rustc", "/tmp/w/p")] {
        let model = with_processes(
            &format!("  {exe}: {{}}"),
            &[process(42, exe, cwd, &[], "/bin/sh")],
        );
        only_target(&model).unwrap_or_else(|s| panic!("{exe} in {cwd}: {s}"));
    }
}

/// AC #3, rule 4: `CARGO_TARGET_DIR` (or any variable, e.g. Cargo's
/// `LD_LIBRARY_PATH` for what it runs) naming the target or a path inside
/// it in a process's initial environment.
#[test]
fn cargo_target_dir_in_the_initial_environment_is_in_use() {
    for var in [
        "CARGO_TARGET_DIR=/tmp/w/p/target",
        "LD_LIBRARY_PATH=/nix/lib:/tmp/w/p/target/debug",
    ] {
        let model = with_processes("", &[process(42, "/bin/sh", "/", &[var], "/bin/sh")]);
        in_use_by(
            &model,
            42,
            &format!(
                "names a path inside the target in its initial environment ({}",
                var.split('=').next().expect("a name")
            ),
        );
    }
}

/// Rule 5: `CARGO_MANIFEST_DIR` in the project directory or below it (the
/// runner `cargo run` execs in its own process, a test binary's runner);
/// one outside the project does not count.
#[test]
fn cargo_manifest_dir_of_the_project_in_the_initial_environment_is_in_use() {
    for dir in ["/tmp/w/p", "/tmp/w/p/crates/a"] {
        let model = with_processes(
            "",
            &[process(
                42,
                "/opt/runner",
                "/",
                &[&format!("CARGO_MANIFEST_DIR={dir}")],
                "/bin/sh",
            )],
        );
        in_use_by(
            &model,
            42,
            "was started by Cargo for a package of the project directory",
        );
    }
}

/// A process that cannot be read (and is not explained as another user's
/// or privileged) could be using the target: `in_use`.
#[test]
fn an_unreadable_process_blocks() {
    let model = model(
        "",
        "processes:\n  - {pid: 42, exe: /bin/sh, proc: failed}\n",
    );
    assert_skip(only_target(&model), Reason::InUse, "cannot be inspected");
}

/// Eligibility deletes nothing and takes no lock: no `unlinkat`, `fchmod`
/// or `flock` on the trace.
#[test]
fn eligibility_neither_deletes_nor_locks() {
    let model = model("", "");
    only_target(&model).expect("eligible");
    for call in model.trace() {
        assert!(
            !matches!(call.op, Op::Unlinkat | Op::Fchmod | Op::Flock),
            "{call:?}"
        );
    }
}

/// The model reads a `cachedir_tag` file as the signature; a tag without
/// it is not a target's.
#[test]
fn only_a_signed_tag_marks_a_target() {
    let text = yaml("", "").replace(
        "      CACHEDIR.TAG: {cachedir_tag: true}\n      debug:",
        "      CACHEDIR.TAG: {}\n      debug:",
    );
    let model = host(&text);
    assert!(found(&model, &["/tmp/w"]).targets.is_empty());
}

// ---- Round-1 review findings ----

/// Codex 1: layout evidence must be directories. A regular file or a
/// symlink called `.fingerprint`, `deps` or `build` (next to a lock file and
/// some data, under a signed tag) is no Cargo layout: `unsupported_layout`.
/// The same names as directories pass (the control).
#[test]
fn layout_markers_must_be_directories() {
    for marker in [".fingerprint", "deps", "build"] {
        for (what, node) in [
            ("a regular file", "{size: 1K}"),
            ("a symlink", "{symlink: /tmp/w/elsewhere}"),
            ("a directory", "{.: {}}"),
        ] {
            let model = model(
                &format!(
                    "  /tmp/w/elsewhere: {{.: {{}}}}\n  /tmp/w/q/target:\n      CACHEDIR.TAG: {{cachedir_tag: true}}\n      debug: {{.cargo-lock: {{}}, valuable-data: {{size: 1K}}, \"{marker}\": {node}}}"
                ),
                "",
            );
            let d = found(&model, &["/tmp/w/q"]);
            assert_eq!(d.targets.len(), 1, "{marker} as {what}: {d:#?}");
            let verdict = judged(&model, &d.targets[0], Some(1));
            if what == "a directory" {
                verdict.unwrap_or_else(|s| panic!("{marker} as a directory: {s}"));
            } else {
                assert_skip(verdict, Reason::UnsupportedLayout, "an unknown layout");
            }
        }
    }
}

/// mped B1: with overlapping roots, a keep marker above the inner root is
/// honoured whatever order the roots are given in (the target is recorded
/// under the outermost root).
#[test]
fn a_keep_marker_above_an_inner_root_counts_in_either_root_order() {
    for order in [["/tmp/w", "/tmp/w/p"], ["/tmp/w/p", "/tmp/w"]] {
        let model = model("  /tmp/w/.diskgc-keep: {}", "");
        let d = found(&model, &order);
        assert_eq!(d.targets.len(), 1, "{order:?}: {d:#?}");
        assert_eq!(d.targets[0].root, PathBuf::from("/tmp/w"), "{order:?}");
        let skip = assert_skip(judged(&model, &d.targets[0], Some(1)), Reason::Kept, "");
        assert_eq!(skip.path, PathBuf::from("/tmp/w/.diskgc-keep"), "{order:?}");
    }
}

/// mped S2: a detached target (its parent holds no `Cargo.toml`: a
/// `--target-dir`, a target-dir from `.cargo/config.toml`, a nested
/// target) is in use while any Cargo runs, wherever it works; with a
/// `Cargo.toml` next to it, a Cargo outside its parent does not count, and
/// another command never does.
#[test]
fn a_cargo_process_anywhere_keeps_a_detached_target() {
    let target = format!(
        "  /opt/cargo: {{}}\n  /tmp/w/q/Cargo.toml: {{}}\n  /tmp/w/q/out/target:\n      CACHEDIR.TAG: {{cachedir_tag: true}}\n      debug:{CLASSIC}"
    );
    let cargo_in_q = process(42, "/opt/cargo", "/tmp/w/q", &[], "/bin/sh");
    let detached = with_processes(&target, std::slice::from_ref(&cargo_in_q));
    let d = found(&detached, &["/tmp/w/q"]);
    let skip = assert_skip(
        judged(&detached, &d.targets[0], Some(1)),
        Reason::InUse,
        "is a Cargo process, and the target is detached",
    );
    assert!(skip.detail.contains("pid 42 ("), "{skip}");
    let attached = with_processes(
        &format!("{target}\n  /tmp/w/q/out/Cargo.toml: {{}}"),
        std::slice::from_ref(&cargo_in_q),
    );
    let d = found(&attached, &["/tmp/w/q"]);
    judged(&attached, &d.targets[0], Some(1)).expect("attached: the cargo works elsewhere");
    let not_cargo = with_processes(
        &target,
        &[process(42, "/bin/sh", "/tmp/w/q", &[], "/bin/sh")],
    );
    let d = found(&not_cargo, &["/tmp/w/q"]);
    judged(&not_cargo, &d.targets[0], Some(1)).expect("detached, but no Cargo runs");
}

/// Codex round 2: only a regular file is a lock file. A directory, symlink,
/// FIFO, socket or character device named `.cargo-lock` (next to a real
/// `build/` directory and some data, under a signed tag) is
/// `unsupported_layout`, so nothing in that profile counts as reclaimable;
/// a regular `.cargo-lock` there is eligible (the control).
#[test]
fn a_lock_name_on_anything_but_a_regular_file_is_unsupported_layout() {
    for (what, node) in [
        ("a directory", "{.: {}}"),
        ("a symlink", "{symlink: /tmp/w/elsewhere}"),
        ("a FIFO", "{type: fifo}"),
        ("a socket", "{type: socket}"),
        ("a character device", "{type: char_device}"),
        ("a regular file", "{}"),
    ] {
        let model = model(
            &format!(
                "  /tmp/w/elsewhere: {{.: {{}}}}\n  /tmp/w/q/target:\n      CACHEDIR.TAG: {{cachedir_tag: true}}\n      debug: {{.cargo-lock: {node}, build: {{.: {{}}}}, valuable-data: {{size: 1K}}}}"
            ),
            "",
        );
        let d = found(&model, &["/tmp/w/q"]);
        assert_eq!(d.targets.len(), 1, "{what}: {d:#?}");
        let verdict = judged(&model, &d.targets[0], Some(1));
        if what == "a regular file" {
            verdict.unwrap_or_else(|s| panic!("the control: {s}"));
        } else {
            let skip = assert_skip(verdict, Reason::UnsupportedLayout, "not a regular file");
            assert_eq!(
                skip.path,
                PathBuf::from("/tmp/w/q/target/debug/.cargo-lock"),
                "{what}"
            );
        }
    }
}

/// The layout is judged from the inspected entry types; an entry whose
/// type changes before its walk (here the per-unit marker `build/`
/// replaced by a regular file right before the walk opens the profile
/// directory) is `changed`, not walked as what it now is.
#[test]
fn a_layout_marker_whose_type_changes_before_its_walk_is_changed() {
    // openat2 of the profile directory: #1 discovery's inspection, #2 the
    // check's inspection, #3 the walk's.
    let profile = "/tmp/w/q/target/debug";
    let model = model(
        "  /tmp/w/q/target:\n      CACHEDIR.TAG: {cachedir_tag: true}\n      debug: {.cargo-lock: {}, build: {.: {}}, valuable-data: {size: 1K}}",
        &format!(
            "  - {{at: {{op: openat2, path: {profile}, nth: 3, when: before}}, remove: {profile}/build}}\n  - {{at: {{op: openat2, path: {profile}, nth: 3, when: after}}, create: {profile}/build}}"
        ),
    );
    let d = found(&model, &["/tmp/w/q"]);
    let skip = assert_skip(
        judged(&model, &d.targets[0], Some(1)),
        Reason::Changed,
        "not of the type inspected",
    );
    assert_eq!(skip.path, PathBuf::from(format!("{profile}/build")));
}

/// Codex round 3: only a regular `Cargo.toml` in its parent makes a target
/// attached. A directory, a symlink to a regular file or a dangling
/// symlink of that name leaves it detached, so a Cargo working elsewhere
/// keeps it `in_use`; a regular `Cargo.toml` makes it attached and that
/// Cargo does not count (the control).
#[test]
fn only_a_regular_cargo_toml_makes_a_target_attached() {
    for (what, node) in [
        ("a directory", "{.: {}}"),
        (
            "a symlink to a regular file",
            "{symlink: /tmp/w/q/Cargo.toml}",
        ),
        ("a dangling symlink", "{symlink: /tmp/w/q/nowhere.toml}"),
        ("a regular file", "{}"),
    ] {
        let model = with_processes(
            &format!(
                "  /opt/cargo: {{}}\n  /tmp/w/q/Cargo.toml: {{}}\n  /tmp/w/q/out/Cargo.toml: {node}\n  /tmp/w/q/out/target:\n      CACHEDIR.TAG: {{cachedir_tag: true}}\n      debug:{CLASSIC}"
            ),
            &[process(42, "/opt/cargo", "/tmp/w/q", &[], "/bin/sh")],
        );
        let d = found(&model, &["/tmp/w/q"]);
        let target = d
            .targets
            .iter()
            .find(|t| t.path == Path::new("/tmp/w/q/out/target"))
            .unwrap_or_else(|| panic!("{what}: {d:#?}"));
        let verdict = judged(&model, target, Some(1));
        if what == "a regular file" {
            verdict.unwrap_or_else(|s| panic!("the control: {s}"));
        } else {
            assert_skip(
                verdict,
                Reason::InUse,
                "is a Cargo process, and the target is detached",
            );
        }
    }
}

/// Codex round 3: `.cargo-lock` atomically replaced by a symlink (a rename
/// over it) between discovery and the check is `changed`.
#[test]
fn a_lock_file_replaced_by_a_symlink_after_discovery_is_changed() {
    let model = model(
        "  /tmp/w/p/target/new-lock: {symlink: /tmp/w/elsewhere}\n  /tmp/w/elsewhere: {.: {}}",
        "  - {at: {op: mountinfo, nth: 2, when: before}, rename: {from: /tmp/w/p/target/new-lock, to: /tmp/w/p/target/debug/.cargo-lock}}",
    );
    let d = found(&model, &["/tmp/w"]);
    assert_skip(
        judged(&model, &d.targets[0], Some(1)),
        Reason::Changed,
        "lock file was created, removed or replaced since discovery",
    );
}
