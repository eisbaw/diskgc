//! What ModelHost answers: path resolution through the core's `resolve`,
//! kernel capabilities, events on the call trace, the virtual clock, the
//! `/proc` readers and derived space.

mod common;

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use common::{dir, failure, host, tmp_yaml};
use diskgc_core::engine::Reason;
use diskgc_core::host::{
    Access, Base, Delete, DirListing, Errno, FileType, Host, Pid, ProcError, ProcLink, Unlink,
};
use diskgc_core::kernel::require_supported;
use diskgc_core::resolve::{ResolveError, Unsafe, Want, open_beneath, resolve};
use diskgc_model::ModelHost;
use diskgc_model::harness::{Outcome, run};
use diskgc_model::host::UPTIME_AT_START;
use diskgc_model::load::parse_yaml;
use diskgc_model::spec::Op;
use diskgc_model::state::Fault;

fn root_bind_yaml() -> String {
    tmp_yaml("").replace(
        "tree:\n",
        "  - {id: 31, at: /tmp/a/sub/bind, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 30, bind_of: 30}\ntree:\n",
    )
}

#[test]
fn root_bind_shares_inodes_but_keeps_a_mount_boundary() {
    let host = host(&root_bind_yaml());
    let source = host.stat_path(Path::new("/tmp/a/f")).expect("source");
    let alias = host
        .stat_path(Path::new("/tmp/a/sub/bind/a/f"))
        .expect("alias");
    assert_eq!(source.dev_ino(), alias.dev_ino());
    assert_ne!(source.mnt_id, alias.mnt_id);
    assert_eq!(
        host.mountinfo().unwrap().last().unwrap().fs_root,
        Path::new("/")
    );
    let outer = dir(&host, "/tmp/a/sub");
    let error = open_beneath(
        &host,
        &outer.fd,
        outer.mount.id,
        Path::new("bind"),
        Access::Path,
        Want::Directory,
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            ResolveError::UnsafePath {
                why: Unsafe::CrossesMount,
                ..
            }
        ),
        "{error}"
    );
}

/// `bind_root`: a bind of the directory `/a` of `/tmp`'s filesystem at
/// `/mnt/a`, on another parent mount (TASK-260).
fn subdir_bind_yaml() -> String {
    tmp_yaml("").replace(
        "tree:\n",
        "  - {id: 31, at: /mnt/a, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 1, bind_of: 30, bind_root: /a}\ntree:\n",
    )
}

#[test]
fn subdir_bind_shows_that_directory_and_its_mountinfo_root() {
    let host = host(&subdir_bind_yaml());
    let source = host.stat_path(Path::new("/tmp/a/f")).expect("source");
    let alias = host.stat_path(Path::new("/mnt/a/f")).expect("alias");
    assert_eq!(source.dev_ino(), alias.dev_ino());
    assert_ne!(source.mnt_id, alias.mnt_id);
    let bind = host.mountinfo().unwrap().last().unwrap().clone();
    assert_eq!(bind.fs_root, Path::new("/a"));
    assert_eq!(bind.mount_point, Path::new("/mnt/a"));
    // The bind's root is the mount root there; the same directory reached
    // through /tmp is not.
    let at_bind = resolve(&host, Path::new("/mnt/a"), Want::Directory).expect("bind");
    assert_eq!(at_bind.stat.mount_root, Some(true));
    assert_eq!(at_bind.mount.id, 31);
    let at_source = resolve(&host, Path::new("/tmp/a"), Want::Directory).expect("source");
    assert_eq!(at_source.stat.mount_root, Some(false));
    assert_eq!(at_source.stat.dev_ino(), at_bind.stat.dev_ino());
    // Nothing above the bound directory is reachable through the bind.
    assert!(host.stat_path(Path::new("/mnt/a/a")).is_none());

    assert_error_contains(
        &subdir_bind_yaml().replace(
            "\"0:42\", size: 1G, parent: 1, bind_of: 30, ",
            "\"0:43\", size: 1G, parent: 1, ",
        ),
        "bind_root requires bind_of",
    );
    assert_error_contains(
        &subdir_bind_yaml().replace("bind_root: /a", "bind_root: /a/f"),
        "tree[/tmp/a]/f: declared twice",
    );
}

