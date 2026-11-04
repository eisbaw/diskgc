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
- name: neg
  find: {roots: [/var/tmp/negroot], paths: ["*"], type: any}
  action: delete
  examples: {match: [/var/tmp/negroot/a]}
- name: walled
  find: {roots: [/var/tmp/walled], paths: ["*/item-*"], type: any}
  action: delete
  examples: {match: [/var/tmp/walled/x/item-1]}
"#;

/// `/` (ext4, 100 GiB) holding `/var/tmp`; the clock is 2026-10-15; pid 42
/// holds `spool-busy` open. Five `spool-*` candidates and two `cache-*`.
fn fixture(events: &str) -> ModelHost {
    fixture_with("", events)
}

/// [`fixture`] with more tree entries `extra` (indented as tree entries).
fn fixture_with(extra: &str, events: &str) -> ModelHost {
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
{extra}processes:
  - {{pid: 42, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{{path: /var/tmp/spool-busy}}], environ: [PATH=/usr/bin]}}
{events}"#
    ))
}

/// Tree entries for the early-stop tests: `/var/tmp/negroot/a`, the one
/// match of `!profile:neg`, and two Cargo targets below `/var/tmp/src`.
fn early_stop_tree() -> String {
    let target = cargo_profile_dir();
    format!(
        "  /var/tmp/negroot/a: {{.: {{mtime: 2026-10-01T00:00:00Z}}}}\n  \
         /var/tmp/src/p/Cargo.toml: {{}}\n  /var/tmp/src/p/target: {{CACHEDIR.TAG: \
         {{cachedir_tag: true}}, debug: {target}}}\n  /var/tmp/src/q/Cargo.toml: {{}}\n  \
         /var/tmp/src/q/target: {{CACHEDIR.TAG: {{cachedir_tag: true}}, debug: {target}}}\n"
    )
}

/// A Cargo profile directory built 2026-10-01 holding 1 MiB.
fn cargo_profile_dir() -> String {
    let at = "2026-10-01T00:00:00Z";
    format!(
        "{{.: {{mtime: {at}}}, .cargo-lock: {{mtime: {at}}}, .fingerprint: {{.: {{mtime: {at}}}, \
         p-1: {{.: {{mtime: {at}}}, bin-p: {{mtime: {at}}}}}}}, deps: {{.: {{mtime: {at}}}, p-1: \
         {{size: 1M, mtime: {at}}}}}}}"
    )
}

