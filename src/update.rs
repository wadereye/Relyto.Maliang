//! 自动更新:本地配置指向一个版本清单地址,空闲时检查、后台下载、下次生效。
//!
//! 三段式,刻意让「查」和「装」分开,不打断正在看图的人:
//!
//! 1. 统一配置 maliang.ini 里的 `UpdateURL` 指向服务端的版本清单(key=value 文本)。
//!    配置文件优先取 exe 同目录(便携部署),否则取数据目录;环境变量
//!    `MALIANG_UPDATE_URL` 可临时覆盖。没配 URL 就整个功能静默停用。
//! 2. [`Updater`] 拉一条后台线程。启动后等一个**空闲窗口**(一段时间没有输入,
//!    且扫描/导入/导出/服务装卸都不在跑)才发起检查;发现新版本先提示用户,
//!    随后在后台按 HTTP Range 分段把包写到 `数据目录/update/pending/download.part`,
//!    每段独立计时、失败重试、断点续传,下完并校验后才定名为成品并写 `state.ini`。
//!    下载期间不改任何程序文件,本次会话照常使用;下载完成会再提示一次。
//! 3. 用户点「安装更新」时才动文件:提权拉起自身 `--apply-update`,等索引服务
//!    状态落定后停服务(服务锁着程序文件,不停就换不了),替换 exe,再按原状态把
//!    服务起回来。停服务之前会先落下中断标记,helper 中途被杀时下次启动自动补起。
//!    当前进程仍跑在旧映像上,**重启程序后**新版本才生效 —— 这就是「下次更新」。
//!
//! 安全边界:清单与包都限长、只认 http/https,明文 http 必须给 Sha256,包必须有
//! 合法且与本机架构相容的 PE 头、可校验大小与 SHA256;清单里的相对 URL 按清单地址
//! 所在目录解析(会剥掉查询串并规范化 `..`)。替换走「备份→换入→失败回滚」,
//! 同一数据目录里的下载/安装由文件锁串行化。

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 本机版本(Cargo.toml 里的包版本)。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 默认检查间隔:24 小时。空闲检查只是省事,不该变成每天几十次请求。
const DEFAULT_INTERVAL_HOURS: u64 = 24;
/// 默认空闲门槛:20 秒没有输入、且没有后台重活,才认为「不忙」。
const DEFAULT_IDLE_SECS: u64 = 20;
/// 最长等待:用户一直在操作也不能永远不检查,5 分钟后兜底发一次。
const MAX_AUTO_WAIT: Duration = Duration::from_secs(300);
/// 清单与更新包的体积上限(防坏服务器把磁盘吃满)。
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_PACKAGE: u64 = 512 * 1024 * 1024;
/// 统一配置文件名(数据目录 + 升级 + 模型都在这一份,见 db::SETTINGS_TEMPLATE)。
const CONFIG_NAME: &str = crate::db::SETTINGS_FILE;
const STATE_NAME: &str = "state.ini";
const RESULT_NAME: &str = "apply-result.ini";
const PARTIAL_NAME: &str = "download.part";
const LOCK_NAME: &str = "update.lock";
const SERVICE_MARKER: &str = "service-pending.ini";

/// 下载按 Range 分段取,每段一个独立的时间预算:慢网不会把整包拖死,
/// 断线也只丢当前这一段。2 MiB / 180s 约等于 12 KB/s 的下限,再慢就明确报错。
const CHUNK_BYTES: u64 = 2 * 1024 * 1024;
const CONNECT_SECS: u64 = 10;
const MANIFEST_GLOBAL_SECS: u64 = 30;
const MAX_SEGMENT_ATTEMPTS: u32 = 4;
/// 单实例锁的陈旧判定:进程被强杀时锁文件留得下来,不能永久挡住后续启动。
const LOCK_STALE_SECS: i64 = 30 * 60;

// ---------------------------------------------------------------- 路径与工具

/// 提权 helper 可能跑在另一个管理员账户下,那时「当前用户的数据目录」会指到
/// 别处。父进程把工作目录的绝对路径从命令行递进来,这里优先用它。
static UPDATE_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// 更新工作目录(清单状态、下载包、安装结果)。
pub fn update_dir() -> PathBuf {
    if let Some(p) = UPDATE_DIR_OVERRIDE.get() {
        return p.clone();
    }
    crate::db::data_dir().join("update")
}

fn pending_dir_at(root: &Path) -> PathBuf {
    root.join("pending")
}

fn state_path_at(root: &Path) -> PathBuf {
    root.join(STATE_NAME)
}

fn state_path() -> PathBuf {
    state_path_at(&update_dir())
}

fn result_path_at(root: &Path) -> PathBuf {
    root.join(RESULT_NAME)
}

fn result_path() -> PathBuf {
    result_path_at(&update_dir())
}

/// 未下完的包。跨会话续传就靠它,清单换了版本/哈希就作废重来。
fn partial_path_at(root: &Path) -> PathBuf {
    pending_dir_at(root).join(PARTIAL_NAME)
}

fn lock_path_at(root: &Path) -> PathBuf {
    root.join(LOCK_NAME)
}

/// 「服务已被本次安装停掉」的标记。helper 中途被杀时,下次启动靠它把服务拉回来。
fn service_marker_path_at(root: &Path) -> PathBuf {
    root.join(SERVICE_MARKER)
}

fn service_marker_path() -> PathBuf {
    service_marker_path_at(&update_dir())
}

/// 单实例锁:同一个数据目录里只允许一个进程处在「下载 / 安装」阶段。
/// 进程被强杀会留下锁文件,所以带时间戳,超过 30 分钟视为陈旧可抢。
struct DirLock {
    path: PathBuf,
    token: String,
}

impl DirLock {
    /// `root` 传进来是为了单测能在临时目录里跑,不碰真实数据目录。
    fn acquire_at(root: &Path) -> Option<DirLock> {
        use std::io::Write;
        let _ = std::fs::create_dir_all(root);
        let path = lock_path_at(root);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let token = format!("{}-{}-{}", std::process::id(), now_secs(), nanos);
        // 先清陈旧锁(崩溃残留),再尝试独占创建;create_new 是抢锁的原子操作。
        if path.exists() {
            let kv = parse_kv(&std::fs::read_to_string(&path).unwrap_or_default());
            let ts = kv.get("time").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
            if now_secs().saturating_sub(ts) < LOCK_STALE_SECS {
                return None;
            }
            let _ = std::fs::remove_file(&path);
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .ok()?;
        let _ = writeln!(f, "Time={}\nToken={}", now_secs(), token);
        Some(DirLock { path, token })
    }

    fn acquire() -> Option<DirLock> {
        Self::acquire_at(&update_dir())
    }
}

/// 是否有另一个实例正处在一把新鲜的下载/安装锁里。
fn another_instance_active() -> bool {
    let Ok(text) = std::fs::read_to_string(lock_path_at(&update_dir())) else {
        return false;
    };
    let kv = parse_kv(&text);
    let ts = kv
        .get("time")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    now_secs().saturating_sub(ts) < LOCK_STALE_SECS
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // 只删自己那把锁,别把后来者(陈旧接管)的锁清掉。
        let mine = std::fs::read_to_string(&self.path)
            .map(|s| s.contains(&self.token))
            .unwrap_or(false);
        if mine {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn mark_service_stopped() {
    let _ = atomic_write(
        &service_marker_path(),
        format!("Time={}\nVersion={}\n", now_secs(), CURRENT_VERSION).as_bytes(),
    );
}

fn clear_service_marker() {
    let _ = std::fs::remove_file(service_marker_path());
}

/// 启动时兜底:上一次安装把服务停了却没起回来(helper 被杀 / 断电 / 超时),
/// 这里尽力恢复。普通权限起不动时才走一次提权,正常路径完全静默。
pub fn restore_service_if_interrupted() {
    if !service_marker_path().exists() {
        return;
    }
    // 另一个实例可能正在按计划停服务,这时不能抢着把它起回来。
    if another_instance_active() {
        return;
    }
    use crate::win::ServiceState;
    if matches!(crate::win::service_state(), ServiceState::Running) {
        clear_service_marker();
        return;
    }
    if !crate::win::service_installed() {
        clear_service_marker();
        return;
    }
    if crate::win::service_start().is_ok() {
        clear_service_marker();
        return;
    }
    #[cfg(windows)]
    {
        // 提权子进程成功后自己清标记;失败就留着,下次启动再试。目录必须随参数
        // 递过去:runas 换了管理员账号后,AppData 会指向另一个用户。
        let _ = crate::win::launch_elevated(&format!(
            "--restore-service \"{}\"",
            update_dir().display()
        ));
    }
}

/// `--restore-service <工作目录>`:提权子进程只做「起服务 + 清标记」。
pub fn cli_restore_service(dir: Option<&str>) -> Result<(), String> {
    if let Some(d) = dir {
        let _ = UPDATE_DIR_OVERRIDE.set(PathBuf::from(d.trim_matches('"')));
    }
    crate::win::service_start()?;
    clear_service_marker();
    Ok(())
}

/// 统一配置文件路径:exe 同目录(便携部署)优先,否则固定配置目录 ——
/// 用 db 的默认目录而非 data_dir(),避免「配置决定数据目录、数据目录又
/// 决定配置在哪」的自指。
pub fn config_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join(CONFIG_NAME);
            if p.exists() {
                return p;
            }
        }
    }
    crate::db::default_settings_path()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 原子写:先落一个带 pid 的 `.tmp` 再改名,断电/中断不会留下半个状态文件。
/// tmp 名字带 pid 是为了多开程序时不互相踩同一个临时文件。
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("建目录 {}: {e}", d.display()))?;
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let tmp = path.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    write_all_synced(&tmp, bytes)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("落盘 {}: {e}", path.display())
    })?;
    Ok(())
}

/// 写文件并刷到磁盘。更新包是 exe,换名之前要确认字节真的落了盘。
fn write_all_synced(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::File::create(path).map_err(|e| format!("建 {}: {e}", path.display()))?;
    f.write_all(bytes)
        .map_err(|e| format!("写 {}: {e}", path.display()))?;
    f.sync_all()
        .map_err(|e| format!("刷盘 {}: {e}", path.display()))?;
    Ok(())
}

/// 解析 `key=value` 文本:忽略空行、`#`/`;` 注释与 `[section]` 行;键统一小写。
pub fn parse_kv(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with(';')
            || (line.starts_with('[') && line.ends_with(']'))
        {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let key = k.trim().to_ascii_lowercase();
            if !key.is_empty() {
                out.insert(key, v.trim().to_string());
            }
        }
    }
    out
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn to_hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

fn bool_of(v: Option<&String>, default: bool) -> bool {
    match v.map(|s| s.as_str()) {
        Some("0") | Some("false") | Some("no") | Some("off") => false,
        Some("1") | Some("true") | Some("yes") | Some("on") => true,
        _ => default,
    }
}

