//! Shipped embedded build profile against real filesystem entries.

use std::path::Path;

use crate::ensure_suite::{ensure_run, report};
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

/// A Zephyr-shaped output tree is deleted. Similar build directories and a
/// source tree remain, including a `zephyr` symlink to a real directory that
/// a following stat would mistake for the required direct child directory.
pub fn west_build_requires_zephyr_directory_and_preserves_sources(ctx: &Ctx) -> TestResult {
    let home = &ctx.home;
    for rel in [
        "app/build/zephyr",
        "ordinary/build",
        "linked/build",
        "source/build/zephyr",
    ] {
        home.mkdirs(rel, 0o700, TESTER)?;
    }
    for rel in [
        "app/build/CMakeCache.txt",
        "ordinary/build/CMakeCache.txt",
        "linked/build/CMakeCache.txt",
        "source/build/CMakeCache.txt",
    ] {
        home.write_file(rel, b"CMake cache", 0o600, TESTER)?;
    }
    home.write_file("app/build/zephyr/zephyr.elf", b"rebuildable", 0o600, TESTER)?;
    home.symlink(
        "linked/build/zephyr",
        Path::new("/home/tester/app/build/zephyr"),
    )?;
    home.symlink(
        "source/build/CMakeLists.txt",
        Path::new("/tmp/nonexistent-source"),
    )?;

    let eligible = home.path("app/build");
    let ordinary = home.path("ordinary/build");
    let linked = home.path("linked/build");
    let source = home.path("source/build");
    let oracle = Oracle::arm(Fixture::new().eligible(eligible.clone()))?;
    let run = ensure_run(
        ctx,
        &[
            "--json",
            "--no-index",
            "--min-free",
            "100%",
            "--select",
            "profile:west-build",
            "--roots",
            "/home/tester",
            "/home/tester",
        ],
    )?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(1)?;
    let report = report(&run)?;
    ensure!(
        report["actions"].as_array().is_some_and(|actions| {
            actions.len() == 1 && actions[0]["candidate"].as_str() == eligible.to_str()
        }),
        "west-build did not delete exactly the Zephyr output: {report}"
    );
    ensure!(!eligible.exists(), "Zephyr build output remains");
    ensure!(ordinary.exists(), "ordinary CMake build was removed");
    ensure!(linked.exists(), "symlink marker build was removed");
    ensure!(source.exists(), "source tree was removed");
    verdict.assert_clean()
}
