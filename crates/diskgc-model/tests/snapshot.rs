//! The index snapshot (TASK-30; docs/architecture.md, The tally: Snapshot;
//! docs/testing.md, index scenarios: snapshot), through `ModelHost`'s state
//! operations: a reclaiming `ensure` that tallies writes the snapshot, the
//! next run starts from it; a corrupt, mismatched, foreign, symlinked or
//! oversized one is ignored with one log line and the run tallies from
//! nothing; an edited profile drops only its own units; `--no-index`
//! neither reads nor writes; a goal already met makes no state call at
//! all; a second writer does not write.
//!
//! Each scenario runs `ensure` twice on one `ModelHost` (one machine, two
//! runs): the candidates are fresh, so `filter:idle-1d` keeps them all
//! (`not_idle`) and the index keeps their units between runs.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::host::{Dev, Host};
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::report::Report;
use diskgc_core::snapshot::RunIndex;
use diskgc_core::snapshot::format::{self, Snapshot};
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
"#;

/// `tmp-a` edited: another definition under the same name.
const PROFILES_EDITED: &str = r#"
- name: tmp-a
  find: {roots: [/tmp], paths: ["a*"], type: any}
  action: delete
  examples: {match: [/tmp/a0]}
- name: tmp-b
  find: {roots: [/tmp], paths: ["b*"], type: dir}
  action: delete
  examples: {match: [/tmp/b0]}
"#;

const STATE: &str = "/home/u/.local/state/diskgc";
/// `/tmp` is a tmpfs without a UUID: its key is its device number.
const SNAPSHOT: &str = "/home/u/.local/state/diskgc/index-dev-0-42";
const LOCK: &str = "/home/u/.local/state/diskgc/index-dev-0-42.lock";

fn library(profiles: &str) -> Library {
    let specs = parse_profiles(profiles).unwrap_or_else(|e| panic!("{e}"));
    Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"))
}

/// `/` (ext4, with a UUID; the state directory's), `/run` with the host
/// lock, a 1 GiB `/tmp` (tmpfs, dev 0:42, no UUID) with three fresh
/// candidates of `tmp-a` and two of `tmp-b`; `rest` appended.
fn yaml(rest: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", uuid: 4fca2e4a-9b1e-4989-a4c7-d05957e51910, size: 100G}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 1G, other_used: 500M, parent: 1}}
state_dir: {STATE}
tree:
  /usr/bin/tool: {{}}
  /run/diskgc/lock: {{uid: 0, gid: 0}}
  /home/u: {{.: {{mode: "0700"}}}}
  /tmp/a1: {{f: {{size: 10K}}}}
  /tmp/a2: {{f: {{size: 10K}}}}
  /tmp/a3: {{f: {{size: 10K}}}}
  /tmp/b1: {{f: {{size: 10K}}}}
  /tmp/b2: {{f: {{size: 10K}}}}
{rest}"#
    )
}

fn tier(library: &Library, select: &str, name: &str) -> Tier {
    let mut tier = Tier::select(select, library, None, 1000).unwrap_or_else(|e| panic!("{e}"));
    tier.name = name.to_owned();
    tier
}

fn request(tier: Tier, min_free: &str) -> Request {
    Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size(min_free).expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier,
    }
}

/// The two tiers `a` then `b`, both kept by the idle filter.
fn tiers(profiles: &str) -> Vec<Tier> {
    let library = library(profiles);
    vec![
        tier(&library, "profile:tmp-a && filter:idle-1d", "a"),
        tier(&library, "profile:tmp-b && filter:idle-1d", "b"),
    ]
}

/// One `ensure` of `tiers` on `host`, with `index`; the report and log.
fn run(host: &ModelHost, tiers: &[Tier], index: &mut RunIndex) -> (Report, Vec<String>) {
    let mut lines = Vec::new();
    let timeouts = vec![Duration::ZERO; tiers.len()];
    let report = executor::ensure_grouped_with_index(
        host,
        &request(tiers[0].clone(), "100%"),
        tiers,
        &timeouts,
        &[1000],
        index,
        &mut |l| lines.push(l.to_owned()),
    );
    eprintln!("log:\n  {}", lines.join("\n  "));
    (report, lines)
}

fn has(lines: &[String], needle: &str) -> bool {
    lines.iter().any(|l| l.contains(needle))
}

/// Port calls of `op` on the trace so far.
fn calls(host: &ModelHost, op: Op) -> usize {
    host.trace_count(op)
}

