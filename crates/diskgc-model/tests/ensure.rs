//! The minimal `ensure` (diskgc-core `executor`, task-10) on ModelHost:
//! thresholds, the host lock, the sandbox gate, privileges, the exit codes,
//! the order of a `cargo-target` tier and its goal, and `--dry-run`. Every
//! call is on the trace, so "without taking the lock" and "deletes
//! nothing" are asserted on what was called, not on what was reported.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::hostlock::TMPFILES_LINES;
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::report::{Report, Status};
use diskgc_core::units::{Size, parse_size};
use diskgc_model::spec::Op;
use diskgc_model::{Call, Holder, ModelHost};

const GIB: u64 = 1 << 30;
const SRC: &str = "/home/mark/src";
const OLD: &str = "/home/mark/src/old/target";
const NEW: &str = "/home/mark/src/new/target";
const HOST_LOCK: &str = "/run/diskgc/lock";

/// A classic profile directory whose every time is `at`, with a `size`
/// file in `deps/`.
fn profile(size: &str, at: &str) -> String {
    format!(
        "{{.: {{mtime: {at}}}, .cargo-lock: {{mtime: {at}}}, \
         .fingerprint: {{.: {{mtime: {at}}}, p-1: {{.: {{mtime: {at}}}, bin-p: {{mtime: {at}}}}}}}, \
         deps: {{.: {{mtime: {at}}}, p-1: {{size: {size}, mtime: {at}}}}}}}"
    )
}

/// `/home` (100G, 80G used by others) holds two attached Cargo targets
/// below `SRC`: `old` (8G, built 2026-10-01) and `new` (4G, built
/// 2026-10-14); `/run` holds the provisioned host lock when `lock`; the
/// clock is 2026-10-15. `extra`: further top-level keys.
fn yaml(lock: bool, extra: &str) -> String {
    let lock = if lock {
        "  /run/diskgc/lock: {uid: 0, gid: 0}\n"
    } else {
        ""
    };
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 2, at: /home, fs: ext4, dev: "254:2", size: 100G, other_used: 80G, parent: 1}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
  - {{id: 4, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}}
tree:
{lock}  /home/mark/src/old/Cargo.toml: {{}}
  /home/mark/src/old/target:
    CACHEDIR.TAG: {{cachedir_tag: true}}
    debug: {old}
  /home/mark/src/new/Cargo.toml: {{}}
  /home/mark/src/new/target:
    CACHEDIR.TAG: {{cachedir_tag: true}}
    debug: {new}
{extra}"#,
        old = profile("8G", "2026-10-01T00:00:00Z"),
        new = profile("4G", "2026-10-14T00:00:00Z"),
    )
}

fn size(text: &str) -> Size {
    parse_size(text).unwrap_or_else(|e| panic!("{e}"))
}

/// `ensure --min-free MIN [--target-free TARGET] --select SELECT --roots
/// ROOTS... PATH` with a 10-minute timeout.
fn request(select: &str, roots: &[&str], path: &str, min: &str, target: Option<&str>) -> Request {
    let library = Library::builtin().unwrap_or_else(|e| panic!("{e}"));
    let roots: Vec<String> = roots.iter().map(|r| (*r).to_owned()).collect();
    Request {
        path: PathBuf::from(path),
        min_free: size(min),
        target_free: target.map(size),
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select(select, &library, Some(&roots), 1000).unwrap_or_else(|e| panic!("{e}")),
    }
}

fn cargo(min: &str, target: Option<&str>) -> Request {
    request("profile:cargo-target", &[SRC], SRC, min, target)
}

/// Runs `ensure` (privileges first, as the binary does); the log lines
/// are printed for a failing test.
fn ensure(host: &ModelHost, request: &Request) -> Report {
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let report = executor::ensure(host, request, &[uid], &mut |l| lines.push(l.to_owned()));
    eprintln!("log:\n  {}", lines.join("\n  "));
    report
}

fn dry_run(host: &ModelHost, request: &Request) -> Report {
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let report = executor::dry_run(host, request, &[uid], &mut |l| lines.push(l.to_owned()));
    eprintln!("log:\n  {}", lines.join("\n  "));
    report
}

fn calls(host: &ModelHost, op: Op) -> Vec<Call> {
    host.trace().into_iter().filter(|c| c.op == op).collect()
}

fn touches(call: &Call, below: &str) -> bool {
    call.path.as_deref().is_some_and(|p| p.starts_with(below))
}

/// Nothing below `/run/diskgc` was opened, statted or locked.
fn host_lock_untouched(host: &ModelHost) -> bool {
    host.trace().iter().all(|c| !touches(c, "/run/diskgc"))
}

fn unlinked_below(host: &ModelHost, below: &str) -> usize {
    calls(host, Op::Unlinkat)
        .iter()
        .filter(|c| touches(c, below) && c.result == "ok")
        .count()
}

fn exists(host: &ModelHost, path: &str) -> bool {
    host.stat_path(Path::new(path)).is_some()
}

/// An already-met ensure verifies provisioning but never takes the lock.
/// Missing and unusable lock objects produce a warning, not a false setup
/// success or an exit failure.
#[test]
fn above_min_free_warns_about_missing_or_unusable_lock_without_taking_it() {
    for (case, model, expected) in [
        (
            "missing",
            yaml(false, ""),
            "host lock /run/diskgc/lock is missing",
        ),
        (
            "fifo",
            yaml(false, "").replacen(
                "tree:\n",
                "tree:\n  /run/diskgc/lock: {type: fifo, uid: 0}\n",
                1,
            ),
            "host lock /run/diskgc/lock is unusable",
        ),
        ("ready", yaml(true, ""), ""),
    ] {
        let host = host(&model);
        let uid = executor::privileges(&host, false).unwrap();
        let mut lines = Vec::new();
        let report = executor::ensure(&host, &cargo("1G", None), &[uid], &mut |line| {
            lines.push(line.to_owned());
        });
        let output = lines.join("\n");
        assert_eq!(report.status, Status::Met, "{case}: {report:?}");
        assert_eq!(report.exit_code(), 0);
        if case == "ready" {
            assert!(!output.contains("host lock is not ready"), "{output}");
        } else {
            assert!(
                output.contains("warning: host lock is not ready"),
                "{output}"
            );
            assert!(output.contains(expected), "{output}");
        }
        assert!(
            calls(&host, Op::Flock).is_empty(),
            "{case}: {:#?}",
            host.trace()
        );
        assert!(calls(&host, Op::Unlinkat).is_empty(), "{case}");
        assert_eq!(report.avail_after_bytes, report.avail_before_bytes);
        assert_eq!(report.mount.as_deref(), Some(Path::new("/home")));
        assert_eq!(report.summary(), None, "nothing needed doing: silent");
    }
}

