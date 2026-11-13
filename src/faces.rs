//! 人脸存储 / 增量聚类 / 质量评分 / 命名建议。
//! 能力映射:质量评分、命名建议、忽略簇、算法版本迁移(wipe_faces)。
use crate::ai::face::{Det, FaceAlgo, Faces};
use crate::db::Store;
use anyhow::{bail, Result};
use rusqlite::params;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// 人脸算法版本(写 settings):检测/嵌入或签名契约变了才升,只动聚类策略不升。
pub const FACE_ALGO_VERSION: &str = "v6";

/// 对齐与采样版本(112 模板、五点相似变换、双线性黑边)。
const ALIGN_VERSION: &str = "a2-112-sim-bilinear";
/// 输入预处理版本(EXIF 摆正、检测缩放、通道与归一化、NCHW)。
const INPUT_VERSION: &str = "i2-exif-1280-640-nchw";
/// 检测设置版本(输出布局与 NMS 阈值)。
const DETECT_VERSION: &str = "d2-yunet12-scrfd9-nms04";

/// 聚类归属阈值(按当前方案的嵌入分布定,数值见 FaceAlgo::cluster_thr):
/// AdaFace 系 512 维余弦同人通常 >0.6,异人 <0.3,0.6 起步比 0.5 更不容易
/// 把不同的人并成一簇;SFace 128 维按 OpenCV 1:1 参考阈值 0.363 上调取严。
pub fn cluster_thr(store: &Store) -> f32 {
    current_algo(store).cluster_thr()
}

/// 命名建议阈值(比聚类放宽一档,误报由用户确认时把关)。
pub fn suggestion_thr(store: &Store) -> f32 {
    current_algo(store).suggestion_thr()
}

/// 建议相对第二候选人物需要的最低分数余量。
const SUGGEST_MARGIN: f32 = 0.02;

/// 当前方案的嵌入维度(加载模板时过滤旧方案的向量,防混库比对)。
fn emb_dim(store: &Store) -> i64 {
    current_algo(store).emb_dim() as i64
}

/// 簇成员数达到该值才算「成熟」:质心有足够支撑,按常规阈值吸脸。
pub const CLUSTER_MATURE: usize = 3;

/// 年轻簇(成员 < CLUSTER_MATURE)吸脸的收紧量:单脸/双脸质心支撑弱,
/// 要求更高的余弦才吸收,垃圾簇在增量路径上难以滚雪球。
pub const YOUNG_PENALTY: f32 = 0.1;

/// 归簇余量:最高分与次高分之差小于该值 = 模棱两可,不当场归簇,
/// 留在未分配等下批再判(重聚类是批处理,必须落位,无此规则)。
pub const CLUSTER_MARGIN: f32 = 0.05;

/// 代表模板预算:每组参与认领判定的高质量互异模板上限。
const REP_BUDGET: usize = 8;
/// 近重复判定的余弦:达到该值视为连拍/同帧重复,不重复计入代表集。
const REP_DUP_COS: f32 = 0.95;
/// 代表模板的同照片上限:一张照片的多张脸不得虚增支持数。
const REP_MAX_PER_PHOTO: usize = 2;
/// 代表模板的最低质量;人工确认的脸不受此门槛限制。
const REP_MIN_Q: f32 = 0.4;
/// 稳健聚合取相似度最高的前 N 个代表模板。
const REP_AGG_TOP: usize = 3;
/// 质心候选放宽量:质心只负责缩小候选,最终归属看代表模板。
const CAND_SLACK: f32 = 0.15;
/// 每张脸的质心候选上限(按质心余弦取前 K 再精算)。
const CAND_K: usize = 8;

/// 连续推理失败上限:达到即停,避免模型坏掉时把全库逐张标失败。
const MAX_ANALYZE_FAILURES: usize = 5;

/// 逐照片状态:待处理。
const SCAN_PENDING: i64 = 0;
/// 逐照片状态:处理中。
const SCAN_RUNNING: i64 = 1;
/// 逐照片状态:成功(含未检出脸)。
const SCAN_DONE: i64 = 2;
/// 逐照片状态:失败。
const SCAN_FAILED: i64 = 3;

/// 扫描/重聚类/清除的进程内互斥,避免同时改写人脸结果。
static FACE_TASK_LOCK: Mutex<()> = Mutex::new(());

fn face_task_guard() -> Result<MutexGuard<'static, ()>> {
    #[cfg(not(test))]
    let g = match FACE_TASK_LOCK.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            bail!("人脸扫描或重聚类正在进行,请先取消再操作")
        }
    };
    // 测试构建改为等待,并发用例不再互相打断。
    #[cfg(test)]
    let g = FACE_TASK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    Ok(g)
}

/// 一条待提交的人脸结果(坐标已映射回未旋转原图)。
struct FaceRec {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    score: f32,
    quality: f32,
    emb: Vec<f32>,
}

/// 当前人脸方案(选项 > 常规 选择,默认 YuNet + SFace)。
pub fn current_algo(store: &Store) -> crate::ai::face::FaceAlgo {
    crate::ai::face::FaceAlgo::from_key(&store.get_setting("face_algo").unwrap_or_default())
}

/// 权重文件指纹按路径/大小/时间缓存,避免逐张照片重复读取。
static FP_CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

/// 权重文件内容哈希前 16 位;缺失或不可读单独标记,不静默当成同一权重。
fn weight_fingerprint(file: &str) -> String {
    let Some(dir) = crate::ai::models_dir() else {
        return "no-dir".into();
    };
    let path = dir.join(file);
    let Ok(meta) = std::fs::metadata(&path) else {
        return "absent".into();
    };
    let stamp = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let key = format!("{}:{}:{}", path.display(), meta.len(), stamp);
    let cache = FP_CACHE.get_or_init(Default::default);
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return v.clone();
    }
    let v = crate::ai_models::sha256_file(&path)
        .map(|s| s[..16].to_string())
        .unwrap_or_else(|_| "unreadable".into());
    cache.lock().unwrap().insert(key, v.clone());
    v
}

/// 由方案 key 构造完整管线签名:权重哈希、导出契约、预处理/对齐/检测版本与维度。
pub fn sig_from_key(key: &str) -> String {
    let algo = FaceAlgo::from_key(key);
    let (det, rec) = algo.files();
    format!(
        "{}|{}|det:{}|rec:{}|{}|{}|{}|{}|dim:{}",
        FACE_ALGO_VERSION,
        algo.key(),
        weight_fingerprint(det),
        weight_fingerprint(rec),
        algo.contract(),
        ALIGN_VERSION,
        INPUT_VERSION,
        DETECT_VERSION,
        algo.emb_dim()
    )
}

/// 当前管线签名(写入与比对都用它)。
pub fn algo_tag(store: &Store) -> String {
    sig_from_key(&store.get_setting("face_algo").unwrap_or_default())
}

/// 可用的管线签名:库中存在非当前签名模板时返回空,表示整库待确认重建。
pub fn effective_sig(store: &Store) -> String {
    let cur = algo_tag(store);
    let stale: i64 = store.read(|conn| {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM face_templates WHERE sig<>?1)",
            params![cur],
            |r| r.get(0),
        )
        .unwrap_or(1)
    });
    if stale == 0 {
        cur
    } else {
        String::new()
    }
}

/// 待确认重建:旧数据不参与比对/检索/归档,也不自动重扫。
pub fn pending_rebuild(store: &Store) -> bool {
    effective_sig(store).is_empty()
}

/// 开机自动续扫判据:非待重建、有待办、模型就位(是否已有在跑扫描由调用方判)。
pub fn should_resume(store: &Store) -> bool {
    !pending_rebuild(store)
        && remaining_count(store) > 0
        && crate::ai::face::Faces::models_present(current_algo(store))
}

/// 旧库迁移口径:face_done=1 且算法标识未变,视为已按当前版本处理。
fn legacy_scanned(store: &Store) -> bool {
    store.get_setting("face_algo_version").map(|v| v == algo_tag(store)).unwrap_or(false)
}

/// 待处理照片的公共条件:当前签名未成功、也未失败(失败需手动重试)。
const TODO_WHERE: &str = "p.kind = 0
     AND NOT EXISTS (SELECT 1 FROM face_scan s
                      WHERE s.photo_id = p.id AND s.sig = ?1 AND s.state = 2)
     AND NOT EXISTS (SELECT 1 FROM face_scan s
                      WHERE s.photo_id = p.id AND s.sig = ?1 AND s.state = 3)
     AND NOT (p.face_done = 1 AND ?2 = 1)";

pub fn photos_todo(store: &Store, cap: usize) -> Vec<(i64, String)> {
    let sig = effective_sig(store);
    if sig.is_empty() {
        return Vec::new();
    }
    let legacy = legacy_scanned(store) as i32;
    let (scope_sql, scope_pats) = scope_clause(store);
    store.read(|conn| {
        let rules = crate::ai_queue::rules_from_conn(conn);
        let base = 3 + scope_pats.len();
        let (order_sql, order_params) = priority_order(&rules, base);
        let limit_idx = base + order_params.len();
        let sql = format!(
            "SELECT p.id, p.path FROM photos p WHERE {TODO_WHERE}{scope_sql} ORDER BY {order_sql} LIMIT ?{limit_idx}"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        params.push(Box::new(sig));
        params.push(Box::new(legacy));
        for p in scope_pats {
            params.push(Box::new(p));
        }
        params.extend(order_params);
        params.push(Box::new(cap as i64));
        let mut st = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map(rusqlite::params_from_iter(params), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    })
}

/// 人脸扫描范围是否限定常用目录(设置 face_scan_scope=common)。
pub fn scan_scope_common(store: &Store) -> bool {
    store.get_setting("face_scan_scope").as_deref() == Some("common")
}

/// 记住人脸扫描范围选择。
pub fn set_scan_scope(store: &Store, common: bool) {
    let _ = store.set_setting("face_scan_scope", if common { "common" } else { "all" });
}

/// 常用目录范围的 SQL 过滤片段与 LIKE 参数(占位符从 ?3 起);全库范围返回空。
fn scope_clause(store: &Store) -> (String, Vec<String>) {
    if !scan_scope_common(store) {
        return (String::new(), Vec::new());
    }
    let dirs = crate::db::common_dirs(store);
    if dirs.is_empty() {
        return (" AND 0".to_string(), Vec::new());
    }
    let mut pats = Vec::new();
    let mut ors = Vec::new();
    for (_, p) in &dirs {
        let idx = 3 + pats.len();
        ors.push(format!("p.path LIKE ?{idx} ESCAPE '\\'"));
        pats.push(crate::db::dir_like(&p.to_string_lossy()).1);
    }
    (format!(" AND ({})", ors.join(" OR ")), pats)
}

/// 待办排序:复用 AI 目录优先级(最长前缀先命中),未命中默认档,再按 id。
fn priority_order(
    rules: &[(String, i32)],
    base: usize,
) -> (String, Vec<Box<dyn rusqlite::types::ToSql>>) {
    let mut rules = rules.to_vec();
    rules.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    let mut whens = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = base;
    for (prefix, prio) in &rules {
        whens.push(format!("WHEN p.path LIKE ?{idx} ESCAPE '\\' THEN ?{}", idx + 1));
        params.push(Box::new(crate::db::dir_like(prefix).1));
        params.push(Box::new(*prio));
        idx += 2;
    }
    let order = if whens.is_empty() {
        "p.id".to_string()
    } else {
        format!("CASE {} ELSE {} END, p.id", whens.join(" "), crate::ai_queue::P_DEFAULT)
    };
    (order, params)
}

/// 存一张人脸(含质量评分与嵌入模板)。
pub fn save_face(
    store: &Store,
    photo_id: i64,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    score: f32,
    quality: f32,
    vec: &[f32],
) -> Result<i64> {
    let mut blob = Vec::with_capacity(vec.len() * 4);
    for v in vec {
        blob.extend_from_slice(&v.to_le_bytes());
    }
    let sig = algo_tag(store);
    store.write(move |conn| -> Result<i64> {
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO faces(photo_id,x1,y1,x2,y2,score,quality,ignored)
             VALUES(?1,?2,?3,?4,?5,?6,?7,0)",
            params![
                photo_id,
                x1 as f64,
                y1 as f64,
                x2 as f64,
                y2 as f64,
                score as f64,
                quality as f64
            ],
        )?;
        let face_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO face_templates(face_id,dim,vec,sig) VALUES(?1,?2,?3,?4)",
            params![face_id, vec.len() as i64, blob, sig],
        )?;
        tx.commit()?;
        Ok(face_id)
    })
}

/// 标记照片开始处理(重启后残留的处理中状态可被再次拾取)。
fn mark_running(store: &Store, photo_id: i64, sig: &str) -> Result<()> {
    let sig = sig.to_string();
    store.write(move |conn| -> Result<()> {
        conn.execute(
            "INSERT INTO face_scan(photo_id,sig,state,mtime,size,ts)
             SELECT id,?2,?3,mtime,size,?4 FROM photos WHERE id=?1
             ON CONFLICT(photo_id) DO UPDATE SET
               sig=excluded.sig, state=excluded.state, ts=excluded.ts",
            params![photo_id, sig, SCAN_RUNNING, crate::ai_queue::now_ms()],
        )?;
        Ok(())
    })
}

/// 在一个短事务内替换某张照片的人脸与模板;签名或修订已变则放弃写入。
fn commit_photo(
    store: &Store,
    photo_id: i64,
    sig: &str,
    mtime: i64,
    size: i64,
    faces: &[FaceRec],
) -> Result<bool> {
    let sig = sig.to_string();
    store.write(move |conn| -> Result<bool> {
        let tx = conn.unchecked_transaction()?;
        let algo: String = tx
            .query_row("SELECT v FROM settings WHERE k='face_algo'", [], |r| r.get(0))
            .unwrap_or_default();
        let current = sig_from_key(&algo);
        if current != sig {
            return Ok(false);
        }
        let rev: Option<(i64, i64)> = tx
            .query_row(
                "SELECT mtime,size FROM photos WHERE id=?1",
                params![photo_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let Some((cur_mtime, cur_size)) = rev else {
            tx.execute("DELETE FROM face_scan WHERE photo_id=?1", params![photo_id])?;
            tx.commit()?;
            return Ok(false);
        };
        if cur_mtime != mtime || cur_size != size {
            // 照片已变:旧结果作废,留待下次按新内容重跑。
            tx.execute(
                "UPDATE face_scan SET state=?2, error=NULL WHERE photo_id=?1 AND sig=?3",
                params![photo_id, SCAN_PENDING, sig],
            )?;
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM face_constraints WHERE a_face IN (SELECT id FROM faces WHERE photo_id=?1)
                OR b_face IN (SELECT id FROM faces WHERE photo_id=?1)",
            params![photo_id],
        )?;
        tx.execute(
            "DELETE FROM face_templates WHERE face_id IN (SELECT id FROM faces WHERE photo_id=?1)",
            params![photo_id],
        )?;
        tx.execute("DELETE FROM faces WHERE photo_id=?1", params![photo_id])?;
        for f in faces {
            tx.execute(
                "INSERT INTO faces(photo_id,x1,y1,x2,y2,score,quality,ignored)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,0)",
                params![
                    photo_id,
                    f.x1 as f64,
                    f.y1 as f64,
                    f.x2 as f64,
                    f.y2 as f64,
                    f.score as f64,
                    f.quality as f64
                ],
            )?;
            let face_id = tx.last_insert_rowid();
            let mut blob = Vec::with_capacity(f.emb.len() * 4);
            for v in &f.emb {
                blob.extend_from_slice(&v.to_le_bytes());
            }
            tx.execute(
                "INSERT INTO face_templates(face_id,dim,vec,sig) VALUES(?1,?2,?3,?4)",
                params![face_id, f.emb.len() as i64, blob, sig],
            )?;
        }
        tx.execute(
            "UPDATE photos SET face_done=1 WHERE id=?1",
            params![photo_id],
        )?;
        tx.execute(
            "INSERT INTO face_scan(photo_id,sig,state,attempts,error,face_count,mtime,size,ts)
             VALUES(?1,?2,?3,0,NULL,?4,?5,?6,?7)
             ON CONFLICT(photo_id) DO UPDATE SET
               sig=excluded.sig, state=excluded.state, attempts=0, error=NULL,
               face_count=excluded.face_count, mtime=excluded.mtime,
               size=excluded.size, ts=excluded.ts",
            params![
                photo_id,
                sig,
                SCAN_DONE,
                faces.len() as i64,
                mtime,
                size,
                crate::ai_queue::now_ms()
            ],
        )?;
        tx.commit()?;
        Ok(true)
    })
}

/// 记录一张照片的处理失败,不写成功状态。
fn mark_failed(store: &Store, photo_id: i64, sig: &str, error: &str) -> Result<()> {
    let sig = sig.to_string();
    let err: String = error.chars().take(300).collect();
    store.write(move |conn| -> Result<()> {
        conn.execute(
            "UPDATE face_scan
                SET state=?2, attempts=attempts+1, error=?3, ts=?4
              WHERE photo_id=?1 AND sig=?5",
            params![photo_id, SCAN_FAILED, err, crate::ai_queue::now_ms(), sig],
        )?;
        Ok(())
    })
}

/// 当前签名下失败的照片数。
pub fn failed_count(store: &Store) -> usize {
    let sig = effective_sig(store);
    store.read(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM face_scan WHERE sig=?1 AND state=?2",
            params![sig, SCAN_FAILED],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n as usize)
        .unwrap_or(0)
    })
}

/// 把失败项重新放回待处理队列。
pub fn retry_failed(store: &Store) -> Result<usize> {
    let sig = effective_sig(store);
    store.write(move |conn| -> Result<usize> {
        let n = conn.execute(
            "UPDATE face_scan SET state=?2, attempts=0, error=NULL
              WHERE sig=?1 AND state=?3",
            params![sig, SCAN_PENDING, SCAN_FAILED],
        )?;
        Ok(n)
    })
}


/// 人脸质量评分 [0,1]:
/// 0.35*检测置信度 + 0.30*尺寸分(短边 48px 满分) + 0.35*锐度分。
/// 锐度 = 人脸框内 32x32 灰度的拉普拉斯方差,经 var/(var+400) 饱和映射。
/// 低质量脸不丢弃,但在聚类中以低权重参与(见 assign_clusters)。
pub fn face_quality(img: &image::DynamicImage, det: &Det) -> f32 {
    let det_score = det.score.clamp(0.0, 1.0);
    let bw = (det.x1 - det.x0).max(1.0);
    let bh = (det.y1 - det.y0).max(1.0);
    let size_score = (bw.min(bh) / 48.0).clamp(0.0, 1.0);
    let (iw, ih) = (img.width(), img.height());
    let x0 = (det.x0.clamp(0.0, (iw - 1) as f32) as u32).min(iw - 1);
    let y0 = (det.y0.clamp(0.0, (ih - 1) as f32) as u32).min(ih - 1);
    let x1 = ((det.x1.clamp(0.0, iw as f32) as u32).max(x0 + 1)).min(iw);
    let y1 = ((det.y1.clamp(0.0, ih as f32) as u32).max(y0 + 1)).min(ih);
    let sharp_score = if x1 > x0 && y1 > y0 {
        let crop = img.crop_imm(x0, y0, x1 - x0, y1 - y0);
        let g = crop
            .resize_exact(32, 32, image::imageops::FilterType::Triangle)
            .to_luma8();
        let mut var = 0.0f32;
        let mut n = 0usize;
        for y in 1..31u32 {
            for x in 1..31u32 {
                let p = g.get_pixel(x, y).0[0] as f32;
                let lap = 4.0 * p
                    - g.get_pixel(x - 1, y).0[0] as f32
                    - g.get_pixel(x + 1, y).0[0] as f32
                    - g.get_pixel(x, y - 1).0[0] as f32
                    - g.get_pixel(x, y + 1).0[0] as f32;
                var += lap * lap;
                n += 1;
            }
        }
        if n > 0 {
            let var = var / n as f32;
            var / (var + 400.0)
        } else {
            0.0
        }
    } else {
        0.0
    };
    0.35 * det_score + 0.30 * size_score + 0.35 * sharp_score
}

fn blob_to_vec(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn cos(a: &[f32], b: &[f32]) -> f32 {
    let mut s = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        s += x * y;
    }
    s
}

/// 簇的吸收阈值:成员不足 CLUSTER_MATURE 的年轻簇质心支撑弱,收紧一点。
fn join_thr(base: f32, members: usize) -> f32 {
    if members < CLUSTER_MATURE {
        base + YOUNG_PENALTY
    } else {
        base
    }
}

/// 参与聚类/认领计算的一张脸(内存中的副本)。
#[derive(Clone)]
struct Member {
    v: Vec<f32>,
    q: f32,
    photo: i64,
    /// 归属来源为人工确认(assign_src='manual')。
    manual: bool,
}