/// `overlay_upper`: an overlay at `/proj/view` (its own device 0:60) whose
/// upper directory is `/tmp/a` (TASK-268).
fn overlay_yaml() -> String {
    tmp_yaml("").replace(
        "tree:\n",
        "  - {id: 31, at: /proj/view, fs: overlay, dev: \"0:60\", size: 1G, parent: 1, \
         overlay_upper: /tmp/a}\ntree:\n",
    )
}

/// overlayfs shows its upper directory under its own device and as its own
/// filesystem (root field `/`), so neither the table nor `statx` relates
/// the two names; an unlink through the upper directory removes the entry
/// from the merged view too (TASK-268).
#[test]
fn overlay_shows_its_upper_directory_under_its_own_device() {
    let host = host(&overlay_yaml());
    let upper = host.stat_path(Path::new("/tmp/a/f")).expect("upper");
    let merged = host.stat_path(Path::new("/proj/view/f")).expect("merged");
    assert_eq!(upper.ino, merged.ino);
    assert_ne!(upper.dev, merged.dev);
    assert_eq!(merged.dev.to_string(), "0:60");
    let overlay = host.mountinfo().unwrap().last().unwrap().clone();
    assert_eq!(overlay.fs_type, "overlay");
    assert_eq!(overlay.fs_root, Path::new("/"));
    assert_eq!(overlay.dev, merged.dev);
    assert_ne!(
        dir(&host, "/tmp").mount.dev,
        dir(&host, "/proj/view").mount.dev
    );
    let model = parse_yaml(&overlay_yaml()).expect("model");
    let (mut state, _) = diskgc_model::load::load(&model).expect("state");
    let upper = state.lookup(Path::new("/tmp/a")).expect("upper");
    assert!(state.unlinkat(upper, OsStr::new("f"), false).is_ok());
    assert!(state.lookup(Path::new("/proj/view/f")).is_err());
    assert!(state.lookup(Path::new("/proj/view/sub")).is_ok());

    for (from, to, wanted) in [
        (
            "fs: overlay",
            "fs: tmpfs",
            "overlay_upper needs fs: overlay",
        ),
        ("\"0:60\"", "\"0:42\"", "is another filesystem's"),
        (
            "overlay_upper: /tmp/a",
            "overlay_upper: /tmp",
            "is a mount point",
        ),
    ] {
        assert_error_contains(&overlay_yaml().replace(from, to), wanted);
    }
}

fn assert_error_contains(yaml: &str, wanted: &str) {
    match ModelHost::from_yaml(yaml) {
        Ok(_) => panic!("loaded, but should not:\n{yaml}"),
        Err(e) => assert!(e.to_string().contains(wanted), "{wanted:?} not in: {e}"),
    }
}

#[test]
fn cross_bind_rename_is_exdev_and_alias_reference_prevents_inode_reuse() {
    let model = diskgc_model::load::parse_yaml(&root_bind_yaml()).expect("model");
    let (mut state, _) = diskgc_model::load::load(&model).expect("state");
    assert_eq!(
        state.rename_path(Path::new("/tmp/a/f"), Path::new("/tmp/a/sub/bind/new")),
        Err(Fault::Errno(Errno::EXDEV))
    );
    assert_eq!(
        state.rename_path(Path::new("/tmp/a/sub/bind/a/f"), Path::new("/tmp/a/new")),
        Err(Fault::Errno(Errno::EXDEV))
    );
    assert_eq!(
        state.rename_path(Path::new("/tmp/a/sub/bind"), Path::new("/tmp/a/sub/moved")),
        Err(Fault::Errno(Errno::EBUSY))
    );
    let sub = state
        .lookup(Path::new("/tmp/a/sub"))
        .expect("parent of bind");
    assert_eq!(
        state.unlinkat(sub, OsStr::new("bind"), true),
        Err(Errno::EBUSY)
    );
    let alias = state
        .lookup(Path::new("/tmp/a/sub/bind/a/f"))
        .expect("alias");
    state.open_ofd(alias, "/tmp/a/sub/bind/a/f".into(), Access::Read);
    state.elapsed = Duration::from_secs(1);
    assert!(
        matches!(state.replace_path_reusing_inode(Path::new("/tmp/a/f")), Err(Fault::Model(message)) if message.contains("no held references"))
    );
}

