//! Name families (docs/reference.md, `survey`): which names a collapse
//! puts into one block before it collapses all names
//! (docs/architecture.md, Memory budget and collapse).
//!
//! The rule, applied in this order: a trailing suffix after the last `.`,
//! `-` or `_` (not at the start of the name) of five or more ASCII
//! letters and digits, at least one of them a letter, becomes `*`; then
//! every run of ASCII digits becomes `#`. So `nix-shell.8raHjU` and
//! `nix-shell.12a345` are both `nix-shell.*` (mktemp's suffixes), while an
//! all-digit suffix is a number: `nix-shell-1144586-3257819220` is
//! `nix-shell-#-#`.

/// The name family of `name`.
pub fn name_family(name: &[u8]) -> Box<[u8]> {
    let mut stem = name;
    if let Some(separator) = name.iter().rposition(|b| matches!(b, b'.' | b'-' | b'_')) {
        let suffix = &name[separator + 1..];
        if separator > 0
            && suffix.len() >= 5
            && suffix.iter().all(u8::is_ascii_alphanumeric)
            && suffix.iter().any(u8::is_ascii_alphabetic)
        {
            stem = &name[..=separator];
        }
    }
    let mut out = Vec::with_capacity(stem.len() + 1);
    let mut in_digits = false;
    for &byte in stem {
        let digit = byte.is_ascii_digit();
        if !digit {
            out.push(byte);
        } else if !in_digits {
            out.push(b'#');
        }
        in_digits = digit;
    }
    if stem.len() < name.len() {
        out.push(b'*');
    }
    out.into_boxed_slice()
}

#[cfg(test)]
mod tests {
    use super::name_family;

    fn family(name: &str) -> String {
        String::from_utf8(name_family(name.as_bytes()).into_vec()).unwrap()
    }

    #[test]
    fn the_reference_examples() {
        assert_eq!(family("nix-shell.8raHjU"), "nix-shell.*");
        assert_eq!(family("nix-shell-1144586-3257819220"), "nix-shell-#-#");
    }

    #[test]
    fn digits_runs_suffixes_and_what_stays() {
        assert_eq!(family("pytest-of-mark"), "pytest-of-mark");
        assert_eq!(family("tmp.Xy12ab"), "tmp.*");
        assert_eq!(family("nix-shell.12a345"), "nix-shell.*");
        assert_eq!(family("nix-shell.123456"), "nix-shell.#");
        assert_eq!(family("tmp.ab1"), "tmp.ab#");
        assert_eq!(family("v2-build-cache"), "v#-build-*");
        assert_eq!(family(".bashrc"), ".bashrc");
        assert_eq!(family("rust_12_34"), "rust_#_#");
        assert_eq!(family("a#12"), "a##");
        assert_eq!(family("x-ab12c"), "x-*");
        assert_eq!(family("x"), "x");
    }
}
