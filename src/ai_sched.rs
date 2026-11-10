//! AI 调度:三阶段 worker、模型会话懒加载、暂停/恢复/换模型与统计快照。

use crate::ai_queue::{self, Stage};
use crate::db::Store;
use anyhow::Result;
use rusqlite::params;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// 队列空闲时的等待间隔。
const IDLE_SLEEP: Duration = Duration::from_millis(200);
/// 模型未就绪时的等待间隔。
const NOT_READY_SLEEP: Duration = Duration::from_secs(2);
/// 统计快照的最短刷新间隔。
const STATS_TTL: Duration = Duration::from_secs(2);
/// 换模型时等待当前批结束的上限。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// 单阶段统计。
#[derive(Debug, Clone, Copy, Default)]
pub struct AiStats {
    pub pending: u64,
    pub done: u64,
    pub failed: u64,
}

/// 阶段与其统计。
#[derive(Debug, Clone)]
pub struct StageStats {
    pub stage: Stage,
    pub stats: AiStats,
}

/// 暂停闸门:worker 在批边界等这里。
struct Gate {
    state: Mutex<GateState>,
    cv: Condvar,
}

struct GateState {
    paused: bool,
    maintenance: bool,
}

struct Maintenance<'a>(&'a Gate);

impl Drop for Maintenance<'_> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().maintenance = false;
        self.0.cv.notify_all();
    }
}

impl Gate {
    fn new(paused: bool) -> Self {
        Gate {
            state: Mutex::new(GateState { paused, maintenance: false }),
            cv: Condvar::new(),
        }
    }

    /// 等到暂停和维护都结束。
    fn wait(&self) {
        let mut state = self.state.lock().unwrap();
        while state.paused || state.maintenance {
            state = self.cv.wait(state).unwrap();
        }
    }

    fn enter(&self, busy: &Busy, quit: &AtomicBool) -> bool {
        let mut state = self.state.lock().unwrap();
        while (state.paused || state.maintenance) && !quit.load(Ordering::Relaxed) {
            state = self.cv.wait(state).unwrap();
        }
        if quit.load(Ordering::Relaxed) {
            return false;
        }
        busy.enter();
        true
    }

    fn maintain(&self) -> Result<Maintenance<'_>, String> {
        let mut state = self.state.lock().unwrap();
        if state.maintenance {
            return Err(crate::i18n::t("已有 AI 维护任务正在运行").into());
        }
        state.maintenance = true;
        Ok(Maintenance(self))
    }

    fn set(&self, paused: bool) {
        self.state.lock().unwrap().paused = paused;
        self.cv.notify_all();
    }

    fn is_paused(&self) -> bool {
        self.state.lock().unwrap().paused
    }
}

/// 批次占用计数(换模型前等待归零)。
struct Busy {
    active: Mutex<usize>,
    cv: Condvar,
}

impl Busy {
    fn new() -> Self {
        Busy {
            active: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    fn enter(&self) {
        *self.active.lock().unwrap() += 1;
    }

    fn leave(&self) {
        let mut n = self.active.lock().unwrap();
        *n = n.saturating_sub(1);
        self.cv.notify_all();
    }

    fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut n = self.active.lock().unwrap();
        while *n > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let (guard, _) = self.cv.wait_timeout(n, left).unwrap();
            n = guard;
        }
        *n == 0
    }
}

/// 全局并发许可,值为可用槽位数。
struct Semaphore {
    free: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new(n: usize) -> Self {
        Semaphore {
            free: Mutex::new(n.max(1)),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self) {
        let mut n = self.free.lock().unwrap();
        while *n == 0 {
            n = self.cv.wait(n).unwrap();
        }
        *n -= 1;
    }

    fn release(&self) {
        let mut n = self.free.lock().unwrap();
        *n += 1;
        self.cv.notify_one();
    }
}

/// 单条任务的结果:跳过不记失败,也不出队。
enum Outcome {
    Ok,
    Skip(String),
    Fail(String),
}

/// 每阶段私有的模型会话与缓存。
#[derive(Default)]
struct Runner {
    vision: Option<crate::clip::MobileClipVision>,
    text: Option<crate::clip::MobileClipText>,
    ocr: Option<crate::ai::ocr::Ocr>,
    label_vecs: Option<(String, Vec<(i64, Vec<f32>)>)>,
    model_sig: String,
}

impl Runner {
    fn clear(&mut self) {
        self.vision = None;
        self.text = None;
        self.ocr = None;
        self.label_vecs = None;
        self.model_sig.clear();
    }

