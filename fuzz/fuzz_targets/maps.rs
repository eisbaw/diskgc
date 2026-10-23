#![no_main]

use diskgc_linux::procfs::parse_maps;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    let _ = parse_maps(input);
});
