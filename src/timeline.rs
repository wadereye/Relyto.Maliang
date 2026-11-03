use crate::db::Store;
use anyhow::Result;
use std::collections::HashMap;
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
    /// 标题(参考件 Caption;缩略图说明/幻灯片/属性面板显示)。
    pub caption: String,
    /// GPS 坐标(十进制度;无定位为 None)。
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// 媒体类型:0=图片,1=视频(P2-17;`video::kind`)。
    pub kind: i32,
    /// 磁盘修改时间(ms,扫描器写入):「按最近更改排序」的 SQL 键。
    /// Rust 侧暂只用于属性面板展示,排序本身在 SQL 完成。
    #[allow(dead_code)]
    pub mtime: i64,
    /// 文件字节数:「按大小排序」的 SQL 键;属性面板展示用。
    #[allow(dead_code)]
    pub size: i64,
    /// 所在目录(`photos.parent`,组头分组键;避免每帧从 path 反推)。
    pub parent: String,
}

/// 排序方式(工具栏「查看选项」> 排序;对应原版 View 菜单里的
/// 四个排序项:按拍摄日期 / 最近 / 按尺寸 / 按名称)。
#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub enum SortBy {
    /// 拍摄日期(`Sort by &Creation Date`,默认,新在前)。
    #[default]
    Date,
    /// 最近更改日期(`Sort by &Recent Changes`,新在前)。
    Recent,
    /// 文件大小(`Sort by &Size`,大在前)。
    Size,
    /// 文件名(`Sort by &Name`,升序)。
    Name,
}

/// 视图过滤(参考件 `searchcontainer/{星标搜索,人脸搜索,视频搜索,地理标签搜索}`,
/// 四个可叠加的开关 + 搜索框文本;全默认 = 显示全部)。
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct Filters {
    /// 只显示加星标的照片。
    pub starred: bool,
    /// 只显示有人物头像的照片。
    pub faces: bool,
    /// 只显示视频。
    pub movies: bool,
    /// 只显示有地理标记的照片。
    pub geotagged: bool,
    /// 搜索框文本(参考件 `searchcontainer/search` + `searchautocomplete`;
    /// 空串 = 不过滤。匹配路径 / 标题 / 标签名)。
    pub text: String,
}

impl SortBy {
    /// 菜单顺序沿用参考件「查看选项」:Date、Recent Changes、Size、Name。
    pub const ALL: [SortBy; 4] = [SortBy::Date, SortBy::Recent, SortBy::Size, SortBy::Name];

    /// 菜单文案(参考件 `Sort by &Creation Date` 等,去掉助记符 &)。
    pub fn label(self) -> &'static str {
        match self {
            SortBy::Date => "按拍摄日期",
            SortBy::Recent => "按最近更改日期",
            SortBy::Size => "按大小",
            SortBy::Name => "按名称",
        }
    }

    /// 设置项里存的 key(与 UI 文案解耦,改文案不破坏已存配置)。
    pub fn key(self) -> &'static str {
        match self {
            SortBy::Date => "date",
            SortBy::Recent => "recent",
            SortBy::Size => "size",
            SortBy::Name => "name",
        }
    }

    pub fn from_key(s: &str) -> Option<SortBy> {
        SortBy::ALL.into_iter().find(|x| x.key() == s)
    }
}

/// 四个过滤开关的(图标, 悬停文案),索引顺序即工具栏从左到右与设置串顺序。
/// 参考件控件名:`searchcontainer/{星标搜索,人脸搜索,视频搜索,地理标签搜索}`。
pub const FILTER_BTNS: [(&str, &str); 4] = [
    ("star", "只显示加星标的照片"),
    ("smile", "只显示有人物头像的照片"),
    ("video", "只显示视频"),
    ("map-pin", "只显示有地理标记的照片"),
];

impl Filters {
    /// 是否没有任何过滤(用于状态栏与 SQL 短路)。
    pub fn is_none(&self) -> bool {
        self.text.trim().is_empty()
            && !(self.starred || self.faces || self.movies || self.geotagged)
    }

    /// 按位读开关(索引即 UI 与设置串的顺序)。
    pub fn flag(&self, i: usize) -> bool {
        match i {
            0 => self.starred,
            1 => self.faces,
            2 => self.movies,
            _ => self.geotagged,
        }
    }

    /// 按位翻转开关。
    pub fn flip(&mut self, i: usize) {
        match i {
            0 => self.starred = !self.starred,
            1 => self.faces = !self.faces,
            2 => self.movies = !self.movies,
            _ => self.geotagged = !self.geotagged,
        }
    }