    fn prepare(&mut self, signature: String) {
        if self.model_sig != signature {
            self.clear();
            self.model_sig = signature;
        }
    }

    fn vision(&mut self, store: &Store) -> Result<&mut crate::clip::MobileClipVision, String> {
        if self.vision.is_none() {
            self.vision = Some(crate::clip::MobileClipVision::load(store)?);
        }
        Ok(self.vision.as_mut().unwrap())
    }

    fn text(&mut self, store: &Store) -> Result<&mut crate::clip::MobileClipText, String> {
        if self.text.is_none() {
            self.text = Some(crate::clip::MobileClipText::load(store)?);
        }
        Ok(self.text.as_mut().unwrap())
    }

    fn ocr(&mut self, store: &Store) -> Result<&mut crate::ai::ocr::Ocr, String> {
        if self.ocr.is_none() {
            self.ocr = Some(crate::ai::ocr::Ocr::load(store)?);
        }
        Ok(self.ocr.as_mut().unwrap())
    }

    /// 标签文本向量按词表签名缓存;词表内容变化时重算。
    fn label_vectors(&mut self, store: &Store) -> Result<&Vec<(i64, Vec<f32>)>, String> {
        let ver = format!("{}|{}", self.model_sig, crate::ai_tags::vocab_version(store));
        let stale = self
            .label_vecs
            .as_ref()
            .map(|(v, _)| v != &ver)
            .unwrap_or(true);
        if stale {
            let labels = crate::ai_tags::labels(store);
            let mut out = Vec::with_capacity(labels.len());
            for l in labels {
                let t = self.text(store)?;
                let v = t.embed_text(&l.prompt)?;
                out.push((l.id, v));
            }
            self.label_vecs = Some((ver, out));
        }
        Ok(&self.label_vecs.as_ref().unwrap().1)
    }
}

fn clip_image(store: &Store, photo_id: i64) -> Result<image::DynamicImage, String> {
    let src = store.read(|conn| {
        conn.query_row("SELECT path FROM photos WHERE id=?1", [photo_id], |r| {
            r.get::<_, String>(0)
        })
    }).map_err(|e| e.to_string())?;
    crate::thumbs::load_for_ai(std::path::Path::new(&src), 1024).map_err(|e| e.to_string())
}

fn execute_clip(store: &Store, runner: &mut Runner, photo_id: i64, ver: &str) -> Outcome {
    let img = match clip_image(store, photo_id) {
        Ok(i) => i,
        Err(e) => return Outcome::Skip(e),
    };
    let emb = match runner.vision(store).and_then(|v| v.embed_image(&img)) {
        Ok(e) => e,
        Err(e) => return Outcome::Fail(e),
    };
    if let Err(e) = crate::ai_search::save_vec(store, photo_id, ver, &emb) {
        return Outcome::Fail(e);
    }
    let _ = ai_queue::enqueue(store, Stage::Tags, photo_id, ai_queue::P_DEFAULT);
    Outcome::Ok
}

fn execute_tags(store: &Store, runner: &mut Runner, photo_id: i64, tag_ver: &str) -> Outcome {
    let clip_ver = ai_queue::stage_version(store, Stage::Clip);
    let emb = match crate::ai_search::load_vec(store, photo_id, &clip_ver) {
        Some(v) => v,
        None => return Outcome::Skip("缺语义向量".into()),
    };
    let (threshold, margin) = crate::ai_tags::thresholds(crate::ai_tags::sensitivity(store));
    let scores = {
        let labels = match runner.label_vectors(store) {
            Ok(v) => v,
            Err(e) => return Outcome::Fail(e),
        };
        let s: Vec<(i64, f32)> = labels
            .iter()
            .filter(|(_, v)| v.len() == emb.len())
            .map(|(id, v)| (*id, crate::ai_tags::cosine(&emb, v)))
            .collect();
        let catalog = crate::ai_tags::labels(store);
        crate::ai_tags::select(&s, &catalog, threshold, margin)
    };
    if let Err(e) = crate::ai_tags::save(store, photo_id, tag_ver, &scores) {
        return Outcome::Fail(e);
    }
    let _ = crate::ai_caption::generate(store, photo_id, tag_ver, false);
    if crate::ai_ocr::should_auto(store, photo_id, tag_ver) {
        let _ = ai_queue::enqueue(store, Stage::Ocr, photo_id, ai_queue::P_DEFAULT);
    }
    Outcome::Ok
}

fn execute_ocr(
    store: &Store,
    runner: &mut Runner,
    photo_id: i64,
    ver: &str,
    priority: i32,
) -> Outcome {
    let tag_ver = ai_queue::stage_version(store, Stage::Tags);
    let manual = priority == ai_queue::P_CURRENT;
    match crate::ai_ocr::mode(store) {
        crate::ai_ocr::Mode::Off if !manual => return Outcome::Skip("OCR 已关闭".into()),
        crate::ai_ocr::Mode::Selected if !manual => return Outcome::Skip("仅手动识别".into()),
        crate::ai_ocr::Mode::Auto
            if !manual && !crate::ai_tags::is_text_photo(store, photo_id, &tag_ver) =>
        {
            // 自动模式判定为非文字照片:记完成,不重复排队。
            return Outcome::Ok;
        }
        _ => {}
    }
    let img = match crate::thumbs::load_cached_tier(crate::thumbs::Tier::Preview, photo_id) {
        Some(i) => i,
        None => {
            // 文档/票据类 1024 不足,回退原图并限制长边。
            let path = store.read(|conn| {
                conn.query_row("SELECT path FROM photos WHERE id=?1", [photo_id], |r| {
                    r.get::<_, String>(0)
                })
                .ok()
            });
            match path.and_then(|p| image::open(p).ok()) {
                Some(i) => {
                    let (w, h) = (i.width(), i.height());
                    if w.max(h) > 2048 {
                        let s = 2048.0 / w.max(h) as f32;
                        i.resize(
                            (w as f32 * s) as u32,
                            (h as f32 * s) as u32,
                            image::imageops::FilterType::Triangle,
                        )
                    } else {
                        i
                    }
                }
                None => return Outcome::Skip("缺预览图".into()),
            }
        }
    };
    let lines = match runner.ocr(store).and_then(|o| o.extract(&img)) {
        Ok(v) => v,
        Err(e) => return Outcome::Fail(e),
    };
    let text = lines
        .iter()
        .map(|t| t.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let boxes: Vec<crate::ai_ocr::LineBox> = lines
        .iter()
        .map(|t| crate::ai_ocr::LineBox {
            x0: t.x0,
            y0: t.y0,
            x1: t.x1,
            y1: t.y1,
            conf: t.conf,
        })
        .collect();
    if let Err(e) = crate::ai_ocr::save(store, photo_id, &text, &boxes, ver) {
        return Outcome::Fail(e);
    }
    let _ = crate::ai_caption::generate(store, photo_id, &tag_ver, false);
    Outcome::Ok
}

fn execute(
    stage: Stage,
    store: &Store,
    runner: &mut Runner,
    photo_id: i64,
    ver: &str,
    priority: i32,
) -> Outcome {
    match stage {
        Stage::Clip => execute_clip(store, runner, photo_id, ver),
        Stage::Tags => execute_tags(store, runner, photo_id, ver),
        Stage::Ocr => execute_ocr(store, runner, photo_id, ver, priority),
    }
}

/// 统计某阶段的本版待办、完成与失败数。
fn snapshot_stage(store: &Store, stage: Stage) -> AiStats {
    let ver = ai_queue::stage_version(store, stage);
    store.read(|conn| {
        let pending = conn
            .query_row(
                "SELECT COUNT(*) FROM ai_jobs j
                  WHERE j.stage = ?1
                    AND NOT EXISTS(
                        SELECT 1 FROM ai_state s
                         WHERE s.photo_id = j.photo_id AND s.stage = j.stage AND s.model_ver = ?2
                    )",
                params![stage.as_str(), ver],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0);
        let done = conn
            .query_row(
                "SELECT COUNT(*) FROM ai_state WHERE stage = ?1 AND model_ver = ?2",
                params![stage.as_str(), ver],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0);
        let failed = conn
            .query_row(
                "SELECT COUNT(*) FROM ai_failed WHERE stage = ?1",
                [stage.as_str()],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0);
        AiStats {
            pending: pending as u64,
            done: done as u64,
            failed: failed as u64,
        }
    })
}

fn worker(
    store: Arc<Store>,
    stage: Stage,
    gate: Arc<Gate>,
    quit: Arc<AtomicBool>,
    sem: Arc<Semaphore>,
    busy: Arc<Busy>,
    release: Arc<AtomicBool>,
) {
    crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);
    let mut runner = Runner::default();
    loop {
        if quit.load(Ordering::Relaxed) {
            break;
        }
        gate.wait();
        if quit.load(Ordering::Relaxed) {
            break;
        }
        if release.swap(false, Ordering::AcqRel) {
            runner.clear();
        }
        if !crate::ai_models::stage_ready(&store, stage) {
            runner.clear();
            std::thread::sleep(NOT_READY_SLEEP);
            continue;
        }
        let ver = ai_queue::stage_version(&store, stage);
        let _ = ai_queue::purge_done(&store, stage, &ver);
        busy.enter();
        let batch = ai_queue::pick_batch(&store, stage, &ver, ai_queue::BATCH_LIMIT);
        if batch.is_empty() {
            busy.leave();
            std::thread::sleep(IDLE_SLEEP);
            continue;
        }
        if release.swap(false, Ordering::AcqRel) {
            runner.clear();
        }
        let mut ok: Vec<i64> = Vec::new();
        let mut failed: Vec<(i64, i32, String)> = Vec::new();
        let mut skipped: Vec<i64> = Vec::new();
        for job in batch {
            if quit.load(Ordering::Relaxed) {
                break;
            }
            sem.acquire();
            let outcome = execute(stage, &store, &mut runner, job.photo_id, &ver, job.priority);
            sem.release();
            match outcome {
                Outcome::Ok => ok.push(job.photo_id),
                Outcome::Skip(_) => skipped.push(job.photo_id),
                Outcome::Fail(e) => failed.push((job.photo_id, job.attempts + 1, e)),
            }
        }
        let _ = ai_queue::defer_batch(&store, stage, &skipped);
        let _ = ai_queue::commit_batch(&store, stage, &ver, &ok, &failed);
        busy.leave();
        gate.wait();
    }
}

/// 调度器句柄,离开作用域时停掉全部 worker。
pub struct AiSched {
    gate: Arc<Gate>,
    quit: Arc<AtomicBool>,
    busy: Arc<Busy>,
    releases: Vec<Arc<AtomicBool>>,
    handles: Vec<JoinHandle<()>>,
    stats: Mutex<Option<(Instant, Vec<StageStats>)>>,
}

impl AiSched {
    /// 启动三阶段 worker;`enabled=false` 时先停在暂停态。
    pub fn spawn(store: Arc<Store>, enabled: bool) -> Self {
        // 服务进程只维护名字索引,不加载模型也不起 worker。
        let service = crate::db::service_mode();
        let enabled = enabled && !service;
        let _ = ai_queue::seed_priority_rules(&store);
        let _ = crate::ai_tags::seed_labels(&store);
        let gate = Arc::new(Gate::new(!enabled));
        let quit = Arc::new(AtomicBool::new(false));
        let busy = Arc::new(Busy::new());
        let permits = (crate::win::cpu_count() / 2).max(1);
        let sem = Arc::new(Semaphore::new(permits));
        let mut handles = Vec::new();
        let mut releases = Vec::new();
        let stages: &[Stage] = if service { &[] } else { &Stage::ALL };
        for &stage in stages {
            let st = store.clone();
            let g = gate.clone();
            let q = quit.clone();
            let s = sem.clone();
            let b = busy.clone();
            let r = Arc::new(AtomicBool::new(false));
            releases.push(r.clone());
            let handle = std::thread::Builder::new()
                .name(format!("ai-{}", stage.as_str()))
                .spawn(move || worker(st, stage, g, q, s, b, r));
            match handle {
                Ok(h) => handles.push(h),
                Err(e) => eprintln!("启动 AI worker 失败: {e}"),
            }
        }
        AiSched {
            gate,
            quit,
            busy,
            releases,
            handles,
            stats: Mutex::new(None),
        }
    }

