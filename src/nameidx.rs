//! 文件名 SoA 内存索引与快速查找(全卷名字列式索引)。
//!
//! 数据布局刻意做成**列式并行数组 + 名字池**,与 mft.rs 的 USN 记录流对接:
//! - `pool`:原始名字 UTF-8 顺序池(展示用);
//! - `fold`:ASCII 折叠(小写)副本(匹配用,子串查找直接 `memmem` 扫池,
//!   数百万条目单次查询毫秒级;UTF-8 自同步性保证不会跨字符误命中);
//! - `off/len/parent/frn/attrs`:定宽列,`parent` 存**条目下标**,拼路径时
//!   沿父链上溯,全卷不存完整路径字符串(省下数倍内存);
//! - `size/mtime/mtime_usn`:元数据列,直接读 `$MFT` 免费拿到(
//!   不必逐文件 `stat`),USN 降级路径下留 0;
//! - `cls`:类别位标记,建库时按扩展名一次算定,快查过滤免字符串比较。
//!
//! 多卷各建一份 `VolumeIndex`,查询时归并排序(前缀命中 > 名字长度)。
//! 常见的托管实现会把每条记录物化成 string + 字典,
//! 数百万条目内存膨胀到 GB 级 —— 本实现是其 SoA 化重写。

#![allow(dead_code)]

use anyhow::Result;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

/// 父列哨兵:父节点是卷根(如 `C:\` 下的一级目录)。
pub const PARENT_ROOT: u32 = u32::MAX;
/// 父列哨兵:父 FRN 在本卷记录流里找不到(系统元文件/枚举窗口期删除)。
pub const PARENT_ORPHAN: u32 = u32::MAX - 1;

/// FRN = (sequence << 48) | index;拼路径只比 index。
const FRN_INDEX_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// 卷根目录的 MFT 记录号。
const ROOT_FRN_INDEX: u64 = 5;
/// 属性位:目录(USN_RECORD_V2.FileAttributes,与 Win32 一致)。
pub const ATTR_DIRECTORY: u32 = 0x10;

/// `cls` 列的位标记(建库时按折叠扩展名一次算定,快查过滤不再逐条比字符串)。
pub const CLASS_IMAGE: u8 = 0x01;
pub const CLASS_VIDEO: u8 = 0x02;
pub const CLASS_AUDIO: u8 = 0x04;
pub const CLASS_ARCHIVE: u8 = 0x08;
pub const CLASS_EXE: u8 = 0x10;
pub const CLASS_DOC: u8 = 0x20;
/// 可入库派生 = 图片 | 视频,与 `scan::is_indexable` 同口径。
pub const FLAG_INDEXABLE: u8 = 0x40;

/// 单卷 SoA 名字索引。
#[derive(Debug, Default)]
pub struct VolumeIndex {
    pub letter: char,
    /// 原始名字 UTF-8 池(展示)。
    pool: Vec<u8>,
    /// ASCII 小写折叠池(匹配,与 pool 同布局)。
    fold: Vec<u8>,
    off: Vec<u32>,
    len: Vec<u32>,
    parent: Vec<u32>,
    frn: Vec<u64>,
    /// Win32 FILE_ATTRIBUTE_*(低 16 位)。
    attrs: Vec<u16>,
    /// 字节数,来自 `$DATA` RealSize;0 = 未知(降级路径拿不到)。
    size: Vec<u64>,
    /// 修改时间,UNIX 微秒(0 = 未知)。取 `$STANDARD_INFORMATION` 的 write time。
    mtime: Vec<u64>,
    /// USN 变更时刻,UNIX 微秒(0 = 本条不是由 journal 事件写进来的)。
    /// 与 `mtime` 语义不同:mtime 是文件内容被改的时间,mtime_usn 是
    /// 「我们观察到它变了」的时间,追赶/保鲜判定要用后者。
    mtime_usn: Vec<u64>,
    /// 类别位标记(`CLASS_*` / `FLAG_INDEXABLE`)。
    cls: Vec<u8>,
    /// 按折叠名排序的条目下标(前缀二分/有序遍历用)。
    order: Vec<u32>,
    /// 目录 → 子项 CSR 起点(n+2 长;槽位 n 是「卷根直属」虚拟桶)。
    csr_start: Vec<u32>,
    /// 目录 → 子项 CSR 子项列。
    csr_child: Vec<u32>,
}

/// 索引构建器:接 `$MFT` / USN 枚举回调,先零散收集,`finish` 时统一收束。
#[derive(Debug)]
pub struct VolumeIndexBuilder {
    letter: char,
    pool: Vec<u8>,
    off: Vec<u32>,
    len: Vec<u32>,
    frn: Vec<u64>,
    attrs: Vec<u16>,
    size: Vec<u64>,
    mtime: Vec<u64>,
    mtime_usn: Vec<u64>,
    cls: Vec<u8>,
    /// 构建期:FRN(index 位)→ 条目下标,父解析用完即弃。
    frn_to_idx: HashMap<u64, u32>,
    /// 构建期:原始父 FRN(含 sequence 位),finish 时解析成下标。
    raw_parent: Vec<u64>,
}

impl VolumeIndexBuilder {
    pub fn new(letter: char) -> Self {
        VolumeIndexBuilder {
            letter,
            pool: Vec::new(),
            off: Vec::new(),
            len: Vec::new(),
            frn: Vec::new(),
            attrs: Vec::new(),
            size: Vec::new(),
            mtime: Vec::new(),
            mtime_usn: Vec::new(),
            cls: Vec::new(),
            frn_to_idx: HashMap::new(),
            raw_parent: Vec::new(),
        }
    }

    /// 已收集条数。
    pub fn count(&self) -> u32 {
        self.frn.len() as u32
    }

    /// 追加一条 `$MFT` 记录(主路径:size/mtime 免费随记录一起拿到)。
    pub fn push_mft(&mut self, r: &crate::rec::MftRec) {
        let Some(name) = r.name.as_deref() else {
            return;
        };
        let units: Vec<u16> = name.encode_utf16().collect();
        // `$FILE_NAME` 的 Flags 才是 Win32 属性;万一没解出来,退回记录头的
        // dir 位(+0x16 bit1),至少别让目录被当成文件。
        let attrs = if r.file_attrs != 0 {
            r.file_attrs
        } else if r.flags & 0x02 != 0 {
            ATTR_DIRECTORY
        } else {
            0
        };
        self.push_inner(
            r.frn(),
            r.parent_frn,
            attrs,
            &units,
            r.data_size.unwrap_or(0),
            filetime_micros(r.si.map(|s| s[1]).unwrap_or(0)),
            0,
        );
    }

    /// 追加一条 USN 记录(降级路径:拿不到 size/mtime,只带变更时刻)。
    pub fn push_usn(&mut self, e: &crate::mft::UsnEntry<'_>) {
        self.push_inner(
            e.frn,
            e.parent_frn,
            e.attrs,
            e.name,
            0,
            0,
            filetime_micros(e.timestamp),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn push_inner(
        &mut self,
        frn: u64,
        parent_frn: u64,
        attrs: u32,
        name: &[u16],
        size: u64,
        mtime: u64,
        mtime_usn: u64,
    ) {
        // 卷根记录(FRN 5)名空父自指,不进索引 —— 根由 PARENT_ROOT 隐式表达。
        if frn & FRN_INDEX_MASK == ROOT_FRN_INDEX || name.is_empty() {
            return;
        }
        let mut s = String::from_utf16_lossy(name);
        s = sanitize_name(s);
        let cls = class_flags(&s);
        let off = self.pool.len() as u32;
        self.pool.extend_from_slice(s.as_bytes());
        self.off.push(off);
        self.len.push(s.len() as u32);
        self.frn.push(frn);
        self.attrs.push(attrs as u16);
        self.size.push(size);
        self.mtime.push(mtime);
        self.mtime_usn.push(mtime_usn);
        self.cls.push(cls);
        self.raw_parent.push(parent_frn);
        self.frn_to_idx
            .insert(frn & FRN_INDEX_MASK, self.off.len() as u32 - 1);
    }

    /// 父解析 → 折叠池 → 排序,产出只读索引。
    pub fn finish(self) -> VolumeIndex {
        let n = self.off.len();
        let mut parent = Vec::with_capacity(n);
        for p in &self.raw_parent {
            let idx = p & FRN_INDEX_MASK;
            let v = if idx == ROOT_FRN_INDEX {
                PARENT_ROOT
            } else {
                match self.frn_to_idx.get(&idx) {
                    Some(&i) => i,
                    None => PARENT_ORPHAN,
                }
            };
            parent.push(v);
        }
        let mut idx = VolumeIndex {
            letter: self.letter,
            pool: self.pool,
            fold: Vec::new(),
            off: self.off,
            len: self.len,
            parent,
            frn: self.frn,
            attrs: self.attrs,
            size: self.size,
            mtime: self.mtime,
            mtime_usn: self.mtime_usn,
            cls: self.cls,
            order: Vec::new(),
            csr_start: Vec::new(),
            csr_child: Vec::new(),
        };
        idx.post_process();
        idx
    }
}

/// 一条搜索命中。
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub letter: char,
    pub idx: u32,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

/// 每卷原始命中收集上限(超过则停止收集,交上层用更精确的词再查;
/// 防止查 "e" 之类的单字符在数百万条目上无限膨胀)。
const RAW_CAP: usize = 100_000;

/// 快速查找的类型过滤档(顶栏/搜索页下拉)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileClass {
    /// 不过滤。
    #[default]
    All,
    /// 图片(含相机 RAW,与 scan.rs 的收录口径一致)。
    Image,
    Video,
    Audio,
    Archive,
    /// 可执行(含脚本与安装包)。
    Executable,
    Document,
}

impl FileClass {
    /// 下拉框显示名。
    pub fn label(self) -> &'static str {
        match self {
            FileClass::All => "全部",
            FileClass::Image => "图片类",
            FileClass::Video => "视频类",
            FileClass::Audio => "音频类",
            FileClass::Archive => "压缩包类",
            FileClass::Executable => "可执行类",
            FileClass::Document => "文档类",
        }
    }

    /// 下拉框顺序。
    pub fn all() -> [FileClass; 7] {
        [
            FileClass::All,
            FileClass::Image,
            FileClass::Video,
            FileClass::Audio,
            FileClass::Archive,
            FileClass::Executable,
            FileClass::Document,
        ]
    }
}

/// 音频扩展名(常见格式)。
const AUDIO_EXTS: &[&str] = &["mp3", "wav", "flac", "aac", "ogg", "m4a", "wma", "opus", "ape", "mid"];
/// 压缩包/镜像扩展名。
const ARCHIVE_EXTS: &[&str] = &[
    "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "cab", "iso",
];
/// 可执行/脚本/安装包扩展名。
const EXE_EXTS: &[&str] = &[
    "exe", "dll", "sys", "msi", "bat", "cmd", "com", "scr", "ps1", "vbs", "jar", "apk",
];
/// 文档扩展名。
const DOC_EXTS: &[&str] = &[
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "md", "rtf", "odt", "ods", "odp",
    "epub", "mobi", "csv",
];

/// 扩展名是否属于指定类(All 恒真;无扩展名/未知扩展名只归 All)。
/// 扩展名表全为小写,与折叠池口径一致。
fn ext_in_class(ext: &[u8], class: FileClass) -> bool {
    if class == FileClass::All {
        return true;
    }
    let Ok(e) = std::str::from_utf8(ext) else {
        return false;
    };
    let hit = |list: &[&str]| list.contains(&e);
    match class {
        FileClass::All => true,
        FileClass::Image => {
            crate::scan::IMG_EXTS.contains(&e) || crate::scan::RAW_EXTS.contains(&e)
        }
        FileClass::Video => crate::video::MOVIE_EXTS.contains(&e),
        FileClass::Audio => hit(AUDIO_EXTS),
        FileClass::Archive => hit(ARCHIVE_EXTS),
        FileClass::Executable => hit(EXE_EXTS),
        FileClass::Document => hit(DOC_EXTS),
    }
}

/// 类别位标记:按折叠扩展名一次算定,`search_class` 之后只做位测试。
/// `FileClass::All` 恒真,不参与打标。
fn class_flags(name: &str) -> u8 {
    let ext: Vec<u8> = match name.rfind('.') {
        Some(p) if p + 1 < name.len() => name[p + 1..]
            .bytes()
            .map(|b| b.to_ascii_lowercase())
            .collect(),
        _ => Vec::new(),
    };
    let mut c = 0u8;
    if ext_in_class(&ext, FileClass::Image) {
        c |= CLASS_IMAGE;
    }
    if ext_in_class(&ext, FileClass::Video) {
        c |= CLASS_VIDEO;
    }
    if ext_in_class(&ext, FileClass::Audio) {
        c |= CLASS_AUDIO;
    }
    if ext_in_class(&ext, FileClass::Archive) {
        c |= CLASS_ARCHIVE;
    }
    if ext_in_class(&ext, FileClass::Executable) {
        c |= CLASS_EXE;
    }
    if ext_in_class(&ext, FileClass::Document) {
        c |= CLASS_DOC;
    }
    if c & (CLASS_IMAGE | CLASS_VIDEO) != 0 {
        c |= FLAG_INDEXABLE;
    }
    c
}

/// `FileClass` → 对应位标记(All 恒真,返回 0 表示不过滤)。
fn class_bit(class: FileClass) -> u8 {
    match class {
        FileClass::All => 0,
        FileClass::Image => CLASS_IMAGE,
        FileClass::Video => CLASS_VIDEO,
        FileClass::Audio => CLASS_AUDIO,
        FileClass::Archive => CLASS_ARCHIVE,
        FileClass::Executable => CLASS_EXE,
        FileClass::Document => CLASS_DOC,
    }
}

// ===================== term 链表模型 =====================
//
// 查询编译:把搜索文本编成一张 term 表。
// - **空格分词 → 词间 AND**;
// - **词内 `|` → OR**,`\|` 与 `\\` 是转义(其余位置的反斜杠原样保留,
//   免得用户粘一条路径进来被吃掉字符);
// - 谓词 `ext:` / `size:` / `date:`。Windows 文件名里 `:` 是非法字符,
//   所以「冒号前是已知谓词名」这个判定不会有歧义;未知前缀(如 `foo:bar`)
//   按字面名字处理 —— 它必然搜不到,但不会静默变成别的意思。

/// 谓词统一收成**闭区间**:比较符、`A..B` 区间、裸值(精确)三种写法都归到它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Range {
    lo: u64,
    hi: u64,
}

impl Range {
    fn contains(self, v: u64) -> bool {
        (self.lo..=self.hi).contains(&v)
    }
    /// 把有符号的秒区间夹成 `Range`(负数一律夹到 0)。
    fn from_secs(lo: i64, hi: i64) -> Range {
        Range {
            lo: lo.max(0) as u64,
            hi: hi.max(0) as u64,
        }
    }
}

/// 比较符;没有比较符时按 `=`(精确)处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

/// 剥掉前缀比较符,返回 (比较符, 剩下的值文本)。
///
/// 两字符的 `>=` / `<=` 必须排在单字符前面试,否则 `>=10mb` 会被读成
/// `>` + `=10mb`,后半截解析失败整词作废。
fn split_op(s: &str) -> (Cmp, &str) {
    for (lit, c) in [
        (">=", Cmp::Ge),
        ("<=", Cmp::Le),
        ("=", Cmp::Eq),
        (">", Cmp::Gt),
        ("<", Cmp::Lt),
    ] {
        if let Some(rest) = s.strip_prefix(lit) {
            return (c, rest.trim_start());
        }
    }
    (Cmp::Eq, s)
}

/// 比较符 → 闭区间。`v` 是已经解析好的数值。
fn range_from_cmp(c: Cmp, v: u64) -> Range {
    match c {
        Cmp::Gt => Range {
            lo: v.saturating_add(1),
            hi: u64::MAX,
        },
        Cmp::Ge => Range { lo: v, hi: u64::MAX },
        Cmp::Lt => Range {
            lo: 0,
            hi: v.saturating_sub(1),
        },
        Cmp::Le => Range { lo: 0, hi: v },
        Cmp::Eq => Range { lo: v, hi: v },
    }
}

/// 解析 `[op]值` 或 `值..值`,数值部分交给 `val`。
fn parse_range(s: &str, val: impl Fn(&str) -> Option<u64>) -> Option<Range> {
    let t = s.trim();
    if let Some((a, b)) = t.split_once("..") {
        // 区间两端不带比较符;写反了(大..小)自动对调,不当成空集。
        let (x, y) = (val(a)?, val(b)?);
        return Some(if x <= y {
            Range { lo: x, hi: y }
        } else {
            Range { lo: y, hi: x }
        });
    }
    let (c, rest) = split_op(t);
    Some(range_from_cmp(c, val(rest)?))
}

/// 体积单位首字母 → 1024 的幂(与资源管理器口径一致,不是 1000)。
fn size_pow(first: u8) -> Option<u32> {
    Some(match first.to_ascii_lowercase() {
        b'b' => 0,
        b'k' => 1,
        b'm' => 2,
        b'g' => 3,
        b't' => 4,
        b'p' => 5,
        _ => return None,
    })
}

/// 解析一个体积字面量:`1024` / `10mb` / `10m` / `10mib` 都收;裸数字按字节。
fn parse_size_value(s: &str) -> Option<u64> {
    let s = s.trim();
    let n = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    if n == 0 {
        return None;
    }
    let v: u64 = s[..n].parse().ok()?;
    let unit = s[n..].trim();
    if unit.is_empty() {
        return Some(v);
    }
    let pow = size_pow(unit.as_bytes()[0])?;
    // 单位尾巴只允许 `b` / `ib`,别的(kbytes、ko 之类)一律拒 —— 拒了整词作废,
    // 好过悄悄按 1024^k 算出一个用户没写的数。
    let tail = &unit[1..];
    if !tail.is_empty() && !tail.eq_ignore_ascii_case("b") && !tail.eq_ignore_ascii_case("ib") {
        return None;
    }
    v.checked_mul(1024u64.pow(pow))
}

