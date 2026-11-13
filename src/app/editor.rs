//! 单图编辑视图。
//!
//! 打开预览后整页接管:顶部导航 + 邻片小图,左栏分类工具 / 撤销重做 /
//! 直方图与相机信息,中间效果预览,右栏常驻属性,底部邻片胶片带。
//! 工具直接作用于当前照片,不再藏在菜单里;带滑杆的工具先只进预览栈,
//! 点「应用」才写入效果栈并记一步撤销。

use super::App;
use crate::i18n::{t, t_with};
use eframe::egui;
use std::collections::BTreeMap;
use std::path::Path;

const LEFT_W: f32 = 238.0;
const RIGHT_W: f32 = 272.0;
const HEADER_H: f32 = 74.0;
const FILM_H: f32 = 116.0;
const LEFT_BOTTOM_H: f32 = 252.0;
const TILE_H: f32 = 56.0;
const THUMB_CAP: usize = 80;

/// 左栏工具分类页。图标 + 中文名,一眼能找到工具在哪一类。
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum EditorTab {
    /// 基本修复:一键修复、常用调色、旋转。
    #[default]
    Basic,
    /// 微调:亮度 / 对比度 / 饱和度 / 曝光等滑杆。
    Tuning,
    /// 效果:一键效果与风格配方。
    Effects,
    /// 曲线。
    Curves,
    /// 裁剪:比例预设 + 图上拖框。
    Crop,
    /// 信息:标题 / 标签 / 星标 / 照片动作。
    Info,
}

impl EditorTab {
    const ALL: [EditorTab; 6] = [
        EditorTab::Basic,
        EditorTab::Tuning,
        EditorTab::Effects,
        EditorTab::Curves,
        EditorTab::Crop,
        EditorTab::Info,
    ];

    fn glyph(self) -> &'static str {
        match self {
            EditorTab::Basic => "✂",
            EditorTab::Tuning => "☼",
            EditorTab::Effects => "✦",
            EditorTab::Curves => "◢",
            EditorTab::Crop => "▣",
            EditorTab::Info => "ⓘ",
        }
    }

    fn label(self) -> &'static str {
        match self {
            EditorTab::Basic => t("基本修复"),
            EditorTab::Tuning => t("微调"),
            EditorTab::Effects => t("效果"),
            EditorTab::Curves => t("曲线"),
            EditorTab::Crop => t("裁剪"),
            EditorTab::Info => t("信息"),
        }
    }
}

/// 预览图的 RGB 直方图(每通道 64 桶)。只在预览纹理重算时统计一次。
#[derive(Clone)]
pub struct Histogram {
    r: [u32; 64],
    g: [u32; 64],
    b: [u32; 64],
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            r: [0; 64],
            g: [0; 64],
            b: [0; 64],
        }
    }
}

impl Histogram {
    pub(super) fn of(img: &image::RgbaImage) -> Self {
        let mut h = Self::default();
        let total = (img.width() as usize).saturating_mul(img.height() as usize);
        let step = (total / 240_000).max(1);
        for (i, px) in img.pixels().enumerate() {
            if i % step != 0 {
                continue;
            }
            h.r[px[0] as usize * 64 / 256] += 1;
            h.g[px[1] as usize * 64 / 256] += 1;
            h.b[px[2] as usize * 64 / 256] += 1;
        }
        h
    }

    fn peak(&self) -> f32 {
        self.r
            .iter()
            .chain(self.g.iter())
            .chain(self.b.iter())
            .copied()
            .max()
            .unwrap_or(1)
            .max(1) as f32
    }
}

/// 工具格子点击后的动作。
#[derive(Clone, Copy)]
enum Act {
    /// 立即把该效果写进效果栈(一步撤销)。
    Effect(&'static str, &'static str),
    /// 打开强度滑杆,先只进预览栈。
    Strength(&'static str, f32),
    /// 切到另一分类页。
    Page(EditorTab),
    /// 90° 旋转(quarter 顺时针次数)。
    Rotate(u32),
    /// 清除这张的全部效果。
    Clear,
}

struct Tile {
    glyph: &'static str,
    label: &'static str,
    act: Act,
}

/// 基本修复页的工具。每个格子都有简笔图标与名称,点了就见效(语言可切换,不能是 const)。
fn basic_tools() -> Vec<Tile> {
    vec![
        Tile {
            glyph: "✂",
            label: t("裁剪"),
            act: Act::Page(EditorTab::Crop),
        },
        Tile {
            glyph: "↗",
            label: t("拉直"),
            act: Act::Strength("tilt", 50.0),
        },
        Tile {
            glyph: "✦",
            label: t("手气不错"),
            act: Act::Effect("enhance_auto", t("手气不错")),
        },
        Tile {
            glyph: "◐",
            label: t("自动对比度"),
            act: Act::Effect("autocontrast", t("自动对比度")),
        },
        Tile {
            glyph: "◑",
            label: t("自动调色"),
            act: Act::Effect("autocolor", t("自动调色")),
        },
        Tile {
            glyph: "☼",
            label: t("补光"),
            act: Act::Strength("fill", 50.0),
        },
        Tile {
            glyph: "◈",
            label: t("锐化"),
            act: Act::Strength("unsharp", 60.0),
        },
        Tile {
            glyph: "◌",
            label: t("暗角"),
            act: Act::Strength("vignette", 50.0),
        },
        Tile {
            glyph: "☾",
            label: t("暗部高光"),
            act: Act::Strength("shadow", 50.0),
        },
        Tile {
            glyph: "▤",
            label: t("黑白"),
            act: Act::Effect("bw", t("黑白")),
        },
        Tile {
            glyph: "▩",
            label: t("棕褐色"),
            act: Act::Effect("sepia", t("棕褐色")),
        },
        Tile {
            glyph: "❄",
            label: t("冷调"),
            act: Act::Strength("cool", 50.0),
        },
        Tile {
            glyph: "☀",
            label: t("暖调"),
            act: Act::Strength("warm", 50.0),
        },
        Tile {
            glyph: "↺",
            label: t("向左旋转"),
            act: Act::Rotate(3),
        },
        Tile {
            glyph: "↻",
            label: t("向右旋转"),
            act: Act::Rotate(1),
        },
        Tile {
            glyph: "◍",
            label: t("褪色"),
            act: Act::Strength("faded", 50.0),
        },
        Tile {
            glyph: "✳",
            label: t("颗粒"),
            act: Act::Strength("grain", 15.0),
        },
        Tile {
            glyph: "✕",
            label: t("清除效果"),
            act: Act::Clear,
        },
    ]
}

/// 一个自绘的图标按钮:上方符号、下方中文名,整块可点。
fn tool_tile(
    ui: &mut egui::Ui,
    size: egui::Vec2,
    glyph: &str,
    label: &str,
    on: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let t = crate::theme::p();
    let p = ui.painter_at(rect);
    let bg = if on {
        t.accent_soft
    } else if resp.hovered() {
        t.widget_hover
    } else {
        t.widget_bg
    };
    p.rect_filled(rect, 6.0, bg);
    p.rect_stroke(
        rect,
        6.0,
        egui::Stroke::new(
            if on { 1.5_f32 } else { 0.8_f32 },
            if on { t.accent } else { t.stroke },
        ),
        egui::StrokeKind::Inside,
    );
    p.text(
        egui::pos2(rect.center().x, rect.top() + 5.0),
        egui::Align2::CENTER_TOP,
        glyph,
        egui::FontId::proportional(23.0),
        if on { t.accent_strong } else { t.text },
    );
    p.text(
        egui::pos2(rect.center().x, rect.bottom() - 6.0),
        egui::Align2::CENTER_BOTTOM,
        label,
        egui::FontId::proportional(11.0),
        t.text,
    );
    resp.on_hover_text(label)
}

fn tool_grid(ui: &mut egui::Ui, tiles: &[Tile], active: Option<&'static str>) -> Option<Act> {
    let w = ((ui.available_width() - 8.0) / 3.0).max(58.0);
    let mut hit = None;
    egui::Grid::new("editor_tool_grid")
        .num_columns(3)
        .spacing(egui::vec2(4.0, 4.0))
        .show(ui, |ui| {
            for (i, t) in tiles.iter().enumerate() {
                let on = match t.act {
                    Act::Strength(n, _) => active == Some(n),
                    _ => false,
                };
                if tool_tile(ui, egui::vec2(w, TILE_H), t.glyph, t.label, on).clicked() {
                    hit = Some(t.act);
                }
                if i % 3 == 2 {
                    ui.end_row();
                }
            }
            if tiles.len() % 3 != 0 {
                ui.end_row();
            }
        });
    hit
}

/// 邻片缩略图格子(顶部小图与底部胶片带共用)。
fn film_tile(
    ui: &mut egui::Ui,
    tex: Option<egui::TextureHandle>,
    current: bool,
    size: egui::Vec2,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let t = crate::theme::p();
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 3.0, if current { t.accent_soft } else { t.card });
    if let Some(tex) = tex {
        p.image(
            tex.id(),
            rect.shrink(3.0),
            super::cover_uv(tex.size()),
            egui::Color32::WHITE,
        );
    } else {
        p.rect_filled(rect.shrink(3.0), 2.0, t.placeholder);
    }
    p.rect_stroke(
        rect,
        3.0,
        egui::Stroke::new(
            if current { 2.0_f32 } else { 0.8_f32 },
            if current { t.accent } else { t.stroke },
        ),
        egui::StrokeKind::Inside,
    );
    resp
}

