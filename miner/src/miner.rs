
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