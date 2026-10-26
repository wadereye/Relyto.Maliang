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

use image::{imageops::FilterType, Rgba, RgbaImage};

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
pub fn box_blur(img: &mut RgbaImage, rx: f32, ry: f32) {
    let rx = rx.round().max(0.0) as i32;
    let ry = ry.round().max(0.0) as i32;
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
pub fn auto_fix(img: &mut RgbaImage) {
    let mut lo = [255u8; 3];
    let mut hi = [0u8; 3];
    for p in img.pixels() {
        for c in 0..3 {
            lo[c] = lo[c].min(p[c]);
            hi[c] = hi[c].max(p[c]);
        }
    }
    let mut lut = [[0u8; 256]; 3];
    for c in 0..3 {
        let (a, b) = (lo[c] as f32, hi[c] as f32);
        let span = (b - a).max(1.0);
        for i in 0..256 {
            lut[c][i] = (((i as f32 - a) / span) * 255.0).round().clamp(0.0, 255.0) as u8;
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

    for p in img.pixels_mut() {
        let y = luma8(p).round().clamp(0.0, 255.0) as usize;
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

/// 边缘检测(描边):Sobel 梯度 → 三角曲线 → 反相,得到**白底黑边**。
///
/// ```text
/// g  = |∇I| · detail/50            // detail 是参考产品的细节滑杆,缺省 50
/// t  = 三角(g)                     // 0→0, 128→255, 255→0
/// out = 255 − t                    // 平坦区纯白,强边缘趋黑
/// ```
///
/// 输出是灰度图(`R=G=B`),所以后续可以直接乘到别的层上做描边。
pub fn edge_detect_b(img: &mut RgbaImage, detail: f32) {
    let (w, h) = img.dimensions();
    if w < 3 || h < 3 {
        return;
    }
    let src = img.clone();
    let gray: Vec<f32> = src.pixels().map(luma8).collect();
    let scale = (detail / 50.0).max(0.1);
    let (iw, ih) = (w as i32, h as i32);
    let g = |x: i32, y: i32| -> f32 {
        let xx = x.clamp(0, iw - 1) as usize;
        let yy = y.clamp(0, ih - 1) as usize;
        gray[yy * (w as usize) + xx]
    };
    for y in 0..ih {
        for x in 0..iw {
            let gx = (g(x + 1, y - 1) + 2.0 * g(x + 1, y) + g(x + 1, y + 1))
                - (g(x - 1, y - 1) + 2.0 * g(x - 1, y) + g(x - 1, y + 1));
            let gy = (g(x - 1, y + 1) + 2.0 * g(x, y + 1) + g(x + 1, y + 1))
                - (g(x - 1, y - 1) + 2.0 * g(x, y - 1) + g(x + 1, y - 1));
            let mag = ((gx * gx + gy * gy).sqrt() * scale).clamp(0.0, 255.0);
            let tri = if mag < 128.0 {
                mag * 2.0
            } else {
                (255.0 - mag) * 2.0
            };
            let v = (255.0 - tri).clamp(0.0, 255.0).round() as u8;
            let p = img.get_pixel_mut(x as u32, y as u32);
            p[0] = v;
            p[1] = v;
            p[2] = v;
        }
    }
}

/// 径向模糊(变焦模糊):以 (`cx`,`cy`) 为中心按等比缩放采样并平均。
///
/// ```text
/// N    = min(30, amount + 5)               // 采样层数
/// s_k  = 1 + k·(1/N − 1)                   // 第 k 层相对原图的缩放
/// out  = mean( orig(c + (p − c)·s_k) )
/// ```
///
/// `amount` 越大层数越多、最外圈缩得越小,拖影越长。
pub fn radial_blur(img: &mut RgbaImage, cx: f32, cy: f32, amount: f32) {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return;
    }
    let n = ((amount.max(0.0) as i32) + 5).min(30).max(2);
    let src = img.clone();
    let (iw, ih) = (w as i32, h as i32);
    for y in 0..ih {
        for x in 0..iw {
            let mut acc = [0f32; 3];
            for k in 0..n {
                let s = 1.0 + (k as f32) * (1.0 / n as f32 - 1.0);
                let sx = (cx + ((x as f32 + 0.5) - cx) * s).round() as i32;
                let sy = (cy + ((y as f32 + 0.5) - cy) * s).round() as i32;
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

/// 方块化:先按 `block` 降采样再按最近邻放大回原尺寸。
///
/// 降采样用三角滤波(取块内均值),放大用最近邻(保持硬块边界)——
/// 两步合起来等价于"每块一个颜色"。
pub fn pixelate(img: &mut RgbaImage, block: f32) {
    let b = block.round().max(2.0) as u32;
    let (w, h) = img.dimensions();
    if w <= b || h <= b {
        return;
    }
    let (sw, sh) = (w / b, h / b);
    let small = image::imageops::resize(img, sw.max(1), sh.max(1), FilterType::Triangle);
    *img = image::imageops::resize(&small, w, h, FilterType::Nearest);
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

/// 生成一层灰度噪声(0~255),用于各种颗粒配方。
///
/// 随机数沿用本项目统一的发生器(与胶片颗粒同源),`low`/`high` 限定取值范围,
/// 取值先在 `[low,high]` 内均匀,再按 `spread` 向中灰收敛(`spread=1` 为原样)。
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
    for (i, p) in layer.pixels_mut().enumerate() {
        if gray {
            p[0] = noise[i];
            p[1] = noise[i];
            p[2] = noise[i];
        } else {
            // 三通道各自取不同噪声(通道掩码全开的情形)
            p[0] = noise[i];
            p[1] = noise[(i + 1) % noise.len()];
            p[2] = noise[(i + 2) % noise.len()];
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

/// 双线性缩放到指定尺寸(仅用于配方内部小幅调整,不做高质量降采样)。
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

/// 单色红外:保留绿通道主导的亮度,再叠一层柔化提亮。
///
/// ```text
/// base = k·R + 2·G + (−1−k)·B        // k = redweight(缺省 −0.5)
/// gray = clamp(base, 0, 255)
/// out  = gray + blur(gray)·glow_alpha  // 绿光晕,呈现红外片的高光溢出
/// ```
pub fn infrared(img: &mut RgbaImage, red_weight: f32, glow_alpha: f32) {
    let k = red_weight;
    for p in img.pixels_mut() {
        let v = (k * p[0] as f32 + 2.0 * p[1] as f32 + (-1.0 - k) * p[2] as f32)
            .clamp(0.0, 255.0);
        let v = v.round() as u8;
        p[0] = v;
        p[1] = v;
        p[2] = v;
    }
    if glow_alpha > 0.0 {
        let mut blurred = img.clone();
        let (w, h) = img.dimensions();
        let r = (w.max(h) as f32 * 0.02).max(1.0);
        box_blur(&mut blurred, r, r);
        composite(img, &blurred, Blend::Screen, glow_alpha);
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
        // 强动态:大半径局部对比,观感接近 HDR
        "hdr" => {
            local_contrast(img, 20.0, 3.0);
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
            infrared(img, -0.5, 0.25);
        }
        // 局部对比:中小半径、温和强度
        "local_contrast" => {
            local_contrast(img, 15.0, 1.5);
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
        // 柔光:大半径模糊以叠加模式半量回混,再走一条抬中间调的曲线
        "bloom" => {
            let mut layer = img.clone();
            box_blur(&mut layer, 20.0, 20.0);
            composite(img, &layer, Blend::Overlay, 0.45);
            let c = crate::curves::presets::soft_glow(62.0);
            crate::curves::apply_curves(img, &c);
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
