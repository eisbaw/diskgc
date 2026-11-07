//! Shipped embedded build profile against real filesystem entries.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use crate::ensure_suite::{ensure_run, report};
use crate::oracle::{Fixture, Oracle};
use crate::snapshot::parse_mountinfo;
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure};

/// Newly created tmpfs entries cannot meet the built-in's three-day ctime
/// floor. The same source predicate is exercised without that floor for
/// real deletion; a built-in dry run checks the floor separately.
const WEST_ANCHOR_KERNEL_PROFILE: &str = r#"version: 1
profiles:
  - name: west-anchor-kernel
    find: {roots: required, paths: [build, "*/build", "*/*/build", "*/*/*/build", "*/*/*/*/build"], type: dir}
    where: {has: CMakeCache.txt, has_dir: zephyr, lacks_entry: CMakeLists.txt, source_anchor: cmake}
    action: delete
    examples: {root: /home/tester/project, match: [/home/tester/project/app/build]}
"#;

/// The recorded CMake source, Zephyr marker, and output type must all agree.
/// A missing or out-of-root source remains untouched on the real kernel.
pub fn anchored_west_build_keeps_unrebuildable_outputs(ctx: &Ctx) -> TestResult {
    let home = &ctx.home;
    home.write_file(
        "diskgc.yaml",
        WEST_ANCHOR_KERNEL_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    for rel in [
        "outside",
        "project/app/build/zephyr",
        "project/missing/build/zephyr",
        "project/outside-root/build/zephyr",
        "project/ordinary/build",
        "project/linked/build",
        "project/source/build/zephyr",
    ] {
        home.mkdirs(rel, 0o700, TESTER)?;
    }
    for rel in [
        "project/app/CMakeLists.txt",
        "outside/CMakeLists.txt",
        "project/linked/CMakeLists.txt",
        "project/source/CMakeLists.txt",
    ] {
        home.write_file(rel, b"project(test)\n", 0o600, TESTER)?;
    }
    for (rel, source) in [
        ("project/app/build", "/home/tester/project/app"),
        ("project/missing/build", "/home/tester/project/missing"),
        ("project/outside-root/build", "/home/tester/outside"),
        ("project/ordinary/build", "/home/tester/project/ordinary"),
        ("project/linked/build", "/home/tester/project/linked"),
        ("project/source/build", "/home/tester/project/source"),
    ] {
        home.write_file(
            &format!("{rel}/CMakeCache.txt"),
            format!("CMAKE_HOME_DIRECTORY:INTERNAL={source}\n").as_bytes(),
            0o600,
            TESTER,
        )?;
    }
    for rel in [
        "project/app/build/zephyr/zephyr.elf",
        "project/missing/build/zephyr/precious.elf",
        "project/outside-root/build/zephyr/precious.elf",
    ] {
        home.write_file(rel, b"keep unless source survives", 0o600, TESTER)?;
    }
    home.symlink(
        "project/linked/build/zephyr",
        Path::new("/home/tester/project/app/build/zephyr"),
    )?;
    home.symlink(
        "project/source/build/CMakeLists.txt",
        Path::new("/tmp/nonexistent-source"),
    )?;

    let eligible = home.path("project/app/build");
    let missing = home.path("project/missing/build");
    let outside = home.path("project/outside-root/build");
    let ordinary = home.path("project/ordinary/build");
    let linked = home.path("project/linked/build");
    let source = home.path("project/source/build");
    let oracle = Oracle::arm(Fixture::new().eligible(eligible.clone()))?;

    let dry = ensure_run(
        ctx,
        &[
            "--json",
            "--dry-run",
            "--no-index",
            "--min-free",
            "100%",
            "--select",
            "profile:west-build-anchored",
            "--roots",
            "/home/tester/project",
            "/home/tester",
        ],
    )?;
    dry.expect_code(0)?;
    let plan = report(&dry)?;
    ensure!(
        plan["plan"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry["candidate"].as_str() == eligible.to_str()
                    && entry["reason"].as_str() == Some("not_idle")
                    && entry["build_evidence"]["source"].as_str()
                        == Some("/home/tester/project/app")
            })),
        "built-in source anchor or idle floor missing: {plan}"
    );
    ensure!(
        plan["plan"]
            .as_array()
            .is_some_and(|entries| entries.iter().all(|entry| {
                entry["candidate"].as_str() != missing.to_str()
                    && entry["candidate"].as_str() != outside.to_str()
            })),
        "unrebuildable output planned by the built-in: {plan}"
    );

    let run = ensure_run(
        ctx,
        &[
            "--json",
            "--config",
            "/home/tester/diskgc.yaml",
            "--no-index",
            "--min-free",
            "100%",
            "--select",
            "profile:west-anchor-kernel",
            "--roots",
            "/home/tester/project",
            "/home/tester",
        ],
    )?;
    let verdict = oracle.judge_runs(&[&dry, &run])?;
    run.expect_code(1)?;
    let report = report(&run)?;
    ensure!(
        report["actions"].as_array().is_some_and(|actions| {
            actions.len() == 1 && actions[0]["candidate"].as_str() == eligible.to_str()
        }),
        "West anchor did not delete exactly the rebuildable output: {report}"
    );
    ensure!(!eligible.exists(), "rebuildable Zephyr output remains");
    for kept in [missing, outside, ordinary, linked, source] {
        ensure!(
            kept.exists(),
            "protected output was removed: {}",
            kept.display()
        );
    }
    ensure!(
        home.path("project/missing/build/zephyr/precious.elf")
            .exists(),
        "missing-source output data was removed"
    );
    ensure!(
        home.path("project/outside-root/build/zephyr/precious.elf")
            .exists(),
        "out-of-root source output data was removed"
    );
    verdict.assert_clean()
}

