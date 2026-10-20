//! Shipped command cache profiles against their real, pinned tool cleaners.
//! A disposable cache-shaped tree is built on the tester's home tmpfs. The
//! traced product run must invoke the cleaner on the candidate it measured,
//! reduce that directory's contents, and remove nothing outside it.

use std::path::Path;
use std::process::Command;

use crate::ensure_suite::{ensure_run, report};
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::under_test::TESTER_ENV;
use crate::{TESTER, ensure};

// ccache rewrites stats while clearing; the fixture must be large enough to
// prove net free space, not only deletion of a fabricated cache file.
const PAYLOAD_BYTES: usize = 1024 * 1024;

struct CacheCase {
    profile: &'static str,
    cache: &'static str,
    payload: Option<&'static str>,
}

fn cleaner_changes_its_measured_cache(ctx: &Ctx, case: CacheCase) -> TestResult {
    let payload_path = if let Some(relative) = case.payload {
        let payload = format!("{}/{}", case.cache, relative);
        let parent = payload
            .rsplit_once('/')
            .expect("cache payload has a parent")
            .0;
        ctx.home.mkdirs(parent, 0o700, TESTER)?;
        Some(
            ctx.home
                .write_file(&payload, &vec![0x71; PAYLOAD_BYTES], 0o600, TESTER)?,
        )
    } else {
        None
    };
    let cache = ctx.home.path(case.cache);
    let before = file_bytes(&cache)?;
    ensure!(
        before >= PAYLOAD_BYTES as u64,
        "{} fixture was empty",
        case.profile
    );

    let oracle = Oracle::arm(Fixture::new().eligible(cache.clone()))?;
    let select = format!("profile:{}", case.profile);
    let run = ensure_run(
        ctx,
        &[
            "--json",
            "--no-index",
            "--min-free",
            "100%",
            "--timeout",
            "30s",
            "--select",
            &select,
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
            actions.len() == 1
                && actions[0]["candidate"].as_str() == cache.to_str()
                && actions[0]["avail_delta_bytes"]
                    .as_i64()
                    .is_some_and(|delta| delta > 0)
        }),
        "{} did not free space on its measured cache: {report}",
        case.profile
    );
    let after = if cache.exists() {
        file_bytes(&cache)?
    } else {
        0
    };
    ensure!(
        after < before && payload_path.as_ref().is_none_or(|path| !path.exists()),
        "{} cleaner left its measured cache unchanged: {before} -> {after} bytes; {report}",
        case.profile
    );
    verdict.assert_clean()
}

fn file_bytes(root: &Path) -> Result<u64, String> {
    let mut total = 0;
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        for child in
            std::fs::read_dir(&path).map_err(|error| format!("read {}: {error}", path.display()))?
        {
            let child = child.map_err(|error| format!("read {}: {error}", path.display()))?;
            let meta = child
                .path()
                .symlink_metadata()
                .map_err(|error| format!("stat {:?}: {error}", child.path()))?;
            if meta.is_dir() {
                pending.push(child.path());
            } else {
                total += meta.len();
            }
        }
    }
    Ok(total)
}

pub fn pip_cache_cleaner(ctx: &Ctx) -> TestResult {
    cleaner_changes_its_measured_cache(
        ctx,
        CacheCase {
            profile: "pip-cache",
            cache: ".cache/pip",
            payload: Some("http-v2/0/0/cached.body"),
        },
    )
}

pub fn uv_cache_cleaner(ctx: &Ctx) -> TestResult {
    cleaner_changes_its_measured_cache(
        ctx,
        CacheCase {
            profile: "uv-cache",
            cache: ".cache/uv",
            payload: Some("archive-v0/example/package"),
        },
    )
}

pub fn go_build_cache_cleaner(ctx: &Ctx) -> TestResult {
    cleaner_changes_its_measured_cache(
        ctx,
        CacheCase {
            profile: "go-build-cache",
            cache: ".cache/go-build",
            payload: Some("00/0123456789abcdef-d"),
        },
    )
}

pub fn npm_cache_cleaner(ctx: &Ctx) -> TestResult {
    cleaner_changes_its_measured_cache(
        ctx,
        CacheCase {
            profile: "npm-cache",
            cache: ".npm",
            payload: Some("_cacache/content-v2/sha512/aa/bb/cache"),
        },
    )
}

pub fn pnpm_store_cleaner(ctx: &Ctx) -> TestResult {
    seed_pnpm_store_offline(ctx)?;
    cleaner_changes_its_measured_cache(
        ctx,
        CacheCase {
            profile: "pnpm-store",
            cache: ".local/share/pnpm/store",
            payload: None,
        },
    )
}

