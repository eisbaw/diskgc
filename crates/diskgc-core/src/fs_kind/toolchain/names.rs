//! The managers' toolchain name grammars, pure (docs/safety.md, Toolchain
//! pins): which directory names a manager installed and can install again,
//! and which installed directories a name in a pin selects.
//!
//! rustup 1.29.1 (`src/dist/mod.rs`, `ParsedToolchainDesc`): a channel
//! (`nightly`, `beta`, `stable`, or a version `M.m[.p][-beta[.N]]`, `1.0`
//! through `1.8` read as `.0`), then an optional `-YYYY-MM-DD`, then an
//! optional `-<target>`. elan 4.2.4 (`lookup_unresolved_toolchain_desc`,
//! `ToolchainDesc::from_resolved_str`): `owner/repo:release` or a bare
//! release of `leanprover/lean4`, stored with `/` written `--` and `:`
//! written `---`.

use std::borrow::Cow;

/// elan's default origin for a name without one.
const ELAN_DEFAULT_ORIGIN: &str = "leanprover/lean4";
/// elan's names that resolve through the network to a release.
const ELAN_CHANNELS: [&str; 3] = ["stable", "beta", "nightly"];
/// elan's release name that fetches another repository's pin.
const ELAN_REMOTE_PIN: &str = "lean-toolchain";

/// A rustup name as rustup's regex reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RustupName<'a> {
    /// The channel, `1.0` through `1.8` written as rustup aliases them.
    pub channel: Cow<'a, str>,
    pub date: Option<&'a str>,
    pub target: Option<&'a str>,
}

/// rustup's `ParsedToolchainDesc` of `name`, or `None` for a name that is
/// not an official one (rustup then reads it as a custom toolchain name).
/// The regex is emulated in its own order: alternatives left to right,
/// optional groups present first, quantifiers greedy.
pub(crate) fn rustup_parse(name: &str) -> Option<RustupName<'_>> {
    let mut ends: Vec<usize> = ["nightly", "beta", "stable"]
        .into_iter()
        .filter(|word| name.starts_with(word))
        .map(str::len)
        .collect();
    ends.extend(rustup_version_ends(name.as_bytes()));
    ends.into_iter().find_map(|end| {
        let (date, target) = date_and_target(&name[end..])?;
        let channel = &name[..end];
        let channel = match channel {
            "1.0" | "1.1" | "1.2" | "1.3" | "1.4" | "1.5" | "1.6" | "1.7" | "1.8" => {
                Cow::Owned(format!("{channel}.0"))
            }
            other => Cow::Borrowed(other),
        };
        Some(RustupName {
            channel,
            date,
            target,
        })
    })
}

/// The ends of every prefix of `name` that matches rustup's version
/// pattern `[0-9]\.(0|[1-9][0-9]{0,2})(\.(0|[1-9][0-9]?))?(-beta(\.[0-9]{1,2})?)?`,
/// in the order the regex tries them.
fn rustup_version_ends(name: &[u8]) -> Vec<usize> {
    let digit = |at: usize| name.get(at).is_some_and(u8::is_ascii_digit);
    let mut out = Vec::new();
    if !digit(0) || name.get(1) != Some(&b'.') {
        return out;
    }
    // `0`, or `[1-9]` then up to two more digits, greedy.
    let numbers = |at: usize, max_more: usize| -> Vec<usize> {
        match name.get(at) {
            Some(b'0') => vec![at + 1],
            Some(b'1'..=b'9') => {
                let more = (1..=max_more).take_while(|i| digit(at + i)).count();
                (0..=more).rev().map(|i| at + 1 + i).collect()
            }
            _ => Vec::new(),
        }
    };
    for minor_end in numbers(2, 2) {
        let mut patches: Vec<usize> = Vec::new();
        if name.get(minor_end) == Some(&b'.') {
            patches.extend(numbers(minor_end + 1, 1));
        }
        patches.push(minor_end);
        for patch_end in patches {
            if name[patch_end..].starts_with(b"-beta") {
                let beta = patch_end + "-beta".len();
                if name.get(beta) == Some(&b'.') && digit(beta + 1) {
                    if digit(beta + 2) {
                        out.push(beta + 3);
                    }
                    out.push(beta + 2);
                }
                out.push(beta);
            }
            out.push(patch_end);
        }
    }
    out
}

