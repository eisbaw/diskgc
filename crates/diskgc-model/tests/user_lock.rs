//! TASK-277 (docs/safety.md, Host lock): a non-root run on a host without
//! the provisioned host lock `/run/diskgc/lock` reclaims under a per-user
//! lock it makes itself (`/run/user/<uid>/diskgc/lock` when that runtime
//! directory is the user's, 0700; otherwise `<home>/.local/state/diskgc/
//! lock`); where that lock cannot be made for a reason of the environment
//! (no place, `ENOSPC`, ...) the reclaim runs without a lock and warns; a
//! failure of the host is exit 3; root still needs the host lock; a
//! provisioned host lock is always preferred; a per-user lock directory or
//! file that is a symlink, another user's, or open to others is refused.
//! Asserted on the trace and the model's filesystem, not on the report
//! alone.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::host;
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::hostlock::TMPFILES_LINES;
use diskgc_core::profile::Library;
use diskgc_core::report::{ErrorCause, Report, Status};
use diskgc_core::units::parse_size;
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;

const SRC: &str = "/home/mark/src";
const OLD: &str = "/home/mark/src/old/target";
const HOST_LOCK: &str = "/run/diskgc/lock";
const STATE: &str = "/home/mark/.local/state/diskgc";
const STATE_LOCK: &str = "/home/mark/.local/state/diskgc/lock";
const RUNTIME: &str = "/run/user/1000";
const RUNTIME_LOCK: &str = "/run/user/1000/diskgc/lock";

/// A classic profile directory whose every time is `at`, with a `size`
/// file in `deps/`.
fn profile(size: &str, at: &str) -> String {
    format!(
        "{{.: {{mtime: {at}}}, .cargo-lock: {{mtime: {at}}}, \
         .fingerprint: {{.: {{mtime: {at}}}, p-1: {{.: {{mtime: {at}}}, bin-p: {{mtime: {at}}}}}}}, \
         deps: {{.: {{mtime: {at}}}, p-1: {{size: {size}, mtime: {at}}}}}}}"
    )
}

/// `/home` (100G, 80G used by others) holds one idle 8G Cargo target
/// below `SRC`, so 8G is available; `/run` is a tmpfs holding the
/// provisioned host lock when `host_lock`; the state directory is
/// `STATE` (absent until made), and the home of the per-user lock is
/// `/home/mark`. `head`: top-level keys before the rest (`own_uid`);
/// `tree`: more tree entries (a `/run/user/1000`).
fn yaml(host_lock: bool, head: &str, tree: &str) -> String {
    let lock = if host_lock {
        format!("  {HOST_LOCK}: {{uid: 0, gid: 0}}\n")
    } else {
        String::new()
    };
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
{head}state_dir: {STATE}
home: /home/mark
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 2, at: /home, fs: ext4, dev: "254:2", size: 100G, other_used: 80G, parent: 1}}
  - {{id: 3, at: /run, fs: tmpfs, dev: "0:30", size: 1G, parent: 1}}
tree:
{lock}  /home/mark: {{.: {{mode: "0700"}}}}
  /home/mark/src/old/Cargo.toml: {{}}
  /home/mark/src/old/target:
    CACHEDIR.TAG: {{cachedir_tag: true}}
    debug: {old}
{tree}"#,
        old = profile("8G", "2026-10-01T00:00:00Z"),
    )
}

/// No top-level key is needed for the runtime directory: whether
/// `/run/user/1000` is the user's (0700) decides (`runtime_tree`).
fn runtime_head() -> String {
    String::new()
}

/// The user's runtime directory as logind makes it: `/run/user/1000`,
/// 0700, holding `inside`.
fn runtime_tree(inside: &str) -> String {
    format!("  {RUNTIME}: {{.: {{mode: \"0700\"}}{inside}}}\n")
}

/// `ensure --min-free MIN --select profile:cargo-target --roots SRC SRC`.
fn cargo(min: &str) -> Request {
    let library = Library::builtin().unwrap_or_else(|e| panic!("{e}"));
    Request {
        path: PathBuf::from(SRC),
        min_free: parse_size(min).unwrap_or_else(|e| panic!("{e}")),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier: Tier::select(
            "profile:cargo-target",
            &library,
            Some(&[SRC.to_owned()]),
            1000,
        )
        .unwrap_or_else(|e| panic!("{e}")),
    }
}

