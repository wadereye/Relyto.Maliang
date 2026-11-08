//! 人脸存储 / 增量聚类 / 质量评分 / 命名建议。
//! 能力映射:质量评分、命名建议、忽略簇、算法版本迁移(wipe_faces)。
use crate::ai::face::{Det, Faces};
use crate::db::Store;
use anyhow::Result;
use rusqlite::params;
use std::collections::HashMap;

/// 人脸算法版本(写 settings;与库中记录不一致时 GUI 提示重扫)。
/// 检测/嵌入变化使库中向量不通用,必须升版本;只动聚类策略(阈值/门控/
/// 合并)向量仍通用,不升 —— 重聚类即可应用,升了反而误导用户清数据。
/// v3: 修复识别裁剪图预处理(to_rgb32f 0~1 值误按 0~255 归一化)。
/// v4: 检测/识别切换 YuNet + AdaFace IR-50(替换非商用 antelopev2)为默认方案;
///     antelopev2 保留为可选方案,版本号连同方案名一起比对。
/// v5: EXIF 方向摆正后再检测(竖拍手机照不再因人脸横躺而漏检);
///     检测输入 640 → 1280(小脸检出率);聚类阈值 0.5 → 0.6;
///     photos_todo 改按 photos.face_done 标记(没检出人脸的照片不再卡队列)。
pub const FACE_ALGO_VERSION: &str = "v5";

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

/// 当前人脸方案(选项 > 常规 选择,默认 YuNet + AdaFace)。
pub fn current_algo(store: &Store) -> crate::ai::face::FaceAlgo {
    crate::ai::face::FaceAlgo::from_key(&store.get_setting("face_algo").unwrap_or_default())
}

/// 算法标识 = 版本 + 方案:两者任一变化,库中向量都不通用,提示重扫。
pub fn algo_tag(store: &Store) -> String {
    format!("{}-{}", FACE_ALGO_VERSION, current_algo(store).key())
}

