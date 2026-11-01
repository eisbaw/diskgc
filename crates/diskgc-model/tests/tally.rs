//! The tally on ModelHost (docs/testing.md, synchronous tally, background
//! tally, pressure brake; TASK-29): the duty cycle as bounds on the virtual
//! clock, the pause while another process holds the host lock, the
//! pressure brake's sequence (each signal alone, the thresholds, no PSI),
//! a sizing walk that survives entries vanishing under it, the order of
//! overdue jobs, the synchronous tally's two passes (only the first tier
//! sized), and the recount of a block on a complete listing only.

mod common;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::config::Order;
use diskgc_core::fs_kind::Find;
use diskgc_core::host::{Dev, Host, MonoTime};
use diskgc_core::hostlock::ReclaimLock;
use diskgc_core::index::{Family, Finder, Index, MountKey, UnitRef};
use diskgc_core::profile::{EntryType, Where};
use diskgc_core::tally::background::{
    Background, BatchWork, Event, IDLE_SLEEP_MAX, Job, LOCK_POLL, due_jobs,
};
use diskgc_core::tally::budget::Budget;
use diskgc_core::tally::listing::{ListingCursor, SourceFind, TallySource};
use diskgc_core::tally::pace::{BATCH_OPS_MAX, BATCH_TIME_MAX};
use diskgc_core::tally::recount::Complete;
use diskgc_core::tally::sync::{Deferred, SyncTier, synchronous};
use diskgc_core::tally::{Rounded, Sizing, TallyState};
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;

const TMP_DEV: Dev = Dev {
    major: 0,
    minor: 42,
};

/// `/`, `/run` holding the provisioned host lock, and `/tmp` (ext4, so
/// birth times are reported). `extra` is appended at the top level.
fn yaml(tree: &str, extra: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
  - {{id: 30, at: /tmp, fs: ext4, dev: "0:42", size: 10G, parent: 1}}
tree:
  /run/diskgc/lock: {{uid: 0, gid: 0}}
{tree}{extra}"#
    )
}

/// `n` directories `/tmp/<root>/dNNN`, each holding `files` files of 4 KiB
/// and a subdirectory with one more.
fn dirs(root: &str, n: usize, files: usize) -> String {
    let mut tree = String::new();
    for i in 0..n {
        writeln!(tree, "  /tmp/{root}/d{i:03}:").unwrap();
        for f in 0..files {
            writeln!(tree, "    f{f}: {{size: 4K}}").unwrap();
        }
        writeln!(tree, "    sub: {{g: {{size: 4K}}}}").unwrap();
    }
    tree
}

fn finder(profile: &str, root: &str) -> Finder {
    Finder {
        profile: profile.into(),
        profile_hash: 1,
        root: Path::new(root).into(),
    }
}

/// Every directory directly below `root`.
fn source(profile: &str, root: &str) -> TallySource {
    TallySource {
        finder: finder(profile, root),
        find: SourceFind::Fs(Box::new(Find::new(
            &["*".to_owned()],
            EntryType::Dir,
            &Where::default(),
            1000,
        ))),
    }
}

fn tmp_mount() -> MountKey {
    MountKey {
        dev: TMP_DEV,
        mount_point: PathBuf::from("/tmp"),
    }
}

fn state() -> TallyState {
    TallyState::new(Index::new(1 << 24))
}

/// Allocated bytes and entries of the tree at `path` (the model's own
/// view, no port call): what a complete sizing must come to.
fn tree_size(host: &ModelHost, path: &Path, names: &[&str]) -> (u64, u64) {
    let mut bytes = 0;
    let mut entries = 0;
    let mut stack = vec![path.to_owned()];
    stack.extend(names.iter().map(|n| path.join(n)));
    for p in stack {
        let stat = host
            .stat_path(&p)
            .unwrap_or_else(|| panic!("{}", p.display()));
        bytes += stat.blocks.unwrap() * 512;
        entries += 1;
    }
    (bytes, entries)
}

fn at(host: &ModelHost) -> Duration {
    host.elapsed()
}

/// A turn's active time (the clock moves only by port calls here) and the
/// sleep it asked for.
#[derive(Debug, Clone, Copy)]
struct Turn {
    active: Duration,
    sleep: Duration,
    batch: Option<BatchWork>,
}

/// Runs `turns` turns of the background tally, sleeping as it says.
fn run_turns(
    host: &ModelHost,
    state: &mut TallyState,
    sources: &[TallySource],
    turns: usize,
    events: &mut Vec<Event>,
) -> Vec<Turn> {
    let mut log = |e: Event| events.push(e);
    let mut background = Background::start(host, &mut log);
    let mut out = Vec::new();
    for _ in 0..turns {
        let before = at(host);
        let mut batch = None;
        let mut log = |e: Event| {
            if let Event::Batch(work) = &e {
                batch = Some(*work);
            }
            events.push(e);
        };
        let sleep = background.turn(host, state, sources, &mut log);
        out.push(Turn {
            active: at(host) - before,
            sleep,
            batch,
        });
        host.sleep(sleep);
    }
    out
}

// ---- Background: duty cycle ----

/// AC #3: the worker is active at most 5 % (plus one batch) of any window
/// of 100 batches, and no batch takes more than 50 ms (plus the one
/// operation that started before the bound) or 2,000 operations: bounds,
/// not exact values, on the virtual clock (every port call costs 1 ms).
#[test]
fn background_duty_cycle_holds_in_every_window_of_100_batches() {
    let host = host(&yaml(&dirs("a", 40, 30), "call_cost: 1ms\n"));
    let sources = [source("a", "/tmp/a")];
    let mut state = state();
    let mut events = Vec::new();
    let turns = run_turns(&host, &mut state, &sources, 400, &mut events);
    let one_op = Duration::from_millis(1);
    // Past its bound a batch finishes at most the reach of one unit or
    // root (its path from the mount, about 4 calls here) and the call
    // under way (a candidate's checks), and, after a stalled step, the
    // entries it must do (STEP_ENTRIES_MIN = LISTING_STEP_MIN = 32). Every
    // batch, listings included (resumable, Mark 2026-10-25).
    let overshoot = one_op * (8 + diskgc_core::tally::size::STEP_ENTRIES_MIN as u32);
    let mut max_active = Duration::ZERO;
    for turn in &turns {
        let work = turn.batch.expect("never braked or paused here");
        assert!(work.ops <= BATCH_OPS_MAX, "{work:?}");
        assert!(turn.active <= BATCH_TIME_MAX + overshoot, "{turn:?}");
        max_active = max_active.max(turn.active);
        assert!(turn.sleep >= turn.active * 19, "{turn:?}");
    }
    for window in turns.windows(100) {
        let active: Duration = window.iter().map(|t| t.active).sum();
        let wall: Duration = window.iter().map(|t| t.active + t.sleep).sum();
        assert!(
            active <= wall / 20 + BATCH_TIME_MAX + overshoot,
            "active {active:?} of {wall:?}"
        );
    }
    eprintln!("longest batch {max_active:?} (bound {BATCH_TIME_MAX:?})");
    // It did the work: every unit sized, at a 5 % duty cycle.
    let mut sized = 0;
    state.index.for_each_unit(|_, _, unit| {
        if let UnitRef::Single(s) = unit {
            sized += usize::from(s.tally.is_some());
        }
    });
    assert_eq!(sized, 40, "{:?}", state.index.stats());
    // It lowered its priority once and read the pressure... never: the
    // model has no PSI by default (one probe at start).
    assert_eq!(host.priority_lowered(), 1);
    assert_eq!(host.trace_count(Op::Pressure), 1);
    assert!(matches!(events[0], Event::NoPsi(_)), "{:?}", events[0]);
}

/// The operation bound on its own: a batch's budget is spent after 2,000
/// operations even when no time passes.
#[test]
fn a_batch_budget_ends_at_2000_operations() {
    let host = host(&yaml("", ""));
    let mut budget = Budget::batch(host.now());
    for _ in 0..BATCH_OPS_MAX {
        assert!(!budget.spent(&host));
        budget.take(1);
    }
    assert!(budget.spent(&host));
}

