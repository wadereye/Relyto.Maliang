//! 全盘扫描与索引 upsert(只读原文件,不复制不移动)
//!
//! 扫描策略:
//! 1. **未变文件快速路径**:按 path+size+mtime 命中库内记录即跳过,不重解码 EXIF。
//! 2. **EXIF 单次读取**:一次打开解析出全部字段(同一 IFD 取多字段)。
//! 3. **排除规则**:目录排除规则(windows/winnt/temp/Program Files/Originals)
//!    + 用户自定义排除目录。
//! 4. **线程优先级**:扫描线程降为 BelowNormal,不与 UI 抢核。
//! 5. **RAW 支持**:白名单含相机 RAW;image crate 解不了的走 Windows Shell 缩略图。
//! 6. **只读速看预览**:GIF 与动图 WebP 逐帧播放、PSD/PSB 出合成图,都不进改写链路。
use crate::db::Store;
use anyhow::Result;
use image::ImageReader;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use walkdir::WalkDir;

/// 可由 `image` crate 出图的常见格式。缩略图只取首帧,动图逐帧播放交给单图预览。
pub const IMG_EXTS: [&str; 7] = ["jpg", "jpeg", "png", "webp", "bmp", "tiff", "gif"];

/// 相机 RAW 格式(image crate 解不了,缩略图走 Windows Shell 扩展;
/// 索引仍照常收录,可按 EXIF 时间/相机/GPS 检索)。
pub const RAW_EXTS: [&str; 14] = [
    "nef", "cr2", "cr3", "arw", "raf", "orf", "rw2", "dng", "pef", "srw", "3fr", "erf", "iiq", "kdc",
];

/// 是否本系统支持预览的图片(常规格式 + RAW + PSD 家族)。
/// 快速查找页双击路由用:命中就走图库预览,否则交给系统关联程序。
pub fn is_image(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            IMG_EXTS.contains(&e.as_str())
                || RAW_EXTS.contains(&e.as_str())
                || crate::psd_preview::is_psd_ext(&e)
        })
        .unwrap_or(false)
}

/// 只读速看预览的格式:可以索引、可以出缩略图与预览,但绝不进入会改写原图的
/// 链路。GIF / 动图 WebP 的帧序列与二值透明、PSD 的图层与色彩模式都经不起
/// JPEG 重编码。静态 WebP 仍按普通图片对待。
pub fn is_readonly_media(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            matches!(e.as_str(), "gif" | "psd" | "psb")
                || (e == "webp" && is_animated_webp(p))
        })
        .unwrap_or(false)
}

/// 动图 WebP 的 VP8X 扩展头里,flags 的第 1 位表示含动画块。
/// 只读 30 字节:这个判定会在打开预览时反复调用,不能整文件解码。
fn is_animated_webp(p: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(p) else {
        return false;
    };
    let mut head = [0u8; 30];
    if f.read_exact(&mut head).is_err() {
        return false;
    }
    head.starts_with(b"RIFF")
        && &head[8..12] == b"WEBP"
        && &head[12..16] == b"VP8X"
        && (head[20] & 0x02) != 0
}

/// 事件驱动用:是否为可索引的媒体文件(图片 + RAW + 视频)。
///
/// 视频照常收录 —— 库中显示视频条目
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

/// 小写扩展名(不含点)是否落在媒体池里(图片 ∪ RAW ∪ 视频)。
///
/// 派生门拿到的是 L1 已经存好的 `ext` 列,不必为了判一次类型再拼出 `Path`。
pub fn is_media_ext(ext: &str) -> bool {
    IMG_EXTS.contains(&ext)
        || RAW_EXTS.contains(&ext)
        || crate::psd_preview::is_psd_ext(ext)
        || crate::video::MOVIE_EXTS.contains(&ext)
}

/// 初始化时依次入集的 4 条排除,**不可关**。
///
/// 单独成组而不是混进 [`Exclusions::default`]:这 4 条是硬编码口径,
/// 用户设置只能往上加、不能减;把它们命名出来,快路(MFT 展开)与慢路
/// (walkdir)才可能共用同一份判定而不各自抄一遍。
pub const HARD_EXCLUDES: [&str; 4] = ["thumbs", "RECYCLER", "Originals", ".picasaoriginals"];

/// 本项目扩充的**硬**排除:按定义里面不会有用户的照片,任何入口都不放行,
/// 而且**根本不进库**(见 `db::Store::purge_hard_excluded_once`)。
///
/// `RECYCLER` 的现代对应物 `$RECYCLE.BIN` 继承同一档硬度:回收站里的东西是
/// 用户已经判了死刑的,不该因为「显式导入」就变成照片库。系统目录、程序目录、
/// 依赖目录同理 —— 它们的内容 mtime 永远最新,收进来只会让「最近更新」被
/// 永远不亮的灰格子占满(首屏 200 张里 190 张是自家 thumbs)。
///
/// 硬层的门槛是「**按定义**不可能是用户的照片」。`temp` 不满足这条(微信把
/// 收藏图片放在 `xwechat_files\...\favorite\temp` 里),所以它在软层。
pub const HARD_EXCLUDES_OURS: [&str; 11] = [
    "$RECYCLE.BIN",
    "System Volume Information",
    "Recovery",
    "$WINDOWS.~BT",
    "$WINDOWS.~WS",
    "windows",
    "winnt",
    "Program Files",
    "Program Files (x86)",
    "node_modules",
    // 构建/测试用的夹具目录:里面是程序自己批量造的假图,不是素材。
    "rk-scratch",
];

/// 本项目扩充的**软**排除:默认不派生,但用户亲手指向(导入页选中它、对着占位格
/// 右键「纳入此目录」)时放行。
///
/// `AppData` 不能一刀切 —— 微信 / QQ / 网盘客户端的接收目录
/// 都在那儿,里面确实有用户要的图。软层的意思是「不主动收,但给出口」。
pub const SOFT_EXCLUDES: [&str; 4] = ["AppData", "temp", "npm-cache", "local-cache"];

/// 排除规则的层次(决定「用户亲手指向一个目录」能不能压过它,
/// 以及这一行要不要留在库里)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExcludeLayer {
    /// 任何入口都不放行,而且不进库:那 4 条硬编码 +
    /// [`HARD_EXCLUDES_OURS`] + 点开头的隐藏目录 + 自家产出目录。
    Hard,
    /// [`SOFT_EXCLUDES`] 与用户自定义排除项 —— **用户亲手指向时让路**。
    ///
    /// 被动命中(全盘发现、滚到过、屏幕上出现过)照样挡住,但行留在库里
    /// 当占位,右键菜单里有「纳入此目录」这个出口。
    Soft,
}

