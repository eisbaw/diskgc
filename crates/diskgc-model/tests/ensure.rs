//! The minimal `ensure` (diskgc-core `executor`, task-10) on ModelHost:
//! thresholds, the host lock, the sandbox gate, privileges, the exit codes,
//! the order of a `cargo-target` tier and its goal, and `--dry-run`. Every
//! call is on the trace, so "without taking the lock" and "deletes
//! nothing" are asserted on what was called, not on what was reported.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::config;
use diskgc_core::engine::Reason;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::hostlock::TMPFILES_LINES;
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::report::{ErrorCause, Report, Status};
use diskgc_core::units::{Size, format_bytes, parse_size};
use diskgc_model::harness::{self, Outcome, fixture_path, load_fixture};
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
        watch: None,
        tier: Tier::select(select, &library, Some(&roots), 1000).unwrap_or_else(|e| panic!("{e}")),
    }
}

fn cargo(min: &str, target: Option<&str>) -> Request {
    request("profile:cargo-target", &[SRC], SRC, min, target)
}

#[test]
fn run_report_carries_utc_start_duration_and_build_version() {
    let host = host(&yaml(true, ""));
    let report = dry_run(&host, &cargo("100G", None));
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
    assert_eq!(json["started_at"], "2026-10-15T03:00:00Z");
    assert!(json["duration_ms"].as_u64().is_some());
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn action_age_uses_the_clock_before_reclaim_even_if_reclaim_advances_time() {
    let host = host(&yaml(
        true,
        "events:\n  - {at: {op: unlinkat, nth: 1, when: after}, advance: 1h}\n",
    ));
    let mut request = cargo("12G", Some("19G"));
    request.timeout = Some(Duration::from_secs(2 * 3600));
    let report = ensure(&host, &request);
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert!(!report.actions.is_empty(), "{report:#?}");
    assert_eq!(
        report.actions[0].idle_age_seconds,
        Some(14 * 86_400 + 3 * 3600)
    );
    assert!(report.duration_ms.unwrap() >= 3_600_000);
}

#[test]
fn cargo_protect_uses_its_own_bound_roots_in_dry_run() {
    let config = config::parse(
        Path::new("/tmp/protect.yaml"),
        b"version: 1\nprotect: [{profile: cargo-target, roots: [/home/mark/src]}]\n",
    )
    .unwrap();
    let mut request = cargo("50G", None);
    request.tier.protections = executor::bind_protections(&config, 1000, false).unwrap();
    let host = host(&yaml(true, ""));
    let report = dry_run(&host, &request);
    assert_eq!(report.status, Status::DryRun);
    for path in [OLD, NEW] {
        assert_eq!(
            report
                .plan
                .iter()
                .find(|entry| entry.candidate.as_deref() == Some(Path::new(path)))
                .map(|entry| entry.reason),
            Some(Some(Reason::Protected)),
            "{path}"
        );
    }
    assert!(calls(&host, Op::Unlinkat).is_empty());
    assert!(calls(&host, Op::MayDelete).is_empty());
}

/// TASK-260, kind `cargo-target` on both sides of the verdict: `/mnt/old`
/// binds `/home/mark/src/old` (mountinfo root `/mark/src/old` of `/home`'s
/// filesystem), a subdirectory of the protected match `/home/mark/src`.
const CARGO_BIND_POLICY: &str = r#"version: 1
profiles:
  - name: src-tree
    find: {roots: [/home/mark], paths: [src], type: dir}
    action: delete
    examples: {match: [/home/mark/src]}
  - name: projects
    find: {roots: required, paths: ['*'], type: dir}
    action: delete
    examples: {root: /srv, match: [/srv/x]}
"#;

fn cargo_bind_request(protect: &str, select: &str, root: &str) -> Request {
    let text = format!("{CARGO_BIND_POLICY}protect: [{protect}]\n");
    let config = config::parse(Path::new("/tmp/protect.yaml"), text.as_bytes()).unwrap();
    let roots = [root.to_owned()];
    let mut request = Request {
        path: PathBuf::from(SRC),
        min_free: size("50G"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier: Tier::select(select, &config.library, Some(&roots), 1000)
            .unwrap_or_else(|e| panic!("{e}")),
    };
    request.tier.protections = executor::bind_protections(&config, 1000, false).unwrap();
    request
}

fn cargo_bind_yaml() -> String {
    yaml(
        true,
        "  /home/mark/other/f: {mtime: 2026-10-01T00:00:00Z}\n",
    )
    .replacen(
        "tree:\n",
        "  - {id: 40, at: /mnt/old, fs: ext4, dev: \"254:2\", size: 100G, other_used: 80G, \
         parent: 1, bind_of: 2, bind_root: /mark/src/old}\ntree:\n",
        1,
    )
}

#[test]
fn a_cargo_target_under_a_bind_of_a_protected_subdirectory_is_protected() {
    let host = host(&cargo_bind_yaml());
    let report = dry_run(
        &host,
        &cargo_bind_request("src-tree", "profile:cargo-target", "/mnt/old"),
    );
    assert_eq!(
        plan_reason(&report, "/mnt/old/target"),
        Some(Some(Reason::Protected)),
        "{report:#?}"
    );
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

#[test]
fn a_protected_cargo_target_seen_through_a_bind_protects_its_source_tree() {
    let host = host(&cargo_bind_yaml());
    let report = dry_run(
        &host,
        &cargo_bind_request(
            "{profile: cargo-target, roots: [/mnt/old]}",
            "profile:projects",
            "/home/mark",
        ),
    );
    // `/home/mark/src` holds the bind's source; no ancestor of it is an
    // ancestor of the match `/mnt/old/target`.
    assert_eq!(
        plan_reason(&report, "/home/mark/src"),
        Some(Some(Reason::Protected)),
        "{report:#?}"
    );
    assert_eq!(
        plan_reason(&report, "/home/mark/other"),
        Some(None),
        "{report:#?}"
    );
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

/// Cargo discovery never descends into a found target, so a protected
/// target holding a mount is still a match (an `fs` match holding one stops
/// the run). A second mount of that filesystem elsewhere shows part of the
/// protected target: `/home/mark/src/old/target/debug/deps/fast` is a tmpfs
/// (0:60) that `/mnt/fast` binds too.
#[test]
fn a_filesystem_mounted_inside_a_protected_cargo_target_is_protected_elsewhere() {
    let model = yaml(
        true,
        "  /home/mark/src/old/target/debug/deps/fast/y/f: {}\n",
    )
    .replacen(
        "tree:\n",
        "  - {id: 60, at: /home/mark/src/old/target/debug/deps/fast, fs: tmpfs, dev: \"0:60\", \
         size: 1G, parent: 2}\n  - {id: 61, at: /mnt/fast, fs: tmpfs, dev: \"0:60\", size: 1G, \
         parent: 1, bind_of: 60}\ntree:\n",
        1,
    );
    let host = host(&model);
    let text = format!("{CARGO_BIND_POLICY}protect: [{{profile: cargo-target, roots: [{SRC}]}}]\n");
    let config = config::parse(Path::new("/tmp/protect.yaml"), text.as_bytes()).unwrap();
    let roots = ["/mnt/fast".to_owned()];
    let mut request = Request {
        path: PathBuf::from("/mnt/fast"),
        min_free: size("100%"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier: Tier::select("profile:projects", &config.library, Some(&roots), 1000)
            .unwrap_or_else(|e| panic!("{e}")),
    };
    request.tier.protections = executor::bind_protections(&config, 1000, false).unwrap();
    let report = dry_run(&host, &request);
    assert_eq!(
        plan_reason(&report, "/mnt/fast/y"),
        Some(Some(Reason::Protected)),
        "{report:#?}"
    );
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

fn configured(policy: &str) -> (Vec<Tier>, Vec<Duration>) {
    let config = config::parse(Path::new("/tmp/diskgc-test-policy.yaml"), policy.as_bytes())
        .unwrap_or_else(|error| panic!("{error}"));
    let mut tiers = Vec::new();
    let mut timeouts = Vec::new();
    for group in &config.groups {
        for setting in &group.tiers {
            tiers.push(
                Tier::configured(&group.name, setting, &config.library, 1000)
                    .unwrap_or_else(|error| panic!("{error}")),
            );
            timeouts.push(setting.timeout);
        }
    }
    (tiers, timeouts)
}

fn grouped(host: &ModelHost, policy: &str, min: &str, target: Option<&str>) -> Report {
    let (tiers, timeouts) = configured(policy);
    let mut request = cargo(min, target);
    request.tier = tiers[0].clone();
    let mut lines = Vec::new();
    let report =
        executor::ensure_grouped(host, &request, &tiers, &timeouts, &[1000], &mut |line| {
            lines.push(line.to_owned())
        });
    eprintln!("log:\n  {}", lines.join("\n  "));
    report
}

#[test]
fn configured_stale_tier_finishes_before_pressure_and_target_stops_pressure() {
    let policy = "version: 1\ngroups:\n  - name: stale\n    tiers:\n      - name: old\n        select: profile:cargo-target && filter:idle-7d\n        roots: [/home/mark/src]\n  - name: pressure\n    tiers:\n      - name: recent\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n";
    let host = host(&yaml(true, ""));
    let report = grouped(&host, policy, "12G", Some("15G"));
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(report.actions.len(), 1, "{report:#?}");
    assert_eq!(report.actions[0].tier, "old");
    assert_eq!(report.actions[0].candidate, Path::new(OLD));
    assert!(
        calls(&host, Op::Flock)
            .iter()
            .all(|call| !touches(call, NEW))
    );
    let host = common::host(&yaml(true, ""));
    let report = grouped(&host, policy, "12G", Some("19G"));
    assert_eq!(
        report
            .actions
            .iter()
            .map(|action| action.tier.as_str())
            .collect::<Vec<_>>(),
        ["old", "recent"],
        "{report:#?}"
    );
    assert_eq!(report.actions[1].candidate, Path::new(NEW));
}

#[test]
fn pressure_waits_for_every_discoverable_eligible_stale_candidate() {
    let policy = "version: 1\ngroups:\n  - name: stale\n    tiers:\n      - name: old\n        select: profile:cargo-target && filter:idle-7d\n        roots: [/home/mark/src]\n  - name: pressure\n    tiers:\n      - name: recent\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n";
    let extra = format!(
        "  /home/mark/src/old2/Cargo.toml: {{}}\n  /home/mark/src/old2/target:\n    CACHEDIR.TAG: {{cachedir_tag: true}}\n    debug: {}\n",
        profile("2G", "2026-10-05T00:00:00Z")
    );
    let host = host(&yaml(true, &extra));
    let report = grouped(&host, policy, "12G", Some("19G"));
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(
        report
            .actions
            .iter()
            .map(|action| (action.tier.as_str(), action.candidate.as_path()))
            .collect::<Vec<_>>(),
        [
            ("old", Path::new(OLD)),
            ("old", Path::new("/home/mark/src/old2/target")),
            ("recent", Path::new(NEW)),
        ],
        "{report:#?}"
    );
}

#[test]
fn misconfigured_group_tier_is_reported_and_next_tier_runs() {
    let policy = "version: 1\ngroups:\n  - name: stale\n    tiers:\n      - name: bad\n        select: profile:cargo-target\n        roots: [/home/mark/link]\n      - name: good\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n";
    let model = yaml(true, "").replacen(
        "tree:\n",
        "tree:\n  /home/mark/link: {symlink: /home/mark/src}\n",
        1,
    );
    let host = common::host(&model);
    let report = grouped(&host, policy, "12G", Some("15G"));
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(report.actions.len(), 1, "{report:#?}");
    assert_eq!(report.actions[0].tier, "good");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.tier.as_deref() == Some("bad")
                && error.message.contains("misconfigured")),
        "{report:#?}"
    );
    assert!(
        calls(&host, Op::Unlinkat)
            .iter()
            .all(|call| !touches(call, "/home/mark/link")),
        "excluded tier reached the delete port: {:#?}",
        host.trace()
    );
}

#[test]
fn every_tier_environment_is_preflighted_before_the_first_deletion() {
    let policy = "version: 1\nprofiles:\n  - name: later-command\n    find: {roots: [/home/mark/empty], paths: [unused-*], type: dir}\n    action: {command: [/usr/bin/rm, -rf, \"{path}\"]}\n    examples: {match: [/home/mark/empty/unused-a]}\ngroups:\n  - name: stale\n    tiers:\n      - name: old\n        select: profile:cargo-target && filter:idle-7d\n        roots: [/home/mark/src/old]\n  - name: pressure\n    tiers:\n      - name: recent\n        select: profile:cargo-target\n        roots: [/home/mark/src/new]\n  - name: later\n    tiers:\n      - name: bad-root\n        select: profile:cargo-target\n        roots: [/home/mark/link]\n      - name: command\n        select: profile:later-command\n";
    let model = yaml(true, "").replacen(
        "tree:\n",
        "tree:\n  /home/mark/empty: {}\n  /home/mark/link: {symlink: /home/mark/src}\n  /usr/bin/rm: {mode: \"0755\"}\n",
        1,
    );
    let host = host(&model);
    let report = grouped(&host, policy, "12G", Some("15G"));
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(report.actions[0].tier, "old");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.tier.as_deref() == Some("bad-root"))
    );

    let trace = host.trace();
    let first_delete = trace
        .iter()
        .position(|call| call.op == Op::Unlinkat)
        .expect("the stale tier deletes");
    for (op, path) in [
        (Op::Openat2, "/home/mark/src/old"),
        (Op::Openat2, "/home/mark/src/new"),
        (Op::Openat2, "/home/mark/link"),
        (Op::Openat2, "/home/mark/empty"),
        (Op::ExecutableFile, "/usr/bin/rm"),
    ] {
        let preflight = trace[..first_delete]
            .iter()
            .position(|call| call.op == op && touches(call, path));
        assert!(
            preflight.is_some(),
            "{op:?} {path} was not preflighted before deletion: {trace:#?}"
        );
    }
}

#[test]
fn all_command_preflights_finish_before_first_group_deletion() {
    let policy = "version: 1\nprofiles:\n  - name: later-command\n    find: {roots: [/home/mark/src], paths: [unused-*], type: dir}\n    action: {command: [/usr/bin/rm, -rf, \"{path}\"]}\n    examples: {match: [/home/mark/src/unused-a]}\ngroups:\n  - name: stale\n    tiers:\n      - name: old\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n      - name: command\n        select: profile:later-command\n";
    let model = yaml(true, "").replacen("tree:\n", "tree:\n  /usr/bin/rm: {mode: \"0755\"}\n", 1);
    let host = host(&model);
    let report = grouped(&host, policy, "12G", Some("15G"));
    assert_eq!(report.status, Status::Met, "{report:#?}");
    let trace = host.trace();
    let preflight = trace
        .iter()
        .rposition(|call| call.op == Op::ExecutableFile && touches(call, "/usr/bin/rm"))
        .expect("later tier executable checked");
    let deletion = trace
        .iter()
        .position(|call| call.op == Op::Unlinkat)
        .expect("first tier deletes");
    assert!(preflight < deletion, "{trace:#?}");
}

#[test]
fn later_unsafe_root_is_checked_before_earlier_tier_deletes() {
    let policy = "version: 1\ngroups:\n  - name: stale\n    tiers:\n      - name: old\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n      - name: bad-later\n        select: profile:cargo-target\n        roots: [/home/mark/link]\n";
    let model = yaml(true, "").replacen(
        "tree:\n",
        "tree:\n  /home/mark/link: {symlink: /home/mark/src}\n",
        1,
    );
    let host = host(&model);
    let report = grouped(&host, policy, "12G", Some("15G"));
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.tier.as_deref() == Some("bad-later")),
        "{report:#?}"
    );
    let trace = host.trace();
    let check = trace
        .iter()
        .rposition(|call| touches(call, "/home/mark/link") && call.op == Op::Openat2)
        .expect("unsafe root resolved in preflight");
    let delete = trace
        .iter()
        .position(|call| call.op == Op::Unlinkat)
        .expect("earlier tier deletes");
    assert!(check < delete, "{trace:#?}");
}

