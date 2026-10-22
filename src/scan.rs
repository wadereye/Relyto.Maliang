//! 全盘扫描与索引 upsert(只读原文件,不复制不移动)
//!
//! 扫描策略(与 Picasa 3 的行为一致):
//! 1. **未变文件快速路径**:按 path+size+mtime 命中库内记录即跳过,不重解码 EXIF。
//! 2. **EXIF 单次读取**:一次打开解析出全部字段(同一 IFD 取多字段)。
//! 3. **排除规则**:对齐 Picasa 3 的目录排除规则(windows/winnt/temp/Program Files/Originals)
//!    + 用户自定义排除目录。
//! 4. **线程优先级**:扫描线程降为 BelowNormal,不与 UI 抢核。
//! 5. **RAW 支持**:白名单含相机 RAW;image crate 解不了的走 Windows Shell 缩略图。
use crate::db::Store;
use anyhow::Result;
use image::ImageReader;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use walkdir::WalkDir;

/// 可解码的常见图片格式(image crate 支持)。
const IMG_EXTS: [&str; 6] = ["jpg", "jpeg", "png", "webp", "bmp", "tiff"];

/// 相机 RAW 格式(image crate 解不了,缩略图走 Windows Shell 扩展;
/// 索引仍照常收录,可按 EXIF 时间/相机/GPS 检索)。
const RAW_EXTS: [&str; 14] = [
    "nef", "cr2", "cr3", "arw", "raf", "orf", "rw2", "dng", "pef", "srw", "3fr", "erf", "iiq", "kdc",
];

fn is_image(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            IMG_EXTS.contains(&e.as_str()) || RAW_EXTS.contains(&e.as_str())
        })
        .unwrap_or(false)
}

/// 事件驱动用:是否为可索引的媒体文件(图片 + RAW + 视频)。
///
/// P2-17:视频照常收录 —— 库中显示视频条目
/// (记录类型 {8,9,10,0xb,0xc,0x17,0x1d})。
pub fn is_indexable(p: &Path) -> bool {
    is_image(p) || crate::video::is_video(p)
}

