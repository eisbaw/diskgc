//! The deletion engine (diskgc-core `engine`) on ModelHost: the eligibility
//! walk's rules and the identity-checked delete (docs/safety.md, Filesystem
//! candidates). ModelHost fails a test on any call the safety rules forbid
//! (an `unlinkat` without the identity `statx` right before it, an `openat2`
//! without `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV`), so
//! every test here also checks the call order and flags of everything the
//! engine does.

mod common;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{dir, host};
use diskgc_core::engine::{
    CARGO_LOCK_NAMES, Candidate, Checkouts, DeleteOptions, Deletion, End, FD_WINDOW, IpcKind,
    Reason, Rules, Skip, WalkRecord, WalkStop, delete_unchecked, is_cargo_lock_name, walk,
};
use diskgc_core::host::{Errno, FileType, Host, MonoTime};
use diskgc_core::resolve::Resolved;
use diskgc_model::host::UPTIME_AT_START;
use diskgc_model::spec::Op;
use diskgc_model::{ModelFd, ModelHost};

const ME: u32 = 1000;
const OTHER: u32 = 1001;

/// `/tmp` (mount 30) holding the candidate `/tmp/c` and a bystander
/// `/tmp/keep`; `extra` goes below (more tree keys, or other top-level
/// keys after a line `events:` etc.).
fn model(extra: &str) -> ModelHost {
    host(&format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}}
tree:
  /tmp/keep: {{size: 1K}}
  /tmp/c:
    a: {{size: 10K}}
    b: {{}}
    l: {{symlink: a}}
    sub:
      f: {{size: 4K}}
      deeper: {{g: {{}}}}
{extra}"#
    ))
}

/// A candidate: its parent directory, resolved as the core does.
struct Target {
    parent: Resolved<ModelFd>,
    path: PathBuf,
}

impl Target {
    fn new(host: &ModelHost, path: &str) -> Self {
        let path = PathBuf::from(path);
        let parent = dir(
            host,
            path.parent()
                .expect("has a parent")
                .to_str()
                .expect("UTF-8"),
        );
        Self { parent, path }
    }

    fn candidate(&self) -> Candidate<'_, ModelFd> {
        Candidate::new(&self.parent, &self.path).expect("a name")
    }
}

/// The rules a walk gets without stating anything but its owners
/// ([`Rules::new`]): checkouts refused, by default.
fn rules() -> Rules<'static> {
    Rules::new(&[ME])
}

fn walked(host: &ModelHost, target: &Target, rules: &Rules<'_>) -> WalkRecord {
    walk(host, &target.candidate(), rules, None).unwrap_or_else(|e| panic!("walk: {e:?}"))
}

fn skipped(host: &ModelHost, target: &Target, rules: &Rules<'_>) -> Skip {
    match walk(host, &target.candidate(), rules, None) {
        Err(WalkStop::Skipped(skip)) => skip,
        other => panic!("expected a skip, got {other:?}"),
    }
}

fn deleted(host: &ModelHost, target: &Target, record: &WalkRecord) -> Deletion {
    delete_unchecked(host, &target.candidate(), record, DeleteOptions::default())
}

/// The paths successfully unlinked, in order.
fn unlinked(host: &ModelHost) -> Vec<String> {
    host.trace()
        .into_iter()
        .filter(|c| c.op == Op::Unlinkat && c.result == "ok")
        .map(|c| c.path.expect("unlinkat has a path").display().to_string())
        .collect()
}

fn exists(host: &ModelHost, path: &str) -> bool {
    host.stat_path(Path::new(path)).is_some()
}

/// Walks and deletes `/tmp/c`, expecting a skip; nothing is unlinked.
fn assert_skipped(host: &ModelHost, rules: &Rules<'_>, reason: Reason, at: &str) -> Skip {
    let target = Target::new(host, "/tmp/c");
    let skip = skipped(host, &target, rules);
    assert_eq!(
        (skip.reason, skip.path.as_path()),
        (reason, Path::new(at)),
        "{skip}"
    );
    assert!(
        unlinked(host).is_empty(),
        "a refused walk unlinked something"
    );
    skip
}

// ---- The whole cycle ----

#[test]
fn a_tree_is_walked_then_deleted_depth_first_in_name_order() {
    let host = model("");
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    // c, a, b, l, sub, sub/deeper, sub/deeper/g, sub/f
    assert_eq!(record.entries(), 8);
    assert_eq!(record.root().identity.file_type, FileType::Directory);
    assert_eq!(record.objects().count(), 8);
    let c = host.stat_path(Path::new("/tmp/c")).expect("c").dev_ino();
    assert_eq!(record.objects().next(), c, "the candidate comes first");
    // 3 directories (4 KiB each in the model) and files of 10K and 4K.
    assert_eq!(record.bytes(), 3 * 4096 + 12 * 1024 + 4096);
    assert!(record.newest_change().is_some());

    let done = deleted(&host, &target, &record);
    assert_eq!(done.end, End::Complete, "{done:?}");
    assert_eq!(done.removed_entries, record.entries());
    assert_eq!(done.removed_bytes, record.bytes());
    assert_eq!(
        unlinked(&host),
        [
            "/tmp/c/a",
            "/tmp/c/b",
            "/tmp/c/l",
            "/tmp/c/sub/deeper/g",
            "/tmp/c/sub/deeper",
            "/tmp/c/sub/f",
            "/tmp/c/sub",
            "/tmp/c",
        ]
    );
    assert!(!exists(&host, "/tmp/c"));
    assert!(exists(&host, "/tmp/keep"), "a bystander went");
}

#[test]
fn every_unlink_comes_right_after_the_statx_of_its_entry_and_the_deadline_checks() {
    let host = model("");
    let target = Target::new(&host, "/tmp/c");
    let far = MonoTime(UPTIME_AT_START + Duration::from_secs(3600));
    let record = walk(&host, &target.candidate(), &rules(), Some(far)).expect("eligible");
    let options = DeleteOptions {
        deadline: Some(far),
        ..DeleteOptions::default()
    };
    let done = delete_unchecked(&host, &target.candidate(), &record, options);
    assert_eq!(done.end, End::Complete);
    let trace = host.trace();
    let mut unlinks = 0;
    for (k, call) in trace.iter().enumerate() {
        if call.op != Op::Unlinkat {
            continue;
        }
        unlinks += 1;
        // ModelHost already insists on the statx right before, clock reads
        // aside: the deadline is read before that statx and once more
        // between it and the unlink (the statx can block; task-18 round 2),
        // nothing else.
        assert_eq!(
            trace[k - 1].op,
            Op::Now,
            "no deadline check right before {call}"
        );
        assert_eq!(trace[k - 2].op, Op::Statx, "{call}");
        assert_eq!(trace[k - 2].path, call.path);
        assert_eq!(
            trace[k - 3].op,
            Op::Now,
            "no deadline check before the statx of {call}"
        );
    }
    assert_eq!(unlinks, 8);
}

