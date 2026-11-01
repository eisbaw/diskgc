//! The `cargo-target` reclaim protocol (diskgc-core
//! `cargo_target::reclaim`; docs/safety.md, `cargo-target`: Reclaim
//! protocol) on ModelHost: lock every lock file, recheck under the locks,
//! delete inside the locked profile directories (fingerprints first, the
//! layout's directories last), release. Each rule has a test that fails
//! when the rule is removed; ModelHost also fails a test on any call the
//! safety rules forbid (an `unlinkat` without the identity `statx` right
//! before it, an `openat2` without the `RESOLVE_*` flags, opening a FIFO
//! for reading).

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::cargo_target::{
    ReclaimEnd, Reclaimed, Stop, Target, discover, eligibility, reach, reclaim,
};
use diskgc_core::engine::{CARGO_LOCK_NAMES, Reason, Rules, is_cargo_lock_name};
use diskgc_core::host::{
    Base, BoundSocket, Cause, Delete, DirListing, Environ, Host, HostError, LinkTarget, LockMode,
    Locked, MapEntry, MonoTime, MountEntry, OpenHow, Pid, ProcError, ProcFd, ProcLink, ProcStatus,
    ProcView, Space, Stat, Timestamp, TryLock, Unlink, WaitLock,
};
use diskgc_core::inuse::{Checker, REUSE_LIMIT};
use diskgc_core::profile::{IdleFor, IdleWindow};
use diskgc_model::spec::Op;
use diskgc_model::{Holder, ModelFd, ModelHost};

const ME: u32 = 1000;
const DAY: Duration = Duration::from_secs(86_400);
const TARGET: &str = "/tmp/w/p/target";

/// A classic profile directory (Cargo 1.99.1 after a build): all three
/// lock files, `.fingerprint/`, `build/`, `deps/`, `examples/`,
/// `incremental/`, the binary. 16 entries but the lock files.
const CLASSIC: &str = "{.cargo-lock: {}, .cargo-build-lock: {}, .cargo-artifact-lock: {}, \
    .fingerprint: {p-1: {bin-p: {}}}, build: {p-2: {out: {}}}, deps: {p-1: {size: 1M}, p-1.d: {}}, \
    examples: {.: {}}, incremental: {p-3: {s-1: {f: {}}}}, p: {size: 1M}, p.d: {}}";

/// The per-unit layout (the 2026-10-16 nightly's default, measured): no
/// `.fingerprint/` and no `deps/`; `build/<package>/<hash>/` with its
/// `fingerprint/`, `out/` and, for a build script's run, `run/`.
const PER_UNIT: &str = "{.cargo-lock: {}, .cargo-build-lock: {}, .cargo-artifact-lock: {}, \
    build: {dep: {d1: {fingerprint: {lib-dep: {}}, out: {libdep.rlib: {size: 1M}}}}, \
    p: {b1: {fingerprint: {run-build: {}}, out: {.: {}}, run: {stdout: {}}}, \
    c1: {fingerprint: {bin-p: {}, dep-bin-p: {}}, out: {p: {size: 1M}}}}}, \
    examples: {.: {}}, incremental: {p-3: {s-1: {f: {}}}}, p: {size: 1M}, p.d: {}}";

/// A profile directory both Cargo 1.99.1 and the nightly built (mped S1):
/// classic `.fingerprint/` and `deps/` (so its layout reads as classic),
/// a classic build script's `build/p-2/`, and a per-unit unit
/// `build/q/h1/` with its own `fingerprint/`. 19 entries but the lock
/// files.
const MIXED: &str = "{.cargo-lock: {}, .cargo-build-lock: {}, .cargo-artifact-lock: {}, \
    .fingerprint: {p-1: {bin-p: {}}}, \
    build: {p-2: {out: {}}, q: {h1: {fingerprint: {lib-q: {}}, out: {libq.rlib: {size: 1M}}}}}, \
    deps: {p-1: {size: 1M}}, incremental: {p-3: {s-1: {f: {}}}}, p: {size: 1M}}";

/// `/tmp/w/p` with its target: `debug` holding `debug`, a triple's
/// `debug` holding `triple` (none: no triple directory), and what the
/// kind never touches: `CACHEDIR.TAG`, `.rustc_info.json`, `doc/`,
/// `tmp/`, `criterion/`. The clock is two days past every time in the tree
/// (an event at the first call). `rest`: further top-level keys
/// (processes, locks, events after the first).
fn yaml(debug: &str, triple: Option<&str>, rest: &str) -> String {
    let triple = triple.map_or(String::new(), |t| {
        format!("      x86_64-unknown-linux-gnu: {{debug: {t}}}\n")
    });
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}}
tree:
  /bin/sh: {{}}
  /nix/store/x-cargo/bin/cargo: {{}}
  /tmp/w/p:
    Cargo.toml: {{}}
    src: {{main.rs: {{}}}}
    target:
      CACHEDIR.TAG: {{cachedir_tag: true}}
      .rustc_info.json: {{}}
      doc: {{p: {{index.html: {{}}}}}}
      tmp: {{t: {{}}}}
      criterion: {{base: {{estimates.json: {{}}}}}}
      debug: {debug}
{triple}{rest}
events:
  - {{at: {{call: 1, when: before}}, advance: 2d}}
"#
    )
}

/// `yaml` with more events after the first (each line indented as an
/// item of `events`).
fn with_events(yaml: &str, events: &[&str]) -> String {
    let mut out = yaml.to_owned();
    for event in events {
        out.push_str("  - ");
        out.push_str(event);
        out.push('\n');
    }
    out
}

/// The rules of a `profile:cargo-target` tier: its `checkouts` setting
/// comes from the built-in profile, as in the executor.
fn rules() -> Rules<'static> {
    common::cargo_target_rules(&[ME])
}

/// The single target found below `/tmp/w`.
fn found(host: &ModelHost) -> Target {
    let d = discover(host, &[PathBuf::from("/tmp/w")], None).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(d.targets.len(), 1, "{d:#?}");
    d.targets.into_iter().next().expect("one")
}

/// Discovery, then the protocol with `rules` and no idle requirement.
fn reclaimed_with(host: &ModelHost, rules: &Rules<'_>) -> Reclaimed {
    let target = found(host);
    let mut checker = Checker::new(rules, REUSE_LIMIT);
    reclaim(
        host,
        &target,
        None,
        rules,
        IdleWindow::default(),
        &mut checker,
        None,
    )
}

fn reclaimed(host: &ModelHost) -> Reclaimed {
    reclaimed_with(host, &rules())
}

