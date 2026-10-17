//! Durations as configuration and the command line write them
//! (docs/reference.md, Sizes and durations): an integer plus one of
//! `s m h d w`. No months; `m` is always minutes; a bare number is an error.
//!
//! Sizes are not here yet (task-10 adds them next to these).

use std::time::Duration;

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

#[cfg(test)]
mod tests {
    use super::*;

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
