use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct Store(pub Mutex<Connection>);

/// 数据目录(索引库 + 模型)。可用 MALIANG_DATA_DIR 覆盖(测试/便携运行)。
pub fn data_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("MALIANG_DATA_DIR") {
        if !v.is_empty() {
            let p = PathBuf::from(v);
            let _ = std::fs::create_dir_all(&p);
            return p;
        }
    }
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    let p = base.join("Relyto").join("Maliang");
    let _ = std::fs::create_dir_all(&p);
    p
}

/// 缓存目录(缩略图)。可用 MALIANG_CACHE_DIR 覆盖。
pub fn cache_dir() -> PathBuf {
    if let Some(v) = std::env::var_os("MALIANG_CACHE_DIR") {
        if !v.is_empty() {
            let p = PathBuf::from(v);
            let _ = std::fs::create_dir_all(&p);
            return p;
        }
    }
    let p = dirs::cache_dir()
        .unwrap_or_else(|| dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")))
        .join("Relyto")
        .join("Maliang");
    let _ = std::fs::create_dir_all(&p);
    p
}

const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS photos (
    id INTEGER PRIMARY KEY,
    path TEXT UNIQUE NOT NULL,
    parent TEXT NOT NULL,
    mtime INTEGER NOT NULL,
    size INTEGER NOT NULL,
    width INTEGER,
    height INTEGER,
    taken_utc TEXT,
    cam TEXT,
    rot INTEGER NOT NULL DEFAULT 0,
    star INTEGER NOT NULL DEFAULT 0,
    thumb_ok INTEGER NOT NULL DEFAULT 0,
    dhash INTEGER,
    baked INTEGER,
    lat REAL,
    lon REAL,
    xmp_imported INTEGER NOT NULL DEFAULT 0,
    uid64 INTEGER,
    -- P2-16 效果栈(`filters` 普通 TEXT 列,序列化 `name=value;`)。
    filters TEXT,
    -- P1-14 字符串池:相机型号走池化列(id = 池内数组下标)。
    -- cam 保留作兼容回退,cam_id 有效时以池为准。详见 `pool.rs` 模块文档。
    cam_id INTEGER,
    -- P2-17 媒体类型:0=图片,1=视频(按"是否视频"二分;
    -- 子类型对应不同解码路径,我方只有 Shell 一条,无须细分)。
    kind INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_photos_taken ON photos(taken_utc);
CREATE INDEX IF NOT EXISTS idx_photos_parent ON photos(parent);
CREATE INDEX IF NOT EXISTS idx_photos_cam_id ON photos(cam_id);
-- P1-14 字符串池。按列隔离(pool 名 = <表>_<列>),
-- id = 池内去重数组下标,**append-only、无引用计数**,回收靠 vacuum() 重建
-- (整池序列化 + 字符串入池只对现存行入池)。
CREATE TABLE IF NOT EXISTS string_pool (
    pool    TEXT    NOT NULL,   -- 池名,如 'photos_cam'(即 <表>_<列>)
    value_id INTEGER NOT NULL,   -- 池内下标 = 行里存的整数
    value   TEXT    NOT NULL,
    PRIMARY KEY (pool, value_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_pool_value ON string_pool(pool, value);
CREATE TABLE IF NOT EXISTS tags (
    id INTEGER PRIMARY KEY,
    name TEXT UNIQUE NOT NULL,
    color TEXT
);
CREATE TABLE IF NOT EXISTS photo_tags (
    photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id),
    UNIQUE(photo_id, tag_id)
);
CREATE TABLE IF NOT EXISTS faces (
    id INTEGER PRIMARY KEY,
    photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    x1 REAL, y1 REAL, x2 REAL, y2 REAL,
    person_id INTEGER,
    cluster INTEGER,
    score REAL,
    quality REAL,
    ignored INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS people (
    id INTEGER PRIMARY KEY,
    name TEXT UNIQUE NOT NULL
);
CREATE TABLE IF NOT EXISTS face_templates (
    face_id INTEGER PRIMARY KEY REFERENCES faces(id) ON DELETE CASCADE,
    dim INTEGER NOT NULL,
    vec BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS clips (
    photo_id INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
    dim INTEGER NOT NULL,
    vec BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS clips_cn (
    photo_id INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
    dim INTEGER NOT NULL,
    vec BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS watched (
    path TEXT PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS settings (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
-- 变更日志(P1-12):记录扫描/事件驱动的变更历史,可回溯。
-- 原版的变更日志只存在内存队列里,写盘 CSV 也只是瞬时快照 —— 不可回溯。
-- 这里做成**持久化 + 带时间戳 + 可查询**的版本(超出原版)。
CREATE TABLE IF NOT EXISTS changes (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,        -- Unix 毫秒
    path TEXT NOT NULL,         -- 变更的路径
    action TEXT NOT NULL,       -- added / modified / removed
    photo_id INTEGER            -- 关联的 photos.id(移除后可能为空)
);
CREATE INDEX IF NOT EXISTS idx_changes_ts ON changes(ts DESC);
CREATE INDEX IF NOT EXISTS idx_changes_path ON changes(path);
-- P2-15 持久化扫描队列:扫描根在启动扫描时登记、完成后删除;
-- 应用中途退出时残留的条目在下次启动自动续扫(原版扫描器是进程内
-- 持久化队列,进程重启后仍可继续;
-- 「扫描不阻塞 UI」由既有后台线程方案保证)。
CREATE TABLE IF NOT EXISTS scan_queue (
    id INTEGER PRIMARY KEY,
    root TEXT UNIQUE NOT NULL,
    scope TEXT NOT NULL DEFAULT '',
    enqueued_ts INTEGER NOT NULL     -- Unix 毫秒
);
-- 各档缩略图的完成状态(P1-9):对应原版的三张缓存表
-- (三张缩略图缓存表)。
-- tier 取值 0=Small(96) 1=Normal(256) 2=Big(512) 3=Preview(1024)。
-- photos.thumb_ok 保留给 Normal 档(向后兼容既有代码与测试)。
CREATE TABLE IF NOT EXISTS thumb_tiers (
    photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    tier INTEGER NOT NULL,
    PRIMARY KEY (photo_id, tier)
);
"#;

impl Store {
    pub fn open() -> Result<Store> {
        let path = data_dir().join("index.db");
        let conn = Connection::open(&path)?;
        conn.execute_batch(SCHEMA)?;
        // 旧库迁移:按需补列(列已存在时静默)
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN dhash INTEGER;");
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN baked INTEGER;");
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN star INTEGER NOT NULL DEFAULT 0;");
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN xmp_imported INTEGER NOT NULL DEFAULT 0;");
        let _ = conn.execute_batch("ALTER TABLE faces ADD COLUMN score REAL;");
        let _ = conn.execute_batch("ALTER TABLE faces ADD COLUMN quality REAL;");
        let _ = conn.execute_batch("ALTER TABLE faces ADD COLUMN ignored INTEGER NOT NULL DEFAULT 0;");
        let _ = conn.execute_batch("ALTER TABLE tags ADD COLUMN color TEXT;");
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN lat REAL;");
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN lon REAL;");
        // P1-11:uid64 持久唯一标识(64 位无符号整数列)。
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN uid64 INTEGER;");
        // P2-16:效果栈列(原版记录表里的 `filters` 列)。
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN filters TEXT;");
        // P2-17:视频标记列(NOT NULL 带默认值,旧行自动 0=图片)。
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN kind INTEGER NOT NULL DEFAULT 0;");
        // P1-14:相机型号池化列 + 池表。
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN cam_id INTEGER;");
        let _ = conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_photos_cam_id ON photos(cam_id);
             CREATE TABLE IF NOT EXISTS string_pool (
                 pool    TEXT    NOT NULL,
                 value_id INTEGER NOT NULL,
                 value   TEXT    NOT NULL,
                 PRIMARY KEY (pool, value_id)
             );
             CREATE UNIQUE INDEX IF NOT EXISTS idx_pool_value ON string_pool(pool, value);",
        );
        let store = Store(Mutex::new(conn));
        // P1-11:启动时补齐缺失的 uid64。
        // 扫描路径会在 `store_insert` 里写,但**外部导入**(WiFi 接收、拖入)
        // 或历史遗留的行可能没有 → 启动时兜底补一次,保证"库内每行都有标识"。
        let _ = store.backfill_uid64();
        // P1-14:把旧行残留的 cam TEXT 迁移进字符串池。
        let _ = store.backfill_cam_pool();
        Ok(store)
    }

    /// 内存库(测试专用,避免污染真实 index.db)。
    #[cfg(test)]
    pub fn open_mem() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store(Mutex::new(conn)))
    }

    pub fn upsert_watched(&self, p: &Path) -> Result<()> {
        self.0.lock().unwrap().execute("INSERT OR IGNORE INTO watched(path) VALUES(?1)", rusqlite::params![p.to_string_lossy()])?;
        Ok(())
    }

    /// 从监听列表移除文件夹(只取消监听,不删索引;对应文件夹管理器)。
    pub fn remove_watched(&self, p: &Path) -> Result<()> {
        self.0.lock().unwrap().execute(
            "DELETE FROM watched WHERE path=?1",
            rusqlite::params![p.to_string_lossy()],
        )?;
        Ok(())
    }

    /// 监听文件夹及其照片数(含子文件夹;文件夹管理器列表面板)。
    pub fn watched_stats(&self) -> Vec<(String, usize)> {
        let conn = self.0.lock().unwrap();
        let mut st = match conn.prepare("SELECT path FROM watched ORDER BY path") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let paths: Vec<String> = st
            .query_map([], |r| r.get::<_, String>(0))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();
        drop(st);
        paths
            .into_iter()
            .map(|p| {
                let n: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM photos WHERE path LIKE ?1 ESCAPE '\\'",
                        rusqlite::params![format!("{}%", like_escape(&p))],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                (p, n as usize)
            })
            .collect()
    }

    /// 读设置项(人脸算法版本等)。
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    pub fn get_setting(&self, k: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT v FROM settings WHERE k=?1",
                rusqlite::params![k],
                |r| r.get::<_, String>(0),
            )
            .ok()
    }

    /// 写设置项(upsert)。
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    pub fn set_setting(&self, k: &str, v: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT INTO settings(k,v) VALUES(?1,?2)
             ON CONFLICT(k) DO UPDATE SET v=?2",
            rusqlite::params![k, v],
        )?;
        Ok(())
    }

    // ===== P2-15 持久化扫描队列 =====

    /// 登记扫描根(已存在则忽略,scope 保留首次值)。
    ///
    /// 用途:① 扫描启动时登记,完成后 `dequeue_scan` 删除;
    /// ② 应用中断退出时残留条目在下次启动被 `pending_scans` 读出 → 续扫。
    pub fn enqueue_scan(&self, roots: &[std::path::PathBuf], scope: &str) -> Result<usize> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let conn = self.0.lock().unwrap();
        let mut n = 0;
        for r in roots {
            n += conn.execute(
                "INSERT INTO scan_queue(root, scope, enqueued_ts) VALUES(?1,?2,?3)
                 ON CONFLICT(root) DO NOTHING",
                rusqlite::params![r.to_string_lossy(), scope, now],
            )?;
        }
        Ok(n)
    }

    /// 读出全部待扫根(按入队先后),`(root, scope)`。
    pub fn pending_scans(&self) -> Vec<(String, String)> {
        let conn = self.0.lock().unwrap();
        let mut st = match conn.prepare("SELECT root, scope FROM scan_queue ORDER BY enqueued_ts, id")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.filter_map(|x| x.ok()).collect())
            .unwrap_or_default()
    }

    /// 扫描完成后移除登记(无论成功/取消 —— 取消是用户意志,不续扫)。
    pub fn dequeue_scan(&self, root: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .execute("DELETE FROM scan_queue WHERE root=?1", rusqlite::params![root])?;
        Ok(())
    }

    /// P2-16:读某张照片的效果栈(原版 `filters` 列)。
    pub fn get_filters(&self, photo_id: i64) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT filters FROM photos WHERE id=?1",
                rusqlite::params![photo_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
    }

    /// P2-16:写某张照片的效果栈(空串 = 无效果)。
    pub fn set_filters(&self, photo_id: i64, filters: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE photos SET filters=?2 WHERE id=?1",
            rusqlite::params![photo_id, filters],
        )?;
        Ok(())
    }

    /// P2-16:批量写效果栈(粘贴所有效果给多张)。
    pub fn set_filters_many(&self, ids: &[i64], filters: &str) -> Result<usize> {
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let mut n = 0;
        for id in ids {
            n += tx.execute(
                "UPDATE photos SET filters=?2 WHERE id=?1",
                rusqlite::params![*id, filters],
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// P2-16:批量读效果栈(复制所有效果时取源照片)。
    pub fn get_filters_many(&self, ids: &[i64]) -> Vec<(i64, String)> {
        let conn = self.0.lock().unwrap();
        let mut out = Vec::new();
        let Ok(mut st) = conn.prepare("SELECT filters FROM photos WHERE id=?1") else {
            return out;
        };
        for id in ids {
            if let Ok(Some(f)) =
                st.query_row(rusqlite::params![*id], |r| r.get::<_, Option<String>>(0))
            {
                out.push((*id, f));
            }
        }
        out
    }

    /// 备份索引库到指定文件(VACUUM INTO;原子、不锁库)。
    /// 目标已存在则先删除(SQLite 要求目标不存在)。
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        if dest.exists() {
            std::fs::remove_file(dest)?;
        }
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        self.0
            .lock()
            .unwrap()
            .execute("VACUUM INTO ?1", rusqlite::params![dest.to_string_lossy()])?;
        Ok(())
    }

    /// 压缩索引库(VACUUM:回收删除行占用的空间)。
    pub fn compact(&self) -> Result<()> {
        self.0.lock().unwrap().execute("VACUUM", [])?;
        Ok(())
    }

    /// 监听文件夹列表(预留:文件夹监视功能)。
    #[allow(dead_code)]
    pub fn list_watched(&self) -> Vec<PathBuf> {
        let conn = self.0.lock().unwrap();
        let mut st = match conn.prepare("SELECT path FROM watched") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = st.query_map([], |r| r.get::<_, String>(0));
        match rows {
            Ok(rows) => rows
                .filter_map(|r| r.ok())
                .map(PathBuf::from)
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 清理已缺失文件的索引行,返回被清理的路径(图库维护)。
    /// 必须连同 faces/face_templates/clips/photo_tags 一起删:SQLite 外键
    /// 级联默认关闭,只删 photos 会留下孤儿行(rowid 复用后会错挂到新照片)。
    pub fn mark_missing(&self) -> Result<Vec<String>> {        let conn = self.0.lock().unwrap();
        let mut st = conn.prepare("SELECT id, path FROM photos")?;
        let all: Vec<(i64, String)> = st
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        drop(st);
        let missing: Vec<(i64, String)> = all
            .into_iter()
            .filter(|(_, p)| !Path::new(p).exists())
            .collect();
        if missing.is_empty() {
            return Ok(Vec::new());
        }
        let tx = conn.unchecked_transaction()?;
        {
            let mut st_tags = tx.prepare_cached("DELETE FROM photo_tags WHERE photo_id=?1")?;
            let mut st_ft = tx.prepare_cached(
                "DELETE FROM face_templates WHERE face_id IN
                 (SELECT id FROM faces WHERE photo_id=?1)",
            )?;
            let mut st_faces = tx.prepare_cached("DELETE FROM faces WHERE photo_id=?1")?;
            let mut st_clips = tx.prepare_cached("DELETE FROM clips WHERE photo_id=?1")?;
            let mut st_photos = tx.prepare_cached("DELETE FROM photos WHERE id=?1")?;
            for (id, _) in &missing {
                st_tags.execute(rusqlite::params![id])?;
                st_clips.execute(rusqlite::params![id])?;
                st_ft.execute(rusqlite::params![id])?;
                st_faces.execute(rusqlite::params![id])?;
                st_photos.execute(rusqlite::params![id])?;
            }
        }
        tx.commit()?;
        Ok(missing.into_iter().map(|(_, p)| p).collect())
    }

    // ===== P1-12 变更日志 =====

    /// 记录一条变更(added / modified / removed)。
    pub fn log_change(&self, path: &str, action: &str, photo_id: Option<i64>) -> Result<()> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.0.lock().unwrap().execute(
            "INSERT INTO changes(ts,path,action,photo_id) VALUES(?1,?2,?3,?4)",
            rusqlite::params![ts, path, action, photo_id],
        )?;
        Ok(())
    }

    /// 读取最近的变更记录(倒序)。
    pub fn recent_changes(&self, limit: usize) -> Vec<(i64, String, String)> {
        let conn = self.0.lock().unwrap();
        let mut st = match conn.prepare(
            "SELECT ts, path, action FROM changes ORDER BY ts DESC, id DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map([limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// 清空变更日志。
    pub fn clear_changes(&self) -> Result<()> {
        self.0.lock().unwrap().execute("DELETE FROM changes", [])?;
        Ok(())
    }

    /// 变更日志总条数(状态栏展示)。
    pub fn change_count(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM changes", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }

    // ===== P1-11 uid64 =====

    /// 为尚未分配 uid64 的照片补一个。
    ///
    /// 算法:对 `path + size + mtime` 做 FNV-1a 64 位哈希。
    /// 取这三个字段是刻意的 —— 路径保证唯一性,size/mtime 让内容变化后
    /// 标识也跟着变(与原版 `uid64` 紧邻 `fdbhash`/`backuphash` 的语义一致:
    /// 标识与文件状态绑定)。纯路径哈希会导致"改了内容还是同一个 id",
    /// 纯内容哈希会导致"改名就换 id",两者都不理想。
    ///
    /// 返回新分配的条数。
    pub fn backfill_uid64(&self) -> Result<usize> {
        let rows: Vec<(i64, String, i64, i64)> = {
            let conn = self.0.lock().unwrap();
            let mut st = match conn
                .prepare("SELECT id, path, size, mtime FROM photos WHERE uid64 IS NULL")
            {
                Ok(s) => s,
                Err(_) => return Ok(0),
            };
            st.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        };
        if rows.is_empty() {
            return Ok(0);
        }
        let conn = self.0.lock().unwrap();
        for (id, path, size, mtime) in &rows {
            let u = fnv1a64(&format!("{path}|{size}|{mtime}"));
            conn.execute(
                "UPDATE photos SET uid64=?2 WHERE id=?1",
                rusqlite::params![id, u as i64],
            )?;
        }
        Ok(rows.len())
    }

    /// 按 uid64 查照片(供外部工具按持久标识引用)。
    pub fn photo_by_uid64(&self, uid: u64) -> Option<i64> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT id FROM photos WHERE uid64=?1",
                rusqlite::params![uid as i64],
                |r| r.get(0),
            )
            .ok()
    }

    // ===== P1-14 字符串池 =====

    /// 把一个字符串驻留进指定池,返回池 id。
    ///
    /// 语义对齐字符串驻留池(见 `pool.rs` 模块文档):
    /// 同串恒返回同 id;新串 append 分配新 id(= 当前长度)。
    ///
    /// 落盘时**先查 `string_pool` 的唯一索引**,命中即复用,
    /// 未命中才分配新 id 并插入 —— 这就是原版的"查重"。
    pub fn intern(&self, pool: &str, value: &str) -> Result<i64> {
        let conn = self.0.lock().unwrap();
        // 查重(原版字符串查重哈希的链上精确比较)
        if let Ok(id) = conn.query_row(
            "SELECT value_id FROM string_pool WHERE pool=?1 AND value=?2",
            rusqlite::params![pool, value],
            |r| r.get::<_, i64>(0),
        ) {
            return Ok(id);
        }
        // append 分配:下一个可用下标(原版字符串查重哈希)
        let next: i64 = conn.query_row(
            "SELECT COALESCE(MAX(value_id) + 1, 0) FROM string_pool WHERE pool=?1",
            rusqlite::params![pool],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO string_pool(pool, value_id, value) VALUES(?1, ?2, ?3)",
            rusqlite::params![pool, next, value],
        )?;
        Ok(next)
    }

    /// 批量驻留:先用内存 `PoolSet` 去重,再**每个唯一值只发一次 SQL**。
    ///
    /// 返回与 `items` 等长同序的 `(池名, 池 id)`。
    ///
    /// 这正是原版「先入内存池、落盘时才整体重算」的意义:
    /// 一万行照片若只有 5 种相机型号,内存池把 1 万次查询压成 5 次。
    pub fn intern_batch(&self, items: &[(String, String)]) -> Result<Vec<(String, i64)>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        // ① 内存池去重,按首次出现顺序收集唯一 (池, 值)
        let mut set = crate::pool::PoolSet::new();
        let mut uniq: Vec<(&str, &str)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (p, v) in items {
            if seen.insert((p.as_str(), v.as_str())) {
                uniq.push((p.as_str(), v.as_str()));
                set.intern(p, v);
            }
        }
        // ② 每个唯一值一次事务性驻留
        let conn = self.0.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        let mut db_ids: std::collections::HashMap<(&str, &str), i64> =
            std::collections::HashMap::new();
        for (p, v) in &uniq {
            let id = Self::intern_tx(&tx, p, v)?;
            db_ids.insert((*p, *v), id);
        }
        tx.commit()?;
        drop(conn);
        // ③ 展开回原始顺序
        let mut out = Vec::with_capacity(items.len());
        for (p, v) in items {
            let id = *db_ids
                .get(&(p.as_str(), v.as_str()))
                .ok_or_else(|| anyhow::anyhow!("intern_batch:未驻留的键 {p}/{v}"))?;
            out.push((p.clone(), id));
        }
        Ok(out)
    }

    /// 借用已有连接/事务做单条驻留(供 `intern_batch` 在事务内复用,避免二次加锁)。
    fn intern_tx(conn: &Connection, pool: &str, value: &str) -> Result<i64> {
        if let Ok(id) = conn.query_row(
            "SELECT value_id FROM string_pool WHERE pool=?1 AND value=?2",
            rusqlite::params![pool, value],
            |r| r.get::<_, i64>(0),
        ) {
            return Ok(id);
        }
        let next: i64 = conn.query_row(
            "SELECT COALESCE(MAX(value_id) + 1, 0) FROM string_pool WHERE pool=?1",
            rusqlite::params![pool],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO string_pool(pool, value_id, value) VALUES(?1, ?2, ?3)",
            rusqlite::params![pool, next, value],
        )?;
        Ok(next)
    }

    /// 按 id 取回池中字符串。原版池内按 id 取串。
    pub fn pooled_value(&self, pool: &str, id: i64) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM string_pool WHERE pool=?1 AND value_id=?2",
                rusqlite::params![pool, id],
                |r| r.get::<_, String>(0),
            )
            .ok()
    }

    /// 启动时把**旧行**里还留在 `cam` TEXT 列的相机型号补进池。
    ///
    /// P1-14 之前入库的行没有 `cam_id`;`timeline::load_all` 的
    /// `LEFT JOIN` 会回退显示 `cam`,但那样这些行就享受不到池化。
    /// 返回被迁移的行数。
    pub fn backfill_cam_pool(&self) -> Result<usize> {
        let pending: Vec<(i64, String)> = {
            let conn = self.0.lock().unwrap();
            let mut st = match conn
                .prepare("SELECT id, cam FROM photos WHERE cam_id IS NULL AND cam IS NOT NULL AND cam<>''")
            {
                Ok(s) => s,
                Err(_) => return Ok(0),
            };
            let rows: Vec<(i64, String)> = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .ok()
                .map(|rows| rows.filter_map(|x| x.ok()).collect())
                .unwrap_or_default();
            rows
        };
        if pending.is_empty() {
            return Ok(0);
        }
        let mut migrated = 0usize;
        for (id, cam) in &pending {
            let cid = self.intern(crate::timeline::CAM_POOL, cam)?;
            let conn = self.0.lock().unwrap();
            let n = conn.execute(
                "UPDATE photos SET cam_id=?2, cam=NULL WHERE id=?1",
                rusqlite::params![id, cid],
            )?;
            migrated += n;
        }
        Ok(migrated)
    }

    /// 列出全部相机型号及各自照片数(按张数降序)。
    ///
    /// 走 `string_pool` 的 LEFT JOIN,池未命中时回退旧 `cam` 列
    /// —— 与 `timeline::load_all` 同一套兼容策略。
    pub fn cams(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.0.lock().unwrap();
        let mut st = conn.prepare(
            // 别名 `cam` 是相关列的表达式,不能直接 GROUP BY 它 ——
            // 必须 GROUP BY 表达式本身,否则 SQLite 会解析到错误来源。
            "SELECT COALESCE(sp.value, p.cam) AS camname, COUNT(*) AS n
             FROM photos p
             LEFT JOIN string_pool sp ON sp.pool = ?1 AND sp.value_id = p.cam_id
             WHERE COALESCE(sp.value, p.cam) IS NOT NULL AND COALESCE(sp.value, p.cam) <> ''
             GROUP BY COALESCE(sp.value, p.cam)
             ORDER BY n DESC, camname",
        )?;
        let rows: Vec<(String, i64)> = st
            .query_map(rusqlite::params![crate::timeline::CAM_POOL], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .filter_map(|x| x.ok())
            .collect();
        Ok(rows)
    }

    /// 池统计:`(池名, 条数, 业务表引用行数, 去重率)`。
    /// 去重率 = 条数 / 引用行数(越小越省)。
    pub fn pool_stats(&self) -> Result<Vec<(String, i64, i64)>> {
        let conn = self.0.lock().unwrap();
        let mut st = conn.prepare(
            // 池条目数与引用行数分别聚合再 join ——
            // 不能写成"FROM string_pool p + 相关子查询里引用 p.pool":
            // 子查询的名字空间里看不到外层别名,SQLite 会报
            // "no such column"。分两步聚合最直白,也不产生行放大。
            "SELECT k.pool, k.n, COALESCE(r.refs, 0) AS refs
             FROM (SELECT pool, COUNT(*) AS n FROM string_pool GROUP BY pool) k
             LEFT JOIN (
                 SELECT sp.pool AS pool, COUNT(*) AS refs
                 FROM photos ph
                 JOIN string_pool sp ON sp.pool = ?1 AND sp.value_id = ph.cam_id
                 GROUP BY sp.pool
             ) r ON r.pool = k.pool
             ORDER BY k.pool",
        )?;
        let rows = st
            .query_map(rusqlite::params![crate::timeline::CAM_POOL], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
            })?
            .filter_map(|x| x.ok())
            .collect();
        Ok(rows)
    }

    /// 把库里各池的**当前存活 id 集合**读出来,交给 `pool::vacuum_plan` 预估收益。
    ///
    /// 判断依据:某 id 是否还被现存行引用
    /// (字符串入池只对当前行数组里的值入池)。
    pub fn live_pool_ids(&self) -> Result<std::collections::HashMap<String, crate::pool::HashSet>> {
        let conn = self.0.lock().unwrap();
        let mut st = conn.prepare(
            "SELECT DISTINCT cam_id FROM photos WHERE cam_id IS NOT NULL",
        )?;
        let ids: Vec<i64> = st
            .query_map([], |r| r.get::<_, i64>(0))?
            .filter_map(|x| x.ok())
            .collect();
        let mut out = std::collections::HashMap::new();
        out.insert(crate::timeline::CAM_POOL.to_string(), ids.into_iter().collect());
        Ok(out)
    }

    /// 把当前 `string_pool` 整表读进内存池(供 UI 预估 / 诊断)。
    pub fn load_pool_set(&self) -> Result<crate::pool::PoolSet> {
        let conn = self.0.lock().unwrap();
        let mut st = conn.prepare("SELECT pool, value_id, value FROM string_pool ORDER BY pool, value_id")?;
        let rows: Vec<(String, i64, String)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .filter_map(|x| x.ok())
            .collect();
        drop(st);
        drop(conn);
        let mut set = crate::pool::PoolSet::new();
        for (pool, id, val) in &rows {
            let got = set.intern(pool, val);
            // 自检:append-only 契约 —— 池 id 必须等于库里的 value_id。
            // 这不是多余断言,而是"id = 数组下标"这条原版语义
            // (字符串查重哈希) 的运行时护栏:
            // 若库里出现空洞/乱序,说明有别处绕过池直接写了 value_id。
            debug_assert_eq!(got, *id, "池 id 应等于 value_id(append-only 契约)");
            debug_assert_eq!(set.get(pool, got), Some(val.as_str()));
        }
        Ok(set)
    }

    /// 写时重建池("无引用计数 + 每轮保存重算整个池")。
    ///
    /// 只保留仍被业务表引用的 id,**按旧 id 升序重新编号**并同步 UPDATE 引用列。
    /// 返回 `(删除的孤儿条数, 被重映射更新的 photos 行数)`。
    ///
    /// 实现要点:新池先写进临时表 `string_pool_new`,再整体切换 ——
    /// 边删边插会撞主键,且中途失败会留下半重建的池。
    pub fn vacuum_pool(&self, pool: &str) -> Result<(usize, usize)> {
        let conn = self.0.lock().unwrap();
        let tx = conn.unchecked_transaction()?;

        // 存活集:仍被 photos 引用的 id,按升序(保证新编号可复现)
        let live: Vec<i64> = {
            let mut st = tx.prepare(
                "SELECT DISTINCT cam_id FROM photos WHERE cam_id IS NOT NULL ORDER BY cam_id",
            )?;
            let rows: Vec<i64> = st
                .query_map([], |r| r.get::<_, i64>(0))?
                .filter_map(|x| x.ok())
                .collect();
            rows
        };
        // 全量池条目,按旧 id 升序
        let all: Vec<(i64, String)> = {
            let mut st =
                tx.prepare("SELECT value_id, value FROM string_pool WHERE pool=?1 ORDER BY value_id")?;
            let rows: Vec<(i64, String)> = st
                .query_map(rusqlite::params![pool], |r| Ok((r.get(0)?, r.get(1)?)))?
                .filter_map(|x| x.ok())
                .collect();
            rows
        };
        // 内存池先算重映射 —— 这是原版的实际路径:
        // 内存池被重写(整池序列化),
        // 再整体落盘。SQL 侧只是这个结果的持久化。
        let mut mem = crate::pool::ColumnPool::new();
        for (_, v) in &all {
            mem.intern(v);
        }
        if mem.is_empty() {
            // 池本来就是空的:无需重建(对齐池序列化的 do-while,count=0 时不进循环)
            return Ok((0, 0));
        }
        let live_set: crate::pool::HashSet = live.iter().copied().collect();
        let remap = mem.vacuum(&live_set);
        let orphan_total = all.len().saturating_sub(remap.len());

        // 业务表引用(旧号 → 新号)。先全部改完再换池,避免中途态。
        let mut updated = 0usize;
        {
            let mut up = tx.prepare("UPDATE photos SET cam_id=?2 WHERE cam_id=?1")?;
            for (old_id, new_id) in remap.iter().filter(|(o, n)| o != n) {
                updated += up.execute(rusqlite::params![old_id, new_id])?;
            }
        }

        // 换池:新内容写临时表,再整体切换(边删边插会撞主键)
        tx.execute_batch("DROP TABLE IF EXISTS string_pool_new;")?;
        tx.execute_batch(
            "CREATE TABLE string_pool_new (
                 pool    TEXT    NOT NULL,
                 value_id INTEGER NOT NULL,
                 value   TEXT    NOT NULL,
                 PRIMARY KEY (pool, value_id)
             );",
        )?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO string_pool_new(pool, value_id, value) VALUES(?1,?2,?3)",
            )?;
            for (new_id, val) in mem.values().iter().enumerate() {
                ins.execute(rusqlite::params![pool, new_id as i64, val])?;
            }
        }
        // execute 只接受单条语句 —— 这里用 params 删本池
        tx.execute(
            "DELETE FROM string_pool WHERE pool=?1",
            rusqlite::params![pool],
        )?;
        tx.execute_batch(
            "INSERT INTO string_pool(pool, value_id, value)
             SELECT pool, value_id, value FROM string_pool_new;",
        )?;
        tx.execute_batch("DROP TABLE string_pool_new;")?;
        tx.commit()?;
        Ok((orphan_total, updated))
    }

    /// 一次性重建**所有**池(UI 的"重建池"按钮走这条)。
    ///
    /// 与逐池 `vacuum_pool` 的区别:先用内存 `PoolSet` 统一算重映射
    /// (`PoolSet::vacuum_all`),再一次事务落盘 —— 多池时省 N−1 次事务。
    /// 返回 `{池名 → (回收孤儿数, 重映射行数)}`。
    pub fn vacuum_all_pools(&self) -> Result<std::collections::BTreeMap<String, (usize, usize)>> {
        let live = self.live_pool_ids()?;
        let mut set = self.load_pool_set()?;
        let remaps = set.vacuum_all(&live); // 纯内存,不动 DB
        let mut out = std::collections::BTreeMap::new();
        for (pool, remap) in remaps {
            // 该池原有条数 - 存活条数 = 可回收孤儿数
            let before: i64 = {
                let conn = self.0.lock().unwrap();
                conn.query_row(
                    "SELECT COUNT(*) FROM string_pool WHERE pool=?1",
                    rusqlite::params![&pool],
                    |r| r.get(0),
                )
                .unwrap_or(0)
            };
            #[allow(clippy::cast_sign_loss)]
            let orphans = (before - remap.len() as i64).max(0) as usize;
            let (o, m) = self.vacuum_pool(&pool)?;
            out.insert(pool, (o.max(orphans.min(o)), m));
        }
        Ok(out)
    }


    // ===== P1-9 多档缩略图 =====

    /// 标记某张照片的某档缩略图已生成。
    pub fn mark_tier_done(&self, photo_id: i64, tier: u8) -> Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT OR IGNORE INTO thumb_tiers(photo_id,tier) VALUES(?1,?2)",
            rusqlite::params![photo_id, tier as i64],
        )?;
        Ok(())
    }

    /// 取该档**尚未生成**的照片 id + 路径(供后台 worker 批量补齐)。
    pub fn missing_tier(&self, tier: u8, limit: usize) -> Vec<(i64, String)> {
        let conn = self.0.lock().unwrap();
        // Normal 档(1)沿用 photos.thumb_ok 作为权威标记,其余走 thumb_tiers 表。
        let (sql, params): (&str, Vec<Box<dyn rusqlite::ToSql>>) = if tier == 1 {
            (
                "SELECT id, path FROM photos WHERE thumb_ok=0 LIMIT ?1",
                vec![Box::new(limit as i64)],
            )
        } else {
            (
                "SELECT p.id, p.path FROM photos p
                 LEFT JOIN thumb_tiers t ON t.photo_id=p.id AND t.tier=?2
                 WHERE t.photo_id IS NULL LIMIT ?1",
                vec![Box::new(limit as i64), Box::new(tier as i64)],
            )
        };
        let mut st = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let params_ref: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        st.query_map(params_ref.as_slice(), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .ok()
        .map(|it| it.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// 该档已生成的照片数(状态栏展示)。
    pub fn tier_count(&self, tier: u8) -> usize {
        if tier == 1 {
            self.0
                .lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM photos WHERE thumb_ok=1", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap_or(0) as usize
        } else {
            self.0
                .lock()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM thumb_tiers WHERE tier=?1",
                    [tier as i64],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0) as usize
        }
    }
}

/// FNV-1a 64 位哈希(生成 uid64 用;无需外部依赖,确定性跨平台)。
pub fn fnv1a64(s: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// LIKE 通配符转义(\、%、_ 在路径名里出现时避免被当作模式符)。
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

/// 扫描根目录候选(按盘符枚举)。
///
/// 卷类型分流:`GetLogicalDriveStrings` + `GetDriveTypeA` 分流。
/// 关键差异:**网络盘(DRIVE_REMOTE)不扫描** —— 原版明确不扫,我方原先 C:\~Z:\ 全枚举
/// 会把映射的网络盘也拖进来,造成"扫描整个图库"长时间卡死。
pub fn default_roots() -> Vec<PathBuf> {
    if cfg!(windows) {
        let mut v = Vec::new();
        for c in b'C'..=b'Z' {
            let root = PathBuf::from(format!("{}:\\", c as char));
            if !std::fs::metadata(&root).map(|m| m.is_dir()).unwrap_or(false) {
                continue;
            }
            match crate::win::drive_type(&root) {
                // 已知卷类型:按原版策略决定收不收(网络盘/光驱直接排除)。
                Some(t) => {
                    if !t.scannable() {
                        continue;
                    }
                    v.push(root);
                }
                // 取不到卷类型(非 Windows 等):退回"目录存在即可"。
                None => v.push(root),
            }
        }
        v
    } else {
        let mut v = Vec::new();
        if let Some(h) = dirs::home_dir() {
            v.push(h.join("Pictures"));
            v.push(h);
        }
        if !v[0].exists() {
            v.remove(0);
        }
        v
    }
}

/// 列出所有可扫描的卷(含盘符),供 UI 展示"扫描整个图库"会覆盖哪些卷。
/// 与 `default_roots` 同源,但不做目录存在性检查(用于说明文案)。
pub fn scannable_volumes() -> Vec<(String, bool)> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let mut v = Vec::new();
    for c in b'C'..=b'Z' {
        let root = format!("{}:\\", c as char);
        let p = PathBuf::from(&root);
        if !std::fs::metadata(&p).map(|m| m.is_dir()).unwrap_or(false) {
            continue;
        }
        let (kind, scannable) = match crate::win::drive_type(&p) {
            Some(crate::win::DriveType::Fixed) => ("固定盘", true),
            Some(crate::win::DriveType::Removable) => ("可移动", true),
            Some(crate::win::DriveType::RamDisk) => ("内存盘", true),
            Some(crate::win::DriveType::Remote) => ("网络盘", false),
            Some(crate::win::DriveType::CdRom) => ("光驱", false),
            Some(crate::win::DriveType::NoRootDir) => ("无根目录", false),
            _ => ("未知", false),
        };
        v.push((format!("{root} ({kind})"), scannable));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P0 修复固化:新库 photos 表必须带 star 列
    /// (scan.rs INSERT / timeline.rs SELECT·UPDATE 都依赖它;旧库靠 ALTER 迁移补)。
    /// settings 读写(人脸算法版本迁移用)。
    #[test]
    fn settings_roundtrip() {
        let store = Store::open_mem().unwrap();
        assert_eq!(store.get_setting("face_algo"), None);
        store.set_setting("face_algo", "2").unwrap();
        assert_eq!(store.get_setting("face_algo").as_deref(), Some("2"));
        store.set_setting("face_algo", "3").unwrap(); // upsert 覆盖
        assert_eq!(store.get_setting("face_algo").as_deref(), Some("3"));
    }

    /// P2-16:效果栈存 `photos.filters` 列(普通 TEXT 列),
    /// 支持单张读写、批量写(粘贴所有效果)、批量读(复制所有效果)。
    #[test]
    fn filters_roundtrip_and_batch() {
        let store = Store::open_mem().unwrap();
        let conn = store.0.lock().unwrap();
        for i in 1..=3i64 {
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,?3,0,0)",
                rusqlite::params![i, format!("/p/{i}.jpg"), "/p"],
            )
            .unwrap();
        }
        drop(conn);

        // 无效果 → None
        assert_eq!(store.get_filters(1), None);
        let s = "enhance=brightness=10.0000,contrast=20.0000;";
        store.set_filters(1, &s).unwrap();
        assert_eq!(store.get_filters(1).as_deref(), Some(s));

        // 批量写(粘贴所有效果)
        let n = store.set_filters_many(&[2, 3], &s).unwrap();
        assert_eq!(n, 2);
        assert_eq!(store.get_filters(2).as_deref(), Some(s));
        assert_eq!(store.get_filters(3).as_deref(), Some(s));

        // 批量读(复制所有效果取源)
        let got = store.get_filters_many(&[1, 2, 3]);
        assert_eq!(got.len(), 3);
        assert!(got.iter().all(|(_, f)| f == s));
    }

    /// watched_stats 统计含子文件夹,remove_watched 只摘监听不删索引。
    #[test]
    fn watched_stats_and_remove() {
        let store = Store::open_mem().unwrap();
        store.upsert_watched(Path::new("/data/pics")).unwrap();
        {
            let c = store.0.lock().unwrap();
            c.execute(
                "INSERT INTO photos(path,parent,mtime,size) VALUES('/data/pics/a.jpg','/data/pics',0,0)",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO photos(path,parent,mtime,size) VALUES('/data/pics/sub/b.jpg','/data/pics/sub',0,0)",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO photos(path,parent,mtime,size) VALUES('/other/c.jpg','/other',0,0)",
                [],
            )
            .unwrap();
        }
        let stats = store.watched_stats();
        assert_eq!(stats, vec![("/data/pics".to_string(), 2)], "应含子文件夹,不含 /other");
        store.remove_watched(Path::new("/data/pics")).unwrap();
        assert!(store.watched_stats().is_empty());
        // 索引不受影响
        let n: i64 = store
            .0
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
    }

    /// 备份(VACUUM INTO)产出的库可独立打开且数据一致。
    #[test]
    fn backup_to_produces_valid_db() {
        let store = Store::open_mem().unwrap();
        store.set_setting("k", "v").unwrap();
        let dest = std::env::temp_dir().join(format!("rk-backup-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dest);
        store.backup_to(&dest).unwrap();
        assert!(dest.exists());
        let c2 = Connection::open(&dest).unwrap();
        let v: String = c2
            .query_row("SELECT v FROM settings WHERE k='k'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "v");
        drop(c2);
        let _ = std::fs::remove_file(&dest);
    }

    /// mark_missing 必须连 faces/face_templates/clips/photo_tags
    /// 一起清(外键级联默认关闭,只删 photos 会留孤儿行)。
    #[test]
    fn mark_missing_clears_related_rows() {
        let store = Store::open_mem().unwrap();
        let keep = format!("{}/keep.jpg", std::env::temp_dir().display());
        let gone = format!("{}/definitely-not-here-{}.jpg", std::env::temp_dir().display(), std::process::id());
        std::fs::write(&keep, b"x").unwrap();
        {
            let c = store.0.lock().unwrap();
            c.execute(
                "INSERT INTO photos(path,parent,mtime,size) VALUES(?1,'/',0,0)",
                rusqlite::params![keep],
            )
            .unwrap();
            c.execute(
                "INSERT INTO photos(path,parent,mtime,size) VALUES(?1,'/',0,0)",
                rusqlite::params![gone],
            )
            .unwrap();
            let gone_id: i64 = c
                .query_row("SELECT id FROM photos WHERE path=?1", rusqlite::params![gone], |r| r.get(0))
                .unwrap();
            // 给"已缺失"那张挂上全部关联行
            c.execute(
                "INSERT INTO faces(photo_id,x1,y1,x2,y2) VALUES(?1,0,0,1,1)",
                rusqlite::params![gone_id],
            )
            .unwrap();
            let face_id = c.last_insert_rowid();
            c.execute(
                "INSERT INTO face_templates(face_id,dim,vec) VALUES(?1,1,X'00')",
                rusqlite::params![face_id],
            )
            .unwrap();
            c.execute(
                "INSERT INTO clips(photo_id,dim,vec) VALUES(?1,1,X'00')",
                rusqlite::params![gone_id],
            )
            .unwrap();
            c.execute("INSERT INTO tags(name) VALUES('t')", []).unwrap();
            c.execute(
                "INSERT INTO photo_tags(photo_id,tag_id)
                 VALUES(?1,(SELECT id FROM tags WHERE name='t'))",
                rusqlite::params![gone_id],
            )
            .unwrap();
        }
        let removed = store.mark_missing().unwrap();
        assert_eq!(removed, vec![gone.clone()]);
        let c = store.0.lock().unwrap();
        for (tbl, col) in [
            ("photos", "path"),
            ("faces", "photo_id"),
            ("face_templates", "face_id"),
            ("clips", "photo_id"),
            ("photo_tags", "photo_id"),
        ] {
            let q = format!("SELECT COUNT(*) FROM {tbl} WHERE {col} IN (SELECT id FROM photos WHERE path=?1)");
            let n: i64 = c.query_row(&q, rusqlite::params![gone], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{tbl} 应为空");
        }
        let n_photos: i64 = c.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0)).unwrap();
        assert_eq!(n_photos, 1, "存在的照片必须保留");
    }

    // ===== P1-14 字符串池 =====

    const POOL: &str = "photos_cam";

    fn add_photo(store: &Store, id: i64, cam_id: Option<i64>) {
        let c = store.0.lock().unwrap();
        c.execute(
            "INSERT INTO photos(id,path,parent,mtime,size,cam_id) VALUES(?1,?2,'/p',0,0,?3)",
            rusqlite::params![id, format!("/p/{id}.jpg"), cam_id],
        )
        .unwrap();
    }

    #[test]
    fn intern_dedups_and_ids_are_append_only() {
        let store = Store::open_mem().unwrap();
        assert_eq!(store.intern(POOL, "Nikon D850").unwrap(), 0);
        assert_eq!(store.intern(POOL, "Canon R5").unwrap(), 1);
        // 同串恒返回同 id(对齐原版字符串查重哈希的查重)
        assert_eq!(store.intern(POOL, "Nikon D850").unwrap(), 0);
        assert_eq!(store.pooled_value(POOL, 0).as_deref(), Some("Nikon D850"));
        assert_eq!(store.pooled_value(POOL, 99), None);
    }

    #[test]
    fn pools_are_isolated_per_column() {
        let store = Store::open_mem().unwrap();
        assert_eq!(store.intern(POOL, "A").unwrap(), 0);
        // 不同池的 id 空间独立:都从 0 开始(每列一个池)
        assert_eq!(store.intern("photos_parent", "B").unwrap(), 0);
        assert_eq!(store.pooled_value("photos_parent", 0).as_deref(), Some("B"));
    }

    #[test]
    fn intern_batch_matches_per_item_intern() {
        let store = Store::open_mem().unwrap();
        let items: Vec<(String, String)> = vec![
            (POOL.into(), "A".into()),
            (POOL.into(), "B".into()),
            (POOL.into(), "A".into()), // 批内重复
            (POOL.into(), "C".into()),
        ];
        let got = store.intern_batch(&items).unwrap();
        // 同序同长
        assert_eq!(got.len(), items.len());
        // A 的两次必须同 id,B/C 依次 append
        assert_eq!(got[0].1, got[2].1);
        assert_eq!(got[0].1, 0);
        assert_eq!(got[1].1, 1);
        assert_eq!(got[3].1, 2);
        // 池里只有 3 条(去重生效)
        assert_eq!(store.pooled_value(POOL, 2).as_deref(), Some("C"));
    }

    #[test]
    fn intern_batch_empty_is_noop() {
        let store = Store::open_mem().unwrap();
        assert!(store.intern_batch(&[]).unwrap().is_empty());
    }

    #[test]
    fn backfill_cam_pool_migrates_legacy_rows() {
        let store = Store::open_mem().unwrap();
        {
            let c = store.0.lock().unwrap();
            // 模拟 P1-14 之前入库的行:cam 有值、cam_id 为 NULL
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,cam) VALUES(1,'/p/a.jpg','/p',0,0,'Nikon D850')",
                [],
            )
            .unwrap();
        }
        assert_eq!(store.backfill_cam_pool().unwrap(), 1);
        let c = store.0.lock().unwrap();
        let cid: Option<i64> = c
            .query_row("SELECT cam_id FROM photos WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert!(cid.is_some(), "应已迁移到池");
        let cam: Option<String> = c
            .query_row("SELECT cam FROM photos WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert!(cam.is_none(), "迁移后应清空旧 TEXT 列");
    }

    #[test]
    fn backfill_cam_pool_is_idempotent() {
        let store = Store::open_mem().unwrap();
        {
            let c = store.0.lock().unwrap();
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,cam) VALUES(1,'/p/a.jpg','/p',0,0,'X')",
                [],
            )
            .unwrap();
        }
        assert_eq!(store.backfill_cam_pool().unwrap(), 1);
        assert_eq!(store.backfill_cam_pool().unwrap(), 0, "第二次无事可做");
    }

    #[test]
    fn vacuum_pool_drops_orphans_and_remaps_live_ids() {
        let store = Store::open_mem().unwrap();
        // 池里造 3 条:A=0(存活) B=1(孤儿) C=2(存活)
        let a = store.intern(POOL, "A").unwrap();
        let _b = store.intern(POOL, "B").unwrap();
        let c = store.intern(POOL, "C").unwrap();
        add_photo(&store, 1, Some(a));
        add_photo(&store, 2, Some(c));
        assert_eq!(store.live_pool_ids().unwrap()[POOL].len(), 2);

        let (orphans, remapped) = store.vacuum_pool(POOL).unwrap();
        assert_eq!(orphans, 1, "B 是孤儿");
        assert_eq!(remapped, 1, "C 的 id 2 → 1,只有 1 行需改");
        // 池里只剩 2 条,且按旧 id 升序重编号
        assert_eq!(store.pooled_value(POOL, 0).as_deref(), Some("A"));
        assert_eq!(store.pooled_value(POOL, 1).as_deref(), Some("C"));
        assert_eq!(store.pooled_value(POOL, 2), None);
        // 业务表引用已同步
        let conn = store.0.lock().unwrap();
        let ids: Vec<i64> = {
            let mut st = conn.prepare("SELECT cam_id FROM photos ORDER BY id").unwrap();
            let v = st
                .query_map([], |r| r.get::<_, i64>(0))
                .unwrap()
                .filter_map(|x| x.ok())
                .collect();
            v
        };
        assert_eq!(ids, vec![0, 1]);
    }

    #[test]
    fn vacuum_pool_on_empty_pool_is_noop() {
        let store = Store::open_mem().unwrap();
        assert_eq!(store.vacuum_pool(POOL).unwrap(), (0, 0));
    }

    #[test]
    fn vacuum_all_pools_reports_per_pool() {
        let store = Store::open_mem().unwrap();
        let a = store.intern(POOL, "A").unwrap();
        let _b = store.intern(POOL, "B").unwrap();
        add_photo(&store, 1, Some(a));
        let rep = store.vacuum_all_pools().unwrap();
        assert_eq!(rep[POOL].0, 1, "B 被回收");
    }

    #[test]
    fn vacuum_plan_predicts_without_mutating() {
        let store = Store::open_mem().unwrap();
        let a = store.intern(POOL, "A").unwrap();
        let _b = store.intern(POOL, "B").unwrap();
        add_photo(&store, 1, Some(a));
        let set = store.load_pool_set().unwrap();
        let live = store.live_pool_ids().unwrap();
        let plan = crate::pool::vacuum_plan(&set, &live);
        assert_eq!(plan, vec![(POOL.to_string(), 2, 1)]);
        // 纯函数:池未被改动
        assert_eq!(store.pooled_value(POOL, 1).as_deref(), Some("B"));
    }

    #[test]
    fn load_pool_set_roundtrips_values_and_ids() {
        let store = Store::open_mem().unwrap();
        for s in ["A", "B", "C"] {
            store.intern(POOL, s).unwrap();
        }
        let set = store.load_pool_set().unwrap();
        assert_eq!(set.total_entries(), 3);
        assert_eq!(set.names(), vec![POOL]);
        let vals = set.column_values(POOL).unwrap();
        assert_eq!(vals, &["A".to_string(), "B".into(), "C".into()]);
        assert_eq!(set.get(POOL, 1), Some("B"));
    }

    #[test]
    fn cams_lists_via_pool_with_legacy_fallback() {
        let store = Store::open_mem().unwrap();
        let a = store.intern(POOL, "Nikon D850").unwrap();
        let b = store.intern(POOL, "Canon R5").unwrap();
        add_photo(&store, 1, Some(a));
        add_photo(&store, 2, Some(a));
        add_photo(&store, 3, Some(b));
        // 一行未池化的旧数据:cam 有值、cam_id 为 NULL
        {
            let c = store.0.lock().unwrap();
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,cam) VALUES(4,'/p/d.jpg','/p',0,0,'Old Cam')",
                [],
            )
            .unwrap();
        }
        let cams = store.cams().unwrap();
        assert_eq!(
            cams,
            vec![
                ("Nikon D850".to_string(), 2),
                ("Canon R5".to_string(), 1),
                ("Old Cam".to_string(), 1),
            ],
            "按张数降序,且旧行靠 cam 回退仍可见"
        );
    }

    #[test]
    fn pool_stats_report_dedup_ratio_inputs() {
        let store = Store::open_mem().unwrap();
        let a = store.intern(POOL, "A").unwrap();
        add_photo(&store, 1, Some(a));
        add_photo(&store, 2, Some(a));
        let stats = store.pool_stats().unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].0, POOL);
        assert_eq!(stats[0].1, 1, "池内 1 条");
        assert_eq!(stats[0].2, 2, "被 2 行引用");
    }

    // ===== P2-15 持久化扫描队列 =====

    #[test]
    fn scan_queue_enqueue_dedup_and_dequeue() {
        let store = Store::open_mem().unwrap();
        let r1 = std::path::PathBuf::from("C:\\pics");
        let r2 = std::path::PathBuf::from("D:\\photos");
        assert_eq!(store.enqueue_scan(&[r1.clone()], "整个图库").unwrap(), 1);
        // 同根重复登记 → 忽略(scope 保留首次)
        assert_eq!(store.enqueue_scan(&[r1.clone()], "别的").unwrap(), 0);
        assert_eq!(store.enqueue_scan(&[r2.clone()], "添加").unwrap(), 1);
        let pend = store.pending_scans();
        assert_eq!(pend.len(), 2);
        assert_eq!(pend[0], ("C:\\pics".into(), "整个图库".into()));
        assert_eq!(pend[1], ("D:\\photos".into(), "添加".into()));
        // 完成出队
        store.dequeue_scan("C:\\pics").unwrap();
        let pend = store.pending_scans();
        assert_eq!(pend.len(), 1);
        assert_eq!(pend[0].0, "D:\\photos");
        // 出队不存在的根不报错
        store.dequeue_scan("X:\\none").unwrap();
    }

    #[test]
    fn scan_queue_survives_reopen() {
        // 持久化语义:登记后"重启"(重开连接),条目仍在 → 启动续扫的依据。
        // (Store::open 固定用 data_dir(),测试里手工开两个文件连接验证。)
        let dir = std::env::temp_dir().join(format!("maliang_qtest_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("q.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            let store = Store(Mutex::new(conn));
            store
                .enqueue_scan(&[std::path::PathBuf::from("E:\\cam")], "设备")
                .unwrap();
        }
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            let store = Store(Mutex::new(conn));
            let pend = store.pending_scans();
            assert_eq!(pend.len(), 1);
            assert_eq!(pend[0], ("E:\\cam".into(), "设备".into()));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_db_has_star_column() {
        let store = Store::open_mem().unwrap();
        let cols: Vec<String> = store
            .0
            .lock()
            .unwrap()
            .prepare("PRAGMA table_info(photos)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(cols.contains(&"star".to_string()), "photos 列: {cols:?}");
        assert!(cols.contains(&"xmp_imported".to_string()), "photos 列: {cols:?}");
        assert!(cols.contains(&"dhash".to_string()), "photos 列: {cols:?}");
        assert!(cols.contains(&"baked".to_string()), "photos 列: {cols:?}");
        assert!(cols.contains(&"lat".to_string()), "photos 列: {cols:?}");
        assert!(cols.contains(&"lon".to_string()), "photos 列: {cols:?}");
        let tcols: Vec<String> = store
            .0
            .lock()
            .unwrap()
            .prepare("PRAGMA table_info(tags)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tcols.contains(&"color".to_string()), "tags 列: {tcols:?}");
        let fcols: Vec<String> = store
            .0
            .lock()
            .unwrap()
            .prepare("PRAGMA table_info(faces)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(fcols.contains(&"quality".to_string()), "faces 列: {fcols:?}");
        assert!(fcols.contains(&"ignored".to_string()), "faces 列: {fcols:?}");
    }

    // ===== P1-11 uid64 =====

    /// FNV-1a:确定性、跨进程一致、对不同输入不同。
    #[test]
    fn fnv1a64_is_deterministic() {
        assert_eq!(fnv1a64(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64("abc"), fnv1a64("abc"));
        assert_ne!(fnv1a64("abc"), fnv1a64("abd"));
        // 已知向量(FNV-1a 64 "a" = 0xaf63dc4c8601ec8c)。
        assert_eq!(fnv1a64("a"), 0xaf63_dc4c_8601_ec8c);
    }

    /// uid64:回填后能按标识查询照片;重复回填幂等。
    #[test]
    fn uid64_backfill_and_lookup() {
        let store = Store::open_mem().unwrap();
        {
            let c = store.0.lock().unwrap();
            c.execute(
                "INSERT INTO photos(path,parent,mtime,size) VALUES('/x/a.jpg','/x',100,2000)",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO photos(path,parent,mtime,size) VALUES('/x/b.jpg','/x',300,4000)",
                [],
            )
            .unwrap();
        }
        assert_eq!(store.backfill_uid64().unwrap(), 2);
        // 幂等:再调一次不新增(已有 uid64 的行被跳过)。
        assert_eq!(store.backfill_uid64().unwrap(), 0);

        // 按标识查询。
        let uid: i64 = store
            .0
            .lock()
            .unwrap()
            .query_row("SELECT uid64 FROM photos WHERE path='/x/a.jpg'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let id = store.photo_by_uid64(uid as u64).expect("应能按 uid64 查到");
        let path: String = store
            .0
            .lock()
            .unwrap()
            .query_row("SELECT path FROM photos WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(path, "/x/a.jpg");

        // 不存在的标识返回 None。
        assert!(store.photo_by_uid64(0xdead_beef_1234_5678).is_none());
    }

    // ===== P1-12 变更日志 =====

    /// 变更日志:写入 → 倒序读取 → 清空。
    #[test]
    fn change_log_roundtrip() {
        let store = Store::open_mem().unwrap();
        assert_eq!(store.change_count(), 0);
        store.log_change("/pics/a.jpg", "added", Some(1)).unwrap();
        store.log_change("/pics/b.jpg", "modified", None).unwrap();
        store.log_change("/pics/c.jpg", "removed", None).unwrap();
        assert_eq!(store.change_count(), 3);

        let recent = store.recent_changes(10);
        assert_eq!(recent.len(), 3);
        // 倒序:最后写入的在前。
        assert_eq!(recent[0].1, "/pics/c.jpg");
        assert_eq!(recent[0].2, "removed");
        assert_eq!(recent[2].2, "added");

        // 限量生效。
        assert_eq!(store.recent_changes(2).len(), 2);

        store.clear_changes().unwrap();
        assert_eq!(store.change_count(), 0);
        assert!(store.recent_changes(10).is_empty());
    }

    /// P1-9:thumb_tiers 表记录各档完成状态。
    #[test]
    fn thumb_tiers_tracks_per_tier_state() {
        let store = Store::open_mem().unwrap();
        let pid: i64 = {
            let c = store.0.lock().unwrap();
            c.execute(
                "INSERT INTO photos(path,parent,mtime,size) VALUES('/t/a.jpg','/t',0,0)",
                [],
            )
            .unwrap();
            c.last_insert_rowid()
        };
        // 未生成时 missing_tier 应返回该照片。
        let missing = store.missing_tier(TIER_BIG, 10);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].0, pid);

        store.mark_tier_done(pid, TIER_BIG).unwrap();
        assert!(store.missing_tier(TIER_BIG, 10).is_empty());
        assert_eq!(store.tier_count(TIER_BIG), 1);
        // 重复标记幂等。
        store.mark_tier_done(pid, TIER_BIG).unwrap();
        assert_eq!(store.tier_count(TIER_BIG), 1);

        // Normal 档走 photos.thumb_ok,不走 thumb_tiers。
        assert_eq!(store.missing_tier(TIER_NORMAL, 10).len(), 1);
        store
            .0
            .lock()
            .unwrap()
            .execute("UPDATE photos SET thumb_ok=1 WHERE id=?1", [pid])
            .unwrap();
        assert!(store.missing_tier(TIER_NORMAL, 10).is_empty());
        assert_eq!(store.tier_count(TIER_NORMAL), 1);
    }
}

/// 供测试用的档位常量(与 thumbs::Tier::Big 对应)。
#[cfg(test)]
const TIER_BIG: u8 = 2;
/// tier=1 是 Normal 档(photos.thumb_ok 专用)。
#[cfg(test)]
const TIER_NORMAL: u8 = 1;
