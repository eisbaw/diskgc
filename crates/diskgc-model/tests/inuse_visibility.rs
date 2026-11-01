//! What the in-use scan cannot learn from processes (TASK-263, TASK-169;
//! docs/safety.md, In use: Visibility and Loop devices): a process list the
//! held `/proc` cannot show whole (`hidepid`, a PID namespace other than
//! the initial one), and a file the kernel holds for a loop device. Each
//! rule has a scenario here that fails when the rule is removed; the
//! real-kernel side of visibility is in diskgc-e2e (`owner_pid_suite`,
//! `release_suite`). No rootless container can attach a loop device, so
//! the loop rules rest on these scenarios and on `LinuxHost`'s listing
//! test.

mod common;

use std::path::{Path, PathBuf};

use common::{dir, host};
use diskgc_core::engine::{Candidate, Reason, Rules, Skip, UnknownCause, walk};
use diskgc_core::host::Host;
use diskgc_core::inuse::{self, Options, Scan, ScanError};
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;

const ME: u32 = 1000;

/// `/tmp` (tmpfs, `0:42`) holding the candidate `/tmp/c` (with an image
/// file `disk.img`) and a bystander `/tmp/keep`; `/srv` on `/`. `top` goes
/// before `tree:` (top-level keys), `tree` below it (tree keys indented by
/// two).
fn yaml(top: &str, tree: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, parent: 1}}
{top}
tree:
  /usr/bin/tool: {{}}
  /srv/other.img: {{size: 1M}}
  /tmp/keep: {{size: 1K}}
  /tmp/c:
    disk.img: {{size: 1M}}
    sub:
      f: {{size: 4K}}
{tree}
processes:
  - {{pid: 7, exe: /usr/bin/tool, maps: [/usr/bin/tool], environ: [PATH=/usr/bin], fds: [{{path: /tmp/keep}}]}}
"#
    )
}

fn model(top: &str, tree: &str) -> ModelHost {
    host(&yaml(top, tree))
}

fn options() -> Options<'static> {
    Options {
        users: &[ME],
        sockets: false,
        pid_lookup: false,
        pytest_facts: false,
    }
}

/// Walks `/tmp/c`, scans and checks it.
fn check_c(host: &ModelHost) -> (Result<(), Skip>, Scan) {
    let parent = dir(host, "/tmp");
    let path = PathBuf::from("/tmp/c");
    let candidate = Candidate::new(&parent, &path).expect("a name");
    let record = walk(host, &candidate, &Rules::new(&[ME]), None).expect("walk");
    let scan = inuse::scan(host, &options(), None).expect("scan");
    (scan.check(&candidate, &record), scan)
}

// ---- Visibility (TASK-263) ----

/// Under `hidepid`, and in a PID namespace other than the initial one, the
/// listing can lack a process using the candidate (a `newgrp` shell, a
/// process outside the container): an unused candidate is
/// `in_use_unknown`, never cleared, and the detail names the rule. Inside
/// a marked test container of a development build the own namespace is
/// taken as complete; `hidepid` there still blocks.
#[test]
fn an_incomplete_process_list_clears_nothing() {
    for (top, blocked) in [
        ("", None),
        ("pid_namespace: test_container", None),
        (
            "pid_namespace: nested",
            Some("diskgc is not in the initial PID namespace"),
        ),
        (
            "proc_hidepid: true",
            Some("the held /proc is mounted with hidepid"),
        ),
        (
            "pid_namespace: test_container\nproc_hidepid: true",
            Some("the held /proc is mounted with hidepid"),
        ),
    ] {
        let host = model(top, "");
        let (result, scan) = check_c(&host);
        match blocked {
            None => assert_eq!(result, Ok(()), "{top:?}"),
            Some(rule) => {
                let skip = result.expect_err(top);
                assert_eq!(skip.reason, Reason::InUseUnknown, "{top:?}: {skip}");
                assert!(skip.detail.starts_with(rule), "{top:?}: {skip}");
                assert!(
                    skip.process_blockers.as_ref().is_none_or(Vec::is_empty),
                    "{top:?}: {skip}"
                );
                assert_eq!(scan.blocked().as_deref(), Some(skip.detail.as_str()));
            }
        }
    }
}

