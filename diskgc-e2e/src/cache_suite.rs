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

/// The built-in cache matchers stay intact while a narrow YAML policy
/// changes only their checkout guard. The candidate trees live entirely in
/// this test's disposable container; every product run is a traced dry run.
pub fn named_checkout_policy_for_command_caches(ctx: &Ctx) -> TestResult {
    for (profile, cache, marker) in [
        ("npm-cache", ".npm", ".npm/_cacache/tmp/git-clone/.git"),
        ("uv-cache", ".cache/uv", ".cache/uv/sdists-v9/.git"),
    ] {
        let parent = marker.rsplit_once('/').expect("marker parent").0;
        ctx.home.mkdirs(parent, 0o700, TESTER)?;
        ctx.home
            .write_file(marker, b"checkout marker", 0o600, TESTER)?;
        let candidate = ctx.home.path(cache);
        for (policy, kept, expected_reason, expected_checkouts) in [
            (false, false, Some("contains_checkout"), "refuse"),
            (true, false, None, "allow"),
            (true, true, Some("keep_marker"), "allow"),
        ] {
            let config_file = format!("diskgc-{profile}-checkout-policy.yaml");
            let config_path = format!("/home/tester/{config_file}");
            if policy && !kept {
                let source = format!(
                    "version: 1\nprofile_policies:\n  - profile: {profile}\n    checkouts: allow\n"
                );
                ctx.home
                    .write_file(&config_file, source.as_bytes(), 0o600, TESTER)?;
                let oracle = Oracle::arm(Fixture::new())?;
                let shown = ctx.run_as_tester(
                    &ctx.diskgc(),
                    &[
                        "profiles",
                        "show",
                        profile,
                        "--config",
                        &config_path,
                        "--json",
                    ],
                )?;
                let verdict = oracle.judge(&shown)?;
                shown.expect_code(0)?;
                let output: serde_json::Value = serde_json::from_str(&shown.stdout)
                    .map_err(|error| format!("{profile}: parse profiles show JSON: {error}"))?;
                ensure!(
                    output["profile"]["checkouts"] == "allow"
                        && output["profile"]["origin"] == "built_in",
                    "{profile}: effective built-in policy missing from profiles show: {output}"
                );
                verdict.assert_clean()?;
            }
            if kept {
                let keep = format!("{cache}/.diskgc-keep");
                ctx.home.write_file(&keep, b"keep", 0o600, TESTER)?;
            }
            let mut args = vec![
                "--dry-run",
                "--json",
                "--no-index",
                "--min-free",
                "100%",
                "--select",
            ];
            let select = format!("profile:{profile}");
            args.push(&select);
            args.extend(["--roots", "/home/tester"]);
            if policy {
                args.extend(["--config", &config_path]);
            }
            args.push("/home/tester");
            let oracle = Oracle::arm(Fixture::new())?;
            let run = ensure_run(ctx, &args)?;
            let verdict = oracle.judge(&run)?;
            run.expect_code(0)?;
            let output = report(&run)?;
            let plan = output["plan"].as_array().ok_or("missing plan")?;
            let entry = plan
                .iter()
                .find(|entry| entry["candidate"] == candidate.to_string_lossy().as_ref())
                .ok_or_else(|| format!("{profile}: candidate missing: {output}"))?;
            ensure!(
                entry["reason"].as_str() == expected_reason
                    && entry["checkouts"] == expected_checkouts,
                "{profile}: expected {expected_reason:?} with {expected_checkouts}, got {entry}"
            );
            verdict.assert_clean()?;
        }
    }
    Ok(())
}

