//! What the executor checks before a tier may delete: the caller's
//! privileges, `PATH` reached and measured (the one lookup without the
//! mount screen, [`crate::resolve::resolve_unscreened`]), the host lock's
//! readiness, and each root's filesystem and mount chain (docs/safety.md,
//! Validation; docs/reference.md, `ensure`).

use crate::engine::Reason;
use crate::fs_kind;
use crate::host::{Dev, Host, MonoTime};
use crate::hostlock::{self, Readiness};
use crate::places;
use crate::report::{ErrorCause, ErrorEntry, Final, Report, SkipEntry, Status, conclude};
use crate::resolve::{ResolveError, Resolved, Want, resolve_in, resolve_unscreened};
use crate::units::Size;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::{Request, Tier, host_error, record_deadline_overrun, usage};

/// Why `PATH` could not be measured: a usage error (exit 2) or a failure
/// of the host (exit 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeasureFailure {
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

/// `CAP_SYS_PTRACE`: its bit in a capability set (linux/capability.h).
pub const CAP_SYS_PTRACE: u32 = 19;

/// Why [`privileges`] refused: `cause` is `safety` (exit 2) for a refusal
/// by rule, `host` (exit 3) when diskgc could not read its own
/// capability set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegeError {
    pub cause: ErrorCause,
    pub message: String,
}

impl std::fmt::Display for PrivilegeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Privileges (docs/safety.md, Privileges), checked once at start:
///
/// - root without `--allow-root` is refused (`safety`);
/// - `--allow-root` is for root: with another effective uid it is refused
///   (`safety`), whatever capabilities the binary holds, because the
///   in-use scan decides root mode by the uid;
/// - `--allow-root` needs `CAP_SYS_PTRACE` in diskgc's effective set
///   (`CapEff:` of its own `/proc` status): without it a root process
///   holding a capability diskgc lacks is excused at the "more capable"
///   boundary (docs/safety.md, In use) and its open files go unseen with
///   no error. A set that was read and lacks it is refused (`safety`); a
///   set that could not be read is a failure of the host (`host`).
///
/// The caller validates and resolves `owners:` before passing those IDs to
/// the planner or reclaim. Returns the effective uid used for `{uid}`
/// substitution, not an implicit ownership allowlist.
pub fn privileges<H: Host>(host: &H, allow_root: bool) -> Result<u32, PrivilegeError> {
    let refuse = |cause, message: String| Err(PrivilegeError { cause, message });
    let uid = host.effective_uid();
    if uid == 0 && !allow_root {
        return refuse(
            ErrorCause::Safety,
            "diskgc refuses to run as root (uid 0) without --allow-root (docs/safety.md, \
             Privileges): run it as the user whose caches it frees"
                .to_owned(),
        );
    }
    if !allow_root {
        return Ok(uid);
    }
    if uid != 0 {
        return refuse(
            ErrorCause::Safety,
            format!(
                "--allow-root is for root; drop it (diskgc runs as uid {uid}; docs/safety.md, \
                 Privileges)"
            ),
        );
    }
    let needs = "--allow-root needs CAP_SYS_PTRACE in diskgc's effective capability set: \
                 without it the open files of a root process more capable than diskgc go \
                 unseen (docs/safety.md, Privileges)";
    match host.proc_status(host.own_pid()) {
        Ok(own) if own.cap_effective & (1 << CAP_SYS_PTRACE) != 0 => Ok(uid),
        Ok(own) => refuse(
            ErrorCause::Safety,
            format!(
                "{needs}; CapEff is {:#018x}, without it. Grant it: a systemd unit's \
                 CapabilityBoundingSet= (docs/architecture.md, Deployment), \
                 `--cap-add SYS_PTRACE` for a container",
                own.cap_effective
            ),
        ),
        Err(e) => refuse(
            ErrorCause::Host,
            format!("{needs}; diskgc could not read its own capability set: {e}"),
        ),
    }
}

/// The in-use check's visibility (docs/safety.md, In use: Visibility),
/// checked once at the start of every `ensure` (its dry run included: a
/// plan shows what a reclaim would do, and a reclaim would not start) and
/// of a reclaiming `watch`, before anything is measured: under `hidepid`,
/// or in a PID namespace other than the initial one, the in-use scan would
/// miss processes silently, so the run is refused (`safety`, exit 2),
/// naming the rule ([`crate::inuse::visibility`], its one home; the scan
/// checks it again on every scan). Facts that cannot be read are a failure
/// of the host (`host`, exit 3). TASK-263.
pub fn process_visibility<H: Host>(host: &H) -> Result<(), PrivilegeError> {
    let facts = host.pid_namespace().map_err(|e| PrivilegeError {
        cause: ErrorCause::Host,
        message: format!(
            "diskgc could not read its PID namespace facts, which the in-use check needs \
             (docs/safety.md, In use: Visibility): {e}"
        ),
    })?;
    crate::inuse::visibility(&facts, host.test_container_pid_namespace()).map_err(|why| {
        PrivilegeError {
            cause: ErrorCause::Safety,
            message: format!(
                "{why}; run diskgc where it sees every process: on the host, outside any \
                 container, with /proc mounted without hidepid (docs/safety.md, In use: \
                 Visibility)"
            ),
        }
    })
}

