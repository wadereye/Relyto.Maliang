use anyhow::Result;
use image::imageops::FilterType;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// 缩略图档位(P1-9)。
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

    /// 该档的缓存子目录名(对应原版的三张表)。
    pub fn dir_name(self) -> &'static str {
        match self {
            Tier::Small => "thumbs",
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

/// 网格默认档(原版主力档 256px)。
pub const THUMB_SIZE: u32 = 256;

/// 该档的缓存目录。
pub fn tier_dir(tier: Tier) -> PathBuf {
    crate::db::cache_dir().join(tier.dir_name())
}

/// 该档某张照片的缓存文件路径。
pub fn tier_path(tier: Tier, id: i64) -> PathBuf {
    tier_dir(tier).join(format!("{id}.jpg"))
}

fn thumb_path(id: i64) -> PathBuf {
    tier_path(Tier::Normal, id)
}

/// 若磁盘缩略图存在则返回路径;否则在哪生成由调用方决定（后台线程）。
#[allow(dead_code)]
pub fn existing(id: i64) -> Option<PathBuf> {
    let p = thumb_path(id);
    if p.exists() { Some(p) } else { None }
}

/// 读取已落盘的缩略图（不解码原图）。
///
/// P1-9 起 UI 走 `load_cached_tier`(按档位读);本函数是"默认档"的便捷包装,
/// 保留给测试与工具代码。
#[cfg_attr(not(test), allow(dead_code))]
pub fn load_cached(id: i64) -> Option<image::DynamicImage> {
    load_cached_tier(Tier::Normal, id)
}

/// 按档位读取已落盘的缩略图（不解码原图）。
pub fn load_cached_tier(tier: Tier, id: i64) -> Option<image::DynamicImage> {
    let p = tier_path(tier, id);
    image::open(&p).ok()
}

/// 加载原图用于**预览大图**（P0-7）。
///
/// 相机 RAW（`.NEF/.CR2/.ARW/.RAF` 等）image crate 解不了，
/// 这里借 Windows Shell 缩略图扩展出图（与原版一致的 COM 路线）。
/// 普通格式走 `image::open`。
///
/// Shell 也拿不到时返回 `None`（预览窗显示"加载失败"）。
///
/// P1-10 起 UI 的预览走后台 worker(`build_preview` + 落盘缓存),
/// 本函数保留给**同步**场景(如导出前取原图、测试)。
#[allow(dead_code)]
pub fn load_for_preview(src: &Path, max_side: u32) -> Option<image::DynamicImage> {
    // P2-17 视频预览:直接走 Shell 抽帧。
    if crate::video::is_video(src) {
        return decode_via_shell_sized(src, max_side);
    }
    let img = match image::open(src) {
        Ok(i) => i,
        Err(_) => decode_via_shell_sized(src, max_side)?,
    };
    Some(img)
}

/// 生成指定档位的缩略图;返回 dHash。
///
/// dHash **只在中档(Normal)计算** —— 它是全图感知哈希,与显示档位无关,
/// 没必要每档重算(原版 dHash/uid64 也只算一次存在 DB 列里)。
/// 其余档位返回 0。
pub fn generate_tier(tier: Tier, id: i64, src: &Path) -> Result<u64> {
    if tier == Tier::Normal {
        return generate(id, src);
    }
    // P2-17 视频:各档位同样走 Shell 抽帧(与 Normal 同源,仅尺寸不同)。
    if crate::video::is_video(src) {
        return match decode_via_shell_sized(src, tier.size()) {
            Some(img) => {
                let fit = img.resize(tier.size(), tier.size(), FilterType::Triangle);
                save_letterboxed(tier.size(), &fit, tier_path(tier, id))?;
                Ok(0)
            }
            None => Err(anyhow::anyhow!("视频抽帧失败(Shell 无解码器): {}", src.display())),
        };
    }
    // 优先复用更小档的缓存?不 —— 各档独立生成,避免低档糊了大档。
    let (img, from_thumb) = match crate::exif::read_jpeg_embedded_thumb(src)
        .and_then(|b| image::load_from_memory(&b).ok())
        .filter(|t| t.width() >= 128 || t.height() >= 128)
    {
        Some(t) => (t, true),
        None => {
            if crate::scan::is_raw(src) {
                match decode_via_shell_sized(src, tier.size()) {
                    Some(t) => (t, true),
                    None => (image::open(src)?, false),
                }
            } else {
                (image::open(src)?, false)
            }
        }
    };
    let flat = if from_thumb {
        img
    } else {
        apply_exif_rotation(img, read_rotation(src))
    };
    let size = tier.size();
    let fit = flat.resize(size, size, FilterType::Triangle);
    save_letterboxed(size, &fit, tier_path(tier, id))?;
    Ok(0)
}

/// 等比缩样到 256x256 内,居中贴浅灰画布(Picasa 式 letterbox)。
/// 返回 (dHash):由缩样图(letterbox 之前)计算,避免边框干扰梯度。
pub fn generate(id: i64, src: &Path) -> Result<u64> {    // P2-17 视频:走 Shell 视频缩略提供程序(系统 DirectShow 系后端)。
    // 走系统 Shell 视频缩略提供程序,进程内完成解码,免外部进程往返。
    if crate::video::is_video(src) {
        return match decode_via_shell(src) {
            Some(img) => {
                let fit = img.resize(THUMB_SIZE, THUMB_SIZE, FilterType::Triangle);
                let dhash = crate::hash::dhash_u64(&fit);
                save_letterboxed(THUMB_SIZE, &fit, thumb_path(id))?;
                Ok(dhash)
            }
            // Shell 拿不到帧(解码器缺失等)→ 显式失败,与原版
            // "missing/loading movie" 占位语义一致(媒体类型判定)。
            None => Err(anyhow::anyhow!("视频抽帧失败(Shell 无解码器): {}", src.display())),
        };
    }
    // 加速(对应原版 "Use EXIF thumbnails"):有 JPEG 内嵌 EXIF 缩略图且
    // 足够大(>=128px)时直接用作基底,免全图解码;内嵌预览已应用相机方向,
    // 不再二次旋转。dHash 与全图解码路径同一公式,结果一致。
    let (img, from_thumb) = match crate::exif::read_jpeg_embedded_thumb(src)
        .and_then(|b| image::load_from_memory(&b).ok())
        .filter(|t| t.width() >= 128 || t.height() >= 128)
    {
        Some(t) => (t, true),
        None => {
            // RAW 等 image crate 解不了的格式 → 借 Windows Shell 缩略图扩展出图
            // (与原版一致的 COM 路线)。Shell 缩略图已按
            // 方向摆正,故与内嵌预览同样不再二次旋转。
            if crate::scan::is_raw(src) {
                match decode_via_shell(src) {
                    Some(t) => (t, true),
                    None => (image::open(src)?, false),
                }
            } else {
                (image::open(src)?, false)
            }
        }
    };
    let flat = if from_thumb {
        img
    } else {
        apply_exif_rotation(img, read_rotation(src))
    };
    let fit = flat.resize(THUMB_SIZE, THUMB_SIZE, FilterType::Triangle);
    let dhash = crate::hash::dhash_u64(&fit);
    save_letterboxed(THUMB_SIZE, &fit, thumb_path(id))?;
    Ok(dhash)
}

/// 把已缩样的图居中贴到 size×size 浅灰画布并落盘(Picasa 式 letterbox)。
/// 目录不存在则先建。
fn save_letterboxed(size: u32, fit: &image::DynamicImage, path: std::path::PathBuf) -> Result<()> {
    let mut canvas = image::RgbImage::from_pixel(size, size, image::Rgb([248, 248, 248]));
    let x = ((size - fit.width()) as i64) / 2;
    let y = ((size - fit.height()) as i64) / 2;
    image::imageops::overlay(&mut canvas, &fit.to_rgb8(), x, y);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    canvas.save_with_format(path, image::ImageFormat::Jpeg)?;
    Ok(())
}

/// 用 Windows Shell 缩略图扩展解码（相机 RAW 等 image crate 解不了的格式）。
///
/// 与原版一致的做法:走 COM `IThumbnailProvider`,
/// 借系统已安装的相机厂商 RAW 解码器出图。Shell 未就绪时返回 None。
fn decode_via_shell(src: &Path) -> Option<image::DynamicImage> {
    decode_via_shell_sized(src, THUMB_SIZE)
}

/// 用 Windows Shell 缩略图扩展解码到指定尺寸。
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
    backfill_tier(store, Tier::Normal, limit)
}

