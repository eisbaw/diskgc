//! Deterministic signal delivery on the virtual clock and between delete
//! calls. No real process is signalled and no host file is removed.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::executor::{self, ProtectedKind, ProtectedSource, Request, Tier};
use diskgc_core::profile::Library;
use diskgc_core::report::{Signal, Status};
use diskgc_core::units::parse_size;
use diskgc_model::spec::Op;

fn target(size: &str, at: &str) -> String {
    format!(
        "{{.: {{mtime: {at}}}, .cargo-lock: {{mtime: {at}}}, \
         .fingerprint: {{.: {{mtime: {at}}}, p-1: {{.: {{mtime: {at}}}, bin-p: {{mtime: {at}}}}}}}, \
         deps: {{.: {{mtime: {at}}}, p-1: {{size: {size}, mtime: {at}}}}}}}"
    )
}

fn model(extra: &str) -> String {
    model_with_new_target_time(extra, "2026-10-14T00:00:00Z")
}

fn model_with_new_target_time(extra: &str, new_target_time: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 2, at: /home, fs: ext4, dev: "254:2", size: 100G, other_used: 80G, parent: 1}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
tree:
  /run/diskgc/lock: {{uid: 0, gid: 0}}
  /home/mark/src/old/Cargo.toml: {{}}
  /home/mark/src/old/target:
    CACHEDIR.TAG: {{cachedir_tag: true}}
    debug: {old}
  /home/mark/src/new/Cargo.toml: {{}}
  /home/mark/src/new/target:
    CACHEDIR.TAG: {{cachedir_tag: true}}
    debug: {new}
{extra}"#,
        old = target("8G", "2026-10-01T00:00:00Z"),
        new = target("4G", new_target_time),
    )
}

fn request() -> Request {
    let roots = vec!["/home/mark/src".to_owned()];
    Request {
        path: PathBuf::from("/home/mark/src"),
        min_free: parse_size("12G").unwrap(),
        target_free: Some(parse_size("19G").unwrap()),
        timeout: Some(Duration::from_secs(10)),
        tally_budget: None,
        tier: Tier::select(
            "profile:cargo-target",
            &Library::builtin().unwrap(),
            Some(&roots),
            1000,
        )
        .unwrap(),
    }
}

fn command_request() -> Request {
    let mut req = request();
    let roots = vec!["/home/mark/src".to_owned()];
    req.tier = Tier::select(
        "profile:pip-cache",
        &Library::builtin().unwrap(),
        Some(&roots),
        1000,
    )
    .unwrap();
    req
}

#[test]
fn sigterm_interrupts_a_contended_host_lock_before_the_deadline() {
    let extra = "  /usr/bin/diskgc: {}\nprocesses:\n  - pid: 4242\n    exe: /usr/bin/diskgc\n    cwd: /\n    maps: [/usr/bin/diskgc]\n    environ: [PATH=/usr/bin]\nlocks:\n  - {path: /run/diskgc/lock, holder: 4242, mode: exclusive}\nevents:\n  - {at: {elapsed: 1s}, signal: term}\n";
    let host = common::host(&model(extra));
    let report = executor::ensure(&host, &request(), &[1000], &mut |_| {});
    assert_eq!(
        report.status,
        Status::Interrupted(Signal::Term),
        "{report:#?}"
    );
    assert_eq!(report.exit_code(), 143);
    assert_eq!(host.elapsed(), Duration::from_secs(1));
    assert!(host.trace().iter().all(|call| call.op != Op::Unlinkat));
}

#[test]
fn sigint_dry_run_has_json_interrupted_status_without_deletion() {
    let extra = "events:\n  - {at: {op: statvfs, path: /home/mark/src, nth: 1, when: after}, signal: int}\n";
    let host = common::host(&model(extra));
    let report = executor::dry_run(&host, &request(), &[1000], &mut |_| {});
    assert_eq!(
        report.status,
        Status::Interrupted(Signal::Int),
        "{report:#?}"
    );
    assert_eq!(report.exit_code(), 130);
    assert!(report.to_json().contains("\"status\":\"interrupted\""));
    assert!(
        report.plan.is_empty(),
        "no candidate planned after the first sample was interrupted"
    );
    assert!(host.trace().iter().all(|call| call.op != Op::Unlinkat));
}