// ---------------------------------------------------------------- 配置

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub url: String,
    pub interval_hours: u64,
    pub idle_secs: u64,
    pub auto_check: bool,
    pub auto_download: bool,
    /// 代理策略:空或 "system" 读环境代理,"none" 直连,其余当作代理地址。
    pub proxy: String,
    /// 更新后是否立刻把索引服务起回来(跨大版本可设 0)。
    pub restart_service: bool,
    /// 每个 Range 分段的 MiB 数,慢网可调小。
    pub chunk_bytes: u64,
    /// 服务端模型目录(结尾带 /)。空 = 界面只给手动放置指引,不提供下载。
    pub model_url: String,
    /// 实际读到的配置文件,窗口里显示出来好让用户知道该改哪一个。
    pub path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            url: String::new(),
            interval_hours: DEFAULT_INTERVAL_HOURS,
            idle_secs: DEFAULT_IDLE_SECS,
            auto_check: true,
            auto_download: true,
            proxy: "system".into(),
            restart_service: true,
            chunk_bytes: CHUNK_BYTES,
            model_url: String::new(),
            path: PathBuf::new(),
        }
    }
}

impl Config {
    fn from_kv(kv: &BTreeMap<String, String>, path: PathBuf) -> Config {
        let mut c = Config {
            path,
            ..Config::default()
        };
        c.url = kv.get("updateurl").cloned().unwrap_or_default();
        c.interval_hours = kv
            .get("checkintervalhours")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_INTERVAL_HOURS);
        c.idle_secs = kv
            .get("idledelayseconds")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_IDLE_SECS);
        c.auto_check = bool_of(kv.get("autocheck"), true);
        c.auto_download = bool_of(kv.get("autodownload"), true);
        c.restart_service = bool_of(kv.get("restartservice"), true);
        c.proxy = kv.get("proxy").cloned().unwrap_or(c.proxy);
        c.model_url = kv.get("modelurl").cloned().unwrap_or_default();
        c.chunk_bytes = kv
            .get("chunkmb")
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&v| v > 0 && v <= 32)
            .map(|v| v * 1024 * 1024)
            .unwrap_or(CHUNK_BYTES);
        c
    }
}

/// 读配置。文件不存在时直接用内存默认值,不落盘 —— 落模板交给迁移函数与
/// 「打开配置文件」:若在这里写,迁移失败留下的空模板会把下次迁移挡住。
pub fn load_config() -> Config {
    let path = config_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut cfg = Config::from_kv(&parse_kv(&text), path);
    if let Ok(v) = std::env::var("MALIANG_UPDATE_URL") {
        if !v.trim().is_empty() {
            cfg.url = v.trim().to_string();
        }
    }
    cfg
}

/// 「打开配置文件」:没有就先补一份模板,再交给系统默认编辑器。
pub fn open_config_file() {
    let p = config_path();
    if !p.exists() {
        let _ = atomic_write(&p, crate::db::SETTINGS_TEMPLATE.as_bytes());
    }
    #[cfg(windows)]
    {
        crate::win::shell_open(&p);
    }
}

// ---------------------------------------------------------------- 版本号

/// 数字段 + 可选预发布串。`v1.2.3` / `1.2` / `1.2.3-beta.2` 都认。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    nums: Vec<u64>,
    pre: Option<String>,
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim().trim_start_matches(|c| c == 'v' || c == 'V');
        let (core, pre) = match s.split_once('-') {
            Some((a, b)) if !b.is_empty() => (a, Some(b.to_string())),
            Some((a, _)) => (a, None),
            None => (s, None),
        };
        let mut nums = Vec::new();
        for p in core.split('.') {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            nums.push(p.parse::<u64>().ok()?);
        }
        if nums.is_empty() {
            return None;
        }
        Some(Version { nums, pre })
    }
}

fn cmp_pre(a: &str, b: &str) -> Ordering {
    let mut ai = a.split('.');
    let mut bi = b.split('.');
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(p), Ok(q)) => p.cmp(&q),
                    _ => x.cmp(y),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.nums.len().max(other.nums.len());
        for i in 0..n {
            let a = self.nums.get(i).copied().unwrap_or(0);
            let b = other.nums.get(i).copied().unwrap_or(0);
            let ord = a.cmp(&b);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(a), Some(b)) => cmp_pre(a, b),
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// 候选版本是否比当前新。解析不了的一律当作「不新」,不会因为一个坏字符串乱弹窗。
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (Version::parse(candidate), Version::parse(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

// ---------------------------------------------------------------- 版本清单

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub notes: String,
}

/// 把相对路径里的 `.` / `..` 收掉,并保留查询串。只用于 URL 路径部分。
fn normalize_path(p: &str) -> String {
    let cut = p.find(|c| c == '?' || c == '#').unwrap_or(p.len());
    let (path, suffix) = p.split_at(cut);
    let mut segs: Vec<&str> = Vec::new();
    for s in path.split('/') {
        match s {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            x => segs.push(x),
        }
    }
    format!("/{}{}", segs.join("/"), suffix)
}

/// 把清单里的相对 URL 按清单地址所在目录展开。只放行 http/https。
pub fn resolve_url(base: &str, link: &str) -> Result<String, String> {
    let link = link.trim();
    let lower = link.to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        return Ok(link.to_string());
    }
    // 别的 scheme 一律拒绝,别让它变成 host 后面一段奇怪的相对路径。
    if lower.contains("://") {
        return Err(format!("更新包地址只支持 http/https:{link}"));
    }
    // 清单地址本身常带 CDN 签名(?token=...),取目录前必须剥掉查询与片段。
    let base_clean = base
        .split('#')
        .next()
        .unwrap_or(base)
        .split('?')
        .next()
        .unwrap_or(base);
    let (scheme_raw, rest) = base_clean
        .split_once("://")
        .ok_or_else(|| "UpdateURL 不是合法的 http/https 地址".to_string())?;
    let scheme = scheme_raw.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        return Err("UpdateURL 只支持 http/https".into());
    }
    let host_end = rest.find('/').unwrap_or(rest.len());
    let host = &rest[..host_end];
    if host.is_empty() {
        return Err("UpdateURL 缺少主机名".into());
    }
    if link.starts_with("//") {
        return Ok(format!("{scheme}:{link}"));
    }
    if link.starts_with('/') {
        return Ok(format!("{scheme}://{host}{}", normalize_path(link)));
    }
    let path_part = &rest[host_end..];
    let dir = path_part
        .rfind('/')
        .map(|i| &path_part[..=i])
        .unwrap_or("/");
    let clean = link.strip_prefix("./").unwrap_or(link);
    let joined = format!("{dir}{clean}");
    Ok(format!("{scheme}://{host}{}", normalize_path(&joined)))
}

pub fn parse_manifest(text: &str, base: &str) -> Result<Manifest, String> {
    let kv = parse_kv(text);
    let version = kv.get("version").cloned().unwrap_or_default();
    if version.is_empty() {
        return Err("版本清单缺少 Version".into());
    }
    if Version::parse(&version).is_none() {
        return Err(format!("版本清单里的 Version 格式不对:{version}"));
    }
    let raw = kv
        .get("url")
        .or_else(|| kv.get("package"))
        .or_else(|| kv.get("file"))
        .cloned()
        .unwrap_or_default();
    if raw.is_empty() {
        return Err("版本清单缺少 URL".into());
    }
    let url = resolve_url(base, &raw)?;
    let sha256 = kv
        .get("sha256")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if !sha256.is_empty()
        && (sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("版本清单里的 Sha256 不是 64 位十六进制".into());
    }
    // 明文 http 没有哈希就等于把任意可执行文件的安装权交给链路上的任何人。
    if url.to_ascii_lowercase().starts_with("http://") && sha256.is_empty() {
        return Err("更新包走明文 http,清单必须提供 Sha256(建议改用 https)".into());
    }
    let size = kv
        .get("size")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if size > MAX_PACKAGE {
        return Err(format!("更新包声明 {} 字节,超过 {} 字节上限", size, MAX_PACKAGE));
    }
    let notes = unesc(
        &kv.get("notes")
            .or_else(|| kv.get("changelog"))
            .cloned()
            .unwrap_or_default(),
    );
    Ok(Manifest {
        version,
        url,
        sha256,
        size,
        notes,
    })
}

/// 从包 URL 取一个安全的文件名,挡住路径穿越与奇怪字符。
fn safe_name(url: &str) -> String {
    let raw = url
        .split('?')
        .next()
        .unwrap_or(url)
        .rsplit('/')
        .next()
        .unwrap_or("");
    let clean: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let clean = clean.trim_matches('.').to_string();
    if clean.is_empty() {
        "maliang.exe".into()
    } else {
        clean
    }
}

// ---------------------------------------------------------------- 落盘状态

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    pub last_check: i64,
    pub skipped: String,
    pub pending_version: String,
    pub pending_file: String,
    pub pending_sha256: String,
    pub pending_size: u64,
    pub pending_notes: String,
    /// 未下完的包:只有版本/地址/哈希三项都与清单一致时才续传。
    pub partial_version: String,
    pub partial_url: String,
    pub partial_sha256: String,
    pub partial_size: u64,
    pub applied_version: String,
    pub last_error: String,
}

impl State {
    fn from_kv(kv: &BTreeMap<String, String>) -> State {
        State {
            last_check: kv.get("lastcheck").and_then(|v| v.parse().ok()).unwrap_or(0),
            skipped: kv.get("skipped").cloned().unwrap_or_default(),
            pending_version: kv.get("pendingversion").cloned().unwrap_or_default(),
            pending_file: kv.get("pendingfile").cloned().unwrap_or_default(),
            pending_sha256: kv.get("pendingsha256").cloned().unwrap_or_default(),
            pending_size: kv
                .get("pendingsize")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            pending_notes: unesc(&kv.get("pendingnotes").cloned().unwrap_or_default()),
            partial_version: kv.get("partialversion").cloned().unwrap_or_default(),
            partial_url: kv.get("partialurl").cloned().unwrap_or_default(),
            partial_sha256: kv.get("partialsha256").cloned().unwrap_or_default(),
            partial_size: kv
                .get("partialsize")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            applied_version: kv.get("appliedversion").cloned().unwrap_or_default(),
            last_error: unesc(&kv.get("lasterror").cloned().unwrap_or_default()),
        }
    }

    fn to_text(&self) -> String {
        format!(
            "# 自动更新状态(由程序维护,不要手改)\n\
             LastCheck={}\nSkipped={}\nPendingVersion={}\nPendingFile={}\n\
             PendingSha256={}\nPendingSize={}\nPendingNotes={}\n\
             PartialVersion={}\nPartialUrl={}\nPartialSha256={}\nPartialSize={}\n\
             AppliedVersion={}\nLastError={}\n",
            self.last_check,
            self.skipped,
            self.pending_version,
            self.pending_file,
            self.pending_sha256,
            self.pending_size,
            esc(&self.pending_notes),
            self.partial_version,
            self.partial_url,
            self.partial_sha256,
            self.partial_size,
            self.applied_version,
            esc(&self.last_error),
        )
    }
}

fn load_state_at(path: &Path) -> State {
    State::from_kv(&parse_kv(&std::fs::read_to_string(path).unwrap_or_default()))
}

fn save_state_at(path: &Path, s: &State) -> Result<(), String> {
    atomic_write(path, s.to_text().as_bytes())
}

pub fn load_state() -> State {
    load_state_at(&state_path())
}

