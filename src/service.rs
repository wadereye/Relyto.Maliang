//! 常驻索引服务(提权服务与非提权 UI 拆进程)。
//!
//! 为什么要拆:`$MFT` 与 USN 日志只有管理员读得到,而 GUI 不该常驻提权。
//! 于是提权的一边只留"索引引擎"——建库、保鲜、把结构元数据写进 L2;
//! 非提权的 UI 通过命名管道([`crate::ipc`])问它要结果。UI 连不上就降级:
//! 逐目录扫描照旧能用,只是没有全盘快查,界面明示这一点。
//!
//! 线程模型(「每卷一个常驻线程」):
//! - **保鲜线程 × 卷数**:阻塞在 `FSCTL_READ_USN_JOURNAL`(`ReturnOnlyOnClose`)
//!   上,醒来才有活干。读日志的那段**不持锁**,拿到 ops 之后才短暂持锁并入;
//! - **建库/追赶线程 × 1**:全量重建与 L2 追赶都是分钟级重活,串行跑,
//!   免得两卷同时建库把磁盘打满;
//! - **落盘线程 × 1**:脏了才写,最快 [`SAVE_INTERVAL`] 一次;
//! - **管道线程 × 1 + 每连接一个**。
//!
//! 停止语义:保鲜线程可能正阻塞在内核的 IOCTL 里,这个等待**取消不了**。
//! 所以关停时先存盘、报 STOPPED,再 `exit` —— 让 SCM 等到超时不如自己收尾。

use crate::ipc::{self, Child, Cursor, Hit, Query, Resp, StatInfo, StatusInfo, VolStatus};
use crate::mft::{self, JournalState, Support};
use crate::nameidx::{self, VolumeIndex};
use anyhow::Result;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// SCM 里的服务名(注册/启动/查询都用它)。
pub const SERVICE_NAME: &str = "RelytoMaliangIndex";
/// 服务列表里显示的名字。
pub const SERVICE_DISPLAY: &str = "Relyto.Maliang 全盘文件名索引";
/// 服务描述(控制面板里那段话,要能让用户判断该不该留着它)。
pub const SERVICE_DESC: &str = "维护全盘文件名索引与照片结构元数据:直读 NTFS $MFT 建库、订阅 USN 日志保鲜,\
通过本地命名管道向 Relyto.Maliang 界面提供快速查找。停用后界面降级为逐目录扫描,没有全盘快查。";

/// 服务日志文件名(落在数据目录)。
const LOG_NAME: &str = "index-service.log";
/// 日志保留上限:超过就截断重写。服务是常驻的,不设上限迟早吃掉 D 盘。
const LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// 索引落盘节奏:脏了才写,最快 5 分钟一次。
/// 全盘索引落一次是 90 MB,开发机上 C 盘每分钟都有变更,
/// 60 s 一轮等于每小时往盘里灌 5 GB —— 而这份文件唯一的作用是省下重启时
/// 那次 $MFT 全量重建,重建只要 4.3 s(C:)+ 2.3 s(D:),不值得。
const SAVE_INTERVAL: Duration = Duration::from_secs(300);
/// 卷问不出来时的退避下/上限(拔盘、日志被关、权限被撤)。
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// 建库进度日志间隔(每秒一条太吵,日志要能留几天)。
const BUILD_LOG_EVERY: Duration = Duration::from_secs(5);

/// 后台任务队列里的一格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    /// 全量重建一卷(首建、journal 失效、用户点了"重建")。
    Rebuild(char),
    /// 只补 L2:索引已在内存里,但结构库落后(服务停过一阵 → 追赶)。
    L2Catchup(char),
}

/// 一卷的非索引元信息(与 `State::vols` 同序同长)。
#[derive(Debug, Clone)]
struct VolMeta {
    letter: char,
    support: Support,
    /// 走不了 MFT/USN 的原因(非 NTFS、无权限、日志没开…);正常为空。
    why: String,
    /// 正在全量建库。
    building: bool,
    /// 建库时走了 USN 降级路径(size/mtime 拿不到)。
    degraded: bool,
}

/// 锁内的全部可变状态。
#[derive(Default)]
struct State {
    /// 各卷 SoA 索引。`search_class` 要 `&[VolumeIndex]`,所以这里必须是
    /// 连续的 `Vec<VolumeIndex>`;元信息另放 `meta`,两边同序。
    vols: Vec<VolumeIndex>,
    meta: Vec<VolMeta>,
    journals: HashMap<char, JournalState>,
    queue: VecDeque<Job>,
    /// 已起保鲜线程的卷(重建后不要重复起)。
    fresh: HashSet<char>,
}

/// 服务日志:提权进程里没有控制台,事后排查全靠这份文件。
struct Log {
    f: Mutex<Option<std::fs::File>>,
    /// 控制台模式(`--console`)同时打到 stdout。
    echo: bool,
}