/// A manifest may itself be a mount point. Both eligibility and reclaim
/// reach the project directory; neither may stat the manifest before
/// consulting mountinfo, even when the target is also the discovery root.
#[test]
fn a_mounted_manifest_is_never_statted_during_eligibility_or_reclaim() {
    let fixture = yaml(CLASSIC, None, "")
        .replace("    Cargo.toml: {}\n", "")
        .replace(
        "  - {id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 1}",
        "  - {id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 1}\n  - {id: 41, at: /tmp/w/p/Cargo.toml, fs: fuse.sshfs, dev: \"0:51\", size: 1G, parent: 30}",
    );
    let manifest = Path::new("/tmp/w/p/Cargo.toml");
    for root in ["/tmp/w", TARGET] {
        let host = host(&fixture);
        let roots = [PathBuf::from(root)];
        let discovery = discover(&host, &roots, None).expect("discover");
        assert_eq!(discovery.targets.len(), 1, "{root}: {discovery:#?}");
        let target = &discovery.targets[0];
        let reached = reach(&host, target, None)
            .expect("reach")
            .expect("reachable");
        assert!(reached.project.detached, "{root}: {reached:#?}");
        let rules = rules();
        let mut checker = Checker::new(&rules, REUSE_LIMIT);
        let eligible = eligibility(
            &host,
            target,
            &rules,
            IdleWindow::default(),
            &mut checker,
            None,
        )
        .expect("eligibility")
        .expect("eligible");
        assert!(eligible.entries > 0, "{root}: {eligible:#?}");
        let done = reclaim(
            &host,
            target,
            None,
            &rules,
            IdleWindow::default(),
            &mut checker,
            None,
        );
        assert_eq!(done.end, ReclaimEnd::Complete, "{root}: {done:#?}");
        let touched: Vec<_> = host
            .trace()
            .into_iter()
            .filter(|call| {
                call.path
                    .as_deref()
                    .is_some_and(|path| path.starts_with(manifest))
            })
            .collect();
        assert!(
            touched.is_empty(),
            "{root}: mounted manifest touched: {touched:#?}"
        );
    }
}

/// Discovery inspects profile entries only one level deep. The shared
/// eligibility walk must preflight a mount further inside an entry before
/// statx, both while judging and again under Cargo's locks.
#[test]
fn a_nested_mount_in_a_profile_entry_is_never_statted() {
    let mounted = Path::new("/tmp/w/p/target/debug/build/dead");
    let fixture = yaml(CLASSIC, None, "").replace(
        "  - {id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 1}",
        "  - {id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 1}\n  - {id: 41, at: /tmp/w/p/target/debug/build/dead, fs: fuse.sshfs, dev: \"0:51\", size: 1G, parent: 30}",
    );
    let host = host(&fixture);
    let target = found(&host);
    let rules = rules();
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    let eligible = eligibility(
        &host,
        &target,
        &rules,
        IdleWindow::default(),
        &mut checker,
        None,
    )
    .expect("eligibility")
    .expect_err("mounted entry must be refused");
    assert_eq!(eligible.reason, Reason::ContainsMount, "{eligible}");
    let done = reclaim(
        &host,
        &target,
        None,
        &rules,
        IdleWindow::default(),
        &mut checker,
        None,
    );
    let touched: Vec<_> = host
        .trace()
        .into_iter()
        .filter(|call| {
            call.path
                .as_deref()
                .is_some_and(|path| path.starts_with(mounted))
        })
        .collect();
    assert!(touched.is_empty(), "nested mount was touched: {touched:#?}");
    assert_skipped(&done, Reason::ContainsMount, "mount point");
}

#[track_caller]
fn assert_skipped(done: &Reclaimed, reason: Reason, needle: &str) {
    match &done.end {
        ReclaimEnd::Skipped(skip) => {
            assert_eq!(skip.reason, reason, "{skip}");
            assert!(
                skip.to_string().contains(needle),
                "{needle:?} not in {skip}"
            );
        }
        other => panic!("expected a {reason} skip, got {other:?}"),
    }
}

/// The paths successfully unlinked, in order.
fn unlinked(host: &ModelHost) -> Vec<PathBuf> {
    host.trace()
        .into_iter()
        .filter(|c| c.op == Op::Unlinkat && c.result == "ok")
        .map(|c| c.path.expect("unlinkat has a path"))
        .collect()
}

/// Every object below `TARGET` with a Cargo lock file name, with its inode.
fn lock_files(host: &ModelHost) -> Vec<(PathBuf, u64)> {
    let mut found = Vec::new();
    for profile in ["debug", "release", "x86_64-unknown-linux-gnu/debug"] {
        for name in CARGO_LOCK_NAMES {
            let path = Path::new(TARGET).join(profile).join(name);
            if let Some(stat) = host.stat_path(&path) {
                found.push((path, stat.ino.expect("inode")));
            }
        }
    }
    found
}

/// The protocol's invariants on any run, whatever its end: no lock file
/// unlinked, nothing unlinked outside the profile directories' entries,
/// no lock held by diskgc afterwards (released by closing).
#[track_caller]
fn assert_invariants(host: &ModelHost, locks_before: &[(PathBuf, u64)]) {
    for path in unlinked(host) {
        assert!(
            !path.file_name().is_some_and(is_cargo_lock_name),
            "a lock file was unlinked: {}",
            path.display()
        );
        let rel = path.strip_prefix(TARGET).expect("inside the target");
        let mut names = rel.components();
        let first = names.next().expect("a name").as_os_str();
        let inside = if first == "x86_64-unknown-linux-gnu" {
            names.next().is_some() && names.next().is_some()
        } else {
            first != "doc" && first != "tmp" && first != "criterion" && names.next().is_some()
        };
        assert!(
            inside,
            "unlinked outside a profile directory's entries: {}",
            path.display()
        );
    }
    for (path, ino) in locks_before {
        let now = host.stat_path(path).and_then(|s| s.ino);
        assert_eq!(now, Some(*ino), "{} gone or replaced", path.display());
        assert!(
            !host
                .locks_on(path)
                .iter()
                .any(|(holder, _)| *holder == Holder::Own),
            "{} still locked by diskgc",
            path.display()
        );
    }
}

fn exists(host: &ModelHost, path: &str) -> bool {
    host.stat_path(Path::new(path)).is_some()
}

/// Whether `path` lies in a fingerprint directory of either layout.
fn is_fingerprint(path: &Path) -> bool {
    let names: Vec<_> = path.components().map(|c| c.as_os_str()).collect();
    names.iter().any(|n| *n == ".fingerprint")
        || names
            .windows(4)
            .any(|w| w[0] == "build" && w[3] == "fingerprint")
}

// ---- The whole protocol ----

