//! dHash 感知哈希:9x8 灰度梯度 64bit,Hamming ≤ 2 视为近似重复。
use image::DynamicImage;

pub fn dhash_u64(img: &DynamicImage) -> u64 {
    let gray = image::imageops::resize(
        &img.to_luma8(),
        9,
        8,
        image::imageops::FilterType::Triangle,
    );
    let mut hash = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            let left = gray.get_pixel(x, y).0[0] as i32;
            let right = gray.get_pixel(x + 1, y).0[0] as i32;
            if left > right {
                hash |= 1u64 << (y * 8 + x);
            }
        }
    }
    hash
}

/// 近似重复判定:hamming 距离 ≤ 阈值(默认 10,见 timeline::dup_groups_near)。
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}