/// Store a real package tarball without network access. pnpm prunes indexed
/// packages; an arbitrary file in its store is not a package and proves
/// nothing about its cleaner.
fn seed_pnpm_store_offline(ctx: &Ctx) -> TestResult {
    ctx.home.mkdirs("pnpm-fixture/package", 0o700, TESTER)?;
    ctx.home.write_file(
        "pnpm-fixture/package/package.json",
        br#"{"name":"diskgc-offline-probe","version":"1.0.0"}"#,
        0o600,
        TESTER,
    )?;
    ctx.home.write_file(
        "pnpm-fixture/package/payload",
        &vec![0x71; PAYLOAD_BYTES],
        0o600,
        TESTER,
    )?;
    let tar = Command::new("/bin/tar")
        .args(["-czf", "-", "-C", "/home/tester/pnpm-fixture", "package"])
        .output()
        .map_err(|error| format!("create offline pnpm tarball: {error}"))?;
    ensure!(
        tar.status.success(),
        "create offline pnpm tarball: {}",
        String::from_utf8_lossy(&tar.stderr)
    );
    ctx.home
        .write_file("pnpm-fixture.tgz", &tar.stdout, 0o600, TESTER)?;
    let add = Command::new(&ctx.self_exe)
        .args([
            "as-tester",
            "--",
            "/bin/pnpm",
            "store",
            "add",
            "file:/home/tester/pnpm-fixture.tgz",
            "--store-dir",
            "/home/tester/.local/share/pnpm/store",
            "--offline",
            "--reporter",
            "append-only",
        ])
        .env_clear()
        .env("HOME", "/home/tester")
        .env("PATH", "/bin")
        .current_dir("/home/tester")
        .output()
        .map_err(|error| format!("seed real pnpm store: {error}"))?;
    ensure!(
        add.status.success(),
        "seed real pnpm store: {:?}\n--- stdout\n{}--- stderr\n{}",
        add.status,
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr)
    );
    Ok(())
}

pub fn yarn_cache_cleaner(ctx: &Ctx) -> TestResult {
    cleaner_changes_its_measured_cache(
        ctx,
        CacheCase {
            profile: "yarn-cache",
            cache: ".cache/yarn",
            payload: Some("v6/npm-example-1.0.0/package"),
        },
    )
}

pub fn ccache_cleaner(ctx: &Ctx) -> TestResult {
    cleaner_changes_its_measured_cache(
        ctx,
        CacheCase {
            profile: "ccache",
            cache: ".cache/ccache",
            payload: Some("0/0/0123456789abcdefR"),
        },
    )
}

/// The command must clear only PlatformIO's cache while leaving installed
/// packages alone. The cleaner runs from the checked `.platformio` parent.
pub fn platformio_cache_cleaner(ctx: &Ctx) -> TestResult {
    ctx.home
        .mkdirs(".platformio/packages/kept", 0o700, TESTER)?;
    ctx.home.mkdirs(".platformio/.cache", 0o700, TESTER)?;
    ctx.home.mkdirs(".platformio/cache", 0o700, TESTER)?;
    let package = ctx.home.write_file(
        ".platformio/packages/kept/package.json",
        b"installed package",
        0o600,
        TESTER,
    )?;
    let default_probe = ctx.home.write_file(
        ".platformio/.cache/default-probe",
        b"default cache",
        0o600,
        TESTER,
    )?;
    let nondefault = ctx.home.write_file(
        ".platformio/cache/keep",
        b"not the default cache",
        0o600,
        TESTER,
    )?;
    // First run the pinned tool without any cache override. This catches a
    // profile that hard-codes a cleaner path different from PIO's default.
    let default_run = Command::new(&ctx.self_exe)
        .args([
            "as-tester",
            "--",
            "/bin/pio",
            "system",
            "prune",
            "--cache",
            "--force",
        ])
        .env_clear()
        .envs(TESTER_ENV.iter().copied())
        .current_dir("/home/tester")
        .output()
        .map_err(|error| format!("run default PlatformIO prune: {error}"))?;
    ensure!(
        default_run.status.success(),
        "default PlatformIO prune failed: {:?}\nstdout: {}\nstderr: {}",
        default_run.status,
        String::from_utf8_lossy(&default_run.stdout),
        String::from_utf8_lossy(&default_run.stderr)
    );
    ensure!(
        !default_probe.exists() && nondefault.exists() && package.exists(),
        "pinned PlatformIO's default cache differs from .platformio/.cache"
    );
    cleaner_changes_its_measured_cache(
        ctx,
        CacheCase {
            profile: "platformio-cache",
            cache: ".platformio/.cache",
            payload: Some("downloaded-archive"),
        },
    )?;
    ensure!(package.exists(), "PlatformIO package was removed");
    ensure!(nondefault.exists(), "non-default cache was removed");
    Ok(())
}
