//! OCR 落库与全文检索:分词、文档/行框写入、FTS 查询、触发策略。

use crate::db::Store;
use anyhow::Result;
use rusqlite::params;

/// 一行文字框(原图坐标)。
#[derive(Debug, Clone, PartialEq)]
pub struct LineBox {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
    pub conf: f32,
}

/// OCR 单行写入上限。
pub const MAX_BOXES: usize = 200;

/// 是否统一表意文字。
fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF)
}

/// 分词:中文单字 + 相邻双字,英文/数字整词小写,空白分隔。
///
/// 双字让「发票」「海边」这类两个字的查询可命中,单字保证短查询可用;
/// 交给 FTS5 的都是空白分隔的原子 token,查询侧用同样的规则。
pub fn seg_text(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut toks: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut cjk_run: Vec<char> = Vec::new();
    let flush_word = |word: &mut String, toks: &mut Vec<String>| {
        if !word.is_empty() {
            toks.push(word.to_lowercase());
            word.clear();
        }
    };
    let flush_cjk = |run: &mut Vec<char>, toks: &mut Vec<String>| {
        for i in 0..run.len() {
            toks.push(run[i].to_string());
            if i + 1 < run.len() {
                toks.push(format!("{}{}", run[i], run[i + 1]));
            }
        }
        run.clear();
    };
    for &c in &chars {
        if is_cjk(c) {
            flush_word(&mut word, &mut toks);
            cjk_run.push(c);
        } else if c.is_alphanumeric() {
            flush_cjk(&mut cjk_run, &mut toks);
            word.push(c);
        } else {
            flush_word(&mut word, &mut toks);
            flush_cjk(&mut cjk_run, &mut toks);
        }
    }
    flush_word(&mut word, &mut toks);
    flush_cjk(&mut cjk_run, &mut toks);
    toks.join(" ")
}

/// 写一张照片的 OCR 结果;清旧行,行框仅保留前 200。
pub fn save(
    store: &Store,
    photo_id: i64,
    text: &str,
    boxes: &[LineBox],
    ver: &str,
) -> Result<(), String> {
    let seg = seg_text(text);
    let lines = boxes.len();
    let chars = text.chars().count();
    let ts = crate::ai_queue::now_ms();
    store
        .write(|conn| -> Result<(), String> {
            let tx = conn.transaction().map_err(|e| e.to_string())?;
            let old: Option<String> = tx
                .query_row(
                    "SELECT seg FROM ocr_docs WHERE photo_id=?1",
                    params![photo_id],
                    |r| r.get(0),
                )
                .ok();
            if let Some(old) = old {
                tx.execute(
                    "INSERT INTO ocr_fts(ocr_fts, rowid, seg) VALUES('delete', ?1, ?2)",
                    params![photo_id, old],
                )
                .map_err(|e| e.to_string())?;
            }
            tx.execute("DELETE FROM ocr_boxes WHERE photo_id=?1", params![photo_id])
                .map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT OR REPLACE INTO ocr_docs(photo_id,text,seg,line_count,char_count,model_ver,done_ts)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![photo_id, text, seg, lines as i64, chars as i64, ver, ts],
            )
            .map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT INTO ocr_fts(rowid, seg) VALUES(?1, ?2)",
                params![photo_id, seg],
            )
            .map_err(|e| e.to_string())?;
            {
                let mut st = tx
                    .prepare("INSERT OR REPLACE INTO ocr_boxes(photo_id,line_no,x0,y0,x1,y1,conf) VALUES(?1,?2,?3,?4,?5,?6,?7)")
                    .map_err(|e| e.to_string())?;
                for (i, b) in boxes.iter().take(MAX_BOXES).enumerate() {
                    st.execute(params![photo_id, i as i64, b.x0, b.y0, b.x1, b.y1, b.conf as f64])
                        .map_err(|e| e.to_string())?;
                }
            }
            tx.commit().map_err(|e| e.to_string())?;
            Ok(())
        })
}

/// 转义 FTS5 短语。
fn quote_token(t: &str) -> String {
    format!("\"{}\"", t.replace('"', "\"\""))
}

/// FTS 查询:按 token OR 召回,返回 (photo_id, 归一化相关度)。
pub fn search(store: &Store, query: &str, limit: usize) -> Vec<(i64, f32)> {
    let toks: Vec<String> = seg_text(query)
        .split_whitespace()
        .map(quote_token)
        .collect();
    if toks.is_empty() {
        return Vec::new();
    }
    let expr = toks.join(" OR ");
    store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) = conn.prepare(
            "SELECT f.rowid, bm25(ocr_fts) FROM ocr_fts f
               JOIN ocr_docs d ON d.photo_id=f.rowid
              WHERE ocr_fts MATCH ?1 ORDER BY bm25(ocr_fts) LIMIT ?2",
        ) else {
            return out;
        };
        if let Ok(rows) = st.query_map(params![expr, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
        }) {
            for (id, rank) in rows.flatten() {
                out.push((id, 1.0 / (1.0 + rank.max(0.0) as f32)));
            }
        }
        out
    })
}

