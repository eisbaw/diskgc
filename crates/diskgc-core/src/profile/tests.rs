//! The profile model: the shipped built-ins, the schema's static rules,
//! roots fixed vs required, `idle_for`, and action equality. Each rule has
//! a failing case.

use std::time::Duration;

use super::*;
use crate::doc::{code, code_blocks, reference_md, section, table};
use crate::engine::Checkouts;
use crate::host::Timestamp;
use crate::select::{CheckError, SelectError, compile};
use crate::units::format_duration;

/// The library with `yaml` as user profiles.
fn with_user(yaml: &str) -> Result<Library, LoadErrors> {
    let user = parse_profiles(yaml).unwrap_or_else(|e| panic!("{e}"));
    Library::with_user_profiles(&user)
}

/// The messages of a user profile list that must not load.
fn user_errors(yaml: &str) -> Vec<String> {
    match with_user(yaml) {
        Ok(_) => panic!("loaded:\n{yaml}"),
        Err(errors) => errors.0.into_iter().map(|e| e.to_string()).collect(),
    }
}

/// The YAML error of a profile list that must not parse.
fn yaml_error(yaml: &str) -> String {
    match parse_profiles(yaml) {
        Ok(specs) => panic!("parsed: {specs:?}"),
        Err(e) => e,
    }
}

fn assert_has(errors: &[String], needle: &str) {
    assert!(
        errors.iter().any(|e| e.contains(needle)),
        "no error contains {needle:?}: {errors:#?}"
    );
}

const BUILT_IN_FILTERS: [&str; 7] = [
    "idle-1h", "idle-1d", "idle-2d", "idle-3d", "idle-7d", "idle-10d", "idle-30d",
];

#[test]
fn app_not_running_is_a_validated_fs_source_predicate() {
    let source = |value: &str| {
        format!(
            "- name: browser\n  find: {{roots: [/tmp], paths: [cache], type: dir}}\n  where: {{app_not_running: '{value}'}}\n  action: delete\n  examples: {{match: [/tmp/cache]}}\n"
        )
    };
    let library =
        with_user(&source("/home/{uid}/.config/chromium/SingletonLock")).expect("valid lock path");
    with_user(&source(".config/chromium/SingletonLock")).expect("lock relative to each root");
    let Body::Source(browser) = &library.get("browser").unwrap().body else {
        panic!("source");
    };
    assert_eq!(
        browser.where_.predicates(),
        vec![PredicateName::AppNotRunning]
    );
    for path in [
        "../SingletonLock",
        "/tmp/other",
        "/tmp/../SingletonLock",
        "/tmp//SingletonLock",
    ] {
        assert_has(&user_errors(&source(path)), "app_not_running");
    }
    for value in ["", "~", "null", "NULL"] {
        let yaml = format!(
            "- name: browser\n  find: {{roots: [/tmp], paths: [cache], type: dir}}\n  where: {{app_not_running: {value}}}\n  action: delete\n"
        );
        assert!(
            parse_profiles(&yaml).is_err(),
            "present null app_not_running was accepted: {yaml}"
        );
    }
    assert_has(&user_errors(&source("")), "app_not_running");
    assert_has(
        &user_errors("- name: only-filter\n  where: {app_not_running: /tmp/SingletonLock}\n"),
        "source predicate",
    );
}

#[test]
fn lacks_entry_cannot_be_silently_disabled_by_a_present_null_or_blank() {
    let source = |value: &str| {
        format!(
            "- name: build\n  find: {{roots: [/tmp], paths: [build], type: dir}}\n  where: {{has: CMakeCache.txt, lacks_entry: {value}}}\n  action: delete\n  examples: {{match: [/tmp/build]}}\n"
        )
    };
    for value in ["", "~", "null", "NULL"] {
        let yaml = source(value);
        assert!(
            parse_profiles(&yaml).is_err(),
            "present null lacks_entry was accepted: {yaml}"
        );
    }
    assert_has(&user_errors(&source("''")), "lacks_entry");
    assert_has(&user_errors(&source("' '")), "lacks_entry");
    with_user(&source("CMakeLists.txt")).expect("valid source guard");
}

#[test]
fn has_dir_is_a_validated_source_guard() {
    let source = |value: &str| {
        format!(
            "- name: west\n  find: {{roots: [/tmp], paths: [build], type: dir}}\n  where: {{has: CMakeCache.txt, has_dir: {value}}}\n  action: delete\n  examples: {{match: [/tmp/build]}}\n"
        )
    };
    for value in ["", "~", "null", "NULL"] {
        let yaml = source(value);
        assert!(
            parse_profiles(&yaml).is_err(),
            "present null has_dir was accepted: {yaml}"
        );
    }
    for value in ["''", "' '", "a/b", ".."] {
        assert_has(&user_errors(&source(value)), "has_dir");
    }
    with_user(&source("zephyr")).expect("valid directory marker");
    assert_has(
        &user_errors("- name: only-filter\n  where: {has_dir: zephyr}\n"),
        "source predicate",
    );
}

#[test]
fn version_guards_cannot_be_silently_disabled_by_present_null() {
    for (field, valid) in [
        ("keep_newest", "2"),
        ("newest_group", "cursor_extension"),
        ("protect_parent_link", "current"),
        (
            "protect_cursor_extensions",
            ".cursor/extensions/extensions.json",
        ),
    ] {
        let source = |value: &str| {
            format!(
                "- name: versions\n  find: {{roots: [/tmp], paths: [versions/*], type: dir}}\n  where: {{{field}: {value}}}\n  action: delete\n  examples: {{match: [/tmp/versions/v1]}}\n"
            )
        };
        for null in ["", "~", "null", "NULL"] {
            let yaml = source(null);
            assert!(
                parse_profiles(&yaml).is_err(),
                "present null {field} was accepted: {yaml}"
            );
        }
        if field != "newest_group" {
            with_user(&source(valid)).expect("valid version guard");
        }
    }
}

#[test]
fn example_root_is_absolute_and_only_for_supplied_roots() {
    let source = |roots: &str, root: &str| {
        format!(
            "- name: build\n  find: {{roots: {roots}, paths: [build], type: dir}}\n  action: delete\n  examples: {{root: {root}, match: [/tmp/build]}}\n"
        )
    };
    with_user(&source("required", "/tmp")).expect("supplied example root");
    for value in ["", "~", "null", "NULL"] {
        assert!(parse_profiles(&source("required", value)).is_err());
    }
    assert_has(
        &user_errors(&source("required", "relative")),
        "example root",
    );
    assert_has(
        &user_errors(&source("[/tmp]", "/tmp")),
        "only for `find.roots: required`",
    );
}

// ---- AC #3: the shipped built-ins load, and a broken one fails the gate

