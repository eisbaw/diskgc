#![no_main]

use std::path::Path;

use diskgc_core::{config, profile};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    // Exercise both YAML entry points: an entire config and a profile list.
    // Static validation after parsing is part of the loader's contract.
    let _ = config::parse(Path::new("/fuzz/config.yaml"), input);
    if let Ok(text) = std::str::from_utf8(input)
        && let Ok(profiles) = profile::parse_profiles(text)
    {
        let _ = profile::Library::with_user_profiles(&profiles);
    }
});
