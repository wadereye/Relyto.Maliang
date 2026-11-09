//! AI 模型槽位:目录布局、清单登记、导入校验、替换与滚回、后台下载。

use crate::ai_queue::Stage;
use crate::db::Store;
use crate::i18n::{t, t_with};
use anyhow::Result;
use rusqlite::params;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

/// 一个模型槽位的静态说明。
pub struct SlotSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub license: &'static str,
    /// 无可验证直链或许可不允许代分发时只能手动导入。
    pub manual: bool,
    /// 版本目录内必须齐备的文件名,首个是主文件。
    pub files: &'static [&'static str],
    /// 旧版平铺文件名,槽位目录缺失时回退。
    pub legacy: &'static [&'static str],
    /// 该槽位参与哪个阶段的版本签名。
    pub stage: Option<Stage>,
}

/// 全部槽位。
pub const SLOTS: [SlotSpec; 8] = [
    SlotSpec {
        key: "clip_vision",
        label: "MobileCLIP2-S2 视觉",
        license: "Apple 模型许可",
        manual: false,
        files: &["vision.onnx"],
        legacy: &["clip_vision.onnx"],
        stage: Some(Stage::Clip),
    },
    SlotSpec {
        key: "clip_text",
        label: "MobileCLIP2-S2 文本",
        license: "Apple 模型许可",
        manual: false,
        files: &["text.onnx", "merges.txt"],
        legacy: &["clip_text.onnx", "clip_merges.txt"],
        stage: Some(Stage::Clip),
    },
    SlotSpec {
        key: "mt_zh_en",
        label: "Opus-MT 中→英",
        license: "CC-BY-4.0",
        manual: true,
        files: &["encoder.onnx", "decoder.onnx", "vocab.txt"],
        legacy: &["mt_encoder.onnx", "mt_decoder.onnx", "mt_vocab.txt"],
        stage: None,
    },
    SlotSpec {
        key: "ocr_det",
        label: "文字检测",
        license: "Apache-2.0",
        manual: false,
        files: &["det.onnx"],
        legacy: &["ch_PP-OCRv4_det_infer.onnx"],
        stage: Some(Stage::Ocr),
    },
    SlotSpec {
        key: "ocr_cls",
        label: "文字方向",
        license: "Apache-2.0",
        manual: false,
        files: &["cls.onnx"],
        legacy: &["ch_ppocr_mobile_v2.0_cls_infer.onnx"],
        stage: Some(Stage::Ocr),
    },
    SlotSpec {
        key: "ocr_rec",
        label: "文字识别",
        license: "Apache-2.0",
        manual: false,
        files: &["rec.onnx", "ppocr_keys_v1.txt"],
        legacy: &["ch_PP-OCRv4_rec_infer.onnx", "ppocr_keys_v1.txt"],
        stage: Some(Stage::Ocr),
    },
    SlotSpec {
        key: "llm_parse",
        label: "查询解析(可选)",
        license: "Apache-2.0",
        manual: true,
        files: &["model.onnx"],
        legacy: &[],
        stage: None,
    },
    SlotSpec {
        key: "face_det",
        label: "人脸检测 / 识别",
        license: "Apache-2.0",
        manual: true,
        files: &["face_det.onnx", "face_rec_sface.onnx"],
        legacy: &["face_det.onnx", "face_rec_sface.onnx"],
        stage: None,
    },
];

/// 按槽位键取静态说明。
pub fn spec(slot: &str) -> Option<&'static SlotSpec> {
    SLOTS.iter().find(|s| s.key == slot)
}

/// 槽位内单个文件的可下载来源与校验信息。
pub struct ModelAsset {
    pub file: &'static str,
    /// 官方直链;空串表示只能从镜像目录(ModelURL)取。
    pub url: &'static str,
    /// 期望字节数;0 表示未知。
    pub bytes: u64,
    /// 期望 SHA256;空串表示只校验大小。
    pub sha256: &'static str,
}

/// 一个槽位的产物清单。
pub struct AssetSlot {
    pub slot: &'static str,
    pub assets: &'static [ModelAsset],
}