/// 某年某月的最后一天(1..=12 已由调用方保证)。
fn last_day_of_month(y: i32, m: u32) -> u32 {
    use chrono::Datelike;
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    chrono::NaiveDate::from_ymd_opt(ny, nm, 1)
        .and_then(|d| d.pred_opt())
        .map(|d| d.day())
        .unwrap_or(28)
}

/// 本地日历日的起点(00:00:00)→ UNIX 秒。
///
/// 用 `earliest()` 而不是 `single()`:夏令时切换那天 00:00 可能不存在或有歧义,
/// `single()` 会返回 None 把整天判成非法日期。
fn local_day_start(y: i32, m: u32, d: u32) -> Option<i64> {
    use chrono::TimeZone;
    let naive = chrono::NaiveDate::from_ymd_opt(y, m, d)?.and_hms_opt(0, 0, 0)?;
    Some(chrono::Local.from_local_datetime(&naive).earliest()?.timestamp())
}

/// 本地日历日的终点(23:59:59)→ UNIX 秒。
fn local_day_end(y: i32, m: u32, d: u32) -> Option<i64> {
    use chrono::TimeZone;
    let naive = chrono::NaiveDate::from_ymd_opt(y, m, d)?.and_hms_opt(23, 59, 59)?;
    Some(chrono::Local.from_local_datetime(&naive).latest()?.timestamp())
}

/// 一个（可能只写到年或月的）日期字面量 → 它覆盖的整段 [起, 止](UNIX 秒)。
///
/// `date:2024` = 整个 2024 年,`date:2024-05` = 整个五月 —— 用户在资源管理器里
/// 看到的就是这个粒度,所以裸日期一律展开成**整段**而不是某一天。
/// `/` 与 `-` 都当分隔符(两种写法都很常见)。
fn date_period(s: &str) -> Option<(i64, i64)> {
    let s = s.trim().replace('/', "-");
    let parts: Vec<&str> = s.split('-').collect();
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let y: i32 = parts[0].parse().ok()?;
    if !(1970..=9999).contains(&y) {
        return None;
    }
    let (sm, sd, em, ed) = match parts.len() {
        1 => (1, 1, 12, 31),
        2 => {
            let m: u32 = parts[1].parse().ok()?;
            if !(1..=12).contains(&m) {
                return None;
            }
            (m, 1, m, last_day_of_month(y, m))
        }
        _ => {
            let m: u32 = parts[1].parse().ok()?;
            let d: u32 = parts[2].parse().ok()?;
            (m, d, m, d)
        }
    };
    Some((local_day_start(y, sm, sd)?, local_day_end(y, em, ed)?))
}

/// 解析 `date:` 的值(区间语义见 [`date_period`])。
///
/// 比较符是相对**整段**说的:`date:>2024-05` = 五月结束以后,
/// `date:>=2024-05` = 五月一号起(含整个五月)。
fn parse_date_range(s: &str) -> Option<Range> {
    let t = s.trim();
    if let Some((a, b)) = t.split_once("..") {
        let (alo, _) = date_period(a)?;
        let (_, bhi) = date_period(b)?;
        let (lo, hi) = if alo <= bhi { (alo, bhi) } else { (bhi, alo) };
        return Some(Range::from_secs(lo, hi));
    }
    let (c, rest) = split_op(t);
    let (start, end) = date_period(rest)?;
    Some(match c {
        Cmp::Gt => Range {
            lo: (end + 1).max(0) as u64,
            hi: u64::MAX,
        },
        Cmp::Ge => Range {
            lo: start.max(0) as u64,
            hi: u64::MAX,
        },
        Cmp::Lt => Range::from_secs(0, start - 1),
        Cmp::Le => Range::from_secs(0, end),
        Cmp::Eq => Range::from_secs(start, end),
    })
}

/// 一个候选(词内 `|` 分隔 → OR)。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Alt {
    /// 名字子串,已折叠成 ASCII 小写(与折叠池同口径)。
    Name(Vec<u8>),
    /// `ext:` 扩展名,已折叠小写、不含点(与 `class_flags` 建库口径一致)。
    Ext(Vec<u8>),
    /// `size:` 体积区间(字节)。
    Size(Range),
    /// `date:` 修改日期区间(UNIX 秒)。
    Date(Range),
}

/// 一个查询词;词间空格分隔 → AND。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Term {
    alts: Vec<Alt>,
}

impl Term {
    /// 本词里最长的名字子串长度(分块时算跨块重叠窗口用)。
    fn name_len(&self) -> usize {
        self.alts
            .iter()
            .map(|a| match a {
                Alt::Name(n) => n.len(),
                _ => 0,
            })
            .max()
            .unwrap_or(0)
    }
}

/// 把搜索文本编成 term 表(查询编译器入口)。
///
/// 空词(连续空格、裸 `|`)直接丢弃;整条查询编不出任何词 → 空表。
/// **空表 ≠ 全命中**:调用方必须把它当「没有命中」处理,否则写错的
/// `size:abc` 会把整盘文件倒出来。
fn compile_query(q: &str) -> Vec<Term> {
    q.split_whitespace().filter_map(compile_term).collect()
}

fn compile_term(tok: &str) -> Option<Term> {
    // **前导谓词管住整个词**:`ext:jpg|png` 是「扩展名 jpg 或 png」,不是
    // 「扩展名 jpg 或名字含 png」。与参考件一致 —— 它先认出词的类型,再把
    // **值**按未转义 `|` 切成候选数组去二分。
    if let Some((head, rest)) = tok.split_once(':') {
        let head: String = head.chars().map(|c| c.to_ascii_lowercase()).collect();
        if matches!(head.as_str(), "ext" | "size" | "date") {
            let alts: Vec<Alt> = split_or(rest)
                .into_iter()
                .filter_map(|p| compile_prefixed(&head, &p))
                .collect();
            return (!alts.is_empty()).then_some(Term { alts });
        }
    }
    // 没有前导谓词:逐候选各自判类型(`a|ext:jpg` = 名字含 a 或 扩展名 jpg)。
    let alts: Vec<Alt> = split_or(tok)
        .into_iter()
        .filter_map(|p| compile_alt(&p))
        .collect();
    (!alts.is_empty()).then_some(Term { alts })
}

/// 已知谓词前缀 + 一个候选值 → `Alt`;值非法则该候选作废。
fn compile_prefixed(head: &str, part: &str) -> Option<Alt> {
    let p = part.trim();
    if p.is_empty() {
        return None;
    }
    match head {
        "ext" => {
            let e = fold_ext_lit(p);
            (!e.is_empty()).then_some(Alt::Ext(e))
        }
        "size" => parse_range(p, parse_size_value).map(Alt::Size),
        "date" => parse_date_range(p).map(Alt::Date),
        _ => None,
    }
}

/// 按**未转义**的 `|` 切分,并把 `\|` / `\\` 还原成字面量。
///
/// 只在后面紧跟 `|` 或 `\` 时才把反斜杠当转义符:别的位置原样保留,
/// 免得用户粘一条 `C:\照片` 进来被吃掉一个字符。
fn split_or(tok: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut it = tok.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' if matches!(it.peek(), Some('|') | Some('\\')) => {
                if let Some(n) = it.next() {
                    cur.push(n);
                }
            }
            '|' => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// `ext:` 的字面量归一:去前导点 + 折叠小写。
fn fold_ext_lit(s: &str) -> Vec<u8> {
    s.trim()
        .trim_start_matches('.')
        .bytes()
        .map(|b| b.to_ascii_lowercase())
        .collect()
}

fn compile_alt(part: &str) -> Option<Alt> {
    let p = part.trim();
    if p.is_empty() {
        return None;
    }
    if let Some((head, rest)) = p.split_once(':') {
        let head: String = head.chars().map(|c| c.to_ascii_lowercase()).collect();
        match head.as_str() {
            "ext" => {
                let e = fold_ext_lit(rest);
                return (!e.is_empty()).then_some(Alt::Ext(e));
            }
            "size" => return parse_range(rest, parse_size_value).map(Alt::Size),
            "date" => return parse_date_range(rest).map(Alt::Date),
            _ => {}
        }
    }
    let n: Vec<u8> = p.bytes().map(|b| b.to_ascii_lowercase()).collect();
    (!n.is_empty()).then_some(Alt::Name(n))
}

// ===================== 分块并行扫描 =====================

/// 每块的条目数:`n_chunks = ceil(记录数 / 4096)`。
const CHUNK_RECORDS: usize = 4096;

/// 块数 = `ceil(条目数 / 4096)`,再被 CPU 核数夹紧(
/// `n_chunks` 被两个最大线程数全局夹紧)。`<= 1` 时调用方就地单线程,不起 rayon。
fn chunk_count(n: usize) -> usize {
    n.div_ceil(CHUNK_RECORDS).clamp(1, crate::win::cpu_count())
}

/// 单卷扫描:分块并行,块结果**按块序合并**。
///
/// 按块序合并是这条设计的要点(每块工作项逐个等回,并按块序并入
/// 同一结果集):rayon 的完成顺序是不定的,直接 `collect` 到扁平列表会让
/// 同一条查询两次跑出不同的并列次序 —— 结果就不可复现了。
/// `Range::into_par_iter` 是 IndexedParallelIterator,`collect::<Vec<_>>()`
/// 保下标序,所以合并天然按块序。
fn scan_volume(v: &VolumeIndex, terms: &[Term], bit: u8, overlap: usize) -> Vec<(u32, usize)> {
    use rayon::prelude::*;
    let n = v.count();
    if n == 0 {
        return Vec::new();
    }
    let chunks = chunk_count(n);
    if chunks <= 1 {
        return scan_chunk(v, terms, bit, 0, n, overlap, RAW_CAP);
    }
    let step = n.div_ceil(chunks);
    (0..chunks)
        .into_par_iter()
        .map(|c| {
            let lo = c * step;
            let hi = (lo + step).min(n);
            scan_chunk(v, terms, bit, lo, hi, overlap, RAW_CAP)
        })
        .collect::<Vec<Vec<(u32, usize)>>>()
        .into_iter()
        .flatten()
        .collect()
}

/// 单卷单块的命中收集。
///
/// `lo..hi` 是**条目下标区间**。词间 AND 用一张 `n` 长的 0/1 表逐词收紧,
/// 任一词把表清空就立刻整块早退(`size:>1tb` 这种词在绝大多数块上都是空的)。
///
/// 排名键 `best_pos` 取**所有名字词里的最小命中位**:单词查询时它与旧实现
/// 逐字节一致,多词查询时的语义是「最早出现的那个词有多靠前」。
fn scan_chunk(
    v: &VolumeIndex,
    terms: &[Term],
    bit: u8,
    lo: usize,
    hi: usize,
    overlap: usize,
    cap: usize,
) -> Vec<(u32, usize)> {
    if lo >= hi {
        return Vec::new();
    }
    let n = hi - lo;
    let mut mark: Vec<u8> = vec![1u8; n];
    let mut live = n;
    let mut best_pos: Vec<usize> = vec![usize::MAX; n];

    for t in terms {
        let mut cur: Vec<u8> = vec![0u8; n];
        for alt in &t.alts {
            match alt {
                Alt::Name(needle) => {
                    for (idx, rel) in v.find_in(lo, hi, overlap, needle, cap) {
                        let k = idx as usize - lo;
                        cur[k] = 1;
                        if rel < best_pos[k] {
                            best_pos[k] = rel;
                        }
                    }
                }
                Alt::Ext(e) => {
                    for i in lo..hi {
                        if v.fold_ext(i) == e.as_slice() {
                            cur[i - lo] = 1;
                        }
                    }
                }
                // size/mtime 列的 0 在 USN 降级路径上表示「拿不到」,
                // 不能当成「0 字节文件」或「1970 年」去命中区间。
                Alt::Size(r) => {
                    for i in lo..hi {
                        let s = v.size[i];
                        if s != 0 && r.contains(s) {
                            cur[i - lo] = 1;
                        }
                    }
                }
                Alt::Date(r) => {
                    for i in lo..hi {
                        let us = v.mtime[i];
                        if us != 0 && r.contains(us / 1_000_000) {
                            cur[i - lo] = 1;
                        }
                    }
                }
            }
        }
        live = 0;
        for k in 0..n {
            mark[k] &= cur[k];
            live += mark[k] as usize;
        }
        if live == 0 {
            return Vec::new();
        }
    }

    let mut out = Vec::with_capacity(live.min(cap));
    for k in 0..n {
        if mark[k] == 0 {
            continue;
        }
        let i = lo + k;
        // 类别在建库时已折成位标记,这里只做一次位测试(All → bit=0 恒过)。
        if bit != 0 && v.cls[i] & bit == 0 {
            continue;
        }
        out.push((i as u32, best_pos[k]));
        if out.len() >= cap {
            break;
        }
    }
    out
}

/// FILETIME → UNIX 微秒;不可表示(含 0 = 没取到)统一记 0。
fn filetime_micros(t: u64) -> u64 {
    match crate::rec::filetime_to_system(t) {
        Some(s) => s
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0),
        None => 0,
    }
}

/// 名字成分不允许出现分隔符/控制符(理论不可能,防御性收口)。
fn sanitize_name(mut s: String) -> String {
    if s.contains(['\\', '/', '\0']) {
        s = s.replace(['\\', '/', '\0'], "_");
    }
    s
}

/// `parent` 列原始值 → CSR 槽位:卷根哨兵落到虚拟桶 `root`(= 条目数),
/// 有效下标原样返回,孤儿返回 None(不挂任何桶)。
fn parent_slot(p: u32, root: usize) -> Option<usize> {
    if p == PARENT_ROOT {
        Some(root)
    } else if (p as usize) < root {
        Some(p as usize)
    } else {
        None
    }
}

/// 有序视图的统一比较口径:按折叠池里的名字字节序。
/// 全量重排(`post_process`)与增量归并(`merge_new_into_order`)共用,
/// 两边口径一旦分叉,查询二分就会静默漏命中。
fn folded_cmp(fold: &[u8], off: &[u32], len: &[u32], a: u32, b: u32) -> std::cmp::Ordering {
    let sa = off[a as usize] as usize;
    let la = len[a as usize] as usize;
    let sb = off[b as usize] as usize;
    let lb = len[b as usize] as usize;
    fold[sa..sa + la].cmp(&fold[sb..sb + lb])
}

/// 一条增量操作(与 journal 记录同序,顺序即语义:删除后重建是合法序列)。
#[derive(Debug, Clone)]
pub enum DeltaOp {
    /// 创建/改名/移动的 CLOSE 终态:以记录里的新名字 + 新父目录为准。
    Upsert(DeltaUpsert),
    /// 删除(或改名旧名)。
    Delete(u64),
}

/// `DeltaOp::Upsert` 的载荷。
#[derive(Debug, Clone)]
pub struct DeltaUpsert {
    pub frn: u64,
    pub parent_frn: u64,
    pub attrs: u32,
    pub name: String,
    /// 字节数;0 = 本次事件拿不到(USN 记录里没有 size)。
    pub size: u64,
    /// 修改时间,UNIX 微秒;0 = 拿不到。
    pub mtime: u64,
    /// 本次事件的变更时刻,UNIX 微秒(来自 USN 记录 +0x20)。
    pub mtime_usn: u64,
}

/// 一次增量合并的统计(报告与测试用)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeltaCounts {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
}

/// 查询入口:所有卷归并排序后的结果(已截断到 limit)。
///
/// 排序口径:命中位靠前(前缀) > 名字更短 > 折叠名字典序
/// (即「精确/靠前优先」,不引入完整相关度模型)。
pub fn search(vols: &[VolumeIndex], query: &str, limit: usize) -> Vec<SearchHit> {
    search_class(vols, query, limit, FileClass::All)
}

/// 同 `search`,按扩展名类过滤(`FileClass::All` 等价 `search`)。
/// 过滤在收集期进行,未命中类别的候选不占展示名额。
///
/// 查询语法见本文件的 term 模型一节:空格 AND、`|` OR(转义 `\|`)、
/// `ext:` / `size:` / `date:` 谓词。收集是分块并行的,
/// 块结果按块序合并,所以同一条查询的结果次序可复现。
pub fn search_class(
    vols: &[VolumeIndex],
    query: &str,
    limit: usize,
    class: FileClass,
) -> Vec<SearchHit> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let terms = compile_query(q);
    if terms.is_empty() {
        // 谓词写错(`size:abc`、`ext:`)→ 没有命中,**不是**全命中。
        return Vec::new();
    }
    // 跨块重叠窗口:最长名字子串少一个字节就够(子串起点落在上一块里)。
    let overlap = terms.iter().map(Term::name_len).max().unwrap_or(0).saturating_sub(1);
    let bit = class_bit(class);
    // (卷序, 条目下标, 命中字节位)
    let mut hits: Vec<(usize, u32, usize)> = Vec::new();
    for (vi, v) in vols.iter().enumerate() {
        let mut per_vol = scan_volume(v, &terms, bit, overlap);
        per_vol.truncate(RAW_CAP);
        hits.extend(per_vol.into_iter().map(|(idx, pos)| (vi, idx, pos)));
    }
    hits.sort_by(|&(va, a, pa), &(vb, b, pb)| {
        let (va_i, vb_i) = (&vols[va], &vols[vb]);
        (pa, va_i.name_len(a as usize))
            .cmp(&(pb, vb_i.name_len(b as usize)))
            .then_with(|| va_i.fold_name(a as usize).cmp(vb_i.fold_name(b as usize)))
            .then_with(|| (va, a).cmp(&(vb, b)))
    });
    hits.truncate(limit);
    hits.into_iter()
        .map(|(vi, idx, _)| {
            let v = &vols[vi];
            SearchHit {
                letter: v.letter,
                idx,
                name: v.name(idx as usize),
                path: v.path_of(idx as usize),
                is_dir: v.is_dir(idx as usize),
            }
        })
        .collect()
}