/// `getdents` of `/tmp` (a listing of the roots) on the trace so far.
fn tmp_listings(host: &ModelHost) -> usize {
    host.trace()
        .iter()
        .filter(|c| c.op == Op::ReadDir && c.path.as_deref() == Some(Path::new("/tmp")))
        .count()
}

/// A first run that writes the snapshot, then a minute.
fn first_run(host: &ModelHost, tiers: &[Tier]) -> Vec<String> {
    let (report, lines) = run(host, tiers, &mut RunIndex::snapshot());
    assert!(report.actions.is_empty(), "fresh candidates: {report:#?}");
    assert!(
        has(&lines, "index: no snapshot index-dev-0-42 yet"),
        "{lines:#?}"
    );
    assert!(
        has(&lines, "index: snapshot index-dev-0-42 written: 5 unit(s)"),
        "{lines:#?}"
    );
    host.sleep(Duration::from_secs(60));
    lines
}

/// The second run starts from nothing: the log says why, both roots are
/// listed again, and a valid snapshot is written over the bad one.
fn assert_ignored(host: &ModelHost, tiers: &[Tier], why: &str) {
    let (report, lines) = run(host, tiers, &mut RunIndex::snapshot());
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("index: snapshot index-dev-0-42 ignored: ") && l.contains(why)),
        "{why}: {lines:#?}"
    );
    assert!(has(&lines, "the synchronous tally starts from nothing"));
    assert!(
        has(&lines, "2 source(s) listed, 0 recent"),
        "tallied from nothing: {lines:#?}"
    );
    assert!(report.actions.is_empty());
    assert!(
        has(&lines, "index: snapshot index-dev-0-42 written: 5 unit(s)"),
        "{lines:#?}"
    );
}

#[test]
fn ensure_writes_the_snapshot_and_the_next_run_starts_from_it() {
    let host = host(&yaml(""));
    let tiers = tiers(PROFILES);
    first_run(&host, &tiers);
    let file = host.stat_path(Path::new(SNAPSHOT)).expect("written");
    assert_eq!(file.perm, Some(0o600));
    assert_eq!(file.uid, Some(1000));
    let state = host.stat_path(Path::new(STATE)).expect("made");
    assert_eq!(state.perm, Some(0o700));
    let listed_by_run_1 = tmp_listings(&host);
    let (report, lines) = run(&host, &tiers, &mut RunIndex::snapshot());
    assert!(report.actions.is_empty());
    assert!(
        has(
            &lines,
            "index: started from snapshot index-dev-0-42 written 60 s ago: 5 unit(s)"
        ),
        "{lines:#?}"
    );
    // Warm: the synchronous tally lists neither root again (each tier's
    // own discovery still does), and both queues come from the index.
    assert!(has(&lines, "0 source(s) listed, 2 recent"), "{lines:#?}");
    assert!(has(&lines, "a: 3 unit(s) from the index"), "{lines:#?}");
    assert!(has(&lines, "b: 2 unit(s) from the index"), "{lines:#?}");
    assert_eq!(listed_by_run_1, 4, "run 1: the tally's and each tier's own");
    assert_eq!(
        tmp_listings(&host) - listed_by_run_1,
        2,
        "run 2: each tier's own only"
    );
}