/// 打开日志文件;打不开就退化成只写控制台(`--console` 下仍可见)。
fn open_log_file() -> Option<std::fs::File> {
    let p = crate::db::data_dir().join(LOG_NAME);
    // 超上限就截断:宁可丢旧账,不可吃掉数据盘。
    if std::fs::metadata(&p).map(|m| m.len() > LOG_MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::remove_file(&p);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .ok()
}

impl Log {
    fn open(echo: bool) -> Log {
        Log {
            f: Mutex::new(open_log_file()),
            echo,
        }
    }

    fn line(&self, msg: &str) {
        use std::io::Write;
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        if self.echo {
            println!("[{stamp}] {msg}");
        }
        let mut g = self.f.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = g.as_mut() {
            let _ = writeln!(f, "[{stamp}] {msg}");
        }
    }
}

/// 索引引擎:服务侧的全部状态与动作。
pub struct Engine {
    state: Mutex<State>,
    cv: Condvar,
    /// L2 写入口。打不开库时只维护 L1(服务不能因为库被占就整体罢工)。
    store: Option<Arc<crate::db::Store>>,
    /// 建库取消位:关停时置起,`build_volume` 在下一个检查点退出。
    ctrl: crate::scan::ScanControl,
    stop: AtomicBool,
    dirty: AtomicBool,
    started: Instant,
    last_save: Mutex<Option<Instant>>,
    /// 索引落盘目标。由构造方给,不在 `save_now` 里现取 `index_path()` ——
    /// 单元测试也会走 `shutdown()`→`save_now()`,让它现取就等于拿 3 条样例记录
    /// 盖掉用户真机上那 90 MB 索引(真发生过,服务下次启动只能全量重建)。
    blob: std::path::PathBuf,
    log: Log,
}

impl Engine {
    /// 起引擎:载入已有索引 → 探测各卷 → 排好首轮任务 → 拉起后台线程。
    ///
    /// `echo` 为真时日志同时打到 stdout(`--console` 开发模式用)。
    pub fn start(echo: bool) -> Arc<Engine> {
        let e = Arc::new(Engine {
            state: Mutex::new(State::default()),
            cv: Condvar::new(),
            store: match crate::db::Store::open() {
                Ok(s) => Some(s),
                Err(err) => {
                    // 库打不开不影响 L1:快查仍然可用,只是结构元数据不落库。
                    Log::open(echo).line(&format!("打开 index.db 失败,L2 落库停用:{err:#}"));
                    None
                }
            },
            ctrl: crate::scan::ScanControl::new(),
            stop: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            started: Instant::now(),
            last_save: Mutex::new(None),
            blob: nameidx::index_path(),
            log: Log::open(echo),
        });
        // 排查的第一行:服务和 UI 是不是同一个数据目录。
        // (就是靠这句才发现两边各建了一套库。)
        e.log.line(&format!(
            "数据目录:{:?}(服务形态={})",
            crate::db::data_dir(),
            crate::db::service_mode()
        ));
        let fresh_now = e.bootstrap();
        // 目录级失效:bootstrap 可能刚挂上失效位(索引版本不符),
        // 加上上一次进程没跑完留下的,这里一次性退回待派生再清位。
        if let Some(s) = &e.store {
            match s.refresh_pending_dirs() {
                Ok(0) => {}
                Ok(n) => e.log.line(&format!("目录级失效:{n} 行退回待派生")),
                Err(err) => e.log.line(&format!("目录级失效处理失败:{err:#}")),
            }
        }
        e.spawn_workers();
        for l in fresh_now {
            e.spawn_fresh(l);
        }
        e
    }

    /// 取锁(容忍中毒:某个线程 panic 过不该让整个服务失能)。
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 启动时的状态装配:落盘索引 + 卷探测结果合成一份内存状态,
    /// 并为每卷决定首轮该干什么(增量保鲜 / 全量重建 / 只补 L2)。
    ///
    /// 返回"可以立刻起保鲜线程"的卷;线程本身由 [`Engine::start`] 拿着 Arc 起。
    fn bootstrap(&self) -> Vec<char> {
        let path = self.blob.clone();
        let loaded = if path.exists() {
            match nameidx::load(&path) {
                Ok(f) => {
                    let n: usize = f.vols.iter().map(|v| v.count()).sum();
                    self.log
                        .line(&format!("载入已有索引:{} 卷 {} 条", f.vols.len(), n));
                    Some(f)
                }
                Err(e) => {
                    self.log.line(&format!("索引载入失败,按全量重建:{e:#}"));
                    // 版本不符 = 结构口径变了,旧账本上的目录全部作废重派生
                    // 。文件缺失/损坏不算:那只是还没有索引,
                    // 重建出来的路径口径没变,已派生的缩略图仍然有效。
                    if let Some(v) = nameidx::blob_version(&path) {
                        if v != nameidx::FORMAT_VERSION {
                            if let Some(s) = &self.store {
                                let n = s.mark_all_dirs_needs_refresh().unwrap_or(0);
                                self.log.line(&format!(
                                    "索引 v{v} 与程序 v{} 不符:{n} 个目录已挂失效位,待重派生",
                                    nameidx::FORMAT_VERSION
                                ));
                            }
                        }
                    }
                    None
                }
            }
        } else {
            self.log.line("尚无索引文件,首启走全量建库");
            None
        };
        let (mut vols, mut journals) = match loaded {
            Some(f) => (f.vols, f.journals),
            None => (Vec::new(), HashMap::new()),
        };

        let probes = mft::probe();
        let mut meta: Vec<VolMeta> = Vec::with_capacity(probes.len() + vols.len());
        let mut fresh_now: Vec<char> = Vec::new();
        let mut jobs: Vec<Job> = Vec::new();
        let mut seen: Vec<char> = Vec::new();
        for p in &probes {
            let letter = p.letter;
            seen.push(letter);
            let usable = p.support == Support::Usn;
            let why = if usable {
                String::new()
            } else {
                p.fault_text()
            };
            // 本轮探测到的卷:有旧索引就接着用,没有就占一个空位
            // (空位也要占 —— `meta` 与 `vols` 必须同序同长)。
            let idx = match vols.iter().position(|v| v.letter == letter) {
                Some(i) => vols.remove(i),
                None => VolumeIndex::empty(letter),
            };
            meta.push(VolMeta {
                letter,
                support: p.support,
                why: why.clone(),
                building: false,
                degraded: false,
            });
            if !usable {
                self.log.line(&format!("[{letter}:] 跳过索引:{why}"));
                vols.push(idx);
                continue;
            }
            // journal 状态是"这一轮"的事实,覆盖旧的(旧的可能来自别的日志实例)。
            let j = p.journal.unwrap_or(JournalState {
                journal_id: 0,
                first_usn: 0,
                next_usn: 0,
            });
            let saved = journals.get(&letter).copied();
            journals.insert(letter, j);
            vols.push(idx);
            let vi = vols.len() - 1;
            if vols[vi].count() > 0 && nameidx::can_resume(saved, &j) {
                fresh_now.push(letter);
                // L2 落后于 L1 → 只补库,不重建索引(的追赶)。
                let behind = self
                    .store
                    .as_ref()
                    .map(|s| s.l2_cursor(letter) < j.next_usn)
                    .unwrap_or(false);
                if behind {
                    jobs.push(Job::L2Catchup(letter));
                }
            } else {
                jobs.push(Job::Rebuild(letter));
            }
        }
        // 本轮没探测到的卷(盘拔了)保留旧索引与旧 checkpoint,原样留着。
        for v in &vols {
            if seen.contains(&v.letter) {
                continue;
            }
            meta.push(VolMeta {
                letter: v.letter,
                support: Support::VolumeUnreachable,
                why: "本轮未发现该卷(已保留上次索引)".into(),
                building: false,
                degraded: false,
            });
        }
        {
            let mut st = self.lock();
            st.vols = vols;
            st.meta = meta;
            st.journals = journals;
            for j in jobs {
                st.queue.push_back(j);
            }
        }
        self.cv.notify_all();
        fresh_now
    }

    /// 拉起后台线程(建库/追赶、落盘、管道)。保鲜线程按卷另起。
    fn spawn_workers(self: &Arc<Self>) {
        std::thread::Builder::new()
            .name("idx-work".into())
            .spawn({
                let e = self.clone();
                move || work_loop(e)
            })
            .ok();
        std::thread::Builder::new()
            .name("idx-save".into())
            .spawn({
                let e = self.clone();
                move || save_loop(e)
            })
            .ok();
        std::thread::Builder::new()
            .name("idx-ipc".into())
            .spawn({
                let e = self.clone();
                move || ipc_loop(e)
            })
            .ok();
    }

    /// 为一卷起保鲜线程(幂等)。
    fn spawn_fresh(self: &Arc<Self>, letter: char) {
        {
            let mut st = self.lock();
            if !st.fresh.insert(letter) {
                return;
            }
        }
        std::thread::Builder::new()
            .name(format!("idx-usn-{letter}"))
            .spawn({
                let e = self.clone();
                move || fresh_loop(e, letter)
            })
            .ok();
    }

    /// 关停:置停止位、唤醒工作队列、取消建库、存盘。
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.ctrl.cancel();
        self.cv.notify_all();
        self.save_now("服务关停");
        self.log.line("服务已停止");
    }

    // ---- 落盘 ----

    fn save_now(&self, why: &str) {
        let st = self.lock();
        let path = self.blob.clone();
        match nameidx::save(&path, &st.vols, &st.journals) {
            Ok(()) => {
                drop(st);
                self.dirty.store(false, Ordering::SeqCst);
                *self.last_save.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                // L2 里路径已不在 L1 的孤儿行清零(type-4 孤儿自愈):
                // 索引刚保鲜过,这一刻的 L1 就是权威,顺手把作用域账对齐。
                if let Some(s) = &self.store {
                    let gate = crate::db::ScopeGate::load(s);
                    if let Ok(n) = s.reconcile_rich_state(&gate) {
                        if n > 0 {
                            self.log.line(&format!("落盘对账:{n} 个脱管目录退回未纳入"));
                        }
                    }
                }
                let mb = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / 1048576.0;
                self.log
                    .line(&format!("索引已落盘({why}):{path:?},{mb:.1} MB"));
            }
            Err(e) => {
                drop(st);
                self.log.line(&format!("索引落盘失败({why}):{e:#}"));
            }
        }
    }

    // ---- 任务 ----

    /// 请求重建。空列表 = 全部能建的卷。
    fn request_rebuild(&self, letters: &[char]) {
        let mut st = self.lock();
        let want: Vec<char> = if letters.is_empty() {
            st.meta
                .iter()
                .filter(|m| m.support == Support::Usn)
                .map(|m| m.letter)
                .collect()
        } else {
            letters.to_vec()
        };
        // 队列里去重:同一卷被点两次不该建两遍。
        for l in want {
            let j = Job::Rebuild(l);
            if !st.queue.contains(&j) {
                st.queue.push_back(j);
            }
        }
        drop(st);
        self.cv.notify_all();
    }

    fn set_meta(&self, letter: char, f: impl FnOnce(&mut VolMeta)) {
        let mut st = self.lock();
        if let Some(m) = st.meta.iter_mut().find(|m| m.letter == letter) {
            f(m);
        }
    }

    /// 全量重建一卷。
    fn rebuild_one(self: &Arc<Self>, letter: char) {
        let probes = mft::probe();
        let Some(p) = probes.into_iter().find(|p| p.letter == letter) else {
            self.set_meta(letter, |m| {
                m.support = Support::VolumeUnreachable;
                m.why = "卷已消失".into();
                m.building = false;
            });
            self.log.line(&format!("[{letter}:] 重建跳过:卷已消失"));
            return;
        };
        self.set_meta(letter, |m| {
            m.support = p.support;
            m.why = if p.support == Support::Usn {
                String::new()
            } else {
                p.fault_text()
            };
            m.building = p.support == Support::Usn;
        });
        if p.support != Support::Usn {
            self.log
                .line(&format!("[{letter}:] 不能建索引:{}", p.fault_text()));
            return;
        }
        let t0 = Instant::now();
        let last = std::cell::Cell::new(Instant::now());
        let log = &self.log;
        self.log.line(&format!("[{letter}:] 开始全量建库(直读 $MFT)…"));
        let built = nameidx::build_volume(&p, Some(&self.ctrl), |n| {
            if last.get().elapsed() >= BUILD_LOG_EVERY {
                last.set(Instant::now());
                log.line(&format!("[{letter}:] 建库中…已 {n} 条"));
            }
        });
        let built = match built {
            Ok(b) => b,
            Err(e) => {
                self.set_meta(letter, |m| m.building = false);
                self.log.line(&format!("[{letter}:] 建库失败:{e:#}"));
                return;
            }
        };
        if let Some(d) = &built.degraded {
            self.log.line(&format!("[{letter}:] {d}"));
        }
        let next_usn = built.journal.next_usn;
        let rows;
        {
            let mut st = self.lock();
            let i = ensure_vol(&mut st, letter);
            st.vols[i] = built.idx;
            st.journals.insert(letter, built.journal);
            if let Some(m) = st.meta.get_mut(i) {
                m.building = false;
                m.degraded = built.degraded.is_some();
                m.support = Support::Usn;
                m.why = String::new();
            }
            rows = nameidx::l1_rows_full(&st.vols[i]);
        }
        self.dirty.store(true, Ordering::SeqCst);
        self.log.line(&format!(
            "[{letter}:] 建库完成:{} → 索引 {} 条,耗时 {:.1}s",
            built.source,
            built.entries,
            t0.elapsed().as_secs_f64()
        ));
        self.l2_full(letter, rows, next_usn);
        self.spawn_fresh(letter);
    }

    /// 只补 L2(索引已在内存,结构库落后)。
    fn l2_catchup(&self, letter: char) {
        if self.store.is_none() {
            return;
        }
        let (rows, next_usn) = {
            let st = self.lock();
            let Some(v) = st.vols.iter().find(|v| v.letter == letter) else {
                return;
            };
            let Some(j) = st.journals.get(&letter) else {
                return;
            };
            (nameidx::l1_rows_full(v), j.next_usn)
        };
        self.log.line(&format!(
            "[{letter}:] L2 追赶:补 {} 行(服务停机期间的变更)",
            rows.len()
        ));
        self.l2_full(letter, rows, next_usn);
    }

    /// 全量写 L2 + 推进 checkpoint。
    fn l2_full(&self, letter: char, rows: Vec<crate::db::L1Row>, next_usn: i64) {
        let Some(store) = &self.store else { return };
        let n = rows.len();
        match nameidx::ingest_l1(store, &rows) {
            Ok(ing) => {
                if let Err(e) = store.set_l2_cursor(letter, next_usn) {
                    self.log.line(&format!(
                        "[{letter}:] L2 写入 {n} 行(新增 {}/更新 {})但 checkpoint 失败:{e:#}",
                        ing.inserted, ing.updated
                    ));
                    return;
                }
                self.log.line(&format!(
                    "[{letter}:] L2 落库 {n} 行:新增 {},更新 {},checkpoint → {next_usn:#x}",
                    ing.inserted, ing.updated
                ));
            }
            Err(e) => self.log.line(&format!(
                "[{letter}:] L2 落库失败({e:#}),checkpoint 不推进,下一轮补"
            )),
        }
    }

    /// 一轮增量:并入 L1(持锁)→ 写 L2(不持锁)。
    fn apply_step(
        &self,
        letter: char,
        jid: u64,
        first_usn: i64,
        ops: Vec<nameidx::DeltaOp>,
        next_usn: i64,
        records: u64,
    ) {
        // 先按 FRN 拆批:apply_delta 会压实,下标随后就不可用了。
        let (up, del) = nameidx::split_delta_frns(&ops);
        let rows;
        let c;
        {
            let mut st = self.lock();
            let Some(i) = st.vols.iter().position(|v| v.letter == letter) else {
                return;
            };
            c = st.vols[i].apply_delta(ops);
            st.journals.insert(
                letter,
                JournalState {
                    journal_id: jid,
                    first_usn,
                    next_usn,
                },
            );
            rows = nameidx::l1_rows_for_frns(&st.vols[i], &up);
        }
        if c.added + c.updated + c.removed > 0 {
            self.dirty.store(true, Ordering::SeqCst);
            self.log.line(&format!(
                "[{letter}:] 增量:CLOSE {records} 条 → 新增 {}/更新 {}/删除 {},checkpoint → {next_usn:#x}",
                c.added, c.updated, c.removed
            ));
        }
        // 什么都没变的轮次不留账:USN 每 ~250ms 读一次,空闲卷也会出一串
        // `CLOSE 0 条`,一天就是几万行,会把真正的建库/失败记录挤出 4 MB 上限。
        self.l2_delta(letter, rows, &del, next_usn);
    }

    /// 增量写 L2 + 推进 checkpoint。
    fn l2_delta(&self, letter: char, rows: Vec<crate::db::L1Row>, del_frns: &[u64], next_usn: i64) {
        let Some(store) = &self.store else { return };
        match nameidx::ingest_l1(store, &rows) {
            Ok(ing) => {
                let dels: Vec<i64> = del_frns
                    .iter()
                    .map(|f| crate::db::frn64_of(letter, *f))
                    .collect();
                let ndel = match store.delete_l1_frn64(&dels) {
                    Ok(n) => n,
                    Err(e) => {
                        self.log.line(&format!(
                            "[{letter}:] L2 增量:新增 {}/更新 {} 已落,删除 {} 条失败({e:#}),checkpoint 不推进",
                            ing.inserted,
                            ing.updated,
                            dels.len()
                        ));
                        return;
                    }
                };
                if let Err(e) = store.set_l2_cursor(letter, next_usn) {
                    self.log.line(&format!(
                        "[{letter}:] L2 增量已落(新增 {}/更新 {}/删除 {ndel})但 checkpoint 失败:{e:#}",
                        ing.inserted, ing.updated
                    ));
                    return;
                }
                if ing.inserted + ing.updated + ndel > 0 {
                    self.log.line(&format!(
                        "[{letter}:] L2 增量落库:新增 {},更新 {},删除 {ndel}",
                        ing.inserted, ing.updated
                    ));
                }
            }
            Err(e) => self.log.line(&format!(
                "[{letter}:] L2 增量落库失败({e:#}),checkpoint 不推进,下一轮补"
            )),
        }
    }
}

