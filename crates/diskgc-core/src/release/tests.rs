use serde_json::{Value, json};

use super::*;
use crate::invocation::Catalog;
use crate::profile::Library;

#[test]
fn the_embedded_descriptor_keeps_its_rules() {
    descriptor().unwrap_or_else(|e| panic!("{e}"));
}

/// The 0.1 row reproduces what v0.1.0 shipped, read from the tag
/// (`git show v0.1.0:flake.nix`, installShippedFiles and
/// mkReleasePackage; `git show v0.1.0:crates/diskgc/src/main.rs`, NOT_YET;
/// its builtin.yaml's `releases: ["0.1"]`). The literals are the point: a
/// tagged release's row never changes, so an edit of it fails here.
#[test]
fn the_0_1_row_is_what_v0_1_0_shipped() {
    let row = release("0.1").expect("0.1");
    assert_eq!(row.release, Some("0.1"));
    assert_eq!(row.tag.as_deref(), Some("v0.1.0"));
    assert_eq!(
        row.commands,
        [Command::Ensure, Command::Check, Command::Profiles],
        "0.1.0 refused every `watch` and `survey`"
    );
    assert_eq!(row.commands_phrase(), "ensure, check and profiles");
    assert_eq!(row.catalog, Some(Release::V0_1));
    assert!(
        row.command_actions.is_empty(),
        "0.1.0 ran no command action"
    );
    assert_eq!(
        row.ships,
        [
            "lib/tmpfiles.d/diskgc.conf",
            "share/doc/diskgc/examples/example.yaml",
            "share/doc/diskgc/README.md",
            "share/doc/diskgc/reference.md",
            "share/doc/diskgc/safety.md",
            "share/doc/diskgc/RELEASE-NOTES.md",
        ]
    );
    let units = &descriptor().expect("descriptor").watch_units;
    assert!(units.iter().all(|u| !row.ships.contains(u)));
    let files = &descriptor().expect("descriptor").files;
    let sources: Vec<&FileSource> = row.ships.iter().map(|p| &files[p]).collect();
    assert_eq!(
        sources,
        [
            &FileSource::Copy("packaging/diskgc.conf".into()),
            &FileSource::Copy("docs/example.yaml".into()),
            &FileSource::Copy("packaging/README.md".into()),
            &FileSource::Copy("docs/reference.md".into()),
            &FileSource::Copy("docs/safety.md".into()),
            &FileSource::ReleaseNotes("docs/reference.md".into()),
        ]
    );
    // The catalog, through the same narrowing the binary uses, with the
    // release's name in what it refuses.
    let library = Catalog::from(row)
        .library(Library::builtin().expect("built-ins"))
        .expect("0.1 catalog");
    let names: Vec<&str> = library.profiles().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "cargo-target",
            "idle-10d",
            "idle-1d",
            "idle-1h",
            "idle-2d",
            "idle-30d",
            "idle-3d",
            "idle-7d",
            "nix-shell-rc",
            "pytest-tmp"
        ]
    );
    assert_eq!(library.release_name(), Some("0.1"));
    assert_eq!(
        library.not_shipped("pytest-cache").as_deref(),
        Some("`pytest-cache` is a built-in that diskgc 0.1 does not ship.")
    );
}

/// 0.2's catalog is 0.1's plus `nix-garbage` (Mark, 2026-11-04, TASK-335),
/// five of TASK-350's project-tree sources (Mark, 2026-11-05, TASK-359;
/// `node-modules` held out, Mark, 2026-11-06, TASK-365) and
/// four delegated cleaners (Mark, 2026-11-05, TASK-360; `jest-cache` left
/// after review), through the same
/// narrowing the binary uses; 0.1's stays without them.
#[test]
fn the_0_2_catalog_is_0_1_plus_nix_garbage_the_project_tree_caches_and_four_cleaners() {
    const ADDED: [&str; 10] = [
        "nix-garbage",
        "pytest-cache",
        "python-pycache",
        "mypy-cache",
        "python-venv",
        "node-compile-cache",
        "pip-cache",
        "uv-cache",
        "pnpm-store",
        "ccache",
    ];
    let names = |release: &str| -> Vec<String> {
        let library = Catalog::from(super::release(release).expect(release))
            .library(Library::builtin().expect("built-ins"))
            .unwrap_or_else(|e| panic!("{release} catalog: {e}"));
        assert_eq!(library.release_name(), Some(release));
        library.profiles().map(|p| p.name.clone()).collect()
    };
    let mut want = names("0.1");
    assert!(
        !want.iter().any(|n| ADDED.contains(&n.as_str())),
        "{want:?}"
    );
    want.extend(ADDED.iter().map(|n| (*n).to_owned()));
    want.sort();
    let got = names("0.2");
    assert!(!got.iter().any(|n| n == "node-modules"), "{got:?}");
    assert_eq!(got, want);
    assert_eq!(release("0.2").expect("0.2").catalog, Some(Release::V0_2));
}

