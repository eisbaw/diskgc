//! "Did you mean" for names a user mistyped: flags and commands on the
//! command line, profile names in selections and `profiles show`. Pure and
//! bounded: an edit-distance scan costs O(len(a) * len(b)), so names longer
//! than [`MAX_NAME_CHARS`] get no suggestion rather than a slow one.

/// The longest name worth a suggestion. A typo is short; a longer input
/// (a pasted path, an attack on the diagnostics) gets none.
pub const MAX_NAME_CHARS: usize = 256;

/// The most edits a suggestion may be away from what was written: one or
/// two slips (`--min-fre`, `--dryrun`), not a different word.
pub const MAX_EDITS: usize = 2;

/// Character edit distance (Levenshtein), for short diagnostic suggestions.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut current = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let substitute = previous[j] + usize::from(ca != *cb);
            current.push(substitute.min(previous[j + 1] + 1).min(current[j] + 1));
        }
        previous = current;
    }
    previous[b.len()]
}

/// The one known name `written` most likely meant: the nearest within
/// [`MAX_EDITS`] (ties: the first in `known`'s order), else the only known
/// name `written` is the start of (`--tally` for `--tally-budget`), else
/// none. Never `written` itself.
pub fn closest<'a>(written: &str, known: &[&'a str]) -> Option<&'a str> {
    let written_len = written.chars().count();
    if written_len > MAX_NAME_CHARS {
        return None;
    }
    let nearest = known
        .iter()
        .filter(|k| **k != written && written_len.abs_diff(k.chars().count()) <= MAX_EDITS)
        .map(|k| (edit_distance(written, k), *k))
        .filter(|(distance, _)| *distance <= MAX_EDITS)
        .min_by_key(|(distance, _)| *distance);
    if let Some((_, name)) = nearest {
        return Some(name);
    }
    // A prefix of one name only; "--" or "-" alone says nothing.
    let mut prefixed = known.iter().filter(|k| {
        **k != written && written.trim_start_matches('-').len() >= 2 && k.starts_with(written)
    });
    match (prefixed.next(), prefixed.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAGS: [&str; 5] = [
        "--min-free",
        "--target-free",
        "--dry-run",
        "--json",
        "--tally-budget",
    ];

    #[test]
    fn a_slip_or_two_suggests_the_flag() {
        for (written, want) in [
            ("--min-fre", Some("--min-free")),
            ("--minfree", Some("--min-free")),
            ("--min_free", Some("--min-free")),
            ("--dryrun", Some("--dry-run")),
            ("--jsno", Some("--json")),
            ("--tally", Some("--tally-budget")),
            ("--frobnicate", None),
            ("--", None),
            ("--json", None),
            ("-", None),
        ] {
            assert_eq!(closest(written, &FLAGS), want, "{written}");
        }
    }

    #[test]
    fn an_ambiguous_prefix_suggests_nothing() {
        assert_eq!(closest("--ta", &["--target-free", "--tally-budget"]), None);
    }

    #[test]
    fn a_huge_name_gets_no_suggestion() {
        let huge = format!("--{}", "m".repeat(MAX_NAME_CHARS));
        assert_eq!(closest(&huge, &FLAGS), None);
    }
}