#[test]
fn later_foreign_root_is_reported_before_earlier_tier_deletes() {
    let policy = "version: 1\ngroups:\n  - name: stale\n    tiers:\n      - name: old\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n      - name: foreign-later\n        select: profile:cargo-target\n        roots: [/home/mark/src/foreign]\n";
    let model = yaml(true, "").replacen(
        "tree:\n",
        "  - {id: 5, at: /home/mark/src/foreign, fs: ext4, dev: \"254:9\", size: 10G, parent: 2}\ntree:\n",
        1,
    );
    let host = host(&model);
    let report = grouped(&host, policy, "12G", Some("15G"));
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(report.actions.len(), 1, "{report:#?}");
    assert_eq!(report.actions[0].tier, "old");
    assert!(report.errors.is_empty(), "{report:#?}");
    assert!(
        report.skipped.iter().any(|skip| {
            skip.tier == "foreign-later"
                && skip.candidate.as_deref() == Some(Path::new("/home/mark/src/foreign"))
                && skip.reason == Reason::OtherFs
        }),
        "{report:#?}"
    );
    let trace = host.trace();
    let preflight = trace
        .iter()
        .rposition(|call| call.op == Op::Statx && touches(call, "/home/mark/src/foreign"))
        .expect("nested foreign root statted in preflight");
    let deletion = trace
        .iter()
        .position(|call| call.op == Op::Unlinkat)
        .expect("first tier deletes");
    assert!(preflight < deletion, "{trace:#?}");
}

#[test]
fn a_same_device_root_remains_eligible_for_preflight() {
    let policy = "version: 1\ngroups:\n  - name: stale\n    tiers:\n      - name: old\n        select: profile:cargo-target\n        roots: [/home/mark/src/old]\n";
    let host = host(&yaml(true, ""));
    let report = grouped(&host, policy, "12G", Some("15G"));
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert!(report.errors.is_empty(), "{report:#?}");
    assert_eq!(report.actions[0].candidate, Path::new(OLD));
}

#[test]
fn configured_dry_run_plans_each_tier_without_the_delete_port() {
    let policy = "version: 1\ngroups:\n  - name: stale\n    tiers:\n      - name: old\n        select: profile:cargo-target && filter:idle-7d\n        roots: [/home/mark/src]\n  - name: pressure\n    tiers:\n      - name: recent\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n";
    let (tiers, timeouts) = configured(policy);
    let host = host(&yaml(true, ""));
    let mut request = cargo("12G", Some("15G"));
    request.tier = tiers[0].clone();
    let report =
        executor::dry_run_grouped(&host, &request, &tiers, &timeouts, &[1000], &mut |_| {});
    assert_eq!(report.status, Status::DryRun, "{report:#?}");
    assert!(
        report.plan.iter().any(|entry| entry.group == "stale"
            && entry.tier == "old"
            && entry.candidate.as_deref() == Some(Path::new(OLD))),
        "{report:#?}"
    );
    assert!(
        report.plan.iter().any(|entry| entry.group == "pressure"
            && entry.tier == "recent"
            && entry.candidate.as_deref() == Some(Path::new(NEW))),
        "{report:#?}"
    );
    assert!(calls(&host, Op::MayDelete).is_empty());
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

#[test]
fn configured_dry_run_continues_after_one_tier_budget_expires() {
    let policy = "version: 1\ngroups:\n  - name: g\n    tiers:\n      - name: quick\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n        timeout: 1s\n      - name: later\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n        timeout: 30s\n";
    let (tiers, timeouts) = configured(policy);
    let events = "events:\n  - {at: {op: statvfs, path: /home/mark/src, nth: 1, when: after}, advance: 2s}\n";
    let host = host(&yaml(true, events));
    let mut request = cargo("12G", Some("15G"));
    request.tier = tiers[0].clone();
    let report =
        executor::dry_run_grouped(&host, &request, &tiers, &timeouts, &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.tier.as_deref() == Some("quick")
                && error.message.contains("deadline")),
        "{report:#?}"
    );
    assert!(
        report
            .plan
            .iter()
            .any(|entry| entry.tier == "later" && entry.reason.is_none()),
        "{report:#?}"
    );
    assert!(calls(&host, Op::Flock).is_empty());
    assert!(calls(&host, Op::Unlinkat).is_empty());
    assert!(host.unfired_events().is_empty());
}

#[test]
fn configured_dry_run_reports_later_tier_budget_expiry_as_error() {
    let policy = "version: 1\ngroups:\n  - name: g\n    tiers:\n      - name: first\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n        timeout: 30s\n      - name: quick\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n        timeout: 1s\n";
    let (tiers, timeouts) = configured(policy);
    let events = "events:\n  - {at: {op: statvfs, path: /home/mark/src, nth: 3, when: after}, advance: 2s}\n";
    let host = host(&yaml(true, events));
    let mut request = cargo("12G", Some("15G"));
    request.tier = tiers[0].clone();
    let report =
        executor::dry_run_grouped(&host, &request, &tiers, &timeouts, &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.tier.as_deref() == Some("quick")
                && error.message.contains("deadline")),
        "{report:#?}"
    );
    assert!(
        report
            .plan
            .iter()
            .any(|entry| entry.tier == "first" && entry.reason.is_none()),
        "{report:#?}"
    );
    assert!(calls(&host, Op::Flock).is_empty());
    assert!(calls(&host, Op::Unlinkat).is_empty());
    assert!(host.unfired_events().is_empty());
}

/// Three tiers whose plans each find the same Cargo targets.
const THREE_TIERS: &str = "version: 1\ngroups:\n  - name: stale\n    tiers:\n      - name: first\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n      - name: second\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n  - name: pressure\n    tiers:\n      - name: third\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n";

/// A grouped dry run whose overall `--timeout` (10 min) passes once the
/// `nth` sample of the filesystem returns.
fn grouped_dry_run_past_the_deadline_at_sample(nth: u32) -> (ModelHost, Report) {
    let (tiers, timeouts) = configured(THREE_TIERS);
    let events = format!(
        "events:\n  - {{at: {{op: statvfs, path: /home/mark/src, nth: {nth}, when: after}}, advance: 11m}}\n"
    );
    let host = host(&yaml(true, &events));
    let mut request = cargo("12G", Some("15G"));
    request.tier = tiers[0].clone();
    let report =
        executor::dry_run_grouped(&host, &request, &tiers, &timeouts, &[1000], &mut |_| {});
    (host, report)
}

fn unplanned_tiers(report: &Report) -> Vec<&str> {
    report
        .errors
        .iter()
        .filter(|error| error.message == "deadline passed before this tier was planned")
        .map(|error| error.tier.as_deref().expect("a tier's error names it"))
        .collect()
}

/// TASK-232: the overall deadline passing after a tier's plan completed
/// stops the grouped plan, and `errors` names every tier left unplanned
/// (docs/reference.md `--dry-run`: the deadline cut the plan short).
#[test]
fn a_grouped_dry_run_past_the_deadline_names_every_tier_it_did_not_plan() {
    // Sample 2 ends the first tier's plan, sample 4 the second's.
    for (nth, planned, unplanned) in [
        (2, &["first"][..], &["second", "third"][..]),
        (4, &["first", "second"][..], &["third"][..]),
    ] {
        let (host, report) = grouped_dry_run_past_the_deadline_at_sample(nth);
        assert!(
            host.unfired_events().is_empty(),
            "{nth}: {:#?}",
            host.trace()
        );
        assert_eq!(report.status, Status::Deadline, "{nth}: {report:#?}");
        assert_eq!(report.exit_code(), 124);
        assert_eq!(unplanned_tiers(&report), unplanned, "{nth}: {report:#?}");
        for tier in planned {
            assert!(
                report
                    .plan
                    .iter()
                    .any(|entry| entry.tier == *tier && entry.reason.is_none()),
                "{nth}: {tier} keeps its judged rows: {report:#?}"
            );
            assert!(
                report
                    .errors
                    .iter()
                    .all(|error| error.tier.as_deref() != Some(*tier)),
                "{nth}: {tier} was planned in full: {report:#?}"
            );
        }
        for tier in unplanned {
            assert!(
                report.plan.iter().all(|entry| entry.tier != *tier),
                "{nth}: {report:#?}"
            );
        }
        assert!(calls(&host, Op::Unlinkat).is_empty());
    }
}

/// A tier whose own budget runs out after its plan completed (in the
/// process scan) leaves nothing missing from the grouped plan: no error,
/// and the complete plan is a successful dry run, for the first tier and
/// for a later one.
#[test]
fn a_tier_budget_spent_after_its_plan_completed_is_no_grouped_error() {
    // Sample 2 ends the first tier's plan, sample 4 the second's.
    for (quick, nth) in [(0, 2), (1, 4)] {
        let mut policy = "version: 1\ngroups:\n  - name: g\n    tiers:\n".to_owned();
        for (index, name) in ["one", "two", "three"].into_iter().enumerate() {
            let timeout = if index == quick { "1s" } else { "30s" };
            policy.push_str(&format!(
                "      - name: {name}\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n        timeout: {timeout}\n"
            ));
        }
        let (tiers, timeouts) = configured(&policy);
        let events = format!(
            "events:\n  - {{at: {{op: statvfs, path: /home/mark/src, nth: {nth}, when: after}}, advance: 2s}}\n"
        );
        let host = host(&yaml(true, &events));
        let mut request = cargo("12G", Some("15G"));
        request.tier = tiers[0].clone();
        let report =
            executor::dry_run_grouped(&host, &request, &tiers, &timeouts, &[1000], &mut |_| {});
        assert!(
            host.unfired_events().is_empty(),
            "{quick}: {:#?}",
            host.trace()
        );
        assert_eq!(report.status, Status::DryRun, "{quick}: {report:#?}");
        assert_eq!(report.exit_code(), 0);
        assert!(report.errors.is_empty(), "{quick}: {report:#?}");
        for tier in ["one", "two", "three"] {
            assert!(
                report
                    .plan
                    .iter()
                    .any(|entry| entry.tier == tier && entry.reason.is_none()),
                "{quick}: {tier}: {report:#?}"
            );
        }
    }
}

#[test]
fn failed_space_sample_in_first_tier_stops_later_tiers_without_deletion() {
    let policy = "version: 1\ngroups:\n  - name: stale\n    tiers:\n      - name: first\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n      - name: later\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n";
    // statvfs: the first sample, under the host lock, then the first
    // tier's own start (TASK-270: the first tier to run starts from the
    // sample under the lock; only later tiers are sampled before they run).
    let model = format!(
        "{}events:\n  - {{at: {{op: statvfs, path: /home/mark/src, nth: 3, when: before}}, fail: EIO}}\n",
        yaml(true, "")
    );
    let host = host(&model);
    let report = grouped(&host, policy, "12G", None);
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.tier.as_deref() == Some("first")
                && error.message.contains("sampling available space")),
        "{report:#?}"
    );
    assert!(report.actions.is_empty(), "{report:#?}");
    assert!(
        calls(&host, Op::Unlinkat).is_empty(),
        "later tier deleted after unknown space"
    );
}

fn touches(call: &Call, below: &str) -> bool {
    call.path.as_deref().is_some_and(|p| p.starts_with(below))
}

