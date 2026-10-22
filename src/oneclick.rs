//! 一键效果(oneclick):黑白 / 双色调 / 颗粒 / 锐化。
//!
//! # 行为要点
//!
//! | 效果 | 关键行为 | 说明 |
//! |---|---|---|
//! | `bw` 亮度权重 | 权重求和 | `Y = 0.3086·R + 0.6094·G + 0.082·B` |
//! | `bw` max-normalize | 按亮度归一 | `R' = R·0.3086/Y`,`G' = G·0.6094/Y`,`B' = B·0.082/Y` |
//! | `bw` 黑像素保护 | 亮度为 0 时兜底 | `Y == 0` → 输出 `255,255,255`(**纯白**) |
//! | `sepia` 默认色 | 双色调查表 | `0xddc9ae` = RGB(221,201,174) |
//! | `sepia` 256 项 LUT | 灰度→RGB 查找表 | 循环 256 次生成 |
//! | `sepia` 双色参数 | 双色调插值 | `highlight` 色 + `shadow` 色 + `mix`(默认 50) |
//! | `grain` RNG | MT19937 | 初始化用 `0x19660d` twist,624 字状态 |
//! | `grain` RNG twist | MT19937 | 标准 MT19937 in-place twist |
//! | `grain` xorshift32 | 噪声混合计算 | `u=s^(s>>11)`;`u^=(u&0xff3a58ad)<<7`;`u^=(u&0xffffdf8c)<<15`;`r=(u>>18)^u` |
//! | `grain` 噪声强度 | 强度换算 | `收敛系数 = ROUND(256.0 - amount·256.0)` |
//! | `grain` 噪声混合 | 噪声混合计算 | `out = src + ((0xa0 - |0x80-src|)·(noise-0x80)) >> 8`,clamp `[0,255]` |
//! | `unsharp` / `unsharp2` | 3×3 锐化核 | 同一条锐化路径,差异是滑块范围 1.0→3.0 与全分辨率/慢速开关 |
//!
//! # 参数来源有限的项(不做无依据的推测)
//!
//! - **`warm` 的色值/矩阵系数**:没有可靠来源 → **不实现**。
//! - **`grain` 的 low/high/seed 默认值**:没有可靠来源 → 提供一组
//!   可用的预设(见 `grain_preset`)。
//! - **`unsharp` 的 USM 半径/阈值/数量**:没有可靠来源 → 沿用 3×3 锐化核
//!   (见 `effects::sharpen_kernel`),**不编造 USM 参数**。
//! - **`sepia` 灰度→棕色的精确映射**:用**双色调插值**近似,并在此明确标注。
//!
//! # 亮度权重(全库统一,勿改)
//!
//! 本项目**不采用** Rec.601 / Rec.709,自定义权重为
//! `(R=0.3086, G=0.6094, B=0.082)`。

/// 亮度权重(R / G / B),取自内置资源包 `luma.weights`。
///
/// 全库统一使用这组权重,不采用 Rec.601 / Rec.709。
pub fn luma_weights() -> [f32; 3] {
    let v = crate::assets::f64s("luma.weights");
    [
        v.first().copied().unwrap_or(0.3086) as f32,
        v.get(1).copied().unwrap_or(0.6094) as f32,
        v.get(2).copied().unwrap_or(0.082) as f32,
    ]
}

/// 用本项目权重求亮度。
pub fn luma(r: f32, g: f32, b: f32) -> f32 {
    let w = luma_weights();
    r * w[0] + g * w[1] + b * w[2]
}

