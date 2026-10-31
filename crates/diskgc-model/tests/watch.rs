//! `watch` on ModelHost (TASK-32; docs/reference.md, `watch`;
//! docs/testing.md, watch scenarios): the start sequence, the virtual-clock
//! loop of samples, cycles and snapshot ticks, the back-off rules, and
//! `--tally-only`, with the background tally inline
//! ([`diskgc_core::watch::run_inline`]: the model has no threads; the
//! thread split is diskgc-linux's `tests/tally_thread.rs`).
//!
//! Available space is scripted by `other_used` events on `/tmp`; every
//! scenario ends with a SIGTERM event and checks the deletion oracle on
//! the trace (`unlinkat`), the host lock (`inherit_lock` follows every
//! exclusive host-lock `flock` the reclaim takes) and the JSON lines.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::invocation::{Catalog, ConfigFile, Platform, Selection};
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::report::Signal;
use diskgc_core::units::parse_size;
use diskgc_core::watch::{self, Ended, Mode};
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;
use serde_json::Value;

const PROFILES: &str = r#"
- name: tmp-a
  find: {roots: [/tmp], paths: ["a*"], type: dir}
  action: delete
  examples: {match: [/tmp/a0]}
"#;

const SELECT: &str = "profile:tmp-a && filter:idle-1h";
const OLD: &str = "2026-10-01T00:00:00Z";
const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
const SNAPSHOT: &str = "/home/u/.local/state/diskgc/index-dev-0-42";
const LOCK: &str = "/home/u/.local/state/diskgc/index-dev-0-42.lock";

fn library() -> Library {
    let specs = parse_profiles(PROFILES).unwrap_or_else(|e| panic!("{e}"));
    Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"))
}

/// `/` (ext4), `/run` with the host lock, a 10 GiB `/tmp` (tmpfs, dev
/// 0:42) with `other_used` used outside the model, the state directory
/// under `/home/u`; process 7 a readable bystander. `tree` and `rest`
/// appended.
fn yaml(other_used: &str, tree: &str, rest: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
call_cost: 1ms
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 10G, other_used: {other_used}, parent: 1}}
state_dir: /home/u/.local/state/diskgc
tree:
  /usr/bin/tool: {{}}
  /run/diskgc/lock: {{uid: 0, gid: 0}}
  /home/u: {{.: {{mode: "0700"}}}}
{tree}processes:
  - {{pid: 7, exe: /usr/bin/tool, cwd: /, maps: [/usr/bin/tool], fds: [{{path: /usr/bin/tool}}], environ: [PATH=/usr/bin]}}
{rest}"#
    )
}

/// `n` old candidates `/tmp/aNN`, each holding one file of `size`.
fn old_candidates(n: usize, size: &str) -> String {
    (0..n)
        .map(|i| {
            format!("  /tmp/a{i:02}: {{.: {{mtime: {OLD}}}, f: {{size: {size}, mtime: {OLD}}}}}\n")
        })
        .collect()
}

fn reclaim(min: &str, target: &str, interval: &str) -> Mode {
    Mode::Reclaim {
        min_free: parse_size(min).unwrap(),
        target_free: parse_size(target).unwrap(),
        interval: diskgc_core::units::parse_duration(interval).unwrap(),
        timeout: Duration::from_secs(60),
    }
}

fn selection() -> Selection {
    Selection {
        path: PathBuf::from("/tmp"),
        select: Some(SELECT.to_owned()),
        roots: Vec::new(),
        allow_root: false,
        test_entry_budget: None,
    }
}

/// Seeds 1, 2, 3, ... for `ranking: random` (the first is the start's).
fn counting_seeds() -> impl Fn() -> Result<u64, String> {
    let next = std::cell::Cell::new(0);
    move || {
        next.set(next.get() + 1);
        Ok(next.get())
    }
}

/// What a `watch` did.
struct Watched {
    lines: Vec<String>,
    /// Each record with the virtual time it was written at.
    records: Vec<(Duration, Value)>,
    ended: Result<Ended, u8>,
}

impl Watched {
    fn has(&self, needle: &str) -> bool {
        self.lines.iter().any(|l| l.contains(needle))
    }

    fn count(&self, needle: &str) -> usize {
        self.lines.iter().filter(|l| l.contains(needle)).count()
    }
}

/// Starts and runs `watch` in `mode` on `host` until it ends (a SIGTERM
/// event of the model).
fn watch(host: &ModelHost, mode: &Mode) -> Watched {
    watch_with(
        host,
        mode,
        &selection(),
        &ConfigFile::NotFound { tried: Vec::new() },
    )
}

/// [`watch`] with its own selection and configuration file.
fn watch_with(
    host: &ModelHost,
    mode: &Mode,
    selection: &Selection,
    config: &ConfigFile,
) -> Watched {
    let library = library();
    let builtins = || Ok(library.clone());
    let seeds = counting_seeds();
    let platform = Platform {
        catalog: Catalog::Development,
        draw_seed: &seeds,
        builtins: &builtins,
    };
    let mut lines: Vec<String> = Vec::new();
    let mut records: Vec<(Duration, Value)> = Vec::new();
    let started = watch::start(
        host,
        mode,
        selection,
        config,
        &platform,
        Some(Path::new("/home/u/.local/state/diskgc")),
        &mut |l| lines.push(l.to_owned()),
    );
    let ended = match started {
        Err(refused) => {
            if let Some(report) = refused.report {
                lines.push(report.to_json());
            }
            Err(refused.exit_code)
        }
        Ok(started) => {
            let mut record = |line: &str| {
                assert!(!line.contains('\n'), "one line per record");
                let value: Value = serde_json::from_str(line).expect("a JSON object");
                records.push((host.elapsed(), value));
                Ok(())
            };
            let (ended, _) =
                watch::run_inline(host, started, &seeds, Some(&mut record), &mut |l| {
                    lines.push(l.to_owned())
                });
            Ok(ended)
        }
    };
    eprintln!("log:\n  {}", lines.join("\n  "));
    Watched {
        lines,
        records,
        ended,
    }
}

fn deletions(host: &ModelHost) -> usize {
    host.trace_count(Op::Unlinkat)
}

/// Exclusive host-lock acquisitions: `hostlock::acquire` makes every one
/// inheritable right after the `flock`.
fn host_locks(host: &ModelHost) -> usize {
    host.trace_count(Op::InheritLock)
}

fn avail(host: &ModelHost) -> u64 {
    host.space_at(Path::new("/tmp")).avail_bytes
}

// ---- AC #1: between the thresholds, nothing ----

