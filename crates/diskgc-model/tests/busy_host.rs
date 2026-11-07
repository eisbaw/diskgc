//! A busy host through the real executor on ModelHost (TASK-340;
//! docs/safety.md, In use: Freshness): a tier whose in-use scans go stale
//! for a while (a fork storm, a load spike) backs off and checks again
//! within the deadline instead of giving up its rest after three
//! candidates in a row, and a run that never got to judge its candidates
//! is not reported `exhausted`.
//!
//! The storm is modelled as in `inuse.rs`: each of the first `stale` scans
//! finishes 2 s after it started (the clock jumps after the scan reads the
//! one process's environment), twice the freshness bound, so no deletion
//! may start on it. Every scan after those is fresh.

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use common::host;
use diskgc_core::engine::{Reason, UnknownCause};
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::host::Host;
use diskgc_core::inuse::STUCK_AFTER;
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::report::{Report, Status};
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;
use serde_json::Value;

const PROFILES: &str = r#"
- name: probe
  find: {roots: [/tmp], paths: ["c-*"], type: dir}
  action: delete
  examples: {match: [/tmp/c-1]}
- name: other
  find: {roots: [/tmp], paths: ["o-*"], type: dir}
  action: delete
  examples: {match: [/tmp/o-1]}
"#;

/// How long one candidate's three stale attempts take: three 2 s scans and
/// the 250 ms and 1 s pauses between them.
const STALE_CANDIDATE: Duration = Duration::from_millis(3 * 2000 + 250 + 1000);

/// `/tmp` (a 4 GiB tmpfs) holding `n` candidates `/tmp/c-01`.. (oldest
/// first) and `big`, a 3 GiB file of someone else's job; one readable
/// process (pid 10) that uses none of them. The first `stale` scans finish
/// stale; `events` are more event items.
fn model(n: usize, stale: u32, events: &str) -> ModelHost {
    model_with(n, stale, "", events)
}

/// [`model`] with more tree entries `extra` (indented as tree entries).
fn model_with(n: usize, stale: u32, extra: &str, events: &str) -> ModelHost {
    model_with_processes(n, stale, extra, "", events)
}

/// [`model_with`] with more processes `procs` (indented as process items).
fn model_with_processes(n: usize, stale: u32, extra: &str, procs: &str, events: &str) -> ModelHost {
    let mut tree = String::from(extra);
    for i in 1..=n {
        tree.push_str(&format!(
            "  /tmp/c-{i:02}: {{.: {{mtime: 2026-10-{i:02}T00:00:00Z}}, data: {{size: 4K, mtime: \
             2026-10-{i:02}T00:00:00Z}}}}\n"
        ));
    }
    let mut stale_events = String::new();
    for nth in 1..=stale {
        stale_events.push_str(&format!(
            "  - {{at: {{op: proc_environ, nth: {nth}, when: after}}, advance: 2s}}\n"
        ));
    }
    host(&format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 4G, parent: 1}}
tree:
  /run/diskgc/lock: {{uid: 0, gid: 0}}
  /usr/bin/tool: {{}}
  /tmp/big: {{size: 3G}}
{tree}processes:
  - {{pid: 10, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], environ: [PATH=/usr/bin], fds: [{{path: /usr/bin/tool}}]}}
{procs}events:
{stale_events}{events}"#
    ))
}

fn request(select: &str, min_free: &str, timeout: Option<Duration>) -> Request {
    let specs = parse_profiles(PROFILES).unwrap_or_else(|e| panic!("{e}"));
    let library = Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"));
    Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size(min_free).expect("a size"),
        target_free: None,
        timeout,
        tally_budget: None,
        watch: None,
        tier: Tier::select(select, &library, None, 1000).unwrap_or_else(|e| panic!("{e}")),
    }
}

/// A reclaim of `request` and its log.
fn ensure(host: &ModelHost, request: &Request) -> (Report, Vec<String>) {
    let mut lines = Vec::new();
    let report = executor::ensure(host, request, &[1000], &mut |l| lines.push(l.to_owned()));
    eprintln!("log:\n  {}", lines.join("\n  "));
    (report, lines)
}

fn candidate(i: usize) -> PathBuf {
    PathBuf::from(format!("/tmp/c-{i:02}"))
}

fn exists(host: &ModelHost, path: &std::path::Path) -> bool {
    host.stat_path(path).is_some()
}

fn acted(report: &Report) -> Vec<PathBuf> {
    report.actions.iter().map(|a| a.candidate.clone()).collect()
}

/// Every `in_use_unknown` row is a candidate of its own that was checked
/// (it spent its attempts, or its scan did not settle): none is a row the
/// tier skipped unchecked, and none is a count of many.
fn assert_each_unknown_was_checked(report: &Report) {
    for skip in report
        .skipped
        .iter()
        .filter(|s| s.reason == Reason::InUseUnknown)
    {
        assert_eq!(skip.count, 1, "{skip:#?}");
        assert_eq!(skip.unknown_cause, Some(UnknownCause::Busy), "{skip:#?}");
        let detail = skip.detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("attempt(s)") && !detail.starts_with("not checked"),
            "a candidate kept unchecked: {skip:#?}"
        );
    }
}

fn unknown(report: &Report) -> BTreeSet<PathBuf> {
    report
        .skipped
        .iter()
        .filter(|s| s.reason == Reason::InUseUnknown)
        .filter_map(|s| s.candidate.clone())
        .collect()
}

/// The pauses the run slept, merged: consecutive `sleep` calls of the
/// back-off's 50 ms slices are one pause. The fresh-scan retries' pauses
/// (250 ms, 1 s) are in it too.
fn pauses(host: &ModelHost) -> Vec<Duration> {
    let mut out: Vec<Duration> = Vec::new();
    let mut previous_was_sleep = false;
    for call in host.trace() {
        if call.op == Op::Sleep {
            let slice = parse_debug_duration(call.result.as_str());
            match (previous_was_sleep, out.last_mut()) {
                (true, Some(last)) => *last += slice,
                _ => out.push(slice),
            }
            previous_was_sleep = true;
        } else if call.op != Op::Now {
            // A clock read between slices (the back-off sleeps by the
            // clock) does not end a pause.
            previous_was_sleep = false;
        }
    }
    out
}

/// `Duration`'s `Debug` text as the model traces a sleep (`50ms`, `2s`,
/// `1.5µs`).
fn parse_debug_duration(text: &str) -> Duration {
    let (number, scale) = [("ns", 1e-9), ("µs", 1e-6), ("ms", 1e-3), ("s", 1.0)]
        .iter()
        .find_map(|(unit, scale)| text.strip_suffix(unit).map(|n| (n, *scale)))
        .unwrap_or_else(|| panic!("not a duration: {text:?}"));
    Duration::from_secs_f64(number.parse::<f64>().expect("a number") * scale)
}

/// The tier's back-off pauses: those longer than the longest fresh-scan
/// retry pause (1 s).
fn back_offs(host: &ModelHost) -> Vec<Duration> {
    pauses(host)
        .into_iter()
        .filter(|p| *p > Duration::from_secs(1))
        .collect()
}