/// 可下载产物(优先使用;空 `url` 表示官方只发布原始权重、ONNX 仅由镜像分发)。
///
/// MobileCLIP2-S2 官方只发布 `mobileclip2_s2.pt`,没有 ONNX;`vision.onnx` /
/// `text.onnx` 由该官方权重转换而来,因此只走镜像目录(ModelURL)。
pub const ASSETS: &[AssetSlot] = &[
    AssetSlot {
        slot: "clip_vision",
        assets: &[ModelAsset {
            file: "vision.onnx",
            url: "",
            bytes: 143076215,
            sha256: "3da728ff1adbd3490249ee603c1f502c7b2d6d9a99849a3bb03bb6137bc68518",
        }],
    },
    AssetSlot {
        slot: "clip_text",
        assets: &[
            ModelAsset {
                file: "text.onnx",
                url: "",
                bytes: 253885242,
                sha256: "c54d4c83ab810cd3b0689d7e54931022f276112123d5017277b843123a04cc4d",
            },
            ModelAsset {
                file: "merges.txt",
                url: "",
                bytes: 524619,
                sha256: "9fd691f7c8039210e0fced15865466c65820d09b63988b0174bfe25de299051a",
            },
        ],
    },
    AssetSlot {
        slot: "ocr_det",
        assets: &[ModelAsset {
            file: "det.onnx",
            url: "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/master/onnx/PP-OCRv4/det/ch_PP-OCRv4_det_mobile.onnx",
            bytes: 4745517,
            sha256: "d2a7720d45a54257208b1e13e36a8479894cb74155a5efe29462512d42f49da9",
        }],
    },
    AssetSlot {
        slot: "ocr_cls",
        assets: &[ModelAsset {
            file: "cls.onnx",
            url: "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/master/onnx/PP-OCRv4/cls/ch_ppocr_mobile_v2.0_cls_mobile.onnx",
            bytes: 585532,
            sha256: "e47acedf663230f8863ff1ab0e64dd2d82b838fceb5957146dab185a89d6215c",
        }],
    },
    AssetSlot {
        slot: "face_det",
        assets: &[
            ModelAsset {
                file: "face_det.onnx",
                url: "",
                bytes: 16923827,
                sha256: "5838f7fe053675b1c7a08b633df49e7af5495cee0493c7dcf6697200b85b5b91",
            },
            ModelAsset {
                file: "face_rec_sface.onnx",
                url: "",
                bytes: 38696353,
                sha256: "0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79",
            },
        ],
    },
    AssetSlot {
        slot: "ocr_rec",
        assets: &[
            ModelAsset {
                file: "rec.onnx",
                url: "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/master/onnx/PP-OCRv4/rec/ch_PP-OCRv4_rec_mobile.onnx",
                bytes: 10857958,
                sha256: "48fc40f24f6d2a207a2b1091d3437eb3cc3eb6b676dc3ef9c37384005483683b",
            },
            ModelAsset {
                file: "ppocr_keys_v1.txt",
                url: "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/master/paddle/PP-OCRv4/rec/ch_PP-OCRv4_rec_mobile/ppocr_keys_v1.txt",
                bytes: 26249,
                sha256: "28b2362ad4ab2dc38769aa72feb535e3a9ddb3fd2a7585a05920e6393b1dc7f7",
            },
        ],
    },
];

/// 取槽位内某文件的产物信息。
pub fn asset(slot: &str, file: &str) -> Option<&'static ModelAsset> {
    ASSETS
        .iter()
        .find(|s| s.slot == slot)?
        .assets
        .iter()
        .find(|a| a.file == file)
}

/// 镜像地址:ModelURL + 文件名;未配 ModelURL 时为空。
fn mirror_url(base: &str, file: &str) -> String {
    let base = base.trim();
    if base.is_empty() {
        return String::new();
    }
    crate::models::file_url(base, file).unwrap_or_default()
}

/// 文件的可下载地址:优先官方直链,其次镜像目录。
pub fn url_for(slot: &str, file: &str) -> String {
    if let Some(a) = asset(slot, file) {
        if !a.url.is_empty() {
            return a.url.to_string();
        }
    }
    mirror_url(&crate::update::load_config().model_url, file)
}

/// 槽位主文件的默认下载地址(官方优先,其次镜像);均不可用时为空。
pub fn default_url(slot: &str) -> String {
    let Some(spec) = spec(slot) else {
        return String::new();
    };
    url_for(slot, spec.files[0])
}

/// 同 [`default_url`],但由调用方给出镜像目录,便于测试与复用。
pub fn default_url_with(base: &str, slot: &str) -> String {
    let Some(spec) = spec(slot) else {
        return String::new();
    };
    let file = spec.files[0];
    if let Some(a) = asset(slot, file) {
        if !a.url.is_empty() {
            return a.url.to_string();
        }
    }
    mirror_url(base, file)
}

/// 生成槽位的逐文件下载计划(自动选源版本);`primary_url` 非空时覆盖主文件地址。
pub fn plans_for(slot: &str, version: &str, primary_url: &str) -> Vec<DownloadPlan> {
    plans_for_pref(slot, version, primary_url, crate::models::SourcePref::Auto)
}

/// 按全局下载源偏好生成逐文件计划。
///
/// 「自动」官方优先、镜像兜底;「仅官方」与「仅镜像」不做回退,
/// 该槽位没有对应来源的文件直接跳过(调用方据此提示用户换源)。
pub fn plans_for_pref(
    slot: &str,
    version: &str,
    primary_url: &str,
    pref: crate::models::SourcePref,
) -> Vec<DownloadPlan> {
    use crate::models::SourcePref;
    let Some(spec) = spec(slot) else {
        return Vec::new();
    };
    let base = crate::update::load_config().model_url;
    let mut out = Vec::with_capacity(spec.files.len());
    for (i, file) in spec.files.iter().enumerate() {
        let a = asset(slot, file);
        let official = a.map(|x| x.url).unwrap_or("");
        let mirror = mirror_url(&base, file);
        let auto = if official.is_empty() {
            mirror.clone()
        } else {
            official.to_string()
        };
        let builtin = auto.clone();
        let given = if i == 0 { primary_url.trim() } else { "" };
        // 用户填的若就是内置的官方/镜像地址,不算自定义,保留校验。
        let custom = !given.is_empty() && given != builtin && given != official && given != mirror;
        let url = if custom {
            given.to_string()
        } else {
            match pref {
                SourcePref::Auto => auto,
                SourcePref::Official => official.to_string(),
                SourcePref::Mirror => mirror,
            }
        };
        if url.is_empty() {
            continue;
        }
        out.push(DownloadPlan {
            slot: slot.to_string(),
            version: version.to_string(),
            file: (*file).to_string(),
            url,
            sha256: if custom {
                String::new()
            } else {
                a.map(|x| x.sha256.to_string()).unwrap_or_default()
            },
            bytes: if custom {
                0
            } else {
                a.map(|x| x.bytes).unwrap_or(0)
            },
        });
    }
    out
}

