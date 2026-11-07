//! 人脸模型就位:启动检查 + 服务端后台下载。
//!
//! 模型文件不进仓库,程序也不内置第三方直链:下载地址一律由 maliang.ini 的
//! `ModelURL`(服务端模型目录)展开,留空时界面回到「手动放置指引」的原行为。
//! 识别模型上百 MB,首启不自动开下 —— 只提示,用户点「下载模型」才联网;
//! 下载走后台线程分段续传,界面显示当前文件与进度。

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ai::face::FaceAlgo;
use crate::update::Config;

const CONNECT_SECS: u64 = 10;
const MAX_SEGMENT_ATTEMPTS: u32 = 4;
/// 单个模型文件的体积上限:超过就不是模型,是有人在目录里塞了别的东西。
const MAX_MODEL: u64 = 1024 * 1024 * 1024;

/// 哪些方案的模型可以由服务端分发。
///
/// YuNet(Apache-2.0)与 AdaFace(MIT)允许再分发;insightface antelopev2 的
/// 权重许可为非商用且不可再分发,只能用户自备,程序不代其下载。
pub fn downloadable(algo: FaceAlgo) -> bool {
    matches!(algo, FaceAlgo::AdaFace)
}

/// 该方案在模型目录里还缺哪些文件名(都在位则返回空)。
pub fn missing_files(algo: FaceAlgo, dir: &Path) -> Vec<String> {
    let (det, rec) = algo.files();
    [det, rec]
        .iter()
        .filter(|f| !dir.join(f).is_file())
        .map(|f| f.to_string())
        .collect()
}

/// 把 `ModelURL`(服务端模型目录)与固定文件名展开成下载地址。
///
/// 只放行 http/https;目录写法不带结尾 `/` 时自动补,查询串与片段先剥掉。
pub fn file_url(base: &str, name: &str) -> Result<String, String> {
    let base = base.trim();
    let lower = base.to_ascii_lowercase();
    if lower.starts_with("file:") || base.contains("://") && !is_http(&lower) {
        return Err(format!("ModelURL 只支持 http/https:{base}"));
    }
    let (scheme, rest) = base
        .split_once("://")
        .ok_or_else(|| "ModelURL 不是合法的 http/https 目录地址".to_string())?;
    // CDN 签名常挂在 ModelURL 后面,取目录前先剥掉查询与片段。
    let clean = rest
        .split('#')
        .next()
        .unwrap_or(rest)
        .split('?')
        .next()
        .unwrap_or(rest);
    let host_end = clean.find('/').unwrap_or(clean.len());
    if clean[..host_end].is_empty() {
        return Err("ModelURL 缺少主机名".into());
    }
    let mut path = clean[host_end..].to_string();
    if !path.ends_with('/') {
        path.push('/');
    }
    Ok(format!("{scheme}://{}{path}{name}", &clean[..host_end]))
}

fn is_http(lower: &str) -> bool {
    lower.starts_with("https://") || lower.starts_with("http://")
}

/// 下载完成后的门禁:先看 ONNX ModelProto 的首字段(`ir_version`,tag 字节 0x08),
/// 再用 onnxruntime 真加载一次。服务端放错文件、传坏、被截断都进不了模型目录。
fn verify_onnx(path: &Path) -> Result<(), String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("打开模型文件失败: {e}"))?;
    let mut head = [0u8; 16];
    let n = f
        .read(&mut head)
        .map_err(|e| format!("读取模型文件失败: {e}"))?;
    if n < 16 || head[0] != 0x08 {
        return Err("下载到的内容不是 ONNX 模型文件".into());
    }
    crate::ai::try_load(path).map(|_| ())
}

// ---------------------------------------------------------------- 后台下载

#[derive(Debug)]
enum Cmd {
    /// 命令自带配置快照:用户改过 maliang.ini 里的 ModelURL 时不必重启程序。
    Download {
        algo: FaceAlgo,
        cfg: Box<Config>,
    },
    Stop,
}

enum Event {
    Downloading {
        file: String,
        idx: usize,
        count: usize,
        pct: u8,
        done: u64,
        total: u64,
    },
    Ready,
    Failed(String),
}