/// The facts are read before the first listing and after the last read:
/// a failed read, either one, makes the list unknown (a `/proc` remounted
/// with `hidepid` mid-scan is a change between the two).
#[test]
fn facts_unread_before_or_after_the_listings_clear_nothing() {
    for nth in [1, 2] {
        let top = format!(
            "events:\n  - {{at: {{op: pid_namespace, nth: {nth}, when: before}}, fail: EIO}}"
        );
        let host = model(&top, "");
        let (result, _) = check_c(&host);
        let skip = result.expect_err("unknown facts");
        assert_eq!(skip.reason, Reason::InUseUnknown, "read {nth}: {skip}");
        assert!(
            skip.detail.starts_with(
                "the PID namespace facts are unknown, so the in-use check cannot see every process"
            ),
            "read {nth}: {skip}"
        );
    }
    let host = model("", "");
    check_c(&host).0.expect("clear without the failures");
    assert_eq!(host.trace_count(Op::PidNamespace), 2, "before and after");
}

/// Out of descriptors while reading the facts is an error of the run
/// (TASK-185), not an unknown list.
#[test]
fn out_of_descriptors_reading_the_facts_is_a_run_error() {
    let host = model(
        "events:\n  - {at: {op: pid_namespace, nth: 1, when: before}, fail: EMFILE}",
        "",
    );
    let error = inuse::scan(&host, &options(), None).expect_err("run error");
    assert!(matches!(error, ScanError::Failed(e) if e.is_out_of_descriptors()));
}

// ---- Loop devices (TASK-169) ----

/// Walks and checks the candidate `path` below `parent` against a fresh
/// scan.
fn check_at(host: &ModelHost, parent: &str, path: &str) -> Result<(), Skip> {
    let parent = dir(host, parent);
    let path = PathBuf::from(path);
    let candidate = Candidate::new(&parent, &path).expect("a name");
    let record = walk(host, &candidate, &Rules::new(&[ME]), None).expect("walk");
    let scan = inuse::scan(host, &options(), None).expect("scan");
    scan.check(&candidate, &record)
}

fn loop_unknown(result: Result<(), Skip>, what: &str) -> Skip {
    let skip = result.expect_err(what);
    assert_eq!(skip.reason, Reason::InUseUnknown, "{what}: {skip}");
    assert_eq!(
        skip.unknown_cause,
        Some(UnknownCause::LoopDevice),
        "{what}: {skip}"
    );
    skip
}

