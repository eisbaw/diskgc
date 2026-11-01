//! Places through the mount types the safety rules name (docs/safety.md):
//! subdirectory and non-recursive binds, a mount whose parent another
//! namespace holds, overlayfs, btrfs subvolumes, and a filesystem mounted
//! inside container storage. Tables are written as mountinfo would show
//! them; nothing is looked up.

use super::*;

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
    }
}

fn place(dev: (u32, u32), path: &str) -> Place {
    Place {
        dev: Dev {
            major: dev.0,
            minor: dev.1,
        },
        path: PathBuf::from(path),
    }
}

const ROOT: (u32, u32) = (254, 1);
const HOME: (u32, u32) = (254, 2);

/// `/src` (a filesystem of its own) with a tmpfs at `/src/c/m`, and
/// `/alias`, a bind of `/src` without its submount (non-recursive), so
/// `/alias/c/m` is the uncovered directory under that tmpfs.
fn non_recursive() -> Vec<MountEntry> {
    vec![
        mount(1, 0, "/", ROOT, "/"),
        mount(2, 1, "/src", HOME, "/"),
        mount(3, 2, "/src/c/m", (0, 50), "/"),
        mount(4, 1, "/alias", HOME, "/"),
    ]
}

#[test]
fn a_subdirectory_bind_is_placed_by_its_root_field() {
    let mounts = non_recursive();
    assert_eq!(
        place_of(&mounts, Path::new("/alias/c/f")),
        Ok(place(HOME, "/c/f"))
    );
    assert_eq!(
        place_of(&mounts, Path::new("/src/c/m/x")),
        Ok(place((0, 50), "/x"))
    );
    let mut odd = mounts.clone();
    odd[3].fs_root = PathBuf::from("net:[4026531840]");
    assert!(
        place_of(&odd, Path::new("/alias/c"))
            .unwrap_err()
            .contains("not an absolute path")
    );
}

/// TASK-290: a mount point below the candidate by place, not by path.
#[test]
fn a_mount_point_reached_through_a_non_recursive_bind_is_within() {
    let mounts = non_recursive();
    let (mount, at) = mount_within(&mounts, Path::new("/alias/c"))
        .expect("placed")
        .expect("within");
    assert_eq!((mount.id, at.as_path()), (3, Path::new("/alias/c/m")));
    // The mount point itself, reached through the bind.
    let (mount, at) = mount_within(&mounts, Path::new("/alias/c/m"))
        .expect("placed")
        .expect("at");
    assert_eq!((mount.id, at.as_path()), (3, Path::new("/alias/c/m")));
    assert_eq!(mount_within(&mounts, Path::new("/alias/d")), Ok(None));
    // By path, as before.
    let (mount, at) = mount_within(&mounts, Path::new("/src/c"))
        .expect("placed")
        .expect("by path");
    assert_eq!((mount.id, at.as_path()), (3, Path::new("/src/c/m")));
}

/// A private bind of a subdirectory (propagation does not repeat the inner
/// mount there either): the mount below its source is within what the
/// bind shows. The bind's own mount point is one by path, as before.
#[test]
fn a_mount_below_a_subdirectory_binds_source_is_within_the_bind() {
    let mut mounts = non_recursive();
    mounts[3].fs_root = PathBuf::from("/c");
    assert_eq!(
        place_of(&mounts, Path::new("/alias/m")),
        Ok(place(HOME, "/c/m"))
    );
    mounts[2].mount_point = PathBuf::from("/src/c/k/m");
    let (mount, at) = mount_within(&mounts, Path::new("/alias/k"))
        .expect("placed")
        .expect("within");
    assert_eq!((mount.id, at.as_path()), (3, Path::new("/alias/k/m")));
    let (mount, at) = mount_within(&mounts, Path::new("/alias"))
        .expect("placed")
        .expect("a mount point");
    assert_eq!((mount.id, at.as_path()), (4, Path::new("/alias")));
    mounts[3].fs_root = PathBuf::from("/d");
    assert_eq!(mount_within(&mounts, Path::new("/alias/k")), Ok(None));
}

