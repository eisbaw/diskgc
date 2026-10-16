//! Behavioural tests of the built `diskgc` binary (scaffold surface only).

use std::process::Command;

fn diskgc(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_diskgc"))
        .args(args)
        .output()
        .expect("failed to spawn the diskgc binary")
}

#[test]
fn version_prints_name_and_package_version() {
    let out = diskgc(&["--version"]);
    assert!(out.status.success(), "status: {:?}", out.status);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("diskgc {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn no_arguments_is_a_usage_error_with_exit_2() {
    let out = diskgc(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage: diskgc"));
}
