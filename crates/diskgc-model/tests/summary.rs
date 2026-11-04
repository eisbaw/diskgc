//! The per-term summary (TASK-324; docs/reference.md, Output: `summary`)
//! through the real executor on ModelHost: a dry run, a dry run the
//! deadline cuts, and a reclaim. Each pins the whole human result (golden
//! text: the human format is not a contract, so a change of wording is a
//! change of these strings) and the JSON `summary` counts.
//!
//! The fixture is a union of three user profiles: `spool` has a source
//! floor (`min_idle_for: 3d`), so its candidates are kept for reasons
//! decided after the eligibility walk (`not_idle`, `future_time`, `in_use`:
//! sized); `cache` has one kept by a keep marker, found during the walk
//! (unsized); `nothing-here` finds nothing.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::host;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::report::{Report, Status};
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use serde_json::{Value, json};

const PROFILES: &str = r#"
- name: spool
  find: {roots: [/var/tmp], paths: ["spool-*"], type: any}
  min_idle_for: 3d
  action: delete
  examples: {match: [/var/tmp/spool-1]}
- name: cache
  find: {roots: [/var/tmp], paths: ["cache-*"], type: dir}
  action: delete
  examples: {match: [/var/tmp/cache-1]}
- name: nothing-here
  find: {roots: [/var/tmp], paths: ["absent-*"], type: dir}
  action: delete
  examples: {match: [/var/tmp/absent-1]}
"#;

/// `/` (ext4, 100 GiB) holding `/var/tmp`; the clock is 2026-10-15; pid 42
/// holds `spool-busy` open. Five `spool-*` candidates and two `cache-*`.
fn fixture(events: &str) -> ModelHost {
    host(&format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G, other_used: 99G}}
tree:
  /usr/bin/tool: {{}}
  /var/tmp/spool-old: {{.: {{mtime: 2026-10-01T00:00:00Z}}, f: {{size: 3M, mtime: 2026-10-01T00:00:00Z}}}}
  /var/tmp/spool-older: {{size: 2M, mtime: 2026-09-01T00:00:00Z}}
  /var/tmp/spool-fresh: {{size: 1M, mtime: 2026-10-14T00:00:00Z}}
  /var/tmp/spool-busy: {{size: 4M, mtime: 2026-10-01T00:00:00Z}}
  /var/tmp/spool-future: {{size: 5M, mtime: 2026-10-16T00:00:00Z}}
  /var/tmp/cache-a: {{.: {{mtime: 2026-10-01T00:00:00Z}}, f: {{size: 6M, mtime: 2026-10-01T00:00:00Z}}}}
  /var/tmp/cache-kept: {{.: {{mtime: 2026-10-01T00:00:00Z}}, .diskgc-keep: {{}}, f: {{size: 7M, mtime: 2026-10-01T00:00:00Z}}}}
processes:
  - {{pid: 42, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{{path: /var/tmp/spool-busy}}], environ: [PATH=/usr/bin]}}
{events}"#
    ))
}

fn request(select: &str, min_free: &str) -> Request {
    let specs = parse_profiles(PROFILES).unwrap_or_else(|e| panic!("{e}"));
    let library = Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"));
    Request {
        path: PathBuf::from("/var/tmp"),
        min_free: parse_size(min_free).expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier: Tier::select(select, &library, None, 1000).unwrap_or_else(|e| panic!("{e}")),
    }
}

fn run(host: &ModelHost, request: &Request, dry_run: bool) -> Report {
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e}"));
    let mut lines = Vec::new();
    let mut log = |l: &str| lines.push(l.to_owned());
    let report = if dry_run {
        executor::dry_run(host, request, &[uid], &mut log)
    } else {
        executor::ensure(host, request, &[uid], &mut log)
    };
    eprintln!("log:\n  {}", lines.join("\n  "));
    report
}

fn summary(report: &Report) -> Value {
    let json: Value = serde_json::from_str(&report.to_json()).expect("JSON");
    json["summary"].clone()
}

const SELECT: &str = "profile:spool || profile:cache || profile:nothing-here";

