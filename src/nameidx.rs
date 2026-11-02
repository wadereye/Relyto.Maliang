//! 文件名 SoA 内存索引与快速查找(全卷名字列式索引)。
//!
//! 数据布局刻意做成**列式并行数组 + 名字池**,与 mft.rs 的 USN 记录流对接:
//! - `pool`:原始名字 UTF-8 顺序池(展示用);
//! - `fold`:ASCII 折叠(小写)副本(匹配用,子串查找直接 `memmem` 扫池,
//!   数百万条目单次查询毫秒级;UTF-8 自同步性保证不会跨字符误命中);
//! - `off/len/parent/frn/attrs`:定宽列,`parent` 存**条目下标**,拼路径时
//!   沿父链上溯,全卷不存完整路径字符串(省下数倍内存)。
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
const ATTR_DIRECTORY: u32 = 0x10;

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
    /// 按折叠名排序的条目下标(前缀二分/有序遍历用)。
    order: Vec<u32>,
}

/// 索引构建器:接 mft.rs 的枚举回调,先零散收集,`finish` 时统一收束。
#[derive(Debug)]
pub struct VolumeIndexBuilder {
    letter: char,
    pool: Vec<u8>,
    off: Vec<u32>,
    len: Vec<u32>,
    frn: Vec<u64>,
    attrs: Vec<u16>,
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
            frn_to_idx: HashMap::new(),
            raw_parent: Vec::new(),
        }
    }

    /// 已收集条数。
    pub fn count(&self) -> u32 {
        self.frn.len() as u32
    }

    /// 追加一条 USN 记录(frn/parent_frn/attrs/name 来自 mft::scan_volume 回调)。
    pub fn push(&mut self, frn: u64, parent_frn: u64, attrs: u32, name: &[u16]) {
        // 卷根记录(FRN 5)名空父自指,不进索引 —— 根由 PARENT_ROOT 隐式表达。
        if frn & FRN_INDEX_MASK == ROOT_FRN_INDEX || name.is_empty() {
            return;
        }
        let mut s = String::from_utf16_lossy(name);
        s = sanitize_name(s);
        let off = self.pool.len() as u32;
        self.pool.extend_from_slice(s.as_bytes());
        self.off.push(off);
        self.len.push(s.len() as u32);
        self.frn.push(frn);
        self.attrs.push(attrs as u16);
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
            order: Vec::new(),
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
    let hit = |list: &[&str]| list.iter().any(|x| *x == e);
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

/// 名字成分不允许出现分隔符/控制符(理论不可能,防御性收口)。
fn sanitize_name(mut s: String) -> String {
    if s.contains(['\\', '/', '\0']) {
        s = s.replace(['\\', '/', '\0'], "_");
    }
    s
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
    let mut q_fold: Vec<u8> = q.as_bytes().to_vec();
    for b in &mut q_fold {
        b.make_ascii_lowercase();
    }
    // (卷序, 条目下标, 命中字节位)
    let mut hits: Vec<(usize, u32, usize)> = Vec::new();
    for (vi, v) in vols.iter().enumerate() {
        for idx in v.find_all(&q_fold, RAW_CAP) {
            if !ext_in_class(v.ext_fold(idx as usize), class) {
                continue;
            }
            let pos = v.match_pos(idx as usize, &q_fold);
            hits.push((vi, idx, pos));
        }
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
    /// 把 journal 增量按序并入索引:删除移除条目,创建/改名/移动更新或追加,
    /// 触碰条目的父引用经 FRN 重解析(父为卷根 → PARENT_ROOT,找不到/已删
    /// → PARENT_ORPHAN),最后重建折叠池与有序视图。
    /// 复杂度 O(旧条目数 + 增量),每卷每次启动跑一次,百万条目亚秒级。
    pub fn apply_delta(&mut self, ops: Vec<DeltaOp>) -> DeltaCounts {
        let mut counts = DeltaCounts::default();
        if ops.is_empty() {
            return counts;
        }
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
                    let off = self.pool.len() as u32;
                    self.pool.extend_from_slice(name.as_bytes());
                    match frn_to_idx.get(&key) {
                        Some(&idx) => {
                            // 原位更新:名字指针指向池尾新副本,旧字节成为
                            // 死空间(下次全量重建时回收,增量不搬运)。
                            self.off[idx as usize] = off;
                            self.len[idx as usize] = name.len() as u32;
                            self.attrs[idx as usize] = u.attrs as u16;
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
        // 无条件压实:原位更新把名字新副本追加到池尾,off 列会暂时失去
        // 单调性(entry_at 的二分依赖它),这里按条目序重写列与名字池,
        // 顺带回收死字节;有删除时同时完成摘除与 parent 重映射。
        self.compact(&removed);
        self.post_process();
        counts
    }

    /// 按当前条目序重建各列与名字池(`removed` 里的下标被摘除;
    /// 父被删的幸存条目降级孤儿,路径退化为自身名字)。
    fn compact(&mut self, removed: &[u32]) {
        let rm: std::collections::HashSet<u32> = removed.iter().copied().collect();
        let n = self.off.len();
        let mut remap = vec![u32::MAX; n];
        let mut keep: Vec<usize> = Vec::with_capacity(n.saturating_sub(removed.len()));
        for i in 0..n {
            if !rm.contains(&(i as u32)) {
                remap[i] = keep.len() as u32;
                keep.push(i);
            }
        }
        let mut pool = Vec::with_capacity(self.pool.len());
        let mut off = Vec::with_capacity(keep.len());
        let mut len = Vec::with_capacity(keep.len());
        let mut parent = Vec::with_capacity(keep.len());
        let mut frn = Vec::with_capacity(keep.len());
        let mut attrs = Vec::with_capacity(keep.len());
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
        }
        self.pool = pool;
        self.off = off;
        self.len = len;
        self.parent = parent;
        self.frn = frn;
        self.attrs = attrs;
    }

    /// 构建/载入共用的收尾:重建折叠池(ASCII 小写,查询折叠口径一致,
    /// CJK 不受影响)与按名排序的有序视图。
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
        order.sort_unstable_by(|&a, &b| {
            let sa = off[a as usize] as usize;
            let la = len[a as usize] as usize;
            let sb = off[b as usize] as usize;
            let lb = len[b as usize] as usize;
            fold[sa..sa + la].cmp(&fold[sb..sb + lb])
        });
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

    /// 折叠名里最后一个 '.' 之后的扩展名(无 '.' 或点在尾部 → 空)。
    /// 类型过滤用;不分配,直接比对折叠字节。
    fn ext_fold(&self, i: usize) -> &[u8] {
        let f = self.fold_name(i);
        match f.iter().rposition(|&b| b == b'.') {
            Some(p) if p + 1 < f.len() => &f[p + 1..],
            _ => &[],
        }
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

    /// 该条目折叠名里子串首次命中的字节位(排序键;找不到返回 usize::MAX)。
    fn match_pos(&self, i: usize, needle: &[u8]) -> usize {
        memchr::memmem::find(self.fold_name(i), needle).unwrap_or(usize::MAX)
    }

    /// 全卷子串查找:memmem 扫折叠池,命中位反查条目下标。
    /// `limit` 只做总量保险,正常排序在上层归并后进行。
    fn find_all(&self, needle: &[u8], limit: usize) -> Vec<u32> {
        let mut out = Vec::new();
        if needle.is_empty() || self.fold.is_empty() {
            return out;
        }
        let finder = memchr::memmem::Finder::new(needle);
        let mut pos = 0usize;
        while let Some(p) = finder.find(&self.fold[pos..]) {
            let abs = pos + p;
            if let Some(idx) = self.entry_at(abs) {
                out.push(idx);
                if out.len() >= limit {
                    break;
                }
            }
            pos = abs + 1;
            if pos >= self.fold.len() {
                break;
            }
        }
        out
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
const MAGIC: &[u8; 4] = b"RMNI";
const FORMAT_VERSION: u32 = 1;

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
        let mut v = VolumeIndex {
            letter,
            pool,
            fold: Vec::new(),
            off,
            len,
            parent,
            frn,
            attrs,
            order: Vec::new(),
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
    Ok(b.chunks_exact(4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).collect())
}

fn read_col_u64(c: &mut Cursor, n: usize) -> Result<Vec<u64>> {
    let b = c.take(n * 8)?;
    Ok(b.chunks_exact(8)
        .map(|s| {
            let mut a = [0u8; 8];
            a.copy_from_slice(s);
            u64::from_le_bytes(a)
        })
        .collect())
}

fn read_col_u16(c: &mut Cursor, n: usize) -> Result<Vec<u16>> {
    let b = c.take(n * 2)?;
    Ok(b.chunks_exact(2).map(|s| u16::from_le_bytes([s[0], s[1]])).collect())
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

/// `maliang --mft-scan [C,D] [--mft-report <文件>]`:全量枚举 NTFS 卷并落盘
/// 名字索引。由计划任务(或提权子进程)拉起;**report 直写文件**而不依赖控制台句柄
/// —— GUI 子系统进程在提权会话里 stdout 可能被 attach 控制台顶掉,导致
/// 输出丢失(已知坑),文件句柄不受影响。统计供首次全量扫描核对(条数/耗时/体积)。
///
/// 报告路径默认取数据目录下的 `mft_report.txt` —— 计划任务的参数是注册时固定的,
/// 不能把路径写死进去(数据目录一改就永远等不到信号),由子进程每次现算。
///
/// 报告协议:子进程**开局截断**旧报告,写过程行,收尾行以「成功」或「失败」开头;
/// 过程行里以 `PROGRESS <0..1> ` 打头的是进度行(比例按已完成卷数,GUI 据此画进度条)。
/// GUI 轮询收尾行作为完成信号,并用文件 mtime 排除上一轮的残留内容
/// (提权进程写下的报告属高完整性级,非提权的 GUI 未必删得动,不能靠删除来清场)。
pub fn cli_mft_scan(spec: Option<&str>, report: Option<&str>) -> Result<()> {
    let path = report
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::db::data_dir().join("mft_report.txt"));
    let res = cli_mft_scan_run(spec, Some(path.as_path()));
    if let Err(e) = &res {
        // 失败也进报告(GUI 据此把状态告诉用户,而不是静默失败)。
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(&path) {
            let _ = writeln!(f, "失败: {e:#}");
        }
    }
    res
}

fn cli_mft_scan_run(spec: Option<&str>, report: Option<&std::path::Path>) -> Result<()> {
    let mut rep = report
        .map(|p| -> Result<std::fs::File> {
            // 截断而非追加:这份报告只属于本轮。
            Ok(std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(p)?)
        })
        .transpose()?;
    let mut say = |line: String| {
        println!("{line}");
        if let Some(f) = rep.as_mut() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.write_all(b"\n");
            let _ = f.flush();
        }
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
    let mut skipped: Vec<(char, crate::mft::Support)> = Vec::new();
    for (vi, p) in probes.into_iter().enumerate() {
        if let Some(w) = &want {
            if !w.contains(&p.letter) {
                continue;
            }
        }
        match p.support {
            crate::mft::Support::Usn => {}
            other => {
                say(format!("[{0}:] 跳过(原因:{1:?})", p.letter, other));
                skipped.push((p.letter, other));
                continue;
            }
        }
        let j = p.journal.expect("Support::Usn 必带 journal");
        let pos = vols.iter().position(|v| v.letter == p.letter);
        let saved = journals.get(&p.letter).copied();
        let can_incr = pos.is_some()
            && saved
                .map(|s| {
                    s.journal_id == j.journal_id
                        && s.next_usn >= j.first_usn
                        && s.next_usn <= j.next_usn
                })
                .unwrap_or(false);
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
            let r = crate::mft::read_journal(p.letter, j.journal_id, s.next_usn, |frn, parent, attrs, reason, name| {
                if reason & crate::mft::USN_REASON_CLOSE == 0 {
                    return true;
                }
                let renamed = reason & crate::mft::USN_REASON_RENAME_NEW_NAME != 0
                    || reason & crate::mft::USN_REASON_FILE_CREATE != 0;
                let gone = !renamed
                    && (reason & crate::mft::USN_REASON_FILE_DELETE != 0
                        || reason & crate::mft::USN_REASON_RENAME_OLD_NAME != 0);
                let nm = String::from_utf16_lossy(name);
                if renamed && !nm.is_empty() {
                    ops.push(DeltaOp::Upsert(DeltaUpsert {
                        frn,
                        parent_frn: parent,
                        attrs,
                        name: nm,
                    }));
                } else if gone {
                    ops.push(DeltaOp::Delete(frn));
                }
                true
            });
            match r {
                Ok(st) => {
                    let c = vols[pos.unwrap()].apply_delta(ops);
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
                    incr_ok = true;
                    touched += 1;
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
        say(format!(
            "PROGRESS {frac:.2} [{letter}:] 全量枚举 USN(journal_id={jid:#x}, next_usn={next:#x})…",
            letter = p.letter,
            jid = j.journal_id,
            next = j.next_usn
        ));
        let mut b = VolumeIndexBuilder::new(p.letter);
        let ctrl = crate::scan::ScanControl::new();
        let last = std::cell::Cell::new(std::time::Instant::now());
        let letter = p.letter;
        let stats = crate::mft::scan_volume(p.letter, &j, Some(&ctrl), |frn, parent, attrs, name| {
            b.push(frn, parent, attrs, name);
            if last.get().elapsed() >= std::time::Duration::from_secs(1) {
                last.set(std::time::Instant::now());
                // 逐秒条数写进报告:原来只有黑控制台里的 \r 计数能看到,现在进界面。
                let n = b.count();
                say(format!("PROGRESS {frac:.2} [{letter}:] 全量枚举中…已 {n} 条"));
            }
            true
        })?;
        let pool_mb = b.pool.len() as f64 / 1048576.0;
        let count = b.count();
        let idx = b.finish();
        say(format!(
            "[{0}:] 完成:内核记录 {1} 条 → 索引 {2} 条(文件 {3},目录 {4}),名字池 {5:.1} MB,耗时 {6:.1}s",
            p.letter, stats.records, count, stats.files, stats.dirs, pool_mb, t1.elapsed().as_secs_f64()
        ));
        journals.insert(p.letter, j);
        match pos {
            Some(i) => vols[i] = idx,
            None => vols.push(idx),
        }
        touched += 1;
    }
    if touched == 0 {
        // 一个卷都没更新:不落盘、不报「成功」。全量/增量都算 touched,
        // 所以走到这里只可能是「被跳过 / 被 spec 排除」。
        let why = if skipped.iter().any(|(_, s)| *s == crate::mft::Support::NeedsAdmin) {
            "未获得管理员权限".to_string()
        } else if skipped.is_empty() {
            "指定卷全部被过滤".to_string()
        } else {
            format!("{} 卷不支持 USN", skipped.len())
        };
        anyhow::bail!("本轮未更新任何卷索引(原因:{why}),保留原索引文件");
    }
    say("PROGRESS 0.99 枚举完成,正在写索引文件…".to_string());
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
    for q in ["jpg", "照片", "IMG_2", ".dng", "Screenshot"] {
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

    /// 造一个小卷:C:\Pics\IMG_0001.JPG、C:\Pics\RAW\DSC.NEF、孤儿条目。
    fn tiny_volume() -> VolumeIndex {
        let mut b = VolumeIndexBuilder::new('C');
        // 目录 Pics(frn 0x1000,父 = 卷根 5)
        b.push(0x1000, 5, ATTR_DIRECTORY, &"Pics".encode_utf16().collect::<Vec<u16>>());
        // 文件 IMG_0001.JPG(frn 0x1001,父 Pics)
        b.push(0x1001, 0x1000, 0x20, &"IMG_0001.JPG".encode_utf16().collect::<Vec<u16>>());
        // 子目录 RAW(frn 0x2000,父 Pics)
        b.push(0x2000, 0x1000, ATTR_DIRECTORY, &"RAW".encode_utf16().collect::<Vec<u16>>());
        // DSC.NEF(frn 0x2001,父 RAW)
        b.push(0x2001, 0x2000, 0x20, &"DSC.NEF".encode_utf16().collect::<Vec<u16>>());
        // 孤儿(父 0xdead 不在流中)
        b.push(0x3000, 0xdead, 0x20, &"orphan.log".encode_utf16().collect::<Vec<u16>>());
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

    #[test]
    fn cjk_names_searchable() {
        let mut b = VolumeIndexBuilder::new('D');
        b.push(0x4000, 5, 0x20, &"旅行照片备份".encode_utf16().collect::<Vec<u16>>());
        let v = b.finish();
        let hits = search(std::slice::from_ref(&v), "旅行", 100);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, r"D:\旅行照片备份");
        // 命中不跨字符:单查一个不存在的串应无结果而非误报。
        let hits = search(std::slice::from_ref(&v), " nonexistent ", 100);
        assert!(hits.is_empty());
    }

    #[test]
    fn prefix_hits_rank_first() {
        let mut b = VolumeIndexBuilder::new('C');
        b.push(0x5000, 5, 0x20, &"xabc.txt".encode_utf16().collect::<Vec<u16>>());
        b.push(0x5001, 5, 0x20, &"abc.jpg".encode_utf16().collect::<Vec<u16>>());
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
    fn delta_rename_updates_in_place() {
        let mut v = tiny_volume();
        let c = v.apply_delta(vec![DeltaOp::Upsert(DeltaUpsert {
            frn: 0x1001,
            parent_frn: 0x1000,
            attrs: 0x20,
            name: "IMG_0002.JPG".into(),
        })]);
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
        let _ = v.apply_delta(vec![DeltaOp::Upsert(DeltaUpsert {
            frn: 0x2000,
            parent_frn: 0x1000,
            attrs: ATTR_DIRECTORY,
            name: "RAW2".into(),
        })]);
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
        let c = v.apply_delta(vec![DeltaOp::Upsert(DeltaUpsert {
            frn: 0x2002,
            parent_frn: 0x2000,
            attrs: 0x20,
            name: "NEW.RAW".into(),
        })]);
        assert_eq!(c.added, 1);
        let hits = search(std::slice::from_ref(&v), "new.raw", 10);
        assert_eq!(hits[0].path, r"C:\Pics\RAW\NEW.RAW");
    }

    #[test]
    fn delta_order_delete_then_recreate_same_frn() {
        let mut v = tiny_volume();
        let c = v.apply_delta(vec![
            DeltaOp::Delete(0x1001),
            DeltaOp::Upsert(DeltaUpsert {
                frn: 0x1001,
                parent_frn: 0x1000,
                attrs: 0x20,
                name: "REBORN.JPG".into(),
            }),
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
            DeltaOp::Upsert(DeltaUpsert {
                frn: 0x1001,
                parent_frn: 0x1000,
                attrs: 0x20,
                name: "改名后.JPG".into(),
            }),
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
}