/// The PRD's acceptance test, over 24 virtual hours: available space held
/// between `--min-free` and `--target-free` while candidates are created
/// and age into eligibility. No deletion, no host lock, no record; one
/// sample per interval; the tally keeps listing; a snapshot every 5
/// minutes.
#[test]
fn watch_steady_between_thresholds_deletes_nothing_and_keeps_its_duty_cycle() {
    // 10 GiB, 5 GiB used elsewhere, old candidates of 100 MiB: about
    // 4.5 GiB available, between 2 GiB and 8 GiB.
    let mut events = String::from("events:\n");
    for hour in 0..24 {
        events += &format!("  - {{at: {{elapsed: {hour}h}}, mkdir: /tmp/a-new-{hour:02}}}\n");
    }
    events += "  - {at: {elapsed: 24h}, signal: term}\n";
    let host = host(&yaml("5G", &old_candidates(5, "100M"), &events));
    let before = avail(&host);
    assert!(before > 2 * GIB && before < 8 * GIB, "{before}");
    let run = watch(&host, &reclaim("2G", "8G", "1m"));
    assert_eq!(run.ended, Ok(Ended::Stopped(Signal::Term)));
    assert_eq!(
        deletions(&host),
        0,
        "nothing deleted between the thresholds"
    );
    assert_eq!(host_locks(&host), 0, "the host lock never taken");
    assert_eq!(host.trace_count(Op::MayDelete), 1, "the start's gate only");
    assert!(run.records.is_empty(), "no record: {:?}", run.records);
    assert!(!run.has("cycle 1"), "no cycle line");
    // One sample a minute (the snapshot guard's are on the state
    // directory, not /tmp: the state directory is on `/`).
    let samples = host
        .trace()
        .iter()
        .filter(|c| c.op == Op::Statvfs && c.path.as_deref() == Some(Path::new("/tmp")))
        .count();
    assert!(
        (24 * 60..=24 * 60 + 2).contains(&samples),
        "{samples} samples"
    );
    // A snapshot every 5 minutes, and one at the stop.
    let writes = host.trace_count(Op::StateWrite);
    assert!((24 * 12..=24 * 12 + 2).contains(&writes), "{writes} writes");
    assert!(host.stat_path(Path::new(SNAPSHOT)).is_some());
    // The tally kept listing /tmp: about once per 5 minutes.
    let listings = host
        .trace()
        .iter()
        .filter(|c| c.op == Op::ReadDir && c.path.as_deref() == Some(Path::new("/tmp")))
        .count();
    assert!(listings >= 24 * 12 - 2, "{listings} listings of /tmp");
    // Inline (the model's one thread) never lowers the priority: the
    // reclaim shares that thread. The tally's own thread does (diskgc-linux
    // tests/tally_thread.rs).
    assert_eq!(host.priority_lowered(), 0);
    // Every port call but the clock's costs 1 ms: all of them together,
    // the tally's and the watcher's, stay far inside the duty cycle's 5 %.
    let busy_ms = host
        .trace()
        .iter()
        .filter(|c| !matches!(c.op, Op::Now | Op::WallClock | Op::Sleep))
        .count() as u64;
    assert!(
        busy_ms * 20 <= 24 * 3600 * 1000,
        "{busy_ms} ms of calls in 24 h"
    );
    assert!(run.has("stopping on SIGTERM"));
}

// ---- One drop: one cycle, to the goal ----

/// One drop below `--min-free` gives exactly one cycle and one record
/// (`met`, `target_met`, `exit_code: null`, `watch.cycle: 1`, no
/// back-off), and deletion stops at the goal.
#[test]
fn watch_reclaims_below_min_free_to_target_free() {
    // 10 GiB, 4 GiB used elsewhere, 10 candidates of 300 MiB: about
    // 3 GiB available. Then 6.5 GiB used elsewhere: about 0.6 GiB, below
    // 1 GiB; the goal 2 GiB needs 5 of the 10 candidates.
    let events = "events:\n  - {at: {elapsed: 90s}, other_used: {mount: /tmp, bytes: 6656M}}\n  \
                  - {at: {elapsed: 10m}, signal: term}\n";
    let host = host(&yaml("4G", &old_candidates(10, "300M"), events));
    let run = watch(&host, &reclaim("1G", "2G", "10s"));
    assert_eq!(run.ended, Ok(Ended::Stopped(Signal::Term)));
    assert_eq!(run.records.len(), 1, "{:?}", run.records);
    let (at, record) = &run.records[0];
    assert!(*at >= Duration::from_secs(90) && *at < Duration::from_secs(120));
    assert_eq!(record["status"], "met");
    assert_eq!(record["target_met"], true);
    assert_eq!(record["exit_code"], Value::Null);
    assert_eq!(record["dry_run"], false);
    assert_eq!(record["plan"], serde_json::json!([]));
    assert_eq!(record["watch"]["cycle"], 1);
    assert_eq!(record["watch"]["backoff"], Value::Null);
    let actions = record["actions"].as_array().unwrap().len();
    assert!((5..10).contains(&actions), "stops at the goal: {actions}");
    assert!(avail(&host) >= 2 * GIB);
    assert_eq!(host_locks(&host), 1);
    assert_eq!(run.count("cycle 1 ended met"), 1, "{:?}", run.lines);
}

// ---- TASK-277: a user's watch without the host lock ----

/// A non-root watch on a host without `/run/diskgc/lock` starts, says once
/// that its reclaims take the per-user lock (in the state directory here:
/// no `$XDG_RUNTIME_DIR`) and do not serialise with other users', and its
/// cycle reclaims under that lock, made inheritable as the host lock is.
#[test]
fn a_user_watch_without_the_host_lock_reclaims_under_its_per_user_lock() {
    const USER_LOCK: &str = "/home/u/.local/state/diskgc/lock";
    let events = "events:\n  - {at: {elapsed: 90s}, other_used: {mount: /tmp, bytes: 6656M}}\n  \
                  - {at: {elapsed: 10m}, signal: term}\n";
    let model = yaml("4G", &old_candidates(10, "300M"), events)
        .replace("  /run/diskgc/lock: {uid: 0, gid: 0}\n", "");
    let host = host(&model);
    let run = watch(&host, &reclaim("1G", "2G", "10s"));
    assert_eq!(
        run.ended,
        Ok(Ended::Stopped(Signal::Term)),
        "{:?}",
        run.lines
    );
    assert_eq!(
        run.count(&format!(
            "reclaims take the per-user lock {USER_LOCK} instead"
        )),
        1,
        "{:?}",
        run.lines
    );
    assert_eq!(run.records.len(), 1, "{:?}", run.records);
    assert_eq!(run.records[0].1["status"], "met");
    assert!(deletions(&host) > 0);
    assert_eq!(host_locks(&host), 1, "taken once and made inheritable");
    let taken = host
        .trace()
        .into_iter()
        .filter(|c| c.op == Op::Flock && c.result == "acquired")
        .filter_map(|c| c.path)
        .filter(|p| p.ends_with("lock") && !p.ends_with("index-dev-0-42.lock"))
        .collect::<Vec<_>>();
    assert!(
        taken.iter().all(|p| p == Path::new(USER_LOCK)) && !taken.is_empty(),
        "{taken:?}"
    );
    assert!(
        run.has(&format!("per-user lock {USER_LOCK} taken after")),
        "{:?}",
        run.lines
    );
    let stat = host.stat_path(Path::new(USER_LOCK)).expect("made");
    assert_eq!((stat.uid, stat.perm), (Some(1000), Some(0o600)));
    // --tally-only: no reclaims of its own; it says what it pauses for.
    let tally_only = self::host(&model);
    let run = watch(&tally_only, &Mode::TallyOnly { min_free: None });
    assert_eq!(
        run.ended,
        Ok(Ended::Stopped(Signal::Term)),
        "{:?}",
        run.lines
    );
    assert_eq!(
        run.count(&format!(
            "the tally pauses only while the per-user lock {USER_LOCK} is held"
        )),
        1,
        "{:?}",
        run.lines
    );
    assert_eq!(deletions(&tally_only), 0);
}

