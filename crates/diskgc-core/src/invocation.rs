//! `ensure`'s composition, after the command line is parsed and the
//! configuration file read: the configuration's text under the build's
//! catalog, static checks of the configuration and the selection,
//! privileges, owners, protections, the tiers in policy order, then the
//! executor. Generic over the `Host` port so the binary (`LinuxHost`) and
//! the model tests (`ModelHost`) run the same decisions; before TASK-121
//! this lived in the binary and each model test that needed a
//! configuration re-assembled a copy of it.
//!
//! What stays in the binary: argument syntax, host startup (the kernel
//! check), the configuration lookup and its file read (passed in as
//! [`ConfigFile`]), and the build's [`Catalog`] and random seed, passed in
//! as [`Platform`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{self, Config, ConfigError, IpcPolicy, Owner, Problems, Ranking, RootEntry};
use crate::engine::Checkouts;
use crate::executor::{self, CompiledSelection, Request, SelectionFailure, Tier};
use crate::host::{Delete, Host, MountEntry};
use crate::profile::{Body, Finds, Library, Release, Roots, Source};
use crate::report::{ErrorCause, ErrorEntry, Report, Status};
use crate::resolve::{MountScreenError, ResolveError, Want, resolve_in};
use crate::select;
use crate::snapshot::RunIndex;
use crate::units::Size;

/// The built-in catalog a build ships: every built-in (development
/// builds), or the proposed 0.1 release's, derived from builtin.yaml's
/// `releases:` ([`Library::proposed_release_01`]). A build-time fact of
/// the binary (its `proposed-release-01-catalog` feature), never an
/// operator's choice; explicit here so a model vector can run either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catalog {
    Development,
    ProposedRelease01,
}

/// What a configuration is parsed for: `profiles test` reports example
/// mismatches as test failures instead of refusing the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigUse {
    Run,
    ProfileTest,
}

impl Catalog {
    /// `builtins` narrowed to this catalog.
    pub fn library(self, builtins: Library) -> Result<Library, String> {
        match self {
            Self::Development => Ok(builtins),
            Self::ProposedRelease01 => builtins.proposed_release_01(),
        }
    }

    /// A configuration file's text under this catalog: what `ensure`,
    /// `profiles` and `check` all load. The one place a configuration is
    /// held to the catalog.
    pub fn config(self, path: &Path, bytes: &[u8], use_: ConfigUse) -> Result<Config, String> {
        let mut config = match use_ {
            ConfigUse::Run => config::parse(path, bytes),
            ConfigUse::ProfileTest => config::parse_for_profile_test(path, bytes),
        }
        .map_err(|e| e.to_string())?;
        if self == Self::ProposedRelease01 {
            enforce_proposed_release_01_config(&mut config).map_err(|e| e.to_string())?;
        }
        Ok(config)
    }
}

/// Recheck every configured tier against the narrowed catalog. Config
/// parsing checked these expressions against the full embedded library;
/// simply replacing that library would leave a previously checked Docker
/// or command selection in a later tier. `--select` is checked separately
/// against the same narrowed library by `Tier::select`.
///
/// A checkout policy may only narrow under the 0.1 catalog: `allow` on a
/// built-in that refuses checkouts (nix-shell-rc: a legacy shell's rc
/// directory is its TMPDIR, so a clone made there is what the guard
/// protects) would widen what the reviewed built-in deletes.
///
/// Every refusal at once (TASK-146, TASK-164), each at its line: an
/// operator adapting a development example should not learn them one run
/// at a time. User profiles are refused here too, and the tiers are checked
/// against the 0.1 built-ins alone, so a tier that names a user profile is
/// reported in the same run as the profile.
fn enforce_proposed_release_01_config(config: &mut Config) -> Result<(), ConfigError> {
    let builtins =
        Library::builtin().map_err(|e| ConfigError::whole_file(&config.path, e.to_string()))?;
    let user = config.library.user_profile_names();
    // With user profiles the configuration is refused anyway; its tiers are
    // then judged against the plain built-ins, so an `override: true` does
    // not also hide the built-in it replaced (from the tiers and from the
    // scope said below). Without, the configuration's library carries its
    // `profile_policies`, which a release run keeps.
    let library = if user.is_empty() {
        config.library.clone()
    } else {
        builtins.clone()
    }
    .release_01_built_ins()
    .map_err(|why| ConfigError::whole_file(&config.path, why))?;
    let mut refused = Problems::default();
    for name in &user {
        refused.push(
            config.user_profile_lines.get(name).copied(),
            format!(
                "profiles: `{name}` is a user profile; diskgc {} runs only its built-ins \
                 (an `override: true` of a built-in counts too): remove it",
                Release::V0_1.name()
            ),
        );
    }
    // Tiers are checked through `ShortRefusals`, so the release's scope is
    // said once below instead of after every refused part (TASK-146 walk).
    let short = ShortRefusals {
        library: &library,
        user: &user,
    };
    let not_shipped = |name: &str| {
        select::Catalog::withheld(&short, name)
            .unwrap_or_else(|| format!("`{name}` is not in this release's catalog"))
    };
    for policy in &config.profile_policies {
        if library.get(&policy.profile).is_none() {
            refused.push(
                policy.line,
                format!("profile_policies: {}", not_shipped(&policy.profile)),
            );
        } else if policy.checkouts.0 == Checkouts::Allow
            && builtins.checkouts(&policy.profile) == Some(Checkouts::Refuse)
        {
            refused.push(
                policy.line,
                format!(
                    "profile_policies: `checkouts: allow` for `{}` is refused: the built-in \
                     refuses checkouts, and a policy may only narrow it in this release",
                    policy.profile
                ),
            );
        }
    }
    for group in &config.groups {
        for tier in &group.tiers {
            if let Err(e) = select::check(tier.select.expr().clone(), &short) {
                refused.push(
                    tier.line,
                    format!("group `{}` tier `{}`: {e}", group.name, tier.name),
                );
            }
        }
    }
    for protected in &config.protect {
        if library.get(&protected.profile).is_none() {
            refused.push(
                protected.line,
                format!("protect: {}", not_shipped(&protected.profile)),
            );
        }
    }
    refused.into_result(&config.path, Some(library.release_scope()))?;
    config.library = library;
    Ok(())
}

