//! The index's collapse on ModelHost (docs/testing.md, index and collapse):
//! the budget holds after every insertion and collapse, a run of collapses
//! ends, a member whose re-validation fails is evicted and never absorbed,
//! a block's totals are the sums of what it absorbed, a replaced site is
//! not collapsed, and a mount point is never touched. ModelHost also fails
//! the test on any call the path-traversal rules forbid.
//!
//! The units are fed by a test driver standing in for the tally (TASK-29):
//! a listing of `/tmp` with a `statx` per name through the port, or
//! identities read from the model without port calls where only the
//! collapse's calls must reach the model (so that events fire on them).

mod common;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use common::{dir, host};
use diskgc_core::host::{Base, Dev, DirListing, Host, Stat};
use diskgc_core::index::{
    Block, CollapseReport, Discovered, Family, Finder, Identity, Index, Inserted, MountKey, Tally,
    UnitRef,
};
use diskgc_model::ModelHost;
use diskgc_model::load::parse_yaml;
use diskgc_model::spec::Op;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

const HEADER: &str = r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}
  - {id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 100G, parent: 1}
"#;

/// The `/tmp` mount of [`HEADER`], without a port call (events count the
/// collapse's calls).
fn tmp_mount(_host: &ModelHost) -> MountKey {
    MountKey {
        dev: Dev {
            major: 0,
            minor: 42,
        },
        mount_point: PathBuf::from("/tmp"),
    }
}

fn finder(profile: &str, root: &str) -> Finder {
    Finder {
        profile: profile.into(),
        profile_hash: 1,
        root: Path::new(root).into(),
    }
}

fn change(stat: &Stat) -> diskgc_core::host::Timestamp {
    stat.mtime.max(stat.ctime).expect("model reports times")
}

/// What discovery would report for `path`, read from the model without a
/// port call.
fn discovered(host: &ModelHost, path: &Path) -> Discovered {
    let stat = host.stat_path(path).expect("exists");
    let parent = host.stat_path(path.parent().unwrap()).expect("parent");
    Discovered {
        identity: Identity::of(&stat, parent.ino.unwrap()).expect("inode reported"),
        newest_change: change(&stat),
        checked_at: diskgc_core::host::MonoTime(std::time::Duration::ZERO),
    }
}

/// `n` distinct names `nix-shell.XXXXXX` (six base-62 characters, as
/// `mktemp` makes them), from a bijection so they never collide.
fn rc_names(n: u64) -> Vec<String> {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const SPACE: u64 = 62u64.pow(6);
    (0..n)
        .map(|i| {
            // 2654435761 is prime, so coprime to 62^6: i -> i*k mod 62^6 is
            // one-to-one (from 1: 0 would give an all-digit name, another
            // name family).
            let mut x = ((i + 1) * 2_654_435_761) % SPACE;
            let mut name = String::from("nix-shell.");
            for _ in 0..6 {
                name.push(char::from(ALPHABET[(x % 62) as usize]));
                x /= 62;
            }
            name
        })
        .collect()
}

#[derive(Debug, Default)]
struct Fed {
    new: usize,
    covered: usize,
    no_room: usize,
    collapses: Vec<CollapseReport>,
    max_accounted: usize,
}

/// The test driver's stand-in for the tally: lists `/tmp` through the port
/// and inserts every name with `finder`, checking the budget after each.
fn feed_listing(host: &ModelHost, index: &mut Index, finder: &Finder) -> Fed {
    let tmp = dir(host, "/tmp");
    let mount = MountKey::of(&tmp.mount);
    let tmp_ino = tmp.stat.ino.unwrap();
    let DirListing::Names(names) = host.read_dir(&tmp.fd, 1_000_000).unwrap() else {
        panic!("listing too long")
    };
    let mut fed = Fed::default();
    for name in names {
        let stat = host.statx(Base::Dir(&tmp.fd), Path::new(&name)).unwrap();
        let found = Discovered {
            identity: Identity::of(&stat, tmp_ino).unwrap(),
            newest_change: change(&stat),
            checked_at: host.now(),
        };
        let path = Path::new("/tmp").join(&name);
        let done = index.insert(host, &mount, &path, finder, found).unwrap();
        match done.outcome {
            Inserted::New => fed.new += 1,
            Inserted::Covered => fed.covered += 1,
            Inserted::NoRoom => fed.no_room += 1,
            other => panic!("{other:?} for a fresh listing"),
        }
        fed.collapses.extend(done.collapse);
        assert!(index.accounted_bytes() <= index.budget_bytes());
        fed.max_accounted = fed.max_accounted.max(index.accounted_bytes());
    }
    fed
}

