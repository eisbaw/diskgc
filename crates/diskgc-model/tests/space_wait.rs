//! The wait for freed space after an action (TASK-334;
//! docs/architecture.md, Measuring space): a filesystem may show the
//! blocks an unlink freed only later (the model's `free_lag`). In a tier
//! that decides by the goal alone (oldest first, diskgc's own deletion),
//! diskgc re-samples for at most 2 s only when what it still expects to
//! show could reach the goal; otherwise the next candidate is taken at
//! once, which the wait could not have changed. A tier that reads the
//! remaining need (best-fit, commands, Nix) always waits, first for bytes
//! earlier tiers left unseen, and the final sample waits once.
//!
//! The fixture's numbers are exact: `/tmp` is 1 GiB with 224 MiB
//! available, and each candidate is one directory (4 KiB) holding one file
//! of 100 MiB - 4 KiB, so it frees exactly 100 MiB, its tally. Model calls
//! take no time; only `sleep` moves the clock, so freed space shows only
//! after a wait.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::config::Order;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::report::Report;
use diskgc_core::snapshot::RunIndex;
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;
use proptest::prelude::*;

const MIB: u64 = 1 << 20;
/// What `/tmp` has available before anything is deleted.
const AVAIL_MIB: u64 = 224;
/// The re-sampling step (`executor::RESAMPLE_EVERY`), as the model's
/// `sleep` call records it.
const STEP: &str = "250ms";

const PROFILES: &str = r#"
- name: tmp-a
  find: {roots: [/tmp], paths: ["a*"], type: dir}
  action: delete
  examples: {match: [/tmp/a0]}
- name: tmp-b
  find: {roots: [/tmp], paths: ["b*"], type: dir}
  action: delete
  examples: {match: [/tmp/b0]}
- name: tmp-b-cleaner
  find: {roots: [/tmp], paths: ["b*"], type: dir}
  action: {command: [rm, -rf, "{path}"]}
  examples: {match: [/tmp/b0]}
"#;

/// `/tmp` with `names` as candidates, oldest first, each of the given
/// size in MiB (directory included); `lag`: its `free_lag`.
fn yaml(candidates: &[(&str, u64)], lag: Option<&str>) -> String {
    let total: u64 = candidates.iter().map(|(_, mib)| mib).sum();
    // 1 GiB = other_used + the root directory + the candidates + avail.
    let other_used = 1024 * MIB - 4096 - total * MIB - AVAIL_MIB * MIB;
    let lag = lag.map_or(String::new(), |l| format!(", free_lag: {l}"));
    let mut tree = String::new();
    for (day, (name, mib)) in candidates.iter().enumerate() {
        let day = day + 1;
        tree.push_str(&format!(
            "  /tmp/{name}:\n    .: {{mtime: 2026-10-{day:02}T00:00:00Z}}\n    f: {{size: {}, \
             mtime: 2026-10-{day:02}T00:00:00Z}}\n",
            mib * MIB - 4096
        ));
    }
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, other_used: {other_used}, parent: 1{lag}}}
state_dir: /home/u/.local/state/diskgc
tree:
  /run/diskgc/lock: {{uid: 0, gid: 0}}
  /usr/bin/rm: {{mode: "0755"}}
  /home/u: {{.: {{mode: "0700"}}}}
{tree}"#
    )
}

fn tier(select: &str, name: &str) -> Tier {
    let specs = parse_profiles(PROFILES).unwrap_or_else(|e| panic!("{e}"));
    let library = Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"));
    let mut tier = Tier::select(select, &library, None, 1000).unwrap_or_else(|e| panic!("{e}"));
    tier.name = name.to_owned();
    tier
}

fn request(tier: Tier, min_free: &str, target_free: Option<&str>) -> Request {
    Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size(min_free).expect("a size"),
        target_free: target_free.map(|t| parse_size(t).expect("a size")),
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier,
    }
}

/// One reclaim of `tiers` in order (a configuration's groups); its report.
fn run(host: &ModelHost, request: &Request, tiers: &[Tier]) -> Report {
    let mut lines = Vec::new();
    let timeouts = vec![Duration::ZERO; tiers.len()];
    let report = executor::ensure_grouped_with_index(
        host,
        request,
        tiers,
        &timeouts,
        &[1000],
        &mut RunIndex::snapshot(),
        &mut |l| lines.push(l.to_owned()),
    );
    eprintln!("log:\n  {}", lines.join("\n  "));
    report
}

fn deleted(report: &Report) -> Vec<PathBuf> {
    report.actions.iter().map(|a| a.candidate.clone()).collect()
}

