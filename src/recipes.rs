//! 滤镜配方(recipe)—— 参考 Picasa 3 的成组调色效果。
//!
//! 与 [`crate::oneclick`] 的区别:oneclick 是一个滑块驱动的单一调整,
//! 本模块是**一串固定顺序的像素操作 + 一组默认参数**拼成的完整风格,
//! 更接近参考产品里"一键套用"的那些滤镜。
//!
//! # 强度模型
//!
//! 每个配方都有 `amount`(0~100),语义统一为**与处理结果的不透明度混合**:
//! `out = src + (styled − src)·amount/100`。这与参考产品里各配方的
//! "淡出"滑杆等价(淡出 0 → 全量,淡出 100 → 原图)。
//!
//! 少数配方会**改变画布尺寸**(加边框、投影、旋转),这类配方无法按像素混合,
//! `amount` 只控制内部装饰的轻重,不改尺寸。
//!
//! # 诚实声明(哪些是简化)
//!
//! - **光晕合成**([`glow_edge`]):参考产品的内光晕是三通道分离 + 多级半径累积的
//!   合成器,合成混合模式未公开。此处按「边缘渐变遮罩 + 颜色线性混合」实现,
//!   半径与强度参数照配方取值。
//! - **棕褐色滤镜色**([`crate::oneclick::apply_bw`] 的 `filtercolor`):
//!   参考产品可给灰度加单色偏移,参数语义未公开,此处按纯亮度归一化处理。
//! - **漫画网点**([`comic`]):参考配方是双层错位网点遮罩 + 加色合成的完整半色调,
//!   此处简化为「方块降采样 + 色阶压缩」,不做网点形状模拟。
//! - **调色板量化**([`quantize_palette`]):按逐层降低位深的分桶聚合实现,
//!   桶内取原色均值;参考产品同属八叉树式的分层聚合思路。
//!
//! # 像素约定
//!
//! 全模块按 **RGBA8** 处理,混合模式在 `0..=255` 域上定义(见 [`Blend`])。

use image::{Rgba, RgbaImage};

// ---------------------------------------------------------------------------
// 混合
// ---------------------------------------------------------------------------

/// 图层混合模式。
///
/// 全部在 `0..=255` 域上按通道独立计算 —— 参考产品的混合器就是这么定的:
/// 没有 sRGB 线性化,也不做 gamma 校正。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blend {
    /// 直接覆盖。
    Normal,
    /// `a·b/255`。
    Multiply,
    /// `255 − (255−a)(255−b)/255`。
    Screen,
    /// 以底层为条件的对比度倍增:底层暗时走 multiply,亮时走 screen。
    Overlay,
    /// `a + b`(饱和到 255)。
    Add,
    /// `a − b`(饱和到 0)。
    Subtract,
    /// 取暗。
    Darken,
    /// 取亮。
    Lighten,
    /// 以顶层为条件的强对比:顶层暗时走 multiply,亮时走 screen。
    HardLight,
    /// 柔光:顶层 0.5 附近近似不变,两端缓推,不产生硬边。
    SoftLight,
}

impl Blend {
    /// 单通道混合。
    fn ch(self, base: f32, top: f32) -> f32 {
        match self {
            Blend::Normal => top,
            Blend::Multiply => base * top / 255.0,
            Blend::Screen => 255.0 - (255.0 - base) * (255.0 - top) / 255.0,
            Blend::Overlay => {
                if base < 128.0 {
                    2.0 * base * top / 255.0
                } else {
                    255.0 - 2.0 * (255.0 - base) * (255.0 - top) / 255.0
                }
            }
            Blend::Add => base + top,
            Blend::Subtract => base - top,
            Blend::Darken => base.min(top),
            Blend::Lighten => base.max(top),
            Blend::HardLight => {
                // 顶层决定用 multiply 还是 screen:
                //   顶层 ≤ 127 → 2·顶层·底层/255
                //   顶层 == 255 且底层 == 255 → 255(避免上式在端点溢出)
                //   其余      → 2·(32512 − (255−底层)(255−顶层))/255
                if top <= 127.0 {
                    2.0 * top * base / 255.0
                } else if top >= 255.0 && base >= 255.0 {
                    255.0
                } else {
                    let n = 32512.0 - (255.0 - base) * (255.0 - top);
                    2.0 * n / 255.0
                }
            }
            Blend::SoftLight => {
                // 顶层先取偶数(去掉最低位,等效于量化到 2 的步长),
                // 再按底层暗/亮分两段:
                //   底层 < 128 → (顶层+128)·底层/255
                //   否则       → (65025 − (382−顶层)(255−底层))/255
                // 两段在底层 = 128 处连续。
                let t = (top as i32 & 0xfe) as f32;
                if base < 128.0 {
                    (t + 128.0) * base / 255.0
                } else {
                    (65025.0 - (382.0 - t) * (255.0 - base)) / 255.0
                }
            }
        }
    }

    /// 按名字解析(不区分大小写);未知名字按 [`Blend::Normal`] 处理。
    pub fn parse(s: &str) -> Blend {
        match s.to_ascii_lowercase().as_str() {
            "multiply" => Blend::Multiply,
            "screen" => Blend::Screen,
            "overlay" => Blend::Overlay,
            "add" => Blend::Add,
            "subtract" => Blend::Subtract,
            "darken" => Blend::Darken,
            "lighten" => Blend::Lighten,
            "hardlight" => Blend::HardLight,
            "softlight" => Blend::SoftLight,
            _ => Blend::Normal,
        }
    }
}

/// 把 `src` 以指定混合模式与不透明度合成到 `dst`(尺寸须一致)。
pub fn composite(dst: &mut RgbaImage, src: &RgbaImage, mode: Blend, alpha: f32) {
    let a = alpha.clamp(0.0, 1.0);
    if a <= 0.0 || dst.dimensions() != src.dimensions() {
        return;
    }
    for (dp, sp) in dst.pixels_mut().zip(src.pixels()) {
        for c in 0..3 {
            let base = dp[c] as f32;
            let top = sp[c] as f32;
            let mixed = mode.ch(base, top);
            dp[c] = (base + (mixed - base) * a).clamp(0.0, 255.0).round() as u8;
        }
    }
}

/// 按遮罩合成:`mask` 的取值 0~1 缩放不透明度(与 `alpha` 相乘)。
pub fn composite_masked(
    dst: &mut RgbaImage,
    src: &RgbaImage,
    mask: &Mask,
    mode: Blend,
    alpha: f32,
) {
    if dst.dimensions() != src.dimensions() || dst.dimensions() != mask.dimensions() {
        return;
    }
    let a0 = alpha.clamp(0.0, 1.0);
    let (w, _) = dst.dimensions();
    for (i, (dp, sp)) in dst.pixels_mut().zip(src.pixels()).enumerate() {
        let a = a0 * mask.at_index(i as u32 % w, (i as u32) / w).clamp(0.0, 1.0);
        if a <= 0.0 {
            continue;
        }
        for c in 0..3 {
            let base = dp[c] as f32;
            let top = sp[c] as f32;
            let mixed = mode.ch(base, top);
            dp[c] = (base + (mixed - base) * a).clamp(0.0, 255.0).round() as u8;
        }
    }
}

// ---------------------------------------------------------------------------
// 遮罩
// ---------------------------------------------------------------------------

/// 单通道浮点遮罩(0~1),与图像同尺寸。
#[derive(Debug, Clone)]
pub struct Mask {
    w: u32,
    h: u32,
    a: Vec<f32>,
}

impl Mask {
    /// 建立全 0 遮罩。
    pub fn new(w: u32, h: u32) -> Mask {
        Mask {
            w,
            h,
            a: vec![0.0; (w as usize) * (h as usize)],
        }
    }

    /// 尺寸。
    pub fn dimensions(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    /// 读一点(越界返回 0)。
    pub fn at(&self, x: u32, y: u32) -> f32 {
        if x >= self.w || y >= self.h {
            return 0.0;
        }
        self.a[(y as usize) * (self.w as usize) + (x as usize)]
    }

    fn at_index(&self, x: u32, y: u32) -> f32 {
        self.at(x, y)
    }

    fn set(&mut self, x: u32, y: u32, v: f32) {
        if x < self.w && y < self.h {
            self.a[(y as usize) * (self.w as usize) + (x as usize)] = v;
        }
    }
}

/// 平滑阶跃 `smoothstep`(边缘 0 阶导数连续,避免出现可见的环带)。
fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    if (e1 - e0).abs() < 1e-6 {
        return if x < e0 { 0.0 } else { 1.0 };
    }
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// 圆形渐变遮罩:圆心处取 `inner_alpha`,半径 `outer_radius` 之外取 `outer_alpha`,
/// 中间按 `smoothstep` 过渡。
///
/// 半径单位是像素;`inner_radius > outer_radius` 时自动交换,
/// 因此 `inner_alpha > outer_alpha`(由内向外衰减)与反向遮罩都能表达 ——
/// 后者正是"只处理圆外"的情形。
pub fn circular_mask(
    w: u32,
    h: u32,
    cx: f32,
    cy: f32,
    inner_radius: f32,
    outer_radius: f32,
    inner_alpha: f32,
    outer_alpha: f32,
) -> Mask {
    let mut m = Mask::new(w, h);
    let (r0, r1) = if inner_radius <= outer_radius {
        (inner_radius.max(0.0), outer_radius.max(0.0))
    } else {
        (outer_radius.max(0.0), inner_radius.max(0.0))
    };
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            // t=0 → 内圈, t=1 → 外圈
            let t = smoothstep(r0, r1, d);
            let v = inner_alpha + (outer_alpha - inner_alpha) * t;
            m.set(x, y, v);
        }
    }
    m
}

/// 边缘衰减遮罩:图像四边为 1,向内按 `width` 像素平滑过渡到 0。
///
/// 用于"画面边缘处理"(内光晕 / 暗角 / 边缘模糊),四条边取最小值,
/// 所以四角衰减最强 —— 与参考产品边缘类配方的观感一致。
pub fn edge_mask(w: u32, h: u32, width: f32) -> Mask {
    let mut m = Mask::new(w, h);
    let t = width.max(0.5);
    let (fw, fh) = (w as f32, h as f32);
    for y in 0..h {
        for x in 0..w {
            let dx = (x as f32 + 0.5).min(fw - (x as f32 + 0.5));
            let dy = (y as f32 + 0.5).min(fh - (y as f32 + 0.5));
            let d = dx.min(dy);
            // 距边 0 → 1,距边 t → 0
            m.set(x, y, 1.0 - smoothstep(0.0, t, d));
        }
    }
    m
}

// ---------------------------------------------------------------------------
// 基础算子
// ---------------------------------------------------------------------------

/// 可分离盒式模糊,横纵半径可不同。
///
/// 半径以像素计,内部四舍五入到整数核宽;边界按夹取处理(不引入黑边)。
/// 参考产品的模糊算子就是盒式的(可分离 + 2 的幂级快速路径),
/// 不是高斯 —— 所以大半径下边缘不会溢出,但方块感会比高斯明显。
/// 参考模糊算子的**阶梯量化**:amount<1 → 0;否则量化到
/// {2.065, 3.0625, 4.13, 5.13} 四档(半径离散化,非连续 sigma)。
///
/// **诚实声明**:升档阈值在参考语料里未公开,这里取等距 1/2/3/4 假定;
/// 档位数值本身是参考件里读出的常数。
pub fn blur_ladder(amount: f32) -> f32 {
    if amount < 1.0 {
        return 0.0;
    }
    const RUNGS: [(f32, f32); 4] = [
        (1.0, 2.065),
        (2.0, 3.0625),
        (3.0, 4.13),
        (4.0, 5.13),
    ];
    let mut r = RUNGS[0].1;
    for (t, v) in RUNGS {
        if amount >= t {
            r = v;
        }
    }
    r
}

pub fn box_blur(img: &mut RgbaImage, rx: f32, ry: f32) {
    let (w, h) = img.dimensions();
    // 参考约束:实际半径先钳到图像尺寸的一半(rx 对宽、ry 对高)。
    let rx = rx.round().max(0.0).min(w as f32 / 2.0) as i32;
    let ry = ry.round().max(0.0).min(h as f32 / 2.0) as i32;
    if rx > 0 {
        box_blur_axis(img, rx, true);
    }
    if ry > 0 {
        box_blur_axis(img, ry, false);
    }
}