/// RGB 直方图:三通道半透明叠加,暗部用平方根抬一点,免得全挤在左边。
fn draw_hist(ui: &mut egui::Ui, h: &Histogram) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width().max(80.0), 78.0),
        egui::Sense::hover(),
    );
    let t = crate::theme::p();
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 4.0, t.extreme_bg);
    p.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(0.8_f32, t.stroke),
        egui::StrokeKind::Inside,
    );
    let peak = h.peak();
    let bw = rect.width() / 64.0;
    let colors = [
        (&h.r, t.text, (232u8, 62u8, 62u8)),
        (&h.g, t.text, (56u8, 176u8, 78u8)),
        (&h.b, t.text, (64u8, 112u8, 236u8)),
    ];
    for (vals, _, (cr, cg, cb)) in colors {
        for (i, v) in vals.iter().enumerate() {
            let f = (*v as f32 / peak).sqrt().min(1.0);
            let hh = f * (rect.height() - 4.0);
            if hh < 0.6 {
                continue;
            }
            p.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(rect.left() + i as f32 * bw, rect.bottom() - 2.0 - hh),
                    egui::vec2(bw.max(1.0), hh),
                ),
                0.0,
                egui::Color32::from_rgba_unmultiplied(cr, cg, cb, 110),
            );
        }
    }
}