/// TASK-340's dogfood in small: a storm long enough for four candidates in
/// a row to end `in_use_unknown`, then the host settles. The tier backs
/// off after the third and checks the fourth after a pause; that one is
/// still stale, so it waits longer before the fifth, which finds the host
/// settled; from there every candidate is checked and reclaimed. Master
/// skipped c-04 to c-08 unchecked as `in_use_unknown` after the third and
/// reported the run `exhausted` with nothing reclaimed.
#[test]
fn a_storm_that_settles_is_waited_out_and_the_rest_is_reclaimed() {
    let host = model(8, 12, "");
    let (report, _) = ensure(&host, &request("profile:probe", "100%", None));
    assert_each_unknown_was_checked(&report);
    assert_eq!(
        unknown(&report),
        (1..=4).map(candidate).collect::<BTreeSet<_>>(),
        "{report:#?}"
    );
    assert_eq!(
        acted(&report),
        (5..=8).map(candidate).collect::<Vec<_>>(),
        "{report:#?}"
    );
    for i in 5..=8 {
        assert!(!exists(&host, &candidate(i)));
    }
    for i in 1..=4 {
        assert!(exists(&host, &candidate(i)), "kept on a stale scan");
    }
    // Every candidate was judged: `exhausted` (exit 1) is the truth here.
    assert_eq!(report.status, Status::Exhausted, "{report:#?}");
    assert_eq!(report.exit_code(), 1);
    // Two back-offs, growing: before c-04 and before c-05.
    let waited = back_offs(&host);
    assert_eq!(waited.len(), 2, "{waited:?}");
    assert!(waited[1] > waited[0], "{waited:?}");
}

/// A short storm: exactly three candidates in a row stale, then the host
/// settles. One back-off, then every other candidate is reclaimed. Master
/// gave up the tier's rest after the third.
#[test]
fn a_short_storm_costs_one_back_off() {
    let host = model(6, 9, "");
    let (report, _) = ensure(&host, &request("profile:probe", "100%", None));
    assert_each_unknown_was_checked(&report);
    assert_eq!(
        unknown(&report),
        (1..=3).map(candidate).collect::<BTreeSet<_>>()
    );
    assert_eq!(acted(&report), (4..=6).map(candidate).collect::<Vec<_>>());
    assert_eq!(back_offs(&host).len(), 1, "{:?}", pauses(&host));
}

/// Below the threshold nothing changes: two stale candidates in a row cost
/// no back-off at all, and the third is checked at once and reclaimed.
#[test]
fn two_stale_candidates_in_a_row_cost_no_back_off() {
    let host = model(5, 6, "");
    let (report, _) = ensure(&host, &request("profile:probe", "100%", None));
    assert_eq!(
        unknown(&report),
        (1..=2).map(candidate).collect::<BTreeSet<_>>()
    );
    assert_eq!(acted(&report), (3..=5).map(candidate).collect::<Vec<_>>());
    assert!(back_offs(&host).is_empty(), "{:?}", pauses(&host));
}

/// A storm that outlasts the deadline: the tier backs off (pauses growing)
/// and checks again until the deadline, deletes nothing on a stale scan,
/// and ends at the deadline: `deadline` (exit 124: retry later), not
/// `exhausted` (exit 1: nothing more to reclaim). The candidates it never
/// got to are in no row: neither `kept` nor skipped. Master gave up after
/// 22 s with 30 s left, kept all eight (five unchecked) and exited 1.
#[test]
fn a_storm_until_the_deadline_ends_deadline_and_leaves_the_rest_unjudged() {
    let timeout = Duration::from_secs(60);
    let host = model(8, 1000, "");
    let started = host.now();
    let (report, lines) = ensure(&host, &request("profile:probe", "100%", Some(timeout)));
    let took = host.now().saturating_since(started);
    assert!(
        acted(&report).is_empty(),
        "deleted on a stale scan: {report:#?}"
    );
    for i in 1..=8 {
        assert!(exists(&host, &candidate(i)));
    }
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert_eq!(report.exit_code(), 124);
    assert_each_unknown_was_checked(&report);
    let judged = unknown(&report);
    assert!(
        judged.len() > 3 && judged.len() < 8,
        "checked again after backing off, not all: {judged:?}"
    );
    // The ones it never reached are in no row at all.
    assert!(
        !report
            .skipped
            .iter()
            .any(|s| s.candidate.as_ref() == Some(&candidate(8)) || s.count > 1),
        "{report:#?}"
    );
    // It used its deadline, and no more than it.
    assert!(took >= timeout, "gave up {took:?} into a {timeout:?} run");
    assert!(
        took < timeout + Duration::from_secs(1),
        "ran {took:?} into a {timeout:?} run"
    );
    let waited = back_offs(&host);
    assert!(waited.len() >= 2, "{waited:?}");
    assert!(
        waited
            .windows(2)
            .take(waited.len() - 2)
            .all(|w| w[1] > w[0]),
        "back-off grows: {waited:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("until the deadline")),
        "the log says why the tier stopped: {lines:#?}"
    );
    // The JSON: `deadline`, 124; kept counts only the checked candidates.
    let json: Value = serde_json::from_str(&report.to_json()).expect("JSON");
    assert_eq!(json["status"], "deadline");
    assert_eq!(json["exit_code"], 124);
    let summary = &json["summary"][0];
    assert_eq!(
        summary["kept"][0]["reason"], "in_use_unknown",
        "{summary:#}"
    );
    assert_eq!(
        summary["kept"][0]["count"],
        u64::try_from(judged.len()).expect("small"),
        "{summary:#}"
    );
    assert_eq!(summary["kept"].as_array().map(Vec::len), Some(1));
    assert_eq!(summary["reclaimed"]["count"], 0);
    // A live run's `not_judged` stays `null` (docs/reference.md, Output:
    // `summary`): the deadline status is what says some were not judged.
    assert!(summary["not_judged"].is_null(), "{summary:#}");
}

/// A storm through a union tier (the reclaim walk's third loop, several
/// source terms): the same back-off, then the rest reclaimed.
#[test]
fn a_union_tier_waits_out_a_storm_too() {
    let host = model(6, 12, "");
    let (report, _) = ensure(
        &host,
        &request("profile:probe || profile:other", "100%", None),
    );
    assert_each_unknown_was_checked(&report);
    assert_eq!(
        unknown(&report),
        (1..=4).map(candidate).collect::<BTreeSet<_>>(),
        "{report:#?}"
    );
    assert_eq!(acted(&report), (5..=6).map(candidate).collect::<Vec<_>>());
    assert_eq!(report.status, Status::Exhausted);
}