/// Limit: a mount whose parent the table does not list (made in another
/// mount namespace, or outside the process's root) is placed by its path
/// only; through a bind it is not seen.
#[test]
fn a_mount_with_an_unlisted_parent_is_seen_by_path_only() {
    let mut mounts = non_recursive();
    mounts[2].parent_id = 99;
    assert!(
        mount_within(&mounts, Path::new("/src/c"))
            .expect("placed")
            .is_some()
    );
    assert_eq!(mount_within(&mounts, Path::new("/alias/c")), Ok(None));
}

/// overlayfs shows its directories under its own device: a mount on the
/// merged view is no mount point of the upper directory (Linux removes the
/// upper entry; the overlay keeps its mount), so neither is within the
/// other by place.
#[test]
fn a_mount_on_an_overlay_is_not_within_its_upper_directory() {
    let mut mounts = vec![
        mount(1, 0, "/", ROOT, "/"),
        mount(2, 1, "/merged", (0, 60), "/"),
        mount(3, 2, "/merged/x/m", (0, 50), "/"),
    ];
    mounts[1].fs_type = "overlay".to_owned();
    assert_eq!(mount_within(&mounts, Path::new("/upper/x")), Ok(None));
    assert!(
        mount_within(&mounts, Path::new("/merged/x"))
            .expect("placed")
            .is_some()
    );
}

/// btrfs subvolumes share one device in mountinfo and differ by root field
/// (`/@`, `/@home`): places tell them apart, and a bind of a directory of
/// one sees a mount below its source.
#[test]
fn btrfs_subvolumes_are_told_apart_by_root_field() {
    const BTRFS: (u32, u32) = (0, 35);
    let mounts = vec![
        mount(1, 0, "/", BTRFS, "/@"),
        mount(2, 1, "/home", BTRFS, "/@home"),
        mount(3, 2, "/home/u/k/m", (0, 50), "/"),
        mount(4, 1, "/alias", BTRFS, "/@home/u"),
        mount(5, 1, "/other", BTRFS, "/@/u"),
    ];
    let (mount, at) = mount_within(&mounts, Path::new("/alias/k"))
        .expect("placed")
        .expect("within");
    assert_eq!((mount.id, at.as_path()), (3, Path::new("/alias/k/m")));
    assert_eq!(mount_within(&mounts, Path::new("/other/k")), Ok(None));
}

/// A mount on the candidate's filesystem whose parent cannot be placed
/// fails closed; one on another filesystem is not asked.
#[test]
fn an_unplaceable_mount_on_the_candidates_filesystem_is_an_error() {
    let mut mounts = non_recursive();
    mounts[2].mount_point = PathBuf::from("/elsewhere/m");
    assert!(
        mount_within(&mounts, Path::new("/alias/c"))
            .unwrap_err()
            .contains("cannot be placed on its parent /src")
    );
    let mut other = non_recursive();
    other.push(mount(5, 3, "/nowhere", (0, 51), "/"));
    assert_eq!(
        mount_within(&other, Path::new("/alias/d")),
        Ok(None),
        "a mount on the tmpfs is not on /alias/d's filesystem"
    );
}

fn views(above: &[(Place, PathBuf)]) -> Vec<&str> {
    above
        .iter()
        .map(|(_, view)| view.to_str().expect("UTF-8"))
        .collect()
}

/// TASK-267: a root that binds an image layer has the storage root above
/// its place, reached through `/home`.
#[test]
fn the_places_above_a_bind_of_a_layer_include_the_storage_root() {
    let storage = "/tester/.local/share/containers/storage";
    let mounts = vec![
        mount(1, 0, "/", ROOT, "/"),
        mount(2, 1, "/home", HOME, "/"),
        mount(
            3,
            1,
            "/mnt/l",
            HOME,
            &format!("{storage}/overlay/0a1b2c/diff"),
        ),
    ];
    let above = places_above(&mounts, Path::new("/mnt/l")).expect("placed");
    assert_eq!(
        views(&above),
        [
            "/",
            "/home",
            "/mnt",
            "/home/tester",
            "/home/tester/.local",
            "/home/tester/.local/share",
            "/home/tester/.local/share/containers",
            "/home/tester/.local/share/containers/storage",
            "/home/tester/.local/share/containers/storage/overlay",
            "/home/tester/.local/share/containers/storage/overlay/0a1b2c",
        ]
    );
    assert!(above.contains(&(
        place(HOME, storage),
        PathBuf::from(format!("/home{storage}"))
    )));
    // Without binds the places above are the path's own ancestors.
    let plain = places_above(&mounts, Path::new("/home/tester/x")).expect("placed");
    assert_eq!(views(&plain), ["/", "/home", "/home/tester"]);
}

