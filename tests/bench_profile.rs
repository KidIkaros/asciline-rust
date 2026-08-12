//! Profile encoder micro-benchmark: isolates `ProfileEncoder::encode` from
//! ffmpeg decode, IO and the quality report, and measures the rayon
//! parallel-block speedup at 1 vs 12 threads.
//!
//! Run: `cargo test --release --test bench_profile -- --ignored --nocapture`

use std::time::Instant;

use asciline::profile::ProfileEncoder;
use asciline::quality::ssim;

fn bench(threads: usize, w: usize, h: usize, frames: usize, qf: u8) -> f64 {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap();
    let bgr: Vec<u8> = (0..w * h * 3)
        .map(|i| ((i as u32 * 2654435761) >> 24) as u8)
        .collect();
    // warmup
    pool.install(|| {
        let mut enc = ProfileEncoder::new(w, h, qf);
        for _ in 0..10u32 {
            enc.encode(&bgr);
        }
        let t0 = Instant::now();
        for _ in 0..frames as u32 {
            enc.encode(&bgr);
        }
        t0.elapsed().as_secs_f64() / frames as f64
    })
}

fn report(tag: &str, threads: usize, w: usize, h: usize, frames: usize, qf: u8) {
    let us = bench(threads, w, h, frames, qf) * 1e6;
    println!(
        "{tag:>6} {threads:>2}T {w:>4}x{h:<4} | {us:>8.1} µs/frame | {:>7.0} fps ceiling",
        1e6 / us,
    );
}

#[test]
#[ignore]
fn bench_profile_parallel() {
    // 480 cols → w=480, h=272 (16:9 @ 480 cols, padded to 16)
    report("qf70", 1, 480, 272, 200, 70);
    report("qf70", 12, 480, 272, 200, 70);
    // 240 cols
    report("qf70", 1, 240, 136, 400, 70);
    report("qf70", 12, 240, 136, 400, 70);
}

/// Isolate the SSIM cost (the dominant per-frame report expense) at 1 vs 12
/// threads. The blur inside must scale with cores.
#[test]
#[ignore]
fn bench_ssim_parallel() {
    let (w, h) = (480usize, 272usize);
    let a: Vec<u8> = (0..w * h)
        .map(|i| ((i as u32 * 2654435761) >> 24) as u8)
        .collect();
    let b: Vec<u8> = a.iter().map(|&v| v.wrapping_add(3)).collect();
    for threads in [1usize, 12] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        let us = pool.install(|| {
            for _ in 0..5 {
                ssim(&a, &b, w, h); // warmup
            }
            let t0 = Instant::now();
            for _ in 0..20 {
                ssim(&a, &b, w, h);
            }
            t0.elapsed().as_secs_f64() / 20.0 * 1e6
        });
        println!("ssim {w}x{h} {threads:>2}T | {us:>8.1} µs/call");
    }
}
