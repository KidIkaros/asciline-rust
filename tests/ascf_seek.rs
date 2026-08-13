//! Seek-index correctness (no wire change): the scan-on-open keyframe index
//! must find every forced keyframe in a real profile stream, and decoding
//! from the floor keyframe with a FRESH decoder must reproduce exactly the
//! same frame bytes as sequential playback from the start (the determinism
//! that makes arbitrary-frame jumps safe).
//!
//! Builds a real `.ascf` container with `ProfileEncoder` (240 frames of
//! synthetic motion, keyframes every 48), scans it with `AscfSeekIndex`,
//! then compares a jumped decode of frame 73 against the sequential one.

use asciline::codec::TAG_PROFILE;
use asciline::profile::{ProfileDecoder, ProfileEncoder};
use asciline::protocol::{parse_ascf_header, write_ascf_header, AscfHeader, AscfSeekIndex};

/// Deterministic synthetic BGR frame: LCG noise + a slowly moving blob (the
/// same generator the roundtrip test uses, so the stream has real inter-frame
/// motion that exercise the motion search between keyframes).
fn synth_bgr(w: usize, h: usize, i: u32, seed: u64) -> Vec<u8> {
    let mut state = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(i as u64);
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as u8
    };
    let mut f = vec![0u8; w * h * 3];
    for px in f.iter_mut() {
        *px = next();
    }
    let cx = (w / 2 + (i as usize * 4) % (w / 2).max(1)) as i32;
    let cy = (h / 2) as i32;
    let r = (h / 8).max(2) as i32;
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            if (x - cx) * (x - cx) + (y - cy) * (y - cy) <= r * r {
                let o = ((y as usize) * w + x as usize) * 3;
                f[o] = 0;
                f[o + 1] = 128;
                f[o + 2] = 255;
            }
        }
    }
    f
}

#[test]
fn seek_index_jump_reproduces_sequential_decode() {
    let (w, h, n) = (48usize, 32usize, 240u32);
    let mut enc = ProfileEncoder::new(w, h, 70);

    // Build the container: ASC2 header + length-prefixed records.
    let mut ascf: Vec<u8> = write_ascf_header(&AscfHeader {
        fps: 30.0,
        mode: 6,
        pixel: true,
        cols: w as u16,
        rows: h as u16,
        total_frames: n,
    });
    // Sequential reference: every frame decoded by a fresh decoder from start.
    let mut ref_dec = ProfileDecoder::new();
    let mut ref_frames: Vec<Vec<u8>> = Vec::new();

    for i in 0..n {
        let f = synth_bgr(w, h, i, 777);
        let (msg, _shown) = enc.encode(&f);
        assert_eq!(msg[4], TAG_PROFILE, "AQ off must emit tag 4");
        let (_, out) = ref_dec.decode(&msg).unwrap();
        ref_frames.push(out);

        ascf.extend_from_slice(&(msg.len() as u32).to_be_bytes());
        ascf.extend_from_slice(&msg);
    }

    // Scan the index exactly as the player/render tools do.
    let mut cursor = std::io::Cursor::new(&ascf);
    let mut reader = std::io::BufReader::new(&mut cursor);
    use std::io::Read;
    let mut hdr = [0u8; 18];
    reader.read_exact(&mut hdr).unwrap();
    let parsed = parse_ascf_header(&hdr).unwrap();
    assert_eq!(parsed.total_frames, n);
    let idx = AscfSeekIndex::scan(&mut reader, 18).unwrap();
    // 240 frames, keyframes at 0, 48, 96, 144, 192
    let kf_frames: Vec<u32> = idx.keyframes.iter().map(|&(f, _)| f).collect();
    assert_eq!(kf_frames, vec![0, 48, 96, 144, 192]);
    assert_eq!(idx.total_frames, 240);

    // Jump to an inter frame between keyframes and reproduce it exactly.
    let target = 73u32;
    let (kf, off) = idx.floor(target).expect("target must be covered");
    assert_eq!(kf, 48);
    assert!(off > 18, "keyframe offset must point past the header");

    let mut jump_cursor = std::io::Cursor::new(&ascf[off as usize..]);
    let mut jump_dec = ProfileDecoder::new(); // fresh decoder resyncs at keyframe
    let mut len_buf = [0u8; 4];
    let mut got: Option<Vec<u8>> = None;
    loop {
        if jump_cursor.read_exact(&mut len_buf).is_err() {
            break;
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut msg = vec![0u8; len];
        jump_cursor.read_exact(&mut msg).unwrap();
        let (fi, frame) = jump_dec.decode(&msg).unwrap();
        if fi == target {
            got = Some(frame);
            break;
        }
    }
    let got = got.expect("must decode the target frame after the jump");
    assert_eq!(
        got, ref_frames[target as usize],
        "seek jump to frame {target} must reproduce sequential playback exactly"
    );
}