/// `ensure` as the user (`allow_root` false) or as root with `--allow-root`
/// and `owners: [1000]`, with its log lines.
fn ensure(host: &ModelHost, request: &Request, root: bool) -> (Report, Vec<String>) {
    let uid = executor::privileges(host, root).unwrap_or_else(|e| panic!("{e}"));
    let owners = if root { vec![1000] } else { vec![uid] };
    let mut lines = Vec::new();
    let report = executor::ensure(host, request, &owners, &mut |l| lines.push(l.to_owned()));
    eprintln!("log:\n  {}", lines.join("\n  "));
    (report, lines)
}

fn dry_run(host: &ModelHost, request: &Request, root: bool) -> (Report, Vec<String>) {
    let uid = executor::privileges(host, root).unwrap_or_else(|e| panic!("{e}"));
    let owners = if root { vec![1000] } else { vec![uid] };
    let mut lines = Vec::new();
    let report = executor::dry_run(host, request, &owners, &mut |l| lines.push(l.to_owned()));
    eprintln!("log:\n  {}", lines.join("\n  "));
    (report, lines)
}

/// The reclaim locks (not a candidate's) `flock`ed and acquired, in order.
fn locked(host: &ModelHost) -> Vec<PathBuf> {
    host.trace()
        .into_iter()
        .filter(|c| c.op == Op::Flock && c.result == "acquired")
        .filter_map(|c| c.path)
        .filter(|p| !p.starts_with(SRC))
        .collect()
}

fn unlinks(host: &ModelHost) -> usize {
    host.trace_count(Op::Unlinkat)
}

fn exists(host: &ModelHost, path: &str) -> bool {
    host.stat_path(Path::new(path)).is_some()
}

/// The owner and permission bits of `path`.
fn owner_mode(host: &ModelHost, path: &str) -> (u32, u32) {
    let stat = host
        .stat_path(Path::new(path))
        .unwrap_or_else(|| panic!("{path} exists"));
    (
        stat.uid.expect("uid modelled"),
        stat.perm.expect("mode modelled"),
    )
}

/// The one log line that names the per-user lock taken.
fn per_user_line<'a>(lines: &'a [String], path: &str) -> &'a str {
    let named: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains(&format!("per-user lock {path}")))
        .collect();
    assert_eq!(named.len(), 1, "one line names the lock: {lines:#?}");
    named[0]
}

/// The headline: Mark's laptop (TASK-277). No `/run/diskgc`, no
/// `/run/user/1000`: the reclaim makes `lock` in `~/.local/state/diskgc`
/// (directory 0700, file 0600, the user's), takes it, deletes, and says in
/// one line which lock it took and that other users' reclaims are not
/// serialised with it.
#[test]
fn a_non_root_run_without_the_host_lock_reclaims_under_a_per_user_lock() {
    let host = host(&yaml(false, "", ""));
    let (report, lines) = ensure(&host, &cargo("12G"), false);
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(report.exit_code(), 0);
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    assert_eq!(report.actions.len(), 1);
    assert_eq!(report.actions[0].candidate, Path::new(OLD));
    assert!(unlinks(&host) > 0);
    assert_eq!(
        locked(&host).first().map(PathBuf::as_path),
        Some(Path::new(STATE_LOCK))
    );
    assert!(
        locked(&host).iter().all(|p| !p.starts_with("/run/diskgc")),
        "{:?}",
        locked(&host)
    );
    assert_eq!(owner_mode(&host, STATE), (1000, 0o700));
    assert_eq!(owner_mode(&host, STATE_LOCK), (1000, 0o600));
    let line = per_user_line(&lines, STATE_LOCK);
    assert!(line.contains("not provisioned"), "{line}");
    assert!(
        line.contains("reclaims by other users are not serialised with this one"),
        "{line}"
    );
    assert!(!line.contains("sudo"), "{line}");
}

