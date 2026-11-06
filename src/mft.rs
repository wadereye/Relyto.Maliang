//! NTFS 卷级文件名枚举(USN/MFT 路线)。
//!
//! 与 walkdir 逐目录 `FindNextFile` 不同,`FSCTL_ENUM_USN_DATA` 由内核一次性
//! 顺序吐出全卷 MFT 记录流(名字 + 父目录 FRN + 属性),成本 O(卷内记录数),
//! 与目标子树大小无关 —— 大范围候选枚举明显更快,并顺带拿到「哪些目录有图」。
//!
//! 路线按微软公开的 USN/Journal 文档实现:全卷枚举靠 `FSCTL_ENUM_USN_DATA`
//! 的 ioctl 循环,记录解析靠 `USN_RECORD_V2/V3` 的公开字段布局。本仓库
//! 不集成、也不分发任何第三方索引器代码。落地时避开社区实现里常见的坑:
//! - 缓冲区从 10KB 放大到 1MB(批次少一个数量级);
//! - 64 位下把结果指针截成 32 位、输出字节数按固定小值截断,
//!   这里用切片长度精确推进;
//! - V2/V3 记录字段偏移一致,统一按 `FileNameOffset/FileNameLength` 解析。
//!
//! **权限**:打开 `\\.\C:` 需要管理员(未提权时得到 ERROR_ACCESS_DENIED),
//! 探测结果用 `Support::NeedsAdmin` 表达,由上层决定提权子进程或降级 walkdir。
//! **能力边界(2026-11-04 校准)**:USN 记录本身仍只有名字/父 FRN/属性 +
//! `Usn`(+0x18)/`TimeStamp`(+0x20),没有 size;但 size/mtime 不必逐文件 stat ——
//! 直接读 `$MFT` 就能从 `$DATA`(RealSize@+0x30)和 `$STANDARD_INFORMATION`
//! (+0x08 = write time)拿到,偏移口径已由 rec.rs 的 7 道门禁在 C:/D: 全量验过。
//! 卷几何(记录大小、簇大小)用 `FSCTL_GET_NTFS_VOLUME_DATA` 取,见
//! [`volume_geometry`]。

#![allow(dead_code)]

use anyhow::{anyhow, Result};
use std::path::PathBuf;

/// 卷的 USN 能力等级。
///
/// 分档依据是 Win32 错误码而不是「反正不可用」:提权能不能救、要不要提示用户
/// 去开 journal、是不是干脆走 walkdir 兜底,三种处置完全不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// USN journal 已激活,可直接全量枚举。
    Usn,
    /// journal 未激活(ERROR_JOURNAL_NOT_ACTIVE=1179)或正在删除
    /// (ERROR_JOURNAL_DELETE_IN_PROGRESS=1178)。提权后可以 `FSCTL_CREATE_USN_JOURNAL`
    /// 激活;激活不了就只能走 MFT 全量 + walkdir 兜底。
    NoJournal,
    /// 打开卷句柄或查询 journal 被拒(ERROR_ACCESS_DENIED=5)—— 未提权。
    NeedsAdmin,
    /// 非 NTFS 卷(无 USN journal 概念)。
    NotNtfs,
    /// 卷打不开但不是权限问题:无介质 / 未就绪 / 共享冲突。提权也没用,
    /// 具体码见 [`VolumeProbe::last_error`]。
    VolumeUnreachable,
    /// 非 Windows 平台。
    Unsupported,
}

impl Support {
    /// 提权重试是否有意义(只对 NeedsAdmin 为真)。
    pub fn elevation_helps(self) -> bool {
        matches!(self, Support::NeedsAdmin)
    }
}

/// 卷几何(`FSCTL_GET_NTFS_VOLUME_DATA` = 0x90064 的子集)。
///
/// 直接读 `$MFT` 的路径必须先拿到它:记录尺寸和簇尺寸都来自这里,
/// 硬编 1024/4096 在 512 字节记录或 8KB 簇的卷上会整卷错位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeGeometry {
    pub bytes_per_sector: u32,
    pub bytes_per_cluster: u32,
    /// MFT 单条记录的字节数。内核把小于簇的记录尺寸编成负数
    /// (`BytesPerFileRecordSegment` 为 u32,按 i32 解释 <0 时真实值 = 2^(-v)),
    /// 这里已折算成正数。
    pub bytes_per_file_record: u32,
    /// 每条记录占几簇;0 表示记录小于簇,此时看 `bytes_per_file_record`。
    pub clusters_per_file_record: u32,
    /// `$MFT` 有效数据字节数,除以记录尺寸就是记录条数上限。
    pub mft_valid_data_length: u64,
    /// `$MFT` 第一段的起始 LCN。**不能**据此线性读全卷:`$MFT` 的 `$DATA`
    /// 是非驻留且碎片化的(C: 常见 6 段),必须解 data run 表(见 rec.rs)。
    pub mft_start_lcn: u64,
}

impl VolumeGeometry {
    /// 记录条数上限。记录尺寸为 0(卷数据异常)时返回 0,由调用方判失败。
    pub fn record_count(&self) -> u64 {
        if self.bytes_per_file_record == 0 {
            0
        } else {
            self.mft_valid_data_length / self.bytes_per_file_record as u64
        }
    }
}

/// 取卷几何。失败原因与 [`probe`] 同口径:未提权时是 ERROR_ACCESS_DENIED。
pub fn volume_geometry(letter: char) -> Result<VolumeGeometry> {
    #[cfg(windows)]
    {
        imp::volume_geometry(letter)
    }
    #[cfg(not(windows))]
    {
        let _ = letter;
        Err(anyhow!("卷几何查询仅支持 Windows"))
    }
}