/// 官方权重参考链接(仅 .pt,程序不直接下载该格式)。
///
/// MobileCLIP2 官方只发布 PyTorch 权重,槽位里的 ONNX 是本项目转换后放到镜像上的;
/// 这里把官方地址展示给用户,方便自行核对或复现转换。
pub fn official_weight_links(slot: &str) -> &'static [(&'static str, &'static str)] {
    match slot {
        "clip_vision" | "clip_text" => &[
            (
                "Hugging Face",
                "https://huggingface.co/apple/MobileCLIP2-S2/resolve/main/mobileclip2_s2.pt",
            ),
            (
                "hf-mirror",
                "https://hf-mirror.com/apple/MobileCLIP2-S2/resolve/main/mobileclip2_s2.pt",
            ),
        ],
        _ => &[],
    }
}

/// 模型根目录(models/)。
pub fn root() -> Option<PathBuf> {
    crate::ai::models_dir()
}

/// 下载暂存目录(models/.staging/)。
pub fn staging(root: &Path) -> PathBuf {
    root.join(".staging")
}

/// 槽位目录(models/<slot>/)。
pub fn slot_dir(root: &Path, slot: &str) -> PathBuf {
    root.join(slot)
}

/// 版本目录(models/<slot>/<version>/)。
pub fn version_dir(root: &Path, slot: &str, version: &str) -> PathBuf {
    slot_dir(root, slot).join(version)
}

/// 已登记的模型行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRecord {
    pub slot: String,
    pub file: String,
    pub version: String,
    pub source: String,
    pub url: Option<String>,
    pub sha256: Option<String>,
    pub bytes: i64,
    pub license: String,
    pub dim: Option<i32>,
    pub input_name: Option<String>,
    pub installed_ts: i64,
}

fn record_from_row(r: &rusqlite::Row) -> rusqlite::Result<ModelRecord> {
    Ok(ModelRecord {
        slot: r.get(0)?,
        file: r.get(1)?,
        version: r.get(2)?,
        source: r.get(3)?,
        url: r.get(4)?,
        sha256: r.get(5)?,
        bytes: r.get(6)?,
        license: r.get(7)?,
        dim: r.get(8)?,
        input_name: r.get(9)?,
        installed_ts: r.get(10)?,
    })
}

/// 读出槽位登记行。
pub fn record(store: &Store, slot: &str) -> Option<ModelRecord> {
    store.read(|conn| {
        conn.query_row(
            "SELECT slot, file, version, source, url, sha256, bytes, license, dim, input_name, installed_ts
               FROM ai_models WHERE slot=?1",
            [slot],
            |r| record_from_row(r),
        )
        .ok()
    })
}

/// 全部登记行。
pub fn records(store: &Store) -> Vec<ModelRecord> {
    store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) = conn.prepare(
            "SELECT slot, file, version, source, url, sha256, bytes, license, dim, input_name, installed_ts
               FROM ai_models ORDER BY slot",
        ) else {
            return out;
        };
        if let Ok(rows) = st.query_map([], |r| record_from_row(r)) {
            out.extend(rows.flatten());
        }
        out
    })
}

/// 解析后的槽位:目录 + 是否走旧平铺路径 + 版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub dir: PathBuf,
    pub version: String,
    pub legacy: bool,
}

/// 在给定根目录下解析槽位:登记行优先,缺文件时回退旧平铺路径。
pub fn resolve_in(root: &Path, store: &Store, slot: &str) -> Option<Resolved> {
    let spec = spec(slot)?;
    if let Some(rec) = record(store, slot) {
        let p = root.join(&rec.file);
        if p.is_file() {
            let dir = p.parent()?.to_path_buf();
            let all = spec.files.iter().all(|f| dir.join(f).is_file());
            if all {
                return Some(Resolved {
                    dir,
                    version: rec.version,
                    legacy: false,
                });
            }
        }
    }
    let all = spec.legacy.iter().all(|f| root.join(f).is_file());
    if all && !spec.legacy.is_empty() {
        return Some(Resolved {
            dir: root.to_path_buf(),
            version: "legacy".into(),
            legacy: true,
        });
    }
    None
}

/// 当前根目录下解析槽位。
pub fn resolve(store: &Store, slot: &str) -> Option<Resolved> {
    resolve_in(&root()?, store, slot)
}

/// 槽位某文件的实际路径(按槽位约定顺序)。
pub fn file_path(store: &Store, slot: &str, file: &str) -> Option<PathBuf> {
    let r = resolve(store, slot)?;
    let p = r.dir.join(file);
    // 旧平铺模型没有新文件名,不校验存在会让 legacy 误判为新链路就绪。
    p.is_file().then_some(p)
}

/// 槽位版本签名;未安装时为 "none"。
pub fn slot_sig(store: &Store, slot: &str) -> String {
    resolve(store, slot)
        .map(|r| r.version)
        .unwrap_or_else(|| "none".into())
}

/// 某阶段依赖的槽位(标签阶段复用语义向量模型)。
pub fn stage_slots(stage: Stage) -> &'static [&'static str] {
    match stage {
        Stage::Clip => &["clip_vision", "clip_text"],
        Stage::Tags => &["clip_vision", "clip_text"],
        Stage::Ocr => &["ocr_det", "ocr_cls", "ocr_rec"],
    }
}

/// 阶段版本签名:参与该阶段的槽位版本拼接。
pub fn stage_signature(store: &Store, stage: Stage) -> Option<String> {
    let parts: Vec<String> = stage_slots(stage)
        .iter()
        .filter_map(|key| spec(key).map(|s| format!("{}={}", s.key, slot_sig(store, s.key))))
        .collect();
    if parts.is_empty() || parts.iter().all(|p| p.ends_with("=none")) {
        None
    } else if stage == Stage::Tags {
        let labels = crate::ai_tags::vocab_version(store);
        Some(format!("{}|labels={labels}", parts.join(",")))
    } else {
        Some(parts.join(","))
    }
}