/// 批量生成指定档位缺失的缩略图(P1-9)。
///
/// 返回成功生成的 id 列表。Normal 档会顺带回写 `thumb_ok=1` 与 `dhash`;
/// 其余档位只记 `thumb_tiers` 表(对应原版的三张缓存表各自独立记账)。
pub fn backfill_tier(store: &crate::db::Store, tier: Tier, limit: usize) -> Result<Vec<i64>> {
    let rows = store.missing_tier(tier as u8, limit);
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ok: Vec<(i64, u64)> = rows
        .par_iter()
        .filter_map(|(id, path)| {
            generate_tier(tier, *id, Path::new(path))
                .ok()
                .map(|h| (*id, h))
        })
        .collect();
    if ok.is_empty() {
        return Ok(Vec::new());
    }
    if tier == Tier::Normal {
        // Normal 档额外回写 thumb_ok + dhash(dhash 只在中档算)。
        let conn = store.0.lock().unwrap();
        for (id, h) in &ok {
            conn.execute(
                "UPDATE photos SET thumb_ok=1, dhash=?2 WHERE id=?1",
                rusqlite::params![id, *h as i64],
            )?;
        }
    }
    for (id, _) in &ok {
        let _ = store.mark_tier_done(*id, tier as u8);
    }
    Ok(ok.into_iter().map(|(id, _)| id).collect())
}

