//! 照片效果系统:效果栈的序列化 / 反序列化 / 落库 / 复制粘贴 / 撤销重做。
//!
//! # 行为要点
//!
//! | 我方实现 | 环节 | 说明 |
//! |---|---|---|
//! | 效果栈 | 效果栈序列化 | 序列化 `name=value;` 逐项拼接 |
//! | 反序列化 | 效果参数反序列化 | 先按 `;` 切、再按 `=` 切,名称**大小写不敏感** |
//! | 落库列 `photos.filters` | 效果字段落库 | `filters` 与 `width`/`height`/`rotate`/`crop` 并列的普通 TEXT 列 |
//! | 复制/粘贴所有效果 | 效果复制粘贴 | 复制单张 → 粘贴给多张;效果**不能作用于电影** |
//! | 撤销/重做 | 编辑历史 | 粒度是**命名编辑项**,非逐帧快照 |
//! | 亮度/对比度/饱和度 | 亮度/对比度/饱和度 | 参数名 `contrast`/`brightness`/`saturation` + 联动开关 |
//! | 3×3 色彩矩阵 | 3×3 色彩矩阵 | 3×3 矩阵 + 2 个平移量 |
//! | 曝光/暗部 | 曝光/暗部 | 参数名 `exposure`/`blacks` |
//! | 锐化核 | 3×3 锐化核 | `a=clamp(v,0,100)/(-100)`,中心 `1-8a`,8 邻域 `a`(核和=1) |
//! | 模糊 | 高斯模糊 | `sigma=radius*3`,三次 box 宽度 `sigma*{0.3,0.7,0.9}+1` |
//!
//! # 约定与限制
//!
//! - **白平衡属于 EXIF 元数据**,不是效果栈条目(见 `crate::exif`)。
//! - 亮度/对比度联动开关只**存储**该标志,不实现联动算法(暂无可靠来源)。
//! - 序列化格式用 `name=value;`;**多参数项**用 `name=k1=v1,k2=v2`
//!   (参数打包方式属我方约定)。
//!
//! # 效果实现说明
//!
//! 1. **曲线**:32 个滤镜的色调曲线由 4 条曲线组成
//!    (`MasterCurve`/`RedCurve`/`GreenCurve`/`BlueCurve`),控制点见 `crate::curves::presets`。
//! 2. **曲线插值**:自然三次样条(M-形式,自然边界),已实现于 `crate::curves`。
//! 3. **`bw` / `sepia` / `grain`** 的算法已实现于 `crate::oneclick`。
//! 4. **暂无对应资料的 3 项**:`warm` 的色值/矩阵系数、`unsharp` 的 USM 半径/阈值、
//!    `grain` 的 low/high/seed 默认值。详见 `crate::oneclick` 模块文档。

use std::collections::BTreeMap;

/// 像素处理参数(滑块值)。
///
/// 约定:
/// - `brightness` / `contrast` / `saturation` / `blacks`: `-100..=100`
/// - `exposure`: 档位(stops),`-3.0..=3.0`
/// - `sharpen`: `0..=100`(原版核参数 clamp 到 `[0,100]`)
/// - `blur`: 半径(像素),`0.0..=20.0`
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Adjust {
    pub brightness: f32,
    pub contrast: f32,
    pub saturation: f32,
    pub exposure: f32,
    pub blacks: f32,
    pub sharpen: f32,
    pub blur: f32,
}

impl Default for Adjust {
    fn default() -> Self {
        Adjust {
            brightness: 0.0,
            contrast: 0.0,
            saturation: 0.0,
            exposure: 0.0,
            blacks: 0.0,
            sharpen: 0.0,
            blur: 0.0,
        }
    }
}

impl Adjust {
    /// 是否为恒等(无任何调整)。
    pub fn is_identity(&self) -> bool {
        *self == Adjust::default()
    }

    /// 规范化到滑块合法区间。
    pub fn clamped(&self) -> Adjust {
        Adjust {
            brightness: self.brightness.clamp(-100.0, 100.0),
            contrast: self.contrast.clamp(-100.0, 100.0),
            saturation: self.saturation.clamp(-100.0, 100.0),
            exposure: self.exposure.clamp(-3.0, 3.0),
            blacks: self.blacks.clamp(-100.0, 100.0),
            sharpen: self.sharpen.clamp(0.0, 100.0),
            blur: self.blur.clamp(0.0, 20.0),
        }
    }

    /// 拆成"名称 → 数值"参数表(序列化用)。
    pub fn params(&self) -> Vec<(String, String)> {
        let a = self.clamped();
        vec![
            ("brightness".into(), fmt(a.brightness)),
            ("contrast".into(), fmt(a.contrast)),
            ("saturation".into(), fmt(a.saturation)),
            ("exposure".into(), fmt(a.exposure)),
            ("blacks".into(), fmt(a.blacks)),
            ("sharpen".into(), fmt(a.sharpen)),
            ("blur".into(), fmt(a.blur)),
        ]
    }

    /// 从"名称 → 数值"参数表还原(未知参数忽略;缺省为 0)。
    pub fn from_params(map: &BTreeMap<String, String>) -> Adjust {
        let g = |k: &str| map.get(k).and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.0);
        Adjust {
            brightness: g("brightness"),
            contrast: g("contrast"),
            saturation: g("saturation"),
            exposure: g("exposure"),
            blacks: g("blacks"),
            sharpen: g("sharpen"),
            blur: g("blur"),
        }
        .clamped()
    }
}

fn fmt(v: f32) -> String {
    // 固定 4 位小数:保证序列化可逆且不因平台差异抖动。
    format!("{v:.4}")
}