fn paths(names: &[&str]) -> Vec<PathBuf> {
    names.iter().map(|n| Path::new("/tmp").join(n)).collect()
}

/// The re-sampling waits of the run (no other wait of this fixture is
/// 250 ms: its in-use scans are fresh).
fn waits(host: &ModelHost) -> usize {
    host.trace()
        .iter()
        .filter(|c| c.op == Op::Sleep && c.result == STEP)
        .count()
}

/// The re-sampling waits before the run's last deletion: those that held
/// up the next candidate (the final sample's come after it).
fn waits_between_deletions(host: &ModelHost) -> usize {
    let trace = host.trace();
    let last_unlink = trace
        .iter()
        .rposition(|c| c.op == Op::Unlinkat)
        .expect("something was deleted");
    trace[..last_unlink]
        .iter()
        .filter(|c| c.op == Op::Sleep && c.result == STEP)
        .count()
}

/// At most one re-sampling window of 250 ms steps.
const ONE_WINDOW: usize = 8;

/// A best-fit tier of `select`.
fn best_fit(select: &str, name: &str) -> Tier {
    let mut tier = tier(select, name);
    tier.order = Order::BestFit;
    tier
}

/// (a) The goal is out of reach even with every lagging byte: no candidate
/// waits for the one before it, the same candidates go (all of them) with
/// the same exit, and the final sample waits once, so the reported space
/// is the settled one.
#[test]
fn an_unreachable_goal_takes_the_next_candidate_without_waiting() {
    let host = host(&yaml(
        &[("a1", 100), ("a2", 100), ("a3", 100)],
        Some("1500ms"),
    ));
    let report = run(
        &host,
        &request(tier("profile:tmp-a", "a"), "1000M", None),
        &[tier("profile:tmp-a", "a")],
    );
    assert_eq!(deleted(&report), paths(&["a1", "a2", "a3"]), "{report:#?}");
    assert_eq!(report.exit_code(), 1, "{report:#?}");
    assert_eq!(waits_between_deletions(&host), 0, "{:#?}", host.trace());
    assert!(waits(&host) <= ONE_WINDOW, "{:#?}", host.trace());
    assert_eq!(report.avail_after_bytes, Some(524 * MIB), "{report:#?}");
}

/// (b) Only a1's and a2's lagging bytes together reach the goal (400 MiB
/// from 224): a1's wait is skipped (324 MiB could not reach it), a2's is
/// not (424 MiB counts a1's carried bytes), and a3 is kept.
#[test]
fn a_goal_reachable_through_carried_bytes_waits_and_takes_no_extra_candidate() {
    let host = host(&yaml(
        &[("a1", 100), ("a2", 100), ("a3", 100)],
        Some("1500ms"),
    ));
    let report = run(
        &host,
        &request(tier("profile:tmp-a", "a"), "400M", None),
        &[tier("profile:tmp-a", "a")],
    );
    assert_eq!(deleted(&report), paths(&["a1", "a2"]), "{report:#?}");
    assert_eq!(report.exit_code(), 0, "{report:#?}");
    assert!(report.avail_after_bytes >= Some(424 * MIB), "{report:#?}");
    assert!(waits(&host) > 0, "{:#?}", host.trace());
}

/// (c) The carried bytes cross a tier boundary: tier a's a1 skips its
/// wait, the boundary sample still misses its 100 MiB, and tier b's b1
/// waits for both; b2 is kept.
#[test]
fn carried_bytes_cross_the_tier_boundary() {
    let host = host(&yaml(
        &[("a1", 100), ("b1", 100), ("b2", 100)],
        Some("1500ms"),
    ));
    let first = tier("profile:tmp-a", "a");
    let report = run(
        &host,
        &request(first.clone(), "400M", None),
        &[first, tier("profile:tmp-b", "b")],
    );
    assert_eq!(deleted(&report), paths(&["a1", "b1"]), "{report:#?}");
    assert_eq!(report.exit_code(), 0, "{report:#?}");
}

/// (d) `--min-free` is reachable, `--target-free` is not: every wait
/// between candidates is skipped, the walk ends, and the final sample waits
/// for the lagging bytes before it decides the exit code (0, as with every
/// wait taken).
#[test]
fn the_final_sample_waits_when_lagging_bytes_could_meet_min_free() {
    let host = host(&yaml(
        &[("a1", 100), ("a2", 100), ("a3", 100)],
        Some("1500ms"),
    ));
    let report = run(
        &host,
        &request(tier("profile:tmp-a", "a"), "500M", Some("600M")),
        &[tier("profile:tmp-a", "a")],
    );
    assert_eq!(deleted(&report), paths(&["a1", "a2", "a3"]), "{report:#?}");
    assert_eq!(waits_between_deletions(&host), 0, "{:#?}", host.trace());
    assert_eq!(report.avail_after_bytes, Some(524 * MIB), "{report:#?}");
    assert_eq!(report.exit_code(), 0, "{report:#?}");
}