/// Root (`--allow-root`) never falls back: a watch without the host lock
/// exits 2 (`provisioning`) at start in reclaim mode, makes no per-user
/// lock, and `--tally-only` starts with a warning.
#[test]
fn watch_as_root_without_the_host_lock_exits_2_at_start() {
    let config = "version: 1\nowners: [tester]\nprofiles:\n  - {name: tmp-a, find: {roots: [/tmp], paths: [\"a*\"], type: dir}, action: delete, examples: {match: [/tmp/a0]}}\ngroups:\n  - name: g\n    tiers:\n      - {name: a, select: \"profile:tmp-a && filter:idle-1h\"}\n";
    let file = ConfigFile::Read {
        path: PathBuf::from("/etc/diskgc/config.yaml"),
        bytes: config.as_bytes().to_vec(),
    };
    let selection = Selection {
        select: None,
        allow_root: true,
        ..selection()
    };
    let model = yaml(
        "5G",
        "  /run/user/0: {.: {uid: 0, mode: \"0700\"}}\n",
        "own_uid: 0\nusers: {tester: 2000}\nruntime_dir: /run/user/0\nevents:\n  - {at: {elapsed: 1m}, signal: term}\n",
    )
    .replace("  /run/diskgc/lock: {uid: 0, gid: 0}\n", "");
    let root = host(&model);
    let run = watch_with(&root, &reclaim("2G", "3G", "10s"), &selection, &file);
    assert_eq!(run.ended, Err(2), "{:?}", run.lines);
    assert!(run.has("\"cause\":\"provisioning\""), "{:?}", run.lines);
    assert!(
        run.has("host lock /run/diskgc/lock is missing"),
        "{:?}",
        run.lines
    );
    assert_eq!(root.trace_count(Op::UserLock), 0);
    assert!(root.stat_path(Path::new("/run/user/0/diskgc")).is_none());
    let root = host(&model);
    let run = watch_with(
        &root,
        &Mode::TallyOnly { min_free: None },
        &selection,
        &file,
    );
    assert_eq!(
        run.ended,
        Ok(Ended::Stopped(Signal::Term)),
        "{:?}",
        run.lines
    );
    assert!(
        run.has("warning: the host lock /run/diskgc/lock is missing"),
        "{:?}",
        run.lines
    );
    assert_eq!(root.trace_count(Op::UserLock), 0);
}

// ---- AC #2: back-off ----

/// An `exhausted` cycle (every candidate fresh): sampling continues, but
/// no cycle before 15 minutes, or a drop by the step (1 GiB here) when
/// that comes first. One back-off line per back-off; the record's
/// `resume_below_bytes` and `wait_seconds` say when.
#[test]
fn watch_hold_back_off_waits_for_15_minutes() {
    let fresh: String = (0..3)
        .map(|i| format!("  /tmp/a{i:02}: {{f: {{size: 10M}}}}\n"))
        .collect();
    let events = "events:\n  - {at: {elapsed: 40m}, signal: term}\n";
    let host = host(&yaml("7G", &fresh, events));
    let run = watch(&host, &reclaim("4G", "5G", "10s"));
    let starts: Vec<u64> = run.records.iter().map(|(t, _)| t.as_secs()).collect();
    // Cycles 15 minutes apart: the first sample after the wait, which
    // runs from the cycle's end.
    assert_eq!(starts.len(), 3, "{starts:?}");
    for gap in starts.windows(2).map(|w| w[1] - w[0]) {
        assert!((900..=915).contains(&gap), "{starts:?}");
    }
    for (_, r) in &run.records {
        assert_eq!(r["status"], "exhausted");
        assert_eq!(r["watch"]["backoff"]["class"], "hold");
        assert_eq!(r["watch"]["backoff"]["attempt"], 0);
        assert_eq!(r["watch"]["backoff"]["wait_seconds"], 900);
        let avail = r["avail_after_bytes"].as_u64().unwrap();
        assert_eq!(
            r["watch"]["backoff"]["resume_below_bytes"].as_u64(),
            Some(avail - GIB)
        );
    }
    assert_eq!(run.count("back-off hold"), 3);
    assert_eq!(deletions(&host), 0);
    // Sampling went on through the back-off: about one per 10 s.
    let samples = host
        .trace()
        .iter()
        .filter(|c| c.op == Op::Statvfs && c.path.as_deref() == Some(Path::new("/tmp")))
        .count();
    assert!(samples >= 40 * 6 - 3, "{samples} samples");
}

/// The same back-off, ended by a drop of the step before the timer: the
/// cycle starts at that sample.
#[test]
fn watch_hold_back_off_ends_on_a_drop_by_the_step() {
    let fresh: String = (0..3)
        .map(|i| format!("  /tmp/a{i:02}: {{f: {{size: 10M}}}}\n"))
        .collect();
    // 7 GiB used: ~3 GiB available. At 3 min, 1 GiB less (a drop of
    // exactly the step).
    let events = "events:\n  - {at: {elapsed: 3m}, other_used: {mount: /tmp, bytes: 8G}}\n  \
                  - {at: {elapsed: 5m}, signal: term}\n";
    let host = host(&yaml("7G", &fresh, events));
    let run = watch(&host, &reclaim("4G", "5G", "10s"));
    let starts: Vec<u64> = run.records.iter().map(|(t, _)| t.as_secs()).collect();
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert!(starts[1] >= 180 && starts[1] <= 192, "{starts:?}");
}

