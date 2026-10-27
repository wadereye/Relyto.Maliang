//! 监听文件夹:事件驱动(`ReadDirectoryChangesW`)+ 降级轮询。
//!
//! 对齐原版 Picasa3 的两段参考结论:
//! - 目录监视初始化 + 目录变更事件源:动态加载 `ReadDirectoryChangesW`,
//!   **其返回值作为开关** —— 拿不到就用轮询兜底(降级门控)。
//! - 目录项枚举:变更后按 `FindNextFileW` 的粒度逐个文件取属性入库,
//!   而不是整目录重扫(本模块的 `scan::index_file` 就是这个粒度)。
//!
//! 实现:`notify-debouncer-full`(Windows 后端即 `ReadDirectoryChangesWatcher`,
//! 与原版同款内核 API)。若 watcher 建立/监听失败(网络盘、不支持的文件系统、
//! 权限不足),自动回落到原来的 30s 轮询路径 —— 即原版的"降级门控"。
//!
//! 事件粒度:收到变更事件后只处理**受影响的目录**(其父目录),逐文件入库,
//! 避免整棵子树重扫(扫描本身已有"未变文件快速路径",但仍要走目录树)。
use crate::db::Store;
use notify_debouncer_full::notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use walkdir::WalkDir;


/// 目录快照:排序后的 (路径, mtime_ms, size) 列表,只含**可索引**的媒体文件
/// (图片 + RAW + 视频 —— 判定统一走 `crate::scan::is_indexable`,
/// 避免此处与事件路径收录范围不一致)。
/// 首轮调用建立基线(不触发扫描)。降级轮询路径使用。
pub fn snapshot(root: &Path) -> Vec<(String, i64, u64)> {
    let mut v: Vec<(String, i64, u64)> = WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| crate::scan::is_indexable(e.path()))
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            let mtime = md
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis() as i64;
            Some((e.path().to_string_lossy().into_owned(), mtime, md.len()))
        })
        .collect();
    v.sort();
    v
}

/// 快照差异:(新增数, 修改数)。删除不计(索引清理走 图库维护)。
pub fn diff_counts(old: &[(String, i64, u64)], new: &[(String, i64, u64)]) -> (usize, usize) {
    let old_m: HashMap<&str, (i64, u64)> = old
        .iter()
        .map(|(p, m, s)| (p.as_str(), (*m, *s)))
        .collect();
    let (mut added, mut modified) = (0usize, 0usize);
    for (p, m, s) in new {
        match old_m.get(p.as_str()) {
            None => added += 1,
            Some(&(om, os)) => {
                if om != *m || os != *s {
                    modified += 1;
                }
            }
        }
    }
    (added, modified)
}

/// 监听器的运行模式(降级门控的结果)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// 事件驱动(`ReadDirectoryChangesW` 可用)。
    Event,
    /// 降级:轮询(事件通道不可用)。
    Poll,
}

impl Mode {
    /// 供 UI 展示当前监听模式。
    pub fn label(self) -> &'static str {
        match self {
            Mode::Event => "事件驱动",
            Mode::Poll => "轮询(降级)",
        }
    }
}

/// 监听器的运行状态:模式 + 消息通道。
pub struct Watcher {
    /// 当前模式(事件驱动 / 降级轮询);启动后可能从 Event 退化为 Poll。
    pub mode: Arc<AtomicU8>,
    /// 有变化并完成增量扫描时发送汇总文案。
    pub events: mpsc::Receiver<String>,
}

/// 模式在 `Watcher.mode` 中的编码(原子字节,便于跨线程读)。
const MODE_EVENT: u8 = 0;
const MODE_POLL: u8 = 1;