/// 常驻后台缩略图 worker 的对外句柄。
///
/// 持有 `JoinHandle`(退出时 join)与"完成信号"接收端。
pub struct ThumbWorker {
    handle: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    /// 每完成一批就发一次信号,携带本批生成成功的照片 id 列表。
    /// UI 据此把对应格子的占位纹理换成真图。
    pub done: mpsc::Receiver<Vec<i64>>,
    /// 当前要补齐的档位(P1-9:UI 切档 → worker 改补那一档)。
    pub tier: Arc<AtomicU8>,
}

/// 启动**常驻**缩略图后台线程(P0-1 + P1-9 多档)。
///
/// 行为(对齐原版 Picasa 的"缩略图缓存后台生成"):
/// - 启动时先补齐存量(所有 `thumb_ok=0` 的照片);
/// - 之后每 2 秒查一次库,发现新增/变更照片就继续补(扫描后自动补齐);
/// - 每批完成后通过 `done` 通道通知 UI,UI 据此清掉对应纹理缓存并重绘网格;
/// - **档位可变**:UI 改 `tier` 后,worker 下一轮就改补那一档
///   (原版切档时按需从对应档位的缓存取,缺则生成 —— 三档各自独立记账)。
///
/// 线程优先级降为 BelowNormal,不与 UI 抢核(与原版一致的做法)。
pub fn spawn_worker(store: Arc<crate::db::Store>) -> ThumbWorker {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let tier = Arc::new(AtomicU8::new(Tier::Normal as u8));
    let tier2 = tier.clone();
    let handle = std::thread::spawn(move || {
        crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);
        // 每批的规模:一轮最多 64 张,避免长时间占用锁。
        const BATCH: usize = 64;
        let mut last_tier = Tier::Normal;
        while !stop2.load(Ordering::Relaxed) {
            let cur = tier2.load(Ordering::Relaxed);
            let t = Tier::from_u8(cur).unwrap_or(Tier::Normal);
            // 档位切换 → 重新标记(UI 侧会清纹理缓存,重新加载该档)。
            if t != last_tier {
                last_tier = t;
            }
            let ids = backfill_tier(&store, t, BATCH).unwrap_or_default();
            if !ids.is_empty() {
                let _ = tx.send(ids);
                // 立刻继续下一批,直到这一轮补完。
                continue;
            }
            // 没有待生成的:睡一会再查(扫描新增照片后自动补齐)。
            std::thread::sleep(Duration::from_millis(2000));
        }
    });
    ThumbWorker {
        handle: Some(handle),
        stop,
        done: rx,
        tier,
    }
}

impl ThumbWorker {
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
        .0
        .lock()
        .unwrap()
        .query_row("SELECT path FROM photos WHERE id=?1", [id], |r| r.get(0))?;
    let h = generate(id, Path::new(&path))?;
    store
        .0
        .lock()
        .unwrap()
        .execute(
            "UPDATE photos SET thumb_ok=1, dhash=?2 WHERE id=?1",
            rusqlite::params![id, h as i64],
        )?;
    Ok(())
}

