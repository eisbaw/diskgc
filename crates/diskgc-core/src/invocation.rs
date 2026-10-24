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

use crate::config::{self, Config, IpcPolicy, Owner, Ranking};
use crate::engine::Checkouts;
use crate::executor::{self, Request, SelectionFailure, Tier};
use crate::host::Delete;
use crate::profile::{Body, Finds, Library, Roots, Source};
use crate::report::{ErrorCause, Report, Status};
use crate::select;
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
            enforce_proposed_release_01_config(&mut config)?;
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
fn enforce_proposed_release_01_config(config: &mut Config) -> Result<(), String> {
    let library = config.library.clone().proposed_release_01()?;
    let builtins = Library::builtin().map_err(|e| e.to_string())?;
    for policy in &config.profile_policies {
        if library.get(&policy.profile).is_none() {
            return Err(format!(
                "proposed 0.1 catalog refuses checkout policy for excluded profile `{}`",
                policy.profile
            ));
        }
        if policy.checkouts.0 == Checkouts::Allow
            && builtins.checkouts(&policy.profile) == Some(Checkouts::Refuse)
        {
            return Err(format!(
                "proposed 0.1 catalog refuses `checkouts: allow` for `{}`: the built-in \
                 refuses checkouts, and a 0.1 checkout policy may only narrow",
                policy.profile
            ));
        }
    }
    for group in &config.groups {
        for tier in &group.tiers {
            select::check(tier.select.expr().clone(), &library).map_err(|e| {
                format!(
                    "proposed 0.1 catalog refuses group `{}` tier `{}`: {e}",
                    group.name, tier.name
                )
            })?;
        }
    }
    for protected in &config.protect {
        if library.get(&protected.profile).is_none() {
            return Err(format!(
                "proposed 0.1 catalog refuses protect profile `{}`",
                protected.profile
            ));
        }
    }
    config.library = library;
    Ok(())
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
    pub path: PathBuf,
    /// A test seam, not an option: the eligibility walk's entry budget in
    /// place of [`crate::engine::DEFAULT_MAX_ENTRIES`], so a model vector
    /// can reach `too_many_entries` with a small tree. The binary always
    /// passes `None`; no flag or configuration key sets it (pinned by
    /// `the_binary_never_sets_the_test_entry_budget` in `main.rs`).
    pub test_entry_budget: Option<usize>,
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

/// Runs `ensure` (or its dry run); every error is in the report. Every
/// refusal before the executor (a configuration that does not load under
/// the catalog, none found where one is needed, a static check) is exit 2
/// and deletes nothing. `log` receives the diagnostics (`error: ...`,
/// `warning: ...`), `progress` the executor's progress lines.
pub fn ensure<H: Delete>(
    host: &H,
    options: &EnsureOptions,
    config: &ConfigFile,
    platform: &Platform<'_>,
    log: &mut dyn FnMut(&str),
    progress: &mut dyn FnMut(&str),
) -> Report {
    match run(host, options, config, platform, log, progress) {
        Ok(report) => report,
        Err((cause, why)) => {
            // Every refusal before the executor is exit 2 and deletes nothing.
            log(&format!("error: {why}"));
            Report::new(Status::UsageError, Some(&options.path)).usage_error_with_cause(cause, why)
        }
    }
}

/// [`ensure`], with a refusal before the executor as its error.
fn run<H: Delete>(
    host: &H,
    options: &EnsureOptions,
    config_file: &ConfigFile,
    platform: &Platform<'_>,
    log: &mut dyn FnMut(&str),
    progress: &mut dyn FnMut(&str),
) -> Result<Report, (ErrorCause, String)> {
    let fail = |cause: ErrorCause, why: String| Err((cause, why));
    let loaded = match config_file {
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
    let config = loaded.as_ref();
    if let Some(config) = config {
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
                "ipc: live-only is statically valid, but ensure cannot enforce it in this build yet (task-21)"
                    .to_owned(),
            );
        }
    }
    let uid =
        executor::privileges(host, options.allow_root).map_err(|why| (ErrorCause::Safety, why))?;
    let owners = if options.allow_root {
        let config = config.expect("root mode requires config (refused above)");
        resolved_owners(host, config).map_err(|why| (ErrorCause::Config, why))?
    } else {
        vec![uid]
    };
    let protections = match config {
        Some(config) => executor::bind_protections(config, uid, options.allow_root)
            .map_err(|why| (ErrorCause::Config, why))?,
        None => Vec::new(),
    };
    let mut configured = if options.select.is_none() {
        let config = config.expect("unselected ensure requires config (refused above)");
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
    let mut tier = if let Some((tiers, _, _)) = &configured {
        tiers[0].clone()
    } else {
        let builtin;
        let library = match config {
            Some(config) => &config.library,
            None => {
                builtin = (platform.builtins)()
                    .and_then(|builtins| platform.catalog.library(builtins))
                    .map_err(|why| (ErrorCause::Config, why))?;
                &builtin
            }
        };
        let tier = selected_tier(options, library, uid, log)?;
        validate_selected_root_action(options.allow_root, &tier)
            .map_err(|why| (ErrorCause::Safety, why))?;
        tier
    };
    tier.protections = protections;
    tier.max_entries = max_entries;
    let request = Request {
        path: options.path.clone(),
        min_free: options.min_free,
        target_free: options.target_free,
        timeout: options.timeout,
        tally_budget: options.tally_budget,
        tier,
    };
    let mut report = if let Some((tiers, timeouts, _)) = &configured {
        if options.dry_run {
            executor::dry_run_grouped(host, &request, tiers, timeouts, &owners, progress)
        } else {
            executor::ensure_grouped(host, &request, tiers, timeouts, &owners, progress)
        }
    } else if options.dry_run {
        // Through `Host` only: the dry run has no way to reach a deleting
        // method (the generic bound is `Delete`; this call does not use it).
        executor::dry_run(host, &request, &owners, progress)
    } else {
        executor::ensure(host, &request, &owners, progress)
    };
    report.seed = configured.as_ref().and_then(|(_, _, seed)| *seed);
    Ok(report)
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

/// The tier `--select` makes, against `library`.
fn selected_tier(
    options: &EnsureOptions,
    library: &Library,
    uid: u32,
    log: &mut dyn FnMut(&str),
) -> Result<Tier, (ErrorCause, String)> {
    let Some(expr) = &options.select else {
        return Err((
            ErrorCause::Usage,
            "an explicit tier requires --select".to_owned(),
        ));
    };
    let roots = (!options.roots.is_empty()).then_some(options.roots.as_slice());
    let (selected, warnings) =
        Tier::select_with_warnings(expr, library, roots, uid).map_err(|failure| match failure {
            SelectionFailure::Parse(message) => (ErrorCause::Parse, message),
            SelectionFailure::Usage(message) => (ErrorCause::Usage, message),
        })?;
    for warning in warnings {
        log(&format!("warning: --select {expr:?}: {warning}"));
    }
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
