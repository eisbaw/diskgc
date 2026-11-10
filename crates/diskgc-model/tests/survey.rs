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
        in_use: true,
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
    assert_eq!(value["schema_version"], 2);
    assert_eq!(value["in_use"], "checked");
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
    // Both modes have the same fields (one of `eligible` and `not_checked`
    // is null).
    for in_use in [true, false] {
        let host = host(&laptop_tmp(""));
        let options = SurveyOptions {
            in_use,
            ..options(&["/tmp"], None)
        };
        let value = json(&run(&host, &options, None));
        let mut problems = Vec::new();
        same_keys(&example, &value, "", &mut problems);
        assert!(problems.is_empty(), "in_use {in_use}: {problems:#?}");
        assert_read_only(&host);
    }
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
    // The human text says it once, naming the profiles.
    let text = survey.to_text(false);
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("deadline passed before this profile was planned"))
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    assert!(lines[0].contains(" profiles: "), "{text}");
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

/// Every `Host` call that reads another process (`/proc`, sockets, loop
/// devices): the in-use scan and the liveness contracts. A survey without
/// `--in-use` makes none.
const PROCESS_READS: [Op; 21] = [
    Op::Pids,
    Op::ProcIdentity,
    Op::ProcIdentityReleased,
    Op::ProcFds,
    Op::ProcLinks,
    Op::ProcMaps,
    Op::ProcEnviron,
    Op::ProcStatus,
    Op::BoundUnixSockets,
    Op::PidNamespace,
    Op::ProcNspid,
    Op::PidAlive,
    Op::ProcUids,
    Op::ProcCmdline,
    Op::ProcRootLookup,
    Op::ProcClimb,
    Op::ProcMountinfo,
    Op::LoopBackingFile,
    Op::LoopBackingFiles,
    Op::LoopBackingIdentity,
    Op::LoopDevices,
];

fn process_reads(host: &ModelHost) -> Vec<(Op, usize)> {
    PROCESS_READS
        .iter()
        .map(|op| (*op, host.trace_count(*op)))
        .filter(|(_, n)| *n > 0)
        .collect()
}

/// The pytest session pid 42 holds open, in both modes: kept `in_use`
/// with `--in-use`; without it, not checked (`eligible: null`), never
/// eligible, and no process was read.
#[test]
fn a_live_session_is_in_use_with_the_flag_and_not_checked_without() {
    let live = "/tmp/nix-shell.Ab12Cd/pytest-of-tester/pytest-1";
    let row = |value: &Value| -> Value {
        value["paths"][0]["candidates"]
            .as_array()
            .expect("rows")
            .iter()
            .find(|r| r["candidate"] == live)
            .unwrap_or_else(|| panic!("no row for {live}: {value}"))
            .clone()
    };

    let checked_host = host(&laptop_tmp(""));
    let checked = json(&run(&checked_host, &options(&["/tmp"], None), None));
    let checked_row = row(&checked);
    assert_eq!(checked_row["eligible"], false, "{checked_row}");
    assert_eq!(checked_row["reason"], "in_use", "{checked_row}");
    assert!(!process_reads(&checked_host).is_empty(), "--in-use scans");

    let default_host = host(&laptop_tmp(""));
    let options = SurveyOptions {
        in_use: false,
        ..options(&["/tmp"], None)
    };
    let survey = run(&default_host, &options, None);
    assert_eq!(survey.status, Status::DryRun, "{survey:?}");
    let value = json(&survey);
    assert_eq!(value["in_use"], "not_checked");
    let default_row = row(&value);
    assert_eq!(default_row["eligible"], Value::Null, "{default_row}");
    assert_eq!(default_row["reason"], Value::Null, "{default_row}");
    let pytest = block(&value, "pytest-tmp");
    assert_eq!(pytest["eligible"], Value::Null, "{pytest}");
    assert_eq!(pytest["not_checked"]["count"], 2, "{pytest}");
    assert_eq!(pytest["kept"], Value::Array(Vec::new()), "{pytest}");
    assert_eq!(pytest["found"], 2, "{pytest}");
    let checked_pytest = block(&checked, "pytest-tmp");
    assert_eq!(
        checked_pytest["not_checked"],
        Value::Null,
        "{checked_pytest}"
    );
    assert_eq!(checked_pytest["eligible"]["count"], 1, "{checked_pytest}");

    // Nothing reads like a checked verdict.
    let text = survey.to_text(true);
    assert!(text.starts_with("in use: not checked"), "{text}");
    assert!(
        text.contains("  profile pytest-tmp: 2 found · 2 matched (in use not checked) est. "),
        "{text}"
    );
    assert!(!text.contains("eligible"), "{text}");
    assert!(text.contains("  not checked "), "{text}");

    assert_eq!(process_reads(&default_host), [], "no process is read");
    assert_read_only(&default_host);
}

