//! Round-trip test for the Rust tag-4 profile encoder/decoder (and the tag-5
//! adaptive-quantization, tag-6 half-pixel and tag-7 quarter-pixel variants).
//!
//! 1. Encodes synthetic BGR frames with OUR `ProfileEncoder`, decodes each
//!    message with OUR `ProfileDecoder` and requires the decoder to reproduce
//!    the encoder's own "shown" BGR reconstruction exactly.
//! 2. Writes `experiments/vectors_profile_rust.bin` (tag 4),
//!    `experiments/vectors_profile_aq_rust.bin` (tag 5, AQ levels 2 and 4),
//!    `experiments/vectors_profile_hpel_rust.bin` (tag 6, half-pel motion,
//!    plain and with AQ) and `experiments/vectors_profile_qpel_rust.bin`
//!    (tag 7, quarter-pel motion, plain and with AQ), all `PRFV` containers,
//!    so the shipped browser decoder (`web/codec.js` `makeProfileDecoder`)
//!    can be checked against our encoder's bitstream:
//!    `node experiments/check_profile_vectors.js`
//!    `node experiments/check_profile_aq_vectors.js`
//!    `node experiments/check_profile_hpel_vectors.js`
//!    `node experiments/check_profile_qpel_vectors.js`

use std::io::Write;

use asciline::profile::{ProfileDecoder, ProfileEncoder};

