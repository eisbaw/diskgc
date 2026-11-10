//! [`unchanged_for`] on tables written as mountinfo would show them, and a
//! property: whenever it says unchanged, every table question the
//! enumeration from the roots asks answers alike in both tables.

use super::*;
use crate::host::Dev;
use crate::places::{mount_for, mounts_within, places_above};
use crate::resolve::{is_mount_point, screen_mount_chain};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use std::collections::BTreeSet;

fn mount(id: u64, parent_id: u64, point: &str, dev: (u32, u32), root: &str) -> MountEntry {
    MountEntry {
        id,
        parent_id,
        dev: Dev {
            major: dev.0,
            minor: dev.1,
        },
        fs_root: PathBuf::from(root),
        mount_point: PathBuf::from(point),
        fs_type: "ext4".to_owned(),
        source: "fixture".to_owned(),
        read_only: false,
        idmapped: false,
        noatime: false,
        upper_dir: None,
    }
}

const ROOT: (u32, u32) = (254, 1);
const HOME: (u32, u32) = (254, 2);

/// `/` and `/home` on their own filesystems, `/var/lib/docker` a plain
/// directory of `/`.
fn base() -> Vec<MountEntry> {
    vec![mount(1, 0, "/", ROOT, "/"), mount(2, 1, "/home", HOME, "/")]
}

fn with(mut table: Vec<MountEntry>, more: MountEntry) -> Vec<MountEntry> {
    table.push(more);
    table
}

fn unchanged(old: &[MountEntry], new: &[MountEntry], roots: &[&str]) -> bool {
    let roots: Vec<&Path> = roots.iter().map(Path::new).collect();
    unchanged_for(old, new, &roots)
}

const SRC: &[&str] = &["/home/u/src"];

fn overlay(id: u64, point: &str, minor: u32) -> MountEntry {
    MountEntry {
        fs_type: "overlay".to_owned(),
        ..mount(id, 1, point, (0, minor), "/")
    }
}

/// Docker's churn: an overlay mounted, unmounted, another one under a
/// reused id and anonymous device; a FUSE portal; nsfs mounts sharing one
/// device. None of it is on the way to, below, or a view of the root.
#[test]
fn container_and_portal_mounts_elsewhere_change_nothing_for_a_root() {
    let old = base();
    let docker = with(
        old.clone(),
        overlay(100, "/var/lib/docker/overlay2/a/merged", 60),
    );
    assert!(unchanged(&old, &docker, SRC));
    assert!(unchanged(&docker, &old, SRC));
    // The same id and device, shown at another container's directory.
    let reused = with(
        old.clone(),
        overlay(100, "/var/lib/docker/overlay2/b/merged", 60),
    );
    assert!(unchanged(&docker, &reused, SRC));
    let netns = with(
        with(
            old.clone(),
            mount(101, 1, "/run/docker/netns/x", (0, 4), "net:[1]"),
        ),
        mount(102, 1, "/run/docker/netns/y", (0, 4), "net:[2]"),
    );
    assert!(unchanged(&old, &netns, SRC));
    let portal = with(
        old.clone(),
        mount(103, 1, "/run/user/1000/doc", (0, 70), "/"),
    );
    assert!(unchanged(&old, &portal, SRC));
}

#[test]
fn a_mount_at_above_below_or_beside_the_way_to_a_root_is_a_change() {
    let old = base();
    for point in [
        "/home/u/src",        // at the root
        "/home/u/src/keep-x", // below it
        "/home/u",            // above it
        "/home/u/other",      // directly in an ancestor (a name the storage probe sees)
        "/home/storage",      // likewise, higher up
        "/data",              // directly in /
    ] {
        let parent = if point.starts_with("/home") { 2 } else { 1 };
        let new = with(old.clone(), mount(100, parent, point, (0, 80), "/"));
        assert!(!unchanged(&old, &new, SRC), "{point}");
        assert!(!unchanged(&new, &old, SRC), "{point} gone");
    }
    // Deeper beside the way is apart.
    let new = with(
        old.clone(),
        mount(100, 2, "/home/u/other/deep", (0, 80), "/"),
    );
    assert!(unchanged(&old, &new, SRC));
}