/// Every release row builds its catalog (TASK-360): the release-catalog
/// check runs against the row's own `command_actions`, so a row that
/// reuses another's label must list the same command actions, and a
/// drift fails here, not when that binary starts.
#[test]
fn every_release_row_builds_its_catalog() {
    for name in descriptor().expect("descriptor").releases.keys() {
        Catalog::from(release(name).expect(name))
            .library(Library::builtin().expect("built-ins"))
            .unwrap_or_else(|e| panic!("release {name}: {e}"));
    }
}

/// Every release-catalog build has a row: the version in Cargo.toml picks
/// it (flake.nix picks the same by `lib.versions.majorMinor`).
#[test]
fn this_version_has_a_release_row() {
    let row = release_of_version(env!("CARGO_PKG_VERSION")).unwrap_or_else(|e| panic!("{e}"));
    assert!(row.release.is_some());
    assert!(row.catalog.is_some());
}

#[test]
fn a_version_names_its_release_by_major_and_minor() {
    assert_eq!(
        release_of_version("0.2.0-dev").unwrap().release,
        Some("0.2")
    );
    assert_eq!(release_of_version("0.1.0").unwrap().release, Some("0.1"));
    assert_eq!(release_of_version("0.1.7").unwrap().release, Some("0.1"));
    let missing = release_of_version("9.4.0").expect_err("no row");
    assert_eq!(missing, "diskgc 9.4.0: releases.json has no release 9.4");
    for bad in ["", "0", "x.1.0", "0.x"] {
        assert!(release_of_version(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn the_development_row_runs_every_command_and_every_built_in() {
    let row = development().expect("development");
    assert_eq!(row.release, None);
    assert_eq!(row.catalog, None);
    for command in Command::ALL {
        assert!(row.allows(command), "{command:?}");
    }
    assert_eq!(
        row.commands_phrase(),
        "ensure, watch, check, profiles and survey"
    );
    let units = &descriptor().expect("descriptor").watch_units;
    assert!(units.iter().all(|u| row.ships.contains(u)));
}

/// The MIT license asks that its notice go with every copy, so every
/// package built from this tree ships the repository's LICENSE: the
/// development row and every release row not yet tagged. A tagged row
/// stays as it shipped (v0.1.0 predates the license; the test above pins
/// it), so a new release row that leaves the license out fails here.
#[test]
fn every_untagged_row_ships_the_license() {
    let descriptor = descriptor().expect("descriptor");
    let license = "share/doc/diskgc/LICENSE";
    assert_eq!(
        descriptor.files.get(license),
        Some(&FileSource::Copy("LICENSE".into()))
    );
    let rows = std::iter::once(("development", &descriptor.development)).chain(
        descriptor
            .releases
            .iter()
            .filter(|(_, b)| b.tag.is_none())
            .map(|(n, b)| (n.as_str(), b)),
    );
    for (name, build) in rows {
        assert!(
            build.ships.iter().any(|p| p == license),
            "{name} does not ship {license}"
        );
    }
}

fn embedded() -> Value {
    serde_json::from_str(EMBEDDED).expect("JSON")
}

fn refused(edit: impl FnOnce(&mut Value)) -> String {
    let mut value = embedded();
    edit(&mut value);
    Descriptor::parse(&value.to_string()).expect_err("an edit that breaks a rule")
}

/// Each rule bites on its own edit of the real file.
#[test]
fn each_rule_refuses_its_edit() {
    let units = |v: &mut Value, row: &str| {
        v["releases"][row]["ships"]
            .as_array_mut()
            .unwrap()
            .retain(|p| !p.as_str().unwrap().starts_with("lib/systemd/"));
    };
    let cases: Vec<(String, &str)> = vec![
        (
            refused(|v| units(v, "0.2")),
            "0.2: runs `watch` but ships no unit templates",
        ),
        (
            refused(|v| {
                v["releases"]["0.2"]["ships"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|p| p != "lib/systemd/user/diskgc-watch@.service")
            }),
            "ships some `watch` unit templates; all or none",
        ),
        (
            refused(|v| v["releases"]["0.2"]["commands"] = json!(["ensure"])),
            "ships the `watch` unit templates but runs no `watch`",
        ),
        (
            refused(|v| {
                v["releases"]["0.1"]["ships"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("bin/other"))
            }),
            "0.1: ships `bin/other`, which `files` does not name",
        ),
        (
            refused(|v| v["files"]["share/doc/extra"] = json!({"copy": "docs/extra"})),
            "`share/doc/extra` is shipped by no build",
        ),
        (
            refused(|v| v["files"]["lib/tmpfiles.d/diskgc.conf"] = json!({"copy": "../x"})),
            "source `../x` must be relative",
        ),
        (
            refused(|v| v["releases"]["0.2"]["catalog"] = Value::Null),
            "release 0.2: a `catalog` is required",
        ),
        (
            refused(|v| v["releases"]["0.2"]["tag"] = json!("v0.3.0")),
            "release 0.2: tag `v0.3.0` is not v0.2.PATCH",
        ),
        (
            refused(|v| v["development"]["catalog"] = json!("0.1")),
            "development: no `catalog`",
        ),
        (
            refused(|v| {
                v["releases"]["0.1"]["commands"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("check"))
            }),
            "0.1: `check` twice",
        ),
        (
            refused(|v| v["releases"]["0.1"]["commands"] = json!(["ensure", "prune"])),
            "unknown command `prune`, expected one of ensure, watch, watch --tally-only",
        ),
        (
            refused(|v| v["releases"]["0.1"]["since"] = json!("0.1")),
            "unknown field `since`",
        ),
        (
            refused(|v| v["releases"]["1"] = v["releases"]["0.1"].clone()),
            "release `1` is not named MAJOR.MINOR",
        ),
        (
            refused(|v| v["releases"]["0.1"]["commands"] = json!(["ensure", "profiles"])),
            "0.1: does not run `check`, which every row runs",
        ),
        (
            refused(|v| {
                v["development"]["ships"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("share/doc/diskgc/RELEASE-NOTES.md"))
            }),
            "development: ships `share/doc/diskgc/RELEASE-NOTES.md`, which is not a copied file",
        ),
        (
            refused(|v| v["watch_units"] = json!(["share/doc/diskgc/RELEASE-NOTES.md"])),
            "watch unit `share/doc/diskgc/RELEASE-NOTES.md` is not a copied file",
        ),
        (
            refused(|v| {
                v["development"]["command_actions"] =
                    json!({"pip-cache": {"command": ["pip"], "cwd": "candidate"}})
            }),
            "development: no `catalog` (it has every built-in), no `command_actions`",
        ),
        (
            refused(|v| {
                v["releases"]["0.2"]["command_actions"]["uv-cache"]["command"] = json!([" "])
            }),
            "0.2: command action of `uv-cache` has no program",
        ),
        (
            refused(|v| v["releases"]["0.2"]["command_actions"]["uv-cache"]["cwd"] = json!("home")),
            "unknown variant `home`, expected `candidate` or `parent`",
        ),
        (
            refused(|v| v["releases"]["0.2"]["command_actions"]["uv-cache"]["env"] = json!([])),
            "unknown field `env`",
        ),
    ];
    for (error, want) in cases {
        assert!(error.contains(want), "{error}\nwanted: {want}");
    }
    // The unedited file passes, so each refusal above is its edit's.
    Descriptor::parse(&embedded().to_string()).expect("the real file");
}

/// The reclaiming-watch decision (TASK-276) is one row's data: adding
/// `watch` to a release's commands is all it takes, and the phrase the
/// binary refuses and helps with follows.
#[test]
fn allowing_a_reclaiming_watch_is_a_one_row_change() {
    let mut value = embedded();
    value["releases"]["0.2"]["commands"]
        .as_array_mut()
        .unwrap()
        .push(json!("watch"));
    let edited = Descriptor::parse(&value.to_string()).expect("still valid");
    let row = &edited.releases["0.2"];
    assert!(row.allows(Command::Watch));
    assert_eq!(row.commands_phrase(), "ensure, watch, check and profiles");
}