/// With the user's runtime directory the per-user lock lives there (a
/// tmpfs: making it costs the full filesystem nothing), and nothing is made
/// in the home for it.
#[test]
fn the_per_user_lock_is_in_the_runtime_directory_when_there_is_one() {
    let host = host(&yaml(false, &runtime_head(), &runtime_tree("")));
    let (report, lines) = ensure(&host, &cargo("12G"), false);
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(
        locked(&host).first().map(PathBuf::as_path),
        Some(Path::new(RUNTIME_LOCK))
    );
    assert_eq!(owner_mode(&host, "/run/user/1000/diskgc"), (1000, 0o700));
    assert_eq!(owner_mode(&host, RUNTIME_LOCK), (1000, 0o600));
    assert!(
        !exists(&host, STATE_LOCK),
        "no lock made in the state directory"
    );
    per_user_line(&lines, RUNTIME_LOCK);
}

/// Root (`--allow-root`) never falls back: without the host lock it exits
/// 2 (`provisioning`) with the tmpfiles commands, as before TASK-277, and
/// makes no per-user lock anywhere.
#[test]
fn root_without_the_host_lock_still_exits_2() {
    let head = format!("own_uid: 0\n{}", runtime_head());
    let host = host(&yaml(false, &head, &runtime_tree("")));
    let (report, _) = ensure(&host, &cargo("12G"), true);
    assert_eq!(report.status, Status::UsageError, "{report:#?}");
    assert_eq!(report.exit_code(), 2);
    assert_eq!(report.errors.len(), 1, "{:#?}", report.errors);
    assert_eq!(report.errors[0].cause, ErrorCause::Provisioning);
    let message = &report.errors[0].message;
    assert!(
        message.contains("host lock /run/diskgc/lock is missing"),
        "{message}"
    );
    for line in TMPFILES_LINES {
        assert!(message.contains(line), "{message}");
    }
    assert!(locked(&host).is_empty(), "{:?}", locked(&host));
    assert_eq!(unlinks(&host), 0);
    assert!(!exists(&host, "/run/user/1000/diskgc"));
    assert!(!exists(&host, STATE));
}

/// A provisioned host lock wins over a per-user lock that could be made:
/// reclaims of every user keep taking turns through it.
#[test]
fn a_provisioned_host_lock_is_preferred() {
    let host = host(&yaml(true, &runtime_head(), &runtime_tree("")));
    let (report, lines) = ensure(&host, &cargo("12G"), false);
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(
        locked(&host).first().map(PathBuf::as_path),
        Some(Path::new(HOST_LOCK))
    );
    assert!(!exists(&host, "/run/user/1000/diskgc"));
    assert!(!exists(&host, STATE_LOCK));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("host lock /run/diskgc/lock taken")),
        "{lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("per-user lock")),
        "{lines:#?}"
    );
}

/// What stands at the per-user lock's directory or file and is refused:
/// exit 2 (`provisioning`), the message names the per-user lock and why,
/// nothing locked or deleted.
#[test]
fn a_symlinked_or_foreign_per_user_lock_directory_is_refused() {
    let cases = [
        (
            "symlinked directory",
            runtime_tree(
                ", elsewhere: {.: {mode: \"0700\"}}, diskgc: {symlink: /run/user/1000/elsewhere}",
            ),
            "symlink",
        ),
        (
            "another user's directory",
            runtime_tree(", diskgc: {.: {uid: 1001, mode: \"0700\"}}"),
            "owned by uid 1000",
        ),
        (
            "a directory others may write to",
            runtime_tree(", diskgc: {.: {mode: \"0777\"}}"),
            "writable by no one else",
        ),
        (
            "not a directory",
            runtime_tree(", diskgc: {}"),
            "not a directory",
        ),
        (
            "symlinked lock file",
            runtime_tree(
                ", diskgc: {.: {mode: \"0700\"}, lock: {symlink: /run/user/1000/x}}, x: {mode: \"0600\"}",
            ),
            "symlink",
        ),
        (
            "another user's lock file",
            runtime_tree(", diskgc: {.: {mode: \"0700\"}, lock: {uid: 1001, mode: \"0600\"}}"),
            "a regular file of uid 1000",
        ),
        (
            "a lock file others may open",
            runtime_tree(", diskgc: {.: {mode: \"0700\"}, lock: {mode: \"0644\"}}"),
            "no one else may open",
        ),
        (
            "a FIFO",
            runtime_tree(", diskgc: {.: {mode: \"0700\"}, lock: {type: fifo, mode: \"0600\"}}"),
            "a regular file of uid 1000",
        ),
    ];
    for (case, tree, why) in cases {
        let host = host(&yaml(false, &runtime_head(), &tree));
        let (report, _) = ensure(&host, &cargo("12G"), false);
        assert_eq!(report.status, Status::UsageError, "{case}: {report:#?}");
        assert_eq!(report.exit_code(), 2, "{case}");
        assert_eq!(report.errors.len(), 1, "{case}: {:#?}", report.errors);
        assert_eq!(report.errors[0].cause, ErrorCause::Provisioning, "{case}");
        let message = &report.errors[0].message;
        assert!(message.contains("per-user lock"), "{case}: {message}");
        assert!(message.contains(why), "{case}: {message}");
        assert!(locked(&host).is_empty(), "{case}: {:?}", locked(&host));
        assert_eq!(unlinks(&host), 0, "{case}");
    }
}