/// 内存计算中的簇状态:成员、代表模板集与质心增量和。
#[derive(Clone)]
struct ClusterMem {
    members: Vec<Member>,
    reps: Vec<Member>,
    sum: Vec<f32>,
    wsum: f32,
    /// 质心缓存:成员一变就重算一次,打分时只读,不再每脸×每簇归一化。
    cent: Option<Vec<f32>>,
}

impl ClusterMem {
    fn new(dim: usize) -> Self {
        Self {
            members: Vec::new(),
            reps: Vec::new(),
            sum: vec![0.0; dim],
            wsum: 0.0,
            cent: None,
        }
    }

    fn refresh_centroid(&mut self) {
        self.cent = sum_centroid(&self.sum, self.wsum);
    }

    fn centroid(&self) -> Option<&[f32]> {
        self.cent.as_deref()
    }

    fn add(&mut self, m: &Member, thr: f32) {
        member_add(&mut self.sum, &mut self.wsum, &mut self.reps, m, thr);
        self.members.push(m.clone());
        self.refresh_centroid();
    }
}

/// 把一张脸并入簇的累计状态(质心增量和 + 代表模板尝试入围)。
fn member_add(
    sum: &mut [f32],
    wsum: &mut f32,
    reps: &mut Vec<Member>,
    m: &Member,
    thr: f32,
) {
    let w = m.q.max(0.05);
    for (a, b) in sum.iter_mut().zip(&m.v) {
        *a += b * w;
    }
    *wsum += w;
    let _ = rep_try_add(reps, m, thr);
}

/// 增量和归一化质心;只用于缩小候选,不作为最终归属凭证。
fn sum_centroid(sum: &[f32], wsum: f32) -> Option<Vec<f32>> {
    if wsum <= 0.0 {
        return None;
    }
    let mut m = sum.to_vec();
    for v in &mut m {
        *v /= wsum;
    }
    let norm = m.iter().map(|v| v * v).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= 0.0 {
        return None;
    }
    for v in &mut m {
        *v /= norm;
    }
    Some(m)
}

/// 尝试把一张脸加入代表模板集:预算、同照片上限、近重复去重、
/// 自动归属脸须获已选人工模板支持(防自动模板自我强化)。
fn rep_try_add(reps: &mut Vec<Member>, m: &Member, thr: f32) -> bool {
    if reps.len() >= REP_BUDGET {
        return false;
    }
    if reps.iter().filter(|r| r.photo == m.photo).count() >= REP_MAX_PER_PHOTO {
        return false;
    }
    if reps.iter().any(|r| cos(&r.v, &m.v) >= REP_DUP_COS) {
        return false;
    }
    if reps.iter().any(|r| r.manual)
        && !m.manual
        && !reps
            .iter()
            .any(|r| r.manual && cos(&r.v, &m.v) >= thr)
    {
        return false;
    }
    reps.push(m.clone());
    true
}

/// 从全组成员选代表模板:人工确认优先、质量降序;低质量脸默认不入围,
/// 全组都不达标时退回质量最高的一张(组不能没有代表)。
fn rep_select(ms: &[Member], thr: f32) -> Vec<Member> {
    let mut order: Vec<usize> = (0..ms.len()).collect();
    order.sort_by(|&a, &b| {
        ms[b]
            .manual
            .cmp(&ms[a].manual)
            .then(
                ms[b]
                    .q
                    .partial_cmp(&ms[a].q)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.cmp(&b))
    });
    let mut reps: Vec<Member> = Vec::new();
    for &i in &order {
        if !ms[i].manual && ms[i].q < REP_MIN_Q {
            continue;
        }
        if rep_try_add(&mut reps, &ms[i], thr) && reps.len() >= REP_BUDGET {
            break;
        }
    }
    if reps.is_empty() {
        if let Some(&i) = order.first() {
            reps.push(ms[i].clone());
        }
    }
    reps
}

/// 候选脸对代表模板集的打分:返回(达到 thr_eff 的支持数, 前 N 高相似度均值)。
fn rep_claim(reps: &[Member], v: &[f32], thr_eff: f32) -> (usize, f32) {
    if reps.is_empty() {
        return (0, f32::MIN);
    }
    let mut sims: Vec<f32> = reps.iter().map(|r| cos(v, &r.v)).collect();
    sims.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let support = sims.iter().filter(|s| **s >= thr_eff).count();
    let top = sims.len().min(REP_AGG_TOP);
    let agg = sims[..top].iter().sum::<f32>() / top as f32;
    (support, agg)
}

/// 认领所需支持数:有人工锚点或仅单模板的组 1 票即可;纯自动的多模板组必须 2 票。
/// 单模板自动组可凭 1 票吸一张脸,吸满两张后转为多模板组、此后需 2 票,
/// 垃圾簇因此在增量路径上难以滚雪球。
fn required_support(reps: &[Member]) -> usize {
    if reps.len() >= 2 && !reps.iter().any(|r| r.manual) {
        2
    } else {
        1
    }
}

/// 两组代表模板是否双向支持:各自至少 min(2, 模板数) 张在对方找到匹配,
/// 防单侧桥接脸把两组硬拉在一起。
fn mutual_support(ra: &[Member], rb: &[Member], thr: f32) -> bool {
    fn side(x: &[Member], y: &[Member], thr: f32) -> bool {
        let need = x.len().min(2);
        let got = x
            .iter()
            .filter(|m| y.iter().any(|n| cos(&m.v, &n.v) >= thr))
            .count();
        got >= need
    }
    !ra.is_empty() && !rb.is_empty() && side(ra, rb, thr) && side(rb, ra, thr)
}

/// 同图脸默认不能自动合并;人工 must 例外。
fn shares_photo(photos: &BTreeMap<i64, Vec<i64>>, cid: i64, photo: i64) -> bool {
    photos
        .get(&cid)
        .map(|ps| ps.contains(&photo))
        .unwrap_or(false)
}

