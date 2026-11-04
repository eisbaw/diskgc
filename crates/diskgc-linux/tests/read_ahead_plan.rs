//! TASK-325: the plan's read-ahead ([`diskgc_core::host::Host::read_ahead`],
//! `diskgc_linux::read_ahead`) changes no verdict. On the real kernel, the
//! same read-only dry run over generated trees, by a host that reads ahead
//! and by one that does not, plans the same candidates with the same
//! reasons and sizes: one source (`fs_kind::plan`) and a union of two
//! (`plan_fs_union`), with more candidates than the read-ahead distance.
//!
//! Read-only: nothing is deleted (a dry run), and the trees are this
//! test's own, below the test temp dir (removed on drop). The model's
//! read-ahead is a no-op, so this equality cannot be shown there.
//!
//! The in-use scan sees diskgc's own process alone ([`OwnProcessOnly`]):
//! what other processes do is no part of the read-ahead, and two scans of
//! a busy host differ. Under load one plan's scan stayed unsettled
//! (`in_use_unknown`, cause `busy`) while the other's settled, so the
//! plans differed whatever the read-ahead did (TASK-337). The kernel still
//! answers every filesystem call and the reads of diskgc's own process:
//! were the scan to stop passing over that process, the readers' open
//! directories (its threads') would show as a use. A reader moved into
//! another process would not be seen here.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use diskgc_core::engine::Reason;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::fs_kind::READ_AHEAD;
use diskgc_core::profile::{Library, parse_profiles};
use diskgc_core::report::Status;
use diskgc_core::units::parse_size;
use diskgc_linux::LinuxHost;

use common::OwnProcessOnly;

const PROFILES: &str = r#"
- name: gen-a
  find: {roots: required, paths: ["a-*"], type: dir}
  action: delete
  examples: {match: [/x/a-1]}
- name: gen-ab
  find: {roots: required, paths: ["a-*", "b-*"], type: dir}
  action: delete
  examples: {match: [/x/a-1, /x/b-1]}
"#;

/// A fresh directory under the test temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(seed: u64) -> Self {
        let dir = std::env::temp_dir().join(format!("dg-rap-{}-{seed}", std::process::id()));
        std::fs::create_dir(&dir).expect("create test dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// xorshift64*: a reproducible tree per seed without a dependency.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    }
}

/// Fills `dir` with up to five entries of mixed kinds, nesting to `depth`.
/// Some kinds make the whole candidate a skip (a FIFO: `contains_ipc`; a
/// keep marker: `kept`; a `.git`: `contains_checkout`; a file dated a day
/// ahead: `future_time`), so plans mix verdicts.
fn fill(dir: &Path, depth: u32, rng: &mut Rng) {
    for i in 0..rng.below(6) {
        let path = dir.join(format!("e{i}"));
        match rng.below(if depth == 0 { 4 } else { 6 }) {
            0 | 1 => {
                let size = usize::try_from(rng.below(4096)).expect("small");
                std::fs::write(&path, vec![b'x'; size]).expect("write");
            }
            2 => std::os::unix::fs::symlink("../..", &path).expect("symlink"),
            3 => match rng.below(12) {
                0 => rustix::fs::mknodat(
                    rustix::fs::CWD,
                    &path,
                    rustix::fs::FileType::Fifo,
                    rustix::fs::Mode::from_raw_mode(0o600),
                    0,
                )
                .expect("mkfifo"),
                1 => std::fs::write(dir.join(".diskgc-keep"), b"").expect("keep"),
                2 => std::fs::create_dir_all(dir.join(".git")).expect("checkout"),
                3 => {
                    let file = std::fs::File::create(&path).expect("create");
                    let ahead = SystemTime::now() + Duration::from_secs(86_400);
                    file.set_modified(ahead).expect("future mtime");
                }
                _ => std::fs::write(&path, b"y").expect("write"),
            },
            _ => {
                std::fs::create_dir(&path).expect("mkdir");
                fill(&path, depth - 1, rng);
            }
        }
    }
}

/// A generated tree: `a-*` and `b-*` candidates, more `a-*` alone than
/// the read-ahead distance, each filled by [`fill`].
fn tree(root: &Path, seed: u64) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let candidates = 2 * READ_AHEAD as u64 + 10 + rng.below(40);
    for n in 0..candidates {
        let name = if rng.below(3) == 0 { "b" } else { "a" };
        let dir = root.join(format!("{name}-{n}"));
        std::fs::create_dir(&dir).expect("mkdir");
        fill(&dir, 3, &mut rng);
    }
}