/// A `/run/user/1000` that is not the user's runtime directory as logind
/// makes it (another mode, another owner, a symlink) is not used: the
/// lock is in the home. Nothing of the environment is read.
#[test]
fn a_runtime_directory_that_is_not_the_users_is_not_used() {
    for (case, tree) in [
        (
            "mode 0755",
            format!("  {RUNTIME}: {{.: {{mode: \"0755\"}}}}\n"),
        ),
        (
            "another owner",
            format!("  {RUNTIME}: {{.: {{uid: 1001, mode: \"0700\"}}}}\n"),
        ),
        (
            "a symlink",
            format!(
                "  /run/elsewhere: {{.: {{mode: \"0700\"}}}}\n  {RUNTIME}: {{symlink: /run/elsewhere}}\n"
            ),
        ),
    ] {
        let host = host(&yaml(false, "", &tree));
        let (report, lines) = ensure(&host, &cargo("12G"), false);
        assert_eq!(report.status, Status::Met, "{case}: {report:#?}");
        assert_eq!(
            locked(&host).first().map(PathBuf::as_path),
            Some(Path::new(STATE_LOCK)),
            "{case}"
        );
        per_user_line(&lines, STATE_LOCK);
    }
}

/// No runtime directory and no home: no place for a per-user lock. That is
/// the environment's, not an unsafe object: the reclaim runs without a
/// lock (TASK-277 review: a user must still be able to reclaim), with one
/// warning, and reaches the goal.
#[test]
fn without_a_place_for_a_per_user_lock_the_reclaim_runs_unlocked_and_warns() {
    let model = yaml(false, "", "").replace("home: /home/mark\n", "");
    let host = host(&model);
    let (report, lines) = ensure(&host, &cargo("12G"), false);
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert_eq!(report.exit_code(), 0);
    assert!(report.errors.is_empty(), "{:#?}", report.errors);
    assert!(unlinks(&host) > 0);
    assert!(locked(&host).is_empty(), "{:?}", locked(&host));
    let warned: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("runs without a lock"))
        .collect();
    assert_eq!(warned.len(), 1, "{lines:#?}");
    assert!(warned[0].starts_with("warning:"), "{}", warned[0]);
    assert!(
        warned[0].contains("no place for a per-user lock"),
        "{}",
        warned[0]
    );
    assert!(!warned[0].contains("sudo"), "{}", warned[0]);
}

/// A per-user lock that cannot be made for a reason of the environment
/// (`ENOSPC`: Mark's full disk): the reclaim runs without a lock, the
/// warning names the path and the reason. A failure of the host (`EIO`)
/// is exit 3, cause `host`, nothing deleted (TASK-277 review, Codex 2).
#[test]
fn a_per_user_lock_that_cannot_be_made_runs_unlocked_and_a_host_failure_is_exit_3() {
    let fail = |errno: &str| {
        host(&format!(
            "{}events:\n  - {{at: {{op: user_lock, nth: 1, when: before}}, fail: {errno}}}\n",
            yaml(false, "", "")
        ))
    };
    for errno in ["ENOSPC", "EDQUOT", "EROFS", "EACCES"] {
        let host = fail(errno);
        let (report, lines) = ensure(&host, &cargo("12G"), false);
        assert_eq!(report.status, Status::Met, "{errno}: {report:#?}");
        assert!(unlinks(&host) > 0, "{errno}");
        assert!(locked(&host).is_empty(), "{errno}");
        let warned: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("runs without a lock"))
            .collect();
        assert_eq!(warned.len(), 1, "{errno}: {lines:#?}");
        assert!(warned[0].contains(STATE_LOCK), "{}", warned[0]);
        assert!(warned[0].contains(errno), "{}", warned[0]);
    }
    let host = fail("EIO");
    let (report, _) = ensure(&host, &cargo("12G"), false);
    assert_eq!(report.status, Status::Error, "{report:#?}");
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.errors.len(), 1, "{:#?}", report.errors);
    assert_eq!(report.errors[0].cause, ErrorCause::Host);
    assert!(
        report.errors[0].message.contains(STATE_LOCK),
        "{:#?}",
        report.errors
    );
    assert_eq!(unlinks(&host), 0);
}

