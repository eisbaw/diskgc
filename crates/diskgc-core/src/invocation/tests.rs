//! The composition's static refusals, tested without a host.

use std::path::Path;

use super::*;
use crate::config;
use crate::profile::{Body, Finds, Library, Roots};

#[test]
fn root_mode_checks_the_final_cli_selection() {
    let library = Library::builtin().expect("built-ins");
    let roots = ["/home/mark".to_owned()];
    let command =
        Tier::select("profile:pip-cache", &library, Some(&roots), 1000).expect("command source");
    let deletion = Tier::select("profile:go-mod-cache", &library, Some(&roots), 1000)
        .expect("deletion source");
    assert!(validate_selected_root_action(false, &command).is_ok());
    assert!(validate_selected_root_action(true, &deletion).is_ok());
    assert!(
        validate_selected_root_action(true, &command)
            .unwrap_err()
            .contains("command actions are refused")
    );
    let docker =
        Tier::select("profile:docker-build-cache", &library, None, 1000).expect("Docker source");
    assert!(
        validate_selected_root_action(true, &docker)
            .unwrap_err()
            .contains("owners: cannot bound")
    );
}

#[test]
fn root_mode_refuses_each_uid_placeholder_input() {
    let library = Library::builtin().expect("built-ins");
    let Some(Body::Source(source)) = library.get("node-compile-cache").map(|p| &p.body) else {
        panic!("missing built-in")
    };
    let mut source = (**source).clone();
    source.roots = Roots::Required;
    let Finds::Paths {
        patterns, exclude, ..
    } = &mut source.finds
    else {
        panic!("expected fs source")
    };
    *patterns = vec!["cache".to_owned()];
    exclude.clear();
    let check =
        |source: &Source, roots: &[String]| reject_root_uid_placeholder("fixture", source, roots);
    assert!(check(&source, &[]).is_ok());
    assert!(
        check(&source, &["/tmp/u-{uid}".to_owned()])
            .unwrap_err()
            .contains("--roots")
    );
    source.roots = Roots::Fixed(vec!["/tmp/u-{uid}".to_owned()]);
    assert!(check(&source, &[]).unwrap_err().contains("find.roots"));
    source.roots = Roots::Required;
    if let Finds::Paths { patterns, .. } = &mut source.finds {
        *patterns = vec!["u-{uid}".to_owned()];
    }
    assert!(check(&source, &[]).unwrap_err().contains("find.paths"));
    if let Finds::Paths {
        patterns, exclude, ..
    } = &mut source.finds
    {
        *patterns = vec!["cache".to_owned()];
        *exclude = vec!["u-{uid}".to_owned()];
    }
    assert!(check(&source, &[]).unwrap_err().contains("find.exclude"));
    if let Finds::Paths { exclude, .. } = &mut source.finds {
        exclude.clear();
    }
    source.where_.app_not_running = Some("/tmp/u-{uid}/SingletonLock".to_owned());
    assert!(
        check(&source, &[])
            .unwrap_err()
            .contains("where.app_not_running")
    );
}

#[test]
fn root_mode_checks_uid_placeholder_in_second_union_source() {
    let specs = crate::profile::parse_profiles(concat!(
        "- name: plain-union\n",
        "  find: {roots: [/tmp], paths: [plain-*], type: dir}\n",
        "  action: delete\n",
        "  examples: {match: [/tmp/plain-x]}\n",
        "- name: uid-union\n",
        "  find: {roots: [/tmp], paths: [\"uid-{uid}\"], type: dir}\n",
        "  action: delete\n",
        "  examples: {match: [/tmp/uid-1000]}\n",
    ))
    .expect("fixture profiles");
    let library = Library::with_user_profiles(&specs).expect("fixture library");
    let selected = Tier::select(
        "profile:plain-union || profile:uid-union",
        &library,
        None,
        0,
    )
    .expect("filesystem union");
    let error = reject_root_uid_selected(&selected, &library, &[])
        .expect_err("second source's uid placeholder must be refused");
    assert!(error.contains("selected profile `uid-union`"), "{error}");
    assert!(error.contains("find.paths"), "{error}");
}

