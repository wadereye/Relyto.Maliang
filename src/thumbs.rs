use anyhow::Result;
use image::imageops::FilterType;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// 缩略图档位。
///
/// 缩略图档位分派:原版是 **8 档枚举**,每档落到**三张缓存表**之一
/// (小 / 大 / 预览),缓存与索引分离。
///
/// 参考原版的像素值常量:**256**、**512**、**192**。
/// 我方取 4 档常用值(小 96 / 中 256 / 大 512 / 预览 1024),
/// 目录分层对应三档缓存:
/// - `thumbs/`    → 小档
/// - `bigthumbs/` → 大档
/// - `previews/`  → 预览档
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// 小档 96px —— 对应原版的小档缓存。
    Small,
    /// 中档 256px —— 原版主力档,也是我方默认。
    Normal,
    /// 大档 512px —— 原版大档缓存。
    Big,
    /// 预览档 1024px —— 原版预览档缓存。
    Preview,
}

impl Tier {
    /// 该档的像素边长。
    pub fn size(self) -> u32 {
        match self {
            Tier::Small => 96,
            Tier::Normal => 256,
            Tier::Big => 512,
            Tier::Preview => 1024,
        }
    }

    /// 该档的缓存子目录名(对应原版的多档缓存表)。
    ///
    /// ⚠️ Small 与 Normal **不能**共用目录:两者尺寸不同(96/256),
    /// 同名文件会互相覆盖 —— 切一次小档就把中档缓存冲掉。
    /// Normal 沿用历史的 `"thumbs"` 目录以保住既有缓存。
    pub fn dir_name(self) -> &'static str {
        match self {
            Tier::Small => "smallthumbs",
            Tier::Normal => "thumbs",
            Tier::Big => "bigthumbs",
            Tier::Preview => "previews",
        }
    }

    /// 供 UI 展示的档位名。
    pub fn label(self) -> &'static str {
        match self {
            Tier::Small => "小缩略图 (96px)",
            Tier::Normal => "普通缩略图 (256px)",
            Tier::Big => "大缩略图 (512px)",
            Tier::Preview => "预览图 (1024px)",
        }
    }

    /// 该档的纹理显存占用(字节,RGBA8)。
    ///
    /// 用途:UI 提示"切到大档会多占多少显存"。
    /// 768 张 × 512×512×4 ≈ 805MB —— 这就是纹理 LRU 上限要按档位调整的原因。
    pub fn bytes(self) -> usize {
        let s = self.size() as usize;
        s * s * 4
    }

    /// 该档建议的纹理 LRU 上限(张)。
    ///
    /// 显存预算按 192MB 反推:768 张 × 256KB(256px) ≈ 192MB;
    /// 大档像素是 4 倍 → 张数降到 1/4。
    pub fn suggested_cache_cap(self) -> usize {
        const BUDGET_BYTES: usize = 192 * 1024 * 1024;
        (BUDGET_BYTES / self.bytes().max(1)).max(16)
    }

    /// 从字节值还原档位(用于跨线程传递)。
    pub fn from_u8(v: u8) -> Option<Tier> {
        match v {
            0 => Some(Tier::Small),
            1 => Some(Tier::Normal),
            2 => Some(Tier::Big),
            3 => Some(Tier::Preview),
            _ => None,
        }
    }
}

/// 全部档位(小 → 预览)。
pub const ALL_TIERS: [Tier; 4] = [Tier::Small, Tier::Normal, Tier::Big, Tier::Preview];

/// 该档的缓存目录。
pub fn tier_dir(tier: Tier) -> PathBuf {
    crate::db::cache_dir().join(tier.dir_name())
}

/// 该档某张照片的缓存文件路径。
pub fn tier_path(tier: Tier, id: i64) -> PathBuf {
    tier_dir(tier).join(format!("{id}.jpg"))
}

/// 清掉某张照片在**所有档位**留下的缩略图缓存文件。
///
/// 索引行被删除(文件已在外部消失)后缓存就成了无主垃圾,
/// 且 id 若被复用会显示成别的照片。监听线程删除索引时一并调用。
pub fn purge_cached(id: i64) {
    for tier in ALL_TIERS {
        let _ = std::fs::remove_file(tier_path(tier, id));
    }
}

/// 孤儿缩略图 GC:清掉「盘上有缓存文件、库里已无对应行」的文件。
///
/// 孤儿来自不走 `purge_cached` 的历史删除(旧版本清行、崩溃窗口、
/// 手动动过缓存目录)。id 复用后孤儿会被当成新照片的缓存显示成别的图,
/// 所以这些文件必须清。返回删除的文件数。
///
/// 逐档 `read_dir` 后分块查库:百万级缓存文件一次拉全表 id 会吃几百 MB,
/// 分块 `id IN (...)` 每块只带回命中的行。只删确认无主的,宁滥勿缺。
pub fn gc_orphans(store: &crate::db::Store, cache_root: &std::path::Path) -> usize {
    let mut removed = 0usize;
    for tier in ALL_TIERS {
        let dir = cache_root.join(tier.dir_name());
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut candidates: Vec<i64> = Vec::new();
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let stem = name.strip_suffix(".jpg").unwrap_or(&name);
            if let Ok(id) = stem.parse::<i64>() {
                candidates.push(id);
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        for chunk in candidates.chunks(500) {
            let alive: std::collections::HashSet<i64> = store.read(|conn| {
                let mut sql =
                    String::from("SELECT id FROM photos WHERE id IN (");
                for k in 0..chunk.len() {
                    sql.push_str(&if k == 0 {
                        format!("?{}", k + 1)
                    } else {
                        format!(",?{}", k + 1)
                    });
                }
                sql.push(')');
                let mut st = match conn.prepare(&sql) {
                    Ok(s) => s,
                    Err(_) => return Default::default(),
                };
                let params: Vec<i64> = chunk.to_vec();
                st.query_map(rusqlite::params_from_iter(params.iter()), |r| r.get::<_, i64>(0))
                    .map(|it| it.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
            });
            for id in chunk {
                if !alive.contains(id)
                    && std::fs::remove_file(cache_root.join(tier.dir_name()).join(format!("{id}.jpg"))).is_ok()
                {
                    removed += 1;
                }
            }
        }
    }
    removed
}

/// 台账对账(文件缺失侧的反向 GC):`thumb_tiers` 说「已生成」、盘上文件却
/// 不在的行,摘掉台账让回填重新生成。
///
/// 缓存目录被外部清空 / 搬移 / 磁盘清理工具扫掉后,格子会永远停在灰色占位
/// —— 台账不摘,回填队列永远取不到它们。只在启动时跑一轮(带设置门控),
/// 运行中的增删都走 `purge_cached` / `clear_tier_row`,台账与文件保持同步。
/// 返回摘掉的台账数。
pub fn gc_missing_tiers(store: &crate::db::Store, cache_root: &std::path::Path) -> usize {
    let mut cleared = 0usize;
    let mut last_id = 0i64;
    loop {
        let chunk: Vec<(i64, u8)> = store.read(|conn| {
            let mut st = match conn.prepare(
                "SELECT photo_id, tier FROM thumb_tiers WHERE photo_id > ?1
                 ORDER BY photo_id LIMIT 20000",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            st.query_map([last_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u8))
            })
            .map(|it| it.flatten().collect())
            .unwrap_or_default()
        });
        if chunk.is_empty() {
            break;
        }
        last_id = chunk.last().map(|(id, _)| *id).unwrap_or(last_id);
        let dead: Vec<(i64, u8)> = chunk
            .iter()
            .filter(|(id, tier)| {
                !cache_root
                    .join(Tier::from_u8(*tier).map(|t| t.dir_name()).unwrap_or_default())
                    .join(format!("{id}.jpg"))
                    .exists()
            })
            .cloned()
            .collect();
        if !dead.is_empty() {
            store.clear_tier_rows(&dead);
            cleared += dead.len();
        }
    }
    cleared
}

/// 按档位读取已落盘的缩略图（不解码原图）。
/// 纯读档(不做自愈);生产路径已换 [`load_tier_repair`],这里留给测试断言用。
#[cfg_attr(not(test), allow(dead_code))]
pub fn load_cached_tier(tier: Tier, id: i64) -> Option<image::DynamicImage> {
    let p = tier_path(tier, id);
    image::open(&p).ok()
}

/// 带自愈的缓存读取(UI 装载路径专用):文件在、解不开(损坏 / 半截)时
/// 删掉坏文件并摘掉该档台账,后台回填会重新生成 —— 不然台账说「已生成」,
/// 坏文件永远占着位子,格子只能显示灰色占位块。
///
/// 只有「文件存在但解码失败」才算损坏:文件不存在是正常未生成,直接返回。
pub fn load_tier_repair(
    store: &crate::db::Store,
    tier: Tier,
    id: i64,
) -> Option<image::DynamicImage> {
    let p = tier_path(tier, id);
    if !p.exists() {
        return None;
    }
    match image::open(&p) {
        Ok(img) => Some(img),
        Err(_) => {
            let _ = std::fs::remove_file(&p);
            store.clear_tier_row(id, tier as u8);
            None
        }
    }
}

// ===================== 解码基底与缩放(②) =====================

/// 内嵌预览够不够格当某一档的基底(按目标尺寸取图)。
///
/// 边长达到目标的一半就收:上采样只会糊,但比整图解码省一个数量级的时间;
/// 达不到一半宁可整图解码,也不要糊成马赛克。
///
/// ⚠ 旧实现是硬编码的「≥128px」,于是 160px 的 EXIF 缩略图会被拿去喂
/// 512 / 1024 档 —— 大档看着就是一团糊。这里按该档的实际尺寸判。
fn embedded_ok(w: u32, h: u32, target: u32) -> bool {
    w.max(h).saturating_mul(2) >= target
}

/// 等比缩到 `size`×`size` 以内该用的目标宽高(与 `image::resize` 同一口径)。
fn fit_dims(w: u32, h: u32, size: u32) -> (u32, u32) {
    let long = w.max(h).max(1) as f64;
    let f = (size as f64 / long).min(1.0);
    (
        ((w as f64 * f).round() as u32).max(1),
        ((h as f64 * f).round() as u32).max(1),
    )
}

/// 缩到 `size` 内(保持宽高比;小图照旧放大,与网格铺满格子的观感一致)。
///
/// 缩小比例 ≥8× 时先用 `thumbnail`(内部 Nearest,按整数倍跳采样)粗缩到
/// 2×目标再精缩一趟 —— 4000×3000 的原图直接 Triangle 到 256,每个输出
/// 像素都要在十几万个源像素上加权,粗缩能省掉绝大部分采样。
fn downscale(img: image::DynamicImage, size: u32) -> image::DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 || size == 0 {
        return img;
    }
    if w.max(h) >= size.saturating_mul(8) {
        let (mw, mh) = fit_dims(w, h, size.saturating_mul(2).max(1));
        return img.thumbnail(mw, mh).resize(size, size, FilterType::Triangle);
    }
    img.resize(size, size, FilterType::Triangle)
}

