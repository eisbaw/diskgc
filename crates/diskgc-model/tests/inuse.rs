//! The in-use check (diskgc-core `inuse`; docs/safety.md, In use) and the
//! deletion step around it (`delete_unused`: lock, walk, check, delete) on
//! ModelHost. Every rule has a scenario here that fails when the rule is
//! removed; the real-kernel side is in diskgc-e2e (`inuse_suite`).

mod common;

use std::path::{Path, PathBuf};

use common::{dir, host};
use diskgc_core::engine::{Candidate, DeleteOptions, Deletion, End, Reason, Rules, Skip, walk};
use diskgc_core::host::{Dev, Host, Pid};
use diskgc_core::inuse::{
    self, Checker, FreshAttempts, HeldByDeleted, MAX_LISTINGS, Options, REUSE_LIMIT, Scan,
};
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;
use diskgc_model::support::{Stop, delete_unused};

const ME: u32 = 1000;

/// `/tmp` (tmpfs, `0:42`) holding the candidates `/tmp/c` and `/tmp/d` and
/// a bystander file `/tmp/keep`; a tool and a library under `/usr`.
/// `extra` goes below the tree keys (more tree keys indented by two, or
/// top-level keys).
fn model(extra: &str) -> ModelHost {
    host(&yaml(extra))
}

fn yaml(extra: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}}
tree:
  /usr/bin/tool: {{}}
  /usr/lib/libx.so: {{}}
  /tmp/keep: {{size: 1K}}
  /tmp/gone: {{size: 8K}}
  /tmp/c:
    a: {{size: 10K}}
    sub:
      f: {{size: 4K}}
  /tmp/d:
    x: {{}}
{extra}"#
    )
}

/// A readable process as a YAML list item: by default it runs
/// `/usr/bin/tool` (also mapped), has `PATH=/usr/bin` and `/tmp/keep`
/// open, and uses nothing of the candidates. `with` replaces or adds keys
/// (`"cwd: /tmp/c"`, `"fds: [{path: /tmp/c/a}]"`).
fn process(pid: u32, with: &[&str]) -> String {
    let mut keys: Vec<(String, String)> = [
        ("exe", "/usr/bin/tool"),
        ("maps", "[/usr/bin/tool]"),
        ("environ", "[PATH=/usr/bin]"),
        ("fds", "[{path: /tmp/keep}]"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
    .collect();
    for w in with {
        let (k, v) = w.split_once(": ").expect("key: value");
        match keys.iter_mut().find(|(key, _)| key == k) {
            Some(slot) => slot.1 = v.to_owned(),
            None => keys.push((k.to_owned(), v.to_owned())),
        }
    }
    let body: Vec<String> = keys.iter().map(|(k, v)| format!("{k}: {v}")).collect();
    format!("  - {{pid: {pid}, {}}}\n", body.join(", "))
}

fn processes(list: &[String]) -> String {
    format!("processes:\n{}", list.concat())
}

fn rules(block_ipc: bool) -> Rules<'static> {
    Rules {
        block_ipc,
        ..Rules::new(&[ME])
    }
}

fn options(sockets: bool) -> Options<'static> {
    Options {
        users: &[ME],
        sockets,
        pid_lookup: false,
        pytest_facts: false,
    }
}

/// Walks `/tmp/c`, scans and checks it (no lock, no delete).
fn check_c(host: &ModelHost, block_ipc: bool) -> (Result<(), Skip>, Scan) {
    let parent = dir(host, "/tmp");
    let path = PathBuf::from("/tmp/c");
    let candidate = Candidate::new(&parent, &path).expect("a name");
    let record = walk(host, &candidate, &rules(block_ipc), None).expect("walk");
    let scan = inuse::scan(host, &options(!block_ipc)).expect("scan");
    (scan.check(&candidate, &record), scan)
}

/// The whole deletion step for `path` with `checker`.
fn reclaim_with(
    host: &ModelHost,
    checker: &mut Checker<'_>,
    path: &str,
    block_ipc: bool,
) -> Result<Deletion, Stop> {
    let parent = dir(host, "/tmp");
    let path = PathBuf::from(path);
    let candidate = Candidate::new(&parent, &path).expect("a name");
    delete_unused(
        host,
        checker,
        &candidate,
        &rules(block_ipc),
        DeleteOptions::default(),
    )
}

fn reclaim(host: &ModelHost, path: &str, block_ipc: bool) -> Result<Deletion, Stop> {
    let mut checker = Checker::new(&rules(block_ipc), REUSE_LIMIT);
    reclaim_with(host, &mut checker, path, block_ipc)
}

fn exists(host: &ModelHost, path: &str) -> bool {
    host.stat_path(Path::new(path)).is_some()
}

fn in_use(result: Result<(), Skip>) -> Skip {
    let skip = result.expect_err("expected in_use");
    assert_eq!(skip.reason, Reason::InUse, "{skip}");
    skip
}

fn skip_of(result: Result<Deletion, Stop>) -> Skip {
    match result {
        Err(Stop::Skipped(skip)) => skip,
        other => panic!("expected a skip, got {other:?}"),
    }
}

fn complete(result: Result<Deletion, Stop>) {
    match result {
        Ok(Deletion {
            end: End::Complete, ..
        }) => {}
        other => panic!("expected a complete deletion, got {other:?}"),
    }
}

// ---- Uses by (device, inode) ----

/// An open fd, the working directory, the root, the executable or a
/// mapping on any object of the candidate makes it `in_use`, named with
/// the process and the object; the same processes using nothing of it
/// leave it clear, and the deletion step then deletes it.
#[test]
fn a_process_using_an_object_of_the_candidate_makes_it_in_use() {
    let cases: [(&[&str], &str, &str); 5] = [
        (
            &["fds: [{path: /tmp/c/sub/f}]"],
            "/tmp/c/sub/f",
            "open as fd 3",
        ),
        (
            &["cwd: /tmp/c/sub"],
            "/tmp/c/sub",
            "as its working directory",
        ),
        (&["root: /tmp/c"], "/tmp/c", "as its root directory"),
        (
            &["exe: /tmp/c/a", "maps: [/usr/lib/libx.so]"],
            "/tmp/c/a",
            "as its executable",
        ),
        (&["maps: [/usr/bin/tool, /tmp/c/a]"], "/tmp/c/a", "mapped"),
    ];
    for (with, at, how) in cases {
        let host = model(&processes(&[process(7, &[]), process(10, with)]));
        let skip = in_use(check_c(&host, true).0);
        assert_eq!(skip.path, Path::new(at), "{with:?}: {skip}");
        assert!(
            skip.detail.starts_with("pid 10 (") && skip.detail.contains(how),
            "{with:?}: {skip}"
        );
        let blocker = &skip.process_blockers.as_ref().expect("attributed")[0];
        assert_eq!(blocker.pid, 10);
        assert!(
            blocker
                .command
                .as_deref()
                .is_some_and(|name| !name.is_empty())
        );
        assert_eq!(
            blocker.evidence.as_str(),
            match how {
                "open as fd 3" => "fd",
                "as its working directory" => "cwd",
                "as its root directory" => "root",
                "as its executable" => "exe",
                "mapped" => "map",
                _ => unreachable!(),
            }
        );
        // The deletion step refuses it and unlinks nothing.
        let skip = skip_of(reclaim(&host, "/tmp/c", true));
        assert_eq!(skip.reason, Reason::InUse, "{skip}");
        assert!(exists(&host, "/tmp/c/a"));
    }
    // The control: nobody uses the candidate.
    let host = model(&processes(&[process(7, &[]), process(10, &["cwd: /tmp"])]));
    assert_eq!(check_c(&host, true).0, Ok(()));
    complete(reclaim(&host, "/tmp/c", true));
    assert!(!exists(&host, "/tmp/c") && exists(&host, "/tmp/keep"));
}