/// AC #3. The embedded YAML passes every static rule and holds the wave-1
/// library: `cargo-target` and the `idle-*` filters. This test is what
/// makes a broken built-in fail `just check` and the flake's
/// workspace-tests (the source scan forbids build scripts, task-50).
#[test]
fn the_built_in_library_loads() {
    let library = Library::builtin().unwrap_or_else(|e| panic!("broken built-in profile:\n{e}"));
    let names: Vec<&str> = library.profiles().map(|p| p.name.as_str()).collect();
    let mut want: Vec<&str> = BUILT_IN_FILTERS.to_vec();
    want.extend([
        "brave-browser-cache",
        "bun-cache",
        "cargo-target",
        "autotools-build-anchored",
        "cmake-build-anchored",
        "meson-build-anchored",
        "containers-image-tmp",
        "chrome-browser-cache",
        "chromium-browser-cache",
        "edge-browser-cache",
        "claude-code-versions",
        "codex-releases",
        "cursor-agent-versions",
        "cursor-extensions",
        "go-mod-cache",
        "nix-user-cache",
        "pytest-cache",
        "platformio-cache",
        "pytest-tmp",
        "claude-code-tmp",
        "chromium-tmp",
        "node-compile-cache",
        "playwright-tmp",
        "zephyr-tmp",
        "renode-tmp",
        "west-build-anchored",
        "xdg-cache-entry-v2",
        "ccache",
        "go-build-cache",
        "docker-build-cache",
        "docker-dangling",
        "docker-unreferenced",
        "jest-cache",
        "npm-cache",
        "pip-cache",
        "python-pycache",
        "mypy-cache",
        "pnpm-store",
        "uv-cache",
        "yarn-cache",
    ]);
    want.sort_unstable();
    assert_eq!(names, want);
    assert!(library.profiles().all(|p| p.origin == Origin::BuiltIn));

    let cargo = library.get("cargo-target").expect("cargo-target");
    let Body::Source(source) = &cargo.body else {
        panic!("cargo-target is not a source");
    };
    assert_eq!(source.kind(), Kind::CargoTarget);
    assert_eq!(source.roots, Roots::Required);
    assert_eq!(source.action, Action::CargoLockProtocol);
    assert!(source.where_.is_empty());
    // Cargo output can contain a checkout made by a build script.
    assert_eq!(source.checkouts, Checkouts::Allow);

    let go = library.get("go-mod-cache").expect("go-mod-cache");
    let Body::Source(go) = &go.body else {
        panic!("go-mod-cache is not a source");
    };
    assert_eq!(go.kind(), Kind::Fs);
    assert_eq!(go.roots, Roots::Required);
    assert_eq!(
        go.action,
        Action::Delete {
            make_writable: true
        }
    );
    let Body::Source(pytest) = &library.get("pytest-tmp").expect("pytest-tmp").body else {
        panic!("pytest-tmp is a source");
    };
    assert_eq!(pytest.checkouts, Checkouts::Allow);
}

#[test]
fn retired_marker_only_build_profiles_fail_selection() {
    let library = Library::builtin().expect("built-ins");
    for retired in ["cmake-build", "west-build"] {
        assert!(library.get(retired).is_none(), "{retired} is still shipped");
        let expression = format!("profile:{retired}");
        let error = compile(&expression, &library)
            .expect_err("a retired deletion profile must fail before scanning")
            .to_string();
        assert!(
            error.contains(&format!("no profile or filter is named `{retired}`")),
            "{expression}: {error}"
        );
    }
}

/// A command profile's paths and argv form one contract: the cleaner must
/// receive the exact directory the finder measured. A changed path or
/// missing `{path}` needs a new built-in name and fresh verification.
#[test]
fn command_cache_builtins_have_frozen_paths_and_cleaners() {
    let library = Library::builtin().expect("built-ins");
    for (name, paths, argv, cwd) in [
        (
            "pip-cache",
            vec![".cache/pip"],
            vec!["pip", "--cache-dir", "{path}", "cache", "purge"],
            Cwd::Candidate,
        ),
        (
            "uv-cache",
            vec![".cache/uv"],
            vec!["uv", "cache", "clean", "--cache-dir", "{path}"],
            Cwd::Parent,
        ),
        (
            "go-build-cache",
            vec![".cache/go-build"],
            vec!["env", "GOCACHE={path}", "go", "clean", "-cache"],
            Cwd::Candidate,
        ),
        (
            "npm-cache",
            vec![".npm"],
            vec!["npm", "cache", "clean", "--force", "--cache", "{path}"],
            Cwd::Candidate,
        ),
        (
            "pnpm-store",
            vec![".local/share/pnpm/store"],
            vec!["pnpm", "store", "prune", "--store-dir", "{path}"],
            Cwd::Candidate,
        ),
        (
            "yarn-cache",
            vec![".cache/yarn"],
            vec!["env", "YARN_CACHE_FOLDER={path}", "yarn", "cache", "clean"],
            Cwd::Parent,
        ),
        (
            "ccache",
            vec![".cache/ccache", ".ccache"],
            vec!["env", "CCACHE_DIR={path}", "ccache", "--clear"],
            Cwd::Candidate,
        ),
    ] {
        let Body::Source(source) = &library.get(name).expect("shipped cache").body else {
            panic!("{name} is not a source");
        };
        assert_eq!(source.roots, Roots::Required, "{name}");
        assert_eq!(source.checkouts, Checkouts::Refuse, "{name}");
        assert_eq!(
            source.finds,
            Finds::Paths {
                patterns: paths.into_iter().map(str::to_owned).collect(),
                exclude: Vec::new(),
                entry_type: EntryType::Dir,
            },
            "{name}"
        );
        assert_eq!(
            source.action,
            Action::Command {
                argv: argv.into_iter().map(str::to_owned).collect(),
                cwd: Some(cwd),
            },
            "{name}"
        );
    }
}

/// The cache deletion units and their application locks are fixed under each
/// built-in name. Changing a path needs a new profile name, including when a
/// browser changes where it keeps its profile data.
#[test]
fn browser_cache_builtins_have_frozen_paths_and_locks() {
    let library = Library::builtin().expect("built-ins");
    for (name, cache, config) in [
        (
            "brave-browser-cache",
            "BraveSoftware/Brave-Browser",
            "BraveSoftware/Brave-Browser",
        ),
        ("chrome-browser-cache", "google-chrome", "google-chrome"),
        ("chromium-browser-cache", "chromium", "chromium"),
        ("edge-browser-cache", "microsoft-edge", "microsoft-edge"),
    ] {
        let Body::Source(source) = &library.get(name).expect("shipped browser cache").body else {
            panic!("{name} must be a source")
        };
        assert_eq!(source.roots, Roots::Required, "{name}");
        assert_eq!(
            source.action,
            Action::Delete {
                make_writable: false
            },
            "{name}"
        );
        assert_eq!(source.checkouts, Checkouts::Refuse, "{name}");
        assert_eq!(
            source.where_.app_not_running.as_deref(),
            Some(format!(".config/{config}/SingletonLock").as_str()),
            "{name}"
        );
        assert_eq!(
            source.finds,
            Finds::Paths {
                patterns: [
                    format!(".cache/{cache}/Default/Cache"),
                    format!(".cache/{cache}/Default/Code Cache"),
                    format!(".cache/{cache}/Profile */Cache"),
                    format!(".cache/{cache}/Profile */Code Cache"),
                ]
                .to_vec(),
                exclude: Vec::new(),
                entry_type: EntryType::Dir,
            },
            "{name}"
        );
    }
}