/// 缩到 `size` 内,**再**按 EXIF 方向摆正。
///
/// 顺序是刻意的:先缩后转。90°/270° 旋转是纯像素置换,
/// 「缩到 S 再转」与「转完再缩到 S」结果逐像素一致(dHash 也不变),
/// 而在 4000×3000 的原图上先转一次要多花十几倍的时间与内存。
///
/// 方案里说的「orientation 6/8 时交换请求宽高」在我方是恒等变换:
/// `image` crate 不支持缩放解码,而各档的目标框都是正方形,交换宽高不变 ——
/// 能省的只有**顺序**这一项。
fn fit_oriented(
    img: image::DynamicImage,
    size: u32,
    orient: u32,
) -> image::DynamicImage {
    apply_exif_rotation(downscale(img, size), orient)
}

/// 解码基底 + 还需施加的 EXIF 方向(1 = 已摆正,不用再转)。
///
/// 取值顺序(快的优先,能不出全图就不出):
///
/// 1. 视频 → Shell 抽帧(`image` crate 解不了容器格式);
/// 2. JPEG 内嵌 EXIF 缩略图 —— 相机原图几乎都有,免整图解码;
/// 3. 相机 RAW → **内嵌 JPEG 预览优先**,Shell 只作最后兜底;
/// 4. PSD / PSB → 只读文件里的合成图,解不了再借系统缩略图;
/// 5. 其余格式走 `image`(按魔数分派),方向随图一起交给调用方。
///
/// 1/2 出的图都已按相机方向摆正(内嵌缩略图与 Shell 缩略图都是),返回 1;
/// RAW 内嵌预览是个例外:各家相机写预览时转不转不一致,见 [`raw_embedded_preview`]。
fn decode_base(src: &Path, target: u32) -> Result<(image::DynamicImage, u32)> {
    // 1) 视频:走系统 Shell 视频缩略提供程序(系统 DirectShow 系后端),
    //    进程内完成解码,免外部进程往返。
    if crate::video::is_video(src) {
        return decode_via_shell_sized(src, target)
            .map(|i| (i, 1))
            // Shell 拿不到帧(解码器缺失等)→ 显式失败,与原版
            // "missing/loading movie" 占位语义一致(媒体类型判定)。
            .ok_or_else(|| anyhow::anyhow!("视频抽帧失败(Shell 无解码器): {}", src.display()));
    }
    // 2) 内嵌 EXIF 缩略图(对应原版 "Use EXIF thumbnails")。
    if let Some(t) = crate::exif::read_jpeg_embedded_thumb(src)
        .and_then(|b| image::load_from_memory(&b).ok())
        .filter(|t| embedded_ok(t.width(), t.height(), target))
    {
        return Ok((t, 1));
    }
    // 3) 相机 RAW:内嵌预览优先,Shell 兜底。
    if crate::scan::is_raw(src) {
        if let Some((img, orient)) = raw_embedded_preview(src, target) {
            return Ok((img, orient));
        }
        if let Some(t) = decode_via_shell_sized(src, target) {
            return Ok((t, 1));
        }
    }
    // 4) PSD / PSB:只读文件里已合并好的合成图,不解析图层、不写回。
    //    合成图缺失(保存时未带上兼容性数据)或色彩模式暂不支持时,
    //    Windows 上再借系统缩略图提供程序兜底;都没有就明确失败立案。
    if crate::psd_preview::is_psd_path(src) {
        if let Some(img) = crate::psd_preview::decode(src) {
            return Ok((img, 1));
        }
        if let Some(t) = decode_via_shell_sized(src, target) {
            return Ok((t, 1));
        }
        return Err(anyhow::anyhow!("无可用预览(合成图缺失或色彩模式暂不支持): {}", src.display()));
    }
    // 5) 常规格式:按**魔数**分派解码器,不是按扩展名。
    //    `image::open` 是照扩展名挑解码器的:缓存台账里 3,340 条
    //    "Illegal start bytes:8950" 就是把 PNG(0x89 'P')喂给了 JPEG 解码器 ——
    //    扩展名写着 .jpg、内容其实是 PNG / WebP 的文件到处都是(下载站、客户端
    //    缓存、批量转档都这么干),按扩展名解会把它们全判成坏图立案。
    //    口径与 `scan::image_dimensions` 一致:先猜格式再解;猜不出来才回退扩展名。
    let img = match image::ImageReader::open(src)?
        .with_guessed_format()?
        .decode()
    {
        Ok(i) => i,
        Err(e) => image::open(src).map_err(|_| e)?,
    };
    Ok((img, read_rotation(src)))
}

/// RAW 的内嵌 JPEG 预览。
///
/// 相机在 TIFF 容器里塞了一张全尺寸 JPEG,直接取它比走 Shell 解码器快一个
/// 量级,也不依赖厂商 RAW 解码扩展装没装。返回(图, 还需施加的方向)。
///
/// 摆正判定:90° 类方向(5..=8)要求宽高互换 —— 若预览自身的宽高比与
/// IFD0 声明的**相反**,说明相机写预览时已经转过了;一致(或缺 IFD0 宽高
/// 标签)则按 Orientation 转。
fn raw_embedded_preview(src: &Path, target: u32) -> Option<(image::DynamicImage, u32)> {
    for p in crate::exif::read_tiff_previews(src, crate::exif::MAX_PREVIEW_CANDIDATES) {
        let Some(img) = image::load_from_memory(&p.bytes).ok() else {
            continue; // 漏网的无损 JPEG 之类:试下一个候选
        };
        let (w, h) = (img.width(), img.height());
        if !embedded_ok(w, h, target) {
            continue; // 太小,喂这一档会糊 → 落回 Shell / 整图解码
        }
        let orient = preview_orient(&p, w, h);
        return Some((img, orient));
    }
    None
}