/// The requested filesystem, measured.
pub(super) struct Measured<F> {
    /// `PATH`, reached safely (held for the run: its fd is what is sampled).
    pub(super) at: Resolved<F>,
    pub(super) path: PathBuf,
    /// `PATH`'s filesystem's space ([`places::space_of`]): what every
    /// "the same filesystem" check compares with (`other_fs`).
    pub(super) space: Dev,
    pub(super) min: u64,
    pub(super) target: u64,
}

/// `PATH` reached safely (the one lookup without the mount screen,
/// [`crate::resolve::resolve_unscreened`]: the operator named this
/// filesystem to measure), held. `watch` holds it for its life.
pub fn reach_path<H: Host>(host: &H, path: &Path) -> Result<Resolved<H::Fd>, MeasureFailure> {
    resolve_unscreened(host, path, Want::Directory).map_err(|error| {
        let message = format!("PATH {}: {error}", path.display());
        match error {
            ResolveError::Host(_) => MeasureFailure::Host(message),
            ResolveError::UnsafePath { .. }
            | ResolveError::UnsupportedFs { .. }
            | ResolveError::Missing { .. }
            | ResolveError::Screened { .. } => MeasureFailure::Usage(message),
        }
    })
}

/// `--min-free` and `--target-free` (`None`: the same) in bytes on a
/// filesystem of `size_bytes`; the target below the minimum is a usage
/// error.
pub fn goal_bytes(
    min_free: Size,
    target_free: Option<Size>,
    size_bytes: u64,
) -> Result<(u64, u64), MeasureFailure> {
    let min = min_free
        .bytes(size_bytes)
        .map_err(|e| MeasureFailure::Usage(format!("--min-free {min_free}: {e}")))?;
    let target = match target_free {
        None => min,
        Some(size) => size
            .bytes(size_bytes)
            .map_err(|e| MeasureFailure::Usage(format!("--target-free {size}: {e}")))?,
    };
    if target < min {
        return Err(MeasureFailure::Usage(format!(
            "--target-free ({target} bytes) is below --min-free ({min} bytes)"
        )));
    }
    Ok((min, target))
}