/// 确保某卷在 `State` 里有位置,返回下标。
fn ensure_vol(st: &mut State, letter: char) -> usize {
    if let Some(i) = st.vols.iter().position(|v| v.letter == letter) {
        return i;
    }
    st.vols.push(VolumeIndex::empty(letter));
    st.meta.push(VolMeta {
        letter,
        support: Support::Usn,
        why: String::new(),
        building: false,
        degraded: false,
    });
    st.vols.len() - 1
}

// ---------------------------------------------------------------- 线程主体

/// 建库/追赶队列。串行:两卷同时建库会把磁盘打满,而这件事本来就不急。
fn work_loop(e: Arc<Engine>) {
    loop {
        let job = {
            let mut st = e.lock();
            loop {
                if let Some(j) = st.queue.pop_front() {
                    break j;
                }
                if e.stop.load(Ordering::SeqCst) {
                    return;
                }
                st = e.cv.wait(st).unwrap_or_else(|x| x.into_inner());
            }
        };
        if e.stop.load(Ordering::SeqCst) {
            return;
        }
        match job {
            Job::Rebuild(l) => e.rebuild_one(l),
            Job::L2Catchup(l) => e.l2_catchup(l),
        }
    }
}

/// 每卷一个保鲜线程。主体是阻塞的 IOCTL:没有变更就不醒。
fn fresh_loop(e: Arc<Engine>, letter: char) {
    e.log.line(&format!("[{letter}:] 保鲜线程启动"));
    let mut backoff = BACKOFF_MIN;
    // 诊断节奏:10s 一拍,把线程还活着、checkpoint 到哪了写进日志。
    // (只在空闲轮次打;有增量的轮次本来就有 增量 行。)
    let mut rounds = 0u64;
    let mut last_diag = Instant::now();
    struct RoundDiag { records: u64, ops: usize, next: i64 }
    let mut last_round = RoundDiag { records: 0, ops: 0, next: 0 };
    while !e.stop.load(Ordering::SeqCst) {
        // 每轮重新问一次 journal:日志可能被重建(id 变了),也可能被关掉。
        let probe = mft::probe().into_iter().find(|p| p.letter == letter);
        let (j, why) = match probe.as_ref() {
            Some(p) if p.support == Support::Usn => match p.journal {
                Some(j) => (Some(j), String::new()),
                None => (None, "卷能枚举但问不出 journal 状态".to_string()),
            },
            Some(p) => (None, p.fault_text()),
            None => (None, "卷已消失".to_string()),
        };
        let Some(j) = j else {
            e.set_meta(letter, |m| m.why = why.clone());
            e.log.line(&format!("[{letter}:] 保鲜暂停:{why}"));
            // 卷没了/日志关了:退避重试,不要空转。
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(BACKOFF_MAX);
            continue;
        };
        backoff = BACKOFF_MIN;
        let start = {
            let st = e.lock();
            st.journals.get(&letter).map(|s| s.next_usn).unwrap_or(j.next_usn)
        };
        rounds += 1;
        if last_diag.elapsed() >= Duration::from_secs(10) {
            last_diag = Instant::now();
            e.log.line(&format!(
                "[{letter}:] 保鲜诊断:checkpoint={start:#x} jid={:x} 近10s轮询 {rounds} 次(末轮 records={lr} ops={lo} next_usn={ln:#x})",
                j.journal_id, lr = last_round.records, lo = last_round.ops, ln = last_round.next
            ));
            rounds = 0;
        }
        match nameidx::poll_journal(letter, j.journal_id, start) {
            nameidx::JournalStep::Delta {
                ops,
                next_usn,
                records,
                raw,
            } => {
                last_round = RoundDiag { records, ops: ops.len(), next: next_usn };
                // 诊断(临时):把有记录的轮次摊开,验证内核到底回了什么、
                // collect_close_op 收了什么。收口前整段摘除。
                if records > 0 {
                    let det: Vec<String> = ops
                        .iter()
                        .take(8)
                        .map(|op| match op {
                            nameidx::DeltaOp::Upsert(u) => format!("up:{}", u.name),
                            nameidx::DeltaOp::Delete(f) => format!("del:{f:x}"),
                        })
                        .collect();
                    let raws: Vec<String> = raw
                        .iter()
                        .take(8)
                        .map(|(r, n)| format!("{r:#x}/{n}"))
                        .collect();
                    e.log.line(&format!(
                        "[{letter}:] 诊断:records={records} ops={} 明细[{}] 原始[{}]",
                        ops.len(),
                        det.join(" "),
                        raws.join(" ")
                    ));
                }
                e.apply_step(letter, j.journal_id, j.first_usn, ops, next_usn, records);
            }
            nameidx::JournalStep::RebuildNeeded => {
                e.log.line(&format!(
                    "[{letter}:] 日志游标已失效(条目被回收),转全量重建"
                ));
                e.request_rebuild(&[letter]);
                // 重建期间不要再抢日志:等建库线程把 checkpoint 换掉。
                std::thread::sleep(BACKOFF_MAX.min(Duration::from_secs(30)));
            }
            nameidx::JournalStep::Unavailable(why) => {
                e.set_meta(letter, |m| m.why = why.clone());
                e.log.line(&format!("[{letter}:] 保鲜失败:{why}({backoff:?} 后重试)"));
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
    e.log.line(&format!("[{letter}:] 保鲜线程退出"));
}

/// 落盘线程:脏了才写(的门控,15 分钟节奏下省下 GB 级无谓写入)。
fn save_loop(e: Arc<Engine>) {
    while !e.stop.load(Ordering::SeqCst) {
        std::thread::sleep(SAVE_INTERVAL);
        if e.dirty.load(Ordering::SeqCst) && !e.stop.load(Ordering::SeqCst) {
            e.save_now("定时");
        }
    }
}

// ---------------------------------------------------------------- IPC 服务端

impl Query for Engine {
    fn hello(&self) -> Resp {
        let st = self.lock();
        let (volumes, entries) = st.vols.iter().fold((0u32, 0u64), |(vs, n), v| {
            if v.count() > 0 {
                (vs + 1, n + v.count() as u64)
            } else {
                (vs, n)
            }
        });
        Resp::Hello {
            proto: ipc::PROTO_VERSION,
            volumes,
            entries,
        }
    }

    fn search(&self, text: &str, limit: u32, class: u8) -> Resp {
        // 上限收口:UI 只展示 top-N,一个恶意/写错的 limit 不该让服务分配无界。
        let lim = limit.clamp(1, 5000) as usize;
        // 投影在服务内完成:size/mtime/cls 服务端已经在内存里握着,
        // 顺手回给 UI 就省一次 stat。降级路径下这几列本来就是 0,含义是
        // "未知"。rich_state 先一律填哨兵,L1 收集完、state 锁放掉之后再统一补。
        let mut out: Vec<Hit> = {
            let st = self.lock();
            nameidx::search_class(&st.vols, text, lim, ipc::class_from_wire(class))
                .into_iter()
                .map(|h| {
                    let (size, mtime, cls) = st
                        .vols
                        .iter()
                        .find(|v| v.letter == h.letter)
                        .filter(|v| (h.idx as usize) < v.count())
                        .map(|v| {
                            (
                                v.size_of(h.idx as usize),
                                v.mtime_of(h.idx as usize) / 1_000_000,
                                v.class_of(h.idx as usize),
                            )
                        })
                        .unwrap_or((0, 0, 0));
                    Hit {
                        letter: h.letter,
                        idx: h.idx,
                        name: h.name,
                        path: h.path,
                        is_dir: h.is_dir,
                        size,
                        mtime,
                        cls,
                        rich_state: ipc::RICH_UNKNOWN,
                    }
                })
                .collect()
        };
        // L2 的派生门状态必须**在锁外**查:扫描线程也要这把 state 锁,持锁期间
        // 去碰 SQLite 等于让全盘扫描被一次磁盘查询堵住。一次批量 IN 查询走
        // `photos.path` 的唯一索引,不逐条查库。目录不是 photos 行,保持哨兵。
        if let Some(store) = self.store.as_ref() {
            let paths: Vec<String> = out
                .iter()
                .filter(|h| !h.is_dir)
                .map(|h| h.path.clone())
                .collect();
            if !paths.is_empty() {
                let rich = store.rich_states_by_paths(&paths);
                for h in out.iter_mut().filter(|h| !h.is_dir) {
                    if let Some(&s) = rich.get(&h.path) {
                        h.rich_state = s as u8;
                    }
                }
            }
        }
        Resp::Hits(out)
    }

    fn path_of(&self, letter: char, idx: u32) -> Resp {
        let st = self.lock();
        let p = st
            .vols
            .iter()
            .find(|v| v.letter == letter)
            .filter(|v| (idx as usize) < v.count())
            .map(|v| v.path_of(idx as usize));
        Resp::Path(p)
    }

    fn children_of(&self, letter: char, idx: u32) -> Resp {
        let st = self.lock();
        let Some(v) = st.vols.iter().find(|v| v.letter == letter) else {
            return Resp::Children(Vec::new());
        };
        // idx == count() 是"卷根直属"虚拟桶,合法;再大就是坏请求。
        if idx as usize > v.count() {
            return Resp::Children(Vec::new());
        }
        Resp::Children(
            v.children_of(idx as usize)
                .iter()
                .map(|&c| Child {
                    idx: c,
                    name: v.name(c as usize),
                    is_dir: v.is_dir(c as usize),
                })
                .collect(),
        )
    }

    fn stat(&self, path: &str) -> Resp {
        let letter = path
            .trim()
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase())
            .unwrap_or('\0');
        let miss = Resp::Stat(StatInfo {
            exists: false,
            letter,
            idx: 0,
            is_dir: false,
            size: 0,
            mtime: 0,
            mtime_usn: 0,
            cls: 0,
        });
        let st = self.lock();
        let Some(v) = st.vols.iter().find(|v| v.letter == letter) else {
            return miss;
        };
        let Some(i) = v.lookup_path(path) else {
            return miss;
        };
        Resp::Stat(StatInfo {
            exists: true,
            letter,
            idx: i as u32,
            is_dir: v.is_dir(i),
            size: v.size_of(i),
            mtime: v.mtime_of(i) / 1_000_000,
            mtime_usn: v.mtime_usn_of(i) / 1_000_000,
            cls: v.class_of(i),
        })
    }

    fn flush(&self) -> Resp {
        self.save_now("UI 请求");
        Resp::Ok
    }

    fn status(&self) -> Resp {
        let st = self.lock();
        let last_save = self
            .last_save
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(u64::MAX);
        Resp::Status(StatusInfo {
            proto: ipc::PROTO_VERSION,
            uptime_secs: self.started.elapsed().as_secs(),
            dirty: self.dirty.load(Ordering::SeqCst),
            last_save_ago_secs: last_save,
            vols: st
                .meta
                .iter()
                .map(|m| {
                    let entries = st
                        .vols
                        .iter()
                        .find(|v| v.letter == m.letter)
                        .map(|v| v.count() as u64)
                        .unwrap_or(0);
                    VolStatus {
                        letter: m.letter,
                        entries,
                        building: m.building,
                        why: m.why.clone(),
                    }
                })
                .collect(),
        })
    }

    fn rebuild(&self, letters: &[char]) -> Resp {
        self.request_rebuild(letters);
        Resp::Ok
    }

    fn cursors(&self) -> Resp {
        let st = self.lock();
        let mut out: Vec<Cursor> = st
            .journals
            .iter()
            .map(|(l, j)| Cursor {
                letter: *l,
                journal_id: j.journal_id,
                first_usn: j.first_usn,
                next_usn: j.next_usn,
                l2_usn: self
                    .store
                    .as_ref()
                    .map(|s| s.l2_cursor(*l))
                    .unwrap_or(0),
            })
            .collect();
        out.sort_by_key(|c| c.letter);
        Resp::Cursors(out)
    }
}

/// 管道服务端:一个接受循环 + 每连接一个线程。
#[cfg(windows)]
fn ipc_loop(e: Arc<Engine>) {
    use std::os::windows::io::FromRawHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, LocalFree, ERROR_PIPE_CONNECTED, FALSE, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    /// SDDL_REVISION_1(windows crate 没导出这个常量,值就是 1)。
    const SDDL_REVISION_1: u32 = 1;

    let name = crate::win::to_wide(ipc::PIPE_NAME);
    let sddl = crate::win::to_wide(ipc::PIPE_SDDL);
    // 安全描述符:见 ipc::PIPE_SDDL。转不出来就**不要**退回默认 DACL ——
    // 默认是"仅创建者",UI 连不上,而我们会以为服务起来了。
    let mut sd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    // SAFETY: sd 是本函数持有的输出指针,失败时保持 null,成功时由末尾 LocalFree 释放;
    // 宽串在本函数作用域内一直有效。
    let conv = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
    };
    if conv.is_err() || sd.0.is_null() {
        e.log.line(&format!(
            "管道安全描述符转换失败({}),索引服务不提供 IPC",
            conv.err().map(|x| x.to_string()).unwrap_or_default()
        ));
        return;
    }
    e.log.line(&format!("IPC 监听 {}", ipc::PIPE_NAME));
    while !e.stop.load(Ordering::SeqCst) {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: FALSE,
        };
        // SAFETY: 管道名为 NUL 结尾宽串;sa 指向本迭代内的有效结构;
        // 同步模式(PIPE_WAIT),失败返回 INVALID_HANDLE_VALUE。
        let h = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                Some(&sa),
            )
        };
        if h.is_invalid() {
            let err = std::io::Error::last_os_error();
            e.log.line(&format!("建管道失败:{err};1s 后重试"));
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        // SAFETY: h 是本函数刚建的管道实例,overlapped 传 NULL = 同步等待。
        let acc = unsafe { ConnectNamedPipe(h, None) };
        match acc {
            Ok(()) => {}
            // 客户端在我们 Connect 之前就连上了,这是成功路径,不是错误。
            Err(err) if err.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
            Err(err) => {
                e.log.line(&format!("等待管道连接失败:{err}"));
                // SAFETY: 句柄由本函数创建,连接失败时必须自己关。
                unsafe {
                    let _ = CloseHandle(h);
                }
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        }
        let ec = e.clone();
        // HANDLE 里是裸指针,不是 Send;跨线程只搬它的数值,到新线程再还原。
        let raw = h.0 as usize;
        std::thread::spawn(move || {
            // SAFETY: 句柄已由上面的 ConnectNamedPipe 移交,交给 File 负责关闭。
            let mut wf =
                unsafe { std::fs::File::from_raw_handle(raw as *mut std::ffi::c_void) };
            // 读写各要一个 &mut,复制一份句柄(DuplicateHandle)最省事。
            let Ok(rf) = wf.try_clone() else { return };
            let mut rf = std::io::BufReader::new(rf);
            // 一条连接上多轮问答;对端关掉即返回。出错只记日志,不影响别的连接。
            if let Err(err) = ipc::serve_conn(&mut rf, &mut wf, ec.as_ref()) {
                ec.log.line(&format!("IPC 会话异常结束:{err}"));
            }
        });
    }
    // SAFETY: 安全描述符由 ConvertStringSecurityDescriptorToSecurityDescriptorW 分配。
    unsafe {
        let _ = LocalFree(HLOCAL(sd.0));
    }
    e.log.line("IPC 已停止");
}