/// 并排对比:左原图、右效果图,各自适应半屏(此模式不参与缩放/平移)。
fn split_paint(
    p: &egui::Painter,
    rect: egui::Rect,
    orig: Option<&egui::TextureHandle>,
    edited: &egui::TextureHandle,
    tw: usize,
    th: usize,
) {
    let gap = 10.0;
    let half = (rect.width() - gap) * 0.5;
    let panes = [rect.left(), rect.left() + half + gap];
    let texs = [orig, Some(edited)];
    let names = [t("原图"), t("效果")];
    for i in 0..2 {
        let r = egui::Rect::from_min_max(
            egui::pos2(panes[i], rect.top()),
            egui::pos2(panes[i] + half, rect.bottom()),
        );
        let s = (r.width() / tw.max(1) as f32)
            .min(r.height() / th.max(1) as f32)
            .min(1.0);
        let ir = egui::Rect::from_center_size(r.center(), egui::vec2(tw as f32 * s, th as f32 * s));
        let Some(t) = texs[i] else {
            continue;
        };
        p.image(
            t.id(),
            ir,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
        let label = egui::pos2(ir.center().x, ir.top() + 8.0);
        let galley = p.layout_no_wrap(
            names[i].to_string(),
            egui::FontId::proportional(12.0),
            egui::Color32::WHITE,
        );
        let bg = egui::Rect::from_center_size(
            label + egui::vec2(0.0, galley.size().y * 0.5),
            galley.size() + egui::vec2(10.0, 4.0),
        );
        p.rect_filled(bg, 3.0, egui::Color32::from_black_alpha(150));
        p.galley(bg.min + egui::vec2(5.0, 2.0), galley, egui::Color32::WHITE);
    }
}

/// 相机信息:只取常见标签,读一次文件后按 id 缓存,换图才重读。
fn exif_rows(path: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(f) = std::fs::File::open(path) else {
        return out;
    };
    let Ok(exif) = exif::Reader::new().read_from_container(&mut std::io::BufReader::new(f)) else {
        return out;
    };
    let tags: &[(&str, exif::Tag)] = &[
        (t("厂商"), exif::Tag::Make),
        (t("机型"), exif::Tag::Model),
        (t("拍摄时间"), exif::Tag::DateTimeOriginal),
        (t("快门"), exif::Tag::ExposureTime),
        (t("光圈"), exif::Tag::FNumber),
        ("ISO", exif::Tag::ISOSpeed),
        (t("焦距"), exif::Tag::FocalLength),
        (t("曝光补偿"), exif::Tag::ExposureBiasValue),
        (t("测光模式"), exif::Tag::MeteringMode),
        (t("曝光程序"), exif::Tag::ExposureProgram),
        (t("闪光灯"), exif::Tag::Flash),
        (t("白平衡"), exif::Tag::WhiteBalance),
        (t("色彩空间"), exif::Tag::ColorSpace),
        (t("方向"), exif::Tag::Orientation),
        (t("软件"), exif::Tag::Software),
    ];
    for (label, tag) in tags {
        if let Some(field) = exif.get_field(*tag, exif::In::PRIMARY) {
            let v = exif_value(&field.value);
            if !v.is_empty() {
                out.push(((*label).into(), v));
            }
        }
    }
    out
}

fn ratio_text(n: f64, d: f64) -> String {
    if d == 0.0 {
        return "—".into();
    }
    let v = n / d;
    if v.abs() >= 1.0 {
        format!("{v:.1}")
    } else if v != 0.0 {
        format!("1/{:.0}", 1.0 / v)
    } else {
        "0".into()
    }
}

fn exif_value(v: &exif::Value) -> String {
    match v {
        exif::Value::Ascii(a) => a
            .first()
            .map(|b| String::from_utf8_lossy(b).trim().to_string())
            .unwrap_or_default(),
        exif::Value::Byte(a) => a.first().map(|x| x.to_string()).unwrap_or_default(),
        exif::Value::Short(a) => a.first().map(|x| x.to_string()).unwrap_or_default(),
        exif::Value::Long(a) => a.first().map(|x| x.to_string()).unwrap_or_default(),
        exif::Value::Rational(a) => a
            .first()
            .map(|x| ratio_text(x.num as f64, x.denom as f64))
            .unwrap_or_default(),
        exif::Value::SRational(a) => a
            .first()
            .map(|x| ratio_text(x.num as f64, x.denom as f64))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// 工具格草稿的参数表。滑杆统一 0..100,但颗粒算子要的是 0..1 的比例。
pub(super) fn editor_params(name: &str, amount: f32) -> BTreeMap<String, String> {
    let mut p = BTreeMap::new();
    let v = if name == "grain" {
        (amount / 100.0).clamp(0.0, 1.0)
    } else {
        amount
    };
    p.insert("amount".into(), format!("{v:.4}"));
    p
}

/// 效果清单里显示中文名(存库仍用内部 id)。
fn effect_label(name: &str) -> &str {
    match name {
        "enhance" => t("亮度 / 对比度调整"),
        "curves" => t("色调曲线"),
        "crop" => t("裁剪"),
        "rot" => t("旋转"),
        "bw" => t("黑白"),
        "sepia" => t("棕褐色"),
        "grain" => t("颗粒"),
        "unsharp" | "unsharp2" => t("锐化"),
        "warm" => t("暖调"),
        "cool" => t("冷调"),
        "vignette" => t("暗角"),
        "faded" => t("褪色"),
        "tilt" => t("拉直"),
        "shadow" => t("暗部高光"),
        "fill" | "backlight" => t("补光"),
        "autocontrast" => t("自动对比度"),
        "enhance_auto" => t("手气不错"),
        "autocolor" => t("自动调色"),
        _ => name,
    }
}

fn strength_label(name: &str) -> &'static str {
    match name {
        "tilt" => t("扶正角度"),
        "fill" => t("补光强度"),
        "unsharp" => t("锐化强度"),
        "vignette" => t("暗角强度"),
        "shadow" => t("修复强度"),
        "cool" => t("冷调强度"),
        "warm" => t("暖调强度"),
        "faded" => t("褪色强度"),
        "grain" => t("颗粒强度"),
        _ => t("强度"),
    }
}

impl App {
    /// 整页编辑视图。预览打开期间由 `update` 在常规面板之前接管。
    pub(super) fn editor_ui(&mut self, ctx: &egui::Context) {
        let Some(mut id) = self.preview else {
            return;
        };
        if !self.id_row.contains_key(&id) {
            self.editor_back_to_library();
            return;
        }
        // 换照片:清掉未提交的工具草稿 / 一次性缓存,避免上一张的效果跟着走。
        // 换图入口(顶栏 / 胶片带 / 上下张)也即时调同一套同步,不留一帧串味。
        self.editor_ensure_photo(id);
        if self.zoom_active_for != Some(id) {
            self.preview_reset_zoom();
            self.zoom_active_for = Some(id);
        }
        self.editor_pump_zoom(ctx, id);
        self.editor_ensure_exif(id);
        let ids = self.view_ids();
        id = self.editor_header(ctx, id, &ids);
        // 顶栏点了「返回到图片库」时本帧就不再画编辑视图的其余面板。
        if self.preview.is_none() {
            return;
        }
        if self.editor_film {
            self.editor_film_ui(ctx, id, &ids);
        }
        if self.editor_props {
            self.editor_props_ui(ctx, id);
        }
        self.editor_tools_ui(ctx, id);
        if self.preview.is_none() {
            return;
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(crate::theme::p().bg))
            .show(ctx, |ui| {
                self.editor_center_ui(ui, ctx, id);
            });
        // 预览纹理还没解出来 / 并排对比时,缩放命令没被中间图区消费,
        // 帧末丢弃,免得切回 A 模式时突然跳一下。
        self.editor_zoom_cmd = 0;
    }

    /// 换到某张照片时立刻重置全部未提交草稿与一次性缓存。
    fn editor_ensure_photo(&mut self, id: i64) {
        if self.editor_for == Some(id) {
            self.editor_ensure_drafts(id);
            return;
        }
        self.editor_for = Some(id);
        self.editor_tool = None;
        self.fx_oneclick = None;
        self.fx_recipe = None;
        self.fx_draft = crate::effects::Adjust::default();
        self.fx_draft_id = None;
        self.fx_curve_draft = crate::curves::Curves::new();
        self.fx_curve_draft_id = None;
        self.fx_applied_sig = None;
        self.editor_orig_tex = None;
        self.editor_hist = None;
        self.editor_exif = None;
        self.crop_rect = None;
        self.crop_drag = None;
        self.editor_zoom_cmd = 0;
        self.editor_ensure_drafts(id);
    }

    /// 两条常驻草稿(调整 / 曲线)按当前照片的已存效果栈载入。
    /// 曲线草稿也在换图时统一重载。
    fn editor_ensure_drafts(&mut self, id: i64) {
        if self.fx_draft_id != Some(id) {
            self.fx_draft_id = Some(id);
            let stored = self.fx_stack_for(id);
            self.fx_draft = stored.merged_adjust();
        }
        if self.fx_curve_draft_id != Some(id) {
            self.fx_curve_draft_id = Some(id);
            let stored = self.fx_stack_for(id);
            self.fx_curve_draft = stored
                .items
                .iter()
                .find(|it| it.name.eq_ignore_ascii_case("curves"))
                .map(|it| it.curve_set())
                .unwrap_or_default();
        }
    }

    /// 切到指定照片(顶栏邻片小图 / 底部胶片带共用)。
    fn editor_switch_to(&mut self, nid: i64) -> i64 {
        if self.preview != Some(nid) {
            self.preview = Some(nid);
            self.preview_tex = None;
            self.preview_failed = None;
            self.preview_missing = None;
            self.anim_reset();
            self.preview_reset_zoom();
            self.editor_ensure_photo(nid);
        }
        nid
    }

    /// 上一张 / 下一张:切完立刻同步草稿。
    fn editor_step(&mut self, delta: i32) -> i64 {
        self.step_preview(delta);
        let id = self.preview.unwrap_or_default();
        self.editor_ensure_photo(id);
        id
    }

    pub(super) fn editor_back_to_library(&mut self) {
        self.preview = None;
        self.preview_tex = None;
        self.preview_base = None;
        self.anim_reset();
        self.preview_reset_zoom();
        self.zoom_active_for = None;
        self.editor_tool = None;
        self.editor_for = None;
        self.editor_thumbs.clear();
        self.editor_orig_tex = None;
        self.editor_hist = None;
        self.editor_exif = None;
        self.fx_oneclick = None;
        self.fx_recipe = None;
        self.fx_applied_sig = None;
        self.fx_draft = crate::effects::Adjust::default();
        self.fx_draft_id = None;
        self.fx_curve_draft = crate::curves::Curves::new();
        self.fx_curve_draft_id = None;
        self.editor_zoom_cmd = 0;
        self.fx_msg.clear();
    }

    /// 收取放大态的全图解码结果(放大超过预览档清晰度时按需解码)。
    fn editor_pump_zoom(&mut self, ctx: &egui::Context, id: i64) {
        if let Some(rx) = &self.zoom_rx {
            match rx.try_recv() {
                Ok((rid, Some(img))) => {
                    if Some(rid) == self.preview {
                        self.preview_full = Some((rid, img));
                        self.zoom_tex = None;
                        self.zoom_failed_for = None;
                    }
                    self.zoom_rx = None;
                    self.zoom_pending = None;
                }
                Ok((rid, None)) => {
                    self.zoom_rx = None;
                    self.zoom_pending = None;
                    // 记下失败照片,避免每帧重发一次全图解码线程。
                    self.zoom_failed_for = Some(rid);
                    self.fx_msg = t("原图过大或解码失败,放大显示停留在预览档清晰度").into();
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.zoom_failed_for = Some(self.zoom_pending.unwrap_or(id));
                    self.zoom_rx = None;
                    self.zoom_pending = None;
                }
            }
            let _ = (ctx, id);
        }
    }

    fn editor_ensure_exif(&mut self, id: i64) {
        if self.editor_exif.as_ref().map(|(k, _)| *k) == Some(id) {
            return;
        }
        let path = self.photo_path(id).unwrap_or_default();
        self.editor_exif = Some((id, exif_rows(Path::new(&path))));
    }

    /// 顶栏:一行菜单 + 一行导航(返回图库 / 一键修复 / 播放 / 上下张 /
    /// 前后邻片小图 / A-B-AA / 缩放 / 旋转 / 面板开关)。
    fn editor_header(&mut self, ctx: &egui::Context, mut id: i64, ids: &[i64]) -> i64 {
        let readonly = self.readonly_media(id);
        egui::TopBottomPanel::top("editor_header")
            .exact_height(HEADER_H)
            .resizable(false)
            .frame(crate::theme::bar_frame())
            .show(ctx, |ui| {
                egui::MenuBar::new().ui(ui, |ui| {
                    ui.label(
                        egui::RichText::new(t("马良 · 编辑照片"))
                            .strong()
                            .color(crate::theme::p().accent),
                    );
                    ui.separator();
                    ui.menu_button(t("文件(F)"), |ui| {
                        if ui
                            .add_enabled(
                                !readonly,
                                egui::Button::new(t("保存(烘培效果到原图)  F2")),
                            )
                            .clicked()
                        {
                            self.start_bake(id);
                            ui.close();
                        }
                        if ui.button(t("在资源管理器中显示")).clicked() {
                            if let Some(p) = self.photo_path(id) {
                                super::show_in_explorer(&p);
                            }
                            ui.close();
                        }
                        ui.separator();
                        if ui.button(t("返回到图片库  Esc")).clicked() {
                            self.editor_back_to_library();
                            ui.close();
                        }
                    });
                    ui.menu_button(t("编辑(E)"), |ui| {
                        if ui.button(t("撤销  Ctrl+Z")).clicked() {
                            self.fx_undo_step(id);
                            ui.close();
                        }
                        if ui.button(t("重做  Ctrl+Y")).clicked() {
                            self.fx_redo_step(id);
                            ui.close();
                        }
                        ui.separator();
                        if ui.button(t("复制所有效果")).clicked() {
                            self.fx_copy_all(id);
                            ui.close();
                        }
                        if ui.button(t("粘贴效果到这张")).clicked() {
                            self.fx_paste_all(&[id]);
                            ui.close();
                        }
                        if ui.button(t("清除这张的所有效果")).clicked() {
                            self.editor_clear_effects(id);
                            ui.close();
                        }
                    });
                    ui.menu_button(t("视图(V)"), |ui| {
                        if ui.button(t("▶ 幻灯片放映")).clicked() {
                            self.start_slideshow();
                            ui.close();
                        }
                        if ui.checkbox(&mut self.editor_props, t("属性面板")).clicked() {
                            ui.close();
                        }
                        if ui
                            .checkbox(&mut self.editor_film, t("底部胶片带"))
                            .clicked()
                        {
                            ui.close();
                        }
                        ui.separator();
                        if ui.button(t("选项…")).clicked() {
                            self.show_options = true;
                            ui.close();
                        }
                    });
                    ui.menu_button(t("图片(P)"), |ui| {
                        if ui.button(t("◀ 上一张")).clicked() {
                            id = self.editor_step(-1);
                            ui.close();
                        }
                        if ui.button(t("下一张 ▶")).clicked() {
                            id = self.editor_step(1);
                            ui.close();
                        }
                        ui.separator();
                        if ui
                            .add_enabled(
                                !readonly,
                                egui::Button::new(t("↺ 向左旋转  Ctrl+Shift+R")),
                            )
                            .clicked()
                        {
                            self.preview_rotate(id, 3);
                            ui.close();
                        }
                        if ui
                            .add_enabled(!readonly, egui::Button::new(t("↻ 向右旋转  Ctrl+R")))
                            .clicked()
                        {
                            self.preview_rotate(id, 1);
                            ui.close();
                        }
                        if ui.button(t("☆ 切换星标")).clicked() {
                            self.toggle_star(id);
                            ui.close();
                        }
                    });
                });
                ui.add_space(3.0);
                // 动作行:纯图标,悬停显示操作说明(与图库/导入页同一套)。
                ui.horizontal(|ui| {
                    if super::icon_action(ui, "arrow-left", &t("返回到图片库  Esc"), true) {
                        self.editor_back_to_library();
                    }
                    ui.separator();
                    if super::icon_action(ui, "wand-sparkles", &t("手气不错"), true) {
                        self.fx_commit_effect(
                            id,
                            t("手气不错"),
                            crate::effects::Effect::oneclick("enhance_auto", BTreeMap::new()),
                        );
                    }
                    if super::icon_action(ui, "play", &t("幻灯片放映"), true) {
                        self.start_slideshow();
                    }
                    ui.add_space(6.0);
                    if super::icon_action(ui, "arrow-left", &t("◀ 上一张"), true) {
                        id = self.editor_step(-1);
                    }
                    if !ids.is_empty() {
                        let pos = ids.iter().position(|&x| x == id).unwrap_or(0);
                        let n = ids.len() as i32;
                        for k in -2i32..=2 {
                            let j = (pos as i32 + k).rem_euclid(n) as usize;
                            let nid = ids[j];
                            let tex = self.editor_thumb(ctx, nid);
                            let r = film_tile(
                                ui,
                                tex,
                                nid == id,
                                egui::vec2(if k == 0 { 30.0 } else { 24.0 }, 26.0),
                            );
                            if r.clicked() && nid != id {
                                id = self.editor_switch_to(nid);
                            }
                        }
                    }
                    if super::icon_action(ui, "arrow-right", &t("下一张 ▶"), true) {
                        id = self.editor_step(1);
                    }
                    ui.add_space(6.0);
                    for (n, name) in [(0u8, "A"), (1u8, "B"), (2u8, "AA")] {
                        if ui
                            .selectable_label(self.editor_compare == n, name)
                            .on_hover_text(match n {
                                0 => t("显示效果(A)"),
                                1 => t("显示原图(B)"),
                                _ => t("原图 / 效果并排(AA)"),
                            })
                            .clicked()
                        {
                            self.editor_compare = n;
                        }
                    }
                    ui.separator();
                    if super::icon_action(ui, "zoom-out", &t("缩小"), true) {
                        self.editor_zoom_cmd = 4;
                    }
                    if super::icon_action(ui, "scan", &t("实际大小(100%)"), true) {
                        self.editor_zoom_cmd = 1;
                    }
                    if super::icon_action(ui, "zoom-in", &t("放大"), true) {
                        self.editor_zoom_cmd = 3;
                    }
                    if super::icon_action(ui, "maximize", &t("适应窗口"), true) {
                        self.editor_zoom_cmd = 2;
                    }
                    ui.small(format!("{:.0}%", self.preview_zoom * 100.0));
                    ui.separator();
                    ui.add_enabled_ui(!readonly, |ui| {
                        if super::icon_action(
                            ui,
                            "rotate-ccw",
                            &t("↺ 向左旋转  Ctrl+Shift+R"),
                            true,
                        ) {
                            self.preview_rotate(id, 3);
                        }
                        if super::icon_action(ui, "rotate-cw", &t("↻ 向右旋转  Ctrl+R"), true)
                        {
                            self.preview_rotate(id, 1);
                        }
                    });
                    ui.separator();
                    // 动图逐帧播放:只在帧序列解出来后出现,暂停时停在当前帧。
                    if let Some(status) = self.anim_status(id) {
                        let playing = self.anim_playing(id);
                        let tip = if playing {
                            t("⏸ 暂停动图")
                        } else {
                            t("▶ 播放动图")
                        };
                        if super::icon_toggle(
                            ui,
                            if playing { "pause" } else { "play" },
                            &tip,
                            playing,
                        ) {
                            self.anim_toggle();
                        }
                        ui.small(status);
                        ui.separator();
                    }
                    if super::icon_toggle(ui, "panel-right", &t("属性面板"), self.editor_props)
                    {
                        self.editor_props = !self.editor_props;
                    }
                    if super::icon_toggle(ui, "film", &t("底部胶片带"), self.editor_film) {
                        self.editor_film = !self.editor_film;
                    }
                });
            });
        id
    }

    /// 底部:蓝色信息条 + 前后各 4 张的胶片带(点击即切到那张)。
    fn editor_film_ui(&mut self, ctx: &egui::Context, mut id: i64, ids: &[i64]) {
        let t = crate::theme::p();
        egui::TopBottomPanel::bottom("editor_film")
            .exact_height(FILM_H)
            .resizable(false)
            .frame(crate::theme::bar_frame())
            .show(ctx, |ui| {
                let pos = ids.iter().position(|&x| x == id).unwrap_or(0);
                let info = self
                    .id_row
                    .get(&id)
                    .and_then(|&i| self.rows.get(i))
                    .map(|r| {
                        let name = Path::new(&r.path)
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| r.path.clone());
                        let dims = match (r.width, r.height) {
                            (Some(w), Some(h)) => format!("{w}×{h}"),
                            _ => "—".into(),
                        };
                        t_with(
                            "{p1}    {p2}    {p3}    {p4}    第 {p5} 个,共 {p6} 个",
                            &[
                                ("p1", (name).to_string()),
                                ("p2", (r.taken.as_deref().unwrap_or("")).to_string()),
                                ("p3", (dims).to_string()),
                                ("p4", (super::fmt_size(r.size as u64)).to_string()),
                                ("p5", (pos + 1).to_string()),
                                ("p6", (ids.len()).to_string()),
                            ],
                        )
                    });
                egui::Frame::new()
                    .fill(t.accent)
                    .inner_margin(egui::Margin {
                        left: 8,
                        right: 8,
                        top: 3,
                        bottom: 3,
                    })
                    .corner_radius(3.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if let Some(s) = info {
                                ui.colored_label(egui::Color32::WHITE, s);
                            }
                            let msg = if self.fx_msg.is_empty() {
                                self.status.clone()
                            } else {
                                self.fx_msg.clone()
                            };
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.colored_label(egui::Color32::WHITE, msg);
                                },
                            );
                        });
                    });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ids.is_empty() {
                        ui.small(crate::i18n::t("当前视图没有照片"));
                        return;
                    }
                    let n = ids.len() as i32;
                    // 9 张小图居中排(当前张略大),左右留白。
                    let strip_w = 74.0 + 8.0 * 62.0 + 8.0 * 4.0;
                    ui.add_space(((ui.available_width() - strip_w) * 0.5).max(0.0));
                    for k in -4i32..=4 {
                        let j = (pos as i32 + k).rem_euclid(n) as usize;
                        let nid = ids[j];
                        let tex = self.editor_thumb(ctx, nid);
                        let r = film_tile(
                            ui,
                            tex,
                            nid == id,
                            egui::vec2(if k == 0 { 74.0 } else { 62.0 }, 68.0),
                        );
                        if r.clicked() && nid != id {
                            id = self.editor_switch_to(nid);
                        }
                    }
                });
            });
    }

    /// 邻片缩略图纹理(优先小档,回退中档/预览档;按 id 缓存)。
    fn editor_thumb(&mut self, ctx: &egui::Context, id: i64) -> Option<egui::TextureHandle> {
        if let Some(t) = self.editor_thumbs.get(&id) {
            return Some(t.clone());
        }
        let t = super::thumb_tex(ctx, &self.store, id, crate::thumbs::Tier::Small)
            .or_else(|| super::thumb_tex(ctx, &self.store, id, crate::thumbs::Tier::Normal))
            .or_else(|| super::thumb_tex(ctx, &self.store, id, crate::thumbs::Tier::Preview))?;
        if self.editor_thumbs.len() >= THUMB_CAP {
            self.editor_thumbs.clear();
        }
        self.editor_thumbs.insert(id, t.clone());
        Some(t)
    }

    /// 右栏常驻属性面板:标题栏 + 属性清单 + 底部页签。
    fn editor_props_ui(&mut self, ctx: &egui::Context, id: i64) {
        egui::SidePanel::right("editor_props")
            .exact_width(RIGHT_W)
            .resizable(false)
            .frame(crate::theme::side_frame())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(t("属性")).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("×").clicked() {
                            self.editor_props = false;
                        }
                        if ui
                            .small_button("«")
                            .on_hover_text(t("收起属性面板"))
                            .clicked()
                        {
                            self.editor_props = false;
                        }
                    });
                });
                ui.separator();
                ui.horizontal(|ui| {
                    for (i, (g, l)) in [
                        ("ⓘ", t("属性")),
                        ("✎", t("标题")),
                        ("⚑", t("标签")),
                        ("✦", t("效果")),
                    ]
                    .iter()
                    .enumerate()
                    {
                        if ui
                            .selectable_label(self.editor_props_tab == i as u8, format!("{g} {l}"))
                            .clicked()
                        {
                            self.editor_props_tab = i as u8;
                        }
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| match self.editor_props_tab {
                        0 => {
                            self.props_detail_ui(ui, id);
                            ui.separator();
                            crate::theme::section_title(ui, t("相机信息 / EXIF"));
                            let rows = self
                                .editor_exif
                                .as_ref()
                                .map(|(_, v)| v.clone())
                                .unwrap_or_default();
                            if rows.is_empty() {
                                ui.small(t("没有可用的 EXIF 数据。"));
                            } else {
                                egui::Grid::new("editor_props_exif")
                                    .num_columns(2)
                                    .spacing(egui::vec2(8.0, 2.0))
                                    .show(ui, |ui| {
                                        for (k, v) in rows {
                                            ui.small(k);
                                            ui.small(v);
                                            ui.end_row();
                                        }
                                    });
                            }
                        }
                        1 => {
                            crate::theme::section_title(ui, t("照片说明"));
                            self.caption_editor(ui, id);
                            ui.add_space(8.0);
                            if ui
                                .button(if self.star_of(id) != 0 {
                                    t("★ 取消星标")
                                } else {
                                    t("☆ 加星")
                                })
                                .clicked()
                            {
                                self.toggle_star(id);
                            }
                        }
                        2 => self.tags_editor(ui, id),
                        _ => self.editor_effect_list_ui(ui, id),
                    });
            });
    }

    /// 左栏:分类标签 + 工具网格(上半可滚动),下半固定撤销/重做与
    /// 直方图 / 相机信息。
    fn editor_tools_ui(&mut self, ctx: &egui::Context, id: i64) {
        let t = crate::theme::p();
        // 只读速看格式:整栏工具灰掉,并说明为什么不参与编辑。
        let readonly = self.readonly_media(id);
        let hint = self.readonly_hint(id);
        egui::SidePanel::left("editor_tools")
            .exact_width(LEFT_W)
            .resizable(false)
            .frame(crate::theme::side_frame())
            .show(ctx, |ui| {
                // 「返回到图片库」按钮在顶栏导航行已有,左栏不再重复。
                ui.add_space(4.0);
                egui::TopBottomPanel::bottom("editor_tools_bottom")
                    .exact_height(LEFT_BOTTOM_H)
                    .resizable(false)
                    .frame(
                        egui::Frame::new()
                            .fill(t.surface)
                            .inner_margin(egui::Margin {
                                left: 0,
                                right: 6,
                                top: 6,
                                bottom: 2,
                            }),
                    )
                    .show_inside(ui, |ui| self.editor_status_ui(ui, id));
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        if readonly {
                            if let Some(hint) = hint {
                                let c = ui.visuals().warn_fg_color;
                                ui.add_space(4.0);
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(egui::RichText::new(hint).size(10.5).color(c));
                                });
                                ui.add_space(6.0);
                            }
                            ui.disable();
                        }
                        self.editor_tabs_ui(ui);
                        ui.separator();
                        crate::theme::section_title(ui, self.editor_tab.label());
                        self.editor_tab_body(ui, id);
                        ui.add_space(4.0);
                        self.editor_tool_strength(ui, id);
                    });
            });
    }

    fn editor_tabs_ui(&mut self, ui: &mut egui::Ui) {
        let t = crate::theme::p();
        let w = ((ui.available_width() - 8.0) / 3.0).max(64.0);
        egui::Grid::new("editor_tabs")
            .num_columns(3)
            .spacing(egui::vec2(4.0, 4.0))
            .show(ui, |ui| {
                for (i, tab) in EditorTab::ALL.iter().enumerate() {
                    let on = self.editor_tab == *tab;
                    let b = egui::Button::new(
                        egui::RichText::new(format!("{} {}", tab.glyph(), tab.label())).size(11.0),
                    )
                    .min_size(egui::vec2(w, 26.0))
                    .fill(if on { t.accent_soft } else { t.widget_bg });
                    if ui.add(b).clicked() {
                        self.editor_tab = *tab;
                    }
                    if i % 3 == 2 {
                        ui.end_row();
                    }
                }
            });
    }

    fn editor_tab_body(&mut self, ui: &mut egui::Ui, id: i64) {
        match self.editor_tab {
            EditorTab::Basic => {
                if let Some(act) = tool_grid(ui, &basic_tools(), self.editor_tool) {
                    self.editor_act(act, id);
                }
                ui.add_space(4.0);
                ui.small(t("点一下即作用到这张照片;带强度的会先实时预览。"));
            }
            EditorTab::Tuning => self.editor_tuning_ui(ui, id),
            EditorTab::Effects => {
                self.editor_tool = None;
                self.fx_oneclick_ui(ui, id);
            }
            EditorTab::Curves => self.fx_curves_ui(ui, id),
            EditorTab::Crop => self.editor_crop_ui(ui, id),
            EditorTab::Info => self.editor_info_ui(ui, id),
        }
    }

    fn editor_act(&mut self, act: Act, id: i64) {
        match act {
            Act::Effect(name, label) => {
                self.fx_commit_effect(
                    id,
                    label,
                    crate::effects::Effect::oneclick(name, BTreeMap::new()),
                );
            }
            Act::Strength(name, default) => {
                self.fx_oneclick = None;
                self.fx_recipe = None;
                self.editor_tool = Some(name);
                self.editor_tool_amount = default;
                self.fx_applied_sig = None;
            }
            Act::Page(tab) => {
                self.editor_tab = tab;
                if tab == EditorTab::Crop {
                    self.crop_mode = true;
                    self.crop_rect = None;
                    self.crop_drag = None;
                }
            }
            Act::Rotate(q) => self.preview_rotate(id, q),
            Act::Clear => self.editor_clear_effects(id),
        }
    }

    /// 强度滑杆:改的是预览栈里的草稿,点「应用」才写库。
    fn editor_tool_strength(&mut self, ui: &mut egui::Ui, id: i64) {
        let Some(name) = self.editor_tool else {
            return;
        };
        ui.separator();
        crate::theme::section_title(ui, t("工具强度(实时预览)"));
        if ui
            .add(
                egui::Slider::new(&mut self.editor_tool_amount, 0.0..=100.0)
                    .text(strength_label(name)),
            )
            .changed()
        {
            self.fx_applied_sig = None;
        }
        ui.horizontal(|ui| {
            if super::icon_action(ui, "check", &t("应用"), true) {
                let params = editor_params(name, self.editor_tool_amount);
                self.fx_commit_effect(
                    id,
                    strength_label(name),
                    crate::effects::Effect::oneclick(name, params),
                );
                self.editor_tool = None;
                self.fx_applied_sig = None;
            }
            if super::icon_action(ui, "x", &t("取消"), true) {
                self.editor_tool = None;
                self.fx_applied_sig = None;
            }
        });
    }

    fn editor_tuning_ui(&mut self, ui: &mut egui::Ui, id: i64) {
        ui.small(t("亮度 / 对比度 / 饱和度 / 曝光 / 暗部 / 锐化 / 模糊"));
        let mut d = self.fx_draft;
        let mut changed = false;
        changed |= ui
            .add(egui::Slider::new(&mut d.brightness, -100.0..=100.0).text(t("亮度")))
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut d.contrast, -100.0..=100.0).text(t("对比度")))
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut d.saturation, -100.0..=100.0).text(t("饱和度")))
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut d.exposure, -3.0..=3.0).text(t("曝光(档)")))
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut d.blacks, -100.0..=100.0).text(t("暗部")))
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut d.sharpen, 0.0..=100.0).text(t("锐化")))
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut d.blur, 0.0..=20.0).text(t("模糊")))
            .changed();
        if changed {
            self.fx_draft = d;
            self.fx_applied_sig = None;
        }
        ui.horizontal(|ui| {
            if super::icon_action(ui, "check", &t("应用到这张"), true) {
                self.fx_apply_draft(&[id]);
            }
            if super::icon_action(ui, "rotate-ccw", &t("重置"), true) {
                self.fx_draft = crate::effects::Adjust::default();
                self.fx_applied_sig = None;
            }
        });
    }

    /// 已应用的效果项清单 + 效果栈动作(属性面板与信息页共用)。
    fn editor_effect_list_ui(&mut self, ui: &mut egui::Ui, id: i64) {
        let st = self.fx_stack_for(id);
        if st.is_empty() {
            ui.small(t("这张照片还没有效果。"));
        } else {
            for it in &st.items {
                let tag = match crate::effects::support_of(&it.name) {
                    crate::effects::Support::ReferenceOnly => t("暂不支持"),
                    crate::effects::Support::Unknown => t("未知名"),
                    crate::effects::Support::Pixel => {
                        if it.is_oneclick() {
                            t("一键")
                        } else {
                            t("调整")
                        }
                    }
                };
                ui.small(format!("· {} [{}]", effect_label(&it.name), tag));
            }
        }
        ui.separator();
        ui.horizontal(|ui| {
            if super::icon_action(ui, "copy", &t("复制所有效果"), true) {
                self.fx_copy_all(id);
            }
            if super::icon_action(ui, "clipboard-paste", &t("粘贴到这张"), true) {
                self.fx_paste_all(&[id]);
            }
            if super::icon_action(ui, "trash-2", &t("清除这张的所有效果"), true) {
                self.editor_clear_effects(id);
            }
            if super::icon_action(ui, "save", &t("保存(烘培效果到原图)"), true) {
                self.start_bake(id);
            }
        });
        ui.small(t("保存会先备份原稿,再写回带效果的原图。"));
    }

    /// 裁剪页:比例预设 + 自定义比例;框在中间图区拖出来。
    fn editor_crop_ui(&mut self, ui: &mut egui::Ui, id: i64) {
        ui.small(t("在中间照片上拖出裁剪框;应用后写进效果栈。"));
        if ui
            .selectable_label(self.crop_mode, t("✂ 裁剪工具"))
            .clicked()
        {
            self.crop_mode = !self.crop_mode;
            self.crop_rect = None;
            self.crop_drag = None;
        }
        let (tw, th) = self
            .preview_base
            .as_ref()
            .map(|(_, img)| (img.width() as f32, img.height() as f32))
            .unwrap_or((4.0, 3.0));
        let mut presets: Vec<(String, Option<(f32, f32)>)> = vec![
            (t("自由").into(), None),
            (t("原比例").into(), Some((tw, th))),
            (t("4 : 3(屏幕)").into(), Some((4.0, 3.0))),
            (t("3 : 2(打印)").into(), Some((3.0, 2.0))),
            (t("16 : 9(电影)").into(), Some((16.0, 9.0))),
            ("16 : 10".into(), Some((16.0, 10.0))),
            (t("1 : 1(方形)").into(), Some((1.0, 1.0))),
            ("4 : 6".into(), Some((4.0, 6.0))),
            ("5 : 7".into(), Some((5.0, 7.0))),
            ("8 : 10".into(), Some((8.0, 10.0))),
            ("10 : 15".into(), Some((10.0, 15.0))),
            ("9 : 13".into(), Some((9.0, 13.0))),
        ];
        for (n, w, h) in self.custom_ratios.clone() {
            presets.push((format!("{n}({w:.0}×{h:.0})"), Some((w, h))));
        }
        let cur = match self.crop_ratio {
            None => t("自由").to_string(),
            Some((w, h)) => format!("{w:.0} : {h:.0}"),
        };
        ui.horizontal(|ui| {
            ui.label(t("比例"));
            egui::ComboBox::from_id_salt("editor_crop_ratio")
                .selected_text(cur)
                .show_ui(ui, |ui| {
                    for (label, r) in &presets {
                        if ui.selectable_label(self.crop_ratio == *r, label).clicked() {
                            self.crop_ratio = *r;
                        }
                    }
                });
        });
        ui.horizontal(|ui| {
            if let Some((w, h)) = self.crop_ratio {
                if super::icon_action(ui, "arrow-left-right", &t("⇄ 对调"), true) {
                    self.crop_ratio = Some((h, w));
                }
            }
            if super::icon_action(ui, "check", &t("应用裁剪"), true) {
                self.commit_crop(id);
            }
            if super::icon_action(ui, "x", &t("取消"), true) {
                self.crop_mode = false;
                self.crop_rect = None;
                self.crop_drag = None;
            }
        });
        ui.label(t("自定义比例"));
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.crop_cw).desired_width(36.0));
            ui.label("×");
            ui.add(egui::TextEdit::singleline(&mut self.crop_ch).desired_width(36.0));
            ui.add(
                egui::TextEdit::singleline(&mut self.crop_cname)
                    .hint_text(t("名称"))
                    .desired_width(80.0),
            );
        });
        if super::icon_action(ui, "save", &t("保存为预设"), true) {
            if let (Ok(w), Ok(h)) = (
                self.crop_cw.trim().parse::<f32>(),
                self.crop_ch.trim().parse::<f32>(),
            ) {
                if w > 0.0 && h > 0.0 {
                    let name = if self.crop_cname.trim().is_empty() {
                        format!("{w:.0}:{h:.0}")
                    } else {
                        self.crop_cname.trim().to_string()
                    };
                    self.custom_ratios.push((name, w, h));
                    self.save_custom_ratios();
                    self.crop_cw.clear();
                    self.crop_ch.clear();
                    self.crop_cname.clear();
                }
            }
        }
    }

    fn editor_info_ui(&mut self, ui: &mut egui::Ui, id: i64) {
        crate::theme::section_title(ui, t("照片说明"));
        self.caption_editor(ui, id);
        ui.add_space(4.0);
        crate::theme::section_title(ui, t("标签"));
        self.tags_editor(ui, id);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let starred = self.star_of(id) != 0;
            let tip = if starred {
                t("★ 取消星标")
            } else {
                t("☆ 加星")
            };
            if super::icon_toggle(ui, "star", &tip, starred) {
                self.toggle_star(id);
            }
            if super::icon_action(ui, "folder-open", &t("在资源管理器中显示"), true) {
                if let Some(p) = self.photo_path(id) {
                    super::show_in_explorer(&p);
                }
            }
        });
        // 视频的时间区间裁剪只在视频上出现。
        self.fx_trim_ui(ui, id);
        ui.separator();
        self.editor_effect_list_ui(ui, id);
    }

    /// 左栏底部固定区:撤销 / 重做 + 直方图 + 相机信息。
    fn editor_status_ui(&mut self, ui: &mut egui::Ui, id: i64) {
        ui.horizontal(|ui| {
            let can_undo = self.fx_undo.get(&id).map(|s| s.can_undo()).unwrap_or(false);
            let can_redo = self.fx_undo.get(&id).map(|s| s.can_redo()).unwrap_or(false);
            if super::icon_action(ui, "undo-2", &t("↶ 撤销"), can_undo) {
                self.fx_undo_step(id);
            }
            if super::icon_action(ui, "redo-2", &t("↷ 重做"), can_redo) {
                self.fx_redo_step(id);
            }
        });
        ui.add_space(4.0);
        crate::theme::section_title(ui, t("直方图和相机信息"));
        match self.editor_hist.clone() {
            Some((hid, _, h)) if hid == id => draw_hist(ui, &h),
            _ => {
                ui.small(t("直方图计算中…"));
            }
        }
        let rows = self
            .editor_exif
            .as_ref()
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        egui::CollapsingHeader::new("EXIF")
            .default_open(true)
            .show(ui, |ui| {
                if rows.is_empty() {
                    ui.small(t("没有可用的 EXIF 数据。"));
                    return;
                }
                egui::ScrollArea::vertical()
                    .max_height(92.0)
                    .show(ui, |ui| {
                        egui::Grid::new("editor_status_exif")
                            .num_columns(2)
                            .spacing(egui::vec2(8.0, 2.0))
                            .show(ui, |ui| {
                                for (k, v) in rows {
                                    ui.small(k);
                                    ui.small(v);
                                    ui.end_row();
                                }
                            });
                    });
            });
    }

    fn editor_clear_effects(&mut self, id: i64) {
        let before = self.fx_stack_for(id);
        if before.is_empty() {
            self.fx_msg = t("这张照片没有效果").into();
            return;
        }
        let after = crate::effects::FilterStack::new();
        if let Err(e) = self.store.set_filters(id, &after.serialize()) {
            self.fx_msg = t_with("写入失败: {e}", &[("e", e.to_string())]);
            return;
        }
        let stack = self
            .fx_undo
            .entry(id)
            .or_insert_with(crate::effects::EditStack::new);
        stack.commit(t("清除效果"), before, after);
        self.fx_applied_sig = None;
        self.fx_draft = crate::effects::Adjust::default();
        self.fx_draft_id = None;
        self.fx_curve_draft = crate::curves::Curves::new();
        self.fx_curve_draft_id = None;
        self.editor_tool = None;
        self.fx_oneclick = None;
        self.fx_recipe = None;
        self.fx_msg = t("已清除效果").into();
    }

    /// 未套效果栈的底图纹理(对比 B / 并排 AA 用)。
    fn editor_orig_tex(&mut self, ctx: &egui::Context, id: i64) -> Option<egui::TextureHandle> {
        let img = &self.preview_base.as_ref().filter(|(k, _)| *k == id)?.1;
        if let Some((k, t)) = &self.editor_orig_tex {
            // 底图被后台重生成时(尺寸变了)要重建,不能只按 id 命中旧缓存。
            if *k == id && t.size() == [img.width() as usize, img.height() as usize] {
                return Some(t.clone());
            }
        }
        let rgba = img.to_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
        let tex = ctx.load_texture(
            format!("preview-{id}-orig"),
            color,
            egui::TextureOptions::default(),
        );
        self.editor_orig_tex = Some((id, tex.clone()));
        Some(tex)
    }

    /// 中间图区:缩放/平移/双击、裁剪框、识别框、对比显示,底部一行照片说明。
    fn editor_center_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, id: i64) {
        let hint = self.readonly_hint(id);
        if let Some(hint) = hint {
            let c = ui.visuals().warn_fg_color;
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(hint).size(11.0).color(c));
            });
        }
        let Some(t) = self.preview_tex(ctx) else {
            if self.preview_missing == Some(id) {
                ui.label(t("原文件已不存在(可能被移动或删除),正在移出图库…"));
            } else if self.preview_failed == Some(id) {
                // 只读文档最常见的失败原因:没写合并合成图,机器上也没有系统缩略图。
                ui.label(if hint.is_some() {
                    t("(只读速看预览生成失败:该文档可能未写入合并合成图,系统也未提供缩略图)")
                } else {
                    t("(预览加载失败:格式不支持或解码出错)")
                });
            } else if crate::thumbs::preview_path(id).exists() {
                ui.label(t("(预览加载失败)"));
            } else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t("预览生成中…(后台解码并缓存)"));
                });
                ctx.request_repaint_after(std::time::Duration::from_millis(150));
            }
            return;
        };
        let [tw, th] = t.size();
        let avail = (ui.available_size() - egui::vec2(0.0, 34.0)).max(egui::vec2(240.0, 160.0));
        let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let orig = if self.editor_compare != 0 {
            self.editor_orig_tex(ctx, id)
        } else {
            None
        };
        if self.editor_compare == 2 {
            split_paint(&painter, rect, orig.as_ref(), &t, tw, th);
        } else {
            let cmd = self.editor_zoom_cmd;
            self.editor_zoom_cmd = 0;
            let mut to_100 = false;
            match cmd {
                1 => to_100 = true,
                2 => {
                    self.preview_zoom = 1.0;
                    self.preview_pan = egui::Vec2::ZERO;
                }
                3 => self.preview_zoom = (self.preview_zoom * 1.25).min(8.0),
                4 => self.preview_zoom = (self.preview_zoom / 1.25).max(1.0),
                _ => {}
            }
            let fit_scale = (rect.width() / tw.max(1) as f32)
                .min(rect.height() / th.max(1) as f32)
                .min(1.0);
            const ZOOM_MIN: f32 = 1.0;
            const ZOOM_MAX: f32 = 8.0;
            if to_100 {
                self.preview_zoom = (1.0 / fit_scale.max(1e-4)).min(ZOOM_MAX);
                self.preview_pan = egui::Vec2::ZERO;
            }
            if resp.hovered() {
                let sd = ui.input(|i| i.raw_scroll_delta.y);
                if sd != 0.0 {
                    let z0 = self.preview_zoom;
                    let z1 = (z0 * f32::exp(sd * 0.0022)).clamp(ZOOM_MIN, ZOOM_MAX);
                    if (z1 - z0).abs() > f32::EPSILON {
                        let c0 = egui::vec2(tw as f32 * fit_scale * z0, th as f32 * fit_scale * z0);
                        let img0 =
                            egui::Rect::from_center_size(rect.center() + self.preview_pan, c0);
                        if let Some(p) = resp.hover_pos() {
                            let uv = egui::vec2(
                                ((p.x - img0.min.x) / c0.x).clamp(0.0, 1.0),
                                ((p.y - img0.min.y) / c0.y).clamp(0.0, 1.0),
                            );
                            let c1 =
                                egui::vec2(tw as f32 * fit_scale * z1, th as f32 * fit_scale * z1);
                            self.preview_pan = p - uv * c1 + c1 * 0.5 - rect.center();
                        }
                        self.preview_zoom = z1;
                    }
                }
            }
            if !self.crop_mode && resp.dragged_by(egui::PointerButton::Primary) {
                self.preview_pan += resp.drag_delta();
            }
            if resp.double_clicked() {
                if self.preview_zoom > 1.01 {
                    self.preview_zoom = 1.0;
                    self.preview_pan = egui::Vec2::ZERO;
                } else {
                    self.preview_zoom = (1.0 / fit_scale.max(1e-4)).min(ZOOM_MAX);
                    self.preview_pan = egui::Vec2::ZERO;
                }
            }
            let base = fit_scale * self.preview_zoom;
            let content = egui::vec2(tw as f32 * base, th as f32 * base);
            let max_pan = ((content - rect.size()) * 0.5).max(egui::Vec2::ZERO);
            self.preview_pan = egui::Vec2::new(
                self.preview_pan.x.clamp(-max_pan.x, max_pan.x),
                self.preview_pan.y.clamp(-max_pan.y, max_pan.y),
            );
            let img_rect = egui::Rect::from_center_size(rect.center() + self.preview_pan, content);
            if self.crop_mode {
                if resp.drag_started() {
                    self.crop_drag = resp.interact_pointer_pos();
                    self.crop_rect = None;
                }
                if let Some(start) = self.crop_drag {
                    if let Some(p) = resp.interact_pointer_pos() {
                        let to_norm = |q: egui::Pos2| {
                            let d = q - img_rect.min;
                            let s = img_rect.size();
                            egui::vec2(
                                (d.x / s.x.max(1.0)).clamp(0.0, 1.0),
                                (d.y / s.y.max(1.0)).clamp(0.0, 1.0),
                            )
                        };
                        let a = to_norm(start);
                        let mut b = to_norm(p);
                        if let Some((rw, rh)) = self.crop_ratio {
                            let k = (rw / rh) * (th as f32 / tw as f32);
                            let mut dw = (b.y - a.y) * k;
                            dw = dw.clamp(-a.x, 1.0 - a.x);
                            let dh = (dw / k).clamp(-a.y, 1.0 - a.y);
                            dw = dh * k;
                            b = egui::vec2(a.x + dw, a.y + dh);
                        }
                        self.crop_rect = Some([a.x, a.y, b.x, b.y]);
                    }
                    if !resp.dragged() {
                        self.crop_drag = None;
                    }
                }
            }
            let need_full = self.preview_zoom > 1.35
                && self.editor_compare == 0
                && self.preview_full.as_ref().map(|(k, _)| *k) != Some(id)
                && self.zoom_failed_for != Some(id)
                && self.zoom_rx.is_none()
                && self.zoom_pending.is_none();
            if need_full {
                let (tx, rx) = std::sync::mpsc::channel();
                self.zoom_rx = Some(rx);
                self.zoom_pending = Some(id);
                let path = self.photo_path(id).unwrap_or_default();
                std::thread::spawn(move || {
                    let ok = image::ImageReader::open(&path)
                        .ok()
                        .and_then(|r| r.with_guessed_format().ok())
                        .and_then(|r| r.decode().ok())
                        .map(|d| {
                            crate::thumbs::apply_exif_rotation(
                                d,
                                crate::exif::read_orientation(Path::new(&path)),
                            )
                        })
                        .filter(|d| (d.width() as u64) * (d.height() as u64) <= 40_000_000)
                        .map(|d| d.to_rgba8());
                    let _ = tx.send((id, ok));
                });
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
            let use_full =
                self.preview_zoom > 1.35 && self.preview_full.as_ref().map(|(k, _)| *k) == Some(id);
            let paint = if self.editor_compare == 1 {
                // B 模式始终给未套效果栈的底图,放大态全图纹理不能盖过它。
                orig.clone().or_else(|| Some(t.clone()))
            } else if use_full {
                self.zoom_tex_get(ctx, id).or_else(|| Some(t.clone()))
            } else {
                Some(t.clone())
            };
            if let Some(pt) = paint {
                painter.image(
                    pt.id(),
                    img_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            let boxes = self.preview_boxes_for(id);
            if !boxes.is_empty() {
                let stroke = egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(255, 90, 90));
                for ((a, b, c, d), label) in &boxes {
                    let r = egui::Rect::from_min_max(
                        egui::pos2(
                            img_rect.min.x + a * img_rect.width(),
                            img_rect.min.y + b * img_rect.height(),
                        ),
                        egui::pos2(
                            img_rect.min.x + c * img_rect.width(),
                            img_rect.min.y + d * img_rect.height(),
                        ),
                    );
                    painter.rect_stroke(r, 0.0, stroke, egui::StrokeKind::Inside);
                    if !label.is_empty() {
                        let galley = painter.layout_no_wrap(
                            label.clone(),
                            egui::FontId::proportional(11.0),
                            egui::Color32::WHITE,
                        );
                        let bg =
                            egui::Rect::from_min_size(r.min, galley.size() + egui::vec2(4.0, 2.0));
                        painter.rect_filled(bg, 0.0, egui::Color32::from_black_alpha(170));
                        painter.galley(bg.min + egui::vec2(2.0, 1.0), galley, egui::Color32::WHITE);
                    }
                }
            }
            if self.crop_mode {
                let dim = egui::Color32::from_black_alpha(110);
                let inner = self.crop_rect.map(|c| {
                    let (x0, x1) = (c[0].min(c[2]), c[0].max(c[2]));
                    let (y0, y1) = (c[1].min(c[3]), c[1].max(c[3]));
                    egui::Rect::from_min_max(
                        img_rect.min + egui::vec2(x0 * img_rect.width(), y0 * img_rect.height()),
                        img_rect.min + egui::vec2(x1 * img_rect.width(), y1 * img_rect.height()),
                    )
                });
                match inner {
                    Some(r) => {
                        painter.rect_filled(
                            egui::Rect::from_min_max(
                                img_rect.min,
                                egui::pos2(img_rect.max.x, r.top()),
                            ),
                            0.0,
                            dim,
                        );
                        painter.rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(img_rect.min.x, r.bottom()),
                                img_rect.max,
                            ),
                            0.0,
                            dim,
                        );
                        painter.rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(img_rect.min.x, r.top()),
                                egui::pos2(r.left(), r.bottom()),
                            ),
                            0.0,
                            dim,
                        );
                        painter.rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(r.right(), r.top()),
                                egui::pos2(img_rect.max.x, r.bottom()),
                            ),
                            0.0,
                            dim,
                        );
                        painter.rect_stroke(
                            r,
                            0.0,
                            egui::Stroke::new(1.5_f32, egui::Color32::WHITE),
                            egui::StrokeKind::Inside,
                        );
                        let lw = egui::Stroke::new(0.75_f32, egui::Color32::from_white_alpha(120));
                        for i in 1..3 {
                            let s = i as f32 / 3.0;
                            let x = r.left() + r.width() * s;
                            let y = r.top() + r.height() * s;
                            painter.line_segment(
                                [egui::pos2(x, r.top()), egui::pos2(x, r.bottom())],
                                lw,
                            );
                            painter.line_segment(
                                [egui::pos2(r.left(), y), egui::pos2(r.right(), y)],
                                lw,
                            );
                        }
                    }
                    None => {
                        painter.rect_filled(img_rect, 0.0, dim);
                    }
                }
            }
        }
        ui.horizontal(|ui| {
            ui.small(crate::i18n::t("照片说明:"));
            self.caption_editor(ui, id);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.editor_compare == 1 {
                    ui.small(crate::i18n::t("正在显示原图"));
                } else if self.editor_compare == 2 {
                    ui.small(crate::i18n::t("左:原图  右:效果"));
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_counts_each_channel() {
        let mut img = image::RgbaImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        img.put_pixel(1, 0, image::Rgba([0, 255, 0, 255]));
        img.put_pixel(0, 1, image::Rgba([0, 0, 255, 255]));
        img.put_pixel(1, 1, image::Rgba([128, 128, 128, 255]));
        let h = Histogram::of(&img);
        assert_eq!(h.r[63], 1);
        assert_eq!(h.g[63], 1);
        assert_eq!(h.b[63], 1);
        assert_eq!(h.r[32], 1);
        // 三个纯色像素的 0 值通道都落进同一桶。
        assert_eq!(h.r[0], 2);
        assert_eq!(h.b[0], 2);
        assert_eq!(h.peak(), 2.0);
    }

    #[test]
    fn ratio_text_formats_shutter_and_aperture() {
        assert_eq!(ratio_text(1.0, 125.0), "1/125");
        assert_eq!(ratio_text(28.0, 10.0), "2.8");
        assert_eq!(ratio_text(50.0, 1.0), "50.0");
        assert_eq!(ratio_text(1.0, 0.0), "—");
    }

    #[test]
    fn tool_params_keep_grain_in_unit_range() {
        assert_eq!(
            editor_params("grain", 15.0).get("amount").unwrap(),
            "0.1500"
        );
        assert_eq!(
            editor_params("grain", 100.0).get("amount").unwrap(),
            "1.0000"
        );
        assert_eq!(
            editor_params("warm", 50.0).get("amount").unwrap(),
            "50.0000"
        );
    }

    #[test]
    fn every_basic_tool_points_at_a_known_effect() {
        for t in basic_tools() {
            if let Act::Effect(name, _) | Act::Strength(name, _) = t.act {
                assert!(
                    matches!(
                        crate::effects::support_of(name),
                        crate::effects::Support::Pixel
                    ),
                    "工具格子引用了不可执行的效果: {name}"
                );
            }
        }
        assert_eq!(effect_label("enhance_auto"), "手气不错");
        assert_eq!(effect_label("some_recipe"), "some_recipe");
    }

    #[test]
    fn every_tab_has_icon_and_label() {
        assert_eq!(EditorTab::ALL.len(), 6);
        for t in EditorTab::ALL {
            assert!(!t.glyph().is_empty());
            assert!(!t.label().is_empty());
        }
        assert_eq!(strength_label("tilt"), "扶正角度");
        assert_eq!(strength_label("unknown"), "强度");
    }
}