#[test]
fn a_single_file_candidate_is_checked_and_unlinked() {
    let host = model("");
    let target = Target::new(&host, "/tmp/keep");
    let record = walked(&host, &target, &rules());
    assert_eq!(record.entries(), 1);
    assert_eq!(record.root().identity.file_type, FileType::Regular);
    assert_eq!(deleted(&host, &target, &record).end, End::Complete);
    assert_eq!(unlinked(&host), ["/tmp/keep"]);
    assert!(exists(&host, "/tmp/c/a"));
}

#[test]
fn a_missing_candidate_is_missing() {
    let host = model("");
    let target = Target::new(&host, "/tmp/nothing");
    assert_eq!(skipped(&host, &target, &rules()).reason, Reason::Missing);
}

// ---- Eligibility rules ----

#[test]
fn one_entry_of_another_user_deep_inside_makes_the_candidate_not_owner() {
    let host = model(&format!("  /tmp/c/sub/deeper/theirs: {{uid: {OTHER}}}\n"));
    let skip = assert_skipped(
        &host,
        &rules(),
        Reason::NotOwner,
        "/tmp/c/sub/deeper/theirs",
    );
    assert!(skip.detail.contains("1001"), "{skip}");
    // A directory of another user too, even an empty one.
    let host = model(&format!("  /tmp/c/sub/theirs: {{.: {{uid: {OTHER}}}}}\n"));
    assert_skipped(&host, &rules(), Reason::NotOwner, "/tmp/c/sub/theirs");
    // And the candidate itself.
    let host = model(&format!("  /tmp/theirs: {{.: {{uid: {OTHER}}}}}\n"));
    let target = Target::new(&host, "/tmp/theirs");
    assert_eq!(skipped(&host, &target, &rules()).reason, Reason::NotOwner);
}

#[test]
fn every_listed_owner_is_authorised() {
    let host = model(&format!("  /tmp/c/sub/deeper/theirs: {{uid: {OTHER}}}\n"));
    let target = Target::new(&host, "/tmp/c");
    let both = Rules {
        owners: &[ME, OTHER],
        ..rules()
    };
    let record = walked(&host, &target, &both);
    assert_eq!(deleted(&host, &target, &record).end, End::Complete);
    assert!(!exists(&host, "/tmp/c"));
}

#[test]
fn a_keep_marker_anywhere_inside_keeps_the_candidate() {
    let host = model("  /tmp/c/sub/deeper/.diskgc-keep: {}\n");
    assert_skipped(
        &host,
        &rules(),
        Reason::KeepMarker,
        "/tmp/c/sub/deeper/.diskgc-keep",
    );
    // A directory of that name counts too.
    let host = model("  /tmp/c/.diskgc-keep: {.: {}}\n");
    assert_skipped(&host, &rules(), Reason::KeepMarker, "/tmp/c/.diskgc-keep");
}

#[test]
fn a_cargo_target_inside_is_contains_special() {
    for (layout, lock) in [
        // <t>/<profile>/ and <t>/<triple>/<profile>/, unknown lock names
        // (known ones are refused anywhere, below).
        (
            "  /tmp/c/sub/target/CACHEDIR.TAG: {}\n",
            "/tmp/c/sub/target/debug/.cargo-future-lock",
        ),
        (
            "  /tmp/c/sub/target/CACHEDIR.TAG: {}\n",
            "/tmp/c/sub/target/x86_64-unknown-linux-gnu/release/.cargo-other-lock-v2",
        ),
        // The candidate itself is the target.
        ("  /tmp/c/CACHEDIR.TAG: {}\n", "/tmp/c/sub/.cargo-lock-next"),
    ] {
        let host = model(&format!("{layout}  {lock}: {{}}\n"));
        let skip = assert_skipped(&host, &rules(), Reason::ContainsSpecial, lock);
        assert!(
            skip.detail.contains("an unknown Cargo lock file name"),
            "{skip}"
        );
    }
}

/// No Cargo lock file is ever deleted (PRD): a known lock name anywhere,
/// with or without a Cargo target around it, the candidate itself
/// included, refuses the candidate. A candidate that is a profile
/// directory (`target/debug`) keeps its `.cargo-lock`.
#[test]
fn a_known_cargo_lock_name_anywhere_is_contains_special() {
    // The known names are a subset of the walk's rule.
    for known in CARGO_LOCK_NAMES {
        assert!(is_cargo_lock_name(OsStr::new(known)), "{known}");
    }
    for lock in [
        "/tmp/c/.cargo-lock",
        "/tmp/c/sub/deeper/.cargo-build-lock",
        "/tmp/c/sub/.cargo-artifact-lock",
    ] {
        let host = model(&format!("  {lock}: {{}}\n"));
        let skip = assert_skipped(&host, &rules(), Reason::ContainsSpecial, lock);
        assert!(
            skip.detail.contains("a known Cargo lock file name"),
            "{skip}"
        );
    }
    // The candidate itself is a lock file.
    let host = model("  /tmp/.cargo-lock: {}\n");
    let target = Target::new(&host, "/tmp/.cargo-lock");
    assert_eq!(
        skipped(&host, &target, &rules()).reason,
        Reason::ContainsSpecial
    );
}

