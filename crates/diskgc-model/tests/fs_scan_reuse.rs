//! TASK-203: one process scan serves consecutive `fs` candidates of a
//! `delete` while each deletion starts on it younger than the Freshness
//! bound (one second; docs/safety.md, In use: Freshness), as TASK-173 did
//! for the parts of a Cargo target. Never a deletion on an older scan; a
//! new scan as soon as the kept one could not serve the next deletion; the
//! TASK-144 attempts unchanged (a kept scan's expiry spends none).
//!
//! The model's clock moves only by the events below (after a scan's
//! `own_pid`, after an `unlinkat`) and by retry pauses, so each scan's start
//! and each candidate's deletion start can be read off the trace.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::host;
use diskgc_core::engine::Reason;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::report::Report;
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;

const PROFILES: &str = r#"
- name: pytest-of-dir
  find: {roots: [/tmp], paths: ["pytest-of-*"], type: dir}
  action: delete
  examples: {match: [/tmp/pytest-of-mark]}
- name: pytest-of-cleaner
  find: {roots: [/tmp], paths: ["pytest-of-*"], type: dir}
  action: {command: [rm, -rf, "{path}"]}
  examples: {match: [/tmp/pytest-of-mark]}
"#;

const OLD: &str = "2026-10-01T00:00:00Z";

/// `/tmp` (tmpfs) holding `candidates` old directories
/// `/tmp/pytest-of-u<N>`, each with one file `f` (two unlinks per
/// candidate); a scan's `own_pid` call `k` (from 1) is followed by
/// `scan_ms[k-1]` of model time, the `j`-th unlink by `unlink_ms[j-1]`.
fn model(candidates: usize, scan_ms: &[u64], unlink_ms: &[u64]) -> ModelHost {
    model_with_checks(candidates, scan_ms, unlink_ms, &[])
}

/// [`model`], and the `i`-th mount table read followed by `check_ms[i-1]`:
/// the reclaim reads it before the lock, under it, and after the scan
/// between a candidate's clear and its deletion (the final checks), so
/// time can pass there too.
fn model_with_checks(
    candidates: usize,
    scan_ms: &[u64],
    unlink_ms: &[u64],
    check_ms: &[u64],
) -> ModelHost {
    let mut tree = String::new();
    for n in 0..candidates {
        tree.push_str(&format!(
            "  /tmp/pytest-of-u{n}: {{.: {{mtime: {OLD}}}, f: {{size: 1M, mtime: {OLD}}}}}\n"
        ));
    }
    let mut events = String::new();
    for (k, ms) in scan_ms.iter().enumerate().filter(|(_, ms)| **ms > 0) {
        events.push_str(&format!(
            "  - {{at: {{op: own_pid, nth: {}, when: after}}, advance: {ms}ms}}\n",
            k + 1
        ));
    }
    for (j, ms) in unlink_ms.iter().enumerate().filter(|(_, ms)| **ms > 0) {
        events.push_str(&format!(
            "  - {{at: {{op: unlinkat, nth: {}, when: after}}, advance: {ms}ms}}\n",
            j + 1
        ));
    }
    for (i, ms) in check_ms.iter().enumerate().filter(|(_, ms)| **ms > 0) {
        events.push_str(&format!(
            "  - {{at: {{op: mountinfo, nth: {}, when: after}}, advance: {ms}ms}}\n",
            i + 1
        ));
    }
    let events = if events.is_empty() {
        String::new()
    } else {
        format!("events:\n{events}")
    };
    host(&format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, other_used: 500M, parent: 1}}
tree:
  /usr/bin/tool: {{}}
  /usr/bin/rm: {{mode: "0755"}}
  /run/diskgc/lock: {{uid: 0, gid: 0}}
{tree}processes:
  - {{pid: 7, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{{path: /usr/bin/tool}}], environ: [PATH=/usr/bin]}}
{events}"#
    ))
}

/// `ensure --select SELECT --min-free 100% /tmp` (never met: every
/// candidate is taken).
fn ensure(host: &ModelHost, select: &str) -> Report {
    let specs = parse_profiles(PROFILES).unwrap_or_else(|e| panic!("{e}"));
    let library = Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"));
    let request = Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size("100%").expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier: Tier::select(select, &library, None, 1000).unwrap_or_else(|e| panic!("{e}")),
    };
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let report = executor::ensure(host, &request, &[uid], &mut |l| lines.push(l.to_owned()));
    eprintln!("log:\n  {}", lines.join("\n  "));
    report
}

/// The model's clock as a `now` call reported it (`Duration`'s `Debug`;
/// the model starts at an hour of uptime, so always in seconds).
fn clock_of(result: &str) -> Duration {
    let secs = result
        .strip_suffix('s')
        .filter(|s| !s.ends_with(['m', 'n', 'µ']))
        .unwrap_or_else(|| panic!("not a clock in seconds: {result:?}"));
    Duration::from_secs_f64(secs.parse().unwrap_or_else(|e| panic!("{result:?}: {e}")))
}

/// Each scan's start (the last clock read before its `own_pid`) and each
/// candidate's deletion start (the last clock read before its first
/// unlink: `delete_cleared`'s or the engine's deadline read right after
/// it, with nothing between that moves the clock), with trace positions.
struct Timeline {
    scans: Vec<(usize, Duration)>,
    deletions: Vec<(usize, String, Duration)>,
}