/// A bind of the root's filesystem that shows the root, a directory above
/// it or part of it elsewhere (deep in an unrelated tree: only its views
/// make it count), and a mount attached on such a bind inside the root's
/// place. A bind of an unrelated directory of the same filesystem shows
/// nothing a question looks at (a kubelet's pod volumes).
#[test]
fn binds_showing_the_roots_tree_and_mounts_on_them_are_changes() {
    let old = base();
    for shown in ["/u/src/keep-x", "/u", "/"] {
        let new = with(old.clone(), mount(100, 1, "/srv/deep/alias", HOME, shown));
        assert!(!unchanged(&old, &new, SRC), "{shown}");
    }
    let new = with(
        old.clone(),
        mount(100, 1, "/srv/deep/alias", HOME, "/v/elsewhere"),
    );
    assert!(unchanged(&old, &new, SRC));
    // A fresh filesystem mounted inside the root's place, through a bind
    // (attached on /home's filesystem at /u/src/keep-x/sub).
    let with_bind = with(
        old.clone(),
        mount(100, 1, "/srv/deep/alias", HOME, "/u/src/keep-x"),
    );
    let new = with(
        with_bind.clone(),
        mount(101, 100, "/srv/deep/alias/sub", (0, 81), "/"),
    );
    assert!(!unchanged(&with_bind, &new, SRC));
    // The same through a bind of an unrelated directory: apart.
    let with_bind = with(
        old.clone(),
        mount(100, 1, "/srv/deep/alias", HOME, "/v/elsewhere"),
    );
    let new = with(
        with_bind.clone(),
        mount(101, 100, "/srv/deep/alias/sub", (0, 81), "/"),
    );
    assert!(unchanged(&with_bind, &new, SRC));
}

#[test]
fn a_mount_with_a_mount_on_it_is_a_change() {
    let old = base();
    let parent = mount(100, 1, "/srv/a", (0, 82), "/");
    let child = mount(101, 100, "/srv/a/b", (0, 83), "/");
    assert!(unchanged(&old, &with(old.clone(), parent.clone()), SRC));
    let both = with(with(old.clone(), parent), child);
    // Each is apart from the root; the parent is not a leaf. Both came,
    // the parent is judged with its child in the table.
    assert!(!unchanged(&old, &both, SRC));
}

/// TASK-285: `/alias` binds `/src/work/sub`; `/src/work` is renamed to
/// `/src/keep-work`. The bind's root field follows the rename.
#[test]
fn a_rename_above_a_bind_source_on_the_roots_filesystem_is_a_change() {
    let old = vec![
        mount(1, 0, "/", ROOT, "/"),
        mount(2, 1, "/src", HOME, "/"),
        mount(3, 1, "/alias", HOME, "/work/sub"),
    ];
    let mut new = old.clone();
    new[2].fs_root = PathBuf::from("/keep-work/sub");
    assert!(!unchanged(&old, &new, &["/src"]));
}

/// A rename of a directory above a root, seen only because an unrelated
/// filesystem is mounted below that directory: `/home/x` renamed to
/// `/home/u` (the root `/home/u/src` appears). The mount's line changed with
/// the same id and device, and the renamed directory is an ancestor of
/// both its place and the root's.
#[test]
fn a_rename_that_moves_a_mount_and_a_root_together_is_a_change() {
    let old = with(base(), mount(100, 2, "/home/x/deep/m", (0, 84), "/"));
    let mut new = old.clone();
    new[2].mount_point = PathBuf::from("/home/u/deep/m");
    assert!(!unchanged(&old, &new, SRC));
    // The same move far from the root: nothing shared below `/`.
    let old = with(base(), mount(100, 1, "/var/a/deep/m", (0, 84), "/"));
    let mut new = old.clone();
    new[2].mount_point = PathBuf::from("/var/b/deep/m");
    assert!(unchanged(&old, &new, SRC));
}

/// On a btrfs `/` mounted by subvolume (root field `/@`), every place
/// shares `/@`: a rename there would change `/`'s own line, so it does not
/// make every moved mount a change.
#[test]
fn a_shared_subvolume_root_field_is_not_a_shared_directory() {
    let old = vec![
        mount(1, 0, "/", ROOT, "/@"),
        overlay(100, "/var/lib/docker/overlay2/a/merged", 60),
    ];
    let mut new = old.clone();
    new[1].mount_point = PathBuf::from("/var/lib/docker/overlay2/b/merged");
    assert!(unchanged(&old, &new, &["/tmp/x"]));
    // Sharing `/@/var`, with `/var` a directory of `/`: a change.
    assert!(!unchanged(&old, &new, &["/var/tmp/x"]));
}