/// AC #1, AC #2 and the order of step 3 on the classic layout: every entry
/// of both profile directories (`debug` and a triple's) but the lock files
/// is deleted, the fingerprints of both first, `build/` and `deps/` last
/// in each; every lock was taken (in Cargo's order, each reopened from its
/// `O_PATH` fd) before the first unlink and released afterwards; the lock
/// files keep their inodes; `CACHEDIR.TAG`, `.rustc_info.json`, `doc/`,
/// `tmp/`, `criterion/` and the triple directory are untouched.
#[test]
fn every_entry_but_the_lock_files_goes_fingerprints_first() {
    let host = host(&yaml(CLASSIC, Some(CLASSIC), ""));
    let locks = lock_files(&host);
    assert_eq!(locks.len(), 6);
    let done = reclaimed(&host);
    assert_eq!(done.end, ReclaimEnd::Complete, "{done:?}");
    assert_eq!(done.removed_entries, 32, "16 entries in each profile");
    assert!(done.locks_held.is_some());
    assert_invariants(&host, &locks);
    for kept in [
        "/tmp/w/p/target/CACHEDIR.TAG",
        "/tmp/w/p/target/.rustc_info.json",
        "/tmp/w/p/target/doc/p/index.html",
        "/tmp/w/p/target/tmp/t",
        "/tmp/w/p/target/criterion/base/estimates.json",
        "/tmp/w/p/target/x86_64-unknown-linux-gnu",
        "/tmp/w/p/target/debug",
    ] {
        assert!(exists(&host, kept), "{kept} was deleted");
    }
    for gone in [
        "debug/deps",
        "debug/.fingerprint",
        "x86_64-unknown-linux-gnu/debug/p",
    ] {
        assert!(!exists(&host, &format!("{TARGET}/{gone}")), "{gone} left");
    }

    let order = unlinked(&host);
    let fingerprints = order.iter().take_while(|p| is_fingerprint(p)).count();
    assert_eq!(
        fingerprints, 6,
        "both profiles' .fingerprint first: {order:#?}"
    );
    assert!(order[fingerprints..].iter().all(|p| !is_fingerprint(p)));
    for profile in ["debug", "x86_64-unknown-linux-gnu/debug"] {
        let dir = Path::new(TARGET).join(profile);
        let top: Vec<&std::ffi::OsStr> = order
            .iter()
            .filter(|p| p.parent() == Some(dir.as_path()))
            .map(|p| p.file_name().expect("a name"))
            .collect();
        assert_eq!(
            top,
            [
                ".fingerprint",
                "examples",
                "incremental",
                "p",
                "p.d",
                "build",
                "deps"
            ],
            "{profile}"
        );
    }

    // The locks: six taken, each from a reopened fd, in Cargo's order per
    // profile, all before the first unlink.
    let trace = host.trace();
    let flocks: Vec<_> = trace.iter().filter(|c| c.op == Op::Flock).collect();
    assert_eq!(flocks.len(), 6);
    assert!(flocks.iter().all(|c| c.result == "acquired"));
    let names: Vec<String> = flocks[..3]
        .iter()
        .map(|c| {
            c.path
                .as_ref()
                .expect("a path")
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        names,
        [".cargo-build-lock", ".cargo-lock", ".cargo-artifact-lock"]
    );
    assert_eq!(
        trace.iter().filter(|c| c.op == Op::ReopenRegular).count(),
        6
    );
    let first_unlink = trace
        .iter()
        .position(|c| c.op == Op::Unlinkat)
        .expect("unlinks");
    let last_flock = trace
        .iter()
        .rposition(|c| c.op == Op::Flock)
        .expect("flocks");
    assert!(last_flock < first_unlink);
}

/// A VCS checkout a build script cloned into `build/<pkg>/out/` (`.git`,
/// `.hg`, `.svn`) is reclaimed with the rest of the profile directory:
/// the built-in `cargo-target` says `checkouts: allow` (task-80; these
/// rules take it from the library, no code exempts the Cargo kind).
#[test]
fn a_vcs_checkout_a_build_script_cloned_is_reclaimed_with_the_target() {
    let debug = "{.cargo-lock: {}, .cargo-build-lock: {}, .cargo-artifact-lock: {}, \
        .fingerprint: {p-1: {bin-p: {}}}, build: {p-2: {out: {vendor: {.git: {HEAD: {}}, \
        hg: {.hg: {store: {}}}, svn: {.svn: {wc.db: {}}}}}}}, deps: {p-1: {size: 1M}}, p: {}}";
    let host = host(&yaml(debug, None, ""));
    let locks = lock_files(&host);
    let done = reclaimed(&host);
    assert_eq!(done.end, ReclaimEnd::Complete, "{done:?}");
    assert_invariants(&host, &locks);
    for gone in [".git", "hg/.hg", "svn/.svn"] {
        let path = format!("{TARGET}/debug/build/p-2/out/vendor/{gone}");
        assert!(!exists(&host, &path), "{path} left");
    }
}

/// The per-unit layout's fingerprints are `build/<package>/<hash>/
/// fingerprint`: every one of them goes first, each split off the record
/// of `build/` (deleted below its unit directory), then the other entries,
/// then the rest of `build/` last.
#[test]
fn per_unit_fingerprints_go_first_and_build_last() {
    let host = host(&yaml(PER_UNIT, None, ""));
    let locks = lock_files(&host);
    let done = reclaimed(&host);
    assert_eq!(done.end, ReclaimEnd::Complete, "{done:?}");
    assert_invariants(&host, &locks);
    let order = unlinked(&host);
    let rel: Vec<String> = order
        .iter()
        .map(|p| {
            p.strip_prefix(format!("{TARGET}/debug"))
                .expect("in debug")
                .display()
                .to_string()
        })
        .collect();
    assert_eq!(
        rel[..7],
        [
            "build/dep/d1/fingerprint/lib-dep",
            "build/dep/d1/fingerprint",
            "build/p/b1/fingerprint/run-build",
            "build/p/b1/fingerprint",
            "build/p/c1/fingerprint/bin-p",
            "build/p/c1/fingerprint/dep-bin-p",
            "build/p/c1/fingerprint",
        ],
        "{rel:#?}"
    );
    assert!(rel[7..].iter().all(|p| !is_fingerprint(Path::new(p))));
    assert_eq!(rel.last().map(String::as_str), Some("build"));
    let top: Vec<&str> = rel
        .iter()
        .filter(|p| !p.contains('/'))
        .map(String::as_str)
        .collect();
    assert_eq!(top, ["examples", "incremental", "p", "p.d", "build"]);
    assert_eq!(done.removed_entries, rel.len());
    assert!(exists(&host, "/tmp/w/p/target/debug/.cargo-artifact-lock"));
}

// ---- Step 1: locks ----

/// A lock held by someone else, exclusive (Cargo 1.90's `.cargo-lock`) or
/// shared (1.99.1's `.cargo-lock` during a build), or `.cargo-build-lock`
/// held by a Cargo waiting for `.cargo-lock`: `locked`, nothing deleted,
/// and every lock diskgc took before is released.
#[test]
fn a_lock_held_by_another_process_is_locked() {
    for (lock, mode, flocks) in [
        (".cargo-lock", "exclusive", 2),
        (".cargo-lock", "shared", 2),
        (".cargo-build-lock", "exclusive", 1),
        (".cargo-artifact-lock", "shared", 3),
    ] {
        let rest = format!(
            "processes:\n  - {{pid: 4242, exe: /nix/store/x-cargo/bin/cargo, cwd: /tmp/w/p, \
             maps: [/nix/store/x-cargo/bin/cargo], environ: [A=1], \
                fds: [{{path: /nix/store/x-cargo/bin/cargo}}]}}\nlocks:\n  - {{path: \
             {TARGET}/debug/{lock}, holder: 4242, mode: {mode}}}"
        );
        let host = host(&yaml(CLASSIC, None, &rest));
        let locks = lock_files(&host);
        let done = reclaimed(&host);
        assert_skipped(&done, Reason::Locked, lock);
        assert_eq!(done.removed_entries, 0);
        assert!(unlinked(&host).is_empty(), "{lock} {mode}");
        assert_invariants(&host, &locks);
        let tried: Vec<String> = host
            .trace()
            .iter()
            .filter(|c| c.op == Op::Flock)
            .map(|c| c.result.clone())
            .collect();
        assert_eq!(tried.len(), flocks, "{lock} {mode}: {tried:?}");
        assert_eq!(tried.last().map(String::as_str), Some("busy"));
        assert_eq!(
            host.locks_on(&Path::new(TARGET).join("debug").join(lock)),
            vec![(
                Holder::Process(4242),
                match mode {
                    "shared" => LockMode::Shared,
                    _ => LockMode::Exclusive,
                }
            )]
        );
    }
}

/// A lock file replaced (a new inode at its name) between discovery and
/// its locking is `changed`: the protocol locks only the lock files it
/// found, and never by name.
#[test]
fn a_lock_file_replaced_before_it_is_locked_is_changed() {
    let model = with_events(
        &yaml(CLASSIC, None, ""),
        &[&format!(
            "{{at: {{op: flock, nth: 1, when: after}}, replace: {TARGET}/debug/.cargo-lock}}"
        )],
    );
    let host = host(&model);
    let done = reclaimed(&host);
    assert_skipped(&done, Reason::Changed, "not the lock file found");
    assert!(unlinked(&host).is_empty());
}

// ---- Step 2: changed between discovery and locking (AC #3) ----

/// AC #3: a profile directory created between discovery and locking (a
/// `cargo build --release` started meanwhile), or a new lock file in a
/// locked profile (a newer Cargo's `.cargo-artifact-lock` next to the two
/// diskgc found), is `changed`, seen by the inspection under the locks:
/// nothing deleted, every lock released. The events fire at the first
/// lock, after discovery.
#[test]
fn a_new_profile_or_lock_file_before_the_locks_are_held_is_changed() {
    let two_locks = CLASSIC.replace(".cargo-artifact-lock: {}, ", "");
    for (debug, created) in [
        (CLASSIC.to_owned(), format!("{TARGET}/release/.cargo-lock")),
        (two_locks, format!("{TARGET}/debug/.cargo-artifact-lock")),
    ] {
        let model = with_events(
            &yaml(&debug, None, ""),
            &[&format!(
                "{{at: {{op: flock, nth: 1, when: before}}, create: {created}}}"
            )],
        );
        let host = host(&model);
        let locks = lock_files(&host);
        let done = reclaimed(&host);
        assert_skipped(
            &done,
            Reason::Changed,
            "created, removed or replaced since discovery",
        );
        assert!(unlinked(&host).is_empty(), "{created}");
        assert!(done.locks_held.is_some(), "the locks were taken first");
        assert_invariants(&host, &locks);
        assert!(exists(&host, &created));
    }
}

/// AC #3: a newer build between the caller's judgement (eligibility) and
/// the locks is `changed` (the last build under the locks is newer than
/// the one judged); without the event the same run deletes; without a
/// judged time the idle requirement still refuses it (`not_idle`).
#[test]
fn a_newer_build_before_the_locks_are_held_is_changed() {
    let build = format!(
        "{{at: {{op: flock, nth: 1, when: before}}, create: {TARGET}/debug/deps/p-2.rlib}}"
    );
    for (events, idle_days, judge, want) in [
        (vec![build.as_str()], None, true, Some(Reason::Changed)),
        (vec![], None, true, None),
        (vec![build.as_str()], Some(1), false, Some(Reason::NotIdle)),
    ] {
        let host = host(&with_events(&yaml(CLASSIC, None, ""), &events));
        let target = found(&host);
        let rules = rules();
        let mut checker = Checker::new(&rules, REUSE_LIMIT);
        let idle = idle_days.map(|d| IdleFor(DAY * d));
        let judged = judge.then(|| {
            eligibility(
                &host,
                &target,
                &rules,
                IdleWindow::at_least(idle),
                &mut checker,
                None,
            )
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|skip| panic!("{skip}"))
            .last_build
            .expect("a last build")
        });
        let done = reclaim(
            &host,
            &target,
            judged,
            &rules,
            IdleWindow::at_least(idle),
            &mut checker,
            None,
        );
        match want {
            Some(reason) => {
                assert_skipped(&done, reason, "");
                assert!(unlinked(&host).is_empty());
            }
            None => assert_eq!(done.end, ReclaimEnd::Complete, "{done:?}"),
        }
    }
}

