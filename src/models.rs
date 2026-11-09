//! 人脸模型就位:启动检查 + 服务端后台下载。
//!
//! 模型文件不进仓库:官方直链(Apache-2.0 可再分发件)内置,maliang.ini 的
//! `ModelURL` 作镜像;用户可在界面里测速并选定走哪个源。识别模型上百 MB,
//! 首启不自动开下 —— 只提示,用户点「下载」才联网;下载走后台线程分段续传,
//! 界面显示当前文件与进度(进度只在下载区显示一份,避免各方案行都转圈)。

use crate::i18n::{t, t_with};
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
/// SFace(YuNet / SFace 均 OpenCV Zoo,Apache-2.0 含权重)走内置官方直链
/// 可代下;AdaFace 识别件用内置的社区 ONNX 导出件(权重训练数据条款未
/// 闭合,是否使用由使用者自决);antelopev2 权重非商用,程序从不代下。
pub fn downloadable(algo: FaceAlgo) -> bool {
    matches!(algo, FaceAlgo::SFace | FaceAlgo::AdaFace)
}

/// 内置下载源(按文件名)。出厂即可下载,ModelURL 配了则可作镜像。
///
/// - YuNet / SFace:OpenCV Zoo 官方直链,Apache-2.0 含权重,可再分发;
/// - AdaFace 识别件:社区按官方权重导出的 ONNX(仓库 MIT;预处理为
///   BGR (x-127.5)/127.5,与本程序 AdaFace 路径一致),权重训练数据
///   (MS1MV2 等)条款未闭合,由使用者自行评估;
/// - antelopev2:权重许可不可再分发,无内置源,只能手动放置。
const YUNET_URL: &str = "https://media.githubusercontent.com/media/opencv/opencv_zoo/main/models/face_detection_yunet/face_detection_yunet_2023mar.onnx";
const SFACE_URL: &str = "https://media.githubusercontent.com/media/opencv/opencv_zoo/main/models/face_recognition_sface/face_recognition_sface_2021dec.onnx";
const ADAFACE_IR50_URL: &str =
    "https://github.com/yakhyo/adaface-onnx/releases/download/weights/adaface_ir_50.onnx";

pub fn builtin_url_for(name: &str) -> Option<&'static str> {
    // 测试里的本地 mock 服务器要独占下载源,设此环境变量让内置源让路。
    if std::env::var_os("MALIANG_NO_BUILTIN_MODELS").is_some() {
        return None;
    }
    match name {
        "face_det.onnx" => Some(YUNET_URL),
        "face_rec_sface.onnx" => Some(SFACE_URL),
        "face_rec.onnx" => Some(ADAFACE_IR50_URL),
        _ => None,
    }
}

/// 不可代下方案的手动获取说明(官方地址)。
pub fn manual_hint(algo: FaceAlgo) -> Option<&'static str> {
    match algo {
        FaceAlgo::AntelopeV2 => {
            Some("https://github.com/deepinsight/insightface/releases/download/v0.7/antelopev2.zip")
        }
        _ => None,
    }
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

/// 下载源选择:「自动」= 官方直链优先、镜像兜底;选定某源则只走该源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePref {
    Auto,
    Official,
    Mirror,
}

impl SourcePref {
    pub const SETTING: &'static str = "model_source_pref";

    pub fn from_setting(v: &str) -> SourcePref {
        match v {
            "official" => SourcePref::Official,
            "mirror" => SourcePref::Mirror,
            _ => SourcePref::Auto,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            SourcePref::Auto => "auto",
            SourcePref::Official => "official",
            SourcePref::Mirror => "mirror",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SourcePref::Auto => t("自动(官方优先,镜像兜底)"),
            SourcePref::Official => t("仅官方直链"),
            SourcePref::Mirror => t("仅镜像"),
        }
    }
}

/// 可选下载源清单(官方直链一项;配了 ModelURL 再加镜像一项)。
pub struct SourceInfo {
    /// 对应 `SourcePref::key()`(镜像/官方;「自动」不参与测速行)。
    pub key: &'static str,
    pub label: String,
    pub probe_url: String,
}