/// Space comes back during a back-off (someone else's job removes its
/// 3 GiB): the tier samples the space after the pause and stops at the
/// goal instead of deleting more. The first back-off starts once the third
/// stale candidate is done; `big` goes 1 s into it.
#[test]
fn space_that_comes_back_during_a_back_off_ends_the_tier_at_the_goal() {
    let into_back_off = STALE_CANDIDATE * 3 + Duration::from_secs(1);
    let host = model(
        6,
        9,
        &format!(
            "  - {{at: {{elapsed: {}ms}}, remove: /tmp/big}}\n",
            into_back_off.as_millis()
        ),
    );
    let (report, _) = ensure(&host, &request("profile:probe", "2G", None));
    assert!(
        acted(&report).is_empty(),
        "deleted after the goal: {report:#?}"
    );
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert!(host.unfired_events().is_empty());
    for i in 1..=6 {
        assert!(exists(&host, &candidate(i)));
    }
}

/// SIGINT during a back-off ends it within one 50 ms slice and stops the
/// run (`interrupted`); no candidate is checked after it.
#[test]
fn a_signal_during_a_back_off_stops_the_run_promptly() {
    let into_back_off = STALE_CANDIDATE * 3 + Duration::from_millis(500);
    let host = model(
        6,
        9,
        &format!(
            "  - {{at: {{elapsed: {}ms}}, signal: int}}\n",
            into_back_off.as_millis()
        ),
    );
    let started = host.now();
    let (report, _) = ensure(&host, &request("profile:probe", "100%", None));
    assert!(
        matches!(report.status, Status::Interrupted(_)),
        "{report:#?}"
    );
    assert!(acted(&report).is_empty());
    assert_eq!(
        unknown(&report),
        (1..=3).map(candidate).collect::<BTreeSet<_>>()
    );
    let took = host.now().saturating_since(started);
    assert!(
        took <= into_back_off + Duration::from_millis(100),
        "the back-off ran on {took:?} after a signal at {into_back_off:?}"
    );
}

/// A classic Cargo profile directory built `at`, with 1 MiB in `deps/`.
fn cargo_profile(at: &str) -> String {
    format!(
        "{{.: {{mtime: {at}}}, .cargo-lock: {{mtime: {at}}}, .fingerprint: {{.: {{mtime: {at}}}, \
         p-1: {{.: {{mtime: {at}}}, bin-p: {{mtime: {at}}}}}}}, deps: {{.: {{mtime: {at}}}, p-1: \
         {{size: 1M, mtime: {at}}}}}}}"
    )
}

/// `n` Cargo targets `/home/u/src/pNN/target` (oldest first) on a `/home`
/// with little space; one readable process using none of them. The first
/// `stale` scans finish stale.
fn cargo_model(n: usize, stale: u32) -> ModelHost {
    let mut stale_events = String::new();
    for nth in 1..=stale {
        stale_events.push_str(&format!(
            "  - {{at: {{op: proc_environ, nth: {nth}, when: after}}, advance: 2s}}\n"
        ));
    }
    cargo_model_with_events(n, &stale_events)
}

/// [`cargo_model`] with event items `events` instead of stale scans.
fn cargo_model_with_events(n: usize, events: &str) -> ModelHost {
    let mut tree = String::new();
    for i in 1..=n {
        tree.push_str(&format!(
            "  /home/u/src/p{i:02}/Cargo.toml: {{}}\n  /home/u/src/p{i:02}/target: {{CACHEDIR.TAG: \
             {{cachedir_tag: true}}, debug: {}}}\n",
            cargo_profile(&format!("2026-10-{i:02}T00:00:00Z"))
        ));
    }
    host(&format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 2, at: /home, fs: ext4, dev: "254:2", size: 100G, other_used: 99G, parent: 1}}
tree:
  /run/diskgc/lock: {{uid: 0, gid: 0}}
  /usr/bin/tool: {{}}
{tree}processes:
  - {{pid: 10, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], environ: [PATH=/usr/bin], fds: [{{path: /usr/bin/tool}}]}}
events:
{events}"#
    ))
}

/// The Cargo tier's loop (the reclaim walk's first): a storm long enough
/// for three targets in a row to end `in_use_unknown` (a Cargo target's
/// check reads the process more than once per attempt, so the count of
/// stale scans is not three per target), a back-off, then the host has
/// settled and the rest is reclaimed. Master skipped the rest unchecked.
#[test]
fn a_cargo_tier_waits_out_a_storm_too() {
    let host = cargo_model(6, 14);
    let library = Library::builtin().unwrap_or_else(|e| panic!("{e}"));
    let request = Request {
        path: PathBuf::from("/home/u/src"),
        min_free: parse_size("100%").expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier: Tier::select(
            "profile:cargo-target",
            &library,
            Some(&["/home/u/src".to_owned()]),
            1000,
        )
        .unwrap_or_else(|e| panic!("{e}")),
    };
    let (report, _) = ensure(&host, &request);
    let target = |i: usize| PathBuf::from(format!("/home/u/src/p{i:02}/target"));
    assert_each_unknown_was_checked(&report);
    // A Cargo skip names the part it stopped at.
    let kept: BTreeSet<PathBuf> = unknown(&report)
        .into_iter()
        .map(|part| {
            part.ancestors()
                .find(|a| a.ends_with("target"))
                .expect("a part of a target")
                .to_owned()
        })
        .collect();
    assert_eq!(kept, (1..=3).map(target).collect(), "{report:#?}");
    assert_eq!(
        acted(&report),
        (4..=6).map(target).collect::<Vec<_>>(),
        "{report:#?}"
    );
    assert_eq!(back_offs(&host).len(), 1, "{:?}", pauses(&host));
}

/// A pause oversleeps on a loaded host (here the clock jumps 5 s during a
/// slice of the last back-off, which the deadline cut to 2.5 s): the
/// back-off sleeps by the clock, so it ends at once instead of sleeping its
/// remaining slices too, and the crossing of the deadline is reported in
/// `overruns` (the PRD: `--timeout` + 10 s unless reported).
#[test]
fn a_back_off_that_oversleeps_the_deadline_stops_at_once_and_reports_it() {
    let timeout = Duration::from_secs(60);
    // The last back-off starts at 57.5 s (see the deadline test above).
    let host = model(8, 1000, "  - {at: {elapsed: 58s}, advance: 5s}\n");
    let started = host.now();
    let (report, _) = ensure(&host, &request("profile:probe", "100%", Some(timeout)));
    let took = host.now().saturating_since(started);
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert!(acted(&report).is_empty());
    let overrun = report
        .overruns
        .iter()
        .find(|o| o.operation == "busy back-off")
        .unwrap_or_else(|| panic!("no back-off overrun: {:#?}", report.overruns));
    assert!(
        (2900..=3100).contains(&overrun.over_ms),
        "over by the 5 s jump less the 2 s left: {overrun:?}"
    );
    assert!(
        took < Duration::from_secs(63) + Duration::from_millis(100),
        "slept on after the jump: {took:?}"
    );
}