/// A back-off ended by space someone else freed: no cycle, one line; a
/// later drop below `--min-free` starts a cycle at that sample.
#[test]
fn watch_recovery_ends_back_off() {
    let fresh: String = (0..3)
        .map(|i| format!("  /tmp/a{i:02}: {{f: {{size: 10M}}}}\n"))
        .collect();
    let events = "events:\n  - {at: {elapsed: 2m}, other_used: {mount: /tmp, bytes: 5G}}\n  \
                  - {at: {elapsed: 4m}, other_used: {mount: /tmp, bytes: 9G}}\n  \
                  - {at: {elapsed: 6m}, signal: term}\n";
    let host = host(&yaml("9G", &fresh, events));
    let run = watch(&host, &reclaim("2G", "3G", "10s"));
    let starts: Vec<u64> = run.records.iter().map(|(t, _)| t.as_secs()).collect();
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert!(starts[1] >= 240 && starts[1] <= 252, "{starts:?}");
    assert_eq!(run.count("space recovered"), 1, "{:?}", run.lines);
}

/// `retry` doubles and caps: a mount stacked on `PATH` after start makes
/// every cycle a `host` error before anything is deleted (the samples
/// still read the watched filesystem, below `--min-free`). Cycles 1, 2,
/// 4, 8, 15, 15 minutes apart.
#[test]
fn watch_path_on_another_device_backs_off_retry_doubling_and_deletes_nothing() {
    let events = "events:\n  \
                  - {at: {elapsed: 10s}, mount: {id: 99, at: /tmp/w, fs: tmpfs, dev: \"0:99\", size: 1G}}\n  \
                  - {at: {elapsed: 30s}, other_used: {mount: /tmp, bytes: 9G}}\n  \
                  - {at: {elapsed: 62m}, signal: term}\n";
    let tree = format!("{}  /tmp/w: {{.: {{}}}}\n", old_candidates(3, "100M"));
    let host = host(&yaml("5G", &tree, events));
    let run = watch_at(&host, &reclaim("2G", "3G", "1s"), "/tmp/w");
    let starts: Vec<u64> = run.records.iter().map(|(t, _)| t.as_secs()).collect();
    let gaps: Vec<u64> = starts.windows(2).map(|w| (w[1] - w[0] + 30) / 60).collect();
    assert_eq!(gaps, [1, 2, 4, 8, 15, 15, 15], "starts {starts:?}");
    for (k, (_, r)) in run.records.iter().enumerate() {
        assert_eq!(r["status"], "error");
        assert_eq!(r["errors"][0]["cause"], "host");
        assert!(
            r["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains("not on the filesystem watched since start")
        );
        assert_eq!(r["watch"]["backoff"]["class"], "retry");
        assert_eq!(r["watch"]["backoff"]["attempt"], k as u64 + 1);
    }
    assert_eq!(deletions(&host), 0);
    assert_eq!(host_locks(&host), 0, "refused before the host lock");
}

/// [`watch`] with another `PATH`.
fn watch_at(host: &ModelHost, mode: &Mode, path: &str) -> Watched {
    let selection = Selection {
        path: PathBuf::from(path),
        ..selection()
    };
    watch_with(
        host,
        mode,
        &selection,
        &ConfigFile::NotFound { tried: Vec::new() },
    )
}

/// Another process holds the host lock past `--timeout`: a `lock_timeout`
/// record, nothing deleted, class `retry`.
#[test]
fn watch_cycle_beside_another_reclaim_times_out_and_retries() {
    let events = "events:\n  - {at: {elapsed: 30s}, other_used: {mount: /tmp, bytes: 9G}}\n  \
                  - {at: {elapsed: 4m}, signal: term}\n";
    let rest =
        format!("{events}locks:\n  - {{path: /run/diskgc/lock, holder: 4242, mode: exclusive}}\n");
    let tree = format!("{}  /usr/bin/diskgc: {{}}\n", old_candidates(3, "100M"));
    let yaml = yaml("5G", &tree, &rest).replace(
        "processes:\n",
        "processes:\n  - {pid: 4242, exe: /usr/bin/diskgc, cwd: /}\n",
    );
    let host = host(&yaml);
    let run = watch(&host, &reclaim("2G", "3G", "10s"));
    assert!(!run.records.is_empty());
    let (_, first) = &run.records[0];
    assert_eq!(first["status"], "lock_timeout");
    assert_eq!(first["watch"]["backoff"]["class"], "retry");
    assert_eq!(first["watch"]["backoff"]["wait_seconds"], 60);
    assert_eq!(deletions(&host), 0);
}

// ---- AC #3: --tally-only ----

/// Available space far below any threshold for an hour: no deletion, no
/// host lock, no gate, no record; a snapshot every 5 minutes and at stop;
/// and while another process holds the host lock, no tally call below
/// `/tmp` until it is released.
#[test]
fn watch_tally_only_never_reclaims_and_keeps_the_snapshot_fresh() {
    let events = "events:\n  - {at: {elapsed: 20m}, exit: 4242}\n  \
                  - {at: {elapsed: 60m}, signal: term}\n";
    let rest =
        format!("{events}locks:\n  - {{path: /run/diskgc/lock, holder: 4242, mode: exclusive}}\n");
    let tree = format!("{}  /usr/bin/diskgc: {{}}\n", old_candidates(5, "100M"));
    let yaml = yaml("10G", &tree, &rest).replace(
        "processes:\n",
        "processes:\n  - {pid: 4242, exe: /usr/bin/diskgc, cwd: /}\n",
    );
    let host = host(&yaml);
    assert!(avail(&host) < 100 * MIB);
    let run = watch(&host, &Mode::TallyOnly { min_free: None });
    assert_eq!(run.ended, Ok(Ended::Stopped(Signal::Term)));
    assert_eq!(deletions(&host), 0, "never deletes");
    assert_eq!(host_locks(&host), 0, "never takes the host lock");
    assert_eq!(host.trace_count(Op::MayDelete), 0, "never asks the gate");
    assert!(run.records.is_empty());
    let writes = host.trace_count(Op::StateWrite);
    assert!((12..=14).contains(&writes), "{writes} writes");
    // The pause: until a probe found the host lock free (the holder exited
    // at 20 minutes), the tally listed nothing and touched no candidate.
    let trace = host.trace();
    let probe = |c: &&diskgc_model::host::Call| {
        c.op == Op::Flock && c.path.as_deref() == Some(Path::new("/run/diskgc/lock"))
    };
    let free = trace
        .iter()
        .position(|c| probe(&c) && c.result == "acquired")
        .expect("a probe found the lock free");
    let busy = trace[..free].iter().filter(|c| probe(c)).count();
    // One a second, plus the probe's own calls.
    assert!(busy >= 20 * 55, "{busy} busy probes");
    assert!(
        trace[..free]
            .iter()
            .all(|c| c.op != Op::ReadDir
                && !c.path.as_ref().is_some_and(|p| p.starts_with("/tmp/a"))),
        "a tally call below /tmp while the host lock was held"
    );
    assert!(run.has("background tally paused"), "{:?}", run.lines);
    assert!(run.has("background tally resumed"), "{:?}", run.lines);
    // The snapshot holds the candidates the tally found.
    assert!(run.has("index: snapshot index-dev-0-42 written: 5 unit(s)"));
}

/// `--min-free` given under `--tally-only`, the state directory on the
/// watched filesystem: a write that would take it below is skipped.
#[test]
fn watch_tally_only_min_free_guards_the_write_on_the_watched_filesystem() {
    let events = "events:\n  - {at: {elapsed: 11m}, signal: term}\n";
    let host = host(
        &yaml("10G", &old_candidates(2, "1M"), events)
            .replace(
                "state_dir: /home/u/.local/state/diskgc",
                "state_dir: /tmp/u/state",
            )
            .replace(
                "  /home/u: {.: {mode: \"0700\"}}\n",
                "  /tmp/u: {.: {mode: \"0700\"}}\n",
            ),
    );
    let library = library();
    let builtins = || Ok(library.clone());
    let platform = Platform {
        catalog: Catalog::Development,
        draw_seed: &|| Ok(0),
        builtins: &builtins,
    };
    let mut lines = Vec::new();
    let started = watch::start(
        &host,
        &Mode::TallyOnly {
            min_free: Some(parse_size("1G").unwrap()),
        },
        &selection(),
        &ConfigFile::NotFound { tried: Vec::new() },
        &platform,
        Some(Path::new("/tmp/u/state")),
        &mut |l: &str| lines.push(l.to_owned()),
    )
    .expect("starts");
    watch::run_inline(&host, started, &|| Ok(0), None, &mut |l| {
        lines.push(l.to_owned())
    });
    assert_eq!(host.trace_count(Op::StateWrite), 0, "{lines:#?}");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("not written: the state directory is on the reclaimed")),
        "{lines:#?}"
    );
}

