//! Browser cache profiles against a real kernel process and Chromium's
//! `SingletonLock` format. The holder intentionally opens a file outside
//! the cache: only the application lock can protect these candidates.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::ensure_suite::{ensure_run, report};
use crate::holder::Holder;
use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::{TESTER, ensure, info};

const CACHE_ROOT: &str = ".cache/BraveSoftware/Brave-Browser/Default";
const CONFIG_ROOT: &str = ".config/BraveSoftware/Brave-Browser";
const LOCK: &str = ".config/BraveSoftware/Brave-Browser/SingletonLock";

/// A Chromium-style lock protects both named caches even though its process
/// has no descriptor in either cache. A stale PID still blocks because the
/// browser may run in another PID namespace. Removing the lock permits only
/// the named cache directories to be reclaimed.
pub fn brave_cache_lock_blocks_until_absent_then_reclaims_only_cache(ctx: &Ctx) -> TestResult {
    let home = &ctx.home;
    for rel in [
        format!("{CACHE_ROOT}/Cache"),
        format!("{CACHE_ROOT}/Code Cache"),
        format!("{CACHE_ROOT}/Service Worker/CacheStorage"),
        format!("{CACHE_ROOT}/Local Storage"),
        format!("{CONFIG_ROOT}/Default/Service Worker/CacheStorage"),
    ] {
        home.mkdirs(&rel, 0o700, TESTER)?;
    }
    home.write_file(
        &format!("{CACHE_ROOT}/Cache/index"),
        b"cache",
        0o600,
        TESTER,
    )?;
    home.write_file(
        &format!("{CACHE_ROOT}/Code Cache/script"),
        b"compiled",
        0o600,
        TESTER,
    )?;
    home.write_file(
        &format!("{CACHE_ROOT}/Service Worker/CacheStorage/site"),
        b"site data",
        0o600,
        TESTER,
    )?;
    home.write_file(
        &format!("{CACHE_ROOT}/Local Storage/site"),
        b"site data",
        0o600,
        TESTER,
    )?;
    home.write_file(
        &format!("{CONFIG_ROOT}/Default/Service Worker/CacheStorage/site"),
        b"site data",
        0o600,
        TESTER,
    )?;
    home.write_file(
        &format!("{CONFIG_ROOT}/Default/Bookmarks"),
        b"bookmarks",
        0o600,
        TESTER,
    )?;

    let holder = Holder::tester(ctx, &["hold", "/home/tester/.profile", "/"], &[])?;
    let pid = holder.pid();
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map_err(|e| format!("read kernel hostname: {e}"))?;
    let hostname = hostname.trim_end();
    ensure!(!hostname.is_empty(), "kernel hostname is empty");
    let target = format!("{hostname}-{pid}");
    let lock = home.symlink(LOCK, Path::new(&target))?;
    ensure!(
        std::fs::read_link(&lock).map_err(|e| format!("read {lock:?}: {e}"))? == Path::new(&target),
        "SingletonLock did not retain {target:?}"
    );
    let proc_path = PathBuf::from(format!("/proc/{pid}"));
    ensure!(
        proc_path.exists(),
        "holder pid {pid} exited before live check"
    );
    info!("Brave-style SingletonLock {target} names live pid {pid}");

    let cache = home.path(&format!("{CACHE_ROOT}/Cache"));
    let code_cache = home.path(&format!("{CACHE_ROOT}/Code Cache"));
    let expected = BTreeSet::from([
        cache.to_string_lossy().into_owned(),
        code_cache.to_string_lossy().into_owned(),
    ]);
    let args = [
        "--json",
        "--no-index",
        "--min-free",
        "100%",
        "--select",
        "profile:brave-browser-cache",
        "--roots",
        "/home/tester",
        "/home/tester",
    ];

    let live_oracle = Oracle::arm(Fixture::new())?;
    let live = ensure_run(ctx, &args)?;
    let live_verdict = live_oracle.judge(&live)?;
    live.expect_code(1)?;
    let live_report = report(&live)?;
    ensure!(
        candidates(&live_report, "actions")?.is_empty(),
        "live browser triggered cache deletion: {live_report}"
    );
    ensure!(
        candidates_with_reason(&live_report, "in_use")? == expected,
        "live browser caches were not both in_use: {live_report}"
    );
    ensure!(
        cache.exists() && code_cache.exists(),
        "live cache disappeared"
    );
    live_verdict.assert_clean()?;

    holder.finish()?;
    ensure!(
        !proc_path.exists(),
        "holder pid {pid} remains after reaping"
    );
    let stale_oracle = Oracle::arm(Fixture::new())?;
    let stale = ensure_run(ctx, &args)?;
    let stale_verdict = stale_oracle.judge(&stale)?;
    stale.expect_code(1)?;
    let stale_report = report(&stale)?;
    ensure!(
        candidates(&stale_report, "actions")?.is_empty(),
        "stale lock triggered cache deletion: {stale_report}"
    );
    ensure!(
        candidates_with_reason(&stale_report, "in_use")? == expected,
        "stale lock did not keep both caches: {stale_report}"
    );
    ensure!(
        cache.exists() && code_cache.exists(),
        "stale lock removed caches"
    );
    stale_verdict.assert_clean()?;

    home.remove_file(LOCK)?;
    let absent_oracle = Oracle::arm(
        Fixture::new()
            .eligible(cache.clone())
            .eligible(code_cache.clone()),
    )?;
    let absent = ensure_run(ctx, &args)?;
    let absent_verdict = absent_oracle.judge(&absent)?;
    absent.expect_code(1)?;
    let absent_report = report(&absent)?;
    ensure!(
        candidates(&absent_report, "actions")? == expected,
        "absent lock did not reclaim exactly the caches: {absent_report}"
    );
    ensure!(
        !cache.exists() && !code_cache.exists(),
        "caches remain after the lock was removed"
    );
    for rel in [
        format!("{CACHE_ROOT}/Service Worker/CacheStorage/site"),
        format!("{CACHE_ROOT}/Local Storage/site"),
        format!("{CONFIG_ROOT}/Default/Service Worker/CacheStorage/site"),
        format!("{CONFIG_ROOT}/Default/Bookmarks"),
    ] {
        ensure!(home.path(&rel).exists(), "browser data was removed: {rel}");
    }
    ensure!(
        std::fs::symlink_metadata(&lock).is_err(),
        "absent SingletonLock reappeared"
    );
    absent_verdict.assert_clean()
}

fn candidates(report: &Value, field: &str) -> Result<BTreeSet<String>, String> {
    report[field]
        .as_array()
        .ok_or_else(|| format!("{field} is not an array: {report}"))?
        .iter()
        .map(|entry| {
            entry["candidate"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("{field} entry lacks candidate: {entry}"))
        })
        .collect()
}

fn candidates_with_reason(report: &Value, reason: &str) -> Result<BTreeSet<String>, String> {
    let skipped = report["skipped"]
        .as_array()
        .ok_or_else(|| format!("skipped is not an array: {report}"))?;
    let mut found = BTreeSet::new();
    for entry in skipped {
        let candidate = entry["candidate"]
            .as_str()
            .ok_or_else(|| format!("skipped entry lacks candidate: {entry}"))?;
        let actual = entry["reason"]
            .as_str()
            .ok_or_else(|| format!("skipped entry lacks reason: {entry}"))?;
        ensure!(
            actual == reason,
            "unexpected skip reason for {candidate}: {actual}"
        );
        found.insert(candidate.to_owned());
    }
    Ok(found)
}
