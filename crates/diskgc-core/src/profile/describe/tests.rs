//! What `--help` and `profiles list | show` say about a library: nothing
//! is lost (the YAML and the JSON each load back to the same profile),
//! the JSON is the one docs/reference.md documents, and every built-in is
//! named without any text naming it.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use super::*;
use crate::config;
use crate::doc::{code_blocks, reference_md, section};
use crate::profile::{Library, parse_profiles};

const ACTIONS: &str = include_str!("../test-actions.yaml");
const SPIKE: &str = include_str!("../../fs_kind/spike-profiles.yaml");

/// The libraries the round trips run over: the built-ins alone, the
/// action fixtures (every action form, every entry type), and the
/// profile spike's kind fs sources (fixed and required roots,
/// `where: {has, has_not}`, `{uid}`, `checkouts: allow`, command actions).
fn libraries() -> Vec<(&'static str, Library)> {
    let actions = Library::with_user_profiles(&parse_profiles(ACTIONS).expect("parses"))
        .expect("the action fixtures load");
    let spike = config::parse(Path::new("spike-profiles.yaml"), SPIKE.as_bytes())
        .expect("the spike's profiles load")
        .library;
    vec![
        ("built-ins", Library::builtin().expect("built-ins")),
        ("test-actions.yaml", actions),
        ("spike-profiles.yaml", spike),
    ]
}

/// `yaml` (one profile) loaded as `origin` would load it.
fn reload(yaml: &str, origin: Origin) -> Profile {
    let library = match origin {
        Origin::BuiltIn => Library::load(yaml, &[]),
        Origin::User | Origin::UserOverride => Library::with_user_profiles(
            &parse_profiles(yaml).unwrap_or_else(|e| panic!("{e}:\n{yaml}")),
        ),
    }
    .unwrap_or_else(|e| panic!("does not load:\n{e}\n{yaml}"));
    let mine: Vec<&Profile> = library.profiles().filter(|p| p.origin == origin).collect();
    match mine.as_slice() {
        [profile] => (*profile).clone(),
        other => panic!("{} profiles of origin {origin:?}:\n{yaml}", other.len()),
    }
}

/// `show NAME`'s YAML is the profile as diskgc understood it: loaded back,
/// it is the same profile, every field (a dropped or wrong field fails).
/// A built-in's YAML is loaded as built-in YAML, which holds only it.
#[test]
fn show_yaml_loads_back_to_the_same_profile() {
    for (from, library) in libraries() {
        let origin = if from == "built-ins" {
            Origin::BuiltIn
        } else {
            Origin::User
        };
        let mine: Vec<&Profile> = library.profiles().filter(|p| p.origin == origin).collect();
        assert!(!mine.is_empty(), "{from}: no {origin:?} profiles");
        for profile in mine {
            let yaml = show_yaml(profile, None);
            assert_eq!(&reload(&yaml, origin), profile, "{from}:\n{yaml}");
        }
    }
}

#[test]
fn cmake_lacks_entry_survives_show_and_json() {
    let library = Library::builtin().expect("built-ins");
    let profile = library
        .get("cmake-build-anchored")
        .expect("cmake-build-anchored");
    let yaml = show_yaml(profile, None);
    assert!(yaml.contains("lacks_entry: CMakeLists.txt"), "{yaml}");
    assert_eq!(&reload(&yaml, Origin::BuiltIn), profile);
    let json: Value = serde_json::from_str(&profile_json(profile).to_text()).expect("JSON");
    assert_eq!(json["where"]["lacks_entry"], "CMakeLists.txt");
    assert_eq!(json["where"]["has_not"], Value::Null);
}

#[test]
fn docker_backend_roots_match_the_documented_json_shape() {
    let library = Library::builtin().expect("built-ins");
    let profile = library.get("docker-dangling").expect("Docker built-in");
    let json: Value = serde_json::from_str(&profile_json(profile).to_text()).expect("JSON");
    // TASK-192: `roots` is always a list; the backend is its own field.
    assert_eq!(json["find"]["roots"], serde_json::json!([]));
    assert_eq!(json["find"]["roots_required"], false);
    assert_eq!(json["find"]["backend"], "daemon");
    let required = library.get("cargo-target").expect("cargo-target");
    let required: Value = serde_json::from_str(&profile_json(required).to_text()).expect("JSON");
    assert_eq!(required["find"]["roots"], serde_json::json!([]));
    assert_eq!(required["find"]["roots_required"], true);
    assert!(required["find"]["backend"].is_null());
    let fixed = library.get("pytest-tmp").expect("pytest-tmp");
    let fixed: Value = serde_json::from_str(&profile_json(fixed).to_text()).expect("JSON");
    assert!(
        fixed["find"]["roots"]
            .as_array()
            .is_some_and(|r| !r.is_empty())
    );
    assert!(fixed["find"]["backend"].is_null());
    let docs = section(&reference_md(), "### `profiles` command");
    assert!(
        docs.contains("(`\"daemon\"`, YAML's `roots: daemon`, for the Docker kinds"),
        "the documented roots JSON shape omitted the Docker backend"
    );
}

