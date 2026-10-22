//! 人脸存储 / 增量聚类 / 质量评分 / 命名建议(对应原版 FR 特性)。
//! 能力映射:质量评分、命名建议、忽略簇、算法版本迁移(wipe_faces)。
use crate::ai::face::{Det, Faces};
use crate::db::Store;
use anyhow::Result;
use rusqlite::params;
use std::collections::HashMap;

/// 人脸算法版本(写 settings;与库中记录不一致时 GUI 提示重扫)。
/// 改动检测/聚类/嵌入算法时提升此版本号。
pub const FACE_ALGO_VERSION: &str = "v2";

pub fn photos_todo(store: &Store, cap: usize) -> Vec<(i64, String)> {
    let conn = store.0.lock().unwrap();
    let mut st = match conn.prepare(
        "SELECT id, path FROM photos
         WHERE id NOT IN (SELECT DISTINCT photo_id FROM faces)
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
    let conn = store.0.lock().unwrap();
    let mut blob = Vec::with_capacity(vec.len() * 4);
    for v in vec {
        blob.extend_from_slice(&v.to_le_bytes());
    }
    conn.execute(
        "INSERT INTO faces(photo_id,x1,y1,x2,y2,score,quality,ignored)
         VALUES(?1,?2,?3,?4,?5,?6,?7,0)",
        params![photo_id, x1 as f64, y1 as f64, x2 as f64, y2 as f64, score as f64, quality as f64],
    )?;
    let face_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO face_templates(face_id,dim,vec) VALUES(?1,?2,?3)",
        params![face_id, vec.len() as i64, blob],
    )?;
    Ok(face_id)
}

/// 人脸质量评分 [0,1](对应原版 facequality):
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