/// A release library whose "not shipped" refusals leave out the release's
/// scope, which the caller states once for all of them, and that names a
/// configuration's user profiles as such rather than as unknown names.
struct ShortRefusals<'a> {
    library: &'a Library,
    user: &'a [String],
}

impl select::Catalog for ShortRefusals<'_> {
    type Action = <Library as select::Catalog>::Action;

    fn lookup(&self, name: &str) -> Option<select::Entry<'_, Self::Action>> {
        self.library.lookup(name)
    }

    fn names(&self) -> Vec<&str> {
        self.library.names()
    }

    fn withheld(&self, name: &str) -> Option<String> {
        if self.user.iter().any(|u| u == name) {
            return Some(format!(
                "`{name}` is a user profile, which diskgc {} refuses",
                Release::V0_1.name()
            ));
        }
        // Said inside a selection's list of problems: no full stop.
        self.library
            .not_shipped(name)
            .map(|why| why.trim_end_matches('.').to_owned())
    }
}

/// The configuration file as the binary's lookup left it (docs/reference.md,
/// Configuration file). A file that was given but is missing, or that
/// cannot be read, is the lookup's error, never `NotFound`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigFile {
    /// Read: its path and its bytes, not yet parsed.
    Read { path: PathBuf, bytes: Vec<u8> },
    /// The search found none; the paths it tried, for the refusal.
    NotFound { tried: Vec<PathBuf> },
}

/// "no configuration file found (tried ...)": `check`'s refusal, and the
/// start of `ensure`'s.
pub fn not_found(tried: &[PathBuf]) -> String {
    format!(
        "no configuration file found (tried {})",
        tried
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// `ensure`'s arguments, checked for syntax (docs/reference.md, `ensure`),
/// without the configuration lookup and output flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsureOptions {
    pub min_free: Size,
    pub target_free: Option<Size>,
    /// `None`: `--timeout 0`, no deadline.
    pub timeout: Option<Duration>,
    /// `None`: `--tally-budget 0`, no limit.
    pub tally_budget: Option<Duration>,
    pub select: Option<String>,
    pub roots: Vec<String>,
    pub dry_run: bool,
    pub allow_root: bool,
    /// `--no-index`: the run neither reads nor writes the index snapshot
    /// (docs/architecture.md, The tally: `--no-index`).
    pub no_index: bool,
    pub path: PathBuf,
    /// A test seam, not an option: the eligibility walk's entry budget in
    /// place of [`crate::engine::DEFAULT_MAX_ENTRIES`], so a model vector
    /// can reach `too_many_entries` with a small tree. The binary always
    /// passes `None`; no flag or configuration key sets it (pinned by
    /// `the_binary_never_sets_the_test_entry_budget` in `main.rs`).
    pub test_entry_budget: Option<usize>,
}

impl EnsureOptions {
    /// The report of a run of these options before anything ran: `PATH`,
    /// `--dry-run` and the goal as far as the command line says it. Every
    /// refusal before the executor (here and in the binary's startup)
    /// starts from it, so a host failure at startup still carries the goal
    /// a harness checks (TASK-191).
    pub fn report(&self, status: Status) -> Report {
        Report::new(status, Some(&self.path), self.dry_run)
            .with_goal(self.min_free, self.target_free)
    }
}

/// What both `ensure` and `watch` take from the command line besides the
/// goal: `PATH`, `--select`, `--roots`, `--allow-root` (and the model's
/// entry-budget seam, [`EnsureOptions::test_entry_budget`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub path: PathBuf,
    pub select: Option<String>,
    pub roots: Vec<String>,
    pub allow_root: bool,
    pub test_entry_budget: Option<usize>,
}

impl EnsureOptions {
    /// What [`prepare`] takes from these options.
    pub fn selection(&self) -> Selection {
        Selection {
            path: self.path.clone(),
            select: self.select.clone(),
            roots: self.roots.clone(),
            allow_root: self.allow_root,
            test_entry_budget: self.test_entry_budget,
        }
    }
}