/// AC #2: below `--min-free` with no `/run/diskgc/lock`: exit 2, the
/// message carries both tmpfiles lines, nothing is deleted or locked.
#[test]
fn a_missing_host_lock_is_exit_2_with_the_tmpfiles_lines() {
    let host = host(&yaml(false, ""));
    let report = ensure(&host, &cargo("12G", None));
    assert_eq!(report.status, Status::UsageError);
    assert_eq!(report.exit_code(), 2);
    let message = &report.errors[0].message;
    for line in TMPFILES_LINES {
        assert!(message.contains(line), "{message}");
    }
    assert!(calls(&host, Op::Flock).is_empty());
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

/// The tier's `oldest` order and the goal: the 8G `old` target is
/// reclaimed first, under the host lock (taken, made inheritable, and
/// held for every unlink), and `new` is not touched once `--min-free` is
/// observed. Lock files stay; the lock is released at the end.
#[test]
fn the_oldest_target_goes_first_and_the_run_stops_at_the_goal() {
    let host = host(&yaml(true, ""));
    let report = ensure(&host, &cargo("12G", None));
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(report.exit_code(), 0);
    assert_eq!(report.actions.len(), 1, "{:#?}", report.actions);
    assert_eq!(report.actions[0].candidate, Path::new(OLD));
    assert!(report.actions[0].avail_delta_bytes.expect("measured") > 7 * GIB as i64);
    assert_eq!(report.target_met(), Some(true));
    assert!(report.skipped.is_empty() && report.errors.is_empty());
    assert!(!exists(&host, &format!("{OLD}/debug/deps")));
    assert!(exists(&host, &format!("{OLD}/debug/.cargo-lock")));
    assert_eq!(unlinked_below(&host, NEW), 0);
    assert!(exists(&host, &format!("{NEW}/debug/deps/p-1")));

    let trace = host.trace();
    let locked = trace
        .iter()
        .position(|c| c.op == Op::Flock && touches(c, HOST_LOCK) && c.result == "acquired")
        .expect("the host lock was taken");
    assert_eq!(
        trace[locked + 1].op,
        Op::InheritLock,
        "made inheritable right away"
    );
    let first_unlink = trace
        .iter()
        .position(|c| c.op == Op::Unlinkat)
        .expect("something was unlinked");
    assert!(locked < first_unlink);
    // The gate was asked before anything was measured.
    let gate = trace
        .iter()
        .position(|c| c.op == Op::MayDelete)
        .expect("asked");
    let statvfs = trace
        .iter()
        .position(|c| c.op == Op::Statvfs)
        .expect("sampled");
    assert!(gate < statvfs);
    // Released (closed) at the end: nobody holds it now.
    assert!(host.locks_on(Path::new(HOST_LOCK)).is_empty());
}

/// `--target-free` above `--min-free` keeps going: both targets go.
#[test]
fn target_free_reclaims_past_min_free() {
    let host = host(&yaml(true, ""));
    let report = ensure(&host, &cargo("12G", Some("19G")));
    assert_eq!(report.status, Status::Met);
    let order: Vec<&Path> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(order, [Path::new(OLD), Path::new(NEW)]);
    assert_eq!(report.target_met(), Some(true));
    // Above everything there is to free: met by --min-free, the target not.
    let host = common::host(&yaml(true, ""));
    let report = ensure(&host, &cargo("12G", Some("40G")));
    assert_eq!(report.status, Status::Met);
    assert_eq!(report.target_met(), Some(false));
    assert_eq!(
        report.exit_code(),
        0,
        "the exit code depends on --min-free only"
    );
}

/// Exit 1: every candidate processed without errors, still below
/// `--min-free` (both targets too recent for `idle-30d`). A process holding
/// a deleted file on `/home` is listed in `held_by_deleted_files`.
#[test]
fn everything_processed_below_min_free_is_exhausted_exit_1() {
    let host = host(&yaml(
        true,
        "  /home/mark/big.log: {size: 1G}\n  /usr/bin/tail: {}\nprocesses:\n  - pid: 4242\n    \
         exe: /usr/bin/tail\n    cwd: /home/mark\n    maps: [/usr/bin/tail]\n    environ: \
         [PATH=/usr/bin]\n    fds: [{path: /home/mark/big.log, deleted: true}]\n",
    ));
    let report = ensure(
        &host,
        &request(
            "profile:cargo-target && filter:idle-30d",
            &[SRC],
            SRC,
            "12G",
            None,
        ),
    );
    assert_eq!(report.status, Status::Exhausted, "{report:#?}");
    assert_eq!(report.exit_code(), 1);
    let skipped: Vec<(&Path, &str)> = report
        .skipped
        .iter()
        .map(|s| (s.candidate.as_path(), s.reason))
        .collect();
    assert_eq!(
        skipped,
        [(Path::new(OLD), "not_idle"), (Path::new(NEW), "not_idle")]
    );
    assert!(calls(&host, Op::Unlinkat).is_empty());
    assert_eq!(report.held_by_deleted_files.len(), 1, "{report:#?}");
    assert_eq!(report.held_by_deleted_files[0].pid, 4242);
    assert_eq!(report.held_by_deleted_files[0].bytes, GIB);
    assert!(report.summary().expect("prints").starts_with("exhausted: "));
}

/// Exit 3: a target whose delete fails is an error of that target; the
/// next target is still reclaimed, and the run ends below `--min-free`.
#[test]
fn a_failed_delete_is_an_error_exit_3_and_the_next_target_is_tried() {
    let host = host(&yaml(
        true,
        "events:\n  - {at: {op: unlinkat, path: /home/mark/src/old/target/debug/.fingerprint/p-1/bin-p, nth: 1, when: before}, fail: EACCES}\n",
    ));
    let report = ensure(&host, &cargo("16G", None));
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.errors.len(), 1, "{:#?}", report.errors);
    assert_eq!(report.errors[0].candidate, OLD);
    assert!(
        report.errors[0].message.contains("EACCES"),
        "{:?}",
        report.errors
    );
    let order: Vec<&Path> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(order, [Path::new(NEW)], "old deleted nothing, new went");
}