/// Below `/run/diskgc` nothing was locked: only looked up (`openat2`,
/// `statx`) and reopened read-only (`reopen_regular`), as the readiness
/// check does (TASK-277).
fn host_lock_only_looked_up(host: &ModelHost) -> bool {
    host.trace().iter().all(|c| {
        !touches(c, "/run/diskgc") || matches!(c.op, Op::Openat2 | Op::Statx | Op::ReopenRegular)
    })
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
/// success or an exit failure. A missing one warns only root: a user's
/// reclaim falls back to its per-user lock (TASK-277), which a met ensure
/// neither makes nor mentions.
#[test]
fn above_min_free_warns_about_missing_or_unusable_lock_without_taking_it() {
    for (case, model, root, expected) in [
        (
            "missing, root",
            format!("own_uid: 0\n{}", yaml(false, "")),
            true,
            "host lock /run/diskgc/lock is missing",
        ),
        ("missing, user", yaml(false, ""), false, ""),
        (
            "fifo",
            yaml(false, "").replacen(
                "tree:\n",
                "tree:\n  /run/diskgc/lock: {type: fifo, uid: 0}\n",
                1,
            ),
            false,
            "host lock /run/diskgc/lock is unusable",
        ),
        ("ready", yaml(true, ""), false, ""),
    ] {
        let host = host(&model);
        let uid = executor::privileges(&host, root).unwrap();
        let owners = if root { vec![1000] } else { vec![uid] };
        let mut lines = Vec::new();
        let report = executor::ensure(&host, &cargo("1G", None), &owners, &mut |line| {
            lines.push(line.to_owned());
        });
        let output = lines.join("\n");
        assert_eq!(report.status, Status::Met, "{case}: {report:?}");
        assert_eq!(report.exit_code(), 0);
        if expected.is_empty() {
            assert!(
                !output.contains("host lock is not ready"),
                "{case}: {output}"
            );
            assert!(!output.contains("per-user lock"), "{case}: {output}");
        } else {
            assert!(
                output.contains("warning: host lock is not ready"),
                "{case}: {output}"
            );
            assert!(output.contains(expected), "{case}: {output}");
        }
        assert!(
            calls(&host, Op::Flock).is_empty(),
            "{case}: {:#?}",
            host.trace()
        );
        assert!(calls(&host, Op::UserLock).is_empty(), "{case}: a lock made");
        assert!(calls(&host, Op::Unlinkat).is_empty(), "{case}");
        assert_eq!(report.avail_after_bytes, report.avail_before_bytes);
        assert_eq!(report.mount.as_deref(), Some(Path::new("/home")));
        assert_eq!(report.summary(), None, "nothing needed doing: silent");
    }
}

// ---- The goal before the process list's visibility (TASK-298) ----

/// The facts that take processes out of the listing (docs/safety.md, In
/// use: Visibility, a process list that cannot be complete).
const INCOMPLETE_PROCESS_LISTS: [(&str, &str); 2] = [
    (
        "proc_hidepid: true\n",
        "the held /proc is mounted with hidepid",
    ),
    (
        "pid_namespace: nested\n",
        "diskgc is not in the initial PID namespace",
    ),
];

/// Every call that reads a process, the PID namespace facts, a loop
/// device or takes or makes a lock.
fn reads_processes_or_locks(op: Op) -> bool {
    matches!(
        op,
        Op::Pids
            | Op::ProcIdentity
            | Op::ProcIdentityReleased
            | Op::ProcFds
            | Op::ProcLinks
            | Op::ProcMaps
            | Op::ProcEnviron
            | Op::ProcStatus
            | Op::ProcNspid
            | Op::ProcUids
            | Op::ProcCmdline
            | Op::ProcRootLookup
            | Op::PidAlive
            | Op::PidNamespace
            | Op::BoundUnixSockets
            | Op::LoopBackingFiles
            | Op::LoopBackingFile
            | Op::LoopBackingIdentity
            | Op::Flock
            | Op::InheritLock
            | Op::UserLock
            | Op::StateLock
            | Op::StateWrite
    )
}

/// PRD: `ensure` exits 0 without taking the lock when the space is already
/// there, also where diskgc cannot see every process: the met run is the
/// usual no-op, call for call the same as with every process visible, and
/// reads no process and no lock on the way, only diskgc's own namespace
/// facts (premises a to c of TASK-298: the sample needs no process,
/// nothing between it and the exit reads another one, no lock is taken or
/// made). It says in one line that a reclaim here would be refused, and
/// why; with every process visible it says nothing of the kind.
#[test]
fn a_met_goal_is_the_usual_no_op_where_the_process_list_is_incomplete() {
    const WOULD: &str = "a reclaim here would be refused: ";
    let visible = host(&yaml(true, ""));
    let (expected, lines) = ensure_logged(&visible, &cargo("1G", None));
    assert_eq!(expected.status, Status::Met, "{expected:#?}");
    assert!(!lines.iter().any(|l| l.contains(WOULD)), "{lines:#?}");
    let expected_ops: Vec<Op> = visible.trace().iter().map(|c| c.op).collect();
    let unreadable = "events:\n  - {at: {op: pid_namespace, nth: 1, when: before}, fail: EIO}\n";
    let mut cases: Vec<(&str, &str)> = INCOMPLETE_PROCESS_LISTS.to_vec();
    cases.push((unreadable, "diskgc could not read its PID namespace facts"));
    for (facts, rule) in cases {
        let hidden = host(&format!("{facts}{}", yaml(true, "")));
        let (report, lines) = ensure_logged(&hidden, &cargo("1G", None));
        assert_eq!(report.status, Status::Met, "{facts}: {report:#?}");
        assert_eq!(report.exit_code(), 0, "{facts}");
        assert!(report.errors.is_empty(), "{facts}: {report:#?}");
        assert_eq!(report.avail_after_bytes, report.avail_before_bytes);
        let warned: Vec<&String> = lines.iter().filter(|l| l.contains(WOULD)).collect();
        assert!(
            warned.len() == 1
                && warned[0].starts_with("warning: the space is there, so this run exits 0")
                && warned[0].contains(rule),
            "{facts}: {lines:#?}"
        );
        let ops: Vec<Op> = hidden.trace().iter().map(|c| c.op).collect();
        assert_eq!(ops, expected_ops, "{facts}: the met run differs");
        assert!(ops.contains(&Op::Statvfs), "{facts}: sampled");
        let read: Vec<Op> = ops
            .into_iter()
            .filter(|op| *op != Op::PidNamespace && reads_processes_or_locks(*op))
            .collect();
        assert!(read.is_empty(), "{facts}: {read:?}");
        assert_eq!(hidden.trace_count(Op::PidNamespace), 1, "{facts}");
    }
}

/// Codex blocker (TASK-298 review): the rule is asked right after the
/// first sample, before the deadline decides anything, so a deadline that
/// passes during the sample cannot turn a permanent refusal into "retry
/// later" (124): below the goal, exit 2 (or 3 for facts it cannot read);
/// met at the first sample, exit 0. A deadline that passes before the
/// sample (here, in the sandbox gate) ends the run before the rule is
/// asked: 124, as documented (docs/reference.md, Exit codes).
#[test]
fn a_deadline_during_the_first_sample_does_not_hide_the_refusal() {
    let late = |op: &str| {
        format!("events:\n  - {{at: {{op: {op}, nth: 1, when: after}}, advance: 11m}}\n")
    };
    let unreadable = "  - {at: {op: pid_namespace, nth: 1, when: before}, fail: EIO}\n";
    let mut cases: Vec<(String, &str, ErrorCause)> = INCOMPLETE_PROCESS_LISTS
        .iter()
        .map(|(facts, rule)| ((*facts).to_owned(), *rule, ErrorCause::Safety))
        .collect();
    cases.push((
        String::new(),
        "diskgc could not read its PID namespace facts",
        ErrorCause::Host,
    ));
    for (facts, rule, cause) in cases {
        let events = if cause == ErrorCause::Host {
            format!("{}{unreadable}", late("statvfs"))
        } else {
            late("statvfs")
        };
        let model = format!("{facts}{events}{}", yaml(true, ""));
        let below = host(&model);
        let report = ensure(&below, &cargo("12G", None));
        let code = if cause == ErrorCause::Host { 3 } else { 2 };
        assert_eq!(report.exit_code(), code, "{facts}{events}: {report:#?}");
        assert_eq!(report.errors.len(), 1, "{report:#?}");
        assert_eq!(report.errors[0].cause, cause);
        assert!(report.errors[0].message.contains(rule), "{report:#?}");
        assert!(calls(&below, Op::Flock).is_empty() && calls(&below, Op::Pids).is_empty());
        assert!(below.unfired_events().is_empty());
        let met = host(&model);
        let report = ensure(&met, &cargo("1G", None));
        assert_eq!(report.exit_code(), 0, "{facts}{events}: {report:#?}");
        assert!(report.errors.is_empty(), "{report:#?}");
    }
    for (facts, _) in INCOMPLETE_PROCESS_LISTS {
        let host = host(&format!("{facts}{}{}", late("may_delete"), yaml(true, "")));
        let report = ensure(&host, &cargo("12G", None));
        assert_eq!(report.status, Status::Deadline, "{facts}: {report:#?}");
        assert_eq!(report.exit_code(), 124);
        assert_eq!(host.trace_count(Op::PidNamespace), 0, "{facts}: not asked");
    }
}

/// Below the goal the reclaim would start, and is refused there (exit 2,
/// `cause: safety`, naming the rule), after the first sample (the report
/// says what was available) and before the tally, the lock and any
/// candidate: nothing listed, locked or deleted.
#[test]
fn below_the_goal_an_incomplete_process_list_refuses_the_reclaim() {
    for (facts, rule) in INCOMPLETE_PROCESS_LISTS {
        let host = host(&format!("{facts}{}", yaml(true, "")));
        let report = ensure(&host, &cargo("12G", None));
        assert_eq!(report.status, Status::UsageError, "{facts}: {report:#?}");
        assert_eq!(report.exit_code(), 2, "{facts}");
        assert_eq!(report.errors.len(), 1, "{facts}: {report:#?}");
        assert_eq!(report.errors[0].cause, ErrorCause::Safety, "{facts}");
        assert!(
            report.errors[0].message.contains(rule)
                && report.errors[0]
                    .message
                    .contains("run diskgc where it sees every process"),
            "{facts}: {}",
            report.errors[0].message
        );
        assert!(
            report
                .avail_before_bytes
                .is_some_and(|avail| avail < 12 * GIB),
            "{facts}: sampled below the goal: {report:#?}"
        );
        assert!(report.actions.is_empty() && report.skipped.is_empty());
        let trace = host.trace();
        let sampled = trace.iter().position(|c| c.op == Op::Statvfs);
        let asked = trace.iter().position(|c| c.op == Op::PidNamespace);
        assert!(
            sampled.is_some() && asked.is_some() && sampled < asked,
            "{facts}: the sample comes first: {trace:#?}"
        );
        for op in [
            Op::Pids,
            Op::Flock,
            Op::UserLock,
            Op::Unlinkat,
            Op::StateRead,
        ] {
            assert!(calls(&host, op).is_empty(), "{facts}: {op} called");
        }
        assert!(
            calls(&host, Op::ReadDir).is_empty(),
            "{facts}: a root was listed (the tally): {trace:#?}"
        );
    }
}

/// Below the goal, facts that cannot be read are a failure of the host
/// (exit 3, `cause: host`), not a refusal; at the goal they are never
/// read.
#[test]
fn below_the_goal_unreadable_namespace_facts_are_a_host_error() {
    let fail = "events:\n  - {at: {op: pid_namespace, nth: 1, when: before}, fail: EIO}\n";
    let host = host(&format!("{fail}{}", yaml(true, "")));
    let report = ensure(&host, &cargo("12G", None));
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.errors.len(), 1, "{report:#?}");
    assert_eq!(report.errors[0].cause, ErrorCause::Host);
    assert!(
        report.errors[0]
            .message
            .contains("could not read its PID namespace facts"),
        "{}",
        report.errors[0].message
    );
    assert!(calls(&host, Op::Flock).is_empty() && calls(&host, Op::Pids).is_empty());
    assert!(host.unfired_events().is_empty());
}