/// `checkouts` is spelled out for every source, the default too
/// (docs/reference.md, `profiles` command).
#[test]
fn show_yaml_spells_out_checkouts_for_every_source() {
    for (from, library) in libraries() {
        for profile in library.profiles() {
            let yaml = show_yaml(profile, None);
            let has = yaml.lines().any(|l| l.starts_with("  checkouts: "));
            assert_eq!(has, profile.role() == "source", "{from}:\n{yaml}");
        }
    }
}

#[test]
fn override_is_visible_in_list_show_and_json() {
    let spec =
        parse_profiles("- name: idle-1d\n  override: true\n  where: {idle_for: 2d}\n").unwrap();
    let library = Library::with_user_profiles(&spec).unwrap();
    let profile = library.get("idle-1d").unwrap();
    assert_eq!(profile.origin, Origin::UserOverride);
    assert!(
        list_text(&library)
            .lines()
            .any(|line| line.starts_with("idle-1d ") && line.contains("user_override"))
    );
    let yaml = show_yaml(profile, None);
    assert!(yaml.contains("  override: true\n"), "{yaml}");
    let json: Value = serde_json::from_str(&profile_json(profile).to_text()).unwrap();
    assert_eq!(json["origin"], "user_override");
    assert_eq!(json["override"], true);
}

/// The schema form of a profile's JSON object: what docs/reference.md
/// (Profiles) would write for it. The JSON object is a superset; this
/// keeps the schema's keys, drops `null`s, turns `roots_required` back
/// into `roots: required` and `action` back into its YAML form. JSON is
/// YAML, so the result loads as a profile.
fn schema_form(json: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for (key, value) in json.as_object().expect("an object") {
        let value = match (key.as_str(), value) {
            ("origin" | "role" | "override", _) | (_, Value::Null) => continue,
            ("find", Value::Object(fields)) => {
                let required = fields["roots_required"].as_bool().expect("a boolean");
                let backend = fields["backend"].as_str();
                let listed = fields["roots"].as_array().expect("roots is always a list");
                assert_eq!(listed.is_empty(), required || backend.is_some(), "{json}");
                let mut find: serde_json::Map<String, Value> = fields
                    .iter()
                    .filter(|(k, v)| !v.is_null() && *k != "roots_required" && *k != "backend")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if required {
                    find.insert("roots".to_owned(), Value::from("required"));
                }
                if let Some(backend) = backend {
                    find.insert("roots".to_owned(), Value::from(backend));
                }
                Value::Object(find)
            }
            ("where" | "examples", Value::Object(fields)) => Value::Object(
                fields
                    .iter()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
            ("action", action) => match action["type"].as_str() {
                Some("delete") if action["make_writable"] == true => {
                    serde_json::json!({"delete": {"make_writable": true}})
                }
                Some("delete") => Value::from("delete"),
                Some("command") if action["cwd"].is_null() => {
                    serde_json::json!({"command": action["command"]})
                }
                Some("command") => {
                    serde_json::json!({"command": action["command"], "cwd": action["cwd"]})
                }
                Some("cargo_lock_protocol") => continue,
                Some("docker_builder_prune") => Value::from("docker-builder-prune"),
                Some("docker_image_prune") => Value::from("docker-image-prune"),
                Some("docker_image_prune_all") => Value::from("docker-image-prune-all"),
                other => panic!("action type {other:?}"),
            },
            _ => value.clone(),
        };
        if value.as_object().is_some_and(serde_json::Map::is_empty) {
            continue;
        }
        out.insert(key.clone(), value);
    }
    Value::Object(out)
}

/// The JSON object of a profile carries the whole definition: rebuilt
/// into the schema, it loads as the same profile.
#[test]
fn profile_json_loads_back_to_the_same_profile() {
    for (from, library) in libraries() {
        for profile in library.profiles() {
            let json: Value =
                serde_json::from_str(&profile_json(profile).to_text()).expect("valid JSON");
            assert_eq!(json["origin"], profile.origin.name());
            let yaml = format!("[{}]", schema_form(&json));
            assert_eq!(&reload(&yaml, profile.origin), profile, "{from}: {yaml}");
        }
    }
}

/// The JSON blocks of docs/reference.md, `profiles` command: `list`'s,
/// then `show`'s.
fn documented() -> (Value, Value) {
    let text = section(&reference_md(), "### `profiles` command");
    let blocks = code_blocks(&text, "json");
    assert_eq!(blocks.len(), 2, "list's and show's JSON blocks");
    let parse = |b: &str| serde_json::from_str(b).unwrap_or_else(|e| panic!("{e}:\n{b}"));
    (parse(&blocks[0]), parse(&blocks[1]))
}

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object()
        .unwrap_or_else(|| panic!("not an object: {v}"))
        .keys()
        .cloned()
        .collect()
}