/// The plan of `select` over `root` by `host`, as comparable rows:
/// candidate, profile, reason, size, operation, checkouts.
fn plan(host: &OwnProcessOnly, select: &str, root: &Path) -> Vec<String> {
    let specs = parse_profiles(PROFILES).unwrap_or_else(|e| panic!("{e}"));
    let library = Library::with_user_profiles(&specs).unwrap_or_else(|e| panic!("{e}"));
    let uid = executor::privileges(host, false).unwrap_or_else(|e| panic!("{e:?}"));
    let roots = [root.display().to_string()];
    let request = Request {
        path: root.to_owned(),
        min_free: parse_size("100%").expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(120)),
        tally_budget: None,
        watch: None,
        tier: Tier::select(select, &library, Some(&roots), uid).unwrap_or_else(|e| panic!("{e}")),
    };
    let report = executor::dry_run(host, &request, &[uid], &mut |_| {});
    assert_eq!(
        report.status,
        Status::DryRun,
        "{select}: {:?}",
        report.errors
    );
    assert!(report.errors.is_empty(), "{select}: {:?}", report.errors);
    // Only diskgc's own process was scanned, and it never counts.
    let in_use: Vec<_> = report
        .plan
        .iter()
        .filter(|row| matches!(row.reason, Some(Reason::InUse | Reason::InUseUnknown)))
        .collect();
    assert!(in_use.is_empty(), "{select}: {in_use:#?}");
    report
        .plan
        .iter()
        .map(|row| {
            format!(
                "{:?} {} {:?} {:?} {} {:?}",
                row.candidate,
                row.profile,
                row.reason,
                row.estimated_bytes,
                row.operation,
                row.checkouts
            )
        })
        .collect()
}

/// Plans each seed's tree with and without the read-ahead (see the module
/// docs); returns the reasons seen (`None`: eligible).
fn same_plans(seeds: &[u64]) -> std::collections::BTreeSet<String> {
    let mut reasons = std::collections::BTreeSet::new();
    for &seed in seeds {
        let dir = TempDir::new(seed);
        tree(&dir.0, seed);
        for select in ["profile:gen-a", "profile:gen-a || profile:gen-ab"] {
            let without = OwnProcessOnly(LinuxHost::new().expect("host").without_read_ahead());
            let with = OwnProcessOnly(LinuxHost::new().expect("host"));
            // Alternate which runs first, so a cache the first run warmed
            // favours neither.
            let (ahead, plain) = if seed % 2 == 0 {
                let ahead = plan(&with, select, &dir.0);
                (ahead, plan(&without, select, &dir.0))
            } else {
                let plain = plan(&without, select, &dir.0);
                (plan(&with, select, &dir.0), plain)
            };
            assert_eq!(ahead, plain, "seed {seed}, `{select}`");
            assert!(
                ahead.len() > READ_AHEAD,
                "seed {seed}: {} rows",
                ahead.len()
            );
            let counts = with.0.read_ahead_counts().expect("the plan read ahead");
            assert_eq!(
                counts.queued.load(Ordering::Relaxed)
                    + counts.unreached.load(Ordering::Relaxed)
                    + counts.queue_full.load(Ordering::Relaxed),
                u64::try_from(ahead.len()).expect("small"),
                "seed {seed}, `{select}`: every candidate named once"
            );
            assert!(counts.queued.load(Ordering::Relaxed) > 0, "seed {seed}");
            assert!(without.0.read_ahead_counts().is_none(), "seed {seed}");
            for row in &ahead {
                reasons.insert(row.split(' ').nth(2).expect("a reason").to_owned());
            }
        }
    }
    reasons
}

/// The trees of these seeds exercise eligible candidates and several kinds
/// of skip; three tests, so that they run side by side.
fn check(seeds: &[u64]) {
    // The in-use check refuses where it cannot see every process (a
    // private PID namespace, as in the Nix build sandbox): no plan there.
    let host = LinuxHost::new().expect("host");
    if let Err(why) = executor::process_visibility(&host) {
        eprintln!("skipped: {}", why.message);
        return;
    }
    let reasons = same_plans(seeds);
    assert!(reasons.contains("None"), "{reasons:?}");
    assert!(reasons.len() >= 3, "{reasons:?}");
}

#[test]
fn reading_ahead_changes_no_verdict_of_a_plan_seeds_1_2() {
    check(&[1, 2]);
}

#[test]
fn reading_ahead_changes_no_verdict_of_a_plan_seeds_3_4() {
    check(&[3, 4]);
}

#[test]
fn reading_ahead_changes_no_verdict_of_a_plan_seeds_5_6() {
    check(&[5, 6]);
}
