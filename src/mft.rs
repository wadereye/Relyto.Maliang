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
//! **能力边界**:USN 记录只有名字/父 FRN/属性,没有 size/mtime ——
//! 入库管线仍需逐文件 stat + EXIF(见 scan.rs)。

#![allow(dead_code)]

use anyhow::{anyhow, Result};
use std::path::PathBuf;

/// 卷的 USN 能力等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// USN journal 已激活,可直接全量枚举。
    Usn,
    /// 卷可用但 journal 未激活(数据盘常见);需提权激活或放弃。
    NoJournal,
    /// 打开卷句柄被拒 —— 未提权。
    NeedsAdmin,
    /// 非 NTFS 卷(无 USN journal 概念)。
    NotNtfs,
    /// 非 Windows 平台。
    Unsupported,
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

/// 卷根目录的 file reference index(MFT 记录 5)。
/// 父 FRN 的低 48 位等于它 → 条目在卷根下。
pub const ROOT_FRN_INDEX: u64 = 5;
/// FRN = (sequence << 48) | index,拼路径时只比 index。
const FRN_INDEX_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

/// 枚举本机可扫卷(固定盘 + 可移动盘,与 scan.rs 的卷分流策略一致:
/// 网络盘/光驱不收录)并探测 USN 能力。非 Windows 返回空。
pub fn probe() -> Vec<VolumeProbe> {
    #[cfg(windows)]
    {
        let mut out = Vec::new();
        for letter in scannable_letters() {
            let root = PathBuf::from(format!("{letter}:\\"));
            let fs = crate::win::volume_filesystem(&root);
            let (support, journal) = match fs.as_deref() {
                Some("NTFS") => match open_volume(letter) {
                    Err(_) => (Support::NeedsAdmin, None),
                    Ok(h) => {
                        let j = query_journal(&h);
                        drop(h);
                        match j {
                            Ok(j) => (Support::Usn, Some(j)),
                            // 未激活/正在删除/条目被回收都归为 NoJournal,
                            // 语义统一为「当前不可枚举」。
                            Err(_) => (Support::NoJournal, None),
                        }
                    }
                },
                _ => (Support::NotNtfs, None),
            };
            out.push(VolumeProbe {
                root,
                letter,
                fs,
                support,
                journal,
            });
        }
        out
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// 全量枚举一个 NTFS 卷的 MFT 记录流。
///
/// `on_entry(frn, parent_frn, attrs, name_utf16)` 逐条回调;返回 `false`
/// 视为取消,枚举提前结束(`ScanStats.cancelled = true`)。
/// `ctrl` 传 `Some` 时按批推进进度(`ScanControl`,total 未知保持 0,
/// UI 应显示「已枚举 N 条」而非百分比)。
pub fn scan_volume(
    letter: char,
    journal: &JournalState,
    ctrl: Option<&crate::scan::ScanControl>,
    mut on_entry: impl FnMut(u64, u64, u32, &[u16]) -> bool,
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
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]])),
                );
                let is_dir = rec.attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
                if is_dir {
                    stats.dirs += 1;
                } else {
                    stats.files += 1;
                }
                n += 1;
                let keep = on_entry(rec.frn, rec.parent_frn, rec.attrs, &name);
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
pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0001;
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0002;
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
/// `on_record(frn, parent_frn, attrs, reason, name_utf16)` 返回 `false`
/// 提前中止(本批内未处理记录随 checkpoint 前进而丢弃,调用方自行权衡)。
pub fn read_journal(
    letter: char,
    journal_id: u64,
    start_usn: i64,
    mut on_record: impl FnMut(u64, u64, u32, u32, &[u16]) -> bool,
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
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]])),
                );
                stats.records += 1;
                let keep = on_record(rec.frn, rec.parent_frn, rec.attrs, rec.reason, &name);
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

/// 打开卷设备句柄失败时的错误分类(UI 据此提示提权)。
pub fn is_access_denied(e: &anyhow::Error) -> bool {
    #[cfg(windows)]
    {
        e.chain().any(|c| {
            c.downcast_ref::<windows::core::Error>()
                .map(|we| we.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0))
                .unwrap_or(false)
        })
    }
    #[cfg(not(windows))]
    {
        let _ = e;
        false
    }
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
    name_off: usize,
    name_len: usize,
}

/// 解析 `USN_RECORD_V2/V3`。两者字段偏移一致,仅版本号与对齐不同,
/// 按 `FileNameOffset/FileNameLength` 通用读取。缓冲不足或长度异常返回 None。
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
        .map_err(|e| anyhow!("打开卷 {path} 失败: {e}"))?;
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
        .map_err(|e| anyhow!("FSCTL_QUERY_USN_JOURNAL 失败: {e}"))?;
        Ok(JournalState {
            journal_id: jq.UsnJournalID,
            first_usn: jq.FirstUsn,
            next_usn: jq.NextUsn,
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
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_HANDLE_EOF, ERROR_JOURNAL_ENTRY_DELETED};
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
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
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
    }
}
