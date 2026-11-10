//! SQLite 访问网关说明(整个软件只此一处直接摸连接):
//!
//! - **写单路**:全库只有 `writer` 一条写连接,所有写走 [`Store::write`]
//!   (同一把锁天然串行,WAL 下单条小写 ~1ms)。`Store::write_async` 是
//!   后台写线程,只留给"火后不理、写完不马上读回"的批量场景 ——
//!   读-改-读的路径必须用同步写,否则读到旧值。
//! - **读多路**:WAL 模式下读不阻塞写、写不阻塞读;只读连接池
//!   (`readers`)供多线程并发共读,后台扫描/缩略图/监听线程的查询
//!   不再和 UI 抢同一把连接锁。
//! - **读缓存**:高频逐帧查询(设置项)走进程内缓存,`get_setting`
//!   命中缓存不碰库。
//!
//! 备份/压缩前调 [`Store::flush`] 等异步队列排干。

use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// 异步写任务:闭包拿到唯一写连接。
type WriteJob = Box<dyn FnOnce(&mut Connection) + Send>;

/// 只读连接池里最多闲置的连接数(超出的用完即弃)。
const READ_POOL_IDLE: usize = 4;

pub struct Store {
    /// 唯一写连接(WAL)。写-写天然串行;异步写线程与同步写共用这一条。
    writer: Arc<Mutex<Connection>>,
    /// 只读连接池(空闲最多 `READ_POOL_IDLE` 个,超出即弃)。
    readers: Mutex<Vec<Connection>>,
    /// 库文件路径;`None` = 内存库(测试),读侧回退写连接。
    file: Option<PathBuf>,
    /// 设置项进程内缓存(避免逐帧查库)。
    settings: Mutex<HashMap<String, Option<String>>>,
    /// 异步写队列入口;`None` = 写线程还没起。
    write_tx: Mutex<Option<mpsc::Sender<WriteJob>>>,
    /// 写线程句柄(Drop 时排干队列再退出,不丢尾部写)。
    write_done: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// 本次启动硬排除清理摘掉的行数(0 = 没动);缩略图 GC 的触发信号之一。
    pub hard_purged: std::sync::atomic::AtomicUsize,
}

/// 统一配置文件名:数据目录定位([location] 节)与自动更新/模型下载
/// ([update] 节)都在这一份里,固定放**默认**目录,先于任何库打开就能读到
/// (避免「配置本身存在哪」的自指问题)。环境变量 MALIANG_DATA_DIR 仍最高优先。
pub(crate) const SETTINGS_FILE: &str = "maliang.ini";
/// 11-07 统一配置前的旧 marker 名,只读兼容(迁移函数会把旧文件并入删除)。
const LEGACY_LOCATION_FILE: &str = "data.location";
/// 11-07 统一配置前的旧升级配置名(曾放 exe 同目录或数据目录)。
const LEGACY_UPDATE_FILE: &str = "update.ini";

/// 出厂默认的版本清单地址。配置文件里不写 UpdateURL 就用它;
/// 显式写成 `UpdateURL=`(空值)则停用自动更新,填别的地址可换源。
/// 这样清掉本机配置或全新安装也能直接检查更新。
pub const DEFAULT_UPDATE_URL: &str = "http://47.109.200.26/maliang/update/latest.ini";
/// 模型目录出厂默认为空:权重许可由用户自行审查,需要代下载时自配 ModelURL。
pub const DEFAULT_MODEL_URL: &str = "http://47.109.200.26/maliang/models/";

/// 统一配置文件模板(首启无配置时写入;[location] 归本模块,[update] 节
/// 供升级/模型下载用,解析端只认键不认节,节头仅作视觉分组)。
pub const SETTINGS_TEMPLATE: &str = "\
# Relyto.Maliang 统一配置文件:数据目录 + 自动更新 + 模型下载。
# 本文件固定存放于此目录(便携版可放 exe 同目录),不随数据目录迁移。
# 改完重启程序生效;「帮助 > 检查更新 > 打开配置文件」可直达。

[location]
# 数据目录:索引库、全盘索引、人脸模型放这里。
# 留空 = 默认(Windows: %APPDATA%\\Relyto\\Maliang),此时缩略图缓存放系统缓存目录。
# 目录迁走后,缩略图缓存也跟着放到这里的 cache\\ 子目录(重启后生效);
# 旧系统缓存的四档缩略图会在下次启动后自动搬过来,历史备份 *.reset-* 保留原处。
# 改动后重启,旧目录内容会自动搬到新目录,搬完旧目录会被删除。
data_dir=

[update]
# UpdateURL 指向一份版本清单(纯文本 key=value)。
# 不写这一行 = 用程序内置的默认清单地址;改成自己的地址 = 换源;
# 写成 UpdateURL=(空值) = 停用自动更新。
UpdateURL=http://47.109.200.26/maliang/update/latest.ini

# 两次自动检查之间的最小间隔(小时)。手动检查不受此限制。
CheckIntervalHours=24

# 启动后空闲多少秒才发起第一次自动检查(扫描/导入/服务装卸中一律推迟)。
IdleDelaySeconds=20

# 启动后是否自动检查;0 = 只在手动点「检查更新」时联网。
AutoCheck=1

# 发现新版本后是否自动在后台下载;0 = 只提示,由用户点「下载」。
AutoDownload=1

# 代理:system(默认,读系统/环境变量代理) / none(直连) / http://host:port。
# 企业网络里环境变量代理指向不对时,可填 none 或显式地址。
Proxy=system

# 安装后是否立刻把索引服务起回来。1(默认)= 尽量不中断索引;
# 0 = 保持停止,等程序重启或开机自启。跨大版本、服务与界面协议不兼容时设 0 更稳。
RestartService=1

# 下载分段大小(MB)。慢网可调小到 1,让每段的时间预算更容易满足。
ChunkMB=2

# 模型镜像目录:服务端放模型文件的目录,结尾带「/」。
# 下载优先走内置官方地址;某文件在官方源缺失时按所选方案的固定文件名
# (face_det.onnx + face_rec_sface.onnx 等)到这个镜像目录兜底,
# 存进数据目录的 models\\。填自己的地址前请自行确认模型分发许可(建议 https);
# 写成 ModelURL=(空值) = 不启用镜像兜底。
ModelURL=http://47.109.200.26/maliang/models/
";

/// 默认数据目录(marker 文件所在;env/marker 都不生效时它就是数据目录)。
fn default_data_dir() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("Relyto").join("Maliang")
}

/// 解析 marker 文本:第一项 = 生效数据目录,第二项 = 待迁移来源(可选)。
fn parse_location(text: &str) -> (Option<PathBuf>, Option<PathBuf>) {
    let mut data = None;
    let mut pending = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("data_dir=") {
            let v = v.trim();
            if !v.is_empty() {
                data = Some(PathBuf::from(v));
            }
        } else if let Some(v) = line.strip_prefix("pending_from=") {
            let v = v.trim();
            if !v.is_empty() {
                pending = Some(PathBuf::from(v));
            }
        }
    }
    (data, pending)
}

/// 机器级标记:索引服务跑在 LocalSystem 下,它的「用户目录」是
/// `C:\Windows\System32\config\systemprofile\...`,读不到登录用户写的
/// `data.location`,于是会悄悄把全盘库建在系统盘里(
/// 服务在 systemprofile 下建出 94 MB 索引 + 86 MB 库,而 UI 读的是 D 盘
/// 那份 —— 两边各一套,图库永远看不到服务入库的元数据)。
/// 标记放 `ProgramData` 一份,`--install-service`(提权)写、服务读。
fn machine_marker_path() -> PathBuf {
    machine_default_dir().join(SETTINGS_FILE)
}

/// 机器级标记文本:统一名优先,旧 data.location 兜底。
fn machine_marker_text() -> Option<String> {
    read_marker_text(&machine_default_dir())
}

/// 机器级默认数据目录。**必须与账号无关**:
/// `dirs::config_dir()` 在 Windows 上就是 `FOLDERID_RoamingAppData`
/// (取依赖源码坐实,dirs-sys 0.5 走的是 RoamingAppData),登录用户是
/// `C:\Users\…\AppData\Roaming`,而 LocalSystem 服务是
/// `C:\Windows\System32\config\systemprofile\AppData\Roaming` —— 用它当
/// 「机器级」等于什么都没修。
/// `PROGRAMDATA` 是系统级环境变量,服务和任何账号读到的都是同一个
/// `C:\ProgramData`。
#[cfg(windows)]
fn machine_default_dir() -> PathBuf {
    let base = std::env::var_os("PROGRAMDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("Relyto").join("Maliang")
}

/// 非 Windows 没有服务形态,机器级只是给用户级兜个底。
#[cfg(not(windows))]
fn machine_default_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("Relyto").join("Maliang")
}

fn machine_marker_dir() -> Option<PathBuf> {
    machine_marker_text().and_then(|t| parse_location(&t).0)
}

/// 本进程是否以服务形态运行(只有 `--service` 入口会置起)。
static SERVICE_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 声明「本进程是常驻服务」,让数据目录走机器级默认值。
/// 必须在任何 [`data_dir`] 调用之前执行(服务的第一个动作)。
pub fn enter_service_mode() {
    SERVICE_MODE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// 本进程是否以服务形态运行(只有 `--service` 入口会置起,日志与诊断用)。
pub fn service_mode() -> bool {
    SERVICE_MODE.load(std::sync::atomic::Ordering::Relaxed)
}

/// 目录选取口径(纯函数,便于把优先级钉成测试):
/// 用户标记 > 机器标记 > 按形态的默认目录。
fn resolve_data_dir(user: Option<PathBuf>, machine: Option<PathBuf>, as_service: bool) -> PathBuf {
    user.or(machine).unwrap_or_else(|| {
        if as_service {
            machine_default_dir()
        } else {
            default_data_dir()
        }
    })
}

fn marker_data_dir() -> Option<PathBuf> {
    read_marker_text(&default_data_dir()).and_then(|t| parse_location(&t).0)
}

/// 统一配置文件的固定位置(用户级)。升级/模型配置也放这一份(update.rs 用)。
pub fn default_settings_path() -> PathBuf {
    default_data_dir().join(SETTINGS_FILE)
}

/// 读目录下的配置文本:统一名优先,旧 marker 名兜底(迁移前的存量安装)。
fn read_marker_text(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(SETTINGS_FILE))
        .ok()
        .or_else(|| std::fs::read_to_string(dir.join(LEGACY_LOCATION_FILE)).ok())
}

/// 数据目录的**纯解析**(不建目录)。优先级:MALIANG_DATA_DIR 环境变量 >
/// 用户默认目录下 maliang.ini 的 [location](选项页写入)>
/// 机器级 maliang.ini(装服务时写入)> 默认目录
/// (服务形态 = ProgramData,普通进程 = 用户 AppData)。
///
/// 缓存落点与扫描排除每帧/每文件都要问一次路径,必须走这里,
/// 不能让「查一下目录在哪」顺手把目录建出来。
pub fn data_dir_loc() -> PathBuf {
    if let Some(v) = std::env::var_os("MALIANG_DATA_DIR") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    resolve_data_dir(marker_data_dir(), machine_marker_dir(), service_mode())
}

/// 数据目录(索引库 + 模型),附带建目录。解析口径见 [`data_dir_loc`]。
pub fn data_dir() -> PathBuf {
    let p = data_dir_loc();
    let _ = std::fs::create_dir_all(&p);
    p
}

/// 装服务时把**当前生效的数据目录**写进机器级标记,服务起来才找得到
/// (只有提权进程写得动 ProgramData)。
///
/// 没迁过目录也照写:不写的话服务会落到机器级默认值,而 UI 落在用户
/// AppData —— 那正是本次要根治的「各建一套库」。多账号机器上以最后一次
/// 装服务的人为准(装服务本身就是机器级动作)。
pub fn sync_machine_marker() -> std::io::Result<PathBuf> {
    let marker = machine_marker_path();
    std::fs::create_dir_all(marker.parent().unwrap())?;
    // 机器级文件只由本函数写(不含 [update] 节),整份重写安全。
    // 旧 data.location 留着不删:服务可能还是按旧文件名读的旧版。
    std::fs::write(
        &marker,
        format!("[location]\ndata_dir={}\n", data_dir().display()),
    )?;
    Ok(marker)
}

/// 选项页:设定数据目录(写统一配置的 [location] 节;`None` = 恢复默认)。
/// 记下 `pending_from = 当前数据目录`,下次启动把旧文件复制过去;
/// 本会话继续用原目录,保证写库不中断。
/// 同一文件里的 [update] 节与注释原样保留,只增删 location 两行。
pub fn set_data_dir(dir: Option<&Path>) -> std::io::Result<()> {
    let marker = default_data_dir().join(SETTINGS_FILE);
    let _ = std::fs::create_dir_all(default_data_dir());
    let (data_line, pending_line) = match dir {
        Some(d) => {
            let cur = data_dir();
            let pending = (cur != d).then(|| format!("pending_from={}", cur.display()));
            (Some(format!("data_dir={}", d.display())), pending)
        }
        None => (None, None),
    };
    // 旧 marker 还在(未迁移)时以它为底本,顺手完成到新文件名的搬迁。
    // 旧文件留着不删:可能还有按旧名读配置的旧版程序在跑。
    let existing = read_marker_text(&default_data_dir()).unwrap_or_default();
    std::fs::write(
        &marker,
        rewrite_marker_text(&existing, data_line, pending_line),
    )?;
    Ok(())
}

/// 把 data_dir=/pending_from= 两行写进配置文本,其余行(注释、[update] 节)
/// 原样保留。`None` = 抹掉该行。行插在 [location] 节头之后;旧 marker 没有
/// 节结构,插在最前 —— 读取端逐行找前缀,位置无关。
fn rewrite_marker_text(
    existing: &str,
    data_line: Option<String>,
    pending_line: Option<String>,
) -> String {
    let kept: Vec<String> = existing
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("data_dir=") || t.starts_with("pending_from="))
        })
        .map(String::from)
        .collect();
    let mut kept = kept;
    let insert_at = match kept.iter().position(|l| l.trim() == "[location]") {
        Some(i) => i + 1,
        None => {
            if kept
                .iter()
                .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'))
            {
                kept.insert(0, "[location]".into());
                1
            } else {
                0
            }
        }
    };
    if let Some(p) = pending_line {
        kept.insert(insert_at, p);
    }
    if let Some(d) = data_line {
        kept.insert(insert_at, d);
    }
    let mut out = kept.join("\n");
    out.push('\n');
    out
}

/// 启动早期(Store 打开前)执行 pending 迁移:把 `pending_from` 目录里的
/// 文件按「目标缺失才复制」搬到当前数据目录(逐文件 tmp+rename,失败不动
/// 已复制部分,下次启动重试),成功后清掉 pending 行,并**尽力删除旧目录**
/// —— 旧目录残留会让人分不清哪份是生效数据;
/// 删不掉的条目(被占用)留着即可,不影响新目录生效(能删尽删)。
/// 环境变量覆盖(测试/便携)时直接跳过,不碰任何目录。
pub fn migrate_pending_data() {
    let env_set = std::env::var_os("MALIANG_DATA_DIR")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    if env_set {
        return;
    }
    let marker_path = default_data_dir().join(SETTINGS_FILE);
    let Some(text) = read_marker_text(&default_data_dir()) else {
        return;
    };
    let (target, pending) = parse_location(&text);
    let (Some(target), Some(from)) = (target, pending) else {
        return;
    };
    let from = PathBuf::from(from);
    if from != target {
        let _ = std::fs::create_dir_all(&target);
        if copy_missing_recursive(&from, &target) {
            remove_tree_best_effort(&from);
            let kept =
                rewrite_marker_text(&text, Some(format!("data_dir={}", target.display())), None);
            let _ = std::fs::write(&marker_path, kept);
        }
    } else {
        // 指回自身:清掉 pending 即可。
        let kept = rewrite_marker_text(&text, Some(format!("data_dir={}", target.display())), None);
        let _ = std::fs::write(&marker_path, kept);
    }
}

/// 递归复制 `src` → `dst`,只复制目标缺失的文件(已有文件一律不覆盖,
/// 避免「旧默认目录的陈旧副本回写覆盖新目录里更新的数据」)。
/// 逐文件先写 `.tmp` 再改名;返回是否全部成功。
/// 跳过:配置文件本身、`*.tmp`、mft_report.txt(瞬态文件)。
fn copy_missing_recursive(src: &Path, dst: &Path) -> bool {
    let mut ok = true;
    let Ok(entries) = std::fs::read_dir(src) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if name_s == SETTINGS_FILE
            || name_s == LEGACY_LOCATION_FILE
            || name_s == "mft_report.txt"
            || name_s.ends_with(".tmp")
        {
            continue;
        }
        let s = entry.path();
        let d = dst.join(&name);
        match entry.file_type() {
            Ok(t) if t.is_dir() => {
                if std::fs::create_dir_all(&d).is_err() || !copy_missing_recursive(&s, &d) {
                    ok = false;
                }
            }
            Ok(_) => {
                if !d.exists() {
                    let tmp = d.with_extension("migrating.tmp");
                    match std::fs::copy(&s, &tmp).and_then(|_| std::fs::rename(&tmp, &d)) {
                        Ok(()) => {}
                        Err(_) => {
                            let _ = std::fs::remove_file(&tmp);
                            ok = false;
                        }
                    }
                }
            }
            Err(_) => ok = false,
        }
    }
    ok
}

/// 尽力删除一棵目录树:单个条目删不掉(被占用)就跳过继续,绝不报错。
/// 用于迁移完成后清旧数据目录 — 残留的少数文件不构成正确性问题。
fn remove_tree_best_effort(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        let _ = std::fs::remove_file(dir);
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => remove_tree_best_effort(&p),
            Ok(_) => {
                let _ = std::fs::remove_file(&p);
            }
            Err(_) => {}
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// 首启一次性迁移(11-07 配置统一):把旧 `data.location` 与旧 `update.ini`
/// 并入统一配置 maliang.ini。
/// **旧文件一律保留不删**:机器上可能还装着按旧文件名读配置的旧版程序
/// (尤其索引服务),删了它就会退回默认目录另建一套库,造成两边数据分裂;
/// 读端对旧名做了兼容,新旧并存互不干扰,等全系统升到新版再清理。
/// data_dir= 并入模板的 [location] 节;update.ini 原文追加到文件尾
/// (模板最后一节就是 [update],裸键值自动归属,解析端逐行找键不看节)。
pub fn migrate_legacy_configs() {
    // 便携部署(exe 同目录放统一配置)以它为准,不生成第二份。
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            if d.join(SETTINGS_FILE).exists() {
                return;
            }
        }
    }
    let dir = default_data_dir();
    // 统一配置已在(迁移过或用户已在用)就绝不再写:旧文件按「只增不删」
    // 政策一直都在,没有这道闸,每次启动都会用模板把用户改过的配置冲掉。
    if !default_settings_path().exists() {
        let loc = std::fs::read_to_string(dir.join(LEGACY_LOCATION_FILE)).ok();
        let upd = legacy_update_ini_text();
        if loc.is_some() || upd.is_some() {
            // 默认目录可能尚不存在(全新机器上不会走到这;存量老安装由
            // marker 迁移首次落笔),fs::write 不自建目录,必须先建。
            let _ = std::fs::create_dir_all(&dir);
            let mut lines: Vec<String> = SETTINGS_TEMPLATE.lines().map(String::from).collect();
            if let Some(t) = &loc {
                let tpl = lines
                    .iter()
                    .position(|l| l.trim() == "data_dir=")
                    .unwrap_or(0);
                let loc_lines: Vec<String> = t.lines().map(String::from).collect();
                let has_data = loc_lines
                    .iter()
                    .any(|l| l.trim_start().starts_with("data_dir="));
                let at = if has_data { tpl } else { tpl + 1 };
                if has_data {
                    lines.remove(tpl);
                }
                for (k, ll) in loc_lines.iter().enumerate() {
                    lines.insert(at + k, ll.clone());
                }
            }
            let mut text = lines.join("\n");
            text.push('\n');
            if let Some(t) = &upd {
                // 只搬键值行:模板已含全部用法说明,旧文件里的注释是历史
                // 叙事,不该带进给用户的配置。
                let kv: Vec<&str> = t
                    .lines()
                    .map(str::trim)
                    .filter(|l| {
                        !l.is_empty()
                            && !l.starts_with('#')
                            && !l.starts_with(';')
                            && !l.starts_with('[')
                    })
                    .collect();
                if !kv.is_empty() {
                    text.push_str("\n# —— 以下键值由旧 update.ini 自动并入 ——\n");
                    for l in kv {
                        text.push_str(l);
                        text.push('\n');
                    }
                }
            }
            let _ = std::fs::write(default_settings_path(), text);
        }
    }
    // 机器级 marker 换名(服务读);同样只增不删。普通进程写不动 ProgramData
    // 就留给装服务动作补。
    let mdir = machine_default_dir();
    if !mdir.join(SETTINGS_FILE).exists() {
        if let Ok(t) = std::fs::read_to_string(mdir.join(LEGACY_LOCATION_FILE)) {
            let _ = std::fs::create_dir_all(&mdir);
            let _ = std::fs::write(mdir.join(SETTINGS_FILE), format!("[location]\n{t}"));
        }
    }
}

/// 旧 update.ini 的内容:exe 同目录优先,其次数据目录(与旧定位规则一致)。
fn legacy_update_ini_text() -> Option<String> {
    legacy_update_ini_path().and_then(|p| std::fs::read_to_string(p).ok())
}

fn legacy_update_ini_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            let p = d.join(LEGACY_UPDATE_FILE);
            if p.exists() {
                return Some(p);
            }
        }
    }
    let p = data_dir().join(LEGACY_UPDATE_FILE);
    p.exists().then_some(p)
}

/// 数据目录被迁走时,缓存挂在它下面的子目录名(与 `models` / `Incoming` 同级)。
const CACHE_SUBDIR: &str = "cache";

/// 系统缓存目录下的缓存位置(未设置数据目录标记时的落点,也是历史落点)。
/// 只算路径,不建目录 —— 它常常是「待迁移来源」,不该被顺手创建。
pub fn system_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")))
        .join("Relyto")
        .join("Maliang")
}

/// 系统缓存目录里是否还留着旧缓存(给「搬过来」按钮判定用)。
/// 只看有没有条目,不做遍历 —— 面板每帧都会问到它。
pub fn system_cache_has_content() -> bool {
    std::fs::read_dir(system_cache_dir())
        .map(|mut r| r.next().is_some())
        .unwrap_or(false)
}

/// 缓存目录解析(纯函数,不含环境变量与建目录):`Some` = 数据目录已迁走,
/// 缓存跟到它的 cache 子目录;`None` = 仍在默认位置,沿用系统缓存目录。
/// 缩略图虽可再生,但体量不小(全库四档可达数 GB),不该在用户把库搬到
/// 大盘之后还继续默默写满系统盘。
fn cache_root_for(relocated: Option<&Path>, system_cache: &Path) -> PathBuf {
    match relocated {
        Some(d) => d.join(CACHE_SUBDIR),
        None => system_cache.to_path_buf(),
    }
}

/// 「数据目录是否已被迁走」的判定:与用户级默认目录一致就不算迁走
/// (既有缓存继续留在系统缓存目录,不无谓失效);不一致才跟随。
/// 单独抽出来是为了让机器级标记 / 环境变量 / 服务形态默认值这三条
/// 来源都走同一判定 —— 只认用户级标记的话,「装服务时写下的机器级
/// data_dir」这种部署读不到,缓存就会继续默默写满系统盘。
fn relocated_data_dir(data: &Path, default: &Path) -> Option<PathBuf> {
    (data != default).then(|| data.to_path_buf())
}

/// 按**生效数据目录**定位缓存落点(纯函数),与 [`data_dir`] 同口径。
fn cache_root_for_data_dir(data: &Path, default: &Path, system_cache: &Path) -> PathBuf {
    cache_root_for(relocated_data_dir(data, default).as_deref(), system_cache)
}

/// 缓存目录路径(缩略图四档),优先级与 [`cache_dir`] 完全一致,但**不建目录** ——
/// 排除规则每索引一个文件都要问一次,顺手 `create_dir_all` 会把「查路径」
/// 变成「改磁盘」。
pub fn cache_dir_loc() -> PathBuf {
    if let Some(v) = std::env::var_os("MALIANG_CACHE_DIR") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    cache_root_for_data_dir(&data_dir_loc(), &default_data_dir(), &system_cache_dir())
}

/// 缓存目录(缩略图四档)。见 [`cache_dir_loc`] 的优先级,附带建目录。
pub fn cache_dir() -> PathBuf {
    let p = cache_dir_loc();
    let _ = std::fs::create_dir_all(&p);
    p
}

/// 本应用**自己写数据**的目录候选(纯路径,不建目录),给扫描排除规则用。
///
/// 要列全而不是只列 [`cache_dir_loc`],是因为「谁在写缓存」和「谁在扫盘」
/// 是两个进程:写缩略图的是 UI 进程(读用户级配置),做全盘发现的是常驻
/// 服务(LocalSystem 读不到用户级配置,只能读到机器级)。两种形态各自
/// 解析出的落点、机器级默认落点、以及历史的系统缓存落点都要排除,
/// 少一条服务就会把自家缩略图当照片收进库。
pub fn own_cache_dirs() -> Vec<PathBuf> {
    let sys = system_cache_dir();
    let dflt = default_data_dir();
    // 另一进程形态(UI ↔ 服务)看到的生效目录可能不同:用户级标记只有
    // 登录用户读得到。两个落点都列进排除表,免得自家缩略图被当照片收进库。
    let other = resolve_data_dir(marker_data_dir(), machine_marker_dir(), !service_mode());
    let cands = [
        cache_dir_loc(),
        cache_root_for_data_dir(&other, &dflt, &sys),
        cache_root_for_data_dir(&machine_default_dir(), &dflt, &sys),
        sys,
    ];
    let mut out: Vec<PathBuf> = Vec::with_capacity(cands.len());
    for p in cands {
        if !out.iter().any(|q| q == &p) {
            out.push(p);
        }
    }
    out
}

/// 一棵目录树的占用(文件数 + 字节数)。目录不存在返回 (0, 0)。
/// 软链/重入目录不做特殊处理(缓存目录里只有普通文件)。
pub fn tree_usage(dir: &Path) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(t) if t.is_dir() => {
                let (f, b) = tree_usage(&entry.path());
                files += f;
                bytes += b;
            }
            Ok(_) => {
                if let Ok(m) = entry.metadata() {
                    files += 1;
                    bytes += m.len();
                }
            }
            Err(_) => {}
        }
    }
    (files, bytes)
}

/// 缓存迁移:把 `from`(通常是 [`system_cache_dir`])整棵搬到当前 [`cache_dir`],
/// 已存在的文件不覆盖;全部搬成功后尽力删除来源(删不掉的留着,不影响新目录生效)。
/// 与数据目录的 pending 迁移同构:逐文件 tmp+rename,失败即保留来源,下次再点重试。
/// 返回 (搬动的文件数, 来源是否已清空)。
pub fn migrate_cache_from(from: &Path) -> Result<(usize, bool)> {
    migrate_cache_to(from, &cache_dir())
}

/// 搬运的落点版本(供测试注入目标目录)。
fn migrate_cache_to(from: &Path, to: &Path) -> Result<(usize, bool)> {
    if same_dir(from, to) {
        return Ok((0, false));
    }
    std::fs::create_dir_all(to)?;
    let mut moved = 0usize;
    let mut failed = false;
    copy_cache_missing(from, to, &mut moved, &mut failed);
    if failed {
        return Ok((moved, false));
    }
    remove_tree_best_effort(from);
    Ok((moved, !from.exists()))
}