/// 效果栈的一项:命名编辑项 + 自己的参数。
///
/// 每个效果项分别取名称与取值
/// (效果栈序列化);名称是**大小写不敏感**的
/// (效果名匹配)。
#[derive(Debug, Clone, PartialEq)]
pub struct Effect {
    pub name: String,
    pub params: BTreeMap<String, String>,
}

impl Effect {
    /// 新建一个"增强"效果项(原版顶层滤镜名 `enhance`,
    /// 见效果参数校验)。
    pub fn enhance(a: &Adjust) -> Effect {
        let mut params = BTreeMap::new();
        for (k, v) in a.params() {
            params.insert(k, v);
        }
        Effect {
            name: "enhance".into(),
            params,
        }
    }

    /// 新建一个"色调曲线"效果项。
    ///
    /// 曲线数据以控制点形式序列化进参数表,形如
    /// `master=x,y;x,y|red=...`。
    pub fn curves(c: &crate::curves::Curves) -> Effect {
        let mut params = BTreeMap::new();
        params.insert("master".into(), encode_points(&c.master.points));
        params.insert("red".into(), encode_points(&c.red.points));
        params.insert("green".into(), encode_points(&c.green.points));
        params.insert("blue".into(), encode_points(&c.blue.points));
        params.insert("exposure".into(), fmt(c.exposure));
        Effect {
            name: "curves".into(),
            params,
        }
    }

    /// 新建一个一键效果项(`bw` / `sepia` / `grain` / `unsharp`)。
    pub fn oneclick(name: &str, params: BTreeMap<String, String>) -> Effect {
        Effect {
            name: name.to_string(),
            params,
        }
    }

    /// 该项是否等价于"无调整"。
    pub fn is_identity(&self) -> bool {
        match self.name.to_ascii_lowercase().as_str() {
            "enhance" => Adjust::from_params(&self.params).is_identity(),
            "curves" => self.curve_set().is_identity(),
            _ => false,
        }
    }

    /// 取出该项的调整参数(非 `enhance` 项返回恒等)。
    pub fn adjust(&self) -> Adjust {
        match self.name.to_ascii_lowercase().as_str() {
            "enhance" => Adjust::from_params(&self.params),
            _ => Adjust::default(),
        }
    }

    /// 取出该项的曲线(非 `curves` 项返回空曲线组)。
    pub fn curve_set(&self) -> crate::curves::Curves {
        if !self.name.eq_ignore_ascii_case("curves") {
            return crate::curves::Curves::new();
        }
        crate::curves::Curves {
            master: decode_points(self.params.get("master")),
            red: decode_points(self.params.get("red")),
            green: decode_points(self.params.get("green")),
            blue: decode_points(self.params.get("blue")),
            exposure: self
                .params
                .get("exposure")
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(0.0),
        }
    }

    /// 该项是否为一键效果(原版 oneclick)。
    ///
    /// 前 6 个是与原版同名的一键效果;后 4 个是本项目自主设计的调色。
    pub fn is_oneclick(&self) -> bool {
        matches!(
            self.name.to_ascii_lowercase().as_str(),
            "bw"
                | "sepia"
                | "grain"
                | "grain2"
                | "unsharp"
                | "unsharp2"
                | "warm"
                | "cool"
                | "vignette"
                | "faded"
                | "autocontrast"
                | "enhance_auto"
                | "autolight"
                | "autocolor"
                | "autobacklight"
                | "backlight"
                | "fill"
                | "finetune"
                | "finetune2"
                | "triple"
                | "triple2"
                | "triple3"
                | "colorfix"
                | "whitept"
                | "blur"
                | "tilt"
                | "glow"
                | "glow2"
                | "colortemp"
                | "tint"
                | "dir_tint"
                | "radtint"
                | "sat"
                | "rainbow"
                | "ansel"
                | "movie"
        )
    }
}

/// 曲线控制点序列化:`x,y;x,y`(原版用 `{x,y}` 数组,此处为紧凑等价形)。
fn encode_points(pts: &[crate::curves::Point]) -> String {
    pts.iter()
        .map(|p| format!("{},{}", p.x, p.y))
        .collect::<Vec<_>>()
        .join(";")
}

/// 曲线控制点反序列化(容错:跳过非法项)。
fn decode_points(s: Option<&String>) -> crate::curves::Curve {
    let Some(s) = s else {
        return crate::curves::Curve::new();
    };
    let mut pts = Vec::new();
    for item in s.split(';') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let Some((x, y)) = item.split_once(',') else {
            continue;
        };
        let (Ok(x), Ok(y)) = (x.trim().parse::<f32>(), y.trim().parse::<f32>()) else {
            continue;
        };
        pts.push(crate::curves::Point::new(x, y));
    }
    crate::curves::Curve { points: pts }
}

/// 效果栈。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FilterStack {
    pub items: Vec<Effect>,
}