impl VolumeIndex {
    /// 一卷的空索引(占位用:服务要求 `vols` 与 `meta` 同序同长,
    /// 还没建库的卷也得有位子,否则下标会串)。
    pub fn empty(letter: char) -> VolumeIndex {
        VolumeIndex {
            letter,
            ..Default::default()
        }
    }

    /// 把 journal 增量按序并入索引:删除移除条目,创建/改名/移动更新或追加,
    /// 触碰条目的父引用经 FRN 重解析(父为卷根 → PARENT_ROOT,找不到/已删
    /// → PARENT_ORPHAN),最后重建折叠池与有序视图。
    /// 复杂度 O(旧条目数 + 增量),每卷每次启动跑一次,百万条目亚秒级。
    pub fn apply_delta(&mut self, ops: Vec<DeltaOp>) -> DeltaCounts {
        let mut counts = DeltaCounts::default();
        if ops.is_empty() {
            return counts;
        }
        // 快路径基准:纯新增轮次只有池尾和列尾是新的,收尾可以只做增量
        // (见 apply_delta 尾部),这两个基准点用来圈出"新"的范围。
        let pool_before = self.pool.len();
        let entries_before = self.off.len();
        let mut frn_to_idx: HashMap<u64, u32> = self
            .frn
            .iter()
            .enumerate()
            .map(|(i, &f)| (f & FRN_INDEX_MASK, i as u32))
            .collect();
        let mut removed: Vec<u32> = Vec::new();
        let mut touched: Vec<(u32, u64)> = Vec::new();
        for op in ops {
            match op {
                DeltaOp::Delete(frn) => {
                    if let Some(idx) = frn_to_idx.remove(&(frn & FRN_INDEX_MASK)) {
                        removed.push(idx);
                        counts.removed += 1;
                    }
                }
                DeltaOp::Upsert(u) => {
                    let key = u.frn & FRN_INDEX_MASK;
                    let name = sanitize_name(u.name);
                    let cls = class_flags(&name);
                    let off = self.pool.len() as u32;
                    self.pool.extend_from_slice(name.as_bytes());
                    match frn_to_idx.get(&key) {
                        Some(&idx) => {
                            // 原位更新:名字指针指向池尾新副本,旧字节成为
                            // 死空间(下次全量重建时回收,增量不搬运)。
                            let i = idx as usize;
                            self.off[i] = off;
                            self.len[i] = name.len() as u32;
                            self.attrs[i] = u.attrs as u16;
                            self.cls[i] = cls;
                            // USN 记录里没有 size/mtime,0 在这条通道上就是
                            // 「拿不到」,不能拿它把建库时从 $MFT 读到的真值抹掉。
                            if u.size != 0 {
                                self.size[i] = u.size;
                            }
                            if u.mtime != 0 {
                                self.mtime[i] = u.mtime;
                            }
                            self.mtime_usn[i] = u.mtime_usn;
                            touched.push((idx, u.parent_frn));
                            counts.updated += 1;
                        }
                        None => {
                            let idx = self.off.len() as u32;
                            self.off.push(off);
                            self.len.push(name.len() as u32);
                            self.parent.push(PARENT_ORPHAN);
                            self.frn.push(u.frn);
                            self.attrs.push(u.attrs as u16);
                            self.size.push(u.size);
                            self.mtime.push(u.mtime);
                            self.mtime_usn.push(u.mtime_usn);
                            self.cls.push(cls);
                            frn_to_idx.insert(key, idx);
                            touched.push((idx, u.parent_frn));
                            counts.added += 1;
                        }
                    }
                }
            }
        }
        for (idx, pfrn) in touched {
            let pidx = pfrn & FRN_INDEX_MASK;
            self.parent[idx as usize] = if pidx == ROOT_FRN_INDEX {
                PARENT_ROOT
            } else {
                frn_to_idx.get(&pidx).copied().unwrap_or(PARENT_ORPHAN)
            };
        }
        if counts.added + counts.updated + counts.removed == 0 {
            // 整批增量没有落到任何条目上(删的都是不认识的 FRN)——
            // 列与名字池一个字节都没动,重建折叠池/有序视图纯属白干。
            return counts;
        }
        // 压实只在列不变量真被破坏时才做:
        // - 删除:要摘除条目并重映射父引用;
        // - 原位更新:新名字副本追加到池尾,off 列失去单调性
        //   (entry_at 的二分依赖它),顺带回收死字节。
        // 纯新增两样都不破坏(off/len/parent/frn/attrs 一律尾插),
        // 跳过压实就省掉一次全池重写 —— 保鲜节奏下这是常态。
        if !removed.is_empty() || counts.updated > 0 {
            self.compact(&removed);
        }
        if removed.is_empty() && counts.updated == 0 {
            // 纯新增轮次走增量收尾:这一轮只有池尾追加了新名字、列尾追加了
            // 新条目,折叠池只需追加新字节的折叠,有序视图只需把新下标归并
            // 进去 —— 不必把百万条全部重排。那一步在服务里拿着 state 锁做,
            // C: 卷每秒都有增量,全量重排一次几百毫秒,期间所有快查/Stat
            // 都在排队(查询延迟 2ms↔900ms 双峰,即此因)。
            self.fold
                .extend(self.pool[pool_before..].iter().map(|b| b.to_ascii_lowercase()));
            self.merge_new_into_order(entries_before);
            self.build_csr();
            return counts;
        }
        self.post_process();
        counts
    }

    /// 把尾部新增的条目(下标 `from..`)按折叠名归并进有序视图。
    ///
    /// 单趟归并 O(n + k·log k),替代全量重排的 O(n·log n);保鲜节奏下
    /// k 通常是个位数。名字唯一的条目归并结果与全量重排逐位一致
    /// (并列名不保证,与 `sort_unstable` 的并列序一样任取)。
    fn merge_new_into_order(&mut self, from: usize) {
        if from >= self.off.len() {
            return;
        }
        let mut adds: Vec<u32> = (from as u32..self.off.len() as u32).collect();
        let VolumeIndex {
            fold, off, len, order, ..
        } = self;
        adds.sort_unstable_by(|&a, &b| folded_cmp(fold, off, len, a, b));
        let mut merged = Vec::with_capacity(order.len() + adds.len());
        let (mut i, mut j) = (0usize, 0usize);
        while i < order.len() && j < adds.len() {
            if folded_cmp(fold, off, len, order[i], adds[j]) != std::cmp::Ordering::Greater {
                merged.push(order[i]);
                i += 1;
            } else {
                merged.push(adds[j]);
                j += 1;
            }
        }
        merged.extend_from_slice(&order[i..]);
        merged.extend_from_slice(&adds[j..]);
        *order = merged;
    }

    /// 按当前条目序重建各列与名字池(`removed` 里的下标被摘除;
    /// 父被删的幸存条目降级孤儿,路径退化为自身名字)。
    fn compact(&mut self, removed: &[u32]) {
        let rm: std::collections::HashSet<u32> = removed.iter().copied().collect();
        let n = self.off.len();
        let mut remap = vec![u32::MAX; n];
        let mut keep: Vec<usize> = Vec::with_capacity(n.saturating_sub(removed.len()));
        for (i, slot) in remap.iter_mut().enumerate() {
            if !rm.contains(&(i as u32)) {
                *slot = keep.len() as u32;
                keep.push(i);
            }
        }
        let mut pool = Vec::with_capacity(self.pool.len());
        let mut off = Vec::with_capacity(keep.len());
        let mut len = Vec::with_capacity(keep.len());
        let mut parent = Vec::with_capacity(keep.len());
        let mut frn = Vec::with_capacity(keep.len());
        let mut attrs = Vec::with_capacity(keep.len());
        let mut size = Vec::with_capacity(keep.len());
        let mut mtime = Vec::with_capacity(keep.len());
        let mut mtime_usn = Vec::with_capacity(keep.len());
        let mut cls = Vec::with_capacity(keep.len());
        for i in keep {
            let s = self.off[i] as usize;
            let l = self.len[i] as usize;
            off.push(pool.len() as u32);
            pool.extend_from_slice(&self.pool[s..s + l]);
            len.push(l as u32);
            parent.push(match self.parent[i] {
                PARENT_ROOT | PARENT_ORPHAN => self.parent[i],
                p => match remap[p as usize] {
                    u32::MAX => PARENT_ORPHAN,
                    np => np,
                },
            });
            frn.push(self.frn[i]);
            attrs.push(self.attrs[i]);
            size.push(self.size[i]);
            mtime.push(self.mtime[i]);
            mtime_usn.push(self.mtime_usn[i]);
            cls.push(self.cls[i]);
        }
        self.pool = pool;
        self.off = off;
        self.len = len;
        self.parent = parent;
        self.frn = frn;
        self.attrs = attrs;
        self.size = size;
        self.mtime = mtime;
        self.mtime_usn = mtime_usn;
        self.cls = cls;
    }

    /// 构建/载入共用的收尾:重建折叠池(ASCII 小写,查询折叠口径一致,
    /// CJK 不受影响)、按名排序的有序视图,以及目录→子项 CSR。
    fn post_process(&mut self) {
        self.fold.clear();
        self.fold.reserve(self.pool.len());
        self.fold
            .extend(self.pool.iter().map(|b| b.to_ascii_lowercase()));
        let n = self.off.len();
        self.order.clear();
        self.order.extend(0..n as u32);
        let VolumeIndex {
            fold, off, len, order, ..
        } = self;
        order.sort_unstable_by(|&a, &b| folded_cmp(fold, off, len, a, b));
        self.build_csr();
    }

    /// 重建目录→子项 CSR(压缩邻接表)。
    ///
    /// 与折叠池/有序视图同批收尾:建库、载入、增量各跑一次 O(n),
    /// 之后 `mft_candidates` 式的目录展开直接切片取子项,不用每次现搭
    /// `Vec<Vec<u32>>`(百万条目一次就是几十 MB 的散列分配)。
    /// 槽位 `n` 是「卷根直属」虚拟桶;孤儿条目不挂任何桶。
    fn build_csr(&mut self) {
        let n = self.off.len();
        let root = n;
        let mut deg = vec![0u32; n + 1];
        for &p in &self.parent {
            if let Some(slot) = parent_slot(p, root) {
                deg[slot] += 1;
            }
        }
        let mut start = Vec::with_capacity(n + 2);
        let mut acc = 0u32;
        for d in &deg {
            start.push(acc);
            acc += d;
        }
        start.push(acc);
        let mut child = vec![0u32; acc as usize];
        let mut cur = start.clone();
        for (i, &p) in self.parent.iter().enumerate() {
            if let Some(slot) = parent_slot(p, root) {
                child[cur[slot] as usize] = i as u32;
                cur[slot] += 1;
            }
        }
        self.csr_start = start;
        self.csr_child = child;
    }

    /// 条目 `i` 的子项下标切片;`i == count()` 取「卷根直属」桶。
    pub fn children_of(&self, i: usize) -> &[u32] {
        if i + 1 >= self.csr_start.len() {
            return &[];
        }
        let a = self.csr_start[i] as usize;
        let b = self.csr_start[i + 1] as usize;
        &self.csr_child[a..b]
    }

    /// 条目是否目录。
    pub fn is_dir(&self, i: usize) -> bool {
        self.attrs[i] & (ATTR_DIRECTORY as u16) != 0
    }

    /// 条目数。
    pub fn count(&self) -> usize {
        self.off.len()
    }

    /// 父列原始值(下标 / `PARENT_ROOT` / `PARENT_ORPHAN`)。
    /// 子树枚举(scan.rs MFT 快路建邻接表)用。
    pub fn parent_of(&self, i: usize) -> u32 {
        self.parent[i]
    }

    /// 原始名字(展示)。
    pub fn name(&self, i: usize) -> String {
        String::from_utf8_lossy(self.name_bytes(i)).into_owned()
    }

    /// 沿父链重建完整路径(`C:\dir\sub\name`)。
    /// 孤儿条目(父不在流中)停在断点,路径可能不完整 —— 少数系统元文件,
    /// 正常磁盘数据不受影响。
    pub fn path_of(&self, i: usize) -> String {
        let mut parts: Vec<&[u8]> = Vec::with_capacity(8);
        let mut cur = i;
        let mut depth = 0;
        while depth < 64 {
            parts.push(self.name_bytes(cur));
            match self.parent[cur] {
                PARENT_ROOT => break,
                PARENT_ORPHAN => break,
                p if p < self.off.len() as u32 => {
                    cur = p as usize;
                    depth += 1;
                }
                _ => break,
            }
        }
        let mut out = String::with_capacity(64);
        out.push(self.letter);
        out.push_str(":\\");
        for (k, p) in parts.iter().rev().enumerate() {
            if k > 0 {
                out.push('\\');
            }
            out.push_str(&String::from_utf8_lossy(p));
        }
        out
    }

    /// 条目的完整 FRN(含序号位)。L2 的身份列 frn64 由它算出。
    pub fn frn_of(&self, i: usize) -> u64 {
        self.frn[i]
    }

    /// 完整路径 → 条目下标(逐层沿 CSR 子项匹配,大小写不敏感)。
    ///
    /// 服务的 `Stat` 走这里:逐层下降每层只比该目录的子项,
    /// 比"给全表拼路径再比较"便宜几个量级。卷标不符/卷根/未命中一律 `None`。
    pub fn lookup_path(&self, path: &str) -> Option<usize> {
        let p = path.trim();
        let b = p.as_bytes();
        // 必须带 `<盘符>:\` 前缀且盘符对得上,否则这卷里不可能有它。
        if b.len() < 4 || b[1] != b':' || (b[2] != b'\\' && b[2] != b'/') {
            return None;
        }
        if !b[0].eq_ignore_ascii_case(&(self.letter as u8)) {
            return None;
        }
        let rest = &p[3..];
        if rest.trim_matches(['\\', '/']).is_empty() {
            // 卷根本身不是条目(它在 CSR 里是虚拟桶,不占下标)。
            return None;
        }
        let mut bucket = self.children_of(self.count());
        let mut found = None;
        for comp in rest.split(['\\', '/']).filter(|s| !s.is_empty()) {
            let want: Vec<u8> = comp.bytes().map(|c| c.to_ascii_lowercase()).collect();
            let hit = *bucket
                .iter()
                .find(|&&c| self.fold_name(c as usize) == want.as_slice())?;
            found = Some(hit as usize);
            bucket = self.children_of(hit as usize);
        }
        found
    }

    /// 条目字节数(0 = 未知)。
    pub fn size_of(&self, i: usize) -> u64 {
        self.size[i]
    }

    /// 条目修改时间,UNIX 微秒(0 = 未知)。
    pub fn mtime_of(&self, i: usize) -> u64 {
        self.mtime[i]
    }

    /// 条目最近一次被 journal 事件写入的时刻,UNIX 微秒(0 = 非事件来源)。
    pub fn mtime_usn_of(&self, i: usize) -> u64 {
        self.mtime_usn[i]
    }

    /// 类别位标记(`CLASS_*` / `FLAG_INDEXABLE`)。
    pub fn class_of(&self, i: usize) -> u8 {
        self.cls[i]
    }

    /// 条目是否可入库派生(图片/视频)。
    pub fn is_indexable(&self, i: usize) -> bool {
        self.cls[i] & FLAG_INDEXABLE != 0
    }

    // ------- 匹配内部 -------

    /// 折叠名字节切片(仅内部排序/比较用)。
    fn fold_name(&self, i: usize) -> &[u8] {
        &self.fold[self.off[i] as usize..(self.off[i] + self.len[i]) as usize]
    }

    fn name_bytes(&self, i: usize) -> &[u8] {
        &self.pool[self.off[i] as usize..(self.off[i] + self.len[i]) as usize]
    }

    fn name_len(&self, i: usize) -> usize {
        self.len[i] as usize
    }

    /// 条目的折叠扩展名(不含点);没有扩展名 → 空切片。
    /// 与 `class_flags` 建库口径一致:点在尾部(`a.`)不算扩展名。
    fn fold_ext(&self, i: usize) -> &[u8] {
        let f = self.fold_name(i);
        match f.iter().rposition(|&b| b == b'.') {
            Some(p) if p + 1 < f.len() => &f[p + 1..],
            _ => &[],
        }
    }