pub fn candidate_sources(cfg: &Config) -> Vec<SourceInfo> {
    let mut v = vec![SourceInfo {
        key: "official",
        label: t("官方直链(GitHub:OpenCV Zoo / AdaFace 社区导出)").into(),
        probe_url: YUNET_URL.to_string(),
    }];
    let base = cfg.model_url.trim();
    if !base.is_empty() {
        if let Ok(u) = file_url(base, "face_det.onnx") {
            v.push(SourceInfo {
                key: "mirror",
                label: t_with("镜像({base})", &[("base", base.to_string())]),
                probe_url: u,
            });
        }
    }
    v
}

/// 单个源的一次测速结论。
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    pub key: &'static str,
    pub label: String,
    pub ok: bool,
    pub ms: u64,
    /// 测得的吞吐(KB/s);拿不到内容长度时为 None。
    pub kbps: Option<f64>,
    pub err: Option<String>,
}

/// 测速:对源的代表文件发一个 256 KB 的 Range 请求,计时并算吞吐。
fn probe_one(cfg: &Config, s: &SourceInfo) -> ProbeResult {
    let mut r = ProbeResult {
        key: s.key,
        label: s.label.clone(),
        ok: false,
        ms: 0,
        kbps: None,
        err: None,
    };
    let agent = crate::update::http_agent(cfg, 30, Some(30));
    let t0 = Instant::now();
    let out = agent
        .get(&s.probe_url)
        .header("Range", "bytes=0-262143")
        // 只测传输,不让压缩干扰计时。
        .header("Accept-Encoding", "identity")
        .call();
    r.ms = t0.elapsed().as_millis() as u64;
    match out {
        Err(e) => r.err = Some(format!("{e}")),
        Ok(mut resp) => {
            let status = resp.status().as_u16();
            if status >= 400 {
                r.err = Some(format!("HTTP {status}"));
            } else {
                match resp
                    .body_mut()
                    .with_config()
                    .limit(262_144 + 1)
                    .read_to_vec()
                {
                    Err(e) => r.err = Some(t_with("读取失败: {e}", &[("e", e.to_string())])),
                    Ok(bytes) => {
                        r.ok = true;
                        let secs = r.ms.max(1) as f64 / 1000.0;
                        r.kbps = Some(bytes.len() as f64 / 1024.0 / secs);
                    }
                }
            }
        }
    }
    r
}

/// 把 `ModelURL`(服务端模型目录)与固定文件名展开成下载地址。
///
/// 只放行 http/https;目录写法不带结尾 `/` 时自动补,查询串与片段先剥掉。
pub fn file_url(base: &str, name: &str) -> Result<String, String> {
    let base = base.trim();
    let lower = base.to_ascii_lowercase();
    if lower.starts_with("file:") || base.contains("://") && !is_http(&lower) {
        return Err(t_with(
            "ModelURL 只支持 http/https:{base}",
            &[("base", base.to_string())],
        ));
    }
    let (scheme, rest) = base
        .split_once("://")
        .ok_or_else(|| t("ModelURL 不是合法的 http/https 目录地址").to_string())?;
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
        return Err(t("ModelURL 缺少主机名").into());
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
    let mut f = std::fs::File::open(path)
        .map_err(|e| t_with("打开模型文件失败: {e}", &[("e", e.to_string())]))?;
    let mut head = [0u8; 16];
    let n = f
        .read(&mut head)
        .map_err(|e| t_with("读取模型文件失败: {e}", &[("e", e.to_string())]))?;
    if n < 16 || head[0] != 0x08 {
        return Err(t("下载到的内容不是 ONNX 模型文件").into());
    }
    crate::ai::try_load(path).map(|_| ())
}

// ---------------------------------------------------------------- 后台下载

