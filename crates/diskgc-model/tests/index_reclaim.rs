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
use diskgc_core::engine::Reason;
use diskgc_core::executor::{self, ProtectedKind, ProtectedSource, Request, Tier};
use diskgc_core::fs_kind::Find;
use diskgc_core::host::{Dev, Host, MonoTime};
use diskgc_core::index::{
    BUDGET_BYTES, Discovered, Family, Finder, Identity, Index, MountKey, Refusal, Tally, UnitRef,
};
use diskgc_core::profile::{EntryType, Library, Where, parse_profiles};
use diskgc_core::report::Report;
use diskgc_core::snapshot::RunIndex;
use diskgc_core::tally::TallyState;
use diskgc_core::units::parse_size;
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

/// The same rule when the failure comes from the tally's sizing (pass 2)
/// rather than its listing: an `EIO` while sizing a stale unit ends the
/// tally with an error, and no tier takes a queue. The `EIO` is transient
/// here, so the tier's own discovery and each candidate's live walk read
/// fine and reclaim both candidates.
#[test]
fn a_host_error_in_the_tallys_sizing_keeps_the_tier_off_its_queue() {
    let tree = format!(
        "  /tmp/a-bad: {{.: {{mtime: {OLD}}}, sub: {{.: {{mtime: {OLD}}}, f: {{size: 1K, mtime: {OLD}}}}}}}\n  \
         /tmp/a-plain: {d}\n",
        d = old_dir()
    );
    let events = "events:\n  - {at: {op: read_dir, path: /tmp/a-bad/sub, nth: 1, when: before}, fail: EIO}\n";
    let host = host(&yaml(&tree, events));
    let mut state = TallyState::new(Index::new(BUDGET_BYTES));
    let (report, lines) = run(&host, &request(tier("profile:tmp-a")), &mut state);
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