// ---- Start ----

/// TASK-30 AC #1, watch's half: another `watch` holds the filesystem's
/// single-writer lock: after its 10 s wait, exit 2 naming it, nothing
/// deleted.
#[test]
fn second_watch_exits_2() {
    let rest = format!(
        "events:\n  - {{at: {{elapsed: 1h}}, signal: term}}\nlocks:\n  - {{path: {LOCK}, holder: 4242, mode: exclusive}}\n"
    );
    let tree = format!(
        "{}  /usr/bin/diskgc: {{}}\n  /home/u/.local/state/diskgc: {{.: {{mode: \"0700\"}}, index-dev-0-42.lock: {{mode: \"0600\"}}}}\n",
        old_candidates(2, "1M")
    );
    let yaml = yaml("9G", &tree, &rest).replace(
        "processes:\n",
        "processes:\n  - {pid: 4242, exe: /usr/bin/diskgc, cwd: /}\n",
    );
    for mode in [
        reclaim("2G", "3G", "10s"),
        Mode::TallyOnly { min_free: None },
    ] {
        let host = host(&yaml);
        let run = watch(&host, &mode);
        assert_eq!(run.ended, Err(2), "{mode:?}");
        assert!(
            run.has("another watch holds index-dev-0-42.lock"),
            "{:?}",
            run.lines
        );
        assert!(run.has("\"exit_code\":2"), "the start-failure object");
        assert!(host.elapsed() >= Duration::from_secs(10), "it waited 10 s");
        assert!(host.elapsed() < Duration::from_secs(12));
        assert_eq!(deletions(&host), 0);
        assert_eq!(host.trace_count(Op::StateWrite), 0);
    }
}

/// The holder is an `ensure` writing: released within the 10 s, the
/// watch starts.
#[test]
fn watch_waits_for_an_ensure_writing_then_starts() {
    let rest = format!(
        "events:\n  - {{at: {{elapsed: 3s}}, exit: 4242}}\n  - {{at: {{elapsed: 1m}}, signal: term}}\nlocks:\n  - {{path: {LOCK}, holder: 4242, mode: exclusive}}\n"
    );
    let tree = "  /usr/bin/diskgc: {}\n  /home/u/.local/state/diskgc: {.: {mode: \"0700\"}, index-dev-0-42.lock: {mode: \"0600\"}}\n";
    let yaml = yaml("5G", tree, &rest).replace(
        "processes:\n",
        "processes:\n  - {pid: 4242, exe: /usr/bin/diskgc, cwd: /}\n",
    );
    let host = host(&yaml);
    let run = watch(&host, &reclaim("2G", "3G", "10s"));
    assert_eq!(run.ended, Ok(Ended::Stopped(Signal::Term)));
    assert!(
        host.trace_count(Op::StateWrite) >= 1,
        "it writes once at stop"
    );
}

/// Refusals at start: each its exit code, nothing deleted.
#[test]
fn watch_start_refusals() {
    // The sandbox gate refuses a reclaiming watch, not --tally-only.
    let gated = yaml(
        "5G",
        "",
        "sandbox_gate: closed\nevents:\n  - {at: {elapsed: 1m}, signal: term}\n",
    );
    let host1 = host(&gated);
    assert_eq!(watch(&host1, &reclaim("2G", "3G", "10s")).ended, Err(2));
    let host2 = host(&gated);
    assert_eq!(
        watch(&host2, &Mode::TallyOnly { min_free: None }).ended,
        Ok(Ended::Stopped(Signal::Term))
    );
    // No host lock file and a per-user lock that is refused (its
    // directory a symlink): exit 2 (provisioning); --tally-only warns.
    // (Root without the host lock: watch_as_root_without_the_host_lock_*;
    // a user's usable per-user lock: a_user_watch_without_the_host_lock_*.)
    let unprovisioned = yaml(
        "5G",
        "  /run/user/1000: {.: {mode: \"0700\"}, x: {.: {mode: \"0700\"}}, diskgc: {symlink: /run/user/1000/x}}\n",
        "runtime_dir: /run/user/1000\nevents:\n  - {at: {elapsed: 1m}, signal: term}\n",
    )
    .replace("  /run/diskgc/lock: {uid: 0, gid: 0}\n", "");
    let host3 = host(&unprovisioned);
    let run = watch(&host3, &reclaim("2G", "3G", "10s"));
    assert_eq!(run.ended, Err(2));
    assert!(run.has("\"cause\":\"provisioning\""), "{:?}", run.lines);
    assert!(
        run.has("per-user lock directory is a symlink"),
        "{:?}",
        run.lines
    );
    let host4 = host(&unprovisioned);
    let run = watch(&host4, &Mode::TallyOnly { min_free: None });
    assert_eq!(run.ended, Ok(Ended::Stopped(Signal::Term)));
    assert!(run.has("warning: the host lock"), "{:?}", run.lines);
    // --target-free below --min-free.
    let host5 = host(&yaml("5G", "", ""));
    assert_eq!(watch(&host5, &reclaim("3G", "2G", "10s")).ended, Err(2));
    for host in [&host1, &host3, &host5] {
        assert_eq!(deletions(host), 0);
    }
}

