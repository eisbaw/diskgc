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
