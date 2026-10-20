//! Contract of the deleting command port on the virtual host. The real
//! kernel's child lifetime and lock inheritance are covered by container
//! tests; the model proves the gate and deadline are visible to core code.

mod common;

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use diskgc_core::host::{Cause, CommandExit, CommandInvocation, CommandRun, Delete, Host};
use diskgc_model::spec::Op;

fn request(host: &impl Host) -> CommandRun {
    CommandRun {
        docker_info: None,
        executable: PathBuf::from("/bin/true"),
        arguments: vec![OsString::from("--one"), OsString::from("/tmp/a")],
        environment: vec![(OsString::from("DISKGC_PATH"), OsString::from("/tmp/a"))],
        output_prefix: "tier/test-profile".to_owned(),
        deadline: host.now().checked_add(Duration::from_secs(1)).unwrap(),
    }
}

#[test]
fn command_port_traces_checked_cwd_and_exit() {
    let host = common::host(&common::tmp_yaml(""));
    let cwd = common::dir(&host, "/tmp/a");
    let run = request(&host);
    assert!(matches!(
        host.run_command(&cwd.fd, &run),
        CommandInvocation::Started(Ok(CommandExit::Exited(0)))
    ));
    let call = host.trace().into_iter().last().unwrap();
    assert_eq!(call.op, Op::RunCommand);
    assert_eq!(call.path, Some(PathBuf::from("/tmp/a")));
    assert_eq!(call.result, "Exited(0)");
    assert_eq!(call.command, Some(run));
    assert_eq!(
        call.object,
        host.stat_path(std::path::Path::new("/tmp/a"))
            .unwrap()
            .dev_ino()
    );
}

#[test]
fn command_port_preserves_a_nonzero_exit() {
    let host = common::host(&common::tmp_yaml("command_exit: 17"));
    let cwd = common::dir(&host, "/tmp/a");
    assert!(matches!(
        host.run_command(&cwd.fd, &request(&host)),
        CommandInvocation::Started(Ok(CommandExit::Exited(17)))
    ));
    assert_eq!(host.trace().last().unwrap().result, "Exited(17)");
}

#[test]
fn command_port_refuses_closed_gate_before_execution() {
    let host = common::host(&common::tmp_yaml("sandbox_gate: closed"));
    let cwd = common::dir(&host, "/tmp/a");
    let CommandInvocation::NotStarted(error) = host.run_command(&cwd.fd, &request(&host)) else {
        panic!("closed gate must prevent spawn");
    };
    assert_eq!(error.cause, Cause::SandboxGate);
    assert_eq!(error.kernel_errno(), None);
    assert_eq!(host.trace().last().unwrap().result, "refused");
}

#[test]
fn command_port_refuses_root_even_when_the_gate_is_open() {
    let host = common::host(&common::tmp_yaml("own_uid: 0"));
    let cwd = common::dir(&host, "/tmp/a");
    let CommandInvocation::NotStarted(error) = host.run_command(&cwd.fd, &request(&host)) else {
        panic!("root must prevent spawn");
    };
    assert!(error.detail.contains("runs as root"), "{error}");
    assert_eq!(host.trace().last().unwrap().result, "root refused");
}

#[test]
fn expired_command_deadline_starts_no_model_child() {
    let host = common::host(&common::tmp_yaml(""));
    let cwd = common::dir(&host, "/tmp/a");
    let mut run = request(&host);
    run.deadline = host.now();
    assert!(matches!(
        host.run_command(&cwd.fd, &run),
        CommandInvocation::DeadlineBeforeStart
    ));
    assert_eq!(host.trace().last().unwrap().result, "DeadlineBeforeStart");
}

#[test]
fn output_failure_after_spawn_remains_an_attempted_command() {
    let host = common::host(&common::tmp_yaml("command_error_after_start: true"));
    let cwd = common::dir(&host, "/tmp/a");
    let result = host.run_command(&cwd.fd, &request(&host));
    let CommandInvocation::Started(Err(error)) = result else {
        panic!("output failure should retain started state: {result:?}");
    };
    assert!(error.detail.contains("after spawn"), "{error}");
    assert_eq!(host.trace().last().unwrap().result, "started: output error");
}