/// AC #3 (docs/testing.md `index_never_exceeds_its_budget`): 98,000 tiny
/// nix-shell rc dirs, a budget that fits a tenth of them as singles.
#[test]
fn index_never_exceeds_its_budget() {
    const DIRS: u64 = 98_000;
    let started = Instant::now();
    let names = rc_names(DIRS);
    // Built as a value: YAML of 98,000 entries is past the parser's node
    // budget. Every dir is a copy of one parsed entry.
    let mut model =
        parse_yaml(&format!("{HEADER}tree:\n  /tmp/x: {{rc: {{size: 3K}}}}\n")).unwrap();
    let rc_dir = model.tree.remove(Path::new("/tmp/x")).unwrap();
    for name in &names {
        model
            .tree
            .insert(Path::new("/tmp").join(name), rc_dir.clone());
    }
    let host = ModelHost::new(&model).unwrap();
    let loaded = started.elapsed();

    // The trie bytes of a tenth of them as singles, measured by inserting
    // them into an index with room to spare.
    let mut calibration = Index::new(1 << 30);
    let rc = finder("nix-shell-rc", "/tmp");
    let mount = tmp_mount(&host);
    for name in names.iter().take((DIRS / 10) as usize) {
        let path = Path::new("/tmp").join(name);
        let found = discovered(&host, &path);
        calibration
            .insert(&host, &mount, &path, &rc, found)
            .unwrap();
    }
    let tenth = calibration.accounted_bytes();
    let per_single = tenth / (DIRS / 10) as usize;

    let mut index = Index::new(tenth);
    let rc = finder("nix-shell-rc", "/tmp");
    let statx_before = host.trace_count(Op::Statx);
    let fed = feed_listing(&host, &mut index, &rc);
    index.assert_consistent();
    let stats = index.stats();
    eprintln!(
        "index_never_exceeds_its_budget: {DIRS} rc dirs; model loaded in {loaded:?}, run {:?}; \
         ~{per_single} B counted per single; budget {} B; max counted {} B; \
         new {} covered {} no_room {}; collapse runs {} (absorbed {}, evicted {}, passes {}); \
         final: {} singles, {} blocks, {} members, {} B counted; statx calls {}",
        started.elapsed(),
        index.budget_bytes(),
        fed.max_accounted,
        fed.new,
        fed.covered,
        fed.no_room,
        fed.collapses.len(),
        fed.collapses.iter().map(|c| c.absorbed).sum::<usize>(),
        fed.collapses.iter().map(|c| c.evicted).sum::<usize>(),
        fed.collapses.iter().map(|c| c.passes).sum::<usize>(),
        stats.singles,
        stats.blocks,
        stats.members,
        stats.accounted_bytes,
        host.trace_count(Op::Statx) - statx_before,
    );
    for (site, block) in blocks(&index) {
        let family = match &block.family {
            Family::Name(name) => String::from_utf8_lossy(name).into_owned(),
            Family::Any => "any".to_owned(),
        };
        eprintln!(
            "  block at {}: family {family}, {} members",
            site.display(),
            block.members
        );
    }
    assert!(fed.max_accounted <= index.budget_bytes());
    assert!(
        !fed.collapses.is_empty(),
        "a tenth fits: the rest needed a collapse"
    );
    assert!(
        fed.collapses.iter().all(|c| c.reached_low_water),
        "{:?}",
        fed.collapses
    );
    assert_eq!(fed.no_room, 0, "collapsing always made room");
    assert_eq!(fed.new + fed.covered, DIRS as usize);
    // Every name found is a single or stands in a block (the members a
    // block counts were absorbed; later ones it covers are not counted).
    assert_eq!(stats.singles as u64 + stats.members, fed.new as u64);
}

/// Feeds `paths` (identities read from the model) with `finder` and tallies
/// unit `i` at `1 << i` bytes, so a block's `est_bytes` names exactly the
/// units it absorbed.
fn feed_tallied(
    host: &ModelHost,
    index: &mut Index,
    finder: &Finder,
    paths: &[PathBuf],
    first_bit: u32,
) {
    let mount = tmp_mount(host);
    for (i, path) in paths.iter().enumerate() {
        let found = discovered(host, path);
        let done = index.insert(host, &mount, path, finder, found).unwrap();
        assert_eq!(done.outcome, Inserted::New, "{}", path.display());
        index.single_mut(&mount, path).unwrap().tally = Some(Tally {
            est_bytes: 1 << (first_bit + u32::try_from(i).unwrap()),
            files: 1,
            tallied_at: host.now(),
        });
    }
}

/// An index whose units fit but stand above the low-water mark, so the
/// next collapse runs: `fill` is run twice, on a roomy index to measure and
/// on the tight one.
fn tight_index(fill: impl Fn(&mut Index)) -> Index {
    let mut roomy = Index::new(1 << 30);
    fill(&mut roomy);
    let mut index = Index::new(roomy.accounted_bytes());
    fill(&mut index);
    assert!(!index.at_low_water());
    index
}

fn blocks(index: &Index) -> Vec<(PathBuf, Block)> {
    let mut out = Vec::new();
    index.for_each_unit(|_, path, unit| {
        if let UnitRef::Block(b) = unit {
            out.push((path.to_owned(), b.clone()));
        }
    });
    out
}

