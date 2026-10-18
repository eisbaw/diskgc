//! The configuration file (docs/reference.md, Configuration file): where it
//! is looked for, and the part of schema `version: 1` this build loads.
//!
//! Lookup ([`lookup`]) is a pure function of the command line and the
//! environment; reading a file is the binary's (through `LinuxHost`), and
//! parsing it ([`parse`]) is pure again, so every rule here is tested
//! without a machine.
//!
//! **What this build loads.** `version` (mandatory, `1`) and the user
//! `profiles` (task-8's loader: the static profile rules, all problems at
//! once). The rest of the schema arrives with task-15; until then:
//!
//! - `protect` with entries and `owners` are refused (exit 2): ignoring
//!   either would change what is deleted (a protected match, or whose
//!   files), so they are never silently dropped;
//! - `groups` is accepted unread: `--select` replaces it
//!   (docs/reference.md, Commands), and `ensure` without `--select` is
//!   refused, so no tier of the file runs. It is **not validated yet**: an
//!   invalid `groups` passes with `--select` (task-15 adds the full static
//!   validation, and `check`).
//!
//! Unknown top-level keys are errors, as everywhere in the schema.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::IgnoredAny;

use crate::profile::{Library, ProfileSpec};

/// The largest configuration file read (bytes): far above any real policy,
/// small enough that a wrong path (a log, an image) is refused rather than
/// parsed.
pub const MAX_CONFIG_BYTES: usize = 1 << 20;

/// The system-wide configuration, tried last.
pub const SYSTEM_CONFIG: &str = "/etc/diskgc/config.yaml";

/// Where the configuration comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// `--config FILE` or `$DISKGC_CONFIG`: that file and only that file;
    /// missing or unreadable is exit 2, never a fallback, so a typo cannot
    /// activate some other policy.
    Given { path: PathBuf, from: &'static str },
    /// Neither given: these files in order, the first that exists wins.
    Search(Vec<PathBuf>),
}

/// The environment the lookup reads (passed in, so the lookup is pure).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    pub diskgc_config: Option<OsString>,
    pub xdg_config_home: Option<OsString>,
    pub home: Option<OsString>,
}

/// Where to look for the configuration (docs/reference.md, Configuration
/// file: Lookup). `flag` is `--config`'s value.
///
/// - An empty `--config` or `$DISKGC_CONFIG` is an error, not "unset": an
///   empty value is what `DISKGC_CONFIG=$TYPO` gives, and falling back to
///   another file is exactly what the rule forbids.
/// - A relative path is an error (the paths rule: absolute only).
/// - `$XDG_CONFIG_HOME` counts only when absolute (XDG Base Directory
///   specification: a relative value is ignored), else `$HOME/.config`;
///   without either (or with a relative `$HOME`), the user file is not
///   looked for.
pub fn lookup(flag: Option<&OsStr>, env: &Env) -> Result<Lookup, String> {
    let given = match (flag, env.diskgc_config.as_deref()) {
        (Some(path), _) => Some((path, "--config")),
        (None, Some(path)) => Some((path, "$DISKGC_CONFIG")),
        (None, None) => None,
    };
    if let Some((path, from)) = given {
        if path.is_empty() {
            return Err(format!(
                "{from} is empty: name the configuration file (an empty value never falls \
                 back to another file)"
            ));
        }
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(format!(
                "{from} {}: the configuration file must be an absolute path",
                path.display()
            ));
        }
        return Ok(Lookup::Given { path, from });
    }
    let absolute = |v: &Option<OsString>| v.as_ref().map(PathBuf::from).filter(|p| p.is_absolute());
    let user = absolute(&env.xdg_config_home)
        .or_else(|| absolute(&env.home).map(|home| home.join(".config")))
        .map(|dir| dir.join("diskgc/config.yaml"));
    Ok(Lookup::Search(
        user.into_iter()
            .chain(std::iter::once(PathBuf::from(SYSTEM_CONFIG)))
            .collect(),
    ))
}

impl Lookup {
    /// The paths to try, in order.
    pub fn paths(&self) -> Vec<&Path> {
        match self {
            Self::Given { path, .. } => vec![path],
            Self::Search(paths) => paths.iter().map(PathBuf::as_path).collect(),
        }
    }
}

/// A configuration file as this build loads it.
#[derive(Debug, Clone)]
pub struct Config {
    /// The file it was read from.
    pub path: PathBuf,
    /// The built-in profiles plus the file's `profiles:`.
    pub library: Library,
    /// Whether the file has `groups:` (unread, see the module docs).
    pub has_groups: bool,
}

/// The top level of schema `version: 1`, as far as this build reads it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigSpec {
    version: u64,
    #[serde(default)]
    profiles: Vec<ProfileSpec>,
    #[serde(default)]
    protect: Option<Vec<IgnoredAny>>,
    #[serde(default)]
    owners: Option<Vec<IgnoredAny>>,
    #[serde(default)]
    groups: Option<IgnoredAny>,
}

/// Why a configuration file was refused: a load-time error, exit 2,
/// nothing deleted. The message names the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid config {}: {}",
            self.path.display(),
            self.message
        )
    }
}

impl std::error::Error for ConfigError {}

