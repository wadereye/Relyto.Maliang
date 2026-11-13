//! 名字索引服务的进程间协议。
//!
//! 形态:一个提权常驻服务握着 L1 内存索引,非提权 UI 通过
//! 命名管道 `\\.\pipe\maliang-idx` 问它。管道两端都在本机、都是自己人,
//! 所以协议是**紧凑二进制**而不是文本/JSON —— 一次搜索最多回 500 条命中,
//! 每条带完整路径,文本编码的开销和解析歧义都不值得。
//!
//! 帧格式:`u32 LE 载荷长度` + 载荷;载荷首字节是操作码。
//! 单帧上限 [`MAX_FRAME`] 既是防御(坏客户端发个 4 GB 长度就想让服务 OOM)
//! 也是自限:命中列表本来就是 top-N,不需要更大的帧。
//!
//! 编解码全部是纯函数(`encode_req`/`decode_req`/`encode_resp`/`decode_resp`),
//! 与管道传输解耦,因此可以单测;`handle` 只依赖 [`Query`] trait,
//! 测试用内存里的 `VolumeIndex` 就能跑通整条服务端路径。

use std::io::{Read, Write};

/// 管道名(固定名字,一个服务一个实例)。
pub const PIPE_NAME: &str = r"\\.\pipe\maliang-idx";

/// 协议版本。请求方在 `Hello` 里带自己的版本,服务不认就回 `Err`,
/// 免得新旧构建混跑时把字节流当成对方的结构解。
///
/// v2:`Hit` 多一字节 `rich_state`(的结果投影)。
/// 这是**破坏性**改动 —— 旧服务回的 8 字段 Hit 若被新客户端按 9 字段解,
/// 读到的就是错位字节而不是错误,所以必须靠版本号挡在握手阶段。
pub const PROTO_VERSION: u8 = 2;

/// `Hit::rich_state` 的哨兵:这条命中**不在 L2 里**(非媒体文件,或还没落库)。
///
/// 与 `db::RICH_OUT`(在库、但未纳入派生作用域)语义不同,不能混用 ——
/// 前者是「快查看得见但图库不收」,后者是「图库收了但不派生」。
pub const RICH_UNKNOWN: u8 = 255;

/// 单帧载荷上限(16 MiB)。
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// 管道安全描述符(SDDL)。
///
/// 默认 DACL 是"创建者全权 + 其他所有人无",那样非提权 UI 连不上;
/// 而完全放开又等于让任意本地进程能命令提权服务重建索引、读全盘文件名。
/// 取中间档:SYSTEM/管理员全权,交互式用户读写执行(够 UI 用),其余无。
pub const PIPE_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGWGX;;;IU)";

// ---- 操作码 ----
const OP_HELLO: u8 = 1;
const OP_SEARCH: u8 = 2;
const OP_PATH_OF: u8 = 3;
const OP_CHILDREN_OF: u8 = 4;
const OP_STAT: u8 = 5;
const OP_FLUSH: u8 = 6;
const OP_STATUS: u8 = 7;
const OP_REBUILD: u8 = 8;
const OP_CURSOR: u8 = 9;
/// 追加操作码不改既有帧布局,不必升协议版本;旧服务不认会回 Err,客户端按失败降级。
const OP_SYNC_MARKER: u8 = 10;

const RP_OK: u8 = 1;
const RP_ERR: u8 = 2;
const RP_HELLO: u8 = 3;
const RP_HITS: u8 = 4;
const RP_PATH: u8 = 5;
const RP_CHILDREN: u8 = 6;
const RP_STAT: u8 = 7;
const RP_STATUS: u8 = 8;
const RP_CURSORS: u8 = 9;

/// 请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Req {
    /// 握手:带客户端协议版本,服务回自己的能力摘要。
    Hello { proto: u8 },
    /// 快查。`class` 用 `nameidx::FileClass` 的判别值(见 [`class_to_wire`])。
    Search { text: String, limit: u32, class: u8 },
    /// 卷内槽位 → 完整路径(结果列表只回槽位时用得上)。
    PathOf { letter: char, idx: u32 },
    /// 目录展开:回直接子项。
    ChildrenOf { letter: char, idx: u32 },
    /// 单条元数据(存在性/大小/时间/类别)。
    Stat { path: String },
    /// 让服务立刻把内存索引落盘(用户在设置里点"保存索引"时用)。
    Flush,
    /// 服务与索引概况(UI 的降级横幅、状态页)。
    Status,
    /// 请求全量重建;空列表 = 全部可扫卷。服务异步执行,立刻回 `Ok`。
    Rebuild { letters: Vec<char> },
    /// 每卷日志游标(追赶判定与诊断)。
    JournalCursor,
    /// 请服务把机器级数据目录标记改到 `path`:迁移后非提权 UI 写不动 ProgramData,
    /// 由 LocalSystem 服务代写,服务下次启动即落在新目录。
    SyncMarker { path: String },
}

/// 一条命中。比 `nameidx::SearchHit` 多带 size/mtime/cls/rich_state ——
/// 前三列服务端已经在内存里握着,顺手回给 UI 就省掉一次 `stat`;
/// `rich_state` 是 L2 的派生门状态,由服务在投影时批量查库补上(
/// 投影在服务内完成,UI 不该为了一个占位标记自己再跑一趟 SQL)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub letter: char,
    pub idx: u32,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// 修改时间,UNIX 秒(USN 降级路径下为 0 = 未知)。
    pub mtime: u64,
    /// 类别位标记(`nameidx::CLASS_*`)。
    pub cls: u8,
    /// 派生状态(`db::RICH_*`);`RICH_UNKNOWN` = 这条不在 L2 里
    /// (非媒体文件、或还没落库),与「未纳入派生作用域」不是一回事。
    pub rich_state: u8,
}