/// docs/testing.md `collapse_revalidates_members`: members removed or
/// replaced after their tally are evicted by the collapse and contribute
/// nothing to the block.
#[test]
fn collapse_revalidates_members() {
    let names = rc_names(20);
    let mut yaml = format!("{HEADER}tree:\n");
    for name in &names {
        writeln!(yaml, "  /tmp/{name}: {{rc: {{size: 3K}}}}").unwrap();
    }
    // After the collapse's mountinfo, before it re-validates: one removed,
    // one replaced by a new inode, one by a new object reusing its inode
    // number (a later birth time).
    write!(
        yaml,
        "events:\n\
         \x20 - {{at: {{op: mountinfo, nth: 1, when: after}}, remove: /tmp/{}}}\n\
         \x20 - {{at: {{op: mountinfo, nth: 1, when: after}}, replace: /tmp/{}}}\n\
         \x20 - {{at: {{op: mountinfo, nth: 1, when: after}}, advance: 1s}}\n\
         \x20 - {{at: {{op: mountinfo, nth: 1, when: after}}, replace_reusing_inode: /tmp/{}}}\n",
        names[3], names[5], names[7]
    )
    .unwrap();
    let paths: Vec<PathBuf> = names.iter().map(|n| Path::new("/tmp").join(n)).collect();
    let host = host(&yaml);
    let mut index = tight_index(|index| {
        let rc = finder("nix-shell-rc", "/tmp");
        feed_tallied(&host, index, &rc, &paths, 0);
    });
    let report = index.collapse_to_low_water(&host).unwrap();
    index.assert_consistent();
    assert!(host.unfired_events().is_empty());
    assert_eq!(
        (report.collapses, report.evicted, report.absorbed),
        (1, 3, 17),
        "{report:?}"
    );
    let blocks = blocks(&index);
    let [(site, block)] = blocks.as_slice() else {
        panic!("{blocks:?}")
    };
    assert_eq!(site, Path::new("/tmp"));
    assert_eq!(block.family, Family::Name(b"nix-shell.*".to_vec().into()));
    let failed: u64 = (1 << 3) | (1 << 5) | (1 << 7);
    assert_eq!(block.est_bytes, ((1u64 << 20) - 1) & !failed);
    assert_eq!((block.members, block.files), (17, 17));
    assert_eq!(index.stats().singles, 0);
}

/// docs/testing.md `collapse_revalidates_members`, the site: a site
/// directory replaced by another inode under the same name is not collapsed
/// and every unit at and beneath it is evicted.
#[test]
fn a_replaced_site_is_not_collapsed_and_its_units_are_evicted() {
    let mut yaml = format!("{HEADER}tree:\n");
    for i in 0..10 {
        writeln!(yaml, "  /tmp/site/x{i}: {{f: {{size: 1K}}}}").unwrap();
    }
    yaml.push_str("events:\n  - {at: {op: mountinfo, nth: 1, when: after}, replace: /tmp/site}\n");
    let host = host(&yaml);
    let inner: Vec<PathBuf> = (0..10)
        .map(|i| PathBuf::from(format!("/tmp/site/x{i}")))
        .collect();
    let mut index = tight_index(|index| {
        let entry = finder("tmp-entry", "/tmp");
        let mount = tmp_mount(&host);
        let site = Path::new("/tmp/site");
        index
            .insert(&host, &mount, site, &entry, discovered(&host, site))
            .unwrap();
        let inner_finder = finder("inner", "/tmp/site");
        feed_tallied(&host, index, &inner_finder, &inner, 0);
    });
    let report = index.collapse_to_low_water(&host).unwrap();
    index.assert_consistent();
    assert_eq!((report.collapses, report.evicted), (0, 11), "{report:?}");
    assert_eq!(index.units(), 0);
}

/// A collapse that climbs re-validates the finer blocks it absorbs: a
/// block whose site was replaced after it was made is evicted, not
/// absorbed. Sites `s0`..`s2` collapse first (deepest); `s1` is replaced
/// while `s2` is re-validated; the climb to `/tmp/p` then takes `s0` and
/// `s2` only. Big untallied units of another profile, one per directory,
/// keep the index above its low-water mark until the climb.
#[test]
fn a_climbing_collapse_evicts_a_finer_block_whose_site_was_replaced() {
    let rc_names = rc_names(6);
    let mut yaml = format!("{HEADER}tree:\n");
    let mut rc_paths = Vec::new();
    for (i, name) in rc_names.iter().enumerate() {
        let path = format!("/tmp/p/s{}/{name}", i / 2);
        writeln!(yaml, "  {path}: {{rc: {{size: 1K}}}}").unwrap();
        rc_paths.push(PathBuf::from(path));
    }
    let big_paths: Vec<PathBuf> = (0..8)
        .map(|i| PathBuf::from(format!("/tmp/b{i}/x")))
        .collect();
    for path in &big_paths {
        writeln!(yaml, "  {}: {{f: {{size: 1K}}}}", path.display()).unwrap();
    }
    writeln!(
        yaml,
        "events:\n  - {{at: {{op: statx, path: {}, when: after}}, replace: /tmp/p/s1}}",
        rc_paths[5].display()
    )
    .unwrap();
    let host = host(&yaml);
    let mut index = tight_index(|index| {
        let rc = finder("nix-shell-rc", "/tmp");
        feed_tallied(&host, index, &rc, &rc_paths, 0);
        let big = finder("big", "/tmp");
        let mount = tmp_mount(&host);
        for path in &big_paths {
            index
                .insert(&host, &mount, path, &big, discovered(&host, path))
                .unwrap();
        }
    });
    let report = index.collapse_to_low_water(&host).unwrap();
    index.assert_consistent();
    assert!(host.unfired_events().is_empty(), "s2 was re-validated");
    let at_p: Vec<Block> = blocks(&index)
        .into_iter()
        .filter(|(site, _)| site == Path::new("/tmp/p"))
        .map(|(_, b)| b)
        .collect();
    let [climbed] = at_p.as_slice() else {
        panic!("no climb to /tmp/p: {report:?} {:?}", blocks(&index))
    };
    // s0 holds units 0 and 1, s2 units 4 and 5; s1's 2 and 3 are gone.
    assert_eq!(climbed.est_bytes, 0b11_0011);
    assert_eq!(climbed.members, 4);
    assert!(report.evicted >= 1, "{report:?}");
}