/// 内嵌预览还需不需要转(判定说明见 [`raw_embedded_preview`])。
fn preview_orient(p: &crate::exif::TiffPreview, w: u32, h: u32) -> u32 {
    if !matches!(p.orient, 5..=8) || p.base_w == 0 || p.base_h == 0 {
        return p.orient; // 不涉及换向,或无从判定 → 按 EXIF 方向转
    }
    if (p.base_w > p.base_h) != (w > h) {
        1 // 宽高比已反 → 相机写预览时已经转过了
    } else {
        p.orient
    }
}

/// 预览大图的**统一取图入口**。
///
/// 返回(图, 还需施加的 EXIF 方向);方向为 1 表示已摆正。
/// 取值顺序见 [`decode_base`];四条路线全失败返回 `None`(预览窗显示"加载失败")。
pub fn load_for_preview(src: &Path, max_side: u32) -> Option<(image::DynamicImage, u32)> {
    decode_base(src, max_side).ok()
}

/// 生成指定档位的缩略图;返回 dHash。
///
/// dHash **只在中档(Normal)计算** —— 它是全图感知哈希,与显示档位无关,
/// 没必要每档重算(原版 dHash/uid64 也只算一次存在 DB 列里)。
/// 其余档位返回 0。
pub fn generate_tier(tier: Tier, id: i64, src: &Path) -> Result<u64> {
    let size = tier.size();
    let (img, orient) = decode_base(src, size)?;
    let fit = fit_oriented(img, size, orient);
    // 各档独立生成,不复用更小档的缓存 —— 免得低档糊了大档。
    let dhash = if tier == Tier::Normal {
        crate::hash::dhash_u64(&fit)
    } else {
        0
    };
    save_letterboxed(size, &fit, tier_path(tier, id))?;
    Ok(dhash)
}

/// 等比缩样到 256x256 内,居中贴浅灰画布(Picasa 式 letterbox)。
/// 返回 (dHash):由缩样图(letterbox 之前)计算,避免边框干扰梯度。
pub fn generate(id: i64, src: &Path) -> Result<u64> {
    generate_tier(Tier::Normal, id, src)
}

/// 原子落盘 JPEG:先写临时文件再改名。
///
/// UI 线程会每帧轮询读缓存文件,若直接写最终路径,读线程可能逮到只写了
/// 一半的 JPEG —— 解码器对截断文件宽容,会解出"半张图"并缓存显示
/// (预览窗首次打开只显示一半的原因)。改名在同一卷上是原子的,杜绝半读。
fn save_jpeg_atomic(img: &image::DynamicImage, path: &std::path::Path) -> Result<()> {
    let tmp = path.with_extension("jpg.tmp");
    img.save_with_format(&tmp, image::ImageFormat::Jpeg)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// JPEG 缓存没有 alpha 通道:先把透明像素合成到浅灰底,免得 GIF / PNG / PSD
/// 的透明区在缩略图里变成黑块(`to_rgb8()` 会直接丢掉 alpha)。
fn flatten_on_bg(img: &image::DynamicImage, bg: image::Rgb<u8>) -> image::RgbImage {
    let rgba = img.to_rgba8();
    let mut out = image::RgbImage::from_pixel(rgba.width(), rgba.height(), bg);
    for (x, y, px) in rgba.enumerate_pixels() {
        let a = px.0[3] as u32;
        if a == 255 {
            *out.get_pixel_mut(x, y) = image::Rgb([px.0[0], px.0[1], px.0[2]]);
        } else if a > 0 {
            let mix = |s: u8, d: u8| ((s as u32 * a + d as u32 * (255 - a)) / 255) as u8;
            *out.get_pixel_mut(x, y) = image::Rgb([
                mix(px.0[0], bg.0[0]),
                mix(px.0[1], bg.0[1]),
                mix(px.0[2], bg.0[2]),
            ]);
        }
    }
    out
}

/// 把已缩样的图居中贴到 size×size 浅灰画布并落盘(Picasa 式 letterbox)。
/// 目录不存在则先建。
fn save_letterboxed(size: u32, fit: &image::DynamicImage, path: std::path::PathBuf) -> Result<()> {
    let mut canvas = image::RgbImage::from_pixel(size, size, image::Rgb([248, 248, 248]));
    let x = ((size - fit.width()) as i64) / 2;
    let y = ((size - fit.height()) as i64) / 2;
    let flat = flatten_on_bg(fit, image::Rgb([248, 248, 248]));
    image::imageops::overlay(&mut canvas, &flat, x, y);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    save_jpeg_atomic(&image::DynamicImage::ImageRgb8(canvas), &path)
}

/// 用 Windows Shell 缩略图扩展解码到指定尺寸（相机 RAW 等 image crate 解不了的格式）。
///
/// 与原版一致的做法:走 COM `IThumbnailProvider`,
/// 借系统已安装的相机厂商 RAW 解码器出图。Shell 未就绪时返回 None。
fn decode_via_shell_sized(src: &Path, size: u32) -> Option<image::DynamicImage> {
    let (bgra, w, h) = crate::win::shell_thumbnail(src, size)?;
    // GDI 32bpp 是 BGRA，且自下而上；转成 image crate 的 RGBA8。
    let mut buf = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let src_i = ((h as usize - 1 - y) * w as usize + x) * 4;
            let dst_i = (y * w as usize + x) * 4;
            buf[dst_i] = bgra[src_i + 2]; // R
            buf[dst_i + 1] = bgra[src_i + 1]; // G
            buf[dst_i + 2] = bgra[src_i]; // B
            buf[dst_i + 3] = 255; // A
        }
    }
    image::RgbaImage::from_raw(w, h, buf).map(image::DynamicImage::ImageRgba8)
}

fn read_rotation(src: &Path) -> u32 {
    crate::exif::read_orientation(src)
}

/// EXIF orientation 修正（值与 imageops::rotate90=顺时针 90 一致）。
pub fn apply_exif_rotation(img: image::DynamicImage, orient: u32) -> image::DynamicImage {
    match orient {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

/// 批量后台生成缺失缩略图；返回成功生成的照片 id 列表，仅成功才置 thumb_ok=1。
///
/// 注意:生成过程**不持有数据库锁**(只锁一小会儿取 id/path 列表),
/// 否则解码大图会长时间阻塞 UI 的数据库访问。
pub fn backfill(store: &crate::db::Store, limit: usize) -> Result<Vec<i64>> {
    backfill_tier(store, Tier::Normal, &[], limit)
}

/// 批量生成指定档位缺失的缩略图。
///
/// 返回成功生成的 id 列表。Normal 档会顺带回写 `thumb_ok=1`、`dhash` 与
/// `rich_state=2`;其余档位只记 `thumb_tiers` 表(对应原版的三张缓存表各自
/// 独立记账)。
///
/// `visible` 是当前可见目录清单,只影响 Normal 档的取行顺序
/// (可见优先 + 全库兜底,见 [`crate::db::Store::missing_tier`])。
pub fn backfill_tier(
    store: &crate::db::Store,
    tier: Tier,
    visible: &[String],
    limit: usize,
) -> Result<Vec<i64>> {
    let rows = store.missing_tier(tier as u8, visible, limit);
    backfill_rows(store, tier, rows)
}

/// 闲时低速补齐:优先队列排空后,把其余目录的缩略图提前做好。
///
/// 参考件的文件夹切换快,是因为打开时缓存早已落盘、只按索引取字节;
/// 若一律等用户切过去才补,每次进新文件夹都要现等生成。这里在空闲时
/// 一小批一小批铺过去,任何可见目录 / 新导入一出现就立刻让位。
pub fn backfill_idle(store: &crate::db::Store, limit: usize) -> Result<Vec<i64>> {
    let rows = store.missing_idle(limit);
    backfill_rows(store, Tier::Normal, rows)
}

/// 缩略图专用 rayon 池:线程数只给核数的四分之一,且每个线程降为
/// BelowNormal。之前用全局池 —— 全局池线程按 Normal 优先级创建,首启
/// 全盘补缩略图时把所有核吃满,UI 线程同等优先级抢不到时间片,整个
/// 程序(乃至系统)卡到补齐为止。窄池 + 低优先级后,UI 永远优先拿核,
/// 生成吞吐牺牲一点,换来「补图全程不卡界面」。
fn thumb_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let n = (crate::win::cpu_count() / 4).clamp(1, 4);
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .thread_name(|i| format!("maliang-thumb-{i}"))
            .start_handler(|_| {
                crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal)
            })
            .build()
            .expect("缩略图线程池初始化失败")
    })
}

