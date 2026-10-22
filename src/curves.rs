//! 色调曲线引擎:自然三次样条 + 四通道曲线 + 曝光。
//!
//! # 行为要点
//!
//! | 我方实现 | 环节 | 说明 |
//! |---|---|---|
//! | `Curve` 控制点 | 曲线控制点的构造 | 点 = `{x, y}` 两个 f32 |
//! | 曲线容器 | 曲线容器的组织 | 点数组 / 点数 / M 数组 / M 计数 / 派生缓存 |
//! | `spline()` 自然三次样条 | 样条求值 | M-形式求值 + 二分定位 |
//! | M[] 求解 | 样条系数求解 | 自然边界 `M₀=M_{n-1}=0`,三对角 Thomas 消元 |
//! | 256 项 LUT | 查找表构建 | 每次 apply 现算;按 B / G / R 各一张 |
//! | 4 曲线合成 | 四曲线合成 | Master 先算得公共 `m`,R/G/B **各自以同一个 m 为输入** |
//! | 曝光曲线点 | 曝光整形点 | 正档 4 点 / 负档 4 点,写入 Master 槽 |
//! | 曝光迭代 | 曝光迭代 | 迭代 N+1 次,末两步按小数权重 `w` 线性混合 |
//! | 舍入 | 查找表构建 | `round(x) = (int)(x+0.5)`,随后 clamp `[0,255]` |
//!
//! # 关键结论(与直觉相反,勿按直觉改)
//!
//! 1. **插值是自然三次样条(natural cubic spline,M-形式)**,不是线性、不是单调三次
//!    (PCHIP/Fritsch-Carlson)、不是 Catmull-Rom。
//!    求解器**无任何斜率符号判断或钳制** → **不保单调**。
//! 2. **不钳制到端点**:二分查找只保证落在某段 `[x_lo, x_hi]`,输入超出控制点 x 范围时
//!    取端点段做**多项式外插**(`u` 或 `v` 可 >1 / <0)。
//! 3. **全程在 sRGB 码值域(0..255)求值**,无 gamma 编码/解码、无线性光域转换。
//! 4. **4 条曲线不是串联,也不是加权混合**:Master 先算出公共中间值 `m`,
//!    R/G/B 三条曲线**各自以同一个 m 为输入**独立求值 → 完全并行、互不影响。
//! 5. **曝光曲线点与 预设 `MasterCurve` 点合并进同一 Master 槽**,作为**一条样条**统一求值。
//!
//! # 公式
//!
//! ```text
//! // 自然三次样条 M-形式(v 落在 [x_lo, x_hi])
//! h = x_hi - x_lo
//! u = (x_hi - v) / h
//! w = (v - x_lo) / h
//! S(v) = y_lo·u + y_hi·w + (h²/6)·[ M_lo·(u³−u) + M_hi·(w³−w) ]
//!
//! // M 由三对角系统解出,自然边界 M₀ = M_{n−1} = 0
//! h_i M_{i−1} + 2(h_{i−1}+h_i) M_i + h_i M_{i+1} = 6·[ (y_{i+1}−y_i)/h_i − (y_i−y_{i−1})/h_{i−1} ]
//!
//! // 主流程(i = 0..255)
//! m   = MasterSpline_曝光迭代(i)
//! R   = clamp(round(RedSpline(m)))    // R 通道查找表
//! G   = clamp(round(GreenSpline(m)))  // G 通道查找表
//! B   = clamp(round(BlueSpline(m)))   // B 通道查找表
//! ```

/// 一个曲线控制点。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Point {
        Point { x, y }
    }
}

/// 一条色调曲线(自然三次样条)。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Curve {
    pub points: Vec<Point>,
}

impl Curve {
    pub fn new() -> Curve {
        Curve { points: Vec::new() }
    }

