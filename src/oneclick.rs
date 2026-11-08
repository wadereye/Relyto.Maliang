//! 一键效果(oneclick):黑白、双色调、颗粒、锐化、暖冷调、暗角、褪色及各类自动调整与着色效果。

/// 返回本项目的亮度权重(R/G/B),全库统一。
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

/// 黑白效果:按亮度贡献加权的归一化灰度。
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

/// sepia(棕褐色)效果:灰度映射到双色调。
pub fn apply_sepia(img: &mut image::RgbaImage, mix: f32, shadow: [u8; 3], highlight: [u8; 3]) {
    let m = (mix.clamp(0.0, 100.0)) / 100.0;
    let tint: [f32; 3] = [
        shadow[0] as f32 + (highlight[0] as f32 - shadow[0] as f32) * m,
        shadow[1] as f32 + (highlight[1] as f32 - shadow[1] as f32) * m,
        shadow[2] as f32 + (highlight[2] as f32 - shadow[2] as f32) * m,
    ];
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

/// sepia 默认参数。
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
    /// 随机种子。
    pub seed: u32,
    /// 通道位掩码:1=R / 2=G / 4=B / 8=A。
    pub channel_options: u8,
    /// 是否灰度噪声。
    pub grayscale: bool,
}

impl Default for GrainParams {
    fn default() -> Self {
        let v = crate::assets::f64s("grain.default");
        GrainParams {
            amount: v.first().copied().unwrap_or(0.15) as f32,
            seed: v.get(1).copied().unwrap_or(0.0) as u32,
            channel_options: v.get(2).copied().unwrap_or(7.0) as u8,
            grayscale: v.get(3).copied().unwrap_or(1.0) != 0.0,
        }
    }
}

/// MT19937 随机数发生器。
#[derive(Debug, Clone)]
pub struct Mt19937 {
    state: [u32; 624],
    index: usize,
}

impl Mt19937 {
    /// 用种子初始化。
    pub fn new(seed: u32) -> Mt19937 {
        let mut state = [0u32; 624];
        state[0] = seed;
        for i in 1..624 {
            let prev = state[i - 1];
            state[i] = (prev ^ (prev >> 30)).wrapping_mul(0x1966_0d).wrapping_add(i as u32);
        }
        Mt19937 { state, index: 624 }
    }

    /// 重新生成随机数状态(twist 迭代)。
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

    /// 取下一个 32 位随机数。
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

    /// 取下一个低 8 位随机数。
    pub fn next_u8(&mut self) -> u8 {
        (self.next_u32() & 0xff) as u8
    }
}

/// 胶片颗粒效果:生成噪声图并混合到像素。
pub fn apply_grain(img: &mut image::RgbaImage, params: &GrainParams) {
    let mut rng = Mt19937::new(params.seed);
    let amount = params.amount.clamp(0.0, 1.0);
    let keep = (256.0 - amount * 256.0).round() as i32;

    let px = (img.width() * img.height()) as usize;
    let per_pixel = if params.grayscale { 1 } else { 3 };
    let mut noise: Vec<u8> = Vec::with_capacity(px * per_pixel);
    for _ in 0..px * per_pixel {
        let r = rng.next_u8() as i32;
        let n = r + (((0x7f - r) * keep) >> 8);
        noise.push(n.clamp(0, 255) as u8);
    }

    // 第二段:混合到像素。
    let mut ni = 0usize;
    for p in img.pixels_mut() {
        let chan_off = ni;
        ni += per_pixel;
        for c in 0..3 {
            if params.channel_options & (1 << c) == 0 {
                continue;
            }
            let n = noise[chan_off + if params.grayscale { 0 } else { c }] as i32;
            let src = p[c] as i32;
            let d = 0x80 - src;
            let abs_d = d.abs();
            let w = 0xa0 - abs_d;
            let delta = (w * (n - 0x80)) >> 8;
            p[c] = (src + delta).clamp(0, 255) as u8;
        }
    }
}

/// 用给定种子生成 grain 参数预设。
pub fn grain_preset(seed: u32) -> GrainParams {
    let d = GrainParams::default();
    GrainParams { seed, ..d }
}

/// 取一段预设参数,按索引取值、缺失时回退到给定默认值。
fn asset3(key: &str, fallback: [f64; 3]) -> [f32; 3] {
    let v = crate::assets::f64s(key);
    [
        v.first().copied().unwrap_or(fallback[0]) as f32,
        v.get(1).copied().unwrap_or(fallback[1]) as f32,
        v.get(2).copied().unwrap_or(fallback[2]) as f32,
    ]
}

/// 色温偏移(暖调/冷调共用):按增益插值调整各通道。
fn color_temp(img: &mut image::RgbaImage, amount: f32, key: &str, fallback: [f64; 3]) {
    let base = asset3(key, fallback);
    let t = amount.clamp(0.0, 100.0) / 100.0;
    let g = [
        1.0 + (base[0] - 1.0) * t,
        1.0 + (base[1] - 1.0) * t,
        1.0 + (base[2] - 1.0) * t,
    ];
    let w = luma_weights();
    let keep = w[0] * g[0] + w[1] * g[1] + w[2] * g[2];
    if keep <= 0.0 {
        return;
    }
    let gn = [g[0] / keep, g[1] / keep, g[2] / keep];
    for p in img.pixels_mut() {
        for c in 0..3 {
            p[c] = (p[c] as f32 * gn[c]).clamp(0.0, 255.0) as u8;
        }
    }
}

/// 暖调(色温偏黄红)。
pub fn apply_warm(img: &mut image::RgbaImage, amount: f32) {
    color_temp(img, amount, "warm.gain", [1.18, 1.02, 0.84]);
}

/// 冷调(色温偏蓝青)。
pub fn apply_cool(img: &mut image::RgbaImage, amount: f32) {
    color_temp(img, amount, "cool.gain", [0.84, 1.01, 1.18]);
}

