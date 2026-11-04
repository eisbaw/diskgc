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
//! - which tests it runs (`tests all`, `tests portable`, `tests docker` or
//!   `tests nix-gc`: the last two run only [`crate::suite::DOCKER_ONLY_TESTS`]
//!   or [`crate::suite::NIX_GC_ONLY_TESTS`]).
//! - where this image runs a real tmpfiles implementation (`tmpfiles PATH`).
//!
//! The format is one `KEY VALUE` per line, `#` comments and blank lines
//! ignored; every key but `read-only` exactly once, unknown keys and values
//! refused. A missing or unreadable descriptor fails the listing and every
//! test's setup: nothing falls back to "a Nix image".

use std::path::{Path, PathBuf};

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
    /// Every test but [`crate::suite::NIX_ONLY_TESTS`] (a Cargo test by
    /// its name before `@TOOLCHAIN`).
    Portable,
    /// Only tests that need a native Docker Engine daemon.
    Docker,
    /// Only tests that need the Nix GC image's own store and daemon.
    NixGc,
}

/// A parsed descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub name: String,
    pub read_only: Vec<PathBuf>,
    pub toolchains: Toolchains,
    pub tests: Tests,
    pub tmpfiles: PathBuf,
}

impl Image {
    /// Reads and parses [`DESCRIPTOR`].
    pub fn load() -> Result<Self, String> {
        Self::load_from(Path::new(DESCRIPTOR))
    }

    /// Reads and parses the descriptor at `path`; a file that is missing or
    /// cannot be read is an error, never a default image.
    pub fn load_from(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "read {} (the test image's descriptor, flake.nix; every e2e image carries \
                 one): {e}",
                path.display()
            )
        })?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Parses descriptor text (see the module docs).
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut name = None;
        let mut read_only = Vec::new();
        let mut toolchains = None;
        let mut tests = None;
        let mut tmpfiles = None;
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
                        "docker" => Tests::Docker,
                        "nix-gc" => Tests::NixGc,
                        other => {
                            return Err(format!(
                                "tests must be all, portable, docker or nix-gc, got {other:?}"
                            ));
                        }
                    };
                    set_once(&mut tests, key, parsed)?;
                }
                "tmpfiles" => {
                    if !value.starts_with('/') {
                        return Err(format!("tmpfiles {value:?} is not an absolute path"));
                    }
                    let tool = PathBuf::from(value);
                    set_once(&mut tmpfiles, key, tool)?;
                }
                other => return Err(format!("unknown key {other:?}")),
            }
        }
        let name = name.ok_or("no name")?;
        let toolchains = toolchains.ok_or("no toolchains (manifest or none)")?;
        let tests = tests.ok_or("no tests (all, portable, docker or nix-gc)")?;
        let tmpfiles = tmpfiles.ok_or("no tmpfiles (absolute path)")?;
        Ok(Self {
            name,
            read_only,
            toolchains,
            tests,
            tmpfiles,
        })
    }

    /// Whether the image runs `listed` (`NAME` or `NAME@TOOLCHAIN`). Docker
    /// and Nix GC tests are kept off images without their daemon.
    pub fn runs(&self, listed: &str, only: &Only<'_>) -> bool {
        let base = listed.split('@').next().unwrap_or(listed);
        let has = |list: &[(&str, &str)]| list.iter().any(|(name, _)| *name == base);
        let special = has(only.docker) || has(only.nix_gc);
        match self.tests {
            Tests::All => !special,
            Tests::Portable => !special && !has(only.nix),
            Tests::Docker => has(only.docker),
            Tests::NixGc => has(only.nix_gc),
        }
    }
}

