//! What the test image is (docs/testing.md, Container setup: Images): a
//! descriptor baked into every test image beside the sandbox marker, read
//! by the harness inside the container. It says, per image, what would
//! otherwise be assumed about Nix:
//!
//! - which directories of the image are bind-mounted read-only over
//!   themselves at setup (`read-only`), so the deletion oracle's snapshot
//!   skips them (the Nix image's `/nix/store`, the Debian image's `/usr`);
//! - whether the image has Cargo toolchains under test (`toolchains
//!   manifest`: the manifest [`crate::cargo_under_test::MANIFEST`] must be
//!   there, and the Cargo tests are listed per toolchain) or none
//!   (`toolchains none`: no Cargo test is listed, and no manifest is read);
//! - which tests it runs (`tests all`, or `tests portable`: only
//!   [`crate::suite::PORTABLE_TESTS`]).
//!
//! The format is one `KEY VALUE` per line, `#` comments and blank lines
//! ignored; every key but `read-only` exactly once, unknown keys and values
//! refused. A missing or unreadable descriptor fails the listing and every
//! test's setup: nothing falls back to "a Nix image".

use std::path::PathBuf;

/// Where the image carries its descriptor (flake.nix, the e2e images'
/// `fakeRootCommands`).
pub const DESCRIPTOR: &str = "/.diskgc-test-image";

/// Whether the image has Cargo toolchains under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toolchains {
    /// The toolchain manifest is required; Cargo tests run per toolchain.
    Manifest,
    /// No toolchains: no manifest is read, no Cargo test is listed.
    None,
}

/// Which tests the image runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tests {
    All,
    /// Only [`crate::suite::PORTABLE_TESTS`] (a Cargo test by its name
    /// before `@TOOLCHAIN`).
    Portable,
}

/// A parsed descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub name: String,
    pub read_only: Vec<PathBuf>,
    pub toolchains: Toolchains,
    pub tests: Tests,
}

impl Image {
    /// Reads and parses [`DESCRIPTOR`].
    pub fn load() -> Result<Self, String> {
        let text = std::fs::read_to_string(DESCRIPTOR).map_err(|e| {
            format!(
                "read {DESCRIPTOR} (the test image's descriptor, flake.nix; every e2e image \
                 carries one): {e}"
            )
        })?;
        Self::parse(&text).map_err(|e| format!("{DESCRIPTOR}: {e}"))
    }

    /// Parses descriptor text (see the module docs).
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut name = None;
        let mut read_only = Vec::new();
        let mut toolchains = None;
        let mut tests = None;
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once(' ')
                .map(|(k, v)| (k, v.trim()))
                .ok_or_else(|| format!("expected `KEY VALUE`, got {line:?}"))?;
            match key {
                "name" => {
                    set_once(&mut name, key, value.to_owned())?;
                }
                "read-only" => {
                    if !value.starts_with('/') {
                        return Err(format!("read-only {value:?} is not an absolute path"));
                    }
                    read_only.push(PathBuf::from(value));
                }
                "toolchains" => {
                    let parsed = match value {
                        "manifest" => Toolchains::Manifest,
                        "none" => Toolchains::None,
                        other => {
                            return Err(format!(
                                "toolchains must be manifest or none, got {other:?}"
                            ));
                        }
                    };
                    set_once(&mut toolchains, key, parsed)?;
                }
                "tests" => {
                    let parsed = match value {
                        "all" => Tests::All,
                        "portable" => Tests::Portable,
                        other => {
                            return Err(format!("tests must be all or portable, got {other:?}"));
                        }
                    };
                    set_once(&mut tests, key, parsed)?;
                }
                other => return Err(format!("unknown key {other:?}")),
            }
        }
        Ok(Self {
            name: name.ok_or("no name")?,
            read_only,
            toolchains: toolchains.ok_or("no toolchains (manifest or none)")?,
            tests: tests.ok_or("no tests (all or portable)")?,
        })
    }

    /// Whether the image runs the test listed as `listed` (`NAME` or
    /// `NAME@TOOLCHAIN`).
    pub fn runs(&self, listed: &str, portable: &[&str]) -> bool {
        match self.tests {
            Tests::All => true,
            Tests::Portable => {
                let base = listed.split('@').next().unwrap_or(listed);
                portable.contains(&base)
            }
        }
    }
}

/// Fills `slot` with `value`, refusing a key given twice.
fn set_once<T>(slot: &mut Option<T>, key: &str, value: T) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("{key} given twice"));
    }
    *slot = Some(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nix_and_debian_descriptors_parse() {
        let nix = Image::parse(
            "# the Nix base image\nname nix\nread-only /nix/store\ntoolchains manifest\ntests all\n",
        )
        .unwrap();
        assert_eq!(nix.read_only, [PathBuf::from("/nix/store")]);
        assert_eq!(nix.toolchains, Toolchains::Manifest);
        assert!(nix.runs("anything@1.99.1", &[]));
        let debian =
            Image::parse("name debian\nread-only /usr\ntoolchains manifest\ntests portable\n")
                .unwrap();
        assert_eq!(debian.tests, Tests::Portable);
        assert!(debian.runs("journey@debian-1.99.1", &["journey"]));
        assert!(debian.runs("journey", &["journey"]));
        assert!(!debian.runs("cargo_race_4@debian-1.99.1", &["journey"]));
        let bare = Image::parse("name bare\ntoolchains none\ntests portable\n").unwrap();
        assert!(bare.read_only.is_empty());
        assert_eq!(bare.toolchains, Toolchains::None);
    }

    #[test]
    fn a_descriptor_is_strict() {
        for (text, why) in [
            ("", "no name"),
            ("name x\ntests all\n", "no toolchains"),
            ("name x\ntoolchains none\n", "no tests"),
            (
                "name x\nname y\ntoolchains none\ntests all\n",
                "given twice",
            ),
            ("name x\ntoolchains maybe\ntests all\n", "manifest or none"),
            ("name x\ntoolchains none\ntests some\n", "all or portable"),
            (
                "name x\nread-only usr\ntoolchains none\ntests all\n",
                "absolute",
            ),
            (
                "name x\nnix yes\ntoolchains none\ntests all\n",
                "unknown key",
            ),
            ("name\n", "KEY VALUE"),
        ] {
            let err = Image::parse(text).unwrap_err();
            assert!(err.contains(why), "{text:?}: {err}");
        }
    }
}