pub fn save_state(s: &State) -> Result<(), String> {
    save_state_at(&state_path(), s)
}

fn clear_pending(s: &mut State) {
    s.pending_version.clear();
    s.pending_file.clear();
    s.pending_sha256.clear();
    s.pending_size = 0;
    s.pending_notes.clear();
}

fn clear_partial(s: &mut State) {
    s.partial_version.clear();
    s.partial_url.clear();
    s.partial_sha256.clear();
    s.partial_size = 0;
}

/// 一份已经下载好、可以安装的更新。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub version: String,
    pub path: PathBuf,
    pub notes: String,
}

/// 已知 PE 机器类型:x86 / x64 / ARM64 / ARM。
fn pe_machine(head: &[u8]) -> Option<u16> {
    if head.len() < 0x40 || &head[..2] != b"MZ" {
        return None;
    }
    let off = u32::from_le_bytes(head[0x3c..0x40].try_into().ok()?) as usize;
    if off + 6 > head.len() || &head[off..off + 4] != b"PE\0\0" {
        return None;
    }
    Some(u16::from_le_bytes(head[off + 4..off + 6].try_into().ok()?))
}

/// 换进来的 exe 必须能在本机跑:ARM64 上装错 x64 包会让服务直接起不来。
fn arch_compatible(machine: u16) -> bool {
    match std::env::consts::ARCH {
        "x86_64" => matches!(machine, 0x8664 | 0x014c), // WOW64 也能跑 32 位包
        "x86" => machine == 0x014c,
        "aarch64" => matches!(machine, 0xaa64 | 0x8664 | 0x014c), // 系统模拟层
        _ => matches!(machine, 0x014c | 0x8664 | 0xaa64 | 0x01c4),
    }
}

fn check_pe_head(head: &[u8]) -> Result<(), String> {
    if head.len() < 2 || &head[..2] != b"MZ" {
        return Err("更新包不是有效的 Windows 可执行程序(MZ 头不符),已丢弃".into());
    }
    match pe_machine(head) {
        None => Err("更新包缺少合法的 PE 头,已丢弃".into()),
        Some(m) if !arch_compatible(m) => Err(format!(
            "更新包是另一种 CPU 架构的 PE(0x{m:04x}),与当前程序不符,已丢弃"
        )),
        Some(_) => Ok(()),
    }
}

fn size_mismatch(got: u64, want: u64) -> Option<String> {
    (want > 0 && got != want).then(|| {
        format!("更新包大小不符:声明 {want} 字节,收到 {got} 字节")
    })
}

fn hash_mismatch(got: &str, want: &str) -> Option<String> {
    (!want.is_empty() && got != want.to_ascii_lowercase())
        .then(|| "更新包 SHA256 校验不符,已丢弃".to_string())
}

/// 内存版校验,只给单测用;生产路径一律走 [`verify_file`] 流式版本。
#[cfg(test)]
fn verify_bytes(bytes: &[u8], size: u64, sha256: &str) -> Result<(), String> {
    check_pe_head(bytes)?;
    if let Some(e) = size_mismatch(bytes.len() as u64, size) {
        return Err(e);
    }
    if let Some(e) = hash_mismatch(&to_hex(&hmac_sha256::Hash::hash(bytes)), sha256) {
        return Err(e);
    }
    Ok(())
}

/// 流式校验:不把整包读进内存,边读边算哈希。返回 (字节数, 十六进制哈希)。
fn verify_file(path: &Path, size: u64, sha256: &str) -> Result<(u64, String), String> {
    use std::io::BufReader;
    let f = std::fs::File::open(path).map_err(|e| format!("打开更新包失败: {e}"))?;
    let mut reader = BufReader::new(f);
    let mut hasher = hmac_sha256::Hash::new();
    let mut total = 0u64;
    let mut head = Vec::with_capacity(64 * 1024);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("读取更新包失败: {e}"))?;
        if n == 0 {
            break;
        }
        let take = n.min(64 * 1024 - head.len());
        head.extend_from_slice(&buf[..take]);
        hasher.update(&buf[..n]);
        total += n as u64;
        if total > MAX_PACKAGE {
            return Err("更新包超过 512 MB 上限,已停止下载".into());
        }
    }
    check_pe_head(&head)?;
    if let Some(e) = size_mismatch(total, size) {
        return Err(e);
    }
    let got = to_hex(&hasher.finalize());
    if let Some(e) = hash_mismatch(&got, sha256) {
        return Err(e);
    }
    Ok((total, got))
}

/// 启动时的轻量检查:看头与长度即可,完整哈希留到真正安装前再算,
/// 免得每次开程序都把几百 MB 读一遍。
fn quick_verify_file(path: &Path, size: u64) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("读更新包属性失败: {e}"))?;
    if let Some(e) = size_mismatch(meta.len(), size) {
        return Err(e);
    }
    // PE 头可能在 0x40 之后较远处,给足 64KB;只读这一段,不扫整包。
    let mut head = vec![0u8; 64 * 1024];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    head.truncate(n);
    check_pe_head(&head)
}

fn pending_at(root: &Path) -> Option<Pending> {
    let state_file = state_path_at(root);
    let st = load_state_at(&state_file);
    if st.pending_version.is_empty() || st.pending_file.is_empty() {
        return None;
    }
    let path = pending_dir_at(root).join(&st.pending_file);
    if quick_verify_file(&path, st.pending_size).is_err() {
        let mut s = st.clone();
        clear_pending(&mut s);
        s.last_error = "已下载的更新包校验不符,已丢弃".into();
        let _ = save_state_at(&state_file, &s);
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(Pending {
        version: st.pending_version,
        path,
        notes: st.pending_notes,
    })
}

/// 读待安装包(公开入口,使用本机数据目录)。
pub fn pending() -> Option<Pending> {
    pending_at(&update_dir())
}

// ---------------------------------------------------------------- 网络

/// 按配置里的代理策略建 HTTP 客户端。模型下载(`crate::models`)也走这一份。
pub(crate) fn http_agent(cfg: &Config, global_secs: u64, body_secs: Option<u64>) -> ureq::Agent {
    let mut b = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(global_secs)))
        .timeout_connect(Some(Duration::from_secs(CONNECT_SECS)))
        .timeout_recv_response(Some(Duration::from_secs(30)));
    if let Some(s) = body_secs {
        b = b.timeout_recv_body(Some(Duration::from_secs(s)));
    }
    // 默认跟随系统/环境变量代理;企业网络里代理指错时可配 none 或显式地址。
    match cfg.proxy.trim().to_ascii_lowercase().as_str() {
        "" | "system" => {}
        "none" | "direct" | "0" | "off" => {
            b = b.proxy(None);
        }
        p => {
            if let Ok(proxy) = ureq::Proxy::new(p) {
                b = b.proxy(Some(proxy));
            }
        }
    }
    b.build().into()
}

fn fetch_manifest(cfg: &Config) -> Result<Manifest, String> {
    fetch_manifest_with(&http_agent(cfg, MANIFEST_GLOBAL_SECS, None), cfg)
}

fn fetch_manifest_with(agent: &ureq::Agent, cfg: &Config) -> Result<Manifest, String> {
    if cfg.url.is_empty() {
        return Err("尚未配置 UpdateURL(见「打开配置文件」)".into());
    }
    let mut resp = agent
        .get(&cfg.url)
        .call()
        .map_err(|e| format!("读取版本清单失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("版本清单返回 HTTP {}", resp.status()));
    }
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(MAX_MANIFEST)
        .read_to_vec()
        .map_err(|e| format!("读取版本清单内容失败: {e}"))?;
    let text = String::from_utf8_lossy(&bytes);
    parse_manifest(&text, &cfg.url)
}

/// 解析 `Content-Range: bytes 100-199/5000`,返回 (起点, 总长)。
fn content_range(v: &str) -> Option<(u64, u64)> {
    let spec = v.trim().strip_prefix("bytes ").unwrap_or_else(|| v.trim());
    let (span, total) = spec.trim().split_once('/')?;
    let (start, _) = span.split_once('-')?;
    Some((start.trim().parse().ok()?, total.trim().parse().ok()?))
}

fn append_all(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("续写下载文件失败: {e}"))?;
    f.write_all(bytes)
        .map_err(|e| format!("写下载文件失败: {e}"))?;
    Ok(())
}

fn report_progress(
    tx: &mpsc::Sender<Event>,
    done: u64,
    total: u64,
    last_pct: &mut i32,
    last_bytes: &mut u64,
) {
    let pct = if total > 0 {
        (done.saturating_mul(100) / total).min(100) as i32
    } else {
        0
    };
    // 知道总长就按百分比走;不知道总长也报已收字节,免得界面像卡死。
    let due = if total > 0 {
        pct != *last_pct && ((pct - *last_pct).abs() >= 2 || pct == 100)
    } else {
        done.saturating_sub(*last_bytes) >= 512 * 1024
    };
    if due {
        *last_pct = pct;
        *last_bytes = done;
        let _ = tx.send(Event::Downloading {
            pct: pct as u8,
            done,
            total,
        });
    }
}

/// 下载Outcome:完成 / 被新命令抢占 / 失败。`Preempted(None)` 表示整个程序要退出。
#[derive(Debug)]
enum DownloadOutcome {
    Done(Pending),
    Preempted(Option<Cmd>),
    Failed(String),
}

/// 下载期间在每段边界看一眼命令队列:用户点「立即检查 / 跳过 / 退出」时
/// 不必等这一整包下完(慢网下原来可能排十几分钟)。
fn poll_ctl(rx: &mpsc::Receiver<Cmd>, stop: &AtomicBool) -> Option<Option<Cmd>> {
    if stop.load(AtomicOrdering::Relaxed) {
        return Some(None);
    }
    match rx.try_recv() {
        Ok(c) => Some(Some(c)),
        Err(mpsc::TryRecvError::Empty) => None,
        Err(mpsc::TryRecvError::Disconnected) => Some(None),
    }
}

fn backoff_or_preempt(
    attempt: u32,
    rx: &mpsc::Receiver<Cmd>,
    stop: &AtomicBool,
) -> Option<Option<Cmd>> {
    let secs = 1u64 << attempt.min(4);
    for _ in 0..secs {
        std::thread::sleep(Duration::from_secs(1));
        if let Some(pre) = poll_ctl(rx, stop) {
            return Some(pre);
        }
    }
    None
}

fn run_download(
    cfg: &Config,
    m: &Manifest,
    tx: &mpsc::Sender<Event>,
    rx: &mpsc::Receiver<Cmd>,
    stop: &AtomicBool,
) -> DownloadOutcome {
    run_download_in(&update_dir(), cfg, m, tx, rx, stop)
}