/// A Cargo target: its process heuristics are in-use evidence too, so
/// without `--in-use` a target a visible Cargo uses is not checked, and
/// with it, kept.
#[test]
fn a_cargo_target_in_use_is_not_checked_without_the_flag() {
    let extra = "  /home/tester/p/Cargo.toml: {}\n  /home/tester/p/target: {CACHEDIR.TAG: \
                 {cachedir_tag: true}, debug: {.cargo-lock: {}, deps: {a: {size: 1M}}}}\n  \
                 /usr/bin/cargo: {}\n";
    let yaml = laptop_tmp(extra).replace(
        "processes:\n",
        "processes:\n  - {pid: 77, exe: /usr/bin/cargo, cwd: /home/tester/p, maps: [/usr/bin/cargo], \
         environ: [HOME=/home/tester], fds: []}\n",
    );
    let target = |in_use: bool| -> (Value, Vec<(Op, usize)>) {
        let host = host(&yaml);
        let options = SurveyOptions {
            in_use,
            ..options(&["/home/tester"], Some("profile:cargo-target"))
        };
        let value = json(&run(&host, &options, None));
        (
            value["paths"][0]["candidates"][0].clone(),
            process_reads(&host),
        )
    };
    let (checked, reads) = target(true);
    // The model's Cargo is seen (in use), or its facts cannot be read
    // (unknown): a verdict on process evidence either way.
    assert!(
        matches!(
            checked["reason"].as_str(),
            Some("in_use" | "in_use_unknown")
        ),
        "{checked}"
    );
    assert!(!reads.is_empty());
    let (unchecked, reads) = target(false);
    assert_eq!(
        (unchecked["eligible"].clone(), unchecked["reason"].clone()),
        (Value::Null, Value::Null),
        "{unchecked}"
    );
    assert_eq!(reads, []);
}

/// Static refusals stand without `--in-use`: a `protect`ed candidate is
/// kept `protected`, a future-dated one `future_time` (under an idle
/// filter).
#[test]
fn static_refusals_stand_without_the_flag() {
    let host = host(&laptop_tmp(
        "  /tmp/tmp.Fut123: {.: {mtime: 2026-10-14T00:00:00Z}, f: {mtime: 2026-10-20T00:00:00Z}}\n",
    ));
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
    let options = SurveyOptions {
        in_use: false,
        ..options(&["/tmp"], Some("profile:shell-tmp && filter:idle-1h"))
    };
    let value = json(&run(&host, &options, Some(config)));
    let reasons: BTreeSet<(String, String)> = value["paths"][0]["candidates"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| {
            (
                r["candidate"].as_str().unwrap().to_owned(),
                r["reason"].as_str().unwrap_or("-").to_owned(),
            )
        })
        .collect();
    assert!(
        reasons.contains(&("/tmp/tmp.Xy12ab".into(), "protected".into())),
        "{reasons:?}"
    );
    assert!(
        reasons.contains(&("/tmp/tmp.Fut123".into(), "future_time".into())),
        "{reasons:?}"
    );
    assert!(
        reasons.contains(&("/tmp/tmp.Qw34er".into(), "-".into())),
        "{reasons:?}"
    );
    assert_eq!(process_reads(&host), []);
}