/// 把 `NTFS_VOLUME_DATA_BUFFER` 的两个记录尺寸字段折算成字节数。
///
/// 老 NTFS 会把「小于一个簇」的记录尺寸编成负数(真实值 = 2^(-v) 个簇),
/// 新 Windows 直接给字节数并把簇数字段留 0 —— 本机现状 C:/D: 都是
/// `BytesPerFileRecordSegment=1024` + `ClustersPerFileRecordSegment=0` +
/// `BytesPerCluster=4096`。三种编码都要能落地:记录尺寸一错,整卷 MFT 全错位。
///
/// `pub(crate)`:rec.rs 的只读诊断程序也用同一套折算,免得两处口径漂移
/// (诊断程序原先直接 `BytesPerFileRecordSegment as usize`,遇到负数编码会算出天文数字)。
pub(crate) fn record_size_bytes(bps_raw: u32, cpr_raw: u32, bytes_per_cluster: u32) -> u32 {
    // u32 字段按 i32 解释是否为负。
    let neg = |v: u32| v >= 0x8000_0000;
    // 2^(-v) 个簇换算成字节;指数超过 31 视为卷数据异常。
    let frac = |v: u32| -> u32 {
        let exp = (v as i32).unsigned_abs();
        if exp <= 31 {
            bytes_per_cluster >> exp
        } else {
            0
        }
    };
    if neg(bps_raw) {
        frac(bps_raw)
    } else if bps_raw != 0 {
        bps_raw
    } else if neg(cpr_raw) {
        frac(cpr_raw)
    } else {
        cpr_raw.saturating_mul(bytes_per_cluster)
    }
}

/// USN journal 元数据(FSCTL_QUERY_USN_JOURNAL 结果的子集)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalState {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
}

/// 单卷探测结果。
#[derive(Debug, Clone)]
pub struct VolumeProbe {
    /// 形如 `C:\` 的根路径。
    pub root: PathBuf,
    pub letter: char,
    pub fs: Option<String>,
    pub support: Support,
    pub journal: Option<JournalState>,
    /// 失败时的原始 Win32 错误码。`Support` 是分档结论,这个是不加解释的原始码,
    /// UI 报「卷不可用」时应带上它,否则用户和我们都不知道该不该提权。
    pub last_error: Option<u32>,
}

/// 全量枚举的统计。
#[derive(Debug, Clone, Default)]
pub struct ScanStats {
    /// 内核吐出的记录总数(含系统内部文件)。
    pub records: u64,
    pub files: u64,
    pub dirs: u64,
    /// 用户请求取消而提前结束。
    pub cancelled: bool,
}

/// 一条 USN 记录解析出的全部字段(名字保持 UTF-16 借用,不分配、不转换)。
///
/// `usn` 与 `timestamp` 是原先被丢掉的两个字段:`usn`(+0x18)是增量续读的游标,
/// `timestamp`(+0x20)是**这条变更事件发生的时刻**,与 MFT `$STANDARD_INFORMATION`
/// 里的内容修改时间语义不同 —— 两者都要存,不能互相顶替。
#[derive(Debug, Clone)]
pub struct UsnEntry<'a> {
    pub frn: u64,
    pub parent_frn: u64,
    /// FILE_ATTRIBUTE_* 位(0x10 = 目录)。
    pub attrs: u32,
    /// 触发原因位(增量读取时判定终态语义)。
    pub reason: u32,
    /// 本条记录在日志里的 USN(记录偏移 +0x18)。
    pub usn: u64,
    /// 变更时刻,FILETIME 原始刻度(记录偏移 +0x20)。
    pub timestamp: u64,
    pub name: &'a [u16],
}

/// 卷根目录的 file reference index(MFT 记录 5)。
/// 父 FRN 的低 48 位等于它 → 条目在卷根下。
pub const ROOT_FRN_INDEX: u64 = 5;
/// FRN = (sequence << 48) | index,拼路径时只比 index。
const FRN_INDEX_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

// Win32 错误码。windows crate 的 `ERROR_*` 常量只在 Windows 下存在,而
// `classify_open` / `classify_journal` 是纯函数、必须能脱离真机单测,
// 所以在这里落成裸常量;`err_consts_match_windows_crate` 负责钉住不漂移。
/// ERROR_ACCESS_DENIED:未提权打开卷设备/查询 journal。
pub const ERR_ACCESS_DENIED: u32 = 5;
/// ERROR_JOURNAL_DELETE_IN_PROGRESS:journal 正在被删除。
pub const ERR_JOURNAL_DELETE_IN_PROGRESS: u32 = 1178;
/// ERROR_JOURNAL_NOT_ACTIVE:journal 未激活(数据盘常见,或被组策略关掉)。
pub const ERR_JOURNAL_NOT_ACTIVE: u32 = 1179;
/// ERROR_JOURNAL_ENTRY_DELETED:游标指向的 USN 已被回收,增量续读不可行。
pub const ERR_JOURNAL_ENTRY_DELETED: u32 = 1181;

/// 打开卷失败的分类。`code` 为 `None` 表示错误链里没找到 Win32 码。
fn classify_open(code: Option<u32>) -> Support {
    // 提权只能救 ERROR_ACCESS_DENIED。其余(无介质 15、未就绪 21、共享冲突 32…)
    // 弹 UAC 也打不开,如实报 VolumeUnreachable,别让 UI 走一趟无效的提权。
    if code == Some(ERR_ACCESS_DENIED) {
        Support::NeedsAdmin
    } else {
        Support::VolumeUnreachable
    }
}

/// 查询 journal 失败的分类(缺陷 A7:原先一律吞成 NoJournal)。
fn classify_journal(code: Option<u32>) -> Support {
    match code {
        Some(ERR_ACCESS_DENIED) => Support::NeedsAdmin,
        // 1179 未激活 / 1178 正在删除:提权后可以 FSCTL_CREATE_USN_JOURNAL 激活。
        Some(ERR_JOURNAL_NOT_ACTIVE) | Some(ERR_JOURNAL_DELETE_IN_PROGRESS) => Support::NoJournal,
        // 其余码(含 None):卷能打开但 journal 问不出来,按「当前不可增量枚举」处置。
        // 原码留在 last_error 里,上层不该只凭 NoJournal 就断言是用户关了日志。
        _ => Support::NoJournal,
    }
}

