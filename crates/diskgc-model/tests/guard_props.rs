//! TASK-367: the nested-build guard may only add protection (Mark,
//! 2026-11-09, after three Codex reviews found filtering, union and
//! reporting paths that lost one). Over generated trees (a CMake build
//! with stamps of Buildroot or OpenWrt at chosen levels, a Buildroot
//! output marker, CMake, Meson and Autotools builds nested in it, a Git
//! checkout or a keep marker, a pytest session that is also a CMake
//! output), generated root sets (nested, overlapping, one on a filesystem
//! of its own, either one the `PATH`), unions of a guarded profile with
//! other profiles (Meson, Autotools, `nix-shell-rc`, `pytest-tmp`), both
//! catalogs, with and without the index snapshot (`--no-index`: the run
//! still tallies with an index of its own), and the stamp present from the start or
//! made by a host event during the run (Codex's fourth review: after
//! discovery, after the lock, during the locked walk), and one operation
//! failing once at its first to sixth call, whoever makes it: a listing
//! of the tree's top, the package or the build (Codex's fifth review), a
//! `statx`, read or open of the build's marker, or the open of its
//! anchor's source file (the sixth review):
//!
//! 1. **Only adds protection.** What the dry run plans eligible and what
//!    `survey` reports eligible are each a subset of the same on the same
//!    tree without the guard's signals (the
//!    stamps and the build-root marker, the only files the guard reads that
//!    nothing else does): the guard switched off by its inputs, no code
//!    seam in the product.
//!    The reclaim deletes in order and a deletion changes its parent's
//!    mtime, so what it removes is compared with what the run without the
//!    signals planned or took (at or below it), not with what that run
//!    happened to remove.
//! 2. **Independent of the view.** A build the guard's rule covers (a
//!    reference predicate over the generated layout, not diskgc's code,
//!    for the builds a guarded term of the selection finds) is never taken,
//!    planned eligible or reported eligible, whatever the roots, `PATH`,
//!    union or index.
//!
//! Cases: `DISKGC_PROPERTY_CASES` (default 128: at 48 the bites of the
//! union veto and of the widening survived the random test; at 128 both
//! fail it). A census of the failure classes: `--ignored`.

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use diskgc_core::invocation::{Catalog, ConfigFile, Platform};
use diskgc_core::profile::Library;
use diskgc_core::survey::{self, SurveyOptions};
use diskgc_model::profile_vector::ProfileVector;
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};
use serde_json::Value;

const OLD: &str = "2026-10-01T00:00:00Z";
const NEW: &str = "2026-10-15T02:00:00Z";