/// `ensure --dry-run` cannot plan without the in-use check: it has no way
/// to ask (the option is made only by the survey; a compile-fail doc test
/// on `diskgc_core::survey::InUseNotChecked` shows nothing else can make
/// it), and on the fixture where a survey without `--in-use` reads no
/// process, the dry run scans and keeps the live session `in_use`.
#[test]
fn ensure_dry_run_always_checks_in_use() {
    use diskgc_core::invocation::{self, EnsureOptions};
    use diskgc_core::units::parse_size;
    let host = host(&laptop_tmp(""));
    let options = EnsureOptions {
        min_free: parse_size("1K").expect("a size"),
        target_free: None,
        timeout: Some(Duration::from_secs(600)),
        tally_budget: None,
        select: Some("profile:pytest-tmp".to_owned()),
        roots: Vec::new(),
        dry_run: true,
        allow_root: false,
        no_index: true,
        path: PathBuf::from("/tmp"),
        test_entry_budget: None,
    };
    let draw_seed = || Err("no seed".to_owned());
    let builtins = || Library::builtin().map_err(|e| e.to_string());
    let platform = Platform {
        catalog: Catalog::Development,
        draw_seed: &draw_seed,
        builtins: &builtins,
    };
    let config = ConfigFile::NotFound { tried: Vec::new() };
    let loaded = invocation::load(options.selection(), &config, &platform, &mut |_| {})
        .unwrap_or_else(|(c, why)| panic!("{c:?}: {why}"));
    let report = invocation::ensure(&host, &options, loaded, &platform, &mut |_| {}, &mut |_| {});
    assert_eq!(report.status, Status::DryRun, "{report:?}");
    let live = report
        .plan
        .iter()
        .find(|r| {
            r.candidate.as_deref()
                == Some("/tmp/nix-shell.Ab12Cd/pytest-of-tester/pytest-1".as_ref())
        })
        .expect("the live session's row");
    assert_eq!(live.reason, Some(diskgc_core::engine::Reason::InUse));
    assert!(host.trace_count(Op::Pids) > 0, "the dry run scanned");
    let json: Value = serde_json::from_str(&report.to_json()).expect("JSON");
    assert!(json.get("in_use").is_none(), "ensure's report is unchanged");
    assert!(json["summary"][0].get("not_checked").is_none());
}

/// A project contract's file-based rules stand without `--in-use`: a
/// virtual environment 60 days old in a project whose source changed
/// yesterday is kept `not_idle` by the contract (no process is read),
/// and one in a project idle 60 days is not checked.
#[test]
fn a_project_contracts_file_rules_stand_without_the_flag() {
    let venv = |name: &str, source: &str| {
        format!(
            "  /home/tester/src/{name}:\n    .: {{mtime: 2026-08-16T03:00:00Z}}\n    \
             pyproject.toml: {{mtime: {source}}}\n    \".venv\":\n      .: {{mtime: \
             2026-08-16T03:00:00Z}}\n      pyvenv.cfg: {{mtime: 2026-08-16T03:00:00Z}}\n      \
             lib: {{.: {{mtime: 2026-08-16T03:00:00Z}}, x.py: {{size: 4K, mtime: \
             2026-08-16T03:00:00Z}}}}\n"
        )
    };
    let extra = venv("busy", "2026-10-14T03:00:00Z") + &venv("idle", "2026-08-16T03:00:00Z");
    let host = host(&laptop_tmp(&extra));
    let options = SurveyOptions {
        in_use: false,
        ..options(&["/home/tester/src"], Some("profile:python-venv"))
    };
    let value = json(&run(&host, &options, None));
    let reason = |dir: &str| -> Value {
        value["paths"][0]["candidates"]
            .as_array()
            .expect("rows")
            .iter()
            .find(|r| r["candidate"] == format!("/home/tester/src/{dir}/.venv"))
            .map(|r| r["reason"].clone())
            .unwrap_or_else(|| panic!("no row for {dir}: {value}"))
    };
    assert_eq!(reason("busy"), "not_idle", "{value}");
    assert_eq!(reason("idle"), Value::Null, "{value}");
    assert_eq!(process_reads(&host), []);
}

