//! 检索:向量精确余弦、标签命中、OCR 全文,按 RRF 融合。

use crate::db::Store;
use anyhow::Result;
use rusqlite::params;
use std::collections::HashMap;

/// 向量落库(按照片主键覆盖,换版本时逐张重算)。
pub fn save_vec(store: &Store, photo_id: i64, ver: &str, emb: &[f32]) -> Result<(), String> {
    store.write(|conn| -> Result<(), String> {
        conn.execute(
            "INSERT OR REPLACE INTO clips_vec(photo_id, ver, dim, embedding) VALUES(?1,?2,?3,?4)",
            params![photo_id, ver, emb.len() as i64, f32_to_blob(emb)],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })
}

/// 按 id 过滤时的分块大小:一次绑太多会撞上 SQLite 的变量上限,整条查询直接失败。
const ID_CHUNK: usize = 900;

/// 只保留拍摄/修改日期落在 [start,end) 的照片 id(日期串按 YYYY-MM-DD 比较)。
pub fn ids_in_date_window(
    store: &Store,
    ids: &[i64],
    start: &str,
    end: &str,
) -> std::collections::HashSet<i64> {
    use std::collections::HashSet;
    let mut out = HashSet::new();
    if ids.is_empty() || start >= end {
        return out;
    }
    store.read(|conn| {
        for chunk in ids.chunks(ID_CHUNK) {
            let sql = format!(
                "SELECT id, taken_utc, mtime FROM photos WHERE id IN ({})",
                vec!["?"; chunk.len()].join(",")
            );
            let Ok(mut st) = conn.prepare(&sql) else {
                continue;
            };
            let Ok(rows) = st.query_map(rusqlite::params_from_iter(chunk.iter().copied()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            }) else {
                continue;
            };
            for (id, taken, mtime) in rows.flatten() {
                let Some(date) = taken
                    .as_deref()
                    .filter(|s| s.len() >= 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-')
                    .map(|s| s[..10].to_string())
                    .or_else(|| mtime_date(mtime))
                else {
                    continue;
                };
                if date.as_str() >= start && date.as_str() < end {
                    out.insert(id);
                }
            }
        }
    });
    out
}

/// 球面距离(公里)。
fn haversine_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let r = 6371.0088_f64;
    let (p1, p2) = (a.0.to_radians(), b.0.to_radians());
    let dp = p2 - p1;
    let dl = (b.1 - a.1).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * r * h.sqrt().asin()
}

/// 取一张照片的基准坐标(无定位返回 None)。
pub fn photo_gps(store: &Store, photo_id: i64) -> Option<(f64, f64)> {
    store.read(|conn| {
        conn.query_row(
            "SELECT lat, lon FROM photos WHERE id=?1",
            params![photo_id],
            |r| Ok((r.get::<_, Option<f64>>(0)?, r.get::<_, Option<f64>>(1)?)),
        )
        .ok()
        .and_then(|(a, b)| a.zip(b))
    })
}

/// 只保留距基准坐标不超过 km 的照片(无定位的行一律排除)。
pub fn ids_near_point(
    store: &Store,
    ids: &[i64],
    lat: f64,
    lon: f64,
    km: f64,
) -> std::collections::HashSet<i64> {
    use std::collections::HashSet;
    let mut out = HashSet::new();
    if ids.is_empty() || km <= 0.0 {
        return out;
    }
    store.read(|conn| {
        for chunk in ids.chunks(ID_CHUNK) {
            let sql = format!(
                "SELECT id, lat, lon FROM photos WHERE id IN ({})",
                vec!["?"; chunk.len()].join(",")
            );
            let Ok(mut st) = conn.prepare(&sql) else {
                continue;
            };
            let Ok(rows) = st.query_map(rusqlite::params_from_iter(chunk.iter().copied()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<f64>>(1)?,
                    r.get::<_, Option<f64>>(2)?,
                ))
            }) else {
                continue;
            };
            for (id, la, lo) in rows.flatten() {
                if let (Some(a), Some(b)) = (la, lo) {
                    if haversine_km((lat, lon), (a, b)) <= km {
                        out.insert(id);
                    }
                }
            }
        }
    });
    out
}