/// 阶段可用于推理:必需槽位都已解析。
pub fn stage_ready(store: &Store, stage: Stage) -> bool {
    match stage {
        Stage::Clip => {
            // 只认新链路的产物文件名,旧平铺模型不算就绪。
            file_path(store, "clip_vision", "vision.onnx").is_some()
                && file_path(store, "clip_text", "text.onnx").is_some()
                && (file_path(store, "clip_text", "merges.txt").is_some()
                    || file_path(store, "clip_text", "clip_merges.txt").is_some())
        }
        Stage::Tags => stage_ready(store, Stage::Clip) && crate::ai_tags::label_count(store) > 0,
        Stage::Ocr => resolve(store, "ocr_det").is_some() && resolve(store, "ocr_rec").is_some(),
    }
}

/// 文件 SHA256(小写十六进制)。
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f =
        std::fs::File::open(path).map_err(|e| t_with("打开文件: {e}", &[("e", e.to_string())]))?;
    let mut hasher = hmac_sha256::Hash::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| t_with("读取文件: {e}", &[("e", e.to_string())]))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// 字节 SHA256(测试与校验共用)。
pub fn sha256_bytes(bytes: &[u8]) -> String {
    to_hex(&hmac_sha256::Hash::hash(bytes))
}

fn to_hex(digest: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 校验文件大小与 SHA256;空哈希表示只校验大小。
pub fn verify_file(path: &Path, expected_bytes: u64, sha256: &str) -> Result<String, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| t_with("读取大小: {e}", &[("e", e.to_string())]))?;
    if expected_bytes > 0 && meta.len() != expected_bytes {
        return Err(t_with(
            "大小不符:期望 {expected_bytes},实际 {p1}",
            &[
                ("expected_bytes", expected_bytes.to_string()),
                ("p1", (meta.len()).to_string()),
            ],
        ));
    }
    let got = sha256_file(path)?;
    if !sha256.trim().is_empty() && !got.eq_ignore_ascii_case(sha256.trim()) {
        return Err(t_with(
            "SHA256 不符:期望 {sha256},实际 {got}",
            &[("sha256", sha256.to_string()), ("got", got.to_string())],
        ));
    }
    Ok(got)
}

/// ONNX 头 + 输入输出签名(加载一次会话)。
pub fn inspect_onnx(path: &Path) -> Result<(Vec<String>, Vec<String>), String> {
    let mut head = [0u8; 16];
    let mut f =
        std::fs::File::open(path).map_err(|e| t_with("打开模型: {e}", &[("e", e.to_string())]))?;
    let n = f
        .read(&mut head)
        .map_err(|e| t_with("读取模型: {e}", &[("e", e.to_string())]))?;
    if n < 16 || head[0] != 0x08 {
        return Err(t("不是 ONNX 模型文件").into());
    }
    crate::ai::inspect(path)
}

/// 校验输入名与输出维度;不传的项不校验。
pub fn verify_slot_file(
    path: &Path,
    input_name: Option<&str>,
    dim: Option<i32>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let (inputs, outputs) = inspect_onnx(path)?;
    if let Some(name) = input_name {
        if !inputs.iter().any(|i| i == name) {
            return Err(t_with(
                "输入名不符:期望 {name},实际 {inputs:?}",
                &[
                    ("name", name.to_string()),
                    ("inputs", format!("{inputs:?}")),
                ],
            ));
        }
    }
    if let Some(d) = dim {
        let hit = crate::ai::output_dim(path).unwrap_or(0);
        if hit != d {
            return Err(t_with(
                "输出维度不符:期望 {d},实际 {hit}",
                &[("d", d.to_string()), ("hit", hit.to_string())],
            ));
        }
    }
    Ok((inputs, outputs))
}

/// 把导入的单个模型文件拷进槽位版本目录;校验失败不落地。
pub fn import_file(
    store: &Store,
    slot: &str,
    src: &Path,
    version: &str,
    dim: Option<i32>,
    input_name: Option<&str>,
) -> Result<ModelRecord, String> {
    let spec =
        spec(slot).ok_or_else(|| t_with("未知槽位 {slot}", &[("slot", slot.to_string())]))?;
    let root = root().ok_or(t("无数据目录"))?;
    if !src.is_file() {
        return Err(t("导入源不是文件").into());
    }
    let (_, outputs) = verify_slot_file(src, input_name, dim)?;
    let dir = version_dir(&root, slot, version);
    std::fs::create_dir_all(&dir).map_err(|e| t_with("建目录: {e}", &[("e", e.to_string())]))?;
    let name = spec.files[0];
    let dst = dir.join(name);
    let tmp = staging(&root).join(format!("{slot}-{name}.import"));
    std::fs::create_dir_all(staging(&root))
        .map_err(|e| t_with("建暂存目录: {e}", &[("e", e.to_string())]))?;
    std::fs::copy(src, &tmp).map_err(|e| t_with("复制到暂存: {e}", &[("e", e.to_string())]))?;
    verify_slot_file(&tmp, input_name, dim)?;
    std::fs::rename(&tmp, &dst).map_err(|e| t_with("落位失败: {e}", &[("e", e.to_string())]))?;
    let sha = sha256_file(&dst)?;
    let bytes = std::fs::metadata(&dst).map(|m| m.len() as i64).unwrap_or(0);
    let rel = format!("{slot}/{version}/{name}");
    let rec = ModelRecord {
        slot: slot.into(),
        file: rel,
        version: version.into(),
        source: "manual".into(),
        url: None,
        sha256: Some(sha),
        bytes,
        license: spec.license.into(),
        dim,
        input_name: input_name.map(str::to_string),
        installed_ts: crate::ai_queue::now_ms(),
    };
    upsert(store, &rec)?;
    let _ = outputs;
    Ok(rec)
}