/// A union in survey's default mode (TASK-337): a candidate both terms
/// find stays under the first term whose own judgement admits it, not
/// checked; it is not handed on to a later term for want of a scan. A
/// term its filter refuses hands it on, as with `--in-use`.
#[test]
fn a_union_candidate_stays_with_its_first_admitting_term_without_the_flag() {
    let extra = "  /tmp/a-0: {.: {mtime: 2026-10-01T00:00:00Z}, f: {size: 8K, mtime: \
                 2026-10-01T00:00:00Z}}\n  /tmp/b-1: {.: {mtime: 2026-10-01T00:00:00Z}, f: \
                 {size: 4K, mtime: 2026-10-01T00:00:00Z}}\n";
    let config = r#"version: 1
profiles:
  - name: gen-a
    find: {roots: [/tmp], paths: ["a-*"], type: dir}
    action: delete
    examples: {match: [/tmp/a-1]}
  - name: gen-ab
    find: {roots: [/tmp], paths: ["a-*", "b-*"], type: dir}
    action: delete
    examples: {match: [/tmp/a-1, /tmp/b-1]}
  - name: has-tag
    where: {has: CACHEDIR.TAG}
"#;
    let rows = |select: &str, in_use: bool| -> Vec<(String, String, Value)> {
        let host = host(&laptop_tmp(extra));
        let options = SurveyOptions {
            in_use,
            ..options(&["/tmp"], Some(select))
        };
        let survey = run(&host, &options, Some(config));
        assert_eq!(survey.status, Status::DryRun, "{survey:?}");
        if !in_use {
            assert_eq!(process_reads(&host), [], "no process is read");
        }
        assert_read_only(&host);
        let value = json(&survey);
        let mut rows: Vec<_> = value["paths"][0]["candidates"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|r| {
                (
                    r["candidate"].as_str().expect("a path").to_owned(),
                    r["profile"].as_str().expect("a profile").to_owned(),
                    r["eligible"].clone(),
                )
            })
            .collect();
        rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        rows
    };
    let row = |candidate: &str, profile: &str, eligible: Value| {
        (candidate.to_owned(), profile.to_owned(), eligible)
    };
    let union = "profile:gen-a || profile:gen-ab";
    assert_eq!(
        rows(union, false),
        [
            row("/tmp/a-0", "gen-a", Value::Null),
            row("/tmp/b-1", "gen-ab", Value::Null),
        ]
    );
    assert_eq!(
        rows(union, true),
        [
            row("/tmp/a-0", "gen-a", true.into()),
            row("/tmp/b-1", "gen-ab", true.into()),
        ]
    );
    let filtered = "(profile:gen-a && filter:has-tag) || profile:gen-ab";
    assert_eq!(
        rows(filtered, false),
        [
            row("/tmp/a-0", "gen-ab", Value::Null),
            row("/tmp/b-1", "gen-ab", Value::Null),
        ]
    );
}