/// A configured tier with its own `timeout` (shorter than the run's) that
/// waits for a busy host until it: the tier is excluded with an error that
/// names the busy host, and the next tier runs on the settled host (exit 3
/// below `--min-free`, as for any tier timeout; docs/reference.md, Exit
/// codes).
#[test]
fn a_tier_timeout_during_a_storm_excludes_the_tier_naming_the_busy_host() {
    let policy = format!(
        "version: 1\nprofiles:{PROFILES}groups:\n  - name: g\n    tiers:\n      - name: first\n        \
         select: profile:probe\n        timeout: 40s\n      - name: second\n        select: \
         profile:other\n"
    );
    let config = diskgc_core::config::parse(
        std::path::Path::new("/etc/diskgc/t.yaml"),
        policy.as_bytes(),
    )
    .unwrap_or_else(|e| panic!("{e}\n{policy}"));
    let mut tiers = Vec::new();
    let mut timeouts = Vec::new();
    for group in &config.groups {
        for setting in &group.tiers {
            tiers.push(
                Tier::configured(&group.name, setting, &config.library, 1000)
                    .unwrap_or_else(|e| panic!("{e}")),
            );
            timeouts.push(setting.timeout);
        }
    }
    // Fourteen stale scans: c-01 to c-04 in full and two of c-05's, whose
    // third attempt would pause past the tier's 40 s; the tier then backs
    // off into its timeout. The second tier's scans are fresh.
    let others = "  /tmp/o-1: {.: {mtime: 2026-10-01T00:00:00Z}}\n  /tmp/o-2: {.: {mtime: \
                  2026-10-02T00:00:00Z}}\n";
    let host = model_with(6, 14, others, "");
    let mut request = request("profile:probe", "100%", Some(Duration::from_secs(600)));
    request.tier = tiers[0].clone();
    let mut lines = Vec::new();
    let report = executor::ensure_grouped(&host, &request, &tiers, &timeouts, &[1000], &mut |l| {
        lines.push(l.to_owned())
    });
    eprintln!("log:\n  {}", lines.join("\n  "));
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    let error = report
        .errors
        .iter()
        .find(|e| e.tier.as_deref() == Some("first"))
        .unwrap_or_else(|| panic!("{report:#?}"));
    assert!(error.message.contains("too busy"), "{error:?}");
    assert!(
        report.actions.iter().all(|a| a.tier == "second"),
        "{report:#?}"
    );
    assert_eq!(
        acted(&report),
        [PathBuf::from("/tmp/o-1"), PathBuf::from("/tmp/o-2")]
    );
    for i in 1..=6 {
        assert!(exists(&host, &candidate(i)));
    }
}

/// `/usr/bin/rust-lld` in the tree, for a linker's process.
const LINKER_TREE: &str = "  /usr/bin/rust-lld: {}\n";

/// A short-lived linker of someone else's build, pid 11, whose `/proc`
/// reads never hold still while it exits (`LinuxHost`: "no trustworthy
/// read in 16 rounds: no task of a complete listing was live").
const EXITING_LINKER: &str =
    "  - {pid: 11, exe: /usr/bin/rust-lld, maps: [/usr/bin/rust-lld], proc: unsettled}\n";

/// A shell of the user started with `newgrp` (pid 12): dumpable, all its
/// uids the user's, refused for its group. It stays so while it runs.
const NEWGRP_SHELL: &str =
    "  - {pid: 12, exe: /usr/bin/tool, name: bash, gid: 1001, proc: denied}\n";

/// The `in_use_unknown` rows in report order, as (candidate, cause,
/// checked, the first blocker's pid).
fn unknown_rows(report: &Report) -> Vec<(PathBuf, Option<UnknownCause>, bool, Option<u32>)> {
    report
        .skipped
        .iter()
        .filter(|s| s.reason == Reason::InUseUnknown)
        .map(|s| {
            (
                s.candidate.clone().expect("one row per candidate"),
                s.unknown_cause,
                !s.detail.as_deref().unwrap_or("").starts_with("not checked"),
                s.process_blockers
                    .as_deref()
                    .and_then(<[_]>::first)
                    .map(|b| b.pid),
            )
        })
        .collect()
}

/// TASK-348's dogfood in small (tmp/walks/2026-11-04-dogfood/a4-rc-real):
/// a concurrent build's linker exits slowly while the tier runs, and every
/// read of it in the meantime ends unsettled. That is a busy host (waiting
/// ends it): the tier waits it out before checking c-01 again (TASK-364),
/// the linker is gone after the first back-off, and every candidate is
/// reclaimed, c-01 included. TASK-348 kept c-01 to c-03 `in_use_unknown`
/// before its back-off; before it, c-04 to c-08 were skipped unchecked as a
/// process that cannot be inspected (127,645 candidates in the dogfood).
#[test]
fn a_process_whose_reads_do_not_settle_is_waited_out_like_a_busy_host() {
    let host = model_with_processes(
        8,
        0,
        LINKER_TREE,
        EXITING_LINKER,
        "  - {at: {elapsed: 1s}, exit: 11}\n",
    );
    let (report, lines) = ensure(&host, &request("profile:probe", "100%", None));
    assert!(unknown_rows(&report).is_empty(), "{report:#?}");
    assert_eq!(
        acted(&report),
        (1..=8).map(candidate).collect::<Vec<_>>(),
        "{report:#?}"
    );
    assert_eq!(report.status, Status::Exhausted, "{report:#?}");
    assert_eq!(
        back_offs(&host),
        [Duration::from_secs(2)],
        "{:?}",
        pauses(&host)
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("/tmp/c-01: the host is too busy")
                && l.contains("pid 11 (rust-lld)")
                && l.contains("no trustworthy read")),
        "the wait names the process and why: {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("checking /tmp/c-01 again")),
        "{lines:#?}"
    );
}

/// TASK-348's follow-up (TASK-364): a process whose reads never settle and
/// that never exits (a task stuck in its exit) is waited out for four
/// back-offs (2 + 4 + 8 + 16 s, [`STUCK_AFTER`]); then it is taken to be
/// stuck, a process that cannot be inspected (`unreadable_process`, a
/// cause waiting does not end, naming it): three candidates in a row kept
/// on its account skip the rest of the tier unchecked, with no further
/// back-off, long before the deadline. Nothing is deleted. TASK-348 held
/// the tier to its deadline instead (124).
#[test]
fn a_process_that_never_settles_is_taken_to_be_stuck_after_four_back_offs() {
    let timeout = Duration::from_secs(600);
    let host = model_with_processes(8, 0, LINKER_TREE, EXITING_LINKER, "");
    let started = host.now();
    let (report, _) = ensure(&host, &request("profile:probe", "100%", Some(timeout)));
    let took = host.now().saturating_since(started);
    assert!(acted(&report).is_empty(), "{report:#?}");
    for i in 1..=8 {
        assert!(exists(&host, &candidate(i)));
    }
    assert_eq!(report.status, Status::Exhausted, "{report:#?}");
    assert_eq!(
        unknown_rows(&report),
        (1..=8)
            .map(|i| (
                candidate(i),
                Some(UnknownCause::UnreadableProcess),
                i <= 3,
                Some(11)
            ))
            .collect::<Vec<_>>(),
        "{report:#?}"
    );
    assert!(
        report.skipped[0]
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("taken to be stuck")),
        "{report:#?}"
    );
    assert_eq!(
        back_offs(&host),
        [2, 4, 8, 16].map(Duration::from_secs),
        "{:?}",
        pauses(&host)
    );
    assert!(
        took >= STUCK_AFTER && took < STUCK_AFTER + Duration::from_secs(1),
        "{took:?}"
    );
}

