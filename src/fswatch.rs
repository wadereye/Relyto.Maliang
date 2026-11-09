//! 自研 ReadDirectoryChangesW 目录监听(替代 notify 8.2 的 Windows 后端)。
//!
//! 背景:notify 8.2 的完成例程 + 泄漏给内核的缓冲区实现,在监听目录里
//! 删除文件时偶发**进程级硬崩溃**(无 panic 输出;上游稳定版未包含修复)。
//! 因此本模块刻意采用
//! 最朴素、无完成例程的实现:
//!
//! - 每个监听根一个线程,overlapped 读 + 事件对象二选一等待;
//!   停止路径是「SetEvent → CancelIo → 关句柄」,不存在句柄生命周期竞态;
//! - 解析按 `GetOverlappedResult` 返回的**实际字节数**严格截断,
//!   越界即断,不猜内核给的数据;
//! - 改名事件在本层配对(`Renamed{from,to}`),上层可保住星标/标签等
//!   元数据(notify 的去重器只给两条独立路径,上层做不到这件事)。
//!
//! 事件汇总(800ms 去抖窗口)由协调线程做,对上层仍是「一批一批」。

#![cfg(windows)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Storage::FileSystem as fs;
use windows::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects};
use windows::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};

/// 事件缓冲 64KB:一次读不完内核会分批给,
/// 每批都会触发一次完成事件,不会漏。
const BUF_SIZE: usize = 64 * 1024;

/// 去抖窗口:秒级节奏。
const DEBOUNCE: Duration = Duration::from_millis(800);

/// FILE_NOTIFY_INFORMATION 的固定头(3 × u32 = 12 字节),
/// FileName 紧跟其后 —— 不能用 Rust 结构体的 size_of(含 padding 会差 4)。
const FNI_HEADER: usize = 12;

/// 单条原始事件。
#[derive(Debug, Clone)]
pub enum FsEvent {
    /// 新增或内容变更(上层重扫所在目录)。
    Changed(PathBuf),
    /// 已删除(上层移出图库)。
    Removed(PathBuf),
    /// 改名(本层已配对;上层可原位改行,保住元数据)。
    Renamed { from: PathBuf, to: PathBuf },
}

enum Cmd {
    Watch(PathBuf),
    Unwatch(PathBuf),
}

/// 监听器句柄:发命令、收批量事件。
pub struct Fswatch {
    cmd_tx: Sender<Cmd>,
    ev_rx: Receiver<Vec<FsEvent>>,
}

impl Fswatch {
    /// 启动协调线程(内部再按需起每根的工作线程)。
    pub fn spawn() -> Fswatch {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let raw: Arc<Mutex<Vec<FsEvent>>> = Arc::new(Mutex::new(Vec::new()));
        std::thread::Builder::new()
            .name("fswatch-coordinator".into())
            .spawn({
                let raw = raw.clone();
                move || coordinator(cmd_rx, ev_tx, raw)
            })
            .expect("启动 fswatch 协调线程失败");
        Fswatch { cmd_tx, ev_rx }
    }

    /// 挂上一个监听根(递归)。返回是否成功(失败不致命,上层有轮询兜底)。
    pub fn watch(&self, p: &Path) -> bool {
        self.cmd_tx.send(Cmd::Watch(p.to_path_buf())).is_ok()
    }

    /// 摘掉一个监听根。
    pub fn unwatch(&self, p: &Path) {
        let _ = self.cmd_tx.send(Cmd::Unwatch(p.to_path_buf()));
    }

    /// 取下一批事件(至多等 `timeout`)。
    pub fn recv_batch(&self, timeout: Duration) -> Result<Vec<FsEvent>, RecvTimeoutError> {
        self.ev_rx.recv_timeout(timeout)
    }
}

/// 协调线程:处理挂/摘命令 + 每 100ms 收割原始事件、按去抖窗口批量转发。
fn coordinator(cmd_rx: Receiver<Cmd>, ev_tx: Sender<Vec<FsEvent>>, raw: Arc<Mutex<Vec<FsEvent>>>) {
    let mut roots: HashMap<PathBuf, StopFlag> = HashMap::new();
    let mut pending: Vec<FsEvent> = Vec::new();
    let mut pending_since: Option<Instant> = None;
    loop {
        // 100ms 一拍:既是命令循环,也是事件收割节拍。
        match cmd_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Cmd::Watch(p)) => {
                if roots.contains_key(&p) {
                    continue;
                }
                let flag = root_thread_start(p.clone(), raw.clone());
                roots.insert(p, flag);
            }
            Ok(Cmd::Unwatch(p)) => {
                if let Some(flag) = roots.remove(&p) {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // 所有者(Fswatch)已丢弃:停掉全部根,收摊。
                for flag in roots.values() {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                return;
            }
        }
        // 收割原始事件。
        let drained: Vec<FsEvent> = raw
            .lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default();
        if !drained.is_empty() {
            pending_since.get_or_insert_with(Instant::now);
            pending.extend(drained);
        }
        // 去抖:窗口到点就整批转发。
        if let Some(t0) = pending_since {
            if t0.elapsed() >= DEBOUNCE && !pending.is_empty() {
                pending_since = None;
                let _ = ev_tx.send(std::mem::take(&mut pending));
            }
        }
    }
}

