//! Probes: small actions a test runs as a subprocess of `diskgc-e2e`, either
//! as the code under test (as `tester`, under strace) or as container root.
//!
//! They exist to prove that the harness's checks bite: some misbehave on
//! purpose (delete or replace what the fixture did not make eligible, touch a
//! path outside the scratch mount). Probes run only after the interlock
//! (layer 2) passed, like everything in `diskgc-e2e`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::scratch::Scratch;
use crate::{ROOT, exit};

/// The file an escape probe tries to create outside the scratch mount.
pub const ESCAPED_NAME: &str = "diskgc-e2e-escaped";

/// Directory outside the scratch mount (on the image's root overlay) that
/// the escape probes aim at.
pub const ESCAPE_TARGET_DIR: &str = "/var/tmp";

/// Interlock layer-3 cases: each tries to make a fixture operation touch a
/// path outside the scratch mount, except `control`, which stays inside.
pub const ESCAPE_CASES: &[&str] = &["absolute", "dotdot", "symlink", "bind-mount", "control"];

/// Runs probe `args[0]` with the remaining arguments; returns the exit code.
pub fn main(args: &[String]) -> i32 {
    let result = match args {
        [cmd] if cmd == "whoami" => whoami(),
        [cmd, path] if cmd == "read-dir" => read_dir(Path::new(path)),
        [cmd, paths @ ..] if cmd == "remove" && !paths.is_empty() => remove(paths),
        [cmd, path] if cmd == "recreate" => recreate(Path::new(path)),
        [cmd, path] if cmd == "rename-over" => rename_over(Path::new(path)),
        [cmd, case] if cmd == "escape" => escape(case),
        _ => {
            eprintln!("diskgc-e2e probe: unknown probe {args:?}");
            return exit::USAGE;
        }
    };
    match result {
        Ok(()) => exit::PASS,
        Err(e) => {
            eprintln!("diskgc-e2e probe {args:?}: {e}");
            exit::FAIL
        }
    }
}

/// Prints this process's credentials from /proc/self/status, one
/// `Name:\tvalues` line each, for Uid, Gid, Groups, CapEff and CapPrm.
fn whoami() -> Result<(), String> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|e| format!("read /proc/self/status: {e}"))?;
    for line in status.lines() {
        if ["Uid:", "Gid:", "Groups:", "CapEff:", "CapPrm:"]
            .iter()
            .any(|k| line.starts_with(k))
        {
            println!("{line}");
        }
    }
    Ok(())
}

/// Prints `ok <entries>` or `err <errno> <message>`; the test decides what
/// it expected.
fn read_dir(path: &Path) -> Result<(), String> {
    match std::fs::read_dir(path) {
        Ok(dir) => println!("ok {}", dir.count()),
        Err(e) => println!("err {} {e}", e.raw_os_error().unwrap_or(0)),
    }
    Ok(())
}

/// Deletes each path (directories recursively).
fn remove(paths: &[String]) -> Result<(), String> {
    for path in paths.iter().map(PathBuf::from) {
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("lstat {}: {e}", path.display()))?;
        if meta.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        }
        .map_err(|e| format!("remove {}: {e}", path.display()))?;
        println!("removed {}", path.display());
    }
    Ok(())
}

/// Unlinks the file and creates a new one with the same content at the same
/// path: same path, new inode (what Cargo does to a `.cargo-lock`).
fn recreate(path: &Path) -> Result<(), String> {
    let content = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    std::fs::remove_file(path).map_err(|e| format!("unlink {}: {e}", path.display()))?;
    create_with(path, &content)?;
    println!("recreated {}", path.display());
    Ok(())
}

/// Writes a copy next to the file and renames it over the original.
fn rename_over(path: &Path) -> Result<(), String> {
    let content = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".diskgc-e2e-new");
    let tmp = PathBuf::from(tmp);
    create_with(&tmp, &content)?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), path.display()))?;
    println!("renamed over {}", path.display());
    Ok(())
}

fn create_with(path: &Path, content: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    file.write_all(content)
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// Interlock layer 3: tries one way of making a fixture operation land
/// outside the scratch mount at `/tmp`. The check must stop the process with
/// [`exit::INTERLOCK`] before the file is created; if the operation returns
/// instead, this prints `NOT ABORTED` and exits 0, which the test reports.
fn escape(case: &str) -> Result<(), String> {
    let scratch = Scratch::attach("/tmp")?;
    let target = format!("{ESCAPE_TARGET_DIR}/{ESCAPED_NAME}");
    let attempted = match case {
        "absolute" => scratch.write_file(&target, b"escaped", 0o644, ROOT)?,
        "dotdot" => scratch.write_file(&format!("..{target}"), b"escaped", 0o644, ROOT)?,
        "symlink" => {
            scratch.symlink("escape-link", Path::new(ESCAPE_TARGET_DIR))?;
            scratch.write_file(
                &format!("escape-link/{ESCAPED_NAME}"),
                b"escaped",
                0o644,
                ROOT,
            )?
        }
        "bind-mount" => {
            // A bind mount of the scratch filesystem itself: same device,
            // different mount id.
            scratch.mkdir("bind-src", 0o755, ROOT)?;
            scratch.mkdir("bind-dst", 0o755, ROOT)?;
            scratch.bind_mount("bind-src", "bind-dst")?;
            scratch.write_file(&format!("bind-dst/{ESCAPED_NAME}"), b"escaped", 0o644, ROOT)?
        }
        "control" => {
            scratch.mkdir("control", 0o755, ROOT)?;
            scratch.write_file(&format!("control/{ESCAPED_NAME}"), b"inside", 0o644, ROOT)?
        }
        other => return Err(format!("unknown escape case {other:?}")),
    };
    println!("NOT ABORTED: created {}", attempted.display());
    Ok(())
}