/// [`request`] with `--roots`.
fn request_with_roots(select: &str, roots: &[&str]) -> Request {
    let specs = parse_profiles(PROFILES).unwrap_or_else(|e| panic!("{e}"));
    let library = Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"));
    let roots: Vec<String> = roots.iter().map(|r| (*r).to_owned()).collect();
    Request {
        tier: Tier::select(select, &library, Some(&roots), 1000).unwrap_or_else(|e| panic!("{e}")),
        ..request("profile:spool", "1028M")
    }
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

/// Every block's `field`, summed; a dry run's counts are never null here.
fn sum(summary: &Value, field: &str) -> u64 {
    summary
        .as_array()
        .unwrap()
        .iter()
        .map(|block| {
            block[field]
                .as_u64()
                .unwrap_or_else(|| panic!("{field}: {summary:#}"))
        })
        .sum()
}

/// Events once `trigger` (a directory the first judged candidate's walk
/// lists, after the negated scan) has been listed: the one match of
/// `!profile:neg` goes and a `systemd-private-*` name appears, so the next
/// candidate's negation check cannot prove its "no" from the first
/// enumeration, and the enumeration it makes again is uncertain: the tier
/// fails part-way.
fn neg_recheck_fails(trigger: &str) -> String {
    format!(
        "events:\n  - {{at: {{op: read_dir, path: {trigger}, when: after}}, remove: \
         /var/tmp/negroot/a}}\n  - {{at: {{op: read_dir, path: {trigger}, when: after}}, mkdir: \
         /var/tmp/negroot/systemd-private-x}}\n"
    )
}

/// One early-stop case: what, select, roots, extra tree, events, and the
/// candidates found.
type EarlyStop<'a> = (&'a str, &'a str, &'a [&'a str], &'a str, &'a str, u64);

/// Codex review 1 (TASK-324): a plan that stops early because its negated
/// matches cannot be enumerated (at the start, or again for a candidate),
/// or because a misconfigured root excludes the tier, still counts every
/// candidate it found and did not judge, so `found` adds up.
#[test]
fn an_early_stop_counts_every_candidate_it_did_not_judge() {
    let union = "(profile:spool && !profile:neg) || profile:cache";
    let cargo = "profile:cargo-target && !profile:neg";
    // `/var/tmp/srcb` is a directory when preflight checks it, then a
    // symlink by the time discovery reaches it: discovery refuses the root
    // as unsafe_path, which excludes the tier after its targets were found.
    let unsafe_root = "  /var/tmp/srcb: {.: {}}\n  /var/tmp/lnk: {symlink: src}\n";
    let turns_symlink = "events:\n  - {at: {op: statx, path: /var/tmp/srcb, when: after}, remove: \
                         /var/tmp/srcb}\n  - {at: {op: statx, path: /var/tmp/srcb, when: after}, \
                         rename: {from: /var/tmp/lnk, to: /var/tmp/srcb}}\n";
    let cases: [EarlyStop<'_>; 5] = [
        (
            "union, the negated scan fails",
            union,
            &[],
            "  /var/tmp/negroot/systemd-private-x: {.: {}}\n",
            "",
            7,
        ),
        (
            "union, a recheck fails",
            union,
            &[],
            "",
            &neg_recheck_fails("/var/tmp/spool-old"),
            7,
        ),
        (
            "cargo, the negated scan fails",
            cargo,
            &["/var/tmp/src"],
            "  /var/tmp/negroot/systemd-private-x: {.: {}}\n",
            "",
            2,
        ),
        (
            "cargo, a recheck fails",
            cargo,
            &["/var/tmp/src"],
            "",
            &neg_recheck_fails("/var/tmp/src/p/target/debug/deps"),
            2,
        ),
        (
            "cargo, a misconfigured root excludes the tier",
            "profile:cargo-target",
            &["/var/tmp/src", "/var/tmp/srcb"],
            unsafe_root,
            turns_symlink,
            2,
        ),
    ];
    for (what, select, roots, extra, events, found) in cases {
        let host = fixture_with(&format!("{}{extra}", early_stop_tree()), events);
        let request = if roots.is_empty() {
            request(select, "1028M")
        } else {
            request_with_roots(select, roots)
        };
        let report = run(&host, &request, true);
        assert!(host.unfired_events().is_empty(), "{what}: events fired");
        assert_eq!(report.status, Status::Error, "{what}: {report:#?}");
        let summary = summary(&report);
        assert_eq!(sum(&summary, "found"), found, "{what}: {summary:#}");
        let judged = report.plan.iter().filter(|p| p.candidate.is_some()).count() as u64;
        assert_eq!(
            judged + sum(&summary, "not_judged"),
            found,
            "{what}: {summary:#}"
        );
    }
}

/// Codex review 2 (TASK-324): a union whose discovery refused a directory
/// on the way and then reached the deadline keeps that refusal's row, so
/// it stays in `found`.
#[test]
fn a_union_cut_in_discovery_keeps_the_refusals_it_listed() {
    // `walled/locked` cannot be listed (root's, 0700); the deadline passes
    // while discovery lists `walled/open`, which comes after it.
    let host = fixture_with(
        "  /var/tmp/walled/locked: {.: {uid: 0, gid: 0, mode: \"0700\"}}\n  \
         /var/tmp/walled/open/item-1: {}\n",
        "events:\n  - {at: {op: read_dir, path: /var/tmp/walled/open, when: after}, advance: \
         700s}\n",
    );
    let report = run(
        &host,
        &request("profile:walled || profile:cache", "1028M"),
        true,
    );
    assert!(host.unfired_events().is_empty(), "the clock moved");
    assert_eq!(report.status, Status::Deadline, "{report:#?}");
    let refused: Vec<_> = report
        .plan
        .iter()
        .filter(|p| p.candidate.as_deref() == Some(std::path::Path::new("/var/tmp/walled/locked")))
        .collect();
    assert_eq!(refused.len(), 1, "the refusal is kept: {report:#?}");
    let summary = summary(&report);
    let walled = summary
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["profile"] == "walled")
        .expect("walled's block");
    assert_eq!(walled["kept"][0]["count"], 1, "{summary:#}");
    assert!(walled["found"].as_u64().unwrap() >= 1, "{summary:#}");
}

/// Codex review 3 (TASK-324): with the tally budget spent before any
/// candidate is sized, a reclaim's own walk under the lock still measures
/// a candidate it then refuses (Cargo: the idle requirement; fs: the
/// in-use check), and the kept row carries that size, not `unsized`.
#[test]
fn a_reclaim_past_its_tally_budget_sizes_what_its_own_walk_refused() {
    let host = fixture_with(&early_stop_tree(), "");
    // Built 2026-10-01: not idle for 30 days.
    let mut cargo =
        request_with_roots("profile:cargo-target && filter:idle-30d", &["/var/tmp/src"]);
    cargo.min_free = parse_size("99G").unwrap();
    cargo.tally_budget = Some(Duration::ZERO);
    let report = run(&host, &cargo, false);
    assert!(report.actions.is_empty(), "{report:#?}");
    let kept = &summary(&report)[0]["kept"];
    assert_eq!(
        kept,
        &json!([{"reason": "not_idle", "count": 2, "estimated_bytes": 2 * cargo_target_bytes(), "unsized": 0}]),
        "{report:#?}"
    );

    let host = fixture("");
    let mut spool = request("profile:spool", "99G");
    spool.tally_budget = Some(Duration::ZERO);
    let report = run(&host, &spool, false);
    let busy = report
        .skipped
        .iter()
        .find(|s| s.candidate.as_deref() == Some(std::path::Path::new("/var/tmp/spool-busy")))
        .unwrap_or_else(|| panic!("{report:#?}"));
    assert_eq!(busy.reason, diskgc_core::engine::Reason::InUse);
    assert_eq!(busy.estimated_bytes, Some(4 << 20), "{report:#?}");
}

/// What one target of [`cargo_profile_dir`] measures: the 1 MiB unit and
/// the directories and files around it the walks count.
fn cargo_target_bytes() -> u64 {
    // Learnt from a dry run's eligible estimate of the same layout.
    let host = fixture_with(&early_stop_tree(), "");
    let report = run(
        &host,
        &request_with_roots("profile:cargo-target", &["/var/tmp/src"]),
        true,
    );
    report.plan[0]
        .estimated_bytes
        .expect("an eligible target's estimate")
}
