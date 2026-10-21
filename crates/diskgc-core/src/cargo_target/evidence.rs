//! The Cargo process heuristics (docs/safety.md, `cargo-target`: In use),
//! on task-6's process scan. Cargo holds no lock while a binary runs
//! (`cargo run`, `cargo test`) nor between compiling and starting it, so
//! these are the only protection then; they are heuristics, with the blind
//! spots that docs/safety.md states.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::{CARGO_MANIFEST, Inspected, Project};
use crate::engine::{ProcessBlocker, ProcessEvidence};
use crate::host::DevIno;
use crate::host::Pid;
use crate::inuse::environ::inside;
use crate::inuse::{How, Scan};

/// The variable Cargo sets for every process it starts for a package
/// (build scripts, `cargo run`'s binary or runner, test binaries): the
/// package's directory. Measured inherited by a runner on Cargo 1.90.0,
/// 1.99.1 and the 2026-10-16 nightly.
pub const CARGO_MANIFEST_DIR: &str = "CARGO_MANIFEST_DIR";

/// One Cargo-specific heuristic, keeping structured attribution next to
/// its diagnostic text so the report never needs to parse a message.
pub(crate) struct CargoEvidence {
    pub detail: String,
    pub blocker: ProcessBlocker,
}

impl CargoEvidence {
    fn new(scan: &Scan, pid: Pid, evidence: ProcessEvidence, detail: String) -> Self {
        Self {
            detail,
            blocker: ProcessBlocker {
                pid: pid.0,
                command: scan.command_option(pid),
                evidence,
            },
        }
    }
}

/// Every piece of evidence in `scan` that the target at `target` (its
/// directory's identity `target_id`, what [`super::inspect`] found in it
/// `inspected`, its project directory's identity `project` where known) is
/// in use, beyond what the walked entries' own records show, one typed
/// item with diagnostic text each. Empty: none. The rules, in this order:
///
/// 1. a process uses (fd, cwd, root, exe, mapping) the target directory, a
///    directory directly in it, a profile directory or a lock file,
///    matched by (device, inode);
/// 2. a process's working directory or executable is inside the target,
///    by path text (a test binary still running; `target/doc`,
///    `target/tmp` are not walked);
/// 3. a Cargo process (executable name starting with `cargo` once a Nix
///    wrapper's `.` and `-wrapped` are taken off: `cargo`,
///    `.cargo-wrapped`, `cargo-nextest`) works in the project directory
///    (the target's parent) or below it, by path text or, for the project
///    directory itself, by (device, inode);
/// 4. a process's initial environment names a path inside the target,
///    whatever the variable (`CARGO_TARGET_DIR`, `CARGO_BUILD_BUILD_DIR`,
///    the `LD_LIBRARY_PATH` Cargo gives what it runs);
/// 5. a process's initial environment has `CARGO_MANIFEST_DIR` in the
///    project directory or below it: something Cargo started for a package
///    of the project (`cargo run` execs its runner in its own process, so
///    no process named `cargo` is left; a test binary's runner);
/// 6. the target is **detached** (its parent holds no `Cargo.toml`:
///    `--target-dir`, `CARGO_TARGET_DIR`, a `target-dir` in
///    `.cargo/config.toml`, a nested target) and a Cargo process runs,
///    wherever it works: where a detached target's Cargo runs cannot be
///    told from its path.
///
/// Path text is the process's view, so a process of another mount
/// namespace is matched by rule 1 and the exact half of rule 3 only.
pub(crate) fn cargo_evidence(
    scan: &Scan,
    target: &Path,
    target_id: DevIno,
    inspected: &Inspected,
    project: Project,
) -> Vec<CargoEvidence> {
    let mut found = Vec::new();
    let who = |pid| format!("pid {pid} ({})", scan.command(pid));
    let objects = std::iter::once((PathBuf::new(), target_id)).chain(inspected.objects());
    for (rel, id) in objects {
        let at = if rel.as_os_str().is_empty() {
            target.to_owned()
        } else {
            target.join(rel)
        };
        for u in scan.uses(id) {
            found.push(CargoEvidence::new(
                scan,
                u.pid,
                match u.how {
                    How::Fd(_) => ProcessEvidence::Fd,
                    How::Cwd => ProcessEvidence::Cwd,
                    How::Root => ProcessEvidence::Root,
                    How::Exe => ProcessEvidence::Exe,
                    How::Map => ProcessEvidence::Map,
                },
                format!("{} has {} {} ({id})", who(u.pid), at.display(), u.how),
            ));
        }
    }
    let project_dir = target.parent();
    for (pid, seen) in scan.readable() {
        for cwd in seen.cwds.iter().filter(|c| inside(c, target)) {
            found.push(CargoEvidence::new(
                scan,
                pid,
                ProcessEvidence::TargetCwd,
                format!(
                    "{} works inside the target (cwd {})",
                    who(pid),
                    cwd.display()
                ),
            ));
        }
        for exe in seen.exes.iter().filter(|e| inside(undeleted(e), target)) {
            found.push(CargoEvidence::new(
                scan,
                pid,
                ProcessEvidence::TargetExe,
                format!(
                    "{} runs an executable inside the target ({})",
                    who(pid),
                    exe.display()
                ),
            ));
        }
        if !is_cargo(&seen.command) {
            continue;
        }
        let by_text = project_dir.and_then(|p| seen.cwds.iter().find(|c| inside(c, p)));
        let by_identity = project.id.is_some_and(|id| {
            scan.uses(id)
                .iter()
                .any(|u| u.pid == pid && u.how == How::Cwd)
        });
        if by_text.is_some() || by_identity {
            found.push(CargoEvidence::new(
                scan,
                pid,
                ProcessEvidence::ProjectCargo,
                format!(
                    "{} is a Cargo process working in the project directory {} (cwd {})",
                    who(pid),
                    project_dir.map_or("?".into(), |p| p.display().to_string()),
                    by_text.map_or("matched by identity".into(), |c| c.display().to_string())
                ),
            ));
        } else if project.detached {
            found.push(CargoEvidence::new(
                scan,
                pid,
                ProcessEvidence::DetachedCargo,
                format!(
                    "{} is a Cargo process, and the target is detached (its parent holds no \
                 {CARGO_MANIFEST}: a --target-dir, a target-dir in .cargo/config.toml or a nested \
                 target), so any Cargo may be using it",
                    who(pid)
                ),
            ));
        }
    }
    for e in scan.environ_paths() {
        if inside(&e.path, target) {
            found.push(CargoEvidence::new(
                scan,
                e.pid,
                ProcessEvidence::Environment,
                format!(
                    "{} names a path inside the target in its initial environment ({}={})",
                    who(e.pid),
                    e.var.to_string_lossy(),
                    e.path.display()
                ),
            ));
        } else if e.var == CARGO_MANIFEST_DIR && project_dir.is_some_and(|p| inside(&e.path, p)) {
            found.push(CargoEvidence::new(
                scan,
                e.pid,
                ProcessEvidence::CargoManifestDir,
                format!(
                    "{} was started by Cargo for a package of the project directory \
                 ({CARGO_MANIFEST_DIR}={})",
                    who(e.pid),
                    e.path.display()
                ),
            ));
        }
    }
    found
}

