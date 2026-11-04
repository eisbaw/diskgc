//! `survey` (TASK-38, TASK-116; docs/reference.md, `survey`) through its
//! composition (`diskgc_core::survey::load` and `run`, what the binary
//! runs) on ModelHost: the per-profile summary is the dry run's plan, the
//! unmatched top-level entries are grouped by name family and sized, the
//! candidate list is cut at 1,000 per profile, the JSON has the documented
//! shape, and nothing is deleted, locked or written.

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use common::host;
use diskgc_core::invocation::{Catalog, ConfigFile, Platform};
use diskgc_core::profile::Library;
use diskgc_core::report::Status;
use diskgc_core::survey::{self, Survey, SurveyOptions};
use diskgc_model::ModelHost;
use diskgc_model::spec::Op;
use serde_json::Value;

/// A laptop's `/tmp` (tmpfs, mount 30) as the 2026-10 dogfood surveys
/// categorised it by hand: pytest sessions inside a `nix-shell.*` temp
/// dir, `nix-shell` rc dirs, Claude Code's temp root (no profile since its
/// built-in was withdrawn), mktemp dirs, and a `systemd-private-*` dir
/// that is never looked into. pid 42 holds a file of `pytest-1` open.
fn laptop_tmp(extra: &str) -> String {
    format!(
        r#"host_model: 1
clock: 2026-10-15T03:00:00Z
mounts:
  - {{id: 1, at: /, fs: ext4, dev: "254:1", size: 100G}}
  - {{id: 30, at: /tmp, fs: tmpfs, dev: "0:42", size: 4G, parent: 1}}
tree:
  /usr/bin/bash: {{}}
  /home/tester: {{.: {{mtime: 2026-10-01T00:00:00Z}}}}
  /tmp/nix-shell.Ab12Cd/pytest-of-tester/pytest-0: {{.: {{mtime: 2026-10-01T00:00:00Z}}, out: {{size: 3M, mtime: 2026-10-01T00:00:00Z}}}}
  /tmp/nix-shell.Ab12Cd/pytest-of-tester/pytest-1: {{.: {{mtime: 2026-10-01T00:00:00Z}}, out: {{size: 1M, mtime: 2026-10-01T00:00:00Z}}}}
  /tmp/nix-shell-4242-17: {{.: {{mtime: 2026-10-01T00:00:00Z}}, env-vars: {{mtime: 2026-10-01T00:00:00Z}}, rc: {{mtime: 2026-10-01T00:00:00Z}}}}
  /tmp/claude-1000: {{.: {{mtime: 2026-10-14T00:00:00Z}}, session: {{size: 2M, mtime: 2026-10-14T00:00:00Z}}}}
  /tmp/tmp.Xy12ab: {{.: {{mtime: 2026-10-10T03:00:00Z}}, f: {{size: 4M, mtime: 2026-10-10T03:00:00Z}}}}
  /tmp/tmp.Qw34er: {{.: {{mtime: 2026-10-14T03:00:00Z}}, f: {{size: 1M, mtime: 2026-10-14T03:00:00Z}}}}
  /tmp/tmp.Zz99zz: {{size: 8K, mtime: 2026-10-12T03:00:00Z}}
  /tmp/systemd-private-abc-colord.service-Q1: {{.: {{mode: "700", uid: 0, gid: 0}}}}
{extra}processes:
  - {{pid: 42, exe: /usr/bin/bash, cwd: /, maps: [/usr/bin/bash], environ: [HOME=/home/tester], fds: [{{path: /tmp/nix-shell.Ab12Cd/pytest-of-tester/pytest-1/out}}]}}
"#
    )
}

fn options(paths: &[&str], select: Option<&str>) -> SurveyOptions {
    SurveyOptions {
        paths: paths.iter().map(PathBuf::from).collect(),
        select: select.map(str::to_owned),
        roots: Vec::new(),
        timeout: Some(Duration::from_secs(600)),
    }
}