/// AC #2: root below `--min-free` with no `/run/diskgc/lock`: exit 2, the
/// message carries both tmpfiles lines, nothing is deleted or locked. (A
/// user falls back to a per-user lock instead, TASK-277: user_lock.rs.)
#[test]
fn a_missing_host_lock_is_exit_2_with_the_tmpfiles_lines() {
    let host = host(&format!("own_uid: 0\n{}", yaml(false, "")));
    assert_eq!(executor::privileges(&host, true), Ok(0));
    let report = executor::ensure(&host, &cargo("12G", None), &[1000], &mut |_| {});
    assert_eq!(report.status, Status::UsageError);
    assert_eq!(report.errors[0].cause, ErrorCause::Provisioning);
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

/// TASK-187: when the process scan that would list open deleted files
/// fails (here diskgc out of descriptors on its first pidfd), what they
/// hold is unknown: `held_by_deleted_files` is `null` in the JSON, live and
/// dry run alike, never `[]` (which says that nothing is held). No
/// candidate reaches the in-use step (both too recent), so the end-of-run
/// scan is the only one.
#[test]
fn a_failed_scan_reports_held_by_deleted_files_as_null() {
    let extra = "  /home/mark/big.log: {size: 1G}\n  /usr/bin/tail: {}\nprocesses:\n  - pid: 4242\n    \
                 exe: /usr/bin/tail\n    cwd: /home/mark\n    maps: [/usr/bin/tail]\n    environ: \
                 [PATH=/usr/bin]\n    fds: [{path: /home/mark/big.log, deleted: true}]\nevents:\n  - \
                 {at: {op: proc_identity, nth: 1, when: before}, fail: EMFILE}\n";
    let request = request(
        "profile:cargo-target && filter:idle-30d",
        &[SRC],
        SRC,
        "12G",
        None,
    );
    for (mode, run) in [
        ("live", ensure as fn(&ModelHost, &Request) -> Report),
        ("dry run", dry_run),
    ] {
        let host = host(&yaml(true, extra));
        let report = run(&host, &request);
        assert!(host.unfired_events().is_empty(), "{mode}");
        assert_eq!(report.held_by_deleted_files, None, "{mode}: {report:#?}");
        let wire: serde_json::Value = serde_json::from_str(&report.to_json()).expect("JSON");
        assert!(
            wire["held_by_deleted_files"].is_null(),
            "{mode}: {}",
            wire["held_by_deleted_files"]
        );
    }
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
    let skipped: Vec<(&Path, Reason)> = report
        .skipped
        .iter()
        .map(|s| (s.candidate.as_deref().expect("a candidate"), s.reason))
        .collect();
    assert_eq!(
        skipped,
        [
            (Path::new(OLD), Reason::NotIdle),
            (Path::new(NEW), Reason::NotIdle)
        ]
    );
    assert!(calls(&host, Op::Unlinkat).is_empty());
    let held = report.held_by_deleted_files.as_deref().expect("a scan ran");
    assert_eq!(held.len(), 1, "{report:#?}");
    assert_eq!(held[0].pid, 4242);
    assert_eq!(held[0].bytes, GIB);
    assert!(
        report
            .summary()
            .expect("prints")
            .starts_with("nothing more could be reclaimed: ")
    );
}

/// A completed tally has already measured both targets as younger than the
/// tier's idle bound. A held Cargo lock on the newer target would produce
/// `locked` if reclaim tried it; the trace proves neither target's lock was
/// attempted. The host lock itself is still taken for the ensure run.
#[test]
fn tallied_nonidle_cargo_targets_skip_before_any_target_flock() {
    let holder = "  /usr/bin/x: {}\nprocesses:\n  - {pid: 4242, exe: /usr/bin/x, \
                  cwd: /, maps: [/usr/bin/x], environ: [PATH=/usr/bin]}\nlocks:\n  - \
                  {path: /home/mark/src/new/target/debug/.cargo-lock, holder: 4242, mode: exclusive}\n";
    let host = host(&yaml(true, holder));
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
    assert_eq!(report.skipped.len(), 2, "{report:#?}");
    for path in [OLD, NEW] {
        let skip = report
            .skipped
            .iter()
            .find(|entry| entry.candidate.as_deref() == Some(Path::new(path)))
            .expect("both tallied targets skipped");
        assert_eq!(skip.reason, Reason::NotIdle, "{path}: {report:#?}");
        assert!(skip.estimated_bytes.is_some(), "{path}: {report:#?}");
        assert!(
            calls(&host, Op::Flock)
                .iter()
                .all(|call| !touches(call, path)),
            "{path}: target lock attempted: {:#?}",
            host.trace()
        );
    }
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

// ---- One filesystem identity (TASK-286) ----

/// One btrfs (superblock device 0:40, 100G with 80G used by others): its
/// top level at `/pool` and two subvolumes, `/home` (`@home`) and `/work`
/// (`@work`), each with its own `statx` device (0:41, 0:42) while
/// mountinfo names 0:40 for all three, as Linux shows btrfs subvolumes;
/// `/data` is another filesystem (ext4). `extra` is appended to the
/// mounts; `tree` holds the tree's entries.
fn btrfs_yaml(extra: &str, tree: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
  - {{id: 40, at: /pool, fs: btrfs, dev: "0:40", size: 100G, other_used: 80G, parent: 1, subvolume: "0:45"}}
  - {{id: 41, at: /home, fs: btrfs, dev: "0:40", size: 100G, other_used: 80G, parent: 1, bind_of: 40, bind_root: /@home, subvolume: "0:41"}}
  - {{id: 42, at: /work, fs: btrfs, dev: "0:40", size: 100G, other_used: 80G, parent: 1, bind_of: 40, bind_root: /@work, subvolume: "0:42"}}
  - {{id: 50, at: /data, fs: ext4, dev: "254:3", size: 100G, parent: 1}}
{extra}tree:
  /run/diskgc/lock: {{uid: 0, gid: 0}}
  /home/mark: {{.: {{mode: "0755"}}}}
{tree}"#
    )
}

/// Two attached Cargo targets below `dir`: `old` (8G) and `new` (4G).
fn targets_below(dir: &str) -> String {
    format!(
        "  {dir}/old/Cargo.toml: {{}}\n  {dir}/old/target:\n    CACHEDIR.TAG: {{cachedir_tag: true}}\n    debug: {old}\n  {dir}/new/Cargo.toml: {{}}\n  {dir}/new/target:\n    CACHEDIR.TAG: {{cachedir_tag: true}}\n    debug: {new}\n",
        old = profile("8G", "2026-10-01T00:00:00Z"),
        new = profile("4G", "2026-10-14T00:00:00Z"),
    )
}

/// TASK-286: a root on a sibling btrfs subvolume of `PATH`'s is on
/// `PATH`'s filesystem (one superblock, one pool of space), though its
/// `statx` device differs: it is searched, not skipped `other_fs`, and the
/// run reclaims there to the goal (it used to exit 3 with the space
/// reclaimable). A root on another filesystem still is `other_fs`.
#[test]
fn a_root_on_a_sibling_btrfs_subvolume_is_on_path_s_filesystem() {
    let sibling = host(&btrfs_yaml("", &targets_below("/work/src")));
    let report = ensure(
        &sibling,
        &request(
            "profile:cargo-target",
            &["/work/src"],
            "/home/mark",
            "12G",
            None,
        ),
    );
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(report.exit_code(), 0);
    assert!(
        report.skipped.iter().all(|s| s.reason != Reason::OtherFs),
        "{report:#?}"
    );
    let acted: Vec<&Path> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(acted, [Path::new("/work/src/old/target")], "{report:#?}");

    let other = host(&btrfs_yaml("", &targets_below("/data/src")));
    let report = ensure(
        &other,
        &request(
            "profile:cargo-target",
            &["/data/src"],
            "/home/mark",
            "30G",
            None,
        ),
    );
    assert_eq!(report.status, Status::Exhausted, "{report:#?}");
    assert_eq!(report.exit_code(), 1);
    let skip = report
        .skipped
        .iter()
        .find(|s| s.candidate.as_deref() == Some(Path::new("/data/src")))
        .expect("the root on another filesystem is skipped");
    assert_eq!(skip.reason, Reason::OtherFs, "{report:#?}");
    assert!(calls(&other, Op::Unlinkat).is_empty());
}

/// TASK-286: `held_by_deleted_files` places a held file by the mount it
/// was opened through: one on a sibling subvolume of `PATH`'s btrfs is
/// listed (its `statx` device is that subvolume's own); one on another
/// btrfs filesystem is not, though mountinfo shows the same type.
#[test]
fn held_by_deleted_files_counts_every_subvolume_of_path_s_btrfs_only() {
    let scenario = btrfs_yaml(
        "  - {id: 60, at: /other, fs: btrfs, dev: \"0:60\", size: 100G, parent: 1, subvolume: \"0:61\"}\n",
        "  /usr/bin/tail: {}\n  /work/gone: {size: 1G}\n  /other/gone: {size: 2G}\nprocesses:\n  - pid: 4242\n    exe: /usr/bin/tail\n    cwd: /home/mark\n    maps: [/usr/bin/tail]\n    environ: [PATH=/usr/bin]\n    fds: [{path: /work/gone, deleted: true}, {path: /other/gone, deleted: true}]\n",
    );
    let host = host(&scenario);
    let report = ensure(
        &host,
        &request(
            "profile:cargo-target",
            &["/home/mark"],
            "/home/mark",
            "50G",
            None,
        ),
    );
    let held = report.held_by_deleted_files.as_deref().expect("a scan ran");
    assert_eq!(held.len(), 1, "{report:#?}");
    assert_eq!(held[0].pid, 4242);
    assert_eq!(held[0].bytes, GIB, "only the file on PATH's btrfs");
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

/// `ensure --min-free MIN PATH` (`/home/mark`, on the btrfs of
/// [`btrfs_yaml`]) with `policy`'s groups.
fn btrfs_policy(host: &ModelHost, policy: &str, min: &str) -> Report {
    let (tiers, timeouts) = configured(policy);
    let mut request = request(
        "profile:cargo-target",
        &["/home/mark"],
        "/home/mark",
        min,
        None,
    );
    request.tier = tiers[0].clone();
    let mut lines = Vec::new();
    let report =
        executor::ensure_grouped(host, &request, &tiers, &timeouts, &[1000], &mut |line| {
            lines.push(line.to_owned())
        });
    eprintln!("log:\n  {}", lines.join("\n  "));
    report
}

/// A `delete` profile of the directories `c-*` below `root`, in one tier.
fn cache_dirs(root: &str) -> String {
    format!(
        "version: 1\nprofiles:\n  - name: cache-dirs\n    find: {{roots: [{root}], paths: [\"c-*\"], type: dir}}\n    action: delete\n    examples: {{match: [{root}/c-a]}}\ngroups:\n  - name: g\n    tiers:\n      - name: t\n        select: profile:cache-dirs\n"
    )
}

/// The skip of `candidate` in `report`, which must be `contains_mount` for
/// a nested subvolume at `at`.
fn nested_subvolume_skip(report: &Report, candidate: &str, at: &str) {
    let skip = report
        .skipped
        .iter()
        .find(|s| s.candidate.as_deref() == Some(Path::new(candidate)))
        .unwrap_or_else(|| panic!("{candidate} is not skipped: {report:#?}"));
    assert_eq!(skip.reason, Reason::ContainsMount, "{report:#?}");
    let detail = skip.detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("nested subvolume") && detail.contains(at),
        "{detail}"
    );
}

/// TASK-286 review (both reviewers' blocker): a btrfs subvolume or
/// snapshot nested (not mounted) inside a candidate shares its mount id,
/// so a walk by mount id alone would empty it and `rmdir` its top (Linux
/// 4.18 deletes an empty subvolume, unprivileged). A directory on another
/// device than the candidate's own is refused: `contains_mount`, nothing
/// deleted.
#[test]
fn a_nested_subvolume_inside_a_candidate_refuses_it() {
    let old = "{mtime: 2026-10-01T00:00:00Z}";
    let tree = format!(
        "  /work/cache:\n    c-a:\n      .: {old}\n      data: {{size: 4G, mtime: 2026-10-01T00:00:00Z}}\n      snap:\n        .: {{mtime: 2026-10-01T00:00:00Z, subvolume: \"0:50\"}}\n        f: {{size: 4G, mtime: 2026-10-01T00:00:00Z}}\n"
    );
    let host = host(&btrfs_yaml("", &tree));
    let report = btrfs_policy(&host, &cache_dirs("/work/cache"), "30G");
    nested_subvolume_skip(&report, "/work/cache/c-a", "/work/cache/c-a/snap");
    assert!(
        0 == unlinked_below(&host, "/work/cache/c-a"),
        "{:#?}",
        host.trace()
    );
}

/// TASK-286 review: the top of a nested subvolume matched as a candidate
/// is refused at discovery (and by the walk), never emptied and removed.
#[test]
fn a_candidate_that_is_a_nested_subvolume_top_is_refused() {
    let tree = "  /work/cache:\n    c-a:\n      .: {mtime: 2026-10-01T00:00:00Z, subvolume: \"0:50\"}\n      f: {size: 4G, mtime: 2026-10-01T00:00:00Z}\n";
    let host = host(&btrfs_yaml("", tree));
    let report = btrfs_policy(&host, &cache_dirs("/work/cache"), "30G");
    nested_subvolume_skip(&report, "/work/cache/c-a", "/work/cache/c-a");
    assert!(
        0 == unlinked_below(&host, "/work/cache/c-a"),
        "{:#?}",
        host.trace()
    );
}

/// TASK-286 review: a candidate inside a nested subvolume (its root there)
/// is on `PATH`'s filesystem and is reclaimed like any other; only the
/// subvolume's top is out of reach.
#[test]
fn a_candidate_inside_a_nested_subvolume_is_reclaimed() {
    let tree = "  /work/sub:\n    .: {subvolume: \"0:51\"}\n    cache:\n      c-b:\n        .: {mtime: 2026-10-01T00:00:00Z}\n        f: {size: 4G, mtime: 2026-10-01T00:00:00Z}\n";
    let host = host(&btrfs_yaml("", tree));
    let report = btrfs_policy(&host, &cache_dirs("/work/sub/cache"), "18G");
    assert_eq!(report.status, Status::Met, "{report:#?}");
    let acted: Vec<&Path> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(acted, [Path::new("/work/sub/cache/c-b")], "{report:#?}");
}

/// TASK-286 review: a nested subvolume inside a Cargo profile directory's
/// entry is refused with the target, as for a filesystem candidate.
#[test]
fn a_nested_subvolume_inside_a_cargo_profile_entry_refuses_the_target() {
    let at = "2026-10-01T00:00:00Z";
    let profile = format!(
        "{{.: {{mtime: {at}}}, .cargo-lock: {{mtime: {at}}}, .fingerprint: {{.: {{mtime: {at}}}, p-1: {{.: {{mtime: {at}}}, bin-p: {{mtime: {at}}}}}}}, deps: {{.: {{mtime: {at}}}, p-1: {{size: 4G, mtime: {at}}}, snap: {{.: {{mtime: {at}, subvolume: \"0:50\"}}, f: {{size: 4G, mtime: {at}}}}}}}}}"
    );
    let tree = format!(
        "  /work/src/old/Cargo.toml: {{}}\n  /work/src/old/target:\n    CACHEDIR.TAG: {{cachedir_tag: true}}\n    debug: {profile}\n"
    );
    let host = host(&btrfs_yaml("", &tree));
    let report = ensure(
        &host,
        &request(
            "profile:cargo-target",
            &["/work/src"],
            "/home/mark",
            "30G",
            None,
        ),
    );
    let skip = report
        .skipped
        .iter()
        .find(|s| s.candidate.as_deref() == Some(Path::new("/work/src/old/target")))
        .unwrap_or_else(|| panic!("the target is not skipped: {report:#?}"));
    assert_eq!(skip.reason, Reason::ContainsMount, "{report:#?}");
    assert!(
        skip.detail
            .as_deref()
            .unwrap_or_default()
            .contains("nested subvolume"),
        "{skip:?}"
    );
    assert!(
        0 == unlinked_below(&host, "/work/src/old/target/debug/deps/snap"),
        "{:#?}",
        host.trace()
    );
}

/// A bcachefs at `/work` (TASK-307): its subvolumes keep the
/// filesystem's device and differ only by `stx_subvol` (the `subvol:`
/// directory attribute). `extra` is added to the mount's line (as
/// `statx_missing: [subvol]`, a kernel before Linux 6.10).
fn bcachefs_yaml(extra: &str, tree: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
  - {{id: 40, at: /work, fs: bcachefs, dev: "254:2", size: 100G, other_used: 80G, parent: 1{extra}}}
tree:
  /run/diskgc/lock: {{uid: 0, gid: 0}}
{tree}"#
    )
}

/// `ensure --min-free MIN /work` with `policy`'s groups.
fn work_policy(host: &ModelHost, policy: &str, min: &str) -> Report {
    let (tiers, timeouts) = configured(policy);
    let mut request = request("profile:cargo-target", &["/work"], "/work", min, None);
    request.tier = tiers[0].clone();
    let mut lines = Vec::new();
    let report =
        executor::ensure_grouped(host, &request, &tiers, &timeouts, &[1000], &mut |line| {
            lines.push(line.to_owned())
        });
    eprintln!("log:\n  {}", lines.join("\n  "));
    report
}

/// The skip of `candidate`, which must have `reason` and say `says`.
fn skip_saying(report: &Report, candidate: &str, reason: Reason, says: &str) {
    let skip = report
        .skipped
        .iter()
        .find(|s| s.candidate.as_deref() == Some(Path::new(candidate)))
        .unwrap_or_else(|| panic!("{candidate} is not skipped: {report:#?}"));
    assert_eq!(skip.reason, reason, "{report:#?}");
    let detail = skip.detail.as_deref().unwrap_or_default();
    assert!(detail.contains(says), "{detail}");
}

/// TASK-307 (the vm-real-kernel check): a bcachefs subvolume nested inside
/// a candidate shows the candidate's own device, so the TASK-286 rule
/// (another device) missed it and the walk emptied it and removed its
/// top. Another `stx_subvol` is the boundary too: `contains_mount`,
/// nothing deleted.
#[test]
fn a_nested_bcachefs_subvolume_inside_a_candidate_refuses_it() {
    let old = "mtime: 2026-10-01T00:00:00Z";
    let tree = format!(
        "  /work/cache:\n    c-a:\n      .: {{{old}}}\n      data: {{size: 4G, {old}}}\n      inner:\n        .: {{{old}, subvol: 2}}\n        f: {{size: 4G, {old}}}\n"
    );
    let host = host(&bcachefs_yaml("", &tree));
    let report = work_policy(&host, &cache_dirs("/work/cache"), "30G");
    skip_saying(
        &report,
        "/work/cache/c-a",
        Reason::ContainsMount,
        "/work/cache/c-a/inner",
    );
    skip_saying(
        &report,
        "/work/cache/c-a",
        Reason::ContainsMount,
        "subvolume 2",
    );
    assert!(
        0 == unlinked_below(&host, "/work/cache/c-a"),
        "{:#?}",
        host.trace()
    );
}

/// TASK-307: the top of a nested bcachefs subvolume matched as a candidate
/// is refused (discovery and walk), never emptied and removed.
#[test]
fn a_candidate_that_is_a_bcachefs_subvolume_top_is_refused() {
    let tree = "  /work/cache:\n    c-a:\n      .: {mtime: 2026-10-01T00:00:00Z, subvol: 2}\n      f: {size: 4G, mtime: 2026-10-01T00:00:00Z}\n";
    let host = host(&bcachefs_yaml("", tree));
    let report = work_policy(&host, &cache_dirs("/work/cache"), "30G");
    skip_saying(
        &report,
        "/work/cache/c-a",
        Reason::ContainsMount,
        "subvolume 2",
    );
    assert!(
        0 == unlinked_below(&host, "/work/cache/c-a"),
        "{:#?}",
        host.trace()
    );
}

/// TASK-307: a candidate inside a nested bcachefs subvolume is reclaimed
/// like any other; only the subvolume's top is out of reach.
#[test]
fn a_candidate_inside_a_bcachefs_subvolume_is_reclaimed() {
    let tree = "  /work/sub:\n    .: {subvol: 3}\n    cache:\n      c-b:\n        .: {mtime: 2026-10-01T00:00:00Z}\n        f: {size: 4G, mtime: 2026-10-01T00:00:00Z}\n";
    let host = host(&bcachefs_yaml("", tree));
    let report = work_policy(&host, &cache_dirs("/work/sub/cache"), "18G");
    assert_eq!(report.status, Status::Met, "{report:#?}");
    let acted: Vec<&Path> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(acted, [Path::new("/work/sub/cache/c-b")], "{report:#?}");
}

/// TASK-307: where the kernel does not report `stx_subvol` (before Linux
/// 6.10), a bcachefs subvolume cannot be told from a directory at all:
/// every directory candidate there is refused (`unsupported_fs`, naming
/// `STATX_SUBVOL`), nothing deleted. Fail closed, never a guess.
#[test]
fn without_stx_subvol_a_bcachefs_directory_candidate_is_refused() {
    let tree = "  /work/cache:\n    c-a:\n      .: {mtime: 2026-10-01T00:00:00Z}\n      f: {size: 4G, mtime: 2026-10-01T00:00:00Z}\n";
    let host = host(&bcachefs_yaml(", statx_missing: [subvol]", tree));
    let report = work_policy(&host, &cache_dirs("/work/cache"), "30G");
    skip_saying(
        &report,
        "/work/cache/c-a",
        Reason::UnsupportedFs,
        "STATX_SUBVOL",
    );
    assert!(
        0 == unlinked_below(&host, "/work/cache/c-a"),
        "{:#?}",
        host.trace()
    );
}

