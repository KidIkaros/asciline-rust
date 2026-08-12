//! Differential test against the ORIGINAL Python encoder.
//!
//! 1. `python3 experiments/gen_python_vectors.py > experiments/vectors_python.bin`
//! 2. `cargo test --test decode_python_vectors -- --ignored`
//! 3. `node experiments/check_rust_vectors.js`
//!
//! Decodes every frame the Python `codec.py` encoded with OUR Rust decoder and
//! requires bit-exact equality with the Python "shown frame" (the lossless
//! plaintext). Then re-encodes the same framebuffers with OUR encoder and
//! verifies a self round-trip — and writes `experiments/vectors_rust.bin` so
//! the shipped `web/codec.js` can validate our encoder from the client side.

use std::io::Write;

use asciline::codec::{CodecDecoder, CodecEncoder, DEFAULT_LEVEL};

const HEADER: usize = 18; // RSTV(4) + version(1) + cell(1) + cols,rows,n(12)

fn u32at(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[test]
#[ignore]
fn decode_python_vectors() {
    let path = "experiments/vectors_python.bin";
    let data = std::fs::read(path).expect("run experiments/gen_python_vectors.py first");
    let mut off = 0usize;
    let mut rust_out: Vec<u8> = Vec::new();
    let mut cases = 0usize;
    let mut frames = 0usize;

    while off + HEADER <= data.len() {
        assert_eq!(&data[off..off + 4], b"RSTV", "bad magic");
        assert_eq!(data[off + 4], 1, "bad vector version");
        let cell = data[off + 5] as usize;
        let cols = u32at(&data, off + 6);
        let rows = u32at(&data, off + 10);
        let n = u32at(&data, off + 14) as usize;
        off += HEADER;

        let mut dec = CodecDecoder::new(cell);
        let mut enc = CodecEncoder::new(cell, DEFAULT_LEVEL, 0);

        // mirror the header so the Node script can walk the same file
        rust_out.extend_from_slice(b"RSTV");
        rust_out.push(1);
        rust_out.push(cell as u8);
        rust_out.extend_from_slice(&cols.to_be_bytes());
        rust_out.extend_from_slice(&rows.to_be_bytes());
        rust_out.extend_from_slice(&(n as u32).to_be_bytes());

        for _ in 0..n {
            let index = u32at(&data, off);
            off += 4;
            let mlen = u32at(&data, off) as usize;
            off += 4;
            let msg = &data[off..off + mlen];
            off += mlen;
            let plen = u32at(&data, off) as usize;
            off += 4;
            let plain = &data[off..off + plen];
            off += plen;

            // 1) our decoder must reproduce the Python encoder's output exactly
            let (idx, frame) = dec
                .decode(msg)
                .unwrap_or_else(|e| panic!("case {cols}x{rows} cell={cell} frame {index}: {e}"));
            assert_eq!(idx, index, "frame index mismatch");
            assert_eq!(
                frame, plain,
                "decoded frame != python plaintext (case {cols}x{rows} cell={cell} frame {index})"
            );

            // 2) our encoder on the same data must also round-trip
            let m = enc.encode(plain, index);
            let (_, f2) = dec.decode(&m).expect("rust self round-trip");
            assert_eq!(
                f2, plain,
                "rust encoder round-trip mismatch (frame {index})"
            );

            // stash our messages for the Node codec.js check
            rust_out.extend_from_slice(&index.to_be_bytes());
            rust_out.extend_from_slice(&(m.len() as u32).to_be_bytes());
            rust_out.extend_from_slice(&m);
            rust_out.extend_from_slice(&(plain.len() as u32).to_be_bytes());
            rust_out.extend_from_slice(plain);

            frames += 1;
        }
        cases += 1;
    }

    let mut f =
        std::fs::File::create("experiments/vectors_rust.bin").expect("create vectors_rust.bin");
    f.write_all(&rust_out).expect("write vectors_rust.bin");
    eprintln!(
        "OK: decoded {frames} Python-encoded frames across {cases} cases (bit-exact); rust vectors written"
    );
}