fn timeline(host: &ModelHost) -> Timeline {
    let (mut scans, mut deletions) = (Vec::new(), Vec::<(usize, String, Duration)>::new());
    let mut clock = None;
    for (at, call) in host.trace().into_iter().enumerate() {
        match call.op {
            Op::Now => clock = Some(clock_of(&call.result)),
            Op::OwnPid => scans.push((at, clock.expect("a scan reads the clock first"))),
            Op::Unlinkat if call.result == "ok" => {
                let path = call.path.expect("unlinkat has a path");
                let candidate = path
                    .strip_prefix("/tmp")
                    .expect("below /tmp")
                    .components()
                    .next()
                    .expect("a name")
                    .as_os_str()
                    .to_string_lossy()
                    .into_owned();
                if deletions.iter().all(|(_, c, _)| *c != candidate) {
                    deletions.push((at, candidate, clock.expect("a delete reads the clock")));
                }
            }
            _ => {}
        }
    }
    Timeline { scans, deletions }
}

/// The scans made before the last deletion started (the tier's closing
/// scan for files held open after deletion follows its own rule).
fn scans_serving(t: &Timeline) -> Vec<(usize, Duration)> {
    let last = t.deletions.last().map_or(0, |(at, _, _)| *at);
    t.scans
        .iter()
        .copied()
        .filter(|(at, _)| *at < last)
        .collect()
}

/// TASK-203: a 600 ms scan and a 200 ms walk-and-delete per candidate (as
/// on the laptop: scans of 0.3-1 s, sessions of 0.1-0.3 s). One scan
/// serves two candidates, both deleted on it younger than 1 s: 3 scans for
/// 6 candidates. With the 500 ms reuse window before TASK-203 every
/// candidate made its own: 6 scans.
#[test]
fn a_600_ms_scan_serves_every_candidate_whose_delete_starts_within_a_second() {
    let host = model(6, &[600; 8], &[100; 12]);
    let report = ensure(&host, "profile:pytest-of-dir");
    assert_eq!(report.actions.len(), 6, "{report:#?}");
    let t = timeline(&host);
    assert_eq!(scans_serving(&t).len(), 3, "{:?}", t.scans);
    assert_eq!(host.trace_count(Op::Sleep), 0, "no attempt spent");
}

/// A command action keeps the 500 ms window: its program may act on
/// anything in the candidate, so the scan that cleared it stays as recent
/// as before. The same timings as above make a scan per candidate.
#[test]
fn a_command_action_keeps_the_half_second_reuse_window() {
    let host = model(3, &[600; 8], &[]);
    let report = ensure(&host, "profile:pytest-of-cleaner");
    assert_eq!(report.actions.len(), 3, "{report:#?}");
    let commands = host.trace_count(Op::RunCommand);
    assert_eq!(commands, 3);
    let scans_before_last_command = {
        let trace = host.trace();
        let last = trace
            .iter()
            .rposition(|c| c.op == Op::RunCommand)
            .expect("a command ran");
        trace[..last].iter().filter(|c| c.op == Op::OwnPid).count()
    };
    assert_eq!(scans_before_last_command, 3, "one scan per candidate");
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

    /// Whatever the scans and deletions take: no candidate's deletion
    /// starts on a scan one second old or older; when every scan takes
    /// under a second, every candidate is deleted without spending an
    /// attempt, and a new scan is made only once the kept one could not
    /// serve the next deletion (it is a second old); with slower scans
    /// every candidate ends deleted or `in_use_unknown`, nothing else.
    #[test]
    fn no_fs_delete_starts_on_a_scan_past_the_bound(
        candidates in 1_usize..7,
        scan_ms in proptest::collection::vec(0_u64..1_300, 1..14),
        unlink_ms in proptest::collection::vec(0_u64..700, 0..14),
        check_ms in proptest::collection::vec(
            proptest::prop_oneof![3 => proptest::strategy::Just(0_u64), 1 => 0_u64..600],
            0..40,
        ),
    ) {
        let host = model_with_checks(candidates, &scan_ms, &unlink_ms, &check_ms);
        let report = ensure(&host, "profile:pytest-of-dir");
        let t = timeline(&host);
        for (at, candidate, started) in &t.deletions {
            let (_, scan) = t
                .scans
                .iter()
                .rev()
                .find(|(s, _)| s < at)
                .expect("a scan before every deletion");
            proptest::prop_assert!(
                started.saturating_sub(*scan) < Duration::from_secs(1),
                "{} started {:?} after its scan", candidate, started.saturating_sub(*scan)
            );
        }
        // A scan is replaced only once it could not serve the next
        // deletion: it is a second old (its reuse ended, or a stale
        // attempt waited for a new one).
        for pair in scans_serving(&t).windows(2) {
            let age = pair[1].1.saturating_sub(pair[0].1);
            proptest::prop_assert!(
                age >= Duration::from_secs(1),
                "a new scan though the kept one was {:?} old", age
            );
        }
        let made = t.scans.len();
        let all_fast = scan_ms.iter().take(made).all(|ms| *ms < 1_000)
            && check_ms.iter().all(|ms| *ms == 0);
        if all_fast {
            proptest::prop_assert_eq!(report.actions.len(), candidates, "{:#?}", report);
            proptest::prop_assert_eq!(host.trace_count(Op::Sleep), 0);
        } else {
            for skip in &report.skipped {
                proptest::prop_assert_eq!(skip.reason, Reason::InUseUnknown, "{:#?}", report);
            }
            let skipped: u64 = report.skipped.iter().map(|s| s.count).sum();
            proptest::prop_assert_eq!(
                report.actions.len() as u64 + skipped,
                candidates as u64,
                "{:#?}", report
            );
        }
        proptest::prop_assert!(report.errors.is_empty(), "{:#?}", report);
    }
}
