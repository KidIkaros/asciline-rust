//! Round-trip test for the Rust tag-4 profile encoder/decoder.
//!
//! 1. Encodes synthetic BGR frames with OUR `ProfileEncoder`, decodes each
//!    message with OUR `ProfileDecoder` and requires the decoder to reproduce
//!    the encoder's own "shown" reconstruction exactly.
//! 2. Writes `experiments/vectors_profile_rust.bin` (`PRFV` container) so the
//!    shipped browser decoder (`web/codec.js` `makeProfileDecoder`) can be
//!    checked against our encoder's bitstream:
//!    `node experiments/check_profile_vectors.js`

use std::io::Write;

use asciline::profile::{ProfileDecoder, ProfileEncoder};

/// Deterministic synthetic BGR frame: LCG noise + a moving bright blob.
fn synth_bgr(w: usize, h: usize, i: u32, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(i as u64);
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

    for (w, h, n, qf) in [(48usize, 32usize, 60u32, 70u8), (64, 48, 30, 50), (32, 16, 12, 90)] {
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
            let (idx, out_frame) = dec.decode(&msg).unwrap_or_else(|e| panic!("{w}x{h} frame {i}: {e}"));
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
    eprintln!("OK: {frames} profile frames round-tripped across {cases} cases; rust vectors written");
}