/// The survey of `options` on `host` with the built-ins and the
/// configuration `config`, as the binary composes it.
fn run(host: &ModelHost, options: &SurveyOptions, config: Option<&str>) -> Survey {
    let config_file = match config {
        Some(text) => ConfigFile::Read {
            path: PathBuf::from("/etc/diskgc/config.yaml"),
            bytes: text.as_bytes().to_vec(),
        },
        None => ConfigFile::NotFound {
            tried: vec![PathBuf::from("/etc/diskgc/config.yaml")],
        },
    };
    let draw_seed = || Err("no seed".to_owned());
    let builtins = || Library::builtin().map_err(|e| e.to_string());
    let platform = Platform {
        catalog: Catalog::Development,
        draw_seed: &draw_seed,
        builtins: &builtins,
    };
    let loaded = survey::load(options, &config_file, &platform, &mut |_| {})
        .unwrap_or_else(|(cause, why)| panic!("{cause:?}: {why}"));
    survey::run(host, options, &loaded, &mut |_| {}, &mut |_| {})
}

fn json(survey: &Survey) -> Value {
    serde_json::from_str(&survey.to_json()).expect("survey JSON parses")
}

/// The summary block of `profile` in the first path's JSON.
fn block<'v>(value: &'v Value, profile: &str) -> &'v Value {
    value["paths"][0]["summary"]
        .as_array()
        .expect("summary")
        .iter()
        .find(|b| b["profile"] == profile)
        .unwrap_or_else(|| panic!("no summary block for {profile}: {value}"))
}

/// The unmatched family `family` in the first path's JSON.
fn family<'v>(value: &'v Value, family: &str) -> &'v Value {
    value["paths"][0]["unmatched"]["families"]
        .as_array()
        .expect("families")
        .iter()
        .find(|f| f["family"] == family)
        .unwrap_or_else(|| panic!("no family {family}: {value}"))
}

fn families(value: &Value) -> BTreeSet<String> {
    value["paths"][0]["unmatched"]["families"]
        .as_array()
        .expect("families")
        .iter()
        .map(|f| f["family"].as_str().expect("a name").to_owned())
        .collect()
}

/// Nothing a survey may never do: no deletion, no permission change, no
/// command, no lock, no state written (the snapshot's or the lock's).
fn assert_read_only(host: &ModelHost) {
    for op in [
        Op::MayDelete,
        Op::Unlinkat,
        Op::Fchmod,
        Op::RunCommand,
        Op::Flock,
        Op::InheritLock,
        Op::NixRoots,
        Op::NixGc,
        Op::StateLock,
        Op::StateRead,
        Op::StateWrite,
        Op::UserLock,
        Op::UserLockPath,
    ] {
        assert_eq!(host.trace_count(op), 0, "survey called {op:?}");
    }
}

