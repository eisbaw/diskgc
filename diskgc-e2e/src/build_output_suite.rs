//! TASK-354 (folding in TASK-331's container tests): admission evidence for
//! the source-anchored build-output built-ins (`cmake-build-anchored`,
//! `meson-build-anchored`, `autotools-build-anchored`,
//! `west-build-anchored`, `buildroot-output-anchored`,
//! `yocto-tmpdir-anchored`, `bazel-output-base-anchored`),
//! docs/testing.md, Admission evidence.
//!
//! Per profile the pinned real build system configures and builds a tiny
//! project in the container, so every anchor file is the tool's own: CMake
//! 4.4.3 (Ninja and Unix Makefiles), Meson 1.12.1, GNU Autoconf 2.73,
//! Zephyr 4.3.0 with West 1.5.0 (native_sim), Buildroot 2026.08.1 (`make
//! defconfig` and host packages of a br2-external tree with
//! `SITE_METHOD = local`: the containers have no network), BitBake 2.20.0
//! with OpenEmbedded-Core yocto-6.1 (a layer's recipe with a local source,
//! run to a task of its own after `do_unpack`: every default task after it
//! needs native tools fetched from the network) and Bazel 7.6.0 (a genrule
//! in WORKSPACE mode, which needs no module registry). Decoys are made by
//! the same tools wherever they can be: a build whose source moved away, a
//! copied or moved output, a source inside the candidate, an in-source
//! build, a build below a Git checkout (reclaimable), a checkout inside the
//! output, and the layout of a single project's tree (OpenWrt's
//! `build_dir`, hand-made top level, real builds inside). Every profile has
//! a three-day floor and every build here is made now, so each reclaim
//! advances the gated binary's clock past it ([`PAST_THE_FLOOR`]); one dry
//! run per scenario without the offset shows the floor. The live
//! counterexample holds a build step in flight (`ctl/hold.sh`) during the
//! reclaim and requires diskgc to keep that output, `in_use` for a use that
//! `/proc` independently shows (Yocto's: `contains_ipc`, BitBake's task
//! FIFO); the build then completes. Finally the tool
//! configures and builds again from the surviving source.
//!
//! [`candidate_catalog_build_output_family_matches_the_development_build`]
//! runs one fixture of all seven through the release build's catalog code
//! path with the family in its catalog (docs/testing.md, the candidate
//! catalog).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;

use crate::admission::{
    CTL, Expected, HOME, InFlight, TOOL_TIMEOUT, check_candidate_catalog, control_dir, cwds_below,
    dry_run_then_reclaim, environ_below, exists, expect_used_by, home, open_gate, plan_of,
    run_ensure, sorted, tester_command, tool, versions, wait_file, write, write_script,
};
use crate::differential_suite::SandboxClockOffset;
use crate::embedded_suite::{west_build_with, west_invocation, west_workspace};
use crate::ensure_suite::report;
use crate::oracle::{Fixture, Oracle};
use crate::owner_pid_suite::wait_for;
use crate::scratch::Scratch;
use crate::suite::{Ctx, TestResult};
use crate::{ensure, info};

/// The family this suite builds fixtures for (the candidate catalog's
/// check, [`crate::admission::check_candidate_catalog`]).
pub(crate) const FAMILY: &[&str] = &[
    "cmake-build-anchored",
    "meson-build-anchored",
    "autotools-build-anchored",
    "west-build-anchored",
    "buildroot-output-anchored",
    "yocto-tmpdir-anchored",
    "bazel-output-base-anchored",
];

/// Past every profile's `min_idle_for: 3d`.
const PAST_THE_FLOOR: Duration = Duration::from_secs(4 * 24 * 3600);

/// The projects below tester's home, the root of the home-rooted profiles.
const SRC: &str = "src";
const SRC_ROOT: &str = "/home/tester/src";