/// Configure and compile a real Zephyr application through pinned West in the
/// disposable container. Both the source and all generated output live on its
/// scratch tmpfs; the immutable Zephyr source and tools come from the Nix store.
fn west_build(ctx: &Ctx, project: &str) -> TestResult {
    west_build_with(ctx, project, "")
}

/// The arguments and environment of `west build` for `project` (below the
/// scratch mount), as [`west_build_with`] runs it.
pub(crate) fn west_invocation(
    ctx: &Ctx,
    project: &str,
) -> (Vec<String>, Vec<(&'static str, String)>) {
    let source = ctx.scratch.path(project);
    let args = vec![
        "build".to_owned(),
        // The 32-bit native_sim variant needs multilib headers, which are
        // intentionally absent from this pinned x86_64 container closure.
        "-b".to_owned(),
        "native_sim/native/64".to_owned(),
        "-s".to_owned(),
        source.display().to_string(),
        "-d".to_owned(),
        source.join("build").display().to_string(),
    ];
    let env = vec![
        ("PATH", format!("{}:/bin", ctx.bin_dir.display())),
        ("HOME", "/home/tester".to_owned()),
        ("LANG", "C.UTF-8".to_owned()),
        (
            "ZEPHYR_BASE",
            ctx.scratch.path("zephyr").display().to_string(),
        ),
        ("ZEPHYR_TOOLCHAIN_VARIANT", "host/gnu".to_owned()),
    ];
    (args, env)
}

/// [`west_build`] of an application whose CMakeLists.txt ends with `extra`
/// (TASK-354 adds a step its live build is held at).
pub(crate) fn west_build_with(ctx: &Ctx, project: &str, extra: &str) -> TestResult {
    let source = ctx.scratch.mkdirs(project, 0o700, TESTER)?;
    ctx.scratch
        .mkdirs(&format!("{project}/src"), 0o700, TESTER)?;
    ctx.scratch.write_file(
        &format!("{project}/CMakeLists.txt"),
        format!(
            "cmake_minimum_required(VERSION 3.20.0)\nfind_package(Zephyr REQUIRED HINTS $ENV{{ZEPHYR_BASE}})\nproject(diskgc_west_fixture)\ntarget_sources(app PRIVATE src/main.c)\n{extra}"
        )
        .as_bytes(),
        0o600,
        TESTER,
    )?;
    ctx.scratch.write_file(
        &format!("{project}/src/main.c"),
        b"#include <zephyr/kernel.h>\nint main(void) { return 0; }\n",
        0o600,
        TESTER,
    )?;
    ctx.scratch
        .write_file(&format!("{project}/prj.conf"), b"\n", 0o600, TESTER)?;
    let output = source.join("build");
    let (args, env) = west_invocation(ctx, project);
    let result = Command::new(ctx.bin_dir.join("west"))
        .current_dir(&source)
        .args(&args)
        .env_clear()
        .envs(env)
        .uid(TESTER.uid)
        .gid(TESTER.gid)
        .output()
        .map_err(|error| format!("run pinned West for {project}: {error}"))?;
    ensure!(
        result.status.success(),
        "West build for {project} failed: status {}, stdout {}, stderr {}; Kconfig outputs: {:?}",
        result.status,
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
        std::fs::read_dir(output.join("Kconfig")).map(|entries| entries
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .collect::<Vec<_>>())
    );
    ensure!(
        output.join("CMakeCache.txt").is_file()
            && output.join("zephyr").is_dir()
            && output.join("zephyr/zephyr.elf").is_file(),
        "West did not produce CMakeCache.txt, zephyr/ and zephyr.elf in {}",
        output.display()
    );
    let cache = std::fs::read_to_string(output.join("CMakeCache.txt"))
        .map_err(|error| format!("read West CMakeCache.txt: {error}"))?;
    ensure!(
        cache
            .lines()
            .any(|line| line == format!("CMAKE_HOME_DIRECTORY:INTERNAL={}", source.display())),
        "West did not record its application source at {}",
        source.display()
    );
    Ok(())
}

/// The shipped profile recognizes actual West output and keeps an equally
/// genuine build after its source file disappears. The no-idle-floor variant
/// proves deletion only for the rebuildable output on the container kernel.
pub fn west_generated_output_uses_surviving_source_anchor(ctx: &Ctx) -> TestResult {
    west_workspace(ctx)?;
    west_build(ctx, "west-app")?;
    west_build(ctx, "west-missing")?;
    west_anchor_scenario(ctx)
}

/// A West workspace on the scratch mount whose manifest is the pinned
/// Zephyr tree, bound read-only at `zephyr`, so West and Zephyr's own CMake
/// find it offline.
pub(crate) fn west_workspace(ctx: &Ctx) -> TestResult {
    // West requires workspace metadata before it loads Zephyr's build command.
    // Point a local, network-isolated workspace at the exact flake-pinned
    // Zephyr tree; West itself generates all build output below each app.
    ctx.scratch.mkdirs(".west", 0o700, TESTER)?;
    ctx.scratch.write_file(
        ".west/config",
        b"[manifest]\npath = zephyr\n",
        0o600,
        TESTER,
    )?;
    // A symlink resolves into /nix/store, outside West's workspace. Bind
    // the pinned source read-only at the workspace path so Zephyr's own
    // CMake subprocess can run `west topdir` from inside that workspace.
    let zephyr = ctx.scratch.mkdirs("zephyr", 0o755, TESTER)?;
    rustix::mount::mount_bind(ctx.bin_dir.join("zephyr-src"), &zephyr)
        .map_err(|error| format!("bind pinned Zephyr source into West workspace: {error}"))?;
    // Podman locked the source mount read-only in its outer namespace, so
    // remounting this bind is forbidden. Check the inherited mount flag.
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| format!("read Zephyr mount flags: {error}"))?;
    ensure!(
        parse_mountinfo(&mountinfo)?
            .iter()
            .any(|mount| { mount.mount_point == zephyr && mount.read_only }),
        "pinned Zephyr source bind at {} is not read-only",
        zephyr.display()
    );
    ctx.home.mkdirs(".cache", 0o700, TESTER)?;
    Ok(())
}