/// 界面上展示的阶段。`ModelDownloader::tick` 把后台事件折进这里,UI 只读快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Downloading {
        file: String,
        idx: usize,
        count: usize,
        pct: u8,
        done: u64,
        total: u64,
    },
    Ready,
    Failed(String),
}

impl Phase {
    pub fn label(&self) -> String {
        match self {
            Phase::Idle => String::new(),
            Phase::Downloading {
                file,
                idx,
                count,
                pct,
                done,
                total,
            } => {
                let mb = |n: u64| format!("{:.1} MB", n as f64 / 1_048_576.0);
                let which = format!("(第 {idx}/{count} 个)");
                if *total > 0 {
                    format!(
                        "正在下载 {file}… {pct}% ({}/{}) {which}",
                        mb(*done),
                        mb(*total)
                    )
                } else {
                    format!("正在下载 {file}… 已收到 {} {which}", mb(*done))
                }
            }
            Phase::Ready => "人脸模型已就位".into(),
            Phase::Failed(e) => format!("下载失败:{e}"),
        }
    }

    pub fn working(&self) -> bool {
        matches!(self, Phase::Downloading { .. })
    }

    /// 状态栏与提示窗上的一句话;无事可报时不给。
    pub fn status_hint(&self) -> Option<String> {
        match self {
            Phase::Ready => Some("人脸模型已就位,可以开始人脸识别".into()),
            Phase::Failed(e) => Some(format!("人脸模型下载失败:{e}")),
            _ => None,
        }
    }
}

pub struct ModelDownloader {
    cmd: Sender<Cmd>,
    events: Receiver<Event>,
    /// 配置文件快照(下载地址与路径都要显示给用户)。
    pub cfg: Config,
    pub phase: Phase,
    /// 首启提示窗:缺模型且配了下载源时置位,用户表过态即关。
    pub prompt_open: bool,
    stop: Arc<AtomicBool>,
}

impl ModelDownloader {
    pub fn new() -> ModelDownloader {
        let cfg = crate::update::load_config();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (ev_tx, ev_rx) = mpsc::channel::<Event>();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let _ = std::thread::Builder::new()
            .name("maliang-models".into())
            .spawn(move || worker(cmd_rx, ev_tx, worker_stop));
        ModelDownloader {
            cmd: cmd_tx,
            events: ev_rx,
            cfg,
            phase: Phase::Idle,
            prompt_open: false,
            stop,
        }
    }

    /// 当前方案缺模型、且服务端下载可用时,把首启提示窗打开。返回是否缺模型。
    pub fn check_on_startup(&mut self, algo: FaceAlgo) -> bool {
        let dir = match crate::ai::models_dir() {
            Some(d) => d,
            None => return false,
        };
        let missing = !missing_files(algo, &dir).is_empty();
        self.prompt_open = missing && downloadable(algo) && !self.model_base().is_empty();
        missing
    }

    /// 生效的模型目录地址(去空白)。空 = 不提供下载。
    pub fn model_base(&self) -> &str {
        self.cfg.model_url.trim()
    }

    /// 手动/提示窗触发下载:先重读配置,好让用户改完 ModelURL 就能直接点。
    pub fn start(&mut self, algo: FaceAlgo) {
        self.cfg = crate::update::load_config();
        if self.phase.working() {
            return;
        }
        if self.model_base().is_empty() {
            self.phase = Phase::Failed("没有可用的下载地址".into());
            return;
        }
        if !downloadable(algo) {
            self.phase = Phase::Failed("该方案的模型许可不允许程序代下载".into());
            return;
        }
        // 先把界面切到「在跑」并报出第一个文件名,否则点了按钮到后台第一帧
        // 进度事件之间界面是空的,看着像没反应。
        let dir = crate::ai::models_dir();
        let missing = dir
            .as_ref()
            .map(|d| missing_files(algo, d))
            .unwrap_or_default();
        let count = missing.len().max(1);
        self.phase = Phase::Downloading {
            file: missing.into_iter().next().unwrap_or_default(),
            idx: 1,
            count,
            pct: 0,
            done: 0,
            total: 0,
        };
        let _ = self.cmd.send(Cmd::Download {
            algo,
            cfg: Box::new(self.cfg.clone()),
        });
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.cmd.send(Cmd::Stop);
    }