/// Exit 75: the host lock held by another reclaim until the deadline, a
/// fresh sample still below `--min-free`; nothing deleted. If the other
/// holder freed enough meanwhile, the result is 0.
#[test]
fn the_host_lock_held_until_the_deadline_is_75_unless_space_appeared() {
    let holder = "  /usr/bin/diskgc: {}\n  /home/mark/junk: {size: 5G}\nprocesses:\n  - pid: \
                  4242\n    exe: /usr/bin/diskgc\n    cwd: /\n    maps: [/usr/bin/diskgc]\n    \
                  environ: [PATH=/usr/bin]\nlocks:\n  - {path: /run/diskgc/lock, holder: 4242, \
                  mode: exclusive}\n";
    // About 3G available; the 5G junk the other holder removes after 1 s
    // brings it above 6G.
    let mut req = cargo("6G", None);
    req.timeout = Some(Duration::from_secs(2));
    let host = host(&yaml(true, holder));
    let report = ensure(&host, &req);
    assert_eq!(report.status, Status::LockTimeout, "{report:#?}");
    assert_eq!(report.exit_code(), 75);
    assert!(calls(&host, Op::Unlinkat).is_empty());

    let freed = format!("{holder}events:\n  - {{at: {{elapsed: 1s}}, remove: /home/mark/junk}}\n");
    let host = common::host(&yaml(true, &freed));
    let report = ensure(&host, &req);
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(report.exit_code(), 0);
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

/// Space that appears while waiting for the lock (the other reclaim freed
/// it) is seen right after the lock is taken: met, exit 0, no discovery.
#[test]
fn space_freed_while_waiting_for_the_lock_is_met_without_a_reclaim() {
    let holder = "  /usr/bin/diskgc: {}\n  /home/mark/junk: {size: 5G}\nprocesses:\n  - pid: \
                  4242\n    exe: /usr/bin/diskgc\n    cwd: /\n    maps: [/usr/bin/diskgc]\n    \
                  environ: [PATH=/usr/bin]\nlocks:\n  - {path: /run/diskgc/lock, holder: 4242, \
                  mode: exclusive}\nevents:\n  - {at: {elapsed: 1s}, remove: /home/mark/junk}\n  \
                  - {at: {elapsed: 1s}, exit: 4242}\n";
    let host = host(&yaml(true, holder));
    let report = ensure(&host, &cargo("6G", None));
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert!(
        host.trace()
            .iter()
            .any(|c| c.op == Op::Flock && touches(c, HOST_LOCK) && c.result == "acquired"),
        "the lock was taken once the holder exited"
    );
    assert!(
        host.trace()
            .iter()
            .all(|c| !touches(c, OLD) && !touches(c, NEW)),
        "no target was reached"
    );
}

/// `ensure`'s `statvfs` calls in order on the default model at 12G:
/// measure (1), after the lock (2), before the tier's first action (3),
/// after it (4), the final sample (5).
fn failing_statvfs(nth: u32) -> String {
    format!(
        "events:\n  - {{at: {{op: statvfs, path: /home/mark/src, nth: {nth}, when: before}}, \
         fail: EIO}}\n"
    )
}

/// B1: a sample that fails after an action leaves its change unknown (JSON
/// null), never 0; the failure is an error, logged, and the run stops
/// (the goal can no longer be observed). The final sample decides the
/// status.
#[test]
fn an_unmeasured_change_is_unknown_never_zero() {
    let host = host(&yaml(true, &failing_statvfs(4)));
    let report = ensure(&host, &cargo("12G", Some("19G")));
    assert_eq!(report.actions.len(), 1, "the run stopped: {report:#?}");
    assert_eq!(report.actions[0].candidate, Path::new(OLD));
    assert_eq!(report.actions[0].avail_delta_bytes, None);
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).expect("JSON");
    assert!(json["actions"][0]["avail_delta_bytes"].is_null(), "{json}");
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(
        report.errors[0].message.contains("EIO"),
        "{:?}",
        report.errors
    );
    assert_eq!(
        unlinked_below(&host, NEW),
        0,
        "no action after an unmeasured one"
    );
}

/// B1: a final sample that fails leaves `avail_after_bytes` unknown and
/// never claims the goal: status error (exit 3), `target_met` null.
#[test]
fn an_unmeasured_final_sample_never_claims_met() {
    let host = host(&yaml(true, &failing_statvfs(5)));
    let report = ensure(&host, &cargo("12G", None));
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.avail_after_bytes, None);
    assert_eq!(report.target_met(), None);
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).expect("JSON");
    assert!(
        json["avail_after_bytes"].is_null() && json["target_met"].is_null(),
        "{json}"
    );
}

/// Should-fix 7: a run that ends early on an error (here the sample right
/// after the lock fails) is an error, never success by default.
#[test]
fn an_early_end_after_the_lock_is_an_error_not_success() {
    let host = host(&yaml(true, &failing_statvfs(2)));
    let report = ensure(&host, &cargo("12G", None));
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.avail_after_bytes, None);
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

/// B1: without a sample before the tier's first action, nothing is
/// reclaimed (the goal could not be observed); the failure is an error.
#[test]
fn without_a_sample_before_the_first_action_nothing_is_reclaimed() {
    let host = host(&yaml(true, &failing_statvfs(3)));
    let report = ensure(&host, &cargo("12G", None));
    assert!(calls(&host, Op::Unlinkat).is_empty());
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert!(report.errors[0].message.contains("EIO"));
}

/// B1: a dry run whose final sample fails reports it unknown, not the
/// earlier sample.
#[test]
fn a_dry_run_with_an_unmeasured_final_sample_reports_it_unknown() {
    let host = host(&yaml(false, &failing_statvfs(2)));
    let report = dry_run(&host, &cargo("1G", None));
    assert_eq!(report.avail_after_bytes, None, "{report:#?}");
    // R3: a dry run keeps its exits; the unknown sample is reported, not
    // turned into exit 3.
    assert_eq!(report.status, Status::DryRun);
    assert_eq!(report.exit_code(), 0);
    assert!(report.avail_before_bytes.is_some());
    assert_eq!(report.errors.len(), 1);
}

/// R4: the first sample (of PATH's filesystem, before anything is done)
/// failing is exit 2 (usage_error): nothing was measured or done.
#[test]
fn a_failed_first_sample_is_exit_2() {
    let host = host(&yaml(true, &failing_statvfs(1)));
    let report = ensure(&host, &cargo("12G", None));
    assert_eq!(report.status, Status::UsageError, "{report:#?}");
    assert_eq!(report.exit_code(), 2);
    assert_eq!(report.avail_before_bytes, None);
    assert!(calls(&host, Op::Flock).is_empty());
}

