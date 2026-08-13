#![no_main]

use asciline::codec::CodecDecoder;
use libfuzzer_sys::fuzz_target;

// The adaptive decoder (tags 0-3: RAW/ZLIB/DELTA/RLE_FULL) against arbitrary
// wire messages, in both cell sizes (4 = ASCII colour, 3 = pixel). A panic
// here is a crash `asciline-player` could hit on a crafted `.ascf` file.
fuzz_target!(|data: &[u8]| {
    let mut dec4 = CodecDecoder::new(4);
    let _ = dec4.decode(data);
    let mut dec3 = CodecDecoder::new(3);
    let _ = dec3.decode(data);
});
