//! The reclaim walk on the index (TASK-31; docs/architecture.md, The reclaim
//! walk; docs/testing.md, index scenarios): every candidate taken from the
//! index is found again and checked live, a stale or wrong unit is never
//! walked, each tier is discovered after its queue, and a collapsed block
//! is expanded by the tier's own discovery, never from above its root.
//!
//! An index is either built by the run's own synchronous tally (made stale
//! by an event after it: the host lock's `flock`, which comes right after
//! the tally) or injected ([`seeded`]), as a snapshot would seed it
//! (TASK-30). Where a verdict must come from the index's queue rather than
//! the tier's discovery after it, the log order shows it ([`from_queue`]).

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::config::Order;
use diskgc_core::engine::Reason;
use diskgc_core::executor::{self, ProtectedKind, ProtectedSource, Request, Tier};
use diskgc_core::fs_kind::Find;
use diskgc_core::host::{Dev, Host, MonoTime, Timestamp};
use diskgc_core::index::{
    BUDGET_BYTES, Discovered, Family, Finder, Identity, Index, MountKey, Refusal, Tally, UnitRef,
};
use diskgc_core::profile::{EntryType, Library, Where, parse_profiles};
use diskgc_core::report::Report;
use diskgc_core::snapshot::RunIndex;
use diskgc_core::tally::TallyState;
use diskgc_core::units::{Size, parse_size};
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;

const PROFILES: &str = r#"
- name: tmp-a
  find: {roots: [/tmp], paths: ["a*"], type: dir}
  action: delete
  examples: {match: [/tmp/a0]}
- name: tmp-b
  find: {roots: [/tmp], paths: ["b*"], type: dir}
  action: delete
  examples: {match: [/tmp/b0]}
- name: two-level-a
  find: {roots: [/tmp], paths: ["*/a*"], type: dir}
  action: delete
  examples: {match: [/tmp/p/a0]}
- name: deep-a
  find: {roots: [/tmp/w/r], paths: ["a*"], type: dir}
  action: delete
  examples: {match: [/tmp/w/r/a0]}
- name: two-level-any
  find: {roots: [/tmp], paths: ["*/*"], type: any}
  action: delete
  examples: {match: [/tmp/p/f]}
- name: two-roots-a
  find: {roots: [/tmp, /var/tmp], paths: ["a*"], type: dir}
  action: delete
  examples: {match: [/tmp/a0, /var/tmp/a0]}
- name: bind-view
  find: {roots: [/tmp/z/a, /tmp/b/z/a/sub], paths: ["f", "*/f"], type: file}
  action: delete
  examples: {match: [/tmp/z/a/sub/f]}
"#;

const OLD: &str = "2026-10-01T00:00:00Z";
const HOST_LOCK_TAKEN: &str = "{op: flock, path: /run/diskgc/lock, when: after}";

fn library() -> Library {
    let specs = parse_profiles(PROFILES).unwrap_or_else(|e| panic!("{e}"));
    Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"))
}

/// `/` (ext4), `/run` with the host lock, and a 1 GiB `/tmp` (tmpfs, dev
/// 0:42, 500 MiB used outside the model); process 7 a readable bystander.
fn yaml(tree: &str, rest: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, other_used: 500M, parent: 1}}
tree:
  /usr/bin/tool: {{}}
  /run/diskgc/lock: {{uid: 0, gid: 0}}
{tree}processes:
  - {{pid: 7, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{{path: /usr/bin/tool}}], environ: [PATH=/usr/bin]}}
{rest}"#
    )
}

/// An old candidate directory holding one 10 KiB file.
fn old_dir() -> String {
    format!("{{.: {{mtime: {OLD}}}, f: {{size: 10K, mtime: {OLD}}}}}")
}

fn tier(select: &str) -> Tier {
    Tier::select(select, &library(), None, 1000).unwrap_or_else(|e| panic!("{e}"))
}

fn request(tier: Tier) -> Request {
    Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size("100%").expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier,
    }
}

fn tmp_mount() -> MountKey {
    MountKey {
        dev: Dev {
            major: 0,
            minor: 42,
        },
        mount_point: PathBuf::from("/tmp"),
    }
}

fn finder(tier: &Tier, root: &str) -> Finder {
    Finder {
        profile: tier.profile.as_str().into(),
        profile_hash: tier.profile_hash(),
        root: Path::new(root).into(),
    }
}

/// What discovery reports for `path`, read from the model without a port
/// call (no event counts it).
fn discovered(host: &ModelHost, path: &str) -> Discovered {
    let stat = host.stat_path(Path::new(path)).expect("exists");
    let parent = host
        .stat_path(Path::new(path).parent().unwrap())
        .expect("parent");
    Discovered {
        identity: Identity::of(&stat, parent.ino.unwrap()).expect("inode reported"),
        newest_change: stat.mtime.max(stat.ctime).expect("times"),
        checked_at: MonoTime(Duration::ZERO),
    }
}

/// An index as a recent snapshot would seed it: each of `paths` found by
/// `finder` and sized now, and the finder's root listed now, so the run's
/// synchronous tally neither lists the root again nor sizes the units
/// (docs/architecture.md, The tally: Synchronous). Whatever changed on the
/// disk since is unknown to it.
fn seeded(host: &ModelHost, finder: &Finder, paths: &[&str]) -> TallyState {
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    for path in paths {
        state
            .index
            .insert(
                host,
                &tmp_mount(),
                Path::new(path),
                finder,
                discovered(host, path),
            )
            .expect("inserted");
        sized_now(host, &mut state, path);
    }
    state.set_discovered_at(finder.clone(), host.now());
    state
}

/// The single at `path` as sized just now: the synchronous tally does not
/// size it again (it would evict one whose identity is wrong).
fn sized_now(host: &ModelHost, state: &mut TallyState, path: &str) {
    let now = host.now();
    let single = state
        .index
        .single_mut(&tmp_mount(), Path::new(path))
        .expect("a single");
    single.tally = Some(Tally {
        est_bytes: 10 << 10,
        files: 2,
        tallied_at: now,
    });
}

fn run(host: &ModelHost, request: &Request, state: &mut TallyState) -> (Report, Vec<String>) {
    let mut lines = Vec::new();
    let report = in_run(state, |index| {
        executor::ensure_with_index(host, request, &[1000], index, &mut |l| {
            lines.push(l.to_owned())
        })
    });
    eprintln!("log:\n  {}", lines.join("\n  "));
    (report, lines)
}

/// Runs `f` with `state` as the run's index (neither read from nor written
/// to a snapshot), leaving what the run made of it in `state`.
fn in_run<R>(state: &mut TallyState, f: impl FnOnce(&mut RunIndex) -> R) -> R {
    let taken = std::mem::replace(state, TallyState::new(Index::new(BUDGET_BYTES)));
    let mut index = RunIndex::of(taken);
    let result = f(&mut index);
    *state = index.state;
    result
}

fn actions(report: &Report) -> Vec<String> {
    report
        .actions
        .iter()
        .map(|a| a.candidate.display().to_string())
        .collect()
}

fn skip_of(report: &Report, candidate: &str) -> Option<Reason> {
    report
        .skipped
        .iter()
        .find(|s| s.candidate.as_deref() == Some(Path::new(candidate)))
        .map(|s| s.reason)
}

fn exists(host: &ModelHost, path: &str) -> bool {
    host.stat_path(Path::new(path)).is_some()
}

/// Whether the log shows `needle` before the tier's own discovery (its
/// "candidate(s) below" line), that is from the index's queue.
fn from_queue(lines: &[String], needle: &str) -> bool {
    let at = lines.iter().position(|l| l.contains(needle));
    let discovery = lines.iter().position(|l| l.contains("candidate(s) below"));
    match (at, discovery) {
        (Some(at), Some(discovery)) => at < discovery,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// AC #2 (docs/testing.md: a stale index): after the run's synchronous
/// tally sized every candidate as old and harmless, each changes before
/// the walk reaches it: a deep modification, a new `.diskgc-keep`, a new
/// Cargo target inside, a new protected match. The walk takes each from
/// the index's queue and its live checks refuse it; nothing is deleted
/// but the one candidate left alone.
#[test]
fn stale_index_changes_after_the_tally_are_refused_at_deletion_time() {
    let tree = format!(
        "  /tmp/a-deep: {{.: {{mtime: {OLD}}}, sub: {{.: {{mtime: {OLD}}}, f: {{size: 10K, mtime: {OLD}}}}}}}\n  \
         /tmp/a-keep: {d}\n  /tmp/a-cargo: {d}\n  /tmp/a-protected: {d}\n  /tmp/a-plain: {d}\n",
        d = old_dir()
    );
    let events = format!(
        "events:\n  \
         - {{at: {HOST_LOCK_TAKEN}, create: /tmp/a-deep/sub/deeper/new}}\n  \
         - {{at: {HOST_LOCK_TAKEN}, create: /tmp/a-keep/sub/.diskgc-keep}}\n  \
         - {{at: {HOST_LOCK_TAKEN}, create: /tmp/a-cargo/proj/target/debug/.cargo-lock}}\n  \
         - {{at: {HOST_LOCK_TAKEN}, mkdir: /tmp/a-protected/precious}}\n"
    );
    let host = host(&yaml(&tree, &events));
    let mut tier = tier("profile:tmp-a && filter:idle-1d");
    tier.protections.push(ProtectedSource {
        profile: "precious".to_owned(),
        roots: vec![PathBuf::from("/tmp")],
        kind: ProtectedKind::Fs(Box::new(Find::new(
            &["*/precious".to_owned()],
            EntryType::Dir,
            &Where::default(),
            1000,
        ))),
    });
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let (report, lines) = run(&host, &request(tier), &mut state);
    assert!(host.unfired_events().is_empty());
    assert_eq!(actions(&report), ["/tmp/a-plain"], "{report:#?}");
    for (candidate, reason) in [
        ("/tmp/a-deep", Reason::NotIdle),
        ("/tmp/a-keep", Reason::KeepMarker),
        ("/tmp/a-cargo", Reason::ContainsSpecial),
        ("/tmp/a-protected", Reason::Protected),
    ] {
        assert_eq!(skip_of(&report, candidate), Some(reason), "{candidate}");
        assert!(
            from_queue(&lines, &format!("skip {reason}: {candidate}")),
            "{candidate} judged from the index's queue"
        );
        assert!(exists(&host, candidate), "{candidate}");
    }
    assert!(exists(&host, "/tmp/a-deep/sub/deeper/new"));
    assert!(exists(&host, "/tmp/a-keep/sub/.diskgc-keep"));
    assert!(exists(&host, "/tmp/a-cargo/proj/target/debug/.cargo-lock"));
    assert!(exists(&host, "/tmp/a-protected/precious"));
}

/// AC #2 with an injected index (a seed whose root listing is recent, so
/// the tally lists nothing and sizes nothing): the same changes, made
/// before the run, are refused just the same.
#[test]
fn a_seeded_index_that_missed_the_changes_is_refused_at_deletion_time() {
    let tree = format!(
        "  /tmp/a-deep: {{.: {{mtime: {OLD}}}, sub: {{.: {{mtime: {OLD}}}, f: {{size: 10K, mtime: {OLD}}}, new: {{size: 1K}}}}}}\n  \
         /tmp/a-keep: {{.: {{mtime: {OLD}}}, sub: {{.diskgc-keep: {{mtime: {OLD}}}}}}}\n  \
         /tmp/a-plain: {d}\n",
        d = old_dir()
    );
    let host = host(&yaml(&tree, ""));
    let tier = tier("profile:tmp-a && filter:idle-1d");
    let mut state = seeded(
        &host,
        &finder(&tier, "/tmp"),
        &["/tmp/a-deep", "/tmp/a-keep", "/tmp/a-plain"],
    );
    let (report, lines) = run(&host, &request(tier), &mut state);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("0 source(s) listed, 1 recent")),
        "the seed's recent listing is not repeated: {lines:#?}"
    );
    assert_eq!(actions(&report), ["/tmp/a-plain"], "{report:#?}");
    assert_eq!(skip_of(&report, "/tmp/a-deep"), Some(Reason::NotIdle));
    assert_eq!(skip_of(&report, "/tmp/a-keep"), Some(Reason::KeepMarker));
    assert!(from_queue(&lines, "skip not_idle: /tmp/a-deep"));
    assert!(from_queue(&lines, "skip keep_marker: /tmp/a-keep"));
}