fn backfill_rows(
    store: &crate::db::Store,
    tier: Tier,
    rows: Vec<(i64, String)>,
) -> Result<Vec<i64>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let (ok, failed): (
        Vec<(i64, u64, Option<crate::scan::Derived>)>,
        Vec<(i64, &String, String)>,
    ) = thumb_pool().install(|| {
        rows.par_iter()
        .filter_map(|(id, path)| {
            // 云盘占位文件(OneDrive / SeaFile 按需下载):生成缩略图要打开
            // 内容,会把占位文件整个拉下来。跳过且**不记账**——这不是坏文件,
            // 用户以后打开/钉住文件后,行还在缺失队列里自然补上。
            if crate::win::is_cloud_placeholder(Path::new(path)) {
                return None;
            }
            Some(match generate_tier(tier, *id, Path::new(path)) {
                Ok(h) => {
                    // 派生列(EXIF / 尺寸 / GPS / 机身)在这一趟读:
                    // 发现路径不打开文件内容,这里是唯一读它的地方。
                    // 只在中档读 —— 其余档共用同一份 EXIF,重读是白花的 IO。
                    let d = (tier == Tier::Normal)
                        .then(|| crate::scan::read_derived(Path::new(path)));
                    rayon::iter::Either::Left((*id, h, d))
                }
                // 失败也要记账:否则下一轮队列还会取回同一批坏文件,
                // worker 永远空转在开头(已知问题: 回收站 $I*.jpg 元数据)。
                Err(e) => rayon::iter::Either::Right((*id, path, short_reason(&e))),
            })
        })
        .partition_map(|either| either)
    });
    for (id, path, why) in &failed {
        escalate_failure(store, *id, path, why);
    }
    // 删除竞态:生成期间照片可能已被移出图库(监听对账),
    // 刚落盘的缩略图必须清掉,否则 rowid 复用时会被当成新照片的缓存。
    let alive: std::collections::HashMap<i64, String> = rows.iter().cloned().collect();
    let (ok, stale): (
        Vec<(i64, u64, Option<crate::scan::Derived>)>,
        Vec<(i64, u64, Option<crate::scan::Derived>)>,
    ) = ok
        .into_iter()
        .partition(|(id, _, _)| alive.get(id).is_some_and(|p| store.photo_alive(*id, p)));
    for (id, _, _) in &stale {
        purge_cached(*id);
    }
    if ok.is_empty() {
        return Ok(Vec::new());
    }
    if tier == Tier::Normal {
        // 派生列 + thumb_ok + dhash + rich_state 一条 UPDATE 落定(见 `mark_derived`)。
        let batch: Vec<(i64, u64, Option<&crate::scan::Derived>)> = ok
            .iter()
            .map(|(id, h, d)| (*id, *h, d.as_ref()))
            .collect();
        store.mark_derived(&batch)?;
    }
    for (id, _, _) in &ok {
        let _ = store.mark_tier_done(*id, tier as u8);
    }
    Ok(ok.into_iter().map(|(id, _, _)| id).collect())
}

/// 派生失败记账,累计到阈值就立案进坏媒体台账。
///
/// 立案后 `rich_state` 变 3、行出队,不再每轮重试同一张坏图;
/// 网格右键给「隐藏此文件 / 不隐藏」两个出口(对齐参考件
/// 坏图清单的 Hide / Don't Hide 两态),后者删台账并把行放回待派生。
fn escalate_failure(store: &crate::db::Store, id: i64, path: &str, why: &str) {
    if store.mark_thumb_fail(id) < crate::db::BAD_MEDIA_THRESHOLD {
        return;
    }
    let _ = store.mark_bad_media(path, why);
}

/// 台账里存的原因串:截断到一行,免得 anyhow 的因果链把台账撑成日志。
fn short_reason(e: &anyhow::Error) -> String {
    let s = format!("{e:#}");
    let mut t: String = s.chars().take(160).collect();
    if t.len() < s.len() {
        t.push('…');
    }
    t
}

/// UI 发布的「当前可见目录」清单:worker 每轮取行前读一次。
///
/// 派生队列的排序键是**可见优先 + 全库兜底**(用户已拍):
/// 眼前的图先清晰,但后台最终会把整个作用域做完 —— 这不是「只补看得见的」。
pub type VisibleDirs = Arc<Mutex<Vec<String>>>;

/// 造一份空的可见目录句柄(UI 与 worker 各持一个 `Arc`)。
pub fn visible_dirs() -> VisibleDirs {
    Arc::new(Mutex::new(Vec::new()))
}

/// 闲时补齐每批的张数:小批短间隔,既不霸占磁盘也不至于 2 秒才动一批。
const IDLE_BATCH: usize = 8;
/// 闲时补齐两批之间的让出时间(毫秒)。
const IDLE_GAP_MS: u64 = 120;
/// 优先队列两批之间的让出时间(毫秒)。
///
/// 首启全盘补图是持续数分钟到数小时的洪峰,批与批之间若无缝衔接,
/// UI 每帧都要从满负荷的后台解码里抢时间片。批间留一条呼吸缝,
/// 一批 32 张的解码量也保证了 UI 侧「占位格逐批变清晰」的即时感。
const BATCH_GAP_MS: u64 = 25;

/// 常驻后台缩略图 worker 的对外句柄。
///
/// 持有 `JoinHandle`(退出时 join)与"完成信号"接收端。
pub struct ThumbWorker {
    handle: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    /// 每完成一批就发一次信号,携带本批生成成功的照片 id 列表。
    /// UI 据此把对应格子的占位纹理换成真图。
    pub done: mpsc::Receiver<Vec<i64>>,
    /// 当前要补齐的档位(UI 切档 → worker 改补那一档)。
    pub tier: Arc<AtomicU8>,
    /// UI 发布的可见目录清单(可见优先 + 全库兜底)。
    /// 用 [`ThumbWorker::publish_visible`] 写,worker 每轮取行前读一次快照。
    visible: VisibleDirs,
}

/// 一次性给**误判**的坏图销案(台账 3,427 条里 3,340 条写着
/// "Illegal start bytes:8950" —— 那是 0x89 'P',PNG 的头被喂给了 JPEG 解码器)。
///
/// 判据不看台账里的原因串(那是历史文案,改一次措辞就对不上),而是**现在**拿
/// 魔数重认一次格式:认得出来 = 改成按魔数分派之后能解,该销案重排队;
/// 认不出来 = 真坏或格式不支持,留在台账里不折腾。
/// 文件已经不存在的行也不动:那是另一件事(缺失清理),不在这一刀的范围里。
///
/// 标记带版本号:解码口径再改一次就往前推一格,老台账要重新过一遍。
fn reset_misfiled_bad_media_once(store: &crate::db::Store) {
    const MARK: &str = "bad_media_resniffed";
    const VERSION: &str = "v1";
    if store.get_setting(MARK).as_deref() == Some(VERSION) {
        return;
    }
    let clear: Vec<String> = store
        .bad_media()
        .into_iter()
        .map(|b| b.path)
        .filter(|p| image::ImageFormat::from_path(Path::new(p)).is_ok())
        .collect();
    let _ = store.unmark_bad_media_batch(&clear);
    let _ = store.set_setting(MARK, VERSION);
}