// ---- Path resolution ----

#[test]
fn a_symlink_on_the_way_is_unsafe_path() {
    let host = host(&tmp_yaml("  /tmp/link: {symlink: /tmp/a}\n"));
    let err = resolve(&host, Path::new("/tmp/link/f"), Want::Any).unwrap_err();
    assert_eq!(err.skip_reason(), Some(Reason::UnsafePath), "{err}");
    let err = resolve(&host, Path::new("/tmp/link"), Want::Any).unwrap_err();
    assert!(
        matches!(
            err,
            ResolveError::UnsafePath {
                why: Unsafe::Symlink,
                ..
            }
        ),
        "a trailing symlink is refused too: {err}"
    );
}

#[test]
fn descending_into_a_mount_is_refused_by_no_xdev() {
    let yaml = tmp_yaml("").replace(
        "parent: 1}\n",
        "parent: 1}\n  - {id: 31, at: /tmp/a/sub, fs: tmpfs, dev: \"0:43\", size: 1M, parent: 30}\n",
    );
    let host = host(&yaml);
    let a = dir(&host, "/tmp/a");
    let err = open_beneath(
        &host,
        &a.fd,
        a.mount.id,
        Path::new("sub"),
        Access::Path,
        Want::Directory,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            ResolveError::UnsafePath {
                why: Unsafe::CrossesMount,
                ..
            }
        ),
        "{err}"
    );
    // Resolved from the root, the path is on the inner mount.
    assert_eq!(dir(&host, "/tmp/a/sub").mount.id, 31);
}

#[test]
fn stacked_mounts_resolve_to_the_topmost() {
    let yaml = tmp_yaml("").replace(
        "parent: 1}\n",
        "parent: 1}\n  - {id: 33, at: /tmp, fs: tmpfs, dev: \"0:44\", size: 2G, parent: 30}\n",
    );
    let host = host(&yaml);
    let tmp = dir(&host, "/tmp");
    assert_eq!(tmp.mount.id, 33);
    assert_eq!(tmp.stat.mnt_id, Some(33));
    // The tree is what the root sees: /tmp/a is on the topmost mount.
    assert_eq!(dir(&host, "/tmp/a").mount.id, 33);
    let mounts = host.mountinfo().unwrap();
    assert_eq!(mounts.len(), 3);
    assert!(
        mounts
            .iter()
            .all(|m| m.id == 1 || m.mount_point == Path::new("/tmp"))
    );
}

#[test]
fn statx_fields_a_filesystem_does_not_answer_are_none() {
    for (field, missing) in [
        ("mnt_id", "STATX_MNT_ID"),
        ("mount_root", "STATX_ATTR_MOUNT_ROOT"),
    ] {
        let yaml = tmp_yaml("").replace(
            "parent: 1}",
            &format!("parent: 1, statx_missing: [{field}]}}"),
        );
        let host = host(&yaml);
        let err = resolve(&host, Path::new("/tmp/a"), Want::Directory).unwrap_err();
        assert_eq!(err.skip_reason(), Some(Reason::UnsupportedFs), "{err}");
        assert!(err.to_string().contains(missing), "{err}");
    }
}

#[test]
fn stat_answers_from_the_inode_graph() {
    let host = host(&tmp_yaml(
        "  /tmp/b:\n    .: {uid: 1001, mode: \"0700\", mtime: 2026-01-01T00:00:00Z}\n    h: {link: /tmp/a/f}\n    s: {type: socket}\n",
    ));
    let b = dir(&host, "/tmp/b");
    assert_eq!(b.stat.uid, Some(1001));
    assert_eq!(b.stat.perm, Some(0o700));
    assert_eq!(b.stat.mtime.map(|t| t.secs), Some(1_767_225_600));
    let a = dir(&host, "/tmp/a");
    let f = host.statx(Base::Dir(&a.fd), Path::new("f")).unwrap();
    let h = host.statx(Base::Dir(&b.fd), Path::new("h")).unwrap();
    assert_eq!(f.dev_ino(), h.dev_ino(), "a hard link is the same inode");
    assert_eq!(f.nlink, Some(2));
    assert_eq!(f.size, Some(10 << 10));
    assert_eq!(f.blocks, Some(24));
    let s = host.statx(Base::Dir(&b.fd), Path::new("s")).unwrap();
    assert_eq!(s.file_type, Some(FileType::Socket));
}