/// 扫描排除规则(目录名匹配,大小写不敏感)。
///
/// 默认排除列表只有 4 项(见 [`HARD_EXCLUDES`])。
/// 它不踩系统目录的真正原因是**默认扫描范围极窄**(只登记
/// 我的文档 / 图片 / 桌面三个壳目录,见 `db::default_roots` 注释),
/// 而非靠排除规则拦截。
///
/// 默认表是**刻意超集**,并分两档(见 [`ExcludeLayer`]):
/// [`HARD_EXCLUDES_OURS`] 里的目录(回收站现代名 $RECYCLE.BIN、系统与程序
/// 目录、依赖目录)按定义不会有用户的照片 —— 连库都不进;
/// [`SOFT_EXCLUDES`] 里的(`AppData` 一类)默认不派生,但用户亲手指向时放行。
/// 另有点开头的隐藏目录一律排除,归硬档(见 [`Exclusions::exclude_layer_dir`])。
/// 用户自定义项来自 `settings` 表的 `exclude_dirs`(每行一个目录名),归软档。
///
/// 还有一类**不能按名字排**的:[`own_prefixes`](自有数据目录前缀)。缩略图缓存
/// 落在哪儿取决于用户把数据目录 / 缓存位置设成了什么,把它写死成
/// `smallthumbs` 这类名字会误伤同名的用户相册(自己生成缩略图的
/// 目录必须排除)。所以缓存目录走**整级路径前缀**命中,且跟着 `MALIANG_CACHE_DIR`
/// / `data.location` 走 —— 用户搬缓存,排除项跟着搬。
#[derive(Debug, Clone)]
pub struct Exclusions {
    names: Vec<String>,
    /// 自有目录前缀(整级命中才算,`D:\maliang\cache` 不罩 `D:\maliang\cached`)。
    own_prefixes: Vec<PathBuf>,
}

impl Default for Exclusions {
    fn default() -> Self {
        let mut names: Vec<String> = HARD_EXCLUDES.iter().map(|s| s.to_string()).collect();
        // —— 扩充层:硬层(不进库)与软层(不主动派生,但用户表态可放行) ——
        names.extend(
            HARD_EXCLUDES_OURS
                .iter()
                .chain(SOFT_EXCLUDES.iter())
                .map(|s| s.to_string()),
        );
        Exclusions {
            names,
            own_prefixes: Vec::new(),
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
        // 自家写缩略图的目录:跟着 `MALIANG_CACHE_DIR` / `data.location` 走,
        // 用户搬缓存,排除项跟着搬。
        for p in crate::db::own_cache_dirs() {
            e = e.with_own_prefix(p);
        }
        e
    }

    /// 该目录名是否应被排除。
    ///
    /// 除显式名单外,**点开头的隐藏目录**(.git / .config / .workbuddy 等)
    /// 一律排除 —— 工具与版本库的内部数据不属于照片集。
    pub fn excludes_dir(&self, name: &str) -> bool {
        if name.starts_with('.') {
            return true;
        }
        self.names.iter().any(|n| n.eq_ignore_ascii_case(name))
    }

    /// 供 UI 展示当前排除规则。
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// 当前自有目录前缀(给 UI 展示「自家缓存目录」)。
    pub fn own_prefixes(&self) -> &[PathBuf] {
        &self.own_prefixes
    }

    /// 链式追加一个自有前缀 —— 单测用它注入固定路径,免得断言跟着
    /// 用户的 `data.location` 漂移。
    pub fn with_own_prefix(mut self, p: impl AsRef<Path>) -> Self {
        self.own_prefixes.push(p.as_ref().to_path_buf());
        self
    }

    /// 粗筛:该目录名是否**可能**是某个自有目录的最后一级。
    ///
    /// 整级前缀判定要完整路径,而 MFT 快路里完整路径得沿父链现拼 ——
    /// 每个目录都拼一次会把百万级开销加在快路上。先按名筛一遍,
    /// 只有撞上自有目录最后一级同名的才值得去拼路径核对。
    pub fn own_leaf_may_match(&self, name: &str) -> bool {
        if self.own_prefixes.is_empty() {
            return false;
        }
        let key = name.to_ascii_uppercase();
        self.own_prefixes.iter().any(|p| {
            p.file_name()
                .map(|f| f.to_string_lossy().to_ascii_uppercase() == key)
                .unwrap_or(false)
        })
    }

    /// 该**字符串**路径是否落在某个自有目录内(整级命中)。
    pub fn under_own_prefix_str(&self, path: &str) -> bool {
        let up = path_key(path);
        self.own_prefixes
            .iter()
            .any(|b| under_key(&up, &path_key(&b.to_string_lossy())))
    }

    /// [`Exclusions::under_own_prefix_str`] 的 `Path` 版。
    fn under_own_prefix(&self, p: &Path) -> bool {
        self.under_own_prefix_str(&p.to_string_lossy())
    }

    /// 路径的**任意一级父目录**命中排除规则则返回 true
    /// (事件驱动时单个文件的路径里可能嵌着被排除的目录名)。
    pub fn excludes_path(&self, p: &Path) -> bool {
        self.under_own_prefix(p)
            || p.ancestors()
                .skip(1) // 跳过文件名本身
                .filter_map(|d| d.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .any(|n| self.excludes_dir(&n))
    }

    /// 目录路径的每一级(含自身)命中排除规则则返回 true。
    ///
    /// 只是 [`Exclusions::exclude_layer_dir`] 的布尔视图:生产代码要看**层**
    /// (硬层不可覆盖),测试只问「有没有命中」。判定逻辑只留一份 ——
    /// 两处各自走一遍名单正是漏判的来源。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn excludes_dir_path(&self, dir: &Path) -> bool {
        self.exclude_layer_dir(dir).is_some()
    }

    /// 目录路径命中的排除**层**;`None` = 不命中任何规则。
    ///
    /// 硬层优先返回(它是「谁都不能放行、也不进库」的那一档),否则取软层命中。
    /// 派生门、双向对账与「能不能纳入」判定都走这里,不再各自抄一遍名单。
    pub fn exclude_layer_dir(&self, dir: &Path) -> Option<ExcludeLayer> {
        if self.under_own_prefix(dir) {
            return Some(ExcludeLayer::Hard);
        }
        let mut soft = None;
        for n in dir
            .ancestors()
            .filter_map(|d| d.file_name())
            .map(|n| n.to_string_lossy().into_owned())
        {
            // 点开头的隐藏目录(.git / .config / .workbuddy…)算硬层:
            // 工具与版本库的内部数据按定义不是照片集,表态也不该收进来。
            if n.starts_with('.')
                || HARD_EXCLUDES
                    .iter()
                    .chain(HARD_EXCLUDES_OURS.iter())
                    .any(|h| h.eq_ignore_ascii_case(&n))
            {
                return Some(ExcludeLayer::Hard);
            }
            if self.excludes_dir(&n) {
                soft = Some(ExcludeLayer::Soft);
            }
        }
        soft
    }
}

/// 路径比较用的键:ASCII 大写归一 + 剥尾分隔符。
/// 只做 ASCII 归一,字节长度不变 —— 下面的整级判断要按字节取下一字符。
fn path_key(s: &str) -> String {
    let mut k = s.to_ascii_uppercase();
    while k.ends_with('\\') || k.ends_with('/') {
        k.pop();
    }
    k
}

/// `up` 是否**整级**落在 `base` 之内:相等,或 `base` 之后紧跟一个分隔符。
/// 「整级」是硬要求 —— `D:\maliang\cache` 可以罩 `D:\maliang\cache\thumbs`,
/// 但不该顺手罩住用户的 `D:\maliang\cached_photos`。
fn under_key(up: &str, base: &str) -> bool {
    if base.is_empty() {
        return false;
    }
    if up == base {
        return true;
    }
    up.len() > base.len() && up.starts_with(base) && matches!(up.as_bytes()[base.len()], b'\\' | b'/')
}

/// 一条**结构**记录:发现路径只写这些列。
///
/// 宽高 / 拍摄时间 / GPS / 机身属于**派生列**,只在派生时读一次
/// ([`read_derived`]),由 [`crate::db::Store::mark_derived`] 回写 ——
/// 全盘发现动辄百万行,逐行解 EXIF 会把"列目录"拖成"读全库"。
#[derive(Clone)]
pub struct Record {
    pub path: String,
    pub parent: String,
    pub mtime: i64,
    pub size: i64,
    /// 媒体类型:`video::kind::PHOTO`(0)=图片,`video::kind::MOVIE`(1)=视频。
    /// 记录类型的"是否视频"二分(7 种子类型对应不同解码路径,
    /// 只有一条 Shell 路径,无须细分)。
    pub kind: i32,
}

/// 派生列:EXIF / 尺寸 / GPS / 机身。
#[derive(Debug, Clone)]
pub struct Derived {
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub taken_utc: Option<String>,
    pub rot: i32,
    /// GPS(十进制度;无定位 None) — 对应 lat/long 列。
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// 相机型号(Make+Model)。
    pub cam: Option<String>,
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

/// 扫描进度与取消控制。
///
/// 扫描进度与取消控制的两处设计:
/// - 扫描进度:**预计算 `1.0/total` 倒数**,
///   之后每次上报只做一次浮点乘法(不反复做整数除法)。`total` 设定后立即算出
///   `inv_total`,进度 = `done * inv_total`。
/// - 目录扫描 + 取消检查:
///   取消标志是**独立于工作线程的原子字节**,热路径每处理 N 个文件查一次;
///   取消时停止批量入库。
///
/// 进度是**原子计数**而非回调 —— UI 线程读时无锁。
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

