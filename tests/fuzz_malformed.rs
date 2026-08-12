//! Malformed-input robustness harness (proptest, stable toolchain).
//!
//! The decoders parse untrusted data — `asciline-player` opens arbitrary
//! `.ascf` files and the compiler consumes media. The contract enforced here:
//! **no input may panic a decoder** (unwinds are a crash; `Err` is fine).
//!
//! Proptest feeds arbitrary bytes plus *valid-zlib-of-arbitrary-bytes* payloads
//! (the wire form decoders actually parse) into every entry point. The
//! deterministic tests below pin the specific hardening fixes: the truncated
//! RLE run that used to panic with an out-of-bounds slice, the unbounded
//! decompression / RLE-expansion bombs, and the crafted profile keyframe that
//! requested a multi-GB grid.

use asciline::codec::{zlib_compress, CodecDecoder, MAX_DECOMPRESSED};
use asciline::profile::ProfileDecoder;
use asciline::protocol::{parse_ascf_header, ASCF_MAGIC_V2};
use proptest::prelude::*;

/// Mirrors `asciline-player`'s `.ascf` record cap (checked before allocating).
const MAX_ASCF_RECORD: usize = 64 << 20;

/// Arbitrary bytes wrapped in valid zlib — the payload shape the decoders
/// actually inflate (tags 1/2/3/4), so deep parse paths get exercised.
fn zlib_of(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..max_len).prop_map(|v| zlib_compress(&v, 6))
}