/// `(?:-([0-9]{4}-[0-9]{2}-[0-9]{2}))?(?:-(.+))?$` on `rest`.
fn date_and_target(rest: &str) -> Option<(Option<&str>, Option<&str>)> {
    fn target(text: &str) -> Option<Option<&str>> {
        if text.is_empty() {
            Some(None)
        } else {
            text.strip_prefix('-')
                .filter(|t| !t.is_empty() && !t.contains('\n'))
                .map(Some)
        }
    }
    if let Some(dated) = rest.strip_prefix('-')
        && let Some(date) = dated.get(..10).filter(|d| is_date(d))
        && let Some(target) = target(&dated[10..])
    {
        return Some((Some(date), target));
    }
    target(rest).map(|target| (None, target))
}

/// `YYYY-MM-DD`, ASCII digits.
fn is_date(text: &str) -> bool {
    text.len() == 10
        && text.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
}

/// Whether `dir`, a directory in rustup's `toolchains/`, is one rustup
/// installed from its dist server and can install again: an official
/// name with a full target triple (at least three `-`-separated parts).
/// Anything else (`esp` from espup, `nightly-local`, a stage1 copy) is a
/// custom toolchain whose loss cannot be undone by the manager.
pub(crate) fn rustup_reinstallable(dir: &str) -> bool {
    rustup_parse(dir)
        .and_then(|name| name.target)
        .is_some_and(|target| {
            let parts: Vec<&str> = target.split('-').collect();
            parts.len() >= 3 && parts.iter().all(|part| !part.is_empty())
        })
}

/// Whether the rustup name `spec` (a default, override, pin or
/// `RUSTUP_TOOLCHAIN`) selects the installed directory `dir`. Official
/// names compare channel and date and ignore the target: rustup completes
/// a partial target from the host, so `stable-gnu` keeps
/// `stable-x86_64-unknown-linux-gnu` (and the same channel for another
/// target: more is kept, not less). Any other name compares whole.
pub(crate) fn rustup_selects(spec: &str, dir: &str) -> bool {
    let spec = spec.trim();
    match (rustup_parse(spec), rustup_parse(dir)) {
        (Some(pinned), Some(installed)) => {
            pinned.channel == installed.channel && pinned.date == installed.date
        }
        _ => spec == dir,
    }
}

/// What an elan name selects among installed directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ElanTarget {
    /// One directory, by its encoded name.
    Dir(String),
    /// Every directory of this encoded origin: a channel the network
    /// resolves.
    Origin(String),
    /// Another repository's `lean-toolchain`, fetched and resolved again:
    /// any installed toolchain may be the one (the name).
    Remote(String),
    /// A path toolchain (the text as written).
    Path(String),
}

/// elan's reading of the name `spec`; `Err` for text elan refuses.
pub(crate) fn elan_target(spec: &str) -> Result<ElanTarget, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("an empty toolchain name".to_owned());
    }
    let (origin, release) = match spec.split_once(':') {
        Some((origin, release)) if elan_origin(origin) && elan_release(release) => {
            (origin.to_owned(), release)
        }
        None if elan_release(spec) => (ELAN_DEFAULT_ORIGIN.to_owned(), spec),
        _ if spec.starts_with(['/', '.']) || spec.contains('/') => {
            return Ok(ElanTarget::Path(spec.to_owned()));
        }
        _ => return Err(format!("{spec:?} is not an elan toolchain name")),
    };
    if release == ELAN_REMOTE_PIN {
        return Ok(ElanTarget::Remote(spec.to_owned()));
    }
    let origin = if release.starts_with("nightly") && !origin.ends_with("-nightly") {
        format!("{origin}-nightly")
    } else {
        origin
    };
    if ELAN_CHANNELS.contains(&release) {
        return Ok(ElanTarget::Origin(elan_encode(&origin)));
    }
    let release = if release.starts_with(|c: char| c.is_ascii_digit()) {
        format!("v{release}")
    } else {
        release.to_owned()
    };
    Ok(ElanTarget::Dir(elan_encode(&format!("{origin}:{release}"))))
}

/// Whether `dir`, a directory in elan's `toolchains/`, is one elan
/// installed from a release and can install again: it decodes (elan's
/// `from_toolchain_dir`: `---` is `:`, `--` is `/`) to a name whose own
/// normal form ([`elan_target`]) is exactly this directory. That excludes
/// a linked or copied custom toolchain (`lean4-patched`), a channel
/// directory an old elan made (`---stable`, which elan 4 resolves to a
/// release instead) and an ambiguous encoding.
pub(crate) fn elan_reinstallable(dir: &str) -> bool {
    let decoded = dir.replace("---", ":").replace("--", "/");
    decoded.contains(':') && elan_target(&decoded) == Ok(ElanTarget::Dir(dir.to_owned()))
}

