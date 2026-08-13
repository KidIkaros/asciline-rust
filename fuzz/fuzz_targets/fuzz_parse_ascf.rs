#![no_main]

use asciline::protocol::parse_ascf_header;
use libfuzzer_sys::fuzz_target;

// The `.ascf` header parser: any byte prefix up to ~20 bytes must parse to a
// `Result`, never panic.
fuzz_target!(|data: &[u8]| {
    if data.len() > 32 {
        return;
    }
    let _ = parse_ascf_header(data);
});