/// No plain delete removes a version-control checkout (task-71): an entry
/// named `.git`, `.hg` or `.svn` anywhere, of any type (a `.git` file of a
/// worktree, a symlink: never followed), refuses the candidate as
/// `contains_checkout` at that entry, before anything below it is read;
/// so does a candidate that is itself such an entry. Nothing is unlinked.
#[test]
fn a_checkout_marker_anywhere_is_contains_checkout() {
    for (tree, at) in [
        ("  /tmp/c/.git: {HEAD: {}}\n", "/tmp/c/.git"),
        ("  /tmp/c/sub/deeper/.git: {}\n", "/tmp/c/sub/deeper/.git"),
        (
            "  /tmp/c/sub/.git: {symlink: /tmp/keep}\n",
            "/tmp/c/sub/.git",
        ),
        ("  /tmp/c/sub/.hg: {store: {}}\n", "/tmp/c/sub/.hg"),
        ("  /tmp/c/.svn: {wc.db: {}}\n", "/tmp/c/.svn"),
    ] {
        let host = model(tree);
        let skip = assert_skipped(&host, &rules(), Reason::ContainsCheckout, at);
        assert!(skip.detail.contains("checkout"), "{skip}");
        // The walk stops at the marker: one `statx` of it (to admit it),
        // and no other call on it or below it (no open, no listing).
        let touched: Vec<_> = host
            .trace()
            .into_iter()
            .filter(|c| c.path.as_deref().is_some_and(|p| p.starts_with(at)))
            .collect();
        assert!(
            matches!(touched.as_slice(), [only] if only.op == Op::Statx
                && only.path.as_deref() == Some(Path::new(at))),
            "expected exactly one statx of the marker: {touched:#?}"
        );
    }
    for marker in [".git", ".hg", ".svn"] {
        let host = model(&format!("  /tmp/r/{marker}: {{HEAD: {{}}}}\n"));
        let target = Target::new(&host, &format!("/tmp/r/{marker}"));
        let skip = skipped(&host, &target, &rules());
        assert_eq!(skip.reason, Reason::ContainsCheckout, "{skip}");
    }
}

/// task-80: the checkout refusal is the source profile's setting, carried
/// in [`Rules::checkouts`]. Rules that do not state it ([`Rules::new`])
/// refuse; `allow` admits the same trees, marker included, and the record
/// holds the marker's entries (they are deleted with the rest).
#[test]
fn checkouts_are_refused_unless_the_rules_allow_them() {
    let tree = "  /tmp/c/sub/.git: {HEAD: {}, objects: {.: {}}}\n  /tmp/c/.hg: {store: {}}\n";
    let host = model(tree);
    let target = Target::new(&host, "/tmp/c");
    assert_eq!(
        skipped(&host, &target, &Rules::new(&[ME])).reason,
        Reason::ContainsCheckout
    );
    let allow = Rules {
        checkouts: Checkouts::Allow,
        ..rules()
    };
    let record = walked(&host, &target, &allow);
    for rel in ["sub/.git/HEAD", "sub/.git/objects", ".hg/store"] {
        assert!(
            record.entry_at(Path::new(rel)).is_some(),
            "{rel} not walked"
        );
    }
    // A candidate that is itself a marker, too.
    let marker = Target::new(&host, "/tmp/c/sub/.git");
    assert_eq!(
        skipped(&host, &marker, &rules()).reason,
        Reason::ContainsCheckout
    );
    walked(&host, &marker, &allow);
    // Explicit `refuse` is the default.
    let refuse = Rules {
        checkouts: Checkouts::Refuse,
        ..allow
    };
    assert_eq!(
        skipped(&host, &target, &refuse).reason,
        Reason::ContainsCheckout
    );
    assert_eq!(deleted(&host, &target, &record).end, End::Complete);
    assert!(!exists(&host, "/tmp/c"));
}

/// An unknown `.cargo-*lock*` name refuses the candidate by its name
/// alone, wherever the target's CACHEDIR.TAG is (TASK-167): Cargo may add
/// finer-grained locks below a profile directory's top level, and a
/// candidate that is itself a profile directory (`*/target/debug`), or a
/// unit directory deep inside one, has the tag outside it.
#[test]
fn an_unknown_cargo_lock_name_without_a_target_around_it_is_contains_special() {
    for (tree, lock) in [
        // No CACHEDIR.TAG.
        ("", "/tmp/c/sub/target/debug/.cargo-future-lock"),
        // CACHEDIR.TAG three levels up: not a profile directory.
        (
            "  /tmp/c/sub/target/CACHEDIR.TAG: {}\n",
            "/tmp/c/sub/target/a/b/c/.cargo-future-lock",
        ),
        // The tag outside the candidate: the candidate is a profile
        // directory, or a unit directory of one.
        ("  /tmp/CACHEDIR.TAG: {}\n", "/tmp/c/.cargo-future-lock"),
        ("", "/tmp/c/build/app/abc123/.cargo-unit-lock"),
        ("", "/tmp/c/.fingerprint/app-1/.cargo-lock.bak"),
    ] {
        let host = model(&format!("{tree}  {lock}: {{}}\n"));
        let skip = assert_skipped(&host, &rules(), Reason::ContainsSpecial, lock);
        assert!(
            skip.detail.contains("an unknown Cargo lock file name"),
            "{skip}"
        );
    }
    // A directory of such a name, deep inside: the rule is by name, any
    // type.
    let host = model("  /tmp/c/sub/.cargo-future-lock/x: {}\n");
    assert_skipped(
        &host,
        &rules(),
        Reason::ContainsSpecial,
        "/tmp/c/sub/.cargo-future-lock",
    );
    // The candidate itself has an unknown lock name.
    let host = model("  /tmp/.cargo-future-lock: {}\n");
    let target = Target::new(&host, "/tmp/.cargo-future-lock");
    assert_eq!(
        skipped(&host, &target, &rules()).reason,
        Reason::ContainsSpecial
    );
}

/// Names that only look like Cargo's are plain files: the rule is
/// `.cargo-*lock*`, no wider.
#[test]
fn names_that_are_not_cargo_lock_names_are_plain_files() {
    for name in [
        ".cargo-ok",
        "cargo-lock",
        ".cargo_lock",
        ".cargo-",
        "x.cargo-lock",
    ] {
        let host = model(&format!(
            "  /tmp/c/sub/target/CACHEDIR.TAG: {{}}\n  /tmp/c/sub/target/debug/{name}: {{}}\n"
        ));
        let target = Target::new(&host, "/tmp/c");
        walked(&host, &target, &rules());
    }
}

#[test]
fn any_socket_or_fifo_keeps_the_candidate_by_default() {
    for kind in ["socket", "fifo"] {
        let host = model(&format!("  /tmp/c/sub/deeper/ipc: {{type: {kind}}}\n"));
        assert_skipped(
            &host,
            &rules(),
            Reason::ContainsIpc,
            "/tmp/c/sub/deeper/ipc",
        );
    }
}

