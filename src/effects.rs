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

/// 已知的一键效果名 **`apply_effect` 中有分派分支**的那一批。
///
/// ⚠️ 每新增一个分派分支都必须同步这里 —— 测试
/// `every_known_effect_touches_pixels` 会在两者漂移时失败,用来防止再度出现
/// 「名字能被识别、但没有落地实现」的静默空操作(历史上 `tilt` /
/// `shadow` 就是这么漏掉的)。
pub const PIXEL_EFFECTS: &[&str] = &[
    "bw",
    "sepia",
    "grain",
    "grain2",
    "unsharp",
    "unsharp2",
    "warm",
    "cool",
    "vignette",
    "faded",
    "autocontrast",
    "enhance_auto",
    "autolight",
    "autocolor",
    "autobacklight",
    "backlight",
    "fill",
    "finetune",
    "finetune2",
    "triple",
    "triple2",
    "triple3",
    "colorfix",
    "whitept",
    "blur",
    "tilt",
    "glow",
    "glow2",
    "colortemp",
    "tint",
    "dir_tint",
    "radtint",
    "sat",
    "rainbow",
    "ansel",
    "movie",
    "gamma",
    "invert",
    "linblur",
    "radblur",
    "radsat",
    "dir_brite",
    "dir_sat",
    "dir_sharp",
    "shadow",
];

/// `tilt`(Straighten)滑杆满量程对应的角度。
///
/// 参考只公开了这根滑杆的量程(range 2.0 / offset 1.0),到角度的换算系数
/// 暂无对应资料 —— 这里取 ±15° 作为"够用且不会一下子转过头"的取值。
const TILT_MAX_DEG: f32 = 15.0;