fn cases() -> u32 {
    std::env::var("DISKGC_PROPERTY_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StampAt {
    Top,
    Pkg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nested {
    None,
    CmakeA,
    MesonAB,
    AutotoolsA,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Root {
    Top,
    Pkg,
    OutA,
}

/// When the stamp exists: in the tree from the start, or made by a host
/// event during the run (Codex's fourth review): (a) after discovery,
/// before a judgement's anchor read (CMakeCache.txt's second read); (b)
/// right after the candidate lock; (c) during the locked contents walk
/// (after the build's third listing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Made {
    Before,
    AfterDiscovery,
    AfterLock,
    DuringWalk,
}

/// One operation that fails once (EACCES) at its `nth` call, whoever
/// makes it: a guarded term's discovery, `where`, anchor, tally or guard,
/// or another term's (Codex's fifth review; the sixth review's B1 for the
/// file operations). In both runs, with and without the signals: a
/// fault, not a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FaultAt {
    /// The listing of the tree's top, the package or the build.
    Top,
    Pkg,
    Out,
    /// The build's marker, `out/CMakeCache.txt`: its `statx` (a `where`),
    /// its read (the anchor) or its open (the anchor, the guard's probe).
    MarkerStatx,
    MarkerRead,
    MarkerOpen,
    /// The open of the anchor's source file, `pkg/CMakeLists.txt`.
    SourceOpen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Union {
    Cmake,
    Meson,
    Autotools,
    NixShellRc,
    PytestTmp,
}

/// One generated case: the layout `TOP/PKG/OUT` with its options.
#[derive(Debug, Clone)]
struct Scenario {
    top: &'static str,
    pkg: &'static str,
    out: &'static str,
    stamp: Option<(StampAt, &'static str)>,
    made: Made,
    fault: Option<(FaultAt, u8)>,
    br_marker: bool,
    out_recent: bool,
    nested: Nested,
    git_in_out: bool,
    keep_at_top: bool,
    roots: Vec<Root>,
    other_fs: bool,
    path_at_pkg: bool,
    union: Union,
    no_index: bool,
    release: bool,
    survey_top: bool,
    survey_pkg: bool,
}

impl Scenario {
    fn pkg_path(&self) -> String {
        format!("{}/{}", self.top, self.pkg)
    }
    fn out_path(&self) -> String {
        format!("{}/{}", self.pkg_path(), self.out)
    }
    fn inner_path(&self) -> Option<String> {
        match self.nested {
            Nested::None => None,
            Nested::CmakeA | Nested::AutotoolsA => Some(format!("{}/a/inner", self.out_path())),
            Nested::MesonAB => Some(format!("{}/a/b/inner", self.out_path())),
        }
    }
    fn tmp(&self) -> bool {
        self.top == "/tmp"
    }

    fn select(&self) -> String {
        let other = match self.union {
            Union::Cmake => return "profile:cmake-build-anchored".to_owned(),
            Union::Meson => "meson-build-anchored",
            Union::Autotools => "autotools-build-anchored",
            Union::NixShellRc => "nix-shell-rc",
            Union::PytestTmp => "pytest-tmp",
        };
        format!("profile:cmake-build-anchored || profile:{other}")
    }

    fn roots(&self) -> Vec<String> {
        let mut roots: Vec<String> = self
            .roots
            .iter()
            .filter_map(|root| match root {
                Root::Top => Some(self.top.to_owned()),
                Root::Pkg => Some(self.pkg_path()),
                Root::OutA => {
                    (self.nested != Nested::None).then(|| format!("{}/a", self.out_path()))
                }
            })
            .collect();
        roots.sort();
        roots.dedup();
        if roots.is_empty() {
            roots.push(self.top.to_owned());
        }
        roots
    }

    fn path(&self) -> String {
        if self.other_fs && self.path_at_pkg {
            self.pkg_path()
        } else {
            self.top.to_owned()
        }
    }

    /// The builds the guard covers in this run, by the guard's rule over
    /// this layout (docs/safety.md, Builds inside another build system's
    /// tree), computed from the layout: a build a guarded term of the
    /// selection finds (one to five levels below one of its roots, of a
    /// selected kind) with the stamp in its parent or the three
    /// directories above it (above `out` for the nested build too, a build
    /// directory it lies in), or the Buildroot output marker anywhere
    /// above. A build no guarded term finds is the other term's to judge.
    fn covered(&self) -> BTreeSet<PathBuf> {
        let out_levels_to_top = self.pkg.split('/').count();
        let stamp = match self.stamp {
            Some((StampAt::Pkg, _)) => true,
            Some((StampAt::Top, _)) => out_levels_to_top <= 3,
            None => false,
        };
        if !(stamp || self.br_marker) {
            return BTreeSet::new();
        }
        let roots = self.roots();
        let found = |build: &str| {
            roots.iter().any(|root| {
                std::path::Path::new(build)
                    .strip_prefix(root)
                    .is_ok_and(|rel| (1..=5).contains(&rel.components().count()))
            })
        };
        let inner_selected = match self.nested {
            Nested::None => false,
            Nested::CmakeA => true,
            Nested::MesonAB => self.union == Union::Meson,
            Nested::AutotoolsA => self.union == Union::Autotools,
        };
        let mut covered = BTreeSet::new();
        let out = self.out_path();
        if found(&out) {
            covered.insert(PathBuf::from(out));
        }
        if let Some(inner) = self
            .inner_path()
            .filter(|inner| inner_selected && found(inner))
        {
            covered.insert(PathBuf::from(inner));
        }
        covered
    }

    /// The host model's tree lines, with or without the guard's signals,
    /// and the paths whose removal the properties compare.
    fn tree(&self, signals: bool) -> (String, Vec<PathBuf>) {
        let mut lines = vec![
            "    /run/diskgc/lock: {uid: 0, gid: 0}".to_owned(),
            "    /usr/bin/bash: {}".to_owned(),
        ];
        let mut paths = Vec::new();
        let top = self.top;
        let pkg = self.pkg_path();
        let out = self.out_path();
        if signals
            && self.made == Made::Before
            && let Some((at, name)) = self.stamp
        {
            let dir = if at == StampAt::Top {
                top.to_owned()
            } else {
                pkg.clone()
            };
            lines.push(format!("    {dir}/{name}: {{mtime: {OLD}}}"));
        }
        if signals && self.br_marker {
            lines.push(format!(
                "    {top}/.br2-external.mk: {{mtime: {OLD}, content: \"BR2_EXTERNAL ?=\\n\"}}"
            ));
        }
        if self.keep_at_top {
            lines.push(format!("    {top}/.diskgc-keep: {{mtime: {OLD}}}"));
        }
        lines.push(format!("    {top}/notes.txt: {{mtime: {OLD}}}"));
        lines.push(format!(
            "    {pkg}/CMakeLists.txt: {{mtime: {OLD}, content: \"project(p)\\n\"}}"
        ));
        let m = if self.out_recent { NEW } else { OLD };
        lines.push(format!(
            "    {out}: {{.: {{mtime: {m}}}, CMakeCache.txt: {{mtime: {m}, content: \"CMAKE_HOME_DIRECTORY:INTERNAL={pkg}\\n\"}}, CMakeFiles: {{.: {{mtime: {m}}}}}, a.o: {{mtime: {m}, size: 8K}}}}"
        ));
        paths.push(PathBuf::from(&out));
        paths.push(PathBuf::from(format!("{out}/a.o")));
        if self.git_in_out {
            lines.push(format!("    {out}/test_repo0/.git/HEAD: {{mtime: {OLD}}}"));
            paths.push(PathBuf::from(format!("{out}/test_repo0/.git/HEAD")));
        }
        if self.pkg.ends_with("pytest-of-tester") {
            let session = format!("{pkg}/pytest-1");
            lines.push(format!(
                "    {session}: {{.: {{mtime: {OLD}}}, f: {{mtime: {OLD}}}}}"
            ));
            paths.push(PathBuf::from(session));
        }
        let src = format!("{out}/a");
        match self.nested {
            Nested::None => {}
            Nested::CmakeA => {
                let inner = format!("{src}/inner");
                lines.push(format!(
                    "    {src}/CMakeLists.txt: {{mtime: {OLD}, content: \"project(q)\\n\"}}"
                ));
                lines.push(format!(
                    "    {inner}: {{.: {{mtime: {OLD}}}, CMakeCache.txt: {{mtime: {OLD}, content: \"CMAKE_HOME_DIRECTORY:INTERNAL={src}\\n\"}}, CMakeFiles: {{.: {{mtime: {OLD}}}}}, i.o: {{mtime: {OLD}, size: 8K}}}}"
                ));
            }
            Nested::MesonAB => {
                let inner = format!("{src}/b/inner");
                let info = format!(
                    "{{\\\"directories\\\":{{\\\"source\\\":\\\"{src}\\\",\\\"build\\\":\\\"{inner}\\\",\\\"info\\\":\\\"{inner}/meson-info\\\"}},\\\"error\\\":false}}"
                );
                lines.push(format!(
                    "    {src}/meson.build: {{mtime: {OLD}, content: \"project('q')\\n\"}}"
                ));
                lines.push(format!(
                    "    {inner}: {{.: {{mtime: {OLD}}}, build.ninja: {{mtime: {OLD}}}, meson-private: {{.: {{mtime: {OLD}}}, coredata.dat: {{mtime: {OLD}, content: coredata}}}}, meson-info: {{.: {{mtime: {OLD}}}, meson-info.json: {{mtime: {OLD}, content: \"{info}\"}}}}, i.o: {{mtime: {OLD}, size: 8K}}}}"
                ));
            }
            Nested::AutotoolsA => {
                let inner = format!("{src}/inner");
                lines.push(format!(
                    "    {src}/configure.ac: {{mtime: {OLD}, content: \"AC_INIT([q],[1])\\n\"}}"
                ));
                lines.push(format!(
                    "    {inner}: {{.: {{mtime: {OLD}}}, config.status: {{mtime: {OLD}, content: \"ac_pwd='{inner}'\\nsrcdir='..'\\n\"}}, config.log: {{mtime: {OLD}}}, Makefile: {{mtime: {OLD}}}, i.o: {{mtime: {OLD}, size: 8K}}}}"
                ));
            }
        }
        if let Some(inner) = self.inner_path() {
            paths.push(PathBuf::from(&inner));
            paths.push(PathBuf::from(format!("{inner}/i.o")));
        }
        // Every directory a key implies is old too (a pytest session's
        // shell temp root, a checkout's `.git`, a nested build's source):
        // only what a case makes new is new.
        let keys: BTreeSet<PathBuf> = lines
            .iter()
            .filter_map(|line| {
                line.trim_start()
                    .split_once(": ")
                    .map(|(k, _)| PathBuf::from(k))
            })
            .collect();
        let mut implied = BTreeSet::new();
        for key in &keys {
            for dir in key.ancestors().skip(1) {
                if dir.starts_with(top) && !keys.contains(dir) {
                    implied.insert(dir.to_path_buf());
                }
            }
        }
        for dir in implied {
            lines.push(format!("    {}: {{.: {{mtime: {OLD}}}}}", dir.display()));
        }
        (lines.join("\n"), paths)
    }

    fn host_yaml(&self, signals: bool) -> (String, Vec<PathBuf>) {
        let mut mounts = vec![
            r#"    - {id: 1, at: /, fs: ext4, dev: "254:1", size: 10G}"#.to_owned(),
            r#"    - {id: 2, at: /home, fs: ext4, dev: "254:2", size: 10G, parent: 1}"#.to_owned(),
            r#"    - {id: 4, at: /tmp, fs: tmpfs, dev: "0:40", size: 4G, parent: 1}"#.to_owned(),
        ];
        if self.other_fs {
            let parent = if self.tmp() { 4 } else { 2 };
            mounts.push(format!(
                "    - {{id: 3, at: {}, fs: ext4, dev: \"254:3\", size: 10G, parent: {parent}}}",
                self.pkg_path()
            ));
        }
        let (tree, paths) = self.tree(signals);
        let yaml = format!(
            "host_model: 1\nclock: 2026-10-15T03:00:00Z\nmounts:\n{}\ntree:\n{tree}\nprocesses:\n    - {{pid: 500, exe: /usr/bin/bash, cwd: /, maps: [/usr/bin/bash], environ: [HOME=/home/tester], fds: [{{path: /usr/bin/bash}}]}}\n{}",
            mounts.join("\n"),
            self.events(signals),
        );
        (yaml, paths)
    }

    /// The host event that makes the stamp during the run, if any.
    fn events(&self, signals: bool) -> String {
        let mut events = Vec::new();
        if let Some((at, nth)) = self.fault {
            let marker = format!("{}/CMakeCache.txt", self.out_path());
            let (op, path) = match at {
                FaultAt::Top => ("read_dir", self.top.to_owned()),
                FaultAt::Pkg => ("read_dir", self.pkg_path()),
                FaultAt::Out => ("read_dir", self.out_path()),
                FaultAt::MarkerStatx => ("statx", marker),
                FaultAt::MarkerRead => ("read_head", marker),
                FaultAt::MarkerOpen => ("openat2", marker),
                FaultAt::SourceOpen => ("openat2", format!("{}/CMakeLists.txt", self.pkg_path())),
            };
            events.push(format!(
                "    - {{at: {{op: {op}, path: {path}, nth: {nth}, when: before}}, fail: EACCES}}"
            ));
        }
        if let Some(stamp) = self.stamp_event().filter(|_| signals) {
            events.push(stamp);
        }
        if events.is_empty() {
            String::new()
        } else {
            format!("events:\n{}\n", events.join("\n"))
        }
    }

    fn stamp_event(&self) -> Option<String> {
        let (at, name) = self.stamp?;
        let dir = if at == StampAt::Top {
            self.top.to_owned()
        } else {
            self.pkg_path()
        };
        let out = self.out_path();
        let trigger = match self.made {
            Made::Before => return None,
            Made::AfterDiscovery => {
                format!("{{op: read_head, path: {out}/CMakeCache.txt, nth: 2, when: before}}")
            }
            Made::AfterLock => format!("{{op: flock, path: {out}, nth: 1, when: after}}"),
            // The build's third listing: in the `--no-index` cases
            // the locked walk's (after the tally's and the ordering walk's),
            // so after the locked anchor check and its guard.
            Made::DuringWalk => format!("{{op: read_dir, path: {out}, nth: 3, when: after}}"),
        };
        Some(format!("    - at: {trigger}\n      create: {dir}/{name}"))
    }

    fn vector(&self, signals: bool) -> (String, Vec<PathBuf>) {
        let (host, paths) = self.host_yaml(signals);
        let host: String = host.lines().map(|l| format!("  {l}\n")).collect();
        let yaml = format!(
            "profile_vector: 1\n{}select: \"{}\"\nroots: [{}]\npath: {}\nmin_free: 100%\nno_index: {}\nhost:\n{host}expect:\n  plan: []\n  reclaim_exit: 1\n  actions: []\n  removed: []\n  skipped: []\n  preserved: []\n",
            if self.release {
                "catalog: release-0.2\n"
            } else {
                ""
            },
            self.select(),
            self.roots().join(", "),
            self.path(),
            self.no_index,
        );
        (yaml, paths)
    }

    fn survey_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if self.survey_top || !self.survey_pkg {
            paths.push(PathBuf::from(self.top));
        }
        if self.survey_pkg {
            paths.push(PathBuf::from(self.pkg_path()));
        }
        paths
    }
}

/// What one tree came to: the reclaim, the dry run and the survey.
#[derive(Debug)]
struct Seen {
    /// Whether the reclaim's events fired (a runtime stamp was made).
    fired: bool,
    removed: BTreeSet<PathBuf>,
    actions: BTreeSet<PathBuf>,
    eligible: BTreeSet<PathBuf>,
    surveyed: BTreeSet<PathBuf>,
}

fn seen(scenario: &Scenario, signals: bool) -> Result<Seen, String> {
    let (yaml, paths) = scenario.vector(signals);
    let vector = ProfileVector::parse(&yaml).map_err(|e| format!("{e}\n{yaml}"))?;
    let outcomes = vector.outcomes(&paths)?;
    let (host_yaml, _) = scenario.host_yaml(signals);
    let host = common::host(&host_yaml);
    let options = SurveyOptions {
        paths: scenario.survey_paths(),
        select: Some(scenario.select()),
        roots: scenario.roots(),
        timeout: Some(Duration::from_secs(600)),
        in_use: true,
    };
    let draw_seed = || Err("no seed".to_owned());
    let builtins = || Library::builtin().map_err(|e| e.to_string());
    let platform = Platform {
        catalog: if scenario.release {
            Catalog::from(diskgc_core::release::release("0.2").map_err(|e| e.to_string())?)
        } else {
            Catalog::Development
        },
        draw_seed: &draw_seed,
        builtins: &builtins,
    };
    let none = ConfigFile::NotFound { tried: Vec::new() };
    let loaded = survey::load(&options, &none, &platform, &mut |_| {})
        .map_err(|(cause, why)| format!("survey load: {cause:?}: {why}"))?;
    let report = survey::run(&host, &options, &loaded, &mut |_| {}, &mut |_| {});
    let value: Value = serde_json::from_str(&report.to_json()).map_err(|e| e.to_string())?;
    let surveyed = value["paths"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|path| path["candidates"].as_array().into_iter().flatten())
        .filter(|row| row["eligible"] == true)
        .filter_map(|row| row["candidate"].as_str().map(PathBuf::from))
        .collect();
    Ok(Seen {
        fired: outcomes.reclaim_events_fired,
        removed: outcomes.removed,
        actions: outcomes.actions,
        eligible: outcomes.eligible,
        surveyed,
    })
}

fn general_scenario() -> impl Strategy<Value = Scenario> {
    let layout = prop_oneof![
        2 => Just(("/home/tester/w", "pkg", "out")),
        1 => Just(("/home/tester/w", "p1/p2/p3/pkg", "out")),
        2 => Just(("/tmp", "pkg", "out")),
        3 => Just(("/tmp", "nix-shell.AB/pytest-of-tester", "pytest-0")),
    ];
    let stamp = prop_oneof![
        1 => Just(None),
        3 => (
            prop_oneof![Just(StampAt::Top), Just(StampAt::Pkg)],
            prop_oneof![Just(".built"), Just(".stamp_built"), Just(".configured_68b3"), Just(".prepared")],
        )
            .prop_map(Some),
    ];
    let made = prop_oneof![
        3 => Just(Made::Before),
        1 => Just(Made::AfterDiscovery),
        1 => Just(Made::AfterLock),
        1 => Just(Made::DuringWalk),
    ];
    let nested = prop_oneof![
        Just(Nested::None),
        Just(Nested::CmakeA),
        Just(Nested::MesonAB),
        Just(Nested::AutotoolsA),
    ];
    let roots = prop::sample::subsequence(vec![Root::Top, Root::Pkg, Root::OutA], 1..=3);
    // pytest-tmp, the term whose candidates can also be CMake builds, most.
    let union = prop_oneof![
        2 => Just(Union::Cmake),
        2 => Just(Union::Meson),
        1 => Just(Union::Autotools),
        1 => Just(Union::NixShellRc),
        4 => Just(Union::PytestTmp),
    ];
    (
        (
            layout,
            stamp,
            prop::bool::weighted(0.15),
            any::<bool>(),
            nested,
        ),
        (
            any::<bool>(),
            prop::bool::weighted(0.2),
            roots,
            any::<bool>(),
            any::<bool>(),
        ),
        (
            union,
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
        ),
        made,
        // Weighted to the package directory and its first listings, where
        // one failed listing hides a build from a guarded term's discovery,
        // and to the build's marker and source, whose failure refuses it
        // in one term's discovery only (the sixth review's B1).
        prop_oneof![
            1 => Just(None),
            1 => (fault_at(), prop_oneof![2 => 1u8..=2, 1 => 3u8..=6]).prop_map(Some),
        ],
    )
        .prop_map(
            |(
                ((top, pkg, out), stamp, br_marker, out_recent, nested),
                (git_in_out, keep_at_top, roots, other_fs, path_at_pkg),
                (union, no_index, release, survey_top, survey_pkg),
                made,
                fault,
            )| {
                // nix-shell-rc and pytest-tmp have /tmp roots of their own.
                let union = match union {
                    Union::NixShellRc | Union::PytestTmp if top != "/tmp" => Union::Meson,
                    other => other,
                };
                Scenario {
                    top,
                    pkg,
                    out,
                    stamp,
                    made,
                    fault,
                    br_marker,
                    out_recent,
                    nested,
                    git_in_out,
                    keep_at_top,
                    roots,
                    other_fs,
                    path_at_pkg,
                    union,
                    no_index,
                    release,
                    survey_top,
                    survey_pkg,
                }
            },
        )
}

/// Where a generated fault lands: listings and the build's file operations
/// about equally.
fn fault_at() -> impl Strategy<Value = FaultAt> {
    prop_oneof![
        1 => Just(FaultAt::Top),
        3 => Just(FaultAt::Pkg),
        1 => Just(FaultAt::Out),
        1 => Just(FaultAt::MarkerStatx),
        1 => Just(FaultAt::MarkerRead),
        2 => Just(FaultAt::MarkerOpen),
        1 => Just(FaultAt::SourceOpen),
    ]
}

/// The generated scenarios: the general family, and one in four from a
/// family aimed at a guarded term's incomplete view (Codex's fifth
/// review): a union with another term that finds the same objects, the
/// stamp in place, one listing failing at any point of the run.
fn scenario() -> impl Strategy<Value = Scenario> {
    let focused = (
        general_scenario(),
        prop_oneof![
            3 => Just(Union::PytestTmp),
            1 => Just(Union::NixShellRc),
            1 => Just(Union::Meson),
        ],
        (fault_at(), prop_oneof![3 => Just(1u8), 1 => 2u8..=6]),
        prop::bool::weighted(0.75),
    )
        .prop_map(|(base, union, fault, pytest_layout)| {
            let (pkg, out) = if pytest_layout {
                ("nix-shell.AB/pytest-of-tester", "pytest-0")
            } else {
                ("pkg", "out")
            };
            Scenario {
                top: "/tmp",
                pkg,
                out,
                union,
                fault: Some(fault),
                made: Made::Before,
                stamp: base.stamp.or(Some((StampAt::Pkg, ".built"))),
                out_recent: false,
                ..base
            }
        });
    prop_oneof![3 => general_scenario(), 1 => focused]
}

/// Both properties for one scenario, and what the run without the
/// guard's signals did ([`Vacuity`]).
fn check(scenario: &Scenario) -> Result<Vacuity, TestCaseError> {
    let on = seen(scenario, true).map_err(TestCaseError::fail)?;
    let off = seen(scenario, false).map_err(TestCaseError::fail)?;
    let fail = |what: &str, detail: String| {
        TestCaseError::fail(format!(
            "{what}: {detail}\nscenario: {scenario:#?}\nwith the guard's signals:\n{}",
            scenario.vector(true).0
        ))
    };
    // 1. Only adds protection, compared on fault-free runs: a one-shot
    // fault at a directory's nth listing lands on another reader once the
    // guard lists less (a stamp in the parent ends its climb), so the two
    // runs would not see the same fault. Faults are judged by 2.
    if scenario.fault.is_none() {
        // The plans (a dry run, a survey) change nothing, so they compare
        // object for object.
        for (what, with, without) in [
            ("planned eligible", &on.eligible, &off.eligible),
            ("surveyed eligible", &on.surveyed, &off.surveyed),
        ] {
            let extra: Vec<_> = with.difference(without).collect();
            if !extra.is_empty() {
                return Err(fail(
                    "the guard removed a protection",
                    format!("{what} with the guard's signals but not without: {extra:?}"),
                ));
            }
        }
        // A reclaim deletes in order, and a deletion changes its parent's
        // mtime, so which later candidate is still idle depends on what went
        // before (seen: without the guard a nested build's deletion made the
        // pytest session holding it look new). What it removes must lie at or
        // below something the run without the signals planned or took.
        let allowed: Vec<&PathBuf> = off.eligible.iter().chain(&off.actions).collect();
        let extra: Vec<_> = on
            .removed
            .iter()
            .filter(|path| !allowed.iter().any(|a| path.starts_with(a)))
            .collect();
        if !extra.is_empty() {
            return Err(fail(
                "the guard removed a protection",
                format!(
                    "removed with the guard's signals, below nothing the run without them planned or took: {extra:?}"
                ),
            ));
        }
    }

    // 2. Independent of the view: a covered build is never a candidate
    // the run takes, plans or reports eligible (another profile's
    // candidate holding it, a pytest session, is judged by that profile:
    // docs/safety.md, Not covered).
    // A stamp made during the run guards only what is judged after it: in
    // the reclaim, once the event fired, the build whose judgement or lock
    // the event is tied to (`out`; a nested build may have gone before,
    // and the plans can precede it).
    let runtime = scenario.made != Made::Before && scenario.stamp.is_some();
    let covered: BTreeSet<PathBuf> = if !runtime || scenario.br_marker {
        scenario.covered()
    } else if on.fired {
        let out = PathBuf::from(scenario.out_path());
        scenario
            .covered()
            .into_iter()
            .filter(|c| *c == out)
            .collect()
    } else {
        BTreeSet::new()
    };
    for covered in covered {
        for (what, set) in [
            ("taken", &on.actions),
            ("planned eligible", &on.eligible),
            ("surveyed eligible", &on.surveyed),
        ] {
            if runtime && what != "taken" && !scenario.br_marker {
                continue;
            }
            if set.contains(&covered) {
                return Err(fail(
                    "a build the guard's rule covers was not protected",
                    format!("{} {what}", covered.display()),
                ));
            }
        }
    }
    Ok(Vacuity {
        removed: !off.removed.is_empty(),
        surveyed: !off.surveyed.is_empty(),
        fired: on.fired,
        out_removed: on.removed.contains(&PathBuf::from(scenario.out_path())),
    })
}

/// What the run without the guard's signals did: the properties are not
/// vacuous when it removes something and `survey` reports something
/// eligible.
struct Vacuity {
    removed: bool,
    surveyed: bool,
    /// The run with the signals fired every host event (a runtime stamp
    /// was made in the reclaim).
    fired: bool,
    /// The run with the signals removed `out`.
    out_removed: bool,
}

#[test]
fn the_nested_build_guard_only_adds_protection() {
    let mut runner = TestRunner::new(ProptestConfig {
        cases: cases(),
        ..ProptestConfig::default()
    });
    let removing = std::cell::Cell::new(0u32);
    let surveying = std::cell::Cell::new(0u32);
    if let Err(error) = runner.run(&scenario(), |scenario| {
        let vacuity = check(&scenario)?;
        removing.set(removing.get() + u32::from(vacuity.removed));
        surveying.set(surveying.get() + u32::from(vacuity.surveyed));
        Ok(())
    }) {
        panic!("{error}");
    }
    // Not vacuous: without the signals survey reports builds eligible (in
    // about a quarter of the cases; faults and recent builds take some).
    assert!(
        surveying.get() * 6 >= cases(),
        "only {} of {} cases surveyed anything eligible without the guard's signals",
        surveying.get(),
        cases()
    );
    // Not vacuous: without the signals the reclaims do delete (in about a
    // fifth of the cases; faults, recent builds and checkouts keep some).
    assert!(
        removing.get() * 6 >= cases(),
        "only {} of {} cases removed anything without the guard's signals",
        removing.get(),
        cases()
    );
}

/// Codex's four cases on eef0dda1 (third review), as fixed scenarios the
/// generator also draws: each must hold both properties.
#[test]
fn codex_third_review_cases_hold_both_properties() {
    let base = Scenario {
        top: "/home/tester/w",
        pkg: "pkg",
        out: "out",
        stamp: Some((StampAt::Top, ".built")),
        made: Made::Before,
        fault: None,
        br_marker: false,
        out_recent: false,
        nested: Nested::None,
        git_in_out: false,
        keep_at_top: false,
        roots: vec![Root::Top, Root::Pkg],
        other_fs: false,
        path_at_pkg: false,
        union: Union::Cmake,
        no_index: false,
        release: false,
        survey_top: true,
        survey_pkg: false,
    };
    let cases = [
        // 1: preflight drops R (another filesystem than the PATH's).
        Scenario {
            other_fs: true,
            path_at_pkg: true,
            ..base.clone()
        },
        // 2: a union whose other term's root holds the stamp.
        Scenario {
            top: "/tmp",
            roots: vec![Root::Pkg],
            union: Union::NixShellRc,
            ..base.clone()
        },
        // 3: the narrower survey PATH.
        Scenario {
            survey_top: true,
            survey_pkg: true,
            ..base.clone()
        },
        // 4: a vetoed contributor's checkout refusal in a union.
        Scenario {
            top: "/tmp",
            pkg: "nix-shell.AB/pytest-of-tester",
            out: "pytest-0",
            stamp: Some((StampAt::Pkg, ".built")),
            git_in_out: true,
            roots: vec![Root::Top],
            union: Union::PytestTmp,
            ..base.clone()
        },
    ];
    let mut failed = Vec::new();
    for (n, scenario) in cases.iter().enumerate() {
        for release in [false, true] {
            let scenario = Scenario {
                release,
                ..scenario.clone()
            };
            let catalog = if release { "0.2" } else { "development" };
            match check(&scenario) {
                // Case 3 compares surveys; the others reclaim.
                Ok(v) if (n == 2 && v.surveyed) || (n != 2 && v.removed) => {}
                Ok(_) => failed.push(format!(
                    "case {} ({catalog}): vacuous, nothing removed or surveyed without the guard",
                    n + 1
                )),
                Err(e) => failed.push(format!("case {} ({catalog}): {e}", n + 1)),
            }
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n\n"));
}

/// Codex's fourth review: stamps made during the run, (a) after discovery,
/// (b) right after the lock, (c) during the locked walk, each for
/// cmake-build-anchored alone and in a union with pytest-tmp (the session
/// a CMake build too, with and without a Git checkout), both catalogs,
/// with `--no-index` (no snapshot is read: the reclaim discovers and
/// tallies the tier itself, with an index of its own in memory).
#[test]
fn codex_fourth_review_runtime_stamps_hold_both_properties() {
    let alone = Scenario {
        top: "/home/tester/w",
        pkg: "pkg",
        out: "out",
        stamp: Some((StampAt::Top, ".built")),
        made: Made::Before,
        fault: None,
        br_marker: false,
        out_recent: false,
        nested: Nested::None,
        git_in_out: false,
        keep_at_top: false,
        roots: vec![Root::Top],
        other_fs: false,
        path_at_pkg: false,
        union: Union::Cmake,
        no_index: true,
        release: false,
        survey_top: true,
        survey_pkg: false,
    };
    let union = Scenario {
        top: "/tmp",
        pkg: "nix-shell.AB/pytest-of-tester",
        out: "pytest-0",
        stamp: Some((StampAt::Pkg, ".built")),
        union: Union::PytestTmp,
        ..alone.clone()
    };
    let mut failed = Vec::new();
    for made in [Made::AfterDiscovery, Made::AfterLock, Made::DuringWalk] {
        for (shape, base) in [
            ("alone", &alone),
            ("union", &union),
            (
                "union with a checkout",
                &Scenario {
                    git_in_out: true,
                    ..union.clone()
                },
            ),
        ] {
            for release in [false, true] {
                let scenario = Scenario {
                    made,
                    release,
                    ..base.clone()
                };
                // Codex's fifth review: each case must reach the phase it
                // names (its event fires in the reclaim), except where the
                // checkout keeps the session before the lock: a case of its
                // own, whose event must not fire and whose session stays.
                let early_checkout = base.git_in_out && made != Made::AfterDiscovery;
                let name = format!("{made:?} {shape} (release {release})");
                match check(&scenario) {
                    Err(e) => failed.push(format!("{name}: {e}")),
                    Ok(v) if !v.removed => failed.push(format!("{name}: vacuous")),
                    Ok(v) if early_checkout && (v.fired || v.out_removed) => failed.push(format!(
                        "{name}: expected the checkout to keep the session before the lock \
                         (event fired {}, session removed {})",
                        v.fired, v.out_removed
                    )),
                    Ok(v) if !early_checkout && !v.fired => {
                        failed.push(format!("{name}: its event never fired"))
                    }
                    Ok(_) => {}
                }
            }
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n\n"));
}

/// Codex's fifth review, item 2: incomplete guarded coverage. The stamp is
/// there from the start; the first listing of the package directory (a
/// guarded term's discovery's) fails once, so no guarded term finds the
/// build: another term must not take it. Both catalogs, with and without
/// the index snapshot.
#[test]
fn codex_fifth_review_unlisted_by_a_guarded_term_holds_both_properties() {
    let base = Scenario {
        top: "/tmp",
        pkg: "nix-shell.AB/pytest-of-tester",
        out: "pytest-0",
        stamp: Some((StampAt::Pkg, ".built")),
        made: Made::Before,
        fault: Some((FaultAt::Pkg, 1)),
        br_marker: false,
        out_recent: false,
        nested: Nested::None,
        git_in_out: false,
        keep_at_top: false,
        roots: vec![Root::Top],
        other_fs: false,
        path_at_pkg: false,
        union: Union::PytestTmp,
        no_index: false,
        release: false,
        survey_top: true,
        survey_pkg: false,
    };
    let mut failed = Vec::new();
    for release in [false, true] {
        for no_index in [false, true] {
            let scenario = Scenario {
                release,
                no_index,
                ..base.clone()
            };
            match check(&scenario) {
                Ok(v) if v.fired => {}
                Ok(_) => failed.push(format!(
                    "release {release}, no_index {no_index}: the fault never fired"
                )),
                Err(e) => failed.push(format!("release {release}, no_index {no_index}: {e}")),
            }
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n\n"));
}

/// The sixth review's B1: with the stamp in place, the session's marker
/// cannot be `statx`ed, read or opened once, or the anchor's source not
/// opened, by whoever comes first (cmake's discovery): cmake refuses the
/// session and never joins the union's view of it. The guard is the
/// selection's, so pytest-tmp's view must keep it too. Both catalogs,
/// with and without the index snapshot; each fault must fire.
#[test]
fn sixth_review_a_guarded_term_refusing_the_build_holds_both_properties() {
    let base = Scenario {
        top: "/tmp",
        pkg: "nix-shell.AB/pytest-of-tester",
        out: "pytest-0",
        stamp: Some((StampAt::Pkg, ".built")),
        made: Made::Before,
        fault: None,
        br_marker: false,
        out_recent: false,
        nested: Nested::None,
        git_in_out: false,
        keep_at_top: false,
        roots: vec![Root::Top],
        other_fs: false,
        path_at_pkg: false,
        union: Union::PytestTmp,
        no_index: false,
        release: false,
        survey_top: true,
        survey_pkg: false,
    };
    let mut failed = Vec::new();
    for at in [
        FaultAt::MarkerStatx,
        FaultAt::MarkerRead,
        FaultAt::MarkerOpen,
        FaultAt::SourceOpen,
    ] {
        for release in [false, true] {
            for no_index in [false, true] {
                let scenario = Scenario {
                    fault: Some((at, 1)),
                    release,
                    no_index,
                    ..base.clone()
                };
                let name = format!("{at:?} (release {release}, no_index {no_index})");
                match check(&scenario) {
                    Ok(v) if v.fired => {}
                    Ok(_) => failed.push(format!("{name}: the fault never fired")),
                    Err(e) => failed.push(format!("{name}: {e}")),
                }
            }
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n\n"));
}

/// A census, not a gate (`cargo test -p diskgc-model --test guard_props --
/// --ignored`): draws `DISKGC_PROPERTY_CASES` scenarios without stopping
/// at the first failure and counts the failures by property and by the
/// shape that matters (union, root set, other filesystem, survey of two
/// PATHs), to show which classes the generator reaches.
#[test]
#[ignore = "a census for reviews; the property test is the gate"]
fn guard_property_failure_census() {
    use proptest::strategy::ValueTree;
    let mut runner = TestRunner::deterministic();
    let mut census = std::collections::BTreeMap::<String, u32>::new();
    let total = cases();
    for _ in 0..total {
        let scenario = scenario()
            .new_tree(&mut runner)
            .expect("a scenario")
            .current();
        if let Err(error) = check(&scenario) {
            let message = error.to_string();
            let property = if message.contains("removed a protection") {
                "1 only adds protection"
            } else {
                "2 independent of the view"
            };
            let shape = format!(
                "union {:?}, roots {:?}, other_fs {}, survey of two PATHs {}",
                scenario.union,
                scenario.roots,
                scenario.other_fs && scenario.path_at_pkg,
                scenario.survey_top && scenario.survey_pkg
            );
            *census.entry(format!("{property}: {shape}")).or_default() += 1;
        }
    }
    for (class, count) in &census {
        eprintln!("{count:5}  {class}");
    }
    eprintln!(
        "{} of {total} scenarios failed",
        census.values().sum::<u32>()
    );
}