/// `root` 可替换,便于在本地 HTTP 服务上完整跑通「分段 / 续传 / 校验」而不碰真数据目录。
fn run_download_in(
    root: &Path,
    cfg: &Config,
    m: &Manifest,
    tx: &mpsc::Sender<Event>,
    rx: &mpsc::Receiver<Cmd>,
    stop: &AtomicBool,
) -> DownloadOutcome {
    use std::io::Write;
    let _lock = match DirLock::acquire_at(root) {
        Some(l) => l,
        None => {
            return DownloadOutcome::Failed("另一个程序实例正在下载或安装更新,稍后再试".into())
        }
    };
    let state_file = state_path_at(root);
    let dir = pending_dir_at(root);
    let part = partial_path_at(root);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return DownloadOutcome::Failed(format!("建下载目录失败: {e}"));
    }

    // 已有成品:同版本不重复下,服务端版本回退也不会把较新的包覆盖掉。
    if let Some(p) = pending_at(root) {
        if !is_newer(&m.version, &p.version) {
            return DownloadOutcome::Done(p);
        }
    }

    let mut st = load_state_at(&state_file);
    let key_ok = st.partial_version == m.version
        && st.partial_url == m.url
        && st.partial_sha256.eq_ignore_ascii_case(m.sha256.trim())
        && st.partial_size == m.size;
    if !key_ok {
        let _ = std::fs::remove_file(&part);
        st.partial_version = m.version.clone();
        st.partial_url = m.url.clone();
        st.partial_sha256 = m.sha256.clone();
        st.partial_size = m.size;
        let _ = save_state_at(&state_file, &st);
    }
    let mut have = std::fs::metadata(&part).map(|x| x.len()).unwrap_or(0);
    if have > MAX_PACKAGE || (m.size > 0 && have > m.size) {
        let _ = std::fs::remove_file(&part);
        have = 0;
    }
    let mut total = m.size;
    let chunk = cfg.chunk_bytes.min(MAX_PACKAGE);
    // 时间预算按段大小算:约 5 KB/s 的下限,慢网不会因一个固定 15 分钟总超时全盘重来。
    let body_secs = (chunk / (5 * 1024) + 30).clamp(60, 900);
    let global_secs = body_secs + CONNECT_SECS + 30;
    let (mut last_pct, mut last_bytes) = (-1i32, 0u64);
    let _ = tx.send(Event::Downloading {
        pct: 0,
        done: have,
        total,
    });
    let mut attempts = 0u32;

    loop {
        if let Some(pre) = poll_ctl(rx, stop) {
            return DownloadOutcome::Preempted(pre);
        }
        if total > 0 && have >= total {
            break;
        }
        let end = if total > 0 {
            (have + chunk - 1).min(total - 1)
        } else {
            have + chunk - 1
        };
        let want = end - have + 1;
        let agent = http_agent(cfg, global_secs, Some(body_secs));
        let resp = agent
            .get(&m.url)
            .header("Range", format!("bytes={have}-{end}"))
            // exe 不能让传输层压缩/改写,否则字节与哈希对不上。
            .header("Accept-Encoding", "identity")
            .call();
        let mut resp = match resp {
            Ok(r) => r,
            Err(e) => {
                attempts += 1;
                if attempts >= MAX_SEGMENT_ATTEMPTS {
                    return DownloadOutcome::Failed(format!(
                        "下载连续 {attempts} 次失败: {e}(已保留 {have} 字节,下次自动续传)"
                    ));
                }
                if let Some(pre) = backoff_or_preempt(attempts, rx, stop) {
                    return DownloadOutcome::Preempted(pre);
                }
                continue;
            }
        };
        let status = resp.status();
        let clen = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(0);
        let cr = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(content_range);

        if status.as_u16() == 206 {
            let (start, cr_total) = cr.unwrap_or((have, 0));
            if start != have {
                return DownloadOutcome::Failed(format!(
                    "服务端 Range 起点不符(请求 {have},返回 {start}),已保留已下部分"
                ));
            }
            if total == 0 && cr_total > 0 {
                total = cr_total;
            }
            let bytes = match resp
                .body_mut()
                .with_config()
                .limit(chunk + 1)
                .read_to_vec()
            {
                Ok(b) => b,
                Err(e) => {
                    attempts += 1;
                    if attempts >= MAX_SEGMENT_ATTEMPTS {
                        return DownloadOutcome::Failed(format!(
                            "下载读取连续失败: {e}(已保留 {have} 字节,下次自动续传)"
                        ));
                    }
                    if let Some(pre) = backoff_or_preempt(attempts, rx, stop) {
                        return DownloadOutcome::Preempted(pre);
                    }
                    continue;
                }
            };
            if bytes.len() as u64 > want {
                return DownloadOutcome::Failed("服务端返回超出请求范围的数据,已丢弃本段".into());
            }
            if let Err(e) = append_all(&part, &bytes) {
                return DownloadOutcome::Failed(e);
            }
            have += bytes.len() as u64;
            attempts = 0;
            report_progress(tx, have, total, &mut last_pct, &mut last_bytes);
            // 短于请求长度 = 已经是文件末尾;再请求只会得到空段。
            if (bytes.len() as u64) < want || (total > 0 && have >= total) {
                break;
            }
        } else if status.is_success() {
            // 服务端不认 Range,整包返回:清掉之前所有分段,一次性流式落盘。
            // 这种服务端无法断点,失败会明确告诉用户,不会假装续传成功。
            let _ = std::fs::remove_file(&part);
            have = 0;
            if total == 0 {
                total = clen;
            }
            let mut file = match std::fs::File::create(&part) {
                Ok(f) => f,
                Err(e) => return DownloadOutcome::Failed(format!("建下载文件失败: {e}")),
            };
            let mut reader = resp.into_body().into_reader().take(MAX_PACKAGE + 1);
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(n) => n,
                    Err(e) => {
                        return DownloadOutcome::Failed(format!(
                            "下载中断: {e}(服务端不支持 Range,需重新完整下载)"
                        ))
                    }
                };
                if n == 0 {
                    break;
                }
                if let Err(e) = file.write_all(&buf[..n]) {
                    return DownloadOutcome::Failed(format!("写下载文件失败: {e}"));
                }
                have += n as u64;
                if have > MAX_PACKAGE {
                    return DownloadOutcome::Failed("更新包超过 512 MB 上限,已停止下载".into());
                }
                report_progress(tx, have, total, &mut last_pct, &mut last_bytes);
            }
            let _ = file.sync_all();
            break;
        } else {
            return DownloadOutcome::Failed(format!("更新包返回 HTTP {status}"));
        }
    }

    let expected = if m.size > 0 { m.size } else { total };
    let (real, sha) = match verify_file(&part, expected, &m.sha256) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            let mut s = load_state_at(&state_file);
            clear_partial(&mut s);
            s.last_error = e.clone();
            let _ = save_state_at(&state_file, &s);
            return DownloadOutcome::Failed(e);
        }
    };
    let name = safe_name(&m.url);
    let target = dir.join(&name);
    // 只清旧成品,别把还在续传的 .part 或本次目标删掉。
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p != part && p != target {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    if let Err(e) = std::fs::rename(&part, &target) {
        return DownloadOutcome::Failed(format!("下载包定名失败: {e}"));
    }
    let mut s = load_state_at(&state_file);
    s.pending_version = m.version.clone();
    s.pending_file = name.clone();
    s.pending_sha256 = sha;
    s.pending_size = real;
    s.pending_notes = m.notes.clone();
    s.last_error.clear();
    clear_partial(&mut s);
    if let Err(e) = save_state_at(&state_file, &s) {
        return DownloadOutcome::Failed(e);
    }
    DownloadOutcome::Done(Pending {
        version: m.version.clone(),
        path: target,
        notes: m.notes.clone(),
    })
}

// ---------------------------------------------------------------- 安装(下次生效)

/// 备份 → 换入 → 失败回滚。抽成纯函数是为了能在临时目录里单测,不必碰真 exe。
/// 备份名带 pid:多个实例或上次崩溃留下的 `.old` 不会互相覆盖。
fn replace_exe_at(exe: &Path, src: &Path) -> Result<(), String> {
    let dir = exe.parent().ok_or_else(|| "取程序目录失败".to_string())?;
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("maliang");
    let ext = exe.extension().and_then(|s| s.to_str()).unwrap_or("exe");
    let tag = std::process::id();
    let staged = dir.join(format!("{stem}.new.{tag}.{ext}"));
    let backup = dir.join(format!("{stem}.old.{tag}.{ext}"));
    // 上次的暂存副本可以随手清;`.old` 可能是上次没恢复成功的唯一好文件,不能乱删,
    // 留给 cleanup_old_exe 在确认主程序存在时再清。
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with(&format!("{stem}.new.")) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    std::fs::copy(src, &staged).map_err(|e| {
        format!("写入新程序文件失败(程序目录不可写 / 网络路径 / 被安全软件占用): {e}")
    })?;
    // 换名之前先确认新字节真的落了盘。
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&staged) {
        let _ = f.sync_all();
    }
    // Windows 允许给正在运行的映像改名,但不能覆盖它 —— 所以先挪走再换入。
    std::fs::rename(exe, &backup).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        format!("备份旧程序失败(程序文件仍被占用,可先关掉其它正在运行的实例): {e}")
    })?;
    if let Err(e) = std::fs::rename(&staged, exe) {
        match std::fs::rename(&backup, exe) {
            Ok(()) => {
                let _ = std::fs::remove_file(&staged);
                return Err(format!("替换程序文件失败,已恢复原文件: {e}"));
            }
            Err(e2) => {
                // 最坏情况:主程序路径暂时空了。把备份路径写进错误,让人能手动改回来。
                return Err(format!(
                    "替换失败且自动回滚也失败: {e} / {e2}。请手动把 {} 改回 {}",
                    backup.display(),
                    exe.display()
                ));
            }
        }
    }
    // 旧映像仍被当前进程占用时删不掉,留给下次启动的 cleanup 处理。
    let _ = std::fs::remove_file(&backup);
    Ok(())
}

