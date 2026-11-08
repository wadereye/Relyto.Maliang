//! 全局主题:两套配色,同一套版式。
//!
//! - `灰色`(默认):中性灰面(#F3F3F3)+ 选中蓝(#25648B),亮色方案。
//! - `蓝黑`:同一版式的暗色方案,点缀换成科技感的浅蓝。
//!
//! 配色通过 `Kind` 在运行时切换(`选项 > 常规`),取色一律走 `theme::p()`,
//! 不要新增写死颜色的常量。

use std::sync::atomic::{AtomicU8, Ordering};

use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, TextStyle, Visuals};

/// 缩略图卡片圆角(与配色无关)。
pub const CARD_RADIUS: u8 = 8;
/// 图片圆角(卡片内;与配色无关)。
pub const IMAGE_RADIUS: u8 = 6;

/// 当前配色方案。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// 灰面浅底,默认。
    Gray,
    /// 蓝黑暗底,科技感点缀。
    Blue,
}

impl Kind {
    /// 设置项 `ui_theme` 的取值。
    pub fn key(self) -> &'static str {
        match self {
            Kind::Gray => "gray",
            Kind::Blue => "blue",
        }
    }

    pub fn from_key(s: &str) -> Option<Kind> {
        match s {
            "gray" => Some(Kind::Gray),
            "blue" => Some(Kind::Blue),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Gray => "灰色(默认)",
            Kind::Blue => "蓝黑",
        }
    }
}

/// 一套配色。字段只描述颜色,版式(圆角/间距/字号)两套共用。
pub struct Palette {
    pub kind: Kind,
    /// 照片网格所在的画布底色。
    pub bg: Color32,
    /// 顶栏/底栏/侧栏的抬升面。
    pub surface: Color32,
    /// 缩略图卡片底色。
    pub card: Color32,
    /// 卡片悬停底色。
    pub card_hover: Color32,
    /// 占位块底色(缩略图未解码)。
    pub placeholder: Color32,
    /// 常规分隔线。
    pub stroke: Color32,
    /// 悬停时的描边。
    pub stroke_light: Color32,
    /// 主文字。
    pub text: Color32,
    /// 次要文字。
    pub muted: Color32,
    /// 更弱的提示文字。
    pub faint: Color32,
    /// 强调色(选中描边、链接)。
    pub accent: Color32,
    /// 强调色深/亮调(激活文字)。
    pub accent_strong: Color32,
    /// 强调色的浅铺底(选中行、选中控件)。
    pub accent_soft: Color32,
    /// 输入框底色。
    pub extreme_bg: Color32,
    /// 控件(勾选框、下拉、按钮)底色。
    pub widget_bg: Color32,
    /// 控件悬停底色。
    pub widget_hover: Color32,
    /// 控件按下/展开底色。
    pub widget_active: Color32,
    /// 窗口与弹层的投影色。
    pub shadow: Color32,
    /// 缩略图角标(星标)的圆底。
    pub badge: Color32,
    /// 角标上「未加星」图形色(强调态直接用 `accent`)。
    pub badge_mute: Color32,
    /// 是否为亮色方案(决定 egui 基线 visuals)。
    pub is_light: bool,
}

impl Palette {
    /// 灰面:中性灰面 + 选中蓝。
    const GRAY: Palette = Palette {
        kind: Kind::Gray,
        bg: Color32::from_rgb(0xED, 0xED, 0xED),
        surface: Color32::from_rgb(0xF6, 0xF6, 0xF6),
        card: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        card_hover: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        placeholder: Color32::from_rgb(0xDC, 0xDC, 0xDC),
        stroke: Color32::from_rgb(0xC8, 0xC8, 0xC8),
        stroke_light: Color32::from_rgb(0xA4, 0xA4, 0xA4),
        text: Color32::from_rgb(0x2A, 0x2A, 0x2A),
        muted: Color32::from_rgb(0x66, 0x66, 0x66),
        faint: Color32::from_rgb(0x96, 0x96, 0x96),
        accent: Color32::from_rgb(0x25, 0x64, 0x8B),
        accent_strong: Color32::from_rgb(0x17, 0x4C, 0x6E),
        accent_soft: Color32::from_rgb(0xD9, 0xE6, 0xEE),
        extreme_bg: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        widget_bg: Color32::from_rgb(0xFF, 0xFF, 0xFF),
        widget_hover: Color32::from_rgb(0xE9, 0xF1, 0xF6),
        widget_active: Color32::from_rgb(0xD9, 0xE6, 0xEE),
        shadow: Color32::from_rgba_premultiplied(0, 0, 0, 34),
        badge: Color32::from_rgba_premultiplied(255, 255, 255, 226),
        badge_mute: Color32::from_rgb(0x8A, 0x8A, 0x8A),
        is_light: true,
    };

