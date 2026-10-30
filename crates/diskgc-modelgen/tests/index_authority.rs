//! The index is never an authority (docs/testing.md, Property tests; TASK-31
//! AC #1): with a randomly stale, partial, collapsed (random budget, a
//! collapse to lumps) or corrupted index (units with wrong identities,
//! finders, roots or paths, paths that are gone, old refusal hints), a
//! reclaim deletes exactly what a run with a correct index deletes: nothing
//! a correct run refuses, nothing outside what its own discovery finds,
//! and (the model being still) nothing less. The correct run is `ensure`
//! from an empty index, whose synchronous tally builds the index (the
//! equivalent of `--no-index --tally-budget 0`).
//!
//! Refusal hints are generated with an old time or one at or after the
//! run's start (a clock step, a corrupt snapshot): only refusals the run's
//! own tally produced may skip a candidate for the run; a hint of either
//! time only ranks its unit last (Codex review of TASK-31, finding 3).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::engine::Reason;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::host::{Host, MonoTime};
use diskgc_core::index::{
    BUDGET_BYTES, Birth, Discovered, Finder, Identity, Index, MountKey, Refusal, Tally,
};
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::report::Report;
use diskgc_core::snapshot::RunIndex;
use diskgc_core::tally::TallyState;
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::{FileSpec, HostModel, NodeSpec, Op};
use diskgc_modelgen::{ROOT, host_model};
use proptest::prelude::*;

const PROFILES: &str = r#"
- name: prop-idx
  find: {roots: [/tmp/gen], paths: ["*a*", "*/*"], type: any}
  action: delete
  examples: {match: [/tmp/gen/a0, /tmp/gen/x/y]}
- name: prop-other
  find: {roots: [/tmp/gen], paths: ["*"], type: any}
  action: delete
  examples: {match: [/tmp/gen/x]}
- name: prop-keep-one
  where: {keep_newest: 1}
"#;

