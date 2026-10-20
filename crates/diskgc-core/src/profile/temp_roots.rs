//! A finite, versioned set of temp-directory prefixes for built-in `/tmp`
//! profiles. This is an internal library preset, not user YAML syntax.
//! Changing this list changes a built-in's match set: add a v2 preset and
//! a new profile name rather than editing v1.

use super::Origin;

const PYTEST_BASE: &str = "pytest-of-*/pytest-*/*";
const MKTEMP_BASE: &str = "tmp.*";
const NODE_COMPILE_CACHE_BASE: &str = "node-compile-cache";
const PLAYWRIGHT_BASE: &str = "playwright*";
const RENODE_BASE: &str = "renode-*";

/// Relative to each fixed root of an opting-in built-in. The empty prefix
/// covers the root itself. Nested-shell patterns end after two shell levels
/// (or Claude's temp root plus two); no recursion or profile reference is
/// involved.
pub(super) const TEMP_PREFIXES_V1: &[&str] = &[
    "",
    "nix-shell.*",
    "tmp.*",
    "nix-shell-*-*",
    "nix-develop-*-*",
    "zephyr-*-run.*",
    "claude-{uid}",
    "nix-shell.*/nix-shell.*",
    "nix-shell.*/tmp.*",
    "tmp.*/nix-shell.*",
    "claude-{uid}/nix-shell.*",
    "claude-{uid}/tmp.*",
    "claude-{uid}/nix-shell.*/nix-shell.*",
];

fn expand(base: &str) -> Vec<String> {
    TEMP_PREFIXES_V1
        .iter()
        .map(|prefix| {
            if prefix.is_empty() {
                base.to_owned()
            } else {
                format!("{prefix}/{base}")
            }
        })
        .collect()
}

/// Effective patterns for a built-in. The first form is the compact base
/// in `builtin.yaml`; the second is `profiles show`'s explicit expanded
/// form, which must load back to the same profile. Any other definition
/// under the same built-in name is refused rather than silently changing
/// the name's meaning.
pub(super) fn effective(
    origin: Origin,
    name: &str,
    patterns: Vec<String>,
) -> Result<Vec<String>, String> {
    if origin != Origin::BuiltIn {
        return Ok(patterns);
    }
    let base = match name {
        "pytest-tmp" => PYTEST_BASE,
        "mktemp-dir" => MKTEMP_BASE,
        "node-compile-cache" => NODE_COMPILE_CACHE_BASE,
        "playwright-tmp" => PLAYWRIGHT_BASE,
        "renode-tmp" => RENODE_BASE,
        _ => return Ok(patterns),
    };
    let expanded = expand(base);
    if patterns == [base] || patterns == expanded {
        Ok(expanded)
    } else {
        Err(format!(
            "built-in `{name}` must use the frozen temp-prefix v1 preset with base `{base}`; change the profile name to change its matching set"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{code, reference_md, section, table};

    #[test]
    fn v1_has_one_root_and_a_finite_two_shell_bound() {
        assert_eq!(
            TEMP_PREFIXES_V1,
            [
                "",
                "nix-shell.*",
                "tmp.*",
                "nix-shell-*-*",
                "nix-develop-*-*",
                "zephyr-*-run.*",
                "claude-{uid}",
                "nix-shell.*/nix-shell.*",
                "nix-shell.*/tmp.*",
                "tmp.*/nix-shell.*",
                "claude-{uid}/nix-shell.*",
                "claude-{uid}/tmp.*",
                "claude-{uid}/nix-shell.*/nix-shell.*",
            ]
        );
        assert!(TEMP_PREFIXES_V1.iter().all(|p| p.split('/').count() <= 3));
    }

    #[test]
    fn show_form_is_idempotent_and_a_mutated_name_is_rejected() {
        for (name, base) in [
            ("pytest-tmp", PYTEST_BASE),
            ("mktemp-dir", MKTEMP_BASE),
            ("node-compile-cache", NODE_COMPILE_CACHE_BASE),
            ("playwright-tmp", PLAYWRIGHT_BASE),
            ("renode-tmp", RENODE_BASE),
        ] {
            let expanded = effective(Origin::BuiltIn, name, vec![base.to_owned()]).expect("preset");
            assert_eq!(
                effective(Origin::BuiltIn, name, expanded.clone()).expect("show form"),
                expanded
            );
            assert!(effective(Origin::BuiltIn, name, vec!["other".into()]).is_err());
        }
        assert_eq!(
            effective(Origin::User, "pytest-tmp", vec!["other".into()]).expect("user"),
            ["other"]
        );
    }

    #[test]
    fn reference_lists_the_frozen_prefixes() {
        let library = section(&reference_md(), "### Built-in profile library");
        let rows = table(&library, "| Prefix (relative to the root) |");
        let documented: Vec<&str> = rows
            .iter()
            .map(|row| {
                if row[0] == "*(empty)*" {
                    ""
                } else {
                    code(&row[0])
                }
            })
            .collect();
        assert_eq!(documented, TEMP_PREFIXES_V1);
    }
}
