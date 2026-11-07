//! MFT 文件记录(`$MFT`)解析器与只读偏移诊断(S2 硬门)。
//!
//! 在任何批量 `$MFT` 读取落地(S3)之前,先在本机用**只读**路径把记录内
//! 偏移一次性打表验证(实施第 0 步):
//! - 记录头:`'FILE'`@+0x00、USA@+0x04、SeqNo@+0x10、HardLinks@+0x12、
//!   FirstAttrOffset@+0x14、Flags@+0x16、BaseFRN@+0x20、RecNum@+0x2C;
//! - 属性遍历:`$STANDARD_INFORMATION`(0x10)四个 FILETIME、
//!   `$FILE_NAME`(0x30)父 FRN + 名字(命名空间选择)、
//!   `$DATA`(0x80)驻留 ValidDataLength / 非驻留 RealSize、
//!   `$ATTRIBUTE_LIST`(0x20)检出、`$INDEX_ROOT`(0x90)目录自洽;
//! - fixup(更新序号数组)按扇区尾原位应用;
//! - 直读 `$MFT` vs `FSCTL_GET_NTFS_FILE_RECORD`(0x90068)vs
//!   `std::fs::metadata` 三方交叉一致。
//!
//! 本模块不做任何写操作:卷句柄只开 GENERIC_READ,ioctl 全为查询类。
//! S3 的批量读取路径将复用这里的纯解析函数;本文件不依赖 scan/app。
//! size 与时间零 `stat` 的取数口径:SI 0x10 与 $DATA 0x80
//! 直接从记录取,全程不调 `GetFileInformationByHandle`(三方验证时才用,
//! 作为「磁盘真值」参照,不进解析路径)。

#![allow(dead_code)]

use anyhow::{anyhow, Result};

/// 记录头固定部分尺寸(到 BaseFRN/RecNum 结束)。
pub const REC_HDR: usize = 0x30;
/// 属性终止符。
pub const ATTR_END: u32 = 0xFFFF_FFFF;

pub const TYPE_SI: u32 = 0x10;
pub const TYPE_ATTR_LIST: u32 = 0x20;
pub const TYPE_FILE_NAME: u32 = 0x30;
pub const TYPE_DATA: u32 = 0x80;
pub const TYPE_INDEX_ROOT: u32 = 0x90;

#[derive(Debug, Clone, Copy)]
pub struct RecErr(pub &'static str);

impl std::fmt::Display for RecErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for RecErr {}

/// 解析结果(全部 owned;S3 一次一百万条也只要几十 MB)。
#[derive(Debug, Clone, Default)]
pub struct MftRec {
    pub index: u64,
    /// +0x10:记录序号(FRN 高 16 位),重建/重用后递增。
    pub seq: u16,
    /// +0x12:硬链接数。
    pub hard_links: u16,
    /// +0x14:首个属性偏移(与「+0x20 才是属性起点」的旧口径互斥,以打表结果为准)。
    pub first_attr_off: u16,
    /// +0x16:bit0 = 使用中,bit1 = 目录。
    pub flags: u16,
    pub in_use: bool,
    pub is_dir: bool,
    /// +0x20:基记录 FRN(非 0 = 扩展记录,属性跨记录时出现)。
    pub base_frn: u64,
    /// +0x28:下一属性 ID。
    pub next_attr_id: u16,
    /// +0x2C(XP+):记录自身序号,应等于 `index`。
    pub rec_num: u32,
    /// $STANDARD_INFORMATION 四时间,按内容偏移原样读出:
    /// **[+0x00 created, +0x08 write, +0x10 mft_change, +0x18 access]**。
    ///
    /// **不是** `FILE_ALL_INFORMATION` 的 [created, access, write, change] 次序,也不是
    /// [created, write, access, mft_change]。本机 9 个样本(C: 3 + D: 6):
    /// `si[1]` 与 `metadata().modified()` **逐 tick 全等 9/9**;`si[3]` 与
    /// `metadata().accessed()` 全等 8/9(唯一例外是正在运行的 explorer.exe,盘上 MFT 记录
    /// 滞后内存态 403 秒)。取 mtime 用 **si[1]**,取 atime 用 **si[3]**。
    pub si: Option<[u64; 4]>,
    /// $FILE_NAME 名字(优先 Win32&POSIX > Win32 > POSIX,跳过 DOS 8.3 短名)。
    pub name: Option<String>,
    pub name_ns: u8,
    /// $FILE_NAME 父目录 FRN(48 位 index + 16 位 seq)。
    pub parent_frn: u64,
    /// 未命名 $DATA 的内容尺寸:驻留 = ValidDataLength,非驻留 = RealSize。
    pub data_size: Option<u64>,
    pub data_resident: bool,
    pub has_data: bool,
    /// 有 $ATTRIBUTE_LIST:属性延伸到扩展记录,单记录遍历不完整。
    pub has_attr_list: bool,
    /// 有 $INDEX_ROOT(目录的强自洽信号:flags bit1 的记录几乎必带)。
    pub has_index_root: bool,
    /// `$FILE_NAME` 的 Flags(内容偏移 +0x38)= Win32 `FILE_ATTRIBUTE_*` 位
    /// (0x10 目录 / 0x02 隐藏 / 0x04 系统 / 0x20 存档)。
    /// 记录头的 `flags`(+0x16)只有 in_use/dir 两位,不是这个。
    pub file_attrs: u32,
    /// 属性起点 8 字节对齐违例数(健康卷应为 0)。
    pub attr_misaligned: u32,
}

impl MftRec {
    /// 完整的 file reference number = (seq << 48) | index。
    /// USN 记录与 `$FILE_NAME` 的父引用都是这个 64 位形态。
    pub fn frn(&self) -> u64 {
        ((self.seq as u64) << 48) | (self.index & 0x0000_FFFF_FFFF_FFFF)
    }
}

/// 直读 `$MFT` 全量枚举的统计。
#[derive(Debug, Clone, Default)]
pub struct MftScanStats {
    /// 扫过的记录号总数(= MftValidDataLength / 记录尺寸)。
    pub total: u64,
    /// 回调出去的条目数(在用的基记录,含靠属性表补名的)。
    pub emitted: u64,
    pub files: u64,
    pub dirs: u64,
    /// 已分配但未在用(in_use=0):删除留下的空壳,不进索引。
    pub unused: u64,
    /// 全零填充的未分配记录(合法,不是错误;C: 样本 140 条)。
    pub zero_filled: u64,
    /// 非零却没有 `FILE` 签名 —— 读取定位口径错,**这才是真失败**(健康卷应为 0)。
    pub sig_bad: u64,
    pub fixup_bad: u64,
    pub parse_bad: u64,
    /// `base_frn != 0` 的扩展记录:属性归基记录,不单独入库。
    pub extended: u64,
    /// 名字落在扩展记录里、靠 `$ATTRIBUTE_LIST` 回查补到的条数。
    pub name_via_attrlist: u64,
    /// 缺名且补不到(NTFS 保留记录 12..=15,或属性表里也没有 0x30)。
    pub nameless: u64,
    pub cancelled: bool,
}

/// 直读 `$MFT` 全量枚举一个 NTFS 卷(「批量建库」路径)。
///
/// 为什么不用 `FSCTL_ENUM_USN_DATA`:那条路只有名字/父 FRN/属性,**没有 size 和
/// 时间**;逐文件 `stat` 又是百万次系统调用。直读 `$MFT` 时 `$DATA`(RealSize)与
/// `$STANDARD_INFORMATION`(+0x08 = 内容修改时间)是白送的,偏移口径已由
/// `--mft-diag` 的门禁在 C:/D: 全量验过。
///
/// 回调只吐**在用的基记录**(`base_frn == 0 && in_use`);扩展记录不单独入库。
/// 名字落在扩展记录里的那一小撮(带 `$ATTRIBUTE_LIST`)会当场回查补名。
/// `on_rec` 返回 `false` 视为取消,提前结束(`MftScanStats.cancelled = true`)。
///
/// 需要管理员权限(打开 `\\.\X:` 卷设备)。
pub fn scan_mft(
    letter: char,
    ctrl: Option<&crate::scan::ScanControl>,
    on_rec: impl FnMut(&MftRec) -> bool,
) -> Result<MftScanStats> {
    #[cfg(windows)]
    {
        io::scan_mft_impl(letter, ctrl, on_rec)
    }
    #[cfg(not(windows))]
    {
        let _ = (letter, ctrl, on_rec);
        Err(anyhow!("直读 $MFT 仅支持 Windows"))
    }
}

/// 原位应用更新序号数组(fixup):记录里每个扇区最后 2 字节写入时被
/// USA 序号覆盖,读取后必须用 USA 槽位值还原,否则扇区尾的数据是脏的。
/// `sector_size` 取卷的 BytesPerSector(典型 512)。
pub fn apply_fixup(rec: &mut [u8], sector_size: usize) -> Result<(), RecErr> {
    if rec.len() < REC_HDR {
        return Err(RecErr("记录不足 48 字节"));
    }
    let usa_off = rd_u16(rec, 0x04) as usize;
    let usa_count = rd_u16(rec, 0x06) as usize;
    // count = 自身 USN 1 个 + 每扇区 1 个;与记录长度互为印证。
    if sector_size == 0 || usa_off + usa_count * 2 > rec.len() {
        return Err(RecErr("USA 越界"));
    }
    let expect = rec.len() / sector_size + 1;
    if usa_count != expect {
        return Err(RecErr("USA 数量与扇区数不符"));
    }
    let usn = rd_u16(rec, usa_off);
    for i in 1..usa_count {
        let end = i * sector_size;
        if end < 2 || end > rec.len() {
            return Err(RecErr("扇区边界越界"));
        }
        let cur = rd_u16(rec, end - 2);
        if cur != usn {
            return Err(RecErr("fixup 序号不匹配(记录损坏或偏移口径错)"));
        }
        let fix = rd_u16(rec, usa_off + i * 2);
        rec[end - 2] = fix as u8;
        rec[end - 1] = (fix >> 8) as u8;
    }
    Ok(())
}

/// 解析一条已应用 fixup 的完整记录(`rec.len()` 必须等于记录分配尺寸,
/// 即 BytesPerFileRecordSegment;切片短于分配尺寸会导致属性越界误报)。
pub fn parse_record(rec: &[u8], index: u64) -> Result<MftRec, RecErr> {
    if rec.len() < REC_HDR {
        return Err(RecErr("记录不足 48 字节"));
    }
    if &rec[0..4] != b"FILE" {
        return Err(RecErr("签名不是 'FILE'"));
    }
    let first_attr_off = rd_u16(rec, 0x14);
    let used = rd_u32(rec, 0x18) as usize;
    // 属性活动范围 = [first_attr_off, used_size);used_size 不可信时退回整条。
    let limit = if used >= REC_HDR && used <= rec.len() {
        used
    } else {
        rec.len()
    };
    if (first_attr_off as usize) < REC_HDR || (first_attr_off as usize) >= limit {
        return Err(RecErr("FirstAttrOffset 越界"));
    }
    let flags = rd_u16(rec, 0x16);
    let mut r = MftRec {
        index,
        seq: rd_u16(rec, 0x10),
        hard_links: rd_u16(rec, 0x12),
        first_attr_off,
        flags,
        in_use: flags & 1 != 0,
        is_dir: flags & 2 != 0,
        base_frn: rd_u64(rec, 0x20),
        next_attr_id: rd_u16(rec, 0x28),
        rec_num: rd_u32(rec, 0x2C),
        ..Default::default()
    };
    let mut off = first_attr_off as usize;
    // 终止符只需 4 字节,允许正好落在 used 边界;正式属性头须完整 8 字节。
    while off + 4 <= limit {
        let typ = rd_u32(rec, off);
        if typ == ATTR_END {
            break;
        }
        if off + 8 > limit {
            return Err(RecErr("属性头被截断"));
        }
        let alen = rd_u32(rec, off + 4) as usize;
        if alen < 0x18 || off + alen > limit {
            return Err(RecErr("属性长度异常"));
        }
        let nonres = rec[off + 8] != 0;
        if !off.is_multiple_of(8) {
            r.attr_misaligned += 1;
        }
        match typ {
            TYPE_SI => {
                if !nonres {
                    if let Some(t) = resident_content(rec, off, 0x20) {
                        r.si = Some([
                            rd_u64(rec, t),
                            rd_u64(rec, t + 8),
                            rd_u64(rec, t + 0x10),
                            rd_u64(rec, t + 0x18),
                        ]);
                    }
                }
            }
            TYPE_FILE_NAME => {
                if !nonres {
                    pick_file_name(rec, off, &mut r);
                }
            }
            TYPE_DATA => {
                // 只认未命名 $DATA(命名流是 ADS,不是文件内容)。
                if rec[off + 9] == 0 {
                    r.has_data = true;
                    if nonres {
                        r.data_resident = false;
                        r.data_size = Some(rd_u64(rec, off + 0x30));
                    } else {
                        r.data_resident = true;
                        r.data_size = Some(rd_u32(rec, off + 0x10) as u64);
                    }
                }
            }
            TYPE_ATTR_LIST => r.has_attr_list = true,
            TYPE_INDEX_ROOT => r.has_index_root = true,
            _ => {}
        }
        off += alen;
    }
    Ok(r)
}

/// 驻留属性内容起点(须含 `need` 字节且不越过属性自身),不满足返回 None。
fn resident_content(rec: &[u8], attr_off: usize, need: usize) -> Option<usize> {
    let co = rd_u16(rec, attr_off + 0x14) as usize;
    let cl = rd_u32(rec, attr_off + 0x10) as usize;
    let alen = rd_u32(rec, attr_off + 4) as usize;
    if cl < need || co + cl > alen || attr_off + co + need > rec.len() {
        return None;
    }
    Some(attr_off + co)
}

fn name_rank(ns: u8) -> u8 {
    match ns {
        3 => 3, // Win32 & POSIX,最完整
        1 => 2, // Win32
        0 => 1, // POSIX
        2 => 0, // DOS 8.3 短名,仅作兜底
        _ => 0,
    }
}

/// `$FILE_NAME`(0x30)驻留内容的字段布局(相对内容起点 `t`):
/// 父 FRN@0x00、四个 FILETIME@0x08..0x28、AllocatedSize@0x28、RealSize@0x30、
/// Flags@0x38、ReparseTag@0x3C、**NameLength@0x40、Namespace@0x41、名字@0x42**。
/// 名字长度/命名空间比"固定部分 0x40 字节"再多 2 字节,名字从 0x42 起 ——
/// 打表口径:整卷 25 万条记录里按 0x3C/0x3D 取长度只命中 161 条(那是
/// ReparseTag 的低字节),按 0x40/0x41 取则 100% 命中。
const FN_FIXED: usize = 0x42;
const FN_NAME_LEN: usize = 0x40;
const FN_NAME_NS: usize = 0x41;
const FN_FLAGS: usize = 0x38;

fn pick_file_name(rec: &[u8], attr_off: usize, r: &mut MftRec) {
    let Some(t) = resident_content(rec, attr_off, FN_FIXED) else {
        return;
    };
    let parent = rd_u64(rec, t);
    let name_len = rec[t + FN_NAME_LEN] as usize;
    let ns = rec[t + FN_NAME_NS];
    if name_len == 0 || name_len > 255 {
        return;
    }
    // 名字必须整个落在该属性的内容长度里(不能踩进下一条属性)。
    let cl = rd_u32(rec, attr_off + 0x10) as usize;
    let need = FN_FIXED + name_len * 2;
    if need > cl {
        return;
    }
    let name_end = t + need;
    if name_end > rec.len() {
        return;
    }
    if name_rank(ns) <= name_rank(r.name_ns) && r.name.is_some() {
        return;
    }
    let units: Vec<u16> = rec[t + FN_FIXED..name_end]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    r.name = Some(String::from_utf16_lossy(&units));
    r.name_ns = ns;
    r.parent_frn = parent;
    // 与名字同源同副本:硬链接会有多条 $FILE_NAME,取的是被选中的那一条的属性位。
    r.file_attrs = rd_u32(rec, t + FN_FLAGS);
}

fn rd_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn rd_u64(buf: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(b)
}

/// FILETIME(1601 起 100ns)→ SystemTime。1970 之前或换算溢出返回 None。
pub fn filetime_to_system(t: u64) -> Option<std::time::SystemTime> {
    /// 1601-01-01 → 1970-01-01 相差 11_644_473_600 秒,换成 100ns 单位要 ×1e7。
    /// 上一版这里多写了两个零(1.16e19),导致一切真实 FILETIME 都被判成「1970 前」返回 None。
    const EPOCH_DIFF: u64 = 116_444_736_000_000_000;
    if t < EPOCH_DIFF {
        return None;
    }
    let nanos = (t - EPOCH_DIFF).checked_mul(100)?;
    std::time::SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_nanos(nanos))
}

