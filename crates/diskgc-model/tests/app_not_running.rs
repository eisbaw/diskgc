//! Chromium SingletonLock is checked through the Host port. Its target is
//! never interpreted: a PID may be live in a different process namespace.

use std::path::{Path, PathBuf};

use diskgc_core::engine::{Reason, Rules};
use diskgc_core::fs_kind::{self, Find, Judge};
use diskgc_core::inuse::{Checker, REUSE_LIMIT};
use diskgc_core::outcome::ReclaimEnd;
use diskgc_core::profile::{EntryType, Where};
use diskgc_model::ModelHost;

fn host(lock: &str, processes: &str, events: &str) -> ModelHost {
    ModelHost::from_yaml(&format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
hostname: model-host
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}}
tree:
  /usr/bin/tool: {{}}
  /tmp/browser-cache: {{.: {{mtime: 2026-10-01T00:00:00Z}}, f: {{size: 4K}}}}
  /tmp/.config/chromium: {{.: {{}}}}
  /home/user/.config/chromium: {{.: {{}}}}
{lock}
{processes}
{events}"#
    ))
    .unwrap_or_else(|error| panic!("model fixture: {error}"))
}

fn verdict(host: &ModelHost) -> Option<Reason> {
    let where_ = Where {
        app_not_running: Some("/home/user/.config/chromium/SingletonLock".into()),
        ..Where::default()
    };
    let find = Find::new(&["browser-cache".into()], EntryType::Dir, &where_, 1000);
    let dev = host.stat_path(Path::new("/tmp")).unwrap().dev;
    let (plan, stop) = fs_kind::plan(
        host,
        &[PathBuf::from("/tmp")],
        &find,
        dev,
        &Rules::new(&[1000]),
        diskgc_core::profile::IdleWindow::default(),
        None,
        &mut |_| {},
    );
    assert!(stop.is_none(), "{stop:?}");
    assert_eq!(plan.judged.len(), 1, "{plan:?}");
    plan.judged[0].2.as_ref().err().map(|skip| skip.reason)
}

const LOCK: &str = "  /home/user/.config/chromium/SingletonLock: {symlink: model-host-42}";
const LIVE: &str = "processes:\n  - {pid: 42, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{path: /usr/bin/tool}], environ: [PATH=/usr/bin]}";

#[test]
fn missing_lock_permits_but_a_stale_pid_cannot_prove_the_browser_stopped() {
    assert_eq!(verdict(&host("", "", "")), None);
    assert_eq!(verdict(&host(LOCK, "", "")), Some(Reason::InUse));
}

#[test]
fn a_present_lock_blocks_regardless_of_which_process_has_its_pid() {
    // A matching host PID does not prove that the lock belongs to this
    // process namespace or that this process is the browser.
    assert_eq!(verdict(&host(LOCK, LIVE, "")), Some(Reason::InUse));
}

#[test]
fn relative_lock_path_is_bound_to_the_source_root() {
    let lock = "  /tmp/.config/chromium/SingletonLock: {symlink: model-host-42}";
    let host = host(lock, LIVE, "");
    let where_ = Where {
        app_not_running: Some(".config/chromium/SingletonLock".into()),
        ..Where::default()
    };
    let find = Find::new(&["browser-cache".into()], EntryType::Dir, &where_, 1000);
    let owners = [1000];
    let (plan, stop) = fs_kind::plan(
        &host,
        &[PathBuf::from("/tmp")],
        &find,
        host.stat_path(Path::new("/tmp")).unwrap().dev,
        &Rules::new(&owners),
        diskgc_core::profile::IdleWindow::default(),
        None,
        &mut |_| {},
    );
    assert!(stop.is_none(), "{stop:?}");
    assert_eq!(plan.judged.len(), 1);
    assert_eq!(
        plan.judged[0].2.as_ref().err().map(|skip| skip.reason),
        Some(Reason::InUse)
    );
}

#[test]
fn foreign_malformed_and_non_symlink_locks_fail_closed() {
    for target in [
        "other-host-42",
        "model-host-nope",
        "model-host-0",
        "broken",
        "/model-host-42",
    ] {
        let lock = format!("  /home/user/.config/chromium/SingletonLock: {{symlink: {target}}}");
        assert_eq!(
            verdict(&host(&lock, "", "")),
            Some(Reason::InUse),
            "{target}"
        );
    }
    let regular = "  /home/user/.config/chromium/SingletonLock: {}";
    assert_eq!(verdict(&host(regular, "", "")), Some(Reason::InUse));
}

#[test]
fn unreadable_lock_fails_closed() {
    let event = "events:\n  - {at: {op: statx, path: /home/user/.config/chromium/SingletonLock, when: before}, fail: EACCES}";
    assert_eq!(verdict(&host(LOCK, "", event)), Some(Reason::Unreadable));
}

#[test]
fn lock_created_after_first_judgement_blocks_deletion() {
    let events = "events:\n  - {at: {op: pids, when: after}, create: /home/user/.config/chromium/SingletonLock}";
    let host = host("", "", events);
    let where_ = Where {
        app_not_running: Some("/home/user/.config/chromium/SingletonLock".into()),
        ..Where::default()
    };
    let find = Find::new(&["browser-cache".into()], EntryType::Dir, &where_, 1000);
    let roots = [PathBuf::from("/tmp")];
    let (found, stopped) = fs_kind::discover(&host, &roots, &find, None);
    assert!(stopped.is_none(), "{stopped:?}");
    let (_, found) = found.candidates.into_iter().next().expect("candidate");
    let found = found.expect("found candidate");
    let owners = [1000];
    let rules = Rules::new(&owners);
    let judge = Judge {
        find: &find,
        rules: &rules,
        idle: diskgc_core::profile::IdleWindow::default(),
        dev: host.stat_path(Path::new("/tmp")).unwrap().dev,
    };
    let mut checker = Checker::new(&rules, REUSE_LIMIT);
    let outcome = fs_kind::reclaim(&host, &judge, &found, false, &mut checker, None);
    assert!(
        matches!(outcome.end, ReclaimEnd::Skipped(ref skip) if skip.reason == Reason::InUse),
        "{outcome:?}\n{:#?}",
        host.trace()
    );
    assert!(host.stat_path(Path::new("/tmp/browser-cache")).is_some());
}
