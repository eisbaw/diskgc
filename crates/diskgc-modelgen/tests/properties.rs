//! Generated safety checks for the 0.1 filesystem deletion path. A failed
//! proptest prints the *shrunk* HostModel as YAML for regression replay.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use diskgc_core::cargo_target;
use diskgc_core::engine::{Candidate, DeleteOptions, End, Rules, delete_unchecked, walk};
use diskgc_core::executor::{self, Request, Tier};
use diskgc_core::executor::{ProtectedKind, ProtectedSource};
use diskgc_core::fs_kind::Find;
use diskgc_core::host::{Base, Delete, DevIno, Host, Unlink};
use diskgc_core::inuse::{Checker, REUSE_LIMIT};
use diskgc_core::outcome::ReclaimEnd;
use diskgc_core::profile::IdleWindow;
use diskgc_core::profile::{EntryType, Library, Where, parse_profiles};
use diskgc_core::report::{Final, Signal, Status, after_lock_timeout, conclude};
use diskgc_core::resolve::{Want, resolve};
use diskgc_core::units::parse_size;
use diskgc_model::spec::{DirSpec, FileSpec, HostModel, NodeSpec, Op, PidNamespaceSpec, Special};
use diskgc_model::{Call, ModelHost};
use diskgc_modelgen::{OWNERS, ROOT, host_model, host_model_with_kernel_holders};

/// The other user of generated models: a file of theirs is in no candidate.
const OWNERS_OTHER: u32 = OWNERS[1];
use proptest::prelude::*;
use proptest::test_runner::{TestError, TestRunner};

fn property_config() -> ProptestConfig {
    let cases = std::env::var("DISKGC_PROPERTY_CASES")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(32);
    ProptestConfig {
        cases,
        failure_persistence: None,
        // A seed is printed by proptest on failure; the YAML below is the
        // portable regression artifact after shrinking.
        ..ProptestConfig::default()
    }
}

fn loaded(model: &HostModel) -> Result<ModelHost, TestCaseError> {
    ModelHost::new(model).map_err(|error| {
        TestCaseError::fail(format!(
            "generated model failed to load: {error}\n{}",
            model.to_yaml()
        ))
    })
}

fn failure(model: &HostModel, message: impl std::fmt::Display) -> TestCaseError {
    TestCaseError::fail(format!(
        "{message}\nSave this shrunk model under crates/diskgc-model/tests/regressions/:\n{}",
        model.to_yaml()
    ))
}

const PROFILES: &str = r#"
- name: prop-a
  find: {roots: [/tmp], paths: ["a*"], type: dir}
  action: delete
  examples: {match: [/tmp/a0]}
- name: prop-b
  find: {roots: [/tmp], paths: ["*b*"], type: dir}
  action: delete
  examples: {match: [/tmp/b0]}
"#;

fn library() -> Library {
    let profiles = parse_profiles(PROFILES).expect("valid fixture profiles");
    Library::with_user_profiles(&profiles).expect("valid fixture library")
}