/// docs/testing.md `index_unit_not_matching_its_root_is_stale`: a unit that
/// discovery would not find under the tier's root (its name does not match
/// the pattern, it lies below another candidate, it is gone, or another
/// object has its name) is skipped `stale` and never walked or locked; one
/// whose finder is another profile or definition never enters the tier's
/// queue. What is really there is judged by the tier's discovery after the
/// queue.
#[test]
fn a_unit_discovery_would_not_find_is_stale_and_never_walked() {
    let tree = format!(
        "  /tmp/x-nomatch: {d}\n  /tmp/a-outer: {{.: {{mtime: {OLD}}}, a-inner: {d}}}\n  \
         /tmp/a-replaced: {d}\n  /tmp/a-gone: {d}\n  /tmp/a-other: {d}\n",
        d = old_dir()
    );
    let events = format!("events:\n  - {{at: {HOST_LOCK_TAKEN}, remove: /tmp/a-gone}}\n");
    let host = host(&yaml(&tree, &events));
    let tier = tier("profile:tmp-a");
    let right = finder(&tier, "/tmp");
    let other_profile = Finder {
        profile: "tmp-b".into(),
        ..right.clone()
    };
    let other_hash = Finder {
        profile_hash: right.profile_hash ^ 1,
        ..right.clone()
    };
    let mut replaced = discovered(&host, "/tmp/a-replaced");
    replaced.identity.ino += 1000;
    let units = [
        (
            "/tmp/x-nomatch",
            &right,
            discovered(&host, "/tmp/x-nomatch"),
        ),
        (
            "/tmp/a-outer/a-inner",
            &right,
            discovered(&host, "/tmp/a-outer/a-inner"),
        ),
        ("/tmp/a-replaced", &right, replaced),
        ("/tmp/a-gone", &right, discovered(&host, "/tmp/a-gone")),
        (
            "/tmp/a-other",
            &other_profile,
            discovered(&host, "/tmp/a-other"),
        ),
        (
            "/tmp/a-other",
            &other_hash,
            discovered(&host, "/tmp/a-other"),
        ),
    ];
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    for (path, finder, found) in units {
        state
            .index
            .insert(&host, &tmp_mount(), Path::new(path), finder, found)
            .expect("inserted");
        sized_now(&host, &mut state, path);
    }
    // The root counts as listed: the tally adds nothing, the index is all
    // the queue has.
    state.set_discovered_at(right.clone(), host.now());
    let (report, lines) = run(&host, &request(tier), &mut state);
    assert!(host.unfired_events().is_empty());
    for stale in [
        "/tmp/x-nomatch",
        "/tmp/a-outer/a-inner",
        "/tmp/a-replaced",
        "/tmp/a-gone",
    ] {
        assert_eq!(
            skip_of(&report, stale),
            Some(Reason::Stale),
            "{stale}: {report:#?}"
        );
        assert!(
            from_queue(&lines, &format!("skip stale: {stale}:")),
            "{stale}"
        );
    }
    assert_eq!(skip_of(&report, "/tmp/a-other"), None, "never queued");
    // Never walked: no name below the non-matching unit is ever looked at,
    // and it is never opened or locked.
    assert!(
        host.trace()
            .iter()
            .all(|call| call
                .path
                .as_deref()
                .is_none_or(|p| !p.starts_with("/tmp/x-nomatch")
                    || !matches!(
                        call.op,
                        Op::Openat2 | Op::ReadDir | Op::Flock | Op::Unlinkat
                    ))),
        "the non-matching unit was walked"
    );
    assert!(exists(&host, "/tmp/x-nomatch/f"));
    // The tier's own discovery judges what is there: the outer candidate
    // (with the inner one), the object that now has the replaced unit's
    // name, and the unit the queue never took.
    let mut acted = actions(&report);
    acted.sort();
    assert_eq!(acted, ["/tmp/a-other", "/tmp/a-outer", "/tmp/a-replaced"]);
}

/// Inserts each of `paths` (on `/tmp`) found by `finder`, sized now.
fn put(host: &ModelHost, state: &mut TallyState, finder: &Finder, paths: &[&str]) {
    for path in paths {
        state
            .index
            .insert(
                host,
                &tmp_mount(),
                Path::new(path),
                finder,
                discovered(host, path),
            )
            .expect("inserted");
        sized_now(host, state, path);
    }
}

/// The blocks of `state`'s index: (site, members, whether it stands for
/// any name at any depth).
fn blocks(state: &TallyState) -> Vec<(PathBuf, u64, bool)> {
    let mut out = Vec::new();
    state.index.for_each_unit(|_, path, unit| {
        if let UnitRef::Block(b) = unit {
            out.push((path.to_owned(), b.members, b.family == Family::Any));
        }
    });
    out
}

/// AC #3 (docs/testing.md
/// `new_candidate_after_a_recent_snapshot_is_found_before_pressure`): the
/// seed's listing of the stale tier's root is recent, so the run's tally
/// does not list it again, and a candidate created after that listing is
/// not in the index. The stale tier's own discovery after its queue finds
/// it, and it is deleted before the pressure tier deletes anything.
#[test]
fn new_cruft_after_the_snapshot_is_deleted_before_the_pressure_tier() {
    let tree = format!(
        "  /tmp/a-indexed: {d}\n  /tmp/a-new: {d}\n  /tmp/b-pressure: {d}\n",
        d = old_dir()
    );
    let host = host(&yaml(&tree, ""));
    let mut stale = tier("profile:tmp-a && filter:idle-1d");
    stale.name = "stale".to_owned();
    let mut pressure = tier("profile:tmp-b");
    pressure.name = "pressure".to_owned();
    let mut state = seeded(&host, &finder(&stale, "/tmp"), &["/tmp/a-indexed"]);
    put(
        &host,
        &mut state,
        &finder(&pressure, "/tmp"),
        &["/tmp/b-pressure"],
    );
    state.set_discovered_at(finder(&pressure, "/tmp"), host.now());
    let tiers = [stale.clone(), pressure];
    let mut lines = Vec::new();
    let report = in_run(&mut state, |index| {
        executor::ensure_grouped_with_index(
            &host,
            &request(stale),
            &tiers,
            &[Duration::ZERO, Duration::ZERO],
            &[1000],
            index,
            &mut |l| lines.push(l.to_owned()),
        )
    });
    eprintln!("log:\n  {}", lines.join("\n  "));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("0 source(s) listed, 2 recent")),
        "the tally listed nothing: {lines:#?}"
    );
    assert_eq!(
        actions(&report),
        ["/tmp/a-indexed", "/tmp/a-new", "/tmp/b-pressure"],
        "{report:#?}"
    );
    let tiers: Vec<&str> = report.actions.iter().map(|a| a.tier.as_str()).collect();
    assert_eq!(tiers, ["stale", "stale", "pressure"]);
}

/// docs/testing.md `stored_refusal_never_fakes_exhaustion`: a unit an
/// earlier tally refused (`keep_marker`, a hint from before this run) whose
/// marker is gone is ranked after the tier's other units, judged again and
/// deleted in the same run.
#[test]
fn a_stored_refusal_hint_only_ranks_the_unit_last() {
    let tree = format!(
        "  /tmp/a-hinted: {{.: {{mtime: 2026-09-01T00:00:00Z}}, f: {{size: 10K, mtime: 2026-09-01T00:00:00Z}}}}\n  \
         /tmp/a-plain: {d}\n",
        d = old_dir()
    );
    let host = host(&yaml(&tree, ""));
    let tier = tier("profile:tmp-a");
    let mut state = seeded(
        &host,
        &finder(&tier, "/tmp"),
        &["/tmp/a-hinted", "/tmp/a-plain"],
    );
    state
        .index
        .single_mut(&tmp_mount(), Path::new("/tmp/a-hinted"))
        .expect("a single")
        .refusal = Some(Refusal {
        reason: Reason::KeepMarker,
        at: MonoTime(Duration::ZERO),
    });
    let (report, lines) = run(&host, &request(tier), &mut state);
    assert!(lines.iter().any(|l| l.contains("1 with a refusal hint")));
    // Oldest first, but the hinted unit after the others.
    assert_eq!(
        actions(&report),
        ["/tmp/a-plain", "/tmp/a-hinted"],
        "{report:#?}"
    );
}