/// SystemTime → FILETIME(1601 起 100ns)。1970 之前返回 None。
/// [D] 用它把 `metadata()` 的时间换回 tick,好和 `$STANDARD_INFORMATION` 里的原值直接比。
pub fn sys_to_filetime(st: Option<std::time::SystemTime>) -> Option<u64> {
    const EPOCH_DIFF: u64 = 116_444_736_000_000_000;
    let d = st?.duration_since(std::time::SystemTime::UNIX_EPOCH).ok()?;
    Some(d.as_nanos() as u64 / 100 + EPOCH_DIFF)
}

// ---------------- $MFT 数据段(data run / mapping pair) ----------------

/// `$MFT` 的 `$DATA` 一段连续簇。
///
/// `$MFT` 就是个普通文件,它的 `$DATA` 非驻留且**通常碎片化**:只按
/// `MftStartLcn` 线性读,过了第一段就读到无关簇。本机 C: 的 1,006,080 条里
/// 只有 204,924 条带 `'FILE'` 签名(≈200 MB,正是第一段的长度),D: 因为
/// 碎片少所以看着"基本能用"——这就是批量建库前要先解码 run 表、
/// 再逐段 `SetFilePointer`+`ReadFile` 的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRun {
    /// 段起始虚拟簇号(`$MFT` 文件内,单位:簇)。
    pub vcn: u64,
    /// 段起始逻辑簇号(卷内,单位:簇)。
    pub lcn: u64,
    /// 段长(簇)。
    pub clusters: u64,
}

impl DataRun {
    /// 该段覆盖的 `$MFT` 内字节区间 `[start, end)`。
    pub fn byte_range(&self, bytes_per_cluster: u64) -> (u64, u64) {
        let s = self.vcn * bytes_per_cluster;
        (s, s + self.clusters * bytes_per_cluster)
    }
}

/// 解码 mapping pair 链(`LH` 变长格式)。
///
/// 首字节高 4 位 = LCN 偏移字段字节数、低 4 位 = 簇数字段字节数;
/// LCN 偏移**符号扩展**且**相对上一段**(第一段相对 0);偏移字段 0 字节
/// = 稀疏段(只推进 VCN、不落 LCN)。链以 0x00 结束。
pub fn decode_data_runs(rec: &[u8], pairs_off: usize, limit: usize) -> Vec<DataRun> {
    let mut runs = Vec::new();
    let (mut vcn, mut lcn) = (0u64, 0i64);
    let mut p = pairs_off;
    while p < limit {
        let hdr = rec[p];
        if hdr == 0 {
            break;
        }
        let len_sz = (hdr >> 4) as usize;
        let cnt_sz = (hdr & 0x0F) as usize;
        p += 1;
        if cnt_sz == 0 || p + cnt_sz + len_sz > limit {
            break;
        }
        let count = rd_le_uint(rec, p, cnt_sz);
        p += cnt_sz;
        if len_sz == 0 {
            vcn += count; // 稀疏段:无 LCN
            continue;
        }
        lcn += rd_le_int(rec, p, len_sz);
        p += len_sz;
        if lcn < 0 || count == 0 {
            break;
        }
        runs.push(DataRun {
            vcn,
            lcn: lcn as u64,
            clusters: count,
        });
        vcn += count;
    }
    runs
}

/// 记录内属性区的边界:`(首属性起点, 活动上界)`。
/// `used_size@+0x18` 不可信时退回整条记录(与 [`parse_record`] 同口径)。
fn attr_bounds(rec: &[u8]) -> Result<(usize, usize), RecErr> {
    if rec.len() < REC_HDR {
        return Err(RecErr("记录不足 48 字节"));
    }
    if &rec[0..4] != b"FILE" {
        return Err(RecErr("签名不是 'FILE'"));
    }
    let first = rd_u16(rec, 0x14) as usize;
    let used = rd_u32(rec, 0x18) as usize;
    let limit = if used >= REC_HDR && used <= rec.len() {
        used
    } else {
        rec.len()
    };
    if first < REC_HDR || first >= limit {
        return Err(RecErr("FirstAttrOffset 越界"));
    }
    Ok((first, limit))
}

/// 从一条已应用 fixup 的记录里取「未命名、非驻留 `$DATA`」的 run 表。
/// `$MFT` 自身(记录 0)就是这样定位的。
pub fn data_runs_of(rec: &[u8]) -> Result<Vec<DataRun>, RecErr> {
    let (first, limit) = attr_bounds(rec)?;
    let mut off = first;
    while off + 4 <= limit {
        let typ = rd_u32(rec, off);
        if typ == ATTR_END {
            break;
        }
        if off + 8 > limit {
            return Err(RecErr("属性头被截断"));
        }
        let alen = rd_u32(rec, off + 4) as usize;
        if alen < 0x18 || off + alen > limit {
            return Err(RecErr("属性长度异常"));
        }
        // 未命名(+0x09 名字长度 0)且非驻留(+0x08 非 0)的 $DATA 才是文件内容。
        if typ == TYPE_DATA && rec[off + 9] == 0 && rec[off + 8] != 0 {
            let pairs = rd_u16(rec, off + 0x20) as usize;
            if pairs < 0x40 || pairs >= alen {
                return Err(RecErr("MappingPairsOffset 越界"));
            }
            return Ok(decode_data_runs(rec, off + pairs, off + alen));
        }
        off += alen;
    }
    Err(RecErr("未找到非驻留的未命名 $DATA"))
}

/// `$ATTRIBUTE_LIST`(0x20)的一条表项。
///
/// 布局:Type@0x00(4)、Length@0x04(2)、NameLength@0x06(1)、NameOffset@0x07(1)、
/// StartingVcn@0x08(8)、**持有该属性的扩展记录 FRN@0x10(8)**、AttributeId@0x18(2)、
/// 名字@0x1A;最小 0x1A 字节,实际按 8 对齐。链以 Type = 0xFFFFFFFF 结束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttrListEntry {
    pub attr_type: u32,
    pub starting_vcn: u64,
    /// 扩展记录的 FRN(低 48 位是记录号,高 16 位是序列号)。
    pub rec_frn: u64,
    pub attr_id: u16,
}

/// 表项最小长度(Type..AttributeId)。
pub const ATTR_LIST_ENTRY_MIN: usize = 0x1A;

/// 解码 `$ATTRIBUTE_LIST` 的内容(驻留时是属性内容,非驻留时是按 run 读回的簇数据)。
pub fn decode_attr_list(buf: &[u8]) -> Vec<AttrListEntry> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + 8 <= buf.len() {
        let typ = rd_u32(buf, p);
        if typ == ATTR_END || typ == 0 {
            break;
        }
        let len = rd_u16(buf, p + 4) as usize;
        if len < ATTR_LIST_ENTRY_MIN || p + len > buf.len() {
            break;
        }
        out.push(AttrListEntry {
            attr_type: typ,
            starting_vcn: rd_u64(buf, p + 0x08),
            rec_frn: rd_u64(buf, p + 0x10),
            attr_id: rd_u16(buf, p + 0x18),
        });
        p += len;
    }
    out
}