/// The in-use check runs under the locks, against a scan made then: a
/// Cargo working in the project directory (between compiling and running,
/// holding no lock) keeps the target, nothing is deleted.
#[test]
fn a_cargo_in_the_project_directory_keeps_the_target_under_the_locks() {
    let rest = "processes:\n  - {pid: 4242, exe: /nix/store/x-cargo/bin/cargo, cwd: /tmp/w/p, \
                maps: [/nix/store/x-cargo/bin/cargo], environ: [A=1], \
                fds: [{path: /nix/store/x-cargo/bin/cargo}]}";
    let host = host(&yaml(CLASSIC, None, rest));
    let locks = lock_files(&host);
    let done = reclaimed(&host);
    assert_skipped(
        &done,
        Reason::InUse,
        "is a Cargo process working in the project directory",
    );
    assert!(unlinked(&host).is_empty());
    assert_invariants(&host, &locks);
    // The scan came after the last lock.
    let trace = host.trace();
    let last_flock = trace
        .iter()
        .rposition(|c| c.op == Op::Flock)
        .expect("flocks");
    let scan = trace
        .iter()
        .rposition(|c| c.op == Op::Pids)
        .expect("a scan");
    assert!(scan > last_flock);
}

/// The in-use check of step 2 uses a scan made under the locks, never one
/// kept from before them: a process that opens a file of the target
/// between the caller's eligibility check (whose scan the checker would
/// still reuse: the virtual clock has not moved) and the locks keeps the
/// target, nothing deleted.
#[test]
fn a_process_started_between_judging_and_locking_is_seen() {
    let rest = "processes:\n  - {pid: 50, exe: /bin/sh, cwd: /, maps: [/bin/sh], environ: [A=1], \
                fds: [{path: /bin/sh}]}";
    let model = with_events(
        &yaml(CLASSIC, None, rest),
        &[&format!(
            "{{at: {{op: flock, nth: 1, when: before}}, open: {{pid: 50, path: {TARGET}/debug/p}}}}"
        )],
    );
    let host = host(&model);
    let target = found(&host);
    let rules = rules();
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    let judged = eligibility(
        &host,
        &target,
        &rules,
        IdleWindow::default(),
        &mut checker,
        None,
    )
    .unwrap_or_else(|e| panic!("{e}"))
    .unwrap_or_else(|skip| panic!("{skip}"))
    .last_build;
    let done = reclaim(
        &host,
        &target,
        judged,
        &rules,
        IdleWindow::default(),
        &mut checker,
        None,
    );
    assert_skipped(&done, Reason::InUse, "pid 50");
    assert!(unlinked(&host).is_empty());
}

/// One entry budget for the whole target (task-9 review, H4): 14 entries
/// in one profile pass with a budget of 16 and are `too_many_entries` with
/// 15, although every entry alone is far below it.
#[test]
fn the_entry_budget_is_one_for_the_whole_target() {
    for (max, complete) in [(16, true), (15, false)] {
        let host = host(&yaml(CLASSIC, None, ""));
        let rules = Rules {
            max_entries: max,
            ..rules()
        };
        let done = reclaimed_with(&host, &rules);
        if complete {
            assert_eq!(done.end, ReclaimEnd::Complete, "{done:?}");
        } else {
            assert_skipped(
                &done,
                Reason::TooManyEntries,
                "one budget for the whole target",
            );
            assert!(unlinked(&host).is_empty());
        }
    }
}

