//! Sizes and durations as configuration and the command line write them
//! (docs/reference.md, Sizes and durations).
//!
//! - Sizes: an integer plus `K M G T` or `KiB MiB GiB TiB` (powers of
//!   1024) or `KB MB GB TB` (powers of 1000), or `N%` of the filesystem
//!   size. A bare number is an error.
//! - Durations: an integer plus one of `s m h d w`. No months; `m` is
//!   always minutes; a bare number is an error.

use std::fmt;
use std::time::Duration;

/// A size as written: bytes, or a share of the filesystem it applies to,
/// which only the filesystem's size turns into bytes ([`Size::bytes`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    Bytes(u64),
    /// `N%`: N percent of the filesystem size (`f_blocks × f_frsize`).
    Percent(u64),
}

/// The size units and their factors (docs/reference.md, Sizes and
/// durations). Case matters: `g` or `Gb` is no unit.
const SIZE_UNITS: [(&str, u64); 12] = [
    ("K", 1 << 10),
    ("M", 1 << 20),
    ("G", 1 << 30),
    ("T", 1 << 40),
    ("KiB", 1 << 10),
    ("MiB", 1 << 20),
    ("GiB", 1 << 30),
    ("TiB", 1 << 40),
    ("KB", 1_000),
    ("MB", 1_000_000),
    ("GB", 1_000_000_000),
    ("TB", 1_000_000_000_000),
];

/// Parses `50G`, `80GiB`, `500MB`, `10%`. The error says what is wrong in
/// words.
pub fn parse_size(text: &str) -> Result<Size, String> {
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    let (number, unit) = text.split_at(digits);
    if number.is_empty() {
        return Err(format!(
            "`{text}` is not a size: write an integer and a unit (K, M, G, T, KiB, ..., KB, \
             ..., or %), e.g. `50G`"
        ));
    }
    let too_big = || format!("`{text}`: the size is too large");
    let n: u64 = number.parse().map_err(|_| too_big())?;
    if unit.is_empty() {
        return Err(format!(
            "`{text}`: a bare number is not a size; add a unit (K, M, G, T, KiB, MiB, GiB, \
             TiB, KB, MB, GB, TB, or %)"
        ));
    }
    if unit == "%" {
        return Ok(Size::Percent(n));
    }
    let factor = SIZE_UNITS
        .iter()
        .find(|(u, _)| *u == unit)
        .map(|(_, f)| *f)
        .ok_or_else(|| {
            format!(
                "`{text}`: unknown unit `{unit}`; the units are K, M, G, T and KiB, MiB, GiB, \
                 TiB (powers of 1024), KB, MB, GB, TB (powers of 1000), and %"
            )
        })?;
    n.checked_mul(factor).map(Size::Bytes).ok_or_else(too_big)
}

impl Size {
    /// The bytes this size means without knowing the filesystem: a byte
    /// count as written, `None` for a percentage (only a sample of the
    /// filesystem's size turns it into bytes, [`Size::bytes`]).
    pub fn absolute_bytes(self) -> Option<u64> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            Self::Percent(_) => None,
        }
    }

    /// The bytes this size means on a filesystem of `fs_size` bytes. A size
    /// above the filesystem's is an error (docs/reference.md: a usage
    /// error), so is a percentage above 100.
    pub fn bytes(self, fs_size: u64) -> Result<u64, String> {
        let bytes = match self {
            Self::Bytes(b) => b,
            Self::Percent(p) => {
                if p > 100 {
                    return Err(format!("{p}% is more than the whole filesystem"));
                }
                // At most fs_size: exact in u128, no rounding up.
                u64::try_from(u128::from(fs_size) * u128::from(p) / 100).expect("at most fs_size")
            }
        };
        if bytes > fs_size {
            let [size, fs] = format_bytes_apart(bytes, fs_size);
            return Err(format!(
                "{size} ({bytes} bytes) is more than the filesystem's size, {fs} ({fs_size} bytes)"
            ));
        }
        Ok(bytes)
    }
}

