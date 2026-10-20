//! Actual-binary stdout failure checks, confined to the disposable test
//! container. The probe runs as tester under strace and starts `diskgc`
//! with a closed pipe or `/dev/full` as stdout.

use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::oracle::{Fixture, Oracle};
use crate::suite::{Ctx, TestResult};
use crate::{ensure, exit};

const DRY_RUN: &[&str] = &[
    "ensure",
    "--dry-run",
    "--json",
    "--min-free",
    "1G",
    "--select",
    "profile:go-mod-cache",
    "--roots",
    "/home/tester",
    "/home/tester",
];

/// Verifies the normal invocation reaches `finish()` before injecting its
/// stdout errors. The selected profile sees only the container's tmpfs home;
/// dry-run cannot delete it. The oracle still checks all three traced runs.
pub fn ensure_stdout_failures_exit_three_without_panicking(ctx: &Ctx) -> TestResult {
    let binary = ctx.diskgc();
    let binary_text = binary.to_str().ok_or("non-UTF-8 diskgc binary path")?;
    let oracle = Oracle::arm(Fixture::new())?;

    let baseline = ctx.run_as_tester(&binary, DRY_RUN)?;
    baseline.expect_code(0)?;
    let report: serde_json::Value = serde_json::from_str(&baseline.stdout)
        .map_err(|error| format!("baseline did not return JSON: {error}: {}", baseline.stdout))?;
    ensure!(
        report["status"] == "dry_run",
        "baseline did not reach ensure dry-run report: {report}"
    );

    let mut runs = Vec::new();
    for (mode, error_name) in [
        ("closed-pipe", "Broken pipe"),
        ("full-device", "No space left on device"),
    ] {
        let mut args = vec!["probe", "stdout-error", mode, binary_text];
        args.extend_from_slice(DRY_RUN);
        let run = ctx.run_as_tester(&ctx.self_exe, &args)?;
        run.expect_code(3)?;
        ensure!(
            run.stdout.is_empty(),
            "{mode} leaked stdout: {}",
            run.stdout
        );
        let diagnostics: Vec<&str> = run
            .stderr
            .lines()
            .filter(|line| line.starts_with("diskgc: error: could not write result to stdout:"))
            .collect();
        ensure!(
            diagnostics.len() == 1 && diagnostics[0].contains(error_name),
            "{mode} did not report its write error once: {}",
            run.stderr
        );
        ensure!(
            !run.stderr.contains("panicked at")
                && !run.stderr.contains("failed printing to stdout"),
            "{mode} panicked: {}",
            run.stderr
        );
        runs.push(run);
    }

    oracle
        .judge_runs(&[&baseline, &runs[0], &runs[1]])?
        .assert_clean()
}

/// Probe entry point, reached only after the test-container interlock. The
/// parent retains stderr so the CLI diagnostic appears in the traced run.
/// Any setup failure has exit 1, distinct from the CLI's expected exit 3.
pub fn redirected_stdout(mode: &str, binary: &Path, args: &[String]) -> i32 {
    let stdout = match mode {
        "closed-pipe" => {
            let (read_end, write_end) =
                match rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC) {
                    Ok(pipe) => pipe,
                    Err(error) => {
                        eprintln!("stdout-error probe: pipe: {error}");
                        return exit::FAIL;
                    }
                };
            drop(read_end);
            Stdio::from(write_end)
        }
        "full-device" => match OpenOptions::new().write(true).open("/dev/full") {
            Ok(file) => Stdio::from(file),
            Err(error) => {
                eprintln!("stdout-error probe: open /dev/full: {error}");
                return exit::FAIL;
            }
        },
        other => {
            eprintln!("stdout-error probe: unknown mode {other}");
            return exit::USAGE;
        }
    };
    match Command::new(binary).args(args).stdout(stdout).status() {
        Ok(status) => status.code().unwrap_or(exit::FAIL),
        Err(error) => {
            eprintln!("stdout-error probe: spawn {}: {error}", binary.display());
            exit::FAIL
        }
    }
}
