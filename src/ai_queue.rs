//! AI 任务队列:入队、取批、完成台账与失败隔离,只做数据库逻辑。

use crate::db::Store;
use anyhow::Result;
use rusqlite::{params, Connection};
use std::time::{SystemTime, UNIX_EPOCH};

/// AI 管线的三个阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    Clip,
    Tags,
    Ocr,
}

impl Stage {
    pub const ALL: [Stage; 3] = [Stage::Clip, Stage::Tags, Stage::Ocr];

    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Clip => "clip",
            Stage::Tags => "tags",
            Stage::Ocr => "ocr",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Stage::Clip => crate::i18n::t("语义向量"),
            Stage::Tags => crate::i18n::t("自动标签"),
            Stage::Ocr => crate::i18n::t("文字识别"),
        }
    }
}

/// 当前正在查看的照片:交互式提级到最高优先。
pub const P_CURRENT: i32 = 0;
/// 没有命中任何目录规则的默认优先级。
pub const P_DEFAULT: i32 = 6;
/// 连续失败达到该次数即移入失败隔离表。
pub const FAIL_THRESHOLD: i32 = 3;
/// 单个 worker 每批最多取走的条数。
pub const BATCH_LIMIT: usize = 16;
/// 依赖暂不满足的任务延后多久再试,避免每轮空转刷批。
pub const SKIP_DELAY_MS: i64 = 30_000;