/// Holds a build step while `ctl/NAME.hold` exists: writes `ctl/NAME.ready`
/// and waits for `ctl/NAME.go`. Touches its second argument, if any (a
/// build rule's output).
const HOLD_SH: &[u8] = b"#!/bin/sh
ctl=/home/tester/ctl
if [ -e \"$ctl/$1.hold\" ]; then
  echo $$ > \"$ctl/$1.ready\"
  while [ ! -e \"$ctl/$1.go\" ]; do sleep 0.1; done
fi
if [ -n \"$2\" ]; then : > \"$2\"; fi
";

const MAIN_C: &[u8] = b"#include <stdio.h>\nint main(void) { puts(\"built\"); return 0; }\n";

fn src(rel: &str) -> PathBuf {
    home(&format!("{SRC}/{rel}"))
}

fn bin(ctx: &Ctx) -> Result<&str, String> {
    ctx.bin_dir
        .to_str()
        .ok_or_else(|| "non-UTF-8 bin dir".to_owned())
}

/// The under-test bundle's tools (CMake, Meson, Ninja, Autoconf, Make, the
/// C compiler, Git, Bazel) before busybox.
fn tools_path(ctx: &Ctx) -> Result<String, String> {
    Ok(format!("{}:{}/git-bin:/bin", bin(ctx)?, bin(ctx)?))
}

fn select(profile: &str) -> String {
    format!("profile:{profile}")
}

fn display(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("non-UTF-8 path {}", path.display()))
}

/// The control directory with the hold script.
fn control(ctx: &Ctx) -> TestResult {
    control_dir(ctx)?;
    write_script(ctx, &format!("{CTL}/hold.sh"), HOLD_SH)
}

/// Makes the next run of the step named `name` wait (see [`HOLD_SH`]).
fn hold(ctx: &Ctx, name: &str) -> TestResult {
    write(ctx, &format!("{CTL}/{name}.hold"), b"hold\n")
}

fn wait_held(run: &mut InFlight, name: &str) -> TestResult {
    wait_file(run, &home(&format!("{CTL}/{name}.ready")))
}

/// Renames `from` to `to` as tester (a moved source or output).
fn mv(ctx: &Ctx, from: &Path, to: &Path) -> TestResult {
    tool(
        ctx,
        Path::new(HOME),
        &[],
        "/bin/mv",
        &[display(from)?, display(to)?],
    )?;
    Ok(())
}

/// Copies `from` to `to` as tester, keeping times (a copied output).
fn cp(ctx: &Ctx, from: &Path, to: &Path) -> TestResult {
    tool(
        ctx,
        Path::new(HOME),
        &[],
        "/bin/cp",
        &["-a", display(from)?, display(to)?],
    )?;
    Ok(())
}

/// `git init` in `dir` as tester: `dir` becomes a checkout.
fn git_init(ctx: &Ctx, dir: &Path) -> TestResult {
    tool(
        ctx,
        dir,
        &[("PATH", &tools_path(ctx)?)],
        ctx.bin_dir.join("git-bin/git"),
        &["init", "-q", "."],
    )?;
    ensure!(dir.join(".git/HEAD").is_file(), "git init made no checkout");
    Ok(())
}

/// Runs a real tool that must fail, returning its output.
fn tool_fails(
    ctx: &Ctx,
    cwd: &Path,
    env: &[(&str, &str)],
    program: impl AsRef<Path>,
    args: &[&str],
) -> Result<String, String> {
    let program = program.as_ref();
    let output = tester_command(ctx, cwd, env, program, args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("start {}: {e}", program.display()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    info!(
        "{} {args:?} (expected to fail): {}\n{text}",
        program.display(),
        output.status
    );
    ensure!(
        !output.status.success(),
        "{} {args:?} succeeded, expected a refusal:\n{text}",
        program.display()
    );
    Ok(text)
}

/// A dry run of `select` with `binary` below `root`, judged by an oracle
/// that allows no deletion; with the clock past the floor when `past`.
/// Returns its `(candidate, reason)` plan.
fn dry_plan(
    ctx: &Ctx,
    binary: &Path,
    select: &str,
    root: &str,
    past: bool,
) -> Result<Vec<(String, Option<String>)>, String> {
    let _clock = past
        .then(|| SandboxClockOffset::install(PAST_THE_FLOOR.as_secs()))
        .transpose()?;
    let oracle = Oracle::arm(Fixture::new())?;
    let dry = run_ensure(ctx, binary, true, select, Some(root), root)?;
    oracle.judge(&dry)?.assert_clean()?;
    dry.expect_code(0)?;
    plan_of(&report(&dry)?)
}

/// Without the clock offset every candidate in `fresh` is `not_idle` (the
/// three-day floor), and the plan finds no candidate outside `found`.
fn expect_floor(
    ctx: &Ctx,
    profile: &str,
    root: &str,
    fresh: &[&Path],
    found: &Expected,
) -> TestResult {
    let plan = dry_plan(ctx, &ctx.diskgc(), &select(profile), root, false)?;
    for path in fresh {
        ensure!(
            plan.iter()
                .any(|(c, r)| c == &path.display().to_string() && r.as_deref() == Some("not_idle")),
            "{profile}: {} is not not_idle before the floor: {plan:?}",
            path.display()
        );
    }
    let known: Vec<String> = found.iter().map(|(p, _)| p.display().to_string()).collect();
    ensure!(
        plan.iter().all(|(c, _)| known.contains(c)),
        "{profile}: the plan before the floor found {plan:?}, beyond {known:?}"
    );
    Ok(())
}

/// The dry run and reclaim of one profile past the floor with `binary`,
/// under the deletion oracle ([`dry_run_then_reclaim`]).
fn reclaim_past_floor(
    ctx: &Ctx,
    binary: &Path,
    profile: &str,
    root: &str,
    expected: &Expected,
) -> Result<Value, String> {
    let _clock = SandboxClockOffset::install(PAST_THE_FLOOR.as_secs())?;
    dry_run_then_reclaim(ctx, binary, &select(profile), Some(root), root, expected)
}

// ---- the single-project decoy ---------------------------------------------

/// OpenWrt's top-level layout (hand-made: a real OpenWrt tree needs its
/// feeds and toolchain downloads) at `src/openwrt`, with `build_dir`'s
/// per-package directories, in which the scenarios build real packages the
/// way OpenWrt's include/*.mk do. Returns the target build directory.
fn openwrt_like(ctx: &Ctx) -> Result<PathBuf, String> {
    for (rel, text) in [
        (
            "openwrt/Makefile",
            "include $(TOPDIR)/include/toplevel.mk\n",
        ),
        (
            "openwrt/rules.mk",
            "BUILD_DIR:=$(TOPDIR)/build_dir/$(TARGET_DIR_NAME)\n",
        ),
        ("openwrt/include/toplevel.mk", "# OpenWrt top level\n"),
        (
            "openwrt/feeds.conf.default",
            "src-git packages https://git.openwrt.org/feed/packages.git\n",
        ),
        (
            "openwrt/.config",
            "CONFIG_TARGET_x86=y\nCONFIG_TARGET_x86_64=y\n",
        ),
        ("openwrt/staging_dir/host/.prereq-build", ""),
    ] {
        write(ctx, &format!("{SRC}/{rel}"), text.as_bytes())?;
    }
    // The layout the scenarios build in is OpenWrt's own (TASK-367): its
    // pinned make rules, read, never run.
    for (file, rule) in [
        (
            "meson.mk",
            "MESON_BUILD_DIR:=$(PKG_BUILD_DIR)/openwrt-build",
        ),
        (
            "cmake.mk",
            "CMAKE_BINARY_DIR = $(PKG_BUILD_DIR)$(if $(CMAKE_BINARY_SUBDIR),/$(CMAKE_BINARY_SUBDIR))",
        ),
    ] {
        ensure!(
            openwrt_rules(ctx, file)?.lines().any(|line| line == rule),
            "OpenWrt's include/{file} no longer says {rule:?}: update the OpenWrt-like layout"
        );
    }
    Ok(src("openwrt/build_dir/target-x86_64_musl"))
}

/// One of OpenWrt v25.12.6's make rules (`include/FILE`).
fn openwrt_rules(ctx: &Ctx, file: &str) -> Result<String, String> {
    let path = ctx.bin_dir.join("openwrt-rules").join(file);
    std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))
}

/// The stamp file `variable` names in `$(PKG_BUILD_DIR)` in OpenWrt's
/// include/package.mk, as a build writes it: a name followed by a make
/// expression (`.configured$(if $(DUMP),,_$(call confvar,...))`) gets a
/// hash-shaped suffix, as it does outside `make DUMP=1`.
fn openwrt_stamp(package_mk: &str, variable: &str) -> Result<String, String> {
    const DIR: &str = "$(PKG_BUILD_DIR)/";
    let value = package_mk
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix(variable))
        .filter_map(|rest| rest.strip_prefix(":=").or_else(|| rest.strip_prefix('=')))
        .find_map(|value| value.strip_prefix(DIR))
        .ok_or_else(|| format!("include/package.mk sets no {variable} in {DIR}"))?;
    Ok(match value.split_once("$(") {
        Some((name, _)) => format!("{name}_68b329da9893e34099c7d8ad5cb9c940"),
        None => value.to_owned(),
    })
}

/// The stamps a completed OpenWrt target package build leaves in its
/// `PKG_BUILD_DIR` `pkg` (prepared, configured, built), named by the pinned
/// include/package.mk. Returns them.
fn openwrt_stamps(ctx: &Ctx, pkg: &Path) -> Result<Vec<String>, String> {
    let package_mk = openwrt_rules(ctx, "package.mk")?;
    let mut stamps = Vec::new();
    for variable in ["STAMP_PREPARED", "STAMP_CONFIGURED", "STAMP_BUILT"] {
        let stamp = openwrt_stamp(&package_mk, variable)?;
        let rel = pkg
            .join(&stamp)
            .strip_prefix(HOME)
            .map_err(|_| format!("{} is not below {HOME}", pkg.display()))?
            .display()
            .to_string();
        write(ctx, &rel, b"")?;
        stamps.push(stamp);
    }
    info!("OpenWrt's stamps in {}: {stamps:?}", pkg.display());
    Ok(stamps)
}

// ---- cmake-build-anchored --------------------------------------------------

const CMAKE_LISTS: &[u8] = b"cmake_minimum_required(VERSION 3.20)
project(app C)
add_executable(app main.c)
add_custom_target(hold ALL COMMAND sh /home/tester/ctl/hold.sh cmake)
";

fn cmake_project(ctx: &Ctx, rel: &str) -> Result<PathBuf, String> {
    write(ctx, &format!("{SRC}/{rel}/CMakeLists.txt"), CMAKE_LISTS)?;
    write(ctx, &format!("{SRC}/{rel}/main.c"), MAIN_C)?;
    Ok(src(rel))
}

/// `cmake -S source -B build -G generator`, then `cmake --build build`.
fn cmake_build(ctx: &Ctx, source: &Path, build: &Path, generator: &str) -> TestResult {
    let path = tools_path(ctx)?;
    let cmake = ctx.bin_dir.join("cmake");
    tool(
        ctx,
        Path::new(HOME),
        &[("PATH", &path)],
        &cmake,
        &[
            "-S",
            display(source)?,
            "-B",
            display(build)?,
            "-G",
            generator,
        ],
    )?;
    tool(
        ctx,
        Path::new(HOME),
        &[("PATH", &path)],
        &cmake,
        &["--build", display(build)?],
    )?;
    ensure!(
        build.join("CMakeCache.txt").is_file() && build.join("CMakeFiles").is_dir(),
        "CMake made no CMakeCache.txt and CMakeFiles/ in {}",
        build.display()
    );
    Ok(())
}

/// The program a build made runs.
fn runs(ctx: &Ctx, program: &Path) -> TestResult {
    let out = tool(ctx, Path::new(HOME), &[], program, &[])?;
    ensure!(
        out.contains("built"),
        "{} printed {out:?}",
        program.display()
    );
    Ok(())
}

/// docs/testing.md, Admission evidence (`cmake-build-anchored`). Real CMake
/// builds below `src`: with Ninja (`app/out`), with Unix Makefiles at depth
/// five (`app/n1/n2/n3/make-build`) and below a Git checkout (`repo/build`)
/// are reclaimed past the floor. Never found: an in-source build, one at
/// depth six. Found but refused by the anchor (not planned): a build whose
/// project moved away, one whose source lies inside it (`wrap/` with
/// `wrap/src`). Kept: a build holding a checkout (`contains_checkout`,
/// FetchContent's layout) and a build in flight, held at a custom target
/// (`in_use`: Make's working directory). In OpenWrt's layout an in-source
/// package build is never found, and a package built in its
/// `openwrt-build` subdirectory (`CMAKE_BINARY_SUBDIR`) beside OpenWrt's
/// stamps is not planned (TASK-367, the nested-build guard), nor is an
/// addon built in cmake-js's layout below `node_modules`. CMake then
/// configures and builds the reclaimed outputs again.
pub fn cmake_build_anchored_real_cmake_builds_reclaim_then_reconfigure_and_a_build_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    let cmake = ctx.bin_dir.join("cmake");
    versions(
        ctx,
        &[
            (display(&cmake)?, &["--version"], "cmake version 4.4.3"),
            (
                display(&ctx.bin_dir.join("ninja"))?,
                &["--version"],
                "1.13.2",
            ),
            (
                display(&ctx.bin_dir.join("make"))?,
                &["--version"],
                "GNU Make 4.4.1",
            ),
        ],
    )?;
    control(ctx)?;
    let app = cmake_project(ctx, "app")?;
    let out = app.join("out");
    cmake_build(ctx, &app, &out, "Ninja")?;
    let deep_ok = app.join("n1/n2/n3/make-build");
    cmake_build(ctx, &app, &deep_ok, "Unix Makefiles")?;
    let too_deep = app.join("n1/n2/n3/n4/too-deep");
    cmake_build(ctx, &app, &too_deep, "Ninja")?;
    let repo = cmake_project(ctx, "repo")?;
    git_init(ctx, &repo)?;
    let repo_build = repo.join("build");
    cmake_build(ctx, &repo, &repo_build, "Ninja")?;
    let insrc = cmake_project(ctx, "insrc")?;
    cmake_build(ctx, &insrc, &insrc, "Unix Makefiles")?;
    let moved = cmake_project(ctx, "moved")?;
    cmake_build(ctx, &moved, &moved.join("build"), "Ninja")?;
    mv(ctx, &moved, &src("moved-away"))?;
    let moved_build = src("moved-away/build");
    let wrap_src = cmake_project(ctx, "wrap/src")?;
    let wrap = src("wrap");
    cmake_build(ctx, &wrap_src, &wrap, "Ninja")?;
    let fetch = app.join("out-fetch");
    cmake_build(ctx, &app, &fetch, "Ninja")?;
    ctx.home.mkdirs(
        &format!("{SRC}/app/out-fetch/_deps/dep-src"),
        0o755,
        crate::TESTER,
    )?;
    git_init(ctx, &fetch.join("_deps/dep-src"))?;
    let openwrt = openwrt_like(ctx)?;
    let ow_insrc = cmake_project(ctx, "openwrt/build_dir/target-x86_64_musl/cminsrc-1.0")?;
    cmake_build(ctx, &ow_insrc, &ow_insrc, "Unix Makefiles")?;
    let ow_sub = cmake_project(ctx, "openwrt/build_dir/target-x86_64_musl/cmsub-1.0")?;
    let ow_sub_build = ow_sub.join("openwrt-build");
    cmake_build(ctx, &ow_sub, &ow_sub_build, "Unix Makefiles")?;
    openwrt_stamps(ctx, &ow_sub)?;
    // cmake-js's layout: an addon configured in its package's build/ below
    // npm's node_modules (its documented default `--out`; cmake-js itself
    // is not in the image).
    let addon = cmake_project(ctx, "napp/node_modules/addon")?;
    let addon_build = addon.join("build");
    cmake_build(ctx, &addon, &addon_build, "Unix Makefiles")?;
    let live = cmake_project(ctx, "live")?;
    let live_build = live.join("build");
    cmake_build(ctx, &live, &live_build, "Unix Makefiles")?;
    info!("OpenWrt-like tree at {}", openwrt.display());

    let expected: Expected = vec![
        (out.clone(), None),
        (deep_ok.clone(), None),
        (repo_build.clone(), None),
        (fetch.clone(), Some("contains_checkout")),
        (live_build.clone(), Some("in_use")),
    ];
    expect_floor(
        ctx,
        "cmake-build-anchored",
        SRC_ROOT,
        &[&out, &deep_ok, &repo_build],
        &expected,
    )?;

    hold(ctx, "cmake")?;
    let path = tools_path(ctx)?;
    let mut build = InFlight::start(
        ctx,
        "cmake-live",
        Path::new(HOME),
        &[("PATH", &path)],
        display(&cmake)?,
        &["--build", display(&live_build)?],
    )?;
    wait_held(&mut build, "cmake")?;
    let reclaimed = reclaim_past_floor(
        ctx,
        &ctx.diskgc(),
        "cmake-build-anchored",
        SRC_ROOT,
        &expected,
    )?;
    expect_used_by(&reclaimed, &live_build, &build, &["cwd"], &|u| {
        u == live_build.display().to_string()
    })?;
    open_gate(ctx, "cmake")?;
    build.expect_success()?;
    runs(ctx, &live_build.join("app"))?;
    for kept in [&too_deep, &insrc, &moved_build, &wrap] {
        ensure!(
            kept.join("CMakeCache.txt").is_file(),
            "{} lost its CMakeCache.txt",
            kept.display()
        );
    }
    ensure!(
        ow_insrc.join("CMakeCache.txt").is_file()
            && ow_sub_build.join("CMakeCache.txt").is_file()
            && addon_build.join("CMakeCache.txt").is_file(),
        "an OpenWrt package build or the node_modules addon build changed"
    );

    cmake_build(ctx, &app, &out, "Ninja")?;
    cmake_build(ctx, &app, &deep_ok, "Unix Makefiles")?;
    cmake_build(ctx, &repo, &repo_build, "Ninja")?;
    for program in [out.join("app"), deep_ok.join("app"), repo_build.join("app")] {
        runs(ctx, &program)?;
    }
    Ok(())
}

