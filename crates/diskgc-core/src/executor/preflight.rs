//! What the executor checks before a tier may delete: the caller's
//! privileges, `PATH` reached and measured (the one lookup without the
//! mount screen, [`crate::resolve::resolve_unscreened`]), the host lock's
//! readiness, and each root's filesystem and mount chain (docs/safety.md,
//! Validation; docs/reference.md, `ensure`).

use crate::engine::Reason;
use crate::fs_kind;
use crate::host::{Dev, Host, MonoTime};
use crate::hostlock;
use crate::report::{ErrorCause, ErrorEntry, Final, Report, SkipEntry, Status, conclude};
use crate::resolve::{ResolveError, Resolved, Want, resolve_in, resolve_unscreened};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::{Request, Tier, host_error, record_deadline_overrun, usage};

pub(super) enum MeasureFailure {
    Usage(String),
    Host(String),
}

pub(super) fn measure_failure(
    report: Report,
    log: &mut dyn FnMut(&str),
    failure: MeasureFailure,
) -> Report {
    match failure {
        MeasureFailure::Usage(message) => usage(report, log, message),
        MeasureFailure::Host(message) => {
            log(&format!("error: {message}"));
            let mut report = report;
            report.status = Status::Error;
            report
                .errors
                .push(ErrorEntry::run(ErrorCause::Host, message));
            report
        }
    }
}

/// Privileges (docs/safety.md, Privileges): diskgc refuses to run as root
/// without `--allow-root`. The caller validates and resolves `owners:`
/// before passing those IDs to the planner or reclaim. Returns the effective
/// uid used for `{uid}` substitution, not an implicit ownership allowlist.
pub fn privileges<H: Host>(host: &H, allow_root: bool) -> Result<u32, String> {
    let uid = host.effective_uid();
    if uid == 0 && !allow_root {
        return Err(
            "diskgc refuses to run as root (uid 0) without --allow-root (docs/safety.md, \
             Privileges): run it as the user whose caches it frees"
                .to_owned(),
        );
    }
    Ok(uid)
}

/// The requested filesystem, measured.
pub(super) struct Measured<F> {
    /// `PATH`, reached safely (held for the run: its fd is what is sampled).
    pub(super) at: Resolved<F>,
    pub(super) path: PathBuf,
    pub(super) dev: Dev,
    pub(super) min: u64,
    pub(super) target: u64,
}

/// Reaches `PATH`, samples its filesystem and turns the sizes into bytes;
/// fills the report's `mount`, sizes and `avail_before_bytes`. Host sampling
/// failures remain distinct from invalid path and size requests.
pub(super) fn measure<H: Host>(
    host: &H,
    request: &Request,
    report: &mut Report,
) -> Result<Measured<H::Fd>, MeasureFailure> {
    // PATH is the one lookup without the mount screen: the operator named
    // this filesystem to measure (resolve_unscreened).
    let at = resolve_unscreened(host, &request.path, Want::Directory).map_err(|error| {
        let message = format!("PATH {}: {error}", request.path.display());
        match error {
            ResolveError::Host(_) => MeasureFailure::Host(message),
            ResolveError::UnsafePath { .. }
            | ResolveError::UnsupportedFs { .. }
            | ResolveError::Missing { .. }
            | ResolveError::Screened { .. } => MeasureFailure::Usage(message),
        }
    })?;
    report.mount = Some(at.mount.mount_point.clone());
    let space = host
        .statvfs(&at.fd)
        .map_err(|e| MeasureFailure::Host(format!("PATH {}: {e}", request.path.display())))?;
    report.avail_before_bytes = Some(space.avail_bytes);
    let min = request
        .min_free
        .bytes(space.size_bytes)
        .map_err(|e| MeasureFailure::Usage(format!("--min-free {}: {e}", request.min_free)))?;
    report.min_free_bytes = Some(min);
    let target = match request.target_free {
        None => min,
        Some(size) => size
            .bytes(space.size_bytes)
            .map_err(|e| MeasureFailure::Usage(format!("--target-free {size}: {e}")))?,
    };
    report.target_free_bytes = Some(target);
    if target < min {
        return Err(MeasureFailure::Usage(format!(
            "--target-free ({target} bytes) is below --min-free ({min} bytes)"
        )));
    }
    Ok(Measured {
        dev: at.stat.dev,
        at,
        path: request.path.clone(),
        min,
        target,
    })
}