/// A filesystem mounted inside storage (a dedicated disk at the graphroot's
/// `overlay`): the storage root is on the parent filesystem, above where
/// that mount is attached.
#[test]
fn the_places_above_reach_through_a_filesystem_mounted_inside_storage() {
    let mounts = vec![
        mount(1, 0, "/", ROOT, "/"),
        mount(2, 1, "/home", HOME, "/"),
        mount(3, 2, "/home/s/overlay", (254, 3), "/"),
        mount(4, 1, "/mnt/l", (254, 3), "/abc/diff"),
    ];
    let above = places_above(&mounts, Path::new("/mnt/l/x")).expect("placed");
    let views = views(&above);
    for wanted in ["/home/s", "/home/s/overlay", "/home/s/overlay/abc"] {
        assert!(views.contains(&wanted), "{wanted} not in {views:?}");
    }
    assert!(
        !views.contains(&"/mnt/l"),
        "the root's own place: {views:?}"
    );
}

/// A bind of an overlay's merged view (a container's root filesystem,
/// mounted inside storage): the overlay's mount inside storage shows the
/// same place, so the storage root is above it.
#[test]
fn the_places_above_a_bind_of_an_overlay_reach_its_other_mount() {
    let mut mounts = vec![
        mount(1, 0, "/", ROOT, "/"),
        mount(2, 1, "/home", HOME, "/"),
        mount(3, 2, "/home/s/overlay/abc/merged", (0, 60), "/"),
        mount(4, 1, "/mnt/m", (0, 60), "/"),
    ];
    mounts[2].fs_type = "overlay".to_owned();
    mounts[3].fs_type = "overlay".to_owned();
    let above = places_above(&mounts, Path::new("/mnt/m/etc")).expect("placed");
    let views = views(&above);
    assert!(views.contains(&"/home/s"), "{views:?}");
}

/// Limit: on a btrfs subvolume mounted alone, the top-level subvolume above
/// it is shown by no mount and left out; the other subvolume's directories
/// above the root are not above it.
#[test]
fn the_places_above_leave_out_what_no_mount_shows() {
    const BTRFS: (u32, u32) = (0, 35);
    let mounts = vec![
        mount(1, 0, "/", BTRFS, "/@"),
        mount(2, 1, "/home", BTRFS, "/@home"),
    ];
    let above = places_above(&mounts, Path::new("/home/u/x")).expect("placed");
    assert_eq!(views(&above), ["/", "/home", "/home/u"]);
    assert!(!above.iter().any(|(place, _)| place.path == Path::new("/")));
    // A container's bind volume: the host directories above it are not in
    // the container's table.
    let container = vec![
        mount(1, 999, "/", (0, 70), "/"),
        mount(2, 1, "/work", HOME, "/home/ci/builds/42"),
    ];
    let above = places_above(&container, Path::new("/work/x")).expect("placed");
    assert_eq!(views(&above), ["/", "/work"]);
}

/// A bind of an ancestor at its own descendant (a loop in the tree) ends.
#[test]
fn the_places_above_end_on_a_bind_loop() {
    let mounts = vec![mount(1, 0, "/", ROOT, "/"), mount(2, 1, "/a/b", ROOT, "/")];
    let above = places_above(&mounts, Path::new("/a/b/a/b/c")).expect("placed");
    assert!(!above.is_empty());
}

