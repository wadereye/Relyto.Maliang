//! 检索:向量精确余弦、标签命中、OCR 全文,按 RRF 融合。

use crate::db::Store;
use anyhow::Result;
use rusqlite::params;

/// 向量落库(同版本覆盖,保留各版本以便回滚)。
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
    let sql = format!(
        "SELECT id, taken_utc, mtime FROM photos WHERE id IN ({})",
        vec!["?"; ids.len()].join(",")
    );
    store.read(|conn| {
        let Ok(mut st) = conn.prepare(&sql) else {
            return;
        };
        let Ok(rows) = st.query_map(rusqlite::params_from_iter(ids.iter().copied()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
            ))
        }) else {
            return;
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

/// 当前版本全部向量(标签阶段用)。
pub fn all_vecs(store: &Store, ver: &str) -> Vec<(i64, Vec<f32>)> {
    store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) = conn.prepare("SELECT photo_id, embedding FROM clips_vec WHERE ver=?1")
        else {
            return out;
        };
        if let Ok(rows) = st.query_map(params![ver], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        }) {
            for (id, b) in rows.flatten() {
                out.push((id, blob_to_f32(&b)));
            }
        }
        out
    })
}

/// 向量精确检索:逐条点积,返回 top limit。
pub fn vector_search(store: &Store, ver: &str, query: &[f32], limit: usize) -> Vec<(i64, f32)> {
    let mut scored: Vec<(i64, f32)> = all_vecs(store, ver)
        .into_iter()
        .filter(|(_, v)| v.len() == query.len())
        .map(|(id, v)| (id, crate::ai_tags::cosine(query, &v)))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
}

/// 标签命中:查询串对标签中英文做包含匹配(带简单权重)。
pub fn tag_search(store: &Store, ver: &str, query: &str, limit: usize) -> Vec<(i64, f32)> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) = conn.prepare(
            "SELECT t.photo_id, MAX(t.score) FROM auto_tags t JOIN tag_labels l ON l.id=t.label_id
              WHERE t.model_ver=?1 AND (instr(l.name_zh, ?2)>0 OR instr(lower(l.name_en), lower(?2))>0)
              GROUP BY t.photo_id ORDER BY MAX(t.score) DESC LIMIT ?3",
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

/// 混合检索:向量(有则用)+ 标签 + OCR,三路融合。
pub fn hybrid(
    store: &Store,
    ver: &str,
    tag_ver: &str,
    query: &str,
    emb: Option<&[f32]>,
    limit: usize,
) -> Vec<i64> {
    let mut lists: Vec<Vec<(i64, f32)>> = Vec::new();
    if let Some(e) = emb {
        lists.push(vector_search(store, ver, e, limit * 2));
    }
    lists.push(tag_search(store, tag_ver, query, limit * 2));
    lists.push(crate::ai_ocr::search(store, query, limit * 2));
    fuse(&lists, 60.0, limit)
        .into_iter()
        .map(|(id, _)| id)
        .collect()
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

    #[test]
    fn hybrid_merges_tag_and_ocr_hits_without_vector() {
        let store = Store::open_mem().unwrap();
        add_photo(&store, 11);
        add_photo(&store, 12);
        crate::ai_tags::seed_labels(&store).unwrap();
        crate::ai_tags::save(&store, 11, "v1", &[(2, 0.8)]).unwrap();
        crate::ai_ocr::save(&store, 12, "合影 发票", &[], "v1").unwrap();
        let hits = hybrid(&store, "v1", "v1", "合影", None, 10);
        assert!(hits.contains(&11), "标签命中应进入融合结果");
        assert!(hits.contains(&12), "图中文字命中应进入融合结果");
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
}