/// TASK-307: btrfs subvolumes have a device each, so a btrfs without
/// `stx_subvol` (before Linux 6.10) keeps the TASK-286 rule alone: a
/// nested subvolume is refused by its device, a plain candidate reclaimed.
#[test]
fn without_stx_subvol_btrfs_keeps_the_device_rule() {
    let tree = "  /work/cache:\n    c-a:\n      .: {mtime: 2026-10-01T00:00:00Z}\n      f: {size: 4G, mtime: 2026-10-01T00:00:00Z}\n    c-b:\n      .: {mtime: 2026-10-01T00:00:00Z}\n      snap:\n        .: {mtime: 2026-10-01T00:00:00Z, subvolume: \"0:50\"}\n        f: {size: 4G, mtime: 2026-10-01T00:00:00Z}\n";
    let yaml = btrfs_yaml("", tree).replace(
        "fs: btrfs, dev: \"0:40\",",
        "fs: btrfs, statx_missing: [subvol], dev: \"0:40\",",
    );
    let host = host(&yaml);
    let report = btrfs_policy(&host, &cache_dirs("/work/cache"), "100%");
    nested_subvolume_skip(&report, "/work/cache/c-b", "/work/cache/c-b/snap");
    let acted: Vec<&Path> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(acted, [Path::new("/work/cache/c-a")], "{report:#?}");
}

/// TASK-307: a nested bcachefs subvolume inside a Cargo profile
/// directory's entry refuses the target, as on btrfs.
#[test]
fn a_nested_bcachefs_subvolume_inside_a_cargo_profile_entry_refuses_the_target() {
    let at = "2026-10-01T00:00:00Z";
    let profile = format!(
        "{{.: {{mtime: {at}}}, .cargo-lock: {{mtime: {at}}}, .fingerprint: {{.: {{mtime: {at}}}, p-1: {{.: {{mtime: {at}}}, bin-p: {{mtime: {at}}}}}}}, deps: {{.: {{mtime: {at}}}, p-1: {{size: 4G, mtime: {at}}}, snap: {{.: {{mtime: {at}, subvol: 2}}, f: {{size: 4G, mtime: {at}}}}}}}}}"
    );
    let tree = format!(
        "  /work/src/old/Cargo.toml: {{}}\n  /work/src/old/target:\n    CACHEDIR.TAG: {{cachedir_tag: true}}\n    debug: {profile}\n"
    );
    let host = host(&bcachefs_yaml("", &tree));
    let report = ensure(
        &host,
        &request("profile:cargo-target", &["/work/src"], "/work", "30G", None),
    );
    skip_saying(
        &report,
        "/work/src/old/target",
        Reason::ContainsMount,
        "subvolume 2",
    );
    assert!(
        0 == unlinked_below(&host, "/work/src/old/target/debug/deps/snap"),
        "{:#?}",
        host.trace()
    );
}

/// TASK-307: the top of a nested bcachefs subvolume is never a Cargo
/// target (discovery), as on btrfs.
#[test]
fn a_cargo_target_that_is_a_bcachefs_subvolume_top_is_refused() {
    let at = "2026-10-01T00:00:00Z";
    let tree = format!(
        "  /work/src/old/Cargo.toml: {{}}\n  /work/src/old/target:\n    .: {{subvol: 2}}\n    CACHEDIR.TAG: {{cachedir_tag: true}}\n    debug: {{.: {{mtime: {at}}}, .cargo-lock: {{mtime: {at}}}, deps: {{.: {{mtime: {at}}}, p-1: {{size: 4G, mtime: {at}}}}}}}\n"
    );
    let host = host(&bcachefs_yaml("", &tree));
    let report = ensure(
        &host,
        &request("profile:cargo-target", &["/work/src"], "/work", "30G", None),
    );
    skip_saying(
        &report,
        "/work/src/old/target",
        Reason::ContainsMount,
        "subvolume 2",
    );
    assert!(
        0 == unlinked_below(&host, "/work/src/old/target"),
        "{:#?}",
        host.trace()
    );
}

/// `ensure --min-free MIN PATH` with `policy`'s groups.
fn policy_at(host: &ModelHost, policy: &str, path: &str, min: &str) -> Report {
    let (tiers, timeouts) = configured(policy);
    let mut request = request("profile:cargo-target", &[path], path, min, None);
    request.tier = tiers[0].clone();
    let mut lines = Vec::new();
    let report =
        executor::ensure_grouped(host, &request, &tiers, &timeouts, &[1000], &mut |line| {
            lines.push(line.to_owned())
        });
    eprintln!("log:\n  {}", lines.join("\n  "));
    report
}

/// The btrfs of [`btrfs_yaml`] with an overlay at `/ov` whose upper
/// directory is `/work/upper` (on the `@work` subvolume); `shown` replaces
/// the `upperdir=` mountinfo shows when given.
fn overlay_on_btrfs(shown: Option<&str>, tree: &str) -> String {
    let shown = shown.map_or(String::new(), |s| format!(", overlay_upperdir_shown: {s}"));
    btrfs_yaml(
        &format!(
            "  - {{id: 60, at: /ov, fs: overlay, dev: \"0:60\", size: 1G, parent: 1, overlay_upper: /work/upper{shown}}}\n"
        ),
        tree,
    )
}

const OVERLAY_TREE: &str = "  /work/upper/cache:\n    c-a:\n      .: {mtime: 2026-10-01T00:00:00Z}\n      data: {size: 4G, mtime: 2026-10-01T00:00:00Z}\n      inner:\n        .: {mtime: 2026-10-01T00:00:00Z, subvolume: \"0:50\"}\n        f: {size: 4G, mtime: 2026-10-01T00:00:00Z}\n    c-b:\n      .: {mtime: 2026-10-01T00:00:00Z}\n      sub: {.: {mtime: 2026-10-01T00:00:00Z}, f: {size: 4G, mtime: 2026-10-01T00:00:00Z}}\n";

/// TASK-307 review (Codex blocker): a btrfs subvolume in an overlay's
/// upper layer, inside a candidate on the overlay. overlayfs shows every
/// directory under its own device, so the device rule cannot see it, and
/// an `rmdir` through the overlay removes the upper entry (`ovl_do_rmdir`
/// -> `vfs_rmdir` -> `btrfs_rmdir` deletes an empty subvolume). The
/// boundary is proved in the upper layer itself: `contains_mount`, nothing
/// deleted; the candidate beside it, plain directories in the same
/// subvolume (Docker's overlay2 on btrfs), is reclaimed.
#[test]
fn an_upper_layer_subvolume_under_an_overlay_candidate_refuses_it() {
    let host = host(&overlay_on_btrfs(None, OVERLAY_TREE));
    let report = policy_at(&host, &cache_dirs("/ov/cache"), "/ov", "100%");
    skip_saying(
        &report,
        "/ov/cache/c-a",
        Reason::ContainsMount,
        "/ov/cache/c-a/inner",
    );
    skip_saying(
        &report,
        "/ov/cache/c-a",
        Reason::ContainsMount,
        "upper layer",
    );
    assert!(
        0 == unlinked_below(&host, "/ov/cache/c-a"),
        "{:#?}",
        host.trace()
    );
    assert!(
        0 == unlinked_below(&host, "/work/upper/cache/c-a"),
        "{:#?}",
        host.trace()
    );
    let acted: Vec<&Path> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(acted, [Path::new("/ov/cache/c-b")], "{report:#?}");
}

/// TASK-307 review: where the upper layer cannot be reached by the name
/// mountinfo shows (here relative, as a mounter may give it), a directory
/// whose `stx_subvol` differs from its candidate's cannot be proved a
/// lower layer's: refused (fail closed). One that does not differ is
/// judged as usual.
#[test]
fn an_unreachable_upper_layer_refuses_only_a_differing_subvolume() {
    let host = host(&overlay_on_btrfs(Some("upper"), OVERLAY_TREE));
    let report = policy_at(&host, &cache_dirs("/ov/cache"), "/ov", "100%");
    skip_saying(
        &report,
        "/ov/cache/c-a",
        Reason::ContainsMount,
        "cannot be reached",
    );
    assert!(
        0 == unlinked_below(&host, "/ov/cache/c-a"),
        "{:#?}",
        host.trace()
    );
    let acted: Vec<&Path> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(acted, [Path::new("/ov/cache/c-b")], "{report:#?}");
}

/// TASK-307 review: the candidate itself the top of an upper-layer
/// subvolume (its `rmdir` through the overlay would delete it).
#[test]
fn an_overlay_candidate_that_is_an_upper_layer_subvolume_is_refused() {
    let tree = "  /work/upper/cache:\n    c-a:\n      .: {mtime: 2026-10-01T00:00:00Z, subvolume: \"0:50\"}\n      f: {size: 4G, mtime: 2026-10-01T00:00:00Z}\n";
    let host = host(&overlay_on_btrfs(None, tree));
    let report = policy_at(&host, &cache_dirs("/ov/cache"), "/ov", "100%");
    skip_saying(
        &report,
        "/ov/cache/c-a",
        Reason::ContainsMount,
        "upper layer",
    );
    assert!(
        0 == unlinked_below(&host, "/ov/cache/c-a"),
        "{:#?}",
        host.trace()
    );
}

/// A Cargo target at `/work/src/{at}`, with a 4G profile entry.
fn target_tree(at: &str, attrs: &str) -> String {
    let t = "2026-10-01T00:00:00Z";
    format!(
        "  /work/src/{at}:\n    .: {{{attrs}}}\n    CACHEDIR.TAG: {{cachedir_tag: true}}\n    debug: {{.: {{mtime: {t}}}, .cargo-lock: {{mtime: {t}}}, deps: {{.: {{mtime: {t}}}, p-1: {{size: 4G, mtime: {t}}}}}}}\n"
    )
}

/// TASK-307 review (Codex medium): a target directly below the discovery
/// root was judged against no parent (the root frame had no volume), so a
/// target that is itself a nested subvolume's top was taken there.
#[test]
fn a_cargo_target_directly_below_its_root_that_is_a_subvolume_top_is_refused() {
    let host = host(&btrfs_yaml(
        "",
        &target_tree("target", "subvolume: \"0:50\""),
    ));
    let report = ensure(
        &host,
        &request(
            "profile:cargo-target",
            &["/work/src"],
            "/home/mark",
            "30G",
            None,
        ),
    );
    skip_saying(
        &report,
        "/work/src/target",
        Reason::ContainsMount,
        "nested subvolume",
    );
    assert!(
        0 == unlinked_below(&host, "/work/src/target"),
        "{:#?}",
        host.trace()
    );
}

/// TASK-307 review (Codex medium): the discovery root itself a target that
/// is a nested subvolume's top is judged against its real parent.
#[test]
fn a_cargo_target_that_is_its_own_root_and_a_subvolume_top_is_refused() {
    let host = host(&bcachefs_yaml("", &target_tree("old/target", "subvol: 2")));
    let report = ensure(
        &host,
        &request(
            "profile:cargo-target",
            &["/work/src/old/target"],
            "/work",
            "30G",
            None,
        ),
    );
    skip_saying(
        &report,
        "/work/src/old/target",
        Reason::ContainsMount,
        "subvolume 2",
    );
    assert!(
        0 == unlinked_below(&host, "/work/src/old/target"),
        "{:#?}",
        host.trace()
    );
}

/// The controls: the same targets in the root's own subvolume reclaim.
#[test]
fn cargo_targets_at_or_directly_below_their_root_in_its_subvolume_reclaim() {
    let below = host(&btrfs_yaml("", &target_tree("target", "")));
    let report = ensure(
        &below,
        &request(
            "profile:cargo-target",
            &["/work/src"],
            "/home/mark",
            "30G",
            None,
        ),
    );
    let acted: Vec<&Path> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(acted, [Path::new("/work/src/target")], "{report:#?}");
    let own = host(&bcachefs_yaml("", &target_tree("old/target", "")));
    let report = ensure(
        &own,
        &request(
            "profile:cargo-target",
            &["/work/src/old/target"],
            "/work",
            "30G",
            None,
        ),
    );
    let acted: Vec<&Path> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(acted, [Path::new("/work/src/old/target")], "{report:#?}");
}

/// TASK-286 review (Codex 2): on bcachefs the superblock device does not
/// identify the filesystem alone, so "the same filesystem" also needs its
/// id (`f_fsid`) read from both held descriptors; read and equal, the run
/// reclaims as on any filesystem.
#[test]
fn bcachefs_is_the_same_filesystem_by_its_id_read_from_both_sides() {
    let host =
        host(&yaml(true, "").replace("fs: ext4, dev: \"254:2\"", "fs: bcachefs, dev: \"254:2\""));
    let report = ensure(&host, &cargo("12G", None));
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert!(
        report.skipped.iter().all(|s| s.reason != Reason::OtherFs),
        "{report:#?}"
    );
    assert!(!calls(&host, Op::FsMagic).is_empty(), "its id was read");
}