/// Every error recorded reaches the log too.
#[test]
fn every_error_is_logged() {
    let host = host(&yaml(true, &failing_statvfs(5)));
    let uid = executor::privileges(&host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let report = executor::ensure(&host, &cargo("12G", None), &[uid], &mut |l| {
        lines.push(l.to_owned())
    });
    assert!(!report.errors.is_empty());
    for error in &report.errors {
        assert!(
            lines
                .iter()
                .any(|l| l.contains("error") && l.contains(&error.message)),
            "{error:?} not in {lines:#?}"
        );
    }
}

/// `--tally-budget`: the tally that orders the tier stops at its budget;
/// what it did not tally is taken after the tallied targets, in discovery
/// order (here, with a budget of zero, `new` before `old`, as found).
#[test]
fn the_tally_budget_leaves_targets_untallied_in_discovery_order() {
    let mut req = cargo("12G", Some("19G"));
    req.tally_budget = Some(Duration::ZERO);
    let host = host(&yaml(true, ""));
    let report = ensure(&host, &req);
    assert_eq!(report.status, Status::Met, "{report:#?}");
    let order: Vec<&Path> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(order, [Path::new(NEW), Path::new(OLD)]);
    // A generous budget: the oldest order.
    req.tally_budget = Some(Duration::from_secs(30));
    let host = common::host(&yaml(true, ""));
    let report = ensure(&host, &req);
    let order: Vec<&Path> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(order, [Path::new(OLD), Path::new(NEW)]);
}

/// Should-fix 10: something other than a regular file at the host lock's
/// path (a FIFO, a directory, a symlink) is refused, exit 2; a FIFO is
/// never opened and nothing is locked.
#[test]
fn a_host_lock_path_that_is_no_regular_file_is_exit_2() {
    for (what, node) in [
        ("fifo", "{type: fifo, uid: 0}"),
        ("directory", "{.: {uid: 0}}"),
        ("symlink", "{symlink: /tmp/elsewhere}"),
    ] {
        let model = yaml(false, "").replacen(
            "tree:\n",
            &format!("tree:\n  /tmp/elsewhere: {{}}\n  /run/diskgc/lock: {node}\n"),
            1,
        );
        let host = host(&model);
        let report = ensure(&host, &cargo("12G", None));
        assert_eq!(report.status, Status::UsageError, "{what}: {report:#?}");
        assert_eq!(report.exit_code(), 2);
        assert!(
            report.errors[0]
                .message
                .contains("host lock /run/diskgc/lock is unusable"),
            "{what}: {:?}",
            report.errors
        );
        for op in [Op::ReopenRegular, Op::Flock, Op::Unlinkat] {
            assert!(calls(&host, op).is_empty(), "{what}: {op} called");
        }
    }
}

/// U1: the lock timeout's fresh sample fails: exit 3 (a run error), not 75.
#[test]
fn a_lock_timeout_with_an_unmeasured_sample_is_exit_3() {
    let holder = "  /usr/bin/x: {}\nprocesses:\n  - {pid: 4242, exe: /usr/bin/x, cwd: /, maps: \
                  [/usr/bin/x], environ: [PATH=/usr/bin]}\nlocks:\n  - {path: /run/diskgc/lock, \
                  holder: 4242, mode: exclusive}\n";
    let mut req = cargo("12G", None);
    req.timeout = Some(Duration::from_secs(1));
    let host = host(&yaml(true, &format!("{holder}{}", failing_statvfs(2))));
    let report = ensure(&host, &req);
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.avail_after_bytes, None);
    assert!(report.errors[0].message.contains("EIO"));
}

/// U1: the deadline stops the reclaim and the final sample fails: exit 3,
/// not 124.
#[test]
fn a_deadline_with_an_unmeasured_final_sample_is_exit_3() {
    let host = host(&yaml(
        true,
        "events:\n  - {at: {op: flock, path: /home/mark/src/old/target/debug/.cargo-lock, nth: 1, \
         when: before}, advance: 20m}\n  - {at: {op: statvfs, path: /home/mark/src, nth: 5, \
         when: before}, fail: EIO}\n",
    ));
    let report = ensure(&host, &cargo("12G", None));
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.avail_after_bytes, None);
}

/// U2: a root reached through a symlink is a misconfiguration: an error
/// (exit 3 below --min-free), and the tier is excluded, also when another
/// root is fine; the dry run says the same. Reported once (an error, no
/// skip or plan entry) and logged once, with `unsafe_path` named once.
#[test]
fn an_unsafe_root_is_a_misconfiguration_that_excludes_the_tier() {
    let link = "  /home/mark/link: {symlink: /home/mark/src}\n";
    let model = yaml(true, "").replacen("tree:\n", &format!("tree:\n{link}"), 1);
    for roots in [&["/home/mark/link"][..], &["/home/mark/link", SRC]] {
        let host = host(&model);
        let report = ensure(
            &host,
            &request("profile:cargo-target", roots, SRC, "12G", None),
        );
        assert_eq!(report.status, Status::Error, "{roots:?}: {report:#?}");
        assert_eq!(report.exit_code(), 3);
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        let message = &report.errors[0].message;
        assert!(
            message.starts_with(
                "root /home/mark/link: unsafe_path (misconfigured, or the mounts changed): "
            ) && message.ends_with("; the tier is excluded")
                && message.matches("unsafe_path").count() == 1,
            "{message}"
        );
        assert!(report.skipped.is_empty(), "an error, not also a skip");
        assert!(
            calls(&host, Op::Unlinkat).is_empty(),
            "{roots:?}: the tier ran"
        );
        let host = common::host(&model);
        let uid = executor::privileges(&host, false).unwrap_or_else(|e| panic!("{e}"));
        let mut lines = Vec::new();
        let plan = executor::dry_run(
            &host,
            &request("profile:cargo-target", roots, SRC, "1G", None),
            &[uid],
            &mut |l| lines.push(l.to_owned()),
        );
        assert_eq!(
            plan.errors, report.errors,
            "the dry run reports it the same way"
        );
        assert!(plan.plan.is_empty(), "{:#?}", plan.plan);
        let mentions = lines.iter().filter(|l| l.contains("unsafe_path")).count();
        assert_eq!(mentions, 1, "logged once: {lines:#?}");
    }
}

/// The default model with a `.diskgc-keep` in `old`'s target directory,
/// and `extra` keys (events).
fn kept_old(extra: &str) -> String {
    yaml(true, extra).replacen(
        "  /home/mark/src/old/target:\n",
        "  /home/mark/src/old/target:\n    .diskgc-keep: {}\n",
        1,
    )
}

/// The skip reason `ensure` reported for `candidate`, if any.
fn skip_reason<'r>(report: &'r Report, candidate: &str) -> Option<&'r str> {
    report
        .skipped
        .iter()
        .find(|s| s.candidate == Path::new(candidate))
        .map(|s| s.reason)
}

/// R1: a tally refusal is final for the run (`cargo_target::tally`): the
/// marker removed after the tally does not get `old` reclaimed in this
/// run; it is skipped as `kept`, and never locked (a missed reclaim until
/// the next run, never an unsafe delete).
#[test]
fn a_tally_refusal_is_final_for_the_run_even_if_it_stops_being_true() {
    let host = host(&kept_old(
        "events:\n  - {at: {op: statvfs, path: /home/mark/src, nth: 3, when: before}, remove: \
         /home/mark/src/old/target/.diskgc-keep}\n",
    ));
    let report = ensure(&host, &cargo("12G", Some("19G")));
    let order: Vec<&Path> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(order, [Path::new(NEW)], "{report:#?}");
    assert_eq!(skip_reason(&report, OLD), Some("kept"));
    assert_eq!(unlinked_below(&host, OLD), 0);
    assert!(
        calls(&host, Op::Flock).iter().all(|c| !touches(c, OLD)),
        "a tally-refused target was locked"
    );
    assert!(
        host.stat_path(Path::new("/home/mark/src/old/target/.diskgc-keep"))
            .is_none()
    );
}