/// 登记在案、**刻意不改像素**的效果名。
///
/// 这些名字有自己的语义(例如 `movie` 只做时间区间裁剪),在渲染管线里
/// 落到别处生效,故不算 bug;清单之外的名字若不动像素,测试会报错。
#[cfg(test)]
const DOCUMENTED_NOOPS: &[&str] = &[
    // 视频裁剪是**时间过滤**语义,参数交给 `crate::video::MovieTrim` 处理。
    "movie",
];

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
/// 除逐项列出的名字外,`crate::recipes` 里的成组配方同样按一键效果对待。
pub fn is_oneclick(&self) -> bool {
    if crate::recipes::is_recipe(&self.name) {
        return true;
    }
    let name = self.name.to_ascii_lowercase();
    PIXEL_EFFECTS.iter().any(|k| *k == name)
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
/// 原版数学形态(3×3 色彩矩阵):3×3 矩阵 + 平移量,见参考的
/// `SimpleColorMatrix`(矩阵右乘序列 `M × Sat × ContrastBright × Hue`)。
/// 亮度/对比度/饱和度三项按**同一矩阵 + 平移**表达,顺序与参考一致。
///
/// - **对比度**(参考合并式 `out = g·in + 127.5·(1−g)`,等价于以
///   **中灰 127.5 为轴**的线性缩放):`k = 1 + contrast/100`,`out = k·(v − 127.5) + 127.5`。
///   由此中灰**保持不动**,`contrast = −100` 时整图折叠到中灰。
///   ⚠️ 早期版本在这里多计了一次 `+(1−k)·128` 平移,导致 +100 把中灰压成纯黑、
///   −100 把整图推成纯白;现有回归锁见 `contrast_keeps_midgray_and_folds_to_midgray`。
/// - **饱和度**:参考是**非对称**的 `s' = 1 + (sat > 0 ? 3·sat : sat) / 100`
///   (正方向 3 倍增益),亮度权重取全库统一的 `(0.3086, 0.6094, 0.082)` ——
///   **不是** Rec.709 的 `(0.2126, 0.7152, 0.0722)`,否则与 `bw` / `tint` 口径不一致。
/// - **亮度**:平移量 `brightness/100 × 127.5`(维持既有 UI 量程,未改)。
///
/// **诚实声明**:参考对比度走的是一张 101 项非线性增益表 `T(x)`(`g = 1 + T(x)`,
/// `T(0)=0`、`T(50)=1.0`、`T(100)=10.0`),表值暂无对应资料 —— 此处保留**线性** `k`。
/// 因此正对比度滑块的手感比参考更温和,这是已知的简化,不是漏实现。
pub fn apply_color_matrix(img: &mut image::RgbaImage, brightness: f32, contrast: f32, saturation: f32) {
    let k = 1.0 + contrast / 100.0; // 对比度:以中灰 127.5 为轴的线性缩放
    let boff = brightness / 100.0 * 127.5; // 亮度:平移量
    // 饱和度:非对称增益 + 全库统一亮度权重
    let s = 1.0 + (if saturation > 0.0 { 3.0 * saturation } else { saturation }) / 100.0;
    let [wr, wg, wb] = crate::oneclick::luma_weights();
    for p in img.pixels_mut() {
        let r = p[0] as f32;
        let g = p[1] as f32;
        let b = p[2] as f32;
        // 对比度(围绕中灰 127.5,不再叠加重复平移)
        let (r, g, b) = ((r - 127.5) * k + 127.5, (g - 127.5) * k + 127.5, (b - 127.5) * k + 127.5);
        // 亮度平移
        let (r, g, b) = (r + boff, g + boff, b + boff);
        // 饱和度(全库统一亮度加权)
        let luma = wr * r + wg * g + wb * b;
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

/// 曝光:走**曝光曲线**(样条)路径,与 `curves` 里的曝光点表共用一套实现。
///
/// 原版曝光是**色调曲线级的三条 DSP 整形**,不是线性乘法:先把 `stops` 拆成
/// 整数档 `N` 与小数权重 `w`,把曝光点表写进 Master 槽后按自然三次样条求值,
/// 迭代 `N+1` 次并在末两步按 `w` 线性混合。点表取自内置资源包:
/// `exposure.pos` / `exposure.neg`。
///
/// ⚠️ 过去这里另有一份线性 `2^stops` 实现,导致库内有两套曝光口径、
/// `curves::exposure_points` 在 Adjust 路径上不可达 —— 现已合并成本函数。
pub fn apply_exposure(img: &mut image::RgbaImage, stops: f32) {
    let c = crate::curves::Curves {
        exposure: stops.clamp(-3.0, 3.0),
        ..Default::default()
    };
    crate::curves::apply_curves(img, &c);
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
///
/// **口径说明**:这只服务调整面板的 `Adjust.blur` 滑杆(参数语义是
/// 「sigma 系数 + 三段宽度系数」,来自我方资源包);效果算子 `blur` 已改走
/// 参考的阶梯量化 + 盒式模糊路径(见 `apply_effect` 与 `recipes::blur_ladder`),
/// 两条路径不要再互相污染。
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

/// 读取组合效果的一个滑杆值:参数是**滑杆原值**(0..1),
/// 按参考的 `<range>` / `<offset>` 折算成物理量 `raw·range − offset`。
///
/// 参考里带 `<offset>` 的滑杆,`offset` 恰好是 `range` 的一半(如 range 2.0 /
/// offset 1.0),说明带偏移的滑杆是**双向**的、原值 0.5 即中位。
fn combo_val(e: &Effect, key: &str, raw_default: f32, range: f32, offset: f32) -> f32 {
    let raw = e
        .params
        .get(key)
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(raw_default);
    raw * range - offset
}

/// 高光 / 阴影响应值 → 色阶的白点 / 黑点。
///
/// **诚实声明**:参考只公开了这两根滑杆的存在与量程(0.48),到黑白点的
/// 折算系数暂无对应资料。此处按标准色阶语义线性折算
/// (高光下调白点、阴影上抬黑点),方向确定、系数属我方重构。
fn highlights_shadows(e: &Effect) -> (f32, f32) {
    let h = combo_val(e, "highlights", 0.0, 0.48, 0.0);
    let s = combo_val(e, "shadows", 0.0, 0.48, 0.0);
    ((1.0 - h).clamp(0.0, 1.0), s.clamp(0.0, 1.0))
}

/// `finetune` / `finetune2`:补光 → 高光 / 阴影 → 色温。
///
/// 两者的差别只在**色温轴的宽度**:参考 finetune 是 range 1.0 / offset 0.5
/// (−0.5..0.5),finetune2 是 range 2.0 / offset 1.0(−1..1)。
fn tuning_combo(img: &mut image::RgbaImage, e: &Effect, wide_axis: bool) {
    let fl = combo_val(e, "fill", 0.0, 1.0, 0.0);
    if fl > 0.0 {
        crate::oneclick::apply_fill_light(img, fl, 0.25);
    }
    let (white, black) = highlights_shadows(e);
    if white < 1.0 || black > 0.0 {
        crate::oneclick::apply_levels(img, black, white, 1.0);
    }
    let temp = if wide_axis {
        combo_val(e, "temp", 0.5, 2.0, 1.0)
    } else {
        combo_val(e, "temp", 0.5, 1.0, 0.5)
    };
    if temp != 0.0 {
        crate::oneclick::apply_color_temp(img, temp, 0.0);
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
        // ===== 组合效果:滑杆清单取自参考产品自带的滤镜描述,各接自己的滑杆 =====
        //
        // | id | 参考滑杆(依次为 id=0/1/2/3) | 我方接线 |
        // |---|---|---|
        // | `triple` | 亮度(range 2.0 / offset 1.0)、对比度(1.0 / 0.5)、补光 | 色彩矩阵(亮度+对比度)→ 补光 |
        // | `triple2` | 补光、黑点、白点(默认 1.0) | 补光 → 色阶(黑白点) |
        // | `triple3` | 补光、高光(0.48)、阴影(0.48) | 补光 → 色阶(高光/阴影折算) |
        // | `finetune` | 补光、高光、阴影、色温(1.0 / 0.5) | 补光 → 高光/阴影 → 色温 |
        // | `finetune2` | 同上,色温轴宽一倍(2.0 / 1.0) | 同上(色温折算不同) |
        // | `colorfix` | 白场吸管 + 色温(1.0 / 0.5) | von Kries 校色 → 色温 |
        //
        // **诚实声明**:参考只公开了「有哪些滑杆」与其量程/偏移,
        // 高光 / 阴影 两根滑杆到色阶的折算系数暂无对应资料 ——
        // 这里按「负=压、正=提」的方向线性折算到白点 / 黑点,属我方重构。
        "finetune" | "finetune2" => {
            tuning_combo(img, e, e.name.eq_ignore_ascii_case("finetune2"));
        }
        "triple" => {
            let b = combo_val(e, "brightness", 0.5, 2.0, 1.0) * 100.0;
            let c = combo_val(e, "contrast", 0.5, 1.0, 0.5) * 100.0;
            if b != 0.0 || c != 0.0 {
                apply_color_matrix(img, b, c, 0.0);
            }
            let fl = combo_val(e, "fill", 0.0, 1.0, 0.0);
            if fl > 0.0 {
                crate::oneclick::apply_fill_light(img, fl, 0.25);
            }
        }
        "triple2" => {
            let fl = combo_val(e, "fill", 0.0, 1.0, 0.0);
            if fl > 0.0 {
                crate::oneclick::apply_fill_light(img, fl, 0.25);
            }
            // 黑点 / 白点:白点的参考默认值就是 1.0(见 XML 的 `<default>`)
            let black = combo_val(e, "black", 0.0, 1.0, 0.0);
            let white = combo_val(e, "white", 1.0, 1.0, 0.0);
            if black > 0.0 || white < 1.0 {
                crate::oneclick::apply_levels(img, black, white, 1.0);
            }
        }
        "triple3" => {
            let fl = combo_val(e, "fill", 0.0, 1.0, 0.0);
            if fl > 0.0 {
                crate::oneclick::apply_fill_light(img, fl, 0.25);
            }
            let (white, black) = highlights_shadows(e);
            if white < 1.0 || black > 0.0 {
                crate::oneclick::apply_levels(img, black, white, 1.0);
            }
        }
        // colorfix = von Kries 校色 + 色温;whitept 只做 von Kries。
        "colorfix" => {
            crate::oneclick::apply_autocolor(img);
            let temp = combo_val(e, "temp", 0.5, 1.0, 0.5);
            if temp != 0.0 {
                crate::oneclick::apply_color_temp(img, temp, 0.0);
            }
        }
        // 倾斜校正(Straighten):参考是一根双向滑杆(range 2.0 / offset 1.0),
        // 即 raw 0..1 → −1..1,中位不旋转。
        //
        // **诚实声明**:滑杆到角度的换算系数暂无对应资料,此处按 ±15° 满量程
        // 线性折算;调用方也可以用 `angle` 参数直接给度数来绕过折算。
        "tilt" => {
            let deg = match e.params.get("angle").and_then(|v| v.parse::<f32>().ok()) {
                Some(d) => d,
                None => {
                    (oneclick_amount(e, 50.0).clamp(0.0, 100.0) / 100.0 * 2.0 - 1.0)
                        * TILT_MAX_DEG
                }
            };
            if deg != 0.0 {
                let rotated = crate::recipes::rotate(img, deg, false, [0, 0, 0]);
                *img = rotated;
            }
        }
        // 阴影 / 高光:参考三根滑杆 Radius / Shadow % / Highlight %。
        // 只给通用强度滑杆 `amount` 时,按"两端同幅度"处理。
        "shadow" => {
            let radius = e
                .params
                .get("radius")
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(25.0);
            let pct = |key: &str| e.params.get(key).and_then(|v| v.parse::<f32>().ok());
            let (sh, hi) = match (pct("shadow"), pct("highlight")) {
                (Some(s), Some(h)) => (s, h),
                _ => {
                    let v = oneclick_amount(e, 0.0).clamp(0.0, 100.0) / 100.0;
                    (v, v)
                }
            };
            crate::recipes::shadow_highlight(img, radius, sh, hi);
        }
        // 反相(Invert Colors):参考就是一条 Master 曲线 `[(0,255), (255,0)]`
        // (见 `<effect>` 的曲线调整节点),数据已在资源包。
        "invert" => {
            crate::curves::apply_curves(img, &crate::curves::presets::invert());
        }
        "whitept" => crate::oneclick::apply_autocolor(img),
        "ansel" => {
            let w = crate::oneclick::luma_weights();
            crate::oneclick::apply_ansel(img, w, 64.0);
        }
        "blur" => {
            let v = oneclick_amount(e, 50.0) / 100.0;
            // 参考口径:滑杆量经阶梯量化(档位 {2.065, 3.0625, 4.13, 5.13}),
            // 再走与 recipes 同一条盒式模糊(内部含半图钳制)—— 库内两条
            // 模糊口径在效果算子这条路上已合并。
            let r = crate::recipes::blur_ladder(v * 8.0);
            if r > 0.0 {
                crate::recipes::box_blur(img, r, r);
            }
        }
        // glow / glow2:参考的光晕算子内部参数无对应资料,
        // 我方固定走 gamma 0.5 + 模糊 + 混回的中性光晕,不读 amount、
        // 阈值写死 128 —— 知情维持,调参留待有资料后再对齐。
        "glow" | "glow2" => {
            crate::oneclick::apply_glow(img, 128.0);
        }
        "colortemp" => {
            // 原版双轴:temp = Cool to Warm(−0.5..0.5)、white = White Shift(0..1)。
            // 兼容旧数据:只存了 amount(0..100)时按 50 = 中性折算到冷暖轴。
            let parse = |key: &str| e.params.get(key).and_then(|v| v.parse::<f32>().ok());
            let temp = match parse("temp") {
                Some(t) => t,
                None => (oneclick_amount(e, 50.0) - 50.0) / 100.0,
            };
            let white = parse("white").unwrap_or(0.0);
            crate::oneclick::apply_color_temp(img, temp, white);
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
        // 伽马校正:参考的 Level 滑杆量程 2.0、偏移 1.0,
        // 即实际指数 = 1 + 滑块值;这里把 amount 映射到 1 + 0.9 的最大偏移。
        "gamma" => {
            let v = oneclick_amount(e, 0.0) / 100.0;
            crate::oneclick::apply_gamma(img, 1.0 + v * 0.9);
        }
        // 线性模糊:方向由 `angle` 给出,跨度随强度增长。
        "linblur" => {
            let a = e
                .params
                .get("angle")
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(0.0);
            let v = oneclick_amount(e, 40.0);
            let long = img.width().max(img.height()) as f32;
            crate::recipes::linear_blur(img, a, v / 100.0 * long * 0.06);
        }
        // 焦点柔化:圆心保持清晰,越靠边越糊。
        "radblur" => {
            let v = oneclick_amount(e, 50.0) / 100.0;
            crate::recipes::soft_focus(img, 0.5, 0.5, 0.30, 0.75, v);
        }
        // 焦点去色:圆心保留色彩,外围转灰度。
        "radsat" => {
            let v = oneclick_amount(e, 50.0) / 100.0;
            // 强度越大,保留色彩的圆越小、过渡越缓
            let inner = 0.45 - 0.25 * v;
            let outer = inner + 0.10 + 0.25 * (1.0 - v);
            crate::recipes::focal_bw(img, 0.5, 0.5, inner, outer);
        }
        // 三个方向性调整:左→右 / 上→下两轴线性渐变。
        "dir_brite" | "dir_sat" | "dir_sharp" => {
            let kind = match e.name.to_ascii_lowercase().as_str() {
                "dir_sat" => crate::recipes::DirKind::Saturation,
                "dir_sharp" => crate::recipes::DirKind::Sharpen,
                _ => crate::recipes::DirKind::Brightness,
            };
            let lr = e
                .params
                .get("lr")
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(0.6);
            let tb = e
                .params
                .get("tb")
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(0.0);
            let v = oneclick_amount(e, 50.0) / 100.0;
            crate::recipes::directional_adjust(img, lr, tb, v, kind);
        }
        // 彩虹:先去饱和,再按亮度走完整一圈色相。
        //
        // 参考的滑杆量程 256、缺省 0,语义是色相起点(0~255 对应 0°~360°),
        // 这里把 amount 映射到同一起点偏移。
        "rainbow" => {
            crate::effects::apply_color_matrix(img, 0.0, 0.0, -100.0);
            let off = oneclick_amount(e, 0.0) / 100.0 * 255.0;
            // 拆成 6 段、每段 60°,这样相邻站点之间走最短弧不会绕回去
            let mut stops = Vec::with_capacity(7);
            for i in 0..7 {
                let pos = i as f32 * 255.0 / 6.0;
                let h = (off + i as f32 * 60.0) % 360.0;
                stops.push((pos, (h, 100.0, 100.0)));
            }
            crate::recipes::hsv_gradient_map(img, &stops, 0.0);
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
        _ => {
            // 成组配方(见 `crate::recipes`):一个名字对应一串固定顺序的操作,
            // 强度由 `amount`(0~100)统一控制 —— 与"淡出"滑杆语义等价;
            // 可选 `blend` 参数指定结果与原图的混合模式。
            if crate::recipes::is_recipe(&e.name) {
                let def = crate::recipes::default_amount(&e.name);
                let amt = oneclick_amount(e, def);
                match e.params.get("blend") {
                    Some(s) => crate::recipes::apply_with(
                        img,
                        &e.name,
                        amt,
                        crate::recipes::Blend::parse(s),
                    ),
                    None => crate::recipes::apply(img, &e.name, amt),
                };
            }
        }
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

    // ===== 审计 3.1 缺陷回归锁(B1 / B2 / B3 / B8 / B9)=====
    // 以下 5 条在修复前必须**失败**,用来固定缺陷结论本身,而不只是固定实现。

    /// B1:对比度不能在平移量上重复计入一次偏移。
    ///
    /// 标准式是 `out = (v − pivot)·k + pivot`:中灰**保持中灰**,负对比度收敛到中灰。
    /// 旧实现多算了一次 `+(1−k)·128`,于是 +100 档把中灰压成纯黑、−100 档把整图推成纯白。
    #[test]
    fn contrast_keeps_midgray_and_folds_to_midgray() {
        let run = |v: u8, c: f32| {
            let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([v, v, v, 255]));
            apply_color_matrix(&mut img, 0.0, c, 0.0);
            img.get_pixel(0, 0)[0]
        };
        assert_eq!(run(128, 100.0), 128, "正满档:中灰必须仍是中灰");
        assert_eq!(run(128, -100.0), 127, "负满档:应折叠到中灰 127.5 而非纯白");
        assert!(
            run(200, 100.0) > run(200, 0.0),
            "正对比度应把亮部继续推亮:"
        );
    }

    /// B2:色彩矩阵的饱和度必须走全库统一的亮度权重(0.3086/0.6094/0.082),
    /// 不能用 Rec.709 的 (0.2126/0.7152/0.0722)。取 `sat = −100`(退化为灰度)时
    /// 输出就是该位的亮度值 —— 纯红应为 **78**,而不是 Rec.709 的 54。
    #[test]
    fn saturation_uses_library_luma_weights() {
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        apply_color_matrix(&mut img, 0.0, 0.0, -100.0);
        let v = img.get_pixel(0, 0)[0];
        assert!((v as i32 - 78).abs() <= 1, "纯红应映射到 78,实得 {v}");
    }

    /// B3:饱和度必须是**非对称**的 —— 正方向按 3 倍增益走。
    ///
    /// 参考的确切规则:`s' = 1 + (sat > 0 ? 3·sat : sat) / 100`。
    /// 于是 `sat = +100` → `s' = 4`(不是线性式的 2);负方向仍是线性。
    #[test]
    fn positive_saturation_gain_is_tripled() {
        let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([140, 120, 130, 255]));
        apply_color_matrix(&mut img, 0.0, 0.0, 100.0);
        let p = img.get_pixel(0, 0);
        assert!((p[0] as i32 - 178).abs() <= 2, "s'=4 时 R 应约 178,实得 {p:?}");
        assert!((p[1] as i32 - 98).abs() <= 2, "s'=4 时 G 应约 98,实得 {p:?}");
    }

    /// B8:调整面板的曝光必须走**曝光曲线**(样条)路径,而不是线性 `2^stops`。
    ///
    /// 参考的曝光点表:`pos (0,17)(47,81)(129,186)(221,255)`、
    /// `neg (13,0)(116,74)(208,156)(255,221)`。输入 100 经曲线约为
    /// **+1 档 → 149**(−1 档 → 63),而线性乘法会给出 200 / 50。
    #[test]
    fn exposure_runs_through_the_curve_spline() {
        let run = |e: f32| {
            let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([100, 100, 100, 255]));
            apply(&mut img, &Adjust {
                exposure: e,
                ..Default::default()
            });
            img.get_pixel(0, 0)[0] as i32
        };
        let up = run(1.0);
        assert!(up > 130 && up < 170, "+1 档应走样条约 149,实得 {up}");
        let down = run(-1.0);
        assert!(down > 50 && down < 80, "-1 档应走样条约 63,实得 {down}");
    }

    /// B9:5 个组合效果不能塌成同一件事 —— 各自的滑杆必须只影响自己。
    ///
    /// 参考的滑杆清单(取自官方 `<filter>` 描述):
    /// `triple` = 亮度 / 对比度 / 补光;`triple2` = 补光 / 黑点 / 白点;
    /// `triple3` = 补光 / 高光 / 阴影;`finetune` = 补光 / 高光 / 阴影 / 色温;
    /// `finetune2` = 同上但色温轴宽一倍;`colorfix` = 白场校色 + 色温。
    ///
    /// 参数一律按**滑杆原值**(0..1)传入,再按参考的 `<range>` / `<offset>`
    /// 折算成物理量 —— 例如 finetune 的色温轴 range=1.0/offset=0.5,
    /// 滑杆 0..1 映射到 −0.5..0.5;finetune2 是 range=2.0/offset=1.0,映射到 −2..2。
    #[test]
    fn combo_effects_are_individually_wired() {
        // 注意:`temp` 传的是**滑杆原值**(0..1),0.5 即中位(中性);
        // finetune / finetune2 / colorfix 按各自的 range / offset 折算。
        // 构造效果项:`param` 为空时用参考的零档默认。
        fn mk(name: &str, params: &[(&str, &str)]) -> Effect {
            let mut m = BTreeMap::new();
            for (k, v) in params {
                m.insert(k.to_string(), v.to_string());
            }
            Effect {
                name: name.into(),
                params: m,
            }
        }
        let run = |name: &str, params: &[(&str, &str)]| {
            let mut img = image::RgbaImage::from_pixel(24, 24, image::Rgba([40, 90, 160, 255]));
            apply_effect(&mut img, &mk(name, params));
            img.into_raw()
        };

        // 各自的滑杆必须只影响自己(给自己的滑杆 → 结果改变)
        assert_ne!(
            run("triple", &[("brightness", "40")]),
            run("triple", &[]),
            "triple 应响应自己的 brightness 滑杆"
        );
        assert_ne!(
            run("triple", &[("contrast", "40")]),
            run("triple", &[]),
            "triple 应响应自己的 contrast 滑杆"
        );
        assert_ne!(
            run("triple2", &[("black", "0.3")]),
            run("triple2", &[]),
            "triple2 应响应自己的 black(黑点)滑杆"
        );
        assert_ne!(
            run("triple3", &[("highlights", "0.2")]),
            run("triple3", &[]),
            "triple3 应响应自己的 highlights 滑杆"
        );
        assert_ne!(
            run("finetune", &[("temp", "1.0")]),
            run("finetune", &[]),
            "finetune 应响应自己的 temp 滑杆"
        );
        assert_ne!(
            run("colorfix", &[("temp", "1.0")]),
            run("colorfix", &[]),
            "colorfix 应响应自己的 temp 滑杆"
        );

        // 别人的滑杆不能串到自己身上
        assert_eq!(
            run("triple2", &[("brightness", "40")]),
            run("triple2", &[]),
            "triple2 没有 brightness 滑杆,不该读取它"
        );

        // finetune 与 finetune2 的色温轴宽度不同(1.0 / 2.0),同一原始滑杆值结果不同
        assert_ne!(
            run("finetune", &[("temp", "1.0")]),
            run("finetune2", &[("temp", "1.0")]),
            "finetune2 的色温轴比 finetune 宽一倍,不应得到同一色温"
        );

        // zerostate:不给参数时,5 个照明/微调类效果都应是恒等(参考 zerostate 语义)
        let identity = image::RgbaImage::from_pixel(24, 24, image::Rgba([40, 90, 160, 255])).into_raw();
        for name in ["triple", "triple2", "triple3", "finetune", "finetune2"] {
            assert_eq!(run(name, &[]), identity, "{name} 的零档应是恒等");
        }
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

    /// B8(续):曝光必须**随档位单调**,且迭代结果始终落在 0..255 内。
    ///
    /// 负向点表末端是 (255,221) —— 即使 −3 档也不会把 250 压到纯黑,这是
    /// 曝光曲线本身的行为(负档只是整条曲线的 mild 压缩),不是钳位遗漏。
    #[test]
    fn exposure_extremes_stay_monotonic_and_in_range() {
        let run = |e: f32| {
            let mut img = image::RgbaImage::from_pixel(2, 2, image::Rgba([250, 250, 250, 255]));
            apply(&mut img, &Adjust {
                exposure: e,
                ..Default::default()
            });
            img.get_pixel(0, 0)[0]
        };
        assert_eq!(run(0.0), 250, "0 档应恒等");
        assert!(run(1.0) > 250, "+1 档应更亮");
        assert_eq!(run(3.0), 255, "+3 档应钳到 255");
        let (d1, d3) = (run(-1.0), run(-3.0));
        assert!(d3 < d1 && d1 < 250, "负档应单调压暗:{d3} < {d1} < 250");
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
        // 灰度值 = 提亮后三通道的 luma —— 必须用**本项目统一权重**
        // (0.3086/0.6094/0.082),不是 Rec.709:早前这里写死 Rec.709,
        // 等于把当时色彩矩阵用错权重的缺陷固化成了断言。
        let w = crate::oneclick::luma_weights();
        let orig_luma = w[0] * 200.0 + w[1] * 80.0 + w[2] * 60.0;
        let bright_luma = w[0] * 263.75 + w[1] * 143.75 + w[2] * 123.75;
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

    /// 第 2 项-a:`tilt`(Straighten)不能再是静默空操作。
    ///
    /// 参考的 `<filter id="tilt">` 只有一根双向滑杆(range 2.0 / offset 1.0),
    /// 所以转动的角度必须随滑块变化,而不是恒为 0。
    #[test]
    fn tilt_straightens_the_image() {
        let mut img = image::RgbaImage::from_pixel(16, 16, image::Rgba([0, 0, 0, 255]));
        for y in 6..10 {
            for x in 2..14 {
                img.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
            }
        }
        let before = img.clone();
        apply_effect(
            &mut img,
            &Effect {
                name: "tilt".into(),
                params: [("amount".to_string(), "100".to_string())]
                    .into_iter()
                    .collect(),
            },
        );
        assert_ne!(
            img.as_raw(),
            before.as_raw(),
            "tilt 没有改动任何像素 —— 仍是静默空操作"
        );
        // 水平白条转过之后,左右两端白像素所在的行号应当错开。
        let row_of = |img: &image::RgbaImage, x: u32| -> Option<u32> {
            (0..img.height()).find(|y| img.get_pixel(x, *y)[0] > 128)
        };
        assert_eq!(
            (row_of(&before, 3), row_of(&before, 12)),
            (Some(6), Some(6)),
            "基准:白条是水平的"
        );
        assert_ne!(
            (row_of(&img, 3), row_of(&img, 12)),
            (Some(6), Some(6)),
            "倾斜后两端行号应不再相同"
        );
    }

    /// 第 2 项-b:`shadow`(Shadow & Highlight)必须真正提亮暗部 / 压回亮部。
    #[test]
    fn shadow_highlight_lifts_darks_and_recovers_highlights() {
        // 暗底 + 中央一块亮方,使局部均值与像素值产生落差。
        let base = || {
            let mut img = image::RgbaImage::from_pixel(24, 24, image::Rgba([10, 10, 10, 255]));
            for y in 8..16 {
                for x in 8..16 {
                    img.put_pixel(x, y, image::Rgba([240, 240, 240, 255]));
                }
            }
            img
        };
        let run = |shadow: &str, highlight: &str| {
            let mut img = base();
            apply_effect(
                &mut img,
                &Effect {
                    name: "shadow".into(),
                    params: [
                        ("shadow".to_string(), shadow.to_string()),
                        ("highlight".to_string(), highlight.to_string()),
                        ("radius".to_string(), "3.0".to_string()),
                    ]
                    .into_iter()
                    .collect(),
                },
            );
            img
        };
        // 紧挨亮块外侧的暗像素:局部均值高于自身,应被提亮
        let dark_probe = |img: &image::RgbaImage| img.get_pixel(7, 12)[0];
        // 亮块**边缘内侧**的像素:邻域里混进暗像素,局部均值低于自身,
        // 开启高光压制时应回落。取平坦区内部(如 12,12)是恒等的 ——
        // 局部均值等于自身,这是该算法的应有行为。
        let bright_probe = |img: &image::RgbaImage| img.get_pixel(15, 12)[0];

        assert_eq!(dark_probe(&run("0.0", "0.0")), 10, "两项都是 0 时应恒等");
        let lifted = dark_probe(&run("1.0", "0.0"));
        assert!(lifted > 10, "暗部应被提亮,实得 {lifted}");
        let pressed = bright_probe(&run("0.0", "1.0"));
        assert!(pressed < 240, "亮部应被压回,实得 {pressed}");
    }

    /// 第 2 项-c:回归锁 —— `PIXEL_EFFECTS` 里除显式登记的名字外,
    /// 每一个都必须真的影响像素。
    #[test]
    fn every_known_effect_touches_pixels() {
        let sample = || {
            let mut img = image::RgbaImage::new(32, 32);
            for y in 0..32 {
                for x in 0..32 {
                    img.put_pixel(
                        x,
                        y,
                        image::Rgba([(x * 8) as u8, (y * 8) as u8, ((x + y) * 4) as u8, 255]),
                    );
                }
            }
            img
        };
        // 给足所有已知参数键,避免某个效果只是因为取不到自己的键才没变化。
        let strong = || -> std::collections::BTreeMap<String, String> {
            [
                ("amount", "100"),
                ("angle", "35"),
                ("color", "ff8040"),
                ("mix", "100"),
                ("seed", "1"),
                ("channeloptions", "7"),
                ("grayscale", "false"),
                ("lr", "0.9"),
                ("tb", "0.9"),
                ("brightness", "0.85"),
                ("contrast", "0.9"),
                ("fill", "0.8"),
                ("black", "0.3"),
                ("white", "0.7"),
                ("temp", "0.9"),
                ("highlights", "0.4"),
                ("shadows", "0.4"),
                ("highlight", "ffeedd"),
                ("shadow", "000000"),
                ("radius", "3.0"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
        };
        let mut silent: Vec<&str> = Vec::new();
        for name in PIXEL_EFFECTS {
            if DOCUMENTED_NOOPS.contains(name) {
                continue;
            }
            let mut img = sample();
            let before = img.as_raw().clone();
            apply_effect(
                &mut img,
                &Effect {
                    name: (*name).to_string(),
                    params: strong(),
                },
            );
            if img.as_raw() == &before {
                silent.push(name);
            }
        }
        assert!(
            silent.is_empty(),
            "这些效果名能被识别却不改像素(静默空操作):{silent:?}"
        );
    }

    /// 第 4 项-a:`Invert` —— 参考是 Master 曲线 `[(0,255), (255,0)]`,
    /// 曲线数据已在 `crate::curves::presets::invert()`,只差这条接线。
    #[test]
    fn invert_effect_negates_the_image() {
        let run = |pix: image::Rgba<u8>| {
            let mut img = image::RgbaImage::from_pixel(2, 2, pix);
            apply_effect(
                &mut img,
                &Effect {
                    name: "invert".into(),
                    params: Default::default(),
                },
            );
            *img.get_pixel(0, 0)
        };
        let white = run(image::Rgba([255, 255, 255, 255]));
        assert_eq!(white[0], 0, "纯白应反相成纯黑,实得 {}", white[0]);
        let mid = run(image::Rgba([64, 128, 200, 255]));
        assert!(
            (mid[1] as i32 - 127).abs() <= 4,
            "中灰反相后仍在中灰附近,实得 {}",
            mid[1]
        );
        assert!(
            mid[0] > 180 && mid[2] < 80,
            "反相应翻转明暗倾向:R={} B={}",
            mid[0],
            mid[2]
        );
    }
}