/// 多文件槽位从目录整体导入(文件名必须与约定一致)。
pub fn import_dir(
    store: &Store,
    slot: &str,
    src: &Path,
    version: &str,
) -> Result<ModelRecord, String> {
    let spec =
        spec(slot).ok_or_else(|| t_with("未知槽位 {slot}", &[("slot", slot.to_string())]))?;
    let root = root().ok_or(t("无数据目录"))?;
    if !src.is_dir() {
        return Err(t("导入源不是目录").into());
    }
    let missing: Vec<&str> = spec
        .files
        .iter()
        .filter(|f| !src.join(f).is_file())
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(t_with(
            "目录缺文件:{missing:?}",
            &[("missing", format!("{missing:?}"))],
        ));
    }
    let dir = version_dir(&root, slot, version);
    std::fs::create_dir_all(&dir).map_err(|e| t_with("建目录: {e}", &[("e", e.to_string())]))?;
    for f in spec.files {
        let p = src.join(f);
        if f.ends_with(".onnx") {
            verify_slot_file(&p, None, None)?;
        }
        std::fs::copy(&p, dir.join(f)).map_err(|e| {
            t_with(
                "复制 {f}: {e}",
                &[("f", f.to_string()), ("e", e.to_string())],
            )
        })?;
    }
    let sha = sha256_file(&dir.join(spec.files[0]))?;
    let bytes = spec
        .files
        .iter()
        .filter_map(|f| std::fs::metadata(dir.join(f)).ok())
        .map(|m| m.len() as i64)
        .sum();
    let rec = ModelRecord {
        slot: slot.into(),
        file: format!("{slot}/{version}/{}", spec.files[0]),
        version: version.into(),
        source: "manual".into(),
        url: None,
        sha256: Some(sha),
        bytes,
        license: spec.license.into(),
        dim: None,
        input_name: None,
        installed_ts: crate::ai_queue::now_ms(),
    };
    upsert(store, &rec)?;
    Ok(rec)
}

/// 写一行登记(同槽位覆盖)。
pub fn upsert(store: &Store, rec: &ModelRecord) -> Result<(), String> {
    store.write(|conn| -> std::result::Result<(), String> {
            conn.execute(
                "INSERT INTO ai_models(slot,file,version,source,url,sha256,bytes,license,dim,input_name,installed_ts)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                 ON CONFLICT(slot) DO UPDATE SET
                   file=excluded.file, version=excluded.version, source=excluded.source,
                   url=excluded.url, sha256=excluded.sha256, bytes=excluded.bytes,
                   license=excluded.license, dim=excluded.dim, input_name=excluded.input_name,
                   installed_ts=excluded.installed_ts",
                params![
                    rec.slot,
                    rec.file,
                    rec.version,
                    rec.source,
                    rec.url,
                    rec.sha256,
                    rec.bytes,
                    rec.license,
                    rec.dim,
                    rec.input_name,
                    rec.installed_ts
                ],
            )
            .map_err(|e| e.to_string())?;
            Ok(())
        })
}