#[test]
fn a_mount_moved_onto_or_off_the_way_is_a_change() {
    let old = with(base(), mount(100, 1, "/srv/m", (0, 85), "/"));
    let mut new = old.clone();
    new[2].mount_point = PathBuf::from("/home/u/src/m");
    new[2].parent_id = 2;
    assert!(!unchanged(&old, &new, SRC));
    assert!(!unchanged(&new, &old, SRC));
}

#[test]
fn tables_that_cannot_be_compared_mount_by_mount_are_a_change() {
    let old = with(base(), mount(100, 1, "/srv/m", (0, 85), "/"));
    let mut reordered = old.clone();
    reordered.swap(1, 2);
    assert!(!unchanged(&old, &reordered, SRC));
    let twice = with(old.clone(), mount(100, 1, "/srv/n", (0, 86), "/"));
    assert!(!unchanged(&old, &twice, SRC));
    // A root the table cannot place.
    let new = with(old.clone(), mount(101, 1, "/srv/x", (0, 87), "/"));
    assert!(!unchanged(&old[1..], &new[1..], &["/tmp/x"]));
}

// ---- The property ----

const NAMES: [&str; 3] = ["a", "b", "c"];

fn path_of(names: &[usize]) -> PathBuf {
    let mut path = PathBuf::from("/");
    for &name in names {
        path.push(NAMES[name]);
    }
    path
}

fn names(depth: std::ops::RangeInclusive<usize>) -> impl Strategy<Value = Vec<usize>> {
    prop::collection::vec(0..NAMES.len(), depth)
}

/// One mount to add: where (a path of up to 3 names), its device (small
/// numbers repeat: binds), its root field.
#[derive(Debug, Clone)]
struct Spec {
    point: Vec<usize>,
    dev: u32,
    root: Vec<usize>,
    /// A root field that is no path (`net:[1]`, as `d_dname` prints).
    odd_root: bool,
}

fn spec() -> impl Strategy<Value = Spec> {
    (
        names(1..=3),
        1_u32..7,
        names(0..=2),
        prop::bool::weighted(0.05),
    )
        .prop_map(|(point, dev, root, odd_root)| Spec {
            point,
            dev,
            root,
            odd_root,
        })
}

/// A mount added as the kernel would: on the mount its point's parent
/// directory is on now (stacked on one at the same point).
fn add(table: &mut Vec<MountEntry>, id: u64, spec: &Spec) {
    let point = path_of(&spec.point);
    let Ok(parent) = mount_for(table, &point) else {
        return;
    };
    let parent_id = parent.id;
    table.push(mount(
        id,
        parent_id,
        &point.to_string_lossy(),
        (0, spec.dev),
        &if spec.odd_root {
            "net:[1]".to_owned()
        } else {
            path_of(&spec.root).to_string_lossy().into_owned()
        },
    ));
}

#[derive(Debug, Clone)]
enum Edit {
    Add(Spec),
    /// Unmount the n-th mount (mod the count) that has no mount on it.
    Remove(usize),
    /// Rename the directory at place `from` of device `dev` to `to`.
    Rename {
        dev: u32,
        from: Vec<usize>,
        to: Vec<usize>,
    },
    /// Move the n-th leaf mount to another point.
    Move(usize, Vec<usize>),
    Remount(usize),
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        spec().prop_map(Edit::Add),
        any::<usize>().prop_map(Edit::Remove),
        (1_u32..7, names(1..=2), names(1..=2)).prop_map(|(dev, from, to)| Edit::Rename {
            dev,
            from,
            to
        }),
        (any::<usize>(), names(1..=3)).prop_map(|(n, to)| Edit::Move(n, to)),
        any::<usize>().prop_map(Edit::Remount),
    ]
}

fn leaves(table: &[MountEntry]) -> Vec<usize> {
    (1..table.len())
        .filter(|&i| {
            !table
                .iter()
                .any(|m| m.parent_id == table[i].id && m.id != table[i].id)
        })
        .collect()
}