#[test]
fn read_dir_lists_the_names_of_a_directory_inode() {
    // An entry created right before the listing (an event on the call) is
    // listed; a file is not a directory.
    let host = host(&tmp_yaml(
        "  /tmp/a/l: {symlink: f}\nevents:\n  - {at: {op: read_dir, path: /tmp/a, when: before}, create: /tmp/a/new}\n",
    ));
    let a = dir(&host, "/tmp/a");
    let names = host.read_dir(&a.fd, 4).expect("read_dir");
    assert_eq!(
        names,
        DirListing::Names(
            ["f", "l", "new", "sub"]
                .map(std::ffi::OsString::from)
                .to_vec()
        )
    );
    // One over the limit: the listing stops.
    assert_eq!(
        host.read_dir(&a.fd, 3).expect("read_dir"),
        DirListing::TooMany
    );
    let sub = open_beneath(
        &host,
        &a.fd,
        a.mount.id,
        Path::new("sub"),
        Access::Read,
        Want::Directory,
    )
    .expect("opens sub")
    .0;
    assert_eq!(
        host.read_dir(&sub, 0).expect("read_dir sub"),
        DirListing::Names(Vec::new())
    );
    let f = resolve(&host, Path::new("/tmp/a/f"), Want::Any).expect("resolve f");
    let err = host.read_dir(&f.fd, 10).unwrap_err();
    assert!(err.is(Errno::ENOTDIR), "{err}");

    let listings: Vec<(Option<String>, String)> = host
        .trace()
        .into_iter()
        .filter(|c| c.op == Op::ReadDir)
        .map(|c| (c.path.map(|p| p.display().to_string()), c.result))
        .collect();
    assert_eq!(
        listings,
        [
            (Some("/tmp/a".to_owned()), "ok".to_owned()),
            (Some("/tmp/a".to_owned()), "too many".to_owned()),
            (Some("/tmp/a/sub".to_owned()), "ok".to_owned()),
            (Some("/tmp/a/f".to_owned()), Errno::ENOTDIR.to_string()),
        ]
    );
    assert!(host.unfired_events().is_empty());
}

// ---- Events on the call trace ----

#[test]
fn an_entry_replaced_between_its_identity_check_and_its_unlink_is_the_one_unlinked() {
    // The accepted window (docs/safety.md, Deletion): Linux has no
    // unlink-by-inode. The trace shows which object went.
    let host = host(&tmp_yaml(
        "events:\n  - {at: {op: unlinkat, path: /tmp/a/f, when: before}, replace: /tmp/a/f}\n",
    ));
    let a = dir(&host, "/tmp/a");
    let checked = host.statx(Base::Dir(&a.fd), Path::new("f")).unwrap();
    host.unlinkat(&a.fd, OsStr::new("f"), Unlink::NonDir)
        .unwrap();
    let unlink = host.trace().pop().unwrap();
    assert_ne!(unlink.object, checked.dev_ino(), "the replacement went");
    assert!(host.unfired_events().is_empty());
}

#[test]
fn events_fire_before_or_after_the_addressed_call() {
    let yaml = tmp_yaml(
        "events:\n  - {at: {call: 2, when: before}, remove: /tmp/a/f}\n  - {at: {op: statx, nth: 2, when: after}, create: /tmp/a/f}\n",
    );
    let host = host(&yaml);
    let a = dir(&host, "/tmp/a"); // calls 1-5: mountinfo, openat2, statx, openat2, statx
    // Call 2 (openat2 of /tmp) was preceded by the removal; the second
    // statx (call 5) was followed by the re-creation.
    assert!(host.stat_path(Path::new("/tmp/a/f")).is_some());
    let f = host.statx(Base::Dir(&a.fd), Path::new("f")).unwrap();
    assert_eq!(f.size, Some(0), "a new, empty file");
    assert!(host.unfired_events().is_empty());
}