/// `got` has `want`'s field names, recursively into objects and the first
/// object of a list, wherever both have one (the documented example
/// shows each field once with a value; where it is `null` in one, the
/// other may be either).
fn same_fields(want: &Value, got: &Value, at: &str) {
    assert_eq!(keys(want), keys(got), "{at}: field names");
    for (name, w) in want.as_object().expect("object") {
        let here = format!("{at}.{name}");
        match (w, &got[name]) {
            (Value::Object(_), g @ Value::Object(_)) => same_fields(w, g, &here),
            (Value::Array(ws), Value::Array(gs)) => {
                if let (Some(w0 @ Value::Object(_)), Some(g0)) = (ws.first(), gs.first()) {
                    same_fields(w0, g0, &here);
                }
            }
            _ => {}
        }
    }
}

/// The values of a documented enum (`"a | b"`).
fn alternatives(v: &Value) -> BTreeSet<String> {
    v.as_str()
        .unwrap_or_else(|| panic!("an enum is documented as a string: {v}"))
        .split('|')
        .map(|s| s.trim().to_owned())
        .collect()
}

/// The JSON of `list` and `show` has exactly the fields docs/reference.md
/// documents, for every profile of every fixture (a source of each kind,
/// a filter, user and built-in), and the enums take documented values
/// only.
#[test]
fn the_json_fields_are_the_docs() {
    let (list_doc, show_doc) = documented();
    let profile_doc = &show_doc["profile"];
    // The doc defines the profile object once, under show's `profile`;
    // list's entries are said to be that object.
    assert_eq!(list_doc["profiles"], serde_json::json!([]));
    let enums = [
        ("origin", alternatives(&profile_doc["origin"])),
        ("role", alternatives(&profile_doc["role"])),
        ("checkouts", alternatives(&profile_doc["checkouts"])),
    ];
    let action_types = alternatives(&profile_doc["action"]["type"]);
    let entry_types = alternatives(&profile_doc["find"]["type"]);
    for (from, library) in libraries() {
        let path = Path::new("/c.yaml");
        let list: Value =
            serde_json::from_str(&list_json(&library, Some(path)).to_text()).expect("JSON");
        assert_eq!(keys(&list), keys(&list_doc), "{from}: list");
        for entry in list["profiles"].as_array().expect("profiles") {
            same_fields(profile_doc, entry, &format!("{from}: list entry"));
        }
        for profile in library.profiles() {
            let show: Value =
                serde_json::from_str(&show_json(profile, None).to_text()).expect("JSON");
            assert_eq!(keys(&show), keys(&show_doc), "{from}: show");
            let got = &show["profile"];
            same_fields(profile_doc, got, &format!("{from}: {}", profile.name));
            for (field, values) in &enums {
                if let Some(v) = got[field].as_str() {
                    assert!(values.contains(v), "{field}: {v} is not documented");
                }
            }
            if let Some(v) = got["action"]["type"].as_str() {
                assert!(
                    action_types.contains(v),
                    "action type {v} is not documented"
                );
            }
            if let Some(v) = got["find"]["type"].as_str() {
                assert!(entry_types.contains(v), "find type {v} is not documented");
            }
        }
    }
}