    /// 暂停取批,已在处理的当前批会跑完。
    pub fn pause(&self) {
        self.gate.set(true);
    }

    /// 恢复取批,从暂停点继续。
    pub fn resume(&self) {
        self.gate.set(false);
    }

    pub fn is_paused(&self) -> bool {
        self.gate.is_paused()
    }

    /// 等待所有阶段当前批结束(不阻塞超过 10 秒)。
    pub fn drain(&self, _stage: Stage) {
        self.busy.wait_idle(DRAIN_TIMEOUT);
    }

    /// 请求该阶段丢弃模型会话(下一轮生效)。
    pub fn release(&self, stage: Stage) {
        if let Some(flag) = Stage::ALL
            .iter()
            .position(|s| *s == stage)
            .and_then(|i| self.releases.get(i))
        {
            flag.store(true, Ordering::Release);
        }
    }

    /// 统计快照(2 秒内复用缓存)。
    pub fn stats(&self, store: &Store) -> Vec<StageStats> {
        let mut guard = self.stats.lock().unwrap();
        if let Some((at, rows)) = guard.as_ref() {
            if at.elapsed() < STATS_TTL {
                return rows.clone();
            }
        }
        let rows: Vec<StageStats> = Stage::ALL
            .into_iter()
            .map(|stage| StageStats {
                stage,
                stats: snapshot_stage(store, stage),
            })
            .collect();
        *guard = Some((Instant::now(), rows.clone()));
        rows
    }
}

impl Drop for AiSched {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        self.gate.set(false);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_queue::{self, Stage};

