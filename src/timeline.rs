use crate::db::Store;
use anyhow::Result;
use std::path::Path;

/// 时间线分组查询：按月聚合 (photo id, month bucket "YYYY-MM")。
pub struct PhotoRow {
    pub id: i64,
    pub path: String,
    pub taken: Option<String>,
    pub star: i32,
    /// 缩略图状态(0/1;预留:网格内"缩略图缺失"提示)。
    #[allow(dead_code)]
    pub thumb_ok: i32,
    /// 像素宽高(预览信息行;EXIF 缺失时为 None)。
    pub width: Option<i32>,
    pub height: Option<i32>,
    /// 相机型号(EXIF Make/Model 合并,预览信息行)。
    pub cam: String,
    /// GPS 坐标(十进制度;无定位为 None)。
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// 媒体类型:0=图片,1=视频(P2-17;`video::kind`)。
    pub kind: i32,
}

/// 相机型号池名(与 `scan.rs` / `db.rs` 的 intern 调用保持一致)。
pub const CAM_POOL: &str = "photos_cam";

pub fn load_all(store: &Store) -> Result<Vec<PhotoRow>> {
    let conn = store.0.lock().unwrap();
    // P1-14:相机型号走字符串池 —— 行里只存 `cam_id`(INTEGER),
    // 字符串本体在 `string_pool` 里去重存一份(对齐原版的池化列)。
    // LEFT JOIN 保证:未池化的历史行(cam 有值、cam_id 为 NULL)仍能显示。
    let mut st = conn.prepare(
        "SELECT p.id, p.path, p.taken_utc, p.star, p.thumb_ok, p.width, p.height,
                COALESCE(sp.value, p.cam, ''), p.lat, p.lon, p.kind
         FROM photos p
         LEFT JOIN string_pool sp ON sp.pool = ?1 AND sp.value_id = p.cam_id
         ORDER BY p.taken_utc DESC, p.path",
    )?;
    let rows: Vec<PhotoRow> = st
        .query_map(rusqlite::params![CAM_POOL], |r| {
            Ok(PhotoRow {
                id: r.get(0)?,
                path: r.get(1)?,
                taken: r.get(2)?,
                star: r.get(3)?,
                thumb_ok: r.get(4)?,
                width: r.get(5)?,
                height: r.get(6)?,
                // 池命中 → sp.value;未命中 → 回退旧 cam 列;都没有 → 空串
                cam: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                lat: r.get(8)?,
                lon: r.get(9)?,
                kind: r.get(10)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// 按月聚合，返回（月份分组、行索引），行序与输入一致（BTreeMap 按月名升序）。
pub fn by_month(rows: &[PhotoRow]) -> Vec<(String, Vec<usize>)> {
    let mut buckets: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
    for (i, r) in rows.iter().enumerate() {
        let bucket = r
            .taken
            .as_ref()
            .filter(|t| t.len() >= 7)
            .map(|t| t[..7].to_string())
            .unwrap_or_else(|| "未知时间".into());
        buckets.entry(bucket).or_default().push(i);
    }
    buckets.into_iter().collect()
}

/// 星标（0/1）。
pub fn set_star(store: &Store, id: i64, star: i32) -> Result<()> {
    store
        .0
        .lock()
        .unwrap()
        .execute("UPDATE photos SET star=?1 WHERE id=?2", rusqlite::params![star, id])?;
    Ok(())
}

/// 重复照片组:按 dHash 精确相等分组(每桶 ≥2 张)。
pub fn dup_groups(store: &Store) -> Result<Vec<(i64, Vec<i64>)>> {
    let conn = store.0.lock().unwrap();
    let mut st = conn.prepare("SELECT id, dhash FROM photos WHERE dhash IS NOT NULL")?;
    let rows: Vec<(i64, i64)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|r| r.ok())
        .collect();
    drop(st);
    let mut buckets: std::collections::HashMap<i64, Vec<i64>> = Default::default();
    for (id, h) in rows {
        buckets.entry(h).or_default().push(id);
    }
    let mut out: Vec<(i64, Vec<i64>)> = buckets
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .collect();
    out.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    Ok(out)
}

/// 从 XMP sidecar 导入单张照片的星标/标签。
/// **DB 权威,仅补空值**:DB 无星标且 XMP 有 → 导入;DB 没有的标签才补。
/// 返回 (是否导入星标, 导入标签数)。
pub fn import_xmp(store: &Store, id: i64, path: &str) -> Result<(bool, usize)> {
    let meta = crate::xmp::read(Path::new(path));
    if meta.star == 0 && meta.tags.is_empty() {
        return Ok((false, 0));
    }
    let cur_star: i32 = {
        let conn = store.0.lock().unwrap();
        conn.query_row(
            "SELECT star FROM photos WHERE id=?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };
    let cur_tags = tags_of(store, id).unwrap_or_default();
    let mut got_star = false;
    if meta.star > 0 && cur_star == 0 {
        set_star(store, id, 1)?;
        got_star = true;
    }
    let mut n = 0usize;
    for t in &meta.tags {
        if !cur_tags.contains(t) {
            add_tag(store, id, t)?;
            n += 1;
        }
    }
    Ok((got_star, n))
}

/// 批量导入所有未处理照片(`xmp_imported=0`)的 XMP sidecar。
/// 返回 (导入星标的照片数, 导入标签的照片数)。用于启动时静默增量 + 菜单手动触发。
pub fn import_xmp_batch(store: &Store, limit: usize) -> Result<(usize, usize)> {
    let rows: Vec<(i64, String)> = {
        let conn = store.0.lock().unwrap();
        let mut st = conn.prepare("SELECT id, path FROM photos WHERE xmp_imported=0 LIMIT ?1")?;
        let it = st.query_map([limit.min(1_000_000_000) as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        it.filter_map(|r| r.ok()).collect()
    };
    let (mut stars, mut tagged) = (0usize, 0usize);
    for (id, path) in rows {
        let (s, t) = import_xmp(store, id, &path)?;
        if s {
            stars += 1;
        }
        if t > 0 {
            tagged += 1;
        }
        let conn = store.0.lock().unwrap();
        conn.execute(
            "UPDATE photos SET xmp_imported=1 WHERE id=?1",
            rusqlite::params![id],
        )?;
    }
    Ok((stars, tagged))
}

/// 近似重复分组:dHash Hamming 距离 ≤ max_dist 的照片并为一组(并查集)。
/// 返回 (组内最小 id 作 key, 成员 id 升序),仅含 ≥2 张的组,按组大小降序。
/// 复杂度 O(n²·popcount);万张级约 0.2s,同步可接受(仅用户勾选时触发)。
pub fn dup_groups_near(store: &Store, max_dist: u32) -> Result<Vec<(i64, Vec<i64>)>> {
    let conn = store.0.lock().unwrap();
    let mut st = conn.prepare("SELECT id, dhash FROM photos WHERE dhash IS NOT NULL")?;
    let rows: Vec<(i64, u64)> = st
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64))
        })?
        .filter_map(|r| r.ok())
        .collect();
    drop(st);
    let n = rows.len();
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(p: &mut Vec<usize>, x: usize) -> usize {
        let mut r = x;
        while p[r] != r {
            r = p[r];
        }
        let mut c = x;
        while p[c] != c {
            let next = p[c];
            p[c] = r;
            c = next;
        }
        r
    }

    for i in 0..n {
        for j in (i + 1)..n {
            if crate::hash::hamming(rows[i].1, rows[j].1) <= max_dist {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj {
                    parent[ri] = rj;
                }
            }
        }
    }
    let mut groups: std::collections::HashMap<usize, Vec<i64>> = Default::default();
    for (i, (id, _)) in rows.iter().enumerate() {
        groups.entry(find(&mut parent, i)).or_default().push(*id);
    }
    let mut out: Vec<(i64, Vec<i64>)> = groups
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(_, mut v)| {
            v.sort();
            let key = v[0];
            (key, v)
        })
        .collect();
    out.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    Ok(out)
}

/// 加标签(不存在则创建),并挂到照片。
pub fn add_tag(store: &Store, id: i64, name: &str) -> Result<()> {
    let conn = store.0.lock().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO tags(name) VALUES(?1)",
        rusqlite::params![name],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO photo_tags(photo_id, tag_id)
         SELECT ?1, id FROM tags WHERE name=?2",
        rusqlite::params![id, name],
    )?;
    Ok(())
}

/// 解除一条标签关联;标签本身保留(其他照片可能仍在用)。
pub fn remove_tag(store: &Store, id: i64, name: &str) -> Result<()> {
    let conn = store.0.lock().unwrap();
    conn.execute(
        "DELETE FROM photo_tags WHERE photo_id=?1 AND tag_id
         IN (SELECT id FROM tags WHERE name=?2)",
        rusqlite::params![id, name],
    )?;
    Ok(())
}

/// 竖排列出照片的所有标签名。
pub fn tags_of(store: &Store, id: i64) -> Result<Vec<String>> {
    let conn = store.0.lock().unwrap();
    let mut st = conn.prepare(
        "SELECT t.name FROM tags t JOIN photo_tags pt ON pt.tag_id=t.id
         WHERE pt.photo_id=?1 ORDER BY t.name",
    )?;
    let rows: Vec<String> = st
        .query_map([id], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// 标签及照片数、颜色(视图 > 标签 左侧列表面板)。
/// color 对应原版彩色标签(color:red..gray),None = 无颜色。
#[derive(Clone, Debug)]
pub struct TagInfo {
    pub name: String,
    pub color: Option<String>,
    pub count: usize,
}

pub fn all_tags(store: &Store) -> Result<Vec<TagInfo>> {
    let conn = store.0.lock().unwrap();
    let mut st = conn.prepare(
        "SELECT t.name, t.color, COUNT(pt.photo_id) AS n FROM tags t
         LEFT JOIN photo_tags pt ON pt.tag_id=t.id
         GROUP BY t.id ORDER BY t.name",
    )?;
    let rows: Vec<TagInfo> = st
        .query_map([], |r| {
            Ok(TagInfo {
                name: r.get::<_, String>(0)?,
                color: r.get::<_, Option<String>>(1)?,
                count: r.get::<_, usize>(2)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// 设置/清除标签颜色(颜色面板;None = 无颜色)。
pub fn set_tag_color(store: &Store, name: &str, color: Option<&str>) -> Result<()> {
    let conn = store.0.lock().unwrap();
    conn.execute(
        "UPDATE tags SET color=?1 WHERE name=?2",
        rusqlite::params![color, name],
    )?;
    Ok(())
}

/// 某标签下的照片 id 列表(按 id 升序)。
pub fn photos_by_tag(store: &Store, tag: &str) -> Result<Vec<i64>> {
    let conn = store.0.lock().unwrap();
    let mut st = conn.prepare(
        "SELECT pt.photo_id FROM photo_tags pt JOIN tags t ON t.id=pt.tag_id
         WHERE t.name=?1 ORDER BY pt.photo_id",
    )?;
    let rows: Vec<i64> = st
        .query_map([tag], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// 从图库索引移除照片:只删索引行(photo_tags/faces/face_templates/photos),
/// 绝不删除磁盘上的原文件。对应菜单 编辑 > 从图库中移除。
pub fn remove_photos(store: &Store, ids: &[i64]) -> Result<()> {
    let conn = store.0.lock().unwrap();
    let tx = conn.unchecked_transaction()?;
    {
        let mut st_tags = tx.prepare_cached("DELETE FROM photo_tags WHERE photo_id=?1")?;
        let mut st_ft = tx.prepare_cached(
            "DELETE FROM face_templates WHERE face_id IN
             (SELECT id FROM faces WHERE photo_id=?1)",
        )?;
        let mut st_faces = tx.prepare_cached("DELETE FROM faces WHERE photo_id=?1")?;
        let mut st_clips = tx.prepare_cached("DELETE FROM clips WHERE photo_id=?1")?;
        let mut st_clips_cn = tx.prepare_cached("DELETE FROM clips_cn WHERE photo_id=?1")?;
        let mut st_photos = tx.prepare_cached("DELETE FROM photos WHERE id=?1")?;
        for id in ids {
            st_tags.execute(rusqlite::params![id])?;
            st_clips.execute(rusqlite::params![id])?;
            st_clips_cn.execute(rusqlite::params![id])?;
            st_ft.execute(rusqlite::params![id])?;
            st_faces.execute(rusqlite::params![id])?;
            st_photos.execute(rusqlite::params![id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// 移除某文件夹(含子文件夹)下所有照片的索引行;返回移除条数。
/// 只删索引,绝不碰磁盘文件(文件夹管理器 > 从图库移除)。
pub fn remove_photos_under(store: &Store, folder: &str) -> Result<usize> {
    let prefix = folder.trim_end_matches(['\\', '/']);
    let ids: Vec<i64> = {
        let conn = store.0.lock().unwrap();
        let mut st = conn.prepare("SELECT id FROM photos WHERE path LIKE ?1 ESCAPE '\\'")?;
        let rows = st.query_map(rusqlite::params![format!("{}%", like_escape(prefix))], |r| {
            r.get::<_, i64>(0)
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };
    let n = ids.len();
    if n > 0 {
        remove_photos(store, &ids)?;
    }
    Ok(n)
}

/// LIKE 通配符转义(与 db.rs 同规则;路径名里的 %/_ 不当模式符)。
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// 带 GPS 的照片点(KML 导出用;按路径排序稳定输出)。
pub struct GpsPoint {
    pub path: String,
    pub taken: Option<String>,
    pub star: i32,
    pub lat: f64,
    pub lon: f64,
}

pub fn gps_points(store: &Store) -> Result<Vec<GpsPoint>> {
    let conn = store.0.lock().unwrap();
    let mut st = conn.prepare(
        "SELECT path, taken_utc, star, lat, lon FROM photos
         WHERE lat IS NOT NULL AND lon IS NOT NULL ORDER BY path",
    )?;
    let rows: Vec<GpsPoint> = st
        .query_map([], |r| {
            Ok(GpsPoint {
                path: r.get(0)?,
                taken: r.get(1)?,
                star: r.get(2)?,
                lat: r.get(3)?,
                lon: r.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;

    fn one_photo(store: &Store) -> i64 {
        store
            .0
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO photos(path,parent,mtime,size) VALUES('a.jpg','.',0,0)",
                [],
            )
            .unwrap();
        store.0.lock().unwrap().last_insert_rowid()
    }

    /// 复核修复:删除标签后 DB 关联必须真正消失(此前 XMP 残留会把标签复活回 DB)。
    #[test]
    fn tag_add_remove_roundtrip() {
        let store = Store::open_mem().unwrap();
        let id = one_photo(&store);
        add_tag(&store, id, "cat").unwrap();
        add_tag(&store, id, "dog").unwrap();
        assert_eq!(tags_of(&store, id).unwrap(), vec!["cat".to_string(), "dog".to_string()]);
        remove_tag(&store, id, "cat").unwrap();
        assert_eq!(tags_of(&store, id).unwrap(), vec!["dog".to_string()]);
        remove_tag(&store, id, "dog").unwrap();
        assert!(tags_of(&store, id).unwrap().is_empty());
    }

    /// 视图 > 标签:列表面板计数与过滤查询一致(含未使用标签计数为 0)。
    #[test]
    fn all_tags_and_photos_by_tag() {
        let store = Store::open_mem().unwrap();
        let id = one_photo(&store);
        add_tag(&store, id, "cat").unwrap();
        add_tag(&store, id, "cat").unwrap(); // 幂等
        let tags = all_tags(&store).unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "cat");
        assert_eq!(tags[0].count, 1);
        assert_eq!(photos_by_tag(&store, "cat").unwrap(), vec![id]);
        assert!(photos_by_tag(&store, "missing").unwrap().is_empty());
    }

    /// P0 固化:star 列读写链路(load_all SELECT / set_star UPDATE)。
    #[test]
    fn star_set_and_read_back() {
        let store = Store::open_mem().unwrap();
        let id = one_photo(&store);
        set_star(&store, id, 1).unwrap();
        let rows = load_all(&store).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].star, 1);
        set_star(&store, id, 0).unwrap();
        assert_eq!(load_all(&store).unwrap()[0].star, 0);
    }

    /// 彩色标签 set/读回(原版 color:red..gray)。
    #[test]
    fn tag_color_roundtrip() {
        let store = Store::open_mem().unwrap();
        let id = one_photo(&store);
        add_tag(&store, id, "海边").unwrap();
        assert!(all_tags(&store).unwrap()[0].color.is_none());
        set_tag_color(&store, "海边", Some("blue")).unwrap();
        let tags = all_tags(&store).unwrap();
        assert_eq!(tags[0].color.as_deref(), Some("blue"));
        assert_eq!(tags[0].count, 1);
        set_tag_color(&store, "海边", None).unwrap();
        assert!(all_tags(&store).unwrap()[0].color.is_none());
    }

    /// 按文件夹移除索引(含子文件夹),别的文件夹不受影响。
    #[test]
    fn remove_photos_under_folder() {
        let store = Store::open_mem().unwrap();
        let c = store.0.lock().unwrap();
        c.execute("INSERT INTO photos(path,parent,mtime,size) VALUES('/d/a.jpg','/d',0,0)", []).unwrap();
        c.execute("INSERT INTO photos(path,parent,mtime,size) VALUES('/d/sub/b.jpg','/d/sub',0,0)", []).unwrap();
        c.execute("INSERT INTO photos(path,parent,mtime,size) VALUES('/e/c.jpg','/e',0,0)", []).unwrap();
        drop(c);
        let n = remove_photos_under(&store, "/d").unwrap();
        assert_eq!(n, 2, "应移除 /d 与 /d/sub 两张");
        let left = load_all(&store).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].path, "/e/c.jpg");
    }

    /// remove_photos 必须同时清 clips 行(外键 cascade 未开)。
    #[test]
    fn remove_photos_clears_clips() {
        let store = Store::open_mem().unwrap();
        let id = one_photo(&store);
        store.0.lock().unwrap().execute(
            "INSERT INTO clips(photo_id,dim,vec) VALUES(?1,2,X'0000803f0000003f')",
            rusqlite::params![id],
        ).unwrap();
        let n: i64 = store.0.lock().unwrap()
            .query_row("SELECT COUNT(*) FROM clips", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        remove_photos(&store, &[id]).unwrap();
        let n2: i64 = store.0.lock().unwrap()
            .query_row("SELECT COUNT(*) FROM clips", [], |r| r.get(0)).unwrap();
        assert_eq!(n2, 0, "clips 行应随照片索引一并删除");
    }

    /// GPS 点查询(有/无坐标过滤)。
    #[test]
    fn gps_points_query() {
        let store = Store::open_mem().unwrap();
        let c = store.0.lock().unwrap();
        c.execute("INSERT INTO photos(path,parent,mtime,size,lat,lon) VALUES('/a.jpg','.',0,0,31.24,121.47)", []).unwrap();
        c.execute("INSERT INTO photos(path,parent,mtime,size) VALUES('/b.jpg','.',0,0)", []).unwrap();
        drop(c);
        let pts = gps_points(&store).unwrap();
        assert_eq!(pts.len(), 1);
        assert!((pts[0].lat - 31.24).abs() < 1e-9);
        assert_eq!(pts[0].path, "/a.jpg");
    }

    /// XMP 导入 — sidecar 星标/标签补入 DB;**一次性**(xmp_imported 标记),
    /// 用户在 app 内清空后不得被 sidecar 复活(同 P0 删标签复活语义),重置标记才重导。
    #[test]
    fn import_xmp_fills_empty_db_only() {
        let dir = std::env::temp_dir().join(format!("rk-xmp-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("a.jpg");
        std::fs::write(&p1, b"x").unwrap();
        crate::xmp::write(
            &p1,
            &crate::xmp::Meta { star: 1, tags: vec!["cat".into()] },
        )
        .unwrap();

        let store = Store::open_mem().unwrap();
        let id = one_photo(&store);
        store
            .0
            .lock()
            .unwrap()
            .execute(
                "UPDATE photos SET path=?1 WHERE id=?2",
                rusqlite::params![p1.to_string_lossy(), id],
            )
            .unwrap();

        // 1. DB 空 → 经 batch 导入并标记
        let (stars, tagged) = import_xmp_batch(&store, 100).unwrap();
        assert_eq!((stars, tagged), (1, 1));
        assert_eq!(load_all(&store).unwrap()[0].star, 1);
        assert_eq!(tags_of(&store, id).unwrap(), vec!["cat".to_string()]);

        // 2. 已导入过(xmp_imported=1):用户在 app 内清空 → batch 不得复活
        set_star(&store, id, 0).unwrap();
        remove_tag(&store, id, "cat").unwrap();
        let (s2, t2) = import_xmp_batch(&store, 100).unwrap();
        assert_eq!((s2, t2), (0, 0), "已导入过的照片不得被 sidecar 复活");
        assert_eq!(load_all(&store).unwrap()[0].star, 0);
        assert!(tags_of(&store, id).unwrap().is_empty());

        // 3. 重置标记 → 合理重导(仅补 DB 空值)
        store
            .0
            .lock()
            .unwrap()
            .execute(
                "UPDATE photos SET xmp_imported=0 WHERE id=?1",
                rusqlite::params![id],
            )
            .unwrap();
        let (s3, t3) = import_xmp_batch(&store, 100).unwrap();
        assert_eq!((s3, t3), (1, 1));

        // 4. 无 sidecar 的照片:直接标记,不报错
        store.0.lock().unwrap().execute(
            "INSERT INTO photos(path,parent,mtime,size) VALUES(?1,'.',0,0)",
            rusqlite::params![dir.join("b.jpg").to_string_lossy()],
        ).unwrap();
        let (s4, t4) = import_xmp_batch(&store, 100).unwrap();
        assert_eq!((s4, t4), (0, 0));
        let left: i64 = store
            .0
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM photos WHERE xmp_imported=0", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(left, 0, "批量后应全部标记已导入");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 近似重复分组 — hamming ≤ 阈值并簇;阈值越小组越拆得细。
    #[test]
    fn dup_groups_near_threshold() {
        let store = Store::open_mem().unwrap();
        {
            let c = store.0.lock().unwrap();
            // 3 张照片:a=0x00,b=0x01(与 a 距 1),c=0x0f(与 a 距 4)
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,dhash) VALUES(1,'a.jpg','.',0,0,0)",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,dhash) VALUES(2,'b.jpg','.',0,0,1)",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,dhash) VALUES(3,'c.jpg','.',0,0,15)",
                [],
            )
            .unwrap();
            // 单独一张(不应成组)
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,dhash) VALUES(4,'d.jpg','.',0,0,-1)",
                [],
            )
            .unwrap();
        }
        // max_dist=1:{a,b} 一组;c、d 单独
        let g = dup_groups_near(&store, 1).unwrap();
        assert_eq!(g.len(), 1, "阈值 1 应只有 a/b 一组: {g:?}");
        assert_eq!(g[0].1, vec![1, 2]);
        // max_dist=5:{a,b,c} 全并(transitive);d 单独
        let g2 = dup_groups_near(&store, 5).unwrap();
        assert_eq!(g2.len(), 1);
        assert_eq!(g2[0].1, vec![1, 2, 3], "距离可传递: a~c 距 4 ≤ 5");
        // max_dist=0:退化为精确相等
        let g0 = dup_groups_near(&store, 0).unwrap();
        assert!(g0.is_empty(), "阈值 0 无相等哈希 → 空");
    }
}