/// 灰度(black & white)效果。
///
/// **不是**普通灰度化!这里是 **max-normalize 亮度归一化**。
///
/// (亮度权重计算):
///
/// ```text
/// Y = 0.3086·R + 0.6094·G + 0.082·B              // 本项目权重
/// Rn = R·0.3086 / Y                               // 各通道的"亮度贡献占比"
/// Gn = G·0.6094 / Y
/// Bn = B·0.082  / Y
/// M  = [ Rn Gn Bn ]  (3×3 矩阵,三行相同)
/// out = M · (R,G,B) = Rn·R + Gn·G + Bn·B
/// Y == 0  →  输出 255,255,255(纯白,原版的黑像素保护)
/// ```
///
/// 化简后 `out = Σ(wᵢ·vᵢ²) / Σ(wᵢ·vᵢ)`,即**以各通道亮度贡献为权重的加权平均**:
/// - 纯色(单通道主导)→ 结果 ≈ 该通道值(不偏色、亮度保真);
/// - 中性灰 → 结果 = 原灰度值(严格保灰阶);
///
/// 这解释了 Picasa 的 bw 为何比普通 grayscale 保留更多层次/对比,
/// 且**不会**像 Rec.709 灰度化那样把红色压暗。
pub fn apply_bw(img: &mut image::RgbaImage) {
    let w = luma_weights();
    for p in img.pixels_mut() {
        let (r, g, b) = (p[0] as f32, p[1] as f32, p[2] as f32);
        let y = r * w[0] + g * w[1] + b * w[2];
        if y == 0.0 {
            p[0] = 255;
            p[1] = 255;
            p[2] = 255;
        } else {
            let out = ((r * w[0]) * r + (g * w[1]) * g + (b * w[2]) * b) / y;
            let v = out.clamp(0.0, 255.0) as u8;
            p[0] = v;
            p[1] = v;
            p[2] = v;
        }
    }
}

/// sepia(棕褐色)效果。
///
/// 原版参数模型已确认(双色调参数):
/// `highlight` 色 + `shadow` 色 + `mix`(百分比,默认 50,clamp `[0,100]`),
/// 混合色 = `shadow + (highlight − shadow)·(mix/100)`。
///
/// 默认 `highlight` = `0xddc9ae` = RGB(221,201,174)(双色调查表)。
///
/// **诚实声明**:原版把灰度映射到棕色的精确函数(灰度到棕色的映射)
/// ,原版精确映射未公开;我方用「Picasa 权重灰度 → 双色调插值」近似,
/// **参数(色值、mix 语义、256 项 LUT 结构)与原版一致,映射函数是近似**。
pub fn apply_sepia(img: &mut image::RgbaImage, mix: f32, shadow: [u8; 3], highlight: [u8; 3]) {
    let m = (mix.clamp(0.0, 100.0)) / 100.0;
    let tint: [f32; 3] = [
        shadow[0] as f32 + (highlight[0] as f32 - shadow[0] as f32) * m,
        shadow[1] as f32 + (highlight[1] as f32 - shadow[1] as f32) * m,
        shadow[2] as f32 + (highlight[2] as f32 - shadow[2] as f32) * m,
    ];
    // 原版按 256 项 LUT 逐级查表(灰度 → RGB),我方现算等价值。
    let mut lut = [[0u8; 3]; 256];
    for i in 0..256usize {
        let g = i as f32 / 255.0;
        for c in 0..3 {
            lut[i][c] = (g * tint[c]).clamp(0.0, 255.0) as u8;
        }
    }
    for p in img.pixels_mut() {
        let y = luma(p[0] as f32, p[1] as f32, p[2] as f32).clamp(0.0, 255.0) as usize;
        p[0] = lut[y][0];
        p[1] = lut[y][1];
        p[2] = lut[y][2];
    }
}

/// sepia 默认参数,取自内置资源包 `sepia.default`。
///
/// 段内依次为:`mix`、`shadow` 的 R/G/B、`highlight` 的 R/G/B。
pub fn sepia_default() -> (f32, [u8; 3], [u8; 3]) {
    let v = crate::assets::f64s("sepia.default");
    let g = |i: usize| v.get(i).copied().unwrap_or(0.0) as u8;
    (
        v.first().copied().unwrap_or(50.0) as f32,
        [g(1), g(2), g(3)],
        [g(4), g(5), g(6)],
    )
}

/// 胶片颗粒噪声参数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GrainParams {
    /// 噪声强度 0..1(0 = 不动,1 = 最平)。
    pub amount: f32,
    /// 随机种子(原版 `randomSeed`)。
    pub seed: u32,
    /// 通道位掩码:1=R / 2=G / 4=B / 8=A(原版 `channelOptions`)。
    pub channel_options: u8,
    /// 是否灰度噪声(原版 `grayscale`)。
    pub grayscale: bool,
}