/// What `ensure` needs from the build and its environment besides the
/// `Host` port. Each function is called only when the run needs it.
pub struct Platform<'a> {
    /// The build's catalog: the configuration and the built-ins are held
    /// to it here, so a model vector runs what a release build runs.
    pub catalog: Catalog,
    /// The seed of a `ranking: random` run (`DISKGC_SEED` or fresh).
    pub draw_seed: &'a dyn Fn() -> Result<u64, String>,
    /// The built-in profiles before [`Self::catalog`] narrows them: the
    /// library `--select` uses when no configuration file was found. The
    /// binary's is the embedded builtin.yaml; a model test may add or
    /// mutate profiles (`user_profiles`, mutation tests).
    pub builtins: &'a dyn Fn() -> Result<Library, String>,
}

/// Runs `ensure` (or its dry run) from what [`load`] checked; every error
/// is in the report. Every refusal before the executor (privileges, a
/// static check for the user, the in-use check's visibility, a supplied
/// root missing) is exit 2, or 3 for a host read, and deletes nothing
/// ([`refused`]). `log` receives the
/// diagnostics (`error: ...`), `progress` the executor's progress lines.
/// `loaded` is [`load`] of `options.selection()`; `options` gives the goal.
pub fn ensure<H: Delete>(
    host: &H,
    options: &EnsureOptions,
    loaded: Loaded,
    platform: &Platform<'_>,
    log: &mut dyn FnMut(&str),
    progress: &mut dyn FnMut(&str),
) -> Report {
    debug_assert_eq!(
        loaded.selection(),
        &options.selection(),
        "loaded from these options"
    );
    match run(host, options, loaded, platform, progress) {
        Ok(report) => report,
        Err((cause, why)) => refused(options, cause, why, log),
    }
}

/// The report of a refusal before the executor (by [`load`] or by
/// [`ensure`]), logged: it deletes nothing and is exit 2, or 3 when a
/// host read failed ([`refusal_status`]).
pub fn refused(
    options: &EnsureOptions,
    cause: ErrorCause,
    why: String,
    log: &mut dyn FnMut(&str),
) -> Report {
    log(&format!("error: {why}"));
    let mut report = options.report(refusal_status(cause));
    report.errors.push(ErrorEntry::run(cause, why));
    report
}

/// The status of a refusal before the executor (`ensure`'s and `watch`'s
/// start): `error` (exit 3) when a host read failed (`host`: diskgc's own
/// capability set), `usage_error` (exit 2) for every other cause.
pub fn refusal_status(cause: ErrorCause) -> Status {
    if cause == ErrorCause::Host {
        Status::Error
    } else {
        Status::UsageError
    }
}

/// [`ensure`], with a refusal before the executor as its error.
fn run<H: Delete>(
    host: &H,
    options: &EnsureOptions,
    loaded: Loaded,
    platform: &Platform<'_>,
    progress: &mut dyn FnMut(&str),
) -> Result<Report, (ErrorCause, String)> {
    let prepared = prepare(host, loaded, platform)?;
    prepared.refuse_missing_roots(host)?;
    // The in-use check's visibility (TASK-263), where a tier relies on that
    // check. A reclaim asks the pre-release sandbox gate first: where it is
    // closed the executor names the gate (exit 2), wherever diskgc runs,
    // its own PID namespace included. A dry run is refused where its
    // reclaim would be. The call's own errors come before the
    // environment's: a missing root (above) or `PATH` is named first.
    if prepared.uses_in_use_scan() && (options.dry_run || host.may_delete().is_ok()) {
        executor::reach_path(host, &options.path).map_err(|failure| match failure {
            executor::MeasureFailure::Usage(why) => (ErrorCause::Usage, why),
            executor::MeasureFailure::Host(why) => (ErrorCause::Host, why),
        })?;
        executor::process_visibility(host).map_err(|e| (e.cause, e.message))?;
    }
    let mut index = if options.no_index {
        RunIndex::no_index()
    } else {
        RunIndex::snapshot()
    };
    let request = prepared.request(
        options.min_free,
        options.target_free,
        options.timeout,
        options.tally_budget,
        None,
    );
    Ok(prepared.execute(host, &request, options.dry_run, &mut index, progress))
}

/// Everything `ensure` and `watch` decide from the command line and the
/// configuration before anything on the machine is measured: the
/// configuration under the catalog, privileges, owners, protections and
/// the tiers in policy order ([`prepare`]). Made once; a `watch` runs it
/// at every reclaim cycle ([`Prepared::execute`]).
#[derive(Debug, Clone)]
pub struct Prepared {
    /// `PATH`, `--select`, `--roots`, `--allow-root` ([`Loaded`]'s).
    selection: Selection,
    /// The `--select` tier, or the first configured one.
    tier: Tier,
    /// The configured tiers, their timeouts and the random seed; `None`
    /// under `--select` (the tier alone).
    configured: Option<(Vec<Tier>, Vec<Duration>, Option<u64>)>,
    owners: Vec<u32>,
    uid: u32,
    config: Option<Config>,
    protections: Vec<executor::ProtectedSource>,
    max_entries: usize,
}