/// TASK-340's other half, kept: a process of the user the kernel refuses
/// to show for a reason no status explains (a `newgrp` shell) stays so
/// while it runs. Three candidates in a row kept on its account skip the
/// tier's rest unchecked at once, without a back-off, each naming it
/// (`unreadable_process`, pid 12).
#[test]
fn a_refused_process_of_the_user_still_skips_the_rest_without_a_back_off() {
    let host = model_with_processes(6, 0, "", NEWGRP_SHELL, "");
    let started = host.now();
    let (report, _) = ensure(&host, &request("profile:probe", "100%", None));
    assert!(acted(&report).is_empty(), "{report:#?}");
    let rows = unknown_rows(&report);
    assert_eq!(
        rows,
        (1..=6)
            .map(|i| (
                candidate(i),
                Some(UnknownCause::UnreadableProcess),
                i <= 3,
                Some(12)
            ))
            .collect::<Vec<_>>(),
        "{report:#?}"
    );
    assert!(back_offs(&host).is_empty(), "{:?}", pauses(&host));
    let took = host.now().saturating_since(started);
    assert!(took < Duration::from_secs(2), "no back-off: {took:?}");
}

/// Both at once: one scan finds the exiting linker and the `newgrp` shell.
/// The shell keeps every candidate whether or not the linker settles, so
/// it decides: the skips name it (`unreadable_process`), and three of them
/// skip the rest without a back-off, as the shell alone would.
#[test]
fn a_refused_process_decides_over_one_whose_reads_do_not_settle() {
    let procs = format!("{EXITING_LINKER}{NEWGRP_SHELL}");
    let host = model_with_processes(5, 0, LINKER_TREE, &procs, "");
    let (report, _) = ensure(&host, &request("profile:probe", "100%", None));
    assert!(acted(&report).is_empty(), "{report:#?}");
    let rows = unknown_rows(&report);
    assert_eq!(
        rows,
        (1..=5)
            .map(|i| (
                candidate(i),
                Some(UnknownCause::UnreadableProcess),
                i <= 3,
                Some(12)
            ))
            .collect::<Vec<_>>(),
        "{report:#?}"
    );
    assert!(back_offs(&host).is_empty(), "{:?}", pauses(&host));
}

/// Codex review of TASK-348: a lasting refusal is not hidden by later
/// rereads that do not settle. The `newgrp` shell's first read in the scan
/// is refused; from then on its task list churns, so every reread of the
/// scan ends unsettled before it reaches the refusal. The refusal was
/// seen and nothing since showed it ended (no successful read, no exit or
/// replacement), so the lasting cause decides: the candidates stay, three
/// skip the rest naming it (`unreadable_process`), without a back-off.
#[test]
fn a_refusal_then_rereads_that_do_not_settle_keeps_the_lasting_cause() {
    let host = model_with_processes(
        5,
        0,
        "",
        NEWGRP_SHELL,
        "  - {at: {op: proc_fds, path: /proc/12/fd, when: after}, access: {pid: 12, proc: \
         unsettled}}\n",
    );
    let (report, _) = ensure(&host, &request("profile:probe", "100%", None));
    assert!(acted(&report).is_empty(), "{report:#?}");
    for i in 1..=5 {
        assert!(exists(&host, &candidate(i)));
    }
    let rows = unknown_rows(&report);
    assert_eq!(
        rows,
        (1..=5)
            .map(|i| (
                candidate(i),
                Some(UnknownCause::UnreadableProcess),
                i <= 3,
                Some(12)
            ))
            .collect::<Vec<_>>(),
        "{report:#?}"
    );
    assert!(back_offs(&host).is_empty(), "{:?}", pauses(&host));
}

/// Codex review of TASK-348: a process whose reads do not settle is never
/// excused by what its status shows (another user's, setuid, more capable,
/// non-dumpable): only a refusal can be explained by a privilege boundary.
/// It blocks every candidate as a busy host's.
#[test]
fn a_process_whose_reads_do_not_settle_is_never_excused_by_its_status() {
    for status in [
        "uid: 2000",
        "euid: 0",
        "privileged: true",
        "non_dumpable: true",
    ] {
        let procs = format!(
            "  - {{pid: 11, exe: /usr/bin/rust-lld, maps: [/usr/bin/rust-lld], {status}, proc: \
             unsettled}}\n"
        );
        let host = model_with_processes(1, 0, LINKER_TREE, &procs, "");
        // A deadline, so a regression that waits for ever fails instead.
        let (report, _) = ensure(
            &host,
            &request("profile:probe", "100%", Some(Duration::from_secs(600))),
        );
        assert!(acted(&report).is_empty(), "{status}: {report:#?}");
        assert!(exists(&host, &candidate(1)), "{status}");
        // Waited out as a busy host until it was taken to be stuck
        // (TASK-364), never excused on the way.
        assert_eq!(
            unknown_rows(&report),
            [(
                candidate(1),
                Some(UnknownCause::UnreadableProcess),
                true,
                Some(11)
            )],
            "{status}: {report:#?}"
        );
        assert_eq!(back_offs(&host).len(), 4, "{status}: {:?}", pauses(&host));
    }
}

// TASK-364: a scan that could judge nothing because the host was busy is
// waited out before the candidate it was made for is given up, whatever
// the size of the tier, in a reclaim and in a dry run alike. TASK-317's
// gate (release_candidate_pip_cache_*: one candidate, "the process list
// was still growing ... after 16 listings", one 20 ms retry, then
// `in_use_unknown` and `exhausted` with ~119 s of its deadline left) and
// TASK-363's ccache run are this shape.

/// A fork storm over the first `listings` process listings: pid 10 forks
/// one child after each, so each listing brings a new process. A scan
/// gives up after 16 listings that each did ([`MAX_LISTINGS`]), so
/// `storm(32)` leaves a scan and its 20 ms retry unsettled, and the next
/// scan settles.
fn storm(listings: u32) -> String {
    (1..=listings)
        .map(|k| {
            format!(
                "  - {{at: {{op: pids, nth: {k}, when: after}}, fork: {{pid: 10, child: {}}}}}\n",
                1000 + k
            )
        })
        .collect()
}

/// A dry run of `request` and its log.
fn dry_run(host: &ModelHost, request: &Request) -> (Report, Vec<String>) {
    let mut lines = Vec::new();
    let report = executor::dry_run(host, request, &[1000], &mut |l| lines.push(l.to_owned()));
    eprintln!("log:\n  {}", lines.join("\n  "));
    (report, lines)
}

/// The dry run's rows, as (candidate, reason).
fn plan_rows(report: &Report) -> Vec<(PathBuf, Option<Reason>)> {
    report
        .plan
        .iter()
        .filter_map(|p| Some((p.candidate.clone()?, p.reason)))
        .collect()
}