/// AC #2: every way a snapshot can be unusable, each ignored with its
/// line, the run tallying from nothing (and then replacing it).
#[test]
fn a_corrupt_mismatched_or_foreign_snapshot_is_ignored_and_tallied_from_nothing() {
    type Spoil = fn(&ModelHost, Vec<u8>);
    let cases: [(&str, Spoil, &str); 9] = [
        (
            "a flipped byte",
            |h, mut b| {
                let at = b.len() / 2;
                b[at] ^= 0x40;
                h.put_file(Path::new(SNAPSHOT), b, 1000, 0o600);
            },
            "checksum mismatch",
        ),
        (
            "truncated",
            |h, b| h.put_file(Path::new(SNAPSHOT), b[..b.len() - 9].to_vec(), 1000, 0o600),
            "checksum mismatch",
        ),
        (
            "another format version",
            |h, mut b| {
                b[8] = 99;
                h.put_file(Path::new(SNAPSHOT), b, 1000, 0o600);
            },
            "format version 99",
        ),
        (
            "another user's",
            |h, b| h.put_file(Path::new(SNAPSHOT), b, 2000, 0o600),
            "owned by uid 2000",
        ),
        (
            "readable by others",
            |h, b| h.put_file(Path::new(SNAPSHOT), b, 1000, 0o644),
            "mode 0644",
        ),
        (
            "a symlink",
            |h, b| {
                h.put_file(Path::new("/home/u/elsewhere"), b, 1000, 0o600);
                h.put_symlink(Path::new(SNAPSHOT), Path::new("/home/u/elsewhere"));
            },
            "a symlink, not a regular file",
        ),
        (
            "oversized",
            |h, b| {
                h.put_file(Path::new(SNAPSHOT), b, 1000, 0o600);
                h.set_size(
                    Path::new(SNAPSHOT),
                    diskgc_core::snapshot::MAX_SNAPSHOT_BYTES + 1,
                );
            },
            "more than the",
        ),
        (
            "another boot (a filesystem without a UUID)",
            |h, _| h.set_boot_id("00000000-0000-4000-8000-0000000000ff"),
            "written in another boot",
        ),
        (
            "another filesystem's",
            |h, b| {
                let mut s = format::decode(&b).expect("valid");
                s.header.key = diskgc_core::host::FsKey::dev(Dev { major: 0, minor: 7 });
                h.put_file(Path::new(SNAPSHOT), format::encode(&s), 1000, 0o600);
            },
            "written for filesystem dev-0-7",
        ),
    ];
    for (case, spoil, why) in cases {
        eprintln!("case: {case}");
        let host = host(&yaml(""));
        let tiers = tiers(PROFILES);
        first_run(&host, &tiers);
        spoil(
            &host,
            host.file_bytes(Path::new(SNAPSHOT)).expect("written"),
        );
        assert_ignored(&host, &tiers, why);
    }
}

/// TASK-31 review (b): a path the index would assert on (relative, not
/// canonical, outside its mount) in a snapshot with a valid checksum is
/// refused at load, never reaching the index or discovery.
#[test]
fn a_snapshot_naming_a_relative_or_outside_path_is_ignored_not_trusted() {
    type Bend = fn(&mut Snapshot);
    let cases: [(&str, Bend, &str); 4] = [
        (
            "relative",
            |s| s.singles[0].path = PathBuf::from("tmp/a1"),
            "is not absolute",
        ),
        (
            "dot-dot",
            |s| s.singles[0].path = PathBuf::from("/tmp/../etc"),
            "holds ParentDir",
        ),
        (
            "doubled slash",
            |s| s.singles[0].path = PathBuf::from("/tmp//a1"),
            "is not canonical",
        ),
        (
            "outside its mount",
            |s| s.singles[0].path = PathBuf::from("/etc/a1"),
            "not strictly below its mount point /tmp",
        ),
    ];
    for (case, bend, why) in cases {
        eprintln!("case: {case}");
        let host = host(&yaml(""));
        let tiers = tiers(PROFILES);
        first_run(&host, &tiers);
        let mut snapshot =
            format::decode(&host.file_bytes(Path::new(SNAPSHOT)).expect("written")).expect("valid");
        bend(&mut snapshot);
        host.put_file(Path::new(SNAPSHOT), format::encode(&snapshot), 1000, 0o600);
        assert_ignored(&host, &tiers, why);
    }
}

/// AC #3: `tmp-a` edited between runs drops `tmp-a`'s units and listing
/// time only; `tmp-b`'s stay warm.
#[test]
fn an_edited_profile_drops_only_its_own_units() {
    let host = host(&yaml(""));
    first_run(&host, &tiers(PROFILES));
    let (_, lines) = run(&host, &tiers(PROFILES_EDITED), &mut RunIndex::snapshot());
    assert!(
        has(
            &lines,
            "2 unit(s), 0 of them blocks; 1 finder(s) of edited profiles dropped with 3 unit(s)"
        ),
        "{lines:#?}"
    );
    assert!(has(&lines, "1 source(s) listed, 1 recent"), "{lines:#?}");
    assert!(has(&lines, "b: 2 unit(s) from the index"), "{lines:#?}");
}

/// AC #3: a run of other tiers (here only `b`) invalidates nothing: the
/// units of `a` stay in the snapshot it writes, for the runs that use them.
#[test]
fn a_run_of_other_tiers_keeps_the_units_it_does_not_use() {
    let host = host(&yaml(""));
    let both = tiers(PROFILES);
    first_run(&host, &both);
    let (_, lines) = run(&host, &both[1..], &mut RunIndex::snapshot());
    assert!(has(&lines, "started from snapshot"), "{lines:#?}");
    assert!(has(&lines, "written: 5 unit(s)"), "{lines:#?}");
    host.sleep(Duration::from_secs(60));
    let (_, lines) = run(&host, &both[..1], &mut RunIndex::snapshot());
    assert!(has(&lines, "0 source(s) listed, 1 recent"), "{lines:#?}");
    assert!(has(&lines, "a: 3 unit(s) from the index"), "{lines:#?}");
}