/// The device's own word decides when diskgc can open it
/// (`LOOP_GET_STATUS64`: `readable`, or root): a file of the candidate
/// keeps it (`in_use`), by whichever name the candidate holds it (a hard
/// link); one elsewhere leaves it clear. When diskgc cannot open the
/// device, the name proves nothing and an owner's file it reaches could be
/// a decoy: every candidate is `in_use_unknown` (cause `loop_device`).
#[test]
fn a_loop_device_backed_by_a_file_of_the_candidate_keeps_it() {
    for (top, tree, verdict) in [
        (
            "loops:\n  - {dev: \"7:0\", backing: /tmp/c/disk.img, readable: true}",
            "",
            Some("/tmp/c/disk.img"),
        ),
        // A hard link: the device names the file by the other name.
        (
            "loops:\n  - {dev: \"7:0\", backing: /tmp/disk-link.img, readable: true}",
            "  /tmp/disk-link.img: {link: /tmp/c/disk.img}",
            Some("/tmp/c/disk.img"),
        ),
        (
            "loops:\n  - {dev: \"7:0\", backing: /srv/other.img, readable: true}",
            "",
            None,
        ),
        // As root diskgc opens every device.
        (
            "own_uid: 0\nloops:\n  - {dev: \"7:0\", backing: /tmp/c/disk.img}",
            "",
            Some("/tmp/c/disk.img"),
        ),
    ] {
        let host = model(top, tree);
        let (result, _) = check_c(&host);
        match verdict {
            Some(at) => {
                let skip = result.expect_err(top);
                assert_eq!(skip.reason, Reason::InUse, "{top:?}: {skip}");
                assert_eq!(skip.path, Path::new(at), "{top:?}: {skip}");
                assert!(
                    skip.detail.starts_with("loop device 7:0 is backed by it"),
                    "{top:?}: {skip}"
                );
            }
            None => assert_eq!(result, Ok(()), "{top:?}"),
        }
    }
    for backing in ["/tmp/c/disk.img", "/srv/other.img"] {
        let host = model(
            &format!("loops:\n  - {{dev: \"7:0\", backing: {backing}}}"),
            "",
        );
        let skip = loop_unknown(check_c(&host).0, backing);
        assert!(
            skip.detail
                .starts_with(&format!("loop device 7:0 is backed by {backing}"))
                && skip.detail.contains("losetup -d"),
            "{skip}"
        );
    }
}

/// Codex 2 (TASK-263 review): the device opened `/tmp/private/image`, a
/// name diskgc cannot look up (`EACCES`), while a hard link of the same
/// file lies in the candidate. Logging and clearing would delete the
/// device's file. Unopenable: `in_use_unknown`; openable: `in_use`.
#[test]
fn an_unreachable_backing_name_with_a_link_in_the_candidate_keeps_it() {
    let tree = "  /tmp/private/image: {size: 1M}\n  /tmp/c/image-link: {link: /tmp/private/image}";
    let unreachable =
        "events:\n  - {at: {op: openat2, path: /tmp/private/image, when: before}, fail: EACCES}";
    let host = model(
        &format!("loops:\n  - {{dev: \"7:0\", backing: /tmp/private/image}}\n{unreachable}"),
        tree,
    );
    let skip = loop_unknown(check_c(&host).0, "unreachable");
    assert!(skip.detail.contains("reaches nothing here"), "{skip}");
    let host = model(
        "loops:\n  - {dev: \"7:0\", backing: /tmp/private/image, readable: true}",
        tree,
    );
    let skip = check_c(&host).0.expect_err("in use");
    assert_eq!(skip.reason, Reason::InUse, "{skip}");
    assert_eq!(skip.path, Path::new("/tmp/c/image-link"), "{skip}");
}

/// Codex 3 (TASK-263 review): `/tmp/src` is bound at `/alias`, the device
/// opened `/tmp/src/c/image`, and a decoy directory was mounted over
/// `/tmp/src` since, holding another `c/image`. The name now reaches the
/// decoy while the real file lies in the candidate `/alias/c`. Unopenable:
/// both candidates `in_use_unknown`; openable: the real one `in_use`, the
/// decoy's clear.
#[test]
fn a_decoy_mounted_over_the_backing_name_does_not_clear_the_real_file() {
    let decoy = |readable: bool| {
        yaml(
            &format!(
                "loops:\n  - {{dev: \"7:0\", backing: /tmp/src/c/image, file: /alias/c/image, readable: {readable}}}"
            ),
            "  /alias/c/image: {size: 1M}\n  /tmp/src/c/image: {size: 1M}",
        )
        .replacen(
            "  - {id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 1}\n",
            "  - {id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 1}\n  - {id: 31, at: /alias, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 1, bind_of: 30, bind_root: /src}\n  - {id: 32, at: /tmp/src, fs: tmpfs, dev: \"0:43\", size: 1G, parent: 30}\n",
            1,
        )
    };
    let text = decoy(false);
    let host = host(&text);
    let skip = loop_unknown(check_at(&host, "/alias", "/alias/c"), "real, unopenable");
    assert!(
        skip.detail
            .contains("its name reaches a file the run's owners hold"),
        "the decoy, not a failed lookup: {skip}"
    );
    let host = common::host(&text);
    loop_unknown(
        check_at(&host, "/tmp/src", "/tmp/src/c"),
        "decoy, unopenable",
    );
    let host = common::host(&decoy(true));
    let skip = check_at(&host, "/alias", "/alias/c").expect_err("real file");
    assert_eq!(skip.reason, Reason::InUse, "{skip}");
    assert_eq!(skip.path, Path::new("/alias/c/image"), "{skip}");
    let host = common::host(&decoy(true));
    assert_eq!(check_at(&host, "/tmp/src", "/tmp/src/c"), Ok(()));
}