/// On overlayfs (and btrfs) a memory map shows the device of the layer,
/// not the one `statx` shows; there a mapping matches by inode number
/// alone. Modelled with a second filesystem whose file has the same inode
/// number as the candidate's file: on an overlay candidate that is a
/// match, on a tmpfs candidate it is not.
#[test]
fn a_mapping_on_overlay_matches_by_inode_number_alone() {
    let build = |fs: &str| {
        host(&format!(
            r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 30, at: /tmp, fs: {fs}, dev: "0:50", size: 1G, parent: 1}}
  - {{id: 31, at: /layer, fs: tmpfs, dev: "0:51", size: 1G, parent: 1}}
tree:
  /usr/bin/tool: {{}}
  /tmp/c:
    lib.so: {{}}
  /layer/c:
    lib.so: {{}}
{}"#,
            processes(&[process(
                10,
                &[
                    "maps: [/usr/bin/tool, /layer/c/lib.so]",
                    "fds: [{path: /usr/bin/tool}]"
                ]
            )])
        ))
    };
    for (fs, expect_in_use) in [("overlay", true), ("btrfs", true), ("tmpfs", false)] {
        let host = build(fs);
        let a = host.stat_path(Path::new("/tmp/c/lib.so")).expect("stat");
        let b = host.stat_path(Path::new("/layer/c/lib.so")).expect("stat");
        assert_eq!(a.ino, b.ino, "the fixture needs equal inode numbers");
        let (result, _) = check_c(&host, true);
        assert_eq!(result.is_err(), expect_in_use, "{fs}: {result:?}");
        if let Err(skip) = result {
            assert!(skip.detail.contains("inode number"), "{skip}");
        }
    }
}

// ---- The initial environment ----

/// A variable of the initial environment naming the candidate or a path
/// inside it makes it `in_use` (also as one component of a `:` list); a
/// sibling with the candidate's name as prefix or a relative path does
/// not.
#[test]
fn the_initial_environment_names_a_path_inside_the_candidate() {
    for (env, expect) in [
        ("[TMPDIR=/tmp/c]", Some("TMPDIR=/tmp/c")),
        (
            "[CARGO_TARGET_DIR=/tmp/c/sub/x]",
            Some("CARGO_TARGET_DIR=/tmp/c/sub/x"),
        ),
        ("['PATH=/usr/bin:/tmp/c/bin']", Some("PATH=/tmp/c/bin")),
        ("[TMPDIR=/tmp/x/../c]", Some("TMPDIR=/tmp/x/../c")),
        ("[TMPDIR=/tmp/cc]", None),
        ("[TMPDIR=c/sub]", None),
        ("[HOME=/tmp]", None),
    ] {
        let host = model(&processes(&[process(10, &[&format!("environ: {env}")])]));
        let (result, _) = check_c(&host, true);
        match expect {
            Some(shown) => {
                let skip = in_use(result);
                assert!(
                    skip.detail.contains("initial environment") && skip.detail.contains(shown),
                    "{env}: {skip}"
                );
                let blocker = &skip.process_blockers.as_ref().expect("attributed")[0];
                assert_eq!(
                    (blocker.pid, blocker.evidence.as_str()),
                    (10, "environment")
                );
            }
            None => assert_eq!(result, Ok(()), "{env}"),
        }
    }
}

/// An environment that cannot be read (empty, as after `env -i`, which
/// LinuxHost reports as `Failed`) is no evidence for the heuristic alone:
/// the process's other uses still count, and the candidate is clear when
/// it has none. It is noted, and read again before that.
#[test]
fn a_failed_environment_alone_is_no_evidence() {
    let host = model(&processes(&[process(10, &["environ: []"])]));
    let (result, scan) = check_c(&host, true);
    assert_eq!(result, Ok(()));
    assert!(
        scan.notes()
            .iter()
            .any(|n| n.starts_with("pid 10 (tool): initial environment unreadable")),
        "{:?}",
        scan.notes()
    );
    let reads = host
        .trace()
        .iter()
        .filter(|c| c.op == Op::ProcEnviron)
        .count();
    assert_eq!(reads, 1 + inuse::REREAD_PAUSES.len(), "read again first");
    // Its other uses still count.
    let host = model(&processes(&[process(10, &["environ: []", "cwd: /tmp/c"])]));
    in_use(check_c(&host, true).0);
}

// ---- Files held after deletion ----

/// A file deleted while a process holds it open is listed per process on
/// its filesystem, with its allocated bytes; other filesystems and files
/// still linked are not.
#[test]
fn a_deleted_but_open_file_is_reported_as_held_by_deleted_files() {
    let host = model(&processes(&[
        process(
            10,
            &["fds: [{path: /tmp/gone, deleted: true}, {path: /tmp/keep}]"],
        ),
        process(11, &[]),
    ]));
    let (_, scan) = check_c(&host, true);
    let tmp = Dev {
        major: 0,
        minor: 42,
    };
    assert_eq!(
        scan.held_by_deleted_files(tmp),
        vec![HeldByDeleted {
            pid: Pid(10),
            command: "tool".into(),
            bytes: 8192,
        }]
    );
    let root = Dev {
        major: 254,
        minor: 1,
    };
    assert_eq!(scan.held_by_deleted_files(root), vec![]);
}

/// Two processes can hold the same unlinked inode. Both are actionable
/// holders, but its allocated blocks must appear only once in the report.
#[test]
fn a_deleted_file_shared_by_processes_is_counted_once() {
    let host = model(&processes(&[
        process(10, &["fds: [{path: /tmp/gone, deleted: true}]"]),
        process(11, &["fds: [{path: /tmp/gone, deleted: true}]"]),
    ]));
    let (_, scan) = check_c(&host, true);
    assert_eq!(
        scan.held_by_deleted_files(Dev {
            major: 0,
            minor: 42,
        }),
        vec![
            HeldByDeleted {
                pid: Pid(10),
                command: "tool".into(),
                bytes: 8192,
            },
            HeldByDeleted {
                pid: Pid(11),
                command: "tool".into(),
                bytes: 0,
            },
        ]
    );
}

// ---- diskgc itself ----

/// diskgc holds the candidate directory open and locked while it checks
/// it; its own pid is listed like any other but never counted, or every
/// candidate would be in use by diskgc itself.
#[test]
fn diskgcs_own_fds_never_make_a_candidate_in_use() {
    let host = model(&processes(&[process(10, &[])]));
    complete(reclaim(&host, "/tmp/c", true));
    assert!(!exists(&host, "/tmp/c"));
    let own = host.own_pid();
    let trace = host.trace();
    assert!(
        trace.iter().any(|c| c.op == Op::Pids && c.result == "ok"),
        "the scan listed the processes"
    );
    assert!(
        !trace.iter().any(|c| c.op == Op::ProcFds
            && c.path.as_deref() == Some(Path::new(&format!("/proc/{own}/fd")))),
        "diskgc read its own fds"
    );
}

// ---- Processes that cannot be read ----

/// Which unreadable processes block every candidate (docs/safety.md, In
/// use: unreadable processes; user decision 2026-10-17): a non-dumpable
/// process of the user, a setuid process and one holding capabilities
/// diskgc lacks are noted and do not; another user's process never does;
/// a dumpable process of the user that is still refused (another group),
/// and one that fails for any other reason, do.
#[test]
fn unreadable_processes_block_unless_their_status_explains_them() {
    let cases: [(&[&str], bool, &str); 9] = [
        (
            &["proc: denied", "non_dumpable: true"],
            false,
            "non-dumpable",
        ),
        (&["proc: denied", "euid: 0"], false, "setuid"),
        (&["proc: denied", "privileged: true"], false, "capabilities"),
        (&["proc: denied", "uid: 1001"], false, "other users"),
        // A Failed read excuses nothing, another user's process included.
        (&["proc: failed", "uid: 1001"], true, "cannot be inspected"),
        // More capable, but in a user namespace of its own (`unshare -r`):
        // the kernel lets diskgc read it there, so a refusal is something
        // else and blocks.
        (
            &["proc: denied", "privileged: true", "uid_map: '0 1000 1'"],
            true,
            "cannot be inspected",
        ),
        (&["proc: denied", "gid: 1001"], true, "cannot be inspected"),
        (&["proc: failed"], true, "cannot be inspected"),
        (
            &["proc: failed", "non_dumpable: true"],
            true,
            "cannot be inspected",
        ),
    ];
    for (with, blocks, noted) in cases {
        let host = model(&processes(&[process(10, with), process(11, &[])]));
        let (result, scan) = check_c(&host, true);
        let notes = scan.notes().join("\n");
        assert!(notes.contains(noted), "{with:?}: {notes}");
        if blocks {
            let skip = in_use(result);
            assert!(
                skip.detail.starts_with("pid 10 (tool) cannot be inspected"),
                "{with:?}: {skip}"
            );
            let blocker = &skip.process_blockers.as_ref().expect("attributed")[0];
            assert_eq!((blocker.pid, blocker.evidence.as_str()), (10, "unreadable"));
        } else {
            assert_eq!(result, Ok(()), "{with:?}: {notes}");
        }
    }
}

