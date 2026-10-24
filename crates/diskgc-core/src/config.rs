//! The configuration file (docs/reference.md, Configuration file): where it
//! is looked for, and schema `version: 1`.
//!
//! Lookup ([`lookup`]) is a pure function of the command line and the
//! environment; reading a file is the binary's (through `LinuxHost`), and
//! parsing it ([`parse`]) is pure again, so every rule here is tested
//! without a machine.
//!
//! Parsing checks every group, tier, protection and profile before any
//! executor sees the policy. The checked data is retained for the planner;
//! no machine paths are opened during static validation.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde::de::Deserializer;

use crate::engine::Checkouts;
use crate::kind::Kind;
use crate::profile::{Body, CheckoutsSpec, DurationSpec, Finds, Library, ProfileSpec, Roots};
use crate::select::{self, Checked};

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

/// A statically checked configuration file.
#[derive(Debug, Clone)]
pub struct Config {
    /// The file it was read from.
    pub path: PathBuf,
    /// The built-in profiles plus the file's `profiles:`.
    pub library: Library,
    pub groups: Vec<Group>,
    pub protect: Vec<Protection>,
    /// User names or explicit non-system UIDs. Names are resolved at run time.
    pub owners: Option<Vec<Owner>>,
    pub warnings: Vec<String>,
    pub ipc: IpcPolicy,
    /// Narrow operator policies checked against the active release catalog.
    pub profile_policies: Vec<CheckoutPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutPolicy {
    pub profile: String,
    pub checkouts: CheckoutsSpec,
}

#[derive(Debug, Clone)]
pub struct Group {
    pub name: String,
    pub ranking: Ranking,
    pub tiers: Vec<Tier>,
}

#[derive(Debug, Clone)]
pub struct Tier {
    pub name: String,
    pub select: Checked,
    pub roots: std::collections::BTreeMap<String, Vec<String>>,
    pub order: Order,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct Protection {
    pub profile: String,
    pub roots: Vec<String>,
}

/// A root-mode owner. The explicit mapping avoids treating a numeric-looking
/// user name as a UID; static musl builds can use UIDs for NSS-only accounts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(untagged)]
pub enum Owner {
    Name(String),
    Uid(UidOwner),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UidOwner {
    pub uid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ranking {
    Ordered,
    Random,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Order {
    Oldest,
    BestFit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IpcPolicy {
    BlockAny,
    LiveOnly,
}

/// The top level of schema `version: 1`. Unknown and repeated mapping keys
/// are errors, including inside a group, tier and protection.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigSpec {
    version: u64,
    #[serde(default)]
    profiles: Vec<ProfileSpec>,
    #[serde(default)]
    profile_policies: Vec<CheckoutPolicy>,
    #[serde(default)]
    protect: Vec<ProtectSpec>,
    #[serde(default, deserialize_with = "present")]
    owners: Option<Vec<Owner>>,
    #[serde(default)]
    groups: Vec<GroupSpec>,
    #[serde(default, deserialize_with = "present")]
    ipc: Option<IpcPolicy>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupSpec {
    name: String,
    #[serde(default, deserialize_with = "present")]
    ranking: Option<Ranking>,
    tiers: Vec<TierSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TierSpec {
    name: String,
    select: String,
    #[serde(default, deserialize_with = "present")]
    roots: Option<Vec<String>>,
    #[serde(default, deserialize_with = "present")]
    order: Option<Order>,
    #[serde(default, deserialize_with = "present")]
    timeout: Option<DurationSpec>,
}

/// A present optional key must carry a value: YAML null must not silently
/// become the default policy. Only an absent key receives the default.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ProtectSpec {
    Bare(String),
    WithRoots(ProtectWithRoots),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtectWithRoots {
    profile: String,
    roots: Vec<String>,
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

/// The first non-system uid: `owners:` entries, by number or by a name's
/// account, must be at least this (docs/reference.md, Configuration file).
pub const SYSTEM_UID_END: u32 = 1000;

/// Parses and checks every static rule of the configuration. It does not
/// access the machine beyond the already supplied bytes.
pub fn parse(path: &Path, bytes: &[u8]) -> Result<Config, ConfigError> {
    parse_inner(path, bytes, true)
}

/// Parses a configuration for `profiles test`. That command reports example
/// mismatches as test failures (exit 1), while all other static rules still
/// fail configuration loading (exit 2).
pub fn parse_for_profile_test(path: &Path, bytes: &[u8]) -> Result<Config, ConfigError> {
    parse_inner(path, bytes, false)
}

fn parse_inner(path: &Path, bytes: &[u8], check_examples: bool) -> Result<Config, ConfigError> {
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
    let mut library =
        Library::with_user_profiles(&spec.profiles).map_err(|e| error(e.to_string()))?;
    let mut policy_names = BTreeSet::new();
    for policy in &spec.profile_policies {
        if !policy_names.insert(policy.profile.as_str()) {
            return Err(error(format!(
                "profile_policies: duplicate policy for `{}`",
                policy.profile
            )));
        }
        if spec
            .profiles
            .iter()
            .any(|profile| profile.name == policy.profile)
        {
            return Err(error(format!(
                "profile_policies: `{}` is also defined in profiles; specify checkouts in one place",
                policy.profile
            )));
        }
        library
            .set_checkouts(&policy.profile, policy.checkouts.0)
            .map_err(|message| error(format!("profile_policies: {message}")))?;
    }
    if check_examples && let Some(first) = crate::profile::examples::failures(&library).first() {
        return Err(error(first.clone()));
    }
    let mut tier_names = BTreeSet::new();
    let mut group_names = BTreeSet::new();
    let mut groups = Vec::new();
    let mut selection_warnings = Vec::new();
    for group in spec.groups {
        valid_name(&group.name, "group").map_err(error)?;
        if !group_names.insert(group.name.clone()) {
            return Err(error(format!("duplicate group name `{}`", group.name)));
        }
        let mut tiers = Vec::new();
        for tier in group.tiers {
            valid_name(&tier.name, "tier").map_err(error)?;
            if !tier_names.insert(tier.name.clone()) {
                return Err(error(format!("duplicate tier name `{}`", tier.name)));
            }
            let checked = select::compile(&tier.select, &library)
                .map_err(|e| error(format!("tier `{}`: {e}", tier.name)))?;
            selection_warnings.extend(
                checked
                    .warnings()
                    .iter()
                    .map(|warning| format!("tier `{}`: {warning}", tier.name)),
            );
            let roots = library
                .bind_roots(&checked, tier.roots.as_deref())
                .map_err(|e| error(format!("tier `{}`: {e}", tier.name)))?;
            tiers.push(Tier {
                name: tier.name,
                select: checked,
                roots,
                order: tier.order.unwrap_or(Order::Oldest),
                timeout: tier.timeout.map_or(Duration::from_secs(600), |t| t.0),
            });
        }
        groups.push(Group {
            name: group.name,
            ranking: group.ranking.unwrap_or(Ranking::Ordered),
            tiers,
        });
    }
    let mut protect = Vec::new();
    for item in spec.protect {
        let (name, supplied) = match item {
            ProtectSpec::Bare(name) => (name, None),
            ProtectSpec::WithRoots(item) => (item.profile, Some(item.roots)),
        };
        let profile = library
            .get(&name)
            .ok_or_else(|| error(format!("protect: unknown profile `{name}`")))?;
        let Body::Source(source) = &profile.body else {
            return Err(error(format!(
                "protect: `{name}` is a filter, not a source profile"
            )));
        };
        if !matches!(source.kind(), Kind::Fs | Kind::CargoTarget) {
            return Err(error(format!(
                "protect: `{name}` has kind {}, which cannot be protected",
                source.kind()
            )));
        }
        let roots = library
            .roots_of(&name, supplied.as_deref())
            .map_err(|e| error(format!("protect `{name}`: {e}")))?;
        protect.push(Protection {
            profile: name,
            roots,
        });
    }
    if let Some(owners) = &spec.owners {
        if owners.is_empty() {
            return Err(error("owners: list must be non-empty".to_owned()));
        }
        for owner in owners {
            match owner {
                Owner::Name(name) if name.trim().is_empty() => {
                    return Err(error("owners: names must be non-empty".to_owned()));
                }
                Owner::Name(name) if name.bytes().all(|byte| byte.is_ascii_digit()) => {
                    return Err(error(format!(
                        "owners: numeric user name {name:?} is ambiguous; use {{uid: {name}}}"
                    )));
                }
                Owner::Uid(UidOwner { uid }) if *uid < SYSTEM_UID_END => {
                    return Err(error(format!(
                        "owners: system uid {uid} (< {SYSTEM_UID_END}) is forbidden"
                    )));
                }
                _ => {}
            }
        }
        let unique: BTreeSet<_> = owners.iter().collect();
        if unique.len() != owners.len() {
            return Err(error("owners: entries must be unique".to_owned()));
        }
    }
    let mut warnings = profile_warnings(&library, &groups, &protect);
    warnings.extend(selection_warnings);
    warnings.extend(
        spec.profile_policies
            .iter()
            .filter(|policy| policy.checkouts.0 == Checkouts::Allow)
            .map(|policy| {
            format!(
                "profile `{}` allows VCS checkouts by profile_policies: candidates may contain uncommitted work",
                policy.profile
            )
            }),
    );
    Ok(Config {
        path: path.to_owned(),
        library,
        groups,
        protect,
        owners: spec.owners,
        warnings,
        ipc: spec.ipc.unwrap_or(IpcPolicy::BlockAny),
        profile_policies: spec.profile_policies,
    })
}

impl Config {
    /// Invocation-specific checks are static too, but `check` has no
    /// `--allow-root` mode and therefore validates the policy itself only.
    pub fn validate_mode(&self, allow_root: bool) -> Result<(), String> {
        match (allow_root, &self.owners) {
            (true, None) => return Err("--allow-root requires an owners: list".to_owned()),
            (false, Some(_)) => return Err("owners: is allowed only with --allow-root".to_owned()),
            _ => {}
        }
        // The built-in library is always loaded. Refuse a command only when
        // a configured tier selects it; an unused built-in must not make an
        // otherwise valid root policy fail static validation.
        if allow_root {
            for tier in self.groups.iter().flat_map(|group| &group.tiers) {
                for term in &tier.select.expr().terms {
                    if let Some(profile) = self.library.get(&term.source)
                        && let Body::Source(source) = &profile.body
                        && let Some(error) = source.action.root_mode_error()
                    {
                        return Err(error.to_owned());
                    }
                }
            }
        }
        Ok(())
    }
}

fn valid_name(name: &str, what: &str) -> Result<(), String> {
    if !select::is_name(name) {
        Err(format!(
            "{what} name `{name}` must be a NAME (lowercase letters, digits, `-`, `_`)"
        ))
    } else {
        Ok(())
    }
}

/// Warnings are advisory: valid policies still pass `check`. They do not
/// affect the meaning of a profile or depend on the deployment machine.
fn profile_warnings(library: &Library, groups: &[Group], protect: &[Protection]) -> Vec<String> {
    let mut warnings = Vec::new();
    for profile in library.profiles().filter(|p| {
        matches!(
            p.origin,
            crate::profile::Origin::User | crate::profile::Origin::UserOverride
        )
    }) {
        let Body::Source(source) = &profile.body else {
            continue;
        };
        if source.checkouts == Checkouts::Allow {
            warnings.push(format!(
                "profile `{}` allows VCS checkouts: candidates may contain uncommitted work",
                profile.name
            ));
        }
        let Finds::Paths { patterns, .. } = &source.finds else {
            continue;
        };
        let broad = patterns
            .iter()
            .any(|pattern| pattern.rsplit('/').next().is_some_and(|last| last == "*"));
        let outside_temp = |root: &str| {
            !Path::new(root).starts_with("/tmp") && !Path::new(root).starts_with("/var/tmp")
        };
        let roots_outside = match &source.roots {
            Roots::Fixed(roots) => roots.iter().any(|root| outside_temp(root)),
            Roots::Required => {
                groups
                    .iter()
                    .flat_map(|g| &g.tiers)
                    .filter_map(|tier| tier.roots.get(&profile.name))
                    .flatten()
                    .any(|root| outside_temp(root))
                    || protect
                        .iter()
                        .filter(|p| p.profile == profile.name)
                        .flat_map(|p| &p.roots)
                        .any(|root| outside_temp(root))
            }
            Roots::Backend => false,
        };
        if broad && roots_outside {
            warnings.push(format!("profile `{}` ends a path pattern with only wildcards outside /tmp or /var/tmp; verify its matches", profile.name));
        }
    }
    warnings
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
        assert_eq!(config.groups.len(), 1);
        assert!(config.library.get("my-target").is_some());
        assert!(config.library.get("cargo-target").is_some());
        assert!(parsed("version: 1\n").expect("loads").groups.is_empty());
    }

    #[test]
    fn malformed_configs_are_refused_not_ignored() {
        for (text, needle) in [
            ("profiles: []\n", "version"),
            ("version: 2\n", "version: 2 is not supported"),
            ("version: 1\nprotect: [not-there]\n", "unknown profile"),
            ("version: 1\nowners: []\n", "owners: list must be non-empty"),
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

    const FS: &str = "profiles:\n  - name: scratch\n    find: {roots: [/scratch], paths: ['x-*'], type: dir}\n    action: delete\n    examples: {match: [/scratch/x-a], no_match: [/scratch/y-a]}\n";

    fn policy(body: &str) -> String {
        format!("version: 1\n{FS}{body}")
    }

    #[test]
    fn groups_are_checked_and_retain_the_policy_order() {
        let config = parsed(&policy("groups:\n  - name: stale\n    ranking: random\n    tiers:\n      - name: old\n        select: profile:scratch && filter:idle-3d\n        order: best-fit\n        timeout: 2m\n      - name: next\n        select: profile:cargo-target\n        roots: [/builds]\n  - name: pressure\n    tiers:\n      - name: recent\n        select: profile:scratch\nprotect:\n  - scratch\n  - {profile: cargo-target, roots: [/builds]}\n"))
            .expect("valid policy");
        assert_eq!(
            config
                .groups
                .iter()
                .map(|g| g.name.as_str())
                .collect::<Vec<_>>(),
            ["stale", "pressure"]
        );
        assert_eq!(config.groups[0].ranking, Ranking::Random);
        assert_eq!(config.groups[1].ranking, Ranking::Ordered);
        assert_eq!(config.groups[0].tiers[0].order, Order::BestFit);
        assert_eq!(config.groups[0].tiers[0].timeout, Duration::from_secs(120));
        assert_eq!(config.groups[0].tiers[1].timeout, Duration::from_secs(600));
        assert_eq!(
            config.groups[0].tiers[0].select.expr().to_string(),
            "profile:scratch && filter:idle-3d"
        );
        assert_eq!(config.groups[0].tiers[1].roots["cargo-target"], ["/builds"]);
        assert_eq!(
            config
                .protect
                .iter()
                .map(|p| p.profile.as_str())
                .collect::<Vec<_>>(),
            ["scratch", "cargo-target"]
        );
    }

    #[test]
    fn every_config_specific_static_rule_has_a_refusal() {
        let valid = policy(
            "groups:\n  - name: g\n    tiers:\n      - name: t\n        select: profile:scratch\n",
        );
        for (text, needle) in [
            ("version: 1\nunknown: x\n".to_owned(), "unknown"),
            ("version: 2\n".to_owned(), "version: 2"),
            ("version: 1\nversion: 1\n".to_owned(), "duplicate"),
            (policy("  - name: scratch\n    find: {roots: [/scratch], paths: ['x-*'], type: dir}\n    action: delete\n    examples: {match: [/scratch/x-a]}\n"), "already used"),
            (policy("  - name: instant\n    where: {idle_for: 0s}\n"), "idle_for"),
            ("version: 1\ngroups:\n  - name: g\n    tiers: []\n  - name: g\n    tiers: []\n".to_owned(), "duplicate group"),
            ("version: 1\ngroups:\n  - name: g\n    tiers: [{name: t, select: 'profile:cargo-target', roots: [/a]}]\n  - name: h\n    tiers: [{name: t, select: 'profile:cargo-target', roots: [/a]}]\n".to_owned(), "duplicate tier"),
            (valid.replace("name: t", "name: bad.name"), "tier name"),
            (valid.replace("select: profile:scratch", "select: scratch"), "invalid selection"),
            (valid.replace("select: profile:scratch", "select: profile:missing"), "no profile"),
            (valid.replace("select: profile:scratch", "select: profile:cargo-target"), "roots: required"),
            (valid.replace("select: profile:scratch", "select: profile:scratch\n        roots: [/a]"), "fixed roots"),
            (valid.replace("select: profile:scratch", "select: profile:scratch\n        timeout: 5"), "bare number"),
            (valid.replace("select: profile:scratch", "select: profile:scratch\n        timeout: 5lightyears"), "5lightyears"),
            (valid.replace("select: profile:scratch", "select: profile:scratch\n        order: newest"), "newest"),
            (valid.replace("select: profile:scratch", "select: profile:scratch\n        typo: x"), "typo"),
            (valid.replace("name: g", "name: g\n    ranking: newest"), "newest"),
            (valid.replace("name: g", "name: g\n    typo: x"), "typo"),
            (policy("protect: [missing]\n"), "unknown profile"),
            (policy("protect: [idle-3d]\n"), "filter"),
            (policy("protect: [cargo-target]\n"), "roots: required"),
            (policy("protect: [{profile: scratch, roots: [/a]}]\n"), "fixed roots"),
            (policy("protect: [{profile: cargo-target, roots: []}]\n"), "empty"),
            (policy("protect: [{profile: cargo-target, roots: [relative]}]\n"), "absolute"),
            (policy("protect: [{profile: cargo-target, roots: [~/src]}]\n"), "~"),
            (policy("protect: [{profile: cargo-target, roots: [/a/../b]}]\n"), "not normalized"),
            (policy("protect: [{profile: cargo-target, roots: [/a, /a]}]\n"), "duplicate root"),
            (policy("groups:\n  - name: g\n    tiers:\n      - {name: t, select: 'profile:cargo-target', roots: [/a//b]}\n"), "not normalized"),
            (policy("owners: []\n"), "non-empty"),
            (policy("owners: null\n"), "owners"),
            (policy("owners: [mark, mark]\n"), "unique"),
            (policy("ipc: null\n"), "ipc"),
            (policy("ipc: sometimes\n"), "sometimes"),
        ] {
            let e = parsed(&text).expect_err(&text);
            assert!(e.contains(needle), "{needle}: {e}\n{text}");
        }
        assert!(
            parsed("version: [\n").is_err(),
            "malformed YAML must fail loading"
        );
    }

    #[test]
    fn embedded_profiles_and_selection_rules_are_checked_by_config_load() {
        let extra = "  - name: other\n    find: {roots: [/scratch], paths: ['y-*'], type: dir}\n    action: delete\n    examples: {match: [/scratch/y-a]}\n";
        let filter = "  - name: has-file\n    where: {has: marker}\n";
        let group =
            "groups:\n  - name: g\n    tiers:\n      - name: t\n        select: profile:scratch\n";
        let two = format!("version: 1\n{FS}{extra}{filter}{group}");
        for (text, needle) in [
            (
                two.replace("name: scratch", "name: cargo-target"),
                "cargo-target",
            ),
            (two.replace("name: scratch", "name: Bad.Name"), "name"),
            (
                two.replace("action: delete", "action: delete\n    typo: x"),
                "typo",
            ),
            (two.replace("roots: [/scratch]", "roots: [~/scratch]"), "~"),
            (
                two.replace("roots: [/scratch]", "roots: [scratch]"),
                "absolute",
            ),
            (two.replace("paths: ['x-*']", "paths: ['../x-*']"), ".."),
            (
                two.replace("match: [/scratch/x-a]", "match: [/scratch/no-match]"),
                "example",
            ),
            (
                two.replace(
                    "select: profile:scratch",
                    "select: profile:scratch || profile:cargo-target",
                ),
                "kind",
            ),
            (
                two.replace(
                    "select: profile:scratch",
                    "select: profile:scratch || profile:other",
                )
                .replace(
                    "paths: ['y-*'], type: dir}\n    action: delete",
                    "paths: ['y-*'], type: dir}\n    action: {command: [/bin/true]}",
                ),
                "action",
            ),
            (
                two.replace(
                    "select: profile:scratch",
                    "select: profile:cargo-target && filter:has-file",
                ),
                "has",
            ),
            (
                two.replace(
                    "select: profile:scratch",
                    "select: profile:scratch && profile:other",
                ),
                "source",
            ),
            (
                two.replace(
                    "select: profile:scratch",
                    "select: profile:scratch || profile:other && filter:idle-3d",
                ),
                "cannot be mixed",
            ),
            (
                two.replace("select: profile:scratch", "select: '!profile:other'"),
                "only narrows",
            ),
            (
                two.replace(
                    "select: profile:scratch",
                    "select: profile:scratch || filter:has-file",
                ),
                "filter",
            ),
            (
                two.replace(
                    "select: profile:scratch",
                    "select: profile:scratch && !profile:cargo-target",
                ),
                "roots: required",
            ),
        ] {
            let e = parsed(&text).expect_err(&text);
            assert!(e.contains(needle), "{needle}: {e}\n{text}");
        }
    }

    #[test]
    fn mode_checks_refuse_owners_mismatch_and_root_commands() {
        let ordinary = parsed(&policy("")).expect("loads");
        assert!(ordinary.validate_mode(false).is_ok());
        assert!(
            ordinary
                .validate_mode(true)
                .unwrap_err()
                .contains("requires an owners")
        );
        let root = parsed(&policy("owners: [mark]\n")).expect("loads");
        assert!(
            root.validate_mode(false)
                .unwrap_err()
                .contains("only with --allow-root")
        );
        assert!(root.validate_mode(true).is_ok());
        let command = "version: 1\nowners: [mark]\nprofiles:\n  - name: cleaner\n    find: {roots: [/scratch], paths: ['x-*'], type: dir}\n    action: {command: [/bin/true]}\n    examples: {match: [/scratch/x-a]}\ngroups:\n  - name: one\n    tiers:\n      - {name: clean, select: 'profile:cleaner'}\n";
        let command = parsed(command).expect("loads");
        assert!(
            command
                .validate_mode(true)
                .unwrap_err()
                .contains("command actions")
        );
    }

    #[test]
    fn explicit_owner_uids_are_distinct_from_names_and_checked_statically() {
        let config = parsed(&policy("owners: [tester, {uid: 2001}]\n")).expect("loads");
        assert_eq!(
            config.owners,
            Some(vec![
                Owner::Name("tester".to_owned()),
                Owner::Uid(UidOwner { uid: 2001 }),
            ])
        );
        assert!(config.validate_mode(true).is_ok());
        for (owners, reason) in [
            ("[{uid: 0}]", "system uid"),
            ("[{uid: 999}]", "system uid"),
            ("[{uid: 2001}, {uid: 2001}]", "unique"),
            ("['2001']", "ambiguous"),
            ("[{uid: 2001, name: tester}]", "owners"),
        ] {
            let error = parsed(&policy(&format!("owners: {owners}\n"))).unwrap_err();
            assert!(error.contains(reason), "{owners}: {error}");
        }
    }

    #[test]
    fn warnings_are_specific_and_nonfatal() {
        let broad = "version: 1\nprofiles:\n  - name: broad\n    find: {roots: [/home/mark/.cache], paths: ['*'], type: dir}\n    action: delete\n    checkouts: allow\n    examples: {match: [/home/mark/.cache/thing]}\n";
        let config = parsed(broad).expect("valid but broad");
        assert_eq!(config.warnings.len(), 2);
        assert!(config.warnings.iter().any(|w| w.contains("only wildcards")));
        assert!(config.warnings.iter().any(|w| w.contains("VCS checkouts")));
        let tmp = parsed(&broad.replace("/home/mark/.cache", "/tmp")).expect("valid tmp");
        assert_eq!(tmp.warnings.len(), 1);
        let required = broad
            .replace("roots: [/home/mark/.cache]", "roots: required")
            .replace("/home/mark/.cache/thing", "/any/thing");
        let bound = format!(
            "{required}groups:\n  - name: g\n    tiers:\n      - name: t\n        select: profile:broad\n        roots: [/home/mark/.cache]\n"
        );
        assert_eq!(
            parsed(&bound).expect("bound required root").warnings.len(),
            2
        );
        let overridden = broad.replace("name: broad", "name: pytest-cache\n    override: true");
        let config = parsed(&overridden).expect("user override with broad path");
        assert_eq!(
            config.library.get("pytest-cache").unwrap().origin,
            crate::profile::Origin::UserOverride
        );
        assert_eq!(config.warnings.len(), 2);
        assert!(config.warnings.iter().any(|w| w.contains("only wildcards")));
        assert!(config.warnings.iter().any(|w| w.contains("VCS checkouts")));
    }

    #[test]
    fn selection_warnings_reach_loaded_config_with_tier_context() {
        let config = parsed(&policy("groups:\n  - name: g\n    tiers:\n      - name: t\n        select: profile:scratch || profile:scratch\n"))
            .expect("duplicate selection is valid but worth warning about");
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("tier `t`: duplicate term `profile:scratch`")),
            "{:?}",
            config.warnings
        );
    }

    #[test]
    fn named_checkout_policy_changes_only_the_effective_source_setting() {
        let baseline = parsed("version: 1\n").expect("built-ins");
        let baseline_npm = baseline.library.get("npm-cache").unwrap().clone();
        let baseline_cargo = baseline.library.get("cargo-target").unwrap().clone();
        assert_eq!(
            baseline.library.checkouts("npm-cache"),
            Some(Checkouts::Refuse)
        );
        assert_eq!(
            baseline.library.checkouts("cargo-target"),
            Some(Checkouts::Allow)
        );

        let config = parsed("version: 1\nprofile_policies:\n  - profile: npm-cache\n    checkouts: allow\n  - profile: cargo-target\n    checkouts: refuse\n").expect("named policies load");
        assert_eq!(
            config.library.checkouts("npm-cache"),
            Some(Checkouts::Allow)
        );
        assert_eq!(
            config.library.checkouts("cargo-target"),
            Some(Checkouts::Refuse)
        );
        let mut npm = config.library.get("npm-cache").unwrap().clone();
        let mut cargo = config.library.get("cargo-target").unwrap().clone();
        if let Body::Source(source) = &mut npm.body {
            source.checkouts = Checkouts::Refuse;
        }
        if let Body::Source(source) = &mut cargo.body {
            source.checkouts = Checkouts::Allow;
        }
        assert_eq!(
            npm, baseline_npm,
            "npm matcher and action must be unchanged"
        );
        assert_eq!(
            cargo, baseline_cargo,
            "Cargo matcher and action must be unchanged"
        );
        assert!(
            config
                .warnings
                .iter()
                .any(|warning| warning.contains("npm-cache") && warning.contains("uncommitted"))
        );
    }

    #[test]
    fn named_checkout_policy_rejects_bad_or_ambiguous_entries() {
        for (body, message) in [
            (
                "  - profile: missing-cache\n    checkouts: allow\n",
                "unknown profile",
            ),
            ("  - profile: idle-1d\n    checkouts: allow\n", "filter"),
            (
                "  - profile: docker-build-cache\n    checkouts: allow\n",
                "no file candidates",
            ),
            (
                "  - profile: npm-cache\n    checkouts: allow\n  - profile: npm-cache\n    checkouts: refuse\n",
                "duplicate policy",
            ),
            ("  - profile: npm-cache\n    checkouts: ~\n", "checkouts"),
            (
                "  - profile: npm-cache\n    checkouts: maybe\n",
                "checkouts",
            ),
            (
                "  - profile: npm-cache\n    checkouts: allow\n    find: {roots: required}\n",
                "unknown field",
            ),
        ] {
            let error = parsed(&format!("version: 1\nprofile_policies:\n{body}")).unwrap_err();
            assert!(error.contains(message), "{body}: {error}");
        }
        let error = parsed("version: 1\nprofiles:\n  - name: npm-cache\n    override: true\n    find: {roots: required, paths: [.npm], type: dir}\n    action: delete\n    examples: {match: [/home/a/.npm]}\nprofile_policies:\n  - profile: npm-cache\n    checkouts: allow\n").unwrap_err();
        assert!(error.contains("specify checkouts in one place"), "{error}");
    }
}