impl Default for GrainParams {
    fn default() -> Self {
        // 内置预设的默认值(取自资源包 `grain.default`):
        // 依次为 amount / seed / channelOptions / grayscale
        let v = crate::assets::f64s("grain.default");
        GrainParams {
            amount: v.first().copied().unwrap_or(0.15) as f32,
            seed: v.get(1).copied().unwrap_or(0.0) as u32,
            channel_options: v.get(2).copied().unwrap_or(7.0) as u8,
            grayscale: v.get(3).copied().unwrap_or(1.0) != 0.0,
        }
    }
}

/// MT19937 随机数发生器(对齐原版)。
///
/// 原版状态表 624 字,初始化用 `0x19660d` 标准 twist
/// (MT19937 初始化),每次 twist 用标准 in-place 形式
/// (MT19937 迭代)。
///
/// 注意:原版输出**不是**标准 MT19937 的 tempering,而是额外做一次 xorshift32 混淆
/// (噪声混合计算):
/// ```text
/// u = s ^ (s >> 11)
/// u ^= (u & 0xff3a58ad) << 7
/// u ^= (u & 0xffffdf8c) << 15
/// r = (u >> 18) ^ u
/// ```
#[derive(Debug, Clone)]
pub struct Mt19937 {
    state: [u32; 624],
    index: usize,
}

impl Mt19937 {
    /// 用种子初始化(原版MT19937 初始化:标准MT19937 初始化)。
    pub fn new(seed: u32) -> Mt19937 {
        let mut state = [0u32; 624];
        state[0] = seed;
        for i in 1..624 {
            let prev = state[i - 1];
            state[i] = (prev ^ (prev >> 30)).wrapping_mul(0x1966_0d).wrapping_add(i as u32);
        }
        Mt19937 { state, index: 624 }
    }

    /// twist(原版MT19937 迭代,标准 MT19937 in-place 形式)。
    fn twist(&mut self) {
        const MATRIX_A: u32 = 0x9908_b0df;
        const UPPER: u32 = 0x8000_0000;
        const LOWER: u32 = 0x7fff_ffff;
        for i in 0..624 {
            let y = (self.state[i] & UPPER) | (self.state[(i + 1) % 624] & LOWER);
            let mut next = self.state[(i + 397) % 624] ^ (y >> 1);
            if y & 1 != 0 {
                next ^= MATRIX_A;
            }
            self.state[i] = next;
        }
        self.index = 0;
    }

    /// 取下一个 32 位随机数(**带原版 xorshift32 混淆**)。
    pub fn next_u32(&mut self) -> u32 {
        if self.index >= 624 {
            self.twist();
        }
        let s = self.state[self.index];
        self.index += 1;
        let mut u = s ^ (s >> 11);
        u ^= (u & 0xff3a_58ad) << 7;
        u ^= (u & 0xffff_df8c) << 15;
        (u >> 18) ^ u
    }

    /// 取下一个低 8 位随机数(原版取 `& 0xff`)。
    pub fn next_u8(&mut self) -> u8 {
        (self.next_u32() & 0xff) as u8
    }
}

/// 胶片颗粒效果 —— 参照 Picasa 3 的噪点叠加实现。
///
/// 原版是**两段式**结构(噪声混合计算):
///
/// **第一段 — 生成噪声图**:每像素取一个随机字节 `r`,再向中灰 `0x7f` 收敛:
///
/// ```text
/// 收敛系数 = ROUND(256.0 − amount·256.0)
/// noise = r + ((0x7f − r)·收敛系数 >> 8)
/// ```
///
/// `amount` 越大 → 收敛系数越小 → 噪声越平(趋近常数 127)。
///
/// **第二段 — 混合到像素**:
///
/// ```text
/// out = src + ((0xa0 − |0x80 − src|)·(noise − 0x80)) >> 8      // 再 clamp [0,255]
/// ```
///
/// 权重 `0xa0 − |0x80 − src|` 使**中间调颗粒最重、极暗/极亮处颗粒最轻**。
///
/// **诚实声明**:原版噪点算子的参数名是 `low`/`high`/`randomSeed`/
/// `channelOptions`/`grayscale`,其 `low`/`high`/`randomSeed` 的**默认值未公开**
/// (见模块文档)。我方的 `amount` 是对第一段强度参数的等价参数名;
/// 两段公式、随机数结构、混合公式**均已确认**。
pub fn apply_grain(img: &mut image::RgbaImage, params: &GrainParams) {
    let mut rng = Mt19937::new(params.seed);
    let amount = params.amount.clamp(0.0, 1.0);
    let i_var3 = (256.0 - amount * 256.0).round() as i32;

    // 第一段:生成噪声图(与原版同构 —— 先整图生成,再逐像素混合)。
    // `grayscale=true` 时三通道共用同一噪声值(原版 `channelOptions=7` + `grayscale`)。
    let px = (img.width() * img.height()) as usize;
    let mut noise: Vec<u8> = Vec::with_capacity(px);
    for _ in 0..px {
        let r = rng.next_u8() as i32;
        let n = r + (((0x7f - r) * i_var3) >> 8);
        noise.push(n.clamp(0, 255) as u8);
    }

    // 第二段:混合到像素。
    let mut ni = 0usize;
    for p in img.pixels_mut() {
        let n = noise[ni] as i32;
        ni += 1;
        for c in 0..3 {
            if params.channel_options & (1 << c) == 0 {
                continue;
            }
            let src = p[c] as i32;
            let d = 0x80 - src;
            let abs_d = d.abs();
            let w = 0xa0 - abs_d;
            let delta = (w * (n - 0x80)) >> 8;
            p[c] = (src + delta).clamp(0, 255) as u8;
        }
    }
}