#[derive(Debug)]
enum Cmd {
    /// 命令自带配置快照与源偏好:用户改过 maliang.ini / 换过源都不必重启程序。
    Download {
        files: Vec<String>,
        pref: SourcePref,
        cfg: Box<Config>,
    },
    /// 兼容收摊路径;运行中的任务由共享的取消位打断。
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
                let which = t_with(
                    "(第 {idx}/{count} 个)",
                    &[("idx", idx.to_string()), ("count", count.to_string())],
                );
                if *total > 0 {
                    t_with(
                        "正在下载 {file}… {pct}% ({p1}/{p2}) {which}",
                        &[
                            ("file", file.to_string()),
                            ("pct", pct.to_string()),
                            ("p1", (mb(*done)).to_string()),
                            ("p2", (mb(*total)).to_string()),
                            ("which", which.to_string()),
                        ],
                    )
                } else {
                    t_with(
                        "正在下载 {file}… 已收到 {p1} {which}",
                        &[
                            ("file", file.to_string()),
                            ("p1", (mb(*done)).to_string()),
                            ("which", which.to_string()),
                        ],
                    )
                }
            }
            Phase::Ready => t("人脸模型已就位").into(),
            Phase::Failed(e) => t_with("下载失败:{e}", &[("e", e.to_string())]),
        }
    }

    pub fn working(&self) -> bool {
        matches!(self, Phase::Downloading { .. })
    }

    /// 状态栏与提示窗上的一句话;无事可报时不给。
    pub fn status_hint(&self) -> Option<String> {
        match self {
            Phase::Ready => Some(t("人脸模型已就位,可以开始人脸识别").into()),
            Phase::Failed(e) => Some(t_with("人脸模型下载失败:{e}", &[("e", e.to_string())])),
            _ => None,
        }
    }
}

/// 下载源测速的状态(UI 只读快照)。
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeState {
    Idle,
    Running,
    Done(Vec<ProbeResult>),
}

pub struct ModelDownloader {
    cmd: Sender<Cmd>,
    events: Receiver<Event>,
    /// 配置文件快照(下载地址与路径都要显示给用户)。
    pub cfg: Config,
    pub phase: Phase,
    /// 首启提示窗:缺模型且配了下载源时置位,用户表过态即关。
    pub prompt_open: bool,
    /// 测速状态与后台结果通道。
    pub probe: ProbeState,
    probe_rx: Option<Receiver<Vec<ProbeResult>>>,
    /// 程序退出(Drop)用的总闸;worker 收到后彻底收摊。
    stop: Arc<AtomicBool>,
    /// 只取消当前下载任务(切源重启/手动停止);worker 不退出,还能接新任务。
    cancel: Arc<AtomicBool>,
}

impl ModelDownloader {
    pub fn new() -> ModelDownloader {
        let cfg = crate::update::load_config();
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (ev_tx, ev_rx) = mpsc::channel::<Event>();
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_cancel = cancel.clone();
        let _ = std::thread::Builder::new()
            .name("maliang-models".into())
            .spawn(move || worker(cmd_rx, ev_tx, worker_stop, worker_cancel));
        ModelDownloader {
            cmd: cmd_tx,
            events: ev_rx,
            cfg,
            phase: Phase::Idle,
            prompt_open: false,
            probe: ProbeState::Idle,
            probe_rx: None,
            stop,
            cancel,
        }
    }

    /// 当前方案缺模型、且服务端下载可用时,把首启提示窗打开。返回是否缺模型。
    pub fn check_on_startup(&mut self, algo: FaceAlgo) -> bool {
        let dir = match crate::ai::models_dir() {
            Some(d) => d,
            None => return false,
        };
        let missing = !missing_files(algo, &dir).is_empty();
        self.prompt_open = missing && downloadable(algo);
        missing
    }

    /// 生效的模型目录地址(去空白)。空 = 不提供下载。
    pub fn model_base(&self) -> &str {
        self.cfg.model_url.trim()
    }

    /// 手动/提示窗触发下载:按用户勾选的文件列表下,先重读配置,
    /// 好让用户改完 ModelURL 就能直接点。已在位的文件自动跳过。
    pub fn start_files(&mut self, files: Vec<String>, pref: SourcePref) {
        self.cfg = crate::update::load_config();
        if self.phase.working() {
            return;
        }
        let dir = crate::ai::models_dir();
        let todo: Vec<String> = dir
            .as_ref()
            .map(|d| files.into_iter().filter(|f| !d.join(f).is_file()).collect())
            .unwrap_or_default();
        if todo.is_empty() {
            self.phase = Phase::Failed(t("所选模型文件都已就位,无需下载").into());
            return;
        }
        let count = todo.len();
        self.phase = Phase::Downloading {
            file: todo[0].clone(),
            idx: 1,
            count,
            pct: 0,
            done: 0,
            total: 0,
        };
        let _ = self.cmd.send(Cmd::Download {
            files: todo,
            pref,
            cfg: Box::new(self.cfg.clone()),
        });
    }