#[test]
fn a_process_opening_a_file_mid_run_shows_in_its_fds() {
    let host = host(&tmp_yaml(
        "processes:\n  - {pid: 7, cwd: /tmp/a}\nevents:\n  - {at: {op: pids, when: after}, open: {pid: 7, path: /tmp/a/f}}\n",
    ));
    assert_eq!(host.pids().unwrap(), [Pid(7), host.own_pid()]);
    let fds = host.proc_fds(Pid(7)).unwrap();
    assert_eq!(fds.len(), 1);
    assert_eq!(fds[0].fd, 3);
    assert_eq!(fds[0].target.text, "/tmp/a/f");
}

#[test]
fn the_harness_fails_a_scenario_whose_event_never_fired() {
    let model = parse_yaml(&tmp_yaml(
        "events:\n  - {at: {op: unlinkat, when: before}, remove: /tmp/a/f}\n",
    ))
    .unwrap();
    let message = failure(|| {
        run(&model, |_| Outcome {
            exit: 0,
            skipped: Vec::new(),
        });
    });
    assert!(message.contains("never fired"), "{message}");
}

#[test]
fn the_harness_fails_a_scenario_whose_outcome_differs_from_expect() {
    let model = parse_yaml(&tmp_yaml("expect: {exit: 1}\n")).unwrap();
    let message = failure(|| {
        run(&model, |_| Outcome {
            exit: 0,
            skipped: Vec::new(),
        });
    });
    assert!(message.contains("expect.exit"), "{message}");
}

// ---- The virtual clock ----

#[test]
fn time_moves_only_by_sleeping_waiting_and_advance_events() {
    let host = host(&tmp_yaml(
        "events:\n  - {at: {op: now, nth: 3, when: before}, advance: 1h}\n",
    ));
    let t0 = host.now();
    assert_eq!(t0.0, UPTIME_AT_START);
    dir(&host, "/tmp/a");
    assert_eq!(host.now(), t0, "calls take no time");
    host.sleep(Duration::from_millis(250));
    // A suspend: CLOCK_BOOTTIME includes it.
    assert_eq!(
        host.now().saturating_since(t0),
        Duration::from_secs(3600) + Duration::from_millis(250)
    );
}

#[test]
fn timed_events_fire_in_order_during_a_sleep() {
    let host = host(&tmp_yaml(
        "events:\n  - {at: {elapsed: 2s}, create: /tmp/a/two}\n  - {at: {elapsed: 1s}, create: /tmp/a/one}\n  - {at: {elapsed: 9s}, create: /tmp/a/nine}\n",
    ));
    host.sleep(Duration::from_secs(5));
    let mtime = |name: &str| {
        host.stat_path(&Path::new("/tmp/a").join(name))
            .and_then(|s| s.mtime)
            .map(|t| t.secs)
    };
    let start = 1_792_033_200; // 2026-10-15T03:00:00Z
    assert_eq!(mtime("one"), Some(start + 1));
    assert_eq!(mtime("two"), Some(start + 2));
    assert_eq!(mtime("nine"), None);
    assert_eq!(host.unfired_events(), [2]);
}

#[test]
fn the_kernel_release_is_modelled() {
    let current = host(&tmp_yaml(""));
    assert!(require_supported(&current).is_ok());
    let old = host(&tmp_yaml("kernel: 5.4.0-150-generic\n"));
    assert!(require_supported(&old).is_err());
}

// ---- /proc readers ----