/// 清掉上次替换留下的备份与暂存副本。只在 GUI 正常启动时跑;
/// 当前 exe 存在才清 `.old`,免得把上次没回滚成功的唯一好文件删掉。
pub fn cleanup_old_exe() {
    // 别的实例正在安装时,`.new.*` 是它刚写好的暂存文件,不能顺手删掉。
    if another_instance_active() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if !exe.exists() {
        return;
    }
    let Some(dir) = exe.parent() else { return };
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("maliang");
    let ext = exe.extension().and_then(|s| s.to_str()).unwrap_or("exe");
    let old_prefix = format!("{stem}.old.");
    let new_prefix = format!("{stem}.new.");
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if (n.starts_with(&old_prefix) || n.starts_with(&new_prefix))
                && n.ends_with(&format!(".{ext}"))
            {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// 安装结果。文件换成功但服务没起来不算整体失败,否则用户会重复安装。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub version: String,
    pub warning: Option<String>,
}

/// 服务状态可能在 START_PENDING / STOP_PENDING 里,先等它落定再决定要不要停。
fn wait_service_settled() -> crate::win::ServiceState {
    use crate::win::ServiceState;
    let mut last = crate::win::service_state();
    for _ in 0..30 {
        match last {
            ServiceState::Running | ServiceState::Stopped | ServiceState::NotInstalled => {
                return last
            }
            ServiceState::Transition(_) => {
                std::thread::sleep(Duration::from_millis(500));
                last = crate::win::service_state();
            }
        }
    }
    // 等不到就按「大概会跑起来」处理:宁可多停一次,也不要在半启动状态换文件。
    match last {
        // 3 = SERVICE_STOP_PENDING,其余过渡态最终都回到运行。
        ServiceState::Transition(3) => ServiceState::Stopped,
        ServiceState::Transition(_) => ServiceState::Running,
        other => other,
    }
}

/// 提权 helper 的主体:等状态 → 立标记 → 停服务 → 换文件 → 起服务。
fn do_replace(p: &Pending, restart_service: bool) -> Result<ApplyOutcome, String> {
    let _lock = DirLock::acquire()
        .ok_or_else(|| "另一个程序实例正在安装更新,稍后再试".to_string())?;
    let st = load_state();
    // 安装前做一次完整流式校验:包可能躺了几天,期间磁盘换了内容也要能挡住。
    verify_file(&p.path, st.pending_size, &st.pending_sha256)?;

    let was_running = matches!(wait_service_settled(), crate::win::ServiceState::Running);
    if was_running {
        // 先立标记再停:helper 中途被杀 / 断电时,下次启动才知道要把服务拉回来。
        mark_service_stopped();
        if let Err(e) = crate::win::service_stop() {
            // 停失败可能是「其实已经停了」,标记留着让启动兜底判断,不擅自清。
            return Err(format!("停止索引服务失败(更新未继续): {e}"));
        }
    }
    let exe = std::env::current_exe().map_err(|e| format!("取自身路径失败: {e}"))?;
    let rep = replace_exe_at(&exe, &p.path);

    let mut warning = None;
    if was_running {
        if restart_service {
            match crate::win::service_start() {
                Ok(()) => clear_service_marker(),
                Err(e) => {
                    // 保留标记,下次启动会再试;文件已换成功,不能当整体失败。
                    warning = Some(format!("索引服务没能自动恢复({e}),下次启动程序会自动重试"));
                }
            }
        } else {
            clear_service_marker();
            warning = Some("按配置保持索引服务停止,重启程序或开机后会自动启动".into());
        }
    }
    rep?;
    Ok(ApplyOutcome {
        version: p.version.clone(),
        warning,
    })
}

/// `--apply-update <工作目录> <令牌> <是否重启服务>`:由提权的自身子进程执行。
/// 工作目录与令牌由父进程传入:runas 换了管理员账号后 AppData 会指向别处,
/// 令牌则防止父进程把上一次遗留的结果文件当成本次结果。
pub fn cli_apply_update(
    dir: Option<&str>,
    token: Option<&str>,
    restart_service: Option<&str>,
) -> Result<(), String> {
    if let Some(d) = dir {
        let _ = UPDATE_DIR_OVERRIDE.set(PathBuf::from(d.trim_matches('"')));
    }
    let token = token.unwrap_or("").trim_matches('"');
    let restart = !matches!(restart_service.unwrap_or("1"), "0" | "false" | "no");
    let p = pending().ok_or_else(|| "没有待安装的更新包".to_string())?;
    let out = do_replace(&p, restart);
    // 结果文件由父进程预先建好,这里原地覆写以保留原用户的 ACL(父进程才删得掉)。
    let text = match &out {
        Ok(v) => {
            let mut st = load_state();
            st.applied_version = v.version.clone();
            st.last_error.clear();
            clear_pending(&mut st);
            clear_partial(&mut st);
            let _ = save_state(&st);
            let _ = std::fs::remove_file(&p.path);
            format!(
                "Result=ok\nToken={}\nVersion={}\nWarning={}\nMessage=更新已写入,重新启动程序后生效\n",
                token,
                esc(&v.version),
                esc(v.warning.as_deref().unwrap_or(""))
            )
        }
        Err(e) => {
            let mut st = load_state();
            st.last_error = e.clone();
            let _ = save_state(&st);
            format!("Result=err\nToken={}\nMessage={}\n", token, esc(e))
        }
    };
    let _ = std::fs::write(result_path(), text.as_bytes());
    out.map(|_| ())
}

fn apply_via_helper(cfg: &Config) -> Result<ApplyOutcome, String> {
    let p = pending().ok_or_else(|| "没有待安装的更新包".to_string())?;
    #[cfg(windows)]
    {
        // UAC 只在这一步弹一次。helper 里已经有管理员权限,停服务与写程序目录
        // 都不再需要第二次授权。窗口里会提前把这一点告诉用户。
        let rp = result_path();
        let _ = std::fs::remove_file(&rp);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let token = format!("{}-{}-{}", std::process::id(), now_secs(), nanos);
        // 先自己建一份占位结果:helper 覆写它,文件所有权仍归当前用户。
        let _ = atomic_write(
            &rp,
            format!("Result=pending\nToken={token}\n").as_bytes(),
        );
        let params = format!(
            "--apply-update \"{}\" \"{}\" \"{}\"",
            update_dir().display(),
            token,
            if cfg.restart_service { 1 } else { 0 }
        );
        let code = crate::win::launch_update_helper(&params)?
            .ok_or_else(|| "提权进程未返回退出码".to_string())?;
        let kv = parse_kv(&std::fs::read_to_string(&rp).unwrap_or_default());
        let _ = std::fs::remove_file(&rp);
        if kv.get("token").map(String::as_str) != Some(token.as_str()) {
            return Err("安装结果无法确认(提权进程可能仍在运行),请稍后重试或重启程序".into());
        }
        let ok = code == 0 && kv.get("result").map(String::as_str) == Some("ok");
        let msg = kv.get("message").cloned().unwrap_or_default();
        if !ok {
            return Err(if msg.is_empty() {
                format!("安装未完成(退出码 {code})")
            } else {
                msg
            });
        }
        let warning = kv.get("warning").cloned().filter(|s| !s.is_empty());
        Ok(ApplyOutcome {
            version: kv.get("version").cloned().unwrap_or(p.version),
            warning,
        })
    }
    #[cfg(not(windows))]
    {
        do_replace(&p, cfg.restart_service)
    }
}

/// `--check-update`:命令行跑一次检查并打印,用来验证配置与清单格式。
pub fn cli_check() -> Result<(), String> {
    let cfg = load_config();
    println!("配置文件:{}", cfg.path.display());
    if cfg.url.is_empty() {
        println!("未配置 UpdateURL,自动更新处于停用状态");
        return Ok(());
    }
    println!("更新地址:{}", cfg.url);
    let m = fetch_manifest(&cfg)?;
    let st = load_state();
    let effective = effective_version(&st);
    println!(
        "服务端版本:{}  本机版本:{}(运行中的映像 v{})  {}",
        m.version,
        effective,
        CURRENT_VERSION,
        if is_newer(&m.version, &effective) {
            "有新版本"
        } else {
            "已是最新"
        }
    );
    println!("更新包:{}", m.url);
    Ok(())
}

/// `--download-update`:只抓取清单、下载并校验,不安装。部署时用来现场确认
/// 配置、代理、服务端 Range 支持与校验值是否正确。
pub fn cli_download() -> Result<(), String> {
    let cfg = load_config();
    if cfg.url.is_empty() {
        return Err("尚未配置 UpdateURL".into());
    }
    let m = fetch_manifest(&cfg)?;
    println!("清单:{}", cfg.url);
    println!("服务端版本:{}  运行中的映像:v{}", m.version, CURRENT_VERSION);
    println!("更新包:{} (声明 {} 字节)", m.url, m.size);
    let (ev_tx, ev_rx) = mpsc::channel::<Event>();
    let (_cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let stop = Arc::new(AtomicBool::new(false));
    let out = run_download(&cfg, &m, &ev_tx, &cmd_rx, &stop);
    let mut last = String::new();
    while let Ok(ev) = ev_rx.try_recv() {
        if let Event::Downloading { pct, done, total } = ev {
            last = if total > 0 {
                format!("进度 {pct}% ({done}/{total} 字节)")
            } else {
                format!("已收 {done} 字节")
            };
        }
    }
    if !last.is_empty() {
        println!("{last}");
    }
    match out {
        DownloadOutcome::Done(p) => {
            let size = std::fs::metadata(&p.path).map(|x| x.len()).unwrap_or(0);
            println!("下载并校验完成:v{} -> {} ({size} 字节)", p.version, p.path.display());
            Ok(())
        }
        DownloadOutcome::Preempted(_) => Err("下载被中断".into()),
        DownloadOutcome::Failed(e) => Err(e),
    }
}

/// 「已经写入磁盘但还没重启」的版本也算已装上,否则安装完再点检查会重复下一次。
pub fn effective_version(st: &State) -> String {
    if !st.applied_version.is_empty() && is_newer(&st.applied_version, CURRENT_VERSION) {
        st.applied_version.clone()
    } else {
        CURRENT_VERSION.to_string()
    }
}

// ---------------------------------------------------------------- 后台线程与 UI 接口

#[derive(Debug)]
enum Cmd {
    /// 命令自带配置快照:用户刚改过 maliang.ini 时,不必重启也能用上新地址/代理。
    Check { manual: bool, cfg: Box<Config> },
    Download(Box<Manifest>, Box<Config>),
    Apply(Box<Config>),
    Skip(String),
    Stop,
}

enum Event {
    Checking,
    UpToDate(String),
    Found(Box<Manifest>),
    Downloading { pct: u8, done: u64, total: u64 },
    Ready(Box<Pending>),
    Applying,
    Applied(ApplyOutcome),
    Failed(String),
}

/// 界面上展示的阶段。`Updater::tick` 把后台事件折进这里,UI 只读快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Checking,
    UpToDate(String),
    Found(Manifest),
    Downloading { pct: u8, done: u64, total: u64 },
    Ready(Pending),
    Applying,
    Applied(ApplyOutcome),
    Failed(String),
}

impl Phase {
    pub fn label(&self) -> String {
        match self {
            Phase::Idle => "尚未检查".into(),
            Phase::Checking => "正在检查更新地址…".into(),
            Phase::UpToDate(v) => format!("已是最新版本 v{v}"),
            Phase::Found(m) => format!("发现新版本 v{}", m.version),
            Phase::Downloading { pct, done, total } => {
                let mb = |n: u64| format!("{:.1} MB", n as f64 / 1_048_576.0);
                if *total > 0 {
                    format!("正在后台下载… {pct}%  ({}/{})", mb(*done), mb(*total))
                } else {
                    format!("正在后台下载… 已收到 {}", mb(*done))
                }
            }
            Phase::Ready(p) => format!("v{} 已下载,可安装", p.version),
            Phase::Applying => "正在安装更新(会先停止索引服务)…".into(),
            Phase::Applied(v) => format!("v{} 已安装,重启程序后生效", v.version),
            Phase::Failed(e) => format!("失败:{e}"),
        }
    }

    pub fn working(&self) -> bool {
        matches!(
            self,
            Phase::Checking | Phase::Downloading { .. } | Phase::Applying
        )
    }

    /// 状态栏上的一句提示;有可安装/可下载的版本时给个可点的入口。
    pub fn status_hint(&self) -> Option<String> {
        match self {
            Phase::Found(m) => Some(format!("发现新版本 v{}", m.version)),
            Phase::Ready(p) => Some(format!("更新 v{} 已就绪", p.version)),
            Phase::Applied(v) => Some(format!("v{} 已安装,重启生效", v.version)),
            _ => None,
        }
    }
}

pub struct Updater {
    cmd: mpsc::Sender<Cmd>,
    events: mpsc::Receiver<Event>,
    pub cfg: Config,
    pub phase: Phase,
    /// 「检查更新」窗口显隐。
    pub win_open: bool,
    /// 需要用户看一眼的一句话(发现新版本、下载完成、安装结果、失败原因)。
    pub notice: Option<String>,
    pub last_check: i64,
    last_input: Instant,
    started: Instant,
    auto_done: bool,
    /// 有一轮检查/下载/安装在途。即使自动检查已完成,也要保持定时重绘,
    /// 否则手动点「立即检查」后后台回来的事件没有帧去消费。
    inflight: bool,
    /// 本次会话里被跳过的版本:迟到的下载完成事件不能把它又弹回来。
    skipped: String,
    stop: Arc<AtomicBool>,
}

impl Updater {
    pub fn new() -> Updater {
        let cfg = load_config();
        let st = load_state();
        let mut phase = Phase::Idle;
        let mut notice = None;
        let mut win_open = false;

        // 上次 helper 的结果(安装时用户直接关掉主程序,结果就会留到这次启动)。
        if result_path().exists() {
            let kv = parse_kv(&std::fs::read_to_string(result_path()).unwrap_or_default());
            match kv.get("result").map(String::as_str) {
                Some("ok") => {
                    let v = kv.get("version").cloned().unwrap_or_default();
                    let warning = kv.get("warning").cloned().filter(|s| !s.is_empty());
                    let out = ApplyOutcome {
                        version: v.clone(),
                        warning: warning.clone(),
                    };
                    phase = Phase::Applied(out);
                    notice = Some(format!(
                        "更新 v{v} 已安装,重新启动程序后即可使用新版本{}",
                        warning.map(|w| format!("。注意:{w}")).unwrap_or_default()
                    ));
                    win_open = true;
                }
                Some("err") => {
                    let msg = kv.get("message").cloned().unwrap_or_default();
                    phase = Phase::Failed(msg.clone());
                    notice = Some(format!("上次安装未完成:{msg}"));
                }
                // pending = 用户取消了 UAC 或 helper 超时还没回来,不当成失败吓人。
                _ => {}
            }
            // 结果文件可能是提权 helper 建的、当前用户删不掉。先把内容改成 seen,
            // 保证即使删除失败,下次启动也不会反复弹同一个提示。
            let _ = std::fs::write(result_path(), b"Result=seen\n");
            let _ = std::fs::remove_file(result_path());
        }
        // 失败后仍留有可安装的包时,保留失败提示,但让界面回到「可重试」状态。
        if !matches!(phase, Phase::Applied(_)) {
            if let Some(p) = pending() {
                phase = Phase::Ready(p);
            }
        }

        let due = st.last_check == 0
            || now_secs().saturating_sub(st.last_check)
                >= (cfg.interval_hours as i64).saturating_mul(3600);
        let auto_done = !cfg.auto_check || cfg.url.is_empty() || !due;

        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (ev_tx, ev_rx) = mpsc::channel::<Event>();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let _ = std::thread::Builder::new()
            .name("maliang-update".into())
            .spawn(move || worker(cmd_rx, ev_tx, worker_stop));
        Updater {
            cmd: cmd_tx,
            events: ev_rx,
            cfg,
            phase,
            win_open,
            notice,
            last_check: st.last_check,
            last_input: Instant::now(),
            started: Instant::now(),
            auto_done,
            inflight: false,
            skipped: st.skipped.clone(),
            stop,
        }
    }

    /// 用户改完 maliang.ini 点「立即检查」时,不必重启程序也能用上新配置。
    fn cfg_snapshot(&mut self) -> Box<Config> {
        self.cfg = load_config();
        Box::new(self.cfg.clone())
    }

    /// 每帧一泵:收后台事件、判空闲、决定要不要再要一帧重绘。
    ///
    /// `busy` 由 App 的 `busy()` 给(扫描/缩略图/导出/服务装卸都在内)。
    /// 有输入就算「人在用」,空闲计时清零;一直有人用也有 5 分钟兜底。
    pub fn tick(&mut self, ctx: &egui::Context, busy: bool) {
        let active = ctx.input(|i| !i.events.is_empty());
        if active {
            self.last_input = Instant::now();
        }
        while let Ok(ev) = self.events.try_recv() {
            self.on_event(ev);
        }
        let idle = self.last_input.elapsed() >= Duration::from_secs(self.cfg.idle_secs);
        let overdue = self.started.elapsed() >= MAX_AUTO_WAIT;
        if !self.auto_done && !busy && (idle || overdue) {
            self.auto_done = true;
            self.inflight = true;
            let cfg = Box::new(self.cfg.clone());
            let _ = self.cmd.send(Cmd::Check { manual: false, cfg });
        }
        if self.phase.working() || !self.auto_done || self.inflight {
            let ms = if self.phase.working() { 200 } else { 1000 };
            ctx.request_repaint_after(Duration::from_millis(ms));
        }
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Checking => self.phase = Phase::Checking,
            Event::UpToDate(v) => {
                self.last_check = now_secs();
                self.phase = Phase::UpToDate(v);
                self.inflight = false;
            }
            Event::Found(m) => {
                self.last_check = now_secs();
                if self.skipped == m.version {
                    self.inflight = false;
                    return;
                }
                self.phase = Phase::Found((*m).clone());
                // 自动下载开着时这还不是终态,继续要重绘。
                self.inflight = self.cfg.auto_download;
                self.notice = Some(format!(
                    "发现新版本 v{}(当前 v{})。{}",
                    m.version,
                    CURRENT_VERSION,
                    if self.cfg.auto_download {
                        "已在后台开始下载,完成后会再提示一次。"
                    } else {
                        "点「下载更新」可在后台取回安装包。"
                    }
                ));
                self.win_open = true;
            }
            Event::Downloading { pct, done, total } => {
                self.phase = Phase::Downloading { pct, done, total }
            }
            Event::Ready(p) => {
                self.inflight = false;
                if self.skipped == p.version {
                    return;
                }
                self.phase = Phase::Ready(*p.clone());
                self.notice = Some(format!(
                    "v{} 已下载完成。点「安装更新」即可写入程序文件;安装时会先停止索引服务,完成后自动恢复,本次会话不受影响,重启后生效。",
                    p.version
                ));
                // 下载完成也是一次明确的提示:自动检查时窗口可能已经被关掉了。
                self.win_open = true;
            }
            Event::Applying => self.phase = Phase::Applying,
            Event::Applied(out) => {
                self.phase = Phase::Applied(out.clone());
                self.inflight = false;
                self.notice = Some(format!(
                    "v{} 已安装,请重新启动程序以使用新版本{}",
                    out.version,
                    out.warning
                        .map(|w| format!("。注意:{w}"))
                        .unwrap_or_default()
                ));
                self.win_open = true;
            }
            Event::Failed(e) => {
                self.phase = Phase::Failed(e.clone());
                self.inflight = false;
                self.notice = Some(e);
            }
        }
    }

    /// 手动检查:立刻联网,不受间隔与空闲门控影响;本次启动不再自动重复检查。
    pub fn check_now(&mut self) {
        self.auto_done = true;
        self.inflight = true;
        self.notice = None;
        let cfg = self.cfg_snapshot();
        let _ = self.cmd.send(Cmd::Check { manual: true, cfg });
    }

    pub fn download(&mut self, m: &Manifest) {
        self.inflight = true;
        self.notice = None;
        let cfg = self.cfg_snapshot();
        let _ = self
            .cmd
            .send(Cmd::Download(Box::new(m.clone()), cfg));
    }

    pub fn apply(&mut self) {
        self.inflight = true;
        self.notice = None;
        let cfg = self.cfg_snapshot();
        let _ = self.cmd.send(Cmd::Apply(cfg));
    }

    pub fn skip(&mut self, version: &str) {
        // 同时置本地标记:即使下载刚好完成、Ready 事件已经排队,也不会再弹回来。
        self.skipped = version.to_string();
        self.inflight = false;
        let _ = self.cmd.send(Cmd::Skip(version.to_string()));
        self.phase = Phase::Idle;
        self.notice = Some(format!("已跳过 v{version},以后不再提示这个版本"));
    }

    pub fn stop(&self) {
        // 置标记让正在下载的 worker 在下一段边界立刻收手,而不是等 15 分钟。
        self.stop.store(true, AtomicOrdering::Relaxed);
        let _ = self.cmd.send(Cmd::Stop);
    }
}