// ---- Background: host lock ----

/// AC #1 (model; the container test is TASK-32's, with `watch`): while
/// another process holds the host lock, the tally makes no call below
/// `/tmp`; it polls every second and starts within a poll of the release.
#[test]
fn background_pauses_while_another_process_holds_the_host_lock() {
    let host = host(&yaml(
        &format!("{}  /usr/bin/diskgc: {{}}\n", dirs("a", 3, 2)),
        &held_until_30s("/run/diskgc/lock"),
    ));
    pauses_until_the_release(&host, &ReclaimLock::Host);
}

/// TASK-277: a non-root watch on a host without the host lock pauses for
/// its own user's per-user lock (`$XDG_RUNTIME_DIR/diskgc/lock`) instead.
#[test]
fn background_pauses_while_another_process_holds_the_per_user_lock() {
    const LOCK: &str = "/run/user/1000/diskgc/lock";
    let tree = format!(
        "{}  /usr/bin/diskgc: {{}}\n  /run/user/1000: {{.: {{mode: \"0700\"}}, diskgc: {{.: {{mode: \"0700\"}}, lock: {{mode: \"0600\"}}}}}}\n",
        dirs("a", 3, 2)
    );
    let model =
        yaml(&tree, &held_until_30s(LOCK)).replace("  /run/diskgc/lock: {uid: 0, gid: 0}\n", "");
    let host = host(&model);
    pauses_until_the_release(&host, &ReclaimLock::User(PathBuf::from(LOCK)));
}

/// TASK-277 review: the probe makes nothing. A per-user lock that does not
/// exist is held by no one: the tally runs, and no directory or file is
/// made for it (`--tally-only` must not write a lock).
#[test]
fn background_probe_makes_no_per_user_lock() {
    let tree = format!(
        "{}  /run/user/1000: {{.: {{mode: \"0700\"}}}}\n",
        dirs("a", 3, 2)
    );
    let model =
        yaml(&tree, "call_cost: 1ms\n").replace("  /run/diskgc/lock: {uid: 0, gid: 0}\n", "");
    let host = host(&model);
    let sources = [source("a", "/tmp/a")];
    let mut state = state();
    let mut events = Vec::new();
    let mut log = |e: Event| events.push(e);
    let mut background = Background::start(&host, &mut log);
    for _ in 0..5 {
        let sleep = background.turn(&host, &mut state, &sources, &mut log);
        host.sleep(sleep);
    }
    assert!(host.trace_count(Op::UserLock) > 0, "it probed");
    assert!(host.stat_path(Path::new("/run/user/1000/diskgc")).is_none());
    assert!(
        !events.iter().any(|e| matches!(e, Event::Paused(_))),
        "{events:?}"
    );
    assert!(state.listed_at(&sources[0].finder).is_some());
}

/// Process 4242 holds `lock` exclusively until it exits at 30 s.
fn held_until_30s(lock: &str) -> String {
    format!(
        "processes:\n  - {{pid: 4242, exe: /usr/bin/diskgc, cwd: /}}\n\
         locks:\n  - {{path: {lock}, holder: 4242, mode: exclusive}}\n\
         events:\n  - {{at: {{elapsed: 30s}}, exit: 4242}}\n\
         call_cost: 1ms\n"
    )
}

/// The background tally on `host` (sources below `/tmp/a`) touches nothing
/// below `/tmp` until the holder of `lock` exits at 30 s, polling every
/// [`LOCK_POLL`], and starts within a poll of the release; its pause names
/// `lock`.
fn pauses_until_the_release(host: &ModelHost, lock: &ReclaimLock) {
    let sources = [source("a", "/tmp/a")];
    let mut state = state();
    let mut events = Vec::new();
    let mut log = |e: Event| events.push(e);
    let mut background = Background::start(host, &mut log);
    let mut first_tmp_call_at = None;
    while at(host) < Duration::from_secs(40) {
        let before = at(host);
        let calls = host.trace().len();
        let sleep = background.turn(host, &mut state, &sources, &mut log);
        let touched = host.trace()[calls..]
            .iter()
            .any(|c| c.path.as_ref().is_some_and(|p| p.starts_with("/tmp")));
        if touched && first_tmp_call_at.is_none() {
            first_tmp_call_at = Some(before);
        }
        if before < Duration::from_secs(30) {
            assert!(
                !touched,
                "a call below /tmp at {before:?} while the lock is held"
            );
            assert_eq!(sleep, LOCK_POLL);
        }
        host.sleep(sleep);
    }
    let started = first_tmp_call_at.expect("it ran after the release");
    assert!(
        started < Duration::from_secs(30) + LOCK_POLL + Duration::from_millis(100),
        "started at {started:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Paused(paused) if paused == lock)),
        "{events:?}"
    );
    assert!(state.listed_at(&sources[0].finder).is_some());
}

// ---- Background: pressure brake ----

fn pressure_yaml(steps: &str) -> String {
    yaml(
        &dirs("a", 2, 1),
        &format!("call_cost: 1ms\npressure:\n{steps}"),
    )
}

/// docs/testing.md `pressure_brake_doubles_the_sleep_and_skips_batches`:
/// with `io` at 30 %, no batch runs and the sleeps are 1, 2, 4 ... s up to
/// 60 s; the first reading below every threshold runs a batch and returns
/// to 19 × t.
#[test]
fn pressure_brake_doubles_the_sleep_and_skips_batches() {
    let host = host(&pressure_yaml(
        "  - {after: 0s, io: 3000, memory: 0, cpu: 0}\n  - {after: 400s, io: 999, memory: 999, cpu: 4999}\n",
    ));
    let sources = [source("a", "/tmp/a")];
    let mut state = state();
    let mut events = Vec::new();
    let turns = run_turns(&host, &mut state, &sources, 14, &mut events);
    let braked: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Event::Braked { sleep, .. } => Some(sleep.as_secs()),
            _ => None,
        })
        .collect();
    // 1+2+4+8+16+32+60x5 = 363 s; the next 60 s ends past 400 s.
    assert_eq!(braked, [1, 2, 4, 8, 16, 32, 60, 60, 60, 60, 60, 60]);
    let first_batch = turns.iter().position(|t| t.batch.is_some()).unwrap();
    assert_eq!(first_batch, braked.len());
    let batch = turns[first_batch];
    assert_eq!(batch.sleep, batch.active * 19, "{batch:?}");
    assert!(state.listed_at(&sources[0].finder).is_some());
}

/// Each of `io`, `memory` (10 %) and `cpu` (50 %) alone brakes above its
/// threshold; a reading at a threshold does not.
#[test]
fn each_pressure_signal_alone_brakes_above_its_threshold_not_at_it() {
    for (io, memory, cpu, brakes) in [
        (1001, 0, 0, true),
        (0, 1001, 0, true),
        (0, 0, 5001, true),
        (1000, 0, 0, false),
        (0, 1000, 0, false),
        (0, 0, 5000, false),
        (1000, 1000, 5000, false),
    ] {
        let host = host(&pressure_yaml(&format!(
            "  - {{after: 0s, io: {io}, memory: {memory}, cpu: {cpu}}}\n"
        )));
        let mut events = Vec::new();
        let turns = run_turns(
            &host,
            &mut state(),
            &[source("a", "/tmp/a")],
            1,
            &mut events,
        );
        assert_eq!(
            turns[0].batch.is_none(),
            brakes,
            "io {io} memory {memory} cpu {cpu}: {events:?}"
        );
        assert_eq!(
            events.iter().any(|e| matches!(e, Event::Braked { .. })),
            brakes
        );
    }
}