// ---- Signals ----

/// SIGTERM in a cycle: the record says `interrupted`, the process stops
/// (exit 0), the snapshot is written once, at the stop.
#[test]
fn watch_sigterm_in_a_cycle_writes_an_interrupted_record_and_stops() {
    // The signal on the second unlinkat of the cycle.
    let events = "events:\n  - {at: {elapsed: 30s}, other_used: {mount: /tmp, bytes: 9G}}\n  \
                  - {at: {op: unlinkat, nth: 2, when: after}, signal: term}\n";
    let host = host(&yaml("5G", &old_candidates(10, "300M"), events));
    let run = watch(&host, &reclaim("2G", "4G", "10s"));
    assert_eq!(run.ended, Ok(Ended::Stopped(Signal::Term)));
    assert_eq!(run.records.len(), 1);
    assert_eq!(run.records[0].1["status"], "interrupted");
    assert_eq!(run.records[0].1["watch"]["backoff"], Value::Null);
    assert_eq!(host.trace_count(Op::StateWrite), 1, "the stop's write");
    assert!(run.has("stopping on SIGTERM"));
}

/// A signal during the start's single-writer wait: exit 0, nothing
/// written.
#[test]
fn a_signal_during_the_start_wait_exits_0_writing_nothing() {
    let rest = format!(
        "events:\n  - {{at: {{elapsed: 2s}}, signal: int}}\nlocks:\n  - {{path: {LOCK}, holder: 4242, mode: exclusive}}\n"
    );
    let tree = "  /usr/bin/diskgc: {}\n  /home/u/.local/state/diskgc: {.: {mode: \"0700\"}, index-dev-0-42.lock: {mode: \"0600\"}}\n";
    let yaml = yaml("5G", tree, &rest).replace(
        "processes:\n",
        "processes:\n  - {pid: 4242, exe: /usr/bin/diskgc, cwd: /}\n",
    );
    let host = host(&yaml);
    let run = watch(&host, &reclaim("2G", "3G", "10s"));
    assert_eq!(run.ended, Err(0));
    assert_eq!(host.trace_count(Op::StateWrite), 0);
}

/// A record that cannot be written ends `watch` at once
/// ([`Ended::OutputFailed`], exit 3 in the binary): no snapshot write
/// after it, no later cycle.
#[test]
fn a_failed_record_write_ends_watch_at_once() {
    let events = "events:\n  - {at: {elapsed: 30s}, other_used: {mount: /tmp, bytes: 9G}}\n  \
                  - {at: {elapsed: 30m}, signal: term}\n";
    let host = host(&yaml("5G", &old_candidates(3, "1M"), events));
    let library = library();
    let builtins = || Ok(library.clone());
    let platform = Platform {
        catalog: Catalog::Development,
        draw_seed: &|| Ok(0),
        builtins: &builtins,
    };
    let mut lines = Vec::new();
    let started = watch::start(
        &host,
        &reclaim("2G", "3G", "10s"),
        &selection(),
        &ConfigFile::NotFound { tried: Vec::new() },
        &platform,
        None,
        &mut |l: &str| lines.push(l.to_owned()),
    )
    .expect("starts");
    let mut broken = |_: &str| Err("Broken pipe (os error 32)".to_owned());
    let (ended, _) = watch::run_inline(&host, started, &|| Ok(0), Some(&mut broken), &mut |l| {
        lines.push(l.to_owned())
    });
    assert_eq!(
        ended,
        Ended::OutputFailed("Broken pipe (os error 32)".to_owned())
    );
    assert!(host.elapsed() < Duration::from_secs(60), "ended at once");
    assert_eq!(host.trace_count(Op::StateWrite), 0, "{lines:#?}");
}

// ---- Review of TASK-32 ----

/// mped B1: the background tally's listing of a source failed with a
/// failure of the host (EIO on one branch) after an earlier complete
/// listing filled the index. The next cycle takes no queue from the index:
/// its own discovery meets the failure first and nothing is deleted. (With
/// the queue, the units of the healthy branch would be deleted before the
/// discovery stops.) A complete listing later makes the index usable again.
#[test]
fn a_failed_background_listing_keeps_the_next_cycle_off_the_index() {
    let profiles = r#"
- name: tmp-deep
  find: {roots: [/tmp], paths: ["*/a*"], type: dir}
  action: delete
  examples: {match: [/tmp/p/a0]}
"#;
    let candidate = |path: &str| {
        format!("  {path}: {{.: {{mtime: {OLD}}}, f: {{size: 100M, mtime: {OLD}}}}}\n")
    };
    let tree = format!(
        "  /tmp/p1: {{.: {{mtime: {OLD}}}}}\n  /tmp/p2: {{.: {{mtime: {OLD}}}}}\n{}{}{}{}",
        candidate("/tmp/p1/a0"),
        candidate("/tmp/p1/a1"),
        candidate("/tmp/p1/a2"),
        candidate("/tmp/p2/a9"),
    );
    // Listing 1 (start) reads /tmp/p2 cleanly; listing 2 (5 min) and the
    // cycle's discovery (6 min) fail there.
    let events = "events:\n  \
                  - {at: {op: read_dir, path: /tmp/p2, nth: 2, when: before}, fail: EIO}\n  \
                  - {at: {op: read_dir, path: /tmp/p2, nth: 3, when: before}, fail: EIO}\n  \
                  - {at: {elapsed: 6m}, other_used: {mount: /tmp, bytes: 9G}}\n  \
                  - {at: {elapsed: 390s}, signal: term}\n";
    let host = host(&yaml("5G", &tree, events));
    let specs = parse_profiles(profiles).unwrap();
    let library = Library::with_user_profiles(&specs).unwrap();
    let builtins = || Ok(library.clone());
    let platform = Platform {
        catalog: Catalog::Development,
        draw_seed: &|| Ok(0),
        builtins: &builtins,
    };
    let selection = Selection {
        select: Some("profile:tmp-deep && filter:idle-1h".to_owned()),
        ..selection()
    };
    let mut lines = Vec::new();
    let started = watch::start(
        &host,
        &reclaim("2G", "3G", "10s"),
        &selection,
        &ConfigFile::NotFound { tried: Vec::new() },
        &platform,
        None,
        &mut |l: &str| lines.push(l.to_owned()),
    )
    .expect("starts");
    let mut records = Vec::new();
    let mut record = |line: &str| {
        records.push(serde_json::from_str::<Value>(line).unwrap());
        Ok(())
    };
    watch::run_inline(&host, started, &|| Ok(0), Some(&mut record), &mut |l| {
        lines.push(l.to_owned())
    });
    eprintln!("log:\n  {}", lines.join("\n  "));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("the background tally's last listing of /tmp")),
        "{lines:#?}"
    );
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["status"], "error");
    assert_eq!(records[0]["watch"]["backoff"]["class"], "retry");
    assert_eq!(
        deletions(&host),
        0,
        "nothing deleted before discovery's EIO"
    );
}