/// `$ATTRIBUTE_LIST` 在记录里的取法:驻留直接给内容切片,非驻留给 run 表
/// (调用方按 run 读簇,再交给 [`decode_attr_list`])。
pub enum AttrListBody<'a> {
    Resident(&'a [u8]),
    NonResident { runs: Vec<DataRun>, size: u64 },
}

/// 定位记录里的 `$ATTRIBUTE_LIST`;没有则 Err。
pub fn attr_list_of(rec: &[u8]) -> Result<AttrListBody<'_>, RecErr> {
    let (first, limit) = attr_bounds(rec)?;
    let mut off = first;
    while off + 4 <= limit {
        let typ = rd_u32(rec, off);
        if typ == ATTR_END {
            break;
        }
        if off + 8 > limit {
            return Err(RecErr("属性头被截断"));
        }
        let alen = rd_u32(rec, off + 4) as usize;
        if alen < 0x18 || off + alen > limit {
            return Err(RecErr("属性长度异常"));
        }
        if typ == TYPE_ATTR_LIST {
            if rec[off + 8] == 0 {
                let co = rd_u16(rec, off + 0x14) as usize;
                let cl = rd_u32(rec, off + 0x10) as usize;
                if co + cl > alen || off + co + cl > rec.len() {
                    return Err(RecErr("$ATTRIBUTE_LIST 驻留内容越界"));
                }
                return Ok(AttrListBody::Resident(&rec[off + co..off + co + cl]));
            }
            let pairs = rd_u16(rec, off + 0x20) as usize;
            if pairs < 0x40 || pairs >= alen {
                return Err(RecErr("MappingPairsOffset 越界"));
            }
            return Ok(AttrListBody::NonResident {
                runs: decode_data_runs(rec, off + pairs, off + alen),
                size: rd_u64(rec, off + 0x30),
            });
        }
        off += alen;
    }
    Err(RecErr("未找到 $ATTRIBUTE_LIST"))
}

fn rd_le_uint(buf: &[u8], off: usize, n: usize) -> u64 {
    let mut v = 0u64;
    for i in (0..n).rev() {
        v = (v << 8) | buf[off + i] as u64;
    }
    v
}

/// 变长小端**有符号**整数(data run 的 LCN 偏移用,须符号扩展)。
/// 减法在 i64 域里做 —— 在 u64 域里 `0xFF - 0x100` 会溢出(debug 下直接 panic)。
fn rd_le_int(buf: &[u8], off: usize, n: usize) -> i64 {
    let v = rd_le_uint(buf, off, n) as i64;
    let bits = n * 8;
    if bits < 64 && v >= (1i64 << (bits - 1)) {
        v - (1i64 << bits)
    } else {
        v
    }
}

// ---------------- 只读诊断(Windows) ----------------