/// A device diskgc cannot open (root's snap images) whose name reaches a
/// file no owner of the run holds keeps nothing: the walk admits only the
/// owners' entries, so no candidate can hold that file. Noted.
#[test]
fn a_loop_on_another_users_file_does_not_block_the_run() {
    let host = model(
        "loops:\n  - {dev: \"7:0\", backing: /var/lib/snapd/snaps/core_1.snap}",
        "  /var/lib/snapd/snaps/core_1.snap: {size: 1M, uid: 0}",
    );
    let (result, scan) = check_c(&host);
    assert_eq!(result, Ok(()));
    assert!(
        scan.notes()
            .iter()
            .any(|note| note.starts_with("loop device 7:0")
                && note.contains("uid 0, which no candidate of this run may hold")),
        "{:?}",
        scan.notes()
    );
}

/// TASK-299 (Codex, full review #3): a root-owned image the device holds,
/// `/srv/build/c/disk.img`, shown by an idmapped bind (`/mnt/u`, stored 0
/// shown as 1000) as the user's, inside the candidate `/mnt/u/c`. Its name
/// reaches it through `/`, where it is root's, but the walk through the
/// bind sees uid 1000 everywhere and admits it: the owner rule cannot
/// clear a candidate on a mount that may show other owners. Candidates on
/// a mount that shows owners as stored (`/tmp/c`) stay clear.
#[test]
fn a_foreign_image_shown_as_an_owners_by_an_idmapped_bind_is_not_cleared() {
    let idmapped = |idmap: &str| {
        yaml(
            "loops:\n  - {dev: \"7:0\", backing: /srv/build/c/disk.img}",
            "  /srv/build/c: {.: {uid: 0, gid: 0}, disk.img: {size: 1M, uid: 0, gid: 0}, f: {uid: 0, gid: 0}}",
        )
        .replacen(
            "  - {id: 30,",
            &format!(
                "  - {{id: 31, at: /mnt/u, fs: ext4, dev: \"254:1\", size: 100G, parent: 1, bind_of: 1, bind_root: /srv/build{idmap}}}\n  - {{id: 30,"
            ),
            1,
        )
    };
    let host = common::host(&idmapped(", idmap: [{stored: 0, shown: 1000}]"));
    let skip = loop_unknown(check_at(&host, "/mnt/u", "/mnt/u/c"), "idmapped");
    assert!(
        skip.detail
            .starts_with("loop device 7:0 is backed by /srv/build/c/disk.img")
            && skip.detail.contains("/mnt/u"),
        "{skip}"
    );
    let host = common::host(&idmapped(", idmap: [{stored: 0, shown: 1000}]"));
    assert_eq!(check_c(&host).0, Ok(()), "a plain mount shows uid 0");
}