#[test]
fn proc_readers_answer_from_the_process_table() {
    let host = host(&tmp_yaml(
        "  /usr/bin/tool: {}\nprocesses:\n  - pid: 7\n    exe: /usr/bin/tool\n    cwd: /tmp/a/sub\n    environ: [TMPDIR=/tmp/a, A=b=c, odd]\n    maps: [/usr/bin/tool]\n    fds: [{fd: 5, path: /tmp/a/f}]\n",
    ));
    let pid = Pid(7);
    let cwd = host.proc_links(pid, ProcLink::Cwd).unwrap();
    assert_eq!(cwd.len(), 1);
    assert_eq!(cwd[0].text, "/tmp/a/sub");
    assert_eq!(
        cwd[0].stat.dev_ino(),
        host.stat_path(Path::new("/tmp/a/sub")).unwrap().dev_ino()
    );
    assert_eq!(host.proc_links(pid, ProcLink::Root).unwrap()[0].text, "/");
    assert_eq!(
        host.proc_links(pid, ProcLink::Exe).unwrap()[0].text,
        "/usr/bin/tool"
    );
    let env = host.proc_environ(pid).unwrap();
    assert_eq!(env.get("TMPDIR"), Some(OsStr::new("/tmp/a")));
    assert_eq!(env.get("A"), Some(OsStr::new("b=c")));
    assert_eq!(env.without_equals().collect::<Vec<_>>(), ["odd"]);
    let maps = host.proc_maps(pid).unwrap();
    assert_eq!(
        Some((maps[0].dev, maps[0].ino)),
        host.stat_path(Path::new("/usr/bin/tool"))
            .and_then(|s| s.dev_ino())
            .map(|d| (d.dev, d.ino))
    );
    assert_eq!(host.proc_fds(pid).unwrap()[0].fd, 5);
}

#[test]
fn unreadable_and_gone_processes_answer_with_their_error() {
    let host = host(&tmp_yaml(
        "processes:\n  - {pid: 7, proc: denied}\n  - {pid: 8, proc: failed}\n  - {pid: 9}\nevents:\n  - {at: {op: proc_fds, path: /proc/9/fd, when: before}, exit: 9}\n",
    ));
    assert!(matches!(host.proc_fds(Pid(7)), Err(ProcError::Denied(e)) if e.is(Errno::EACCES)));
    assert!(matches!(host.proc_maps(Pid(8)), Err(ProcError::Failed(_))));
    assert!(matches!(host.proc_fds(Pid(9)), Err(ProcError::Gone)));
    assert_eq!(host.pids().unwrap(), [Pid(7), Pid(8), host.own_pid()]);
    let last = host
        .trace()
        .into_iter()
        .find(|c| c.op == Op::ProcFds && c.result == "gone");
    assert!(last.is_some());
}

/// diskgc itself is among the pids (`own_pid`, 65000 unless the model
/// says otherwise); its fds are the ones the code under test holds open,
/// its other entries are not modelled (`Failed`), its status is uid 1000.
#[test]
fn diskgc_itself_is_listed_with_the_fds_it_holds() {
    let host = host(&tmp_yaml(
        "own_pid: 50
processes:
  - {pid: 7, cwd: /tmp/a}
",
    ));
    let own = host.own_pid();
    assert_eq!(own, Pid(50));
    assert_eq!(host.pids().unwrap(), [Pid(7), own]);
    assert_eq!(host.proc_fds(own).unwrap(), []);
    let a = dir(&host, "/tmp/a");
    let fds = host.proc_fds(own).unwrap();
    assert_eq!(fds.len(), 1);
    assert_eq!(fds[0].target.stat.dev_ino(), a.stat.dev_ino());
    drop(a);
    assert_eq!(host.proc_fds(own).unwrap(), []);
    assert!(matches!(host.proc_maps(own), Err(ProcError::Failed(_))));
    assert_eq!(host.proc_status(own).unwrap().uid.effective, 1000);
    let clash = diskgc_model::load::parse_yaml(&tmp_yaml(
        "own_pid: 7\nprocesses:\n  - {pid: 7, cwd: /tmp/a}\n",
    ))
    .unwrap();
    assert!(
        ModelHost::new(&clash)
            .err()
            .is_some_and(|e| e.message.contains("own pid")),
        "a process with diskgc's own pid is refused"
    );
}