/// A target diskgc cannot reclaim as found is refused before any lock is
/// taken: nothing but lock files (`empty`), an unknown lock name
/// (`unsupported_layout`: diskgc would not know to take it).
#[test]
fn an_empty_or_unknown_layout_takes_no_lock() {
    for (debug, reason) in [
        ("{.cargo-lock: {}}", Reason::Empty),
        (
            "{.cargo-lock: {}, .cargo-new-lock: {}, deps: {d: {}}}",
            Reason::UnsupportedLayout,
        ),
    ] {
        let host = host(&yaml(debug, None, ""));
        let done = reclaimed(&host);
        assert_skipped(&done, reason, "");
        assert!(host.trace().iter().all(|c| c.op != Op::Flock));
        assert_eq!(done.locks_held, None);
    }
}

/// The checker must be built from the walk's rules (its users are the
/// owners): otherwise an error of the run, nothing locked.
#[test]
fn a_checker_for_other_users_is_an_error_of_the_run() {
    let host = host(&yaml(CLASSIC, None, ""));
    let target = found(&host);
    let other_rules = Rules::new(&[ME, 1001]);
    let mut checker = Checker::new(&other_rules, REUSE_LIMIT);
    let done = reclaim(
        &host,
        &target,
        None,
        &rules(),
        IdleWindow::default(),
        &mut checker,
        None,
    );
    assert!(
        matches!(done.end, ReclaimEnd::Stopped(Stop::Failed { .. })),
        "{done:?}"
    );
    assert!(host.trace().iter().all(|c| c.op != Op::Flock));
}

// ---- Step 3: while deleting ----

/// mped H6 (a): a lock file created while diskgc holds the locks (a 1.99.1
/// build on a target 1.90 built creates `.cargo-build-lock`, then waits
/// for `.cargo-lock`) stops the deletion before the next part (`changed`):
/// only the fingerprints were deleted, the new lock file is left alone.
#[test]
fn a_lock_file_created_while_the_locks_are_held_stops_the_deletion() {
    let only_cargo_lock = CLASSIC
        .replace(".cargo-build-lock: {}, ", "")
        .replace(".cargo-artifact-lock: {}, ", "");
    let model = with_events(
        &yaml(&only_cargo_lock, None, ""),
        &[&format!(
            "{{at: {{op: unlinkat, nth: 1, when: after}}, create: {TARGET}/debug/.cargo-build-lock}}"
        )],
    );
    let host = host(&model);
    let done = reclaimed(&host);
    assert_skipped(&done, Reason::Changed, "while diskgc held the locks");
    assert_eq!(done.removed_entries, 3, "the fingerprints only");
    assert!(unlinked(&host).iter().all(|p| is_fingerprint(p)));
    assert!(exists(&host, &format!("{TARGET}/debug/.cargo-build-lock")));
    assert!(exists(&host, &format!("{TARGET}/debug/deps")));
}

/// Every part is checked right before its delete: a process that opens a
/// file of a later part after the first part was deleted, once the scan
/// that cleared the first one is too old to be reused, stops the deletion
/// (`in_use`) with the fingerprints gone and that file kept.
#[test]
fn a_part_used_after_the_first_deletions_is_kept() {
    let rest = "processes:\n  - {pid: 50, exe: /bin/sh, cwd: /, maps: [/bin/sh], environ: [A=1], \
                fds: [{path: /bin/sh}]}";
    let model = with_events(
        &yaml(CLASSIC, None, rest),
        &[
            &format!(
                "{{at: {{op: unlinkat, nth: 3, when: after}}, open: {{pid: 50, path: {TARGET}/debug/p}}}}"
            ),
            "{at: {op: unlinkat, nth: 3, when: after}, advance: 1s}",
        ],
    );
    let host = host(&model);
    let done = reclaimed(&host);
    assert_skipped(&done, Reason::InUse, "pid 50");
    assert!(exists(&host, &format!("{TARGET}/debug/p")));
    assert!(unlinked(&host).iter().all(|p| !p.ends_with("debug/p")));
    assert!(!exists(&host, &format!("{TARGET}/debug/.fingerprint")));
}

/// Race test 6's model: the deletion stopped at every unlink in turn (the
/// `unlinkat` fails with `EIO`, an error for the target). Whatever the
/// point, no lock file is unlinked, nothing outside the entries is, the
/// locks are released; once anything but a fingerprint is gone, no
/// fingerprint is left (a half-deleted profile always rebuilds); and the
/// profile is still in a known layout (or holds only lock files), so the
/// next run reclaims the rest. On both layouts and a mix of them, with two
/// profile directories each.
#[test]
fn stopped_at_any_unlink_the_profile_rebuilds_and_is_still_recognised() {
    let classic_fingerprints = [".fingerprint", ".fingerprint/p-1", ".fingerprint/p-1/bin-p"];
    let per_unit_fingerprints = [
        "build/dep/d1/fingerprint",
        "build/dep/d1/fingerprint/lib-dep",
        "build/p/b1/fingerprint",
        "build/p/b1/fingerprint/run-build",
        "build/p/c1/fingerprint",
        "build/p/c1/fingerprint/bin-p",
        "build/p/c1/fingerprint/dep-bin-p",
    ];
    // Two profile directories each (debug and a triple's): the fingerprints
    // of both go before anything else of either (in one classic profile,
    // name order alone would put `.fingerprint/` first).
    let profiles = ["debug", "x86_64-unknown-linux-gnu/debug"];
    let mixed_fingerprints = [
        ".fingerprint",
        ".fingerprint/p-1",
        ".fingerprint/p-1/bin-p",
        "build/q/h1/fingerprint",
        "build/q/h1/fingerprint/lib-q",
    ];
    for (layout, debug, entries, fingerprints) in [
        ("classic", CLASSIC, 32, &classic_fingerprints[..]),
        ("per-unit", PER_UNIT, 54, &per_unit_fingerprints[..]),
        ("mixed", MIXED, 38, &mixed_fingerprints[..]),
    ] {
        for k in 1..=entries {
            let model = with_events(
                &yaml(debug, Some(debug), ""),
                &[&format!(
                    "{{at: {{op: unlinkat, nth: {k}, when: before}}, fail: EIO}}"
                )],
            );
            let host = host(&model);
            let locks = lock_files(&host);
            let done = reclaimed(&host);
            assert!(
                matches!(&done.end, ReclaimEnd::Failed { error, .. } if error.is(diskgc_core::host::Errno::EIO)),
                "{layout} at unlinkat {k}: {done:?}"
            );
            assert_eq!(done.removed_entries, k - 1, "{layout} at {k}");
            assert_invariants(&host, &locks);
            let gone = unlinked(&host);
            if gone.iter().any(|p| !is_fingerprint(p)) {
                let left: Vec<String> = profiles
                    .iter()
                    .flat_map(|p| {
                        fingerprints
                            .iter()
                            .map(move |f| format!("{TARGET}/{p}/{f}"))
                    })
                    .filter(|f| exists(&host, f))
                    .collect();
                assert!(
                    left.is_empty(),
                    "{layout} at {k}: fingerprints left after other entries went: {left:?}"
                );
            }
            // The next discovery still knows every profile.
            let again = found(&host);
            for profile in &again.inspected.profiles {
                assert!(
                    profile.is_empty() || profile.layout().is_some(),
                    "{layout} at {k}: a profile is in no known layout: {profile:#?}"
                );
            }
        }
        // Past the last unlink there is no point: the event never fires.
        let host = host(&yaml(debug, Some(debug), ""));
        let done = reclaimed(&host);
        assert_eq!(done.removed_entries, entries, "{layout}: {done:?}");
    }
}