/// Codex review of TASK-31 (finding 3, a single index corruption): a
/// refusal hint whose time reads as this run's (a clock that went back, a
/// snapshot written by a later boot, a corrupt value) must not stand for
/// this run's own tally refusing the unit. Only refusals this invocation's
/// tally produced skip a candidate; a cached hint only ranks it last. So
/// the stale tier's eligible candidate is judged and deleted before the
/// pressure tier runs (PRD, What bad looks like: a pressure tier running
/// while an earlier tier still had eligible candidates).
#[test]
fn a_cached_refusal_hint_timed_in_this_run_never_suppresses_the_candidate() {
    let tree = format!(
        "  /tmp/a-hinted: {d}\n  /tmp/b-pressure: {d}\n",
        d = old_dir()
    );
    let host = host(&yaml(&tree, ""));
    let mut stale = tier("profile:tmp-a && filter:idle-1d");
    stale.name = "stale".to_owned();
    let mut pressure = tier("profile:tmp-b");
    pressure.name = "pressure".to_owned();
    let mut state = seeded(&host, &finder(&stale, "/tmp"), &["/tmp/a-hinted"]);
    put(
        &host,
        &mut state,
        &finder(&pressure, "/tmp"),
        &["/tmp/b-pressure"],
    );
    state.set_discovered_at(finder(&pressure, "/tmp"), host.now());
    // At or after any run start this test can have.
    state
        .index
        .single_mut(&tmp_mount(), Path::new("/tmp/a-hinted"))
        .expect("a single")
        .refusal = Some(Refusal {
        reason: Reason::KeepMarker,
        at: MonoTime(Duration::from_secs(1 << 40)),
    });
    let tiers = [stale.clone(), pressure];
    let mut lines = Vec::new();
    let report = in_run(&mut state, |index| {
        executor::ensure_grouped_with_index(
            &host,
            &request(stale),
            &tiers,
            &[Duration::ZERO, Duration::ZERO],
            &[1000],
            index,
            &mut |l| lines.push(l.to_owned()),
        )
    });
    eprintln!("log:\n  {}", lines.join("\n  "));
    assert_eq!(
        actions(&report),
        ["/tmp/a-hinted", "/tmp/b-pressure"],
        "{report:#?}"
    );
    let tiers: Vec<&str> = report.actions.iter().map(|a| a.tier.as_str()).collect();
    assert_eq!(tiers, ["stale", "pressure"]);
    assert_eq!(skip_of(&report, "/tmp/a-hinted"), None, "{report:#?}");
}

/// docs/architecture.md, Refusals: a refusal this run's own synchronous
/// tally found skips the candidate for the run, though the tree reads
/// fine later: it is never walked or locked again in this run.
#[test]
fn a_refusal_of_this_runs_tally_skips_the_candidate_for_the_run() {
    let tree = format!(
        "  /tmp/a-locked-down: {{.: {{mtime: {OLD}}}, sub: {{.: {{mtime: {OLD}}}, f: {{size: 1K, mtime: {OLD}}}}}}}\n  \
         /tmp/a-plain: {d}\n",
        d = old_dir()
    );
    let events = "events:\n  - {at: {op: read_dir, path: /tmp/a-locked-down/sub, nth: 1, when: before}, fail: EACCES}\n";
    let host = host(&yaml(&tree, events));
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let (report, _) = run(&host, &request(tier("profile:tmp-a")), &mut state);
    assert!(host.unfired_events().is_empty());
    assert_eq!(actions(&report), ["/tmp/a-plain"], "{report:#?}");
    assert_eq!(
        skip_of(&report, "/tmp/a-locked-down"),
        Some(Reason::Unreadable)
    );
    assert!(
        host.trace()
            .iter()
            .all(|c| !(c.op == Op::Flock
                && c.path.as_deref() == Some(Path::new("/tmp/a-locked-down")))),
        "never locked"
    );
    assert_eq!(
        host.trace()
            .iter()
            .filter(|c| c.op == Op::ReadDir
                && c.path.as_deref() == Some(Path::new("/tmp/a-locked-down/sub")))
            .count(),
        1,
        "listed by the tally only"
    );
}

/// A family block of the two old candidates `/tmp/a-1` and `/tmp/a-2`,
/// sited at `/tmp`, from a collapse of their singles.
fn family_block(host: &ModelHost, tier: &Tier) -> TallyState {
    let mut state = seeded(host, &finder(tier, "/tmp"), &["/tmp/a-1", "/tmp/a-2"]);
    let before = state.index.accounted_bytes();
    state
        .index
        .collapse_to(host, before - 1)
        .expect("collapsed");
    assert_eq!(
        blocks(&state),
        [(PathBuf::from("/tmp"), 2, false)],
        "one family block"
    );
    state
}

/// AC #5 (docs/testing.md: an index block whose later member is opened
/// after an earlier member was deleted): the block is expanded, its first
/// member deleted; five seconds later a process opens a file of the second.
/// The scan of the first member is past the reuse limit by then: the
/// second is checked on a new scan and kept (`in_use`). Within the reuse
/// window the open is not seen (the accepted limit 5, pinned as such).
#[test]
fn a_block_member_opened_after_an_earlier_member_was_deleted_is_in_use() {
    let tree = "  /tmp/a-1: {.: {mtime: 2026-09-01T00:00:00Z}, f: {size: 10K, mtime: 2026-09-01T00:00:00Z}}\n  \
                /tmp/a-2: {.: {mtime: 2026-10-01T00:00:00Z}, f: {size: 10K, mtime: 2026-10-01T00:00:00Z}}\n";
    let process = "  - {pid: 9, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{path: /usr/bin/tool}], environ: [PATH=/usr/bin]}\n";
    let deleted = "{op: unlinkat, path: /tmp/a-1, when: after}";
    for (advance, expect_second) in [("advance: 5s", Some(Reason::InUse)), ("advance: 0s", None)] {
        // The rest of the model continues the process list.
        let rest = format!(
            "{process}events:\n  - {{at: {deleted}, {advance}}}\n  - {{at: {deleted}, open: {{pid: 9, path: /tmp/a-2/f}}}}\n"
        );
        let host = host(&yaml(tree, &rest));
        let tier = tier("profile:tmp-a");
        let mut state = family_block(&host, &tier);
        let (report, lines) = run(&host, &request(tier), &mut state);
        assert!(host.unfired_events().is_empty(), "{advance}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("block at /tmp (2 recorded member(s)) expanded: 2")),
            "{advance}: {lines:#?}"
        );
        assert_eq!(
            report.actions[0].candidate,
            Path::new("/tmp/a-1"),
            "{advance}"
        );
        match expect_second {
            Some(reason) => {
                assert_eq!(
                    skip_of(&report, "/tmp/a-2"),
                    Some(reason),
                    "{advance}: {report:#?}"
                );
                assert!(exists(&host, "/tmp/a-2/f"));
            }
            None => assert_eq!(actions(&report), ["/tmp/a-1", "/tmp/a-2"], "{advance}"),
        }
    }
}

/// AC #6 (docs/testing.md `block_expansion_finds_only_what_discovery_would`;
/// docs/architecture.md, Expanding a block): a lump of two finder sets
/// collapsed to the mount's root `/tmp` stands above the bound root
/// `/tmp/w/r` of the tier that expands it. The tier's discovery runs from
/// the deeper of the two, its own root: names that match its pattern above
/// the root (`/tmp/a-top`, `/tmp/w/a-above`) are never members, nor are a
/// non-matching name, a symlink or a mount beneath the root; every member,
/// including one created after the collapse, is checked live (a kept one
/// is refused).
#[test]
fn a_lump_above_the_bound_root_is_expanded_from_the_root_only() {
    let tree = format!(
        "  /tmp/a-top: {d}\n  /tmp/w/a-above: {d}\n  /tmp/w/r/a-1: {d}\n  /tmp/w/r/a-2: {d}\n  \
         /tmp/w/r/a-kept: {{.: {{mtime: {OLD}}}, .diskgc-keep: {{mtime: {OLD}}}}}\n  \
         /tmp/w/r/x-nomatch: {d}\n  /tmp/w/r/a-link: {{symlink: /tmp/a-top}}\n  /tmp/w/r/a-mnt: {{.: {{}}}}\n  \
         /tmp/b-1: {d}\n",
        d = old_dir()
    );
    let mounts =
        "  - {id: 31, at: /tmp/w/r/a-mnt, fs: tmpfs, dev: \"0:43\", size: 1M, parent: 30}\n";
    let model = yaml(&tree, "").replace(
        "  - {id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, other_used: 500M, parent: 1}\n",
        &format!(
            "  - {{id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, other_used: 500M, parent: 1}}\n{mounts}"
        ),
    ) + "events:\n  - {at: {op: flock, path: /run/diskgc/lock, when: after}, mkdir: /tmp/w/r/a-3}\n";
    let host = host(&model);
    let deep = Tier::select("profile:deep-a", &library(), None, 1000).expect("tier");
    let other = tier("profile:tmp-b");
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    put(
        &host,
        &mut state,
        &finder(&deep, "/tmp/w/r"),
        &["/tmp/w/r/a-1", "/tmp/w/r/a-2", "/tmp/w/r/a-kept"],
    );
    put(&host, &mut state, &finder(&other, "/tmp"), &["/tmp/b-1"]);
    state.set_discovered_at(finder(&deep, "/tmp/w/r"), host.now());
    state
        .index
        .collapse_to(&host, 0)
        .expect("collapsed to the limit");
    assert_eq!(
        blocks(&state),
        [(PathBuf::from("/tmp"), 4, true)],
        "one lump at the mount's root"
    );
    let (report, lines) = run(&host, &request(deep), &mut state);
    assert!(host.unfired_events().is_empty());
    assert!(
        lines
            .iter()
            .any(|l| l.contains("block at /tmp (4 recorded member(s)) expanded: 4")),
        "a-1, a-2, a-kept and a-3: {lines:#?}"
    );
    let mut acted = actions(&report);
    acted.sort();
    assert_eq!(
        acted,
        ["/tmp/w/r/a-1", "/tmp/w/r/a-2", "/tmp/w/r/a-3"],
        "{report:#?}"
    );
    assert_eq!(
        skip_of(&report, "/tmp/w/r/a-kept"),
        Some(Reason::KeepMarker)
    );
    assert!(from_queue(&lines, "skip keep_marker: /tmp/w/r/a-kept"));
    for kept in [
        "/tmp/a-top/f",
        "/tmp/w/a-above/f",
        "/tmp/w/r/x-nomatch/f",
        "/tmp/b-1/f",
        "/tmp/w/r/a-link",
    ] {
        assert!(exists(&host, kept), "{kept}");
    }
    // Never looked at from above the root: nothing but the way down to it.
    let above: Vec<_> = host
        .trace()
        .into_iter()
        .filter(|c| {
            c.path.as_deref().is_some_and(|p| {
                (p.starts_with("/tmp/a-top") || p.starts_with("/tmp/w/a-above"))
                    && matches!(c.op, Op::Openat2 | Op::ReadDir | Op::Flock | Op::Unlinkat)
            })
        })
        .collect();
    assert!(above.is_empty(), "{above:#?}");
}

