//! 滚动阻尼:把"逐事件像素位移"整形成"平滑速度",供缩略图网格限速跟随。
//!
//! 下列常量与曲线形状取自 Picasa 3 的观感(参考其行为,非移植其代码),
//! 以保证迁移过来的用户对滚动手感不产生明显差异。
//!
//! # 阻尼算法
//!
//! ```text
//! if delta == 0            -> vel = 0;  return
//! v  = delta * (-2.4)                     
//! neg = v < 0                             
//! v  = |v|                                
//! range = (hi - lo)                      
//! t  = powf(v / range, 0.3)               
//! v  = t * A                              
//! if v < 100                              
//!     v = 100 * powf(v / 100, 4.25)       
//! if neg: v = -v                          
//! vel = v
//! ```
//!
//! ## 三处易错点(勿按直觉改)
//!
//! 1. **`0.3` 是 `powf` 的指数,不是比较阈值。** 它只作为 `powf` 第 2 参数。
//! 2. **`100` 是 ease-out 分支阈值 + 反变换缩放因子,不是 clamp。**
//!    `v >= 100` 时直接保留原值,**不做封顶**;因此这里**不能**用 `.clamp()`。
//! 3. **`0.5` 是包围盒中点系数(出现两次),不是"质心权重"。**
//!    缩略图质心位移算法(缩略图滚动事件):
//!    `delta = ((node_box.x0 + node_box.x2) * 0.5 - (content_box.x0 + content_box.x2) * 0.5) * 2.7`。
//!
//! 另外:`2.7` 是调用方的放大系数,`-2.4` 是阻尼计算首步的衰减系数。
//!
//! # 参数语义:`delta` 是"逐事件的像素位移"
//!
//! 各调用方:
//! - 键盘加速滚动 → 越界像素量 × 4
//! - 鼠标拖拽/滚轮 → 拖拽像素量 × 4
//! - 焦点切换/销毁 → 传 `0` 复位
//!
//! 所以阻尼计算的语义是:**把一次事件的像素位移整形成"平滑速度"**,
//! 供缩略图网格做限速跟随。
//!
//! # 与本项目的对接
//!
//! 落地为**滚动限速**:滚动越快 → 每帧允许同步解码的缩略图越少 → 大图库滚动不掉帧。
//!
//! `powf` 与系统 CRT `powf` 可能有 1 ULP 差异,测试用近似比较而非精确相等。

/// 7 个滚动事件名(滚动事件名匹配 / 滚动事件处理)。
///
/// 前 6 个按事件名派发给对应的滚动处理;
/// 第 7 个 `scroll/thumb` 走缩略图质心位移算法。
///
/// 本项目把同名事件用于**键盘翻页 / 相册跳转**(见 `app.rs` 的画廊快捷键)。
pub const SCROLL_EVENTS: [&str; 7] = [
    "scroll/album_top",
    "scroll/album_bottom",
    "scroll/page_up",
    "scroll/page_down",
    "scroll/next_album",
    "scroll/prev_album",
    "scroll/thumb",
];

/// 缩略图滚动事件名(唯一走质心位移算法的事件)。
///
/// 对应缩略图质心位移算法(滚动事件处理直接调它,不走派发路径)。
pub const EVENT_SCROLL_THUMB: &str = "scroll/thumb";

/// 该事件对应的质心位移算法(供 UI 侧标注当前生效的路径)。
pub fn thumb_event_name() -> &'static str {
    EVENT_SCROLL_THUMB
}

/// 滚动停止时的缩略图解码满额(每帧最多解码多少张)。
pub const MAX_THUMB_DECODE_PER_FRAME: usize = 64;

/// 阻尼计算的浮点常量(取自内置资源包 `scroll.damp`)。
mod consts {
    use crate::assets::scroll_damp;

    /// 阻尼系数(被调方首步)。
    pub fn damp() -> f64 {
        scroll_damp().damp
    }
    /// 调用方增益。
    pub fn gain() -> f64 {
        scroll_damp().gain
    }
    /// `powf` 指数(float)。
    pub fn exp_soft() -> f32 {
        scroll_damp().exp_soft
    }
    /// ease-out `powf` 指数(float)。
    pub fn exp_ease() -> f32 {
        scroll_damp().exp_ease
    }
    /// 分支阈值 + 反变换缩放因子。
    pub fn ease_scale() -> f64 {
        scroll_damp().ease_scale
    }
    /// 包围盒中点系数(缩略图质心位移算法中出现两次)。
    pub fn mid() -> f64 {
        scroll_damp().mid
    }
}