/// The deadline is checked between operations: one that passes during the
/// deletion stops it (an error of the run), the locks released.
#[test]
fn a_deadline_passing_mid_deletion_stops_it() {
    let model = with_events(
        &yaml(CLASSIC, None, ""),
        &["{at: {op: unlinkat, nth: 2, when: after}, advance: 1h}"],
    );
    let host = host(&model);
    let locks = lock_files(&host);
    let target = found(&host);
    let rules = rules();
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    let deadline = host.now().checked_add(Duration::from_secs(60));
    let done = reclaim(
        &host,
        &target,
        None,
        &rules,
        IdleWindow::default(),
        &mut checker,
        deadline,
    );
    assert!(
        matches!(done.end, ReclaimEnd::Stopped(Stop::DeadlinePassed { .. })),
        "{done:?}"
    );
    assert_eq!(done.removed_entries, 2);
    assert_invariants(&host, &locks);
}

/// The pre-release sandbox gate refusing the first unlink is an error of
/// the run (`GateRefused`), never a skip; nothing is deleted.
#[test]
fn a_gate_refusal_is_an_error_of_the_run() {
    let host = Gated(host(&yaml(CLASSIC, None, "")));
    let target = {
        let d = discover(&host, &[PathBuf::from("/tmp/w")], None).unwrap_or_else(|e| panic!("{e}"));
        d.targets.into_iter().next().expect("a target")
    };
    let rules = rules();
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    let done = reclaim(
        &host,
        &target,
        None,
        &rules,
        IdleWindow::default(),
        &mut checker,
        None,
    );
    match &done.end {
        ReclaimEnd::GateRefused { error, .. } => assert_eq!(error.cause, Cause::SandboxGate),
        other => panic!("expected GateRefused, got {other:?}"),
    }
    assert_eq!(done.removed_entries, 0);
}

/// ModelHost behind a sandbox gate that is closed: every deleting call is
/// refused, as `LinuxHost` refuses it outside the test containers.
struct Gated(ModelHost);

impl Host for Gated {
    type Fd = ModelFd;
    type ProcIdentity = <ModelHost as Host>::ProcIdentity;

    fn mountinfo(&self) -> Result<Vec<MountEntry>, HostError> {
        self.0.mountinfo()
    }
    fn openat2(
        &self,
        base: Base<'_, ModelFd>,
        path: &Path,
        how: OpenHow,
    ) -> Result<ModelFd, HostError> {
        self.0.openat2(base, path, how)
    }
    fn statx(&self, base: Base<'_, ModelFd>, path: &Path) -> Result<Stat, HostError> {
        self.0.statx(base, path)
    }
    fn read_link(
        &self,
        dir: &ModelFd,
        name: &std::ffi::OsStr,
    ) -> Result<std::path::PathBuf, HostError> {
        self.0.read_link(dir, name)
    }
    fn read_dir(&self, dir: &ModelFd, limit: usize) -> Result<DirListing, HostError> {
        self.0.read_dir(dir, limit)
    }
    fn listable(&self, dir: &ModelFd) -> Result<(), HostError> {
        self.0.listable(dir)
    }
    fn read_head(&self, file: &ModelFd, limit: usize) -> Result<Vec<u8>, HostError> {
        self.0.read_head(file, limit)
    }
    fn reopen_regular(&self, file: &ModelFd) -> Result<ModelFd, HostError> {
        self.0.reopen_regular(file)
    }
    fn statvfs(&self, fd: &ModelFd) -> Result<Space, HostError> {
        self.0.statvfs(fd)
    }
    fn flock_try(&self, fd: ModelFd, mode: LockMode) -> Result<TryLock<ModelFd>, HostError> {
        self.0.flock_try(fd, mode)
    }
    fn flock_wait(
        &self,
        fd: ModelFd,
        mode: LockMode,
        deadline: Option<MonoTime>,
    ) -> Result<WaitLock<ModelFd>, HostError> {
        self.0.flock_wait(fd, mode, deadline)
    }
    fn now(&self) -> MonoTime {
        self.0.now()
    }
    fn interrupted(&self) -> Option<diskgc_core::report::Signal> {
        self.0.interrupted()
    }
    fn second_signal(&self) -> bool {
        self.0.second_signal()
    }
    fn wall_clock(&self) -> Timestamp {
        self.0.wall_clock()
    }
    fn sleep(&self, duration: Duration) {
        self.0.sleep(duration);
    }
    fn pressure(&self) -> Result<diskgc_core::host::Pressure, HostError> {
        self.0.pressure()
    }
    fn lower_priority(&self) -> Result<(), HostError> {
        self.0.lower_priority()
    }
    fn kernel_release(&self) -> Result<String, HostError> {
        self.0.kernel_release()
    }
    fn hostname(&self) -> Result<String, HostError> {
        self.0.hostname()
    }
    fn pids(&self) -> Result<Vec<Pid>, HostError> {
        self.0.pids()
    }
    fn proc_identity(&self, pid: Pid) -> Result<Self::ProcIdentity, ProcError> {
        self.0.proc_identity(pid)
    }
    fn proc_identity_released(&self, identity: &Self::ProcIdentity) -> Result<bool, ProcError> {
        self.0.proc_identity_released(identity)
    }
    fn proc_fds(&self, pid: Pid) -> Result<Vec<ProcFd>, ProcError> {
        self.0.proc_fds(pid)
    }
    fn proc_links(&self, pid: Pid, which: ProcLink) -> Result<Vec<LinkTarget>, ProcError> {
        self.0.proc_links(pid, which)
    }
    fn proc_maps(&self, pid: Pid) -> Result<Vec<MapEntry>, ProcError> {
        self.0.proc_maps(pid)
    }
    fn proc_environ(&self, pid: Pid) -> Result<Environ, ProcError> {
        self.0.proc_environ(pid)
    }
    fn proc_views(&self, pids: &[Pid], facts: bool) -> Vec<Result<ProcView, ProcError>> {
        self.0.proc_views(pids, facts)
    }
    fn own_pid(&self) -> Pid {
        self.0.own_pid()
    }
    fn effective_uid(&self) -> u32 {
        self.0.effective_uid()
    }
    fn account_uid(&self, name: &str) -> Result<Option<u32>, HostError> {
        self.0.account_uid(name)
    }
    fn inherit_lock(&self, lock: &Locked<ModelFd>) -> Result<(), HostError> {
        self.0.inherit_lock(lock)
    }
    fn proc_status(&self, pid: Pid) -> Result<ProcStatus, ProcError> {
        self.0.proc_status(pid)
    }
    fn bound_unix_sockets(&self) -> Result<Vec<BoundSocket>, HostError> {
        self.0.bound_unix_sockets()
    }