impl Drop for Updater {
    fn drop(&mut self) {
        self.stop();
    }
}

fn handle_download(
    cfg: &Config,
    m: &Manifest,
    tx: &mpsc::Sender<Event>,
    rx: &mpsc::Receiver<Cmd>,
    stop: &Arc<AtomicBool>,
) -> Option<Cmd> {
    match run_download(cfg, m, tx, rx, stop) {
        DownloadOutcome::Done(p) => {
            let _ = tx.send(Event::Ready(Box::new(p)));
            None
        }
        DownloadOutcome::Preempted(next) => next,
        DownloadOutcome::Failed(e) => {
            fail(tx, &e);
            None
        }
    }
}

fn worker(rx: mpsc::Receiver<Cmd>, tx: mpsc::Sender<Event>, stop: Arc<AtomicBool>) {
    // 下载被新命令抢占时,把那条命令留到下一轮直接处理,不再排到队尾。
    let mut queued: Option<Cmd> = None;
    loop {
        if stop.load(AtomicOrdering::Relaxed) {
            break;
        }
        let cmd = match queued.take() {
            Some(c) => c,
            None => match rx.recv() {
                Ok(c) => c,
                Err(_) => break,
            },
        };
        match cmd {
            Cmd::Stop => break,
            Cmd::Skip(v) => {
                let mut st = load_state();
                st.skipped = v;
                let _ = save_state(&st);
            }
            Cmd::Check { manual, cfg } => {
                let _ = tx.send(Event::Checking);
                let mut st = load_state();
                match fetch_manifest(&cfg) {
                    Ok(m) => {
                        st.last_check = now_secs();
                        st.last_error.clear();
                        let _ = save_state(&st);
                        let effective = effective_version(&st);
                        let newer = is_newer(&m.version, &effective);
                        let skipped = !manual && st.skipped == m.version;
                        if !newer || skipped {
                            let _ = tx.send(Event::UpToDate(m.version));
                            continue;
                        }
                        let _ = tx.send(Event::Found(Box::new(m.clone())));
                        if cfg.auto_download {
                            queued = handle_download(&cfg, &m, &tx, &rx, &stop);
                        }
                    }
                    Err(e) => {
                        st.last_error = e.clone();
                        let _ = save_state(&st);
                        fail(&tx, &e);
                    }
                }
            }
            Cmd::Download(m, cfg) => {
                queued = handle_download(&cfg, &m, &tx, &rx, &stop);
            }
            Cmd::Apply(cfg) => {
                let _ = tx.send(Event::Applying);
                match apply_via_helper(&cfg) {
                    Ok(v) => {
                        let _ = tx.send(Event::Applied(v));
                    }
                    Err(e) => fail(&tx, &e),
                }
            }
        }
    }
}