#[test]
fn root_mode_checks_uid_placeholder_in_negated_source() {
    let specs = crate::profile::parse_profiles(concat!(
        "- name: plain-source\n",
        "  find: {roots: [/tmp], paths: [plain-*], type: dir}\n",
        "  action: delete\n",
        "  examples: {match: [/tmp/plain-x]}\n",
        "- name: uid-exclusion\n",
        "  find: {roots: [/tmp], paths: [\"uid-{uid}\"], type: dir}\n",
        "  action: delete\n",
        "  examples: {match: [/tmp/uid-1000]}\n",
    ))
    .expect("fixture profiles");
    let library = Library::with_user_profiles(&specs).expect("fixture library");
    let selected = Tier::select(
        "profile:plain-source && !profile:uid-exclusion",
        &library,
        None,
        0,
    )
    .expect("negated filesystem source");
    let error = reject_root_uid_selected(&selected, &library, &[])
        .expect_err("negated source's uid placeholder must be refused");
    assert!(
        error.contains("selected profile `uid-exclusion`"),
        "{error}"
    );
    assert!(error.contains("find.paths"), "{error}");
}

#[test]
fn configured_random_ranking_is_reproducible_and_preserves_group_order() {
    let source = "version: 1\ngroups:\n  - name: stale\n    ranking: random\n    tiers:\n      - name: first\n        select: profile:cargo-target\n        roots: [/tmp]\n      - name: second\n        select: profile:cargo-target\n        roots: [/tmp]\n      - name: third\n        select: profile:cargo-target\n        roots: [/tmp]\n  - name: pressure\n    tiers:\n      - name: last\n        select: profile:cargo-target\n        roots: [/tmp]\n";
    let config = config::parse(Path::new("/tmp/policy.yaml"), source.as_bytes())
        .expect("valid static policy");
    let (first, first_timeouts) = configured_tiers(&config, 1000, false, Some(42)).unwrap();
    let (again, again_timeouts) = configured_tiers(&config, 1000, false, Some(42)).unwrap();
    assert_eq!(first, again);
    assert_eq!(first_timeouts, again_timeouts);
    assert_eq!(
        first
            .iter()
            .map(|tier| tier.name.as_str())
            .collect::<Vec<_>>(),
        ["first", "third", "second", "last"]
    );
    assert_eq!(first.last().expect("pressure tier").name, "last");
    assert!(first.iter().take(3).all(|tier| tier.group == "stale"));
    let error = configured_tiers(&config, 1000, false, None).expect_err("seed required");
    assert!(error.contains("needs a random seed"), "{error}");
}

#[test]
fn configured_execution_accepts_best_fit() {
    let source = "version: 1\ngroups:\n  - name: stale\n    tiers:\n      - name: cargo\n        select: profile:cargo-target\n        roots: [/tmp]\n        order: best-fit\n";
    let config = config::parse(Path::new("/tmp/policy.yaml"), source.as_bytes())
        .expect("valid static policy");
    let (tiers, _) = configured_tiers(&config, 1000, false, None).expect("best-fit is supported");
    assert_eq!(tiers[0].order, crate::config::Order::BestFit);
}

#[test]
fn configured_root_mode_checks_later_groups_before_execution() {
    let source = concat!(
        "version: 1\nprofiles:\n",
        "  - name: uid-in-later-group\n",
        "    find: {roots: [/tmp], paths: [\"uid-{uid}\"], type: dir}\n",
        "    action: delete\n",
        "    examples: {match: [/tmp/uid-1000]}\n",
        "groups:\n",
        "  - name: first\n    tiers:\n",
        "      - name: cargo\n        select: profile:cargo-target\n        roots: [/tmp]\n",
        "  - name: later\n    tiers:\n",
        "      - name: uid\n        select: profile:uid-in-later-group\n",
    );
    let config = config::parse(Path::new("/tmp/policy.yaml"), source.as_bytes())
        .expect("valid static policy");
    let error = configured_tiers(&config, 0, true, None)
        .expect_err("later uid placeholder must be refused");
    assert!(
        error.contains("uid-in-later-group") && error.contains("find.paths"),
        "{error}"
    );
}

/// `source` under the proposed 0.1 catalog, after checking that the
/// development catalog loads it (`expectation`): a refusal is the 0.1
/// catalog's.
fn under_both_catalogs(source: &[u8], expectation: &str) -> Result<Config, String> {
    let path = Path::new("/tmp/policy.yaml");
    Catalog::Development
        .config(path, source, ConfigUse::Run)
        .expect(expectation);
    Catalog::ProposedRelease01.config(path, source, ConfigUse::Run)
}