/// `status` answers for every process, readable or not: ids, the command
/// name, and the owner of its fd directory (root when not dumpable).
#[test]
fn every_process_shows_its_status() {
    let host = host(&tmp_yaml(
        "processes:
  - {pid: 7, proc: denied, uid: 1000, euid: 0, gid: 100, name: sudo}
  - {pid: 8, proc: denied, non_dumpable: true, privileged: true, exe: /tmp/a/f}
",
    ));
    let s = host.proc_status(Pid(7)).unwrap();
    assert_eq!((s.uid.real, s.uid.effective, s.uid.saved), (1000, 0, 0));
    assert_eq!((s.gid.real, s.name.to_str()), (100, Some("sudo")));
    assert_eq!(s.fd_dir_owner, Some(0), "dumpable: owned by its euid 0");
    let s = host.proc_status(Pid(8)).unwrap();
    assert_eq!((s.uid.effective, s.fd_dir_owner), (1000, Some(0)));
    assert_eq!((s.name.to_str(), s.cap_permitted), (Some("f"), 1 << 35));
}

/// A fork copies the process, sharing its open file descriptions: the
/// child keeps a file its parent closes afterwards, and a lock held through
/// a shared description stays held.
#[test]
fn a_forked_child_shares_its_parents_open_files() {
    let host = host(&tmp_yaml(
        "processes:
  - {pid: 7, cwd: /tmp/a, fds: [{path: /tmp/a/f}]}
events:
  - {at: {op: pids, when: after}, fork: {pid: 7, child: 9}}
  - {at: {op: pids, when: after}, close: {pid: 7, path: /tmp/a/f}}
  - {at: {op: proc_fds, path: /proc/9/fd, when: after}, exit: 9}
",
    ));
    // The fork and the close happen right after the first listing.
    assert_eq!(host.pids().unwrap(), [Pid(7), host.own_pid()]);
    assert_eq!(host.pids().unwrap(), [Pid(7), Pid(9), host.own_pid()]);
    // The parent's only fd is closed: an empty table, Failed as on Linux.
    assert!(matches!(host.proc_fds(Pid(7)), Err(ProcError::Failed(_))));
    let fds = host.proc_fds(Pid(9)).unwrap();
    assert_eq!(fds[0].target.text, "/tmp/a/f");
    assert!(matches!(host.proc_fds(Pid(9)), Err(ProcError::Gone)));
}

/// The bound sockets: those the model marks `bound`, by device and inode.
#[test]
fn bound_sockets_are_the_ones_the_model_marks() {
    let host = host(&tmp_yaml(
        "  /tmp/s: {type: socket, bound: true}
  /tmp/t: {type: socket}
",
    ));
    let s = host
        .stat_path(Path::new("/tmp/s"))
        .unwrap()
        .dev_ino()
        .unwrap();
    let t = host
        .stat_path(Path::new("/tmp/t"))
        .unwrap()
        .dev_ino()
        .unwrap();
    let bound = host.bound_unix_sockets().unwrap();
    assert!(bound.iter().any(|b| b.is(s)) && !bound.iter().any(|b| b.is(t)));
    assert!(
        diskgc_model::load::parse_yaml(&tmp_yaml(
            "  /tmp/u: {bound: true}
"
        ))
        .ok()
        .and_then(|m| ModelHost::new(&m).err())
        .is_some_and(|e| e.message.contains("only a socket")),
        "bound on a regular file is refused"
    );
}

// ---- Derived space ----

#[test]
fn a_deleted_file_keeps_its_space_while_a_process_holds_it_open() {
    let host = host(&tmp_yaml(
        "  /tmp/big: {size: 1M}\nprocesses:\n  - {pid: 7, fds: [{path: /tmp/big, deleted: true}]}\nevents:\n  - {at: {elapsed: 1s}, exit: 7}\n",
    ));
    assert!(host.stat_path(Path::new("/tmp/big")).is_none(), "no name");
    let fds = host.proc_fds(Pid(7)).unwrap();
    assert_eq!(fds[0].target.text, "/tmp/big (deleted)");
    assert_eq!(fds[0].target.stat.nlink, Some(0));
    let held = host.space_at(Path::new("/tmp")).avail_bytes;
    host.sleep(Duration::from_secs(1));
    assert_eq!(
        host.space_at(Path::new("/tmp")).avail_bytes,
        held + (1 << 20)
    );
}