/// TASK-286 review (mped S3; the PRD's non-goal "warned about"): a run
/// that reclaims on btrfs says once that a deletion may free nothing while
/// a snapshot shares the extents; on ext4 it does not.
#[test]
fn a_reclaim_on_btrfs_warns_that_snapshots_may_keep_the_space() {
    let lines_of = |host: &ModelHost, path: &str, root: &str| {
        let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
        let mut lines = Vec::new();
        executor::ensure(
            host,
            &request("profile:cargo-target", &[root], path, "12G", None),
            &[uid],
            &mut |l| lines.push(l.to_owned()),
        );
        lines
    };
    let btrfs = host(&btrfs_yaml("", &targets_below("/work/src")));
    let lines = lines_of(&btrfs, "/home/mark", "/work/src");
    let warned = |lines: &[String]| {
        lines
            .iter()
            .filter(|l| l.contains("frees nothing while a snapshot shares its extents"))
            .count()
    };
    assert_eq!(warned(&lines), 1, "{lines:#?}");
    let ext4 = host(&yaml(true, ""));
    assert_eq!(warned(&lines_of(&ext4, SRC, SRC)), 0);
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
    assert_eq!(report.errors[0].candidate.as_deref(), Some(OLD));
    assert_eq!(report.errors[0].cause, ErrorCause::Operation);
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

/// Every reported action has one progress line, including a candidate
/// whose deletion stopped after its first successful unlink. Error and
/// process-scan lines are not action lines.
#[test]
fn each_complete_or_partial_action_has_one_log_line_and_one_report_row() {
    for event in [
        "",
        "events:\n  - {at: {op: unlinkat, nth: 2, when: before}, fail: EACCES}\n",
    ] {
        let host = host(&yaml(true, event));
        let mut lines = Vec::new();
        let report = executor::ensure(&host, &cargo("19G", None), &[1000], &mut |line| {
            lines.push(line.to_owned());
        });
        let action_lines: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .filter(|line| {
                line.contains(": reclaimed ")
                    || line.contains(": deleted part-way:")
                    || line.contains(": command finished with ")
            })
            .collect();
        assert_eq!(report.actions.len(), 2, "{report:#?}; {lines:#?}");
        assert_eq!(action_lines.len(), report.actions.len(), "{lines:#?}");
        for (line, action) in action_lines.iter().zip(&report.actions) {
            assert!(
                line.contains(&format!("cargo-target {}:", action.candidate.display())),
                "action and log disagree: {action:?}; {line}"
            );
        }
        let json: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
        assert_eq!(
            json["actions"].as_array().unwrap().len(),
            action_lines.len(),
            "JSON and log disagree: {json}; {lines:#?}"
        );
        if !event.is_empty() {
            assert!(
                action_lines
                    .iter()
                    .any(|line| line.contains("deleted part-way:")),
                "the injected failure did not exercise a partial action: {lines:#?}"
            );
        }
    }
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

/// `tests/models/hostlock/<case>.yaml` run by `ensure --min-free 30G
/// --select profile:cargo-target --roots SRC SRC --timeout 2s`, its `expect`
/// and fired events checked by the harness.
fn host_lock_case(case: &str) -> (ModelHost, Outcome) {
    let model = load_fixture(&fixture_path(env!("CARGO_MANIFEST_DIR"), "hostlock", case))
        .unwrap_or_else(|e| panic!("{e}"));
    let mut outcome = None;
    let host = harness::run(&model, |host| {
        let mut req = cargo("30G", None);
        req.timeout = Some(Duration::from_secs(2));
        let report = ensure(host, &req);
        let got = Outcome {
            exit: i32::from(report.exit_code()),
            skipped: Vec::new(),
        };
        outcome = Some(got.clone());
        got
    });
    (host, outcome.expect("the scenario ran"))
}

fn host_lock_acquired(host: &ModelHost) -> bool {
    host.trace()
        .iter()
        .any(|c| c.op == Op::Flock && touches(c, HOST_LOCK) && c.result == "acquired")
}

/// task-64 #37: a host lock is free only when the last reference to its
/// open file goes, not when its holder exits. A child sharing it (an
/// inherited lock fd, or the kernel-side reference of a `/proc` reader)
/// keeps it held: `ensure` waits to its deadline (75), reaching no target.
#[test]
fn a_host_lock_stays_held_while_another_reference_to_its_file_remains() {
    let (host, _) = host_lock_case("held-by-a-remaining-reference");
    assert!(
        !host_lock_acquired(&host),
        "the lock was taken while 4243 held it"
    );
    assert!(
        host.trace().iter().all(|c| !touches(c, OLD)),
        "no target was reached"
    );
    assert_eq!(
        host.locks_on(Path::new(HOST_LOCK)),
        vec![(
            Holder::Process(4243),
            diskgc_core::host::LockMode::Exclusive
        )]
    );
}

/// The same, with the last reference closed at 1.5 s: `ensure` keeps
/// polling past the holder's exit at 1 s and takes the lock then.
#[test]
fn a_host_lock_is_taken_once_its_last_reference_closes() {
    let (host, _) = host_lock_case("released-by-the-last-reference");
    assert!(host_lock_acquired(&host), "the lock was never taken");
    assert!(
        host.elapsed() >= Duration::from_millis(1500),
        "ensure ended at {:?}, before the last reference closed",
        host.elapsed()
    );
    assert!(
        calls(&host, Op::Unlinkat).iter().any(|c| touches(c, OLD)),
        "the target was reclaimed after the lock was taken"
    );
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

#[test]
fn no_timeout_waits_past_the_default_ten_minutes_for_the_host_lock() {
    let holder = "  /usr/bin/diskgc: {}\n  /home/mark/junk: {size: 5G}\nprocesses:\n  - pid: 4242\n    exe: /usr/bin/diskgc\n    cwd: /\n    maps: [/usr/bin/diskgc]\n    environ: [PATH=/usr/bin]\nlocks:\n  - {path: /run/diskgc/lock, holder: 4242, mode: exclusive}\nevents:\n  - {at: {elapsed: 11m}, remove: /home/mark/junk}\n  - {at: {elapsed: 11m}, exit: 4242}\n";
    let host = common::host(&yaml(true, holder));
    let mut req = cargo("6G", None);
    req.timeout = None;
    let report = ensure(&host, &req);
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(report.exit_code(), 0);
    assert!(host.elapsed() >= Duration::from_secs(11 * 60));
    assert!(calls(&host, Op::Unlinkat).is_empty());
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

/// A dry run whose final sample fails reports it unknown and fails the run.
#[test]
fn a_dry_run_with_an_unmeasured_final_sample_reports_it_unknown() {
    let host = host(&yaml(false, &failing_statvfs(2)));
    let report = dry_run(&host, &cargo("1G", None));
    assert_eq!(report.avail_after_bytes, None, "{report:#?}");
    assert_eq!(report.status, Status::Error);
    assert_eq!(report.exit_code(), 3);
    assert!(report.avail_before_bytes.is_some());
    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].cause, ErrorCause::Host);
}

/// The first sample is a host operation; its I/O failure is exit 3 even
/// though nothing was measured or done.
#[test]
fn a_failed_first_sample_is_a_host_error() {
    let host = host(&yaml(true, &failing_statvfs(1)));
    let report = ensure(&host, &cargo("12G", None));
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.errors[0].cause, ErrorCause::Host);
    assert_eq!(report.avail_before_bytes, None);
    assert!(calls(&host, Op::Flock).is_empty());
}

#[test]
fn a_host_error_resolving_path_is_exit_3_but_missing_path_is_usage() {
    let event =
        "events:\n  - {at: {op: openat2, path: /home/mark/src, nth: 1, when: before}, fail: EIO}\n";
    for planning in [false, true] {
        let host = host(&yaml(!planning, event));
        let request = cargo("12G", None);
        let report = if planning {
            dry_run(&host, &request)
        } else {
            ensure(&host, &request)
        };
        assert_eq!(report.status, Status::Error, "{planning}: {report:#?}");
        assert_eq!(report.exit_code(), 3);
        assert_eq!(report.errors[0].cause, ErrorCause::Host);
        assert!(report.errors[0].message.contains("EIO"));
        assert!(calls(&host, Op::Unlinkat).is_empty());
    }

    let host = host(&yaml(false, ""));
    let mut request = cargo("12G", None);
    request.path = PathBuf::from("/home/mark/missing");
    let missing = dry_run(&host, &request);
    assert_eq!(missing.status, Status::UsageError, "{missing:#?}");
    assert_eq!(missing.exit_code(), 2);
}

#[test]
fn a_host_error_during_root_preflight_is_not_a_safety_refusal() {
    let event =
        "events:\n  - {at: {op: openat2, path: /home/mark/src, nth: 2, when: before}, fail: EIO}\n";
    let host = host(&yaml(true, event));
    let report = ensure(&host, &cargo("12G", None));
    assert!(host.unfired_events().is_empty(), "{:#?}", host.trace());
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.cause == ErrorCause::Host && error.message.contains("EIO")),
        "{report:#?}"
    );
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

#[test]
fn discovery_host_failure_in_dry_run_has_host_cause() {
    let event = "events:\n  - {at: {op: read_dir, path: /home/mark/src, nth: 1, when: before}, fail: EIO}\n";
    let host = host(&yaml(false, event));
    let report = dry_run(&host, &cargo("1G", None));
    assert!(host.unfired_events().is_empty(), "{:#?}", host.trace());
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.cause == ErrorCause::Host && error.message.contains("EIO")),
        "{report:#?}"
    );
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

#[test]
fn tally_host_failure_in_reclaim_has_host_cause() {
    let event = "events:\n  - {at: {op: read_dir, path: /home/mark/src/old/target, nth: 1, when: before}, fail: EIO}\n";
    let host = host(&yaml(true, event));
    let report = ensure(&host, &cargo("12G", None));
    assert!(host.unfired_events().is_empty(), "{:#?}", host.trace());
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.cause == ErrorCause::Host && error.message.contains("EIO")),
        "{report:#?}"
    );
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

#[test]
fn a_failed_first_sample_in_dry_run_and_grouped_paths_is_a_host_error() {
    let policy = "version: 1\ngroups:\n  - name: g\n    tiers:\n      - name: first\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n      - name: later\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n";
    let (tiers, timeouts) = configured(policy);
    for grouped in [false, true] {
        let host = host(&yaml(false, &failing_statvfs(1)));
        let mut request = cargo("12G", None);
        request.tier = tiers[0].clone();
        let report = if grouped {
            executor::dry_run_grouped(&host, &request, &tiers, &timeouts, &[1000], &mut |_| {})
        } else {
            dry_run(&host, &request)
        };
        assert_eq!(report.status, Status::Error, "{grouped}: {report:#?}");
        assert_eq!(report.exit_code(), 3);
        assert_eq!(report.errors[0].cause, ErrorCause::Host);
        assert!(report.plan.is_empty());
    }
    let host = host(&yaml(true, &failing_statvfs(1)));
    let report = grouped(&host, policy, "12G", None);
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.errors[0].cause, ErrorCause::Host);
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

#[test]
fn grouped_dry_run_reports_later_tier_preflight_failure_with_a_partial_plan() {
    let policy = "version: 1\ngroups:\n  - name: g\n    tiers:\n      - name: first\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n      - name: unsafe\n        select: profile:cargo-target\n        roots: [/tmp/systemd-private-abc-cups.service-x1/tmp]\n";
    let (tiers, timeouts) = configured(policy);
    let host = host(&yaml(false, ""));
    let mut request = cargo("12G", None);
    request.tier = tiers[0].clone();
    let report =
        executor::dry_run_grouped(&host, &request, &tiers, &timeouts, &[1000], &mut |_| {});
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert!(report.plan.iter().any(|entry| entry.tier == "first"));
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.tier.as_deref() == Some("unsafe")
                && error.cause == ErrorCause::Safety
                && error.message.contains("PrivateTmp"))
    );
    assert!(calls(&host, Op::Unlinkat).is_empty());
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
    assert!(report.tally_cut_short);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&report.to_json()).unwrap()["tally_cut_short"],
        true
    );
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
    assert!(!report.tally_cut_short);
    let order: Vec<&Path> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(order, [Path::new(OLD), Path::new(NEW)]);
}