#[cfg(not(windows))]
fn ipc_loop(e: Arc<Engine>) {
    e.log.line("非 Windows 平台:不提供命名管道 IPC");
    while !e.stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(500));
    }
}

// ---------------------------------------------------------------- 进程入口

/// `--console`:同码当普通控制台进程跑(开发/排障用,不需要 SCM)。
pub fn run_console() -> Result<()> {
    install_ctrl_handler();
    let e = Engine::start(true);
    e.log.line("控制台模式:索引引擎已启动(Ctrl+C 退出)");
    while !e.stop.load(Ordering::SeqCst) && !ctrl_stop() {
        std::thread::sleep(Duration::from_millis(300));
    }
    e.shutdown();
    Ok(())
}

/// `--service`:交给 SCM 分派(阻塞到服务被停止)。
#[cfg(windows)]
pub fn run_service() -> Result<()> {
    // 必须在任何数据目录解析之前声明服务形态:LocalSystem 的「用户目录」
    // 是 systemprofile,不声明就会把全盘库建到系统盘的账号目录里。
    crate::db::enter_service_mode();
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| anyhow::anyhow!("服务分派失败:{e}"))
}

#[cfg(not(windows))]
pub fn run_service() -> Result<()> {
    anyhow::bail!("Windows 服务仅支持 Windows")
}