    fn pid_namespace(&self) -> Result<diskgc_core::host::PidNamespace, HostError> {
        self.0.pid_namespace()
    }

    fn proc_nspid(&self, pid: Pid) -> Result<Vec<u32>, diskgc_core::host::ProcError> {
        self.0.proc_nspid(pid)
    }

    fn pid_alive(&self, pid: Pid) -> Result<bool, HostError> {
        self.0.pid_alive(pid)
    }

    fn fs_magic(&self, fd: &Self::Fd) -> Result<u64, HostError> {
        self.0.fs_magic(fd)
    }

    fn proc_uids(&self, pid: Pid) -> Result<diskgc_core::host::Ids, diskgc_core::host::ProcError> {
        self.0.proc_uids(pid)
    }

    fn proc_cmdline(
        &self,
        pid: Pid,
    ) -> Result<Vec<std::ffi::OsString>, diskgc_core::host::ProcError> {
        self.0.proc_cmdline(pid)
    }

    fn proc_root_lookup(
        &self,
        pid: Pid,
        path: &std::path::Path,
        climb: bool,
    ) -> Result<Vec<diskgc_core::host::DevIno>, diskgc_core::host::ProcError> {
        self.0.proc_root_lookup(pid, path, climb)
    }
    fn fs_uuid(&self, mount: &diskgc_core::host::MountEntry) -> Result<Option<String>, HostError> {
        self.0.fs_uuid(mount)
    }

    fn loop_backing_file(
        &self,
        dev: diskgc_core::host::Dev,
    ) -> Result<Option<std::path::PathBuf>, HostError> {
        self.0.loop_backing_file(dev)
    }

    fn loop_backing_files(
        &self,
    ) -> Result<Vec<(diskgc_core::host::Dev, std::path::PathBuf)>, HostError> {
        self.0.loop_backing_files()
    }

    fn loop_backing_identity(
        &self,
        dev: diskgc_core::host::Dev,
    ) -> Result<diskgc_core::host::LoopIdentity, HostError> {
        self.0.loop_backing_identity(dev)
    }

    fn test_container_pid_namespace(&self) -> bool {
        self.0.test_container_pid_namespace()
    }

    fn boot_id(&self) -> Result<String, HostError> {
        self.0.boot_id()
    }

    fn state_lock(
        &self,
        key: &diskgc_core::host::FsKey,
    ) -> Result<diskgc_core::host::StateLock<Self::Fd>, HostError> {
        self.0.state_lock(key)
    }
    fn user_lock_path(&self) -> Option<std::path::PathBuf> {
        self.0.user_lock_path()
    }

    fn user_lock(
        &self,
        make: diskgc_core::host::MakeUserLock,
    ) -> Result<Option<diskgc_core::host::UserLock<Self::Fd>>, diskgc_core::host::UserLockError>
    {
        self.0.user_lock(make)
    }

    fn state_read(
        &self,
        key: &diskgc_core::host::FsKey,
        limit: u64,
    ) -> Result<Option<diskgc_core::host::StateFile>, HostError> {
        self.0.state_read(key, limit)
    }

    fn state_write(
        &self,
        lock: &diskgc_core::host::Locked<Self::Fd>,
        key: &diskgc_core::host::FsKey,
        bytes: &[u8],
        cancel: &dyn Fn() -> bool,
    ) -> Result<diskgc_core::host::StateWritten, HostError> {
        self.0.state_write(lock, key, bytes, cancel)
    }
}