    /// 每帧一泵:收后台事件,在下载期间继续要重绘帧。
    pub fn tick(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.events.try_recv() {
            self.on_event(ev);
        }
        if self.phase.working() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Downloading {
                file,
                idx,
                count,
                pct,
                done,
                total,
            } => {
                self.phase = Phase::Downloading {
                    file,
                    idx,
                    count,
                    pct,
                    done,
                    total,
                }
            }
            Event::Ready => self.phase = Phase::Ready,
            Event::Failed(e) => self.phase = Phase::Failed(e),
        }
    }
}

impl Drop for ModelDownloader {
    fn drop(&mut self) {
        self.stop();
    }
}

fn worker(rx: Receiver<Cmd>, tx: Sender<Event>, stop: Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match rx.recv() {
            Ok(Cmd::Stop) => break,
            Ok(Cmd::Download { algo, cfg }) => {
                let dir = crate::ai::models_dir();
                let out = match dir {
                    Some(d) => fetch_all(&d, &cfg, algo, &tx, &stop, verify_onnx),
                    None => Err("没有数据目录,模型没地方放".to_string()),
                };
                match out {
                    Ok(()) => {
                        let _ = tx.send(Event::Ready);
                    }
                    Err(e) => {
                        if !stop.load(Ordering::Relaxed) {
                            let _ = tx.send(Event::Failed(e));
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
}

/// 下齐该方案缺的模型文件。`dir` 与 `verify` 可替换,便于在本地 HTTP 服务上
/// 完整跑通「分段 / 续传 / 定名」而不碰真模型目录。
fn fetch_all(
    dir: &Path,
    cfg: &Config,
    algo: FaceAlgo,
    tx: &Sender<Event>,
    stop: &AtomicBool,
    verify: fn(&Path) -> Result<(), String>,
) -> Result<(), String> {
    if !downloadable(algo) {
        return Err("该方案的模型许可不允许程序代下载,请自行放置文件".into());
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("建模型目录失败: {e}"))?;
    let names = missing_files(algo, dir);
    let count = names.len();
    for (i, name) in names.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            return Err("已取消下载".into());
        }
        let url = file_url(&cfg.model_url, name)?;
        download_one(dir, cfg, &url, name, i + 1, count, tx, stop, verify)?;
    }
    Ok(())
}

/// 单个模型文件:按 Range 分段取到 `名字.part`,校验通过后改名定名。
#[allow(clippy::too_many_arguments)]
fn download_one(
    dir: &Path,
    cfg: &Config,
    url: &str,
    name: &str,
    idx: usize,
    count: usize,
    tx: &Sender<Event>,
    stop: &AtomicBool,
    verify: fn(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let target = dir.join(name);
    let part = dir.join(format!("{name}.part"));
    let chunk = cfg.chunk_bytes.min(MAX_MODEL);
    // 时间预算按段大小算:约 5 KB/s 的下限,慢网不会因一个固定总超时全盘重来。
    let body_secs = (chunk / (5 * 1024) + 30).clamp(60, 900);
    let global_secs = body_secs + CONNECT_SECS + 30;
    let mut have = std::fs::metadata(&part).map(|x| x.len()).unwrap_or(0);
    if have > MAX_MODEL {
        let _ = std::fs::remove_file(&part);
        have = 0;
    }
    let mut total = 0u64;
    let mut attempts = 0u32;
    let (mut last_pct, mut last_bytes) = (-1i32, 0u64);
    report(
        tx,
        name,
        idx,
        count,
        have,
        total,
        &mut last_pct,
        &mut last_bytes,
    );

    loop {
        if stop.load(Ordering::Relaxed) {
            return Err("已取消下载".into());
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
        let agent = crate::update::http_agent(cfg, global_secs, Some(body_secs));
        let resp = agent
            .get(url)
            .header("Range", format!("bytes={have}-{end}"))
            // 模型字节要原样落盘,不能让传输层压缩/改写。
            .header("Accept-Encoding", "identity")
            .call();
        let mut resp = match resp {
            Ok(r) => r,
            Err(e) => {
                // 服务端没有这个文件(或不让取)是终态,重试只是白等四段。
                if let ureq::Error::StatusCode(code) = &e {
                    if (400..=499).contains(code) && *code != 408 {
                        return Err(format!("模型文件返回 HTTP {code}"));
                    }
                }
                attempts += 1;
                if attempts >= MAX_SEGMENT_ATTEMPTS {
                    return Err(format!(
                        "下载连续 {attempts} 次失败: {e}(已保留 {have} 字节,再点下载可续传)"
                    ));
                }
                if !sleep_or_stop(attempts, stop) {
                    return Err("已取消下载".into());
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
                return Err(format!(
                    "服务端 Range 起点不符(请求 {have},返回 {start}),已保留已下部分"
                ));
            }
            if cr_total > 0 {
                total = cr_total;
            }
            let bytes = resp
                .body_mut()
                .with_config()
                .limit(chunk + 1)
                .read_to_vec()
                .map_err(|e| format!("读取模型内容失败: {e}"))?;
            if bytes.len() as u64 > want {
                return Err("服务端返回超出请求范围的数据,已丢弃本段".into());
            }
            append_all(&part, &bytes)?;
            have += bytes.len() as u64;
            attempts = 0;
            report(
                tx,
                name,
                idx,
                count,
                have,
                total,
                &mut last_pct,
                &mut last_bytes,
            );
            // 短于请求长度 = 已经是文件末尾;再请求只会拿到空段。
            if (bytes.len() as u64) < want {
                break;
            }
        } else if status.is_success() {
            // 服务端不认 Range,整包返回:清掉之前的分段,一次性流式落盘。
            let _ = std::fs::remove_file(&part);
            have = 0;
            total = clen;
            let mut file =
                std::fs::File::create(&part).map_err(|e| format!("建下载文件失败: {e}"))?;
            let mut reader = resp.into_body().into_reader().take(MAX_MODEL + 1);
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(n) => n,
                    Err(e) => {
                        return Err(format!("下载中断: {e}(服务端不支持 Range,需重新完整下载)"))
                    }
                };
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n])
                    .map_err(|e| format!("写下载文件失败: {e}"))?;
                have += n as u64;
                if have > MAX_MODEL {
                    return Err("模型超过 1 GB 上限,已停止下载".into());
                }
                report(
                    tx,
                    name,
                    idx,
                    count,
                    have,
                    total,
                    &mut last_pct,
                    &mut last_bytes,
                );
            }
            let _ = file.sync_all();
            break;
        } else {
            return Err(format!("模型文件返回 HTTP {status}"));
        }
    }

    if have == 0 {
        let _ = std::fs::remove_file(&part);
        return Err("服务端没有这个模型文件".into());
    }
    if let Err(e) = verify(&part) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    // 定名:同目录同名只可能是另一个进程刚放好的文件,直接覆盖成刚校验过的这份。
    if let Err(e) = move_into(&part, &target) {
        return Err(format!("模型定名失败: {e}"));
    }
    Ok(())
}

fn move_into(part: &Path, target: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(target);
    std::fs::rename(part, target)
}

/// 解析 `Content-Range: bytes 100-199/5000`,返回 (起点, 总长)。
fn content_range(v: &str) -> Option<(u64, u64)> {
    let spec = v.trim().strip_prefix("bytes ").unwrap_or_else(|| v.trim());
    let (span, total) = spec.trim().split_once('/')?;
    let (start, _) = span.split_once('-')?;
    Some((start.trim().parse().ok()?, total.trim().parse().ok()?))
}

fn append_all(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("续写下载文件失败: {e}"))?;
    f.write_all(bytes)
        .map_err(|e| format!("写下载文件失败: {e}"))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn report(
    tx: &Sender<Event>,
    file: &str,
    idx: usize,
    count: usize,
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
            file: file.to_string(),
            idx,
            count,
            pct: pct as u8,
            done,
            total,
        });
    }
}