impl Prepared {
    /// Whether a tier relies on diskgc's own in-use check: the `fs` kind
    /// (its `delete` and its command actions, rechecked against a scan
    /// before they start) and `cargo-target`. The Docker kinds leave
    /// liveness to their daemon, and are not refused where the scan
    /// cannot see every process (docs/safety.md, In use: Visibility).
    pub fn uses_in_use_scan(&self) -> bool {
        self.tiers().0.iter().any(|tier| {
            matches!(
                tier.source,
                executor::SourceKind::Fs(_) | executor::SourceKind::CargoTarget
            )
        })
    }

    /// The tiers a run takes, in policy order, with their own timeouts
    /// (zero: none).
    pub fn tiers(&self) -> (Vec<&Tier>, Vec<Duration>) {
        match &self.configured {
            Some((tiers, timeouts, _)) => (tiers.iter().collect(), timeouts.clone()),
            None => (vec![&self.tier], vec![Duration::ZERO]),
        }
    }

    /// The configuration file the decisions were read from, if any.
    pub fn config_path(&self) -> Option<&Path> {
        self.config.as_ref().map(|c| c.path.as_path())
    }

    /// The request of one run toward a goal: `--min-free`, `--target-free`,
    /// `--timeout`, `--tally-budget` and, for a `watch` cycle, what the
    /// cycle adds.
    pub fn request(
        &self,
        min_free: Size,
        target_free: Option<Size>,
        timeout: Option<Duration>,
        tally_budget: Option<Duration>,
        watch: Option<executor::WatchCycle>,
    ) -> Request {
        Request {
            path: self.selection.path.clone(),
            min_free,
            target_free,
            timeout,
            tally_budget,
            watch,
            tier: self.tier.clone(),
        }
    }

    /// The decisions again for one `watch` cycle, before its first
    /// deletion (docs/safety.md, Validation: Environmental, at the start of
    /// every reclaim): `owners:` names resolved again (a name gone or now
    /// a system uid refuses the cycle, cause `config`; a name now another
    /// uid is that uid, said in one line), and groups with `ranking:
    /// random` shuffled again with a fresh seed from `draw_seed` (a fixed
    /// `DISKGC_SEED` stays fixed: docs/reference.md, Tier fields).
    pub fn for_cycle<H: Delete>(
        &self,
        host: &H,
        draw_seed: &dyn Fn() -> Result<u64, String>,
        log: &mut dyn FnMut(&str),
    ) -> Result<Self, (ErrorCause, String)> {
        let mut next = self.clone();
        if self.selection.allow_root {
            let config = self.config.as_ref().expect("root mode has a config");
            let owners = resolved_owners(host, config).map_err(|why| (ErrorCause::Config, why))?;
            if owners != self.owners {
                log(&format!(
                    "owners: now resolve to uid(s) {owners:?} (were {:?})",
                    self.owners
                ));
            }
            next.owners = owners;
        }
        if let (Some(config), Some(_)) = (&self.config, &self.configured)
            && config.groups.iter().any(|g| g.ranking == Ranking::Random)
        {
            let seed = draw_seed().map_err(|why| (ErrorCause::Config, why))?;
            let (mut tiers, timeouts) =
                configured_tiers(config, self.uid, self.selection.allow_root, Some(seed))
                    .map_err(|why| (ErrorCause::Config, why))?;
            for tier in &mut tiers {
                tier.protections = self.protections.clone();
                tier.max_entries = self.max_entries;
            }
            next.tier = tiers[0].clone();
            next.configured = Some((tiers, timeouts, Some(seed)));
        }
        Ok(next)
    }

    /// A supplied root that does not exist refuses the run (exit 2,
    /// [`refuse_missing_roots`]).
    pub fn refuse_missing_roots<H: Host>(&self, host: &H) -> Result<(), (ErrorCause, String)> {
        refuse_missing_roots(host, self.config.as_ref(), &self.selection, self.uid)
    }

    /// Runs the executor on `request` ([`Self::request`]): the dry run, or
    /// the reclaim on `index` (docs/architecture.md, The reclaim walk).
    /// Every error is in the report.
    pub fn execute<H: Delete>(
        &self,
        host: &H,
        request: &Request,
        dry_run: bool,
        index: &mut RunIndex,
        progress: &mut dyn FnMut(&str),
    ) -> Report {
        let owners = &self.owners;
        let mut report = if let Some((tiers, timeouts, _)) = &self.configured {
            if dry_run {
                executor::dry_run_grouped(host, request, tiers, timeouts, owners, progress)
            } else {
                executor::ensure_grouped_with_index(
                    host, request, tiers, timeouts, owners, index, progress,
                )
            }
        } else if dry_run {
            // Through `Host` only: the dry run has no way to reach a deleting
            // method (the generic bound is `Delete`; this call does not use it).
            executor::dry_run(host, request, owners, progress)
        } else {
            executor::ensure_with_index(host, request, owners, index, progress)
        };
        report.seed = self.configured.as_ref().and_then(|(_, _, seed)| *seed);
        report
    }
}