// ---- meson-build-anchored --------------------------------------------------

const MESON_BUILD: &[u8] = b"project('app', 'c')
executable('app', 'main.c')
custom_target('hold', output: 'hold.stamp',
  command: ['sh', '/home/tester/ctl/hold.sh', 'meson', '@OUTPUT@'],
  build_by_default: true, build_always_stale: true)
";

fn meson_project(ctx: &Ctx, rel: &str) -> Result<PathBuf, String> {
    write(ctx, &format!("{SRC}/{rel}/meson.build"), MESON_BUILD)?;
    write(ctx, &format!("{SRC}/{rel}/main.c"), MAIN_C)?;
    Ok(src(rel))
}

/// `meson setup build source`, then `ninja -C build`.
fn meson_build(ctx: &Ctx, source: &Path, build: &Path) -> TestResult {
    let path = tools_path(ctx)?;
    tool(
        ctx,
        Path::new(HOME),
        &[("PATH", &path)],
        ctx.bin_dir.join("meson"),
        &["setup", display(build)?, display(source)?],
    )?;
    tool(
        ctx,
        Path::new(HOME),
        &[("PATH", &path)],
        ctx.bin_dir.join("ninja"),
        &["-C", display(build)?],
    )?;
    ensure!(
        build.join("build.ninja").is_file()
            && build.join("meson-private/coredata.dat").is_file()
            && build.join("meson-info/meson-info.json").is_file(),
        "Meson made no build.ninja, coredata.dat and meson-info.json in {}",
        build.display()
    );
    Ok(())
}

/// docs/testing.md, Admission evidence (`meson-build-anchored`). Real Meson
/// builds below `src` (`app/bld`, below a Git checkout `repo/bld`) are
/// reclaimed past the floor. Meson itself refuses an in-source build and a
/// build directory around its source. A build whose project moved away and
/// a copy of a build (their `meson-info.json` names another directory) are
/// refused visibly: `unsupported_layout`, reported at that file. Kept: a build holding a checkout and a
/// Ninja in flight held at a custom target (`in_use`, its working
/// directory). OpenWrt's meson.mk builds every package in
/// `PKG_BUILD_DIR/openwrt-build`: beside OpenWrt's stamps that build is not
/// planned (TASK-367, the nested-build guard). Meson then sets up and
/// builds again.
pub fn meson_build_anchored_real_meson_builds_reclaim_then_reconfigure_and_a_ninja_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    let meson = ctx.bin_dir.join("meson");
    versions(
        ctx,
        &[
            (display(&meson)?, &["--version"], "1.12.1"),
            (
                display(&ctx.bin_dir.join("ninja"))?,
                &["--version"],
                "1.13.2",
            ),
        ],
    )?;
    control(ctx)?;
    let path = tools_path(ctx)?;
    let app = meson_project(ctx, "app")?;
    let bld = app.join("bld");
    meson_build(ctx, &app, &bld)?;
    let refusal = tool_fails(ctx, &app, &[("PATH", &path)], &meson, &["setup", ".", "."])?;
    info!("Meson refuses an in-source build: {}", refusal.trim());
    let repo = meson_project(ctx, "repo")?;
    git_init(ctx, &repo)?;
    let repo_bld = repo.join("bld");
    meson_build(ctx, &repo, &repo_bld)?;
    let moved = meson_project(ctx, "moved")?;
    meson_build(ctx, &moved, &moved.join("bld"))?;
    mv(ctx, &moved, &src("moved-away"))?;
    let copy = app.join("bld-copy");
    cp(ctx, &bld, &copy)?;
    let wrap_src = meson_project(ctx, "wrap/src")?;
    let wrap = src("wrap");
    let wrapped = tool_fails(
        ctx,
        Path::new(HOME),
        &[("PATH", &path)],
        &meson,
        &["setup", display(&wrap)?, display(&wrap_src)?],
    )?;
    ensure!(
        wrapped.contains("cannot be a parent of source directory"),
        "Meson refused a build directory around its source for another reason: {wrapped}"
    );
    let fetch = app.join("bld-sub");
    meson_build(ctx, &app, &fetch)?;
    ctx.home.mkdirs(
        &format!("{SRC}/app/bld-sub/subprojects/dep"),
        0o755,
        crate::TESTER,
    )?;
    git_init(ctx, &fetch.join("subprojects/dep"))?;
    openwrt_like(ctx)?;
    let ow_pkg = meson_project(ctx, "openwrt/build_dir/target-x86_64_musl/mspkg-1.0")?;
    let ow_build = ow_pkg.join("openwrt-build");
    meson_build(ctx, &ow_pkg, &ow_build)?;
    openwrt_stamps(ctx, &ow_pkg)?;
    let live = meson_project(ctx, "live")?;
    let live_bld = live.join("bld");
    meson_build(ctx, &live, &live_bld)?;

    let expected: Expected = vec![
        (bld.clone(), None),
        (repo_bld.clone(), None),
        (fetch.clone(), Some("contains_checkout")),
        (live_bld.clone(), Some("in_use")),
        // A moved or copied build's meson-info.json names another build
        // directory: a visible refusal, reported at the marker file.
        (
            copy.join("meson-info/meson-info.json"),
            Some("unsupported_layout"),
        ),
        (
            src("moved-away/bld/meson-info/meson-info.json"),
            Some("unsupported_layout"),
        ),
    ];
    expect_floor(
        ctx,
        "meson-build-anchored",
        SRC_ROOT,
        &[&bld, &repo_bld],
        &expected,
    )?;

    hold(ctx, "meson")?;
    let ninja = ctx.bin_dir.join("ninja");
    let mut build = InFlight::start(
        ctx,
        "ninja-live",
        Path::new(HOME),
        &[("PATH", &path)],
        display(&ninja)?,
        &["-C", display(&live_bld)?],
    )?;
    wait_held(&mut build, "meson")?;
    let reclaimed = reclaim_past_floor(
        ctx,
        &ctx.diskgc(),
        "meson-build-anchored",
        SRC_ROOT,
        &expected,
    )?;
    expect_used_by(&reclaimed, &live_bld, &build, &["cwd", "fd"], &|u| {
        u == live_bld.display().to_string()
    })?;
    open_gate(ctx, "meson")?;
    build.expect_success()?;
    for kept in [src("moved-away/bld"), copy.clone(), ow_build.clone()] {
        ensure!(
            kept.join("build.ninja").is_file(),
            "{} lost its build.ninja",
            kept.display()
        );
    }
    meson_build(ctx, &app, &bld)?;
    meson_build(ctx, &repo, &repo_bld)?;
    runs(ctx, &bld.join("app"))?;
    runs(ctx, &repo_bld.join("app"))?;
    Ok(())
}

// ---- autotools-build-anchored ----------------------------------------------

const CONFIGURE_AC: &[u8] = b"AC_INIT([app],[1.0])
AC_PROG_CC
AC_CONFIG_FILES([Makefile])
AC_OUTPUT
";
const MAKEFILE_IN: &[u8] = b"srcdir = @srcdir@
CC = @CC@
all: app
app: $(srcdir)/main.c
\tsh /home/tester/ctl/hold.sh autotools
\t$(CC) -o $@ $(srcdir)/main.c
";

/// An Autoconf project at `src/REL` with `configure` generated by the real
/// Autoconf.
fn autotools_project(ctx: &Ctx, rel: &str) -> Result<PathBuf, String> {
    write(ctx, &format!("{SRC}/{rel}/configure.ac"), CONFIGURE_AC)?;
    write(ctx, &format!("{SRC}/{rel}/Makefile.in"), MAKEFILE_IN)?;
    write(ctx, &format!("{SRC}/{rel}/main.c"), MAIN_C)?;
    let source = src(rel);
    tool(
        ctx,
        &source,
        &[("PATH", &tools_path(ctx)?)],
        ctx.bin_dir.join("autoconf"),
        &[],
    )?;
    Ok(source)
}

/// `configure` (run as `configure_as`, relative or absolute) and `make` in
/// `build`, made first.
fn autotools_build(ctx: &Ctx, build: &Path, configure_as: &str) -> TestResult {
    let path = tools_path(ctx)?;
    tool(
        ctx,
        Path::new(HOME),
        &[],
        "/bin/mkdir",
        &["-p", display(build)?],
    )?;
    tool(ctx, build, &[("PATH", &path)], configure_as, &[])?;
    tool(
        ctx,
        build,
        &[("PATH", &path)],
        ctx.bin_dir.join("make"),
        &[],
    )?;
    ensure!(
        ["config.status", "config.log", "Makefile"]
            .iter()
            .all(|f| build.join(f).is_file()),
        "configure made no config.status, config.log and Makefile in {}",
        build.display()
    );
    Ok(())
}