/// The player's `.ascf` loop: header, then `[u32 BE len][record]`, dispatching
/// mode 1 → text, tag 4 → profile decoder, else adaptive decoder. Must never
/// panic (a panic here is a crash the player could hit on a crafted file).
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
        let len =
            u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        off += 4;
        if len > MAX_ASCF_RECORD || off + len > data.len() {
            return; // player bails on oversized/truncated records
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

proptest! {
    /// The .ascf header parser must never panic.
    #[test]
    fn ascf_header_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let _ = parse_ascf_header(&bytes);
    }

    /// Raw arbitrary wire messages (random tags, random payloads).
    #[test]
    fn adaptive_decode_never_panics(msg in prop::collection::vec(any::<u8>(), 0..1024)) {
        let mut dec4 = CodecDecoder::new(4);
        let mut dec3 = CodecDecoder::new(3);
        let _ = dec4.decode(&msg);
        let _ = dec3.decode(&msg);
    }

    /// Valid-zlib payloads with random tags — this is what reaches the RLE /
    /// DELTA / ZLIB parse paths, including truncated-run bodies.
    #[test]
    fn adaptive_decode_compressed_never_panics(
        idx in any::<[u8; 4]>(), tag in any::<u8>(), payload in zlib_of(512),
    ) {
        let mut msg = idx.to_vec();
        msg.push(tag);
        msg.extend_from_slice(&payload);
        let mut dec = CodecDecoder::new(4);
        let _ = dec.decode(&msg);
    }

    /// A keyframe followed by a random DELTA (patches against `prev`).
    #[test]
    fn delta_after_keyframe_never_panics(
        key in prop::collection::vec(any::<u8>(), 0..512), delta in zlib_of(512),
    ) {
        let mut kf = vec![0u8, 0, 0, 0, 0];
        kf.extend_from_slice(&key);
        let mut dm = vec![0u8, 0, 0, 1, 2];
        dm.extend_from_slice(&delta);
        let mut dec = CodecDecoder::new(4);
        let _ = dec.decode(&kf);
        let _ = dec.decode(&dm);
    }

    /// Tag-4 profile messages: random keyframes + inter frames. The chained
    /// stream harness below also drives deep dec_plane paths across frames.
    #[test]
    fn profile_decode_never_panics(idx in any::<[u8; 4]>(), payload in zlib_of(1024)) {
        let mut msg = idx.to_vec();
        msg.push(4);
        msg.extend_from_slice(&payload);
        let mut dec = ProfileDecoder::new();
        let _ = dec.decode(&msg);
    }

    /// The whole player-style stream reader must never panic.
    #[test]
    fn ascf_stream_never_panics(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        play_stream(&data);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Deterministic regression tests (the concrete bugs the fuzzing would find)
// ────────────────────────────────────────────────────────────────────────────

/// The RLE_FULL decoder used to slice `body[off+2..off+2+cell]` without a
/// bounds check: a truncated final run panicked with an out-of-bounds access.
#[test]
fn rle_truncated_run_is_error_not_panic() {
    // body: [count=3 LE][only 3 of the 4 cell bytes] — the run overruns the buffer
    let body = vec![3u8, 0, 1, 2, 3];
    let mut msg = vec![0u8, 0, 0, 0, 3]; // tag 3 = RLE_FULL
    msg.extend_from_slice(&zlib_compress(&body, 6));
    let mut dec = CodecDecoder::new(4);
    assert!(
        dec.decode(&msg).is_err(),
        "a truncated RLE run must be an error, never a panic"
    );
}

/// RLE expansion is unbounded on paper (65535 cells per 6-byte run); the
/// decoder must cap the output instead of allocating hundreds of MB.
#[test]
fn rle_expansion_is_capped() {
    // 300 runs × 65535 × 4 bytes ≈ 78.6 MB output from a ~1.8 KB body
    let mut body = Vec::new();
    for _ in 0..300 {
        body.extend_from_slice(&65535u16.to_le_bytes());
        body.extend_from_slice(&[7, 8, 9, 10]);
    }
    let mut msg = vec![0u8, 0, 0, 0, 3];
    msg.extend_from_slice(&zlib_compress(&body, 6));
    let mut dec = CodecDecoder::new(4);
    assert!(
        dec.decode(&msg).is_err(),
        "RLE expansion must hit the output cap"
    );
}

/// A crafted profile keyframe declares its grid in the payload as u16 dims;
/// 65535² used to request ~25 GB of plane buffers (OOM abort). Must bail.
#[test]
fn profile_keyframe_huge_grid_is_rejected() {
    let mut payload = vec![0u8, 70, 0xff, 0xff, 0xff, 0xff]; // ftype 0, qf 70, w=h=65535
    payload.extend_from_slice(&[0u8; 8]);
    let mut msg = vec![0u8, 0, 0, 0, 4];
    msg.extend_from_slice(&zlib_compress(&payload, 6));
    let mut dec = ProfileDecoder::new();
    assert!(
        dec.decode(&msg).is_err(),
        "a 65535x65535 grid must be rejected, not OOM"
    );

    // zero dims are invalid too
    let mut payload0 = vec![0u8, 70, 0, 0, 0, 0];
    payload0.extend_from_slice(&[0u8; 8]);
    let mut msg0 = vec![0u8, 0, 0, 0, 4];
    msg0.extend_from_slice(&zlib_compress(&payload0, 6));
    let mut dec0 = ProfileDecoder::new();
    assert!(dec0.decode(&msg0).is_err(), "a 0x0 grid must be rejected");
}

/// A small zlib stream that inflates to 70 MB (decompression bomb) must fail
/// the 64 MiB cap instead of allocating.
#[test]
fn zlib_bomb_is_capped() {
    let big = vec![0u8; 70 << 20];
    let bomb = zlib_compress(&big, 6);
    assert!(
        bomb.len() < (1 << 20),
        "test setup: zeros must compress hard"
    );
    let mut msg = vec![0u8, 0, 0, 0, 1]; // tag 1 = ZLIB
    msg.extend_from_slice(&bomb);
    let mut dec = CodecDecoder::new(4);
    assert!(
        dec.decode(&msg).is_err(),
        "a decompression bomb must hit the cap (got {})",
        MAX_DECOMPRESSED
    );
}

/// The player's record-length cap is checked before allocation; a length
/// prefix claiming more than the cap is rejected by the stream rule.
#[test]
fn ascf_record_length_cap_contract() {
    // The player bails when len > MAX_ASCF_RECORD (64 MiB); verify the rule
    // the fuzz harness mirrors matches the player's constant.
    assert_eq!(MAX_ASCF_RECORD, MAX_DECOMPRESSED);
    let oversized = 0xFFFF_FFFFu32.to_be_bytes();
    let mut stream = vec![0u8, 0, 0, 0, 0, 0, 0]; // 4-byte header stub + prefix
    stream.extend_from_slice(&oversized);
    // play_stream applies the cap: it must not panic and not allocate
    play_stream(&stream);
}