/// Codex 6: a full filesystem (0 bytes available) after an `exhausted`
/// cycle: the drop of rule 1 cannot happen, so cycles come 15 minutes
/// apart, not at every interval; the record says `resume_below_bytes:
/// null` and the log why.
#[test]
fn a_full_filesystem_backs_off_15_minutes_not_every_interval() {
    let fresh: String = (0..3)
        .map(|i| format!("  /tmp/a{i:02}: {{f: {{size: 10M}}}}\n"))
        .collect();
    let events = "events:\n  - {at: {elapsed: 40m}, signal: term}\n";
    let host = host(&yaml("10G", &fresh, events));
    assert_eq!(avail(&host), 0);
    let run = watch(&host, &reclaim("2G", "3G", "10s"));
    let starts: Vec<u64> = run.records.iter().map(|(t, _)| t.as_secs()).collect();
    assert_eq!(starts.len(), 3, "{starts:?}");
    for (_, r) in &run.records {
        assert_eq!(r["status"], "exhausted");
        assert_eq!(r["watch"]["backoff"]["resume_below_bytes"], Value::Null);
    }
    assert!(run.has("cannot fall by the step"), "{:?}", run.lines);
}

/// Codex 9: `ranking: random` shuffles on each reclaim: every cycle draws
/// its own seed (the record's `seed`), as each `ensure` would.
#[test]
fn a_random_group_is_shuffled_again_for_each_cycle() {
    let config = format!(
        "version: 1\nprofiles:\n{}groups:\n  - name: g\n    ranking: random\n    tiers:\n      \
         - {{name: a, select: \"profile:tmp-a && filter:idle-1h\"}}\n      \
         - {{name: b, select: \"profile:tmp-b && filter:idle-1h\"}}\n",
        "  - {name: tmp-a, find: {roots: [/tmp], paths: [\"a*\"], type: dir}, action: delete, examples: {match: [/tmp/a0]}}\n  \
         - {name: tmp-b, find: {roots: [/tmp], paths: [\"b*\"], type: dir}, action: delete, examples: {match: [/tmp/b0]}}\n"
    );
    let fresh: String = (0..2)
        .map(|i| format!("  /tmp/a{i}: {{f: {{size: 10M}}}}\n  /tmp/b{i}: {{f: {{size: 10M}}}}\n"))
        .collect();
    let events = "events:\n  - {at: {elapsed: 35m}, signal: term}\n";
    let host = host(&yaml("9G", &fresh, events));
    let selection = Selection {
        select: None,
        ..selection()
    };
    let file = ConfigFile::Read {
        path: PathBuf::from("/home/u/.config/diskgc/config.yaml"),
        bytes: config.into_bytes(),
    };
    let run = watch_with(&host, &reclaim("2G", "3G", "10s"), &selection, &file);
    let seeds: Vec<&Value> = run.records.iter().map(|(_, r)| &r["seed"]).collect();
    // The start drew 1; the three cycles 2, 3, 4.
    assert_eq!(seeds, [&Value::from(2), &Value::from(3), &Value::from(4)]);
}

/// Codex 2: under `--allow-root`, `owners:` names resolve again before
/// each cycle's first deletion. The account `tester` is gone by the
/// cycle: it refuses (an error of `config`, back-off `hold`), nothing
/// deleted for the uid the name had at start.
#[test]
fn an_owner_name_gone_since_start_refuses_the_cycle() {
    let config = "version: 1\nowners: [tester]\nprofiles:\n  - {name: tmp-a, find: {roots: [/tmp], paths: [\"a*\"], type: dir}, action: delete, examples: {match: [/tmp/a0]}}\ngroups:\n  - name: g\n    tiers:\n      - {name: a, select: \"profile:tmp-a && filter:idle-1h\"}\n";
    let tree: String = (0..3)
        .map(|i| {
            format!(
                "  /tmp/a{i:02}: {{.: {{mtime: {OLD}, uid: 2000}}, f: {{size: 100M, mtime: {OLD}, uid: 2000}}}}\n"
            )
        })
        .collect();
    let events = "own_uid: 0\nusers: {tester: 2000}\nevents:\n  \
                  - {at: {elapsed: 1m}, account: {name: tester, uid: null}}\n  \
                  - {at: {elapsed: 2m}, other_used: {mount: /tmp, bytes: 9G}}\n  \
                  - {at: {elapsed: 3m}, signal: term}\n";
    let host = host(&yaml("5G", &tree, events));
    let selection = Selection {
        select: None,
        allow_root: true,
        ..selection()
    };
    let file = ConfigFile::Read {
        path: PathBuf::from("/etc/diskgc/config.yaml"),
        bytes: config.as_bytes().to_vec(),
    };
    let run = watch_with(&host, &reclaim("2G", "3G", "10s"), &selection, &file);
    assert_eq!(
        run.ended,
        Ok(Ended::Stopped(Signal::Term)),
        "{:?}",
        run.lines
    );
    assert_eq!(run.records.len(), 1, "{:?}", run.records);
    let r = &run.records[0].1;
    assert_eq!(r["status"], "error");
    assert_eq!(r["errors"][0]["cause"], "config");
    assert!(
        r["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("\"tester\" does not exist"),
        "{r}"
    );
    assert_eq!(r["watch"]["backoff"]["class"], "hold");
    assert_eq!(deletions(&host), 0);
}