/// A family step over singles whose trie nodes stay (another profile's
/// units lie beneath them) frees only their boxes; with a long family name
/// the new block would cost more. Such a step is not taken (before the fix
/// it panicked on the budget assertion); the room comes from the
/// all-names steps instead (review of 884a59b).
#[test]
fn a_collapse_that_would_grow_the_index_is_not_taken() {
    let stem = "a".repeat(240);
    let (n1, n2) = (format!("{stem}.Abcde"), format!("{stem}.Xyzwv"));
    let host = host(&format!(
        "{HEADER}tree:\n  /tmp/{n1}/y: {{.: {{}}}}\n  /tmp/{n2}/y: {{.: {{}}}}\n  /tmp/z: {{.: {{}}}}\n"
    ));
    let fill = |index: &mut Index| {
        let outer = finder("p1", "/tmp");
        let inner = finder("p2", "/tmp");
        let mount = tmp_mount(&host);
        for name in [&n1, &n2] {
            let path = Path::new("/tmp").join(name);
            index
                .insert(&host, &mount, &path, &outer, discovered(&host, &path))
                .unwrap();
            let below = path.join("y");
            index
                .insert(&host, &mount, &below, &inner, discovered(&host, &below))
                .unwrap();
        }
        outer
    };
    let mut roomy = Index::new(1 << 30);
    fill(&mut roomy);
    let mut index = Index::new(roomy.accounted_bytes());
    let outer = fill(&mut index);
    let z = Path::new("/tmp/z");
    let done = index
        .insert(&host, &tmp_mount(&host), z, &outer, discovered(&host, z))
        .unwrap();
    index.assert_consistent();
    // The long family step was skipped; the all-names steps (no family
    // name to pay for) made room, and z is in the outer `any` block.
    assert_eq!(done.outcome, Inserted::Covered, "{done:?}");
    assert!(
        blocks(&index).iter().all(|(_, b)| b.family == Family::Any),
        "{:?}",
        blocks(&index)
    );
    assert!(index.accounted_bytes() <= index.budget_bytes());
}

/// A block sited at a directory that was replaced (and found again as a
/// single of the new identity) is never absorbed under the new identity:
/// it goes when the single is replaced (review of 884a59b).
#[test]
fn a_block_of_a_replaced_site_is_never_absorbed_under_the_new_identity() {
    let old = host(&format!(
        "{HEADER}tree:\n  /tmp/D/x1: {{.: {{}}}}\n  /tmp/D/x2: {{.: {{}}}}\n"
    ));
    // The same names, D on another inode.
    let new = host(&format!(
        "{HEADER}tree:\n  /tmp/0pad/q/r: {{.: {{}}}}\n  /tmp/D/x1: {{.: {{}}}}\n  \
         /tmp/D/x2: {{.: {{}}}}\n  /tmp/D/y: {{.: {{}}}}\n"
    ));
    let d = Path::new("/tmp/D");
    assert_ne!(discovered(&old, d).identity, discovered(&new, d).identity);
    let fill = |index: &mut Index| {
        let a = finder("a", "/tmp");
        let b = finder("b", "/tmp");
        for (path, f) in [("/tmp/D", &a), ("/tmp/D/x1", &b), ("/tmp/D/x2", &b)] {
            let path = Path::new(path);
            index
                .insert(&old, &tmp_mount(&old), path, f, discovered(&old, path))
                .unwrap();
        }
        (a, b)
    };
    let mut index = tight_index(|index| {
        fill(index);
    });
    let (a, b) = (index_finder("a"), index_finder("b"));
    index.collapse_to_low_water(&old).unwrap();
    assert_eq!(blocks(&index).len(), 1, "x1 and x2 collapsed at D");
    let mount = tmp_mount(&new);
    let done = index
        .insert(&new, &mount, d, &a, discovered(&new, d))
        .unwrap();
    assert_eq!(done.outcome, Inserted::Replaced);
    index.assert_consistent();
    assert!(
        blocks(&index).is_empty(),
        "the old site's block went with it"
    );
    let y = Path::new("/tmp/D/y");
    let done = index
        .insert(&new, &mount, y, &b, discovered(&new, y))
        .unwrap();
    assert_eq!(done.outcome, Inserted::New, "not covered by a stale block");
}