#[test]
fn hard_links_are_counted_once_and_free_nothing_until_the_last_name_goes() {
    let host = host(&tmp_yaml("  /tmp/h: {link: /tmp/a/f}\n"));
    let tmp = dir(&host, "/tmp");
    let before = host.space_at(Path::new("/tmp"));
    assert_eq!(
        before.size_bytes - before.avail_bytes,
        // 10 KiB file once, plus /tmp, /tmp/a and /tmp/a/sub.
        3 * 4096 + 3 * 4096
    );
    host.statx(Base::Dir(&tmp.fd), Path::new("h")).unwrap();
    host.unlinkat(&tmp.fd, OsStr::new("h"), Unlink::NonDir)
        .unwrap();
    assert_eq!(host.space_at(Path::new("/tmp")), before);
}

#[test]
fn sockets_and_fifos_cannot_be_opened_for_reading() {
    let host = host(&tmp_yaml(
        "  /tmp/s: {type: socket}\n  /tmp/p: {type: fifo}\n",
    ));
    let tmp = dir(&host, "/tmp");
    let open = |name: &str, access| {
        open_beneath(
            &host,
            &tmp.fd,
            tmp.mount.id,
            Path::new(name),
            access,
            Want::Any,
        )
    };
    let err = open("s", Access::Read).unwrap_err();
    assert!(
        matches!(err, ResolveError::Host(ref e) if e.is(Errno::ENXIO)),
        "{err}"
    );
    assert!(open("s", Access::Path).is_ok());
    assert!(open("p", Access::Path).is_ok());
    let message = failure(|| {
        let _ = open("p", Access::Read);
    });
    assert!(message.contains("FIFO"), "{message}");
}

#[test]
fn remove_of_a_held_directory_empties_it_like_rm_r() {
    // rm -r unlinks every entry: an open directory keeps only itself, its
    // entries are gone and their space is freed (task-5's concurrent
    // rm -rf of a candidate).
    let host = host(&tmp_yaml(
        "  /tmp/a/sub/g: {size: 1M}\nevents:\n  - {at: {op: statx, path: /tmp/a/f, when: before}, remove: /tmp/a}\n",
    ));
    let a = dir(&host, "/tmp/a");
    let sub = dir(&host, "/tmp/a/sub");
    let before = host.space_at(Path::new("/tmp")).avail_bytes;
    let err = host
        .statx(Base::Dir(&a.fd), Path::new("f"))
        .expect_err("f was removed");
    assert_eq!(err.kernel_errno(), Some(Errno::ENOENT));
    let err = host
        .statx(Base::Dir(&sub.fd), Path::new("g"))
        .expect_err("g was removed");
    assert_eq!(err.kernel_errno(), Some(Errno::ENOENT));
    assert_eq!(
        host.statx(Base::Dir(&a.fd), Path::new("")).unwrap().nlink,
        Some(0),
        "the held directory itself is detached"
    );
    // f (12 KiB allocated) and g (1 MiB) are freed; the held a and sub
    // keep their own block each.
    assert_eq!(
        host.space_at(Path::new("/tmp")).avail_bytes,
        before + 3 * 4096 + (1 << 20)
    );
}

#[test]
fn an_empty_proc_answer_is_failed_as_linuxhost_reports_it() {
    // LinuxHost never answers a live process's fd table, maps, environment
    // or exe link with nothing (tasks.rs: an empty read can be one bound to
    // a dead leader), so neither does the model.
    let host = host(&tmp_yaml("processes:\n  - {pid: 7}\n"));
    let pid = Pid(7);
    let failed = |r: Result<usize, ProcError>, what: &str| {
        assert!(
            matches!(&r, Err(ProcError::Failed(e)) if e.to_string().contains("empty")),
            "{what}: {r:?}"
        );
    };
    failed(host.proc_fds(pid).map(|v| v.len()), "fds");
    failed(host.proc_maps(pid).map(|v| v.len()), "maps");
    failed(host.proc_environ(pid).map(|e| e.vars().count()), "environ");
    failed(host.proc_links(pid, ProcLink::Exe).map(|v| v.len()), "exe");
    // cwd and root always exist (default /).
    assert_eq!(host.proc_links(pid, ProcLink::Cwd).unwrap().len(), 1);
}