/// docs/testing.md, Admission evidence (`autotools-build-anchored`). Real
/// Autoconf 2.73 `configure` builds below `src` run as `../configure`
/// (`app/build`, `srcdir=..`), by absolute path at depth five
/// (`app/a/b/c/abs`) and below a Git checkout (`repo/build`) are reclaimed
/// past the floor. Never found: an in-source build (and OpenWrt's default,
/// in-source package builds). Refused by the anchor (not planned): a build
/// whose project moved away, a copy of a build (`ac_pwd` names the
/// original). Kept: a build holding a checkout and a Make in flight held in
/// a rule (`in_use`, its working directory). The reclaimed builds then
/// configure and build again.
pub fn autotools_build_anchored_real_configure_builds_reclaim_then_reconfigure_and_a_make_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    versions(
        ctx,
        &[
            (
                display(&ctx.bin_dir.join("autoconf"))?,
                &["--version"],
                "autoconf (GNU Autoconf) 2.73",
            ),
            (
                display(&ctx.bin_dir.join("make"))?,
                &["--version"],
                "GNU Make 4.4.1",
            ),
        ],
    )?;
    control(ctx)?;
    let app = autotools_project(ctx, "app")?;
    let build = app.join("build");
    autotools_build(ctx, &build, "../configure")?;
    let abs = app.join("a/b/c/abs");
    let configure = app.join("configure");
    autotools_build(ctx, &abs, display(&configure)?)?;
    let repo = autotools_project(ctx, "repo")?;
    git_init(ctx, &repo)?;
    let repo_build = repo.join("build");
    autotools_build(ctx, &repo_build, "../configure")?;
    let insrc = autotools_project(ctx, "insrc")?;
    autotools_build(ctx, &insrc, "./configure")?;
    let moved = autotools_project(ctx, "moved")?;
    autotools_build(ctx, &moved.join("build"), "../configure")?;
    mv(ctx, &moved, &src("moved-away"))?;
    let copy = app.join("build-copy");
    cp(ctx, &build, &copy)?;
    let fetch = app.join("build-git");
    autotools_build(ctx, &fetch, "../configure")?;
    ctx.home.mkdirs(
        &format!("{SRC}/app/build-git/vendor/dep"),
        0o755,
        crate::TESTER,
    )?;
    git_init(ctx, &fetch.join("vendor/dep"))?;
    openwrt_like(ctx)?;
    let ow_pkg = autotools_project(ctx, "openwrt/build_dir/target-x86_64_musl/atpkg-1.0")?;
    autotools_build(ctx, &ow_pkg, "./configure")?;
    let live = autotools_project(ctx, "live")?;
    let live_build = live.join("build");
    autotools_build(ctx, &live_build, "../configure")?;
    // The live build's rule runs again: its target goes.
    tool(ctx, &live_build, &[], "/bin/mv", &["app", "app.previous"])?;

    let expected: Expected = vec![
        (build.clone(), None),
        (abs.clone(), None),
        (repo_build.clone(), None),
        (fetch.clone(), Some("contains_checkout")),
        (live_build.clone(), Some("in_use")),
    ];
    expect_floor(
        ctx,
        "autotools-build-anchored",
        SRC_ROOT,
        &[&build, &abs, &repo_build],
        &expected,
    )?;

    hold(ctx, "autotools")?;
    let path = tools_path(ctx)?;
    let make = ctx.bin_dir.join("make");
    let mut run = InFlight::start(
        ctx,
        "make-live",
        &live_build,
        &[("PATH", &path)],
        display(&make)?,
        &[],
    )?;
    wait_held(&mut run, "autotools")?;
    let reclaimed = reclaim_past_floor(
        ctx,
        &ctx.diskgc(),
        "autotools-build-anchored",
        SRC_ROOT,
        &expected,
    )?;
    expect_used_by(&reclaimed, &live_build, &run, &["cwd"], &|u| {
        u == live_build.display().to_string()
    })?;
    open_gate(ctx, "autotools")?;
    run.expect_success()?;
    runs(ctx, &live_build.join("app"))?;
    for kept in [
        src("moved-away/build"),
        copy.clone(),
        insrc.clone(),
        ow_pkg.clone(),
    ] {
        ensure!(
            kept.join("config.status").is_file(),
            "{} lost its config.status",
            kept.display()
        );
    }
    autotools_build(ctx, &build, "../configure")?;
    autotools_build(ctx, &abs, display(&configure)?)?;
    autotools_build(ctx, &repo_build, "../configure")?;
    for program in [build.join("app"), abs.join("app"), repo_build.join("app")] {
        runs(ctx, &program)?;
    }
    Ok(())
}

// ---- west-build-anchored ---------------------------------------------------

/// The step a West build is held at: a custom target every build runs.
const WEST_HOLD: &str = "add_custom_target(diskgc_hold ALL COMMAND sh /home/tester/ctl/hold.sh west)\nadd_dependencies(app diskgc_hold)\n";
const WEST_ROOT: &str = "/tmp";

/// docs/testing.md, Admission evidence (`west-build-anchored`). Real West
/// 1.5.0 builds of a Zephyr 4.3.0 native_sim application (the pinned
/// Zephyr tree bound read-only into an offline West workspace on the
/// scratch mount, root `/tmp`): `west-app/build` is reclaimed past the
/// floor; a build whose application moved away is refused by the anchor
/// (not planned); a `west build` in flight, held at a custom target
/// (`in_use`: Ninja's working directory), is kept and completes. `west
/// build` then builds the reclaimed output again (`zephyr.elf`).
pub fn west_build_anchored_real_west_builds_reclaim_then_rebuild_and_a_build_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    versions(
        ctx,
        &[(
            display(&ctx.bin_dir.join("west"))?,
            &["--version"],
            "v1.5.0",
        )],
    )?;
    control(ctx)?;
    west_workspace(ctx)?;
    west_build_with(ctx, "west-app", WEST_HOLD)?;
    west_build_with(ctx, "west-moved", WEST_HOLD)?;
    west_build_with(ctx, "west-live", WEST_HOLD)?;
    let moved_to = ctx.scratch.path("west-moved-away");
    tool(
        ctx,
        Path::new(HOME),
        &[],
        "/bin/mv",
        &[
            display(&ctx.scratch.path("west-moved"))?,
            display(&moved_to)?,
        ],
    )?;
    let app = ctx.scratch.path("west-app/build");
    let live = ctx.scratch.path("west-live/build");
    let expected: Expected = vec![(app.clone(), None), (live.clone(), Some("in_use"))];
    expect_floor(ctx, "west-build-anchored", WEST_ROOT, &[&app], &expected)?;

    hold(ctx, "west")?;
    let (args, env) = west_invocation(ctx, "west-live");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let west = ctx.bin_dir.join("west");
    let mut build = InFlight::start(
        ctx,
        "west-live",
        &ctx.scratch.path("west-live"),
        &env,
        display(&west)?,
        &args,
    )?;
    wait_held(&mut build, "west")?;
    let reclaimed = reclaim_past_floor(
        ctx,
        &ctx.diskgc(),
        "west-build-anchored",
        WEST_ROOT,
        &expected,
    )?;
    expect_used_by(&reclaimed, &live, &build, &["cwd", "fd"], &|u| {
        u == live.display().to_string()
    })?;
    open_gate(ctx, "west")?;
    build.expect_success()?;
    ensure!(
        moved_to.join("build/zephyr/zephyr.elf").is_file(),
        "the moved application's output changed"
    );
    let (args, env) = west_invocation(ctx, "west-app");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    tool(ctx, &ctx.scratch.path("west-app"), &env, &west, &args)?;
    ensure!(
        app.join("zephyr/zephyr.elf").is_file(),
        "west build did not build the reclaimed output again"
    );
    Ok(())
}

// ---- buildroot-output-anchored ---------------------------------------------

const BR_EXT: &str = "br-ext";

/// A br2-external tree (the user's own source) with three host packages
/// built from local sources: `host-hello` (a generic package, selected by
/// the tree's `diskgc_defconfig`), `host-cmk` (Buildroot's own
/// cmake-package infrastructure with `SUPPORTS_IN_SOURCE_BUILD = NO`, so
/// real CMake configures it in `buildroot-build`; it depends on
/// host-pkgconf, whose pinned source goes into the tree's `dl/`) and
/// `host-msn` (real Meson in `buildroot-build`, run as pkg-meson.mk runs
/// it: its host-meson needs host-python3 from the network).
fn br_external(ctx: &Ctx) -> TestResult {
    let bin = bin(ctx)?;
    let msn_mk = format!(
        "HOST_MSN_VERSION = 1.0
HOST_MSN_SITE = $(BR2_EXTERNAL_DISKGC_PATH)/src/msn
HOST_MSN_SITE_METHOD = local
define HOST_MSN_CONFIGURE_CMDS
\trm -rf $(@D)/buildroot-build
\tmkdir -p $(@D)/buildroot-build
\tPATH={bin}:$$PATH {bin}/meson setup $(@D) $(@D)/buildroot-build
endef
define HOST_MSN_BUILD_CMDS
\t{bin}/ninja -C $(@D)/buildroot-build
endef
$(eval $(host-generic-package))
"
    );
    let files: &[(&str, &str)] = &[
        (
            "external.desc",
            "name: DISKGC\ndesc: diskgc admission fixture\n",
        ),
        (
            "external.mk",
            "include $(sort $(wildcard $(BR2_EXTERNAL_DISKGC_PATH)/package/*/*.mk))\n",
        ),
        (
            "Config.in",
            "config BR2_PACKAGE_HOST_HELLO\n\tbool \"host-hello\"\n",
        ),
        ("configs/diskgc_defconfig", "BR2_PACKAGE_HOST_HELLO=y\n"),
        (
            "package/hello/hello.mk",
            "HOST_HELLO_VERSION = 1.0
HOST_HELLO_SITE = $(BR2_EXTERNAL_DISKGC_PATH)/src/hello
HOST_HELLO_SITE_METHOD = local
define HOST_HELLO_BUILD_CMDS
\t$(HOSTCC) $(HOST_CFLAGS) -o $(@D)/hello $(@D)/main.c
endef
define HOST_HELLO_INSTALL_CMDS
\t$(INSTALL) -D -m 0755 $(@D)/hello $(HOST_DIR)/bin/hello
endef
$(eval $(host-generic-package))
",
        ),
        (
            "package/cmk/cmk.mk",
            "CMK_VERSION = 1.0
CMK_SITE = $(BR2_EXTERNAL_DISKGC_PATH)/src/cmk
CMK_SITE_METHOD = local
CMK_SUPPORTS_IN_SOURCE_BUILD = NO
$(eval $(host-cmake-package))
",
        ),
        ("package/msn/msn.mk", &msn_mk),
    ];
    for (rel, text) in files {
        write(ctx, &format!("{SRC}/{BR_EXT}/{rel}"), text.as_bytes())?;
    }
    write(ctx, &format!("{SRC}/{BR_EXT}/src/hello/main.c"), MAIN_C)?;
    write(
        ctx,
        &format!("{SRC}/{BR_EXT}/src/cmk/CMakeLists.txt"),
        b"cmake_minimum_required(VERSION 3.20)\nproject(cmk C)\nadd_executable(cmk main.c)\ninstall(TARGETS cmk)\n",
    )?;
    write(ctx, &format!("{SRC}/{BR_EXT}/src/cmk/main.c"), MAIN_C)?;
    write(
        ctx,
        &format!("{SRC}/{BR_EXT}/src/msn/meson.build"),
        b"project('msn', 'c')\nexecutable('msn', 'main.c')\n",
    )?;
    write(ctx, &format!("{SRC}/{BR_EXT}/src/msn/main.c"), MAIN_C)?;
    ctx.home
        .mkdirs(&format!("{SRC}/buildroot/dl/pkgconf"), 0o755, crate::TESTER)?;
    tool(
        ctx,
        Path::new(HOME),
        &[],
        "/bin/cp",
        &[
            "-L",
            &format!("{bin}/buildroot-dl-pkgconf-2.3.0.tar.xz"),
            display(&src("buildroot/dl/pkgconf/pkgconf-2.3.0.tar.xz"))?,
        ],
    )?;
    Ok(())
}

fn br_env(ctx: &Ctx) -> Result<Vec<(&'static str, String)>, String> {
    Ok(vec![("PATH", format!("{}/buildroot-tools", bin(ctx)?))])
}