/// The kept reason and profile of each row of `value`'s first path
/// whose candidate starts with `prefix`, by candidate.
fn verdicts(value: &Value, prefix: &str) -> Vec<(String, String, Value)> {
    let mut rows: Vec<_> = value["paths"][0]["candidates"]
        .as_array()
        .expect("rows")
        .iter()
        .filter(|r| {
            r["candidate"]
                .as_str()
                .is_some_and(|c| c.starts_with(prefix))
        })
        .map(|r| {
            (
                r["candidate"].as_str().expect("a path").to_owned(),
                r["profile"].as_str().expect("a profile").to_owned(),
                r["reason"].clone(),
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// TASK-343 (1): a union's liveness contract vetoes by its file rules in
/// survey's default mode too. `python-venv || any-venv` (and the other
/// order): a 60-day-old `.venv` in a project whose `pyproject.toml`
/// changed yesterday is kept `not_idle` by python-venv's contract with
/// `--in-use`, whichever term the union selected, and so it must be
/// without it: the veto's project walk and its rules 1 and 2 read files,
/// no process. The venv of an idle project stays not checked.
#[test]
fn a_unions_contract_file_rules_veto_without_the_flag() {
    let venv = |name: &str, source: &str| {
        format!(
            "  /home/tester/src/{name}:\n    .: {{mtime: 2026-08-16T03:00:00Z}}\n    \
             pyproject.toml: {{mtime: {source}}}\n    \".venv\":\n      .: {{mtime: \
             2026-08-16T03:00:00Z}}\n      pyvenv.cfg: {{mtime: 2026-08-16T03:00:00Z}}\n      \
             lib: {{.: {{mtime: 2026-08-16T03:00:00Z}}, x.py: {{size: 4K, mtime: \
             2026-08-16T03:00:00Z}}}}\n"
        )
    };
    let extra = venv("busy", "2026-10-14T03:00:00Z") + &venv("idle", "2026-08-16T03:00:00Z");
    let config = r#"version: 1
profiles:
  - name: any-venv
    find:
      roots: required
      paths: [.venv, "*/.venv", "*/*/.venv"]
      type: dir
    action: delete
    examples: {root: /home/x, match: [/home/x/a/.venv]}
"#;
    let survey_rows = |select: &str, in_use: bool| {
        let host = host(&laptop_tmp(&extra));
        let options = SurveyOptions {
            in_use,
            ..options(&["/home/tester/src"], Some(select))
        };
        let survey = run(&host, &options, Some(config));
        assert_eq!(survey.status, Status::DryRun, "{survey:?}");
        if !in_use {
            assert_eq!(process_reads(&host), [], "no process is read");
        }
        assert_read_only(&host);
        verdicts(&json(&survey), "/home/tester/src/")
    };
    for (select, idle_profile) in [
        ("profile:python-venv || profile:any-venv", "python-venv"),
        ("profile:any-venv || profile:python-venv", "any-venv"),
    ] {
        let busy = (
            "/home/tester/src/busy/.venv".to_owned(),
            "python-venv".to_owned(),
            Value::from("not_idle"),
        );
        let checked = survey_rows(select, true);
        assert_eq!(checked[0], busy, "`{select}` --in-use: {checked:?}");
        let unchecked = survey_rows(select, false);
        assert_eq!(
            unchecked,
            [
                busy,
                (
                    "/home/tester/src/idle/.venv".to_owned(),
                    idle_profile.to_owned(),
                    Value::Null
                ),
            ],
            "`{select}` without --in-use"
        );
    }
}

/// TASK-343 (2): pytest's file rules stand in survey's default mode. Old
/// sessions whose `.lock` is no pid marker (empty, a leading zero, two
/// numbers, too long, not digits, a directory, a symlink: the decision
/// table's row 3) are `producer_unknown` with `--in-use`, and so without
/// it, alone and in a union whose other term the union selects (the
/// contract vetoes); so is a session in a `pytest-of` directory of a uid
/// diskgc does not judge for (a check before the table). A session whose
/// `.lock` holds a pid needs Owner PID (rows 4 and 5, processes): kept
/// `in_use` with `--in-use` (a process could use the temp root), not
/// checked without.
#[test]
fn pytests_file_rules_stand_without_the_flag() {
    let session = |n: usize, lock: &str| {
        format!(
            "  /tmp/pytest-of-tester/pytest-{n}: {{.: {{mtime: 2026-10-05T03:00:00Z}}, .lock: \
             {lock}}}\n"
        )
    };
    let old = "mtime: 2026-10-05T03:00:00Z";
    let malformed = [
        format!("{{content: \"\", {old}}}"),
        format!("{{content: \"0042\", {old}}}"),
        format!("{{content: \"42 43\", {old}}}"),
        format!("{{content: \"12345678901234567\", {old}}}"),
        format!("{{content: \"abc\", {old}}}"),
        format!("{{.: {{{old}}}}}"),
        format!("{{symlink: /tmp/x, {old}}}"),
    ];
    let mut extra = String::from("  /tmp/pytest-of-tester: {.: {mtime: 2026-10-05T03:00:00Z}}\n");
    for (n, lock) in malformed.iter().enumerate() {
        extra += &session(n, lock);
    }
    extra += &session(7, &format!("{{content: \"4321\", {old}}}"));
    // Before the table: P belongs to a uid diskgc does not judge for. The
    // session has no .lock and shares its temp root with the others, so
    // without --in-use only that owner rule can keep it.
    extra += "  /tmp/pytest-of-other: {.: {uid: 1001, mtime: 2026-10-05T03:00:00Z}}\n  \
              /tmp/pytest-of-other/pytest-0: {.: {mtime: 2026-10-05T03:00:00Z}}\n";
    let config = r#"version: 1
profiles:
  - name: any-session
    find: {roots: [/tmp], paths: ["pytest-of-*/pytest-*"], type: dir}
    checkouts: allow
    action: delete
    examples: {match: [/tmp/pytest-of-x/pytest-1]}
"#;
    let survey_rows = |select: &str, in_use: bool| {
        let host = host(&laptop_tmp(&extra));
        let options = SurveyOptions {
            in_use,
            ..options(&["/tmp"], Some(select))
        };
        let survey = run(&host, &options, Some(config));
        assert_eq!(survey.status, Status::DryRun, "{survey:?}");
        if !in_use {
            assert_eq!(process_reads(&host), [], "no process is read");
        }
        assert_read_only(&host);
        let value = json(&survey);
        verdicts(&value, "/tmp/pytest-of-")
    };
    let row =
        |path: &str, profile: &str, reason: Value| (path.to_owned(), profile.to_owned(), reason);
    let unknown = Value::from("producer_unknown");
    for (select, pid_lock_profile) in [
        ("profile:pytest-tmp", "pytest-tmp"),
        ("profile:any-session || profile:pytest-tmp", "any-session"),
    ] {
        let expected = |pid_lock: (&str, Value)| {
            let mut rows = vec![row(
                "/tmp/pytest-of-other/pytest-0",
                "pytest-tmp",
                unknown.clone(),
            )];
            rows.extend((0..7).map(|n| {
                row(
                    &format!("/tmp/pytest-of-tester/pytest-{n}"),
                    "pytest-tmp",
                    unknown.clone(),
                )
            }));
            rows.push(row(
                "/tmp/pytest-of-tester/pytest-7",
                pid_lock.0,
                pid_lock.1,
            ));
            rows
        };
        assert_eq!(
            survey_rows(select, true),
            expected(("pytest-tmp", Value::from("in_use"))),
            "`{select}` --in-use"
        );
        assert_eq!(
            survey_rows(select, false),
            expected((pid_lock_profile, Value::Null)),
            "`{select}` without --in-use"
        );
    }
}

/// TASK-343, the toolchain guard's static half: a custom toolchain (a
/// name rustup cannot install again) is kept `pinned_toolchain` by the
/// guarded term's place rule in a union with a guard-free term, in both
/// orders, with and without `--in-use`: the rule is a retention guard
/// the union applies without a scan. A reinstallable toolchain nothing
/// pins is not checked without `--in-use` (the live half needs one).
#[test]
fn a_unions_toolchain_place_rule_vetoes_without_the_flag() {
    let extra = "  /home/tester/.rustup/settings.toml: {content: \"version = \\\"12\\\"\\n\"}\n  \
                 /home/tester/.rustup/toolchains:\n    my-custom: {.: {mtime: \
                 2026-09-01T00:00:00Z}, bin: {.: {mtime: 2026-09-01T00:00:00Z}, rustc: {size: \
                 8K, mode: \"0755\", mtime: 2026-09-01T00:00:00Z, atime: \
                 2026-09-01T00:00:00Z}}}\n    1.89.0-x86_64-unknown-linux-gnu: {.: {mtime: \
                 2026-09-01T00:00:00Z}, bin: {.: {mtime: 2026-09-01T00:00:00Z}, rustc: {size: \
                 8K, mode: \"0755\", mtime: 2026-09-01T00:00:00Z, atime: \
                 2026-09-01T00:00:00Z}}}\n";
    let config = r#"version: 1
profiles:
  - name: toolchains
    find: {roots: required, paths: [.rustup/toolchains/*], type: dir}
    where: {protect_toolchains: rustup}
    action: delete
    examples: {root: /home/x, match: [/home/x/.rustup/toolchains/1.90.0-x86_64-unknown-linux-gnu]}
  - name: generic
    find: {roots: required, paths: [.rustup/toolchains/*], type: dir}
    action: delete
    examples: {root: /home/x, match: [/home/x/.rustup/toolchains/1.90.0-x86_64-unknown-linux-gnu]}
"#;
    let survey_rows = |select: &str, in_use: bool| {
        let host = host(&laptop_tmp(extra));
        let options = SurveyOptions {
            in_use,
            ..options(&["/home/tester"], Some(select))
        };
        let survey = run(&host, &options, Some(config));
        assert_eq!(survey.status, Status::DryRun, "{survey:?}");
        if !in_use {
            assert_eq!(process_reads(&host), [], "no process is read");
        }
        verdicts(&json(&survey), "/home/tester/.rustup/toolchains/")
    };
    let custom = (
        "/home/tester/.rustup/toolchains/my-custom".to_owned(),
        "toolchains".to_owned(),
        Value::from("pinned_toolchain"),
    );
    for (select, free_profile) in [
        ("profile:toolchains || profile:generic", "toolchains"),
        ("profile:generic || profile:toolchains", "generic"),
    ] {
        let checked = survey_rows(select, true);
        assert_eq!(checked[1], custom, "`{select}` --in-use: {checked:?}");
        assert_eq!(
            survey_rows(select, false),
            [
                (
                    "/home/tester/.rustup/toolchains/1.89.0-x86_64-unknown-linux-gnu".to_owned(),
                    free_profile.to_owned(),
                    Value::Null
                ),
                custom.clone(),
            ],
            "`{select}` without --in-use"
        );
    }
}

/// TASK-367 (Codex's second review): survey reports what the reclaim
/// would take, so a build the nested-build guard vetoes, and an old build
/// nested in it whatever root or profile reaches it, is no candidate row,
/// eligible or not. `.built` in the root vetoes the recently changed CMake
/// build `pkg/out`; below it an old CMake build (its source `out/a`) and an
/// old Meson build (`out/a/b/inner`, its source `out/a` too) lie past
/// their own four directories; the roots are the root and `out/a`. The
/// positive control: a standalone old CMake build is an eligible row.
#[test]
fn survey_reports_no_build_inside_a_vetoed_one_as_a_candidate() {
    let src = "/home/tester/src";
    let cmake = |source: &str, mtime: &str| {
        format!(
            "{{.: {{mtime: {mtime}}}, CMakeCache.txt: {{mtime: {mtime}, content: \"CMAKE_HOME_DIRECTORY:INTERNAL={source}\\n\"}}, CMakeFiles: {{.: {{mtime: {mtime}}}}}, a.o: {{mtime: {mtime}, size: 8K}}}}"
        )
    };
    let inner = format!("{src}/pkg/out/a/b/inner");
    let meson_info = format!(
        "{{\\\"directories\\\":{{\\\"source\\\":\\\"{src}/pkg/out/a\\\",\\\"build\\\":\\\"{inner}\\\",\\\"info\\\":\\\"{inner}/meson-info\\\"}},\\\"error\\\":false}}"
    );
    let old = "2026-10-01T00:00:00Z";
    let extra = format!(
        "  {src}/.built: {{mtime: {old}}}\n  {src}/pkg/CMakeLists.txt: {{content: \"project(p)\\n\"}}\n  {src}/pkg/out: {}\n  {src}/pkg/out/a/CMakeLists.txt: {{content: \"project(q)\\n\"}}\n  {src}/pkg/out/a/meson.build: {{content: \"project('q')\\n\"}}\n  {src}/pkg/out/a/cm: {}\n  {inner}: {{.: {{mtime: {old}}}, build.ninja: {{mtime: {old}}}, meson-private: {{.: {{mtime: {old}}}, coredata.dat: {{mtime: {old}, content: coredata}}}}, meson-info: {{.: {{mtime: {old}}}, meson-info.json: {{mtime: {old}, content: \"{meson_info}\"}}}}, a.o: {{mtime: {old}, size: 8K}}}}\n  /home/tester/alone/CMakeLists.txt: {{content: \"project(s)\\n\"}}\n  /home/tester/alone/out: {}\n",
        cmake(&format!("{src}/pkg"), "2026-10-15T02:00:00Z"),
        cmake(&format!("{src}/pkg/out/a"), old),
        cmake("/home/tester/alone", old),
    );
    let host = host(&laptop_tmp(&extra));
    let mut options = options(
        &["/home/tester"],
        Some("profile:cmake-build-anchored || profile:meson-build-anchored"),
    );
    options.roots = vec![
        src.to_owned(),
        format!("{src}/pkg/out/a"),
        "/home/tester/alone".to_owned(),
    ];
    let value = json(&run(&host, &options, None));
    let rows = value["paths"][0]["candidates"].as_array().expect("rows");
    let named: Vec<&str> = rows
        .iter()
        .filter_map(|r| r["candidate"].as_str())
        .collect();
    for vetoed in [
        format!("{src}/pkg/out"),
        format!("{src}/pkg/out/a/cm"),
        inner.clone(),
    ] {
        assert!(!named.contains(&vetoed.as_str()), "{vetoed}: {value}");
    }
    let alone = rows
        .iter()
        .find(|r| r["candidate"] == "/home/tester/alone/out")
        .unwrap_or_else(|| panic!("positive control: no row for the standalone build: {value}"));
    assert_eq!(alone["eligible"], true, "{alone}");
    assert_read_only(&host);
}

/// TASK-367, Codex's third review, case 3: a survey of two PATHs, the
/// tree's top and a directory below it, with roots `R` and `R/pkg` and a
/// stamp in `R`. Survey plans each PATH with the roots below it, so the
/// narrower PATH saw only `R/pkg`, and a guard bounded by the roots
/// reported the vetoed build eligible there. The guard's climb is a
/// function of the path alone: no PATH reports it.
#[test]
fn a_narrower_survey_path_does_not_report_a_vetoed_build() {
    let src = "/home/tester/src";
    let old = "2026-10-01T00:00:00Z";
    let extra = format!(
        "  {src}/.built: {{mtime: {old}}}\n  {src}/pkg/CMakeLists.txt: {{content: \"project(p)\\n\"}}\n  {src}/pkg/out: {{.: {{mtime: {old}}}, CMakeCache.txt: {{mtime: {old}, content: \"CMAKE_HOME_DIRECTORY:INTERNAL={src}/pkg\\n\"}}, CMakeFiles: {{.: {{mtime: {old}}}}}, a.o: {{mtime: {old}, size: 8K}}}}\n"
    );
    let host = host(&laptop_tmp(&extra));
    let mut options = options(
        &[src, &format!("{src}/pkg")],
        Some("profile:cmake-build-anchored"),
    );
    options.roots = vec![src.to_owned(), format!("{src}/pkg")];
    let value = json(&run(&host, &options, None));
    let paths = value["paths"].as_array().expect("paths");
    assert_eq!(paths.len(), 2, "{value}");
    for path in paths {
        let rows = path["candidates"].as_array().expect("rows");
        assert!(
            rows.iter()
                .all(|r| r["candidate"] != format!("{src}/pkg/out").as_str()),
            "{}: reports the vetoed build: {path}",
            path["path"]
        );
    }
    assert_read_only(&host);
}