/// A dry run's blocks: one per term of the union, a term that found nothing
/// included; kept counted and sized by reason: sized where the walk was
/// complete before the refusal, `unsized` where it stopped first (the keep
/// marker); `found` is their sum.
#[test]
fn a_dry_run_explains_each_term_with_sizes_by_reason() {
    let host = fixture("");
    // 996 MiB available, 1000 MiB asked for: spool-older (2 MiB) and
    // spool-old (3 MiB) cover the 4 MiB needed; cache-a is not needed.
    let report = run(&host, &request(SELECT, "1000M"), true);
    assert_eq!(report.status, Status::DryRun, "{report:#?}");
    assert_eq!(
        report.summary().unwrap(),
        [
            "dry run: 996.0 MiB available on / (min-free 1000.0 MiB, target-free 1000.0 MiB); would reclaim 2 of 3 eligible, est. 5.0 MiB of 4.0 MiB needed: the goal is reachable by estimate; 4 kept (took 0 ms):",
            "  profile spool: 5 found · 2 eligible est. 5.0 MiB (would reclaim 2) · kept 1 in_use est. 4.0 MiB, 1 not_idle est. 1.0 MiB, 1 future_time est. 5.0 MiB",
            "  profile cache: 2 found · 1 eligible est. 6.0 MiB (would reclaim 0) · kept 1 keep marker unsized",
            "  profile nothing-here: 0 found · 0 eligible",
            "  would reclaim /var/tmp/spool-older: est. 2.0 MiB",
            "  would reclaim /var/tmp/spool-old: est. 3.0 MiB",
            "  eligible, not needed /var/tmp/cache-a: est. 6.0 MiB",
            "  kept /var/tmp/spool-busy: in_use, used by pid 42 (tool)",
            "  kept /var/tmp/spool-fresh: not_idle, newest change 1 d ago; idle_for 3d required",
            "  kept /var/tmp/spool-future: future_time, its newest change cannot be told (its last change is in the future): an unknown age is never idle",
            "  kept /var/tmp/cache-kept: keep marker, inside the candidate",
            "  operations: spool: delete; cache: delete",
        ]
        .join("\n")
    );
    let summary = summary(&report);
    assert_eq!(
        summary[0],
        json!({
            "tier": "select", "profile": "spool", "found": 5,
            // spool-old's directory itself is 4 KiB.
            "eligible": {"count": 2, "estimated_bytes": (5 << 20) + 4096, "unsized": 0},
            "would_reclaim": {"count": 2, "estimated_bytes": (5 << 20) + 4096, "unsized": 0},
            "reclaimed": null,
            "kept": [
                {"reason": "in_use", "count": 1, "estimated_bytes": 4 << 20, "unsized": 0},
                {"reason": "not_idle", "count": 1, "estimated_bytes": 1 << 20, "unsized": 0},
                {"reason": "future_time", "count": 1, "estimated_bytes": 5 << 20, "unsized": 0},
            ],
            "not_judged": 0,
        }),
        "{summary:#}"
    );
    assert_eq!(
        summary[1]["kept"],
        json!([{"reason": "keep_marker", "count": 1, "estimated_bytes": 0, "unsized": 1}])
    );
    assert_eq!(summary[2]["found"], 0);
}

/// TASK-227's cut, counted: the deadline passes while a candidate's walk
/// lists it, so it and every later one are found but not judged, and
/// `found` still adds up to what discovery listed. Both planners: a union
/// (each unjudged candidate counted for the first term that found it) and
/// a single source.
#[test]
fn a_dry_run_cut_by_the_deadline_counts_what_it_did_not_judge() {
    for (select, cut_at, listed) in [
        ("profile:cache || profile:spool", "/var/tmp/cache-a", 7),
        ("profile:spool", "/var/tmp/spool-old", 5),
    ] {
        let host = fixture(&format!(
            "events:\n  - {{at: {{op: read_dir, path: {cut_at}, when: after}}, advance: 700s}}\n"
        ));
        let report = run(&host, &request(select, "1028M"), true);
        assert!(
            host.unfired_events().is_empty(),
            "{select}: the clock moved"
        );
        assert_eq!(report.status, Status::Deadline, "{select}: {report:#?}");
        let summary = summary(&report);
        let sum = |field: &str| -> u64 {
            summary
                .as_array()
                .unwrap()
                .iter()
                .map(|block| block[field].as_u64().expect("a dry run's count"))
                .sum()
        };
        assert_eq!(sum("found"), listed, "{select}: {summary:#}");
        let judged = report.plan.iter().filter(|p| p.candidate.is_some()).count() as u64;
        assert!(sum("not_judged") > 0, "{select}: {summary:#}");
        assert_eq!(judged + sum("not_judged"), listed, "{select}: {summary:#}");
        let text = report.summary().unwrap();
        assert!(text.contains(" not judged (deadline)"), "{select}: {text}");
    }
}

/// A reclaim's blocks: what each term reclaimed (count and observed
/// change), and what it kept by reason, sized where a walk measured the
/// candidate first (its tally, or the refusing check's own walk).
#[test]
fn a_reclaim_explains_each_term_with_what_it_reclaimed_and_kept() {
    let host = fixture("");
    let report = run(&host, &request(SELECT, "99G"), false);
    assert_eq!(report.status, Status::Exhausted, "{report:#?}");
    assert_eq!(
        report.summary().unwrap(),
        [
            "nothing more could be reclaimed: 1007.0 MiB available on / (min-free 99.0 GiB, target-free 99.0 GiB); 3 reclaimed (observed change +11.0 MiB), 4 kept, 0 error(s); took 0 ms",
            "  profile spool: reclaimed 2 (observed change +5.0 MiB) · kept 1 not_idle est. 1.0 MiB, 1 future_time est. 5.0 MiB, 1 in_use est. 4.0 MiB",
            "  profile cache: reclaimed 1 (observed change +6.0 MiB) · kept 1 keep marker unsized",
            "  reclaimed /var/tmp/spool-older: +2.0 MiB",
            "  reclaimed /var/tmp/spool-old: +3.0 MiB",
            "  reclaimed /var/tmp/cache-a: +6.0 MiB",
            "  kept /var/tmp/spool-fresh: not_idle, newest change 1 d ago; idle_for 3d required",
            "  kept /var/tmp/spool-future: future_time, its newest change cannot be told (its last change is in the future): an unknown age is never idle",
            "  kept /var/tmp/cache-kept: keep marker, inside the candidate",
            "  kept /var/tmp/spool-busy: in_use, used by pid 42 (tool)",
        ]
        .join("\n")
    );
    let summary = summary(&report);
    assert_eq!(
        summary[0]["reclaimed"],
        json!({"count": 2, "avail_delta_bytes": (5 << 20) + 4096})
    );
    for field in ["found", "eligible", "would_reclaim", "not_judged"] {
        assert!(summary[0][field].is_null(), "{field}: {summary:#}");
    }
}