#[test]
fn proposed_catalog_refuses_excluded_config_tiers_even_when_later() {
    for excluded in ["docker-build-cache", "pip-cache", "pytest-cache"] {
        let roots = if excluded == "docker-build-cache" {
            ""
        } else {
            "        roots: [/tmp]\n"
        };
        let source = format!(
            "version: 1\ngroups:\n  - name: first\n    tiers:\n      - name: safe\n        select: profile:cargo-target\n        roots: [/tmp]\n  - name: later\n    tiers:\n      - name: excluded\n        select: profile:{excluded}\n{roots}"
        );
        let error = under_both_catalogs(source.as_bytes(), "valid full catalog policy")
            .expect_err("later tier must fail before execution");
        assert!(
            error.contains("later") && error.contains(excluded),
            "{error}"
        );
    }
}

#[test]
fn proposed_catalog_refuses_operator_override_of_an_allowed_name() {
    let source = concat!(
        "version: 1\nprofiles:\n",
        "  - name: cargo-target\n    override: true\n",
        "    kind: cargo-target\n    find: {roots: required}\n",
        "groups:\n  - name: first\n    tiers:\n",
        "      - name: cargo\n        select: profile:cargo-target\n        roots: [/tmp]\n",
    );
    let error = under_both_catalogs(source.as_bytes(), "valid full catalog override")
        .expect_err("operator override must not impersonate approved Cargo");
    assert!(
        error.contains("user profiles are not available in diskgc 0.1")
            && error.contains("`cargo-target`"),
        "{error}"
    );
}

#[test]
fn proposed_catalog_accepts_only_checkout_policy_for_an_admitted_builtin() {
    let source =
        "version: 1\nprofile_policies:\n  - profile: cargo-target\n    checkouts: refuse\n";
    let config = under_both_catalogs(source.as_bytes(), "narrow policy parses")
        .expect("Cargo remains admitted");
    assert_eq!(
        config.library.checkouts("cargo-target"),
        Some(crate::engine::Checkouts::Refuse)
    );
    assert_eq!(
        config.library.get("cargo-target").unwrap().origin,
        crate::profile::Origin::BuiltIn
    );

    // Narrowing and keeping a built-in's setting are accepted; widening a
    // refusing built-in (nix-shell-rc) is not.
    for (name, checkouts) in [
        ("nix-shell-rc", "refuse"),
        ("pytest-tmp", "refuse"),
        ("pytest-tmp", "allow"),
    ] {
        let source = format!(
            "version: 1\nprofile_policies:\n  - profile: {name}\n    checkouts: {checkouts}\n"
        );
        under_both_catalogs(source.as_bytes(), "policy parses").expect(name);
    }
    let source = "version: 1\nprofile_policies:\n  - profile: nix-shell-rc\n    checkouts: allow\n";
    let error = under_both_catalogs(source.as_bytes(), "development accepts widening")
        .expect_err("0.1 refuses widening a refusing built-in");
    assert!(
        error.contains("nix-shell-rc") && error.contains("may only narrow"),
        "{error}"
    );

    for excluded in ["npm-cache", "uv-cache"] {
        let source = format!(
            "version: 1\nprofile_policies:\n  - profile: {excluded}\n    checkouts: allow\n"
        );
        let error = under_both_catalogs(source.as_bytes(), "full library accepts policy")
            .expect_err("excluded profile policy must not pass release gate");
        assert!(
            error.contains(&format!(
                "`{excluded}` is a built-in that diskgc 0.1 does not ship"
            )),
            "{error}"
        );
    }
}

#[test]
fn proposed_catalog_refuses_user_command_action_before_selection() {
    let source = concat!(
        "version: 1\nprofiles:\n",
        "  - name: custom-cleaner\n",
        "    find: {roots: required, paths: [.cache/tool], type: dir}\n",
        "    action: {command: [tool, clean, '{path}']}\n",
        "    examples: {match: [/tmp/.cache/tool]}\n",
        "groups:\n  - name: first\n    tiers:\n",
        "      - name: cleaner\n        select: profile:custom-cleaner\n        roots: [/tmp]\n",
    );
    let error = under_both_catalogs(source.as_bytes(), "valid full catalog command policy")
        .expect_err("user command action must be refused before any backend");
    assert!(
        error.contains("user profiles are not available in diskgc 0.1")
            && error.contains("`custom-cleaner`"),
        "{error}"
    );
}