/// 枚举本机可扫卷(固定盘 + 可移动盘,与 scan.rs 的卷分流策略一致:
/// 网络盘/光驱不收录)并探测 USN 能力。非 Windows 返回空。
pub fn probe() -> Vec<VolumeProbe> {
    #[cfg(windows)]
    {
        let mut out = Vec::new();
        for letter in scannable_letters() {
            let root = PathBuf::from(format!("{letter}:\\"));
            let fs = crate::win::volume_filesystem(&root);
            let (support, journal, last_error) = match fs.as_deref() {
                Some("NTFS") => match open_volume(letter) {
                    Err(e) => {
                        let code = win32_code(&e);
                        (classify_open(code), None, code)
                    }
                    Ok(h) => {
                        let j = query_journal(&h);
                        drop(h);
                        match j {
                            Ok(j) => (Support::Usn, Some(j), None),
                            Err(e) => {
                                let code = win32_code(&e);
                                (classify_journal(code), None, code)
                            }
                        }
                    }
                },
                _ => (Support::NotNtfs, None, None),
            };
            out.push(VolumeProbe {
                root,
                letter,
                fs,
                support,
                journal,
                last_error,
            });
        }
        out
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

impl VolumeProbe {
    /// 跳过原因的人话版本。`Support` 只是分档,错误码翻成文案在这里做一次,
    /// 免得每个调用点各写一套(且写错)。可枚举时返回空串。
    pub fn fault_text(&self) -> String {
        match self.support {
            Support::Usn => String::new(),
            Support::NotNtfs => format!(
                "非 NTFS 卷({})",
                self.fs.as_deref().unwrap_or("文件系统未知")
            ),
            Support::NeedsAdmin => "未获得管理员权限,打开卷设备被拒".to_string(),
            Support::VolumeUnreachable => match self.last_error {
                Some(c) => format!("卷打不开(Win32 错误码 {c}),提权也无效"),
                None => "卷打不开(未取到错误码)".to_string(),
            },
            Support::NoJournal => match self.last_error {
                Some(ERR_JOURNAL_NOT_ACTIVE) => "USN 日志未激活".to_string(),
                Some(ERR_JOURNAL_DELETE_IN_PROGRESS) => "USN 日志正在被删除".to_string(),
                Some(c) => format!("USN 日志不可用(Win32 错误码 {c})"),
                None => "USN 日志不可用(未取到错误码)".to_string(),
            },
            Support::Unsupported => "非 Windows 平台".to_string(),
        }
    }
}

/// 全量枚举一个 NTFS 卷的 MFT 记录流。
///
/// `on_entry(&UsnEntry)` 逐条回调;返回 `false`
/// 视为取消,枚举提前结束(`ScanStats.cancelled = true`)。
///
/// **这条路拿不到 size/mtime**(USN 记录里没有),所以它只作为直读 `$MFT`
/// 失败时的降级路径:名字索引还在,元数据列留 0。主路径见 `rec::scan_mft`。
/// `ctrl` 传 `Some` 时按批推进进度(`ScanControl`,total 未知保持 0,
/// UI 应显示「已枚举 N 条」而非百分比)。
pub fn scan_volume(
    letter: char,
    journal: &JournalState,
    ctrl: Option<&crate::scan::ScanControl>,
    mut on_entry: impl FnMut(&UsnEntry<'_>) -> bool,
) -> Result<ScanStats> {
    #[cfg(windows)]
    {
        let h = open_volume(letter)?;
        let mut stats = ScanStats::default();
        let mut med = MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: 0,
            LowUsn: 0,
            HighUsn: journal.next_usn,
        };
        let mut buf = vec![0u8; USN_BUF_SIZE];
        let mut name: Vec<u16> = Vec::with_capacity(1024);
        let med_bytes = std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32;
        loop {
            if let Some(c) = ctrl {
                if c.is_cancelled() {
                    stats.cancelled = true;
                    break;
                }
            }
            let mut ret = 0u32;
            let res = unsafe {
                DeviceIoControl(
                    h.handle,
                    FSCTL_ENUM_USN_DATA,
                    Some(&med as *const _ as *const std::ffi::c_void),
                    med_bytes,
                    Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
                    buf.len() as u32,
                    Some(&mut ret),
                    None,
                )
            };
            match res {
                Ok(()) => {}
                // 枚举到卷尾的正常结束码。
                Err(e) if e.code() == HRESULT::from_win32(ERROR_HANDLE_EOF.0) => break,
                Err(e) => return Err(anyhow!("FSCTL_ENUM_USN_DATA({letter}:) 失败: {e}")),
            }
            let end = ret as usize;
            if end < 8 {
                break;
            }
            // 输出块头 8 字节 = 下一批起始 FRN(内核推进游标)。
            med.StartFileReferenceNumber = rd_u64(&buf, 0);
            let mut off = 8usize;
            let mut n = 0u64;
            while off + 60 <= end {
                let Some(rec) = parse_record(&buf[off..end]) else {
                    break;
                };
                let nb = rec.name_off..(rec.name_off + rec.name_len);
                name.clear();
                name.extend(
                    buf[off + nb.start..off + nb.end]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes(*c)),
                );
                let is_dir = rec.attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
                if is_dir {
                    stats.dirs += 1;
                } else {
                    stats.files += 1;
                }
                n += 1;
                let entry = UsnEntry {
                    frn: rec.frn,
                    parent_frn: rec.parent_frn,
                    attrs: rec.attrs,
                    reason: rec.reason,
                    usn: rec.usn,
                    timestamp: rec.timestamp,
                    name: &name,
                };
                let keep = on_entry(&entry);
                off += rec.len;
                if !keep {
                    stats.cancelled = true;
                    break;
                }
            }
            stats.records += n;
            if let Some(c) = ctrl {
                c.advance(n as usize);
            }
            if stats.cancelled {
                break;
            }
        }
        Ok(stats)
    }
    #[cfg(not(windows))]
    {
        let _ = (letter, journal, ctrl, &mut on_entry);
        Err(anyhow!("USN 枚举仅支持 Windows"))
    }
}

/// 增量读取关心的原因位:只处理影响「存在/名字/位置」的变更,
/// 内容修改(DATA_OVERWRITE 等)不进名字索引。
/// (2026-11-06 修正:FILE_CREATE/FILE_DELETE 此前误写成 0x1/0x2 ——
/// 那两位其实是 DATA_OVERWRITE/DATA_EXTEND,导致新建文件的 CLOSE
/// 记录被 collect_close_op 错折成 Delete,增量新建永远进不了索引。)
pub const USN_REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
pub const USN_REASON_DATA_EXTEND: u32 = 0x0000_0002;
pub const USN_REASON_DATA_TRUNCATION: u32 = 0x0000_0004;
/// 名字索引不关心这三个内容位 —— 列出来只是为了让「不进索引」这件事可测。
pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
/// 关闭位:`ReturnOnlyOnClose = TRUE` 时内核只吐带此位的终态记录
/// (一条记录聚合该文件自打开以来的全部原因)。
pub const USN_REASON_CLOSE: u32 = 0x8000_0000;
const READ_REASON_MASK: u32 = USN_REASON_FILE_CREATE
    | USN_REASON_FILE_DELETE
    | USN_REASON_RENAME_OLD_NAME
    | USN_REASON_RENAME_NEW_NAME
    | USN_REASON_CLOSE;

/// journal 读取统计。
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadStats {
    /// 收到的 CLOSE 终态记录数。
    pub records: u64,
    /// 读完后的下一 USN(写回 checkpoint 的续读点)。
    pub next_usn: i64,
}

/// journal 已失效(条目被回收/日志被重建),checkpoint 不可续读,
/// 该卷需要全量重建。用 downcast 识别,区别于一般 IO 错误。
#[derive(Debug)]
pub struct JournalInvalidated;
impl std::fmt::Display for JournalInvalidated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "USN journal 已失效(checkpoint 不可续读)")
    }
}
impl std::error::Error for JournalInvalidated {}

