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
use diskgc_core::engine::{Candidate, Reason, Rules, Skip, walk};
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
                "PID namespace unreadable, so the in-use check cannot see every process"
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

/// A loop device backed by a file of the candidate keeps it (`in_use`),
/// whichever name the candidate holds it by: the device holds the file
/// open in the kernel, where no process shows it. One backed by a file
/// elsewhere leaves the candidate clear.
#[test]
fn a_loop_device_backed_by_a_file_of_the_candidate_keeps_it() {
    for (top, tree, at) in [
        (
            "loops:\n  - {dev: \"7:0\", backing: /tmp/c/disk.img}",
            "",
            Some("/tmp/c/disk.img"),
        ),
        // A hard link: the device names the file by the other name.
        (
            "loops:\n  - {dev: \"7:0\", backing: /tmp/disk-link.img}",
            "  /tmp/disk-link.img: {link: /tmp/c/disk.img}",
            Some("/tmp/c/disk.img"),
        ),
        (
            "loops:\n  - {dev: \"7:0\", backing: /srv/other.img}",
            "",
            None,
        ),
    ] {
        let host = model(top, tree);
        let (result, _) = check_c(&host);
        match at {
            Some(at) => {
                let skip = result.expect_err(top);
                assert_eq!(skip.reason, Reason::InUse, "{top:?}: {skip}");
                assert_eq!(skip.path, Path::new(at), "{top:?}: {skip}");
                assert!(
                    skip.detail.starts_with("loop device 7:0 is backed by it"),
                    "{top:?}: {skip}"
                );
                assert!(
                    skip.process_blockers.as_ref().is_none_or(Vec::is_empty),
                    "{top:?}: {skip}"
                );
            }
            None => assert_eq!(result, Ok(()), "{top:?}"),
        }
    }
}

/// A mount on a loop device whose image is in the candidate keeps it too:
/// the listing covers mounted and unmounted devices alike.
#[test]
fn a_mounted_loop_image_in_the_candidate_keeps_it() {
    let text = yaml("", "").replacen(
        "  - {id: 30,",
        "  - {id: 31, at: /mnt/img, fs: ext4, dev: \"7:3\", size: 1G, parent: 1, loop_backing: /tmp/c/disk.img}\n  - {id: 30,",
        1,
    );
    let host = host(&text);
    let skip = check_c(&host).0.expect_err("in use");
    assert_eq!(skip.reason, Reason::InUse, "{skip}");
    assert!(skip.detail.starts_with("loop device 7:3"), "{skip}");
}

/// A backing file whose name was deleted cannot be identified, and another
/// name of it (a hard link) could be in any candidate: every candidate is
/// `in_use_unknown`, naming the device and how to end it.
#[test]
fn a_deleted_backing_file_blocks_every_candidate() {
    let host = model(
        "loops:\n  - {dev: \"7:1\", backing: \"/var/tmp/old.img (deleted)\"}",
        "",
    );
    let (result, scan) = check_c(&host);
    let skip = result.expect_err("unknown");
    assert_eq!(skip.reason, Reason::InUseUnknown, "{skip}");
    assert!(
        skip.detail.starts_with(inuse::loops::LOOP_UNKNOWN)
            && skip.detail.contains("loop device 7:1")
            && skip.detail.contains("losetup -d"),
        "{skip}"
    );
    assert!(scan.blocked().is_some());
}

/// The kernel's text follows the file: deleted under a live device it
/// reads ` (deleted)` (here diskgc's own earlier deletion is modelled by an
/// event), renamed it reads the new name, which the scan looks up again.
#[test]
fn the_backing_text_follows_renames_and_deletions() {
    // Renamed between the listing and the lookup: the listed name names
    // nothing, and the re-read finds the new one.
    let host = model(
        "loops:\n  - {dev: \"7:0\", backing: /tmp/c/disk.img}\nevents:\n  - {at: {op: loop_backing_files, when: after}, rename: {from: /tmp/c/disk.img, to: /tmp/c/sub/disk.img}}",
        "",
    );
    let skip = check_c(&host).0.expect_err("in use");
    assert_eq!(skip.reason, Reason::InUse, "{skip}");
    // The walk, made before the rename, names the object.
    assert_eq!(skip.path, Path::new("/tmp/c/disk.img"), "{skip}");
    assert_eq!(host.trace_count(Op::LoopBackingFile), 1, "one re-read");
    // Deleted (its only name): ` (deleted)`.
    let host = model(
        "loops:\n  - {dev: \"7:0\", backing: /srv/other.img}\nevents:\n  - {at: {op: loop_backing_files, when: before}, remove: /srv/other.img}",
        "",
    );
    let skip = check_c(&host).0.expect_err("unknown");
    assert_eq!(skip.reason, Reason::InUseUnknown, "{skip}");
    assert!(skip.detail.contains("/srv/other.img (deleted)"), "{skip}");
}

/// A text that names nothing the walk could reach (another mount
/// namespace's path, an anonymous file) and stays the same on a second
/// read is noted, not blocking: no candidate holds the file by that name.
#[test]
fn a_backing_file_named_nowhere_here_is_noted() {
    for backing in ["/var/lib/containers/disk.img", "anon_inode:[loop]"] {
        let host = model(
            &format!("loops:\n  - {{dev: \"7:2\", backing: \"{backing}\"}}"),
            "",
        );
        let (result, scan) = check_c(&host);
        assert_eq!(result, Ok(()), "{backing}");
        assert!(
            scan.notes()
                .iter()
                .any(|note| note.starts_with("loop device 7:2")
                    && note.contains(backing)
                    && note.contains("another mount namespace")),
            "{backing}: {:?}",
            scan.notes()
        );
    }
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