/// Built-in examples are checked as patterns, including negative paths.
/// `roots: required` examples cannot assert a relative depth: the caller
/// chooses the root, so the model golden fixture pins depths separately.
#[test]
fn built_in_fs_examples_hold() {
    let library = Library::builtin().expect("built-ins");
    let failures = super::examples::failures(&library);
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Every `idle-*` built-in requires exactly what its name says.
#[test]
fn idle_filter_names_say_their_duration() {
    let library = Library::builtin().expect("built-ins");
    for name in BUILT_IN_FILTERS {
        let Some(Profile {
            body: Body::Filter(where_),
            ..
        }) = library.get(name)
        else {
            panic!("{name} is not a filter");
        };
        let idle = where_.idle_for.expect("idle_for");
        assert_eq!(format!("idle-{}", format_duration(idle.0)), name);
        assert_eq!(where_.predicates(), vec![PredicateName::IdleFor], "{name}");
    }
}

/// Every shipped built-in is a row of docs/reference.md's library table,
/// in the category it says.
#[test]
fn built_ins_are_in_the_reference_table() {
    let library_md = section(&reference_md(), "### Built-in profile library");
    let rows = table(&library_md, "| Category | Profile |");
    let documented: Vec<(&str, &str)> = rows
        .iter()
        .flat_map(|row| row[1].split(", ").map(|name| (row[0].as_str(), code(name))))
        .collect();
    for profile in Library::builtin().expect("built-ins").profiles() {
        let category = profile.category.as_deref().expect("built-ins have one");
        assert!(
            documented.contains(&(category, profile.name.as_str())),
            "built-in `{}` (category {category}) is not in the reference table",
            profile.name
        );
    }
}

/// task-80: docs/reference.md's library table is the one home for which
/// built-ins allow checkouts. For every shipped built-in source, its row
/// says `checkouts: allow` exactly when the profile does. (Rows of
/// profiles not shipped yet are checked when they ship.)
#[test]
fn the_reference_table_says_which_built_ins_allow_checkouts() {
    let library_md = section(&reference_md(), "### Built-in profile library");
    let rows = table(&library_md, "| Category | Profile |");
    let library = Library::builtin().expect("built-ins");
    let mut checked = 0;
    for profile in library.profiles() {
        let Body::Source(source) = &profile.body else {
            continue;
        };
        let row = rows
            .iter()
            .find(|row| row[1].split(", ").any(|name| code(name) == profile.name))
            .unwrap_or_else(|| panic!("no row for `{}`", profile.name));
        assert_eq!(
            row[2].contains("`checkouts: allow`"),
            source.checkouts == Checkouts::Allow,
            "`{}`: the reference row and the profile disagree on checkouts",
            profile.name
        );
        checked += 1;
    }
    assert!(checked > 0, "no built-in source was checked");
}

/// The mechanism behind AC #3: built-in YAML that breaks a rule does not
/// load. Built-ins also need `category`, `description` and (sources)
/// `verified_with`.
#[test]
fn a_broken_built_in_does_not_load() {
    let good = "- name: idle-9d\n  category: filters\n  description: d\n  where: {idle_for: 9d}\n";
    Library::load(good, &[]).expect("the control loads");
    for (yaml, needle) in [
        ("- name: x\n  where: {idle_for: 1d", "built-in profiles: "),
        (
            "- name: Idle-9d\n  category: c\n  description: d\n  where: {idle_for: 9d}\n",
            "`Idle-9d` is not a valid name",
        ),
        (
            "- name: idle-9d\n  description: d\n  where: {idle_for: 9d}\n",
            "a built-in profile needs `category`",
        ),
        (
            "- name: idle-9d\n  category: c\n  where: {idle_for: 9d}\n",
            "a built-in profile needs `description`",
        ),
        (
            "- name: c\n  category: c\n  description: d\n  kind: cargo-target\n  find: {roots: required}\n",
            "needs `verified_with`",
        ),
        (
            "- name: t\n  category: c\n  description: d\n  verified_with: v\n  find: {roots: [/tmp], paths: [\"t-*\"], type: dir}\n  action: delete\n",
            "kind fs needs `examples`",
        ),
        (
            "- name: t\n  category: c\n  description: d\n  verified_with: v\n  find: {roots: [/tmp], paths: [\"t-*\"], type: dir}\n  action: delete\n  examples: {no_match: [/tmp/u]}\n",
            "`examples` needs at least one `match`",
        ),
        (
            "- name: idle-9d\n  category: c\n  description: d\n  verified_with: v\n  where: {idle_for: 9d}\n",
            "a filter profile has no `verified_with`",
        ),
    ] {
        let errors = Library::load(yaml, &[]).expect_err(yaml).to_string();
        assert!(errors.contains(needle), "{yaml}\n=> {errors}");
    }
}

// ---- The schema: unknown keys, names, kinds

/// deny_unknown_fields at every level of a profile.
#[test]
fn unknown_keys_are_rejected_everywhere() {
    for (yaml, key) in [
        (
            "- {name: x, where: {idle_for: 1d}, enabled: true}",
            "enabled",
        ),
        (
            "- {name: x, find: {roots: [/tmp], paths: [a], type: dir, depth: 2}, action: delete}",
            "depth",
        ),
        ("- {name: x, where: {idle_for: 1d, size: 3}}", "size"),
        (
            "- {name: x, find: {roots: [/tmp], paths: [a], type: dir}, action: delete, examples: {match: [/tmp/a], maybe: []}}",
            "maybe",
        ),
        (
            "- {name: x, find: {roots: [/tmp], paths: [a], type: dir}, action: {delete: {}, timeout: 5m}}",
            "timeout",
        ),
        (
            "- {name: x, find: {roots: [/tmp], paths: [a], type: dir}, action: {delete: {force: true}}}",
            "force",
        ),
        // The Rust spellings of the keyword keys are not keys.
        ("- {name: x, where_: {idle_for: 1d}}", "where_"),
        ("- {name: x, \"r#where\": {idle_for: 1d}}", "r#where"),
        (
            "- {name: x, find: {roots: [/tmp], paths: [a], type_: dir}, action: delete}",
            "type_",
        ),
        (
            "- {name: x, find: {roots: [/tmp], paths: [a], type: dir}, action: delete, examples: {r#match: [/tmp/a]}}",
            "r#match",
        ),
    ] {
        let error = yaml_error(yaml);
        assert!(
            error.contains("unknown field") && error.contains(key),
            "{yaml}\n=> {error}"
        );
    }
    for yaml in [
        "- {name: x, where: {idle_for: 1d}, where: {idle_for: 2d}}",
        "- {name: x, find: {roots: [/tmp], roots: [/var/tmp], paths: [a], type: dir}, action: delete}",
        "- {name: x, find: {roots: [/tmp], paths: [a], type: dir}, action: delete, examples: {match: [/tmp/a], match: [/tmp/b]}}",
    ] {
        let error = yaml_error(yaml);
        assert!(error.contains("duplicate"), "{yaml}\n=> {error}");
    }
    for (yaml, key) in [
        ("- {where: {idle_for: 1d}}", "name"),
        (
            "- {name: x, find: {paths: [a], type: dir}, action: delete}",
            "roots",
        ),
    ] {
        let error = yaml_error(yaml);
        assert!(
            error.contains("missing field") && error.contains(key),
            "{yaml}\n=> {error}"
        );
    }
}

/// Values of the wrong shape are YAML errors that say what was expected.
#[test]
fn malformed_values_are_yaml_errors() {
    for yaml in [
        "- {name: x, category: [a], where: {idle_for: 1d}}",
        "- {name: [x], where: {idle_for: 1d}}",
        "- {name: x, find: {roots: [/tmp], paths: a, type: dir}, action: delete}",
    ] {
        let error = yaml_error(yaml);
        assert!(
            !error.contains("Option<"),
            "a Rust type in a user error: {error}"
        );
    }
    for (yaml, needle) in [
        ("- {name: x, where: {idle_for: 3}}", "bare number"),
        ("- {name: x, where: {idle_for: 1M}}", "no months"),
        (
            "- {name: x, kind: docker, find: {roots: required}}",
            "docker",
        ),
        (
            "- {name: x, find: {roots: /tmp, paths: [a], type: dir}}",
            "roots `/tmp`",
        ),
        (
            "- {name: x, find: {roots: [/tmp], paths: [a], type: dirs}}",
            "dirs",
        ),
        (
            "- {name: x, find: {roots: [/tmp], paths: [a], type: dir}, action: remove}",
            "unknown action `remove`",
        ),
        (
            "- {name: x, find: {roots: [/tmp], paths: [a], type: dir}, action: command}",
            "a command action is a mapping",
        ),
        (
            "- {name: x, find: {roots: [/tmp], paths: [a], type: dir}, action: {delete: {}, command: [rm]}}",
            "not both",
        ),
        (
            "- {name: x, find: {roots: [/tmp], paths: [a], type: dir}, action: {delete: {}, cwd: parent}}",
            "`cwd` belongs to command actions",
        ),
        (
            "- {name: x, find: {roots: [/tmp], paths: [a], type: dir}, action: {cwd: parent}}",
            "needs `delete",
        ),
        (
            "- {name: x, find: {roots: [/tmp], paths: [a], type: dir}, action: {command: [a], cwd: here}}",
            "here",
        ),
    ] {
        let error = yaml_error(yaml);
        assert!(error.contains(needle), "{yaml}\n=> {error}");
    }
}

/// NAME is `[a-z0-9][a-z0-9_-]*` for every profile and filter name.
#[test]
fn every_profile_name_is_a_name() {
    for name in ["Tmp", "-x", "_x", "a.b", "a b", "", "å"] {
        for body in [
            "where: {idle_for: 1d}",
            "find: {roots: [/tmp], paths: [a], type: dir}\n  action: delete",
        ] {
            let yaml = format!("- name: {name:?}\n  {body}\n");
            assert_has(&user_errors(&yaml), "is not a valid name");
        }
    }
    with_user("- name: 9-a_b\n  where: {idle_for: 1d}\n").expect("a valid name loads");
}

#[test]
fn sources_and_filters_have_their_own_fields() {
    for (yaml, needle) in [
        (
            "- name: x\n  description: nothing\n",
            "has `find` (a source profile) or `where`",
        ),
        ("- name: x\n  where: {}\n", "names at least one predicate"),
        (
            "- name: x\n  kind: fs\n  where: {idle_for: 1d}\n",
            "a filter profile has no `kind`",
        ),
        (
            "- name: x\n  where: {idle_for: 1d}\n  action: delete\n",
            "a filter profile has no `action`",
        ),
        (
            "- name: x\n  where: {idle_for: 1d}\n  examples: {match: [/tmp/a]}\n",
            "a filter profile has no `examples`",
        ),
        ("- name: x\n  where: {has: a/b}\n", "must name one entry"),
        ("- name: x\n  where: {has_not: ..}\n", "must name one entry"),
        (
            "- name: x\n  where: {has_dir: a/b}\n",
            "must name one entry",
        ),
        (
            "- name: x\n  where: {lacks_entry: a/b}\n",
            "must name one entry",
        ),
        (
            "- name: x\n  where: {lacks_entry: marker}\n",
            "source predicate",
        ),
        (
            "- name: x\n  category: \" \"\n  where: {idle_for: 1d}\n",
            "`category` is empty",
        ),
        (
            "- name: x\n  verified_with: pytest 8\n  where: {idle_for: 1d}\n",
            "`verified_with` is for built-in profiles only",
        ),
    ] {
        assert_has(&user_errors(yaml), needle);
    }
}

#[test]
fn kind_fs_needs_paths_type_and_action() {
    for (yaml, needle) in [
        (
            "- name: x\n  find: {roots: [/tmp], type: dir}\n  action: delete\n",
            "needs `find.paths`",
        ),
        (
            "- name: x\n  find: {roots: [/tmp], paths: [], type: dir}\n  action: delete\n",
            "`find.paths` is empty",
        ),
        (
            "- name: x\n  find: {roots: [/tmp], paths: [a]}\n  action: delete\n",
            "needs `find.type`",
        ),
        (
            "- name: x\n  find: {roots: [/tmp], paths: [a], type: dir}\n",
            "needs `action`",
        ),
        (
            "- name: x\n  find: {roots: [/tmp], paths: [a], type: dir}\n  action: {command: []}\n",
            "`command` is empty",
        ),
        (
            "- name: x\n  find: {roots: [/tmp], paths: [a], type: dir}\n  action: delete\n  examples: {match: [tmp/a]}\n",
            "example `tmp/a` is not an absolute path",
        ),
    ] {
        assert_has(&user_errors(yaml), needle);
    }
}

/// A kind fs source with `find` and `action` but the examples given.
fn fs_source(extra: &str) -> String {
    format!(
        "- name: x\n  find: {{roots: [/tmp], paths: [\"x-*\"], type: dir}}\n  action: delete\n{extra}"
    )
}

/// U2: user fs profiles carry examples with at least one `match`, like
/// built-ins.
#[test]
fn user_fs_profiles_need_examples_with_a_match() {
    with_user(&fs_source("  examples: {match: [/tmp/x-1]}\n")).expect("the control loads");
    assert_has(&user_errors(&fs_source("")), "kind fs needs `examples`");
    assert_has(
        &user_errors(&fs_source("  examples: {no_match: [/tmp/y]}\n")),
        "`examples` needs at least one `match`",
    );
    assert_has(
        &user_errors(&fs_source("  examples: {match: []}\n")),
        "`examples` needs at least one `match`",
    );
}

/// task-80: `checkouts` is a source's setting. Absent, it is `refuse`
/// (fail closed); `allow` and `refuse` are the only values, on any kind of
/// source; a filter has none.
#[test]
fn checkouts_is_refuse_unless_a_source_says_allow() {
    let example = "  examples: {match: [/tmp/x-1]}\n";
    let checkouts = |extra: &str| {
        let library =
            with_user(&fs_source(&format!("{example}{extra}"))).unwrap_or_else(|e| panic!("{e}"));
        match &library.get("x").expect("x").body {
            Body::Source(source) => source.checkouts,
            Body::Filter(_) => panic!("not a source"),
        }
    };
    assert_eq!(checkouts(""), Checkouts::Refuse);
    assert_eq!(checkouts("  checkouts: refuse\n"), Checkouts::Refuse);
    assert_eq!(checkouts("  checkouts: allow\n"), Checkouts::Allow);
    let cargo = with_user(
        "- {name: my-targets, kind: cargo-target, find: {roots: required}}\n\
         - {name: my-vendored, kind: cargo-target, find: {roots: required}, checkouts: allow}\n",
    )
    .unwrap_or_else(|e| panic!("{e}"));
    for (name, want) in [
        ("my-targets", Checkouts::Refuse),
        ("my-vendored", Checkouts::Allow),
    ] {
        let Some(Body::Source(source)) = cargo.get(name).map(|p| &p.body) else {
            panic!("{name}");
        };
        assert_eq!(source.checkouts, want, "{name}");
    }
    for value in ["Allow", "yes", "off", "true", "\"\""] {
        let error = yaml_error(&fs_source(&format!("{example}  checkouts: {value}\n")));
        assert!(
            error.contains("refuse") && error.contains("allow"),
            "`checkouts: {value}`: {error}"
        );
    }
    for nested in ["[allow]", "[]", "{}", "{allow: true}"] {
        let error = yaml_error(&fs_source(&format!("{example}  checkouts: {nested}\n")));
        assert!(
            error.contains("`refuse` or `allow`"),
            "`checkouts: {nested}`: {error}"
        );
    }
    // A null through an alias is a null too.
    let error = yaml_error(&fs_source(&format!(
        "{example}  description: &n null\n  checkouts: *n\n"
    )));
    assert!(error.contains("`checkouts` has no value"), "alias: {error}");
    // A present null is not absence: on a source and on a filter, every
    // YAML spelling of null is refused, never read as the default.
    for null in ["", " ~", " null", " Null", " NULL", " !!null", " !!null ''"] {
        let error = yaml_error(&fs_source(&format!("{example}  checkouts:{null}\n")));
        assert!(
            error.contains("`checkouts` has no value"),
            "`checkouts:{null}`: {error}"
        );
        let error = yaml_error(&format!(
            "- name: f\n  where: {{idle_for: 1d}}\n  checkouts:{null}\n"
        ));
        assert!(
            error.contains("`checkouts` has no value"),
            "filter `checkouts:{null}`: {error}"
        );
    }
    assert_has(
        &user_errors("- {name: f, where: {idle_for: 1d}, checkouts: allow}"),
        "a filter profile has no `checkouts`",
    );
}

/// U1: a source's own `where` takes entry predicates, never `idle_for`
/// ("when" is the tier's, through filters).
#[test]
fn sources_have_no_idle_for_of_their_own() {
    let example = "  examples: {match: [/tmp/x-1]}\n";
    with_user(&fs_source(&format!(
        "{example}  where: {{has_not: .keep}}\n"
    )))
    .expect("has_not on a source loads");
    for where_ in ["{idle_for: 10d}", "{idle_for: 1d, has: CACHEDIR.TAG}"] {
        assert_has(
            &user_errors(&fs_source(&format!("{example}  where: {where_}\n"))),
            "a source profile's `where` cannot use `idle_for`",
        );
    }
    let errors = user_errors(
        "- name: x\n  kind: cargo-target\n  find: {roots: required}\n  where: {idle_for: 1d}\n",
    );
    assert_eq!(
        errors.len(),
        1,
        "one message, not also an unsupported one: {errors:?}"
    );
}

/// S2: `cwd: candidate` only where every candidate is a directory.
#[test]
fn cwd_candidate_needs_directory_candidates() {
    let command = |entry_type: &str, cwd: &str| {
        format!(
            "- name: x\n  find: {{roots: [/tmp], paths: [\"x-*\"], type: {entry_type}}}\n  action: {{command: [cleanup, \"{{path}}\"]{cwd}}}\n  examples: {{match: [/tmp/x-1]}}\n"
        )
    };
    with_user(&command("dir", ", cwd: candidate")).expect("dir + candidate loads");
    for entry_type in ["any", "file"] {
        with_user(&command(entry_type, ", cwd: parent")).expect("parent loads");
        with_user(&command(entry_type, "")).expect("the default loads");
        assert_has(
            &user_errors(&command(entry_type, ", cwd: candidate")),
            "`cwd: candidate` needs directory candidates",
        );
    }
}

/// S3: the program is a name or path, never the candidate.
#[test]
fn the_program_is_never_the_candidate() {
    let command = |argv: &str| {
        format!(
            "- name: x\n  find: {{roots: [/tmp], paths: [\"x-*\"], type: dir}}\n  action: {{command: {argv}}}\n  examples: {{match: [/tmp/x-1]}}\n"
        )
    };
    with_user(&command("[env, \"GOCACHE={path}\", go, clean, -cache]"))
        .expect("{path} in an argument loads");
    for argv in [
        "[\"{path}\"]",
        "[\"{path}/bin/clean\", -a]",
        "[\"/opt/{path}\"]",
    ] {
        assert_has(&user_errors(&command(argv)), "contains `{path}`");
    }
}

#[test]
fn kind_cargo_target_finds_by_structure_and_fixes_its_action() {
    with_user("- name: my-targets\n  kind: cargo-target\n  find: {roots: [/srv/ci]}\n")
        .expect("a fixed-root cargo-target profile loads");
    for (yaml, needle) in [
        (
            "- name: x\n  kind: cargo-target\n  find: {roots: required, paths: [target]}\n",
            "has no `find.paths`",
        ),
        (
            "- name: x\n  kind: cargo-target\n  find: {roots: required, type: dir}\n",
            "has no `find.type`",
        ),
        (
            "- name: x\n  kind: cargo-target\n  find: {roots: required}\n  action: delete\n",
            "kind cargo-target fixes its action",
        ),
        (
            "- name: x\n  kind: cargo-target\n  find: {roots: required}\n  examples: {match: [/a/target]}\n",
            "has no `examples`",
        ),
        (
            "- name: x\n  kind: cargo-target\n  find: {roots: required}\n  where: {has: CACHEDIR.TAG}\n",
            "`where` uses `has`, which kind cargo-target does not support (`has` works on: fs)",
        ),
        (
            "- name: x\n  kind: nix-store\n  find: {roots: required}\n",
            "kind nix-store is not supported yet",
        ),
    ] {
        assert_has(&user_errors(yaml), needle);
    }
}

#[test]
fn roots_and_patterns_are_checked_paths() {
    for (yaml, needle) in [
        (
            "- name: x\n  find: {roots: [], paths: [a], type: dir}\n  action: delete\n",
            "`find.roots` is empty",
        ),
        (
            "- name: x\n  find: {roots: [tmp], paths: [a], type: dir}\n  action: delete\n",
            "root `tmp` is not an absolute path",
        ),
        (
            "- name: x\n  find: {roots: [~/src], paths: [a], type: dir}\n  action: delete\n",
            "`~` is not allowed",
        ),
        (
            "- name: x\n  find: {roots: [\"/tmp/{user}\"], paths: [a], type: dir}\n  action: delete\n",
            "the only placeholder is `{uid}`",
        ),
        (
            "- name: x\n  find: {roots: [/tmp], paths: [/a], type: dir}\n  action: delete\n",
            "patterns are relative",
        ),
        (
            "- name: x\n  find: {roots: [/tmp], paths: [../etc], type: dir}\n  action: delete\n",
            "stays below its roots",
        ),
        (
            "- name: x\n  find: {roots: [/tmp], paths: [a/./b], type: dir}\n  action: delete\n",
            "stays below its roots",
        ),
        (
            "- name: x\n  find: {roots: [/tmp], paths: [\"**/target\"], type: dir}\n  action: delete\n",
            "`**`",
        ),
        (
            "- name: x\n  find: {roots: [/tmp], paths: [\"a**\"], type: dir}\n  action: delete\n",
            "`**`",
        ),
        (
            "- name: x\n  find: {roots: [/tmp], paths: [a//b], type: dir}\n  action: delete\n",
            "empty segment",
        ),
        (
            "- name: x\n  find: {roots: [/tmp], paths: [a/], type: dir}\n  action: delete\n",
            "empty segment",
        ),
    ] {
        assert_has(&user_errors(yaml), needle);
    }
    with_user(
        "- name: x\n  find: {roots: [\"/tmp/claude-{uid}\"], paths: [\"*/*\", \"s-{uid}\"], type: dir}\n  action: delete\n  examples: {match: [/tmp/claude-1000/a/b]}\n",
    )
    .expect("{uid} is allowed");
}

#[test]
fn exclude_patterns_obey_path_rules_and_only_fs_sources_accept_them() {
    let base = "- name: x\n  find: {roots: [/tmp], paths: ['*'], exclude: [../private], type: dir}\n  action: delete\n  examples: {match: [/tmp/cache]}\n";
    assert_has(&user_errors(base), "`find.exclude`: pattern `../private`");

    let cargo = "- name: x\n  kind: cargo-target\n  find: {roots: [/tmp], exclude: [private]}\n";
    assert_has(&user_errors(cargo), "no `find.exclude`");

    for null in ["null", "~"] {
        let yaml = format!(
            "- name: x\n  find: {{roots: [/tmp], paths: ['*'], exclude: {null}, type: dir}}\n  action: delete\n"
        );
        assert!(
            parse_profiles(&yaml).is_err(),
            "present null exclude was accepted: {yaml}"
        );
    }
    let blank = "- name: x\n  find:\n    roots: [/tmp]\n    paths: ['*']\n    exclude:\n    type: dir\n  action: delete\n";
    assert!(parse_profiles(blank).is_err(), "blank exclude was accepted");
}

#[test]
fn names_are_unique_and_built_ins_need_explicit_override() {
    let errors = user_errors(
        "- name: idle-1d\n  where: {idle_for: 2d}\n- name: x\n  where: {idle_for: 1d}\n- name: x\n  where: {idle_for: 2d}\n",
    );
    assert_eq!(
        errors,
        vec![
            "profiles[0] (`idle-1d`): a built-in profile is named `idle-1d`; use `override: true` \
             to replace it",
            "profiles[2] (`x`): the name `x` is already used by profiles[1] (`x`)",
        ]
    );
}

#[test]
fn explicit_override_replaces_one_built_in_and_rejects_typos() {
    let yaml = "- name: idle-1d\n  override: true\n  where: {idle_for: 2d}\n";
    let library = with_user(yaml).expect("override loads");
    let replacement = library.get("idle-1d").expect("replacement");
    assert_eq!(replacement.origin, Origin::UserOverride);
    let Body::Filter(where_) = &replacement.body else {
        panic!("filter")
    };
    assert_eq!(where_.idle_for, Some(IdleFor(Duration::from_secs(2 * DAY))));
    assert_eq!(
        library.profiles().filter(|p| p.name == "idle-1d").count(),
        1
    );
    assert_has(
        &user_errors("- name: idle-1d\n  where: {idle_for: 2d}\n"),
        "use `override: true`",
    );
    assert_has(
        &user_errors("- name: idle-1x\n  override: true\n  where: {idle_for: 2d}\n"),
        "names no built-in",
    );
    assert_has(
        &user_errors(
            "- name: idle-1d\n  override: true\n  where: {idle_for: 2d}\n- name: idle-1d\n  override: true\n  where: {idle_for: 3d}\n",
        ),
        "already used",
    );
    let errors = Library::load(
        "- name: x\n  override: true\n  where: {idle_for: 1d}\n",
        &[],
    )
    .unwrap_err();
    assert!(
        errors
            .to_string()
            .contains("`override` is for user profiles only")
    );
}

/// All problems are reported together, each with where it is.
#[test]
fn every_problem_is_reported_with_its_profile() {
    let errors = user_errors(
        "- name: ok\n  where: {idle_for: 1d}\n- name: Bad\n  find: {roots: [tmp], paths: [\"**\"]}\n",
    );
    assert_eq!(
        errors,
        vec![
            "profiles[1] (`Bad`): `Bad` is not a valid name: a name is lowercase letters, \
             digits, `-` and `_`, starting with a letter or a digit",
            "profiles[1] (`Bad`): root `tmp` is not an absolute path",
            "profiles[1] (`Bad`): pattern `**` uses `**`: each segment is one directory level",
            "profiles[1] (`Bad`): kind fs needs `find.type` (dir, file or any)",
            "profiles[1] (`Bad`): kind fs needs `action` (`delete` or `{command: [...]}`); \
             there is no default",
            "profiles[1] (`Bad`): kind fs needs `examples` (`{match: [...], no_match: \
             [...]}`), with at least one `match`",
        ]
    );
}

// ---- The doc's profiles load

/// docs/reference.md's profile example (Profiles) is a valid built-in, and
/// the example config's `profiles:` (Configuration file) are valid user
/// profiles.
#[test]
fn the_reference_examples_load() {
    let doc = reference_md();
    let profiles_md = section(&doc, "## Profiles");
    let [example] = code_blocks(&profiles_md, "yaml")
        .try_into()
        .expect("one YAML block in Profiles");
    let library = Library::load(&example, &[]).unwrap_or_else(|e| panic!("{e}"));
    let pytest = library.get("pytest-tmp").expect("pytest-tmp");
    let Body::Source(source) = &pytest.body else {
        panic!("not a source");
    };
    assert_eq!(
        source.action,
        Action::Delete {
            make_writable: false
        }
    );
    assert_eq!(source.examples.matches.len(), 2);
    assert_eq!(
        source.checkouts,
        Checkouts::Allow,
        "as its library row says"
    );

    #[derive(serde::Deserialize)]
    struct ConfigProfiles {
        profiles: Vec<ProfileSpec>,
    }
    let config_md = section(&doc, "## Configuration file");
    let [config] = code_blocks(&config_md, "yaml")
        .try_into()
        .expect("one YAML block in Configuration file");
    let config: ConfigProfiles = serde_saphyr::from_str(&config).unwrap_or_else(|e| panic!("{e}"));
    let library = Library::with_user_profiles(&config.profiles).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        library.get("zephyr-run").map(|p| p.origin),
        Some(Origin::User)
    );
}