/// 内容对象的度量(阻尼计算读取的 4 个整数字段)。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ContentMetrics {
    /// 乘数 `A`(像素量级),`v = powf(...) * A` 里的 `A`。
    pub scale: f32,
    /// 可视范围下界(lo)。
    pub lo: f32,
    /// 预留字段(当前计算未使用,保留以对齐原始布局)。
    #[allow(dead_code)]
    pub _unused: f32,
    /// 可视范围上界(hi),`range = hi - lo`。
    pub hi: f32,
}

impl ContentMetrics {
    /// 可视范围 `hi - lo`(对应原版的计算)。
    pub fn range(&self) -> f32 {
        self.hi - self.lo
    }
}

/// 滚动阻尼器(对应 Picasa 3 滚动限速节点的**计算部分**)。
///
/// 保留原版的两个关键状态:
/// - `vel`:阻尼整形后的像素速度;
/// - `has_content`:是否已挂内容对象(未挂 → 范围视为 0)。
#[derive(Clone, Debug, Default)]
pub struct ScrollThrottle {
    /// 阻尼整形后的像素速度(px/帧)。初值 0.0。
    pub vel: f64,
    /// 是否已挂内容对象;未挂时范围视为 0。
    pub has_content: bool,
    /// 内容对象的度量。
    pub content: Option<ContentMetrics>,
    /// 滚动模式(原版由滚动容器写入;本项目不消费它,保留以对齐原始布局)。
    #[allow(dead_code)]
    pub mode: u8,
}

impl ScrollThrottle {
    /// 新建一个未挂内容对象的阻尼器。
    pub fn new() -> Self {
        Self {
            vel: 0.0,
            has_content: false,
            content: None,
            mode: 2,
        }
    }

    /// 挂上内容对象并更新可视范围。
    pub fn set_content(&mut self, m: ContentMetrics) {
        self.content = Some(m);
        self.has_content = true;
    }

    /// 核心阻尼函数(与 Picasa 3 行为一致)。
    ///
    /// `delta` 是**一次事件的像素位移**(见模块文档的参数语义说明)。
    ///
    /// ⚠️ 与原版的**唯一**刻意差异:原版在"无内容对象"时 `range == 0`,
    /// 会触发 `0/0 → NaN` 并把 NaN 写进速度状态;这里在无内容时直接
    /// 返回 0.0,避免 NaN 污染下游。**有内容对象时行为与原版完全一致。**
    pub fn damp(&mut self, delta: f32) -> f64 {
        if delta == 0.0 {
            // 原版:delta==0 → 立即复位 vel 并返回。
            self.vel = 0.0;
            return 0.0;
        }

        
        let mut v = delta as f64 * consts::damp();
        // 记录 delta 的符号
        let neg = v < 0.0;
        // 取绝对值
        v = v.abs();

        // 仅当存在内容对象时才取其实例
        let (a, range) = if self.has_content {
            match self.content {
                Some(c) => (c.scale as f64, c.range() as f64),
                None => (0.0, 0.0),
            }
        } else {
            (0.0, 0.0)
        };

        if range == 0.0 {
            // 原程序此处是 0/0 = NaN;我们直接归零(见函数文档说明)。
            self.vel = 0.0;
            return 0.0;
        }

        // 软缩放:t = (v/range)^0.3
        let t = (v / range).powf(consts::exp_soft() as f64);

        v = t * a;
        // 暂存速度
        self.vel = v;

        // v < 100 时走 ease-out 分支
        if v < consts::ease_scale() {

            v = (v / consts::ease_scale()).powf(consts::exp_ease() as f64) * consts::ease_scale();
        }
        // 恢复符号(原版在 ease-out 之后执行)
        if neg {
            v = -v;
        }
        // 写回最终速度
        self.vel = v;
        v
    }

    /// 复位(原版的唯一复位方式就是 `damp(0.0)`,等价于焦点切换时的复位 / 销毁时的复位的调用)。
    pub fn reset(&mut self) {
        self.vel = 0.0;
    }
}