/// A root process may hold a file of a configured non-system owner. A denied
/// process with any root UID blocks, including setuid and capable processes.
#[test]
fn root_mode_does_not_excuse_an_unreadable_root_process() {
    for (name, with) in [
        ("root", vec!["uid: 0", "proc: denied"]),
        ("setuid root", vec!["uid: 0", "euid: 2000", "proc: denied"]),
        (
            "capable root",
            vec!["uid: 0", "privileged: true", "proc: denied"],
        ),
    ] {
        let host = model(&format!("{}own_uid: 0\n", processes(&[process(10, &with)])));
        let (result, scan) = check_c(&host, true);
        let skip = in_use(result);
        assert!(skip.detail.contains("pid 10"), "{name}: {skip}");
        assert!(
            scan.notes().iter().all(|note| !note.contains("excused")),
            "{name}: {:?}",
            scan.notes()
        );
    }
}

/// Every refused process of the user is read again before it is excused,
/// the setuid and more capable ones too (an exec makes any process look
/// that way for a moment); only another user's process is not.
#[test]
fn refused_processes_of_the_user_are_read_again_before_they_are_excused() {
    let reads = |with: &[&str]| {
        let host = model(&processes(&[process(10, with)]));
        let (result, _) = check_c(&host, true);
        assert_eq!(result, Ok(()), "{with:?}");
        host.trace()
            .iter()
            .filter(|c| c.op == Op::ProcFds && c.path.as_deref() == Some(Path::new("/proc/10/fd")))
            .count()
    };
    let all = 1 + inuse::REREAD_PAUSES.len();
    assert_eq!(reads(&["proc: denied", "euid: 0"]), all, "setuid");
    assert_eq!(reads(&["proc: denied", "privileged: true"]), all, "capable");
    assert_eq!(
        reads(&["proc: denied", "non_dumpable: true"]),
        all,
        "non-dumpable"
    );
    assert_eq!(reads(&["proc: denied", "uid: 1001"]), 1, "another user's");
}

/// The handoff during re-reads (task-6 review, B1): pid 10 holds the
/// candidate's file and is read again because its environment cannot be
/// read; during the first pause it forks 11, which inherits the fd, and
/// closes its own. The re-read finds nothing on 10, but the use its first
/// read found still counts, and 11, born before that re-read, is listed
/// and read: `in_use`.
#[test]
fn a_child_forked_while_its_parent_is_read_again_is_found() {
    let host = model(&format!(
        "{}events:
  - {{at: {{op: sleep, nth: 1, when: after}}, fork: {{pid: 10, child: 11}}}}
  - {{at: {{op: sleep, nth: 1, when: after}}, close: {{pid: 10, path: /tmp/c/a}}}}
",
        processes(&[process(
            10,
            &["environ: []", "fds: [{path: /tmp/keep}, {path: /tmp/c/a}]"]
        )])
    ));
    let (result, scan) = check_c(&host, true);
    let skip = in_use(result);
    assert!(skip.detail.contains("has it open"), "{skip}");
    let pids = host.trace().iter().filter(|c| c.op == Op::Pids).count();
    assert!(pids >= 3, "listed again after the re-reads: {pids}");
    // The union alone would keep it in use; the child is read too.
    assert!(
        host.trace()
            .iter()
            .any(|c| c.op == Op::ProcFds && c.path.as_deref() == Some(Path::new("/proc/11/fd"))),
        "the child was never read; notes: {:?}",
        scan.notes()
    );
}

/// The status read that finds a process gone also comes before a listing
/// (task-6 review, B1): pid 10 cannot be read; just before its status is
/// read it forks 11 and exits. 10 is gone, but 11 was born before that
/// read, so the next listing shows it and it is read (it cannot be read
/// either, so it blocks: `in_use`).
#[test]
fn a_child_forked_before_its_parent_was_found_gone_is_read() {
    let host = model(&format!(
        "{}events:
  - {{at: {{op: proc_status, path: /proc/10/status, when: before}}, fork: {{pid: 10, child: 11}}}}
  - {{at: {{op: proc_status, path: /proc/10/status, when: before}}, exit: 10}}
",
        processes(&[process(10, &["proc: failed"])])
    ));
    let (result, scan) = check_c(&host, true);
    let skip = in_use(result);
    assert!(skip.detail.starts_with("pid 11 "), "{skip}");
    assert_eq!(scan.gone(), 1);
}

/// Every read only adds (task-6 review, B1): a use a first read found
/// still counts when a re-read no longer shows it. Here pid 10, read again
/// for its unreadable environment, hands the candidate's file over to pid
/// 12, already read, during the pause (an fd passed over a socket, here an
/// open and a close): without the first read's evidence the file would look
/// unused while 12 holds it.
#[test]
fn a_use_found_by_an_earlier_read_still_counts() {
    let host = model(&format!(
        "{}events:
  - {{at: {{op: sleep, nth: 1, when: after}}, open: {{pid: 12, path: /tmp/c/a}}}}
  - {{at: {{op: sleep, nth: 1, when: after}}, close: {{pid: 10, path: /tmp/c/a}}}}
",
        processes(&[
            process(
                10,
                &["environ: []", "fds: [{path: /tmp/keep}, {path: /tmp/c/a}]"]
            ),
            process(12, &[]),
        ])
    ));
    let skip = in_use(check_c(&host, true).0);
    assert!(skip.detail.starts_with("pid 10 "), "{skip}");
}