    /// 设定总量并预计算倒数(核心优化)。
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
    store.read(|conn| {
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
    })
}

/// 并行扫描多个根目录,仅记录存在的图像文件(增量 upsert 交给数据库)
///
/// - 未变文件(path+size+mtime 与库内一致)直接跳过,不重解码 EXIF、不写库。
/// - 扫描线程降优先级(BelowNormal),避免与 UI 抢核。
/// - `ctrl` 提供进度上报与取消。
pub fn scan_dir(store: &Store, roots: &[PathBuf]) -> Result<ScanSummary> {
    let ctrl = Arc::new(ScanControl::new());
    scan_dir_ctrl(store, roots, &ctrl, None)
}

/// 带进度与取消的扫描。
///
/// 取消语义:
/// - **遍历阶段**每 64 个目录项查一次取消标志(分段查,不是每文件一次);
/// - **并行处理阶段**用 rayon 的 `collect::<Result<Vec<_>,_>>()`,
///   任一线程发现取消即整体短路;
/// - 取消时**已算出的记录照常入库**(已完成的工作不丢),
///   只是不再继续处理剩余文件。
/// MFT 名字索引候选快路的结果。
pub struct MftCandidates {
    pub paths: Vec<PathBuf>,
    pub excluded_dirs: usize,
}

/// 在已载入的全盘名字索引里定位 `root` 子树,按扩展名(图片+RAW+视频)
/// 直接产出候选路径 —— 跳过 walkdir 的逐目录枚举(索引已在内存,
/// 枚举成本毫秒级;真正的 stat/EXIF 管线不变)。
///
/// 返回 `None` = 索引定位不到该根(盘不在索引 / 逐级名字失配,常见于
/// 索引快照之后新建的目录)→ 调用方回退 walkdir;
/// `Some(空)` = 定位成功但子树没有可收录文件。
/// 口径:索引是快照(默认每次启动 USN 增量保鲜);watch 事件链路不受影响。
pub fn mft_candidates(
    vols: &[crate::nameidx::VolumeIndex],
    root: &Path,
    excl: &Exclusions,
    ctrl: Option<&ScanControl>,
) -> Option<MftCandidates> {
    use std::path::Component;
    let mut letter = None;
    let mut comps: Vec<String> = Vec::new();
    for c in root.components() {
        match c {
            Component::Prefix(p) => {
                // "C:" / "\\?\C:" 都取首个 ASCII 字母当盘符。
                let s = p.as_os_str().to_string_lossy();
                let b = *s.as_bytes().first()?;
                if !b.is_ascii_alphabetic() {
                    return None;
                }
                letter = Some(b.to_ascii_uppercase() as char);
            }
            Component::RootDir => {}
            Component::Normal(s) => comps.push(s.to_string_lossy().into_owned()),
            _ => return None,
        }
    }
    let letter = letter?;
    let v = vols.iter().find(|v| v.letter == letter)?;
    // 子项邻接表用索引自带的 CSR(建库/载入/增量时一次构建),这里只切片;
    // 槽位 count() 是「卷根直属」虚拟桶,PARENT_ROOT 哨兵统一落进去。
    // 从卷根逐级下钻定位根目录(名字比较口径与折叠池一致:ASCII 不区分大小写)。
    let mut level: &[u32] = v.children_of(v.count());
    for comp in &comps {
        let hit = level
            .iter()
            .copied()
            .find(|&ci| v.is_dir(ci as usize) && v.name(ci as usize).eq_ignore_ascii_case(comp));
        level = match hit {
            Some(ci) => v.children_of(ci as usize),
            None => return None,
        };
    }
    // level 即根目录直属条目(comps 为空时 = 卷根直属)。
    let mut paths = Vec::new();
    let mut excluded_dirs = 0usize;
    let mut stack: Vec<u32> = level.to_vec();
    // 已经取消就别开工:直接交回空结果(仍是 Some,理由同下)。
    if ctrl.is_some_and(|c| c.is_cancelled()) {
        return Some(MftCandidates {
            paths: Vec::new(),
            excluded_dirs: 0,
        });
    }
    // 展开可能覆盖整棵子树(百万级),每 4096 项查一次取消(修 A5):
    // 取消时返回**已收集的部分**而不是 None —— None 会让调用方回退 walkdir,
    // 那正是用户想中止的慢路径。调用方随后自己的 is_cancelled 会终止整轮扫描。
    let mut since_check = 0usize;
    while let Some(i) = stack.pop() {
        since_check += 1;
        if since_check >= 4096 {
            since_check = 0;
            if let Some(c) = ctrl {
                if c.is_cancelled() {
                    break;
                }
            }
        }
        let name = v.name(i as usize);
        if v.is_dir(i as usize) {
            // 自家缓存目录走整级路径前缀(见 `Exclusions::own_prefixes`);
            // 拼完整路径有成本,所以先按目录名粗筛。
            let own = excl.own_leaf_may_match(&name)
                && excl.under_own_prefix_str(&v.path_of(i as usize));
            if excl.excludes_dir(&name) || own {
                excluded_dirs += 1;
                continue;
            }
            stack.extend(v.children_of(i as usize).iter().copied());
        } else if is_indexable(Path::new(&name)) {
            paths.push(PathBuf::from(v.path_of(i as usize)));
        }
    }
    Some(MftCandidates {
        paths,
        excluded_dirs,
    })
}

/// 启动对账的删除侧:把「库里有行、磁盘上已消失」的照片移出图库。
///
/// 以磁盘为准(与 watch 事件路径同一原语 `drop_missing_paths`:逐路径复核
/// 存在性,目录缺失连子树一起摘)。只考虑**当前监听根之内**、且根目录仍
/// 在线(可 stat)的行 —— 根被删/盘未挂载/网络断开时整根跳过,防止离线
/// 造成整根误清;已移除监听的目录行不在此管。返回被移除的 (id, path)。
pub fn reconcile_missing(store: &Store, roots: &[PathBuf]) -> Vec<(i64, String)> {
    let online: Vec<&PathBuf> = roots.iter().filter(|r| r.is_dir()).collect();
    if online.is_empty() {
        return Vec::new();
    }
    let all: Vec<String> = store.read(|conn| {
        let mut st = match conn.prepare("SELECT path FROM photos") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows: Vec<String> = st
            .query_map([], |r| r.get::<_, String>(0))
            .ok()
            .map(|it| it.flatten().collect())
            .unwrap_or_default();
        rows
    });
    let in_roots: Vec<String> = all
        .into_iter()
        .filter(|p| {
            let pb = Path::new(p.as_str());
            online.iter().any(|r| pb.starts_with(r.as_path()))
        })
        .collect();
    if in_roots.is_empty() {
        return Vec::new();
    }
    store.drop_missing_paths(&in_roots).unwrap_or_default()
}

/// 扫描专用 rayon 池:核数的一半、每线程 BelowNormal。
///
/// 与缩略图同理(见 `crate::thumbs::thumb_pool`):全局池线程按 Normal
/// 优先级创建,首启全盘扫描的 stat 洪峰会把所有核吃满,UI 线程同级
/// 抢不到时间片。窄池 + 低优先级让扫描全程不挡界面,扫描本身只是
/// 元数据读取,半数核的吞吐足够。
fn scan_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let n = (crate::win::cpu_count() / 2).clamp(2, 8);
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .thread_name(|i| format!("maliang-scan-{i}"))
            .start_handler(|_| {
                crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal)
            })
            .build()
            .expect("扫描线程池初始化失败")
    })
}