/// (e) A best-fit tier reads the remaining need at every pop, so it never
/// skips a wait (architect review): from 224 MiB to 324, nothing covers 100
/// MiB, so b90 goes; once its bytes show, 10 MiB is the need and b45 covers
/// it. Without the wait the need would still read 100 and take b60.
#[test]
fn a_best_fit_tier_waits_so_that_it_pops_by_the_need_left() {
    let host = host(&yaml(
        &[("b90", 90), ("b60", 60), ("b45", 45)],
        Some("1500ms"),
    ));
    let only = best_fit("profile:tmp-b", "b");
    let report = run(&host, &request(only.clone(), "324M", None), &[only]);
    assert_eq!(deleted(&report), paths(&["b90", "b45"]), "{report:#?}");
    assert_eq!(report.exit_code(), 0, "{report:#?}");
}

/// (f) A tier that decides by the goal alone skipped a1's wait (324 MiB
/// could not reach 354); before the next tier, whose command reads the
/// remaining need, the run waits for those bytes, so the command is told
/// 30 MiB (354 - 324), as when a1 had waited, not 130.
#[test]
fn a_tier_that_reads_the_need_first_waits_for_bytes_earlier_tiers_freed() {
    let host = host(&yaml(&[("a1", 100), ("b1", 40)], Some("1500ms")));
    let first = tier("profile:tmp-a", "a");
    let report = run(
        &host,
        &request(first.clone(), "354M", None),
        &[first, tier("profile:tmp-b-cleaner", "b")],
    );
    let needs: Vec<String> = host
        .trace()
        .iter()
        .filter_map(|c| c.command.as_ref())
        .filter_map(|run| {
            run.environment
                .iter()
                .find(|(key, _)| key == "DISKGC_NEED_BYTES")
                .map(|(_, value)| value.to_string_lossy().into_owned())
        })
        .collect();
    assert_eq!(needs, vec![(30 * MIB).to_string()], "{report:#?}");
}

/// `n` KiB as a size flag.
fn kib(bytes: u64) -> String {
    assert_eq!(bytes % 1024, 0);
    format!("{}K", bytes / 1024)
}

/// A candidate deleted only part-way (re-review): its tally is not what it
/// frees, so it waits as before rather than skip on its full tally. a1
/// holds `e` (90 MiB - 4 KiB, freed) and `f` (10 MiB - 4 KiB, its unlink
/// fails): it frees about 90 of its 100 MiB tally. To 185 MiB above the
/// start, a1's wait shows its 90 MiB, a2's 100 MiB then reach the goal and
/// a3 is kept, as with a wait after every action. Skipping a1's wait on
/// its tally (100 MiB "expected", short of 185) would take a3 as well.
#[test]
fn a_part_way_deletion_waits_as_before() {
    let mut model = yaml(&[("a1", 10), ("a2", 100), ("a3", 100)], Some("1500ms"));
    model.push_str(&format!(
        "  /tmp/a1/e: {{size: {}, mtime: 2026-10-01T00:00:00Z}}\n\
         events:\n  - {{at: {{op: unlinkat, path: /tmp/a1/f, when: before}}, fail: EIO}}\n",
        90 * MIB - 4096
    ));
    let host = host(&model);
    let start = AVAIL_MIB * MIB - (90 * MIB - 4096);
    let report = run(
        &host,
        &request(tier("profile:tmp-a", "a"), &kib(start + 185 * MIB), None),
        &[tier("profile:tmp-a", "a")],
    );
    assert!(host.unfired_events().is_empty(), "{:#?}", host.trace());
    assert_eq!(report.avail_before_bytes, Some(start), "{report:#?}");
    assert_eq!(deleted(&report), paths(&["a1", "a2"]), "{report:#?}");
    assert!(waits_between_deletions(&host) > 0, "{:#?}", host.trace());
}