/// 当前 Unix 毫秒时间戳。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 路径归一化:统一分隔符并折叠大小写,便于前缀匹配。
fn norm(path: &str) -> String {
    path.replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

/// 从连接读取优先级规则(目录前缀, 档位)。
pub fn rules_from_conn(conn: &Connection) -> Vec<(String, i32)> {
    try_rules_from_conn(conn).unwrap_or_default()
}

/// 读取目录优先级规则并传播数据库错误。
fn try_rules_from_conn(conn: &Connection) -> Result<Vec<(String, i32)>> {
    let mut st = conn.prepare("SELECT prefix, prio FROM ai_priority_rules")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// 按最长目录前缀取优先级,未命中返回默认档。
pub fn priority_with_rules(rules: &[(String, i32)], path: &str) -> i32 {
    let p = norm(path);
    let mut best: Option<(usize, i32)> = None;
    for (prefix, prio) in rules {
        let hit = p.len() == prefix.len() && p.starts_with(prefix.as_str())
            || p.len() > prefix.len()
                && p.starts_with(prefix.as_str())
                && p.as_bytes()[prefix.len()] == b'\\';
        if hit && best.map_or(true, |(len, _)| prefix.len() > len) {
            best = Some((prefix.len(), *prio));
        }
    }
    best.map(|(_, prio)| prio).unwrap_or(P_DEFAULT)
}

/// 写入默认优先级规则:壳目录与已导入目录。
pub fn seed_priority_rules(store: &Store) -> Result<()> {
    let mut defaults: Vec<(String, i32)> = Vec::new();
    for (name, path) in crate::win::common_places() {
        let prio = match name {
            "我的文档" => 1,
            "我的图片" => 2,
            "我的视频" => 3,
            "桌面" => 4,
            _ => continue,
        };
        if let Some(p) = path {
            defaults.push((norm(&p.to_string_lossy()), prio));
        }
    }
    store.write(|conn| -> Result<()> {
        let tx = conn.transaction()?;
        {
            let mut st =
                tx.prepare("INSERT OR IGNORE INTO ai_priority_rules(prefix, prio) VALUES(?1, ?2)")?;
            for (prefix, prio) in &defaults {
                st.execute(params![prefix, prio])?;
            }
            let imported: Vec<String> = {
                let mut q = tx.prepare("SELECT path FROM dir_intent WHERE state='imported'")?;
                let rows = q.query_map([], |r| r.get::<_, String>(0))?;
                rows.flatten().collect()
            };
            for path in imported {
                st.execute(params![norm(&path), 5])?;
            }
        }
        tx.commit()?;
        Ok(())
    })
}

/// 单条入队;重复入队不改已有行的优先级。
pub fn enqueue(store: &Store, stage: Stage, photo_id: i64, priority: i32) -> Result<()> {
    store.write(|conn| -> Result<()> {
        conn.execute(
            "INSERT OR IGNORE INTO ai_jobs(stage, photo_id, priority, enqueued_ts, attempts)
             VALUES(?1, ?2, ?3, ?4, 0)",
            params![stage.as_str(), photo_id, priority, now_ms()],
        )?;
        Ok(())
    })
}

/// 把照片提级到当前查看,三个阶段各入队一次。
pub fn enqueue_current(store: &Store, photo_id: i64) {
    for stage in Stage::ALL {
        let _ = enqueue(store, stage, photo_id, P_CURRENT);
    }
    // 被延后的任务重新可见,打开预览时不用等下一轮重试。
    let _ = store.write(|conn| -> Result<()> {
        conn.execute(
            "UPDATE ai_jobs SET enqueued_ts=?1
              WHERE photo_id=?2 AND enqueued_ts > ?1",
            params![now_ms(), photo_id],
        )?;
        Ok(())
    });
}

/// 按 id 批量入队某阶段,优先级按目录规则现算。
pub fn enqueue_for_ids(store: &Store, stage: Stage, ids: &[i64]) {
    if ids.is_empty() {
        return;
    }
    let ids = ids.to_vec();
    store.write_async(move |conn| {
        let rules = rules_from_conn(conn);
        let Ok(tx) = conn.transaction() else {
            return;
        };
        let Ok(mut st) = tx.prepare(
            "INSERT OR IGNORE INTO ai_jobs(stage, photo_id, priority, enqueued_ts, attempts)
             VALUES(?1, ?2, ?3, ?4, 0)",
        ) else {
            return;
        };
        let ts = now_ms();
        for id in ids {
            let path: Option<String> = tx
                .query_row("SELECT path FROM photos WHERE id=?1", [id], |r| r.get(0))
                .ok();
            let Some(path) = path else {
                continue;
            };
            let prio = priority_with_rules(&rules, &path);
            let _ = st.execute(params![stage.as_str(), id, prio, ts]);
        }
        drop(st);
        let _ = tx.commit();
    });
}

/// 全库入队所有阶段;已在本版完成或已隔离的照片跳过。
pub fn enqueue_all(store: &Store) -> Result<usize> {
    let rules = store.read(rules_from_conn);
    let versions = Stage::ALL.map(|stage| stage_version(store, stage));
    let tag_ver = stage_version(store, Stage::Tags);
    let ocr_auto = crate::ai_ocr::mode(store) == crate::ai_ocr::Mode::Auto;
    let mut last = 0i64;
    let mut total = 0usize;
    loop {
        let rows: Vec<(i64, String)> = store.read(|conn| {
            let mut out = Vec::new();
            let Ok(mut st) =
                conn.prepare("SELECT id, path FROM photos WHERE id > ?1 ORDER BY id LIMIT 2000")
            else {
                return out;
            };
            if let Ok(iter) = st.query_map([last], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            }) {
                for row in iter.flatten() {
                    out.push(row);
                }
            }
            out
        });
        let Some(tail) = rows.last() else {
            break;
        };
        last = tail.0;
        // OCR 只排标签已判定的文字照片:提前空跑记完成会让它此后永远不再识别。
        let text_ids = if ocr_auto {
            let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
            crate::ai_tags::text_photo_ids(store, &tag_ver, &ids)
        } else {
            std::collections::HashSet::new()
        };
        let ts = now_ms();
        total += store.write(|conn| -> Result<usize> {
            let tx = conn.transaction()?;
            let mut n = 0usize;
            {
                let mut st = tx.prepare(
                    "INSERT OR IGNORE INTO ai_jobs(stage, photo_id, priority, enqueued_ts, attempts)
                     SELECT ?1, ?2, ?3, ?4, 0
                      WHERE NOT EXISTS(
                          SELECT 1 FROM ai_state s
                           WHERE s.photo_id = ?2 AND s.stage = ?1 AND s.model_ver = ?5
                      )
                        AND NOT EXISTS(
                          SELECT 1 FROM ai_failed f
                           WHERE f.photo_id = ?2 AND f.stage = ?1
                      )",
                )?;
                for (id, path) in &rows {
                    let prio = priority_with_rules(&rules, path);
                    for (i, stage) in Stage::ALL.iter().enumerate() {
                        if *stage == Stage::Ocr && !text_ids.contains(id) {
                            continue;
                        }
                        n += st.execute(params![stage.as_str(), id, prio, ts, versions[i]])?;
                    }
                }
            }
            tx.commit()?;
            Ok(n)
        })?;
    }
    Ok(total)
}

/// 队列里的一条待办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Job {
    pub photo_id: i64,
    pub priority: i32,
    /// 入队后已失败的次数(0 = 尚未失败)。
    pub attempts: i32,
}

