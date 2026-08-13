#![no_main]

use asciline::codec::{CodecDecoder, MAX_DECOMPRESSED};
use asciline::profile::ProfileDecoder;
use asciline::protocol::{parse_ascf_header, ASCF_MAGIC_V2};
use libfuzzer_sys::fuzz_target;

// Mirrors `asciline-player`'s `.ascf` loop: header, then
// `[u32 BE len][record]`, dispatching mode 1 → text, tag 4 → profile decoder,
// else adaptive decoder. Must never panic on any byte string.
fn play_stream(data: &[u8]) {
    if data.len() < 14 {
        return;
    }
    let is_v2 = &data[..4] == ASCF_MAGIC_V2;
    let hdr_len = if is_v2 { 18 } else { 14 };
    if data.len() < hdr_len {
        return;
    }
    let Ok(header) = parse_ascf_header(&data[..hdr_len]) else {
        return;
    };
    let cell_bytes = if header.pixel { 3 } else { 4 };
    let mut adec = CodecDecoder::new(cell_bytes);
    let mut pdec = ProfileDecoder::new();
    let mut off = hdr_len;
    while off + 4 <= data.len() {
        let len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        off += 4;
        if len > MAX_DECOMPRESSED || off + len > data.len() {
            return; // the player bails on oversized/truncated records
        }
        let msg = &data[off..off + len];
        off += len;
        if header.mode == 1 {
            let _ = String::from_utf8_lossy(msg);
        } else if msg.len() >= 5 && msg[4] == 4 {
            let _ = pdec.decode(msg);
        } else {
            let _ = adec.decode(msg);
        }
    }
}

fuzz_target!(|data: &[u8]| {
    play_stream(data);
});