#[cfg(windows)]
windows_service::define_windows_service!(ffi_service_main, service_main);

#[cfg(windows)]
fn service_main(_arguments: Vec<std::ffi::OsString>) {
    if let Err(e) = service_body() {
        // SCM 已经把状态标成 STOPPED 了,这里只能留下痕迹。
        eprintln!("索引服务异常退出:{e:#}");
    }
}

#[cfg(windows)]
fn service_body() -> Result<()> {
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let handler = move |c: ServiceControl| -> ServiceControlHandlerResult {
        match c {
            ServiceControl::Stop | ServiceControl::Shutdown | ServiceControl::Preshutdown => {
                let _ = tx.send(());
                ServiceControlHandlerResult::NoError
            }
            // 所有服务都必须接受 Interrogate,哪怕是空操作。
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status = service_control_handler::register(SERVICE_NAME, handler)
        .map_err(|e| anyhow::anyhow!("注册服务控制处理器失败:{e}"))?;
    let mk = |state: ServiceState, accept: ServiceControlAccept, checkpoint: u32| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint,
        wait_hint: Duration::from_secs(30),
        // None = 让 windows-service 填当前进程 ID(SERVICE_CONTROL_INTERROGATE 需要)。
        process_id: None,
    };
    status
        .set_service_status(mk(ServiceState::StartPending, ServiceControlAccept::empty(), 1))
        .map_err(|e| anyhow::anyhow!("上报 StartPending 失败:{e}"))?;
    let e = Engine::start(false);
    status
        .set_service_status(mk(
            ServiceState::Running,
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            0,
        ))
        .map_err(|e2| anyhow::anyhow!("上报 Running 失败:{e2}"))?;
    // 阻塞到 SCM 发停止/关机。
    let _ = rx.recv();
    status
        .set_service_status(mk(ServiceState::StopPending, ServiceControlAccept::empty(), 1))
        .ok();
    e.shutdown();
    status
        .set_service_status(mk(ServiceState::Stopped, ServiceControlAccept::empty(), 0))
        .ok();
    // 保鲜线程可能还阻塞在内核的 USN IOCTL 里(取消不了)。索引已存盘、
    // 状态已上报,直接结束进程比让 SCM 等到超时更干净。
    std::process::exit(0);
}

/// 控制台模式的 Ctrl+C 位。
#[cfg(windows)]
static CTRL_STOP: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
fn ctrl_stop() -> bool {
    CTRL_STOP.load(Ordering::SeqCst)
}

#[cfg(not(windows))]
fn ctrl_stop() -> bool {
    false
}

/// 挂 Ctrl+C 处理器:控制台模式也要走存盘再退,不然索引文件白丢一轮。
#[cfg(windows)]
fn install_ctrl_handler() {
    use windows::Win32::Foundation::{BOOL, TRUE};
    use windows::Win32::System::Console::SetConsoleCtrlHandler;

    // SAFETY: 处理器是进程存活期内一直有效的 `extern "system"` 函数指针。
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(ctrl_trampoline), TRUE);
    }

    unsafe extern "system" fn ctrl_trampoline(_kind: u32) -> BOOL {
        CTRL_STOP.store(true, Ordering::SeqCst);
        // 返回 FALSE:让默认处理接着结束进程;主循环看到停止位后会先存盘。
        BOOL(0)
    }
}