// ---- AC #1: roots fixed vs required

fn checked(library: &Library, text: &str) -> crate::select::Checked {
    compile(text, library).unwrap_or_else(|e| panic!("{e}"))
}

fn roots(list: &[&str]) -> Vec<String> {
    list.iter().map(|r| (*r).to_owned()).collect()
}

const FIXED: &str = "
- name: tmp-a
  find: {roots: [/tmp], paths: [\"a-*\"], type: dir}
  action: delete
  examples: {match: [/tmp/a-1]}
- name: tmp-b
  find: {roots: [/var/tmp], paths: [\"b-*\"], type: dir}
  action: delete
  examples: {match: [/var/tmp/b-1]}
- name: cache-c
  find: {roots: required, paths: [\".c\"], type: dir}
  action: delete
  examples: {match: [/home/u/.c]}
";

/// AC #1, first half: a `roots: required` profile without roots is an
/// error, also when it is only negated.
#[test]
fn ac1_a_required_profile_without_roots_is_an_error() {
    let library = with_user(FIXED).expect("loads");
    for (text, missing) in [
        (
            "profile:cargo-target && filter:idle-1d",
            vec!["cargo-target"],
        ),
        ("profile:tmp-a || profile:cache-c", vec!["cache-c"]),
        ("profile:tmp-a && !profile:cache-c", vec!["cache-c"]),
        (
            "profile:cargo-target && !profile:cargo-target",
            vec!["cargo-target"],
        ),
    ] {
        assert_eq!(
            library.bind_roots(&checked(&library, text), None),
            Err(RootsError::Missing {
                profiles: missing.iter().map(|m| (*m).to_owned()).collect()
            }),
            "`{text}`"
        );
    }
    assert_eq!(
        library
            .bind_roots(&checked(&library, "profile:cargo-target"), None)
            .unwrap_err()
            .to_string(),
        "`cargo-target` has `roots: required`, and no roots were given"
    );
    assert_eq!(
        library.roots_of("cache-c", None),
        Err(RootsError::Missing {
            profiles: vec!["cache-c".into()]
        }),
        "a bare `protect` entry of a required profile"
    );
}

/// AC #1, second half: roots given where only fixed-root profiles are is an
/// error.
#[test]
fn ac1_roots_for_only_fixed_root_profiles_are_an_error() {
    let library = with_user(FIXED).expect("loads");
    let given = roots(&["/srv/ci"]);
    let error = library
        .bind_roots(
            &checked(
                &library,
                "(profile:tmp-a || profile:tmp-b) && filter:idle-1d",
            ),
            Some(&given),
        )
        .unwrap_err();
    assert_eq!(
        error,
        RootsError::Unused {
            profiles: vec!["tmp-a".into(), "tmp-b".into()]
        }
    );
    assert_eq!(
        error.to_string(),
        "roots were given, but `tmp-a`, `tmp-b` have fixed roots: only `roots: required` \
         profiles take roots"
    );
    assert!(matches!(
        library.roots_of("tmp-a", Some(&given)),
        Err(RootsError::Unused { .. })
    ));
}

/// Fixed-root profiles keep their own roots next to required ones; a
/// negated required profile takes the given roots too.
#[test]
fn given_roots_apply_to_required_profiles_only() {
    let library = with_user(FIXED).expect("loads");
    let given = roots(&["/home/mark/src", "/srv/ci"]);
    let bound = library
        .bind_roots(
            &checked(
                &library,
                "(profile:tmp-a || profile:cache-c) && !profile:cargo-target",
            ),
            Some(&given),
        )
        .expect("binds");
    assert_eq!(bound["tmp-a"], roots(&["/tmp"]));
    assert_eq!(bound["cache-c"], given);
    assert_eq!(bound["cargo-target"], given);
    assert_eq!(bound.len(), 3);
    assert_eq!(library.roots_of("tmp-b", None), Ok(roots(&["/var/tmp"])));
    assert_eq!(
        library.roots_of("cargo-target", Some(&given)),
        Ok(given.clone())
    );
}

#[test]
fn given_roots_are_checked_paths() {
    let library = Library::builtin().expect("built-ins");
    let selection = checked(&library, "profile:cargo-target");
    assert_eq!(
        library.bind_roots(&selection, Some(&[])),
        Err(RootsError::Empty)
    );
    for (root, needle) in [
        ("src", "not an absolute path"),
        ("~/src", "`~` is not allowed"),
        ("/srv/{job}", "only placeholder"),
    ] {
        let error = library
            .bind_roots(&selection, Some(&roots(&[root])))
            .unwrap_err()
            .to_string();
        assert!(error.contains(needle), "{root}: {error}");
    }
    assert_eq!(
        library.roots_of("idle-1d", None),
        Err(RootsError::NotASource("idle-1d".into()))
    );
}

// ---- AC #2: idle_for

fn idle(text: &str) -> Option<Duration> {
    let library = with_user(
        "- name: scratch\n  find: {roots: [/tmp], paths: [\"s-*\"], type: dir}\n  where: {has_not: .keep}\n  action: delete\n  examples: {match: [/tmp/s-1]}\n",
    )
    .expect("loads");
    let selection = checked(&library, text);
    let [term] = selection.expr().terms.as_slice() else {
        panic!("one term");
    };
    library.idle_for(term).expect("known names").map(|i| i.0)
}

const DAY: u64 = 86_400;

/// AC #2: two (or more) idle filters in one term combine to the longest,
/// whatever their order; `!filter:` is a complement and does not; a
/// source's own `where` adds no idle requirement (it cannot have one, U1).
#[test]
fn ac2_idle_filters_in_one_term_combine_to_the_longest() {
    for text in [
        "profile:cargo-target && filter:idle-1d && filter:idle-7d",
        "profile:cargo-target && filter:idle-7d && filter:idle-1d",
        "profile:cargo-target && filter:idle-1h && filter:idle-7d && filter:idle-3d",
        "profile:scratch && filter:idle-3d && filter:idle-7d",
    ] {
        assert_eq!(idle(text), Some(Duration::from_secs(7 * DAY)), "`{text}`");
    }
    assert_eq!(
        idle("profile:cargo-target && filter:idle-2d && !filter:idle-30d"),
        Some(Duration::from_secs(2 * DAY)),
        "a negated filter is not a requirement"
    );
    assert_eq!(idle("profile:cargo-target"), None);
    assert_eq!(idle("profile:scratch"), None);
    assert_eq!(idle("profile:cargo-target && !filter:idle-30d"), None);
}

/// A source's minimum is mandatory for a bare selection and cannot be
/// weakened by a shorter tier filter. Its name is independent of this rule.
#[test]
fn source_minimum_idle_is_a_floor_for_every_term() {
    let library = with_user(
        "- name: arbitrary-spool\n  find: {roots: [/var/tmp], paths: [\"spool-*\"], type: any}\n  min_idle_for: 3d\n  action: delete\n  examples: {match: [/var/tmp/spool-1]}\n",
    )
    .expect("source with idle floor loads");
    for (selection, expected) in [
        ("profile:arbitrary-spool", 3),
        ("profile:arbitrary-spool && filter:idle-1d", 3),
        ("profile:arbitrary-spool && filter:idle-7d", 7),
    ] {
        let checked = checked(&library, selection);
        let [term] = checked.expr().terms.as_slice() else {
            panic!("one term");
        };
        assert_eq!(
            library.idle_for(term).unwrap(),
            Some(IdleFor(Duration::from_secs(expected * DAY))),
            "{selection}"
        );
    }
    assert_has(
        &user_errors("- name: only-filter\n  min_idle_for: 3d\n  where: {idle_for: 1d}\n"),
        "a filter profile has no `min_idle_for`",
    );
    assert_has(
        &user_errors(
            "- name: no-floor\n  find: {roots: [/tmp], paths: [\"s-*\"], type: any}\n  min_idle_for: 0s\n  action: delete\n  examples: {match: [/tmp/s-1]}\n",
        ),
        "`min_idle_for` must be greater than zero",
    );
    assert!(yaml_error("- name: x\n  min_idle_for: null\n  find: {roots: [/tmp], paths: [\"x-*\"], type: any}\n").contains("invalid type"));
}

#[test]
fn containers_image_tmp_has_an_immutable_minimum_age_and_exact_scope() {
    let library = Library::builtin().unwrap();
    let profile = library.get("containers-image-tmp").unwrap();
    let Body::Source(source) = &profile.body else {
        panic!("not a source");
    };
    assert_eq!(source.roots, Roots::Fixed(vec!["/var/tmp".into()]));
    assert_eq!(
        source.finds,
        Finds::Paths {
            patterns: vec!["buildah*".into(), "container_images_storage*".into()],
            exclude: Vec::new(),
            entry_type: EntryType::Any,
        }
    );
    assert_eq!(
        source.min_idle_for,
        Some(IdleFor(Duration::from_secs(3 * DAY)))
    );
    assert_eq!(
        source.action,
        Action::Delete {
            make_writable: false
        }
    );
    assert_eq!(source.checkouts, Checkouts::Refuse);
    let checked = checked(&library, "profile:containers-image-tmp");
    assert_eq!(
        library.idle_for(&checked.expr().terms[0]).unwrap(),
        source.min_idle_for,
    );
}

/// S1: the verdict is three-way, and an unknown age passes neither
/// `filter:` nor `!filter:`.
#[test]
fn an_unknown_age_passes_no_idle_filter() {
    let at = |secs: i64, nanos: u32| Timestamp { secs, nanos };
    let one_day = IdleFor(Duration::from_secs(DAY));
    let now = at(1_000_000, 500);
    for (last_use, want) in [
        (Some(at(1_000_000 - 86_400, 500)), Idleness::Idle),
        (Some(at(0, 0)), Idleness::Idle),
        (Some(at(1_000_000 - 86_400, 501)), Idleness::NotIdle),
        (Some(now), Idleness::NotIdle),
        (None, Idleness::Unknown(Unknown::Unanswered)),
        (
            Some(at(1_000_000, 501)),
            Idleness::Unknown(Unknown::InFuture),
        ),
    ] {
        assert_eq!(one_day.verdict(last_use, now), want, "{last_use:?}");
    }
    assert_eq!(
        IdleFor(Duration::ZERO).verdict(Some(now), now),
        Idleness::Idle
    );
    for (verdict, positive, negated) in [
        (Idleness::Idle, true, false),
        (Idleness::NotIdle, false, true),
        (Idleness::Unknown(Unknown::Unanswered), false, false),
        (Idleness::Unknown(Unknown::InFuture), false, false),
    ] {
        assert_eq!(verdict.passes(false), positive, "filter: on {verdict:?}");
        assert_eq!(verdict.passes(true), negated, "!filter: on {verdict:?}");
    }
    assert_eq!(Unknown::InFuture.skip_reason(), "future_time");
    assert_eq!(Unknown::Unanswered.skip_reason(), "unsupported_fs");
}

/// Every skip reason an unknown age reports is in docs/reference.md's
/// `skipped` reason enum.
#[test]
fn unknown_age_reasons_are_documented() {
    let doc = reference_md();
    let line = doc
        .lines()
        .find(|l| l.contains("\"skipped\":"))
        .expect("the skipped line of the JSON result");
    for unknown in [Unknown::Unanswered, Unknown::InFuture] {
        let reason = unknown.skip_reason();
        assert!(
            line.contains(&format!(" {reason} ")),
            "`{reason}` is not a documented skip reason"
        );
    }
}

/// The kinds with an idle measure are exactly the kinds `idle_for`
/// supports (docs/reference.md, both Predicates tables).
#[test]
fn every_kind_with_idle_for_has_a_measure() {
    for kind in Kind::ALL {
        assert_eq!(
            IdleMeasure::of(kind).is_some(),
            PredicateName::IdleFor.supported_by(kind),
            "{kind}"
        );
    }
    assert_eq!(
        IdleMeasure::of(Kind::CargoTarget),
        Some(IdleMeasure::LastBuild)
    );
    assert_eq!(IdleMeasure::of(Kind::Fs), Some(IdleMeasure::NewestChange));
}

// ---- Action equality

const ACTIONS: &str = include_str!("test-actions.yaml");

fn action(library: &Library, name: &str) -> Action {
    match &library.get(name).expect(name).body {
        Body::Source(source) => source.action.clone(),
        Body::Filter(_) => panic!("{name} is a filter"),
    }
}

/// Two sources share an action exactly when the action and every option
/// are equal; the default `cwd` is resolved, so writing it changes nothing.
#[test]
fn actions_are_equal_only_with_every_option_equal() {
    let library = with_user(ACTIONS).expect("loads");
    let a = |name| action(&library, name);
    assert_eq!(a("del"), a("del-2"));
    assert_eq!(a("go"), a("go-2"), "cwd: candidate is the default for dirs");
    for (x, y) in [
        ("del", "del-w"),
        ("go", "go-parent"),
        ("go", "go-modcache"),
        ("go", "spaced"),
        ("any-cmd", "file-cmd"),
        ("del", "go"),
    ] {
        assert_ne!(a(x), a(y), "{x} vs {y}");
    }
    assert_eq!(
        a("file-cmd"),
        Action::Command {
            argv: vec!["cleanup".into(), "{path}".into()],
            cwd: Some(Cwd::Parent)
        }
    );
    assert_eq!(
        a("any-cmd"),
        Action::Command {
            argv: vec!["cleanup".into(), "{path}".into()],
            cwd: None
        },
        "for any type the default is decided per candidate"
    );
}

/// The "one action" error shows what differs.
#[test]
fn the_one_action_error_shows_what_differs() {
    let library = with_user(ACTIONS).expect("loads");
    for (text, first, other) in [
        (
            "profile:del || profile:del-w",
            "delete",
            "delete {make_writable: true}",
        ),
        (
            "profile:go || profile:go-modcache",
            "command [\"env\", \"GOCACHE={path}\", \"go\", \"clean\", \"-cache\"], cwd: candidate",
            "command [\"env\", \"GOMODCACHE={path}\", \"go\", \"clean\", \"-modcache\"], cwd: candidate",
        ),
        (
            "profile:go || profile:go-parent",
            "command [\"env\", \"GOCACHE={path}\", \"go\", \"clean\", \"-cache\"], cwd: candidate",
            "command [\"env\", \"GOCACHE={path}\", \"go\", \"clean\", \"-cache\"], cwd: parent",
        ),
        (
            "profile:go || profile:spaced",
            "command [\"env\", \"GOCACHE={path}\", \"go\", \"clean\", \"-cache\"], cwd: candidate",
            "command [\"env\", \"GOCACHE={path}\", \"go\", \"clean\", \"-cache \"], cwd: candidate",
        ),
        (
            "profile:any-cmd || profile:file-cmd",
            "command [\"cleanup\", \"{path}\"]",
            "command [\"cleanup\", \"{path}\"], cwd: parent",
        ),
    ] {
        let Err(SelectError::Check(errors)) = compile(text, &library) else {
            panic!("`{text}` accepted");
        };
        let [
            CheckError::ActionsDiffer {
                first_action,
                other_action,
                ..
            },
        ] = errors.errors.as_slice()
        else {
            panic!("`{text}`: {errors}");
        };
        assert_eq!(
            (first_action.as_str(), other_action.as_str()),
            (first, other)
        );
        assert_ne!(
            first_action, other_action,
            "the rendering shows the difference"
        );
    }
    for text in ["profile:del || profile:del-2", "profile:go || profile:go-2"] {
        compile(text, &library).unwrap_or_else(|e| panic!("{e}"));
    }
}

/// Every `action:` form in docs/reference.md (Actions) loads with its kind,
/// and each one names a distinct action.
#[test]
fn the_documented_action_forms_load() {
    let actions_md = section(&reference_md(), "### Actions");
    let [block] = code_blocks(&actions_md, "yaml")
        .try_into()
        .expect("one YAML block in Actions");
    let mut seen = Vec::new();
    for (i, line) in block.lines().enumerate() {
        let yaml = if line.contains("docker-") {
            let kind = if line.contains("builder") {
                "docker-build-cache"
            } else {
                "docker-images"
            };
            format!("- name: p{i}\n  kind: {kind}\n  find: {{roots: daemon}}\n  {line}\n")
        } else {
            format!(
                "- name: p{i}\n  find: {{roots: required, paths: [c], type: dir}}\n  {line}\n  examples: {{match: [/home/u/c]}}\n"
            )
        };
        let library = with_user(&yaml).unwrap_or_else(|e| panic!("{line}: {e}"));
        let action = action(&library, &format!("p{i}"));
        assert!(!seen.contains(&action), "{line} repeats an action");
        seen.push(action);
    }
    assert_eq!(seen.len(), 6);
}