/// The options of the in-use check must be those of the walk: a user list
/// narrower than the owners would excuse an owner's processes as another
/// user's. `Checker::serves` refuses them (here through the test support's
/// `delete_unused`, which the `fs` and Cargo protocols share the check
/// with).
#[test]
fn the_check_refuses_options_that_disagree_with_the_walk() {
    let host = model(&processes(&[process(10, &[])]));
    let other_rules = Rules::new(&[]);
    let mut checker = Checker::new(&other_rules, REUSE_LIMIT);
    match reclaim_with(&host, &mut checker, "/tmp/c", true) {
        Err(Stop::Failed(e)) => assert!(e.detail.contains("from_rules"), "{e}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(exists(&host, "/tmp/c/a"));
}

/// C1 (task-6 review round 2): an excuse must come out of every status
/// read of the process. A non-dumpable refused process is excused when all
/// four reads agree; one read showing it dumpable (as a read during an
/// exec's tid swap can show a false non-dumpable owner once) and it blocks.
#[test]
fn an_excuse_must_hold_on_every_status_read() {
    let non_dumpable = || process(10, &["proc: denied", "non_dumpable: true"]);
    let host = model(&processes(&[non_dumpable()]));
    assert_eq!(check_c(&host, true).0, Ok(()));
    let host = model(&format!(
        "{}events:
  - {{at: {{op: proc_status, path: /proc/10/status, nth: 2, when: before}}, dumpable: {{pid: 10, value: true}}}}
  - {{at: {{op: proc_status, path: /proc/10/status, nth: 2, when: after}}, dumpable: {{pid: 10, value: false}}}}
",
        processes(&[non_dumpable()])
    ));
    let skip = in_use(check_c(&host, true).0);
    assert!(skip.detail.starts_with("pid 10 "), "{skip}");
}

/// C2: equal `uid_map` text is no proof of one user namespace: two sibling
/// namespaces can both show `1000 1000 1`. The capability excuse applies
/// only when diskgc's map and the process's are the full identity map.
#[test]
fn the_capability_excuse_needs_the_initial_user_namespace() {
    let capable = |map: &str| {
        process(
            10,
            &[
                "proc: denied",
                "privileged: true",
                &format!("uid_map: '{map}'"),
            ],
        )
    };
    let init = "0 0 4294967295";
    let sibling = "1000 1000 1";
    for (own, target, excused) in [
        (init, init, true),
        (sibling, sibling, false),
        (sibling, init, false),
        ("none", init, false),
    ] {
        let host = model(&format!(
            "own_uid_map: '{own}'\n{}",
            processes(&[capable(target)])
        ));
        let (result, scan) = check_c(&host, true);
        assert_eq!(
            result.is_ok(),
            excused,
            "own {own}, target {target}: {:?}",
            scan.notes()
        );
    }
}

/// mped should-fix 1: a kernel without user namespaces shows no `uid_map`;
/// that only removes the capability excuse, every other rule works.
#[test]
fn a_missing_uid_map_only_removes_the_capability_excuse() {
    let host = model(&format!(
        "own_uid_map: none\n{}",
        processes(&[
            process(10, &["proc: denied", "non_dumpable: true", "uid_map: none"]),
            process(11, &["proc: denied", "euid: 0", "uid_map: none"]),
            process(12, &["uid_map: none", "cwd: /tmp"]),
        ])
    ));
    assert_eq!(check_c(&host, true).0, Ok(()));
}

/// Re-reads keep their own spacing (mped should-fix 2): a process due
/// again 2, 8 and then 30 ms after its reads is not read sooner because
/// other processes appear meanwhile.
#[test]
fn each_process_is_read_again_on_its_own_schedule() {
    let forks: String = (1..=4)
        .map(|k| {
            format!(
                "  - {{at: {{op: proc_fds, path: /proc/10/fd, nth: {k}, when: after}}, fork: {{pid: 20, child: {}}}}}\n",
                20 + k
            )
        })
        .collect();
    let host = model(&format!(
        "{}events:\n{forks}",
        processes(&[process(10, &["proc: failed"]), process(20, &[])])
    ));
    let (_, _) = check_c(&host, true);
    // The times at which 10 was read, from the clock calls around them.
    let trace = host.trace();
    let mut at = std::time::Duration::ZERO;
    let mut reads = Vec::new();
    for c in &trace {
        if c.op == Op::Now {
            at = parse_duration(&c.result);
        }
        if c.op == Op::ProcFds && c.path.as_deref() == Some(Path::new("/proc/10/fd")) {
            reads.push(at);
        }
    }
    assert_eq!(reads.len(), 4, "{reads:?}");
    let gaps: Vec<u128> = reads
        .windows(2)
        .map(|w| (w[1] - w[0]).as_millis())
        .collect();
    for (gap, pause) in gaps.iter().zip(inuse::REREAD_PAUSES) {
        assert!(
            *gap >= pause.as_millis(),
            "re-read after {gap} ms, due after {pause:?}: {gaps:?}"
        );
    }
}

fn parse_duration(debug: &str) -> std::time::Duration {
    // `Duration`'s Debug, e.g. `3600.002s` or `3600s`.
    let secs: f64 = debug.trim_end_matches('s').parse().expect("seconds");
    std::time::Duration::from_secs_f64(secs)
}

/// The listing bound counts only listings that bring new pids (mped round
/// 3, should-fix 1): while a non-dumpable process is read again on its own
/// schedule, a process list that grows in 15 consecutive listings still
/// settles; one that grows in [`MAX_LISTINGS`] does not.
#[test]
fn only_listings_with_new_pids_count_against_the_bound() {
    let run = |k: u32| {
        let forks: String = (1..=k)
            .map(|n| {
                format!(
                    "  - {{at: {{op: pids, nth: {n}, when: before}}, fork: {{pid: 20, child: {}}}}}\n",
                    100 + n
                )
            })
            .collect();
        let host = model(&format!(
            "{}events:\n{forks}",
            processes(&[
                process(10, &["proc: denied", "non_dumpable: true"]),
                process(20, &[])
            ])
        ));
        check_c(&host, true)
    };
    let max = u32::try_from(MAX_LISTINGS).expect("small");
    for k in [1, max - 3, max - 1] {
        let (result, scan) = run(k);
        assert_eq!(result, Ok(()), "k={k}: {:?}", scan.notes());
    }
    let (result, _) = run(max);
    assert!(in_use(result).detail.contains("still growing"));
}

/// A clear borrows the exact candidate and record that were checked. Two
/// hard-linked names have the same root identity, but clearing one can only
/// delete that name through the checked capability.
#[test]
fn a_clear_for_one_candidate_deletes_only_that_candidate() {
    let host = model(&format!(
        "  /tmp/alias: {{link: /tmp/keep}}\n{}",
        processes(&[process(10, &["fds: [{path: /usr/bin/tool}]"])])
    ));
    let parent = dir(&host, "/tmp");
    let (c, d) = (PathBuf::from("/tmp/keep"), PathBuf::from("/tmp/alias"));
    let (cc, dc) = (
        Candidate::new(&parent, &c).expect("a name"),
        Candidate::new(&parent, &d).expect("a name"),
    );
    let rules = rules(true);
    let c_record = walk(&host, &cc, &rules, None).expect("walk c");
    let d_record = walk(&host, &dc, &rules, None).expect("walk d");
    assert_eq!(c_record.root().identity, d_record.root().identity);
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    let mismatch = checker
        .check(&host, &dc, &c_record, &mut FreshAttempts::new(None))
        .expect_err("a hard-linked alias is a different candidate name");
    assert!(
        mismatch.detail.contains("another candidate name"),
        "{mismatch}"
    );
    let clear = checker
        .check(&host, &cc, &c_record, &mut FreshAttempts::new(None))
        .expect("check")
        .expect("c is clear");
    let deleted = inuse::delete_cleared(&host, clear, DeleteOptions::default())
        .expect("the checked candidate is deleted");
    assert_eq!(deleted.end, End::Complete);
    assert!(exists(&host, "/tmp/alias"));
    assert!(!exists(&host, "/tmp/keep"));
}

/// Refusals are counted on their own (Codex round 3, non-blocking): a
/// process whose first reads failed only for its environment and that is
/// refused from its fourth read on still gets four status reads, and so
/// four chances to show it has no excuse, before it is classified.
#[test]
fn a_late_refusal_gets_every_classification() {
    let host = model(&format!(
        "{}events:
  - {{at: {{op: sleep, nth: 3, when: before}}, access: {{pid: 10, proc: denied}}}}
  - {{at: {{op: sleep, nth: 3, when: before}}, dumpable: {{pid: 10, value: false}}}}
",
        processes(&[process(10, &["environ: []"])])
    ));
    let (result, scan) = check_c(&host, true);
    assert_eq!(result, Ok(()), "{:?}", scan.notes());
    let status_reads = host
        .trace()
        .iter()
        .filter(|c| {
            c.op == Op::ProcStatus && c.path.as_deref() == Some(Path::new("/proc/10/status"))
        })
        .count();
    assert_eq!(status_reads, 1 + inuse::REREAD_PAUSES.len());
}

/// mped's pid-reuse probe: pid 10 is found gone; later pid 40 forks a
/// child that gets pid 10 again, hands it the candidate's file and closes
/// its own. A listed pid that was found gone is read again as a new
/// process (once per scan), so the new 10 is read: `in_use`.
#[test]
fn a_reused_pid_is_read_as_a_new_process() {
    let host = model(&format!(
        "{}events:
  - {{at: {{op: proc_fds, path: /proc/10/fd, nth: 1, when: before}}, exit: 10}}
  - {{at: {{op: sleep, nth: 1, when: before}}, fork: {{pid: 20, child: 40}}}}
  - {{at: {{op: sleep, nth: 1, when: before}}, open: {{pid: 40, path: /tmp/c/a}}}}
  - {{at: {{op: proc_fds, path: /proc/40/fd, nth: 1, when: before}}, fork: {{pid: 40, child: 10}}}}
  - {{at: {{op: proc_fds, path: /proc/40/fd, nth: 1, when: before}}, close: {{pid: 40, path: /tmp/c/a}}}}
  - {{at: {{op: proc_status, path: /proc/30/status, nth: 3, when: before}}, exit: 30}}
",
        processes(&[
            process(10, &[]),
            process(20, &[]),
            process(30, &["proc: failed"]),
        ])
    ));
    let (result, scan) = check_c(&host, true);
    let skip = in_use(result);
    assert!(
        skip.detail.starts_with("pid 10 "),
        "{skip}; {:?}",
        scan.notes()
    );
}

/// Numeric pid listings can be identical even when a settled process was
/// replaced. The old scan could stop at the second listing without ever
/// reading the replacement that inherited the candidate's fd.
#[test]
fn a_reused_settled_pid_with_unchanged_listing_is_read_again() {
    let host = model(&format!(
        "{}events:
  - {{at: {{op: pids, nth: 2, when: before}}, open: {{pid: 11, path: /tmp/c/a}}}}
  - {{at: {{op: pids, nth: 2, when: before}}, exit: 10}}
  - {{at: {{op: pids, nth: 2, when: before}}, fork: {{pid: 11, child: 10}}}}
  - {{at: {{op: pids, nth: 2, when: before}}, close: {{pid: 11, path: /tmp/c/a}}}}
",
        processes(&[process(10, &[]), process(11, &[])])
    ));
    let (result, scan) = check_c(&host, true);
    let skip = in_use(result);
    assert!(
        skip.detail.starts_with("pid 10 "),
        "{skip}; {:?}",
        scan.notes()
    );
    assert!(
        host.trace_count(Op::ProcIdentity) >= 3,
        "replacement must be pinned and read"
    );
}

/// A process can fork a child and exit after it was listed but before its
/// identity can be opened. Its child was absent from that listing, so a
/// failed identity open must force a fresh listing, even with no read batch.
#[test]
fn a_process_gone_before_identity_open_does_not_end_the_scan() {
    let host = model(&format!(
        "{}events:
  - {{at: {{op: proc_identity, nth: 1, when: before}}, open: {{pid: 10, path: /tmp/c/a}}}}
  - {{at: {{op: proc_identity, nth: 1, when: before}}, fork: {{pid: 10, child: 11}}}}
  - {{at: {{op: proc_identity, nth: 1, when: before}}, close: {{pid: 10, path: /tmp/c/a}}}}
  - {{at: {{op: proc_identity, nth: 1, when: before}}, exit: 10}}
",
        processes(&[process(10, &[])])
    ));
    let (result, scan) = check_c(&host, true);
    let skip = in_use(result);
    assert!(
        skip.detail.starts_with("pid 11 "),
        "{skip}; {:?}",
        scan.notes()
    );
    assert!(
        host.trace_count(Op::Pids) >= 2,
        "a vanished pin forces relisting"
    );
}

#[test]
fn an_identity_probe_error_blocks_every_candidate_without_aborting_the_scan() {
    let host = model(&format!(
        "{}events:
  - {{at: {{op: proc_identity_released, nth: 1, when: before}}, fail: EIO}}
",
        processes(&[process(10, &[])])
    ));
    let (result, scan) = check_c(&host, true);
    let skip = in_use(result);
    assert!(
        skip.detail
            .contains("process identity could not be checked"),
        "{skip}"
    );
    assert!(
        scan.notes()
            .iter()
            .any(|note| note.contains("process identity could not be checked")),
        "{:?}",
        scan.notes()
    );
}

#[test]
fn an_identity_open_error_also_blocks_every_candidate() {
    let host = model(&format!(
        "{}events:
  - {{at: {{op: proc_identity, nth: 1, when: before}}, fail: EIO}}
",
        processes(&[process(10, &[])])
    ));
    let (result, scan) = check_c(&host, true);
    let skip = in_use(result);
    assert!(
        skip.detail
            .contains("process identity could not be checked"),
        "{skip}"
    );
    assert!(
        scan.notes()
            .iter()
            .any(|note| note.contains("pidfd_open 10: EIO")),
        "{:?}",
        scan.notes()
    );
}

/// A process that could not be read is read again after a pause; one that
/// exits meanwhile is gone and blocks nothing.
#[test]
fn an_unreadable_process_is_read_again_and_may_be_gone() {
    let host = model(&format!(
        "{}events:\n  - {{at: {{op: sleep, nth: 1, when: after}}, exit: 10}}\n",
        processes(&[process(10, &["proc: failed"]), process(11, &[])])
    ));
    let (result, scan) = check_c(&host, true);
    assert_eq!(result, Ok(()), "{:?}", scan.notes());
    assert_eq!(scan.gone(), 1);
}

/// AC #4: a process exiting while the scan reads others (its readers then
/// say `Gone`) does not abort the scan: another process's use is still
/// found, and with nobody left the candidate is clear.
#[test]
fn a_process_exiting_mid_scan_does_not_abort_it() {
    let exits = "events:\n  - {at: {op: proc_fds, path: /proc/5/fd, when: before}, exit: 5}\n";
    let host = model(&format!(
        "{}{exits}",
        processes(&[process(5, &[]), process(10, &["cwd: /tmp/c"])])
    ));
    let skip = in_use(check_c(&host, true).0);
    assert!(skip.detail.starts_with("pid 10 "), "{skip}");
    let host = model(&format!(
        "{}{exits}",
        processes(&[process(5, &["cwd: /tmp/c"])])
    ));
    let (result, scan) = check_c(&host, true);
    assert_eq!(result, Ok(()));
    assert_eq!(scan.gone(), 1);
}

// ---- The process list ----

/// The process-level handoff (task-3 review): diskgc lists P; P forks C,
/// which inherits P's fd on the candidate's file, and P closes it; then
/// diskgc reads P and finds nothing. C was born before P's read, so the
/// next listing shows it, and it is read: `in_use` by C.
#[test]
fn a_child_forked_before_its_parent_was_read_is_found_by_listing_again() {
    let host = model(&format!(
        "{}events:
  - {{at: {{op: pids, nth: 1, when: after}}, fork: {{pid: 10, child: 11}}}}
  - {{at: {{op: pids, nth: 1, when: after}}, close: {{pid: 10, path: /tmp/c/a}}}}
",
        processes(&[process(10, &["fds: [{path: /tmp/keep}, {path: /tmp/c/a}]"])])
    ));
    let skip = in_use(check_c(&host, true).0);
    assert!(
        skip.detail.starts_with("pid 11 (tool) has it open"),
        "{skip}"
    );
    let listings = host.trace().iter().filter(|c| c.op == Op::Pids).count();
    assert_eq!(
        listings, 3,
        "list, read 10; list, read 11; list: nothing new"
    );
}

/// A process list that keeps growing for [`MAX_LISTINGS`] listings leaves
/// the scan unsettled: every candidate is `in_use`. One that settles
/// within the bound does not.
#[test]
fn a_process_list_that_keeps_growing_leaves_the_scan_unsettled() {
    let forks = |n: u32| -> String {
        (1..=n)
            .map(|k| {
                format!(
                    "  - {{at: {{op: pids, nth: {k}, when: after}}, fork: {{pid: 10, child: {}}}}}\n",
                    100 + k
                )
            })
            .collect()
    };
    let n = u32::try_from(MAX_LISTINGS).expect("small");
    let host = model(&format!(
        "{}events:\n{}",
        processes(&[process(10, &[])]),
        forks(n)
    ));
    let skip = in_use(check_c(&host, true).0);
    assert!(skip.detail.contains("still growing"), "{skip}");
    assert!(
        skip.process_blockers.is_none(),
        "unsettled scans cannot attribute a holder"
    );
    let host = model(&format!(
        "{}events:\n{}",
        processes(&[process(10, &[])]),
        forks(n - 2)
    ));
    assert_eq!(check_c(&host, true).0, Ok(()));
}

/// The first 16 growing listings do not condemn the candidate: after a
/// short pause, a complete second scan sees a stable process list. The
/// candidate may be cleared only on that settled scan.
#[test]
fn a_transiently_unsettled_scan_is_retried_before_judgement() {
    let n = u32::try_from(MAX_LISTINGS).expect("small");
    let forks: String = (1..=n)
        .map(|k| {
            format!(
                "  - {{at: {{op: pids, nth: {k}, when: after}}, fork: {{pid: 10, child: {}}}}}\n",
                100 + k
            )
        })
        .collect();
    let host = model(&format!(
        "{}events:\n{forks}",
        processes(&[process(10, &[])])
    ));
    let parent = dir(&host, "/tmp");
    let path = PathBuf::from("/tmp/c");
    let candidate = Candidate::new(&parent, &path).expect("candidate");
    let rules = rules(true);
    let record = walk(&host, &candidate, &rules, None).expect("walk");
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    assert!(
        checker
            .check(&host, &candidate, &record, &mut FreshAttempts::new(None))
            .expect("check")
            .is_ok()
    );
    assert!(host.unfired_events().is_empty());
    assert_eq!(
        host.trace().iter().filter(|c| c.op == Op::OwnPid).count(),
        2,
        "the first unsettled scan must be followed by one complete scan"
    );
    assert!(
        checker
            .latest()
            .expect("scan")
            .notes()
            .iter()
            .any(|n| n.contains("retried")),
        "the retry is observable"
    );
}

/// A fork storm that outlasts the one retry never clears a candidate. A
/// later check scans immediately: an unsettled answer cannot stand for
/// another candidate even inside the ordinary blocked reuse window.
#[test]
fn a_persistently_unsettled_scan_is_never_reused() {
    let n = u32::try_from(MAX_LISTINGS).expect("small");
    let forks: String = (1..=2 * n)
        .map(|k| {
            format!(
                "  - {{at: {{op: pids, nth: {k}, when: after}}, fork: {{pid: 10, child: {}}}}}\n",
                100 + k
            )
        })
        .collect();
    let host = model(&format!(
        "{}events:\n{forks}",
        processes(&[process(10, &[])])
    ));
    let parent = dir(&host, "/tmp");
    let path = PathBuf::from("/tmp/c");
    let candidate = Candidate::new(&parent, &path).expect("candidate");
    let rules = rules(true);
    let record = walk(&host, &candidate, &rules, None).expect("walk");
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    assert_eq!(
        checker
            .check(&host, &candidate, &record, &mut FreshAttempts::new(None))
            .expect("check")
            .expect_err("two unsettled scans must block")
            .reason,
        Reason::InUse
    );
    assert_eq!(
        host.trace().iter().filter(|c| c.op == Op::OwnPid).count(),
        2
    );
    assert_eq!(
        host.trace().iter().filter(|c| c.op == Op::Pids).count(),
        2 * MAX_LISTINGS,
        "each scan stops at the 16-listing bound under continuing forks"
    );
    assert!(
        checker
            .check(&host, &candidate, &record, &mut FreshAttempts::new(None))
            .expect("check")
            .is_ok()
    );
    assert_eq!(
        host.trace().iter().filter(|c| c.op == Op::OwnPid).count(),
        3,
        "the next candidate must rescan immediately"
    );
    assert!(host.unfired_events().is_empty());
}

/// An explicit zero reuse limit also applies when a scan blocks everyone.
#[test]
fn a_blocking_scan_respects_zero_reuse() {
    let host = model(&processes(&[process(10, &["proc: denied", "gid: 1001"])]));
    let parent = dir(&host, "/tmp");
    let path = PathBuf::from("/tmp/c");
    let candidate = Candidate::new(&parent, &path).expect("candidate");
    let rules = rules(true);
    let record = walk(&host, &candidate, &rules, None).expect("walk");
    let mut checker = Checker::new(&rules, std::time::Duration::ZERO);
    for _ in 0..2 {
        assert_eq!(
            checker
                .check(&host, &candidate, &record, &mut FreshAttempts::new(None))
                .expect("check")
                .expect_err("unreadable process blocks")
                .reason,
            Reason::InUse
        );
    }
    assert_eq!(
        host.trace().iter().filter(|c| c.op == Op::OwnPid).count(),
        2
    );
}

/// A slow blocking scan is shared from completion, not already expired by
/// the time its verdict is ready.
#[test]
fn a_slow_blocking_scan_is_reused_from_completion() {
    let host = model(&format!(
        "{}events:\n  - {{at: {{op: own_pid, nth: 1, when: after}}, advance: 600ms}}\n",
        processes(&[process(10, &["proc: denied", "gid: 1001"])])
    ));
    let parent = dir(&host, "/tmp");
    let path = PathBuf::from("/tmp/c");
    let candidate = Candidate::new(&parent, &path).expect("candidate");
    let rules = rules(true);
    let record = walk(&host, &candidate, &rules, None).expect("walk");
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    for _ in 0..2 {
        assert!(
            checker
                .check(&host, &candidate, &record, &mut FreshAttempts::new(None))
                .expect("check")
                .is_err()
        );
    }
    assert_eq!(
        host.trace().iter().filter(|c| c.op == Op::OwnPid).count(),
        1
    );
    host.sleep(std::time::Duration::from_millis(500));
    assert!(
        checker
            .check(&host, &candidate, &record, &mut FreshAttempts::new(None))
            .expect("check")
            .is_err()
    );
    assert_eq!(
        host.trace().iter().filter(|c| c.op == Op::OwnPid).count(),
        2
    );
}

/// Model a 100 ms scan and 40,000 small-candidate checks taking another
/// 400 ms. The process scan belongs to the reuse window, not to an
/// individual candidate. A following check at exactly 500 ms must scan
/// again. The integrated fs reclaim is separately
/// exercised by the two-candidate freshness tests below.
#[test]
fn forty_thousand_candidate_checks_share_a_scan_within_freshness() {
    let host = model("events:\n  - {at: {op: own_pid, nth: 1, when: after}, advance: 100ms}\n");
    let parent = dir(&host, "/tmp");
    let path = PathBuf::from("/tmp/c");
    let candidate = Candidate::new(&parent, &path).expect("candidate");
    let rules = rules(true);
    let record = walk(&host, &candidate, &rules, None).expect("walk");
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    for _ in 0..40_000 {
        assert!(
            checker
                .check(&host, &candidate, &record, &mut FreshAttempts::new(None))
                .expect("check")
                .is_ok()
        );
        host.sleep(std::time::Duration::from_micros(10));
    }
    assert_eq!(
        host.trace().iter().filter(|c| c.op == Op::OwnPid).count(),
        1,
        "40,000 checks inside one reuse window use one scan"
    );
    host.sleep(std::time::Duration::from_millis(100));
    assert!(
        checker
            .check(&host, &candidate, &record, &mut FreshAttempts::new(None))
            .expect("check")
            .is_ok()
    );
    assert_eq!(
        host.trace().iter().filter(|c| c.op == Op::OwnPid).count(),
        2,
        "the exact reuse boundary forces a second scan"
    );
}

// ---- flock ----

/// A candidate directory `flock`ed by another process (shared or
/// exclusive) is `locked`: diskgc's exclusive lock without waiting fails,
/// and nothing is walked or unlinked. A lock on a file inside is the open
/// fd of its holder: `in_use`.
#[test]
fn a_directory_flocked_by_another_process_is_locked() {
    for mode in ["shared", "exclusive"] {
        let host = model(&format!(
            "locks:\n  - {{path: /tmp/c, holder: 10, mode: {mode}}}\n{}",
            processes(&[process(10, &[])])
        ));
        let skip = skip_of(reclaim(&host, "/tmp/c", true));
        assert_eq!(
            (skip.reason, skip.path.as_path()),
            (Reason::Locked, Path::new("/tmp/c")),
            "{mode}: {skip}"
        );
        assert!(
            skip.process_blockers.is_none(),
            "flock does not identify its holder"
        );
        assert!(
            !host
                .trace()
                .iter()
                .any(|c| matches!(c.op, Op::ReadDir | Op::Unlinkat)),
            "{mode}: walked or unlinked under someone else's lock"
        );
    }
    let host = model(&format!(
        "locks:\n  - {{path: /tmp/c/a, holder: 10, mode: exclusive}}\n{}",
        processes(&[process(10, &[])])
    ));
    let skip = skip_of(reclaim(&host, "/tmp/c", true));
    assert_eq!(skip.reason, Reason::InUse, "{skip}");
    // diskgc's own lock is released when the step returns.
    let host = model(&processes(&[process(10, &[])]));
    let skip = skip_of(reclaim(
        &model(&processes(&[process(10, &["cwd: /tmp/c"])])),
        "/tmp/c",
        true,
    ));
    assert_eq!(skip.reason, Reason::InUse, "{skip}");
    complete(reclaim(&host, "/tmp/c", true));
    assert!(
        host.trace()
            .iter()
            .any(|c| c.op == Op::Flock && c.result == "acquired"),
        "the candidate was locked"
    );
    assert!(host.locks_on(Path::new("/tmp/d")).is_empty());
}

/// The lock is taken on the directory the step opened; if the name is
/// replaced between that and the walk (another directory moved in), the
/// walked directory is not the locked one: `changed`, nothing unlinked.
#[test]
fn a_directory_replaced_between_its_lock_and_its_walk_is_changed() {
    let host = model(&format!(
        "{}events:
  - {{at: {{op: flock, when: after}}, rename: {{from: /tmp/c, to: /tmp/moved}}}}
  - {{at: {{op: flock, when: after}}, rename: {{from: /tmp/d, to: /tmp/c}}}}
",
        processes(&[process(10, &[])])
    ));
    let skip = skip_of(reclaim(&host, "/tmp/c", true));
    assert_eq!(
        (skip.reason, skip.path.as_path()),
        (Reason::Changed, Path::new("/tmp/c")),
        "{skip}"
    );
    assert!(exists(&host, "/tmp/c/x") && exists(&host, "/tmp/moved/a"));
}

// ---- Sockets and FIFOs under the "live only" policy ----

/// With the walk recording sockets and FIFOs (`block_ipc` off): a FIFO
/// some process holds open makes the candidate `in_use`, a socket bound in
/// diskgc's network namespace makes it `contains_ipc`, and so does a
/// socket whose liveness cannot be told (the query failed); a stale FIFO
/// and a stale socket file are deleted with the candidate.
#[test]
fn only_live_sockets_and_fifos_keep_their_candidate_under_the_live_policy() {
    let tree = |bound: bool| {
        format!(
            "  /tmp/c/fifo: {{type: fifo}}\n  /tmp/c/sub/sock: {{type: socket{}}}\n",
            if bound { ", bound: true" } else { "" }
        )
    };
    // Stale: deleted.
    let host = model(&format!(
        "{}{}",
        tree(false),
        processes(&[process(10, &[])])
    ));
    complete(reclaim(&host, "/tmp/c", false));
    assert!(!exists(&host, "/tmp/c"));
    // A FIFO held open.
    let host = model(&format!(
        "{}{}",
        tree(false),
        processes(&[process(10, &["fds: [{path: /tmp/c/fifo}]"])])
    ));
    let skip = skip_of(reclaim(&host, "/tmp/c", false));
    assert_eq!(
        (skip.reason, skip.path.as_path()),
        (Reason::InUse, Path::new("/tmp/c/fifo")),
        "{skip}"
    );
    // A bound socket.
    let host = model(&format!("{}{}", tree(true), processes(&[process(10, &[])])));
    let skip = skip_of(reclaim(&host, "/tmp/c", false));
    assert_eq!(
        (skip.reason, skip.path.as_path()),
        (Reason::ContainsIpc, Path::new("/tmp/c/sub/sock")),
        "{skip}"
    );
    assert!(skip.detail.contains("bound"), "{skip}");
    // The socket query fails: live.
    let host = model(&format!(
        "{}{}events:\n  - {{at: {{op: bound_unix_sockets, when: before}}, fail: EIO}}\n",
        tree(false),
        processes(&[process(10, &[])])
    ));
    let skip = skip_of(reclaim(&host, "/tmp/c", false));
    assert_eq!(skip.reason, Reason::ContainsIpc, "{skip}");
    assert!(skip.detail.contains("cannot be told"), "{skip}");
    // A scan that did not ask for sockets cannot clear one either.
    let host = model(&format!(
        "{}{}",
        tree(false),
        processes(&[process(10, &[])])
    ));
    let parent = dir(&host, "/tmp");
    let path = PathBuf::from("/tmp/c");
    let candidate = Candidate::new(&parent, &path).expect("a name");
    let record = walk(&host, &candidate, &rules(false), None).expect("walk");
    let scan = inuse::scan(&host, &options(false)).expect("scan");
    let skip = scan.check(&candidate, &record).expect_err("cannot tell");
    assert_eq!(skip.reason, Reason::ContainsIpc, "{skip}");
}

// ---- Freshness (AC #6) ----

/// Model events after the last unlink of `/tmp/c` (the candidate itself).
fn after_c_is_deleted(actions: &[&str]) -> String {
    let events: String = actions
        .iter()
        .map(|a| format!("  - {{at: {{op: unlinkat, path: /tmp/c, when: after}}, {a}}}\n"))
        .collect();
    format!("events:\n{events}")
}

/// A scan serves later candidates only while it is young, measured on the
/// clock that includes suspend time: after a suspend (the clock jumps 2 s)
/// the next candidate gets a new scan, which sees a file opened meanwhile.
/// Within the reuse window, a use that starts after the scan is not seen
/// (docs/safety.md, In use, limit 5): pinned here as the known limit.
#[test]
fn a_scan_is_not_reused_across_a_suspend() {
    let procs = processes(&[process(10, &[])]);
    let host = model(&format!(
        "{procs}{}",
        after_c_is_deleted(&["advance: 2s", "open: {pid: 10, path: /tmp/d/x}"])
    ));
    let mut checker = Checker::new(&rules(true), REUSE_LIMIT);
    complete(reclaim_with(&host, &mut checker, "/tmp/c", true));
    let skip = skip_of(reclaim_with(&host, &mut checker, "/tmp/d", true));
    assert_eq!(skip.reason, Reason::InUse, "{skip}");
    assert!(exists(&host, "/tmp/d/x"));
    // The limit: without the suspend the open falls in the reuse window.
    let host = model(&format!(
        "{procs}{}",
        after_c_is_deleted(&["open: {pid: 10, path: /tmp/d/x}"])
    ));
    let mut checker = Checker::new(&rules(true), REUSE_LIMIT);
    complete(reclaim_with(&host, &mut checker, "/tmp/c", true));
    complete(reclaim_with(&host, &mut checker, "/tmp/d", true));
}

/// A clear scan is reusable only strictly before the one-second freshness
/// boundary. At the boundary, the next candidate gets a new scan that
/// sees a file opened meanwhile.
#[test]
fn a_scan_at_the_freshness_boundary_is_not_reused() {
    let host = model(&format!(
        "{}{}",
        processes(&[process(10, &[])]),
        after_c_is_deleted(&["advance: 1s", "open: {pid: 10, path: /tmp/d/x}"])
    ));
    let mut checker = Checker::new(&rules(true), REUSE_LIMIT);
    complete(reclaim_with(&host, &mut checker, "/tmp/c", true));
    let skip = skip_of(reclaim_with(&host, &mut checker, "/tmp/d", true));
    assert_eq!(skip.reason, Reason::InUse, "{skip}");
}

/// A settled scan serves the next candidate at 400 ms. The checker and
/// delete entry both check freshness before unlinking it.
#[test]
fn a_settled_scan_is_reused_inside_the_reuse_window() {
    let host = model(&format!(
        "{}{}",
        processes(&[process(10, &[])]),
        after_c_is_deleted(&["advance: 400ms"])
    ));
    let mut checker = Checker::new(&rules(true), REUSE_LIMIT);
    complete(reclaim_with(&host, &mut checker, "/tmp/c", true));
    complete(reclaim_with(&host, &mut checker, "/tmp/d", true));
    assert_eq!(
        host.trace().iter().filter(|c| c.op == Op::OwnPid).count(),
        1,
        "both candidates use one process scan"
    );
}

/// The call numbers of the last two clock reads before the first unlink of
/// a clean run of the deletion step on the bystander model: the checker's
/// freshness check after its verdict, then `delete_cleared`'s.
fn last_clock_reads_before_the_delete() -> (u64, u64) {
    let host = model(&processes(&[process(10, &[])]));
    complete(reclaim(&host, "/tmp/c", true));
    let trace = host.trace();
    let first_unlink = trace
        .iter()
        .position(|c| c.op == Op::Unlinkat)
        .expect("an unlink");
    let nows: Vec<u64> = trace[..first_unlink]
        .iter()
        .filter(|c| c.op == Op::Now)
        .map(|c| c.index)
        .collect();
    let [.., verdict, delete] = nows[..] else {
        panic!("fewer than two clock reads: {nows:?}");
    };
    (verdict, delete)
}

/// A suspend between the scan and the checker's verdict check (its last
/// clock read): the checker checks again with a new scan, which sees a
/// file opened meanwhile.
#[test]
fn a_check_that_expired_before_its_verdict_is_made_again() {
    let (verdict, _) = last_clock_reads_before_the_delete();
    let host = model(&format!(
        "{}events:
  - {{at: {{call: {verdict}, when: before}}, advance: 2s}}
  - {{at: {{call: {verdict}, when: before}}, open: {{pid: 10, path: /tmp/c/a}}}}
",
        processes(&[process(10, &[])])
    ));
    let skip = skip_of(reclaim(&host, "/tmp/c", true));
    assert_eq!(skip.reason, Reason::InUse, "{skip}");
    let scans = host.trace().iter().filter(|c| c.op == Op::OwnPid).count();
    assert_eq!(scans, 2, "two scans");
}

/// A suspend between the clear and the deletion (`delete_cleared`'s clock
/// read): the clear has expired, nothing is unlinked on it, and the step
/// checks once more with a new scan, which sees a file opened meanwhile.
#[test]
fn a_clear_that_expired_before_the_delete_is_made_again() {
    let (_, delete) = last_clock_reads_before_the_delete();
    let host = model(&format!(
        "{}events:
  - {{at: {{call: {delete}, when: before}}, advance: 2s}}
  - {{at: {{call: {delete}, when: before}}, open: {{pid: 10, path: /tmp/c/a}}}}
",
        processes(&[process(10, &[])])
    ));
    let skip = skip_of(reclaim(&host, "/tmp/c", true));
    assert_eq!(skip.reason, Reason::InUse, "{skip}");
    assert!(exists(&host, "/tmp/c/a"));
}

/// A suspend during the scan leaves it stale on arrival: the checker scans
/// once more instead of clearing on it.
#[test]
fn a_suspend_during_the_scan_makes_the_checker_scan_again() {
    let host = model(&format!(
        "{}events:\n  - {{at: {{op: proc_environ, nth: 1, when: after}}, advance: 2s}}\n",
        processes(&[process(10, &[])])
    ));
    complete(reclaim(&host, "/tmp/c", true));
    let scans = host.trace().iter().filter(|c| c.op == Op::OwnPid).count();
    assert_eq!(scans, 2, "the stale scan was replaced");
}

// ---- No fresh scan (TASK-144) ----

/// Events that make each of the first `stale` scans finish stale: the
/// clock jumps 2 s (twice the freshness bound) right after the scan reads
/// the one process's environment, as a scan on a busy host would take.
fn stale_scans(stale: u32) -> String {
    let mut events = String::from("events:\n");
    for nth in 1..=stale {
        events.push_str(&format!(
            "  - {{at: {{op: proc_environ, nth: {nth}, when: after}}, advance: 2s}}\n"
        ));
    }
    format!("{}{events}", processes(&[process(10, &[])]))
}

fn scans(host: &ModelHost) -> usize {
    host.trace().iter().filter(|c| c.op == Op::OwnPid).count()
}

/// Every scan finishes stale: the candidate gets `FRESH_ATTEMPTS` scans,
/// paused 250 ms then 1 s, and is then skipped `in_use_unknown` (never an
/// error of the run); nothing is deleted on a stale scan.
#[test]
fn a_candidate_without_a_fresh_scan_is_skipped_in_use_unknown() {
    let host = model(&stale_scans(inuse::FRESH_ATTEMPTS as u32));
    let started = host.now();
    let skip = skip_of(reclaim(&host, "/tmp/c", true));
    assert_eq!(skip.reason, Reason::InUseUnknown, "{skip}");
    assert!(
        skip.detail.contains("too busy") && skip.detail.contains("3 attempt(s)"),
        "{skip}"
    );
    assert_eq!(scans(&host), inuse::FRESH_ATTEMPTS);
    assert!(exists(&host, "/tmp/c/a") && exists(&host, "/tmp/c/sub/f"));
    assert!(
        !host.trace().iter().any(|c| c.op == Op::Unlinkat),
        "nothing is unlinked on a stale scan"
    );
    let pauses: std::time::Duration = inuse::FRESH_RETRY_PAUSES.iter().sum();
    let stale = std::time::Duration::from_secs(2) * inuse::FRESH_ATTEMPTS as u32;
    assert_eq!(host.now().saturating_since(started), stale + pauses);
    assert!(host.unfired_events().is_empty());
}

/// Two stale scans, then a fresh one: the third attempt deletes.
#[test]
fn a_fresh_scan_on_the_last_attempt_deletes() {
    let host = model(&stale_scans(inuse::FRESH_ATTEMPTS as u32 - 1));
    complete(reclaim(&host, "/tmp/c", true));
    assert_eq!(scans(&host), inuse::FRESH_ATTEMPTS);
    assert!(!exists(&host, "/tmp/c"));
}

/// The retries never pause to or past the deadline: with less time left
/// than the first pause, the first stale scan is the last attempt.
#[test]
fn no_retry_pause_reaches_the_deadline() {
    let host = model(&stale_scans(inuse::FRESH_ATTEMPTS as u32));
    let deadline = host
        .now()
        .checked_add(std::time::Duration::from_millis(2100))
        .expect("in range");
    let parent = dir(&host, "/tmp");
    let path = PathBuf::from("/tmp/c");
    let candidate = Candidate::new(&parent, &path).expect("a name");
    let mut checker = Checker::new(&rules(true), REUSE_LIMIT);
    let options = DeleteOptions {
        deadline: Some(deadline),
        ..DeleteOptions::default()
    };
    let skip = skip_of(delete_unused(
        &host,
        &mut checker,
        &candidate,
        &rules(true),
        options,
    ));
    assert_eq!(skip.reason, Reason::InUseUnknown, "{skip}");
    assert!(skip.detail.contains("deadline"), "{skip}");
    assert_eq!(scans(&host), 1);
    assert!(host.now() < deadline, "no pause ran into the deadline");
    assert!(exists(&host, "/tmp/c/a"));
}

/// A clear that expires before the delete on every attempt (the caller's
/// own check between them is slower than the bound) spends the same
/// budget: `in_use_unknown` after `FRESH_ATTEMPTS` scans, nothing deleted.
#[test]
fn clears_that_keep_expiring_share_the_budget() {
    let host = model(&processes(&[process(10, &[])]));
    let parent = dir(&host, "/tmp");
    let path = PathBuf::from("/tmp/c");
    let candidate = Candidate::new(&parent, &path).expect("a name");
    let rules = rules(true);
    let record = walk(&host, &candidate, &rules, None).expect("walk");
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    let mut caller_checks = 0;
    let result = inuse::clear_and_delete(
        &host,
        &mut checker,
        &candidate,
        &record,
        DeleteOptions::default(),
        &mut FreshAttempts::new(None),
        &mut |_| {
            caller_checks += 1;
            host.sleep(std::time::Duration::from_secs(2));
            Ok(())
        },
    );
    let Err(inuse::NotDeleted::Skipped(skip)) = result else {
        panic!("expected a skip, got {result:?}");
    };
    assert_eq!(skip.reason, Reason::InUseUnknown, "{skip}");
    assert!(skip.detail.contains("expired"), "{skip}");
    assert_eq!(caller_checks, inuse::FRESH_ATTEMPTS);
    assert_eq!(scans(&host), inuse::FRESH_ATTEMPTS);
    assert!(exists(&host, "/tmp/c/a"));
}

/// The checker's stale scans and the caller's expired clears spend one
/// budget: a stale scan, then a clear that expires before the delete, then
/// another stale scan is `in_use_unknown` after exactly `FRESH_ATTEMPTS`
/// scans, not a fresh budget per kind of retry.
#[test]
fn stale_scans_and_expired_clears_spend_one_budget() {
    let host = model(&format!(
        "{}events:
  - {{at: {{op: proc_environ, nth: 1, when: after}}, advance: 2s}}
  - {{at: {{op: proc_environ, nth: 3, when: after}}, advance: 2s}}
",
        processes(&[process(10, &[])])
    ));
    let parent = dir(&host, "/tmp");
    let path = PathBuf::from("/tmp/c");
    let candidate = Candidate::new(&parent, &path).expect("a name");
    let rules = rules(true);
    let record = walk(&host, &candidate, &rules, None).expect("walk");
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    let mut caller_checks = 0;
    let result = inuse::clear_and_delete(
        &host,
        &mut checker,
        &candidate,
        &record,
        DeleteOptions::default(),
        &mut FreshAttempts::new(None),
        &mut |_| {
            // Only the second scan clears; its clear expires here.
            caller_checks += 1;
            host.sleep(std::time::Duration::from_secs(2));
            Ok(())
        },
    );
    let Err(inuse::NotDeleted::Skipped(skip)) = result else {
        panic!("expected a skip, got {result:?}");
    };
    assert_eq!(skip.reason, Reason::InUseUnknown, "{skip}");
    assert!(skip.detail.contains("3 attempt(s)"), "{skip}");
    assert_eq!(caller_checks, 1);
    assert_eq!(scans(&host), inuse::FRESH_ATTEMPTS);
    assert!(host.unfired_events().is_empty());
    assert!(exists(&host, "/tmp/c/a"));
}

/// After an `in_use_unknown` skip the checker still has its last (stale)
/// scan, so a caller can log its notes; it is never reused for a verdict.
#[test]
fn an_in_use_unknown_skip_keeps_the_stale_scan_as_latest() {
    let host = model(&stale_scans(inuse::FRESH_ATTEMPTS as u32));
    let mut checker = Checker::new(&rules(true), REUSE_LIMIT);
    let skip = skip_of(reclaim_with(&host, &mut checker, "/tmp/c", true));
    assert_eq!(skip.reason, Reason::InUseUnknown, "{skip}");
    let latest = checker.latest().expect("the stale scan is kept");
    assert!(!latest.is_fresh_at(host.now()));
}