/// Step 1 of `ensure`'s and `watch`'s start, which reads nothing on the
/// machine: the configuration file under the catalog (what `check`
/// reports), the static checks of the mode (`owners:` and `--allow-root`,
/// `ipc:`), and `--select`'s expression, parsed, checked against its
/// library and its roots bound ([`Tier::compile_selection`]). The binary
/// runs it before its host startup (the kernel and `/proc` checks), so a
/// bad configuration or expression is exit 2 whatever the host
/// (docs/reference.md, Watch start; Exit codes (`ensure`)). What depends
/// on the user diskgc runs as (privileges, `owners:` names, the tiers and
/// protections for that uid) is [`prepare`]'s, after it. `log` receives
/// the configuration's path and warnings, and the expression's.
pub fn load(
    selection: Selection,
    config_file: &ConfigFile,
    platform: &Platform<'_>,
    log: &mut dyn FnMut(&str),
) -> Result<Loaded, (ErrorCause, String)> {
    let options = &selection;
    let fail = |cause: ErrorCause, why: String| Err((cause, why));
    let config = match config_file {
        ConfigFile::Read { path, bytes } => Some(
            platform
                .catalog
                .config(path, bytes, ConfigUse::Run)
                .map_err(|why| (ErrorCause::Config, why))?,
        ),
        ConfigFile::NotFound { .. } if options.allow_root => {
            return fail(
                ErrorCause::Config,
                "--allow-root requires a configuration file with an owners: list".to_owned(),
            );
        }
        ConfigFile::NotFound { tried } if options.select.is_none() => {
            return fail(
                ErrorCause::Config,
                format!("{}; ensure without --select needs one", not_found(tried)),
            );
        }
        ConfigFile::NotFound { .. } => None,
    };
    if let Some(config) = &config {
        log(&format!("config {}", config.path.display()));
        for warning in &config.warnings {
            log(&format!("warning: {warning}"));
        }
        config
            .validate_mode(options.allow_root)
            .map_err(|why| (ErrorCause::Safety, why))?;
        if config.ipc == IpcPolicy::LiveOnly {
            return fail(
                ErrorCause::Safety,
                "ipc: live-only is valid in the configuration, but this version of ensure cannot \
                 enforce it yet; remove `ipc:` to use the default, block-any"
                    .to_owned(),
            );
        }
    }
    let select = match &options.select {
        None => None,
        Some(expr) => {
            // The configuration's library (its user profiles), else the
            // catalog's built-ins.
            let library = match &config {
                Some(config) => config.library.clone(),
                None => (platform.builtins)()
                    .and_then(|builtins| platform.catalog.library(builtins))
                    .map_err(|why| (ErrorCause::Config, why))?,
            };
            let roots = (!options.roots.is_empty()).then_some(options.roots.as_slice());
            let compiled =
                Tier::compile_selection(expr, &library, roots).map_err(
                    |failure| match failure {
                        SelectionFailure::Parse(message) => (ErrorCause::Parse, message),
                        SelectionFailure::Usage(message) => (ErrorCause::Usage, message),
                    },
                )?;
            for warning in compiled.warnings() {
                log(&format!("warning: --select {expr:?}: {warning}"));
            }
            Some(LoadedSelect { compiled, library })
        }
    };
    Ok(Loaded {
        selection,
        config,
        select,
    })
}

/// What [`load`] checked, for [`prepare`], with the selection it checked:
/// the one source of `PATH`, `--select`, `--roots` and `--allow-root`
/// from here on.
#[derive(Debug, Clone)]
pub struct Loaded {
    selection: Selection,
    config: Option<Config>,
    /// `None` without `--select`.
    select: Option<LoadedSelect>,
}

impl Loaded {
    /// The selection [`load`] checked.
    pub fn selection(&self) -> &Selection {
        &self.selection
    }
}

/// `--select`, compiled, and the library it was compiled against.
#[derive(Debug, Clone)]
struct LoadedSelect {
    compiled: CompiledSelection,
    library: Library,
}

