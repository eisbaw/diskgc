//! Behavioural tests of the built `diskgc` binary, outside the test
//! containers (the dev shell and the Nix build sandbox).
//!
//! **Nothing here may reclaim on the machine running the tests.** The only
//! reclaiming `ensure` is task-10's AC #6, and it is shaped so that even a
//! broken gate could touch nothing but the test's own fixture: its roots
//! and PATH are a fresh temp directory holding a fake Cargo target, and its
//! config is a file of the test's own. Every other run here is refused
//! before the machine is measured (usage errors).

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn diskgc(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_diskgc"))
        .args(args)
        // No configuration of the developer's leaks in.
        .env_remove("DISKGC_CONFIG")
        .env("XDG_CONFIG_HOME", "/nonexistent-diskgc-test")
        .output()
        .expect("failed to spawn the diskgc binary")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The one JSON object on stdout.
fn json(out: &std::process::Output) -> Value {
    let stdout = text(&out.stdout);
    assert_eq!(stdout.lines().count(), 1, "one line of JSON: {stdout:?}");
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout:?}"))
}

#[test]
fn version_prints_name_and_package_version() {
    let out = diskgc(&["--version"]);
    assert!(out.status.success(), "status: {:?}", out.status);
    assert_eq!(
        text(&out.stdout),
        format!("diskgc {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn no_arguments_is_a_usage_error_with_exit_2() {
    let out = diskgc(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(text(&out.stderr).contains("usage: diskgc"));
}

/// AC #4 (syntax half): a bare number and a month duration are usage
/// errors, exit 2, before anything on the machine is measured; with
/// `--json`, exactly one JSON object says so.
#[test]
fn bare_numbers_and_months_are_usage_errors() {
    for (args, needle) in [
        (
            &["ensure", "--min-free", "50", "/nonexistent"][..],
            "bare number",
        ),
        (
            &[
                "ensure",
                "--min-free",
                "1G",
                "--timeout",
                "1M",
                "/nonexistent",
            ],
            "no months",
        ),
    ] {
        let out = diskgc(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "human mode: nothing on stdout");
        let stderr = text(&out.stderr);
        assert!(
            stderr.contains(needle) && stderr.contains("usage: diskgc"),
            "{stderr}"
        );

        let with_json: Vec<&str> = args.iter().copied().chain(["--json"]).collect();
        let out = diskgc(&with_json);
        assert_eq!(out.status.code(), Some(2));
        let report = json(&out);
        assert_eq!(report["status"], "usage_error");
        assert!(report["path"].is_null() && report["mount"].is_null());
        let message = report["errors"][0]["message"].as_str().expect("message");
        assert!(message.contains(needle), "{message}");
    }
}

/// B2: `--timeout 0` and `--tally-budget 0` as docs/reference.md spells
/// them parse: the run gets past the command line and stops at the
/// nonexistent PATH. It is a dry run with a config file of the test's own:
/// after the command line it reads that file, then resolves PATH and stops
/// there with a usage error, before any scan of the machine's processes.
/// Any other bare number stays a usage error naming the flag.
#[test]
fn timeout_and_tally_budget_zero_as_documented_parse() {
    let tmp = TempDir::new("zero");
    let config = tmp.0.join("config.yaml");
    std::fs::write(&config, "version: 1\n").expect("write");
    let out = diskgc(&[
        "ensure",
        "--dry-run",
        "--json",
        "--config",
        config.to_str().expect("UTF-8"),
        "--min-free",
        "1G",
        "--timeout",
        "0",
        "--tally-budget",
        "0",
        "--select",
        "profile:cargo-target",
        "--roots",
        "/nonexistent-diskgc-test",
        "/nonexistent-diskgc-test",
    ]);
    assert_eq!(out.status.code(), Some(2));
    let report = json(&out);
    let message = report["errors"][0]["message"].as_str().expect("message");
    assert!(
        message.starts_with("PATH /nonexistent-diskgc-test: missing"),
        "past the command line: {message}"
    );
    let out = diskgc(&[
        "ensure",
        "--min-free",
        "1G",
        "--timeout",
        "5",
        "--json",
        "/x",
    ]);
    assert_eq!(out.status.code(), Some(2));
    let message = json(&out)["errors"][0]["message"]
        .as_str()
        .expect("message")
        .to_owned();
    assert!(
        message.contains("--timeout: `5`: a bare number"),
        "{message}"
    );
}

/// A given configuration file that does not exist is exit 2, never a
/// fallback to another file.
#[test]
fn a_missing_given_config_is_exit_2() {
    let out = diskgc(&[
        "ensure",
        "--min-free",
        "1G",
        "--config",
        "/nonexistent-diskgc-test/config.yaml",
        "--select",
        "profile:cargo-target",
        "--roots",
        "/nonexistent",
        "--json",
        "/nonexistent",
    ]);
    assert_eq!(out.status.code(), Some(2));
    let report = json(&out);
    assert_eq!(report["status"], "usage_error");
    let message = report["errors"][0]["message"].as_str().expect("message");
    assert!(
        message.contains("/nonexistent-diskgc-test/config.yaml") && message.contains("ENOENT"),
        "{message}"
    );
}

/// A fresh directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("diskgc-cli-test-{}-{name}", std::process::id()));
        std::fs::create_dir(&dir).expect("create a fresh temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Every object below `root`, with its inode.
fn inventory(root: &Path) -> Vec<(PathBuf, u64)> {
    let mut found = Vec::new();
    let mut todo = vec![root.to_owned()];
    while let Some(at) = todo.pop() {
        let meta = std::fs::symlink_metadata(&at).expect("lstat");
        if meta.is_dir() {
            for entry in std::fs::read_dir(&at).expect("read_dir") {
                todo.push(entry.expect("entry").path());
            }
        }
        found.push((at, meta.ino()));
    }
    found.sort();
    found
}

/// AC #6 of task-10: a reclaiming `ensure` outside a diskgc test container
/// exits 2, naming the pre-release sandbox gate, and deletes nothing. It
/// runs on the developer's machine (and in the Nix build sandbox), so it is
/// shaped to be harmless even if the gate were broken: `--roots` and PATH
/// are this test's fresh temp directory, holding a fake but recognisable
/// Cargo target (a valid CACHEDIR.TAG, a profile directory with
/// `.cargo-lock`, `.fingerprint/` and `deps/`), with a goal that cannot be
/// met (100% free) and a config file of its own; never a real path, `$HOME`,
/// `/tmp` at large or a default root. Every fixture object is still there,
/// same inode, afterwards.
#[test]
fn a_reclaiming_ensure_outside_a_container_exits_2_naming_the_sandbox_gate() {
    let tmp = TempDir::new("gate");
    let w = tmp.0.join("w");
    let debug = w.join("p/target/debug");
    std::fs::create_dir_all(debug.join(".fingerprint/p-1")).expect("mkdir");
    std::fs::create_dir_all(debug.join("deps")).expect("mkdir");
    std::fs::write(w.join("p/Cargo.toml"), "[package]\nname = \"p\"\n").expect("write");
    std::fs::write(
        w.join("p/target/CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    )
    .expect("write");
    std::fs::write(debug.join(".cargo-lock"), "").expect("write");
    std::fs::write(debug.join(".fingerprint/p-1/bin-p"), "x").expect("write");
    std::fs::write(debug.join("deps/p-1"), vec![7u8; 64 << 10]).expect("write");
    let config = tmp.0.join("config.yaml");
    std::fs::write(&config, "version: 1\n").expect("write");
    let before = inventory(&w);

    let w_text = w.to_str().expect("UTF-8 temp path");
    let out = diskgc(&[
        "ensure",
        "--min-free",
        "100%",
        "--config",
        config.to_str().expect("UTF-8"),
        "--select",
        "profile:cargo-target",
        "--roots",
        w_text,
        "--json",
        w_text,
    ]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(stderr.contains("sandbox gate"), "{stderr}");
    let report = json(&out);
    assert_eq!(report["status"], "usage_error");
    let message = report["errors"][0]["message"].as_str().expect("message");
    assert!(message.contains("pre-release sandbox gate"), "{message}");
    assert!(report["actions"].as_array().is_some_and(Vec::is_empty));
    assert_eq!(
        inventory(&w),
        before,
        "a fixture object went or was replaced"
    );
}