#[test]
fn sigint_during_initial_space_sample_is_an_interruption() {
    let extra = "events:\n  - {at: {op: statvfs, path: /home/mark/src, nth: 1, when: before}, signal: int}\n";
    let host = common::host(&model(extra));
    let report = executor::ensure(&host, &request(), &[1000], &mut |_| {});
    assert_eq!(
        report.status,
        Status::Interrupted(Signal::Int),
        "{report:#?}"
    );
    assert_eq!(report.exit_code(), 130);
    assert!(host.trace().iter().all(|call| call.op != Op::Unlinkat));
}

#[test]
fn sigterm_after_first_unlink_reports_partial_candidate_and_starts_no_next_candidate() {
    let extra = "events:\n  - {at: {op: unlinkat, nth: 1, when: after}, signal: term}\n";
    let host = common::host(&model(extra));
    let report = executor::ensure(&host, &request(), &[1000], &mut |_| {});
    assert_eq!(
        report.status,
        Status::Interrupted(Signal::Term),
        "{report:#?}"
    );
    assert_eq!(report.exit_code(), 143);
    assert!(
        report
            .actions
            .iter()
            .any(|action| action.candidate == Path::new("/home/mark/src/old/target")),
        "{report:#?}"
    );
    assert!(
        host.stat_path(Path::new("/home/mark/src/new/target"))
            .is_some()
    );
    assert!(
        host.trace()
            .iter()
            .filter(|call| call.op == Op::Unlinkat)
            .all(|call| !call
                .path
                .as_ref()
                .is_some_and(|p| p.starts_with("/home/mark/src/new")))
    );
}

#[test]
fn sigterm_before_first_unlink_changes_no_candidate() {
    let extra = "events:\n  - {at: {op: unlinkat, nth: 1, when: before}, signal: term}\n";
    let host = common::host(&model(extra));
    let report = executor::ensure(&host, &request(), &[1000], &mut |_| {});
    assert_eq!(
        report.status,
        Status::Interrupted(Signal::Term),
        "{report:#?}"
    );
    assert_eq!(report.exit_code(), 143);
    assert!(report.actions.is_empty(), "{report:#?}");
    for target in ["/home/mark/src/old/target", "/home/mark/src/new/target"] {
        assert!(host.stat_path(Path::new(target)).is_some(), "{target}");
    }
    assert!(
        host.trace()
            .iter()
            .filter(|call| call.op == Op::Unlinkat)
            .all(|call| call.result == "interrupted")
    );
}

#[test]
fn a_blocking_candidate_operation_reports_overrun_and_starts_no_next_candidate() {
    let extra = "events:\n  - {at: {op: flock, path: /home/mark/src/old/target/debug/.cargo-lock, nth: 1, when: before}, advance: 20m}\n";
    // The second target must be eligible too, or the no-next-work check is
    // vacuous. It is newer than `old`, but still well beyond idle-3d.
    let new_target_time = "2026-10-02T00:00:00Z";
    let plan_host = common::host(&model_with_new_target_time("", new_target_time));
    let plan = executor::dry_run(&plan_host, &request(), &[1000], &mut |_| {});
    assert!(
        plan.plan.iter().any(|entry| {
            entry.candidate == Path::new("/home/mark/src/new/target") && entry.reason.is_none()
        }),
        "the later candidate must be eligible: {plan:#?}"
    );
    let host = common::host(&model_with_new_target_time(extra, new_target_time));
    let mut logs = Vec::new();
    let report = executor::ensure(&host, &request(), &[1000], &mut |line| {
        logs.push(line.to_owned())
    });
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert_eq!(report.exit_code(), 124);
    assert!(
        report.overruns.iter().any(|entry| {
            entry.operation == "reclaim"
                && entry.path == Path::new("/home/mark/src/old/target")
                && entry.over_ms > 0
        }),
        "{report:#?}"
    );
    assert!(host.trace().iter().all(|call| {
        call.op != Op::Flock
            || !call
                .path
                .as_ref()
                .is_some_and(|path| path.starts_with("/home/mark/src/new"))
    }));
    assert!(
        host.stat_path(Path::new("/home/mark/src/new/target"))
            .is_some()
    );
    assert!(
        logs.iter()
            .all(|line| !line.contains("reclaiming /home/mark/src/new/target")),
        "the next candidate must not even enter reclaim: {logs:#?}"
    );
}