impl Delete for Gated {
    /// Open: this host models a gate that refuses only at the deleting
    /// call itself, which the protocol must still treat as a run error.
    fn may_delete(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn unlinkat(&self, _: &ModelFd, name: &std::ffi::OsStr, _: Unlink) -> Result<(), HostError> {
        Err(HostError::sandbox_gate(
            "unlinkat",
            name.to_string_lossy(),
            "not in a test container",
        ))
    }
    fn fchmod(&self, _: &ModelFd, _: Option<MonoTime>) -> Result<bool, HostError> {
        Err(HostError::sandbox_gate(
            "fchmod",
            "?",
            "not in a test container",
        ))
    }
    fn run_command(
        &self,
        _: &ModelFd,
        _: &diskgc_core::host::CommandRun,
    ) -> diskgc_core::host::CommandInvocation {
        diskgc_core::host::CommandInvocation::NotStarted(HostError::sandbox_gate(
            "run command",
            "?",
            "not in a test container",
        ))
    }
}

// ---- Step 3: one scan serves the parts within Freshness (TASK-173) ----

/// The model's clock as a `now` call reported it (`Duration`'s `Debug`;
/// the model starts at an hour of uptime, so always in seconds).
fn clock_of(result: &str) -> Duration {
    let secs = result
        .strip_suffix('s')
        .filter(|s| !s.ends_with(['m', 'n', 'µ']))
        .unwrap_or_else(|| panic!("not a clock in seconds: {result:?}"));
    Duration::from_secs_f64(secs.parse().unwrap_or_else(|e| panic!("{result:?}: {e}")))
}

/// What happened when, read off the trace: each scan's start (its first
/// clock read, the last one before its `own_pid`) and each part's start
/// (the last clock read before the part's first unlink: `delete_cleared`'s
/// or later, with nothing between that moves the clock: these tests move
/// it only after an `own_pid` or an `unlinkat`, and by retry pauses).
/// Parts are the entries of `debug`, by their first name below it.
struct Timeline {
    /// (trace position, clock) of every scan's start.
    scans: Vec<(usize, Duration)>,
    /// (trace position, part, clock) of every part's first unlink.
    parts: Vec<(usize, String, Duration)>,
}

fn timeline(host: &ModelHost) -> Timeline {
    let (mut scans, mut parts) = (Vec::new(), Vec::<(usize, String, Duration)>::new());
    let mut clock = None;
    for (at, call) in host.trace().into_iter().enumerate() {
        match call.op {
            Op::Now => clock = Some(clock_of(&call.result)),
            Op::OwnPid => scans.push((at, clock.expect("a scan reads the clock first"))),
            Op::Unlinkat if call.result == "ok" => {
                let path = call.path.expect("unlinkat has a path");
                let rel = path
                    .strip_prefix(format!("{TARGET}/debug"))
                    .expect("in debug");
                let part = rel
                    .components()
                    .next()
                    .expect("a name")
                    .as_os_str()
                    .to_string_lossy()
                    .into_owned();
                if parts.last().is_none_or(|(_, last, _)| *last != part) {
                    parts.push((at, part, clock.expect("the delete reads the clock first")));
                }
            }
            _ => {}
        }
    }
    Timeline { scans, parts }
}

/// The classic profile's parts, in deletion order: 16 entries in 7 parts.
const CLASSIC_PARTS: [&str; 7] = [
    ".fingerprint",
    "examples",
    "incremental",
    "p",
    "p.d",
    "build",
    "deps",
];

/// The model with a scan of `scan_ms[k]` for the k-th scan and
/// `unlink_ms[j]` after the j-th unlink (zero: no event).
fn timed(scan_ms: &[u64], unlink_ms: &[u64]) -> ModelHost {
    let mut events = Vec::new();
    for (k, ms) in scan_ms.iter().enumerate().filter(|(_, ms)| **ms > 0) {
        events.push(format!(
            "{{at: {{op: own_pid, nth: {}, when: after}}, advance: {ms}ms}}",
            k + 1
        ));
    }
    for (j, ms) in unlink_ms.iter().enumerate().filter(|(_, ms)| **ms > 0) {
        events.push(format!(
            "{{at: {{op: unlinkat, nth: {}, when: after}}, advance: {ms}ms}}",
            j + 1
        ));
    }
    let events: Vec<&str> = events.iter().map(String::as_str).collect();
    host(&with_events(&yaml(CLASSIC, None, ""), &events))
}

fn sleeps(host: &ModelHost) -> usize {
    host.trace_count(Op::Sleep)
}

/// A 600 ms scan (past the 500 ms reuse window of separate candidates)
/// made under the locks serves every part: all seven start within 1 s of
/// its start, so there is no second scan. Before TASK-173 each part made
/// its own: (parts + 1) scans per target on a busy host.
#[test]
fn one_slow_scan_serves_every_part_that_starts_within_the_bound() {
    let host = timed(&[600], &[]);
    let locks = lock_files(&host);
    let done = reclaimed(&host);
    assert_eq!(done.end, ReclaimEnd::Complete, "{done:?}");
    assert_eq!(done.removed_entries, 16);
    assert_invariants(&host, &locks);
    let t = timeline(&host);
    assert_eq!(t.scans.len(), 1, "one scan for the target");
    let parts: Vec<&str> = t.parts.iter().map(|(_, p, _)| p.as_str()).collect();
    assert_eq!(parts, CLASSIC_PARTS);
    assert_eq!(sleeps(&host), 0, "no attempt spent");
}

/// A part that would start at or past 1 s after the scan's start gets a new
/// scan; the parts before it share the first. The new scan is no retry: no
/// attempt is spent, no pause.
#[test]
fn a_part_that_would_start_past_the_bound_gets_a_new_scan() {
    // The scan ends at 600 ms; `.fingerprint` (3 unlinks) then takes
    // 400 ms: `examples` would start at exactly 1 s.
    let host = timed(&[600], &[0, 0, 400]);
    let done = reclaimed(&host);
    assert_eq!(done.end, ReclaimEnd::Complete, "{done:?}");
    let t = timeline(&host);
    assert_eq!(t.scans.len(), 2, "{:?}", t.scans);
    let (second_scan_at, _) = t.scans[1];
    let (examples_at, ref part, _) = t.parts[1];
    assert_eq!(part, "examples");
    assert!(t.parts[0].0 < second_scan_at && second_scan_at < examples_at);
    assert_eq!(sleeps(&host), 0, "no attempt spent");
}

/// The call numbers of the last two clock reads before `part`'s first
/// unlink in an untimed run of `model`: the checker's verdict check, then
/// `delete_cleared`'s.
fn last_clock_reads_before(model: &str, part: &str) -> (u64, u64) {
    let host = host(model);
    assert_eq!(reclaimed(&host).end, ReclaimEnd::Complete);
    let t = timeline(&host);
    let (first_unlink, _, _) = t
        .parts
        .iter()
        .find(|(_, p, _)| p == part)
        .expect("the part was deleted");
    let trace = host.trace();
    let nows: Vec<u64> = trace[..*first_unlink]
        .iter()
        .filter(|c| c.op == Op::Now)
        .map(|c| c.index)
        .collect();
    let [.., verdict, delete] = nows[..] else {
        panic!("fewer than two clock reads: {nows:?}");
    };
    (verdict, delete)
}

/// A kept scan that reaches the bound while a part is checked, or between
/// its clear and its delete, is the end of its reuse, not a busy host: a
/// new scan, no attempt spent (no pause), nothing deleted on the old one,
/// and the use the new scan sees stops the target.
#[test]
fn a_kept_scan_expiring_at_a_parts_check_is_replaced_without_spending_an_attempt() {
    let rest = "processes:\n  - {pid: 50, exe: /bin/sh, cwd: /, maps: [/bin/sh], \
                environ: [A=1], fds: [{path: /bin/sh}]}";
    let untimed = yaml(CLASSIC, None, rest);
    let (verdict, delete) = last_clock_reads_before(&untimed, "examples");
    for call in [verdict, delete] {
        let model = with_events(
            &untimed,
            &[
                &format!("{{at: {{call: {call}, when: before}}, advance: 1s}}"),
                &format!(
                    "{{at: {{call: {call}, when: before}}, open: {{pid: 50, path: \
                     {TARGET}/debug/examples}}}}"
                ),
            ],
        );
        let host = host(&model);
        let done = reclaimed(&host);
        assert_skipped(&done, Reason::InUse, "pid 50");
        assert!(
            exists(&host, &format!("{TARGET}/debug/examples")),
            "call {call}"
        );
        assert!(!exists(&host, &format!("{TARGET}/debug/.fingerprint")));
        assert_eq!(timeline(&host).scans.len(), 2, "call {call}");
        assert_eq!(sleeps(&host), 0, "call {call}: no attempt spent");
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

    /// Whatever the scans and deletions take: no part starts on a scan
    /// 1 s old or older (Freshness); a target whose scans all take under
    /// 1 s is reclaimed completely without spending an attempt, and makes
    /// a new scan only for a part the kept one could not serve; one with
    /// slower scans ends complete or `in_use_unknown`, never anything
    /// else; and the lock invariants hold.
    #[test]
    fn no_part_starts_on_a_scan_past_the_bound(
        scan_ms in proptest::collection::vec(0_u64..1_300, 1..12),
        unlink_ms in proptest::collection::vec(0_u64..700, 0..16),
    ) {
        let host = timed(&scan_ms, &unlink_ms);
        let locks = lock_files(&host);
        let done = reclaimed(&host);
        assert_invariants(&host, &locks);
        let t = timeline(&host);
        for (at, part, started) in &t.parts {
            let (_, scan) = t.scans.iter().rev().find(|(s, _)| s < at).expect("a scan before");
            proptest::prop_assert!(
                started.saturating_sub(*scan) < Duration::from_secs(1),
                "{} started {:?} after its scan", part, started.saturating_sub(*scan)
            );
        }
        let made = t.scans.len();
        let all_fast = scan_ms.iter().take(made).all(|ms| *ms < 1_000);
        if all_fast {
            proptest::prop_assert_eq!(&done.end, &ReclaimEnd::Complete);
            proptest::prop_assert_eq!(done.removed_entries, 16);
            proptest::prop_assert_eq!(sleeps(&host), 0);
            // A new scan only once the kept one is 1 s old: the model moves
            // no time between the checker's decision to scan and the new
            // scan's first read, so its start is the kept scan's age then.
            for pair in t.scans.windows(2) {
                let (_, previous) = pair[0];
                let (_, next) = pair[1];
                let age = next.saturating_sub(previous);
                proptest::prop_assert!(
                    age >= Duration::from_secs(1),
                    "a new scan though the kept one was {:?} old", age
                );
            }
        } else {
            match &done.end {
                ReclaimEnd::Complete => {}
                ReclaimEnd::Skipped(skip) => {
                    proptest::prop_assert_eq!(skip.reason, Reason::InUseUnknown, "{}", skip);
                }
                other => proptest::prop_assert!(false, "unexpected end {:?}", other),
            }
        }
    }
}