/// 段间退避;返回 false 表示用户在这期间点了取消。
fn sleep_or_stop(attempt: u32, stop: &AtomicBool) -> bool {
    let ms = (1u64 << attempt.min(5)) * 250;
    let until = Instant::now() + Duration::from_millis(ms);
    loop {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(Duration::from_millis(200)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("maliang-models-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        let _ = std::fs::create_dir_all(&p);
        p
    }

    /// 假校验:只认「非空」,把 onnxruntime 挡在单测之外。
    fn accept_any(p: &Path) -> Result<(), String> {
        if std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false) {
            Ok(())
        } else {
            Err("空文件".into())
        }
    }

    /// 假校验:一律判不通过,用来验「校验失败不留半成品」。
    fn reject_any(_p: &Path) -> Result<(), String> {
        Err("不是可加载的模型".into())
    }

    type Reply = (u16, Vec<(String, String)>, Vec<u8>);
    type Handler = Box<dyn Fn(&str, &BTreeMap<String, String>) -> Reply + Send>;

    struct TestServer {
        port: u16,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn serve(handler: Handler) -> TestServer {
        use std::io::BufRead;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
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
                            if reader.read_line(&mut l).unwrap_or(0) == 0 || l.trim_end().is_empty()
                            {
                                break;
                            }
                            if let Some((k, v)) = l.split_once(':') {
                                hdrs.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                            }
                        }
                        let (code, extra, body) = handler(line.trim(), &hdrs);
                        let reason = match code {
                            200 => "OK",
                            206 => "Partial Content",
                            404 => "Not Found",
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

    fn model_dir(port: u16) -> Config {
        Config {
            model_url: format!("http://127.0.0.1:{port}/models/"),
            proxy: "none".into(),
            chunk_bytes: 4096,
            ..Config::default()
        }
    }

    /// 按文件名给内容;`range` 为 false 时服务端不认 Range,一律整包 200。
    fn files_server(files: Vec<(String, Vec<u8>)>, range: bool) -> TestServer {
        let table: BTreeMap<String, Vec<u8>> = files.into_iter().collect();
        serve(Box::new(move |line, hdrs| {
            let name = line.split_whitespace().nth(1).unwrap_or("");
            let Some(all) = table.get(name) else {
                return (
                    404,
                    vec![("Content-Type".into(), "text/plain".into())],
                    Vec::new(),
                );
            };
            let total = all.len();
            if total == 0 {
                return (200, Vec::new(), Vec::new());
            }
            match (range, hdrs.get("range")) {
                (true, Some(r)) => {
                    let (s, e) = split_range(r, total);
                    let body = all[s..=e].to_vec();
                    (
                        206,
                        vec![("Content-Range".into(), format!("bytes {s}-{e}/{total}"))],
                        body,
                    )
                }
                _ => (200, Vec::new(), all.clone()),
            }
        }))
    }

    fn run_fetch(dir: &Path, cfg: &Config) -> Result<(), String> {
        let (tx, _rx) = mpsc::channel::<Event>();
        let stop = AtomicBool::new(false);
        fetch_all(dir, cfg, FaceAlgo::AdaFace, &tx, &stop, accept_any)
    }

    #[test]
    fn file_url_appends_name_to_model_dir() {
        let u = file_url("https://cdn.example.com/maliang/models/", "face_det.onnx").unwrap();
        assert_eq!(u, "https://cdn.example.com/maliang/models/face_det.onnx");
        // 结尾不带斜杠、带 CDN 签名查询串都要落到同一个目录。
        let v = file_url(
            "https://cdn.example.com/maliang/models?token=abc",
            "face_rec.onnx",
        )
        .unwrap();
        assert_eq!(v, "https://cdn.example.com/maliang/models/face_rec.onnx");
        assert!(file_url("ftp://host/models/", "a.onnx").is_err());
        assert!(file_url("https:///models/", "a.onnx").is_err());
    }

    #[test]
    fn missing_files_lists_absent_models() {
        let dir = tmp("missing");
        let miss = missing_files(FaceAlgo::AdaFace, &dir);
        assert_eq!(
            miss,
            vec!["face_det.onnx".to_string(), "face_rec.onnx".to_string()]
        );
        for f in &miss {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        assert!(missing_files(FaceAlgo::AdaFace, &dir).is_empty());
    }

    #[test]
    fn downloads_every_missing_file() {
        let srv = files_server(
            vec![
                ("/models/face_det.onnx".into(), vec![0x08u8; 9000]),
                ("/models/face_rec.onnx".into(), vec![7u8; 5000]),
            ],
            true,
        );
        let dir = tmp("full");
        run_fetch(&dir, &model_dir(srv.port)).unwrap();
        assert_eq!(
            std::fs::metadata(dir.join("face_det.onnx")).unwrap().len(),
            9000
        );
        assert_eq!(
            std::fs::metadata(dir.join("face_rec.onnx")).unwrap().len(),
            5000
        );
        assert!(!dir.join("face_det.onnx.part").exists());
    }

    #[test]
    fn resumes_from_leftover_part() {
        let body: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let srv = files_server(vec![("/models/face_det.onnx".into(), body.clone())], true);
        let dir = tmp("resume");
        // 上次中断留下的前半段:这次只补后半,定名后字节必须和原始内容一致。
        std::fs::write(dir.join("face_det.onnx.part"), &body[..4_000]).unwrap();
        std::fs::write(dir.join("face_rec.onnx"), b"already there").unwrap();
        let (tx, _rx) = mpsc::channel::<Event>();
        let stop = AtomicBool::new(false);
        fetch_all(
            &dir,
            &model_dir(srv.port),
            FaceAlgo::AdaFace,
            &tx,
            &stop,
            accept_any,
        )
        .unwrap();
        assert_eq!(std::fs::read(dir.join("face_det.onnx")).unwrap(), body);
    }

    #[test]
    fn falls_back_to_whole_body_without_range() {
        let srv = files_server(
            vec![("/models/face_det.onnx".into(), vec![3u8; 7_000])],
            false,
        );
        let dir = tmp("noranage");
        let (tx, _rx) = mpsc::channel::<Event>();
        let stop = AtomicBool::new(false);
        // 服务端不认 Range:一次 200 整包也要能定名(另一件已就位,只下这一件)。
        std::fs::write(dir.join("face_rec.onnx"), b"x").unwrap();
        fetch_all(
            &dir,
            &model_dir(srv.port),
            FaceAlgo::AdaFace,
            &tx,
            &stop,
            accept_any,
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(dir.join("face_det.onnx")).unwrap().len(),
            7_000
        );
    }

    #[test]
    fn rejects_file_failing_verify_and_keeps_nothing() {
        let srv = files_server(
            vec![("/models/face_det.onnx".into(), vec![0x08u8; 300])],
            true,
        );
        let dir = tmp("bad");
        std::fs::write(dir.join("face_rec.onnx"), b"x").unwrap();
        let (tx, _rx) = mpsc::channel::<Event>();
        let stop = AtomicBool::new(false);
        let err = fetch_all(
            &dir,
            &model_dir(srv.port),
            FaceAlgo::AdaFace,
            &tx,
            &stop,
            reject_any,
        )
        .unwrap_err();
        assert!(err.contains("不是可加载的模型"), "{err}");
        assert!(!dir.join("face_det.onnx").exists());
        assert!(!dir.join("face_det.onnx.part").exists());
    }

    #[test]
    fn empty_file_on_server_is_not_taken_as_model() {
        let srv = files_server(vec![("/models/face_det.onnx".into(), Vec::new())], true);
        let dir = tmp("empty");
        std::fs::write(dir.join("face_rec.onnx"), b"x").unwrap();
        let err = run_fetch(&dir, &model_dir(srv.port)).unwrap_err();
        assert!(err.contains("服务端没有这个模型文件"), "{err}");
        assert!(!dir.join("face_det.onnx").exists());
    }

    #[test]
    fn missing_server_entry_reports_http_status() {
        let srv = files_server(Vec::new(), true);
        let dir = tmp("gone");
        let err = run_fetch(&dir, &model_dir(srv.port)).unwrap_err();
        assert!(err.contains("HTTP 404"), "{err}");
    }

    #[test]
    fn non_redistributable_scheme_is_refused() {
        assert!(downloadable(FaceAlgo::AdaFace));
        assert!(!downloadable(FaceAlgo::AntelopeV2));
        let dir = tmp("refuse");
        let (tx, _rx) = mpsc::channel::<Event>();
        let stop = AtomicBool::new(false);
        let cfg = model_dir(8);
        let err = fetch_all(&dir, &cfg, FaceAlgo::AntelopeV2, &tx, &stop, accept_any).unwrap_err();
        assert!(err.contains("不允许程序代下载"), "{err}");
    }
}