fn fail(tx: &mpsc::Sender<Event>, e: &str) {
    let mut st = load_state();
    st.last_error = e.to_string();
    let _ = save_state(&st);
    let _ = tx.send(Event::Failed(e.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("maliang-upd-{}-{name}", std::process::id()))
    }

    /// 造一个最小 PE 头,让校验函数能在本机架构下通过。
    fn host_machine() -> u16 {
        match std::env::consts::ARCH {
            "x86" => 0x014c,
            "aarch64" => 0xaa64,
            _ => 0x8664,
        }
    }

    fn foreign_machine() -> u16 {
        match std::env::consts::ARCH {
            "x86_64" => 0xaa64,
            "aarch64" => 0x01c4,
            _ => 0x8664,
        }
    }

    fn fake_pe(machine: u16, tail: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; 0x40];
        v[0..2].copy_from_slice(b"MZ");
        v[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        v.extend_from_slice(b"PE\0\0");
        v.extend_from_slice(&machine.to_le_bytes());
        v.extend_from_slice(tail);
        v
    }

    fn test_root(name: &str) -> PathBuf {
        let p = tmp(name);
        let _ = std::fs::remove_dir_all(&p);
        let _ = std::fs::create_dir_all(&p);
        p
    }

    /// 测试专用:直连、不读环境里的代理变量,免得本机代理把 127.0.0.1 也劫走。
    fn direct_agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .timeout_connect(Some(Duration::from_secs(3)))
            .proxy(None)
            .build()
            .into()
    }

    type Reply = (u16, Vec<(String, String)>, Vec<u8>);
    type Handler = Box<dyn Fn(usize, &str, &BTreeMap<String, String>) -> Reply + Send>;

    /// 本地 HTTP 服务:每次请求交给 handler 决定状态码/头/体,能模拟
    /// 206 分段、200 忽略 Range、500 瞬时故障,也能记录客户端发来的 Range。
    struct TestServer {
        port: u16,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, AtomicOrdering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn serve(handler: Handler) -> TestServer {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut idx = 0usize;
            while !stop2.load(AtomicOrdering::Relaxed) {
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
                        let mut reader = std::io::BufReader::new(sock.try_clone().unwrap());
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            continue;
                        }
                        let mut hdrs = BTreeMap::new();
                        loop {
                            let mut l = String::new();
                            if reader.read_line(&mut l).unwrap_or(0) == 0
                                || l.trim_end().is_empty()
                            {
                                break;
                            }
                            if let Some((k, v)) = l.split_once(':') {
                                hdrs.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                            }
                        }
                        let (code, extra, body) = handler(idx, line.trim(), &hdrs);
                        idx += 1;
                        let reason = match code {
                            200 => "OK",
                            206 => "Partial Content",
                            _ => "Error",
                        };
                        let mut head = format!(
                            "HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                            body.len()
                        );
                        for (k, v) in extra {
                            head.push_str(&format!("{k}: {v}\r\n"));
                        }
                        head.push_str("\r\n");
                        let _ = sock.write_all(head.as_bytes());
                        let _ = sock.write_all(&body);
                        let _ = sock.flush();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        TestServer {
            port,
            stop,
            handle: Some(handle),
        }
    }

    /// 把 `bytes=100-199` 解析成 (100, 199),并钳到文件范围内。
    fn split_range(r: &str, total: usize) -> (usize, usize) {
        let spec = r.strip_prefix("bytes=").unwrap_or("0-");
        let (a, b) = spec.split_once('-').unwrap_or(("0", ""));
        let start = a.parse::<usize>().unwrap_or(0).min(total.saturating_sub(1));
        let end = b
            .parse::<usize>()
            .unwrap_or(total.saturating_sub(1))
            .min(total.saturating_sub(1));
        (start, end)
    }

    fn test_cfg(root: &Path, port: u16, chunk: u64) -> (Config, String) {
        let url = format!("http://127.0.0.1:{port}/latest.ini?token=abc");
        let cfg = Config {
            url: url.clone(),
            proxy: "none".into(),
            chunk_bytes: chunk,
            path: root.join("maliang.ini"),
            ..Config::default()
        };
        (cfg, url)
    }

    /// 清单抓取 → 带查询串的相对 URL 展开 → 多段 Range 下载 → 流式校验 → 落盘状态。
    #[test]
    fn manifest_fetch_and_range_download_roundtrip() {
        let package = fake_pe(host_machine(), &vec![7u8; 200_000]);
        let sha = to_hex(&hmac_sha256::Hash::hash(&package));
        let manifest = format!(
            "[update]\nVersion=0.9.0\nURL=maliang.exe?x=1\nSize={}\nSha256={sha}\nNotes=第一行\\n第二行\n",
            package.len()
        );
        let range_log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let log = range_log.clone();
        let pkg = package.clone();
        let man = manifest.clone();
        let server = serve(Box::new(move |_idx, line, hdrs| {
            if line.starts_with("GET /latest.ini") {
                return (200, vec![], man.as_bytes().to_vec());
            }
            let r = hdrs.get("range").cloned().unwrap_or_default();
            log.lock().unwrap().push(r.clone());
            let (s, e) = split_range(&r, pkg.len());
            (
                206,
                vec![(
                    "Content-Range".into(),
                    format!("bytes {s}-{e}/{}", pkg.len()),
                )],
                pkg[s..=e].to_vec(),
            )
        }));
        let root = test_root("range");
        let (cfg, url) = test_cfg(&root, server.port, 64 * 1024);
        let m = fetch_manifest_with(&direct_agent(), &cfg).unwrap();
        assert_eq!(m.version, "0.9.0");
        assert_eq!(m.url, format!("http://127.0.0.1:{}/maliang.exe?x=1", server.port));
        assert_eq!(m.notes, "第一行\n第二行");
        assert!(is_newer(&m.version, CURRENT_VERSION));

        let (ev_tx, ev_rx) = mpsc::channel();
        let (_cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let stop = AtomicBool::new(false);
        let out = run_download_in(&root, &cfg, &m, &ev_tx, &cmd_rx, &stop);
        let p = match out {
            DownloadOutcome::Done(p) => p,
            other => panic!("下载应当完成,实际:{other:?}"),
        };
        assert_eq!(std::fs::read(&p.path).unwrap(), package);
        assert_eq!(p.version, "0.9.0");
        // 200KB / 64KB = 4 段,进度事件里应该能看到 100%。
        let evs: Vec<Event> = ev_rx.try_iter().collect();
        assert!(evs.iter().any(|e| matches!(
            e,
            Event::Downloading { pct: 100, .. }
        )));
        assert!(range_log.lock().unwrap().len() >= 4, "应当分成多段请求");
        let st = load_state_at(&state_path_at(&root));
        assert_eq!(st.pending_version, "0.9.0");
        assert_eq!(st.pending_size, package.len() as u64);
        assert_eq!(st.pending_sha256, sha);
        assert!(st.partial_version.is_empty(), "完成后应清掉续传标记");
        assert_eq!(pending_at(&root).unwrap().version, "0.9.0");
        let _ = url;
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 上次下到一半 + 服务端第一次 500:应当重试并从已有字节继续,而不是从头下。
    #[test]
    fn partial_download_resumes_after_transient_failure() {
        let package = fake_pe(host_machine(), &vec![3u8; 150_000]);
        let sha = to_hex(&hmac_sha256::Hash::hash(&package));
        let half = 70_000usize;
        let ranges = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = ranges.clone();
        let pkg = package.clone();
        let server = serve(Box::new(move |idx, _line, hdrs| {
            if idx == 0 {
                return (500, vec![], b"transient".to_vec());
            }
            let r = hdrs.get("range").cloned().unwrap_or_default();
            seen.lock().unwrap().push(r.clone());
            let (s, e) = split_range(&r, pkg.len());
            (
                206,
                vec![(
                    "Content-Range".into(),
                    format!("bytes {s}-{e}/{}", pkg.len()),
                )],
                pkg[s..=e].to_vec(),
            )
        }));
        let root = test_root("resume");
        let (cfg, _url) = test_cfg(&root, server.port, 64 * 1024);
        let m = parse_manifest(
            &format!("Version=0.9.1\nURL=maliang.exe\nSize={}\nSha256={sha}\n", package.len()),
            &cfg.url,
        )
        .unwrap();
        // 预置半成品与匹配的续传状态。
        let part = partial_path_at(&root);
        std::fs::create_dir_all(part.parent().unwrap()).unwrap();
        std::fs::write(&part, &package[..half]).unwrap();
        let mut st = State::default();
        st.partial_version = m.version.clone();
        st.partial_url = m.url.clone();
        st.partial_sha256 = m.sha256.clone();
        st.partial_size = m.size;
        save_state_at(&state_path_at(&root), &st).unwrap();

        let (ev_tx, _ev_rx) = mpsc::channel();
        let (_cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let stop = AtomicBool::new(false);
        let p = match run_download_in(&root, &cfg, &m, &ev_tx, &cmd_rx, &stop) {
            DownloadOutcome::Done(p) => p,
            other => panic!("续传应当完成,实际:{other:?}"),
        };
        assert_eq!(std::fs::read(&p.path).unwrap(), package);
        let logged = ranges.lock().unwrap();
        assert!(!logged.is_empty());
        assert!(
            logged[0].starts_with(&format!("bytes={half}-")),
            "失败重来时第一个分段应从已有字节之后开始:{logged:?}"
        );
        assert!(logged.len() >= 2, "剩余部分应继续分段取回:{logged:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 服务端不认 Range 时回 200 整包,也要能正确落盘并校验。
    #[test]
    fn server_ignoring_range_falls_back_to_single_stream() {
        let package = fake_pe(host_machine(), &vec![9u8; 90_000]);
        let sha = to_hex(&hmac_sha256::Hash::hash(&package));
        let pkg = package.clone();
        let server = serve(Box::new(move |_idx, _line, _hdrs| (200, vec![], pkg.clone())));
        let root = test_root("no-range");
        let (cfg, _url) = test_cfg(&root, server.port, 32 * 1024);
        let m = parse_manifest(
            &format!("Version=0.9.2\nURL=pkg.exe\nSize={}\nSha256={sha}\n", package.len()),
            &cfg.url,
        )
        .unwrap();
        let (ev_tx, _ev_rx) = mpsc::channel();
        let (_cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let stop = AtomicBool::new(false);
        let p = match run_download_in(&root, &cfg, &m, &ev_tx, &cmd_rx, &stop) {
            DownloadOutcome::Done(p) => p,
            other => panic!("整包下载应当完成,实际:{other:?}"),
        };
        assert_eq!(std::fs::read(&p.path).unwrap(), package);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 程序要退出时,下载应在下一段边界立刻收手,不留半截成品。
    #[test]
    fn stop_flag_preempts_download() {
        let root = test_root("cancel");
        let (cfg, _url) = test_cfg(&root, 1, 64 * 1024);
        let m = parse_manifest(
            &format!(
                "Version=0.9.3\nURL=pkg.exe\nSha256={}\n",
                "a".repeat(64)
            ),
            &cfg.url,
        )
        .unwrap();
        let (ev_tx, _ev_rx) = mpsc::channel();
        let (_cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let stop = AtomicBool::new(true);
        assert!(matches!(
            run_download_in(&root, &cfg, &m, &ev_tx, &cmd_rx, &stop),
            DownloadOutcome::Preempted(None)
        ));
        assert!(load_state_at(&state_path_at(&root))
            .pending_version
            .is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn single_instance_lock_and_content_range() {
        let root = test_root("lock");
        let a = DirLock::acquire_at(&root).unwrap();
        assert!(DirLock::acquire_at(&root).is_none(), "第二个实例应被挡住");
        drop(a);
        assert!(DirLock::acquire_at(&root).is_some(), "锁释放后应能重新拿到");
        assert_eq!(content_range("bytes 100-199/5000"), Some((100, 5000)));
        assert_eq!(content_range("bytes 0-0/1"), Some((0, 1)));
        assert_eq!(content_range("garbage"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kv_ignores_comments_sections_and_bom() {
        let text = "\u{feff}[update]\n# 注释\n; 注释\n UpdateURL = https://a/b.ini \nCheckIntervalHours=12\n";
        let kv = parse_kv(text);
        assert_eq!(kv.get("updateurl").unwrap(), "https://a/b.ini");
        assert_eq!(kv.get("checkintervalhours").unwrap(), "12");
        assert!(!kv.contains_key("[update]"));
    }

    #[test]
    fn config_defaults_and_overrides() {
        let c = Config::from_kv(&parse_kv(""), PathBuf::from("x.ini"));
        assert_eq!(c.interval_hours, DEFAULT_INTERVAL_HOURS);
        assert_eq!(c.idle_secs, DEFAULT_IDLE_SECS);
        assert!(c.auto_check && c.auto_download);
        assert!(c.url.is_empty());
        assert_eq!(c.chunk_bytes, CHUNK_BYTES);
        assert!(c.restart_service);
        assert_eq!(c.proxy, "system");
        // 模型下载默认关:没填 ModelURL 时界面只给手动放置指引。
        assert!(c.model_url.is_empty());
        let c = Config::from_kv(
            &parse_kv("UpdateURL=https://a/b\nAutoCheck=0\nAutoDownload=no\nIdleDelaySeconds=3\nCheckIntervalHours=0\nProxy=none\nRestartService=0\nChunkMB=4\nModelURL=https://a/models/"),
            PathBuf::from("x.ini"),
        );
        assert_eq!(c.url, "https://a/b");
        assert!(!c.auto_check && !c.auto_download);
        assert_eq!(c.idle_secs, 3);
        // 0 小时是明显写错的值,退回默认,而不是每次启动都联网。
        assert_eq!(c.interval_hours, DEFAULT_INTERVAL_HOURS);
        assert_eq!(c.proxy, "none");
        assert!(!c.restart_service);
        assert_eq!(c.chunk_bytes, 4 * 1024 * 1024);
        assert_eq!(c.model_url, "https://a/models/");
    }

    #[test]
    fn version_ordering_handles_padding_and_prerelease() {
        assert!(is_newer("0.3.0", "0.2.0"));
        assert!(is_newer("v1.0", "0.99.99"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("0.3.0-beta.1", "0.3.0"));
        assert!(is_newer("0.3.0", "0.3.0-rc.2"));
        assert!(is_newer("1.0.1", "1.0"));
        assert!(is_newer("0.3.0-beta.10", "0.3.0-beta.2"));
        assert!(!is_newer("乱写", "0.2.0"));
    }

    #[test]
    fn url_resolution_covers_absolute_and_relative() {
        let base = "https://example.com/releases/latest.ini";
        assert_eq!(
            resolve_url(base, "https://cdn.example.com/m.exe").unwrap(),
            "https://cdn.example.com/m.exe"
        );
        assert_eq!(
            resolve_url(base, "maliang-0.3.exe").unwrap(),
            "https://example.com/releases/maliang-0.3.exe"
        );
        assert_eq!(
            resolve_url(base, "/files/m.exe").unwrap(),
            "https://example.com/files/m.exe"
        );
        assert_eq!(
            resolve_url(base, "./m.exe").unwrap(),
            "https://example.com/releases/m.exe"
        );
        assert!(resolve_url("ftp://example.com/x", "m.exe").is_err());
        assert!(resolve_url("example.com/x", "m.exe").is_err());
        // CDN 签名清单:取目录时不能把 ?token 当路径的一部分。
        assert_eq!(
            resolve_url("https://example.com/releases/latest.ini?token=abc&x=1", "m.exe")
                .unwrap(),
            "https://example.com/releases/m.exe"
        );
        // 相对路径里的 .. 要收掉,查询串要原样留着。
        assert_eq!(
            resolve_url(base, "../files/m.exe?v=2").unwrap(),
            "https://example.com/files/m.exe?v=2"
        );
        assert_eq!(
            resolve_url("HTTPS://example.com/a/latest.ini", "m.exe").unwrap(),
            "https://example.com/a/m.exe"
        );
        assert!(resolve_url(base, "file://etc/passwd").is_err());
    }

    #[test]
    fn manifest_parsing_validates_fields() {
        let text = "[update]\nVersion=0.3.0\nURL=maliang-0.3.exe\nSize=1234\nSha256=AB\nNotes=修复\\n若干问题\n";
        assert!(parse_manifest(text, "https://a/b.ini").is_err(), "短哈希要拒绝");
        let text = "Version=0.3.0\nURL=./maliang-0.3.exe\nSize=1234\nNotes=修复\\n若干问题\n";
        let m = parse_manifest(text, "https://a/b.ini").unwrap();
        assert_eq!(m.version, "0.3.0");
        assert_eq!(m.url, "https://a/maliang-0.3.exe");
        assert_eq!(m.size, 1234);
        assert_eq!(m.notes, "修复\n若干问题");
        assert!(parse_manifest("URL=x.exe", "https://a/b").is_err());
        assert!(parse_manifest("Version=0.3.0", "https://a/b").is_err());
        assert!(parse_manifest("Version=nope\nURL=x.exe", "https://a/b").is_err());
        // 明文 http 没有哈希时不接受任意可执行文件。
        assert!(parse_manifest("Version=0.3.0\nURL=x.exe", "http://a/b.ini").is_err());
        assert!(parse_manifest(
            "Version=0.3.0\nURL=x.exe\nSize=999999999999",
            "https://a/b.ini"
        )
        .is_err());
    }

    #[test]
    fn safe_names_block_traversal() {
        assert_eq!(safe_name("https://a/b/maliang-0.3.exe"), "maliang-0.3.exe");
        let hostile = safe_name("https://a/b/..%2Fevil.exe");
        assert!(
            !hostile.contains('/') && !hostile.contains('\\') && !hostile.starts_with(".."),
            "{hostile} 不该能逃出下载目录"
        );
        assert_eq!(safe_name("https://a/b/"), "maliang.exe");
        assert_eq!(safe_name("https://a/b/x.exe?token=1"), "x.exe");
    }

    #[test]
    fn state_roundtrip_keeps_multiline_notes() {
        let path = tmp("state.ini");
        let mut s = State::default();
        s.last_check = 1_700_000_000;
        s.pending_version = "0.3.0".into();
        s.pending_file = "m.exe".into();
        s.pending_notes = "第一行\n第二行 \\ 反斜杠".into();
        s.last_error = "一行\n又一行".into();
        s.partial_version = "0.4.0".into();
        s.partial_url = "https://a/pkg.exe".into();
        s.partial_sha256 = "b".repeat(64);
        s.partial_size = 12345;
        save_state_at(&path, &s).unwrap();
        assert_eq!(load_state_at(&path), s);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn effective_version_counts_applied_but_not_restarted() {
        let mut s = State::default();
        assert_eq!(effective_version(&s), CURRENT_VERSION);
        s.applied_version = "999.0.0".into();
        assert_eq!(effective_version(&s), "999.0.0");
        s.applied_version = "0.0.1".into();
        assert_eq!(effective_version(&s), CURRENT_VERSION);
    }

    #[test]
    fn verification_rejects_bad_payloads() {
        let good = fake_pe(host_machine(), b"hello");
        let sha = to_hex(&hmac_sha256::Hash::hash(&good));
        assert!(verify_bytes(&good, good.len() as u64, &sha).is_ok());
        assert!(verify_bytes(&good, 999, &sha).is_err(), "大小不符");
        assert!(verify_bytes(&good, 0, &"0".repeat(64)).is_err(), "哈希不符");
        assert!(verify_bytes(b"PK\x03\x04", 0, "").is_err(), "非 PE 头");
        assert!(verify_bytes(&good, 0, "").is_ok(), "没写哈希时只查 PE 头");
        // 架构不对的包要挡住(ARM64 上装 x64 包之类)。
        let foreign = fake_pe(foreign_machine(), b"hello");
        assert!(verify_bytes(&foreign, 0, "").is_err(), "异构 PE 要拒绝");
        // 流式校验与内存校验结论一致。
        let root = test_root("verify-file");
        let path = root.join("pkg.exe");
        std::fs::write(&path, &good).unwrap();
        let (n, got) = verify_file(&path, good.len() as u64, &sha).unwrap();
        assert_eq!(n, good.len() as u64);
        assert_eq!(got, sha);
        assert!(verify_file(&path, 0, &"0".repeat(64)).is_err());
        assert!(quick_verify_file(&path, good.len() as u64).is_ok());
        assert!(quick_verify_file(&path, 999).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn replace_exe_swaps_and_rolls_back() {
        let dir = test_root("replace");
        let exe = dir.join("maliang.exe");
        let new1 = dir.join("src1.bin");
        let new2 = dir.join("src2.bin");
        std::fs::write(&exe, b"MZ-old").unwrap();
        std::fs::write(&new1, b"MZ-new").unwrap();
        std::fs::write(&new2, b"MZ-newer").unwrap();
        replace_exe_at(&exe, &new1).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ-new");
        // 备份在测试里没人占用,应当已被清掉。
        assert!(std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().contains(".old.")));
        // 再来一次(模拟连续安装),仍应成功。
        replace_exe_at(&exe, &new2).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ-newer");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn phase_labels_are_readable() {
        assert_eq!(Phase::Idle.label(), "尚未检查");
        let dl = Phase::Downloading {
            pct: 42,
            done: 1_048_576,
            total: 2_097_152,
        };
        assert!(dl.working());
        assert!(dl.label().contains("42%"));
        let no_total = Phase::Downloading {
            pct: 0,
            done: 1_048_576,
            total: 0,
        };
        assert!(no_total.label().contains("1.0 MB"), "总长未知也要报字节数");
        assert!(!Phase::Ready(Pending {
            version: "0.3".into(),
            path: PathBuf::new(),
            notes: String::new(),
        })
        .working());
        assert!(Phase::Found(Manifest {
            version: "0.3".into(),
            url: String::new(),
            sha256: String::new(),
            size: 0,
            notes: String::new(),
        })
        .status_hint()
        .is_some());
        let out = ApplyOutcome {
            version: "0.4".into(),
            warning: None,
        };
        assert!(Phase::Applied(out).status_hint().unwrap().contains("0.4"));
    }
}