/// `list --json` holds every profile of the library once, built-ins and
/// user profiles, and names the configuration it read.
#[test]
fn list_json_holds_every_profile() {
    for (from, library) in libraries() {
        let list: Value = serde_json::from_str(&list_json(&library, None).to_text()).expect("JSON");
        assert!(list["config"].is_null());
        let names: Vec<&str> = list["profiles"]
            .as_array()
            .expect("profiles")
            .iter()
            .map(|p| p["name"].as_str().expect("name"))
            .collect();
        let want: BTreeSet<&str> = library.profiles().map(|p| p.name.as_str()).collect();
        assert_eq!(names.len(), want.len(), "{from}: each once");
        assert_eq!(names.into_iter().collect::<BTreeSet<_>>(), want, "{from}");
    }
    let config: Value = serde_json::from_str(
        &list_json(
            &Library::builtin().expect("built-ins"),
            Some(Path::new("/c.yaml")),
        )
        .to_text(),
    )
    .expect("JSON");
    assert_eq!(config["config"], "/c.yaml");
}

/// The listing order, over every fixture library: every profile once,
/// sources first by name, then filters by `idle_for` (shortest first,
/// those without one last) and name, so `idle-1h` comes before `idle-1d`.
#[test]
fn filters_are_listed_by_how_long_they_require() {
    for (from, library) in libraries() {
        let listed = listing_order(&library);
        let mut names: Vec<&str> = listed.iter().map(|p| p.name.as_str()).collect();
        let split = listed.iter().take_while(|p| p.role() == "source").count();
        assert!(
            listed[split..].iter().all(|p| p.role() == "filter"),
            "{from}: {names:?}"
        );
        assert!(names[..split].is_sorted(), "{from}: {names:?}");
        let key = |p: &Profile| match &p.body {
            Body::Filter(w) => (w.idle_for.is_none(), w.idle_for, p.name.clone()),
            Body::Source(_) => unreachable!("sources come first"),
        };
        let filters: Vec<_> = listed[split..].iter().map(|p| key(p)).collect();
        assert!(filters.is_sorted(), "{from}: {names:?}");
        names.sort_unstable();
        let all: Vec<&str> = library.profiles().map(|p| p.name.as_str()).collect();
        assert_eq!(names, all, "{from}: every profile once");
    }
    let builtin = Library::builtin().expect("built-ins");
    let order: Vec<&str> = listing_order(&builtin)
        .into_iter()
        .map(|p| p.name.as_str())
        .collect();
    let position = |name: &str| order.iter().position(|n| *n == name);
    assert!(position("idle-1h") < position("idle-1d"), "{order:?}");
}

/// `--help` names every built-in, sources on their own line with their
/// category and description, filters together, and says where to learn
/// more. The library is iterated, so a new built-in is covered without
/// editing this test (the CLI test checks the binary the same way).
#[test]
fn help_names_every_built_in() {
    let library = Library::builtin().expect("built-ins");
    let help = help_section(&library);
    let filters = help
        .lines()
        .find(|l| l.trim_start().starts_with("filters: "))
        .expect("a filters line");
    for profile in library.profiles() {
        match &profile.body {
            Body::Source(source) => {
                let line = help
                    .lines()
                    .find(|l| l.split_whitespace().next() == Some(profile.name.as_str()))
                    .unwrap_or_else(|| panic!("no line for {}:\n{help}", profile.name));
                let category = profile.category.as_deref().expect("built-ins have one");
                let description = profile.description.as_deref().expect("built-ins have one");
                assert!(line.contains(category), "{line}");
                assert!(line.contains(&one_line(description)), "{line}");
                assert_eq!(
                    line.ends_with("(roots required: --roots DIR)"),
                    source.roots == Roots::Required,
                    "{line}"
                );
            }
            Body::Filter(_) => assert!(
                filters
                    .trim_start()
                    .trim_start_matches("filters: ")
                    .split(", ")
                    .any(|n| n == profile.name),
                "{} not in {filters}",
                profile.name
            ),
        }
    }
    assert!(help.ends_with(&format!("{}\n", more(&library))), "{help}");
}

/// The help section lists built-ins only: user profiles are what `profiles
/// list` is for.
#[test]
fn help_leaves_user_profiles_to_profiles_list() {
    let (_, actions) = libraries().swap_remove(1);
    let help = help_section(&actions);
    assert_eq!(help, help_section(&Library::builtin().expect("built-ins")));
    assert!(!help.contains("any-cmd"));
}