/// `make ARGS` in the Buildroot top directory with Buildroot's host tools.
fn br_make(ctx: &Ctx, top: &Path, args: &[&str]) -> Result<String, String> {
    let env = br_env(ctx)?;
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    tool(
        ctx,
        top,
        &env,
        ctx.bin_dir.join("buildroot-tools/make"),
        args,
    )
}

/// Buildroot's source copied from the pinned tree to `src/buildroot` (a
/// user's clone: writable, and a Git checkout), configured with `make
/// defconfig` (default `O=output`: `.config` in the top directory) and
/// `packages` built. Returns the output.
fn buildroot_fixture(ctx: &Ctx, packages: &[&str]) -> Result<PathBuf, String> {
    usr_bin(ctx, "buildroot-tools", &["env", "file"])?;
    let top = src("buildroot");
    let env = br_env(ctx)?;
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let tools = ctx.bin_dir.join("buildroot-tools");
    ctx.home.mkdirs(SRC, 0o755, crate::TESTER)?;
    tool(
        ctx,
        Path::new(HOME),
        &env,
        tools.join("cp"),
        &[
            "-r",
            &format!("{}/buildroot-src/.", bin(ctx)?),
            display(&top)?,
        ],
    )?;
    tool(
        ctx,
        Path::new(HOME),
        &env,
        tools.join("chmod"),
        &["-R", "u+w", display(&top)?],
    )?;
    git_init(ctx, &top)?;
    br_make(ctx, &top, &["defconfig"])?;
    if !packages.is_empty() {
        br_make(ctx, &top, packages)?;
    }
    let output = top.join("output");
    ensure!(
        output.join(".br2-external.mk").is_file()
            && output.join("build").is_dir()
            && top.join(".config").is_file(),
        "Buildroot made no output/.br2-external.mk, output/build/ and top-level .config"
    );
    Ok(output)
}

/// A compiler that a test can hold ([`HOLD_SH`] as `buildroot`) when it
/// compiles makedevs.c, in the package's build step (Make also runs the
/// compiler while it reads its makefiles), then the real one.
const HELD_CC: &[u8] = b"#!/bin/sh\ncase \"$*\" in *makedevs.c*) sh /home/tester/ctl/hold.sh buildroot;; esac\nexec gcc \"$@\"\n";

/// docs/testing.md, Admission evidence (`buildroot-output-anchored`). The
/// real Buildroot 2026.08.1, copied to a Git checkout below `src`: `make
/// defconfig` and `make host-makedevs` (a package whose source is in the
/// Buildroot tree) build `output/`. Refused by the anchor (not planned): an
/// `O=` output (its `.config` is inside it) and the output while the
/// top-level `.config` is away. `make host-makedevs-rebuild` in flight,
/// its compile held, runs from the top directory (no working directory in
/// the output); its processes' exported `BASE_DIR` keeps the output
/// `in_use` (`environment`), and it completes. Past the floor the output is
/// reclaimed and `make host-makedevs` builds it again. Then with a
/// br2-external tree (`make BR2_EXTERNAL=... diskgc_defconfig`, its
/// packages built) the output is never planned: `.br2-external.mk` is
/// Buildroot's only record of that tree, and without it the next `make`
/// rewrites the top `.config` without the tree's options, which the test
/// shows on a copy. Then [`nested_builds_in_buildroot`] (TASK-367).
pub fn buildroot_output_anchored_real_buildroot_reclaim_then_rebuild_and_a_build_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    control(ctx)?;
    let output = buildroot_fixture(ctx, &["host-makedevs"])?;
    let top = src("buildroot");
    let version = br_make(ctx, &top, &["print-version"])?;
    info!("tool version: Buildroot {}", version.trim());
    ensure!(
        version.trim() == "2026.08.1",
        "Buildroot reports {:?}, not the verified 2026.08.1",
        version.trim()
    );
    let makedevs = output.join("host/bin/makedevs");
    ensure!(makedevs.is_file(), "Buildroot built no host makedevs");
    let o_output = src("elsewhere/output");
    br_make(
        ctx,
        &top,
        &[&format!("O={}", display(&o_output)?), "defconfig"],
    )?;
    ensure!(
        o_output.join(".config").is_file() && o_output.join(".br2-external.mk").is_file(),
        "the O= output holds no .config and .br2-external.mk"
    );

    let profile = "buildroot-output-anchored";
    let expected: Expected = vec![(output.clone(), None)];
    expect_floor(ctx, profile, SRC_ROOT, &[&output], &expected)?;
    let binary = ctx.diskgc();
    mv(ctx, &top.join(".config"), &src("config.away"))?;
    let plan = dry_plan(ctx, &binary, &select(profile), SRC_ROOT, true)?;
    mv(ctx, &src("config.away"), &top.join(".config"))?;
    ensure!(
        plan.is_empty(),
        "{profile} planned {plan:?} while the top .config was away"
    );

    hold(ctx, "buildroot")?;
    write_script(ctx, &format!("{CTL}/held-cc"), HELD_CC)?;
    let env = br_env(ctx)?;
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let make = ctx.bin_dir.join("buildroot-tools/make");
    let mut build = InFlight::start(
        ctx,
        "buildroot-live",
        &top,
        &env,
        display(&make)?,
        &[
            "host-makedevs-rebuild",
            &format!("HOSTCC={HOME}/{CTL}/held-cc"),
        ],
    )?;
    wait_held(&mut build, "buildroot")?;
    let cwds = cwds_below(&build, &output)?;
    let named = environ_below(&build, &output)?;
    info!(
        "the held Buildroot build's working directories {cwds:?} and environment {named:?} in the output"
    );
    ensure!(
        named.iter().any(|e| e.starts_with("BASE_DIR=")),
        "precondition: no process of the held build names BASE_DIR: {named:?}"
    );
    let live: Expected = vec![(output.clone(), Some("in_use"))];
    let reclaimed = reclaim_past_floor(ctx, &binary, profile, SRC_ROOT, &live)?;
    expect_used_by(&reclaimed, &output, &build, &["environment"], &|u| {
        u.starts_with("BASE_DIR=")
    })?;
    open_gate(ctx, "buildroot")?;
    build.expect_success()?;

    reclaim_past_floor(ctx, &binary, profile, SRC_ROOT, &expected)?;
    ensure!(
        o_output.join(".config").is_file(),
        "the O= output was touched"
    );
    br_make(ctx, &top, &["host-makedevs"])?;
    ensure!(makedevs.is_file(), "Buildroot did not build makedevs again");

    // With a br2-external tree the output is never a candidate.
    br_external(ctx)?;
    let external = format!("BR2_EXTERNAL={}", display(&src(BR_EXT))?);
    br_make(ctx, &top, &[&external, "diskgc_defconfig"])?;
    br_make(ctx, &top, &["host-hello", "host-cmk", "host-msn"])?;
    runs(ctx, &output.join("host/bin/hello"))?;
    let plan = dry_plan(ctx, &binary, &select(profile), SRC_ROOT, true)?;
    ensure!(
        plan.is_empty(),
        "{profile} planned {plan:?}: an output naming a br2-external tree"
    );
    // Why: on a copy of the tree without its output, Buildroot forgets the
    // external tree and drops its options from the top .config.
    let selected = std::fs::read_to_string(top.join(".config"))
        .map_err(|e| format!("read the top .config: {e}"))?;
    ensure!(
        selected.contains("BR2_PACKAGE_HOST_HELLO=y"),
        "precondition: diskgc_defconfig did not select the external package"
    );
    let copy = src("buildroot-copy");
    cp(ctx, &top, &copy)?;
    mv(ctx, &copy.join("output"), &src("copy-output.away"))?;
    br_make(ctx, &copy, &["syncconfig"])?;
    let config = std::fs::read_to_string(copy.join(".config"))
        .map_err(|e| format!("read the copy's .config: {e}"))?;
    ensure!(
        !config.contains("BR2_PACKAGE_HOST_HELLO=y"),
        "Buildroot kept the external option without its output: update the anchor's reason"
    );
    nested_builds_in_buildroot(ctx, &binary, &top, &output)
}

/// TASK-367, the nested-build guard, on Buildroot's real package
/// directories: `host-cmk` (cmake-package) and `host-msn` (Meson), each
/// with Buildroot's stamps beside its `buildroot-build`, inside the output.
/// Neither build is planned by its profile with both signals, with the
/// stamps alone (`.br2-external.mk` away) or with the output's marker
/// alone (the stamps away); with neither, both are (the guard is what
/// keeps them). Why: with `buildroot-build` gone and its stamps left,
/// `make host-cmk-rebuild` fails.
fn nested_builds_in_buildroot(ctx: &Ctx, binary: &Path, top: &Path, output: &Path) -> TestResult {
    let packages = [
        ("cmake-build-anchored", output.join("build/host-cmk-1.0")),
        ("meson-build-anchored", output.join("build/host-msn-1.0")),
    ];
    let mut stamps = Vec::new();
    for (_, pkg) in &packages {
        let mut names: Vec<String> = std::fs::read_dir(pkg)
            .map_err(|e| format!("list {}: {e}", pkg.display()))?
            .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_, _>>()
            .map_err(|e| format!("list {}: {e}", pkg.display()))?;
        names.retain(|name| name.starts_with('.'));
        names.sort();
        info!("Buildroot's dot files in {}: {names:?}", pkg.display());
        for wanted in [".stamp_configured", ".stamp_built"] {
            ensure!(
                names.iter().any(|name| name == wanted),
                "Buildroot wrote no {wanted} in {}: {names:?}",
                pkg.display()
            );
        }
        stamps.extend(
            names
                .into_iter()
                .filter(|name| name.starts_with(".stamp_"))
                .map(|name| pkg.join(name)),
        );
    }
    ensure!(
        packages[0]
            .1
            .join("buildroot-build/CMakeCache.txt")
            .is_file()
            && packages[1]
                .1
                .join("buildroot-build/meson-info/meson-info.json")
                .is_file(),
        "precondition: CMake and Meson configured no buildroot-build"
    );
    let planned = |label: &str| -> Result<Vec<&'static str>, String> {
        let mut planned = Vec::new();
        for (profile, pkg) in &packages {
            let plan = dry_plan(ctx, binary, &select(profile), SRC_ROOT, true)?;
            info!("{profile} in Buildroot's output, {label}: {plan:?}");
            if plan
                .iter()
                .any(|(c, _)| c == &pkg.join("buildroot-build").display().to_string())
            {
                planned.push(*profile);
            }
        }
        Ok(planned)
    };
    let marker = output.join(".br2-external.mk");
    let marker_away = src("br2-external.mk.away");
    let stamps_away = src("stamps.away");
    ctx.home
        .mkdirs(&format!("{SRC}/stamps.away"), 0o755, crate::TESTER)?;
    let away = |stamp: &Path| -> Result<PathBuf, String> {
        let pkg = stamp
            .parent()
            .and_then(Path::file_name)
            .ok_or_else(|| format!("{} has no package directory", stamp.display()))?;
        let name = stamp
            .file_name()
            .ok_or_else(|| format!("{} has no name", stamp.display()))?;
        Ok(stamps_away.join(format!(
            "{}{}",
            pkg.to_string_lossy(),
            name.to_string_lossy()
        )))
    };

    let both = planned("both signals")?;
    ensure!(both.is_empty(), "planned with both signals: {both:?}");
    mv(ctx, &marker, &marker_away)?;
    let stamps_only = planned("stamps only")?;
    for stamp in &stamps {
        mv(ctx, stamp, &away(stamp)?)?;
    }
    let neither = planned("neither signal")?;
    mv(ctx, &marker_away, &marker)?;
    let root_only = planned("the output's marker only")?;
    for stamp in &stamps {
        mv(ctx, &away(stamp)?, stamp)?;
    }
    ensure!(
        stamps_only.is_empty(),
        "planned with Buildroot's stamps alone: {stamps_only:?}"
    );
    ensure!(
        root_only.is_empty(),
        "planned with the output's .br2-external.mk alone: {root_only:?}"
    );
    ensure!(
        neither.len() == packages.len(),
        "positive control: without either signal only {neither:?} planned"
    );

    // Buildroot's next rebuild of a package whose build directory is gone
    // fails while its stamps say it is configured.
    let cmk = &packages[0].1;
    let build_away = src("cmk-build.away");
    mv(ctx, &cmk.join("buildroot-build"), &build_away)?;
    let env = br_env(ctx)?;
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let failed = tool_fails(
        ctx,
        top,
        &env,
        ctx.bin_dir.join("buildroot-tools/make"),
        &["host-cmk-rebuild"],
    )?;
    info!("make host-cmk-rebuild without buildroot-build: {failed}");
    mv(ctx, &build_away, &cmk.join("buildroot-build"))?;
    br_make(ctx, top, &["host-cmk-rebuild"])?;
    runs(ctx, &output.join("host/bin/cmk"))?;
    Ok(())
}

