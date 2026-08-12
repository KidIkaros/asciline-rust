//! Throughput benchmark for the map + adaptive-encode stage.
//!
//! Run: `cargo test --release --test bench_encode -- --ignored --nocapture`

use std::time::Instant;

use asciline::codec::{CodecEncoder, DEFAULT_LEVEL};
use asciline::mapper::Mapper;

fn bench(cols: usize, rows: usize, frames: usize, tolerance: u32) {
    let rgb: Vec<u8> = (0..cols * rows * 3)
        .map(|i| ((i as u32 * 2654435761) >> 24) as u8)
        .collect();
    let mapper = Mapper::default(0);
    let mut enc = CodecEncoder::new(4, DEFAULT_LEVEL, tolerance);
    let mut fb = vec![0u8; cols * rows * 4];

    // warmup
    for i in 0..10 {
        mapper.map_ascii(&rgb, cols, rows, &mut fb);
        enc.encode(&fb, i as u32);
    }

    let t0 = Instant::now();
    let mut bytes = 0usize;
    for i in 0..frames {
        mapper.map_ascii(&rgb, cols, rows, &mut fb);
        let msg = enc.encode(&fb, i as u32);
        bytes += msg.len();
    }
    let per_frame = t0.elapsed().as_secs_f64() / frames as f64;
    println!(
        "{:>4}x{:<4} {:>6} cells | {:>7.1} µs/frame (map+encode) | {:>7.0} fps ceiling | {:.1} KB/frame",
        cols,
        rows,
        cols * rows,
        per_frame * 1e6,
        1.0 / per_frame,
        bytes as f64 / frames as f64 / 1024.0
    );
}

#[test]
#[ignore]
fn bench_map_encode() {
    bench(80, 23, 1000, 0);
    bench(200, 56, 1000, 0);
    bench(240, 67, 1000, 0);
    bench(480, 135, 500, 0);
    bench(560, 315, 200, 0); // heavy pixel-ish grid
}