/// TASK-146: a development-library configuration is refused under the 0.1
/// catalog with every refusal in one message, each in a user's words (a
/// built-in "this release does not ship", never "no profile named"), and
/// with what 0.1 does ship.
#[test]
fn proposed_catalog_names_every_refusal_at_once_in_user_words() {
    let source = concat!(
        "version: 1\n",
        "profile_policies:\n  - profile: npm-cache\n    checkouts: allow\n",
        "protect:\n  - {profile: pytest-cache, roots: [/home/mark/src]}\n",
        "groups:\n  - name: stale\n    tiers:\n",
        "      - name: node\n        select: profile:node-compile-cache && filter:idle-3d\n",
        "      - name: cargo\n        select: profile:cargo-target && filter:idle-7d\n",
        "        roots: [/home/mark/src]\n",
        "  - name: pressure\n    tiers:\n",
        "      - name: docker\n        select: profile:docker-dangling\n",
    );
    let error = under_both_catalogs(source.as_bytes(), "the development library loads it")
        .expect_err("0.1 refuses it");
    for part in [
        "4 part(s) are not available in this release",
        "profile_policies: `npm-cache` is a built-in that diskgc 0.1 does not ship",
        "group `stale` tier `node`",
        "`node-compile-cache` is a built-in that diskgc 0.1 does not ship",
        "group `pressure` tier `docker`",
        "protect: `pytest-cache` is a built-in that diskgc 0.1 does not ship",
        "diskgc 0.1 ships the sources cargo-target, nix-shell-rc, pytest-tmp and the filters",
    ] {
        assert!(error.contains(part), "{part:?} missing from: {error}");
    }
    for jargon in [
        "proposed",
        "operator-authored",
        "no profile or filter is named",
    ] {
        assert!(!error.contains(jargon), "{jargon:?} in: {error}");
    }
}

/// TASK-139 (the same class as TASK-146): `--select` of a built-in the
/// 0.1 catalog leaves out says so, not "no profile named".
#[test]
fn proposed_catalog_select_of_a_withheld_builtin_names_the_release() {
    let library = Catalog::ProposedRelease01
        .library(Library::builtin().expect("built-ins load"))
        .expect("0.1 catalog");
    let error = crate::select::compile("profile:pytest-cache && filter:idle-1d", &library)
        .expect_err("withheld")
        .to_string();
    assert!(
        error.contains("`pytest-cache` is a built-in that diskgc 0.1 does not ship")
            && !error.contains("no profile or filter is named"),
        "{error}"
    );
    // A name no built-in has stays unknown, with its suggestions.
    let error = crate::select::compile("profile:pytest-tnp", &library)
        .expect_err("unknown")
        .to_string();
    assert!(
        error.contains("no profile or filter is named `pytest-tnp`; did you mean `pytest-tmp`?"),
        "{error}"
    );
}

/// TASK-146: every configuration example in docs/reference.md
/// (Configuration file) and docs/example.yaml loads under the catalog it
/// claims. One marked "Development build only" loads in development builds
/// and is refused by 0.1; every other one loads under 0.1.
#[test]
fn documented_config_examples_load_under_the_catalog_they_claim() {
    let doc = crate::doc::reference_md();
    let section = crate::doc::section(&doc, "## Configuration file");
    let mut blocks = crate::doc::code_blocks(&section, "yaml");
    assert!(blocks.len() >= 2, "a 0.1 and a development example");
    blocks.push(
        std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/example.yaml"
        ))
        .expect("docs/example.yaml (the flake's test source must include it)"),
    );
    let path = Path::new("/docs/example.yaml");
    for block in &blocks {
        let development = block.starts_with("# Development build only");
        let release = Catalog::ProposedRelease01.config(path, block.as_bytes(), ConfigUse::Run);
        if development {
            Catalog::Development
                .config(path, block.as_bytes(), ConfigUse::Run)
                .unwrap_or_else(|e| panic!("development example: {e}\n{block}"));
            assert!(
                release.is_err(),
                "labelled development only, 0.1 loads it:\n{block}"
            );
        } else if let Err(e) = release {
            panic!("unlabelled example refused by 0.1: {e}\n{block}");
        }
    }
}