/// A genuinely feature-built binary is needed here: the standard developer
/// binary has the full built-in library and cannot prove release exclusions.
pub fn named_checkout_policy_cannot_expand_proposed_catalog(ctx: &Ctx) -> TestResult {
    let binary = ctx.bin_dir.join("diskgc-proposed-01");
    for excluded in ["npm-cache", "uv-cache"] {
        let source = format!(
            "version: 1\nprofile_policies:\n  - profile: {excluded}\n    checkouts: allow\n"
        );
        let config_file = format!("diskgc-{excluded}-excluded-policy.yaml");
        let config_path = format!("/home/tester/{config_file}");
        ctx.home
            .write_file(&config_file, source.as_bytes(), 0o600, TESTER)?;
        let oracle = Oracle::arm(Fixture::new())?;
        let run = ctx.run_as_tester(&binary, &["check", "--config", &config_path])?;
        let verdict = oracle.judge(&run)?;
        run.expect_code(2)?;
        ensure!(
            run.stderr.contains(&format!(
                "`{excluded}` is a built-in that diskgc 0.1 does not ship"
            )),
            "{excluded}: expected catalog refusal, got {}",
            run.stderr
        );
        verdict.assert_clean()?;
    }
    ctx.home.write_file(
        "diskgc-admitted-policy.yaml",
        b"version: 1\nprofile_policies:\n  - profile: cargo-target\n    checkouts: refuse\n",
        0o600,
        TESTER,
    )?;
    let oracle = Oracle::arm(Fixture::new())?;
    let run = ctx.run_as_tester(
        &binary,
        &[
            "check",
            "--config",
            "/home/tester/diskgc-admitted-policy.yaml",
        ],
    )?;
    let verdict = oracle.judge(&run)?;
    run.expect_code(0)?;
    verdict.assert_clean()
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

/// The pinned Jest package reports its default cache under /tmp. The
/// profile must pass that exact candidate back to Jest and remove no other
/// scratch directory, including one whose leaf looks like another cache.
pub fn jest_cache_cleaner(ctx: &Ctx) -> TestResult {
    let config = Command::new(&ctx.self_exe)
        .args([
            "as-tester",
            "--",
            "/bin/jest",
            "--config",
            "{}",
            "--rootDir",
            "/tmp",
            "--showConfig",
        ])
        .env_clear()
        .envs(TESTER_ENV.iter().copied())
        .current_dir("/tmp")
        .output()
        .map_err(|error| format!("query pinned Jest default cache: {error}"))?;
    ensure!(
        config.status.success(),
        "query pinned Jest default cache: {:?}\nstdout: {}\nstderr: {}",
        config.status,
        String::from_utf8_lossy(&config.stdout),
        String::from_utf8_lossy(&config.stderr)
    );
    let config: serde_json::Value = serde_json::from_slice(&config.stdout)
        .map_err(|error| format!("parse Jest configuration: {error}"))?;
    let default_cache = config["configs"][0]["cacheDirectory"]
        .as_str()
        .ok_or("Jest did not report a cacheDirectory")?;
    let relative = Path::new(default_cache)
        .strip_prefix("/tmp/")
        .map_err(|error| format!("Jest default cache {default_cache} is outside /tmp: {error}"))?;
    let name = relative
        .to_str()
        .ok_or("Jest default cache has a non-UTF-8 name")?;
    ensure!(
        !name.contains('/') && name.starts_with("jest_"),
        "Jest's default cache no longer matches the profile: {default_cache}"
    );
    ctx.scratch.mkdirs(name, 0o700, TESTER)?;
    let payload = ctx.scratch.write_file(
        &format!("{name}/payload"),
        &vec![0x71; PAYLOAD_BYTES],
        0o600,
        TESTER,
    )?;
    ctx.scratch.mkdirs("subdir/jest_decoy", 0o700, TESTER)?;
    let decoy = ctx.scratch.write_file(
        "subdir/jest_decoy/keep",
        b"outside Jest cache",
        0o600,
        TESTER,
    )?;
    let cache = ctx.scratch.path(name);
    let before = file_bytes(&cache)?;
    let oracle = Oracle::arm(Fixture::new().eligible(cache.clone()))?;
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
            "profile:jest-cache",
            "/tmp",
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
        "Jest did not free space in its measured cache: {report}"
    );
    ensure!(
        before >= PAYLOAD_BYTES as u64 && !cache.exists() && !payload.exists() && decoy.exists(),
        "Jest did not delete exactly its measured cache: {report}"
    );
    verdict.assert_clean()
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