/// HANDLE 原始指针不自动实现 Send;停止信号改走 `Arc<AtomicBool>`,
/// 根线程自己建事件对象、自己关句柄 —— 不跨线程移交任何内核句柄。
type StopFlag = Arc<std::sync::atomic::AtomicBool>;

/// 停止等待超时(根线程每 500ms 醒一次看停止标志;事件延迟不受影响)。
const WAIT_SLICE: u32 = 500;

/// 为一个监听根起工作线程,返回停止标志(协调线程持有)。
fn root_thread_start(root: PathBuf, raw: Arc<Mutex<Vec<FsEvent>>>) -> StopFlag {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let raw2 = raw;
    let stop2 = stop.clone();
    let started = std::thread::Builder::new()
        .name(format!("fswatch-root[{}]", root.display()))
        .spawn(move || root_thread_loop(root, stop2, raw2));
    if started.is_err() {
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    stop
}

/// 每根的工作线程:overlapped 读 + 等完成/停止二选一。
fn root_thread_loop(root: PathBuf, stop: StopFlag, raw: Arc<Mutex<Vec<FsEvent>>>) {
    // SAFETY:路径 UTF-16 + NUL 结尾;句柄/事件的生命周期都在本线程内闭环。
    let wide: Vec<u16> = root
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let dir = unsafe {
        fs::CreateFileW(
            PCWSTR(wide.as_ptr()),
            fs::FILE_LIST_DIRECTORY.0,
            fs::FILE_SHARE_READ | fs::FILE_SHARE_WRITE | fs::FILE_SHARE_DELETE,
            None,
            fs::OPEN_EXISTING,
            fs::FILE_FLAG_BACKUP_SEMANTICS | fs::FILE_FLAG_OVERLAPPED,
            None,
        )
    };
    let dir = match dir {
        Ok(h) if h != INVALID_HANDLE_VALUE => h,
        _ => return,
    };
    let io_ev = unsafe { CreateEventW(None, false, false, PCWSTR::null()) };
    let io_ev = match io_ev {
        Ok(h) if h != INVALID_HANDLE_VALUE => h,
        _ => {
            unsafe {
                let _ = CloseHandle(dir);
            }
            return;
        }
    };
    let mut buf = vec![0u8; BUF_SIZE];
    loop {
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        let mut ovl = OVERLAPPED::default();
        ovl.hEvent = io_ev;
        // SAFETY:buf 在本次 overlapped 读完成前不动(等待在本线程);
        // ovl/hEvent 都在本线程栈上,GetOverlappedResult 同线程等待。
        let issued = unsafe {
            fs::ReadDirectoryChangesW(
                dir,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                true, // 递归子树
                fs::FILE_NOTIFY_CHANGE_FILE_NAME
                    | fs::FILE_NOTIFY_CHANGE_DIR_NAME
                    | fs::FILE_NOTIFY_CHANGE_LAST_WRITE
                    | fs::FILE_NOTIFY_CHANGE_SIZE,
                None,
                Some(&mut ovl),
                None, // 不用完成例程:崩溃面越小越好
            )
        };
        if issued.is_err() {
            break;
        }
        // 等完成事件,最多 WAIT_SLICE;超时则回看停止标志。
        // 内层循环:超时不重发读(上一轮读还挂着),只继续等。
        let handles = [io_ev];
        loop {
            let wait = unsafe { WaitForMultipleObjects(&handles, false, WAIT_SLICE) };
            let wait = wait.0;
            if wait == WAIT_TIMEOUT.0 {
                // 没等到完成:看停止标志再决定要不要继续等。
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    unsafe {
                        let _ = CancelIo(dir);
                        // 排干被取消的操作:完成包不落地就关句柄,
                        // 内核仍可能写缓冲 —— 等它结束再走。
                        let mut n: u32 = 0;
                        let _ = GetOverlappedResult(dir, &mut ovl, &mut n, true);
                    }
                    break;
                }
                continue;
            }
            if wait != WAIT_OBJECT_0.0 {
                // io_ev 意外 / 出错:视为致命,交给上层轮询兜底。
                break;
            }
            let mut transferred: u32 = 0;
            let got = unsafe { GetOverlappedResult(dir, &mut ovl, &mut transferred, false) };
            if got.is_err() {
                // 目录被删/卷拔了:通知上层(去掉这个根由重启/对账兜底)。
                break;
            }
            parse_notifications(&root, &buf[..transferred as usize], &raw);
            break;
        }
    }
    unsafe {
        let _ = CloseHandle(dir);
        let _ = CloseHandle(io_ev);
    }
}

