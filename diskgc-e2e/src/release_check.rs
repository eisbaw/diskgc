//! The release package contains neither the test driver nor the controlled
//! `Host` wrapper (docs/testing.md, Controlled pause and kill points): the
//! one sanctioned exception to "no hooks in the product" stays outside the
//! product build.
//!
//! The flake check `release-excludes-e2e` runs `diskgc-e2e-release-check`
//! on three inputs:
//!
//! - `--shipped BIN`: the release package's `diskgc` binary. Its raw bytes
//!   (symbol table and read-only data alike) must contain none of the
//!   [`needles`]: a symbol of the diskgc-e2e crate, the wrapper's type in
//!   any crate, the wrapper's control-line prefix, the driver's usage line.
//!   Moving the wrapper into a product crate still trips the last three.
//! - `--dep-tree FILE`: `cargo tree --package diskgc --edges normal,build`
//!   of the workspace as built (`--locked`), one package per line: it must
//!   not name `diskgc-e2e`.
//! - `--control BIN`: a binary that links the driver and the wrapper
//!   (`diskgc-e2e-driver`). It must contain every needle, so the needles are
//!   what that code compiles to, not stale text: the built-in bite proof.
//!
//! Two more positive controls keep the checks from passing vacuously: the
//! shipped binary must have a symbol table (the mangled crate name
//! `diskgc_linux` appears only in symbols; source paths say
//! `diskgc-linux`), and the dependency tree must name `diskgc-linux`.
//! If a later release build strips symbols, this check fails and must be
//! revised with it, rather than silently checking less.

use std::path::PathBuf;

use crate::controlled::CONTROL_PREFIX;
use crate::{driver, exit};

const USAGE: &str = "\
usage: diskgc-e2e-release-check --shipped BIN --dep-tree FILE --control BIN\n\
       diskgc-e2e-release-check --clock-offset-excluded BIN --clock-offset-control BIN";

/// The complete runtime switch for TASK-89. The positive control is a
/// development binary built from the same source, while the excluded binary
/// has the release-without-sandbox-gate feature enabled.
const CLOCK_OFFSET_NEEDLE: &str = "/.diskgc-test-clock-offset";

pub fn check_clock_offset(excluded: &[u8], control: &[u8]) -> Vec<String> {
    let mut problems = Vec::new();
    if contains(excluded, CLOCK_OFFSET_NEEDLE) {
        problems.push(format!(
            "feature-built binary contains test clock offset path {CLOCK_OFFSET_NEEDLE:?}"
        ));
    }
    if !contains(control, CLOCK_OFFSET_NEEDLE) {
        problems.push(format!(
            "positive control: development binary lacks test clock offset path \
             {CLOCK_OFFSET_NEEDLE:?}; the exclusion check would be vacuous"
        ));
    }
    problems
}

/// The crate whose code must never reach the release package.
pub const E2E_CRATE: &str = "diskgc-e2e";

/// Byte strings only the driver and the wrapper produce, with what each is.
pub fn needles() -> Vec<(&'static str, &'static str)> {
    vec![
        ("diskgc_e2e", "a symbol of the diskgc-e2e crate"),
        // v0 symbol mangling writes each identifier as its length and name;
        // the positive control fails if the scheme changes.
        ("10Controlled", "the controlled Host wrapper's type"),
        (CONTROL_PREFIX, "the wrapper's control-line prefix"),
        (
            driver::USAGE.lines().next().unwrap_or(driver::USAGE),
            "the driver's usage line",
        ),
    ]
}

/// Present in the symbol table of a binary built from this workspace with
/// symbols: the mangled name of the crate `LinuxHost` lives in.
const SYMBOL_CONTROL: &str = "diskgc_linux";

/// Named by the dependency tree of the real `diskgc` package.
const TREE_CONTROL: &str = "diskgc-linux";

fn contains(haystack: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// The package names of a `cargo tree --prefix none` listing (`name vX.Y.Z
/// (source)` per line; `(*)` marks repeats).
fn tree_packages(tree: &str) -> Vec<&str> {
    tree.lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect()
}

/// Every problem found; empty when the release package is clean and every
/// positive control held.
pub fn check(shipped: &[u8], dep_tree: &str, control: &[u8]) -> Vec<String> {
    let mut problems = Vec::new();
    for (needle, what) in needles() {
        if contains(shipped, needle) {
            problems.push(format!(
                "the release binary contains {what} ({needle:?}): the test driver or the \
                 controlled Host wrapper was linked into the product"
            ));
        }
        if !contains(control, needle) {
            problems.push(format!(
                "positive control: the driver binary does not contain {what} ({needle:?}); \
                 the needle is stale, so the check would miss that code"
            ));
        }
    }
    if !contains(shipped, SYMBOL_CONTROL) {
        problems.push(format!(
            "positive control: the release binary has no {SYMBOL_CONTROL:?} symbol: it was \
             stripped, so symbol needles prove nothing; revise this check with the build"
        ));
    }
    let packages = tree_packages(dep_tree);
    if packages.contains(&E2E_CRATE) {
        problems.push(format!(
            "the release package's dependency tree contains {E2E_CRATE}:\n{dep_tree}"
        ));
    }
    if !packages.contains(&TREE_CONTROL) {
        problems.push(format!(
            "positive control: the dependency tree does not name {TREE_CONTROL}; it is not \
             the tree of the diskgc package:\n{dep_tree}"
        ));
    }
    problems
}

fn read(path: &PathBuf) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))
}