/// 从 `start_usn` 起增量读取一个卷的 USN journal 终态记录。
///
/// 协议要点(MSDN「Walking a Buffer of Change Journal Records」):
/// 输出缓冲头 8 字节是**下一续读 USN**(与 ENUM_USN_DATA 的 FRN 游标不同),
/// 记录从偏移 8 开始;返回 8 字节即已追平。
/// `on_record(&UsnEntry)` 返回 `false`
/// 提前中止(本批内未处理记录随 checkpoint 前进而丢弃,调用方自行权衡)。
pub fn read_journal(
    letter: char,
    journal_id: u64,
    start_usn: i64,
    mut on_record: impl FnMut(&UsnEntry<'_>) -> bool,
) -> Result<ReadStats> {
    #[cfg(windows)]
    {
        let h = open_volume(letter)?;
        let mut stats = ReadStats { next_usn: start_usn, ..Default::default() };
        let mut buf = vec![0u8; USN_BUF_SIZE];
        let mut name: Vec<u16> = Vec::with_capacity(1024);
        let mut usn = start_usn;
        let data_bytes = std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32;
        loop {
            let data = READ_USN_JOURNAL_DATA_V0 {
                StartUsn: usn,
                ReasonMask: READ_REASON_MASK,
                // 只收 CLOSE 终态:同一文件的多次变更合并为一条,语义最省心。
                ReturnOnlyOnClose: 1,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: journal_id,
            };
            let mut ret = 0u32;
            let res = unsafe {
                DeviceIoControl(
                    h.handle,
                    FSCTL_READ_USN_JOURNAL,
                    Some(&data as *const _ as *const std::ffi::c_void),
                    data_bytes,
                    Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
                    buf.len() as u32,
                    Some(&mut ret),
                    None,
                )
            };
            match res {
                Ok(()) => {}
                Err(e) if e.code() == HRESULT::from_win32(ERROR_JOURNAL_ENTRY_DELETED.0) => {
                    return Err(anyhow::Error::new(JournalInvalidated));
                }
                Err(e) => return Err(anyhow!("FSCTL_READ_USN_JOURNAL({letter}:) 失败: {e}")),
            }
            let end = ret as usize;
            if end < 8 {
                break;
            }
            let next = rd_u64(&buf, 0) as i64;
            stats.next_usn = next;
            // 8 字节 = 已追平;next 不前进 = 防御性兜底(理论不发生)。
            if end == 8 || next <= usn {
                break;
            }
            let mut off = 8usize;
            while off + 60 <= end {
                let Some(rec) = parse_record(&buf[off..end]) else {
                    break;
                };
                let nb = rec.name_off..(rec.name_off + rec.name_len);
                name.clear();
                name.extend(
                    buf[off + nb.start..off + nb.end]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes(*c)),
                );
                stats.records += 1;
                let entry = UsnEntry {
                    frn: rec.frn,
                    parent_frn: rec.parent_frn,
                    attrs: rec.attrs,
                    reason: rec.reason,
                    usn: rec.usn,
                    timestamp: rec.timestamp,
                    name: &name,
                };
                let keep = on_record(&entry);
                off += rec.len;
                if !keep {
                    return Ok(stats);
                }
            }
            usn = next;
        }
        Ok(stats)
    }
    #[cfg(not(windows))]
    {
        let _ = (letter, journal_id, start_usn, &mut on_record);
        Err(anyhow!("USN 增量读取仅支持 Windows"))
    }
}