/// AC 1: the manual categorisation of the laptop's `/tmp`, reproduced by
/// the built-ins with every profile applied: pytest sessions in a
/// nix-shell temp dir and the rc dir are their profiles' candidates (one
/// session in use), and what no profile matches is grouped by name family
/// and sized: the nix-shell temp dir holding the sessions is matched, the
/// Claude root and the mktemp dirs are not, and `systemd-private-*` is
/// neither listed nor statted.
#[test]
fn the_laptop_tmp_is_categorised_by_profile_and_name_family() {
    let host = host(&laptop_tmp(""));
    let survey = run(&host, &options(&["/tmp"], None), None);
    assert_eq!(survey.status, Status::DryRun, "{survey:?}");
    let value = json(&survey);
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["exit_code"], 0);

    let pytest = block(&value, "pytest-tmp");
    assert_eq!(pytest["found"], 2, "{pytest}");
    let kept: Vec<&str> = pytest["kept"]
        .as_array()
        .expect("kept")
        .iter()
        .map(|k| k["reason"].as_str().expect("reason"))
        .collect();
    assert!(kept.contains(&"in_use"), "{pytest}");
    assert_eq!(block(&value, "nix-shell-rc")["found"], 1);
    // A profile below /tmp that finds nothing still has its block.
    assert_eq!(block(&value, "node-compile-cache")["found"], 0);

    assert_eq!(
        families(&value),
        BTreeSet::from(["claude-#".to_owned(), "tmp.*".to_owned()]),
        "the matched nix-shell dirs and systemd-private-* are no family: {value}"
    );
    let tmp = family(&value, "tmp.*");
    assert_eq!(tmp["count"], 3);
    assert_eq!(
        tmp["examples"][0], "/tmp/tmp.Xy12ab",
        "largest first: {tmp}"
    );
    let bytes = tmp["estimated_bytes"].as_u64().expect("sized");
    assert!(
        bytes >= (5 << 20) + (8 << 10),
        "the three trees' bytes: {tmp}"
    );
    assert_eq!(tmp["newest_idle_age_seconds"], 86_400, "{tmp}");
    assert_eq!(tmp["oldest_idle_age_seconds"], 5 * 86_400, "{tmp}");
    assert_eq!(family(&value, "claude-#")["count"], 1);
    assert_eq!(value["paths"][0]["unmatched"]["count"], 4);

    let private = "/tmp/systemd-private-abc-colord.service-Q1";
    assert!(
        !host
            .trace()
            .iter()
            .any(|c| c.op == Op::Statx && c.path.as_deref() == Some(private.as_ref())),
        "systemd-private-* was statted"
    );
    assert_read_only(&host);
    // Without --select the backend kinds are named, not queried.
    for op in [
        Op::DockerQuery,
        Op::PodmanQuery,
        Op::NixQuery,
        Op::NixDaemonProbe,
    ] {
        assert_eq!(host.trace_count(op), 0, "survey called {op:?}");
    }
    let not_surveyed: Vec<(&str, &str)> = value["paths"][0]["not_surveyed"]
        .as_array()
        .expect("not_surveyed")
        .iter()
        .map(|n| (n["profile"].as_str().unwrap(), n["why"].as_str().unwrap()))
        .collect();
    assert!(
        not_surveyed.contains(&("nix-garbage", "backend")),
        "{not_surveyed:?}"
    );

    let text = survey.to_text(false);
    assert!(text.contains("survey /tmp (filesystem /tmp"), "{text}");
    assert!(text.contains("  profile pytest-tmp: 2 found"), "{text}");
    assert!(text.contains("    tmp.*  3 entries  est. "), "{text}");
    assert!(
        !text.contains("would reclaim"),
        "a survey has no goal: {text}"
    );
}

/// A `--select` union of two profiles is one tier with one block per
/// profile; `--list` and the JSON list each profile's candidates largest
/// first, at most 1,000 per profile, and count the rest.
#[test]
fn the_candidates_are_listed_up_to_1000_per_profile_and_the_rest_counted() {
    let many: String = (0..1003)
        .map(|n| format!("  /tmp/many/m-{n:04}: {{size: {}K}}\n", 4 + n % 7))
        .collect();
    let host = host(&laptop_tmp(&many));
    let config = r#"version: 1
profiles:
  - name: many-files
    find: {roots: [/tmp], paths: ["many/m-*"], type: file}
    action: delete
    examples: {match: [/tmp/many/m-1]}
  - name: shell-tmp
    find: {roots: [/tmp], paths: ["tmp.*"], type: any}
    action: delete
    examples: {match: [/tmp/tmp.x]}
"#;
    let survey = run(
        &host,
        &options(&["/tmp"], Some("profile:many-files || profile:shell-tmp")),
        Some(config),
    );
    assert_eq!(survey.status, Status::DryRun, "{survey:?}");
    let value = json(&survey);
    let many = block(&value, "many-files");
    assert_eq!(many["found"], 1003);
    assert_eq!(many["listed"], 1000);
    assert_eq!(many["left_out"], 3);
    let shell = block(&value, "shell-tmp");
    assert_eq!(
        (shell["found"].clone(), shell["listed"].clone()),
        (3.into(), 3.into())
    );
    let rows = value["paths"][0]["candidates"].as_array().expect("rows");
    assert_eq!(rows.len(), 1003, "1000 + 3");
    let sizes: Vec<u64> = rows
        .iter()
        .filter(|r| r["profile"] == "many-files")
        .map(|r| r["estimated_bytes"].as_u64().expect("sized"))
        .collect();
    assert!(sizes.windows(2).all(|w| w[0] >= w[1]), "largest first");
    // The selected profiles' matches are no family; the rest of /tmp is.
    assert!(!families(&value).contains("tmp.*"), "{value}");
    assert!(!families(&value).contains("many"), "{value}");

    let listed = survey.to_text(true);
    assert!(listed.contains("      ... 3 more not listed\n"), "{listed}");
    assert!(!survey.to_text(false).contains("more not listed"));
    assert_read_only(&host);
}