/// 计算 `throttle/throttlethumb` 的质心位移增量。
///
/// 缩略图质心位移算法(与 Picasa 3 行为一致):
///
/// ```text
/// node_box    = 节点自身的变换后包围盒(40 字节 = 5 个 POINT,取 [0] 与 [2])
/// content_box = 内容对象的变换后包围盒
/// delta = ((node_box[0] + node_box[2]) * 0.5
///        - (content_box[0] + content_box[2]) * 0.5) * 2.7
/// ```
///
/// 注意 `* 0.5` 出现**两次**:这是 int→double 隐式提升
/// 一并算进表达式导致的显示效果,语义上就是"取包围盒首尾两点的中点"。
///
/// `node_box` / `content_box` 均为 5 个 POINT 的**滚动轴**分量
/// (原版结构为 5×POINT;这里只取滚动轴上的两个端点,放在槽位 0 与 2)。
pub fn thumb_delta(node_box: &[i32; 5], content_box: &[i32; 5]) -> f32 {
    let node_mid = (node_box[0] as f64 + node_box[2] as f64) * consts::mid();
    let content_mid = (content_box[0] as f64 + content_box[2] as f64) * consts::mid();
    
    ((node_mid - content_mid) * consts::gain()) as f32
}

/// 滚动限速器(本项目对 Picasa 3 滚动阻尼的等价落地)。
///
/// 职责链(与原版一致):
/// 1. `thumb_delta`:按原版缩略图质心位移算法的质心算法算出**这次滚动事件的像素位移**
///    (含原版的 × 2.7 调用方增益);
/// 2. `ScrollThrottle::damp`:按原版阻尼计算的阻尼曲线把它整形成**平滑速度**;
/// 3. `decode_budget`:用平滑速度决定"本帧允许同步解码多少张缩略图"。
#[derive(Clone, Debug)]
pub struct ScrollSmoother {
    th: ScrollThrottle,
    /// 上一帧的节点包围盒(滚动轴分量),用于逐帧求差。
    prev_node: [i32; 5],
    /// 内容包围盒(滚动轴分量)。
    content: [i32; 5],
    /// 上一次的原始位移(px)。
    last_raw: f32,
    /// 视口在滚动轴上的长度(px),作为阻尼的可视范围与标度。
    viewport: f32,
}

impl ScrollSmoother {
    /// 新建限速器。
    ///
    /// `viewport` = 视口在滚动轴上的长度(px);`content_len` = 内容总长(px)。
    pub fn new(viewport: f32, content_len: f32) -> Self {
        let viewport = viewport.max(1.0);
        let content_len = content_len.max(0.0);
        Self {
            th: ScrollThrottle::new(),
            // 初始节点盒 = 偏移 0 处的视口区间(否则首帧会算出一个假的"位移")。
            prev_node: [0, 0, viewport.round() as i32, 0, 0],
            content: [0, 0, content_len.round() as i32, 0, 0],
            last_raw: 0.0,
            viewport,
        }
    }

    /// 更新视口长度(px)。
    pub fn set_viewport(&mut self, h: f32) {
        self.viewport = h.max(1.0);
    }

    /// 更新内容总长(px)。
    pub fn set_content_len(&mut self, len: f32) {
        self.content = [0, 0, len.round() as i32, 0, 0];
    }

    /// 喂入**当前滚动偏移**(px,内容坐标系)与内容总长,返回整形后的速度(px/帧)。
    ///
    /// 内部完全按原版两步走:
    /// - `thumb_delta(node_now, content) - thumb_delta(node_prev, content)`
    ///   = 节点质心在两帧之间的位移 × 2.7(原版调用方增益);
    /// - `damp(delta)` → 原版阻尼计算的阻尼曲线。
    pub fn feed_offset(&mut self, offset: f32, content_len: f32) -> f64 {
        self.set_content_len(content_len);
        // 节点包围盒 = 视口在内容坐标系里的区间 [offset, offset + viewport]。
        let node = [
            offset.round() as i32,
            0,
            (offset + self.viewport).round() as i32,
            0,
            0,
        ];
        // 原版是"节点质心 vs 内容质心",这里逐帧求差 → 得到本次事件的像素位移。
        let disp_now = thumb_delta(&node, &self.content);
        let disp_prev = thumb_delta(&self.prev_node, &self.content);
        self.prev_node = node;
        let raw = disp_now - disp_prev;
        self.last_raw = raw;
        if raw == 0.0 {
            // 原版:delta==0 → 复位速度状态。
            self.th.reset();
            return 0.0;
        }
        // 内容对象度量:A 取视口长度(像素标度),可视范围取视口长度。
        self.th.set_content(ContentMetrics {
            scale: self.viewport,
            lo: 0.0,
            _unused: 0.0,
            hi: self.viewport,
        });
        self.th.damp(raw)
    }

