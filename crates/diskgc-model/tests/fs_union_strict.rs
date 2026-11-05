//! TASK-337 (post-merge review): a union's plan is never less strict than
//! its reclaim. A toolchain-guarded source (`protect_toolchains: rustup`)
//! and a generic source find the same toolchain directory. The reclaim
//! selects the guarded term by its own evidence (place, walk), checks the
//! candidate against a fresh process scan, and then vetoes it by every
//! admitted term's toolchain guard (docs/reference.md, the filesystem
//! union paragraph; the guard: docs/safety.md, Toolchain pins), so a
//! toolchain a live shell selects is kept `pinned_toolchain` whichever
//! term found it. Two plan bugs let the generic term plan it eligible:
//!
//! - the plan's union veto ran only where a source had a liveness
//!   contract, so a toolchain guard alone vetoed nothing in a union;
//! - an in-use clearance made for one term (on its walk and its scan) was
//!   carried over to the next term, past that term's own newer scan.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::engine::Reason;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::report::Report;
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;

const PROFILES: &str = r#"
- name: toolchains
  find: {roots: required, paths: [.rustup/toolchains/*], type: dir}
  where: {protect_toolchains: rustup}
  action: delete
  examples: {root: /home/x, match: [/home/x/.rustup/toolchains/1.90.0-x86_64-unknown-linux-gnu]}
- name: generic
  find: {roots: required, paths: [.rustup/toolchains/*], type: dir}
  action: delete
  examples: {root: /home/x, match: [/home/x/.rustup/toolchains/1.90.0-x86_64-unknown-linux-gnu]}
"#;

const UNION: &str = "profile:toolchains || profile:generic";
/// The generic term first: the guarded term is judged after the union
/// selected `generic`.
const UNION_GENERIC_FIRST: &str = "profile:generic || profile:toolchains";

/// Selected by the shell's `RUSTUP_TOOLCHAIN`.
const PINNED: &str = "/home/tester/.rustup/toolchains/1.87.0-x86_64-unknown-linux-gnu";
/// Selected by nothing.
const FREE: &str = "/home/tester/.rustup/toolchains/1.89.0-x86_64-unknown-linux-gnu";

/// A rustup home with two toolchains; pid 901 is a shell whose initial
/// environment selects 1.87.0 (the toolchain guard's live half), pid 7 a
/// readable bystander. `events`: further `events:` entries.
fn model(events: &str) -> ModelHost {
    let events = if events.is_empty() {
        String::new()
    } else {
        format!("events:\n{events}")
    };
    host(&format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 10G}}
  - {{id: 2, at: /home, fs: ext4, dev: "254:2", size: 10G, parent: 1}}
tree:
  /run/diskgc/lock: {{uid: 0, gid: 0}}
  /usr/bin/tool: {{mode: "0755"}}
  /home/tester/.rustup/settings.toml: {{content: "version = \"12\"\n"}}
  /home/tester/.rustup/toolchains:
    1.87.0-x86_64-unknown-linux-gnu: {{.: {{mtime: 2026-09-01T00:00:00Z}}, bin: {{.: {{mtime: 2026-09-01T00:00:00Z}}, rustc: {{size: 8K, mode: "0755", mtime: 2026-09-01T00:00:00Z}}}}}}
    1.89.0-x86_64-unknown-linux-gnu: {{.: {{mtime: 2026-09-01T00:00:00Z}}, bin: {{.: {{mtime: 2026-09-01T00:00:00Z}}, rustc: {{size: 8K, mode: "0755", mtime: 2026-09-01T00:00:00Z}}}}}}
processes:
  - {{pid: 7, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], environ: [PATH=/usr/bin], fds: [{{path: /usr/bin/tool}}]}}
  - {{pid: 901, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], environ: [HOME=/home/tester, RUSTUP_TOOLCHAIN=1.87.0], fds: [{{path: /usr/bin/tool}}]}}
{events}"#
    ))
}

fn request(select: &str) -> Request {
    let specs = parse_profiles(PROFILES).unwrap_or_else(|e| panic!("{e}"));
    let library = Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"));
    let roots = ["/home/tester".to_owned()];
    Request {
        path: PathBuf::from("/home"),
        min_free: parse_size("100%").expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier: Tier::select(select, &library, Some(&roots), 1000).unwrap_or_else(|e| panic!("{e}")),
    }
}

/// The plan's verdict on `candidate`: its profile and reason.
fn planned(report: &Report, candidate: &str) -> (String, Option<Reason>) {
    let rows: Vec<_> = report
        .plan
        .iter()
        .filter(|row| row.candidate.as_deref() == Some(Path::new(candidate)))
        .collect();
    assert_eq!(rows.len(), 1, "{candidate}: {:#?}", report.plan);
    (rows[0].profile.clone(), rows[0].reason)
}

/// The plan is at least as strict as the reclaim: nothing the reclaim
/// keeps is planned eligible, and every candidate the reclaim deleted
/// was planned eligible.
fn plan_predicts_reclaim(plan: &Report, reclaim: &Report) {
    for skip in &reclaim.skipped {
        let Some(candidate) = skip.candidate.as_deref() else {
            continue;
        };
        let candidate = candidate.to_str().expect("utf-8");
        assert!(
            planned(plan, candidate).1.is_some(),
            "{candidate}: the reclaim kept it ({:?} under {}), the plan said eligible: {:#?}",
            skip.reason,
            skip.profile,
            plan.plan
        );
    }
    for action in &reclaim.actions {
        let candidate = action.candidate.to_str().expect("utf-8");
        assert_eq!(planned(plan, candidate).1, None, "{candidate}");
    }
}

fn dry_run(host: &ModelHost, select: &str) -> Report {
    let report = executor::dry_run(host, &request(select), &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    report
}

fn ensure(host: &ModelHost, select: &str) -> Report {
    let report = executor::ensure(host, &request(select), &[1000], &mut |_| {});
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    report
}

/// The single guarded source, for reference: the shell keeps 1.87.0.
#[test]
fn the_guarded_source_alone_keeps_the_toolchain_a_shell_selects() {
    let host = model("");
    let report = executor::dry_run(&host, &request("profile:toolchains"), &[1000], &mut |_| {});
    assert_eq!(
        planned(&report, PINNED),
        ("toolchains".to_owned(), Some(Reason::PinnedToolchain))
    );
    assert_eq!(planned(&report, FREE), ("toolchains".to_owned(), None));
}

/// The union, in both orders: the reclaim vetoes 1.87.0 by the guarded
/// term's toolchain guard; the plan must too, not plan it eligible under
/// `generic` because no source has a liveness contract. With `generic`
/// first, the guarded term is judged after the selection.
#[test]
fn a_union_plan_applies_a_toolchain_guard_of_another_term() {
    for (select, free_profile) in [(UNION, "toolchains"), (UNION_GENERIC_FIRST, "generic")] {
        let plan = dry_run(&model(""), select);
        assert_eq!(
            planned(&plan, PINNED),
            ("toolchains".to_owned(), Some(Reason::PinnedToolchain)),
            "`{select}`: {:#?}",
            plan.plan
        );
        assert_eq!(
            planned(&plan, FREE),
            (free_profile.to_owned(), None),
            "`{select}`"
        );

        let host = model("");
        let reclaim = ensure(&host, select);
        plan_predicts_reclaim(&plan, &reclaim);
        assert!(
            host.stat_path(Path::new(PINNED)).is_some(),
            "`{select}`: the reclaim kept 1.87.0: {reclaim:#?}"
        );
        assert!(
            host.stat_path(Path::new(FREE)).is_none(),
            "`{select}`: {reclaim:#?}"
        );
    }
}

/// The guarded term's scan clears 1.87.0 of use and its guard keeps it
/// (pid 901's `RUSTUP_TOOLCHAIN`); then the plan's scan expires (61 s
/// pass: the plan reuses a settled scan for 60 s) and pids 7 and 901
/// become unreadable, so the next scan can tell nothing of use
/// (`in_use_unknown`) and the toolchain guard's live half gets no
/// evidence from it. The `generic` term must be judged on that newer scan,
/// not on the guarded term's clearance: only its own in-use check keeps
/// 1.87.0 (the union veto on the newer scan would not). The events fire
/// once the guarded term's scan has settled (its second process listing;
/// candidates are judged in order, 1.87.0 first).
///
/// The profile is also the witness that the events fired between the two
/// terms (earlier, the guarded term itself would be `in_use_unknown`);
/// that the reclaim names `toolchains` instead is TASK-346's attribution.
#[test]
fn an_in_use_clearance_is_not_carried_to_a_later_term_past_a_newer_scan() {
    let events = "  - {at: {op: pids, nth: 2, when: after}, advance: 61s}\n  \
                  - {at: {op: pids, nth: 2, when: after}, access: {pid: 7, proc: failed}}\n  \
                  - {at: {op: pids, nth: 2, when: after}, access: {pid: 901, proc: failed}}\n";
    let host = model(events);
    let plan = dry_run(&host, UNION);
    assert!(host.unfired_events().is_empty(), "the events fired");
    assert_eq!(
        planned(&plan, PINNED),
        ("generic".to_owned(), Some(Reason::InUseUnknown)),
        "{:#?}",
        plan.plan
    );

    let host = model(events);
    let reclaim = ensure(&host, UNION);
    plan_predicts_reclaim(&plan, &reclaim);
    assert!(
        host.stat_path(Path::new(PINNED)).is_some(),
        "the reclaim kept 1.87.0: {reclaim:#?}"
    );
}
