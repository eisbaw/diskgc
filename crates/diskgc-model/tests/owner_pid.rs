//! The model's Owner PID facts as the `Host` port answers them, and the
//! in-use scan's PID table built from them (TASK-112 step 2). These pin
//! the model to what `LinuxHost` does: zombies are listed, gone for every
//! reader but `status` and the pidfd probe; an unreadable `NSpid` is read
//! again and then recorded; a process gone before its `status` read drops
//! out.

mod common;

use common::{dir, host, tmp_yaml};
use diskgc_core::host::{Host, Pid, PidNamespace, ProcError};
use diskgc_core::inuse::{self, Options};
use diskgc_core::liveness::{self, LOCAL_FILESYSTEMS, OwnerPid};

fn scan_with_pids(host: &diskgc_model::ModelHost) -> inuse::Scan {
    inuse::scan(
        host,
        &Options {
            users: &[1000],
            sockets: false,
            pid_lookup: true,
            pytest_facts: false,
        },
        None,
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn the_port_answers_the_modelled_facts() {
    let model = host(&tmp_yaml(
        "pid_namespace: nested\nproc_hidepid: true\nprocesses:\n  - {pid: 7, exe: /tmp/a/f, maps: [/tmp/a/f], environ: [A=1], fds: [{path: /tmp/a/f}], nspid: [7, 1]}\n  - {pid: 8, zombie: true}\n  - {pid: 9, nspid: unreadable, exe: /tmp/a/f, maps: [/tmp/a/f], environ: [A=1], fds: [{path: /tmp/a/f}]}\n",
    ));
    assert_eq!(
        model.pid_namespace().expect("facts"),
        PidNamespace {
            initial: false,
            hidepid: true
        }
    );
    assert_eq!(model.proc_nspid(Pid(7)), Ok(vec![7, 1]));
    assert_eq!(model.proc_nspid(Pid(8)), Ok(vec![8]));
    assert!(matches!(
        model.proc_nspid(Pid(9)),
        Err(ProcError::Failed(_))
    ));
    assert_eq!(model.proc_nspid(Pid(10)), Err(ProcError::Gone));
    // A zombie occupies its pid: listed and alive for the probe, gone for
    // the readers that need a live task.
    assert!(model.pids().expect("pids").contains(&Pid(8)));
    assert_eq!(model.pid_alive(Pid(8)), Ok(true));
    let identity = model.proc_identity(Pid(8)).expect("a zombie's pidfd opens");
    assert_eq!(model.proc_identity_released(&identity), Ok(false));
    assert_eq!(model.proc_fds(Pid(8)), Err(ProcError::Gone));
    assert_eq!(model.pid_alive(Pid(10)), Ok(false));
    let tmp = dir(&model, "/tmp");
    assert_eq!(model.fs_magic(&tmp.fd), Ok(0x0102_1994));
}

/// The model's filesystem magics agree with the core's local list, so a
/// vector's `fs:` name means in the model what `fstatfs` means on Linux.
#[test]
fn model_magics_match_the_local_filesystem_list() {
    for (magic, name) in LOCAL_FILESYSTEMS {
        assert_eq!(diskgc_model::state::fs_magic(name), *magic, "{name}");
    }
    for name in [
        "overlay",
        "virtiofs",
        "9p",
        "zfs",
        "fuse",
        "nfs",
        "unheard-of",
    ] {
        assert_eq!(
            liveness::local_filesystem(diskgc_model::state::fs_magic(name)),
            None,
            "{name}"
        );
    }
}

#[test]
fn the_scan_tables_every_level_and_drops_gone_processes() {
    let model = host(&tmp_yaml(
        "processes:\n  - {pid: 3025375, exe: /tmp/a/f, maps: [/tmp/a/f], environ: [A=1], fds: [{path: /tmp/a/f}], nspid: [3025375, 9]}\n  - {pid: 4242, zombie: true}\nevents:\n  - {at: {op: proc_nspid, path: /proc/4242/status, when: before}, exit: 4242}\n",
    ));
    let scan = scan_with_pids(&model);
    let table = scan.pid_table().expect("asked for");
    assert_eq!(table.carrier(9), Some(Pid(3_025_375)));
    assert_eq!(table.carrier(3_025_375), Some(Pid(3_025_375)));
    // The zombie exited (was reaped) just before its status read: gone, so
    // neither carried nor unreadable.
    assert_eq!(table.carrier(4242), None);
    assert!(table.unreadable().is_empty());
}

#[test]
fn an_unreadable_nspid_is_reread_then_blocks_every_answer() {
    let model = host(&tmp_yaml(
        "processes:\n  - {pid: 77, uid: 2000, proc: denied, nspid: unreadable}\n",
    ));
    let scan = scan_with_pids(&model);
    let table = scan.pid_table().expect("asked for");
    assert_eq!(table.unreadable(), [Pid(77)]);
    let reads = model
        .trace()
        .iter()
        .filter(|call| {
            call.op == diskgc_model::spec::Op::ProcNspid
                && call.path.as_deref() == Some(std::path::Path::new("/proc/77/status"))
        })
        .count();
    assert_eq!(reads, 1 + inuse::REREAD_PAUSES.len());
    let tmp = dir(&model, "/tmp");
    let answer = liveness::owner_pid(
        table,
        model.fs_magic(&tmp.fd).map_err(|e| e.to_string()),
        4242,
        |_| panic!("not probed"),
    )
    .expect("decided");
    let OwnerPid::Unknown(unknown) = answer else {
        panic!("{answer:?}");
    };
    assert_eq!(unknown.blockers, [Pid(77)]);
}

#[test]
fn without_the_option_no_status_is_read() {
    let model = host(&tmp_yaml(
        "processes:\n  - {pid: 7, exe: /tmp/a/f, maps: [/tmp/a/f], environ: [A=1], fds: [{path: /tmp/a/f}]}\n",
    ));
    let scan = inuse::scan(
        &model,
        &Options {
            users: &[1000],
            sockets: false,
            pid_lookup: false,
            pytest_facts: false,
        },
        None,
    )
    .expect("scan");
    assert!(scan.pid_table().is_none());
    assert!(!model.trace().iter().any(|call| matches!(
        call.op,
        diskgc_model::spec::Op::ProcNspid | diskgc_model::spec::Op::PidNamespace
    )));
}

/// Many processes whose `NSpid` cannot be read are read again in rounds:
/// the scan pauses once per round for all of them, not once per process.
#[test]
fn rereads_are_rounds_not_per_process_pauses() {
    let processes: String = (100..150)
        .map(|pid| format!("  - {{pid: {pid}, uid: 2000, proc: denied, nspid: unreadable}}\n"))
        .collect();
    let model = host(&tmp_yaml(&format!("processes:\n{processes}")));
    let scan = scan_with_pids(&model);
    assert_eq!(scan.pid_table().expect("asked for").unreadable().len(), 50);
    let trace = model.trace();
    let count = |op| trace.iter().filter(|call| call.op == op).count();
    assert_eq!(
        count(diskgc_model::spec::Op::Sleep),
        inuse::REREAD_PAUSES.len()
    );
    assert_eq!(
        count(diskgc_model::spec::Op::ProcNspid),
        50 * (1 + inuse::REREAD_PAUSES.len()) + 1
    );
}

/// When a namespace precondition already fails, no `NSpid` is read: every
/// answer is unknown whatever the table holds.
#[test]
fn no_nspid_is_read_when_a_precondition_fails() {
    for facts in ["pid_namespace: nested\n", "proc_hidepid: true\n"] {
        let model = host(&tmp_yaml(&format!(
            "{facts}processes:\n  - {{pid: 7, exe: /tmp/a/f, maps: [/tmp/a/f], environ: [A=1], fds: [{{path: /tmp/a/f}}]}}\n"
        )));
        let scan = scan_with_pids(&model);
        assert!(!scan.pid_table().expect("asked for").preconditions_hold());
        assert_eq!(
            model.trace_count(diskgc_model::spec::Op::ProcNspid),
            0,
            "{facts}"
        );
    }
}

/// A zombie that is not reaped keeps its number in the table: it is
/// listed and its `status` readable, though In use finds it gone.
#[test]
fn an_unreaped_zombie_stays_in_the_table() {
    let model = host(&tmp_yaml(
        "processes:\n  - {pid: 4242, zombie: true, nspid: [4242, 9]}\n",
    ));
    let scan = scan_with_pids(&model);
    let table = scan.pid_table().expect("asked for");
    assert_eq!(table.carrier(4242), Some(Pid(4242)));
    assert_eq!(table.carrier(9), Some(Pid(4242)));
}

/// The namespace facts are read before the listings and again after the
/// lookups (what a difference between the reads means is pinned by the
/// core's `namespace_facts_must_hold_over_the_whole_scan`).
#[test]
fn namespace_facts_are_read_before_and_after_the_scan() {
    let model = host(&tmp_yaml(
        "processes:\n  - {pid: 7, exe: /tmp/a/f, maps: [/tmp/a/f], environ: [A=1], fds: [{path: /tmp/a/f}]}\n",
    ));
    let scan = scan_with_pids(&model);
    let reads = model.trace_count(diskgc_model::spec::Op::PidNamespace);
    assert_eq!(reads, 2, "read before the listings and after the lookups");
    assert!(scan.pid_table().expect("asked for").preconditions_hold());
}