/// `owner/repository`, each `[A-Za-z0-9_-]+`.
fn elan_origin(text: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    text.split_once('/')
        .is_some_and(|(owner, repo)| part(owner) && part(repo))
}

/// `[A-Za-z0-9.-]+`.
fn elan_release(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

pub(crate) fn elan_encode(name: &str) -> String {
    name.replace('/', "--").replace(':', "---")
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const HOST: &str = "x86_64-unknown-linux-gnu";

    #[test]
    fn rustup_names_parse_as_rustups_regex_reads_them() {
        let parsed = |name| rustup_parse(name).map(|n| (n.channel.into_owned(), n.date, n.target));
        let some = |c: &str, d, t| Some((c.to_owned(), d, t));
        assert_eq!(parsed("stable"), some("stable", None, None));
        assert_eq!(
            parsed("nightly-2025-11-21"),
            some("nightly", Some("2025-11-21"), None)
        );
        assert_eq!(
            parsed("nightly-2025-11-21-x86_64-unknown-linux-gnu"),
            some("nightly", Some("2025-11-21"), Some(HOST))
        );
        assert_eq!(
            parsed("1.90.0-x86_64-unknown-linux-gnu"),
            some("1.90.0", None, Some(HOST))
        );
        assert_eq!(parsed("1.90"), some("1.90", None, None));
        assert_eq!(parsed("1.8"), some("1.8.0", None, None));
        assert_eq!(
            parsed("1.8-x86_64-unknown-linux-gnu"),
            some("1.8.0", None, Some(HOST))
        );
        assert_eq!(parsed("1.9"), some("1.9", None, None));
        assert_eq!(parsed("1.80.0-beta.3"), some("1.80.0-beta.3", None, None));
        assert_eq!(
            parsed("1.80.0-beta-gnu"),
            some("1.80.0-beta", None, Some("gnu"))
        );
        assert_eq!(
            parsed("nightly-local"),
            some("nightly", None, Some("local"))
        );
        assert_eq!(parsed("stable-"), None);
        assert_eq!(parsed("esp"), None);
        assert_eq!(parsed("10.1.0"), None);
        assert_eq!(parsed("1.0123"), None);
    }

    #[test]
    fn only_official_names_with_a_full_triple_are_reinstallable() {
        for dir in [
            format!("stable-{HOST}"),
            format!("1.90.0-{HOST}"),
            format!("nightly-2025-11-21-{HOST}"),
            "1.8.0-aarch64-apple-darwin".to_owned(),
        ] {
            assert!(rustup_reinstallable(&dir), "{dir}");
        }
        for dir in [
            "esp",
            "nightly-local",
            "stable",
            "custom",
            "stage1",
            "nightly-x86_64",
            "1.90.0-a--b",
        ] {
            assert!(!rustup_reinstallable(dir), "{dir}");
        }
    }

    #[test]
    fn rustup_compares_channel_and_date_not_the_triple() {
        for (spec, dir, expected) in [
            (
                "nightly-x86_64-unknown-linux-gnu",
                format!("nightly-{HOST}"),
                true,
            ),
            ("nightly", format!("nightly-{HOST}"), true),
            ("nightly", format!("nightly-2025-11-21-{HOST}"), false),
            (
                "nightly-2025-11-21",
                format!("nightly-2025-11-21-{HOST}"),
                true,
            ),
            (
                "nightly-2025-11-21",
                format!("nightly-2025-11-22-{HOST}"),
                false,
            ),
            ("1.90.0", format!("1.90.0-{HOST}"), true),
            ("1.90.0", format!("1.90-{HOST}"), false),
            ("1.90", format!("1.90-{HOST}"), true),
            ("1.8", format!("1.8.0-{HOST}"), true),
            ("1.8.0", format!("1.8-{HOST}"), true),
            ("stable", format!("stable-{HOST}"), true),
            ("stable-gnu", format!("stable-{HOST}"), true),
            ("stable", format!("beta-{HOST}"), false),
            ("1.80.0-beta.3", format!("1.80.0-beta.3-{HOST}"), true),
            ("1.80.0-beta.3", format!("1.80.0-{HOST}"), false),
            (" 1.90.0\n", format!("1.90.0-{HOST}"), true),
            ("custom", "custom".to_owned(), true),
            ("custom", "custom-2".to_owned(), false),
        ] {
            assert_eq!(rustup_selects(spec, &dir), expected, "{spec:?} vs {dir:?}");
        }
    }

    #[test]
    fn elan_normalises_names_as_elan_does() {
        let dir = |d: &str| Ok(ElanTarget::Dir(d.to_owned()));
        let origin = |o: &str| Ok(ElanTarget::Origin(o.to_owned()));
        assert_eq!(
            elan_target("leanprover/lean4:v4.16.0"),
            dir("leanprover--lean4---v4.16.0")
        );
        assert_eq!(elan_target("4.16.0"), dir("leanprover--lean4---v4.16.0"));
        assert_eq!(elan_target("v4.16.0"), dir("leanprover--lean4---v4.16.0"));
        assert_eq!(
            elan_target("nightly-2025-01-01"),
            dir("leanprover--lean4-nightly---nightly-2025-01-01")
        );
        assert_eq!(elan_target("stable"), origin("leanprover--lean4"));
        assert_eq!(elan_target("nightly"), origin("leanprover--lean4-nightly"));
        assert_eq!(elan_target("other/fork:beta"), origin("other--fork"));
        assert_eq!(
            elan_target("leanprover-community/mathlib4:lean-toolchain"),
            Ok(ElanTarget::Remote(
                "leanprover-community/mathlib4:lean-toolchain".to_owned()
            ))
        );
        assert_eq!(
            elan_target("lean-toolchain"),
            Ok(ElanTarget::Remote("lean-toolchain".to_owned()))
        );
        assert_eq!(
            elan_target("../lean4/build/release/stage1"),
            Ok(ElanTarget::Path("../lean4/build/release/stage1".to_owned()))
        );
        for bad in ["", "  ", "lean4:v1", "a b"] {
            assert!(elan_target(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_encoded_releases_are_elan_reinstallable() {
        for dir in [
            "leanprover--lean4---v4.16.0",
            "leanprover--lean4-nightly---nightly-2025-01-01",
            "other_org--fork---v1.0.0-rc1",
        ] {
            assert!(elan_reinstallable(dir), "{dir}");
        }
        for dir in [
            "lean4-patched",
            "leanprover--lean4---stable",
            "leanprover--lean4---lean-toolchain",
            "leanprover--lean4",
            "a---b--c---v1",
            "x--y---v1:2",
            "leanprover--lean4---4.16.0",
            "leanprover--lean4---nightly-2025-01-01",
        ] {
            assert!(!elan_reinstallable(dir), "{dir}");
        }
    }

    proptest! {
        /// A reinstallable rustup directory parses, and selects itself and
        /// every name of the same channel and date.
        #[test]
        fn rustup_reinstallable_names_select_themselves(
            channel in prop_oneof![
                Just("stable".to_owned()), Just("beta".to_owned()), Just("nightly".to_owned()),
                (1u8..10, 0u16..1000, proptest::option::of(0u8..100))
                    .prop_map(|(a, b, c)| match c { Some(c) => format!("{a}.{b}.{c}"), None => format!("{a}.{b}") }),
            ],
            date in proptest::option::of((2015u16..2100, 1u8..13, 1u8..29)),
            arch in "[a-z0-9_]{1,8}", vendor in "[a-z]{1,8}", os in "[a-z]{1,8}",
        ) {
            let date = date.map(|(y, m, d)| format!("-{y:04}-{m:02}-{d:02}")).unwrap_or_default();
            let dir = format!("{channel}{date}-{arch}-{vendor}-{os}");
            prop_assert!(rustup_reinstallable(&dir), "{}", dir);
            prop_assert!(rustup_selects(&dir, &dir));
            let pin = format!("{channel}{date}");
            prop_assert!(rustup_selects(&pin, &dir));
        }

        /// No text panics either grammar, and every elan directory that is
        /// reinstallable is selected by its own decoded name.
        #[test]
        fn grammars_are_total(text in "\\PC{0,40}") {
            let _ = rustup_parse(&text);
            let _ = rustup_selects(&text, "stable-x86_64-unknown-linux-gnu");
            let _ = elan_target(&text);
            if elan_reinstallable(&text) {
                let decoded = text.replace("---", ":").replace("--", "/");
                prop_assert_eq!(elan_target(&decoded), Ok(ElanTarget::Dir(text.clone())));
            }
        }
    }
}