pub fn photos_todo(store: &Store, cap: usize) -> Vec<(i64, String)> {
    store.read(|conn| {
        // face_done = 已分析标记:**没检出人脸的照片也置 1**。此前用
        // "faces 表里有没有行"判定,导致无人脸照片永远留在队头反复重跑,
        // 后面的照片永远轮不到 —— 扫描看起来"只扫了一小部分"就是它。
        // kind=0 只扫图片;视频没有人脸可检,不浪费模型推理。
        let mut st = match conn.prepare(
            "SELECT id, path FROM photos
             WHERE face_done = 0 AND kind = 0
             ORDER BY id LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map(params![cap as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    })
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
    store.write(move |conn| -> Result<i64> {
        conn.execute(
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
        let face_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO face_templates(face_id,dim,vec) VALUES(?1,?2,?3)",
            params![face_id, vec.len() as i64, blob],
        )?;
        Ok(face_id)
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

/// 质量加权质心(mean + L2 归一化);低质量脸保底权重 0.05。
fn mean_vec_weighted(vs: &[(Vec<f32>, f32)]) -> Option<Vec<f32>> {
    if vs.is_empty() {
        return None;
    }
    let mut m = vec![0.0f32; vs[0].0.len()];
    let mut wsum = 0.0f32;
    for (v, q) in vs {
        let w = q.max(0.05);
        for (a, b) in m.iter_mut().zip(v) {
            *a += b * w;
        }
        wsum += w;
    }
    if wsum <= 0.0 {
        return None;
    }
    for v in &mut m {
        *v /= wsum;
    }
    let norm = m.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm <= 0.0 {
        return None;
    }
    for v in &mut m {
        *v /= norm;
    }
    Some(m)
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

/// 增量聚类:未分配且未忽略的脸与各簇**质量加权质心**比余弦,
/// 过该簇的成熟度阈值且与次高簇拉开足够余量才归簇;都不够格则新建簇,
/// 模棱两可的留在未分配,下批随质心移动或合并结果再判。
/// 忽略的脸(ignored=1)不参与也不成簇。
pub fn assign_clusters(store: &Store, thr: f32) -> Result<usize> {
    let dim = emb_dim(store);
    let mut clusters: HashMap<i64, Vec<(Vec<f32>, f32)>> = store.read(|conn| -> Result<_> {
        let mut clusters: HashMap<i64, Vec<(Vec<f32>, f32)>> = HashMap::new();
        let mut st = conn.prepare(
            "SELECT f.cluster, t.vec, COALESCE(f.quality,0.5) FROM faces f
             JOIN face_templates t ON t.face_id=f.id AND t.dim=?1
             WHERE f.cluster IS NOT NULL AND f.ignored=0",
        )?;
        let mut rows = st.query(params![dim])?;
        while let Some(r) = rows.next()? {
            let cid: i64 = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            let q: f64 = r.get(2)?;
            clusters
                .entry(cid)
                .or_default()
                .push((blob_to_vec(&blob), q as f32));
        }
        Ok(clusters)
    })?;
    let todo: Vec<(i64, Vec<f32>, f32)> = store.read(|conn| -> Result<_> {
        let mut st = conn.prepare(
            "SELECT f.id, t.vec, COALESCE(f.quality,0.5) FROM faces f
             JOIN face_templates t ON t.face_id=f.id AND t.dim=?1
             WHERE f.cluster IS NULL AND f.ignored=0",
        )?;
        let mut rows = st.query(params![dim])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            let q: f64 = r.get(2)?;
            out.push((id, blob_to_vec(&blob), q as f32));
        }
        Ok(out)
    })?;
    // 先在内存里定簇,再一次事务写回 —— 每脸一条 UPDATE 也锁一次写连接。
    let mut updates: Vec<(i64, i64)> = Vec::with_capacity(todo.len());
    for (face_id, v, q) in todo {
        // 各簇按余弦打分;合格 = 分数过该簇的成熟度阈值(年轻簇收紧)。
        let mut scored: Vec<(i64, f32)> = Vec::new();
        for (&cid, vs) in &clusters {
            if let Some(m) = mean_vec_weighted(vs) {
                scored.push((cid, cos(&m, &v)));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let best = scored
            .iter()
            .copied()
            .find(|(cid, c)| *c >= join_thr(thr, clusters[cid].len()));
        match best {
            Some((cid, c)) => {
                // 次高簇只差一点 = 归属存疑,不当场归簇也不另开新簇。
                let runner = scored
                    .iter()
                    .filter(|(other, _)| *other != cid)
                    .map(|(_, c)| *c)
                    .fold(f32::NEG_INFINITY, f32::max);
                if c - runner >= CLUSTER_MARGIN {
                    updates.push((face_id, cid));
                    clusters.get_mut(&cid).unwrap().push((v, q));
                }
            }
            None => {
                let next = clusters.keys().copied().max().unwrap_or(0) + 1;
                updates.push((face_id, next));
                clusters.insert(next, vec![(v, q)]);
            }
        }
    }
    store.write(|conn| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        {
            let mut st = tx.prepare_cached("UPDATE faces SET cluster=?1 WHERE id=?2")?;
            for (face_id, cid) in &updates {
                st.execute(params![cid, face_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    })?;
    // 簇的默认名不写 people 表:people_overview 是 LEFT JOIN,name 为 NULL 时
    // GUI 回退显示"簇 N";rename_cluster 才会写入 people(id=cluster, name)。
    Ok(clusters.len())
}

/// 合并质心足够相近的簇(贪心聚类「只分不合」的补丁;质心级比较能修
/// 人脸级阈值修不好的分裂)。已命名的簇互不合并 —— 命名即用户意图,
/// 不越权改写;只有一方命名时名字跟到保留方(优先保留有名字的,
/// 其次成员多的)。返回减少的簇数。
pub fn merge_similar_clusters(store: &Store, thr: f32) -> Result<usize> {
    let (mut clusters, mut names): (
        HashMap<i64, Vec<(Vec<f32>, f32)>>,
        HashMap<i64, String>,
    ) = store.read(|conn| -> Result<_> {
        let mut st = conn.prepare(
            "SELECT f.cluster, t.vec, COALESCE(f.quality,0.5) FROM faces f
             JOIN face_templates t ON t.face_id=f.id AND t.dim=?1
             WHERE f.cluster IS NOT NULL AND f.ignored=0",
        )?;
        let mut rows = st.query(params![emb_dim(store)])?;
        let mut acc: HashMap<i64, Vec<(Vec<f32>, f32)>> = HashMap::new();
        while let Some(r) = rows.next()? {
            let cid: i64 = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            let q: f64 = r.get(2)?;
            acc.entry(cid).or_default().push((blob_to_vec(&blob), q as f32));
        }
        let mut names: HashMap<i64, String> = HashMap::new();
        let mut st = conn.prepare("SELECT id, name FROM people")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            names.insert(r.get(0)?, r.get(1)?);
        }
        Ok((acc, names))
    })?;
    if clusters.len() < 2 {
        return Ok(0);
    }
    // 内存里反复并当前最像的一对,直到没有合格对;簇号直接指向最终归宿
    // (被并的簇即刻移出,后续只会再并入仍存活的簇,不会出现链式改号)。
    let mut merges: Vec<(i64, i64)> = Vec::new(); // (被并掉的簇, 保留方)
    loop {
        let cents: Vec<(i64, Vec<f32>)> = clusters
            .iter()
            .filter_map(|(&cid, vs)| mean_vec_weighted(vs).map(|m| (cid, m)))
            .collect();
        let mut best: Option<(i64, i64, f32)> = None;
        for i in 0..cents.len() {
            for j in i + 1..cents.len() {
                let (a, ma) = &cents[i];
                let (b, mb) = &cents[j];
                if names.contains_key(a) && names.contains_key(b) {
                    continue;
                }
                let c = cos(ma, mb);
                if c >= thr && best.map(|(_, _, bc)| c > bc).unwrap_or(true) {
                    best = Some((*a, *b, c));
                }
            }
        }
        let Some((a, b, _)) = best else { break };
        // 保留方:有名字的一方优先,其次成员多的;平局取小号。
        let (keep, drop) = match (names.contains_key(&a), names.contains_key(&b)) {
            (true, false) => (a, b),
            (false, true) => (b, a),
            _ => {
                let (na, nb) = (clusters[&a].len(), clusters[&b].len());
                if na > nb || (na == nb && a < b) {
                    (a, b)
                } else {
                    (b, a)
                }
            }
        };
        let moved = clusters.remove(&drop).unwrap();
        clusters.get_mut(&keep).unwrap().extend(moved);
        if let Some(n) = names.remove(&drop) {
            names.entry(keep).or_insert(n);
        }
        merges.push((drop, keep));
    }
    if merges.is_empty() {
        return Ok(0);
    }
    let merged = merges.len();
    // 一次事务落库:被并簇的人脸改归属、名字行清掉,保留方写入最终名。
    store.write(move |conn| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        {
            let mut st = tx.prepare_cached("UPDATE faces SET cluster=?1 WHERE cluster=?2")?;
            for (drop, keep) in &merges {
                st.execute(params![keep, drop])?;
            }
            let mut del = tx.prepare_cached("DELETE FROM people WHERE id=?1")?;
            for (drop, _) in &merges {
                del.execute(params![drop])?;
            }
        }
        for (cid, name) in &names {
            conn.execute(
                "INSERT OR REPLACE INTO people(id,name) VALUES(?1,?2)",
                params![cid, name],
            )?;
        }
        tx.commit()?;
        Ok(())
    })?;
    Ok(merged)
}

#[derive(Clone)]
pub struct PeopleRow {
    pub cluster: i64,
    pub person: Option<String>,
    pub count: usize,
    /// 簇内平均人脸质量(0..1;低质量簇值得人工复核)。
    pub quality: f32,
}

/// 簇列表(忽略的人脸不计入;按人数升序,与 GUI 显示一致)。
pub fn people_overview(store: &Store) -> Vec<PeopleRow> {
    store.read(|conn| {
        let mut st = match conn.prepare(
            "SELECT f.cluster, p.name, COUNT(*), AVG(COALESCE(f.quality,0.5))
             FROM faces f LEFT JOIN people p ON p.id = f.cluster
             WHERE f.cluster IS NOT NULL AND f.ignored=0
             GROUP BY f.cluster ORDER BY count(f.cluster)",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map([], |r| {
            Ok(PeopleRow {
                cluster: r.get::<_, i64>(0)?,
                person: r.get(1)?,
                count: r.get::<_, i64>(2)? as usize,
                quality: r.get::<_, f64>(3)? as f32,
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

/// 一张照片的人脸 + 归属(右栏人物面板用)。
pub struct PhotoFace {
    pub face_id: i64,
    /// 所属簇;未聚类 = None。
    pub cluster: Option<i64>,
    /// 簇的姓名(未命名 = None)。
    pub person: Option<String>,
    pub ignored: bool,
    pub quality: f32,
    /// 人脸框(未旋转原图像素): (x1, y1, x2, y2)。
    pub rect: (f32, f32, f32, f32),
}

/// 单张照片的人脸清单,连簇号与姓名(左连接 people,未命名照样返回)。
pub fn photo_faces(store: &Store, photo_id: i64) -> Vec<PhotoFace> {
    store.read(|conn| {
        let mut st = match conn.prepare(
            "SELECT f.id, f.cluster, p.name, f.ignored, COALESCE(f.quality,0.5),
                    f.x1, f.y1, f.x2, f.y2
             FROM faces f LEFT JOIN people p ON p.id = f.cluster
             WHERE f.photo_id = ?1 ORDER BY f.id",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map(params![photo_id], |r| {
            Ok(PhotoFace {
                face_id: r.get(0)?,
                cluster: r.get(1)?,
                person: r.get(2)?,
                ignored: r.get::<_, i64>(3)? != 0,
                quality: r.get::<_, f64>(4)? as f32,
                rect: (
                    r.get::<_, f64>(5)? as f32,
                    r.get::<_, f64>(6)? as f32,
                    r.get::<_, f64>(7)? as f32,
                    r.get::<_, f64>(8)? as f32,
                ),
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    })
}

pub fn rename_cluster(store: &Store, cluster: i64, name: &str) -> Result<()> {
    store.write(|conn| -> Result<()> {
        conn.execute(
            "INSERT OR REPLACE INTO people(id,name) VALUES(?1,?2)",
            params![cluster, name],
        )?;
        Ok(())
    })?;
    Ok(())
}

/// 忽略/恢复整个簇(误检簇一键排除)。
pub fn ignore_cluster(store: &Store, cluster: i64, ignored: bool) -> Result<()> {
    store.write(|conn| -> Result<()> {
        conn.execute(
            "UPDATE faces SET ignored=?1 WHERE cluster=?2",
            params![ignored as i32, cluster],
        )?;
        Ok(())
    })?;
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
    analyzed_photo_ids(store).into_iter().collect()
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
/// 同时把 photos.face_done 归零,否则清除后没有照片会重新进队列。
pub fn wipe_faces(store: &Store) -> Result<()> {
    store.write(|conn| -> Result<()> {
        conn.execute("DELETE FROM face_templates", [])?;
        conn.execute("DELETE FROM faces", [])?;
        conn.execute("DELETE FROM people", [])?;
        conn.execute("UPDATE photos SET face_done = 0", [])?;
        Ok(())
    })?;
    Ok(())
}

/// 全库未分析照片数(人物页/状态栏显示剩余量)。
pub fn remaining_count(store: &Store) -> usize {
    store.read(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM photos WHERE face_done = 0 AND kind = 0",
            [],
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

/// 后台扫描一批未分析照片:每处理完一张就把(本批已走张数, 本批总张数)
/// 回报给调用方,供 UI 画百分比 —— 一批 200 张在机械盘上要跑几十秒,
/// 没有进度就是黑箱。不需要进度时传 `&|_, _| {}`。
pub fn scan_batch(
    store: &Store,
    cap: usize,
    on_progress: &(dyn Fn(usize, usize) + Send),
) -> Result<(usize, usize)> {
    let done = photo_ids_done(store);
    let todo = photos_todo(store, cap);
    let total = todo.len();
    let mut faces_engine = match Faces::load(current_algo(store)) {
        Ok(e) => e,
        Err(e) => return Err(anyhow::anyhow!(e)),
    };
    let mut face_count = 0;
    let mut photo_count = 0;
    // 本批走过的照片(含解码失败/无脸的)统一记 face_done=1 ——
    // 判定依据是"分析过",不是"检出过脸"。
    let mut walked: Vec<i64> = Vec::with_capacity(total);
    for (i, (pid, path)) in todo.into_iter().enumerate() {
        // 进度按"走过的张数"算,跳过的(已有结果 / 解不开)也算走完,才不会卡住不动。
        let step = i + 1;
        if done.contains(&pid) {
            on_progress(step, total);
            walked.push(pid);
            continue;
        }
        let p = std::path::Path::new(&path);
        // EXIF 方向摆正:竖拍手机照不转的话人脸是横躺的,检测/嵌入都会失败。
        // 旋转语义与 thumbs::apply_exif_rotation 一致(2/4 翻转,5-8 旋转)。
        let orient = crate::exif::read_orientation(p);
        let img = match image::open(p) {
            Ok(i) => i,
            Err(_) => {
                on_progress(step, total);
                walked.push(pid);
                continue;
            }
        };
        let img = if orient == 1 {
            img
        } else {
            crate::thumbs::apply_exif_rotation(img, orient)
        };
        let res = match faces_engine.analyze(&img) {
            Ok(r) => r,
            Err(_) => {
                on_progress(step, total);
                walked.push(pid);
                continue;
            }
        };
        // 库里统一存**未旋转原图**坐标(预览叠框处再按 EXIF 方向转一次),
        // 所以检测框要从摆正后的图映射回原图;质量评分先用摆正后的框算。
        let (rw, rh) = (img.width() as f32, img.height() as f32);
        for f in &res {
            let q = face_quality(&img, &f.det);
            let (ax, ay) = map_unorient(f.det.x0, f.det.y0, rw, rh, orient);
            let (bx, by) = map_unorient(f.det.x1, f.det.y1, rw, rh, orient);
            save_face(
                store,
                pid,
                ax.min(bx),
                ay.min(by),
                ax.max(bx),
                ay.max(by),
                f.det.score,
                q,
                &f.emb,
            )?;
        }
        face_count += res.len();
        photo_count += 1;
        walked.push(pid);
        on_progress(step, total);
    }
    if !walked.is_empty() {
        let holders = vec!["?"; walked.len()].join(",");
        let sql = format!(
            "UPDATE photos SET face_done = 1 WHERE face_done = 0 AND kind = 0 AND id IN ({holders})"
        );
        let pv: Vec<&dyn rusqlite::ToSql> =
            walked.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        store.write(move |conn| -> Result<()> {
            conn.execute(&sql, pv.as_slice())?;
            Ok(())
        })?;
    }
    let thr = cluster_thr(store);
    assign_clusters(store, thr)?;
    merge_similar_clusters(store, thr)?;
    Ok((photo_count, face_count))
}

/// 连续扫描直到库里没有未分析照片(批间聚类一次;cancel 置位后当批跑完即停)。
/// 返回累计(照片数, 人脸数)。终止保证:批内所有照片(含解码失败的)都会
/// 置 face_done=1,队列必然单调收缩,空批即退出。
pub fn scan_all(
    store: &Store,
    cap: usize,
    cancel: &std::sync::atomic::AtomicBool,
    on_progress: &(dyn Fn(usize, usize) + Send + Sync),
) -> Result<(usize, usize)> {
    use std::sync::atomic::Ordering;
    let (mut photo_total, mut face_total) = (0usize, 0usize);
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let (p, f) = scan_batch(store, cap, on_progress)?;
        photo_total += p;
        face_total += f;
        if p == 0 && f == 0 {
            break; // 队列已空(或剩余全是解不开的文件,也已标记走完)
        }
    }
    Ok((photo_total, face_total))
}

/// 全量重聚类:库内全部未忽略的脸按质量降序重做一遍贪心聚类,覆盖旧簇号;
/// 年轻簇吸收收紧,收尾再做一次质心级合并。
/// 已命名簇的名字按"旧质心 → 新簇"最近匹配保留(低于 0.5 的不保)。
/// 返回新簇数。
pub fn recluster_all(store: &Store, thr: f32) -> Result<usize> {
    let dim = emb_dim(store);
    let mut faces: Vec<(i64, Vec<f32>, f32)> = store.read(|conn| -> Result<_> {
        let mut st = conn.prepare(
            "SELECT f.id, t.vec, COALESCE(f.quality,0.5) FROM faces f
             JOIN face_templates t ON t.face_id=f.id AND t.dim=?1
             WHERE f.ignored=0",
        )?;
        let mut rows = st.query(params![dim])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            let q: f64 = r.get(2)?;
            out.push((id, blob_to_vec(&blob), q as f32));
        }
        Ok(out)
    })?;
    if faces.is_empty() {
        return Ok(0);
    }
    // 质量降序、同质量按 id:高质量脸先当种子,簇心不被模糊脸带偏。
    faces.sort_by(|a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    // 簇心存**未归一化的加权和**,吸收新脸时增量更新,避免每步重算均值。
    let mut clusters: Vec<(Vec<f32>, f32, usize)> = Vec::new(); // (加权和, 权重和, 张数)
    let mut assign: HashMap<i64, i64> = HashMap::with_capacity(faces.len());
    for (id, v, q) in &faces {
        let w = q.max(0.05);
        // 与增量路径同规则:年轻簇质心支撑弱,吸收收紧。
        // 批处理必须人人落位,余量规则在此不适用。
        let mut scored: Vec<(usize, f32)> = Vec::new();
        for (ci, (sum, wsum, _)) in clusters.iter().enumerate() {
            if let Some(m) = mean_vec_weighted(&[(sum.clone(), *wsum)]) {
                scored.push((ci, cos(&m, v)));
            }
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let best = scored
            .iter()
            .copied()
            .find(|(ci, c)| *c >= join_thr(thr, clusters[*ci].2));
        match best {
            Some((ci, _)) => {
                let (sum, wsum, n) = &mut clusters[ci];
                for (a, b) in sum.iter_mut().zip(v) {
                    *a += b * w;
                }
                *wsum += w;
                *n += 1;
                assign.insert(*id, ci as i64);
            }
            None => {
                clusters.push((v.clone(), w, 1));
                assign.insert(*id, (clusters.len() - 1) as i64);
            }
        }
    }
    // 写回新簇号(簇号 = 下标 + 1,与 assign_clusters 的 1 起编号一致)。
    // 旧簇号→名字的读取(上面 named)必须在这次覆盖前完成,顺序不可换。
    store.write(|conn| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        {
            let mut st = tx.prepare_cached("UPDATE faces SET cluster=?1 WHERE id=?2")?;
            for (face_id, ci) in &assign {
                st.execute(params![ci + 1, face_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    })?;
    // 命名保留:旧簇(id)质心 → 新簇(下标)最近匹配。
    // 新簇质心先一次算好,后面逐个旧名比余弦,避免反复重算均值。
    let named: Vec<(String, Vec<f32>)> = store.read(|conn| -> Result<_> {
        // 旧簇 id → (vecs, quality) 先聚合,名字按 id 另查。
        let mut st = conn.prepare(
            "SELECT f.cluster, t.vec, COALESCE(f.quality,0.5) FROM faces f
             JOIN face_templates t ON t.face_id=f.id AND t.dim=?1
             JOIN people p ON p.id=f.cluster
             WHERE f.ignored=0",
        )?;
        let mut rows = st.query(params![dim])?;
        let mut acc: HashMap<i64, Vec<(Vec<f32>, f32)>> = HashMap::new();
        while let Some(r) = rows.next()? {
            let cid: i64 = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            let q: f64 = r.get(2)?;
            acc.entry(cid).or_default().push((blob_to_vec(&blob), q as f32));
        }
        let mut out = Vec::new();
        for cid in acc.keys().copied().collect::<Vec<_>>() {
            let name: Option<String> = conn
                .query_row("SELECT name FROM people WHERE id=?1", params![cid], |r| {
                    r.get(0)
                })
                .ok();
            if let (Some(name), Some(m)) =
                (name, mean_vec_weighted(&acc[&cid]))
            {
                out.push((name, m));
            }
        }
        Ok(out)
    })?;
    let new_centroids: Vec<(i64, Vec<f32>)> = clusters
        .iter()
        .enumerate()
        .filter_map(|(i, (sum, wsum, _))| {
            mean_vec_weighted(&[(sum.clone(), *wsum)]).map(|m| (i as i64 + 1, m))
        })
        .collect();
    store.write(move |conn| -> Result<()> {
        for (name, m) in &named {
            let mut best: Option<(i64, f32)> = None;
            for (cid, cm) in &new_centroids {
                let c = cos(cm, m);
                if best.as_ref().map(|(_, b)| c > *b).unwrap_or(true) {
                    best = Some((*cid, c));
                }
            }
            if let Some((cid, c)) = best {
                if c >= 0.5 {
                    conn.execute(
                        "INSERT OR REPLACE INTO people(id,name) VALUES(?1,?2)",
                        params![cid, name],
                    )?;
                }
            }
        }
        Ok(())
    })?;
    // 收尾合并:年轻簇收紧仍可能把同人裂成两簇,质心级合并兜底,
    // 已命名的簇不受影响(互不合并)。返回值随之扣掉并掉的簇数。
    let merged = merge_similar_clusters(store, thr)?;
    Ok(clusters.len() - merged)
}

/// 未命名簇的命名建议:未命名簇质心与
/// 已命名簇质心比余弦,>= 阈值即建议该名字。只读,不写库。
#[derive(Clone, Debug)]
pub struct Suggestion {
    pub cluster: i64,
    pub name: String,
    pub score: f32,
    pub count: usize,
}

/// 生成命名建议(score 降序)。比对口径借鉴参考实现的簇间识别:
/// 未命名簇取质量最高的两张脸作 probe,与已命名簇的**全部模板**逐一比
/// 取最优(而非只比质心)—— 偏斜簇的质心会被多数同姿态脸拉偏,
/// probe×最优模板对侧脸为主的簇明显更稳。thr 为建议下限;只读,不写库。
pub fn face_suggestions(store: &Store, thr: f32) -> Vec<Suggestion> {
    let dim = emb_dim(store);
    store.read(|conn| {
        // 已命名簇 -> (name, [(vec, quality)])
        let mut named: HashMap<i64, (String, Vec<(Vec<f32>, f32)>)> = HashMap::new();
        {
            let mut st = match conn.prepare(
                "SELECT f.cluster, p.name, t.vec, COALESCE(f.quality,0.5) FROM faces f
                 JOIN face_templates t ON t.face_id=f.id AND t.dim=?1
                 JOIN people p ON p.id=f.cluster
                 WHERE f.ignored=0",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let mut rows = match st.query(params![dim]) {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            loop {
                let Ok(Some(r)) = rows.next() else { break };
                let (Ok(cid), Ok(name), Ok(blob), Ok(q)) = (
                    r.get::<_, i64>(0),
                    r.get::<_, String>(1),
                    r.get::<_, Vec<u8>>(2),
                    r.get::<_, f64>(3),
                ) else {
                    continue;
                };
                named
                    .entry(cid)
                    .or_insert_with(|| (name, Vec::new()))
                    .1
                    .push((blob_to_vec(&blob), q as f32));
            }
        }
        // 未命名簇 -> [(vec, quality)]
        let mut unnamed: HashMap<i64, Vec<(Vec<f32>, f32)>> = HashMap::new();
        {
            let mut st = match conn.prepare(
                "SELECT f.cluster, t.vec, COALESCE(f.quality,0.5) FROM faces f
                 JOIN face_templates t ON t.face_id=f.id AND t.dim=?1
                 LEFT JOIN people p ON p.id=f.cluster
                 WHERE p.id IS NULL AND f.cluster IS NOT NULL AND f.ignored=0",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let mut rows = match st.query(params![dim]) {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            loop {
                let Ok(Some(r)) = rows.next() else { break };
                let (Ok(cid), Ok(blob), Ok(q)) = (
                    r.get::<_, i64>(0),
                    r.get::<_, Vec<u8>>(1),
                    r.get::<_, f64>(2),
                ) else {
                    continue;
                };
                unnamed
                    .entry(cid)
                    .or_default()
                    .push((blob_to_vec(&blob), q as f32));
            }
        }
        let mut out: Vec<Suggestion> = unnamed
            .iter()
            .filter_map(|(&cid, vs)| {
                // probe:质量降序取前 2(不足则全部);质心兜底参与比对。
                let mut ranked = vs.clone();
                ranked.sort_by(|a, b| {
                    b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                });
                let probes: Vec<&Vec<f32>> = ranked.iter().take(2).map(|(v, _)| v).collect();
                let mut best: Option<(String, f32)> = None;
                for (name, tpl) in named.values() {
                    // probe × 对方簇全部模板取最优;簇内自身信息不参与(防自比满分)。
                    let mut s = 0.0f32;
                    for p in &probes {
                        for (v, _) in tpl {
                            s = s.max(cos(p, v));
                        }
                    }
                    if best.as_ref().map(|(_, b)| s > *b).unwrap_or(true) {
                        best = Some((name.clone(), s));
                    }
                }
                let (name, score) = best?;
                if score >= thr {
                    Some(Suggestion {
                        cluster: cid,
                        name,
                        score,
                        count: vs.len(),
                    })
                } else {
                    None
                }
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

    /// 直接插入一张带簇/质量的人脸(绕过模型,测聚类逻辑)。
    /// 测试向量不足方案维度时零填充到位 —— 零填充不改余弦值,查询端的
    /// t.dim 过滤才能命中。
    fn insert_face(store: &Store, photo_id: i64, vec: &[f32], quality: f32, cluster: Option<i64>) {
        let dim = emb_dim(store) as usize;
        let mut full = vec.to_vec();
        full.resize(dim.max(vec.len()), 0.0);
        let mut blob = Vec::new();
        for v in &full {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        let c = store.test_conn();
        c.execute(
            "INSERT INTO faces(photo_id,x1,y1,x2,y2,score,quality,ignored,cluster)
             VALUES(?1,10,10,50,50,0.9,?2,0,?3)",
            params![photo_id, quality as f64, cluster],
        )
        .unwrap();
        let fid = c.last_insert_rowid();
        c.execute(
            "INSERT INTO face_templates(face_id,dim,vec) VALUES(?1,?2,?3)",
            params![fid, full.len() as i64, blob],
        )
        .unwrap();
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
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(90,'x.jpg','.','0',1)",
            [],
        )
        .unwrap();
        drop(conn);
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.9, 0.1, 0.0];
        let c = vec![0.0, 1.0, 0.0];
        save_face_t(&store, 90, &a, 0.8).unwrap();
        save_face_t(&store, 90, &b, 0.8).unwrap();
        save_face_t(&store, 90, &c, 0.8).unwrap();
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
        assert!(sugs[0].score > 0.9, "score={}", sugs[0].score);
        assert_eq!(sugs[0].count, 1);
    }

    /// photos_todo 按 face_done/kind 过滤:没检出人脸的照片置 1 后不再重扫,
    /// 视频不进队列 —— "扫描只扫一小部分"卡死问题的回归测试。
    #[test]
    fn photos_todo_respects_face_done_and_kind() {
        let store = test_store();
        let conn = store.test_conn();
        // 1=待分析图片, 2=已分析图片, 3=视频
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size,face_done) VALUES(2,'b.jpg','.',0,1,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO photos(id,path,parent,mtime,size,kind) VALUES(3,'c.mp4','.',0,1,1)",
            [],
        )
        .unwrap();
        drop(conn);
        let todo = photos_todo(&store, 10);
        assert_eq!(
            todo.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![1],
            "只应剩未分析的图片: {todo:?}"
        );
        assert_eq!(remaining_count(&store), 1);
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

    /// 全量重聚类:质量降序重分簇,已命名按质心最近迁移到新簇号。
    #[test]
    fn recluster_all_rebuilds_and_keeps_names() {
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
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        rename_cluster(&store, 1, "张三").unwrap();
        insert_face(&store, 2, &[0.9, 0.1, 0.0], 0.9, Some(2));
        insert_face(&store, 3, &[0.0, 1.0, 0.0], 0.9, Some(3));
        let n = recluster_all(&store, 0.6).unwrap();
        assert_eq!(n, 2, "cos≈0.99 的两张聚一簇,正交一张: {n}");
        let rows = people_overview(&store);
        assert_eq!(rows.len(), 2);
        let named = rows.iter().find(|r| r.person.is_some()).expect("名字应保留");
        assert_eq!(named.person.as_deref(), Some("张三"));
        assert_eq!(named.count, 2, "两张相似脸应同在新簇");
        assert!(rows.iter().all(|r| r.count >= 1));
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
        drop(conn);
        // 簇 1(成熟,3 张):两张高质量 x + 一张 0.05 权重的 90° 异类脸
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        insert_face(&store, 1, &[0.0, 1.0, 0.0], 0.05, Some(1));
        // 未分配的新脸与 x 相同
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, None);
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
            rows.iter().map(|r| (r.cluster, r.count)).collect::<Vec<_>>()
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
        let merged = merge_similar_clusters(&store, 0.6).unwrap();
        assert_eq!(merged, 1, "应并掉一簇");
        let rows = people_overview(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 5, "两簇成员应合到一起");
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
        let merged = merge_similar_clusters(&store, 0.6).unwrap();
        assert_eq!(merged, 0, "双方都有名字不应并");
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
        let merged = merge_similar_clusters(&store, 0.6).unwrap();
        assert_eq!(merged, 1);
        let rows = people_overview(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].person.as_deref(), Some("李四"), "名字应保留");
        assert_eq!(rows[0].count, 5);
    }

    /// 右栏人物面板的数据口:一 photo 多脸,带簇号、姓名与忽略态。
    #[test]
    fn photo_faces_carries_cluster_name_and_rect() {
        let store = test_store();
        store.test_conn().execute(
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
        assert_eq!(faces.len(), 3, "未聚类的脸也要返回");
        assert_eq!(faces[0].person.as_deref(), Some("小明"));
        assert_eq!(faces[0].cluster, Some(1));
        assert!(!faces[0].ignored);
        assert_eq!(faces[0].rect, (10.0, 10.0, 50.0, 50.0));
        assert!(faces[1].ignored, "被忽略的簇要带着标记返回");
        assert_eq!(faces[1].person, None, "没命名的簇不该被写成空串");
        assert_eq!(faces[2].cluster, None);
        assert!(photo_faces(&store, 999).is_empty(), "别的照片不该混进来");
    }
}