/// TASK-44 AC #4 and its review: a `watch --allow-root` exits at start in
/// both modes, before it reaches `PATH`, the host lock or the state
/// directory, when its effective set lacks `CAP_SYS_PTRACE` (2, `safety`),
/// when it is not root although it holds the capability (2, `safety`), and
/// when its capability set cannot be read (3, `host`); root with the
/// capability starts (and stops on the SIGTERM).
#[test]
fn watch_allow_root_without_cap_sys_ptrace_is_refused_at_start_in_both_modes() {
    let config = "version: 1\nowners: [tester]\nprofiles:\n  - {name: tmp-a, find: {roots: [/tmp], paths: [\"a*\"], type: dir}, action: delete, examples: {match: [/tmp/a0]}}\ngroups:\n  - name: g\n    tiers:\n      - {name: a, select: \"profile:tmp-a && filter:idle-1h\"}\n";
    let file = ConfigFile::Read {
        path: PathBuf::from("/etc/diskgc/config.yaml"),
        bytes: config.as_bytes().to_vec(),
    };
    let selection = Selection {
        select: None,
        allow_root: true,
        ..selection()
    };
    let ptrace = 1u64 << diskgc_core::executor::CAP_SYS_PTRACE;
    let all = diskgc_model::state::ROOT_CAPABILITIES;
    let unreadable = "events:\n  - {at: {op: proc_status, nth: 1, when: before}, fail: EIO}\n";
    let modes = [
        reclaim("2G", "3G", "10s"),
        Mode::TallyOnly { min_free: None },
    ];
    // (own uid, its set, extra model lines, the refusal: exit code, cause,
    // message; `None`: it starts)
    type Refusal = Option<(u8, &'static str, &'static str)>;
    let cases: [(u32, u64, &str, Refusal); 4] = [
        (
            0,
            !ptrace & all,
            "",
            Some((2, "safety", "--allow-root needs CAP_SYS_PTRACE")),
        ),
        (
            1000,
            ptrace,
            "",
            Some((2, "safety", "--allow-root is for root; drop it")),
        ),
        (
            0,
            all,
            unreadable,
            Some((3, "host", "could not read its own capability set")),
        ),
        (0, ptrace, "", None),
    ];
    for mode in &modes {
        for (uid, caps, extra, refusal) in cases {
            let events = if extra.is_empty() {
                "events:\n  - {at: {elapsed: 1m}, signal: term}\n"
            } else {
                extra
            };
            let rest = format!(
                "own_uid: {uid}\nown_cap_effective: {caps}\nusers: {{tester: 2000}}\n{events}"
            );
            let host = host(&yaml("1G", &old_candidates(1, "100M"), &rest));
            let run = watch_with(&host, mode, &selection, &file);
            let case = format!("{mode:?}, uid {uid}, caps {caps:#x}");
            match refusal {
                Some((code, cause, message)) => {
                    assert_eq!(run.ended, Err(code), "{case}: {:?}", run.lines);
                    let report = run.lines.last().expect("the refusal's report");
                    let report: Value = serde_json::from_str(report).expect("JSON");
                    assert_eq!(report["errors"][0]["cause"], cause, "{case}: {report}");
                    let text = report["errors"][0]["message"].as_str().unwrap();
                    assert!(text.contains(message), "{case}: {text}");
                    for op in [Op::Statvfs, Op::StateLock, Op::Flock, Op::Unlinkat] {
                        assert_eq!(host.trace_count(op), 0, "{case}: {op} after the refusal");
                    }
                }
                None => assert_eq!(
                    run.ended,
                    Ok(Ended::Stopped(Signal::Term)),
                    "{case}: {:?}",
                    run.lines
                ),
            }
        }
    }
}

/// Codex 7 / mped N2: the first sample is not a start check. It fails at
/// start, with percentage thresholds: `watch` starts (no exit 3); the
/// loop's first sample fails too and is the first cycle (`error`, `host`,
/// `retry`); the next one that works resolves the goal against the
/// filesystem's real size, which the back-off's 1 % then uses.
#[test]
fn a_failing_first_sample_is_the_first_cycle_and_the_goal_resolves_later() {
    let events = "events:\n  \
                  - {at: {op: statvfs, path: /tmp, nth: 1, when: before}, fail: EIO}\n  \
                  - {at: {op: statvfs, path: /tmp, nth: 2, when: before}, fail: EIO}\n  \
                  - {at: {elapsed: 5m}, signal: term}\n";
    let host = host(&yaml("5G", &old_candidates(2, "1M"), events));
    let mode = Mode::Reclaim {
        min_free: parse_size("20%").unwrap(),
        target_free: parse_size("30%").unwrap(),
        interval: Duration::from_secs(10),
        timeout: Duration::from_secs(60),
    };
    let run = watch(&host, &mode);
    assert_eq!(
        run.ended,
        Ok(Ended::Stopped(Signal::Term)),
        "{:?}",
        run.lines
    );
    assert!(run.has("warning: the first sample of PATH /tmp failed"));
    assert_eq!(run.records.len(), 1, "{:?}", run.records);
    let r = &run.records[0].1;
    assert_eq!(r["status"], "error");
    assert_eq!(r["errors"][0]["cause"], "host");
    assert_eq!(r["min_free_bytes"], Value::Null, "not resolved yet: {r}");
    assert_eq!(r["watch"]["backoff"]["class"], "retry");
    // About 4.5 GiB available is above 20 % of 10 GiB: the next sample that
    // works ends the back-off.
    assert!(run.has("space recovered"), "{:?}", run.lines);
    assert_eq!(deletions(&host), 0);
}

/// docs/testing.md `watch_failed_samples_back_off`: `statvfs` of the
/// watched filesystem failing for an hour gives `error` records (`cause:
/// host`) at 0, 1, 3, 7, 15, 30, 45 and 60 minutes, not one per interval,
/// and the process keeps running.
#[test]
fn watch_failed_samples_back_off() {
    let mut events = String::from("events:\n");
    for nth in 1..=370 {
        events += &format!(
            "  - {{at: {{op: statvfs, path: /tmp, nth: {nth}, when: before}}, fail: EIO}}\n"
        );
    }
    events += "  - {at: {elapsed: 61m}, signal: term}\n";
    let host = host(&yaml("5G", &old_candidates(2, "1M"), &events));
    let run = watch(&host, &reclaim("2G", "3G", "10s"));
    assert_eq!(run.ended, Ok(Ended::Stopped(Signal::Term)));
    let minutes: Vec<u64> = run
        .records
        .iter()
        .map(|(t, _)| (t.as_secs() + 30) / 60)
        .collect();
    assert_eq!(
        minutes,
        [0, 1, 3, 7, 15, 30, 45, 60],
        "{:?}",
        run.records.len()
    );
    for (_, r) in &run.records {
        assert_eq!(r["status"], "error");
        assert_eq!(r["errors"][0]["cause"], "host");
        assert_eq!(r["watch"]["backoff"]["class"], "retry");
    }
    assert_eq!(deletions(&host), 0);
}