/// 目录展开的一条子项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    pub idx: u32,
    pub name: String,
    pub is_dir: bool,
}

/// `Stat` 的答复。`exists=false` 时其余字段无意义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatInfo {
    pub exists: bool,
    pub letter: char,
    pub idx: u32,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u64,
    pub mtime_usn: u64,
    pub cls: u8,
}

/// 一卷的健康状况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolStatus {
    pub letter: char,
    /// 条目数;0 且 `why` 非空 = 这卷没索引。
    pub entries: u64,
    /// 正在全量建库。
    pub building: bool,
    /// 走不了 USN/MFT 的原因(非 NTFS、无权限、日志没开…)。空 = 正常。
    pub why: String,
}

/// 服务概况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusInfo {
    pub proto: u8,
    pub uptime_secs: u64,
    /// 索引是否已落盘到最新(dirty = 有未保存的变更)。
    pub dirty: bool,
    pub last_save_ago_secs: u64,
    pub vols: Vec<VolStatus>,
}

/// 一卷的日志游标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub letter: char,
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    /// L2(structural 入库)已消费到的 USN;落后于 `next_usn` 就是待补。
    pub l2_usn: i64,
}

/// 答复。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resp {
    Ok,
    /// 服务侧的可读错误(不区分种类:UI 只需要显示或降级)。
    Err(String),
    Hello {
        proto: u8,
        volumes: u32,
        entries: u64,
    },
    Hits(Vec<Hit>),
    Path(Option<String>),
    Children(Vec<Child>),
    Stat(StatInfo),
    Status(StatusInfo),
    Cursors(Vec<Cursor>),
}

/// 服务端要实现的查询面。做成 trait 是为了让 `handle` 可测:
/// 测试里塞一个内存 `VolumeIndex` 就够,不需要真的起服务、真的读 `$MFT`。
pub trait Query {
    fn hello(&self) -> Resp;
    fn search(&self, text: &str, limit: u32, class: u8) -> Resp;
    fn path_of(&self, letter: char, idx: u32) -> Resp;
    fn children_of(&self, letter: char, idx: u32) -> Resp;
    fn stat(&self, path: &str) -> Resp;
    fn flush(&self) -> Resp;
    fn status(&self) -> Resp;
    fn rebuild(&self, letters: &[char]) -> Resp;
    fn cursors(&self) -> Resp;
    fn sync_marker(&self, path: &str) -> Resp;
}

/// `nameidx::FileClass` → 线上判别值。
///
/// 不用 `as` 直接转:枚举顺序一变线上语义就悄悄变了。
pub fn class_to_wire(c: crate::nameidx::FileClass) -> u8 {
    use crate::nameidx::FileClass as F;
    match c {
        F::All => 0,
        F::Image => 1,
        F::Video => 2,
        F::Audio => 3,
        F::Archive => 4,
        F::Executable => 5,
        F::Document => 6,
    }
}

/// 线上判别值 → `nameidx::FileClass`(未知值退回 `All`,不报错:
/// 过滤条件退化比整条查询失败好)。
pub fn class_from_wire(v: u8) -> crate::nameidx::FileClass {
    use crate::nameidx::FileClass as F;
    match v {
        1 => F::Image,
        2 => F::Video,
        3 => F::Audio,
        4 => F::Archive,
        5 => F::Executable,
        6 => F::Document,
        _ => F::All,
    }
}

// ---------------------------------------------------------------- 编解码原语

/// 写出端:小端定长整数 + 长度前缀字符串。
struct W {
    b: Vec<u8>,
}

impl W {
    fn new(op: u8) -> Self {
        W { b: vec![op] }
    }
    fn u8(&mut self, v: u8) {
        self.b.push(v);
    }
    fn bool(&mut self, v: bool) {
        self.b.push(v as u8);
    }
    fn u32(&mut self, v: u32) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    /// 盘符按 ASCII 单字节存(Windows 卷标就是 A–Z)。
    fn letter(&mut self, c: char) {
        self.b.push(c as u8);
    }
    fn str(&mut self, s: &str) {
        self.b.extend_from_slice(&(s.len() as u32).to_le_bytes());
        self.b.extend_from_slice(s.as_bytes());
    }
    /// `None` 用长度 `u32::MAX` 表示:字符串长度不可能到这个量级,
    /// 省一个可选标记字节。
    fn opt_str(&mut self, s: Option<&str>) {
        match s {
            Some(v) => self.str(v),
            None => self.b.extend_from_slice(&u32::MAX.to_le_bytes()),
        }
    }
    fn into_vec(self) -> Vec<u8> {
        self.b
    }
}

/// 读入端。**所有读取都返回 `Option`**:坏帧(截断/越界长度)只能被
/// 拒绝,不能让服务端 panic —— 管道对面是另一个进程,内容不可信。
struct R<'a> {
    b: &'a [u8],
    p: usize,
}

/// 解码失败原因(统一成一句话:UI 只显示,不分支)。
const BAD_FRAME: &str = "IPC 帧格式不符";