#[cfg(not(windows))]
fn install_ctrl_handler() {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nameidx::VolumeIndexBuilder;

    /// 造一个只含内存索引的引擎(不连库、不起线程),用来测 `Query` 实现。
    fn engine_with_vols(vols: Vec<VolumeIndex>) -> Engine {
        // 每个引擎一个独立 blob 名:测试是并发跑的,共用的话
        // 「落盘只写注入目标」那条会随机被别的用例删掉文件。
        static BLOB_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let meta = vols
            .iter()
            .map(|v| VolMeta {
                letter: v.letter,
                support: Support::Usn,
                why: String::new(),
                building: false,
                degraded: false,
            })
            .collect();
        Engine {
            state: Mutex::new(State {
                vols,
                meta,
                journals: HashMap::new(),
                queue: VecDeque::new(),
                fresh: HashSet::new(),
            }),
            cv: Condvar::new(),
            store: None,
            ctrl: crate::scan::ScanControl::new(),
            stop: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            started: Instant::now(),
            last_save: Mutex::new(None),
            // 落盘目标指到临时文件:测试里的 `shutdown()` 会真写一遍索引,
            // 指到 `index_path()` 就是盖掉本机用户的真索引。
            blob: std::env::temp_dir().join(format!(
                "rk-blob-{}-{}.bin",
                std::process::id(),
                BLOB_SEQ.fetch_add(1, Ordering::SeqCst)
            )),
            log: Log {
                f: Mutex::new(None),
                echo: false,
            },
        }
    }

    fn push(b: &mut VolumeIndexBuilder, frn: u64, parent: u64, attrs: u32, name: &str) {
        let units: Vec<u16> = name.encode_utf16().collect();
        let e = mft::UsnEntry {
            frn,
            parent_frn: parent,
            attrs,
            reason: 0,
            usn: 0,
            name: &units,
            timestamp: 0,
        };
        b.push_usn(&e);
    }

    fn sample_vol(letter: char) -> VolumeIndex {
        let mut b = VolumeIndexBuilder::new(letter);
        push(&mut b, 10, mft::ROOT_FRN_INDEX, nameidx::ATTR_DIRECTORY, "Users");
        push(&mut b, 100, 10, 0, "photo.jpg");
        push(&mut b, 101, 10, 0, "notes.txt");
        b.finish()
    }

    /// 空引擎:所有查询都要安全返回,不许 panic(服务面对的是外部请求)。
    #[test]
    fn empty_engine_answers_safely() {
        let e = engine_with_vols(Vec::new());
        assert!(matches!(
            e.hello(),
            Resp::Hello {
                proto: ipc::PROTO_VERSION,
                volumes: 0,
                entries: 0
            }
        ));
        assert_eq!(e.search("photo", 10, 0), Resp::Hits(Vec::new()));
        assert_eq!(e.path_of('C', 0), Resp::Path(None));
        assert_eq!(e.children_of('C', 0), Resp::Children(Vec::new()));
        match e.stat(r"C:\x.jpg") {
            Resp::Stat(s) => assert!(!s.exists),
            other => panic!("期望 Stat,得到 {other:?}"),
        }
        match e.status() {
            Resp::Status(s) => assert!(s.vols.is_empty()),
            other => panic!("期望 Status,得到 {other:?}"),
        }
        assert_eq!(e.cursors(), Resp::Cursors(Vec::new()));
    }

    /// 越界槽位必须被挡住:`VolumeIndex` 的访问器不做边界检查,
    /// 一个坏 idx 就能把服务打崩(而服务是常驻提权进程)。
    #[test]
    fn out_of_range_slots_are_rejected() {
        let e = engine_with_vols(vec![sample_vol('C')]);
        assert_eq!(e.path_of('C', 9999), Resp::Path(None));
        assert_eq!(e.children_of('C', 9999), Resp::Children(Vec::new()));
        // idx == count() 是卷根虚拟桶,合法。
        // idx == count() 是卷根虚拟桶:里面只有 Users。
        match e.children_of('C', 3) {
            Resp::Children(c) => assert_eq!(c.len(), 1),
            other => panic!("期望 Children,得到 {other:?}"),
        }
        match e.search("photo", 10, 0) {
            Resp::Hits(h) => {
                assert_eq!(h.len(), 1);
                assert_eq!(h[0].path, r"C:\Users\photo.jpg");
                assert!(!h[0].is_dir);
            }
            other => panic!("期望 Hits,得到 {other:?}"),
        }
    }

    /// `Stat` 走 `lookup_path`:命中回条目,未命中/串卷/卷根都回"不存在"。
    #[test]
    fn stat_resolves_paths() {
        let e = engine_with_vols(vec![sample_vol('C')]);
        match e.stat(r"C:\Users\photo.jpg") {
            Resp::Stat(s) => {
                assert!(s.exists);
                assert_eq!(s.idx, 1);
                assert_eq!(s.letter, 'C');
                assert!(!s.is_dir);
            }
            other => panic!("期望 Stat,得到 {other:?}"),
        }
        // 大小写与分隔符不敏感。
        assert!(matches!(
            e.stat(r"c:/users/PHOTO.JPG"),
            Resp::Stat(StatInfo { exists: true, .. })
        ));
        // 目录也算命中。
        assert!(matches!(
            e.stat(r"C:\Users"),
            Resp::Stat(StatInfo {
                exists: true,
                is_dir: true,
                ..
            })
        ));
        for miss in [
            r"C:\Users\gone.jpg",
            r"D:\Users\photo.jpg",
            r"C:\",
            "photo.jpg",
            "",
        ] {
            assert!(
                matches!(e.stat(miss), Resp::Stat(StatInfo { exists: false, .. })),
                "{miss} 应判不存在"
            );
        }
    }

    /// 类别过滤在服务端生效,`limit` 被收口到 [1,5000]。
    #[test]
    fn search_applies_class_and_limit() {
        let e = engine_with_vols(vec![sample_vol('C')]);
        match e.search("o", 10, ipc::class_to_wire(crate::nameidx::FileClass::Image)) {
            Resp::Hits(h) => {
                assert_eq!(h.len(), 1);
                assert_eq!(h[0].name, "photo.jpg");
            }
            other => panic!("期望 Hits,得到 {other:?}"),
        }
        match e.search("o", 10, ipc::class_to_wire(crate::nameidx::FileClass::Document)) {
            Resp::Hits(h) => assert_eq!(h.len(), 1),
            other => panic!("期望 Hits,得到 {other:?}"),
        }
        // limit=0 → 收成 1,而不是 0 条或无界。
        match e.search("photo", 0, 0) {
            Resp::Hits(h) => assert_eq!(h.len(), 1),
            other => panic!("期望 Hits,得到 {other:?}"),
        }
        match e.search("photo", u32::MAX, 0) {
            Resp::Hits(h) => assert_eq!(h.len(), 1),
            other => panic!("期望 Hits,得到 {other:?}"),
        }
        // 空词不查(与 nameidx::search_class 同口径)。
        assert_eq!(e.search("   ", 10, 0), Resp::Hits(Vec::new()));
    }

    /// 结果投影把 L2 的派生状态补上。
    ///
    /// 钉三件事:没有库时全部是哨兵;入库的行按 `rich_state` 回真值;
    /// **没入库的行必须留在哨兵上** —— `RICH_UNKNOWN` 与 `RICH_OUT`
    /// 语义不同(「快查看得见但图库不收」vs「图库收了但不派生」),
    /// 混成一个值 UI 就会把非媒体文件也标成「未纳入范围」。
    #[test]
    fn search_projects_rich_state_from_l2() {
        let mut e = engine_with_vols(vec![sample_vol('C')]);
        let hits = match e.search("o", 10, 0) {
            Resp::Hits(h) => h,
            other => panic!("期望 Hits,得到 {other:?}"),
        };
        assert!(
            hits.iter().all(|h| h.rich_state == ipc::RICH_UNKNOWN),
            "没连库时不该编出任何派生状态"
        );
        let jpg = hits
            .iter()
            .find(|h| h.name == "photo.jpg")
            .map(|h| h.path.clone())
            .expect("样本卷里应有 photo.jpg");
        let txt = hits
            .iter()
            .find(|h| h.name == "notes.txt")
            .map(|h| h.path.clone())
            .expect("样本卷里应有 notes.txt");

        let store = Arc::new(crate::db::Store::open_mem().unwrap());
        {
            let c = store.test_conn();
            // 只把 photo.jpg 收进 L2;notes.txt 故意不收进索引。
            c.execute(
                "INSERT INTO photos(path,parent,mtime,size,rich_state)
                 VALUES(?1,'C:\\Users',0,1,?2)",
                rusqlite::params![jpg, crate::db::RICH_PENDING],
            )
            .unwrap();
        }
        e.store = Some(store);

        let hits = match e.search("o", 10, 0) {
            Resp::Hits(h) => h,
            other => panic!("期望 Hits,得到 {other:?}"),
        };
        let state_of = |name: &str| {
            hits.iter()
                .find(|h| h.name == name)
                .map(|h| h.rich_state)
                .unwrap_or_else(|| panic!("结果里应有 {name}"))
        };
        assert_eq!(state_of("photo.jpg"), crate::db::RICH_PENDING as u8);
        assert_eq!(state_of("notes.txt"), ipc::RICH_UNKNOWN);
        // 哨兵必须与四个真实状态都不撞车,否则上面那条断言是空的。
        for real in [
            crate::db::RICH_OUT,
            crate::db::RICH_PENDING,
            crate::db::RICH_DONE,
            crate::db::RICH_BAD,
        ] {
            assert_ne!(i32::from(ipc::RICH_UNKNOWN), real);
        }

        // 再把 notes.txt 以 `RICH_OUT` 入库:它必须变成 0 而不是留在哨兵上 ——
        // 「图库收了但不派生」与「图库没收」在 UI 上是两句不同的话。
        {
            let c = e.store.as_ref().unwrap().test_conn();
            c.execute(
                "INSERT INTO photos(path,parent,mtime,size,rich_state)
                 VALUES(?1,'C:\\Users',0,1,?2)",
                rusqlite::params![txt, crate::db::RICH_OUT],
            )
            .unwrap();
        }
        match e.search("o", 10, 0) {
            Resp::Hits(h) => {
                let n = h.iter().find(|x| x.name == "notes.txt").unwrap();
                assert_eq!(n.rich_state, crate::db::RICH_OUT as u8);
            }
            other => panic!("期望 Hits,得到 {other:?}"),
        }
    }

    /// 目录命中不查 L2:`photos` 里只有文件行,目录一律哨兵。
    #[test]
    fn search_leaves_directories_at_the_sentinel() {
        let mut e = engine_with_vols(vec![sample_vol('C')]);
        let store = Arc::new(crate::db::Store::open_mem().unwrap());
        e.store = Some(store);
        match e.search("Users", 10, 0) {
            Resp::Hits(h) => {
                let dir = h.iter().find(|x| x.is_dir).expect("Users 是目录");
                assert_eq!(dir.rich_state, ipc::RICH_UNKNOWN);
            }
            other => panic!("期望 Hits,得到 {other:?}"),
        }
    }

    /// 重建请求进队列并去重;空列表 = 全部 Usn 卷。
    #[test]
    fn rebuild_requests_are_queued_and_deduped() {
        let e = engine_with_vols(vec![sample_vol('C'), sample_vol('D')]);
        e.request_rebuild(&['C']);
        e.request_rebuild(&['C']);
        {
            let st = e.lock();
            assert_eq!(st.queue.len(), 1);
            assert_eq!(st.queue[0], Job::Rebuild('C'));
        }
        // 空列表 = 全部 Usn 卷 ['C','D'],但 C 已在队列里,去重后只补 D。
        e.request_rebuild(&[]);
        {
            let st = e.lock();
            assert_eq!(st.queue.len(), 2);
        }
        assert_eq!(e.rebuild(&['D']), Resp::Ok);
    }

    /// 增量一轮:L1 并入 + checkpoint 前移 + 脏标记置起。
    #[test]
    fn apply_step_advances_index_and_checkpoint() {
        let e = engine_with_vols(vec![sample_vol('C')]);
        assert!(!e.dirty.load(Ordering::SeqCst));
        let ops = vec![nameidx::DeltaOp::Upsert(nameidx::DeltaUpsert {
            frn: 200,
            parent_frn: 10,
            attrs: 0,
            name: "added.jpg".into(),
            size: 4242,
            mtime: 1_700_000_000_000_000,
            mtime_usn: 0,
        })];
        e.apply_step('C', 0xABCD, 0, ops, 999, 1);
        {
            let st = e.lock();
            let v = st.vols.iter().find(|v| v.letter == 'C').unwrap();
            assert_eq!(v.count(), 4);
            let i = v.lookup_path(r"C:\Users\added.jpg").unwrap();
            assert_eq!(v.size_of(i), 4242);
            assert_eq!(st.journals.get(&'C').unwrap().next_usn, 999);
        }
        assert!(e.dirty.load(Ordering::SeqCst));
        // 游标随之前移,L2 位在没有库时为 0。
        match e.cursors() {
            Resp::Cursors(c) => {
                assert_eq!(c.len(), 1);
                assert_eq!(c[0].next_usn, 999);
                assert_eq!(c[0].journal_id, 0xABCD);
                assert_eq!(c[0].l2_usn, 0);
            }
            other => panic!("期望 Cursors,得到 {other:?}"),
        }
    }

    /// 空增量不置脏:机器闲置时不该触发 56 MB 的重写。
    #[test]
    fn empty_delta_does_not_mark_dirty() {
        let e = engine_with_vols(vec![sample_vol('C')]);
        e.apply_step('C', 1, 0, Vec::new(), 5, 0);
        assert!(!e.dirty.load(Ordering::SeqCst));
    }

    /// 关停置停止位并取消建库(建库线程会在下一个检查点退出)。
    #[test]
    fn shutdown_sets_stop_and_cancels_build() {
        let e = engine_with_vols(vec![sample_vol('C')]);
        // 没有数据目录可写时 save_now 只记日志,不该 panic。
        e.shutdown();
        assert!(e.stop.load(Ordering::SeqCst));
        assert!(e.ctrl.is_cancelled());
    }

    /// 落盘只写构造方注入的目标,**绝不碰 `index_path()`**。
    /// 这条不变量防止 `shutdown()`→`save_now()` 现取 `index_path()`,
    /// 把样例记录写到真机索引路径上。
    #[test]
    fn save_now_writes_only_the_injected_blob() {
        let e = engine_with_vols(vec![sample_vol('C')]);
        let _ = std::fs::remove_file(&e.blob);
        e.save_now("单测");
        assert!(e.blob.exists(), "应写进注入的临时 blob:{:?}", e.blob);
        let back = nameidx::load(&e.blob).expect("刚落盘的索引要读得回来");
        assert_eq!(back.vols[0].letter, 'C');
        assert_eq!(back.vols[0].count(), 3);
        let _ = std::fs::remove_file(&e.blob);
    }

    /// `ensure_vol` 复用已有槽位,不重复插入同卷。
    #[test]
    fn ensure_vol_reuses_existing_slot() {
        let mut st = State::default();
        let a = ensure_vol(&mut st, 'C');
        let b = ensure_vol(&mut st, 'C');
        let d = ensure_vol(&mut st, 'D');
        assert_eq!(a, b);
        assert_eq!(d, 1);
        assert_eq!(st.vols.len(), 2);
        assert_eq!(st.meta.len(), 2);
    }
}