/// The decisions of [`Prepared`] from what [`load`] checked, or the
/// refusal before the executor.
pub fn prepare<H: Delete>(
    host: &H,
    loaded: Loaded,
    platform: &Platform<'_>,
) -> Result<Prepared, (ErrorCause, String)> {
    let Loaded {
        selection,
        config: loaded_config,
        select,
    } = loaded;
    let options = &selection;
    let config = loaded_config.as_ref();
    let uid = executor::privileges(host, options.allow_root).map_err(|e| (e.cause, e.message))?;
    let owners = if options.allow_root {
        let config = config.expect("root mode requires config (refused by load)");
        resolved_owners(host, config).map_err(|why| (ErrorCause::Config, why))?
    } else {
        vec![uid]
    };
    let protections = match config {
        Some(config) => executor::bind_protections(config, uid, options.allow_root)
            .map_err(|why| (ErrorCause::Config, why))?,
        None => Vec::new(),
    };
    let mut configured = if select.is_none() {
        let config = config.expect("unselected ensure requires config (refused by load)");
        let seed = if config
            .groups
            .iter()
            .any(|group| group.ranking == Ranking::Random)
        {
            Some((platform.draw_seed)().map_err(|why| (ErrorCause::Config, why))?)
        } else {
            None
        };
        let (tiers, timeouts) = configured_tiers(config, uid, options.allow_root, seed)
            .map_err(|why| (ErrorCause::Config, why))?;
        Some((tiers, timeouts, seed))
    } else {
        None
    };
    // The walk's entry budget is run-wide, set beside the protections on
    // every tier: the product's constant unless a model test lowers it.
    let max_entries = options
        .test_entry_budget
        .unwrap_or(crate::engine::DEFAULT_MAX_ENTRIES);
    assert!(
        max_entries >= 1,
        "an entry budget of 0 would refuse every candidate"
    );
    if let Some((tiers, _, _)) = &mut configured {
        for tier in tiers {
            tier.protections = protections.clone();
            tier.max_entries = max_entries;
        }
    }
    let mut tier = match (&configured, &select) {
        (Some((tiers, _, _)), _) => tiers[0].clone(),
        (None, Some(select)) => {
            let tier = selected_tier(options, &select.compiled, &select.library, uid)?;
            validate_selected_root_action(options.allow_root, &tier)
                .map_err(|why| (ErrorCause::Safety, why))?;
            tier
        }
        (None, None) => unreachable!("load refuses a run without a configuration or --select"),
    };
    tier.protections = protections.clone();
    tier.max_entries = max_entries;
    Ok(Prepared {
        tier,
        configured,
        owners,
        uid,
        config: loaded_config,
        protections,
        max_entries,
        selection,
    })
}

/// A root the operator supplied (a tier's or a protect entry's `roots:`,
/// or `--roots`) that does not exist on this machine is a configuration
/// error (TASK-152, mark-emulator for Mark): exit 2 before anything is
/// planned or deleted, naming each such root and where it was written. A
/// forgotten `/home/example/src` used to read as one skipped candidate and
/// exit 0. Roots a profile fixes itself (`/tmp`, a user profile's
/// `find.roots`) may legitimately be absent and stay an empty match set.
///
/// Only [`RootFinding::Missing`] refuses here. A root the mount screen
/// refuses, or an unsafe, foreign or unreadable one, stays the executor's
/// per-tier report, and a mount table that cannot be read is left to the
/// executor's own host error (exit 3), not turned into a usage error. A
/// root removed after this check is still the executor's `missing` skip.
/// `check` previews the same decision ([`check_roots`]).
fn refuse_missing_roots<H: Host>(
    host: &H,
    config: Option<&Config>,
    options: &Selection,
    uid: u32,
) -> Result<(), (ErrorCause, String)> {
    // (root as written, where), in the order the operator wrote them.
    let mut supplied: Vec<(&str, String)> = Vec::new();
    if let Some(config) = config {
        // `--select` replaces the configured tiers, not `protect`.
        supplied.extend(configured_roots(config, options.select.is_none()));
    }
    supplied.extend(
        options
            .roots
            .iter()
            .map(|root| (root.as_str(), "--roots".to_owned())),
    );
    if supplied.is_empty() {
        return Ok(());
    }
    let Ok(mounts) = host.mountinfo() else {
        return Ok(());
    };
    let is_missing = |path: &Path| {
        matches!(
            root_finding(host, &mounts, path),
            Some(RootFinding::Missing)
        )
    };
    // A missing PATH is the executor's first refusal (`PATH ...:
    // missing`): the more basic mistake is reported first.
    if is_missing(&options.path) {
        return Ok(());
    }
    let mut from_config = false;
    let missing: Vec<String> = supplied
        .into_iter()
        .filter_map(|(root, at)| {
            let path = expand_uid(root, uid);
            let gone = is_missing(&path);
            from_config |= gone && at != "--roots";
            gone.then(|| format!("{} ({at})", path.display()))
        })
        .collect();
    let cause = if from_config {
        ErrorCause::Config
    } else {
        ErrorCause::Usage
    };
    match missing.as_slice() {
        [] => Ok(()),
        [one] => Err((
            cause,
            format!(
                "root {one} does not exist on this machine: set it to a directory that \
                 exists here, or remove it"
            ),
        )),
        many => Err((
            cause,
            format!(
                "{} roots do not exist on this machine (set each to a directory that exists \
                 here, or remove it):\n  {}",
                many.len(),
                many.join("\n  ")
            ),
        )),
    }
}

/// What `check` and `ensure` find about a supplied root before anything
/// plans with it: the one decision both make (TASK-194; before it `check`
/// had its own lookup, and TASK-178 had to be fixed twice).
#[derive(Debug, Clone, PartialEq, Eq)]
enum RootFinding {
    /// It does not exist here: ENOENT or ENOTDIR on the safe resolution
    /// the executor uses.
    Missing,
    /// Its mount chain is refused, so it was not looked up.
    NotLookedUp(MountScreenError),
}