/// 路径等价判定:两边各 canonicalize 一次(目标不存在时退回字符串比较)。
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// 递归搬运(只搬目标缺失的文件),累计搬动数/失败数。
/// 跳过配置文件本身、`*.tmp` 与瞬态报告文件。
fn copy_cache_missing(src: &Path, dst: &Path, moved: &mut usize, failed: &mut bool) {
    let Ok(entries) = std::fs::read_dir(src) else {
        *failed = true;
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if name_s == SETTINGS_FILE
            || name_s == LEGACY_LOCATION_FILE
            || name_s == "mft_report.txt"
            || name_s.ends_with(".tmp")
        {
            continue;
        }
        let s = entry.path();
        let d = dst.join(&name);
        match entry.file_type() {
            Ok(t) if t.is_dir() => {
                if std::fs::create_dir_all(&d).is_err() {
                    *failed = true;
                } else {
                    copy_cache_missing(&s, &d, moved, failed);
                }
            }
            Ok(_) => {
                if d.exists() {
                    continue;
                }
                let tmp = d.with_extension("moving.tmp");
                if std::fs::copy(&s, &tmp)
                    .and_then(|_| std::fs::rename(&tmp, &d))
                    .is_ok()
                {
                    *moved += 1;
                } else {
                    let _ = std::fs::remove_file(&tmp);
                    *failed = true;
                }
            }
            Err(_) => *failed = true,
        }
    }
}

/// 缩略图四档的目录名(对应 `thumbs::Tier::dir_name`)。db 不依赖 thumbs,
/// 这里写死并由测试反向核对。自动搬迁只认这四个目录 —— 旧位置里的
/// `*.reset-*` 历史备份体量可能和缓存本身相当,是否清掉该由用户决定,
/// 不该在后台静默移动/删除。
const CACHE_TIER_DIRS: [&str; 4] = ["thumbs", "smallthumbs", "bigthumbs", "previews"];

/// 启动后台缓存搬迁状态:0 = 未开始/无需搬,1 = 进行中,2 = 完成,3 = 部分失败。
static CACHE_MIGRATION_STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
static CACHE_MIGRATION_MOVED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 给选项页读的搬迁状态 (状态码, 已搬文件数)。
pub fn cache_migration_status() -> (u8, usize) {
    use std::sync::atomic::Ordering;
    (
        CACHE_MIGRATION_STATE.load(Ordering::Relaxed),
        CACHE_MIGRATION_MOVED.load(Ordering::Relaxed),
    )
}

/// 把旧系统缓存里四个档位的缩略图搬到 `to`。只补缺、不覆盖;
/// 全部成功后按档位清掉来源子目录,顶层目录与其它内容(reset 备份、
/// 配置文件)保持原样。同步执行,调用方负责放进后台线程。
fn relocate_tier_caches(from: &Path, to: &Path) -> (usize, bool) {
    let mut moved = 0usize;
    let mut failed = false;
    for name in CACHE_TIER_DIRS {
        let s = from.join(name);
        if !s.is_dir() {
            continue;
        }
        let d = to.join(name);
        if std::fs::create_dir_all(&d).is_err() {
            failed = true;
            continue;
        }
        copy_cache_missing(&s, &d, &mut moved, &mut failed);
    }
    if failed {
        return (moved, false);
    }
    for name in CACHE_TIER_DIRS {
        let s = from.join(name);
        if s.is_dir() {
            remove_tree_best_effort(&s);
        }
    }
    (moved, true)
}

/// 数据目录迁走后,启动时自动把旧系统缓存搬到新落点。
/// 与手动「把旧缓存搬过来」整棵搬不同,这里只搬四个档位目录,
/// 状态写进 [`cache_migration_status`] 供选项页显示。
pub fn auto_relocate_system_cache() -> (usize, bool) {
    use std::sync::atomic::Ordering;
    let from = system_cache_dir();
    let to = cache_dir_loc();
    if same_dir(&from, &to) || !from.is_dir() {
        return (0, true);
    }
    if !CACHE_TIER_DIRS.iter().any(|n| from.join(n).is_dir()) {
        return (0, true);
    }
    CACHE_MIGRATION_STATE.store(1, Ordering::Relaxed);
    let (moved, ok) = relocate_tier_caches(&from, &to);
    CACHE_MIGRATION_MOVED.store(moved, Ordering::Relaxed);
    CACHE_MIGRATION_STATE.store(if ok { 2 } else { 3 }, Ordering::Relaxed);
    (moved, ok)
}

/// 由标签全路径重算父层级(挂到最长的已存在祖先)。
pub(crate) const REBUILD_TAG_PARENTS_SQL: &str =
    "UPDATE tags SET parent_id = (
       SELECT p.id FROM tags p
        WHERE substr(tags.name, 1, length(p.name) + 1) = p.name || '/'
        ORDER BY length(p.name) DESC LIMIT 1);";

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
    -- 子类型对应不同解码路径,只有 Shell 一条,无须细分)。
    kind INTEGER NOT NULL DEFAULT 0,
    -- 派生状态:0=未纳入作用域,1=待派生,2=已派生,3=坏文件。
    -- 结构列(path/size/mtime/kind)全盘都有,派生列(宽高/时间/缩略图)
    -- 只有 1/2 的目录才有;0 与 3 在网格里都是"不派生",含义不同。
    rich_state INTEGER NOT NULL DEFAULT 0,
    -- 小写扩展名(不含点),从 L1 索引直接落,免逐条字符串比较。
    ext TEXT,
    -- L1 身份:fnv1a64("<盘符>:<FRN index>")。改名/移动时不变,
    -- 增量落库按它 upsert(path 只是属性,不是身份)。非 NTFS/历史行为 NULL。
    frn64 INTEGER,
    -- 人脸扫描「已分析」标记(0=待分析,1=已分析;没检出人脸也要置 1,
    -- 否则待办队列永远卡在同批照片上)。清除人脸数据重扫时归零。
    face_done INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_photos_taken ON photos(taken_utc);
CREATE INDEX IF NOT EXISTS idx_photos_parent ON photos(parent);
-- 注意:建在迁移列(如 cam_id)上的索引不能写在这里 —— 旧库表已存在,
-- CREATE TABLE IF NOT EXISTS 跳过、索引先于 ALTER 执行会直接失败。
-- 这类索引一律放 `from_conn` 的迁移段(ALTER 之后)。
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
    color TEXT,
    parent_id INTEGER
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
    vec BLOB NOT NULL,
    sig TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_face_templates_sig ON face_templates(sig);
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
-- 变更日志只存在内存队列里,写盘 CSV 也只是瞬时快照 —— 不可回溯。
-- 这里做成**持久化 + 带时间戳 + 可查询**的版本。
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
-- 应用中途退出时残留的条目在下次启动自动续扫(扫描器是进程内
-- 持久化队列,进程重启后仍可继续;
-- 「扫描不阻塞 UI」由既有后台线程方案保证)。
CREATE TABLE IF NOT EXISTS scan_queue (
    id INTEGER PRIMARY KEY,
    root TEXT UNIQUE NOT NULL,
    scope TEXT NOT NULL DEFAULT '',
    enqueued_ts INTEGER NOT NULL     -- Unix 毫秒
);
-- 各档缩略图的完成状态(P1-9):三张缓存表
-- (三张缩略图缓存表)。
-- tier 取值 0=Small(96) 1=Normal(256) 2=Big(512) 3=Preview(1024)。
-- photos.thumb_ok 保留给 Normal 档(向后兼容既有代码与测试)。
CREATE TABLE IF NOT EXISTS thumb_tiers (
    photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    tier INTEGER NOT NULL,
    PRIMARY KEY (photo_id, tier)
);
-- 目录意图账本:派生(缩略图/EXIF/人脸)以**目录**为记账单位 —— 全盘发现的
-- 文件都在库里,但只有落进账本的目录才打开文件做派生。
-- state:default_root(首启默认范围)/ imported(用户主动导入)/ clicked(点开过)/
--        never(明确不纳入)
-- source:initial_scan_dialog / default_shell / import / click(记账来源,可解释)
-- needs_refresh:目录级失效位(索引版本不匹配或用户强制刷新时置 1,
--        该目录的行整体退回待派生;按目录重做缩略图的粒度)
CREATE TABLE IF NOT EXISTS dir_intent (
    path           TEXT    PRIMARY KEY,  -- 目录路径(不带结尾分隔符)
    volume         TEXT,                 -- 卷根,如 'D:\\'(全盘索引按卷记账用)
    frn            INTEGER,              -- 该目录的文件记录号(索引侧身份,可空)
    state          TEXT    NOT NULL DEFAULT 'default_root',
    source         TEXT    NOT NULL DEFAULT 'default_shell',
    since_usn      INTEGER,              -- 纳入时的日志游标(追赶用)
    needs_refresh  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_dir_intent_state ON dir_intent(state, needs_refresh);
-- 坏媒体台账:解码反复失败的条目在此立案,不再每轮重试同一张坏图。
-- 网格上给「隐藏此文件 / 不隐藏」两个出口(退出后删行放回)。
CREATE TABLE IF NOT EXISTS bad_media (
    path   TEXT PRIMARY KEY,
    reason TEXT NOT NULL,
    since  INTEGER NOT NULL             -- Unix 毫秒
);
-- AI 待办:一个 (阶段, 照片) 一行,重复入队无副作用。
CREATE TABLE IF NOT EXISTS ai_jobs (
    stage       TEXT    NOT NULL,
    photo_id    INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    priority    INTEGER NOT NULL,
    enqueued_ts INTEGER NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (stage, photo_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_ai_jobs_pick
    ON ai_jobs(stage, priority, enqueued_ts, photo_id);
-- AI 完成台账:model_ver 不一致即视为未完成。
CREATE TABLE IF NOT EXISTS ai_state (
    photo_id  INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    stage     TEXT    NOT NULL,
    model_ver TEXT    NOT NULL,
    done_ts   INTEGER NOT NULL,
    PRIMARY KEY (photo_id, stage)
) WITHOUT ROWID;
-- AI 失败隔离:达到阈值后移出队列,不再反复重试坏图。
CREATE TABLE IF NOT EXISTS ai_failed (
    photo_id INTEGER NOT NULL,
    stage    TEXT    NOT NULL,
    err      TEXT    NOT NULL,
    attempts INTEGER NOT NULL,
    ts       INTEGER NOT NULL,
    PRIMARY KEY (photo_id, stage)
);
-- AI 优先级:目录前缀最长匹配。
CREATE TABLE IF NOT EXISTS ai_priority_rules (
    prefix TEXT PRIMARY KEY,
    prio   INTEGER NOT NULL
);
-- AI 模型槽位清单。
CREATE TABLE IF NOT EXISTS ai_models (
    slot         TEXT PRIMARY KEY,
    file         TEXT NOT NULL,
    version      TEXT NOT NULL,
    source       TEXT NOT NULL,
    url          TEXT,
    sha256       TEXT,
    bytes        INTEGER NOT NULL,
    license      TEXT NOT NULL,
    dim          INTEGER,
    input_name   TEXT,
    installed_ts INTEGER NOT NULL
);
-- 语义向量:photo_id 主键,浮点数组存 BLOB(精确余弦检索,免外部扩展)。
CREATE TABLE IF NOT EXISTS clips_vec (
    photo_id  INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
    ver       TEXT    NOT NULL,
    dim       INTEGER NOT NULL,
    embedding BLOB    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_clips_vec_ver ON clips_vec(ver);
-- 自动标签词表:一份词表同时承载标签与分类(层级路径 node)。
CREATE TABLE IF NOT EXISTS tag_labels (
    id        INTEGER PRIMARY KEY,
    node      TEXT NOT NULL,
    name_zh   TEXT NOT NULL,
    name_en   TEXT NOT NULL,
    prompt_en TEXT NOT NULL,
    parent    INTEGER,
    builtin   INTEGER NOT NULL,
    ver       TEXT NOT NULL
);
-- 自动标签结果:只按当前 model_ver 读取。
CREATE TABLE IF NOT EXISTS auto_tags (
    photo_id  INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    label_id  INTEGER NOT NULL,
    score     REAL    NOT NULL,
    model_ver TEXT    NOT NULL,
    PRIMARY KEY (photo_id, label_id, model_ver)
);
CREATE INDEX IF NOT EXISTS idx_auto_tags_ver ON auto_tags(model_ver, label_id);
-- OCR 原文与检索用分词文本。
CREATE TABLE IF NOT EXISTS ocr_docs (
    photo_id   INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
    text       TEXT    NOT NULL,
    seg        TEXT    NOT NULL,
    line_count INTEGER NOT NULL,
    char_count INTEGER NOT NULL,
    model_ver  TEXT    NOT NULL,
    done_ts    INTEGER NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS ocr_fts USING fts5(
    seg,
    content='ocr_docs',
    content_rowid='photo_id',
    tokenize="unicode61 remove_diacritics 0"
);
-- OCR 行框:只对文字类照片写入,最多前 200 行。
CREATE TABLE IF NOT EXISTS ocr_boxes (
    photo_id INTEGER NOT NULL,
    line_no  INTEGER NOT NULL,
    x0 INTEGER, y0 INTEGER, x1 INTEGER, y1 INTEGER,
    conf REAL,
    PRIMARY KEY (photo_id, line_no)
);
-- 查询词典:中文词 → 英文 prompt 片段(人工优先级高于机器翻译)。
CREATE TABLE IF NOT EXISTS ai_dict (
    term_zh TEXT PRIMARY KEY,
    term_en TEXT NOT NULL
);
-- 人脸约束(M5):must = 同一人,never = 不同人;审核后供聚类使用。
CREATE TABLE IF NOT EXISTS face_constraints (
    a_face INTEGER NOT NULL,
    b_face INTEGER NOT NULL,
    kind   TEXT    NOT NULL,   -- 'must' | 'never'
    ts     INTEGER NOT NULL,
    PRIMARY KEY (a_face, b_face)
);
-- 人脸逐照片状态:0=待处理 1=处理中 2=成功(含无脸) 3=失败。
CREATE TABLE IF NOT EXISTS face_scan (
    photo_id INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
    sig TEXT NOT NULL,
    state INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    face_count INTEGER NOT NULL DEFAULT 0,
    mtime INTEGER NOT NULL DEFAULT 0,
    size INTEGER NOT NULL DEFAULT 0,
    ts INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_face_scan_state ON face_scan(sig, state);
-- 删脸/删照片时同步清理模板、约束与扫描状态,避免留下悬空引用。
CREATE TRIGGER IF NOT EXISTS trg_face_cleanup AFTER DELETE ON faces BEGIN
    DELETE FROM face_templates WHERE face_id = OLD.id;
    DELETE FROM face_constraints WHERE a_face = OLD.id OR b_face = OLD.id;
END;
CREATE TRIGGER IF NOT EXISTS trg_photo_scan_cleanup AFTER DELETE ON photos BEGIN
    DELETE FROM face_scan WHERE photo_id = OLD.id;
END;
"#;

/// 一条 L1 → L2 的结构行(全盘元数据入库,**不打开文件**)。
///
/// 只带 photos 的结构列;派生列(宽高/拍摄时间/GPS/相机)留空,
/// 由 L3 在作用域门控通过后另行填充。身份是 `frn64`,path 只是属性。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct L1Row {
    /// `frn64_of(letter, frn)` 的结果。
    pub frn64: i64,
    pub path: String,
    pub parent: String,
    /// 修改时间,UNIX **秒**(与 photos.mtime 现有口径一致)。
    pub mtime: i64,
    pub size: i64,
    /// 0 = 图片,1 = 视频(`video::kind`)。
    pub kind: i32,
    /// 小写扩展名(不含点);无扩展名 None。
    pub ext: Option<String>,
}

/// `ingest_l1_rows` 的计数(报告与测试用)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct L1Ingest {
    pub inserted: usize,
    pub updated: usize,
}

// ===== 派生作用域账本 =====

/// `photos.rich_state` 只是**后台派生队列的排队状态**,不是「能不能显示」的开关。
/// 显示一律照常读磁盘缓存;真排除的目录根本不纳入图库,库里有的行就该显示。
/// 0 = 还没排进队列(由可见时按张放行 / 闲时低速补齐处理)。
pub const RICH_OUT: i32 = 0;
/// 1 = 已排队,等缩略图/EXIF。
pub const RICH_PENDING: i32 = 1;
/// 2 = 已派生(缓存与 dhash 都在)。
pub const RICH_DONE: i32 = 2;
/// 3 = 坏文件,已在 `bad_media` 立案,不再每轮重试。
pub const RICH_BAD: i32 = 3;

/// 派生失败多少次才立案进 `bad_media`。
///
/// 沿用改造前 `thumb_fail<3` 的口径:一次失败可能只是解码器/Shell 临时抽风,
/// 三次都失败才算这个文件真解不开。
pub const BAD_MEDIA_THRESHOLD: i64 = 3;

/// 硬排除清理的版本号,写进 `hard_excluded_purged` 标记里。
///
/// 改一次硬名单(见 `scan::HARD_EXCLUDES_OURS`)就把它往前推一格:老库上次
/// 清理时用的是旧名单,不推版本的话新加进硬层的那些目录会永远留在库里当灰格子。
// 名单里新增了构建/测试夹具目录,改版本号让历史库再对账一次。
const HARD_PURGE_VERSION: &str = "hard-excluded-v2";

/// `dir_intent.state`:首启默认范围(范围对话框 / 壳目录)。
pub const INTENT_DEFAULT_ROOT: &str = "default_root";
/// `dir_intent.state`:用户主动导入。
pub const INTENT_IMPORTED: &str = "imported";
/// `dir_intent.state`:在树/网格里点开过(点击懒加载)。
pub const INTENT_CLICKED: &str = "clicked";

/// `dir_intent.source`:首次启动的范围对话框。
pub const SRC_INITIAL_DIALOG: &str = "initial_scan_dialog";
/// `dir_intent.source`:用户导入。
pub const SRC_IMPORT: &str = "import";
/// `dir_intent.source`:点击展开。
pub const SRC_CLICK: &str = "click";
/// 历史账本里出现过的来源值:滚动时上屏过。现在滚动只按张放行、不再写
/// 目录账本,但旧库里还留着这个字符串,读账本时要认得它。
#[allow(dead_code)]
pub const SRC_VIEW: &str = "visible";

/// `state` 是否属于纳入组(的第三个条件)。
pub fn intent_included(state: &str) -> bool {
    matches!(
        state,
        INTENT_DEFAULT_ROOT | INTENT_IMPORTED | INTENT_CLICKED
    )
}

/// 目录意图账本的一行。
///
/// 记账单位是**目录**:全盘发现的文件都在 L2 里,但只有落进账本、且状态
/// 属于纳入组的目录,其下的行才会真的打开文件做派生。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirIntent {
    /// 归一化目录路径(不带结尾分隔符;卷根保留 `D:\` 形式,见 [`norm_dir`])。
    pub path: String,
    /// 卷根,如 `D:\`。
    pub volume: Option<String>,
    /// 该目录在索引侧的文件记录号(可空:非 NTFS 或还没对上)。
    pub frn: Option<i64>,
    pub state: String,
    pub source: String,
    /// 纳入时的日志游标(追赶用)。
    pub since_usn: Option<i64>,
    /// 目录级失效位(索引 blob 版本不匹配或 `force_thumb_update` 偏好为真)。
    pub needs_refresh: bool,
}

impl DirIntent {
    /// 造一条待落账的意图(`path` 会归一化,`since_usn`/`needs_refresh` 取默认)。
    pub fn new(path: &str, state: &str, source: &str) -> Self {
        DirIntent {
            path: norm_dir(path),
            volume: volume_of(path),
            frn: None,
            state: state.to_string(),
            source: source.to_string(),
            since_usn: None,
            needs_refresh: false,
        }
    }

    pub fn included(&self) -> bool {
        intent_included(&self.state)
    }
}

/// 坏媒体台账的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadMedia {
    pub path: String,
    pub reason: String,
    /// 立案时刻,Unix 毫秒。
    pub since: i64,
}

/// 目录路径归一化:去掉结尾分隔符,但卷根保留(`D:\` → `D:\`,不是 `D:`)。
///
/// 裸 `D:` 在 Win32 里是「该驱动器的当前目录」,不是卷根;账本用它当键
/// 会让同一个卷根写出两种形式,前缀匹配随之失效。
pub fn norm_dir(p: &str) -> String {
    let t = p.trim_end_matches(['\\', '/']);
    if t.is_empty() {
        return p.to_string();
    }
    let b = t.as_bytes();
    if b.len() == 2 && b[1] == b':' {
        return format!("{t}\\");
    }
    t.to_string()
}

/// 从任意路径取卷根(`D:\Photos\a.jpg` → `Some("D:\\")`);无盘符前缀返回 None。
pub fn volume_of(p: &str) -> Option<String> {
    let b = p.as_bytes();
    if b.len() >= 2 && b[1] == b':' {
        Some(format!("{}\\", &p[..2]))
    } else {
        None
    }
}

/// 目录意图的作用域快照:全表读一次,之后逐条判定零 IO。
///
/// 判定口径 = **最长前缀命中**:一条路径的状态由覆盖它的最深那条账本决定,
/// 这样 `never`(用户显式退出)才能盖过祖先目录的纳入 —— 否则「别扫这个
/// 文件夹」永远关不掉。
///
/// 键一律大写:Windows 路径大小写不敏感,而 L1 的名字取自 MFT、壳目录取自
/// Shell API,两边的大小写不保证一致。
#[derive(Debug, Default, Clone)]
pub struct ScopeMap {
    /// 归一化大写路径 → (state, source)。source 也要带上:软排除层让不让路,
    /// 只看「这条账本是不是用户亲手导入的」(方案 ④)。
    by_upper: HashMap<String, (String, String)>,
}

impl ScopeMap {
    pub fn load(store: &Store) -> Self {
        let rows: Vec<(String, String, String)> = store.read(|conn| {
            let mut st = match conn.prepare("SELECT path,state,source FROM dir_intent") {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    // source 允许为空(历史行/手工写账),空串按「非亲手导入」处理。
                    r.get::<_, Option<String>>(2)
                        .ok()
                        .flatten()
                        .unwrap_or_default(),
                ))
            })
            .ok()
            .map(|it| it.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        });
        ScopeMap {
            by_upper: rows
                .into_iter()
                .map(|(p, s, src)| (norm_dir(&p).to_ascii_uppercase(), (s, src)))
                .collect(),
        }
    }

    // 只有 `ScopeGate::is_empty` 用它,而那个是测试断言用的(见下)。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.by_upper.is_empty()
    }

    /// 覆盖该目录的最深那条账本(state + source);一条都不覆盖时 None。
    fn entry_of(&self, dir: &str) -> Option<&(String, String)> {
        if self.by_upper.is_empty() {
            return None;
        }
        let up = norm_dir(dir).to_ascii_uppercase();
        let mut end = up.len();
        loop {
            if let Some(st) = self.by_upper.get(&up[..end]) {
                return Some(st);
            }
            // 上溯一级:切掉最后一个分隔符之后的部分。
            let cut = match up[..end].rfind(['\\', '/']) {
                // 只剩开头那个分隔符:POSIX 的 `/` 也是一级(根),要让它命中账本;
                // 已经在根上——或 Windows 侧的 UNC 残段——就没有更上的一级了。
                Some(0) => {
                    if end == 1 {
                        return None;
                    }
                    return self.by_upper.get("/");
                }
                Some(i) => i,
                None => return None,
            };
            let parent = &up[..cut];
            let pb = parent.as_bytes();
            end = if pb.len() == 2 && pb[1] == b':' {
                // 切到 `D:` 说明上一级就是卷根 `D:\`,补回分隔符再试一次;
                // 已经在卷根上就说明到顶了。
                if end == cut + 1 {
                    return None;
                }
                cut + 1
            } else {
                cut
            };
        }
    }

    /// 覆盖该目录的最深那条账本的状态;一条都不覆盖时 None(= 未纳入)。
    pub fn state_of(&self, dir: &str) -> Option<&str> {
        self.entry_of(dir).map(|(s, _)| s.as_str())
    }

    /// 该目录(或被它覆盖的祖先)是否在派生作用域内。
    pub fn includes(&self, dir: &str) -> bool {
        matches!(self.state_of(dir), Some(s) if intent_included(s))
    }

    /// 该目录是否由**用户亲手导入**而纳入(账本 source = `import`)。
    ///
    /// 只有这一条能压过软排除层:首启对话框落的全盘卷根(`initial_scan_dialog`)、
    /// 滚动/点击自动记的(`click` / `visible`)都不算 —— 那正是
    /// 「滚到过不等于要派生」要挡的场景。
    pub fn explicit_import(&self, dir: &str) -> bool {
        matches!(self.entry_of(dir), Some((s, src)) if intent_included(s) && src == SRC_IMPORT)
    }
}

/// 派生门:`ext 命中媒体池 && 未被三层跳过 && dir_intent ∈ 纳入组`
/// → `rich_state=1`,否则 `0`。
///
/// **不比 size、不比像素** —— 去重口径里没有第三条尺寸比较门;
/// 「小文件没必要做」是靠目录作用域天然达成的,不是靠阈值。
/// 判定纯用 L1 已有的列(`ext` / `parent` / `path`),零 IO。
///
/// 三层跳过 = 硬编码 4 条 ∪ `Exclusions`(用户设置)∪ `bad_media` 台账。
/// 前两层已由 [`crate::scan::Exclusions::default`] 并成一份名单
/// (`from_store` 从默认值起步,用户设置只能往上加、减不掉那 4 条),
/// 所以这里一次目录判定就覆盖了前两层。
#[derive(Clone)]
pub struct ScopeGate {
    excl: crate::scan::Exclusions,
    scope: ScopeMap,
    /// 台账路径(大写),第三层跳过。
    bad: std::collections::HashSet<String>,
}

impl ScopeGate {
    /// 读一次库,之后逐行判定都不再碰库。
    pub fn load(store: &Store) -> Self {
        ScopeGate {
            excl: crate::scan::Exclusions::from_store(store),
            scope: ScopeMap::load(store),
            bad: store.bad_media_upper(),
        }
    }

    /// 一条 L1 结构行的派生状态([`RICH_PENDING`] / [`RICH_OUT`])。
    pub fn rich_state(&self, r: &L1Row) -> i32 {
        self.rich_state_of(&r.path, &r.parent, r.ext.as_deref())
    }

    /// 与 [`ScopeGate::rich_state`] 同一条判定,入参换成散列。
    ///
    /// walkdir 那条发现路拿到的是 `scan::Record`(没有 `L1Row`),两边必须共用
    /// 同一个门,否则非 NTFS 卷会整个绕过作用域。
    pub fn rich_state_of(&self, path: &str, parent: &str, ext: Option<&str>) -> i32 {
        if self.in_scope(path, parent, ext) {
            RICH_PENDING
        } else {
            RICH_OUT
        }
    }

    /// 目录级的**派生许可** —— 行判定、点击懒加载、双向对账共用这一份口径。
    ///
    /// 三层:硬排除(4 条 + 回收站现代名 + 自家产出)一票否决;软排除
    /// (AppData / Program Files / temp / node_modules / 点开头目录 / 用户自定义)
    /// 只在用户亲手导入时让路;都不命中就回账本。
    pub fn dir_derivable(&self, dir: &str) -> bool {
        use crate::scan::ExcludeLayer;
        match self.excl.exclude_layer_dir(Path::new(dir)) {
            Some(ExcludeLayer::Hard) => false,
            Some(ExcludeLayer::Soft) => self.scope.explicit_import(dir),
            None => self.scope.includes(dir),
        }
    }