/// R2: a keep marker that stays, on the under-lock path where the
/// protocol's recheck (`judge_inspected`) is the only guard: with
/// a zero tally budget (set internally; on the CLI `--tally-budget 0`
/// means no limit) nothing is tallied, `old` is locked and
/// judged under its locks, skipped as `kept`, never unlinked; the run ends
/// below the goal (exit 1). And on the tallied path, where the tally
/// refuses it before any lock.
#[test]
fn a_kept_target_is_never_deleted_on_either_path() {
    for budget in [Some(Duration::ZERO), None] {
        let host = host(&kept_old(""));
        let mut req = cargo("19G", None);
        req.tally_budget = budget;
        let report = ensure(&host, &req);
        assert_eq!(
            skip_reason(&report, OLD),
            Some("kept"),
            "{budget:?}: {report:#?}"
        );
        assert_eq!(unlinked_below(&host, OLD), 0, "{budget:?}");
        assert_eq!(report.status, Status::Exhausted, "{budget:?}: {report:#?}");
        assert_eq!(report.exit_code(), 1);
        let old_locked = calls(&host, Op::Flock).iter().any(|c| touches(c, OLD));
        assert_eq!(
            old_locked,
            budget == Some(Duration::ZERO),
            "{budget:?}: the untallied path locks before it judges, the tallied path never \
             locks a refused target"
        );
    }
}

/// Three attached targets `a`, `b`, `c` (found in that order), built so
/// that the oldest order is `c`, `b`, `a`.
fn three(extra: &str) -> String {
    let target = |name: &str, at: &str| {
        format!(
            "  /home/mark/src/{name}/Cargo.toml: {{}}\n  /home/mark/src/{name}/target:\n    \
             CACHEDIR.TAG: {{cachedir_tag: true}}\n    debug: {}\n",
            profile("2G", at)
        )
    };
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 2, at: /home, fs: ext4, dev: "254:2", size: 100G, other_used: 80G, parent: 1}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
tree:
  /run/diskgc/lock: {{uid: 0, gid: 0}}
{}{}{}{extra}"#,
        target("a", "2026-10-14T00:00:00Z"),
        target("b", "2026-10-10T00:00:00Z"),
        target("c", "2026-10-01T00:00:00Z"),
    )
}

/// The second target's tally reads `b`'s profile directory a second time
/// (discovery read it first): a clock jump there.
fn jump_in_bs_tally(by: &str) -> String {
    format!(
        "events:\n  - {{at: {{op: read_dir, path: /home/mark/src/b/target/debug, nth: 2, when: \
         before}}, advance: {by}}}\n"
    )
}

/// U4: the budget spent during the second target's tally leaves `a`
/// tallied and `b`, `c` untallied: the order is `a`, `b`, `c` (tallied
/// first, then the rest as found), not the oldest order `c`, `b`, `a`.
#[test]
fn a_budget_spent_mid_tally_orders_tallied_then_untallied_as_found() {
    let host = host(&three(&jump_in_bs_tally("1m")));
    let mut req = request("profile:cargo-target", &[SRC], SRC, "15G", Some("19G"));
    req.tally_budget = Some(Duration::from_secs(30));
    let report = ensure(&host, &req);
    assert_eq!(report.status, Status::Met, "{report:#?}");
    let order: Vec<String> = report
        .actions
        .iter()
        .map(|a| a.candidate.display().to_string())
        .collect();
    assert_eq!(
        order,
        [
            "/home/mark/src/a/target",
            "/home/mark/src/b/target",
            "/home/mark/src/c/target"
        ]
    );
    // The same without a jump: the oldest order.
    let host = common::host(&three(""));
    let report = ensure(&host, &req);
    let first = report.actions[0].candidate.display().to_string();
    assert_eq!(first, "/home/mark/src/c/target");
}

