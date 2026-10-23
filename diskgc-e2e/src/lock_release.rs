//! Asserting that a lock was released. A flock goes when the last reference
//! to its open file goes, which can be after the holder's `close()` or exit
//! returned: a `/proc/<pid>/fd` or `fdinfo` reader on the machine (an
//! in-use scan, `lsof`) can briefly hold that reference (docs/safety.md, Host lock;
//! task-64 #37). So a release is awaited with [`RELEASE_BOUND`], never tried
//! once (docs/testing.md, Negative feedback). A held-lock assertion needs no
//! wait: a lagging release only keeps a lock longer.

use std::path::Path;
use std::time::{Duration, Instant};

/// How long a release may lag the last close. The measured lag is far
/// below this (docs/safety.md, Host lock); the bound is for a `/proc` reader
/// preempted on a loaded host, and a lock never released still fails.
pub const RELEASE_BOUND: Duration = Duration::from_secs(2);

/// Polls `released` until it is true or [`RELEASE_BOUND`] has passed;
/// whether it became true. An error from `released` ends the wait.
pub fn within_release_bound(
    mut released: impl FnMut() -> Result<bool, String>,
) -> Result<bool, String> {
    let deadline = Instant::now() + RELEASE_BOUND;
    loop {
        if released()? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Whether someone holds a flock on `path`: a non-blocking exclusive
/// `flock` by the harness on its own open file description is refused.
/// When it is granted, the probe's own lock goes with its fd; its release
/// can lag too, so a later release check must also wait.
pub fn flock_held(path: &Path) -> Result<bool, String> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| format!("open {}: {e}", path.display()))?;
    match rustix::fs::flock(&fd, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(false),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(true),
        Err(e) => Err(format!("flock {}: {e}", path.display())),
    }
}

/// Whether no one holds a flock on `path`, or no one does within
/// [`RELEASE_BOUND`]. The probe's own lock is released on return, and that
/// release can lag too: a consumer that then tries the lock once keeps
/// that exposure, so it must wait as well.
pub fn flock_free_within_release_bound(path: &Path) -> Result<bool, String> {
    within_release_bound(|| flock_held(path).map(|held| !held))
}