// ===== P1-10 预览缓存 =====

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
/// 流程:优先 EXIF 内嵌缩略图(≥128px)→ RAW 走 Shell 扩展 → 全图解码;
/// 最后等比缩到 1024 内、按 EXIF 方向摆正、存 jpeg。
///
/// **不计算 dHash** —— dHash 是全图感知哈希,中档已算过,预览档复用即可。
pub fn build_preview(id: i64, src: &Path) -> Result<()> {
    let (img, from_thumb) = match crate::exif::read_jpeg_embedded_thumb(src)
        .and_then(|b| image::load_from_memory(&b).ok())
        .filter(|t| t.width() >= 128 || t.height() >= 128)
    {
        Some(t) => (t, true),
        None => {
            if crate::scan::is_raw(src) {
                match decode_via_shell_sized(src, PREVIEW_SIZE) {
                    Some(t) => (t, true),
                    None => (image::open(src)?, false),
                }
            } else {
                (image::open(src)?, false)
            }
        }
    };
    let flat = if from_thumb {
        img
    } else {
        apply_exif_rotation(img, read_rotation(src))
    };
    let resized = if flat.width().max(flat.height()) > PREVIEW_SIZE {
        flat.resize(
            PREVIEW_SIZE,
            PREVIEW_SIZE,
            image::imageops::FilterType::Triangle,
        )
    } else {
        flat
    };
    let dir = tier_dir(Tier::Preview);
    let _ = std::fs::create_dir_all(&dir);
    resized.to_rgb8().save_with_format(preview_path(id), image::ImageFormat::Jpeg)?;
    Ok(())
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
                let _ = store.mark_tier_done(id, Tier::Preview as u8);
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
        for tier in ALL_TIERS {
            let _ = std::fs::remove_file(tier_path(tier, id));
        }
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
        let ifd1_at = 8 + 2 + 0 * 12 + 4;
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
        assert!(existing(tid).is_some(), "应落盘缩略图");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P0-1:`load_cached` 读的是落盘的 256px 小图,不解码原图。
    /// 生成后应能读回,且尺寸为 256×256(letterbox 画布)。
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
        let cached = load_cached(id).expect("应能读回落盘缩略图");
        assert_eq!(cached.width(), THUMB_SIZE);
        assert_eq!(cached.height(), THUMB_SIZE);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P0-1:后台 worker 会把 `thumb_ok=0` 的照片补上缩略图,并回报 id。
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
        // 手动插一行(thumb_ok=0),模拟"扫描已入库但缩略图未生成"。
        // id 走 unique_id():两个各自开 in-memory 库的测试若都用自增 id 1,
        // 会同时往同一批缓存文件上写。
        let photo_id = unique_id();
        let _guard = CacheGuard(photo_id);
        {
            let conn = store.0.lock().unwrap();
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,?3,0,0)",
                rusqlite::params![photo_id, src.to_string_lossy(), dir.to_string_lossy()],
            )
            .unwrap();
        }
        let worker = spawn_worker(store.clone());
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
        // 库里应已置 thumb_ok=1
        let ok: i64 = store
            .0
            .lock()
            .unwrap()
            .query_row("SELECT thumb_ok FROM photos WHERE path=?1", [&src.to_string_lossy()], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, 1, "应回写 thumb_ok=1");

        worker.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===== P1-9 多档缩略图 =====

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
            let conn = store.0.lock().unwrap();
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,?3,0,0)",
                rusqlite::params![photo_id, src.to_string_lossy(), dir.to_string_lossy()],
            )
            .unwrap();
        }

        // Big 档(2):不写 thumb_ok,只写 thumb_tiers。
        let ids = backfill_tier(&store, Tier::Big, 10).unwrap();
        assert_eq!(ids, vec![photo_id], "应生成 Big 档");
        let ok: i64 = store
            .0
            .lock()
            .unwrap()
            .query_row("SELECT thumb_ok FROM photos WHERE id=?1", [photo_id], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, 0, "Big 档不应改 thumb_ok(Normal 档专用)");
        assert_eq!(store.tier_count(Tier::Big as u8), 1, "应记入 thumb_tiers");

        // 再调一次 → 没有缺失的了(幂等)。
        let again = backfill_tier(&store, Tier::Big, 10).unwrap();
        assert!(again.is_empty(), "已生成的档不应重复生成");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===== P1-10 预览缓存 =====

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
}
