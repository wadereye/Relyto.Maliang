//! M4a 批量导出：把当前视图范围内的原图复制到用户选定的目录。
//! 导出是显式用户操作,与"库内不复制原图"原则不冲突。
use std::path::{Path, PathBuf};

fn is_image(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "webp" | "bmp" | "tiff"
            )
        })
        .unwrap_or(false)
}

/// 生成不冲突的目标路径:同名已存在时插 ` (n)` 后缀。
fn free_target(dest: &Path, name: &str) -> PathBuf {
    let mut cand = dest.join(name);
    if !cand.exists() {
        return cand;
    }
    let stem = Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let ext = Path::new(name)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for n in 1.. {
        cand = dest.join(format!("{stem} ({n}){ext}"));
        if !cand.exists() {
            return cand;
        }
    }
    unreachable!()
}

/// 串行复制(paths 已是过滤后的图像列表)。
/// 返回 (成功数, 失败详情)。失败不中断整体;目标目录必须已存在。
pub fn copy_all(paths: &[PathBuf], dest: &Path) -> (usize, Vec<String>) {
    let mut ok = 0;
    let mut errs = Vec::new();
    for src in paths {
        if !src.is_file() {
            errs.push(format!("{}: 源文件不存在", src.display()));
            continue;
        }
        let name = match src.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => {
                errs.push(format!("{}: 无文件名", src.display()));
                continue;
            }
        };
        if !is_image(Path::new(&name)) {
            continue;
        }
        let target = free_target(dest, &name);
        match std::fs::copy(src, &target) {
            Ok(_) => ok += 1,
            Err(e) => errs.push(format!("{}: {e}", src.display())),
        }
    }
    (ok, errs)
}

/// P2-16:导出时套用效果栈(把渲染结果写到目标目录,**不改原图**)。
///
/// 原版的"保存"会把编辑结果写回原文件;我方为安全起见只提供
/// 「导出为带效果副本」,原图与 `photos.filters` 保持不变。
///
/// 栈为空 → 直接复制(与 `copy_all` 等价)。
pub fn copy_all_with_effects(
    items: &[(PathBuf, crate::effects::FilterStack)],
    dest: &Path,
) -> (usize, Vec<String>) {
    let mut ok = 0;
    let mut errs = Vec::new();
    for (src, stack) in items {
        if !src.is_file() {
            errs.push(format!("{}: 源文件不存在", src.display()));
            continue;
        }
        let name = match src.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => {
                errs.push(format!("{}: 无文件名", src.display()));
                continue;
            }
        };
        if !is_image(Path::new(&name)) {
            continue;
        }
        let target = free_target(dest, &name);
        if stack.is_empty() {
            match std::fs::copy(src, &target) {
                Ok(_) => ok += 1,
                Err(e) => errs.push(format!("{}: {e}", src.display())),
            }
            continue;
        }
        match render_with_stack(src, &target, stack) {
            Ok(()) => ok += 1,
            Err(e) => errs.push(format!("{}: {e}", src.display())),
        }
    }
    (ok, errs)
}

/// 渲染单张图(套用效果栈)到目标文件。
fn render_with_stack(
    src: &Path,
    dst: &Path,
    stack: &crate::effects::FilterStack,
) -> anyhow::Result<()> {
    let img = image::open(src)?;
    // 与预览同一坐标空间:先按 EXIF 方向摆正,效果栈的几何类效果
    // (旋转/裁剪)才能在导出时落到与预览一致的位置。
    let img = crate::thumbs::apply_exif_rotation(img, crate::exif::read_orientation(src));
    let mut rgba = img.to_rgba8();
    crate::effects::apply_stack(&mut rgba, stack);
    let out = image::DynamicImage::ImageRgba8(rgba);
    // 目标扩展名是 jpg → JPEG 编码;否则按扩展名推断。
    let fmt = match dst
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
    {
        Some(ref e) if e == "png" => image::ImageFormat::Png,
        Some(ref e) if e == "webp" => image::ImageFormat::WebP,
        Some(ref e) if e == "bmp" => image::ImageFormat::Bmp,
        Some(ref e) if e == "tiff" => image::ImageFormat::Tiff,
        _ => image::ImageFormat::Jpeg,
    };
    out.save_with_format(dst, fmt)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_target_suffixes_on_conflict() {
        let dir = std::env::temp_dir().join(format!("maliang-exp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.jpg"), b"x").unwrap();
        assert_eq!(free_target(&dir, "a.jpg"), dir.join("a (1).jpg"));
        std::fs::write(dir.join("a (1).jpg"), b"x").unwrap();
        assert_eq!(free_target(&dir, "a.jpg"), dir.join("a (2).jpg"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P2-16:导出带效果副本 —— 目标文件应存在且像素已被调整,
    /// 同时**原图不得被修改**。
    #[test]
    fn export_with_effects_leaves_source_untouched() {
        let dir = std::env::temp_dir().join(format!("maliang-fxexp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.jpg");
        let img = image::RgbImage::from_pixel(16, 16, image::Rgb([100, 100, 100]));
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(&src, &buf).unwrap();
        let src_bytes_before = std::fs::read(&src).unwrap();

        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let mut st = crate::effects::FilterStack::new();
        st.push(crate::effects::Effect::enhance(&crate::effects::Adjust {
            brightness: 60.0,
            ..Default::default()
        }));
        let (ok, errs) = copy_all_with_effects(&[(src.clone(), st)], &out);
        assert_eq!(ok, 1, "导出应成功: {errs:?}");
        assert!(errs.is_empty());

        // 原图未被改动
        assert_eq!(std::fs::read(&src).unwrap(), src_bytes_before);
        // 副本存在且被提亮
        let got = image::open(out.join("a.jpg")).unwrap().to_rgb8();
        assert!(got.get_pixel(8, 8)[0] > 100, "副本应被提亮, got {}", got.get_pixel(8, 8)[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