/// An executable's link text without the ` (deleted)` the kernel appends,
/// byte for byte: paths are compared as bytes, and a lossy conversion of a
/// non-UTF-8 name would make the comparison miss.
fn undeleted(exe: &Path) -> &Path {
    let bytes = exe.as_os_str().as_bytes();
    Path::new(OsStr::from_bytes(
        bytes.strip_suffix(b" (deleted)").unwrap_or(bytes),
    ))
}

/// Whether `command` (an executable's last name) is a Cargo: it starts
/// with `cargo` once a Nix wrapper's leading `.` and trailing `-wrapped`
/// are taken off (bytes, no conversion).
pub fn is_cargo(command: &OsStr) -> bool {
    let name = command.as_bytes();
    let name = name.strip_prefix(b".").unwrap_or(name);
    let name = name.strip_suffix(b"-wrapped").unwrap_or(name);
    name.starts_with(b"cargo")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_and_its_wrappers_and_subcommands_are_cargo() {
        for yes in [
            "cargo",
            ".cargo-wrapped",
            "cargo-nextest",
            "cargo-clippy",
            "cargo-wrapped",
        ] {
            assert!(is_cargo(OsStr::new(yes)), "{yes}");
        }
        for no in [
            "rustc",
            "carg",
            "xcargo",
            ".rustc-wrapped",
            "",
            "diskgc-e2e",
        ] {
            assert!(!is_cargo(OsStr::new(no)), "{no}");
        }
    }

    #[test]
    fn a_deleted_executable_is_matched_by_its_path() {
        assert_eq!(
            undeleted(Path::new("/p/target/debug/x (deleted)")),
            Path::new("/p/target/debug/x")
        );
        assert_eq!(undeleted(Path::new("/bin/sh")), Path::new("/bin/sh"));
    }

    /// Codex finding 2: a target below a non-UTF-8 name. The executable's
    /// path must stay byte for byte, deleted or not, so it is still inside
    /// the target; and a Cargo with a non-UTF-8 byte after its name is
    /// still a Cargo.
    #[test]
    fn non_utf8_paths_and_names_are_compared_as_bytes() {
        let target = Path::new(OsStr::from_bytes(b"/tmp/w/\xff/p/target"));
        for exe in [
            &b"/tmp/w/\xff/p/target/tmp/helper"[..],
            b"/tmp/w/\xff/p/target/tmp/helper (deleted)",
        ] {
            let exe = Path::new(OsStr::from_bytes(exe));
            assert!(inside(undeleted(exe), target), "{exe:?}");
        }
        let elsewhere = Path::new(OsStr::from_bytes(b"/tmp/w/\xfe/p/target/tmp/helper"));
        assert!(!inside(undeleted(elsewhere), target));
        assert!(is_cargo(OsStr::from_bytes(b"cargo\xff")));
        assert!(!is_cargo(OsStr::from_bytes(b"\xffcargo")));
    }
}