/// A tier of one candidate, and one of two, whose first scan and its quick
/// retry do not settle (a storm of 32 growing listings): the tier backs off
/// once (2 s), scans again, finds the host settled and reclaims every
/// candidate. On the stack's base each was kept `in_use_unknown` (busy) and
/// the run ended `exhausted` at once: TASK-340's back-off needs three in a
/// row.
#[test]
fn small_tiers_wait_out_scans_that_do_not_settle_and_reclaim() {
    for n in [1, 2] {
        let host = model(n, 0, &storm(32));
        let (report, lines) = ensure(
            &host,
            &request("profile:probe", "100%", Some(Duration::from_secs(120))),
        );
        assert!(unknown_rows(&report).is_empty(), "{n}: {report:#?}");
        assert_eq!(
            acted(&report),
            (1..=n).map(candidate).collect::<Vec<_>>(),
            "{n}: {report:#?}"
        );
        for i in 1..=n {
            assert!(!exists(&host, &candidate(i)), "{n}");
        }
        assert_eq!(
            back_offs(&host),
            [Duration::from_secs(2)],
            "{n}: {:?}",
            pauses(&host)
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("the host is too busy") && l.contains("still growing")),
            "{n}: {lines:#?}"
        );
        assert!(host.unfired_events().is_empty(), "{n}");
    }
}

/// The same storm under a dry run: the plan's checker waits it out before
/// judging, so the plan says what the reclaim would do once the host
/// settles (eligible), not `in_use_unknown` (the stack's base: the plan
/// had no retry beyond the 20 ms one). Nothing is deleted.
#[test]
fn a_dry_run_waits_out_scans_that_do_not_settle() {
    let host = model(1, 0, &storm(32));
    let (report, lines) = dry_run(
        &host,
        &request("profile:probe", "100%", Some(Duration::from_secs(120))),
    );
    assert_eq!(report.status, Status::DryRun, "{report:#?}");
    assert_eq!(plan_rows(&report), [(candidate(1), None)], "{report:#?}");
    assert!(exists(&host, &candidate(1)));
    assert_eq!(
        back_offs(&host),
        [Duration::from_secs(2)],
        "{:?}",
        pauses(&host)
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("/tmp/c-01: the host is too busy")),
        "the wait is logged: {lines:#?}"
    );
}

/// A storm that outlasts the deadline, for a tier of one candidate: the
/// tier backs off (growing pauses) until the deadline, deletes nothing,
/// keeps the candidate as `busy` (it was checked), and ends `deadline`
/// (124: retry later) with the time it was given used, no more. A dry run
/// ends the same way, its plan keeping the candidate's `in_use_unknown`
/// row. On the stack's base both ended at once (`exhausted`, 1, and a
/// `dry_run` plan of that row, 0) with the deadline unused.
#[test]
fn a_storm_until_the_deadline_ends_a_single_candidate_run_124() {
    let timeout = Duration::from_secs(60);
    let host = model(1, 0, &storm(400));
    let started = host.now();
    let (report, lines) = ensure(&host, &request("profile:probe", "100%", Some(timeout)));
    let took = host.now().saturating_since(started);
    assert!(acted(&report).is_empty(), "{report:#?}");
    assert!(exists(&host, &candidate(1)));
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert_eq!(report.exit_code(), 124);
    assert_eq!(
        unknown_rows(&report),
        [(candidate(1), Some(UnknownCause::Busy), true, None)],
        "{report:#?}"
    );
    assert!(
        took >= timeout && took < timeout + Duration::from_secs(1),
        "{took:?}"
    );
    let waited = back_offs(&host);
    assert!(waited.len() >= 4, "{waited:?}");
    assert!(
        waited
            .windows(2)
            .take(waited.len() - 2)
            .all(|w| w[1] > w[0]),
        "back-off grows: {waited:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("until the deadline")),
        "{lines:#?}"
    );

    let host = model(1, 0, &storm(400));
    let started = host.now();
    let (report, _) = dry_run(&host, &request("profile:probe", "100%", Some(timeout)));
    let took = host.now().saturating_since(started);
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert_eq!(report.exit_code(), 124);
    assert_eq!(
        plan_rows(&report),
        [(candidate(1), Some(Reason::InUseUnknown))],
        "the candidate waited for keeps its busy verdict, as in the reclaim: {report:#?}"
    );
    assert!(
        took >= timeout && took < timeout + Duration::from_secs(1),
        "{took:?}"
    );
}

/// A cause that stays (a `newgrp` shell the kernel refuses to show) is
/// not waited out, in a tier of one candidate and in its dry run: the
/// candidate is kept `unreadable_process` at once, naming it. The stack's
/// base did the same; the mutation "wait out any blocked scan" fails it.
#[test]
fn a_lasting_cause_skips_a_single_candidate_without_waiting() {
    // A deadline, so a regression that waits fails instead of hanging.
    let timeout = Some(Duration::from_secs(600));
    let host = model_with_processes(1, 0, "", NEWGRP_SHELL, "");
    let started = host.now();
    let (report, _) = ensure(&host, &request("profile:probe", "100%", timeout));
    assert!(acted(&report).is_empty(), "{report:#?}");
    assert_eq!(
        unknown_rows(&report),
        [(
            candidate(1),
            Some(UnknownCause::UnreadableProcess),
            true,
            Some(12)
        )],
        "{report:#?}"
    );
    assert!(back_offs(&host).is_empty(), "{:?}", pauses(&host));
    assert!(host.now().saturating_since(started) < Duration::from_secs(1));

    let host = model_with_processes(1, 0, "", NEWGRP_SHELL, "");
    let (report, _) = dry_run(&host, &request("profile:probe", "100%", timeout));
    assert_eq!(
        plan_rows(&report),
        [(candidate(1), Some(Reason::InUseUnknown))],
        "{report:#?}"
    );
    assert!(back_offs(&host).is_empty(), "{:?}", pauses(&host));
}

// TASK-366: the project contracts read every process's command line
// (fs_kind/project.rs `argv_naming`). One that does not hold still to be
// read (a process exiting while it is read) is a busy host's, waited out as
// a scan that did not settle is; one that cannot be read for a lasting
// cause still keeps every project it may name.

/// Two idle Node projects `/home/tester/p` and `/home/tester/q`, each with
/// its `node_modules`, a shell that uses neither, and process 950 (a
/// container's, `node` from elsewhere) with `cmdline_950` as its command
/// line, which uses neither; `events` are more event items.
fn projects_model(cmdline_950: &str, events: &str) -> ModelHost {
    let project = |name: &str| {
        format!(
            r#"  /home/tester/{name}:
    .: {{mtime: 2026-08-16T03:00:00Z}}
    package.json: {{content: "{{\"name\": \"{name}\", \"version\": \"1.0.0\"}}", mtime: 2026-08-16T03:00:00Z}}
    src:
      .: {{mtime: 2026-08-16T03:00:00Z}}
      a.js: {{mtime: 2026-08-16T03:00:00Z}}
    node_modules:
      .: {{mtime: 2026-08-16T03:00:00Z}}
      x:
        .: {{mtime: 2026-08-16T03:00:00Z}}
        i.js: {{size: 4K, mtime: 2026-08-16T03:00:00Z}}
"#
        )
    };
    host(&format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 10G}}
  - {{id: 2, at: /home, fs: ext4, dev: "254:2", size: 10G, parent: 1}}