/// With `block_ipc` off (the other policy) the walk refuses nothing for a
/// socket or FIFO: it records them, with their paths below the candidate,
/// for the caller's liveness check; the candidate is otherwise eligible.
#[test]
fn without_block_ipc_sockets_and_fifos_are_recorded_for_the_caller() {
    let host = model("  /tmp/c/sub/sock: {type: socket}\n  /tmp/c/sub/deeper/pipe: {type: fifo}\n");
    let target = Target::new(&host, "/tmp/c");
    let open = Rules {
        block_ipc: false,
        ..rules()
    };
    let record = walked(&host, &target, &open);
    let mut found: Vec<(IpcKind, String)> = record
        .ipc_entries()
        .into_iter()
        .map(|e| (e.kind, e.rel.display().to_string()))
        .collect();
    found.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        found,
        [
            (IpcKind::Fifo, "sub/deeper/pipe".to_owned()),
            (IpcKind::Socket, "sub/sock".to_owned())
        ]
    );
    let sock = host
        .stat_path(Path::new("/tmp/c/sub/sock"))
        .and_then(|s| s.dev_ino());
    assert!(record.ipc_entries().iter().any(|e| Some(e.id) == sock));
    // Without any, nothing is recorded; with block_ipc, nothing gets that far.
    let host = model("");
    let target = Target::new(&host, "/tmp/c");
    assert!(walked(&host, &target, &open).ipc_entries().is_empty());
}

#[test]
fn a_mount_inside_or_on_the_candidate_is_contains_mount() {
    let mounted = |at: &str| {
        host(&format!(
            r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}}
  - {{id: 31, at: {at}, fs: tmpfs, dev: "0:43", size: 1M, parent: 30}}
tree:
  /tmp/c/sub/f: {{}}
"#
        ))
    };
    // Refused from mountinfo before statx: a dead mount must not be touched.
    let touched = |host: &ModelHost, path: &str| {
        host.trace().iter().any(|c| {
            matches!(c.op, Op::Openat2 | Op::Statx | Op::ReadDir)
                && c.path.as_deref() == Some(Path::new(path))
        })
    };
    let host = mounted("/tmp/c/sub/m");
    let skip = assert_skipped(&host, &rules(), Reason::ContainsMount, "/tmp/c/sub/m");
    assert!(skip.detail.contains("mountinfo"), "{skip}");
    assert!(!touched(&host, "/tmp/c/sub/m"));
    let host = mounted("/tmp/c");
    assert_skipped(&host, &rules(), Reason::ContainsMount, "/tmp/c");
    assert!(!touched(&host, "/tmp/c"));
}

#[test]
fn a_field_the_kernel_does_not_report_is_unsupported_fs() {
    // (A missing mount id already fails resolving the parent: resolve's
    // own unsupported_fs.)
    for field in ["uid", "ino", "type"] {
        let host = host(&format!(
            r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 30, at: /tmp, fs: odd, dev: "0:42", size: 1G, parent: 1, statx_missing: [{field}]}}
tree:
  /tmp/c/f: {{}}
"#
        ));
        let target = Target::new(&host, "/tmp/c");
        let reason = match walk(&host, &target.candidate(), &rules(), None) {
            Err(WalkStop::Skipped(skip)) => skip.reason,
            other => panic!("{field}: {other:?}"),
        };
        assert_eq!(reason, Reason::UnsupportedFs, "{field}");
    }
}

/// A directory this user may not open or list, or an entry it may not
/// stat (`EACCES`, `EPERM`: a mode-000 directory), refuses the candidate as
/// `unreadable`: never treated as empty.
#[test]
fn a_directory_that_cannot_be_listed_or_opened_is_unreadable_never_empty() {
    for (op, errno, at) in [
        ("read_dir", "EACCES", "/tmp/c/sub/deeper"),
        ("read_dir", "EPERM", "/tmp/c/sub/deeper"),
        ("openat2", "EACCES", "/tmp/c/sub/deeper"),
        ("statx", "EACCES", "/tmp/c/sub/f"),
    ] {
        let host = model(&format!(
            "events:\n  - {{at: {{op: {op}, path: {at}, when: before}}, fail: {errno}}}\n"
        ));
        let skip = assert_skipped(&host, &rules(), Reason::Unreadable, at);
        assert!(skip.detail.contains(errno), "{op} {errno}: {skip}");
        assert!(host.unfired_events().is_empty());
    }
}

/// Errors that say nothing about the candidate and would hit every other
/// one (the fd limit, memory, I/O) fail the walk as an error of the run,
/// never as a skip.
#[test]
fn a_process_level_error_fails_the_walk_instead_of_skipping() {
    for (op, errno, at) in [
        ("openat2", "EMFILE", "/tmp/c/sub/deeper"),
        ("read_dir", "EMFILE", "/tmp/c/sub"),
        ("read_dir", "EIO", "/tmp/c/sub/deeper"),
        ("statx", "ENOMEM", "/tmp/c/sub/f"),
        ("openat2", "ENFILE", "/tmp/c/sub"),
    ] {
        let host = model(&format!(
            "events:\n  - {{at: {{op: {op}, path: {at}, when: before}}, fail: {errno}}}\n"
        ));
        let target = Target::new(&host, "/tmp/c");
        match walk(&host, &target.candidate(), &rules(), None) {
            Err(WalkStop::Failed { path, error }) => {
                assert_eq!(path, Path::new(at), "{op} {errno}");
                assert_eq!(error.kernel_errno().and_then(|e| e.name()), Some(errno));
            }
            other => panic!("{op} {errno}: expected a failure of the run, got {other:?}"),
        }
        assert!(unlinked(&host).is_empty());
    }
}

/// An object that vanishes while the walk runs (a concurrent change) is
/// `changed`, whichever call notices: the statx of a listed name or the
/// listing of an opened directory.
#[test]
fn something_vanishing_mid_walk_is_changed() {
    for (op, at) in [
        ("read_dir", "/tmp/c/sub"),
        ("statx", "/tmp/c/sub/f"),
        ("openat2", "/tmp/c/sub"),
    ] {
        let host = model(&format!(
            "events:\n  - {{at: {{op: {op}, path: {at}, when: before}}, fail: ENOENT}}\n"
        ));
        assert_skipped(&host, &rules(), Reason::Changed, at);
    }
}

#[test]
fn more_entries_than_the_budget_is_too_many_entries() {
    let host = model("");
    let target = Target::new(&host, "/tmp/c");
    // 8 entries: a budget of 8 is enough, 7 is not.
    let enough = Rules {
        max_entries: 8,
        ..rules()
    };
    walked(&host, &target, &enough);
    let short = Rules {
        max_entries: 7,
        ..rules()
    };
    let skip = skipped(&host, &target, &short);
    assert_eq!(skip.reason, Reason::TooManyEntries, "{skip}");
    // The listing that would exceed it stopped (the last one, deeper's).
    let last_listing = host
        .trace()
        .into_iter()
        .rfind(|c| c.op == Op::ReadDir)
        .expect("listed");
    assert_eq!(last_listing.result, "too many");
}