/// `table` after `edit`, as the kernel would show it; `None` when the edit
/// is not possible (a rename of a mount point, into itself, ...).
fn apply(table: &[MountEntry], edit: &Edit, next_id: u64) -> Option<Vec<MountEntry>> {
    let mut new = table.to_vec();
    match edit {
        Edit::Add(spec) => {
            add(&mut new, next_id, spec);
            (new.len() > table.len()).then_some(new)
        }
        Edit::Remove(n) => {
            let leaves = leaves(table);
            let index = *leaves.get(n % leaves.len().max(1))?;
            new.remove(index);
            Some(new)
        }
        Edit::Remount(n) => {
            let index = 1 + n % (table.len() - 1).max(1);
            let line = new.get_mut(index)?;
            line.read_only = !line.read_only;
            Some(new)
        }
        Edit::Move(n, to) => {
            let leaves = leaves(table);
            let index = *leaves.get(n % leaves.len().max(1))?;
            let moved = new.remove(index);
            let point = path_of(to);
            let parent = mount_for(&new, &point).ok()?.id;
            new.insert(
                index,
                MountEntry {
                    parent_id: parent,
                    mount_point: point,
                    ..moved
                },
            );
            Some(new)
        }
        Edit::Rename { dev, from, to } => rename(
            table,
            Dev {
                major: 0,
                minor: *dev,
            },
            &path_of(from),
            &path_of(to),
        ),
    }
}

/// `rename(2)` of the directory at place `from` of filesystem `dev`: every
/// root field and every attachment place below it moves; mount points are
/// derived again down each parent chain. A directory that is a mount's
/// point (`EBUSY`) or a rename into itself is not possible.
fn rename(table: &[MountEntry], dev: Dev, from: &Path, to: &Path) -> Option<Vec<MountEntry>> {
    if to.starts_with(from) || from.starts_with(to) {
        return None;
    }
    let by_id = by_id(table);
    let moved = |path: &Path| -> PathBuf {
        match path.strip_prefix(from) {
            Ok(rest) => joined(to, rest),
            Err(_) => path.to_owned(),
        }
    };
    // The attachment place of each mount, before.
    let mut attached = Vec::new();
    for line in table {
        let parent = by_id.get(&line.parent_id).filter(|p| p.id != line.id);
        attached.push(match parent {
            Some(parent) => {
                let at = attached_on(parent, line).ok()?;
                if at.dev == dev && at.path == from {
                    return None;
                }
                Some((parent.id, at))
            }
            None => None,
        });
    }
    let mut new: Vec<MountEntry> = table.to_vec();
    for line in &mut new {
        if line.dev == dev {
            line.fs_root = moved(&line.fs_root);
        }
    }
    // Mount points top down: each from its parent's new point and root.
    for _ in 0..new.len() {
        for index in 0..new.len() {
            let Some((parent_id, at)) = &attached[index] else {
                continue;
            };
            let at_path = if at.dev == dev {
                moved(&at.path)
            } else {
                at.path.clone()
            };
            let parent = new.iter().find(|p| p.id == *parent_id)?.clone();
            let below = at_path.strip_prefix(&parent.fs_root).ok()?;
            new[index].mount_point = joined(&parent.mount_point, below);
        }
    }
    Some(new)
}

/// The paths the enumeration from `roots` asks the table about (module
/// doc of `relevance`), by what it asks: at and below a root, everything
/// (the mount it is on, the way to it, the mounts inside it by path and by
/// place); on the way to a root or to a view `places_above` reports of a
/// directory above its place, the mount and the way; for a name directly in
/// one of those directories, whether it is a mount point (`MountedNames`).
#[derive(Debug, Default)]
struct Questions {
    below: BTreeSet<PathBuf>,
    way: BTreeSet<PathBuf>,
    names: BTreeSet<PathBuf>,
}

fn questions(tables: [&[MountEntry]; 2], roots: &[PathBuf]) -> Questions {
    let mut asked = Questions::default();
    let way = |asked: &mut Questions, dir: &Path| {
        for at in dir.ancestors() {
            asked.way.insert(at.to_owned());
            for name in NAMES {
                asked.names.insert(at.join(name));
            }
        }
    };
    for root in roots {
        if let Some(parent) = root.parent() {
            way(&mut asked, parent);
        }
        asked.below.insert(root.clone());
        for a in NAMES {
            asked.below.insert(root.join(a));
            for b in NAMES {
                asked.below.insert(root.join(a).join(b));
            }
        }
        for table in tables {
            if let Ok(above) = places_above(table, root) {
                for (_, view) in above {
                    way(&mut asked, &view);
                }
            }
        }
    }
    asked
}