/// A liveness contract takes no scan older than its candidate's finding
/// (Owner PID's ordering), and the index's queue finds each candidate again
/// right before judging it: found ahead in a batch, the candidates of a
/// contract tier still share one process scan (TASK-203), not one each.
#[test]
fn a_liveness_tier_from_the_index_shares_one_scan() {
    let mut tree = String::from("  /usr/bin/bash: {}\n");
    for n in 0..5 {
        tree.push_str(&format!(
            "  /tmp/nix-shell-4242-{n}: {{.: {{mtime: {OLD}}}, rc: {{mtime: {OLD}}}}}\n"
        ));
    }
    // Every port call takes 1 ms: a candidate found again after a scan
    // started is found later than that scan.
    let host = host(&yaml(&tree, "call_cost: 1ms\n"));
    let library = Library::builtin().expect("built-ins");
    let tier = Tier::select("profile:nix-shell-rc", &library, None, 1000).expect("tier");
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let (report, lines) = run(&host, &request(tier), &mut state);
    assert_eq!(report.actions.len(), 5, "{report:#?}");
    assert!(lines.iter().any(|l| l.contains("5 unit(s) from the index")));
    assert_eq!(host.trace_count(Op::OwnPid), 1, "one scan for the five");
}

/// docs/testing.md `sync_tally_budget_ending_in_discovery_still_finds_the_rest`:
/// a `--tally-budget` that ends during the synchronous tally's listing
/// leaves most candidates out of the index; the tier's own discovery after
/// the queue deletes them all, and the report says the tally was cut short.
#[test]
fn sync_tally_budget_ending_in_discovery_still_finds_the_rest() {
    let mut tree = String::new();
    for n in 0..10 {
        tree.push_str(&format!("  /tmp/a-{n}: {}\n", old_dir()));
    }
    // Every port call takes 1 ms; the budget is 5 ms.
    let host = host(&yaml(&tree, "call_cost: 1ms\n"));
    let mut request = request(tier("profile:tmp-a"));
    request.tally_budget = Some(Duration::from_millis(5));
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let (report, lines) = run(&host, &request, &mut state);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("index: synchronous tally") && l.contains("stopped")),
        "{lines:#?}"
    );
    assert!(report.tally_cut_short, "{report:#?}");
    assert_eq!(report.actions.len(), 10, "{report:#?}");
}

/// Fail closed on a failure of the host (docs/safety.md, Fail closed; the
/// reclaim walk acts only on a discovery that completed): an `EIO` while
/// listing a branch of the tier's root that holds no queued unit. Without
/// the index the tier's discovery stops and nothing of the tier is
/// reclaimed; with a seeded unit elsewhere under that root the same must
/// hold. The synchronous tally lists the root first and meets the `EIO`:
/// the tier then takes no queue and its own discovery stops it, an error
/// of the run (exit 3), nothing deleted.
#[test]
fn a_host_error_in_the_tally_keeps_the_tier_off_its_queue() {
    let tree = format!("  /tmp/p/a-x: {d}\n  /tmp/z/a-y: {d}\n", d = old_dir());
    let events = "events:\n  \
                  - {at: {op: read_dir, path: /tmp/z, nth: 1, when: before}, fail: EIO}\n  \
                  - {at: {op: read_dir, path: /tmp/z, nth: 2, when: before}, fail: EIO}\n";
    let host = host(&yaml(&tree, events));
    let tier = tier("profile:two-level-a");
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    put(&host, &mut state, &finder(&tier, "/tmp"), &["/tmp/p/a-x"]);
    let (report, lines) = run(&host, &request(tier), &mut state);
    assert!(report.actions.is_empty(), "{report:#?}");
    assert!(exists(&host, "/tmp/p/a-x/f"));
    assert_eq!(report.exit_code(), 3, "{report:#?}");
    assert!(
        report.errors.iter().any(|e| e.message.contains("/tmp/z")),
        "{report:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("index: ") && l.contains("/tmp/z")),
        "the tally's failure is logged: {lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("unit(s) from the index")),
        "no queue: {lines:#?}"
    );
}

/// The same rule when the failure comes from the tally's sizing (pass 2,
/// which sizes a `best-fit` first tier) rather than its listing: an `EIO`
/// while sizing a stale unit ends the tally with an error, and no tier
/// takes a queue. The `EIO` is transient here, so the tier's own discovery
/// and each candidate's live walk read fine and reclaim both candidates.
#[test]
fn a_host_error_in_the_tallys_sizing_keeps_the_tier_off_its_queue() {
    let host = host(&yaml(&bad_and_plain(), SIZING_EIO));
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let mut tier = tier("profile:tmp-a");
    tier.order = Order::BestFit;
    let (report, lines) = run(&host, &request(tier), &mut state);
    assert!(host.unfired_events().is_empty());
    assert!(
        lines
            .iter()
            .any(|l| l.contains("synchronous tally failed") && l.contains("/tmp/a-bad/sub")),
        "the tally's failure is logged: {lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("unit(s) from the index")),
        "no queue: {lines:#?}"
    );
    let mut deleted = actions(&report);
    deleted.sort();
    assert_eq!(deleted, ["/tmp/a-bad", "/tmp/a-plain"], "{report:#?}");
    assert!(report.errors.is_empty(), "{report:#?}");
}

/// `/tmp/a-bad` (its `sub` holding a 1 KiB file) and `/tmp/a-plain`, old.
fn bad_and_plain() -> String {
    format!(
        "  /tmp/a-bad: {{.: {{mtime: {OLD}}}, sub: {{.: {{mtime: {OLD}}}, f: {{size: 1K, mtime: {OLD}}}}}}}\n  \
         /tmp/a-plain: {d}\n",
        d = old_dir()
    )
}

/// An `EIO` at the first listing of `/tmp/a-bad/sub`: the first sizing walk
/// of `/tmp/a-bad`.
const SIZING_EIO: &str =
    "events:\n  - {at: {op: read_dir, path: /tmp/a-bad/sub, nth: 1, when: before}, fail: EIO}\n";

/// TASK-251: under `oldest` the first tier's sizing is the walk's, under
/// the host lock, as its queue reaches each unit; the same `EIO` there is
/// an error of the run, as any failure of the host during the walk (docs/
/// safety.md, Fail closed): the run stops before `/tmp/a-bad`, the first
/// unit, is judged, and deletes nothing. Stricter than pass 2's rule above
/// for a transient failure (there the tier fell back to its discovery and
/// reclaimed both); a persistent one ends both runs in an error.
#[test]
fn a_host_error_in_the_walks_deferred_sizing_is_an_error_of_the_run() {
    let host = host(&yaml(&bad_and_plain(), SIZING_EIO));
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let (report, lines) = run(&host, &request(tier("profile:tmp-a")), &mut state);
    assert!(host.unfired_events().is_empty());
    assert!(
        lines.iter().any(|l| l.contains("index: synchronous tally")
            && l.contains("sizing of the first tier's 2 stale unit(s) left to the reclaim walk")),
        "{lines:#?}"
    );
    assert!(report.actions.is_empty(), "{report:#?}");
    assert_eq!(report.errors.len(), 1, "{report:#?}");
    assert!(
        report.errors[0].message.contains("sizing /tmp/a-bad")
            && report.errors[0].message.contains("EIO"),
        "{report:#?}"
    );
    assert!(exists(&host, "/tmp/a-bad/sub/f") && exists(&host, "/tmp/a-plain/f"));
}