/// `--usn-dump <卷字母> <输出文件> [回看字节数]`:提权诊断入口。
///
/// 与 [`read_journal`] 的差异:掩码全开(0xFFFF_FFFF)、不限 CLOSE ——
/// 把内核**真的返回了什么**原样写进文件,用来对照服务增量路径,
/// 判定「某文件的记录是内核没回,还是我们没收」。GUI 子系统下 stdout
/// 无处可去,结果只能落盘。
pub fn cli_usn_dump(letter: char, out_path: &str, tail_bytes: u64) -> Result<()> {
    #[cfg(windows)]
    {
        use imp::{open_volume, query_journal, USN_BUF_SIZE};
        let h = open_volume(letter)?;
        let j = query_journal(&h)?;
        let start = (j.next_usn - tail_bytes as i64).max(j.first_usn);
        let mut out = format!(
            "journal_id={:x} first_usn={:x} head={:x} start={:x}\r\n",
            j.journal_id, j.first_usn, j.next_usn, start
        );
        let mut buf = vec![0u8; USN_BUF_SIZE];
        let mut usn = start;
        let data_bytes = std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32;
        loop {
            let data = READ_USN_JOURNAL_DATA_V0 {
                StartUsn: usn,
                ReasonMask: 0xFFFF_FFFF,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: j.journal_id,
            };
            let mut ret = 0u32;
            let res = unsafe {
                DeviceIoControl(
                    h.handle,
                    FSCTL_READ_USN_JOURNAL,
                    Some(&data as *const _ as *const std::ffi::c_void),
                    data_bytes,
                    Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
                    buf.len() as u32,
                    Some(&mut ret),
                    None,
                )
            };
            match res {
                Ok(()) => {}
                Err(e) if e.code() == HRESULT::from_win32(ERROR_JOURNAL_ENTRY_DELETED.0) => {
                    return Err(anyhow::Error::new(JournalInvalidated));
                }
                Err(e) => return Err(anyhow!("FSCTL_READ_USN_JOURNAL({letter}:) 失败: {e}")),
            }
            let end = ret as usize;
            if end < 8 {
                break;
            }
            let next = rd_u64(&buf, 0) as i64;
            if end == 8 || next <= usn {
                break;
            }
            let mut off = 8usize;
            while off + 60 <= end {
                let Some(rec) = parse_record(&buf[off..end]) else {
                    break;
                };
                let nb = rec.name_off..(rec.name_off + rec.name_len);
                let name: Vec<u16> = buf[off + nb.start..off + nb.end]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c))
                    .collect();
                out.push_str(&format!(
                    "usn={:x} reason={:#010x} frn={} pfrn={} {}\r\n",
                    rec.usn,
                    rec.reason,
                    rec.frn,
                    rec.parent_frn,
                    String::from_utf16_lossy(&name)
                ));
                off += rec.len;
            }
            usn = next;
            // 防御上限:8 MB 输出足够任何诊断场景。
            if out.len() > 8_000_000 {
                break;
            }
        }
        std::fs::write(out_path, out)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (letter, out_path, tail_bytes);
        Err(anyhow!("USN 诊断仅支持 Windows"))
    }
}

/// 从 anyhow 错误链里取出 Win32 错误码。
///
/// windows crate 把 `WIN32_ERROR` 包成 HRESULT 时固定落在 facility 7:
/// `0x8007_0000 | code`。非 Win32 facility(如 NTSTATUS 直传的 `0xC000_xxxx`)
/// 返回 `None` —— 那种码不该被当成 Win32 码解释。
pub fn win32_code(e: &anyhow::Error) -> Option<u32> {
    #[cfg(windows)]
    {
        e.chain().find_map(|c| {
            let we = c.downcast_ref::<windows::core::Error>()?;
            let h = we.code().0 as u32;
            (h & 0xFFFF_0000 == 0x8007_0000).then_some(h & 0xFFFF)
        })
    }
    #[cfg(not(windows))]
    {
        let _ = e;
        None
    }
}

/// 打开卷设备句柄失败时的错误分类(UI 据此提示提权)。
pub fn is_access_denied(e: &anyhow::Error) -> bool {
    win32_code(e) == Some(ERR_ACCESS_DENIED)
}

// ---------------- 内部实现 ----------------

/// 单条 USN 记录解析结果(全部借用偏移,不做 UTF-16 转换)。
#[derive(Debug, Clone, Copy)]
struct RawRec {
    /// 记录总长(含名字与填充),即下一条的步进。
    len: usize,
    frn: u64,
    parent_frn: u64,
    attrs: u32,
    /// 触发原因位(增量读取时判定终态语义)。
    reason: u32,
    /// 本条记录的 USN(+0x18),增量续读游标。
    usn: u64,
    /// 变更时刻 FILETIME(+0x20)。
    timestamp: u64,
    name_off: usize,
    name_len: usize,
}