    /// 这一行的目录是否属于**永远不该进库**的那一类(硬层)。
    ///
    /// 自家产出目录、回收站、系统与程序目录、点开头目录 —— 里面的文件不是素材,而且 mtime 永远最新,进库就会霸占「最近更新」首屏
    /// (首屏 200 张里 190 张是自家 thumbs,全是永远不亮的灰格子)。
    /// L1→L2 落库前按这条筛掉新行,历史行由 [`Store::purge_hard_excluded_once`] 清。
    pub fn never_index(&self, parent: &str) -> bool {
        matches!(
            self.excl.exclude_layer_dir(Path::new(parent)),
            Some(crate::scan::ExcludeLayer::Hard)
        )
    }

    /// 空账本 = 谁都不派生(首启对话框还没答过时的状态)。
    ///
    /// 产品代码不查它(门逐行判定,空账本自然谁都不放行),留着是给
    /// `first_run_scope_seeds_once` 那条断言用的。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.scope.is_empty()
    }

    fn in_scope(&self, path: &str, parent: &str, ext: Option<&str>) -> bool {
        // ① 类型级:扩展名必须落在媒体池里。
        let Some(ext) = ext else {
            return false;
        };
        if !crate::scan::is_media_ext(ext) {
            return false;
        }
        // ② 坏文件台账:立过案的不再自动派生,只有「不隐藏」出口能放行。
        if self.bad.contains(&path.to_ascii_uppercase()) {
            return false;
        }
        // ③ 目录级:排除层 + 意图账本(口径见 [`ScopeGate::dir_derivable`])。
        self.dir_derivable(parent)
    }
}

/// L1 身份:`fnv1a64("<盘符>:<FRN index>")`。
///
/// 只取 FRN 的低 48 位(index)—— 高 16 位是记录序号,文件删除后记录被重用
/// 时它会递增,拿它当身份会让「同一个槽位的不同文件」被判成同一个。
/// 反过来,重用意味着旧文件必然先来了一条 DELETE,行已经删掉了。
pub fn frn64_of(letter: char, frn: u64) -> i64 {
    let idx = frn & 0x0000_FFFF_FFFF_FFFF;
    fnv1a64(&format!("{letter}:{idx:#x}")) as i64
}

impl Store {
    pub fn open() -> Result<Arc<Store>> {
        let path = data_dir().join("index.db");
        // 服务进程可能在 checkpoint/截断 WAL,首次打开偶发 SQLITE_IOERR;短重试避免启动即崩。
        let mut last: Option<anyhow::Error> = None;
        for _ in 0..4 {
            match Connection::open(&path) {
                Ok(conn) => match Self::from_conn(conn) {
                    Ok(s) => return Ok(s.with_file(path)),
                    Err(e) => {
                        last = Some(e);
                        std::thread::sleep(std::time::Duration::from_millis(250));
                    }
                },
                Err(e) => {
                    last = Some(e.into());
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        }
        Err(last
            .expect("重试循环至少执行一次")
            .context(format!("库文件 {}", path.display())))
    }

    /// 文件库补记路径(只读连接池按它开新连接);内存库不调用。
    fn with_file(mut self, path: PathBuf) -> Arc<Store> {
        self.file = Some(path);
        Arc::new(self)
    }

    /// 对既有连接执行建表 + 旧库迁移(测试可注入模拟旧库)。
    pub fn from_conn(conn: Connection) -> Result<Store> {
        use anyhow::Context;
        // 单独执行 WAL 切换:库尾 WAL 需要恢复/截断时失败点都在这里,带上上下文便于定位。
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .context("开启 WAL 日志模式")?;
        conn.execute_batch(SCHEMA)?;
        // 旧库迁移:按需补列(列已存在时静默)
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN dhash INTEGER;");
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN baked INTEGER;");
        let _ =
            conn.execute_batch("ALTER TABLE photos ADD COLUMN star INTEGER NOT NULL DEFAULT 0;");
        let _ = conn.execute_batch(
            "ALTER TABLE photos ADD COLUMN xmp_imported INTEGER NOT NULL DEFAULT 0;",
        );
        let _ = conn.execute_batch("ALTER TABLE faces ADD COLUMN score REAL;");
        let _ = conn.execute_batch("ALTER TABLE faces ADD COLUMN quality REAL;");
        let _ =
            conn.execute_batch("ALTER TABLE faces ADD COLUMN ignored INTEGER NOT NULL DEFAULT 0;");
        // 旧模板无签名,标为未知版本,不参与当前管线的比对与归档。
        let _ = conn.execute_batch(
            "ALTER TABLE face_templates ADD COLUMN sig TEXT NOT NULL DEFAULT '';
             UPDATE face_templates SET sig='unknown' WHERE sig='';",
        );
        let _ = conn.execute_batch("ALTER TABLE tags ADD COLUMN color TEXT;");
        let _ = conn.execute_batch("ALTER TABLE tags ADD COLUMN parent_id INTEGER;");
        let _ = conn.execute_batch(REBUILD_TAG_PARENTS_SQL);
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN lat REAL;");
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN lon REAL;");
        // uid64 持久唯一标识(64 位无符号整数列)。
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN uid64 INTEGER;");
        // 效果栈列(`filters` 列)。
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN filters TEXT;");
        // 视频标记列(NOT NULL 带默认值,旧行自动 0=图片)。
        let _ =
            conn.execute_batch("ALTER TABLE photos ADD COLUMN kind INTEGER NOT NULL DEFAULT 0;");
        // 相机型号池化列 + 池表。
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN cam_id INTEGER;");
        // 缩略图生成失败计数(防 worker 对坏文件无限重试,已知问题见
        // thumbs.rs::backfill_tier 注释):失败满 3 次不再进回填队列。
        let _ = conn
            .execute_batch("ALTER TABLE photos ADD COLUMN thumb_fail INTEGER NOT NULL DEFAULT 0;");
        // P3-ui:标题(缩略图说明;幻灯片/属性面板显示)。
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN caption TEXT;");
        let _ = conn
            .execute_batch("ALTER TABLE photos ADD COLUMN caption_auto INTEGER NOT NULL DEFAULT 0;");
        // 派生作用域两列:rich_state 是 L3 门控的唯一开关(0 未纳入 / 1 待派生 /
        // 2 已派生 / 3 坏文件),ext 让小写扩展名判定不必再逐条切字符串。
        // 旧库不带这两列,靠 ALTER 补;带默认值的 NOT NULL 列,旧行自动落 0。
        let _ = conn
            .execute_batch("ALTER TABLE photos ADD COLUMN rich_state INTEGER NOT NULL DEFAULT 0;");
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN ext TEXT;");
        // L1 身份列:frn64 = fnv1a64("<盘符>:<FRN index>")。
        // 改名/移动时 path 变了但 FRN 不变,所以**身份走 frn64、path 只是属性** ——
        // 这是「path 不再是唯一键」的落地方式。没有另起 uid64 的口径:uid64 的
        // 契约是「一经分配即不变、供外部引用」(见 backfill_uid64),换算法会毁掉它。
        let _ = conn.execute_batch("ALTER TABLE photos ADD COLUMN frn64 INTEGER;");
        // 人脸扫描「已分析」标记:没检出人脸的照片也要记 1,否则它们永远留在
        // photos_todo 队头,每批重复分析、后面的照片永远轮不到(队列卡死)。
        // 清除人脸数据重扫时随 wipe_faces 一并归零。
        let _ = conn
            .execute_batch("ALTER TABLE photos ADD COLUMN face_done INTEGER NOT NULL DEFAULT 0;");
        // P3-ui:相簿(相簿是照片 id 的命名集合,不移动文件)。
        let _ = conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS albums (
                id INTEGER PRIMARY KEY,
                name TEXT UNIQUE NOT NULL,
                ts INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS album_photos (
                album_id INTEGER NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
                photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
                pos INTEGER NOT NULL DEFAULT 0,
                UNIQUE(album_id, photo_id)
            );
            CREATE INDEX IF NOT EXISTS idx_album_photos_album ON album_photos(album_id, pos);",
        );
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
        // 建在迁移列上的索引一律排在 ALTER 之后(同上的旧库坑)。
        // rich_state 是派生队列的取行条件,ext 是快查的过滤条件,都要走索引。
        let _ = conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_photos_rich_state ON photos(rich_state);
             CREATE INDEX IF NOT EXISTS idx_photos_ext ON photos(ext);
             CREATE INDEX IF NOT EXISTS idx_faces_person ON faces(person_id);",
        );
        // frn64 唯一索引 = L1 增量落库的 upsert 冲突靶。
        // **不能**做成 partial index:`ON CONFLICT(frn64)` 只认无 WHERE 的
        // 唯一索引,带上 `WHERE frn64 IS NOT NULL` 会直接报
        // "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint"。
        // 历史行/非 NTFS 行的 frn64 是 NULL,而 SQLite 的唯一索引允许多个 NULL,
        // 所以它们照样共存,仍按 path 身份走。
        let _ = conn
            .execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS idx_photos_frn64 ON photos(frn64);");
        // 写连接兜底等待:与异步写批/其它同步写撞车时排队而不是报 BUSY。
        let _ = conn.busy_timeout(std::time::Duration::from_millis(5000));
        let store = Store {
            writer: Arc::new(Mutex::new(conn)),
            readers: Mutex::new(Vec::new()),
            file: None,
            settings: Mutex::new(HashMap::new()),
            write_tx: Mutex::new(None),
            write_done: Mutex::new(None),
            hard_purged: std::sync::atomic::AtomicUsize::new(0),
        };
        // 启动时补齐缺失的 uid64。
        // 扫描路径会在 `store_insert` 里写,但**外部导入**(WiFi 接收、拖入)
        // 或历史遗留的行可能没有 → 启动时兜底补一次,保证"库内每行都有标识"。
        let _ = store.backfill_uid64();
        // 把旧行残留的 cam TEXT 迁移进字符串池。
        let _ = store.backfill_cam_pool();
        // 旧行补齐小写扩展名(ext IS NULL 的行;填完条件自然不再命中)。
        let _ = store.backfill_ext();
        // 老库簇号即身份,一次性补齐人脸身份列。
        let _ = store.backfill_person_ids_once();
        // 一次性把历史库的派生状态播种进 rich_state。
        let _ = store.seed_rich_state_once();
        // 一次性清掉永远不该进库的历史行(自家缓存、回收站、系统目录…)。
        // 行数留给启动任务:有删除才值得跑一次缩略图缓存 GC。
        let purged = store.purge_hard_excluded_once().unwrap_or(0);
        store
            .hard_purged
            .store(purged, std::sync::atomic::Ordering::Relaxed);
        Ok(store)
    }

    /// 内存库(测试专用,避免污染真实 index.db)。
    #[cfg(test)]
    pub fn open_mem() -> Result<Store> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    // ===== 读写网关(全软件唯一摸连接的入口) =====

    /// 读网关:优先从池里取只读连接(WAL 下与写互不阻塞);
    /// 池空则按文件新开只读连接,开不出来(文件被动过)回退写连接。
    /// 内存库(测试)直接回退写连接。
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        let Some(path) = self.file.clone() else {
            let conn = self.writer.lock().unwrap();
            return f(&conn);
        };
        let popped = self.readers.lock().unwrap().pop();
        let conn = match popped {
            Some(c) => c,
            None => match Self::open_reader(&path) {
                Some(c) => c,
                None => {
                    let conn = self.writer.lock().unwrap();
                    return f(&conn);
                }
            },
        };
        let out = f(&conn);
        if let Ok(mut pool) = self.readers.lock() {
            if pool.len() < READ_POOL_IDLE {
                pool.push(conn);
            }
        }
        out
    }

    /// 同步写网关:要返回值 / 事务 / 大批量写走这里(排队等唯一写连接)。
    pub fn write<T>(&self, f: impl FnOnce(&mut Connection) -> T) -> T {
        let mut conn = self.writer.lock().unwrap();
        f(&mut conn)
    }

    /// 异步写网关:任务进后台写线程的队列,立即返回(UI 写不落等待)。
    /// 写线程按到达顺序执行,并把积压任务共用一次加锁排干(合并提交窗口)。
    pub fn write_async(&self, f: impl FnOnce(&mut Connection) + Send + 'static) {
        let tx = {
            let mut slot = self.write_tx.lock().unwrap();
            if slot.is_none() {
                let (tx, rx) = mpsc::channel::<WriteJob>();
                let writer = self.writer.clone();
                let h = std::thread::Builder::new()
                    .name("db-write".into())
                    .spawn(move || {
                        // 首个任务阻塞等;之后同锁排干积压(一次加锁跑一批)。
                        while let Ok(job) = rx.recv() {
                            let Ok(mut conn) = writer.lock() else {
                                break;
                            };
                            job(&mut conn);
                            while let Ok(job) = rx.try_recv() {
                                job(&mut conn);
                            }
                        }
                    });
                *self.write_done.lock().unwrap() = h.ok();
                slot.insert(tx).clone()
            } else {
                slot.as_ref().expect("写队列入口必在").clone()
            }
        };
        let _ = tx.send(Box::new(f));
    }

