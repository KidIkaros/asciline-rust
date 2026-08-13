#![no_main]

use asciline::profile::ProfileDecoder;
use libfuzzer_sys::fuzz_target;

// The tag-4 lossy DCT profile decoder against arbitrary messages. The wire
// payload is zlib-compressed, so libFuzzer rarely produces valid zlib on its
// own; the corpus seeds (see `fuzz/corpus`) carry real profile frames that
// the mutator then perturbs into the deep `dec_plane` paths.
fuzz_target!(|data: &[u8]| {
    let mut dec = ProfileDecoder::new();
    let _ = dec.decode(data);
});