/// TASK-252 (Codex review of TASK-31, finding 1): a directory diskgc can
/// search but not read (mode `0333`) at a level where the tier's pattern
/// has a `*`. Discovery cannot list it and refuses it (`unreadable`); the
/// queue's re-match looks the unit's names up instead of listing, and must
/// refuse it as discovery does, leaving the unit to the tier's discovery
/// rather than deleting what discovery never finds. With an index seeded
/// as a recent snapshot would (the queue decides) and with an empty one
/// (the run's own tally and discovery decide), alike.
#[test]
fn a_unit_below_a_glob_directory_discovery_cannot_list_is_never_deleted() {
    let tree = format!(
        "  /tmp/p: {{.: {{mtime: {OLD}, mode: \"0333\"}}, f: {{size: 10K, mtime: {OLD}}}}}\n  \
         /tmp/q: {{.: {{mtime: {OLD}}}, f: {{size: 10K, mtime: {OLD}}}}}\n"
    );
    for seed in [false, true] {
        let host = host(&yaml(&tree, ""));
        let tier = tier("profile:two-level-any");
        let mut state = if seed {
            seeded(&host, &finder(&tier, "/tmp"), &["/tmp/p/f", "/tmp/q/f"])
        } else {
            TallyState::new(Index::new(BUDGET_BYTES))
        };
        let (report, _) = run(&host, &request(tier), &mut state);
        assert_eq!(actions(&report), ["/tmp/q/f"], "seed {seed}: {report:#?}");
        assert_eq!(
            skip_of(&report, "/tmp/p"),
            Some(Reason::Unreadable),
            "seed {seed}: {report:#?}"
        );
        assert!(exists(&host, "/tmp/p/f"), "seed {seed}");
        assert!(
            !host
                .trace()
                .iter()
                .any(|c| c.op == Op::Unlinkat && c.path.as_deref() == Some(Path::new("/tmp/p/f"))),
            "seed {seed}"
        );
        // The queue opened `/tmp/p` as discovery's listing would (refused),
        // and `/tmp` (a `*` level too); it listed neither.
        let probes: Vec<(String, String)> = host
            .trace()
            .into_iter()
            .filter(|c| c.op == Op::Listable)
            .map(|c| (c.path.unwrap().display().to_string(), c.result))
            .collect();
        if seed {
            assert!(
                probes.contains(&("/tmp/p".to_owned(), "EACCES (13)".to_owned())),
                "{probes:?}"
            );
            assert!(
                probes.iter().any(|(p, r)| p == "/tmp" && r == "ok"),
                "{probes:?}"
            );
        } else {
            // The run's own tally could not list `/tmp/p`: `/tmp/p/f` was
            // never queued.
            assert!(probes.iter().all(|(p, _)| p != "/tmp/p"), "{probes:?}");
        }
    }

    // A failure of the host on that open is an error of the run, as on
    // discovery's listing: nothing deleted, exit 3.
    let events =
        "events:\n  - {at: {op: listable, path: /tmp/p, nth: 1, when: before}, fail: EIO}\n";
    let host = host(&yaml(&tree, events));
    let tier = tier("profile:two-level-any");
    let mut state = seeded(&host, &finder(&tier, "/tmp"), &["/tmp/p/f"]);
    let (report, _) = run(&host, &request(tier), &mut state);
    assert!(host.unfired_events().is_empty());
    assert!(report.actions.is_empty(), "{report:#?}");
    assert_eq!(report.exit_code(), 3, "{report:#?}");
    assert!(
        report.errors.iter().any(|e| e.message.contains("/tmp/p")),
        "{report:#?}"
    );
}

/// TASK-252 (Codex review of TASK-31, finding 2): `/tmp/z/f` is indexed;
/// later a hard link `/tmp/k/f` to it appears in `/tmp/k`, which holds a
/// `.diskgc-keep`. Discovery meets `k/f` first (names sorted), finds the
/// inode once, and refuses the kept candidate; `z/f` is a second name of
/// it, not a candidate. The queue's re-match sees only `z/f`: a multiply
/// linked file is left to the tier's discovery, so its verdict is
/// discovery's. The converse order (`a/f` indexed and found first, its
/// kept alias `k/f` after it) is deleted by discovery itself: the alias
/// rule only picks which name discovery reports, and removing one name of
/// a file that keeps another removes no data (docs/safety.md, Hard
/// links).
#[test]
fn a_multiply_linked_unit_gets_discoverys_verdict_not_its_own_name() {
    let tree = format!(
        "  /tmp/k: {{.: {{mtime: {OLD}}}, .diskgc-keep: {{mtime: {OLD}}}, f: {{link: /tmp/z/f}}}}\n  \
         /tmp/z: {{.: {{mtime: {OLD}}}, f: {{size: 10K, mtime: {OLD}}}}}\n"
    );
    for seed in [false, true] {
        let host = host(&yaml(&tree, ""));
        let tier = tier("profile:two-level-any");
        let mut state = if seed {
            seeded(&host, &finder(&tier, "/tmp"), &["/tmp/z/f"])
        } else {
            TallyState::new(Index::new(BUDGET_BYTES))
        };
        let (report, _) = run(&host, &request(tier), &mut state);
        assert!(report.actions.is_empty(), "seed {seed}: {report:#?}");
        assert_eq!(
            skip_of(&report, "/tmp/k/f"),
            Some(Reason::KeepMarker),
            "seed {seed}: {report:#?}"
        );
        assert_eq!(
            skip_of(&report, "/tmp/z/f"),
            None,
            "seed {seed}: {report:#?}"
        );
        assert!(exists(&host, "/tmp/z/f") && exists(&host, "/tmp/k/f"));
        assert_eq!(host.trace_count(Op::Unlinkat), 0, "seed {seed}");
    }

    // The converse: discovery finds the eligible name first and deletes
    // it; the queue, left to discovery, does the same.
    let tree = format!(
        "  /tmp/a: {{.: {{mtime: {OLD}}}, f: {{size: 10K, mtime: {OLD}}}}}\n  \
         /tmp/k: {{.: {{mtime: {OLD}}}, .diskgc-keep: {{mtime: {OLD}}}, f: {{link: /tmp/a/f}}}}\n"
    );
    for seed in [false, true] {
        let host = host(&yaml(&tree, ""));
        let tier = tier("profile:two-level-any");
        let mut state = if seed {
            seeded(&host, &finder(&tier, "/tmp"), &["/tmp/a/f"])
        } else {
            TallyState::new(Index::new(BUDGET_BYTES))
        };
        let (report, _) = run(&host, &request(tier), &mut state);
        assert_eq!(actions(&report), ["/tmp/a/f"], "seed {seed}: {report:#?}");
        assert_eq!(skip_of(&report, "/tmp/k/f"), None, "seed {seed}");
        assert!(exists(&host, "/tmp/k/f"), "the kept name stays");
    }
}

/// TASK-252, finding 2 for a block: `/tmp/z/f-1` and `/tmp/z/f-2` are
/// collapsed into a family block at `/tmp/z`; a kept hard link
/// `/tmp/k/f-1` to the first appears. Expanding the block lists `/tmp/z`
/// only; its multiply linked member is left to the tier's discovery, which
/// refuses it under its kept name, while the other member is deleted.
#[test]
fn a_multiply_linked_block_member_gets_discoverys_verdict() {
    let tree = format!(
        "  /tmp/k: {{.: {{mtime: {OLD}}}, .diskgc-keep: {{mtime: {OLD}}}, f-1: {{link: /tmp/z/f-1}}}}\n  \
         /tmp/z: {{.: {{mtime: {OLD}}}, f-1: {{size: 10K, mtime: {OLD}}}, f-2: {{size: 10K, mtime: {OLD}}}}}\n"
    );
    let host = host(&yaml(&tree, ""));
    let tier = tier("profile:two-level-any");
    let mut state = seeded(&host, &finder(&tier, "/tmp"), &["/tmp/z/f-1", "/tmp/z/f-2"]);
    let before = state.index.accounted_bytes();
    state
        .index
        .collapse_to(&host, before - 1)
        .expect("collapsed");
    assert_eq!(
        blocks(&state),
        [(PathBuf::from("/tmp/z"), 2, false)],
        "one family block"
    );
    let (report, lines) = run(&host, &request(tier), &mut state);
    assert!(
        lines.iter().any(|l| l.contains(
            "block at /tmp/z (2 recorded member(s)) expanded: 1 member(s) found, 1 multiply \
                 linked left to discovery"
        )),
        "{lines:#?}"
    );
    assert_eq!(actions(&report), ["/tmp/z/f-2"], "{report:#?}");
    assert_eq!(
        skip_of(&report, "/tmp/k/f-1"),
        Some(Reason::KeepMarker),
        "{report:#?}"
    );
    assert!(exists(&host, "/tmp/z/f-1") && exists(&host, "/tmp/k/f-1"));
}

/// TASK-252: a block whose every member is multiply linked queues none of
/// them, but it is not empty, so it stays in the index (an empty one is
/// evicted); the tier's discovery judges them (kept).
#[test]
fn a_block_of_only_multiply_linked_members_stays_in_the_index() {
    let tree = format!(
        "  /tmp/k: {{.: {{mtime: {OLD}}}, .diskgc-keep: {{mtime: {OLD}}}, f-1: {{link: /tmp/z/f-1}}, \
         f-2: {{link: /tmp/z/f-2}}}}\n  \
         /tmp/z: {{.: {{mtime: {OLD}}}, f-1: {{size: 10K, mtime: {OLD}}}, f-2: {{size: 10K, mtime: {OLD}}}}}\n"
    );
    let host = host(&yaml(&tree, ""));
    let tier = tier("profile:two-level-any");
    let mut state = seeded(&host, &finder(&tier, "/tmp"), &["/tmp/z/f-1", "/tmp/z/f-2"]);
    let before = state.index.accounted_bytes();
    state
        .index
        .collapse_to(&host, before - 1)
        .expect("collapsed");
    let (report, lines) = run(&host, &request(tier), &mut state);
    assert!(
        lines.iter().any(|l| l.contains(
            "block at /tmp/z (2 recorded member(s)) expanded: 0 member(s) found, 2 multiply \
             linked left to discovery"
        )),
        "{lines:#?}"
    );
    assert!(report.actions.is_empty(), "{report:#?}");
    assert_eq!(
        blocks(&state),
        [(PathBuf::from("/tmp/z"), 2, false)],
        "kept in the index"
    );
}