#[test]
fn an_entry_that_changes_during_the_walk_is_changed() {
    // Gone between its listing and its statx.
    let host = model(
        "events:\n  - {at: {op: read_dir, path: /tmp/c/sub, when: after}, remove: /tmp/c/sub/f}\n",
    );
    assert_skipped(&host, &rules(), Reason::Changed, "/tmp/c/sub/f");
    // A directory replaced between its statx and its opening.
    let host = model(
        "events:\n  - {at: {op: openat2, path: /tmp/c/sub, when: before}, replace: /tmp/c/sub}\n",
    );
    assert_skipped(&host, &rules(), Reason::Changed, "/tmp/c/sub");
}

// ---- The delete ----

#[test]
fn an_entry_replaced_between_walk_and_unlink_is_never_deleted_and_the_candidate_is_changed() {
    // After the walk, before the delete reopens the candidate (its second
    // openat2), b is replaced by a new inode.
    let host = model(
        "events:\n  - {at: {op: openat2, path: /tmp/c, nth: 2, when: before}, replace: /tmp/c/b}\n",
    );
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    let done = deleted(&host, &target, &record);
    assert!(host.unfired_events().is_empty());
    let replacement = host
        .stat_path(Path::new("/tmp/c/b"))
        .expect("the replacement is still there")
        .dev_ino();
    let checked = record.objects().nth(2);
    assert!(replacement.is_some() && replacement != checked);
    let End::Changed { path, detail } = &done.end else {
        panic!("expected changed, got {done:?}");
    };
    assert_eq!(path, Path::new("/tmp/c/b"), "{detail}");
    // a went (before b in name order); b's replacement and everything
    // after b stayed: the delete stopped there.
    assert_eq!(unlinked(&host), ["/tmp/c/a"]);
    // (`exists` refuses symlinks: l is checked on the trace above.)
    assert!(exists(&host, "/tmp/c/sub/f"));
    assert_eq!(done.removed_entries, 1);
}

#[test]
fn a_directory_replaced_between_walk_and_delete_is_never_entered() {
    let host = model(
        "events:\n  - {at: {op: unlinkat, path: /tmp/c/l, when: after}, replace: /tmp/c/sub}\n",
    );
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    let done = deleted(&host, &target, &record);
    assert!(
        matches!(&done.end, End::Changed { path, .. } if path == Path::new("/tmp/c/sub")),
        "{done:?}"
    );
    assert_eq!(unlinked(&host), ["/tmp/c/a", "/tmp/c/b", "/tmp/c/l"]);
    assert!(exists(&host, "/tmp/c/sub"));
}

/// The window docs/safety.md accepts: a replacement between the identity
/// `statx` and the `unlinkat` (Linux has no unlink-by-inode) is unlinked in
/// the checked entry's place. Pinned so a change in it is noticed.
#[test]
fn a_replacement_between_the_identity_check_and_the_unlink_is_the_accepted_window() {
    let host = model(
        "events:\n  - {at: {op: unlinkat, path: /tmp/c/b, when: before}, replace: /tmp/c/b}\n",
    );
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    let done = deleted(&host, &target, &record);
    assert_eq!(done.end, End::Complete);
    let trace = host.trace();
    let unlink = trace
        .iter()
        .position(|c| c.op == Op::Unlinkat && c.path.as_deref() == Some(Path::new("/tmp/c/b")))
        .expect("b unlinked");
    assert_ne!(
        trace[unlink].object,
        trace[unlink - 1].object,
        "the unlinked object is not the one the identity check saw"
    );
}

#[test]
fn something_created_after_the_walk_is_kept_and_its_directory_reports_enotempty() {
    let host = model(
        "events:\n  - {at: {op: openat2, path: /tmp/c, nth: 2, when: before}, create: /tmp/c/sub/new}\n",
    );
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    let done = deleted(&host, &target, &record);
    let End::Failed { path, error } = &done.end else {
        panic!("expected an error, got {done:?}");
    };
    assert_eq!(path, Path::new("/tmp/c/sub"));
    assert!(error.is(Errno::ENOTEMPTY), "{error}");
    assert!(
        exists(&host, "/tmp/c/sub/new"),
        "an unchecked entry was deleted"
    );
    assert!(!exists(&host, "/tmp/c/sub/f"));
}

#[test]
fn a_mount_appearing_mid_delete_stops_it_there_as_changed() {
    // The delete's opening of sub (its second openat2) meets a mount:
    // RESOLVE_NO_XDEV refuses with EXDEV.
    let host = model(
        "events:\n  - {at: {op: openat2, path: /tmp/c/sub, nth: 2, when: before}, fail: EXDEV}\n",
    );
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    let done = deleted(&host, &target, &record);
    let End::Changed { path, detail } = &done.end else {
        panic!("expected changed, got {done:?}");
    };
    assert_eq!(path, Path::new("/tmp/c/sub"));
    assert!(detail.contains("EXDEV"), "{detail}");
    assert_eq!(unlinked(&host), ["/tmp/c/a", "/tmp/c/b", "/tmp/c/l"]);
    assert!(exists(&host, "/tmp/c/sub/deeper/g"));
}

#[test]
fn an_unlink_the_host_refuses_is_an_error_and_stops_the_delete() {
    let host =
        model("events:\n  - {at: {op: unlinkat, path: /tmp/c/b, when: before}, fail: EACCES}\n");
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    let done = deleted(&host, &target, &record);
    assert!(
        matches!(&done.end, End::Failed { path, error } if path == Path::new("/tmp/c/b") && error.is(Errno::EACCES)),
        "{done:?}"
    );
    assert_eq!(unlinked(&host), ["/tmp/c/a"]);
}

#[test]
fn a_candidate_renamed_mid_delete_is_still_the_object_deleted() {
    let host = model(
        "events:\n  - {at: {op: unlinkat, path: /tmp/c/a, when: after}, rename: {from: /tmp/c, to: /tmp/moved}}\n",
    );
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    let done = deleted(&host, &target, &record);
    // Everything inside went through the held fd; the candidate's own name
    // is gone from its parent, so its last unlink finds it changed.
    assert!(
        matches!(&done.end, End::Changed { path, .. } if path == Path::new("/tmp/c")),
        "{done:?}"
    );
    assert_eq!(done.removed_entries, record.entries() - 1);
    assert!(
        exists(&host, "/tmp/moved"),
        "the renamed candidate itself stays"
    );
    assert!(!exists(&host, "/tmp/moved/sub"));
    assert!(exists(&host, "/tmp/keep"));
}