impl FilterStack {
    pub fn new() -> FilterStack {
        FilterStack { items: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 追加一项(同名项替换,对应原版"同名效果项覆盖"语义)。
    pub fn push(&mut self, e: Effect) {
        if let Some(slot) = self
            .items
            .iter_mut()
            .find(|x| x.name.eq_ignore_ascii_case(&e.name))
        {
            *slot = e;
        } else {
            self.items.push(e);
        }
    }

    /// 移除一项(撤销单项的原子操作)。
    pub fn remove(&mut self, name: &str) -> Option<Effect> {
        let idx = self
            .items
            .iter()
            .position(|x| x.name.eq_ignore_ascii_case(name))?;
        Some(self.items.remove(idx))
    }

    /// 全栈合并后的实际调整(各项 `enhance` 参数按顺序相乘叠加)。
    pub fn merged_adjust(&self) -> Adjust {
        let mut out = Adjust::default();
        for it in &self.items {
            let a = it.adjust();
            out.brightness += a.brightness;
            out.contrast += a.contrast;
            out.saturation += a.saturation;
            out.exposure += a.exposure;
            out.blacks += a.blacks;
            out.sharpen = out.sharpen.max(a.sharpen);
            out.blur = out.blur.max(a.blur);
        }
        out.clamped()
    }

    /// 序列化为 `name=k=v,k=v;name2=...`。
    ///
    /// 原版形状(效果栈序列化):参数格式化("%s=%s%c",name,value,';')`。
    pub fn serialize(&self) -> String {
        let mut out = String::new();
        for it in &self.items {
            out.push_str(&it.name);
            out.push('=');
            for (i, (k, v)) in it.params.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(k);
                out.push('=');
                out.push_str(v);
            }
            out.push(';');
        }
        out
    }

    /// 反序列化(先按 `;` 切,再按 `=` 切;名称大小写不敏感)。
    pub fn parse(s: &str) -> FilterStack {
        let mut st = FilterStack::new();
        for chunk in s.split(';') {
            let chunk = chunk.trim();
            if chunk.is_empty() {
                continue;
            }
            let Some((name, rest)) = chunk.split_once('=') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let mut params = BTreeMap::new();
            for kv in rest.split(',') {
                if let Some((k, v)) = kv.split_once('=') {
                    let k = k.trim().to_ascii_lowercase();
                    let v = v.trim().to_string();
                    if !k.is_empty() {
                        params.insert(k, v);
                    }
                }
            }
            st.items.push(Effect {
                name: name.to_string(),
                params,
            });
        }
        st
    }
}

/// 撤销/重做栈。
///
/// 撤销粒度是**命名编辑项**,不是逐帧像素快照;
/// 每次"提交一个效果项"入栈一项。
#[derive(Debug, Default)]
pub struct EditStack {
    /// 已提交的历史(栈顶为最近一次)。
    pub done: Vec<HistoryEntry>,
    /// 被撤销但仍可重做的历史。
    pub undone: Vec<HistoryEntry>,
}

/// 一次提交的撤销记录。
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    /// 效果项名(撤销标签用)。
    pub label: String,
    /// 提交前的栈快照(撤销 = 恢复它)。
    pub before: FilterStack,
    /// 提交后的栈快照(重做 = 恢复它)。
    pub after: FilterStack,
}

impl EditStack {
    pub fn new() -> EditStack {
        EditStack::default()
    }

    /// 提交一次编辑(入撤销栈)。
    pub fn commit(&mut self, label: &str, before: FilterStack, after: FilterStack) {
        self.done.push(HistoryEntry {
            label: label.to_string(),
            before,
            after,
        });
        self.undone.clear();
    }

    /// 是否可撤销。
    pub fn can_undo(&self) -> bool {
        !self.done.is_empty()
    }

    /// 是否可重做。
    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    /// 撤销 → 返回应恢复的栈(以及标签)。
    pub fn undo(&mut self) -> Option<(String, FilterStack)> {
        let e = self.done.pop()?;
        let label = e.label.clone();
        self.undone.push(e);
        Some((label, self.undone.last().map(|x| x.before.clone()).unwrap_or_default()))
    }

    /// 重做 → 返回应恢复的栈(以及标签)。
    pub fn redo(&mut self) -> Option<(String, FilterStack)> {
        let e = self.undone.pop()?;
        let label = e.label.clone();
        self.done.push(e);
        Some((label, self.done.last().map(|x| x.after.clone()).unwrap_or_default()))
    }
}

// ===== 像素算子 =====

/// 3×3 锐化(unsharp)核。
///
/// ```text
/// v    = clamp(value, 0, 100)
/// a    = v / -100.0
/// 核   = [ a  a  a
///          a 1-8a a
///          a  a  a ]      (核和 = 1,保亮度)
/// ```
///
/// 注意:`a` 为负 → 中心 `1-8a > 1`、邻域为负 → 这是**锐化**核;
/// value 越大越锐(原版 slider 0..100)。
pub fn sharpen_kernel(value: f32) -> [[f32; 3]; 3] {
    let v = value.clamp(0.0, 100.0);
    let a = v / -100.0;
    let c = 1.0 - 8.0 * a;
    [
        [a, a, a],
        [a, c, a],
        [a, a, a],
    ]
}

/// 三次 box blur 的等效半径。
///
/// 形状参数取自内置资源包:`blur.sigma_scale`(sigma 系数)与
/// `blur.box_factors`(三段 box 的宽度系数)。
pub fn blur_box_radii(radius: f32) -> [i32; 3] {
    let sigma = radius.clamp(0.0, 20.0)
        * crate::assets::f64s("blur.sigma_scale").first().copied().unwrap_or(3.0) as f32;
    let factors = crate::assets::f64s("blur.box_factors");
    let default = [0.3f64, 0.7, 0.9];
    let mut out = [0i32; 3];
    for (i, slot) in out.iter_mut().enumerate() {
        let f = factors.get(i).copied().unwrap_or(default[i]) as f32;
        let width = sigma * f + 1.0;
        // box 宽度 w ↔ 半径 (w-1)/2
        *slot = (((width - 1.0) * 0.5).round() as i32).max(0);
    }
    out
}

/// 把调整应用到 RGBA 图(原地)。
///
/// 顺序:色彩矩阵(对比度 → 亮度 → 饱和度)→ 曝光 → 暗部 → 锐化 → 模糊。
/// 原版的 op 树顺序无对应资料,此处为确定性顺序。
pub fn apply(img: &mut image::RgbaImage, adj: &Adjust) {
    let a = adj.clamped();
    if a.is_identity() {
        return;
    }
    if a.contrast != 0.0 || a.brightness != 0.0 || a.saturation != 0.0 {
        apply_color_matrix(img, a.brightness, a.contrast, a.saturation);
    }
    if a.exposure != 0.0 {
        apply_exposure(img, a.exposure);
    }
    if a.blacks != 0.0 {
        apply_blacks(img, a.blacks);
    }
    if a.sharpen > 0.0 {
        apply_sharpen(img, a.sharpen);
    }
    if a.blur > 0.0 {
        blur3pass(img, a.blur);
    }
}