/// docs/testing.md `no_psi_means_duty_cycle_only`: without PSI one log
/// line at start, then the plain duty cycle and no further reads.
#[test]
fn no_psi_means_duty_cycle_only() {
    let host = host(&yaml(&dirs("a", 2, 1), "call_cost: 1ms\n"));
    let mut events = Vec::new();
    let turns = run_turns(
        &host,
        &mut state(),
        &[source("a", "/tmp/a")],
        20,
        &mut events,
    );
    let no_psi = events
        .iter()
        .filter(|e| matches!(e, Event::NoPsi(_)))
        .count();
    assert_eq!(no_psi, 1);
    assert!(matches!(events[1], Event::Started { psi: false }));
    assert_eq!(host.trace_count(Op::Pressure), 1, "only the probe at start");
    for turn in turns {
        let batch = turn.batch.expect("never braked");
        assert!(turn.sleep >= turn.active * 19, "{turn:?}");
        assert!(
            turn.sleep <= IDLE_SLEEP_MAX.max(turn.active * 19),
            "{batch:?}"
        );
    }
}

// ---- Sizing: the cursor ----

/// A sizing walk resumes across batches by names, and a directory that
/// vanishes between two batches ends only its branch: the walk completes
/// with what is left. Nothing is held open between batches.
#[test]
fn a_sizing_cursor_survives_a_directory_vanishing_between_batches() {
    let mut tree = String::from("  /tmp/a/big:\n");
    for (dir, files) in [("a", 60), ("b", 30), ("c", 30)] {
        writeln!(tree, "    {dir}:").unwrap();
        for f in 0..files {
            writeln!(tree, "      f{f}: {{size: 4K}}").unwrap();
        }
    }
    // Batch 1 (50 ms at 1 ms a call) ends inside `a`; `b` goes at 1 s.
    let host = host(&yaml(
        &tree,
        "call_cost: 1ms\nevents:\n  - {at: {elapsed: 1s}, remove: /tmp/a/big/b}\n",
    ));
    let mut state = state();
    let listed = state
        .list_source(&host, &source("a", "/tmp/a"), None)
        .unwrap();
    assert_eq!(listed.new, 1);
    let mount = tmp_mount();
    let path = Path::new("/tmp/a/big");
    let mut steps = Vec::new();
    loop {
        let mounts = host.mountinfo().unwrap();
        let mut budget = Budget::batch(host.now());
        let sizing = state
            .size_unit(&host, &mounts, &mount, path, &mut budget)
            .unwrap();
        steps.push(sizing);
        assert_eq!(host.open_fds(), 0, "an fd held between batches");
        if sizing != Sizing::Paused {
            break;
        }
        assert!(state.cursor_unit().is_some());
        assert!(state.index.accounted_bytes() > 0);
        host.sleep(Duration::from_secs(1));
        assert!(steps.len() < 100, "the walk does not end");
    }
    assert_eq!(steps.last(), Some(&Sizing::Done), "{steps:?}");
    assert!(steps.len() >= 2, "it paused: {steps:?}");
    assert!(state.cursor_unit().is_none());
    let tally = state
        .index
        .single(&mount, path)
        .unwrap()
        .tally
        .expect("sized");
    // `big`, `a` and `c` with their files; `b` was removed before the walk
    // reached it (the first batch ended inside `a`).
    let names: Vec<String> = [("a", 60), ("c", 30)]
        .iter()
        .flat_map(|(d, n)| {
            std::iter::once((*d).to_owned()).chain((0..*n).map(move |f| format!("{d}/f{f}")))
        })
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let (bytes, entries) = tree_size(&host, path, &names);
    assert_eq!((tally.est_bytes, tally.files), (bytes, entries));
}

/// The directory the walk was in vanishes between two batches: its branch
/// ends (what was counted in it stays counted), and the walk completes.
#[test]
fn a_sizing_cursor_resumes_when_the_directory_it_was_in_vanishes() {
    let mut tree = String::from("  /tmp/a/big:\n");
    for dir in ["a", "b", "c"] {
        writeln!(tree, "    {dir}:").unwrap();
        for f in 0..30 {
            writeln!(tree, "      f{f}: {{size: 4K}}").unwrap();
        }
    }
    // Batch 1 ends inside `b`, which goes at 1 s.
    let host = host(&yaml(
        &tree,
        "call_cost: 1ms\nevents:\n  - {at: {elapsed: 1s}, remove: /tmp/a/big/b}\n",
    ));
    let mut state = state();
    state
        .list_source(&host, &source("a", "/tmp/a"), None)
        .unwrap();
    let (mount, path) = (tmp_mount(), Path::new("/tmp/a/big"));
    let mut steps = Vec::new();
    while steps.last() != Some(&Sizing::Done) {
        let mounts = host.mountinfo().unwrap();
        let mut budget = Budget::batch(host.now());
        steps.push(
            state
                .size_unit(&host, &mounts, &mount, path, &mut budget)
                .unwrap(),
        );
        assert_eq!(host.open_fds(), 0);
        host.sleep(Duration::from_secs(1));
        assert!(steps.len() < 100, "{steps:?}");
    }
    let files = state
        .index
        .single(&mount, path)
        .unwrap()
        .tally
        .unwrap()
        .files;
    // big, a and c with their files (63), plus `b` and the files of it
    // counted before it went.
    assert!((63 + 1..63 + 31).contains(&files), "{files} {steps:?}");
}

// ---- Aging ----

/// The next jobs are the most overdue relative to their period: a never
/// sized single, due since the tally started 86 minutes ago (26/60 = 0.43
/// late), before a single re-checked 13 minutes ago (0.3) before one sized
/// 75 minutes ago (0.25) before a source listed 6 minutes ago (0.2).
#[test]
fn due_jobs_are_ordered_by_lateness_over_period() {
    let host = host(&yaml(
        "  /tmp/a/x: {f: {size: 4K}}\n  /tmp/a/y: {f: {size: 4K}}\n  /tmp/a/z: {f: {size: 4K}}\n",
        "",
    ));
    let sources = [source("a", "/tmp/a")];
    let mut state = state();
    let started = host.now();
    host.sleep(Duration::from_secs(80 * 60));
    state.list_source(&host, &sources[0], None).unwrap();
    host.sleep(Duration::from_secs(6 * 60)); // listed 6 minutes ago
    let now = host.now();
    let ago = |m: u64| MonoTime(now.0 - Duration::from_secs(m * 60));
    let mount = tmp_mount();
    let set = |state: &mut TallyState, name: &str, checked: u64, tallied: Option<u64>| {
        let single = state
            .index
            .single_mut(&mount, &Path::new("/tmp/a").join(name))
            .unwrap();
        single.checked_at = ago(checked);
        single.tally = tallied.map(|m| diskgc_core::index::Tally {
            est_bytes: 1,
            files: 1,
            tallied_at: ago(m),
        });
    };
    set(&mut state, "x", 13, Some(5)); // re-check 0.3 late
    set(&mut state, "y", 1, Some(75)); // re-tally 0.25 late
    set(&mut state, "z", 2, None); // never sized: due since the start
    let (jobs, _) = due_jobs(&state, &sources, started, now);
    let p = |n: &str| Path::new("/tmp/a").join(n);
    assert_eq!(
        jobs,
        [
            Job::Size(mount.clone(), p("z")),
            Job::Recheck(
                mount.clone(),
                p("x"),
                state.index.single(&mount, &p("x")).unwrap().identity
            ),
            Job::Size(mount.clone(), p("y")),
            Job::List(0),
        ]
    );
}