impl<'a> R<'a> {
    fn new(b: &'a [u8]) -> Self {
        R { b, p: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let e = self.p.checked_add(n)?;
        if e > self.b.len() {
            return None;
        }
        let s = &self.b[self.p..e];
        self.p = e;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn bool(&mut self) -> Option<bool> {
        Some(self.u8()? != 0)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn letter(&mut self) -> Option<char> {
        let b = self.u8()?;
        if b.is_ascii_uppercase() {
            Some(b as char)
        } else {
            None
        }
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        // 长度字段自己就可能是恶意的大数,先跟剩余字节数比,别去分配。
        if n > self.b.len() - self.p {
            return None;
        }
        let s = self.take(n)?;
        String::from_utf8(s.to_vec()).ok()
    }
    fn opt_str(&mut self) -> Option<Option<String>> {
        let n = self.u32()?;
        if n == u32::MAX {
            return Some(None);
        }
        let n = n as usize;
        if n > self.b.len() - self.p {
            return None;
        }
        let s = self.take(n)?;
        Some(String::from_utf8(s.to_vec()).ok())
    }
    /// 帧读完后不允许有剩余字节(否则说明两边版本对不上)。
    fn done(&self) -> bool {
        self.p == self.b.len()
    }
}

// ---------------------------------------------------------------- 请求编解码

pub fn encode_req(r: &Req) -> Vec<u8> {
    match r {
        Req::Hello { proto } => {
            let mut w = W::new(OP_HELLO);
            w.u8(*proto);
            w.into_vec()
        }
        Req::Search { text, limit, class } => {
            let mut w = W::new(OP_SEARCH);
            w.str(text);
            w.u32(*limit);
            w.u8(*class);
            w.into_vec()
        }
        Req::PathOf { letter, idx } => {
            let mut w = W::new(OP_PATH_OF);
            w.letter(*letter);
            w.u32(*idx);
            w.into_vec()
        }
        Req::ChildrenOf { letter, idx } => {
            let mut w = W::new(OP_CHILDREN_OF);
            w.letter(*letter);
            w.u32(*idx);
            w.into_vec()
        }
        Req::Stat { path } => {
            let mut w = W::new(OP_STAT);
            w.str(path);
            w.into_vec()
        }
        Req::Flush => W::new(OP_FLUSH).into_vec(),
        Req::Status => W::new(OP_STATUS).into_vec(),
        Req::Rebuild { letters } => {
            let mut w = W::new(OP_REBUILD);
            w.u32(letters.len() as u32);
            for c in letters {
                w.letter(*c);
            }
            w.into_vec()
        }
        Req::JournalCursor => W::new(OP_CURSOR).into_vec(),
        Req::SyncMarker { path } => {
            let mut w = W::new(OP_SYNC_MARKER);
            w.str(path);
            w.into_vec()
        }
    }
}

pub fn decode_req(b: &[u8]) -> Result<Req, &'static str> {
    let mut r = R::new(b);
    let op = r.u8().ok_or(BAD_FRAME)?;
    let out = match op {
        OP_HELLO => Req::Hello {
            proto: r.u8().ok_or(BAD_FRAME)?,
        },
        OP_SEARCH => Req::Search {
            text: r.str().ok_or(BAD_FRAME)?,
            limit: r.u32().ok_or(BAD_FRAME)?,
            class: r.u8().ok_or(BAD_FRAME)?,
        },
        OP_PATH_OF => Req::PathOf {
            letter: r.letter().ok_or(BAD_FRAME)?,
            idx: r.u32().ok_or(BAD_FRAME)?,
        },
        OP_CHILDREN_OF => Req::ChildrenOf {
            letter: r.letter().ok_or(BAD_FRAME)?,
            idx: r.u32().ok_or(BAD_FRAME)?,
        },
        OP_STAT => Req::Stat {
            path: r.str().ok_or(BAD_FRAME)?,
        },
        OP_FLUSH => Req::Flush,
        OP_STATUS => Req::Status,
        OP_REBUILD => {
            let n = r.u32().ok_or(BAD_FRAME)? as usize;
            // 卷字母表最多 26 个,再多就是坏帧。
            if n > 26 {
                return Err(BAD_FRAME);
            }
            let mut letters = Vec::with_capacity(n);
            for _ in 0..n {
                letters.push(r.letter().ok_or(BAD_FRAME)?);
            }
            Req::Rebuild { letters }
        }
        OP_CURSOR => Req::JournalCursor,
        OP_SYNC_MARKER => Req::SyncMarker {
            path: r.str().ok_or(BAD_FRAME)?,
        },
        _ => return Err(BAD_FRAME),
    };
    if !r.done() {
        return Err(BAD_FRAME);
    }
    Ok(out)
}

// ---------------------------------------------------------------- 答复编解码

fn put_hit(w: &mut W, h: &Hit) {
    w.letter(h.letter);
    w.u32(h.idx);
    w.bool(h.is_dir);
    w.u64(h.size);
    w.u64(h.mtime);
    w.u8(h.cls);
    w.u8(h.rich_state);
    w.str(&h.name);
    w.str(&h.path);
}

fn get_hit(r: &mut R<'_>) -> Option<Hit> {
    Some(Hit {
        letter: r.letter()?,
        idx: r.u32()?,
        is_dir: r.bool()?,
        size: r.u64()?,
        mtime: r.u64()?,
        cls: r.u8()?,
        rich_state: r.u8()?,
        name: r.str()?,
        path: r.str()?,
    })
}

pub fn encode_resp(v: &Resp) -> Vec<u8> {
    match v {
        Resp::Ok => W::new(RP_OK).into_vec(),
        Resp::Err(e) => {
            let mut w = W::new(RP_ERR);
            w.str(e);
            w.into_vec()
        }
        Resp::Hello {
            proto,
            volumes,
            entries,
        } => {
            let mut w = W::new(RP_HELLO);
            w.u8(*proto);
            w.u32(*volumes);
            w.u64(*entries);
            w.into_vec()
        }
        Resp::Hits(hs) => {
            let mut w = W::new(RP_HITS);
            w.u32(hs.len() as u32);
            for h in hs {
                put_hit(&mut w, h);
            }
            w.into_vec()
        }
        Resp::Path(p) => {
            let mut w = W::new(RP_PATH);
            w.opt_str(p.as_deref());
            w.into_vec()
        }
        Resp::Children(cs) => {
            let mut w = W::new(RP_CHILDREN);
            w.u32(cs.len() as u32);
            for c in cs {
                w.u32(c.idx);
                w.bool(c.is_dir);
                w.str(&c.name);
            }
            w.into_vec()
        }
        Resp::Stat(s) => {
            let mut w = W::new(RP_STAT);
            w.bool(s.exists);
            w.letter(s.letter);
            w.u32(s.idx);
            w.bool(s.is_dir);
            w.u64(s.size);
            w.u64(s.mtime);
            w.u64(s.mtime_usn);
            w.u8(s.cls);
            w.into_vec()
        }
        Resp::Status(s) => {
            let mut w = W::new(RP_STATUS);
            w.u8(s.proto);
            w.u64(s.uptime_secs);
            w.bool(s.dirty);
            w.u64(s.last_save_ago_secs);
            w.u32(s.vols.len() as u32);
            for v in &s.vols {
                w.letter(v.letter);
                w.u64(v.entries);
                w.bool(v.building);
                w.str(&v.why);
            }
            w.into_vec()
        }
        Resp::Cursors(cs) => {
            let mut w = W::new(RP_CURSORS);
            w.u32(cs.len() as u32);
            for c in cs {
                w.letter(c.letter);
                w.u64(c.journal_id);
                w.i64(c.first_usn);
                w.i64(c.next_usn);
                w.i64(c.l2_usn);
            }
            w.into_vec()
        }
    }
}

pub fn decode_resp(b: &[u8]) -> Result<Resp, &'static str> {
    let mut r = R::new(b);
    let op = r.u8().ok_or(BAD_FRAME)?;
    let out = match op {
        RP_OK => Resp::Ok,
        RP_ERR => Resp::Err(r.str().ok_or(BAD_FRAME)?),
        RP_HELLO => Resp::Hello {
            proto: r.u8().ok_or(BAD_FRAME)?,
            volumes: r.u32().ok_or(BAD_FRAME)?,
            entries: r.u64().ok_or(BAD_FRAME)?,
        },
        RP_HITS => {
            let n = r.u32().ok_or(BAD_FRAME)? as usize;
            let mut hs = Vec::new();
            for _ in 0..n {
                hs.push(get_hit(&mut r).ok_or(BAD_FRAME)?);
            }
            Resp::Hits(hs)
        }
        RP_PATH => Resp::Path(r.opt_str().ok_or(BAD_FRAME)?),
        RP_CHILDREN => {
            let n = r.u32().ok_or(BAD_FRAME)? as usize;
            let mut cs = Vec::new();
            for _ in 0..n {
                cs.push(Child {
                    idx: r.u32().ok_or(BAD_FRAME)?,
                    is_dir: r.bool().ok_or(BAD_FRAME)?,
                    name: r.str().ok_or(BAD_FRAME)?,
                });
            }
            Resp::Children(cs)
        }
        RP_STAT => Resp::Stat(StatInfo {
            exists: r.bool().ok_or(BAD_FRAME)?,
            letter: r.letter().ok_or(BAD_FRAME)?,
            idx: r.u32().ok_or(BAD_FRAME)?,
            is_dir: r.bool().ok_or(BAD_FRAME)?,
            size: r.u64().ok_or(BAD_FRAME)?,
            mtime: r.u64().ok_or(BAD_FRAME)?,
            mtime_usn: r.u64().ok_or(BAD_FRAME)?,
            cls: r.u8().ok_or(BAD_FRAME)?,
        }),
        RP_STATUS => {
            let s = StatusInfo {
                proto: r.u8().ok_or(BAD_FRAME)?,
                uptime_secs: r.u64().ok_or(BAD_FRAME)?,
                dirty: r.bool().ok_or(BAD_FRAME)?,
                last_save_ago_secs: r.u64().ok_or(BAD_FRAME)?,
                vols: {
                    let n = r.u32().ok_or(BAD_FRAME)? as usize;
                    let mut vs = Vec::new();
                    for _ in 0..n {
                        vs.push(VolStatus {
                            letter: r.letter().ok_or(BAD_FRAME)?,
                            entries: r.u64().ok_or(BAD_FRAME)?,
                            building: r.bool().ok_or(BAD_FRAME)?,
                            why: r.str().ok_or(BAD_FRAME)?,
                        });
                    }
                    vs
                },
            };
            Resp::Status(s)
        }
        RP_CURSORS => {
            let n = r.u32().ok_or(BAD_FRAME)? as usize;
            let mut cs = Vec::new();
            for _ in 0..n {
                cs.push(Cursor {
                    letter: r.letter().ok_or(BAD_FRAME)?,
                    journal_id: r.u64().ok_or(BAD_FRAME)?,
                    first_usn: r.i64().ok_or(BAD_FRAME)?,
                    next_usn: r.i64().ok_or(BAD_FRAME)?,
                    l2_usn: r.i64().ok_or(BAD_FRAME)?,
                });
            }
            Resp::Cursors(cs)
        }
        _ => return Err(BAD_FRAME),
    };
    if !r.done() {
        return Err(BAD_FRAME);
    }
    Ok(out)
}

// ---------------------------------------------------------------- 分帧传输

/// 写一帧(长度前缀 + 载荷)。
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "IPC 载荷超出单帧上限",
        ));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// 读一帧。`Ok(None)` = 对端干净地关了(服务重启/客户端断开),不是错误。