/// Reaches `PATH` ([`reach_path`]), samples its filesystem and turns the
/// sizes into bytes ([`goal_bytes`]); fills the report's `mount`, sizes and
/// `avail_before_bytes`. Host sampling failures remain distinct from
/// invalid path and size requests. In a `watch` cycle, `PATH` must be on
/// the filesystem it was on at start ([`super::WatchCycle::expect_space`]):
/// this is the resolution the cycle measures and reclaims on, so no mount
/// of another filesystem made on `PATH` meanwhile goes unseen.
pub(super) fn measure<H: Host>(
    host: &H,
    request: &Request,
    report: &mut Report,
) -> Result<Measured<H::Fd>, MeasureFailure> {
    let at = reach_path(host, &request.path)?;
    report.mount = Some(at.mount.mount_point.clone());
    let fs_space = places::space_of(&at.mount);
    if let Some(cycle) = request.watch
        && fs_space != cycle.expect_space
    {
        return Err(MeasureFailure::Host(format!(
            "PATH {} is on device {fs_space}, not on the filesystem watched since start (device \
             {}): nothing is reclaimed there (a mount stacked on PATH?)",
            request.path.display(),
            cycle.expect_space
        )));
    }
    let space = host
        .statvfs(&at.fd)
        .map_err(|e| MeasureFailure::Host(format!("PATH {}: {e}", request.path.display())))?;
    report.avail_before_bytes = Some(space.avail_bytes);
    let (min, target) = match goal_bytes(request.min_free, request.target_free, space.size_bytes) {
        Ok(bytes) => bytes,
        Err(failure) => {
            // What could be resolved stays in the report.
            report.min_free_bytes = request.min_free.bytes(space.size_bytes).ok();
            report.target_free_bytes = request
                .target_free
                .map_or(report.min_free_bytes, |t| t.bytes(space.size_bytes).ok());
            return Err(failure);
        }
    };
    report.min_free_bytes = Some(min);
    report.target_free_bytes = Some(target);
    Ok(Measured {
        space: fs_space,
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

/// Warns when the lock a reclaim would take is not ready, without taking
/// or making it ([`hostlock::readiness`]). A non-root run without the host
/// lock is ready ([`Readiness::FallsBack`]): its reclaim makes and takes
/// its per-user lock (docs/safety.md, Host lock).
fn warn_if_host_lock_unready<H: Host>(host: &H, log: &mut dyn FnMut(&str)) -> Option<Readiness> {
    match hostlock::readiness(host) {
        Ok(readiness) => Some(readiness),
        Err(error) => {
            log(&format!("warning: host lock is not ready: {error}"));
            None
        }
    }
}

/// [`warn_if_host_lock_unready`], its time an overrun when it passes the
/// deadline. An already-met `ensure` says nothing about a per-user
/// fallback: it is not a fault, and the instant no-op stays quiet.
pub(super) fn warn_if_host_lock_unready_timed<H: Host>(
    host: &H,
    report: &mut Report,
    deadline: Option<MonoTime>,
    log: &mut dyn FnMut(&str),
) -> Option<Readiness> {
    let started = host.now();
    let readiness = warn_if_host_lock_unready(host, log);
    record_deadline_overrun(
        host,
        report,
        deadline,
        started,
        "host_lock_readiness",
        &hostlock::host_lock_path(),
    );
    readiness
}

/// The dry run's word on the lock its reclaim would take (it takes and
/// makes none): [`warn_if_host_lock_unready_timed`], and for a non-root
/// run without the host lock one line naming the per-user lock its
/// reclaims take, without telling the user to provision anything, or, with
/// no place for one, a warning that they would run without a lock
/// (docs/safety.md, Host lock).
pub(super) fn say_lock_readiness<H: Host>(
    host: &H,
    report: &mut Report,
    deadline: Option<MonoTime>,
    log: &mut dyn FnMut(&str),
) {
    let host_lock = hostlock::host_lock_path();
    match warn_if_host_lock_unready_timed(host, report, deadline, log) {
        Some(Readiness::FallsBack(Some(path))) => log(&format!(
            "note: the host lock {} is not provisioned: a reclaim by this user takes the \
             per-user lock {} instead, which does not serialise it with other users' reclaims \
             (docs/safety.md, Host lock)",
            host_lock.display(),
            path.display()
        )),
        Some(Readiness::FallsBack(None)) => log(&format!(
            "warning: the host lock {} is not provisioned and there is no place for a \
             per-user lock (no /run/user/<uid> of this user, no home directory): a reclaim \
             would run without a lock (docs/safety.md, Host lock)",
            host_lock.display()
        )),
        Some(Readiness::Host) | None => {}
    }
}

/// A foreign source root cannot free space on PATH. Omit it from every
/// source before discovery, including union terms. A negated profile's root
/// is kept wherever it is: its matches only exclude, and one on another
/// filesystem can still overlap a candidate by name, place or through an
/// overlay (TASK-260). Fatal root errors still exclude the whole tier
/// because discovery may be incomplete.
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
        for source in &mut filtered.fs_union {
            source
                .roots
                .retain(|root| !foreign.contains(root.as_path()));
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
/// A missing root remains an empty source. A foreign source root is
/// reported and skipped while same-device roots proceed: once per tier,
/// however many of its sources (a union's terms) share that root, so
/// that every tier reports it exactly once. A negated profile's foreign
/// root is screened like any root but neither skipped nor reported: it is
/// searched (TASK-260) (TASK-210, decided for 0.1: a
/// skipped `other_fs` row, no new JSON field). An unsafe or unreadable root
/// excludes its tier; all roots are checked before another tier may delete.
/// Compare spaces ([`places::space_of`]), not mount ids or `st_dev`: a bind
/// mount of the requested filesystem, or another subvolume of its btrfs,
/// frees space on PATH.
pub(super) fn preflight_roots<H: Host>(host: &H, tier: &Tier, space: Dev) -> RootPreflight {
    // (profile, root, whether it is a negated profile's root)
    let mut roots = Vec::new();
    roots.extend(
        tier.roots
            .iter()
            .map(|root| (tier.profile.as_str(), root, false)),
    );
    for source in &tier.negated {
        roots.extend(
            source
                .roots
                .iter()
                .map(|root| (source.profile.as_str(), root, true)),
        );
    }
    for source in &tier.fs_union {
        roots.extend(
            source
                .roots
                .iter()
                .map(|root| (source.profile.as_str(), root, false)),
        );
        for negated in &source.negated {
            roots.extend(
                negated
                    .roots
                    .iter()
                    .map(|root| (negated.profile.as_str(), root, true)),
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
    roots.retain(|(profile, root, _)| {
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
    for (profile, root, negated_only) in roots {
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
            Ok(resolved) if places::space_of(&resolved.mount) != space && !negated_only => {
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
                        unknown_cause: None,
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