/// No starvation under churn (scaled down from docs/testing.md's
/// 100,000-entry model): with new candidates appearing every minute and a
/// backlog to size, every source is listed within twice its period, every
/// single the listings do not refresh (another finder's) is re-checked
/// within twice its period, and every single is sized within twice its
/// period of being found.
#[test]
fn background_serves_every_kind_of_job_within_twice_its_period() {
    let mut extra = String::from("call_cost: 1ms\nevents:\n");
    for minute in 1..120 {
        writeln!(
            extra,
            "  - {{at: {{elapsed: {minute}m}}, mkdir: /tmp/a/new{minute:03}}}"
        )
        .unwrap();
    }
    let tree = format!("{}{}", dirs("a", 60, 5), dirs("b", 20, 2));
    let host = host(&yaml(&tree, &extra));
    // `b`'s units are known (a snapshot's, another tier's) but not listed
    // by this tally: only re-checks keep them fresh.
    let sources = [source("a", "/tmp/a")];
    let mut state = state();
    state
        .list_source(&host, &source("b", "/tmp/b"), None)
        .unwrap();
    let mut events = Vec::new();
    let mut log = |e: Event| events.push(e);
    let mut background = Background::start(&host, &mut log);
    let mut listed_at: Vec<Duration> = Vec::new();
    let mut checked: BTreeMap<PathBuf, Vec<Duration>> = BTreeMap::new();
    while at(&host) < Duration::from_secs(150 * 60) {
        let sleep = background.turn(&host, &mut state, &sources, &mut log);
        if state.listed_at(&sources[0].finder).map(|t| t.0) != listed_at.last().copied() {
            listed_at.push(state.listed_at(&sources[0].finder).unwrap().0);
        }
        state.index.for_each_unit(|_, path, unit| {
            if let UnitRef::Single(s) = unit
                && path.starts_with("/tmp/b")
            {
                let seen = checked.entry(path.to_owned()).or_default();
                if seen.last() != Some(&s.checked_at.0) {
                    seen.push(s.checked_at.0);
                }
            }
        });
        host.sleep(sleep);
    }
    let twice = |period: u64| Duration::from_secs(2 * period * 60);
    for pair in listed_at.windows(2) {
        assert!(pair[1] - pair[0] <= twice(5), "listings {listed_at:?}");
    }
    assert!(listed_at.len() >= 25, "{listed_at:?}");
    for (path, times) in &checked {
        for pair in times.windows(2) {
            assert!(
                pair[1] - pair[0] <= twice(10),
                "{}: {times:?}",
                path.display()
            );
        }
        assert!(times.len() >= 10, "{}: {times:?}", path.display());
    }
    // Every single sized, the last found ones too, within 2 x 60 minutes
    // of being found (they were all found at least 30 minutes ago).
    let mut not_sized = Vec::new();
    state.index.for_each_unit(|_, path, unit| {
        if let UnitRef::Single(s) = unit
            && s.tally.is_none()
        {
            not_sized.push(path.to_owned());
        }
    });
    assert!(
        not_sized.iter().all(|p| p.starts_with("/tmp/a/new1")),
        "{not_sized:?}"
    );
}

// ---- Synchronous ----

fn sync_model(extra: &str) -> ModelHost {
    host(&yaml(
        &format!("{}{}", dirs("a", 3, 2), dirs("b", 3, 2)),
        extra,
    ))
}

/// Two tiers ordered `best-fit`: pass 2 sizes the first one's stale units
/// (under `oldest` it leaves them to the reclaim walk:
/// [`sync_tally_leaves_an_oldest_first_tiers_sizing_to_the_walk`]).
fn tiers() -> Vec<SyncTier> {
    tiers_ordered(Order::BestFit)
}

fn tiers_ordered(order: Order) -> Vec<SyncTier> {
    vec![
        SyncTier {
            sources: vec![source("a", "/tmp/a")],
            order,
        },
        SyncTier {
            sources: vec![source("b", "/tmp/b")],
            order,
        },
    ]
}

/// TASK-251: under `oldest` pass 2 sizes nothing; it reports the first
/// tier and its stale units, which the reclaim walk sizes as its queue
/// reaches them (tests/index_reclaim.rs, `deferred_sizing_*`). No tree
/// below a candidate is read.
#[test]
fn sync_tally_leaves_an_oldest_first_tiers_sizing_to_the_walk() {
    let host = sync_model("");
    let mut state = state();
    let report = synchronous(
        &host,
        &mut state,
        &tiers_ordered(Order::Oldest),
        TMP_DEV,
        None,
    )
    .unwrap();
    assert!(report.pass1_done && report.stopped.is_none(), "{report:?}");
    assert_eq!(report.listed.len(), 2);
    assert_eq!(
        (report.first_tier, report.sized, report.untallied),
        (None, 0, 0)
    );
    assert_eq!(report.deferred, Some(Deferred { tier: 0, stale: 3 }));
    let below = |p: &Path, root: &str| p.starts_with(root) && p != Path::new(root);
    assert!(
        !host.trace().iter().any(|c| c.op == Op::ReadDir
            && c.path
                .as_deref()
                .is_some_and(|p| below(p, "/tmp/a") || below(p, "/tmp/b"))),
        "no unit walked"
    );
    state.index.for_each_unit(|_, path, unit| {
        let UnitRef::Single(s) = unit else {
            panic!("no blocks here")
        };
        assert!(s.tally.is_none(), "{} sized", path.display());
    });
}

/// docs/testing.md `sync_tally_discovers_every_tier_before_sizing` and
/// `sync_tally_sizes_only_the_first_tiers_stale_units`: pass 1 lists every
/// tier before any tree is walked; pass 2 sizes only the first tier's
/// units. No sleep, no pressure read, no lock, no priority change.
#[test]
fn sync_tally_discovers_every_tier_then_sizes_only_the_first() {
    let host = sync_model("");
    let mut state = state();
    let report = synchronous(&host, &mut state, &tiers(), TMP_DEV, None).unwrap();
    assert!(report.pass1_done && report.stopped.is_none(), "{report:?}");
    assert_eq!(report.listed.len(), 2);
    assert_eq!(
        (report.first_tier, report.sized, report.untallied),
        (Some(0), 3, 0)
    );
    let trace = host.trace();
    let listing_b = trace
        .iter()
        .position(|c| c.op == Op::ReadDir && c.path.as_deref() == Some(Path::new("/tmp/b")))
        .expect("tier b listed");
    // Path::starts_with compares components: "/tmp/a/" is "/tmp/a".
    let below = |p: &Path, root: &str| p.starts_with(root) && p != Path::new(root);
    let first_walk = trace
        .iter()
        .position(|c| c.op == Op::ReadDir && c.path.as_deref().is_some_and(|p| below(p, "/tmp/a")))
        .expect("a unit of tier a walked");
    assert!(
        listing_b < first_walk,
        "sizing began before tier b was listed"
    );
    assert!(
        !trace.iter().any(|c| {
            c.op == Op::ReadDir && c.path.as_deref().is_some_and(|p| below(p, "/tmp/b"))
        })
    );
    for op in [
        Op::Sleep,
        Op::Pressure,
        Op::Flock,
        Op::LowerPriority,
        Op::Unlinkat,
    ] {
        assert_eq!(host.trace_count(op), 0, "{op}");
    }
    state.index.for_each_unit(|_, path, unit| {
        let UnitRef::Single(s) = unit else {
            panic!("no blocks here")
        };
        assert_eq!(
            s.tally.is_some(),
            path.starts_with("/tmp/a"),
            "{}",
            path.display()
        );
    });
}

/// With a seed (a second run): a source listed completely less than five
/// minutes before is not listed again (and not reported as discovered in
/// this run), only stale units are sized again.
#[test]
fn sync_tally_skips_a_recent_listing_and_sizes_only_stale_units() {
    let host = sync_model("");
    let mut state = state();
    synchronous(&host, &mut state, &tiers(), TMP_DEV, None).unwrap();
    host.sleep(Duration::from_secs(3 * 60));
    let reads = host.trace_count(Op::ReadDir);
    let report = synchronous(&host, &mut state, &tiers(), TMP_DEV, None).unwrap();
    assert_eq!(report.skipped_recent.len(), 2);
    assert!(report.listed.is_empty());
    assert_eq!(report.sized, 0, "tallied 3 minutes ago: not stale");
    assert_eq!(host.trace_count(Op::ReadDir), reads);
    // 70 minutes later both are listed again and tier a is sized again.
    host.sleep(Duration::from_secs(70 * 60));
    let report = synchronous(&host, &mut state, &tiers(), TMP_DEV, None).unwrap();
    assert_eq!((report.listed.len(), report.sized), (2, 3));
}