/// TASK-285: a rename above a bind's source changes its root field, and so
/// the ancestry of everything reached through it; a mount elsewhere does
/// not.
#[test]
fn an_ancestry_changes_with_a_rename_above_a_binds_source() {
    let then = vec![
        mount(1, 0, "/", ROOT, "/"),
        mount(2, 1, "/src", HOME, "/"),
        mount(3, 1, "/alias", HOME, "/work/sub"),
    ];
    let mut renamed = then.clone();
    renamed[2].fs_root = PathBuf::from("/keep-work/sub");
    let mut elsewhere = then.clone();
    elsewhere.push(mount(9, 1, "/media/usb", (8, 1), "/"));
    let cruft = Path::new("/alias/cruft");
    let before = ancestry(&then, cruft).expect("placed");
    assert_eq!(before.place, place(HOME, "/work/sub/cruft"));
    assert_ne!(ancestry(&renamed, cruft).expect("placed"), before);
    assert_eq!(ancestry(&elsewhere, cruft).expect("placed"), before);
    // A mount point's ancestor renamed: where the bind is attached moves.
    let mut moved = then.clone();
    moved[2].mount_point = PathBuf::from("/keep/alias");
    assert_ne!(
        ancestry(&moved, Path::new("/src/work/sub/cruft")).expect("placed"),
        ancestry(&then, Path::new("/src/work/sub/cruft")).expect("placed")
    );
}

/// TASK-286: the one rule of "the same filesystem". Every mount of one
/// superblock is the same space (a bind, a btrfs subvolume: mountinfo
/// names one device for them); writes elsewhere take nothing from it only
/// when independence is proved, never for an overlay, a stacking or FUSE
/// filesystem, a loop device's filesystem or another dataset of the same
/// ZFS pool, whichever of the two mounts it is.
#[test]
fn one_superblock_is_one_space_and_independence_must_be_proved() {
    let typed = |id, point: &str, dev: (u32, u32), root: &str, fs_type: &str, source: &str| {
        let mut m = mount(id, 1, point, dev, root);
        m.fs_type = fs_type.to_owned();
        m.source = source.to_owned();
        m
    };
    let home = typed(2, "/home", (0, 40), "/@home", "btrfs", "/dev/nvme0n1p2");
    let work = typed(3, "/work", (0, 40), "/@work", "btrfs", "/dev/nvme0n1p2");
    let bind = typed(
        4,
        "/alias",
        (0, 40),
        "/@home/mark",
        "btrfs",
        "/dev/nvme0n1p2",
    );
    let data = typed(5, "/data", (254, 3), "/", "ext4", "/dev/sda1");
    let tmp = typed(6, "/tmp", (0, 42), "/", "tmpfs", "tmpfs");
    for other in [&work, &bind] {
        assert!(same_space(&home, other));
        assert!(shared_space(other, &home).is_some_and(|why| why.contains("same filesystem")));
    }
    for other in [&data, &tmp] {
        assert!(!same_space(&home, other));
        assert_eq!(shared_space(other, &home), None);
        assert_eq!(shared_space(&home, other), None);
    }
    let overlay = typed(7, "/var/state", (0, 60), "/", "overlay", "overlay");
    let ecryptfs = typed(8, "/home/u", (0, 61), "/", "ecryptfs", "/home/.u");
    let fuse = typed(9, "/mnt/b", (0, 62), "/", "fuse.bindfs", "/data/x");
    let image = typed(10, "/mnt/img", (7, 0), "/", "ext4", "/dev/loop0");
    for stacked in [&overlay, &ecryptfs, &fuse, &image] {
        assert!(!same_space(stacked, &data), "{stacked:?}");
        assert!(shared_space(stacked, &data).is_some(), "{stacked:?}");
        assert!(shared_space(&data, stacked).is_some(), "{stacked:?}");
    }
    let tank_a = typed(11, "/tank/a", (0, 70), "/", "zfs", "tank/a");
    let tank_b = typed(12, "/tank/b", (0, 71), "/", "zfs", "tank/b");
    let other_pool = typed(13, "/pool/c", (0, 72), "/", "zfs", "pool/c");
    assert!(!same_space(&tank_a, &tank_b));
    assert!(shared_space(&tank_b, &tank_a).is_some_and(|why| why.contains("ZFS pool tank")));
    assert_eq!(shared_space(&other_pool, &tank_a), None);
    assert_eq!(
        space_of(&home).dev(),
        Dev {
            major: 0,
            minor: 40
        }
    );
}