/// 把历史库里可能是秒/毫秒/微秒的 mtime 换成本地日期串。
fn mtime_date(v: i64) -> Option<String> {
    let secs = if v >= 100_000_000_000_000 {
        v / 1_000_000
    } else if v >= 100_000_000_000 {
        v / 1_000
    } else {
        v
    };
    let dt = chrono::DateTime::from_timestamp(secs, 0)?;
    Some(dt.with_timezone(&chrono::Local).date_naive().to_string())
}

/// 向量转小端字节。
pub fn f32_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// 字节转小端向量(长度不整时截断)。
pub fn blob_to_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// 读一张照片的向量。
pub fn load_vec(store: &Store, photo_id: i64, ver: &str) -> Option<Vec<f32>> {
    store.read(|conn| {
        conn.query_row(
            "SELECT embedding FROM clips_vec WHERE photo_id=?1 AND ver=?2",
            params![photo_id, ver],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .ok()
        .map(|b| blob_to_f32(&b))
    })
}

/// 当前版本的向量总数(进度展示用)。
pub fn count(store: &Store, ver: &str) -> usize {
    store.read(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM clips_vec WHERE ver=?1",
            params![ver],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize
    })
}

/// 定长 top-K:堆顶是当前最差的命中,挤不进来就丢弃,内存只占 K 条。
struct TopK {
    limit: usize,
    heap: std::collections::BinaryHeap<WorstFirst>,
}

/// 排序方向与展示顺序(分数降序、同分 id 升序)相反,好让堆顶先被淘汰。
#[derive(PartialEq)]
struct WorstFirst(i64, f32);

impl Eq for WorstFirst {}

impl PartialOrd for WorstFirst {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for WorstFirst {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .1
            .partial_cmp(&self.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(self.0.cmp(&other.0))
    }
}

impl TopK {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            heap: std::collections::BinaryHeap::new(),
        }
    }

    fn push(&mut self, id: i64, score: f32) {
        if self.limit == 0 || score.is_nan() {
            return;
        }
        let item = WorstFirst(id, score);
        if self.heap.len() < self.limit {
            self.heap.push(item);
        } else if self.heap.peek().is_some_and(|w| item < *w) {
            // 堆顶是最差命中;新项序更小(=分数更高)才挤得进来。
            self.heap.pop();
            self.heap.push(item);
        }
    }

    fn into_sorted(self) -> Vec<(i64, f32)> {
        let mut out: Vec<(i64, f32)> = self.heap.into_iter().map(|w| (w.0, w.1)).collect();
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        out
    }
}

/// 向量精确检索:边读边留 top limit,不把全库向量堆进内存。
pub fn vector_search(store: &Store, ver: &str, query: &[f32], limit: usize) -> Vec<(i64, f32)> {
    let mut top = TopK::new(limit);
    if limit == 0 || query.is_empty() {
        return Vec::new();
    }
    store.read(|conn| {
        let Ok(mut st) = conn.prepare("SELECT photo_id, embedding FROM clips_vec WHERE ver=?1")
        else {
            return;
        };
        let Ok(rows) = st.query_map(params![ver], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        }) else {
            return;
        };
        for (id, blob) in rows.flatten() {
            let v = blob_to_f32(&blob);
            if v.len() == query.len() {
                top.push(id, crate::ai_tags::cosine(query, &v));
            }
        }
    });
    top.into_sorted()
}

/// 以图搜图:全库精确余弦,排除查询照片自身。
pub fn vector_search_excluding(
    store: &Store,
    ver: &str,
    query: &[f32],
    exclude: i64,
    limit: usize,
) -> Vec<(i64, f32)> {
    let mut hits = vector_search(store, ver, query, limit + 1);
    hits.retain(|(id, _)| *id != exclude);
    hits.truncate(limit);
    hits
}

/// 标签命中:自动标签(中英文名与分类路径)与人工标签一起匹配,人工命中权重最高。
pub fn tag_search(store: &Store, ver: &str, query: &str, limit: usize) -> Vec<(i64, f32)> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) = conn.prepare(
            "SELECT photo_id, MAX(score) FROM (
               SELECT t.photo_id AS photo_id, t.score AS score
                 FROM auto_tags t JOIN tag_labels l ON l.id=t.label_id
                WHERE t.model_ver=?1
                  AND (instr(l.name_zh, ?2)>0 OR instr(lower(l.name_en), lower(?2))>0
                       OR instr(l.node, ?2)>0)
               UNION ALL
               SELECT pt.photo_id, 1.0 FROM photo_tags pt JOIN tags g ON g.id=pt.tag_id
                WHERE instr(g.name, ?2)>0
             ) GROUP BY photo_id ORDER BY MAX(score) DESC, photo_id LIMIT ?3",
        ) else {
            return out;
        };
        if let Ok(rows) = st.query_map(params![ver, q, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)? as f32))
        }) {
            out.extend(rows.flatten());
        }
        out
    })
}