/// U4: the run's deadline passing during the tally is the deadline (124),
/// not a spent budget.
#[test]
fn the_deadline_during_the_tally_is_124_not_a_spent_budget() {
    let host = host(&three(&jump_in_bs_tally("20m")));
    let mut req = request("profile:cargo-target", &[SRC], SRC, "15G", None);
    req.tally_budget = Some(Duration::from_secs(30 * 60));
    let uid = executor::privileges(&host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let report = executor::ensure(&host, &req, &[uid], &mut |l| lines.push(l.to_owned()));
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert_eq!(report.exit_code(), 124);
    assert!(calls(&host, Op::Unlinkat).is_empty());
    assert!(
        lines.iter().all(|l| !l.contains("--tally-budget spent")),
        "{lines:#?}"
    );
}

/// Exit 124 (cheaply, ahead of task-54): the deadline passes during the
/// reclaim (a clock jump at the first target's lock), below `--min-free`.
#[test]
fn the_deadline_during_the_reclaim_is_124() {
    let host = host(&yaml(
        true,
        "events:\n  - {at: {op: flock, path: /home/mark/src/old/target/debug/.cargo-lock, nth: 1, when: before}, advance: 20m}\n",
    ));
    let report = ensure(&host, &cargo("12G", None));
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert_eq!(report.exit_code(), 124);
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

/// AC #4 (privileges half): uid 0 is refused without `--allow-root`;
/// the binary checks `owners:` and resolves it before reaching the executor.
#[test]
fn root_is_refused_without_allow_root() {
    let root = host(&format!("own_uid: 0\n{}", yaml(true, "")));
    let error = executor::privileges(&root, false).expect_err("root refused");
    assert!(error.contains("refuses to run as root"), "{error}");
    assert_eq!(executor::privileges(&root, true), Ok(0));
    let user = host(&yaml(true, ""));
    assert_eq!(executor::privileges(&user, false), Ok(1000));
    assert_eq!(executor::privileges(&user, true), Ok(1000));
    assert!(calls(&root, Op::Unlinkat).is_empty());
}

/// AC #6's logic (the binary's test runs it on a real host): a closed
/// sandbox gate stops ensure before it measures, locks or plans, as a
/// usage error naming the gate.
#[test]
fn a_closed_sandbox_gate_stops_ensure_before_it_measures() {
    let host = host(&format!("sandbox_gate: closed\n{}", yaml(true, "")));
    let report = ensure(&host, &cargo("12G", None));
    assert_eq!(report.status, Status::UsageError);
    assert_eq!(report.exit_code(), 2);
    assert!(
        report.errors[0].message.contains("sandbox gate"),
        "{:?}",
        report.errors
    );
    let ops: Vec<Op> = host.trace().iter().map(|c| c.op).collect();
    assert!(ops.contains(&Op::MayDelete));
    for op in [Op::Statvfs, Op::Openat2, Op::Flock, Op::Unlinkat] {
        assert!(!ops.contains(&op), "{op} after a closed gate: {ops:?}");
    }
}

/// A gate that refuses only at the deleting call (asked first, it said
/// open): the refusal is an error of the run, which stops; the second
/// target is never locked.
#[test]
fn a_gate_refusal_at_the_delete_stops_the_run() {
    let host = host(&format!(
        "sandbox_gate: refuses_deletes\n{}",
        yaml(true, "")
    ));
    let report = ensure(&host, &cargo("16G", None));
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.errors.len(), 1);
    assert!(report.errors[0].message.contains("sandbox gate"));
    assert!(
        report.skipped.is_empty(),
        "never a skip: {:?}",
        report.skipped
    );
    assert!(
        calls(&host, Op::Flock).iter().all(|c| !touches(c, NEW)),
        "the run went on to the next target"
    );
}

/// `--dry-run` (docs/reference.md): plans every target, eligible first in
/// the order ensure takes them, even with the goal met; no lock of any
/// kind (no flock at all), no gate question, nothing unlinked, and
/// `/run/diskgc` not reached (the model has none).
#[test]
fn a_dry_run_plans_every_target_and_locks_and_deletes_nothing() {
    let host = host(&yaml(false, ""));
    let report = dry_run(
        &host,
        &request(
            "profile:cargo-target && filter:idle-7d",
            &[SRC],
            SRC,
            "1G",
            None,
        ),
    );
    assert_eq!(report.status, Status::DryRun, "{report:#?}");
    assert_eq!(report.exit_code(), 0);
    let plan: Vec<(&Path, Option<&str>)> = report
        .plan
        .iter()
        .map(|p| (p.candidate.as_path(), p.reason))
        .collect();
    assert_eq!(
        plan,
        [(Path::new(OLD), None), (Path::new(NEW), Some("not_idle"))]
    );
    assert!(report.plan[0].estimated_bytes.expect("eligible: sized") >= 8 * GIB);
    assert_eq!(report.plan[1].estimated_bytes, None);
    assert_eq!(report.plan[0].operation, "cargo lock protocol");
    for op in [
        Op::Flock,
        Op::Unlinkat,
        Op::Fchmod,
        Op::MayDelete,
        Op::InheritLock,
    ] {
        assert!(calls(&host, op).is_empty(), "{op} in a dry run");
    }
    assert!(host_lock_untouched(&host));
    assert!(report.actions.is_empty() && report.skipped.is_empty());
    let text = report.summary().expect("a dry run always prints");
    assert!(
        text.contains(OLD) && text.contains("skip not_idle"),
        "{text}"
    );

    // Without a filter, both eligible, oldest first.
    let host = common::host(&yaml(false, ""));
    let report = dry_run(&host, &cargo("1G", None));
    let order: Vec<&Path> = report.plan.iter().map(|p| p.candidate.as_path()).collect();
    assert_eq!(order, [Path::new(OLD), Path::new(NEW)]);
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

/// A target on another filesystem than PATH's is `other_fs`: deleting it
/// would free nothing there.
#[test]
fn a_target_on_another_filesystem_is_other_fs() {
    let tmp = "  /tmp/w/q/Cargo.toml: {}\n  /tmp/w/q/target:\n    CACHEDIR.TAG: {cachedir_tag: \
               true}\n    debug: {.cargo-lock: {}, deps: {q-1: {size: 100M}}}\n";
    let host = host(&yaml(true, tmp));
    let report = ensure(
        &host,
        &request("profile:cargo-target", &[SRC, "/tmp/w"], SRC, "16G", None),
    );
    let other: Vec<&Path> = report
        .skipped
        .iter()
        .filter(|s| s.reason == "other_fs")
        .map(|s| s.candidate.as_path())
        .collect();
    assert_eq!(other, [Path::new("/tmp/w/q/target")], "{report:#?}");
    assert_eq!(unlinked_below(&host, "/tmp"), 0);
}

/// Sizes are resolved against PATH's filesystem: above it, or a target
/// below the minimum, is a usage error before anything is locked.
#[test]
fn sizes_beyond_the_filesystem_or_target_below_min_are_usage_errors() {
    for (min, target, needle) in [
        ("101%", None, "more than the whole filesystem"),
        ("101G", None, "more than the filesystem's size"),
        ("12G", Some("11G"), "below --min-free"),
    ] {
        let host = host(&yaml(true, ""));
        let report = ensure(&host, &cargo(min, target));
        assert_eq!(report.status, Status::UsageError, "{min} {target:?}");
        assert!(
            report.errors[0].message.contains(needle),
            "{:?}",
            report.errors
        );
        assert!(calls(&host, Op::Flock).is_empty());
    }
    let host = host(&yaml(true, ""));
    let report = ensure(
        &host,
        &request("profile:cargo-target", &[SRC], "/home/nope", "1G", None),
    );
    assert_eq!(report.status, Status::UsageError);
    assert!(
        report.errors[0].message.contains("missing"),
        "{:?}",
        report.errors
    );
    assert_eq!(report.mount, None);
}

/// What the grammar allows but this build cannot run yet is refused when
/// the tier is built (exit 2), never approximated.
#[test]
fn selections_this_build_cannot_run_are_refused() {
    let library = Library::builtin().unwrap_or_else(|e| panic!("{e}"));
    let roots = vec![SRC.to_owned()];
    for (expr, needle) in [
        (
            "profile:cargo-target && filter:idle-1d && !filter:idle-30d",
            "`!filter:idle-30d` is not supported",
        ),
        (
            "profile:cargo-target && !profile:cargo-target",
            "`!profile:cargo-target` is not supported",
        ),
        (
            "(profile:cargo-target && filter:idle-1d) || profile:cargo-target",
            "a union",
        ),
        ("profile:nope", "no profile or filter is named `nope`"),
    ] {
        let error = Tier::select(expr, &library, Some(&roots), 1000).expect_err(expr);
        assert!(error.contains(needle), "{expr}: {error}");
    }
    let error = Tier::select("profile:cargo-target", &library, None, 1000).expect_err("no roots");
    assert!(error.contains("roots: required"), "{error}");
    let tier = Tier::select(
        "profile:cargo-target && filter:idle-1d && filter:idle-7d",
        &library,
        Some(&["/tmp/claude-{uid}".to_owned()]),
        1000,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(tier.roots, [PathBuf::from("/tmp/claude-1000")]);
    assert_eq!(
        tier.idle_for.map(|i| i.0),
        Some(Duration::from_secs(7 * 86_400))
    );
    assert_eq!(tier.name, "--select");
}

/// The host lock is held exclusively by this run while it reclaims: a
/// process's shared lock is enough to make it wait (and time out).
#[test]
fn a_shared_holder_of_the_host_lock_blocks_too() {
    let holder = "  /usr/bin/x: {}\nprocesses:\n  - {pid: 4242, exe: /usr/bin/x, cwd: /, maps: \
                  [/usr/bin/x], environ: [PATH=/usr/bin]}\nlocks:\n  - {path: /run/diskgc/lock, \
                  holder: 4242, mode: shared}\n";
    let mut req = cargo("12G", None);
    req.timeout = Some(Duration::from_secs(1));
    let host = host(&yaml(true, holder));
    let report = ensure(&host, &req);
    assert_eq!(report.status, Status::LockTimeout);
    assert_eq!(
        host.locks_on(Path::new(HOST_LOCK)),
        vec![(Holder::Process(4242), diskgc_core::host::LockMode::Shared)]
    );
}

// ---- task-80: the checkout guard is the source profile's setting

/// A Git checkout a build script cloned into `old`'s target, as old as
/// the rest of it (so `old` stays the oldest target).
const VENDORED: &str = "  /home/mark/src/old/target/debug/build:\n    .: {mtime: \
    2026-10-01T00:00:00Z}\n    p-2: {.: {mtime: 2026-10-01T00:00:00Z}, out: {.: {mtime: \
    2026-10-01T00:00:00Z}, vendor: {.: {mtime: 2026-10-01T00:00:00Z}, .git: {.: {mtime: \
    2026-10-01T00:00:00Z}, HEAD: {mtime: 2026-10-01T00:00:00Z}}}}}\n";

/// A third target found inside a checkout's metadata (`zz/.git/...`, found last),
/// built 2026-10-05.
fn in_metadata() -> String {
    format!(
        "  /home/mark/src/zz/.git/modules/target:\n    CACHEDIR.TAG: {{cachedir_tag: true}}\n    \
         debug: {}\n",
        profile("1G", "2026-10-05T00:00:00Z")
    )
}

/// The tier of a user profile of kind cargo-target that does not say
/// `checkouts` (refused, the default), with `--roots SRC`.
fn my_targets(min: &str) -> Request {
    let specs = parse_profiles("- {name: my-targets, kind: cargo-target, find: {roots: required}}")
        .unwrap_or_else(|e| panic!("{e}"));
    let library = Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"));
    Request {
        tier: Tier::select(
            "profile:my-targets",
            &library,
            Some(&[SRC.to_owned()]),
            1000,
        )
        .unwrap_or_else(|e| panic!("{e}")),
        ..cargo(min, None)
    }
}

/// [`dry_run`] and its log lines.
fn dry_run_logged(host: &ModelHost, request: &Request) -> (Report, Vec<String>) {
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let report = executor::dry_run(host, request, &[uid], &mut |l| lines.push(l.to_owned()));
    eprintln!("log:\n  {}", lines.join("\n  "));
    (report, lines)
}

/// [`ensure`] and its log lines.
fn ensure_logged(host: &ModelHost, request: &Request) -> (Report, Vec<String>) {
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let report = executor::ensure(host, request, &[uid], &mut |l| lines.push(l.to_owned()));
    eprintln!("log:\n  {}", lines.join("\n  "));
    (report, lines)
}

/// The log lines that say a profile allows checkouts.
fn allow_lines(lines: &[String]) -> Vec<&String> {
    lines
        .iter()
        .filter(|l| l.contains("checkouts: allow"))
        .collect()
}

/// The plan's reason for `candidate` (`Some(None)`: eligible).
fn plan_reason<'r>(report: &'r Report, candidate: &str) -> Option<Option<&'r str>> {
    report
        .plan
        .iter()
        .find(|p| p.candidate == Path::new(candidate))
        .map(|p| p.reason)
}

/// task-80: the built-in `cargo-target` says `checkouts: allow` (the code
/// has no Cargo exemption): a target holding a vendored checkout, and one
/// found inside a checkout's metadata, are eligible, and the log says the
/// profile allows checkouts. A user profile of the same kind without the
/// field refuses both (`contains_checkout`), the first by the walk, the
/// second by its path, and logs nothing about checkouts.
#[test]
fn a_cargo_target_tier_takes_its_checkout_setting_from_its_profile() {
    let meta = "/home/mark/src/zz/.git/modules/target";
    let fixture = yaml(false, &format!("{VENDORED}{}", in_metadata()));

    let host = common::host(&fixture);
    let (report, lines) = dry_run_logged(&host, &cargo("1G", None));
    assert_eq!(plan_reason(&report, OLD), Some(None), "{report:#?}");
    assert_eq!(plan_reason(&report, meta), Some(None), "{report:#?}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("profile `cargo-target` says `checkouts: allow`")),
        "{lines:#?}"
    );

    let host = common::host(&fixture);
    let (report, lines) = dry_run_logged(&host, &my_targets("1G"));
    assert_eq!(
        plan_reason(&report, OLD),
        Some(Some("contains_checkout")),
        "{report:#?}"
    );
    assert_eq!(
        plan_reason(&report, meta),
        Some(Some("contains_checkout")),
        "{report:#?}"
    );
    assert_eq!(plan_reason(&report, NEW), Some(None), "{report:#?}");
    assert!(
        lines.iter().all(|l| !l.contains("checkouts: allow")),
        "{lines:#?}"
    );
}

/// task-80, the reclaim: with the built-in profile, `old` and the
/// vendored checkout in it go; with a profile that refuses checkouts,
/// `old` is skipped as `contains_checkout` and never unlinked on either
/// path (refused by the tally, or, untallied, by the protocol's walk under
/// its locks), the target inside a checkout's metadata is refused by its
/// path before any lock (by the tally, or by the protocol), and only `new`
/// is reclaimed: the goal is not met. Only the reclaim under the allowing
/// profile logs that it allows checkouts.
#[test]
fn a_cargo_target_tier_reclaims_checkouts_only_when_its_profile_allows() {
    let vendor = "/home/mark/src/old/target/debug/build/p-2/out/vendor/.git";
    let host = common::host(&yaml(true, VENDORED));
    let (report, lines) = ensure_logged(&host, &cargo("11G", None));
    assert_eq!(report.status, Status::Met, "{report:#?}");
    // The reclaim says it runs with checkouts allowed, once, by profile.
    let allowed = allow_lines(&lines);
    assert!(
        matches!(allowed.as_slice(), [line]
            if line.contains("profile `cargo-target` says `checkouts: allow`")),
        "{lines:#?}"
    );
    assert_eq!(report.actions[0].candidate, Path::new(OLD), "{report:#?}");
    assert!(!exists(&host, vendor));

    let meta = "/home/mark/src/zz/.git/modules/target";
    for budget in [Some(Duration::ZERO), None] {
        let host = common::host(&yaml(true, &format!("{VENDORED}{}", in_metadata())));
        // A goal only `old` could meet: every target is judged.
        let mut req = my_targets("13G");
        req.tally_budget = budget;
        let (report, lines) = ensure_logged(&host, &req);
        assert!(allow_lines(&lines).is_empty(), "{budget:?}: {lines:#?}");
        assert_eq!(report.status, Status::Exhausted, "{budget:?}: {report:#?}");
        assert_eq!(
            skip_reason(&report, OLD),
            Some("contains_checkout"),
            "{budget:?}: {report:#?}"
        );
        assert_eq!(
            skip_reason(&report, meta),
            Some("contains_checkout"),
            "{budget:?}: {report:#?}"
        );
        let order: Vec<&Path> = report
            .actions
            .iter()
            .map(|a| a.candidate.as_path())
            .collect();
        assert_eq!(order, [Path::new(NEW)], "{budget:?}: {report:#?}");
        assert_eq!(unlinked_below(&host, OLD), 0, "{budget:?}");
        assert_eq!(unlinked_below(&host, meta), 0, "{budget:?}");
        assert!(exists(&host, vendor), "{budget:?}");
        assert!(
            calls(&host, Op::Flock).iter().all(|c| !touches(c, meta)),
            "{budget:?}: a target refused by its path was locked"
        );
    }
}

/// Codex round 2 (task-80): the Cargo kind's checkout path rule covers
/// every name between the root and each entry it would delete, not only
/// the way down to the target: the target's own name (`<M>/debug`), a
/// profile directory's (`target/<M>`) and a target triple's
/// (`target/<M>/debug`), for each marker `<M>`. Each layout is discovered
/// as a target, below `/home/mark/src/x`.
fn checkout_layouts() -> Vec<(String, String)> {
    let mut layouts = Vec::new();
    for marker in [".git", ".hg", ".svn"] {
        let debug = profile("1G", "2026-10-02T00:00:00Z");
        let tag = "CACHEDIR.TAG: {cachedir_tag: true}";
        layouts.push((
            format!("/home/mark/src/x/{marker}"),
            format!("  /home/mark/src/x/{marker}:\n    {tag}\n    debug: {debug}\n"),
        ));
        layouts.push((
            "/home/mark/src/x/target".to_owned(),
            format!("  /home/mark/src/x/target:\n    {tag}\n    {marker}: {debug}\n"),
        ));
        layouts.push((
            "/home/mark/src/x/target".to_owned(),
            format!("  /home/mark/src/x/target:\n    {tag}\n    {marker}: {{debug: {debug}}}\n"),
        ));
    }
    layouts
}

/// Under a profile that refuses checkouts, each of those targets is
/// `contains_checkout` in a dry run (eligibility), and in a reclaim
/// whether the tally refuses it or, untallied, the protocol does (before
/// any lock): nothing below it is unlinked, and it is never locked. Under
/// the built-in (`checkouts: allow`) the same targets are eligible.
#[test]
fn a_cargo_target_with_a_marker_between_root_and_its_entries_is_refused() {
    for (target, tree) in checkout_layouts() {
        let fixture = yaml(true, &tree);
        let host = common::host(&fixture);
        let report = dry_run(&host, &my_targets("1G"));
        assert_eq!(
            plan_reason(&report, &target),
            Some(Some("contains_checkout")),
            "{tree}: {report:#?}"
        );
        let host = common::host(&fixture);
        let report = dry_run(&host, &cargo("1G", None));
        assert_eq!(
            plan_reason(&report, &target),
            Some(None),
            "{tree}: {report:#?}"
        );
        for budget in [Some(Duration::ZERO), None] {
            let host = common::host(&fixture);
            // A goal nothing meets: every target is judged.
            let mut req = my_targets("100G");
            req.tally_budget = budget;
            let report = ensure(&host, &req);
            assert_eq!(
                skip_reason(&report, &target),
                Some("contains_checkout"),
                "{budget:?} {tree}: {report:#?}"
            );
            assert_eq!(unlinked_below(&host, &target), 0, "{budget:?} {tree}");
            assert!(
                calls(&host, Op::Flock).iter().all(|c| !touches(c, &target)),
                "{budget:?} {tree}: locked before it was refused"
            );
        }
    }
}

/// A profile directory holding nothing but its lock file, every time `at`.
fn empty_profile(at: &str) -> String {
    format!("{{.: {{mtime: {at}}}, .cargo-lock: {{mtime: {at}}}}}")
}

/// task-81: targets whose profile directories inside a checkout's
/// metadata hold no entries, each discovered below `/home/mark/src/x`:
/// one found inside `.git` with only an empty profile directory, and, for
/// each marker `<M>`, one whose empty profile directory is `target/<M>`
/// or `target/<M>/debug` next to a full `debug`. With the target's
/// expected reason under the built-in `checkouts: allow` (`None`:
/// eligible).
fn empty_checkout_layouts() -> Vec<(String, String, Option<&'static str>)> {
    let at = "2026-10-02T00:00:00Z";
    let (full, empty) = (profile("1G", at), empty_profile(at));
    let tag = "CACHEDIR.TAG: {cachedir_tag: true}";
    let mut layouts = vec![(
        "/home/mark/src/x/.git/target".to_owned(),
        format!("  /home/mark/src/x/.git/target:\n    {tag}\n    debug: {empty}\n"),
        Some("empty"),
    )];
    for marker in [".git", ".hg", ".svn"] {
        for inner in [empty.clone(), format!("{{debug: {empty}}}")] {
            layouts.push((
                "/home/mark/src/x/target".to_owned(),
                format!(
                    "  /home/mark/src/x/target:\n    {tag}\n    debug: {full}\n    {marker}: \
                     {inner}\n"
                ),
                None,
            ));
        }
    }
    layouts
}

/// mped, task-80 r3: the Cargo kind's checkout path rule judges each
/// profile directory by its path, from the root down to it, its own name
/// included, whatever it holds: a profile directory with no entries
/// inside a checkout's metadata refuses its target under a profile that
/// refuses checkouts, in a dry run and in a reclaim, whether the tally
/// refuses it or, untallied, the protocol does, before any lock: no lock
/// file below it is `flock`ed and nothing is unlinked. Under the built-in
/// (`checkouts: allow`) the rule refuses nothing (`empty` is the layout's
/// verdict).
#[test]
fn a_cargo_target_with_an_empty_profile_directory_in_checkout_metadata_is_refused() {
    for (target, tree, allowed) in empty_checkout_layouts() {
        let fixture = yaml(true, &tree);
        let host = common::host(&fixture);
        let report = dry_run(&host, &my_targets("1G"));
        assert_eq!(
            plan_reason(&report, &target),
            Some(Some("contains_checkout")),
            "{tree}: {report:#?}"
        );
        let host = common::host(&fixture);
        let report = dry_run(&host, &cargo("1G", None));
        assert_eq!(
            plan_reason(&report, &target),
            Some(allowed),
            "{tree}: {report:#?}"
        );
        for budget in [Some(Duration::ZERO), None] {
            let host = common::host(&fixture);
            // A goal nothing meets: every target is judged.
            let mut req = my_targets("100G");
            req.tally_budget = budget;
            let report = ensure(&host, &req);
            assert_eq!(
                skip_reason(&report, &target),
                Some("contains_checkout"),
                "{budget:?} {tree}: {report:#?}"
            );
            assert_eq!(unlinked_below(&host, &target), 0, "{budget:?} {tree}");
            assert!(
                calls(&host, Op::Flock).iter().all(|c| !touches(c, &target)),
                "{budget:?} {tree}: locked before it was refused"
            );
        }
    }
}