/// The tests only some images run, each list with why
/// ([`crate::suite::NIX_ONLY_TESTS`] and its siblings).
#[derive(Debug, Clone, Copy)]
pub struct Only<'a> {
    pub nix: &'a [(&'a str, &'a str)],
    pub docker: &'a [(&'a str, &'a str)],
    pub nix_gc: &'a [(&'a str, &'a str)],
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
            "# the Nix base image\nname nix\nread-only /nix/store\ntoolchains manifest\ntests all\ntmpfiles /opt/diskgc-e2e/bin/systemd-tmpfiles\n",
        )
        .unwrap();
        assert_eq!(nix.read_only, [PathBuf::from("/nix/store")]);
        assert_eq!(nix.toolchains, Toolchains::Manifest);
        assert_eq!(
            nix.tmpfiles,
            PathBuf::from("/opt/diskgc-e2e/bin/systemd-tmpfiles")
        );
        let only = Only {
            nix: &[("cargo_race_4", "needs the nightly")],
            docker: &[("native_docker_engine_dind_smoke", "needs dockerd")],
            nix_gc: &[("nix_gc_keeps_every_kind_of_root", "needs its own store")],
        };
        assert!(nix.runs("cargo_race_4@1.99.1", &only));
        assert!(!nix.runs("native_docker_engine_dind_smoke", &only));
        assert!(!nix.runs("nix_gc_keeps_every_kind_of_root", &only));
        let debian =
            Image::parse("name debian\nread-only /usr\ntoolchains manifest\ntests portable\ntmpfiles /usr/bin/systemd-tmpfiles\n")
                .unwrap();
        assert_eq!(debian.tests, Tests::Portable);
        assert!(debian.runs("journey@debian-1.99.1", &only));
        assert!(debian.runs("sandbox_trivial", &only));
        assert!(!debian.runs("cargo_race_4@debian-1.99.1", &only));
        assert!(!debian.runs("cargo_race_4", &only));
        assert!(!debian.runs("nix_gc_keeps_every_kind_of_root", &only));
        let docker = Image::parse(
            "name docker\ntoolchains none\ntests docker\ntmpfiles /usr/bin/sd-tmpfiles\n",
        )
        .unwrap();
        assert_eq!(docker.tmpfiles, PathBuf::from("/usr/bin/sd-tmpfiles"));
        assert!(docker.runs("native_docker_engine_dind_smoke", &only));
        assert!(!docker.runs("sandbox_trivial", &only));
        assert!(!docker.runs("nix_gc_keeps_every_kind_of_root", &only));
        let nix_gc = Image::parse(
            "name nix-gc
toolchains none
tests nix-gc
tmpfiles /opt/diskgc-e2e/bin/systemd-tmpfiles
",
        )
        .unwrap();
        assert_eq!(nix_gc.tests, Tests::NixGc);
        assert!(nix_gc.runs("nix_gc_keeps_every_kind_of_root", &only));
        assert!(!nix_gc.runs("sandbox_trivial", &only));
        assert!(!nix_gc.runs("native_docker_engine_dind_smoke", &only));
        let bare = Image::parse(
            "name bare\ntoolchains none\ntests portable\ntmpfiles /usr/bin/systemd-tmpfiles\n",
        )
        .unwrap();
        assert!(bare.read_only.is_empty());
        assert_eq!(bare.toolchains, Toolchains::None);
    }

    /// A temporary directory, removed when dropped (also when a test
    /// panics).
    struct TempDir(PathBuf);

    impl Drop for TempDir {
        #[expect(
            clippy::disallowed_methods,
            reason = "a unit test's own temporary directory, in the Nix build sandbox"
        )]
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A missing or unreadable descriptor is an error: no image is assumed.
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "a unit test's own temporary directory, in the Nix build sandbox"
    )]
    fn a_missing_or_unreadable_descriptor_is_an_error() {
        let guard =
            TempDir(std::env::temp_dir().join(format!("diskgc-e2e-image-{}", std::process::id())));
        let dir = &guard.0;
        std::fs::create_dir_all(dir).unwrap();
        let missing = Image::load_from(&dir.join("no-such-descriptor")).unwrap_err();
        assert!(missing.contains("descriptor"), "{missing}");
        // A directory cannot be read as a file.
        let unreadable = Image::load_from(dir).unwrap_err();
        assert!(unreadable.contains("descriptor"), "{unreadable}");
        let file = dir.join("descriptor");
        std::fs::write(
            &file,
            "name x\ntoolchains none\ntests all\ntmpfiles /usr/bin/systemd-tmpfiles\n",
        )
        .unwrap();
        assert_eq!(Image::load_from(&file).unwrap().name, "x");
    }

    #[test]
    fn a_descriptor_is_strict() {
        for (text, why) in [
            ("", "no name"),
            ("name x\ntests all\n", "no toolchains"),
            ("name x\ntoolchains none\n", "no tests"),
            ("name x\ntoolchains none\ntests all\n", "no tmpfiles"),
            (
                "name x\nname y\ntoolchains none\ntests all\n",
                "given twice",
            ),
            ("name x\ntoolchains maybe\ntests all\n", "manifest or none"),
            (
                "name x\ntoolchains none\ntests some\n",
                "all, portable, docker or nix-gc",
            ),
            (
                "name x\nread-only usr\ntoolchains none\ntests all\n",
                "absolute",
            ),
            (
                "name x\ntoolchains none\ntests all\ntmpfiles relative\n",
                "absolute path",
            ),
            (
                "name x\ntoolchains none\ntests portable\ntmpfiles none\n",
                "absolute path",
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