pub fn scan_dir_ctrl(
    store: &Store,
    roots: &[PathBuf],
    ctrl: &Arc<ScanControl>,
    index: Option<&crate::nameidx::IndexFile>,
) -> Result<ScanSummary> {
    crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);

    let excl = Exclusions::from_store(store);
    // 快速路径基线:库内已有指纹
    let prints = load_fingerprints(store);
    // MFT 快路的卷数据(索引在位才有;Linux/无索引为 None → walkdir)。
    let vols: Option<&[crate::nameidx::VolumeIndex]> =
        index.map(|f| f.vols.as_slice());

    let mut excluded_dirs = 0usize;
    let mut candidates: Vec<PathBuf> = Vec::new();
    // 遍历阶段的可取消性:目录项计数(按文件队列分段查取消)。
    let mut visit_counter = 0usize;
    'outer: for r in roots {
        // MFT 快路:索引在位且根可定位 → 免逐目录枚举;定位失败回退 walkdir。
        if let Some(vs) = vols {
            if let Some(mc) = mft_candidates(vs, r, &excl, Some(ctrl.as_ref())) {
                excluded_dirs += mc.excluded_dirs;
                candidates.extend(mc.paths);
                if ctrl.is_cancelled() {
                    break 'outer;
                }
                continue;
            }
        }
        let it = WalkDir::new(r)
            .into_iter()
            .filter_entry(|e| {
                // 目录级排除:命中即整棵子树跳过
                if e.file_type().is_dir() {
                    let name = e.file_name().to_string_lossy();
                    let own = excl.under_own_prefix(e.path());
                    if excl.excludes_dir(&name) || own {
                        excluded_dirs += 1;
                        return false;
                    }
                }
                true
            })
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            // 视频同样收录(`photos.kind` 记为 MOVIE),否则 `crate::video`
            // 的 21 个扩展名与缩略图抽帧链路都不可达。
            .filter(|e| is_indexable(e.path()))
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

    // 取消后不再处理新文件,但已算出的记录全部保留(不丢已完成的工作)。
    let collected: Vec<Option<Record>> = scan_pool().install(|| {
        candidates
            .into_par_iter()
            .map(|p| {
                // 取消检查:已取消就跳过剩余候选(在途记录照常保留)。
                if ctrl2.is_cancelled() {
                    return None;
                }
                // ---- 快速路径:指纹一致则整条跳过(不读 EXIF、不解码) ----
                let md = match std::fs::metadata(&p) {
                    Ok(m) => m,
                    Err(_) => return None,
                };
                let mtime = match md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                {
                    Some(d) => d.as_millis() as i64,
                    None => return None,
                };
                let size = md.len() as i64;
                let key = p.to_string_lossy().into_owned();
                if let Some(&(osize, omtime)) = prints.get(&key) {
                    if osize == size && omtime == mtime {
                        unchanged2.fetch_add(1, Ordering::Relaxed);
                        counter2.fetch_add(1, Ordering::Relaxed);
                        ctrl2.advance(1);
                        return None;
                    }
                    // 库里有但内容变了 → 算"修改"(变更日志)。
                    modified2.fetch_add(1, Ordering::Relaxed);
                } else {
                    // 库里没有 → 算"新增"。
                    added2.fetch_add(1, Ordering::Relaxed);
                }
                // 取消检查:每处理 64 个文件查一次。
                let n = counter2.fetch_add(1, Ordering::Relaxed);
                ctrl2.advance(1);
                if ctrl2.should_stop(n) && ctrl2.is_cancelled() {
                    return None;
                }
                Some(read_record(&p, size, mtime))
            })
            .collect()
    });

    let cancelled = ctrl.is_cancelled();
    let records: Vec<Record> = collected.into_iter().flatten().collect();
    let indexed = records.len();
    // 取消也要把已算出的记录入库(不丢已完成的工作)。
    store_insert(store, &records)?;
    // 写变更日志(新增/修改各一条聚合记录,避免逐条写库拖慢扫描)。
    let n_added = added.load(Ordering::Relaxed);
    let n_mod = modified.load(Ordering::Relaxed);
    if n_added > 0 {
        let _ = store.log_change("<扫描>", "added", None);
    }
    if n_mod > 0 {
        let _ = store.log_change("<扫描>", "modified", None);
    }
    // 扫描时顺带补 uid64(不必手动点菜单)。
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