/// The same without a single at the site: a family block sited at `D`
/// is the only record of `D`'s identity. `D` is replaced (and refilled)
/// after the first collapse; the next collapse at `D` evicts the old block
/// instead of absorbing it under the new identity.
#[test]
fn a_collapse_evicts_a_block_sited_at_a_replaced_site() {
    let mut yaml = format!("{HEADER}tree:\n  /tmp/D/x1: {{.: {{}}}}\n  /tmp/D/x2: {{.: {{}}}}\n");
    for i in 0..20 {
        writeln!(yaml, "  /tmp/f{i}: {{.: {{}}}}").unwrap();
    }
    for i in 0..60 {
        writeln!(yaml, "  /tmp/g/g{i}: {{.: {{}}}}").unwrap();
    }
    // Fired by the test's own mountinfo, after the first collapse.
    yaml.push_str(
        "events:\n\
         \x20 - {at: {op: mountinfo, nth: 2, when: after}, replace: /tmp/D}\n\
         \x20 - {at: {op: mountinfo, nth: 2, when: after}, mkdir: /tmp/D/y1}\n\
         \x20 - {at: {op: mountinfo, nth: 2, when: after}, mkdir: /tmp/D/y2}\n",
    );
    let host = host(&yaml);
    let mount = tmp_mount(&host);
    let phase_one = |index: &mut Index| {
        let b = finder("b", "/tmp");
        let c = finder("c", "/tmp");
        for path in ["/tmp/D/x1", "/tmp/D/x2"] {
            let path = Path::new(path);
            index
                .insert(&host, &mount, path, &b, discovered(&host, path))
                .unwrap();
        }
        for i in 0..20 {
            let path = PathBuf::from(format!("/tmp/f{i}"));
            index
                .insert(&host, &mount, &path, &c, discovered(&host, &path))
                .unwrap();
        }
        b
    };
    let mut index = tight_index(|index| {
        phase_one(index);
    });
    let b = finder("b", "/tmp");
    let first = index.collapse_to_low_water(&host).unwrap();
    let x_family = Family::Name(b"x#".to_vec().into());
    assert!(
        blocks(&index)
            .iter()
            .any(|(site, blk)| site == Path::new("/tmp/D") && blk.family == x_family),
        "{first:?} {:?}",
        blocks(&index)
    );
    let _ = host.mountinfo().unwrap();
    for path in ["/tmp/D/y1", "/tmp/D/y2"] {
        let path = Path::new(path);
        let done = index
            .insert(&host, &mount, path, &b, discovered(&host, path))
            .unwrap();
        assert_eq!(done.outcome, Inserted::New);
    }
    // Fill (another profile, elsewhere) until the next collapse must run.
    let c = finder("c", "/tmp");
    for i in 0..60 {
        if !index.at_low_water() {
            break;
        }
        let path = PathBuf::from(format!("/tmp/g/g{i}"));
        index
            .insert(&host, &mount, &path, &c, discovered(&host, &path))
            .unwrap();
    }
    assert!(!index.at_low_water(), "the next collapse must run");
    let second = index.collapse_to_low_water(&host).unwrap();
    index.assert_consistent();
    assert!(host.unfired_events().is_empty());
    for (site, blk) in blocks(&index) {
        if site == Path::new("/tmp/D") {
            assert_ne!(blk.family, x_family, "{second:?}");
            assert_eq!(blk.members, 2, "only y1 and y2: {second:?} {blk:?}");
        }
    }
    assert!(second.evicted >= 1, "{second:?}");
}

/// Collapsing every unit of two finder sets into one lump leaves one set
/// (their union) in the table: the absorbed sets are swept, and the count
/// drops by their bytes.
#[test]
fn a_lump_sweeps_the_sets_it_absorbed() {
    let mut yaml = format!("{HEADER}tree:\n");
    for i in 0..6 {
        writeln!(yaml, "  /tmp/p{i}/f: {{size: 1K}}").unwrap();
        writeln!(yaml, "  /tmp/q{i}/f: {{size: 1K}}").unwrap();
    }
    let host = host(&yaml);
    let mut index = Index::new(1 << 30);
    let mount = tmp_mount(&host);
    for i in 0..6 {
        for (dir, profile) in [("p", "a"), ("q", "b")] {
            let path = PathBuf::from(format!("/tmp/{dir}{i}"));
            index
                .insert(
                    &host,
                    &mount,
                    &path,
                    &finder(profile, "/tmp"),
                    discovered(&host, &path),
                )
                .unwrap();
        }
    }
    assert_eq!(index.stats().finder_sets, 2);
    let before = index.accounted_bytes();
    let report = index.collapse_to(&host, 0).unwrap();
    index.assert_consistent();
    let stats = index.stats();
    assert_eq!(
        (stats.singles, stats.blocks, stats.members),
        (0, 1, 12),
        "{report:?}"
    );
    assert_eq!(stats.finder_sets, 1, "only the union is left");
    assert!(report.lumps >= 1, "{report:?}");
    let [(site, lump)] = blocks(&index).try_into().unwrap();
    assert_eq!(site, Path::new("/tmp"));
    assert_eq!(index.finder_set(lump.finders()).len(), 2);
    assert!(index.accounted_bytes() < before);
    // And evicting the lump's last member empties the tables.
    for _ in 0..12 {
        index.block_member_removed(
            &mount,
            Path::new("/tmp"),
            lump.finders(),
            &Family::Any,
            0,
            0,
        );
    }
    index.sweep_tables();
    index.assert_consistent();
    assert_eq!(index.accounted_bytes(), 0);
}