/// The size as [`parse_size`] reads it back to the same value, in the
/// largest unit that divides it (`K` to `T`, `KB` to `TB`): `7TB`, not
/// `6835937500K`. Every parsed size is a multiple of one of them; a byte
/// count no unit divides (only code makes one) prints as `N bytes`. So a
/// message echoes `--min-free 1560M` as the user wrote it (TASK-217), not
/// `1635778560 bytes`; an equal value written in other units comes back
/// in these (`1024KB` as `1000K`).
impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Percent(p) => write!(f, "{p}%"),
            Self::Bytes(0) => write!(f, "0K"),
            Self::Bytes(b) => {
                let unit = ["T", "G", "M", "K", "TB", "GB", "MB", "KB"]
                    .into_iter()
                    .map(|unit| (unit, size_unit_factor(unit)))
                    .filter(|(_, factor)| b.is_multiple_of(*factor))
                    .max_by_key(|(_, factor)| *factor);
                match unit {
                    Some((unit, factor)) => write!(f, "{}{unit}", b / factor),
                    None => write!(f, "{b} bytes"),
                }
            }
        }
    }
}

fn size_unit_factor(unit: &str) -> u64 {
    SIZE_UNITS
        .iter()
        .find(|(u, _)| *u == unit)
        .map(|(_, factor)| *factor)
        .expect("a unit of SIZE_UNITS")
}

/// `bytes` for people: binary units with one decimal (`52.3 GiB`), whole
/// bytes below 1 KiB. Never parsed back.
pub fn format_bytes(bytes: u64) -> String {
    format_bytes_with(bytes, 1)
}

/// Two different sizes for people, each as [`format_bytes`] would, but
/// with as many decimals (up to three) as it takes to tell them apart:
/// `1.52 GiB` and `1.50 GiB`, never `1.5 GiB is more than 1.5 GiB`
/// (TASK-217). Values that still read alike are told apart by the byte
/// counts their messages give beside them.
pub fn format_bytes_apart(a: u64, b: u64) -> [String; 2] {
    (1..=3)
        .map(|decimals| {
            [
                format_bytes_with(a, decimals),
                format_bytes_with(b, decimals),
            ]
        })
        .find(|[x, y]| x != y)
        .unwrap_or_else(|| [format_bytes(a), format_bytes(b)])
}

fn format_bytes_with(bytes: u64, decimals: usize) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.decimals$} {}", UNITS[unit])
}

/// The units, longest first (the order [`format_duration`] tries them).
const UNITS: [(char, u64); 5] = [
    ('w', 7 * 24 * 3600),
    ('d', 24 * 3600),
    ('h', 3600),
    ('m', 60),
    ('s', 1),
];

/// Parses `3d`, `90m`, `1w`. The error says what is wrong in words.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    let (number, unit) = text.split_at(digits);
    if number.is_empty() {
        return Err(format!(
            "`{text}` is not a duration: write an integer and a unit (s, m, h, d or w), e.g. `3d`"
        ));
    }
    let seconds_per_unit = match unit {
        "" => {
            return Err(format!(
                "`{text}`: a bare number is not a duration; add a unit (s, m, h, d or w)"
            ));
        }
        "M" | "mo" | "month" | "months" => {
            return Err(format!(
                "`{text}`: durations have no months (`m` is minutes); write days or weeks"
            ));
        }
        _ => {
            let mut chars = unit.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => UNITS
                    .iter()
                    .find(|(u, _)| *u == c)
                    .map(|(_, secs)| *secs)
                    .ok_or_else(|| unknown_unit(text, unit))?,
                _ => return Err(unknown_unit(text, unit)),
            }
        }
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(seconds_per_unit))
        .map(Duration::from_secs)
        .ok_or_else(|| format!("`{text}`: the duration is too long"))
}

fn unknown_unit(text: &str, unit: &str) -> String {
    format!("`{text}`: unknown unit `{unit}`; the units are s, m, h, d and w")
}

/// Text [`parse_duration`] reads back as `duration` (whole seconds;
/// sub-second parts are dropped), in the longest unit up to days that
/// divides it: `7d`, not `1w`, as the built-in filters are named.
pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let (unit, per) = UNITS
        .iter()
        .filter(|(unit, _)| *unit != 'w')
        .find(|(_, per)| secs != 0 && secs.is_multiple_of(*per))
        .copied()
        .unwrap_or(('s', 1));
    format!("{}{unit}", secs / per)
}

