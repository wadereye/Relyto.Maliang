//! 图片描述:把已有标签、图中文字、人物与元数据拼成可检索的说明。

use crate::db::Store;
use rusqlite::params;

/// 描述模板版本。
pub const TEMPLATE_VER: &str = "v1";

/// 拼一段描述;无可用信息时返回空串。
pub fn build(store: &Store, photo_id: i64, tag_ver: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let sep = if crate::i18n::lang() == crate::i18n::Lang::En {
        ", "
    } else {
        "、"
    };
    let labels: Vec<String> = crate::ai_tags::photo_tags(store, photo_id, tag_ver)
        .into_iter()
        .take(6)
        .map(|(_, _, name)| name.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if !labels.is_empty() {
        parts.push(labels.join(sep));
    }
    if let Some(text) = crate::ai_ocr::text_of(store, photo_id) {
        let flat: String = text
            .chars()
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .collect();
        let joined = flat.split_whitespace().collect::<Vec<_>>().join(" ");
        let short: String = joined.chars().take(60).collect();
        if !short.trim().is_empty() {
            parts.push(short);
        }
    }
    let mut names: Vec<String> = crate::faces::photo_faces(store, photo_id)
        .into_iter()
        .filter(|f| !f.ignored)
        .filter_map(|f| f.person)
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    names.sort();
    names.dedup();
    if !names.is_empty() {
        parts.push(names.join(sep));
    }
    // 没有识别结果时不拿纯元数据冒充描述。
    if parts.is_empty() {
        return String::new();
    }
    let meta: Option<(Option<String>, String, Option<f64>, Option<f64>)> = store.read(|conn| {
        conn.query_row(
            "SELECT p.taken_utc, COALESCE(sp.value, p.cam, ''), p.lat, p.lon
               FROM photos p
               LEFT JOIN string_pool sp ON sp.pool = ?1 AND sp.value_id = p.cam_id
              WHERE p.id = ?2",
            params![crate::timeline::CAM_POOL, photo_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .ok()
    });
    if let Some((taken, cam, lat, lon)) = meta {
        if let Some(t) = taken.as_deref().filter(|s| s.len() >= 10) {
            parts.push(t[..10].to_string());
        }
        if !cam.trim().is_empty() {
            parts.push(cam.trim().to_string());
        }
        if let (Some(a), Some(b)) = (lat, lon) {
            parts.push(format!("{a:.4},{b:.4}"));
        }
    }
    parts.join(" · ").chars().take(200).collect()
}

/// 当前标题是否由本模块生成。
fn is_auto(store: &Store, photo_id: i64) -> bool {
    store.read(|conn| {
        conn.query_row(
            "SELECT caption_auto FROM photos WHERE id=?1",
            params![photo_id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            != 0
    })
}

/// 生成或刷新描述;force=false 时不覆盖用户手填的标题。
pub fn generate(store: &Store, photo_id: i64, tag_ver: &str, force: bool) -> bool {
    let existing = store.get_caption(photo_id);
    let auto = is_auto(store, photo_id);
    if !force && !auto && !existing.trim().is_empty() {
        return false;
    }
    let text = build(store, photo_id, tag_ver);
    if text.is_empty() || (auto && text == existing) {
        return false;
    }
    store.write(|conn| {
        conn.execute(
            "UPDATE photos SET caption=?2, caption_auto=1 WHERE id=?1",
            params![photo_id, text],
        )
        .unwrap_or(0)
            > 0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo(store: &Store, id: i64) {
        store
            .write(|conn| {
                conn.execute(
                    "INSERT INTO photos(id,path,parent,mtime,size,taken_utc,cam,lat,lon)
                     VALUES(?1,?2,'C:\\',0,0,'2024-05-01T10:00:00','Canon',39.9,116.4)",
                    params![id, format!("C:\\p{id}.jpg")],
                )
            })
            .unwrap();
    }

    #[test]
    fn build_merges_tags_text_and_metadata() {
        let store = Store::open_mem().unwrap();
        photo(&store, 7);
        crate::ai_tags::seed_labels(&store).unwrap();
        crate::ai_tags::save(&store, 7, "v1", &[(2, 0.9)]).unwrap();
        crate::ai_ocr::save(&store, 7, "发票 号码 123", &[], "v1").unwrap();
        let text = build(&store, 7, "v1");
        assert!(text.contains("合影"), "应带标签名: {text}");
        assert!(text.contains("发票"), "应带图中文字: {text}");
        assert!(text.contains("2024-05-01"), "应带拍摄日期: {text}");
        assert!(text.contains("Canon"), "应带相机: {text}");
        assert!(text.contains("39.9000"), "应带定位: {text}");
    }

    #[test]
    fn generate_keeps_manual_caption_but_refreshes_auto() {
        let store = Store::open_mem().unwrap();
        photo(&store, 8);
        crate::ai_tags::seed_labels(&store).unwrap();
        crate::ai_tags::save(&store, 8, "v1", &[(2, 0.9)]).unwrap();
        assert!(generate(&store, 8, "v1", false));
        assert!(is_auto(&store, 8));
        let first = store.get_caption(8);
        assert!(!generate(&store, 8, "v1", false), "内容未变时不重写");
        crate::ai_ocr::save(&store, 8, "收据 456", &[], "v1").unwrap();
        assert!(generate(&store, 8, "v1", false), "OCR 完成后应刷新");
        assert!(store.get_caption(8).contains("收据"));
        store.set_caption(8, "我自己写的标题").unwrap();
        assert!(!is_auto(&store, 8));
        assert!(!generate(&store, 8, "v1", false), "不得覆盖手填标题");
        assert_eq!(store.get_caption(8), "我自己写的标题");
        assert!(generate(&store, 8, "v1", true), "显式重建应生效");
        assert_ne!(store.get_caption(8), first);
    }

    #[test]
    fn empty_photo_has_no_description() {
        let store = Store::open_mem().unwrap();
        photo(&store, 9);
        assert!(!generate(&store, 9, "v1", false));
        assert!(store.get_caption(9).is_empty());
    }
}