/// Each `PATH` is surveyed on its own: a fixed root outside it is not
/// searched there, `roots: required` profiles look below it, and its
/// unmatched entries are its own.
#[test]
fn each_path_reports_only_what_lies_below_it() {
    let extra = "  /home/tester/src/pkg/__pycache__: {.: {mtime: 2026-10-01T00:00:00Z}, \
                 m.cpython-312.pyc: {size: 2M, mtime: 2026-10-01T00:00:00Z}}\n  \
                 /home/tester/notes-12: {size: 4K}\n";
    let host = host(&laptop_tmp(extra));
    let survey = run(&host, &options(&["/home/tester", "/tmp"], None), None);
    let value = json(&survey);
    let home = &value["paths"][0];
    assert_eq!(home["path"], "/home/tester");
    let profiles: BTreeSet<&str> = home["summary"]
        .as_array()
        .expect("summary")
        .iter()
        .map(|b| b["profile"].as_str().expect("profile"))
        .collect();
    assert!(
        !profiles.contains("pytest-tmp"),
        "/tmp is not below /home/tester"
    );
    let pycache = home["summary"]
        .as_array()
        .expect("summary")
        .iter()
        .find(|b| b["profile"] == "python-pycache")
        .expect("a roots: required profile looks below PATH");
    assert_eq!(pycache["found"], 1, "{pycache}");
    let home_families: BTreeSet<&str> = home["unmatched"]["families"]
        .as_array()
        .expect("families")
        .iter()
        .map(|f| f["family"].as_str().expect("family"))
        .collect();
    assert_eq!(
        home_families,
        BTreeSet::from(["notes-#"]),
        "src holds a candidate"
    );
    assert_eq!(value["paths"][1]["path"], "/tmp");
    assert_read_only(&host);
}

/// The JSON's fields are the documented ones, in the reference's example
/// (docs/reference.md, `survey`): each key it shows is present, and none
/// it does not show.
#[test]
fn the_json_has_exactly_the_documented_fields() {
    // Read at run time, as the other doc tests do (the flake's
    // workspace-tests source includes the reference).
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/reference.md");
    let reference = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let example = survey_json_example(&reference);
    let host = host(&laptop_tmp(""));
    let value = json(&run(&host, &options(&["/tmp"], None), None));
    let mut problems = Vec::new();
    same_keys(&example, &value, "", &mut problems);
    assert!(problems.is_empty(), "{problems:#?}");
    assert_read_only(&host);
}

/// The first ```json block after the `survey` JSON heading.
fn survey_json_example(reference: &str) -> Value {
    let start = reference
        .find("#### Survey JSON")
        .expect("docs/reference.md has a `#### Survey JSON` section");
    let rest = &reference[start..];
    let open = rest.find("```json\n").expect("a JSON example") + "```json\n".len();
    let close = rest[open..].find("```").expect("the example ends") + open;
    serde_json::from_str(&rest[open..close]).expect("the example parses")
}

