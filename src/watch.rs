//! 监听文件夹:事件驱动(自研 `ReadDirectoryChangesW`,见 [`crate::fswatch`])
//! + 降级轮询。
//!
//! 设计结论:
//! - 目录监视初始化 + 目录变更事件源:动态加载 `ReadDirectoryChangesW`,
//!   **其返回值作为开关** —— 拿不到就用轮询兜底(降级门控)。
//! - 目录项枚举:变更后按 `FindNextFileW` 的粒度逐个文件取属性入库,
//!   而不是整目录重扫(本模块的 `scan::index_file` 就是这个粒度)。
//!
//! 实现:自研 `fswatch`(内核 API)。此前用 notify-debouncer-full,
//! 其 Windows 后端在监听目录里删文件时偶发进程级硬崩溃(
//! 到复现;上游修复只在 9.0-rc),故换成自研实现。若监听失败(网络盘、
//! 不支持的文件系统、权限不足),自动回落到 30s 轮询路径 ——
//! 即"降级门控"。
//!
//! 事件粒度:收到变更事件后只处理**受影响的目录**(其父目录),逐文件入库,
//! 避免整棵子树重扫(扫描本身已有"未变文件快速路径",但仍要走目录树)。
//!
//! 删除是例外:不做重扫,而是即时对账 —— 改名在 fswatch 层已配对成
//! `Renamed{from,to}`(原位改行,元数据全保留),剩下的 `Removed` 就是
//! 真删除,直接把索引行与缩略图缓存清掉(移出图库)。
//! 降级轮询路径不自动删行:快照分不清重命名与"删除+新增",误删会丢元数据。
use crate::db::Store;
#[cfg(windows)]
use crate::fswatch::{FsEvent, Fswatch};
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
///
/// `excl` 用于**整目录剪枝**(修 A1):被排除的目录连进都不进。此前快照不看
/// 排除规则,回收站 / 应用数据目录里的条目会进基线,再被兜底扫描写进库。
/// 首轮调用建立基线(不触发扫描)。降级轮询路径使用。
pub fn snapshot(root: &Path, excl: &crate::scan::Exclusions) -> Vec<(String, i64, u64)> {
    let mut v: Vec<(String, i64, u64)> = WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !prune(e, excl))
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

/// WalkDir 剪枝判定:是目录且命中排除规则 → 整棵子树不进。
///
/// `depth() > 0` 是刻意的:**监听根本身永不剪**。用户显式把 `D:\Temp`
/// 加进监听,那是明确意图,不该被出厂排除名单一票否决;剪的是它下面的子目录。
fn prune(e: &walkdir::DirEntry, excl: &crate::scan::Exclusions) -> bool {
    e.depth() > 0
        && e.file_type().is_dir()
        && e.file_name()
            .to_str()
            .is_some_and(|n| excl.excludes_dir(n))
}