/// Mounts first (docs/safety.md, Before the walk): a path whose mount chain
/// [`crate::resolve::screen_mount_chain`] refuses (a FUSE, network or untriggered automount
/// mount, PrivateTmp, an unclear mount) is never resolved, so it is never
/// "missing". Opening and `statx`ing a dead FUSE mount point hangs in the
/// kernel (TASK-178), and a lookup through an automount point mounts it
/// (TASK-180); the executor's preflight and protect discovery refuse such
/// a root from the same screen without touching it. `None`: the root is
/// there, or what is wrong with it is the executor's to report.
fn root_finding<H: Host>(host: &H, mounts: &[MountEntry], path: &Path) -> Option<RootFinding> {
    match resolve_in(host, mounts, path, Want::Directory) {
        Err(ResolveError::Screened { why, .. }) => Some(RootFinding::NotLookedUp(why)),
        Err(ResolveError::Missing { .. }) => Some(RootFinding::Missing),
        _ => None,
    }
}

/// The roots `config` supplies, as written, and where ("config PATH:
/// line N, tier `T`"), in [`Config::written_roots`]' order; a tier's only
/// when `tiers` (an
/// `ensure` with `--select` runs no configured tier).
fn configured_roots(config: &Config, tiers: bool) -> impl Iterator<Item = (&str, String)> {
    config
        .written_roots
        .iter()
        .filter(move |written| tiers || !matches!(written.entry, RootEntry::Tier(_)))
        .map(|written| {
            (
                written.root.as_str(),
                format!("config {}: {written}", config.path.display()),
            )
        })
}

/// A written root with `{uid}` expanded, as the executor will see it.
fn expand_uid(root: &str, uid: u32) -> PathBuf {
    PathBuf::from(root.replace("{uid}", &uid.to_string()))
}

/// `check`'s preview of [`refuse_missing_roots`] (TASK-152, TASK-194): an
/// advisory warning per configured root that `ensure` would refuse as
/// missing here, or that it will never open because its mount chain is
/// refused, plus one if the mount table cannot be read. `check` keeps its
/// exit code: packagers run it where the deployment's roots do not exist
/// (the NixOS module at build time, the flake's sandbox check). `{uid}` is
/// the caller's, as `ensure` will expand it. Read-only: nothing is opened
/// beyond the screened chain, nothing is written.
pub fn check_roots<H: Host>(host: &H, config: &Config) -> Vec<String> {
    let supplied: Vec<(&str, String)> = configured_roots(config, true).collect();
    if supplied.is_empty() {
        return Vec::new();
    }
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(why) => {
            return vec![format!(
                "cannot read the mount table ({why}); configured roots not checked"
            )];
        }
    };
    let uid = host.effective_uid();
    supplied
        .into_iter()
        .filter_map(|(root, at)| {
            let path = expand_uid(root, uid);
            Some(match root_finding(host, &mounts, &path)? {
                RootFinding::Missing => format!(
                    "root {} does not exist on this machine ({at}); ensure refuses this \
                     configuration here (exit 2)",
                    path.display()
                ),
                RootFinding::NotLookedUp(why) => format!(
                    "root {} is not looked up: {why} ({at}); ensure never opens it",
                    path.display()
                ),
            })
        })
        .collect()
}

/// Translate configured owners once before any planning or deletion. Two
/// names or entries resolving to the same uid hide a policy error.
fn resolved_owners<H: Delete>(host: &H, config: &Config) -> Result<Vec<u32>, String> {
    let entries = config
        .owners
        .as_ref()
        .ok_or("--allow-root requires an owners: list")?;
    let mut ids = Vec::with_capacity(entries.len());
    for owner in entries {
        let uid = match owner {
            Owner::Name(name) => resolve_owner(host, name)?,
            Owner::Uid(explicit) => explicit.uid,
        };
        if ids.contains(&uid) {
            return Err(format!("owners: entries resolve to duplicate uid {uid}"));
        }
        ids.push(uid);
    }
    Ok(ids)
}

/// An `owners:` name to its uid through the host's account database. A
/// name resolving to a system uid is refused as a `{uid: N}` entry is
/// (docs/reference.md, Configuration file: `owners`).
fn resolve_owner<H: Delete>(host: &H, name: &str) -> Result<u32, String> {
    let uid = host
        .account_uid(name)
        .map_err(|e| format!("owners: cannot resolve {name:?}: {e}"))?
        .ok_or_else(|| {
            format!(
                "owners: user {name:?} does not exist in this binary's account resolver \
                 (static musl builds cannot load LDAP/sssd NSS modules; use a trusted \
                 {{uid: N}} entry, a local passwd entry, or nscd)"
            )
        })?;
    if uid < config::SYSTEM_UID_END {
        return Err(format!(
            "owners: user {name:?} resolves to system uid {uid} (< {})",
            config::SYSTEM_UID_END
        ));
    }
    Ok(uid)
}