pub fn read_frame<R: Read>(r: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "IPC 帧长度超上限",
        ));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// 服务端主循环的一格:解一帧、跑一次查询、编一帧。
///
/// 任何解码失败都回 `Err` 而不是断连 —— 一个坏请求不该把 UI 整条会话打掉。
pub fn handle(payload: &[u8], q: &dyn Query) -> Vec<u8> {
    let req = match decode_req(payload) {
        Ok(r) => r,
        Err(e) => return encode_resp(&Resp::Err(e.to_string())),
    };
    let resp = match &req {
        Req::Hello { proto } => {
            if *proto != PROTO_VERSION {
                Resp::Err(format!(
                    "IPC 协议版本不符(客户端 v{proto},服务端 v{PROTO_VERSION})"
                ))
            } else {
                q.hello()
            }
        }
        Req::Search { text, limit, class } => q.search(text, *limit, *class),
        Req::PathOf { letter, idx } => q.path_of(*letter, *idx),
        Req::ChildrenOf { letter, idx } => q.children_of(*letter, *idx),
        Req::Stat { path } => q.stat(path),
        Req::Flush => q.flush(),
        Req::Status => q.status(),
        Req::Rebuild { letters } => q.rebuild(letters),
        Req::JournalCursor => q.cursors(),
        Req::SyncMarker { path } => q.sync_marker(path),
    };
    encode_resp(&resp)
}