/// The first tier is the earliest with a discovered candidate.
#[test]
fn sync_tally_sizes_the_first_tier_that_found_something() {
    let host = host(&yaml(&dirs("b", 2, 1), "  /tmp/a: {.: {}}\n"));
    let mut state = state();
    let report = synchronous(&host, &mut state, &tiers(), TMP_DEV, None).unwrap();
    assert_eq!((report.first_tier, report.sized), (Some(1), 2));
}

/// A budget that ends in pass 1 leaves the tally not useful and lists
/// nothing more; one that ends in pass 2 leaves stale units untallied
/// (docs/testing.md `sync_tally_budget_ending_in_discovery_still_finds_the_rest`
/// is the reclaim walk's, TASK-31).
#[test]
fn sync_tally_budget_ends_in_either_pass() {
    let host = sync_model("call_cost: 1ms\n");
    let report = synchronous(&host, &mut state(), &tiers(), TMP_DEV, Some(host.now())).unwrap();
    assert!(!report.pass1_done && report.listed.is_empty(), "{report:?}");

    // Pass 1's cost, measured on a copy of the model.
    let probe = sync_model("call_cost: 1ms\n");
    let mut probe_state = state();
    for tier in tiers() {
        for source in &tier.sources {
            probe_state.list_source(&probe, source, None).unwrap();
        }
    }
    let pass1 = probe.elapsed();
    let host = sync_model("call_cost: 1ms\n");
    let deadline = MonoTime(host.now().0 + pass1 + Duration::from_millis(15));
    let report = synchronous(&host, &mut state(), &tiers(), TMP_DEV, Some(deadline)).unwrap();
    assert!(report.pass1_done, "{report:?}");
    assert!(report.untallied > 0 && report.sized < 3, "{report:?}");
    assert!(report.stopped.is_some(), "{report:?}");
}

// ---- Recount ----

/// Mark's rule (2026-10-25): a complete listing of a block's site recounts
/// its members and estimates; a partial one leaves them.
#[test]
fn a_block_is_recounted_on_a_complete_listing_only() {
    let mut tree = String::new();
    for i in 0..40 {
        writeln!(tree, "  /tmp/a/nix-shell.ab{i:03}x: {{rc: {{size: 4K}}}}").unwrap();
    }
    let mut events = String::from("events:\n");
    for i in 0..10 {
        writeln!(
            events,
            "  - {{at: {{elapsed: 10s}}, remove: /tmp/a/nix-shell.ab{i:03}x}}"
        )
        .unwrap();
    }
    let host = host(&yaml(&tree, &events));
    let source = source("a", "/tmp/a");
    // Collapse all 40 into one family block: an index with room for
    // about 30 singles.
    let mut sizing = state();
    sizing.list_source(&host, &source, None).unwrap();
    let per_single = sizing.index.accounted_bytes() / 40;
    let mut state = TallyState::new(Index::new(per_single * 30));
    let first = state.list_source(&host, &source, None).unwrap();
    assert!(first.covered > 0, "{first:?}");
    // The candidates the new block covered were not counted (TASK-28):
    // the listing was complete, so its recount counts them.
    let short = state.index.stats();
    assert!(short.singles as u64 + short.members < 40, "{short:?}");
    state.recount(&[Complete {
        finder: &source.finder,
        listed: &first.listing.listed,
        unindexed: &first.listing.unindexed,
    }]);
    let block = |state: &TallyState| {
        let mut found = Vec::new();
        state.index.for_each_unit(|_, _, unit| {
            if let UnitRef::Block(b) = unit {
                found.push(b.clone());
            }
        });
        assert_eq!(found.len(), 1, "{:?}", state.index.stats());
        found.pop().unwrap()
    };
    let before = block(&state);
    assert!(matches!(before.family, Family::Name(_)));
    let singles_before = state.index.stats().singles as u64;
    assert_eq!(before.members + singles_before, 40);
    // Ten go. A listing stopped by its deadline is partial: no recount.
    host.sleep(Duration::from_secs(11));
    let partial = state.list_source(&host, &source, Some(host.now())).unwrap();
    assert!(!partial.listing.complete);
    assert_eq!(block(&state), before);
    // A complete one recounts the block (or dissolves it, if it fits).
    let complete = state.list_source(&host, &source, None).unwrap();
    assert!(complete.listing.complete);
    let recount = state.recount(&[Complete {
        finder: &source.finder,
        listed: &complete.listing.listed,
        unindexed: &complete.listing.unindexed,
    }]);
    let stats = state.index.stats();
    assert_eq!(
        stats.singles as u64 + stats.members,
        30,
        "{recount:?} {stats:?}"
    );
    // 30 singles do not fit under the low-water mark of a budget of about
    // 30: the block is recounted, not dissolved.
    assert_eq!((recount.blocks_recounted, recount.blocks_dissolved), (1, 0));
    let after = block(&state);
    assert_eq!(after.members, before.members - 10, "{after:?}");
}

/// Two-level sources: candidates `*/*` below `/tmp/a`, in `grp` (40 rc
/// dirs, collapsed into a block by a budget of about 30 singles) and in
/// `locked`, whose listing fails (EACCES) on the given listings of it:
/// those listings are partial.
fn recount_model(failing_listings: &[u32]) -> ModelHost {
    let mut tree = String::from("  /tmp/a/locked/one: {f: {size: 4K}}\n");
    for i in 0..40 {
        writeln!(
            tree,
            "  /tmp/a/grp/nix-shell.ab{i:03}x: {{rc: {{size: 4K}}}}"
        )
        .unwrap();
    }
    let mut events = String::from("events:\n");
    for i in 0..10 {
        writeln!(
            events,
            "  - {{at: {{elapsed: 6m}}, remove: /tmp/a/grp/nix-shell.ab{i:03}x}}"
        )
        .unwrap();
    }
    for nth in failing_listings {
        writeln!(
            events,
            "  - {{at: {{op: read_dir, path: /tmp/a/locked, nth: {nth}, when: before}}, fail: EACCES}}"
        )
        .unwrap();
    }
    host(&yaml(&tree, &events))
}

fn two_level(profile: &str) -> TallySource {
    TallySource {
        finder: finder(profile, "/tmp/a"),
        find: SourceFind::Fs(Box::new(Find::new(
            &["*/*".to_owned()],
            EntryType::Dir,
            &Where::default(),
            1000,
        ))),
    }
}

/// The members a block holds plus the singles: what the index counts.
fn counted(state: &TallyState) -> u64 {
    let stats = state.index.stats();
    stats.singles as u64 + stats.members
}

/// A state whose budget holds about 30 of the 41 candidates as singles
/// (measured on a copy of the model, so the model's listings are not used).
fn small_state() -> TallyState {
    let copy = recount_model(&[]);
    let mut sizing = state();
    sizing.list_source(&copy, &two_level("a"), None).unwrap();
    TallyState::new(Index::new(sizing.index.accounted_bytes() * 30 / 41))
}

/// Mark's rule through the synchronous tally: its pass-1 round recounts on
/// complete listings only. Run 1 (complete) counts all 41; run 2, after ten
/// members went, has a partial listing and leaves the count; run 3
/// (complete) recounts it to 31.
#[test]
fn sync_tally_recounts_blocks_on_complete_listings_only() {
    let host = recount_model(&[2]);
    let mut state = small_state();
    let tier = [SyncTier {
        sources: vec![two_level("a")],
        order: Order::Oldest,
    }];
    let run = |state: &mut TallyState| synchronous(&host, state, &tier, TMP_DEV, None).unwrap();
    let first = run(&mut state);
    assert!(state.index.stats().blocks >= 1, "{:?}", state.index.stats());
    assert_eq!(counted(&state), 41, "{first:?}");
    host.sleep(Duration::from_secs(7 * 60));
    let second = run(&mut state);
    assert_eq!(
        second.recount,
        Default::default(),
        "a partial listing recounted"
    );
    assert_eq!(counted(&state), 41);
    host.sleep(Duration::from_secs(6 * 60));
    let third = run(&mut state);
    assert!(third.recount.blocks_recounted + third.recount.blocks_dissolved >= 1);
    assert_eq!(counted(&state), 31, "{third:?}");
}