/// Every object in `documented` and `observed` has the same keys,
/// recursively; arrays compare their first elements (when both have one).
fn same_keys(documented: &Value, observed: &Value, at: &str, problems: &mut Vec<String>) {
    match (documented, observed) {
        (Value::Object(d), Value::Object(o)) => {
            let dk: BTreeSet<&String> = d.keys().collect();
            let ok: BTreeSet<&String> = o.keys().collect();
            if dk != ok {
                problems.push(format!("{at}: documented {dk:?}, observed {ok:?}"));
            }
            for key in dk.intersection(&ok) {
                same_keys(&d[*key], &o[*key], &format!("{at}.{key}"), problems);
            }
        }
        (Value::Array(d), Value::Array(o)) => {
            if let (Some(d), Some(o)) = (d.first(), o.first()) {
                same_keys(d, o, &format!("{at}[0]"), problems);
            }
        }
        _ => {}
    }
}

/// A survey refuses what `ensure` refuses before it plans: a missing
/// `--roots` directory is exit 2 and nothing is looked at.
#[test]
fn a_missing_supplied_root_refuses_the_survey() {
    let host = host(&laptop_tmp(""));
    let mut options = options(&["/home/tester"], Some("profile:cmake-build-anchored"));
    options.roots = vec!["/home/tester/absent".to_owned()];
    let survey = run(&host, &options, None);
    assert_eq!(survey.status, Status::UsageError, "{survey:?}");
    assert!(survey.paths.is_empty());
    assert!(
        survey.errors[0].message.contains("/home/tester/absent"),
        "{survey:?}"
    );
    let value = json(&survey);
    assert_eq!(value["exit_code"], 2);
    assert_eq!(value["paths"], Value::Array(Vec::new()));
    assert_read_only(&host);
}

/// One profile's plan error (here a fixed root reached through a symlink,
/// a misconfiguration that excludes its tier) stops no other profile:
/// each is planned on its own, and the survey is an error (exit 3).
#[test]
fn one_profiles_error_stops_no_other_profile() {
    let host = host(&laptop_tmp("  /tmp/link: {symlink: /tmp/claude-1000}\n"));
    let config = r#"version: 1
profiles:
  - name: aaa-broken
    find: {roots: [/tmp/link], paths: ["*"], type: any}
    action: delete
    examples: {match: [/tmp/link/x]}
"#;
    let survey = run(&host, &options(&["/tmp"], None), Some(config));
    assert_eq!(survey.status, Status::Error, "{survey:?}");
    let value = json(&survey);
    assert_eq!(value["exit_code"], 3);
    let errors = value["paths"][0]["errors"].as_array().expect("errors");
    assert!(
        errors.iter().any(|e| e["tier"] == "aaa-broken"),
        "{errors:?}"
    );
    assert_eq!(
        block(&value, "pytest-tmp")["found"],
        2,
        "planned after the error"
    );
    assert_eq!(block(&value, "nix-shell-rc")["found"], 1);
    assert_read_only(&host);
}

/// A `PATH` that is itself a candidate has no unmatched entry: everything
/// below it is the candidate's.
#[test]
fn a_path_that_is_a_candidate_has_no_unmatched_entries() {
    let extra = "  /home/tester/p/Cargo.toml: {}\n  /home/tester/p/target: {CACHEDIR.TAG: \
                 {cachedir_tag: true}, debug: {.cargo-lock: {}, deps: {a: {size: 1M}}}}\n";
    let host = host(&laptop_tmp(extra));
    let survey = run(
        &host,
        &options(&["/home/tester/p/target"], Some("profile:cargo-target")),
        None,
    );
    let value = json(&survey);
    let rows = value["paths"][0]["candidates"].as_array().expect("rows");
    assert_eq!(rows.len(), 1, "{value}");
    assert_eq!(rows[0]["candidate"], "/home/tester/p/target");
    assert_eq!(value["paths"][0]["unmatched"]["count"], 0, "{value}");
    assert_read_only(&host);
}