/// Parses the configuration file `path` holding `bytes` (see the module
/// docs for what is loaded and what is refused).
pub fn parse(path: &Path, bytes: &[u8]) -> Result<Config, ConfigError> {
    let error = |message: String| ConfigError {
        path: path.to_owned(),
        message,
    };
    let text = std::str::from_utf8(bytes).map_err(|e| error(format!("not UTF-8: {e}")))?;
    let spec: ConfigSpec = serde_saphyr::from_str(text).map_err(|e| error(e.to_string()))?;
    if spec.version != 1 {
        return Err(error(format!(
            "version: {} is not supported; this build reads version: 1",
            spec.version
        )));
    }
    if spec.protect.as_ref().is_some_and(|p| !p.is_empty()) {
        return Err(error(
            "protect: is not supported by this build yet (task-15, task-16); it is refused \
             rather than ignored, since ignoring it would delete what it protects"
                .to_owned(),
        ));
    }
    if spec.owners.is_some() {
        return Err(error(
            "owners: is allowed only with --allow-root (docs/reference.md, Configuration \
             file), which this build does not support yet (task-21)"
                .to_owned(),
        ));
    }
    let library = Library::with_user_profiles(&spec.profiles).map_err(|e| error(e.to_string()))?;
    Ok(Config {
        path: path.to_owned(),
        library,
        has_groups: spec.groups.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    fn env(config: Option<&str>, xdg: Option<&str>, home: Option<&str>) -> Env {
        Env {
            diskgc_config: config.map(os),
            xdg_config_home: xdg.map(os),
            home: home.map(os),
        }
    }

    #[test]
    fn a_given_file_is_the_only_one() {
        let e = env(Some("/srv/env.yaml"), Some("/x"), Some("/home/m"));
        assert_eq!(
            lookup(Some(OsStr::new("/srv/flag.yaml")), &e),
            Ok(Lookup::Given {
                path: "/srv/flag.yaml".into(),
                from: "--config"
            })
        );
        assert_eq!(
            lookup(None, &e),
            Ok(Lookup::Given {
                path: "/srv/env.yaml".into(),
                from: "$DISKGC_CONFIG"
            })
        );
    }

    #[test]
    fn an_empty_or_relative_given_file_is_an_error_not_a_fallback() {
        let e = env(Some(""), None, Some("/home/m"));
        assert!(
            lookup(None, &e)
                .unwrap_err()
                .contains("$DISKGC_CONFIG is empty")
        );
        assert!(
            lookup(Some(OsStr::new("")), &Env::default())
                .unwrap_err()
                .contains("--config is empty")
        );
        assert!(
            lookup(Some(OsStr::new("config.yaml")), &Env::default())
                .unwrap_err()
                .contains("absolute")
        );
    }

    #[test]
    fn the_search_is_xdg_or_home_then_etc() {
        let search = |e: &Env| lookup(None, e).expect("lookup");
        assert_eq!(
            search(&env(None, Some("/cfg"), Some("/home/m"))),
            Lookup::Search(vec![
                "/cfg/diskgc/config.yaml".into(),
                "/etc/diskgc/config.yaml".into()
            ])
        );
        // A relative XDG_CONFIG_HOME is ignored (XDG Base Directory spec).
        assert_eq!(
            search(&env(None, Some("cfg"), Some("/home/m"))),
            Lookup::Search(vec![
                "/home/m/.config/diskgc/config.yaml".into(),
                "/etc/diskgc/config.yaml".into()
            ])
        );
        assert_eq!(
            search(&env(None, None, None)),
            Lookup::Search(vec!["/etc/diskgc/config.yaml".into()])
        );
    }

    fn parsed(text: &str) -> Result<Config, String> {
        parse(Path::new("/c.yaml"), text.as_bytes()).map_err(|e| e.to_string())
    }

    #[test]
    fn version_1_with_profiles_and_groups_loads() {
        let config = parsed(
            "version: 1\nprofiles:\n  - name: my-target\n    kind: cargo-target\n    find: \
             {roots: required}\ngroups:\n  - name: g\n    tiers: []\nprotect: []\n",
        )
        .expect("loads");
        assert!(config.has_groups);
        assert!(config.library.get("my-target").is_some());
        assert!(config.library.get("cargo-target").is_some());
        assert!(!parsed("version: 1\n").expect("loads").has_groups);
    }

    #[test]
    fn what_this_build_cannot_honour_is_refused_not_ignored() {
        for (text, needle) in [
            ("profiles: []\n", "version"),
            ("version: 2\n", "version: 2 is not supported"),
            (
                "version: 1\nprotect: [claude-code-tmp]\n",
                "protect: is not supported",
            ),
            (
                "version: 1\nowners: [mark]\n",
                "owners: is allowed only with --allow-root",
            ),
            (
                "version: 1\nowners: []\n",
                "owners: is allowed only with --allow-root",
            ),
            ("version: 1\ntiers: []\n", "tiers"),
            (
                "version: 1\nprofiles:\n  - name: cargo-target\n    kind: cargo-target\n    \
                 find: {roots: required}\n",
                "cargo-target",
            ),
        ] {
            let error = parsed(text).expect_err(text);
            assert!(error.contains(needle), "{text:?}: {error}");
            assert!(error.starts_with("invalid config /c.yaml: "), "{error}");
        }
        assert!(
            parse(Path::new("/c.yaml"), b"version: 1\n\xff")
                .unwrap_err()
                .message
                .contains("UTF-8")
        );
    }
}