    /// 打开一条只读连接(池补充用);失败返回 None(调用方回退写连接)。
    fn open_reader(path: &Path) -> Option<Connection> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .ok()?;
        let _ = conn.busy_timeout(std::time::Duration::from_millis(5000));
        Some(conn)
    }

    /// 等异步写队列排干(同步点)。备份/压缩前必须调,否则快照可能缺尾部写。
    pub fn flush(&self) {
        let (tx, rx) = mpsc::channel::<()>();
        self.write_async(move |_| {
            let _ = tx.send(());
        });
        let _ = rx.recv();
    }

    /// 测试夹具专用:借写连接直接造数据/验证(生产代码一律走网关)。
    #[cfg(test)]
    pub(crate) fn test_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.writer.lock().unwrap()
    }

    pub fn upsert_watched(&self, p: &Path) -> Result<()> {
        let path = p.to_string_lossy().into_owned();
        self.write(move |conn| {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO watched(path) VALUES(?1)",
                rusqlite::params![path],
            );
        });
        Ok(())
    }

    /// 从监听列表移除文件夹(只取消监听,不删索引;对应文件夹管理器)。
    pub fn remove_watched(&self, p: &Path) -> Result<()> {
        let path = p.to_string_lossy().into_owned();
        self.write(move |conn| {
            let _ = conn.execute("DELETE FROM watched WHERE path=?1", rusqlite::params![path]);
        });
        Ok(())
    }

    /// 首启范围是否已落账(的对话框只答一次)。
    pub fn scope_initialized(&self) -> bool {
        self.get_setting("scan_initialized").is_some()
    }

    /// 落账首启范围:批量写派生意图 + 登记监听根 + 打标记,返回首扫的根。
    ///
    /// `complete=true` = 全盘档:每个可扫卷根各落一条 `dir_intent`,
    /// 靠最长前缀命中覆盖整卷;否则只落三个壳目录
    /// (我的文档 / 图片 / 桌面,**不含视频** —— 用户已拍)。
    ///
    /// ⚠ 这一档决定的是**派生**作用域,不是库的作用域:全盘条目照样进 L2,
    /// 没进作用域的行结构列齐全、派生列全空,网格里是占位。
    ///
    /// `watched` 一律只登记壳目录:整卷挂 `ReadDirectoryChangesW` 会被系统临时
    /// 文件的写事件刷爆,全盘的保鲜归服务的 USN 日志管。
    pub fn apply_initial_scope(&self, complete: bool) -> Vec<PathBuf> {
        if self.scope_initialized() {
            return Vec::new();
        }
        let scope_roots: Vec<PathBuf> = if complete {
            crate::win::drive_roots()
        } else {
            default_roots()
        };
        let intents: Vec<DirIntent> = scope_roots
            .iter()
            .map(|r| {
                DirIntent::new(
                    &r.to_string_lossy(),
                    INTENT_DEFAULT_ROOT,
                    SRC_INITIAL_DIALOG,
                )
            })
            .collect();
        let _ = self.set_dir_intents(&intents);
        for r in default_roots() {
            let _ = self.upsert_watched(&r);
        }
        let _ = self.set_setting(
            "initial_scope",
            if complete { "complete" } else { "limited" },
        );
        let _ = self.set_setting("scan_initialized", "1");
        scope_roots
    }

    /// 监听文件夹及其照片数(含子文件夹;文件夹管理器列表面板)。
    pub fn watched_stats(&self) -> Vec<(String, usize)> {
        self.read(|conn| {
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
        })
    }

    /// 读设置项(人脸算法版本等)。走进程内缓存,未命中才查库。
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    pub fn get_setting(&self, k: &str) -> Option<String> {
        if let Some(v) = self.settings.lock().unwrap().get(k) {
            return v.clone();
        }
        let key = k.to_string();
        let v: Option<String> = self.read(move |conn| {
            conn.query_row(
                "SELECT v FROM settings WHERE k=?1",
                rusqlite::params![key],
                |r| r.get::<_, String>(0),
            )
            .ok()
        });
        self.settings.lock().unwrap().insert(k.into(), v.clone());
        v
    }

    /// 写设置项(upsert)。先更新进程内缓存(读侧立即可见),库走异步写。
    #[cfg_attr(not(feature = "ai"), allow(dead_code))]
    pub fn set_setting(&self, k: &str, v: &str) -> Result<()> {
        self.settings
            .lock()
            .unwrap()
            .insert(k.to_string(), Some(v.to_string()));
        let (k, v) = (k.to_string(), v.to_string());
        self.write(move |conn| {
            let _ = conn.execute(
                "INSERT INTO settings(k,v) VALUES(?1,?2)
                 ON CONFLICT(k) DO UPDATE SET v=?2",
                rusqlite::params![k, v],
            );
        });
        Ok(())
    }

    // ===== P3-ui 标题(Caption)与相簿(Album) =====

    /// 读照片标题(无标题返回空串)。
    pub fn get_caption(&self, id: i64) -> String {
        self.read(move |conn| {
            conn.query_row(
                "SELECT caption FROM photos WHERE id=?1",
                rusqlite::params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
            .unwrap_or_default()
        })
    }

    /// 写照片标题(空串 = 清除)。
    pub fn set_caption(&self, id: i64, caption: &str) -> Result<()> {
        let cap = if caption.is_empty() {
            None
        } else {
            Some(caption.to_string())
        };
        self.write(move |conn| {
            let _ = conn.execute(
                "UPDATE photos SET caption=?2 WHERE id=?1",
                rusqlite::params![id, cap],
            );
            // 手改过的标题不再被后台描述刷新覆盖;旧库缺列时静默忽略。
            let _ = conn.execute(
                "UPDATE photos SET caption_auto=0 WHERE id=?1",
                rusqlite::params![id],
            );
        });
        Ok(())
    }

    /// 相簿列表(按创建顺序)。返回 (id, 名称, 照片数)。
    pub fn albums(&self) -> Vec<(i64, String, usize)> {
        self.read(|conn| {
            let mut st = match conn.prepare(
                "SELECT a.id, a.name,
                        (SELECT COUNT(*) FROM album_photos ap WHERE ap.album_id = a.id)
                 FROM albums a ORDER BY a.id",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            st.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? as usize,
                ))
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        })
    }

    /// 单个相簿的名字(标题用)。
    ///
    /// 只取一行;`albums()` 会为每个相簿跑一次成员 COUNT(*) 子查询,
    /// 逐帧画标题时别用它。
    pub fn album_name(&self, album_id: i64) -> Option<String> {
        self.read(move |conn| {
            conn.query_row("SELECT name FROM albums WHERE id=?1", [album_id], |r| {
                r.get::<_, String>(0)
            })
            .ok()
        })
    }

    /// 新建相簿,返回 id(重名返回既有 id)。
    pub fn album_create(&self, name: &str) -> Result<i64> {
        let name = name.to_string();
        self.write(move |conn| {
            if let Ok(id) = conn.query_row(
                "SELECT id FROM albums WHERE name=?1",
                rusqlite::params![name],
                |r| r.get(0),
            ) {
                return Ok(id);
            }
            conn.execute(
                "INSERT INTO albums(name, ts) VALUES(?1, ?2)",
                rusqlite::params![name, chrono::Utc::now().timestamp_millis()],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// 相簿重命名。
    pub fn album_rename(&self, id: i64, name: &str) -> Result<()> {
        let name = name.to_string();
        self.write(move |conn| {
            let _ = conn.execute(
                "UPDATE albums SET name=?2 WHERE id=?1",
                rusqlite::params![id, name],
            );
        });
        Ok(())
    }

    /// 删除相簿(成员照片不动,ON DELETE CASCADE 清映射)。
    pub fn album_delete(&self, id: i64) -> Result<()> {
        self.write(move |conn| {
            let _ = conn.execute("DELETE FROM albums WHERE id=?1", rusqlite::params![id]);
        });
        Ok(())
    }

    /// 照片加入相簿(已存在则忽略;pos 追加到尾部)。
    pub fn album_add_photo(&self, album_id: i64, photo_id: i64) -> Result<()> {
        self.write(move |conn| {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM album_photos WHERE album_id=?1 AND photo_id=?2",
                    rusqlite::params![album_id, photo_id],
                    |r| r.get(0),
                )
                .unwrap_or(1);
            if exists > 0 {
                return;
            }
            let pos: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(pos),-1)+1 FROM album_photos WHERE album_id=?1",
                    rusqlite::params![album_id],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let _ = conn.execute(
                "INSERT INTO album_photos(album_id, photo_id, pos) VALUES(?1,?2,?3)",
                rusqlite::params![album_id, photo_id, pos],
            );
        });
        Ok(())
    }

    /// 照片移出相簿。
    pub fn album_remove_photo(&self, album_id: i64, photo_id: i64) -> Result<()> {
        self.write(move |conn| {
            let _ = conn.execute(
                "DELETE FROM album_photos WHERE album_id=?1 AND photo_id=?2",
                rusqlite::params![album_id, photo_id],
            );
        });
        Ok(())
    }

    /// 相簿成员(按 pos 序),返回照片 id 列表。
    pub fn album_photo_ids(&self, album_id: i64) -> Vec<i64> {
        self.read(move |conn| {
            let mut st = match conn
                .prepare("SELECT photo_id FROM album_photos WHERE album_id=?1 ORDER BY pos")
            {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            st.query_map([album_id], |r| r.get(0))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        })
    }

    /// 照片所在的相簿(用于右键菜单勾选状态)。
    pub fn albums_of_photo(&self, photo_id: i64) -> Vec<(i64, String)> {
        self.read(move |conn| {
            let mut st = match conn.prepare(
                "SELECT a.id, a.name FROM albums a
                 JOIN album_photos ap ON ap.album_id = a.id
                 WHERE ap.photo_id = ?1 ORDER BY a.id",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            st.query_map([photo_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        })
    }

    // ===== 持久化扫描队列 =====

    /// 登记扫描根(已存在则忽略,scope 保留首次值)。
    ///
    /// 用途:① 扫描启动时登记,完成后 `dequeue_scan` 删除;
    /// ② 应用中断退出时残留条目在下次启动被 `pending_scans` 读出 → 续扫。
    pub fn enqueue_scan(&self, roots: &[std::path::PathBuf], scope: &str) -> Result<usize> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let roots: Vec<String> = roots
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect();
        let scope = scope.to_string();
        self.write(move |conn| {
            let mut n = 0;
            for r in &roots {
                n += conn.execute(
                    "INSERT INTO scan_queue(root, scope, enqueued_ts) VALUES(?1,?2,?3)
                     ON CONFLICT(root) DO NOTHING",
                    rusqlite::params![r, scope, now],
                )?;
            }
            Ok(n)
        })
    }

    /// 读出全部待扫根(按入队先后),`(root, scope)`。
    pub fn pending_scans(&self) -> Vec<(String, String)> {
        self.read(|conn| {
            let mut st =
                match conn.prepare("SELECT root, scope FROM scan_queue ORDER BY enqueued_ts, id") {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(|rows| rows.filter_map(|x| x.ok()).collect())
                .unwrap_or_default()
        })
    }

    /// 扫描完成后移除登记(无论成功/取消 —— 取消是用户意志,不续扫)。
    pub fn dequeue_scan(&self, root: &str) -> Result<()> {
        let root = root.to_string();
        self.write(move |conn| {
            let _ = conn.execute(
                "DELETE FROM scan_queue WHERE root=?1",
                rusqlite::params![root],
            );
        });
        Ok(())
    }

    /// 读某张照片的效果栈(`filters` 列)。
    pub fn get_filters(&self, photo_id: i64) -> Option<String> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT filters FROM photos WHERE id=?1",
                rusqlite::params![photo_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        })
    }

    /// 写某张照片的效果栈(空串 = 无效果)。
    pub fn set_filters(&self, photo_id: i64, filters: &str) -> Result<()> {
        let f = filters.to_string();
        self.write(move |conn| {
            let _ = conn.execute(
                "UPDATE photos SET filters=?2 WHERE id=?1",
                rusqlite::params![photo_id, f],
            );
        });
        Ok(())
    }

    /// 批量写效果栈(粘贴所有效果给多张)。
    pub fn set_filters_many(&self, ids: &[i64], filters: &str) -> Result<usize> {
        let ids = ids.to_vec();
        let f = filters.to_string();
        self.write(move |conn| {
            let tx = conn.transaction()?;
            let mut n = 0;
            for id in &ids {
                n += tx.execute(
                    "UPDATE photos SET filters=?2 WHERE id=?1",
                    rusqlite::params![id, f],
                )?;
            }
            tx.commit()?;
            Ok(n)
        })
    }

    /// 批量读效果栈(复制所有效果时取源照片)。
    pub fn get_filters_many(&self, ids: &[i64]) -> Vec<(i64, String)> {
        let ids = ids.to_vec();
        self.read(move |conn| {
            let mut out = Vec::new();
            let Ok(mut st) = conn.prepare("SELECT filters FROM photos WHERE id=?1") else {
                return out;
            };
            for id in &ids {
                if let Ok(Some(f)) =
                    st.query_row(rusqlite::params![id], |r| r.get::<_, Option<String>>(0))
                {
                    out.push((*id, f));
                }
            }
            out
        })
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
        self.flush();
        let d = dest.to_string_lossy().into_owned();
        self.write(move |conn| {
            conn.execute("VACUUM INTO ?1", rusqlite::params![d])
                .map(|_| ())
        })?;
        Ok(())
    }

    /// 压缩索引库(VACUUM:回收删除行占用的空间)。
    pub fn compact(&self) -> Result<()> {
        self.flush();
        self.write(|conn| conn.execute("VACUUM", []).map(|_| ()))?;
        Ok(())
    }

    /// 监听文件夹列表(预留:文件夹监视功能)。
    #[allow(dead_code)]
    pub fn list_watched(&self) -> Vec<PathBuf> {
        self.read(|conn| {
            let mut st = match conn.prepare("SELECT path FROM watched") {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let rows = st.query_map([], |r| r.get::<_, String>(0));
            match rows {
                Ok(rows) => rows.filter_map(|r| r.ok()).map(PathBuf::from).collect(),
                Err(_) => Vec::new(),
            }
        })
    }

    /// 删除一批照片行及其**全部**引用行(须在事务内调用)。
    ///
    /// SQLite 外键级联默认关闭,只删 photos 会留下孤儿行 ——
    /// rowid 复用后孤儿会错挂到新照片上,所以引用表必须逐一显式删。
    /// 同时往变更日志写 `removed`(便于回溯:这些行不是用户手动移除的)。
    fn delete_photo_rows(tx: &rusqlite::Transaction<'_>, rows: &[(i64, String)]) -> Result<()> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut st_tags = tx.prepare_cached("DELETE FROM photo_tags WHERE photo_id=?1")?;
        let mut st_ft = tx.prepare_cached(
            "DELETE FROM face_templates WHERE face_id IN
             (SELECT id FROM faces WHERE photo_id=?1)",
        )?;
        let mut st_faces = tx.prepare_cached("DELETE FROM faces WHERE photo_id=?1")?;
        let mut st_clips = tx.prepare_cached("DELETE FROM clips WHERE photo_id=?1")?;
        let mut st_clips_cn = tx.prepare_cached("DELETE FROM clips_cn WHERE photo_id=?1")?;
        let mut st_tiers = tx.prepare_cached("DELETE FROM thumb_tiers WHERE photo_id=?1")?;
        let mut st_album = tx.prepare_cached("DELETE FROM album_photos WHERE photo_id=?1")?;
        let mut st_jobs = tx.prepare_cached("DELETE FROM ai_jobs WHERE photo_id=?1")?;
        let mut st_state = tx.prepare_cached("DELETE FROM ai_state WHERE photo_id=?1")?;
        let mut st_failed = tx.prepare_cached("DELETE FROM ai_failed WHERE photo_id=?1")?;
        let mut st_auto = tx.prepare_cached("DELETE FROM auto_tags WHERE photo_id=?1")?;
        let mut st_vec = tx.prepare_cached("DELETE FROM clips_vec WHERE photo_id=?1")?;
        let mut st_ocr_seg =
            tx.prepare_cached("SELECT seg FROM ocr_docs WHERE photo_id=?1")?;
        let mut st_fts = tx.prepare_cached(
            "INSERT INTO ocr_fts(ocr_fts, rowid, seg) VALUES('delete', ?1, ?2)",
        )?;
        let mut st_docs = tx.prepare_cached("DELETE FROM ocr_docs WHERE photo_id=?1")?;
        let mut st_boxes = tx.prepare_cached("DELETE FROM ocr_boxes WHERE photo_id=?1")?;
        let mut st_photos = tx.prepare_cached("DELETE FROM photos WHERE id=?1")?;
        let mut st_log = tx.prepare_cached(
            "INSERT INTO changes(ts,path,action,photo_id) VALUES(?1,?2,'removed',?3)",
        )?;
        for (id, path) in rows {
            st_tags.execute(rusqlite::params![id])?;
            st_ft.execute(rusqlite::params![id])?;
            st_faces.execute(rusqlite::params![id])?;
            st_clips.execute(rusqlite::params![id])?;
            st_clips_cn.execute(rusqlite::params![id])?;
            st_tiers.execute(rusqlite::params![id])?;
            st_album.execute(rusqlite::params![id])?;
            // FTS 是外部内容表,删正文前要按原分词发一次 delete。
            if let Ok(seg) = st_ocr_seg.query_row(rusqlite::params![id], |r| r.get::<_, String>(0)) {
                let _ = st_fts.execute(rusqlite::params![id, seg]);
            }
            st_jobs.execute(rusqlite::params![id])?;
            st_state.execute(rusqlite::params![id])?;
            st_failed.execute(rusqlite::params![id])?;
            st_auto.execute(rusqlite::params![id])?;
            st_vec.execute(rusqlite::params![id])?;
            st_docs.execute(rusqlite::params![id])?;
            st_boxes.execute(rusqlite::params![id])?;
            st_photos.execute(rusqlite::params![id])?;
            st_log.execute(rusqlite::params![ts, path, id])?;
        }
        Ok(())
    }

    /// 清理已缺失文件的索引行,返回被清理的路径(图库维护)。
    /// 引用行的清理见 `delete_photo_rows`。
    pub fn mark_missing(&self) -> Result<Vec<String>> {
        let all: Vec<(i64, String)> = self.read(|conn| -> Result<Vec<(i64, String)>> {
            let mut st = conn.prepare("SELECT id, path FROM photos")?;
            let rows: Vec<(i64, String)> = st
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })?;
        let missing: Vec<(i64, String)> = all
            .into_iter()
            .filter(|(_, p)| !Path::new(p).exists())
            .collect();
        if missing.is_empty() {
            return Ok(Vec::new());
        }
        self.write(|conn| {
            let tx = conn.unchecked_transaction()?;
            Self::delete_photo_rows(&tx, &missing)?;
            tx.commit()?;
            Ok(missing.into_iter().map(|(_, p)| p).collect())
        })
    }

    /// 精确移除一批**已确认不存在**的路径的索引行,返回实际删除的 (id, 路径)。
    ///
    /// 与 `mark_missing` 的区别:只查给定路径(及其子项),不遍历全库 ——
    /// 供监听线程在收到删除事件时即时对账,不必等用户手动维护。
    /// 以磁盘为准:路径仍存在时跳过(事件与重命名/回滚之间存在竞态)。
    ///
    /// 目录被删时事件只报目录本身(子文件不一定逐个上报),
    /// 因此还要按路径边界清掉库里的子项。
    pub fn drop_missing_paths(&self, paths: &[String]) -> Result<Vec<(i64, String)>> {
        // 先按磁盘筛掉仍存在的路径(事件与重命名/回滚之间的竞态兜底)。
        let gone: Vec<String> = paths
            .iter()
            .filter(|p| !Path::new(p.as_str()).exists())
            .cloned()
            .collect();
        self.drop_paths_confirmed(&gone)
    }

    /// 精确移除一批**调用方已确认不存在**的路径的索引行,返回删除的 (id, 路径)。
    ///
    /// 与 [`Store::drop_missing_paths`] 的差别只在「谁做存在性复核」:那个自己
    /// 逐个 `stat`,这个把复核交给调用方 —— 监听线程问得到 L1 索引服务时,
    /// 一次管道往返就够了,不必再摸一遍磁盘。
    ///
    /// 目录被删时事件只报目录本身(子文件不一定逐个上报),
    /// 因此还要按路径边界清掉库里的子项。
    pub fn drop_paths_confirmed(&self, gone: &[String]) -> Result<Vec<(i64, String)>> {
        if gone.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(i64, String)> = self.read(|conn| -> Result<Vec<(i64, String)>> {
            let mut st = conn.prepare("SELECT id, path FROM photos WHERE path=?1")?;
            let mut st_sub = conn.prepare(
                "SELECT id, path FROM photos
                 WHERE instr(path, ?1) = 1
                   AND length(path) > length(?1)
                   AND substr(path, length(?1) + 1, 1) IN ('\\', '/')",
            )?;
            let mut rows: Vec<(i64, String)> = Vec::new();
            for p in gone {
                let mut hit = Vec::new();
                if let Ok(r) = st.query_row([p.as_str()], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                }) {
                    hit.push(r);
                }
                if let Ok(it) = st_sub.query_map([p.as_str()], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                }) {
                    hit.extend(it.flatten());
                }
                for r in hit {
                    if !rows.iter().any(|(id, _)| *id == r.0) {
                        rows.push(r);
                    }
                }
            }
            Ok(rows)
        })?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        self.write(|conn| {
            let tx = conn.unchecked_transaction()?;
            Self::delete_photo_rows(&tx, &rows)?;
            tx.commit()?;
            Ok(rows)
        })
    }

    /// 原位改名:把 `from` 的索引行改指 `to`,**id 不变**
    /// (星标/标签/相册/人脸/缩略图缓存全部保留 —— id 键不变)。
    ///
    /// 目标路径已有行(改名覆盖了现有文件)时不覆盖:只删旧行,新位置的内容
    /// 由扫描刷新,返回 Ok(false) 让调用方知道该目录要重扫。
    pub fn rename_photo(&self, from: &str, to: &str, parent: &str) -> Result<bool> {
        let (from, to, parent) = (from.to_string(), to.to_string(), parent.to_string());
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.write(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let dupe: bool = tx
                .query_row("SELECT 1 FROM photos WHERE path=?1", [&to], |_| Ok(true))
                .unwrap_or(false);
            let mut moved = false;
            if dupe {
                // 覆盖式改名:删旧行;`to` 的行还在(内容已变,扫描会刷新)。
                let mut st = tx.prepare("SELECT id, path FROM photos WHERE path=?1")?;
                let rows: Vec<(i64, String)> = st
                    .query_map([&from], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map(|it| it.flatten().collect())
                    .unwrap_or_default();
                drop(st);
                Self::delete_photo_rows(&tx, &rows)?;
            } else {
                let n = tx.execute(
                    "UPDATE photos SET path=?2, parent=?3 WHERE path=?1",
                    rusqlite::params![from, to, parent],
                )?;
                if n > 0 {
                    moved = true;
                    let id: Option<i64> = tx
                        .query_row("SELECT id FROM photos WHERE path=?1", [&to], |r| r.get(0))
                        .ok();
                    tx.execute(
                        "INSERT INTO changes(ts,path,action,photo_id) VALUES(?1,?2,?3,?4)",
                        rusqlite::params![ts, to, "renamed", id],
                    )?;
                }
            }
            tx.commit()?;
            Ok(moved)
        })
    }

    // ===== 变更日志 =====

    /// 记录一条变更(added / modified / removed)。
    pub fn log_change(&self, path: &str, action: &str, photo_id: Option<i64>) -> Result<()> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let (path, action) = (path.to_string(), action.to_string());
        self.write(move |conn| {
            let _ = conn.execute(
                "INSERT INTO changes(ts,path,action,photo_id) VALUES(?1,?2,?3,?4)",
                rusqlite::params![ts, path, action, photo_id],
            );
        });
        Ok(())
    }

    /// 读取最近的变更记录(倒序)。
    pub fn recent_changes(&self, limit: usize) -> Vec<(i64, String, String)> {
        self.read(move |conn| {
            let mut st = match conn
                .prepare("SELECT ts, path, action FROM changes ORDER BY ts DESC, id DESC LIMIT ?1")
            {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            st.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        })
    }

    /// 清空变更日志。
    pub fn clear_changes(&self) -> Result<()> {
        self.write(|conn| {
            let _ = conn.execute("DELETE FROM changes", []);
        });
        Ok(())
    }

    /// 变更日志总条数(状态栏展示)。
    pub fn change_count(&self) -> usize {
        self.read(|conn| {
            conn.query_row("SELECT COUNT(*) FROM changes", [], |r| r.get::<_, i64>(0))
                .unwrap_or(0) as usize
        })
    }

    // ===== uid64 =====

    /// 为尚未分配 uid64 的照片补一个。
    ///
    /// 算法:对 `path + size + mtime` 做 FNV-1a 64 位哈希。
    /// 取这三个字段是刻意的 —— 路径保证唯一性,size/mtime 把标识与文件
    /// 状态绑定(纯路径哈希会导致"改了内容还是同一个 id",纯内容哈希会
    /// 导致"改名就换 id",两者都不理想)。
    ///
    /// **uid64 一经分配即保持不变**:写入用 COALESCE,内容变化**不会**换 id
    /// (uid64 生成算法暂无对应资料,本实现采用上述方案)。
    ///
    /// 返回新分配的条数。
    pub fn backfill_uid64(&self) -> Result<usize> {
        let rows: Vec<(i64, String, i64, i64)> = self.read(|conn| {
            let mut st = match conn
                .prepare("SELECT id, path, size, mtime FROM photos WHERE uid64 IS NULL")
            {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        });
        if rows.is_empty() {
            return Ok(0);
        }
        self.write(move |conn| {
            for (id, path, size, mtime) in &rows {
                let u = fnv1a64(&format!("{path}|{size}|{mtime}"));
                conn.execute(
                    "UPDATE photos SET uid64=?2 WHERE id=?1",
                    rusqlite::params![id, u as i64],
                )?;
            }
            Ok(rows.len())
        })
    }

    /// 小写扩展名(不含点);无扩展名返回 None(与 `scan` 的判定口径一致:
    /// 比较前先 `to_ascii_lowercase`)。
    pub fn ext_of(path: &str) -> Option<String> {
        std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
    }

    /// 旧行补齐 `photos.ext`(NULL 才补,填完自然不再命中,无需额外标记)。
    pub fn backfill_ext(&self) -> Result<usize> {
        let rows: Vec<(i64, String)> = self.read(|conn| {
            let mut st = match conn.prepare("SELECT id, path FROM photos WHERE ext IS NULL") {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        });
        if rows.is_empty() {
            return Ok(0);
        }
        self.write(move |conn| {
            let mut n = 0usize;
            for (id, path) in &rows {
                if let Some(e) = Self::ext_of(path) {
                    conn.execute(
                        "UPDATE photos SET ext=?2 WHERE id=?1",
                        rusqlite::params![id, e],
                    )?;
                    n += 1;
                }
            }
            Ok(n)
        })
    }

    /// 老库簇号即身份,一次性回填人脸身份列;已移出人物的行不再被回写。
    pub fn backfill_person_ids_once(&self) -> Result<usize> {
        if self.get_setting("person_id_v1").is_some() {
            return Ok(0);
        }
        let n = self.write(|conn| {
            conn.execute(
                "UPDATE faces SET person_id=cluster WHERE cluster IS NOT NULL AND person_id IS NULL",
                [],
            )
            .unwrap_or(0)
        });
        let _ = self.set_setting("person_id_v1", "1");
        Ok(n)
    }

    /// 一次性播种 `rich_state`:历史库没有作用域概念,凡是已经有普通档缩略图的行
    /// 视为「已派生」(2),其余留 0。用 settings 标记保证只跑一次 ——
    /// 之后 rich_state 完全由派生门控管,不能被这里的规则回写。
    pub fn seed_rich_state_once(&self) -> Result<usize> {
        if self.get_setting("rich_state_seeded").is_some() {
            return Ok(0);
        }
        let n = self.write(|conn| {
            conn.execute(
                "UPDATE photos SET rich_state=2 WHERE thumb_ok=1 AND rich_state<>2",
                [],
            )
            .unwrap_or(0)
        });
        let _ = self.set_setting("rich_state_seeded", "1");
        Ok(n)
    }

    /// 一次性把**永远不该进库**的历史行清掉。
    ///
    /// 命中 [`crate::scan::ExcludeLayer::Hard`] 的那些目录 —— 自家产出目录、
    /// 回收站、系统与程序目录、点开头目录 —— 里面的文件不是素材:它们的
    /// mtime 永远最新,于是「最近更新」首屏 200 张里 190 张是自家 thumbs
    /// (全是 `rich_state=0` 的灰格子),用户看到的就是「整屏不亮、等半天
    /// 也不更新」。
    ///
    /// 增量入库那一侧同步拦掉新行(`nameidx::ingest_l1`),这里清历史行。
    /// 软层(AppData 一类)**不清** —— 那些行留在库里当占位,用户对着它
    /// 右键「纳入此目录」就能放行(AppData 也要能访问)。
    ///
    /// 标记值 = 名单版本 + 当前解析出的自有前缀清单,不是一个布尔:缓存位置
    /// 会被 `MALIANG_CACHE_DIR` / `data.location` 改到别处,硬名单也会随版本
    /// 调整,任一处变了就得再清一次。
    pub fn purge_hard_excluded_once(&self) -> Result<usize> {
        let excl = crate::scan::Exclusions::from_store(self);
        let mut key = String::from(HARD_PURGE_VERSION);
        for p in excl.own_prefixes() {
            key.push('\n');
            key.push_str(&p.to_string_lossy().to_ascii_lowercase());
        }
        if self.get_setting("hard_excluded_purged").as_deref() == Some(key.as_str()) {
            return Ok(0);
        }
        let mut total = 0usize;
        let mut last_id = 0i64;
        loop {
            // 分页扫而不是一次拉完:全盘索引的行数是百万级,
            // 一次 `SELECT id,parent FROM photos` 会把整表路径堆在内存里。
            let chunk: Vec<(i64, String)> = self.read(|conn| -> Result<Vec<(i64, String)>> {
                let mut st = conn.prepare(
                    "SELECT id, parent FROM photos WHERE id > ?1 ORDER BY id LIMIT 50000",
                )?;
                let rows = st.query_map([last_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })?;
                Ok(rows.filter_map(|r| r.ok()).collect())
            })?;
            if chunk.is_empty() {
                break;
            }
            last_id = chunk.last().map(|(id, _)| *id).unwrap_or(last_id);
            let hits: Vec<i64> = chunk
                .into_iter()
                .filter(|(_, parent)| {
                    matches!(
                        excl.exclude_layer_dir(Path::new(parent)),
                        Some(crate::scan::ExcludeLayer::Hard)
                    )
                })
                .map(|(id, _)| id)
                .collect();
            if !hits.is_empty() {
                total += self.delete_rows_quiet(hits)?;
            }
        }
        let _ = self.set_setting("hard_excluded_purged", &key);
        Ok(total)
    }

    /// 事务内清掉 purge_ids 里的照片及全部引用行(不写变更日志)。
    fn delete_purge_ids_tx(tx: &rusqlite::Transaction<'_>) -> Result<()> {
        // FTS 是外部内容表,删正文前要按原分词发一次 delete。
        let _ = tx.execute_batch(
            "INSERT INTO ocr_fts(ocr_fts, rowid, seg)
               SELECT 'delete', d.photo_id, d.seg FROM ocr_docs d
                WHERE d.photo_id IN (SELECT id FROM purge_ids);",
        );
        tx.execute_batch(
            "DELETE FROM photo_tags     WHERE photo_id IN (SELECT id FROM purge_ids);
             DELETE FROM face_templates WHERE face_id IN
                 (SELECT id FROM faces WHERE photo_id IN (SELECT id FROM purge_ids));
             DELETE FROM faces         WHERE photo_id IN (SELECT id FROM purge_ids);
             DELETE FROM clips         WHERE photo_id IN (SELECT id FROM purge_ids);
             DELETE FROM clips_cn      WHERE photo_id IN (SELECT id FROM purge_ids);
             DELETE FROM thumb_tiers   WHERE photo_id IN (SELECT id FROM purge_ids);
             DELETE FROM album_photos  WHERE photo_id IN (SELECT id FROM purge_ids);
             DELETE FROM ai_jobs       WHERE photo_id IN (SELECT id FROM purge_ids);
             DELETE FROM ai_state      WHERE photo_id IN (SELECT id FROM purge_ids);
             DELETE FROM ai_failed     WHERE photo_id IN (SELECT id FROM purge_ids);
             DELETE FROM auto_tags     WHERE photo_id IN (SELECT id FROM purge_ids);
             DELETE FROM clips_vec     WHERE photo_id IN (SELECT id FROM purge_ids);
             DELETE FROM ocr_docs      WHERE photo_id IN (SELECT id FROM purge_ids);
             DELETE FROM ocr_boxes     WHERE photo_id IN (SELECT id FROM purge_ids);
             DELETE FROM photos        WHERE id IN (SELECT id FROM purge_ids);",
        )?;
        Ok(())
    }

    /// 按 id 批量删行,**不写变更日志**:清的是从未属于图库的行,往 `changes`
    /// 里灌十万条 `removed` 会把真正的用户删除记录冲掉。
    ///
    /// 引用表逐条清:SQLite 外键级联默认关闭,只删 `photos` 会留下孤儿行,
    /// rowid 复用后孤儿会错挂到新照片上(口径同 [`Store::delete_photo_rows`])。
    fn delete_rows_quiet(&self, ids: Vec<i64>) -> Result<usize> {
        const CHUNK: usize = 20_000;
        let mut total = 0usize;
        for part in ids.chunks(CHUNK) {
            let chunk = part.to_vec();
            let chunk2 = chunk.clone();
            total += self.write(move |conn| {
                let part = chunk2;
                let tx = conn.unchecked_transaction()?;
                tx.execute_batch(
                    "CREATE TEMP TABLE IF NOT EXISTS purge_ids(id INTEGER PRIMARY KEY);
                     DELETE FROM purge_ids;",
                )?;
                {
                    let mut st = tx.prepare_cached("INSERT INTO purge_ids(id) VALUES(?1)")?;
                    for id in &part {
                        st.execute(rusqlite::params![id])?;
                    }
                }
                Self::delete_purge_ids_tx(&tx)?;
                let n = part.len();
                tx.commit()?;
                Ok::<usize, anyhow::Error>(n)
            })?;
            // 行删了,盘上的缩略图/预览缓存也得跟着走:无主文件既占空间,
            // rowid 复用后还会把旧图显示给新照片。
            for id in &chunk {
                crate::thumbs::purge_cached(*id);
            }
        }
        Ok(total)
    }

    /// 按 uid64 查照片(供外部工具按持久标识引用)。
    pub fn photo_by_uid64(&self, uid: u64) -> Option<i64> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT id FROM photos WHERE uid64=?1",
                rusqlite::params![uid as i64],
                |r| r.get(0),
            )
            .ok()
        })
    }

    // ===== L1 → L2 增量落库 =====

    /// 批量把 L1 结构行写进 photos(单事务,WAL/synchronous=NORMAL)。
    ///
    /// 身份是 `frn64`:改名/移动命中同一行 → 只改 path/parent,星标、标签、
    /// 说明、已派生的缩略图状态全部保住。这正是「path 不再是唯一键」的意义。
    ///
    /// path 上仍留着建表时的 UNIQUE(一个路径同时只可能是一个文件,这条约束
    /// 本身没错),所以 upsert 前先摘掉「同 path、不同 frn64」的陈旧行 ——
    /// 那必然是被删除/被覆盖的旧文件,不摘就会撞约束。
    ///
    /// `gate` 给定时顺手把派生门的结果写进 `rich_state`;
    /// 给 `None` 表示调用方不参与作用域(测试/兼容扫描),新行按未纳入落。
    /// 冲突分支**只升不降**:门说在作用域内才可能把 0 抬到 1,已派生(2)与
    /// 坏文件(3)由 [`Store::invalidate_dir`] / [`Store::mark_bad_media`] 管;
    /// 文件变了(mtime 不同)则退回 1 重派生,与 `thumb_ok` 的重置同一口径。
    pub fn ingest_l1_rows(&self, rows: &[L1Row], gate: Option<&ScopeGate>) -> Result<L1Ingest> {
        if rows.is_empty() {
            return Ok(L1Ingest::default());
        }
        let rows = rows.to_vec();
        self.write(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let mut ins = tx.prepare_cached(
                "INSERT INTO photos(path,parent,mtime,size,kind,ext,frn64,uid64,rich_state)
                 VALUES(:path,:parent,:mtime,:size,:kind,:ext,:frn64,:uid64,:rich)
                 ON CONFLICT(frn64) DO UPDATE SET
                   path=:path, parent=:parent, mtime=:mtime, size=:size,
                   kind=:kind, ext=:ext,
                   -- 内容变了不清旧缩略图与 dhash:磁盘缓存还在,旧图继续显示、
                   -- 重复检测分组也不散。只重新排队,等稳定期过后做一次覆盖。
                   rich_state=CASE
                     WHEN photos.mtime<>:mtime AND photos.thumb_ok=1
                          AND photos.rich_state<>3 THEN 1
                     WHEN :rich<>1 THEN photos.rich_state
                     WHEN photos.rich_state=0 THEN 1
                     ELSE photos.rich_state
                   END",
            )?;
            let mut stale = tx.prepare_cached(
                "SELECT id FROM photos WHERE path=:path AND (frn64 IS NULL OR frn64<>:frn64)",
            )?;
            let mut probe = tx.prepare_cached("SELECT 1 FROM photos WHERE frn64=:frn64")?;
            let mut out = L1Ingest::default();
            for r in &rows {
                // 先摘陈旧占用(连引用表一起),再 upsert;同事务,失败一起回滚。
                let ids: Vec<i64> = stale
                    .query_map(
                        rusqlite::named_params! { ":path": r.path, ":frn64": r.frn64 },
                        |row| row.get(0),
                    )?
                    .filter_map(|x| x.ok())
                    .collect();
                if !ids.is_empty() {
                    tx.execute_batch(
                        "CREATE TEMP TABLE IF NOT EXISTS purge_ids(id INTEGER PRIMARY KEY);
                         DELETE FROM purge_ids;",
                    )?;
                    {
                        let mut put = tx.prepare_cached("INSERT INTO purge_ids(id) VALUES(?1)")?;
                        for id in &ids {
                            put.execute(rusqlite::params![id])?;
                        }
                    }
                    Self::delete_purge_ids_tx(&tx)?;
                }
                let existed = probe
                    .exists(rusqlite::named_params! { ":frn64": r.frn64 })
                    .unwrap_or(false);
                // uid64 只在**新建行**时分配:它的契约是「一经分配即不变」,
                // 冲突更新分支不碰它。
                let uid = fnv1a64(&format!("{}|{}|{}", r.path, r.size, r.mtime)) as i64;
                let rich = gate.map(|g| g.rich_state(r)).unwrap_or(RICH_OUT);
                ins.execute(rusqlite::named_params! {
                    ":path": r.path, ":parent": r.parent, ":mtime": r.mtime, ":size": r.size,
                    ":kind": r.kind, ":ext": r.ext, ":frn64": r.frn64, ":uid64": uid,
                    ":rich": rich,
                })?;
                if existed {
                    out.updated += 1;
                } else {
                    out.inserted += 1;
                }
            }
            drop(ins);
            drop(stale);
            drop(probe);
            tx.commit()?;
            Ok(out)
        })
    }

    /// 按 L1 身份批量删除(FRN 消失 = 文件被删,与 path 无关)。
    /// 返回实际删掉的行数。
    pub fn delete_l1_frn64(&self, frn64s: &[i64]) -> Result<usize> {
        if frn64s.is_empty() {
            return Ok(0);
        }
        let frn64s = frn64s.to_vec();
        self.write(move |conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS purge_ids(id INTEGER PRIMARY KEY);
                 DELETE FROM purge_ids;",
            )?;
            {
                let mut put = tx.prepare_cached(
                    "INSERT OR IGNORE INTO purge_ids(id) SELECT id FROM photos WHERE frn64=?1",
                )?;
                for f in &frn64s {
                    put.execute(rusqlite::params![f])?;
                }
            }
            let n: usize = tx.query_row("SELECT COUNT(*) FROM purge_ids", [], |r| r.get(0))?;
            if n > 0 {
                Self::delete_purge_ids_tx(&tx)?;
            }
            tx.commit()?;
            Ok(n)
        })
    }

    /// L2 已落库到的 USN(每卷一个;0 = 从未落过)。
    /// App 关着的那段时间服务侧 journal 一直在推进;UI 起来后比对
    /// `nameidx::journal_cursors` 与此值,把差额补进 L2。
    pub fn l2_cursor(&self, letter: char) -> i64 {
        self.get_setting(&format!("l1_ingest_usn_{letter}"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    /// 记下 L2 已落库到的 USN。
    pub fn set_l2_cursor(&self, letter: char, usn: i64) -> Result<()> {
        self.set_setting(&format!("l1_ingest_usn_{letter}"), &usn.to_string())
    }

    // ===== 字符串池 =====

    /// 把一个字符串驻留进指定池,返回池 id。
    ///
    /// 语义对齐字符串驻留池(见 `pool.rs` 模块文档):
    /// 同串恒返回同 id;新串 append 分配新 id(= 当前长度)。
    ///
    /// 落盘时**先查 `string_pool` 的唯一索引**,命中即复用,
    /// 未命中才分配新 id 并插入 —— 这就是"查重"。
    ///
    /// 批量落库不走这里(逐条一次写事务太慢),走 [`Store::intern_batch`]
    /// (内存 `PoolSet` 先去重,每个唯一值只发一次 SQL);
    /// 本函数是单条入口,产品代码里没有调用点,留给测试与工具。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn intern(&self, pool: &str, value: &str) -> Result<i64> {
        let (pool, value) = (pool.to_string(), value.to_string());
        self.write(move |conn| {
            // 查重(字符串查重哈希的链上精确比较)
            if let Ok(id) = conn.query_row(
                "SELECT value_id FROM string_pool WHERE pool=?1 AND value=?2",
                rusqlite::params![pool, value],
                |r| r.get::<_, i64>(0),
            ) {
                return Ok(id);
            }
            // append 分配:下一个可用下标(字符串查重哈希)
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
        })
    }

    /// 批量驻留:先用内存 `PoolSet` 去重,再**每个唯一值只发一次 SQL**。
    ///
    /// 返回与 `items` 等长同序的 `(池名, 池 id)`。
    ///
    /// 这正是「先入内存池、落盘时才整体重算」的意义:
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
        let uniq: Vec<(String, String)> = uniq
            .into_iter()
            .map(|(p, v)| (p.to_string(), v.to_string()))
            .collect();
        let db_ids: std::collections::HashMap<(String, String), i64> = self.write(
            |conn| -> Result<std::collections::HashMap<(String, String), i64>> {
                let tx = conn.unchecked_transaction()?;
                let mut db_ids: std::collections::HashMap<(String, String), i64> =
                    std::collections::HashMap::new();
                for (p, v) in &uniq {
                    let id = Self::intern_tx(&tx, p, v)?;
                    db_ids.insert((p.clone(), v.clone()), id);
                }
                tx.commit()?;
                Ok(db_ids)
            },
        )?;
        // ③ 展开回原始顺序
        let mut out = Vec::with_capacity(items.len());
        for (p, v) in items {
            let id = *db_ids
                .get(&(p.clone(), v.clone()))
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

    /// 按 id 取回池中字符串。池内按 id 取串。
    pub fn pooled_value(&self, pool: &str, id: i64) -> Option<String> {
        self.read(move |conn| {
            conn.query_row(
                "SELECT value FROM string_pool WHERE pool=?1 AND value_id=?2",
                rusqlite::params![pool, id],
                |r| r.get::<_, String>(0),
            )
            .ok()
        })
    }

    /// 启动时把**旧行**里还留在 `cam` TEXT 列的相机型号补进池。
    ///
    /// 之前入库的行没有 `cam_id`;`timeline::load_all` 的
    /// `LEFT JOIN` 会回退显示 `cam`,但那样这些行就享受不到池化。
    /// 返回被迁移的行数。
    pub fn backfill_cam_pool(&self) -> Result<usize> {
        let pending: Vec<(i64, String)> = self.read(|conn| {
            let mut st = match conn.prepare(
                "SELECT id, cam FROM photos WHERE cam_id IS NULL AND cam IS NOT NULL AND cam<>''",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let rows: Vec<(i64, String)> = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .ok()
                .map(|rows| rows.filter_map(|x| x.ok()).collect())
                .unwrap_or_default();
            rows
        });
        if pending.is_empty() {
            return Ok(0);
        }
        let mut migrated = 0usize;
        for (id, cam) in &pending {
            // 每行一次写连接驻留 + 更新(同锁完成,避免两条写交错)。
            let (pool, id) = (crate::timeline::CAM_POOL.to_string(), *id);
            let cam = cam.clone();
            let n: usize = self.write(move |conn| -> Result<usize> {
                let cid = Self::intern_tx(conn, &pool, &cam)?;
                Ok(conn.execute(
                    "UPDATE photos SET cam_id=?2, cam=NULL WHERE id=?1",
                    rusqlite::params![id, cid],
                )?)
            })?;
            migrated += n;
        }
        Ok(migrated)
    }

    /// 列出全部相机型号及各自照片数(按张数降序)。
    ///
    /// 走 `string_pool` 的 LEFT JOIN,池未命中时回退旧 `cam` 列
    /// —— 与 `timeline::load_all` 同一套兼容策略。
    pub fn cams(&self) -> Result<Vec<(String, i64)>> {
        self.read(|conn| {
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
        })
    }

    /// 池统计:`(池名, 条数, 业务表引用行数, 去重率)`。
    /// 去重率 = 条数 / 引用行数(越小越省)。
    pub fn pool_stats(&self) -> Result<Vec<(String, i64, i64)>> {
        self.read(|conn| {
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
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?
                .filter_map(|x| x.ok())
                .collect();
            Ok(rows)
        })
    }

    /// 把库里各池的**当前存活 id 集合**读出来,交给 `pool::vacuum_plan` 预估收益。
    ///
    /// 判断依据:某 id 是否还被现存行引用
    /// (字符串入池只对当前行数组里的值入池)。
    pub fn live_pool_ids(&self) -> Result<std::collections::HashMap<String, crate::pool::HashSet>> {
        self.read(|conn| {
            let mut st =
                conn.prepare("SELECT DISTINCT cam_id FROM photos WHERE cam_id IS NOT NULL")?;
            let ids: Vec<i64> = st
                .query_map([], |r| r.get::<_, i64>(0))?
                .filter_map(|x| x.ok())
                .collect();
            let mut out = std::collections::HashMap::new();
            out.insert(
                crate::timeline::CAM_POOL.to_string(),
                ids.into_iter().collect(),
            );
            Ok(out)
        })
    }

    /// 把当前 `string_pool` 整表读进内存池(供 UI 预估 / 诊断)。
    pub fn load_pool_set(&self) -> Result<crate::pool::PoolSet> {
        let rows: Vec<(String, i64, String)> =
            self.read(|conn| -> Result<Vec<(String, i64, String)>> {
                let mut st = conn.prepare(
                    "SELECT pool, value_id, value FROM string_pool ORDER BY pool, value_id",
                )?;
                let rows: Vec<(String, i64, String)> = st
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .filter_map(|x| x.ok())
                    .collect();
                Ok(rows)
            })?;
        let mut set = crate::pool::PoolSet::new();
        for (pool, id, val) in &rows {
            let got = set.intern(pool, val);
            // 自检:append-only 契约 —— 池 id 必须等于库里的 value_id。
            // 这不是多余断言,而是"id = 数组下标"这条语义
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
        let pool = pool.to_string();
        self.write(move |conn| {
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
                let mut st = tx.prepare(
                    "SELECT value_id, value FROM string_pool WHERE pool=?1 ORDER BY value_id",
                )?;
                let rows: Vec<(i64, String)> = st
                    .query_map(rusqlite::params![pool], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .filter_map(|x| x.ok())
                    .collect();
                rows
            };
            // 内存池先算重映射 —— 这是实际路径:
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
        })
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
                let p = pool.clone();
                self.read(move |conn| {
                    conn.query_row(
                        "SELECT COUNT(*) FROM string_pool WHERE pool=?1",
                        rusqlite::params![p],
                        |r| r.get(0),
                    )
                    .unwrap_or(0)
                })
            };
            #[allow(clippy::cast_sign_loss)]
            let orphans = (before - remap.len() as i64).max(0) as usize;
            let (o, m) = self.vacuum_pool(&pool)?;
            out.insert(pool, (o.max(orphans.min(o)), m));
        }
        Ok(out)
    }

    // ===== 多档缩略图 =====

    /// 标记某张照片的某档缩略图已生成。
    pub fn mark_tier_done(&self, photo_id: i64, tier: u8) -> Result<()> {
        // 只为仍存在的照片记完成:删除(监听对账)与后台缩略图存在竞态,
        // 孤儿 thumb_tiers 行会在 rowid 复用时错挂到新照片上。
        self.write(move |conn| {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO thumb_tiers(photo_id,tier)
                 SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM photos WHERE id=?1)",
                rusqlite::params![photo_id, tier as i64],
            );
        });
        Ok(())
    }

    /// (id, 路径)是否仍是同一条索引行 —— 后台生成(缩略图/预览)与删除对账
    /// 存在竞态,生成完成后用它判断产物是否已过期。比对路径是为了覆盖
    /// rowid 被复用给另一张照片的情况。
    pub fn photo_alive(&self, id: i64, path: &str) -> bool {
        let path = path.to_string();
        self.read(move |conn| {
            conn.query_row(
                "SELECT 1 FROM photos WHERE id=?1 AND path=?2",
                rusqlite::params![id, path],
                |_| Ok(()),
            )
            .is_ok()
        })
    }

    /// 取该档**尚未生成**的照片 id + 路径(供后台 worker 批量补齐)。
    ///
    /// Normal 档(tier 1)的取行条件从「所有 `thumb_ok=0`」改成派生门放行的
    /// `rich_state=1`:不在作用域里的行根本不进队列,而不是
    /// 进了队列再失败。排序键是**可见优先 + 全库兜底** —— `visible` 里的目录
    /// 先出,不足 `limit` 再用全库补齐;用户眼前的图先清晰,后台最终把整个
    /// 作用域做完。
    ///
    /// 反复失败的行不再靠 `thumb_fail` 阈值过滤:达到阈值即由
    /// [`Store::mark_bad_media`] 立案、`rich_state` 置 3 出队,
    /// 台账才是权威,`thumb_fail` 退化成计数。
    /// 摘掉某张照片某一档的缩略图台账,让回填队列重新取它。
    ///
    /// 中档(Normal)缓存损坏时只删行还不够:派生状态已落 `rich_state=2`,
    /// `pending_rich` 不会再取 —— 把它退回「待派生」(仅当仍是 2,坏文件立案
    /// 的 3 不能动)才会重新生成。其余档删 `thumb_tiers` 行即可。
    /// 批量摘台账(启动对账用):一次事务里删 `thumb_tiers` 行,并把其中
    /// 中档(Normal)对应、且仍处「已派生」的照片退回「待派生」。
    pub fn clear_tier_rows(&self, rows: &[(i64, u8)]) {
        if rows.is_empty() {
            return;
        }
        let rows = rows.to_vec();
        let _ = self.write(move |conn| {
            let _ = conn.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS clear_ids(id INTEGER, tier INTEGER);
                 DELETE FROM clear_ids;",
            );
            let mut st = match conn.prepare("INSERT INTO clear_ids(id, tier) VALUES(?1, ?2)") {
                Ok(s) => s,
                Err(_) => return,
            };
            for (id, tier) in &rows {
                let _ = st.execute(rusqlite::params![id, *tier as i64]);
            }
            drop(st);
            let _ = conn.execute_batch(
                "DELETE FROM thumb_tiers
                 WHERE (photo_id, tier) IN (SELECT id, tier FROM clear_ids);
                 UPDATE photos SET thumb_ok=0, rich_state=1
                 WHERE rich_state=2
                   AND id IN (SELECT id FROM clear_ids WHERE tier=1);",
            );
        });
    }

    pub fn clear_tier_row(&self, photo_id: i64, tier: u8) {
        let _ = self.write(move |conn| {
            let _ = conn.execute(
                "DELETE FROM thumb_tiers WHERE photo_id=?1 AND tier=?2",
                rusqlite::params![photo_id, tier as i64],
            );
            if tier == 1 {
                let _ = conn.execute(
                    "UPDATE photos SET thumb_ok=0, rich_state=1
                     WHERE id=?1 AND rich_state=2",
                    rusqlite::params![photo_id],
                );
            }
        });
    }

    pub fn missing_tier(&self, tier: u8, visible: &[String], limit: usize) -> Vec<(i64, String)> {
        if limit == 0 {
            return Vec::new();
        }
        if tier != 1 {
            // 其余档走 thumb_tiers 表:没有该行 = 还没生成。这些档只在用户
            // 真点开时按需回填,不参与作用域门控。
            return self.read(move |conn| {
                let mut st = match conn.prepare(
                    "SELECT p.id, p.path FROM photos p
                     LEFT JOIN thumb_tiers t ON t.photo_id=p.id AND t.tier=?2
                     WHERE t.photo_id IS NULL ORDER BY p.id LIMIT ?1",
                ) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                st.query_map(rusqlite::params![limit as i64, tier as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .ok()
                .map(|it| it.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
            });
        }
        // 第一趟只服务用户眼前那几个目录:立刻做,不设稳定期。
        // 其余批次都走稳定期:反复改写的文件(下载中、接收目录、编辑器保存)
        // 等它安静一会儿再重做一次,避免同一张图被反复解码。
        let out = if visible.is_empty() {
            self.pending_rich(&[], limit, &[], true)
        } else {
            let mut o = self.pending_rich(visible, limit, &[], false);
            if o.len() < limit {
                let skip: Vec<i64> = o.iter().map(|r| r.0).collect();
                o.extend(self.pending_rich(&[], limit - o.len(), &skip, true));
            }
            o
        };
        out
    }

    /// 闲时低速补齐:既不在常用/导入目录、又不在排除名单里的普通照片。
    ///
    /// 只在派生队列排空之后由后台调用,每轮固定一小批,不跟用户眼前的图
    /// 或常用目录抢时间。按最近变更优先取,让全盘的重复检测、首页与时间线
    /// 数据最终补齐;排除名单在 Rust 侧过滤(名单来自设置,SQL 里查不到)。
    pub fn missing_idle(&self, limit: usize) -> Vec<(i64, String)> {
        if limit == 0 {
            return Vec::new();
        }
        let cand: Vec<(i64, String, String)> = self.read(|conn| {
            let mut st = match conn.prepare(
                "SELECT id, path, parent FROM photos
                 WHERE rich_state=0 AND thumb_ok=0
                   AND upper(path) NOT IN (SELECT upper(path) FROM bad_media)
                 ORDER BY CASE WHEN mtime >= 100000000000000 THEN mtime / 1000000
                               WHEN mtime >= 100000000000 THEN mtime / 1000
                               ELSE mtime END DESC, id
                 LIMIT ?1",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            st.query_map([limit as i64 * 4], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .ok()
            .map(|it| it.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        });
        let excl = crate::scan::Exclusions::from_store(self);
        cand.into_iter()
            .filter(|(_, _, parent)| excl.exclude_layer_dir(Path::new(parent)).is_none())
            .take(limit)
            .map(|(id, path, _)| (id, path))
            .collect()
    }

    /// 待派生队列里的行(`rich_state=1`)。
    ///
    /// 条件不再要求 `thumb_ok=0`:文件改过之后旧缩略图仍留在磁盘上供界面
    /// 显示,行照样回到队列里刷新一次,`mark_derived` 会整条覆盖回写。
    /// `dirs` 非空时只取这些目录**直属**的行(可见优先那一趟);`skip` 里的
    /// id 不再返回(兜底那一趟要避开已经拿到的)。SQL 与参数表一起动态拼,
    /// 但值一律走绑定,不做字符串内插。
    fn pending_rich(
        &self,
        dirs: &[String],
        limit: usize,
        skip: &[i64],
        settle: bool,
    ) -> Vec<(i64, String)> {
        if limit == 0 {
            return Vec::new();
        }
        let mut sql = String::from("SELECT id, path FROM photos WHERE rich_state=1");
        if settle {
            // thumb_ok=0 是第一次派生(新照片),不压;thumb_ok=1 是内容改过要
            // 重做的旧照片,给它一段安静期。归一后的 mtime 是 UNIX 秒。
            sql.push_str(
                " AND (thumb_ok=0 OR
                   CASE WHEN mtime >= 100000000000000 THEN mtime / 1000000
                        WHEN mtime >= 100000000000 THEN mtime / 1000
                        ELSE mtime END
                   <= CAST(strftime('%s','now') AS INTEGER) - 20)",
            );
        }
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        let dirs: Vec<&String> = dirs
            .iter()
            .filter(|d| !d.trim().is_empty())
            .take(64)
            .collect();
        if !dirs.is_empty() {
            let mut ors = Vec::with_capacity(dirs.len());
            for d in dirs {
                params.push(Box::new(like_escape(&norm_dir(d))));
                ors.push(format!("parent LIKE ?{} ESCAPE '\\'", params.len()));
            }
            sql.push_str(&format!(" AND ({})", ors.join(" OR ")));
        }
        for id in skip {
            params.push(Box::new(*id));
            sql.push_str(&format!(" AND id<>?{}", params.len()));
        }
        // 最近变更优先:同一批里先做用户刚动过的照片。mtime 在历史库里有
        // 秒 / 毫秒 / 100ns 三种写法,沿用时间线的归一表达式再排。
        sql.push_str(
            " ORDER BY CASE WHEN mtime >= 100000000000000 THEN mtime / 1000000
                            WHEN mtime >= 100000000000 THEN mtime / 1000
                            ELSE mtime END DESC, id",
        );
        params.push(Box::new(limit as i64));
        sql.push_str(&format!(" LIMIT ?{}", params.len()));
        self.read(move |conn| {
            let mut st = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
            st.query_map(refs.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))
                .ok()
                .map(|it| it.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        })
    }

    /// 派生失败记账:失败计数 +1,返回累计值。
    ///
    /// 计数达 [`BAD_MEDIA_THRESHOLD`] 时由调用方立案:写
    /// `bad_media` 台账并把 `rich_state` 置 3 出队,不再每轮重试同一张坏图。
    /// 文件本身变化(mtime 更新)由落库路径重置 `thumb_ok` 并退回待派生。
    pub fn mark_thumb_fail(&self, id: i64) -> i64 {
        self.write(move |conn| {
            let _ = conn.execute(
                "UPDATE photos SET thumb_fail=thumb_fail+1 WHERE id=?1",
                rusqlite::params![id],
            );
            conn.query_row(
                "SELECT thumb_fail FROM photos WHERE id=?1",
                rusqlite::params![id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
        })
    }

    /// 中档派生完成回写:派生列 + 缩略图指纹 + 状态,一条 UPDATE 落定。
    ///
    /// 发现路径不读文件内容,`width/height/taken_utc/cam_id/lat/lon/rot`
    /// 这些派生列只由这里写;`thumb_ok` / `dhash` / `rich_state` 必须同一条语句 ——
    /// 分开写会出现 `thumb_ok=1` 而 `rich_state` 还停在待派生的中间态,
    /// 下一轮又被取回来重做。
    ///
    /// 入参 `(id, dhash, 派生列)`;`d` 为 `None` 表示这行没有派生列可写
    /// (只推状态)。相机型号先入池再写 id,池化必须在事务**外**做完 ——
    /// `intern_batch` 自身要拿连接锁,不能在 tx 内重入。
    pub fn mark_derived(&self, rows: &[(i64, u64, Option<&crate::scan::Derived>)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let cam_items: Vec<(String, String)> = rows
            .iter()
            .map(|(_, _, d)| {
                (
                    crate::timeline::CAM_POOL.to_string(),
                    d.and_then(|d| d.cam.as_deref())
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                )
            })
            .collect();
        let has_cam: Vec<bool> = cam_items.iter().map(|(_, v)| !v.is_empty()).collect();
        let interned = self.intern_batch(&cam_items)?;
        let cam_ids: Vec<Option<i64>> = interned
            .iter()
            .zip(has_cam.iter())
            .map(|((_, id), has)| if *has { Some(*id) } else { None })
            .collect();
        self.write(|conn| {
            let tx = conn.unchecked_transaction()?;
            let mut st = tx.prepare_cached(
                "UPDATE photos SET
                   thumb_ok=1, dhash=?2, rich_state=?3,
                   width=COALESCE(?4, width), height=COALESCE(?5, height),
                   taken_utc=COALESCE(?6, taken_utc), rot=COALESCE(?7, rot),
                   lat=COALESCE(?8, lat), lon=COALESCE(?9, lon),
                   cam=NULL, cam_id=COALESCE(?10, cam_id)
                 WHERE id=?1",
            )?;
            for (i, (id, hash, d)) in rows.iter().enumerate() {
                st.execute(rusqlite::params![
                    id,
                    *hash as i64,
                    RICH_DONE,
                    d.and_then(|d| d.width),
                    d.and_then(|d| d.height),
                    d.and_then(|d| d.taken_utc.as_deref()),
                    d.map(|d| d.rot),
                    d.and_then(|d| d.lat),
                    d.and_then(|d| d.lon),
                    cam_ids[i],
                ])?;
            }
            drop(st);
            tx.commit()?;
            Ok(())
        })
    }

    /// 该档已生成的照片数(状态栏展示)。
    pub fn tier_count(&self, tier: u8) -> usize {
        if tier == 1 {
            self.read(|conn| {
                conn.query_row("SELECT COUNT(*) FROM photos WHERE thumb_ok=1", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap_or(0) as usize
            })
        } else {
            self.read(move |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM thumb_tiers WHERE tier=?1",
                    [tier as i64],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0) as usize
            })
        }
    }

    // ===== S5 派生作用域账本:dir_intent / bad_media / rich_state =====

    /// 读一条目录意图(未落账返回 None)。
    pub fn dir_intent(&self, path: &str) -> Option<DirIntent> {
        let p = norm_dir(path);
        self.read(move |conn| {
            conn.query_row(
                "SELECT path,volume,frn,state,source,since_usn,needs_refresh
                 FROM dir_intent WHERE path=?1",
                rusqlite::params![p],
                dir_intent_row,
            )
            .ok()
        })
    }

    /// 落账一条目录意图。
    ///
    /// 已存在则改 `state`/`source`;`volume`/`frn`/`since_usn` 只在给了新值时
    /// 覆盖(`COALESCE`),`needs_refresh` 由 [`Store::set_dir_needs_refresh`]
    /// 单独管 —— 纳入动作不该顺手抹掉一个还没处理的失效位。
    pub fn set_dir_intent(&self, d: &DirIntent) -> Result<()> {
        let d = d.clone();
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO dir_intent(path,volume,frn,state,source,since_usn,needs_refresh)
                 VALUES(?1,?2,?3,?4,?5,?6,0)
                 ON CONFLICT(path) DO UPDATE SET
                   volume=COALESCE(?2, dir_intent.volume),
                   frn=COALESCE(?3, dir_intent.frn),
                   state=?4, source=?5,
                   since_usn=COALESCE(?6, dir_intent.since_usn)",
                rusqlite::params![
                    norm_dir(&d.path),
                    d.volume,
                    d.frn,
                    d.state,
                    d.source,
                    d.since_usn,
                ],
            )?;
            Ok(())
        })
    }

    /// 批量落账(首启范围对话框一次写完整棵作用域根)。返回写入条数。
    pub fn set_dir_intents(&self, list: &[DirIntent]) -> Result<usize> {
        if list.is_empty() {
            return Ok(0);
        }
        let list: Vec<DirIntent> = list.to_vec();
        self.write(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let mut n = 0usize;
            {
                let mut st = tx.prepare_cached(
                    "INSERT INTO dir_intent(path,volume,frn,state,source,since_usn,needs_refresh)
                     VALUES(?1,?2,?3,?4,?5,?6,0)
                     ON CONFLICT(path) DO UPDATE SET
                       volume=COALESCE(?2, dir_intent.volume),
                       frn=COALESCE(?3, dir_intent.frn),
                       state=?4, source=?5,
                       since_usn=COALESCE(?6, dir_intent.since_usn)",
                )?;
                for d in &list {
                    n += st.execute(rusqlite::params![
                        norm_dir(&d.path),
                        d.volume,
                        d.frn,
                        d.state,
                        d.source,
                        d.since_usn,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(n)
        })
    }

    /// 所有待失效的目录(`needs_refresh=1`)。
    pub fn dirs_needing_refresh(&self) -> Vec<String> {
        self.read(|conn| {
            let mut st = match conn
                .prepare("SELECT path FROM dir_intent WHERE needs_refresh=1 ORDER BY path")
            {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            st.query_map([], |r| r.get::<_, String>(0))
                .ok()
                .map(|it| it.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        })
    }

    /// 清掉全部失效位(逐目录退回待派生之后调)。返回清掉的条数。
    pub fn clear_needs_refresh(&self) -> Result<usize> {
        self.write(|conn| {
            Ok(conn
                .execute(
                    "UPDATE dir_intent SET needs_refresh=0 WHERE needs_refresh<>0",
                    [],
                )
                .unwrap_or(0))
        })
    }

    /// 给账本上**所有**目录挂失效位。
    ///
    /// 两个触发点用它:索引 blob 格式版本不匹配(整库重建 → 目录账本全部作废),
    /// 以及用户在选项里勾「强制刷新缩略图」(`force_thumb_update` 偏好)。
    /// 只挂位,不立即改 `rich_state` —— 退回动作由 [`Store::refresh_pending_dirs`]
    /// 在启动路径上一次性做完,免得挂位的当下正好卡住别的事务。
    pub fn mark_all_dirs_needs_refresh(&self) -> Result<usize> {
        self.write(|conn| {
            Ok(conn
                .execute(
                    "UPDATE dir_intent SET needs_refresh=1 WHERE needs_refresh=0",
                    [],
                )
                .unwrap_or(0))
        })
    }

    /// 把挂了失效位的目录逐个退回待派生,然后清位。
    ///
    /// `invalidate_dir` 是"全部退回 1",含已派生(2)与坏文件(3)——
    /// 用户要求强制刷新时坏文件也该再试一次(台账本身不动,再失败会重新立案)。
    /// 返回退回的总行数。
    pub fn refresh_pending_dirs(&self) -> Result<usize> {
        let dirs = self.dirs_needing_refresh();
        if dirs.is_empty() {
            return Ok(0);
        }
        let mut total = 0usize;
        for d in &dirs {
            total += self.invalidate_dir(d).unwrap_or(0);
        }
        let _ = self.clear_needs_refresh();
        Ok(total)
    }

    /// 这个目录能不能写进派生作用域账本。
    ///
    /// 分两层:
    /// - **硬层**(4 条 + 回收站现代名 + 自家产出目录)任何入口都不放行;
    /// - **软层**(AppData / Program Files / temp / node_modules / 点开头目录 /
    ///   用户自定义排除)只在 `deliberate=true` —— 用户在文件夹管理器里选中它、
    ///   或对着占位格右键「纳入此目录」—— 才放行。
    ///
    /// 滚到过、屏幕上出现过一律算被动,不写账本(那条口径 11-05 已拍,保留)。
    /// 空路径同样不算可纳入 —— 调用方拿它的返回值决定要不要给回执。
    pub fn adoptable_dir(&self, dir: &str, deliberate: bool) -> bool {
        use crate::scan::ExcludeLayer;
        if dir.is_empty() {
            return false;
        }
        match crate::scan::Exclusions::from_store(self).exclude_layer_dir(Path::new(dir)) {
            Some(ExcludeLayer::Hard) => false,
            Some(ExcludeLayer::Soft) => deliberate,
            None => true,
        }
    }

    /// 点击懒加载:该目录子树里**未纳入**的行置为待派生。
    ///
    /// 只动 0 —— 已派生(2)不必重做,坏文件(3)由台账拦着,不该因为点开
    /// 一次目录就又开始每轮重试。子树口径与 [`ScopeMap`] 的最长前缀命中一致:
    /// 账本上写 `My Pictures` 就该覆盖 `My Pictures\2024`,否则那些行永远停在
    /// 未纳入。返回置行数。
    pub fn promote_dir(&self, dir: &str) -> Result<usize> {
        let (eq, sub) = dir_like(dir);
        self.write(move |conn| {
            Ok(conn
                .execute(
                    "UPDATE photos SET rich_state=?1
                     WHERE rich_state=0
                       AND path NOT IN (SELECT path FROM bad_media)
                       AND (parent LIKE ?2 ESCAPE '\\' OR parent LIKE ?3 ESCAPE '\\')",
                    rusqlite::params![RICH_PENDING, eq, sub],
                )
                .unwrap_or(0))
        })
    }

    /// 按张放行:只把屏幕上这几张缺缓存的照片抬进派生队列。
    ///
    /// 调用方已经判过磁盘:这一档缓存确实不在。所以台账说「已生成」也算数 ——
    /// 缓存文件可能被外部清掉,这时 `thumb_ok` 要一并退回 0,否则队列不会
    /// 重新生成,格子会一直停在「载入中」。dhash 保留:旧哈希让重复分组不散,
    /// 重派生完成时由 `mark_derived` 覆盖。坏文件不动,重复调用是幂等的。
    pub fn promote_photos(&self, ids: &[i64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let ids = ids.to_vec();
        self.write(move |conn| {
            let mut st = conn.prepare(
                "UPDATE photos SET rich_state=?1, thumb_ok=0
                 WHERE id=?2 AND rich_state<>3
                   AND upper(path) NOT IN (SELECT upper(path) FROM bad_media)",
            )?;
            let mut n = 0usize;
            for id in &ids {
                n += st.execute(rusqlite::params![RICH_PENDING, id])?;
            }
            Ok(n)
        })
    }

    /// 该目录子树里**已放行**的行(`rich_state<>0`),连同状态一起回读。
    ///
    /// `promote_dir` 改的是库,而网格每帧读的是内存里的 `PhotoRow`,那一列要等
    /// `rebuild` 才跟库对齐。界面于是必须拿这份回读当唯一真相 —— 若让它
    /// 自己按排除名单再判一遍门,结果库里早就派生完了,屏幕上还锁着
    /// 「未纳入范围」的占位块。取满 5 万行即止,剩下的下一轮重建补。
    pub fn promoted_states(&self, dir: &str) -> Vec<(i64, i32)> {
        let (eq, sub) = dir_like(dir);
        self.read(|conn| {
            let mut st = match conn.prepare(
                "SELECT id, rich_state FROM photos
                 WHERE rich_state<>?1
                   AND (parent LIKE ?2 ESCAPE '\\' OR parent LIKE ?3 ESCAPE '\\')
                 LIMIT 50000",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            st.query_map(rusqlite::params![RICH_OUT, eq, sub], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map(|it| it.filter_map(|x| x.ok()).collect())
            .unwrap_or_default()
        })
    }

    /// 目录级失效:该目录子树的行**全部**退回待派生,
    /// 含已派生(2)与坏文件(3)—— 用户要求强制刷新时,坏文件也该再试一次
    /// (台账本身不动,重试再失败会重新立案)。返回置行数。
    pub fn invalidate_dir(&self, dir: &str) -> Result<usize> {
        let (eq, sub) = dir_like(dir);
        self.write(move |conn| {
            Ok(conn
                .execute(
                    "UPDATE photos SET rich_state=?1
                     WHERE rich_state<>?1
                       AND (parent LIKE ?2 ESCAPE '\\' OR parent LIKE ?3 ESCAPE '\\')",
                    rusqlite::params![RICH_PENDING, eq, sub],
                )
                .unwrap_or(0))
        })
    }

    /// 批量按路径取派生状态;不在库里的路径**不出现在结果里**
    /// (调用方据此区分「未纳入作用域」与「压根不是媒体/还没落库」)。
    ///
    /// `photos.path` 是 `TEXT UNIQUE`,SQLite 自带唯一索引,所以一次 IN 查询
    /// 就是逐条走索引 —— 快查投影只取 top-N(≤5000,远低于 SQLite 的
    /// 变量上限),不需要分批。
    pub fn rich_states_by_paths(&self, paths: &[String]) -> HashMap<String, i32> {
        if paths.is_empty() {
            return HashMap::new();
        }
        let sql = format!(
            "SELECT path, rich_state FROM photos WHERE path IN ({})",
            vec!["?"; paths.len()].join(",")
        );
        self.read(|conn| {
            let mut st = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(_) => return HashMap::new(),
            };
            let rows = match st.query_map(rusqlite::params_from_iter(paths.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i32>(1)?))
            }) {
                Ok(it) => it,
                Err(_) => return HashMap::new(),
            };
            rows.filter_map(|r| r.ok()).collect()
        })
    }

    /// 按路径精确取图库行 id(快速查找页双击进预览用)。
    ///
    /// 路径口径与 [`Store::rich_states_by_paths`] 一致:快速查找回的路径本就
    /// 直接拿来做 IN 匹配,这里同样原样等值查询,不做分隔符归一。
    /// `photos.path` 唯一索引,单条等值查询即索引点查。
    pub fn photo_id_at(&self, path: &str) -> Option<i64> {
        self.read(|conn| {
            conn.query_row("SELECT id FROM photos WHERE path=?1", [path], |r| r.get(0))
                .ok()
        })
    }

    /// 按 id 取右键菜单要用的行字段(星标 / 媒体类型)。
    ///
    /// 快速查找页把图库右键菜单借给已入库的媒体命中:菜单要显示
    /// 「加星标还是取消星标」「预览还是播放」,这两样只认库里的行。
    /// 路径与父目录由调用方自带(命中本来就是全路径),不必回查。
    pub fn photo_brief_by_id(&self, id: i64) -> Option<(i64, i64)> {
        self.read(|conn| {
            conn.query_row("SELECT star, kind FROM photos WHERE id=?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .ok()
        })
    }

    /// 拿账本复核一遍库里的行:把常用目录 / 用户导入目录里还没排队的抬进队列。
    ///
    /// 只做「升」,不再做「降」。派生状态只是排队进度,不是显示开关:
    /// 目录移出批量计划,不代表它的缓存要作废、更不代表正在排队的几张要被踢出去
    /// (按张放行随时会往队列里放行几张,降分支会和它抢,把刚放行的又打回 0)。
    /// 真正的排除由硬排除名单在入库层完成 —— 那些目录根本不进 `photos` 表;
    /// 文件被删掉由 [`Store::delete_l1_frn64`] 直接删行。
    ///
    /// 少了「升」这一刀,后来才纳入的目录要等文件 mtime 变一次才重算,
    /// 用户看到的就是「导入完了照片永远是占位块」。
    /// 判定走 [`ScopeGate::dir_derivable`],所以「祖先被纳入但本级命中排除」
    /// 的目录不会被抬进门。返回变动的行数。
    pub fn reconcile_rich_state(&self, gate: &ScopeGate) -> Result<usize> {
        let mut promote: Vec<String> = Vec::new();
        self.read(|conn| -> Result<()> {
            let mut st = conn.prepare("SELECT DISTINCT parent FROM photos WHERE rich_state=0")?;
            for r in st.query_map([], |r| r.get::<_, String>(0))? {
                if let Ok(d) = r {
                    if gate.dir_derivable(&d) {
                        promote.push(d);
                    }
                }
            }
            Ok(())
        })?;
        if promote.is_empty() {
            return Ok(0);
        }
        self.write(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let mut n = 0usize;
            {
                // 升:台账里的行回 3(仍在案,不出队);已有缓存的历史行直接
                // 记成已完成,不再排一次队重做;其余回 1。
                let mut up_bad = tx.prepare_cached(
                    "UPDATE photos SET rich_state=3 WHERE rich_state=0 AND parent=?1 \
                     AND upper(path) IN (SELECT upper(path) FROM bad_media)",
                )?;
                let mut up_done = tx.prepare_cached(
                    "UPDATE photos SET rich_state=2 WHERE rich_state=0 AND parent=?1 \
                     AND thumb_ok=1 \
                     AND upper(path) NOT IN (SELECT upper(path) FROM bad_media)",
                )?;
                let mut up = tx.prepare_cached(
                    "UPDATE photos SET rich_state=1 WHERE rich_state=0 AND parent=?1 \
                     AND thumb_ok=0 \
                     AND upper(path) NOT IN (SELECT upper(path) FROM bad_media)",
                )?;
                for d in &promote {
                    n += up_bad.execute(rusqlite::params![d])?;
                    n += up_done.execute(rusqlite::params![d])?;
                    n += up.execute(rusqlite::params![d])?;
                }
            }
            tx.commit()?;
            Ok(n)
        })
    }

    /// 坏媒体台账全表(网格右键的「隐藏 / 不隐藏」出口要列它)。
    pub fn bad_media(&self) -> Vec<BadMedia> {
        self.read(|conn| {
            let mut st = match conn
                .prepare("SELECT path,reason,since FROM bad_media ORDER BY since DESC, path")
            {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            st.query_map([], |r| {
                Ok(BadMedia {
                    path: r.get(0)?,
                    reason: r.get(1)?,
                    since: r.get(2)?,
                })
            })
            .ok()
            .map(|it| it.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        })
    }

    /// 台账路径集合(大写归一化)—— [`ScopeGate`] 的第三层跳过用。
    pub fn bad_media_upper(&self) -> std::collections::HashSet<String> {
        self.bad_media()
            .into_iter()
            .map(|b| b.path.to_ascii_uppercase())
            .collect()
    }

    /// 立案:写台账 + 该行 `rich_state` 1→3,不再每轮重试。
    ///
    /// 台账按 path 记;文件改名后旧条目会失去
    /// 目标,那时它拦不住任何东西,只是一条待清理的死账。
    pub fn mark_bad_media(&self, path: &str, reason: &str) -> Result<()> {
        let p = path.to_string();
        let why = reason.to_string();
        let now = chrono::Utc::now().timestamp_millis();
        self.write(move |conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO bad_media(path,reason,since) VALUES(?1,?2,?3)
                 ON CONFLICT(path) DO UPDATE SET reason=?2, since=?3",
                rusqlite::params![p, why, now],
            )?;
            tx.execute(
                "UPDATE photos SET rich_state=?2 WHERE path=?1 AND rich_state=?3",
                rusqlite::params![p, RICH_BAD, RICH_PENDING],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// 销案(「不隐藏」出口):删台账 + 该行 `rich_state` 3→1 重新排队。
    pub fn unmark_bad_media(&self, path: &str) -> Result<()> {
        self.unmark_bad_media_batch([path])?;
        Ok(())
    }

    /// 批量销案:台账行删掉、`rich_state=3` 的行放回待派生、失败计数清零。
    /// 返回真正被放回队列的行数。
    ///
    /// 单事务而不是一条一个事务:整批复盘(比如「误判坏图一次性销案」)动辄
    /// 几千条,WAL 下每次提交都要落盘,逐条写会把一次后台维护拖成几十秒。
    pub fn unmark_bad_media_batch(
        &self,
        paths: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<usize> {
        let paths: Vec<String> = paths.into_iter().map(|p| p.as_ref().to_string()).collect();
        if paths.is_empty() {
            return Ok(0);
        }
        self.write(move |conn| {
            let tx = conn.unchecked_transaction()?;
            let mut n = 0usize;
            {
                let mut del = tx.prepare_cached("DELETE FROM bad_media WHERE path=?1")?;
                let mut requeue = tx.prepare_cached(
                    "UPDATE photos SET rich_state=?2, thumb_fail=0 WHERE path=?1 AND rich_state=?3",
                )?;
                for p in &paths {
                    del.execute(rusqlite::params![p])?;
                    n += requeue.execute(rusqlite::params![p, RICH_PENDING, RICH_BAD])?;
                }
            }
            tx.commit()?;
            Ok::<usize, anyhow::Error>(n)
        })
    }

    /// 该路径是否已在坏媒体台账里。
    pub fn is_bad_media(&self, path: &str) -> bool {
        let p = path.to_string();
        self.read(move |conn| {
            conn.query_row(
                "SELECT 1 FROM bad_media WHERE path=?1",
                rusqlite::params![p],
                |_| Ok(()),
            )
            .is_ok()
        })
    }
}

/// `dir_intent` 一行 → [`DirIntent`](列序与 SELECT 一致)。
fn dir_intent_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DirIntent> {
    Ok(DirIntent {
        path: r.get(0)?,
        volume: r.get(1)?,
        frn: r.get(2)?,
        state: r.get(3)?,
        source: r.get(4)?,
        since_usn: r.get(5)?,
        needs_refresh: r.get::<_, i64>(6)? != 0,
    })
}

impl Drop for Store {
    fn drop(&mut self) {
        // 关队列 → 写线程把剩余任务跑完退出 → join 等它,不丢尾部写。
        self.write_tx.lock().unwrap().take();
        if let Some(h) = self.write_done.lock().unwrap().take() {
            let _ = h.join();
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
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// 目录子树匹配用的两条 LIKE 模式:(直属该目录, 该目录子树)。
///
/// 一律走 LIKE 而不是 `=`:`photos.parent` 是普通 TEXT,`=` 大小写敏感,
/// 而 L1 的名字取自 MFT、账本里的路径可能来自 Shell API 或用户输入,
/// 大小写不保证一致;SQLite 的 LIKE 对 ASCII 天然折叠大小写。
///
/// 转义口径:`ESCAPE '\'` 下字面反斜杠要写两遍,而结尾那个 `%` 必须**裸着**
/// 才是通配符。所以子树模式 = 转义后的目录 + 转义后的分隔符 + 裸 `%`;
/// 卷根要先去掉结尾分隔符,否则会多转义出一个字面反斜杠。
fn dir_like(dir: &str) -> (String, String) {
    let d = norm_dir(dir);
    let base = d.trim_end_matches(['\\', '/']);
    // 子树通配的分隔符跟着路径自己的写法走:POSIX 上 `photos.parent` 只有 `/`,
    // 硬编码反斜杠会让整棵子树一条也匹配不到(Linux 端 promote 全 0)。
    let sep = if d.contains('/') && !d.contains('\\') {
        "/"
    } else {
        "\\\\"
    };
    (like_escape(&d), format!("{}{}%", like_escape(base), sep))
}

/// 扫描根目录候选(按盘符枚举)。
///
/// 默认扫描范围:用户壳目录(我的文档 / 图片 / 桌面)。
///
/// 首启口径 —— 安装文案原文
/// "Only search My Documents, My Pictures, and the Desktop":
/// 首启只登记这三个壳目录并扫描一次;扩大范围由用户在文件夹管理器手动添加。
/// **绝不返回盘根** —— 盘根会把系统 / 应用数据目录整卷拖进库。
pub fn default_roots() -> Vec<PathBuf> {
    crate::win::known_dirs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_template_ships_official_defaults() {
        // 模板里的两行要和代码内置的默认常量一致,否则「看配置」和
        // 「没配置文件时的实际行为」会给出不同地址。
        assert!(SETTINGS_TEMPLATE.contains(&format!("UpdateURL={DEFAULT_UPDATE_URL}")));
        assert!(SETTINGS_TEMPLATE.contains(&format!("ModelURL={DEFAULT_MODEL_URL}")));
        let kv = crate::update::parse_kv(SETTINGS_TEMPLATE);
        assert_eq!(kv.get("updateurl").unwrap(), DEFAULT_UPDATE_URL);
        assert_eq!(kv.get("modelurl").unwrap(), DEFAULT_MODEL_URL);
    }

    #[test]
    fn rewrite_marker_preserves_update_section() {
        // 统一格式:替换 data_dir 两行,注释与 [update] 节原样保留。
        let src = "# 头注释\n[location]\ndata_dir=C:\\old\n\n[update]\nUpdateURL=http://a/latest.ini\nModelURL=http://a/models/\n";
        let out = rewrite_marker_text(
            src,
            Some("data_dir=D:\\new".into()),
            Some("pending_from=C:\\old".into()),
        );
        assert!(out.contains("data_dir=D:\\new"));
        assert!(out.contains("pending_from=C:\\old"));
        assert!(out.contains("[update]"));
        assert!(out.contains("UpdateURL=http://a/latest.ini"));
        assert!(out.contains("ModelURL=http://a/models/"));
        assert!(!out.contains("data_dir=C:\\old"));
        // 旧 marker(无节结构、纯键值):旧行滤净后重建 [location] 节。
        let out = rewrite_marker_text(
            "data_dir=D:\\maliang\npending_from=D:\\orig\n",
            Some("data_dir=E:\\elsewhere".into()),
            None,
        );
        assert!(out.contains("data_dir=E:\\elsewhere"));
        assert!(!out.contains("pending_from="));
        assert_eq!(out.matches("data_dir=").count(), 1);
        // 空底本(首启):生成最小 [location] 节。
        let out = rewrite_marker_text("", Some("data_dir=D:\\x".into()), None);
        assert_eq!(out, "[location]\ndata_dir=D:\\x\n");
    }

    #[test]
    fn parse_location_reads_both_lines_and_ignores_junk() {
        let (d, p) = parse_location("data_dir=D:\\mliang\npending_from=C:\\Users\\x\\AppData\n");
        assert_eq!(d, Some(PathBuf::from("D:\\mliang")));
        assert_eq!(p, Some(PathBuf::from("C:\\Users\\x\\AppData")));
        let (d, p) = parse_location("data_dir=\n垃圾行\ndata_dir=E:\\only\n");
        assert_eq!(d, Some(PathBuf::from("E:\\only")), "空行忽略,最后一行生效");
        assert_eq!(p, None);
        let (d, p) = parse_location("");
        assert_eq!((d, p), (None, None));
    }

    #[test]
    fn copy_missing_copies_only_absent_files_skips_transients() {
        let base = std::env::temp_dir().join(format!("rk-migr-{}", std::process::id()));
        let src = base.join("src");
        let dst = base.join("dst");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("index.db"), b"AAA").unwrap();
        std::fs::write(src.join("sub").join("nameindex.bin"), b"BBB").unwrap();
        std::fs::write(src.join("maliang.ini"), b"[location]\ndata_dir=X").unwrap();
        std::fs::write(src.join("data.location"), b"data_dir=X").unwrap();
        std::fs::write(src.join("mft_report.txt"), b"r").unwrap();
        std::fs::write(src.join("junk.tmp"), b"t").unwrap();
        // 目标已有同名文件 → 不覆盖。
        std::fs::write(dst.join("index.db"), b"OLD").unwrap();
        assert!(copy_missing_recursive(&src, &dst));
        assert_eq!(std::fs::read(dst.join("index.db")).unwrap(), b"OLD");
        assert_eq!(
            std::fs::read(dst.join("sub").join("nameindex.bin")).unwrap(),
            b"BBB"
        );
        assert!(!dst.join("maliang.ini").exists(), "统一配置不随迁");
        assert!(!dst.join("data.location").exists(), "旧 marker 不随迁");
        assert!(!dst.join("mft_report.txt").exists());
        assert!(!dst.join("junk.tmp").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 缓存目录解析:数据目录被 `data.location` 迁走时缓存跟到大盘
    /// (缺陷:库在 D:,缩略图却默默写满 C:)。
    #[test]
    fn cache_root_follows_relocated_data_dir() {
        let sys = Path::new("C:\\Users\\x\\AppData\\Local\\Relyto\\Maliang");
        // 没迁库:保持历史落点,既有缓存不失效。
        assert_eq!(cache_root_for(None, sys), sys.to_path_buf());
        // 迁了库:落到数据目录下的 cache 子目录,与 models/Incoming 同级。
        assert_eq!(
            cache_root_for(Some(Path::new("D:\\maliang")), sys),
            PathBuf::from("D:\\maliang").join(CACHE_SUBDIR)
        );
    }

    /// 缓存落点必须按**生效**数据目录判定,不能只认用户级标记:
    /// 机器级标记(装服务时写下的)、服务形态默认目录(ProgramData)、
    /// 环境变量覆盖,这几条来源都要跟随。缺一条就会出现「库在 D:、
    /// 缩略图却继续写满 C:」—— 而界面还写着会自动跟随。
    #[test]
    fn cache_root_follows_every_effective_data_dir_source() {
        let sys = Path::new("C:\\Users\\x\\AppData\\Local\\Relyto\\Maliang");
        let dflt = Path::new("C:\\Users\\x\\AppData\\Roaming\\Relyto\\Maliang");
        // 没迁走:沿用历史落点,既有缓存不失效。
        assert_eq!(cache_root_for_data_dir(dflt, dflt, sys), sys.to_path_buf());
        // 用户级标记 / 机器级标记 / 服务形态默认值 / 任意显式目录,
        // 只要偏离默认就跟随到 <数据目录>/cache。
        let sources = [
            resolve_data_dir(Some(PathBuf::from("D:\\user")), None, false),
            resolve_data_dir(None, Some(PathBuf::from("D:\\machine")), false),
            machine_default_dir(),
            PathBuf::from("D:\\env_override"),
        ];
        for d in sources {
            assert_ne!(d.as_path(), dflt, "样本本身必须代表「已迁走」: {d:?}");
            assert_eq!(
                cache_root_for_data_dir(&d, dflt, sys),
                d.join(CACHE_SUBDIR),
                "{d:?} 这条来源的缓存必须跟随"
            );
        }
    }

    /// 档位目录名写死在 db,反向核对 thumbs 模块,防止加档漏登记。
    #[test]
    fn cache_tier_dirs_match_thumbs_module() {
        let src = include_str!("thumbs.rs");
        for name in CACHE_TIER_DIRS {
            assert!(
                src.contains(&format!("\"{name}\"")),
                "{name} 没在 thumbs.rs 的 dir_name 里出现,自动搬迁会漏这一档"
            );
        }
    }

    /// 自动搬迁只动四个档位目录:历史重置备份与顶层目录原样保留,
    /// 不替用户静默删除体量相当的历史数据;目标已有同名文件不覆盖。
    #[test]
    fn auto_cache_relocation_moves_tiers_only() {
        let base = std::env::temp_dir().join(format!("rk-auto-cache-{}", std::process::id()));
        let from = base.join("old");
        let to = base.join("new");
        let _ = std::fs::remove_dir_all(&base);
        for d in ["thumbs", "smallthumbs", "thumbs.reset-20261107"] {
            std::fs::create_dir_all(from.join(d)).unwrap();
        }
        std::fs::write(from.join("thumbs").join("1.jpg"), b"A").unwrap();
        std::fs::write(from.join("smallthumbs").join("2.jpg"), b"B").unwrap();
        std::fs::write(from.join("thumbs.reset-20261107").join("9.jpg"), b"OLD").unwrap();

        let (moved, ok) = relocate_tier_caches(&from, &to);
        assert_eq!(moved, 2, "只搬两档: {moved}");
        assert!(ok);
        assert_eq!(
            std::fs::read(to.join("thumbs").join("1.jpg")).unwrap(),
            b"A"
        );
        assert_eq!(
            std::fs::read(to.join("smallthumbs").join("2.jpg")).unwrap(),
            b"B"
        );
        assert!(!from.join("thumbs").exists(), "搬完清空该档来源");
        assert!(
            from.join("thumbs.reset-20261107").join("9.jpg").exists(),
            "历史备份保留原处"
        );
        assert!(from.exists(), "顶层目录不删");

        // 目标已有同名文件不覆盖。
        std::fs::create_dir_all(from.join("bigthumbs")).unwrap();
        std::fs::write(from.join("bigthumbs").join("3.jpg"), b"OLD3").unwrap();
        std::fs::create_dir_all(to.join("bigthumbs")).unwrap();
        std::fs::write(to.join("bigthumbs").join("3.jpg"), b"KEPT").unwrap();
        let (moved2, ok2) = relocate_tier_caches(&from, &to);
        assert_eq!(moved2, 0);
        assert!(ok2);
        assert_eq!(
            std::fs::read(to.join("bigthumbs").join("3.jpg")).unwrap(),
            b"KEPT"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 数据目录优先级:用户标记 > 机器标记 > 按进程形态的默认目录。
    /// 钉住的是服务与界面必须用同一份配置的口径 —— 服务(LocalSystem)读不到
    /// 登录用户的标记,必须能从机器级标记拿到同一个目录,而不是各建一套库。
    #[test]
    fn data_dir_prefers_user_marker_then_machine_marker() {
        let user = PathBuf::from("D:\\maliang");
        let machine = PathBuf::from("E:\\svc");
        assert_eq!(
            resolve_data_dir(Some(user.clone()), Some(machine.clone()), true),
            user,
            "登录用户迁过目录时,服务也跟着迁"
        );
        assert_eq!(
            resolve_data_dir(None, Some(machine.clone()), true),
            machine,
            "用户标记读不到(服务正是这样)时用机器级标记"
        );
        // 两个标记都没有:服务落机器级,普通进程落自己的 AppData。
        assert_eq!(resolve_data_dir(None, None, true), machine_default_dir());
        assert_eq!(resolve_data_dir(None, None, false), default_data_dir());
        #[cfg(windows)]
        {
            // 钉住「与账号无关」:服务绝不能落进 systemprofile(那是 LocalSystem
            // 的「用户目录」),也不能和用户级默认值撞成同一个目录。
            let root = std::env::var_os("PROGRAMDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
            assert_eq!(
                machine_default_dir(),
                root.join("Relyto").join("Maliang"),
                "机器级目录必须挂在 ProgramData 下"
            );
            assert_ne!(
                machine_default_dir(),
                default_data_dir(),
                "机器级默认值不能等于用户 AppData,否则服务照样各建一套库"
            );
        }
    }

    /// 缓存搬运:只补缺、不覆盖、瞬态文件不跟,成功后清来源。
    #[test]
    fn migrate_cache_to_keeps_existing_and_clears_source() {
        let base = std::env::temp_dir().join(format!("rk-cache-migr-{}", std::process::id()));
        let from = base.join("old");
        let to = base.join("new");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(from.join("smallthumbs")).unwrap();
        std::fs::create_dir_all(from.join("thumbs")).unwrap();
        std::fs::create_dir_all(to.join("thumbs")).unwrap();
        std::fs::write(from.join("smallthumbs").join("1.jpg"), b"NEW").unwrap();
        std::fs::write(from.join("thumbs").join("2.jpg"), b"OLD").unwrap();
        std::fs::write(from.join("junk.tmp"), b"t").unwrap();
        // 目标已有同档同名文件 → 不覆盖(目标侧是更新的那份)。
        std::fs::write(to.join("thumbs").join("2.jpg"), b"KEPT").unwrap();

        let (moved, cleared) = migrate_cache_to(&from, &to).unwrap();
        assert_eq!(moved, 1, "只有缺失的一档被搬: {moved}");
        assert_eq!(
            std::fs::read(to.join("thumbs").join("2.jpg")).unwrap(),
            b"KEPT"
        );
        assert_eq!(
            std::fs::read(to.join("smallthumbs").join("1.jpg")).unwrap(),
            b"NEW"
        );
        assert!(cleared, "搬完来源应清空");
        assert!(!from.exists());
        assert!(!to.join("junk.tmp").exists(), "瞬态文件不跟迁");

        // 同目录再点一次:空操作,绝不把自己搬进自己再删掉。
        let (moved2, _) = migrate_cache_to(&to, &to).unwrap();
        assert_eq!(moved2, 0);
        assert!(
            to.join("thumbs").join("2.jpg").exists(),
            "自迁移不能删掉目标"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 新库:派生记账的两张表 + 两列建齐,默认值落对。
    #[test]
    fn derivation_ledger_tables_and_columns_exist() {
        let store = Store::open_mem().unwrap();
        let c = store.test_conn();
        c.execute(
            "INSERT INTO dir_intent(path,volume,frn,state,source,since_usn)
             VALUES('D:\\Photos','D:\\',12345,'imported','import',987654)",
            [],
        )
        .unwrap();
        let (state, source, needs): (String, String, i64) = c
            .query_row(
                "SELECT state,source,needs_refresh FROM dir_intent WHERE path='D:\\Photos'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((state.as_str(), source.as_str()), ("imported", "import"));
        assert_eq!(needs, 0, "新建账本默认不需要刷新");

        c.execute(
            "INSERT INTO bad_media(path,reason,since) VALUES('D:\\bad.jpg','decode',1)",
            [],
        )
        .unwrap();
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM bad_media", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );

        // 派生队列按 rich_state 取行、快查按 ext 过滤 —— 两者都建在迁移列上,
        // 索引必须存在(否则 S5 起每轮回填都是全表扫)。
        for idx in [
            "idx_photos_rich_state",
            "idx_photos_ext",
            "idx_dir_intent_state",
        ] {
            let n: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                    [idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "缺索引:{idx}");
        }
    }

    /// 批量按路径取派生状态(快查投影用)。
    ///
    /// 钉两件事:**不在库里的路径不出现在结果里**(调用方靠这个把
    /// 「未纳入作用域」和「压根不是媒体」分开),以及空输入不生成
    /// `IN ()` 这种非法 SQL。
    #[test]
    fn rich_states_by_paths_only_returns_rows_that_exist() {
        let store = Store::open_mem().unwrap();
        {
            let c = store.test_conn();
            c.execute_batch(
                "INSERT INTO photos(path,parent,mtime,size,rich_state) VALUES
                    ('D:/Pictures/a.jpg','D:/Pictures',0,1,0),
                    ('D:/Pictures/b.jpg','D:/Pictures',0,2,2),
                    ('D:/Pictures/c.jpg','D:/Pictures',0,3,3);",
            )
            .unwrap();
        }
        let q = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        let got = store.rich_states_by_paths(&q(&[
            "D:/Pictures/a.jpg",
            "D:/Pictures/c.jpg",
            "D:/Pictures/nope.jpg",
        ]));
        assert_eq!(got.len(), 2, "不存在的路径不该出现在结果里");
        assert_eq!(got["D:/Pictures/a.jpg"], RICH_OUT);
        assert_eq!(got["D:/Pictures/c.jpg"], RICH_BAD);
        assert!(!got.contains_key("D:/Pictures/nope.jpg"));
        // b.jpg 没问就不回:结果只覆盖请求里的路径。
        assert!(!got.contains_key("D:/Pictures/b.jpg"));
        assert_eq!(
            store.rich_states_by_paths(&q(&["D:/Pictures/b.jpg"]))["D:/Pictures/b.jpg"],
            RICH_DONE
        );
        assert!(store.rich_states_by_paths(&[]).is_empty());
    }

    /// 固化两件事 —— 迁移列的索引排在 ALTER 之后建(写在 SCHEMA 里会直接失败),
    /// 以及已有缩略图的旧行被认作「已派生」。
    #[test]
    fn legacy_db_upgrades_rich_state_and_ext() {
        let conn = Connection::open_in_memory().unwrap();
        // 之前的形状:无 uid64/cam_id/kind/thumb_fail/caption/rich_state/ext。
        conn.execute_batch(
            "CREATE TABLE photos (
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
                thumb_ok INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO photos(id,path,parent,mtime,size,thumb_ok) VALUES
                (1,'D:/Pictures/a.JPG','D:/Pictures',0,1,1),
                (2,'D:/Pictures/b.png','D:/Pictures',0,2,0),
                (3,'D:/Videos/c','D:/Videos',0,3,0);",
        )
        .unwrap();
        let store = Store::from_conn(conn).unwrap();
        let c = store.test_conn();
        let rows: Vec<(String, i64, Option<String>)> = {
            let mut st = c
                .prepare("SELECT path,rich_state,ext FROM photos ORDER BY id")
                .unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        assert_eq!(
            rows,
            vec![
                // 已有缩略图 → 播种成「已派生」;扩展名统一小写。
                ("D:/Pictures/a.JPG".into(), 2, Some("jpg".into())),
                ("D:/Pictures/b.png".into(), 0, Some("png".into())),
                // 无扩展名留 NULL(而不是空串)。
                ("D:/Videos/c".into(), 0, None),
            ]
        );
        // 播种只跑一次:之后 rich_state 归派生门控管,不能再被这里回写。
        c.execute("UPDATE photos SET rich_state=1 WHERE id=1", [])
            .unwrap();
        assert_eq!(store.seed_rich_state_once().unwrap(), 0);
        let still: i64 = c
            .query_row("SELECT rich_state FROM photos WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(still, 1, "播种标记已置,二次调用不得回写");
    }

    #[test]
    fn ext_of_lowercases_and_none_without_extension() {
        assert_eq!(Store::ext_of("D:\\a\\PICS.JpG").as_deref(), Some("jpg"));
        assert_eq!(Store::ext_of("D:\\a\\no-ext").as_deref(), None);
        assert_eq!(Store::ext_of("D:\\a.tar.gz").as_deref(), Some("gz"));
    }

    #[test]
    fn remove_tree_best_effort_deletes_all_and_survives_locked_file() {
        let base = std::env::temp_dir().join(format!("rk-rmbe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sub = base.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(base.join("a.db"), b"A").unwrap();
        std::fs::write(sub.join("b.bin"), b"B").unwrap();
        // Windows 上只读文件删除会被拒 — 正好验证 best-effort 不中断不 panic。
        let ro = base.join("locked.txt");
        std::fs::write(&ro, b"R").unwrap();
        let mut perm = std::fs::metadata(&ro).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&ro, perm).unwrap();
        remove_tree_best_effort(&base);
        assert!(!base.join("a.db").exists(), "普通文件应删掉");
        assert!(!sub.exists(), "子目录应删掉");
        if ro.exists() {
            let mut perm = std::fs::metadata(&ro).unwrap().permissions();
            perm.set_readonly(false);
            std::fs::set_permissions(&ro, perm).unwrap();
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 默认扫描范围是壳目录(我的文档/图片/桌面),
    /// 绝不返回盘根 —— 盘根会把系统/应用数据目录整卷拖进库。
    #[test]
    fn default_roots_are_shell_dirs_never_drive_roots() {
        for p in default_roots() {
            assert!(p.exists(), "默认根不存在:{p:?}");
            assert!(p.parent().is_some(), "默认根不允许是盘根/卷根:{p:?}");
        }
    }

    /// 首启行为:范围只落账一次,之后由用户在文件夹管理器自管。
    #[test]
    fn first_run_scope_seeds_once() {
        let store = Store::open_mem().unwrap();
        assert!(store.watched_stats().is_empty());
        assert!(!store.scope_initialized());
        // 保守档:派生作用域 = 三个壳目录(不含视频),监听根也是它们。
        let roots = store.apply_initial_scope(false);
        assert_eq!(roots, default_roots(), "首扫根应等于落账的作用域根");
        assert!(!store.watched_stats().is_empty());
        assert!(store.scope_initialized());
        assert_eq!(
            store.get_setting("initial_scope").as_deref(),
            Some("limited")
        );
        let gate = ScopeGate::load(&store);
        assert!(!gate.is_empty(), "落账后门不该再是空的");
        // 第二次调用是空操作(已初始化)。
        assert!(store.apply_initial_scope(true).is_empty());
        assert_eq!(
            store.get_setting("initial_scope").as_deref(),
            Some("limited")
        );
    }

    /// 全盘档:每个可扫卷根各落一条意图,靠最长前缀命中覆盖卷内任意子目录;
    /// 但监听根仍只有壳目录(整卷挂监听会被系统临时文件刷爆)。
    #[test]
    fn initial_scope_complete_ledgers_volume_roots_only() {
        let store = Store::open_mem().unwrap();
        let roots = store.apply_initial_scope(true);
        assert_eq!(
            store.get_setting("initial_scope").as_deref(),
            Some("complete")
        );
        for r in &roots {
            let s = r.to_string_lossy().into_owned();
            assert_eq!(
                store.dir_intent(&s).map(|d| d.state),
                Some(INTENT_DEFAULT_ROOT.to_string()),
                "卷根 {s} 应已落账"
            );
            assert_eq!(
                store.dir_intent(&s).map(|d| d.source),
                Some(SRC_INITIAL_DIALOG.to_string())
            );
        }
        if let Some(v) = roots.first() {
            let deep = v.join("Users").join("me").join("Pictures").join("2026");
            let gate = ScopeGate::load(&store);
            assert!(
                gate.dir_derivable(&deep.to_string_lossy()),
                "卷内任意深度都应被卷根那条意图覆盖:{deep:?}"
            );
        }
        // 监听根不跟着变:仍是壳目录那一组。
        let watched: Vec<String> = store.watched_stats().into_iter().map(|(p, _)| p).collect();
        assert_eq!(watched.len(), default_roots().len());
    }

    /// 排除名单分两层:软层能被用户表态覆盖(AppData 也要能
    /// 访问),硬层谁都不能、而且不进库。这条口径是 `adoptable_dir` /
    /// `dir_derivable` / `purge_hard_excluded_once` 共用的那一份判定,
    /// 测试打在门口,免得 UI 与入库各抄一遍。
    #[cfg(windows)]
    #[test]
    fn adoptable_dir_splits_hard_and_soft_excludes() {
        let store = Store::open_mem().unwrap();
        assert!(
            store.adoptable_dir("D:\\4.Photos\\coco", false),
            "普通照片目录该放行"
        );
        assert!(!store.adoptable_dir("", true), "空路径不是一条目录");

        // 硬层:表态也打不开。
        for d in [
            "D:\\$RECYCLE.BIN\\S-1-5-21-1",
            "D:\\maliang\\cache\\thumbs",
            "E:\\photos\\Originals",
            "C:\\Windows\\Web\\Wallpaper",
            "C:\\Program Files (x86)\\someapp\\res",
            "E:\\proj\\node_modules\\pkg",
            "C:\\Users\\me\\.cache\\pkg",
        ] {
            assert!(!store.adoptable_dir(d, false), "{d} 被动该拦住");
            assert!(!store.adoptable_dir(d, true), "{d} 属于硬层,表态也不放行");
        }

        // 软层:被动拦住、表态放行。`temp` 在这一层 —— 微信把收藏图片放在
        // `xwechat_files\...\favorite\temp` 里,不能一票否决到没有出口。
        store.set_setting("exclude_dirs", "mytmp").unwrap();
        for d in [
            "C:\\Users\\me\\AppData\\Roaming\\kingsoft\\kaccountsdk",
            "C:\\Users\\me\\AppData\\Local\\npm-cache\\x",
            "D:\\xwechat_files\\me\\business\\favorite\\temp",
            "D:\\maliang-archive-20261105\\local-cache",
            "D:\\mytmp\\sub",
        ] {
            assert!(!store.adoptable_dir(d, false), "{d} 不该因为被看到就进账本");
            assert!(store.adoptable_dir(d, true), "{d} 属于软层,用户表态要放行");
        }
    }

    /// 同一套分层在 POSIX 路径上的对偶:分隔符是 `/`、没有盘符,分层只认名字与 own-prefix。
    #[cfg(not(windows))]
    #[test]
    fn adoptable_dir_splits_hard_and_soft_excludes_posix() {
        let store = Store::open_mem().unwrap();
        assert!(
            store.adoptable_dir("/home/me/Photos/coco", false),
            "普通照片目录该放行"
        );
        assert!(!store.adoptable_dir("", true), "空路径不是一条目录");

        // 硬排除:点开头目录 / 同名目录 / 我们的系统目录 —— deliberate 也救不回来。
        for d in [
            "/home/me/.cache/pkg",
            "/mnt/win/$RECYCLE.BIN/S-1-5-21-1",
            "/mnt/win/Windows/Web/Wallpaper",
            "/mnt/win/Program Files (x86)/someapp/res",
            "/srv/proj/node_modules/pkg",
            "/home/me/Pictures/Originals",
        ] {
            assert!(!store.adoptable_dir(d, false), "{d} 属于硬排除");
            assert!(
                !store.adoptable_dir(d, true),
                "{d} 是硬排除,明确点名也不放行"
            );
        }
        let own = cache_dir_loc().join("thumbs").join("a.jpg");
        assert!(
            !store.adoptable_dir(&own.to_string_lossy(), true),
            "自家缓存(由 MALIANG_CACHE_DIR / data.location 推导)整棵子树硬排除",
        );

        // 软排除:名字规则(含用户自己加的)—— 只有明确点名才放行。
        store.set_setting("exclude_dirs", "mytmp").unwrap();
        for d in [
            "/home/me/Downloads/AppData/Roaming/kingsoft",
            "/home/me/Downloads/backup/temp",
            "/srv/npm-cache/x",
            "/home/me/mytmp/sub",
        ] {
            assert!(!store.adoptable_dir(d, false), "{d} 属于软排除,看到不放行");
            assert!(store.adoptable_dir(d, true), "{d} 软排除,点到该放行");
        }
    }

    /// 作用域上溯(修 6)+ 子树 LIKE(修 7)在 POSIX 分隔符下的口径钉子。
    #[cfg(not(windows))]
    #[test]
    fn scope_walk_and_subtree_pattern_work_on_posix_paths() {
        let store = Store::open_mem().unwrap();
        store
            .set_dir_intent(&DirIntent::new(
                "/home/me/Pictures",
                INTENT_IMPORTED,
                SRC_INITIAL_DIALOG,
            ))
            .unwrap();
        let map = ScopeMap::load(&store);
        assert_eq!(
            map.state_of("/home/me/Pictures/2026/summer"),
            Some(INTENT_IMPORTED),
            "深层目录要靠上溯命中祖先那条意图",
        );
        assert_eq!(map.state_of("/other/deep"), None, "账本覆盖不到就仍未纳入");
        // 根也算一级:落一条根的意图要能覆盖根下任意深度。
        store
            .set_dir_intent(&DirIntent::new("/", INTENT_IMPORTED, SRC_INITIAL_DIALOG))
            .unwrap();
        let map = ScopeMap::load(&store);
        assert_eq!(
            map.state_of("/srv/photos"),
            Some(INTENT_IMPORTED),
            "POSIX 根该被命中"
        );
        let (eq, sub) = dir_like("/home/me/Pictures");
        assert_eq!(eq, "/home/me/Pictures");
        assert_eq!(sub, "/home/me/Pictures/%");
        assert_eq!(dir_like("/").1, "/%", "POSIX 根的子树");
    }

    /// 自家产出目录里的行不该留在库里(「最近更新」首屏
    /// 200 张里 190 张是自家 thumbs,全是永远不亮的灰格子)。
    /// 清完打标记,二次调用不再动库;引用表跟着清,不留孤儿。
    #[cfg(windows)]
    #[test]
    fn purge_hard_excluded_drops_own_cache_and_recycle_bin_rows() {
        let store = Store::open_mem().unwrap();
        let loc = cache_dir_loc();
        let own = loc.join("thumbs").join("a.jpg");
        {
            let c = store.test_conn();
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,rich_state) VALUES
                 (1,?1,?2,0,0,0),
                 (2,'D:\\$RECYCLE.BIN\\S-1\\$R9X.jpg','D:\\$RECYCLE.BIN\\S-1',0,0,0),
                 (3,'D:\\pics\\b.jpg','D:\\pics',0,0,2)",
                rusqlite::params![own.to_string_lossy(), loc.join("thumbs").to_string_lossy()],
            )
            .unwrap();
            c.execute("INSERT INTO tags(id,name) VALUES(7,'t')", [])
                .unwrap();
            c.execute(
                "INSERT INTO photo_tags(photo_id,tag_id) VALUES(1,7),(3,7)",
                [],
            )
            .unwrap();
        }
        // 打开库时已经跑过一遍(空库,没清到东西),标记得先失效才能验清理逻辑。
        store.set_setting("hard_excluded_purged", "stale").unwrap();
        let n = store.purge_hard_excluded_once().unwrap();
        assert_eq!(n, 2, "自家 thumbs 与回收站那两行该被清掉");
        let left: Vec<i64> = store.read(|conn| {
            let mut st = conn.prepare("SELECT id FROM photos ORDER BY id").unwrap();
            st.query_map([], |r| r.get(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        });
        assert_eq!(left, vec![3], "普通照片行不受影响");
        let orphan: i64 = store
            .read(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM photo_tags WHERE photo_id=1",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(orphan, 0, "被清行的标签关联也要跟着清");
        assert_eq!(
            store.purge_hard_excluded_once().unwrap(),
            0,
            "打过标记不再重跑"
        );

        /// 同一条清库规则在 POSIX 路径上的对偶:回收站那行的分隔符是 `/`。
        #[cfg(not(windows))]
        #[test]
        fn purge_hard_excluded_drops_own_cache_and_recycle_bin_rows_posix() {
            let store = Store::open_mem().unwrap();
            let loc = cache_dir_loc();
            let own = loc.join("thumbs").join("a.jpg");
            {
                let c = store.test_conn();
                c.execute(
                    "INSERT INTO photos(id,path,parent,mtime,size,rich_state) VALUES
                 (1,?1,?2,0,0,0),
                 (2,'/mnt/win/$RECYCLE.BIN/S-1/$R9X.jpg','/mnt/win/$RECYCLE.BIN/S-1',0,0,0),
                 (3,'/home/me/pics/b.jpg','/home/me/pics',0,0,2)",
                    rusqlite::params![own.to_string_lossy(), loc.join("thumbs").to_string_lossy()],
                )
                .unwrap();
                c.execute("INSERT INTO tags(id,name) VALUES(7,'t')", [])
                    .unwrap();
                c.execute(
                    "INSERT INTO photo_tags(photo_id,tag_id) VALUES(1,7),(3,7)",
                    [],
                )
                .unwrap();
            }
            store.set_setting("hard_excluded_purged", "stale").unwrap();
            let n = store.purge_hard_excluded_once().unwrap();
            assert_eq!(n, 2, "自家 thumbs 与回收站那两行该被清掉");
            let left: Vec<i64> = store.read(|conn| {
                let mut st = conn.prepare("SELECT id FROM photos ORDER BY id").unwrap();
                st.query_map([], |r| r.get(0))
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect()
            });
            assert_eq!(left, vec![3], "只留没被硬排除的那行");
            let orphan: i64 = store
                .read(|conn| {
                    conn.query_row(
                        "SELECT COUNT(*) FROM photo_tags WHERE photo_id=1",
                        [],
                        |r| r.get(0),
                    )
                })
                .unwrap();
            assert_eq!(orphan, 0, "被删照片的标签关联要跟着走");
            assert_eq!(
                store.purge_hard_excluded_once().unwrap(),
                0,
                "已经打过标记就不再重扫",
            );
        }
    }

    /// 双向对账:撤出作用域的目录要退回占位,新纳入的目录要放出去派生。
    ///
    /// 只做单向(旧实现只降不升)的话,账本一改,库里就永远留着一批「本该亮
    /// 却停在灰格子」的行 —— 用户看到的就是「启动了半天也没有更新」。
    /// 顺带验软层的两条:用户亲手导入过的 `AppData` 子目录放行,没导入的不放。
    #[test]
    fn reconcile_rich_state_moves_rows_both_ways() {
        let store = Store::open_mem().unwrap();
        store
            .set_dir_intent(&DirIntent::new("D:\\in", INTENT_IMPORTED, SRC_IMPORT))
            .unwrap();
        store
            .set_dir_intent(&DirIntent::new(
                "C:\\Users\\me\\AppData\\Roaming\\kingsoft",
                INTENT_IMPORTED,
                SRC_IMPORT,
            ))
            .unwrap();
        {
            let c = store.test_conn();
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,rich_state) VALUES
                 (1,'D:\\in\\a.jpg','D:\\in',0,0,0),
                 (2,'D:\\in\\bad.jpg','D:\\in',0,0,0),
                 (3,'D:\\gone\\b.jpg','D:\\gone',0,0,2),
                 (4,'C:\\Users\\me\\AppData\\Roaming\\kingsoft\\k.jpg',
                     'C:\\Users\\me\\AppData\\Roaming\\kingsoft',0,0,0),
                 (5,'C:\\Users\\me\\AppData\\Local\\npm-cache\\n.png',
                     'C:\\Users\\me\\AppData\\Local\\npm-cache',0,0,0)",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO bad_media(path,reason,since) VALUES('D:\\in\\bad.jpg','x',0)",
                [],
            )
            .unwrap();
        }
        let gate = ScopeGate::load(&store);
        assert_eq!(store.reconcile_rich_state(&gate).unwrap(), 3, "该动 3 行");
        let states: Vec<(i64, i64)> = store.read(|conn| {
            let mut st = conn
                .prepare("SELECT id, rich_state FROM photos ORDER BY id")
                .unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        });
        assert_eq!(
            states,
            vec![
                (1, 1), // 新纳入 → 待派生
                (2, 3), // 新纳入但已在坏文件台账 → 回 3,不出队
                (3, 2), // 已派生、目录不在账本 → 保持,缓存不作废
                (4, 1), // 软排除 + 用户亲手导入 → 放行
                (5, 0), // 软排除、没表态 → 留给闲时补齐 / 按张放行
            ]
        );
    }

    /// 对账只升不降:已派生行、正在排队的行、坏文件都保持原状。
    ///
    /// 旧口径整目录 `SET rich_state=0`,造出一批「dhash 在、缩略图在、状态却是 0」
    /// 的矛盾行:重复检测按哈希把它们列成组,界面却拿状态当没图。它还会把
    /// 刚按张放行、还没轮到生成的行踢回队列外,和显示层抢状态。
    #[test]
    fn reconcile_keeps_derived_rows_when_dir_leaves_scope() {
        let store = Store::open_mem().unwrap();
        {
            let c = store.test_conn();
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,rich_state,thumb_ok,dhash) VALUES
                 (1,'D:\\gone\\done.jpg','D:\\gone',0,0,2,1,777),
                 (2,'D:\\gone\\waiting.jpg','D:\\gone',0,0,1,0,NULL),
                 (3,'D:\\gone\\broken.jpg','D:\\gone',0,0,3,0,NULL)",
                [],
            )
            .unwrap();
        }
        let gate = ScopeGate::load(&store);
        assert!(!gate.dir_derivable("D:\\gone"), "这个目录不在账本里");
        store.reconcile_rich_state(&gate).unwrap();
        let states: Vec<(i64, i32, i64, Option<i64>)> = store.read(|conn| {
            let mut st = conn
                .prepare("SELECT id, rich_state, thumb_ok, dhash FROM photos ORDER BY id")
                .unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        });
        assert_eq!(
            states,
            vec![
                (1, RICH_DONE, 1, Some(777)),
                (2, RICH_PENDING, 0, None),
                (3, RICH_BAD, 0, None),
            ]
        );
    }

    /// 按张放行:只抬点名的那几张,不牵动同目录其它行,坏文件不复活。
    #[test]
    fn promote_photos_only_touches_named_rows() {
        let store = Store::open_mem().unwrap();
        {
            let c = store.test_conn();
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,rich_state,thumb_ok) VALUES
                 (1,'/p/a.jpg','/p',0,0,0,0),
                 (2,'/p/b.jpg','/p',0,0,0,0),
                 (3,'/p/c.jpg','/p',0,0,2,1),
                 (4,'/p/bad.jpg','/p',0,0,3,0)",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO bad_media(path,reason,since) VALUES('/p/bad.jpg','x',0)",
                [],
            )
            .unwrap();
        }
        assert_eq!(store.promote_photos(&[1, 4]).unwrap(), 1, "坏文件不复活");
        // 台账说已生成、但磁盘缓存没了的行,按张放行要一并退回待派生。
        assert_eq!(store.promote_photos(&[3]).unwrap(), 1);
        let missing: Vec<i64> = store
            .missing_tier(1, &[], 10)
            .into_iter()
            .map(|r| r.0)
            .collect();
        assert_eq!(missing, vec![1, 3], "只放行点名的两张,同目录其它行不动");
    }

    /// 首次派生不等稳定期;「已做过又变了」的重做行在后台兜底批次里等安静,
    /// 但用户当前可见的目录立刻出队 —— 眼前的图优先。
    #[test]
    fn redoing_rows_wait_for_quiet_period_but_visible_dirs_do_not() {
        let store = Store::open_mem().unwrap();
        let now = chrono::Utc::now().timestamp();
        {
            let c = store.test_conn();
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,rich_state,thumb_ok) VALUES
                 (1,'/p/fresh.jpg','/p',?1,0,1,0),
                 (2,'/p/changed.jpg','/p',?1,0,1,1),
                 (3,'/p/settled.jpg','/p',?2,0,1,1)",
                rusqlite::params![now, now - 600],
            )
            .unwrap();
        }
        let mut idle: Vec<i64> = store
            .missing_tier(1, &[], 10)
            .into_iter()
            .map(|r| r.0)
            .collect();
        idle.sort();
        assert_eq!(idle, vec![1, 3], "首次派生与已安静的重做行照常出队");
        let mut visible: Vec<i64> = store
            .missing_tier(1, &["/p".to_string()], 10)
            .into_iter()
            .map(|r| r.0)
            .collect();
        visible.sort();
        assert_eq!(visible, vec![1, 2, 3], "可见目录不受稳定期限制");
    }

    /// 放行与回读必须是同一条口径(`AppData` 里早已派生完的图,
    /// 屏幕上仍锁在「未纳入范围」—— 因为界面按排除名单自己又判了一遍门)。
    ///
    /// `promote_dir` 只改库;界面拿 `promoted_states` 回读真相。判定只留库里这一份。
    #[test]
    fn promoted_states_reads_back_exactly_what_promote_dir_flipped() {
        let store = Store::open_mem().unwrap();
        {
            let c = store.test_conn();
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,rich_state) VALUES
                 (1,'D:\\p\\a.jpg','D:\\p',0,0,0),
                 (2,'D:\\p\\sub\\b.jpg','D:\\p\\sub',0,0,0),
                 (3,'D:\\q\\c.jpg','D:\\q',0,0,0),
                 (4,'D:\\p\\d.jpg','D:\\p',0,0,3),
                 (6,'D:\\pi\\f.jpg','D:\\pi',0,0,0)",
                [],
            )
            .unwrap();
        }
        // 已立案、但状态还停在 0 的行(台账拦放行,回读也不该把它说成已放行)。
        store.mark_bad_media("D:\\p\\e.jpg", "立案").unwrap();
        store
            .test_conn()
            .execute(
                "INSERT INTO photos(id,path,parent,mtime,size,rich_state) VALUES(5,'D:\\p\\e.jpg','D:\\p',0,0,0)",
                [],
            )
            .unwrap();

        assert_eq!(
            store.promote_dir("D:\\p").unwrap(),
            2,
            "只放行本目录与子树里未纳入且未立案的行"
        );
        let mut got = store.promoted_states("D:\\p");
        got.sort();
        assert_eq!(
            got,
            vec![(1, RICH_PENDING), (2, RICH_PENDING), (4, RICH_BAD)],
            "回读给界面的就是库里的现状:5 号在台账里、没被放行,也就不在这份名单里"
        );
        assert!(
            store.promoted_states("D:\\q").is_empty(),
            "不该串到别的目录"
        );
    }

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

    /// 效果栈存 `photos.filters` 列(普通 TEXT 列),
    /// 支持单张读写、批量写(粘贴所有效果)、批量读(复制所有效果)。
    #[test]
    fn filters_roundtrip_and_batch() {
        let store = Store::open_mem().unwrap();
        let conn = store.test_conn();
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
            let c = store.test_conn();
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
        assert_eq!(
            stats,
            vec![("/data/pics".to_string(), 2)],
            "应含子文件夹,不含 /other"
        );
        store.remove_watched(Path::new("/data/pics")).unwrap();
        assert!(store.watched_stats().is_empty());
        // 索引不受影响
        let n: i64 = store
            .test_conn()
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
        let gone = format!(
            "{}/definitely-not-here-{}.jpg",
            std::env::temp_dir().display(),
            std::process::id()
        );
        std::fs::write(&keep, b"x").unwrap();
        {
            let c = store.test_conn();
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
                .query_row(
                    "SELECT id FROM photos WHERE path=?1",
                    rusqlite::params![gone],
                    |r| r.get(0),
                )
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
        let c = store.test_conn();
        for (tbl, col) in [
            ("photos", "path"),
            ("faces", "photo_id"),
            ("face_templates", "face_id"),
            ("clips", "photo_id"),
            ("photo_tags", "photo_id"),
        ] {
            let q = format!(
                "SELECT COUNT(*) FROM {tbl} WHERE {col} IN (SELECT id FROM photos WHERE path=?1)"
            );
            let n: i64 = c
                .query_row(&q, rusqlite::params![gone], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{tbl} 应为空");
        }
        let n_photos: i64 = c
            .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_photos, 1, "存在的照片必须保留");
    }

    /// 删照片时 AI 队列/台账/标签/向量/OCR 与全文索引一并清,避免 id 复用串图。
    #[test]
    fn delete_photo_clears_ai_rows_and_fts() {
        let store = Store::open_mem().unwrap();
        {
            let c = store.test_conn();
            c.execute_batch(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(42,'C:/gone.jpg','C:/',0,0);
                 INSERT INTO ai_jobs(stage,photo_id,priority,enqueued_ts,attempts) VALUES('clip',42,6,0,0);
                 INSERT INTO ai_state(photo_id,stage,model_ver,done_ts) VALUES(42,'clip','v1',0);
                 INSERT INTO ai_failed(photo_id,stage,err,attempts,ts) VALUES(42,'clip','e',3,0);
                 INSERT INTO clips_vec(photo_id,ver,dim,embedding) VALUES(42,'v1',1,X'00');
                 INSERT INTO tag_labels(id,node,name_zh,name_en,prompt_en,builtin,ver)
                     VALUES(1,'人物','合影','group','a group',1,'v1');
                 INSERT INTO auto_tags(photo_id,label_id,score,model_ver) VALUES(42,1,0.9,'v1');
                 INSERT INTO ocr_docs(photo_id,text,seg,line_count,char_count,model_ver,done_ts)
                     VALUES(42,'发票','发 票',1,2,'v1',0);
                 INSERT INTO ocr_fts(rowid,seg) VALUES(42,'发 票');
                 INSERT INTO ocr_boxes(photo_id,line_no) VALUES(42,0);",
            )
            .unwrap();
        }
        {
            let c = store.test_conn();
            let tx = c.unchecked_transaction().unwrap();
            Store::delete_photo_rows(&tx, &[(42, "C:/gone.jpg".into())]).unwrap();
            tx.commit().unwrap();
        }
        let c = store.test_conn();
        let ai_tables = [
            "ai_jobs", "ai_state", "ai_failed", "clips_vec", "auto_tags", "ocr_docs",
            "ocr_boxes",
        ];
        for tbl in ai_tables {
            let q = format!("SELECT COUNT(*) FROM {tbl} WHERE photo_id=42");
            let n: i64 = c.query_row(&q, [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{tbl} 应随照片一起删除");
        }
        let fts: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM ocr_fts WHERE ocr_fts MATCH '发 票'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts, 0, "OCR 全文索引不该留下已删照片");
        let labels: i64 = c
            .query_row("SELECT COUNT(*) FROM tag_labels", [], |r| r.get(0))
            .unwrap();
        assert_eq!(labels, 1, "标签词表是全局数据,不能跟着照片删");
        c.execute(
            "INSERT INTO photos(id,path,parent,mtime,size) VALUES(42,'C:/new.jpg','C:/',0,0)",
            [],
        )
        .unwrap();
        for tbl in ai_tables {
            let q = format!("SELECT COUNT(*) FROM {tbl} WHERE photo_id=42");
            let n: i64 = c.query_row(&q, [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{tbl} 不该被复用 id 的新照片继承");
        }
    }

    /// 监听线程的精确对账:只删给定且确实不存在的路径,仍存在的跳过。
    /// 关联行(缩略图档 / 相册 / 中文 CLIP)一并清,
    /// 并写入 `removed` 变更日志。
    #[test]
    fn drop_missing_paths_only_removes_vanished_ones() {
        let store = Store::open_mem().unwrap();
        let dir = std::env::temp_dir();
        let keep = format!("{}/keep-{}.jpg", dir.display(), std::process::id());
        let gone = format!("{}/gone-{}.jpg", dir.display(), std::process::id());
        std::fs::write(&keep, b"x").unwrap();
        let _ = std::fs::remove_file(&gone);
        let gone_id;
        {
            let c = store.test_conn();
            for p in [&keep, &gone] {
                c.execute(
                    "INSERT INTO photos(path,parent,mtime,size) VALUES(?1,'/',0,0)",
                    rusqlite::params![p],
                )
                .unwrap();
            }
            gone_id = c
                .query_row(
                    "SELECT id FROM photos WHERE path=?1",
                    rusqlite::params![gone],
                    |r| r.get(0),
                )
                .unwrap();
            c.execute(
                "INSERT INTO clips_cn(photo_id,dim,vec) VALUES(?1,1,X'00')",
                rusqlite::params![gone_id],
            )
            .unwrap();
            c.execute(
                "INSERT INTO thumb_tiers(photo_id,tier) VALUES(?1,1)",
                rusqlite::params![gone_id],
            )
            .unwrap();
            c.execute("INSERT INTO albums(name,ts) VALUES('a',0)", [])
                .unwrap();
            let album_id = c.last_insert_rowid();
            c.execute(
                "INSERT INTO album_photos(album_id,photo_id,pos) VALUES(?1,?2,0)",
                rusqlite::params![album_id, gone_id],
            )
            .unwrap();
        }
        let removed = store
            .drop_missing_paths(&[keep.clone(), gone.clone(), format!("{gone}x")])
            .unwrap();
        assert_eq!(removed, vec![(gone_id, gone.clone())]);

        let c = store.test_conn();
        for (tbl, col) in [
            ("photos", "id"),
            ("clips_cn", "photo_id"),
            ("thumb_tiers", "photo_id"),
            ("album_photos", "photo_id"),
        ] {
            let n: i64 = c
                .query_row(
                    &format!("SELECT COUNT(*) FROM {tbl} WHERE {col}=?1"),
                    rusqlite::params![gone_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "{tbl} 应清空");
        }
        let kept: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM photos WHERE path=?1",
                rusqlite::params![keep],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1, "仍存在的文件不能移出图库");
        let logged: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM changes WHERE action='removed' AND photo_id=?1",
                rusqlite::params![gone_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(logged, 1, "移出应写入变更日志");
        drop(c);
        let _ = std::fs::remove_file(&keep);
    }

    /// 目录被删:事件只报目录路径,库里的子文件也要一起移出;
    /// 前缀相近的兄弟文件不能误伤。
    #[test]
    fn drop_missing_paths_removes_subtree_but_not_sibling_prefix() {
        let store = Store::open_mem().unwrap();
        let dir = std::env::temp_dir().join(format!("rk-gone-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let child = dir.join("a.jpg");
        let sibling = PathBuf::from(format!("{}x.jpg", dir.display()));
        {
            let c = store.test_conn();
            for p in [&child, &sibling] {
                c.execute(
                    "INSERT INTO photos(path,parent,mtime,size) VALUES(?1,'/',0,0)",
                    rusqlite::params![p.to_string_lossy().as_ref()],
                )
                .unwrap();
            }
        }
        let removed = store
            .drop_missing_paths(&[dir.to_string_lossy().into_owned()])
            .unwrap();
        assert_eq!(removed.len(), 1, "只应移出目录内的文件:{removed:?}");
        assert_eq!(removed[0].1, child.to_string_lossy());

        let c = store.test_conn();
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM photos WHERE path=?1",
                rusqlite::params![sibling.to_string_lossy().as_ref()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "前缀相近的兄弟文件不能误删");
    }

    // ===== 字符串池 =====

    const POOL: &str = "photos_cam";

    fn add_photo(store: &Store, id: i64, cam_id: Option<i64>) {
        let c = store.test_conn();
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
        // 同串恒返回同 id(对齐字符串查重哈希的查重)
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
    fn missing_tier_takes_scoped_rows_and_ledger_removes_bad_ones() {
        // 回填队列的取行条件是派生门放行的 `rich_state=1`,
        // 不再是"所有 thumb_ok=0" —— 不在作用域里的行根本不进队列。
        // 反复失败的行由坏媒体台账立案后置 3 出队,
        // 出队靠台账而不是 thumb_fail 阈值(否则一批坏文件会让 worker
        // 永远空转在开头,常见来源是回收站 $I*.jpg 元数据)。
        let store = Store::open_mem().unwrap();
        {
            let c = store.test_conn();
            // 1:已进门待派生 → 该出队。
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,rich_state)
                 VALUES(1,'/x/a.jpg','/x',0,0,1)",
                [],
            )
            .unwrap();
            // 2:未纳入作用域 → 不该出队(哪怕 thumb_ok=0)。
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,rich_state)
                 VALUES(2,'/x/b.jpg','/x',0,0,0)",
                [],
            )
            .unwrap();
            // 3:已派生 → 不该出队。
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,rich_state,thumb_ok)
                 VALUES(3,'/x/c.jpg','/x',0,0,2,1)",
                [],
            )
            .unwrap();
        }
        let ids = |s: &Store| -> Vec<i64> {
            s.missing_tier(1, &[], 10)
                .into_iter()
                .map(|(id, _)| id)
                .collect()
        };
        assert_eq!(ids(&store), vec![1], "只有 rich_state=1 的行进队列");

        // 失败记账:每次 +1,并回传累计值供调用方判断是否到阈值。
        assert_eq!(store.mark_thumb_fail(1), 1);
        assert_eq!(store.mark_thumb_fail(1), 2);
        assert_eq!(ids(&store), vec![1], "没到阈值仍在队列里");

        // 到阈值 → 立案:台账建行 + rich_state 1→3 + 出队。
        assert_eq!(store.mark_thumb_fail(1), BAD_MEDIA_THRESHOLD);
        store.mark_bad_media("/x/a.jpg", "decode failed").unwrap();
        assert!(store.is_bad_media("/x/a.jpg"));
        assert!(ids(&store).is_empty(), "立案后不再每轮重试同一张坏图");

        // 「不隐藏」出口:销案 → 台账删行 + rich_state 3→1 + 计数归零 + 重新排队。
        store.unmark_bad_media("/x/a.jpg").unwrap();
        assert!(!store.is_bad_media("/x/a.jpg"));
        assert_eq!(ids(&store), vec![1], "销案后放回队列");
        let f: i64 = store
            .test_conn()
            .query_row("SELECT thumb_fail FROM photos WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(f, 0);
    }

    #[test]
    fn missing_tier_prefers_visible_dirs_then_falls_back() {
        // 的排序键(用户已拍):可见区域命中优先 + 其余全库兜底。
        let store = Store::open_mem().unwrap();
        {
            let c = store.test_conn();
            for (id, parent) in [(1, "/far"), (2, "/near"), (3, "/near")] {
                c.execute(
                    "INSERT INTO photos(id,path,parent,mtime,size,rich_state)
                     VALUES(?1,?2 || '/' || ?1 || '.jpg',?2,0,0,1)",
                    rusqlite::params![id, parent],
                )
                .unwrap();
            }
        }
        let vis = vec!["/near".to_string()];
        let ids: Vec<i64> = store
            .missing_tier(1, &vis, 2)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec![2, 3], "先补眼前的");
        let ids: Vec<i64> = store
            .missing_tier(1, &vis, 3)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            ids,
            vec![2, 3, 1],
            "可见的拿完了,不足一批用全库补齐且不重复"
        );
    }

    #[test]
    fn open_migrates_legacy_db_without_cam_id_column() {
        // 模拟早期版本的 index.db:photos 表已存在但只有老列
        // (没有 cam_id / filters / kind 等后续迁移列)。
        // SCHEMA 不得引用任何迁移列,否则建索引先于 ALTER 失败,
        // 程序启动即 panic,且没有回退余地。
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE photos(
                 id INTEGER PRIMARY KEY,
                 path TEXT UNIQUE NOT NULL,
                 parent TEXT NOT NULL,
                 mtime INTEGER NOT NULL,
                 size INTEGER NOT NULL,
                 taken_utc TEXT,
                 cam TEXT,
                 rot INTEGER NOT NULL DEFAULT 0,
                 thumb_ok INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO photos(path,parent,mtime,size,cam)
                 VALUES('/p/a.jpg','/p',0,0,'Nikon D850');",
        )
        .unwrap();
        let store = Store::from_conn(conn).unwrap();
        let c = store.test_conn();
        // 迁移列已补齐,池索引已建立
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index'
                 AND name='idx_photos_cam_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "cam_id 索引应在迁移后建立");
        // 旧行的相机型号已被 backfill_cam_pool 迁入字符串池(cam 置空)
        let cid: Option<i64> = c
            .query_row("SELECT cam_id FROM photos WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert!(cid.is_some(), "旧行相机型号应已入池");
        let val: String = c
            .query_row(
                "SELECT value FROM string_pool WHERE pool='photos_cam' AND value_id=?1",
                rusqlite::params![cid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(val, "Nikon D850");
    }

    #[test]
    fn backfill_person_ids_once_migrates_legacy_rows() {
        let store = Store::open_mem().unwrap();
        // open_mem 已跑过回填;清掉库内标记与进程缓存,模拟尚未迁移的老库。
        {
            let c = store.test_conn();
            c.execute("DELETE FROM settings WHERE k='person_id_v1'", [])
                .unwrap();
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,1)",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO faces(photo_id,cluster,person_id) VALUES(1,7,NULL)",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO faces(photo_id,cluster,person_id) VALUES(1,NULL,NULL)",
                [],
            )
            .unwrap();
        }
        store.settings.lock().unwrap().remove("person_id_v1");
        assert_eq!(store.backfill_person_ids_once().unwrap(), 1);
        let c = store.test_conn();
        let pid: Option<i64> = c
            .query_row(
                "SELECT person_id FROM faces WHERE cluster=7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pid, Some(7), "老库簇号应回填为身份");
        // 门控生效后新行不再回填,避免把「移出人物」的 NULL 又写回去。
        c.execute(
            "INSERT INTO faces(photo_id,cluster,person_id) VALUES(1,9,NULL)",
            [],
        )
        .unwrap();
        assert_eq!(store.backfill_person_ids_once().unwrap(), 0);
        let pid: Option<i64> = c
            .query_row(
                "SELECT person_id FROM faces WHERE cluster=9",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pid, None);
    }

    #[test]
    fn backfill_cam_pool_migrates_legacy_rows() {
        let store = Store::open_mem().unwrap();
        {
            let c = store.test_conn();
            // 模拟 之前入库的行:cam 有值、cam_id 为 NULL
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size,cam) VALUES(1,'/p/a.jpg','/p',0,0,'Nikon D850')",
                [],
            )
            .unwrap();
        }
        assert_eq!(store.backfill_cam_pool().unwrap(), 1);
        let c = store.test_conn();
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
            let c = store.test_conn();
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
        let conn = store.test_conn();
        let ids: Vec<i64> = {
            let mut st = conn
                .prepare("SELECT cam_id FROM photos ORDER BY id")
                .unwrap();
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
            let c = store.test_conn();
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

    // ===== 持久化扫描队列 =====

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
            let store = Store::from_conn(Connection::open(&path).unwrap()).unwrap();
            store
                .enqueue_scan(&[std::path::PathBuf::from("E:\\cam")], "设备")
                .unwrap();
        }
        {
            let store = Store::from_conn(Connection::open(&path).unwrap()).unwrap();
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
            .test_conn()
            .prepare("PRAGMA table_info(photos)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(cols.contains(&"star".to_string()), "photos 列: {cols:?}");
        assert!(
            cols.contains(&"xmp_imported".to_string()),
            "photos 列: {cols:?}"
        );
        assert!(cols.contains(&"dhash".to_string()), "photos 列: {cols:?}");
        assert!(cols.contains(&"baked".to_string()), "photos 列: {cols:?}");
        assert!(cols.contains(&"lat".to_string()), "photos 列: {cols:?}");
        assert!(cols.contains(&"lon".to_string()), "photos 列: {cols:?}");
        let tcols: Vec<String> = store
            .test_conn()
            .prepare("PRAGMA table_info(tags)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tcols.contains(&"color".to_string()), "tags 列: {tcols:?}");
        let fcols: Vec<String> = store
            .test_conn()
            .prepare("PRAGMA table_info(faces)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            fcols.contains(&"quality".to_string()),
            "faces 列: {fcols:?}"
        );
        assert!(
            fcols.contains(&"ignored".to_string()),
            "faces 列: {fcols:?}"
        );
    }

    // ===== uid64 =====

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
            let c = store.test_conn();
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
            .test_conn()
            .query_row("SELECT uid64 FROM photos WHERE path='/x/a.jpg'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let id = store.photo_by_uid64(uid as u64).expect("应能按 uid64 查到");
        let path: String = store
            .test_conn()
            .query_row("SELECT path FROM photos WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(path, "/x/a.jpg");

        // 不存在的标识返回 None。
        assert!(store.photo_by_uid64(0xdead_beef_1234_5678).is_none());
    }

    // ===== 变更日志 =====

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

    /// thumb_tiers 表记录各档完成状态。
    #[test]
    fn thumb_tiers_tracks_per_tier_state() {
        let store = Store::open_mem().unwrap();
        let pid: i64 = {
            let c = store.test_conn();
            c.execute(
                "INSERT INTO photos(path,parent,mtime,size) VALUES('/t/a.jpg','/t',0,0)",
                [],
            )
            .unwrap();
            c.last_insert_rowid()
        };
        // 未生成时 missing_tier 应返回该照片。
        let missing = store.missing_tier(TIER_BIG, &[], 10);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].0, pid);

        store.mark_tier_done(pid, TIER_BIG).unwrap();
        assert!(store.missing_tier(TIER_BIG, &[], 10).is_empty());
        assert_eq!(store.tier_count(TIER_BIG), 1);
        // 重复标记幂等。
        store.mark_tier_done(pid, TIER_BIG).unwrap();
        assert_eq!(store.tier_count(TIER_BIG), 1);

        // Normal 档走 photos.thumb_ok,不走 thumb_tiers;取行看的是派生队列
        // 状态:rich_state=0 的行不进队列(由按张放行 / 闲时补齐处理)。
        assert!(
            store.missing_tier(TIER_NORMAL, &[], 10).is_empty(),
            "没排进派生队列的行不进回填队列"
        );
        store
            .test_conn()
            .execute("UPDATE photos SET rich_state=1 WHERE id=?1", [pid])
            .unwrap();
        assert_eq!(store.missing_tier(TIER_NORMAL, &[], 10).len(), 1);
        // 派生完成是 thumb_ok 与 rich_state 一起落定(mark_derived 一条语句):
        // 只改 thumb_ok 却还停在排队态,属于要重做的行,仍会出队。
        store
            .test_conn()
            .execute("UPDATE photos SET thumb_ok=1 WHERE id=?1", [pid])
            .unwrap();
        assert_eq!(store.missing_tier(TIER_NORMAL, &[], 10).len(), 1);
        store
            .test_conn()
            .execute("UPDATE photos SET rich_state=2 WHERE id=?1", [pid])
            .unwrap();
        assert!(store.missing_tier(TIER_NORMAL, &[], 10).is_empty());
        assert_eq!(store.tier_count(TIER_NORMAL), 1);
    }

    /// 造一条 L1 结构行(frn64 由盘符 + FRN 算)。
    fn row(letter: char, frn: u64, path: &str, mtime: i64, size: i64) -> L1Row {
        let parent = std::path::Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = match path.rfind('.') {
            Some(p) if p + 1 < path.len() => Some(path[p + 1..].to_ascii_lowercase()),
            _ => None,
        };
        L1Row {
            frn64: frn64_of(letter, frn),
            path: path.into(),
            parent,
            mtime,
            size,
            kind: 0,
            ext,
        }
    }

    #[test]
    fn frn64_ignores_sequence_bits() {
        // 高 16 位是记录序号,重建/重用后递增 —— 身份不能跟着变。
        assert_eq!(frn64_of('C', 0x0001_0000_0000_0234), frn64_of('C', 0x0234));
        // 不同卷、不同 index 必须分开。
        assert_ne!(frn64_of('C', 0x234), frn64_of('D', 0x234));
        assert_ne!(frn64_of('C', 0x234), frn64_of('C', 0x235));
    }

    #[test]
    fn l1_ingest_inserts_then_updates_same_identity() {
        let store = Store::open_mem().unwrap();
        let c = store
            .ingest_l1_rows(&[row('C', 0x100, r"C:\pics\a.jpg", 1000, 4096)], None)
            .unwrap();
        assert_eq!(
            c,
            L1Ingest {
                inserted: 1,
                updated: 0
            }
        );
        let id: i64 = store
            .test_conn()
            .query_row(
                "SELECT id FROM photos WHERE path=?1",
                [r"C:\pics\a.jpg"],
                |r| r.get(0),
            )
            .unwrap();
        // 同 FRN 改名 + 变大 → 命中同一行,身份与外部引用都不换。
        let c = store
            .ingest_l1_rows(&[row('C', 0x100, r"C:\albums\b.jpg", 2000, 8192)], None)
            .unwrap();
        assert_eq!(
            c,
            L1Ingest {
                inserted: 0,
                updated: 1
            }
        );
        let conn = store.test_conn();
        let (n, path, size, mtime): (i64, String, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), MAX(path), MAX(size), MAX(mtime) FROM photos",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(n, 1, "改名不该多出一行");
        assert_eq!(path, r"C:\albums\b.jpg");
        assert_eq!((size, mtime), (8192, 2000));
        let same_id: i64 = conn
            .query_row(
                "SELECT id FROM photos WHERE path=?1",
                [r"C:\albums\b.jpg"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(same_id, id, "行 id 不变 → 星标/标签/说明都还在");
    }

    #[test]
    fn l1_ingest_rename_keeps_uid64_and_stars() {
        let store = Store::open_mem().unwrap();
        store
            .ingest_l1_rows(&[row('C', 0x100, r"C:\pics\a.jpg", 1000, 4096)], None)
            .unwrap();
        let uid_before: i64 = store
            .test_conn()
            .query_row("SELECT uid64 FROM photos", [], |r| r.get(0))
            .unwrap();
        store
            .test_conn()
            .execute("UPDATE photos SET star=5", [])
            .unwrap();
        store
            .ingest_l1_rows(&[row('C', 0x100, r"C:\pics\renamed.jpg", 1000, 4096)], None)
            .unwrap();
        let (uid_after, star): (i64, i64) = store
            .test_conn()
            .query_row("SELECT uid64, star FROM photos", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(uid_after, uid_before, "uid64 的契约是一经分配即不变");
        assert_eq!(star, 5, "用户数据不因改名丢失");
    }

    #[test]
    fn l1_ingest_mtime_change_keeps_old_thumb_and_requeues() {
        let store = Store::open_mem().unwrap();
        store
            .ingest_l1_rows(&[row('C', 0x100, r"C:\pics\a.jpg", 1000, 4096)], None)
            .unwrap();
        store
            .test_conn()
            .execute("UPDATE photos SET thumb_ok=1, dhash=123, rich_state=2", [])
            .unwrap();
        // 内容变了(mtime 前进)→ 只重新排队。旧缩略图继续显示、旧 dhash
        // 让重复分组不散,等后台重派生完成时一次性覆盖。
        store
            .ingest_l1_rows(&[row('C', 0x100, r"C:\pics\a.jpg", 2000, 5000)], None)
            .unwrap();
        let (thumb, dhash, rich): (i64, Option<i64>, i32) = store
            .test_conn()
            .query_row("SELECT thumb_ok, dhash, rich_state FROM photos", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!((thumb, dhash), (1, Some(123)));
        assert_eq!(rich, RICH_PENDING, "内容变了要重新排队");
        // mtime 没动 → 派生态与状态都保留。
        store
            .test_conn()
            .execute("UPDATE photos SET thumb_ok=1, dhash=999, rich_state=2", [])
            .unwrap();
        store
            .ingest_l1_rows(&[row('C', 0x100, r"C:\pics\a2.jpg", 2000, 5000)], None)
            .unwrap();
        let (thumb, dhash, rich): (i64, Option<i64>, i32) = store
            .test_conn()
            .query_row("SELECT thumb_ok, dhash, rich_state FROM photos", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!((thumb, dhash, rich), (1, Some(999), RICH_DONE));
    }

    #[test]
    fn l1_ingest_purges_stale_path_holder() {
        let store = Store::open_mem().unwrap();
        // 旧文件占着目标路径(身份不同,frn64=NULL 的历史行也算)。
        store
            .test_conn()
            .execute(
                "INSERT INTO photos(path,parent,mtime,size) VALUES(?1,'/p',0,0)",
                [r"C:\pics\slot.jpg"],
            )
            .unwrap();
        let c = store
            .ingest_l1_rows(&[row('C', 0x200, r"C:\pics\slot.jpg", 10, 20)], None)
            .unwrap();
        assert_eq!(
            c,
            L1Ingest {
                inserted: 1,
                updated: 0
            }
        );
        let n: i64 = store
            .test_conn()
            .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "path 上的 UNIQUE 仍成立:陈旧占用被摘掉");
    }

    #[test]
    fn l1_delete_by_frn64_ignores_unknown() {
        let store = Store::open_mem().unwrap();
        store
            .ingest_l1_rows(
                &[
                    row('C', 0x100, r"C:\pics\a.jpg", 1, 1),
                    row('C', 0x101, r"C:\pics\b.jpg", 1, 1),
                ],
                None,
            )
            .unwrap();
        let n = store
            .delete_l1_frn64(&[frn64_of('C', 0x100), frn64_of('C', 0xdead)])
            .unwrap();
        assert_eq!(n, 1, "不认识的 FRN 不算删除");
        let left: String = store
            .test_conn()
            .query_row("SELECT path FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, r"C:\pics\b.jpg");
        assert_eq!(store.delete_l1_frn64(&[]).unwrap(), 0);
    }

    /// L1 同步删除不能只删主表:引用表与 FTS 幽灵行都要清。
    #[test]
    fn l1_purge_clears_reference_rows() {
        let store = Store::open_mem().unwrap();
        let seed = |frn: u64| {
            store
                .ingest_l1_rows(&[row('C', frn, r"C:\pics\slot.jpg", 1, 1)], None)
                .unwrap();
            store
                .test_conn()
                .query_row("SELECT id FROM photos WHERE path='C:\\pics\\slot.jpg'", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap()
        };
        let refs = |id: i64| {
            store
                .test_conn()
                .execute_batch(&format!(
                    "INSERT OR IGNORE INTO tags(name) VALUES('l1tag');
                     INSERT OR IGNORE INTO photo_tags(photo_id,tag_id)
                       SELECT {id}, id FROM tags WHERE name='l1tag';
                     INSERT INTO ai_jobs(stage,photo_id,priority,enqueued_ts) VALUES('Clip',{id},1,0);
                     INSERT INTO ocr_docs(photo_id,text,seg,line_count,char_count,model_ver,done_ts)
                       VALUES({id},'发票','发票',1,2,'v1',0);
                     INSERT INTO ocr_fts(rowid,seg) VALUES({id},'发票');"
                ))
                .unwrap();
        };
        let count = |sql: &str| -> i64 {
            store
                .test_conn()
                .query_row(sql, [], |r| r.get(0))
                .unwrap()
        };
        let first = seed(0x300);
        refs(first);
        assert_eq!(store.delete_l1_frn64(&[frn64_of('C', 0x300)]).unwrap(), 1);
        assert_eq!(count("SELECT COUNT(*) FROM photos"), 0);
        assert_eq!(count("SELECT COUNT(*) FROM photo_tags"), 0);
        assert_eq!(count("SELECT COUNT(*) FROM ai_jobs"), 0);
        assert_eq!(count("SELECT COUNT(*) FROM ocr_docs"), 0);
        assert_eq!(
            count("SELECT COUNT(*) FROM ocr_fts WHERE ocr_fts MATCH '发票'"),
            0,
            "外部内容 FTS 不能留下幽灵行"
        );
        // 同 path 换 FRN 触发的陈旧占用清理,也要走同一份逐表清理。
        let second = seed(0x301);
        refs(second);
        store
            .ingest_l1_rows(&[row('C', 0x302, r"C:\pics\slot.jpg", 1, 1)], None)
            .unwrap();
        assert_eq!(count("SELECT COUNT(*) FROM photos"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM photo_tags"), 0);
        assert_eq!(count("SELECT COUNT(*) FROM ai_jobs"), 0);
        assert_eq!(count("SELECT COUNT(*) FROM ocr_docs"), 0);
        assert_eq!(
            count("SELECT COUNT(*) FROM ocr_fts WHERE ocr_fts MATCH '发票'"),
            0
        );
    }

    #[test]
    fn l2_cursor_roundtrips_per_volume() {
        let store = Store::open_mem().unwrap();
        assert_eq!(store.l2_cursor('C'), 0, "从未落过 → 0");
        store.set_l2_cursor('C', 0x1a2b_3c4d).unwrap();
        store.set_l2_cursor('D', 7).unwrap();
        assert_eq!(store.l2_cursor('C'), 0x1a2b_3c4d);
        assert_eq!(store.l2_cursor('D'), 7);
        assert_eq!(store.l2_cursor('E'), 0, "各卷互不串");
    }
}

/// 供测试用的档位常量(与 thumbs::Tier::Big 对应)。
#[cfg(test)]
const TIER_BIG: u8 = 2;
/// tier=1 是 Normal 档(photos.thumb_ok 专用)。
#[cfg(test)]
const TIER_NORMAL: u8 = 1;