fn request(select: &str) -> Request {
    Request {
        path: PathBuf::from("/tmp"),
        min_free: parse_size("100%").expect("valid size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        watch: None,
        tier: Tier::select(select, &library(), None, 1000).expect("supported selection"),
    }
}

/// Four candidate names, including `ab0` which both sources find. The
/// random mask decides presence; a regular child makes deletion observable.
fn candidates(mask: u8) -> HostModel {
    let mut model = diskgc_model::load::parse_yaml(
        "host_model: 1\nclock: 2026-10-15T03:00:00Z\nmounts:\n  - {id: 1, at: /, fs: ext4, dev: '254:1', size: 100G}\n  - {id: 30, at: /tmp, fs: tmpfs, dev: '0:42', size: 1G, parent: 1}\n"
    ).expect("base model");
    model.tree.insert(
        PathBuf::from("/run/diskgc/lock"),
        NodeSpec::File(FileSpec {
            uid: Some(0),
            gid: Some(0),
            ..FileSpec::default()
        }),
    );
    for (bit, name) in ["a0", "b0", "ab0", "c0"].into_iter().enumerate() {
        if mask & (1 << bit) != 0 {
            let mut dir = DirSpec::default();
            dir.entries.insert(
                "data".to_owned(),
                NodeSpec::File(FileSpec {
                    size: Some(diskgc_model::spec::Bytes(4096)),
                    ..FileSpec::default()
                }),
            );
            model
                .tree
                .insert(PathBuf::from(format!("/tmp/{name}")), NodeSpec::Dir(dir));
        }
    }
    model
}

fn eligible(host: &ModelHost, select: &str) -> BTreeSet<PathBuf> {
    executor::dry_run(host, &request(select), &[1000], &mut |_| {})
        .plan
        .into_iter()
        .filter(|entry| entry.reason.is_none())
        .map(|entry| entry.candidate.expect("a candidate"))
        .collect()
}

fn protected_a(host: &ModelHost) -> BTreeSet<PathBuf> {
    let mut request = request("profile:prop-a");
    request.tier.protections.push(ProtectedSource {
        profile: "prop-b".to_owned(),
        roots: vec![PathBuf::from("/tmp")],
        kind: ProtectedKind::Fs(Box::new(Find::new(
            &["*b*".to_owned()],
            EntryType::Dir,
            &Where::default(),
            1000,
        ))),
    });
    executor::dry_run(host, &request, &[1000], &mut |_| {})
        .plan
        .into_iter()
        .filter(|entry| entry.reason.is_none())
        .map(|entry| entry.candidate.expect("a candidate"))
        .collect()
}

fn unexpected_unlink(trace: &[Call], checked: &BTreeSet<(PathBuf, DevIno)>) -> Option<String> {
    trace
        .iter()
        .find(|call| {
            call.op == Op::Unlinkat
                && call.result == "ok"
                && !call
                    .path
                    .as_ref()
                    .zip(call.object)
                    .is_some_and(|(path, object)| checked.contains(&(path.clone(), object)))
        })
        .map(ToString::to_string)
}

/// Independent of `WalkRecord`: the generated YAML tree is the authority
/// for which names exist, who owns them, and which entries must refuse a
/// whole-candidate walk. This oracle must not call the implementation's
/// path reconstruction or owner judgement.
#[derive(Default)]
struct SpecOracle {
    paths: BTreeSet<PathBuf>,
    symlinks: BTreeSet<PathBuf>,
    refuses_walk: bool,
}

fn collect_spec(path: &Path, node: &NodeSpec, oracle: &mut SpecOracle) {
    oracle.paths.insert(path.to_owned());
    let name = path
        .file_name()
        .and_then(|part| part.to_str())
        .unwrap_or("");
    if matches!(
        name,
        ".diskgc-keep"
            | ".git"
            | ".hg"
            | ".svn"
            | ".cargo-lock"
            | ".cargo-build-lock"
            | ".cargo-artifact-lock"
    ) {
        oracle.refuses_walk = true;
    }
    match node {
        NodeSpec::Dir(dir) => {
            oracle.refuses_walk |= dir.attrs.uid.is_some_and(|uid| uid != 1000);
            for (name, child) in &dir.entries {
                collect_spec(&path.join(name), child, oracle);
            }
        }
        NodeSpec::File(file) => {
            oracle.refuses_walk |= file.uid.is_some_and(|uid| uid != 1000)
                || file.special.is_some_and(|kind| {
                    matches!(kind, Special::Socket | Special::Fifo | Special::CharDevice)
                });
            if file.symlink.is_some() {
                oracle.symlinks.insert(path.to_owned());
            }
        }
    }
}

fn spec_oracle(model: &HostModel) -> SpecOracle {
    let mut oracle = SpecOracle::default();
    collect_spec(Path::new(ROOT), &model.tree[Path::new(ROOT)], &mut oracle);
    if model.mounts.iter().any(|mount| mount.at.starts_with(ROOT)) {
        oracle.refuses_walk = true;
    }
    oracle
}

fn forbidden_traversal(model: &HostModel, oracle: &SpecOracle, trace: &[Call]) -> Option<String> {
    let nested_mounts: Vec<_> = model
        .mounts
        .iter()
        .filter(|mount| mount.at.starts_with(ROOT))
        .map(|mount| mount.at.as_path())
        .collect();
    trace
        .iter()
        .find(|call| {
            let Some(path) = &call.path else {
                return false;
            };
            // mountinfo must be consulted before touching a nested mount point.
            let touched_mount = nested_mounts.iter().any(|mount| path.starts_with(mount))
                && matches!(
                    call.op,
                    Op::Openat2 | Op::Statx | Op::ReadDir | Op::Unlinkat | Op::Fchmod
                );
            let followed_link = oracle.symlinks.iter().any(|link| {
                (path != link && path.starts_with(link))
                    || (path == link && matches!(call.op, Op::Openat2 | Op::ReadDir | Op::Fchmod))
            });
            touched_mount || followed_link
        })
        .map(ToString::to_string)
}

fn cargo_model(mask: u8, release: bool, bytes: u64) -> HostModel {
    let profile = if release { "release" } else { "debug" };
    let yaml = format!(
        "host_model: 1\nclock: 2026-10-15T03:00:00Z\nmounts:\n  - {{id: 1, at: /, fs: ext4, dev: '254:1', size: 100G}}\n  - {{id: 30, at: /tmp, fs: tmpfs, dev: '0:42', size: 1G, parent: 1}}\ntree:\n  /tmp/w/p:\n    Cargo.toml: {{}}\n    target:\n      CACHEDIR.TAG: {{cachedir_tag: true}}\n      {profile}:\n        .cargo-lock: {{}}\n        .cargo-build-lock: {{}}\n        .cargo-artifact-lock: {{}}\n        deps:\n          built: {{size: 1M}}\n"
    );
    let mut model = diskgc_model::load::parse_yaml(&yaml).expect("cargo fixture parses");
    for bit in 0..4 {
        if mask & (1 << bit) != 0 {
            model.tree.insert(
                PathBuf::from(format!("/tmp/w/p/target/{profile}/deps/extra-{bit}")),
                NodeSpec::File(FileSpec {
                    size: Some(diskgc_model::spec::Bytes(bytes)),
                    ..FileSpec::default()
                }),
            );
        }
    }
    model
}

proptest! {
    #![proptest_config(property_config())]

    #[test]
    fn a_deletion_only_unlinks_walked_authorised_entries(model in host_model(3, 3, 0)) {
        let oracle = spec_oracle(&model);
        let host = loaded(&model)?;
        let parent = resolve(&host, Path::new("/tmp"), Want::Directory)
            .map_err(|error| failure(&model, format!("resolve /tmp: {error}")))?;
        let candidate = Candidate::new(&parent, Path::new(ROOT))
            .ok_or_else(|| failure(&model, "generated candidate has no basename"))?;
        let rules = Rules::new(&[1000]);
        let record = walk(&host, &candidate, &rules, None);
        match record {
            Ok(record) => {
                if oracle.refuses_walk {
                    return Err(failure(&model, "walk accepted a forbidden name, owner, IPC entry or nested mount in the model specification"));
                }
                let checked: BTreeSet<_> = record.objects().map(|object| {
                    let relative = record.path_of(object).expect("walked object has a path");
                    (Path::new(ROOT).join(relative), object)
                }).collect();
                if record.all_entries().any(|entry| entry.identity.uid != 1000) {
                    return Err(failure(&model, "walk accepted a foreign-owned entry"));
                }
                let deletion = delete_unchecked(&host, &candidate, &record, DeleteOptions::default());
                if deletion.end != End::Complete {
                    return Err(failure(&model, format!("stable eligible tree failed deletion: {deletion:?}")));
                }
                let trace = host.trace();
                let unlinks: Vec<_> = trace.iter().filter(|call| call.op == Op::Unlinkat && call.result == "ok").collect();
                let actual_paths: BTreeSet<_> = unlinks.iter().filter_map(|call| call.path.clone()).collect();
                if actual_paths != oracle.paths || unlinks.len() != oracle.paths.len() {
                    return Err(failure(&model, format!("unlinked {actual_paths:?}; model specifies {:?}", oracle.paths)));
                }
                if let Some(unlink) = unexpected_unlink(&trace, &checked) {
                    return Err(failure(&model, format!("unchecked entry unlinked: {unlink}")));
                }
            }
            Err(_) => {
                if !oracle.refuses_walk {
                    return Err(failure(&model, "walk refused a model-specification-safe candidate"));
                }
                if host.trace().iter().any(|call| call.op == Op::Unlinkat) {
                    return Err(failure(&model, "refused walk issued unlinkat"));
                }
            }
        }
        if let Some(call) = forbidden_traversal(&model, &oracle, &host.trace()) {
            return Err(failure(&model, format!("crossed a modelled mount or followed a symlink: {call}")));
        }
    }

    /// TASK-263, TASK-169: what no process shows still keeps a candidate.
    /// Judged from the model's specification, not from the scan: a process
    /// list that cannot be complete (`hidepid`, or a PID namespace other
    /// than the initial one outside a test container) makes the candidate
    /// `in_use_unknown`, and so does a loop device diskgc cannot open whose
    /// name proves nothing: deleted (the candidate shares its directory's
    /// filesystem), naming nothing, or naming a file of the run's owner.
    /// Otherwise a device diskgc can open, backed by a file of the
    /// candidate, keeps it. The candidate is never cleared in either case.
    #[test]
    fn the_in_use_check_never_clears_what_it_cannot_see_or_a_loop_device_holds(
        model in host_model_with_kernel_holders(3, 3, 2),
    ) {
        let host = loaded(&model)?;
        let parent = resolve(&host, Path::new("/tmp"), Want::Directory)
            .map_err(|error| failure(&model, format!("resolve /tmp: {error}")))?;
        let candidate = Candidate::new(&parent, Path::new(ROOT))
            .ok_or_else(|| failure(&model, "generated candidate has no basename"))?;
        let rules = Rules::new(&[1000]);
        let Ok(record) = walk(&host, &candidate, &rules, None) else {
            // A refused walk deletes nothing: the other property's ground.
            return Ok(());
        };
        let hidden = model.proc_hidepid || model.pid_namespace == PidNamespaceSpec::Nested;
        let objects: BTreeSet<DevIno> = record.objects().collect();
        let identity = |path: &Path| {
            host.stat_path(path)
                .and_then(|stat| stat.ino.map(|ino| (DevIno { dev: stat.dev, ino }, stat.uid)))
        };
        let mut unknown = false;
        let mut held = false;
        for spec in &model.loops {
            let text = spec.backing.to_string_lossy();
            if spec.readable {
                held |= identity(&spec.backing).is_some_and(|(id, _)| objects.contains(&id));
            } else if text.ends_with(" (deleted)") {
                // Its directory, /tmp/gen, is on the candidate's filesystem.
                unknown = true;
            } else {
                match identity(&spec.backing) {
                    None => unknown = true,
                    Some((_, uid)) => unknown |= uid != Some(OWNERS_OTHER),
                }
            }
        }
        let scan = diskgc_core::inuse::scan(
            &host,
            &diskgc_core::inuse::Options::from_rules(&rules),
            None,
        )
        .map_err(|error| failure(&model, format!("scan: {error}")))?;
        let verdict = scan.check(&candidate, &record);
        let reason = verdict.as_ref().err().map(|skip| skip.reason);
        if hidden || unknown {
            if reason != Some(diskgc_core::engine::Reason::InUseUnknown) {
                return Err(failure(&model, format!(
                    "hidden {hidden}, loop device unknown {unknown}: {verdict:?}"
                )));
            }
        } else if held && verdict.is_ok() {
            return Err(failure(&model, "a loop device's backing file in the candidate was cleared"));
        }
    }

    #[test]
    fn exit_code_precedence_follows_independent_table(
        sample in 0u8..3,
        deadline in any::<bool>(),
        errors in any::<bool>(),
        interrupt in 0u8..3,
        lock_timed_out in any::<bool>(),
        usage in any::<bool>(),
    ) {
        let final_sample = match sample { 0 => Final::Met, 1 => Final::Below, _ => Final::Unknown };
        let actual = if usage {
            Status::UsageError
        } else if interrupt == 1 {
            Status::Interrupted(Signal::Int)
        } else if interrupt == 2 {
            Status::Interrupted(Signal::Term)
        } else if lock_timed_out {
            after_lock_timeout(final_sample)
        } else {
            conclude(final_sample, deadline, errors)
        };
        let expected = if usage { 2 }
            else if interrupt == 1 { 130 }
            else if interrupt == 2 { 143 }
            else if sample == 0 { 0 }
            else if sample == 2 { 3 }
            else if lock_timed_out { 75 }
            else if deadline { 124 }
            else if errors { 3 }
            else { 1 };
        prop_assert_eq!(actual.exit_code(), expected);
    }

    #[test]
    fn selection_matches_reference_sets_and_negation_only_narrows(mask in 0u8..16) {
        let model = candidates(mask);
        let host = loaded(&model)?;
        let names: BTreeSet<_> = ["a0", "b0", "ab0", "c0"]
            .into_iter().enumerate().filter(|(bit, _)| mask & (1 << bit) != 0)
            .map(|(_, name)| name).collect();
        let expected_a: BTreeSet<_> = names.iter().filter(|name| name.starts_with('a'))
            .map(|name| PathBuf::from(format!("/tmp/{name}"))).collect();
        let expected_b: BTreeSet<_> = names.iter().filter(|name| name.contains('b'))
            .map(|name| PathBuf::from(format!("/tmp/{name}"))).collect();
        let actual_a = eligible(&host, "profile:prop-a");
        let actual_b = eligible(&host, "profile:prop-b");
        let union_ab = eligible(&host, "profile:prop-a || profile:prop-b");
        let union_ba = eligible(&host, "profile:prop-b || profile:prop-a");
        let minus_b = eligible(&host, "profile:prop-a && !profile:prop-b");
        let protected = protected_a(&host);
        let expected_union: BTreeSet<PathBuf> = expected_a.union(&expected_b).cloned().collect();
        let expected_minus: BTreeSet<PathBuf> = expected_a.difference(&expected_b).cloned().collect();
        for (label, actual, expected) in [
            ("a", actual_a, expected_a.clone()), ("b", actual_b, expected_b.clone()),
            ("a || b", union_ab, expected_union.clone()),
            ("b || a", union_ba, expected_union),
            ("a && !b", minus_b, expected_minus.clone()),
            ("a protected by b", protected, expected_minus),
        ] {
            if actual != expected {
                return Err(failure(&model, format!("selection {label}: got {actual:?}, expected {expected:?}")));
            }
        }
    }

    #[test]
    fn ordered_dry_run_agrees_with_deletion_prefix(mask in 0u8..16) {
        let model = candidates(mask);
        let plan_host = loaded(&model)?;
        let request = request("profile:prop-a || profile:prop-b");
        let plan = executor::dry_run(&plan_host, &request, &[1000], &mut |_| {});
        if !plan.errors.is_empty() {
            return Err(failure(&model, format!("plan errors: {:?}", plan.errors)));
        }
        let planned: Vec<_> = plan.plan.iter().filter(|entry| entry.reason.is_none())
            .map(|entry| entry.candidate.clone().expect("a candidate")).collect();
        let run_host = loaded(&model)?;
        let report = executor::ensure(&run_host, &request, &[1000], &mut |_| {});
        if !report.errors.is_empty() || !report.skipped.is_empty() {
            return Err(failure(&model, format!("unexpected stable-run refusal: {report:?}")));
        }
        let actions: Vec<_> = report.actions.iter().map(|action| action.candidate.clone()).collect();
        if !planned.starts_with(&actions) {
            return Err(failure(&model, format!("actions {actions:?} are not a prefix of eligible plan {planned:?}")));
        }
        if actions.len() != planned.len() {
            return Err(failure(&model, format!("stable run acted on {} of {} eligible entries", actions.len(), planned.len())));
        }
    }

    #[test]
    fn cargo_reclaim_never_unlinks_or_moves_a_generated_lock_file(
        mask in 0u8..16,
        release in any::<bool>(),
        bytes in 1u64..65536,
    ) {
        let model = cargo_model(mask, release, bytes);
        let host = loaded(&model)?;
        let discovered = cargo_target::discover(&host, &[PathBuf::from("/tmp/w")], None)
            .map_err(|error| failure(&model, format!("discover: {error}")))?;
        if discovered.targets.len() != 1 {
            return Err(failure(&model, format!("expected one Cargo target: {discovered:?}")));
        }
        let profile = if release { "release" } else { "debug" };
        let directory = PathBuf::from(format!("/tmp/w/p/target/{profile}"));
        let locks: Vec<_> = [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"]
            .into_iter().map(|name| directory.join(name)).map(|path| {
                let inode = host.stat_path(&path).and_then(|stat| stat.ino)
                    .expect("generated lock exists with inode");
                (path, inode)
            }).collect();
        let rules = Rules::new(&[1000]);
        let mut checker = Checker::new(&rules, REUSE_LIMIT);
        let done = cargo_target::reclaim(
            &host, &discovered.targets[0], None, &rules,
            IdleWindow::default(), &mut checker, None,
        );
        if done.end != ReclaimEnd::Complete {
            return Err(failure(&model, format!("generated Cargo reclaim: {done:?}")));
        }
        for (path, inode) in &locks {
            if host.stat_path(path).and_then(|stat| stat.ino) != Some(*inode) {
                return Err(failure(&model, format!("Cargo lock disappeared or moved: {}", path.display())));
            }
        }
        let trace = host.trace();
        if let Some(call) = trace.iter().find(|call| {
            call.op == Op::RunCommand || (call.op == Op::Unlinkat && call.result == "ok"
                && call.path.as_ref().is_some_and(|path| {
                    !path.starts_with(&directory) || path.file_name().is_some_and(|name| locks.iter().any(|(lock, _)| lock.file_name() == Some(name)))
                }))
        }) {
            return Err(failure(&model, format!("Cargo operated on a lock or outside its profile: {call}")));
        }
    }
}

/// Injects the observable effect of a deliberately broken delete that
/// unlinks an extra name. The same oracle used above must reject it; the
/// generated input shrinks to the saved, replayed YAML model.
fn planted_unchecked_unlink(model: &HostModel) -> Result<(), TestCaseError> {
    let host = loaded(model)?;
    let parent = resolve(&host, Path::new("/tmp"), Want::Directory)
        .map_err(|error| failure(model, error))?;
    let candidate = Candidate::new(&parent, Path::new("/tmp/a0"))
        .ok_or_else(|| failure(model, "a0 candidate"))?;
    let record = walk(&host, &candidate, &Rules::new(&[1000]), None)
        .map_err(|error| failure(model, format!("walk: {error:?}")))?;
    let checked: BTreeSet<_> = record
        .objects()
        .map(|object| {
            (
                Path::new("/tmp/a0").join(record.path_of(object).expect("walked path")),
                object,
            )
        })
        .collect();
    let end = delete_unchecked(&host, &candidate, &record, DeleteOptions::default());
    if end.end != End::Complete {
        return Err(failure(model, format!("delete: {end:?}")));
    }
    // This deliberately broken caller makes a *real ModelHost deletion*
    // outside the walk. ModelHost requires a preceding statx even for the
    // planted operation, so the trace is genuine and correctly indexed.
    host.statx(Base::Dir(&parent.fd), Path::new("unchecked"))
        .map_err(|error| failure(model, format!("plant statx: {error}")))?;
    host.unlinkat(&parent.fd, OsStr::new("unchecked"), Unlink::NonDir)
        .map_err(|error| failure(model, format!("plant unlinkat: {error}")))?;
    let trace = host.trace();
    if let Some(unlink) = unexpected_unlink(&trace, &checked) {
        Err(failure(
            model,
            format!("planted unchecked unlink: {unlink}"),
        ))
    } else {
        Ok(())
    }
}

#[test]
fn planted_unchecked_unlink_shrinks_to_saved_yaml_and_replays() {
    let mut runner = TestRunner::new(ProptestConfig {
        cases: 32,
        failure_persistence: None,
        ..ProptestConfig::default()
    });
    let result = runner.run(&(0u8..16), |mask| {
        let mut model = candidates(mask | 1); // a0 is always eligible.
        model.tree.insert(
            PathBuf::from("/tmp/unchecked"),
            NodeSpec::File(FileSpec::default()),
        );
        planted_unchecked_unlink(&model)
    });
    let (reason, shrunk_mask) = match result {
        Err(TestError::Fail(reason, mask)) => (reason, mask),
        other => panic!("planted bug did not shrink: {other:?}"),
    };
    assert!(
        reason.to_string().contains("planted unchecked unlink"),
        "{reason}"
    );
    assert_eq!(shrunk_mask, 0, "the optional candidates should shrink away");
    let saved = diskgc_model::load::parse_yaml(include_str!(
        "../../diskgc-model/tests/regressions/unchecked-entry.yaml"
    ))
    .expect("saved regression parses");
    let mut shrunk = candidates(shrunk_mask | 1);
    shrunk.tree.insert(
        PathBuf::from("/tmp/unchecked"),
        NodeSpec::File(FileSpec::default()),
    );
    assert_eq!(saved, shrunk, "saved YAML replays the shrunk input");
    let replay = planted_unchecked_unlink(&saved).expect_err("saved bug must replay");
    assert!(
        replay.to_string().contains("planted unchecked unlink"),
        "{replay}"
    );
}
