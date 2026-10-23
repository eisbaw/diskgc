#![no_main]

use diskgc_core::select;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    if let Ok(text) = std::str::from_utf8(input)
        && let Ok(expression) = select::parse(text)
    {
        // The printed, distributed AST is itself a valid selection expression.
        let printed = expression.to_string();
        assert_eq!(select::parse(&printed).ok(), Some(expression));
    }
});