/// The same through the background tally: a listing recounts after it only
/// when it was complete. Listings at 0, 5, 10 and 15 minutes; the second
/// and third are partial; ten members go at 6 minutes.
#[test]
fn background_tally_recounts_blocks_on_complete_listings_only() {
    let host = recount_model(&[2, 3]);
    let mut state = small_state();
    let sources = [two_level("a")];
    let mut events = Vec::new();
    let mut log = |e: Event| events.push(e);
    let mut background = Background::start(&host, &mut log);
    let mut counts = Vec::new();
    while at(&host) < Duration::from_secs(11 * 60) {
        let listed = state.listed_at(&sources[0].finder);
        let sleep = background.turn(&host, &mut state, &sources, &mut log);
        if state.listed_at(&sources[0].finder) != listed {
            counts.push(counted(&state));
        }
        host.sleep(sleep);
    }
    // Round 1 collapsed while it ran (the budget holds about 30 of 41),
    // so it left its blocks' counts (members it covered after the
    // collapse are not counted, TASK-28); rounds 2 and 3 are partial and
    // must leave them too.
    assert_eq!(counts.len(), 3, "{counts:?}");
    assert!(
        counts.iter().all(|c| *c == counts[0]),
        "a partial listing recounted: {counts:?}"
    );
    while at(&host) < Duration::from_secs(16 * 60) {
        let sleep = background.turn(&host, &mut state, &sources, &mut log);
        host.sleep(sleep);
    }
    assert_eq!(counted(&state), 31);
}

/// Sizing a unit whose cursor path is so deep that reopening it takes a
/// whole batch (2 ms a call, 30 levels) still ends: a stalled step makes
/// the next one do some entries whatever its bounds.
#[test]
fn a_sizing_walk_whose_reopening_takes_a_batch_still_ends() {
    let mut deep = String::from("/tmp/a/deep");
    for level in 0..30 {
        write!(deep, "/l{level}").unwrap();
    }
    let mut tree = String::new();
    for f in 0..40 {
        writeln!(tree, "  {deep}/f{f}: {{size: 4K}}").unwrap();
    }
    let host = host(&yaml(&tree, "call_cost: 2ms\n"));
    let mut state = state();
    state
        .list_source(&host, &source("a", "/tmp/a"), None)
        .unwrap();
    let (mount, path) = (tmp_mount(), Path::new("/tmp/a/deep"));
    let mut steps = Vec::new();
    while steps.last() != Some(&Sizing::Done) {
        let mounts = host.mountinfo().unwrap();
        let mut budget = Budget::batch(host.now());
        steps.push(
            state
                .size_unit(&host, &mounts, &mount, path, &mut budget)
                .unwrap(),
        );
        host.sleep(Duration::from_secs(1));
        assert!(
            steps.len() < 400,
            "the walk does not end: {} steps",
            steps.len()
        );
    }
    assert_eq!(
        state
            .index
            .single(&mount, path)
            .unwrap()
            .tally
            .unwrap()
            .files,
        31 + 40
    );
}

/// A failure of the host (EIO on every listing below `/tmp/a`) is not
/// retried every batch: the failing jobs wait, the batches back off to
/// 60 s, and the failure is logged once.
#[test]
fn a_failing_host_is_not_retried_every_batch() {
    let mut events = String::from("call_cost: 1ms\nevents:\n");
    for nth in 1..400 {
        writeln!(
            events,
            "  - {{at: {{op: read_dir, path: /tmp/a/d000, nth: {nth}, when: before}}, fail: EIO}}"
        )
        .unwrap();
    }
    let host = host(&yaml(&dirs("a", 1, 3), &events));
    let mut state = state();
    let mut log = Vec::new();
    let turns = run_turns(&host, &mut state, &[source("a", "/tmp/a")], 60, &mut log);
    let failed = log.iter().filter(|e| matches!(e, Event::Failed(_))).count();
    assert_eq!(failed, 1, "{log:?}");
    let attempts = host
        .trace()
        .iter()
        .filter(|c| c.op == Op::ReadDir && c.path.as_deref() == Some(Path::new("/tmp/a/d000")))
        .count();
    let elapsed = host.elapsed().as_secs();
    // One attempt per re-check period (10 min) at most, plus the first.
    assert!(
        attempts as u64 <= 1 + elapsed / 600,
        "{attempts} attempts in {elapsed} s"
    );
    assert!(turns.iter().all(|t| {
        t.batch
            .is_none_or(|b| b.failed == 0 || t.sleep >= Duration::from_secs(1))
    }));
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig { cases: 48, ..Default::default() })]

    /// Cut into steps of any size, a sizing walk comes to exactly what one
    /// uninterrupted walk does: no entry counted twice or missed at a
    /// pause point.
    #[test]
    fn a_walk_cut_into_steps_of_any_size_sizes_the_same(
        shape in proptest::collection::vec((0usize..4, 0usize..5), 1..12),
        ops in 1usize..40,
    ) {
        let mut tree = String::new();
        for (i, (depth, files)) in shape.iter().enumerate() {
            let mut dir = format!("/tmp/a/u/s{i}");
            for level in 0..*depth {
                write!(dir, "/n{level}").unwrap();
            }
            writeln!(tree, "  {dir}/x: {{size: 4K}}").unwrap();
            for f in 0..*files {
                writeln!(tree, "  {dir}/f{f}: {{size: 8K}}").unwrap();
            }
        }
        let host = host(&yaml(&tree, ""));
        let (mount, path) = (tmp_mount(), Path::new("/tmp/a/u"));
        let mounts = host.mountinfo().unwrap();
        let mut whole = state();
        whole.list_source(&host, &source("a", "/tmp/a"), None).unwrap();
        let mut budget = Budget::until(None);
        proptest::prop_assert_eq!(
            whole.size_unit(&host, &mounts, &mount, path, &mut budget).unwrap(),
            Sizing::Done
        );
        let mut cut = state();
        cut.list_source(&host, &source("a", "/tmp/a"), None).unwrap();
        let mut steps = 0;
        loop {
            let mut budget = Budget::ops_only(ops);
            match cut.size_unit(&host, &mounts, &mount, path, &mut budget).unwrap() {
                Sizing::Paused => {}
                Sizing::Done => break,
                other => proptest::prop_assert!(false, "{other:?}"),
            }
            steps += 1;
            proptest::prop_assert!(steps < 10_000);
        }
        let tally = |s: &TallyState| {
            let t = s.index.single(&mount, path).unwrap().tally.unwrap();
            (t.est_bytes, t.files)
        };
        proptest::prop_assert_eq!(tally(&cut), tally(&whole));
    }
}

/// A path that holds a single (found by two profiles) beneath a block of
/// one of them is that single, never also one of the block's members: a
/// recount counts it once.
#[test]
fn a_recount_never_counts_a_single_as_a_block_member_too() {
    let host = recount_model(&[]);
    let mut state = small_state();
    let one = TallySource {
        finder: finder("b", "/tmp/a"),
        find: SourceFind::Fs(Box::new(Find::new(
            &["grp/nix-shell.ab000x".to_owned()],
            EntryType::Dir,
            &Where::default(),
            1000,
        ))),
    };
    let a = two_level("a");
    for source in [&one, &a] {
        let listed = state.list_source(&host, source, None).unwrap();
        assert!(listed.listing.complete);
        state.recount(&[Complete {
            finder: &source.finder,
            listed: &listed.listing.listed,
            unindexed: &listed.listing.unindexed,
        }]);
    }
    let x = Path::new("/tmp/a/grp/nix-shell.ab000x");
    let single = state
        .index
        .single(&tmp_mount(), x)
        .expect("x stays a single");
    assert_eq!(state.index.finder_set(single.finders()).len(), 2);
    assert!(state.index.stats().blocks >= 1, "{:?}", state.index.stats());
    assert_eq!(counted(&state), 41, "{:?}", state.index.stats());
}