/// TASK-252 (found by the index-authority property once it generated
/// unreadable directories): the run's tally sizes an unsized unit
/// `/tmp/p/d` below the unreadable `/tmp/p` (reached by name, as a
/// snapshot's unit is) and refuses it, `d` being unreadable too (under
/// `oldest` the walk's deferred sizing does, TASK-251: the same tally). A refusal
/// of this run's tally skips the candidate re-matched at its path; this
/// unit's re-match is refused on the way, so it is left to the tier's
/// discovery, which reports `/tmp/p` only, as a run without the unit does.
#[test]
fn a_tally_refusal_of_a_unit_discovery_cannot_reach_is_not_reported() {
    let tree = format!(
        "  /tmp/p: {{.: {{mtime: {OLD}, mode: \"0333\"}}, d: {{.: {{mtime: {OLD}, mode: \"0333\"}}, \
         x: {{size: 1K, mtime: {OLD}}}}}}}\n"
    );
    let host = host(&yaml(&tree, ""));
    let tier = tier("profile:two-level-any");
    let right = finder(&tier, "/tmp");
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    state
        .index
        .insert(
            &host,
            &tmp_mount(),
            Path::new("/tmp/p/d"),
            &right,
            discovered(&host, "/tmp/p/d"),
        )
        .expect("inserted");
    state.set_discovered_at(right, host.now());
    let (report, lines) = run(&host, &request(tier), &mut state);
    assert!(
        lines.iter().any(|l| l.contains("1 unit(s) from the index"))
            && lines
                .iter()
                .any(|l| l.contains("0 unit(s) sized as the queue reached them, 1 refused")),
        "the walk's deferred sizing sized and refused the unit: {lines:#?}"
    );
    assert!(report.actions.is_empty(), "{report:#?}");
    assert_eq!(skip_of(&report, "/tmp/p"), Some(Reason::Unreadable));
    assert_eq!(skip_of(&report, "/tmp/p/d"), None, "{report:#?}");
    assert!(exists(&host, "/tmp/p/d/x"));
}

/// TASK-252 review (Codex, ce91a5b): `/tmp/b` is a bind mount of `/tmp`'s
/// filesystem, and a tier's two roots `/tmp/z/a` and `/tmp/b/z/a/sub`
/// reach one file, `/tmp/z/a/sub/f` (link count 1), under two spellings.
/// Discovery walks the outer root first, records `/tmp/z/a/sub/f`, finds
/// the bind spelling again (the same inode) and refuses the recorded one:
/// `/tmp/z/a` holds a `.diskgc-keep`. A unit at the bind spelling, re-matched
/// from its own root alone, would start its checks below the marker, and
/// unlinking that spelling removes the very same directory entry. A tier
/// whose roots lie on two mounts of one filesystem takes no queue: full
/// discovery decides, with or without an index.
#[test]
fn a_tier_with_roots_on_two_mounts_of_one_filesystem_takes_no_queue() {
    let tree = format!(
        "  /tmp/z/a: {{.: {{mtime: {OLD}}}, .diskgc-keep: {{mtime: {OLD}}}, sub: {{.: {{mtime: {OLD}}}, \
         f: {{size: 10K, mtime: {OLD}}}}}}}\n  /tmp/b: {{.: {{}}}}\n"
    );
    let model = yaml(&tree, "").replace(
        "  - {id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, other_used: 500M, parent: 1}\n",
        "  - {id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, other_used: 500M, parent: 1}\n  \
         - {id: 31, at: /tmp/b, fs: tmpfs, dev: \"0:42\", size: 1G, other_used: 500M, parent: 30, \
         bind_of: 30}\n",
    );
    let mut logs = Vec::new();
    for seed in [false, true] {
        let host = host(&model);
        assert_eq!(
            host.stat_path(Path::new("/tmp/z/a/sub/f"))
                .and_then(|s| s.dev_ino()),
            host.stat_path(Path::new("/tmp/b/z/a/sub/f"))
                .and_then(|s| s.dev_ino()),
            "one file, two spellings"
        );
        let tier = tier("profile:bind-view");
        let outer = finder(&tier, "/tmp/z/a");
        let view = finder(&tier, "/tmp/b/z/a/sub");
        let mut state = TallyState::new(Index::new(BUDGET_BYTES));
        if seed {
            // A partial index: the bind spelling only, both roots listed
            // recently, so the run's tally adds nothing.
            let bind = MountKey {
                mount_point: PathBuf::from("/tmp/b"),
                ..tmp_mount()
            };
            state
                .index
                .insert(
                    &host,
                    &bind,
                    Path::new("/tmp/b/z/a/sub/f"),
                    &view,
                    discovered(&host, "/tmp/b/z/a/sub/f"),
                )
                .expect("inserted");
            state.set_discovered_at(outer, host.now());
            state.set_discovered_at(view, host.now());
        }
        let (report, lines) = run(&host, &request(tier), &mut state);
        assert!(report.actions.is_empty(), "seed {seed}: {report:#?}");
        assert_eq!(
            skip_of(&report, "/tmp/z/a/sub/f"),
            Some(Reason::KeepMarker),
            "seed {seed}: {report:#?}"
        );
        assert!(exists(&host, "/tmp/z/a/sub/f"), "seed {seed}");
        assert_eq!(host.trace_count(Op::Unlinkat), 0, "seed {seed}");
        logs.push(lines);
    }
    for lines in logs {
        assert!(
            lines
                .iter()
                .any(|l| l.contains("two mounts of one filesystem")),
            "the queue is not taken: {lines:#?}"
        );
    }
}

/// TASK-269, the index path: a `protect` match the run enumerated is gone
/// (a protected tree cleaned meanwhile) by the time the reclaim walk takes
/// a candidate from the index's queue. Its identity cannot be proven any
/// more, which says nothing about the candidate: the first overlap screen
/// enumerates the matches again and judges against those, as every later
/// check does (TASK-200), instead of stopping the run (exit 3).
#[test]
fn a_protected_match_removed_before_the_first_screen_from_the_index_does_not_stop_the_run() {
    let tree = format!(
        "  /tmp/a-plain: {d}\n  /tmp/stash: {{.: {{mtime: {OLD}}}, precious: {{size: 1K, mtime: {OLD}}}}}\n",
        d = old_dir()
    );
    // The enumeration's statx of the match, under the host lock, is the
    // last call before the queue's first candidate is screened.
    let events = "events:\n  - {at: {op: statx, path: /tmp/stash/precious, nth: 1, when: after}, \
                  remove: /tmp/stash/precious}\n";
    let host = host(&yaml(&tree, events));
    let mut tier = tier("profile:tmp-a && filter:idle-1d");
    tier.protections.push(ProtectedSource {
        profile: "precious".to_owned(),
        roots: vec![PathBuf::from("/tmp")],
        kind: ProtectedKind::Fs(Box::new(Find::new(
            &["*/precious".to_owned()],
            EntryType::Any,
            &Where::default(),
            1000,
        ))),
    });
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let (report, lines) = run(&host, &request(tier), &mut state);
    assert!(host.unfired_events().is_empty());
    assert!(report.errors.is_empty(), "{report:#?}");
    assert_eq!(actions(&report), ["/tmp/a-plain"], "{report:#?}");
    assert!(
        from_queue(&lines, "/tmp/a-plain"),
        "judged from the index's queue"
    );
}

/// Listings of `/tmp` in a run whose `protect` match goes right after the
/// tier enumerated it, with `n` old candidates from the index's queue.
fn tmp_listings_after_a_removed_match(n: usize) -> usize {
    let mut tree =
        format!("  /tmp/stash: {{.: {{mtime: {OLD}}}, precious: {{size: 1K, mtime: {OLD}}}}}\n");
    for i in 0..n {
        tree.push_str(&format!("  /tmp/a-{i}: {}\n", old_dir()));
    }
    let events = "events:\n  - {at: {op: statx, path: /tmp/stash/precious, nth: 1, when: after}, \
                  remove: /tmp/stash/precious}\n";
    let host = host(&yaml(&tree, events));
    let mut tier = tier("profile:tmp-a && filter:idle-1d");
    tier.protections.push(ProtectedSource {
        profile: "precious".to_owned(),
        roots: vec![PathBuf::from("/tmp")],
        kind: ProtectedKind::Fs(Box::new(Find::new(
            &["*/precious".to_owned()],
            EntryType::Any,
            &Where::default(),
            1000,
        ))),
    });
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let (report, _) = run(&host, &request(tier), &mut state);
    assert!(host.unfired_events().is_empty());
    assert!(report.errors.is_empty(), "{report:#?}");
    assert_eq!(actions(&report).len(), n, "{report:#?}");
    host.trace()
        .iter()
        .filter(|call| call.op == Op::ReadDir && call.path.as_deref() == Some(Path::new("/tmp")))
        .count()
}

/// TASK-269 review (mped S1): the matches a check enumerated again replace
/// the tier's, so one removed match costs one enumeration again, not one
/// (or three) per later candidate.
#[test]
fn one_removed_protect_match_costs_one_enumeration_not_one_per_candidate() {
    let two = tmp_listings_after_a_removed_match(2);
    let five = tmp_listings_after_a_removed_match(5);
    assert_eq!(
        two, five,
        "listings of /tmp: 2 candidates {two}, 5 candidates {five}"
    );
}

/// TASK-251: four candidates whose roots' times put them in one order and
/// whose trees' newest changes in another (`/tmp/a-1` and `/tmp/a-3` hold a
/// change newer than their root, `a-3` two levels down). Under `oldest` the
/// synchronous tally leaves their sizing to the walk, which sizes each as
/// the queue reaches it: the reclaim still takes them in the order of their
/// sized `newest_change`, the order the dry run's plan (which judges every
/// candidate in full) gives, not the order of their roots.
#[test]
fn deferred_sizing_takes_the_units_in_the_order_of_their_sized_keys() {
    let day = |d: u32| format!("2026-10-0{d}T00:00:00Z");
    let tree = format!(
        "  /tmp/a-1: {{.: {{mtime: {t1}}}, f: {{size: 10K, mtime: {t5}}}}}\n  \
         /tmp/a-2: {{.: {{mtime: {t2}}}, f: {{size: 10K, mtime: {t2}}}}}\n  \
         /tmp/a-3: {{.: {{mtime: {t3}}}, deep: {{.: {{mtime: {t3}}}, g: {{size: 10K, mtime: {t6}}}}}}}\n  \
         /tmp/a-4: {{.: {{mtime: {t4}}}, f: {{size: 10K, mtime: {t4}}}}}\n",
        t1 = day(1),
        t2 = day(2),
        t3 = day(3),
        t4 = day(4),
        t5 = day(5),
        t6 = day(6),
    );
    let want = ["/tmp/a-2", "/tmp/a-4", "/tmp/a-1", "/tmp/a-3"];
    let planned = host(&yaml(&tree, ""));
    let uid = executor::privileges(&planned, false).unwrap_or_else(|e| panic!("{e}"));
    let plan = executor::dry_run(
        &planned,
        &request(tier("profile:tmp-a")),
        &[uid],
        &mut |_| {},
    );
    let plan: Vec<String> = plan
        .plan
        .iter()
        .filter_map(|p| p.candidate.as_ref().map(|c| c.display().to_string()))
        .collect();
    assert_eq!(plan, want, "the dry run's order");
    let host = host(&yaml(&tree, ""));
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let (report, lines) = run(&host, &request(tier("profile:tmp-a")), &mut state);
    assert!(
        lines
            .iter()
            .any(|l| l
                .contains("sizing of the first tier's 4 stale unit(s) left to the reclaim walk")),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("4 unit(s) sized as the queue reached them")),
        "{lines:#?}"
    );
    assert_eq!(actions(&report), want, "{report:#?}");
    assert!(!report.tally_cut_short);
}