fn index_finder(profile: &str) -> Finder {
    finder(profile, "/tmp")
}

/// A name that became a mount point is never `statx`ed or opened: its unit
/// is evicted from the mountinfo line alone.
#[test]
fn a_mount_point_beneath_the_site_is_evicted_untouched() {
    let names = rc_names(6);
    let mut yaml = format!("{HEADER}tree:\n");
    for name in &names {
        writeln!(yaml, "  /tmp/{name}: {{rc: {{size: 3K}}}}").unwrap();
    }
    writeln!(
        yaml,
        "events:\n  - {{at: {{op: mountinfo, nth: 1, when: before}}, mount: \
         {{id: 77, at: /tmp/{}, fs: fuse, dev: \"0:77\", size: 1G}}}}",
        names[2]
    )
    .unwrap();
    let host = host(&yaml);
    let paths: Vec<PathBuf> = names.iter().map(|n| Path::new("/tmp").join(n)).collect();
    let mut index = tight_index(|index| {
        let rc = finder("nix-shell-rc", "/tmp");
        feed_tallied(&host, index, &rc, &paths, 0);
    });
    let report = index.collapse_to_low_water(&host).unwrap();
    index.assert_consistent();
    assert_eq!(
        (report.collapses, report.evicted, report.absorbed),
        (1, 1, 5),
        "{report:?}"
    );
    let mounted = &paths[2];
    assert!(
        host.trace()
            .iter()
            .all(|call| call.path.as_deref() != Some(mounted.as_path())),
        "the mount point was touched"
    );
}

// ---- Properties ----

/// A unit of a generated tree: its path below `/tmp` (one to three names
/// from small alphabets, so paths nest, share parents and families) and
/// which of two profiles found it.
#[derive(Debug, Clone)]
struct GenUnit {
    names: Vec<String>,
    profile_b: bool,
}

fn gen_unit() -> impl Strategy<Value = GenUnit> {
    let name = prop_oneof![
        (0u8..3).prop_map(|d| format!("d{d}")),
        (0u32..40).prop_map(|n| format!("nix-shell.ab{n:03}x")),
        (0u32..40).prop_map(|n| format!("pytest-{n}")),
        // Long families: a family block's name can outweigh the singles.
        (0u32..8).prop_map(|n| format!("{}.Ab{n:03}", "l".repeat(200))),
    ];
    (prop::collection::vec(name, 1..4), any::<bool>())
        .prop_map(|(names, profile_b)| GenUnit { names, profile_b })
}

/// What happens to a unit's path when the collapse starts.
#[derive(Debug, Clone, Copy)]
enum Vandal {
    Keep,
    Remove,
    Replace,
}

fn vandal() -> impl Strategy<Value = Vandal> {
    prop_oneof![6 => Just(Vandal::Keep), 1 => Just(Vandal::Remove), 1 => Just(Vandal::Replace)]
}