// ---- Resumable discovery (Mark, 2026-10-25) ----

fn cargo_source(root: &str) -> TallySource {
    TallySource {
        finder: finder("cargo-target", root),
        find: SourceFind::CargoTarget,
    }
}

/// `n` Cargo targets below `/tmp/w`, at depths 1 to 3, among plain dirs.
fn cargo_tree(n: usize) -> String {
    let mut tree = String::new();
    for i in 0..n {
        let base = match i % 3 {
            0 => format!("/tmp/w/p{i}"),
            1 => format!("/tmp/w/g{}/p{i}", i % 5),
            _ => format!("/tmp/w/g{}/h/p{i}", i % 4),
        };
        writeln!(tree, "  {base}/Cargo.toml: {{}}").unwrap();
        writeln!(tree, "  {base}/src/main.rs: {{size: 1K}}").unwrap();
        writeln!(tree, "  {base}/target/CACHEDIR.TAG: {{cachedir_tag: true}}").unwrap();
        writeln!(tree, "  {base}/target/debug/.cargo-lock: {{}}").unwrap();
        writeln!(tree, "  {base}/target/debug/deps/x: {{size: 4K}}").unwrap();
    }
    tree
}

/// Runs a listing round of `source` to its end in steps of `ops`
/// operations; the paths found, and whether it was complete.
fn round_in_steps(
    host: &ModelHost,
    source: &TallySource,
    ops: usize,
) -> (Vec<PathBuf>, bool, usize) {
    let mut cursor = ListingCursor::new(source, host.now());
    let mut found = Vec::new();
    for steps in 1.. {
        let mut budget = Budget::ops_only(ops);
        let step = cursor.step(host, source, &mut budget).unwrap();
        assert_eq!(host.open_fds(), 0, "an fd held between steps");
        found.extend(step.listed.into_iter().map(|l| l.path));
        found.extend(step.unindexed);
        if step.done {
            found.sort();
            return (found, step.complete, steps);
        }
        assert!(steps < 100_000, "the round does not end");
    }
    unreachable!()
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig { cases: 32, ..Default::default() })]

    /// Cut into steps of any size, a listing round finds exactly what one
    /// uninterrupted discovery does, for both kinds, and is complete.
    #[test]
    fn a_listing_round_in_steps_of_any_size_finds_what_discovery_finds(
        dirs_n in 1usize..40,
        targets in 1usize..12,
        ops in 1usize..30,
    ) {
        let tree = format!("{}{}", dirs("a", dirs_n, 1), cargo_tree(targets));
        let host = host(&yaml(&tree, ""));
        for source in [source("a", "/tmp/a"), cargo_source("/tmp/w")] {
            let mut whole = state();
            let listed = whole.list_source(&host, &source, None).unwrap();
            let mut expected: Vec<PathBuf> = listed
                .listing
                .listed
                .iter()
                .map(|l| l.path.clone())
                .chain(listed.listing.unindexed.iter().cloned())
                .collect();
            expected.sort();
            proptest::prop_assert!(listed.listing.complete);
            let (found, complete, steps) = round_in_steps(&host, &source, ops);
            proptest::prop_assert!(complete);
            proptest::prop_assert_eq!(&found, &expected, "{} steps", steps);
            proptest::prop_assert!(!expected.is_empty());
        }
    }
}

/// A flat root of 3,000 candidates is listed by the background tally across
/// many batches, each within its bounds, and the round ends complete with
/// every candidate in the index.
#[test]
fn a_huge_root_is_listed_across_many_small_batches() {
    let mut tree = String::new();
    for i in 0..3000 {
        writeln!(tree, "  /tmp/a/c{i:05}: {{.: {{}}}}").unwrap();
    }
    let host = host(&yaml(&tree, "call_cost: 1ms\n"));
    let sources = [source("a", "/tmp/a")];
    let mut state = state();
    let mut events = Vec::new();
    let mut log = |e: Event| events.push(e);
    let mut background = Background::start(&host, &mut log);
    let overshoot = Duration::from_millis(8 + 32);
    let mut batches = 0;
    while state.listed_at(&sources[0].finder).is_none() {
        let before = at(&host);
        let sleep = background.turn(&host, &mut state, &sources, &mut log);
        assert!(
            at(&host) - before <= BATCH_TIME_MAX + overshoot,
            "a batch of {:?}",
            at(&host) - before
        );
        assert_eq!(host.open_fds(), 0);
        batches += 1;
        host.sleep(sleep);
        assert!(batches < 10_000);
    }
    assert!(batches >= 40, "listed in {batches} batches");
    assert_eq!(state.index.stats().singles, 3000);
    assert!(
        state.discovered_at(&sources[0].finder).is_some(),
        "complete"
    );
}

/// Entries changing under a live round: a candidate removed and one created
/// after their directory was listed, a directory on the way down replaced.
/// The round stays sound (no panic, no fd held, what it reports exists or
/// existed) and a replaced directory makes it incomplete: nothing evicted.
#[test]
fn a_round_survives_entries_changing_under_it_and_a_replacement_makes_it_partial() {
    let mut tree = String::new();
    for g in 0..4 {
        for i in 0..40 {
            writeln!(tree, "  /tmp/a/g{g}/c{i:03}: {{.: {{}}}}").unwrap();
        }
    }
    // At 1 ms a call: the round's first step asks the directories above
    // the root for container storage (`/` and the `/tmp` mount, about
    // 30 calls, TASK-242), so g0 is entered and listed at about 30 ms,
    // g1 at about 75 ms, g2 at about 120 ms.
    let events = "events:\n\
        - {at: {elapsed: 50ms}, replace: /tmp/a/g0}\n\
        - {at: {elapsed: 50ms}, remove: /tmp/a/g2}\n\
        - {at: {elapsed: 100ms}, remove: /tmp/a/g1/c039}\n\
        - {at: {elapsed: 100ms}, mkdir: /tmp/a/g1/c040}\n";
    let host = host(&yaml(
        &tree,
        &format!("call_cost: 1ms\n{}", events.replace("\n-", "\n  -")),
    ));
    let source = two_level("a");
    let mut cursor = ListingCursor::new(&source, host.now());
    let mut found = Vec::new();
    loop {
        let mut budget = Budget::ops_only(10);
        let step = cursor.step(&host, &source, &mut budget).unwrap();
        assert_eq!(host.open_fds(), 0);
        found.extend(step.listed.into_iter().map(|l| l.path));
        if step.done {
            assert!(!step.complete, "g0 was replaced under the round");
            break;
        }
    }
    assert!(
        !found.iter().any(|p| p.starts_with("/tmp/a/g2")),
        "g2 vanished before it was reached"
    );
    assert!(
        found.contains(&PathBuf::from("/tmp/a/g0/c000")),
        "found before the replacement"
    );
    assert!(
        !found.contains(&PathBuf::from("/tmp/a/g0/c039")),
        "after the replacement: not walked on"
    );
    // g1 was listed before c039 went and c040 came: the removed one is
    // statted, gone, skipped; the new one is not seen by this round.
    assert!(!found.contains(&PathBuf::from("/tmp/a/g1/c039")));
    assert!(!found.contains(&PathBuf::from("/tmp/a/g1/c040")));
    assert!(found.contains(&PathBuf::from("/tmp/a/g3/c039")));
}