#[test]
fn an_entry_renamed_out_after_the_walk_is_changed_and_kept() {
    let host = model(
        "events:\n  - {at: {op: openat2, path: /tmp/c, nth: 2, when: before}, rename: {from: /tmp/c/b, to: /tmp/escaped}}\n",
    );
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    let done = deleted(&host, &target, &record);
    assert!(host.unfired_events().is_empty());
    assert!(
        matches!(&done.end, End::Changed { path, detail } if path == Path::new("/tmp/c/b") && detail.contains("gone")),
        "{done:?}"
    );
    assert_eq!(unlinked(&host), ["/tmp/c/a"]);
    assert!(exists(&host, "/tmp/escaped"), "moved entry was deleted");
    assert!(exists(&host, "/tmp/c/sub"), "later entry was deleted");
}

#[test]
fn make_writable_adds_owner_write_to_read_only_directories_after_the_checks() {
    let tree = "  /tmp/c/sub/deeper: {.: {mode: \"0555\"}}\n";
    let host = model(tree);
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    let options = DeleteOptions {
        make_writable: true,
        deadline: Some(MonoTime(UPTIME_AT_START + Duration::from_secs(3600))),
    };
    let done = delete_unchecked(&host, &target.candidate(), &record, options);
    assert_eq!(done.end, End::Complete);
    let trace = host.trace();
    let chmods: Vec<(String, String)> = trace
        .iter()
        .filter(|c| c.op == Op::Fchmod)
        .map(|c| {
            (
                c.path.as_ref().expect("path").display().to_string(),
                c.result.clone(),
            )
        })
        .collect();
    assert_eq!(
        chmods,
        [("/tmp/c/sub/deeper".to_owned(), "0755".to_owned())]
    );
    // After the directory was opened for the delete and before anything in
    // it was unlinked.
    let chmod = trace
        .iter()
        .position(|c| c.op == Op::Fchmod)
        .expect("chmod");
    let first_unlink_inside = trace
        .iter()
        .position(|c| {
            c.op == Op::Unlinkat && c.path.as_deref() == Some(Path::new("/tmp/c/sub/deeper/g"))
        })
        .expect("g unlinked");
    assert!(chmod < first_unlink_inside);
    assert_eq!(trace[chmod - 1].op, Op::Now, "deadline check before fchmod");
    assert_eq!(trace[chmod - 3].op, Op::Openat2, "{}", trace[chmod - 3]);

    // Without it, permissions are never touched.
    let host = model(tree);
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    deleted(&host, &target, &record);
    assert!(host.trace().iter().all(|c| c.op != Op::Fchmod));
}

#[test]
fn the_deadline_stops_the_walk_and_the_delete_before_the_next_step() {
    let host = model("");
    let target = Target::new(&host, "/tmp/c");
    let now = MonoTime(UPTIME_AT_START);
    assert!(matches!(
        walk(&host, &target.candidate(), &rules(), Some(now)),
        Err(WalkStop::DeadlinePassed { .. })
    ));
    assert!(host.trace().iter().all(|c| c.op == Op::Now
        || c.op == Op::Mountinfo
        || c.op == Op::Openat2
        || c.op == Op::Statx));

    // The clock jumps past the deadline after the second unlink.
    let host =
        model("events:\n  - {at: {op: unlinkat, path: /tmp/c/b, when: after}, advance: 10s}\n");
    let target = Target::new(&host, "/tmp/c");
    let deadline = MonoTime(UPTIME_AT_START + Duration::from_secs(5));
    let record = walk(&host, &target.candidate(), &rules(), Some(deadline)).expect("eligible");
    let options = DeleteOptions {
        deadline: Some(deadline),
        ..DeleteOptions::default()
    };
    let done = delete_unchecked(&host, &target.candidate(), &record, options);
    assert_eq!(
        done.end,
        End::DeadlinePassed {
            path: PathBuf::from("/tmp/c/l")
        }
    );
    assert_eq!(unlinked(&host), ["/tmp/c/a", "/tmp/c/b"]);
    assert_eq!(done.removed_entries, 2);
}

#[test]
fn a_deep_tree_is_handled_with_a_bounded_number_of_open_fds() {
    const DEPTH: usize = 60;
    let chain: String = (0..DEPTH).map(|_| "d/").collect();
    let host = model(&format!("  /tmp/c/{chain}f: {{}}\n  /tmp/c/d/side: {{}}\n"));
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    // The base tree's 8 entries, the chain and its 2 files.
    assert_eq!(record.entries(), 8 + DEPTH + 2);
    let done = deleted(&host, &target, &record);
    assert_eq!(done.end, End::Complete, "{done:?}");
    assert!(!exists(&host, "/tmp/c"));
    // The window, the candidate, its parent (the test's fd) and one
    // transient open: never one fd per level (an absolute bound too, so a
    // larger window cannot pass by moving the bound with it).
    let peak = host.peak_open_fds();
    // (DEPTH / 2 is well above the window, so this bound is a real one.)
    assert!(
        peak < DEPTH / 2,
        "{peak} fds open at once for a {DEPTH}-deep tree"
    );
    // The window's fds are closed before the next one opens: the window,
    // the candidate and its parent (the test's fd), nothing more.
    assert!(
        peak <= FD_WINDOW + 2,
        "{peak} fds open at once: over the window"
    );
    // Climbing back past the window reopened directories by name, each
    // checked like the first time (no `..`).
    let opens = host.trace().iter().filter(|c| c.op == Op::Openat2).count();
    assert!(
        opens > 2 * (DEPTH + 1),
        "{opens} opens: nothing was reopened"
    );
}

// ---- Identity, errno and deadline details (task-5 review) ----

/// The owner is part of the identity: an entry chowned between the walk and
/// its unlink is not the recorded one (same inode, same type).
#[test]
fn an_entry_chowned_between_walk_and_unlink_is_changed() {
    let host = model(&format!(
        "events:\n  - {{at: {{op: openat2, path: /tmp/c, nth: 2, when: before}}, chown: {{path: /tmp/c/b, uid: {OTHER}}}}}\n"
    ));
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    let done = deleted(&host, &target, &record);
    assert!(host.unfired_events().is_empty());
    assert!(
        matches!(&done.end, End::Changed { path, .. } if path == Path::new("/tmp/c/b")),
        "{done:?}"
    );
    assert_eq!(unlinked(&host), ["/tmp/c/a"]);
    assert!(exists(&host, "/tmp/c/b"));
}