    /// 蓝黑:同一版式的暗色方案,浅蓝点缀。
    const BLUE: Palette = Palette {
        kind: Kind::Blue,
        bg: Color32::from_rgb(0x0E, 0x14, 0x18),
        surface: Color32::from_rgb(0x15, 0x1E, 0x24),
        card: Color32::from_rgb(0x1B, 0x26, 0x2E),
        card_hover: Color32::from_rgb(0x23, 0x30, 0x39),
        placeholder: Color32::from_rgb(0x18, 0x22, 0x2A),
        stroke: Color32::from_rgb(0x2A, 0x3A, 0x45),
        stroke_light: Color32::from_rgb(0x3D, 0x55, 0x66),
        text: Color32::from_rgb(0xE3, 0xEE, 0xF6),
        muted: Color32::from_rgb(0x92, 0xA9, 0xB8),
        faint: Color32::from_rgb(0x5E, 0x74, 0x84),
        accent: Color32::from_rgb(0x4F, 0xC3, 0xF7),
        accent_strong: Color32::from_rgb(0x9B, 0xDD, 0xFF),
        accent_soft: Color32::from_rgb(0x14, 0x30, 0x40),
        extreme_bg: Color32::from_rgb(0x0A, 0x10, 0x14),
        widget_bg: Color32::from_rgb(0x1E, 0x2C, 0x35),
        widget_hover: Color32::from_rgb(0x27, 0x40, 0x4E),
        widget_active: Color32::from_rgb(0x2E, 0x55, 0x68),
        shadow: Color32::from_rgba_premultiplied(0, 0, 0, 120),
        badge: Color32::from_rgba_premultiplied(0, 0, 0, 170),
        badge_mute: Color32::from_rgb(0xE1, 0xE1, 0xE1),
        is_light: false,
    };
}

static PALETTES: [Palette; 2] = [Palette::GRAY, Palette::BLUE];
const DEFAULT_KIND: Kind = Kind::Gray;

static CURRENT: AtomicU8 = AtomicU8::new(match DEFAULT_KIND {
    Kind::Gray => 0,
    Kind::Blue => 1,
});

/// 当前生效的配色。渲染期随处可取。
pub fn p() -> &'static Palette {
    let i = CURRENT.load(Ordering::Relaxed) as usize;
    &PALETTES[i.min(PALETTES.len() - 1)]
}

pub fn kind() -> Kind {
    p().kind
}

/// 切换配色(只改状态,不重刷 visuals;配合 `apply` 使用)。
pub fn set_kind(k: Kind) {
    CURRENT.store(
        match k {
            Kind::Gray => 0,
            Kind::Blue => 1,
        },
        Ordering::Relaxed,
    );
}

/// 按设置项取值切换并落回 egui style。
pub fn switch(ctx: &egui::Context, k: Kind) {
    set_kind(k);
    apply(ctx);
}

/// 把当前配色写进 egui style。
pub fn apply(ctx: &egui::Context) {
    let t = p();
    let mut style = (*ctx.style()).clone();
    let mut v = if t.is_light { Visuals::light() } else { Visuals::dark() };

    v.panel_fill = t.bg;
    v.window_fill = t.surface;
    v.window_stroke = Stroke::new(1.0_f32, t.stroke);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    let shadow = egui::epaint::Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: t.shadow,
    };
    v.window_shadow = shadow;
    v.popup_shadow = shadow;
    v.extreme_bg_color = t.extreme_bg;
    v.text_edit_bg_color = Some(t.extreme_bg);
    v.faint_bg_color = t.surface;
    v.code_bg_color = t.card;
    v.hyperlink_color = t.accent;
    v.weak_text_color = Some(t.muted);
    v.selection.bg_fill = t.accent_soft;
    v.selection.stroke = Stroke::new(1.0_f32, t.accent);
    v.indent_has_left_vline = false;
    v.interact_cursor = Some(egui::CursorIcon::PointingHand);

    v.widgets.noninteractive.bg_fill = t.surface;
    v.widgets.noninteractive.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, t.stroke);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, t.text);
    v.widgets.noninteractive.corner_radius = CornerRadius::same(IMAGE_RADIUS);

    v.widgets.inactive.bg_fill = t.widget_bg;
    v.widgets.inactive.weak_bg_fill = t.card;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, t.stroke);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, t.text);
    v.widgets.inactive.corner_radius = CornerRadius::same(IMAGE_RADIUS);

    v.widgets.hovered.bg_fill = t.widget_hover;
    v.widgets.hovered.weak_bg_fill = t.card_hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, t.stroke_light);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, t.accent_strong);
    v.widgets.hovered.corner_radius = CornerRadius::same(IMAGE_RADIUS);

    v.widgets.active.bg_fill = t.widget_active;
    v.widgets.active.weak_bg_fill = t.widget_active;
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, t.accent);
    v.widgets.active.fg_stroke = Stroke::new(1.0_f32, t.accent_strong);
    v.widgets.active.corner_radius = CornerRadius::same(IMAGE_RADIUS);

    v.widgets.open = v.widgets.active.clone();

    style.visuals = v;
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.window_margin = Margin::same(10);
    style.spacing.menu_margin = Margin::same(6);
    style.text_styles = [
        (TextStyle::Heading, FontId::new(18.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(12.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
    ]
    .into();

    // egui 0.33 按主题分存 dark_style / light_style 两份,set_style 只写「当前主题」那一份。
    // 首帧之前系统主题还没传进来,写进去的往往是 dark 槽;Windows 处于浅色时,第一帧会切到
    // 从未被改过的 light_style 默认值,整套配色/间距/字号全部失效。两个槽都写才稳。
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
}

/// 顶栏/底栏的抬升面。
pub fn bar_frame() -> egui::Frame {
    let t = p();
    egui::Frame::new().fill(t.surface).inner_margin(Margin {
        left: 10,
        right: 10,
        top: 4,
        bottom: 4,
    })
}

/// 侧栏的抬升面。
pub fn side_frame() -> egui::Frame {
    let t = p();
    egui::Frame::new().fill(t.surface).inner_margin(Margin {
        left: 10,
        right: 10,
        top: 10,
        bottom: 10,
    })
}

/// 侧栏小标题:小号、弱色、加粗,替代大号 heading。
pub fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(text)
            .size(12.0)
            .color(p().muted)
            .strong(),
    );
    ui.add_space(4.0);
}