/// 读一条**结构**记录:只碰文件系统元数据,不打开文件内容。
fn read_record(p: &Path, size: i64, mtime: i64) -> Record {
    Record {
        path: p.to_string_lossy().into_owned(),
        parent: p
            .parent()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default(),
        mtime,
        size,
        kind: if crate::video::is_video(p) {
            crate::video::kind::MOVIE
        } else {
            crate::video::kind::PHOTO
        },
    }
}

/// 读派生列(EXIF 只解析一次;RAW 走 Shell 拿尺寸)。
///
/// 只在派生时调用 —— 缩略图那一趟已经把文件打开了,顺手读一次 EXIF
/// 比在发现路径上逐行解要省得多。
pub fn read_derived(p: &Path) -> Derived {
    // 视频不解析 EXIF、也不起 COM 抽帧:容器里没有 Orientation / 机身信息,
    // 逐个拉 Shell 缩略图更是拖垮整趟派生。尺寸留空,
    // 显示时由 `crate::thumbs` 的抽帧路径按需补齐。
    let is_vid = crate::video::is_video(p);
    let meta = if is_vid {
        crate::exif::Meta {
            orientation: 1,
            ..Default::default()
        }
    } else {
        crate::exif::read_meta(p)
    };
    // 尺寸:优先 EXIF;缺失时用解码器读头(RAW 解不了,改用 Shell 缩略图尺寸)。
    let (mut w, mut h) = (meta.width, meta.height);
    if !is_vid && (w.is_none() || h.is_none()) {
        let dims = image_dimensions(p).or_else(|| shell_dimensions(p));
        w = w.or(dims.map(|d| d.0));
        h = h.or(dims.map(|d| d.1));
    }
    Derived {
        width: w,
        height: h,
        taken_utc: meta.taken,
        rot: meta.orientation as i32,
        lat: meta.gps.map(|g| g.0),
        lon: meta.gps.map(|g| g.1),
        cam: meta.cam,
    }
}

/// 只读图片头拿宽高(不解码整图)。PSD / PSB 自备头解析,不走 `image` crate。
fn image_dimensions(p: &Path) -> Option<(i32, i32)> {
    if crate::psd_preview::is_psd_path(p) {
        let (w, h) = crate::psd_preview::dimensions(p)?;
        return Some((w as i32, h as i32));
    }
    let (a, b) = ImageReader::open(p)
        .ok()?
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    Some((a as i32, b as i32))
}

/// RAW 等 image crate 解不了的格式:借 Windows Shell 缩略图扩展取尺寸
/// (COM 路线)。
fn shell_dimensions(p: &Path) -> Option<(i32, i32)> {
    let (_bgra, w, h) = crate::win::shell_thumbnail(p, 256)?;
    Some((w as i32, h as i32))
}

/// 索引**单个**文件(事件驱动用)。
///
/// 事件驱动(watch)拿到的是"哪个文件变了",不是"哪个目录变了";
/// 这里只处理这一个文件,不做整树遍历 —— 目录项枚举的粒度
/// 是 `FindNextFileW` 一次拿一个目录项的属性。
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
    if !is_indexable(path) {
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

    let key = path.to_string_lossy().into_owned();
    // 快速路径:指纹一致则跳过(事件重复通知时不再重解码 EXIF)。
    // 读网关返回 None = 未变(外层直接跳过);Some(existed) = 要入库,
    // existed 指示新增(false)还是修改(true)。注意闭包内的 return
    // 只退出闭包,"跳过"必须用返回值传出来。
    let existed: bool = match store.read(|conn| -> anyhow::Result<Option<bool>> {
        let old: Option<(i64, i64)> = conn
            .query_row(
                "SELECT size, mtime FROM photos WHERE path=?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        Ok(match old {
            Some((osize, omtime)) if osize == size && omtime == mtime => None,
            Some(_) => Some(true),  // 已有记录但内容变了 → 修改
            None => Some(false),    // 库里没有 → 新增
        })
    })? {
        Some(existed) => existed,
        None => return Ok(false),
    };

    let rec = read_record(path, size, mtime);
    store_insert(store, &[rec])?;
    // 事件驱动路径也要写变更日志(否则只有扫描会记,监听发现的变化会丢)。
    let _ = store.log_change(
        &key,
        if existed { "modified" } else { "added" },
        None,
    );
    Ok(true)
}

/// 结构入库:发现路径只写结构列,**派生列一律不动**。
///
/// 派生列归 [`crate::db::Store::mark_derived`] 所有 —— 一条记录被重扫时
/// 若在这里覆盖,已派生的 EXIF 会在下一趟发现里被抹成 NULL。
/// 内容变了(mtime 不同)只把该行退回待派生,旧的缩略图与 dhash 留着继续用,
/// 等后台稳定期过后重做一次覆盖(见 `db::Store::pending_rich`)。
///
/// 顺手按派生门算 `rich_state`:walkdir 与 MFT 两条发现路必须
/// 共用同一个门,否则非 NTFS 卷会整个绕过作用域。
/// 单事务落库的行数上限。
///
/// 全盘首扫可能一次带来几十万条记录:一条事务包到底,写连接锁要被
/// 持有数秒到数十秒,期间 UI 线程的任何写操作(改设置、记账、摘行)
/// 都得干等。按块拆事务,单块毫秒级提交,UI 的写延迟有界。
const INSERT_CHUNK: usize = 4096;