/// A chown of the candidate after the walk is caught when its directory is
/// reopened, before any child can be unlinked.
#[test]
fn a_candidate_chowned_before_delete_reopen_is_changed_without_unlinks() {
    let host = model(&format!(
        "events:\n  - {{at: {{op: openat2, path: /tmp/c, nth: 2, when: before}}, chown: {{path: /tmp/c, uid: {OTHER}}}}}\n"
    ));
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    let done = deleted(&host, &target, &record);
    assert!(host.unfired_events().is_empty());
    assert!(
        matches!(&done.end, End::Changed { path, .. } if path == Path::new("/tmp/c")),
        "{done:?}"
    );
    assert!(unlinked(&host).is_empty());
    assert!(exists(&host, "/tmp/c/a"));
}

/// Simulate inode reuse after unlink: dev, inode, owner and type all remain
/// equal. Birth time must stop deletion when reported; a filesystem that
/// omits it has the documented residual risk.
#[test]
fn reused_inode_is_stopped_by_birth_time_when_reported() {
    for missing in [false, true] {
        let missing_field = if missing {
            ", statx_missing: [btime]"
        } else {
            ""
        };
        let host = host(&format!(
            r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1{missing_field}}}
tree:
  /tmp/c: {{b: {{}}}}
events:
  - {{at: {{op: openat2, path: /tmp/c, nth: 2, when: before}}, replace_reusing_inode: /tmp/c/b}}
"#
        ));
        let target = Target::new(&host, "/tmp/c");
        let record = walked(&host, &target, &rules());
        let before = record.entry_at(Path::new("b")).expect("b").identity;
        host.sleep(Duration::from_secs(1));
        let done = deleted(&host, &target, &record);
        assert!(host.unfired_events().is_empty());
        let after = host
            .stat_path(Path::new("/tmp/c/b"))
            .map(|stat| diskgc_core::engine::Identity::of(&stat).expect("identity"));
        if missing {
            assert_eq!(done.end, End::Complete, "{done:?}");
            assert!(
                after.is_none(),
                "reuse is indistinguishable without birth time"
            );
        } else {
            assert!(
                matches!(&done.end, End::Changed { path, .. } if path == Path::new("/tmp/c/b")),
                "{done:?}"
            );
            let after = after.expect("replacement kept");
            assert_eq!(before.id, after.id, "inode number was reused");
            assert_eq!(before.uid, after.uid);
            assert_eq!(before.file_type, after.file_type);
            assert_ne!(before.btime, after.btime);
            assert!(unlinked(&host).is_empty());
        }
    }
}

/// The same birth-time check applies when reopening a recorded directory,
/// before making it writable or unlinking anything below it.
#[test]
fn reused_directory_inode_is_stopped_at_reopen() {
    let host = model(
        "events:\n  - {at: {op: openat2, path: /tmp/c, nth: 2, when: before}, replace_reusing_inode: /tmp/c}\n",
    );
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    let before = record.root().identity;
    host.sleep(Duration::from_secs(1));
    let done = deleted(&host, &target, &record);
    assert!(host.unfired_events().is_empty());
    let after = diskgc_core::engine::Identity::of(
        &host
            .stat_path(Path::new("/tmp/c"))
            .expect("replacement kept"),
    )
    .expect("identity");
    assert_eq!(before.id, after.id, "directory inode number was reused");
    assert_ne!(before.btime, after.btime);
    assert!(
        matches!(&done.end, End::Changed { path, .. } if path == Path::new("/tmp/c")),
        "{done:?}"
    );
    assert!(unlinked(&host).is_empty());
}

/// An entry gone, mounted on, or of another type when the unlink runs
/// (after its identity check: the accepted window) is `changed`, never
/// counted as removed, and the delete stops.
#[test]
fn an_unlink_that_finds_the_entry_gone_or_swapped_is_changed() {
    let gone =
        "events:\n  - {at: {op: unlinkat, path: /tmp/c/b, when: before}, remove: /tmp/c/b}\n";
    let mut events = vec![gone.to_owned()];
    for errno in ["EBUSY", "EISDIR", "ENOTDIR"] {
        events.push(format!(
            "events:\n  - {{at: {{op: unlinkat, path: /tmp/c/b, when: before}}, fail: {errno}}}\n"
        ));
    }
    for extra in events {
        let host = model(&extra);
        let target = Target::new(&host, "/tmp/c");
        let record = walked(&host, &target, &rules());
        let done = deleted(&host, &target, &record);
        assert!(
            matches!(&done.end, End::Changed { path, detail } if path == Path::new("/tmp/c/b") && detail.contains("after its check")),
            "{extra}: {done:?}"
        );
        assert_eq!(done.removed_entries, 1, "{extra}: only a counts as removed");
        assert_eq!(unlinked(&host), ["/tmp/c/a"], "{extra}");
    }
}

/// Every directory reopened on the way back up is matched against the
/// record, not only the one wanted: a directory above the fd window,
/// swapped for another (the recorded one moved away, its subtree moved
/// into the newcomer) after the delete passed it, stops the delete when the
/// chain is reopened through it.
#[test]
fn an_ancestor_swapped_above_the_window_stops_the_delete_when_reopened() {
    const DEPTH: usize = 30;
    let d = |n: usize| format!("/tmp/k{}", "/d".repeat(n));
    let mut tree = String::new();
    for level in 0..=DEPTH {
        tree.push_str(&format!("  {}/f: {{}}\n", d(level)));
    }
    let events = format!(
        "events:\n\
         \x20 - {{at: {{op: unlinkat, path: {bottom}/f, when: after}}, rename: {{from: {five}, to: /tmp/x}}}}\n\
         \x20 - {{at: {{op: unlinkat, path: {bottom}/f, when: after}}, mkdir: {five}}}\n\
         \x20 - {{at: {{op: unlinkat, path: {bottom}/f, when: after}}, rename: {{from: /tmp/x/d, to: {six}}}}}\n",
        bottom = d(DEPTH),
        five = d(5),
        six = d(6),
    );
    let host = model(&format!("{tree}{events}"));
    let target = Target::new(&host, "/tmp/k");
    let record = walked(&host, &target, &rules());
    let done = deleted(&host, &target, &record);
    assert!(host.unfired_events().is_empty());
    assert!(
        matches!(&done.end, End::Changed { path, .. } if *path == Path::new(&d(5))),
        "{done:?}"
    );
    // The recorded level 6, now inside the newcomer, was never reached
    // through it.
    assert!(exists(&host, &d(6)), "deleted through a swapped ancestor");
}