    /// 对候选源各发一个小请求测速,结果经后台线程送回 `probe`。
    pub fn start_probe(&mut self) {
        if matches!(self.probe, ProbeState::Running) {
            return;
        }
        self.cfg = crate::update::load_config();
        let sources = candidate_sources(&self.cfg);
        if sources.is_empty() {
            return;
        }
        let (tx, rx) = mpsc::channel::<Vec<ProbeResult>>();
        self.probe_rx = Some(rx);
        self.probe = ProbeState::Running;
        let cfg = self.cfg.clone();
        let _ = std::thread::Builder::new()
            .name("maliang-model-probe".into())
            .spawn(move || {
                let res = sources
                    .iter()
                    .map(|s| probe_one(&cfg, s))
                    .collect::<Vec<_>>();
                let _ = tx.send(res);
            });
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.cmd.send(Cmd::Stop);
    }

    /// 停掉当前下载任务但保留 worker:断点(.part)留在原地,接着就能续传。
    pub fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.phase = Phase::Idle;
    }

    /// 每帧一泵:收后台事件(下载 + 测速),在忙时继续要重绘帧。
    pub fn tick(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.events.try_recv() {
            self.on_event(ev);
        }
        if let Some(rx) = &self.probe_rx {
            if let Ok(res) = rx.try_recv() {
                self.probe = ProbeState::Done(res);
                self.probe_rx = None;
            }
        }
        if self.phase.working() || matches!(self.probe, ProbeState::Running) {
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

fn worker(rx: Receiver<Cmd>, tx: Sender<Event>, stop: Arc<AtomicBool>, cancel: Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match rx.recv() {
            // 空闲期收到取消:当前没有任务在跑,直接忽略(标志位由下个任务开工时清零)。
            Ok(Cmd::Stop) => continue,
            Ok(Cmd::Download { files, pref, cfg }) => {
                // 新任务开工前先清取消位:让「取消旧任务 → 立刻重启」的连招能落地。
                cancel.store(false, Ordering::Relaxed);
                let dir = crate::ai::models_dir();
                let out = match dir {
                    Some(d) => fetch_files(&d, &cfg, &files, pref, &tx, &cancel, verify_onnx),
                    None => Err(t("没有数据目录,模型没地方放").to_string()),
                };
                match out {
                    Ok(()) => {
                        if !cancel.load(Ordering::Relaxed) {
                            let _ = tx.send(Event::Ready);
                        }
                    }
                    Err(e) => {
                        if !stop.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
                            let _ = tx.send(Event::Failed(e));
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
}

/// 逐个下齐 `files` 列出的模型文件。`dir` 与 `verify` 可替换,便于在本地
/// HTTP 服务上完整跑通「分段 / 续传 / 定名」而不碰真模型目录。
fn fetch_files(
    dir: &Path,
    cfg: &Config,
    files: &[String],
    pref: SourcePref,
    tx: &Sender<Event>,
    stop: &AtomicBool,
    verify: fn(&Path) -> Result<(), String>,
) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| t_with("建模型目录失败: {e}", &[("e", e.to_string())]))?;
    let count = files.len();
    for (i, name) in files.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            return Err(t("已取消下载").into());
        }
        // 候选源按用户偏好来:「自动」= 内置官方直链在前、ModelURL(镜像)兜底;
        // 选定单源则只用该源。404 才换源(自动档),其余错误(断网/取消)当场
        // 终止,续传字节留给下一次。
        let mut urls: Vec<String> = Vec::new();
        if pref != SourcePref::Mirror {
            if let Some(u) = builtin_url_for(name) {
                urls.push(u.to_string());
            }
        }
        if pref != SourcePref::Official && !cfg.model_url.trim().is_empty() {
            if let Ok(u) = file_url(&cfg.model_url, name) {
                if !urls.iter().any(|x| x == &u) {
                    urls.push(u);
                }
            }
        }
        if urls.is_empty() {
            return Err(t_with(
                "模型 {name} 在所选下载源上不可用,请换源或手动放置",
                &[("name", name.to_string())],
            ));
        }
        for (si, url) in urls.iter().enumerate() {
            match download_one(dir, cfg, url, name, i + 1, count, tx, stop, verify) {
                Ok(()) => break,
                Err(e) => {
                    let try_next = e.contains("HTTP 404") && si + 1 < urls.len();
                    if !try_next {
                        return Err(e);
                    }
                    let _ = std::fs::remove_file(dir.join(format!("{name}.part")));
                }
            }
        }
    }
    Ok(())
}

/// 按方案下齐缺的模型文件(仅单测用;生产路径走 `fetch_files`)。
#[cfg(test)]
fn fetch_all(
    dir: &Path,
    cfg: &Config,
    algo: FaceAlgo,
    tx: &Sender<Event>,
    stop: &AtomicBool,
    verify: fn(&Path) -> Result<(), String>,
) -> Result<(), String> {
    if !downloadable(algo) {
        return Err(t("该方案的模型许可不允许程序代下载,请自行放置文件").into());
    }
    let names = missing_files(algo, dir);
    fetch_files(dir, cfg, &names, SourcePref::Auto, tx, stop, verify)
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
            return Err(t("已取消下载").into());
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
                        return Err(t_with(
                            "模型文件返回 HTTP {code}",
                            &[("code", code.to_string())],
                        ));
                    }
                }
                attempts += 1;
                if attempts >= MAX_SEGMENT_ATTEMPTS {
                    return Err(t_with(
                        "下载连续 {attempts} 次失败: {e}(已保留 {have} 字节,再点下载可续传)",
                        &[
                            ("attempts", attempts.to_string()),
                            ("e", e.to_string()),
                            ("have", have.to_string()),
                        ],
                    ));
                }
                if !sleep_or_stop(attempts, stop) {
                    return Err(t("已取消下载").into());
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
                return Err(t_with(
                    "服务端 Range 起点不符(请求 {have},返回 {start}),已保留已下部分",
                    &[("have", have.to_string()), ("start", start.to_string())],
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
                .map_err(|e| t_with("读取模型内容失败: {e}", &[("e", e.to_string())]))?;
            if bytes.len() as u64 > want {
                return Err(t("服务端返回超出请求范围的数据,已丢弃本段").into());
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
            let mut file = std::fs::File::create(&part)
                .map_err(|e| t_with("建下载文件失败: {e}", &[("e", e.to_string())]))?;
            let mut reader = resp.into_body().into_reader().take(MAX_MODEL + 1);
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(n) => n,
                    Err(e) => {
                        return Err(t_with(
                            "下载中断: {e}(服务端不支持 Range,需重新完整下载)",
                            &[("e", e.to_string())],
                        ))
                    }
                };
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n])
                    .map_err(|e| t_with("写下载文件失败: {e}", &[("e", e.to_string())]))?;
                have += n as u64;
                if have > MAX_MODEL {
                    return Err(t("模型超过 1 GB 上限,已停止下载").into());
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
            return Err(t_with(
                "模型文件返回 HTTP {status}",
                &[("status", status.to_string())],
            ));
        }
    }

    if have == 0 {
        let _ = std::fs::remove_file(&part);
        return Err(t("服务端没有这个模型文件").into());
    }
    if let Err(e) = verify(&part) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    // 定名:同目录同名只可能是另一个进程刚放好的文件,直接覆盖成刚校验过的这份。
    if let Err(e) = move_into(&part, &target) {
        return Err(t_with("模型定名失败: {e}", &[("e", e.to_string())]));
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
        .map_err(|e| t_with("续写下载文件失败: {e}", &[("e", e.to_string())]))?;
    f.write_all(bytes)
        .map_err(|e| t_with("写下载文件失败: {e}", &[("e", e.to_string())]))?;
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
        std::env::set_var("MALIANG_NO_BUILTIN_MODELS", "1");
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