/// 取一批待办,按优先级与入队时间排序,跳过当前版本已完成的行。
pub fn pick_batch(store: &Store, stage: Stage, model_ver: &str, limit: usize) -> Vec<Job> {
    store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) = conn.prepare(
            "SELECT j.photo_id, j.priority, j.attempts
               FROM ai_jobs j
               LEFT JOIN ai_state s
                 ON s.photo_id = j.photo_id AND s.stage = j.stage
              WHERE j.stage = ?1 AND (s.model_ver IS NULL OR s.model_ver <> ?2)
                AND j.enqueued_ts <= ?4
              ORDER BY j.priority, j.enqueued_ts, j.photo_id
              LIMIT ?3",
        ) else {
            return out;
        };
        if let Ok(rows) = st.query_map(
            params![stage.as_str(), model_ver, limit as i64, now_ms()],
            |r| {
            Ok(Job {
                photo_id: r.get(0)?,
                priority: r.get(1)?,
                attempts: r.get(2)?,
            })
        }) {
            for row in rows.flatten() {
                out.push(row);
            }
        }
        out
    })
}

/// 提交一批结果:成功写台账并出队,失败计次或移入隔离表。
pub fn commit_batch(
    store: &Store,
    stage: Stage,
    model_ver: &str,
    ok: &[i64],
    failed: &[(i64, i32, String)],
) -> Result<()> {
    if ok.is_empty() && failed.is_empty() {
        return Ok(());
    }
    let ts = now_ms();
    store.write(|conn| -> Result<()> {
        let tx = conn.transaction()?;
        {
            let mut state = tx.prepare(
                "INSERT INTO ai_state(photo_id, stage, model_ver, done_ts)
                 VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(photo_id, stage)
                 DO UPDATE SET model_ver=excluded.model_ver, done_ts=excluded.done_ts",
            )?;
            let mut del = tx.prepare("DELETE FROM ai_jobs WHERE stage=?1 AND photo_id=?2")?;
            for id in ok {
                state.execute(params![id, stage.as_str(), model_ver, ts])?;
                del.execute(params![stage.as_str(), id])?;
            }
            let mut bump =
                tx.prepare("UPDATE ai_jobs SET attempts=?3 WHERE stage=?1 AND photo_id=?2")?;
            let mut isolate = tx.prepare(
                "INSERT INTO ai_failed(photo_id, stage, err, attempts, ts)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(photo_id, stage)
                 DO UPDATE SET err=excluded.err, attempts=excluded.attempts, ts=excluded.ts",
            )?;
            for (id, attempts, err) in failed {
                if *attempts >= FAIL_THRESHOLD {
                    isolate.execute(params![id, stage.as_str(), err, attempts, ts])?;
                    del.execute(params![stage.as_str(), id])?;
                } else {
                    bump.execute(params![stage.as_str(), id, attempts])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    })
}

/// 把依赖暂不满足的任务延后,不在队列里反复空转。
pub fn defer_batch(store: &Store, stage: Stage, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let until = now_ms() + SKIP_DELAY_MS;
    store.write(|conn| -> Result<()> {
        let tx = conn.transaction()?;
        {
            let mut st = tx.prepare(
                "UPDATE ai_jobs SET enqueued_ts=?3
                  WHERE stage=?1 AND photo_id=?2",
            )?;
            for id in ids {
                st.execute(params![stage.as_str(), id, until])?;
            }
        }
        tx.commit()?;
        Ok(())
    })
}

/// 删除本版已完成却仍留在队列里的行(版本切换后的残留)。
pub fn purge_done(store: &Store, stage: Stage, model_ver: &str) -> Result<()> {
    store.write(|conn| -> Result<()> {
        conn.execute(
            "DELETE FROM ai_jobs
              WHERE stage = ?1
                AND EXISTS(
                    SELECT 1 FROM ai_state s
                     WHERE s.photo_id = ai_jobs.photo_id
                       AND s.stage = ai_jobs.stage
                       AND s.model_ver = ?2
                )",
            params![stage.as_str(), model_ver],
        )?;
        Ok(())
    })
}

/// 单阶段全库入队(换模型后重建用)。
pub fn enqueue_all_for(store: &Store, stage: Stage) -> Result<usize> {
    let rules = store.read(rules_from_conn);
    let ver = stage_version(store, stage);
    let tag_ver = stage_version(store, Stage::Tags);
    let ocr_auto = crate::ai_ocr::mode(store) == crate::ai_ocr::Mode::Auto;
    if stage == Stage::Ocr && !ocr_auto {
        return Ok(0);
    }
    let mut last = 0i64;
    let mut total = 0usize;
    loop {
        let rows: Vec<(i64, String)> = store.read(|conn| {
            let mut out = Vec::new();
            let Ok(mut st) =
                conn.prepare("SELECT id, path FROM photos WHERE id > ?1 ORDER BY id LIMIT 2000")
            else {
                return out;
            };
            if let Ok(iter) = st.query_map([last], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            }) {
                for row in iter.flatten() {
                    out.push(row);
                }
            }
            out
        });
        let Some(tail) = rows.last() else {
            break;
        };
        last = tail.0;
        let text_ids = if stage == Stage::Ocr {
            let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
            crate::ai_tags::text_photo_ids(store, &tag_ver, &ids)
        } else {
            std::collections::HashSet::new()
        };
        let ts = now_ms();
        total += store.write(|conn| -> Result<usize> {
            let tx = conn.transaction()?;
            let mut n = 0usize;
            {
                let mut st = tx.prepare(
                    "INSERT OR IGNORE INTO ai_jobs(stage, photo_id, priority, enqueued_ts, attempts)
                     SELECT ?1, ?2, ?3, ?4, 0
                      WHERE NOT EXISTS(
                          SELECT 1 FROM ai_state s
                           WHERE s.photo_id = ?2 AND s.stage = ?1 AND s.model_ver = ?5
                      )
                        AND NOT EXISTS(
                          SELECT 1 FROM ai_failed f
                           WHERE f.photo_id = ?2 AND f.stage = ?1
                      )",
                )?;
                for (id, path) in &rows {
                    if stage == Stage::Ocr && !text_ids.contains(id) {
                        continue;
                    }
                    let prio = priority_with_rules(&rules, path);
                    n += st.execute(params![stage.as_str(), id, prio, ts, ver])?;
                }
            }
            tx.commit()?;
            Ok(n)
        })?;
    }
    Ok(total)
}

/// 继续处理:只补当前版本缺失/过期的任务,保留有效完成项。
pub fn continue_stage(store: &Store, stage: Stage) -> Result<usize> {
    enqueue_all_for(store, stage)
}

/// 重试失败:把该阶段隔离的失败项放回队列并清零重试次数。
pub fn retry_failed(store: &Store, stage: Stage) -> Result<usize> {
    let ts = now_ms();
    store.write(|conn| -> Result<usize> {
        let tx = conn.transaction()?;
        let n = {
            let mut st = tx.prepare(
                "INSERT INTO ai_jobs(stage, photo_id, priority, enqueued_ts, attempts)
                 SELECT ?1, f.photo_id, ?2, ?3, 0
                   FROM ai_failed f JOIN photos p ON p.id = f.photo_id
                  WHERE f.stage = ?1
                 ON CONFLICT(stage, photo_id)
                 DO UPDATE SET attempts=0, enqueued_ts=excluded.enqueued_ts",
            )?;
            st.execute(params![stage.as_str(), P_DEFAULT, ts])?
        };
        tx.execute("DELETE FROM ai_failed WHERE stage=?1", [stage.as_str()])?;
        tx.commit()?;
        Ok(n)
    })
}

/// 强制重建:清除本阶段的完成/失败状态与派生产物后重新入队,人工标注保留。
pub fn rebuild_stage(store: &Store, stage: Stage) -> Result<usize> {
    let clip_ver = stage_version(store, Stage::Clip);
    let tags_ver = stage_version(store, Stage::Tags);
    store.write(|conn| -> Result<()> {
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM ai_state WHERE stage=?1", [stage.as_str()])?;
        tx.execute("DELETE FROM ai_jobs WHERE stage=?1", [stage.as_str()])?;
        tx.execute("DELETE FROM ai_failed WHERE stage=?1", [stage.as_str()])?;
        match stage {
            Stage::Clip => {
                tx.execute("DELETE FROM clips_vec WHERE ver=?1", [&clip_ver])?;
                // 语义向量重建后,由它派生的自动标签与标题一起失效。
                tx.execute("DELETE FROM ai_state WHERE stage='tags'", [])?;
                tx.execute("DELETE FROM auto_tags WHERE model_ver=?1", [&tags_ver])?;
                tx.execute(
                    "UPDATE photos SET caption=NULL, caption_auto=0 WHERE caption_auto=1",
                    [],
                )?;
            }
            Stage::Tags => {
                tx.execute("DELETE FROM auto_tags WHERE model_ver=?1", [&tags_ver])?;
                tx.execute(
                    "UPDATE photos SET caption=NULL, caption_auto=0 WHERE caption_auto=1",
                    [],
                )?;
            }
            Stage::Ocr => {
                tx.execute("INSERT INTO ocr_fts(ocr_fts) VALUES('delete-all')", [])?;
                tx.execute("DELETE FROM ocr_docs", [])?;
                tx.execute("DELETE FROM ocr_boxes", [])?;
            }
        }
        tx.commit()?;
        Ok(())
    })?;
    enqueue_all_for(store, stage)
}

/// 该阶段当前生效的模型版本;未配置时用基准版本。
pub fn stage_version(store: &Store, stage: Stage) -> String {
    crate::ai_models::stage_signature(store, stage)
        .or_else(|| store.get_setting(&format!("ai_active:{}", stage.as_str())))
        .unwrap_or_else(|| "v0".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Store {
        Store::open_mem().unwrap()
    }

    fn add_photo(store: &Store, id: i64, path: &str) {
        store
            .write(|conn| {
                conn.execute(
                    "INSERT INTO photos(id, path, parent, mtime, size) VALUES(?1, ?2, ?3, 0, 0)",
                    params![id, path, path],
                )
            })
            .unwrap();
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.read(|conn| conn.query_row(sql, [], |r| r.get(0)).unwrap())
    }

    #[test]
    fn duplicate_enqueue_keeps_one_job() {
        let store = mem();
        add_photo(&store, 1, r"C:\docs\a.jpg");
        enqueue(&store, Stage::Clip, 1, 6).unwrap();
        enqueue(&store, Stage::Clip, 1, P_CURRENT).unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_jobs"), 1);
        let prio = store.read(|conn| {
            conn.query_row("SELECT priority FROM ai_jobs", [], |r| r.get::<_, i32>(0))
                .unwrap()
        });
        assert_eq!(prio, 6, "重复入队不应改写已有优先级");
    }

    #[test]
    fn priority_uses_longest_prefix() {
        let rules = vec![
            (norm(r"C:\Users\me\Documents"), 1),
            (norm(r"C:\Users\me\Documents\Pictures"), 2),
        ];
        assert_eq!(
            priority_with_rules(&rules, r"C:\Users\me\Documents\Pictures\a.jpg"),
            2
        );
        assert_eq!(
            priority_with_rules(&rules, r"C:\Users\me\Documents\b.jpg"),
            1
        );
        assert_eq!(
            priority_with_rules(&rules, r"C:\Users\me\DocumentsOld\c.jpg"),
            P_DEFAULT
        );
    }

    #[test]
    fn pick_batch_prefers_priority_zero() {
        let store = mem();
        add_photo(&store, 1, r"C:\other\a.jpg");
        add_photo(&store, 2, r"C:\other\b.jpg");
        enqueue(&store, Stage::Clip, 1, 6).unwrap();
        enqueue(&store, Stage::Clip, 2, P_CURRENT).unwrap();
        let batch = pick_batch(&store, Stage::Clip, "v0", BATCH_LIMIT);
        assert_eq!(batch.first().map(|j| j.photo_id), Some(2));
        assert!(batch.len() <= BATCH_LIMIT);
    }

    #[test]
    fn complete_writes_state_and_removes_job() {
        let store = mem();
        add_photo(&store, 1, r"C:\docs\a.jpg");
        enqueue(&store, Stage::Ocr, 1, 1).unwrap();
        commit_batch(&store, Stage::Ocr, "v0", &[1], &[]).unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_jobs"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_state"), 1);
        // 同版本再次入队会被取批逻辑跳过(台账已存在)。
        enqueue(&store, Stage::Ocr, 1, 1).unwrap();
        assert!(pick_batch(&store, Stage::Ocr, "v0", BATCH_LIMIT).is_empty());
        assert!(!pick_batch(&store, Stage::Ocr, "v1", BATCH_LIMIT).is_empty());
    }

    #[test]
    fn failures_reach_threshold_then_isolate() {
        let store = mem();
        add_photo(&store, 1, r"C:\docs\bad.jpg");
        enqueue(&store, Stage::Clip, 1, 1).unwrap();
        commit_batch(&store, Stage::Clip, "v0", &[], &[(1, 1, "decode".into())]).unwrap();
        commit_batch(&store, Stage::Clip, "v0", &[], &[(1, 2, "decode".into())]).unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_jobs"), 1);
        commit_batch(&store, Stage::Clip, "v0", &[], &[(1, 3, "decode".into())]).unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_jobs"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_failed"), 1);
    }

    #[test]
    fn enqueue_all_covers_every_photo_for_clip_and_tags() {
        let store = mem();
        add_photo(&store, 1, r"C:\a\1.jpg");
        add_photo(&store, 2, r"C:\a\2.jpg");
        let n = enqueue_all(&store).unwrap();
        assert_eq!(n, 4);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_jobs"), 4);
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM ai_jobs WHERE stage='ocr'"),
            0,
            "标签还没判定文字类,OCR 不该入队"
        );
    }

    #[test]
    fn ocr_queues_only_text_photos() {
        let store = mem();
        add_photo(&store, 1, r"C:\a\1.jpg");
        add_photo(&store, 2, r"C:\a\2.jpg");
        crate::ai_tags::seed_labels(&store).unwrap();
        let ver = stage_version(&store, Stage::Tags);
        let lid: i64 = store.read(|c| {
            c.query_row(
                "SELECT id FROM tag_labels WHERE node='文档票据/截图'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        store
            .write(|c| {
                c.execute(
                    "INSERT INTO auto_tags(photo_id,label_id,score,model_ver) VALUES(2,?1,0.9,?2)",
                    params![lid, ver],
                )
            })
            .unwrap();
        assert_eq!(enqueue_all_for(&store, Stage::Ocr).unwrap(), 1);
        let ids: Vec<i64> = store.read(|c| {
            let mut st = c
                .prepare("SELECT photo_id FROM ai_jobs WHERE stage='ocr'")
                .unwrap();
            st.query_map([], |r| r.get(0)).unwrap().flatten().collect()
        });
        assert_eq!(ids, vec![2], "只有文字类照片该排 OCR");
        assert_eq!(
            enqueue_all_for(&store, Stage::Ocr).unwrap(),
            0,
            "重复入队不该多排"
        );
    }

    #[test]
    fn ocr_not_queued_when_automatic_disabled() {
        for mode in ["off", "selected"] {
            let store = mem();
            store.set_setting(crate::ai_ocr::Mode::KEY, mode).unwrap();
            add_photo(&store, 1, r"C:\a\1.jpg");
            assert_eq!(enqueue_all(&store).unwrap(), 2, "{mode} 模式只排两阶段");
            assert_eq!(enqueue_all_for(&store, Stage::Ocr).unwrap(), 0);
            assert_eq!(
                count(&store, "SELECT COUNT(*) FROM ai_jobs WHERE stage='ocr'"),
                0,
                "{mode} 模式不该留下永远跑不掉的 OCR 待办"
            );
        }
    }

    #[test]
    fn enqueue_all_skips_done_and_failed() {
        let store = mem();
        add_photo(&store, 1, r"C:\a\1.jpg");
        add_photo(&store, 2, r"C:\a\2.jpg");
        commit_batch(&store, Stage::Clip, "v0", &[1], &[]).unwrap();
        enqueue(&store, Stage::Ocr, 2, 1).unwrap();
        commit_batch(&store, Stage::Ocr, "v0", &[], &[(2, 3, "decode".into())]).unwrap();
        enqueue_all(&store).unwrap();
        let clip1 = count(
            &store,
            "SELECT COUNT(*) FROM ai_jobs WHERE stage='clip' AND photo_id=1",
        );
        let ocr2 = count(
            &store,
            "SELECT COUNT(*) FROM ai_jobs WHERE stage='ocr' AND photo_id=2",
        );
        assert_eq!(clip1, 0);
        assert_eq!(ocr2, 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_jobs"), 3);
    }

    /// 性能基准:50 万合成行的造数、入队与取批耗时(默认忽略,手动加 `--ignored` 跑)。
    #[test]
    #[ignore = "性能基准,手动运行"]
    fn perf_500k_enqueue_and_pick() {
        const N: i64 = 500_000;
        let store = mem();
        let t0 = std::time::Instant::now();
        let mut start = 0i64;
        while start < N {
            let end = (start + 5_000).min(N);
            store
                .write(|conn| -> Result<()> {
                    let tx = conn.transaction()?;
                    {
                        let mut st = tx.prepare(
                            "INSERT INTO photos(id, path, parent, mtime, size) VALUES(?1, ?2, ?3, 0, 0)",
                        )?;
                        for id in start + 1..=end {
                            let path = format!("C:\\lib\\{id}.jpg");
                            st.execute(params![id, path, "C:\\lib"])?;
                        }
                    }
                    tx.commit()?;
                    Ok(())
                })
                .unwrap();
            start = end;
        }
        let t_ins = t0.elapsed();
        let t1 = std::time::Instant::now();
        let n = enqueue_all_for(&store, Stage::Clip).unwrap();
        let t_enq = t1.elapsed();
        let t2 = std::time::Instant::now();
        let ver = stage_version(&store, Stage::Clip);
        let batch = pick_batch(&store, Stage::Clip, &ver, 16);
        let t_pick = t2.elapsed();
        eprintln!("50万行: 造数 {t_ins:?}, 入队 {t_enq:?}, 取批 {t_pick:?}");
        assert_eq!(n as i64, N);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_jobs"), N);
        assert_eq!(batch.len(), 16);
    }

    #[test]
    fn retry_failed_requeues_only_isolated_rows_with_zero_attempts() {
        let store = mem();
        add_photo(&store, 1, r"C:\docs\a.jpg");
        add_photo(&store, 2, r"C:\docs\b.jpg");
        enqueue(&store, Stage::Clip, 1, 1).unwrap();
        commit_batch(&store, Stage::Clip, "v0", &[1], &[]).unwrap();
        enqueue(&store, Stage::Clip, 2, 1).unwrap();
        commit_batch(&store, Stage::Clip, "v0", &[], &[(2, 3, "decode".into())]).unwrap();
        assert_eq!(retry_failed(&store, Stage::Clip).unwrap(), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_failed"), 0);
        let jobs: Vec<(i64, i32)> = store.read(|conn| {
            let mut st = conn.prepare("SELECT photo_id, attempts FROM ai_jobs").unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .flatten()
                .collect()
        });
        assert_eq!(jobs, vec![(2, 0)], "已完成项不该被重试,失败项应清零次数");
    }

    #[test]
    fn rebuild_stage_clears_derived_state_and_requeues() {
        let store = mem();
        add_photo(&store, 1, r"C:\docs\a.jpg");
        add_photo(&store, 2, r"C:\docs\b.jpg");
        let ver = stage_version(&store, Stage::Tags);
        commit_batch(&store, Stage::Tags, &ver, &[1, 2], &[]).unwrap();
        store
            .write(|conn| {
                conn.execute(
                    "INSERT INTO auto_tags(photo_id,label_id,score,model_ver) VALUES(1,1,0.9,?1)",
                    params![ver],
                )?;
                conn.execute(
                    "UPDATE photos SET caption='auto', caption_auto=1 WHERE id=1",
                    [],
                )
            })
            .unwrap();
        let n = rebuild_stage(&store, Stage::Tags).unwrap();
        assert_eq!(n, 2);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_state WHERE stage='tags'"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM auto_tags"), 0);
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM photos WHERE caption_auto=1"),
            0,
            "自动标题应随标签重建失效"
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_jobs WHERE stage='tags'"), 2);
    }

    #[test]
    fn continue_stage_keeps_completed_items() {
        let store = mem();
        add_photo(&store, 1, r"C:\docs\a.jpg");
        add_photo(&store, 2, r"C:\docs\b.jpg");
        let ver = stage_version(&store, Stage::Clip);
        commit_batch(&store, Stage::Clip, &ver, &[1], &[]).unwrap();
        continue_stage(&store, Stage::Clip).unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_state WHERE stage='clip'"), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_jobs WHERE stage='clip'"), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM ai_jobs WHERE photo_id=2"), 1);
    }

    #[test]
    fn deferred_jobs_leave_the_batch_and_return_on_preview() {
        let store = mem();
        add_photo(&store, 1, r"C:\docs\a.jpg");
        enqueue(&store, Stage::Clip, 1, P_DEFAULT).unwrap();
        assert_eq!(pick_batch(&store, Stage::Clip, "v0", BATCH_LIMIT).len(), 1);
        defer_batch(&store, Stage::Clip, &[1]).unwrap();
        assert!(
            pick_batch(&store, Stage::Clip, "v0", BATCH_LIMIT).is_empty(),
            "延后期间不该反复空转取到同一条"
        );
        enqueue_current(&store, 1);
        assert_eq!(
            pick_batch(&store, Stage::Clip, "v0", BATCH_LIMIT).len(),
            1,
            "打开预览应立即把延后任务拉回"
        );
    }
}