// ---- yocto-tmpdir-anchored -------------------------------------------------

const YOCTO: &str = "src/yocto";
const LAYER: &str = "src/yocto/meta-diskgc";

/// A layer with one recipe built from a local source, run to its own task
/// `do_diskgc_build` after `do_unpack` (every default task after it needs
/// native tools fetched from the network), which [`HOLD_SH`] can hold.
fn yocto_layer(ctx: &Ctx) -> TestResult {
    write(
        ctx,
        &format!("{LAYER}/conf/layer.conf"),
        b"BBPATH .= \":${LAYERDIR}\"
BBFILES += \"${LAYERDIR}/recipes-*/*/*.bb\"
BBFILE_COLLECTIONS += \"diskgc\"
BBFILE_PATTERN_diskgc = \"^${LAYERDIR}/\"
BBFILE_PRIORITY_diskgc = \"6\"
LAYERSERIES_COMPAT_diskgc = \"blacksail\"
",
    )?;
    write(
        ctx,
        &format!("{LAYER}/recipes-diskgc/mini/mini_1.0.bb"),
        b"SUMMARY = \"diskgc admission fixture\"
LICENSE = \"MIT\"
LIC_FILES_CHKSUM = \"file://${COMMON_LICENSE_DIR}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302\"
SRC_URI = \"file://main.c\"
INHIBIT_DEFAULT_DEPS = \"1\"
S = \"${UNPACKDIR}\"
do_diskgc_build() {
    sh /home/tester/ctl/hold.sh yocto
    ${BUILD_CC} ${S}/main.c -o ${B}/mini
    ${B}/mini
}
do_diskgc_build[dirs] = \"${B}\"
do_diskgc_build[nostamp] = \"1\"
addtask diskgc_build after do_unpack
",
    )?;
    write(
        ctx,
        &format!("{LAYER}/recipes-diskgc/mini/files/main.c"),
        MAIN_C,
    )
}

/// The environment BitBake runs in: OE-Core's host tools (no busybox), the
/// en_US.UTF-8 locale its sanity check requires.
fn yocto_env(ctx: &Ctx) -> Result<Vec<(&'static str, String)>, String> {
    let bin = bin(ctx)?;
    Ok(vec![
        ("PATH", format!("{bin}/yocto-tools")),
        ("LANG", "en_US.UTF-8".to_owned()),
        ("LC_ALL", "en_US.UTF-8".to_owned()),
        ("LOCALE_ARCHIVE", format!("{bin}/yocto-locale-archive")),
        // BitBake starts its server with only the variables it passes
        // through; Nix's glibc finds the locale only through this one.
        ("BB_ENV_PASSTHROUGH_ADDITIONS", "LOCALE_ARCHIVE".to_owned()),
        ("BITBAKEDIR", format!("{HOME}/{YOCTO}/bitbake")),
    ])
}

/// `source oe-init-build-env build && SCRIPT` in bash, as tester.
fn bitbake(ctx: &Ctx, script: &str) -> Result<String, String> {
    let env = yocto_env(ctx)?;
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let line = format!(
        ". {}/oe-core-src/oe-init-build-env {HOME}/{YOCTO}/build > /dev/null && {script}",
        bin(ctx)?
    );
    let out = tool(
        ctx,
        &home(YOCTO),
        &env,
        ctx.bin_dir.join("yocto-tools/bash"),
        &["-c", &line],
    )?;
    bitbake_server_gone()?;
    Ok(out)
}

/// The working directories below `dir` of any visible process.
fn any_cwd_below(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").map_err(|e| format!("read /proc: {e}"))? {
        let entry = entry.map_err(|e| format!("read /proc: {e}"))?;
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.parse::<u32>().is_ok())
        {
            continue;
        }
        // A process may end between the listing and the read.
        if let Ok(cwd) = std::fs::read_link(entry.path().join("cwd"))
            && cwd.starts_with(dir)
        {
            found.push(cwd);
        }
    }
    Ok(found)
}

/// The FIFOs below `dir` (not following symlinks).
fn fifos_below(dir: &Path) -> Result<Vec<PathBuf>, String> {
    use std::os::unix::fs::FileTypeExt;
    let mut found = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(next) = pending.pop() {
        for entry in
            std::fs::read_dir(&next).map_err(|e| format!("read {}: {e}", next.display()))?
        {
            let entry = entry.map_err(|e| format!("read {}: {e}", next.display()))?;
            let kind = entry
                .file_type()
                .map_err(|e| format!("stat {}: {e}", entry.path().display()))?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_fifo() {
                found.push(entry.path());
            }
        }
    }
    Ok(found)
}

/// BitBake's server outlives its client by a moment, then removes its
/// lock and sockets in the build directory: a run of diskgc judged by the
/// oracle starts only after that.
fn bitbake_server_gone() -> TestResult {
    let build = home(&format!("{YOCTO}/build"));
    wait_for(TOOL_TIMEOUT, "BitBake's server to exit", || {
        Ok(["bitbake.lock", "bitbake.sock", "hashserve.sock"]
            .iter()
            .all(|f| !exists(&build.join(f)))
            .then_some(()))
    })
}

/// What every distribution has at these paths and the image does not:
/// Buildroot's and BitBake's scripts start with `#!/usr/bin/env`, and
/// Buildroot's dependency check requires exactly `/usr/bin/file` (libtool
/// runs it there). The disposable container gets each of `names` in
/// `/usr/bin`, linked to `tools` (a bundle link to a tool set).
#[expect(
    clippy::disallowed_methods,
    reason = "the disposable container's /usr/bin, outside every scratch mount, before any oracle is armed"
)]
fn usr_bin(ctx: &Ctx, tools: &str, names: &[&str]) -> TestResult {
    std::fs::create_dir_all("/usr/bin").map_err(|e| format!("make /usr/bin: {e}"))?;
    for name in names {
        let link = Path::new("/usr/bin").join(name);
        if std::fs::symlink_metadata(&link).is_ok() {
            continue;
        }
        std::os::unix::fs::symlink(ctx.bin_dir.join(tools).join(name), &link)
            .map_err(|e| format!("link {}: {e}", link.display()))?;
    }
    Ok(())
}

/// OE-Core's sanity check refuses a `/bin/sh` that links to anything but
/// bash or dash; the image's links to busybox. The disposable container's
/// `/bin/sh` becomes bash.
#[expect(
    clippy::disallowed_methods,
    reason = "the disposable container's /bin/sh, outside every scratch mount, before any oracle is armed"
)]
fn bash_as_bin_sh(ctx: &Ctx) -> TestResult {
    let sh = Path::new("/bin/sh");
    let target = std::fs::read_link(sh).map_err(|e| format!("read the link /bin/sh: {e}"))?;
    info!(
        "/bin/sh links to {}; replaced by bash for OE-Core",
        target.display()
    );
    std::fs::rename(sh, "/bin/sh.busybox")
        .map_err(|e| format!("move the container's /bin/sh aside: {e}"))?;
    std::os::unix::fs::symlink(ctx.bin_dir.join("yocto-tools/bash"), sh)
        .map_err(|e| format!("link /bin/sh to bash: {e}"))
}

/// BitBake copied to `src/yocto/bitbake` (BitBake 2.20 run from a read-only
/// tree writes its parser tables to the working directory and then fails to
/// import them, so it runs from a clone, as users do), OE-Core from the
/// pinned tree; `oe-init-build-env` makes `build/conf`, the fixture layer is
/// added and the recipe's task run. Returns TMPDIR (`build/tmp`).
fn yocto_fixture(ctx: &Ctx) -> Result<PathBuf, String> {
    bash_as_bin_sh(ctx)?;
    usr_bin(ctx, "yocto-tools", &["env"])?;
    let env = yocto_env(ctx)?;
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let tools = ctx.bin_dir.join("yocto-tools");
    ctx.home.mkdirs(YOCTO, 0o755, crate::TESTER)?;
    let bitbake_dir = home(&format!("{YOCTO}/bitbake"));
    tool(
        ctx,
        Path::new(HOME),
        &env,
        tools.join("cp"),
        &[
            "-r",
            &format!("{}/bitbake-src/.", bin(ctx)?),
            display(&bitbake_dir)?,
        ],
    )?;
    tool(
        ctx,
        Path::new(HOME),
        &env,
        tools.join("chmod"),
        &["-R", "u+w", display(&bitbake_dir)?],
    )?;
    yocto_layer(ctx)?;
    bitbake(
        ctx,
        &format!(
            "bitbake-layers add-layer {HOME}/{LAYER} && chmod u+w conf/local.conf && \
             echo 'BB_NO_NETWORK = \"1\"' >> conf/local.conf && \
             bitbake -c diskgc_build mini"
        ),
    )?;
    let tmpdir = home(&format!("{YOCTO}/build/tmp"));
    ensure!(
        ["saved_tmpdir", "abi_version"]
            .iter()
            .all(|f| tmpdir.join(f).is_file())
            && tmpdir.join("stamps").is_dir(),
        "BitBake made no saved_tmpdir, abi_version and stamps/ in {}",
        tmpdir.display()
    );
    Ok(tmpdir)
}