/// An `ensure` that finds the space already there stays an instant no-op:
/// it makes no per-user lock (a state write) and, for a user, does not
/// warn that the host lock is missing.
#[test]
fn an_already_met_ensure_makes_no_per_user_lock_and_does_not_warn() {
    let host = host(&yaml(false, &runtime_head(), &runtime_tree("")));
    let (report, lines) = ensure(&host, &cargo("1G"), false);
    assert_eq!(report.status, Status::Met, "{report:#?}");
    assert!(locked(&host).is_empty());
    assert!(!exists(&host, "/run/user/1000/diskgc"));
    assert!(!exists(&host, STATE));
    assert!(!lines.iter().any(|l| l.contains("warning")), "{lines:#?}");
}

/// A dry run takes no lock and makes none. As a user without the host
/// lock it says that reclaims fall back to a per-user lock, and does not
/// tell the user to run `sudo`; as root it still warns with the tmpfiles
/// commands.
#[test]
fn a_dry_run_says_a_user_falls_back_and_tells_root_to_provision() {
    let user = host(&yaml(false, &runtime_head(), &runtime_tree("")));
    let (report, lines) = dry_run(&user, &cargo("12G"), false);
    assert_eq!(report.status, Status::DryRun, "{report:#?}");
    assert!(locked(&user).is_empty());
    assert!(!exists(&user, "/run/user/1000/diskgc"));
    let said: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("/run/diskgc/lock"))
        .collect();
    assert_eq!(said.len(), 1, "{lines:#?}");
    assert!(said[0].contains("not provisioned"), "{}", said[0]);
    assert!(
        said[0].contains(&format!("per-user lock {RUNTIME_LOCK}")),
        "{}",
        said[0]
    );
    assert!(!said[0].contains("sudo"), "{}", said[0]);
    assert!(!lines.iter().any(|l| l.contains("sudo")), "{lines:#?}");

    let root = host(&yaml(
        false,
        &format!("own_uid: 0\n{}", runtime_head()),
        &runtime_tree(""),
    ));
    let (report, lines) = dry_run(&root, &cargo("12G"), true);
    assert_eq!(report.status, Status::DryRun, "{report:#?}");
    let output = lines.join("\n");
    assert!(
        output.contains("warning: host lock is not ready"),
        "{output}"
    );
    assert!(output.contains(TMPFILES_LINES[1]), "{output}");
    assert!(locked(&root).is_empty());
}

/// Two reclaims of one user serialise on the per-user lock: another
/// process holding it until the deadline is a lock timeout (75), nothing
/// deleted, as with the host lock.
#[test]
fn another_holder_of_the_per_user_lock_makes_the_reclaim_wait() {
    let tree = runtime_tree(", diskgc: {.: {mode: \"0700\"}, lock: {mode: \"0600\"}}")
        + "  /usr/bin/diskgc: {}\nprocesses:\n  - {pid: 4242, exe: /usr/bin/diskgc, cwd: /}\n"
        + &format!("locks:\n  - {{path: {RUNTIME_LOCK}, holder: 4242, mode: exclusive}}\n");
    let host = host(&yaml(false, &runtime_head(), &tree));
    let mut request = cargo("12G");
    request.timeout = Some(Duration::from_secs(5));
    let (report, lines) = ensure(&host, &request, false);
    assert_eq!(report.exit_code(), 75, "{report:#?}");
    assert_eq!(unlinks(&host), 0);
    assert!(
        lines
            .iter()
            .any(|l| l.contains(&format!("per-user lock {RUNTIME_LOCK} was still held"))),
        "{lines:#?}"
    );
}