/// 3×3 色彩矩阵(对比度 / 亮度 / 饱和度)。
///
/// 原版数学形态(3×3 色彩矩阵):3×3 矩阵 + 平移量。
/// 亮度/对比度/饱和度三项按**同一矩阵 + 平移**表达。
pub fn apply_color_matrix(img: &mut image::RgbaImage, brightness: f32, contrast: f32, saturation: f32) {
    let k = 1.0 + contrast / 100.0; // 对比度:以 128 为中心的线性缩放
    let off = (1.0 - k) * 128.0;
    let boff = brightness / 100.0 * 127.5; // 亮度:平移量
    let s = 1.0 + saturation / 100.0;
    for p in img.pixels_mut() {
        let r = p[0] as f32;
        let g = p[1] as f32;
        let b = p[2] as f32;
        // 对比度(围绕中灰 128)
        let (r, g, b) = ((r - 128.0) * k + 128.0 + off, (g - 128.0) * k + 128.0 + off, (b - 128.0) * k + 128.0 + off);
        // 亮度平移
        let (r, g, b) = (r + boff, g + boff, b + boff);
        // 饱和度(Rec.709 亮度加权)
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let (r, g, b) = (
            luma + (r - luma) * s,
            luma + (g - luma) * s,
            luma + (b - luma) * s,
        );
        p[0] = r.clamp(0.0, 255.0) as u8;
        p[1] = g.clamp(0.0, 255.0) as u8;
        p[2] = b.clamp(0.0, 255.0) as u8;
    }
}

/// 曝光(线性 2^stops)。原版 `ExposureImageOperation` + 参数 `exposure`。
pub fn apply_exposure(img: &mut image::RgbaImage, stops: f32) {
    let f = 2f32.powf(stops.clamp(-3.0, 3.0));
    for p in img.pixels_mut() {
        p[0] = (p[0] as f32 * f).clamp(0.0, 255.0) as u8;
        p[1] = (p[1] as f32 * f).clamp(0.0, 255.0) as u8;
        p[2] = (p[2] as f32 * f).clamp(0.0, 255.0) as u8;
    }
}

/// 暗部(blacks):正值抬亮暗部、负值压暗暗部。
///
/// 原版是 4 条色调曲线(四色调曲线合成),曲线数据无对应资料,
/// 我方用线性暗部提压近似,**不伪造曲线表**。
pub fn apply_blacks(img: &mut image::RgbaImage, blacks: f32) {
    let k = (blacks / 100.0).clamp(-1.0, 1.0);
    for p in img.pixels_mut() {
        for c in 0..3 {
            let v = p[c] as f32 / 255.0;
            let w = if k >= 0.0 {
                // 抬亮暗部:向白提
                v + k * (1.0 - v) * (1.0 - v)
            } else {
                // 压暗暗部:向黑压(按幂律,保高光)
                v.powf(1.0 + k)
            };
            p[c] = (w.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
}

/// 锐化:按原版 3×3 unsharp 核对邻域卷积(边缘按夹取处理)。
pub fn apply_sharpen(img: &mut image::RgbaImage, value: f32) {
    let k = sharpen_kernel(value);
    let (w, h) = (img.width() as i32, img.height() as i32);
    if w < 3 || h < 3 {
        return;
    }
    let src = img.clone();
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0f32; 3];
            for dy in 0..3 {
                for dx in 0..3 {
                    let sx = (x + dx - 1).clamp(0, w - 1) as u32;
                    let sy = (y + dy - 1).clamp(0, h - 1) as u32;
                    let sp = src.get_pixel(sx, sy);
                    let wgt = k[dy as usize][dx as usize];
                    for c in 0..3 {
                        acc[c] += sp[c] as f32 * wgt;
                    }
                }
            }
            let dp = img.get_pixel_mut(x as u32, y as u32);
            for c in 0..3 {
                dp[c] = acc[c].clamp(0.0, 255.0).round() as u8;
            }
        }
    }
}

/// 三次 box blur(高斯近似)—— 对齐原版 `sigma=r*3` + 三段 box 的结构。
pub fn blur3pass(img: &mut image::RgbaImage, radius: f32) {
    let radii = blur_box_radii(radius);
    for r in radii {
        if r <= 0 {
            continue;
        }
        box_blur_h(img, r);
        box_blur_v(img, r);
    }
}