    /// 当前整形后的速度(px/帧)。
    pub fn velocity(&self) -> f64 {
        self.th.vel
    }

    /// 上一次的原始位移(px)。
    pub fn last_raw(&self) -> f32 {
        self.last_raw
    }

    /// 复位(对应原版焦点切换时的复位 / 销毁时的复位的复位调用)。
    pub fn reset(&mut self) {
        self.th.reset();
        self.last_raw = 0.0;
        self.prev_node = [0, 0, 0, 0, 0];
    }
}

/// 把整形后的滚动速度换算成"本帧允许同步解码的缩略图数量"。
///
/// `velocity_px` 是整形后的速度(px/帧);`scale_px` 是标度
/// (本项目取视口长度,见 `ScrollSmoother::feed_offset`)。
///
/// 归一化后 `frac = |vel| / scale` ∈ [0,1]:
/// - `frac = 0`(没在滚)→ 满额;
/// - `frac` 越大 → 预算越小 → 极端时 0(只画灰色占位块,滚动永不掉帧)。
///
/// 映射是单调递减的:滚动越快 → 预算越小。
pub fn decode_budget(velocity_px: f64, scale_px: f64) -> usize {
    if !velocity_px.is_finite() || velocity_px <= 0.0 || scale_px <= 0.0 {
        return MAX_THUMB_DECODE_PER_FRAME;
    }
    let frac = (velocity_px.abs() / scale_px).clamp(0.0, 1.0);
    let budget = (MAX_THUMB_DECODE_PER_FRAME as f64 * (1.0 - frac)).round();
    budget.clamp(0.0, MAX_THUMB_DECODE_PER_FRAME as f64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    /// 原版的 `0.3` 是 **float** 常量(`0x3E99999A`),不是 f64 的 0.3。
    /// 期望值必须用同一个常量算,否则会有 ~1e-6 的偏差。
    const EXP_SOFT_F32: f32 = 0.3;

    /// 阻尼算法的黄金值:手工按阻尼计算算法得到的期望结果。
    ///
    /// 参数:delta=100, scale=100, range=100
    /// → v = 100 * -2.4 = -240 → |v| = 240
    /// → t = powf(240/100, 0.3) = powf(2.4, 0.3) ≈ 1.30036
    /// → v = 1.30036 * 100 = 130.036  (>= 100 → 不走 ease-out)
    /// → 恢复符号 → -130.036
    #[test]
    fn damp_matches_hand_computed_value() {
        let mut th = ScrollThrottle::new();
        th.set_content(ContentMetrics {
            scale: 100.0,
            lo: 0.0,
            _unused: 0.0,
            hi: 100.0,
        });
        let v = th.damp(100.0);

        let t = (240.0f64 / 100.0).powf(EXP_SOFT_F32 as f64);
        let expect = t * 100.0;
        assert!(
            approx(v, -expect, 1e-9),
            "阻尼结果应等于手算值: got {v}, expect {expect}"
        );
        assert!(approx(th.vel, v, 1e-12), "vel 应等于返回值");
    }

    /// ease-out 分支:`v < 100` 时施加 `100*powf(v/100, 4.25)`,且**不** clamp。
    #[test]
    fn damp_applies_ease_out_below_100_and_does_not_clamp() {
        let mut th = ScrollThrottle::new();
        th.set_content(ContentMetrics {
            scale: 1.0,
            lo: 0.0,
            _unused: 0.0,
            hi: 100.0,
        });
        let v = th.damp(100.0);

        let t = (240.0f64 / 100.0).powf(EXP_SOFT_F32 as f64);
        let expect = 100.0 * (t / 100.0).powf(4.25);
        assert!(
            approx(v, -expect, 1e-9),
            "<100 时应施加 ease-out: got {v}, expect {expect}"
        );
        assert!(v.abs() < 100.0, "ease-out 后应落在 100 以内");
    }

    /// `v >= 100` 时**不**做变换,也**不** clamp。
    #[test]
    fn damp_skips_ease_out_above_100_and_does_not_clamp() {
        let mut th = ScrollThrottle::new();
        th.set_content(ContentMetrics {
            scale: 1000.0,
            lo: 0.0,
            _unused: 0.0,
            hi: 100.0,
        });
        let v = th.damp(100.0);
        let t = (240.0f64 / 100.0).powf(EXP_SOFT_F32 as f64);
        let expect = t * 1000.0;
        assert!(
            approx(v, -expect, 1e-9),
            ">=100 时应保留原值(不 clamp): got {v}, expect {expect}"
        );
        assert!(v.abs() > 100.0, "绝不能被 clamp 到 100");
    }

    /// 负位移:符号必须恢复。
    #[test]
    fn damp_restores_sign_for_negative_delta() {
        let mut th = ScrollThrottle::new();
        th.set_content(ContentMetrics {
            scale: 1.0,
            lo: 0.0,
            _unused: 0.0,
            hi: 100.0,
        });
        let pos = th.damp(50.0);
        let neg = th.damp(-50.0);
        assert!(pos < 0.0, "正 delta 经 ×(-2.4) 后应为负");
        assert!(neg > 0.0, "负 delta 经 ×(-2.4) 后应为正");
        assert!(
            approx(pos.abs(), neg.abs(), 1e-12),
            "正负同幅应等幅反号"
        );
    }

    /// delta == 0 → 复位(对齐原版)。
    #[test]
    fn damp_zero_resets_velocity() {
        let mut th = ScrollThrottle::new();
        th.set_content(ContentMetrics {
            scale: 1.0,
            lo: 0.0,
            _unused: 0.0,
            hi: 100.0,
        });
        th.damp(100.0);
        assert!(th.vel != 0.0);
        let v = th.damp(0.0);
        assert_eq!(v, 0.0);
        assert_eq!(th.vel, 0.0, "delta==0 必须复位 vel");
    }

    /// 无内容对象 → 归零(原程序此处会产生 NaN,本项目刻意规避)。
    #[test]
    fn damp_without_content_is_zero_not_nan() {
        let mut th = ScrollThrottle::new();
        let v = th.damp(100.0);
        assert_eq!(v, 0.0);
        assert!(!v.is_nan(), "绝不能产生 NaN");
    }

    /// 单调性:输入越大,输出越大。
    #[test]
    fn damp_is_monotonic() {
        let mut th = ScrollThrottle::new();
        th.set_content(ContentMetrics {
            scale: 1.0,
            lo: 0.0,
            _unused: 0.0,
            hi: 100.0,
        });
        let mut prev = f64::NEG_INFINITY;
        for d in [1.0f32, 5.0, 20.0, 60.0, 200.0, 800.0] {
            let v = th.damp(d).abs();
            assert!(v > prev, "应单调递增: d={d} v={v} prev={prev}");
            prev = v;
        }
    }

    /// 质心位移:0.5 系数出现两次,整体 × 2.7。
    #[test]
    fn thumb_delta_matches_formula() {
        let node = [0i32, 0, 100, 0, 0];
        let content = [0i32, 0, 0, 0, 0];
        // ((0+100)*0.5 - (0+0)*0.5) * 2.7 = 50 * 2.7 = 135
        assert!(approx(thumb_delta(&node, &content) as f64, 135.0, 1e-6));

        let node2 = [10i32, 0, 30, 0, 0]; // 中点 20
        let content2 = [0i32, 0, 10, 0, 0]; // 中点 5
        // (20 - 5) * 2.7 = 40.5
        assert!(approx(
            thumb_delta(&node2, &content2) as f64,
            40.5,
            1e-6
        ));
    }

    /// 质心位移为 0(两者对齐)→ delta 为 0。
    #[test]
    fn thumb_delta_zero_when_aligned() {
        let b = [7i32, 0, 23, 0, 0];
        assert_eq!(thumb_delta(&b, &b), 0.0);
    }

    /// 7 个滚动事件名(清单,防止写错)。
    #[test]
    fn scroll_event_names_are_stable() {
        assert_eq!(SCROLL_EVENTS.len(), 7);
        assert_eq!(SCROLL_EVENTS[0], "scroll/album_top");
        assert_eq!(SCROLL_EVENTS[1], "scroll/album_bottom");
        assert_eq!(SCROLL_EVENTS[2], "scroll/page_up");
        assert_eq!(SCROLL_EVENTS[3], "scroll/page_down");
        assert_eq!(SCROLL_EVENTS[4], "scroll/next_album");
        assert_eq!(SCROLL_EVENTS[5], "scroll/prev_album");
        assert_eq!(SCROLL_EVENTS[6], EVENT_SCROLL_THUMB);
        // 全部以 "scroll/" 前缀命名
        for e in SCROLL_EVENTS {
            assert!(e.starts_with("scroll/"), "事件名应带 scroll/ 前缀: {e}");
        }
    }

    /// 限速器:逐帧偏移变化 → 质心位移 → 阻尼 → 速度。
    #[test]
    fn smoother_computes_velocity_from_offset_changes() {
        let mut s = ScrollSmoother::new(600.0, 6000.0);
        // 第一帧:偏移 0 → 位移 0 → 复位,速度 0
        assert_eq!(s.feed_offset(0.0, 6000.0), 0.0);
        // 第二帧:偏移 +100 → 质心位移 = 100 * 2.7 = 270
        let v = s.feed_offset(100.0, 6000.0);
        assert!(v != 0.0, "有滚动时应产生非零速度");
        assert!(v.is_finite());
        assert!(
            approx(s.last_raw() as f64, 270.0, 1e-3),
            "原始位移应含 ×2.7 增益"
        );
    }

    /// 限速器:偏移不变 → 速度归零(原版 `damp(0)` 复位)。
    #[test]
    fn smoother_resets_when_offset_unchanged() {
        let mut s = ScrollSmoother::new(600.0, 6000.0);
        s.feed_offset(0.0, 6000.0);
        s.feed_offset(100.0, 6000.0);
        assert!(s.velocity() != 0.0);
        let v = s.feed_offset(100.0, 6000.0);
        assert_eq!(v, 0.0, "偏移不变 → 位移 0 → 复位");
        assert_eq!(s.velocity(), 0.0);
    }

    /// 限速器:反向滚动 → 速度符号相反。
    #[test]
    fn smoother_reverses_sign() {
        let mut s = ScrollSmoother::new(600.0, 6000.0);
        s.feed_offset(0.0, 6000.0);
        let up = s.feed_offset(100.0, 6000.0);
        let down = s.feed_offset(0.0, 6000.0);
        assert!(up.is_sign_negative() != down.is_sign_negative(), "反向应异号");
    }

    /// 限速器复位。
    #[test]
    fn smoother_reset_clears_state() {
        let mut s = ScrollSmoother::new(600.0, 6000.0);
        s.feed_offset(0.0, 6000.0);
        s.feed_offset(100.0, 6000.0);
        s.reset();
        assert_eq!(s.velocity(), 0.0);
        assert_eq!(s.last_raw(), 0.0);
    }

    /// 解码预算:不滚动 → 满额;滚动越快 → 预算越小,最低 0。
    #[test]
    fn decode_budget_is_monotonic_decreasing() {
        assert_eq!(decode_budget(0.0, 600.0), MAX_THUMB_DECODE_PER_FRAME);
        assert_eq!(decode_budget(-5.0, 600.0), MAX_THUMB_DECODE_PER_FRAME);
        let mut prev = usize::MAX;
        for v in [0.0, 60.0, 300.0, 600.0, 3000.0] {
            let b = decode_budget(v, 600.0);
            assert!(b <= prev, "预算应单调不增: v={v} b={b} prev={prev}");
            prev = b;
        }
        assert_eq!(decode_budget(1e9, 600.0), 0, "极快滚动应降到 0");
    }

    /// 预算永远不超过满额,且不会下溢。
    #[test]
    fn decode_budget_never_overflows() {
        for v in [0.0, 1.0, 1e3, 1e6, f64::INFINITY] {
            let b = decode_budget(v, 600.0);
            assert!(b <= MAX_THUMB_DECODE_PER_FRAME);
        }
        // scale 非法时退回满额
        assert_eq!(decode_budget(100.0, 0.0), MAX_THUMB_DECODE_PER_FRAME);
    }
}