/// 快照差异 → (新增数, 修改数, 变化的文件路径)。
///
/// 删除**不计**:轮询快照分不清"重命名"与"删除+新增",
/// 自动删行会丢星标/标签等元数据(靠改名配对保住,见点 4)。
/// 删除对账只走事件驱动路径(事件源已把改名配对成 Modify)。
///
/// 连路径一起返回是为了让 30s 兜底能**只处理变化的那些目录**:
/// 把路径喂给 [`affected_dirs`],而不是拿整个监听根去重扫一遍。
pub fn diff(
    old: &[(String, i64, u64)],
    new: &[(String, i64, u64)],
) -> (usize, usize, Vec<PathBuf>) {
    let old_m: HashMap<&str, (i64, u64)> = old
        .iter()
        .map(|(p, m, s)| (p.as_str(), (*m, *s)))
        .collect();
    let (mut added, mut modified) = (0usize, 0usize);
    let mut paths = Vec::new();
    for (p, m, s) in new {
        match old_m.get(p.as_str()) {
            None => {
                added += 1;
                paths.push(PathBuf::from(p));
            }
            Some(&(om, os)) => {
                if om != *m || os != *s {
                    modified += 1;
                    paths.push(PathBuf::from(p));
                }
            }
        }
    }
    (added, modified, paths)
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
#[cfg_attr(not(windows), allow(dead_code))]
const MODE_EVENT: u8 = 0;
#[cfg_attr(windows, allow(dead_code))]
const MODE_POLL: u8 = 1;

/// 启动监听线程。`on` 开关控制是否扫描(关闭时清空基线,恢复后重建,避免惊群)。
/// 返回消息接收端:有变化并完成增量扫描时发送汇总文案。
pub fn spawn(store: Arc<Store>, on: Arc<AtomicBool>) -> Watcher {
    let (tx, rx) = mpsc::channel();
    let mode = Arc::new(AtomicU8::new(MODE_EVENT));
    let mode2 = mode.clone();
    std::thread::spawn(move || {
        // ---- 建立事件源(降级门控:目录变更事件源的"拿不到就轮询") ----
        // fswatch 已在内部做 800ms 去抖和改名配对;这里拿到的批次里
        // Removed = 真删除(即时对账),Changed = 新增/变更(重扫目录),
        // Renamed = 改名对(原位改行,元数据保留)。
        #[cfg(windows)]
        {
            let fsw = Fswatch::spawn();
            mode2.store(MODE_EVENT, Ordering::SeqCst);
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
                    if fsw.watch(r) {
                        watched.push(r.clone());
                    }
                    // 挂不上(网络盘/不支持的文件系统)也不致命:
                    // 超时分支的基线轮询仍会覆盖这些根目录。
                }
                // 移除:已从图库删掉的目录
                for r in &to_remove {
                    fsw.unwatch(r);
                    watched.retain(|w| w != r);
                }

                // 等待事件(短超时,便于及时感知 watched 表变化;
                // 变更检测是秒级的(约 1000ms 一次)。
                // **基线比对兜底**仍按 30s 周期做(见下面的 poll 计时)。
                match fsw.recv_batch(Duration::from_millis(1000)) {
                    Ok(items) => {
                        if !on.load(Ordering::SeqCst) {
                            base.clear();
                            continue;
                        }
                        let mut removed_paths: Vec<PathBuf> = Vec::new();
                        let mut touched: Vec<PathBuf> = Vec::new();
                        let mut renames: Vec<(PathBuf, PathBuf)> = Vec::new();
                        for e in items {
                            match e {
                                FsEvent::Removed(p) => removed_paths.push(p),
                                FsEvent::Changed(p) => touched.push(p),
                                FsEvent::Renamed { from, to } => renames.push((from, to)),
                            }
                        }
                        // 删除对账:能走到这里的 Removed 就是真删除 →
                        // 直接从图库移出(L1 在线时先问索引,省逐条 stat)。
                        let removed = drop_removed(&store, &removed_paths, &roots);
                        // 改名对:原位改行(id 不变,元数据保留);目标已存在
                        // 等回退情形返回待重扫目录。
                        let renamed = apply_renames(&store, &renames, &roots);
                        touched.extend(renamed.iter().cloned());
                        let dirs = affected_dirs(&touched, &roots);
                        let n = if dirs.is_empty() {
                            0
                        } else {
                            // 事件路径与兜底路径共用同一份排除规则
                            // 快路慢路不许各判一套。
                            let excl = crate::scan::Exclusions::from_store(&store);
                            scan_dirs(&store, &dirs, &excl)
                        };
                        if n == 0 && removed == 0 && renamed.is_empty() {
                            continue;
                        }
                        let mut msg = format!("监听:{n} 张照片变更,已增量入库(事件驱动)");
                        if removed > 0 {
                            msg.push_str(&format!(";{removed} 个已删除文件移出图库"));
                        }
                        if !renamed.is_empty() {
                            msg.push_str(&format!(";{} 个改名已原位更新", renamed.len()));
                        }
                        let _ = tx.send(msg);
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
                        let (added, modified, ndirs) = poll_once(&store, &roots, &mut base);
                        if ndirs > 0 {
                            let _ = tx.send(format!(
                                "监听:{added} 张新照片、{modified} 张变更,已增量扫描 {ndirs} 个目录(轮询兜底)"
                            ));
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        }
        #[cfg(not(windows))]
        {
            // 非 Windows 没有 fswatch:直接走纯轮询(回落路径)。
            mode2.store(MODE_POLL, Ordering::SeqCst);
            let mut base: HashMap<PathBuf, Vec<(String, i64, u64)>> = HashMap::new();
            let mut last_poll = std::time::Instant::now();
            loop {
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
                let (added, modified, ndirs) = poll_once(&store, &roots, &mut base);
                if ndirs > 0 {
                    let _ = tx.send(format!(
                        "监听:{added} 张新照片、{modified} 张变更,已增量扫描 {ndirs} 个目录(轮询模式)"
                    ));
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

/// 逐文件入库(事件驱动的粒度:`FindNextFileW` 一次一个目录项)。
/// 返回新增/变更的文件数。被排除的目录整棵剪掉(与 [`snapshot`] 同一份规则)。
fn scan_dirs(store: &Store, dirs: &[PathBuf], excl: &crate::scan::Exclusions) -> usize {
    let mut n = 0usize;
    for d in dirs {
        let it = WalkDir::new(d)
            .into_iter()
            .filter_entry(|e| !prune(e, excl))
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

/// 30s 基线比对兜底的一轮:逐根做快照 diff,只把**变化的目录**交给增量扫描。
///
/// 事件驱动与降级轮询两条路共用这一段:兜底不再是"整根重扫",
/// 而是 diff 出变化的文件 → [`affected_dirs`] → 只扫这些目录。
///
/// 返回 `(新增数, 修改数, 实际扫描的目录数)`;首轮只建基线,目录数为 0。
fn poll_once(
    store: &Store,
    roots: &[PathBuf],
    base: &mut HashMap<PathBuf, Vec<(String, i64, u64)>>,
) -> (usize, usize, usize) {
    // 每轮重读一次排除规则:用户在设置里改了名单,下一轮兜底就该生效。
    let excl = crate::scan::Exclusions::from_store(store);
    let (mut added, mut modified) = (0usize, 0usize);
    let mut changed: Vec<PathBuf> = Vec::new();
    for r in roots {
        let snap = snapshot(r, &excl);
        match base.get(r) {
            // 首轮只建基线,不触发扫描(否则每次开监听都等于全量重扫)。
            None => {
                base.insert(r.clone(), snap);
            }
            Some(old) => {
                let (a, m, paths) = diff(old, &snap);
                added += a;
                modified += m;
                changed.extend(paths);
                base.insert(r.clone(), snap);
            }
        }
    }
    // 监听集合缩了 → 对应基线也丢掉,免得下次重新加回来时拿陈旧基线比对。
    base.retain(|k, _| roots.contains(k));
    if changed.is_empty() {
        return (added, modified, 0);
    }
    let dirs = affected_dirs(&changed, roots);
    let n = dirs.len();
    scan_dirs(store, &dirs, &excl);
    (added, modified, n)
}

/// 删除事件 → 移出图库(删除索引行 + 清缩略图缓存),返回实际移出的张数。
///
/// 只处理监听根之内的路径。是否真被删了**先问 L1 再问磁盘**(
/// 的省 IO + 竞态兜底两者兼得):
/// 服务在线且那一卷建了索引时,L1 说「还在」的路径直接跳过,一次 `stat`
/// 都不做;L1 说「不在」的还要落盘复核一次 —— 因为 L1 是**滞后快照**,
/// 它对「刚删掉」和「刚建好还没扫进索引」两种情况给的是同一个答案,
/// 只凭它会把毫秒前刚写好的文件从图库里抹掉(服务装起来后
/// 单测就是这么稳定复现的)。磁盘才是当场的事实。
fn drop_removed(store: &Store, removed: &[PathBuf], roots: &[PathBuf]) -> usize {
    let mut paths: Vec<String> = Vec::new();
    for p in removed {
        if !roots.iter().any(|r| p.starts_with(r)) {
            continue;
        }
        let s = p.to_string_lossy().into_owned();
        if !paths.contains(&s) {
            paths.push(s);
        }
    }
    if paths.is_empty() {
        return 0;
    }
    match l1_gone(&paths) {
        Some(gone) => {
            if gone.is_empty() {
                return 0;
            }
            let real: Vec<String> = gone
                .into_iter()
                .filter(|p| !Path::new(p).exists())
                .collect();
            if real.is_empty() {
                return 0;
            }
            drop_and_purge(store.drop_paths_confirmed(&real))
        }
        None => drop_and_purge(store.drop_missing_paths(&paths)),
    }
}

/// 删行 + 清缩略图缓存,返回移出的张数。
fn drop_and_purge(r: anyhow::Result<Vec<(i64, String)>>) -> usize {
    match r {
        Ok(rows) => {
            for (id, _) in &rows {
                crate::thumbs::purge_cached(*id);
            }
            rows.len()
        }
        Err(_) => 0,
    }
}

/// 改名对 → 原位改行(id 不变,星标/标签/相册/缩略图全保留)。
///
/// 返回**需要重扫的目录**:目录改名走「旧前缀摘行 + 新子树重录」
/// (行的路径串全变,原位更新不划算);目标路径已有行的覆盖式改名
/// 同样交给重扫。只处理监听根之内的事件。
fn apply_renames(
    store: &Store,
    renames: &[(PathBuf, PathBuf)],
    roots: &[PathBuf],
) -> Vec<PathBuf> {
    let mut rescan: Vec<PathBuf> = Vec::new();
    for (from, to) in renames {
        if !roots.iter().any(|r| from.starts_with(r) && to.starts_with(r)) {
            continue;
        }
        if Path::new(to).is_dir() {
            // 目录改名:旧前缀(含子树)整棵摘掉;新子树由重扫补齐。
            let f = from.to_string_lossy().into_owned();
            drop_and_purge(store.drop_paths_confirmed(&[f]));
            rescan.push(to.clone());
            continue;
        }
        let parent = match to.parent() {
            Some(d) => d.to_string_lossy().into_owned(),
            None => continue,
        };
        match store.rename_photo(
            from.to_string_lossy().as_ref(),
            to.to_string_lossy().as_ref(),
            &parent,
        ) {
            Ok(true) => {}
            Ok(false) => {
                // 目标已有行(覆盖式改名):该目录交给重扫去对账。
                if let Some(d) = to.parent() {
                    rescan.push(d.to_path_buf());
                }
            }
            Err(_) => {}
        }
    }
    rescan
}

/// 问 L1 索引服务:这批路径里哪些**可能**已经不在了(候选,调用方仍需落盘复核)。
///
/// 返回 `None` = 不能拿 L1 当权威,调用方退回磁盘复核。两种情况会给 `None`:
/// 管道连不上/协议失败,以及**该路径所在卷不在服务的可信索引里** ——
/// 服务对没有索引的卷一律回 `exists=false`,把那个当"文件没了"会整卷误删
/// (非 NTFS 卷、还没建完的卷都属此类,要求它们走兼容路径)。
///
/// 同理,任何一条问答失败就把**整批**交回磁盘复核:宁可多几次 `stat`,
/// 也不能因为一次管道抖动把还在的文件移出图库。
fn l1_gone(paths: &[String]) -> Option<Vec<String>> {
    // 用户关掉全盘快查时,监听对账也不该偷偷连服务 —— 与 UI 的降级门
    // (`app::SVC_LINK_ENABLED`,管心跳/重连/装卸)同一口径。返回 None
    // 让调用方走纯磁盘复核;顺带省掉逐条 Stat 的管道往返,批量删除
    // 不会再被服务的查询排队拖到秒级(20 张 12.6s)。
    if !crate::app::SVC_LINK_ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let mut c = crate::ipc::Client::connect(200).ok()?;
    // 先问一次概况,拿到"哪些卷真的建了索引"。
    let trusted: std::collections::HashSet<char> = match c.call(&crate::ipc::Req::Status) {
        Ok(crate::ipc::Resp::Status(s)) => s
            .vols
            .iter()
            .filter(|v| v.entries > 0 && !v.building)
            .map(|v| v.letter)
            .collect(),
        _ => return None,
    };
    let mut gone = Vec::new();
    for p in paths {
        let letter = p
            .trim()
            .chars()
            .next()
            .map(|ch| ch.to_ascii_uppercase())
            .unwrap_or('\0');
        if !trusted.contains(&letter) {
            return None;
        }
        match c.call(&crate::ipc::Req::Stat { path: p.clone() }) {
            Ok(crate::ipc::Resp::Stat(si)) => {
                if !si.exists {
                    gone.push(p.clone());
                }
            }
            _ => return None,
        }
    }
    Some(gone)
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
        let excl = crate::scan::Exclusions::default();
        let s1 = snapshot(&dir, &excl);
        assert_eq!(s1.len(), 2, "只应收录 2 张图像");

        // 基线 → 无变化
        let (a0, m0, p0) = diff(&s1, &s1);
        assert_eq!((a0, m0), (0, 0));
        assert!(p0.is_empty());

        // 新增一张 + 修改一张内容(内容变了 size 也变)
        std::fs::write(&a, b"longer-content").unwrap();
        std::fs::write(dir.join("c.webp"), b"333").unwrap();
        let s2 = snapshot(&dir, &excl);
        let (add, modif, paths) = diff(&s1, &s2);
        assert_eq!(add, 1, "应检出 1 张新增");
        assert_eq!(modif, 1, "应检出 1 张修改");
        assert_eq!(s2.len(), 3);
        // 变化的路径要一并给出:30s 兜底靠它算出「只扫哪些目录」。
        assert_eq!(paths.len(), 2, "新增 + 修改各一条");
        assert!(paths.iter().any(|p| p == &a), "被改的那张要在里面");
        assert!(
            paths.iter().any(|p| p.file_name().is_some_and(|n| n == "c.webp")),
            "新增的那张要在里面"
        );

        // 空目录快照为空
        let empty = std::env::temp_dir().join(format!("rk-watch-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        assert!(snapshot(&empty, &excl).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }

    /// 快照要按排除规则**整目录剪枝**(修 A1),但监听根本身不剪。
    #[test]
    fn snapshot_prunes_excluded_dirs_but_not_the_root() {
        let dir = std::env::temp_dir().join(format!("rk-watch-excl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // 回收站旧名在硬编码 4 条里,不可关。
        std::fs::create_dir_all(dir.join("RECYCLER")).unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("keep.jpg"), b"1").unwrap();
        std::fs::write(dir.join("sub").join("deep.jpg"), b"2").unwrap();
        std::fs::write(dir.join("RECYCLER").join("$I001.jpg"), b"3").unwrap();
        let excl = crate::scan::Exclusions::default();
        let s = snapshot(&dir, &excl);
        assert_eq!(s.len(), 2, "排除目录里的条目不该进快照:{s:?}");
        assert!(
            !s.iter().any(|(p, _, _)| p.contains("RECYCLER")),
            "RECYCLER 整棵子树应被剪掉:{s:?}"
        );

        // 用户显式把被排除的目录加进监听 → 根自己照样扫(剪的是它下面的子目录)。
        let rec = dir.join("RECYCLER");
        assert_eq!(snapshot(&rec, &excl).len(), 1, "监听根本身永不剪");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 事件路径 → 受影响目录:只取父目录,且必须在监听根之内。
    #[cfg(windows)]
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

    /// 同一条口径在 POSIX 路径上的对偶:只取父目录,且必须在监听根之内。
    #[cfg(not(windows))]
    #[test]
    fn affected_dirs_takes_parent_and_filters_roots_posix() {
        let roots = vec![PathBuf::from("/pics")];
        let paths = vec![
            PathBuf::from("/pics/sub/a.jpg"),
            PathBuf::from("/pics/sub/a.jpg"), // 重复
            PathBuf::from("/other/b.jpg"),    // 根之外,应被过滤
        ];
        let dirs = affected_dirs(&paths, &roots);
        assert_eq!(dirs.len(), 1, "重复路径应去重,根外应过滤");
        assert_eq!(dirs[0], PathBuf::from("/pics/sub"));
    }

    /// 删除事件 → 只处理监听根之内、且磁盘上确实已消失的路径。
    /// 装了索引服务的开发机上这条会走 L1 分支:索引里还没有 `kept.jpg`
    /// (刚落盘,保鲜还没扫到),不许因此把它当成已删除。
    #[test]
    fn drop_removed_filters_roots_and_checks_disk() {
        let store = crate::db::Store::open_mem().unwrap();
        let root = std::env::temp_dir().join(format!("rk-watch-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let kept = root.join("kept.jpg");
        let gone = root.join("gone.jpg");
        std::fs::write(&kept, b"x").unwrap();
        let _ = std::fs::remove_file(&gone);
        {
            let c = store.test_conn();
            for p in [&kept, &gone] {
                c.execute(
                    "INSERT INTO photos(path,parent,mtime,size) VALUES(?1,'/',0,0)",
                    rusqlite::params![p.to_string_lossy().as_ref()],
                )
                .unwrap();
            }
        }
        let outside = std::env::temp_dir().join("outside-root.jpg");
        let items = vec![
            gone.clone(),
            kept.clone(),   // 仍存在 → 必须跳过
            outside,        // 根之外 → 必须跳过
        ];
        assert_eq!(drop_removed(&store, &items, &[root.clone()]), 1);

        let c = store.test_conn();
        let kept_n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM photos WHERE path=?1",
                rusqlite::params![kept.to_string_lossy().as_ref()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept_n, 1, "仍存在的文件不能被移出图库");
        let all_n: i64 = c.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0)).unwrap();
        assert_eq!(all_n, 1, "只有确已消失的文件被删除");
        drop(c);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 回归:fswatch 必须把"文件被删除"报成 `Removed`、改名配对成
    /// `Renamed` —— 这是删除对账与改名保元的触发条件。
    #[test]
    #[cfg(windows)]
    fn fswatch_reports_remove_and_rename() {
        let dir = std::env::temp_dir().join(format!("rk-watch-kind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fsw = Fswatch::spawn();
        assert!(fsw.watch(&dir), "挂监听失败");
        let f = dir.join("x.jpg");
        std::fs::write(&f, b"1").unwrap();
        // 先让创建事件的一批过去,避免与后续事件混在同一批里。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match fsw.recv_batch(Duration::from_millis(500)) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }
        // 改名:应配对成 Renamed。
        let g = dir.join("y.jpg");
        std::fs::rename(&f, &g).unwrap();
        let mut seen_rename = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match fsw.recv_batch(Duration::from_millis(500)) {
                Ok(batch) => {
                    if batch.iter().any(|e| matches!(e,
                        FsEvent::Renamed { from, to }
                        if from.file_name().map(|n| n == "x.jpg").unwrap_or(false)
                            && to.file_name().map(|n| n == "y.jpg").unwrap_or(false)
                    )) {
                        seen_rename = true;
                        break;
                    }
                }
                Err(_) => continue,
            }
        }
        // 删除:应报成 Removed。
        std::fs::remove_file(&g).unwrap();
        let mut seen_remove = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match fsw.recv_batch(Duration::from_millis(500)) {
                Ok(batch) => {
                    if batch.iter().any(|e| matches!(e,
                        FsEvent::Removed(p)
                        if p.file_name().map(|n| n == "y.jpg").unwrap_or(false)
                    )) {
                        seen_remove = true;
                        break;
                    }
                }
                Err(_) => continue,
            }
        }
        fsw.unwatch(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(seen_rename, "fswatch 未把改名配对成 Renamed");
        assert!(seen_remove, "fswatch 未把删除报成 Removed 事件");
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