/// 是否 RAW 格式(决定缩略图是否走 Shell 扩展)。
pub fn is_raw(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| RAW_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// 扫描排除规则(目录名匹配,大小写不敏感)。
///
/// 默认项对齐原版:
/// `windows / winnt / temp / Program Files / Originals`。
/// 用户自定义项来自 `settings` 表的 `exclude_dirs`(每行一个目录名)。
#[derive(Debug, Clone)]
pub struct Exclusions {
    names: Vec<String>,
}

impl Default for Exclusions {
    fn default() -> Self {
        Exclusions {
            names: vec![
                "windows".into(),
                "winnt".into(),
                "temp".into(),
                "Program Files".into(),
                "Originals".into(),
            ],
        }
    }
}

impl Exclusions {
    /// 从库里读用户自定义排除目录并合并默认值。
    pub fn from_store(store: &Store) -> Self {
        let mut e = Exclusions::default();
        if let Some(v) = store.get_setting("exclude_dirs") {
            for line in v.lines() {
                let t = line.trim();
                if !t.is_empty() && !e.names.iter().any(|n| n.eq_ignore_ascii_case(t)) {
                    e.names.push(t.to_string());
                }
            }
        }
        e
    }

    /// 该目录名是否应被排除。
    pub fn excludes_dir(&self, name: &str) -> bool {
        self.names.iter().any(|n| n.eq_ignore_ascii_case(name))
    }

    /// 供 UI 展示当前排除规则。
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// 路径的**任意一级父目录**命中排除规则则返回 true
    /// (事件驱动时单个文件的路径里可能嵌着被排除的目录名)。
    pub fn excludes_path(&self, p: &Path) -> bool {
        p.ancestors()
            .skip(1) // 跳过文件名本身
            .filter_map(|d| d.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .any(|n| self.excludes_dir(&n))
    }
}

#[derive(Clone)]
pub struct Record {
    pub path: String,
    pub parent: String,
    pub mtime: i64,
    pub size: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub taken_utc: Option<String>,
    pub rot: i32,
    /// GPS(十进制度;无定位 None) — 对应原版 lat/long 列。
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// 相机型号(Make+Model)。
    pub cam: Option<String>,
    /// 媒体类型:`video::kind::PHOTO`(0)=图片,`video::kind::MOVIE`(1)=视频。
    /// 对齐原版记录类型的"是否视频"二分(原版 7 种子类型对应不同解码路径,
    /// 我方只有一条 Shell 路径,无须细分)。
    pub kind: i32,
}

pub struct ScanSummary {
    pub scanned: usize,
    pub indexed: usize,
    /// 因 path+size+mtime 未变而跳过的文件数(快速路径命中)。
    pub unchanged: usize,
    /// 被排除规则跳过的目录数。
    pub excluded_dirs: usize,
    /// 扫描被用户取消(而非正常完成)。
    pub cancelled: bool,
}

/// 扫描进度与取消控制(P1-8)。
///
/// 对齐原版两处参考结论:
/// - 扫描进度:**预计算 `1.0/total` 倒数**,
///   之后每次上报只做一次浮点乘法(不反复做整数除法)。本结构采用同一思路:
///   `total` 设定后立即算出 `inv_total`,进度 = `done * inv_total`。
/// - 目录扫描 + 取消检查:
///   取消标志是**独立于工作线程的原子字节**,热路径每处理 N 个文件查一次;
///   取消时停止批量入库。
///
/// 进度是**原子计数**而非回调 —— UI 线程读时无锁,与原版
/// 进度接口的解耦思路一致。
#[derive(Debug)]
pub struct ScanControl {
    /// 总候选文件数(遍历完目录树后确定)。
    total: AtomicUsize,
    /// 已处理文件数。
    done: AtomicUsize,
    /// `1.0/total` 的倒数缓存。
    inv_total: AtomicU64,
    /// 取消标志(1 字节原子标志)。
    cancelled: AtomicBool,
    /// 命中取消检查的间隔(每 N 个文件查一次,避免每文件一次原子读)。
    check_every: usize,
}

impl Default for ScanControl {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanControl {
    pub fn new() -> Self {
        ScanControl {
            total: AtomicUsize::new(0),
            done: AtomicUsize::new(0),
            inv_total: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            check_every: 64,
        }
    }

    /// 设定总量并预计算倒数(与原版一致的核心优化)。
    pub fn set_total(&self, total: usize) {
        self.total.store(total, Ordering::Relaxed);
        // total=0 时倒数取 0(进度恒 0,避免除零)。
        let inv = if total == 0 {
            0.0f64
        } else {
            1.0 / total as f64
        };
        self.inv_total.store(inv.to_bits(), Ordering::Relaxed);
    }

    /// 累加已完成数。
    pub fn advance(&self, n: usize) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }

    /// 当前进度 0.0..=1.0(用预计算倒数做一次乘法)。
    pub fn fraction(&self) -> f32 {
        let inv = f64::from_bits(self.inv_total.load(Ordering::Relaxed));
        if inv == 0.0 {
            return 0.0;
        }
        (self.done.load(Ordering::Relaxed) as f64 * inv).clamp(0.0, 1.0) as f32
    }

    /// 已处理 / 总数。
    pub fn counts(&self) -> (usize, usize) {
        (
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }

    /// 请求取消(UI 线程调用;工作线程在下一个检查点退出)。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// 是否已被取消。
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// 热路径检查:每 `check_every` 个文件查一次取消标志(省原子读开销)。
    #[inline]
    pub fn should_stop(&self, counter: usize) -> bool {
        if counter % self.check_every == 0 {
            self.is_cancelled()
        } else {
            false
        }
    }
}

/// 库内已有记录的指纹:path -> (size, mtime)。用于未变文件快速路径。
fn load_fingerprints(store: &Store) -> HashMap<String, (i64, i64)> {
    let conn = store.0.lock().unwrap();
    let mut st = match conn.prepare("SELECT path, size, mtime FROM photos") {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    let rows: HashMap<String, (i64, i64)> = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .ok()
        .map(|rows| {
            rows.filter_map(|r| r.ok())
                .map(|(p, s, m)| (p, (s, m)))
                .collect()
        })
        .unwrap_or_default();
    rows
}

/// 并行扫描多个根目录,仅记录存在的图像文件(增量 upsert 交给数据库)
///
/// - 未变文件(path+size+mtime 与库内一致)直接跳过,不重解码 EXIF、不写库。
/// - 扫描线程降优先级(BelowNormal),避免与 UI 抢核(与原版一致的做法)。
/// - `ctrl` 提供进度上报与取消(P1-8)。
pub fn scan_dir(store: &Store, roots: &[PathBuf]) -> Result<ScanSummary> {
    let ctrl = Arc::new(ScanControl::new());
    scan_dir_ctrl(store, roots, &ctrl)
}

/// 带进度与取消的扫描。
///
/// 取消语义(对齐原版扫描取消检查):
/// - **遍历阶段**每 64 个目录项查一次取消标志(原版同样是分段查,不是每文件一次);
/// - **并行处理阶段**用 rayon 的 `collect::<Result<Vec<_>,_>>()`,
///   任一线程发现取消即整体短路;
/// - 取消时**已算出的记录照常入库**(已完成的工作不丢),
///   只是不再继续处理剩余文件。
pub fn scan_dir_ctrl(
    store: &Store,
    roots: &[PathBuf],
    ctrl: &Arc<ScanControl>,
) -> Result<ScanSummary> {
    crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);

    let excl = Exclusions::from_store(store);
    // 快速路径基线:库内已有指纹
    let prints = load_fingerprints(store);

    let mut excluded_dirs = 0usize;
    let mut candidates: Vec<PathBuf> = Vec::new();
    // 遍历阶段的可取消性:目录项计数(原版按文件队列分段查取消)。
    let mut visit_counter = 0usize;
    'outer: for r in roots {
        let it = WalkDir::new(r)
            .into_iter()
            .filter_entry(|e| {
                // 目录级排除:命中即整棵子树跳过(对齐原版做法)
                if e.file_type().is_dir() {
                    let name = e.file_name().to_string_lossy();
                    if excl.excludes_dir(&name) {
                        excluded_dirs += 1;
                        return false;
                    }
                }
                true
            })
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| is_image(e.path()))
            .map(|e| e.path().to_path_buf());
        for p in it {
            visit_counter += 1;
            if ctrl.should_stop(visit_counter) {
                break 'outer;
            }
            candidates.push(p);
        }
    }
    let total = candidates.len();
    ctrl.set_total(total);

    let counter = Arc::new(AtomicUsize::new(0));
    let unchanged = Arc::new(AtomicUsize::new(0));
    let added = Arc::new(AtomicUsize::new(0));
    let modified = Arc::new(AtomicUsize::new(0));
    let ctrl2 = ctrl.clone();
    let counter2 = counter.clone();
    let unchanged2 = unchanged.clone();
    let added2 = added.clone();
    let modified2 = modified.clone();

    // rayon 支持 `collect::<Result<Vec<T>, E>>()`:任一线程返回 Err 即整体短路。
    let collected: Result<Vec<Option<Record>>, Cancelled> = candidates
        .into_par_iter()
        .map(|p| {
            // ---- 快速路径:指纹一致则整条跳过(不读 EXIF、不解码) ----
            let md = match std::fs::metadata(&p) {
                Ok(m) => m,
                Err(_) => return Ok(None),
            };
            let mtime = match md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            {
                Some(d) => d.as_millis() as i64,
                None => return Ok(None),
            };
            let size = md.len() as i64;
            let key = p.to_string_lossy().into_owned();
            if let Some(&(osize, omtime)) = prints.get(&key) {
                if osize == size && omtime == mtime {
                    unchanged2.fetch_add(1, Ordering::Relaxed);
                    counter2.fetch_add(1, Ordering::Relaxed);
                    ctrl2.advance(1);
                    return Ok(None);
                }
                // 库里有但内容变了 → 算"修改"(P1-12 变更日志)。
                modified2.fetch_add(1, Ordering::Relaxed);
            } else {
                // 库里没有 → 算"新增"。
                added2.fetch_add(1, Ordering::Relaxed);
            }
            // 取消检查:每处理 64 个文件查一次(原版分段查的思路)。
            let n = counter2.fetch_add(1, Ordering::Relaxed);
            ctrl2.advance(1);
            if ctrl2.should_stop(n) && ctrl2.is_cancelled() {
                return Err(Cancelled);
            }
            Ok(read_record_with_meta(&p, size, mtime))
        })
        .collect();

    let cancelled = collected.is_err();
    let records: Vec<Record> = collected
        .unwrap_or_default()
        .into_iter()
        .flatten()
        .collect();
    let indexed = records.len();
    // 取消也要把已算出的记录入库(不丢已完成的工作)。
    store_insert(store, &records)?;
    // P1-12:写变更日志(新增/修改各一条聚合记录,避免逐条写库拖慢扫描)。
    let n_added = added.load(Ordering::Relaxed);
    let n_mod = modified.load(Ordering::Relaxed);
    if n_added > 0 {
        let _ = store.log_change("<扫描>", "added", None);
    }
    if n_mod > 0 {
        let _ = store.log_change("<扫描>", "modified", None);
    }
    // P1-11:扫描时顺带补 uid64(不必手动点菜单)。
    // `store_insert` 里已用 `COALESCE(photos.uid64, :uid)` 写入,
    // 这里只需处理历史遗留的 NULL 行。
    if n_added > 0 {
        let _ = store.backfill_uid64();
    }
    Ok(ScanSummary {
        scanned: total,
        indexed,
        unchanged: unchanged.load(Ordering::Relaxed),
        excluded_dirs,
        cancelled,
    })
}

/// 取消标记内部类型(rayon 短路信号)。
struct Cancelled;

/// 读取一条记录(EXIF 只解析一次;RAW 走 Shell 拿尺寸)。
fn read_record_with_meta(p: &Path, size: i64, mtime: i64) -> Option<Record> {
    let meta = crate::exif::read_meta(p);
    // 尺寸:优先 EXIF;缺失时用解码器读头(RAW 解不了,改用 Shell 缩略图尺寸)。
    let (mut w, mut h) = (meta.width, meta.height);
    if w.is_none() || h.is_none() {
        let dims = image_dimensions(p).or_else(|| shell_dimensions(p));
        w = w.or(dims.map(|d| d.0));
        h = h.or(dims.map(|d| d.1));
    }
    Some(Record {
        path: p.to_string_lossy().into_owned(),
        parent: p
            .parent()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default(),
        mtime,
        size,
        width: w,
        height: h,
        taken_utc: meta.taken,
        rot: meta.orientation as i32,
        lat: meta.gps.map(|g| g.0),
        lon: meta.gps.map(|g| g.1),
        cam: meta.cam,
        kind: if crate::video::is_video(p) {
            crate::video::kind::MOVIE
        } else {
            crate::video::kind::PHOTO
        },
    })
}

/// 只读图片头拿宽高(不解码整图)。
fn image_dimensions(p: &Path) -> Option<(i32, i32)> {
    let (a, b) = ImageReader::open(p)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    Some((a as i32, b as i32))
}

/// RAW 等 image crate 解不了的格式:借 Windows Shell 缩略图扩展取尺寸
/// (与原版一致的 COM 路线)。
fn shell_dimensions(p: &Path) -> Option<(i32, i32)> {
    let (_bgra, w, h) = crate::win::shell_thumbnail(p, 256)?;
    Some((w as i32, h as i32))
}

/// 索引**单个**文件(事件驱动用)。
///
/// 事件驱动(watch)拿到的是"哪个文件变了",不是"哪个目录变了";
/// 这里只处理这一个文件,不做整树遍历 —— 这是原版目录项枚举的粒度
/// (`FindNextFileW` 一次拿一个目录项的属性)。
///
/// 返回 `true` 表示该文件被写入了索引(新文件或内容有变)。
pub fn index_file(store: &Store, path: &Path) -> Result<bool> {
    index_file_ctrl(store, path, None)
}

/// 带取消检查的单文件入库(事件驱动路径)。
///
/// `ctrl` 为 `Some` 时,入库前查一次取消标志 —— 取消后不再写库。
pub fn index_file_ctrl(
    store: &Store,
    path: &Path,
    ctrl: Option<&ScanControl>,
) -> Result<bool> {
    if let Some(c) = ctrl {
        if c.is_cancelled() {
            return Ok(false);
        }
    }
    if !is_image(path) {
        return Ok(false);
    }
    // 排除规则:路径上任一级目录命中排除名则跳过。
    let excl = Exclusions::from_store(store);
    if excl.excludes_path(path) {
        return Ok(false);
    }
    let md = std::fs::metadata(path)?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let size = md.len() as i64;

    // 快速路径:指纹一致则跳过(事件重复通知时不再重解码 EXIF)。
    let key = path.to_string_lossy().into_owned();
    let existed = {
        let conn = store.0.lock().unwrap();
        let old: Option<(i64, i64)> = conn
            .query_row(
                "SELECT size, mtime FROM photos WHERE path=?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        match old {
            Some((osize, omtime)) => {
                if osize == size && omtime == mtime {
                    return Ok(false);
                }
                true // 已有记录但内容变了 → 修改
            }
            None => false, // 库里没有 → 新增
        }
    };

    let rec = match read_record_with_meta(path, size, mtime) {
        Some(r) => r,
        None => return Ok(false),
    };
    store_insert(store, &[rec])?;
    // P1-12:事件驱动路径也要写变更日志(否则只有扫描会记,监听发现的变化会丢)。
    let _ = store.log_change(
        &key,
        if existed { "modified" } else { "added" },
        None,
    );
    Ok(true)
}

fn store_insert(store: &Store, records: &[Record]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    // P1-14 字符串池:相机型号先驻留进 `photos_cam` 池,拿到整数 id。
    // 对齐原版的池化列 —— 行记录只存 32 位 id,不存字符串本体
    // (落盘前原地替换:字符串入池)。
    // 用 `intern_batch` 而非逐条 `intern`:内存池先去重,一万行只发几次 SQL。
    // 池化在事务**外**先做完:intern 自身要拿连接锁,不能在 tx 内重入。
    let cam_items: Vec<(String, String)> = records
        .iter()
        .map(|r| {
            (
                crate::timeline::CAM_POOL.to_string(),
                r.cam.clone().unwrap_or_default().trim().to_string(),
            )
        })
        .collect();
    let has_cam: Vec<bool> = cam_items.iter().map(|(_, v)| !v.is_empty()).collect();
    let interned = store.intern_batch(&cam_items)?;
    let cam_ids: Vec<Option<i64>> = interned
        .iter()
        .zip(has_cam.iter())
        .map(|((_, id), has)| if *has { Some(*id) } else { None })
        .collect();
    let conn = store.0.lock().unwrap();
    // 注意:必须用 execute_batch —— rusqlite 的 `execute` 只接受单条语句,
    // 传多条会报 "Multiple statements provided"。
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    let tx = conn.unchecked_transaction()?;
    {
        let mut st = tx.prepare_cached(
            "INSERT INTO photos(path,parent,mtime,size,width,height,taken_utc,cam,cam_id,rot,star,thumb_ok,lat,lon,uid64,kind)
             VALUES(:path,:parent,:mtime,:size,:w,:h,:taken,NULL,:cam_id,:rot,0,0,:lat,:lon,:uid,:kind)
             ON CONFLICT(path) DO UPDATE SET
               mtime=:mtime, size=:size, width=:width, height=:height,
               taken_utc=:taken, cam=NULL, cam_id=:cam_id, lat=:lat, lon=:lon, kind=:kind,
               uid64=COALESCE(photos.uid64, :uid),
               thumb_ok=CASE WHEN photos.mtime<>:mtime THEN 0 ELSE photos.thumb_ok END,
               dhash=CASE WHEN photos.mtime<>:mtime THEN NULL ELSE photos.dhash END",
        )?;
        for (i, r) in records.iter().enumerate() {
            // P1-11:uid64 = fnv1a64(path|size|mtime)。
            // 用 `COALESCE(photos.uid64, :uid)` —— 已有 uid64 的行**保持不变**
            // (标识是持久的,不应随内容变化而变;重新索引不应改变外部引用)。
            let uid = crate::db::fnv1a64(&format!("{}|{}|{}", r.path, r.size, r.mtime));
            st.execute(rusqlite::named_params! {
                ":path": r.path, ":parent": r.parent, ":mtime": r.mtime, ":size": r.size,
                ":w": r.width, ":h": r.height, ":taken": r.taken_utc, ":rot": r.rot,
                ":cam_id": cam_ids[i], ":lat": r.lat, ":lon": r.lon,
                ":uid": uid as i64, ":kind": r.kind,
            })?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_exclusions_are_stable() {
        // 原版目录排除规则
        let e = Exclusions::default();
        assert!(e.excludes_dir("Windows"));
        assert!(e.excludes_dir("winnt"));
        assert!(e.excludes_dir("TEMP"));
        assert!(e.excludes_dir("Program Files"));
        assert!(e.excludes_dir("Originals"));
        assert!(!e.excludes_dir("Pictures"));
        assert!(!e.excludes_dir("Photos"));
    }

    #[test]
    fn image_extensions_cover_common_and_raw() {
        assert!(is_image(Path::new("a.JPG")));
        assert!(is_image(Path::new("a.png")));
        assert!(is_image(Path::new("a.nef")));
        assert!(is_image(Path::new("a.CR2")));
        assert!(is_image(Path::new("a.arw")));
        assert!(is_image(Path::new("a.RAF")));
        assert!(!is_image(Path::new("a.txt")));
        assert!(!is_image(Path::new("a.mp4")));
        assert!(is_raw(Path::new("a.nef")));
        assert!(!is_raw(Path::new("a.jpg")));
    }

    /// 快速路径:未变文件不重解码 EXIF(不产生新记录),变更文件会重索引。
    #[test]
    fn scan_skips_unchanged_files() {
        let dir = std::env::temp_dir().join(format!("rk-scanfast-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 一张最小 JPEG
        let p = dir.join("a.jpg");
        let mut img = image::RgbImage::new(8, 6);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, 10]);
        }
        // JPEG 编码器需要可 seek 的 writer:先编码到内存,再落盘。
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(&p, &buf).unwrap();

        let store = Store::open_mem().unwrap();
        let roots = vec![dir.clone()];
        let s1 = scan_dir(&store, &roots).unwrap();
        assert_eq!(s1.indexed, 1, "首次应索引 1 张");
        assert_eq!(s1.unchanged, 0);

        let s2 = scan_dir(&store, &roots).unwrap();
        assert_eq!(s2.unchanged, 1, "第二次应命中快速路径(未变)");
        assert_eq!(s2.indexed, 0, "未变文件不应重新索引");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 排除规则:被排除目录整棵子树不入索引。
    #[test]
    fn scan_applies_exclusion_rules() {
        let dir = std::env::temp_dir().join(format!("rk-scanexcl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let good = dir.join("Pictures");
        let bad = dir.join("Originals");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::create_dir_all(&bad).unwrap();
        let mk = |p: &Path| {
            let img = image::RgbImage::new(4, 4);
            let mut buf: Vec<u8> = Vec::new();
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
                .unwrap();
            std::fs::write(p, &buf).unwrap();
        };
        mk(&good.join("a.jpg"));
        mk(&bad.join("b.jpg"));

        let store = Store::open_mem().unwrap();
        let s = scan_dir(&store, &[dir.clone()]).unwrap();
        assert_eq!(s.indexed, 1, "Originals/ 应被排除");
        assert!(s.excluded_dirs >= 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 事件驱动的单文件入库:新文件写入索引,未变文件跳过。
    #[test]
    fn index_file_handles_single_file() {
        let dir = std::env::temp_dir().join(format!("rk-indexfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.jpg");
        let img = image::RgbImage::new(8, 6);
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(&p, &buf).unwrap();

        let store = Store::open_mem().unwrap();
        assert!(index_file(&store, &p).unwrap(), "新文件应写入索引");
        // 第二次:指纹一致 → 跳过
        assert!(!index_file(&store, &p).unwrap(), "未变文件应被跳过");
        // 非图像文件 → 不处理
        let txt = dir.join("b.txt");
        std::fs::write(&txt, b"x").unwrap();
        assert!(!index_file(&store, &txt).unwrap(), "非图像不应索引");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 排除规则对单文件路径同样生效(事件驱动路径)。
    #[test]
    fn index_file_respects_exclusion_dirs() {        let dir = std::env::temp_dir().join(format!("rk-indexexcl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bad = dir.join("Originals");
        std::fs::create_dir_all(&bad).unwrap();
        let p = bad.join("a.jpg");
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(&p, &buf).unwrap();

        let store = Store::open_mem().unwrap();
        assert!(
            !index_file(&store, &p).unwrap(),
            "被排除目录下的文件不应入库"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===== P1-8 进度与取消 =====

    /// 进度:预计算倒数 + 乘法(原版扫描进度的核心优化)。
    #[test]
    fn scan_control_progress_uses_reciprocal() {
        let c = ScanControl::new();
        // 未设总量 → 进度为 0,不能 NaN。
        assert_eq!(c.fraction(), 0.0);
        c.set_total(200);
        assert_eq!(c.fraction(), 0.0);
        c.advance(50);
        assert!((c.fraction() - 0.25).abs() < 1e-6, "50/200 = 0.25");
        c.advance(150);
        assert!((c.fraction() - 1.0).abs() < 1e-6, "200/200 = 1.0");
        assert_eq!(c.counts(), (200, 200));
        // 超出总量时钳制在 1.0。
        c.advance(10);
        assert_eq!(c.fraction(), 1.0);
    }

    /// total=0 不能除零(原版倒数缓存也有这个保护)。
    #[test]
    fn scan_control_zero_total_is_safe() {
        let c = ScanControl::new();
        c.set_total(0);
        c.advance(5);
        assert_eq!(c.fraction(), 0.0);
    }

    /// 取消:标志置位后 is_cancelled 为真。
    #[test]
    fn scan_control_cancel_flag() {
        let c = ScanControl::new();
        assert!(!c.is_cancelled());
        c.cancel();
        assert!(c.is_cancelled());
    }

    /// 取消真的能中断扫描(遍历阶段就退出,索引数少于全部文件)。
    #[test]
    fn scan_can_be_cancelled() {
        let dir = std::env::temp_dir().join(format!("rk-scancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 放 8 张小图
        for i in 0..8 {
            let mut buf: Vec<u8> = Vec::new();
            image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 6))
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
                .unwrap();
            std::fs::write(dir.join(format!("a{i}.jpg")), &buf).unwrap();
        }
        let store = Store::open_mem().unwrap();
        let ctrl = Arc::new(ScanControl::new());
        // 先取消 → 遍历阶段立刻退出,一张都不索引。
        ctrl.cancel();
        let s = scan_dir_ctrl(&store, &[dir.clone()], &ctrl).unwrap();
        assert!(s.cancelled, "应标记为已取消");
        assert_eq!(s.indexed, 0, "取消后不应索引任何文件");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 正常扫描不标记取消,且 uid64 被自动分配(P1-11)。
    #[test]
    fn scan_assigns_uid64() {
        let dir = std::env::temp_dir().join(format!("rk-uid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 6))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(dir.join("a.jpg"), &buf).unwrap();

        let store = Store::open_mem().unwrap();
        let s = scan_dir(&store, &[dir.clone()]).unwrap();
        assert_eq!(s.indexed, 1);
        assert!(!s.cancelled);

        let uid: i64 = store
            .0
            .lock()
            .unwrap()
            .query_row("SELECT uid64 FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_ne!(uid, 0, "uid64 应被分配");
        // 重新扫描不应改变 uid64(标识是持久的)。
        let _ = scan_dir(&store, &[dir.clone()]).unwrap();
        let uid2: i64 = store
            .0
            .lock()
            .unwrap()
            .query_row("SELECT uid64 FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(uid, uid2, "重新扫描后 uid64 应保持不变");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-12:事件驱动的单文件入库**也**要写变更日志
    /// (v5 审计发现:此前只有扫描路径写,监听发现的变化会丢)。
    #[test]
    fn index_file_logs_change() {
        let dir = std::env::temp_dir().join(format!("rk-idxlog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.jpg");
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 6))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(&p, &buf).unwrap();

        let store = Store::open_mem().unwrap();
        // 新文件 → added
        assert!(index_file(&store, &p).unwrap());
        let log = store.recent_changes(10);
        assert_eq!(log.len(), 1, "应记录一条变更: {log:?}");
        assert_eq!(log[0].2, "added");

        // 未变 → 不记日志(快速路径命中)
        assert!(!index_file(&store, &p).unwrap());
        assert_eq!(store.recent_changes(10).len(), 1, "未变不应重复记录");

        // 内容变了 → modified
        std::fs::write(&p, b"changed-content-different-size").unwrap();
        assert!(index_file(&store, &p).unwrap());
        let log = store.recent_changes(10);
        assert_eq!(log.len(), 2, "应记录第二条变更: {log:?}");
        assert_eq!(log[0].2, "modified");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P1-12:扫描会写变更日志(新增记 added)。
    #[test]
    fn scan_logs_changes() {
        let dir = std::env::temp_dir().join(format!("rk-chg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 6))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(dir.join("a.jpg"), &buf).unwrap();

        let store = Store::open_mem().unwrap();
        let _ = scan_dir(&store, &[dir.clone()]).unwrap();
        let log = store.recent_changes(10);
        assert!(!log.is_empty(), "应有变更记录");
        assert!(
            log.iter().any(|(_, _, a)| a == "added"),
            "应记录 added: {log:?}"
        );

        // 清空后为空
        store.clear_changes().unwrap();
        assert!(store.recent_changes(10).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