/// Completeness spans the round: a candidate gone before the round is lost
/// only when every step of it was clean; a refusal in a later step (an
/// unreadable directory) makes the whole round partial.
#[test]
fn a_round_is_complete_only_if_every_step_was_clean() {
    for (fail, evicted) in [(false, true), (true, false)] {
        let mut tree = String::new();
        for g in 0..3 {
            for i in 0..30 {
                writeln!(tree, "  /tmp/a/g{g}/c{i:03}: {{.: {{}}}}").unwrap();
            }
        }
        let mut extra = String::from(
            "call_cost: 1ms\nevents:\n  - {at: {elapsed: 10s}, remove: /tmp/a/g0/c000}\n",
        );
        if fail {
            extra.push_str(
                "  - {at: {op: read_dir, path: /tmp/a/g2, nth: 2, when: before}, fail: EACCES}\n",
            );
        }
        let host = host(&yaml(&tree, &extra));
        let source = two_level("a");
        let mut state = state();
        state.list_source(&host, &source, None).unwrap(); // round 0, at once
        host.sleep(Duration::from_secs(11));
        let gone = Path::new("/tmp/a/g0/c000");
        assert!(state.index.single(&tmp_mount(), gone).is_some());
        let outcome = loop {
            let mut budget = Budget::ops_only(15);
            match state.list_round(&host, &source, &mut budget).unwrap() {
                Rounded::Paused => {}
                other => break other,
            }
        };
        assert!(
            matches!(outcome, Rounded::Done { complete, .. } if complete == !fail),
            "{outcome:?}"
        );
        assert_eq!(
            state.index.single(&tmp_mount(), gone).is_none(),
            evicted,
            "fail {fail}"
        );
    }
}

/// A listing round whose kept names do not fit the index's budget is
/// dropped (partial) and never makes the index collapse on its behalf.
#[test]
fn a_round_that_does_not_fit_never_collapses_the_index() {
    let mut tree = dirs("a", 30, 1);
    for i in 0..400 {
        writeln!(
            tree,
            "  /tmp/b/x{i:04}-a-rather-long-name-to-keep: {{.: {{}}}}"
        )
        .unwrap();
    }
    let host = host(&yaml(&tree, ""));
    let a = source("a", "/tmp/a");
    let b = source("b", "/tmp/b");
    // An index just big enough for /tmp/a's 30 singles.
    let mut sizing = state();
    sizing.list_source(&host, &a, None).unwrap();
    let mut state = TallyState::new(Index::new(sizing.index.accounted_bytes() + 2048));
    state.list_source(&host, &a, None).unwrap();
    let before = state.index.stats();
    let mut budget = Budget::ops_only(5);
    let outcome = state.list_round(&host, &b, &mut budget).unwrap();
    assert_eq!(outcome, Rounded::NoRoom);
    let after = state.index.stats();
    assert_eq!(
        (after.singles, after.blocks),
        (before.singles, before.blocks),
        "collapsed for a round"
    );
    assert!(state.round_finder().is_none());
    assert!(state.listed_at(&b.finder).is_some(), "waits its period");
}

/// A round ended by a job redirected to it is not started again by its own
/// job in the same batch: a source is listed at most once per period.
#[test]
fn a_source_is_not_listed_twice_in_one_period() {
    let mut tree = dirs("a", 60, 1);
    tree.push_str(&dirs("b", 60, 1));
    let host = host(&yaml(&tree, "call_cost: 1ms\n"));
    let sources = [source("a", "/tmp/a"), source("b", "/tmp/b")];
    let mut state = state();
    let mut events = Vec::new();
    let mut log = |e: Event| events.push(e);
    let mut background = Background::start(&host, &mut log);
    let mut rounds: Vec<(usize, Duration)> = Vec::new();
    while at(&host) < Duration::from_secs(12 * 60) {
        let listed: Vec<_> = sources.iter().map(|s| state.listed_at(&s.finder)).collect();
        let sleep = background.turn(&host, &mut state, &sources, &mut log);
        for (i, s) in sources.iter().enumerate() {
            if state.listed_at(&s.finder) != listed[i] {
                rounds.push((i, at(&host)));
            }
        }
        host.sleep(sleep);
    }
    for i in 0..2 {
        let times: Vec<Duration> = rounds.iter().filter(|r| r.0 == i).map(|r| r.1).collect();
        for pair in times.windows(2) {
            assert!(
                pair[1] - pair[0] >= Duration::from_secs(5 * 60),
                "source {i}: {times:?}"
            );
        }
        assert!(times.len() >= 2, "source {i}: {times:?}");
    }
}

/// The redirect case: a round of A is live, B is more overdue, so B's job
/// comes first and finishes A's round; A's own job in the same batch must
/// not start a new round of A at once.
#[test]
fn a_redirected_job_that_ends_a_round_does_not_restart_it() {
    let host = host(&yaml(
        &format!("{}{}", dirs("a", 3, 1), dirs("b", 3, 1)),
        "",
    ));
    let sources = [source("a", "/tmp/a"), source("b", "/tmp/b")];
    let mut state = state();
    let mut events = Vec::new();
    let mut log = |e: Event| events.push(e);
    let mut background = Background::start(&host, &mut log);
    // B listed at 0, A at 1 min (whole, as a snapshot seed would leave
    // them).
    background.turn(&host, &mut state, &sources[1..], &mut log);
    host.sleep(Duration::from_secs(60));
    background.turn(&host, &mut state, &sources[..1], &mut log);
    let a_first = state.listed_at(&sources[0].finder).unwrap();
    // At 6 min 10 s a round of A starts and pauses at once.
    host.sleep(Duration::from_secs(5 * 60 + 10));
    let mut budget = Budget::ops_only(0);
    assert_eq!(
        state.list_round(&host, &sources[0], &mut budget).unwrap(),
        Rounded::Paused
    );
    host.sleep(Duration::from_secs(10));
    // B is 1 min 20 s late, A 20 s: B's job comes first and ends A's round.
    let calls = host.trace().len();
    background.turn(&host, &mut state, &sources, &mut log);
    let listings_of_a = host.trace()[calls..]
        .iter()
        .filter(|c| c.op == Op::ReadDir && c.path.as_deref() == Some(Path::new("/tmp/a")))
        .count();
    assert_eq!(listings_of_a, 1, "A listed twice in one batch");
    let a_second = state.listed_at(&sources[0].finder).unwrap();
    assert!(a_second > a_first, "A's round ended");
    assert!(
        state.round_finder().is_none(),
        "no new round of A started: {:?}",
        state.round_finder()
    );
}

/// TASK-242: container storage is excluded on purpose, never unread. A
/// listing whose walk meets a storage root (and refuses it) is still
/// complete, so its recount evicts what the index held inside storage.
/// Both kinds, in one call and in steps.
#[test]
fn a_listing_that_meets_container_storage_is_still_complete() {
    let mut tree = cargo_tree(2);
    // A rootless graphroot below the Cargo root, a target in a layer.
    tree.push_str(
        "  /tmp/w/store/storage.lock: {}\n  /tmp/w/store/overlay-layers: {.: {}}\n  \
         /tmp/w/store/overlay/l1/target/CACHEDIR.TAG: {cachedir_tag: true}\n  \
         /tmp/w/store/overlay/l1/target/debug/.cargo-lock: {}\n",
    );
    // An fs root that is a separate image store itself.
    tree.push_str(
        "  /tmp/a/overlay-layers: {.: {}}\n  /tmp/a/overlay-images: {.: {}}\n  \
         /tmp/a/overlay/l1: {.: {}}\n",
    );
    let host = host(&yaml(&tree, ""));
    for (source, expected) in [(cargo_source("/tmp/w"), 2), (source("a", "/tmp/a"), 0)] {
        let listed = state().list_source(&host, &source, None).unwrap();
        assert!(listed.listing.complete, "{:?}", source.finder);
        assert_eq!(listed.listing.listed.len(), expected, "{:?}", source.finder);
        assert!(
            listed
                .listing
                .listed
                .iter()
                .all(|l| !l.path.starts_with("/tmp/w/store")),
            "nothing inside storage is listed"
        );
        for ops in [1, 7, 1000] {
            let (found, complete, _) = round_in_steps(&host, &source, ops);
            assert!(complete, "{:?} in steps of {ops}", source.finder);
            assert_eq!(found.len(), expected);
        }
    }
}