fn store_insert(store: &Store, records: &[Record]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    // 门读一次库,之后逐行判定纯内存(百万行落库不能被逐行查库拖成 IO 密集)。
    let gate = crate::db::ScopeGate::load(store);
    // 注意:必须用 execute_batch —— rusqlite 的 `execute` 只接受单条语句,
    // 传多条会报 "Multiple statements provided"。
    for chunk in records.chunks(INSERT_CHUNK) {
        store.write(|conn| -> Result<()> {
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
            let tx = conn.unchecked_transaction()?;
            let mut st = tx.prepare_cached(
                "INSERT INTO photos(path,parent,mtime,size,star,thumb_ok,uid64,kind,ext,rich_state)
                 VALUES(:path,:parent,:mtime,:size,0,0,:uid,:kind,:ext,:rich)
                 ON CONFLICT(path) DO UPDATE SET
                   mtime=:mtime, size=:size, kind=:kind,
                   uid64=COALESCE(photos.uid64, :uid),
                   rich_state=CASE
                     WHEN photos.mtime<>:mtime AND photos.thumb_ok=1
                          AND photos.rich_state<>3 THEN 1
                     WHEN :rich<>1 THEN photos.rich_state
                     WHEN photos.rich_state=0 THEN 1
                     ELSE photos.rich_state END",
            )?;
            for r in chunk {
                // uid64 = fnv1a64(path|size|mtime)。
                // 用 `COALESCE(photos.uid64, :uid)` —— 已有 uid64 的行**保持不变**
                // (标识是持久的,不应随内容变化而变;重新索引不应改变外部引用)。
                let uid = crate::db::fnv1a64(&format!("{}|{}|{}", r.path, r.size, r.mtime));
                let ext = crate::db::Store::ext_of(&r.path);
                let rich = gate.rich_state_of(&r.path, &r.parent, ext.as_deref());
                st.execute(rusqlite::named_params! {
                    ":path": r.path, ":parent": r.parent, ":mtime": r.mtime, ":size": r.size,
                    ":uid": uid as i64, ":kind": r.kind, ":ext": ext, ":rich": rich,
                })?;
            }
            drop(st);
            tx.commit()?;
            Ok(())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 删除对账:消失的行被移出、在盘的保留;离线根与根外路径一律不碰。
    #[test]
    fn reconcile_missing_drops_gone_keeps_present_and_offline() {
        let store = crate::db::Store::open_mem().unwrap();
        let root = scratch_dir("reconcile-root");
        let kept = root.join("kept.jpg");
        std::fs::write(&kept, b"x").unwrap();
        let gone = root.join("gone.jpg"); // 从未落盘
        // 离线根:确定不存在的目录(不能用 Q: 这类盘符 —— 有些机器真有映射)。
        let offline = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("rk-reconcile-offline-{}", std::process::id()));
        let outside = scratch_dir("reconcile-outside");
        let outside_file = outside.join("other.jpg");
        std::fs::write(&outside_file, b"y").unwrap();
        {
            let c = store.test_conn();
            for p in [&kept, &gone, &offline.join("u.jpg"), &outside_file] {
                c.execute(
                    "INSERT INTO photos(path,parent,mtime,size) VALUES(?1,'/',0,0)",
                    rusqlite::params![p.to_string_lossy().as_ref()],
                )
                .unwrap();
            }
        }
        let dropped = reconcile_missing(&store, &[root.clone(), offline.clone()]);
        let dropped_paths: Vec<&str> = dropped.iter().map(|(_, p)| p.as_str()).collect();
        assert_eq!(dropped.len(), 1, "只有消失的文件被移出: {dropped_paths:?}");
        assert!(dropped_paths[0].ends_with("gone.jpg"));
        // 离线根之下的行不能被误清(根不可 stat → 整根跳过)。
        let off_str = offline.join("u.jpg").to_string_lossy().into_owned();
        let n: i64 = store
            .test_conn()
            .query_row(
                "SELECT COUNT(*) FROM photos WHERE path=?1",
                [off_str],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "离线根下的行必须保留");
        // 根之外的行不在此管。
        let o: i64 = store
            .test_conn()
            .query_row("SELECT COUNT(*) FROM photos WHERE path=?1", [outside_file.to_string_lossy().as_ref()], |r| r.get(0))
            .unwrap();
        assert_eq!(o, 1, "监听根之外的行必须保留");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// 测试暂存目录。固定在 `target/ml-scratch/` 下,不能用系统 temp:
    /// Windows 的 temp 落在 `...\AppData\Local\Temp`,路径里同时命中默认排除表的
    /// `AppData` 与 `temp` 两项,`excludes_path` 逐级祖先匹配会把待测文件判成排除,
    /// 使 `index_file` 恒返回 false(换台机器 TMP 不在 AppData 下就复现不了)。
    /// 也不能叫 `rk-scratch`:那是硬排除名单里登记的构建夹具目录名。
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("ml-scratch")
            .join(format!("{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_exclusions_are_stable() {
        // 目录排除规则
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
    fn default_exclusions_skip_system_and_cache_dirs() {
        // 已知问题: 回收站元数据文件($I*.jpg)不是图片,解码永远失败,
        // 缩略图 worker 对失败不记账,每轮取到同一批 → 进度永久卡死。
        // 应用数据/缓存目录同理,不该进照片索引。
        let e = Exclusions::default();
        assert!(e.excludes_dir("$RECYCLE.BIN"), "回收站必须排除");
        assert!(e.excludes_dir("System Volume Information"));
        assert!(e.excludes_dir("AppData"));
        assert!(e.excludes_dir("Program Files (x86)"));
        assert!(e.excludes_dir("node_modules"));
        assert!(e.excludes_dir("npm-cache"));
        assert!(e.excludes_dir("Recovery"));
        assert!(e.excludes_dir("$WINDOWS.~BT"));
        // 点开头的隐藏目录(.git / .config / .workbuddy 等)一律排除
        assert!(e.excludes_dir(".git"));
        assert!(e.excludes_dir(".workbuddy"));
        assert!(e.excludes_dir(".config"));
        assert!(!e.excludes_dir("SeaDriveRoot"));
    }

    #[test]
    fn image_extensions_cover_common_and_raw() {
        assert!(is_image(Path::new("a.JPG")));
        assert!(is_image(Path::new("a.png")));
        assert!(is_image(Path::new("a.nef")));
        assert!(is_image(Path::new("a.CR2")));
        assert!(is_image(Path::new("a.arw")));
        assert!(is_image(Path::new("a.RAF")));
        assert!(is_image(Path::new("a.gif")), "GIF 首帧应进索引");
        assert!(is_image(Path::new("a.PSD")), "PSD 只读预览应进索引");
        assert!(is_image(Path::new("a.psb")));
        assert!(!is_image(Path::new("a.txt")));
        assert!(!is_image(Path::new("a.mp4")));
        assert!(is_raw(Path::new("a.nef")));
        assert!(!is_raw(Path::new("a.jpg")));
        assert!(is_readonly_media(Path::new("a.gif")));
        assert!(is_readonly_media(Path::new("a.psd")));
        assert!(!is_readonly_media(Path::new("a.jpg")));
        assert!(is_media_ext("psd"));
    }

    #[test]
    fn animated_webp_is_readonly_but_static_webp_is_not() {
        let dir = scratch_dir("webp-anim");
        let make = |name: &str, flags: u8| -> std::path::PathBuf {
            let mut h = [0u8; 30];
            h[0..4].copy_from_slice(b"RIFF");
            h[8..12].copy_from_slice(b"WEBP");
            h[12..16].copy_from_slice(b"VP8X");
            h[20] = flags;
            let p = dir.join(name);
            std::fs::write(&p, h).unwrap();
            p
        };
        let anim = make("anim.webp", 0x02);
        let still = make("still.webp", 0x00);
        assert!(is_readonly_media(&anim), "动图 WebP 不能被烘培成 JPEG 首帧");
        assert!(!is_readonly_media(&still), "静态 WebP 仍可正常编辑");
        assert!(!is_readonly_media(Path::new("missing.webp")));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 快速路径:未变文件不重解码 EXIF(不产生新记录),变更文件会重索引。
    #[test]
    fn scan_skips_unchanged_files() {
        let dir = scratch_dir("scanfast");
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
        let dir = scratch_dir("scanexcl");
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
        let dir = scratch_dir("indexfile");
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

    /// 视频必须真的能入库 —— 目录扫描与单文件入库两条路径都要放行视频。
    ///
    /// 收录判定按扩展名分流,不解码文件本身,所以这里用一个
    /// 只有文件头的占位 .mp4 就够了。
    #[test]
    fn video_files_are_indexed() {
        let dir = scratch_dir("scanvideo");
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(4, 4))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(dir.join("a.jpg"), &buf).unwrap();
        std::fs::write(dir.join("clip.mp4"), b"\0\0\0\x18ftypmp42").unwrap();

        // 目录扫描:视频与图片应一并收录
        let store = Store::open_mem().unwrap();
        let s = scan_dir(&store, &[dir.clone()]).unwrap();
        assert_eq!(s.indexed, 2, "视频应与图片一样被收录,实得 {}", s.indexed);

        // 事件驱动的单文件入库
        let store2 = Store::open_mem().unwrap();
        assert!(
            index_file(&store2, &dir.join("clip.mp4")).unwrap(),
            "视频应能通过单文件入库路径进入索引"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 排除规则对单文件路径同样生效(事件驱动路径)。
    #[test]
    fn index_file_respects_exclusion_dirs() {
        let dir = scratch_dir("indexexcl");
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

    // ===== 自有目录按整级路径前缀排除 =====

    /// 自家缓存目录走**路径前缀**而不是目录名:同名用户相册不受误伤,
    /// 前缀也不会罩住名字前缀相同的兄弟目录(`cache` 不罩 `cached_photos`)。
    #[cfg(windows)]
    #[test]
    fn own_prefix_excludes_whole_subtree_by_path() {
        let e = Exclusions::default().with_own_prefix(r"D:\maliang\cache");
        assert!(
            e.excludes_dir_path(Path::new(r"D:\maliang\cache")),
            "缓存目录自身就该排除"
        );
        assert!(e.excludes_dir_path(Path::new(r"D:\maliang\cache\smallthumbs")));
        assert!(e.excludes_path(Path::new(r"D:\maliang\cache\thumbs\a.jpg")));
        assert!(
            e.excludes_dir_path(Path::new(r"d:\Maliang\CACHE\X")),
            "路径比较大小写不敏感"
        );
        assert!(
            !e.excludes_dir_path(Path::new(r"D:\maliang\cached_photos")),
            "整级命中才算:名字前缀相同不算"
        );
        assert!(
            !e.excludes_dir_path(Path::new(r"D:\maliang")),
            "前缀不能反向罩住自己的父目录"
        );
        assert!(
            !e.excludes_dir_path(Path::new(r"C:\Users\me\Pictures\cache")),
            "别的卷/别的父路径下同名目录不算"
        );
        assert!(e.own_leaf_may_match("cache") && e.own_leaf_may_match("CACHE"));
        assert!(!e.own_leaf_may_match("smallthumbs"));
    }

    /// own-prefix 子树拦截在 POSIX 路径上的对偶:自家缓存目录由环境推导,
    /// 前缀边界改用固定的绝对路径 —— 避开「以点开头的目录一律排除」那条名字规则。
    #[cfg(not(windows))]
    #[test]
    fn own_prefix_excludes_whole_subtree_by_path_posix() {
        let own = crate::db::system_cache_dir();
        let leaf = own
            .file_name()
            .expect("自家缓存目录该有一个叶子名")
            .to_string_lossy()
            .into_owned();
        let e = Exclusions::default().with_own_prefix(&own);
        assert!(e.under_own_prefix_str(&own.to_string_lossy()), "缓存根本身算子树");
        assert!(
            e.under_own_prefix_str(&own.join("albums").join("2026").to_string_lossy()),
            "自家缓存的任意深度都归 own-prefix 管",
        );
        assert!(e.own_leaf_may_match(&leaf) && e.own_leaf_may_match(&leaf.to_uppercase()));

        // 前缀边界:同词头的兄弟目录既不是子树,叶子名也不该被快路径放行。
        let b = Exclusions::default().with_own_prefix("/var/lib/ml/cache");
        assert!(b.excludes_dir_path(Path::new("/var/lib/ml/cache/albums")));
        assert!(b.excludes_path(Path::new("/var/lib/ml/cache/albums/a.jpg")));
        assert!(
            !b.excludes_dir_path(Path::new("/var/lib/ml/cache_backup")),
            "同词头兄弟目录不是子树",
        );
        assert!(!b.own_leaf_may_match("cache_backup"));
    }

    /// 尾分隔符与不同分隔符写法不能改变判定。
    #[test]
    fn own_prefix_matching_ignores_trailing_and_mixed_separators() {
        let e = Exclusions::default().with_own_prefix(r"D:\maliang\cache\");
        assert!(e.excludes_dir_path(Path::new(r"D:\maliang\cache")));
        assert!(e.excludes_dir_path(Path::new(r"D:\maliang\cache\previews")));
        let fwd = Exclusions::default().with_own_prefix("/own/cache/");
        assert!(fwd.excludes_dir_path(Path::new("/own/cache/thumbs")));
        assert!(!fwd.excludes_dir_path(Path::new("/own/cached")));
    }

    /// 默认构造不带任何自有前缀 —— 老调用点(纯名字判定)行为不变。
    #[test]
    fn default_exclusions_have_no_own_prefixes() {
        let e = Exclusions::default();
        assert!(e.own_prefixes().is_empty());
        assert!(!e.own_leaf_may_match("cache"));
        assert!(!e.excludes_dir_path(Path::new(r"D:\anything\cache\x")));
    }

    /// 生产入口确实把当前缓存位置注入了进来(不写死路径,跟着解析走)。
    #[test]
    fn from_store_injects_the_resolved_cache_dir() {
        let store = Store::open_mem().unwrap();
        let e = Exclusions::from_store(&store);
        let loc = crate::db::cache_dir_loc();
        assert!(
            e.own_prefixes().iter().any(|p| *p == loc),
            "排除项里必须有当前缓存目录 {loc:?}"
        );
        assert!(e.excludes_dir_path(&loc));
    }

    // ===== 进度与取消 =====

    /// 进度:预计算倒数 + 乘法(扫描进度的核心优化)。
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

    /// total=0 不能除零(倒数缓存也有这个保护)。
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
        let dir = scratch_dir("scancel");
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
        let s = scan_dir_ctrl(&store, &[dir.clone()], &ctrl, None).unwrap();
        assert!(s.cancelled, "应标记为已取消");
        assert_eq!(s.indexed, 0, "取消后不应索引任何文件");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 取消时已完成记录不丢(issue #1):取消后 photos 里应能查到
    /// 取消前处理完的记录,恢复扫描只补剩余、不产生重复。
    #[test]
    fn cancelled_scan_keeps_completed_records() {
        let dir = scratch_dir("scancelkeep");
        let n_files = 4096;
        let mut buf: Vec<u8> = Vec::new();
        for i in 0..n_files {
            buf.clear();
            image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 6))
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
                .unwrap();
            std::fs::write(dir.join(format!("a{i}.jpg")), &buf).unwrap();
        }
        let store = Store::open_mem().unwrap();
        let ctrl = Arc::new(ScanControl::new());
        // 后台线程在进度过 1/4 时请求取消,模拟用户中途停止。
        let ctrl_bg = ctrl.clone();
        let bg = std::thread::spawn(move || {
            while ctrl_bg.fraction() < 0.25 {
                std::thread::yield_now();
            }
            ctrl_bg.cancel();
        });
        let s = scan_dir_ctrl(&store, &[dir.clone()], &ctrl, None).unwrap();
        bg.join().unwrap();
        assert!(s.cancelled, "扫描应被取消");
        assert!(s.indexed > 0, "取消前已完成的记录不应丢失");
        let stored: i64 = store
            .test_conn()
            .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored as usize, s.indexed, "已完成的记录应全部入库");
        // 恢复扫描补齐剩余文件,总数应恰好等于文件数(无重复)。
        let s2 = scan_dir(&store, &[dir.clone()]).unwrap();
        assert!(!s2.cancelled);
        let stored2: i64 = store
            .test_conn()
            .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored2 as usize, n_files, "恢复扫描后应恰好入库全部文件");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 正常扫描不标记取消,且 uid64 被自动分配。
    #[test]
    fn scan_assigns_uid64() {
        let dir = scratch_dir("uid");
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 6))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(dir.join("a.jpg"), &buf).unwrap();

        let store = Store::open_mem().unwrap();
        let s = scan_dir(&store, &[dir.clone()]).unwrap();
        assert_eq!(s.indexed, 1);
        assert!(!s.cancelled);

        let uid: i64 = store.test_conn().query_row("SELECT uid64 FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_ne!(uid, 0, "uid64 应被分配");
        // 重新扫描不应改变 uid64(标识是持久的)。
        let _ = scan_dir(&store, &[dir.clone()]).unwrap();
        let uid2: i64 = store.test_conn().query_row("SELECT uid64 FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(uid, uid2, "重新扫描后 uid64 应保持不变");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 事件驱动的单文件入库**也**要写变更日志
    /// (v5 审计发现:此前只有扫描路径写,监听发现的变化会丢)。
    #[test]
    fn index_file_logs_change() {
        let dir = scratch_dir("idxlog");
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

    /// 扫描会写变更日志(新增记 added)。
    #[test]
    fn scan_logs_changes() {
        let dir = scratch_dir("chg");
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

    /// MFT 快路:索引子树定位、排除目录、扩展名过滤、大小写与回退。
    #[cfg(windows)]
    #[test]
    fn mft_candidates_resolves_subtree_and_falls_back() {
        use crate::nameidx::VolumeIndexBuilder;
        // 0x10 = FILE_ATTRIBUTE_DIRECTORY(nameidx 私有常量,这里用字面量)。
        const DIR: u32 = 0x10;
        /// 测试用的简化入口:按 USN 记录形态追加(元数据列全 0)。
        fn push(b: &mut VolumeIndexBuilder, frn: u64, parent: u64, attrs: u32, name: &str) {
            let units: Vec<u16> = name.encode_utf16().collect();
            b.push_usn(&crate::mft::UsnEntry {
                frn,
                parent_frn: parent,
                attrs,
                reason: 0,
                usn: 0,
                timestamp: 0,
                name: &units,
            });
        }
        let mut b = VolumeIndexBuilder::new('C');
        push(&mut b, 0x1000, 5, DIR, "Pics");
        push(&mut b, 0x1001, 0x1000, 0x20, "IMG_0001.JPG");
        push(&mut b, 0x2000, 0x1000, DIR, "RAW");
        push(&mut b, 0x2001, 0x2000, 0x20, "DSC.NEF");
        push(&mut b, 0x3000, 0x1000, DIR, "thumbs");
        push(&mut b, 0x3001, 0x3000, 0x20, "cache.jpg");
        push(&mut b, 0x1002, 0x1000, 0x20, "notes.txt");
        let v = b.finish();
        let excl = Exclusions::default();

        // 子树根可定位:只收 JPG/NEF,排除 thumbs 整棵,notes.txt 不收。
        let mc =
            mft_candidates(std::slice::from_ref(&v), Path::new(r"C:\Pics"), &excl, None).unwrap();
        assert_eq!(mc.paths.len(), 2, "实际: {:?}", mc.paths);
        assert!(mc.paths.iter().any(|p| p == Path::new(r"C:\Pics\IMG_0001.JPG")));
        assert!(mc.paths.iter().any(|p| p == Path::new(r"C:\Pics\RAW\DSC.NEF")));
        assert_eq!(mc.excluded_dirs, 1, "thumbs 应计一次目录排除");

        // 根目录大小写不敏感(与折叠池口径一致)。
        let mc2 =
            mft_candidates(std::slice::from_ref(&v), Path::new(r"c:\pics"), &excl, None).unwrap();
        assert_eq!(mc2.paths.len(), 2);

        // 逐级失配 / 盘不在索引 → None(调用方回退 walkdir)。
        assert!(
            mft_candidates(std::slice::from_ref(&v), Path::new(r"C:\NoSuchDir"), &excl, None)
                .is_none()
        );
        assert!(mft_candidates(&[], Path::new(r"C:\Pics"), &excl, None).is_none());

        // 卷根直扫:整卷候选(排除 thumbs 子树)。
        let mc3 = mft_candidates(std::slice::from_ref(&v), Path::new(r"C:\"), &excl, None).unwrap();
        assert_eq!(mc3.paths.len(), 2, "实际: {:?}", mc3.paths);
        assert_eq!(mc3.excluded_dirs, 1);

        // 取消已置位 → 立即收手,但返回 Some(部分结果)而不是 None:
        // None 会让调用方回退 walkdir,那正是用户想中止的慢路径。
        let ctrl = ScanControl::new();
        ctrl.cancel();
        let mc4 = mft_candidates(
            std::slice::from_ref(&v),
            Path::new(r"C:\Pics"),
            &excl,
            Some(&ctrl),
        )
        .expect("取消不该退化成 walkdir 回退");
        assert!(mc4.paths.is_empty(), "第一次检查就该停,不收任何候选");
    }
}