/// 描述命中:标题子串匹配(不区分大小写,命中位置靠前的优先)。
pub fn caption_search(store: &Store, query: &str, limit: usize) -> Vec<(i64, f32)> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) = conn.prepare(
            "SELECT id, instr(lower(COALESCE(caption,'')), ?1) AS pos FROM photos
              WHERE instr(lower(COALESCE(caption,'')), ?1) > 0
              ORDER BY pos, id LIMIT ?2",
        ) else {
            return out;
        };
        if let Ok(rows) = st.query_map(params![q, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as f32))
        }) {
            out.extend(rows.flatten());
        }
        out
    })
}

/// Reciprocal Rank Fusion:多路结果按 1/(k+rank) 相加。
pub fn fuse(lists: &[Vec<(i64, f32)>], k: f32, limit: usize) -> Vec<(i64, f32)> {
    use std::collections::HashMap;
    let mut acc: HashMap<i64, f32> = HashMap::new();
    for list in lists {
        for (rank, (id, _)) in list.iter().enumerate() {
            *acc.entry(*id).or_insert(0.0) += 1.0 / (k + rank as f32 + 1.0);
        }
    }
    let mut out: Vec<(i64, f32)> = acc.into_iter().collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(limit);
    out
}

/// 混合检索:向量(有则用)+ 标签 + OCR + 描述,四路融合排序。
/// 分数是查询与照片的真实余弦;只由文本通道命中的照片没有余弦,记 None。
pub fn hybrid(
    store: &Store,
    ver: &str,
    tag_ver: &str,
    query: &str,
    emb: Option<&[f32]>,
    limit: usize,
) -> Vec<(i64, Option<f32>)> {
    let mut lists: Vec<Vec<(i64, f32)>> = Vec::new();
    if let Some(e) = emb {
        lists.push(vector_search(store, ver, e, limit * 2));
    }
    lists.push(tag_search(store, tag_ver, query, limit * 2));
    lists.push(crate::ai_ocr::search(store, query, limit * 2));
    lists.push(caption_search(store, query, limit * 2));
    let ids: Vec<i64> = fuse(&lists, 60.0, limit)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let sims = match emb {
        Some(e) => cosine_for(store, ver, &ids, e),
        None => HashMap::new(),
    };
    ids.into_iter()
        .map(|id| (id, sims.get(&id).copied()))
        .collect()
}