fn cases() -> u32 {
    std::env::var("DISKGC_PROPERTY_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64)
}

fn request(select: &str) -> Request {
    let specs = parse_profiles(PROFILES).expect("profiles");
    let library = Library::with_user_profiles(&specs).expect("library");
    Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size("100%").expect("size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        tier: Tier::select(select, &library, None, 1000).expect("tier"),
    }
}

/// Which finder a corrupted unit carries.
#[derive(Debug, Clone, Copy)]
enum Who {
    Right,
    OtherProfile,
    OtherHash,
    /// The tier's profile bound to another root (`/tmp`).
    OtherRoot,
}

/// Which identity it carries.
#[derive(Debug, Clone, Copy)]
enum Id {
    Right,
    WrongIno,
    WrongBirth,
}

/// One unit of the corrupted index: a path of the model (or one that does
/// not exist), with its finder, identity, a tally or not, a refusal hint
/// (old, or timed in the run) or not, and a stored change shifted to misorder it.
#[derive(Debug, Clone)]
struct Unit {
    path: usize,
    who: Who,
    id: Id,
    sized: bool,
    hint: Option<Reason>,
    /// The hint is timed at or after the run's start, not before it.
    hint_in_run: bool,
    shift_days: i64,
}

#[derive(Debug, Clone)]
struct Corruption {
    units: Vec<Unit>,
    /// The tier's root counts as listed recently: the tally adds nothing.
    listed: bool,
    /// A budget that forces collapses while inserting (`None`: the real one).
    budget: Option<usize>,
    /// Collapse to one lump per mount at the end.
    lumps: bool,
    /// The tier's selection ([`SELECTIONS`]).
    select: usize,
}

/// The tier's selection: plain, with an idle filter, and with a version
/// filter (`keep_newest`, a retention guard ranking a candidate among its
/// siblings: judged per candidate when it is reclaimed, so a corrupt
/// order must not change which siblings it keeps).
const SELECTIONS: [&str; 3] = [
    "profile:prop-idx",
    "profile:prop-idx && filter:idle-1d",
    "profile:prop-idx && filter:prop-keep-one",
];

fn unit() -> impl Strategy<Value = Unit> {
    (
        0usize..64,
        prop_oneof![
            6 => Just(Who::Right),
            1 => Just(Who::OtherProfile),
            1 => Just(Who::OtherHash),
            1 => Just(Who::OtherRoot)
        ],
        prop_oneof![5 => Just(Id::Right), 1 => Just(Id::WrongIno), 1 => Just(Id::WrongBirth)],
        any::<bool>(),
        prop_oneof![
            4 => Just(None),
            1 => Just(Some(Reason::KeepMarker)),
            1 => Just(Some(Reason::TooManyEntries))
        ],
        any::<bool>(),
        -40i64..40,
    )
        .prop_map(
            |(path, who, id, sized, hint, hint_in_run, shift_days)| Unit {
                path,
                who,
                id,
                sized,
                hint,
                hint_in_run,
                shift_days,
            },
        )
}

fn corruption() -> impl Strategy<Value = Corruption> {
    (
        proptest::collection::vec(unit(), 0..24),
        any::<bool>(),
        proptest::option::of(1024usize..6000),
        any::<bool>(),
        0..SELECTIONS.len(),
    )
        .prop_map(|(units, listed, budget, lumps, select)| Corruption {
            units,
            listed,
            budget,
            lumps,
            select,
        })
}

/// Every path of the model's tree below [`ROOT`] (directories, files,
/// links), and two that do not exist.
fn model_paths(model: &HostModel) -> Vec<PathBuf> {
    fn walk(at: &Path, node: &NodeSpec, out: &mut Vec<PathBuf>) {
        out.push(at.to_owned());
        if let NodeSpec::Dir(dir) = node {
            for (name, child) in &dir.entries {
                walk(&at.join(name), child, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(Path::new(ROOT), &model.tree[Path::new(ROOT)], &mut out);
    out.retain(|p| p != Path::new(ROOT));
    out.push(PathBuf::from(ROOT).join("zz-gone"));
    out.push(PathBuf::from(ROOT).join("a-gone/deeper"));
    out
}

/// The mount `path` lies on (the deepest mount point above it), from the
/// model's mount table.
fn mount_of(host: &ModelHost, path: &Path) -> MountKey {
    let mounts = host.mountinfo().expect("mountinfo");
    let entry = mounts
        .iter()
        .filter(|m| path.starts_with(&m.mount_point))
        .max_by_key(|m| m.mount_point.components().count())
        .expect("below /");
    MountKey::of(entry)
}

/// The corrupted index for `model` on `host` (whose calls the collapses
/// make; they are traced before the run starts).
fn corrupted(
    host: &ModelHost,
    model: &HostModel,
    tier: &Tier,
    corruption: &Corruption,
) -> TallyState {
    let paths = model_paths(model);
    let right = Finder {
        profile: tier.profile.as_str().into(),
        profile_hash: tier.profile_hash(),
        root: Path::new(ROOT).into(),
    };
    let mut state = TallyState::new(Index::new(corruption.budget.unwrap_or(BUDGET_BYTES)));
    // A stand-in identity for paths that do not exist: any object's.
    let fallback = host.stat_path(Path::new(ROOT)).expect("the root");
    for unit in &corruption.units {
        let path = &paths[unit.path % paths.len()];
        let mount = mount_of(host, path);
        if *path == mount.mount_point {
            continue; // a candidate is never a mount point
        }
        let finder = match unit.who {
            Who::Right => right.clone(),
            Who::OtherProfile => Finder {
                profile: "prop-other".into(),
                ..right.clone()
            },
            Who::OtherHash => Finder {
                profile_hash: right.profile_hash ^ 0x5a,
                ..right.clone()
            },
            Who::OtherRoot => Finder {
                root: Path::new("/tmp").into(),
                ..right.clone()
            },
        };
        let stat = host.stat_path(path).unwrap_or_else(|| fallback.clone());
        let parent_ino = path
            .parent()
            .and_then(|p| host.stat_path(p))
            .and_then(|s| s.ino)
            .unwrap_or(1);
        let Some(mut identity) = Identity::of(&stat, parent_ino) else {
            continue;
        };
        identity.dev = mount.dev;
        match unit.id {
            Id::Right => {}
            Id::WrongIno => identity.ino = identity.ino.wrapping_add(7919),
            Id::WrongBirth => {
                identity.birth = match identity.birth {
                    Birth::Time(mut t) => {
                        t.secs -= 3600;
                        Birth::Time(t)
                    }
                    Birth::ParentIno(ino) => Birth::ParentIno(ino ^ 1),
                }
            }
        }
        let Some(mut change) = stat.mtime else {
            continue;
        };
        change.secs += unit.shift_days * 86_400;
        let found = Discovered {
            identity,
            newest_change: change,
            checked_at: MonoTime(Duration::ZERO),
        };
        state
            .index
            .insert(host, &mount, path, &finder, found)
            .expect("insert");
        if let Some(single) = state.index.single_mut(&mount, path) {
            if unit.sized {
                single.tally = Some(Tally {
                    est_bytes: 4096 * (unit.path as u64 + 1),
                    files: 1,
                    tallied_at: host.now(),
                });
            }
            single.refusal = unit.hint.map(|reason| Refusal {
                reason,
                at: if unit.hint_in_run {
                    MonoTime(Duration::from_secs(1 << 40))
                } else {
                    MonoTime(Duration::ZERO)
                },
            });
        }
    }
    if corruption.lumps {
        state.index.collapse_to(host, 0).expect("collapse");
    }
    if corruption.listed {
        state.set_discovered_at(right, host.now());
    }
    state
}

/// `model` with the host lock provisioned (as the tmpfiles line makes it),
/// so that a reclaim runs.
fn provisioned(mut model: HostModel) -> HostModel {
    model.tree.insert(
        PathBuf::from("/run/diskgc/lock"),
        NodeSpec::File(FileSpec {
            uid: Some(0),
            gid: Some(0),
            ..FileSpec::default()
        }),
    );
    model
}

fn unlinked(host: &ModelHost) -> BTreeSet<PathBuf> {
    host.trace()
        .into_iter()
        .filter(|c| c.op == Op::Unlinkat && c.result == "ok")
        .filter_map(|c| c.path)
        .collect()
}

fn acted(report: &Report) -> BTreeSet<PathBuf> {
    report.actions.iter().map(|a| a.candidate.clone()).collect()
}

/// The skips other than `stale` (which only the corrupted index makes).
fn refused(report: &Report) -> BTreeSet<(PathBuf, &'static str)> {
    report
        .skipped
        .iter()
        .filter(|s| s.reason != Reason::Stale)
        .filter_map(|s| s.candidate.clone().map(|c| (c, s.reason.as_str())))
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: cases(),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn the_index_is_never_an_authority(
        (model, corruption) in host_model(3, 6, 1).prop_flat_map(|m| (Just(m), corruption()))
    ) {
        let model = provisioned(model);
        let request = request(SELECTIONS[corruption.select]);
        let correct_host = ModelHost::new(&model).expect("model loads");
        let correct = executor::ensure(&correct_host, &request, &[1000], &mut |_| {});
        let host = ModelHost::new(&model).expect("model loads");
        let mut index = RunIndex::of(corrupted(&host, &model, &request.tier, &corruption));
        let mut lines = Vec::new();
        let report = executor::ensure_with_index(
            &host, &request, &[1000], &mut index, &mut |l| lines.push(l.to_owned()),
        );
        let explain = || format!(
            "{}\ncorruption: {corruption:#?}\ncorrect: {correct:#?}\ncorrupted: {report:#?}\nlog:\n  {}",
            model.to_yaml(),
            lines.join("\n  ")
        );
        // Nothing a correct run refuses or does not find is deleted, and
        // no entry is unlinked that the correct run did not unlink.
        prop_assert!(acted(&report).is_subset(&acted(&correct)), "{}", explain());
        prop_assert!(unlinked(&host).is_subset(&unlinked(&correct_host)), "{}", explain());
        // The model is still: the index costs nothing either (its queue
        // only reorders; what it misses, the tier's discovery finds).
        prop_assert_eq!(acted(&report), acted(&correct), "{}", explain());
        prop_assert_eq!(refused(&report), refused(&correct), "{}", explain());
        prop_assert_eq!(report.exit_code(), correct.exit_code(), "{}", explain());
    }
}