/// The tier `--select` makes for the user `uid`, from its compiled
/// expression ([`load`]) and the `library` it was compiled against.
fn selected_tier(
    options: &Selection,
    compiled: &CompiledSelection,
    library: &Library,
    uid: u32,
) -> Result<Tier, (ErrorCause, String)> {
    let selected =
        Tier::from_selection(compiled, library, uid).map_err(|failure| match failure {
            SelectionFailure::Parse(message) => (ErrorCause::Parse, message),
            SelectionFailure::Usage(message) => (ErrorCause::Usage, message),
        })?;
    if options.allow_root {
        reject_root_uid_selected(&selected, library, &options.roots)
            .map_err(|message| (ErrorCause::Safety, message))?;
    }
    Ok(selected)
}

/// A stable, small PRNG for Fisher-Yates tier shuffles. The seed is printed
/// so a run can be reproduced without relying on a library RNG version.
fn next_shuffle_word(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut word = *state;
    word = (word ^ (word >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    word = (word ^ (word >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    word ^ (word >> 31)
}

/// Compile every configured tier before any action. Unsupported execution
/// options are errors rather than silently changing policy order.
pub fn configured_tiers(
    config: &Config,
    uid: u32,
    allow_root: bool,
    seed: Option<u64>,
) -> Result<(Vec<Tier>, Vec<Duration>), String> {
    let mut tiers = Vec::new();
    let mut timeouts = Vec::new();
    let mut state = seed.unwrap_or_default();
    for group in &config.groups {
        let mut settings = group.tiers.iter().collect::<Vec<_>>();
        if group.ranking == Ranking::Random {
            if seed.is_none() {
                return Err(format!("group `{}` needs a random seed", group.name));
            }
            for end in (1..settings.len()).rev() {
                let pick = (next_shuffle_word(&mut state) % (end as u64 + 1)) as usize;
                settings.swap(end, pick);
            }
        }
        for setting in settings {
            let tier = Tier::configured(&group.name, setting, &config.library, uid)?;
            if allow_root {
                let supplied: Vec<String> = setting.roots.values().flatten().cloned().collect();
                reject_root_uid_selected(&tier, &config.library, &supplied)?;
                validate_selected_root_action(true, &tier)?;
            }
            tiers.push(tier);
            timeouts.push(setting.timeout);
        }
    }
    if tiers.is_empty() {
        return Err(format!(
            "config {}: groups contain no tiers",
            config.path.display()
        ));
    }
    Ok((tiers, timeouts))
}

/// Root mode must validate every source of a positive union. Checking only
/// the first would let a later term bind `{uid}` to root's uid 0.
fn reject_root_uid_selected(
    selected: &Tier,
    library: &Library,
    supplied_roots: &[String],
) -> Result<(), String> {
    let profiles: Vec<&str> = std::iter::once(selected.profile.as_str())
        .chain(
            selected
                .negated
                .iter()
                .map(|source| source.profile.as_str()),
        )
        .chain(selected.fs_union.iter().flat_map(|source| {
            std::iter::once(source.profile.as_str()).chain(
                source
                    .negated
                    .iter()
                    .map(|negated| negated.profile.as_str()),
            )
        }))
        .collect();
    for profile in profiles {
        let Some(Body::Source(source)) = library.get(profile).map(|p| &p.body) else {
            return Err(format!(
                "--select `{}`: selected source `{profile}` is missing",
                selected.select
            ));
        };
        reject_root_uid_placeholder(profile, source, supplied_roots)?;
    }
    Ok(())
}

/// In root mode `{uid}` expands to uid 0, not to each configured owner. A
/// root-owned namespace can therefore select a different user's files.
/// Refuse the source before its substituted find can plan or reclaim.
fn reject_root_uid_placeholder(
    profile: &str,
    source: &Source,
    supplied_roots: &[String],
) -> Result<(), String> {
    let in_field = if supplied_roots.iter().any(|root| root.contains("{uid}")) {
        Some("--roots")
    } else if matches!(&source.roots, Roots::Fixed(roots) if roots.iter().any(|root| root.contains("{uid}")))
    {
        Some("find.roots")
    } else if matches!(&source.finds, Finds::Paths { patterns, .. } if patterns.iter().any(|path| path.contains("{uid}")))
    {
        Some("find.paths")
    } else if matches!(&source.finds, Finds::Paths { exclude, .. } if exclude.iter().any(|path| path.contains("{uid}")))
    {
        Some("find.exclude")
    } else if source
        .where_
        .app_not_running
        .as_deref()
        .is_some_and(|path| path.contains("{uid}"))
    {
        Some("where.app_not_running")
    } else {
        None
    };
    if let Some(field) = in_field {
        return Err(format!(
            "--allow-root refuses `{{uid}}` in {field} of selected profile `{profile}`: \
             it would expand to root (uid 0), not to an owners: entry"
        ));
    }
    Ok(())
}

/// Check the action that this invocation actually selected. Static config
/// checks cover configured tiers; `--select` can choose another built-in or
/// user profile from the same library.
fn validate_selected_root_action(allow_root: bool, tier: &Tier) -> Result<(), String> {
    if allow_root && let Some(error) = tier.action.root_mode_error() {
        return Err(error.to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