/// TASK-299: the other way round. The device opened the user's
/// `/tmp/c/disk.img` through an idmapped bind of `/tmp` (`/mnt/r`, stored
/// 1000 shown as 0), so its name reaches a file of uid 0, while the walk of
/// `/tmp/c` sees the stored owner, 1000. An owner read through a mount that
/// may show other owners proves nothing: every candidate.
#[test]
fn a_backing_name_read_through_an_idmapped_mount_proves_no_owner() {
    let text = yaml(
        "loops:\n  - {dev: \"7:0\", backing: /mnt/r/c/disk.img}",
        "",
    )
    .replacen(
        "  - {id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 1}\n",
        "  - {id: 30, at: /tmp, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 1}\n  - {id: 31, at: /mnt/r, fs: tmpfs, dev: \"0:42\", size: 1G, parent: 1, bind_of: 30, idmap: [{stored: 1000, shown: 0}]}\n",
        1,
    );
    let host = common::host(&text);
    let skip = loop_unknown(check_c(&host).0, "read through an idmapped mount");
    assert!(skip.detail.contains("/mnt/r"), "{skip}");
}

/// TASK-299: an overlay may show its layers' files under other owners
/// (an idmapped layer is a private mount the table does not list), so a
/// candidate on an overlay is not cleared by the owner rule either.
#[test]
fn a_foreign_image_does_not_clear_a_candidate_on_an_overlay() {
    let text = yaml(
        "loops:\n  - {dev: \"7:0\", backing: /var/lib/snapd/snaps/core_1.snap}",
        "  /var/lib/snapd/snaps/core_1.snap: {size: 1M, uid: 0}\n  /data/upper/c/f: {size: 1K}",
    )
    .replacen(
        "  - {id: 30,",
        "  - {id: 60, at: /merged, fs: overlay, dev: \"0:60\", size: 1G, parent: 1, overlay_upper: /data/upper}\n  - {id: 30,",
        1,
    );
    let host = common::host(&text);
    loop_unknown(check_at(&host, "/merged", "/merged/c"), "on an overlay");
    let host = common::host(&text);
    assert_eq!(check_c(&host).0, Ok(()), "on a plain mount");
}

/// TASK-299 (Codex, full review #3): the device opened `/merged/img`
/// through an overlay whose upper directory is `/data/upper` on `/`; the
/// merged name was removed while a hard link survives at
/// `/data/keep/c/img-link`, on `/` but outside the upper directory. The
/// surviving parent `/merged` reports the overlay's device, the link the
/// underlying one: scoping the doubt to the overlay's device would clear
/// the link's last name. A deleted name on a filesystem stored in another's
/// directories leaves every candidate in doubt.
#[test]
fn a_deleted_name_on_an_overlay_keeps_every_candidate_in_doubt() {
    let text = |loops: &str| {
        yaml(
            &format!("{loops}\nevents:\n  - {{at: {{op: loop_backing_files, when: before}}, remove: /merged/img}}"),
            "  /data/upper/img: {size: 1M}\n  /data/keep/c/img-link: {link: /data/upper/img}",
        )
        .replacen(
            "  - {id: 30,",
            "  - {id: 60, at: /merged, fs: overlay, dev: \"0:60\", size: 1G, parent: 1, overlay_upper: /data/upper}\n  - {id: 30,",
            1,
        )
    };
    let host = common::host(&text("loops:\n  - {dev: \"7:1\", backing: /merged/img}"));
    let skip = loop_unknown(
        check_at(&host, "/data/keep", "/data/keep/c"),
        "deleted on an overlay",
    );
    assert!(
        skip.detail
            .contains("loop device 7:1 is backed by /merged/img (deleted)")
            && skip.detail.contains("overlay"),
        "{skip}"
    );
}