    /// 设置项里存的 4 位开关串,顺序即 `FILTER_BTNS`。
    /// 搜索文本是临时输入,不落库。
    pub fn as_setting(&self) -> String {
        (0..FILTER_BTNS.len())
            .map(|i| if self.flag(i) { '1' } else { '0' })
            .collect()
    }

    /// 解析 `as_setting`;长度不符或含非 0/1 一律回退全关(宁可不带过滤,不炸启动)。
    pub fn from_setting(s: &str) -> Filters {
        let mut f = Filters::default();
        for i in 0..FILTER_BTNS.len() {
            if s.as_bytes().get(i) == Some(&b'1') {
                f.flip(i);
            }
        }
        f
    }
}

/// 相机型号池名(与 `scan.rs` / `db.rs` 的 intern 调用保持一致)。
pub const CAM_POOL: &str = "photos_cam";

/// 全库默认加载(无过滤、按拍摄日期)。
/// app 侧一律走 `load_query`(视图带排序与过滤),这里只留给测试与历史调用点。
#[allow(dead_code)]
pub fn load_all(store: &Store) -> Result<Vec<PhotoRow>> {
    load_query(store, SortBy::Date, false, &Filters::default())
}

/// 带排序与过滤的加载。
///
/// 过滤走 SQL 谓词(不是取回全表再在 UI 层筛):星标/视频/地理标记是
/// `photos` 上的列,「只显示人物头像」需要 `faces` 表的 EXISTS,
/// 大图库下这一步留在 SQL 侧才不必每帧搬全表。
/// 排序键全部取自表列,SQL 拼串只用固定字面量(不接受外部输入)。
pub fn load_query(
    store: &Store,
    sort: SortBy,
    reverse: bool,
    filters: &Filters,
) -> Result<Vec<PhotoRow>> {
    let mut where_sql: Vec<&'static str> = Vec::new();
    if filters.starred {
        where_sql.push("p.star > 0");
    }
    if filters.faces {
        where_sql.push("EXISTS (SELECT 1 FROM faces f WHERE f.photo_id = p.id AND f.ignored = 0)");
    }
    if filters.movies {
        where_sql.push("p.kind = 1");
    }
    if filters.geotagged {
        where_sql.push("p.lat IS NOT NULL AND p.lon IS NOT NULL");
    }
    // 搜索框文本:命中路径、标题或标签名任一即可(参考件的库内搜索范围)。
    let text = filters.text.trim();
    let has_text = !text.is_empty();
    if has_text {
        where_sql.push(
            "(p.path LIKE ?2 ESCAPE '\\' OR COALESCE(p.caption,'') LIKE ?2 ESCAPE '\\'
              OR EXISTS (SELECT 1 FROM photo_tags pt JOIN tags t ON t.id = pt.tag_id
                         WHERE pt.photo_id = p.id AND t.name LIKE ?2 ESCAPE '\\'))",
        );
    }
    // 各模式的"自然方向"(反向开关再翻转):日期/更改新在前、大小大在前、名称升序。
    let order = match sort {
        SortBy::Date => "p.taken_utc IS NULL, p.taken_utc DESC",
        SortBy::Recent => "p.mtime DESC",
        SortBy::Size => "p.size DESC",
        SortBy::Name => "p.path COLLATE NOCASE ASC",
    };
    let order = if reverse {
        match sort {
            SortBy::Date => "p.taken_utc IS NULL, p.taken_utc ASC",
            SortBy::Recent => "p.mtime ASC",
            SortBy::Size => "p.size ASC",
            SortBy::Name => "p.path COLLATE NOCASE DESC",
        }
    } else {
        order
    };
    let sql = format!(
        "SELECT p.id, p.path, p.taken_utc, p.star, p.thumb_ok, p.width, p.height,
                COALESCE(sp.value, p.cam, ''), p.lat, p.lon, p.kind, p.caption,
                p.mtime, p.size, p.parent
         FROM photos p
         LEFT JOIN string_pool sp ON sp.pool = ?1 AND sp.value_id = p.cam_id
         {}
         ORDER BY {}",
        if where_sql.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_sql.join(" AND "))
        },
        order
    );
    let conn = store.0.lock().unwrap();
    // P1-14:相机型号走字符串池 —— 行里只存 `cam_id`(INTEGER),
    // 字符串本体在 `string_pool` 里去重存一份(对齐原版的池化列)。
    // LEFT JOIN 保证:未池化的历史行(cam 有值、cam_id 为 NULL)仍能显示。
    let pat = format!("%{}%", like_escape(text));
    // ?2 只在有搜索文本时出现在 SQL 里,参数也必须同步 —— 绑一个查询里
    // 不存在的下标会被 SQLite 拒绝,所以按分支给参数。
    let args: Vec<&dyn rusqlite::types::ToSql> = if has_text {
        vec![&CAM_POOL, &pat]
    } else {
        vec![&CAM_POOL]
    };
    let mut st = conn.prepare(&sql)?;
    let rows: Vec<PhotoRow> = st
        .query_map(rusqlite::params_from_iter(args.iter()), |r| {
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
                caption: r.get::<_, Option<String>>(11)?.unwrap_or_default(),
                mtime: r.get(12)?,
                size: r.get(13)?,
                parent: r.get(14)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// 按目录分组,组内与组间都保持传入顺序(即排序方式直接决定组头排列)。
/// 返回 (目录, 该目录的行下标);目录名取 `PhotoRow::parent`。
pub fn by_folder(rows: &[PhotoRow]) -> Vec<(String, Vec<usize>)> {
    let mut order: Vec<String> = Vec::new();
    let mut buckets: HashMap<String, Vec<usize>> = Default::default();
    for (i, r) in rows.iter().enumerate() {
        let key = if r.parent.is_empty() {
            match folder_of(&r.path) {
                Some(p) => p.to_string(),
                None => String::new(),
            }
        } else {
            r.parent.clone()
        };
        if !buckets.contains_key(&key) {
            order.push(key.clone());
        }
        buckets.entry(key).or_default().push(i);
    }
    order
        .into_iter()
        .map(|k| {
            let idx = buckets.remove(&k).unwrap_or_default();
            (k, idx)
        })
        .collect()
}

/// 取路径的父目录(空路径/根返回 None)。
fn folder_of(path: &str) -> Option<&str> {
    let p = Path::new(path);
    p.parent().and_then(|x| x.to_str()).map(|s| s.trim_end())
}

/// 组的显示日期:组内最早的拍摄时间(`taken_utc` 前 10 字 = 日期);
/// 组内全无 EXIF 拍摄时间时返回空串,由 UI 显示张数而非假日期。
/// 参考件的相册头用「相册日期」(album date),即组内最早拍摄日期。
pub fn group_date(rows: &[PhotoRow], idx: &[usize]) -> String {
    let mut best: Option<&str> = None;
    for i in idx {
        if let Some(t) = rows.get(*i).and_then(|r| r.taken.as_deref()) {
            if best.map(|b| t < b).unwrap_or(true) {
                best = Some(t);
            }
        }
    }
    match best {
        Some(t) if t.len() >= 10 => t[..10].to_string(),
        Some(t) => t.to_string(),
        None => String::new(),
    }
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
    store.0.lock().unwrap().execute(
        "UPDATE photos SET star=?1 WHERE id=?2",
        rusqlite::params![star, id],
    )?;
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
    let mut out: Vec<(i64, Vec<i64>)> = buckets.into_iter().filter(|(_, v)| v.len() > 1).collect();
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
        let rows = st.query_map(
            rusqlite::params![format!("{}%", like_escape(prefix))],
            |r| r.get::<_, i64>(0),
        )?;
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
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
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
        assert_eq!(
            tags_of(&store, id).unwrap(),
            vec!["cat".to_string(), "dog".to_string()]
        );
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
        c.execute(
            "INSERT INTO photos(path,parent,mtime,size) VALUES('/d/a.jpg','/d',0,0)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO photos(path,parent,mtime,size) VALUES('/d/sub/b.jpg','/d/sub',0,0)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO photos(path,parent,mtime,size) VALUES('/e/c.jpg','/e',0,0)",
            [],
        )
        .unwrap();
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
        store
            .0
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO clips(photo_id,dim,vec) VALUES(?1,2,X'0000803f0000003f')",
                rusqlite::params![id],
            )
            .unwrap();
        let n: i64 = store
            .0
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM clips", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        remove_photos(&store, &[id]).unwrap();
        let n2: i64 = store
            .0
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM clips", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n2, 0, "clips 行应随照片索引一并删除");
    }

    /// GPS 点查询(有/无坐标过滤)。
    #[test]
    fn gps_points_query() {
        let store = Store::open_mem().unwrap();
        let c = store.0.lock().unwrap();
        c.execute("INSERT INTO photos(path,parent,mtime,size,lat,lon) VALUES('/a.jpg','.',0,0,31.24,121.47)", []).unwrap();
        c.execute(
            "INSERT INTO photos(path,parent,mtime,size) VALUES('/b.jpg','.',0,0)",
            [],
        )
        .unwrap();
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
            &crate::xmp::Meta {
                star: 1,
                tags: vec!["cat".into()],
            },
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
        store
            .0
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO photos(path,parent,mtime,size) VALUES(?1,'.',0,0)",
                rusqlite::params![dir.join("b.jpg").to_string_lossy()],
            )
            .unwrap();
        let (s4, t4) = import_xmp_batch(&store, 100).unwrap();
        assert_eq!((s4, t4), (0, 0));
        let left: i64 = store
            .0
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM photos WHERE xmp_imported=0",
                [],
                |r| r.get(0),
            )
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

    /// 造一条属性可控的照片行(排序/过滤/组头测试用)。
    /// `lat` 为 Some 时经度固定填一个值,只用于区分"有/无 GPS"。
    #[allow(clippy::too_many_arguments)]
    fn insert_photo(
        store: &Store,
        path: &str,
        parent: &str,
        taken: Option<&str>,
        star: i32,
        kind: i32,
        mtime: i64,
        size: i64,
        lat: Option<f64>,
    ) -> i64 {
        let c = store.0.lock().unwrap();
        c.execute(
            "INSERT INTO photos(path,parent,mtime,size,star,kind,taken_utc,lat,lon)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            rusqlite::params![
                path,
                parent,
                mtime,
                size,
                star,
                kind,
                taken,
                lat,
                lat.map(|_| 121.47)
            ],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    fn add_face(store: &Store, photo_id: i64, ignored: i32) {
        store
            .0
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO faces(photo_id,x1,y1,x2,y2,ignored) VALUES(?1,0.1,0.1,0.5,0.5,?2)",
                rusqlite::params![photo_id, ignored],
            )
            .unwrap();
    }

    /// 工具栏四个过滤开关:各自精确圈定行,可叠加,全关时不过滤。
    /// 「只显示人物头像」必须忽略 `ignored=1` 的人脸(用户已忽略的不算)。
    #[test]
    fn load_query_filters_isolate_rows() {
        let store = Store::open_mem().unwrap();
        let starred = insert_photo(
            &store,
            "/p/1star.jpg",
            "/p",
            Some("2024-01-01T00:00:00"),
            2,
            0,
            10,
            100,
            None,
        );
        let faced = insert_photo(
            &store,
            "/p/2face.jpg",
            "/p",
            Some("2024-01-02T00:00:00"),
            0,
            0,
            20,
            200,
            None,
        );
        let movie = insert_photo(
            &store,
            "/p/3movie.mp4",
            "/p",
            Some("2024-01-03T00:00:00"),
            0,
            1,
            30,
            300,
            None,
        );
        let geo = insert_photo(
            &store,
            "/p/4geo.jpg",
            "/p",
            Some("2024-01-04T00:00:00"),
            0,
            0,
            40,
            400,
            Some(31.24),
        );
        let plain = insert_photo(
            &store,
            "/p/5plain.jpg",
            "/p",
            Some("2024-01-05T00:00:00"),
            0,
            0,
            50,
            500,
            None,
        );
        add_face(&store, faced, 0);
        add_face(&store, plain, 1); // 已忽略 → 不算"有人物头像"
        let all = vec![starred, faced, movie, geo, plain];

        let ids = |f: Filters| -> Vec<i64> {
            let mut v: Vec<i64> = load_query(&store, SortBy::Name, false, &f)
                .unwrap()
                .into_iter()
                .map(|r| r.id)
                .collect();
            v.sort();
            v
        };
        assert_eq!(ids(Filters::default()), all, "全关 = 不过滤");
        assert!(Filters::default().is_none());

        let mut f = Filters::default();
        f.starred = true;
        assert!(!f.is_none(), "任一开关打开即非「无过滤」");
        assert_eq!(ids(f), vec![starred]);
        f = Filters::default();
        f.faces = true;
        assert_eq!(ids(f), vec![faced], "ignored=1 的人脸不应命中");
        f = Filters::default();
        f.movies = true;
        assert_eq!(ids(f), vec![movie]);
        f = Filters::default();
        f.geotagged = true;
        assert_eq!(ids(f), vec![geo]);
        // 叠加取交集
        f = Filters::default();
        f.movies = true;
        f.geotagged = true;
        assert!(ids(f).is_empty(), "过滤开关叠加 = AND 交集");
        f = Filters::default();
        f.faces = true;
        f.movies = true;
        assert!(ids(f).is_empty());
    }

    /// 搜索框文本:命中路径、标题或标签名任一;与开关过滤叠加(AND)。
    #[test]
    fn load_query_text_search() {
        let store = Store::open_mem().unwrap();
        let sea = insert_photo(
            &store,
            "/p/sea-2024.jpg",
            "/p",
            Some("2024-01-01T00:00:00"),
            0,
            0,
            10,
            100,
            None,
        );
        let titled = insert_photo(
            &store,
            "/p/DSC001.jpg",
            "/p",
            Some("2024-01-02T00:00:00"),
            0,
            0,
            20,
            200,
            None,
        );
        let tagged = insert_photo(
            &store,
            "/p/DSC002.jpg",
            "/p",
            Some("2024-01-03T00:00:00"),
            0,
            0,
            30,
            300,
            None,
        );
        insert_photo(
            &store,
            "/p/other.jpg",
            "/p",
            Some("2024-01-04T00:00:00"),
            0,
            0,
            40,
            400,
            None,
        );
        store
            .0
            .lock()
            .unwrap()
            .execute(
                "UPDATE photos SET caption='海边日落' WHERE id=?1",
                rusqlite::params![titled],
            )
            .unwrap();
        add_tag(&store, tagged, "海景").unwrap();

        let ids = |text: &str| -> Vec<i64> {
            let f = Filters {
                text: text.into(),
                ..Default::default()
            };
            load_query(&store, SortBy::Name, false, &f)
                .unwrap()
                .into_iter()
                .map(|r| r.id)
                .collect()
        };
        assert_eq!(ids("sea"), vec![sea], "按路径片段命中");
        assert_eq!(ids("海边"), vec![titled], "按标题命中(中文)");
        assert_eq!(ids("海景"), vec![tagged], "按标签名命中");
        assert_eq!(ids("DSC0"), vec![titled, tagged], "同前缀两命中,按名称升序");
        assert!(ids("不存在的词").is_empty());
        // 通配符按字面匹配,不当 LIKE 模式(路径里真有 % 也能搜到)
        assert!(ids("%").is_empty(), "% 应转义为字面字符");
        // 文本 + 开关 = 交集
        let f = Filters {
            text: "DSC".into(),
            movies: true,
            ..Default::default()
        };
        assert!(
            load_query(&store, SortBy::Name, false, &f)
                .unwrap()
                .is_empty(),
            "视频∩DSC 无"
        );
        assert!(
            Filters {
                text: "  ".into(),
                ..Default::default()
            }
            .is_none(),
            "纯空白 = 无过滤"
        );
    }

    /// 四种排序方式 + 反向开关的方向;NULL 拍摄时间恒排最后(反向也不提前)。
    #[test]
    fn load_query_sorts_and_reverse() {
        let store = Store::open_mem().unwrap();
        // 路径故意混大小写,验证 NOCASE;a/B/c 与 taken、mtime、size 三组次序互不相同。
        insert_photo(
            &store,
            "/p/a.jpg",
            "/p",
            Some("2024-03-01T00:00:00"),
            0,
            0,
            30,
            100,
            None,
        );
        insert_photo(
            &store,
            "/p/B.jpg",
            "/p",
            Some("2024-01-01T00:00:00"),
            0,
            0,
            10,
            300,
            None,
        );
        insert_photo(&store, "/p/c.jpg", "/p", None, 0, 0, 20, 200, None);
        let order = |sort: SortBy, reverse: bool| -> Vec<String> {
            load_query(&store, sort, reverse, &Filters::default())
                .unwrap()
                .into_iter()
                .map(|r| r.path.rsplit('/').next().unwrap().to_string())
                .collect()
        };
        // 拍摄日期:新在前,无 EXIF 排最后
        assert_eq!(order(SortBy::Date, false), ["a.jpg", "B.jpg", "c.jpg"]);
        assert_eq!(
            order(SortBy::Date, true),
            ["B.jpg", "a.jpg", "c.jpg"],
            "反向仍 NULL 最后"
        );
        // 最近更改:mtime 大在前
        assert_eq!(order(SortBy::Recent, false), ["a.jpg", "c.jpg", "B.jpg"]);
        assert_eq!(order(SortBy::Recent, true), ["B.jpg", "c.jpg", "a.jpg"]);
        // 大小:大在前
        assert_eq!(order(SortBy::Size, false), ["B.jpg", "c.jpg", "a.jpg"]);
        assert_eq!(order(SortBy::Size, true), ["a.jpg", "c.jpg", "B.jpg"]);
        // 名称:NOCASE 升序
        assert_eq!(order(SortBy::Name, false), ["a.jpg", "B.jpg", "c.jpg"]);
        assert_eq!(order(SortBy::Name, true), ["c.jpg", "B.jpg", "a.jpg"]);
    }

    /// 组头分组:组序与组内序都跟随排序方式(参考件平铺视图即按当前排序铺组)。
    /// 组头日期 = 组内最早拍摄日期(不是最新)。
    #[test]
    fn by_folder_keeps_sort_order_and_group_date() {
        let store = Store::open_mem().unwrap();
        insert_photo(
            &store,
            "/n/a.jpg",
            "/n",
            Some("2024-05-01T00:00:00"),
            0,
            0,
            30,
            100,
            None,
        );
        insert_photo(
            &store,
            "/n/b.jpg",
            "/n",
            Some("2024-01-01T00:00:00"),
            0,
            0,
            10,
            100,
            None,
        );
        insert_photo(
            &store,
            "/o/c.jpg",
            "/o",
            Some("2024-02-01T00:00:00"),
            0,
            0,
            20,
            100,
            None,
        );
        let rows = load_query(&store, SortBy::Recent, false, &Filters::default()).unwrap();
        let groups = by_folder(&rows);
        assert_eq!(
            groups.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["/n", "/o"],
            "组序跟随排序:/n 有最新 mtime=30"
        );
        assert_eq!(
            rows[groups[0].1[0]].path, "/n/a.jpg",
            "组内同样按 mtime 降序"
        );
        assert_eq!(rows[groups[0].1[1]].path, "/n/b.jpg");
        assert_eq!(group_date(&rows, &groups[0].1), "2024-01-01", "取组内最早");
        assert_eq!(group_date(&rows, &groups[1].1), "2024-02-01");
        // parent 为空的旧行回退到路径父目录
        insert_photo(
            &store,
            "/fb/x.jpg",
            "",
            Some("2024-06-01T00:00:00"),
            0,
            0,
            40,
            100,
            None,
        );
        let rows2 = load_query(&store, SortBy::Recent, false, &Filters::default()).unwrap();
        let g2 = by_folder(&rows2);
        assert_eq!(g2[0].0, "/fb", "parent 空 → 从 path 反推父目录");
    }

    /// 组内全无 EXIF 拍摄时间 → 组头日期为空串(UI 改显示张数,不编造假日期)。
    #[test]
    fn group_date_is_empty_without_taken() {
        let store = Store::open_mem().unwrap();
        insert_photo(&store, "/x/a.jpg", "/x", None, 0, 0, 1, 1, None);
        insert_photo(&store, "/x/b.jpg", "/x", None, 0, 0, 2, 1, None);
        let rows = load_all(&store).unwrap();
        assert_eq!(group_date(&rows, &[0, 1]), "");
        assert_eq!(group_date(&rows, &[]), "");
    }

    /// 视图状态存取(重启后保持上次的排序与过滤);脏值一律回退默认,不炸启动。
    #[test]
    fn sort_and_filters_setting_roundtrip() {
        for s in SortBy::ALL {
            assert_eq!(SortBy::from_key(s.key()), Some(s));
        }
        assert_eq!(SortBy::from_key("bogus"), None);
        assert_eq!(SortBy::default(), SortBy::Date);

        let f = Filters {
            starred: true,
            faces: false,
            movies: true,
            geotagged: false,
            text: String::new(),
        };
        assert_eq!(f.as_setting(), "1010");
        assert_eq!(Filters::from_setting(&f.as_setting()), f);
        assert_eq!(Filters::from_setting(""), Filters::default());
        assert_eq!(Filters::from_setting(" junk"), Filters::default());
        assert!(
            !Filters::from_setting("1111").is_none(),
            "全开也是「有过滤」"
        );
        // 搜索文本不进设置串:重启后开关保留、输入清空。
        let with_text = Filters {
            text: "海边".into(),
            ..f
        };
        assert_eq!(with_text.as_setting(), "1010");
        assert_eq!(Filters::from_setting("1010"), f);
    }
}