/// TASK-251: a hundred old candidates and a goal one candidate meets. The
/// walk sizes the units its queue reaches, the 64 it finds ahead of
/// judging the first (`FOUND_AHEAD_MAX`), and deletes one; the other 36
/// are never sized in this run (the synchronous tally sized none).
#[test]
fn deferred_sizing_sizes_only_what_the_queue_reaches() {
    let mut tree = String::new();
    for n in 0..100 {
        tree.push_str(&format!("  /tmp/a-{n:03}: {}\n", old_dir()));
    }
    let measured = host(&yaml(&tree, ""));
    let uid = executor::privileges(&measured, false).unwrap_or_else(|e| panic!("{e}"));
    let avail = executor::dry_run(
        &measured,
        &request(tier("profile:tmp-a")),
        &[uid],
        &mut |_| {},
    )
    .avail_before_bytes
    .expect("measured");
    let host = host(&yaml(&tree, ""));
    let mut request = request(tier("profile:tmp-a"));
    request.min_free = Size::Bytes(avail + (5 << 10));
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let (report, lines) = run(&host, &request, &mut state);
    assert_eq!(actions(&report), ["/tmp/a-000"], "{report:#?}");
    assert!(
        lines.iter().any(
            |l| l.contains("64 unit(s) sized as the queue reached them, 0 refused")
                && l.contains("(64 of the first tier's 100 stale unit(s) so far)")
        ),
        "{lines:#?}"
    );
    let mut sized = 0;
    let mut not_sized = 0;
    state.index.for_each_unit(|_, _, unit| match unit {
        UnitRef::Single(s) if s.tally.is_some() => sized += 1,
        UnitRef::Single(_) => not_sized += 1,
        UnitRef::Block(_) => panic!("no blocks"),
    });
    assert_eq!((sized, not_sized), (63, 36));
}

/// The `--tally-budget` of [`deferred_sizing_stops_at_the_tally_budget`],
/// in model milliseconds (1 ms per port call): past the synchronous
/// tally's discovery of ten units (32 ms), short of their sizing (3 of
/// them fit when this was written).
const BUDGET_MS: u64 = 60;

/// TASK-251: the walk's deferred sizing spends what discovery left of
/// `--tally-budget`, so the tally's work stays within it. Every port call
/// takes 1 ms; the budget ends after a few of the ten units are sized.
/// The rest are taken by their stored keys (the same order here), every
/// candidate is reclaimed, and the report says the tally was cut short.
#[test]
fn deferred_sizing_stops_at_the_tally_budget() {
    let mut tree = String::new();
    for n in 0..10 {
        tree.push_str(&format!("  /tmp/a-{n}: {}\n", old_dir()));
    }
    let host = host(&yaml(&tree, "call_cost: 1ms\n"));
    let mut request = request(tier("profile:tmp-a"));
    request.tally_budget = Some(Duration::from_millis(BUDGET_MS));
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let (report, lines) = run(&host, &request, &mut state);
    assert!(
        lines.iter().any(|l| l.contains("index: synchronous tally")
            && l.contains("left to the reclaim walk")
            && !l.contains("stopped")),
        "discovery fits the budget: {lines:#?}"
    );
    let sized = lines
        .iter()
        .find(|l| l.contains("--tally-budget ended: the rest by stored keys"))
        .unwrap_or_else(|| panic!("the budget ends the deferred sizing: {lines:#?}"));
    assert!(
        !sized.contains(" 0 unit(s) sized") && !sized.contains("10 unit(s) sized"),
        "some, not all: {sized}"
    );
    assert!(report.tally_cut_short, "{report:#?}");
    let mut deleted = actions(&report);
    deleted.sort();
    assert_eq!(deleted.len(), 10, "{report:#?}");
}

/// TASK-251: deferred sizing beside an expanded block. A family block at
/// `/tmp/p` stands for `a-1` (changed day 2) and `a-2` (day 4); the singles
/// `/tmp/q/a-3` (its root from day 1, a change inside from day 3) and
/// `/tmp/q/a-4` (day 5) were never sized. The walk sizes `a-3` before the
/// block (its stored key, day 1, comes first), expands the block at its
/// oldest member's key, and merges members and sized units by key: the
/// order of the dry run's plan, a-1, a-3, a-2, a-4. `a-4` is sized only
/// once no member comes before its stored key.
#[test]
fn deferred_sizing_merges_with_an_expanded_blocks_members() {
    let day = |d: u32| format!("2026-10-0{d}T00:00:00Z");
    let dir = |d: u32| {
        format!(
            "{{.: {{mtime: {t}}}, f: {{size: 10K, mtime: {t}}}}}",
            t = day(d)
        )
    };
    let tree = format!(
        "  /tmp/p/a-1: {a1}\n  /tmp/p/a-2: {a2}\n  \
         /tmp/q/a-3: {{.: {{mtime: {t1}}}, f: {{size: 10K, mtime: {t3}}}}}\n  /tmp/q/a-4: {a4}\n",
        a1 = dir(2),
        a2 = dir(4),
        a4 = dir(5),
        t1 = day(1),
        t3 = day(3),
    );
    let want = ["/tmp/p/a-1", "/tmp/q/a-3", "/tmp/p/a-2", "/tmp/q/a-4"];
    let planned = host(&yaml(&tree, ""));
    let uid = executor::privileges(&planned, false).unwrap_or_else(|e| panic!("{e}"));
    let plan = executor::dry_run(
        &planned,
        &request(tier("profile:two-level-a")),
        &[uid],
        &mut |_| {},
    );
    let plan: Vec<String> = plan
        .plan
        .iter()
        .filter_map(|p| p.candidate.as_ref().map(|c| c.display().to_string()))
        .collect();
    assert_eq!(plan, want, "the dry run's order");

    let host = host(&yaml(&tree, ""));
    let tier = tier("profile:two-level-a");
    let right = finder(&tier, "/tmp");
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    put(&host, &mut state, &right, &["/tmp/p/a-1", "/tmp/p/a-2"]);
    let before = state.index.accounted_bytes();
    state
        .index
        .collapse_to(&host, before - 1)
        .expect("collapsed");
    assert_eq!(blocks(&state), [(PathBuf::from("/tmp/p"), 2, false)]);
    for path in ["/tmp/q/a-3", "/tmp/q/a-4"] {
        state
            .index
            .insert(
                &host,
                &tmp_mount(),
                Path::new(path),
                &right,
                discovered(&host, path),
            )
            .expect("inserted");
    }
    state.set_discovered_at(right, host.now());
    let (report, lines) = run(&host, &request(tier), &mut state);
    assert!(
        lines
            .iter()
            .any(|l| l
                .contains("sizing of the first tier's 2 stale unit(s) left to the reclaim walk")),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("block at /tmp/p (2 recorded member(s)) expanded: 2")),
        "{lines:#?}"
    );
    assert_eq!(actions(&report), want, "{report:#?}");
}

/// The plan order of `select` over `tree` (a dry run judges every candidate
/// in full).
fn plan_order(tree: &str, select: &str) -> Vec<String> {
    let planned = host(&yaml(tree, ""));
    let uid = executor::privileges(&planned, false).unwrap_or_else(|e| panic!("{e}"));
    executor::dry_run(&planned, &request(tier(select)), &[uid], &mut |_| {})
        .plan
        .iter()
        .filter_map(|p| p.candidate.as_ref().map(|c| c.display().to_string()))
        .collect()
}

/// `/tmp` available space before any action, for a goal of `extra` bytes.
fn avail(tree: &str, select: &str) -> u64 {
    let measured = host(&yaml(tree, ""));
    let uid = executor::privileges(&measured, false).unwrap_or_else(|e| panic!("{e}"));
    executor::dry_run(&measured, &request(tier(select)), &[uid], &mut |_| {})
        .avail_before_bytes
        .expect("measured")
}

/// Codex review of TASK-251, finding 1: a key an earlier run stored is no
/// lower bound. `/tmp/a-1` changed last on 10-02 (its root), `/tmp/a-2` on
/// 10-05, but the index holds 10-20 for a-1: a snapshot's tally saw a
/// future-dated file there, since removed (`tallied`), or a snapshot's
/// listing saw a root mtime since set back, a listing this run does not
/// repeat (its source was listed less than five minutes before). Full
/// sizing takes a-1 first; sizing as the queue reaches them must too, and
/// with a goal one candidate meets, delete a-1, not a-2. The walk reads
/// such a unit's root again before it trusts its key as a bound.
fn stored_key_case(tallied: bool) {
    let tree = "  /tmp/a-1: {.: {mtime: 2026-10-02T00:00:00Z}, f: {size: 10K, mtime: 2026-10-01T00:00:00Z}}\n  \
         /tmp/a-2: {.: {mtime: 2026-10-05T00:00:00Z}, f: {size: 10K, mtime: 2026-10-05T00:00:00Z}}\n"
        .to_owned();
    assert_eq!(plan_order(&tree, "profile:tmp-a"), ["/tmp/a-1", "/tmp/a-2"]);
    let host = host(&yaml(&tree, ""));
    let tier = tier("profile:tmp-a");
    let source = finder(&tier, "/tmp");
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    // Stored two hours before the run (tallied: stale).
    let then = host.now();
    for (path, stored) in [("/tmp/a-1", "2026-10-20"), ("/tmp/a-2", "2026-10-05")] {
        state
            .index
            .insert(
                &host,
                &tmp_mount(),
                Path::new(path),
                &source,
                discovered(&host, path),
            )
            .expect("inserted");
        let single = state
            .index
            .single_mut(&tmp_mount(), Path::new(path))
            .expect("a single");
        single.newest_change = Timestamp {
            secs: chrono_secs(stored),
            nanos: 0,
        };
        if tallied {
            single.tally = Some(Tally {
                est_bytes: 10 << 10,
                files: 2,
                tallied_at: then,
            });
        }
    }
    host.sleep(Duration::from_secs(2 * 3600));
    if !tallied {
        state.set_discovered_at(source, host.now());
    }
    let mut request = request(tier);
    request.min_free = Size::Bytes(avail(&tree, "profile:tmp-a") + (5 << 10));
    let (report, lines) = run(&host, &request, &mut state);
    assert_eq!(actions(&report), ["/tmp/a-1"], "{report:#?}");
    assert!(exists(&host, "/tmp/a-2/f"));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("2 root(s) read again for their keys")),
        "{lines:#?}"
    );
}