/// 人脸与簇内成员是否存在 never 约束。
fn blocked_by_never(
    members: &BTreeMap<i64, Vec<i64>>,
    never: &HashMap<i64, Vec<i64>>,
    cid: i64,
    face: i64,
) -> bool {
    members
        .get(&cid)
        .map(|ms| {
            ms.iter().any(|m| {
                never
                    .get(&face)
                    .map(|ns| ns.contains(m))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// 人脸与簇内成员是否存在 must 约束。
fn forced_by_must(
    members: &BTreeMap<i64, Vec<i64>>,
    must: &HashMap<i64, Vec<i64>>,
    cid: i64,
    face: i64,
) -> bool {
    members
        .get(&cid)
        .map(|ms| {
            ms.iter().any(|m| {
                must.get(&face)
                    .map(|ns| ns.contains(m))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// 增量聚类:质心只负责缩小候选,最终归属由**代表模板**的支持数与
/// 聚合相似度决定;过该簇的成熟度阈值且与次高簇拉开足够余量才归簇,
/// 都不够格则新建簇,模棱两可的留在未分配,下批随质心移动或合并结果再判。
/// 同图两张脸默认不并(人工 must 例外);忽略的脸不参与也不成簇。
/// 私有:只能在人脸任务守卫内调用,外部一律走 scan_all / recluster_all。
fn assign_clusters(store: &Store, thr: f32) -> Result<usize> {
    let sig = effective_sig(store);
    if sig.is_empty() {
        return Ok(0);
    }
    let dim = emb_dim(store);
    let (mut clusters, mut members, mut photos, cluster_pid, mut next_id) =
        store.read(|conn| -> Result<_> {
            let mut clusters: BTreeMap<i64, ClusterMem> = BTreeMap::new();
            let mut members: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
            let mut photos: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
            let mut st = conn.prepare(
                "SELECT f.id, f.cluster, f.photo_id, t.vec, COALESCE(f.quality,0.5),
                        COALESCE(f.assign_src,'') FROM faces f
                 JOIN face_templates t ON t.face_id=f.id AND t.dim=?1 AND t.sig=?2
                 WHERE f.cluster IS NOT NULL AND f.ignored=0",
            )?;
            let mut rows = st.query(params![dim, sig])?;
            while let Some(r) = rows.next()? {
                let fid: i64 = r.get(0)?;
                let cid: i64 = r.get(1)?;
                let photo_id: i64 = r.get(2)?;
                let blob: Vec<u8> = r.get(3)?;
                let q: f64 = r.get(4)?;
                let src: String = r.get(5)?;
                let m = Member {
                    v: blob_to_vec(&blob),
                    q: q as f32,
                    photo: photo_id,
                    manual: src == "manual",
                };
                let e = clusters
                    .entry(cid)
                    .or_insert_with(|| ClusterMem::new(dim as usize));
                let w = m.q.max(0.05);
                for (a, b) in e.sum.iter_mut().zip(&m.v) {
                    *a += b * w;
                }
                e.wsum += w;
                e.members.push(m);
                members.entry(cid).or_default().push(fid);
                photos.entry(cid).or_default().push(photo_id);
            }
            for cm in clusters.values_mut() {
                cm.reps = rep_select(&cm.members, thr);
                cm.refresh_centroid();
            }
            let mut cluster_pid: BTreeMap<i64, (i64, i64)> = BTreeMap::new();
            let mut st = conn.prepare(
                "SELECT cluster, person_id, COUNT(*) FROM faces
                 WHERE cluster IS NOT NULL AND person_id IS NOT NULL AND ignored=0
                 GROUP BY cluster, person_id",
            )?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                let cid: i64 = r.get(0)?;
                let pid: i64 = r.get(1)?;
                let n: i64 = r.get(2)?;
                let better = cluster_pid
                    .get(&cid)
                    .map(|(p, c)| n > *c || (n == *c && pid < *p))
                    .unwrap_or(true);
                if better {
                    cluster_pid.insert(cid, (pid, n));
                }
            }
            let max_cluster: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(cluster),0) FROM faces WHERE cluster IS NOT NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let max_person: i64 = conn
                .query_row("SELECT COALESCE(MAX(person_id),0) FROM faces", [], |r| r.get(0))
                .unwrap_or(0);
            let max_people: i64 = conn
                .query_row("SELECT COALESCE(MAX(id),0) FROM people", [], |r| r.get(0))
                .unwrap_or(0);
            Ok((
                clusters,
                members,
                photos,
                cluster_pid,
                max_cluster.max(max_person).max(max_people) + 1,
            ))
        })?;
    let todo: Vec<(i64, Member)> = store.read(|conn| -> Result<_> {
        let mut st = conn.prepare(
            "SELECT f.id, f.photo_id, t.vec, COALESCE(f.quality,0.5),
                    COALESCE(f.assign_src,'') FROM faces f
             JOIN face_templates t ON t.face_id=f.id AND t.dim=?1 AND t.sig=?2
             WHERE f.cluster IS NULL AND f.ignored=0
             ORDER BY COALESCE(f.quality,0.5) DESC, f.id",
        )?;
        let mut rows = st.query(params![dim, sig])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let photo_id: i64 = r.get(1)?;
            let blob: Vec<u8> = r.get(2)?;
            let q: f64 = r.get(3)?;
            let src: String = r.get(4)?;
            out.push((
                id,
                Member {
                    v: blob_to_vec(&blob),
                    q: q as f32,
                    photo: photo_id,
                    manual: src == "manual",
                },
            ));
        }
        Ok(out)
    })?;
    // 人工约束:must 优先归入同伴簇,never 禁止与对立面同簇。
    let cons = crate::ai_review::all(store);
    let mut must: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut never: HashMap<i64, Vec<i64>> = HashMap::new();
    for (a, b, k) in &cons {
        let map = match k {
            crate::ai_review::Kind::Must => &mut must,
            crate::ai_review::Kind::Never => &mut never,
        };
        map.entry(*a).or_default().push(*b);
        map.entry(*b).or_default().push(*a);
    }
    // 先在内存里定簇,再一次事务写回 —— 每脸一条 UPDATE 也锁一次写连接。
    let mut updates: Vec<(i64, i64, Option<i64>)> = Vec::with_capacity(todo.len());
    for (face_id, m) in todo {
        // must 同伴已在某簇:直接归入(never 仍优先,不参与阈值判定)。
        let forced = must.get(&face_id).and_then(|ps| {
            members.iter().find_map(|(&cid, ms)| {
                (!blocked_by_never(&members, &never, cid, face_id)
                    && ms.iter().any(|x| ps.contains(x)))
                .then_some(cid)
            })
        });
        if let Some(cid) = forced {
            updates.push((face_id, cid, cluster_pid.get(&cid).map(|(p, _)| *p)));
            join_cluster(&mut clusters, &mut members, &mut photos, cid, &m, face_id, thr);
            continue;
        }
        // 各簇按余弦打分;同图不并,never 不并。
        let mut cents: Vec<(i64, f32)> = Vec::new();
        for (&cid, cm) in &clusters {
            if blocked_by_never(&members, &never, cid, face_id) {
                continue;
            }
            if shares_photo(&photos, cid, m.photo)
                && !forced_by_must(&members, &must, cid, face_id)
            {
                continue;
            }
            if let Some(c) = cm.centroid() {
                cents.push((cid, cos(c, &m.v)));
            }
        }
        cents.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        cents.truncate(CAND_K);
        let mut scored: Vec<(i64, f32, bool)> = cents
            .into_iter()
            .filter(|(_, c)| *c >= thr - CAND_SLACK)
            .map(|(cid, _)| {
                let cm = &clusters[&cid];
                let thr_eff = join_thr(thr, cm.members.len());
                let (support, agg) = rep_claim(&cm.reps, &m.v, thr_eff);
                (cid, agg, agg >= thr_eff && support >= required_support(&cm.reps))
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        match scored.iter().find(|(_, _, ok)| *ok) {
            Some((cid, agg, _)) => {
                // 次高簇只差一点 = 归属存疑,不当场归簇也不另开新簇。
                let runner = scored
                    .iter()
                    .filter(|(other, _, _)| other != cid)
                    .map(|(_, a, _)| *a)
                    .fold(f32::NEG_INFINITY, f32::max);
                if *agg - runner >= CLUSTER_MARGIN {
                    updates.push((face_id, *cid, cluster_pid.get(cid).map(|(p, _)| *p)));
                    join_cluster(&mut clusters, &mut members, &mut photos, *cid, &m, face_id, thr);
                }
            }
            None => {
                let next = next_id;
                next_id += 1;
                updates.push((face_id, next, None));
                clusters.insert(next, {
                    let mut cm = ClusterMem::new(dim as usize);
                    cm.add(&m, thr);
                    cm
                });
                members.insert(next, vec![face_id]);
                photos.insert(next, vec![m.photo]);
            }
        }
    }
    store.write(|conn| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        {
            let mut st = tx.prepare_cached(
                "UPDATE faces SET cluster=?1,
                 person_id=CASE WHEN person_id IS NULL THEN ?2 ELSE person_id END,
                 assign_src=CASE WHEN person_id IS NULL AND ?2 IS NOT NULL
                                 THEN 'auto' ELSE assign_src END
                 WHERE id=?3",
            )?;
            for (face_id, cid, pid) in &updates {
                st.execute(params![cid, pid, face_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    })?;
    // 归入已有簇的新脸继承该簇身份;未命名身份不写名字表。
    Ok(clusters.len())
}

/// 把一张脸并入簇的全部内存状态(成员、质心、代表模板、索引表)。
#[allow(clippy::too_many_arguments)]
fn join_cluster(
    clusters: &mut BTreeMap<i64, ClusterMem>,
    members: &mut BTreeMap<i64, Vec<i64>>,
    photos: &mut BTreeMap<i64, Vec<i64>>,
    cid: i64,
    m: &Member,
    fid: i64,
    thr: f32,
) {
    if let Some(cm) = clusters.get_mut(&cid) {
        cm.add(m, thr);
    }
    members.entry(cid).or_default().push(fid);
    photos.entry(cid).or_default().push(m.photo);
}

/// 两簇之间是否存在 never 约束(任一对人脸)。
fn never_between(never: &HashMap<i64, Vec<i64>>, a: &[i64], b: &[i64]) -> bool {
    a.iter().any(|x| {
        b.iter()
            .any(|y| never.get(x).map(|v| v.contains(y)).unwrap_or(false))
    })
}

/// 两簇之间是否存在 must 约束(任一对人脸)。
fn must_between(must: &HashMap<i64, Vec<i64>>, a: &[i64], b: &[i64]) -> bool {
    a.iter().any(|x| {
        b.iter()
            .any(|y| must.get(x).map(|v| v.contains(y)).unwrap_or(false))
    })
}

/// 合并质心足够相近的簇;已命名人物所在的簇互不合并,只重排簇号。
/// 私有:只能在人脸任务守卫内调用,外部一律走 recluster_all。
fn merge_similar_clusters(store: &Store, thr: f32) -> Result<usize> {
    let sig = effective_sig(store);
    if sig.is_empty() {
        return Ok(0);
    }
    let dim = emb_dim(store);
    let (mut clusters, named, mut members, mut photos): (
        HashMap<i64, ClusterMem>,
        HashSet<i64>,
        HashMap<i64, Vec<i64>>,
        HashMap<i64, Vec<i64>>,
    ) = store.read(|conn| -> Result<_> {
        let mut st = conn.prepare(
            "SELECT f.id, f.cluster, f.photo_id, t.vec, COALESCE(f.quality,0.5),
                    COALESCE(f.assign_src,'') FROM faces f
             JOIN face_templates t ON t.face_id=f.id AND t.dim=?1 AND t.sig=?2
             WHERE f.cluster IS NOT NULL AND f.ignored=0",
        )?;
        let mut rows = st.query(params![dim, sig])?;
        let mut acc: HashMap<i64, ClusterMem> = HashMap::new();
        let mut members: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut photos: HashMap<i64, Vec<i64>> = HashMap::new();
        while let Some(r) = rows.next()? {
            let fid: i64 = r.get(0)?;
            let cid: i64 = r.get(1)?;
            let photo_id: i64 = r.get(2)?;
            let blob: Vec<u8> = r.get(3)?;
            let q: f64 = r.get(4)?;
            let src: String = r.get(5)?;
            let m = Member {
                v: blob_to_vec(&blob),
                q: q as f32,
                photo: photo_id,
                manual: src == "manual",
            };
            let e = acc.entry(cid).or_insert_with(|| ClusterMem::new(dim as usize));
            let w = m.q.max(0.05);
            for (a, b) in e.sum.iter_mut().zip(&m.v) {
                *a += b * w;
            }
            e.wsum += w;
            e.members.push(m);
            members.entry(cid).or_default().push(fid);
            photos.entry(cid).or_default().push(photo_id);
        }
        for cm in acc.values_mut() {
            cm.reps = rep_select(&cm.members, thr);
            cm.refresh_centroid();
        }
        let mut named: HashSet<i64> = HashSet::new();
        let mut st = conn.prepare(
            "SELECT DISTINCT f.cluster FROM faces f JOIN people p ON p.id=f.person_id
             WHERE f.cluster IS NOT NULL AND f.ignored=0",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            named.insert(r.get::<_, i64>(0)?);
        }
        Ok((acc, named, members, photos))
    })?;
    if clusters.len() < 2 {
        return Ok(0);
    }
    // 人工约束:must 强制合并,never 禁止合并。
    let cons = crate::ai_review::all(store);
    let mut must: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut never: HashMap<i64, Vec<i64>> = HashMap::new();
    for (a, b, k) in &cons {
        let map = match k {
            crate::ai_review::Kind::Must => &mut must,
            crate::ai_review::Kind::Never => &mut never,
        };
        map.entry(*a).or_default().push(*b);
        map.entry(*b).or_default().push(*a);
    }
    // 内存里反复并当前最像的一对,直到没有合格对;簇号直接指向最终归宿
    // (被并的簇即刻移出,后续只会再并入仍存活的簇,不会出现链式改号)。
    let mut merges: Vec<(i64, i64)> = Vec::new(); // (被并掉的簇, 保留方)
    loop {
        // 质心已缓存,每轮只取簇号;排序让同分时的取舍可复现。
        let mut cids: Vec<i64> = clusters.keys().copied().collect();
        cids.sort_unstable();
        let mut best: Option<(i64, i64, f32)> = None;
        for i in 0..cids.len() {
            for j in i + 1..cids.len() {
                let (a, b) = (cids[i], cids[j]);
                let (Some(ca), Some(cb)) = (clusters.get(&a), clusters.get(&b)) else {
                    continue;
                };
                let (Some(ma), Some(mb)) = (ca.centroid(), cb.centroid()) else {
                    continue;
                };
                let (Some(ia), Some(ib)) = (members.get(&a), members.get(&b)) else {
                    continue;
                };
                let forced = must_between(&must, ia, ib);
                // never 永远压过 must;两个已命名簇只在 must 明确要求时合并。
                if never_between(&never, ia, ib) {
                    continue;
                }
                if !forced && named.contains(&a) && named.contains(&b) {
                    continue;
                }
                // 同图两簇默认不并,避免合照里的不同人被当成同一个人。
                let shared = photos
                    .get(&a)
                    .map(|pa| photos.get(&b).map(|pb| pa.iter().any(|p| pb.contains(p))).unwrap_or(false))
                    .unwrap_or(false);
                if shared && !forced {
                    continue;
                }
                let c = cos(ma, mb);
                if !forced {
                    // 任一方成员过少时支撑弱,合并阈值同样收紧,防绕过年轻簇保护。
                    let thr_eff = if ca.members.len() < CLUSTER_MATURE
                        || cb.members.len() < CLUSTER_MATURE
                    {
                        thr + YOUNG_PENALTY
                    } else {
                        thr
                    };
                    if c < thr_eff {
                        continue;
                    }
                    // 双向多点代表支持:只有单侧桥接脸相似时不合并。
                    if !mutual_support(&ca.reps, &cb.reps, thr_eff) {
                        continue;
                    }
                }
                // must 对带 2.0 偏置,保证排在所有普通候选之前。
                let sc = if forced { 2.0 + c } else { c };
                if (forced || c >= thr) && best.map(|(_, _, bc)| sc > bc).unwrap_or(true) {
                    best = Some((a, b, sc));
                }
            }
        }
        let Some((a, b, _)) = best else { break };
        // 保留方:有名字的一方优先,其次成员多的;平局取小号。
        let (keep, drop) = match (named.contains(&a), named.contains(&b)) {
            (true, false) => (a, b),
            (false, true) => (b, a),
            _ => {
                let (na, nb) = (clusters[&a].members.len(), clusters[&b].members.len());
                if na > nb || (na == nb && a < b) {
                    (a, b)
                } else {
                    (b, a)
                }
            }
        };
        let mut moved = clusters.remove(&drop).unwrap();
        let kept = clusters.get_mut(&keep).unwrap();
        for (x, y) in kept.sum.iter_mut().zip(&moved.sum) {
            *x += y;
        }
        kept.wsum += moved.wsum;
        kept.members.append(&mut moved.members);
        kept.reps = rep_select(&kept.members, thr);
        kept.refresh_centroid();
        if let Some(ids) = members.remove(&drop) {
            members.get_mut(&keep).unwrap().extend(ids);
        }
        if let Some(ps) = photos.remove(&drop) {
            photos.get_mut(&keep).unwrap().extend(ps);
        }
        merges.push((drop, keep));
    }
    if merges.is_empty() {
        return Ok(0);
    }
    let merged = merges.len();
    // 一次事务落库:只重排簇号,人物档案与身份列一律不动。
    store.write(move |conn| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        {
            let mut st = tx.prepare_cached("UPDATE faces SET cluster=?1 WHERE cluster=?2")?;
            for (drop, keep) in &merges {
                st.execute(params![keep, drop])?;
            }
        }
        tx.commit()?;
        Ok(())
    })?;
    Ok(merged)
}

#[derive(Clone)]
pub struct PeopleRow {
    /// 代表簇号(忽略与审核入口)。
    pub cluster: i64,
    /// 稳定身份(未归属 = None)。
    pub person_id: Option<i64>,
    /// 该组涉及的簇号。
    pub clusters: Vec<i64>,
    pub person: Option<String>,
    pub count: usize,
    /// 组内平均人脸质量(0..1;低质量组值得人工复核)。
    pub quality: f32,
}

/// 人物列表:已归属按身份合并,未归属按簇分列(忽略的脸不计入)。
pub fn people_overview(store: &Store) -> Vec<PeopleRow> {
    store.read(|conn| {
        let mut st = match conn.prepare(
            "SELECT g.cluster, g.pid, p.name, g.cnt, g.q, g.cs FROM
              (SELECT MIN(cluster) AS cluster, person_id AS pid, COUNT(*) AS cnt,
                      AVG(COALESCE(quality,0.5)) AS q,
                      GROUP_CONCAT(DISTINCT cluster) AS cs
               FROM faces WHERE cluster IS NOT NULL AND ignored=0 AND person_id IS NOT NULL
               GROUP BY person_id
               UNION ALL
               SELECT cluster AS cluster, NULL AS pid, COUNT(*) AS cnt,
                      AVG(COALESCE(quality,0.5)) AS q, CAST(cluster AS TEXT) AS cs
               FROM faces WHERE cluster IS NOT NULL AND ignored=0 AND person_id IS NULL
               GROUP BY cluster) g
             LEFT JOIN people p ON p.id = g.pid
             ORDER BY g.cnt DESC, g.cluster",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map([], |r| {
            let cs: String = r.get(5)?;
            Ok(PeopleRow {
                cluster: r.get::<_, i64>(0)?,
                person_id: r.get(1)?,
                person: r.get(2)?,
                count: r.get::<_, i64>(3)? as usize,
                quality: r.get::<_, f64>(4)? as f32,
                clusters: cs
                    .split(',')
                    .filter_map(|s| s.parse::<i64>().ok())
                    .collect(),
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    })
}

pub struct FaceHit {
    pub photo_id: i64,
    /// 人脸框信息(预留:照片网格上叠画人脸框)。
    #[allow(dead_code)]
    pub face_id: i64,
    #[allow(dead_code)]
    pub photo_path: String,
    #[allow(dead_code)]
    pub x1: f32,
    #[allow(dead_code)]
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    /// 质量评分(面板过滤/排序用)。
    pub quality: f32,
}

/// 簇内人脸(可含被忽略的;min_quality 过滤低质量脸)。
pub fn faces_for_cluster(
    store: &Store,
    cluster: i64,
    include_ignored: bool,
    min_quality: f32,
) -> Vec<FaceHit> {
    store.read(|conn| {
        let mut st = match conn.prepare(
            "SELECT f.id, f.photo_id, p.path, f.x1, f.y1, f.x2, f.y2, COALESCE(f.quality,0.5)
             FROM faces f JOIN photos p ON p.id = f.photo_id
             WHERE f.cluster = ?1 AND (?2=1 OR f.ignored=0)
               AND COALESCE(f.quality,0.5) >= ?3
             ORDER BY p.path",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map(
            params![cluster, include_ignored as i32, min_quality as f64],
            |r| {
                Ok(FaceHit {
                    face_id: r.get(0)?,
                    photo_id: r.get(1)?,
                    photo_path: r.get(2)?,
                    x1: r.get::<_, f64>(3)? as f32,
                    y1: r.get::<_, f64>(4)? as f32,
                    x2: r.get::<_, f64>(5)? as f32,
                    y2: r.get::<_, f64>(6)? as f32,
                    quality: r.get::<_, f64>(7)? as f32,
                })
            },
        )
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    })
}

/// 稳定身份下的全部人脸(可跨簇;min_quality 过滤低质量脸)。
pub fn faces_for_person(
    store: &Store,
    person_id: i64,
    include_ignored: bool,
    min_quality: f32,
) -> Vec<FaceHit> {
    store.read(|conn| {
        let mut st = match conn.prepare(
            "SELECT f.id, f.photo_id, p.path, f.x1, f.y1, f.x2, f.y2, COALESCE(f.quality,0.5)
             FROM faces f JOIN photos p ON p.id = f.photo_id
             WHERE f.person_id = ?1 AND (?2=1 OR f.ignored=0)
               AND COALESCE(f.quality,0.5) >= ?3
             ORDER BY p.path",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map(
            params![person_id, include_ignored as i32, min_quality as f64],
            |r| {
                Ok(FaceHit {
                    face_id: r.get(0)?,
                    photo_id: r.get(1)?,
                    photo_path: r.get(2)?,
                    x1: r.get::<_, f64>(3)? as f32,
                    y1: r.get::<_, f64>(4)? as f32,
                    x2: r.get::<_, f64>(5)? as f32,
                    y2: r.get::<_, f64>(6)? as f32,
                    quality: r.get::<_, f64>(7)? as f32,
                })
            },
        )
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    })
}

/// 单张照片的全部人脸(预览窗叠框用;坐标 = 未旋转原图像素)。
pub fn faces_for_photo(store: &Store, photo_id: i64) -> Vec<FaceHit> {
    store.read(|conn| {
        let mut st = match conn.prepare(
            "SELECT f.id, f.photo_id, p.path, f.x1, f.y1, f.x2, f.y2, COALESCE(f.quality,0.5)
             FROM faces f JOIN photos p ON p.id = f.photo_id
             WHERE f.photo_id = ?1 ORDER BY f.id",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map(params![photo_id], |r| {
            Ok(FaceHit {
                face_id: r.get(0)?,
                photo_id: r.get(1)?,
                photo_path: r.get(2)?,
                x1: r.get::<_, f64>(3)? as f32,
                y1: r.get::<_, f64>(4)? as f32,
                x2: r.get::<_, f64>(5)? as f32,
                y2: r.get::<_, f64>(6)? as f32,
                quality: r.get::<_, f64>(7)? as f32,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    })
}

/// 按脸检索的候选(跨全库当前管线)。
pub struct SimilarFace {
    pub face_id: i64,
    #[allow(dead_code)]
    pub photo_id: i64,
    pub photo_path: String,
    pub cluster: Option<i64>,
    /// 稳定身份(未归属 = None)。
    pub person_id: Option<i64>,
    pub person: Option<String>,
    #[allow(dead_code)]
    pub ignored: bool,
    pub quality: f32,
    pub score: f32,
    #[allow(dead_code)]
    pub rect: (f32, f32, f32, f32),
}

/// 全库按脸相似检索:排除自身,按余弦降序截断。
pub fn similar_faces(
    store: &Store,
    face_id: i64,
    min_score: f32,
    limit: usize,
    include_ignored: bool,
) -> Vec<SimilarFace> {
    let sig = effective_sig(store);
    if sig.is_empty() {
        return Vec::new();
    }
    let dim = emb_dim(store);
    store
        .read(|conn| -> Result<Vec<SimilarFace>> {
            let blob: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT vec FROM face_templates WHERE face_id=?1 AND dim=?2 AND sig=?3",
                    params![face_id, dim, sig],
                    |r| r.get(0),
                )
                .ok();
            let Some(blob) = blob else {
                return Ok(Vec::new());
            };
            let query = blob_to_vec(&blob);
            if query.len() != dim as usize {
                return Ok(Vec::new());
            }
            let mut st = conn.prepare(
                "SELECT f.id, f.photo_id, p.path, f.cluster, f.person_id, pe.name, f.ignored,
                        COALESCE(f.quality,0.5), t.vec, f.x1, f.y1, f.x2, f.y2
                 FROM faces f
                 JOIN face_templates t ON t.face_id=f.id AND t.dim=?1 AND t.sig=?4
                 JOIN photos p ON p.id=f.photo_id
                 LEFT JOIN people pe ON pe.id=f.person_id
                 WHERE f.id<>?2 AND (?3=1 OR f.ignored=0)",
            )?;
            let mut rows = st.query(params![dim, face_id, include_ignored as i32, sig])?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? {
                let vec = blob_to_vec(&r.get::<_, Vec<u8>>(8)?);
                if vec.len() != query.len() {
                    continue;
                }
                let score = cos(&query, &vec);
                if !score.is_finite() || score < min_score {
                    continue;
                }
                out.push(SimilarFace {
                    face_id: r.get(0)?,
                    photo_id: r.get(1)?,
                    photo_path: r.get(2)?,
                    cluster: r.get(3)?,
                    person_id: r.get(4)?,
                    person: r.get(5)?,
                    ignored: r.get::<_, i64>(6)? != 0,
                    quality: r.get::<_, f64>(7)? as f32,
                    rect: (
                        r.get::<_, f64>(9)? as f32,
                        r.get::<_, f64>(10)? as f32,
                        r.get::<_, f64>(11)? as f32,
                        r.get::<_, f64>(12)? as f32,
                    ),
                    score,
                });
            }
            out.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.face_id.cmp(&b.face_id))
            });
            out.truncate(limit);
            Ok(out)
        })
        .unwrap_or_default()
}

/// 一张照片的人脸 + 身份(右栏人物面板用)。
pub struct PhotoFace {
    pub face_id: i64,
    /// 所属簇;未聚类 = None。
    pub cluster: Option<i64>,
    /// 稳定身份;未归属 = None。
    pub person_id: Option<i64>,
    /// 人物姓名(未命名 = None)。
    pub person: Option<String>,
    pub ignored: bool,
    pub quality: f32,
    /// 人脸框(未旋转原图像素): (x1, y1, x2, y2)。
    pub rect: (f32, f32, f32, f32),
}

/// 单张照片的人脸清单,连簇号、身份与姓名(未命名照样返回)。
pub fn photo_faces(store: &Store, photo_id: i64) -> Vec<PhotoFace> {
    store.read(|conn| {
        let mut st = match conn.prepare(
            "SELECT f.id, f.cluster, f.person_id, p.name, f.ignored, COALESCE(f.quality,0.5),
                    f.x1, f.y1, f.x2, f.y2
             FROM faces f LEFT JOIN people p ON p.id = f.person_id
             WHERE f.photo_id = ?1 ORDER BY f.id",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map(params![photo_id], |r| {
            Ok(PhotoFace {
                face_id: r.get(0)?,
                cluster: r.get(1)?,
                person_id: r.get(2)?,
                person: r.get(3)?,
                ignored: r.get::<_, i64>(4)? != 0,
                quality: r.get::<_, f64>(5)? as f32,
                rect: (
                    r.get::<_, f64>(6)? as f32,
                    r.get::<_, f64>(7)? as f32,
                    r.get::<_, f64>(8)? as f32,
                    r.get::<_, f64>(9)? as f32,
                ),
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    })
}

/// 新发一个人物身份号,不撞已有身份与簇号。
fn alloc_person_id(conn: &rusqlite::Connection) -> Result<i64> {
    let a: i64 = conn.query_row("SELECT COALESCE(MAX(person_id),0) FROM faces", [], |r| {
        r.get(0)
    })?;
    let b: i64 = conn.query_row("SELECT COALESCE(MAX(id),0) FROM people", [], |r| r.get(0))?;
    let c: i64 = conn.query_row(
        "SELECT COALESCE(MAX(cluster),0) FROM faces WHERE cluster IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    Ok(a.max(b).max(c) + 1)
}

/// 簇的代表身份(成员多数票;无票返回 None)。
fn cluster_person(conn: &rusqlite::Connection, cluster: i64) -> Option<i64> {
    conn.query_row(
        "SELECT person_id FROM faces WHERE cluster=?1 AND person_id IS NOT NULL
         GROUP BY person_id ORDER BY COUNT(*) DESC, person_id ASC LIMIT 1",
        params![cluster],
        |r| r.get(0),
    )
    .ok()
}

/// 按稳定身份命名(名字全局唯一)。
pub fn rename_person(store: &Store, person_id: i64, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        bail!("名字不能为空");
    }
    if name.chars().count() > 64 {
        bail!("名字过长");
    }
    let name = name.to_string();
    let _g = face_task_guard()?;
    store.write(move |conn| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        let holder: Option<i64> = tx
            .query_row("SELECT id FROM people WHERE name=?1", params![name], |r| {
                r.get(0)
            })
            .ok();
        if let Some(id) = holder {
            if id != person_id {
                bail!("已有人物使用该名字");
            }
        }
        tx.execute(
            "INSERT INTO people(id,name) VALUES(?1,?2)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name",
            params![person_id, name],
        )?;
        tx.execute(
            "UPDATE faces SET assign_src='manual' WHERE person_id=?1",
            params![person_id],
        )?;
        tx.commit()?;
        Ok(())
    })
}

/// 按簇命名:整簇人脸归入同一稳定身份后再写名字。
/// 簇内已有身份(多数票)则沿用,否则新建;命名会把簇里尚未归属的脸
/// 一并扫进该身份,避免「命名一个簇却只命一部分」。
pub fn rename_cluster(store: &Store, cluster: i64, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        bail!("名字不能为空");
    }
    if name.chars().count() > 64 {
        bail!("名字过长");
    }
    let name = name.to_string();
    let _g = face_task_guard()?;
    store.write(move |conn| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        let ids: Vec<i64> = {
            let mut st = tx.prepare("SELECT id FROM faces WHERE cluster=?1")?;
            let rows = st.query_map(params![cluster], |r| r.get(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        if ids.is_empty() {
            bail!("该簇没有人脸");
        }
        if never_conflict(&tx, &ids)? {
            bail!("该簇人脸之间存在「不同人」约束,请先移除冲突约束");
        }
        let pid = match cluster_person(&tx, cluster) {
            Some(p) => p,
            None => alloc_person_id(&tx)?,
        };
        let holder: Option<i64> = tx
            .query_row("SELECT id FROM people WHERE name=?1", params![name], |r| {
                r.get(0)
            })
            .ok();
        if let Some(id) = holder {
            if id != pid {
                bail!("已有人物使用该名字");
            }
        }
        tx.execute(
            "UPDATE faces SET person_id=?1 WHERE cluster=?2",
            params![pid, cluster],
        )?;
        tx.execute(
            "UPDATE faces SET assign_src='manual' WHERE person_id=?1",
            params![pid],
        )?;
        tx.execute(
            "INSERT INTO people(id,name) VALUES(?1,?2)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name",
            params![pid, name],
        )?;
        tx.commit()?;
        Ok(())
    })
}

fn never_conflict(conn: &rusqlite::Connection, group: &[i64]) -> Result<bool> {
    let set: HashSet<i64> = group.iter().copied().collect();
    let mut st = conn.prepare("SELECT a_face,b_face FROM face_constraints WHERE kind='never'")?;
    let mut rows = st.query([])?;
    while let Some(r) = rows.next()? {
        let a: i64 = r.get(0)?;
        let b: i64 = r.get(1)?;
        if set.contains(&a) && set.contains(&b) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 人工把脸归入已有的人物(忽略状态清除;never 冲突拒绝)。
pub fn assign_faces(store: &Store, face_ids: &[i64], person_id: i64) -> Result<usize> {
    if face_ids.is_empty() {
        return Ok(0);
    }
    let _g = face_task_guard()?;
    let mut ids = face_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    store.write(move |conn| -> Result<usize> {
        let tx = conn.unchecked_transaction()?;
        let exists: i64 = tx.query_row(
            "SELECT COUNT(*) FROM faces WHERE person_id=?1",
            params![person_id],
            |r| r.get(0),
        )?;
        if exists == 0 {
            bail!("目标人物不存在,请先刷新人物列表");
        }
        let mut group: Vec<i64> = Vec::new();
        {
            let mut st = tx.prepare("SELECT id FROM faces WHERE person_id=?1")?;
            let mut rows = st.query(params![person_id])?;
            while let Some(r) = rows.next()? {
                group.push(r.get(0)?);
            }
        }
        group.extend(ids.iter().copied());
        if never_conflict(&tx, &group)? {
            bail!("所选人脸与目标人物存在「不同人」约束,请先移除冲突约束");
        }
        let cluster: Option<i64> = tx
            .query_row(
                "SELECT cluster FROM faces WHERE person_id=?1 AND cluster IS NOT NULL
                 GROUP BY cluster ORDER BY COUNT(*) DESC, cluster ASC LIMIT 1",
                params![person_id],
                |r| r.get(0),
            )
            .ok();
        let mut n = 0usize;
        {
            let mut st = tx.prepare(
                "UPDATE faces SET person_id=?1, cluster=COALESCE(?2, cluster), ignored=0,
                 assign_src='manual' WHERE id=?3",
            )?;
            for id in &ids {
                n += st.execute(params![person_id, cluster, id])?;
            }
        }
        tx.commit()?;
        Ok(n)
    })
}

/// 把待命名的一组脸并入目标人物(不新建同名档案,never 冲突拒绝)。
pub fn merge_into_person(
    store: &Store,
    cluster: i64,
    person_id: Option<i64>,
    target: i64,
) -> Result<usize> {
    let _g = face_task_guard()?;
    store.write(move |conn| -> Result<usize> {
        let tx = conn.unchecked_transaction()?;
        let exists: i64 = tx.query_row(
            "SELECT COUNT(*) FROM people WHERE id=?1",
            params![target],
            |r| r.get(0),
        )?;
        if exists == 0 {
            bail!("目标人物不存在,请先刷新人物列表");
        }
        if let Some(pid) = person_id {
            let named: i64 = tx.query_row(
                "SELECT COUNT(*) FROM people WHERE id=?1",
                params![pid],
                |r| r.get(0),
            )?;
            if named > 0 {
                bail!("该组已有独立档案,请改用重命名或移出");
            }
        }
        let target_cluster: Option<i64> = tx
            .query_row(
                "SELECT cluster FROM faces WHERE person_id=?1 AND cluster IS NOT NULL
                 GROUP BY cluster ORDER BY COUNT(*) DESC, cluster ASC LIMIT 1",
                params![target],
                |r| r.get(0),
            )
            .ok();
        let mut group: Vec<i64> = Vec::new();
        {
            let mut st = tx.prepare("SELECT id FROM faces WHERE person_id=?1")?;
            let mut rows = st.query(params![target])?;
            while let Some(r) = rows.next()? {
                group.push(r.get(0)?);
            }
        }
        {
            let sql = if person_id.is_some() {
                "SELECT id FROM faces WHERE person_id=?1"
            } else {
                "SELECT id FROM faces WHERE cluster=?1 AND person_id IS NULL"
            };
            let mut st = tx.prepare(sql)?;
            let mut rows = st.query(params![person_id.unwrap_or(cluster)])?;
            while let Some(r) = rows.next()? {
                group.push(r.get(0)?);
            }
        }
        if never_conflict(&tx, &group)? {
            bail!("所选人脸与目标人物存在「不同人」约束,请先移除冲突约束");
        }
        let n = match person_id {
            Some(pid) => tx.execute(
                "UPDATE faces SET person_id=?1, cluster=COALESCE(?2, cluster), ignored=0,
                 assign_src='manual'
                 WHERE person_id=?3",
                params![target, target_cluster, pid],
            )?,
            None => tx.execute(
                "UPDATE faces SET person_id=?1, cluster=COALESCE(?2, cluster), ignored=0,
                 assign_src='manual'
                 WHERE cluster=?3 AND person_id IS NULL",
                params![target, target_cluster, cluster],
            )?,
        };
        if n == 0 {
            bail!("没有待并入的人脸");
        }
        tx.commit()?;
        Ok(n)
    })
}

/// 人工把脸移出人物,回到未归属状态。
pub fn remove_faces(store: &Store, face_ids: &[i64]) -> Result<usize> {
    if face_ids.is_empty() {
        return Ok(0);
    }
    let _g = face_task_guard()?;
    let mut ids = face_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    store.write(move |conn| -> Result<usize> {
        let tx = conn.unchecked_transaction()?;
        let mut n = 0usize;
        {
            let mut st = tx.prepare(
                "UPDATE faces SET person_id=NULL, cluster=NULL, assign_src=NULL WHERE id=?1",
            )?;
            for id in &ids {
                n += st.execute(params![id])?;
            }
        }
        tx.commit()?;
        Ok(n)
    })
}

/// 忽略/恢复指定人脸;忽略项不参与默认相册与模板。
pub fn set_ignored(store: &Store, face_ids: &[i64], ignored: bool) -> Result<usize> {
    if face_ids.is_empty() {
        return Ok(0);
    }
    let _g = face_task_guard()?;
    let mut ids = face_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let n = store.write(move |conn| -> Result<usize> {
        let tx = conn.unchecked_transaction()?;
        let mut n = 0usize;
        {
            let mut st = tx.prepare("UPDATE faces SET ignored=?1 WHERE id=?2")?;
            for id in &ids {
                n += st.execute(params![ignored as i32, id])?;
            }
        }
        tx.commit()?;
        Ok(n)
    })?;
    let photos = store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) = conn.prepare("SELECT photo_id FROM faces WHERE id=?1") else {
            return out;
        };
        for id in face_ids {
            if let Ok(p) = st.query_row(params![id], |r| r.get::<_, i64>(0)) {
                out.push(p);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    });
    refresh_people_tags(store, photos);
    Ok(n)
}

/// 用所选人脸新建人物(身份号全局唯一;可选名字需未占用)。
pub fn create_person(store: &Store, face_ids: &[i64], name: Option<&str>) -> Result<i64> {
    if face_ids.is_empty() {
        bail!("请先选择人脸");
    }
    let name = name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(n) = &name {
        if n.chars().count() > 64 {
            bail!("名字过长");
        }
    }
    let _g = face_task_guard()?;
    let mut ids = face_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    store.write(move |conn| -> Result<i64> {
        let tx = conn.unchecked_transaction()?;
        if never_conflict(&tx, &ids)? {
            bail!("所选人脸之间存在「不同人」约束,请先移除冲突约束");
        }
        if let Some(n) = &name {
            let used: i64 = tx.query_row(
                "SELECT COUNT(*) FROM people WHERE name=?1",
                params![n],
                |r| r.get(0),
            )?;
            if used > 0 {
                bail!("已有人物使用该名字");
            }
        }
        let pid = alloc_person_id(&tx)?;
        if let Some(n) = &name {
            tx.execute("INSERT INTO people(id,name) VALUES(?1,?2)", params![pid, n])?;
        }
        {
            let mut st = tx.prepare(
                "UPDATE faces SET person_id=?1, cluster=?2, ignored=0,
                 assign_src='manual' WHERE id=?3",
            )?;
            for id in &ids {
                st.execute(params![pid, pid, id])?;
            }
        }
        tx.commit()?;
        Ok(pid)
    })
}

/// 忽略/恢复整个簇(误检簇一键排除)。
pub fn ignore_cluster(store: &Store, cluster: i64, ignored: bool) -> Result<()> {
    let _g = face_task_guard()?;
    store.write(|conn| -> Result<()> {
        conn.execute(
            "UPDATE faces SET ignored=?1 WHERE cluster=?2",
            params![ignored as i32, cluster],
        )?;
        Ok(())
    })?;
    let photos = store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) =
            conn.prepare("SELECT DISTINCT photo_id FROM faces WHERE cluster=?1")
        else {
            return out;
        };
        if let Ok(rows) = st.query_map(params![cluster], |r| r.get::<_, i64>(0)) {
            out.extend(rows.flatten());
        }
        out
    });
    refresh_people_tags(store, photos);
    Ok(())
}

pub fn analyzed_photo_ids(store: &Store) -> Vec<i64> {
    store.read(|conn| {
        let mut st = match conn.prepare("SELECT DISTINCT photo_id FROM faces") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map([], |r| r.get(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    })
}

pub fn photo_ids_done(store: &Store) -> std::collections::HashSet<i64> {
    let sig = effective_sig(store);
    store.read(|conn| {
        let mut out = std::collections::HashSet::new();
        let Ok(mut st) = conn.prepare("SELECT photo_id FROM face_scan WHERE sig=?1 AND state=?2")
        else {
            return out;
        };
        if let Ok(rows) = st.query_map(params![sig, SCAN_DONE], |r| r.get::<_, i64>(0)) {
            out.extend(rows.filter_map(|r| r.ok()));
        }
        out
    })
}

/// 被忽略的簇列表(GUI "显示已忽略" 时恢复用)。
pub fn ignored_clusters(store: &Store) -> Vec<i64> {
    store.read(|conn| {
        let mut st = match conn.prepare(
            "SELECT DISTINCT cluster FROM faces
             WHERE ignored=1 AND cluster IS NOT NULL ORDER BY cluster",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map([], |r| r.get::<_, i64>(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    })
}

/// 清空全部人脸数据(算法版本迁移:旧算法产物作废,整库重扫)。
/// 事务内一并清约束、逐照片状态与 photo 标记,失败整体回滚。
pub fn wipe_faces(store: &Store) -> Result<()> {
    let _guard = face_task_guard()?;
    store.write(|conn| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM face_templates", [])?;
        tx.execute("DELETE FROM face_constraints", [])?;
        tx.execute("DELETE FROM faces", [])?;
        tx.execute("DELETE FROM people", [])?;
        tx.execute("DELETE FROM face_scan", [])?;
        tx.execute(
            "DELETE FROM auto_tags WHERE label_id IN
                 (SELECT id FROM tag_labels WHERE node='人物' OR node LIKE '人物/%')",
            [],
        )?;
        tx.execute("UPDATE photos SET face_done = 0", [])?;
        tx.commit()?;
        Ok(())
    })?;
    Ok(())
}

/// 全库未分析照片数(不含失败项;失败项单独统计)。
pub fn remaining_count(store: &Store) -> usize {
    let sig = effective_sig(store);
    if sig.is_empty() {
        return 0;
    }
    let legacy = legacy_scanned(store) as i32;
    let (scope_sql, scope_pats) = scope_clause(store);
    store.read(|conn| {
        let sql = format!("SELECT COUNT(*) FROM photos p WHERE {TODO_WHERE}{scope_sql}");
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        params.push(Box::new(sig));
        params.push(Box::new(legacy));
        for p in scope_pats {
            params.push(Box::new(p));
        }
        conn.query_row(&sql, rusqlite::params_from_iter(params), |r| r.get::<_, i64>(0))
            .map(|n| n as usize)
            .unwrap_or(0)
    })
}

/// 已检出但未归簇的脸数(增量聚类的漏网项,重聚类可收齐)。
pub fn unassigned_count(store: &Store) -> usize {
    let sig = effective_sig(store);
    if sig.is_empty() {
        return 0;
    }
    let dim = emb_dim(store);
    store.read(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM faces f
               JOIN face_templates t ON t.face_id=f.id AND t.dim=?1 AND t.sig=?2
              WHERE f.cluster IS NULL AND f.ignored=0",
            params![dim, sig],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n as usize)
        .unwrap_or(0)
    })
}

/// 摆正后图里的点 → 未旋转原图坐标(`map_orient` 的逆):
/// 2/3/4 镜像与 180° 自逆;5=转置、7=反对置均自逆;6 与 8 互逆。
/// 入参 (w,h) 是**摆正后**的宽高(orient 5..=8 时 = 原图的 h,w)。
fn map_unorient(x: f32, y: f32, w: f32, h: f32, orient: u32) -> (f32, f32) {
    match orient {
        2 => (w - 1.0 - x, y),
        3 => (w - 1.0 - x, h - 1.0 - y),
        4 => (x, h - 1.0 - y),
        5 => (y, x),
        6 => (y, w - 1.0 - x),
        7 => (h - 1.0 - y, w - 1.0 - x),
        8 => (h - 1.0 - y, x),
        _ => (x, y),
    }
}

/// 后台扫描一批未分析照片:逐张回报进度,失败单独记录并重试。
pub fn scan_batch(
    store: &Store,
    cap: usize,
    on_progress: &(dyn Fn(usize, usize) + Send + Sync),
) -> Result<(usize, usize)> {
    let _guard = face_task_guard()?;
    scan_batch_inner(store, cap, None, on_progress)
}

fn scan_batch_inner(
    store: &Store,
    cap: usize,
    cancel: Option<&AtomicBool>,
    on_progress: &(dyn Fn(usize, usize) + Send + Sync),
) -> Result<(usize, usize)> {
    let sig = effective_sig(store);
    if sig.is_empty() {
        bail!("人脸数据与当前管线不一致,请先确认清除并重建");
    }
    let todo = photos_todo(store, cap);
    let total = todo.len();
    if total == 0 {
        return Ok((0, 0));
    }
    let mut engine = Faces::load(current_algo(store)).map_err(|e| anyhow::anyhow!(e))?;
    let mut face_count = 0usize;
    let mut photo_count = 0usize;
    let mut analyze_failures = 0usize;
    for (i, (pid, path)) in todo.into_iter().enumerate() {
        let step = i + 1;
        if cancel.map(|c| c.load(Ordering::Relaxed)).unwrap_or(false) {
            break;
        }
        if algo_tag(store) != sig {
            bail!("人脸方案已切换,本次扫描停止");
        }
        let rev: Option<(i64, i64)> = store.read(|conn| {
            conn.query_row(
                "SELECT mtime,size FROM photos WHERE id=?1",
                params![pid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()
        });
        let Some((mtime, size)) = rev else {
            store.write(|conn| -> Result<()> {
                conn.execute("DELETE FROM face_scan WHERE photo_id=?1", params![pid])?;
                Ok(())
            })?;
            on_progress(step, total);
            continue;
        };
        mark_running(store, pid, &sig)?;
        let p = std::path::Path::new(&path);
        // EXIF 方向摆正:竖拍手机照不转的话人脸是横躺的,检测/嵌入都会失败。
        let orient = crate::exif::read_orientation(p);
        let img = match image::open(p) {
            Ok(i) => i,
            Err(e) => {
                mark_failed(store, pid, &sig, &format!("解码失败: {e}"))?;
                on_progress(step, total);
                continue;
            }
        };
        let img = if orient == 1 {
            img
        } else {
            crate::thumbs::apply_exif_rotation(img, orient)
        };
        let res = match engine.analyze(&img) {
            Ok(r) => {
                analyze_failures = 0;
                r
            }
            Err(e) => {
                mark_failed(store, pid, &sig, &format!("推理失败: {e}"))?;
                analyze_failures += 1;
                on_progress(step, total);
                if analyze_failures >= MAX_ANALYZE_FAILURES {
                    bail!("人脸推理连续失败,已停止: {e}");
                }
                continue;
            }
        };
        // 库里存未旋转原图坐标;检测框从摆正后的图映射回去。
        let (rw, rh) = (img.width() as f32, img.height() as f32);
        let recs: Vec<FaceRec> = res
            .iter()
            .map(|f| {
                let (ax, ay) = map_unorient(f.det.x0, f.det.y0, rw, rh, orient);
                let (bx, by) = map_unorient(f.det.x1, f.det.y1, rw, rh, orient);
                FaceRec {
                    x1: ax.min(bx),
                    y1: ay.min(by),
                    x2: ax.max(bx),
                    y2: ay.max(by),
                    score: f.det.score,
                    quality: face_quality(&img, &f.det),
                    emb: f.emb.clone(),
                }
            })
            .collect();
        let committed = commit_photo(store, pid, &sig, mtime, size, &recs)?;
        if !committed {
            if algo_tag(store) != sig {
                bail!("人脸方案已切换,本次扫描停止");
            }
            on_progress(step, total);
            continue;
        }
        face_count += recs.len();
        photo_count += 1;
        on_progress(step, total);
    }
    let thr = cluster_thr(store);
    assign_clusters(store, thr)?;
    merge_similar_clusters(store, thr)?;
    Ok((photo_count, face_count))
}

/// 连续扫描直到队列空;进度按全库口径回报(累计已处理, 开扫时待处理总数),可取消。
pub fn scan_all(
    store: &Store,
    cap: usize,
    cancel: &std::sync::atomic::AtomicBool,
    on_progress: &(dyn Fn(usize, usize) + Send + Sync),
) -> Result<(usize, usize)> {
    let _guard = face_task_guard()?;
    let total = remaining_count(store);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let (mut photo_total, mut face_total) = (0usize, 0usize);
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let before = done.load(Ordering::Relaxed);
        let (p, f) = scan_batch_inner(store, cap, Some(cancel), &|step, _| {
            let d = before + step;
            done.store(d, Ordering::Relaxed);
            on_progress(d, total);
        })?;
        photo_total += p;
        face_total += f;
        // 本批一张都没走 = 队列空;整批失败也要继续,不能按成功数判停。
        if done.load(Ordering::Relaxed) == before {
            break;
        }
    }
    // 收尾补一次全量重聚类:增量聚类会留下未归簇的脸,批处理能收齐。
    if !cancel.load(Ordering::Relaxed) {
        recluster_locked(store, cluster_thr(store))?;
    }
    if photo_total > 0 {
        refresh_people_tags(store, analyzed_photo_ids(store));
    }
    Ok((photo_total, face_total))
}

/// 人物标签由人脸数派生,人脸状态变化后按当前标签版本刷新。
fn refresh_people_tags(store: &Store, photos: Vec<i64>) {
    if photos.is_empty() {
        return;
    }
    let ver = crate::ai_queue::stage_version(store, crate::ai_queue::Stage::Tags);
    let _ = crate::ai_tags::sync_people_tags(store, &photos, &ver);
}

/// 人脸数据待重建期间,派生的人物标签一并作废(旧结果不参与检索)。
pub fn drop_people_tags(store: &Store) {
    let _ = store.write(|conn| {
        conn.execute(
            "DELETE FROM auto_tags WHERE label_id IN
                 (SELECT id FROM tag_labels WHERE node='人物' OR node LIKE '人物/%')",
            [],
        )
    });
}

/// 重聚类分组累加器(加权和 + 同图约束所需的照片集合)。
struct ReclusterGroup {
    sum: Vec<f32>,
    wsum: f32,
    reps: Vec<Member>,
    photos: Vec<i64>,
    faces: Vec<i64>,
}

fn group_add(g: &mut ReclusterGroup, m: &Member, face: i64, thr: f32) {
    member_add(&mut g.sum, &mut g.wsum, &mut g.reps, m, thr);
    if !g.photos.contains(&m.photo) {
        g.photos.push(m.photo);
    }
    g.faces.push(face);
}

fn linked_faces(map: &HashMap<i64, Vec<i64>>, face: i64, others: &[i64]) -> bool {
    map.get(&face)
        .map(|ns| others.iter().any(|o| ns.contains(o)))
        .unwrap_or(false)
}

/// 全量重聚类:只按向量重排簇号,身份由成员旧 person_id 多数票继承。
pub fn recluster_all(store: &Store, thr: f32) -> Result<usize> {
    let _guard = face_task_guard()?;
    recluster_locked(store, thr)
}

/// 全量重聚类主体(调用方已持人脸任务互斥)。
fn recluster_locked(store: &Store, thr: f32) -> Result<usize> {
    let sig = effective_sig(store);
    if sig.is_empty() {
        return Ok(0);
    }
    let dim = emb_dim(store);
    // (face_id, 内存副本, 旧簇, 旧身份)
    let mut faces: Vec<(i64, Member, Option<i64>, Option<i64>)> =
        store.read(|conn| -> Result<_> {
            let mut st = conn.prepare(
                "SELECT f.id, t.vec, COALESCE(f.quality,0.5), f.cluster, f.person_id,
                        f.photo_id, COALESCE(f.assign_src,'') FROM faces f
                 JOIN face_templates t ON t.face_id=f.id AND t.dim=?1 AND t.sig=?2
                 WHERE f.ignored=0",
            )?;
            let mut rows = st.query(params![dim, sig])?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? {
                let id: i64 = r.get(0)?;
                let blob: Vec<u8> = r.get(1)?;
                let q: f64 = r.get(2)?;
                let old: Option<i64> = r.get(3)?;
                let pid: Option<i64> = r.get(4)?;
                let photo_id: i64 = r.get(5)?;
                let src: String = r.get(6)?;
                out.push((
                    id,
                    Member {
                        v: blob_to_vec(&blob),
                        q: q as f32,
                        photo: photo_id,
                        manual: src == "manual",
                    },
                    old,
                    pid,
                ));
            }
            Ok(out)
        })?;
    if faces.is_empty() {
        return Ok(0);
    }
    let mut next_id: i64 = store.read(|conn| -> Result<_> {
        let max_cluster: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(cluster),0) FROM faces WHERE cluster IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let max_person: i64 = conn
            .query_row("SELECT COALESCE(MAX(person_id),0) FROM faces", [], |r| r.get(0))
            .unwrap_or(0);
        let max_people: i64 = conn
            .query_row("SELECT COALESCE(MAX(id),0) FROM people", [], |r| r.get(0))
            .unwrap_or(0);
        Ok(max_cluster.max(max_person).max(max_people) + 1)
    })?;
    faces.sort_by(|a, b| {
        b.1
            .q
            .partial_cmp(&a.1.q)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    let cons = crate::ai_review::all(store);
    let mut never: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut must: HashMap<i64, Vec<i64>> = HashMap::new();
    for (a, b, k) in &cons {
        let map = match k {
            crate::ai_review::Kind::Must => &mut must,
            crate::ai_review::Kind::Never => &mut never,
        };
        map.entry(*a).or_default().push(*b);
        map.entry(*b).or_default().push(*a);
    }
    let mut groups: Vec<ReclusterGroup> = Vec::new();
    let mut assign: HashMap<i64, usize> = HashMap::new();
    // 所有脸一视同仁按质量降序贪心分组,身份不参与分组。
    for (fid, m, _, _) in &faces {
        let fid = *fid;
        if assign.contains_key(&fid) {
            continue;
        }
        let mut cents: Vec<(usize, f32)> = Vec::new();
        for (gi, g) in groups.iter().enumerate() {
            if linked_faces(&never, fid, &g.faces) {
                continue;
            }
            let same_photo = g.photos.contains(&m.photo);
            if same_photo && !linked_faces(&must, fid, &g.faces) {
                continue;
            }
            if let Some(c) = sum_centroid(&g.sum, g.wsum) {
                cents.push((gi, cos(&c, &m.v)));
            }
        }
        cents.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        cents.truncate(CAND_K);
        let mut scored: Vec<(usize, f32, bool)> = cents
            .into_iter()
            .filter(|(_, c)| *c >= thr - CAND_SLACK)
            .map(|(gi, _)| {
                let g = &groups[gi];
                let thr_eff = join_thr(thr, g.faces.len());
                let (support, agg) = rep_claim(&g.reps, &m.v, thr_eff);
                (
                    gi,
                    agg,
                    agg >= thr_eff && support >= required_support(&g.reps),
                )
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        // 重聚类必须给每张脸落位:没有存疑搁置规则,取第一个合格组。
        match scored.iter().find(|(_, _, ok)| *ok).map(|(gi, _, _)| *gi) {
            Some(gi) => {
                group_add(&mut groups[gi], m, fid, thr);
                assign.insert(fid, gi);
            }
            None => {
                groups.push(ReclusterGroup {
                    sum: vec![0.0; dim as usize],
                    wsum: 0.0,
                    reps: Vec::new(),
                    photos: Vec::new(),
                    faces: Vec::new(),
                });
                let gi = groups.len() - 1;
                group_add(&mut groups[gi], m, fid, thr);
                assign.insert(fid, gi);
            }
        }
    }
    let final_groups: Vec<Vec<i64>> = if cons.is_empty() {
        groups.iter().map(|g| g.faces.clone()).collect()
    } else {
        let base: Vec<Vec<i64>> = groups.iter().map(|g| g.faces.clone()).collect();
        crate::ai_review::apply(&cons, &base)
    };
    let old_cluster: HashMap<i64, Option<i64>> = faces.iter().map(|f| (f.0, f.2)).collect();
    let old_person: HashMap<i64, Option<i64>> = faces.iter().map(|f| (f.0, f.3)).collect();
    // 每组按旧身份多数票选代表;平票取小号。
    let mut votes: Vec<(Option<i64>, usize)> = Vec::with_capacity(final_groups.len());
    for g in &final_groups {
        let mut tally: HashMap<i64, usize> = HashMap::new();
        for fid in g {
            if let Some(pid) = old_person.get(fid).copied().flatten() {
                *tally.entry(pid).or_insert(0) += 1;
            }
        }
        let pick = tally
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
            .map(|(p, n)| (*p, *n));
        votes.push((pick.map(|(p, _)| p), pick.map(|(_, n)| n).unwrap_or(0)));
    }
    // 同一身份只给净票最多的组(簇分裂时身份随多数走)。
    let mut winners: HashMap<i64, (usize, usize)> = HashMap::new();
    for (gi, (pid, n)) in votes.iter().enumerate() {
        if let Some(p) = pid {
            let e = winners.entry(*p).or_insert((gi, *n));
            if *n > e.1 || (*n == e.1 && gi < e.0) {
                *e = (gi, *n);
            }
        }
    }
    let mut used: HashSet<i64> = HashSet::new();
    let mut writes: Vec<(i64, i64, Option<i64>)> = Vec::with_capacity(faces.len());
    for (gi, g) in final_groups.iter().enumerate() {
        if g.is_empty() {
            continue;
        }
        // 簇号按旧簇号多数继承,已占用的号另发新号。
        let mut cs: HashMap<i64, usize> = HashMap::new();
        for fid in g {
            if let Some(oc) = old_cluster.get(fid).copied().flatten() {
                *cs.entry(oc).or_insert(0) += 1;
            }
        }
        let pick = cs
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
            .map(|(c, _)| *c);
        let cid = match pick {
            Some(c) if !used.contains(&c) => c,
            _ => {
                let id = next_id;
                next_id += 1;
                id
            }
        };
        used.insert(cid);
        let mut pid = votes[gi].0;
        if let Some(p) = pid {
            if winners.get(&p).map(|w| w.0) != Some(gi) {
                pid = None;
            }
        }
        for fid in g {
            writes.push((*fid, cid, pid));
        }
    }
    let count = used.len();
    store.write(move |conn| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        {
            let mut st = tx.prepare_cached(
                "UPDATE faces SET cluster=?1, person_id=?2,
                 assign_src=CASE WHEN ?2 IS NULL THEN NULL
                                 WHEN assign_src IS NULL THEN 'auto'
                                 ELSE assign_src END
                 WHERE id=?3",
            )?;
            for (fid, cid, pid) in &writes {
                st.execute(params![cid, pid, fid])?;
            }
        }
        tx.commit()?;
        Ok(())
    })?;
    let merged = merge_similar_clusters(store, thr)?;
    Ok(count.saturating_sub(merged))
}

/// 未命名身份的命名建议;只读,不写库。
#[derive(Clone, Debug)]
pub struct Suggestion {
    pub cluster: i64,
    pub person_id: Option<i64>,
    /// 建议并入的已命名身份。
    pub target_person_id: i64,
    pub name: String,
    pub score: f32,
    pub count: usize,
}

/// 生成命名建议(score 降序);只读,不写库。
pub fn face_suggestions(store: &Store, thr: f32) -> Vec<Suggestion> {
    let sig = effective_sig(store);
    if sig.is_empty() {
        return Vec::new();
    }
    let dim = emb_dim(store);
    store.read(|conn| {
        // 已命名人物 -> (name, 成员列表)
        let mut named: HashMap<i64, (String, Vec<Member>)> = HashMap::new();
        {
            let mut st = match conn.prepare(
                "SELECT f.person_id, p.name, t.vec, COALESCE(f.quality,0.5),
                        f.photo_id, COALESCE(f.assign_src,'') FROM faces f
                 JOIN face_templates t ON t.face_id=f.id AND t.dim=?1 AND t.sig=?2
                 JOIN people p ON p.id=f.person_id
                 WHERE f.ignored=0",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let mut rows = match st.query(params![dim, sig]) {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            loop {
                let Ok(Some(r)) = rows.next() else { break };
                let (Ok(pid), Ok(name), Ok(blob), Ok(q), Ok(photo_id), Ok(src)) = (
                    r.get::<_, i64>(0),
                    r.get::<_, String>(1),
                    r.get::<_, Vec<u8>>(2),
                    r.get::<_, f64>(3),
                    r.get::<_, i64>(4),
                    r.get::<_, String>(5),
                ) else {
                    continue;
                };
                named
                    .entry(pid)
                    .or_insert_with(|| (name, Vec::new()))
                    .1
                    .push(Member {
                        v: blob_to_vec(&blob),
                        q: q as f32,
                        photo: photo_id,
                        manual: src == "manual",
                    });
            }
        }
        // 待命名组按(簇,身份)分开,避免把不同身份的脸混成一条建议。
        let mut unnamed: HashMap<(i64, Option<i64>), Vec<Member>> = HashMap::new();
        {
            let mut st = match conn.prepare(
                "SELECT f.cluster, f.person_id, t.vec, COALESCE(f.quality,0.5),
                        f.photo_id, COALESCE(f.assign_src,'') FROM faces f
                 JOIN face_templates t ON t.face_id=f.id AND t.dim=?1 AND t.sig=?2
                 LEFT JOIN people p ON p.id=f.person_id
                 WHERE p.id IS NULL AND f.cluster IS NOT NULL AND f.ignored=0",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let mut rows = match st.query(params![dim, sig]) {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            loop {
                let Ok(Some(r)) = rows.next() else { break };
                let (Ok(cid), Ok(pid), Ok(blob), Ok(q), Ok(photo_id), Ok(src)) = (
                    r.get::<_, i64>(0),
                    r.get::<_, Option<i64>>(1),
                    r.get::<_, Vec<u8>>(2),
                    r.get::<_, f64>(3),
                    r.get::<_, i64>(4),
                    r.get::<_, String>(5),
                ) else {
                    continue;
                };
                unnamed
                    .entry((cid, pid))
                    .or_default()
                    .push(Member {
                        v: blob_to_vec(&blob),
                        q: q as f32,
                        photo: photo_id,
                        manual: src == "manual",
                    });
            }
        }
        let mut out: Vec<Suggestion> = unnamed
            .iter()
            .filter_map(|(&(cid, pid), vs)| {
                // 双方都用代表模板集:低质量与近重复脸不再直接参与建议打分。
                let probes: Vec<Member> = rep_select(vs, thr).into_iter().take(2).collect();
                if probes.is_empty() {
                    return None;
                }
                let mut scored: Vec<(i64, String, f32)> = named
                    .iter()
                    .filter_map(|(&target_pid, (name, tpl))| {
                        let reps = rep_select(tpl, thr);
                        if reps.is_empty() {
                            return None;
                        }
                        let mut total = 0.0f32;
                        let mut support = 0usize;
                        for p in &probes {
                            let best = reps
                                .iter()
                                .map(|r| cos(&p.v, &r.v))
                                .fold(f32::MIN, f32::max);
                            if best >= thr {
                                support += 1;
                            }
                            total += best;
                        }
                        (support > 0).then_some((
                            target_pid,
                            name.clone(),
                            total / probes.len() as f32,
                        ))
                    })
                    .collect();
                scored.sort_by(|a, b| {
                    b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal)
                });
                let (target_pid, name, score) = scored.first()?.clone();
                if score < thr {
                    return None;
                }
                if let Some(second) = scored.get(1) {
                    if score - second.2 < SUGGEST_MARGIN {
                        return None;
                    }
                }
                Some(Suggestion {
                    cluster: cid,
                    person_id: pid,
                    target_person_id: target_pid,
                    name,
                    score,
                    count: vs.len(),
                })
            })
            .collect();
        out.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> Store {
        Store::open_mem().unwrap()
    }

    /// 串行化会争抢人脸任务锁的用例(测试默认并行)。
    fn test_serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 直接插入一张人脸(向量零填充到方案维度,身份默认随簇号)。
    fn insert_face(
        store: &Store,
        photo_id: i64,
        vec: &[f32],
        quality: f32,
        cluster: Option<i64>,
    ) -> i64 {
        insert_face_with(store, photo_id, vec, quality, cluster, cluster)
    }

    /// 直接插入一张人脸,簇号与身份可分别指定。
    fn insert_face_with(
        store: &Store,
        photo_id: i64,
        vec: &[f32],
        quality: f32,
        cluster: Option<i64>,
        person_id: Option<i64>,
    ) -> i64 {
        let dim = emb_dim(store) as usize;
        let mut full = vec.to_vec();
        full.resize(dim.max(vec.len()), 0.0);
        let mut blob = Vec::new();
        for v in &full {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        let c = store.test_conn();
        // 外键约束开着,脸必须挂在真实照片行上。
        c.execute(
            "INSERT OR IGNORE INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.',0,0)",
            params![photo_id, format!("p{photo_id}.jpg")],
        )
        .unwrap();
        c.execute(
            "INSERT INTO faces(photo_id,x1,y1,x2,y2,score,quality,ignored,cluster,person_id)
             VALUES(?1,10,10,50,50,0.9,?2,0,?3,?4)",
            params![photo_id, quality as f64, cluster, person_id],
        )
        .unwrap();
        let fid = c.last_insert_rowid();
        c.execute(
            "INSERT INTO face_templates(face_id,dim,vec,sig) VALUES(?1,?2,?3,?4)",
            params![fid, full.len() as i64, blob, algo_tag(store)],
        )
        .unwrap();
        fid
    }

    /// 读取人物档案快照。
    fn people_rows(store: &Store) -> Vec<(i64, String)> {
        store.read(|conn| {
            let mut st = conn.prepare("SELECT id,name FROM people ORDER BY id").unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        })
    }

    /// 读取某张脸的当前身份。
    fn person_of(store: &Store, fid: i64) -> Option<i64> {
        store
            .test_conn()
            .query_row("SELECT person_id FROM faces WHERE id=?1", params![fid], |r| {
                r.get(0)
            })
            .unwrap()
    }

    /// 读取某张脸的当前簇号。
    fn cluster_of(store: &Store, fid: i64) -> Option<i64> {
        store
            .test_conn()
            .query_row("SELECT cluster FROM faces WHERE id=?1", params![fid], |r| {
                r.get(0)
            })
            .unwrap()
    }

    /// 读取某张脸的归属来源(assign_src)。
    fn assign_src_of(store: &Store, fid: i64) -> Option<String> {
        store
            .test_conn()
            .query_row("SELECT assign_src FROM faces WHERE id=?1", params![fid], |r| {
                r.get(0)
            })
            .unwrap()
    }

    /// 测试用 save_face 包装:向量零填充到方案维度(不改余弦,过 t.dim 过滤)。
    fn save_face_t(store: &Store, photo_id: i64, vec: &[f32], quality: f32) -> anyhow::Result<i64> {
        let dim = emb_dim(store) as usize;
        let mut full = vec.to_vec();
        full.resize(dim.max(vec.len()), 0.0);
        save_face(store, photo_id, 0.0, 0.0, 1.0, 1.0, 0.9, quality, &full)
    }

    #[test]
    fn cluster_roundtrip() {
        let store = test_store();
        let conn = store.test_conn();
        for (i, p) in ["x.jpg", "y.jpg", "z.jpg"].iter().enumerate() {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.',0,1)",
                params![(90 + i) as i64, p],
            )
            .unwrap();
        }
        drop(conn);
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.9, 0.1, 0.0];
        let c = vec![0.0, 1.0, 0.0];
        save_face_t(&store, 90, &a, 0.8).unwrap();
        save_face_t(&store, 91, &b, 0.8).unwrap();
        save_face_t(&store, 92, &c, 0.8).unwrap();
        let n = assign_clusters(&store, 0.5).unwrap();
        let rows = people_overview(&store);
        assert_eq!(n, 2);
        assert_eq!(rows.len(), 2, "应聚成 2 簇");
        assert!(rows.iter().all(|r| r.count >= 1));
        assert!(rows.iter().all(|r| r.quality > 0.0), "平均质量应记录");
    }

    /// 质量评分区分清晰/模糊(锐度项拉普拉斯方差)。
    #[test]
    fn quality_orders_sharp_over_blurry() {
        let mut img = image::RgbImage::new(240, 240);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let c = if (x / 4 + y / 4) % 2 == 0 {
                20u8
            } else {
                235u8
            };
            *p = image::Rgb([c, c, c]);
        }
        let sharp = image::DynamicImage::ImageRgb8(img.clone());
        let blurry = image::DynamicImage::ImageRgb8(image::imageops::resize(
            &img,
            24,
            24,
            image::imageops::FilterType::Triangle,
        ))
        .resize(240, 240, image::imageops::FilterType::Triangle);
        let det = Det {
            x0: 60.0,
            y0: 60.0,
            x1: 180.0,
            y1: 180.0,
            score: 0.9,
            kps: [(0.0, 0.0); 5],
        };
        let qs = face_quality(&sharp, &det);
        let qb = face_quality(&blurry, &det);
        assert!(qs > 0.6, "清晰脸应高分: {qs}");
        assert!(qs > qb + 0.1, "清晰应显著高于模糊: {qs} vs {qb}");
    }

    /// 未命名簇按质心相似度获得命名建议;不相似的没有。
    #[test]
    fn suggestion_for_similar_unnamed_cluster() {
        let store = test_store();
        let conn = store.test_conn();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(2,'b.jpg','.',0,1)",
            [],
        )
        .unwrap();
        drop(conn);
        // 簇 1 = 张三(已命名)
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        rename_cluster(&store, 1, "张三").unwrap();
        // 簇 2 与张三高度相似(未命名);簇 3 正交(不应建议)
        insert_face(&store, 1, &[0.95, 0.31, 0.0], 1.0, Some(2));
        insert_face(&store, 2, &[0.0, 1.0, 0.0], 1.0, Some(3));
        let sugs = face_suggestions(&store, 0.55);
        assert_eq!(
            sugs.len(),
            1,
            "只应有 1 条建议: {:?}",
            sugs.iter().map(|s| (&s.name, s.score)).collect::<Vec<_>>()
        );
        assert_eq!(sugs[0].name, "张三");
        assert_eq!(sugs[0].cluster, 2);
        assert_eq!(sugs[0].person_id, Some(2), "建议应落在待命名身份所在簇");
        assert_eq!(sugs[0].target_person_id, 1, "建议指向已命名的张三");
        assert!(sugs[0].score > 0.9, "score={}", sugs[0].score);
        assert_eq!(sugs[0].count, 1);
    }

    /// 与两个已命名人物都相近时,余量不足不给建议;明显更近才给。
    #[test]
    fn suggestion_requires_margin_over_second_person() {
        let store = test_store();
        let conn = store.test_conn();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)",
            [],
        )
        .unwrap();
        drop(conn);
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        rename_cluster(&store, 1, "甲").unwrap();
        insert_face(&store, 1, &[0.9, 0.4359, 0.0], 1.0, Some(2));
        rename_cluster(&store, 2, "乙").unwrap();
        // 12° 处:与甲 0.978、与乙 0.971,余量不足。
        insert_face(&store, 1, &[0.978, 0.208, 0.0], 1.0, Some(3));
        assert!(
            face_suggestions(&store, 0.5).is_empty(),
            "两个人物都相近时不应给建议"
        );
        // 10° 处:与甲 0.985、与乙 0.9605,余量足够。
        store
            .test_conn()
            .execute(
                "UPDATE faces SET cluster=NULL WHERE cluster=3",
                [],
            )
            .unwrap();
        insert_face(&store, 1, &[0.985, 0.1736, 0.0], 1.0, Some(3));
        let sugs = face_suggestions(&store, 0.5);
        assert_eq!(sugs.len(), 1);
        assert_eq!(sugs[0].cluster, 3);
        assert_eq!(sugs[0].person_id, Some(3));
        assert_eq!(sugs[0].target_person_id, 1);
        assert_eq!(sugs[0].name, "甲");
    }

    /// 应用建议是把脸并入已有身份,不再新建同名档案。
    #[test]
    fn suggestion_apply_merges_into_existing_person() {
        let _g = test_serial();
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg"]);
        let named = insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        rename_cluster(&store, 1, "张三").unwrap();
        let other = insert_face(&store, 2, &[0.95, 0.31, 0.0], 1.0, Some(2));
        let sugs = face_suggestions(&store, 0.55);
        assert_eq!(sugs.len(), 1);
        let s = &sugs[0];
        assert_eq!(s.target_person_id, 1);
        let n = merge_into_person(&store, s.cluster, s.person_id, s.target_person_id).unwrap();
        assert_eq!(n, 1);
        assert_eq!(person_of(&store, named), Some(1));
        assert_eq!(person_of(&store, other), Some(1), "脸迁到目标身份");
        assert_eq!(people_rows(&store), vec![(1, "张三".to_string())]);
        assert_eq!(faces_for_person(&store, 1, false, 0.0).len(), 2);
        assert!(face_suggestions(&store, 0.55).is_empty(), "并入后不再有建议");
    }

    /// 未归属组按簇并入,同簇已有身份的脸不被带走。
    #[test]
    fn merge_into_person_handles_unowned_group() {
        let _g = test_serial();
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg"]);
        insert_face_with(&store, 1, &[1.0, 0.0], 1.0, Some(1), Some(11));
        rename_person(&store, 11, "张三").unwrap();
        let belonging = insert_face_with(&store, 2, &[1.0, 0.0], 1.0, Some(2), Some(22));
        let unowned = insert_face_with(&store, 2, &[1.0, 0.0], 1.0, Some(2), None);
        assert_eq!(merge_into_person(&store, 2, None, 11).unwrap(), 1);
        assert_eq!(person_of(&store, unowned), Some(11));
        assert_eq!(person_of(&store, belonging), Some(22));
        assert_eq!(faces_for_person(&store, 11, false, 0.0).len(), 2);
        assert!(merge_into_person(&store, 2, None, 999).is_err());
    }

    /// 并入受「不同人」约束保护,失败时身份不变。
    #[test]
    fn merge_into_person_respects_never_constraint() {
        let _g = test_serial();
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg"]);
        let a = insert_face_with(&store, 1, &[1.0, 0.0], 1.0, Some(1), Some(11));
        rename_person(&store, 11, "张三").unwrap();
        let b = insert_face_with(&store, 2, &[1.0, 0.0], 1.0, Some(2), Some(22));
        crate::ai_review::add(&store, a, b, crate::ai_review::Kind::Never).unwrap();
        assert!(merge_into_person(&store, 2, Some(22), 11).is_err());
        assert_eq!(person_of(&store, b), Some(22));
    }

    /// 人物列表按身份合并:跨簇同一身份一行,混合簇按身份分列。
    #[test]
    fn people_overview_groups_by_person_id() {
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg", "c.jpg"]);
        insert_face_with(&store, 1, &[1.0, 0.0], 1.0, Some(1), Some(11));
        insert_face_with(&store, 2, &[1.0, 0.0], 1.0, Some(2), Some(11));
        insert_face_with(&store, 3, &[0.0, 1.0], 1.0, Some(2), Some(22));
        insert_face_with(&store, 3, &[0.0, 1.0], 1.0, Some(2), None);
        rename_person(&store, 11, "张三").unwrap();
        let rows = people_overview(&store);
        assert_eq!(rows.len(), 3, "两身份 + 一个未归属组");
        let zhang = rows.iter().find(|r| r.person_id == Some(11)).unwrap();
        assert_eq!(zhang.count, 2, "跨簇身份合成一行");
        assert_eq!(zhang.person.as_deref(), Some("张三"));
        assert_eq!(zhang.cluster, 1, "代表簇取最小号");
        let mut cs = zhang.clusters.clone();
        cs.sort_unstable();
        assert_eq!(cs, vec![1, 2]);
        assert_eq!(faces_for_person(&store, 11, false, 0.0).len(), 2);
        let unowned = rows.iter().find(|r| r.person_id.is_none()).unwrap();
        assert_eq!(unowned.count, 1);
        assert_eq!(unowned.clusters, vec![2]);
    }

    /// 逐照片状态按签名隔离:当前签名成功/失败的项不再自动重跑,视频不进队列。
    #[test]
    fn photos_todo_uses_sig_state_and_kind() {
        let store = test_store();
        let sig = algo_tag(&store);
        let conn = store.test_conn();
        for (id, path, kind) in [(1, "a.jpg", 0), (2, "b.jpg", 0), (3, "c.mp4", 1), (4, "d.jpg", 0)] {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,kind) VALUES(?1,?2,'.',0,1,?3)",
                params![id, path, kind],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO face_scan(photo_id,sig,state) VALUES(2,?1,2)",
            params![sig],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO face_scan(photo_id,sig,state) VALUES(4,?1,3)",
            params![sig],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO face_scan(photo_id,sig,state) VALUES(1,'old',2)",
            [],
        )
        .unwrap();
        drop(conn);
        let todo = photos_todo(&store, 10);
        assert_eq!(
            todo.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![1],
            "旧签名的成功项仍需重跑: {todo:?}"
        );
        assert_eq!(remaining_count(&store), 1);
        assert_eq!(failed_count(&store), 1);
        assert_eq!(retry_failed(&store).unwrap(), 1);
        let todo = photos_todo(&store, 10);
        assert!(todo.iter().any(|(id, _)| *id == 4), "重试后失败项回到队列");
        assert_eq!(failed_count(&store), 0);
    }

    /// 待办按目录优先级排序;只常用目录范围按导入目录前缀过滤。
    #[test]
    fn photos_todo_scope_filter_and_priority() {
        let store = test_store();
        let conn = store.test_conn();
        conn.execute(
            "INSERT INTO dir_intent(path,state,source) VALUES('D:\\Scope','imported','import')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO ai_priority_rules(prefix,prio) VALUES('d:\\scope',1)", [])
            .unwrap();
        conn.execute("INSERT INTO ai_priority_rules(prefix,prio) VALUES('d:\\other',2)", [])
            .unwrap();
        for (id, path) in [
            (1, "D:\\Scope\\sub\\a.jpg"),
            (2, "D:\\Other\\b.jpg"),
            (3, "D:\\Scope\\c.jpg"),
        ] {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,kind) VALUES(?1,?2,'.',0,1,0)",
                params![id, path],
            )
            .unwrap();
        }
        drop(conn);
        let all = photos_todo(&store, 10);
        assert_eq!(
            all.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![1, 3, 2],
            "常用目录优先级在前,同级按 id: {all:?}"
        );
        assert_eq!(remaining_count(&store), 3);
        set_scan_scope(&store, true);
        let scoped = photos_todo(&store, 10);
        let ids: Vec<_> = scoped.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&1) && ids.contains(&3) && !ids.contains(&2), "范围外应排除: {ids:?}");
        assert_eq!(remaining_count(&store), 2);
    }

    /// 扫描范围设置往返:默认全库,common/all 互切并落库记忆。
    #[test]
    fn scan_scope_roundtrip() {
        let store = test_store();
        assert!(!scan_scope_common(&store), "默认应为全库");
        set_scan_scope(&store, true);
        assert!(scan_scope_common(&store));
        assert_eq!(store.get_setting("face_scan_scope").as_deref(), Some("common"));
        set_scan_scope(&store, false);
        assert!(!scan_scope_common(&store));
        assert_eq!(store.get_setting("face_scan_scope").as_deref(), Some("all"));
    }

    /// 范围过滤片段:全库为空;常用目录每个目录贡献一条子树 LIKE 参数(从 ?3 起)。
    #[test]
    fn scope_clause_patterns() {
        let store = test_store();
        let (sql, pats) = scope_clause(&store);
        assert!(sql.is_empty() && pats.is_empty(), "全库范围不应有过滤片段");
        store
            .test_conn()
            .execute(
                "INSERT INTO dir_intent(path,state,source) VALUES('Z:\\CommonTest','imported','import')",
                [],
            )
            .unwrap();
        set_scan_scope(&store, true);
        let (sql, pats) = scope_clause(&store);
        let dirs = crate::db::common_dirs(&store);
        assert_eq!(pats.len(), dirs.len(), "每个常用目录一个 LIKE 参数");
        assert!(sql.contains("LIKE ?3"), "占位符应从 ?3 起: {sql}");
        assert!(
            pats.iter().any(|p| p.contains("CommonTest") && p.ends_with('%')),
            "导入目录应生成子树通配: {pats:?}"
        );
    }

    /// 待办优先级:最长前缀先命中(深目录盖过浅目录),未命中落默认档,同级按 id。
    #[test]
    fn priority_order_longest_prefix() {
        let store = test_store();
        let conn = store.test_conn();
        conn.execute(
            "INSERT INTO ai_priority_rules(prefix,prio) VALUES('d:\\photos',5)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ai_priority_rules(prefix,prio) VALUES('d:\\photos\\family',1)",
            [],
        )
        .unwrap();
        for (id, path) in [
            (1, "D:\\Photos\\misc\\a.jpg"),
            (2, "D:\\Photos\\Family\\b.jpg"),
            (3, "D:\\Other\\c.jpg"),
            (4, "D:\\Photos\\Family\\d.jpg"),
        ] {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,kind) VALUES(?1,?2,'.',0,1,0)",
                params![id, path],
            )
            .unwrap();
        }
        drop(conn);
        let ids: Vec<_> = photos_todo(&store, 10).iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![2, 4, 1, 3], "最长前缀优先,同级按 id: {ids:?}");
    }

    /// 自动续扫判据:空库不续;有待办时取决于模型就位;待重建一律不续。
    #[test]
    fn should_resume_predicate() {
        let store = test_store();
        assert!(!should_resume(&store), "空库无待办,不续扫");
        store
            .test_conn()
            .execute(
                "INSERT INTO photos(id,path,parent,mtime,size,kind) VALUES(1,'D:\\X\\a.jpg','.',0,1,0)",
                [],
            )
            .unwrap();
        let present = crate::ai::face::Faces::models_present(current_algo(&store));
        assert_eq!(
            should_resume(&store),
            present,
            "有待办+非重建时,续扫与否应完全取决于模型就位"
        );
        let fid = insert_face(&store, 1, &[1.0, 0.0, 0.0], 0.5, None);
        store
            .test_conn()
            .execute(
                "UPDATE face_templates SET sig='stale-sig' WHERE face_id=?1",
                params![fid],
            )
            .unwrap();
        assert!(pending_rebuild(&store), "旧签名模板应触发待重建");
        assert!(!should_resume(&store), "待重建时不自动续扫");
    }

    /// 空队列:scan_all 立即返回 (0,0),不触发进度回调(此路径无需加载模型)。
    #[test]
    fn scan_all_empty_queue() {
        let store = test_store();
        let cancel = AtomicBool::new(false);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let (p, f) = scan_all(&store, 200, &cancel, &|_, _| {
            calls.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
        assert_eq!((p, f), (0, 0));
        assert_eq!(calls.load(Ordering::Relaxed), 0, "空队列不应回报进度");
        assert_eq!(remaining_count(&store), 0);
    }

    /// 真实模型端到端:跨批进度单调累计到 total、扫完待办归零、开扫即取消不耗队列。
    #[test]
    #[ignore = "需要真实人脸模型与临时图片,显式 --ignored 运行"]
    fn scan_all_global_progress_real_models() {
        let algo = crate::ai::face::FaceAlgo::SFace;
        if !crate::ai::face::Faces::models_present(algo) {
            eprintln!("SFace 模型未就位,跳过");
            return;
        }
        let _serial = test_serial();
        let store = test_store();
        let _ = store.set_setting("face_algo", "sface");
        crate::ai_tags::seed_labels(&store).unwrap();
        let dir = std::env::temp_dir().join(format!("maliang_scan_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = std::env::var_os("MALIANG_FACE_TEST_IMAGE").map(std::path::PathBuf::from);
        let ext = src
            .as_ref()
            .and_then(|s| s.extension())
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_else(|| "png".to_string());
        let mut paths = Vec::new();
        for i in 0..3 {
            let p = dir.join(format!("t{i}.{ext}"));
            match &src {
                Some(s) => {
                    std::fs::copy(s, &p).unwrap();
                }
                None => {
                    image::RgbImage::from_fn(64, 64, |x, y| {
                        image::Rgb([(x * 4) as u8, (y * 4) as u8, 128])
                    })
                    .save(&p)
                    .unwrap();
                }
            }
            paths.push(p);
        }
        let conn = store.test_conn();
        for (i, p) in paths.iter().enumerate() {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,kind) VALUES(?1,?2,'.',0,1,0)",
                params![(i + 1) as i64, p.to_string_lossy()],
            )
            .unwrap();
        }
        drop(conn);
        let total0 = remaining_count(&store);
        assert_eq!(total0, paths.len());
        // 开扫即取消:不动队列,不加载模型。
        let cancel = AtomicBool::new(true);
        let (p0, f0) = scan_all(&store, 2, &cancel, &|_, _| {}).unwrap();
        assert_eq!((p0, f0), (0, 0));
        assert_eq!(remaining_count(&store), total0, "取消不应消耗待办");
        // cap=2 < 3 张:强制跨批,验证 done 是全局累计而非每批重置。
        let cancel = AtomicBool::new(false);
        let seen = Mutex::new(Vec::<(usize, usize)>::new());
        let (p, f) = scan_all(&store, 2, &cancel, &|d, t| {
            seen.lock().unwrap().push((d, t));
        })
        .unwrap();
        let log = seen.lock().unwrap();
        assert!(!log.is_empty(), "应有进度回调");
        assert!(
            log.iter().all(|(_, t)| *t == total0),
            "total 应恒为开扫口径 {total0}: {log:?}"
        );
        let mut last = 0;
        for (d, _) in log.iter() {
            assert!(*d >= last, "done 应单调不减: {log:?}");
            last = *d;
        }
        assert_eq!(last, total0, "扫完 done 应等于 total: {log:?}");
        assert_eq!(remaining_count(&store), 0, "扫完待办应归零");
        assert_eq!(p, paths.len(), "每张都应提交(含 0 脸)");
        assert_eq!(unassigned_count(&store), 0, "扫完自动补跑的重聚类应收齐全部脸");
        if f > 0 {
            let ver = crate::ai_queue::stage_version(&store, crate::ai_queue::Stage::Tags);
            assert!(
                !crate::ai_tags::category_ids(&store, "人物", &ver, 100).is_empty(),
                "检出人脸的照片应自动带上人物标签"
            );
        }
        if src.is_some() {
            assert!(f > 0, "真实人脸图应检出人脸");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// map_unorient 是预览侧 map_orient 的逆:正着转过去再映射回来要还原。
    #[test]
    fn map_unorient_roundtrips() {
        // 摆正方向表(与 app.rs::map_orient 一致,仅测试内复写)。
        let fwd = |x: f32, y: f32, w: f32, h: f32, o: u32| -> (f32, f32) {
            match o {
                2 => (w - 1.0 - x, y),
                3 => (w - 1.0 - x, h - 1.0 - y),
                4 => (x, h - 1.0 - y),
                5 => (y, x),
                6 => (h - 1.0 - y, x),
                7 => (h - 1.0 - y, w - 1.0 - x),
                8 => (y, w - 1.0 - x),
                _ => (x, y),
            }
        };
        let (w, h) = (400.0f32, 300.0f32);
        for o in 1..=8u32 {
            let (dw, dh) = if (5..=8).contains(&o) { (h, w) } else { (w, h) };
            for &(x, y) in &[(0.0, 0.0), (100.5, 50.25), (w - 1.0, h - 1.0)] {
                let (dx, dy) = fwd(x, y, w, h, o);
                let (rx, ry) = map_unorient(dx, dy, dw, dh, o);
                assert!(
                    (rx - x).abs() < 1e-3 && (ry - y).abs() < 1e-3,
                    "orient {o}: ({x},{y}) → ({dx},{dy}) → ({rx},{ry})"
                );
            }
        }
    }

    /// 全量重聚类:新簇按旧身份多数票继承,名字与身份不受簇号重排影响。
    #[test]
    fn recluster_all_rebuilds_and_keeps_names() {
        let _g = test_serial();
        let store = test_store();
        let conn = store.test_conn();
        for (i, p) in ["a.jpg", "b.jpg", "c.jpg"].iter().enumerate() {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.',0,1)",
                params![(i + 1) as i64, p],
            )
            .unwrap();
        }
        drop(conn);
        // 张三(已命名)+ 一张同簇新脸(此前分在无名簇 2)应重聚为一簇;
        // 正交脸自成另一簇。
        let zhang = insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        rename_cluster(&store, 1, "张三").unwrap();
        let before = people_rows(&store);
        insert_face(&store, 2, &[0.9, 0.1, 0.0], 0.9, Some(2));
        insert_face(&store, 3, &[0.0, 1.0, 0.0], 0.9, Some(3));
        let n = recluster_all(&store, 0.6).unwrap();
        assert_eq!(n, 2, "cos≈0.99 的两张聚一簇,正交一张: {n}");
        assert_eq!(person_of(&store, zhang), Some(1), "身份号不随簇号变化");
        assert_eq!(people_rows(&store), before, "重聚类不得改动人物档案");
        let rows = people_overview(&store);
        assert_eq!(rows.len(), 2);
        let named = rows
            .iter()
            .find(|r| r.person.is_some())
            .expect("名字应保留");
        assert_eq!(named.person.as_deref(), Some("张三"));
        assert_eq!(named.person_id, Some(1));
        assert_eq!(named.count, 2, "两张相似脸应同在新簇");
        assert!(rows.iter().all(|r| r.count >= 1));
    }

    /// 重聚类可任意重排簇号,但身份与姓名始终不变。
    #[test]
    fn recluster_keeps_person_id_regardless_of_cluster() {
        let _g = test_serial();
        let store = test_store();
        let conn = store.test_conn();
        for (i, p) in ["a.jpg", "b.jpg", "c.jpg", "d.jpg"].iter().enumerate() {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.',0,1)",
                params![(i + 1) as i64, p],
            )
            .unwrap();
        }
        drop(conn);
        // 张三占旧簇 2、质量 0.5,重聚类时排到最后,新簇号会变成 3。
        let f1 = insert_face(&store, 1, &[1.0, 0.0, 0.0], 0.5, Some(2));
        let _f2 = insert_face(&store, 2, &[1.0, 0.0, 0.0], 0.5, Some(2));
        rename_cluster(&store, 2, "张三").unwrap();
        let before = people_rows(&store);
        // 两张正交脸质量更高,先占新簇 1、2。
        insert_face(&store, 3, &[0.0, 1.0, 0.0], 1.0, Some(1));
        insert_face(&store, 4, &[0.0, 0.0, 1.0], 1.0, Some(3));
        recluster_all(&store, 0.6).unwrap();
        assert_eq!(person_of(&store, f1), Some(2), "身份号不随簇号变化");
        assert_eq!(people_rows(&store), before, "人物档案原样保留");
        let rows = people_overview(&store);
        let named = rows
            .iter()
            .find(|r| r.person.is_some())
            .expect("名字应保留");
        assert_eq!(named.person.as_deref(), Some("张三"));
        assert_eq!(named.person_id, Some(2), "名字挂在稳定身份而非簇号上");
        assert_eq!(named.count, 2);
    }

    /// 簇号可以任意重排甚至复用,姓名始终绑定 person_id。
    #[test]
    fn recluster_preserves_person_name_and_id() {
        let _g = test_serial();
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg"]);
        let f1 = insert_face_with(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(3), Some(11));
        let f2 = insert_face_with(&store, 2, &[0.0, 1.0, 0.0], 1.0, Some(4), Some(22));
        rename_person(&store, 11, "赵六").unwrap();
        rename_person(&store, 22, "钱七").unwrap();
        let before = people_rows(&store);
        recluster_all(&store, 0.6).unwrap();
        assert_eq!(people_rows(&store), before, "重聚类不动人物档案");
        assert_eq!(person_of(&store, f1), Some(11));
        assert_eq!(person_of(&store, f2), Some(22));
        // 手工把所有簇号改成同一个,姓名仍随身份走。
        store
            .test_conn()
            .execute("UPDATE faces SET cluster=100", [])
            .unwrap();
        let mut names: Vec<String> = photo_faces(&store, 1)
            .into_iter()
            .filter_map(|f| f.person)
            .collect();
        names.extend(photo_faces(&store, 2).into_iter().filter_map(|f| f.person));
        assert_eq!(names, vec!["赵六".to_string(), "钱七".to_string()]);
    }

    /// 同一身份被拆成两个新簇时,身份只跟票数多的一组。
    #[test]
    fn recluster_split_identity_follows_majority() {
        let _g = test_serial();
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg", "c.jpg", "d.jpg"]);
        let x1 = insert_face_with(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(7), Some(7));
        let x2 = insert_face_with(&store, 2, &[1.0, 0.0, 0.0], 1.0, Some(7), Some(7));
        let x3 = insert_face_with(&store, 3, &[1.0, 0.0, 0.0], 1.0, Some(7), Some(7));
        let y = insert_face_with(&store, 4, &[0.0, 1.0, 0.0], 1.0, Some(7), Some(7));
        rename_person(&store, 7, "王五").unwrap();
        let before = people_rows(&store);
        recluster_all(&store, 0.6).unwrap();
        assert_eq!(people_rows(&store), before, "名字不因分裂丢失");
        let xs = [x1, x2, x3]
            .iter()
            .map(|f| person_of(&store, *f))
            .collect::<Vec<_>>();
        assert!(xs.iter().all(|p| *p == Some(7)), "多数派保留身份: {xs:?}");
        assert_eq!(person_of(&store, y), None, "少数派回到未归属");
        assert_eq!(photo_faces(&store, 1)[0].person.as_deref(), Some("王五"));
    }

    /// 同名冲突被拒;给还没有身份的簇命名时新建身份档案。
    #[test]
    fn rename_person_rejects_duplicate_and_names_unowned_cluster() {
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg", "c.jpg"]);
        insert_face(&store, 1, &[1.0, 0.0], 0.9, Some(1));
        insert_face(&store, 2, &[0.0, 1.0], 0.9, Some(2));
        let f3 = insert_face_with(&store, 3, &[0.7, 0.7], 0.9, Some(3), None);
        rename_cluster(&store, 1, "张三").unwrap();
        assert!(rename_person(&store, 2, "张三").is_err(), "同名应被拒绝");
        assert!(rename_person(&store, 1, "  ").is_err(), "空名应被拒绝");
        rename_cluster(&store, 3, "王五").unwrap();
        let pid = person_of(&store, f3).expect("无身份簇应分配新身份");
        assert!(pid > 3, "新身份号不撞已有簇号");
        assert_eq!(photo_faces(&store, 3)[0].person.as_deref(), Some("王五"));
        assert_eq!(person_of(&store, 1), Some(1), "别的簇不受影响");
        assert!(rename_cluster(&store, 3, "张三").is_err(), "已有身份后仍拒绝同名");
    }

    /// never 约束:增量归簇、质心合并、全量重聚类三处都不得让两人同簇。
    #[test]
    fn never_constraint_prevents_merging() {
        let _g = test_serial();
        let store = test_store();
        let conn = store.test_conn();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(2,'b.jpg','.',0,1)",
            [],
        )
        .unwrap();
        drop(conn);
        let f1 = insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, None);
        let f2 = insert_face(&store, 2, &[1.0, 0.0, 0.0], 1.0, None);
        crate::ai_review::add(&store, f1, f2, crate::ai_review::Kind::Never).unwrap();
        assign_clusters(&store, 0.6).unwrap();
        assert_eq!(people_overview(&store).len(), 2, "增量归簇不得同簇");
        assert_eq!(
            merge_similar_clusters(&store, 0.6).unwrap(),
            0,
            "质心合并不得越权"
        );
        let n = recluster_all(&store, 0.6).unwrap();
        assert_eq!(n, 2, "全量重聚类不得同簇");
    }

    /// must 约束:增量归簇时即使余弦很低也直接并入同伴簇。
    #[test]
    fn must_constraint_forces_join_in_incremental() {
        let store = test_store();
        let conn = store.test_conn();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(2,'b.jpg','.',0,1)",
            [],
        )
        .unwrap();
        drop(conn);
        let f1 = insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, None);
        let f2 = insert_face(&store, 2, &[0.0, 1.0, 0.0], 1.0, None);
        crate::ai_review::add(&store, f1, f2, crate::ai_review::Kind::Must).unwrap();
        assign_clusters(&store, 0.6).unwrap();
        let rows = people_overview(&store);
        assert_eq!(rows.len(), 1, "must 应强制同簇");
        assert_eq!(rows[0].count, 2);
    }

    /// 忽略簇后不出现在列表/网格,恢复后回来。
    #[test]
    fn ignore_hides_from_overview() {
        let store = test_store();
        let conn = store.test_conn();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(2,'b.jpg','.',0,1)",
            [],
        )
        .unwrap();
        drop(conn);
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 0.9, None);
        insert_face(&store, 2, &[0.98, 0.2, 0.0], 0.9, None);
        assign_clusters(&store, 0.5).unwrap();
        let rows = people_overview(&store);
        assert_eq!(rows.len(), 1);
        let cid = rows[0].cluster;
        ignore_cluster(&store, cid, true).unwrap();
        assert!(people_overview(&store).is_empty(), "忽略后不应出现在列表");
        assert!(
            faces_for_cluster(&store, cid, false, 0.0).is_empty(),
            "默认不显示已忽略"
        );
        assert_eq!(
            faces_for_cluster(&store, cid, true, 0.0).len(),
            2,
            "显式包含时可看"
        );
        ignore_cluster(&store, cid, false).unwrap();
        assert_eq!(people_overview(&store).len(), 1, "恢复后回来");
    }

    /// 低质量脸以低权重参与质心,不把簇心拉偏;成熟簇按常规阈值吸脸。
    #[test]
    fn weighted_centroid_resists_low_quality_outlier() {
        let store = test_store();
        let conn = store.test_conn();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(2,'b.jpg','.',0,1)",
            [],
        )
        .unwrap();
        drop(conn);
        // 簇 1(成熟,3 张):两张高质量 x + 一张 0.05 权重的 90° 异类脸
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        insert_face(&store, 1, &[0.0, 1.0, 0.0], 0.05, Some(1));
        // 未分配的新脸与 x 相同
        insert_face(&store, 2, &[1.0, 0.0, 0.0], 1.0, None);
        // thr=0.9:无权重的均值质心 cos≈0.894 会另起新簇;加权后 cos≈0.999 归簇 1
        assign_clusters(&store, 0.9).unwrap();
        let rows = people_overview(&store);
        assert_eq!(
            rows.len(),
            1,
            "低质量异类不应撕裂簇: {:?}",
            rows.iter()
                .map(|r| (r.cluster, r.count))
                .collect::<Vec<_>>()
        );
        assert_eq!(rows[0].count, 4);
    }

    /// 质心缓存必须跟着成员走:加脸与并簇之后都等于按增量和重算,不能读到旧值。
    #[test]
    fn centroid_cache_tracks_membership() {
        let mut cm = ClusterMem::new(2);
        assert!(cm.centroid().is_none(), "空簇没有质心");
        cm.add(
            &Member { v: vec![1.0, 0.0], q: 0.9, photo: 1, manual: false },
            0.6,
        );
        let first = cm.centroid().unwrap().to_vec();
        assert!((first[0] - 1.0).abs() < 1e-6 && first[1].abs() < 1e-6);
        cm.add(
            &Member { v: vec![0.0, 1.0], q: 0.9, photo: 2, manual: false },
            0.6,
        );
        assert_eq!(
            cm.centroid().unwrap().to_vec(),
            sum_centroid(&cm.sum, cm.wsum).unwrap(),
            "加脸后缓存应等于重算"
        );
        assert_ne!(cm.centroid().unwrap().to_vec(), first, "质心应随新成员转向");
        // 并簇直接累加增量和,不走 add,同样要刷新缓存。
        let mut other = ClusterMem::new(2);
        other.add(
            &Member { v: vec![0.0, 1.0], q: 0.5, photo: 3, manual: false },
            0.6,
        );
        for (x, y) in cm.sum.iter_mut().zip(&other.sum) {
            *x += y;
        }
        cm.wsum += other.wsum;
        cm.members.append(&mut other.members);
        cm.refresh_centroid();
        let c = cm.centroid().unwrap().to_vec();
        assert_eq!(c, sum_centroid(&cm.sum, cm.wsum).unwrap());
        assert!(c[1] > c[0], "并入偏 y 的簇后质心应偏 y: {c:?}");
    }

    /// 年轻簇(成员 < 3)吸脸收紧:余弦 0.66 过常规阈值、过不了年轻簇的,
    /// 不能被单脸簇吸走,只能另立门户。
    #[test]
    fn young_cluster_needs_stronger_evidence() {
        let store = test_store();
        let conn = store.test_conn();
        for (i, p) in ["a.jpg", "b.jpg"].iter().enumerate() {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.',0,1)",
                params![(i + 1) as i64, p],
            )
            .unwrap();
        }
        drop(conn);
        // 单脸簇 1:[1,0,0];新脸与它余弦 ≈0.66:过 0.6,不过 0.7。
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        insert_face(&store, 2, &[0.66, 0.751, 0.0], 1.0, None);
        assign_clusters(&store, 0.6).unwrap();
        let rows = people_overview(&store);
        assert_eq!(
            rows.len(),
            2,
            "0.66 进不了年轻簇,应另开新簇: {:?}",
            rows.iter()
                .map(|r| (r.cluster, r.count))
                .collect::<Vec<_>>()
        );
    }

    /// 成熟簇(成员 ≥ 3)按常规阈值吸脸:同样的 0.66 余弦能进。
    #[test]
    fn mature_cluster_joins_at_base_threshold() {
        let store = test_store();
        let conn = store.test_conn();
        for (i, p) in ["a.jpg", "b.jpg"].iter().enumerate() {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.',0,1)",
                params![(i + 1) as i64, p],
            )
            .unwrap();
        }
        drop(conn);
        for _ in 0..3 {
            insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        }
        insert_face(&store, 2, &[0.66, 0.751, 0.0], 1.0, None);
        assign_clusters(&store, 0.6).unwrap();
        assert_eq!(people_overview(&store).len(), 1, "成熟簇按常规阈值吸收");
        assert_eq!(people_overview(&store)[0].count, 4);
    }

    /// 模棱两可的脸不当场归簇:与两个成熟簇几乎同分(余量不足),
    /// 留在未分配,等下批或重聚类再判。
    #[test]
    fn ambiguous_face_stays_unassigned() {
        let store = test_store();
        let conn = store.test_conn();
        for (i, p) in ["a.jpg", "b.jpg", "c.jpg"].iter().enumerate() {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.',0,1)",
                params![(i + 1) as i64, p],
            )
            .unwrap();
        }
        drop(conn);
        // 两个正交的成熟簇;新脸与两者各成 45°(余弦都是 0.707)。
        for _ in 0..3 {
            insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
            insert_face(&store, 2, &[0.0, 1.0, 0.0], 1.0, Some(2));
        }
        insert_face(&store, 3, &[0.707, 0.707, 0.0], 1.0, None);
        assign_clusters(&store, 0.6).unwrap();
        assert_eq!(people_overview(&store).len(), 2, "不应冒出第三簇");
        let unassigned: usize = store
            .read(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM faces WHERE cluster IS NULL AND ignored=0",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .map(|n| n as usize)
                .map_err(anyhow::Error::from)
            })
            .unwrap();
        assert_eq!(unassigned, 1, "模棱两可的脸应留在未分配");
    }

    /// 质心级合并:同人被裂成两簇,质心相近就并回去。
    #[test]
    fn merge_repairs_split_clusters() {
        let store = test_store();
        let conn = store.test_conn();
        for (i, p) in ["a.jpg", "b.jpg"].iter().enumerate() {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.',0,1)",
                params![(i + 1) as i64, p],
            )
            .unwrap();
        }
        drop(conn);
        // 簇 1(3 张 [1,0,0])与簇 2(2 张 [0.93,0.36,0],余弦 ≈0.93),都未命名。
        for _ in 0..3 {
            insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        }
        for _ in 0..2 {
            insert_face(&store, 2, &[0.93, 0.36, 0.0], 1.0, Some(2));
        }
        let before = people_rows(&store);
        let merged = merge_similar_clusters(&store, 0.6).unwrap();
        assert_eq!(merged, 1, "应并掉一簇");
        assert_eq!(people_rows(&store), before, "并簇不得增删人物档案");
        let rows = people_overview(&store);
        assert_eq!(rows.len(), 2, "并簇只改簇号,两个身份仍各自成行");
        assert_eq!(rows.iter().map(|r| r.count).sum::<usize>(), 5);
        assert!(rows.iter().all(|r| r.clusters == vec![1]));
        assert_eq!(
            faces_for_cluster(&store, 1, false, 0.0).len(),
            5,
            "簇内成员已合并"
        );
    }

    /// 已命名的簇互不合并 —— 命名即用户意图,哪怕质心再像也不越权。
    #[test]
    fn merge_never_joins_two_named_clusters() {
        let store = test_store();
        let conn = store.test_conn();
        for (i, p) in ["a.jpg", "b.jpg"].iter().enumerate() {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.',0,1)",
                params![(i + 1) as i64, p],
            )
            .unwrap();
        }
        drop(conn);
        for _ in 0..3 {
            insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        }
        for _ in 0..2 {
            insert_face(&store, 2, &[0.93, 0.36, 0.0], 1.0, Some(2));
        }
        rename_cluster(&store, 1, "张三").unwrap();
        rename_cluster(&store, 2, "李四").unwrap();
        let before = people_rows(&store);
        let merged = merge_similar_clusters(&store, 0.6).unwrap();
        assert_eq!(merged, 0, "双方都有名字不应并");
        assert_eq!(people_rows(&store), before, "两个已命名身份都保留");
        let rows = people_overview(&store);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.person.as_deref() == Some("张三")));
        assert!(rows.iter().any(|r| r.person.as_deref() == Some("李四")));
    }

    /// 只有一方命名时:名字跟到保留方(有名字的簇为保留方)。
    #[test]
    fn merge_keeps_name_of_named_side() {
        let store = test_store();
        let conn = store.test_conn();
        for (i, p) in ["a.jpg", "b.jpg"].iter().enumerate() {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.',0,1)",
                params![(i + 1) as i64, p],
            )
            .unwrap();
        }
        drop(conn);
        for _ in 0..3 {
            insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        }
        for _ in 0..2 {
            insert_face(&store, 2, &[0.93, 0.36, 0.0], 1.0, Some(2));
        }
        rename_cluster(&store, 2, "李四").unwrap();
        let before = people_rows(&store);
        let merged = merge_similar_clusters(&store, 0.6).unwrap();
        assert_eq!(merged, 1);
        assert_eq!(people_rows(&store), before, "并簇只改簇号");
        let rows = people_overview(&store);
        assert_eq!(rows.len(), 2, "并簇不改身份,两行各留各的身份");
        let named = rows
            .iter()
            .find(|r| r.person.is_some())
            .expect("名字应保留");
        assert_eq!(named.person.as_deref(), Some("李四"));
        assert_eq!(named.person_id, Some(2));
        assert_eq!(named.count, 2);
        assert_eq!(
            faces_for_cluster(&store, 2, false, 0.0).len(),
            5,
            "两个身份的脸已在同一簇"
        );
    }

    /// 右栏人物面板的数据口:一 photo 多脸,带簇号、姓名与忽略态。
    #[test]
    fn photo_faces_carries_cluster_name_and_rect() {
        let store = test_store();
        store
            .test_conn()
            .execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(7,'g.jpg','.',0,1)",
                [],
            )
            .unwrap();
        // 有名字的簇、没名字的簇、还没聚类的脸
        insert_face(&store, 7, &[1.0, 0.0, 0.0], 0.9, Some(1));
        insert_face(&store, 7, &[1.0, 0.0, 0.0], 0.7, Some(2));
        insert_face(&store, 7, &[0.0, 1.0, 0.0], 0.5, None);
        rename_cluster(&store, 1, "小明").unwrap();
        ignore_cluster(&store, 2, true).unwrap();

        let faces = photo_faces(&store, 7);
        assert_eq!(faces.len(), 3, "未归属的脸也要返回");
        assert_eq!(faces[0].person.as_deref(), Some("小明"));
        assert_eq!(faces[0].cluster, Some(1));
        assert_eq!(faces[0].person_id, Some(1));
        assert!(!faces[0].ignored);
        assert_eq!(faces[0].rect, (10.0, 10.0, 50.0, 50.0));
        assert!(faces[1].ignored, "被忽略的簇要带着标记返回");
        assert_eq!(faces[1].person, None, "没命名的身份不该被写成空串");
        assert_eq!(faces[1].person_id, Some(2), "未命名身份也有稳定编号");
        assert_eq!(faces[2].cluster, None);
        assert_eq!(faces[2].person_id, None, "未归属 = person_id IS NULL");
        assert!(photo_faces(&store, 999).is_empty(), "别的照片不该混进来");
    }

    /// 同图两张脸默认不自动并;人工 must 明确要求时例外。
    #[test]
    fn same_photo_faces_do_not_auto_merge() {
        let store = test_store();
        let conn = store.test_conn();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)",
            [],
        )
        .unwrap();
        drop(conn);
        let f1 = insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, None);
        let f2 = insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, None);
        assign_clusters(&store, 0.6).unwrap();
        assert_eq!(people_overview(&store).len(), 2, "同图不同脸不得自动同簇");
        crate::ai_review::add(&store, f1, f2, crate::ai_review::Kind::Must).unwrap();
        store
            .write(|conn| -> Result<()> {
                conn.execute("UPDATE faces SET cluster=NULL", [])?;
                Ok(())
            })
            .unwrap();
        assign_clusters(&store, 0.6).unwrap();
        assert_eq!(people_overview(&store).len(), 1, "人工 must 应越过同图规则");
    }

    /// 质心合并同样不并同图两簇。
    #[test]
    fn merge_never_joins_clusters_sharing_a_photo() {
        let store = test_store();
        let conn = store.test_conn();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)",
            [],
        )
        .unwrap();
        drop(conn);
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(2));
        assert_eq!(merge_similar_clusters(&store, 0.6).unwrap(), 0);
        assert_eq!(people_overview(&store).len(), 2);
    }

    /// never 比 must 更硬:即使 must 要求合并,存在 never 就不并。
    #[test]
    fn never_beats_must_in_merge() {
        let store = test_store();
        let conn = store.test_conn();
        for (i, p) in ["a.jpg", "b.jpg", "c.jpg", "d.jpg"].iter().enumerate() {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.',0,1)",
                params![(i + 1) as i64, p],
            )
            .unwrap();
        }
        drop(conn);
        let f1 = insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        insert_face(&store, 2, &[1.0, 0.0, 0.0], 1.0, Some(1));
        let f3 = insert_face(&store, 3, &[1.0, 0.0, 0.0], 1.0, Some(2));
        let f4 = insert_face(&store, 4, &[1.0, 0.0, 0.0], 1.0, Some(2));
        crate::ai_review::add(&store, f1, f3, crate::ai_review::Kind::Must).unwrap();
        crate::ai_review::add(&store, f1, f4, crate::ai_review::Kind::Never).unwrap();
        assert_eq!(merge_similar_clusters(&store, 0.6).unwrap(), 0);
        assert_eq!(people_overview(&store).len(), 2);
    }

    /// 重命名不能覆盖同名档案,也不能用空名。
    #[test]
    fn rename_rejects_duplicate_and_empty_names() {
        let store = test_store();
        let conn = store.test_conn();
        for (i, p) in ["a.jpg", "b.jpg"].iter().enumerate() {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.',0,1)",
                params![(i + 1) as i64, p],
            )
            .unwrap();
        }
        drop(conn);
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        insert_face(&store, 2, &[0.0, 1.0, 0.0], 1.0, Some(2));
        rename_cluster(&store, 1, "张三").unwrap();
        assert_eq!(people_rows(&store), vec![(1, "张三".to_string())]);
        assert!(rename_cluster(&store, 2, "张三").is_err(), "同名应被拒绝");
        assert!(rename_person(&store, 2, "张三").is_err(), "按身份命名同样拒绝同名");
        assert!(rename_cluster(&store, 2, "   ").is_err(), "空名应被拒绝");
        rename_cluster(&store, 2, "李四").unwrap();
        let names: Vec<String> = store
            .read(|conn| -> Result<Vec<String>> {
                let mut st = conn.prepare("SELECT name FROM people ORDER BY id")?;
                let rows = st.query_map([], |r| r.get::<_, String>(0))?;
                Ok(rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap();
        assert_eq!(names, vec!["张三".to_string(), "李四".to_string()]);
        assert_eq!(person_of(&store, 1), Some(1));
        assert_eq!(person_of(&store, 2), Some(2), "两个簇映射到不同身份");
    }

    /// 命名混合簇:已有身份的脸沿用该身份,尚未归属的脸一并扫入,不再只命一部分。
    #[test]
    fn rename_cluster_sweeps_unassigned_faces_into_identity() {
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg", "c.jpg"]);
        let named_a = insert_face_with(&store, 1, &[1.0, 0.0], 1.0, Some(1), Some(11));
        let named_b = insert_face_with(&store, 2, &[1.0, 0.0], 1.0, Some(1), Some(11));
        let unowned = insert_face_with(&store, 3, &[1.0, 0.0], 1.0, Some(1), None);
        rename_person(&store, 11, "张三").unwrap();
        rename_cluster(&store, 1, "张三").unwrap();
        assert_eq!(person_of(&store, named_a), Some(11));
        assert_eq!(person_of(&store, named_b), Some(11));
        assert_eq!(
            person_of(&store, unowned),
            Some(11),
            "未归属的脸应被扫进同一身份"
        );
        assert_eq!(people_rows(&store), vec![(11, "张三".to_string())]);
        assert_eq!(faces_for_person(&store, 11, false, 0.0).len(), 3);
    }

    /// 命名整簇受「不同人」约束保护:簇内存在 never 时拒绝,身份不变。
    #[test]
    fn rename_cluster_respects_never_constraint() {
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg"]);
        let a = insert_face_with(&store, 1, &[1.0, 0.0], 1.0, Some(1), Some(11));
        let b = insert_face_with(&store, 2, &[1.0, 0.0], 1.0, Some(1), None);
        crate::ai_review::add(&store, a, b, crate::ai_review::Kind::Never).unwrap();
        assert!(rename_cluster(&store, 1, "张三").is_err());
        assert_eq!(person_of(&store, b), None, "冲突时未归属的脸不被扫入");
        assert!(people_rows(&store).is_empty());
    }

    /// 清除人脸数据在事务内连约束与扫描状态一起清掉。
    #[test]
    fn wipe_clears_constraints_and_scan_state() {
        let _g = test_serial();
        let store = test_store();
        let conn = store.test_conn();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(2,'b.jpg','.',0,1)",
            [],
        )
        .unwrap();
        drop(conn);
        let f1 = insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        let f2 = insert_face(&store, 2, &[1.0, 0.0, 0.0], 1.0, Some(1));
        crate::ai_review::add(&store, f1, f2, crate::ai_review::Kind::Never).unwrap();
        let sig = algo_tag(&store);
        store
            .write(|conn| -> Result<()> {
                conn.execute(
                    "INSERT INTO face_scan(photo_id,sig,state) VALUES(1,?1,2)",
                    params![sig],
                )?;
                Ok(())
            })
            .unwrap();
        wipe_faces(&store).unwrap();
        let counts: (i64, i64, i64, i64, i64, i64) = store
            .read(|conn| -> Result<(i64, i64, i64, i64, i64, i64)> {
                Ok((
                    conn.query_row("SELECT COUNT(*) FROM faces", [], |r| r.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM face_templates", [], |r| r.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM people", [], |r| r.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM face_constraints", [], |r| r.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM face_scan", [], |r| r.get(0))?,
                    conn.query_row("SELECT face_done FROM photos WHERE id=1", [], |r| r.get(0))?,
                ))
            })
            .unwrap();
        assert_eq!(counts, (0, 0, 0, 0, 0, 0));
    }

    /// 每张照片重跑时替换旧脸,不留半成品;修订变化则放弃写入。
    #[test]
    fn commit_photo_replaces_faces_atomically() {
        let store = test_store();
        store
            .test_conn()
            .execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)",
                [],
            )
            .unwrap();
        let sig = algo_tag(&store);
        let rec = |x: f32| FaceRec {
            x1: x,
            y1: 0.0,
            x2: x + 1.0,
            y2: 1.0,
            score: 0.9,
            quality: 0.8,
            emb: vec![1.0, 0.0, 0.0],
        };
        assert!(commit_photo(&store, 1, &sig, 0, 1, &[rec(0.0), rec(0.1)]).unwrap());
        assert!(commit_photo(&store, 1, &sig, 0, 1, &[rec(0.2)]).unwrap());
        let (faces, tpls, state, cnt): (i64, i64, i64, i64) = store
            .read(|conn| -> Result<(i64, i64, i64, i64)> {
                Ok((
                    conn.query_row("SELECT COUNT(*) FROM faces", [], |r| r.get(0))?,
                    conn.query_row("SELECT COUNT(*) FROM face_templates", [], |r| r.get(0))?,
                    conn.query_row("SELECT state FROM face_scan WHERE photo_id=1", [], |r| r.get(0))?,
                    conn.query_row("SELECT face_count FROM face_scan WHERE photo_id=1", [], |r| r.get(0))?,
                ))
            })
            .unwrap();
        assert_eq!((faces, tpls, state, cnt), (1, 1, SCAN_DONE, 1), "重跑不得重复插入");
        let stale: i64 = store
            .test_conn()
            .query_row(
                "SELECT COUNT(*) FROM face_templates WHERE sig<>?1",
                params![sig],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0, "提交的人脸必须带当前签名");
        // 源文件修订变化:不写回旧结果。
        assert!(!commit_photo(&store, 1, &sig, 9, 1, &[rec(0.3)]).unwrap());
        let faces: i64 = store
            .read(|conn| {
                conn.query_row("SELECT COUNT(*) FROM faces", [], |r| r.get(0))
                    .map_err(anyhow::Error::from)
            })
            .unwrap();
        assert_eq!(faces, 1);
        // 签名变化:同样不写回。
        assert!(!commit_photo(&store, 1, "old", 0, 1, &[rec(0.4)]).unwrap());
    }

    /// 种子脸也按质量加权,首张低质量脸不会把质心带偏。
    #[test]
    fn group_accumulation_weights_first_face() {
        let mut g = ReclusterGroup {
            sum: vec![0.0, 0.0],
            wsum: 0.0,
            reps: Vec::new(),
            photos: Vec::new(),
            faces: Vec::new(),
        };
        let hi = Member {
            v: vec![1.0, 0.0],
            q: 1.0,
            photo: 1,
            manual: false,
        };
        let lo = Member {
            v: vec![0.0, 1.0],
            q: 0.05,
            photo: 2,
            manual: false,
        };
        group_add(&mut g, &hi, 1, 0.6);
        group_add(&mut g, &lo, 2, 0.6);
        let c = sum_centroid(&g.sum, g.wsum).unwrap();
        assert!(c[0] > 0.99, "质心应由高质量脸主导: {c:?}");
    }

    fn mem(v: &[f32], q: f32, photo: i64, manual: bool) -> Member {
        Member {
            v: v.to_vec(),
            q,
            photo,
            manual,
        }
    }

    /// 代表模板:近重复去重 + 低质量门控。
    #[test]
    fn rep_select_dedups_and_gates_quality() {
        let ms = vec![
            mem(&[1.0, 0.0], 0.9, 1, false),
            mem(&[1.0, 0.0], 0.8, 2, false), // 与首张近重复(cos=1.0)→ 剔除
            mem(&[0.0, 1.0], 0.1, 3, false), // 质量低于 REP_MIN_Q → 跳过
            mem(&[0.6, 0.8], 0.7, 4, false), // 足够互异 → 入选
        ];
        let reps = rep_select(&ms, 0.5);
        let photos: Vec<i64> = reps.iter().map(|r| r.photo).collect();
        assert_eq!(reps.len(), 2, "近重复与低质量都不进代表集: {photos:?}");
        assert!(photos.contains(&1) && photos.contains(&4));
        assert!(!photos.contains(&2) && !photos.contains(&3));
    }

    /// 代表模板:人工锚点优先,未被人工锚点支持的自动脸不入围(防自我强化)。
    #[test]
    fn rep_select_manual_first_blocks_unsupported_auto() {
        let ms = vec![
            mem(&[1.0, 0.0], 0.9, 1, false), // 高质量自动脸,但与人工锚点正交
            mem(&[0.0, 1.0], 0.3, 2, true),  // 低质量人工锚点(人工不受质量门槛限制)
        ];
        let reps = rep_select(&ms, 0.5);
        assert_eq!(reps.len(), 1, "未被人工锚点支持的自动脸不得进代表集");
        assert!(reps[0].manual && reps[0].photo == 2);
    }

    /// 代表模板:同照片上限,防一张照片多脸虚增支持数。
    #[test]
    fn rep_select_caps_per_photo() {
        let ms = vec![
            mem(&[1.0, 0.0, 0.0], 0.9, 1, false),
            mem(&[0.0, 1.0, 0.0], 0.8, 1, false),
            mem(&[0.0, 0.0, 1.0], 0.7, 1, false), // 同照片第三张 → 触顶
        ];
        let reps = rep_select(&ms, 0.5);
        assert_eq!(reps.len(), REP_MAX_PER_PHOTO, "同照片最多贡献两张代表");
        assert!(reps.iter().all(|r| r.photo == 1));
    }

    /// 纯自动多模板簇:桥接脸质心过关但只命中一张代表,支持数不足 → 不自动认领。
    #[test]
    fn auto_claim_needs_two_rep_support() {
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg", "c.jpg", "d.jpg"]);
        // 成熟簇(3 成员、2 互异代表,无人工锚点 → 需 2 票支持)。
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        insert_face(&store, 2, &[0.0, 1.0, 0.0], 1.0, Some(1));
        insert_face(&store, 3, &[1.0, 0.0, 0.0], 1.0, Some(1)); // 近重复,只占成员不进代表
        // 桥接脸:偏向其中一个代表(cos≈0.95),对另一个仅 cos≈0.31。
        let bridge = insert_face(&store, 4, &[0.95, 0.31, 0.0], 1.0, None);
        assign_clusters(&store, 0.6).unwrap();
        let bc = cluster_of(&store, bridge);
        assert_ne!(bc, Some(1), "单代表支持不足以自动认领桥接脸");
        assert!(bc.is_some(), "被拒的桥接脸应另立新簇而非丢弃");
    }

    /// 簇合并:质心相近但代表仅单向支持 → 拒绝合并(防桥接脸硬拉两簇)。
    #[test]
    fn merge_rejected_without_mutual_support() {
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg", "f.jpg"]);
        // 簇 A:两个正交代表 [1,0,0] 与 [0,1,0],质心 ≈[0.894,0.447,0]。
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        insert_face(&store, 2, &[0.0, 1.0, 0.0], 1.0, Some(1));
        insert_face(&store, 3, &[1.0, 0.0, 0.0], 1.0, Some(1));
        // 簇 B:三张同向 [0.894,0.447,0](近重复 → 单代表),质心与 A 几乎重合。
        for pid in 4..7 {
            insert_face(&store, pid, &[0.894, 0.447, 0.0], 1.0, Some(2));
        }
        let merged = merge_similar_clusters(&store, 0.6).unwrap();
        assert_eq!(merged, 0, "A 的两个代表只有一个是 B 命中,双向支持不足不应合并");
        assert_eq!(people_overview(&store).len(), 2, "两簇仍各自独立");
    }

    /// 归属来源:人工建档/归入标记 manual,移出清除。
    #[test]
    fn manual_ops_mark_assign_src() {
        let _g = test_serial();
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg"]);
        let f1 = insert_face(&store, 1, &[1.0, 0.0], 1.0, None);
        let f2 = insert_face(&store, 2, &[1.0, 0.0], 1.0, None);
        assert_eq!(assign_src_of(&store, f1), None);
        let pid = create_person(&store, &[f1], Some("Alice")).unwrap();
        assert_eq!(
            assign_src_of(&store, f1),
            Some("manual".to_string()),
            "建档即人工确认"
        );
        assign_faces(&store, &[f2], pid).unwrap();
        assert_eq!(assign_src_of(&store, f2), Some("manual".to_string()));
        remove_faces(&store, &[f2]).unwrap();
        assert_eq!(assign_src_of(&store, f2), None, "移出后归属来源清空");
        assert!(person_of(&store, f2).is_none());
    }

    fn insert_photos(store: &Store, paths: &[&str]) {
        let conn = store.test_conn();
        for (i, p) in paths.iter().enumerate() {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.',0,1)",
                params![(i + 1) as i64, p],
            )
            .unwrap();
        }
    }

    #[test]
    fn similar_faces_orders_excludes_self_and_ignored() {
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg", "c.jpg"]);
        let q = insert_face(&store, 1, &[1.0, 0.0], 0.9, None);
        let near = insert_face(&store, 2, &[1.0, 0.0], 0.9, None);
        let mid = insert_face(&store, 3, &[0.8, 0.6], 0.9, None);
        let far = insert_face(&store, 3, &[0.0, 1.0], 0.9, None);
        let hits = similar_faces(&store, q, 0.5, 10, false);
        let ids: Vec<i64> = hits.iter().map(|h| h.face_id).collect();
        assert_eq!(ids, vec![near, mid], "按相似度降序且排除自身/低分");
        assert!(hits.iter().all(|h| h.person_id.is_none()), "新脸未归属");
        assert!((hits[0].score - 1.0).abs() < 1e-5);
        assert!(!hits.iter().any(|h| h.face_id == far));
        assert_eq!(similar_faces(&store, q, 0.5, 1, false).len(), 1, "limit 生效");
        set_ignored(&store, &[near], true).unwrap();
        let ids: Vec<i64> = similar_faces(&store, q, 0.5, 10, false)
            .iter()
            .map(|h| h.face_id)
            .collect();
        assert_eq!(ids, vec![mid], "忽略项默认隐藏");
        assert_eq!(similar_faces(&store, q, 0.5, 10, true).len(), 2);
    }

    #[test]
    fn manual_assignment_respects_never_and_can_undo() {
        let _g = test_serial();
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg"]);
        let a = insert_face(&store, 1, &[1.0, 0.0], 0.9, Some(1));
        let b = insert_face(&store, 2, &[0.0, 1.0], 0.9, Some(2));
        crate::ai_review::add(&store, a, b, crate::ai_review::Kind::Never).unwrap();
        assert!(assign_faces(&store, &[a], 2).is_err(), "never 冲突必须拒绝");
        assert_eq!(faces_for_cluster(&store, 1, true, 0.0).len(), 1);
        crate::ai_review::remove(&store, a, b).unwrap();
        assert_eq!(assign_faces(&store, &[a], 2).unwrap(), 1);
        assert_eq!(person_of(&store, a), Some(2), "加入此人物按身份落库");
        assert!(faces_for_cluster(&store, 1, true, 0.0).is_empty());
        assert_eq!(remove_faces(&store, &[a]).unwrap(), 1);
        assert_eq!(person_of(&store, a), None, "移出人物后回到未归属");
        let left: Vec<i64> = faces_for_cluster(&store, 2, true, 0.0)
            .iter()
            .map(|f| f.face_id)
            .collect();
        assert_eq!(left, vec![b]);
    }

    #[test]
    fn create_person_allocates_stable_id_and_rejects_duplicate_name() {
        let _g = test_serial();
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg"]);
        let a = insert_face(&store, 1, &[1.0, 0.0], 0.9, Some(5));
        rename_cluster(&store, 5, "张三").unwrap();
        let b = insert_face(&store, 2, &[0.0, 1.0], 0.9, None);
        let pid = create_person(&store, &[b], Some("李四")).unwrap();
        assert!(pid > 5, "新身份号不得撞已有簇/档案");
        assert_eq!(person_of(&store, b), Some(pid), "新建人物写入稳定身份");
        assert_eq!(
            people_rows(&store),
            vec![(5, "张三".to_string()), (pid, "李四".to_string())]
        );
        let names: Vec<String> = photo_faces(&store, 2)
            .into_iter()
            .filter_map(|f| f.person)
            .collect();
        assert_eq!(names, vec!["李四".to_string()]);
        assert!(create_person(&store, &[b], Some("张三")).is_err());
        assert!(create_person(&store, &[], None).is_err());
        let _ = a;
    }

    /// 管线签名含方案契约、维度、权重指纹与各处理版本,同方案稳定。
    #[test]
    fn pipeline_sig_covers_contract_and_versions() {
        let a = sig_from_key("sface");
        let b = sig_from_key("adaface");
        assert_ne!(a, b, "不同方案的向量不可互相冒充");
        assert_eq!(a, sig_from_key("sface"), "同方案签名必须稳定");
        assert!(a.contains("dim:128") && b.contains("dim:512"));
        for part in [
            FACE_ALGO_VERSION,
            ALIGN_VERSION,
            INPUT_VERSION,
            DETECT_VERSION,
            FaceAlgo::SFace.contract(),
            "det:",
            "rec:",
        ] {
            assert!(a.contains(part), "签名缺 {part}");
        }
    }

    /// 写入的模板逐条带签名,缺签名的旧行视为未知版本。
    #[test]
    fn templates_carry_sig_and_unknown_is_quarantined() {
        let store = test_store();
        insert_photos(&store, &["a.jpg"]);
        let fid = save_face_t(&store, 1, &[1.0, 0.0], 0.9).unwrap();
        let sig: String = store
            .test_conn()
            .query_row(
                "SELECT sig FROM face_templates WHERE face_id=?1",
                params![fid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sig, algo_tag(&store));
        assert!(!pending_rebuild(&store));
        store
            .test_conn()
            .execute(
                "UPDATE face_templates SET sig='' WHERE face_id=?1",
                params![fid],
            )
            .unwrap();
        assert!(pending_rebuild(&store), "缺签名的旧库不能补写成当前版本");
    }

    /// 待重建时旧向量不参与检索/聚类/建议,也不会自动重扫覆盖人工数据。
    #[test]
    fn stale_templates_block_queries_and_rescan() {
        let _g = test_serial();
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg"]);
        let a = insert_face(&store, 1, &[1.0, 0.0], 0.9, Some(1));
        insert_face(&store, 2, &[1.0, 0.0], 0.9, None);
        assert_eq!(similar_faces(&store, a, 0.0, 10, false).len(), 1);
        store
            .test_conn()
            .execute(
                "UPDATE face_templates SET sig='unknown' WHERE face_id=?1",
                params![a],
            )
            .unwrap();
        assert!(pending_rebuild(&store));
        assert!(effective_sig(&store).is_empty());
        assert!(photos_todo(&store, 10).is_empty(), "待重建时不得自动重扫");
        assert_eq!(remaining_count(&store), 0);
        assert!(similar_faces(&store, a, 0.0, 10, false).is_empty());
        assert_eq!(assign_clusters(&store, 0.4).unwrap(), 0);
        assert!(face_suggestions(&store, 0.4).is_empty());
        assert_eq!(recluster_all(&store, 0.4).unwrap(), 0);
        assert!(scan_batch(&store, 10, &|_, _| {}).is_err());
        wipe_faces(&store).unwrap();
        assert!(!pending_rebuild(&store), "确认重建后恢复当前签名");
        assert_eq!(photos_todo(&store, 10).len(), 2);
    }

    /// 改方案后旧签名自动失效,新签名不得与旧向量混用。
    #[test]
    fn changing_algo_quarantines_old_vectors() {
        let store = test_store();
        insert_photos(&store, &["a.jpg"]);
        insert_face(&store, 1, &[1.0, 0.0], 0.9, None);
        assert!(!pending_rebuild(&store));
        let before = algo_tag(&store);
        store.set_setting("face_algo", "adaface").unwrap();
        assert_ne!(algo_tag(&store), before);
        assert!(pending_rebuild(&store));
        assert!(photos_todo(&store, 10).is_empty());
        assert!(scan_batch(&store, 10, &|_, _| {}).is_err());
    }

    /// 人脸任务互斥:守卫被占着时扫描入口必须排队,不能并发改写人脸结果。
    #[test]
    fn scan_entry_waits_for_face_task_guard() {
        let _s = test_serial();
        let store = test_store();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let g = face_task_guard().unwrap();
        let h = std::thread::spawn(move || {
            let _ = scan_batch(&store, 10, &|_, _| {});
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "守卫被占时扫描不该跑完"
        );
        drop(g);
        h.join().unwrap();
        assert!(rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok());
    }

    /// 人物标签的词表 id(按节点取,词表增删不写死 id)。
    fn people_label(store: &Store, node: &str) -> i64 {
        store
            .test_conn()
            .query_row(
                "SELECT id FROM tag_labels WHERE node=?1",
                params![node],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn tag_ver(store: &Store) -> String {
        crate::ai_queue::stage_version(store, crate::ai_queue::Stage::Tags)
    }

    /// 未归簇计数只认当前签名与维度,已归簇/已忽略的不算。
    #[test]
    fn unassigned_count_counts_only_current_pipeline() {
        let _g = test_serial();
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg"]);
        assert_eq!(unassigned_count(&store), 0);
        insert_face(&store, 1, &[1.0, 0.0], 0.9, None);
        insert_face(&store, 1, &[0.0, 1.0], 0.9, Some(3));
        let ign = insert_face(&store, 2, &[0.0, 0.0, 1.0], 0.9, None);
        set_ignored(&store, &[ign], true).unwrap();
        assert_eq!(unassigned_count(&store), 1, "已归簇与已忽略的不计入");
        store.set_setting("face_algo", "adaface").unwrap();
        assert_eq!(unassigned_count(&store), 0, "待重建期间不报未归簇");
    }

    /// 增量归簇留下的漏网脸,全量重聚类能收齐(扫完自动补跑的就是这一步)。
    #[test]
    fn recluster_collects_faces_left_unassigned() {
        let _g = test_serial();
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg", "c.jpg"]);
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 0.9, None);
        insert_face(&store, 2, &[0.99, 0.01, 0.0], 0.5, None);
        insert_face(&store, 3, &[0.0, 1.0, 0.0], 0.9, None);
        assert_eq!(unassigned_count(&store), 3);
        recluster_all(&store, 0.6).unwrap();
        assert_eq!(unassigned_count(&store), 0, "重聚类后不应再有未归簇的脸");
        assert_eq!(people_overview(&store).len(), 2);
    }

    /// 单脸忽略不牵连整组,且人物标签随人脸数改写。
    #[test]
    fn ignoring_one_face_keeps_group_and_refreshes_people_tag() {
        let _g = test_serial();
        let store = test_store();
        insert_photos(&store, &["a.jpg"]);
        crate::ai_tags::seed_labels(&store).unwrap();
        let f1 = insert_face(&store, 1, &[1.0, 0.0, 0.0], 0.9, Some(7));
        let f2 = insert_face(&store, 1, &[0.0, 1.0, 0.0], 0.9, Some(7));
        let ver = tag_ver(&store);
        crate::ai_tags::sync_people_tags(&store, &[1], &ver).unwrap();
        let tags = crate::ai_tags::photo_tags(&store, 1, &ver);
        assert_eq!(tags[0].0, people_label(&store, "人物/合影"));
        set_ignored(&store, &[f1], true).unwrap();
        let other: i64 = store
            .test_conn()
            .query_row("SELECT ignored FROM faces WHERE id=?1", params![f2], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(other, 0, "忽略单脸不得连带整组");
        let tags = crate::ai_tags::photo_tags(&store, 1, &ver);
        assert_eq!(tags[0].0, people_label(&store, "人物/人像"), "剩一张脸应改判人像");
        ignore_cluster(&store, 7, true).unwrap();
        assert!(
            crate::ai_tags::photo_tags(&store, 1, &ver).is_empty(),
            "整组忽略后人物标签应清除"
        );
    }

    /// 人物列表按人数从多到少排。
    #[test]
    fn people_overview_orders_by_count_desc() {
        let store = test_store();
        insert_photos(&store, &["a.jpg", "b.jpg", "c.jpg", "d.jpg"]);
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 0.9, Some(2));
        insert_face(&store, 2, &[0.0, 1.0, 0.0], 0.9, Some(1));
        insert_face(&store, 3, &[0.0, 1.0, 0.0], 0.9, Some(1));
        insert_face(&store, 4, &[0.0, 1.0, 0.0], 0.9, Some(1));
        let rows = people_overview(&store);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].count, 3, "人多的排前面");
        assert_eq!(rows[1].count, 1);
    }

    /// 清空人脸数据时,派生的人物标签一并作废。
    #[test]
    fn wipe_faces_clears_people_tags() {
        let _g = test_serial();
        let store = test_store();
        insert_photos(&store, &["a.jpg"]);
        crate::ai_tags::seed_labels(&store).unwrap();
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 0.9, None);
        let ver = tag_ver(&store);
        crate::ai_tags::sync_people_tags(&store, &[1], &ver).unwrap();
        assert_eq!(crate::ai_tags::photo_tags(&store, 1, &ver).len(), 1);
        wipe_faces(&store).unwrap();
        assert!(crate::ai_tags::photo_tags(&store, 1, &ver).is_empty());
    }
}