/// 槽位里已有的版本目录名(按名字排序)。
pub fn versions(store: &Store, slot: &str) -> Vec<String> {
    let _ = store;
    let Some(root) = root() else {
        return Vec::new();
    };
    let dir = slot_dir(&root, slot);
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// 切换启用版本(不删除旧目录,支持滚回)。
pub fn activate(store: &Store, slot: &str, version: &str) -> Result<(), String> {
    let spec =
        spec(slot).ok_or_else(|| t_with("未知槽位 {slot}", &[("slot", slot.to_string())]))?;
    let root = root().ok_or(t("无数据目录"))?;
    let dir = version_dir(&root, slot, version);
    if !spec.files.iter().all(|f| dir.join(f).is_file()) {
        return Err(t_with(
            "版本目录缺文件:{p1}",
            &[("p1", (dir.display()).to_string())],
        ));
    }
    let rec = ModelRecord {
        slot: slot.into(),
        file: format!("{slot}/{version}/{}", spec.files[0]),
        version: version.into(),
        source: record(store, slot)
            .map(|r| r.source)
            .unwrap_or_else(|| "manual".into()),
        url: None,
        sha256: None,
        bytes: spec
            .files
            .iter()
            .filter_map(|f| std::fs::metadata(dir.join(f)).ok())
            .map(|m| m.len() as i64)
            .sum(),
        license: spec.license.into(),
        dim: None,
        input_name: None,
        installed_ts: crate::ai_queue::now_ms(),
    };
    upsert(store, &rec)?;
    store
        .set_setting(&format!("ai_active:{slot}"), version)
        .map_err(|e| e.to_string())
}

/// 换模型后按固定顺序落地:drain → 释放会话 → 换版本 → 重建 → 恢复。
pub fn replace_model(
    store: &Arc<Store>,
    sched: &crate::ai_sched::AiSched,
    slot: &str,
    version: &str,
) -> Result<(), String> {
    let Some(spec) = spec(slot) else {
        return Err(t_with("未知槽位 {slot}", &[("slot", slot.to_string())]));
    };
    let Some(stage) = spec.stage else {
        activate(store, slot, version)?;
        return Ok(());
    };
    sched.pause();
    sched.drain(stage);
    sched.release(stage);
    let out = activate(store, slot, version)
        .and_then(|_| crate::ai_queue::enqueue_all_for(store, stage).map_err(|e| e.to_string()))
        .map(|_| ());
    sched.resume();
    out
}

/// 槽位展示状态。
#[derive(Debug, Clone)]
pub struct SlotStatus {
    pub key: String,
    pub label: String,
    pub license: String,
    pub manual: bool,
    pub installed: bool,
    pub version: String,
    pub bytes: u64,
    pub legacy: bool,
    pub active: String,
    pub versions: Vec<String>,
}

/// 汇总全部槽位的状态(离线可用,只读文件系统)。
pub fn status(store: &Store) -> Vec<SlotStatus> {
    SLOTS
        .iter()
        .map(|s| {
            let resolved = resolve(store, s.key);
            let bytes = resolved
                .as_ref()
                .map(|r| {
                    s.files
                        .iter()
                        .filter_map(|f| std::fs::metadata(r.dir.join(f)).ok())
                        .map(|m| m.len())
                        .sum()
                })
                .unwrap_or(0);
            let version = resolved
                .as_ref()
                .map(|r| r.version.clone())
                .unwrap_or_default();
            let active = store
                .get_setting(&format!("ai_active:{}", s.key))
                .or_else(|| record(store, s.key).map(|r| r.version))
                .unwrap_or_else(|| version.clone());
            SlotStatus {
                key: s.key.into(),
                label: t(s.label).into(),
                license: t(s.license).into(),
                manual: s.manual,
                installed: resolved.is_some(),
                version,
                bytes,
                legacy: resolved.map(|r| r.legacy).unwrap_or(false),
                active,
                versions: versions(store, s.key),
            }
        })
        .collect()
}

/// 模型目录总占用(含旧版本,不含暂存)。
pub fn disk_usage(store: &Store) -> u64 {
    let _ = store;
    fn walk(p: &Path) -> u64 {
        let Ok(rd) = std::fs::read_dir(p) else {
            return 0;
        };
        rd.flatten()
            .map(|e| {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().map(|n| n == ".staging").unwrap_or(false) {
                        0
                    } else {
                        walk(&p)
                    }
                } else {
                    e.metadata().map(|m| m.len()).unwrap_or(0)
                }
            })
            .sum()
    }
    root().map(|r| walk(&r)).unwrap_or(0)
}

// ---------------------------------------------------------------- 后台下载

/// 一份下载计划(清单驱动;url 与 sha256 都可留空以便本地测试)。
#[derive(Debug, Clone)]
pub struct DownloadPlan {
    pub slot: String,
    pub version: String,
    pub file: String,
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
}

/// 下载器对外的进度快照。
#[derive(Debug, Clone, PartialEq)]
pub enum DlPhase {
    Idle,
    Downloading { file: String, done: u64, total: u64 },
    Done,
    Failed(String),
}

impl DlPhase {
    /// 状态栏一句话;空闲时为空。
    pub fn label(&self) -> String {
        match self {
            DlPhase::Idle => String::new(),
            DlPhase::Downloading { file, done, total } => {
                let mb = |n: u64| format!("{:.1}MB", n as f64 / 1_048_576.0);
                if *total > 0 {
                    t_with(
                        "下载 {file} {p1}/{p2}",
                        &[
                            ("file", file.to_string()),
                            ("p1", (mb(*done)).to_string()),
                            ("p2", (mb(*total)).to_string()),
                        ],
                    )
                } else {
                    t_with(
                        "下载 {file} 已收到 {p1}",
                        &[("file", file.to_string()), ("p1", (mb(*done)).to_string())],
                    )
                }
            }
            DlPhase::Done => t("模型下载完成").into(),
            DlPhase::Failed(e) => t_with("模型下载失败:{e}", &[("e", e.to_string())]),
        }
    }

    /// 是否正在下载。
    pub fn working(&self) -> bool {
        matches!(self, DlPhase::Downloading { .. })
    }
}

enum Ev {
    Progress { file: String, done: u64, total: u64 },
    Done,
    Failed(String),
}

/// 单任务下载器:同一时间只跑一个,支持取消与断点续传。
pub struct SlotDownloader {
    tx: Sender<DownloadPlan>,
    rx: Receiver<Ev>,
    stop: Arc<AtomicBool>,
    pub phase: DlPhase,
}

impl SlotDownloader {
    /// 起后台线程与事件通道。
    pub fn new(store: Arc<Store>) -> Self {
        let (tx, cmd_rx) = mpsc::channel::<DownloadPlan>();
        let (ev_tx, rx) = mpsc::channel::<Ev>();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        std::thread::Builder::new()
            .name("ai-model-dl".into())
            .spawn(move || {
                crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);
                while let Ok(plan) = cmd_rx.recv() {
                    let _ = run_download(&store, &plan, &ev_tx, &s);
                }
            })
            .ok();
        SlotDownloader {
            tx,
            rx,
            stop,
            phase: DlPhase::Idle,
        }
    }

    /// 发起下载;已有任务在跑时拒绝。
    pub fn start(&mut self, plan: DownloadPlan) -> Result<(), String> {
        if self.phase.working() {
            return Err(t("已有下载任务在跑").into());
        }
        self.stop.store(false, Ordering::Relaxed);
        self.phase = DlPhase::Downloading {
            file: plan.file.clone(),
            done: 0,
            total: plan.bytes,
        };
        self.tx.send(plan).map_err(|e| e.to_string())
    }

    /// 取消当前任务(保留 .part 供续传)。
    pub fn cancel(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// 把后台事件折进状态快照,UI 每帧调用。
    pub fn tick(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            self.phase = match ev {
                Ev::Progress { file, done, total } => DlPhase::Downloading { file, done, total },
                Ev::Done => DlPhase::Done,
                Ev::Failed(e) => DlPhase::Failed(e),
            };
        }
    }
}