fn answers(table: &[MountEntry], roots: &[PathBuf], asked: &Questions) -> Vec<String> {
    let mut out = Vec::new();
    for root in roots {
        out.push(format!("above {root:?}: {:?}", places_above(table, root)));
    }
    for path in &asked.below {
        out.push(format!(
            "{path:?}: on {:?}; within {:?}; screen {:?}",
            mount_for(table, path),
            mounts_within(table, path),
            screen_mount_chain(table, path),
        ));
    }
    for path in &asked.way {
        out.push(format!(
            "way {path:?}: on {:?}; screen {:?}",
            mount_for(table, path),
            screen_mount_chain(table, path),
        ));
    }
    for path in &asked.names {
        out.push(format!("name {path:?}: {}", is_mount_point(table, path)));
    }
    out
}

fn table_from(specs: &[Spec]) -> Vec<MountEntry> {
    let mut table = vec![mount(1, 0, "/", (0, 1), "/")];
    for (n, spec) in specs.iter().enumerate() {
        add(&mut table, 10 + n as u64, spec);
    }
    table
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4096))]

    /// Whenever `unchanged_for` says a change cannot matter to the roots,
    /// every question their enumeration asks of the table answers alike.
    #[test]
    fn unchanged_means_every_question_answers_alike(
        specs in prop::collection::vec(spec(), 0..7),
        roots in prop::collection::vec(names(1..=2), 1..=2),
        edits in prop::collection::vec(edit(), 1..=3),
    ) {
        let old = table_from(&specs);
        let roots: Vec<PathBuf> = roots.iter().map(|r| path_of(r)).collect();
        let anchors: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
        let mut new = old.clone();
        for (n, edit) in edits.iter().enumerate() {
            if let Some(next) = apply(&new, edit, 100 + n as u64) {
                // The rename oracle (mped review): a rename of a directory
                // at or above a reached place that moved any line is a
                // change, whatever the table questions answer (the roots'
                // own directories moved with it; no table question shows
                // that).
                if let Edit::Rename { dev, from, .. } = edit
                    && next != new
                    && let Ok(scope) = Scope::of(&new, &anchors)
                {
                    let dev = Dev { major: 0, minor: *dev };
                    let from = path_of(from);
                    if scope
                        .places
                        .iter()
                        .any(|place| place.dev == dev && place.path.starts_with(&from))
                    {
                        prop_assert!(
                            !unchanged_for(&new, &next, &anchors),
                            "rename of {} on {dev} above a reached place\nbefore {:#?}\nafter {:#?}",
                            from.display(),
                            new,
                            next
                        );
                    }
                }
                new = next;
            }
        }
        if unchanged_for(&old, &new, &anchors) {
            let paths = questions([&old, &new], &roots);
            let (before, after) = (answers(&old, &roots, &paths), answers(&new, &roots, &paths));
            let differing: Vec<_> = before.iter().zip(&after).filter(|(a, b)| a != b).collect();
            prop_assert!(differing.is_empty(), "old {:#?}\nnew {:#?}\n{:#?}", old, new, differing);
        }
    }
}

/// The property is not vacuous: random edits often leave a root's answers
/// alone and are then recognised as such, and often do not.
#[test]
fn the_generator_produces_both_verdicts() {
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    let strategy = (
        prop::collection::vec(spec(), 0..7),
        prop::collection::vec(names(1..=2), 1..=2),
        prop::collection::vec(edit(), 1..=3),
    );
    let (mut same, mut changed, mut differing) = (0, 0, 0);
    for _ in 0..2000 {
        let (specs, roots, edits) = strategy.new_tree(&mut runner).unwrap().current();
        let old = table_from(&specs);
        let mut new = old.clone();
        for (n, edit) in edits.iter().enumerate() {
            if let Some(next) = apply(&new, edit, 100 + n as u64) {
                new = next;
            }
        }
        if old == new {
            continue;
        }
        differing += 1;
        let roots: Vec<PathBuf> = roots.iter().map(|r| path_of(r)).collect();
        let anchors: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
        if unchanged_for(&old, &new, &anchors) {
            same += 1;
        } else {
            changed += 1;
        }
    }
    assert!(differing > 1000, "{differing}");
    assert!(
        same > 100 && changed > 100,
        "unchanged {same}, changed {changed}"
    );
}
