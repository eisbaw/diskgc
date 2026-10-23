#![no_main]

use diskgc_linux::mountinfo::parse_mountinfo;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    let _ = parse_mountinfo(input);
});
