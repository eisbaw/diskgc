#![no_main]

use diskgc_linux::procfs::parse_environ;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    let parsed = parse_environ(input);
    let body = input.strip_suffix(b"\0").unwrap_or(input);
    let count = if body.is_empty() {
        0
    } else {
        body.split(|byte| *byte == 0).count()
    };
    assert_eq!(parsed.vars.len() + parsed.without_equals.len(), count);
});