/// A model holding every unit as a directory with a file, and events that
/// vandalise units at the first `mountinfo` (the first collapse's).
fn gen_model(units: &[(PathBuf, bool, Vandal)]) -> ModelHost {
    let mut yaml = format!("{HEADER}tree:\n");
    for (path, _, _) in units {
        writeln!(yaml, "  {}/f: {{size: 1K}}", path.display()).unwrap();
    }
    let events: Vec<String> = units
        .iter()
        .filter_map(|(path, _, vandal)| match vandal {
            Vandal::Keep => None,
            Vandal::Remove => Some(format!("remove: {}", path.display())),
            Vandal::Replace => Some(format!("replace: {}", path.display())),
        })
        .collect();
    if !events.is_empty() {
        yaml.push_str("events:\n");
        for event in events {
            writeln!(
                yaml,
                "  - {{at: {{op: mountinfo, nth: 1, when: after}}, {event}}}"
            )
            .unwrap();
        }
    }
    host(&yaml)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    /// The budget holds after every insertion and collapse; every run of
    /// collapses ends; no block absorbs a unit whose identity no longer
    /// matches; every block's totals are the sums of the units it
    /// absorbed; and no unit that is still valid is lost.
    #[test]
    fn collapse_keeps_the_budget_and_absorbs_only_revalidated_units(
        generated in prop::collection::vec((gen_unit(), vandal()), 2..40),
        budget in 2_000usize..12_000,
    ) {
        // Distinct paths; the first occurrence wins.
        let mut units: Vec<(PathBuf, bool, Vandal)> = Vec::new();
        for (unit, vandal) in generated {
            let path = Path::new("/tmp").join(unit.names.join("/"));
            if units.iter().all(|(p, _, _)| *p != path) {
                units.push((path, unit.profile_b, vandal));
            }
        }
        // A unit below a vandalised one goes with it, untouched itself
        // (the model refuses to remove what is already gone).
        let vandalised: Vec<PathBuf> = units
            .iter()
            .filter(|(_, _, v)| !matches!(v, Vandal::Keep))
            .map(|(p, _, _)| p.clone())
            .collect();
        for (path, _, vandal) in &mut units {
            if vandalised.iter().any(|v| path.starts_with(v) && v != path) {
                *vandal = Vandal::Keep;
            }
        }
        let host = gen_model(&units);
        let mut index = Index::new(budget);
        let a = finder("a", "/tmp");
        let b = finder("b", "/tmp");
        let mount = tmp_mount(&host);
        let mut recorded: Vec<(PathBuf, Identity, u32)> = Vec::new();
        let mut collapse_runs = 0;
        let mut lumped = 0;
        for (bit, (path, profile_b, _)) in units.iter().enumerate() {
            let bit = u32::try_from(bit).unwrap();
            let Some(stat) = host.stat_path(path) else { continue };
            let parent = host.stat_path(path.parent().unwrap()).unwrap();
            let found = Discovered {
                identity: Identity::of(&stat, parent.ino.unwrap()).unwrap(),
                newest_change: change(&stat),
                checked_at: host.now(),
            };
            let f = if *profile_b { &b } else { &a };
            let done = index.insert(&host, &mount, path, f, found).unwrap();
            prop_assert!(index.accounted_bytes() <= index.budget_bytes());
            index.assert_consistent();
            if let Some(c) = done.collapse {
                collapse_runs += 1;
                prop_assert!(c.passes <= 2 * units.len() + 4);
            }
            lumped += usize::from(done.outcome == Inserted::Lumped);
            if done.outcome == Inserted::New {
                index.single_mut(&mount, path).unwrap().tally = Some(Tally {
                    est_bytes: 1 << bit,
                    files: 1,
                    tallied_at: host.now(),
                });
                recorded.push((path.clone(), found.identity, bit));
            }
        }
        // One more run at the end, wherever the index stands.
        let last = index.collapse_to_low_water(&host).unwrap();
        index.assert_consistent();
        prop_assert_eq!(last.reached_low_water, index.at_low_water());
        prop_assert!(last.passes <= 2 * units.len() + 4);

        let valid = |path: &Path, identity: Identity| -> bool {
            let (Some(stat), Some(parent)) = (host.stat_path(path), host.stat_path(path.parent().unwrap())) else {
                return false;
            };
            Identity::of(&stat, parent.ino.unwrap()) == Some(identity)
        };
        let mut in_blocks: u64 = 0;
        let mut as_singles: u64 = 0;
        let mut untallied_members: u64 = 0;
        index.for_each_unit(|_, _, unit| match unit {
            UnitRef::Block(block) => {
                assert_eq!(in_blocks & block.est_bytes, 0, "a unit in two blocks");
                // Tallied members carry one bit and one file each; a member
                // that joined a lump at the limit carries neither.
                assert_eq!(u64::from(block.est_bytes.count_ones()), block.files, "sums != absorbed");
                untallied_members += block.members - block.files;
                in_blocks |= block.est_bytes;
            }
            UnitRef::Single(single) => {
                if let Some(t) = single.tally {
                    as_singles |= t.est_bytes;
                }
            }
        });
        prop_assert_eq!(in_blocks & as_singles, 0);
        prop_assert_eq!(untallied_members, lumped as u64, "lumped members");
        for (path, identity, bit) in &recorded {
            let mask = 1u64 << bit;
            if in_blocks & mask != 0 {
                prop_assert!(valid(path, *identity), "absorbed {} after it failed", path.display());
            } else if valid(path, *identity) && collapse_runs > 0 {
                // A valid unit is still a single, or was evicted only with
                // a site that was vandalised above it.
                let above_vandalised = vandalised.iter().any(|p| path.starts_with(p) && p != path);
                prop_assert!(
                    as_singles & mask != 0 || above_vandalised,
                    "valid unit {} lost", path.display()
                );
            }
        }
    }

    /// Mark's rule (2026-10-25): with a budget at or above the floor (one
    /// lump per mount with its finders, plus the tables), no insertion is
    /// ever refused; the budget always holds; every collapse ends. Two
    /// mounts, three profiles with roots on both.
    #[test]
    fn no_insertion_is_refused_at_or_above_the_floor(
        generated in prop::collection::vec((gen_unit(), 0u8..3, any::<bool>()), 2..40),
        above_floor in 0usize..3_000,
    ) {
        let mut units: Vec<(PathBuf, u8)> = Vec::new();
        for (unit, profile, on_var) in generated {
            let base = if on_var { "/var/tmp" } else { "/tmp" };
            let path = Path::new(base).join(unit.names.join("/"));
            if units.iter().all(|(p, _)| *p != path) {
                units.push((path, profile));
            }
        }
        let mut yaml = format!(
            "{}  - {{id: 31, at: /var/tmp, fs: tmpfs, dev: \"0:43\", size: 1G, parent: 1}}\ntree:\n",
            HEADER
        );
        for (path, _) in &units {
            writeln!(yaml, "  {}/f: {{size: 1K}}", path.display()).unwrap();
        }
        let host = host(&yaml);
        let mount_of = |path: &Path| MountKey {
            dev: if path.starts_with("/var/tmp") { Dev { major: 0, minor: 43 } } else { Dev { major: 0, minor: 42 } },
            mount_point: PathBuf::from(if path.starts_with("/var/tmp") { "/var/tmp" } else { "/tmp" }),
        };
        let finder_of = |path: &Path, profile: u8| finder(
            ["a", "b", "c"][usize::from(profile)],
            if path.starts_with("/var/tmp") { "/var/tmp" } else { "/tmp" },
        );
        let feed = |index: &mut Index, check: bool| -> Result<Vec<Inserted>, TestCaseError> {
            let mut outcomes = Vec::new();
            for (path, profile) in &units {
                let done = index
                    .insert(&host, &mount_of(path), path, &finder_of(path, *profile), discovered(&host, path))
                    .unwrap();
                if check {
                    prop_assert!(index.accounted_bytes() <= index.budget_bytes());
                    index.assert_consistent();
                }
                outcomes.push(done.outcome);
            }
            Ok(outcomes)
        };
        // The floor: everything collapsed to one lump per mount.
        let mut reference = Index::new(1 << 30);
        feed(&mut reference, false)?;
        reference.collapse_to(&host, 0).unwrap();
        reference.assert_consistent();
        let stats = reference.stats();
        prop_assert!(stats.singles == 0 && stats.blocks == stats.mounts, "{:?}", stats);
        let floor = reference.accounted_bytes();

        let mut index = Index::new(floor.max(1024) + above_floor);
        let outcomes = feed(&mut index, true)?;
        prop_assert!(!outcomes.contains(&Inserted::NoRoom), "{:?} at floor {}", outcomes, floor);
        let stats = index.stats();
        let found = outcomes.iter().filter(|o| matches!(o, Inserted::New | Inserted::Lumped)).count();
        prop_assert_eq!(stats.singles as u64 + stats.members, found as u64, "{:?}", stats);
    }
}