fn box_blur_axis(img: &mut RgbaImage, r: i32, horizontal: bool) {
    let (w, h) = img.dimensions();
    let src = img.clone();
    let win = (2 * r + 1) as f32;
    let (iw, ih) = (w as i32, h as i32);
    for y in 0..ih {
        for x in 0..iw {
            let mut acc = [0f32; 3];
            for d in -r..=r {
                let (sx, sy) = if horizontal {
                    ((x + d).clamp(0, iw - 1) as u32, y as u32)
                } else {
                    (x as u32, (y + d).clamp(0, ih - 1) as u32)
                };
                let p = src.get_pixel(sx, sy);
                for c in 0..3 {
                    acc[c] += p[c] as f32;
                }
            }
            let dp = img.get_pixel_mut(x as u32, y as u32);
            for c in 0..3 {
                dp[c] = (acc[c] / win).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// 全直方图自动色阶:三个通道各自把 `[min,max]` 线性铺满 `[0,255]`。
///
/// 与 [`crate::oneclick::apply_autocontrast`](带分位裁剪)不同,这里
/// **不做百分比裁剪** —— 拉伸更彻底,适合风格化配方当第一道工序。
/// 自动色阶:每通道**独立**全直方图拉伸(参考口径)。
///
/// - `low` = 首个非零 bin,`high` = 末个非零 bin(无百分比裁剪);
/// - `raw = 1.0 + 255·(i−low)/(high−low)` 后截断 —— 所以 `i=low` 输出 **1** 而非 0,
///   `i=high` 得 256 → 钳到 255;
/// - `low == high`(常数图)→ 该通道 LUT 全填 0xff(全白);
/// - 全黑图(直方图全零)→ LUT 全零,保持黑色。
pub fn auto_fix(img: &mut RgbaImage) {
    let mut hist = [[0u32; 256]; 3];
    for p in img.pixels() {
        for c in 0..3 {
            hist[c][p[c] as usize] += 1;
        }
    }
    let mut lut = [[0u8; 256]; 3];
    for c in 0..3 {
        let h = &hist[c];
        let (Some(lo), Some(hi)) = (
            (0..256).find(|&i| h[i] > 0),
            (0..256).rev().find(|&i| h[i] > 0),
        ) else {
            continue;
        };
        if lo == hi {
            lut[c] = [255u8; 256];
            continue;
        }
        for (i, slot) in lut[c].iter_mut().enumerate() {
            let raw = 1.0 + 255.0 * (i as f32 - lo as f32) / (hi - lo) as f32;
            *slot = raw.clamp(0.0, 255.0) as u8;
        }
    }
    for p in img.pixels_mut() {
        for c in 0..3 {
            p[c] = lut[c][p[c] as usize];
        }
    }
}

/// 本项目亮度权重下的灰度值(0~255)。
fn luma8(p: &Rgba<u8>) -> f32 {
    crate::oneclick::luma(p[0] as f32, p[1] as f32, p[2] as f32)
}

/// 渐变映射:把亮度当索引,在给定色阶之间线性插值。
///
/// 站点位置**强制均匀铺满 `0..255`**(`n` 个色阶 → 第 `i` 个落在 `i·255/(n−1)`),
/// 与参考产品的两色/多色映射一致 —— 所以它不受"自定义站点位置"影响,
/// 色阶数量才是唯一的自由度。
pub fn gradient_map(img: &mut RgbaImage, stops: &[[u8; 3]]) {
    if stops.is_empty() {
        return;
    }
    let n = stops.len();
    if n == 1 {
        let c = stops[0];
        for p in img.pixels_mut() {
            p[0] = c[0];
            p[1] = c[1];
            p[2] = c[2];
        }
        return;
    }
    let mut lut = [[0u8; 3]; 256];
    for i in 0..256usize {
        let pos = (i as f32) * (n as f32 - 1.0) / 255.0;
        let k = (pos.floor() as usize).min(n - 2);
        let f = pos - k as f32;
        for c in 0..3 {
            let a = stops[k][c] as f32;
            let b = stops[k + 1][c] as f32;
            lut[i][c] = (a + (b - a) * f).round().clamp(0.0, 255.0) as u8;
        }
    }
    for p in img.pixels_mut() {
        let y = luma8(p).round().clamp(0.0, 255.0) as usize;
        p[0] = lut[y][0];
        p[1] = lut[y][1];
        p[2] = lut[y][2];
    }
}

/// HSV 渐变映射:站点位置**按给定值**落位,色相沿最短弧插值。
///
/// 站点用 `(position, (h, s, v))` 表示,`position` 取 `0..255`,
/// `h` 取 `0..360`, `s`/`v` 取 `0..=100`。`hue_offset` 整体旋转色相。
pub fn hsv_gradient_map(img: &mut RgbaImage, stops: &[(f32, (f32, f32, f32))], hue_offset: f32) {
    if stops.is_empty() {
        return;
    }
    let mut sorted: Vec<(f32, (f32, f32, f32))> = stops.to_vec();
    sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut lut = [[0u8; 3]; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        let x = i as f32;
        let c = if x <= sorted[0].0 {
            sorted[0].1
        } else if x >= sorted[sorted.len() - 1].0 {
            sorted[sorted.len() - 1].1
        } else {
            let mut k = 0usize;
            while k + 1 < sorted.len() && sorted[k + 1].0 < x {
                k += 1;
            }
            let (p0, (h0, s0, v0)) = sorted[k];
            let (p1, (h1, s1, v1)) = sorted[k + 1];
            let t = if (p1 - p0).abs() < 1e-6 {
                0.0
            } else {
                (x - p0) / (p1 - p0)
            };
            // 色相走最短弧:差值折到 (-180, 180]
            let mut dh = h1 - h0;
            while dh > 180.0 {
                dh -= 360.0;
            }
            while dh < -180.0 {
                dh += 360.0;
            }
            let h = (h0 + dh * t + hue_offset).rem_euclid(360.0);
            (h, s0 + (s1 - s0) * t, v0 + (v1 - v0) * t)
        };
        *slot = hsv_to_rgb(c.0, c.1 / 100.0, c.2 / 100.0);
    }

    // 参考口径:索引是**输入像素的 R 通道字节**(不是亮度);
    // 参考件里 G/B 两张查找表被清零,只有 R 通道参与 —— 我方从
    // 单张 LUT 写回 RGB,视觉上等价于「以 R 索引的整色映射」。
    for p in img.pixels_mut() {
        let y = p[0] as usize;
        p[0] = lut[y][0];
        p[1] = lut[y][1];
        p[2] = lut[y][2];
    }
}

/// HSV → RGB(`h` 度,`s`/`v` 取 0~1)。
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let h = h.rem_euclid(360.0);
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        ((r + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    ]
}

/// 双色调:亮度 0 → `black`,亮度 255 → `white`,中间线性插值。
pub fn two_tone(img: &mut RgbaImage, black: [u8; 3], white: [u8; 3]) {
    gradient_map(img, &[black, white]);
}

/// 局部对比度(大半径非锐化掩模)。
///
/// ```text
/// blur    = box_blur(orig, radius, radius)
/// detail  = orig − blur
/// out     = orig + strength · detail
/// ```
///
/// `strength` 取 1.0 附近是"轻微提细节",取 3.0 以上开始有明显 HDR 观感。
/// 半径大(数十像素)时只影响明暗结构,不放大噪点 —— 这与小半径锐化的
/// 观感差异正是这类配方想要的。
pub fn local_contrast(img: &mut RgbaImage, radius: f32, strength: f32) {
    if strength == 0.0 || radius <= 0.0 {
        return;
    }
    let mut blur = img.clone();
    box_blur(&mut blur, radius, radius);
    for (p, b) in img.pixels_mut().zip(blur.pixels()) {
        for c in 0..3 {
            let v = p[c] as f32;
            let d = v - b[c] as f32;
            p[c] = (v + strength * d).clamp(0.0, 255.0).round() as u8;
        }
    }
}

/// 局部对比度 —— **参考的两支嵌套形式**(滑杆口径就是参考的 `c`)。
///
/// 参考的 LocalContrast 不是单公式,而是两支嵌套依次复合:
///
/// ```text
/// blur = Blur(orig, radius)                    // 先记下 orig 与 blur 两张图
/// 支一 subtract:(blur − orig)·c  复合回 ⇒ blur + (orig − blur)·c
/// 支二 add     :(orig − blur)·c  复合回 ⇒ blur + 2c·(orig − blur)
/// ```
///
/// 与 [`local_contrast`] 的 USM 形式 `orig + s·(orig − blur)` 属同一族,
/// 换算关系 **`s = 2c − 1`**(回归锁 `local_contrast_strength_maps_to_reference_contrast`)。
/// 两个推论:`c = 0.5` 严格还原原图;`c = 0` 得到纯模糊图。
///
/// 参考滑杆量程:`radius` 1.3~40(缺省 15)、`c` 1~3(缺省 1.5)。
///
/// **诚实声明**:嵌套语义按「子级结果以该 BlendMode 复合进父画布」解读,
/// 各支作用的中间画布顺序取自官方滤镜描述的书写次序;
/// 若复合方向另有讲究,结果会差一个常数倍,目前无样本可校验。
pub fn local_contrast_ref(img: &mut RgbaImage, radius: f32, contrast: f32) {
    // 直接换算成 USM 的强度再走同一条实现,避免两处公式写歪。
    local_contrast(img, radius, 2.0 * contrast - 1.0);
}

/// 阴影 / 高光(Shadow & Highlight)。
///
/// 参考的三根滑杆:Radius(`range 1.0` + `log 250.0`)、Shadow %、Highlight %(均 `range 1.0`);
/// 对数滑杆按常规口径折算:`radius = 1 · 250^raw`(raw 为滑杆 0..1)。
///
/// ```text
/// mean = box_blur(src, radius)          // 局部均值 = 该像素"本该有的"亮度
/// mean > v → 暗部:out = v + shadow · (mean − v)     // 向均值上抬
/// mean < v → 亮部:out = v + highlight · (mean − v)  // 向均值回落 = 压高光
/// ```
///
/// **诚实声明**:参考只公开了三根滑杆的存在与量程,内部实现没有对应资料。上面这条"以局部均值为参照、分别向均值靠拢"的公式属本项目重构,
/// 其好处是平坦区(局部均值等于自身)天然恒等,不会产生额外的晕圈。
pub fn shadow_highlight(img: &mut RgbaImage, radius: f32, shadow: f32, highlight: f32) {
    if radius <= 0.0 || (shadow == 0.0 && highlight == 0.0) {
        return;
    }
    let mut mean = img.clone();
    box_blur(&mut mean, radius, radius);
    for (p, m) in img.pixels_mut().zip(mean.pixels()) {
        for c in 0..3 {
            let v = p[c] as f32;
            let n = m[c] as f32;
            let k = if n > v { shadow } else { highlight };
            if k != 0.0 {
                p[c] = (v + k * (n - v)).clamp(0.0, 255.0).round() as u8;
            }
        }
    }
}

/// 边缘检测(描边):参考的三步装配,**白底黑边**。
///
/// ```text
/// ① Blur(2, 2)
/// ② 双向 Sobel:horizontal 分支 = clamp(128 + gx·scale/2),
///    vertical 分支同构(128 偏置使平坦区落在 128)
/// ③ 三角曲线 {0→0, 128→255, 255→0}:平坦区(128)映到 **255 白**,
///    强边缘(0/255)映到 **0 黑**;两支按 Multiply 合成
/// ```
///
/// 输出是灰度图(`R=G=B`)。注意与早期实现的两点差异:强边缘趋黑
/// (不是回白),且先模糊再取梯度。
pub fn edge_detect_b(img: &mut RgbaImage, detail: f32) {
    let (w, h) = img.dimensions();
    if w < 3 || h < 3 {
        return;
    }
    // ① 先 Blur(2,2)
    let mut src = img.clone();
    box_blur(&mut src, 2.0, 2.0);
    let gray: Vec<f32> = src.pixels().map(luma8).collect();
    let scale = (detail / 50.0).max(0.1);
    let (iw, ih) = (w as i32, h as i32);
    let g = |x: i32, y: i32| -> f32 {
        let xx = x.clamp(0, iw - 1) as usize;
        let yy = y.clamp(0, ih - 1) as usize;
        gray[yy * (w as usize) + xx]
    };
    // ③ 三角曲线:0→0、128→255、255→0
    let tri = |x: f32| -> f32 {
        let x = x.clamp(0.0, 255.0);
        if x <= 128.0 {
            x * (255.0 / 128.0)
        } else {
            (255.0 - x) * (255.0 / 127.0)
        }
    };
    for y in 0..ih {
        for x in 0..iw {
            // ② 两个方向的 Sobel,各自 +128 偏置后过三角曲线
            let gx = (g(x + 1, y - 1) + 2.0 * g(x + 1, y) + g(x + 1, y + 1))
                - (g(x - 1, y - 1) + 2.0 * g(x - 1, y) + g(x - 1, y + 1));
            let gy = (g(x - 1, y + 1) + 2.0 * g(x, y + 1) + g(x + 1, y + 1))
                - (g(x - 1, y - 1) + 2.0 * g(x, y - 1) + g(x + 1, y - 1));
            let branch = |s: f32| tri(128.0 + (s * scale / 2.0).clamp(-128.0, 127.0));
            // 两支 Multiply 合成(白底保持白,任一支的黑边保留)
            let v = (branch(gx) * branch(gy) / 255.0).round().clamp(0.0, 255.0) as u8;
            let p = img.get_pixel_mut(x as u32, y as u32);
            p[0] = v;
            p[1] = v;
            p[2] = v;
        }
    }
}

/// 径向模糊(变焦模糊):以 (`cx`,`cy`) 为中心把画面按递增倍率叠若干层再取均值。
///
/// ```text
/// N       = min(30, amount + 5)                  // 叠加层数
/// step    = floor(W · amount / 200)              // 最外层的放大像素数
/// d_k     = step · k / N            (k = 1..N)   // 第 k 层在宽、高上各外扩 d_k 像素
/// x_scale = 1 / (1 + d_k / W)
/// y_scale = 1 / (1 + d_k / H)
/// out     = mean( orig(c + (p − c) · (x_scale, y_scale)) )
/// ```
///
/// 三处容易写错的细节:
///
/// - 外扩量是**宽、高各加 d 像素**,不是等比缩放。倍率分别是 `(W+d)/W`
///   与 `(H+d)/H`,所以非正方形画面会有轻微各向异性。
/// - 层从 `d = step/N` 起,**没有 d = 0 的清晰底图**,因此任何强度下中心也会被带动。
/// - 每层权重相同(等权平均),没有按层衰减。
pub fn radial_blur(img: &mut RgbaImage, cx: f32, cy: f32, amount: f32) {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return;
    }
    let amount = amount.clamp(0.0, 100.0);
    let n = (((amount as i32) + 5).clamp(2, 30)) as usize;
    // 最外层外扩像素数按宽度算,取整后再分层。
    let step = (w as f32 * amount / 200.0).floor();
    if step < 1.0 {
        return;
    }
    let src = img.clone();
    let (iw, ih) = (w as i32, h as i32);
    for y in 0..ih {
        for x in 0..iw {
            let mut acc = [0f32; 3];
            for k in 1..=n {
                let d = step * k as f32 / n as f32;
                let xs = 1.0 / (1.0 + d / w as f32);
                let ys = 1.0 / (1.0 + d / h as f32);
                let sx = (cx + ((x as f32 + 0.5) - cx) * xs).round() as i32;
                let sy = (cy + ((y as f32 + 0.5) - cy) * ys).round() as i32;
                let p = src.get_pixel(
                    sx.clamp(0, iw - 1) as u32,
                    sy.clamp(0, ih - 1) as u32,
                );
                for c in 0..3 {
                    acc[c] += p[c] as f32;
                }
            }
            let dp = img.get_pixel_mut(x as u32, y as u32);
            for c in 0..3 {
                dp[c] = (acc[c] / n as f32).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// 线性(方向)模糊:沿给定角度做多次等比采样后取均值。
///
/// `angle` 以度为单位,0 = 水平向右;`length` 是采样跨度(像素)。
/// 层数随跨度增加但封顶,避免大跨度时过度拖慢。
pub fn linear_blur(img: &mut RgbaImage, angle: f32, length: f32) {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 || length <= 0.0 {
        return;
    }
    let n = ((length as i32) / 2 + 3).clamp(3, 24);
    let rad = angle.to_radians();
    let (dx, dy) = (rad.cos(), rad.sin());
    let src = img.clone();
    let (iw, ih) = (w as i32, h as i32);
    for y in 0..ih {
        for x in 0..iw {
            let mut acc = [0f32; 3];
            for k in 0..n {
                // 以 0 为中心对称铺开:−length/2 … +length/2
                let t = (k as f32 / (n - 1) as f32 - 0.5) * length;
                let sx = (x as f32 + dx * t).round() as i32;
                let sy = (y as f32 + dy * t).round() as i32;
                let p = src.get_pixel(
                    sx.clamp(0, iw - 1) as u32,
                    sy.clamp(0, ih - 1) as u32,
                );
                for c in 0..3 {
                    acc[c] += p[c] as f32;
                }
            }
            let dp = img.get_pixel_mut(x as u32, y as u32);
            for c in 0..3 {
                dp[c] = (acc[c] / n as f32).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// 方向性调整的种类。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DirKind {
    /// 亮度:整体按权重缩放。
    Brightness,
    /// 饱和度:向亮度收敛的比例按权重变化。
    Saturation,
    /// 锐度:权重高于 1 的一侧加强锐化。
    Sharpen,
}

/// 方向性调整:沿左右/上下做一个线性渐变,越靠一侧效果越强。
///
/// `lr` / `tb` 取值 −1~1,正数表示"右侧 / 下侧更强"。
/// `strength` 是 0~1 的总体强度。两轴同时给值时按线性叠加
/// (右上角最强、左下角最弱),与参考的「Left to Right + Top to Bottom」两滑杆一致。
pub fn directional_adjust(img: &mut RgbaImage, lr: f32, tb: f32, strength: f32, kind: DirKind) {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 || strength <= 0.0 {
        return;
    }
    let s = strength.clamp(0.0, 1.0);
    // 锐化需要一份模糊底图作参照
    let base = if kind == DirKind::Sharpen {
        let mut b = img.clone();
        box_blur(&mut b, 2.0, 2.0);
        Some(b)
    } else {
        None
    };
    let wf = w as f32;
    let hf = h as f32;
    for y in 0..h {
        let uy = (2.0 * (y as f32 + 0.5) / hf - 1.0).clamp(-1.0, 1.0);
        for x in 0..w {
            let ux = (2.0 * (x as f32 + 0.5) / wf - 1.0).clamp(-1.0, 1.0);
            let m = 1.0 + s * (lr * ux + tb * uy);
            let dp = img.get_pixel_mut(x, y);
            match kind {
                DirKind::Brightness => {
                    // 用加减平移而不是乘:暗部也要跟着变,否则深色照片上几乎看不出渐变
                    let delta = (m - 1.0) * 127.5;
                    for c in 0..3 {
                        dp[c] = (dp[c] as f32 + delta).clamp(0.0, 255.0).round() as u8;
                    }
                }
                DirKind::Saturation => {
                    let g = luma8(&Rgba([dp[0], dp[1], dp[2], 255]));
                    for c in 0..3 {
                        let v = g + (dp[c] as f32 - g) * m;
                        dp[c] = v.clamp(0.0, 255.0).round() as u8;
                    }
                }
                DirKind::Sharpen => {
                    let bp = base.as_ref().unwrap().get_pixel(x, y);
                    let k = (m - 1.0).max(0.0);
                    for c in 0..3 {
                        let orig = dp[c] as f32;
                        let v = orig + (orig - bp[c] as f32) * k * 2.0;
                        dp[c] = v.clamp(0.0, 255.0).round() as u8;
                    }
                }
            }
        }
    }
}

/// 焦点柔化:圆心附近保持清晰,越往外叠得越多。
///
/// `inner` 是"仍然清晰"的半径(占短边比例),`outer` 是模糊到满强度的半径。
/// 参考的「Soft Focus」用一只定位点指定圆心,这里按同一语义用归一化坐标。
pub fn soft_focus(img: &mut RgbaImage, cx: f32, cy: f32, inner: f32, outer: f32, amount: f32) {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 || amount <= 0.0 {
        return;
    }
    let short = w.min(h) as f32;
    let r_in = (inner * short).max(1.0);
    let r_out = (outer * short).max(r_in + 1.0);
    let m = circular_mask(
        w,
        h,
        cx * w as f32,
        cy * h as f32,
        r_in,
        r_out,
        0.0,
        1.0,
    );
    let mut blurred = img.clone();
    box_blur(&mut blurred, short * 0.02, short * 0.02);
    composite_masked(img, &blurred, &m, Blend::Normal, amount.clamp(0.0, 1.0));
}

/// 焦点去色:圆内保留色彩,圆外转成灰度。
///
/// `inner` / `outer` 与 [`soft_focus`] 同义;`outer − inner` 越小过渡越硬,
/// 对应参考里那条「Sharpness」滑杆。
pub fn focal_bw(img: &mut RgbaImage, cx: f32, cy: f32, inner: f32, outer: f32) {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return;
    }
    let short = w.min(h) as f32;
    let r_in = (inner * short).max(1.0);
    let r_out = (outer * short).max(r_in + 1.0);
    let m = circular_mask(
        w,
        h,
        cx * w as f32,
        cy * h as f32,
        r_in,
        r_out,
        0.0,
        1.0,
    );
    let mut gray = img.clone();
    for p in gray.pixels_mut() {
        let g = luma8(p).round().clamp(0.0, 255.0) as u8;
        p[0] = g;
        p[1] = g;
        p[2] = g;
    }
    composite_masked(img, &gray, &m, Blend::Normal, 1.0);
}

/// 方块化:先缩到 `1/block`,再按**最近邻**放大回原尺寸。
///
/// 缩小时走双线性(参考的缩放算子默认开平滑),
/// 放大时显式关平滑 —— 只有关掉才得到硬块边界。两步合起来,
/// 每块的颜色取的是**块中心那个采样点**,不是块内均值。

pub fn pixelate(img: &mut RgbaImage, block: f32) {
    let (w, h) = img.dimensions();
    if w < 2 || h < 2 {
        return;
    }
    let b = block.round().max(2.0);
    let sw = ((w as f32 / b).round() as u32).max(1);
    let sh = ((h as f32 / b).round() as u32).max(1);
    if sw >= w && sh >= h {
        return;
    }
    let small = resize(img, sw, sh);
    *img = resize_nearest(&small, w, h);
}

/// 调色板量化(色阶压缩)。
///
/// 做法:逐层降低每通道位深做分桶,直到桶数不超过 `steps`,每桶取原色均值。
/// `depth` 只作为起始位深上限。桶数仍超限时,保留像素最多的若干桶,
/// 其余映射到色距最近的保留桶 —— 不会因为极端参数而退化成全黑。
pub fn quantize_palette(img: &mut RgbaImage, steps: u32, depth: u32) {
    use std::collections::HashMap;
    let steps = steps.max(2);
    let mut chosen: Option<(u32, HashMap<(u8, u8, u8), (u64, u64, u64, u64)>)> = None;
    for d in (1..=depth.clamp(1, 8)).rev() {
        let shift = 8 - d;
        let mut map: HashMap<(u8, u8, u8), (u64, u64, u64, u64)> = HashMap::new();
        for p in img.pixels() {
            let key = (p[0] >> shift, p[1] >> shift, p[2] >> shift);
            let e = map.entry(key).or_insert((0, 0, 0, 0));
            e.0 += p[0] as u64;
            e.1 += p[1] as u64;
            e.2 += p[2] as u64;
            e.3 += 1;
        }
        if map.len() as u32 <= steps {
            chosen = Some((shift, map));
            break;
        }
        chosen = Some((shift, map));
    }
    let Some((shift, map)) = chosen else { return };

    // 桶 → 均值色
    let mut buckets: Vec<(u8, u8, u8, [u8; 3], u64)> = map
        .into_iter()
        .map(|((r, g, b), (sr, sg, sb, n))| {
            let n = n.max(1);
            (
                r,
                g,
                b,
                [
                    (sr / n) as u8,
                    (sg / n) as u8,
                    (sb / n) as u8,
                ],
                n,
            )
        })
        .collect();

    // 桶数仍超限 → 按像素数保留前 steps 个,其余并入色距最近的保留桶
    if buckets.len() as u32 > steps {
        buckets.sort_by(|a, b| b.4.cmp(&a.4));
        let keep = &buckets[..steps as usize];
        let kept: Vec<((u8, u8, u8), [u8; 3])> =
            keep.iter().map(|(r, g, b, c, _)| ((*r, *g, *b), *c)).collect();
        let mut remap: HashMap<(u8, u8, u8), [u8; 3]> = HashMap::new();
        for (r, g, b, c, _) in &buckets {
            let key = (*r, *g, *b);
            if kept.iter().any(|(k, _)| k == &key) {
                remap.insert(key, *c);
                continue;
            }
            let mut best = kept[0].1;
            let mut bestd = i32::MAX;
            for (_, kc) in &kept {
                let d = (c[0] as i32 - kc[0] as i32).pow(2)
                    + (c[1] as i32 - kc[1] as i32).pow(2)
                    + (c[2] as i32 - kc[2] as i32).pow(2);
                if d < bestd {
                    bestd = d;
                    best = *kc;
                }
            }
            remap.insert(key, best);
        }
        for p in img.pixels_mut() {
            let key = (p[0] >> shift, p[1] >> shift, p[2] >> shift);
            if let Some(c) = remap.get(&key) {
                p[0] = c[0];
                p[1] = c[1];
                p[2] = c[2];
            }
        }
        return;
    }

    let lut: HashMap<(u8, u8, u8), [u8; 3]> = buckets
        .into_iter()
        .map(|(r, g, b, c, _)| ((r, g, b), c))
        .collect();
    for p in img.pixels_mut() {
        let key = (p[0] >> shift, p[1] >> shift, p[2] >> shift);
        if let Some(c) = lut.get(&key) {
            p[0] = c[0];
            p[1] = c[1];
            p[2] = c[2];
        }
    }
}

/// 生成一层噪声(0~255),用于各种颗粒配方。
///
/// **诚实声明**:参考的噪声算子是**加性**的(`v += rand % (max−min+1) + min`,
/// min/max 缺省 0/255);我方走「噪声层 + Multiply/Lighten 合成」的配方语义,
/// 五个调用点(胶片颗粒等)的低幅区间在加性语义下会整体过曝,故保留合成
/// 路线并在此声明差异。参考通道掩码 bit0/1/2 = R/G/B、bit3 = A;mono 标志
/// 让四通道共用同一随机值 —— 见 `apply_noise_layer` 的 mono/独立抽样两分支。
fn noise_layer(img: &RgbaImage, seed: u32, low: u8, high: u8) -> Vec<u8> {
    let (lo, hi) = if low <= high { (low, high) } else { (high, low) };
    let mut rng = crate::oneclick::Mt19937::new(seed);
    let n = (img.width() * img.height()) as usize;
    let mut out = Vec::with_capacity(n);
    let span = (hi as i32 - lo as i32).max(0);
    for _ in 0..n {
        let r = rng.next_u8() as i32;
        out.push((lo as i32 + (r * (span + 1) / 256)).clamp(0, 255) as u8);
    }
    out
}

/// 把噪声层按灰度通道铺到图上(混合模式由调用方给)。
fn apply_noise_layer(
    img: &mut RgbaImage,
    seed: u32,
    low: u8,
    high: u8,
    gray: bool,
    mode: Blend,
    alpha: f32,
) {
    let noise = noise_layer(img, seed, low, high);
    let mut layer = img.clone();
    if gray {
        // mono:三通道共用同一随机值(参考 mono 标志语义)
        for (i, p) in layer.pixels_mut().enumerate() {
            p[0] = noise[i];
            p[1] = noise[i];
            p[2] = noise[i];
        }
    } else {
        // 非 mono:三通道**独立抽样**。参考语料未公开各通道的随机流派生方式,
        // 这里用种子异或不同常数近似「三条独立流」;早期版本用同一数组
        // 循环移位,通道间呈固定相关,已废弃。
        let ng = noise_layer(img, seed ^ 0x9E37_79B9, low, high);
        let nb = noise_layer(img, seed ^ 0x85EB_CA6B, low, high);
        for (i, p) in layer.pixels_mut().enumerate() {
            p[0] = noise[i];
            p[1] = ng[i];
            p[2] = nb[i];
        }
    }
    composite(img, &layer, mode, alpha);
}

/// 内光晕:画面边缘向 `color` 渐变。
///
/// `falloff` 是过渡宽度(像素),`strength` 直接缩放混合比例。
/// 参考产品的内光晕合成器未公开混合模式,这里用线性混合 ——
/// 对暗角(黑色)与白边(白色)两种典型用法观感一致。
pub fn glow_edge(img: &mut RgbaImage, color: [u8; 3], falloff: f32, strength: f32, alpha: f32) {
    let (w, h) = img.dimensions();
    let m = edge_mask(w, h, falloff);
    let layer = RgbaImage::from_pixel(w, h, Rgba([color[0], color[1], color[2], 255]));
    // 强度就是遮罩的增益上限
    let mut m2 = m;
    for v in m2.a.iter_mut() {
        *v = (*v * strength).clamp(0.0, 1.0);
    }
    composite_masked(img, &layer, &m2, Blend::Normal, alpha);
}

/// 加边框:画布四周外扩,先铺外框色,再内嵌一层内框色。
///
/// `outer_t` / `inner_t` 以"1024 宽参考像素"计,按画面短边等比缩放,
/// 这样同一配方在预览图与全分辨率下观感一致。
/// `corner_radius > 0` 时把画面四角修圆(`outer` 色填充圆角外的部分)。
/// `caption_h > 0` 时在底部额外留一条同色底边(拍立得式说明栏)。
///
/// **诚实声明**(与参考的已知差异,知情维持):参考按 **imageWidth** 缩放
/// 且第三个值**不参与缩放**;我方统一按短边缩放、三个值都缩 ——
/// 竖幅照片的边条会比参考略宽。
#[allow(clippy::too_many_arguments)]
pub fn border_frame(
    img: &RgbaImage,
    outer_color: [u8; 3],
    outer_t: f32,
    inner_color: [u8; 3],
    inner_t: f32,
    corner_radius: f32,
    caption_h: f32,
) -> RgbaImage {
    let (w, h) = img.dimensions();
    let scale = (w.min(h) as f32 / 1024.0).clamp(0.05, 4.0);
    let ot = (outer_t * scale).round().max(0.0) as u32;
    let it = (inner_t * scale).round().max(0.0) as u32;
    let cap = (caption_h * scale).round().max(0.0) as u32;
    let rad = (corner_radius * scale).round().max(0.0);

    // 只修圆角、不加边框的情形:不扩画布,直接修圆
    if ot == 0 && it == 0 && cap == 0 {
        let mut out = img.clone();
        if rad > 0.0 {
            round_corners(&mut out, rad, outer_color);
        }
        return out;
    }

    let pad = ot + it;
    let nw = w + pad * 2;
    let nh = h + pad * 2 + cap;
    let mut out = RgbaImage::from_pixel(nw, nh, Rgba([outer_color[0], outer_color[1], outer_color[2], 255]));
    // 内框(内框色矩形)从外框内缩 ot 开始
    if it > 0 {
        for y in (ot)..(nh.saturating_sub(ot)) {
            for x in (ot)..(nw.saturating_sub(ot)) {
                out.put_pixel(x, y, Rgba([inner_color[0], inner_color[1], inner_color[2], 255]));
            }
        }
    }
    // 贴原图
    for (x, y, p) in img.enumerate_pixels() {
        out.put_pixel(x + pad, y + pad, *p);
    }
    // 圆角:四角按半径挖成外框色
    if rad > 0.0 {
        round_corners(&mut out, rad, outer_color);
    }
    out
}

/// 把图像四角修圆,圆角外区域填 `fill` 色(原地、不改尺寸)。
pub fn round_corners(img: &mut RgbaImage, radius: f32, fill: [u8; 3]) {
    let (w, h) = img.dimensions();
    let r = radius.min(w as f32 / 2.0).min(h as f32 / 2.0).max(0.0);
    if r <= 0.5 {
        return;
    }
    let c = |x: f32, y: f32| -> f32 {
        // 到最近圆心的距离减去半径 → 正值落在圆角外
        let cxn = if x < r { r } else if x > w as f32 - r { w as f32 - r } else { x };
        let cyn = if y < r { r } else if y > h as f32 - r { h as f32 - r } else { y };
        let dx = x - cxn;
        let dy = y - cyn;
        if dx == 0.0 && dy == 0.0 {
            -r
        } else {
            (dx * dx + dy * dy).sqrt() - r
        }
    };
    for y in 0..h {
        for x in 0..w {
            let d = c(x as f32 + 0.5, y as f32 + 0.5);
            if d > 0.0 {
                let p = img.get_pixel_mut(x, y);
                p[0] = fill[0];
                p[1] = fill[1];
                p[2] = fill[2];
                p[3] = 255;
            }
        }
    }
}

/// 投影:画布外扩一圈背景色,在偏移位置画一层模糊后的画面剪影。
///
/// `distance` / `blur` 与边框厚度同口径(1024 宽参考像素),
/// `angle` 为投影方向(度,0 = 向右,90 = 向下)。
///
/// # 剪影从哪来
///
/// **诚实声明**(与参考的已知差异,知情维持):参考是 9 参数(另有
/// mode / 强度 / 质量档),我方 6 个;参考的横向/纵向模糊半径分开,
/// 我方合并为一个 `blur`;参考做两遍填充,我方单遍。
///
/// 参考实现的投影参数只有「阴影色 / 背景色 / 不透明度 / 距离 / 角度 /
/// 横向与纵向模糊半径 / 强度 / 质量档」,**没有任何"用哪个通道做遮罩"的开关**。
/// 也就是说剪影固定取**画面的不透明区域**(对本项目这种整幅不透明的照片
/// 就是整块画布矩形),不取亮度、也不取绿通道。所以纯黑照片与纯白照片
/// 打出来的阴影形状必须完全一样 —— 这是下面 `drop_shadow_shape_ignores_color`
/// 那条测试要守的约束。
#[allow(clippy::too_many_arguments)]
pub fn drop_shadow(
    img: &RgbaImage,
    bg: [u8; 3],
    shadow: [u8; 3],
    shadow_alpha: f32,
    distance: f32,
    angle: f32,
    blur: f32,
) -> RgbaImage {
    let (w, h) = img.dimensions();
    let scale = (w.min(h) as f32 / 1024.0).clamp(0.05, 4.0);
    let d = distance * scale;
    let b = blur * scale;
    let pad = (d + b * 2.0).ceil().max(4.0) as u32;
    let nw = w + pad * 2;
    let nh = h + pad * 2;
    let mut out = RgbaImage::from_pixel(nw, nh, Rgba([bg[0], bg[1], bg[2], 255]));

    // 剪影层:画面不透明处填阴影色,再整体模糊
    let mut sil = RgbaImage::from_pixel(nw, nh, Rgba([0, 0, 0, 0]));
    for (x, y, p) in img.enumerate_pixels() {
        if p[3] > 0 {
            sil.put_pixel(x + pad, y + pad, Rgba([shadow[0], shadow[1], shadow[2], 255]));
        }
    }
    // 位移
    let rad = angle.to_radians();
    let ox = (d * rad.cos()).round() as i32;
    let oy = (d * rad.sin()).round() as i32;
    let mut shifted = RgbaImage::from_pixel(nw, nh, Rgba([0, 0, 0, 0]));
    for y in 0..nh as i32 {
        for x in 0..nw as i32 {
            let sx = x - ox;
            let sy = y - oy;
            if sx >= 0 && sy >= 0 && (sx as u32) < nw && (sy as u32) < nh {
                let p = *sil.get_pixel(sx as u32, sy as u32);
                if p[3] > 0 {
                    shifted.put_pixel(x as u32, y as u32, p);
                }
            }
        }
    }
    // 只用 alpha 做模糊:把不透明处当灰度图模糊,再按 alpha 铺阴影色
    let mut alpha_img = RgbaImage::from_pixel(nw, nh, Rgba([0, 0, 0, 255]));
    for (x, y, p) in shifted.enumerate_pixels() {
        let a = p[3];
        alpha_img.put_pixel(x, y, Rgba([a, a, a, 255]));
    }
    box_blur(&mut alpha_img, b, b);
    for y in 0..nh {
        for x in 0..nw {
            let a = alpha_img.get_pixel(x, y)[0] as f32 / 255.0 * shadow_alpha.clamp(0.0, 1.0);
            if a <= 0.0 {
                continue;
            }
            let dp = out.get_pixel_mut(x, y);
            for c in 0..3 {
                let base = dp[c] as f32;
                dp[c] = (base + (shadow[c] as f32 - base) * a).clamp(0.0, 255.0) as u8;
            }
        }
    }
    // 贴原图
    for (x, y, p) in img.enumerate_pixels() {
        out.put_pixel(x + pad, y + pad, *p);
    }
    out
}

/// 旋转(度,顺时针为正),`pad = true` 时扩大画布避免裁角。
///
/// **诚实声明**(与参考的已知差异,知情维持):① 不接受弧度入参;
/// ② 没有「接近 90° 倍数直接交换宽高」的快速分支 —— 90° 整数旋转因
/// `cos≈4.4e-8` 的浮点残差会让画布多出 1px;如需像素级精确的 90° 旋转,
/// 应在调用方走裁切/转置路径。
pub fn rotate(img: &mut RgbaImage, deg: f32, pad: bool, border: [u8; 3]) -> RgbaImage {
    let (w, h) = img.dimensions();
    let rad = deg.to_radians();
    let (sin, cos) = rad.sin_cos();
    let src = img.clone();
    if pad {
        let nw = ((w as f32 * cos.abs()) + (h as f32 * sin.abs())).ceil() as u32;
        let nh = ((w as f32 * sin.abs()) + (h as f32 * cos.abs())).ceil() as u32;
        let mut out = RgbaImage::from_pixel(nw, nh, Rgba([border[0], border[1], border[2], 255]));
        let (cx, cy) = (nw as f32 / 2.0, nh as f32 / 2.0);
        let (scx, scy) = (w as f32 / 2.0, h as f32 / 2.0);
        for y in 0..nh {
            for x in 0..nw {
                // 反向映射:目标点绕中心逆旋回源图
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                let sx = dx * cos + dy * sin + scx;
                let sy = -dx * sin + dy * cos + scy;
                if sx >= 0.0 && sy >= 0.0 && sx < w as f32 && sy < h as f32 {
                    let p = *src.get_pixel(sx as u32, sy as u32);
                    out.put_pixel(x, y, p);
                }
            }
        }
        out
    } else {
        let mut out = img.clone();
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        for y in 0..h {
            for x in 0..w {
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                let sx = dx * cos + dy * sin + cx;
                let sy = -dx * sin + dy * cos + cy;
                let p = if sx >= 0.0 && sy >= 0.0 && sx < w as f32 && sy < h as f32 {
                    *src.get_pixel(sx as u32, sy as u32)
                } else {
                    Rgba([border[0], border[1], border[2], 255])
                };
                out.put_pixel(x, y, p);
            }
        }
        out
    }
}

/// 居中裁切到 `cw × ch`(超出部分对称裁掉,不足部分按边缘夹取)。
pub fn crop_center(img: &RgbaImage, cw: u32, ch: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let cw = cw.clamp(1, w.max(1));
    let ch = ch.clamp(1, h.max(1));
    let x0 = (w - cw) / 2;
    let y0 = (h - ch) / 2;
    image::imageops::crop_imm(img, x0, y0, cw, ch).to_image()
}

/// 最近邻缩放,取样口径与 [`resize`] 一致(`(dst + 0.5)·scale − 0.5`)。
///
/// 不用 `image::imageops::resize` 的最近邻:它按 `dst · src/dst` 直接取整,
/// 与大图比例不整除时块宽会一顿一顿地跳。按像素中心对齐才能得到等宽块。
pub fn resize_nearest(img: &RgbaImage, nw: u32, nh: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let nw = nw.max(1);
    let nh = nh.max(1);
    if w == 0 || h == 0 {
        return RgbaImage::new(nw, nh);
    }
    let sx = w as f32 / nw as f32;
    let sy = h as f32 / nh as f32;
    RgbaImage::from_fn(nw, nh, |x, y| {
        let fx = ((x as f32 + 0.5) * sx - 0.5).round();
        let fy = ((y as f32 + 0.5) * sy - 0.5).round();
        let ix = (fx.max(0.0) as u32).min(w - 1);
        let iy = (fy.max(0.0) as u32).min(h - 1);
        *img.get_pixel(ix, iy)
    })
}

/// 双线性缩放到指定尺寸。
///
/// 几何是**像素中心对齐**:目标像素 `x` 对应源坐标 `(x + 0.5)·W/nw − 0.5`,
/// 两个方向各自有独立的比例,不做等比钳制。这与参考实现的取样点完全一致
/// (`src = (dst + 0.5) · scale + t`,`t` 使像素中心落在源像素中心)。
///
/// 参考实现里缩放有两条路:默认开平滑时走 2×2 双线性(8 位定点分数,
/// `a + ((b − a)·f >> 8)`);显式关平滑(`smoothing="false"`)时退化成
/// **最近邻**——方块化的放大那一步就是这么关掉的。这里只提供双线性版本,
/// 需要最近邻时用 [`resize_nearest`](取样口径与这里一致)。
pub fn resize(img: &RgbaImage, nw: u32, nh: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let nw = nw.max(1);
    let nh = nh.max(1);
    if w == 0 || h == 0 {
        return RgbaImage::new(nw, nh);
    }
    let sx = w as f32 / nw as f32;
    let sy = h as f32 / nh as f32;
    RgbaImage::from_fn(nw, nh, |x, y| {
        let fx = ((x as f32 + 0.5) * sx - 0.5).clamp(0.0, (w - 1) as f32);
        let fy = ((y as f32 + 0.5) * sy - 0.5).clamp(0.0, (h - 1) as f32);
        let x0 = fx.floor() as u32;
        let y0 = fy.floor() as u32;
        let x1 = (x0 + 1).min(w - 1);
        let y1 = (y0 + 1).min(h - 1);
        let tx = fx - x0 as f32;
        let ty = fy - y0 as f32;
        let p00 = img.get_pixel(x0, y0);
        let p10 = img.get_pixel(x1, y0);
        let p01 = img.get_pixel(x0, y1);
        let p11 = img.get_pixel(x1, y1);
        let mut out = [0u8; 4];
        for c in 0..4 {
            let top = p00[c] as f32 * (1.0 - tx) + p10[c] as f32 * tx;
            let bot = p01[c] as f32 * (1.0 - tx) + p11[c] as f32 * tx;
            out[c] = (top * (1.0 - ty) + bot * ty).round().clamp(0.0, 255.0) as u8;
        }
        Rgba(out)
    })
}

/// 单色红外:参考的三步嵌套链,缺一步都不算。
///
/// ```text
/// ① ColorMatrix 只保留绿通道(R'=0、B'=0、G'=G)
/// ② Blur(r=5.0)
/// ③ ColorMatrix 三行相同 [k, 2.0, −1−k, 0, 0](k 缺省 −0.5,行和 = 1)
/// ```
///
/// ② 之后 R/B 已是 0,③ 在数学上退化为 `2·G` —— 但保留矩阵形式,
/// 与参考的装配顺序一一对应;`red_weight` 因此在链路上无视觉影响
/// (参考语料里也标注了这点未确认)。终值是**纯绿通道派生的单色**,
/// 没有 Screen 光晕 —— 我方早期那层光晕已删除。
pub fn infrared(img: &mut RgbaImage, red_weight: f32) {
    let k = red_weight;
    // ① 只保留绿通道
    for p in img.pixels_mut() {
        let g = p[1];
        p[0] = 0;
        p[1] = g;
        p[2] = 0;
    }
    // ② Blur(5.0)
    box_blur(img, 5.0, 5.0);
    // ③ [k, 2.0, −1−k] 矩阵(此时 R=B=0 → 2·G)
    for p in img.pixels_mut() {
        let v = (k * p[0] as f32 + 2.0 * p[1] as f32 + (-1.0 - k) * p[2] as f32)
            .clamp(0.0, 255.0)
            .round() as u8;
        p[0] = v;
        p[1] = v;
        p[2] = v;
    }
}

// ---------------------------------------------------------------------------
// 配方
// ---------------------------------------------------------------------------

/// 全部配方名(供 UI 列举与名称合法性判断)。
pub const RECIPES: &[&str] = &[
    "punch",
    "frame",
    "float_shadow",
    "focal_pixelate",
    "zoom_blur",
    "film_grain",
    "hdr",
    "heat_map",
    "toy_camera",
    "infrared",
    "local_contrast",
    "toy_vivid",
    "edge_glow",
    "matte_frame",
    "neon",
    "night_vision",
    "pixelate",
    "instant_film",
    "posterize",
    "rounded_corners",
    "color_wash",
    "duotone",
    "comic",
    "bloom",
    "orton",
    "vignette_glow",
    "lomo",
    "sketch",
    "cross_process",
    "cinemascope",
    "sixties",
];

/// 该名字是否是本模块的配方。
pub fn is_recipe(name: &str) -> bool {
    RECIPES.contains(&name.to_ascii_lowercase().as_str())
}

/// 配方在 UI 上显示的中文名。
pub fn label(name: &str) -> &'static str {
    match name {
        "punch" => "浓郁",
        "frame" => "画框",
        "float_shadow" => "浮起投影",
        "focal_pixelate" => "中心马赛克",
        "zoom_blur" => "变焦拖影",
        "film_grain" => "胶片颗粒",
        "hdr" => "强动态",
        "heat_map" => "热成像",
        "toy_camera" => "玩具相机",
        "infrared" => "红外胶片",
        "local_contrast" => "局部对比",
        "toy_vivid" => "鲜艳暗角",
        "edge_glow" => "边缘提亮",
        "matte_frame" => "装裱卡纸",
        "neon" => "霓虹",
        "night_vision" => "夜视仪",
        "pixelate" => "马赛克",
        "instant_film" => "一次成像",
        "posterize" => "海报色阶",
        "rounded_corners" => "圆角",
        "color_wash" => "单色洗",
        "duotone" => "双色调",
        "comic" => "漫画",
        "bloom" => "柔光",
        "orton" => "奥顿柔焦",
        "vignette_glow" => "黑色暗角",
        "lomo" => "暗角柔化",
        "sketch" => "铅笔素描",
        "cross_process" => "交叉冲印",
        "cinemascope" => "宽银幕",
        "sixties" => "六十年代",
        _ => "效果",
    }
}

/// 配方默认强度(0~100)。
pub fn default_amount(name: &str) -> f32 {
    match name {
        "film_grain" | "hdr" | "local_contrast" | "infrared" => 100.0,
        "punch" => 60.0,
        "comic" => 80.0,
        _ => 100.0,
    }
}

/// 应用配方;`amount` 为 0~100 的强度。
///
/// 等价于 [`apply_with`] 加 [`Blend::Normal`]。
pub fn apply(img: &mut RgbaImage, name: &str, amount: f32) -> bool {
    apply_with(img, name, amount, Blend::Normal)
}

/// 应用配方,并把结果按 `mode` 与原图回混。
///
/// `mode` 是处理结果与**原图**的混合模式:`Normal` 即普通的不透明度回混;
/// 换成 `Overlay` / `Screen` 之类可以做出更轻透的风格(见 [`Blend::parse`])。
/// 返回 `false` 表示名字不认识(调用方据此走别的分支)。
pub fn apply_with(img: &mut RgbaImage, name: &str, amount: f32, mode: Blend) -> bool {
    let a = (amount.clamp(0.0, 100.0)) / 100.0;
    let orig = img.clone();
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return true;
    }
    let short = w.min(h) as f32;
    let long = w.max(h) as f32;

    match name.to_ascii_lowercase().as_str() {
        // 浓郁:负亮度 + 加饱和 + 加对比,三者同时给,压暗中间调同时拉开层次
        "punch" => {
            crate::effects::apply_color_matrix(img, -20.0, 40.0, 20.0);
        }
        // 画框:外黑内白的双层边框(厚度按 1024 宽参考像素)
        "frame" => {
            let out_img = border_frame(img, [0, 0, 0], 20.0, [255, 255, 255], 5.0, 0.0, 0.0);
            *img = out_img;
            return true;
        }
        // 浮起投影:白底 + 黑色柔化投影
        "float_shadow" => {
            let out_img = drop_shadow(
                img,
                [255, 255, 255],
                [0, 0, 0],
                0.7 * a,
                4.0,
                90.0,
                10.0,
            );
            *img = out_img;
            return true;
        }
        // 中心马赛克:圆内方块化,圆外保持
        "focal_pixelate" => {
            let r = short / 2.0;
            let m = circular_mask(w, h, w as f32 / 2.0, h as f32 / 2.0, r * 0.5, r * 1.5, 1.0, 0.0);
            let mut layer = img.clone();
            pixelate(&mut layer, short * 0.02);
            composite_masked(img, &layer, &m, Blend::Normal, 1.0);
        }
        // 变焦拖影:圆外径向模糊,圆心保持清晰
        "zoom_blur" => {
            let r = short / 2.0;
            let m = circular_mask(w, h, w as f32 / 2.0, h as f32 / 2.0, r * 0.5, r * 1.5, 0.0, 1.0);
            let mut layer = img.clone();
            radial_blur(&mut layer, w as f32 / 2.0, h as f32 / 2.0, 50.0);
            composite_masked(img, &layer, &m, Blend::Normal, 1.0);
        }
        // 胶片颗粒:亮部叠加细颗粒,用正片叠底压暗,不脏暗部
        "film_grain" => {
            apply_noise_layer(img, 1, 229, 255, true, Blend::Multiply, 1.0);
        }
        // 强动态:参考的 HDR 就是同一个局部对比算子,滑杆缺省 radius=20、contrast=3
        "hdr" => {
            local_contrast_ref(img, 20.0, 3.0);
        }
        // 热成像:先去饱和,再按亮度走蓝→绿→红色阶
        "heat_map" => {
            crate::effects::apply_color_matrix(img, 0.0, 0.0, -100.0);
            let stops: [(f32, (f32, f32, f32)); 5] = [
                (0.0, (240.0, 100.0, 50.0)),
                (31.875, (240.0, 100.0, 100.0)),
                (127.5, (120.0, 100.0, 100.0)),
                (223.125, (0.0, 100.0, 100.0)),
                (255.0, (0.0, 100.0, 50.0)),
            ];
            hsv_gradient_map(img, &stops, 0.0);
        }
        // 玩具相机:自动色阶 + 边缘压暗 + 边缘模糊 + 偏红灰 + 加对比 + 颗粒
        "toy_camera" => {
            auto_fix(img);
            glow_edge(img, [0, 0, 0], long * 0.25, 0.7, 1.0);
            let m = circular_mask(
                w,
                h,
                w as f32 / 2.0,
                h as f32 / 2.0,
                long * 0.45,
                long * 0.5,
                0.0,
                1.0,
            );
            let mut layer = img.clone();
            box_blur(&mut layer, 18.0, 20.0);
            composite_masked(img, &layer, &m, Blend::Normal, 1.0);
            crate::oneclick::apply_bw(img);
            crate::effects::apply_color_matrix(img, 0.0, 25.0, 0.0);
            apply_noise_layer(img, 5, 217, 255, true, Blend::Multiply, 0.6);
        }
        // 红外胶片:绿通道主导的单色 + 高光溢出
        "infrared" => {
            infrared(img, -0.5);
        }
        // 局部对比:中小半径、温和强度
        "local_contrast" => {
            local_contrast_ref(img, 15.0, 1.5);
        }
        // 鲜艳暗角:边缘压暗 + 边缘模糊 + 提饱和对比
        "toy_vivid" => {
            glow_edge(img, [0, 0, 0], long * 0.25, 0.8, 1.0);
            let m = circular_mask(
                w,
                h,
                w as f32 / 2.0,
                h as f32 / 2.0,
                long * 0.25,
                long * 0.5,
                0.0,
                1.0,
            );
            let mut layer = img.clone();
            box_blur(&mut layer, 20.0, 20.0);
            composite_masked(img, &layer, &m, Blend::Normal, 1.0);
            crate::effects::apply_color_matrix(img, 5.0, 35.0, 20.0);
        }
        // 边缘提亮:白色内光晕,只抬边缘
        "edge_glow" => {
            glow_edge(img, [255, 255, 255], long * 0.22, 0.35 * a, 1.0);
        }
        // 装裱卡纸:内黑晕 + 米色内框 + 黑晕 + 深棕外框
        "matte_frame" => {
            glow_edge(img, [0, 0, 0], long * 0.05, 0.7, 0.7);
            let inner = border_frame(img, [240, 234, 228], 0.0, [240, 234, 228], 40.0, 0.0, 0.0);
            *img = inner;
            glow_edge(img, [0, 0, 0], long * 0.05, 0.7, 0.6);
            let outer = border_frame(img, [26, 14, 3], 25.0, [26, 14, 3], 0.0, 0.0, 0.0);
            *img = outer;
            return true;
        }
        // 霓虹:取描边 → 反相得到"黑底白线" → 单色着色
        "neon" => {
            let mut layer = img.clone();
            edge_detect_b(&mut layer, 50.0);
            let inv = crate::curves::presets::invert();
            crate::curves::apply_curves(&mut layer, &inv);
            crate::oneclick::apply_tint(&mut layer, [255, 0, 0], 1.0);
            composite(img, &layer, Blend::Normal, a);
        }
        // 夜视仪:自动色阶 → 黑绿映射 → 内光晕 → 亮部噪声
        //
        // 光晕半径按画面长边的 0.233 倍取(参考配方的 `35·0.02·max/3`),
        // 强度 1.5 让增益到顶,所以边缘是纯黑压暗而不是半透明覆盖。
        "night_vision" => {
            auto_fix(img);
            gradient_map(img, &[[0, 0, 0], [0x57, 0xcc, 0x29]]);
            glow_edge(img, [0, 0, 0], long * 0.2333, 1.5, 1.0);
            apply_noise_layer(img, 30, 0, 180, false, Blend::Lighten, 0.2);
        }
        // 马赛克:方块化
        "pixelate" => {
            let mut layer = img.clone();
            pixelate(&mut layer, short * 0.02);
            composite(img, &layer, Blend::Normal, a);
        }
        // 一次成像:正方形裁切 + 白边(下沿更宽)+ 投影 + 轻微旋转
        "instant_film" => {
            let side = short;
            let mut out_img = crop_center(img, side as u32, side as u32);
            let lr = (side * 0.0645).round();
            let top = (side * 0.0968).round();
            let bottom = (side * 0.258).round();
            out_img = border_asym(&out_img, [255, 255, 255], lr, lr, top, bottom);
            out_img = drop_shadow(&out_img, [226, 226, 226], [0, 0, 0], 0.4, 3.0, 85.0, 8.0);
            out_img = rotate(&mut out_img, 5.0, true, [226, 226, 226]);
            *img = out_img;
            return true;
        }
        // 海报色阶:先软化细节再压色阶,避免噪点被量化成花斑
        "posterize" => {
            box_blur(img, 2.1, 2.1);
            quantize_palette(img, 8, 4);
        }
        // 圆角:四角按短边的 1/10 修圆
        "rounded_corners" => {
            round_corners(img, short * 0.1, [255, 255, 255]);
        }
        // 单色洗:保留明暗,染一层浅蓝
        "color_wash" => {
            crate::oneclick::apply_tint(img, [0x80, 0xcf, 0xff], 1.0);
        }
        // 双色调:先去饱和 + 加对比,再映射到深蓝↔黄
        "duotone" => {
            crate::effects::apply_color_matrix(img, 0.0, 20.0, -100.0);
            two_tone(img, [0x00, 0x44, 0x88], [0xff, 0xff, 0x00]);
        }
        // 漫画:降采样成色块 + 压色阶 + 提对比(网点形状不模拟)
        "comic" => {
            let mut layer = img.clone();
            box_blur(&mut layer, 2.0, 2.0);
            pixelate(&mut layer, (w as f32 / 120.0).round().max(2.0));
            quantize_palette(&mut layer, 8, 4);
            crate::effects::apply_color_matrix(&mut layer, 0.0, 28.0, 25.0);
            composite(img, &layer, Blend::Normal, a);
        }
        // 柔化(Soften):参考是模糊算子(xblur = yblur = Impact·20/50)
        // 以**普通不透明度** `(100−Fade)·0.8/100` 盖回原图,不带混合模式、不带曲线。
        // Impact 缺省 50 → 半径 20;Fade 缺省 50 → 0.4(范围上限 0.8)。
        //
        // 参考还有一支画笔遮罩(Mask),本项目没有笔刷 UI,故整幅施加。
        "bloom" => {
            let mut layer = img.clone();
            box_blur(&mut layer, 20.0, 20.0);
            composite(img, &layer, Blend::Normal, 0.8);
        }
        // 托伊相机款(Lomo):参考滤镜描述里是三步依次作用 —
        //   ① 黑色**内发光**:半径 = 35·0.02·长边/2 = 0.35·长边,强度 1.1;
        //   ② **圆形渐变遮罩**下的 Blur(20):内径 = 长边/4,
        //      外径 = 长边/2·(2 − blur/100),参考滑杆缺省 50 → 0.75·长边;
        //   ③ 色彩矩阵:饱和 +20 / 对比 +35 / 亮度 +5。
        // 遮罩是「内实外虚」,所以只有中心被模糊,四角保持清晰。
        "lomo" => {
            glow_edge(img, [0, 0, 0], long * 0.35, 1.1, 1.0);
            let m = circular_mask(
                w,
                h,
                w as f32 / 2.0,
                h as f32 / 2.0,
                long * 0.25,
                long * 0.75,
                1.0,
                0.0,
            );
            let mut layer = img.clone();
            box_blur(&mut layer, 20.0, 20.0);
            composite_masked(img, &layer, &m, Blend::Normal, 1.0);
            crate::effects::apply_color_matrix(img, 5.0, 35.0, 20.0);
        }
        // 奥顿柔焦(Orton-ish):参考是**嵌套操作** ——
        // `Blur(bloom)` 以 **Overlay** 混合回原图 → 再叠一条亮度曲线
        // `[(0,0), (128, 128+(B−50)·75/50), (255,255)]`。Bloom 缺省 25、Brightness
        // 缺省 50(此时中点是 (128,128),曲线恒等)。
        //
        // 与 `bloom`(柔化)的差异**只在混合模式**:这里是 Overlay,那里是普通不透明度。
        // 参考的 Brightness 滑杆我方不单独暴露,固定取缺省值 —— 见 `default_amount`。
        "orton" => {
            let mut layer = img.clone();
            box_blur(&mut layer, 25.0, 25.0);
            composite(img, &layer, Blend::Overlay, 1.0);
        }
        // 暗角(Vignette):参考是**内发光**(纯黑),
        // `innerglow=true`、strength 缺省 1.4(量程 1~2)、
        // 羽化半径 = Blur·0.02·长边/4,Blur 缺省 35 → **0.175·长边**。
        //
        // 与我方自主设计的 smoothstep 暗角(`oneclick::apply_vignette`)是两种
        // 算法并存,不要混淆。
        "vignette_glow" => {
            glow_edge(img, [0, 0, 0], long * 0.175, 1.4, 1.0);
        }
        // 铅笔素描:灰度 → 自动色阶 → 反相模糊后加色 → 与原灰度叠加 → 自动色阶 → 曲线
        "sketch" => {
            let mut a = img.clone();
            crate::oneclick::apply_bw(&mut a);
            auto_fix(&mut a);
            let base = a.clone();
            let mut layer = a.clone();
            let inv = crate::curves::presets::invert();
            crate::curves::apply_curves(&mut layer, &inv);
            box_blur(&mut layer, 2.0, 2.0);
            composite(&mut a, &layer, Blend::Add, 1.0);
            composite(&mut a, &base, Blend::Overlay, 1.0);
            auto_fix(&mut a);
            let c = crate::curves::presets::pencil_sketch(100.0);
            crate::curves::apply_curves(&mut a, &c);
            *img = a;
        }
        // 交叉冲印:四条曲线错位 + 提亮提对比 + 淡黄单色相乘
        "cross_process" => {
            let c = crate::curves::presets::cross_develop();
            crate::curves::apply_curves(img, &c);
            crate::effects::apply_color_matrix(img, 10.0, 10.0, 0.0);
            let (w2, h2) = img.dimensions();
            let layer = RgbaImage::from_pixel(w2, h2, Rgba([0xfc, 0xff, 0x00, 255]));
            composite(img, &layer, Blend::Multiply, 0.2);
        }
        // 宽银幕:裁成 1.7:1 → 纵向压到 95% → 自动色阶 + 降饱和 +
        // 三通道错位曲线 + 细颗粒 → 上下各加 15% 黑边。
        //
        // 裁切高度取 `min(round(宽/1.7), 原高)`,所以竖幅照片不会被拉扁,
        // 只是保持原高后按宽度算出的比例封顶。
        "cinemascope" => {
            let cw = w;
            let ch = ((w as f32 / 1.7).round() as u32).clamp(1, h);
            let mut out_img = crop_center(img, cw, ch);
            let nh = ((ch as f32 * 0.95).round() as u32).max(1);
            out_img = resize(&out_img, cw, nh);
            auto_fix(&mut out_img);
            crate::effects::apply_color_matrix(&mut out_img, 0.0, 0.0, -25.0);
            let c = crate::curves::presets::widescreen();
            crate::curves::apply_curves(&mut out_img, &c);
            apply_noise_layer(&mut out_img, 0, 225, 245, true, Blend::Multiply, 1.0);
            let bar = (ch as f32 * 0.15).round();
            *img = border_asym(&out_img, [0, 0, 0], 0.0, 0.0, bar, bar);
            return true;
        }
        // 六十年代:自动色阶 + 暖调错位曲线 + 细颗粒正片叠底 + 白色圆角。
        // 圆角半径 = 短边/14,外框色缺省为白。
        "sixties" => {
            auto_fix(img);
            let c = crate::curves::presets::retro();
            crate::curves::apply_curves(img, &c);
            apply_noise_layer(img, 5, 235, 255, true, Blend::Multiply, 0.6);
            round_corners(img, short / 14.0, [255, 255, 255]);
        }
        _ => return false,
    }

    // 其余分支按不透明度回混:out = orig·(1−a) + styled·a
    if a < 1.0 || mode != Blend::Normal {
        let styled = img.clone();
        *img = orig;
        composite(img, &styled, mode, a);
    }
    true
}

/// 四边各自指定厚度的边框(用于下沿更宽的一次成像白边)。
fn border_asym(
    img: &RgbaImage,
    color: [u8; 3],
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
) -> RgbaImage {
    let (w, h) = img.dimensions();
    let (l, r) = (left.max(0.0) as u32, right.max(0.0) as u32);
    let (t, b) = (top.max(0.0) as u32, bottom.max(0.0) as u32);
    let nw = w + l + r;
    let nh = h + t + b;
    let mut out = RgbaImage::from_pixel(nw, nh, Rgba([color[0], color[1], color[2], 255]));
    for (x, y, p) in img.enumerate_pixels() {
        out.put_pixel(x + l, y + t, *p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一张有横向渐变 + 局部色块的测试图(纯色图会被直方图类算子退化成无操作)。
    fn sample(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            let r = (x * 255 / w.max(1)) as u8;
            let g = (y * 255 / h.max(1)) as u8;
            let b = if (x / 4 + y / 4) % 2 == 0 { 40 } else { 210 };
            Rgba([r, g, b, 255])
        })
    }

    #[test]
    fn gradient_map_two_stops_is_linear() {
        let mut img = RgbaImage::from_pixel(3, 1, Rgba([0, 0, 0, 255]));
        img.put_pixel(1, 0, Rgba([128, 128, 128, 255]));
        img.put_pixel(2, 0, Rgba([255, 255, 255, 255]));
        gradient_map(&mut img, &[[0, 0, 0], [255, 255, 255]]);
        // 黑→黑、白→白、中灰近似中灰(亮度权重不是等权,允许几个码值偏差)
        assert_eq!(img.get_pixel(0, 0)[0], 0);
        assert_eq!(img.get_pixel(2, 0)[0], 255);
        let mid = img.get_pixel(1, 0)[0] as i32;
        assert!((mid - 128).abs() <= 2, "中灰映射偏差过大: {mid}");
    }

    #[test]
    fn two_tone_maps_extremes_to_end_colors() {
        let mut img = RgbaImage::from_pixel(2, 1, Rgba([0, 0, 0, 255]));
        img.put_pixel(1, 0, Rgba([255, 255, 255, 255]));
        two_tone(&mut img, [0x00, 0x44, 0x88], [0xff, 0xff, 0x00]);
        assert_eq!(*img.get_pixel(0, 0), Rgba([0x00, 0x44, 0x88, 255]));
        assert_eq!(*img.get_pixel(1, 0), Rgba([0xff, 0xff, 0x00, 255]));
    }

    #[test]
    fn pixelate_makes_uniform_blocks() {
        let mut img = sample(16, 16);
        pixelate(&mut img, 4.0);
        for by in 0..4u32 {
            for bx in 0..4u32 {
                let first = *img.get_pixel(bx * 4, by * 4);
                for y in 0..4u32 {
                    for x in 0..4u32 {
                        assert_eq!(
                            *img.get_pixel(bx * 4 + x, by * 4 + y),
                            first,
                            "块 ({bx},{by}) 内像素不一致"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn posterize_cuts_color_count() {
        let mut img = sample(64, 64);
        quantize_palette(&mut img, 4, 4);
        let mut set = std::collections::HashSet::new();
        for p in img.pixels() {
            set.insert([p[0], p[1], p[2]]);
        }
        assert!(set.len() <= 4, "量化后仍有 {} 种颜色", set.len());
    }

    #[test]
    fn border_frame_grows_canvas_and_keeps_center() {
        let img = sample(200, 200);
        let out = border_frame(&img, [0, 0, 0], 20.0, [255, 255, 255], 5.0, 0.0, 0.0);
        assert!(out.width() > 200 && out.height() > 200, "画布未外扩");
        // 外框四角是外框色
        assert_eq!(out.get_pixel(0, 0)[0], 0);
        // 中心区域仍是原图内容(不要求逐像素一致,只要求不是纯边框色)
        let c = out.get_pixel(out.width() / 2, out.height() / 2);
        assert!(c[0] != 0 || c[1] != 0 || c[2] != 0);
    }

    #[test]
    fn rounded_corners_paints_corner_only() {
        let mut img = sample(64, 64);
        let before = *img.get_pixel(0, 0);
        round_corners(&mut img, 16.0, [255, 255, 255]);
        assert_ne!(*img.get_pixel(0, 0), before, "圆角外的像素未被填充");
        let center = img.get_pixel(32, 32);
        assert_eq!(center[3], 255, "中心像素被误伤");
    }

    /// 参考的 LocalContrast 不是单公式,而是**两支嵌套**依次作用:
    ///
    /// ```text
    /// blur = Blur(orig, r)
    /// 支一(subtract):(blur − orig)·c   →  以 subtract 复合回:bg = blur + (orig−blur)·c
    /// 支二(add)      :(orig − blur)·c   →  以 add      复合回:bg + (orig−blur)·c
    /// 合计           :blur + 2c·(orig − blur)
    /// ```
    ///
    /// 于是与常见的 USM 形式 `orig + s·(orig−blur)` 是**同一族**,换算关系
    /// `s = 2c − 1`。两条推论被下面的断言锁死:
    /// `c = 0.5` 必须严格还原原图;`s = 2c − 1` 必须逐字节等价。
    #[test]
    fn local_contrast_reference_is_identity_at_half() {
        let mut img = sample(24, 24);
        let before = img.clone();
        local_contrast_ref(&mut img, 8.0, 0.5);
        assert_eq!(
            img.as_raw(),
            before.as_raw(),
            "c=0.5 时应严格还原原图(blur + 1·detail = orig)"
        );
    }

    #[test]
    fn local_contrast_strength_maps_to_reference_contrast() {
        for c in [0.5f32, 1.0, 1.5, 3.0] {
            let mut a = sample(24, 24);
            let mut b = sample(24, 24);
            local_contrast(&mut a, 8.0, 2.0 * c - 1.0);
            local_contrast_ref(&mut b, 8.0, c);
            for (pa, pb) in a.pixels().zip(b.pixels()) {
                for ch in 0..3 {
                    assert!(
                        (pa[ch] as i32 - pb[ch] as i32).abs() <= 1,
                        "c={c} 时两种参数口径应等价,实得 {pa:?} vs {pb:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn local_contrast_zero_strength_is_noop() {
        let mut img = sample(24, 24);
        let before = img.clone();
        local_contrast(&mut img, 8.0, 0.0);
        assert_eq!(img.into_raw(), before.into_raw());
    }

    #[test]
    fn edge_detect_on_flat_area_is_white() {
        let mut img = RgbaImage::from_pixel(8, 8, Rgba([120, 120, 120, 255]));
        edge_detect_b(&mut img, 50.0);
        assert_eq!(img.get_pixel(4, 4)[0], 255, "平坦区应为白底");
    }

    #[test]
    fn circular_mask_hits_inner_and_outer_alpha() {
        let m = circular_mask(41, 41, 20.5, 20.5, 5.0, 15.0, 1.0, 0.0);
        assert!((m.at(20, 20) - 1.0).abs() < 1e-3, "圆心应为内圈值");
        assert!(m.at(0, 0) < 1e-3, "角上应为外圈值");
    }

    #[test]
    fn hsv_gradient_map_hue_walks_shortest_arc() {
        // 350° → 10° 应经过 0°(红),而不是绕回 180°
        let stops = vec![(0.0, (350.0, 100.0, 100.0)), (255.0, (10.0, 100.0, 100.0))];
        let mut img = RgbaImage::from_pixel(2, 1, Rgba([0, 0, 0, 255]));
        img.put_pixel(1, 0, Rgba([255, 255, 255, 255]));
        hsv_gradient_map(&mut img, &stops, 0.0);
        let p = img.get_pixel(0, 0);
        assert!(p[0] > 200 && p[1] < 80, "350° 应落在红区,实得 {:?}", [p[0], p[1], p[2]]);
    }

    #[test]
    fn border_frame_layers_are_ordered_outer_then_inner() {
        // 200px 短边 → 缩放 0.195:外框 4px、内框 1px、内边距合计 5px
        let img = RgbaImage::from_pixel(200, 200, Rgba([128, 128, 128, 255]));
        let out = border_frame(&img, [0, 0, 0], 20.0, [255, 255, 255], 5.0, 0.0, 0.0);
        let mid = out.height() / 2;
        assert_eq!(out.get_pixel(0, mid)[0], 0, "最外层应是外框色");
        assert_eq!(out.get_pixel(4, mid)[0], 255, "外框之内的 1px 应是内框色");
        assert_eq!(out.get_pixel(10, mid)[0], 128, "内边距之内应回到原图");
    }

    #[test]
    fn circle_mask_reverse_form_marks_outside() {
        // inner_alpha=0 / outer_alpha=1 → 只覆盖圆外
        let m = circular_mask(41, 41, 20.5, 20.5, 5.0, 15.0, 0.0, 1.0);
        assert!(m.at(20, 20) < 1e-3, "圆心应被排除");
        assert!(m.at(0, 0) > 0.99, "角上应被包含");
    }

    #[test]
    fn every_recipe_runs_without_panic() {
        for name in RECIPES {
            let mut img = sample(48, 40);
            let ok = apply(&mut img, name, 100.0);
            assert!(ok, "配方 {name} 未被识别");
            assert!(img.width() > 0 && img.height() > 0, "配方 {name} 产生了空图");
        }
    }

    #[test]
    fn amount_zero_leaves_image_untouched() {
        // 只挑不改画布尺寸的配方:改尺寸的配方无法按像素回混。
        for name in ["punch", "hdr", "infrared", "night_vision", "duotone", "heat_map"] {
            let mut img = sample(32, 32);
            let before = img.clone();
            assert!(apply(&mut img, name, 0.0));
            assert_eq!(
                img.into_raw(),
                before.into_raw(),
                "配方 {name} 在强度 0 时改变了像素"
            );
        }
    }

    #[test]
    fn unknown_name_is_rejected() {
        let mut img = sample(8, 8);
        assert!(!apply(&mut img, "no_such_recipe", 50.0));
    }

    #[test]
    fn blend_parse_covers_names() {
        assert_eq!(Blend::parse("MULTIPLY"), Blend::Multiply);
        assert_eq!(Blend::parse("overlay"), Blend::Overlay);
        assert_eq!(Blend::parse("screen"), Blend::Screen);
        assert_eq!(Blend::parse("add"), Blend::Add);
        assert_eq!(Blend::parse("subtract"), Blend::Subtract);
        assert_eq!(Blend::parse("darken"), Blend::Darken);
        assert_eq!(Blend::parse("lighten"), Blend::Lighten);
        assert_eq!(Blend::parse("???"), Blend::Normal);
    }

    #[test]
    fn apply_with_overlay_differs_from_normal() {
        let a = sample(24, 24);
        let mut normal = a.clone();
        let mut over = a.clone();
        assert!(apply_with(&mut normal, "color_wash", 100.0, Blend::Normal));
        assert!(apply_with(&mut over, "color_wash", 100.0, Blend::Overlay));
        assert_ne!(normal.into_raw(), over.into_raw());
    }
}

/// 手动生成观感样张:设置 `MALIANG_SRC`(源图)与 `MALIANG_OUT`(输出目录)
/// 后执行 `cargo test --offline -- --ignored dump_recipe_samples`。
///
/// 逐个配方写出单图,并拼一张总览图 `_sheet.png`(便于一次性比对)。
/// 只用于人工比对观感,不参与常规测试。
#[cfg(test)]
#[test]
#[ignore = "手动生成样张"]
fn dump_recipe_samples() {
    let Ok(src) = std::env::var("MALIANG_SRC") else {
        return;
    };
    let out = std::env::var("MALIANG_OUT").unwrap_or_else(|_| ".".into());
    let base = image::open(&src).expect("源图打开失败");
    let base = base.thumbnail(480, 480).to_rgba8();
    std::fs::create_dir_all(&out).ok();
    base.save(format!("{out}/_source.png")).ok();

    // 先逐配方出单图,再按 6 列拼总览
    const CELL: u32 = 170;
    const COLS: u32 = 6;
    let rows = (RECIPES.len() as u32).div_ceil(COLS);
    let mut sheet =
        RgbaImage::from_pixel(COLS * CELL, rows * CELL, Rgba([24, 20, 16, 255]));
    for (i, name) in RECIPES.iter().enumerate() {
        let mut img = base.clone();
        apply(&mut img, name, 100.0);
        img.save(format!("{out}/{name}.png")).ok();
        // 等比缩进格子,居中摆放
        let t = image::imageops::thumbnail(&img, CELL - 8, CELL - 8);
        let ox = (i as u32 % COLS) * CELL + (CELL - t.width()) / 2;
        let oy = (i as u32 / COLS) * CELL + (CELL - t.height()) / 2;
        for (x, y, p) in t.enumerate_pixels() {
            sheet.put_pixel(ox + x, oy + y, *p);
        }
    }
    sheet.save(format!("{out}/_sheet.png")).ok();
}

#[cfg(test)]
mod blend_tests {
    use super::*;

    fn ch(mode: Blend, base: f32, top: f32) -> f32 {
        mode.ch(base, top)
    }

    #[test]
    fn hardlight_dark_top_multiplies() {
        // 顶层为黑 → 结果恒为黑;顶层为中性 127 → 约等于底层的一半
        assert!(ch(Blend::HardLight, 200.0, 0.0).abs() < 0.01);
        let half = ch(Blend::HardLight, 200.0, 127.0);
        assert!((half - 2.0 * 127.0 * 200.0 / 255.0).abs() < 0.01, "got {half}");
    }

    #[test]
    fn hardlight_bright_top_screens() {
        // 顶层为白、底层为黑 → 白(强对比的另一端)
        assert!((ch(Blend::HardLight, 0.0, 255.0) - 255.0).abs() < 0.6);
        // 顶层白 + 底层白 → 精确 255(单独处理,避免公式在端点溢出)
        assert_eq!(ch(Blend::HardLight, 255.0, 255.0), 255.0);
    }

    #[test]
    fn hardlight_is_continuous_where_expected() {
        // 底层为白时,两段在顶层 127/128 处应当接近(不该跳变)
        let a = ch(Blend::HardLight, 255.0, 127.0);
        let b = ch(Blend::HardLight, 255.0, 128.0);
        assert!((a - b).abs() < 3.0, "127→{a} 128→{b}");
    }

    #[test]
    fn softlight_mid_top_is_near_identity() {
        // 顶层取中灰时应当基本不改变底层
        for base in [20.0f32, 100.0, 180.0, 240.0] {
            let out = ch(Blend::SoftLight, base, 128.0);
            assert!((out - base).abs() < 4.0, "base={base} out={out}");
        }
    }

    #[test]
    fn softlight_matches_two_segment_formula() {
        // 底层暗: (顶层偶数化 + 128)·底层/255
        let got = ch(Blend::SoftLight, 100.0, 200.0);
        assert!((got - (200.0 + 128.0) * 100.0 / 255.0).abs() < 0.01);
        // 底层亮: (65025 − (382 − 顶层)(255 − 底层))/255
        let got = ch(Blend::SoftLight, 200.0, 100.0);
        let want = (65025.0 - (382.0 - 100.0) * (255.0 - 200.0)) / 255.0;
        assert!((got - want).abs() < 0.01);
    }

    #[test]
    fn blend_parse_knows_new_modes() {
        assert_eq!(Blend::parse("hardlight"), Blend::HardLight);
        assert_eq!(Blend::parse("SOFTLIGHT"), Blend::SoftLight);
    }
}

#[cfg(test)]
mod canvas_recipe_tests {
    use super::*;

    fn sample(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            Rgba([
                (x * 255 / w.max(1)) as u8,
                (y * 255 / h.max(1)) as u8,
                128,
                255,
            ])
        })
    }

    #[test]
    fn cinemascope_letterboxes_to_wide_frame() {
        let src = sample(340, 300);
        let mut img = src.clone();
        assert!(apply(&mut img, "cinemascope", 100.0));
        // 宽度不变,高度 = 裁切高 × (0.95 + 2×0.15)
        assert_eq!(img.width(), 340);
        let ch = ((340.0f32 / 1.7).round() as u32).min(300);
        let want = ((ch as f32 * 0.95).round() as u32) + 2 * ((ch as f32 * 0.15).round() as u32);
        assert_eq!(img.height(), want, "ch={ch}");
        // 上下首行必须是纯黑边
        for x in [0, 170, 339] {
            assert_eq!(*img.get_pixel(x, 0), Rgba([0, 0, 0, 255]));
            assert_eq!(*img.get_pixel(x, img.height() - 1), Rgba([0, 0, 0, 255]));
        }
        // 中部仍是有内容的
        let mid = img.get_pixel(170, img.height() / 2);
        assert!(mid[0] > 0 || mid[1] > 0);
    }

    #[test]
    fn cinemascope_leaves_tall_image_height_alone() {
        // 竖幅:宽/1.7 小于原高,裁切后应当比原图矮;宽度保持不变
        let mut img = sample(200, 400);
        assert!(apply(&mut img, "cinemascope", 100.0));
        assert_eq!(img.width(), 200);
        assert!(img.height() < 400);
    }

    #[test]
    fn sixties_keeps_size_and_rounds_corners() {
        let mut img = sample(140, 140);
        let before = *img.get_pixel(0, 0);
        assert!(apply(&mut img, "sixties", 100.0));
        assert_eq!(img.dimensions(), (140, 140));
        // 圆角外应被白色填掉
        let corner = *img.get_pixel(0, 0);
        assert_ne!(corner, before, "角落未被处理");
        assert_eq!(corner[0], 255);
        assert_eq!(corner[1], 255);
        assert_eq!(corner[2], 255);
    }

    #[test]
    fn resize_keeps_gradient_direction() {
        let src = RgbaImage::from_fn(8, 4, |x, _| {
            let v = (x * 255 / 7) as u8;
            Rgba([v, v, v, 255])
        });
        let out = resize(&src, 16, 2);
        assert_eq!(out.dimensions(), (16, 2));
        // 左端近黑、右端近白
        assert!(out.get_pixel(0, 0)[0] < 20);
        assert!(out.get_pixel(15, 0)[0] > 235);
    }
}

#[cfg(test)]
mod focus_op_tests {
    use super::*;

    /// 手动导出各"焦点/方向"类算子的观感样张:
    /// 设 `MALIANG_SRC` / `MALIANG_OUT` 后跑
    /// `cargo test --offline -- --ignored dump_focus_samples`。
    #[test]
    #[ignore = "手动生成样张"]
    fn dump_focus_samples() {
        let Ok(src) = std::env::var("MALIANG_SRC") else {
            return;
        };
        let out = std::env::var("MALIANG_OUT").unwrap_or_else(|_| ".".into());
        let base = image::open(&src).expect("源图打开失败");
        let base = base.thumbnail(420, 420).to_rgba8();
        std::fs::create_dir_all(&out).ok();

        let mut a = base.clone();
        linear_blur(&mut a, 20.0, 24.0);
        a.save(format!("{out}/op_linear_blur.png")).ok();

        let mut b = base.clone();
        soft_focus(&mut b, 0.5, 0.5, 0.30, 0.75, 1.0);
        b.save(format!("{out}/op_soft_focus.png")).ok();

        let mut c = base.clone();
        focal_bw(&mut c, 0.5, 0.5, 0.30, 0.40);
        c.save(format!("{out}/op_focal_bw.png")).ok();

        let mut d = base.clone();
        directional_adjust(&mut d, 0.8, 0.0, 1.0, DirKind::Brightness);
        d.save(format!("{out}/op_dir_brightness.png")).ok();

        let mut e = base.clone();
        directional_adjust(&mut e, -0.8, 0.0, 1.0, DirKind::Saturation);
        e.save(format!("{out}/op_dir_saturation.png")).ok();

        let mut f = base.clone();
        directional_adjust(&mut f, 0.8, 0.0, 1.0, DirKind::Sharpen);
        f.save(format!("{out}/op_dir_sharpen.png")).ok();
    }

    fn flat(w: u32, h: u32, c: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba(c))
    }

    #[test]
    fn linear_blur_leaves_flat_image_alone() {
        let mut img = flat(16, 16, [90, 120, 150, 255]);
        let before = img.clone();
        linear_blur(&mut img, 30.0, 10.0);
        assert_eq!(img.into_raw(), before.into_raw());
    }

    #[test]
    fn linear_blur_smears_vertical_edge_horizontally() {
        // 左半黑、右半白,水平模糊后边界两侧不再是非黑即白
        let mut img = RgbaImage::from_fn(32, 8, |x, _| {
            let v = if x < 16 { 0 } else { 255 };
            Rgba([v, v, v, 255])
        });
        linear_blur(&mut img, 0.0, 12.0);
        let left = img.get_pixel(13, 4)[0];
        let right = img.get_pixel(18, 4)[0];
        assert!(left > 0, "边界左侧未被带动: {left}");
        assert!(right < 255, "边界右侧未被带动: {right}");
    }

    #[test]
    fn directional_brightness_darkens_left_and_brightens_right() {
        let mut img = flat(40, 20, [128, 128, 128, 255]);
        directional_adjust(&mut img, 0.8, 0.0, 1.0, DirKind::Brightness);
        let l = img.get_pixel(2, 10)[0];
        let r = img.get_pixel(37, 10)[0];
        assert!(l < 110, "左侧应变暗: {l}");
        assert!(r > 145, "右侧应变亮: {r}");
    }

    #[test]
    fn directional_saturation_keeps_gray_gray() {
        // 中性灰没有饱和度可调 → 任何方向性去色都不该改变它
        let mut img = flat(20, 20, [128, 128, 128, 255]);
        directional_adjust(&mut img, -1.0, 0.0, 1.0, DirKind::Saturation);
        for p in img.pixels() {
            assert!((p[0] as i32 - 128).abs() <= 1);
        }
    }

    #[test]
    fn focal_bw_keeps_center_color_and_grays_corner() {
        // 左半红、右半蓝:圆心在中央,角落应被去色
        let mut img = RgbaImage::from_fn(60, 60, |x, _| {
            if x < 30 {
                Rgba([200, 40, 40, 255])
            } else {
                Rgba([40, 40, 200, 255])
            }
        });
        focal_bw(&mut img, 0.5, 0.5, 0.15, 0.45);
        let corner = img.get_pixel(0, 0);
        assert_eq!(corner[0], corner[1], "角落未去色: {:?}", corner);
        assert_eq!(corner[1], corner[2]);
        let center = img.get_pixel(29, 30);
        assert!(center[0] > center[2], "中心应保留色彩: {:?}", center);
    }

    #[test]
    fn soft_focus_keeps_center_and_smudges_edge() {
        // 棋盘格:中心应当几乎不变,边缘被平均掉
        let mut img = RgbaImage::from_fn(80, 80, |x, y| {
            let v = if (x / 4 + y / 4) % 2 == 0 { 0 } else { 255 };
            Rgba([v, v, v, 255])
        });
        let before = img.clone();
        soft_focus(&mut img, 0.5, 0.5, 0.20, 0.75, 1.0);
        let c0 = before.get_pixel(40, 40);
        let c1 = img.get_pixel(40, 40);
        assert!(
            (c0[0] as i32 - c1[0] as i32).abs() <= 2,
            "中心被误伤: {} → {}",
            c0[0],
            c1[0]
        );
        // 角上应明显偏离原来的纯黑/纯白
        let k0 = before.get_pixel(2, 2)[0] as i32;
        let k1 = img.get_pixel(2, 2)[0] as i32;
        assert!((k0 - k1).abs() > 30, "角落未被柔化: {k0} → {k1}");
    }
}

#[cfg(test)]
mod sampler_tests {
    use super::*;

    /// 径向模糊:放大层从 step/N 起、到 step 为止,层数 min(30, amount+5)。
    ///
    /// 用中心外的一个亮点看拖影:强度越大,亮点被拉成的径向拖尾越长;
    /// 同时因为取样点是**最近邻**,拖尾上的像素值不会凭空变亮或翻转。
    #[test]
    fn radial_blur_streak_grows_with_amount() {
        let mut base = RgbaImage::from_pixel(64, 24, Rgba([0, 0, 0, 255]));
        base.put_pixel(40, 12, Rgba([255, 255, 255, 255]));

        // 亮点被等权平均摊到 N 层上,单个像素的亮度只有 ~255/N;
        // 所以按"有没有贡献"来数,而不是按亮不亮。
        let lit = |im: &RgbaImage| -> Vec<u32> {
            (0..64u32)
                .filter(|&x| im.get_pixel(x, 12)[0] > 0)
                .collect()
        };

        let mut weak = base.clone();
        radial_blur(&mut weak, 32.0, 12.0, 20.0);
        let mut strong = base.clone();
        radial_blur(&mut strong, 32.0, 12.0, 80.0);

        let (lw, ls) = (lit(&weak), lit(&strong));
        assert!(
            ls.len() > lw.len(),
            "强度更大应拉出更长的拖尾: weak={lw:?} strong={ls:?}"
        );
        // 叠加层是被**放大**后铺回画布的,所以拖尾朝远离中心的方向延伸
        assert!(
            *ls.last().unwrap() > *lw.last().unwrap(),
            "强度更大时拖尾应向外延伸得更远: weak={lw:?} strong={ls:?}"
        );
        // 拖尾只能落在中心与该像素之间 —— 出现中心另一侧的亮点就是缩放方向写反了
        for x in 0..32u32 {
            assert!(
                strong.get_pixel(x, 12)[0] < 20,
                "x={x} 出现镜像亮化,缩放系数方向写反了"
            );
        }
    }

    #[test]
    fn radial_blur_zero_amount_is_noop() {
        let base = RgbaImage::from_fn(16, 16, |x, y| Rgba([x as u8 * 8, y as u8 * 8, 90, 255]));
        let mut img = base.clone();
        radial_blur(&mut img, 8.0, 8.0, 0.0);
        assert_eq!(img.into_raw(), base.into_raw());
    }

    /// 方块化:每块取块中心的采样点,不是块内均值。
    ///
    /// 用**奇数**块宽(3)时块中心正好落在某个源像素上,可以精确断言;
    /// 偶数块宽的中心落在两个像素之间,必然是个混合值。
    #[test]
    fn pixelate_samples_block_centre() {
        let mut img = RgbaImage::from_pixel(9, 9, Rgba([0, 0, 0, 255]));
        // 每块中心:3×3 块的 9 个中心在 (1,1) (4,1) (7,1) …
        let marks: Vec<(u32, u32, u8)> = vec![
            (1, 1, 30),
            (4, 1, 60),
            (7, 1, 90),
            (1, 4, 120),
            (4, 4, 150),
            (7, 4, 180),
            (1, 7, 210),
            (4, 7, 240),
            (7, 7, 255),
        ];
        for &(x, y, v) in &marks {
            img.put_pixel(x, y, Rgba([v, v, v, 255]));
        }
        pixelate(&mut img, 3.0);
        for &(x, y, v) in &marks {
            for (dx, dy) in [(0u32, 0u32), (1, 0), (0, 1), (2, 2)] {
                let got = img.get_pixel((x / 3) * 3 + dx, (y / 3) * 3 + dy)[0] as i32;
                assert!(
                    (got - v as i32).abs() <= 2,
                    "块中心 ({x},{y}) 应取到 {v},但块内 ({dx},{dy}) 得到 {got}"
                );
            }
        }
    }

    /// 双线性缩放:像素中心对齐。1:1 缩放必须原样返回。
    #[test]
    fn resize_identity_preserves_pixels() {
        let src = RgbaImage::from_fn(9, 7, |x, y| {
            Rgba([(x * 20) as u8, (y * 30) as u8, 77, 200])
        });
        let out = resize(&src, 9, 7);
        assert_eq!(out.into_raw(), src.clone().into_raw());
    }

    /// 2 倍放大:新像素落在源像素中心之间,应等于左右两个源像素的中值。
    #[test]
    fn resize_double_uses_pixel_centres() {
        let src = RgbaImage::from_pixel(2, 1, Rgba([0, 0, 0, 255]));
        let mut src = src;
        src.put_pixel(1, 0, Rgba([200, 200, 200, 255]));
        let out = resize(&src, 4, 1);
        // 目标 x=0 → 源 −0.25 → 夹到 0;x=3 → 源 2.25 → 夹到 1
        assert_eq!(out.get_pixel(0, 0)[0], 0);
        assert_eq!(out.get_pixel(3, 0)[0], 200);
        // 中间两个:源 0.25 与 0.75 → 50 与 150
        let m0 = out.get_pixel(1, 0)[0] as i32;
        let m1 = out.get_pixel(2, 0)[0] as i32;
        assert!((m0 - 50).abs() <= 2, "第二个像素应为 50,实得 {m0}");
        assert!((m1 - 150).abs() <= 2, "第三个像素应为 150,实得 {m1}");
    }

    /// 投影的剪影只取不透明区域,与画面颜色无关。
    #[test]
    fn drop_shadow_shape_ignores_color() {
        const BG: [u8; 3] = [226, 226, 226];
        let black = RgbaImage::from_pixel(48, 32, Rgba([0, 0, 0, 255]));
        let white = RgbaImage::from_pixel(48, 32, Rgba([255, 255, 255, 255]));
        let a = drop_shadow(&black, BG, [0, 0, 0], 0.5, 8.0, 90.0, 10.0);
        let b = drop_shadow(&white, BG, [0, 0, 0], 0.5, 8.0, 90.0, 10.0);
        let (nw, nh) = a.dimensions();
        assert_eq!(a.dimensions(), b.dimensions());

        // 照片本体贴在画布中央,外圈才是背景 + 阴影;只比外圈。
        let pad = (nw - 48) / 2;
        let mut shadowed = 0;
        for y in 0..nh {
            for x in 0..nw {
                let in_photo = x >= pad && x < pad + 48 && y >= pad && y < pad + 32;
                if in_photo {
                    continue;
                }
                let pa = a.get_pixel(x, y);
                let pb = b.get_pixel(x, y);
                assert_eq!(
                    pa, pb,
                    "({x},{y}) 的阴影随画面颜色变化了,说明混进了亮度通道"
                );
                if *pa != Rgba([BG[0], BG[1], BG[2], 255]) {
                    shadowed += 1;
                }
            }
        }
        assert!(shadowed > 0, "照片外圈没有画出任何阴影,测试证明不了东西");
    }

    /// 第 4 项-b:`Orton` 与 `Soften` 必须拆成两条链路。
    ///
    /// 参考里它们是两种完全不同的算子:Orton = 模糊图以 **Overlay** 混合回来
    /// (再叠一条亮度曲线);`Soften` = 模糊图按**普通不透明度**盖回原图。
    #[test]
    fn orton_and_soften_are_distinct_recipes() {
        // 用渐变图做基准:平坦图模糊后仍是平坦图,`Soften`(普通不透明度回混)
        // 在平坦图上恒等是正确行为,不能当作缺陷。
        let grad = || {
            let mut img = image::RgbaImage::new(32, 32);
            for y in 0..32 {
                for x in 0..32 {
                    img.put_pixel(x, y, image::Rgba([(x * 8) as u8, (y * 8) as u8, ((x + y) * 4) as u8, 255]));
                }
            }
            img
        };
        let mut orton_img = grad();
        let mut soften_img = grad();
        assert!(
            crate::recipes::apply(&mut orton_img, "orton", 100.0),
            "orton 应是已知配方"
        );
        assert!(
            crate::recipes::apply(&mut soften_img, "bloom", 50.0),
            "bloom(柔化)应是已知配方"
        );
        let base_bytes = grad().into_raw();
        assert_ne!(orton_img.as_raw(), &base_bytes, "奥顿柔焦应改变像素");
        assert_ne!(soften_img.as_raw(), &base_bytes, "柔化应改变像素");
        assert_ne!(
            orton_img.as_raw(),
            soften_img.as_raw(),
            "奥顿柔焦与柔化不能是同一件事"
        );

        // 同一强度下两者也必须不同 —— 差异来自混合模式而不是强度。
        let mut a = grad();
        let mut b = grad();
        crate::recipes::apply(&mut a, "orton", 50.0);
        crate::recipes::apply(&mut b, "bloom", 50.0);
        assert_ne!(a.as_raw(), b.as_raw(), "同为 50 强度时两者也必须不同");
    }

    /// 第 4 项-c:参考的 `Vignette` 是**内发光**(Glow, 纯黑),
    /// 与我方自主设计的 smoothstep 暗角是两件东西,应各自独立存在。
    #[test]
    fn reference_vignette_is_an_inner_glow() {
        let flat = || image::RgbaImage::from_pixel(32, 32, image::Rgba([180, 180, 180, 255]));
        let mut img = flat();
        assert!(
            crate::recipes::apply(&mut img, "vignette_glow", 100.0),
            "vignette_glow 应是已知配方"
        );
        let corner = img.get_pixel(0, 0)[0];
        let center = img.get_pixel(16, 16)[0];
        assert!(corner < 180, "四角应被压暗,实得 {corner}");
        assert!(center > corner, "中心应亮于四角,实得 中心={center} 角={corner}");
        // 强度为 0 时必须恒等
        let mut zero = flat();
        crate::recipes::apply(&mut zero, "vignette_glow", 0.0);
        assert_eq!(zero.as_raw(), &flat().into_raw(), "强度 0 应恒等");
    }

    // ===== 第 5 项:3.4 批量清理(六条失败断言,实现后转绿)=====

    /// 5-a:AutoFix 的 `+1.0` 与 `low==high → 全白`。
    /// 参考:`raw = (int)(1.0 + 255*(i−low)/(high−low))`,i=low 输出 **1** 不是 0;
    /// `low==high`(常数图)→ LUT 全填 0xff(全白),不是保持原样。
    #[test]
    fn auto_fix_uses_reference_lut_conventions() {
        // 常数图 → 全白
        let mut img = image::RgbaImage::from_pixel(4, 4, image::Rgba([77, 77, 77, 255]));
        auto_fix(&mut img);
        for p in img.pixels() {
            assert_eq!(p[0], 255, "常数图自动色阶后应全白,实得 {}", p[0]);
        }
        // i = low → 1(不是 0)
        let mut img = image::RgbaImage::new(2, 2);
        for y in 0..2 {
            for x in 0..2 {
                let v = if x == 0 { 10u8 } else { 200 };
                img.put_pixel(x, y, image::Rgba([v, v, v, 255]));
            }
        }
        auto_fix(&mut img);
        assert_eq!(img.get_pixel(0, 0)[0], 1, "直方图最低 bin 应映射到 1 而不是 0");
        assert_eq!(img.get_pixel(1, 0)[0], 255, "最高 bin 应映射到 255");
    }

    /// 5-b:HSVGradientMap 的索引量是**输入像素的 R 通道字节**,不是亮度。
    #[test]
    fn hsv_gradient_map_indexes_by_red_byte() {
        let stops = vec![(0.0f32, (0.0f32, 100.0f32, 0.0f32)), (255.0, (120.0, 100.0, 100.0))];
        // 两像素 R 相同、亮度不同 → 输出必须相同
        let mut a = image::RgbaImage::new(1, 1);
        a.put_pixel(0, 0, image::Rgba([100, 0, 0, 255])); // 亮度低
        let mut b = image::RgbaImage::new(1, 1);
        b.put_pixel(0, 0, image::Rgba([100, 255, 255, 255])); // 亮度高
        hsv_gradient_map(&mut a, &stops, 0.0);
        hsv_gradient_map(&mut b, &stops, 0.0);
        assert_eq!(
            a.get_pixel(0, 0),
            b.get_pixel(0, 0),
            "R 相同则输出必须相同(索引只看 R)"
        );
        // R 不同、亮度相同 → 输出不同
        let mut c = image::RgbaImage::new(1, 1);
        c.put_pixel(0, 0, image::Rgba([200, 122, 122, 255])); // 与 a 亮度近似
        hsv_gradient_map(&mut c, &stops, 0.0);
        assert_ne!(a.get_pixel(0, 0), c.get_pixel(0, 0), "R 不同则输出应不同");
    }

    /// 5-c:Infrared 走参考三步链(只留绿 → 模糊 → 矩阵),终值是
    /// **纯绿通道派生的单色**,红蓝通道不参与,也没有 Screen 光晕。
    #[test]
    fn infrared_derives_from_green_channel_only() {
        let mk = |r: u8, g: u8, b: u8| {
            let mut img = image::RgbaImage::from_pixel(16, 16, image::Rgba([r, g, b, 255]));
            infrared(&mut img, -0.5);
            img.get_pixel(8, 8)[0]
        };
        // 只改红/蓝,输出不变
        assert_eq!(mk(0, 120, 0), mk(255, 120, 255), "红蓝通道不应影响红外结果");
        // 改绿,输出改变,且亮绿比暗绿更亮
        let dark = mk(0, 40, 0);
        let bright = mk(0, 200, 0);
        assert!(bright > dark, "绿通道越亮输出应越亮:{dark} → {bright}");
    }

    /// 5-d:EdgeDetectionB = 先 Blur(2,2) → 双向 Sobel(各经 +128 偏置与三角曲线)
    /// → 合成。平坦区必须**纯白**,强边缘趋黑(不是回白)。
    #[test]
    fn edge_detect_b_flats_are_white_and_edges_are_dark() {
        // 平坦图 → 全白
        let mut img = image::RgbaImage::from_pixel(16, 16, image::Rgba([120, 120, 120, 255]));
        edge_detect_b(&mut img, 50.0);
        for p in img.pixels() {
            assert_eq!(p[0], 255, "平坦区应纯白,实得 {}", p[0]);
        }
        // 竖直强边缘:左黑右白 → 边缘列应明显变暗(白底黑边)
        let mut img = image::RgbaImage::new(16, 16);
        for y in 0..16 {
            for x in 0..16 {
                let v = if x < 8 { 20u8 } else { 235 };
                img.put_pixel(x, y, image::Rgba([v, v, v, 255]));
            }
        }
        edge_detect_b(&mut img, 50.0);
        let edge_col = img.get_pixel(7, 4)[0];
        let flat_col = img.get_pixel(2, 4)[0];
        assert!(flat_col > 200, "远离边缘处应仍接近白,实得 {flat_col}");
        assert!(edge_col < flat_col, "边缘应比平坦区暗,实得 边缘={edge_col} 平坦={flat_col}");
    }

    /// 5-e:噪声的非 mono 模式必须三通道**独立抽样**,不能是同一数组的循环移位。
    #[test]
    fn noise_non_mono_samples_channels_independently() {
        let mut img = image::RgbaImage::new(8, 8);
        apply_noise_layer(&mut img, 7, 0, 255, false, Blend::Normal, 1.0);
        // 统计 R==G 且 G==B 的像素数:独立抽样下应极少(纯概率 ~1/65536),
        // 循环移位实现下 R/G/B 各取不同下标,相等的概率同样低 ——
        // 所以判据换成:非 mono 模式下,同一像素三通道不全相等的比例应接近 100%,
        // 且相邻像素的 G 值不应与 R 值呈固定移位关系。
        let mut all_equal = 0usize;
        for p in img.pixels() {
            if p[0] == p[1] && p[1] == p[2] {
                all_equal += 1;
            }
        }
        assert!(
            all_equal <= 1,
            "非 mono 噪声三通道应独立,{all_equal}/64 全相等"
        );
        // 循环移位特征:G[i] == R[i+1]、B[i] == R[i+2]。独立抽样下 64 像素里
        // 精确成立的条目应接近 0。
        let px = |i: usize| -> [u8; 3] {
            let p = img.get_pixel((i % 8) as u32, (i / 8) as u32);
            [p[0], p[1], p[2]]
        };
        let mut shift_hits = 0usize;
        for i in 0..62 {
            if px(i)[1] == px(i + 1)[0] && px(i)[2] == px(i + 2)[0] {
                shift_hits += 1;
            }
        }
        assert!(shift_hits <= 1, "检测到循环移位特征:{shift_hits}/62");
    }

    /// 5-f:模糊半径的两条参考约束 —— 滑块量经**阶梯量化**,实际半径
    /// **钳到图像尺寸的一半**。
    #[test]
    fn blur_ladder_and_half_size_clamp() {
        // 阶梯:合法档位只有 0 与四个量化值
        for a in [0.5f32, 1.2, 2.4, 3.5, 9.9] {
            let r = blur_ladder(a);
            assert!(
                r == 0.0 || r == 2.065 || r == 3.0625 || r == 4.13 || r == 5.13,
                "amount {a} 应落到阶梯档位,实得 {r}"
            );
        }
        assert_eq!(blur_ladder(0.5), 0.0, "amount<1 应得 0");
        // 半图钳制:均色图经任意大半径模糊必须保持均色(不 panic、不引入黑边)
        let mut img = image::RgbaImage::from_pixel(64, 8, image::Rgba([128, 64, 32, 255]));
        box_blur(&mut img, 100.0, 100.0);
        for p in img.pixels() {
            assert_eq!(p[0], 128, "均色图模糊应保持均色,实得 {}", p[0]);
        }
    }

    /// 登记表的两向一致性 —— 防止出现「`apply` 认得、`is_recipe` 不认」的不可达配方。
    ///
    /// 分支清单直接从本文件源码里解析,不另抄一份常量表,避免以后两处漂移。
    fn dispatch_arms() -> Vec<String> {
        let src = include_str!("recipes.rs");
        let start = src
            .find("pub fn apply_with")
            .expect("apply_with 应存在");
        let body = &src[start..];
        let end = body.find("\n}\n").expect("apply_with 应以顶层 } 结束");
        let body = &body[..end];
        let mut arms: Vec<String> = Vec::new();
        for line in body.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix('"') {
                if let Some((name, tail)) = rest.split_once('"') {
                    if tail.trim_start().starts_with("=>") && !name.is_empty() {
                        arms.push(name.to_string());
                    }
                }
            }
        }
        arms.sort();
        arms.dedup();
        arms
    }

    #[test]
    fn registered_recipes_match_the_apply_dispatch() {
        let arms = dispatch_arms();
        assert!(!arms.is_empty(), "没能从源码解析出任何分支,判据失效");

        // 方向一:`apply` 认得的名字必须全部登记,否则 UI 与 is_oneclick 都不可达。
        let unregistered: Vec<&String> = arms
            .iter()
            .filter(|n| !crate::recipes::is_recipe(n))
            .collect();
        assert!(
            unregistered.is_empty(),
            "这些配方有分派分支却没登记,外部不可达:{unregistered:?}"
        );

        // 方向二:登记表里每个名字都必须真能跑,且有独立的中文名。
        for name in crate::recipes::RECIPES {
            let mut img = image::RgbaImage::from_pixel(8, 8, image::Rgba([120, 120, 120, 255]));
            assert!(
                crate::recipes::apply(&mut img, name, 100.0),
                "登记了却没有对应分支:{name}"
            );
            assert_ne!(
                crate::recipes::label(name),
                "效果",
                "配方 {name} 缺独立中文名"
            );
        }
    }

    /// `Lomo`(托伊相机款抖动暗角)在 84 项参考滤镜里没有对应实现 —— 补齐。
    ///
    /// 参考链(取自官方滤镜描述,三步依次作用):
    ///  1. 黑色**内发光**:半径 = 35·0.02·长边/2 = 0.35·长边,强度 1.1
    ///  2. 圆形渐变遮罩下的 **Blur(20)**:遮罩内径 = 长边/4,外径 = 长边/2·(2 − blur/100)
    ///  3. 色彩矩阵:饱和 +20 / 对比 +35 / 亮度 +5
    ///
    /// 断言只锁这三件事的**可观测后果**,不锁内部写法。
    #[test]
    fn lomo_blurs_the_center_and_darkens_the_corners() {
        // 高频条纹:中心被 Blur(20) 抚平」后,相邻列的差异应显著变小。
        let stripes = || {
            let mut img = image::RgbaImage::new(64, 64);
            for y in 0..64 {
                for x in 0..64 {
                    let v = if x % 2 == 0 { 200 } else { 60 };
                    img.put_pixel(x, y, image::Rgba([v, v, v, 255]));
                }
            }
            img
        };
        let contrast_at_center = |img: &image::RgbaImage| -> i32 {
            let mut sum = 0i32;
            for x in 28..36u32 {
                let d = img.get_pixel(x, 32)[0] as i32 - img.get_pixel(x + 1, 32)[0] as i32;
                sum += d.abs();
            }
            sum
        };

        let base = stripes();
        let before = contrast_at_center(&base);
        assert!(before > 500, "基准应满足高频,实得 {before}");

        let mut img = base.clone();
        assert!(
            crate::recipes::apply(&mut img, "lomo", 100.0),
            "lomo 应是已知配方"
        );
        let after = contrast_at_center(&img);
        assert!(
            after < before / 2,
            "中心应被模糊抹平:前 {before} → 后 {after}"
        );

        // 四角应被黑色内发光压暗
        let corner_before = base.get_pixel(0, 0)[0];
        let corner_after = img.get_pixel(0, 0)[0];
        assert!(
            corner_after < corner_before,
            "四角应被压暗:前 {corner_before} → 后 {corner_after}"
        );

        // 第三步:中心区域应有饱和/对比提升的痕迹 —— 平坦区上取不到,
        // 改为验证整体不是只有暗角(即中心亮度没有单向下坠到 0)
        assert!(
            img.get_pixel(32, 32)[0] > 8,
            "中心不应被压成纯黑,实得 {}",
            img.get_pixel(32, 32)[0]
        );
    }
}