/// Entry point of `diskgc-e2e-release-check`; returns the exit code.
pub fn main(args: &[String]) -> i32 {
    if let [flag, excluded, control_flag, control] = args
        && flag == "--clock-offset-excluded"
        && control_flag == "--clock-offset-control"
    {
        let inputs = read(&PathBuf::from(excluded)).and_then(|excluded_bytes| {
            read(&PathBuf::from(control)).map(|bytes| (excluded_bytes, bytes))
        });
        let (excluded_bytes, control_bytes) = match inputs {
            Ok(inputs) => inputs,
            Err(error) => {
                eprintln!("diskgc-e2e-release-check: {error}");
                return exit::FAIL;
            }
        };
        let problems = check_clock_offset(&excluded_bytes, &control_bytes);
        if problems.is_empty() {
            println!(
                "test clock offset absent from feature-built binary and present in development control"
            );
            return exit::PASS;
        }
        for problem in problems {
            eprintln!("diskgc-e2e-release-check: {problem}");
        }
        return exit::FAIL;
    }
    let paths = match args {
        [a, shipped, b, tree, c, control]
            if a == "--shipped" && b == "--dep-tree" && c == "--control" =>
        {
            [shipped, tree, control].map(PathBuf::from)
        }
        _ => {
            eprintln!("{USAGE}");
            return exit::USAGE;
        }
    };
    let [shipped, tree, control] = &paths;
    let inputs = read(shipped).and_then(|s| {
        let tree = String::from_utf8(read(tree)?)
            .map_err(|e| format!("{}: not UTF-8: {e}", tree.display()))?;
        Ok((s, tree, read(control)?))
    });
    let (shipped_bytes, tree_text, control_bytes) = match inputs {
        Ok(inputs) => inputs,
        Err(e) => {
            eprintln!("diskgc-e2e-release-check: {e}");
            return exit::FAIL;
        }
    };
    let problems = check(&shipped_bytes, &tree_text, &control_bytes);
    if problems.is_empty() {
        println!(
            "release package clean: {} has none of {} needles (all found in {}), and its \
             dependency tree ({} packages) excludes {E2E_CRATE}",
            shipped.display(),
            needles().len(),
            control.display(),
            tree_packages(&tree_text).len()
        );
        exit::PASS
    } else {
        for problem in &problems {
            eprintln!("diskgc-e2e-release-check: {problem}");
        }
        exit::FAIL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_offset_exclusion_has_a_positive_control() {
        assert!(check_clock_offset(b"feature build", CLOCK_OFFSET_NEEDLE.as_bytes()).is_empty());
        assert_eq!(
            check_clock_offset(
                CLOCK_OFFSET_NEEDLE.as_bytes(),
                CLOCK_OFFSET_NEEDLE.as_bytes()
            )
            .len(),
            1
        );
        assert_eq!(
            check_clock_offset(b"feature build", b"development without offset").len(),
            1
        );
    }

    const TREE: &str = "\
diskgc v0.1.0 (/build/source/crates/diskgc)
diskgc-core v0.1.0 (/build/source/crates/diskgc-core)
diskgc-linux v0.1.0 (/build/source/crates/diskgc-linux)
diskgc-core v0.1.0 (/build/source/crates/diskgc-core) (*)
rustix v1.1.5
";

    fn all_needles() -> Vec<u8> {
        needles()
            .iter()
            .flat_map(|(n, _)| n.bytes().chain(*b"\0"))
            .collect()
    }

    fn clean_binary() -> Vec<u8> {
        b"\x7fELF ... _ZN12diskgc_linux4host9LinuxHost3new17h0123E ... diskgc 0.1.0".to_vec()
    }

    #[test]
    fn a_clean_release_passes() {
        assert_eq!(
            check(&clean_binary(), TREE, &all_needles()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn any_needle_in_the_release_binary_fails() {
        for (needle, what) in needles() {
            let mut shipped = clean_binary();
            shipped.extend_from_slice(needle.as_bytes());
            let problems = check(&shipped, TREE, &all_needles());
            assert_eq!(problems.len(), 1, "{needle:?}: {problems:?}");
            assert!(problems[0].contains(what), "{problems:?}");
        }
    }

    #[test]
    fn the_e2e_crate_in_the_dependency_tree_fails() {
        let tree = format!("{TREE}diskgc-e2e v0.0.0 (/build/source/diskgc-e2e)\n");
        let problems = check(&clean_binary(), &tree, &all_needles());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("dependency tree contains diskgc-e2e"));
        // A crate whose name merely starts with it is not it.
        let tree = format!("{TREE}diskgc-e2e-extras v1.0.0\n");
        assert!(check(&clean_binary(), &tree, &all_needles()).is_empty());
    }

    #[test]
    fn vacuous_inputs_fail_their_positive_controls() {
        let stripped = b"\x7fELF diskgc 0.1.0".to_vec();
        let problems = check(&stripped, TREE, &all_needles());
        assert!(
            problems.iter().any(|p| p.contains("stripped")),
            "{problems:?}"
        );

        let problems = check(&clean_binary(), "", &all_needles());
        assert!(
            problems.iter().any(|p| p.contains("not the tree")),
            "{problems:?}"
        );

        let (first, what) = needles()[0];
        let control: Vec<u8> = all_needles()
            .split(|b| *b == 0)
            .filter(|n| *n != first.as_bytes())
            .flat_map(|n| n.iter().copied().chain([0]))
            .collect();
        let problems = check(&clean_binary(), TREE, &control);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("stale") && problems[0].contains(what));
    }

    #[test]
    fn this_test_binary_links_the_wrapper_and_is_flagged() {
        // The unit-test binary links the whole crate: as a release binary it
        // must fail (a real binary, not hand-made bytes).
        let own = std::fs::read(std::env::current_exe().expect("own path")).expect("read self");
        let problems = check(&own, TREE, &own);
        assert!(
            problems.iter().any(|p| p.contains("diskgc-e2e crate")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("control-line prefix")),
            "{problems:?}"
        );
    }
}
