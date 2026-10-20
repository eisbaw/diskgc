//! Release files are exercised in the same disposable container as diskgc.

use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::suite::{Ctx, TestResult};
use crate::{ROOT, TESTER, ensure};

pub fn shipped_tmpfiles_and_example_work_in_container(ctx: &Ctx) -> TestResult {
    let root = ctx.scratch.mkdirs("package-root", 0o755, ROOT)?;
    ctx.scratch
        .mkdirs("package-root/usr/lib/tmpfiles.d", 0o755, ROOT)?;
    let rule = std::fs::read(ctx.bin_dir.join("diskgc-tmpfiles.conf"))
        .map_err(|e| format!("read shipped tmpfiles rule: {e}"))?;
    ctx.scratch.write_file(
        "package-root/usr/lib/tmpfiles.d/diskgc.conf",
        &rule,
        0o644,
        ROOT,
    )?;

    let applied = Command::new(ctx.bin_dir.join("systemd-tmpfiles"))
        .arg(format!("--root={}", root.display()))
        .arg("--create")
        .env_clear()
        .env("PATH", ctx.bin_dir.as_os_str())
        .output()
        .map_err(|e| format!("run pinned systemd-tmpfiles: {e}"))?;
    ensure!(
        applied.status.success(),
        "shipped tmpfiles rule failed: status {}, stderr {}",
        applied.status,
        String::from_utf8_lossy(&applied.stderr)
    );
    let dir = std::fs::symlink_metadata(root.join("run/diskgc"))
        .map_err(|e| format!("stat created lock directory: {e}"))?;
    let lock = std::fs::symlink_metadata(root.join("run/diskgc/lock"))
        .map_err(|e| format!("stat created lock file: {e}"))?;
    ensure!(
        dir.is_dir() && dir.mode() & 0o7777 == 0o755 && dir.uid() == 0 && dir.gid() == 0,
        "shipped drop-in created wrong lock directory: {dir:?}"
    );
    ensure!(
        lock.is_file() && lock.mode() & 0o7777 == 0o644 && lock.uid() == 0 && lock.gid() == 0,
        "shipped drop-in created wrong lock file: {lock:?}"
    );

    let checked = Command::new(ctx.bin_dir.join("diskgc"))
        .args(["check", "--config"])
        .arg(ctx.bin_dir.join("diskgc-example.yaml"))
        .env_clear()
        .env("PATH", ctx.bin_dir.as_os_str())
        .uid(TESTER.uid)
        .gid(TESTER.gid)
        .output()
        .map_err(|e| format!("validate shipped example config: {e}"))?;
    ensure!(
        checked.status.success(),
        "shipped example config failed check: status {}, stdout {}, stderr {}",
        checked.status,
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    Ok(())
}