pub(super) fn measure_until<H: Host>(
    host: &H,
    request: &Request,
    report: &mut Report,
    deadline: Option<MonoTime>,
) -> Result<Measured<H::Fd>, MeasureFailure> {
    let started = host.now();
    let result = measure(host, request, report);
    record_deadline_overrun(host, report, deadline, started, "measure", &request.path);
    result
}

/// Setup can consume the whole call deadline before its first space sample.
/// Take that sample before choosing 0/124. A failed host sample is a run
/// error even when no prior measurement exists.
pub(super) fn deadline_before_first_sample<H: Host>(
    host: &H,
    request: &Request,
    deadline: Option<MonoTime>,
    mut report: Report,
    log: &mut dyn FnMut(&str),
) -> Report {
    let measured = match measure_until(host, request, &mut report, deadline) {
        Ok(measured) => measured,
        Err(_) if host.interrupted().is_some() => return report,
        Err(error) => return measure_failure(report, log, error),
    };
    report.avail_after_bytes = report.avail_before_bytes;
    report.status = conclude(
        Final::of(report.avail_after_bytes, measured.min),
        true,
        false,
    );
    report
}

pub(super) fn warn_if_host_lock_unready<H: Host>(host: &H, log: &mut dyn FnMut(&str)) {
    if let Err(error) = hostlock::readiness(host) {
        log(&format!("warning: host lock is not ready: {error}"));
    }
}

pub(super) fn warn_if_host_lock_unready_timed<H: Host>(
    host: &H,
    report: &mut Report,
    deadline: Option<MonoTime>,
    log: &mut dyn FnMut(&str),
) {
    let started = host.now();
    warn_if_host_lock_unready(host, log);
    record_deadline_overrun(
        host,
        report,
        deadline,
        started,
        "host_lock_readiness",
        &hostlock::host_lock_path(),
    );
}

/// A foreign root cannot free space on PATH. Omit it from every source and
/// negated source before discovery, including union terms. Fatal root errors
/// still exclude the whole tier because discovery may be incomplete.
pub(super) struct RootPreflight {
    pub(super) errors: Vec<ErrorEntry>,
    pub(super) skipped: Vec<SkipEntry>,
}

impl RootPreflight {
    pub(super) fn filtered_tier(&self, tier: &Tier) -> Tier {
        let foreign: HashSet<&Path> = self
            .skipped
            .iter()
            .filter_map(|s| s.candidate.as_deref())
            .collect();
        let mut filtered = tier.clone();
        filtered
            .roots
            .retain(|root| !foreign.contains(root.as_path()));
        for source in &mut filtered.negated {
            source
                .roots
                .retain(|root| !foreign.contains(root.as_path()));
        }
        for source in &mut filtered.fs_union {
            source
                .roots
                .retain(|root| !foreign.contains(root.as_path()));
            for negated in &mut source.negated {
                negated
                    .roots
                    .retain(|root| !foreign.contains(root.as_path()));
            }
        }
        filtered
    }
}

