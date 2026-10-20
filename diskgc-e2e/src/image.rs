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
//! - which tests it runs (`tests all`, `tests portable`, or `tests docker`:
//!   the last runs only [`crate::suite::DOCKER_ONLY_TESTS`]).
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
                        other => {
                            return Err(format!(
                                "tests must be all, portable or docker, got {other:?}"
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
        let tests = tests.ok_or("no tests (all, portable or docker)")?;
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
    /// tests are kept off images without a Docker Engine daemon.
    pub fn runs(
        &self,
        listed: &str,
        nix_only: &[(&str, &str)],
        docker_only: &[(&str, &str)],
    ) -> bool {
        let base = listed.split('@').next().unwrap_or(listed);
        match self.tests {
            Tests::All => !docker_only.iter().any(|(name, _)| *name == base),
            Tests::Portable => {
                !docker_only.iter().any(|(name, _)| *name == base)
                    && !nix_only.iter().any(|(name, _)| *name == base)
            }
            Tests::Docker => docker_only.iter().any(|(name, _)| *name == base),
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
            "# the Nix base image\nname nix\nread-only /nix/store\ntoolchains manifest\ntests all\ntmpfiles /opt/diskgc-e2e/bin/systemd-tmpfiles\n",
        )
        .unwrap();
        assert_eq!(nix.read_only, [PathBuf::from("/nix/store")]);
        assert_eq!(nix.toolchains, Toolchains::Manifest);
        assert_eq!(
            nix.tmpfiles,
            PathBuf::from("/opt/diskgc-e2e/bin/systemd-tmpfiles")
        );
        let nix_only = [("cargo_race_4", "needs the nightly")];
        let docker_only = [("native_docker_engine_dind_smoke", "needs dockerd")];
        assert!(nix.runs("cargo_race_4@1.99.1", &nix_only, &docker_only));
        assert!(!nix.runs("native_docker_engine_dind_smoke", &nix_only, &docker_only));
        let debian =
            Image::parse("name debian\nread-only /usr\ntoolchains manifest\ntests portable\ntmpfiles /usr/bin/systemd-tmpfiles\n")
                .unwrap();
        assert_eq!(debian.tests, Tests::Portable);
        assert!(debian.runs("journey@debian-1.99.1", &nix_only, &docker_only));
        assert!(debian.runs("sandbox_trivial", &nix_only, &docker_only));
        assert!(!debian.runs("cargo_race_4@debian-1.99.1", &nix_only, &docker_only));
        assert!(!debian.runs("cargo_race_4", &nix_only, &docker_only));
        let docker = Image::parse(
            "name docker\ntoolchains none\ntests docker\ntmpfiles /usr/bin/sd-tmpfiles\n",
        )
        .unwrap();
        assert_eq!(docker.tmpfiles, PathBuf::from("/usr/bin/sd-tmpfiles"));
        assert!(docker.runs("native_docker_engine_dind_smoke", &nix_only, &docker_only));
        assert!(!docker.runs("sandbox_trivial", &nix_only, &docker_only));
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
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A missing or unreadable descriptor is an error: no image is assumed.
    #[test]
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
                "all, portable or docker",
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