    /// 从控制点构造(原版曲线控制点的构造逐点追加)。
    pub fn from_points(points: &[Point]) -> Curve {
        Curve {
            points: points.to_vec(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// 求值(原版样条求值)。
    ///
    /// - 空曲线 → 恒等返回 `x`(样条求值)。
    /// - 单点 → 恒等(无区间可插值)。
    /// - 越界 → **外插**(不钳制),与原版一致。
    pub fn eval(&self, x: f32) -> f32 {
        let n = self.points.len();
        if n < 2 {
            return x;
        }
        let m = self.second_derivatives();
        // 二分查找:定位使 x[lo] <= x <= x[hi] 的区间。
        // 原版在 x 越界时取端点段(下标 0 或 n-2)。
        let (lo, hi) = if x <= self.points[0].x {
            (0, 1)
        } else if x >= self.points[n - 1].x {
            (n - 2, n - 1)
        } else {
            let mut lo = 0usize;
            let mut hi = n - 1;
            while hi - lo > 1 {
                let mid = (lo + hi) / 2;
                if self.points[mid].x <= x {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            (lo, hi)
        };
        let h = self.points[hi].x - self.points[lo].x;
        if h == 0.0 {
            // 退化区间(控制点 x 重复)→ 取 lo 点的值,避免除零。
            return self.points[lo].y;
        }
        let u = (self.points[hi].x - x) / h;
        let w = (x - self.points[lo].x) / h;
        let y_lo = self.points[lo].y;
        let y_hi = self.points[hi].y;
        let m_lo = m[lo];
        let m_hi = m[hi];
        y_lo * u + y_hi * w + ((u * u * u - u) * m_lo + (w * w * w - w) * m_hi) * (h * h) / 6.0
    }

    /// 二阶导数 M[](原版样条系数求解,自然边界 `M₀=M_{n−1}=0`)。
    ///
    /// 每次调用现算(与 Picasa 3 的"懒计算后缓存"行为等价)。
    fn second_derivatives(&self) -> Vec<f32> {
        let n = self.points.len();
        let mut m = vec![0f32; n];
        if n < 3 {
            return m;
        }
        // 标准自然样条三对角系统,Thomas 消元。
        let mut sub = vec![0f32; n]; // c'
        let mut diag = vec![0f32; n]; // b
        let mut sup = vec![0f32; n]; // a
        let mut rhs = vec![0f32; n]; // d
        for i in 1..n - 1 {
            let h_prev = self.points[i].x - self.points[i - 1].x;
            let h_next = self.points[i + 1].x - self.points[i].x;
            if h_prev == 0.0 || h_next == 0.0 {
                continue;
            }
            let alpha = 3.0 * ((self.points[i + 1].y - self.points[i].y) / h_next
                - (self.points[i].y - self.points[i - 1].y) / h_prev);
            if i == 1 {
                diag[i] = 2.0 * (h_prev + h_next);
                rhs[i] = alpha;
            } else {
                let ratio = h_prev / diag[i - 1];
                diag[i] = 2.0 * (h_prev + h_next) - ratio * h_next;
                rhs[i] = alpha - ratio * rhs[i - 1];
            }
            sup[i] = h_next;
        }
        // 回代(自然边界 M₀ = M_{n-1} = 0 → 边界方程退化为 diag[1]·M₁ = rhs[1])
        for i in (1..n - 1).rev() {
            m[i] = (rhs[i] - sup[i] * m[i + 1]) / diag[i];
        }
        let _ = &mut sub;
        m
    }
}

/// 4 条色调曲线(Master / Red / Green / Blue)+ 曝光。
///
/// 四条曲线 + 曝光档位:
/// Master / Red / Green / Blue 四条曲线,
/// 外加曝光整数档与曝光小数权重两个槽。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Curves {
    pub master: Curve,
    pub red: Curve,
    pub green: Curve,
    pub blue: Curve,
    /// 曝光档位(stops)。
    pub exposure: f32,
}

impl Curves {
    pub fn new() -> Curves {
        Curves::default()
    }

    /// 是否为恒等(无曲线、无曝光)。
    pub fn is_identity(&self) -> bool {
        self.master.is_empty()
            && self.red.is_empty()
            && self.green.is_empty()
            && self.blue.is_empty()
            && self.exposure == 0.0
    }

    /// 曝光整数档 `N` 与小数权重 `w`(原版曝光档位与权重)。
    ///
    /// 原版把 `stops` 拆成整数档 `N` 与小数权重 `w`,再迭代 N+1 次。
    fn exposure_parts(&self) -> (u32, f32) {
        let e = self.exposure;
        if e == 0.0 {
            return (0, 0.0);
        }
        let n = e.abs().floor() as u32;
        let w = e.abs() - n as f32;
        let (n, w) = if e < 0.0 { (n, w) } else { (n, w) };
        (n, w)
    }

    /// Master 槽的完整点集:曝光整形点(若有)在前 + 预设 MasterCurve 点在后。
    ///
    /// 原版四曲线合成:曝光整形点先写曝光 4 点,
    /// 预设曲线追加再把 预设 Master 点**追加**到同一槽 → 作为**一条样条**统一求值。
    fn master_spline(&self) -> Curve {
        let mut pts: Vec<Point> = Vec::new();
        if self.exposure != 0.0 {
            pts.extend_from_slice(&exposure_points(self.exposure));
        }
        pts.extend_from_slice(&self.master.points);
        Curve { points: pts }
    }

    /// Master 求值(含曝光迭代,原版曝光迭代)。
    ///
    /// ```text
    /// v_{k+1} = MasterSpline(v_k),迭代 N+1 次
    /// m = v_N + w·(v_{N+1} − v_N)
    /// ```
    fn master_value(&self, x: f32) -> f32 {
        let spline = self.master_spline();
        if spline.is_empty() {
            return x;
        }
        let (n, w) = self.exposure_parts();
        if n == 0 && w == 0.0 {
            return spline.eval(x);
        }
        let mut v = x;
        let mut last = x;
        // 迭代 N+1 次
        for _ in 0..=n {
            last = v;
            v = spline.eval(v);
        }
        // 末两步按小数权重线性混合
        last + w * (v - last)
    }

    /// 构建 256 项 LUT(原版查找表构建)。
    ///
    /// 返回 `[[r,g,b]; 256]`,索引即输入码值。
    pub fn build_lut(&self) -> [[u8; 3]; 256] {
        let mut lut = [[0u8; 3]; 256];
        let has_any = !self.master.is_empty()
            || !self.red.is_empty()
            || !self.green.is_empty()
            || !self.blue.is_empty()
            || self.exposure != 0.0;
        if !has_any {
            return lut;
        }
        for i in 0..256usize {
            let m = self.master_value(i as f32);
            let r = self.red.eval(m);
            let g = self.green.eval(m);
            let b = self.blue.eval(m);
            lut[i] = [
                clamp_u8(r),
                clamp_u8(g),
                clamp_u8(b),
            ];
        }
        lut
    }
}

/// 原版舍入 + 钳位(查找表构建)。
///
/// `round(x) = (int)(x + 0.5)`(round-half-up),随后 clamp 到 `[0,255]`。
fn clamp_u8(v: f32) -> u8 {
    let r = (v + 0.5).floor();
    if r <= 0.0 {
        0
    } else if r >= 255.0 {
        255
    } else {
        r as u8
    }
}

/// 曝光整形点(取自内置资源包 `exposure.pos` / `exposure.neg`)。
///
/// 档位为 0 时返回空(此时清空 Master 槽)。
pub fn exposure_points(stops: f32) -> Vec<Point> {
    if stops == 0.0 {
        return Vec::new();
    }
    let key = if stops > 0.0 { "exposure.pos" } else { "exposure.neg" };
    crate::assets::pairs(key).into_iter().map(|(x, y)| Point::new(x, y)).collect()
}

/// 把 LUT 应用到图像(逐像素查表)。
///
/// 查找表由 apply 时现算,像素按码值查表。
pub fn apply_lut(img: &mut image::RgbaImage, lut: &[[u8; 3]; 256]) {
    for p in img.pixels_mut() {
        let c = lut[p[0] as usize];
        p[0] = c[0];
        p[1] = c[1];
        p[2] = c[2];
    }
}

/// 把曲线直接应用到图像(现算 LUT + 查表)。
pub fn apply_curves(img: &mut image::RgbaImage, c: &Curves) {
    if c.is_identity() {
        return;
    }
    let lut = c.build_lut();
    apply_lut(img, &lut);
}

// ===== 内置滤镜曲线预设(数据取自资源包 `assets/presets.dat`) =====

/// 内置滤镜预设的曲线数据(控制点存放在资源包,勿随意改动)。
///
/// 每个预设由 4 条曲线组成,对应资源包里的
/// `curve.<预设名>.master` / `.red` / `.green` / `.blue` 四段;
/// 段内数值按 `x0 y0 x1 y1 …` 成对排列。空段表示该通道恒等。
pub mod presets {
    use super::{Curve, Curves, Point};
    use crate::assets;

    /// 从资源包读一条曲线;段不存在或为空时返回恒等曲线。
    fn curve_of(name: &str) -> Curve {
        let pts: Vec<Point> = assets::pairs(name).into_iter().map(|(x, y)| Point::new(x, y)).collect();
        if pts.is_empty() {
            Curve::new()
        } else {
            Curve::from_points(&pts)
        }
    }

    /// 从资源包读一组四通道曲线。
    fn curves_of(prefix: &str) -> Curves {
        Curves {
            master: curve_of(&format!("curve.{prefix}.master")),
            red: curve_of(&format!("curve.{prefix}.red")),
            green: curve_of(&format!("curve.{prefix}.green")),
            blue: curve_of(&format!("curve.{prefix}.blue")),
            exposure: 0.0,
        }
    }

    /// 参数化预设:按 `curve.<名>.form` 段的 `[y 基准, 参数基准, 增益]`
    /// 调整中间控制点的 y 值(其余点不变):
    /// `y = y 基准 + (参数 − 参数基准) × 增益`。
    fn apply_form(prefix: &str, param: f32) -> Curves {
        let mut c = curves_of(prefix);
        let form = assets::f64s(&format!("curve.{prefix}.form"));
        if form.len() >= 3 && c.master.points.len() == 3 {
            let (y0, p0, gain) = (form[0] as f32, form[1] as f32, form[2] as f32);
            let mid = &mut c.master.points[1];
            mid.y = y0 + (param - p0) * gain;
        }
        c
    }

    /// 宽银幕预设。
    pub fn widescreen() -> Curves {
        curves_of("widescreen")
    }

    /// 交叉冲印预设。
    pub fn cross_develop() -> Curves {
        curves_of("cross_develop")
    }

    /// 复古预设。
    pub fn retro() -> Curves {
        curves_of("retro")
    }

    /// 反相预设(仅 Master 反相)。
    pub fn invert() -> Curves {
        curves_of("invert")
    }

    /// 柔焦预设(亮度滑块默认值 50)。
    pub fn soft_glow(brightness: f32) -> Curves {
        apply_form("soft_glow", brightness)
    }

    /// 铅笔素描预设(对比度滑块默认值 100)。
    pub fn pencil_sketch(contrast: f32) -> Curves {
        apply_form("pencil_sketch", contrast)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_curve_is_transparent() {
        let c = Curve::from_points(&[
            Point::new(0.0, 0.0),
            Point::new(128.0, 128.0),
            Point::new(255.0, 255.0),
        ]);
        for v in [0.0f32, 37.0, 128.0, 200.0, 255.0] {
            let got = c.eval(v);
            assert!((got - v).abs() < 1e-3, "v={v} got={got}");
        }
    }

    #[test]
    fn empty_curve_returns_input() {
        let c = Curve::new();
        assert_eq!(c.eval(42.0), 42.0);
    }

    #[test]
    fn spline_passes_through_control_points() {
        // 自然样条必须精确通过所有控制点。
        let c = Curve::from_points(&[
            Point::new(0.0, 0.0),
            Point::new(80.0, 30.0),
            Point::new(160.0, 200.0),
            Point::new(255.0, 255.0),
        ]);
        for p in &c.points {
            let got = c.eval(p.x);
            assert!(
                (got - p.y).abs() < 0.05,
                "x={} 期望 y={} 实得 {got}",
                p.x,
                p.y
            );
        }
    }

    #[test]
    fn spline_is_not_monotonic_preserving() {
        // 控制点本身非单调 → 自然样条在点之间也会回折(不保单调)。
        // 构造:输出先升后降,样条不应把它"修平"。
        let c = Curve::from_points(&[
            Point::new(0.0, 0.0),
            Point::new(100.0, 200.0),
            Point::new(200.0, 50.0),
            Point::new(255.0, 255.0),
        ]);
        // 在 100..200 之间应能找到下降段
        let mut descending = false;
        let mut prev = c.eval(100.0);
        for x in 101..200 {
            let v = c.eval(x as f32);
            if v < prev {
                descending = true;
            }
            prev = v;
        }
        assert!(descending, "自然样条不保单调,应出现下降段");
    }

    #[test]
    fn spline_extrapolates_beyond_endpoints() {
        // 原版越界**不钳制** → 端点段外插。
        let c = Curve::from_points(&[
            Point::new(0.0, 0.0),
            Point::new(100.0, 100.0),
            Point::new(255.0, 255.0),
        ]);
        let over = c.eval(300.0);
        assert!(over > 255.0, "越界应外插(>255),got {over}");
        let under = c.eval(-50.0);
        assert!(under < 0.0, "越界应外插(<0),got {under}");
    }

    #[test]
    fn invert_preset_inverts_codes() {
        let c = presets::invert();
        let lut = c.build_lut();
        assert_eq!(lut[0], [255, 255, 255]);
        assert_eq!(lut[255], [0, 0, 0]);
        assert_eq!(lut[128][0], 127, "round(127.0)=127");
    }

    #[test]
    fn lut_rounds_half_up_and_clamps() {
        // 恒等 Master + Red 反相:验证 round-half-up 与 clamp。
        let c = Curves {
            master: Curve::new(),
            red: Curve::from_points(&[Point::new(0.0, 255.0), Point::new(255.0, 0.0)]),
            green: Curve::new(),
            blue: Curve::new(),
            exposure: 0.0,
        };
        let lut = c.build_lut();
        for i in 0..=255usize {
            assert_eq!(lut[i][0], (255 - i) as u8, "i={i}");
        }
    }

    #[test]
    fn rgb_curves_share_same_master_input() {
        // 关键语义:三条曲线**各自以同一个 m 为输入**,而非各自以原码值为输入。
        // 构造:Master 抬升(0→50,255→255),Red 原样。
        // 则 R(0) 应 = 50(因为 m=50,Red 恒等 → 50),而不是 0。
        let c = Curves {
            master: Curve::from_points(&[Point::new(0.0, 50.0), Point::new(255.0, 255.0)]),
            red: Curve::from_points(&[Point::new(0.0, 0.0), Point::new(255.0, 255.0)]),
            green: Curve::from_points(&[Point::new(0.0, 0.0), Point::new(255.0, 255.0)]),
            blue: Curve::from_points(&[Point::new(0.0, 0.0), Point::new(255.0, 255.0)]),
            exposure: 0.0,
        };
        let lut = c.build_lut();
        assert_eq!(lut[0][0], 50, "R 应以 m=50 求值");
        assert_eq!(lut[0][1], 50);
        assert_eq!(lut[0][2], 50);
    }

    #[test]
    fn exposure_points_match_original_constants() {
        let pos = exposure_points(1.0);
        assert_eq!(pos.len(), 4);
        assert_eq!(pos[0], Point::new(0.0, 17.0));
        assert_eq!(pos[3], Point::new(221.0, 255.0));
        let neg = exposure_points(-1.0);
        assert_eq!(neg[0], Point::new(13.0, 0.0));
        assert_eq!(neg[3], Point::new(255.0, 221.0));
        assert!(exposure_points(0.0).is_empty());
    }

    #[test]
    fn exposure_brightens_shadows() {
        let c = Curves {
            exposure: 1.0,
            ..Default::default()
        };
        let lut = c.build_lut();
        // 正档曲线把暗部抬高
        assert!(lut[0][0] > 0, "曝光正档应抬亮暗部,got {}", lut[0][0]);
        assert!(lut[255][0] == 255);
    }

    #[test]
    fn apply_lut_preserves_alpha() {
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 128]));
        let lut = presets::invert().build_lut();
        apply_lut(&mut img, &lut);
        let p = img.get_pixel(0, 0);
        assert_eq!(p[0], 245, "10 反相 → 245");
        assert_eq!(p[3], 128, "alpha 不应被改");
    }

    #[test]
    fn apply_curves_identity_is_noop() {
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
        let before = img.clone();
        apply_curves(&mut img, &Curves::new());
        assert_eq!(img, before);
    }

    #[test]
    fn preset_data_is_stable() {
        // 抽查宽银幕预设的 Master 曲线首末点
        let c = presets::widescreen();
        assert_eq!(c.master.points[0], Point::new(0.0, 0.0));
        assert_eq!(c.master.points[1], Point::new(29.0, 19.0));
        assert_eq!(c.master.points[4], Point::new(255.0, 255.0));
        assert_eq!(c.red.points[1], Point::new(111.0, 141.0));
        assert_eq!(c.blue.points[1], Point::new(136.0, 121.0));
    }
}