/// A tier's roots for its log line.
pub(super) fn roots_text(roots: &[PathBuf]) -> String {
    roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The log line of a tier left without a root: each was on another
/// filesystem than PATH's ([`preflight_roots`] skips those as `other_fs`
/// before the tier runs), so it looked nowhere. It replaces the count line,
/// which read `judged below  (` with an empty root list (TASK-209).
pub(super) fn nothing_searched(tier: &Tier) -> String {
    format!(
        "{}: `{}`: nothing searched: no root of the tier is on PATH's filesystem (skipped as \
         other_fs)",
        tier.name, tier.select
    )
}

/// Check every source root against PATH's device before enumerating candidates.
/// A missing root remains an empty source. A foreign root is reported and
/// skipped while same-device roots proceed: once per tier, however many of
/// its sources (a union's terms, a negated profile) share that root, so
/// that every tier reports it exactly once (TASK-210, decided for 0.1: a
/// skipped `other_fs` row, no new JSON field). An unsafe or unreadable root
/// excludes its tier; all roots are checked before another tier may delete.
/// Compare devices, not mount ids: a bind mount of the requested filesystem
/// can free space on PATH.
pub(super) fn preflight_roots<H: Host>(host: &H, tier: &Tier, requested_dev: Dev) -> RootPreflight {
    let mut roots = Vec::new();
    roots.extend(tier.roots.iter().map(|root| (tier.profile.as_str(), root)));
    for source in &tier.negated {
        roots.extend(
            source
                .roots
                .iter()
                .map(|root| (source.profile.as_str(), root)),
        );
    }
    for source in &tier.fs_union {
        roots.extend(
            source
                .roots
                .iter()
                .map(|root| (source.profile.as_str(), root)),
        );
        for negated in &source.negated {
            roots.extend(
                negated
                    .roots
                    .iter()
                    .map(|root| (negated.profile.as_str(), root)),
            );
        }
    }
    if roots.is_empty() {
        return RootPreflight {
            errors: Vec::new(),
            skipped: Vec::new(),
        };
    }
    // The root itself may be inside systemd's PrivateTmp. Reject it from
    // its path components before resolve_in can open or stat any ancestor.
    let mut errors = Vec::new();
    let mut skipped = Vec::new();
    roots.retain(|(profile, root)| {
        if !fs_kind::path_in_private_tmp(root) {
            return true;
        }
        errors.push(ErrorEntry {
            tier: Some(tier.name.clone()),
            profile: Some((*profile).to_owned()),
            candidate: Some(root.display().to_string()),
            cause: ErrorCause::Safety,
            message: format!(
                "root {}: inside systemd PrivateTmp (systemd-private-*); misconfigured root; the tier is excluded",
                root.display()
            ),
        });
        false
    });
    if roots.is_empty() {
        return RootPreflight { errors, skipped };
    }
    let mounts = match host.mountinfo() {
        Ok(mounts) => mounts,
        Err(error) => {
            errors.push(host_error(
                tier,
                format!("cannot read mountinfo for root preflight: {error}; the tier is excluded"),
            ));
            return RootPreflight { errors, skipped };
        }
    };
    for (profile, root) in roots {
        let mut error = |cause: ErrorCause, message: String| {
            errors.push(ErrorEntry {
                tier: Some(tier.name.clone()),
                profile: Some(profile.to_owned()),
                candidate: Some(root.display().to_string()),
                cause,
                message,
            })
        };
        // A local bind mount can sit below an unsafe ancestor: resolve_in
        // screens the full mount chain before it opens any of it.
        match resolve_in(host, &mounts, root, Want::Directory) {
            Ok(resolved) if resolved.stat.dev != requested_dev => {
                if !skipped
                    .iter()
                    .any(|skip: &SkipEntry| skip.candidate.as_ref() == Some(root))
                {
                    skipped.push(SkipEntry {
                        tier: tier.name.clone(),
                        profile: profile.to_owned(),
                        candidate: Some(root.clone()),
                        count: 1,
                        examples: Vec::new(),
                        reason: Reason::OtherFs,
                        detail: Some(crate::report::OTHER_FS_ROOT_DETAIL.to_owned()),
                        process_blockers: None,
                        estimated_bytes: None,
                    });
                }
            }
            Ok(_) | Err(ResolveError::Missing { .. }) => {}
            Err(ResolveError::Screened { why, .. }) => error(
                ErrorCause::Safety,
                format!("root {}: {why}; the tier is excluded", root.display()),
            ),
            Err(ResolveError::UnsafePath { why, .. }) => error(
                ErrorCause::Safety,
                format!(
                    "root {}: unsafe_path (misconfigured, or the mounts changed): {why}; the tier is excluded",
                    root.display()
                ),
            ),
            Err(ResolveError::Host(host_error)) => error(
                ErrorCause::Host,
                format!(
                    "root {}: {host_error}; the tier is excluded",
                    root.display()
                ),
            ),
            Err(ResolveError::UnsupportedFs { missing, .. }) => error(
                ErrorCause::Safety,
                format!(
                    "root {}: unsupported filesystem check {missing}; the tier is excluded",
                    root.display()
                ),
            ),
        }
    }
    RootPreflight { errors, skipped }
}