/// A tally above what a deletion frees costs one window, not one per
/// later action (re-review): a1's 200 MiB file has a second name outside
/// the candidate, so deleting a1 frees 4 KiB. To 250 MiB above the start:
/// a1 skips (200 expected), a2 waits (300 expected) and runs out its window
/// at 100, which resets what the run expects; a3 skips again (200) and a4
/// waits and meets the goal. Without the reset a3 would wait a window too.
#[test]
fn an_estimate_above_what_a_deletion_frees_costs_one_window() {
    let mut model = yaml(
        &[("a1", 200), ("a2", 100), ("a3", 100), ("a4", 100)],
        Some("1500ms"),
    );
    model.push_str("  /tmp/keep-a1-f: {link: /tmp/a1/f}\n");
    let host = host(&model);
    let report = run(
        &host,
        &request(tier("profile:tmp-a", "a"), "474M", None),
        &[tier("profile:tmp-a", "a")],
    );
    assert_eq!(
        deleted(&report),
        paths(&["a1", "a2", "a3", "a4"]),
        "{report:#?}"
    );
    assert_eq!(report.exit_code(), 0, "{report:#?}");
    assert!(
        waits_between_deletions(&host) <= ONE_WINDOW,
        "{:#?}",
        host.trace()
    );
}

fn cases() -> u32 {
    std::env::var("DISKGC_PROPERTY_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(48)
        .min(48)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// With right estimates, no other writer, and every lag within the
    /// 2 s window, the run deletes exactly the shortest oldest-first
    /// prefix whose bytes reach `--target-free` (what waiting at every
    /// candidate, the rule before TASK-334, deletes), and exits 0 exactly
    /// when those bytes reach `--min-free`.
    #[test]
    fn with_right_estimates_and_lag_within_the_window_the_deleted_set_is_the_ideal_prefix(
        sizes in prop::collection::vec(1u64..=120, 1..=5),
        lag_ms in prop::option::of(1u64..=1900),
        min_extra in 1u64..=500,
        target_above_min in prop::option::of(0u64..=300),
    ) {
        let names: Vec<String> = (1..=sizes.len()).map(|i| format!("a{i}")).collect();
        let candidates: Vec<(&str, u64)> =
            names.iter().map(String::as_str).zip(sizes.iter().copied()).collect();
        let lag = lag_ms.map(|ms| format!("{ms}ms"));
        let host = host(&yaml(&candidates, lag.as_deref()));
        let min = AVAIL_MIB + min_extra;
        let target = min + target_above_min.unwrap_or(0);
        let report = run(
            &host,
            &request(
                tier("profile:tmp-a", "a"),
                &format!("{min}M"),
                target_above_min.map(|_| format!("{target}M")).as_deref(),
            ),
            &[tier("profile:tmp-a", "a")],
        );
        let mut avail = AVAIL_MIB;
        let mut ideal = Vec::new();
        for (name, mib) in &candidates {
            if avail >= target {
                break;
            }
            ideal.push(*name);
            avail += mib;
        }
        prop_assert_eq!(deleted(&report), paths(&ideal), "{:#?}", report);
        prop_assert_eq!(report.exit_code(), if avail >= min { 0 } else { 1 }, "{:#?}", report);
        if avail < target {
            prop_assert_eq!(report.avail_after_bytes, Some(avail * MIB), "{:#?}", report);
        }
    }

    /// A best-fit tier (the first, so the tally sizes its candidates and
    /// every pop reads the need) deletes what the rule before TASK-334
    /// deletes, simulated with every lag shown before the next pop: the
    /// smallest candidate covering the need left, else the largest.
    #[test]
    fn a_best_fit_tier_deletes_what_waiting_everywhere_deletes(
        sizes in prop::collection::btree_set(1u64..=120, 1..=5),
        lag_ms in prop::option::of(1u64..=1900),
        min_extra in 1u64..=400,
    ) {
        let sizes: Vec<u64> = sizes.into_iter().collect();
        let names: Vec<String> = sizes.iter().map(|mib| format!("b{mib}")).collect();
        let candidates: Vec<(&str, u64)> =
            names.iter().map(String::as_str).zip(sizes.iter().copied()).collect();
        let lag = lag_ms.map(|ms| format!("{ms}ms"));
        let host = host(&yaml(&candidates, lag.as_deref()));
        let target = AVAIL_MIB + min_extra;
        let only = best_fit("profile:tmp-b", "b");
        let report = run(&host, &request(only.clone(), &format!("{target}M"), None), &[only]);
        let mut avail = AVAIL_MIB;
        let mut expected = Vec::new();
        let mut left = sizes.clone();
        while avail < target && !left.is_empty() {
            let need = target - avail;
            let pick = left
                .iter()
                .copied()
                .filter(|mib| *mib >= need)
                .min()
                .unwrap_or_else(|| *left.iter().max().expect("nonempty"));
            left.retain(|mib| *mib != pick);
            expected.push(format!("b{pick}"));
            avail += pick;
        }
        let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
        prop_assert_eq!(deleted(&report), paths(&expected), "{:#?}", report);
        prop_assert_eq!(report.exit_code(), if avail >= target { 0 } else { 1 }, "{:#?}", report);
    }
}
