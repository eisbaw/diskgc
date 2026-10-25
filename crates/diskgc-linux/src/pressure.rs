//! Pressure stall information (`/proc/pressure/<resource>`, Documentation
//! /accounting/psi.rst): the `some avg10` value the background tally's
//! pressure brake reads ([`diskgc_core::host::Host::pressure`]).
//!
//! The file's first line is
//! `some avg10=1.23 avg60=0.50 avg300=0.10 total=123456`; `cpu` has a
//! `full` line too from 5.13 on. The averages are printed with exactly two
//! decimals (`LOAD_INT`/`LOAD_FRAC`), so they parse exactly into
//! hundredths of a percent, with no floating point.

/// The `some avg10` of a pressure file's text, in hundredths of a percent.
pub(crate) fn some_avg10(text: &[u8]) -> Result<u32, String> {
    let text = std::str::from_utf8(text).map_err(|_| "not UTF-8")?;
    let line = text
        .lines()
        .find(|line| line.starts_with("some "))
        .ok_or("no `some` line")?;
    let value = line
        .split_ascii_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))
        .ok_or("no `avg10=` field on the `some` line")?;
    let (whole, fraction) = value
        .split_once('.')
        .ok_or_else(|| format!("avg10 `{value}` has no decimal point"))?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || fraction.len() != 2 || !digits(fraction) {
        return Err(format!(
            "avg10 `{value}` is not a percentage with two decimals"
        ));
    }
    let whole: u32 = whole.parse().map_err(|e| format!("avg10 `{value}`: {e}"))?;
    let fraction: u32 = fraction
        .parse()
        .map_err(|e| format!("avg10 `{value}`: {e}"))?;
    if whole > 100 {
        return Err(format!("avg10 `{value}` is above 100 %"));
    }
    Ok(whole * 100 + fraction)
}

#[cfg(test)]
mod tests {
    use super::some_avg10;

    #[test]
    fn reads_the_some_line_in_hundredths() {
        let io = "some avg10=12.34 avg60=1.00 avg300=0.10 total=99\n\
                  full avg10=3.00 avg60=0.00 avg300=0.00 total=9\n";
        assert_eq!(some_avg10(io.as_bytes()), Ok(1234));
        assert_eq!(
            some_avg10(b"some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"),
            Ok(0)
        );
        assert_eq!(
            some_avg10(b"some avg10=100.00 avg60=0.00 avg300=0.00 total=0\n"),
            Ok(10_000)
        );
        // cpu before 5.13: one line.
        assert_eq!(some_avg10(b"some avg10=50.01 avg60=0 total=1"), Ok(5001));
    }

    #[test]
    fn refuses_what_is_not_a_psi_file() {
        for text in [
            "",
            "full avg10=1.00 avg60=0.00 avg300=0.00 total=0",
            "some avg60=1.00",
            "some avg10=1.0 avg60=0.00",
            "some avg10=1 avg60=0.00",
            "some avg10=-1.00",
            "some avg10=1.0a",
            "some avg10=101.00",
            "some avg10=99999999999.00",
        ] {
            assert!(some_avg10(text.as_bytes()).is_err(), "{text:?}");
        }
    }
}
