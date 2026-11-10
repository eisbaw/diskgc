//! The vector format (`profile_vector: 1`) and its one runner: a reviewable
//! scenario of an `ensure` invocation (a configuration file, `--select`,
//! `--roots`, `--allow-root`, the request) on a `host_model: 1`, and the
//! independently written expected plan and reclaim. The runner drives
//! `diskgc_core::invocation::ensure`, the composition the binary runs, on
//! two fresh `ModelHost`s (one dry run, one reclaim) and compares the
//! results with the expectations and with invariants that hold for every
//! vector (docs/testing.md, Vector corpus). Test-only: product code never
//! reads vector expectations.
//!
//! The format began as one built-in profile per file (`profile:`); every
//! other key is optional, so those files are unchanged.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::invocation::{self, Catalog, ConfigFile, ConfigUse, EnsureOptions, Platform};
use diskgc_core::profile::{Action, Body, Library, Origin, ProfileSpec};
use diskgc_core::report::Report;
use diskgc_core::units::{parse_duration, parse_size};
use serde::Deserialize;
use serde_json::Value;

use crate::ModelHost;
use crate::spec::{HostModel, Op};

/// Where a vector's `config:` text is said to come from, and the path a
/// search without one tried (messages only).
const CONFIG_PATH: &str = "/etc/diskgc/config.yaml";

/// `--timeout` when a vector gives none: long enough that no modelled
/// scenario reaches it by accident (the clock is virtual).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Directories below a corpus root a walk descends into: areas, and areas
/// within areas. A deeper file is an error, not a silently skipped vector.
const CORPUS_DEPTH_MAX: usize = 3;

/// One versioned test case. The host is the ordinary strict host model.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileVector {
    pub profile_vector: u32,
    /// Shorthand for `select: profile:NAME`.
    #[serde(default)]
    pub profile: Option<String>,
    /// `--select`. Without it and without `profile`, the configured groups
    /// run (a `config` is then required, as by the binary).
    #[serde(default)]
    pub select: Option<String>,
    /// The configuration file's text (`--config`); absent: none was found.
    #[serde(default)]
    pub config: Option<String>,
    /// User profiles without a configuration file: the `--select` library
    /// is the built-ins plus these. Not together with `config`.
    #[serde(default)]
    pub user_profiles: Vec<ProfileSpec>,
    pub path: PathBuf,
    #[serde(default)]
    pub roots: Vec<String>,
    pub min_free: String,
    #[serde(default)]
    pub target_free: Option<String>,
    /// `--timeout` (`0`: none); default [`DEFAULT_TIMEOUT`].
    #[serde(default)]
    pub timeout: Option<String>,
    /// `--tally-budget` (`0`: none); default derived from the timeout.
    #[serde(default)]
    pub tally_budget: Option<String>,
    #[serde(default)]
    pub allow_root: bool,
    /// The built-in catalog of the build under test (`development`, the
    /// default, `release-0.1` or `release-0.2`, that release's row of the
    /// release descriptor): what the binary's `release-catalog` feature
    /// selects.
    #[serde(default)]
    pub catalog: VectorCatalog,
    /// `DISKGC_SEED` for `ranking: random`; absent: drawing one fails.
    #[serde(default)]
    pub seed: Option<u64>,
    /// The walk's entry budget per candidate, in place of the product's
    /// 2,000,000: a test seam (`EnsureOptions::test_entry_budget`) the
    /// binary cannot set, so `too_many_entries` fits in a small model.
    #[serde(default)]
    pub entry_budget: Option<usize>,
    /// `--no-index`: the run neither reads nor writes the index snapshot
    /// (it still tallies and queues with an index of its own, in memory).
    #[serde(default)]
    pub no_index: bool,
    pub host: HostModel,
    pub expect: Expected,
}

/// What a vector's runs did, for a property that compares two runs
/// ([`ProfileVector::outcomes`]) instead of an expectation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcomes {
    /// The dry run's plan entries without a skip reason.
    pub eligible: BTreeSet<PathBuf>,
    /// The reclaim's actions.
    pub actions: BTreeSet<PathBuf>,
    /// Of the paths asked about, those the reclaim left absent.
    pub removed: BTreeSet<PathBuf>,
    /// Every event of the host fired during the reclaim.
    pub reclaim_events_fired: bool,
}

/// [`Catalog`], as a vector names it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum VectorCatalog {
    #[default]
    #[serde(rename = "development")]
    Development,
    #[serde(rename = "release-0.1")]
    Release01,
    /// Release 0.2's row: its catalog and the command actions it runs.
    #[serde(rename = "release-0.2")]
    Release02,
}