tree:
  /run/diskgc/lock: {{uid: 0, gid: 0}}
  /usr/bin/bash: {{}}
  /usr/bin/node: {{}}
  /home/tester/.bash_history: {{size: 1K}}
{}{}processes:
  - {{pid: 900, exe: /usr/bin/bash, cwd: /home/tester, maps: [/usr/bin/bash], environ: ["HOME=/home/tester"], fds: [{{path: /usr/bin/bash}}, {{path: /home/tester/.bash_history}}]}}
  - {{pid: 950, exe: /usr/bin/node, cwd: /, cmdline: {cmdline_950}, maps: [/usr/bin/node], environ: ["HOME=/root"], fds: [{{path: /usr/bin/node}}]}}
events:
{events}"#,
        project("p"),
        project("q"),
    ))
}

/// The built-in `node-modules` profile below `/home/tester`.
fn projects_request(timeout: Option<Duration>) -> Request {
    let library = Library::builtin().unwrap_or_else(|e| panic!("{e}"));
    Request {
        path: PathBuf::from("/home"),
        min_free: parse_size("100%").expect("a size"),
        target_free: None,
        timeout,
        tally_budget: None,
        watch: None,
        tier: Tier::select(
            "profile:node-modules",
            &library,
            Some(&["/home/tester".to_owned()]),
            1000,
        )
        .unwrap_or_else(|e| panic!("{e}")),
    }
}

fn node_modules(project: &str) -> PathBuf {
    PathBuf::from(format!("/home/tester/{project}/node_modules"))
}

/// TASK-348's e2e run 1 in small (TASK-366): a container process, read in
/// full by the scan, is exiting when its command line is read ("no listed
/// task gave its command line while live"). That is a busy host: both
/// projects wait for one back-off, the process is gone by then, and both
/// `node_modules` are reclaimed; the dry run waits likewise and plans both
/// eligible. On the stack's base its unread command line kept every project
/// `in_use` ("its command line could not be read ..., so it may name the
/// project"), in the reclaim and the plan.
#[test]
fn a_command_line_that_does_not_hold_still_is_waited_out_for_every_project() {
    let exits = "  - {at: {elapsed: 1s}, exit: 950}\n";
    let host = projects_model("unsettled", exits);
    let (report, _) = dry_run(&host, &projects_request(Some(Duration::from_secs(120))));
    assert_eq!(
        plan_rows(&report),
        [(node_modules("p"), None), (node_modules("q"), None)],
        "{report:#?}"
    );
    assert_eq!(
        back_offs(&host),
        [Duration::from_secs(2)],
        "{:?}",
        pauses(&host)
    );

    let host = projects_model("unsettled", exits);
    let (report, lines) = ensure(&host, &projects_request(Some(Duration::from_secs(120))));
    assert!(report.skipped.is_empty(), "{report:#?}");
    assert_eq!(
        acted(&report),
        [node_modules("p"), node_modules("q")],
        "{report:#?}"
    );
    assert!(!exists(&host, &node_modules("p")));
    assert!(!exists(&host, &node_modules("q")));
    assert_eq!(
        back_offs(&host),
        [Duration::from_secs(2)],
        "{:?}",
        pauses(&host)
    );
    assert!(
        lines.iter().any(|l| l.contains("the host is too busy")
            && l.contains("pid 950 (node): its command line did not hold still")),
        "the wait names the process and its command line: {lines:#?}"
    );
}

/// The guarantee kept: a command line that is never read is never taken to
/// name nothing. One that never holds still (its process stuck in its
/// exit) is waited out for [`STUCK_AFTER`], then keeps every project as an
/// unread command line does (`in_use`, evidence `unreadable`, naming pid
/// 950), with no further back-off; one that cannot be read for a lasting
/// cause (`Failed`) keeps every project at once, without waiting. Nothing
/// is deleted.
#[test]
fn an_unread_command_line_still_keeps_every_project() {
    for (cmdline, waits) in [("unsettled", 4), ("unreadable", 0)] {
        let host = projects_model(cmdline, "");
        let (report, _) = ensure(&host, &projects_request(Some(Duration::from_secs(600))));
        assert!(acted(&report).is_empty(), "{cmdline}: {report:#?}");
        for project in ["p", "q"] {
            assert!(exists(&host, &node_modules(project)), "{cmdline}");
            let skip = report
                .skipped
                .iter()
                .find(|s| s.candidate.as_ref() == Some(&node_modules(project)))
                .unwrap_or_else(|| panic!("{cmdline}: {project} not kept: {report:#?}"));
            assert_eq!(skip.reason, Reason::InUse, "{cmdline}: {skip:#?}");
            assert!(
                skip.detail
                    .as_deref()
                    .is_some_and(|d| d.contains("command line could not be read")),
                "{cmdline}: {skip:#?}"
            );
            assert_eq!(
                skip.process_blockers
                    .as_deref()
                    .map(|b| b.iter().map(|b| b.pid).collect::<Vec<_>>()),
                Some(vec![950]),
                "{cmdline}: {skip:#?}"
            );
        }
        assert_eq!(
            back_offs(&host).len(),
            waits,
            "{cmdline}: {:?}",
            pauses(&host)
        );
    }
}

/// A Cargo target is reclaimed part by part, each part checked on its own
/// scan. When the host turns busy after its first part was deleted (here
/// pid 10's reads stop holding still once the first entry is unlinked, and
/// the clock moves past the scan's reuse), the next part's scan judges
/// nothing and the target stops there, part-way, as for `in_use`. That
/// target is recorded as it is, its deletion an action, not run again
/// after a back-off: a run again would report only the second run's
/// deletions and lose the first's.
#[test]
fn a_cargo_target_the_host_turns_busy_on_part_way_is_recorded_not_retried() {
    let host = cargo_model_with_events(
        1,
        "  - {at: {op: unlinkat, nth: 1, when: after}, access: {pid: 10, proc: unsettled}}\n  - \
         {at: {op: unlinkat, nth: 1, when: after}, advance: 2s}\n",
    );
    let library = Library::builtin().unwrap_or_else(|e| panic!("{e}"));
    let request = Request {
        path: PathBuf::from("/home/u/src"),
        min_free: parse_size("100%").expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier: Tier::select(
            "profile:cargo-target",
            &library,
            Some(&["/home/u/src".to_owned()]),
            1000,
        )
        .unwrap_or_else(|e| panic!("{e}")),
    };
    let (report, _) = ensure(&host, &request);
    assert!(host.unfired_events().is_empty(), "{report:#?}");
    let target = PathBuf::from("/home/u/src/p01/target");
    assert_eq!(acted(&report), std::slice::from_ref(&target), "{report:#?}");
    let kept = unknown(&report);
    assert_eq!(kept.len(), 1, "{report:#?}");
    assert!(
        kept.iter().all(|part| part.starts_with(&target)),
        "{report:#?}"
    );
    assert!(back_offs(&host).is_empty(), "{:?}", pauses(&host));
}