/// `--mft-diag [卷字母表] [报告文件]`:全盘只读验证 MFT 偏移口径。
/// 返回是否过关;任何 FAIL 项都会让 main 以退出码 2 结束。
pub fn cli_mft_diag(spec: Option<&str>, report: Option<&str>) -> Result<()> {
    #[cfg(windows)]
    {
        let passed = io::run_diag(spec, report)?;
        if !passed {
            std::process::exit(2);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (spec, report);
        Err(anyhow!("MFT 诊断仅支持 Windows"))
    }
}

#[cfg(windows)]
mod io {
    use super::*;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, ReadFile, SetFilePointerEx,
        BY_HANDLE_FILE_INFORMATION, FILE_BEGIN, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Ioctl::{
        FSCTL_GET_NTFS_FILE_RECORD, FSCTL_GET_NTFS_VOLUME_DATA, NTFS_FILE_RECORD_INPUT_BUFFER,
        NTFS_VOLUME_DATA_BUFFER,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    /// 卷设备句柄 RAII;只读打开,与 mft.rs 的扫描句柄同参但独立持有。
    struct Vol {
        h: HANDLE,
    }
    impl Drop for Vol {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.h);
            }
        }
    }

    fn open_volume(letter: char) -> Result<Vol> {
        let path = format!("\\\\.\\{letter}:");
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: wide 是 NUL 结尾的 UTF-16 卷设备路径;句柄由 Vol 接管。
        let h = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        }
        .map_err(|e| anyhow!("打开卷 {path} 失败(未提权时典型为拒绝访问): {e}"))?;
        Ok(Vol { h })
    }

    fn volume_data(v: &Vol) -> Result<NTFS_VOLUME_DATA_BUFFER> {
        let mut out = NTFS_VOLUME_DATA_BUFFER::default();
        let mut ret = 0u32;
        // SAFETY: out 是合法出参结构,尺寸与结构一致。
        unsafe {
            DeviceIoControl(
                v.h,
                FSCTL_GET_NTFS_VOLUME_DATA,
                None,
                0,
                Some(&mut out as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
                Some(&mut ret),
                None,
            )
        }
        .map_err(|e| anyhow!("FSCTL_GET_NTFS_VOLUME_DATA 失败: {e}"))?;
        Ok(out)
    }

    /// 顺序读 `$MFT` 的滑动窗口:按 data run 映射,每次整记录对齐地读一大块,
    /// 块不跨 run(run 内簇连续,一次线性 `ReadFile` 就够)。
    struct MftReader {
        h: HANDLE,
        rs: usize,
        bpc: u64,
        total: u64,
        runs: Vec<DataRun>,
        buf: Vec<u8>,
        first: u64,
        have: u64,
    }

    impl MftReader {
        /// 建窗口:先按 `MftStartLcn` 读记录 0(`$MFT` 自身,恒在第一段起点),
        /// 解码它 `$DATA` 的 run 表;之后所有读取都按 run 映射。
        fn new(h: HANDLE, d: &NTFS_VOLUME_DATA_BUFFER, sector: usize) -> Result<Self> {
            let rs = crate::mft::record_size_bytes(
                d.BytesPerFileRecordSegment,
                d.ClustersPerFileRecordSegment,
                d.BytesPerCluster,
            ) as usize;
            let bpc = d.BytesPerCluster as u64;
            if rs == 0 || bpc == 0 {
                return Err(anyhow!("卷参数异常:record={rs} cluster={bpc}"));
            }
            let total = d.MftValidDataLength as u64 / rs as u64;
            let runs = read_mft_runs(h, d, rs, sector)?;
            let chunk = ((8usize << 20) / rs).max(1);
            Ok(MftReader {
                h,
                rs,
                bpc,
                total,
                runs,
                buf: vec![0u8; chunk * rs],
                first: u64::MAX,
                have: 0,
            })
        }

        /// run 表覆盖的字节数(与 `MftValidDataLength` 互为印证)。
        fn covered_bytes(&self) -> u64 {
            self.runs.iter().map(|r| r.clusters * self.bpc).sum()
        }

        fn fill(&mut self, start: u64) -> Result<u64> {
            let buf_recs = self.buf.len() as u64 / self.rs as u64;
            let mut n = (self.total - start).min(buf_recs);
            let byte_off = self.map_offset(start, &mut n)?;
            let want = (n as usize) * self.rs;
            let mut got = 0u32;
            // SAFETY: h 为本结构独占句柄;buf 长度即请求长度,整记录对齐。
            unsafe {
                SetFilePointerEx(self.h, byte_off, None, FILE_BEGIN)?;
                ReadFile(self.h, Some(&mut self.buf[..want]), Some(&mut got), None)?;
            }
            if got as usize != want {
                return Err(anyhow!("$MFT 读块不足:要 {want} 实得 {got}"));
            }
            self.first = start;
            Ok(n)
        }

        /// 记录号 → 卷内绝对字节偏移,并把 `n` 夹到当前 run 末尾
        /// (跨 run 时分块,下次 `fill` 接着读)。
        fn map_offset(&self, start: u64, n: &mut u64) -> Result<i64> {
            let byte_pos = start * self.rs as u64;
            let run = self
                .runs
                .iter()
                .find(|r| {
                    let (s, e) = r.byte_range(self.bpc);
                    byte_pos >= s && byte_pos < e
                })
                .ok_or_else(|| {
                    anyhow!("$MFT 记录 {start}(字节 {byte_pos})不在任何 data run 内")
                })?;
            let (run_start, run_end) = run.byte_range(self.bpc);
            // 本 run 内还能连续读多少条整记录(簇 ≥ 记录时至少 1 条)。
            let avail = ((run_end - byte_pos) / self.rs as u64).max(1);
            *n = (*n).min(avail);
            Ok((run.lcn * self.bpc + (byte_pos - run_start)) as i64)
        }

        /// 返回该记录的切片(已含整条);必要时从磁盘补块。
        fn record(&mut self, idx: u64) -> Result<&mut [u8]> {
            if idx >= self.total {
                return Err(anyhow!("记录 {idx} 超出 MFT 有效长度"));
            }
            if !(idx >= self.first && idx < self.first + self.have) {
                self.have = self.fill(idx)?;
            }
            let at = ((idx - self.first) * self.rs as u64) as usize;
            Ok(&mut self.buf[at..at + self.rs])
        }
    }

    /// 读 `$MFT` 记录 0 并解码它 `$DATA` 的 run 表。拿不到就退回
    /// 「从 `MftStartLcn` 线性读」一段 —— 碎片卷会如实报出签名坏,
    /// 不会被静默当成正确结果。
    fn read_mft_runs(
        h: HANDLE,
        d: &NTFS_VOLUME_DATA_BUFFER,
        rs: usize,
        sector: usize,
    ) -> Result<Vec<DataRun>> {
        let total = d.MftValidDataLength as u64 / rs as u64;
        let bpc = d.BytesPerCluster as u64;
        let linear = |why: String| -> Vec<DataRun> {
            eprintln!("!! $MFT run 表解码失败({why}),退回线性读取口径");
            vec![DataRun {
                vcn: 0,
                lcn: d.MftStartLcn as u64,
                clusters: (total * rs as u64) / bpc + 1,
            }]
        };
        let mut buf = vec![0u8; rs];
        let mut got = 0u32;
        // SAFETY: h 为调用方独占句柄;buf 长度恰为一条记录。
        let io = unsafe {
            SetFilePointerEx(h, d.MftStartLcn * d.BytesPerCluster as i64, None, FILE_BEGIN)
                .and_then(|_| ReadFile(h, Some(&mut buf), Some(&mut got), None))
        };
        if io.is_err() || got as usize != rs {
            return Ok(linear(format!("读记录 0 失败/不足 {got}")));
        }
        if apply_fixup(&mut buf, sector).is_err() {
            return Ok(linear("记录 0 fixup 失败".into()));
        }
        match data_runs_of(&buf) {
            Ok(r) if !r.is_empty() => Ok(r),
            Ok(_) => Ok(linear("run 表为空".into())),
            Err(e) => Ok(linear(e.to_string())),
        }
    }

    /// `FSCTL_GET_NTFS_FILE_RECORD` 单条回查。
    /// 返回与直读记录同构的字节序列。
    ///
    /// 输出缓冲是 `NTFS_FILE_RECORD_OUTPUT_BUFFER`:FileReferenceNumber@0x00(8)、
    /// FileRecordLength@0x08(8)、**FileRecordBlock@0x10** —— 头部 16 字节不是 12,
    /// 少算 4 字节会把整条记录错位,fixup 立刻报"序号不匹配"(踩过)。
    /// 记录起点另用 `'FILE'` 签名在前 64 字节里定位一次,口径不符就照实上报,
    /// 不靠"文档应该如此"过关。
    fn get_file_record(h: HANDLE, frn: u64, rs: usize, out: &mut String) -> Result<Vec<u8>> {
        // 文档口径 NTFS_FILE_RECORD_OUTPUT_BUFFER = { u64 FRN; u64 FileRecordLength; BYTE block[] }
        // → 头 16 字节。本机打表 ret = 12 + 1024,FileRecordLength 只占 4 字节,头是 **12**。
        // 所以不硬编头长:在前 32 字节里扫 'FILE' 签名定位记录起点,把实际值打进报告。
        const OUT_HDR: usize = 12;
        const SCAN: usize = 32;
        let input = NTFS_FILE_RECORD_INPUT_BUFFER {
            FileReferenceNumber: frn as i64,
        };
        let mut buf = vec![0u8; SCAN + rs];
        let mut ret = 0u32;
        // SAFETY: 输入 8 字节结构;输出缓冲留足头部 + 整条记录。
        unsafe {
            DeviceIoControl(
                h,
                FSCTL_GET_NTFS_FILE_RECORD,
                Some(&input as *const _ as *const std::ffi::c_void),
                std::mem::size_of::<NTFS_FILE_RECORD_INPUT_BUFFER>() as u32,
                Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
                buf.len() as u32,
                Some(&mut ret),
                None,
            )
        }
        .map_err(|e| anyhow!("FSCTL_GET_NTFS_FILE_RECORD 失败: {e}"))?;
        if (ret as usize) < OUT_HDR {
            return Err(anyhow!("0x90068 返回 {ret} 字节,不足头部"));
        }
        let len = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
        let sig_at = buf
            .windows(4)
            .take(SCAN)
            .position(|w| w == b"FILE")
            .ok_or_else(|| anyhow!("0x90068 输出前 {SCAN} 字节里找不到 'FILE' 签名"))?;
        out.push_str(&format!(
            "    0x90068 输出:ret={ret} FileRecordLength={len} 头 16 字节={} 'FILE' 签名定位在 +0x{sig_at:x}\n",
            hexdump(&buf[..16])
        ));
        if sig_at != OUT_HDR {
            out.push_str(&format!(
                "    !! 记录起点与上一轮结果(0xc)不符,本轮 0x{sig_at:x} —— 以签名扫描结果为准\n"
            ));
        }
        if len == 0 || len < rs {
            return Err(anyhow!("0x90068 记录长度 {len} 异常(期望 {rs})"));
        }
        if sig_at + rs > buf.len() {
            return Err(anyhow!("0x90068 缓冲不足以放下整条记录"));
        }
        Ok(buf[sig_at..sig_at + rs].to_vec())
    }

    /// 按 run 表读一段非驻留属性的前 `want` 字节(卷内绝对偏移 = lcn × 簇大小)。
    fn read_nonresident(h: HANDLE, runs: &[DataRun], bpc: u64, want: usize) -> Result<Vec<u8>> {
        let mut out = vec![0u8; want];
        let mut filled = 0usize;
        for r in runs {
            if filled >= want {
                break;
            }
            let seg = (r.clusters as usize).saturating_mul(bpc as usize);
            let take = seg.min(want - filled);
            if take == 0 {
                continue;
            }
            let mut got = 0u32;
            // SAFETY: h 为调用方独占句柄;切片长度恰为 take。
            unsafe {
                SetFilePointerEx(h, (r.lcn * bpc) as i64, None, FILE_BEGIN)?;
                ReadFile(h, Some(&mut out[filled..filled + take]), Some(&mut got), None)?;
            }
            if got as usize != take {
                return Err(anyhow!("非驻留属性读块不足:要 {take} 实得 {got}"));
            }
            filled += take;
        }
        out.truncate(filled);
        Ok(out)
    }

    fn hexdump(b: &[u8]) -> String {
        b.iter()
            .map(|x| format!("{x:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// 已知文件 → FRN(`GetFileInformationByHandle` 的 64 位文件 ID)。
    fn frn_of(path: &std::path::Path) -> Result<u64> {
        let wide: Vec<u16> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: wide 是 NUL 结尾 UTF-16 路径;句柄在函数内关闭。
        let h = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        }
        .map_err(|e| anyhow!("打开 {} 失败: {e}", path.display()))?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        let r = unsafe { GetFileInformationByHandle(h, &mut info) };
        unsafe {
            let _ = CloseHandle(h);
        }
        r.map_err(|e| anyhow!("GetFileInformationByHandle({}): {e}", path.display()))?;
        Ok(((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64)
    }

    #[derive(Default)]
    struct Gate {
        items: Vec<(String, bool, String)>,
        infos: Vec<(String, String)>,
    }
    impl Gate {
        fn add(&mut self, name: &str, ok: bool, detail: String) {
            self.items.push((name.into(), ok, detail));
        }
        /// 信息项(不参与过关判定):S3 必须处理但样本有无不影响本卷口径。
        fn add_info(&mut self, name: &str, detail: String) {
            self.infos.push((name.into(), detail));
        }
        fn all_pass(&self) -> bool {
            self.items.iter().all(|(_, ok, _)| *ok)
        }
        fn print(&self, out: &mut String) {
            out.push_str("[E] 过关判定\n");
            for (i, (name, ok, detail)) in self.items.iter().enumerate() {
                out.push_str(&format!(
                    "  {}. [{}] {} —— {}\n",
                    i + 1,
                    if *ok { "PASS" } else { "FAIL" },
                    name,
                    detail
                ));
            }
            for (name, detail) in &self.infos {
                out.push_str(&format!("  · [INFO] {name} —— {detail}\n"));
            }
        }
    }

    pub fn run_diag(spec: Option<&str>, report: Option<&str>) -> Result<bool> {
        let mut out = String::new();
        let mut all_ok = true;
        let letters = pick_letters(spec)?;
        if letters.is_empty() {
            return Err(anyhow!(
                "没有可读的 NTFS 卷 —— 请在管理员终端运行(打开卷设备需要提权)"
            ));
        }
        for letter in letters {
            let ok = diag_volume(letter, &mut out).unwrap_or_else(|e| {
                out.push_str(&format!("!! {letter}:\\ 诊断中止:{e:#}\n"));
                false
            });
            all_ok &= ok;
        }
        print!("{out}");
        if let Some(p) = report {
            std::fs::write(p, &out).map_err(|e| anyhow!("写报告 {} 失败: {e}", p))?;
            println!("报告已落盘 -> {p}");
        }
        Ok(all_ok)
    }

    /// 直读 `$MFT` 只要求「卷是 NTFS 且句柄打得开」,**不要求 USN journal 激活**
    /// —— 数据盘上 journal 常被关,但 MFT 照样能读。所以 NoJournal 也是合法输入,
    /// 只有 NeedsAdmin / VolumeUnreachable / NotNtfs 才该拒。
    fn mft_readable(v: &crate::mft::VolumeProbe) -> bool {
        matches!(
            v.support,
            crate::mft::Support::Usn | crate::mft::Support::NoJournal
        )
    }

    /// 卷字母表:缺省取全部可直读 MFT 的卷;指定则逐字校验存在性。
    fn pick_letters(spec: Option<&str>) -> Result<Vec<char>> {
        let probe = crate::mft::probe();
        match spec {
            None => Ok(probe.iter().filter(|v| mft_readable(v)).map(|v| v.letter).collect()),
            Some(s) => {
                let mut out = Vec::new();
                for c in s.chars() {
                    if !c.is_ascii_alphabetic() {
                        return Err(anyhow!("卷字母表含非字母字符 {c:?}"));
                    }
                    let c = c.to_ascii_uppercase();
                    let p = probe.iter().find(|v| v.letter == c);
                    match p {
                        None => return Err(anyhow!("找不到卷 {c}:")),
                        Some(v) if !mft_readable(v) => {
                            return Err(anyhow!("{c}: {}", v.fault_text()))
                        }
                        _ => out.push(c),
                    }
                }
                Ok(out)
            }
        }
    }

    /// 顺着 $ATTRIBUTE_LIST 回查扩展记录,把 `want` 类型的属性所在的那条扩展记录解析出来。
    ///
    /// 两个用途:
    /// - `[C2]` 正向核验 —— 基记录缺属性时不豁免、不猜测,而是真把扩展记录读出来;
    /// - `scan_mft` 批量建库 —— 名字落在扩展记录里的那一小撮(带属性表的基记录,
    ///   C: 245 条 / D: 392 条)靠它补名,否则这些条目连同其子项的路径都会丢。
    ///
    /// 找不到返回 `Ok(None)`(不是错误):属性表里确实没有该类型。
    fn follow_attr_list(
        reader: &mut MftReader,
        idx: u64,
        sector: usize,
        want: u32,
    ) -> Result<Option<MftRec>> {
        const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
        let mut base = reader.record(idx)?.to_vec();
        // 批量扫描是**原地**在窗口缓冲上做 fixup 的,所以这里拿到的可能已经修过;
        // 再修一次会因序号不匹配而报错 —— 那种失败恰恰说明「已修好」,直接用即可。
        let _ = apply_fixup(&mut base, sector);
        let entries = {
            match attr_list_of(&base)? {
                AttrListBody::Resident(b) => decode_attr_list(b),
                AttrListBody::NonResident { runs, size } => {
                    let buf = read_nonresident(reader.h, &runs, reader.bpc, size as usize)?;
                    decode_attr_list(&buf)
                }
            }
        };
        for e in entries.iter().filter(|e| e.attr_type == want) {
            let eidx = e.rec_frn & FRN_MASK;
            if eidx == idx {
                continue;
            }
            let mut ext = reader.record(eidx)?.to_vec();
            let _ = apply_fixup(&mut ext, sector);
            let r = parse_record(&ext, eidx)?;
            let hit = match want {
                TYPE_FILE_NAME => r.name.is_some(),
                TYPE_INDEX_ROOT => r.has_index_root,
                _ => false,
            };
            if hit && r.base_frn & FRN_MASK == idx {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    /// 基记录缺 `$FILE_NAME` 时,顺着属性表去扩展记录里把名字捞回来,原地补进 `r`。
    /// 返回是否补到。回查本身出错(单条记录损坏)按「补不到」处理,不让整卷扫描失败。
    fn fill_name_via_attr_list(
        reader: &mut MftReader,
        idx: u64,
        sector: usize,
        r: &mut MftRec,
    ) -> bool {
        match follow_attr_list(reader, idx, sector, TYPE_FILE_NAME) {
            Ok(Some(ext)) => {
                r.name = ext.name;
                r.name_ns = ext.name_ns;
                r.parent_frn = ext.parent_frn;
                true
            }
            _ => false,
        }
    }

    /// `scan_mft` 的 Windows 实现:顺序滑窗直读 `$MFT`,逐条 fixup + 解析。
    pub(super) fn scan_mft_impl(
        letter: char,
        ctrl: Option<&crate::scan::ScanControl>,
        mut on_rec: impl FnMut(&MftRec) -> bool,
    ) -> Result<MftScanStats> {
        const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
        let v = open_volume(letter)?;
        let d = volume_data(&v)?;
        let sector = d.BytesPerSector as usize;
        let mut reader = MftReader::new(v.h, &d, sector)?;
        let total = reader.total;
        let mut st = MftScanStats {
            total,
            ..Default::default()
        };
        if let Some(c) = ctrl {
            c.set_total(total as usize);
        }
        // 每 1024 条推进一次进度;取消检查用
        // should_stop(它自带 check_every 节流,不必再套一层计数)。
        let mut pending = 0u64;
        for idx in 0..total {
            if let Some(c) = ctrl {
                if c.should_stop(idx as usize) {
                    st.cancelled = true;
                    break;
                }
            }
            let rec = reader.record(idx)?;
            if &rec[0..4] != b"FILE" {
                // 全零 = 已分配但从未用过的记录(合法);非零却没签名 = 定位口径错。
                if rec.iter().all(|&b| b == 0) {
                    st.zero_filled += 1;
                } else {
                    st.sig_bad += 1;
                }
                continue;
            }
            if apply_fixup(rec, sector).is_err() {
                st.fixup_bad += 1;
                continue;
            }
            let mut r = match parse_record(rec, idx) {
                Ok(r) => r,
                Err(_) => {
                    st.parse_bad += 1;
                    continue;
                }
            };
            if r.base_frn & FRN_MASK != 0 {
                st.extended += 1;
                continue;
            }
            if !r.in_use {
                st.unused += 1;
                continue;
            }
            if r.name.is_none() {
                // NTFS 保留记录 12..=15($Extend=11 之后)在用但天生无名,补不到也不该报警。
                let resolved = r.has_attr_list
                    && !(12..=15).contains(&idx)
                    && fill_name_via_attr_list(&mut reader, idx, sector, &mut r);
                if resolved {
                    st.name_via_attrlist += 1;
                } else {
                    st.nameless += 1;
                    continue;
                }
            }
            if r.is_dir {
                st.dirs += 1;
            } else {
                st.files += 1;
            }
            st.emitted += 1;
            pending += 1;
            if pending >= 1024 {
                if let Some(c) = ctrl {
                    c.advance(pending as usize);
                }
                pending = 0;
            }
            if !on_rec(&r) {
                st.cancelled = true;
                break;
            }
        }
        if let Some(c) = ctrl {
            c.advance(pending as usize);
        }
        // 非零签名坏 = 没按 run 表读(碎片卷读飞了),不能当成「扫完了」交出去。
        // 零填充、扩展记录、缺名都不是错误:诊断程序已逐类核验过它们合法。
        if st.sig_bad > 0 {
            return Err(anyhow!(
                "{letter}: $MFT 有 {} 条记录带非零内容却没有 FILE 签名(定位口径错)",
                st.sig_bad
            ));
        }
        Ok(st)
    }

    fn diag_volume(letter: char, out: &mut String) -> Result<bool> {
        out.push_str(&format!("\n==== MFT 偏移诊断 {letter}:\\(只读)====\n"));
        let v = open_volume(letter)?;
        let d = volume_data(&v)?;
        let rs = crate::mft::record_size_bytes(
            d.BytesPerFileRecordSegment,
            d.ClustersPerFileRecordSegment,
            d.BytesPerCluster,
        ) as usize;
        let sector = d.BytesPerSector as usize;
        out.push_str("[A] 卷参数\n");
        out.push_str(&format!(
            "    sector={sector} cluster={} record={} ClustersPerFileRecordSegment={}\n",
            d.BytesPerCluster, rs, d.ClustersPerFileRecordSegment
        ));
        out.push_str(&format!(
            "    MftStartLcn={} MftValidDataLength={} 记录数≈{}\n",
            d.MftStartLcn,
            d.MftValidDataLength,
            d.MftValidDataLength as u64 / rs as u64
        ));

        let mut g = Gate::default();
        let mut reader = MftReader::new(v.h, &d, sector)?;

        // [A2] data run 表:$MFT 碎片化时必须逐段读,否则过了第一段就是垃圾。
        let covered = reader.covered_bytes();
        let vdl = d.MftValidDataLength as u64;
        let runs = reader.runs.clone();
        out.push_str(&format!(
            "    data run:{} 段,覆盖 {covered} 字节 vs MftValidDataLength {vdl}(差 {})\n",
            runs.len(),
            covered as i64 - vdl as i64
        ));
        for (i, r) in runs.iter().enumerate().take(8) {
            out.push_str(&format!(
                "      [{i}] vcn={} lcn={} clusters={}\n",
                r.vcn, r.lcn, r.clusters
            ));
        }
        if runs.len() > 8 {
            out.push_str(&format!("      …余 {} 段\n", runs.len() - 8));
        }
        // mft::volume_geometry(S3 批量读路径要用的那个封装)与原始 ioctl 字段互证:
        // 两处必须完全一致,否则诊断程序和批量路径会对同一个卷算出不同的记录尺寸。
        let geo = crate::mft::volume_geometry(letter)?;
        let geo_ok = geo.bytes_per_sector == d.BytesPerSector
            && geo.bytes_per_cluster == d.BytesPerCluster
            && geo.bytes_per_file_record as usize == rs
            && geo.clusters_per_file_record == d.ClustersPerFileRecordSegment
            && geo.mft_valid_data_length == vdl
            && geo.mft_start_lcn == d.MftStartLcn as u64
            && rs != 0
            && geo.record_count() == vdl / rs as u64;
        out.push_str(&format!(
            "    mft::volume_geometry 复核:sector={} cluster={} record={} vdl={} lcn={} → 与原始字段{}\n",
            geo.bytes_per_sector,
            geo.bytes_per_cluster,
            geo.bytes_per_file_record,
            geo.mft_valid_data_length,
            geo.mft_start_lcn,
            if geo_ok { "一致" } else { "不一致" }
        ));
        // 结论在这里算,但门禁条目排到最后一条(第 8 条),
        // 免得把已经出过报告的 1..7 编号全部挪位。
        let geo_gate = (geo_ok, format!("{geo:?}"));
        g.add(
            "data run 表解码且覆盖 MftValidDataLength",
            !runs.is_empty() && covered >= vdl,
            format!("{} 段 / 覆盖 {covered} ≥ {vdl}", runs.len()),
        );

        // [B] 头部偏移打表:系统文件 0/3/5 各打一张。
        let mut head_ok = true;
        for idx in [0u64, 3, 5] {
            let raw = reader.record(idx)?.to_vec();
            let mut rec = raw.clone();
            let fx = apply_fixup(&mut rec, sector);
            match fx.and_then(|_| parse_record(&rec, idx)) {
                Ok(r) => {
                    print_head(out, idx, &r);
                    // 自洽:RecNum@+0x2C 应等于 index。
                    if r.rec_num != idx as u32 {
                        out.push_str(&format!(
                            "    !! 记录 {idx} 的 RecNum@+0x2C = {} 不等于 {idx}\n",
                            r.rec_num
                        ));
                        head_ok = false;
                    }
                }
                Err(e) => {
                    out.push_str(&format!("    !! 记录 {idx} 解析失败:{e}\n"));
                    head_ok = false;
                }
            }
            let _ = raw;
        }
        g.add(
            "头部偏移打表(记录 0/3/5,RecNum@+0x2C 自洽)",
            head_ok,
            "见 [B]".into(),
        );

        // [C] 全量抽样统计。
        let (mut sig_bad, mut fx_bad, mut pr_bad) = (0u64, 0u64, 0u64);
        // 签名坏分两类:整条全零 = $MFT 已分配但从未用过的记录(合法);
        // 非零却不带 'FILE' = 读取定位错(碎片/run 表口径错),这才是真失败。
        let (mut sig_zero, mut sig_nonzero) = (0u64, 0u64);
        let (mut in_use, mut ext_recs) = (0u64, 0u64);
        // 目录统计分基记录 / 扩展记录:扩展记录(base_frn != 0)的 Flags 不代表目录身份,
        // 不该进这个不变式。基记录里也可能因为属性表外溢而暂时看不到 $INDEX_ROOT,[C2] 会回查。
        let (mut dirs_base, mut ix_base, mut dirs_ext) = (0u64, 0u64, 0u64);
        let (mut c_si, mut c_fn, mut c_dres, mut c_dnon, mut c_al) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        let mut misaligned = 0u64;
        let mut recnum_mismatch = 0u64;
        // 在用的基记录必然带 $FILE_NAME(L1 的全部价值就在名字上)。缺名的只有两种合法来源:
        //   1. NTFS 保留记录 12..=15($Extend=11 之后),在用但天生无名;
        //   2. 带 $ATTRIBUTE_LIST 的记录 —— 名字落在扩展记录里,必须回查后才算取到。
        // 第 2 类不是豁免:下面 [C2] 会真去解属性表、真去读扩展记录,把名字找出来。
        // 两类之外若还有缺名(no_name_other > 0),说明偏移口径确实错了,门禁失败。
        let (mut no_name, mut no_name_reserved, mut no_name_attrlist, mut no_name_other) =
            (0u64, 0u64, 0u64, 0u64);
        let mut no_name_samples: Vec<String> = Vec::new();
        // 与缺名同构:目录基记录缺 $INDEX_ROOT 也必须能被属性表解释。
        let (mut dir_no_ix_attrlist, mut dir_no_ix_other) = (0u64, 0u64);
        let mut verify_name: Vec<u64> = Vec::new();
        let mut verify_ix: Vec<u64> = Vec::new();
        let mut off_hist: std::collections::BTreeMap<u16, u64> = Default::default();
        let mut bad_samples: Vec<String> = Vec::new();
        let mut al_samples: Vec<String> = Vec::new();
        let mut dir_no_ix: Vec<String> = Vec::new();
        let total = reader.total;
        for idx in 0..total {
            let rec = reader.record(idx)?;
            if &rec[0..4] != b"FILE" {
                sig_bad += 1;
                if rec.iter().all(|&b| b == 0) {
                    sig_zero += 1;
                } else {
                    sig_nonzero += 1;
                    if bad_samples.len() < 5 {
                        bad_samples.push(format!("{idx}:签名(非零)"));
                    }
                }
                continue;
            }
            let fx = apply_fixup(rec, sector);
            if let Err(e) = fx {
                fx_bad += 1;
                if bad_samples.len() < 5 {
                    bad_samples.push(format!("{idx}:fixup({e})"));
                }
                continue;
            }
            let r = match parse_record(rec, idx) {
                Ok(r) => r,
                Err(e) => {
                    pr_bad += 1;
                    if bad_samples.len() < 5 {
                        bad_samples.push(format!("{idx}:parse({e})"));
                    }
                    continue;
                }
            };
            *off_hist.entry(r.first_attr_off).or_default() += 1;
            misaligned += r.attr_misaligned as u64;
            if r.rec_num != idx as u32 {
                recnum_mismatch += 1;
            }
            if r.in_use {
                in_use += 1;
            }
            if r.is_dir {
                if r.base_frn == 0 {
                    dirs_base += 1;
                    if r.has_index_root {
                        ix_base += 1;
                    } else if r.has_attr_list {
                        dir_no_ix_attrlist += 1;
                        if verify_ix.len() < 12 {
                            verify_ix.push(idx);
                        }
                        if dir_no_ix.len() < 5 {
                            dir_no_ix.push(format!(
                                "idx={idx} in_use={} attr_list=1 next_attr_id={}",
                                r.in_use as u8,
                                r.next_attr_id
                            ));
                        }
                    } else {
                        dir_no_ix_other += 1;
                        if dir_no_ix.len() < 5 {
                            dir_no_ix.push(format!(
                                "idx={idx} in_use={} attr_list=0 next_attr_id={} ← 无法解释",
                                r.in_use as u8,
                                r.next_attr_id
                            ));
                        }
                    }
                } else {
                    dirs_ext += 1;
                }
            }
            if r.base_frn != 0 {
                ext_recs += 1;
            }
            if r.si.is_some() {
                c_si += 1;
            }
            if r.name.is_some() {
                c_fn += 1;
            } else if r.base_frn == 0 && r.in_use {
                no_name += 1;
                if (12..=15).contains(&idx) {
                    no_name_reserved += 1;
                } else if r.has_attr_list {
                    no_name_attrlist += 1;
                    if verify_name.len() < 12 {
                        verify_name.push(idx);
                    }
                } else {
                    no_name_other += 1;
                }
                if no_name_samples.len() < 5 {
                    no_name_samples.push(format!(
                        "idx={idx} is_dir={} attr_list={} next_attr_id={}",
                        r.is_dir as u8,
                        r.has_attr_list as u8,
                        r.next_attr_id
                    ));
                }
            }
            match (r.has_data, r.data_resident) {
                (true, true) => c_dres += 1,
                (true, false) => c_dnon += 1,
                _ => {}
            }
            if r.has_attr_list {
                c_al += 1;
                if al_samples.len() < 10 {
                    al_samples.push(format!(
                        "idx={} frn={:012x} base={:x}",
                        idx,
                        idx | ((r.seq as u64) << 48),
                        r.base_frn
                    ));
                }
            }
        }
        let parsed = total - sig_bad - fx_bad - pr_bad;
        out.push_str(&format!(
            "[C] 全量抽样 {total} 条:签名坏 {sig_bad}(全零未分配 {sig_zero} · 非零 {sig_nonzero}) · fixup 坏 {fx_bad} · 解析坏 {pr_bad} · in_use {in_use} · 扩展记录 {ext_recs}\n",
        ));
        out.push_str(&format!(
            "    目录:基记录 {dirs_base}(带 0x90 索引根 {ix_base})· 扩展记录 {dirs_ext}\n",
        ));
        out.push_str(&format!(
            "    属性出现率(解析成功 {parsed} 条):0x10 SI {c_si} · 0x30 名字 {c_fn} · 0x80 驻留 {c_dres} / 非驻留 {c_dnon} · 0x20 属性表 {c_al}\n",
        ));
        let hist: Vec<String> = off_hist
            .iter()
            .map(|(k, n)| format!("0x{k:x}×{n}"))
            .collect();
        out.push_str(&format!("    FirstAttrOffset 直方图:{}\n", hist.join(" ")));
        out.push_str(&format!(
            "    属性 8 字节对齐违例 {misaligned} · RecNum@+0x2C 不符 {recnum_mismatch}\n"
        ));
        if !bad_samples.is_empty() {
            out.push_str(&format!("    坏例(前 5):{}\n", bad_samples.join(" | ")));
        }
        if !no_name_samples.is_empty() {
            out.push_str(&format!(
                "    缺 $FILE_NAME 分类:NTFS 保留记录(12..=15) {no_name_reserved} · 带属性表 {no_name_attrlist} · 无法解释 {no_name_other}(合计 {no_name})\n",
            ));
            out.push_str(&format!(
                "    缺 $FILE_NAME 样例(前 5):\n      {}\n",
                no_name_samples.join("\n      ")
            ));
        }
        if !dir_no_ix.is_empty() {
            out.push_str(&format!(
                "    目录缺 $INDEX_ROOT 分类:带属性表 {dir_no_ix_attrlist} · 无法解释 {dir_no_ix_other}(合计 {})\n",
                dirs_base - ix_base
            ));
            out.push_str(&format!(
                "    目录缺 $INDEX_ROOT 样例(前 5):\n      {}\n",
                dir_no_ix.join("\n      ")
            ));
        }
        if !al_samples.is_empty() {
            out.push_str(&format!(
                "    $ATTRIBUTE_LIST 样例(前 10):\n      {}\n",
                al_samples.join("\n      ")
            ));
        }

        // [C2] 正向验证:上面归到「带属性表」的记录,真去解 $ATTRIBUTE_LIST、真去读它指向的
        // 扩展记录,把缺的 $FILE_NAME / $INDEX_ROOT 找出来。找到 = 偏移口径没错,只是属性跨记录;
        // 找不到 = 口径确实有问题。这里不做豁免,直接核验。
        let mut c2_lines: Vec<String> = Vec::new();
        let (mut name_ok, mut name_bad) = (0usize, 0usize);
        for &idx in &verify_name {
            match follow_attr_list(&mut reader, idx, sector, TYPE_FILE_NAME) {
                Ok(Some(_)) => name_ok += 1,
                Ok(None) => {
                    name_bad += 1;
                    c2_lines.push(format!("idx={idx} 属性表里没有 0x30"));
                }
                Err(e) => {
                    name_bad += 1;
                    c2_lines.push(format!("idx={idx} 回查失败:{e:#}"));
                }
            }
        }
        let (mut ix_ok, mut ix_bad) = (0usize, 0usize);
        for &idx in &verify_ix {
            match follow_attr_list(&mut reader, idx, sector, TYPE_INDEX_ROOT) {
                Ok(Some(_)) => ix_ok += 1,
                Ok(None) => {
                    ix_bad += 1;
                    c2_lines.push(format!("idx={idx} 属性表里没有 0x90"));
                }
                Err(e) => {
                    ix_bad += 1;
                    c2_lines.push(format!("idx={idx} 回查失败:{e:#}"));
                }
            }
        }
        out.push_str(&format!(
            "[C2] $ATTRIBUTE_LIST 正向回查:0x30 命中 {name_ok}/{} · 0x90 命中 {ix_ok}/{}\n",
            verify_name.len(),
            verify_ix.len()
        ));
        if !c2_lines.is_empty() {
            out.push_str(&format!(
                "    未命中(前 8):\n      {}\n",
                c2_lines.iter().take(8).cloned().collect::<Vec<_>>().join("\n      ")
            ));
        }

        g.add(
            "定位正确:签名坏的全是零填充未分配记录,fixup/解析零失败",
            sig_nonzero == 0 && fx_bad == 0 && pr_bad == 0,
            format!("非零签名坏 {sig_nonzero} · fixup {fx_bad} · 解析 {pr_bad} · 零填充 {sig_zero}"),
        );
        g.add(
            "头部偏移全卷自洽(FirstAttrOffset 对齐、RecNum 一致)",
            misaligned == 0 && recnum_mismatch == 0,
            format!("违例 {misaligned} · 不符 {recnum_mismatch}"),
        );
        g.add(
            "$FILE_NAME 偏移口径:缺名者 100% 可被保留记录或属性表解释,且属性表回查真能取到名字",
            no_name_other == 0 && name_bad == 0,
            format!(
                "取到 {c_fn} · 缺名 {no_name}(保留 {no_name_reserved} / 属性表 {no_name_attrlist} / 无法解释 {no_name_other})· 回查命中 {name_ok}"
            ),
        );
        g.add(
            "属性遍历闭环:缺 $INDEX_ROOT 的目录 100% 可被属性表解释,且回查真能取到索引根",
            dir_no_ix_other == 0 && ix_bad == 0,
            format!(
                "{ix_base}/{dirs_base} · 缺 {dir_no_ix_attrlist}(属性表)/ {dir_no_ix_other}(无法解释)· 回查命中 {ix_ok}"
            ),
        );
        g.add_info(
            "$ATTRIBUTE_LIST 检出情况",
            if c_al > 0 {
                format!("{c_al} 条,样例见 [C] —— 批量路径必须处理跨记录属性(扩展记录)")
            } else {
                "本卷 0 条,无样本;S3 批量路径仍须处理该类型".into()
            },
        );

        // [D] 三方交叉验证:已知文件 → FRN → 直读 vs 0x90068 vs metadata。
        // 系统卷用三个已知文件;非系统卷(没有 \Windows)自己从卷根往下找真实文件,
        // 保证 [D] 在任何卷上都有样本,不会退化成 0/0。
        let mut cands: Vec<std::path::PathBuf> = [
            "\\Windows\\explorer.exe",
            "\\Windows\\notepad.exe",
            "\\Windows\\System32\\kernel32.dll",
        ]
        .iter()
        .map(|k| std::path::PathBuf::from(format!("{letter}:{k}")))
        .filter(|p| p.is_file())
        .collect();
        if cands.is_empty() {
            let mut stack = vec![std::path::PathBuf::from(format!("{letter}:\\"))];
            while cands.len() < 8 {
                let Some(d) = stack.pop() else { break };
                let Ok(rd) = std::fs::read_dir(&d) else {
                    continue;
                };
                for e in rd.flatten() {
                    let p = e.path();
                    let Ok(m) = std::fs::metadata(&p) else {
                        continue;
                    };
                    if m.is_file() && m.len() > 0 {
                        cands.push(p);
                        if cands.len() >= 8 {
                            break;
                        }
                    } else if m.is_dir() && stack.len() < 32 {
                        stack.push(p);
                    }
                }
            }
        }
        let (mut cross_seen, mut cross_pass, mut cross_skip) = (0u32, 0u32, 0u32);
        let (mut cross_disc, mut cross_v3, mut cross_v2) = (0u32, 0u32, 0u32);
        for path in &cands {
            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            let frn = match frn_of(path) {
                Ok(f) => f,
                Err(e) => {
                    out.push_str(&format!("    !! {}:{e:#}\n", path.display()));
                    continue;
                }
            };
            let idx = frn & 0x0000_FFFF_FFFF_FFFF;
            out.push_str(&format!(
                "[D] 三方交叉验证:{}\n    FRN={frn:#018x}(index {idx} · seq {:#x})\n",
                path.display(),
                frn >> 48,
            ));
            let raw_direct = reader.record(idx)?.to_vec();
            let raw_ioctl = match get_file_record(v.h, frn, rs, out) {
                Ok(b) => b,
                Err(e) => {
                    out.push_str(&format!("    !! 0x90068 回查失败:{e:#}\n"));
                    continue;
                }
            };
            let mut rec_direct = raw_direct.clone();
            let fx1 = apply_fixup(&mut rec_direct, sector);
            // 0x90068 的输出可能已经替我们做过 fixup(扇区尾不是 USN 而是真数据),
            // 此时再 apply 一次必然报「序号不匹配」。先试一次,失败就退回原始输出。
            // 两条路都不猜:下面用「直读 fixup 后 == ioctl 输出」的字节相等来判定到底是哪种。
            let mut rec_ioctl = raw_ioctl.clone();
            let ioctl_needs_fixup = apply_fixup(&mut rec_ioctl, sector).is_ok();
            if !ioctl_needs_fixup {
                rec_ioctl = raw_ioctl.clone();
            }
            let (pd, pi) = match (
                fx1.and_then(|_| parse_record(&rec_direct, idx)),
                parse_record(&rec_ioctl, idx),
            ) {
                (Ok(a), Ok(b)) => (a, b),
                (e1, e2) => {
                    out.push_str(&format!(
                        "    !! 记录解析失败:direct {e1:?} / ioctl {e2:?}\n"
                    ));
                    continue;
                }
            };
            out.push_str(&format!(
                "    fixup 口径:0x90068 输出{}(直读 fixup 后与 ioctl 输出字节相等 = {})\n",
                if ioctl_needs_fixup {
                    "未做 fixup,需自行还原"
                } else {
                    "已做 fixup,直接用"
                },
                rec_direct == rec_ioctl
            ));
            let byte_eq = rec_direct == rec_ioctl
                || raw_direct == raw_ioctl
                || rec_direct == raw_ioctl
                || raw_direct == rec_ioctl;
            if pd.data_size.is_none() {
                // $DATA 落在扩展记录里(该文件带 $ATTRIBUTE_LIST),基记录单条读不到 size。
                // 这不是失败,是样本不适用于「三方 size 全等」这条断言,换一个继续。
                out.push_str("    -- 该样本 $DATA 跨记录,不计入,换下一个\n");
                cross_skip += 1;
                continue;
            }
            let size_meta = meta.len();
            let size_direct = pd.data_size.unwrap_or(u64::MAX);
            let size_ioctl = pi.data_size.unwrap_or(u64::MAX);
            // SI 四时间的次序不能靠文档,要用 metadata 反证:把 SystemTime 换回 FILETIME tick
            // 直接和 si[i] 比。metadata 也是 100ns 粒度,只允许 as_nanos 截断带来的 ±1 tick。
            let mod_t = sys_to_filetime(meta.modified().ok());
            let acc_t = sys_to_filetime(meta.accessed().ok());
            let near = |a: Option<u64>,
                        b: Option<u64>|
             -> bool { matches!((a, b), (Some(x), Some(y)) if x.abs_diff(y) <= 1) };
            let si_d = pd.si;
            let si_i = pi.si;
            let t0 = si_d.map(|t| t[0]);
            let t1 = si_d.map(|t| t[1]);
            let t2 = si_d.map(|t| t[2]);
            let t3 = si_d.map(|t| t[3]);
            // 断言 1(每个样本都必须成立):si[1] 就是内容修改时间。
            let write_is_t1 = near(t1, mod_t);
            // 断言 2(整卷至少 1 个样本成立):该样本的 si[1]/si[2]/si[3] 两两不同,
            // 于是「si[1]==modified」不是空断言 —— 换任何一个槽位都会失败。
            let discriminating = write_is_t1
                && !near(t2, mod_t)
                && !near(t3, mod_t)
                && t1 != t2
                && t2 != t3
                && t1 != t3;
            // 断言 3(投票):si[2]/si[3] 里谁更像 metadata.accessed()。热文件的盘上 MFT 记录
            // 会滞后于内存态,所以按「谁更近」投票,不要求逐个相等。
            let (vote3, vote2) = match (t2, t3, acc_t) {
                (Some(a), Some(b), Some(c)) => (b.abs_diff(c) < a.abs_diff(c), a.abs_diff(c) < b.abs_diff(c)),
                _ => (false, false),
            };
            out.push_str(&format!(
                "    size:direct={size_direct} ioctl={size_ioctl} metadata={size_meta}
"
            ));
            out.push_str(&format!(
                "    SI tick:+0x00 created={} · +0x08 write={} · +0x10 mft_change={} · +0x18 access={} · direct/ioctl 同序={}
",
                t0.unwrap_or(0),
                t1.unwrap_or(0),
                t2.unwrap_or(0),
                t3.unwrap_or(0),
                si_d == si_i
            ));
            out.push_str(&format!("    metadata tick:modified={mod_t:?} accessed={acc_t:?}
"));
            out.push_str(&format!(
                "    次序判定:si[1]==modified → {} · 该样本能区分次序 → {} · accessed 更像 {}
",
                if write_is_t1 { "成立" } else { "不成立" },
                if discriminating { "是" } else { "否(时间重合,空断言)" },
                if vote3 { "si[3]" } else if vote2 { "si[2]" } else { "无法判定" }
            ));
            out.push_str(&format!(
                "    记录字节:直读 vs 0x90068 = {}
",
                if byte_eq { "一致" } else { "不一致" }
            ));
            out.push_str(&format!(
                "    RecNum@+0x2C:direct={} ioctl={} · FirstAttrOffset direct=0x{:x} ioctl=0x{:x}
",
                pd.rec_num, pi.rec_num, pd.first_attr_off, pi.first_attr_off
            ));
            let ok_one =
                size_direct == size_ioctl && size_ioctl == size_meta && byte_eq && si_d == si_i && write_is_t1;
            cross_seen += 1;
            cross_pass += ok_one as u32;
            cross_disc += discriminating as u32;
            cross_v3 += vote3 as u32;
            cross_v2 += vote2 as u32;
        }
        // 次序结论:[+0x00 created, +0x08 write, +0x10 mft_change, +0x18 access]。
        // 上一版按 FILE_ALL_INFORMATION 的 [created, access, write, change] 标注是错的,
        // 而 mtime 取错槽位会让整个 L2 的时间列全错 —— 所以这一条必须是门禁,不是备注。
        let cross_ok = cross_seen > 0
            && cross_pass == cross_seen
            && cross_disc >= 1
            && cross_v3 > cross_v2;
        g.add(
            "三方交叉验证(size / SI 四时间次序 / 字节一致)",
            cross_ok,
            format!(
                "{cross_pass}/{cross_seen} 个样本全等(跳过跨记录 {cross_skip}·可区分次序 {cross_disc}·accessed 投票 si[3]={cross_v3} vs si[2]={cross_v2}):size 三方一致 + 直读与 0x90068 字节一致 + si[1] 就是 mtime,见 [D]"
            ),
        );

        g.add(
            "mft::volume_geometry 与 0x90064 原始字段互证(记录尺寸折算口径统一)",
            geo_gate.0,
            geo_gate.1,
        );

        g.print(out);
        Ok(g.all_pass())
    }

    fn print_head(out: &mut String, idx: u64, r: &MftRec) {
        out.push_str(&format!(
            "[B] 记录 {idx}:SeqNo={} HardLinks={} FirstAttrOffset=0x{:x} Flags=0x{:04x}(in_use={} dir={}) BaseFRN={:x} NextAttrID={} RecNum={}\n",
            r.seq, r.hard_links, r.first_attr_off, r.flags, r.in_use as u8, r.is_dir as u8, r.base_frn, r.next_attr_id, r.rec_num
        ));
        if let Some(t) = r.si {
            out.push_str(&format!(
                "    SI 四时间(+0x00/+0x08/+0x10/+0x18):created={} write={} mft_change={} access={}\n",
                t[0], t[1], t[2], t[3]
            ));
        }
        if let Some(n) = &r.name {
            out.push_str(&format!(
                "    名字 ns={} parent_frn={:x} name={n:?} data_size={:?}\n",
                r.name_ns, r.parent_frn, r.data_size
            ));
        }
    }
}

// ---------------- 合成字节单测 ----------------

#[cfg(test)]
mod tests {
    use super::*;

    const RS: usize = 1024;
    const SECTOR: usize = 512;

    /// 造一条 1024 字节的合法记录:fixup 序号 0xAAAA,扇区尾被污染,
    /// 属性依次为 SI / FILE_NAME(先 DOS 短名再 Win32)/ 驻留 DATA / 终止符。
    fn synth_record() -> Vec<u8> {
        let mut b = vec![0u8; RS];
        b[0..4].copy_from_slice(b"FILE");
        b[0x04..0x06].copy_from_slice(&0x30u16.to_le_bytes()); // USA 偏移
        b[0x06..0x08].copy_from_slice(&3u16.to_le_bytes()); // 1 + 2 扇区
        b[0x08..0x10].copy_from_slice(&0x1234u64.to_le_bytes()); // LSN(不校验)
        b[0x10..0x12].copy_from_slice(&7u16.to_le_bytes()); // seq
        b[0x12..0x14].copy_from_slice(&2u16.to_le_bytes()); // hard_links
        b[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes()); // 首属性(USA 之后)
        b[0x16..0x18].copy_from_slice(&0x0001u16.to_le_bytes()); // in_use 文件
        b[0x20..0x28].copy_from_slice(&0u64.to_le_bytes()); // BaseFRN
        b[0x28..0x2A].copy_from_slice(&4u16.to_le_bytes()); // NextAttrID
        b[0x2C..0x30].copy_from_slice(&42u32.to_le_bytes()); // RecNum
                                                             // USA:[0xAAAA, 0x1111, 0x2222];扇区尾污染成 0xAAAA。
        b[0x30..0x32].copy_from_slice(&0xAAAAu16.to_le_bytes());
        b[0x32..0x34].copy_from_slice(&0x1111u16.to_le_bytes());
        b[0x34..0x36].copy_from_slice(&0x2222u16.to_le_bytes());
        b[SECTOR - 2..SECTOR].copy_from_slice(&0xAAAAu16.to_le_bytes());
        b[RS - 2..RS].copy_from_slice(&0xAAAAu16.to_le_bytes());

        let mut off = 0x38usize;
        // SI:内容 0x30(四时间),属性长 0x48。
        b[off..off + 4].copy_from_slice(&TYPE_SI.to_le_bytes());
        b[off + 4..off + 8].copy_from_slice(&0x48u32.to_le_bytes());
        b[off + 8] = 0; // resident
        b[off + 0x10..off + 0x14].copy_from_slice(&0x30u32.to_le_bytes());
        b[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        for (i, t) in [11u64, 22, 33, 44].iter().enumerate() {
            b[off + 0x18 + i * 8..off + 0x20 + i * 8].copy_from_slice(&t.to_le_bytes());
        }
        off += 0x48;
        // FILE_NAME ×2:先 DOS 短名 "AB~1"(ns=2),再 Win32 "测试.jpg"(ns=1)。
        // 内容长 = 0x42 + 名字字节数(长度字节@0x40、命名空间@0x41、名字@0x42);
        // 属性长按 8 对齐 —— 名字不许踩进下一属性。
        for (name, ns, parent) in [
            ("AB~1".as_bytes(), 2u8, 5u64),
            ("测试.jpg".as_bytes(), 1u8, 9u64),
        ] {
            let units: Vec<u16> = std::str::from_utf8(name).unwrap().encode_utf16().collect();
            let clen = FN_FIXED + units.len() * 2;
            let alen = (0x18 + clen).div_ceil(8) * 8;
            b[off..off + 4].copy_from_slice(&TYPE_FILE_NAME.to_le_bytes());
            b[off + 4..off + 8].copy_from_slice(&(alen as u32).to_le_bytes());
            b[off + 8] = 0;
            b[off + 0x10..off + 0x14].copy_from_slice(&(clen as u32).to_le_bytes());
            b[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
            let t = off + 0x18;
            b[t..t + 8].copy_from_slice(&parent.to_le_bytes());
            b[t + FN_NAME_LEN] = units.len() as u8;
            b[t + FN_NAME_NS] = ns;
            for (i, u) in units.iter().enumerate() {
                b[t + FN_FIXED + i * 2..t + FN_FIXED + i * 2 + 2]
                    .copy_from_slice(&u.to_le_bytes());
            }
            off += alen;
        }
        // 驻留 DATA:内容 7 字节 "PAYLOAD"(补齐 8)。
        b[off..off + 4].copy_from_slice(&TYPE_DATA.to_le_bytes());
        b[off + 4..off + 8].copy_from_slice(&0x28u32.to_le_bytes());
        b[off + 8] = 0;
        b[off + 0x10..off + 0x14].copy_from_slice(&7u32.to_le_bytes());
        b[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        b[off + 0x18..off + 0x1F].copy_from_slice(b"PAYLOAD");
        off += 0x28;
        b[off..off + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        let used = off as u32 + 4;
        b[0x18..0x1C].copy_from_slice(&used.to_le_bytes());
        b
    }

    fn fixed() -> Vec<u8> {
        let mut b = synth_record();
        apply_fixup(&mut b, SECTOR).unwrap();
        b
    }

    #[test]
    fn fixup_restores_sector_tails() {
        let mut b = synth_record();
        apply_fixup(&mut b, SECTOR).unwrap();
        // 扇区尾被还原为 fixup 值,不再是 USA 序号。
        assert_eq!(rd_u16(&b, SECTOR - 2), 0x1111);
        assert_eq!(rd_u16(&b, RS - 2), 0x2222);
        // 序号不匹配 → 拒绝。
        let mut c = synth_record();
        c[SECTOR - 2..SECTOR].copy_from_slice(&0xBEEFu16.to_le_bytes());
        assert!(apply_fixup(&mut c, SECTOR).is_err());
        // USA 计数与扇区数不符 → 拒绝。
        let mut d = synth_record();
        d[0x06..0x08].copy_from_slice(&5u16.to_le_bytes());
        assert!(apply_fixup(&mut d, SECTOR).is_err());
    }

    #[test]
    fn parse_reads_header_offsets() {
        let r = parse_record(&fixed(), 42).unwrap();
        assert_eq!(r.seq, 7);
        assert_eq!(r.hard_links, 2);
        assert_eq!(r.first_attr_off, 0x38);
        assert!(r.in_use);
        assert!(!r.is_dir);
        assert_eq!(r.base_frn, 0);
        assert_eq!(r.next_attr_id, 4);
        assert_eq!(r.rec_num, 42);
        assert_eq!(r.attr_misaligned, 0);
    }

    #[test]
    fn parse_reads_si_times_and_best_name() {
        let r = parse_record(&fixed(), 42).unwrap();
        assert_eq!(r.si, Some([11, 22, 33, 44]));
        // DOS 短名被跳过,取 Win32 名与它的父目录。
        assert_eq!(r.name.as_deref(), Some("测试.jpg"));
        assert_eq!(r.name_ns, 1);
        assert_eq!(r.parent_frn, 9);
    }

    #[test]
    fn parse_reads_resident_data_size() {
        let r = parse_record(&fixed(), 42).unwrap();
        assert!(r.has_data && r.data_resident);
        assert_eq!(r.data_size, Some(7));
        assert!(!r.has_attr_list && !r.has_index_root);
    }

    #[test]
    fn nonresident_data_real_size_and_flags() {
        let mut b = fixed();
        // 把驻留 DATA 改成非驻留:RealSize 放在属性 +0x30。
        let mut off = 0x38usize;
        while off + 8 <= RS {
            let typ = rd_u32(&b, off);
            if typ == TYPE_DATA {
                break;
            }
            off += rd_u32(&b, off + 4) as usize;
        }
        assert_eq!(rd_u32(&b, off), TYPE_DATA);
        b[off + 8] = 1; // non-resident
        b[off + 0x30..off + 0x38].copy_from_slice(&123_456u64.to_le_bytes());
        let r = parse_record(&b, 42).unwrap();
        assert!(!r.data_resident);
        assert_eq!(r.data_size, Some(123_456));

        // 目录标志 + $INDEX_ROOT 检出。
        let mut d = fixed();
        d[0x16..0x18].copy_from_slice(&0x0003u16.to_le_bytes());
        // 复用 DATA 槽位验证检出(换成 INDEX_ROOT 后不应再报有 DATA)。
        let mut off = 0x38usize;
        while off + 8 <= RS {
            if rd_u32(&d, off) == TYPE_DATA {
                d[off..off + 4].copy_from_slice(&TYPE_INDEX_ROOT.to_le_bytes());
                break;
            }
            off += rd_u32(&d, off + 4) as usize;
        }
        let r = parse_record(&d, 42).unwrap();
        assert!(r.is_dir);
        assert!(r.has_index_root);
        assert!(!r.has_data, "换成 INDEX_ROOT 后不应再报有 DATA");
    }

    #[test]
    fn attr_list_detected_and_end_marker_stops() {
        // SI 槽位换成 ATTRIBUTE_LIST → 检出。
        let mut b = fixed();
        b[0x38..0x3C].copy_from_slice(&TYPE_ATTR_LIST.to_le_bytes());
        let r = parse_record(&b, 42).unwrap();
        assert!(r.has_attr_list);
        assert!(r.si.is_none(), "SI 已被换走");
    }

    #[test]
    fn garbage_after_end_marker_is_ignored() {
        // 找到终止符位置,在其后塞垃圾,解析结果必须与塞前一致。
        let mut b = fixed();
        let end_off = {
            let mut off = 0x38usize;
            loop {
                assert!(rd_u32(&b, off) != ATTR_END, "终止符应在合成记录里");
                let alen = rd_u32(&b, off + 4) as usize;
                if rd_u32(&b, off + alen) == ATTR_END {
                    break off + alen;
                }
                off += alen;
            }
        };
        b[end_off + 4..end_off + 8].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        let r = parse_record(&b, 42).unwrap();
        assert_eq!(r.si, Some([11, 22, 33, 44]), "终止符后的垃圾不应污染 SI");
        assert_eq!(r.data_size, Some(7));
        assert!(!r.has_attr_list, "0x10 槽位未变,不应误报");
    }

    #[test]
    fn rejects_bad_signature_and_short_record() {
        assert!(parse_record(&[0u8; 8], 0).is_err());
        let mut b = fixed();
        b[0..4].copy_from_slice(b"BAAD");
        assert!(parse_record(&b, 0).is_err());
    }

    /// mapping pair 变长解码:多段、负偏移回跳、稀疏段、0x00 终止。
    #[test]
    fn decode_runs_handles_multisegment_and_sparse() {
        // 头字节 = (LCN 偏移字节数 << 4) | 簇数字节数;两个字段都是小端。
        // A: 0x22 → count=0x0100(256 簇), delta=+0x2000      → vcn 0     lcn 0x2000
        // B: 0x22 → count=0x0050(80 簇),  delta=-0x1000(0xF000) → vcn 0x100 lcn 0x1000
        // S: 0x01 → 偏移字段 0 字节 = 稀疏段,count=0x20,只推进 VCN
        // C: 0x22 → count=0x0030(48 簇),  delta=+0x0010      → vcn 0x170 lcn 0x1010
        // 0x00 终止,其后垃圾必须被忽略。
        let buf: Vec<u8> = vec![
            0x22, 0x00, 0x01, 0x00, 0x20, // A
            0x22, 0x50, 0x00, 0x00, 0xF0, // B
            0x01, 0x20, // S(稀疏)
            0x22, 0x30, 0x00, 0x10, 0x00, // C
            0x00, 0xDE, 0xAD, // 终止 + 垃圾
        ];
        let runs = decode_data_runs(&buf, 0, buf.len());
        assert_eq!(
            runs,
            vec![
                DataRun {
                    vcn: 0,
                    lcn: 0x2000,
                    clusters: 0x100
                },
                DataRun {
                    vcn: 0x100,
                    lcn: 0x1000,
                    clusters: 0x50
                },
                DataRun {
                    vcn: 0x170,
                    lcn: 0x1010,
                    clusters: 0x30
                },
            ],
            "稀疏段推进 VCN 但不产 run;终止符后的垃圾不参与"
        );
        // 空链 / 首字节即终止 → 空表。
        assert!(decode_data_runs(&[0x00], 0, 1).is_empty());
        // 簇数字段 0 字节是非法编码,不能死循环。
        assert!(decode_data_runs(&[0x10, 0x00, 0x00], 0, 3).is_empty());
        // byte_range 与 bpc 的关系(S3 的读块映射靠它)。
        let (s, e) = runs[1].byte_range(4096);
        assert_eq!((s, e), (0x100 * 4096, (0x100 + 0x50) * 4096));
    }

    /// 变长有符号整数的符号扩展(data run 的 LCN 偏移靠它回跳)。
    #[test]
    fn variable_length_signed_decode() {
        assert_eq!(rd_le_int(&[0xFF], 0, 1), -1);
        assert_eq!(rd_le_int(&[0x80], 0, 1), -128);
        assert_eq!(rd_le_int(&[0x7F], 0, 1), 127);
        assert_eq!(rd_le_int(&[0x00, 0xF0], 0, 2), -4096);
        assert_eq!(rd_le_int(&[0x01, 0x00], 0, 2), 1);
        assert_eq!(rd_le_uint(&[0x01, 0x02], 0, 2), 0x0201);
    }

    /// 把合成记录里的驻留 `$DATA` 换成非驻留并挂上 run 表,
    /// 验证 `data_runs_of` 的属性定位(未命名 + 非驻留)与越界拒绝。
    #[test]
    fn data_runs_of_finds_unnamed_nonresident_data() {
        let mut b = fixed();
        // 一段:头字节 0x32 → 簇数字段 2 字节、偏移字段 3 字节;
        // count=0x0100(256 簇)、delta=+0x020000 → LCN 0x20000。
        let runs: Vec<u8> = vec![0x32, 0x00, 0x01, 0x00, 0x00, 0x02, 0x00];
        let mut off = 0x38usize;
        loop {
            assert!(off + 8 <= RS);
            if rd_u32(&b, off) == TYPE_DATA {
                break;
            }
            off += rd_u32(&b, off + 4) as usize;
        }
        // 改成非驻留:HighestVcn@+0x18、MappingPairsOffset@+0x20、RealSize@+0x30,
        // run 表放在属性 +0x40,属性长度扩到容纳它。
        let alen = 0x40 + runs.len() + 8;
        b[off + 4..off + 8].copy_from_slice(&(alen as u32).to_le_bytes());
        b[off + 8] = 1; // non-resident
        b[off + 0x18..off + 0x20].copy_from_slice(&0xFFu64.to_le_bytes());
        b[off + 0x20..off + 0x22].copy_from_slice(&0x40u16.to_le_bytes());
        b[off + 0x30..off + 0x38].copy_from_slice(&1_048_576u64.to_le_bytes());
        b[off + 0x40..off + 0x40 + runs.len()].copy_from_slice(&runs);
        b[off + 0x40 + runs.len()] = 0x00;
        // 终止符与 used_size 跟着挪。
        let end = off + alen;
        b[end..end + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        b[0x18..0x1C].copy_from_slice(&((end + 4) as u32).to_le_bytes());

        let got = data_runs_of(&b).unwrap();
        assert_eq!(
            got,
            vec![DataRun {
                vcn: 0,
                lcn: 0x20000,
                clusters: 0x100
            }]
        );
        // 非驻留 RealSize 仍按 +0x30 取(size 的权威来源之一)。
        let r = parse_record(&b, 42).unwrap();
        assert!(!r.data_resident);
        assert_eq!(r.data_size, Some(1_048_576));

        // MappingPairsOffset 指到属性外 → 拒绝,不许读越界。
        b[off + 0x20..off + 0x22].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert!(data_runs_of(&b).is_err());
        // 驻留 $DATA(原合成形状)不是文件内容段 → 找不到 run 表。
        assert!(data_runs_of(&fixed()).is_err());
    }

    /// `$FILE_NAME` 字段偏移的回归:名字长度在内容 +0x40、命名空间 +0x41、
    /// 名字 +0x42。按 +0x3C/+0x3D 取会读到 ReparseTag 的低字节 —— 普通文件
    /// 那是 0,整卷只命中 161/258072 条(打表),所以这里用**非零 ReparseTag**
    /// 把两种口径区分开。
    #[test]
    fn file_name_offsets_ignore_reparse_tag() {
        let mut b = fixed();
        // 找到 Win32 那条 FILE_NAME(内容 +0x3C 是 ReparseTag)。
        let mut off = 0x38usize;
        let mut fn_off = None;
        loop {
            assert!(off + 8 <= RS);
            let typ = rd_u32(&b, off);
            if typ == ATTR_END {
                break;
            }
            if typ == TYPE_FILE_NAME && b[off + 0x18 + FN_NAME_NS] == 1 {
                fn_off = Some(off);
            }
            off += rd_u32(&b, off + 4) as usize;
        }
        let off = fn_off.expect("合成记录里应有 Win32 名字属性");
        // ReparseTag = 0x0000_0003:低字节非零,若按 +0x3C 当名字长度就会读到 3。
        b[off + 0x18 + 0x3C..off + 0x18 + 0x40].copy_from_slice(&3u32.to_le_bytes());
        let r = parse_record(&b, 42).unwrap();
        assert_eq!(r.name.as_deref(), Some("测试.jpg"), "ReparseTag 不许被当成名字长度");
        assert_eq!(r.name_ns, 1);
        assert_eq!(r.parent_frn, 9);

        // 名字长度越出属性内容长度 → 拒绝(不许踩进下一条属性)。
        b[off + 0x18 + FN_NAME_LEN] = 200;
        let r = parse_record(&b, 42).unwrap();
        assert_ne!(r.name.as_deref(), Some("测试.jpg"));
    }

    #[test]
    fn filetime_roundtrip() {
        // 示例量级:2026-11-04T00:00:00Z = unix 1_793_750_400 s,FILETIME 要再加 1601→1970 的
        // 11_644_473_600 s,然后 ×1e7 换成 100ns 单位。上一版测试自己抄了错的常量,所以测不出错。
        let t: u64 = (1_793_750_400u64 + 11_644_473_600) * 10_000_000;
        assert_eq!(t, 134_382_240_000_000_000);
        let s = filetime_to_system(t).unwrap();
        assert_eq!(sys_to_filetime(Some(s)), Some(t), "两个方向必须互逆");
        assert_eq!(
            filetime_to_system(116_444_736_000_000_000),
            Some(std::time::SystemTime::UNIX_EPOCH),
            "1970-01-01 正好落在纪元差上"
        );
        assert!(filetime_to_system(0).is_none());
        assert!(filetime_to_system(116_444_736_000_000_000 - 1).is_none());
        assert!(sys_to_filetime(None).is_none());
    }
}