fn west_anchor_scenario(ctx: &Ctx) -> TestResult {
    ctx.scratch.remove_file("west-missing/CMakeLists.txt")?;
    ctx.home.write_file(
        "diskgc.yaml",
        WEST_ANCHOR_KERNEL_PROFILE.as_bytes(),
        0o644,
        TESTER,
    )?;
    let eligible = ctx.scratch.path("west-app/build");
    let missing = ctx.scratch.path("west-missing/build");
    let oracle = Oracle::arm(Fixture::new().eligible(eligible.clone()))?;
    let built_in = ensure_run(
        ctx,
        &[
            "--json",
            "--dry-run",
            "--no-index",
            "--min-free",
            "100%",
            "--select",
            "profile:west-build-anchored",
            "--roots",
            "/tmp",
            "/tmp",
        ],
    )?;
    built_in.expect_code(0)?;
    let plan = report(&built_in)?;
    ensure!(
        plan["plan"]
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry["candidate"].as_str() == eligible.to_str()
                    && entry["reason"].as_str() == Some("not_idle")
                    && entry["build_evidence"]["marker"].as_str() == Some("CMakeCache.txt")
                    && entry["build_evidence"]["source"].as_str() == Some("/tmp/west-app")
            })),
        "shipped West profile did not recognize genuine output with source and idle floor: {plan}"
    );
    ensure!(
        plan["plan"].as_array().is_some_and(|entries| entries
            .iter()
            .all(|entry| { entry["candidate"].as_str() != missing.to_str() })),
        "shipped West profile planned genuine output with missing source: {plan}"
    );
    let run = ensure_run(
        ctx,
        &[
            "--json",
            "--config",
            "/home/tester/diskgc.yaml",
            "--no-index",
            "--min-free",
            "100%",
            "--select",
            "profile:west-anchor-kernel",
            "--roots",
            "/tmp",
            "/tmp",
        ],
    )?;
    let verdict = oracle.judge_runs(&[&built_in, &run])?;
    run.expect_code(1)?;
    let outcome = report(&run)?;
    ensure!(
        outcome["actions"].as_array().is_some_and(|actions| {
            actions.len() == 1 && actions[0]["candidate"].as_str() == eligible.to_str()
        }),
        "West reclaim selected unexpected output: {outcome}"
    );
    ensure!(!eligible.exists(), "rebuildable West output remains");
    ensure!(
        missing.join("zephyr/zephyr.elf").exists(),
        "missing-source West output was removed"
    );
    verdict.assert_clean()
}
