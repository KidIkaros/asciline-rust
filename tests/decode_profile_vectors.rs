//! Differential test: the ORIGINAL Python `ProfileEncoder` (tag 4, lossy DCT)
//! encodes synthetic frames; OUR Rust `ProfileDecoder` must reproduce the
//! Python "shown" BGR reconstruction byte-for-byte.
//!
//! 1. `python3 experiments/gen_profile_vectors.py > experiments/vectors_profile_py.bin`
//! 2. `cargo test --test decode_profile_vectors -- --ignored`
//!
//! Container (`PRFV`): `[4B magic][1B version][2B W][2B H][4B n]` then per
//! frame `[4B index][4B msg_len][msg][4B shown_len][shown]`.

use asciline::profile::ProfileDecoder;

const HEADER: usize = 13; // PRFV(4) + version(1) + W,H(4) + n(4)

fn u16at(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}
fn u32at(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[test]
#[ignore]
fn decode_profile_vectors() {
    let data = std::fs::read("experiments/vectors_profile_py.bin")
        .expect("run experiments/gen_profile_vectors.py first");
    let mut off = 0usize;
    let mut cases = 0usize;
    let mut frames = 0usize;

    while off + HEADER <= data.len() {
        assert_eq!(&data[off..off + 4], b"PRFV", "bad magic");
        assert_eq!(data[off + 4], 1, "bad vector version");
        let w = u16at(&data, off + 5) as usize;
        let h = u16at(&data, off + 7) as usize;
        let n = u32at(&data, off + 9) as usize;
        off += HEADER;

        let mut dec = ProfileDecoder::new();
        for _ in 0..n {
            let index = u32at(&data, off);
            off += 4;
            let mlen = u32at(&data, off) as usize;
            off += 4;
            let msg = &data[off..off + mlen];
            off += mlen;
            let slen = u32at(&data, off) as usize;
            off += 4;
            let shown = &data[off..off + slen];
            off += slen;

            let (idx, frame) = dec
                .decode(msg)
                .unwrap_or_else(|e| panic!("{w}x{h} frame {index}: {e}"));
            assert_eq!(idx, index, "frame index mismatch");
            assert_eq!(frame.len(), w * h * 3, "decoded size mismatch");
            assert_eq!(
                frame, shown,
                "decoded frame != python shown frame ({w}x{h} frame {index})"
            );
            frames += 1;
        }
        cases += 1;
    }
    eprintln!("OK: decoded {frames} Python profile-encoded frames across {cases} cases (bit-exact)");
}