/// 增量聚类:未分配且未忽略的脸与各簇**质量加权质心**比余弦,
/// >= thr 归簇,否则新建簇。忽略的脸(ignored=1)不参与也不成簇。
pub fn assign_clusters(store: &Store, thr: f32) -> Result<usize> {
    let conn = store.0.lock().unwrap();
    let mut clusters: HashMap<i64, Vec<(Vec<f32>, f32)>> = HashMap::new();
    {
        let mut st = conn.prepare(
            "SELECT f.cluster, t.vec, COALESCE(f.quality,0.5) FROM faces f
             JOIN face_templates t ON t.face_id=f.id
             WHERE f.cluster IS NOT NULL AND f.ignored=0",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let cid: i64 = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            let q: f64 = r.get(2)?;
            clusters.entry(cid).or_default().push((blob_to_vec(&blob), q as f32));
        }
    }
    let todo: Vec<(i64, Vec<f32>, f32)> = {
        let mut st = conn.prepare(
            "SELECT f.id, t.vec, COALESCE(f.quality,0.5) FROM faces f
             JOIN face_templates t ON t.face_id=f.id
             WHERE f.cluster IS NULL AND f.ignored=0",
        )?;
        let mut rows = st.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            let q: f64 = r.get(2)?;
            out.push((id, blob_to_vec(&blob), q as f32));
        }
        out
    };
    for (face_id, v, q) in todo {
        let mut best: Option<(i64, f32)> = None;
        for (&cid, vs) in &clusters {
            if let Some(m) = mean_vec_weighted(vs) {
                let c = cos(&m, &v);
                if best.map(|(_, b)| c > b).unwrap_or(true) {
                    best = Some((cid, c));
                }
            }
        }
        match best {
            Some((cid, c)) if c >= thr => {
                conn.execute("UPDATE faces SET cluster=?1 WHERE id=?2", params![cid, face_id])?;
                clusters.get_mut(&cid).unwrap().push((v, q));
            }
            _ => {
                let next = clusters.keys().copied().max().unwrap_or(0) + 1;
                conn.execute("UPDATE faces SET cluster=?1 WHERE id=?2", params![next, face_id])?;
                clusters.insert(next, vec![(v, q)]);
            }
        }
    }
    // 簇的默认名不写 people 表:people_overview 是 LEFT JOIN,name 为 NULL 时
    // GUI 回退显示"簇 N";rename_cluster 才会写入 people(id=cluster, name)。
    Ok(clusters.len())
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
    let conn = store.0.lock().unwrap();
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
    let conn = store.0.lock().unwrap();
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
    st.query_map(params![cluster, include_ignored as i32, min_quality as f64], |r| {
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
}

/// 单张照片的全部人脸(预览窗叠框用;坐标 = 未旋转原图像素)。
pub fn faces_for_photo(store: &Store, photo_id: i64) -> Vec<FaceHit> {
    let conn = store.0.lock().unwrap();
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
}

pub fn rename_cluster(store: &Store, cluster: i64, name: &str) -> Result<()> {
    store
        .0
        .lock()
        .unwrap()
        .execute("INSERT OR REPLACE INTO people(id,name) VALUES(?1,?2)", params![cluster, name])?;
    Ok(())
}

/// 忽略/恢复整个簇(对应原版 ignoreface:误检簇一键排除)。
pub fn ignore_cluster(store: &Store, cluster: i64, ignored: bool) -> Result<()> {
    store
        .0
        .lock()
        .unwrap()
        .execute(
            "UPDATE faces SET ignored=?1 WHERE cluster=?2",
            params![ignored as i32, cluster],
        )?;
    Ok(())
}

pub fn analyzed_photo_ids(store: &Store) -> Vec<i64> {
    let conn = store.0.lock().unwrap();
    let mut st = match conn.prepare("SELECT DISTINCT photo_id FROM faces") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    st.query_map([], |r| r.get(0))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

pub fn photo_ids_done(store: &Store) -> std::collections::HashSet<i64> {
    analyzed_photo_ids(store).into_iter().collect()
}

/// 被忽略的簇列表(GUI "显示已忽略" 时恢复用)。
pub fn ignored_clusters(store: &Store) -> Vec<i64> {
    let conn = store.0.lock().unwrap();
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
}

/// 清空全部人脸数据(算法版本迁移:旧算法产物作废,整库重扫)。
pub fn wipe_faces(store: &Store) -> Result<()> {
    let conn = store.0.lock().unwrap();
    conn.execute("DELETE FROM face_templates", [])?;
    conn.execute("DELETE FROM faces", [])?;
    conn.execute("DELETE FROM people", [])?;
    Ok(())
}

/// 后台扫描一批未分析照片。
pub fn scan_batch(store: &Store, cap: usize) -> Result<(usize, usize)> {
    let done = photo_ids_done(store);
    let todo = photos_todo(store, cap);
    let mut faces_engine = match Faces::load() {
        Ok(e) => e,
        Err(e) => return Err(anyhow::anyhow!(e)),
    };
    let mut face_count = 0;
    let mut photo_count = 0;
    for (pid, path) in todo {
        if done.contains(&pid) {
            continue;
        }
        let img = match image::open(&path) {
            Ok(i) => i,
            Err(_) => continue,
        };
        let res = match faces_engine.analyze(&img) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for f in &res {
            let q = face_quality(&img, &f.det);
            save_face(store, pid, f.det.x0, f.det.y0, f.det.x1, f.det.y1, f.det.score, q, &f.emb)?;
        }
        face_count += res.len();
        photo_count += 1;
    }
    assign_clusters(store, 0.5)?;
    Ok((photo_count, face_count))
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

/// 生成命名建议(score 降序)。thr 建议下限(ArcFace 512 维:
/// 同人通常 >0.6,异人 <0.3;0.55 起步,分数展示供用户判断)。
pub fn face_suggestions(store: &Store, thr: f32) -> Vec<Suggestion> {
    let conn = store.0.lock().unwrap();
    // 已命名簇 -> (name, [(vec, quality)])
    let mut named: HashMap<i64, (String, Vec<(Vec<f32>, f32)>)> = HashMap::new();
    {
        let mut st = match conn.prepare(
            "SELECT f.cluster, p.name, t.vec, COALESCE(f.quality,0.5) FROM faces f
             JOIN face_templates t ON t.face_id=f.id
             JOIN people p ON p.id=f.cluster
             WHERE f.ignored=0",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let mut rows = match st.query([]) {
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
            named.entry(cid).or_insert_with(|| (name, Vec::new())).1.push((blob_to_vec(&blob), q as f32));
        }
    }
    let named_centroids: Vec<(String, Vec<f32>)> = named
        .iter()
        .filter_map(|(_, (n, vs))| mean_vec_weighted(vs).map(|m| (n.clone(), m)))
        .collect();
    if named_centroids.is_empty() {
        return Vec::new();
    }
    // 未命名簇 -> centroid + count
    let mut unnamed: HashMap<i64, Vec<(Vec<f32>, f32)>> = HashMap::new();
    {
        let mut st = match conn.prepare(
            "SELECT f.cluster, t.vec, COALESCE(f.quality,0.5) FROM faces f
             JOIN face_templates t ON t.face_id=f.id
             LEFT JOIN people p ON p.id=f.cluster
             WHERE p.id IS NULL AND f.cluster IS NOT NULL AND f.ignored=0",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let mut rows = match st.query([]) {
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
            unnamed.entry(cid).or_default().push((blob_to_vec(&blob), q as f32));
        }
    }
    let mut out: Vec<Suggestion> = unnamed
        .iter()
        .filter_map(|(&cid, vs)| {
            let centroid = mean_vec_weighted(vs)?;
            let mut best: Option<(String, f32)> = None;
            for (name, c) in &named_centroids {
                let s = cos(&centroid, c);
                if best.as_ref().map(|(_, b)| s > *b).unwrap_or(true) {
                    best = Some((name.clone(), s));
                }
            }
            let (name, score) = best?;
            if score >= thr {
                Some(Suggestion { cluster: cid, name, score, count: vs.len() })
            } else {
                None
            }
        })
        .collect();
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> Store {
        Store::open_mem().unwrap()
    }

    /// 直接插入一张带簇/质量的人脸(绕过模型,测聚类逻辑)。
    fn insert_face(store: &Store, photo_id: i64, vec: &[f32], quality: f32, cluster: Option<i64>) {
        let mut blob = Vec::new();
        for v in vec {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        let c = store.0.lock().unwrap();
        c.execute(
            "INSERT INTO faces(photo_id,x1,y1,x2,y2,score,quality,ignored,cluster)
             VALUES(?1,10,10,50,50,0.9,?2,0,?3)",
            params![photo_id, quality as f64, cluster],
        )
        .unwrap();
        let fid = c.last_insert_rowid();
        c.execute(
            "INSERT INTO face_templates(face_id,dim,vec) VALUES(?1,?2,?3)",
            params![fid, vec.len() as i64, blob],
        )
        .unwrap();
    }

    #[test]
    fn cluster_roundtrip() {
        let store = test_store();
        let conn = store.0.lock().unwrap();
        conn.execute("INSERT INTO photos(id,path,parent,mtime,size) VALUES(90,'x.jpg','.','0',1)", [])
            .unwrap();
        drop(conn);
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.9, 0.1, 0.0];
        let c = vec![0.0, 1.0, 0.0];
        save_face(&store, 90, 0.0, 0.0, 1.0, 1.0, 0.9, 0.8, &a).unwrap();
        save_face(&store, 90, 0.0, 0.0, 1.0, 1.0, 0.9, 0.8, &b).unwrap();
        save_face(&store, 90, 0.0, 0.0, 1.0, 1.0, 0.9, 0.8, &c).unwrap();
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
            let c = if (x / 4 + y / 4) % 2 == 0 { 20u8 } else { 235u8 };
            *p = image::Rgb([c, c, c]);
        }
        let sharp = image::DynamicImage::ImageRgb8(img.clone());
        let blurry = image::DynamicImage::ImageRgb8(
            image::imageops::resize(&img, 24, 24, image::imageops::FilterType::Triangle),
        )
        .resize(240, 240, image::imageops::FilterType::Triangle);
        let det = Det { x0: 60.0, y0: 60.0, x1: 180.0, y1: 180.0, score: 0.9, kps: [(0.0, 0.0); 5] };
        let qs = face_quality(&sharp, &det);
        let qb = face_quality(&blurry, &det);
        assert!(qs > 0.6, "清晰脸应高分: {qs}");
        assert!(qs > qb + 0.1, "清晰应显著高于模糊: {qs} vs {qb}");
    }

    /// 未命名簇按质心相似度获得命名建议;不相似的没有。
    #[test]
    fn suggestion_for_similar_unnamed_cluster() {
        let store = test_store();
        let conn = store.0.lock().unwrap();
        conn.execute("INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)", [])
            .unwrap();
        conn.execute("INSERT INTO photos(id,path,parent,mtime,size) VALUES(2,'b.jpg','.',0,1)", [])
            .unwrap();
        drop(conn);
        // 簇 1 = 张三(已命名)
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        rename_cluster(&store, 1, "张三").unwrap();
        // 簇 2 与张三高度相似(未命名);簇 3 正交(不应建议)
        insert_face(&store, 1, &[0.95, 0.31, 0.0], 1.0, Some(2));
        insert_face(&store, 2, &[0.0, 1.0, 0.0], 1.0, Some(3));
        let sugs = face_suggestions(&store, 0.55);
        assert_eq!(sugs.len(), 1, "只应有 1 条建议: {:?}", sugs.iter().map(|s| (&s.name, s.score)).collect::<Vec<_>>());
        assert_eq!(sugs[0].name, "张三");
        assert_eq!(sugs[0].cluster, 2);
        assert!(sugs[0].score > 0.9, "score={}", sugs[0].score);
        assert_eq!(sugs[0].count, 1);
    }

    /// 忽略簇后不出现在列表/网格,恢复后回来。
    #[test]
    fn ignore_hides_from_overview() {
        let store = test_store();
        let conn = store.0.lock().unwrap();
        conn.execute("INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)", [])
            .unwrap();
        conn.execute("INSERT INTO photos(id,path,parent,mtime,size) VALUES(2,'b.jpg','.',0,1)", [])
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
        assert!(faces_for_cluster(&store, cid, false, 0.0).is_empty(), "默认不显示已忽略");
        assert_eq!(faces_for_cluster(&store, cid, true, 0.0).len(), 2, "显式包含时可看");
        ignore_cluster(&store, cid, false).unwrap();
        assert_eq!(people_overview(&store).len(), 1, "恢复后回来");
    }

    /// 低质量脸以低权重参与质心,不把簇心拉偏(对应原版 facequality 用途)。
    #[test]
    fn weighted_centroid_resists_low_quality_outlier() {
        let store = test_store();
        let conn = store.0.lock().unwrap();
        conn.execute("INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)", [])
            .unwrap();
        drop(conn);
        // 簇 1:一张高质量 x + 一张 0.05 权重的 75° 异类脸
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, Some(1));
        insert_face(&store, 1, &[0.259, 0.966, 0.0], 0.05, Some(1));
        // 未分配的新脸与 x 相同
        insert_face(&store, 1, &[1.0, 0.0, 0.0], 1.0, None);
        // thr=0.9:无权重的均值质心 cos≈0.79 会另起新簇;加权后 cos≈0.999 归簇 1
        assign_clusters(&store, 0.9).unwrap();
        let rows = people_overview(&store);
        assert_eq!(rows.len(), 1, "低质量异类不应撕裂簇: {:?}", rows.iter().map(|r| (r.cluster, r.count)).collect::<Vec<_>>());
        assert_eq!(rows[0].count, 3);
    }
}
