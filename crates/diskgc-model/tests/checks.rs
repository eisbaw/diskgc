//! ModelHost checks how it is called (task-14 AC #2 and #4;
//! docs/architecture.md, The Host port), and locks live exactly as long as
//! their fds.

mod common;

use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

use common::{dir, failure, host, tmp_yaml};
use diskgc_core::host::{
    Access, Base, Delete, Errno, Host, LockMode, OpenHow, TryLock, Unlink, WaitLock,
};
use diskgc_core::resolve::{Want, beneath_how, mount_point_how, open_beneath};
use diskgc_model::Holder;
use diskgc_model::spec::Op;

const CHECK_FAILED: &str = "ModelHost call check failed";

/// A flag the check must name, and how to drop it from correct arguments.
type FlagCase = (&'static str, fn(&mut OpenHow));

// ---- openat2 flags (AC #2) ----

#[test]
fn the_cores_resolution_passes_the_call_checks() {
    let host = host(&tmp_yaml(""));
    let a = dir(&host, "/tmp/a");
    open_beneath(
        &host,
        &a.fd,
        a.mount.id,
        Path::new("sub"),
        Access::Read,
        Want::Directory,
    )
    .expect("opens");
    let hows: Vec<OpenHow> = host.trace().iter().filter_map(|c| c.how).collect();
    assert_eq!(
        hows,
        [
            mount_point_how(),
            beneath_how(Access::Path, Want::Directory),
            beneath_how(Access::Read, Want::Directory),
        ]
    );
}

#[test]
fn openat2_below_a_directory_without_a_required_flag_fails_the_test() {
    let cases: [FlagCase; 4] = [
        ("RESOLVE_BENEATH", |how| how.resolve.beneath = false),
        ("RESOLVE_NO_SYMLINKS", |how| how.resolve.no_symlinks = false),
        ("RESOLVE_NO_XDEV", |how| how.resolve.no_xdev = false),
        ("O_NOFOLLOW", |how| how.nofollow = true),
    ];
    for (flag, change) in cases {
        let host = host(&tmp_yaml(""));
        let a = dir(&host, "/tmp/a");
        let mut how = beneath_how(Access::Path, Want::Any);
        change(&mut how);
        let message = failure(|| {
            let _ = host.openat2(Base::Dir(&a.fd), Path::new("f"), how);
        });
        assert!(
            message.contains(CHECK_FAILED) && message.contains(flag),
            "{flag}: {message}"
        );
    }
}

#[test]
fn openat2_of_a_mount_point_without_a_required_flag_fails_the_test() {
    let cases: [FlagCase; 3] = [
        ("RESOLVE_NO_SYMLINKS", |how| how.resolve.no_symlinks = false),
        ("O_NOFOLLOW", |how| how.nofollow = false),
        ("O_DIRECTORY", |how| how.directory = false),
    ];
    for (flag, change) in cases {
        let host = host(&tmp_yaml(""));
        let mut how = mount_point_how();
        change(&mut how);
        let message = failure(|| {
            let _ = host.openat2(Base::Root, Path::new("/tmp"), how);
        });
        assert!(
            message.contains(CHECK_FAILED) && message.contains(flag),
            "{flag}: {message}"
        );
    }
}

#[test]
fn absolute_paths_below_a_directory_and_relative_ones_from_the_root_fail_the_test() {
    let host = host(&tmp_yaml(""));
    let a = dir(&host, "/tmp/a");
    let message = failure(|| {
        let _ = host.openat2(
            Base::Dir(&a.fd),
            Path::new("/tmp/a/f"),
            beneath_how(Access::Path, Want::Any),
        );
    });
    assert!(message.contains("absolute path"), "{message}");
    let message = failure(|| {
        let _ = host.openat2(Base::Root, Path::new("tmp"), mount_point_how());
    });
    assert!(message.contains("relative path"), "{message}");
}

#[test]
fn statx_of_more_than_one_name_or_from_the_root_fails_the_test() {
    let host = host(&tmp_yaml(""));
    let tmp = dir(&host, "/tmp");
    let message = failure(|| {
        let _ = host.statx(Base::Dir(&tmp.fd), Path::new("a/f"));
    });
    assert!(message.contains("single name"), "{message}");
    let message = failure(|| {
        let _ = host.statx(Base::Root, Path::new("/tmp/a"));
    });
    assert!(message.contains("from the root"), "{message}");
}

// ---- unlinkat needs an identity check (AC #4) ----

#[test]
fn unlinkat_without_a_preceding_identity_check_fails_the_test() {
    let host = host(&tmp_yaml(""));
    let a = dir(&host, "/tmp/a");
    let message = failure(|| {
        let _ = host.unlinkat(&a.fd, OsStr::new("f"), Unlink::NonDir);
    });
    assert!(
        message.contains(CHECK_FAILED) && message.contains("identity check"),
        "{message}"
    );
    assert!(
        host.stat_path(Path::new("/tmp/a/f")).is_some(),
        "not removed"
    );
}

#[test]
fn an_identity_check_of_another_name_does_not_count() {
    let host = host(&tmp_yaml(""));
    let a = dir(&host, "/tmp/a");
    host.statx(Base::Dir(&a.fd), Path::new("sub"))
        .expect("stat");
    let message = failure(|| {
        let _ = host.unlinkat(&a.fd, OsStr::new("f"), Unlink::NonDir);
    });
    assert!(message.contains("identity check"), "{message}");
}

#[test]
fn an_identity_check_covers_one_unlinkat() {
    let host = host(&tmp_yaml(""));
    let a = dir(&host, "/tmp/a");
    host.statx(Base::Dir(&a.fd), Path::new("sub"))
        .expect("stat");
    host.unlinkat(&a.fd, OsStr::new("sub"), Unlink::Dir)
        .expect("removes");
    let message = failure(|| {
        let _ = host.unlinkat(&a.fd, OsStr::new("sub"), Unlink::Dir);
    });
    assert!(message.contains("identity check"), "{message}");
}

#[test]
fn an_identity_check_from_an_earlier_walk_does_not_count() {
    // The walk statted the entry long ago, through another fd; the delete
    // must stat it again right before unlinking (docs/safety.md, Deletion).
    let host = host(&tmp_yaml(""));
    let walk = dir(&host, "/tmp/a");
    host.statx(Base::Dir(&walk.fd), Path::new("f"))
        .expect("stat");
    drop(walk);
    host.sleep(Duration::from_secs(3600));
    let delete = dir(&host, "/tmp/a");
    let message = failure(|| {
        let _ = host.unlinkat(&delete.fd, OsStr::new("f"), Unlink::NonDir);
    });
    assert!(message.contains("identity check"), "{message}");
}

#[test]
fn rmdir_after_descending_needs_a_fresh_identity_check() {
    let host = host(&tmp_yaml(""));
    let a = dir(&host, "/tmp/a");
    host.statx(Base::Dir(&a.fd), Path::new("sub"))
        .expect("stat");
    let sub = open_beneath(
        &host,
        &a.fd,
        a.mount.id,
        Path::new("sub"),
        Access::Read,
        Want::Directory,
    )
    .expect("descends");
    drop(sub);
    let message = failure(|| {
        let _ = host.unlinkat(&a.fd, OsStr::new("sub"), Unlink::Dir);
    });
    assert!(message.contains("identity check"), "{message}");
    // Statted again right before: allowed, through any fd of the directory.
    let again = dir(&host, "/tmp/a");
    host.statx(Base::Dir(&a.fd), Path::new("sub"))
        .expect("stat");
    host.unlinkat(&again.fd, OsStr::new("sub"), Unlink::Dir)
        .expect("removes");
}

#[test]
fn unlinkat_after_an_identity_check_removes_the_name_and_frees_its_space() {
    let host = host(&tmp_yaml(""));
    let a = dir(&host, "/tmp/a");
    let before = host.space_at(Path::new("/tmp")).avail_bytes;
    let stat = host.statx(Base::Dir(&a.fd), Path::new("f")).expect("stat");
    host.unlinkat(&a.fd, OsStr::new("f"), Unlink::NonDir)
        .expect("removes");
    assert!(host.stat_path(Path::new("/tmp/a/f")).is_none());
    // 10 KiB allocate three 4 KiB blocks.
    assert_eq!(
        host.space_at(Path::new("/tmp")).avail_bytes,
        before + 3 * 4096
    );
    let unlink = host.trace().pop().expect("a call");
    assert_eq!(unlink.op, Op::Unlinkat);
    assert_eq!(unlink.object, stat.dev_ino());
}

#[test]
fn unlinkat_answers_like_the_kernel() {
    let host = host(&tmp_yaml(""));
    let a = dir(&host, "/tmp/a");
    let tmp = dir(&host, "/tmp");
    let errno = |fd, name: &str, what| {
        host.statx(Base::Dir(fd), Path::new(name)).expect("stat");
        host.unlinkat(fd, OsStr::new(name), what)
            .expect_err("refused")
            .kernel_errno()
    };
    assert_eq!(errno(&a.fd, "sub", Unlink::NonDir), Some(Errno::EISDIR));
    assert_eq!(errno(&a.fd, "f", Unlink::Dir), Some(Errno::ENOTDIR));
    assert_eq!(errno(&tmp.fd, "a", Unlink::Dir), Some(Errno::ENOTEMPTY));
}

#[test]
fn unlinkat_of_a_path_fails_the_test() {
    let host = host(&tmp_yaml(""));
    let tmp = dir(&host, "/tmp");
    let message = failure(|| {
        let _ = host.unlinkat(&tmp.fd, OsStr::new("a/f"), Unlink::NonDir);
    });
    assert!(message.contains("single name"), "{message}");
}

#[test]
fn fchmod_sets_permission_bits_of_directories_only() {
    let host = host(&tmp_yaml(""));
    let a = dir(&host, "/tmp/a");
    host.fchmod(&a.fd, 0o700).expect("fchmod");
    assert_eq!(
        host.stat_path(Path::new("/tmp/a")).and_then(|s| s.perm),
        Some(0o700)
    );
    let f = open_f(&host);
    let err = host.fchmod(&f, 0o600).expect_err("a file");
    assert!(err.is(Errno::ENOTDIR), "{err}");
    let message = failure(|| {
        let _ = host.fchmod(&a.fd, 0o40_700);
    });
    assert!(message.contains(CHECK_FAILED), "{message}");
}

#[test]
fn a_fail_event_makes_its_call_fail_without_acting() {
    let host = host(&tmp_yaml(
        "events:\n  - {at: {op: unlinkat, path: /tmp/a/f, when: before}, fail: EROFS}\n",
    ));
    let a = dir(&host, "/tmp/a");
    host.statx(Base::Dir(&a.fd), Path::new("f")).expect("stat");
    let err = host
        .unlinkat(&a.fd, OsStr::new("f"), Unlink::NonDir)
        .expect_err("injected");
    assert!(err.is(Errno::EROFS), "{err}");
    assert!(host.stat_path(Path::new("/tmp/a/f")).is_some(), "it acted");
    // Once: the next call acts again.
    host.statx(Base::Dir(&a.fd), Path::new("f")).expect("stat");
    host.unlinkat(&a.fd, OsStr::new("f"), Unlink::NonDir)
        .expect("unlinks");
}

// ---- Locks live exactly as long as their fds (AC #2) ----

fn open_f(host: &diskgc_model::ModelHost) -> diskgc_model::ModelFd {
    let a = dir(host, "/tmp/a");
    open_beneath(
        host,
        &a.fd,
        a.mount.id,
        Path::new("f"),
        Access::Read,
        Want::Any,
    )
    .expect("opens")
    .0
}

#[test]
fn a_lock_lives_exactly_as_long_as_its_fd() {
    let host = host(&tmp_yaml(""));
    let f = Path::new("/tmp/a/f");
    let TryLock::Acquired(locked) = host.flock_try(open_f(&host), LockMode::Exclusive).unwrap()
    else {
        panic!("the lock is free")
    };
    assert_eq!(host.locks_on(f), [(Holder::Own, LockMode::Exclusive)]);
    // Another open file description of the same file is refused.
    let TryLock::Busy(other) = host.flock_try(open_f(&host), LockMode::Shared).unwrap() else {
        panic!("held exclusively")
    };
    drop(locked);
    assert_eq!(host.locks_on(f), []);
    let TryLock::Acquired(_shared) = host.flock_try(other, LockMode::Shared).unwrap() else {
        panic!("released when its fd closed")
    };
}

#[test]
fn shared_locks_coexist_and_keep_out_an_exclusive_one() {
    let host = host(&tmp_yaml(""));
    let shared = |fd| match host.flock_try(fd, LockMode::Shared).unwrap() {
        TryLock::Acquired(locked) => locked,
        TryLock::Busy(_) => panic!("shared locks coexist"),
    };
    let _one = shared(open_f(&host));
    let _two = shared(open_f(&host));
    assert!(matches!(
        host.flock_try(open_f(&host), LockMode::Exclusive).unwrap(),
        TryLock::Busy(_)
    ));
}

#[test]
fn flock_on_an_o_path_fd_is_ebadf() {
    let host = host(&tmp_yaml(""));
    let a = dir(&host, "/tmp/a");
    let err = host
        .flock_try(a.fd, LockMode::Exclusive)
        .expect_err("O_PATH fds cannot be locked");
    assert_eq!(err.kernel_errno(), Some(Errno::EBADF));
}

/// Pid 7 holds an exclusive lock on /tmp/a/f; `events` are appended.
fn held_by_7(events: &str) -> String {
    tmp_yaml(&format!(
        "processes:\n  - {{pid: 7}}\nlocks:\n  - {{path: /tmp/a/f, holder: 7, mode: exclusive}}\n{events}"
    ))
}

#[test]
fn a_process_lock_lives_as_long_as_the_process_holds_its_fd() {
    let host = host(&held_by_7(
        "events:\n  - {at: {elapsed: 5s}, close: {pid: 7, path: /tmp/a/f}}\n",
    ));
    let f = Path::new("/tmp/a/f");
    assert_eq!(
        host.locks_on(f),
        [(Holder::Process(7), LockMode::Exclusive)]
    );
    let TryLock::Busy(fd) = host.flock_try(open_f(&host), LockMode::Shared).unwrap() else {
        panic!("held by 7")
    };
    host.sleep(Duration::from_secs(5));
    assert_eq!(host.locks_on(f), []);
    assert!(matches!(
        host.flock_try(fd, LockMode::Shared).unwrap(),
        TryLock::Acquired(_)
    ));
}

#[test]
fn a_process_lock_ends_when_the_process_exits() {
    let host = host(&held_by_7(
        "events:\n  - {at: {op: now, nth: 1, when: before}, exit: 7}\n",
    ));
    assert_eq!(host.locks_on(Path::new("/tmp/a/f")).len(), 1);
    host.now();
    assert_eq!(host.locks_on(Path::new("/tmp/a/f")), []);
    assert!(matches!(
        host.flock_try(open_f(&host), LockMode::Exclusive).unwrap(),
        TryLock::Acquired(_)
    ));
}

// ---- Waiting on the virtual clock ----

#[test]
fn flock_wait_jumps_to_the_event_that_releases_the_lock() {
    let host = host(&held_by_7("events:\n  - {at: {elapsed: 7s}, exit: 7}\n"));
    let start = host.now();
    let deadline = start.checked_add(Duration::from_secs(60));
    let got = host
        .flock_wait(open_f(&host), LockMode::Exclusive, deadline)
        .unwrap();
    assert!(matches!(got, WaitLock::Acquired(_)));
    assert_eq!(host.now().saturating_since(start), Duration::from_secs(7));
}

#[test]
fn flock_wait_gives_up_at_the_deadline() {
    let host = host(&held_by_7(""));
    let start = host.now();
    let got = host
        .flock_wait(
            open_f(&host),
            LockMode::Exclusive,
            start.checked_add(Duration::from_secs(3)),
        )
        .unwrap();
    assert!(matches!(got, WaitLock::DeadlinePassed(_)));
    assert_eq!(host.now().saturating_since(start), Duration::from_secs(3));
    let flock = host
        .trace()
        .into_iter()
        .find(|c| c.op == Op::Flock)
        .unwrap();
    assert_eq!(flock.result, "deadline passed");
}

#[test]
fn flock_wait_without_a_deadline_on_a_lock_nothing_releases_fails_the_test() {
    let host = host(&held_by_7(""));
    let message = failure(|| {
        let _ = host.flock_wait(open_f(&host), LockMode::Exclusive, None);
    });
    assert!(message.contains("block forever"), "{message}");
}