/// 启动监听线程。`on` 开关控制是否扫描(关闭时清空基线,恢复后重建,避免惊群)。
/// 返回消息接收端:有变化并完成增量扫描时发送汇总文案。
pub fn spawn(store: Arc<Store>, on: Arc<AtomicBool>) -> Watcher {
    let (tx, rx) = mpsc::channel();
    let mode = Arc::new(AtomicU8::new(MODE_EVENT));
    let mode2 = mode.clone();
    std::thread::spawn(move || {
        // ---- 建立事件源(降级门控:目录变更事件源的"拿不到就轮询") ----
        let (evt_tx, evt_rx) = mpsc::channel::<Vec<PathBuf>>();
        let watcher = new_debouncer(
            Duration::from_millis(800), // 去抖窗口
            None,
            move |res: DebounceEventResult| {
                if let Ok(events) = res {
                    let paths: Vec<PathBuf> =
                        events.into_iter().flat_map(|e| e.paths.clone()).collect();
                    if !paths.is_empty() {
                        let _ = evt_tx.send(paths);
                    }
                }
            },
        );

        match watcher {
            Ok(mut debouncer) => {
                // 已监听的根目录集合(用于增删 watch)。
                let mut watched: Vec<PathBuf> = Vec::new();
                let mut base: HashMap<PathBuf, Vec<(String, i64, u64)>> = HashMap::new();
                // 上次基线比对的时间(30s 周期)。
                let mut last_poll = std::time::Instant::now();
                loop {
                    // 同步监听集合(用户随时可能增删文件夹)。
                    let roots = store.list_watched();
                    let (to_add, to_remove) = sync_plan(&watched, &roots);
                    // 新增:挂上 watch
                    for r in &to_add {
                        if debouncer.watch(r, RecursiveMode::Recursive).is_ok() {
                            watched.push(r.clone());
                        }
                        // 挂不上(网络盘/不支持的文件系统)也不致命:
                        // 超时分支的基线轮询仍会覆盖这些根目录。
                    }
                    // 移除:已从图库删掉的目录
                    for r in &to_remove {
                        let _ = debouncer.unwatch(r);
                        watched.retain(|w| w != r);
                    }

                    // 等待事件(短超时,便于及时感知 watched 表变化;
                    // 原版的变更检测是秒级的(约 1000ms 一次))。
                    // **基线比对兜底**仍按 30s 周期做(见下面的 poll 计时)。
                    match evt_rx.recv_timeout(Duration::from_millis(1000)) {
                        Ok(paths) => {
                            if !on.load(Ordering::SeqCst) {
                                base.clear();
                                continue;
                            }
                            let dirs = affected_dirs(&paths, &roots);
                            if dirs.is_empty() {
                                continue;
                            }
                            let n = scan_dirs(&store, &dirs);
                            let _ = tx.send(format!(
                                "监听:{n} 张照片变更,已增量入库(事件驱动)"
                            ));
                            last_poll = std::time::Instant::now();
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            // 1s 未收到事件 → 检查是否到了 30s 的基线比对周期。
                            // (短暂超时是常态,不代表出问题了)
                            if last_poll.elapsed() < Duration::from_secs(30) {
                                continue;
                            }
                            last_poll = std::time::Instant::now();
                            // 30s 无事件:做一次基线比对,兜住漏掉的事件
                            // (某些网络盘/虚拟文件系统不保证事件完整)。
                            if !on.load(Ordering::SeqCst) {
                                base.clear();
                                continue;
                            }
                            let mut changed: Vec<PathBuf> = Vec::new();
                            let (mut added, mut modified) = (0usize, 0usize);
                            for r in &roots {
                                let snap = snapshot(r);
                                match base.get(r) {
                                    None => {
                                        base.insert(r.clone(), snap);
                                    }
                                    Some(old) => {
                                        let (a, m) = diff_counts(old, &snap);
                                        if a + m > 0 {
                                            changed.push(r.clone());
                                            added += a;
                                            modified += m;
                                        }
                                        base.insert(r.clone(), snap);
                                    }
                                }
                            }
                            base.retain(|k, _| roots.contains(k));
                            if !changed.is_empty() {
                                let _ = crate::scan::scan_dir(&store, &changed);
                                let _ = tx.send(format!(
                                    "监听:{added} 张新照片、{modified} 张变更,已增量扫描(轮询兜底)"
                                ));
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            }
            Err(_) => {
                // ---- 降级:事件通道不可用 → 纯轮询(原版的回落路径) ----
                mode2.store(MODE_POLL, Ordering::SeqCst);
                let mut base: HashMap<PathBuf, Vec<(String, i64, u64)>> = HashMap::new();
                let mut last_poll = std::time::Instant::now();
                loop {
                    // 短睡眠 + 30s 计时:与事件驱动模式的节奏一致,
                    // 保证"新增监听目录"最迟 30s 内被感知。
                    std::thread::sleep(Duration::from_millis(1000));
                    if last_poll.elapsed() < Duration::from_secs(30) {
                        continue;
                    }
                    last_poll = std::time::Instant::now();
                    if !on.load(Ordering::SeqCst) {
                        base.clear();
                        continue;
                    }
                    let roots = store.list_watched();
                    let mut changed: Vec<PathBuf> = Vec::new();
                    let (mut added, mut modified) = (0usize, 0usize);
                    for r in &roots {
                        let snap = snapshot(r);
                        match base.get(r) {
                            None => {
                                base.insert(r.clone(), snap);
                            }
                            Some(old) => {
                                let (a, m) = diff_counts(old, &snap);
                                if a + m > 0 {
                                    changed.push(r.clone());
                                    added += a;
                                    modified += m;
                                }
                                base.insert(r.clone(), snap);
                            }
                        }
                    }
                    base.retain(|k, _| roots.contains(k));
                    if !changed.is_empty() {
                        let _ = crate::scan::scan_dir(&store, &changed);
                        let _ = tx.send(format!(
                            "监听:{added} 张新照片、{modified} 张变更,已增量扫描(轮询模式)"
                        ));
                    }
                }
            }
        }
    });
    Watcher { mode, events: rx }
}

/// 计算监听集合的增删(可测的纯逻辑)。
///
/// 返回 `(要新增的根, 要移除的根)`。
/// - 新增:当前 `roots` 里有、但 `watched` 里没有的;
/// - 移除:当前 `roots` 里没有、但 `watched` 里还挂着的。
///
/// 抽成纯函数是为了能测"用户新增监听目录后,下一轮同步就会挂上"
/// —— 这正是 v5 审计发现的缺陷(watcher 启动后新增目录要等 30s 才生效)。
fn sync_plan(watched: &[PathBuf], roots: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let to_add: Vec<PathBuf> = roots
        .iter()
        .filter(|r| !watched.contains(r))
        .cloned()
        .collect();
    let to_remove: Vec<PathBuf> = watched
        .iter()
        .filter(|w| !roots.contains(w))
        .cloned()
        .collect();
    (to_add, to_remove)
}

/// 事件路径 → 需要重新扫描的根目录(去重)。
///
/// 只取"事件的父目录",不再往上走到根 —— 上层目录里的其它文件没变,
/// 重扫会被"未变快速路径"跳过,但仍要走目录树,浪费 IO。
fn affected_dirs(paths: &[PathBuf], roots: &[PathBuf]) -> Vec<PathBuf> {    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        // 事件本身可能是目录(新建文件夹)→ 用它自己;否则用父目录。
        let dir = if p.is_dir() {
            p.clone()
        } else {
            match p.parent() {
                Some(d) => d.to_path_buf(),
                None => continue,
            }
        };
        // 只处理在监听根之内的路径。
        if !roots.iter().any(|r| dir.starts_with(r)) {
            continue;
        }
        if !out.contains(&dir) {
            out.push(dir);
        }
    }
    out
}

/// 逐文件入库(事件驱动的粒度:原版 `FindNextFileW` 一次一个目录项)。
/// 返回新增/变更的文件数。
fn scan_dirs(store: &Store, dirs: &[PathBuf]) -> usize {
    let mut n = 0usize;
    for d in dirs {
        let it = WalkDir::new(d)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter(|e| crate::scan::is_indexable(e.path()));
        for e in it {
            if crate::scan::index_file(store, e.path()).unwrap_or(false) {
                n += 1;
            }
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_and_diff() {
        let dir = std::env::temp_dir().join(format!("rk-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.jpg");
        let b = dir.join("b.png");
        let c = dir.join("note.txt"); // 非图像应被忽略
        std::fs::write(&a, b"1").unwrap();
        std::fs::write(&b, b"22").unwrap();
        std::fs::write(&c, b"skip").unwrap();
        let s1 = snapshot(&dir);
        assert_eq!(s1.len(), 2, "只应收录 2 张图像");

        // 基线 → 无变化
        assert_eq!(diff_counts(&s1, &s1), (0, 0));

        // 新增一张 + 修改一张内容(内容变了 size 也变)
        std::fs::write(&a, b"longer-content").unwrap();
        std::fs::write(dir.join("c.webp"), b"333").unwrap();
        let s2 = snapshot(&dir);
        let (add, modif) = diff_counts(&s1, &s2);
        assert_eq!(add, 1, "应检出 1 张新增");
        assert_eq!(modif, 1, "应检出 1 张修改");
        assert_eq!(s2.len(), 3);

        // 空目录快照为空
        let empty = std::env::temp_dir().join(format!("rk-watch-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        assert!(snapshot(&empty).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }

    /// 事件路径 → 受影响目录:只取父目录,且必须在监听根之内。
    #[test]
    fn affected_dirs_takes_parent_and_filters_roots() {
        let roots = vec![PathBuf::from("C:\\pics")];
        let paths = vec![
            PathBuf::from("C:\\pics\\sub\\a.jpg"),
            PathBuf::from("C:\\pics\\sub\\a.jpg"), // 重复
            PathBuf::from("C:\\other\\b.jpg"),      // 根之外,应被过滤
        ];
        let dirs = affected_dirs(&paths, &roots);
        assert_eq!(dirs.len(), 1, "重复路径应去重,根外应过滤");
        assert_eq!(dirs[0], PathBuf::from("C:\\pics\\sub"));
    }

    /// v5 审计:watcher 启动后新增监听目录,下一轮同步就要挂上
    /// (此前要等 30s 超时才能感知到 watched 表的变化)。
    #[test]
    fn sync_plan_detects_added_and_removed_roots() {
        let a = PathBuf::from("C:\\pics");
        let b = PathBuf::from("C:\\more");

        // 初始:已挂 a,当前只有 a → 无增无删。
        let (add, rm) = sync_plan(&[a.clone()], &[a.clone()]);
        assert!(add.is_empty() && rm.is_empty());

        // 用户新增 b → 应在下一轮挂上。
        let (add, rm) = sync_plan(&[a.clone()], &[a.clone(), b.clone()]);
        assert_eq!(add, vec![b.clone()]);
        assert!(rm.is_empty());

        // 用户移除 a → 应解挂。
        let (add, rm) = sync_plan(&[a.clone(), b.clone()], &[b.clone()]);
        assert!(add.is_empty());
        assert_eq!(rm, vec![a.clone()]);

        // 全换 → 两个方向都有。
        let (add, rm) = sync_plan(&[a.clone()], &[b.clone()]);
        assert_eq!(add, vec![b.clone()]);
        assert_eq!(rm, vec![a.clone()]);
    }
}