    /// 在折叠池的**一段字节窗口**里找子串,只回 `entry_at` 落在 `lo..hi`
    /// 这个条目区间里的命中,并给出命中位在**条目内**的字节偏移(排名键,
    /// 0 = 前缀命中)。
    ///
    /// 窗口右端多取 `overlap` 字节:分块并行时子串可能正好跨块边界,
    /// 起点落在本块、尾巴伸进下一块 —— 少取这段就会漏命中。
    /// 反过来,重叠区里查到的命中若属于下一块的条目,会被 `lo..hi`
    /// 判定挡掉(那条命中由下一块自己负责),所以既不漏也不重,
    /// 不需要额外的去重集合。
    fn find_in(
        &self,
        lo: usize,
        hi: usize,
        overlap: usize,
        needle: &[u8],
        limit: usize,
    ) -> Vec<(u32, usize)> {
        let mut out = Vec::new();
        if needle.is_empty() || self.fold.is_empty() || lo >= hi || hi > self.off.len() {
            return out;
        }
        let b_lo = self.off[lo] as usize;
        let last = hi - 1;
        let b_hi = (self.off[last] as usize + self.len[last] as usize + overlap).min(self.fold.len());
        if b_lo >= b_hi {
            return out;
        }
        let finder = memchr::memmem::Finder::new(needle);
        let mut pos = b_lo;
        let mut last_idx = u32::MAX;
        while pos < b_hi {
            let Some(p) = finder.find(&self.fold[pos..b_hi]) else {
                break;
            };
            let abs = pos + p;
            if let Some(idx) = self.entry_at(abs) {
                let i = idx as usize;
                // 同一个名字里出现两次(如 "photo.jpg" 里两个 o)只算一条命中:
                // 池按条目顺序排布 → 命中位递增则条目下标单调不减,比 last 就够。
                if i >= lo && i < hi && idx != last_idx {
                    out.push((idx, abs - self.off[i] as usize));
                    last_idx = idx;
                    if out.len() >= limit {
                        break;
                    }
                }
            }
            pos = abs + 1;
        }
        out
    }

    /// 全卷子串查找:memmem 扫折叠池,命中位反查条目下标。
    /// `limit` 只做总量保险,正常排序在上层归并后进行。
    fn find_all(&self, needle: &[u8], limit: usize) -> Vec<u32> {
        self.find_in(
            0,
            self.off.len(),
            needle.len().saturating_sub(1),
            needle,
            limit,
        )
        .into_iter()
        .map(|(i, _)| i)
        .collect()
    }

    /// 字节位 → 条目下标(off 列二分;命中位必须落在某个名字区间内)。
    fn entry_at(&self, byte_pos: usize) -> Option<u32> {
        // off 单调递增;找最后一个 <= byte_pos 的 off。
        let mut lo = 0usize;
        let mut hi = self.off.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if (self.off[mid] as usize) <= byte_pos {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return None;
        }
        let i = lo - 1;
        let s = self.off[i] as usize;
        if byte_pos < s + self.len[i] as usize {
            Some(i as u32)
        } else {
            None
        }
    }
}

// ---------------- 持久化(紧凑列式 blob) ----------------

/// 文件头魔数(`RMNI` = Relyto.Maliang Name Index)与格式版本。
/// v2:每卷在 attrs 之后追加 size/mtime/mtime_usn(各 u64)与 cls(u8)四列。
/// 旧快照走 `load` 的版本不符分支,由上层重建。
const MAGIC: &[u8; 4] = b"RMNI";
pub const FORMAT_VERSION: u32 = 2;

/// 读一个已落盘 blob 的格式版本(不解析其余内容)。
///
/// 服务启动时用它把「版本不符 → 整库重建」与其它载入失败(文件缺失、损坏)
/// 区分开:前者要把目录账本全部挂失效位重派生,后者不用。
pub fn blob_version(path: &Path) -> Option<u32> {
    let raw = std::fs::read(path).ok()?;
    if raw.len() < 8 || &raw[0..4] != MAGIC {
        return None;
    }
    Some(u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]))
}

/// 落盘索引文件的唯一路径口径(服务、CLI、UI 三处都问这里,免得各拼各的)。
pub fn index_path() -> std::path::PathBuf {
    crate::db::data_dir().join("nameindex.bin")
}

/// 一个已落盘索引文件的内容:各卷 SoA 索引 + 每卷 USN checkpoint
/// (增量更新与"journal 是否还有效"的判定依据)。
#[derive(Debug, Default)]
pub struct IndexFile {
    pub vols: Vec<VolumeIndex>,
    pub journals: HashMap<char, crate::mft::JournalState>,
}

/// 原子写索引文件(先写 `.tmp` 再改名;数百万条目写一半崩溃不留半个索引)。
pub fn save(path: &Path, vols: &[VolumeIndex], journals: &HashMap<char, crate::mft::JournalState>) -> Result<()> {
    let tmp = path.with_extension("bin.tmp");
    let f = std::fs::File::create(&tmp)?;
    let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
    let mut scratch: Vec<u8> = Vec::with_capacity(1 << 20);

    w.write_all(MAGIC)?;
    scratch.clear();
    scratch.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    scratch.extend_from_slice(&(vols.len() as u32).to_le_bytes());
    w.write_all(&scratch)?;

    for v in vols {
        let n = v.off.len() as u32;
        scratch.clear();
        scratch.push(v.letter as u8);
        scratch.extend_from_slice(&n.to_le_bytes());
        scratch.extend_from_slice(&(v.pool.len() as u64).to_le_bytes());
        w.write_all(&scratch)?;
        w.write_all(&v.pool)?;
        write_col_u32(&mut w, &mut scratch, &v.off)?;
        write_col_u32(&mut w, &mut scratch, &v.len)?;
        write_col_u32(&mut w, &mut scratch, &v.parent)?;
        write_col_u64(&mut w, &mut scratch, &v.frn)?;
        write_col_u16(&mut w, &mut scratch, &v.attrs)?;
        write_col_u64(&mut w, &mut scratch, &v.size)?;
        write_col_u64(&mut w, &mut scratch, &v.mtime)?;
        write_col_u64(&mut w, &mut scratch, &v.mtime_usn)?;
        w.write_all(&v.cls)?;
        // 每卷 checkpoint:无 journal 记为 0。
        scratch.clear();
        match journals.get(&v.letter) {
            Some(j) => {
                scratch.push(1);
                scratch.extend_from_slice(&j.journal_id.to_le_bytes());
                scratch.extend_from_slice(&j.first_usn.to_le_bytes());
                scratch.extend_from_slice(&j.next_usn.to_le_bytes());
            }
            None => scratch.push(0),
        }
        w.write_all(&scratch)?;
    }
    w.flush()?;
    drop(w);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 读取索引文件(整文件进内存后解析;数百 MB 顺序读 NVMe 亚秒级)。
pub fn load(path: &Path) -> Result<IndexFile> {
    let raw = std::fs::read(path)?;
    let mut c = Cursor { b: &raw, p: 0 };
    if c.take(4)? != MAGIC {
        anyhow::bail!("索引文件魔数不符(可能损坏): {}", path.display());
    }
    let ver = c.u32()?;
    if ver != FORMAT_VERSION {
        anyhow::bail!("索引格式版本不符: 文件 v{ver},程序 v{FORMAT_VERSION}(重建即可)");
    }
    let nvol = c.u32()? as usize;
    let mut out = IndexFile::default();
    for _ in 0..nvol {
        let letter = c.u8()? as char;
        let n = c.u32()? as usize;
        let pool_len = c.u64()? as usize;
        let pool = c.take(pool_len)?.to_vec();
        let off = read_col_u32(&mut c, n)?;
        let len = read_col_u32(&mut c, n)?;
        let parent = read_col_u32(&mut c, n)?;
        let frn = read_col_u64(&mut c, n)?;
        let attrs = read_col_u16(&mut c, n)?;
        let size = read_col_u64(&mut c, n)?;
        let mtime = read_col_u64(&mut c, n)?;
        let mtime_usn = read_col_u64(&mut c, n)?;
        let cls = c.take(n)?.to_vec();
        let mut v = VolumeIndex {
            letter,
            pool,
            fold: Vec::new(),
            off,
            len,
            parent,
            frn,
            attrs,
            size,
            mtime,
            mtime_usn,
            cls,
            order: Vec::new(),
            csr_start: Vec::new(),
            csr_child: Vec::new(),
        };
        v.post_process();
        out.vols.push(v);
        if c.u8()? == 1 {
            let journal_id = c.u64()?;
            let first_usn = c.u64()? as i64;
            let next_usn = c.u64()? as i64;
            out.journals.insert(
                letter,
                crate::mft::JournalState {
                    journal_id,
                    first_usn,
                    next_usn,
                },
            );
        }
    }
    Ok(out)
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.p + n;
        if end > self.b.len() {
            anyhow::bail!("索引文件被截断(读到 {} / 共 {})", end, self.b.len());
        }
        let s = &self.b[self.p..end];
        self.p = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Result<u64> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(u64::from_le_bytes(a))
    }
}

fn read_col_u32(c: &mut Cursor, n: usize) -> Result<Vec<u32>> {
    let b = c.take(n * 4)?;
    Ok(b.as_chunks::<4>()
        .0
        .iter()
        .map(|s| u32::from_le_bytes(*s))
        .collect())
}

fn read_col_u64(c: &mut Cursor, n: usize) -> Result<Vec<u64>> {
    let b = c.take(n * 8)?;
    Ok(b.as_chunks::<8>()
        .0
        .iter()
        .map(|s| u64::from_le_bytes(*s))
        .collect())
}

fn read_col_u16(c: &mut Cursor, n: usize) -> Result<Vec<u16>> {
    let b = c.take(n * 2)?;
    Ok(b.as_chunks::<2>()
        .0
        .iter()
        .map(|s| u16::from_le_bytes(*s))
        .collect())
}

fn write_col_u32(w: &mut impl Write, scratch: &mut Vec<u8>, col: &[u32]) -> Result<()> {
    scratch.clear();
    for x in col {
        scratch.extend_from_slice(&x.to_le_bytes());
        if scratch.len() >= (1 << 20) - 16 {
            w.write_all(scratch)?;
            scratch.clear();
        }
    }
    w.write_all(scratch)?;
    Ok(())
}

fn write_col_u64(w: &mut impl Write, scratch: &mut Vec<u8>, col: &[u64]) -> Result<()> {
    scratch.clear();
    for x in col {
        scratch.extend_from_slice(&x.to_le_bytes());
        if scratch.len() >= (1 << 20) - 16 {
            w.write_all(scratch)?;
            scratch.clear();
        }
    }
    w.write_all(scratch)?;
    Ok(())
}

fn write_col_u16(w: &mut impl Write, scratch: &mut Vec<u8>, col: &[u16]) -> Result<()> {
    scratch.clear();
    for x in col {
        scratch.extend_from_slice(&x.to_le_bytes());
        if scratch.len() >= (1 << 20) - 16 {
            w.write_all(scratch)?;
            scratch.clear();
        }
    }
    w.write_all(scratch)?;
    Ok(())
}

// ---------------- CLI:提权索引器 ----------------

/// `maliang --mft-scan [C,D]`:全量枚举 NTFS 卷并落盘名字索引。
///
/// 这是**手动/诊断**入口(需要提权才有 $MFT 读权限);常驻路径是 `--service`
/// 里的索引服务。原先的 `--mft-report <文件>` 报告协议整体废除:
/// 报告是给"提权子进程 → 非提权 GUI"单向回报用的,现在 GUI 直接连管道问
/// `Status`,不需要靠轮询一个文本文件来猜进度。统计仍打到 stdout
/// (命令行子命令由 `win::attach_parent_console` 挂回调用方终端)。
pub fn cli_mft_scan(spec: Option<&str>) -> Result<()> {
    cli_mft_scan_run(spec)
}

/// 把一条 USN 记录折成增量操作:只认 CLOSE 终态。
/// - 创建 / 改名新名 → `Upsert`(以记录里的新名字 + 新父目录为准);
/// - 删除 / 改名旧名 → `Delete`。
///
/// 其余事件(OPEN、DATA_OVERWRITE、属性/安全描述符改动…)不进索引:
/// 名字与父子关系都没变。size/mtime 同理 —— USN 记录里没有这两项,
/// 传 0 表示「拿不到」,`apply_delta` 会保留建库时从 `$MFT` 读到的真值。
fn collect_close_op(e: &crate::mft::UsnEntry<'_>, ops: &mut Vec<DeltaOp>) {
    if e.reason & crate::mft::USN_REASON_CLOSE == 0 {
        return;
    }
    let renamed = e.reason & crate::mft::USN_REASON_RENAME_NEW_NAME != 0
        || e.reason & crate::mft::USN_REASON_FILE_CREATE != 0;
    let gone = !renamed
        && (e.reason & crate::mft::USN_REASON_FILE_DELETE != 0
            || e.reason & crate::mft::USN_REASON_RENAME_OLD_NAME != 0);
    if renamed && !e.name.is_empty() {
        ops.push(DeltaOp::Upsert(DeltaUpsert {
            frn: e.frn,
            parent_frn: e.parent_frn,
            attrs: e.attrs,
            name: String::from_utf16_lossy(e.name),
            size: 0,
            mtime: 0,
            mtime_usn: filetime_micros(e.timestamp),
        }));
    } else if gone {
        ops.push(DeltaOp::Delete(e.frn));
    }
}

/// 一轮日志读取的结果(**不含**索引变更:读与并分开,服务才能在
/// 不持锁的情况下阻塞等日志)。
#[derive(Debug)]
pub enum JournalStep {
    /// 读到一批 CLOSE 终态记录,`next_usn` 是新的 checkpoint。
    /// `raw` 是本批原始记录摘要(reason, 名字)最多 12 条,诊断用。
    Delta {
        ops: Vec<DeltaOp>,
        next_usn: i64,
        records: u64,
        raw: Vec<(u32, String)>,
    },
    /// journal 已失效(条目被回收 / 日志被重建)→ 必须全卷重建。
    RebuildNeeded,
    /// 卷或 journal 当前问不出来(权限不足、日志被关、盘拔出)。
    /// 调用方按文案报因并退避重试,**不要**当成「本轮无变化」。
    Unavailable(String),
}

/// 阻塞读一轮 USN 日志,只收集 CLOSE 终态,不动索引。
///
/// `read_journal` 带 `ReturnOnlyOnClose`:日志里没有新记录时这次 IOCTL
/// **在内核里挂住**,所以它本身就是常驻线程的主体 —— 醒来即有活干,
/// 不轮询、不烧 CPU(现码把 Timeout/BytesToWaitFor 都留 0,是轮询语义)。
///
/// 服务侧的用法:`poll_journal`(可能阻塞几十分钟)→ 拿锁 → `apply_delta`
/// → 放锁 → L2 落库。读的那段绝不能持锁,否则 UI 的快查会被冻住。
pub fn poll_journal(letter: char, journal_id: u64, start_usn: i64) -> JournalStep {
    let mut ops: Vec<DeltaOp> = Vec::new();
    let mut raw: Vec<(u32, String)> = Vec::new();
    match crate::mft::read_journal(letter, journal_id, start_usn, |e| {
        if raw.len() < 12 {
            raw.push((e.reason, String::from_utf16_lossy(e.name)));
        }
        collect_close_op(e, &mut ops);
        true
    }) {
        Ok(st) => JournalStep::Delta {
            ops,
            next_usn: st.next_usn,
            records: st.records,
            raw,
        },
        Err(e) => {
            if e.downcast_ref::<crate::mft::JournalInvalidated>().is_some() {
                JournalStep::RebuildNeeded
            } else {
                JournalStep::Unavailable(format!("{e:#}"))
            }
        }
    }
}

/// 单卷保鲜一步的结果(服务侧每卷一个常驻线程反复调用)。
#[derive(Debug)]
pub enum Freshness {
    /// 正常推进:本批增量已并入索引,`next_usn` 是该卷新的 checkpoint。
    Advanced {
        counts: DeltaCounts,
        next_usn: i64,
        records: u64,
    },
    /// journal 已失效(条目被回收 / 日志被重建),checkpoint 不可续读
    /// → 调用方必须全卷重建。对应 ERROR_JOURNAL_ENTRY_DELETED(1181)。
    RebuildNeeded,
    /// 卷或 journal 当前问不出来(权限不足、日志被关、盘拔出)。
    /// 调用方按文案报因并退避重试,**不要**当成「本轮无变化」。
    Unavailable(String),
}

/// 读一轮日志并就地并入索引(CLI 用;服务走 [`poll_journal`] + `apply_delta`
/// 的两段式,好在阻塞期间不持锁)。
pub fn refresh_volume(v: &mut VolumeIndex, journal_id: u64, start_usn: i64) -> Freshness {
    match poll_journal(v.letter, journal_id, start_usn) {
        JournalStep::Delta {
            ops,
            next_usn,
            records,
            ..
        } => Freshness::Advanced {
            counts: v.apply_delta(ops),
            next_usn,
            records,
        },
        JournalStep::RebuildNeeded => Freshness::RebuildNeeded,
        JournalStep::Unavailable(e) => Freshness::Unavailable(e),
    }
}

/// checkpoint 是否还能续读(CLI 增量分支与服务 bootstrap 共用同一判定)。
///
/// 三个条件缺一不可:日志还是同一个实例(id 相同 = 没被重建)、
/// checkpoint 没落到已回收区(`>= first_usn`)、也没跑到未来(`<= next_usn`)。
/// 任一不满足就只能全量重建。
pub fn can_resume(
    saved: Option<crate::mft::JournalState>,
    cur: &crate::mft::JournalState,
) -> bool {
    saved
        .map(|s| {
            s.journal_id == cur.journal_id
                && s.next_usn >= cur.first_usn
                && s.next_usn <= cur.next_usn
        })
        .unwrap_or(false)
}

/// 每卷的 journal 游标(服务持久化 next_usn;UI 启动时拉这份表,
/// 把「上次落库之后」的 delta 补进 L2 —— App 关着也保持元数据不旧)。
pub fn journal_cursors(journals: &HashMap<char, crate::mft::JournalState>) -> Vec<(char, u64)> {
    let mut out: Vec<(char, u64)> = journals
        .iter()
        .map(|(l, j)| (*l, j.next_usn as u64))
        .collect();
    out.sort_by_key(|(l, _)| *l);
    out
}