/// An age or interval of `seconds` for people, in its largest whole unit
/// (truncated, so a reader never sees an age larger than the real one):
/// `56 s`, `59 m`, `1 h`, `6 d`. Never parsed back; config text uses
/// [`format_duration`].
pub fn format_age(seconds: u64) -> String {
    let (amount, unit) = UNITS
        .iter()
        .filter(|(unit, _)| *unit != 'w')
        .find(|(_, per)| seconds >= *per)
        .map_or((seconds, 's'), |(unit, per)| (seconds / per, *unit));
    format!("{amount} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_truncate_to_their_largest_whole_unit() {
        for (seconds, text) in [
            (0, "0 s"),
            (56, "56 s"),
            (60, "1 m"),
            (3_599, "59 m"),
            (3_742, "1 h"),
            (86_399, "23 h"),
            (604_800, "7 d"),
        ] {
            assert_eq!(format_age(seconds), text, "{seconds}");
        }
    }

    #[test]
    fn durations_are_an_integer_and_a_unit() {
        for (text, secs) in [
            ("0s", 0),
            ("45s", 45),
            ("90m", 5400),
            ("1h", 3600),
            ("2d", 172_800),
            ("1w", 604_800),
            ("007d", 7 * 86_400),
        ] {
            assert_eq!(
                parse_duration(text),
                Ok(Duration::from_secs(secs)),
                "{text}"
            );
        }
    }

    #[test]
    fn bare_numbers_months_and_other_forms_are_errors() {
        for (text, needle) in [
            ("3", "bare number"),
            ("1M", "no months"),
            ("1mo", "no months"),
            ("3 d", "unknown unit"),
            ("3dd", "unknown unit"),
            ("1y", "unknown unit"),
            ("d", "not a duration"),
            ("", "not a duration"),
            ("-1d", "not a duration"),
            ("+1d", "not a duration"),
            ("1.5h", "unknown unit"),
            ("99999999999999999999s", "too long"),
            ("99999999999999999w", "too long"),
        ] {
            let error = parse_duration(text).expect_err(text);
            assert!(error.contains(needle), "{text}: {error}");
        }
    }

    #[test]
    fn sizes_are_an_integer_and_a_unit_or_a_percentage() {
        for (text, want) in [
            ("0K", Size::Bytes(0)),
            ("1K", Size::Bytes(1024)),
            ("50G", Size::Bytes(50 << 30)),
            ("50GiB", Size::Bytes(50 << 30)),
            ("2T", Size::Bytes(2 << 40)),
            ("3MiB", Size::Bytes(3 << 20)),
            ("1KiB", Size::Bytes(1024)),
            ("5TiB", Size::Bytes(5 << 40)),
            ("1KB", Size::Bytes(1000)),
            ("500MB", Size::Bytes(500_000_000)),
            ("50GB", Size::Bytes(50_000_000_000)),
            ("1TB", Size::Bytes(1_000_000_000_000)),
            ("10%", Size::Percent(10)),
            ("0%", Size::Percent(0)),
            ("100%", Size::Percent(100)),
        ] {
            assert_eq!(parse_size(text), Ok(want), "{text}");
        }
    }

    /// AC #4 of task-10 (sizes half): a bare number is a usage error, and
    /// so is every unit the doc does not list.
    #[test]
    fn bare_numbers_and_unknown_units_are_not_sizes() {
        for (text, needle) in [
            ("50", "bare number"),
            ("0", "bare number"),
            ("50g", "unknown unit"),
            ("50Gb", "unknown unit"),
            ("50gib", "unknown unit"),
            ("50B", "unknown unit"),
            ("1.5G", "unknown unit"),
            ("50 G", "unknown unit"),
            ("G", "not a size"),
            ("", "not a size"),
            ("-1G", "not a size"),
            ("99999999999999999999G", "too large"),
            ("99999999T", "too large"),
        ] {
            let error = parse_size(text).expect_err(text);
            assert!(error.contains(needle), "{text}: {error}");
        }
    }

    #[test]
    fn a_size_is_resolved_against_the_filesystem_and_never_exceeds_it() {
        let fs = 1000;
        assert_eq!(Size::Percent(10).bytes(fs), Ok(100));
        assert_eq!(Size::Percent(100).bytes(fs), Ok(1000));
        assert_eq!(Size::Percent(0).bytes(fs), Ok(0));
        // Rounded down, never above the share.
        assert_eq!(Size::Percent(33).bytes(10), Ok(3));
        assert_eq!(Size::Percent(50).bytes(u64::MAX), Ok(u64::MAX / 2));
        assert_eq!(Size::Bytes(1000).bytes(fs), Ok(1000));
        let too_large = Size::Bytes(16 << 30).bytes(15 << 30).unwrap_err();
        assert!(
            too_large.contains("16.0 GiB") && too_large.contains("15.0 GiB"),
            "{too_large}"
        );
        assert!(Size::Percent(101).bytes(fs).is_err());
    }

    /// TASK-217, the Debian walk: `--min-free 1560M` on a 1.5 GiB tmpfs
    /// read "1.5 GiB (1635778560 bytes) is more than the filesystem's size
    /// (1.5 GiB; ...)". The sizes now differ where the reader looks.
    #[test]
    fn a_size_just_above_the_filesystem_reads_as_larger() {
        let size = parse_size("1560M").unwrap();
        let error = size.bytes(1_610_612_736).unwrap_err();
        assert_eq!(
            format!("--min-free {size}: {error}"),
            "--min-free 1560M: 1.52 GiB (1635778560 bytes) is more than the filesystem's \
             size, 1.50 GiB (1610612736 bytes)"
        );
    }

    #[test]
    fn sizes_are_told_apart_with_as_few_decimals_as_needed() {
        assert_eq!(
            format_bytes_apart(16 << 30, 15 << 30),
            ["16.0 GiB", "15.0 GiB"]
        );
        assert_eq!(
            format_bytes_apart(1_635_778_560, 1_610_612_736),
            ["1.52 GiB", "1.50 GiB"]
        );
        // Three decimals are not enough for one byte: the counts beside
        // them in the message tell these apart.
        assert_eq!(
            format_bytes_apart(1 << 30, (1 << 30) + 1),
            ["1.0 GiB", "1.0 GiB"]
        );
        assert_eq!(format_bytes_apart(100, 101), ["100 B", "101 B"]);
    }

    #[test]
    fn sizes_display_in_the_unit_they_were_written_in() {
        for text in [
            "1560M", "50G", "500MB", "1GB", "2K", "3T", "7TB", "10%", "0%", "0K",
        ] {
            assert_eq!(parse_size(text).unwrap().to_string(), text);
        }
        assert_eq!(parse_size("80GiB").unwrap().to_string(), "80G");
        assert_eq!(parse_size("1024KB").unwrap().to_string(), "1000K");
        assert_eq!(Size::Bytes(1_000_001).to_string(), "1000001 bytes");
    }

    proptest::proptest! {
        /// Whatever a user writes, the size it echoes parses back to the
        /// same value.
        #[test]
        fn a_displayed_size_parses_back_to_itself(n in 0u64..5_000_000, unit in 0usize..SIZE_UNITS.len()) {
            let text = format!("{n}{}", SIZE_UNITS[unit].0);
            if let Ok(size) = parse_size(&text) {
                proptest::prop_assert_eq!(parse_size(&size.to_string()), Ok(size), "{}", text);
            }
        }
    }

    #[test]
    fn bytes_for_people() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(50 << 30), "50.0 GiB");
        assert_eq!(format_bytes((3 << 20) / 2), "1.5 MiB");
    }

    #[test]
    fn formatting_uses_the_longest_exact_unit_and_parses_back() {
        for (secs, text) in [
            (0, "0s"),
            (59, "59s"),
            (60, "1m"),
            (3600, "1h"),
            (5400, "90m"),
            (86_400, "1d"),
            (7 * 86_400, "7d"),
            (10 * 86_400, "10d"),
            (30 * 86_400, "30d"),
        ] {
            let duration = Duration::from_secs(secs);
            assert_eq!(format_duration(duration), text);
            assert_eq!(parse_duration(text), Ok(duration));
        }
    }
}