fn run_download(
    store: &Store,
    plan: &DownloadPlan,
    tx: &Sender<Ev>,
    stop: &AtomicBool,
) -> Result<(), String> {
    let root = root().ok_or(t("无数据目录"))?;
    let config = crate::update::load_config();
    let sdir = staging(&root);
    std::fs::create_dir_all(&sdir)
        .map_err(|e| t_with("建暂存目录: {e}", &[("e", e.to_string())]))?;
    let part = sdir.join(format!("{}-{}.part", plan.slot, plan.file));
    let mut done = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let mut total = plan.bytes.max(done);
    let mut attempts = 0u32;
    while total == 0 || done < total {
        if stop.load(Ordering::Relaxed) {
            let _ = tx.send(Ev::Failed(t("已取消").into()));
            return Err(t("已取消").into());
        }
        let end = if total > 0 {
            (done + 4 * 1024 * 1024 - 1).min(total - 1)
        } else {
            done + 4 * 1024 * 1024 - 1
        };
        let agent = crate::update::http_agent(&config, 30, Some(60));
        let resp = agent
            .get(&plan.url)
            .header("Range", format!("bytes={done}-{end}"))
            .header("Accept-Encoding", "identity")
            .call();
        let mut resp = match resp {
            Ok(r) => r,
            Err(e) => {
                attempts += 1;
                if attempts >= 4 {
                    let msg = t_with(
                        "请求失败: {e}(已保留 {done} 字节,可续传)",
                        &[("e", e.to_string()), ("done", done.to_string())],
                    );
                    let _ = tx.send(Ev::Failed(msg.clone()));
                    return Err(msg);
                }
                std::thread::sleep(std::time::Duration::from_millis(500 * attempts as u64));
                continue;
            }
        };
        let status = resp.status().as_u16();
        if status == 416 && total > 0 && done >= total {
            break;
        }
        if status >= 400 {
            let e = format!("HTTP {status}");
            let _ = tx.send(Ev::Failed(e.clone()));
            return Err(e);
        }
        if status == 200 && done > 0 {
            // 服务端不支持 Range,回退为整文件重下。
            let _ = std::fs::remove_file(&part);
            done = 0;
            continue;
        }
        // 未预置大小时,从 Content-Range 响应头补全总长度,避免越界后续传。
        if total == 0 {
            if let Some(cr) = resp
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
            {
                if let Some(t) = cr
                    .rsplit('/')
                    .next()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                {
                    total = t;
                }
            }
        }
        let limit = if status == 200 {
            8 * 1024 * 1024 * 1024u64
        } else {
            end - done + 2
        };
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|e| t_with("读取响应: {e}", &[("e", e.to_string())]))?;
        if bytes.is_empty() {
            break;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&part)
            .map_err(|e| t_with("打开暂存: {e}", &[("e", e.to_string())]))?;
        f.write_all(&bytes)
            .map_err(|e| t_with("写暂存: {e}", &[("e", e.to_string())]))?;
        done += bytes.len() as u64;
        attempts = 0;
        // 服务端忽略 Range 且未报长度:整包读完即视为完成。
        if total == 0 && status == 200 {
            total = done;
        }
        let _ = tx.send(Ev::Progress {
            file: plan.file.clone(),
            done,
            total: total.max(done),
        });
        if total > 0 && done >= total {
            break;
        }
    }
    let sha = verify_file(&part, plan.bytes, &plan.sha256)?;
    let dir = version_dir(&root, &plan.slot, &plan.version);
    std::fs::create_dir_all(&dir)
        .map_err(|e| t_with("建版本目录: {e}", &[("e", e.to_string())]))?;
    let dst = dir.join(&plan.file);
    std::fs::rename(&part, &dst).map_err(|e| t_with("落位: {e}", &[("e", e.to_string())]))?;
    if dst.extension().map(|e| e == "onnx").unwrap_or(false) {
        if let Err(e) = inspect_onnx(&dst) {
            let _ = std::fs::remove_file(&dst);
            let msg = t_with("下载内容校验失败: {e}", &[("e", e.to_string())]);
            let _ = tx.send(Ev::Failed(msg.clone()));
            return Err(msg);
        }
    }
    let spec = spec(&plan.slot).ok_or(t("未知槽位"))?;
    let bytes = std::fs::metadata(&dst).map(|m| m.len() as i64).unwrap_or(0);
    let rec = ModelRecord {
        slot: plan.slot.clone(),
        file: format!("{}/{}/{}", plan.slot, plan.version, plan.file),
        version: plan.version.clone(),
        source: "auto".into(),
        url: Some(plan.url.clone()),
        sha256: Some(sha),
        bytes,
        license: spec.license.into(),
        dim: None,
        input_name: None,
        installed_ts: crate::ai_queue::now_ms(),
    };
    upsert(store, &rec)?;
    let _ = tx.send(Ev::Done);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Store {
        Store::open_mem().unwrap()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "maliang-ai-models-{name}-{}",
            crate::ai_queue::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(sha256_bytes(b"abc").len(), 64);
    }

    #[test]
    fn rejects_non_onnx_bytes() {
        let dir = temp_dir("bad");
        let p = dir.join("bad.onnx");
        std::fs::write(&p, b"not a model at all").unwrap();
        assert!(inspect_onnx(&p).is_err());
    }

    #[test]
    fn resolve_prefers_slot_then_legacy() {
        let store = mem();
        let root = temp_dir("layout");
        let spec = super::spec("clip_vision").unwrap();
        // 旧平铺文件在位 → 回退。
        std::fs::write(root.join(spec.legacy[0]), b"x").unwrap();
        let r = resolve_in(&root, &store, "clip_vision").unwrap();
        assert!(r.legacy);
        assert_eq!(r.version, "legacy");
        // 登记新槽位后优先走新路径。
        let vdir = version_dir(&root, "clip_vision", "v2");
        std::fs::create_dir_all(&vdir).unwrap();
        std::fs::write(vdir.join(spec.files[0]), b"y").unwrap();
        let rec = ModelRecord {
            slot: "clip_vision".into(),
            file: format!("clip_vision/v2/{}", spec.files[0]),
            version: "v2".into(),
            source: "manual".into(),
            url: None,
            sha256: None,
            bytes: 1,
            license: "test".into(),
            dim: Some(512),
            input_name: None,
            installed_ts: 0,
        };
        upsert(&store, &rec).unwrap();
        let r = resolve_in(&root, &store, "clip_vision").unwrap();
        assert!(!r.legacy);
        assert_eq!(r.version, "v2");
    }

    #[test]
    fn stage_slots_cover_dependencies() {
        assert_eq!(stage_slots(Stage::Clip).len(), 2);
        assert_eq!(stage_slots(Stage::Tags).len(), 2, "标签复用语义模型");
        assert!(stage_slots(Stage::Ocr).contains(&"ocr_det"));
    }

    #[test]
    fn verify_file_checks_size_and_hash() {
        let dir = temp_dir("verify");
        let p = dir.join("a.bin");
        std::fs::write(&p, b"abc").unwrap();
        let ok = sha256_bytes(b"abc");
        assert!(verify_file(&p, 3, &ok).is_ok());
        assert!(verify_file(&p, 4, &ok).is_err());
        assert!(verify_file(&p, 3, "deadbeef").is_err());
        assert!(verify_file(&p, 0, "").is_ok());
    }

    #[test]
    fn default_url_prefers_official_then_mirror() {
        // 无官方直链的槽位回退镜像(ModelURL + 主文件名)。
        assert_eq!(
            default_url_with("https://cdn.example.com/models/", "face_det"),
            "https://cdn.example.com/models/face_det.onnx"
        );
        // 有官方直链的槽位不再依赖镜像配置。
        assert_eq!(
            default_url_with("https://cdn.example.com/models", "ocr_det"),
            "https://www.modelscope.cn/models/RapidAI/RapidOCR/resolve/master/onnx/PP-OCRv4/det/ch_PP-OCRv4_det_mobile.onnx"
        );
        assert!(default_url_with("  ", "face_det").is_empty());
        assert!(default_url_with("https://cdn.example.com/models/", "nope").is_empty());
    }

    #[test]
    fn plans_cover_all_slot_files_in_order() {
        let plans = plans_for("ocr_rec", "v1", "");
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].file, "rec.onnx");
        assert_eq!(plans[1].file, "ppocr_keys_v1.txt");
        assert!(plans.iter().all(|p| p.sha256.len() == 64 && p.bytes > 0));
        // 主文件自定义地址时不再套用官方校验值。
        let custom = plans_for("ocr_rec", "v1", "https://example.com/rec.onnx");
        assert_eq!(custom[0].url, "https://example.com/rec.onnx");
        assert!(custom[0].sha256.is_empty());
        assert_eq!(custom[1].url, plans[1].url);
    }

    #[test]
    fn source_pref_controls_official_and_mirror() {
        use crate::models::SourcePref;
        // 仅官方:有官方直链的槽位照常出计划,没有的直接跳过。
        let det = plans_for_pref("ocr_det", "v1", "", SourcePref::Official);
        assert_eq!(det.len(), 1);
        assert!(det[0].url.starts_with("https://www.modelscope.cn/"));
        assert!(plans_for_pref("clip_vision", "v1", "", SourcePref::Official).is_empty());
        // 自定义地址在「仅官方」下仍优先。
        let custom = plans_for_pref(
            "ocr_det",
            "v1",
            "https://example.com/det.onnx",
            SourcePref::Official,
        );
        assert_eq!(custom[0].url, "https://example.com/det.onnx");
        assert!(custom[0].sha256.is_empty());
    }

    #[test]
    fn manual_slots_match_download_capability() {
        use crate::models::SourcePref;
        for key in ["mt_zh_en", "llm_parse", "face_det"] {
            assert!(spec(key).unwrap().manual, "{key} 应标为手动放置");
            assert!(
                plans_for_pref(key, "v1", "", SourcePref::Official).is_empty(),
                "{key} 不应有官方直链"
            );
        }
        assert!(asset("face_det", "face_det.onnx").is_some());
        assert!(asset("face_det", "face_rec_sface.onnx").is_some());
        assert!(asset("mt_zh_en", "encoder.onnx").is_none());
        assert!(asset("llm_parse", "model.onnx").is_none());
    }

    #[test]
    fn official_assets_are_checksummed() {
        for slot in ASSETS {
            for a in slot.assets {
                assert!(a.bytes > 0, "{}/{} 缺字节数", slot.slot, a.file);
                assert_eq!(a.sha256.len(), 64, "{}/{} 缺 SHA256", slot.slot, a.file);
            }
        }
    }
}