/// 把条目 `i` 折成一条 L2 结构行(**不打开文件**,只读 L1 的列)。
fn l1_row(v: &VolumeIndex, i: usize) -> crate::db::L1Row {
    let path = v.path_of(i);
    let parent = Path::new(&path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = v.name(i);
    let ext = match name.rfind('.') {
        Some(p) if p + 1 < name.len() => Some(name[p + 1..].to_ascii_lowercase()),
        _ => None,
    };
    let cls = v.class_of(i);
    crate::db::L1Row {
        frn64: crate::db::frn64_of(v.letter, v.frn_of(i)),
        path,
        parent,
        // L1 存 UNIX 微秒,photos.mtime 是秒(与既有扫描路径同口径)。
        mtime: (v.mtime_of(i) / 1_000_000) as i64,
        size: v.size_of(i) as i64,
        kind: if cls & CLASS_VIDEO != 0 {
            crate::video::kind::MOVIE
        } else {
            crate::video::kind::PHOTO
        },
        ext,
    }
}

/// 全卷入库:索引里所有可入库条目(图片/视频)→ L2 结构行。
/// 这是「全盘元数据入库」的主路,派生与否由 L3 的 rich_state 门控另说。
pub fn l1_rows_full(v: &VolumeIndex) -> Vec<crate::db::L1Row> {
    (0..v.count())
        .filter(|&i| v.is_indexable(i))
        .map(|i| l1_row(v, i))
        .collect()
}

/// 增量入库(按 FRN):delta 只给 FRN,而 `apply_delta` 压实后下标会变,
/// 所以这里按 FRN 集合做一次线性筛 —— 一遍 O(n),比维护「FRN→下标」的
/// 反向映射便宜(那张表本身就要 O(n) 建,还得跟着压实改)。
/// 不在索引里的 FRN(已删/非可入库)自然落空。
pub fn l1_rows_for_frns(v: &VolumeIndex, frns: &[u64]) -> Vec<crate::db::L1Row> {
    if frns.is_empty() {
        return Vec::new();
    }
    let want: std::collections::HashSet<u64> =
        frns.iter().map(|f| f & FRN_INDEX_MASK).collect();
    (0..v.count())
        .filter(|&i| v.is_indexable(i) && want.contains(&(v.frn_of(i) & FRN_INDEX_MASK)))
        .map(|i| l1_row(v, i))
        .collect()
}

/// 把一批增量操作拆成「要落库的 FRN」与「要删行的 FRN」。
/// 顺序无关:落库走 upsert,删行走 DELETE,两边都以 FRN 为身份。
pub fn split_delta_frns(ops: &[DeltaOp]) -> (Vec<u64>, Vec<u64>) {
    let mut up = Vec::new();
    let mut del = Vec::new();
    for op in ops {
        match op {
            DeltaOp::Upsert(u) => up.push(u.frn),
            DeltaOp::Delete(f) => del.push(*f),
        }
    }
    (up, del)
}

/// 分批写库的批大小:单事务太大时 WAL 会长时间占住写锁,UI 侧的读会排队。
const L1_INGEST_CHUNK: usize = 20_000;

/// 把结构行分批落库,返回累计计数。
///
/// 派生门在这里加载**一次**:全量落库动辄百万行,逐行读库判定
/// 会把落库拖成 IO 密集;读一次账本、之后纯内存判定才符合「判定零 IO」的口径。
///
/// **硬排除目录的行连结构都不落库**(自家产出目录、回收站、系统与程序目录、
/// temp、点开头目录):那是程序自己写的文件或用户已经判了死刑的文件,收进库
/// 既不会出图(作用域门挡着),又会因为 mtime 最新霸占「最近更新」首屏,
/// 还每轮给自己制造 USN 事件(首屏 200 张里 190 张是自家 thumbs)。
///
/// 被摘掉的行还要按 FRN **反删** L2 里的旧行:照片被删进回收站时,L1 给的是一条
/// 改名 Upsert(路径变成 `$RECYCLE.BIN\...`),只「不落库」会让 L2 留着一条指向
/// 已不存在路径的陈旧行。walkdir 那条发现路不必重复这一刀 —— 遍历时已经按
/// 排除规则跳过这些目录了。
pub fn ingest_l1(
    store: &crate::db::Store,
    rows: &[crate::db::L1Row],
) -> Result<crate::db::L1Ingest> {
    if rows.is_empty() {
        return Ok(crate::db::L1Ingest::default());
    }
    let gate = crate::db::ScopeGate::load(store);
    let mut kept: Vec<&crate::db::L1Row> = Vec::with_capacity(rows.len());
    let mut dropped: Vec<i64> = Vec::new();
    for r in rows {
        if gate.never_index(&r.parent) {
            dropped.push(r.frn64);
        } else {
            kept.push(r);
        }
    }
    let mut out = crate::db::L1Ingest::default();
    for c in kept.chunks(L1_INGEST_CHUNK) {
        let batch: Vec<crate::db::L1Row> = c.iter().map(|r| (*r).clone()).collect();
        let r = store.ingest_l1_rows(&batch, Some(&gate))?;
        out.inserted += r.inserted;
        out.updated += r.updated;
    }
    for c in dropped.chunks(L1_INGEST_CHUNK) {
        store.delete_l1_frn64(c)?;
    }
    Ok(out)
}

/// 把一轮增量的结果落到 L2。`up_frns`/`del_frns` 必须在 `apply_delta` **之前**
/// 从 `ops` 里拆出来:delta 一旦并进去,压实会让旧下标失效,而这里要的是
/// 「按 FRN 找新下标」,只有拆出来的 FRN 在压实后仍然有效。
///
/// 失败不回传错误:L1 已经更新成功了,L2 的 checkpoint 就不推进,
/// 下一轮增量会重放同一段日志把它补上(重放幂等,只处理 CLOSE 记录)。
fn l2_ingest_delta(
    store: &crate::db::Store,
    v: &VolumeIndex,
    letter: char,
    up_frns: &[u64],
    del_frns: &[u64],
    next_usn: i64,
    say: &mut impl FnMut(String),
) {
    let rows = l1_rows_for_frns(v, up_frns);
    let ing = match ingest_l1(store, &rows) {
        Ok(i) => i,
        Err(e) => {
            say(format!(
                "[{letter}:] L2 落库失败({e:#}),L1 已更新,checkpoint 不推进,下一轮补"
            ));
            return;
        }
    };
    let dels: Vec<i64> = del_frns
        .iter()
        .map(|f| crate::db::frn64_of(letter, *f))
        .collect();
    let ndel = match store.delete_l1_frn64(&dels) {
        Ok(n) => n,
        Err(e) => {
            say(format!(
                "[{letter}:] L2 新增 {}/更新 {} 已落,删除 {} 条失败({e:#}),下一轮补",
                ing.inserted,
                ing.updated,
                dels.len()
            ));
            return;
        }
    };
    if let Err(e) = store.set_l2_cursor(letter, next_usn) {
        say(format!(
            "[{letter}:] L2 已落(新增 {},更新 {},删除 {ndel}),但 checkpoint 写入失败({e:#}),下一轮会重放",
            ing.inserted, ing.updated
        ));
        return;
    }
    say(format!(
        "[{letter}:] L2 落库:新增 {},更新 {},删除 {ndel},checkpoint → {next_usn:#x}",
        ing.inserted, ing.updated
    ));
}

/// 全量建一卷索引的结果(CLI 与服务共用同一条建库路径)。
pub struct VolumeBuild {
    pub idx: VolumeIndex,
    /// 建库完成时刻的 journal 状态,直接当 checkpoint 用。
    pub journal: crate::mft::JournalState,
    /// 数据来源文案(报给用户:"$MFT 记录 N 条(…)" / "USN 记录 N 条(…)")。
    pub source: String,
    /// 走了降级路径的原因(直读 `$MFT` 失败 → USN 枚举,size/mtime 留空)。
    pub degraded: Option<String>,
    pub entries: u32,
    pub pool_bytes: usize,
}

/// 全量建一卷:主路径直读 `$MFT`,失败退回 USN 全量枚举。
///
/// 为什么 MFT 是主路径:USN 记录里**没有** size/mtime,而这两列是 L1 的免费
/// 元数据来源,只有 `$MFT` 给得起。退回枚举时名字齐全、元数据留 0,
/// 降级事实通过 [`VolumeBuild::degraded`] 如实上报,不静默。
///
/// `progress` 每秒至多回调一次,参数是当前已收条目数(不给百分比 ——
/// 枚举前拿不到总量,假装精度不如不给)。
pub fn build_volume(
    p: &crate::mft::VolumeProbe,
    ctrl: Option<&crate::scan::ScanControl>,
    mut progress: impl FnMut(usize),
) -> Result<VolumeBuild> {
    let j = p
        .journal
        .ok_or_else(|| anyhow::anyhow!("[{}:] 没有 journal 状态,无法全量建库", p.letter))?;
    let mut b = VolumeIndexBuilder::new(p.letter);
    let last = std::cell::Cell::new(std::time::Instant::now());
    let mft_res = crate::rec::scan_mft(p.letter, ctrl, |r| {
        b.push_mft(r);
        if last.get().elapsed() >= std::time::Duration::from_secs(1) {
            last.set(std::time::Instant::now());
            progress(b.count() as usize);
        }
        true
    });
    let (source, degraded) = match mft_res {
        Ok(st) => (
            format!(
                "$MFT 记录 {} 条(文件 {},目录 {},扩展 {})",
                st.total, st.files, st.dirs, st.extended
            ),
            None,
        ),
        Err(e) => {
            b = VolumeIndexBuilder::new(p.letter);
            last.set(std::time::Instant::now());
            let st = crate::mft::scan_volume(p.letter, &j, ctrl, |entry| {
                b.push_usn(entry);
                if last.get().elapsed() >= std::time::Duration::from_secs(1) {
                    last.set(std::time::Instant::now());
                    progress(b.count() as usize);
                }
                true
            })?;
            (
                format!("USN 记录 {} 条(文件 {},目录 {})", st.records, st.files, st.dirs),
                Some(format!(
                    "直读 $MFT 失败({e:#}),退回全量枚举 USN(size/mtime 将留空)"
                )),
            )
        }
    };
    let entries = b.count();
    let pool_bytes = b.pool.len();
    let idx = b.finish();
    Ok(VolumeBuild {
        idx,
        journal: j,
        source,
        degraded,
        entries,
        pool_bytes,
    })
}

fn cli_mft_scan_run(spec: Option<&str>) -> Result<()> {
    let mut say = |line: String| {
        println!("{line}");
    };
    let want: Option<Vec<char>> = spec.map(|s| {
        s.split(',')
            .filter_map(|t| t.trim().chars().next())
            .map(|c| c.to_ascii_uppercase())
            .collect()
    });
    let t0 = std::time::Instant::now();
    let probes = crate::mft::probe();
    if probes.is_empty() {
        anyhow::bail!("未发现任何可扫卷");
    }
    // 进度比例按「已完成卷数 / 本轮卷数」:USN 枚举不给单卷总数,
    // 卷内的实时条数进文案,不假装成百分比。
    let total = probes.len();
    say(format!("PROGRESS 0.00 发现 {total} 个可扫描卷…"));
    let out = crate::db::data_dir().join("nameindex.bin");
    // L2 落库口:全盘元数据入库。开不了库(权限/被别的进程占住)
    // 就只更新 L1、索引文件照写,L2 下一轮补 —— 库暂时不可用不该让整轮扫失败。
    let store = match crate::db::Store::open() {
        Ok(s) => Some(s),
        Err(e) => {
            say(format!("L2 落库跳过:打不开 index.db({e:#})"));
            None
        }
    };
    // 已有索引 → 逐卷尝试增量(checkpoint 可续读才走增量,否则该卷全量);
    // 载入失败整体转全量。本轮 probe 之外的卷(如临时拔出的盘)保留旧数据。
    let (mut vols, mut journals): (Vec<VolumeIndex>, HashMap<char, crate::mft::JournalState>) =
        if out.exists() {
            match load(&out) {
                Ok(f) => {
                    let n: usize = f.vols.iter().map(|v| v.count()).sum();
                    say(format!("已有索引:{} 卷 {} 条,尝试增量…", f.vols.len(), n));
                    (f.vols, f.journals)
                }
                Err(e) => {
                    say(format!("已有索引载入失败,按全量重建: {e:#}"));
                    (Vec::new(), HashMap::new())
                }
            }
        } else {
            (Vec::new(), HashMap::new())
        };
    // 本轮是否真的动过某个卷。全被跳过时 vols 仍带着旧索引(从盘上载入的),
    // 光靠 vols.is_empty() 抓不住 —— 那种空跑会照样重写 nameindex.bin 并报「成功」,
    // GUI 就把「什么都没更新」当成索引就绪了。
    let mut touched = 0usize;
    // 索引内容是否真变了(全量重建 / 增量有条目落地)。落盘门控用。
    let mut changed = false;
    let mut skipped: Vec<(char, crate::mft::Support, String)> = Vec::new();
    for (vi, p) in probes.into_iter().enumerate() {
        if let Some(w) = &want {
            if !w.contains(&p.letter) {
                continue;
            }
        }
        match p.support {
            crate::mft::Support::Usn => {}
            other => {
                // 报故障用 fault_text(带 Win32 码),不用 {:?}:NoJournal 这一个变体
                // 底下混着「日志没开」「日志正在删」「卷问不出来码不认得」。
                let why = p.fault_text();
                say(format!("[{0}:] 跳过:{1}", p.letter, why));
                skipped.push((p.letter, other, why));
                continue;
            }
        }
        let j = p.journal.expect("Support::Usn 必带 journal");
        let pos = vols.iter().position(|v| v.letter == p.letter);
        let saved = journals.get(&p.letter).copied();
        let can_incr = pos.is_some() && can_resume(saved, &j);
        let frac = vi as f32 / total as f32;
        let t1 = std::time::Instant::now();
        let mut incr_ok = false;
        if can_incr {
            let s = saved.unwrap();
            let next_usn = s.next_usn;
            say(format!(
                "PROGRESS {frac:.2} [{letter}:] 增量更新 USN(自 next_usn={next_usn:#x})…",
                letter = p.letter
            ));
            let mut ops: Vec<DeltaOp> = Vec::new();
            let r = crate::mft::read_journal(
                p.letter,
                j.journal_id,
                s.next_usn,
                |e: &crate::mft::UsnEntry<'_>| {
                    collect_close_op(e, &mut ops);
                    true
                },
            );
            match r {
                Ok(st) => {
                    // 先按 FRN 拆出「要落库 / 要删行」两批,再并 L1 ——
                    // apply_delta 会压实,下标随后就不可用了。
                    let (up_frns, del_frns) = split_delta_frns(&ops);
                    let vi = pos.unwrap();
                    let c = vols[vi].apply_delta(ops);
                    journals.insert(
                        p.letter,
                        crate::mft::JournalState {
                            journal_id: j.journal_id,
                            first_usn: j.first_usn,
                            next_usn: st.next_usn,
                        },
                    );
                    say(format!(
                        "[{0}:] 增量完成:CLOSE 记录 {1} 条 → 新增 {2},更新 {3},删除 {4},checkpoint → {5:#x},耗时 {6:.1}s",
                        p.letter, st.records, c.added, c.updated, c.removed, st.next_usn, t1.elapsed().as_secs_f64()
                    ));
                    if let Some(s) = &store {
                        l2_ingest_delta(s, &vols[vi], p.letter, &up_frns, &del_frns, st.next_usn, &mut say);
                    }
                    incr_ok = true;
                    touched += 1;
                    if c.added + c.updated + c.removed > 0 {
                        changed = true;
                    }
                }
                Err(e) => {
                    say(format!(
                        "[{0}:] 增量读取失败({1:#}),转全量重建",
                        p.letter, e
                    ));
                }
            }
        }
        if incr_ok {
            continue;
        }
        let ctrl = crate::scan::ScanControl::new();
        let letter = p.letter;
        say(format!(
            "PROGRESS {frac:.2} [{letter}:] 全量读取 $MFT(journal_id={jid:#x}, next_usn={next:#x})…",
            jid = j.journal_id,
            next = j.next_usn
        ));
        let built = build_volume(&p, Some(&ctrl), |n| {
            // 逐秒条数写进报告:原来只有黑控制台里的 \r 计数能看到,现在进界面。
            say(format!("PROGRESS {frac:.2} [{letter}:] 全量读取中…已 {n} 条"));
        })?;
        if let Some(d) = &built.degraded {
            say(format!("[{letter}:] {d}"));
        }
        let src = built.source.clone();
        let pool_mb = built.pool_bytes as f64 / 1048576.0;
        let count = built.entries;
        let idx = built.idx;
        // built.journal 随后被 move 进 journals,checkpoint 先抄出来给 L2 用。
        let full_next_usn = built.journal.next_usn;
        say(format!(
            "[{0}:] 完成:{1} → 索引 {2} 条,名字池 {3:.1} MB,耗时 {4:.1}s",
            p.letter,
            src,
            count,
            pool_mb,
            t1.elapsed().as_secs_f64()
        ));
        journals.insert(p.letter, built.journal);
        let vi = match pos {
            Some(i) => {
                vols[i] = idx;
                i
            }
            None => {
                vols.push(idx);
                vols.len() - 1
            }
        };
        if let Some(s) = &store {
            // 全量路径一次可能上百万行,分批走 L1_INGEST_CHUNK。
            say(format!(
                "PROGRESS {frac:.2} [{letter}:] 全量落 L2(结构入库,不打开文件)…"
            ));
            let rows = l1_rows_full(&vols[vi]);
            match ingest_l1(s, &rows) {
                Ok(ing) => {
                    let _ = s.set_l2_cursor(p.letter, full_next_usn);
                    say(format!(
                        "[{0}:] L2 全量落库:新增 {1},更新 {2}(共 {3} 行),checkpoint → {4:#x}",
                        p.letter,
                        ing.inserted,
                        ing.updated,
                        rows.len(),
                        full_next_usn
                    ));
                }
                Err(e) => say(format!(
                    "[{0}:] L2 全量落库失败({1:#}),L1 已写入索引文件,下一轮补",
                    p.letter, e
                )),
            }
        }
        touched += 1;
        changed = true;
    }
    if touched == 0 {
        // 一个卷都没更新:不落盘、不报「成功」。全量/增量都算 touched,
        // 所以走到这里只可能是「被跳过 / 被 spec 排除」。
        let why = if skipped
            .iter()
            .any(|(_, s, _)| *s == crate::mft::Support::NeedsAdmin)
        {
            "未获得管理员权限".to_string()
        } else if skipped.is_empty() {
            "指定卷全部被过滤".to_string()
        } else {
            skipped
                .iter()
                .map(|(l, _, t)| format!("{l}: {t}"))
                .collect::<Vec<_>>()
                .join(";")
        };
        anyhow::bail!("本轮未更新任何卷索引(原因:{why}),保留原索引文件");
    }
    say("PROGRESS 0.99 枚举完成,正在写索引文件…".to_string());
    if !changed {
        // 脏标记门控:增量跑完一条都没落地(机器闲置)时不重写索引文件。
        // 保鲜节奏是 15 分钟一次,56.5 MB 全量重写 = 5.4 GB/天的无谓写入。
        // checkpoint 不跟着落盘是有意的:下次从旧 next_usn 重放同一段日志是
        // 幂等的,而且只重放 CLOSE 记录,比重写整个索引便宜得多。
        say(format!(
            "成功 本轮无实质变化,保留原索引文件({} 卷),总耗时 {:.1}s",
            vols.len(),
            t0.elapsed().as_secs_f64()
        ));
        return Ok(());
    }
    save(&out, &vols, &journals)?;
    say(format!(
        "成功 索引已写入 {}({} 卷,文件 {:.1} MB),总耗时 {:.1}s",
        out.display(),
        vols.len(),
        std::fs::metadata(&out)?.len() as f64 / 1048576.0,
        t0.elapsed().as_secs_f64()
    ));
    Ok(())
}