/// A mount met when the walk opens a directory (`EXDEV` from
/// `RESOLVE_NO_XDEV`, e.g. mounted between its statx and its opening) is
/// `contains_mount`.
#[test]
fn a_mount_met_opening_a_directory_in_the_walk_is_contains_mount() {
    let host =
        model("events:\n  - {at: {op: openat2, path: /tmp/c/sub, when: before}, fail: EXDEV}\n");
    let skip = assert_skipped(&host, &rules(), Reason::ContainsMount, "/tmp/c/sub");
    assert!(skip.detail.contains("EXDEV"), "{skip}");
}

/// Device nodes are never deleted.
#[test]
fn a_device_node_inside_is_contains_special() {
    let host = model("  /tmp/c/sub/deeper/tty: {type: char_device}\n");
    let skip = assert_skipped(
        &host,
        &rules(),
        Reason::ContainsSpecial,
        "/tmp/c/sub/deeper/tty",
    );
    assert!(skip.detail.contains("CharDevice"), "{skip}");
}

/// A delete whose deadline has already passed touches nothing: no open, no
/// permission change, no unlink.
#[test]
fn an_expired_delete_touches_nothing() {
    let host = model("  /tmp/c/sub/deeper: {.: {mode: \"0555\"}}\n");
    let target = Target::new(&host, "/tmp/c");
    let record = walked(&host, &target, &rules());
    let walked_calls = host.trace().len();
    let options = DeleteOptions {
        make_writable: true,
        deadline: Some(MonoTime(UPTIME_AT_START)),
    };
    let done = delete_unchecked(&host, &target.candidate(), &record, options);
    assert_eq!(
        done.end,
        End::DeadlinePassed {
            path: PathBuf::from("/tmp/c")
        }
    );
    assert_eq!(done.removed_entries, 0);
    let after: Vec<Op> = host.trace()[walked_calls..].iter().map(|c| c.op).collect();
    assert_eq!(after, [Op::Now], "only the clock was read");
}

/// Reopening a chain of directories checks the deadline before each open,
/// so a deadline that passes mid-chain stops it there.
#[test]
fn reopening_a_long_chain_stops_at_the_deadline() {
    const DEPTH: usize = 30;
    let d = |n: usize| format!("/tmp/k{}", "/d".repeat(n));
    let mut tree = String::new();
    for level in 0..=DEPTH {
        tree.push_str(&format!("  {}/f: {{}}\n", d(level)));
    }
    // The second opening of level 3 is in the chain reopened from the
    // candidate when the delete climbs back past the window.
    let host = model(&format!(
        "{tree}events:\n  - {{at: {{op: openat2, path: {}, nth: 2, when: after}}, advance: 1h}}\n",
        d(3)
    ));
    let target = Target::new(&host, "/tmp/k");
    let deadline = MonoTime(UPTIME_AT_START + Duration::from_secs(60));
    let record = walk(&host, &target.candidate(), &rules(), Some(deadline)).expect("eligible");
    let options = DeleteOptions {
        deadline: Some(deadline),
        ..DeleteOptions::default()
    };
    let done = delete_unchecked(&host, &target.candidate(), &record, options);
    assert!(host.unfired_events().is_empty());
    assert!(matches!(done.end, End::DeadlinePassed { .. }), "{done:?}");
    let trace = host.trace();
    let jump = trace
        .iter()
        .position(|c| {
            c.op == Op::Openat2
                && c.path.as_deref() == Some(Path::new(&d(3)))
                && c.index > 1
                && trace
                    .iter()
                    .filter(|o| o.op == Op::Openat2 && o.path == c.path && o.index <= c.index)
                    .count()
                    == 2
        })
        .expect("the reopening of level 3");
    let opened_after: Vec<String> = trace[jump + 1..]
        .iter()
        .filter(|c| c.op == Op::Openat2)
        .map(|c| c.path.as_ref().expect("path").display().to_string())
        .collect();
    assert!(
        opened_after.is_empty(),
        "opened after the deadline: {opened_after:?}"
    );
}

/// `split_off` takes a part of a record out as a record of its own: the
/// part is deleted first (below its parent, reached and matched by the
/// caller), then the rest, which finds the part's directory as the walk
/// left it minus the part, so neither delete meets anything it did not
/// record and nothing is unlinked twice. Counts move with the part; the
/// record is unchanged for a name it does not hold or for the candidate
/// itself.
#[test]
fn a_split_off_part_and_the_rest_are_deleted_on_their_own() {
    let host = model("");
    let target = Target::new(&host, "/tmp/c");
    let mut rest = walked(&host, &target, &rules());
    let (all_entries, all_bytes) = (rest.entries(), rest.bytes());
    assert!(rest.split_off(Path::new("")).is_none());
    assert!(rest.split_off(Path::new("sub/none")).is_none());
    assert!(rest.split_off(Path::new("a/x")).is_none(), "a is a file");
    assert_eq!(rest.entries(), all_entries);

    let mut part = rest.split_off(Path::new("sub/deeper")).expect("recorded");
    assert_eq!(part.root().name, "deeper");
    assert_eq!(part.entries(), 2, "deeper and g");
    assert_eq!(rest.entries() + part.entries(), all_entries);
    assert_eq!(rest.bytes() + part.bytes(), all_bytes);
    assert!(rest.entry_at(Path::new("sub/deeper")).is_none());
    assert!(rest.entry_at(Path::new("sub/f")).is_some());
    assert!(part.entry_at(Path::new("g")).is_some());
    assert_eq!(
        rest.listing_at(Path::new("sub")).map(|l| l.len()),
        Some(1),
        "only f is left in sub"
    );
    // Nothing of the part is left among the objects the rest's in-use
    // check matches.
    let g = part.entry_at(Path::new("g")).expect("g").identity.id;
    assert!(rest.objects().all(|id| id != g));
    // A part of the part.
    assert!(part.split_off(Path::new("g")).is_some());
    assert_eq!(part.entries(), 1);
    let _ = part;

    let mut rest = walked(&host, &target, &rules());
    let part = rest.split_off(Path::new("sub/deeper")).expect("recorded");
    let sub = Target::new(&host, "/tmp/c/sub/deeper");
    let done = delete_unchecked(&host, &sub.candidate(), &part, DeleteOptions::default());
    assert_eq!((done.end, done.removed_entries), (End::Complete, 2));
    assert_eq!(
        unlinked(&host),
        ["/tmp/c/sub/deeper/g", "/tmp/c/sub/deeper"]
    );
    let done = deleted(&host, &target, &rest);
    assert_eq!(done.end, End::Complete);
    assert_eq!(done.removed_entries, all_entries - 2);
    assert!(!exists(&host, "/tmp/c") && exists(&host, "/tmp/keep"));
    let all = unlinked(&host);
    let mut unique = all.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), all.len(), "an entry unlinked twice: {all:?}");
}
