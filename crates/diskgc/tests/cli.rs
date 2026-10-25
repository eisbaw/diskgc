//! Behavioural tests of the built `diskgc` binary, outside the test
//! containers (the dev shell and the Nix build sandbox). The `profiles`
//! tests compare its output with `diskgc-core`'s library, so a built-in
//! added there is checked here without editing a test.
//!
//! **Nothing here may reclaim on the machine running the tests.** The only
//! reclaiming `ensure`s are task-10's AC #6 (and task-18's kind `fs` twin
//! in the same test), shaped so that even a broken gate could touch nothing
//! but the test's own fixture: their roots and PATH are a fresh temp
//! directory holding a fake Cargo target and an old directory, and their
//! config is a file of the test's own. Every other run here is refused
//! before the machine is measured (usage errors).

use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(not(feature = "proposed-release-01-catalog"))]
use diskgc_core::config;
use diskgc_core::profile::Library;
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
fn version_with_json_in_either_order_is_plain_text() {
    for args in [
        ["--version", "--json"],
        ["--json", "--version"],
        ["-V", "--json"],
        ["--json", "-V"],
    ] {
        let out = diskgc(&args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: {}",
            text(&out.stderr)
        );
        assert_eq!(
            text(&out.stdout),
            format!("diskgc {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}

#[test]
fn non_ensure_json_errors_have_typed_causes() {
    let bad_flag = diskgc(&["profiles", "list", "--json=maybe"]);
    assert_eq!(bad_flag.status.code(), Some(2));
    let parsed = json(&bad_flag);
    let keys: BTreeSet<&str> = parsed
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from(["status", "exit_code", "cause", "error"])
    );
    assert_eq!(parsed["status"], "usage_error");
    assert_eq!(parsed["exit_code"], 2);
    assert_eq!(parsed["cause"], "parse");
    assert!(parsed["error"].as_str().unwrap().contains("--json"));

    let missing = diskgc(&[
        "profiles",
        "list",
        "--config",
        "/nonexistent-diskgc-config.yaml",
        "--json",
    ]);
    assert_eq!(missing.status.code(), Some(2));
    assert_eq!(json(&missing)["cause"], "config");
}

#[test]
fn explicit_selection_syntax_and_root_binding_have_distinct_causes() {
    let common = ["ensure", "--dry-run", "--json", "--min-free", "1G"];
    for (selection, cause) in [
        ("profile:cargo-target && (", "parse"),
        ("profile:cargo-target", "usage"),
    ] {
        let out = diskgc(
            &[
                &common[..],
                &["--select", selection, "/nonexistent-diskgc-test"],
            ]
            .concat(),
        );
        assert_eq!(
            out.status.code(),
            Some(2),
            "{selection}: {}",
            text(&out.stderr)
        );
        let report = json(&out);
        assert_eq!(report["errors"][0]["cause"], cause, "{selection}");
    }
}

#[test]
fn selection_error_escapes_control_characters_in_the_original_argument() {
    let out = diskgc(&[
        "ensure",
        "--dry-run",
        "--json",
        "--min-free",
        "1G",
        "--select",
        "profile:cargo-target\n",
        "/nonexistent-diskgc-test",
    ]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("profile:cargo-target\\n"), "{stderr:?}");
    assert!(!stderr.contains("profile:cargo-target\n"), "{stderr:?}");
}

#[test]
fn unsupported_valid_selection_escapes_newline_in_its_diagnostic() {
    let expression = "(profile:cargo-target && !filter:idle-1d)\n || profile:cargo-target";
    let out = diskgc(&[
        "ensure",
        "--dry-run",
        "--json",
        "--min-free",
        "1G",
        "--select",
        expression,
        "--roots",
        "/nonexistent-diskgc-test",
        "/nonexistent-diskgc-test",
    ]);
    assert_eq!(out.status.code(), Some(2));
    let report = json(&out);
    assert_eq!(report["errors"][0]["cause"], "usage");
    let stderr = text(&out.stderr);
    assert!(stderr.contains("!filter:idle-1d)\\n"), "{stderr:?}");
    assert!(!stderr.contains("!filter:idle-1d)\n"), "{stderr:?}");
}

#[test]
fn selected_redundancy_warning_reaches_cli_without_changing_result() {
    let tmp = TempDir::new("select-warning");
    let config = tmp.0.join("config.yaml");
    std::fs::write(&config, "version: 1\n").expect("write config");
    let base = [
        "ensure",
        "--dry-run",
        "--json",
        "--config",
        config.to_str().expect("UTF-8"),
        "--min-free",
        "1G",
    ];
    let roots = [
        "--roots",
        "/nonexistent-diskgc-test",
        "/nonexistent-diskgc-test",
    ];
    let plain = diskgc(&[&base[..], &["--select", "profile:cargo-target"], &roots].concat());
    let redundant = diskgc(
        &[
            &base[..],
            &["--select", "profile:cargo-target || profile:cargo-target"],
            &roots,
        ]
        .concat(),
    );
    assert_eq!(plain.status.code(), redundant.status.code());
    assert_eq!(redundant.status.code(), Some(2));
    assert_eq!(json(&plain)["status"], json(&redundant)["status"]);
    let stderr = text(&redundant.stderr);
    assert!(
        stderr.contains("warning: --select") && stderr.contains("duplicate"),
        "{stderr}"
    );
}

#[test]
fn no_arguments_is_a_usage_error_with_exit_2() {
    let out = diskgc(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(text(&out.stderr).contains("usage: diskgc"));
}

/// TASK-148: a usage error is its message first, then only the command's
/// usage lines and one pointer to `--help`; the help body (which would
/// scroll the error away) is not printed.
#[test]
fn a_usage_error_shows_the_error_then_only_the_command_usage() {
    for (args, error, usage_lines) in [
        (
            &["ensure", "--min-fre", "1G", "/nonexistent"][..],
            "diskgc: error: ensure: unknown flag --min-fre; did you mean --min-free?",
            3,
        ),
        (
            &["profiles", "frob"],
            "diskgc: error: profiles: unknown subcommand frob",
            3,
        ),
        (
            &["check", "extra"],
            "diskgc: error: check: unknown argument extra",
            1,
        ),
        (&[], "diskgc: error: no command given", 9),
    ] {
        let out = diskgc(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let stderr = text(&out.stderr);
        let lines: Vec<&str> = stderr.lines().collect();
        assert!(lines[0].starts_with(error), "{args:?}:\n{stderr}");
        assert!(lines[1].starts_with("usage: diskgc"), "{args:?}:\n{stderr}");
        assert_eq!(lines.len(), 1 + usage_lines + 1, "{args:?}:\n{stderr}");
        assert_eq!(
            lines.last(),
            Some(&"Run `diskgc --help` for examples, sizes, durations and exit codes."),
            "{args:?}"
        );
        assert!(!stderr.contains("Start here"), "{args:?}:\n{stderr}");
    }
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
        assert_eq!(report["errors"][0]["cause"], "parse");
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
    assert!(report["started_at"].is_null());
    assert!(report["duration_ms"].is_null());
    assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
    let message = report["errors"][0]["message"].as_str().expect("message");
    assert!(
        message.contains("/nonexistent-diskgc-test/config.yaml") && message.contains("ENOENT"),
        "{message}"
    );
}

#[cfg(not(feature = "proposed-release-01-catalog"))]
#[test]
fn check_prints_normalized_tiers_and_needs_no_deployment_roots() {
    let tmp = TempDir::new("check-valid");
    let path = tmp.0.join("config.yaml");
    std::fs::write(&path, "version: 1\nprofiles:\n  - name: a\n    find: {roots: [/a-does-not-exist], paths: ['x-*'], type: dir}\n    action: delete\n    examples: {match: [/a-does-not-exist/x-1]}\n  - name: b\n    find: {roots: [/b-does-not-exist], paths: ['x-*'], type: dir}\n    action: delete\n    examples: {match: [/b-does-not-exist/x-1]}\ngroups:\n  - name: stale\n    tiers:\n      - name: old\n        select: (profile:a || profile:b) && filter:idle-3d\n").expect("write");
    let out = diskgc(&["check", "--config", path.to_str().expect("UTF-8")]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.starts_with(&format!("config {}\n", path.display())),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "stale.old: (profile:a && filter:idle-3d) || (profile:b && filter:idle-3d) => [a, b]"
        ),
        "{stdout}"
    );
}

/// TASK-152 and TASK-178: `check` warns about a supplied root that does not
/// exist, and only that one: an existing root, and one reached through a
/// symlink (never followed, so never looked up beyond the screened mount
/// chain), get no warning. Exit 0 either way.
#[cfg(not(feature = "proposed-release-01-catalog"))]
#[test]
fn check_warns_only_about_an_absent_supplied_root() {
    let tmp = TempDir::new("check-missing-root");
    let present = tmp.0.join("present");
    std::fs::create_dir(&present).expect("mkdir");
    std::os::unix::fs::symlink("/nonexistent-diskgc-target", tmp.0.join("link")).expect("symlink");
    let absent = tmp.0.join("absent");
    let through_link = tmp.0.join("link/below");
    let path = tmp.0.join("config.yaml");
    std::fs::write(
        &path,
        format!(
            "version: 1\nprofiles:\n  - name: guard\n    find: {{roots: required, paths: ['*'], type: dir}}\n    action: delete\n    examples: {{root: /tmp, match: [/tmp/x]}}\nprotect: [{{profile: guard, roots: [{}, {}, {}]}}]\n",
            present.display(),
            absent.display(),
            through_link.display()
        ),
    )
    .expect("write");
    let out = diskgc(&["check", "--config", path.to_str().expect("UTF-8")]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert_eq!(
        stderr.matches("does not exist on this machine").count(),
        1,
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "warning: root {} does not exist",
            absent.display()
        )),
        "{stderr}"
    );
}

#[cfg(not(feature = "proposed-release-01-catalog"))]
#[test]
fn check_warns_about_broad_non_tmp_patterns_and_checkout_permission() {
    let tmp = TempDir::new("check-warning");
    let path = tmp.0.join("config.yaml");
    std::fs::write(&path, "version: 1\nprofiles:\n  - name: broad\n    find: {roots: [/home/example/.cache], paths: ['*'], type: dir}\n    action: delete\n    checkouts: allow\n    examples: {match: [/home/example/.cache/entry]}\n").expect("write");
    let out = diskgc(&["check", "--config", path.to_str().expect("UTF-8")]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("only wildcards outside /tmp or /var/tmp"),
        "{stderr}"
    );
    assert!(stderr.contains("allows VCS checkouts"), "{stderr}");
    assert_eq!(
        stderr.matches("allows VCS checkouts").count(),
        1,
        "{stderr}"
    );
    assert!(
        !text(&out.stdout).contains("warning:"),
        "{}",
        text(&out.stdout)
    );
}

#[cfg(not(feature = "proposed-release-01-catalog"))]
#[test]
fn check_warns_once_for_explicit_builtin_checkout_allowance() {
    let tmp = TempDir::new("check-builtin-allow-warning");
    let path = tmp.0.join("config.yaml");
    std::fs::write(
        &path,
        "version: 1\nprofile_policies:\n  - profile: cargo-target\n    checkouts: allow\n",
    )
    .expect("write");
    let out = diskgc(&["check", "--config", path.to_str().expect("UTF-8")]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stderr = text(&out.stderr);
    assert_eq!(
        stderr.matches("allows VCS checkouts").count(),
        1,
        "{stderr}"
    );
    assert!(
        !text(&out.stdout).contains("warning:"),
        "{}",
        text(&out.stdout)
    );
}

#[test]
fn check_never_falls_back_after_an_explicit_file_typo() {
    let tmp = TempDir::new("check-lookup");
    let xdg = tmp.0.join("xdg/diskgc");
    std::fs::create_dir_all(&xdg).expect("mkdir");
    std::fs::write(xdg.join("config.yaml"), "version: 1\n").expect("write");
    let absent = tmp.0.join("typo.yaml");
    let mut command = Command::new(env!("CARGO_BIN_EXE_diskgc"));
    command
        .args(["check", "--config", absent.to_str().expect("UTF-8")])
        .env("XDG_CONFIG_HOME", tmp.0.join("xdg"));
    let out = command.output().expect("check");
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains(&absent.display().to_string()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_diskgc"));
    command
        .arg("check")
        .env("DISKGC_CONFIG", &absent)
        .env("XDG_CONFIG_HOME", tmp.0.join("xdg"));
    let out = command.output().expect("check");
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains(&absent.display().to_string()));
}

#[test]
fn invalid_config_has_the_same_error_in_check_and_ensure_before_deletion() {
    let tmp = TempDir::new("check-invalid");
    let path = tmp.0.join("config.yaml");
    std::fs::write(&path, "version: 1\ngroups:\n  - name: stale\n    tiers:\n      - name: old\n        select: profile:missing\n").expect("write");
    let before = inventory(&tmp.0);
    let config = path.to_str().expect("UTF-8");
    let root = tmp.0.to_str().expect("UTF-8");
    let check = diskgc(&["check", "--config", config]);
    assert_eq!(check.status.code(), Some(2));
    let ensure = diskgc(&[
        "ensure",
        "--config",
        config,
        "--min-free",
        "100%",
        "--select",
        "profile:cargo-target",
        "--roots",
        root,
        root,
    ]);
    assert_eq!(ensure.status.code(), Some(2));
    let check_error = text(&check.stderr);
    let ensure_error = text(&ensure.stderr);
    assert!(
        check_error.contains("tier `old`: invalid selection"),
        "{check_error}"
    );
    assert!(
        ensure_error.contains("tier `old`: invalid selection"),
        "{ensure_error}"
    );
    assert_eq!(inventory(&tmp.0), before, "invalid policy touched fixture");
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
/// same inode, afterwards. The same for a kind `fs` reclaim (task-18): a
/// user profile of the config, `roots: required`, given the same temp
/// directory, which holds an old plain directory it finds.
#[cfg(not(feature = "proposed-release-01-catalog"))]
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
    std::fs::create_dir_all(w.join("old-scratch/sub")).expect("mkdir");
    std::fs::write(w.join("old-scratch/sub/f"), vec![7u8; 4096]).expect("write");
    let config = tmp.0.join("config.yaml");
    std::fs::write(
        &config,
        "version: 1\nprofiles:\n  - name: gate-scratch\n    find: {roots: required, paths: \
         [\"old-*\"], type: dir}\n    action: delete\n    examples: {match: \
         [/srv/w/old-x]}\n",
    )
    .expect("write");
    let before = inventory(&w);

    let w_text = w.to_str().expect("UTF-8 temp path");
    for select in ["profile:cargo-target", "profile:gate-scratch"] {
        let out = diskgc(&[
            "ensure",
            "--min-free",
            "100%",
            "--config",
            config.to_str().expect("UTF-8"),
            "--select",
            select,
            "--roots",
            w_text,
            "--json",
            w_text,
        ]);
        let stderr = text(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{select}: stderr: {stderr}");
        assert!(stderr.contains("sandbox gate"), "{select}: {stderr}");
        let report = json(&out);
        assert_eq!(report["status"], "usage_error", "{select}");
        let message = report["errors"][0]["message"].as_str().expect("message");
        assert!(
            message.contains("pre-release sandbox gate"),
            "{select}: {message}"
        );
        assert!(report["actions"].as_array().is_some_and(Vec::is_empty));
        assert_eq!(
            inventory(&w),
            before,
            "{select}: a fixture object went or was replaced"
        );
    }
}

// ---- task-92: `--help` and `profiles list | show` (agent onboarding)

/// The message of the one JSON error object a failed command other than
/// `ensure` prints, after checking the fields agents branch on.
fn error_of(out: &std::process::Output) -> String {
    let object = json(out);
    assert_eq!(object["status"], "usage_error", "{object}");
    assert_eq!(object["exit_code"], 2, "{object}");
    assert_eq!(out.status.code(), Some(2));
    object["error"].as_str().expect("an error").to_owned()
}

/// docs/reference.md, read from the source tree (the flake's
/// workspace-tests source includes it).
fn reference_md() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/reference.md");
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The exit codes of docs/reference.md's `ensure` table (`130 / 143` is
/// two).
fn documented_exit_codes() -> BTreeSet<u32> {
    let doc = reference_md();
    let table = doc
        .lines()
        .skip_while(|l| *l != "## Exit codes (`ensure`)")
        .skip_while(|l| !l.starts_with("| Code |"))
        .skip(2)
        .take_while(|l| l.starts_with('|'));
    let codes: BTreeSet<u32> = table
        .flat_map(|row| {
            let cell = row.split('|').nth(1).expect("a code cell").to_owned();
            cell.split('/')
                .map(|c| c.trim().parse::<u32>().expect("a code"))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(!codes.is_empty(), "no exit code table in docs/reference.md");
    codes
}

/// task-92 round 2 (M1): `--help` lets an agent start on its own: a dry
/// run first, what PATH is, that --min-free is required, that ensure
/// without --dry-run deletes, the --json promise, and one exit-code line
/// holding exactly the codes of docs/reference.md's table.
#[test]
fn help_starts_an_agent_with_a_dry_run() {
    let out = diskgc(&["--help"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let help = text(&out.stdout);
    let (_, start) = help
        .split_once("\nStart here")
        .unwrap_or_else(|| panic!("no Start here block:\n{help}"));
    let example = start.lines().nth(1).expect("the example's first line");
    assert!(
        example
            .trim_start()
            .starts_with("diskgc ensure --dry-run --json --min-free"),
        "the dry run comes first: {example}"
    );
    for needle in [
        "--select 'profile:cargo-target && filter:idle-3d' --roots DIR PATH",
        "PATH: the directory whose filesystem must have --min-free available",
        "--min-free is required, also with --dry-run",
        "Without --dry-run, ensure DELETES",
        "--json prints exactly one JSON object on stdout for every exit when stdout",
        "accepts the write; --help prints the help as text",
    ] {
        assert!(start.contains(needle), "{needle:?}:\n{start}");
    }
    let mut lines = start
        .lines()
        .skip_while(|l| !l.starts_with("Exit codes (ensure): "));
    let mut exit_line = lines.next().expect("an exit code line").to_owned();
    for more in lines.take_while(|l| l.starts_with("  ")) {
        exit_line.push(' ');
        exit_line.push_str(more);
    }
    let exit_line = exit_line.split_whitespace().collect::<Vec<_>>().join(" ");
    // Each code with its label, the dry run's exit 0 explained.
    for labelled in [
        "0 met (--dry-run: 0 with status dry_run; read target_met)",
        "1 exhausted",
        "2 usage/config",
        "3 error",
        "75 another reclaim holds the lock (retry later)",
        "124 deadline",
        "130/143 signal",
    ] {
        assert!(exit_line.contains(labelled), "{labelled:?}: {exit_line}");
    }
    let codes: BTreeSet<u32> = exit_line
        .split(|c: char| !c.is_ascii_digit())
        .filter(|w| !w.is_empty())
        .map(|w| w.parse().expect("a number"))
        .collect();
    assert_eq!(codes, documented_exit_codes(), "{exit_line}");
    assert!(
        help.contains("(roots required: --roots DIR)"),
        "the required-roots marker:\n{help}"
    );
}

/// task-92 round 2 (M2): `-h` / `--help` anywhere is the full help (the
/// Profiles section included), exit 0, nothing on stderr.
#[test]
fn help_anywhere_prints_the_full_help() {
    let full = diskgc(&["--help"]);
    assert_eq!(full.status.code(), Some(0));
    for args in [
        &["profiles", "--help"][..],
        &["profiles", "list", "--help"],
        &["profiles", "show", "cargo-target", "-h"],
        &["ensure", "--help"],
        &["ensure", "--min-free", "50", "--help"],
    ] {
        let out = diskgc(args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: {}",
            text(&out.stderr)
        );
        assert!(out.stderr.is_empty(), "{args:?}: {}", text(&out.stderr));
        assert_eq!(out.stdout, full.stdout, "{args:?}");
    }
    assert!(text(&full.stdout).contains("\nProfiles built in"));
}

/// task-92 round 2 (M3): `--json` gives one JSON object on every exit,
/// also when the parser cannot take it as the flag: `--json=yes` and
/// `--config --json` (where it is `--config`'s value).
#[test]
fn json_anywhere_gives_one_object() {
    for (args, needle) in [
        (
            &["profiles", "list", "--json=yes"][..],
            "--json takes no value",
        ),
        (
            &["profiles", "list", "--config", "--json"],
            "the configuration file must be an absolute path",
        ),
        (
            &["profiles", "--json"],
            "list, show NAME or test is required",
        ),
        (&["watch", "--json"], "`watch` is not in diskgc 0.1.0 yet"),
    ] {
        let out = diskgc(args);
        let error = error_of(&out);
        assert!(error.contains(needle), "{args:?}: {error}");
    }
    for args in [
        &["ensure", "--json=yes", "--min-free", "1G", "/x"][..],
        &["ensure", "--min-free", "1G", "--config", "--json", "/x"],
    ] {
        let out = diskgc(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert_eq!(json(&out)["status"], "usage_error", "{args:?}");
    }
}

/// The built-in profiles, as the binary's library has them.
#[cfg(not(feature = "proposed-release-01-catalog"))]
fn builtin() -> Library {
    Library::builtin().expect("the built-in profiles load")
}

/// The library `profiles` should show for a run whose JSON says it read
/// `config` (`null`: none found, the built-ins alone). Normally `null`
/// here; a machine with `/etc/diskgc/config.yaml` installed (which the
/// search reaches, whatever `$XDG_CONFIG_HOME` says) is compared with
/// that file instead.
#[cfg(not(feature = "proposed-release-01-catalog"))]
fn expected_library(config: &Value) -> Library {
    match config.as_str() {
        None => builtin(),
        Some(path) => {
            let bytes = std::fs::read(path).expect("read the config the binary read");
            config::parse(Path::new(path), &bytes)
                .expect("it loaded for the binary")
                .library
        }
    }
}

#[cfg(not(feature = "proposed-release-01-catalog"))]
fn names(library: &Library) -> BTreeSet<String> {
    library.profiles().map(|p| p.name.clone()).collect()
}

/// The profile names of a `profiles list --json` object, each once.
fn listed(list: &Value) -> BTreeSet<String> {
    let entries = list["profiles"].as_array().expect("a profiles list");
    let names: BTreeSet<String> = entries
        .iter()
        .map(|p| p["name"].as_str().expect("a name").to_owned())
        .collect();
    assert_eq!(names.len(), entries.len(), "a profile listed twice");
    names
}

/// A config file holding user profiles, in a fresh temp dir.
#[cfg(not(feature = "proposed-release-01-catalog"))]
const USER_PROFILES: &str = "\
version: 1
profiles:
  - name: zephyr-run
    category: build
    description: Zephyr test runs
    find: {roots: [/tmp], paths: [\"zephyr-*-run.*\"], type: dir}
    action: delete
    examples: {match: [/tmp/zephyr-h5e5-run.jTm9zGlH], no_match: [/tmp/zephyr-hal]}
  - name: go-cache
    find: {roots: required, paths: [go-build], type: dir}
    action: {command: [env, \"GOCACHE={path}\", go, clean, -cache]}
    examples: {match: [/home/u/go-build]}
";

#[cfg(not(feature = "proposed-release-01-catalog"))]
fn user_config(tmp: &TempDir) -> (PathBuf, Library) {
    let path = tmp.0.join("config.yaml");
    std::fs::write(&path, USER_PROFILES).expect("write");
    let library = config::parse(&path, USER_PROFILES.as_bytes())
        .expect("the test config loads")
        .library;
    (path, library)
}

/// task-92 AC #1 and #3: `--help` ends with a Profiles section naming
/// every built-in profile. The test iterates the library, so a new
/// built-in is checked without editing the help text or this test; a
/// help text that names profiles by hand fails as soon as one is added.
#[cfg(not(feature = "proposed-release-01-catalog"))]
#[test]
fn help_names_every_built_in_profile() {
    let out = diskgc(&["--help"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(out.stderr.is_empty(), "{}", text(&out.stderr));
    let help = text(&out.stdout);
    assert!(help.starts_with("usage: diskgc ensure"), "{help}");
    let (_, section) = help
        .split_once("\nProfiles built in")
        .unwrap_or_else(|| panic!("no Profiles section:\n{help}"));
    let words: BTreeSet<&str> = section
        .split(|c: char| c.is_whitespace() || c == ',')
        .collect();
    let library = builtin();
    for profile in library.profiles() {
        assert!(
            words.contains(profile.name.as_str()),
            "--help does not name the built-in `{}`:\n{section}",
            profile.name
        );
    }
    for pointer in [
        "`diskgc profiles list`",
        "`diskgc profiles show NAME`",
        "`--json`",
    ] {
        assert!(section.contains(pointer), "{pointer}:\n{section}");
    }
}

/// Exercise the built binary with the proposed catalog feature, including
/// the command paths that do not construct a Tier.
#[cfg(feature = "proposed-release-01-catalog")]
#[test]
fn proposed_catalog_is_consistent_across_cli_surfaces() {
    let help = diskgc(&["--help"]);
    assert_eq!(help.status.code(), Some(0));
    let help = text(&help.stdout);
    assert!(help.contains("cargo-target"));
    assert!(!help.contains("docker-build-cache"));

    let list = diskgc(&["profiles", "list", "--json"]);
    assert_eq!(list.status.code(), Some(0), "{}", text(&list.stderr));
    let names = listed(&json(&list));
    // The expected catalog is derived from builtin.yaml's `releases:`, the
    // one place it is decided: a hard-coded count went stale silently when
    // TASK-65 added nix-shell-rc and pytest-tmp.
    let catalog: BTreeSet<String> = Library::builtin()
        .expect("the built-in profiles load")
        .proposed_release_01()
        .expect("the 0.1 catalog")
        .profiles()
        .map(|p| p.name.clone())
        .collect();
    assert_eq!(names, catalog);
    assert!(names.contains("cargo-target"));
    assert!(!names.contains("docker-build-cache"));

    let show = diskgc(&["profiles", "show", "docker-build-cache", "--json"]);
    assert_eq!(show.status.code(), Some(2));
    assert!(error_of(&show).contains("docker-build-cache"));

    let tmp = TempDir::new("proposed-catalog");
    let config = tmp.0.join("config.yaml");
    std::fs::write(
        &config,
        "version: 1\ngroups:\n  - name: safe\n    tiers:\n      - name: cargo\n        select: profile:cargo-target\n        roots: [/tmp]\n  - name: excluded\n    tiers:\n      - name: docker\n        select: profile:docker-build-cache\n",
    )
    .expect("write config");
    let check = diskgc(&["check", "--config", config.to_str().expect("UTF-8")]);
    assert_eq!(check.status.code(), Some(2));
    assert!(text(&check.stderr).contains("docker-build-cache"));

    let ensure = diskgc(&[
        "ensure",
        "--min-free",
        "1G",
        "--dry-run",
        "--json",
        "--select",
        "profile:docker-build-cache",
        "--roots",
        "/nonexistent-diskgc-test",
        "/nonexistent-diskgc-test",
    ]);
    assert_eq!(ensure.status.code(), Some(2));
    assert!(
        json(&ensure)["errors"][0]["message"]
            .as_str()
            .expect("message")
            .contains("docker-build-cache")
    );
}

/// task-92 AC #2 and #3: `profiles list --json` is one object listing the
/// library's profiles, each once: the built-ins without a config file (no
/// error), plus the config's user profiles with one.
#[cfg(not(feature = "proposed-release-01-catalog"))]
#[test]
fn profiles_list_json_lists_the_library() {
    let out = diskgc(&["profiles", "list", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let list = json(&out);
    assert_eq!(listed(&list), names(&expected_library(&list["config"])));

    let tmp = TempDir::new("list");
    let (path, library) = user_config(&tmp);
    let path_text = path.to_str().expect("UTF-8");
    let out = diskgc(&["profiles", "list", "--config", path_text, "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains(&format!("config {path_text}")));
    let list = json(&out);
    assert_eq!(list["config"], path_text);
    assert_eq!(listed(&list), names(&library));
    assert!(listed(&list).is_superset(&names(&builtin())));
    for entry in list["profiles"].as_array().expect("profiles") {
        let name = entry["name"].as_str().expect("name");
        let profile = library.get(name).expect("a profile of the library");
        assert_eq!(entry["origin"], profile.origin.name(), "{name}");
    }
}

/// The human listing: one header and one row per profile, each row
/// starting with its name.
#[cfg(not(feature = "proposed-release-01-catalog"))]
#[test]
fn profiles_list_is_one_table_row_per_profile() {
    let tmp = TempDir::new("table");
    let (path, library) = user_config(&tmp);
    let out = diskgc(&[
        "profiles",
        "list",
        "--config",
        path.to_str().expect("UTF-8"),
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let table = text(&out.stdout);
    let mut lines = table.lines();
    let header = lines.next().expect("a header");
    for column in [
        "NAME",
        "ORIGIN",
        "ROLE",
        "CATEGORY",
        "KIND",
        "ACTION",
        "ROOTS",
        "CHECKOUTS",
    ] {
        assert!(header.contains(column), "{header}");
    }
    let rows: BTreeSet<String> = lines
        .map(|l| l.split_whitespace().next().expect("a name").to_owned())
        .collect();
    assert_eq!(rows, names(&library));
    let zephyr = table
        .lines()
        .find(|l| l.starts_with("zephyr-run "))
        .expect("the user profile's row");
    for cell in ["user", "source", "build", "fs", "delete", "/tmp", "refuse"] {
        assert!(zephyr.contains(cell), "{zephyr}");
    }
}

/// task-92 AC #2: `show cargo-target --json` is its full definition:
/// `checkouts: allow`, roots required, the kind's fixed action, the tool
/// versions; it takes no examples (`null`). A user fs profile's examples
/// are shown, `match` and `no_match`.
#[cfg(not(feature = "proposed-release-01-catalog"))]
#[test]
fn profiles_show_json_is_the_full_definition() {
    let out = diskgc(&["profiles", "show", "cargo-target", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let shown = json(&out);
    let cargo = &shown["profile"];
    assert_eq!(cargo["name"], "cargo-target");
    assert_eq!(cargo["origin"], "built_in");
    assert_eq!(cargo["kind"], "cargo-target");
    assert_eq!(cargo["checkouts"], "allow");
    assert!(cargo["find"]["roots"].is_null(), "{cargo}");
    assert_eq!(cargo["find"]["roots_required"], true);
    assert_eq!(cargo["action"]["type"], "cargo_lock_protocol");
    assert!(
        cargo["examples"].is_null(),
        "cargo-target takes no examples"
    );
    assert!(
        cargo["verified_with"]
            .as_str()
            .is_some_and(|v| v.contains("cargo")),
        "{cargo}"
    );

    let tmp = TempDir::new("show");
    let (path, _) = user_config(&tmp);
    let config = path.to_str().expect("UTF-8");
    let out = diskgc(&[
        "profiles",
        "show",
        "zephyr-run",
        "--config",
        config,
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let zephyr = &json(&out)["profile"];
    assert_eq!(zephyr["origin"], "user");
    assert_eq!(zephyr["checkouts"], "refuse", "the default, spelled out");
    assert_eq!(
        zephyr["examples"]["match"][0],
        "/tmp/zephyr-h5e5-run.jTm9zGlH"
    );
    assert_eq!(zephyr["examples"]["no_match"][0], "/tmp/zephyr-hal");
    assert_eq!(zephyr["find"]["paths"][0], "zephyr-*-run.*");
    assert_eq!(zephyr["find"]["roots"][0], "/tmp");
    assert_eq!(zephyr["find"]["roots_required"], false);

    // The human form is YAML that spells the same out.
    let out = diskgc(&["profiles", "show", "zephyr-run", "--config", config]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let yaml = text(&out.stdout);
    for line in [
        "- name: zephyr-run",
        "  checkouts: refuse",
        "    match: [/tmp/zephyr-h5e5-run.jTm9zGlH]",
        "    no_match: [/tmp/zephyr-hal]",
    ] {
        assert!(yaml.lines().any(|l| l == line), "{line:?}:\n{yaml}");
    }
}

/// An unknown name is exit 2, naming the close names; with `--json`, one
/// object says so.
#[test]
fn profiles_show_of_an_unknown_name_is_exit_2() {
    let out = diskgc(&["profiles", "show", "cargo"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("no profile is named `cargo`; close names: `cargo-target`"),
        "{stderr}"
    );
    let out = diskgc(&["profiles", "show", "idle-5d", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(json(&out)["cause"], "usage");
    let error = error_of(&out);
    assert!(error.contains("`idle-1d`, `idle-2d`"), "{error}");
}

/// A broken config is exit 2 with the loader's message, as `ensure`
/// reports it; a given config that is missing is exit 2, never a fallback
/// to the built-ins.
#[test]
fn profiles_with_a_broken_or_missing_config_is_exit_2() {
    let tmp = TempDir::new("broken");
    let path = tmp.0.join("config.yaml");
    std::fs::write(
        &path,
        "version: 1\nprofiles:\n  - name: x\n    find: {roots: [/tmp], paths: [a]}\n",
    )
    .expect("write");
    let config = path.to_str().expect("UTF-8");
    for args in [
        &["profiles", "list", "--config", config][..],
        &["profiles", "show", "cargo-target", "--config", config],
    ] {
        let out = diskgc(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty());
        let stderr = text(&out.stderr);
        assert!(
            stderr.contains(&format!("{config}:3: profiles[0] (`x`)"))
                && stderr.contains("kind fs needs `find.type`"),
            "{stderr}"
        );
    }
    let out = diskgc(&["profiles", "list", "--json", "--config", config]);
    assert_eq!(out.status.code(), Some(2));
    let error = error_of(&out);
    assert!(error.contains("kind fs needs `find.type`"), "{error}");

    let out = diskgc(&[
        "profiles",
        "list",
        "--config",
        "/nonexistent-diskgc-test/config.yaml",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(text(&out.stderr).contains("ENOENT"));
}

/// Usage errors of `profiles` are exit 2, one JSON object with `--json`;
/// `profiles test` checks path examples in core and reports all failures.
#[test]
fn profiles_usage_errors_are_exit_2() {
    for (args, needle) in [
        (&["profiles"][..], "list, show NAME or test"),
        (&["profiles", "show"], "show needs a NAME"),
        (&["profiles", "list", "x"], "list takes no NAME"),
        (&["profiles", "list", "--frob"], "unknown flag --frob"),
        (&["profiles", "test", "x"], "test takes no NAME"),
    ] {
        let out = diskgc(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(text(&out.stderr).contains(needle), "{args:?}");
        let with_json: Vec<&str> = args.iter().copied().chain(["--json"]).collect();
        let out = diskgc(&with_json);
        assert_eq!(out.status.code(), Some(2), "{with_json:?}");
        let error = error_of(&out);
        assert!(error.contains(needle), "{with_json:?}: {error}");
    }
}

#[cfg(not(feature = "proposed-release-01-catalog"))]
#[test]
fn profiles_test_exits_one_on_failing_examples_in_human_and_json_modes() {
    let tmp = TempDir::new("profile-test");
    let path = tmp.0.join("config.yaml");
    std::fs::write(&path, "version: 1\nprofiles:\n  - name: scratch\n    find: {roots: [/tmp], paths: ['scratch-*'], type: dir}\n    action: delete\n    examples: {match: [/tmp/other], no_match: [/tmp/scratch-oops]}\n").expect("write config");
    let config = path.to_str().expect("UTF-8");
    let out = diskgc(&["profiles", "test", "--config", config]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let human = text(&out.stdout);
    assert!(
        human.contains("scratch: match example /tmp/other was not found"),
        "{human}"
    );
    assert!(
        human.contains("scratch: no_match example /tmp/scratch-oops was found"),
        "{human}"
    );
    let out = diskgc(&["profiles", "test", "--config", config, "--json"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let data = json(&out);
    assert_eq!(data["status"], "failed");
    assert_eq!(data["exit_code"], 1);
    assert_eq!(data["failures"].as_array().unwrap().len(), 2);
}

#[cfg(not(feature = "proposed-release-01-catalog"))]
#[test]
fn profiles_test_passes_and_an_override_is_visible() {
    let tmp = TempDir::new("profile-override");
    let path = tmp.0.join("config.yaml");
    std::fs::write(&path, "version: 1\nprofiles:\n  - name: idle-1d\n    override: true\n    where: {idle_for: 2d}\n  - name: scratch\n    find: {roots: [/tmp], paths: ['scratch-*'], type: dir}\n    action: delete\n    examples: {match: [/tmp/scratch-ok], no_match: [/tmp/other]}\n").expect("write config");
    let config = path.to_str().expect("UTF-8");
    let out = diskgc(&["profiles", "test", "--config", config, "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let data = json(&out);
    assert_eq!(data["status"], "passed");
    assert_eq!(data["failures"].as_array().unwrap().len(), 0);
    let out = diskgc(&["profiles", "show", "idle-1d", "--config", config, "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let profile = &json(&out)["profile"];
    assert_eq!(profile["origin"], "user_override");
    assert_eq!(profile["override"], true);
    let out = diskgc(&["profiles", "show", "idle-1d", "--config", config]);
    assert_eq!(out.status.code(), Some(0));
    assert!(text(&out.stdout).contains("  override: true\n"));
    let out = diskgc(&["profiles", "list", "--config", config]);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        text(&out.stdout)
            .lines()
            .any(|line| line.starts_with("idle-1d ") && line.contains("user_override"))
    );
}