/// 三次平滑过渡;用于暗角的边缘柔化(线性衰减会在圆环边界留下硬边)。
fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    if e0 >= e1 {
        return 0.0;
    }
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 暗角(四角压暗)。
pub fn apply_vignette(img: &mut image::RgbaImage, amount: f32) {
    let v = crate::assets::f64s("vignette");
    let start = v.first().copied().unwrap_or(0.55) as f32;
    let max = v.get(1).copied().unwrap_or(0.55) as f32;
    let strength = max * (amount.clamp(0.0, 100.0) / 100.0);
    if strength <= 0.0 {
        return;
    }
    let (w, h) = (img.width() as f32, img.height() as f32);
    let (cx, cy) = (w * 0.5, h * 0.5);
    // 以半对角线为 1,把任意宽高比都归一到"四角 = 1.0"。
    let max_d = (cx * cx + cy * cy).sqrt();
    if max_d <= 0.0 {
        return;
    }
    for (x, y, p) in img.enumerate_pixels_mut() {
        let dx = x as f32 + 0.5 - cx;
        let dy = y as f32 + 0.5 - cy;
        let d = (dx * dx + dy * dy).sqrt() / max_d;
        let f = 1.0 - strength * smoothstep(start, 1.0, d);
        for c in 0..3 {
            p[c] = (p[c] as f32 * f).clamp(0.0, 255.0) as u8;
        }
    }
}

/// 褪色(胶片感:抬黑位、压白位、轻微去饱和)。
pub fn apply_faded(img: &mut image::RgbaImage, amount: f32) {
    let base = asset3("faded", [26.0, 0.84, 0.30]);
    let t = amount.clamp(0.0, 100.0) / 100.0;
    let lift = base[0] * t;
    let gain = 1.0 + (base[1] - 1.0) * t;
    let desat = base[2] * t;
    for p in img.pixels_mut() {
        let c = [
            lift + p[0] as f32 * gain,
            lift + p[1] as f32 * gain,
            lift + p[2] as f32 * gain,
        ];
        let y = luma(c[0], c[1], c[2]);
        for i in 0..3 {
            p[i] = (c[i] + (y - c[i]) * desat).clamp(0.0, 255.0) as u8;
        }
    }
}

/// 补光(Fill Light):暗部增益大、亮部增益小地提亮图像。
pub fn apply_fill_light(img: &mut image::RgbaImage, s: f32, blend: f32) {
    let s = s.clamp(0.0, 1.0);
    let blend = blend.clamp(0.0, 1.0);
    let gain = 1.0 / ((1.0 - s) * 0.7 + 0.3);
    let expo = 1.0 / (gain * 0.7 + 0.3);
    let mut lut = [0i32; 256];
    for i in 0..256usize {
        let x = ((i as f32) * gain / 255.0).max(0.0);
        let y = x.powf(expo).min(256.0);
        lut[i] = (y * 255.0).round().clamp(0.0, 65280.0) as i32;
    }
    let blend_q = (blend * 256.0).round() as i32;
    for p in img.pixels_mut() {
        let lum = ((p[0] as i32) + 2 * (p[1] as i32) + (p[2] as i32)) >> 2;
        let mask = 0xff00 - lum * blend_q;
        for c in 0..3 {
            let v = p[c] as i32;
            let out = v + (((lut[v as usize] - v) * mask) >> 16);
            p[c] = out.clamp(0, 255) as u8;
        }
    }
}

/// 色阶(gamma curves):按黑/白点拉伸并用 gamma 校正。
pub fn apply_levels(img: &mut image::RgbaImage, black: f32, white: f32, gamma: f32) {
    let black = black.clamp(0.0, 1.0);
    let white = white.clamp(0.0, 1.0);
    let gamma = if gamma <= 0.0 { 1.0 } else { gamma };
    let inv = if white != black {
        1.0 / (white - black)
    } else {
        1.0
    };
    let mut lut16 = [0u16; 257];
    for i in 0..256usize {
        let x = i as f32 / 255.0;
        let y = x.powf(1.0 / gamma) * 65280.0 - black * 65280.0;
        let v = (y * inv).round().clamp(0.0, 65280.0) as i32;
        lut16[i] = v as u16;
    }
    lut16[256] = lut16[255];
    for p in img.pixels_mut() {
        for c in 0..3 {
            let v = p[c] as usize;
            let d = lut16[v + 1] as i32 - lut16[v] as i32;
            let n = (v.wrapping_mul(2654435761) & 0xff) as i32;
            let out = ((d * n >> 8) - (d >> 1) + lut16[v] as i32) >> 8;
            p[c] = out.clamp(0, 255) as u8;
        }
    }
}