/// The human listing: a header and one row per profile, columns aligned
/// (every row's ROLE starts where the header's does).
#[test]
fn list_text_is_one_aligned_table() {
    let (_, actions) = libraries().swap_remove(1);
    let text = list_text(&actions);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), actions.profiles().count() + 1);
    let column = lines[0].find("ROLE").expect("a ROLE column");
    for line in &lines[1..] {
        let role = &line[column..];
        assert!(
            role.starts_with("source ") || role.starts_with("filter "),
            "{line}"
        );
    }
    let go = lines
        .iter()
        .find(|l| l.starts_with("go-parent "))
        .expect("go-parent");
    for cell in ["user", "fs", "command go ", "required", "refuse"] {
        assert!(go.contains(cell), "{go}");
    }
    let idle = lines
        .iter()
        .find(|l| l.starts_with("idle-1h "))
        .expect("idle-1h");
    let cells: Vec<&str> = idle.split_whitespace().collect();
    assert_eq!(
        &cells[..5],
        ["idle-1h", "built_in", "filter", "filters", "-"],
        "{idle}"
    );
}

/// An unknown name is answered with the names it was probably meant to be.
#[test]
fn unknown_names_get_close_names() {
    let library = Library::builtin().expect("built-ins");
    for (name, want) in [
        ("cargo", &["cargo-registry-src", "cargo-target"][..]),
        ("profile:cargo-target", &["cargo-target"]),
        ("cargo-targets", &["cargo-target"]),
        ("idle-5d", &["idle-1d", "idle-2d", "idle-3d", "idle-7d"]),
        ("idle-1w", &["idle-1d", "idle-1h"]),
        ("", &[]),
    ] {
        assert_eq!(close_names(&library, name), want, "{name:?}");
    }
    assert_eq!(
        unknown_name(&library, "cargo"),
        "no profile is named `cargo`; close names: `cargo-registry-src`, `cargo-target`"
    );
    assert_eq!(
        unknown_name(&library, "nope"),
        "no profile is named `nope`; `diskgc profiles list` lists them"
    );
}

/// A string `show` writes reads back as the same string, whatever YAML
/// would otherwise make of it (a number, a boolean, an alias, a mapping).
#[test]
fn yaml_scalars_read_back_as_written() {
    let nasty = [
        "plain",
        "/tmp/x",
        "pytest 8.3",
        "true",
        "False",
        "null",
        "~",
        "1.5",
        "0x1f",
        "*x",
        "&a",
        "!t",
        "-x",
        "- x",
        "a: b",
        "a #b",
        "#c",
        "{uid}",
        "[x]",
        "a,b",
        "'q'",
        "\"d\"",
        " lead",
        "trail ",
        "",
        "tab\tx",
        "new\nline",
        "é",
        "%x",
        "@x",
        "`x`",
        "?x",
        "|",
        ">",
        "yes",
        "No",
        "on",
        "y",
        "1",
        "-1",
        ".inf",
        "a\\b",
    ];
    let list: Vec<String> = nasty.iter().map(|s| (*s).to_owned()).collect();
    let yaml = yaml_list(&list);
    let back: Vec<String> =
        serde_saphyr::from_str(&yaml).unwrap_or_else(|e| panic!("{e}:\n{yaml}"));
    assert_eq!(back, list, "{yaml}");
    for text in nasty {
        let line = format!("k: {}", yaml_scalar(text));
        let back: std::collections::BTreeMap<String, String> =
            serde_saphyr::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert_eq!(back["k"], text, "{line}");
    }
}

#[test]
fn edit_distance_counts_insertions_deletions_and_substitutions() {
    for (a, b, d) in [
        ("", "", 0),
        ("a", "", 1),
        ("idle-5d", "idle-1d", 1),
        ("idle-5d", "idle-10d", 2),
        ("kitten", "sitting", 3),
    ] {
        assert_eq!(edit_distance(a, b), d, "{a} {b}");
        assert_eq!(edit_distance(b, a), d, "{b} {a}");
    }
}

#[test]
fn long_unknown_profile_name_has_no_costly_suggestions_or_raw_controls() {
    let library = Library::with_user_profiles(&[]).unwrap();
    let name = format!("{}\n", "cargo-target".repeat(10_000));
    assert!(close_names(&library, &name).is_empty());
    let error = unknown_name(&library, &name);
    assert!(!error.contains('\n'), "{error}");
    assert!(error.contains("\\n"), "{error}");
}

/// The listing names the program a command action runs, also behind `env`.
#[test]
fn the_listing_names_the_program_of_a_command() {
    let argv = |words: &[&str]| words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
    for (words, want) in [
        (&["pip", "--cache-dir", "{path}"][..], Some("pip")),
        (&["env", "GOCACHE={path}", "go", "clean"], Some("go")),
        (&["/usr/bin/env", "-i", "A=b", "npm", "cache"], Some("npm")),
        (&["env"], Some("env")),
        (&[], None),
    ] {
        assert_eq!(program(&argv(words)), want, "{words:?}");
    }
}