/// TASK-299 audit of the device's own word: `LOOP_GET_STATUS64` answers
/// with the (device, inode) of the file as the mount it was opened through
/// shows it (`vfs_getattr` of its path), and overlayfs shows a file under
/// its own device (or a layer's anonymous one), never the underlying
/// filesystem's. The device opened `/merged/img`; its only other name is
/// `/data/keep/c/img-link`, which the walk sees on `/`. The identity
/// matches no object there, so a device on a filesystem stored in
/// another's directories leaves every candidate in doubt, whatever its
/// name says (here a rendering from another namespace, `/ns/img`). An
/// identity whose file is reached on a plain mount is still exact.
#[test]
fn a_device_identity_on_an_overlay_keeps_every_candidate_in_doubt() {
    let text = |backing: &str, file: &str| {
        yaml(
            &format!(
                "loops:\n  - {{dev: \"7:1\", backing: {backing}, file: {file}, readable: true}}"
            ),
            "  /data/upper/img: {size: 1M}\n  /data/keep/c/img-link: {link: /data/upper/img}",
        )
        .replacen(
            "  - {id: 30,",
            "  - {id: 60, at: /merged, fs: overlay, dev: \"0:60\", size: 1G, parent: 1, overlay_upper: /data/upper}\n  - {id: 30,",
            1,
        )
    };
    let host = common::host(&text("/ns/img", "/merged/img"));
    let skip = loop_unknown(
        check_at(&host, "/data/keep", "/data/keep/c"),
        "identity on an overlay",
    );
    assert!(skip.detail.contains("overlay"), "{skip}");
    // Opened on `/`: the identity is the link's own, `in_use`.
    let host = common::host(&text("/ns/img", "/data/upper/img"));
    let skip = check_at(&host, "/data/keep", "/data/keep/c").expect_err("in use");
    assert_eq!(skip.reason, Reason::InUse, "{skip}");
}

/// A device diskgc cannot open whose name was deleted: another link of the
/// file can only be on the filesystem of the name's surviving directory,
/// so candidates there are `in_use_unknown` and others are clear (mped
/// option b). With no surviving directory, every candidate.
#[test]
fn a_deleted_backing_name_blocks_only_its_filesystem() {
    let tree = "  /srv/c/f: {size: 1K}\n  /tmp/old: {.: {}}";
    let host = model(
        "loops:\n  - {dev: \"7:1\", backing: \"/tmp/old/x.img (deleted)\"}",
        tree,
    );
    let skip = loop_unknown(check_c(&host).0, "same filesystem");
    assert!(
        skip.detail
            .contains("loop device 7:1 is backed by /tmp/old/x.img (deleted)")
            && skip.detail.contains("the filesystem of /tmp/old"),
        "{skip}"
    );
    let host = model(
        "loops:\n  - {dev: \"7:1\", backing: \"/tmp/old/x.img (deleted)\"}",
        tree,
    );
    assert_eq!(
        check_at(&host, "/srv", "/srv/c"),
        Ok(()),
        "another filesystem"
    );
    for top in [
        "loops:\n  - {dev: \"7:1\", backing: \"/tmp/gone/x.img (deleted)\"}",
        "loops:\n  - {dev: \"7:1\", backing: \"x.img (deleted)\"}",
    ] {
        let host = model(top, tree);
        loop_unknown(check_at(&host, "/srv", "/srv/c"), top);
    }
}

/// A mount on a loop device is listed too; diskgc cannot open it, and its
/// image is an owner's file: every candidate `in_use_unknown`.
#[test]
fn a_mounted_loop_image_is_listed() {
    let text = yaml("", "").replacen(
        "  - {id: 30,",
        "  - {id: 31, at: /mnt/img, fs: ext4, dev: \"7:3\", size: 1G, parent: 1, loop_backing: /tmp/c/disk.img}\n  - {id: 30,",
        1,
    );
    let host = host(&text);
    let skip = loop_unknown(check_c(&host).0, "mounted");
    assert!(skip.detail.starts_with("loop device 7:3"), "{skip}");
}