/// 把一整批 FILE_NOTIFY_INFORMATION 解析成事件;改名(OLD→NEW)在此配对。
fn parse_notifications(root: &Path, data: &[u8], raw: &Arc<Mutex<Vec<FsEvent>>>) {
    let mut pending_old: Option<PathBuf> = None;
    let mut offset = 0usize;
    while offset + FNI_HEADER <= data.len() {
        let next = read_u32(data, offset);
        let action = read_u32(data, offset + 4);
        let name_bytes = read_u32(data, offset + 8) as usize;
        let name_start = offset + FNI_HEADER;
        // 严格按实际接收字节数截断:半条/越界一律丢弃,不猜。
        if name_bytes % 2 != 0 || name_start + name_bytes > data.len() {
            break;
        }
        let units = name_bytes / 2;
        let name = String::from_utf16_lossy(
            &data[name_start..name_start + name_bytes]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<u16>>()[..units],
        );
        // FileName 相对监听根(不带根);根前缀在转发前拼好。
        let full = if name.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&name)
        };
        match action {
            a if a == fs::FILE_ACTION_ADDED.0 || a == fs::FILE_ACTION_MODIFIED.0 => {
                push(raw, FsEvent::Changed(full));
            }
            a if a == fs::FILE_ACTION_REMOVED.0 => {
                // 改名序列里 OLD_NAME 后面必跟 NEW_NAME;没跟就是真删除。
                if let Some(old) = pending_old.take() {
                    push(raw, FsEvent::Removed(old));
                }
                push(raw, FsEvent::Removed(full));
            }
            a if a == fs::FILE_ACTION_RENAMED_OLD_NAME.0 => {
                if let Some(old) = pending_old.take() {
                    push(raw, FsEvent::Removed(old));
                }
                pending_old = Some(full);
            }
            a if a == fs::FILE_ACTION_RENAMED_NEW_NAME.0 => {
                if let Some(old) = pending_old.take() {
                    push(
                        raw,
                        FsEvent::Renamed {
                            from: old,
                            to: full,
                        },
                    );
                } else {
                    // 没配对上的 NEW_NAME(丢批兜底):按新增处理。
                    push(raw, FsEvent::Changed(full));
                }
            }
            _ => {}
        }
        if next == 0 {
            break;
        }
        offset += next as usize;
    }
    if let Some(old) = pending_old.take() {
        push(raw, FsEvent::Removed(old));
    }
}

fn push(raw: &Arc<Mutex<Vec<FsEvent>>>, e: FsEvent) {
    if let Ok(mut v) = raw.lock() {
        v.push(e);
    }
}

/// 小端 u32 读取(不足即 0,调用方已保证 offset+4 <= len)。
fn read_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

// ---- 可测的纯逻辑 ----

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析必须严格按接收字节数截断:半条记录不许产出事件。
    #[test]
    fn parse_truncates_at_partial_record() {
        let root = Path::new("D:\\p");
        let raw: Arc<Mutex<Vec<FsEvent>>> = Arc::new(Mutex::new(Vec::new()));
        // 头说名字 4 字节,实际只给了 2 字节 → 整条丢弃。
        let mut d = Vec::new();
        d.extend_from_slice(&0u32.to_le_bytes()); // NextEntryOffset = 0
        d.extend_from_slice(&fs::FILE_ACTION_ADDED.0.to_le_bytes());
        d.extend_from_slice(&4u32.to_le_bytes()); // FileNameLength = 4 字节
        d.extend_from_slice(&b"ab"[..]); // 只给了 1 个 UTF-16 单元
        parse_notifications(root, &d, &raw);
        assert!(raw.lock().unwrap().is_empty(), "半条记录必须丢弃");
    }

    /// 改名 OLD→NEW 配对成 Renamed;单独 OLD 兜底成 Removed。
    #[test]
    fn parse_pairs_renames() {
        let root = Path::new("D:\\p");
        let raw: Arc<Mutex<Vec<FsEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let mut d = Vec::new();
        let name_wide: Vec<u8> = "a.jpg"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let name2_wide: Vec<u8> = "b.jpg"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        // 条目一:RENAMED_OLD_NAME a.jpg,有下一条
        d.extend_from_slice(&((FNI_HEADER + name_wide.len()) as u32).to_le_bytes());
        d.extend_from_slice(&fs::FILE_ACTION_RENAMED_OLD_NAME.0.to_le_bytes());
        d.extend_from_slice(&(name_wide.len() as u32).to_le_bytes());
        d.extend_from_slice(&name_wide);
        // 条目二:RENAMED_NEW_NAME b.jpg,无下一条
        d.extend_from_slice(&0u32.to_le_bytes());
        d.extend_from_slice(&fs::FILE_ACTION_RENAMED_NEW_NAME.0.to_le_bytes());
        d.extend_from_slice(&(name2_wide.len() as u32).to_le_bytes());
        d.extend_from_slice(&name2_wide);
        parse_notifications(root, &d, &raw);
        let v = raw.lock().unwrap();
        assert_eq!(v.len(), 1);
        match &v[0] {
            FsEvent::Renamed { from, to } => {
                assert_eq!(from, &PathBuf::from("D:\\p\\a.jpg"));
                assert_eq!(to, &PathBuf::from("D:\\p\\b.jpg"));
            }
            other => panic!("应配对成 Renamed,实际 {other:?}"),
        }
    }
}