/// docs/testing.md, Admission evidence (`yocto-tmpdir-anchored`). The real
/// BitBake 2.20.0 with OE-Core yocto-6.1 (`oe-init-build-env`,
/// `bitbake-layers add-layer`, a recipe's task after `do_unpack`) makes
/// `build/tmp` with its sanity check's `saved_tmpdir` and `abi_version`
/// and `stamps/`. Refused by the anchor (not planned): a copy of TMPDIR
/// (its `saved_tmpdir` names the original), TMPDIR while its
/// `abi_version`, the fixture layer or `conf/local.conf` is away. A task
/// in flight held in `do_diskgc_build` keeps TMPDIR `in_use` (the task's
/// working directory and BitBake's open logs) and completes. Past the
/// floor TMPDIR is reclaimed and BitBake runs the task again from the
/// surviving build directory and layers. Then a recipe fetched by BitBake's
/// own Git fetcher (a local repository) unpacks a clone into `tmp/work`:
/// the checkout guard keeps TMPDIR (`contains_checkout`, the row's
/// residual 3).
pub fn yocto_tmpdir_anchored_real_bitbake_reclaim_then_rebuild_and_a_task_in_flight_is_kept(
    ctx: &Ctx,
) -> TestResult {
    control(ctx)?;
    let tmpdir = yocto_fixture(ctx)?;
    let version = bitbake(ctx, "bitbake --version")?;
    info!("tool version: {}", version.trim());
    ensure!(
        version.contains("2.20.0"),
        "BitBake reports {:?}, not the verified 2.20.0",
        version.trim()
    );
    let profile = "yocto-tmpdir-anchored";
    let root = &format!("{HOME}/{YOCTO}");
    let copy = home(&format!("{YOCTO}/build/tmp-copy"));
    cp(ctx, &tmpdir, &copy)?;
    let expected: Expected = vec![(tmpdir.clone(), None)];
    expect_floor(ctx, profile, root, &[&tmpdir], &expected)?;
    let binary = ctx.diskgc();
    for (away, back) in [
        (
            tmpdir.join("abi_version"),
            home(&format!("{YOCTO}/abi_version.away")),
        ),
        (home(LAYER), home(&format!("{YOCTO}/meta-diskgc.away"))),
        (
            home(&format!("{YOCTO}/build/conf/local.conf")),
            home(&format!("{YOCTO}/local.conf.away")),
        ),
    ] {
        mv(ctx, &away, &back)?;
        let plan = dry_plan(ctx, &binary, &select(profile), root, true)?;
        mv(ctx, &back, &away)?;
        ensure!(
            plan.is_empty(),
            "{profile} planned {plan:?} while {} was away",
            away.display()
        );
    }

    hold(ctx, "yocto")?;
    let env = yocto_env(ctx)?;
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let line = format!(
        ". {}/oe-core-src/oe-init-build-env {HOME}/{YOCTO}/build > /dev/null && \
         bitbake -c diskgc_build mini",
        bin(ctx)?
    );
    let bash = ctx.bin_dir.join("yocto-tools/bash");
    let mut task = InFlight::start(
        ctx,
        "bitbake-live",
        &home(YOCTO),
        &env,
        display(&bash)?,
        &["-c", &line],
    )?;
    wait_held(&mut task, "yocto")?;
    // A task in flight has its working directory in tmp/work, and BitBake
    // keeps a FIFO for the task's messages in its temp directory: the walk
    // keeps TMPDIR for the FIFO (`contains_ipc`) before the in-use scan.
    let work = tmpdir.join("work");
    // BitBake's server, a daemon, runs the task: not in the client's tree.
    let cwds = any_cwd_below(&work)?;
    let fifos = fifos_below(&tmpdir)?;
    info!("the held task's working directories {cwds:?}, FIFOs in TMPDIR {fifos:?}");
    ensure!(
        !cwds.is_empty() && !fifos.is_empty(),
        "precondition: the held task has no working directory in tmp/work ({cwds:?}) or \
         TMPDIR holds no FIFO ({fifos:?})"
    );
    let live: Expected = vec![(tmpdir.clone(), Some("contains_ipc"))];
    reclaim_past_floor(ctx, &binary, profile, root, &live)?;
    open_gate(ctx, "yocto")?;
    task.expect_success()?;
    bitbake_server_gone()?;

    reclaim_past_floor(ctx, &binary, profile, root, &expected)?;
    ensure!(
        copy.join("saved_tmpdir").is_file(),
        "the TMPDIR copy was touched"
    );
    bitbake(ctx, "bitbake -c diskgc_build mini")?;
    ensure!(
        tmpdir.join("saved_tmpdir").is_file() && tmpdir.join("stamps").is_dir(),
        "BitBake did not make TMPDIR again"
    );

    // Residual 3: the Git fetcher's clone in tmp/work.
    let repo = home(&format!("{YOCTO}/gitsrc"));
    write(ctx, &format!("{YOCTO}/gitsrc/main.c"), MAIN_C)?;
    let git_env: Vec<(&str, &str)> = env
        .iter()
        .copied()
        .chain([
            ("GIT_AUTHOR_NAME", "tester"),
            ("GIT_AUTHOR_EMAIL", "tester@example.invalid"),
            ("GIT_COMMITTER_NAME", "tester"),
            ("GIT_COMMITTER_EMAIL", "tester@example.invalid"),
        ])
        .collect();
    let git = ctx.bin_dir.join("yocto-tools/git");
    tool(
        ctx,
        &repo,
        &git_env,
        &git,
        &["init", "-q", "-b", "main", "."],
    )?;
    tool(ctx, &repo, &git_env, &git, &["add", "main.c"])?;
    tool(
        ctx,
        &repo,
        &git_env,
        &git,
        &["commit", "-q", "-m", "fixture"],
    )?;
    let rev = tool(ctx, &repo, &git_env, &git, &["rev-parse", "HEAD"])?;
    write(
        ctx,
        &format!("{LAYER}/recipes-diskgc/gitmini/gitmini_1.0.bb"),
        format!(
            "SUMMARY = \"diskgc admission fixture, fetched with Git\"
LICENSE = \"MIT\"
LIC_FILES_CHKSUM = \"file://${{COMMON_LICENSE_DIR}}/MIT;md5=0835ade698e0bcf8506ecda2f7b4f302\"
SRC_URI = \"git://{}/{YOCTO}/gitsrc;protocol=file;branch=main\"
SRCREV = \"{}\"
INHIBIT_DEFAULT_DEPS = \"1\"
",
            HOME,
            rev.trim()
        )
        .as_bytes(),
    )?;
    bitbake(ctx, "bitbake -c unpack gitmini")?;
    let plan = dry_plan(ctx, &binary, &select(profile), root, true)?;
    ensure!(
        plan == sorted(&vec![(tmpdir.clone(), Some("contains_checkout"))]),
        "{profile} with the Git fetcher's clone in tmp/work planned {plan:?}"
    );
    Ok(())
}

// ---- bazel-output-base-anchored --------------------------------------------

const BAZEL_ROOT: &str = "/home/tester/.cache/bazel";
const BAZEL_RC: &[u8] = b"startup --host_jvm_args=-Xmx512m
common --noenable_bzlmod --enable_workspace
";
const BAZEL_BUILD: &[u8] =
    b"genrule(name = \"hello\", outs = [\"hello.txt\"], cmd = \"echo built > $@\")