// The new waits' edges (QA review of TASK-364): a signal, the goal, an
// oversleep and a tier's own timeout during them, and a stuck command line
// in a dry run. TASK-340's tests of these use stale scans, which take the
// tier's back-off (`before_check`), not these waits.

/// SIGINT 3 s into the waits (the second back-off): the reclaim and the
/// dry run stop within one 50 ms slice (`interrupted`), nothing deleted.
#[test]
fn a_signal_during_a_busy_wait_stops_the_run_promptly() {
    let events = format!(
        "{}  - {{at: {{elapsed: 3000ms}}, signal: int}}\n",
        storm(400)
    );
    let timeout = Some(Duration::from_secs(600));
    let host = model(1, 0, &events);
    let started = host.now();
    let (report, _) = ensure(&host, &request("profile:probe", "100%", timeout));
    let took = host.now().saturating_since(started);
    assert!(
        matches!(report.status, Status::Interrupted(_)),
        "{report:#?}"
    );
    assert!(acted(&report).is_empty(), "{report:#?}");
    assert!(exists(&host, &candidate(1)));
    assert!(took <= Duration::from_millis(3100), "{took:?}");

    let host = model(1, 0, &events);
    let started = host.now();
    let (report, _) = dry_run(&host, &request("profile:probe", "100%", timeout));
    let took = host.now().saturating_since(started);
    assert!(
        matches!(report.status, Status::Interrupted(_)),
        "{report:#?}"
    );
    assert!(took <= Duration::from_millis(3100), "{took:?}");
}

/// Someone else frees the goal 1 s into the waits (their 3 GiB `big`
/// goes): the space is sampled after each back-off, so the tier ends at
/// the goal after the first (2 s), deleting nothing, also in a storm that
/// would outlast the deadline. Before the QA review the waits sampled only
/// once the host settled, holding the lock to the deadline after the
/// space was there.
#[test]
fn space_that_comes_back_during_a_busy_wait_ends_it_at_the_next_back_off() {
    for (listings, timeout) in [(64, 600), (400, 60)] {
        let host = model(
            1,
            0,
            &format!(
                "{}  - {{at: {{elapsed: 1000ms}}, remove: /tmp/big}}\n",
                storm(listings)
            ),
        );
        let started = host.now();
        let (report, _) = ensure(
            &host,
            &request("profile:probe", "2G", Some(Duration::from_secs(timeout))),
        );
        let took = host.now().saturating_since(started);
        assert!(acted(&report).is_empty(), "{listings}: {report:#?}");
        assert!(exists(&host, &candidate(1)), "{listings}");
        assert_eq!(report.status, Status::Met, "{listings}: {report:#?}");
        assert_eq!(
            back_offs(&host),
            [Duration::from_secs(2)],
            "{listings}: {:?}",
            pauses(&host)
        );
        assert!(took < Duration::from_secs(3), "{listings}: {took:?}");
    }
}

/// The clock jumps 5 s during the last back-off of a storm that outlasts
/// a 60 s deadline: the waits stop at once and the crossing is one
/// `overruns` entry, `busy back-off` (~3 s), not also one for the reclaim
/// that ended before the waits.
#[test]
fn an_oversleep_during_a_busy_wait_is_one_overrun() {
    let host = model(
        1,
        0,
        &format!("{}  - {{at: {{elapsed: 58s}}, advance: 5s}}\n", storm(400)),
    );
    let (report, _) = ensure(
        &host,
        &request("profile:probe", "100%", Some(Duration::from_secs(60))),
    );
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    assert_eq!(report.overruns.len(), 1, "{:#?}", report.overruns);
    let overrun = &report.overruns[0];
    assert_eq!(overrun.operation, "busy back-off", "{overrun:?}");
    assert!((2900..=3100).contains(&overrun.over_ms), "{overrun:?}");
}

/// A configured tier with its own `timeout` (40 s) whose candidate waits
/// for a busy host until it: the tier is excluded with an error naming
/// the busy host, and the next tier runs once the storm is over.
#[test]
fn a_tier_timeout_during_a_busy_wait_names_the_busy_host() {
    let policy = format!(
        "version: 1\nprofiles:{PROFILES}groups:\n  - name: g\n    tiers:\n      - name: first\n        \
         select: profile:probe\n        timeout: 40s\n      - name: second\n        select: \
         profile:other\n"
    );
    let config = diskgc_core::config::parse(
        std::path::Path::new("/etc/diskgc/t.yaml"),
        policy.as_bytes(),
    )
    .unwrap_or_else(|e| panic!("{e}\n{policy}"));
    let mut tiers = Vec::new();
    let mut timeouts = Vec::new();
    for group in &config.groups {
        for setting in &group.tiers {
            tiers.push(
                Tier::configured(&group.name, setting, &config.library, 1000)
                    .unwrap_or_else(|e| panic!("{e}")),
            );
            timeouts.push(setting.timeout);
        }
    }
    // Ten scans of 16 growing listings: the storm outlasts the first
    // tier's 40 s of waits (2 + 4 + 8 + 16 + 10 s), then ends.
    let others = "  /tmp/o-1: {.: {mtime: 2026-10-01T00:00:00Z}}\n";
    let host = model_with(1, 0, others, &storm(160));
    let mut request = request("profile:probe", "100%", Some(Duration::from_secs(600)));
    request.tier = tiers[0].clone();
    let mut lines = Vec::new();
    let report = executor::ensure_grouped(&host, &request, &tiers, &timeouts, &[1000], &mut |l| {
        lines.push(l.to_owned())
    });
    eprintln!("log:\n  {}", lines.join("\n  "));
    let error = report
        .errors
        .iter()
        .find(|e| e.tier.as_deref() == Some("first"))
        .unwrap_or_else(|| panic!("{report:#?}"));
    assert!(error.message.contains("too busy"), "{error:?}");
    assert!(exists(&host, &candidate(1)));
    assert_eq!(acted(&report), [PathBuf::from("/tmp/o-1")], "{report:#?}");
}

/// A command line that never holds still, in a dry run: the plan waits
/// four back-offs ([`STUCK_AFTER`]), then plans both projects `in_use`
/// (an unread command line keeps every project), as the reclaim keeps
/// them.
#[test]
fn a_stuck_command_line_keeps_every_project_in_a_dry_run() {
    let host = projects_model("unsettled", "");
    let (report, _) = dry_run(&host, &projects_request(Some(Duration::from_secs(600))));
    assert_eq!(
        plan_rows(&report),
        [
            (node_modules("p"), Some(Reason::InUse)),
            (node_modules("q"), Some(Reason::InUse))
        ],
        "{report:#?}"
    );
    assert_eq!(back_offs(&host).len(), 4, "{:?}", pauses(&host));
}