/// 已识别文档数。
pub fn count(store: &Store) -> usize {
    store.read(|conn| {
        conn.query_row("SELECT COUNT(*) FROM ocr_docs", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as usize
    })
}

/// OCR 原文。
pub fn text_of(store: &Store, photo_id: i64) -> Option<String> {
    store.read(|conn| {
        conn.query_row(
            "SELECT text FROM ocr_docs WHERE photo_id=?1",
            params![photo_id],
            |r| r.get(0),
        )
        .ok()
    })
}

/// 行框。
pub fn boxes_of(store: &Store, photo_id: i64) -> Vec<LineBox> {
    store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) = conn
            .prepare("SELECT x0,y0,x1,y1,conf FROM ocr_boxes WHERE photo_id=?1 ORDER BY line_no")
        else {
            return out;
        };
        if let Ok(rows) = st.query_map(params![photo_id], |r| {
            Ok(LineBox {
                x0: r.get(0)?,
                y0: r.get(1)?,
                x1: r.get(2)?,
                y1: r.get(3)?,
                conf: r.get::<_, f64>(4)? as f32,
            })
        }) {
            out.extend(rows.flatten());
        }
        out
    })
}

/// OCR 触发模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Auto,
    Selected,
}

impl Mode {
    /// 设置键。
    pub const KEY: &'static str = "ai_ocr_mode";

    /// 从设置值解析(缺省自动)。
    pub fn from_setting(v: Option<String>) -> Mode {
        match v.as_deref() {
            Some("off") => Mode::Off,
            Some("selected") => Mode::Selected,
            _ => Mode::Auto,
        }
    }

    /// 持久化值。
    pub fn key(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Auto => "auto",
            Mode::Selected => "selected",
        }
    }

    /// 界面文案。
    pub fn label(self) -> &'static str {
        match self {
            Mode::Off => crate::i18n::t("停用"),
            Mode::Auto => crate::i18n::t("自动(文字类照片)"),
            Mode::Selected => crate::i18n::t("仅选中/手动"),
        }
    }
}

/// 读当前触发模式。
pub fn mode(store: &Store) -> Mode {
    Mode::from_setting(store.get_setting(Mode::KEY))
}

/// 自动模式下是否应识别:标签判定为文字类。
pub fn should_auto(store: &Store, photo_id: i64, tag_ver: &str) -> bool {
    mode(store) == Mode::Auto && crate::ai_tags::is_text_photo(store, photo_id, tag_ver)
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
    fn segmentation_covers_short_chinese_queries() {
        let seg = seg_text("发票 2024-05 金额");
        assert!(
            seg.split_whitespace().any(|t| t == "发票"),
            "双字词应成 token: {seg}"
        );
        assert!(
            seg.split_whitespace().any(|t| t == "发"),
            "单字也应成 token: {seg}"
        );
        assert!(
            seg.split_whitespace().any(|t| t == "2024"),
            "数字词应保留: {seg}"
        );
    }

    #[test]
    fn save_and_search_roundtrip() {
        let store = Store::open_mem().unwrap();
        add_photo(&store, 7);
        let b = vec![LineBox {
            x0: 1,
            y0: 2,
            x1: 3,
            y1: 4,
            conf: 0.9,
        }];
        save(&store, 7, "早餐发票 金额", &b, "v1").unwrap();
        let hits = search(&store, "发票", 10);
        assert!(hits.iter().any(|(id, _)| *id == 7), "应命中 7: {hits:?}");
        assert!(search(&store, "早餐", 10).iter().any(|(id, _)| *id == 7));
        assert!(search(&store, "火锅", 10).is_empty());
        assert_eq!(text_of(&store, 7).as_deref(), Some("早餐发票 金额"));
        assert_eq!(boxes_of(&store, 7).len(), 1);
        // 重写后旧索引应被删掉。
        save(&store, 7, "火锅", &[], "v1").unwrap();
        assert!(search(&store, "发票", 10).is_empty());
        assert!(search(&store, "火锅", 10).iter().any(|(id, _)| *id == 7));
    }

    #[test]
    fn boxes_are_capped() {
        let store = Store::open_mem().unwrap();
        add_photo(&store, 9);
        let b: Vec<LineBox> = (0..(MAX_BOXES + 50))
            .map(|i| LineBox {
                x0: i as u32,
                y0: 0,
                x1: i as u32 + 1,
                y1: 1,
                conf: 1.0,
            })
            .collect();
        save(&store, 9, "测试", &b, "v1").unwrap();
        assert_eq!(boxes_of(&store, 9).len(), MAX_BOXES);
    }

    #[test]
    fn mode_parsing_roundtrip() {
        assert_eq!(Mode::from_setting(None), Mode::Auto);
        assert_eq!(Mode::from_setting(Some("off".into())), Mode::Off);
        assert_eq!(Mode::from_setting(Some("selected".into())), Mode::Selected);
        assert_eq!(Mode::Auto.key(), "auto");
    }
}
