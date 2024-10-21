#![feature(portable_simd)]

use std::simd::{ToBytes, u64x4, u8x32};
use std::thread::sleep;
use std::time::{Duration, Instant};

fn solve(data: &mut [u8]) {
    let step = size_of::<u64x4>();

    let mut last = 0u64;

    for i in (0..data.len()).step_by(step) {
        let chunk = u64x4::from_le_bytes(u8x32::from_slice(&data[i..i + step]));
        let mut modified_chunk = u64x4::from([0u64; 4]);

        for j in 0..4 {
            let x = chunk[j];

            modified_chunk[j] = x << 1 | x << 2 | last >> 63 | last >> 62;

            last = x;
        }

        data[i..i + step].copy_from_slice((chunk ^ modified_chunk).to_le_bytes().as_array());
    }
}

fn main() {
    let mut data = Vec::with_capacity(262144);

    data.push(1u8);

    for _ in 0..31 {
        data.push(0u8)
    }

    let now = Instant::now();

    for i in 1..1_000_000 {
        if i % 128 == 0 {
            for _ in 0..32 {
                data.push(0);
            }
        }

        solve(&mut data);
        println!("{data:?}");
        sleep(Duration::from_millis(500));
    }

    let elapsed = now.elapsed();

    println!("Elapsed: {elapsed:.2?}");
}