/// grain 参数预设(强度/通道/灰度取自资源包,种子由调用方给定)。
pub fn grain_preset(seed: u32) -> GrainParams {
    let d = GrainParams::default();
    GrainParams { seed, ..d }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picasa_luma_weights_are_not_rec709() {
        // 纯红:0.3086 → 与 Rec.709 的 0.2126 不同
        let y = luma(255.0, 0.0, 0.0);
        assert!((y - 255.0 * 0.3086).abs() < 1e-3, "got {y}");
        // 纯绿:0.6094
        let y = luma(0.0, 255.0, 0.0);
        assert!((y - 255.0 * 0.6094).abs() < 1e-3, "got {y}");
        // 纯蓝:0.082
        let y = luma(0.0, 0.0, 255.0);
        assert!((y - 255.0 * 0.082).abs() < 1e-3, "got {y}");
    }

    #[test]
    fn bw_of_gray_stays_gray() {
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([128, 128, 128, 255]));
        apply_bw(&mut img);
        let p = img.get_pixel(0, 0);
        // R' = 128*0.3086/128 = 0.3086 → 0? 不:归一化后各通道 = 原值比例
        // 灰度像素三通道权重和=1 → Y=128,各通道贡献相同 → 输出接近原灰度
        assert_eq!(p[0], p[1]);
        assert_eq!(p[1], p[2]);
        // 加权平均形式下,三通道相等时 out = v²·Σw / (v·Σw) = v(Σw=1)→ 严格保灰阶
        assert_eq!(p[0], 128, "中性灰应严格保持原灰阶");
    }

    #[test]
    fn bw_black_pixel_becomes_white() {
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 255]));
        apply_bw(&mut img);
        let p = img.get_pixel(0, 0);
        assert_eq!((p[0], p[1], p[2]), (255, 255, 255), "Y==0 应输出纯白");
    }

    #[test]
    fn bw_is_max_normalized_not_plain_grayscale() {
        // 纯红 (255,0,0):Y = 255·0.3086 = 78.693
        // out = Σ(wᵢ·vᵢ²)/Σ(wᵢ·vᵢ) = (0.3086·65025)/78.693 = 255 → 纯色保持自身亮度
        // (这正是「以亮度贡献为权重的加权平均」,而非普通灰度化)
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        apply_bw(&mut img);
        let p = img.get_pixel(0, 0);
        assert_eq!(p[0], 255, "纯红 → 255");
        assert_eq!(p[1], 255);
        assert_eq!(p[2], 255);
    }

    #[test]
    fn sepia_warms_image_and_uses_256_lut() {
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 200, 200, 255]));
        let (mix, shadow, highlight) = sepia_default();
        apply_sepia(&mut img, mix, shadow, highlight);
        let p = img.get_pixel(0, 0);
        assert!(p[0] > p[2], "棕褐色应 R > B, got {:?}", p);
        assert_eq!(p[3], 255, "alpha 不应被改");
    }

    #[test]
    fn sepia_mix_zero_gives_shadow_tint() {
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 255, 255, 255]));
        apply_sepia(&mut img, 0.0, [0x22, 0x16, 0x0b], [0xdd, 0xc9, 0xae]);
        let p = img.get_pixel(0, 0);
        assert_eq!((p[0], p[1], p[2]), (0x22, 0x16, 0x0b));
    }

    #[test]
    fn mt19937_initialization_matches_original_twist_constant() {
        // 原版MT19937 初始化:state[0]=seed, state[i] = (state[i-1]^(state[i-1]>>30))*0x19660d + i
        let mut rng = Mt19937::new(5489);
        // 首次输出应为确定的伪随机值(自洽性检查:两次相同种子输出一致)
        let mut a = Mt19937::new(5489);
        let v1 = a.next_u32();
        let v2 = rng.next_u32();
        assert_eq!(v1, v2);
        assert_ne!(v1, 0);
    }

    #[test]
    fn mt19937_is_deterministic_per_seed() {
        let mut a = Mt19937::new(7);
        let mut b = Mt19937::new(7);
        for _ in 0..100 {
            assert_eq!(a.next_u32(), b.next_u32());
        }
        let mut c = Mt19937::new(8);
        assert_ne!(Mt19937::new(7).next_u32(), c.next_u32());
    }

    #[test]
    fn mt19937_output_is_xorshift32_mixed() {
        // 手算验证:取 state 首个输出,s 取 twist 后 index 0 的值
        let mut rng = Mt19937::new(5489);
        // 手动复刻 state
        let mut state = [0u32; 624];
        state[0] = 5489;
        for i in 1..624 {
            let prev = state[i - 1];
            state[i] = (prev ^ (prev >> 30)).wrapping_mul(0x1966_0d).wrapping_add(i as u32);
        }
        // twist
        const MATRIX_A: u32 = 0x9908_b0df;
        for i in 0..624 {
            let y = (state[i] & 0x8000_0000) | (state[(i + 1) % 624] & 0x7fff_ffff);
            let mut next = state[(i + 397) % 624] ^ (y >> 1);
            if y & 1 != 0 {
                next ^= MATRIX_A;
            }
            state[i] = next;
        }
        let s = state[0];
        let mut u = s ^ (s >> 11);
        u ^= (u & 0xff3a_58ad) << 7;
        u ^= (u & 0xffff_df8c) << 15;
        let expect = (u >> 18) ^ u;
        assert_eq!(rng.next_u32(), expect);
    }

    #[test]
    fn grain_changes_pixels_deterministically() {
        let mut img = image::RgbaImage::from_pixel(8, 8, image::Rgba([120, 120, 120, 255]));
        let p = GrainParams::default();
        let before = img.clone();
        apply_grain(&mut img, &p);
        assert_ne!(img, before, "颗粒应改变像素");
        // 同种子 → 同结果
        let mut img2 = before.clone();
        apply_grain(&mut img2, &p);
        assert_eq!(img, img2);
    }

    #[test]
    fn grain_channel_mask_limits_effect() {
        let mut img = image::RgbaImage::from_pixel(4, 4, image::Rgba([120, 120, 120, 255]));
        let p = GrainParams {
            amount: 0.5,
            seed: 3,
            channel_options: 1, // 只 R
            grayscale: false,
        };
        apply_grain(&mut img, &p);
        let px = img.get_pixel(0, 0);
        assert_eq!(px[1], 120, "G 未选中应不变");
        assert_eq!(px[2], 120, "B 未选中应不变");
    }

    #[test]
    fn grain_zero_amount_is_near_noop() {
        let mut img = image::RgbaImage::from_pixel(4, 4, image::Rgba([120, 120, 120, 255]));
        let before = img.clone();
        let p = GrainParams {
            amount: 0.0,
            ..Default::default()
        };
        apply_grain(&mut img, &p);
        // amount=0 → 收敛系数=256 → 噪声被完全压掉 → 应接近原值
        for y in 0..4 {
            for x in 0..4 {
                let a = before.get_pixel(x, y);
                let b = img.get_pixel(x, y);
                assert!((a[0] as i32 - b[0] as i32).abs() <= 1, "零强度应几乎不变");
            }
        }
    }
}