/// Letters only (`a`..`z`), one-to-one: a family stem that no digit run or
/// suffix rule folds into another.
fn letters(mut i: usize) -> String {
    let mut out = String::new();
    loop {
        out.push(char::from(b'a' + u8::try_from(i % 26).unwrap()));
        i /= 26;
        if i == 0 {
            return out;
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 6, ..ProptestConfig::default() })]

    /// TASK-230: a crowded plan. Beneath `/tmp/d`, more name families than
    /// a pass plans steps (`PLAN_LIMIT`), each of two singles of profile `a`
    /// with a long family name whose trie nodes stay (each holds a single of
    /// profile `b` beneath it): every family step would grow the index and
    /// all of them sort before the all-names steps that shrink it. Filled to
    /// the ceiling, more insertions must still collapse (never refused
    /// above the floor), the budget holding after each.
    #[test]
    fn a_crowded_plan_never_refuses_an_insertion_above_the_floor(
        extra_families in 1usize..64,
        stem_len in 120usize..240,
        slack in 0usize..170,
        inserts in 2usize..12,
    ) {
        use diskgc_core::index::collapse::PLAN_LIMIT;
        let families = PLAN_LIMIT + extra_families;
        let stem = "s".repeat(stem_len);
        let mut model = parse_yaml(&format!(
            "{HEADER}tree:\n  /tmp/x: {{inner: {{f: {{size: 1K}}}}}}\n  /tmp/e: {{.: {{}}}}\n"
        ))
        .unwrap();
        let unit = model.tree.remove(Path::new("/tmp/x")).unwrap();
        let mut outer: Vec<PathBuf> = Vec::new();
        for i in 0..families {
            for suffix in ["abcde", "fghij"] {
                let path = Path::new("/tmp/d").join(format!("{stem}{}.{suffix}", letters(i)));
                model.tree.insert(path.clone(), unit.clone());
                outer.push(path);
            }
        }
        let extra: Vec<PathBuf> = (0..inserts)
            .map(|i| Path::new("/tmp/e").join(format!("late-{}", letters(i))))
            .collect();
        for path in &extra {
            model.tree.insert(path.clone(), unit.clone());
        }
        let host = ModelHost::new(&model).unwrap();
        let mount = tmp_mount(&host);
        let (a, b) = (finder("a", "/tmp"), finder("b", "/tmp"));
        let fill = |index: &mut Index| {
            for path in &outer {
                for (finder, path) in [(&a, path.clone()), (&b, path.join("inner"))] {
                    let done = index
                        .insert(&host, &mount, &path, finder, discovered(&host, &path))
                        .unwrap();
                    assert_eq!(done.outcome, Inserted::New);
                }
            }
        };
        let mut sizing = Index::new(1 << 30);
        fill(&mut sizing);
        let mut index = Index::new(sizing.accounted_bytes() + slack);
        fill(&mut index);
        index.assert_consistent();
        for path in &extra {
            let done = index
                .insert(&host, &mount, path, &a, discovered(&host, path))
                .unwrap();
            prop_assert!(
                !matches!(done.outcome, Inserted::NoRoom | Inserted::Lumped),
                "{:?} ({:?}) with {} families: the crowded plan stopped above the floor",
                done.outcome, done.collapse, families
            );
            prop_assert!(index.accounted_bytes() <= index.budget_bytes());
            index.assert_consistent();
        }
    }
}
