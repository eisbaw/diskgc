//! A snapshot's contents never panic diskgc (Codex and mped review of
//! TASK-30): whatever a checksum-valid file says, decoding, the validity
//! checks, restoring into the index (with collapses on a small budget),
//! the index's statistics and a collapse to one lump per mount either work
//! or refuse the snapshot; they never overflow a sum or exhaust the stack.
//! Counters are drawn near `u64::MAX` and paths up to 300 components deep
//! on purpose. Cases: `DISKGC_PROPERTY_CASES` (default 128).

use std::path::PathBuf;

use diskgc_core::engine::Reason;
use diskgc_core::host::{Dev, FsKey, Timestamp};
use diskgc_core::index::{Birth, Family, Finder, Identity, MountKey};
use diskgc_core::snapshot::format::{
    self, BlockRec, FinderRec, Header, SingleRec, Snapshot, TallyRec,
};
use diskgc_core::snapshot::restore::{self, Clocks};
use diskgc_core::snapshot::{DISKGC_VERSION, fits};
use diskgc_model::ModelHost;
use proptest::prelude::*;

const TMP: Dev = Dev {
    major: 0,
    minor: 42,
};

fn cases() -> u32 {
    std::env::var("DISKGC_PROPERTY_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128)
}

/// `/tmp` with a few nested directories, so that units of the snapshot
/// name real objects and survive a collapse's re-validation (their sums
/// are then really added).
fn host() -> ModelHost {
    ModelHost::from_yaml(
        "host_model: 1\nclock: 2026-10-15T03:00:00Z\nmounts:\n  \
         - {id: 1, at: /, fs: ext4, dev: \"254:1\", size: 100G}\n  \
         - {id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 1}\n\
         tree:\n  /tmp/a/x: {.: {}}\n  /tmp/a/y: {.: {}}\n  /tmp/b: {.: {}}\n  \
         /tmp/c/d/e: {.: {}}\n",
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

const PATHS: [&str; 7] = [
    "/tmp/a",
    "/tmp/a/x",
    "/tmp/a/y",
    "/tmp/b",
    "/tmp/c",
    "/tmp/c/d",
    "/tmp/c/d/e",
];

/// The identity discovery would record for `path` in [`host`]'s model.
fn identity_of(host: &ModelHost, path: &str) -> Identity {
    let path = std::path::Path::new(path);
    let stat = host.stat_path(path).expect("in the model");
    let parent = host
        .stat_path(path.parent().expect("a parent"))
        .expect("parent");
    Identity::of(&stat, parent.ino.expect("ino")).expect("an identity")
}

fn counter() -> impl Strategy<Value = u64> {
    prop_oneof![
        3 => 0u64..10_000,
        1 => Just(u64::MAX),
        1 => Just(u64::MAX / 2 + 1),
        1 => Just(1u64 << 62),
        1 => Just((1u64 << 62) - 1),
    ]
}

fn time() -> impl Strategy<Value = Timestamp> {
    prop_oneof![
        3 => (1_700_000_000i64..1_800_000_000).prop_map(|secs| Timestamp { secs, nanos: 0 }),
        1 => Just(Timestamp { secs: i64::MAX, nanos: 999_999_999 }),
        1 => Just(Timestamp { secs: i64::MIN, nanos: 0 }),
    ]
}

/// A path of the model (its real identity, or a wrong one), or rarely a
/// deep one that names nothing (deeper than the 256-component bound).
#[derive(Debug, Clone)]
enum At {
    Real { path: usize, right: bool },
    Deep(usize),
}

fn at() -> impl Strategy<Value = At> {
    prop_oneof![
        8 => (0..PATHS.len(), any::<bool>()).prop_map(|(path, right)| At::Real { path, right }),
        1 => (200usize..300).prop_map(At::Deep),
    ]
}

fn resolve(host: &ModelHost, at: &At, wrong: Identity) -> (PathBuf, Identity) {
    match at {
        At::Real { path, right } => {
            let id = if *right {
                identity_of(host, PATHS[*path])
            } else {
                wrong
            };
            (PathBuf::from(PATHS[*path]), id)
        }
        At::Deep(depth) => (PathBuf::from(format!("/tmp{}", "/z".repeat(*depth))), wrong),
    }
}

/// A wrong identity (on the right device).
fn identity() -> impl Strategy<Value = Identity> {
    (
        1000u64..1064,
        prop_oneof![
            time().prop_map(Birth::Time),
            (1u64..64).prop_map(Birth::ParentIno)
        ],
    )
        .prop_map(|(ino, birth)| Identity {
            dev: TMP,
            ino,
            birth,
        })
}

fn finder_ids() -> impl Strategy<Value = Vec<usize>> {
    prop_oneof![Just(vec![0]), Just(vec![1]), Just(vec![0, 1])]
}

type Counters = (u64, u64, u64);

#[derive(Debug, Clone)]
struct GenSingle {
    at: At,
    wrong: Identity,
    finders: Vec<usize>,
    newest: Timestamp,
    tally: Option<(u64, u64, Timestamp)>,
    checked: Timestamp,
    refusal: Option<Timestamp>,
}

#[derive(Debug, Clone)]
struct GenBlock {
    site: Option<At>,
    wrong: Identity,
    finders: Vec<usize>,
    family: bool,
    counters: Counters,
    t: Timestamp,
    tallied: Option<Timestamp>,
}

fn single() -> impl Strategy<Value = GenSingle> {
    (
        at(),
        identity(),
        finder_ids(),
        time(),
        proptest::option::of((counter(), counter(), time())),
        time(),
        proptest::option::of(time()),
    )
        .prop_map(
            |(at, wrong, finders, newest, tally, checked, refusal)| GenSingle {
                at,
                wrong,
                finders,
                newest,
                tally,
                checked,
                refusal,
            },
        )
}

fn block() -> impl Strategy<Value = GenBlock> {
    (
        proptest::option::of(at()),
        identity(),
        finder_ids(),
        any::<bool>(),
        (counter(), counter(), counter()),
        time(),
        proptest::option::of(time()),
    )
        .prop_map(
            |(site, wrong, finders, family, counters, t, tallied)| GenBlock {
                site,
                wrong,
                finders,
                family,
                counters,
                t,
                tallied,
            },
        )
}

#[derive(Debug, Clone)]
struct Gen {
    singles: Vec<GenSingle>,
    blocks: Vec<GenBlock>,
    discovered: Option<Timestamp>,
}

fn generated() -> impl Strategy<Value = Gen> {
    (
        proptest::collection::vec(single(), 0..12),
        proptest::collection::vec(block(), 0..8),
        proptest::option::of(time()),
    )
        .prop_map(|(singles, blocks, discovered)| Gen {
            singles,
            blocks,
            discovered,
        })
}

fn header() -> Header {
    Header {
        diskgc_version: DISKGC_VERSION.to_owned(),
        key: FsKey::dev(TMP),
        uid: 1000,
        boot_id: diskgc_model::state::DEFAULT_BOOT_ID.to_owned(),
        written: Timestamp {
            secs: 1_760_000_000,
            nanos: 0,
        },
    }
}

fn mounts() -> Vec<MountKey> {
    vec![MountKey {
        dev: TMP,
        mount_point: PathBuf::from("/tmp"),
    }]
}

fn finders(discovered: Option<Timestamp>) -> Vec<FinderRec> {
    let finder = |profile: &str| Finder {
        profile: profile.into(),
        profile_hash: 7,
        root: std::path::Path::new("/tmp").into(),
    };
    vec![
        FinderRec {
            finder: finder("p-a"),
            discovered_at: discovered,
        },
        FinderRec {
            finder: finder("p-b"),
            discovered_at: None,
        },
    ]
}

fn snapshot(host: &ModelHost, g: &Gen) -> Snapshot {
    let singles = g
        .singles
        .iter()
        .map(|u| {
            let (path, identity) = resolve(host, &u.at, u.wrong);
            SingleRec {
                mount: 0,
                path,
                identity,
                finders: u.finders.clone(),
                newest_change: u.newest,
                tally: u.tally.map(|(est_bytes, files, tallied_at)| TallyRec {
                    est_bytes,
                    files,
                    tallied_at,
                }),
                checked_at: u.checked,
                refusal: u.refusal.map(|at| (Reason::KeepMarker, at)),
            }
        })
        .collect();
    let blocks = g
        .blocks
        .iter()
        .map(|b| {
            let (site, site_identity) = match &b.site {
                Some(at) => resolve(host, at, b.wrong),
                None => (PathBuf::from("/tmp"), b.wrong),
            };
            let (members, est_bytes, files) = b.counters;
            BlockRec {
                mount: 0,
                site,
                site_identity,
                finders: b.finders.clone(),
                family: if b.family {
                    Family::Name(b"x".to_vec().into())
                } else {
                    Family::Any
                },
                members: members.max(1),
                est_bytes,
                files,
                oldest_change: b.t,
                newest_change: b.t,
                tallied_at: b.tallied,
            }
        })
        .collect();
    Snapshot {
        header: header(),
        finders: finders(g.discovered),
        mounts: mounts(),
        singles,
        blocks,
    }
}

/// Seeds from `s` as a run would and collapses to one lump: never panics.
fn seed_and_collapse(host: &ModelHost, s: &Snapshot, budget: usize) {
    let Ok(decoded) = format::decode(&format::encode(s)) else {
        return; // refused: the run starts from nothing
    };
    let Ok(valid) = fits(host, &FsKey::dev(TMP), decoded) else {
        return;
    };
    let Ok((mut state, _)) = restore::restore(host, &valid, Clocks::now(host), budget, |_| true)
    else {
        return; // a host failure during a collapse: reported, no panic
    };
    let _ = state.index.stats();
    let _ = state.index.collapse_to(host, 0);
    let _ = state.index.stats();
    let again = restore::capture(&state, Clocks::now(host), valid.header.clone());
    assert!(format::decode(&format::encode(&again)).is_ok());
}

/// The stack bound (Codex review of TASK-30): a snapshot path tens of
/// thousands of components deep is refused at decode, so the index's
/// recursive walks and drop never see it.
#[test]
fn a_path_far_deeper_than_the_bound_is_refused_before_the_index() {
    let host = host();
    let deep = PathBuf::from(format!("/tmp{}", "/z".repeat(60_000)));
    let s = Snapshot {
        header: header(),
        finders: finders(None),
        mounts: mounts(),
        singles: vec![SingleRec {
            mount: 0,
            path: deep,
            identity: identity_of(&host, "/tmp/b"),
            finders: vec![0],
            newest_change: header().written,
            tally: None,
            checked_at: header().written,
            refusal: None,
        }],
        blocks: Vec::new(),
    };
    assert!(format::decode(&format::encode(&s)).is_err());
    seed_and_collapse(&host, &s, 1 << 26);
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: cases(),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn no_snapshot_content_panics_the_seed_or_a_later_collapse(
        g in generated(),
        budget in prop_oneof![Just(1usize << 26), 2048usize..16_384],
    ) {
        let host = host();
        seed_and_collapse(&host, &snapshot(&host, &g), budget);
    }

    #[test]
    fn decoding_any_bytes_never_panics(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
        prefix in any::<bool>(),
    ) {
        let mut data = if prefix { format::MAGIC.to_vec() } else { Vec::new() };
        data.extend(bytes);
        let _ = format::decode(&data);
    }
}