/// 启动**常驻**缩略图后台线程(+ 多档 + 作用域队列)。
///
/// 行为(对齐原版 Picasa 的"缩略图缓存后台生成"):
/// - 补齐的是**派生门放行的行**(`rich_state=1`),不再是"所有 `thumb_ok=0`";
/// - 之后每 2 秒查一次库,发现新增/变更照片就继续补(扫描后自动补齐);
/// - 每批完成后通过 `done` 通道通知 UI,UI 据此清掉对应纹理缓存并重绘网格;
/// - **档位可变**:UI 改 `tier` 后,worker 下一轮就改补那一档
///   (原版切档时按需从对应档位的缓存取,缺则生成 —— 三档各自独立记账)。
///
/// 线程优先级降为 BelowNormal,不与 UI 抢核(与原版一致的做法)。
pub fn spawn_worker(store: Arc<crate::db::Store>, visible: VisibleDirs) -> ThumbWorker {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let tier = Arc::new(AtomicU8::new(Tier::Normal as u8));
    let tier2 = tier.clone();
    let visible2 = visible.clone();
    let handle = std::thread::spawn(move || {
        crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);
        // 误判坏图的一次性销案放在 worker 线程里做:几千个文件要各读一次头,
        // 挂在 UI 线程上会卡住第一帧。
        reset_misfiled_bad_media_once(&store);
        // 批内走专用窄池(见 `thumb_pool`),批规模取小批:一批完成 → 通知 UI
        // 一次 → 占位格立刻换真图。大批只减少通信次数,却让单批解码拖长、
        // 占住写库连接,首启全盘场景下 UI 会长时间看不到新图。
        let batch = (crate::win::cpu_count() * 4).clamp(8, 32);
        let mut last_tier = Tier::Normal;
        while !stop2.load(Ordering::Relaxed) {
            let cur = tier2.load(Ordering::Relaxed);
            let t = Tier::from_u8(cur).unwrap_or(Tier::Normal);
            // 档位切换 → 重新标记(UI 侧会清纹理缓存,重新加载该档)。
            if t != last_tier {
                last_tier = t;
            }
            // 可见清单每轮取一次快照:锁只在这一瞬间持有,生成期间不碰它。
            let vis: Vec<String> = visible2.lock().unwrap().clone();
            let ids = backfill_tier(&store, t, &vis, batch).unwrap_or_default();
            if !ids.is_empty() {
                let _ = tx.send(ids);
                // 优先队列批间也让一下(BATCH_GAP_MS):洪峰期不与 UI 帧抢核。
                std::thread::sleep(Duration::from_millis(BATCH_GAP_MS));
                // 立刻继续下一批,直到这一轮补完。
                continue;
            }
            // 常用目录 / 可见目录 / 新导入都排空了 —— 才轮到其余目录。
            // 每批只补一小把,中间让出 CPU;下一轮开头会重新检查优先队列,
            // 用户一滚动或导入就立刻让位,不会跟眼前的图抢时间。
            if t == Tier::Normal {
                let idle = backfill_idle(&store, IDLE_BATCH).unwrap_or_default();
                if !idle.is_empty() {
                    let _ = tx.send(idle);
                    std::thread::sleep(Duration::from_millis(IDLE_GAP_MS));
                    continue;
                }
            }
            // 全库都补齐了:睡一会再查(扫描新增照片后自动补齐)。
            std::thread::sleep(Duration::from_millis(2000));
        }
    });
    ThumbWorker {
        handle: Some(handle),
        stop,
        done: rx,
        tier,
        visible,
    }
}

impl ThumbWorker {
    /// 发布当前可见目录(网格所在目录 / 搜索结果覆盖的目录)。
    ///
    /// worker 下一轮取行时就先补这些目录里的行,不足一批再用全库兜底。
    /// 清单没变就不写锁,免得逐帧调用白白抢一次互斥。
    pub fn publish_visible(&self, dirs: Vec<String>) {
        let mut g = self.visible.lock().unwrap();
        if *g != dirs {
            *g = dirs;
        }
    }