genrule(
    name = \"held\",
    outs = [\"held.txt\"],
    cmd = \"sh /home/tester/ctl/hold.sh bazel && echo built > $@\",
    tags = [\"no-sandbox\"],
)
";

/// A Bazel workspace (WORKSPACE mode) at `rel` below `scratch` (tester's
/// home or the scratch mount).
fn bazel_workspace(scratch: &Scratch, rel: &str) -> Result<PathBuf, String> {
    let dir = scratch.mkdirs(rel, 0o755, crate::TESTER)?;
    for (name, text) in [
        ("WORKSPACE", &b""[..]),
        ("BUILD", BAZEL_BUILD),
        (".bazelrc", BAZEL_RC),
    ] {
        scratch.write_file(&format!("{rel}/{name}"), text, 0o644, crate::TESTER)?;
    }
    Ok(dir)
}

/// `bazel ARGS` in `workspace` as tester.
fn bazel(ctx: &Ctx, workspace: &Path, args: &[&str]) -> Result<String, String> {
    tool(
        ctx,
        workspace,
        &[("PATH", &tools_path(ctx)?), ("USER", "tester")],
        ctx.bin_dir.join("bazel"),
        args,
    )
}

/// Builds `//:hello` in `workspace` and returns its output base; the server
/// is shut down unless `keep_server`.
fn bazel_build(ctx: &Ctx, workspace: &Path, keep_server: bool) -> Result<PathBuf, String> {
    bazel(ctx, workspace, &["build", "//:hello"])?;
    let base = PathBuf::from(bazel(ctx, workspace, &["info", "output_base"])?.trim());
    ensure!(
        workspace.join("bazel-bin/hello.txt").exists()
            && base.join("DO_NOT_BUILD_HERE").is_file()
            && base.join("execroot").is_dir(),
        "Bazel made no DO_NOT_BUILD_HERE and execroot/ in {}",
        base.display()
    );
    if !keep_server {
        bazel_shutdown(ctx, workspace, &base)?;
    }
    Ok(base)
}

/// The pid of the Bazel server of output base `base`.
fn server_pid(base: &Path) -> Result<u32, String> {
    let text = std::fs::read_to_string(base.join("server/server.pid.txt"))
        .map_err(|e| format!("read the server pid of {}: {e}", base.display()))?;
    text.trim()
        .parse()
        .map_err(|e| format!("server pid {text:?}: {e}"))
}

/// `bazel shutdown` in `workspace`. The server is a daemon, an orphan the
/// harness inherits as the container's init: it is reaped here, or the
/// client waits on its zombie and then fails (exit 36).
fn bazel_shutdown(ctx: &Ctx, workspace: &Path, base: &Path) -> TestResult {
    let pid = server_pid(base)?;
    let mut client = InFlight::start(
        ctx,
        &format!("bazel-shutdown-{pid}"),
        workspace,
        &[("PATH", &tools_path(ctx)?), ("USER", "tester")],
        display(&ctx.bin_dir.join("bazel"))?,
        &["shutdown"],
    )?;
    let server = rustix::process::Pid::from_raw(
        i32::try_from(pid).map_err(|e| format!("server pid {pid}: {e}"))?,
    )
    .ok_or_else(|| format!("server pid {pid}"))?;
    wait_for(
        TOOL_TIMEOUT,
        &format!("the Bazel server {pid} to exit"),
        || {
            match rustix::process::waitpid(Some(server), rustix::process::WaitOptions::NOHANG) {
                Ok(Some(_)) => Ok(Some(())),
                Ok(None) => Ok(None),
                // Not the harness's child: gone once /proc no longer lists it.
                Err(rustix::io::Errno::CHILD) => {
                    Ok((!exists(Path::new(&format!("/proc/{pid}")))).then_some(()))
                }
                Err(e) => Err(format!("wait for the Bazel server {pid}: {e}")),
            }
        },
    )?;
    client.expect_success()?;
    Ok(())
}

/// docs/testing.md, Admission evidence (`bazel-output-base-anchored`). The
/// real Bazel 7.6.0 builds a genrule in five workspaces below `src/bz`.
/// Reclaimed past the floor: the output base of a plain workspace. Kept:
/// the output base of a Git workspace (`contains_checkout`: Bazel's
/// execroot links the workspace's top-level entries, `.git` included; the
/// guard counts a symlink named `.git` as a checkout, TASK-331's question),
/// one whose server is still running idle (`in_use`, its open files) and
/// one with a build in flight held in a genrule. Refused by the anchor (not
/// planned): the output base of a workspace that moved away and of one on
/// another filesystem (`/tmp`). The install base and repository cache are
/// never planned. Bazel then builds the reclaimed workspace again.
pub fn bazel_output_base_anchored_real_bazel_reclaim_then_rebuild_and_a_live_server_is_kept(
    ctx: &Ctx,
) -> TestResult {
    let version = bazel(ctx, Path::new(HOME), &["--version"])?;
    info!("tool version: {}", version.trim());
    ensure!(
        version.contains("7.6.0"),
        "Bazel reports {:?}, not the verified 7.6.0",
        version.trim()
    );
    control(ctx)?;
    let ws = bazel_workspace(&ctx.home, "src/bz/ws")?;
    let ws_git = bazel_workspace(&ctx.home, "src/bz/ws-git")?;
    let ws_moved = bazel_workspace(&ctx.home, "src/bz/ws-moved")?;
    let ws_idle = bazel_workspace(&ctx.home, "src/bz/ws-idle")?;
    let ws_busy = bazel_workspace(&ctx.home, "src/bz/ws-busy")?;
    let ws_other = bazel_workspace(&ctx.scratch, "bz-otherfs")?;
    git_init(ctx, &ws_git)?;
    let base = bazel_build(ctx, &ws, false)?;
    let base_git = bazel_build(ctx, &ws_git, false)?;
    let git_link = base_git.join("execroot/__main__/.git");
    let link =
        std::fs::read_link(&git_link).map_err(|e| format!("read {}: {e}", git_link.display()))?;
    info!("{} -> {}", git_link.display(), link.display());
    let base_moved = bazel_build(ctx, &ws_moved, false)?;
    mv(ctx, &ws_moved, &src("bz/ws-moved-away"))?;
    let base_other = bazel_build(ctx, &ws_other, false)?;
    let base_idle = bazel_build(ctx, &ws_idle, true)?;
    let base_busy = bazel_build(ctx, &ws_busy, true)?;

    let expected: Expected = vec![
        (base.clone(), None),
        (base_git.clone(), Some("contains_checkout")),
        (base_idle.clone(), Some("in_use")),
        (base_busy.clone(), Some("in_use")),
        // The workspace on another filesystem: a visible refusal, reported
        // at the workspace's path.
        (ws_other.clone(), Some("other_fs")),
    ];
    let profile = "bazel-output-base-anchored";
    hold(ctx, "bazel")?;
    let mut busy = InFlight::start(
        ctx,
        "bazel-busy",
        &ws_busy,
        &[("PATH", &tools_path(ctx)?), ("USER", "tester")],
        display(&ctx.bin_dir.join("bazel"))?,
        &["build", "//:held"],
    )?;
    wait_held(&mut busy, "bazel")?;
    expect_floor(ctx, profile, BAZEL_ROOT, &[&base], &expected)?;
    // The idle servers write their logs while the scenario runs.
    let reclaimed = reclaim_past_floor(ctx, &ctx.diskgc(), profile, BAZEL_ROOT, &expected)?;
    for kept in [&base_idle, &base_busy] {
        let pid = server_pid(kept)?;
        let held = crate::admission::held_in(pid, kept)?;
        info!("the Bazel server {pid} holds {held:?}");
        ensure!(
            !held.is_empty(),
            "precondition: the server {pid} holds nothing in {}",
            kept.display()
        );
        let entry = crate::admission::skipped(&reclaimed, kept)?;
        ensure!(
            entry["process_blockers"]
                .as_array()
                .is_some_and(|blockers| blockers
                    .iter()
                    .any(|b| b["pid"].as_u64() == Some(u64::from(pid)))),
            "{} is not kept by its server {pid}: {entry}",
            kept.display()
        );
    }
    // The held action runs under its server (a daemon), in the execroot:
    // a build in flight, not only an idle server.
    let action_cwds = any_cwd_below(&base_busy.join("execroot"))?;
    info!("the held genrule's working directories {action_cwds:?}");
    ensure!(
        !action_cwds.is_empty(),
        "precondition: no process has its working directory in {}'s execroot",
        base_busy.display()
    );
    open_gate(ctx, "bazel")?;
    busy.expect_success()?;
    for kept in [&base_moved, &base_other] {
        ensure!(
            kept.join("DO_NOT_BUILD_HERE").is_file(),
            "{} was touched",
            kept.display()
        );
    }
    for excluded in ["install", "cache"] {
        ensure!(
            Path::new(BAZEL_ROOT)
                .join("_bazel_tester")
                .join(excluded)
                .is_dir(),
            "the {excluded} directory is gone"
        );
    }
    let again = bazel_build(ctx, &ws, false)?;
    ensure!(
        again == base,
        "Bazel chose another output base: {}",
        again.display()
    );
    bazel_shutdown(ctx, &ws_idle, &base_idle)?;
    bazel_shutdown(ctx, &ws_busy, &base_busy)?;
    Ok(())
}

// ---- the candidate catalog -------------------------------------------------

/// docs/testing.md, Admission evidence (candidate catalog): the release
/// build's catalog code path with this family in its catalog. One fixture
/// of every member made by the real tools (West's on the scratch mount,
/// root `/tmp`; Bazel's output user root; the rest below `src`), with a
/// running Bazel server (the live counterexample): per profile, past the
/// floor, the candidate build's dry run plans exactly what the development
/// build's does and its reclaim deletes exactly the reclaimable set; the
/// tools then build again.
pub fn candidate_catalog_build_output_family_matches_the_development_build(
    ctx: &Ctx,
) -> TestResult {
    let candidate = check_candidate_catalog(ctx, FAMILY)?;
    control(ctx)?;
    let app = cmake_project(ctx, "cm")?;
    cmake_build(ctx, &app, &app.join("out"), "Ninja")?;
    let mapp = meson_project(ctx, "ms")?;
    meson_build(ctx, &mapp, &mapp.join("bld"))?;
    let aapp = autotools_project(ctx, "at")?;
    autotools_build(ctx, &aapp.join("build"), "../configure")?;
    west_workspace(ctx)?;
    west_build_with(ctx, "west-app", "")?;
    let br_output = buildroot_fixture(ctx, &[])?;
    let tmpdir = yocto_fixture(ctx)?;
    let ws = bazel_workspace(&ctx.home, "src/bz/ws")?;
    let ws_idle = bazel_workspace(&ctx.home, "src/bz/ws-idle")?;
    let base = bazel_build(ctx, &ws, false)?;
    let base_idle = bazel_build(ctx, &ws_idle, true)?;
    let yocto_root = format!("{HOME}/{YOCTO}");

    let cases: Vec<(&str, &str, Expected)> = vec![
        (
            "cmake-build-anchored",
            SRC_ROOT,
            vec![(app.join("out"), None)],
        ),
        (
            "meson-build-anchored",
            SRC_ROOT,
            vec![(mapp.join("bld"), None)],
        ),
        (
            "autotools-build-anchored",
            SRC_ROOT,
            vec![(aapp.join("build"), None)],
        ),
        (
            "west-build-anchored",
            WEST_ROOT,
            vec![(ctx.scratch.path("west-app/build"), None)],
        ),
        (
            "buildroot-output-anchored",
            SRC_ROOT,
            vec![(br_output.clone(), None)],
        ),
        (
            "yocto-tmpdir-anchored",
            &yocto_root,
            vec![(tmpdir.clone(), None)],
        ),
        (
            "bazel-output-base-anchored",
            BAZEL_ROOT,
            vec![(base.clone(), None), (base_idle.clone(), Some("in_use"))],
        ),
    ];
    for (profile, root, expected) in &cases {
        let development = dry_plan(ctx, &ctx.diskgc(), &select(profile), root, true)?;
        ensure!(
            development == sorted(expected),
            "{profile}: the development build planned {development:?}, expected {:?}",
            sorted(expected)
        );
        let candidate_plan = dry_plan(ctx, &candidate, &select(profile), root, true)?;
        ensure!(
            candidate_plan == development,
            "{profile}: the candidate catalog planned {candidate_plan:?}, the development \
             build {development:?}"
        );
        reclaim_past_floor(ctx, &candidate, profile, root, expected)?;
    }
    bazel_shutdown(ctx, &ws_idle, &base_idle)?;

    cmake_build(ctx, &app, &app.join("out"), "Ninja")?;
    meson_build(ctx, &mapp, &mapp.join("bld"))?;
    autotools_build(ctx, &aapp.join("build"), "../configure")?;
    let (args, env) = west_invocation(ctx, "west-app");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    tool(
        ctx,
        &ctx.scratch.path("west-app"),
        &env,
        ctx.bin_dir.join("west"),
        &args,
    )?;
    br_make(ctx, &src("buildroot"), &["host-makedevs"])?;
    bitbake(ctx, "bitbake -c diskgc_build mini")?;
    bazel_build(ctx, &ws, false)?;
    for rebuilt in [
        app.join("out/app"),
        mapp.join("bld/app"),
        aapp.join("build/app"),
        ctx.scratch.path("west-app/build/zephyr/zephyr.elf"),
        br_output.join("host/bin/makedevs"),
        tmpdir.join("saved_tmpdir"),
        base.join("DO_NOT_BUILD_HERE"),
    ] {
        ensure!(
            exists(&rebuilt),
            "the tool did not build {} again",
            rebuilt.display()
        );
    }
    Ok(())
}