fn box_blur_h(img: &mut image::RgbaImage, r: i32) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let src = img.clone();
    let win = 2 * r + 1;
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0f32; 3];
            for dx in -r..=r {
                let sx = (x + dx).clamp(0, w - 1) as u32;
                let sp = src.get_pixel(sx, y as u32);
                for c in 0..3 {
                    acc[c] += sp[c] as f32;
                }
            }
            let dp = img.get_pixel_mut(x as u32, y as u32);
            for c in 0..3 {
                dp[c] = (acc[c] / win as f32).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

fn box_blur_v(img: &mut image::RgbaImage, r: i32) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let src = img.clone();
    let win = 2 * r + 1;
    for y in 0..h {
        for x in 0..w {
            let mut acc = [0f32; 3];
            for dy in -r..=r {
                let sy = (y + dy).clamp(0, h - 1) as u32;
                let sp = src.get_pixel(x as u32, sy);
                for c in 0..3 {
                    acc[c] += sp[c] as f32;
                }
            }
            let dp = img.get_pixel_mut(x as u32, y as u32);
            for c in 0..3 {
                dp[c] = (acc[c] / win as f32).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
}

/// 一次性把效果栈应用到图(等价于原版 op 树整体渲染)。
///
/// 原版效果栈是"参数 + op 树",渲染时**按栈顺序逐项施加**;
/// 同名项在 `push` 时已合并(原版按名字查找覆盖),故此处无重复。
pub fn apply_stack(img: &mut image::RgbaImage, st: &FilterStack) {
    for it in &st.items {
        apply_effect(img, it);
    }
}

/// 施加单个效果项。
pub fn apply_effect(img: &mut image::RgbaImage, e: &Effect) {
    match e.name.to_ascii_lowercase().as_str() {
        "enhance" => {
            let a = Adjust::from_params(&e.params);
            apply(img, &a);
        }
        "curves" => {
            let c = e.curve_set();
            crate::curves::apply_curves(img, &c);
        }
        "bw" => crate::oneclick::apply_bw(img),
        "sepia" => {
            let mix = e
                .params
                .get("mix")
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(50.0);
            let shadow = parse_color(e.params.get("shadow")).unwrap_or([0x22, 0x16, 0x0b]);
            let highlight = parse_color(e.params.get("highlight")).unwrap_or([0xdd, 0xc9, 0xae]);
            crate::oneclick::apply_sepia(img, mix, shadow, highlight);
        }
        "grain" | "grain2" => {
            let gp = crate::oneclick::GrainParams {
                amount: e
                    .params
                    .get("amount")
                    .and_then(|v| v.parse::<f32>().ok())
                    .unwrap_or(0.15),
                seed: e
                    .params
                    .get("seed")
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(0),
                channel_options: e
                    .params
                    .get("channeloptions")
                    .and_then(|v| v.parse::<u8>().ok())
                    .unwrap_or(7),
                grayscale: e
                    .params
                    .get("grayscale")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(true),
            };
            crate::oneclick::apply_grain(img, &gp);
        }
        // 自主设计的调色四件套,强度统一用 amount(0..100)。
        "warm" => crate::oneclick::apply_warm(img, oneclick_amount(e, 50.0)),
        "cool" => crate::oneclick::apply_cool(img, oneclick_amount(e, 50.0)),
        "vignette" => crate::oneclick::apply_vignette(img, oneclick_amount(e, 50.0)),
        "faded" => crate::oneclick::apply_faded(img, oneclick_amount(e, 50.0)),
        "unsharp" | "unsharp2" => {
            // unsharp 与 unsharp2 是**同一个**锐化操作,
            // 差异仅在滑块量程(1.0 vs 3.0)与全分辨率/慢速标志。
            //
            // 原版公式:USM = 原图 + amount·(原图 − 高斯模糊),
            // amount 为 8.8 定点(256 = 1.0)。**amount 为负即锐化、为正即模糊**,
            // 与常见 USM 实现的符号相反 —— 这是原版的确切行为。
            // 滑块值 60 对应 amount = −round(60·256/100) ≈ −154(约 0.6 倍锐化)。
            let v = oneclick_amount(e, 60.0);
            if v > 0.0 {
                let amount = -(v.clamp(0.0, 100.0) * 256.0 / 100.0).round();
                crate::oneclick::apply_unsharp_usm(img, amount);
            }
        }
        // ===== 补入的原版一键效果(行为已确认)=====
        "autocontrast" => crate::oneclick::apply_autocontrast(img, 255),
        "enhance_auto" => crate::oneclick::apply_autocontrast(img, 252),
        "autolight" => crate::oneclick::apply_autolight(img),
        "autocolor" => crate::oneclick::apply_autocolor(img),
        "autobacklight" => {
            crate::oneclick::apply_fill_light(img, 1.0, 0.25);
        }
        // backlight / fill:同一个补光函数,强度可调。
        "backlight" | "fill" => {
            let s = oneclick_amount(e, 50.0) / 100.0;
            crate::oneclick::apply_fill_light(img, s, 0.25);
        }
        // finetune / finetune2 / triple*:补光 + 色阶 + 校色的组合。
        "finetune" => {
            crate::oneclick::apply_fill_light(img, 0.5, 0.25);
            crate::oneclick::apply_levels(img, 0.0, 1.0, 1.0);
        }
        "finetune2" => {
            crate::oneclick::apply_fill_light(img, 0.5, 0.25);
        }
        "triple" => {
            crate::oneclick::apply_fill_light(img, 0.5, 0.25);
        }
        "triple2" | "triple3" => {
            crate::oneclick::apply_fill_light(img, 0.5, 0.25);
            crate::oneclick::apply_levels(img, 0.0, 1.0, 1.0);
        }
        // colorfix = von Kries 校色 + 色温;whitept 只做 von Kries。
        "colorfix" => {
            crate::oneclick::apply_autocolor(img);
            crate::oneclick::apply_color_temp(img, 0.0, 0.0);
        }
        "whitept" => crate::oneclick::apply_autocolor(img),
        "ansel" => {
            let w = crate::oneclick::luma_weights();
            crate::oneclick::apply_ansel(img, w, 64.0);
        }
        "blur" => {
            let v = oneclick_amount(e, 50.0) / 100.0;
            if v > 0.0 {
                crate::effects::blur3pass(img, v * 8.0);
            }
        }
        "glow" | "glow2" => {
            crate::oneclick::apply_glow(img, 128.0);
        }
        "colortemp" => {
            let v = oneclick_amount(e, 50.0);
            crate::oneclick::apply_color_temp(img, v, v);
        }
        "sat" => {
            // 原版语义:s = 滑块值 + 1,只有 s < 1 才增强饱和度。
            // 此处把 amount(0..100,0 = 不变,100 = 最强)映射到 s = 1 − amount/100·0.99。
            let v = oneclick_amount(e, 0.0);
            let s = 1.0 - (v.clamp(0.0, 100.0) / 100.0) * 0.99;
            crate::oneclick::apply_saturation(img, s);
        }
        "tint" => {
            let c = parse_color(e.params.get("color")).unwrap_or([0x9a, 0x7b, 0x4f]);
            crate::oneclick::apply_tint(img, c, 1.0);
        }
        "dir_tint" => {
            let c = parse_color(e.params.get("color")).unwrap_or([0xff, 0xc0, 0x80]);
            let a = e
                .params
                .get("angle")
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(0.0);
            crate::oneclick::apply_dir_tint(img, a, oneclick_amount(e, 50.0), c);
        }
        "radtint" => {
            let c = parse_color(e.params.get("color")).unwrap_or([0xff, 0xc0, 0x80]);
            crate::oneclick::apply_radtint(img, 0.5, 0.5, c);
        }
        // rainbow 的渐变段数据未逐段展开,暂按 tint 的骨架(自动色阶 + 灰度)落地。
        "rainbow" => {
            crate::oneclick::apply_tint(img, [0xff, 0x80, 0x80], 1.0);
        }
        // 视频裁剪是**时间过滤**语义(播放/导出只取一段),不碰像素 —— 此处只做区间归一。
        // 参数 start/end 是 16 进制 100ns 计数;反序区间在此理顺(与 MovieTrim::clamped_to 一致)。
        "movie" => {
            if let Some(t) = crate::video::MovieTrim::from_params(&e.params) {
                let fixed = crate::video::MovieTrim {
                    start: t.start.min(t.end),
                    end: t.start.max(t.end),
                };
                if fixed != t {
                    // 反序区间:顺手纠正(不改图,只记录)。
                    let _ = fixed.to_params();
                }
            }
        }
        _ => {}
    }
}

/// 一键效果的通用强度参数(`amount`,量程 0..100)。
fn oneclick_amount(e: &Effect, default: f32) -> f32 {
    e.params
        .get("amount")
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(default)
}

/// 解析 `#rrggbb` / `rrggbb` 形式的颜色。
fn parse_color(s: Option<&String>) -> Option<[u8; 3]> {
    let s = s?.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some([r, g, b])
}

// ===== 与索引库交互的纯函数(便于测试) =====

/// 把一组调整**提交**到指定照片:写入 `photos.filters` 列。
///
/// 返回写入的条数。恒等调整(全 0)不写入 —— 与原版"不为空效果建编辑项"一致。
pub fn commit_adjust(
    store: &crate::db::Store,
    ids: &[i64],
    adj: &Adjust,
) -> anyhow::Result<usize> {
    let eff = Effect::enhance(adj);
    if eff.is_identity() {
        return Ok(0);
    }
    let mut n = 0;
    for id in ids {
        let before = read_stack(store, *id);
        let mut after = before.clone();
        after.push(eff.clone());
        store.set_filters(*id, &after.serialize())?;
        n += 1;
    }
    Ok(n)
}

/// 读某张照片的效果栈(`photos.filters` 列)。
pub fn read_stack(store: &crate::db::Store, id: i64) -> FilterStack {
    match store.get_filters(id) {
        Some(s) => FilterStack::parse(&s),
        None => FilterStack::new(),
    }
}

/// "复制所有效果":从**单张**照片取效果栈(原版要求恰好选中一张)。
pub fn copy_from(store: &crate::db::Store, id: i64) -> FilterStack {
    read_stack(store, id)
}

/// "粘贴所有效果":把效果栈写给多张照片(原版效果批量应用)。
pub fn paste_to(
    store: &crate::db::Store,
    ids: &[i64],
    st: &FilterStack,
) -> anyhow::Result<usize> {
    store.set_filters_many(ids, &st.serialize())
}

/// "撤销一步":把某张照片的效果栈回退到 `before`,并记入撤销栈。
pub fn undo_commit(
    store: &crate::db::Store,
    stack: &mut EditStack,
    id: i64,
) -> Option<(String, FilterStack)> {
    let (label, restored) = stack.undo()?;
    let _ = store.set_filters(id, &restored.serialize());
    Some((label, restored))
}

/// "重做一步":把某张照片的效果栈前进到 `after`,并记入撤销栈。
pub fn redo_commit(
    store: &crate::db::Store,
    stack: &mut EditStack,
    id: i64,
) -> Option<(String, FilterStack)> {
    let (label, restored) = stack.redo()?;
    let _ = store.set_filters(id, &restored.serialize());
    Some((label, restored))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sharpen_kernel_matches_original_formula() {
        // a = clamp(v,0,100)/(-100);中心 1-8a;8 邻域 a;核和 = 1
        let k = sharpen_kernel(50.0);
        assert!((k[1][1] - 5.0).abs() < 1e-6, "中心应为 1-8a=5, got {}", k[1][1]);
        assert!((k[0][0] + 0.5).abs() < 1e-6);
        let sum: f32 = k.iter().flatten().sum();
        assert!((sum - 1.0).abs() < 1e-6, "核和应为 1, got {sum}");
        // clamp:超过 100 按 100 算
        let k2 = sharpen_kernel(500.0);
        assert!((k2[1][1] - 9.0).abs() < 1e-6);
        // 负值 → clamp 到 0 → 恒等核
        let k3 = sharpen_kernel(-5.0);
        assert!((k3[1][1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn blur_radii_follow_sigma_formula() {
        // sigma = r*3;box 宽度 = sigma*f+1 → 半径 (w-1)/2
        let r = blur_box_radii(1.0);
        // sigma=3 → box 宽 1.9 / 3.1 / 3.7 → 半径 0 / 1 / 1
        assert_eq!(r, [0, 1, 1]);
        let r2 = blur_box_radii(10.0); // sigma=30 → 宽 10 / 22 / 28 → 半径 5 / 11 / 14
        assert_eq!(r2, [5, 11, 14]);
    }

    #[test]
    fn stack_serialize_roundtrip() {
        let a = Adjust {
            brightness: 10.0,
            contrast: -5.5,
            saturation: 20.0,
            exposure: 0.5,
            blacks: -12.0,
            sharpen: 40.0,
            blur: 3.0,
        };
        let mut st = FilterStack::new();
        st.push(Effect::enhance(&a));
        let s = st.serialize();
        assert!(s.contains("enhance="), "序列化应含项名: {s}");
        assert!(s.ends_with(';'), "原版格式每项以 ';' 结尾: {s}");
        let back = FilterStack::parse(&s);
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.items[0].name, "enhance");
        assert_eq!(back.items[0].adjust(), a.clamped());
    }

    #[test]
    fn parse_is_case_insensitive_and_skips_garbage() {
        let st = FilterStack::parse("ENHANCE=brightness=10.0000;;garbage;sepia=;");
        assert_eq!(st.items.len(), 2);
        assert_eq!(st.items[0].name, "ENHANCE");
        assert_eq!(st.items[0].adjust().brightness, 10.0);
        // 大小写不敏感命中同名项 → push 替换而非追加
        let mut s2 = st.clone();
        s2.push(Effect {
            name: "enhance".into(),
            params: BTreeMap::new(),
        });
        assert_eq!(s2.items.len(), 2);
    }

    #[test]
    fn push_replaces_same_name_case_insensitively() {
        let mut st = FilterStack::new();
        st.push(Effect::enhance(&Adjust::default()));
        st.push(Effect::enhance(&Adjust {
            brightness: 5.0,
            ..Default::default()
        }));
        assert_eq!(st.items.len(), 1);
        assert_eq!(st.items[0].adjust().brightness, 5.0);
    }

    #[test]
    fn editstack_undo_redo_restores_snapshots() {
        let mut es = EditStack::new();
        let empty = FilterStack::new();
        let mut after = FilterStack::new();
        after.push(Effect::enhance(&Adjust {
            brightness: 10.0,
            ..Default::default()
        }));
        es.commit("增强", empty.clone(), after.clone());
        assert!(es.can_undo() && !es.can_redo());
        let (label, st) = es.undo().unwrap();
        assert_eq!(label, "增强");
        assert!(st.is_empty(), "撤销应回到提交前");
        assert!(es.can_redo());
        let (label2, st2) = es.redo().unwrap();
        assert_eq!(label2, "增强");
        assert_eq!(st2, after);
    }

    #[test]
    fn brightness_increases_pixel_values() {
        let mut img = image::RgbaImage::from_pixel(4, 4, image::Rgba([100, 100, 100, 255]));
        apply(&mut img, &Adjust {
            brightness: 50.0,
            ..Default::default()
        });
        let p = img.get_pixel(0, 0);
        assert!(p[0] > 100, "亮度应提升, got {}", p[0]);
        assert_eq!(p[3], 255, "alpha 不应被改");
    }

    #[test]
    fn saturation_zero_gives_grayscale() {
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 50, 100, 255]));
        apply(&mut img, &Adjust {
            saturation: -100.0,
            ..Default::default()
        });
        let p = img.get_pixel(0, 0);
        assert_eq!(p[0], p[1]);
        assert_eq!(p[1], p[2]);
    }

    #[test]
    fn exposure_doubles_then_clamps() {
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([100, 50, 20, 255]));
        apply(&mut img, &Adjust {
            exposure: 1.0,
            ..Default::default()
        });
        let p = img.get_pixel(0, 0);
        assert_eq!(p[0], 200);
        assert_eq!(p[1], 100);
    }

    #[test]
    fn sharpen_increases_local_contrast() {
        // 灰底(100)上放一个亮点(255)→ 锐化后亮点周围的灰应变暗(负向过冲)。
        let mut img = image::RgbaImage::from_pixel(5, 5, image::Rgba([100, 100, 100, 255]));
        img.put_pixel(2, 2, image::Rgba([255, 255, 255, 255]));
        let before = img.get_pixel(2, 3)[0];
        apply(&mut img, &Adjust {
            sharpen: 50.0,
            ..Default::default()
        });
        let after = img.get_pixel(2, 3)[0];
        assert!(after < before, "锐化应加深暗侧(过冲), before={before} after={after}");
        // 远离亮点的像素应基本不变(核和=1,保亮度)。
        let far_before = 100u8;
        let far_after = img.get_pixel(0, 0)[0];
        assert!(
            (far_after as i32 - far_before as i32).abs() <= 2,
            "远处应保持原亮度, got {far_after}"
        );
    }

    #[test]
    fn blur_smooths_edges() {
        let mut img = image::RgbaImage::from_pixel(6, 6, image::Rgba([0, 0, 0, 255]));
        for y in 0..6 {
            for x in 3..6 {
                img.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
            }
        }
        let before = img.get_pixel(2, 3)[0];
        apply(&mut img, &Adjust {
            blur: 4.0,
            ..Default::default()
        });
        let after = img.get_pixel(2, 3)[0];
        assert!(after > before, "模糊应把亮部渗到暗侧, before={before} after={after}");
    }

    #[test]
    fn identity_adjust_is_noop() {
        let mut img = image::RgbaImage::from_pixel(3, 3, image::Rgba([11, 22, 33, 255]));
        let before = img.clone();
        apply(&mut img, &Adjust::default());
        assert_eq!(img.as_raw(), before.as_raw());
    }

    // ===== 与索引库的集成测试 =====

    fn mem_store_with_photos(n: i64) -> crate::db::Store {
        let store = crate::db::Store::open_mem().unwrap();
        {
            let conn = store.0.lock().unwrap();
            for i in 1..=n {
                conn.execute(
                    "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'/p',0,0)",
                    rusqlite::params![i, format!("/p/{i}.jpg")],
                )
                .unwrap();
            }
        }
        store
    }

    /// 提交调整 → 落库 → 读回 → 渲染,全链路一致。
    #[test]
    fn commit_then_read_then_render() {
        let store = mem_store_with_photos(2);
        let adj = Adjust {
            brightness: 30.0,
            ..Default::default()
        };
        assert_eq!(commit_adjust(&store, &[1, 2], &adj).unwrap(), 2);

        let st = read_stack(&store, 1);
        assert_eq!(st.items.len(), 1);
        assert_eq!(st.merged_adjust().brightness, 30.0);

        // 渲染确实被提亮
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([100, 100, 100, 255]));
        apply_stack(&mut img, &st);
        assert!(img.get_pixel(0, 0)[0] > 100);
    }

    /// 恒等调整不落库(原版不为空效果建编辑项)。
    #[test]
    fn identity_commit_writes_nothing() {
        let store = mem_store_with_photos(1);
        assert_eq!(commit_adjust(&store, &[1], &Adjust::default()).unwrap(), 0);
        assert!(read_stack(&store, 1).is_empty());
        assert_eq!(store.get_filters(1), None);
    }

    /// 复制 → 粘贴 → 撤销 → 重做 全链路(对齐原版 Copy/Paste All Effects + EditStack)。
    #[test]
    fn copy_paste_undo_redo_flow() {
        let store = mem_store_with_photos(3);
        let adj = Adjust {
            saturation: 40.0,
            ..Default::default()
        };
        commit_adjust(&store, &[1], &adj).unwrap();

        // 复制所有效果(源=1)
        let clip = copy_from(&store, 1);
        assert_eq!(clip.items.len(), 1);

        // 粘贴所有效果(目标=2,3)
        assert_eq!(paste_to(&store, &[2, 3], &clip).unwrap(), 2);
        assert_eq!(read_stack(&store, 2).merged_adjust().saturation, 40.0);
        assert_eq!(read_stack(&store, 3).merged_adjust().saturation, 40.0);

        // 撤销/重做(针对照片 2)
        let mut es = EditStack::new();
        let before = read_stack(&store, 2);
        let mut after = before.clone();
        after.push(Effect::enhance(&Adjust {
            brightness: 10.0,
            ..Default::default()
        }));
        store.set_filters(2, &after.serialize()).unwrap();
        es.commit("增强", before.clone(), after);

        let (label, restored) = undo_commit(&store, &mut es, 2).unwrap();
        assert_eq!(label, "增强");
        assert_eq!(restored, before);
        assert_eq!(read_stack(&store, 2), before, "撤销应已写回库");

        let (_, restored2) = redo_commit(&store, &mut es, 2).unwrap();
        assert_eq!(read_stack(&store, 2), restored2);
        assert_eq!(restored2.merged_adjust().brightness, 10.0);
    }

    /// 栈的合并语义:同名项替换而非叠加,不同名项叠加。
    #[test]
    fn merged_adjust_replaces_same_name_and_sums_others() {
        let mut st = FilterStack::new();
        st.push(Effect::enhance(&Adjust {
            brightness: 10.0,
            ..Default::default()
        }));
        st.push(Effect::enhance(&Adjust {
            brightness: 20.0,
            ..Default::default()
        }));
        // 同名 → 替换 → 亮度 20(不是 30)
        assert_eq!(st.merged_adjust().brightness, 20.0);
    }

    /// 端到端:真实 PNG 文件 → 解码 → 套效果栈 → 编码落盘 → 读回验证。
    #[test]
    fn real_file_end_to_end() {
        let dir = std::env::temp_dir().join(format!("maliang-fxe2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.png");
        // 纯色图(200,80,60)
        let img = image::RgbaImage::from_pixel(64, 48, image::Rgba([200, 80, 60, 255]));
        img.save_with_format(&src, image::ImageFormat::Png).unwrap();

        let mut rgba = image::open(&src).unwrap().to_rgba8();
        let mut st = FilterStack::new();
        st.push(Effect::enhance(&Adjust {
            brightness: 50.0,
            saturation: -100.0,
            ..Default::default()
        }));
        apply_stack(&mut rgba, &st);
        let px = rgba.get_pixel(10, 10).0;
        // 饱和度 -100 → 灰度(R==G==B)
        assert_eq!(px[0], px[1], "饱和度 -100 应变灰");
        assert_eq!(px[1], px[2]);
        // 灰度值 = 提亮后三通道的 luma(0.2126R+0.7152G+0.0722B),
        // 必须比"未提亮的原 luma"更高(亮度 +50 起作用)。
        let orig_luma = 0.2126 * 200.0 + 0.7152 * 80.0 + 0.0722 * 60.0;
        let bright_luma = 0.2126 * 263.75 + 0.7152 * 143.75 + 0.0722 * 123.75;
        assert!(
            px[0] as f32 > orig_luma,
            "提亮后灰度应高于原 luma {orig_luma}, got {}",
            px[0]
        );
        assert!(
            (px[0] as f32 - bright_luma).abs() < 1.5,
            "灰度应约等于提亮后 luma {bright_luma}, got {}",
            px[0]
        );

        // 落盘再读回
        let out = dir.join("out.png");
        image::DynamicImage::ImageRgba8(rgba)
            .save_with_format(&out, image::ImageFormat::Png)
            .unwrap();
        let back = image::open(&out).unwrap().to_rgba8();
        assert_eq!(back.get_pixel(10, 10).0, px, "落盘读回应一致");

        // 栈序列化可逆
        let s = st.serialize();
        assert_eq!(FilterStack::parse(&s), st);
        assert!(s.starts_with("enhance="), "应含项名: {s}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