#[test]
fn no_index_neither_reads_nor_writes_the_snapshot() {
    let host = host(&yaml(""));
    let tiers = tiers(PROFILES);
    first_run(&host, &tiers);
    let before = host.file_bytes(Path::new(SNAPSHOT));
    let state_calls = |h: &ModelHost| {
        [
            Op::StateLock,
            Op::StateRead,
            Op::StateWrite,
            Op::FsUuid,
            Op::BootId,
        ]
        .map(|op| calls(h, op))
    };
    let counted = state_calls(&host);
    let (_, lines) = run(&host, &tiers, &mut RunIndex::no_index());
    assert_eq!(state_calls(&host), counted, "no state call");
    assert!(has(&lines, "--no-index"), "{lines:#?}");
    assert!(has(&lines, "2 source(s) listed, 0 recent"), "{lines:#?}");
    assert_eq!(host.file_bytes(Path::new(SNAPSHOT)), before);
}

/// docs/architecture.md, Snapshot: an `ensure` that finds the space
/// already there stays an instant no-op: no tally, no state call, no
/// state directory or lock file made.
#[test]
fn an_ensure_whose_goal_is_met_makes_no_state_call() {
    let host = host(&yaml(""));
    let tiers = tiers(PROFILES);
    let mut lines = Vec::new();
    let report = executor::ensure_grouped_with_index(
        &host,
        &request(tiers[0].clone(), "1M"),
        &tiers,
        &[Duration::ZERO, Duration::ZERO],
        &[1000],
        &mut RunIndex::snapshot(),
        &mut |l| lines.push(l.to_owned()),
    );
    assert_eq!(report.exit_code(), 0, "{report:#?}");
    for op in [
        Op::StateLock,
        Op::StateRead,
        Op::StateWrite,
        Op::FsUuid,
        Op::BootId,
    ] {
        assert_eq!(calls(&host, op), 0, "{op}");
    }
    assert_eq!(tmp_listings(&host), 0, "no tally");
    assert!(
        host.stat_path(Path::new(STATE)).is_none(),
        "no state directory"
    );
    assert!(lines.iter().all(|l| !l.contains("index")), "{lines:#?}");
}

/// docs/architecture.md, Single writer: while another process holds the
/// single-writer lock (a `watch`), `ensure` reads the snapshot but skips
/// its write with one line; the file is unchanged.
#[test]
fn a_second_writer_does_not_write() {
    let host = host(&yaml(&format!(
        "  {LOCK}: {{mode: \"0600\"}}\nprocesses:\n  - {{pid: 7, exe: /usr/bin/tool, cwd: /}}\n\
         locks:\n  - {{path: {LOCK}, holder: 7, mode: exclusive}}\n"
    )));
    let (report, lines) = run(&host, &tiers(PROFILES), &mut RunIndex::snapshot());
    assert!(report.actions.is_empty());
    assert!(
        has(
            &lines,
            "index: snapshot index-dev-0-42 not written: index-dev-0-42.lock is held by another \
             writer"
        ),
        "{lines:#?}"
    );
    assert_eq!(calls(&host, Op::StateWrite), 0);
    assert!(host.stat_path(Path::new(SNAPSHOT)).is_none());
}

/// docs/architecture.md, Snapshot: Never at the cost of the goal: the
/// state directory on the filesystem being reclaimed, below `--min-free`,
/// the write is skipped with one line.
#[test]
fn the_write_never_takes_the_reclaimed_filesystem_below_min_free() {
    let host = host(&yaml("").replace(
        &format!("state_dir: {STATE}"),
        "state_dir: /tmp/.state/diskgc",
    ));
    let (_, lines) = run(&host, &tiers(PROFILES), &mut RunIndex::snapshot());
    assert!(
        has(&lines, "the state directory is on the reclaimed filesystem"),
        "{lines:#?}"
    );
    assert_eq!(calls(&host, Op::StateWrite), 0);
    assert!(
        host.stat_path(Path::new("/tmp/.state/diskgc/index-dev-0-42"))
            .is_none()
    );
}