/// 在一次已建立的会话上跑请求/答复循环,直到对端关闭或出错。
pub fn serve_conn<R: Read, W: Write>(r: &mut R, w: &mut W, q: &dyn Query) -> std::io::Result<()> {
    loop {
        match read_frame(r)? {
            None => return Ok(()),
            Some(p) => {
                let out = handle(&p, q);
                write_frame(w, &out)?;
            }
        }
    }
}

// ---------------------------------------------------------------- 客户端

/// 管道客户端。持一个 `File`(管道句柄包装),同步一问一答。
pub struct Client {
    f: std::fs::File,
}

impl Client {
    /// 连接服务。`timeout_ms` 只影响"管道忙"时的等待,不是读写超时。
    pub fn connect(timeout_ms: u32) -> Result<Client, String> {
        #[cfg(not(windows))]
        {
            let _ = timeout_ms;
            Err("仅支持 Windows".into())
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::FromRawHandle;
            use windows::core::PCWSTR;
            use windows::Win32::Foundation::{ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE};
            use windows::Win32::Storage::FileSystem::{
                CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            };
            use windows::Win32::System::Pipes::WaitNamedPipeW;

            let name: Vec<u16> = crate::win::to_wide(PIPE_NAME);
            // 管道忙(服务的实例都被占着)时 WaitNamedPipeW 等一个空位,
            // 最多试 3 次:UI 不该因为服务正在处理别人的请求就直接判降级。
            let mut last = String::from("连接索引服务失败");
            for _ in 0..3 {
                // SAFETY: name 是 NUL 结尾宽串;其余参数为常规打开标志,
                // 失败返回 INVALID_HANDLE_VALUE,由 is_invalid 判定。
                let h = unsafe {
                    CreateFileW(
                        PCWSTR(name.as_ptr()),
                        GENERIC_READ.0 | GENERIC_WRITE.0,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        None,
                        OPEN_EXISTING,
                        windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
                        None,
                    )
                };
                match h {
                    Ok(h) if !h.is_invalid() => {
                        // SAFETY: 句柄由本函数独占,交给 File 后由它负责关闭。
                        return Ok(Client {
                            f: unsafe { std::fs::File::from_raw_handle(h.0 as *mut _) },
                        });
                    }
                    Ok(h) => {
                        // SAFETY: 无效句柄不需要关闭,这里只是让绑定不被忽略。
                        let _ = h;
                        last = "打开管道返回无效句柄".into();
                    }
                    Err(e) => {
                        last = format!("{e}");
                        if e.code() != ERROR_PIPE_BUSY.to_hresult() {
                            break;
                        }
                        // SAFETY: 同一根管道名;超时以毫秒计。
                        let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), timeout_ms) };
                    }
                }
            }
            Err(format!("连接 {PIPE_NAME} 失败:{last}"))
        }
    }

    /// 一问一答。
    pub fn call(&mut self, req: &Req) -> Result<Resp, String> {
        let payload = encode_req(req);
        write_frame(&mut self.f, &payload).map_err(|e| format!("发送失败:{e}"))?;
        let back = read_frame(&mut self.f)
            .map_err(|e| format!("接收失败:{e}"))?
            .ok_or_else(|| "服务已关闭连接".to_string())?;
        match decode_resp(&back) {
            Ok(r) => Ok(r),
            Err(e) => Err(e.to_string()),
        }
    }

    /// 把 `Err` 语义摊平:`Resp::Err` 也算失败,调用方少写一层 match。
    pub fn call_ok(&mut self, req: &Req) -> Result<Resp, String> {
        match self.call(req)? {
            Resp::Err(e) => Err(e),
            r => Ok(r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nameidx::{FileClass, VolumeIndex, VolumeIndexBuilder};

    /// 服务端查询面的内存实现:只用来验证 `handle` 的分派与编解码闭环。
    struct Fake {
        vols: Vec<VolumeIndex>,
        rebuilt: std::cell::RefCell<Vec<char>>,
        flushed: std::cell::Cell<bool>,
        synced: std::cell::RefCell<Option<String>>,
    }

    impl Fake {
        fn new() -> Self {
            let mut b = VolumeIndexBuilder::new('C');
            // 父 = 卷根 FRN(5)→ `Users` 落在卷根虚拟桶里;
            // 条目下标按推入顺序:0=Users,1=photo.jpg,2=notes.txt。
            push(
                &mut b,
                10,
                crate::mft::ROOT_FRN_INDEX,
                crate::nameidx::ATTR_DIRECTORY,
                "Users",
            );
            push(&mut b, 100, 10, 0, "photo.jpg");
            push(&mut b, 101, 10, 0, "notes.txt");
            Fake {
                vols: vec![b.finish()],
                rebuilt: Default::default(),
                flushed: Default::default(),
                synced: Default::default(),
            }
        }
    }

    impl Query for Fake {
        fn hello(&self) -> Resp {
            Resp::Hello {
                proto: PROTO_VERSION,
                volumes: self.vols.len() as u32,
                entries: self.vols.iter().map(|v| v.count() as u64).sum(),
            }
        }
        fn search(&self, text: &str, limit: u32, class: u8) -> Resp {
            let hits = crate::nameidx::search_class(
                &self.vols,
                text,
                limit as usize,
                class_from_wire(class),
            );
            Resp::Hits(
                hits.into_iter()
                    .map(|h| Hit {
                        letter: h.letter,
                        idx: h.idx,
                        name: h.name,
                        path: h.path,
                        is_dir: h.is_dir,
                        size: 0,
                        mtime: 0,
                        cls: 0,
                        rich_state: RICH_UNKNOWN,
                    })
                    .collect(),
            )
        }
        fn path_of(&self, letter: char, idx: u32) -> Resp {
            let v = self.vols.iter().find(|v| v.letter == letter);
            Resp::Path(v.map(|v| v.path_of(idx as usize)))
        }
        fn children_of(&self, letter: char, idx: u32) -> Resp {
            let Some(v) = self.vols.iter().find(|v| v.letter == letter) else {
                return Resp::Children(Vec::new());
            };
            Resp::Children(
                v.children_of(idx as usize)
                    .iter()
                    .map(|c| Child {
                        idx: *c,
                        name: v.name(*c as usize),
                        is_dir: v.is_dir(*c as usize),
                    })
                    .collect(),
            )
        }
        fn stat(&self, path: &str) -> Resp {
            let exists = path.contains("photo.jpg");
            Resp::Stat(StatInfo {
                exists,
                letter: 'C',
                idx: 2,
                is_dir: false,
                size: 1234,
                mtime: 55,
                mtime_usn: 66,
                cls: crate::nameidx::CLASS_IMAGE,
            })
        }
        fn flush(&self) -> Resp {
            self.flushed.set(true);
            Resp::Ok
        }
        fn status(&self) -> Resp {
            Resp::Status(StatusInfo {
                proto: PROTO_VERSION,
                uptime_secs: 7,
                dirty: true,
                last_save_ago_secs: 3,
                vols: vec![VolStatus {
                    letter: 'C',
                    entries: 3,
                    building: false,
                    why: String::new(),
                }],
            })
        }
        fn rebuild(&self, letters: &[char]) -> Resp {
            *self.rebuilt.borrow_mut() = letters.to_vec();
            Resp::Ok
        }
        fn cursors(&self) -> Resp {
            Resp::Cursors(vec![Cursor {
                letter: 'C',
                journal_id: 0x1234,
                first_usn: 0,
                next_usn: 99,
                l2_usn: 90,
            }])
        }
        fn sync_marker(&self, path: &str) -> Resp {
            *self.synced.borrow_mut() = Some(path.to_string());
            Resp::Ok
        }
    }

    /// 测试用建索引条目(与 nameidx/scan 测试里的同名助手一致口径)。
    fn push(b: &mut VolumeIndexBuilder, frn: u64, parent: u64, attrs: u32, name: &str) {
        let units: Vec<u16> = name.encode_utf16().collect();
        let e = crate::mft::UsnEntry {
            frn,
            parent_frn: parent,
            attrs,
            reason: 0,
            usn: 0,
            name: &units,
            timestamp: 0,
        };
        b.push_usn(&e);
    }

    fn roundtrip_req(r: &Req) {
        assert_eq!(decode_req(&encode_req(r)).unwrap(), *r);
    }

    fn roundtrip_resp(v: &Resp) {
        assert_eq!(decode_resp(&encode_resp(v)).unwrap(), *v);
    }

    #[test]
    fn every_request_roundtrips() {
        roundtrip_req(&Req::Hello {
            proto: PROTO_VERSION,
        });
        roundtrip_req(&Req::Search {
            text: "照片 2024".into(),
            limit: 500,
            class: class_to_wire(FileClass::Image),
        });
        roundtrip_req(&Req::PathOf {
            letter: 'D',
            idx: 42,
        });
        roundtrip_req(&Req::ChildrenOf {
            letter: 'C',
            idx: 0,
        });
        roundtrip_req(&Req::Stat {
            path: r"C:\a\b.jpg".into(),
        });
        roundtrip_req(&Req::Flush);
        roundtrip_req(&Req::Status);
        roundtrip_req(&Req::Rebuild {
            letters: vec!['C', 'D'],
        });
        roundtrip_req(&Req::Rebuild {
            letters: Vec::new(),
        });
        roundtrip_req(&Req::JournalCursor);
        roundtrip_req(&Req::SyncMarker {
            path: r"D:\ml".into(),
        });
    }

    /// 同步标记请求要原样送到查询面(路径不被截断/改写)。
    #[test]
    fn handle_sync_marker_reaches_query() {
        let f = Fake::new();
        let out = handle(
            &encode_req(&Req::SyncMarker {
                path: r"D:\ml".into(),
            }),
            &f,
        );
        assert_eq!(decode_resp(&out).unwrap(), Resp::Ok);
        assert_eq!(f.synced.borrow().as_deref(), Some(r"D:\ml"));
    }

    #[test]
    fn every_response_roundtrips() {
        roundtrip_resp(&Resp::Ok);
        roundtrip_resp(&Resp::Err("坏帧".into()));
        roundtrip_resp(&Resp::Hello {
            proto: 1,
            volumes: 3,
            entries: 1_280_000,
        });
        // 两条命中:一条带真实的派生状态,一条是「不在 L2 里」的哨兵 ——
        // rich_state 是 v2 才加上的字段,取两种不同值才能钉住它的编解码位置。
        roundtrip_resp(&Resp::Hits(vec![
            Hit {
                letter: 'C',
                idx: 7,
                name: "a.jpg".into(),
                path: r"C:\x\a.jpg".into(),
                is_dir: false,
                size: 1 << 40,
                mtime: 1_700_000_000,
                cls: crate::nameidx::CLASS_IMAGE | crate::nameidx::FLAG_INDEXABLE,
                rich_state: 2,
            },
            Hit {
                letter: 'C',
                idx: 8,
                name: "b.txt".into(),
                path: r"C:\x\b.txt".into(),
                is_dir: false,
                size: 0,
                mtime: 0,
                cls: 0,
                rich_state: RICH_UNKNOWN,
            },
        ]));
        roundtrip_resp(&Resp::Hits(Vec::new()));
        roundtrip_resp(&Resp::Path(Some(r"C:\x".into())));
        roundtrip_resp(&Resp::Path(None));
        roundtrip_resp(&Resp::Children(vec![Child {
            idx: 1,
            name: "子目录".into(),
            is_dir: true,
        }]));
        roundtrip_resp(&Resp::Stat(StatInfo {
            exists: true,
            letter: 'Z',
            idx: 3,
            is_dir: true,
            size: 0,
            mtime: 0,
            mtime_usn: 0,
            cls: 0,
        }));
        roundtrip_resp(&Resp::Status(StatusInfo {
            proto: 1,
            uptime_secs: 3600,
            dirty: false,
            last_save_ago_secs: 12,
            vols: vec![
                VolStatus {
                    letter: 'C',
                    entries: 10,
                    building: false,
                    why: String::new(),
                },
                VolStatus {
                    letter: 'Q',
                    entries: 0,
                    building: true,
                    why: "非 NTFS 卷".into(),
                },
            ],
        }));
        roundtrip_resp(&Resp::Cursors(vec![Cursor {
            letter: 'C',
            journal_id: 0xABCD,
            first_usn: 0,
            next_usn: -1,
            l2_usn: 0,
        }]));
    }

    /// 线上判别值与 `FileClass` 一一对应,`All` 归 0。
    #[test]
    fn class_wire_values_are_stable() {
        for (i, c) in FileClass::all().into_iter().enumerate() {
            assert_eq!(class_to_wire(c), i as u8);
            assert_eq!(class_from_wire(i as u8), c);
        }
        // 未知值退回 All,而不是 panic 或报错。
        assert_eq!(class_from_wire(200), FileClass::All);
    }

    /// 坏帧一律拒绝,不许 panic、不许解成半个请求。
    #[test]
    fn truncated_and_bogus_frames_are_rejected() {
        assert!(decode_req(&[]).is_err());
        assert!(decode_req(&[OP_SEARCH]).is_err());
        assert!(decode_req(&[0xFE]).is_err());
        // 声称 100 字节字符串但只剩 2 字节。
        assert!(decode_req(&[OP_STAT, 100, 0, 0, 0, b'a', b'b']).is_err());
        // 合法请求后面多带字节 = 版本不符,拒绝。
        let mut over = encode_req(&Req::Flush);
        over.push(0);
        assert!(decode_req(&over).is_err());
        // Rebuild 的卷数上限。
        let mut many = vec![OP_REBUILD];
        many.extend_from_slice(&27u32.to_le_bytes());
        assert!(decode_req(&many).is_err());
        // 小写盘符不是合法卷标。
        assert!(decode_req(&[OP_PATH_OF, b'c', 0, 0, 0, 0]).is_err());
        assert!(decode_resp(&[]).is_err());
        assert!(decode_resp(&[RP_HITS, 1, 0, 0, 0]).is_err());
    }

    /// 分帧读写闭环,超长帧被挡住。
    #[test]
    fn framing_roundtrips_and_caps_length() {
        let payload = encode_req(&Req::Status);
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &payload).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        assert_eq!(read_frame(&mut cur).unwrap().unwrap(), payload);
        // 干净 EOF 不是错误。
        assert!(read_frame(&mut cur).unwrap().is_none());

        let mut sink = Vec::new();
        assert!(write_frame(&mut sink, &vec![0u8; MAX_FRAME + 1]).is_err());
        // 帧头声称超过上限 → 读端拒绝,不去分配。
        let mut evil = Vec::new();
        write_frame(&mut evil, b"x").unwrap();
        evil[0..4].copy_from_slice(&(u32::MAX).to_le_bytes());
        assert!(read_frame(&mut std::io::Cursor::new(evil)).is_err());
    }

    /// `handle` 的分派:每种请求都走到对应的查询方法,答复可解码。
    #[test]
    fn handle_dispatches_every_opcode() {
        let f = Fake::new();
        let call = |req: &Req| -> Resp { decode_resp(&handle(&encode_req(req), &f)).unwrap() };
        assert!(matches!(
            call(&Req::Hello {
                proto: PROTO_VERSION
            }),
            Resp::Hello {
                proto: PROTO_VERSION,
                volumes: 1,
                entries: 3
            }
        ));
        match call(&Req::Search {
            text: "photo".into(),
            limit: 10,
            class: class_to_wire(FileClass::All),
        }) {
            Resp::Hits(h) => {
                assert_eq!(h.len(), 1);
                assert_eq!(h[0].path, r"C:\Users\photo.jpg");
            }
            other => panic!("期望 Hits,得到 {other:?}"),
        }
        // 类别过滤在服务端生效:图片类只剩 photo.jpg。
        match call(&Req::Search {
            text: "o".into(),
            limit: 10,
            class: class_to_wire(FileClass::Image),
        }) {
            Resp::Hits(h) => assert!(h.iter().all(|x| x.name.ends_with(".jpg"))),
            other => panic!("期望 Hits,得到 {other:?}"),
        }
        assert_eq!(
            call(&Req::PathOf {
                letter: 'C',
                idx: 1
            }),
            Resp::Path(Some(r"C:\Users\photo.jpg".into()))
        );
        // 未知卷 → None,而不是错误。
        assert_eq!(
            call(&Req::PathOf {
                letter: 'Q',
                idx: 1
            }),
            Resp::Path(None)
        );
        match call(&Req::ChildrenOf {
            letter: 'C',
            idx: 0,
        }) {
            Resp::Children(c) => {
                let mut names: Vec<_> = c.iter().map(|x| x.name.clone()).collect();
                names.sort();
                assert_eq!(names, ["notes.txt", "photo.jpg"]);
            }
            other => panic!("期望 Children,得到 {other:?}"),
        }
        assert!(matches!(
            call(&Req::Stat {
                path: r"C:\Users\photo.jpg".into()
            }),
            Resp::Stat(StatInfo { exists: true, .. })
        ));
        assert!(matches!(
            call(&Req::Stat {
                path: r"C:\Users\gone.jpg".into()
            }),
            Resp::Stat(StatInfo { exists: false, .. })
        ));
        assert_eq!(call(&Req::Flush), Resp::Ok);
        assert!(f.flushed.get());
        assert!(matches!(call(&Req::Status), Resp::Status(_)));
        assert_eq!(
            call(&Req::JournalCursor),
            Resp::Cursors(vec![Cursor {
                letter: 'C',
                journal_id: 0x1234,
                first_usn: 0,
                next_usn: 99,
                l2_usn: 90,
            }])
        );
        assert_eq!(call(&Req::Rebuild { letters: vec!['D'] }), Resp::Ok);
        assert_eq!(*f.rebuilt.borrow(), vec!['D']);
    }

    /// 协议版本不符必须在握手就被挡住(混跑新旧构建时的第一道防线)。
    #[test]
    fn hello_rejects_foreign_protocol_version() {
        let f = Fake::new();
        match decode_resp(&handle(&encode_req(&Req::Hello { proto: 99 }), &f)).unwrap() {
            Resp::Err(e) => assert!(e.contains("协议版本不符"), "{e}"),
            other => panic!("期望 Err,得到 {other:?}"),
        }
    }

    /// 坏帧经 `handle` 变成 `Err` 答复,连接不断。
    #[test]
    fn handle_answers_bad_frame_with_err() {
        let f = Fake::new();
        match decode_resp(&handle(&[0xFE, 1, 2], &f)).unwrap() {
            Resp::Err(_) => {}
            other => panic!("期望 Err,得到 {other:?}"),
        }
    }

    /// `serve_conn` 支持一次会话里多轮问答,对端关闭时正常收尾。
    #[test]
    fn serve_conn_handles_multiple_turns_then_eof() {
        let f = Fake::new();
        let mut inbound = Vec::new();
        write_frame(&mut inbound, &encode_req(&Req::Status)).unwrap();
        write_frame(
            &mut inbound,
            &encode_req(&Req::Hello {
                proto: PROTO_VERSION,
            }),
        )
        .unwrap();
        let mut out = Vec::new();
        let mut cur = std::io::Cursor::new(inbound);
        serve_conn(&mut cur, &mut out, &f).unwrap();
        let mut back = std::io::Cursor::new(out);
        assert!(matches!(
            decode_resp(&read_frame(&mut back).unwrap().unwrap()).unwrap(),
            Resp::Status(_)
        ));
        assert!(matches!(
            decode_resp(&read_frame(&mut back).unwrap().unwrap()).unwrap(),
            Resp::Hello { .. }
        ));
        assert!(read_frame(&mut back).unwrap().is_none());
    }

    /// `Query` 的答复里卷标必须是 ASCII 大写盘符,否则 `letter` 编不出来。
    /// 这条守住"服务把非法卷标塞进答复导致 UI 解不了"的回归。
    #[test]
    fn status_volume_letters_are_encodable() {
        let r = encode_resp(&Resp::Status(StatusInfo {
            proto: 1,
            uptime_secs: 0,
            dirty: false,
            last_save_ago_secs: 0,
            vols: (b'A'..=b'Z')
                .map(|c| VolStatus {
                    letter: c as char,
                    entries: 0,
                    building: false,
                    why: String::new(),
                })
                .collect(),
        }));
        let back = decode_resp(&r).unwrap();
        match back {
            Resp::Status(s) => assert_eq!(s.vols.len(), 26),
            other => panic!("期望 Status,得到 {other:?}"),
        }
    }
}