/// 只读命中集的向量算余弦(融合可能带进向量榜外的照片,不能只复用榜单分数)。
fn cosine_for(store: &Store, ver: &str, ids: &[i64], query: &[f32]) -> HashMap<i64, f32> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return out;
    }
    store.read(|conn| {
        let Ok(mut st) =
            conn.prepare("SELECT embedding FROM clips_vec WHERE photo_id=?1 AND ver=?2")
        else {
            return out;
        };
        for id in ids {
            if let Ok(blob) = st.query_row(params![id, ver], |r| r.get::<_, Vec<u8>>(0)) {
                let v = blob_to_f32(&blob);
                if v.len() == query.len() {
                    out.insert(*id, crate::ai_tags::cosine(query, &v));
                }
            }
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn add_photo(store: &Store, id: i64) {
        store
            .write(|conn| {
                conn.execute(
                    "INSERT INTO photos(id, path, parent, mtime, size) VALUES(?1, ?2, 'C:\\', 0, 0)",
                    params![id, format!("C:\\p{id}.jpg")],
                )
            })
            .unwrap();
    }

    #[test]
    fn blob_roundtrip() {
        let v = vec![0.5f32, -0.25, 1.0];
        assert_eq!(blob_to_f32(&f32_to_blob(&v)), v);
    }

    #[test]
    fn vector_search_orders_by_cosine() {
        let store = Store::open_mem().unwrap();
        for id in [1, 2, 3] {
            add_photo(&store, id);
        }
        save_vec(&store, 1, "v1", &[1.0, 0.0]).unwrap();
        save_vec(&store, 2, "v1", &[0.0, 1.0]).unwrap();
        save_vec(&store, 3, "v1", &[0.8, 0.6]).unwrap();
        let got = vector_search(&store, "v1", &[1.0, 0.0], 3);
        assert_eq!(got[0].0, 1);
        assert_eq!(got[1].0, 3);
        assert_eq!(got[2].0, 2);
        assert!(vector_search(&store, "v0", &[1.0, 0.0], 3).is_empty());
    }

    #[test]
    fn vector_search_excluding_drops_self_and_caps() {
        let store = Store::open_mem().unwrap();
        for id in [1, 2, 3] {
            add_photo(&store, id);
        }
        save_vec(&store, 1, "v1", &[1.0, 0.0]).unwrap();
        save_vec(&store, 2, "v1", &[0.0, 1.0]).unwrap();
        save_vec(&store, 3, "v1", &[0.8, 0.6]).unwrap();
        let got = vector_search_excluding(&store, "v1", &[1.0, 0.0], 1, 5);
        assert_eq!(got.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![3, 2]);
        let one = vector_search_excluding(&store, "v1", &[1.0, 0.0], 1, 1);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].0, 3);
    }

    #[test]
    fn fusion_prefers_items_in_multiple_lists() {
        let a = vec![(1, 0.9), (2, 0.5)];
        let b = vec![(2, 0.8), (3, 0.4)];
        let got = fuse(&[a, b], 60.0, 3);
        assert_eq!(got[0].0, 2, "两路都出现的应排第一");
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn tag_search_matches_chinese_and_english() {
        let store = Store::open_mem().unwrap();
        add_photo(&store, 11);
        crate::ai_tags::seed_labels(&store).unwrap();
        crate::ai_tags::save(&store, 11, "v1", &[(2, 0.8)]).unwrap();
        assert!(tag_search(&store, "v1", "合影", 5)
            .iter()
            .any(|(id, _)| *id == 11));
        assert!(tag_search(&store, "v1", "group", 5)
            .iter()
            .any(|(id, _)| *id == 11));
        assert!(tag_search(&store, "v1", "猫", 5).is_empty());
    }

    /// 标签检索要覆盖分类路径与人工标签,人工命中排在自动命中之前。
    #[test]
    fn tag_search_covers_node_paths_and_manual_tags() {
        let store = Store::open_mem().unwrap();
        crate::ai_tags::seed_labels(&store).unwrap();
        for id in 1..=4 {
            add_photo(&store, id);
        }
        let cat = store.read(|c| {
            c.query_row(
                "SELECT id FROM tag_labels WHERE node='动物/猫'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        });
        crate::ai_tags::save(&store, 1, "v1", &[(cat, 0.5)]).unwrap();
        crate::timeline::add_tag(&store, 2, "猫").unwrap();
        crate::timeline::add_tag(&store, 3, "人物").unwrap();
        crate::timeline::add_tag(&store, 4, "旅行/2024").unwrap();
        let ids = |q: &str| {
            tag_search(&store, "v1", q, 10)
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("动物"), vec![1], "分类路径要能命中");
        assert_eq!(ids("人物"), vec![3], "人工标签要能命中");
        assert_eq!(ids("旅行"), vec![4], "人工层级标签按前缀命中");
        assert_eq!(ids("cat"), vec![1], "英文名仍要命中");
        let both = tag_search(&store, "v1", "猫", 10);
        assert_eq!(
            both.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![2, 1],
            "人工命中权重更高"
        );
        assert!(both[0].1 > both[1].1);
        assert!(ids("不存在").is_empty());
    }

    #[test]
    fn hybrid_merges_tag_and_ocr_hits_without_vector() {
        let store = Store::open_mem().unwrap();
        add_photo(&store, 11);
        add_photo(&store, 12);
        crate::ai_tags::seed_labels(&store).unwrap();
        crate::ai_tags::save(&store, 11, "v1", &[(2, 0.8)]).unwrap();
        crate::ai_ocr::save(&store, 12, "合影 发票", &[], "v1").unwrap();
        add_photo(&store, 13);
        store.set_caption(13, "合影留念").unwrap();
        let hits = hybrid(&store, "v1", "v1", "合影", None, 10);
        let ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&11), "标签命中应进入融合结果");
        assert!(ids.contains(&12), "图中文字命中应进入融合结果");
        assert!(ids.contains(&13), "描述命中应进入融合结果");
        assert!(hits.iter().all(|(_, s)| s.is_none()), "无查询向量时不编造分数");
    }

    #[test]
    fn hybrid_reports_real_cosine_and_marks_text_only_hits() {
        let store = Store::open_mem().unwrap();
        for id in [1, 2, 3] {
            add_photo(&store, id);
        }
        crate::ai_tags::seed_labels(&store).unwrap();
        crate::ai_tags::save(&store, 3, "v1", &[(2, 0.8)]).unwrap();
        save_vec(&store, 1, "v1", &[1.0, 0.0]).unwrap();
        save_vec(&store, 2, "v1", &[0.0, 1.0]).unwrap();
        let hits = hybrid(&store, "v1", "v1", "合影", Some(&[1.0, 0.0]), 10);
        let by_id: HashMap<i64, Option<f32>> = hits.into_iter().collect();
        assert!((by_id[&1].unwrap() - 1.0).abs() < 1e-6, "同向余弦为 1");
        assert!(by_id[&2].unwrap().abs() < 1e-6, "正交余弦为 0");
        assert_eq!(by_id[&3], None, "只由标签命中的照片没有余弦可给");
    }

    #[test]
    fn caption_search_matches_manual_text_without_wildcards() {
        let store = Store::open_mem().unwrap();
        add_photo(&store, 21);
        add_photo(&store, 22);
        store.set_caption(21, "身份证放在抽屉里").unwrap();
        store.set_caption(22, "海边日落").unwrap();
        let ids: Vec<i64> = caption_search(&store, "身份证", 10)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec![21]);
        assert!(caption_search(&store, "抽屉", 10)
            .iter()
            .any(|(id, _)| *id == 21));
        assert!(caption_search(&store, "%", 10).is_empty(), "百分号不是通配符");
        assert!(caption_search(&store, "  ", 10).is_empty(), "空词不查全库");
    }

    #[test]
    fn near_point_filters_by_distance_and_requires_gps() {
        let store = Store::open_mem().unwrap();
        for id in [31, 32, 33] {
            add_photo(&store, id);
        }
        store.write(|conn| {
            conn.execute("UPDATE photos SET lat=39.9, lon=116.4 WHERE id=31", [])
                .unwrap();
            conn.execute("UPDATE photos SET lat=39.91, lon=116.41 WHERE id=32", [])
                .unwrap();
        });
        let ids = [31, 32, 33];
        let near = ids_near_point(&store, &ids, 39.9, 116.4, 5.0);
        assert!(near.contains(&31));
        assert!(near.contains(&32), "约 1.4 公里应落在 5 公里内");
        assert!(!near.contains(&33), "无定位的照片不参与附近筛选");
        let tight = ids_near_point(&store, &ids, 39.9, 116.4, 0.1);
        assert!(tight.contains(&31));
        assert!(!tight.contains(&32));
        assert!(ids_near_point(&store, &[], 39.9, 116.4, 5.0).is_empty());
        assert!(photo_gps(&store, 31).is_some());
        assert_eq!(photo_gps(&store, 33), None);
    }

    #[test]
    fn date_window_filters_by_taken_and_falls_back_to_mtime() {
        use chrono::TimeZone;
        let store = Store::open_mem().unwrap();
        let put = |id: i64, taken: Option<&str>, mtime: i64| {
            store
                .write(|conn| {
                    conn.execute(
                        "INSERT INTO photos(id, path, parent, mtime, size, taken_utc)
                         VALUES(?1, ?2, 'C:\\', ?3, 0, ?4)",
                        params![id, format!("C:\\p{id}.jpg"), mtime, taken],
                    )
                })
                .unwrap();
        };
        let secs = chrono::Local
            .with_ymd_and_hms(2026, 11, 9, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        put(1, Some("2026-11-09T10:00:00"), 0);
        put(2, Some("2024-05-01T10:00:00"), 0);
        put(3, None, secs);
        put(4, None, secs * 1000);
        put(5, None, 0);
        let ids = [1, 2, 3, 4, 5];
        let got = ids_in_date_window(&store, &ids, "2026-11-09", "2026-11-10");
        assert!(got.contains(&1), "拍摄日期命中");
        assert!(!got.contains(&2), "窗口外的拍摄日期排除");
        assert!(got.contains(&3), "无拍摄时间时回落 mtime 秒");
        assert!(got.contains(&4), "mtime 毫秒口径也要归一");
        assert!(!got.contains(&5), "1970 年不应落进窗口");
        assert!(ids_in_date_window(&store, &ids, "2026-11-09", "2026-11-09").is_empty());
    }

    /// top-K 流式打分必须与「全量算分再排序」逐名一致,含淘汰与维度不符的行。
    #[test]
    fn vector_search_topk_matches_brute_force() {
        let store = Store::open_mem().unwrap();
        let mut vs: Vec<(i64, Vec<f32>)> = Vec::new();
        let mut seed = 12345u32;
        for id in 1..=40i64 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let a = (seed >> 8) as f32 / 16_777_216.0 * std::f32::consts::TAU;
            vs.push((id, vec![a.cos(), a.sin()]));
        }
        for (id, v) in &vs {
            add_photo(&store, *id);
            save_vec(&store, *id, "v1", v).unwrap();
        }
        add_photo(&store, 99);
        save_vec(&store, 99, "v1", &[1.0, 0.0, 0.0]).unwrap();
        let q = [0.6f32, 0.8];
        for k in [1usize, 3, 7, 40, 100] {
            let mut want: Vec<(i64, f32)> = vs
                .iter()
                .map(|(id, v)| (*id, crate::ai_tags::cosine(&q, v)))
                .collect();
            want.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.0.cmp(&b.0))
            });
            want.truncate(k);
            let got = vector_search(&store, "v1", &q, k);
            assert_eq!(
                got.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                want.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                "top-{k} 应与全量排序一致"
            );
            for (g, w) in got.iter().zip(&want) {
                assert!((g.1 - w.1).abs() < 1e-6, "分数应一致: {} vs {}", g.1, w.1);
            }
        }
        assert!(vector_search(&store, "v1", &q, 0).is_empty());
    }

    /// 同分时按 id 升序取舍,淘汰的必须是号大的那张。
    #[test]
    fn vector_search_ties_prefer_smaller_id() {
        let store = Store::open_mem().unwrap();
        for id in [5i64, 3, 9, 1] {
            add_photo(&store, id);
            save_vec(&store, id, "v1", &[1.0, 0.0]).unwrap();
        }
        let got = vector_search(&store, "v1", &[1.0, 0.0], 2);
        assert_eq!(got.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![1, 3]);
    }

    /// id 多到超过一块时,日期与地点过滤都不能整条查询失败或漏块。
    #[test]
    fn id_filters_survive_more_ids_than_one_chunk() {
        let store = Store::open_mem().unwrap();
        let n = (ID_CHUNK * 2 + 5) as i64;
        store.write(|conn| {
            let tx = conn.transaction().unwrap();
            for id in 1..=n {
                let hit = id % 3 == 0;
                let taken = if hit {
                    "2026-11-09T10:00:00"
                } else {
                    "2024-01-01T10:00:00"
                };
                let lat = if hit { 30.0 } else { 40.0 };
                tx.execute(
                    "INSERT INTO photos(id,path,parent,mtime,size,taken_utc,lat,lon)
                     VALUES(?1,?2,'C:\\',0,0,?3,?4,120.0)",
                    params![id, format!("C:\\p{id}.jpg"), taken, lat],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        });
        let ids: Vec<i64> = (1..=n).collect();
        let want: Vec<i64> = ids.iter().copied().filter(|id| id % 3 == 0).collect();
        let got = ids_in_date_window(&store, &ids, "2026-11-09", "2026-11-10");
        assert_eq!(got.len(), want.len(), "日期过滤不应漏块");
        let near = ids_near_point(&store, &ids, 30.0, 120.0, 1.0);
        assert_eq!(near.len(), want.len(), "地点过滤不应漏块");
        for id in want {
            assert!(got.contains(&id), "{id} 应落在日期窗口内");
            assert!(near.contains(&id), "{id} 应落在基准点附近");
        }
    }
}