/// The port's shape (docs/architecture.md, The Host port): the model
/// fails a write made without holding the key's lock exclusively.
#[test]
#[should_panic(expected = "without holding index-dev-0-42.lock exclusively")]
fn a_state_write_without_the_exclusive_lock_fails_the_test() {
    use diskgc_core::host::{FsKey, LockMode, TryLock};
    let host = host(&yaml(""));
    let key = FsKey::dev(Dev {
        major: 0,
        minor: 42,
    });
    let lock = host.state_lock(&key).expect("lock file");
    let TryLock::Acquired(shared) = host.flock_try(lock.fd, LockMode::Shared).expect("flock")
    else {
        panic!("busy")
    };
    let _ = host.state_write(&shared, &lock.dir, &key, b"x", &|| false);
}

/// TASK-30 review (Codex 6, mped): the write's allocation, not its encoded
/// length, is guarded, and it comes before the run's final sample. With the
/// state directory on the reclaimed filesystem (made by an earlier run) and
/// the walk ending 1 KiB above `--min-free`, a snapshot of a few hundred
/// bytes would still take a 4 KiB block: it is not written, and the run's
/// exit 0 means the space is really there.
#[test]
fn a_snapshot_with_1_kib_of_headroom_is_not_written_and_exit_0_holds() {
    let yaml = yaml("  /tmp/.state/diskgc: {.: {mode: \"0700\"}}\n")
        .replace(
            &format!("state_dir: {STATE}"),
            "state_dir: /tmp/.state/diskgc",
        )
        .replace(
            "  /tmp/a2: {f: {size: 10K}}\n  /tmp/a3: {f: {size: 10K}}\n",
            "",
        )
        .replace(
            "  /tmp/b1: {f: {size: 10K}}\n  /tmp/b2: {f: {size: 10K}}\n",
            "",
        )
        .replace("/tmp/a1: {f: {size: 10K}}", "/tmp/a1: {f: {size: 4K}}");
    let host = host(&yaml);
    let before = host.space_at(Path::new("/tmp")).avail_bytes;
    // /tmp/a1 frees one block for its directory and one for its file.
    let min = before + 8192 - 1024;
    assert_eq!(min % 1024, 0);
    let library = library(PROFILES);
    let tier = tier(&library, "profile:tmp-a", "a");
    let mut lines = Vec::new();
    let report = executor::ensure_with_index(
        &host,
        &Request {
            min_free: parse_size(&format!("{}K", min / 1024)).expect("a size"),
            ..request(tier, "1K")
        },
        &[1000],
        &mut RunIndex::snapshot(),
        &mut |l| lines.push(l.to_owned()),
    );
    eprintln!("log:\n  {}", lines.join("\n  "));
    let after = host.space_at(Path::new("/tmp")).avail_bytes;
    assert_eq!(report.exit_code(), 0, "{report:#?}");
    assert!(after >= min, "exit 0 with {after} available, below {min}");
    assert_eq!(report.avail_after_bytes, Some(after));
    assert!(
        has(&lines, "the state directory is on the reclaimed filesystem"),
        "{lines:#?}"
    );
    assert_eq!(calls(&host, Op::StateWrite), 0);
}

/// TASK-30 review: a SIGTERM that arrives while the snapshot is written is
/// the run's status (exit 143), since the write is part of the run.
#[test]
fn a_signal_during_the_snapshot_write_is_the_runs_exit_code() {
    let host = host(&yaml(
        "events:\n  - {at: {op: state_write, when: after}, signal: term}\n",
    ));
    let (report, lines) = run(&host, &tiers(PROFILES), &mut RunIndex::snapshot());
    assert!(host.unfired_events().is_empty());
    assert!(has(&lines, "written: 5 unit(s)"), "{lines:#?}");
    assert_eq!(report.exit_code(), 143, "{report:#?}");
}

/// A state directory diskgc may not use (others may write to it) is said
/// once, and nothing is read or written there.
#[test]
fn an_unusable_state_directory_is_one_line_and_no_write() {
    let host = host(&yaml(&format!("  {STATE}: {{.: {{mode: \"0777\"}}}}\n")));
    let (_, lines) = run(&host, &tiers(PROFILES), &mut RunIndex::snapshot());
    let index_lines: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("index: ") && !l.starts_with("index: synchronous"))
        .collect();
    assert_eq!(index_lines.len(), 1, "{lines:#?}");
    assert!(
        index_lines[0].contains("writable by no one else")
            && index_lines[0].contains("nothing is written"),
        "{lines:#?}"
    );
    assert_eq!(calls(&host, Op::StateWrite), 0);
}