/// At the deadline the survey stops: the profiles not yet planned are
/// errors of their own, the unmatched entries are counted unsized, and
/// the status is `deadline` (exit 124).
#[test]
fn the_deadline_stops_the_survey_with_what_it_has() {
    // The first listing of /tmp takes past the 600 s timeout.
    let yaml = laptop_tmp("")
        + "events:\n  - {at: {op: read_dir, path: /tmp, nth: 1, when: after}, advance: 700s}\n";
    let host = host(&yaml);
    let survey = run(&host, &options(&["/tmp"], None), None);
    assert_eq!(survey.status, Status::Deadline, "{survey:?}");
    let value = json(&survey);
    assert_eq!(value["exit_code"], 124);
    let errors = value["paths"][0]["errors"].as_array().expect("errors");
    assert!(
        errors
            .iter()
            .any(|e| e["message"] == "deadline passed before this profile was planned"),
        "{errors:?}"
    );
    let unmatched = &value["paths"][0]["unmatched"];
    assert_eq!(unmatched["unsized"], unmatched["count"], "{unmatched}");
    assert_read_only(&host);
}

/// A kept candidate (here `protected`) has no idle age, and the profile's
/// age range is its eligible candidates' alone.
#[test]
fn a_kept_candidate_has_no_idle_age() {
    let host = host(&laptop_tmp(""));
    let config = r#"version: 1
profiles:
  - name: shell-tmp
    find: {roots: [/tmp], paths: ["tmp.*"], type: any}
    action: delete
    examples: {match: [/tmp/tmp.x]}
  - name: guard
    find: {roots: [/tmp], paths: [tmp.Xy12ab], type: dir}
    action: delete
    examples: {match: [/tmp/tmp.Xy12ab]}
protect: [guard]
"#;
    let survey = run(
        &host,
        &options(&["/tmp"], Some("profile:shell-tmp")),
        Some(config),
    );
    let value = json(&survey);
    let rows = value["paths"][0]["candidates"].as_array().expect("rows");
    let guarded = rows
        .iter()
        .find(|r| r["candidate"] == "/tmp/tmp.Xy12ab")
        .expect("the protected row");
    assert_eq!(guarded["reason"], "protected", "{guarded}");
    assert_eq!(guarded["idle_age_seconds"], Value::Null, "{guarded}");
    let shell = block(&value, "shell-tmp");
    assert_eq!(shell["newest_idle_age_seconds"], 86_400, "{shell}");
    assert_eq!(shell["oldest_idle_age_seconds"], 3 * 86_400, "{shell}");
    assert_read_only(&host);
}

/// `--select` of a profile whose fixed roots are all outside `PATH` does
/// not look for it there, and says so (`not_surveyed`), rather than
/// reporting that it found nothing.
#[test]
fn a_selected_profile_rooted_elsewhere_is_not_surveyed() {
    let host = host(&laptop_tmp(""));
    let survey = run(
        &host,
        &options(&["/home/tester"], Some("profile:pytest-tmp")),
        None,
    );
    let value = json(&survey);
    let path = &value["paths"][0];
    assert_eq!(path["summary"], Value::Array(Vec::new()), "{path}");
    assert_eq!(path["not_surveyed"][0]["profile"], "pytest-tmp");
    assert_eq!(path["not_surveyed"][0]["why"], "roots_elsewhere");
    assert!(
        survey
            .to_text(false)
            .contains("not looked for here (their roots are elsewhere): pytest-tmp"),
        "{}",
        survey.to_text(false)
    );
    assert_read_only(&host);
}

/// A mount point below `PATH` is another filesystem: no unmatched entry.
#[test]
fn a_mount_point_below_path_is_not_an_unmatched_entry() {
    let host = host(&laptop_tmp(""));
    let survey = run(&host, &options(&["/"], Some("profile:nix-shell-rc")), None);
    let value = json(&survey);
    let families: BTreeSet<String> = families(&value);
    assert!(!families.contains("tmp"), "{families:?}");
    assert!(
        families.contains("usr") && families.contains("home"),
        "{families:?}"
    );
    assert_eq!(
        block(&value, "nix-shell-rc")["found"],
        1,
        "/tmp's root is below /"
    );
    assert_read_only(&host);
}