impl From<VectorCatalog> for Catalog {
    fn from(catalog: VectorCatalog) -> Self {
        match catalog {
            VectorCatalog::Development => Catalog::Development,
            VectorCatalog::Release01 => Catalog::from(
                diskgc_core::release::release("0.1").expect("release 0.1 in the descriptor"),
            ),
            VectorCatalog::Release02 => Catalog::from(
                diskgc_core::release::release("0.2").expect("release 0.2 in the descriptor"),
            ),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    /// Complete dry-run plan, including skipped candidates.
    pub plan: Vec<PlanVerdict>,
    /// The dry run's exit code (default 0).
    #[serde(default)]
    pub dry_run_exit: i32,
    /// Complete dry-run error set (default none).
    #[serde(default)]
    pub dry_run_errors: Vec<ErrorVerdict>,
    pub reclaim_exit: i32,
    /// Complete reclaim error set (default none).
    #[serde(default)]
    pub errors: Vec<ErrorVerdict>,
    /// Complete action set.
    pub actions: Vec<PathBuf>,
    /// Paths that must be gone after reclaim. Actions can retain their
    /// candidate (for example Cargo keeps the tagged target root).
    pub removed: Vec<PathBuf>,
    /// Complete reclaim skip set.
    pub skipped: Vec<SkipVerdict>,
    /// Existing paths that must survive reclaim, including candidates not
    /// discovered because required source evidence is absent.
    pub preserved: Vec<PathBuf>,
    /// Complete list of command runs (command and Docker actions), in
    /// order; absent: not compared (the invariants still apply).
    #[serde(default)]
    pub commands: Option<Vec<CommandVerdict>>,
    /// Complete list of the reclaim's Nix children (queries, the root
    /// check, the GC), in order: argv and environment; absent: not
    /// compared (a dry run never runs the root check or the GC either way).
    #[serde(default)]
    pub nix_commands: Option<Vec<NixCommandVerdict>>,
    /// The same for the dry run, which runs read-only queries only.
    #[serde(default)]
    pub dry_run_nix_commands: Option<Vec<NixCommandVerdict>>,
    /// A subset of the reclaim's JSON report: every key given must be
    /// there with that value (objects recursively, arrays elementwise).
    #[serde(default)]
    pub report: Option<Value>,
    /// The same for the dry run's report.
    #[serde(default)]
    pub dry_run_report: Option<Value>,
    /// `check`'s root warnings ([`invocation::check_roots`], TASK-194), in
    /// order: each must contain its text, and there must be no other.
    /// Absent: not compared, but `check` still runs on every vector whose
    /// configuration loads, read-only and without reaching a dead mount.
    #[serde(default)]
    pub check_warnings: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanVerdict {
    /// `null` for a row of the tier as a whole (no candidate).
    pub candidate: Option<PathBuf>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub source: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkipVerdict {
    /// `null` for a skip of the tier as a whole (no candidate).
    pub candidate: Option<PathBuf>,
    pub reason: String,
    /// The complete `process_blockers` (pid and evidence); absent: not
    /// compared, `[]`: the report's must be `null`.
    #[serde(default)]
    pub process_blockers: Option<Vec<BlockerVerdict>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockerVerdict {
    pub pid: u32,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorVerdict {
    /// The stable `cause` code.
    pub cause: String,
    /// Text the message must contain (messages are for people; keep this
    /// to the stable part).
    #[serde(default)]
    pub contains: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandVerdict {
    /// The checked working directory.
    pub cwd: PathBuf,
    /// The executable followed by its arguments.
    pub argv: Vec<String>,
}

/// One Nix child ([`crate::host::NixCall`]).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NixCommandVerdict {
    /// The program followed by its arguments; the pinned socket is
    /// `unix:///proc/self/fd/N`.
    pub argv: Vec<String>,
    /// Exactly these `NAME=VALUE`s; absent: diskgc's own environment.
    #[serde(default)]
    pub env: Option<Vec<String>>,
}

/// The Nix children of a run, as a vector states them.
fn nix_commands(host: &ModelHost) -> Vec<NixCommandVerdict> {
    host.trace()
        .iter()
        .filter_map(|call| call.nix.as_ref())
        .map(|nix| NixCommandVerdict {
            argv: nix
                .argv
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
            env: nix.environment.as_ref().map(|environment| {
                environment
                    .iter()
                    .map(|(name, value)| {
                        format!("{}={}", name.to_string_lossy(), value.to_string_lossy())
                    })
                    .collect()
            }),
        })
        .collect()
}

/// What a passing vector produced, for coverage questions asked of the
/// whole corpus (which reasons, which profiles). Every entry here matched
/// the vector's expectation.
#[derive(Debug, Clone)]
pub struct Checked {
    pub dry_run: Report,
    pub reclaim: Report,
    /// Names the run's library took unchanged from the built-ins (not
    /// overridden by the vector's own profiles).
    pub builtin: BTreeSet<String>,
}

impl ProfileVector {
    pub fn parse(yaml: &str) -> Result<Self, String> {
        let vector: Self = serde_saphyr::from_str(yaml).map_err(|e| e.to_string())?;
        if vector.profile_vector != 1 {
            return Err(format!(
                "unsupported profile_vector {} (expected 1)",
                vector.profile_vector
            ));
        }
        if vector.host.expect.is_some() {
            return Err("profile vector host must not contain a second expect block".to_owned());
        }
        if vector.profile.is_some() && vector.select.is_some() {
            return Err("give `profile` or `select`, not both".to_owned());
        }
        if vector.expect.check_warnings.is_some() && vector.config.is_none() {
            return Err("check_warnings needs a config: check reads one".to_owned());
        }
        if vector.config.is_some() && !vector.user_profiles.is_empty() {
            return Err(
                "user_profiles belong in the config's `profiles:` when a config is given"
                    .to_owned(),
            );
        }
        if !vector.roots.is_empty() && vector.select_expression().is_none() {
            return Err("roots are --roots, only valid with profile or select".to_owned());
        }
        ModelHost::new(&vector.host).map_err(|e| e.to_string())?;
        vector.options(false)?;
        Ok(vector)
    }

    /// `--select` as the binary would get it.
    pub fn select_expression(&self) -> Option<String> {
        self.select
            .clone()
            .or_else(|| self.profile.as_ref().map(|name| format!("profile:{name}")))
    }

    fn options(&self, dry_run: bool) -> Result<EnsureOptions, String> {
        let duration = |text: &str| -> Result<Option<Duration>, String> {
            if text == "0" {
                return Ok(None);
            }
            parse_duration(text).map(Some)
        };
        let timeout = match &self.timeout {
            Some(text) => duration(text)?,
            None => Some(DEFAULT_TIMEOUT),
        };
        let tally_budget = match &self.tally_budget {
            Some(text) => duration(text)?,
            None => Some(diskgc_core::executor::default_tally_budget(timeout)),
        };
        Ok(EnsureOptions {
            min_free: parse_size(&self.min_free)?,
            target_free: self.target_free.as_deref().map(parse_size).transpose()?,
            timeout,
            tally_budget,
            select: self.select_expression(),
            roots: self.roots.clone(),
            dry_run,
            allow_root: self.allow_root,
            no_index: self.no_index,
            path: self.path.clone(),
            test_entry_budget: self.entry_budget,
        })
    }

    /// The configuration file as the binary's lookup would leave it.
    fn config_file(&self) -> ConfigFile {
        match &self.config {
            Some(text) => ConfigFile::Read {
                path: PathBuf::from(CONFIG_PATH),
                bytes: text.as_bytes().to_vec(),
            },
            None => ConfigFile::NotFound {
                tried: vec![PathBuf::from(CONFIG_PATH)],
            },
        }
    }

    /// The built-ins before the catalog narrows them: the embedded ones
    /// plus the vector's `user_profiles`.
    fn builtins(&self) -> Result<Library, String> {
        if self.user_profiles.is_empty() {
            Library::builtin().map_err(|e| format!("built-ins: {e}"))
        } else {
            Library::with_user_profiles(&self.user_profiles)
                .map_err(|e| format!("vector user profiles: {e}"))
        }
    }

    /// The library the run selects from, for the per-action invariants,
    /// loaded through the product's [`Catalog::config`] or
    /// [`Catalog::library`]: the configuration's, or `builtins` under the
    /// vector's catalog. `None` when that does not load, so the run must
    /// refuse (checked by [`Self::check_with`]).
    fn library(&self, builtins: Library) -> Option<Library> {
        let catalog = Catalog::from(self.catalog);
        match &self.config {
            Some(text) => catalog
                .config(Path::new(CONFIG_PATH), text.as_bytes(), ConfigUse::Run)
                .ok()
                .map(|config| config.library),
            None => catalog.library(builtins).ok(),
        }
    }

    /// `builtins`: the built-ins the run's [`Platform`] offers (the
    /// mutation seam of [`Self::check`]).
    fn run(&self, host: &ModelHost, dry_run: bool, builtins: &Library) -> Result<Report, String> {
        let options = self.options(dry_run)?;
        let seed = self.seed;
        let draw_seed = || seed.ok_or_else(|| "no DISKGC_SEED in this vector".to_owned());
        let offered = || Ok(builtins.clone());
        let platform = Platform {
            catalog: self.catalog.into(),
            draw_seed: &draw_seed,
            builtins: &offered,
        };
        // The binary's order: the configuration and `--select` first,
        // then the run on the host.
        let loaded = match invocation::load(
            options.selection(),
            &self.config_file(),
            &platform,
            &mut |_| {},
        ) {
            Ok(loaded) => loaded,
            Err((cause, why)) => {
                return Ok(invocation::refused(&options, cause, why, &mut |_| {}));
            }
        };
        Ok(invocation::ensure(
            host,
            &options,
            loaded,
            &platform,
            &mut |_| {},
            &mut |_| {},
        ))
    }

    /// Runs both modes against independent copies of the same inode graph
    /// with the built-in library (and the vector's own profiles).
    pub fn check_builtin(&self) -> Result<Checked, String> {
        self.check_with(&self.builtins()?)
    }

    /// [`Self::check_builtin`] with `library` in place of the built-ins
    /// when no configuration is given: how a mutated profile is tried
    /// against an unchanged vector.
    pub fn check(&self, library: &Library) -> Result<(), String> {
        let builtins = if self.user_profiles.is_empty() {
            library.clone()
        } else {
            self.builtins()?
        };
        self.check_with(&builtins).map(|_| ())
    }

    /// The expectations are independent data: no expected path is derived
    /// from diskgc's report.
    fn check_with(&self, builtins: &Library) -> Result<Checked, String> {
        let library = self.library(builtins.clone());
        let planning = ModelHost::new(&self.host).map_err(|e| e.to_string())?;
        let dry_run = self.run(&planning, true, builtins)?;
        compare_plan(&dry_run, &self.expect.plan).map_err(|e| {
            format!(
                "{e}\n(dry-run exit {}, errors {:?})",
                dry_run.exit_code(),
                dry_run.errors
            )
        })?;
        compare_exit("dry-run", &dry_run, self.expect.dry_run_exit)?;
        compare_errors("dry-run", &dry_run, &self.expect.dry_run_errors)?;
        if let Some(subset) = &self.expect.dry_run_report {
            json_subset(subset, &json(&dry_run)?, "dry_run_report")?;
        }
        for op in [
            Op::Unlinkat,
            Op::Fchmod,
            Op::RunCommand,
            Op::NixRoots,
            Op::NixGc,
        ] {
            if planning.trace().iter().any(|call| call.op == op) {
                return Err(format!("dry-run called {op}"));
            }
        }
        if let Some(expected) = &self.expect.dry_run_nix_commands {
            let actual = nix_commands(&planning);
            if &actual != expected {
                return Err(format!(
                    "dry-run nix_commands {actual:#?}, expected {expected:#?}"
                ));
            }
        }

        self.check_check()?;

        let reclaiming = ModelHost::new(&self.host).map_err(|e| e.to_string())?;
        // Both lists name paths that existed before reclaim, so a typo is
        // an error, not a vacuous postcondition. A preserved path may also
        // be one an event makes (a race's replacement or new marker).
        let made = self.paths_events_make();
        for (what, paths) in [
            ("removed", &self.expect.removed),
            ("preserved", &self.expect.preserved),
        ] {
            for path in paths {
                let by_event = what == "preserved" && made.iter().any(|m| path.starts_with(m));
                if reclaiming.stat_path(path).is_none() && !by_event {
                    return Err(format!(
                        "{what} path {} was absent before reclaim (and no event makes it)",
                        path.display()
                    ));
                }
            }
        }
        let reclaim = self.run(&reclaiming, false, builtins)?;
        compare_exit("reclaim", &reclaim, self.expect.reclaim_exit)?;
        compare_errors("reclaim", &reclaim, &self.expect.errors)?;
        let actions = sorted(reclaim.actions.iter().map(|e| e.candidate.clone()));
        let expected_actions = sorted(self.expect.actions.iter().cloned());
        if actions != expected_actions {
            return Err(format!(
                "reclaim actions {actions:?}, expected {expected_actions:?}"
            ));
        }
        compare_skips(&reclaim, &self.expect.skipped)?;
        if let Some(subset) = &self.expect.report {
            json_subset(subset, &json(&reclaim)?, "report")?;
        }
        if library.is_none()
            && (dry_run.exit_code() != 2 || reclaim.exit_code() != 2 || !reclaim.actions.is_empty())
        {
            return Err(
                "the configuration or built-ins do not load under the catalog, yet the run did \
                 not refuse with exit 2"
                    .to_owned(),
            );
        }
        let library = library.as_ref();
        self.check_filesystem(&reclaiming, &reclaim, library)?;
        self.check_commands(&reclaiming, &reclaim, library)?;
        if !reclaiming.unfired_events().is_empty() {
            return Err(format!("unfired events: {:?}", reclaiming.unfired_events()));
        }
        let builtin = library
            .into_iter()
            .flat_map(Library::profiles)
            .filter(|profile| profile.origin == Origin::BuiltIn)
            .map(|profile| profile.name.clone())
            .collect();
        Ok(Checked {
            dry_run,
            reclaim,
            builtin,
        })
    }

    /// `check`'s root warnings on a fresh host, when the configuration
    /// loads under the vector's catalog: compared with `check_warnings`
    /// when given; always read-only. A call that reaches a `dead` mount
    /// fails the test inside the host model.
    fn check_check(&self) -> Result<(), String> {
        let Some(text) = &self.config else {
            return Ok(());
        };
        let Ok(config) = Catalog::from(self.catalog).config(
            Path::new(CONFIG_PATH),
            text.as_bytes(),
            ConfigUse::Run,
        ) else {
            return match self.expect.check_warnings {
                Some(_) => Err("check_warnings given, but the configuration does not load".into()),
                None => Ok(()),
            };
        };
        let host = ModelHost::new(&self.host).map_err(|e| e.to_string())?;
        let warnings = invocation::check_roots(&host, &config);
        for op in [
            Op::Unlinkat,
            Op::Fchmod,
            Op::RunCommand,
            Op::NixRoots,
            Op::NixGc,
        ] {
            if host.trace().iter().any(|call| call.op == op) {
                return Err(format!("check called {op}"));
            }
        }
        let Some(expected) = &self.expect.check_warnings else {
            return Ok(());
        };
        let matches = warnings.len() == expected.len()
            && warnings
                .iter()
                .zip(expected)
                .all(|(got, want)| got.contains(want.as_str()));
        if matches {
            Ok(())
        } else {
            Err(format!(
                "check warnings {warnings:#?}, expected (contained, in order) {expected:#?}"
            ))
        }
    }

    /// Paths an event creates, renames to, replaces or mounts on.
    fn paths_events_make(&self) -> Vec<PathBuf> {
        self.host
            .events
            .iter()
            .flat_map(|event| {
                [
                    event.create.clone(),
                    event.mkdir.clone(),
                    event.rename.as_ref().map(|r| r.to.clone()),
                    event.replace.clone(),
                    event.replace_reusing_inode.clone(),
                    event.mount.as_ref().map(|m| m.at.clone()),
                ]
            })
            .flatten()
            .collect()
    }

    /// The reclaim run's call trace, one line per host call: for writing
    /// and debugging a vector's events (`VECTOR_TRACE`, docs/testing.md).
    pub fn reclaim_trace(&self) -> Result<Vec<String>, String> {
        let host = ModelHost::new(&self.host).map_err(|e| e.to_string())?;
        self.run(&host, false, &self.builtins()?)?;
        Ok(host.trace().iter().map(ToString::to_string).collect())
    }

    /// `removed` and `preserved` against the model after reclaim; every
    /// unlink inside an action; every deleting action with a postcondition.
    fn check_filesystem(
        &self,
        host: &ModelHost,
        reclaim: &Report,
        library: Option<&Library>,
    ) -> Result<(), String> {
        let actions = &self.expect.actions;
        for path in &self.expect.removed {
            if !actions.iter().any(|action| path.starts_with(action)) {
                return Err(format!(
                    "removed path {} is outside every action",
                    path.display()
                ));
            }
            if host.stat_path(path).is_some() {
                return Err(format!("removed path {} still exists", path.display()));
            }
        }
        for entry in &reclaim.actions {
            let deletes = matches!(
                action_of(library, &entry.profile)?,
                Action::Delete { .. } | Action::CargoLockProtocol
            );
            if deletes
                && !self
                    .expect
                    .removed
                    .iter()
                    .any(|path| path.starts_with(&entry.candidate))
            {
                return Err(format!(
                    "action {} has no explicit removed-path postcondition",
                    entry.candidate.display()
                ));
            }
        }
        for path in &self.expect.preserved {
            if host.stat_path(path).is_none() {
                return Err(format!("preserved path {} disappeared", path.display()));
            }
        }
        for call in host.trace().iter().filter(|call| call.op == Op::Unlinkat) {
            let Some(path) = call.path.as_deref() else {
                return Err(format!("unlinkat without a path: {call}"));
            };
            if !actions.iter().any(|candidate| path.starts_with(candidate)) {
                return Err(format!("unlinkat outside expected actions: {path:?}"));
            }
        }
        Ok(())
    }

    /// Every command run belongs to a command or backend action, a command
    /// action's in its checked candidate or that candidate's parent (PRD,
    /// What bad looks like), a Docker or Podman action's of its own client,
    /// and `expect.commands` when given.
    fn check_commands(
        &self,
        host: &ModelHost,
        reclaim: &Report,
        library: Option<&Library>,
    ) -> Result<(), String> {
        let runs: Vec<_> = host
            .trace()
            .into_iter()
            .filter(|call| call.op == Op::RunCommand)
            .collect();
        // Pair every run with exactly one action it may belong to: a
        // command action's run in its candidate or that candidate's
        // parent, a backend action's run of its client (`None`: a command
        // action). An action runs at most once; a run left over is a
        // command diskgc had no checked candidate for.
        let mut unpaired: Vec<(&PathBuf, Option<&str>)> = Vec::new();
        let mut nix_actions = 0;
        for entry in &reclaim.actions {
            match action_of(library, &entry.profile)? {
                Action::Command { .. } => unpaired.push((&entry.candidate, None)),
                Action::DockerBuilderPrune | Action::DockerImagePrune { .. } => {
                    unpaired.push((&entry.candidate, Some("docker")));
                }
                Action::PodmanImagePrune { .. } => {
                    unpaired.push((&entry.candidate, Some("podman")));
                }
                Action::NixStoreGc => nix_actions += 1,
                Action::Delete { .. } | Action::CargoLockProtocol => {}
            }
        }
        for call in &runs {
            let run = call
                .command
                .as_ref()
                .ok_or_else(|| format!("run_command without its request: {call}"))?;
            let program = run.executable.file_name();
            let cwd = call.path.as_deref();
            let pair = unpaired
                .iter()
                .position(|(candidate, client)| match client {
                    None => {
                        cwd.is_some()
                            && (cwd == Some(candidate.as_path()) || cwd == candidate.parent())
                    }
                    Some(client) => program.is_some_and(|name| name == *client),
                });
            match pair {
                Some(index) => {
                    unpaired.remove(index);
                }
                None => {
                    return Err(format!(
                        "command run {call} ({:?}) belongs to no command or backend action",
                        run.executable
                    ));
                }
            }
        }
        if let Some((candidate, _)) = unpaired.iter().find(|(_, client)| client.is_none()) {
            return Err(format!(
                "command action {} has no run in it or its parent",
                candidate.display()
            ));
        }
        // A Nix action is its one GC; a GC is a Nix action's.
        let gcs = host
            .trace()
            .iter()
            .filter(|call| call.op == Op::NixGc)
            .count();
        if gcs != nix_actions {
            return Err(format!(
                "{gcs} Nix GC run(s) for {nix_actions} nix-store action(s): each action is \
                 exactly one GC"
            ));
        }
        if let Some(expected) = &self.expect.nix_commands {
            let actual = nix_commands(host);
            if &actual != expected {
                return Err(format!("nix_commands {actual:#?}, expected {expected:#?}"));
            }
        }
        if let Some(expected) = &self.expect.commands {
            let actual: Vec<CommandVerdict> = runs
                .iter()
                .map(|call| {
                    let run = call
                        .command
                        .as_ref()
                        .expect("run_command records its request");
                    CommandVerdict {
                        cwd: call.path.clone().unwrap_or_default(),
                        argv: std::iter::once(run.executable.as_os_str().to_owned())
                            .chain(run.arguments.iter().cloned())
                            .map(|arg: OsString| arg.to_string_lossy().into_owned())
                            .collect(),
                    }
                })
                .collect();
            if &actual != expected {
                return Err(format!("commands {actual:#?}, expected {expected:#?}"));
            }
        }
        Ok(())
    }

    pub fn expected_actions(&self) -> &[PathBuf] {
        &self.expect.actions
    }

    pub fn expected_preserved(&self) -> &[PathBuf] {
        &self.expect.preserved
    }

    /// A dry run and a reclaim of the vector, each on a fresh copy of its
    /// host, with the built-in library; `paths` are the paths to report
    /// absent after the reclaim. Its `expect` is not consulted.
    pub fn outcomes(&self, paths: &[PathBuf]) -> Result<Outcomes, String> {
        let builtins = self.builtins()?;
        let planning = ModelHost::new(&self.host).map_err(|e| e.to_string())?;
        let dry_run = self.run(&planning, true, &builtins)?;
        let reclaiming = ModelHost::new(&self.host).map_err(|e| e.to_string())?;
        let reclaim = self.run(&reclaiming, false, &builtins)?;
        Ok(Outcomes {
            eligible: dry_run
                .plan
                .iter()
                .filter(|entry| entry.reason.is_none())
                .filter_map(|entry| entry.candidate.clone())
                .collect(),
            actions: reclaim
                .actions
                .iter()
                .map(|action| action.candidate.clone())
                .collect(),
            removed: paths
                .iter()
                .filter(|path| reclaiming.stat_path(path).is_none())
                .cloned()
                .collect(),
            reclaim_events_fired: reclaiming.unfired_events().is_empty(),
        })
    }

    /// The one profile of a `profile:` vector.
    pub fn profile_name(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    pub fn root_path(&self) -> &Path {
        &self.path
    }
}

/// `profile`'s action in the run's library; an error when there is none
/// (a refused run has no actions to ask about).
fn action_of<'l>(library: Option<&'l Library>, profile: &str) -> Result<&'l Action, String> {
    let library = library.ok_or_else(|| format!("action of `{profile}` in a refused run"))?;
    match library.get(profile).map(|p| &p.body) {
        Some(Body::Source(source)) => Ok(&source.action),
        _ => Err(format!("action of unknown source profile `{profile}`")),
    }
}

fn sorted<T: Ord>(items: impl Iterator<Item = T>) -> Vec<T> {
    let mut items: Vec<T> = items.collect();
    items.sort();
    items
}

fn compare_exit(mode: &str, report: &Report, expected: i32) -> Result<(), String> {
    let exit = i32::from(report.exit_code());
    if exit != expected {
        return Err(format!(
            "{mode} exit {exit}, expected {expected}; errors: {:?}",
            report.errors
        ));
    }
    Ok(())
}

fn compare_errors(mode: &str, report: &Report, expected: &[ErrorVerdict]) -> Result<(), String> {
    let matches = report.errors.len() == expected.len()
        && report.errors.iter().zip(expected).all(|(error, want)| {
            error.cause.as_str() == want.cause
                && want
                    .contains
                    .as_ref()
                    .is_none_or(|text| error.message.contains(text.as_str()))
        });
    if !matches {
        return Err(format!(
            "{mode} errors {:#?}, expected {expected:#?}",
            report.errors
        ));
    }
    Ok(())
}

fn compare_plan(report: &Report, expected: &[PlanVerdict]) -> Result<(), String> {
    let mut actual: Vec<_> = report
        .plan
        .iter()
        .map(|entry| PlanVerdict {
            candidate: entry.candidate.clone(),
            reason: entry.reason.map(|reason| reason.as_str().to_owned()),
            source: entry.build_evidence.as_ref().map(|e| e.source.clone()),
        })
        .collect();
    actual.sort_by(|a, b| a.candidate.cmp(&b.candidate));
    let mut expected = expected.to_vec();
    expected.sort_by(|a, b| a.candidate.cmp(&b.candidate));
    if actual != expected {
        return Err(format!("dry-run plan {}", difference(&actual, &expected)));
    }
    Ok(())
}

/// What `actual` has that `expected` lacks, and the reverse: the readable
/// part of a mismatch between two long lists.
fn difference<T: PartialEq + std::fmt::Debug>(actual: &[T], expected: &[T]) -> String {
    let only = |a: &[T], b: &[T]| -> Vec<String> {
        a.iter()
            .filter(|x| !b.contains(x))
            .map(|x| format!("{x:?}"))
            .collect()
    };
    format!(
        "differs\n  only actual:\n    {}\n  only expected:\n    {}",
        only(actual, expected).join("\n    "),
        only(expected, actual).join("\n    ")
    )
}

fn compare_skips(report: &Report, expected: &[SkipVerdict]) -> Result<(), String> {
    let mut actual: Vec<_> = report
        .skipped
        .iter()
        .map(|entry| SkipVerdict {
            candidate: entry.candidate.clone(),
            reason: entry.reason.as_str().to_owned(),
            process_blockers: entry.process_blockers.as_ref().map(|blockers| {
                sorted(blockers.iter().map(|blocker| BlockerVerdict {
                    pid: blocker.pid,
                    evidence: blocker.evidence.as_str().to_owned(),
                }))
            }),
        })
        .collect();
    // The contract has `null` or a nonempty list, never `[]`.
    if let Some(empty) = actual
        .iter()
        .find(|skip| skip.process_blockers.as_ref().is_some_and(Vec::is_empty))
    {
        return Err(format!(
            "{}: process_blockers is an empty list, not null",
            empty
                .candidate
                .as_deref()
                .unwrap_or(std::path::Path::new("(tier)"))
                .display()
        ));
    }
    actual.sort_by(|a, b| a.candidate.cmp(&b.candidate));
    let mut expected = expected.to_vec();
    expected.sort_by(|a, b| a.candidate.cmp(&b.candidate));
    let same = actual.len() == expected.len()
        && actual.iter().zip(&expected).all(|(got, want)| {
            got.candidate == want.candidate
                && got.reason == want.reason
                && want.process_blockers.as_ref().is_none_or(|blockers| {
                    sorted(blockers.iter().cloned())
                        == got.process_blockers.clone().unwrap_or_default()
                })
        });
    if !same {
        // Compare like with like: blockers only where the vector gives them.
        let shown: Vec<_> = actual
            .iter()
            .map(|got| {
                let compared = expected
                    .iter()
                    .any(|want| want.candidate == got.candidate && want.process_blockers.is_some());
                SkipVerdict {
                    process_blockers: got.process_blockers.clone().filter(|_| compared),
                    ..got.clone()
                }
            })
            .collect();
        return Err(format!("reclaim skipped {}", difference(&shown, &expected)));
    }
    Ok(())
}

fn json(report: &Report) -> Result<Value, String> {
    serde_json::from_str(&report.to_json()).map_err(|e| format!("report JSON: {e}"))
}

/// `expected` is contained in `actual`: objects by key, recursively;
/// arrays of equal length elementwise; scalars equal.
fn json_subset(expected: &Value, actual: &Value, at: &str) -> Result<(), String> {
    match (expected, actual) {
        (Value::Object(want), Value::Object(got)) => {
            for (key, value) in want {
                let found = got
                    .get(key)
                    .ok_or_else(|| format!("{at}.{key}: missing in {actual}"))?;
                json_subset(value, found, &format!("{at}.{key}"))?;
            }
            Ok(())
        }
        (Value::Array(want), Value::Array(got)) if want.len() == got.len() => want
            .iter()
            .zip(got)
            .enumerate()
            .try_for_each(|(i, (w, g))| json_subset(w, g, &format!("{at}[{i}]"))),
        _ if expected == actual => Ok(()),
        _ => Err(format!("{at}: {actual}, expected {expected}")),
    }
}

/// Every vector file below `roots`, sorted: `*.yaml` up to
/// [`CORPUS_DEPTH_MAX`] directories deep. Anything else there is an error,
/// so a misnamed vector is not silently skipped.
pub fn corpus_files(roots: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let mut pending: Vec<(PathBuf, usize)> = roots.iter().map(|r| (r.clone(), 0)).collect();
    while let Some((directory, depth)) = pending.pop() {
        let entries =
            std::fs::read_dir(&directory).map_err(|e| format!("{}: {e}", directory.display()))?;
        for entry in entries {
            let path = entry
                .map_err(|e| format!("{}: {e}", directory.display()))?
                .path();
            if path.is_dir() {
                if depth + 1 >= CORPUS_DEPTH_MAX {
                    return Err(format!("{}: corpus nested too deep", path.display()));
                }
                pending.push((path, depth + 1));
            } else if path.extension().is_some_and(|ext| ext == "yaml") {
                files.push(path);
            } else if path.file_name().is_some_and(|name| name != "README.md") {
                return Err(format!("{}: not a .yaml vector", path.display()));
            }
        }
    }
    files.sort();
    let unique: BTreeSet<_> = files.iter().collect();
    assert_eq!(unique.len(), files.len(), "overlapping corpus roots");
    Ok(files)
}