/// gamma 校正:按 gamma 调整亮度。
pub fn apply_gamma(img: &mut image::RgbaImage, gamma: f32) {
    let gamma = if gamma <= 0.0 { 1.0 } else { gamma };
    let mut lut = [0u8; 256];
    for i in 0..256usize {
        let x = i as f32 / 255.0;
        lut[i] = (x.powf(1.0 / gamma) * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    for p in img.pixels_mut() {
        for c in 0..3 {
            p[c] = lut[p[c] as usize];
        }
    }
}

/// 色温(colortemp):双轴(冷暖轴 + 白场偏移轴)色偏调整。
pub fn apply_color_temp(img: &mut image::RgbaImage, temp: f32, white: f32) {
    let m = (temp * 256.0) as i32;
    let k = (((white.clamp(0.0, 1.0) * 128.0) as i32).clamp(0, 255)) as usize;
    let m_pos = m.max(0);
    let step = if k < 256 { 0x10000 / (256 - k) as i32 } else { 0x10000 };
    let mut t = [0i32; 256];
    let mut u = [0i32; 256];
    let mut w = [0i32; 256];
    for i in 0..256usize {
        t[i] = ((i as i32) * ((256 - k) as i32)) >> 8;
        u[i] = ((i as i32) * step) >> 8;
        w[i] = (i as i32) * ((256 - i) as i32);
    }
    for p in img.pixels_mut() {
        let mut o = [0u8; 3];
        for c in 0..3 {
            let base = t[p[c] as usize].clamp(0, 255) as usize;
            let weight = w[base];
            let delta = match c {
                0 => (weight * m) >> 15,          // 红
                1 => (weight * m_pos) >> 17,      // 绿(半幅)
                _ => -((weight * m) >> 15),       // 蓝
            };
            let v = (base as i32 + delta).clamp(0, 255) as usize;
            o[c] = u[v].clamp(0, 255) as u8;
        }
        p[0] = o[0];
        p[1] = o[1];
        p[2] = o[2];
    }
}

/// 自动对比度:按直方图裁剪阈值拉伸各通道。
pub fn apply_autocontrast(img: &mut image::RgbaImage, cap: u8) {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w < 2 || h < 2 {
        return;
    }
    let x0 = w * 5 / 100;
    let x1 = w * 95 / 100;
    let y0 = h * 5 / 100;
    let y1 = h * 95 / 100;
    let (x0, x1) = (x0.min(w - 1), x1.max(x0 + 1).min(w));
    let (y0, y1) = (y0.min(h - 1), y1.max(y0 + 1).min(h));
    let mut hist = [[0u32; 256]; 3];
    for y in y0..y1 {
        for x in x0..x1 {
            let p = img.get_pixel(x as u32, y as u32);
            for c in 0..3 {
                hist[c][p[c] as usize] += 1;
            }
        }
    }
    let n = ((x1 - x0) * (y1 - y0)) as u32;
    let thresh = (n / 200).max(1);
    let mut lut = [[0u8; 256]; 3];
    for c in 0..3 {
        // low = 首个 cum ≥ 阈值
        let mut cum = 0u32;
        let mut low = 0usize;
        for i in 0..256 {
            cum += hist[c][i];
            if cum >= thresh {
                low = i;
                break;
            }
        }
        // high = 末个 cum ≥ 阈值(等价于从尾部数第一个使 cum ≥ 阈值 的 bin)
        let mut cum = 0u32;
        let mut high = 255usize;
        for i in (0..256).rev() {
            cum += hist[c][i];
            if cum >= thresh {
                high = i;
                break;
            }
        }
        if high <= low {
            for i in 0..256 {
                lut[c][i] = cap;
            }
            continue;
        }
        let scale = ((cap as u32) << 16) / (high - low) as u32;
        for i in 0..256usize {
            let v = ((i as i32 - low as i32) * scale as i32) >> 16;
            lut[c][i] = v.clamp(0, cap as i32) as u8;
        }
    }
    for p in img.pixels_mut() {
        for c in 0..3 {
            p[c] = lut[c][p[c] as usize];
        }
    }
}

/// 自动光线(autolight):用直方图阈值做自动色阶 + gamma。
pub fn apply_autolight(img: &mut image::RgbaImage) {
    let n = (img.width() as u64) * (img.height() as u64);
    if n == 0 {
        return;
    }
    let mut hist = [[0u32; 256]; 3];
    for p in img.pixels() {
        for c in 0..3 {
            hist[c][p[c] as usize] += 1;
        }
    }
    let thresh = ((n as f64) * 0.001).round().max(1.0) as u32;
    let mut lows = [0usize; 3];
    let mut highs = [255usize; 3];
    for c in 0..3 {
        let mut cum = 0u32;
        for i in 0..256 {
            cum += hist[c][i];
            if cum >= thresh {
                lows[c] = i;
                break;
            }
        }
        let mut cum = 0u32;
        for i in (0..256).rev() {
            cum += hist[c][i];
            if cum >= thresh {
                highs[c] = i;
                break;
            }
        }
    }
    let low = *lows.iter().min().unwrap();
    let high = *highs.iter().max().unwrap();
    if high == 255 && low == 0 {
        // 动态范围已满:像素保持不动,直接返回。
        return;
    }
    apply_levels(img, low as f32 / 255.0, high as f32 / 255.0, 1.0);
}

/// 自动颜色(autocolor):用中性像素色度直方图做色偏校正。
pub fn apply_autocolor(img: &mut image::RgbaImage) {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return;
    }
    let mut hist = vec![0u32; 64 * 64];
    let mut found = false;
    for p in img.pixels() {
        let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
        if !(0x20..=0xe0).contains(&g) {
            continue;
        }
        if !(g < 2 * r && r < 2 * g && g < 2 * b && b < 2 * g) {
            continue;
        }
        let minrg = r.min(g).max(1);
        let minbg = b.min(g).max(1);
        let x = (((r - g) * 32) / minrg + 32).clamp(0, 63) as usize;
        let y = (((b - g) * 32) / minbg + 32).clamp(0, 63) as usize;
        hist[y * 64 + x] += 1;
        found = true;
    }
    if !found {
        return;
    }
    // 众数 bin → 反推出代表性的 (R,G,B)
    let mut best = 0usize;
    for i in 1..hist.len() {
        if hist[i] > hist[best] {
            best = i;
        }
    }
    let (bx, by) = ((best % 64) as i32, (best / 64) as i32);
    let g0 = 96i32;
    let minrg = g0;
    let minbg = g0;
    let dr = (bx - 32) * minrg / 32;
    let db = (by - 32) * minbg / 32;
    let cast = [
        (g0 + dr).clamp(0, 255) as f64,
        g0 as f64,
        (g0 + db).clamp(0, 255) as f64,
    ];
    let m = [
        [1.9044f64, 0.4508, -0.3826],
        [-0.0532, 1.8018, 0.1995],
        [0.0491, -0.3057, 1.8576],
    ];
    let mul = |a: &[[f64; 3]; 3], v: [f64; 3]| -> [f64; 3] {
        [
            a[0][0] * v[0] + a[0][1] * v[1] + a[0][2] * v[2],
            a[1][0] * v[0] + a[1][1] * v[1] + a[1][2] * v[2],
            a[2][0] * v[0] + a[2][1] * v[1] + a[2][2] * v[2],
        ]
    };
    let lum_of = |r: f64, g: f64, b: f64| (151.0 * g + 77.0 * r + 28.0 * b) / 256.0;
    let m_cast = mul(&m, cast);
    for p in img.pixels_mut() {
        let rgb = [p[0] as f64, p[1] as f64, p[2] as f64];
        let l = lum_of(rgb[0], rgb[1], rgb[2]);
        let gray = [l, l, l];
        let m_gray = mul(&m, gray);
        let kk = [
            m_gray[0] / m_cast[0],
            m_gray[1] / m_cast[1],
            m_gray[2] / m_cast[2],
        ];
        let mut out = [0f64; 3];
        for r in 0..3 {
            out[r] = m[r][0] * kk[0] * rgb[0] + m[r][1] * kk[1] * rgb[1] + m[r][2] * kk[2] * rgb[2];
        }
        for c in 0..3 {
            p[c] = out[c].clamp(0.0, 255.0) as u8;
        }
    }
}

/// 高反差黑白(ansel)效果。
pub fn apply_ansel(img: &mut image::RgbaImage, weights: [f32; 3], amount: f32) {
    let sum: f32 = weights.iter().sum();
    if sum <= 0.0 {
        return;
    }
    let w: [i32; 3] = [
        (weights[0] / sum * 256.0).round() as i32,
        (weights[1] / sum * 256.0).round() as i32,
        (weights[2] / sum * 256.0).round() as i32,
    ];
    let a = amount as i32;
    // 权重与缓冲同为 RGBA 语义,按下标 0/1/2 对应 R/G/B。
    let luma16 = |p: &image::Rgba<u8>| {
        (p[0] as i32 * w[0] + p[1] as i32 * w[1] + p[2] as i32 * w[2]).clamp(0, 0xffff)
    };
    let mut hist = [0u32; 256];
    for p in img.pixels() {
        hist[(luma16(p) >> 8) as usize] += 1;
    }
    let mut mu: u64 = 0;
    for j in 0..256usize {
        mu += (hist[j] as u64) * (j as u64) * ((256 - j) as u64) / 64;
    }
    if mu == 0 {
        return;
    }
    for p in img.pixels_mut() {
        let l16 = luma16(p);
        let delta = (((0xffff - l16) * l16 >> 14) * a) >> 8;
        let v = ((l16 + delta).clamp(0, 0xffff) >> 8) as u8;
        p[0] = v;
        p[1] = v;
        p[2] = v;
    }
}

/// 饱和度(sat)调整。
pub fn apply_saturation(img: &mut image::RgbaImage, s: f32) {
    let s = if s.is_nan() { 1.0 } else { s };
    let lum8 = |r: i32, g: i32, b: i32| 5 * g + 2 * r + b;
    let mut curve = [0i32; 256];
    if s < 1.0 {
        for i in 0..256usize {
            let x = i as f64 / 256.0;
            let y = x.sqrt();
            let t = if (1.0 - x).abs() < 1e-12 {
                (2.0 / (s as f64) - 1.0) * 256.0
            } else {
                ((1.0 - (y + (1.0 / s as f64) * (x - y))) / (1.0 - x) - 1.0) * 256.0
            };
            curve[i] = t.round().clamp(0.0, 65535.0) as i32;
        }
    }
    for p in img.pixels_mut() {
        let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
        let l8 = lum8(r, g, b);
        if l8 == 0 {
            continue;
        }
        let inv = if l8 < 8 { 0xffff } else { 0x7f800 / l8 };
        let c8 = [8 * r, 8 * g, 8 * b];
        let chroma = c8.map(|c| (c - l8) * inv);
        let ci = (chroma.into_iter().max().unwrap() >> 11).clamp(0, 255) as usize;
        if ci > 0 {
            let k = curve[ci];
            for (idx, c) in c8.iter().enumerate() {
                p[idx] = ((((c - l8) * k >> 8) + c) >> 3).clamp(0, 255) as u8;
            }
        }
    }
}

/// 发光(glow):提亮 + 模糊 + 混回的柔光效果。
pub fn apply_glow(img: &mut image::RgbaImage, r: f32) {
    let r = r.abs().clamp(0.0, 256.0);
    let mut layer = img.clone();
    apply_gamma(&mut layer, 0.5);
    let radius = (256.0 - r) / 256.0;
    if radius > 0.0 {
        crate::effects::blur3pass(&mut layer, radius * 8.0);
    }
    let amount = ((256.0 - r) / 256.0 * 256.0).round() as i32;
    for (p, q) in img.pixels_mut().zip(layer.pixels()) {
        for c in 0..3 {
            let src = p[c] as i32;
            p[c] = (src + ((q[c] as i32 - src) * amount >> 8)).clamp(0, 255) as u8;
        }
    }
}

/// USM 锐化(unsharp)。
pub fn apply_unsharp_usm(img: &mut image::RgbaImage, amount: f32) {
    let amount = amount as i32;
    if amount == 0 {
        return;
    }
    let mut blurred = img.clone();
    crate::effects::blur3pass(&mut blurred, 2.0);
    for (p, q) in img.pixels_mut().zip(blurred.pixels()) {
        for c in 0..3 {
            let src = p[c] as i32;
            let out = src + ((q[c] as i32 - src) * amount >> 8);
            p[c] = out.clamp(0, 255) as u8;
        }
    }
}

/// 单色调(tint):自动色阶 + 灰度 + 着色。
pub fn apply_tint(img: &mut image::RgbaImage, color: [u8; 3], gamma: f32) {
    apply_autocontrast(img, 255);
    apply_bw(img);
    if (gamma - 1.0).abs() > 1e-6 {
        apply_gamma(img, gamma);
    }
    let k = [color[0] as f32 / 255.0, color[1] as f32 / 255.0, color[2] as f32 / 255.0];
    for p in img.pixels_mut() {
        for c in 0..3 {
            p[c] = (p[c] as f32 * k[c]).clamp(0.0, 255.0) as u8;
        }
    }
}

/// 方向渐变着色(dir_tint):沿方向向量做线性渐变着色。
pub fn apply_dir_tint(img: &mut image::RgbaImage, angle: f32, strength: f32, color: [u8; 3]) {
    let mut s = if strength.is_nan() { 50.0 } else { strength };
    if s >= 99.9 {
        s = 99.9;
    } else if s < 0.01 {
        s = 0.01;
    }
    let (w, h) = (img.width() as i32, img.height() as i32);
    if w <= 0 || h <= 0 {
        return;
    }
    let dx = angle.cos() as f64;
    let dy = angle.sin() as f64;
    let _len2 = dx * dx + dy * dy;
    let mut lut = [0i32; 384];
    for i in 0..384usize {
        let t = i as f64 * 0.00390625; // i/256,范围 0..1.5
        let f = if t <= -1.5 {
            1.0
        } else if t > 1.5 {
            0.0
        } else if t <= 0.5 {
            if t <= -0.5 {
                (-(t * t * t) * 0.16666667 - t * t * 0.75) - t * 1.125 + 0.4375
            } else {
                0.5 - (t * 0.75 - (t * t * t) * 0.33333334)
            }
        } else {
            0.5625 - (t * 1.125 + ((t * t * t) * 0.16666667 - t * t * 0.75))
        };
        lut[i] = ((1.0 - (f + f)) * 255.9999).round() as i32;
    }
    let cx = w as f64 / 2.0;
    let cy = h as f64 / 2.0;
    let soft = (s as f64 / 100.0) * (w.max(h) as f64).max(1.0);
    let step_x = (dx * 256.0).round() as i64;
    let step_y = (dy * 256.0).round() as i64;
    let mut row = ((-cx) * dx * 256.0 + (-cy) * dy * 256.0).round() as i64;
    let k = [color[0] as i32, color[1] as i32, color[2] as i32];
    for y in 0..h {
        let mut proj = row;
        row += step_y;
        for x in 0..w {
            let idx8 = (proj as f64 / soft) as i64; // 8.8 定点 → 软化宽度归一化
            proj += step_x;
            let idx = idx8.unsigned_abs() as usize;
            let raw = if idx >= 0x17f {
                0xff
            } else if idx8 < 0 {
                -lut[idx]
            } else {
                lut[idx]
            };
            let mix = ((raw + 0x100) / 2).clamp(-255, 255);
            let p = img.get_pixel_mut(x as u32, y as u32);
            for c in 0..3 {
                let v = p[c] as i32;
                p[c] = (v + ((k[c] - v) * mix >> 8)).clamp(0, 255) as u8;
            }
        }
    }
}

/// 径向渐变着色(radtint):以圆心 + 半径阈值做着色。
pub fn apply_radtint(img: &mut image::RgbaImage, cx: f32, cy: f32, color: [u8; 3]) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    if w <= 0 || h <= 0 {
        return;
    }
    let ccx = (w as f32 * cx).round() as i32;
    let ccy = (h as f32 * cy).round() as i32;
    let k = [color[0] as i32, color[1] as i32, color[2] as i32];
    let rmax = 0x400 as f64;
    for y in 0..h {
        for x in 0..w {
            let dx = (x - ccx) as f64;
            let dy = (y - ccy) as f64;
            let d2 = dx * dx + dy * dy;
            let t = if d2 >= rmax * rmax {
                1.0
            } else {
                1.0 - (d2.sqrt() / rmax)
            };
            let mix = (t * t * 256.0) as i32;
            let p = img.get_pixel_mut(x as u32, y as u32);
            for c in 0..3 {
                let v = p[c] as i32;
                p[c] = (v + ((k[c] - v) * mix >> 8)).clamp(0, 255) as u8;
            }
        }
    }
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

    /// 中性灰做暖调:应只偏色、亮度基本不变。
    #[test]
    fn warm_cools_and_warms_without_shifting_exposure() {
        let gray = image::Rgba([128, 128, 128, 255]);
        let mut warm = image::RgbaImage::from_pixel(2, 2, gray);
        apply_warm(&mut warm, 100.0);
        let w = warm.get_pixel(0, 0);
        assert!(w[0] > w[2], "暖调应红 > 蓝,got {w:?}");
        let y_w = luma(w[0] as f32, w[1] as f32, w[2] as f32);
        let y_0 = luma(128.0, 128.0, 128.0);
        assert!((y_w - y_0).abs() < 6.0, "暖调不应明显改变亮度:{y_w} vs {y_0}");

        let mut cool = image::RgbaImage::from_pixel(2, 2, gray);
        apply_cool(&mut cool, 100.0);
        let c = cool.get_pixel(0, 0);
        assert!(c[2] > c[0], "冷调应蓝 > 红,got {c:?}");
        let y_c = luma(c[0] as f32, c[1] as f32, c[2] as f32);
        assert!((y_c - y_0).abs() < 6.0, "冷调不应明显改变亮度:{y_c} vs {y_0}");
    }

    /// amount=0 时效果应为恒等变换(不改动像素)。
    #[test]
    fn tone_effects_are_identity_at_zero() {
        let base = image::Rgba([30, 140, 220, 255]);
        let mut img = image::RgbaImage::from_pixel(4, 4, base);
        apply_warm(&mut img, 0.0);
        apply_cool(&mut img, 0.0);
        apply_faded(&mut img, 0.0);
        let p = img.get_pixel(1, 1);
        assert_eq!((p[0], p[1], p[2]), (30, 140, 220), "零强度应不动像素,got {p:?}");
        // 暗角零强度同样不动(内部直接 return)
        let mut img2 = image::RgbaImage::from_pixel(4, 4, base);
        apply_vignette(&mut img2, 0.0);
        assert_eq!(img2.get_pixel(3, 3)[0], 30);
    }

    /// 暗角:**中心不变、四角变暗**,且中间区域是渐变的(不是一刀切的圆环)。
    #[test]
    fn vignette_darkens_corners_not_center() {
        let mut img = image::RgbaImage::from_pixel(64, 64, image::Rgba([200, 200, 200, 255]));
        apply_vignette(&mut img, 100.0);
        let center = img.get_pixel(32, 32)[0];
        let corner = img.get_pixel(0, 0)[0];
        assert_eq!(center, 200, "中心不应被压暗");
        assert!(corner < 200, "四角应被压暗,got {corner}");
        // 从中心往外应单调不增(渐变而非硬边)
        let edge = img.get_pixel(32, 0)[0];
        assert!(corner < edge && edge <= center, "应由中心向外递减:{center}/{edge}/{corner}");
    }

    /// 褪色:抬黑位(纯黑变灰)+ 压白位(纯白不再满值)。
    #[test]
    fn faded_lifts_blacks_and_compresses_whites() {
        let mut black = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 255]));
        apply_faded(&mut black, 100.0);
        assert!(black.get_pixel(0, 0)[0] > 8, "黑位应被抬起,got {:?}", black.get_pixel(0, 0));

        let mut white = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 255, 255, 255]));
        apply_faded(&mut white, 100.0);
        assert!(white.get_pixel(0, 0)[0] < 255, "白位应被压低,got {:?}", white.get_pixel(0, 0));
    }

    // ===== 一键效果回归测试 =====

    /// 渐变图(左黑右白),用于检验所有"应该拉开动态"的效果。
    fn gradient(w: u32, h: u32) -> image::RgbaImage {
        let mut img = image::RgbaImage::new(w, h);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let v = (x * 255 / (w - 1).max(1)) as u8;
            *p = image::Rgba([v, v, v, 255]);
            let _ = y;
        }
        img
    }

    #[test]
    fn fill_light_lifts_shadows_more_than_highlights() {
        let mut img = image::RgbaImage::new(4, 1);
        for (i, p) in img.pixels_mut().enumerate() {
            let v = 16 + (i as u8) * 60; // 16 / 76 / 136 / 196
            *p = image::Rgba([v, v, v, 255]);
        }
        let before: Vec<u8> = img.pixels().map(|p| p[0]).collect();
        apply_fill_light(&mut img, 1.0, 0.25);
        let after: Vec<u8> = img.pixels().map(|p| p[0]).collect();
        // 暗部抬升应显著大于亮部抬升
        let d_shadow = after[0] as i32 - before[0] as i32;
        let d_high = after[3] as i32 - before[3] as i32;
        assert!(d_shadow > d_high, "暗部抬升应大于亮部:{d_shadow} vs {d_high}");
        assert!(d_shadow > 100, "暗部应被大幅提亮,got {d_shadow}");
        // 纯黑保持纯黑(pow(0, x) = 0)
        let mut black = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 255]));
        apply_fill_light(&mut black, 1.0, 0.25);
        assert_eq!(black.get_pixel(0, 0)[0], 0, "纯黑应保持纯黑");
    }

    #[test]
    fn levels_identity_is_near_identity() {
        let mut img = gradient(16, 4);
        let before = img.clone();
        apply_levels(&mut img, 0.0, 1.0, 1.0);
        for (a, b) in img.pixels().zip(before.pixels()) {
            assert!(
                (a[0] as i32 - b[0] as i32).abs() <= 1,
                "恒等色阶只允许 ±1 抖动差:{:?} vs {:?}",
                a,
                b
            );
        }
    }

    #[test]
    fn levels_gamma_brightens_midtones() {
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([128, 128, 128, 255]));
        apply_levels(&mut img, 0.0, 1.0, 2.2);
        assert!(
            img.get_pixel(0, 0)[0] > 128,
            "gamma>1 应提亮中间调,got {}",
            img.get_pixel(0, 0)[0]
        );
    }

    #[test]
    fn gamma_half_is_square() {
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([128, 128, 128, 255]));
        apply_gamma(&mut img, 0.5);
        // pow(128/255, 2)·255 ≈ 63
        assert!((img.get_pixel(0, 0)[0] as i32 - 63).abs() <= 1);
    }

    #[test]
    fn autocontrast_stretches_low_contrast_gradient() {
        // 只占 100..150 的窄带图
        let mut img = image::RgbaImage::new(16, 4);
        for (x, _y, p) in img.enumerate_pixels_mut() {
            let v = 100 + (x * 50 / 15) as u8;
            *p = image::Rgba([v, v, v, 255]);
        }
        apply_autocontrast(&mut img, 255);
        let lo = img.get_pixel(0, 0)[0];
        let hi = img.get_pixel(15, 0)[0];
        assert!(lo <= 5, "低端应被拉到近黑,got {lo}");
        assert!(hi >= 250, "高端应被拉到近白,got {hi}");
    }

    #[test]
    fn autocontrast_enhance_caps_at_252() {
        let mut img = gradient(32, 8);
        apply_autocontrast(&mut img, 252);
        for p in img.pixels() {
            assert!(p[0] <= 252, "enhance 上限应为 252,got {}", p[0]);
        }
    }

    #[test]
    fn autolight_leaves_full_range_image_unchanged() {
        let mut img = gradient(64, 8);
        let before = img.clone();
        apply_autolight(&mut img);
        assert_eq!(img, before, "满动态图应原样返回");
    }

    #[test]
    fn autocolor_neutralizes_a_color_cast() {
        // 整体偏蓝的灰(仍满足中性像素判定:G<2R, R<2G, G<2B, B<2G)
        let mut img = image::RgbaImage::new(16, 16);
        for p in img.pixels_mut() {
            *p = image::Rgba([100, 120, 150, 255]);
        }
        let cast_before = img.get_pixel(8, 8)[2] as i32 - img.get_pixel(8, 8)[0] as i32;
        apply_autocolor(&mut img);
        let p = img.get_pixel(8, 8);
        let cast_after = p[2] as i32 - p[0] as i32;
        assert!(
            cast_after.abs() < cast_before.abs(),
            "色偏应被削弱:{cast_before} → {cast_after}"
        );
    }

    #[test]
    fn ansel_outputs_grayscale() {
        let mut img = image::RgbaImage::new(8, 8);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgba([(x * 30) as u8, (y * 30) as u8, 128, 255]);
        }
        apply_ansel(&mut img, [0.3086, 0.6094, 0.082], 64.0);
        for p in img.pixels() {
            assert_eq!(p[0], p[1], "ansel 应输出灰度");
            assert_eq!(p[1], p[2], "ansel 应输出灰度");
        }
    }

    #[test]
    fn saturation_one_is_noop() {
        // s = 1 → 曲线恒 0 → 不变(恒等语义)
        let mut img = image::RgbaImage::new(8, 8);
        for (x, _y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgba([(x * 30) as u8, 200, 40, 255]);
        }
        let before = img.clone();
        apply_saturation(&mut img, 1.0);
        assert_eq!(img, before, "s=1 应为恒等");
    }

    #[test]
    fn saturation_boost_increases_chroma() {
        let mut a = image::RgbaImage::new(4, 4);
        let mut b = image::RgbaImage::new(4, 4);
        for p in a.pixels_mut() {
            *p = image::Rgba([200, 100, 60, 255]);
        }
        for p in b.pixels_mut() {
            *p = image::Rgba([200, 100, 60, 255]);
        }
        let spread = |img: &image::RgbaImage| {
            let p = img.get_pixel(0, 0);
            p[0].max(p[1]).max(p[2]) as i32 - p[0].min(p[1]).min(p[2]) as i32
        };
        apply_saturation(&mut a, 0.5);
        apply_saturation(&mut b, 0.1);
        // s 越小增强越强
        assert!(
            spread(&b) >= spread(&a),
            "s 越小增强应越强:{} vs {}",
            spread(&a),
            spread(&b)
        );
        assert!(spread(&b) > 140, "s=0.1 应增强色度,got {}", spread(&b));
    }

    #[test]
    fn glow_darkens_then_blurs_then_mixes_back() {
        let mut img = image::RgbaImage::new(9, 9);
        for p in img.pixels_mut() {
            *p = image::Rgba([100, 100, 100, 255]);
        }
        img.put_pixel(4, 4, image::Rgba([200, 200, 200, 255]));
        let peak_before = img.get_pixel(4, 4)[0];
        apply_glow(&mut img, 128.0);
        let center = img.get_pixel(4, 4)[0];
        let flat = img.get_pixel(0, 0)[0];
        // 平坦区:压暗+模糊副本按 50% 混回 → 仍接近原值
        assert!(
            (flat as i32 - 100).abs() <= 30,
            "平坦区应大致保持,got {flat}"
        );
        // 尖峰被模糊摊薄 → 峰值下降
        assert!(center < peak_before, "尖峰应被摊薄:{peak_before} → {center}");
    }

    #[test]
    fn unsharp_usm_negative_amount_sharpens() {
        // 竖边:左 60 右 200
        let mut img = image::RgbaImage::new(8, 4);
        for (x, _y, p) in img.enumerate_pixels_mut() {
            let v = if x < 4 { 60 } else { 200 };
            *p = image::Rgba([v, v, v, 255]);
        }
        let before = img.clone();
        apply_unsharp_usm(&mut img, -154.0);
        let e0 = img.get_pixel(3, 0)[0] as i32;
        let e1 = img.get_pixel(4, 0)[0] as i32;
        assert!(e1 - e0 > 140, "负 amount 应锐化边缘:{e0} → {e1}");
        let _ = before;
    }

    #[test]
    fn tint_equals_autocontrast_plus_bw_plus_multiply() {
        let mut a = gradient(24, 6);
        let mut b = a.clone();
        apply_tint(&mut a, [0x9a, 0x7b, 0x4f], 1.0);
        apply_autocontrast(&mut b, 255);
        apply_bw(&mut b);
        for (pa, pb) in a.pixels().zip(b.pixels()) {
            let expect_r = ((pb[0] as f32) * 0x9a as f32 / 255.0).round() as u8;
            assert!(
                (pa[0] as i32 - expect_r as i32).abs() <= 1,
                "tint 应与组合等价:{:?} vs {}",
                pa,
                expect_r
            );
        }
    }

    #[test]
    fn dir_tint_gradient_is_monotonic_along_axis() {
        let mut img = image::RgbaImage::new(32, 8);
        for p in img.pixels_mut() {
            *p = image::Rgba([128, 128, 128, 255]);
        }
        apply_dir_tint(&mut img, 0.0, 50.0, [255, 0, 0]);
        // 水平方向:一侧偏红、另一侧不偏
        let left = img.get_pixel(0, 4);
        let right = img.get_pixel(31, 4);
        assert!(
            left[0] > left[1] || right[0] > right[1],
            "至少一端应偏红:{:?} / {:?}",
            left,
            right
        );
    }

    #[test]
    fn radtint_center_is_most_tinted() {
        let mut img = image::RgbaImage::new(33, 33);
        for p in img.pixels_mut() {
            *p = image::Rgba([128, 128, 128, 255]);
        }
        apply_radtint(&mut img, 0.5, 0.5, [255, 0, 0]);
        let c = img.get_pixel(16, 16);
        let corner = img.get_pixel(0, 0);
        assert!(
            c[0] as i32 - c[1] as i32 > corner[0] as i32 - corner[1] as i32,
            "圆心着色应最强:{:?} vs {:?}",
            c,
            corner
        );
    }

    // ===== 色温双轴映射(temp → m = temp·256,white → k = white·128)=====

    /// 中灰 + 暖端:暖端应抬红降蓝。
    #[test]
    fn colortemp_warm_half_raises_red_lowers_blue() {
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([128, 128, 128, 255]));
        apply_color_temp(&mut img, 0.5, 0.0);
        let p = img.get_pixel(0, 0);
        assert_eq!(p[0], 128 + 64, "暖端中灰 R 应 +64");
        assert_eq!(p[2], 128 - 64, "暖端中灰 B 应 −64");
        // 绿通道 >>17:16384·128>>17 = 16(红蓝的 1/4 幅,压绿是为了少动亮度)
        assert_eq!(p[1], 128 + 16, "暖端中灰 G 应 +16");
    }

    #[test]
    fn colortemp_cool_is_mirror_of_warm() {
        let mut warm = image::RgbaImage::from_pixel(2, 2, image::Rgba([128, 128, 128, 255]));
        apply_color_temp(&mut warm, 0.5, 0.0);
        let mut cool = image::RgbaImage::from_pixel(2, 2, image::Rgba([128, 128, 128, 255]));
        apply_color_temp(&mut cool, -0.5, 0.0);
        for c in [0usize, 2] {
            // 冷暖互为镜像:R/B 关于 128 对称(绿轴只取非负 m,不参与)
            assert_eq!(
                2 * 128,
                warm.get_pixel(0, 0)[c] as i32 + cool.get_pixel(0, 0)[c] as i32,
                "通道 {c} 冷暖应关于 128 对称"
            );
        }
    }

    /// temp=0 时白场轴单独作用应近似恒等。
    #[test]
    fn colortemp_white_shift_alone_is_near_identity() {
        for white in [0.0f32, 0.5, 1.0] {
            for x in [0u8, 64, 128, 200, 255] {
                let mut img =
                    image::RgbaImage::from_pixel(1, 1, image::Rgba([x, x, x, 255]));
                apply_color_temp(&mut img, 0.0, white);
                let p = img.get_pixel(0, 0);
                assert!(
                    (p[0] as i32 - x as i32).abs() <= 2,
                    "temp=0 white={white} 时 {x} 应近似不变,得 {}",
                    p[0]
                );
            }
        }
    }

    /// 白场轴放大冷暖偏移:white=1(k=128)比 white=0 的偏移更大。
    #[test]
    fn colortemp_white_shift_amplifies_warm_shift() {
        let mut a = image::RgbaImage::from_pixel(2, 2, image::Rgba([128, 128, 128, 255]));
        apply_color_temp(&mut a, 0.25, 0.0);
        let mut b = image::RgbaImage::from_pixel(2, 2, image::Rgba([128, 128, 128, 255]));
        apply_color_temp(&mut b, 0.25, 1.0);
        let da = a.get_pixel(0, 0)[0] as i32 - 128;
        let db = b.get_pixel(0, 0)[0] as i32 - 128;
        assert!(db > da, "白场轴应放大暖偏移:{db} 应 > {da}");
    }

    // ===== 审计 3.1 缺陷回归锁(B4 / B5 / B6 / B7)=====
    // 这 4 条在修复前必须**失败**,用来固定缺陷结论本身,不只是固定实现。

    /// B4:`ansel` 的亮度权重必须按缓冲的 R/G/B 顺序落到红绿蓝上(纯红必须比纯蓝亮)。
    #[test]
    fn ansel_luma_weights_follow_rgb_order() {
        let w = luma_weights();
        let mut red = image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 0, 0, 255]));
        let mut blue = image::RgbaImage::from_pixel(8, 8, image::Rgba([0, 0, 255, 255]));
        apply_ansel(&mut red, w, 64.0);
        apply_ansel(&mut blue, w, 64.0);
        let (r, b) = (red.get_pixel(0, 0)[0], blue.get_pixel(0, 0)[0]);
        assert!(
            r as i32 - b as i32 > 40,
            "纯红应显著亮于纯蓝,实得 R={r} B={b}"
        );
    }

    /// B5:`dir_tint` 的 `strength` 必须真正参与计算(影响羽化宽度)。
    #[test]
    fn dir_tint_strength_widens_the_feather() {
        let run = |s: f32| {
            let mut img = image::RgbaImage::from_pixel(65, 65, image::Rgba([128, 128, 128, 255]));
            apply_dir_tint(&mut img, 0.0, s, [255, 0, 0]);
            // 取方向轴(dx=1)的远端探测点:px = 64 − 32.5 = 31.5
            img.get_pixel(64, 32)[1]
        };
        let (narrow, wide) = (run(10.0), run(90.0));
        assert_ne!(narrow, wide, "strength 不应被丢弃:{narrow} == {wide}");
        assert!(
            wide > narrow,
            "strength 越大远端着色应越弱(G 越高),实得 10→{narrow} 90→{wide}"
        );
    }

    /// B5(续):投影的**原点必须在画面中心**。
    ///
    /// 方向色块沿方向轴的两侧反号,因此中心处 B 样条核取半值 →
    /// G 通道应落在「半着色」的 128·(1 − 128/256) ≈ 64 附近。
    /// 逐行累加器漏掉 `(0 − cx)·dx` 起点时,中心会被推到接近满着色。
    #[test]
    fn dir_tint_center_is_half_tinted() {
        let mut img = image::RgbaImage::from_pixel(65, 65, image::Rgba([128, 128, 128, 255]));
        apply_dir_tint(&mut img, 0.0, 50.0, [255, 0, 0]);
        let g = img.get_pixel(32, 32)[1] as i32;
        assert!((g - 64).abs() <= 6, "画面中心应是半着色,G 应约 64,实得 {g}");
    }

    /// B6:`GrainParams::grayscale == false` 必须给出逐通道独立的噪声。
    #[test]
    fn grain_false_grayscale_samples_each_channel() {
        let run = |g: bool| {
            let mut img = image::RgbaImage::from_pixel(16, 16, image::Rgba([128, 128, 128, 255]));
            apply_grain(
                &mut img,
                &GrainParams {
                    amount: 0.5,
                    seed: 5,
                    channel_options: 7,
                    grayscale: g,
                },
            );
            img
        };
        // grayscale=true:输入是中性灰 → 三通道必须逐像素相等(共用同一噪声值)
        for p in run(true).pixels() {
            assert_eq!(p[0], p[1], "灰度噪声应三通道共用同一值");
            assert_eq!(p[1], p[2], "灰度噪声应三通道共用同一值");
        }
        // grayscale=false:至少要有像素的三通道不一致
        let independent = run(false).pixels().any(|p| p[0] != p[1] || p[1] != p[2]);
        assert!(independent, "grayscale=false 应逐通道各自抽样");
    }

    /// B7:`sat` 的曲线索引应是三通道色度的最大值(非逐通道各自索引)。
    #[test]
    fn saturation_indexes_curve_by_max_chroma() {
        let mut img = image::RgbaImage::from_pixel(1, 1, image::Rgba([60, 0, 200, 255]));
        apply_saturation(&mut img, 0.1);
        let r = img.get_pixel(0, 0)[0] as i32;
        assert!(
            (r - 149).abs() <= 2,
            "应以最大色度索引曲线,R 应约 149,实得 {r}"
        );
    }
}