/// A best-fit tier must re-pick after the first deletion's space sample.
/// The starting 2G of free space needs 9G: no unit covers that, so it
/// takes 8G. The remaining need is then 1G, so 4G precedes 6G.
#[test]
fn best_fit_cargo_recalculates_after_each_observed_sample() {
    let model = three("")
        .replace(
            &profile("2G", "2026-10-14T00:00:00Z"),
            &profile("8G", "2026-10-14T00:00:00Z"),
        )
        .replace(
            &profile("2G", "2026-10-10T00:00:00Z"),
            &profile("6G", "2026-10-10T00:00:00Z"),
        )
        .replace(
            &profile("2G", "2026-10-01T00:00:00Z"),
            &profile("4G", "2026-10-01T00:00:00Z"),
        );
    let policy = "version: 1\ngroups:\n  - name: pressure\n    tiers:\n      - name: fit\n        select: profile:cargo-target\n        roots: [/home/mark/src]\n        order: best-fit\n";
    let host = host(&model);
    let report = grouped(&host, policy, "11G", None);
    assert_eq!(report.status, Status::Met, "{report:#?}");
    let actions: Vec<_> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(
        actions,
        [
            Path::new("/home/mark/src/a/target"),
            Path::new("/home/mark/src/c/target")
        ]
    );
    assert!(
        host.stat_path(Path::new("/home/mark/src/b/target"))
            .is_some()
    );

    let (tiers, _) = configured(policy);
    let host = common::host(&model);
    let mut request = cargo("11G", None);
    request.tier = tiers[0].clone();
    let plan = dry_run(&host, &request);
    let paths: Vec<_> = plan
        .plan
        .iter()
        .map(|p| p.candidate.as_deref().expect("a candidate"))
        .collect();
    assert_eq!(
        paths,
        [
            Path::new("/home/mark/src/a/target"),
            Path::new("/home/mark/src/c/target"),
            Path::new("/home/mark/src/b/target")
        ]
    );
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

#[test]
fn best_fit_fs_chooses_smallest_covering_unit_then_recalculates() {
    let model = r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}
  - {id: 2, at: /home, fs: ext4, dev: "254:2", size: 100G, other_used: 80G, parent: 1}
  - {id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}
tree:
  /run/diskgc/lock: {uid: 0, gid: 0}
  /home/mark/src/cache-a: {data: {size: 8G}}
  /home/mark/src/cache-b: {data: {size: 6G}}
  /home/mark/src/cache-c: {data: {size: 4G}}
"#;
    let policy = "version: 1\nprofiles:\n  - name: test-cache\n    find: {roots: [/home/mark/src], paths: [cache-*], type: dir}\n    action: delete\n    examples: {match: [/home/mark/src/cache-a]}\ngroups:\n  - name: pressure\n    tiers:\n      - name: fit\n        select: profile:test-cache\n        order: best-fit\n";
    let host = host(model);
    let report = grouped(&host, policy, "11G", None);
    assert_eq!(report.status, Status::Met, "{report:#?}");
    let actions: Vec<_> = report
        .actions
        .iter()
        .map(|a| a.candidate.as_path())
        .collect();
    assert_eq!(
        actions,
        [
            Path::new("/home/mark/src/cache-a"),
            Path::new("/home/mark/src/cache-c")
        ]
    );
    assert!(
        host.stat_path(Path::new("/home/mark/src/cache-b"))
            .is_some()
    );
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
        assert_eq!(report.errors[0].cause, ErrorCause::Safety);
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
fn skip_reason(report: &Report, candidate: &str) -> Option<Reason> {
    report
        .skipped
        .iter()
        .find(|s| s.candidate.as_deref() == Some(Path::new(candidate)))
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
    assert_eq!(skip_reason(&report, OLD), Some(Reason::KeepMarker));
    assert_eq!(
        report
            .skipped
            .iter()
            .find(|entry| entry.candidate.as_deref() == Some(Path::new(OLD)))
            .and_then(|entry| entry.estimated_bytes),
        None,
        "a tally refused before completion cannot report a size"
    );
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
            Some(Reason::KeepMarker),
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
    assert!(report.tally_cut_short);
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
    assert!(!report.tally_cut_short);
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
    assert_eq!(error.cause, ErrorCause::Safety);
    assert!(error.message.contains("refuses to run as root"), "{error}");
    assert_eq!(executor::privileges(&root, true), Ok(0));
    let user = host(&yaml(true, ""));
    assert_eq!(executor::privileges(&user, false), Ok(1000));
    // Without --allow-root the capability set is not read at all.
    assert_eq!(user.trace_count(Op::ProcStatus), 0);
    assert!(calls(&root, Op::Unlinkat).is_empty());
}

/// TASK-44 AC #4 and its review: `--allow-root` is for root (euid 0, which
/// is how the in-use scan decides root mode), and needs `CAP_SYS_PTRACE` in
/// the effective set, read once from diskgc's own `/proc` status. Every
/// other capability does not replace it, it alone is enough; a non-root
/// binary is refused even holding it (file capability `cap_sys_ptrace=ep`,
/// Codex B1); a set that was read and lacks it is `safety`, one that could
/// not be read is `host` (exit 3). (The entry points, ensure in both modes
/// and watch in both, are the vectors `allow-root-*.yaml` and watch.rs.)
#[test]
fn allow_root_needs_root_and_cap_sys_ptrace_in_the_effective_set() {
    let ptrace = 1u64 << executor::CAP_SYS_PTRACE;
    assert_eq!(ptrace, 0x80000, "CAP_SYS_PTRACE is capability 19");
    let all = diskgc_model::state::ROOT_CAPABILITIES;
    let caps = |uid: u32, caps: Option<u64>| {
        let caps = caps.map_or(String::new(), |c| format!("own_cap_effective: {c}\n"));
        host(&format!("own_uid: {uid}\n{caps}{}", yaml(true, "")))
    };
    // Root's default (every capability) and CAP_SYS_PTRACE alone pass.
    for host in [caps(0, None), caps(0, Some(ptrace))] {
        assert_eq!(executor::privileges(&host, true), Ok(0));
        assert_eq!(host.trace_count(Op::ProcStatus), 1, "read once");
    }
    // Root with everything but it, or nothing: safety.
    for set in [!ptrace & all, 0] {
        let error = executor::privileges(&caps(0, Some(set)), true).expect_err("refused");
        assert_eq!(error.cause, ErrorCause::Safety);
        assert!(
            error.message.contains("--allow-root needs CAP_SYS_PTRACE"),
            "{error}"
        );
        assert!(
            error.message.contains(&format!("CapEff is {set:#018x}")),
            "{error}"
        );
    }
    // Not root, with or without the capability: refused before it is read.
    for set in [None, Some(ptrace), Some(all)] {
        let user = caps(1000, set);
        let error = executor::privileges(&user, true).expect_err("not root");
        assert_eq!(error.cause, ErrorCause::Safety);
        assert!(
            error.message.contains("--allow-root is for root; drop it"),
            "{error}"
        );
        assert_eq!(user.trace_count(Op::ProcStatus), 0);
    }
    // A set that cannot be read: a failure of the host, not a refusal.
    let unreadable = host(&format!(
        "own_uid: 0\n{}events:\n  - {{at: {{op: proc_status, nth: 1, when: before}}, fail: EIO}}\n",
        yaml(true, "")
    ));
    let error = executor::privileges(&unreadable, true).expect_err("unreadable");
    assert_eq!(error.cause, ErrorCause::Host);
    assert!(
        error
            .message
            .contains("could not read its own capability set"),
        "{error}"
    );
    assert!(error.message.contains("EIO"), "{error}");
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
/// kind (no flock at all), no gate question, nothing unlinked, and no lock
/// made: `/run/diskgc` (the model has none) is only looked up, to say what
/// a reclaim would lock (TASK-277), and no per-user lock is made.
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
    let plan: Vec<(&Path, Option<Reason>)> = report
        .plan
        .iter()
        .map(|p| (p.candidate.as_deref().expect("a candidate"), p.reason))
        .collect();
    assert_eq!(
        plan,
        [
            (Path::new(OLD), None),
            (Path::new(NEW), Some(Reason::NotIdle))
        ]
    );
    assert!(report.plan[0].estimated_bytes.expect("eligible: sized") >= 8 * GIB);
    // Refused after its walks: sized as it was measured (TASK-324).
    assert!(report.plan[1].estimated_bytes.is_some_and(|b| b > 0));
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
    assert!(host_lock_only_looked_up(&host));
    assert!(
        calls(&host, Op::UserLock).is_empty(),
        "a per-user lock made"
    );
    assert!(report.actions.is_empty() && report.skipped.is_empty());
    let text = report.summary().expect("a dry run always prints");
    assert!(
        text.contains(&format!("eligible, not needed {OLD}: est. "))
            && text.contains(&format!(
                "kept {NEW}: not_idle, last build 1 d ago; idle_for 7d required"
            )),
        "{text}"
    );

    // Without a filter, both eligible, oldest first.
    let host = common::host(&yaml(false, ""));
    let report = dry_run(&host, &cargo("1G", None));
    let order: Vec<&Path> = report
        .plan
        .iter()
        .map(|p| p.candidate.as_deref().expect("a candidate"))
        .collect();
    assert_eq!(order, [Path::new(OLD), Path::new(NEW)]);
    assert!(calls(&host, Op::Unlinkat).is_empty());
}

#[test]
fn a_spent_tally_budget_does_not_truncate_the_dry_run_plan() {
    let mut request = request(
        "profile:cargo-target && filter:idle-7d",
        &[SRC],
        SRC,
        "1G",
        None,
    );
    let ordinary = dry_run(&host(&yaml(false, "")), &request);
    request.tally_budget = Some(Duration::ZERO);
    let spent = dry_run(&host(&yaml(false, "")), &request);
    assert_eq!(ordinary.status, Status::DryRun);
    assert_eq!(spent.status, Status::DryRun, "{spent:#?}");
    assert!(!ordinary.tally_cut_short && !spent.tally_cut_short);
    assert_eq!(
        spent.plan, ordinary.plan,
        "a budget only limits reclaim tallying"
    );
    assert_eq!(spent.plan.len(), 2, "every candidate remains judged");
}

/// A root on another filesystem than PATH's is skipped in preflight:
/// deleting below it would free nothing on PATH, while same-device roots
/// remain eligible.
#[test]
fn a_foreign_root_is_skipped_while_same_device_roots_reclaim() {
    let tmp = "  /tmp/w/q/Cargo.toml: {}\n  /tmp/w/q/target:\n    CACHEDIR.TAG: {cachedir_tag: \
               true}\n    debug: {.cargo-lock: {}, deps: {q-1: {size: 100M}}}\n";
    let host = host(&yaml(true, tmp));
    let report = ensure(
        &host,
        &request("profile:cargo-target", &[SRC, "/tmp/w"], SRC, "16G", None),
    );
    assert!(report.errors.is_empty(), "{report:#?}");
    assert!(
        report.skipped.iter().any(
            |skip| skip.candidate.as_deref() == Some(Path::new("/tmp/w"))
                && skip.reason == Reason::OtherFs
        ),
        "{report:#?}"
    );
    assert_eq!(report.actions.len(), 2, "{report:#?}");
    assert!(
        report
            .actions
            .iter()
            .all(|action| action.candidate.starts_with(SRC)),
        "{report:#?}"
    );
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

#[test]
fn progress_distinguishes_available_space_from_a_close_minimum() {
    let host = host(&yaml(true, ""));
    let available = dry_run(&host, &cargo("1G", None))
        .avail_before_bytes
        .expect("model measures space");
    let minimum = available + (1 << 20);
    assert_eq!(format_bytes(available), format_bytes(minimum));
    let mut request = cargo("1G", None);
    request.min_free = Size::Bytes(minimum);
    let (_, lines) = ensure_logged(&host, &request);
    let progress = lines
        .iter()
        .find(|line| line.contains("below --min-free"))
        .expect("run logs why it takes the host lock");
    assert!(
        progress.contains(&format!(
            "{} ({available} B) available",
            format_bytes(available)
        )),
        "{progress}"
    );
    assert!(
        progress.contains(&format!(
            "--min-free {} ({minimum} B)",
            format_bytes(minimum)
        )),
        "{progress}"
    );
}

/// What the grammar allows but this build cannot run yet is refused when
/// the tier is built (exit 2), never approximated.
#[test]
fn selections_this_build_cannot_run_are_refused() {
    let library = Library::builtin().unwrap_or_else(|e| panic!("{e}"));
    let roots = vec![SRC.to_owned()];
    for (expr, needle) in [
        (
            "(profile:cargo-target && !filter:idle-1d) || profile:cargo-target",
            "a union",
        ),
        ("profile:nope", "no profile or filter is named `nope`"),
    ] {
        let error = Tier::select(expr, &library, Some(&roots), 1000).expect_err(expr);
        assert!(error.contains(needle), "{expr}: {error}");
    }
    let error = Tier::select("profile:cargo-target", &library, None, 1000).expect_err("no roots");
    assert!(error.contains("roots: required"), "{error}");
    let excluded = Tier::select(
        "profile:cargo-target && !profile:cargo-target",
        &library,
        Some(&roots),
        1000,
    )
    .expect("a bounded negated Cargo source is supported");
    assert_eq!(excluded.negated.len(), 1);
    assert_eq!(excluded.negated[0].profile, "cargo-target");
    assert_eq!(excluded.negated[0].roots, [PathBuf::from(SRC)]);
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
    assert_eq!(tier.name, "select");
}

#[test]
fn same_cargo_source_positive_idle_union_uses_weakest_bound_once() {
    let first =
        "(profile:cargo-target && filter:idle-7d) || (profile:cargo-target && filter:idle-2d)";
    let reverse =
        "(profile:cargo-target && filter:idle-2d) || (profile:cargo-target && filter:idle-7d)";
    for expr in [first, reverse] {
        let request = request(expr, &[SRC], SRC, "12G", Some("15G"));
        assert_eq!(
            request.tier.idle_for.unwrap().0,
            Duration::from_secs(2 * 86_400)
        );
        assert_eq!(request.tier.select, expr);
        let planning = host(&yaml(false, ""));
        let plan = dry_run(&planning, &request);
        assert_eq!(plan.status, Status::DryRun, "{plan:#?}");
        assert_eq!(plan.plan.len(), 2, "{plan:#?}");
        assert_eq!(plan.plan[0].candidate.as_deref(), Some(Path::new(OLD)));
        assert_eq!(plan.plan[0].reason, None);
        assert_eq!(plan.plan[1].candidate.as_deref(), Some(Path::new(NEW)));
        assert_eq!(plan.plan[1].reason, Some(Reason::NotIdle));
        assert!(calls(&planning, Op::Unlinkat).is_empty());

        let reclaiming = host(&yaml(true, ""));
        let report = ensure(&reclaiming, &request);
        assert_eq!(report.status, Status::Met, "{report:#?}");
        assert_eq!(report.actions.len(), 1, "{report:#?}");
        assert_eq!(report.actions[0].candidate, Path::new(OLD));
        assert!(!calls(&reclaiming, Op::Unlinkat).is_empty());
        assert!(calls(&reclaiming, Op::Unlinkat).iter().all(|call| {
            call.path
                .as_deref()
                .is_some_and(|path| path.starts_with(OLD))
        }));
    }

    let unfiltered = request(
        "(profile:cargo-target && filter:idle-7d) || profile:cargo-target",
        &[SRC],
        SRC,
        "1G",
        None,
    );
    assert_eq!(unfiltered.tier.idle_for, None);
    let plan = dry_run(&host(&yaml(false, "")), &unfiltered);
    assert_eq!(plan.plan.len(), 2, "{plan:#?}");
    assert!(
        plan.plan.iter().all(|entry| entry.reason.is_none()),
        "{plan:#?}"
    );

    let (configured_tiers, _) = configured(
        "version: 1\ngroups:\n  - name: pressure\n    tiers:\n      - name: cargo-union\n        select: (profile:cargo-target && filter:idle-7d) || (profile:cargo-target && filter:idle-2d)\n        roots: [/home/mark/src]\n",
    );
    assert_eq!(configured_tiers.len(), 1);
    assert_eq!(
        configured_tiers[0].idle_for.unwrap().0,
        Duration::from_secs(2 * 86_400)
    );
}

#[test]
fn unfiltered_cargo_union_member_keeps_its_source_minimum_age() {
    let specs = parse_profiles(
        "- {name: floored-targets, kind: cargo-target, find: {roots: required}, min_idle_for: 3d}\n- {name: other-targets, kind: cargo-target, find: {roots: required}}",
    )
    .unwrap();
    let library = Library::with_user_profiles(&specs).unwrap();
    let roots = [SRC.to_owned()];
    for expr in [
        "(profile:floored-targets && filter:idle-7d) || profile:floored-targets",
        "profile:floored-targets || (profile:floored-targets && filter:idle-7d)",
    ] {
        let tier = Tier::select(expr, &library, Some(&roots), 1000).unwrap();
        assert_eq!(tier.idle_for.unwrap().0, Duration::from_secs(3 * 86_400));
        let request = Request {
            tier,
            ..cargo("12G", Some("15G"))
        };
        let planning = host(&yaml(false, ""));
        let plan = dry_run(&planning, &request);
        assert_eq!(plan.status, Status::DryRun, "{plan:#?}");
        assert_eq!(plan.plan.len(), 2, "{plan:#?}");
        assert_eq!(plan.plan[0].candidate.as_deref(), Some(Path::new(OLD)));
        assert_eq!(plan.plan[0].reason, None);
        assert_eq!(plan.plan[1].candidate.as_deref(), Some(Path::new(NEW)));
        assert_eq!(plan.plan[1].reason, Some(Reason::NotIdle));
        assert!(calls(&planning, Op::Unlinkat).is_empty());

        let reclaiming = host(&yaml(true, ""));
        let report = ensure(&reclaiming, &request);
        assert_eq!(report.status, Status::Met, "{report:#?}");
        assert_eq!(report.actions.len(), 1, "{report:#?}");
        assert_eq!(report.actions[0].candidate, Path::new(OLD));
        assert!(!calls(&reclaiming, Op::Unlinkat).is_empty());
        assert!(calls(&reclaiming, Op::Unlinkat).iter().all(|call| {
            call.path
                .as_deref()
                .is_some_and(|path| path.starts_with(OLD))
        }));
    }

    let error = Tier::select(
        "profile:floored-targets || profile:other-targets",
        &library,
        Some(&roots),
        1000,
    )
    .expect_err("different Cargo sources need distinct tiers");
    assert!(error.contains("a union of this kind or action"), "{error}");
}

#[test]
fn negated_idle_filter_reclaims_only_recent_cargo_target() {
    let host = host(&yaml(true, ""));
    let request = request(
        "profile:cargo-target && filter:idle-1d && !filter:idle-2d",
        &[SRC],
        SRC,
        "11G",
        None,
    );
    let plan = dry_run(&host, &request);
    assert!(
        plan.plan
            .iter()
            .any(|e| e.candidate.as_deref() == Some(Path::new(NEW)) && e.reason.is_none()),
        "{plan:#?}"
    );
    assert!(
        plan.plan
            .iter()
            .any(|e| e.candidate.as_deref() == Some(Path::new(OLD))
                && e.reason == Some(Reason::TooIdle)),
        "{plan:#?}"
    );
    let report = ensure(&host, &request);
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert!(
        report.actions.iter().any(|e| e.candidate == Path::new(NEW)),
        "{report:#?}"
    );
    assert!(
        !report.actions.iter().any(|e| e.candidate == Path::new(OLD)),
        "{report:#?}"
    );
}

/// The target can age past a negated filter after planning but before its
/// Cargo locks are acquired. The locked reclaim must judge that age again.
#[test]
fn negated_idle_filter_rechecks_cargo_target_after_lock_time_passes() {
    let model = yaml(
        true,
        "events:\n  - {at: {op: flock, path: /home/mark/src/new/target/debug/.cargo-lock, nth: 1, when: before}, advance: 2m}\n",
    )
    .replacen(
        "clock: 2026-10-15T03:00:00Z",
        "clock: 2026-10-15T23:59:00Z",
        1,
    );
    let host = host(&model);
    let request = request(
        "profile:cargo-target && !filter:idle-2d",
        &[SRC],
        SRC,
        "11G",
        None,
    );
    let report = ensure(&host, &request);
    assert!(
        report
            .skipped
            .iter()
            .any(|e| e.candidate.as_deref() == Some(Path::new(NEW)) && e.reason == Reason::TooIdle),
        "{report:#?}"
    );
    let late_skip = report
        .skipped
        .iter()
        .find(|entry| entry.candidate.as_deref() == Some(Path::new(NEW)))
        .expect("late refusal after a completed tally");
    assert!(
        late_skip
            .estimated_bytes
            .expect("completed prior tally is reported")
            >= 4 * GIB,
        "{late_skip:#?}"
    );
    assert!(report.actions.is_empty(), "{report:#?}");
    assert!(calls(&host, Op::Unlinkat).is_empty());
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
fn plan_reason(report: &Report, candidate: &str) -> Option<Option<Reason>> {
    report
        .plan
        .iter()
        .find(|p| p.candidate.as_deref() == Some(Path::new(candidate)))
        .map(|p| p.reason)
}

/// task-80: the built-in `cargo-target` says `checkouts: allow` (the code
/// has no Cargo exemption): a target holding a vendored checkout, and one
/// found inside a checkout's metadata, are eligible. The documented built-in
/// default is not repeated in every run's log. A user profile of the same kind without the
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
    assert!(allow_lines(&lines).is_empty(), "{lines:#?}");

    let host = common::host(&fixture);
    let (report, lines) = dry_run_logged(&host, &my_targets("1G"));
    assert_eq!(
        plan_reason(&report, OLD),
        Some(Some(Reason::ContainsCheckout)),
        "{report:#?}"
    );
    assert_eq!(
        plan_reason(&report, meta),
        Some(Some(Reason::ContainsCheckout)),
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
/// is reclaimed: the goal is not met. The built-in default does not add a
/// repeated checkout warning to either run log.
#[test]
fn a_cargo_target_tier_reclaims_checkouts_only_when_its_profile_allows() {
    let vendor = "/home/mark/src/old/target/debug/build/p-2/out/vendor/.git";
    let host = common::host(&yaml(true, VENDORED));
    let (report, lines) = ensure_logged(&host, &cargo("11G", None));
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert!(allow_lines(&lines).is_empty(), "{lines:#?}");
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
            Some(Reason::ContainsCheckout),
            "{budget:?}: {report:#?}"
        );
        assert_eq!(
            skip_reason(&report, meta),
            Some(Reason::ContainsCheckout),
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
            Some(Some(Reason::ContainsCheckout)),
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
                Some(Reason::ContainsCheckout),
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

/// A reclaimed Cargo target remains discoverable through its lock files.
/// Later live runs have no work to skip, whether they tally first or judge
/// directly under the Cargo locks. A plan can still show why it is ineligible.
#[test]
fn a_reclaimed_cargo_target_is_not_a_skip_on_later_ensures() {
    for budget in [None, Some(Duration::ZERO)] {
        let host = host(&yaml(true, ""));
        let mut req = cargo("12G", Some("19G"));
        req.tally_budget = budget;
        let first = ensure(&host, &req);
        assert_eq!(first.status, Status::Met, "{budget:?}: {first:#?}");
        assert_eq!(first.actions.len(), 2, "{budget:?}: {first:#?}");

        let req = cargo("100G", None);
        let plan = dry_run(&host, &req);
        for target in [OLD, NEW] {
            assert_eq!(
                plan_reason(&plan, target),
                Some(Some(Reason::Empty)),
                "{budget:?}: {plan:#?}"
            );
        }
        for _ in 0..2 {
            let uid = executor::privileges(&host, false).unwrap_or_else(|e| panic!("{e}"));
            let mut lines = Vec::new();
            let later = executor::ensure(&host, &req, &[uid], &mut |l| lines.push(l.to_owned()));
            assert_eq!(later.status, Status::Exhausted, "{budget:?}: {later:#?}");
            assert!(later.actions.is_empty(), "{budget:?}: {later:#?}");
            assert!(later.skipped.is_empty(), "{budget:?}: {later:#?}");
            assert!(later.errors.is_empty(), "{budget:?}: {later:#?}");
            assert!(later.summary().unwrap().contains("0 kept"));
            // TASK-213: the log's "2 target(s) below" adds up: each omitted
            // target has a line saying why it is not in the result.
            assert!(
                lines
                    .iter()
                    .any(|l| l.ends_with("2 target(s) below /home/mark/src")),
                "{lines:#?}"
            );
            for target in [OLD, NEW] {
                let want = format!(
                    "select: {target}: its profile directories hold nothing but lock files; \
                     nothing to reclaim, not listed in the result"
                );
                assert!(lines.contains(&want), "{want}: {lines:#?}");
            }
        }
    }
}

/// task-81: targets whose profile directories inside a checkout's
/// metadata hold no entries, each discovered below `/home/mark/src/x`:
/// one found inside `.git` with only an empty profile directory, and, for
/// each marker `<M>`, one whose empty profile directory is `target/<M>`
/// or `target/<M>/debug` next to a full `debug`. With the target's
/// expected reason under the built-in `checkouts: allow` (`None`:
/// eligible).
fn empty_checkout_layouts() -> Vec<(String, String, Option<Reason>)> {
    let at = "2026-10-02T00:00:00Z";
    let (full, empty) = (profile("1G", at), empty_profile(at));
    let tag = "CACHEDIR.TAG: {cachedir_tag: true}";
    let mut layouts = vec![(
        "/home/mark/src/x/.git/target".to_owned(),
        format!("  /home/mark/src/x/.git/target:\n    {tag}\n    debug: {empty}\n"),
        Some(Reason::Empty),
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
            Some(Some(Reason::ContainsCheckout)),
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
                Some(Reason::ContainsCheckout),
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

/// A `protect` entry for a `keep-me` file in any Cargo target's `deps/`
/// below `SRC`.
fn protect_keep_me_in_deps() -> Vec<executor::ProtectedSource> {
    let config = config::parse(
        Path::new("/tmp/protect.yaml"),
        br#"version: 1
profiles:
  - name: keep-me
    find: {roots: [/home/mark/src], paths: ["*/target/debug/deps/keep-me"], type: file}
    action: delete
    examples: {match: [/home/mark/src/app/target/debug/deps/keep-me]}
protect: [keep-me]
"#,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    executor::bind_protections(&config, 1000, false).unwrap_or_else(|e| panic!("{e}"))
}

/// TASK-200, kind `cargo-target`: a protected match created in a locked
/// profile directory before its walk is a change the walk sees, so the
/// matches are enumerated again before the first part is deleted and the
/// target is kept; one created after the walk is never unlinked (its
/// directory, a part, cannot be removed).
#[test]
fn a_protected_match_made_in_a_cargo_target_is_seen_or_never_unlinked() {
    let lock = format!("{OLD}/debug/.cargo-lock");
    let keep = format!("{OLD}/debug/deps/keep-me");
    let before_walk = format!(
        "events:\n  - {{at: {{op: flock, path: {lock}, nth: 1, when: after}}, create: {keep}}}\n"
    );
    let host_before = host(&yaml(true, &before_walk));
    let mut request = cargo("11G", None);
    request.tier.protections = protect_keep_me_in_deps();
    let report = ensure(&host_before, &request);
    assert!(host_before.unfired_events().is_empty());
    let old = report
        .skipped
        .iter()
        .find(|e| e.candidate.as_deref() == Some(Path::new(OLD)))
        .unwrap_or_else(|| panic!("{report:#?}"));
    assert_eq!(old.reason, Reason::Protected, "{report:#?}");
    assert!(
        !calls(&host_before, Op::Unlinkat)
            .iter()
            .any(|c| c.path.as_deref().is_some_and(|p| p.starts_with(OLD))),
        "{report:#?}"
    );

    let after_walk =
        format!("events:\n  - {{at: {{op: own_pid, nth: 1, when: after}}, create: {keep}}}\n");
    let host_after = host(&yaml(true, &after_walk));
    let report = ensure(&host_after, &request);
    assert!(host_after.unfired_events().is_empty());
    let keep_alive = host_after
        .trace()
        .iter()
        .all(|c| !(c.op == Op::Unlinkat && c.path.as_deref() == Some(Path::new(&keep))));
    assert!(keep_alive, "{report:#?}");
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.message.contains("ENOTEMPTY")),
        "{report:#?}"
    );
}

/// TASK-269, kind `cargo-target`: a `protect` match gone right after the
/// tier enumerated it, before the targets' first overlap screen, stopped
/// the run (exit 3); the first screen enumerates again now, as every later
/// check does (TASK-200), and the old target is reclaimed.
#[test]
fn a_protected_match_removed_before_a_cargo_targets_first_screen_does_not_stop_the_run() {
    let keep = "/home/mark/src/new/target/debug/deps/keep-me";
    let extra = format!(
        "  {keep}: {{}}\nevents:\n  - {{at: {{op: statx, path: {keep}, nth: 1, when: after}}, \
         remove: {keep}}}\n"
    );
    let host = host(&yaml(true, &extra));
    let mut request = cargo("11G", None);
    request.tier.protections = protect_keep_me_in_deps();
    let report = ensure(&host, &request);
    assert!(host.unfired_events().is_empty());
    assert!(report.errors.is_empty(), "{report:#?}");
    assert!(
        report
            .actions
            .iter()
            .any(|action| action.candidate == Path::new(OLD)),
        "{report:#?}"
    );
}

/// TASK-269 review (Codex 5, mped S3), kind `cargo-target`: a `!profile:`
/// match (`keep-me` in the new target's `deps/`) removed right after the
/// tier enumerated it stopped the run (exit 3) at the first exclusion
/// check, in `ensure` and the dry run alike; now the check enumerates the
/// negated matches again, and the old target is judged on its own.
fn cargo_negated_removed() -> (ModelHost, Request) {
    let keep = "/home/mark/src/new/target/debug/deps/keep-me";
    let extra = format!(
        "  {keep}: {{}}\nevents:\n  - {{at: {{op: statx, path: {keep}, nth: 1, when: after}}, \
         remove: {keep}}}\n"
    );
    let config = config::parse(
        Path::new("/tmp/negate.yaml"),
        br#"version: 1
profiles:
  - name: keep-me
    find: {roots: [/home/mark/src], paths: ["*/target/debug/deps/keep-me"], type: file}
    action: delete
    examples: {match: [/home/mark/src/app/target/debug/deps/keep-me]}
"#,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let mut request = cargo("11G", None);
    request.tier = Tier::select(
        "profile:cargo-target && !profile:keep-me",
        &config.library,
        Some(&[SRC.to_owned()]),
        1000,
    )
    .unwrap_or_else(|e| panic!("{e}"));
    (host(&yaml(true, &extra)), request)
}

#[test]
fn a_negated_match_removed_before_a_cargo_targets_first_screen_does_not_stop_the_dry_run() {
    let (host, request) = cargo_negated_removed();
    let report = dry_run(&host, &request);
    assert!(host.unfired_events().is_empty());
    assert!(report.errors.is_empty(), "{report:#?}");
    assert_eq!(plan_reason(&report, OLD), Some(None), "{report:#?}");
    // Judged first, against the enumerated match it holds by name: an
    // overlap by name needs no proof, so the stale match still excludes it.
    assert_eq!(
        plan_reason(&report, NEW),
        Some(Some(Reason::Excluded)),
        "{report:#?}"
    );
}

#[test]
fn a_negated_match_removed_before_a_cargo_targets_first_screen_does_not_stop_the_run() {
    let (host, request) = cargo_negated_removed();
    let report = ensure(&host, &request);
    assert!(host.unfired_events().is_empty());
    assert!(report.errors.is_empty(), "{report:#?}");
    assert!(
        report
            .actions
            .iter()
            .any(|action| action.candidate == Path::new(OLD)),
        "{report:#?}"
    );
}

/// TASK-210/211/209 (the 0.1 Debian walk): a grouped run on /home whose
/// /tmp tiers have no root there. Each tier reports the foreign /tmp root
/// once (a union's two /tmp sources included), the human summary names
/// every such tier, the tiers log that they searched nothing, and the
/// process scan's unchanged notes are logged once, not once per tier.
#[test]
fn foreign_roots_are_one_row_per_tier_and_scan_notes_are_logged_once() {
    let policy = "version: 1\ngroups:\n  - name: stale\n    tiers:\n      - name: tmp-rc\n        select: profile:nix-shell-rc\n      - name: tmp-union\n        select: profile:pytest-tmp || profile:nix-shell-rc\n      - name: old\n        select: profile:cargo-target && filter:idle-30d\n        roots: [/home/mark/src]\n";
    let other_user = "processes:\n  - {pid: 77, exe: /usr/bin/x, maps: [/usr/bin/x], environ: [PATH=/usr/bin], \
                      fds: [], proc: denied, uid: 1001}\n";
    let (tiers, timeouts) = configured(policy);
    let mut request = cargo("50G", None);
    request.tier = tiers[0].clone();
    for live in [false, true] {
        let host = host(&yaml(true, &format!("  /usr/bin/x: {{}}\n{other_user}")));
        let mut lines = Vec::new();
        let mut log = |line: &str| lines.push(line.to_owned());
        let report = if live {
            executor::ensure_grouped(&host, &request, &tiers, &timeouts, &[1000], &mut log)
        } else {
            executor::dry_run_grouped(&host, &request, &tiers, &timeouts, &[1000], &mut log)
        };
        eprintln!("log:\n  {}", lines.join("\n  "));
        let foreign: Vec<(String, String, Option<String>)> = if live {
            report
                .skipped
                .iter()
                .filter(|skip| skip.reason == Reason::OtherFs)
                .map(|skip| {
                    (
                        skip.tier.clone(),
                        skip.candidate.as_deref().unwrap().display().to_string(),
                        skip.detail.clone(),
                    )
                })
                .collect()
        } else {
            report
                .plan
                .iter()
                .filter(|entry| entry.reason == Some(Reason::OtherFs))
                .map(|entry| {
                    (
                        entry.tier.clone(),
                        entry.candidate.as_deref().unwrap().display().to_string(),
                        entry.detail.clone(),
                    )
                })
                .collect()
        };
        let detail = Some(diskgc_core::report::OTHER_FS_ROOT_DETAIL.to_owned());
        assert_eq!(
            foreign,
            [
                ("tmp-rc".to_owned(), "/tmp".to_owned(), detail.clone()),
                ("tmp-union".to_owned(), "/tmp".to_owned(), detail),
            ],
            "live {live}: {report:#?}"
        );
        let summary = report.summary().expect("a summary");
        assert!(
            summary.contains(
                "\n  kept /tmp: other_fs, a root of the tier, on another filesystem than PATH's: \
                 not searched [tmp-rc, tmp-union]"
            ),
            "live {live}: {summary}"
        );
        for tier in [
            "tmp-rc: `profile:nix-shell-rc`",
            "tmp-union: `profile:pytest-tmp || profile:nix-shell-rc`",
        ] {
            let want = format!(
                "{tier}: nothing searched: no root of the tier is on PATH's filesystem (skipped \
                 as other_fs)"
            );
            assert!(lines.contains(&want), "live {live}: {want}: {lines:#?}");
        }
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("below  ") || line.ends_with("below ")),
            "live {live}: {lines:#?}"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("processes of other users not inspected"))
                .count(),
            1,
            "live {live}: {lines:#?}"
        );
        assert!(
            lines.iter().any(|line| line.starts_with("old: ")),
            "the last tier ran: {lines:#?}"
        );
    }
}