/// `maliang --mft-check`:载入落盘索引,打印统计 + 几组样例查询。
/// 诊断用:验证持久化往返与真盘数据的搜索质量,不写不删。
pub fn cli_mft_check() -> Result<()> {
    let path = crate::db::data_dir().join("nameindex.bin");
    let t0 = std::time::Instant::now();
    let idx = load(&path)?;
    println!(
        "载入 {}({} 卷)耗时 {:.2}s",
        path.display(),
        idx.vols.len(),
        t0.elapsed().as_secs_f64()
    );
    for v in &idx.vols {
        println!(
            "[{0}:] {1} 条,名字池 {2:.1} MB,checkpoint next_usn={3:#x}",
            v.letter,
            v.count(),
            v.pool.len() as f64 / 1048576.0,
            idx.journals.get(&v.letter).map(|j| j.next_usn).unwrap_or(0)
        );
    }
    // 前 5 条是单串子串查询;后 6 条专门把的 term 语法走一遍 ——
    // 空格 AND、`|` OR、`ext:` / `size:` / `date:` 谓词。最后一条 `size:abc`
    // 是**故意写错**的:它必须无命中,而不是退化成「全命中」把整盘倒出来。
    // 注意 `size:` / `date:` 读的是索引里的 size/mtime 两列,USN 降级建出来的
    // 卷这两列恒为 0(含义是"未知"),谓词会跳过它们 → 那种卷上查不到是正常现象。
    for q in [
        "jpg",
        "照片",
        "IMG_2",
        ".dng",
        "Screenshot",
        "IMG_2 jpg",
        "jpg|dng",
        "ext:jpg|dng",
        "size:>10mb",
        "date:>=2024",
        "size:abc",
    ] {
        let t1 = std::time::Instant::now();
        let hits = search(&idx.vols, q, 5);
        println!(
            "\n查询 \"{}\"({:.1} ms,展示前 5 条):",
            q,
            t1.elapsed().as_secs_f64() * 1000.0
        );
        for h in &hits {
            println!("  {} {}", if h.is_dir { "<目录>" } else { "      " }, h.path);
        }
        if hits.is_empty() {
            println!("  (无命中)");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 带 size/mtime 的追加入口(`size:` / `date:` 谓词测试用)。
    /// `push` 走 USN 形态,这两列恒为 0,喂不出谓词命中。
    #[allow(clippy::too_many_arguments)]
    fn push_meta(
        b: &mut VolumeIndexBuilder,
        frn: u64,
        parent: u64,
        attrs: u32,
        name: &str,
        size: u64,
        mtime_us: u64,
    ) {
        let units: Vec<u16> = name.encode_utf16().collect();
        b.push_inner(frn, parent, attrs, &units, size, mtime_us, 0);
    }

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

    /// 测试用的简化 Upsert:元数据列全 0(与 journal 通道的语义一致)。
    fn up(frn: u64, parent: u64, attrs: u32, name: &str) -> DeltaUpsert {
        DeltaUpsert {
            frn,
            parent_frn: parent,
            attrs,
            name: name.into(),
            size: 0,
            mtime: 0,
            mtime_usn: 0,
        }
    }

    /// FILETIME 纪元(1601-01-01)到 UNIX 纪元的 100ns 刻度差。
    /// 测试里用它把"UNIX 微秒"换算成 `$MFT` 里的原始 FILETIME。
    const FT_EPOCH: u64 = 116_444_736_000_000_000;

    /// 造一条 `$MFT` 解析结果(只填索引用得上的字段)。
    fn mft_rec(
        index: u64,
        parent: u64,
        name: &str,
        attrs: u32,
        size: u64,
        write_time: u64,
    ) -> crate::rec::MftRec {
        crate::rec::MftRec {
            index,
            seq: 1,
            parent_frn: parent,
            name: Some(name.into()),
            file_attrs: attrs,
            data_size: Some(size),
            si: Some([0, write_time, 0, 0]),
            ..Default::default()
        }
    }

    /// 造一个小卷:C:\Pics\IMG_0001.JPG、C:\Pics\RAW\DSC.NEF、孤儿条目。
    fn tiny_volume() -> VolumeIndex {
        let mut b = VolumeIndexBuilder::new('C');
        // 目录 Pics(frn 0x1000,父 = 卷根 5)
        push(&mut b, 0x1000, 5, ATTR_DIRECTORY, "Pics");
        // 文件 IMG_0001.JPG(frn 0x1001,父 Pics)
        push(&mut b, 0x1001, 0x1000, 0x20, "IMG_0001.JPG");
        // 子目录 RAW(frn 0x2000,父 Pics)
        push(&mut b, 0x2000, 0x1000, ATTR_DIRECTORY, "RAW");
        // DSC.NEF(frn 0x2001,父 RAW)
        push(&mut b, 0x2001, 0x2000, 0x20, "DSC.NEF");
        // 孤儿(父 0xdead 不在流中)
        push(&mut b, 0x3000, 0xdead, 0x20, "orphan.log");
        b.finish()
    }

    #[test]
    fn builds_and_counts() {
        let v = tiny_volume();
        assert_eq!(v.count(), 5);
        assert!(v.is_dir(0), "Pics 应是目录");
        assert!(!v.is_dir(1), "IMG_0001.JPG 应是文件");
    }

    #[test]
    fn path_reconstruction_walks_parents() {
        let v = tiny_volume();
        assert_eq!(v.path_of(0), r"C:\Pics");
        assert_eq!(v.path_of(1), r"C:\Pics\IMG_0001.JPG");
        assert_eq!(v.path_of(3), r"C:\Pics\RAW\DSC.NEF");
    }

    #[test]
    fn orphan_path_stops_at_break() {
        let v = tiny_volume();
        assert_eq!(v.path_of(4), r"C:\orphan.log");
    }

    #[test]
    fn substring_search_case_insensitive() {
        let v = tiny_volume();
        let hits = search(std::slice::from_ref(&v), "img", 100);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, r"C:\Pics\IMG_0001.JPG");
        let hits = search(std::slice::from_ref(&v), "PIC", 100);
        assert_eq!(hits.len(), 1, "Pics 大小写不敏感");
        assert!(hits[0].is_dir);
        let hits = search(std::slice::from_ref(&v), ".nef", 100);
        assert_eq!(hits[0].path, r"C:\Pics\RAW\DSC.NEF");
    }

    /// 同一名字里子串出现多次只算一条命中。
    /// 回归钉:find_all 曾按「命中位」出条,"banana.txt" 里两个 "na" 各报一次,
    /// 上层归并后同一文件在快查结果里重复出现。
    #[test]
    fn repeated_match_positions_collapse_to_one_hit() {
        let mut b = VolumeIndexBuilder::new('C');
        push(&mut b, 0x6000, 5, 0x20, "banana.txt");
        push(&mut b, 0x6001, 5, 0x20, "notes.txt");
        let v = b.finish();
        // "na" 在条目 0 里出现两次 → 只出一条;条目 1 不含 "na"。
        assert_eq!(v.find_all(b"na", 100), vec![0]);
        assert_eq!(search(std::slice::from_ref(&v), "na", 100).len(), 1);
        // 不同条目各自命中不互相吞掉:"n" 在两条里都有。
        assert_eq!(v.find_all(b"n", 100), vec![0, 1]);
        // 同名不同目录是两条独立记录,不是重复。
        let mut b2 = VolumeIndexBuilder::new('D');
        push(&mut b2, 0x7000, 5, ATTR_DIRECTORY, "aa");
        push(&mut b2, 0x7001, 0x7000, 0x20, "x.txt");
        push(&mut b2, 0x7002, 5, 0x20, "x.txt");
        let v2 = b2.finish();
        assert_eq!(v2.find_all(b"x.txt", 100), vec![1, 2]);
    }

    #[test]
    fn cjk_names_searchable() {
        let mut b = VolumeIndexBuilder::new('D');
        push(&mut b, 0x4000, 5, 0x20, "旅行照片备份");
        let v = b.finish();
        let hits = search(std::slice::from_ref(&v), "旅行", 100);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, r"D:\旅行照片备份");
        // 命中不跨字符:单查一个不存在的串应无结果而非误报。
        let hits = search(std::slice::from_ref(&v), " nonexistent ", 100);
        assert!(hits.is_empty());
    }

    // ===================== term 模型 =====================

    /// 一个词的候选类型摘要(断言用,省得逐字段比整棵 AST)。
    fn alt_kinds(t: &Term) -> Vec<&'static str> {
        t.alts
            .iter()
            .map(|a| match a {
                Alt::Name(_) => "name",
                Alt::Ext(_) => "ext",
                Alt::Size(_) => "size",
                Alt::Date(_) => "date",
            })
            .collect()
    }

    /// 只含一个名字子串词的 term 表(直接喂 `scan_volume` / `scan_chunk` 用)。
    fn name_term(s: &[u8]) -> Vec<Term> {
        vec![Term {
            alts: vec![Alt::Name(s.to_vec())],
        }]
    }

    fn names(hits: &[SearchHit]) -> Vec<String> {
        hits.iter().map(|h| h.name.clone()).collect()
    }

    #[test]
    fn compile_splits_on_space_into_and_terms() {
        let ts = compile_query("foo   bar");
        assert_eq!(ts.len(), 2, "空格分词 → 两个 AND 词");
        assert_eq!(ts[0].alts, vec![Alt::Name(b"foo".to_vec())]);
        assert_eq!(ts[1].alts, vec![Alt::Name(b"bar".to_vec())]);
        // 前后空白与连续空格都不产生空词。
        assert_eq!(compile_query("  a  ").len(), 1);
        assert!(compile_query("   ").is_empty());
        assert!(compile_query("").is_empty());
    }

    #[test]
    fn compile_splits_pipe_into_or_alts() {
        let ts = compile_query("jpg|png|gif");
        assert_eq!(ts.len(), 1, "词内 `|` 不分词");
        assert_eq!(ts[0].alts.len(), 3);
        assert_eq!(ts[0].alts[1], Alt::Name(b"png".to_vec()));
        // 裸 `|` 产生的空候选被丢弃,不留一个恒不命中的空串。
        assert_eq!(compile_query("a||b")[0].alts.len(), 2);
        assert!(compile_query("|").is_empty(), "整词只有一个 `|` → 无词");
    }

    #[test]
    fn compile_honours_pipe_escape() {
        // `\|` 是字面量竖线,不分候选。
        assert_eq!(
            compile_query(r"a\|b")[0].alts,
            vec![Alt::Name(b"a|b".to_vec())]
        );
        // `\\` 是字面量反斜杠。
        assert_eq!(
            compile_query(r"a\\b")[0].alts,
            vec![Alt::Name(b"a\\b".to_vec())]
        );
        // 别的位置的反斜杠**原样保留**:用户粘一条路径进来不该被吃掉字符。
        assert_eq!(
            compile_query(r"C:\照片")[0].alts,
            vec![Alt::Name("c:\\照片".as_bytes().to_vec())],
            "非转义位置的反斜杠应原样保留"
        );
        // 折叠只做 ASCII 小写,CJK 字节不被拆坏。
        assert_eq!(
            compile_query("照片")[0].alts,
            vec![Alt::Name("照片".as_bytes().to_vec())]
        );
    }

    #[test]
    fn compile_classifies_predicates() {
        let ts = compile_query("ext:JPG|png size:>10mb date:2024-05");
        assert_eq!(ts.len(), 3);
        assert_eq!(alt_kinds(&ts[0]), vec!["ext", "ext"], "前导 `ext:` 管住整个词");
        assert_eq!(ts[0].alts[0], Alt::Ext(b"jpg".to_vec()), "ext 值折叠小写");
        assert_eq!(alt_kinds(&ts[1]), vec!["size"]);
        assert_eq!(alt_kinds(&ts[2]), vec!["date"]);
        // 谓词名大小写不敏感。
        assert_eq!(alt_kinds(&compile_query("EXT:jpg")[0]), vec!["ext"]);
        assert_eq!(alt_kinds(&compile_query("Size:1kb")[0]), vec!["size"]);
        // 带点的写法也认(用户容易顺手打 `.jpg`)。
        assert_eq!(
            compile_query("ext:.jpg")[0].alts[0],
            Alt::Ext(b"jpg".to_vec())
        );
    }

    #[test]
    fn unknown_predicate_prefix_stays_literal_name() {
        // Windows 文件名里 `:` 非法 → 这种词必然搜不到,但不能静默变成别的意思。
        assert_eq!(
            compile_query("foo:bar")[0].alts,
            vec![Alt::Name(b"foo:bar".to_vec())]
        );
    }

    #[test]
    fn leading_predicate_governs_all_pipe_alternatives() {
        // `ext:jpg|png` = 扩展名 jpg 或 png,不是「扩展名 jpg 或名字含 png」。
        assert_eq!(
            compile_query("ext:jpg|png")[0].alts,
            vec![Alt::Ext(b"jpg".to_vec()), Alt::Ext(b"png".to_vec())]
        );
        // 谓词值也能写区间 / 多段。
        assert_eq!(
            compile_query("date:2024-05..2024-07|2023")[0].alts.len(),
            2,
            "`|` 两侧各是一段日期"
        );
        assert_eq!(alt_kinds(&compile_query("size:1kb|>10mb")[0]), vec!["size", "size"]);
        // 没有前导谓词时才逐候选各自判类型。
        assert_eq!(
            alt_kinds(&compile_query("a|ext:jpg")[0]),
            vec!["name", "ext"],
            "无前导谓词 → 逐候选判"
        );
        // 候选值非法则该候选作废,但不连累同词的其它候选。
        assert_eq!(
            compile_query("size:abc|1kb")[0].alts,
            vec![Alt::Size(Range { lo: 1024, hi: 1024 })]
        );
        assert!(compile_query("size:abc|").is_empty(), "全部候选作废 → 整词作废");
    }

    /// 谓词写错 → 整词作废;**整条查询作废时必须回空,不能退化成全命中**。
    #[test]
    fn bad_predicate_yields_no_hits_not_all_hits() {
        assert!(compile_query("size:abc").is_empty());
        assert!(compile_query("ext:").is_empty());
        assert!(compile_query("date:not-a-date").is_empty());
        assert!(compile_query("size:10xb").is_empty(), "单位尾巴不认 → 作废");

        let v = tiny_volume();
        assert!(search(std::slice::from_ref(&v), "size:abc", 100).is_empty());
        assert!(search(std::slice::from_ref(&v), "ext:", 100).is_empty());
        assert!(
            !search(std::slice::from_ref(&v), "img", 100).is_empty(),
            "对照:正常词在这卷上有命中"
        );
    }

    #[test]
    fn size_units_are_1024_based() {
        assert_eq!(parse_size_value("1024"), Some(1024));
        assert_eq!(parse_size_value("1kb"), Some(1024));
        assert_eq!(parse_size_value("1k"), Some(1024));
        assert_eq!(parse_size_value("1KiB"), Some(1024));
        assert_eq!(parse_size_value("2mb"), Some(2 * 1024 * 1024));
        assert_eq!(parse_size_value("3gb"), Some(3u64 * 1024 * 1024 * 1024));
        assert_eq!(
            parse_size_value(" 4 tb "),
            Some(4u64 * 1024 * 1024 * 1024 * 1024)
        );
        assert_eq!(parse_size_value("0"), Some(0));
        assert_eq!(parse_size_value(""), None);
        assert_eq!(parse_size_value("mb"), None);
        assert_eq!(parse_size_value("10kbytes"), None);
        assert_eq!(parse_size_value("1.5mb"), None, "只认整数字面量");
    }

    #[test]
    fn size_range_operators() {
        let mb = 1024 * 1024;
        assert_eq!(
            parse_range("10mb", parse_size_value),
            Some(Range {
                lo: 10 * mb,
                hi: 10 * mb
            }),
            "裸值 = 精确"
        );
        assert_eq!(
            parse_range(">10mb", parse_size_value),
            Some(Range {
                lo: 10 * mb + 1,
                hi: u64::MAX
            })
        );
        assert_eq!(
            parse_range(">=10mb", parse_size_value),
            Some(Range {
                lo: 10 * mb,
                hi: u64::MAX
            }),
            "`>=` 必须先于 `>` 试"
        );
        assert_eq!(
            parse_range("<1kb", parse_size_value),
            Some(Range { lo: 0, hi: 1023 })
        );
        assert_eq!(
            parse_range("<=1kb", parse_size_value),
            Some(Range { lo: 0, hi: 1024 })
        );
        assert_eq!(
            parse_range("=2kb", parse_size_value),
            Some(Range { lo: 2048, hi: 2048 })
        );
        assert_eq!(
            parse_range("1mb..10mb", parse_size_value),
            Some(Range { lo: mb, hi: 10 * mb })
        );
        assert_eq!(
            parse_range("10mb..1mb", parse_size_value),
            Some(Range { lo: mb, hi: 10 * mb }),
            "写反自动对调,不当空集"
        );
        assert_eq!(
            parse_range("<0", parse_size_value),
            Some(Range { lo: 0, hi: 0 }),
            "`<0` 不下溢"
        );
    }

    #[test]
    fn last_day_handles_leap_and_december() {
        assert_eq!(last_day_of_month(2024, 2), 29, "闰年二月");
        assert_eq!(last_day_of_month(2023, 2), 28, "平年二月");
        assert_eq!(
            last_day_of_month(2024, 12),
            31,
            "十二月要跨到下一年一月再退一天"
        );
        assert_eq!(last_day_of_month(2024, 4), 30);
    }

    #[test]
    fn date_period_expands_to_whole_span() {
        assert_eq!(
            date_period("2024"),
            Some((
                local_day_start(2024, 1, 1).unwrap(),
                local_day_end(2024, 12, 31).unwrap()
            )),
            "裸年 = 整年"
        );
        assert_eq!(
            date_period("2024-05"),
            Some((
                local_day_start(2024, 5, 1).unwrap(),
                local_day_end(2024, 5, 31).unwrap()
            )),
            "裸月 = 整月"
        );
        assert_eq!(
            date_period("2024-05-07"),
            Some((
                local_day_start(2024, 5, 7).unwrap(),
                local_day_end(2024, 5, 7).unwrap()
            )),
            "裸日 = 当天一整天"
        );
        assert_eq!(
            date_period("2024/05/07"),
            date_period("2024-05-07"),
            "`/` 与 `-` 等价"
        );
        // 2024 是闰年 → 整年 366 天。容 1 小时:本地时区若有夏令时,
        // 年初与年末的 UTC 偏移会差一小时,这不是 bug。
        let (lo, hi) = date_period("2024").unwrap();
        assert!(
            (hi - lo - (366 * 86400 - 1)).abs() <= 3600,
            "整年应约 366 天,实得 {} 秒",
            hi - lo
        );
        assert!(date_period("2024-13").is_none(), "月份越界");
        assert!(date_period("2024-05-32").is_none(), "日期越界");
        assert!(date_period("abc").is_none());
        assert!(date_period("1900").is_none(), "早于 UNIX 纪元");
        assert!(date_period("").is_none());
        assert!(date_period("2024-05-").is_none(), "尾巴空段");
    }

    #[test]
    fn date_range_operators_are_relative_to_the_whole_span() {
        let may_start = local_day_start(2024, 5, 1).unwrap();
        let may_end = local_day_end(2024, 5, 31).unwrap();
        assert_eq!(
            parse_date_range("2024-05"),
            Some(Range::from_secs(may_start, may_end))
        );
        assert_eq!(
            parse_date_range(">=2024-05"),
            Some(Range {
                lo: may_start as u64,
                hi: u64::MAX
            }),
            "`>=月` 含整个月"
        );
        assert_eq!(
            parse_date_range(">2024-05").unwrap().lo,
            (may_end + 1) as u64,
            "`>月` 从月结束以后起"
        );
        assert_eq!(
            parse_date_range("<2024-05"),
            Some(Range::from_secs(0, may_start - 1))
        );
        assert_eq!(
            parse_date_range("<=2024-05"),
            Some(Range::from_secs(0, may_end))
        );
        assert_eq!(
            parse_date_range("2024-05..2024-07"),
            Some(Range::from_secs(
                may_start,
                local_day_end(2024, 7, 31).unwrap()
            ))
        );
        assert!(parse_date_range("2024-05-").is_none());
    }

    /// 带 size/mtime 的测试卷(`push` 走 USN 形态,这两列恒为 0,喂不出谓词命中)。
    ///
    /// `unknown.jpg` 故意两列全 0 —— 它是 USN 降级路径的样子,
    /// 谓词必须把它当「拿不到」而不是「0 字节 / 1970 年」。
    fn meta_volume() -> VolumeIndex {
        let may = local_day_start(2024, 5, 7).unwrap() as u64 * 1_000_000;
        let jan = local_day_start(2023, 1, 2).unwrap() as u64 * 1_000_000;
        let mut b = VolumeIndexBuilder::new('C');
        push(&mut b, 0x1000, 5, ATTR_DIRECTORY, "Pics");
        push_meta(&mut b, 0x1001, 0x1000, 0x20, "big.jpg", 20 * 1024 * 1024, may);
        push_meta(&mut b, 0x1002, 0x1000, 0x20, "small.png", 1024, jan);
        push_meta(&mut b, 0x1003, 0x1000, 0x20, "big.mp4", 20 * 1024 * 1024, may);
        push_meta(&mut b, 0x1004, 0x1000, 0x20, "notes.txt", 1024, jan);
        push(&mut b, 0x1005, 0x1000, 0x20, "unknown.jpg");
        b.finish()
    }

    #[test]
    fn multi_term_query_is_conjunction() {
        let v = meta_volume();
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "big jpg", 100)),
            vec!["big.jpg"]
        );
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "big", 100)).len(),
            2,
            "big.jpg 与 big.mp4"
        );
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "jpg big", 100)),
            names(&search(std::slice::from_ref(&v), "big jpg", 100)),
            "AND 与词序无关"
        );
        assert!(search(std::slice::from_ref(&v), "big txt", 100).is_empty());
    }

    #[test]
    fn pipe_alternatives_are_disjunction() {
        let v = meta_volume();
        let got = names(&search(std::slice::from_ref(&v), "png|txt", 100));
        assert_eq!(got.len(), 2);
        assert!(got.contains(&"small.png".to_string()));
        assert!(got.contains(&"notes.txt".to_string()));
        // 与 AND 对照:没有名字同时含 png 和 txt。
        assert!(search(std::slice::from_ref(&v), "png txt", 100).is_empty());
        // 转义后的竖线是字面量:没有名字真含 `|` → 无命中。
        assert!(search(std::slice::from_ref(&v), r"png\|txt", 100).is_empty());
    }

    #[test]
    fn ext_predicate_matches_extension_not_substring() {
        let v = meta_volume();
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "ext:jpg", 100)),
            vec!["big.jpg", "unknown.jpg"]
        );
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "ext:JPG|png", 100)).len(),
            3
        );
        // 目录没有扩展名 → `ext:` 永不命中目录。
        assert!(search(std::slice::from_ref(&v), "ext:pics", 100).is_empty());
        // 与名字词 AND。
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "ext:jpg big", 100)),
            vec!["big.jpg"]
        );
    }

    #[test]
    fn size_predicate_reads_the_size_column() {
        let v = meta_volume();
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "size:>10mb", 100)),
            vec!["big.jpg", "big.mp4"]
        );
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "size:<10kb", 100)),
            vec!["notes.txt", "small.png"]
        );
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "size:1024", 100)).len(),
            2,
            "裸值 = 精确字节数"
        );
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "size:1kb..1mb", 100)).len(),
            2
        );
        // size 列的 0 = 「拿不到」,不能被 `size:0` 当成 0 字节文件。
        assert!(search(std::slice::from_ref(&v), "size:0", 100).is_empty());
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "size:>10mb ext:jpg", 100)),
            vec!["big.jpg"]
        );
    }

    #[test]
    fn date_predicate_reads_the_mtime_column() {
        let v = meta_volume();
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "date:2024-05", 100)),
            vec!["big.jpg", "big.mp4"]
        );
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "date:2023", 100)),
            vec!["notes.txt", "small.png"]
        );
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "date:2024-05-07", 100)).len(),
            2
        );
        assert!(search(std::slice::from_ref(&v), "date:2024-06", 100).is_empty());
        // mtime 列的 0 = 「拿不到」→ 不命中任何日期区间。
        assert!(
            search(std::slice::from_ref(&v), "date:>1970", 100)
                .iter()
                .all(|h| h.name != "unknown.jpg")
        );
        // 三类谓词叠一起。
        assert_eq!(
            names(&search(
                std::slice::from_ref(&v),
                "date:2024 size:>10mb ext:mp4",
                100
            )),
            vec!["big.mp4"]
        );
    }

    #[test]
    fn class_filter_still_applies_to_predicate_hits() {
        let v = meta_volume();
        assert_eq!(
            names(&search_class(
                std::slice::from_ref(&v),
                "size:>10mb",
                100,
                FileClass::Image
            )),
            vec!["big.jpg"]
        );
        assert_eq!(
            names(&search_class(
                std::slice::from_ref(&v),
                "size:>10mb",
                100,
                FileClass::Video
            )),
            vec!["big.mp4"]
        );
        assert!(search_class(
            std::slice::from_ref(&v),
            "size:>10mb",
            100,
            FileClass::Document
        )
        .is_empty());
    }

    // ===================== 分块并行 =====================

    /// 造一个远超一块的卷,让 `chunk_count` 真的 > 1。
    fn wide_volume(n: usize) -> VolumeIndex {
        let mut b = VolumeIndexBuilder::new('C');
        push(&mut b, 0x1000, 5, ATTR_DIRECTORY, "Pics");
        for i in 0..n {
            // 名字长度故意不齐(0..6 个填充),让条目边界不落在整齐位置上。
            let pad = "x".repeat(i % 7);
            push_meta(
                &mut b,
                0x2000 + i as u64,
                0x1000,
                0x20,
                &format!("img_{i:05}{pad}.jpg"),
                (i as u64 + 1) * 1024,
                0,
            );
        }
        b.finish()
    }

    #[test]
    fn chunk_count_follows_records_over_4096_clamped_by_cpu() {
        let cpus = crate::win::cpu_count();
        assert_eq!(chunk_count(0), 1);
        assert_eq!(chunk_count(1), 1);
        assert_eq!(chunk_count(CHUNK_RECORDS), 1);
        assert_eq!(chunk_count(CHUNK_RECORDS + 1), 2.min(cpus));
        assert_eq!(chunk_count(CHUNK_RECORDS * 100), 100.min(cpus));
        // 再多也不会超过核数(n_chunks 被最大线程数全局夹紧)。
        assert_eq!(chunk_count(usize::MAX / 2), cpus);
        assert!(chunk_count(1_000_000) <= cpus);
        assert!(chunk_count(1_000_000) >= 1);
    }

    #[test]
    fn chunked_scan_matches_single_threaded_scan() {
        let n = CHUNK_RECORDS * 3 + 17; // 4 块,且最后一块不满
        let v = wide_volume(n);
        if chunk_count(v.count()) <= 1 {
            // 单核机器上分块路径根本不启用,没什么可比的(不算失败)。
            return;
        }
        let terms = compile_query("img_0004");
        let ov = terms[0].name_len() - 1;
        let par = scan_volume(&v, &terms, 0, ov);
        let seq = scan_chunk(&v, &terms, 0, 0, v.count(), ov, RAW_CAP);
        assert!(!par.is_empty(), "样本里应有命中,否则测试没考到东西");
        assert_eq!(par, seq, "分块并行与整卷单线程应逐项一致");
    }

    /// 每条名字都含 `.jpg`;分块若漏了跨块重叠窗口,总数就会少于条目数。
    #[test]
    fn chunked_scan_finds_every_entry() {
        let n = CHUNK_RECORDS * 2 + 5;
        let v = wide_volume(n);
        let terms = compile_query(".jpg");
        let got = scan_volume(&v, &terms, 0, terms[0].name_len() - 1);
        // 目录 "Pics" 不含 .jpg,其余 n 条都含。
        assert_eq!(got.len(), n, "每条都应命中,块边界处不能漏");
        assert_eq!(v.count(), n + 1);
        // 下标严格递增且无重复:去重靠「命中归属条目落在本块区间内」这一条判定,
        // 不靠额外的去重集合。
        assert!(got.windows(2).all(|w| w[0].0 < w[1].0));
    }

    /// 专门考跨块边界的命中:子串起点在上一块末尾、尾巴伸进下一块。
    #[test]
    fn overlap_window_recovers_straddling_match() {
        let n = CHUNK_RECORDS * 2 + 5;
        let mut b = VolumeIndexBuilder::new('C');
        push(&mut b, 0x1000, 5, ATTR_DIRECTORY, "Pics");
        for i in 0..n {
            // 定长 8 位数字名:只有跨块边界那一处的「尾 4 位 + 头 4 位」拼接是唯一的。
            push_meta(&mut b, 0x2000 + i as u64, 0x1000, 0x20, &format!("{i:08}"), 1024, 0);
        }
        let v = b.finish();
        // 块边界**不是** 4096 的整数倍:`step = n.div_ceil(chunks)`,而 chunks 又被
        // 核数夹紧,所以边界随卷长与核数变。这里按 scan_volume 的同一条公式取真边界。
        let chunks = chunk_count(v.count());
        if chunks <= 1 {
            return; // 单核机器上不分块,没有边界可考。
        }
        let bnd = v.count().div_ceil(chunks); // 第二块的起点(条目下标)
        let a = v.fold_name(bnd - 1).to_vec();
        let c = v.fold_name(bnd).to_vec();
        let mut needle = a[a.len() - 4..].to_vec();
        needle.extend_from_slice(&c[..4]);
        assert_eq!(needle.len(), 8);

        let terms = name_term(&needle);
        // 不给重叠窗口:块 A 的窗口到本块末尾为止,块 B 的窗口从下一块开头起,
        // 这条跨块命中两头都够不着 → 漏。
        assert!(
            scan_volume(&v, &terms, 0, 0).is_empty(),
            "不给重叠窗口就该漏,否则这个测试没考到东西"
        );
        // 给了 needle_len-1 的重叠 → 找回来了,归属起点所在的那条,命中位是条目内偏移 4。
        assert_eq!(
            scan_volume(&v, &terms, 0, needle.len() - 1),
            vec![(bnd as u32 - 1, 4)]
        );
        // 整卷单线程扫描同样能找到(它本来就看得见整个池)。
        assert_eq!(
            scan_chunk(&v, &terms, 0, 0, v.count(), needle.len() - 1, RAW_CAP),
            vec![(bnd as u32 - 1, 4)]
        );
    }

    /// rayon 的完成顺序是不定的:合并必须按块序,否则同一条查询两次跑出的
    /// 并列次序会不一样,结果就不可复现。
    #[test]
    fn chunked_results_are_reproducible() {
        let n = CHUNK_RECORDS * 3 + 17;
        let v = wide_volume(n);
        let terms = compile_query("img");
        let ov = terms[0].name_len() - 1;
        let first = scan_volume(&v, &terms, 0, ov);
        assert!(!first.is_empty());
        for _ in 0..5 {
            assert_eq!(
                scan_volume(&v, &terms, 0, ov),
                first,
                "同一条查询的命中次序必须可复现"
            );
        }
    }

    #[test]
    fn search_over_chunked_volume_is_correct() {
        let n = CHUNK_RECORDS * 2 + 5;
        let v = wide_volume(n);
        // `img_0004` 命中 i = 40..49 这 10 条(名字是 img_{i:05})。
        assert_eq!(names(&search(std::slice::from_ref(&v), "img_0004", 100)).len(), 10);
        assert_eq!(
            names(&search(std::slice::from_ref(&v), "img_0004 ext:jpg", 100)).len(),
            10,
            "分块路径上谓词 AND 一样生效"
        );
        assert!(search(std::slice::from_ref(&v), "img_0004 ext:png", 100).is_empty());
        // limit 仍然截断。
        assert_eq!(search(std::slice::from_ref(&v), "img", 3).len(), 3);
    }

    #[test]
    fn prefix_hits_rank_first() {
        let mut b = VolumeIndexBuilder::new('C');
        push(&mut b, 0x5000, 5, 0x20, "xabc.txt");
        push(&mut b, 0x5001, 5, 0x20, "abc.jpg");
        let v = b.finish();
        let hits = search(std::slice::from_ref(&v), "abc", 100);
        assert_eq!(hits[0].name, "abc.jpg", "前缀命中应排前");
        assert_eq!(hits[1].name, "xabc.txt");
    }

    #[test]
    fn empty_and_no_match() {
        let v = tiny_volume();
        assert!(search(std::slice::from_ref(&v), "", 100).is_empty());
        assert!(search(std::slice::from_ref(&v), "zzzz-not-exist", 100).is_empty());
    }

    #[test]
    fn class_filter_limits_results() {
        let v = tiny_volume();
        let hits = search_class(std::slice::from_ref(&v), "img", 10, FileClass::Image);
        assert_eq!(hits.len(), 1, "IMG_0001.JPG 属图片类");
        assert!(search_class(std::slice::from_ref(&v), "img", 10, FileClass::Archive).is_empty());
        // 目录名无扩展名 → 只归「全部」。
        assert!(search_class(std::slice::from_ref(&v), "pics", 10, FileClass::Image).is_empty());
        assert_eq!(search_class(std::slice::from_ref(&v), "pics", 10, FileClass::All).len(), 1);
        // RAW 扩展名归图片类(与收录口径一致)。
        assert_eq!(search_class(std::slice::from_ref(&v), ".nef", 10, FileClass::Image).len(), 1);
        // 视频/音频类各归各表。
        assert_eq!(search_class(std::slice::from_ref(&v), "dsc", 10, FileClass::Video).len(), 0);
        // search() 默认全部档。
        assert_eq!(search(std::slice::from_ref(&v), "img", 10).len(), 1);
    }

    #[test]
    fn class_flags_marks_each_category() {
        assert_eq!(class_flags("a.jpg"), CLASS_IMAGE | FLAG_INDEXABLE);
        assert_eq!(class_flags("a.MP4"), CLASS_VIDEO | FLAG_INDEXABLE);
        assert_eq!(class_flags("a.mp3"), CLASS_AUDIO);
        assert_eq!(class_flags("a.zip"), CLASS_ARCHIVE);
        assert_eq!(class_flags("a.exe"), CLASS_EXE);
        assert_eq!(class_flags("a.pdf"), CLASS_DOC);
        assert_eq!(class_flags("README"), 0);
        assert_eq!(class_flags("a."), 0, "点在尾部不算扩展名");
    }

    #[test]
    fn save_load_roundtrip() {
        let v = tiny_volume();
        let mut journals = HashMap::new();
        journals.insert(
            'C',
            crate::mft::JournalState {
                journal_id: 0x1234_5678,
                first_usn: 0x1_0000,
                next_usn: 0x2_0000,
            },
        );
        let dir = std::env::temp_dir().join(format!("rk-nameidx-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("nameindex.bin");
        save(&p, &[v], &journals).expect("save");
        let loaded = load(&p).expect("load");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(loaded.vols.len(), 1);
        let v2 = &loaded.vols[0];
        assert_eq!(v2.letter, 'C');
        assert_eq!(v2.count(), 5);
        assert_eq!(v2.path_of(1), r"C:\Pics\IMG_0001.JPG");
        let hits = search(&loaded.vols, "raw", 100);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, r"C:\Pics\RAW");
        assert_eq!(loaded.journals[&'C'].journal_id, 0x1234_5678);
    }

    #[test]
    fn metadata_survives_save_load_roundtrip() {
        let mut b = VolumeIndexBuilder::new('C');
        b.push_mft(&mft_rec(0x100, 5, "a.nef", 0x20, 1_234_567, FT_EPOCH + 10_000_000));
        let v = b.finish();
        let dir = std::env::temp_dir().join(format!("rk-nameidx-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("nameindex.bin");
        save(&p, std::slice::from_ref(&v), &HashMap::new()).expect("save");
        let loaded = load(&p).expect("load");
        std::fs::remove_dir_all(&dir).ok();
        let v2 = &loaded.vols[0];
        assert_eq!(v2.size_of(0), 1_234_567);
        assert_eq!(v2.mtime_of(0), 1_000_000);
        assert_eq!(v2.class_of(0), v.class_of(0));
        assert!(v2.is_indexable(0));
    }

    #[test]
    fn mft_build_carries_size_and_mtime() {
        let mut b = VolumeIndexBuilder::new('C');
        b.push_mft(&mft_rec(0x100, 5, "a.jpg", 0x20, 4096, FT_EPOCH + 10_000_000));
        b.push_mft(&mft_rec(0x101, 5, "dir", ATTR_DIRECTORY, 0, 0));
        // 卷根(下标 5)与无名记录不进索引。
        b.push_mft(&mft_rec(5, 5, "", ATTR_DIRECTORY, 0, 0));
        let v = b.finish();
        assert_eq!(v.count(), 2);
        assert_eq!(v.size_of(0), 4096);
        assert_eq!(v.mtime_of(0), 1_000_000, "FILETIME → UNIX 微秒");
        assert_eq!(v.mtime_usn_of(0), 0, "不是 journal 事件写进来的,变更时刻留空");
        assert!(v.is_indexable(0));
        assert!(v.is_dir(1));
        assert!(!v.is_indexable(1));
        assert_eq!(v.mtime_of(1), 0, "write_time=0 不可表示 → 记 0");
    }

    #[test]
    fn delta_rename_updates_in_place() {
        let mut v = tiny_volume();
        let c = v.apply_delta(vec![DeltaOp::Upsert(up(
            0x1001,
            0x1000,
            0x20,
            "IMG_0002.JPG",
        ))]);
        assert_eq!((c.added, c.updated, c.removed), (0, 1, 0));
        let hits = search(std::slice::from_ref(&v), "IMG_0002", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, r"C:\Pics\IMG_0002.JPG");
        assert!(search(std::slice::from_ref(&v), "IMG_0001", 10).is_empty());
        // 改名的是文件,兄弟与父路径不受影响。
        let hits = search(std::slice::from_ref(&v), "dsc.nef", 10);
        assert_eq!(hits[0].path, r"C:\Pics\RAW\DSC.NEF");
    }

    #[test]
    fn delta_rename_dir_keeps_children_paths() {
        let mut v = tiny_volume();
        let _ = v.apply_delta(vec![DeltaOp::Upsert(up(
            0x2000,
            0x1000,
            ATTR_DIRECTORY,
            "RAW2",
        ))]);
        let hits = search(std::slice::from_ref(&v), "dsc.nef", 10);
        assert_eq!(hits[0].path, r"C:\Pics\RAW2\DSC.NEF", "目录改名后子路径应跟随");
    }

    #[test]
    fn delta_delete_file_and_dir() {
        let mut v = tiny_volume();
        let c = v.apply_delta(vec![DeltaOp::Delete(0x2001)]);
        assert_eq!(c.removed, 1);
        assert!(search(std::slice::from_ref(&v), ".nef", 10).is_empty());
        let c = v.apply_delta(vec![DeltaOp::Delete(0x2000)]);
        assert_eq!(c.removed, 1);
        assert!(search(std::slice::from_ref(&v), "raw", 10).is_empty());
        let hits = search(std::slice::from_ref(&v), "IMG_0001", 10);
        assert_eq!(hits[0].path, r"C:\Pics\IMG_0001.JPG", "无关条目路径不变");
    }

    #[test]
    fn delta_delete_dir_orphans_children() {
        let mut v = tiny_volume();
        let _ = v.apply_delta(vec![DeltaOp::Delete(0x2000)]);
        let hits = search(std::slice::from_ref(&v), "dsc.nef", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, r"C:\DSC.NEF", "父目录被删 → 子条目路径退化为自身");
    }

    #[test]
    fn delta_add_under_existing_dir() {
        let mut v = tiny_volume();
        let c = v.apply_delta(vec![DeltaOp::Upsert(up(0x2002, 0x2000, 0x20, "NEW.RAW"))]);
        assert_eq!(c.added, 1);
        let hits = search(std::slice::from_ref(&v), "new.raw", 10);
        assert_eq!(hits[0].path, r"C:\Pics\RAW\NEW.RAW");
    }

    /// 连续多轮增量(新增 / 改名 / 删除交错)之后,查询视图必须始终与名字池同步。
    ///
    /// 已知现象: 服务日志一直报「新增 N」,但新建的文件在快查里
    /// 查不到、落盘索引里也没有它的名字 —— 增量只改了列、没把折叠池与有序
    /// 视图一起刷新。单轮新增能过的测试抓不到这种"跑久了才失配"的情形,
    /// 所以这里按轮校验不变量,并在最后同时验证子串查询与路径反查。
    #[test]
    fn delta_keeps_query_view_in_sync_across_rounds() {
        let mut v = tiny_volume();
        let rounds: Vec<Vec<DeltaOp>> = vec![
            // 纯新增:走"跳过压实"的快路径。
            vec![DeltaOp::Upsert(up(0x2002, 0x2000, 0x20, "NEW_SHOT.CR3"))],
            // 新增 + 原位改名混在一批:会触发压实与全池重写。
            vec![
                DeltaOp::Upsert(up(0x2003, 0x2000, 0x20, "ANOTHER.NEF")),
                DeltaOp::Upsert(up(0x1001, 0x1000, 0x20, "RENAMED.JPG")),
            ],
            // 删除:摘条目 + 重映射父引用。
            vec![DeltaOp::Delete(0x2002)],
        ];
        for ops in rounds {
            let _ = v.apply_delta(ops);
            assert_eq!(v.fold.len(), v.pool.len(), "折叠池必须覆盖整个名字池");
            assert_eq!(v.order.len(), v.count(), "有序视图必须覆盖全部条目");
            // 每轮都真跑一次查询:视图长度对得上但内容错位时也能暴露。
            let _ = search(std::slice::from_ref(&v), "jpg", 10);
        }
        assert_eq!(
            search(std::slice::from_ref(&v), "another.nef", 10).len(),
            1,
            "第二轮新增的名字必须可查"
        );
        assert!(
            search(std::slice::from_ref(&v), "new_shot", 10).is_empty(),
            "已删除的名字不该再命中"
        );
        assert_eq!(
            search(std::slice::from_ref(&v), "renamed", 10)[0].path,
            r"C:\Pics\RENAMED.JPG",
            "改名后的名字与父路径都要更新"
        );
        assert!(
            v.lookup_path(r"C:\Pics\RENAMED.JPG").is_some(),
            "按完整路径反查也要能看到增量结果"
        );
    }

    /// 纯新增轮次走增量收尾(追加折叠 + 单趟归并),各视图必须与全量重建逐位一致。
    ///
    /// 增量收尾是性能路径,最容易出的错是"口径分叉":归并用一份比较口径、
    /// 全量重排用另一份,查询二分就会静默漏命中。这里每轮都拿一个只走全量
    /// `post_process` 的孪生卷对账(名字唯一,无并列歧义),并把 CSR 一并对。
    #[test]
    fn delta_pure_add_incremental_matches_full_rebuild() {
        let mut v = tiny_volume();
        let mut twin = tiny_volume();
        for r in 0..12u32 {
            let ops: Vec<DeltaOp> = (0..3)
                .map(|k| {
                    let frn = 0x9000u64 + ((r * 3 + k) as u64);
                    DeltaOp::Upsert(up(frn, 0x1000, 0x20, &format!("BATCH{r:02}_IMG{k}.JPG")))
                })
                .collect();
            let c1 = v.apply_delta(ops.clone());
            let c2 = twin.apply_delta(ops);
            assert_eq!((c1.added, c2.added), (3, 3));
            assert_eq!(
                (c1.updated, c1.removed),
                (0, 0),
                "本测试只喂纯新增,喂错了就走不到快路径"
            );
            assert_eq!(v.order, twin.order, "第 {r} 轮后有序视图应与全量重建一致");
            assert_eq!(v.fold, twin.fold, "第 {r} 轮后折叠池应与全量重建一致");
            assert_eq!(v.csr_start, twin.csr_start, "第 {r} 轮后 CSR 应与全量重建一致");
            assert_eq!(v.csr_child, twin.csr_child, "第 {r} 轮后 CSR 应与全量重建一致");
        }
        let hits = search(std::slice::from_ref(&v), "batch11_img2", 10);
        assert_eq!(hits.len(), 1, "最后一轮新增的名字必须立即可查");
        assert!(v.lookup_path(r"C:\Pics\BATCH11_IMG2.JPG").is_some());
    }

    #[test]
    fn delta_order_delete_then_recreate_same_frn() {
        let mut v = tiny_volume();
        let c = v.apply_delta(vec![
            DeltaOp::Delete(0x1001),
            DeltaOp::Upsert(up(0x1001, 0x1000, 0x20, "REBORN.JPG")),
        ]);
        assert_eq!((c.added, c.updated, c.removed), (1, 0, 1));
        let hits = search(std::slice::from_ref(&v), "reborn", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, r"C:\Pics\REBORN.JPG");
    }

    #[test]
    fn delta_survives_save_load_roundtrip() {
        let mut v = tiny_volume();
        let _ = v.apply_delta(vec![
            DeltaOp::Upsert(up(0x1001, 0x1000, 0x20, "改名后.JPG")),
            DeltaOp::Delete(0x3000),
        ]);
        let dir = std::env::temp_dir().join(format!("rk-nameidx-delta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("nameindex.bin");
        save(&p, &[v], &HashMap::new()).expect("save");
        let loaded = load(&p).expect("load");
        std::fs::remove_dir_all(&dir).ok();
        let hits = search(&loaded.vols, "改名后", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, r"C:\Pics\改名后.JPG");
        assert!(search(&loaded.vols, "orphan.log", 10).is_empty());
    }

    #[test]
    fn delta_does_not_wipe_mft_metadata() {
        let mut b = VolumeIndexBuilder::new('C');
        b.push_mft(&mft_rec(0x100, 5, "a.jpg", 0x20, 4096, FT_EPOCH + 10_000_000));
        let mut v = b.finish();
        let mut u = up(0x100, 5, 0x20, "b.jpg");
        u.mtime_usn = 777;
        let c = v.apply_delta(vec![DeltaOp::Upsert(u)]);
        assert_eq!(c.updated, 1);
        assert_eq!(v.name(0), "b.jpg");
        assert_eq!(
            v.size_of(0),
            4096,
            "journal 通道 size=0 表示「拿不到」,不能抹掉建库时的真值"
        );
        assert_eq!(v.mtime_of(0), 1_000_000);
        assert_eq!(v.mtime_usn_of(0), 777);
        assert!(v.is_indexable(0), "改名后类别位要按新名字重算");
        // 改成非媒体名 → 可入库标记必须撤掉。
        let c = v.apply_delta(vec![DeltaOp::Upsert(up(0x100, 5, 0x20, "b.txt"))]);
        assert_eq!(c.updated, 1);
        assert!(!v.is_indexable(0));
        assert_eq!(v.class_of(0), CLASS_DOC);
    }

    #[test]
    fn no_op_delta_leaves_index_untouched() {
        let mut v = tiny_volume();
        let pool_before = v.pool.len();
        let n_before = v.count();
        let c = v.apply_delta(vec![DeltaOp::Delete(0xdead_beef)]);
        assert_eq!(c, DeltaCounts::default());
        assert_eq!(v.count(), n_before);
        assert_eq!(v.pool.len(), pool_before, "零变化不该重建折叠池");
    }

    #[test]
    fn pure_add_delta_skips_compact() {
        let mut v = tiny_volume();
        let pool_before = v.pool.len();
        let c = v.apply_delta(vec![DeltaOp::Upsert(up(0x2002, 0x2000, 0x20, "NEW.RAW"))]);
        assert_eq!(c.added, 1);
        assert_eq!(
            v.pool.len(),
            pool_before + "NEW.RAW".len(),
            "纯新增不该重写名字池"
        );
        assert!(v.off.windows(2).all(|w| w[0] <= w[1]), "off 列仍单调");
        assert_eq!(v.path_of(v.count() - 1), r"C:\Pics\RAW\NEW.RAW");
    }

    #[test]
    fn update_delta_compacts_and_reclaims_dead_bytes() {
        let mut v = tiny_volume();
        let pool_before = v.pool.len();
        let _ = v.apply_delta(vec![DeltaOp::Upsert(up(
            0x1001,
            0x1000,
            0x20,
            "IMG_0002.JPG",
        ))]);
        assert_eq!(v.count(), 5, "改名不换条数");
        assert!(
            v.pool.len() <= pool_before + "IMG_0002.JPG".len(),
            "压实应回收旧名字的死字节"
        );
        assert!(v.off.windows(2).all(|w| w[0] <= w[1]), "off 列恢复单调");
    }

    #[test]
    fn csr_children_match_parent_column() {
        let v = tiny_volume();
        let n = v.count();
        // 卷根桶:只有 Pics(0)。orphan.log(4) 的父不在流里,不挂任何桶。
        assert_eq!(v.children_of(n), &[0]);
        // Pics 的子项:IMG_0001.JPG(1)、RAW(2)。
        assert_eq!(v.children_of(0), &[1, 2]);
        // RAW 的子项:DSC.NEF(3)。
        assert_eq!(v.children_of(2), &[3]);
        // 叶子没有子项。
        assert!(v.children_of(1).is_empty());
        // 孤儿条目的父不在流里,不挂任何桶 —— 全部子项总数 = 非孤儿条目数。
        let total: usize = (0..=n).map(|i| v.children_of(i).len()).sum();
        assert_eq!(total, n - 1, "只有 orphan.log 没被挂进桶");
        // 越界槽位安全返回空。
        assert!(v.children_of(n + 1).is_empty());
    }

    #[test]
    fn csr_is_rebuilt_after_delta() {
        let mut v = tiny_volume();
        let n0 = v.count();
        let _ = v.apply_delta(vec![DeltaOp::Upsert(up(0x2002, 0x2000, 0x20, "NEW.RAW"))]);
        // RAW 的下标在压实后可能变,按名字找回来再验子项。
        let raw = (0..v.count())
            .find(|&i| v.name(i) == "RAW")
            .expect("RAW 仍在");
        assert_eq!(v.children_of(raw).len(), 2, "RAW 下应挂 DSC.NEF 与 NEW.RAW");
        let _ = v.apply_delta(vec![DeltaOp::Delete(0x2002)]);
        assert_eq!(v.children_of(raw).len(), 1, "删掉后子项跟着少一个");
        assert_eq!(v.count(), n0);
    }
}
