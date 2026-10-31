//! Release files are exercised in the same disposable container as diskgc.

use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::suite::{Ctx, TestResult};
use crate::{ROOT, TESTER, ensure};

enum RuleSelection {
    Discover,
    Explicit,
}

pub fn shipped_tmpfiles_and_example_work_in_container(ctx: &Ctx) -> TestResult {
    verify_shipped_files(
        ctx,
        RuleSelection::Discover,
        "diskgc",
        "diskgc-tmpfiles.conf",
        "diskgc-example.yaml",
    )
}

pub fn docker_shipped_tmpfiles_and_example_work_in_container(ctx: &Ctx) -> TestResult {
    // sd-tools 0.99.0 crashes when scanning a scratch root's tmpfiles.d
    // directories. Naming the staged file still proves the rule's effect.
    verify_shipped_files(
        ctx,
        RuleSelection::Explicit,
        "diskgc",
        "diskgc-tmpfiles.conf",
        "diskgc-example.yaml",
    )
}

pub fn release_candidate_files_work_in_container(ctx: &Ctx) -> TestResult {
    verify_shipped_files(
        ctx,
        RuleSelection::Discover,
        "diskgc-release",
        "diskgc-release-tmpfiles.conf",
        "diskgc-release-example.yaml",
    )
}

fn verify_shipped_files(
    ctx: &Ctx,
    rule_selection: RuleSelection,
    binary: &str,
    rule_file: &str,
    example_file: &str,
) -> TestResult {
    let tmpfiles = &ctx.image.tmpfiles;
    let root = ctx.scratch.mkdirs("package-root", 0o755, ROOT)?;
    ctx.scratch
        .mkdirs("package-root/usr/lib/tmpfiles.d", 0o755, ROOT)?;
    // With --root, sd-tmpfiles resolves even the rule's named users against
    // the target root. Give this isolated root the test image's account map.
    ctx.scratch.mkdirs("package-root/etc", 0o755, ROOT)?;
    for file in ["passwd", "group"] {
        let source = format!("/etc/{file}");
        let contents = std::fs::read(&source).map_err(|e| format!("read {source}: {e}"))?;
        ctx.scratch
            .write_file(&format!("package-root/etc/{file}"), &contents, 0o644, ROOT)?;
    }
    let rule = std::fs::read(ctx.bin_dir.join(rule_file))
        .map_err(|e| format!("read shipped tmpfiles rule: {e}"))?;
    ctx.scratch.write_file(
        "package-root/usr/lib/tmpfiles.d/diskgc.conf",
        &rule,
        0o644,
        ROOT,
    )?;

    let mut apply = Command::new(tmpfiles);
    apply
        .arg(format!("--root={}", root.display()))
        .arg("--create");
    if matches!(rule_selection, RuleSelection::Explicit) {
        apply.arg(root.join("usr/lib/tmpfiles.d/diskgc.conf"));
    }
    let applied = apply
        .env_clear()
        .env("PATH", ctx.bin_dir.as_os_str())
        .output()
        .map_err(|e| format!("run {}: {e}", tmpfiles.display()))?;
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

    let checked = Command::new(ctx.bin_dir.join(binary))
        .args(["check", "--config"])
        .arg(ctx.bin_dir.join(example_file))
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