/// mped S4: the model's text follows the entry the device opened, as
/// `d_path` follows its dentry: a rename of it shows the new path;
/// unlinking it shows ` (deleted)` even while another link remains.
#[test]
fn the_backing_text_follows_the_opened_name() {
    let dev = diskgc_core::host::Dev { major: 7, minor: 0 };
    let read = |host: &ModelHost| host.loop_backing_file(dev).expect("read").expect("bound");
    let host = model(
        "loops:\n  - {dev: \"7:0\", backing: /tmp/c/disk.img}\nevents:\n  - {at: {op: loop_backing_files, when: after}, rename: {from: /tmp/c/disk.img, to: /tmp/c/sub/disk.img}}",
        "  /tmp/c/other-link.img: {link: /tmp/c/disk.img}",
    );
    host.loop_backing_files().expect("listing");
    assert_eq!(read(&host), Path::new("/tmp/c/sub/disk.img"));
    let host = model(
        "loops:\n  - {dev: \"7:0\", backing: /tmp/c/disk.img}\nevents:\n  - {at: {op: loop_backing_files, when: after}, remove: /tmp/c/disk.img}",
        "  /tmp/c/other-link.img: {link: /tmp/c/disk.img}",
    );
    host.loop_backing_files().expect("listing");
    assert_eq!(read(&host), Path::new("/tmp/c/disk.img (deleted)"));
}

/// A name that reaches nothing is read again: a root-owned image renamed
/// between the listing and the lookup is found under its new name and,
/// no owner's file, blocks nothing. Without the re-read it would block
/// every candidate.
#[test]
fn a_backing_name_is_read_again_before_it_blocks() {
    let host = model(
        "loops:\n  - {dev: \"7:0\", backing: /var/img/a.img}\nevents:\n  - {at: {op: loop_backing_files, when: after}, rename: {from: /var/img/a.img, to: /var/img/b.img}}",
        "  /var/img/a.img: {size: 1M, uid: 0}",
    );
    assert_eq!(check_c(&host).0, Ok(()));
    assert_eq!(host.trace_count(Op::LoopBackingFile), 1, "one re-read");
    // Anonymous objects (no directory names them) block nothing.
    let host = model(
        "loops:\n  - {dev: \"7:2\", backing: \"anon_inode:[loop]\"}",
        "",
    );
    assert_eq!(check_c(&host).0, Ok(()));
}

/// A listing that fails, or a backing file that cannot be read, is an
/// error of the run, never a shorter list.
#[test]
fn an_unreadable_loop_listing_is_a_run_error() {
    for top in [
        "events:\n  - {at: {op: loop_backing_files, when: before}, fail: EIO}".to_owned(),
        // A mounted loop device without a readable backing file.
        String::new(),
    ] {
        let mut text = yaml(&top, "");
        if top.is_empty() {
            text = text.replacen(
                "  - {id: 30,",
                "  - {id: 31, at: /mnt/img, fs: ext4, dev: \"7:3\", size: 1G, parent: 1}\n  - {id: 30,",
                1,
            );
        }
        let host = host(&text);
        let error = inuse::scan(&host, &options(), None).expect_err("run error");
        assert!(matches!(error, ScanError::Failed(_)), "{top:?}: {error}");
    }
}

/// No loop device configured: one listing, no mount table read for it.
#[test]
fn no_loop_device_costs_one_listing() {
    let host = model("", "");
    check_c(&host).0.expect("clear");
    assert_eq!(host.trace_count(Op::LoopBackingFiles), 1);
    assert_eq!(host.trace_count(Op::LoopBackingFile), 0);
    assert!(host.loop_backing_files().expect("listing").is_empty());
}

/// mped N2: the test-container exception exists only where the gate lets
/// the e2e build delete, so the model refuses it with the gate closed.
#[test]
fn a_test_container_namespace_needs_the_gate_open() {
    let Err(error) = ModelHost::from_yaml(&yaml(
        "pid_namespace: test_container\nsandbox_gate: closed",
        "",
    )) else {
        panic!("a test container with the gate closed loaded");
    };
    assert!(
        error
            .to_string()
            .contains("test_container needs the sandbox gate open"),
        "{error}"
    );
    assert!(
        ModelHost::from_yaml(&yaml(
            "pid_namespace: test_container\nsandbox_gate: refuses_deletes",
            "",
        ))
        .is_ok(),
        "may_delete answers open there"
    );
}