/// 解析 `USN_RECORD_V2/V3`。两者字段偏移一致,仅版本号与对齐不同,
/// 按 `FileNameOffset/FileNameLength` 通用读取。缓冲不足或长度异常返回 None。
///
/// 偏移表(字节):RecordLength@0(u16)、FRN@8、父 FRN@16、Usn@24、TimeStamp@32、
/// Reason@40、FileAttributes@52、FileNameLength@56、FileNameOffset@58、名字@60 起。
fn parse_record(buf: &[u8]) -> Option<RawRec> {
    if buf.len() < 60 {
        return None;
    }
    let len = rd_u16(buf, 0) as usize;
    // 头 60 字节 + 至少 2 字节名字;len 越界视为流损坏。
    if len < 62 || len > buf.len() {
        return None;
    }
    Some(RawRec {
        len,
        frn: rd_u64(buf, 8),
        parent_frn: rd_u64(buf, 16),
        attrs: rd_u32(buf, 52),
        reason: rd_u32(buf, 40),
        usn: rd_u64(buf, 24),
        timestamp: rd_u64(buf, 32),
        name_off: rd_u16(buf, 58) as usize,
        name_len: rd_u16(buf, 56) as usize,
    })
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

#[cfg(windows)]
mod imp {
    use super::*;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetLogicalDrives, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, USN_JOURNAL_DATA_V0};

    /// 单次 ioctl 输出缓冲 1MB:比参考实现的 10KB 少两个数量级的系统调用,
    /// 又不会在内存紧张的机器上碰 ERROR_INVALID_USER_BUFFER。
    pub(super) const USN_BUF_SIZE: usize = 1 << 20;
    pub(super) const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

    /// 卷设备句柄 RAII(Drop 时 CloseHandle,错误路径也不泄漏)。
    pub(super) struct VolumeHandle {
        pub handle: HANDLE,
    }
    impl Drop for VolumeHandle {
        fn drop(&mut self) {
            // SAFETY: handle 是本结构独占的合法句柄,关闭一次。
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }

    pub(super) fn scannable_letters() -> Vec<char> {
        let mask = unsafe { GetLogicalDrives() };
        let mut out = Vec::new();
        if mask == 0 {
            return out;
        }
        for i in 0..26u32 {
            if mask & (1u32 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = PathBuf::from(format!("{letter}:\\"));
            if crate::win::drive_type(&root).map(|t| t.scannable()).unwrap_or(false) {
                out.push(letter);
            }
        }
        out
    }

    pub(super) fn open_volume(letter: char) -> Result<VolumeHandle> {
        use windows::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
        let path = format!("\\\\.\\{letter}:");
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: wide 是 NUL 结尾的 UTF-16 卷设备路径;句柄由 VolumeHandle 接管。
        let h = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                windows::Win32::Foundation::GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        }
        // 必须用 Error::new(...).context(...) 而不是 anyhow!("...: {e}"):
        // 后者把 windows::core::Error 格式化成字符串、丢掉错误链,
        // 上层就拿不到 Win32 码,只能把「未提权」误报成「卷打不开」(缺陷 A7 的另一半)。
        .map_err(|e| anyhow::Error::new(e).context(format!("打开卷 {path} 失败")))?;
        Ok(VolumeHandle { handle: h })
    }

    pub(super) fn query_journal(h: &VolumeHandle) -> Result<JournalState> {
        let mut jq = USN_JOURNAL_DATA_V0::default();
        let mut ret = 0u32;
        // SAFETY: jq 是合法出参结构,缓冲大小与其大小一致。
        unsafe {
            DeviceIoControl(
                h.handle,
                FSCTL_QUERY_USN_JOURNAL,
                None,
                0,
                Some(&mut jq as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
                Some(&mut ret),
                None,
            )
        }
        .map_err(|e| anyhow::Error::new(e).context("FSCTL_QUERY_USN_JOURNAL 失败"))?;
        Ok(JournalState {
            journal_id: jq.UsnJournalID,
            first_usn: jq.FirstUsn,
            next_usn: jq.NextUsn,
        })
    }

    /// 卷几何。打开卷这一步与 USN 枚举同权限(未提权 → ERROR_ACCESS_DENIED),
    /// 所以调用方拿到失败时可以复用 [`classify_open`] 的结论。
    pub(super) fn volume_geometry(letter: char) -> Result<VolumeGeometry> {
        use windows::Win32::System::Ioctl::{
            FSCTL_GET_NTFS_VOLUME_DATA, NTFS_VOLUME_DATA_BUFFER,
        };
        let v = open_volume(letter)?;
        let mut d = NTFS_VOLUME_DATA_BUFFER::default();
        let mut ret = 0u32;
        // SAFETY: d 是合法出参结构,缓冲大小与结构一致;v.handle 由 VolumeHandle 持有。
        unsafe {
            DeviceIoControl(
                v.handle,
                FSCTL_GET_NTFS_VOLUME_DATA,
                None,
                0,
                Some(&mut d as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
                Some(&mut ret),
                None,
            )
        }
        .map_err(|e| {
            anyhow::Error::new(e).context(format!("{letter}: FSCTL_GET_NTFS_VOLUME_DATA 失败"))
        })?;
        let bytes_per_cluster = d.BytesPerCluster;
        Ok(VolumeGeometry {
            bytes_per_sector: d.BytesPerSector,
            bytes_per_cluster,
            bytes_per_file_record: record_size_bytes(
                d.BytesPerFileRecordSegment,
                d.ClustersPerFileRecordSegment,
                bytes_per_cluster,
            ),
            clusters_per_file_record: d.ClustersPerFileRecordSegment,
            mft_valid_data_length: d.MftValidDataLength.max(0) as u64,
            mft_start_lcn: d.MftStartLcn.max(0) as u64,
        })
    }
}

#[cfg(windows)]
use imp::{
    open_volume, query_journal, scannable_letters, FILE_ATTRIBUTE_DIRECTORY, USN_BUF_SIZE,
};

#[cfg(windows)]
use windows::core::HRESULT;
#[cfg(windows)]
use windows::Win32::Foundation::{ERROR_HANDLE_EOF, ERROR_JOURNAL_ENTRY_DELETED};
#[cfg(windows)]
use windows::Win32::System::IO::DeviceIoControl;
#[cfg(windows)]
use windows::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0, READ_USN_JOURNAL_DATA_V0,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一条 64 字节的 USN_RECORD_V2:"AB"(4 字节名字,补齐到 64)。
    fn synth_record_v2() -> Vec<u8> {
        let mut b = vec![0u8; 64];
        b[0..2].copy_from_slice(&64u16.to_le_bytes()); // RecordLength
        b[2..4].copy_from_slice(&2u16.to_le_bytes()); // MajorVersion
        b[4..6].copy_from_slice(&0u16.to_le_bytes()); // MinorVersion
        b[8..16].copy_from_slice(&0x0000_0000_0010_0234u64.to_le_bytes()); // FRN
        b[16..24].copy_from_slice(&0x0000_0000_0000_0005u64.to_le_bytes()); // 父 = 卷根
        // 134382240000000000 = 2026-11-04T00:00:00Z 的 FILETIME 刻度(真实量级)。
        b[24..32].copy_from_slice(&0x01DB_5F48_9A34_0000u64.to_le_bytes()); // Usn
        b[32..40].copy_from_slice(&134_382_240_000_000_000u64.to_le_bytes()); // TimeStamp
        b[40..44].copy_from_slice(&0x8000_2000u32.to_le_bytes()); // CLOSE|RENAME_NEW_NAME
        b[52..56].copy_from_slice(&0x20u32.to_le_bytes()); // ARCHIVE
        b[56..58].copy_from_slice(&4u16.to_le_bytes()); // FileNameLength
        b[58..60].copy_from_slice(&60u16.to_le_bytes()); // FileNameOffset
        b[60..64].copy_from_slice(&"AB"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect::<Vec<u8>>());
        b
    }

    #[test]
    fn parse_record_reads_all_fields() {
        let buf = synth_record_v2();
        let r = parse_record(&buf).expect("应能解析");
        assert_eq!(r.len, 64);
        assert_eq!(r.frn, 0x0000_0000_0010_0234);
        assert_eq!(r.parent_frn & FRN_INDEX_MASK, ROOT_FRN_INDEX);
        assert_eq!(r.attrs, 0x20);
        assert_eq!(r.reason, 0x8000_2000);
        // Usn/TimeStamp 曾被整段丢掉:游标丢了就做不了追赶,变更时刻丢了就分不清
        // 「内容修改时间」和「这条 USN 事件的时间」。两个都必须原样读出来。
        assert_eq!(r.usn, 0x01DB_5F48_9A34_0000);
        assert_eq!(r.timestamp, 134_382_240_000_000_000);
        assert_eq!(
            crate::rec::filetime_to_system(r.timestamp).map(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap().as_secs()),
            Some(1_793_750_400),
            "TimeStamp 必须能换回 2026-11-04T00:00:00Z"
        );
        assert_eq!(r.name_off, 60);
        assert_eq!(r.name_len, 4);
    }

    #[test]
    fn parse_record_rejects_short_and_overrun() {
        assert!(parse_record(&[0u8; 59]).is_none(), "不足头 60 字节应拒绝");
        let mut buf = synth_record_v2();
        buf[0..2].copy_from_slice(&128u16.to_le_bytes()); // len 越界
        assert!(parse_record(&buf).is_none());
        let mut buf2 = synth_record_v2();
        buf2[0..2].copy_from_slice(&0u16.to_le_bytes()); // len=0
        assert!(parse_record(&buf2).is_none());
    }

    #[test]
    fn utf16_name_roundtrip() {
        let buf = synth_record_v2();
        let r = parse_record(&buf).unwrap();
        let name: String = String::from_utf16_lossy(
            &buf[r.name_off..r.name_off + r.name_len]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect::<Vec<u16>>(),
        );
        assert_eq!(name, "AB");
    }

    #[cfg(windows)]
    #[test]
    fn probe_lists_ntfs_system_volume() {
        let vols = probe();
        let c = vols.iter().find(|v| v.letter == 'C').expect("应有 C:");
        assert_eq!(c.fs.as_deref(), Some("NTFS"));
        // 未提权的 CI/开发机上 NeedsAdmin,提权终端里是 Usn —— 两者都合法。
        assert!(matches!(c.support, Support::Usn | Support::NeedsAdmin));
        // 分档结论必须自带原始错误码:失败时既要有码也要有人话,否则 UI 只能瞎猜该不该提权。
        if c.support != Support::Usn {
            assert_eq!(
                c.last_error,
                Some(ERR_ACCESS_DENIED),
                "C: 未提权时的失败码应是 5,实际 {:?}",
                c.last_error
            );
            assert!(c.fault_text().contains("管理员"), "{}", c.fault_text());
        } else {
            assert!(c.last_error.is_none());
            assert!(c.fault_text().is_empty());
        }
    }

    #[cfg(windows)]
    #[test]
    fn volume_geometry_matches_diag_measurements() {
        // rec.rs 的 --mft-diag 已在 C:/D: 打表验证过:sector=512 cluster=4096 record=1024、
        // ClustersPerFileRecordSegment=0。这里用同一个 ioctl 复核,防止两处口径漂移。
        if probe()
            .iter()
            .find(|v| v.letter == 'C')
            .map(|v| v.support != Support::Usn)
            .unwrap_or(true)
        {
            return; // 未提权,跳过(不是失败)
        }
        let g = volume_geometry('C').expect("Usn 卷必然能取到几何");
        let pow2 = |v: u32| v != 0 && v.is_power_of_two();
        assert!(pow2(g.bytes_per_sector) && g.bytes_per_sector >= 512, "{g:?}");
        assert!(pow2(g.bytes_per_cluster) && g.bytes_per_cluster >= g.bytes_per_sector, "{g:?}");
        assert!(pow2(g.bytes_per_file_record) && g.bytes_per_file_record >= 512, "{g:?}");
        assert!(g.mft_valid_data_length >= 1 << 20, "{g:?}");
        assert!(g.mft_start_lcn > 0, "{g:?}");
        // 记录数上限必须与 diag 报的量级一致(C: ≈ 100 万条),且与折算后的记录尺寸自洽。
        let n = g.record_count();
        assert!((10_000..100_000_000).contains(&n), "记录数 {n} 不合理");
        assert_eq!(
            n,
            g.mft_valid_data_length / g.bytes_per_file_record as u64,
            "record_count 必须就是折算后尺寸除出来的值"
        );
    }

    #[test]
    fn classify_open_splits_access_denied_from_unreachable() {
        assert_eq!(classify_open(Some(ERR_ACCESS_DENIED)), Support::NeedsAdmin);
        // 无介质 15 / 未就绪 21 / 共享冲突 32:提权救不回来,不许再报 NeedsAdmin。
        for code in [Some(15), Some(21), Some(32), None] {
            assert_eq!(
                classify_open(code),
                Support::VolumeUnreachable,
                "code={code:?}"
            );
        }
        assert!(Support::NeedsAdmin.elevation_helps());
        assert!(!Support::VolumeUnreachable.elevation_helps());
    }

    #[test]
    fn classify_journal_splits_codes() {
        assert_eq!(
            classify_journal(Some(ERR_JOURNAL_NOT_ACTIVE)),
            Support::NoJournal
        );
        assert_eq!(
            classify_journal(Some(ERR_JOURNAL_DELETE_IN_PROGRESS)),
            Support::NoJournal
        );
        assert_eq!(
            classify_journal(Some(ERR_ACCESS_DENIED)),
            Support::NeedsAdmin,
            "查 journal 被拒也是提权问题,不能吞成 NoJournal"
        );
        assert_eq!(classify_journal(Some(ERR_JOURNAL_ENTRY_DELETED)), Support::NoJournal);
        assert_eq!(classify_journal(None), Support::NoJournal);
    }

    #[test]
    fn record_size_bytes_handles_all_three_encodings() {
        // 本机口径:直接给字节数,簇数字段为 0。
        assert_eq!(record_size_bytes(1024, 0, 4096), 1024);
        // 老编码:记录尺寸 = 2^(-2) 簇 = 4096/4 = 1024(u32 里 -2 = 0xFFFFFFFE)。
        assert_eq!(record_size_bytes((-2i32) as u32, 0, 4096), 1024);
        // 字节字段为 0 时退到簇字段:正数按「几簇」算。
        assert_eq!(record_size_bytes(0, 2, 4096), 8192);
        // 簇字段也是负数编码。
        assert_eq!(record_size_bytes(0, (-1i32) as u32, 4096), 2048);
        // 指数越界不许 panic,也不许给出荒谬值。
        assert_eq!(record_size_bytes((-40i32) as u32, 0, 4096), 0);
        // 全 0 视为卷数据异常 → 0,由 record_count() 兜成 0。
        assert_eq!(record_size_bytes(0, 0, 4096), 0);
    }

    #[test]
    fn geometry_record_count_guards_zero_record_size() {
        let g = VolumeGeometry {
            bytes_per_sector: 512,
            bytes_per_cluster: 4096,
            bytes_per_file_record: 0,
            clusters_per_file_record: 0,
            mft_valid_data_length: 1 << 30,
            mft_start_lcn: 786432,
        };
        assert_eq!(g.record_count(), 0, "记录尺寸为 0 时不许除零");
    }

    #[test]
    fn fault_text_names_the_journal_code() {
        let mk = |support: Support, code: Option<u32>, fs: Option<&str>| VolumeProbe {
            root: PathBuf::from("D:\\"),
            letter: 'D',
            fs: fs.map(String::from),
            support,
            journal: None,
            last_error: code,
        };
        assert_eq!(
            mk(Support::NoJournal, Some(ERR_JOURNAL_NOT_ACTIVE), Some("NTFS")).fault_text(),
            "USN 日志未激活"
        );
        assert_eq!(
            mk(
                Support::NoJournal,
                Some(ERR_JOURNAL_DELETE_IN_PROGRESS),
                Some("NTFS")
            )
            .fault_text(),
            "USN 日志正在被删除"
        );
        assert!(mk(Support::NoJournal, Some(1181), Some("NTFS"))
            .fault_text()
            .contains("1181"));
        assert!(mk(Support::NotNtfs, None, Some("exFAT"))
            .fault_text()
            .contains("exFAT"));
        assert!(mk(Support::VolumeUnreachable, Some(21), Some("NTFS"))
            .fault_text()
            .contains("提权也无效"));
    }

    #[cfg(windows)]
    #[test]
    fn win32_code_reads_facility_seven_only() {
        use windows::core::HRESULT;
        let e = anyhow::Error::new(windows::core::Error::from_hresult(HRESULT::from_win32(1179)))
            .context("外层包装也要能挖到");
        assert_eq!(win32_code(&e), Some(1179));
        assert_eq!(win32_code(&e), Some(ERR_JOURNAL_NOT_ACTIVE));
        // 非 Win32 facility(NTSTATUS 直传)不许被当成 Win32 码解释。
        let n = anyhow::Error::new(windows::core::Error::from_hresult(HRESULT(0xC000_0001u32 as i32)));
        assert_eq!(win32_code(&n), None);
        // 没有 windows::core::Error 的错误链 → None。
        assert_eq!(win32_code(&anyhow!("纯文本错误")), None);
        assert!(is_access_denied(&anyhow::Error::new(
            windows::core::Error::from_hresult(HRESULT::from_win32(5))
        )));
        assert!(!is_access_denied(&anyhow!("纯文本错误")));
    }

    #[cfg(windows)]
    #[test]
    fn err_consts_match_windows_crate() {
        // 裸常量是为了让分类逻辑脱离真机可测;这个断言负责钉住它不与 crate 漂移。
        use windows::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED,
            ERROR_JOURNAL_NOT_ACTIVE,
        };
        assert_eq!(ERR_ACCESS_DENIED, ERROR_ACCESS_DENIED.0);
        assert_eq!(
            ERR_JOURNAL_DELETE_IN_PROGRESS,
            ERROR_JOURNAL_DELETE_IN_PROGRESS.0
        );
        assert_eq!(ERR_JOURNAL_NOT_ACTIVE, ERROR_JOURNAL_NOT_ACTIVE.0);
        assert_eq!(ERR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_ENTRY_DELETED.0);
    }
}
