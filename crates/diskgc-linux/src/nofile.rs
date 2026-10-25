//! diskgc's open-file limit (`RLIMIT_NOFILE`; docs/safety.md, In use: file
//! descriptors).
//!
//! The in-use scan holds one pidfd for every visible process for the whole
//! scan, so it needs about one descriptor per process (measured on a host
//! of ~2,250 processes: 2,402 at peak). The stock soft limit of most
//! distributions is 1024 (hard 524288), which a busy laptop or CI host
//! exceeds. The binary therefore raises its soft limit to the hard limit at
//! startup ([`raise_to_hard`]), as systemd and Go's runtime do; the hard
//! limit is the administrator's ceiling and is never raised. A scan that
//! still runs out is an error of the run, never an `in_use` skip.
//!
//! Children (command actions, Docker queries) get back the soft limit
//! diskgc inherited ([`restore_inherited_in_child`]), so a program written
//! for `select(2)` (fds below 1024) sees the environment it was started in.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use diskgc_core::host::HostError;
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

use crate::host::errno_of;

/// Whether [`raise_to_hard`] changed the soft limit; read by children.
static RAISED: AtomicBool = AtomicBool::new(false);
/// The soft limit diskgc inherited ([`RLIM_INFINITY`] for none), valid
/// once [`RAISED`] is set.
static INHERITED_SOFT: AtomicU64 = AtomicU64::new(RLIM_INFINITY);

/// `RLIM_INFINITY` as the kernel encodes it (`rustix` maps it to `None`).
const RLIM_INFINITY: u64 = u64::MAX;

/// The soft and hard limits before and after [`raise_to_hard`] (`None`:
/// unlimited).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenFilesLimit {
    pub inherited_soft: Option<u64>,
    pub soft: Option<u64>,
    pub hard: Option<u64>,
}

/// Raises this process's soft `RLIMIT_NOFILE` to its hard limit. Call it
/// once, at startup, before any thread or child is started: a second call
/// would record the raised limit as the inherited one. A failure (a seccomp filter or LSM
/// refusing `prlimit64`, a hard limit above `fs.nr_open`) leaves the limit
/// as it was: the caller reports it, and a scan that then runs out of
/// descriptors fails loudly.
pub fn raise_to_hard() -> Result<OpenFilesLimit, HostError> {
    let inherited = getrlimit(Resource::Nofile);
    let unchanged = OpenFilesLimit {
        inherited_soft: inherited.current,
        soft: inherited.current,
        hard: inherited.maximum,
    };
    debug_assert!(
        !RAISED.load(Ordering::Acquire),
        "raise_to_hard is called once, at startup"
    );
    if inherited.current == inherited.maximum {
        return Ok(unchanged);
    }
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: inherited.maximum,
            maximum: inherited.maximum,
        },
    )
    .map_err(|e| {
        HostError::errno(
            "setrlimit",
            format!(
                "RLIMIT_NOFILE soft {} to hard {}",
                shown(inherited.current),
                shown(inherited.maximum)
            ),
            errno_of(e),
        )
    })?;
    INHERITED_SOFT.store(
        inherited.current.unwrap_or(RLIM_INFINITY),
        Ordering::Relaxed,
    );
    RAISED.store(true, Ordering::Release);
    Ok(OpenFilesLimit {
        soft: inherited.maximum,
        ..unchanged
    })
}

/// Sets the soft limit back to the inherited one, in a forked child before
/// `exec` (`pre_exec`): two atomic loads and one `prlimit64` syscall, no
/// allocation, no lock (async-signal-safe). Lowering the soft limit cannot
/// be refused by the kernel; a filter that refuses it fails the spawn.
pub(crate) fn restore_inherited_in_child() -> std::io::Result<()> {
    if !RAISED.load(Ordering::Acquire) {
        return Ok(());
    }
    let soft = INHERITED_SOFT.load(Ordering::Relaxed);
    let hard = getrlimit(Resource::Nofile).maximum;
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: (soft != RLIM_INFINITY).then_some(soft),
            maximum: hard,
        },
    )
    .map_err(std::io::Error::from)
}

fn shown(limit: Option<u64>) -> String {
    limit.map_or_else(|| "unlimited".to_owned(), |n| n.to_string())
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    use super::*;

    /// Which stage of [`the_soft_limit_is_raised_and_children_get_the_inherited_one`]
    /// this process is: `raise` (started with soft 64) or `report`.
    const STAGE: &str = "DISKGC_NOFILE_TEST_STAGE";
    const TEST: &str = "nofile::tests::the_soft_limit_is_raised_and_children_get_the_inherited_one";
    const INHERITED: u64 = 64;

    fn rerun(stage: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("own executable"));
        command
            .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
            .env(STAGE, stage);
        command
    }

    /// The test process's limits are never touched: a child started with
    /// soft 64 raises its own to the hard limit, and a grandchild it starts
    /// through [`restore_inherited_in_child`] has soft 64 again.
    #[test]
    fn the_soft_limit_is_raised_and_children_get_the_inherited_one() {
        match std::env::var(STAGE).as_deref() {
            Err(_) => {
                let hard = getrlimit(Resource::Nofile).maximum;
                assert!(
                    hard.is_none_or(|hard| hard > INHERITED),
                    "a hard limit above {INHERITED} is needed for this test: {hard:?}"
                );
                let mut child = rerun("raise");
                // SAFETY: one prlimit64 syscall between fork and exec.
                unsafe {
                    child.pre_exec(move || {
                        setrlimit(
                            Resource::Nofile,
                            Rlimit {
                                current: Some(INHERITED),
                                maximum: hard,
                            },
                        )
                        .map_err(std::io::Error::from)
                    });
                }
                let out = child.output().expect("run the raising child");
                let text = String::from_utf8_lossy(&out.stdout);
                assert!(
                    out.status.success(),
                    "{text}{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                assert!(
                    text.contains(&format!("raised from {INHERITED} to {hard:?}\n")),
                    "{text}"
                );
                assert!(
                    text.contains(&format!("grandchild soft {INHERITED}\n")),
                    "{text}"
                );
            }
            Ok("raise") => {
                let limit = raise_to_hard().expect("raise the soft limit");
                assert_eq!(limit.inherited_soft, Some(INHERITED));
                assert_eq!(limit.soft, limit.hard);
                assert_eq!(getrlimit(Resource::Nofile).current, limit.hard);
                println!("raised from {INHERITED} to {:?}", limit.hard);
                let mut grandchild = rerun("report");
                // SAFETY: restore_inherited_in_child is async-signal-safe.
                unsafe {
                    grandchild.pre_exec(restore_inherited_in_child);
                }
                let out = grandchild.output().expect("run the grandchild");
                assert!(out.status.success(), "{out:?}");
                print!("{}", String::from_utf8_lossy(&out.stdout));
            }
            Ok("report") => {
                let soft = getrlimit(Resource::Nofile).current;
                println!("grandchild soft {}", shown(soft));
            }
            Ok(other) => panic!("unknown stage {other:?}"),
        }
    }
}