/// A profile's error stays the survey's status when the deadline then
/// cuts the plan: `error` (exit 3) comes before `deadline` (124).
#[test]
fn an_error_before_the_deadline_stays_the_status() {
    let yaml = laptop_tmp("  /tmp/link: {symlink: /tmp/claude-1000}\n")
        + "events:\n  - {at: {op: read_dir, path: /tmp, nth: 1, when: after}, advance: 700s}\n";
    let host = host(&yaml);
    let config = r#"version: 1
profiles:
  - name: aaa-broken
    find: {roots: [/tmp/link], paths: ["*"], type: any}
    action: delete
    examples: {match: [/tmp/link/x]}
"#;
    let survey = run(&host, &options(&["/tmp"], None), Some(config));
    assert_eq!(survey.status, Status::Error, "{survey:?}");
    let value = json(&survey);
    assert_eq!(value["exit_code"], 3);
    assert_eq!(value["paths"][0]["status"], "error");
    let errors = value["paths"][0]["errors"].as_array().expect("errors");
    assert!(
        errors
            .iter()
            .any(|e| e["message"] == "deadline passed before this profile was planned"),
        "{errors:?}"
    );
    assert_read_only(&host);
}

/// A profile whose discovery fails (EIO on the first listing of `/tmp`,
/// the first profile's) was not surveyed: it has no summary block and no
/// "found nothing" (a failed listing is never an empty match set), its
/// error names it, the human text says it could not be surveyed and why,
/// and every other profile is planned as without the failure.
#[test]
fn a_profile_whose_listing_fails_is_not_reported_as_finding_nothing() {
    let failing = laptop_tmp("")
        + "events:\n  - {at: {op: read_dir, path: /tmp, nth: 1, when: before}, fail: EIO}\n";
    let survey = run(&host(&failing), &options(&["/tmp"], None), None);
    let clean = run(&host(&laptop_tmp("")), &options(&["/tmp"], None), None);
    assert_eq!(survey.status, Status::Error, "{survey:?}");
    let value = json(&survey);
    let errors = value["paths"][0]["errors"].as_array().expect("errors");
    let failed: Vec<&str> = errors
        .iter()
        .filter(|e| e["cause"] == "host")
        .filter_map(|e| e["profile"].as_str())
        .collect();
    let [failed] = failed[..] else {
        panic!("one profile's host error: {errors:?}")
    };
    assert!(
        errors
            .iter()
            .any(|e| e["profile"] == failed && e["message"].as_str().unwrap().contains("EIO")),
        "{errors:?}"
    );
    let blocks = value["paths"][0]["summary"].as_array().expect("summary");
    assert!(
        blocks.iter().all(|b| b["profile"] != failed),
        "{failed} has a summary block though its listing failed: {blocks:?}"
    );
    let text = survey.to_text(false);
    let nothing = text
        .lines()
        .find(|l| l.starts_with("  found nothing:"))
        .unwrap_or_default();
    assert!(
        !nothing.contains(failed),
        "{failed} reads as found nothing:\n{text}"
    );
    assert!(
        text.contains(&format!("  profile {failed}: could not be surveyed: "))
            && text.contains("EIO"),
        "{text}"
    );
    // Every other profile's block is what it is without the failure.
    let clean = json(&clean);
    let others = |v: &Value| -> Vec<Value> {
        v["paths"][0]["summary"]
            .as_array()
            .expect("summary")
            .iter()
            .filter(|b| b["profile"] != failed)
            .cloned()
            .collect()
    };
    assert_eq!(others(&value), others(&clean));
    assert_eq!(block(&value, "pytest-tmp")["found"], 2);
}