    /// 停止 worker(UI 退出时调用)。
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for ThumbWorker {
    /// App 被丢弃时(退出)停掉 worker,不留游离后台线程。
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 重新生成单张缩略图并回写 dhash/thumb_ok(烘培保存后调用)。
pub fn regen(store: &crate::db::Store, id: i64) -> Result<()> {
    let path: String = store
        .read(|conn| conn.query_row("SELECT path FROM photos WHERE id=?1", [id], |r| r.get(0)))?;
    let h = generate(id, Path::new(&path))?;
    store.write(|conn| -> anyhow::Result<()> {
        conn.execute(
            "UPDATE photos SET thumb_ok=1, dhash=?2 WHERE id=?1",
            rusqlite::params![id, h as i64],
        )?;
        Ok(())
    })?;
    Ok(())
}

// ===== 预览缓存 =====

/// 预览图的最长边(像素)。原版预览图是独立的缓存表。
pub const PREVIEW_SIZE: u32 = 1024;

/// 预览缓存文件路径(落盘 `previews/{id}.jpg`)。
///
/// 原版把预览图存进独立的块文件库,我方用同构的独立目录 + 同名 jpg。
pub fn preview_path(id: i64) -> PathBuf {
    tier_path(Tier::Preview, id)
}

/// 读取已缓存的预览图(不解码原图)。命中即返回。
pub fn load_preview_cached(id: i64) -> Option<image::DynamicImage> {
    image::open(preview_path(id)).ok()
}

/// 生成预览图并落盘(后台线程调用)。返回是否成功。
///
/// 取图统一走 `load_for_preview`(内嵌缩略图 → RAW 内嵌预览 → Shell → 全图解码),
/// 再等比缩到 1024 内、按 EXIF 方向摆正、存 jpeg。
///
/// **不计算 dHash** —— dHash 是全图感知哈希,中档已算过,预览档复用即可。
pub fn build_preview(id: i64, src: &Path) -> Result<()> {
    let (img, orient) = load_for_preview(src, PREVIEW_SIZE)
        .ok_or_else(|| anyhow::anyhow!("预览解码失败: {}", src.display()))?;
    // 预览**不放大**:小图原样摆正即可(缩略图那一路才需要铺满格子)。
    let resized = if img.width().max(img.height()) > PREVIEW_SIZE {
        fit_oriented(img, PREVIEW_SIZE, orient)
    } else {
        apply_exif_rotation(img, orient)
    };
    let dir = tier_dir(Tier::Preview);
    let _ = std::fs::create_dir_all(&dir);
    save_jpeg_atomic(&resized, &preview_path(id))
}

/// 预览请求的后台结果通道类型。
pub type PreviewRx = mpsc::Receiver<(i64, Result<bool, String>)>;

/// 预览 worker 的对外句柄(可停止)。
pub struct PreviewWorker {
    /// 发送预览请求 (照片 id, 路径)。
    pub req: mpsc::Sender<(i64, String)>,
    /// 接收完成信号 (照片 id, 结果)。
    pub done: PreviewRx,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// 启动预览生成 worker(常驻)。
///
/// UI 打开一张照片时:先看 `load_preview_cached` 有没有缓存;
/// 没有就发请求给这个 worker,worker 在后台解码 + 落盘,
/// 完成后通过通道回报,UI 再加载纹理。
///
/// 这样 UI 线程**不做解码**(原来同步 `image::open` 4K 原图会卡住界面几百毫秒)。
///
/// 对应原版 `PicasaPhotoViewer.exe` 独立进程 + `previews.db` 的分工:
/// 原版把预览解码放在**独立进程**里,我方用后台线程达到同样效果(不阻塞 UI)。
pub fn spawn_preview_worker(store: Arc<crate::db::Store>) -> PreviewWorker {
    let (req_tx, req_rx) = mpsc::channel::<(i64, String)>();
    let (done_tx, done_rx) = mpsc::channel::<(i64, Result<bool, String>)>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let handle = std::thread::spawn(move || {
        crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);
        while !stop2.load(Ordering::Relaxed) {
            // 阻塞等待请求(有请求才干活,不空转)。
            let req = match req_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(r) => r,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let (id, path) = req;
            // 已有缓存 → 直接回报命中。
            if preview_path(id).exists() {
                let _ = done_tx.send((id, Ok(true)));
                continue;
            }
            let res = build_preview(id, Path::new(&path)).map(|_| true).map_err(|e| e.to_string());
            if res.is_ok() {
                if store.photo_alive(id, &path) {
                    let _ = store.mark_tier_done(id, Tier::Preview as u8);
                } else {
                    // 删除竞态:生成期间已被移出图库 → 丢弃刚写的缓存。
                    purge_cached(id);
                }
            }
            let _ = done_tx.send((id, res));
        }
    });
    PreviewWorker {
        req: req_tx,
        done: done_rx,
        stop,
        handle: Some(handle),
    }
}

impl PreviewWorker {
    /// 停止预览 worker(UI 退出时调用)。
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PreviewWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成测试专用的**唯一**照片 id。
    ///
    /// 缩略图/预览缓存目录是**跨进程共享**的(`~/.cache/Relyto/Maliang`),
    /// 用固定 id 会与其它测试(或历史遗留文件)撞车 ——
    /// 表现为"初始应无缓存"这类断言偶发失败。
    ///
    /// 因此 id 由「纳秒时间戳低位 + 进程内自增序号」合成:
    /// - 时间戳保证**跨运行**不重复 —— 单靠 PID 不够,系统会循环复用 PID;
    /// - 自增序号保证**同进程内**多个测试互不干扰。
    fn unique_id() -> i64 {
        use std::sync::atomic::AtomicU64;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed) as i64;
        900_000_000 + ((nanos % 900_000) * 1_000) + seq
    }

    /// 清掉某个 id 在所有档位目录下留下的缓存文件。
    ///
    /// 缓存目录位于进程之外、不会被自动回收,测试写完必须自己清:
    /// 否则既会无限堆积(此前单目录已积累数百个历史文件),
    /// 也让「初始应无缓存」这类断言变成靠运气。
    fn purge_cache(id: i64) {
        purge_cached(id);
    }

    /// 测试期缓存清理守卫:`Drop` 时清掉该 id 的缓存文件。
    ///
    /// 用守卫而非在测试末尾手写清理,是为了**断言失败时也能清干净** ——
    /// 否则一次失败的测试留下脏文件,会反过来污染下一次运行。
    struct CacheGuard(i64);

    impl Drop for CacheGuard {
        fn drop(&mut self) {
            purge_cache(self.0);
        }
    }

    /// 回归:缓存清理必须覆盖**全部**档位目录。
    ///
    /// 缓存目录跨进程共享、且不会被自动回收。只要漏掉一个档位,
    /// 那个目录就会永久堆积历史文件,最终被后续运行的 `unique_id()` 撞上
    /// (表现为「初始应无缓存」偶发失败)。
    #[test]
    fn purge_cache_clears_every_tier() {
        let id = unique_id();
        let _guard = CacheGuard(id);
        // 每个档位目录各放一个文件,模拟刚生成完缓存的状态。
        for tier in ALL_TIERS {
            let p = tier_path(tier, id);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
            assert!(p.exists(), "{tier:?} 缓存应已写入");
        }
        purge_cache(id);
        for tier in ALL_TIERS {
            assert!(
                !tier_path(tier, id).exists(),
                "{tier:?} 缓存清理后不应残留"
            );
        }
    }

    /// 内嵌 EXIF 缩略图路径端到端(把一张真编码的 JPEG 塞进 APP1 当内嵌预览)。
    #[test]
    fn generate_uses_embedded_thumb() {
        let mut img = image::RgbImage::new(300, 200);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]);
        }
        let mut jpg_bytes: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut jpg_bytes), image::ImageFormat::Jpeg)
            .unwrap();
        // 手工构造 APP1/Exif,IFD1 指向该 JPEG
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes());
        // IFD0 是空的:8(TIFF 头)+ 2(计数=0)+ 0 条目录项 + 4(next 指针)。
        let ifd1_at = 8 + 2 + 4;
        tiff.extend_from_slice(&0u16.to_le_bytes()); // IFD0 空
        tiff.extend_from_slice(&(ifd1_at as u32).to_le_bytes()); // next=IFD1
        tiff.extend_from_slice(&3u16.to_le_bytes()); // IFD1:3 条
        let entries_len = 2 + 3 * 12 + 4;
        let thumb_at = ifd1_at + entries_len;
        let push = |buf: &mut Vec<u8>, tag: u16, typ: u16, cnt: u32, v: u32| {
            buf.extend_from_slice(&tag.to_le_bytes());
            buf.extend_from_slice(&typ.to_le_bytes());
            buf.extend_from_slice(&cnt.to_le_bytes());
            buf.extend_from_slice(&v.to_le_bytes());
        };
        push(&mut tiff, 0x0103, 3, 1, 6);
        push(&mut tiff, 0x0201, 4, 1, thumb_at as u32);
        push(&mut tiff, 0x0202, 4, 1, jpg_bytes.len() as u32);
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(&jpg_bytes);
        let mut full: Vec<u8> = vec![0xFF, 0xD8];
        let seg_len = 2 + 6 + tiff.len();
        full.extend_from_slice(&[0xFF, 0xE1]);
        full.extend_from_slice(&(seg_len as u16).to_be_bytes());
        full.extend_from_slice(b"Exif\x00\x00");
        full.extend_from_slice(&tiff);
        full.extend_from_slice(&[0xFF, 0xD9]);

        let dir = std::env::temp_dir().join(format!("rk-thumbgen-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("cam.jpg");
        std::fs::write(&src, &full).unwrap();
        let tid = unique_id();
        let _guard = CacheGuard(tid);
        purge_cache(tid);
        let h = generate(tid, &src).expect("内嵌缩略图路径应成功");
        assert!(h != 0, "dHash 不应为 0");
        assert!(
            tier_path(Tier::Normal, tid).exists(),
            "应落盘缩略图"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 读回落盘缩略图不走原图解码。
    /// 生成后应能读回,且尺寸为默认档的 256×256(letterbox 画布)。
    #[test]
    fn load_cached_returns_256_canvas() {
        let dir = std::env::temp_dir().join(format!("rk-thumblog-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("a.jpg");
        let mut img = image::RgbImage::new(640, 480);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([(x % 256) as u8, (y % 256) as u8, 64]);
        }
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(&src, &buf).unwrap();

        let id = unique_id();
        let _guard = CacheGuard(id);
        purge_cache(id);
        generate(id, &src).expect("应生成缩略图");
        let cached = load_cached_tier(Tier::Normal, id).expect("应能读回落盘缩略图");
        assert_eq!(cached.width(), Tier::Normal.size());
        assert_eq!(cached.height(), Tier::Normal.size());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 生成失败要记账:同一张反复失败达阈值后立案进坏媒体台账并出队。
    /// 否则一批坏文件(如回收站元数据)会让 worker 永远空转。
    #[test]
    fn backfill_marks_repeated_failures_then_ledgers_them() {
        let store = std::sync::Arc::new(crate::db::Store::open_mem().unwrap());
        let photo_id = unique_id();
        {
            let conn = store.test_conn();
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,rich_state)
                 VALUES(?1,?2,'/x',0,0,1)",
                rusqlite::params![photo_id, "/nonexistent/rk-fail/bad.jpg"],
            )
            .unwrap();
        }
        // 每轮:取到这张 → 生成失败 → 记账。三轮后达阈值立案。
        for round in 1..=3 {
            let ids = backfill_tier(&store, Tier::Normal, &[], 10).unwrap();
            assert!(ids.is_empty(), "第 {round} 轮不应有成功产出");
        }
        let (f, rs): (i64, i64) = store
            .test_conn()
            .query_row(
                "SELECT thumb_fail, rich_state FROM photos WHERE id=?1",
                [photo_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(f, 3, "三次失败应各记一笔");
        assert_eq!(rs, crate::db::RICH_BAD as i64, "达阈值即立案为坏文件");
        assert!(
            store.is_bad_media("/nonexistent/rk-fail/bad.jpg"),
            "台账里要有这一条"
        );
        // 出队:worker 得以推进到后续照片,不再每轮重试同一张坏图。
        assert!(
            store.missing_tier(1, &[], 10).is_empty(),
            "立案后应离开回填队列"
        );
    }

    /// + 后台 worker 会把**派生门放行**(`rich_state=1`)的照片补上
    /// 缩略图,回报 id,并把状态推到已派生。
    #[test]
    fn worker_backfills_missing_thumbs() {
        let dir = std::env::temp_dir().join(format!("rk-thumbworker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.jpg");
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(32, 24))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(&src, &buf).unwrap();

        let store = std::sync::Arc::new(crate::db::Store::open_mem().unwrap());
        // 手动插一行(thumb_ok=0 + rich_state=1),模拟"扫描已入库、派生门已放行、
        // 缩略图还没生成"。id 走 unique_id():两个各自开 in-memory 库的测试
        // 若都用自增 id 1,会同时往同一批缓存文件上写。
        let photo_id = unique_id();
        let _guard = CacheGuard(photo_id);
        {
            let conn = store.test_conn();
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,rich_state) VALUES(?1,?2,?3,0,0,1)",
                rusqlite::params![photo_id, src.to_string_lossy(), dir.to_string_lossy()],
            )
            .unwrap();
        }
        let worker = spawn_worker(store.clone(), visible_dirs());
        // 等待 worker 回报(最多 5s)
        let mut done = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while done.is_empty() && std::time::Instant::now() < deadline {
            match worker.done.recv_timeout(Duration::from_millis(200)) {
                Ok(ids) => done.extend(ids),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(!done.is_empty(), "worker 应回报生成成功的 id");
        // 库里应已置 thumb_ok=1,且派生状态推到已派生。
        let (ok, rs): (i64, i64) = store
            .test_conn()
            .query_row(
                "SELECT thumb_ok, rich_state FROM photos WHERE path=?1",
                [&src.to_string_lossy()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(ok, 1, "应回写 thumb_ok=1");
        assert_eq!(rs, crate::db::RICH_DONE as i64, "应回写 rich_state=2");

        worker.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未纳入作用域(`rich_state=0`)的行不进队列 —— worker 不该去打开它。
    #[test]
    fn worker_skips_rows_out_of_scope() {
        let store = std::sync::Arc::new(crate::db::Store::open_mem().unwrap());
        let photo_id = unique_id();
        let _guard = CacheGuard(photo_id);
        {
            let conn = store.test_conn();
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,rich_state)
                 VALUES(?1,'/nonexistent/rk-oos/a.jpg','/nonexistent/rk-oos',0,0,0)",
                rusqlite::params![photo_id],
            )
            .unwrap();
        }
        assert!(
            backfill_tier(&store, Tier::Normal, &[], 10)
                .unwrap()
                .is_empty(),
            "作用域外的行不该被派生"
        );
        let f: i64 = store
            .test_conn()
            .query_row(
                "SELECT thumb_fail FROM photos WHERE id=?1",
                [photo_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(f, 0, "没进队列就不该记失败");
    }

    // ===== 多档缩略图 =====

    /// 损坏缓存自愈:文件在但解不开 → 删文件 + 摘台账;中档还要把
    /// 已派生的行退回「待派生」,回填队列才会重新生成。
    #[test]
    fn load_tier_repair_heals_corrupt_cache() {
        let store = std::sync::Arc::new(crate::db::Store::open_mem().unwrap());
        let photo_id = unique_id();
        let _guard = CacheGuard(photo_id);
        {
            let conn = store.test_conn();
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,rich_state,thumb_ok)
                 VALUES(?1,'/x/a.jpg/anything.jpg','/x',0,0,2,0)",
                rusqlite::params![photo_id],
            )
            .unwrap();
        }
        // 中档坏文件:写一段不是 JPEG 的字节占位。
        let p = tier_path(Tier::Normal, photo_id);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"not a jpeg").unwrap();
        assert!(
            load_tier_repair(&store, Tier::Normal, photo_id).is_none(),
            "坏文件应返回 None"
        );
        assert!(!p.exists(), "坏文件应被删掉");
        let (ok, rs): (i64, i64) = store
            .test_conn()
            .query_row(
                "SELECT thumb_ok, rich_state FROM photos WHERE id=?1",
                [photo_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(ok, 0, "中档自愈应把 thumb_ok 退回 0");
        assert_eq!(rs, crate::db::RICH_PENDING as i64, "应退回待派生");
        // 好文件不被误删。
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 8));
        img.save(&p).unwrap();
        assert!(
            load_tier_repair(&store, Tier::Normal, photo_id).is_some(),
            "好文件应正常读回"
        );
        assert!(p.exists(), "好文件不该被动");
    }

    /// 孤儿 GC:盘上有文件、库里无行 → 删;有行的 → 留。
    #[test]
    fn gc_orphans_removes_only_dead_files() {
        let store = std::sync::Arc::new(crate::db::Store::open_mem().unwrap());
        let alive_id = unique_id();
        let dead_id = unique_id();
        let _g1 = CacheGuard(alive_id);
        let _g2 = CacheGuard(dead_id);
        {
            let conn = store.test_conn();
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,'/x/alive.jpg','/x',0,0)",
                rusqlite::params![alive_id],
            )
            .unwrap();
        }
        let root = std::env::temp_dir().join(format!("ml-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // gc_orphans 的根由调用方给:生产传 cache_dir(),测试传临时目录,
        // 绝不能拿真实缓存目录当试验场(会当成孤儿全清)。
        let moved = |tier: Tier, id: i64| root.join(tier.dir_name()).join(format!("{id}.jpg"));
        for (tier, id) in [(Tier::Normal, alive_id), (Tier::Normal, dead_id), (Tier::Small, dead_id)] {
            let p = moved(tier, id);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
        }
        let removed = gc_orphans(&store, &root);
        assert!(removed >= 2, "至少应清掉无主的两枚,实际 {removed}");
        assert!(
            moved(Tier::Normal, alive_id).exists(),
            "有行的缓存不该被清"
        );
        assert!(
            !moved(Tier::Normal, dead_id).exists(),
            "无主的缓存应被清"
        );
        assert!(!moved(Tier::Small, dead_id).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 档位元数据:像素尺寸、目录、LRU 上限。
    #[test]
    fn tier_metadata_is_sane() {
        assert_eq!(Tier::Small.size(), 96);
        assert_eq!(Tier::Normal.size(), 256);
        assert_eq!(Tier::Big.size(), 512);
        assert_eq!(Tier::Preview.size(), 1024);
        // 大档与小档目录不同(对应原版三张表分开记账)。
        assert_ne!(Tier::Big.dir_name(), Tier::Normal.dir_name());
        assert_ne!(Tier::Preview.dir_name(), Tier::Normal.dir_name());
        // 档位越大建议 LRU 越小(显存预算 192MB)。
        assert!(Tier::Big.suggested_cache_cap() < Tier::Normal.suggested_cache_cap());
        assert!(Tier::Small.suggested_cache_cap() > Tier::Normal.suggested_cache_cap());
        // 字节数 = 边长² × 4。
        assert_eq!(Tier::Normal.bytes(), 256 * 256 * 4);
        // from_u8 往返。
        for t in ALL_TIERS {
            assert_eq!(Tier::from_u8(t as u8), Some(t));
        }
        assert_eq!(Tier::from_u8(99), None);
    }

    /// 多档生成:各档落到各自目录,尺寸正确。
    #[test]
    fn generate_tier_writes_each_tier() {
        let dir = std::env::temp_dir().join(format!("rk-tiers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.jpg");
        let mut img = image::RgbImage::new(400, 300);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([(x % 256) as u8, (y % 256) as u8, 77]);
        }
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(&src, &buf).unwrap();

        let id = unique_id();
        let _guard = CacheGuard(id);
        purge_cache(id);
        for tier in [Tier::Small, Tier::Normal, Tier::Big] {
            generate_tier(tier, id, &src).expect("应生成该档");
            let got = load_cached_tier(tier, id).expect("应能读回该档");
            assert_eq!(got.width(), tier.size(), "{tier:?} 宽度不符");
            assert_eq!(got.height(), tier.size(), "{tier:?} 高度不符");
            // 各档路径不同。
            assert!(tier_path(tier, id).exists());
        }
        // 大档与小档是不同文件(不共用)。
        assert_ne!(
            tier_path(Tier::Small, id),
            tier_path(Tier::Big, id)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 后台 worker 按档位补齐:Normal 档回写 thumb_ok,其它档写 thumb_tiers。
    #[test]
    fn backfill_tier_marks_tier_table() {
        let dir = std::env::temp_dir().join(format!("rk-tierw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.jpg");
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(32, 24))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(&src, &buf).unwrap();

        let store = std::sync::Arc::new(crate::db::Store::open_mem().unwrap());
        let photo_id = unique_id();
        let _guard = CacheGuard(photo_id);
        {
            let conn = store.test_conn();
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,?3,0,0)",
                rusqlite::params![photo_id, src.to_string_lossy(), dir.to_string_lossy()],
            )
            .unwrap();
        }

        // Big 档(2):不写 thumb_ok,只写 thumb_tiers。
        let ids = backfill_tier(&store, Tier::Big, &[], 10).unwrap();
        assert_eq!(ids, vec![photo_id], "应生成 Big 档");
        let ok: i64 = store
            .test_conn()
            .query_row("SELECT thumb_ok FROM photos WHERE id=?1", [photo_id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(ok, 0, "Big 档不应改 thumb_ok(Normal 档专用)");
        assert_eq!(store.tier_count(Tier::Big as u8), 1, "应记入 thumb_tiers");

        // 再调一次 → 没有缺失的了(幂等)。
        let again = backfill_tier(&store, Tier::Big, &[], 10).unwrap();
        assert!(again.is_empty(), "已生成的档不应重复生成");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===== 预览缓存 =====

    /// 预览缓存:生成后可读回,尺寸 ≤ 1024。
    #[test]
    fn preview_cache_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rk-prev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.jpg");
        let mut img = image::RgbImage::new(800, 600);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([(x % 256) as u8, 128, (y % 256) as u8]);
        }
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(&src, &buf).unwrap();

        // 用进程内唯一的 id,避免与其它测试的缓存文件撞车
        // (缓存目录是全局共享的 ~/.cache/Relyto/Maliang)。
        let id = unique_id();
        let _guard = CacheGuard(id);
        purge_cache(id);
        assert!(load_preview_cached(id).is_none(), "初始应无缓存");
        build_preview(id, &src).expect("应生成预览");
        let got = load_preview_cached(id).expect("应能读回预览");
        assert!(got.width() <= PREVIEW_SIZE && got.height() <= PREVIEW_SIZE);
        // 预览落在 previews 目录(对应原版预览档缓存)。
        assert!(preview_path(id).exists());
        assert!(preview_path(id).to_string_lossy().contains("previews"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `build_preview` 现在统一走 `load_for_preview`,这里锁住该入口的取值约定:
    /// 常规格式 → 返回还需施加的 EXIF 方向(无 EXIF 的 PNG 是 1);
    /// 四条路线全失败 → `None`(预览窗显示"加载失败",而不是 panic)。
    #[test]
    fn load_for_preview_routes() {
        let dir = std::env::temp_dir().join(format!("rk-lfp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.png");
        image::RgbImage::from_pixel(300, 200, image::Rgb([10, 20, 30]))
            .save(&src)
            .unwrap();
        let (img, orient) = load_for_preview(&src, PREVIEW_SIZE).expect("常规格式应能解码");
        assert_eq!(img.width(), 300);
        // 无 EXIF 的 PNG:方向 1 = 没什么可转的,build_preview 直接落盘。
        assert_eq!(orient, 1, "无 EXIF → 方向应为 1");
        // 不存在/无法解码的文件:视频/内嵌/RAW/常规四条路线全落空 → None
        assert!(load_for_preview(&dir.join("nope.png"), PREVIEW_SIZE).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 内嵌预览的**按档尺寸门**。
    ///
    /// 旧实现是硬编码 128px,于是 160px 的 EXIF 缩略图会被拿去喂 512/1024 档,
    /// 大档看着就是一团糊 —— 这条锁住"边长要达到目标的一半"。
    #[test]
    fn embedded_preview_is_gated_by_tier_size() {
        assert!(embedded_ok(160, 120, Tier::Small.size()), "96 档:160px 够用");
        assert!(embedded_ok(160, 120, Tier::Normal.size()), "256 档:160×2=320 ≥ 256");
        assert!(!embedded_ok(160, 120, Tier::Big.size()), "512 档:160px 不够,得整图解码");
        assert!(!embedded_ok(160, 120, Tier::Preview.size()), "1024 档同上");
        assert!(embedded_ok(1600, 1200, Tier::Preview.size()));
        // 取的是**长边**:竖幅预览同样按长边判
        assert!(embedded_ok(120, 160, Tier::Normal.size()));
        assert!(!embedded_ok(0, 0, Tier::Small.size()));
    }

    /// 先缩后转:90° 旋转是纯像素置换,「缩到 S 再转」与「转完再缩到 S」
    /// 结果一致 —— 顺序换了但缩略图不能变(否则新旧库的重复图判定会对不上)。
    #[test]
    fn fit_oriented_matches_rotate_then_resize() {
        let mut src = image::RgbImage::from_pixel(400, 300, image::Rgb([0, 0, 0]));
        // 画几条不对称的横带,旋转后能被区分出来
        for y in 0..300u32 {
            for x in 0..400u32 {
                src.put_pixel(x, y, image::Rgb([(x / 8) as u8, (y / 4) as u8, 200]));
            }
        }
        let img = image::DynamicImage::ImageRgb8(src);
        for orient in [1u32, 3, 6, 8] {
            let a = fit_oriented(img.clone(), 96, orient);
            let b = downscale(apply_exif_rotation(img.clone(), orient), 96);
            assert_eq!(a.width(), b.width(), "orient={orient} 宽应一致");
            assert_eq!(a.height(), b.height(), "orient={orient} 高应一致");
            // 重采样的定点舍入可能差一两个色阶,但绝不能是"转错了方向"那种量级。
            let (pa, pb) = (a.to_rgb8(), b.to_rgb8());
            let max_diff = pa
                .pixels()
                .zip(pb.pixels())
                .map(|(x, y)| {
                    x.0.iter()
                        .zip(y.0.iter())
                        .map(|(u, v)| (*u as i32 - *v as i32).abs())
                        .max()
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0);
            assert!(max_diff <= 2, "orient={orient} 换顺序后像素应一致,最大差 {max_diff}");
        }
        // 90° 类方向要把宽高换过来:横图 → 竖图
        let r = fit_oriented(img, 96, 6);
        assert!(r.height() > r.width(), "横图转 90° 后应变成竖图");
    }

    /// 大比例缩小走粗缩 + 精缩,尺寸口径必须与直接 resize 一致。
    #[test]
    fn downscale_keeps_aspect_and_fits_box() {
        let big = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2400,
            1800,
            image::Rgb([9, 9, 9]),
        ));
        let got = downscale(big.clone(), 256);
        assert_eq!(
            (got.width(), got.height()),
            (256, 192),
            "4:3 应缩成 256×192"
        );
        let direct = big.resize(256, 256, FilterType::Triangle);
        assert_eq!(
            (got.width(), got.height()),
            (direct.width(), direct.height()),
            "粗缩一趟不得改变尺寸口径"
        );
        // 小图照旧放大(网格要铺满格子)
        let small = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(40, 30, image::Rgb([9, 9, 9])));
        let up = downscale(small, 256);
        assert_eq!((up.width(), up.height()), (256, 192));
        assert_eq!(fit_dims(4000, 3000, 512), (512, 384));
        assert_eq!(fit_dims(0, 0, 512), (1, 1), "退化尺寸不得为 0");
    }

    /// RAW 内嵌预览的摆正判定:宽高比与 IFD0 声明相反 → 相机已经转过了。
    #[test]
    fn preview_orient_uses_aspect_to_detect_pre_rotated() {
        let mk = |orient: u32, bw: u32, bh: u32| crate::exif::TiffPreview {
            bytes: Vec::new(),
            base_w: bw,
            base_h: bh,
            orient,
        };
        // IFD0 声明横幅 + Orientation=6(要转成竖) + 预览本身已是竖幅 → 不用再转
        assert_eq!(preview_orient(&mk(6, 6000, 4000), 4000, 6000), 1);
        // 预览还是横幅 → 必须转
        assert_eq!(preview_orient(&mk(6, 6000, 4000), 6000, 4000), 6);
        // 缺 IFD0 宽高标签 → 无从判定,保守按 EXIF 方向转
        assert_eq!(preview_orient(&mk(8, 0, 0), 4000, 6000), 8);
        // 不涉及换向的方向原样透传
        assert_eq!(preview_orient(&mk(3, 6000, 4000), 6000, 4000), 3);
        assert_eq!(preview_orient(&mk(1, 6000, 4000), 6000, 4000), 1);
    }

    /// 各档位的缓存路径必须两两不同 —— 否则相邻档位互相覆盖,
    /// 表现为"切小档后大档缓存失踪"。
    #[test]
    fn tier_paths_are_pairwise_distinct() {
        for (a, b) in [
            (Tier::Small, Tier::Normal),
            (Tier::Normal, Tier::Big),
            (Tier::Big, Tier::Preview),
            (Tier::Small, Tier::Big),
            (Tier::Small, Tier::Preview),
            (Tier::Normal, Tier::Preview),
        ] {
            assert_ne!(
                tier_path(a, 42),
                tier_path(b, 42),
                "{a:?} 与 {b:?} 的缓存路径不能相同"
            );
        }
    }

    /// 回归:扩展名骗人时按**魔数**认格式(坏文件台账 3,427 条里
    /// 3,340 条写着 "Illegal start bytes:8950" —— 0x89 'P' 是 PNG 的头,
    /// 被喂给了 JPEG 解码器;那些文件其实好得很)。
    #[test]
    fn decode_base_sniffs_magic_bytes_not_extension() {
        let dir = std::env::temp_dir().join(format!("maliang-decode-{}", unique_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let lying = dir.join("actually-png.jpg");
        image::DynamicImage::new_rgb8(40, 30)
            .save_with_format(&lying, image::ImageFormat::Png)
            .unwrap();
        let (img, orient) = decode_base(&lying, 256).unwrap();
        assert_eq!(
            (img.width(), img.height()),
            (40, 30),
            ".jpg 壳里的 PNG 也该解出来"
        );
        assert_eq!(orient, 1, "PNG 不带 EXIF 方向");

        // 真坏的文件仍要报错:这一刀改的是「怎么认格式」,不是「认不出就放过」。
        let junk = dir.join("junk.jpg");
        std::fs::write(&junk, b"this is not an image at all").unwrap();
        assert!(decode_base(&junk, 256).is_err(), "垃圾内容必须仍然失败");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