#[test]
fn deferred_sizing_does_not_trust_a_snapshots_stale_tally_key_as_a_bound() {
    stored_key_case(true);
}

#[test]
fn deferred_sizing_does_not_trust_a_snapshots_listing_key_as_a_bound() {
    stored_key_case(false);
}

/// Seconds since the epoch of midnight UTC on `day` (YYYY-MM-DD, 2026-10).
fn chrono_secs(day: &str) -> i64 {
    let d: i64 = day[8..10].parse().expect("a day");
    // 2026-10-01T00:00:00Z
    1_790_812_800 + (d - 1) * 86_400
}

/// Four candidates whose roots' times order them a-1, a-2, a-3, a-4 and
/// whose trees' newest changes a-2, a-4, a-1, a-3 (the latter is the
/// order full sizing gives).
fn reordered_tree() -> String {
    let day = |d: u32| format!("2026-10-0{d}T00:00:00Z");
    format!(
        "  /tmp/a-1: {{.: {{mtime: {t1}}}, f: {{size: 10K, mtime: {t5}}}}}\n  \
         /tmp/a-2: {{.: {{mtime: {t2}}}, f: {{size: 10K, mtime: {t2}}}}}\n  \
         /tmp/a-3: {{.: {{mtime: {t3}}}, deep: {{.: {{mtime: {t3}}}, g: {{size: 10K, mtime: {t6}}}}}}}\n  \
         /tmp/a-4: {{.: {{mtime: {t4}}}, f: {{size: 10K, mtime: {t4}}}}}\n",
        t1 = day(1),
        t2 = day(2),
        t3 = day(3),
        t4 = day(4),
        t5 = day(5),
        t6 = day(6),
    )
}

/// mped review of TASK-251 (S1): preflight drops a tier's root on another
/// filesystem than PATH's (`/var/tmp` on `/`, for `ensure /tmp`), so the
/// walk's tier has fewer sources than the tally's. It still takes the
/// deferred sizing (its sources are among the tally's): the reclaim keeps
/// the order of the sized keys, and no line says the deferral went unused.
#[test]
fn deferred_sizing_survives_a_root_preflight_drops() {
    let tree = format!("{}  /var/tmp: {{.: {{}}}}\n", reordered_tree());
    let want = ["/tmp/a-2", "/tmp/a-4", "/tmp/a-1", "/tmp/a-3"];
    let plan = plan_order(&tree, "profile:two-roots-a");
    assert_eq!(
        plan.last().map(String::as_str),
        Some("/var/tmp"),
        "its other_fs row"
    );
    assert_eq!(plan[..4], want, "the dry run's order");
    let host = host(&yaml(&tree, ""));
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let (report, lines) = run(&host, &request(tier("profile:two-roots-a")), &mut state);
    assert_eq!(actions(&report), want, "{report:#?}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("4 unit(s) sized as the queue reached them")),
        "{lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("no tier took")),
        "{lines:#?}"
    );
}

/// A deferral no tier takes is said in one line: here someone else frees
/// the space while the run waits for the host lock, so no tier runs.
#[test]
fn an_unused_deferral_is_logged() {
    let tree = reordered_tree();
    let events =
        "events:\n  - {at: {op: flock, path: /run/diskgc/lock, when: after}, remove: /tmp/a-4}\n";
    let host = host(&yaml(&tree, events));
    let mut request = request(tier("profile:tmp-a"));
    request.min_free = Size::Bytes(avail(&tree, "profile:tmp-a") + (5 << 10));
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let (report, lines) = run(&host, &request, &mut state);
    assert!(host.unfired_events().is_empty());
    assert!(report.actions.is_empty(), "{report:#?}");
    assert!(
        lines.iter().any(|l| l.contains(
            "index: no tier took the first tier's deferred sizing (4 stale unit(s) of tmp-a at /tmp"
        )),
        "{lines:#?}"
    );
}

/// Codex review of TASK-251, finding 3: the walk sizes the first tier as
/// its queue reaches it, 64 candidates ahead of judging them. An `EIO`
/// while sizing `/tmp/a-070`, in the second batch, comes after the first
/// 64 were deleted, each having passed every live check: an error of the
/// run (exit 3), nothing more deleted (docs/architecture.md, The reclaim
/// walk: a failure of the host during the deferred sizing).
#[test]
fn a_host_error_in_deferred_sizing_after_the_first_batch_stops_the_run() {
    let mut tree = String::new();
    for n in 0..100 {
        if n == 70 {
            tree.push_str(&format!(
                "  /tmp/a-070: {{.: {{mtime: {OLD}}}, sub: {{.: {{mtime: {OLD}}}, f: {{size: 1K, mtime: {OLD}}}}}}}\n"
            ));
        } else {
            tree.push_str(&format!("  /tmp/a-{n:03}: {}\n", old_dir()));
        }
    }
    let host = host(&yaml(
        &tree,
        "events:\n  - {at: {op: read_dir, path: /tmp/a-070/sub, nth: 1, when: before}, fail: EIO}\n",
    ));
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let (report, _) = run(&host, &request(tier("profile:tmp-a")), &mut state);
    assert!(host.unfired_events().is_empty());
    let want: Vec<String> = (0..64).map(|n| format!("/tmp/a-{n:03}")).collect();
    assert_eq!(actions(&report), want, "{report:#?}");
    assert_eq!(report.errors.len(), 1, "{report:#?}");
    assert!(
        report.errors[0].message.contains("sizing /tmp/a-070")
            && report.errors[0].message.contains("EIO"),
        "{report:#?}"
    );
    for n in 64..100 {
        assert!(exists(&host, &format!("/tmp/a-{n:03}")), "a-{n:03}");
    }
}

/// Codex review of TASK-251, finding 2: the deferred sizing's first
/// stretch reads the mount table, and that time is the tally's too. With
/// some 60 ms of `--tally-budget` left after discovery, a mount table read
/// that takes 100 ms (an event advances the clock right after it) leaves
/// nothing for sizing: the rest goes by stored keys and the run says the
/// tally was cut short. Charged only from after that read, the sizing
/// would have fit and the cut gone unreported.
#[test]
fn the_deferred_sizings_mount_table_read_is_charged_to_the_tally_budget() {
    let mut tree = String::new();
    for n in 0..3 {
        tree.push_str(&format!("  /tmp/a-{n}: {}\n", old_dir()));
    }
    let model = |extra: &str| host(&yaml(&tree, &format!("call_cost: 1ms\n{extra}")));
    let request = |budget: Duration| {
        let mut r = request(tier("profile:tmp-a"));
        r.tally_budget = Some(budget);
        r
    };
    // Probe: how long discovery takes, and which mount table read is the
    // deferred sizing's (the last before the first unit's tree is listed
    // under the host lock).
    let probe = model("");
    let (_, lines) = run(
        &probe,
        &request(Duration::from_secs(60)),
        &mut TallyState::new(Index::new(BUDGET_BYTES)),
    );
    let discovery: u64 = lines
        .iter()
        .find_map(|l| l.split("(discovery ").nth(1))
        .and_then(|rest| rest.split(' ').next())
        .and_then(|ms| ms.parse().ok())
        .unwrap_or_else(|| panic!("{lines:#?}"));
    let trace = probe.trace();
    let locked = trace
        .iter()
        .position(|c| c.op == Op::Flock && c.path.as_deref() == Some(Path::new("/run/diskgc/lock")))
        .expect("host lock");
    let sized = locked
        + trace[locked..]
            .iter()
            .position(|c| {
                c.op == Op::ReadDir && c.path.as_deref().is_some_and(|p| p.starts_with("/tmp/a-0"))
            })
            .expect("a unit sized");
    let nth = trace[..sized]
        .iter()
        .filter(|c| c.op == Op::Mountinfo)
        .count();
    let budget = Duration::from_millis(discovery + 60);

    // Without the slow read the sizing fits the budget.
    let (report, lines) = run(
        &model(""),
        &request(budget),
        &mut TallyState::new(Index::new(BUDGET_BYTES)),
    );
    assert!(!report.tally_cut_short, "{lines:#?}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("3 unit(s) sized as the queue reached them")),
        "{lines:#?}"
    );

    let slow = model(&format!(
        "events:\n  - {{at: {{op: mountinfo, nth: {nth}, when: after}}, advance: 100ms}}\n"
    ));
    let (report, lines) = run(
        &slow,
        &request(budget),
        &mut TallyState::new(Index::new(BUDGET_BYTES)),
    );
    assert!(slow.unfired_events().is_empty());
    assert!(
        lines
            .iter()
            .any(|l| l.contains("0 unit(s) sized as the queue reached them")
                && l.contains("--tally-budget ended")),
        "{lines:#?}"
    );
    assert!(report.tally_cut_short, "{report:#?}");
    assert_eq!(report.actions.len(), 3, "{report:#?}");
}