/// Deterministic synthetic BGR frame: LCG noise + a moving bright blob.
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
#[ignore]
fn roundtrip_profile() {
    let mut out: Vec<u8> = Vec::new();
    let mut cases = 0usize;
    let mut frames = 0usize;

    for (w, h, n, qf) in [
        (48usize, 32usize, 60u32, 70u8),
        (64, 48, 30, 50),
        (32, 16, 12, 90),
    ] {
        let mut enc = ProfileEncoder::new(w, h, qf);
        let mut dec = ProfileDecoder::new();

        out.extend_from_slice(b"PRFV");
        out.push(1);
        out.extend_from_slice(&(w as u16).to_be_bytes());
        out.extend_from_slice(&(h as u16).to_be_bytes());
        out.extend_from_slice(&n.to_be_bytes());

        for i in 0..n {
            let f = synth_bgr(w, h, i, 777);
            let (msg, shown) = enc.encode(&f);
            let (idx, out_frame) = dec
                .decode(&msg)
                .unwrap_or_else(|e| panic!("{w}x{h} frame {i}: {e}"));
            assert_eq!(idx, i);
            assert_eq!(
                out_frame, shown,
                "decoder != encoder shown ({w}x{h} frame {i})"
            );

            // stash for the Node codec.js check
            out.extend_from_slice(&i.to_be_bytes());
            out.extend_from_slice(&(msg.len() as u32).to_be_bytes());
            out.extend_from_slice(&msg);
            out.extend_from_slice(&(shown.len() as u32).to_be_bytes());
            out.extend_from_slice(&shown);

            frames += 1;
        }
        cases += 1;
    }

    let mut f = std::fs::File::create("experiments/vectors_profile_rust.bin").expect("create file");
    f.write_all(&out).expect("write vectors");
    eprintln!(
        "OK: {frames} profile frames round-tripped across {cases} cases; rust vectors written"
    );

    // Tag-5 AQ vectors: same PRFV container, messages tagged 5. codec.js must
    // decode these bit-exactly too (see experiments/check_profile_aq_vectors.js).
    let mut aq_out: Vec<u8> = Vec::new();
    let mut aq_cases = 0usize;
    let mut aq_frames = 0usize;
    for (w, h, n, qf, levels) in [
        (48usize, 32usize, 40u32, 70u8, 2u8),
        (64, 48, 20, 50, 4),
        (32, 16, 10, 90, 4),
    ] {
        let mut enc = ProfileEncoder::new(w, h, qf);
        enc.aq_levels = levels;
        enc.scene_cut_mad = 20.0; // force at least one keyframe mid-stream
        let mut dec = ProfileDecoder::new();

        aq_out.extend_from_slice(b"PRFV");
        aq_out.push(1);
        aq_out.extend_from_slice(&(w as u16).to_be_bytes());
        aq_out.extend_from_slice(&(h as u16).to_be_bytes());
        aq_out.extend_from_slice(&n.to_be_bytes());

        for i in 0..n {
            let f = if i == n / 2 {
                // hard scene change → forced keyframe inside the stream
                synth_bgr(w, h, i + 999, 4242)
            } else {
                synth_bgr(w, h, i, 777)
            };
            let (msg, shown) = enc.encode(&f);
            assert_eq!(msg[4], 5, "AQ encoder must emit tag 5 ({w}x{h} frame {i})");
            let (idx, out_frame) = dec
                .decode(&msg)
                .unwrap_or_else(|e| panic!("AQ {w}x{h} frame {i}: {e}"));
            assert_eq!(idx, i);
            assert_eq!(
                out_frame, shown,
                "AQ decoder != encoder shown ({w}x{h} frame {i})"
            );

            aq_out.extend_from_slice(&i.to_be_bytes());
            aq_out.extend_from_slice(&(msg.len() as u32).to_be_bytes());
            aq_out.extend_from_slice(&msg);
            aq_out.extend_from_slice(&(shown.len() as u32).to_be_bytes());
            aq_out.extend_from_slice(&shown);
            aq_frames += 1;
        }
        aq_cases += 1;
    }
    let mut aqf =
        std::fs::File::create("experiments/vectors_profile_aq_rust.bin").expect("create file");
    aqf.write_all(&aq_out).expect("write aq vectors");
    eprintln!(
        "OK: {aq_frames} AQ frames round-tripped across {aq_cases} cases; aq rust vectors written"
    );

    // Tag-6 half-pel vectors: same PRFV container, messages tagged 6, half-pel
    // motion on (plain and combined with AQ). codec.js must decode these
    // bit-exactly too (see experiments/check_profile_hpel_vectors.js).
    let mut hp_out: Vec<u8> = Vec::new();
    let mut hp_cases = 0usize;
    let mut hp_frames = 0usize;
    for (w, h, n, qf, levels) in [
        (48usize, 32usize, 50u32, 70u8, 0u8),
        (64, 48, 30, 50, 2),
        (32, 16, 15, 90, 4),
    ] {
        let mut enc = ProfileEncoder::new(w, h, qf);
        enc.hpel = true;
        enc.aq_levels = levels;
        enc.scene_cut_mad = 20.0; // force at least one keyframe mid-stream
        let mut dec = ProfileDecoder::new();

        hp_out.extend_from_slice(b"PRFV");
        hp_out.push(1);
        hp_out.extend_from_slice(&(w as u16).to_be_bytes());
        hp_out.extend_from_slice(&(h as u16).to_be_bytes());
        hp_out.extend_from_slice(&n.to_be_bytes());

        for i in 0..n {
            let f = if i == n / 2 {
                // hard scene change → forced keyframe inside the stream
                synth_bgr(w, h, i + 999, 4242)
            } else {
                synth_bgr(w, h, i, 777)
            };
            let (msg, shown) = enc.encode(&f);
            assert_eq!(
                msg[4], 6,
                "hpel encoder must emit tag 6 ({w}x{h} frame {i})"
            );
            let (idx, out_frame) = dec
                .decode(&msg)
                .unwrap_or_else(|e| panic!("HPEL {w}x{h} frame {i}: {e}"));
            assert_eq!(idx, i);
            assert_eq!(
                out_frame, shown,
                "HPEL decoder != encoder shown ({w}x{h} frame {i})"
            );

            hp_out.extend_from_slice(&i.to_be_bytes());
            hp_out.extend_from_slice(&(msg.len() as u32).to_be_bytes());
            hp_out.extend_from_slice(&msg);
            hp_out.extend_from_slice(&(shown.len() as u32).to_be_bytes());
            hp_out.extend_from_slice(&shown);
            hp_frames += 1;
        }
        hp_cases += 1;
    }
    let mut hpf =
        std::fs::File::create("experiments/vectors_profile_hpel_rust.bin").expect("create file");
    hpf.write_all(&hp_out).expect("write hpel vectors");
    eprintln!(
        "OK: {hp_frames} HPEL frames round-tripped across {hp_cases} cases; hpel rust vectors written"
    );

    // Tag-7 quarter-pel vectors: same PRFV container, messages tagged 7,
    // quarter-pel motion on (plain and combined with AQ). codec.js must decode
    // these bit-exactly too (see experiments/check_profile_qpel_vectors.js).
    let mut qp_out: Vec<u8> = Vec::new();
    let mut qp_cases = 0usize;
    let mut qp_frames = 0usize;
    for (w, h, n, qf, levels) in [
        (48usize, 32usize, 50u32, 70u8, 0u8),
        (64, 48, 30, 50, 2),
        (32, 16, 15, 90, 4),
    ] {
        let mut enc = ProfileEncoder::new(w, h, qf);
        enc.qpel = true;
        enc.aq_levels = levels;
        enc.scene_cut_mad = 20.0; // force at least one keyframe mid-stream
        let mut dec = ProfileDecoder::new();

        qp_out.extend_from_slice(b"PRFV");
        qp_out.push(1);
        qp_out.extend_from_slice(&(w as u16).to_be_bytes());
        qp_out.extend_from_slice(&(h as u16).to_be_bytes());
        qp_out.extend_from_slice(&n.to_be_bytes());

        for i in 0..n {
            let f = if i == n / 2 {
                // hard scene change → forced keyframe inside the stream
                synth_bgr(w, h, i + 999, 4242)
            } else {
                synth_bgr(w, h, i, 777)
            };
            let (msg, shown) = enc.encode(&f);
            assert_eq!(
                msg[4], 7,
                "qpel encoder must emit tag 7 ({w}x{h} frame {i})"
            );
            let (idx, out_frame) = dec
                .decode(&msg)
                .unwrap_or_else(|e| panic!("QPEL {w}x{h} frame {i}: {e}"));
            assert_eq!(idx, i);
            assert_eq!(
                out_frame, shown,
                "QPEL decoder != encoder shown ({w}x{h} frame {i})"
            );

            qp_out.extend_from_slice(&i.to_be_bytes());
            qp_out.extend_from_slice(&(msg.len() as u32).to_be_bytes());
            qp_out.extend_from_slice(&msg);
            qp_out.extend_from_slice(&(shown.len() as u32).to_be_bytes());
            qp_out.extend_from_slice(&shown);
            qp_frames += 1;
        }
        qp_cases += 1;
    }
    let mut qpf =
        std::fs::File::create("experiments/vectors_profile_qpel_rust.bin").expect("create file");
    qpf.write_all(&qp_out).expect("write qpel vectors");
    eprintln!(
        "OK: {qp_frames} QPEL frames round-tripped across {qp_cases} cases; qpel rust vectors written"
    );
}