    #[test]
    fn gate_blocks_and_resumes() {
        let gate = Arc::new(Gate::new(true));
        assert!(gate.is_paused());
        let g = gate.clone();
        let done = Arc::new(AtomicBool::new(false));
        let d = done.clone();
        let h = std::thread::spawn(move || {
            g.wait();
            d.store(true, Ordering::Relaxed);
        });
        std::thread::sleep(Duration::from_millis(80));
        assert!(!done.load(Ordering::Relaxed));
        gate.set(false);
        h.join().unwrap();
        assert!(done.load(Ordering::Relaxed));
    }

    #[test]
    fn stats_count_pending_and_done() {
        let store = Store::open_mem().unwrap();
        store
            .write(|conn| {
                conn.execute(
                    "INSERT INTO photos(id, path, parent, mtime, size) VALUES(1, 'C:\\a.jpg', 'C:\\', 0, 0)",
                    [],
                )
            })
            .unwrap();
        ai_queue::enqueue(&store, Stage::Clip, 1, 1).unwrap();
        let s = snapshot_stage(&store, Stage::Clip);
        assert_eq!(s.pending, 1);
        assert_eq!(s.done, 0);
        ai_queue::commit_batch(&store, Stage::Clip, "v0", &[1], &[]).unwrap();
        let s = snapshot_stage(&store, Stage::Clip);
        assert_eq!(s.pending, 0);
        assert_eq!(s.done, 1);
    }

    #[test]
    fn busy_tracks_batches() {
        let b = Busy::new();
        b.enter();
        b.enter();
        b.leave();
        b.wait_idle(Duration::from_millis(1));
        assert_eq!(*b.active.lock().unwrap(), 1);
        b.leave();
        b.wait_idle(Duration::from_millis(20));
        assert_eq!(*b.active.lock().unwrap(), 0);
    }

    #[test]
    fn replace_model_is_ordered() {
        let store = Arc::new(Store::open_mem().unwrap());
        let sched = AiSched::spawn(store.clone(), false);
        // 无模型时 drain/release 不应阻塞,失败后调度器应恢复可用状态。
        assert!(crate::ai_models::replace_model(&store, &sched, "ocr_det", "v1").is_err());
        assert!(!sched.is_paused(), "替换失败也必须解除暂停");
    }
}