#[test]
fn a_blocking_discovery_operation_reports_overrun_without_deleting() {
    let extra = "events:\n  - {at: {op: read_dir, path: /home/mark/src, nth: 1, when: before}, advance: 20m}\n";
    let host = common::host(&model(extra));
    let report = executor::ensure(&host, &request(), &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert!(
        report.overruns.iter().any(|entry| {
            entry.operation == "discover"
                && entry.path == Path::new("/home/mark/src")
                && entry.over_ms > 0
        }),
        "{report:#?}"
    );
    assert!(host.trace().iter().all(|call| call.op != Op::Unlinkat));
}

#[test]
fn a_blocking_tally_operation_reports_overrun_without_deleting() {
    let extra = "events:\n  - {at: {op: read_dir, path: /home/mark/src/old/target/debug, nth: 2, when: before}, advance: 20m}\n";
    let host = common::host(&model(extra));
    let report = executor::ensure(&host, &request(), &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert!(
        report.overruns.iter().any(|entry| {
            entry.operation == "tally"
                && entry.path == Path::new("/home/mark/src/old/target")
                && entry.over_ms > 0
        }),
        "{report:#?}"
    );
    assert!(host.trace().iter().all(|call| call.op != Op::Unlinkat));
}

#[test]
fn initial_measure_overrun_stops_before_the_host_lock() {
    let extra = "events:\n  - {at: {op: statvfs, path: /home/mark/src, nth: 1, when: before}, advance: 20m}\n";
    let host = common::host(&model(extra));
    let report = executor::ensure(&host, &request(), &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert!(
        report
            .overruns
            .iter()
            .any(|entry| entry.operation == "measure" && entry.over_ms > 0),
        "{report:#?}"
    );
    assert!(
        host.trace()
            .iter()
            .all(|call| call.op != Op::Flock && call.op != Op::Unlinkat)
    );
}

#[test]
fn sandbox_gate_overrun_is_part_of_the_whole_call_deadline() {
    let extra = "events:\n  - {at: {op: may_delete, nth: 1, when: before}, advance: 20m}\n";
    let host = common::host(&model(extra));
    let report = executor::ensure(&host, &request(), &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert!(report.avail_before_bytes.is_some(), "{report:#?}");
    assert!(
        report
            .overruns
            .iter()
            .any(|entry| { entry.operation == "sandbox_gate" && entry.over_ms > 0 }),
        "{report:#?}"
    );
    assert!(
        host.trace()
            .iter()
            .all(|call| call.op != Op::Flock && call.op != Op::Unlinkat)
    );
}

#[test]
fn closed_sandbox_gate_stays_a_usage_error_after_timeout() {
    let extra = "sandbox_gate: closed\nevents:\n  - {at: {op: may_delete, nth: 1, when: before}, advance: 20m}\n";
    let host = common::host(&model(extra));
    let report = executor::ensure(&host, &request(), &[1000], &mut |_| {});
    assert_eq!(report.status, Status::UsageError, "{report:#?}");
    assert_eq!(report.exit_code(), 2);
    assert!(
        report
            .overruns
            .iter()
            .any(|entry| { entry.operation == "sandbox_gate" && entry.over_ms > 0 }),
        "{report:#?}"
    );
    assert!(
        host.trace()
            .iter()
            .all(|call| call.op != Op::Statvfs && call.op != Op::Unlinkat)
    );
}

#[test]
fn grouped_sandbox_gate_overrun_samples_before_status() {
    let extra = "events:\n  - {at: {op: may_delete, nth: 1, when: before}, advance: 20m}\n";
    let host = common::host(&model(extra));
    let req = request();
    let report = executor::ensure_grouped(
        &host,
        &req,
        std::slice::from_ref(&req.tier),
        &[Duration::from_secs(10)],
        &[1000],
        &mut |_| {},
    );
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert!(report.avail_before_bytes.is_some(), "{report:#?}");
    assert!(
        report
            .overruns
            .iter()
            .any(|entry| { entry.operation == "sandbox_gate" && entry.over_ms > 0 }),
        "{report:#?}"
    );
    assert!(
        host.trace()
            .iter()
            .all(|call| call.op != Op::Flock && call.op != Op::Unlinkat)
    );
}

#[test]
fn dry_run_measure_overrun_publishes_no_partial_plan() {
    let extra = "events:\n  - {at: {op: statvfs, path: /home/mark/src, nth: 1, when: before}, advance: 20m}\n";
    let host = common::host(&model(extra));
    let report = executor::dry_run(&host, &request(), &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert!(
        report
            .overruns
            .iter()
            .any(|entry| entry.operation == "measure" && entry.over_ms > 0),
        "{report:#?}"
    );
    assert!(report.plan.is_empty());
    assert!(host.trace().iter().all(|call| call.op != Op::Unlinkat));
}

#[test]
fn command_preflight_overrun_samples_before_choosing_exit_status() {
    let extra = "  /usr/bin/pip: {mode: '0755'}\nevents:\n  - {at: {op: executable_file, path: /usr/bin/pip, nth: 1, when: before}, advance: 20m}\n";
    let host = common::host(&model(extra));
    let report = executor::ensure(&host, &command_request(), &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert!(report.avail_before_bytes.is_some(), "{report:#?}");
    assert!(
        report
            .overruns
            .iter()
            .any(|entry| entry.operation == "preflight" && entry.over_ms > 0),
        "{report:#?}"
    );
    assert!(host.trace().iter().all(|call| call.op != Op::Unlinkat));
}

#[test]
fn command_preflight_overrun_exits_met_when_space_appeared() {
    let extra = "  /usr/bin/pip: {mode: '0755'}\nevents:\n  - {at: {op: executable_file, path: /usr/bin/pip, nth: 1, when: before}, advance: 20m}\n  - {at: {elapsed: 1s}, remove: /home/mark/src/old/target}\n";
    let host = common::host(&model(extra));
    let report = executor::ensure(&host, &command_request(), &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(report.exit_code(), 0);
    assert!(
        report
            .overruns
            .iter()
            .any(|entry| entry.operation == "preflight" && entry.over_ms > 0),
        "{report:#?}"
    );
    assert!(host.trace().iter().all(|call| call.op != Op::Unlinkat));
}

#[test]
fn command_preflight_overrun_with_failed_first_sample_is_host_error() {
    let extra = "  /usr/bin/pip: {mode: '0755'}\nevents:\n  - {at: {op: executable_file, path: /usr/bin/pip, nth: 1, when: before}, advance: 20m}\n  - {at: {op: openat2, path: /home/mark/src, nth: 1, when: before}, fail: EIO}\n";
    let host = common::host(&model(extra));
    let report = executor::ensure(&host, &command_request(), &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert_eq!(
        report.errors[0].cause,
        diskgc_core::report::ErrorCause::Host
    );
    assert!(
        report
            .overruns
            .iter()
            .any(|entry| entry.operation == "preflight" && entry.over_ms > 0),
        "{report:#?}"
    );
    assert!(host.trace().iter().all(|call| call.op != Op::Unlinkat));
}

#[test]
fn host_lock_operation_overrun_is_reported_before_any_delete() {
    let extra = "events:\n  - {at: {op: flock, path: /run/diskgc/lock, nth: 1, when: before}, advance: 20m}\n";
    let host = common::host(&model(extra));
    let report = executor::ensure(&host, &request(), &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert!(
        report
            .overruns
            .iter()
            .any(|entry| entry.operation == "host_lock" && entry.over_ms > 0),
        "{report:#?}"
    );
    assert!(host.trace().iter().all(|call| call.op != Op::Unlinkat));
}

#[test]
fn root_preflight_overrun_stops_before_discovery() {
    let extra = "events:\n  - {at: {op: mountinfo, nth: 3, when: before}, advance: 20m}\n";
    let host = common::host(&model(extra));
    let report = executor::ensure(&host, &request(), &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert!(
        report
            .overruns
            .iter()
            .any(|entry| entry.operation == "preflight" && entry.over_ms > 0),
        "{report:#?}"
    );
    assert!(host.trace().iter().all(|call| call.op != Op::Unlinkat));
}

#[test]
fn protection_scan_overrun_stops_before_discovery() {
    let extra = "  /home/mark/guard: {}\nevents:\n  - {at: {op: openat2, path: /home/mark/guard, nth: 1, when: before}, advance: 20m}\n";
    let host = common::host(&model(extra));
    let mut req = request();
    req.tier.protections.push(ProtectedSource {
        profile: "guard".to_owned(),
        roots: vec![PathBuf::from("/home/mark/guard")],
        kind: ProtectedKind::CargoTarget,
    });
    let report = executor::ensure(&host, &req, &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert!(
        report
            .overruns
            .iter()
            .any(|entry| entry.operation == "protect" && entry.over_ms > 0),
        "{report:#?}"
    );
    assert!(host.trace().iter().all(|call| call.op != Op::Unlinkat));
}

#[test]
fn post_run_in_use_scan_overrun_is_reported() {
    let extra = "events:\n  - {at: {op: pids, nth: 1, when: before}, advance: 20m}\n";
    let host = common::host(&model(extra));
    let mut req = request();
    req.tier.roots.clear();
    let report = executor::ensure(&host, &req, &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert!(
        report
            .overruns
            .iter()
            .any(|entry| entry.operation == "in_use" && entry.over_ms > 0),
        "{report:#?}"
    );
    assert!(host.trace().iter().all(|call| call.op != Op::Unlinkat));
}

#[test]
fn final_sample_overrun_is_reported() {
    let extra = "events:\n  - {at: {op: statvfs, path: /home/mark/src, nth: 4, when: before}, advance: 20m}\n";
    let host = common::host(&model(extra));
    let mut req = request();
    req.tier.roots.clear();
    let report = executor::ensure(&host, &req, &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert!(
        report
            .overruns
            .iter()
            .any(|entry| entry.operation == "statvfs" && entry.over_ms > 0),
        "{report:#?}"
    );
    assert!(host.trace().iter().all(|call| call.op != Op::Unlinkat));
}

#[test]
fn delayed_block_release_is_observed_before_the_next_candidate() {
    let extra = "  /usr/bin/tail: {}\nprocesses:\n  - {pid: 7, exe: /usr/bin/tail, cwd: /, maps: [/usr/bin/tail], fds: [{path: /usr/bin/tail}], environ: [PATH=/usr/bin]}\nevents:\n  - {at: {op: unlinkat, path: /home/mark/src/old/target/debug/deps/p-1, when: before}, open: {pid: 7, path: /home/mark/src/old/target/debug/deps/p-1}}\n  - {at: {elapsed: 1s}, exit: 7}\n";
    let host = common::host(&model(extra));
    let mut req = request();
    req.target_free = Some(parse_size("15G").unwrap());
    let report = executor::ensure(&host, &req, &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert!(host.elapsed() >= Duration::from_secs(1));
    assert!(host.elapsed() <= Duration::from_secs(2));
    assert!(
        host.stat_path(Path::new("/home/mark/src/new/target"))
            .is_some()
    );
    assert!(
        host.trace()
            .iter()
            .filter(|call| call.op == Op::Unlinkat)
            .all(|call| {
                !call
                    .path
                    .as_ref()
                    .is_some_and(|path| path.starts_with("/home/mark/src/new"))
            }),
        "{report:#?}"
    );
}
