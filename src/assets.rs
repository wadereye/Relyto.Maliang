//! 内置参数资源(曲线预设、亮度权重、滤镜默认参数、阻尼常量等)。
//!
//! 这些数值集中存放在 `assets/presets.dat` —— 一个带版本号的二进制资源包,
//! 不散落在源码里:调整观感只改数据、不必改代码。
//!
//! 载入顺序:
//! 1. 用户目录下的 `presets.dat`(可选覆盖,见 [`user_asset_path`]);
//! 2. 程序内嵌的 `assets/presets.dat`(编译进可执行文件,始终可用)。
//!
//! 覆盖文件损坏时自动回退到内嵌副本,不会导致程序不可用。
//!
//! 资源包格式见 [`crate::assets_format`];文本与二进制之间可用
//! `--export-assets` / `--import-assets` 互转。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::assets_format::decode;

/// 编译期内嵌的资源包。
static EMBEDDED: &[u8] = include_bytes!("../assets/presets.dat");

/// 已载入的段表(首次访问时解析)。
static SECTIONS: OnceLock<BTreeMap<String, Vec<f64>>> = OnceLock::new();

fn sections() -> &'static BTreeMap<String, Vec<f64>> {
    SECTIONS.get_or_init(|| {
        if let Some(path) = user_asset_path() {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Some(map) = decode(&bytes) {
                    return map;
                }
            }
        }
        decode(EMBEDDED).unwrap_or_default()
    })
}

/// 取一段的数值列表;不存在时返回空切片。
pub fn f64s(key: &str) -> &'static [f64] {
    sections().get(key).map(|v| v.as_slice()).unwrap_or(&[])
}

/// 取一段的数值,按 `(x, y)` 成对解释(不足一对的尾部忽略)。
pub fn pairs(key: &str) -> Vec<(f32, f32)> {
    f64s(key)
        .chunks_exact(2)
        .map(|c| (c[0] as f32, c[1] as f32))
        .collect()
}

/// 滚动阻尼相关的浮点常量。
#[derive(Debug, Clone, Copy)]
pub struct ScrollDamp {
    /// 阻尼系数(被调方首步)。
    pub damp: f64,
    /// 调用方增益。
    pub gain: f64,
    /// `powf` 指数(float)。
    pub exp_soft: f32,
    /// ease-out `powf` 指数(float)。
    pub exp_ease: f32,
    /// 分支阈值 + 反变换缩放因子。
    pub ease_scale: f64,
    /// 包围盒中点系数(缩略图质心位移算法中出现两次)。
    pub mid: f64,
}

static SCROLL_DAMP: OnceLock<ScrollDamp> = OnceLock::new();

/// 滚动阻尼常量(来自资源包)。
pub fn scroll_damp() -> &'static ScrollDamp {
    SCROLL_DAMP.get_or_init(|| {
        let v = f64s("scroll.damp");
        let g = |i: usize| v.get(i).copied().unwrap_or(0.0);
        ScrollDamp {
            damp: g(0),
            gain: g(1),
            exp_soft: g(2) as f32,
            exp_ease: g(3) as f32,
            ease_scale: g(4),
            mid: g(5),
        }
    })
}

/// 用户目录下的覆盖资源路径(`%APPDATA%\Maliang\presets.dat`)。
pub fn user_asset_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("Maliang").join("presets.dat"))
}

/// 把资源包导出为文本(便于人工编辑后再导入)。
pub fn export_text() -> String {
    let mut out = String::new();
    for (name, vals) in sections() {
        out.push_str(&format!("[{name}]\n"));
        if !vals.is_empty() {
            let items: Vec<String> = vals
                .iter()
                .map(|v| {
                    // 整数值不带小数尾巴,便于手写回填。
                    if v.fract() == 0.0 && v.abs() < 1e9 {
                        format!("{}", *v as i64)
                    } else {
                        format!("{v}")
                    }
                })
                .collect();
            out.push_str(&items.join(" "));
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// 从文本重建资源包二进制;格式错误时返回错误信息。
pub fn import_text(text: &str) -> Result<Vec<u8>, String> {
    let sections = crate::assets_format::parse_sections(text)?;
    Ok(crate::assets_format::encode(&sections))
}

/// 把资源包写入用户目录(下次启动生效)。
pub fn write_user_asset(bytes: &[u8]) -> Result<PathBuf, String> {
    let path = user_asset_path().ok_or_else(|| "无法定位用户配置目录".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败:{e}"))?;
    }
    std::fs::write(&path, bytes).map_err(|e| format!("写入失败:{e}"))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_bundle_decodes() {
        let map = decode(EMBEDDED).expect("内嵌资源包应能解码");
        assert!(!map.is_empty(), "资源包不应为空");
    }

    #[test]
    fn embedded_bundle_reencodes_identically() {
        // 内嵌包必须能被本模块的编码器原样重建 —— 保证格式与生成器一致。
        let map = decode(EMBEDDED).expect("内嵌资源包应能解码");
        assert_eq!(crate::assets_format::encode(&map), EMBEDDED);
    }

    #[test]
    fn required_sections_present() {
        for key in [
            "curve.widescreen.master",
            "curve.widescreen.red",
            "curve.widescreen.blue",
            "curve.cross_develop.red",
            "curve.cross_develop.green",
            "curve.cross_develop.blue",
            "curve.retro.master",
            "curve.retro.red",
            "curve.retro.green",
            "curve.retro.blue",
            "curve.invert.master",
            "curve.soft_glow.master",
            "curve.soft_glow.form",
            "curve.pencil_sketch.master",
            "curve.pencil_sketch.form",
            "exposure.pos",
            "exposure.neg",
            "luma.weights",
            "sepia.default",
            "grain.default",
            "warm.gain",
            "cool.gain",
            "vignette",
            "faded",
            "blur.sigma_scale",
            "blur.box_factors",
            "scroll.damp",
        ] {
            assert!(sections().contains_key(key), "缺少资源段:{key}");
        }
    }

    #[test]
    fn curve_sections_have_even_length() {
        for (name, vals) in sections() {
            if name.starts_with("curve.") && !name.ends_with(".form") {
                assert_eq!(vals.len() % 2, 0, "{name} 的控制点应成对");
            }
        }
    }

    #[test]
    fn luma_weights_are_ordered_and_normalized() {
        let w = f64s("luma.weights");
        assert_eq!(w.len(), 3);
        assert!(w[1] > w[0] && w[0] > w[2], "亮度权重应为 绿>红>蓝,got {w:?}");
        let sum: f64 = w.iter().sum();
        assert!((sum - 1.0).abs() < 0.01, "亮度权重之和应接近 1,got {sum}");
    }

    #[test]
    fn text_roundtrip_is_stable() {
        let text = export_text();
        let bytes = import_text(&text).expect("导出的文本应能重新导入");
        let reparsed = decode(&bytes).expect("重新编码的包应能解码");
        let original = sections();
        assert_eq!(reparsed.len(), original.len());
        for (k, v) in original.iter() {
            assert_eq!(reparsed.get(k), Some(v), "段 {k} 往返后不一致");
        }
    }
}
