//! 模型功能(OCR/人脸/CLIP)。**默认启用**(`default = ["ai"]`);
//! `cargo build --no-default-features` 可关闭。
//! 运行时模型文件放数据目录 `models\`(跟随数据目录迁移),缺失时全部功能优雅不可用。

use ort::environment::Environment;
use ort::session::Session;
use std::sync::OnceLock;

pub fn models_dir() -> Option<std::path::PathBuf> {
    Some(crate::db::data_dir().join("models"))
}

static ENV: OnceLock<Result<Environment, String>> = OnceLock::new();

/// 进程级唯一 Environment;运行库不可用时返回错误,不终止进程。
pub(crate) fn env() -> Result<&'static Environment, String> {
    ENV.get_or_init(|| ort::init().build().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| e.clone())
}

/// 用 onnxruntime 加载一个模型,返回输入输出名(连通性冒烟/导入校验用)。
pub fn inspect(path: &std::path::Path) -> Result<(Vec<String>, Vec<String>), String> {
    let session = Session::builder(env()?)
        .map_err(|e| format!("会话构建: {e}"))?
        .commit_from_file(path)
        .map_err(|e| format!("加载模型: {e}"))?;
    let inputs: Vec<String> = session
        .inputs()
        .iter()
        .map(|i| i.name().to_string())
        .collect();
    let outputs: Vec<String> = session
        .outputs()
        .iter()
        .map(|o| o.name().to_string())
        .collect();
    Ok((inputs, outputs))
}

/// 首个张量输出的最后一维(语义嵌入维度校验用)。
pub fn output_dim(path: &std::path::Path) -> Option<i32> {
    let session = Session::builder(env().ok()?).ok()?.commit_from_file(path).ok()?;
    for o in session.outputs() {
        if let ort::value::ValueType::Tensor { shape, .. } = o.dtype() {
            if let Some(&d) = shape.last() {
                if d > 0 {
                    return Some(d as i32);
                }
            }
        }
    }
    None
}

/// 加载模型的签名串(排障用)。
#[allow(dead_code)]
pub fn try_load(path: &std::path::Path) -> Result<String, String> {
    let (inputs, outputs) = inspect(path)?;
    Ok(format!("{path:?}: inputs={inputs:?}, outputs={outputs:?}"))
}

/// 列出模型目录里已放置的 `.onnx` 模型文件名(无则空)。
///
/// UI 据此在语义面板上直说"模型目录还没有模型",而不是等用户点了索引才报错。
pub fn available_models() -> Vec<String> {
    models_dir()
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().map(|x| x == "onnx").unwrap_or(false))
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// OCR(PP-OCRv4 det + rec)。
pub mod ocr {
    use super::{env, models_dir};
    use ort::session::Session;
    use ort::value::Tensor;
    use std::collections::VecDeque;

    #[derive(Debug, Clone)]
    pub struct Txt {
        pub text: String,
        pub conf: f32,
        /// 文本框坐标(原图坐标系;预留:OCR 结果图上标注)。
        #[allow(dead_code)]
        pub x0: u32,
        #[allow(dead_code)]
        pub y0: u32,
        #[allow(dead_code)]
        pub x1: u32,
        #[allow(dead_code)]
        pub y1: u32,
    }

    pub struct Ocr {
        det: Session,
        rec: Session,
        dict: Vec<String>,
        cls: Option<Session>,
    }

    fn open_session(p: &std::path::Path) -> Result<Session, String> {
        Session::builder(env()?)
            .map_err(|e| e.to_string())?
            .commit_from_file(p)
            .map_err(|e| e.to_string())
    }

    impl Ocr {
        /// 按槽位目录加载(旧平铺文件自动回退)。
        pub fn load(store: &crate::db::Store) -> Result<Self, String> {
            let det = crate::ai_models::file_path(store, "ocr_det", "det.onnx")
                .or_else(|| {
                    crate::ai_models::file_path(store, "ocr_det", "ch_PP-OCRv4_det_infer.onnx")
                })
                .ok_or("缺少文字检测模型")?;
            let rec = crate::ai_models::file_path(store, "ocr_rec", "rec.onnx")
                .or_else(|| {
                    crate::ai_models::file_path(store, "ocr_rec", "ch_PP-OCRv4_rec_infer.onnx")
                })
                .ok_or("缺少文字识别模型")?;
            let dict_path = crate::ai_models::file_path(store, "ocr_rec", "ppocr_keys_v1.txt")
                .or_else(|| models_dir().map(|d| d.join("ppocr_keys_v1.txt")))
                .ok_or("缺少字符表")?;
            let cls = crate::ai_models::file_path(store, "ocr_cls", "cls.onnx")
                .or_else(|| models_dir().map(|d| d.join("ch_ppocr_mobile_v2.0_cls_infer.onnx")));
            Self::from_paths(&det, &rec, &dict_path, cls.as_deref())
        }

        /// 显式路径加载,便于单测与替换流程。
        pub fn from_paths(
            det: &std::path::Path,
            rec: &std::path::Path,
            dict_path: &std::path::Path,
            cls: Option<&std::path::Path>,
        ) -> Result<Self, String> {
            let det = open_session(det)?;
            let rec = open_session(rec)?;
            let cls = cls.filter(|p| p.is_file()).map(open_session).transpose()?;
            let dict = std::fs::read_to_string(dict_path)
                .map_err(|e| format!("读字符表: {e}"))?
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect();
            Ok(Self {
                det,
                rec,
                dict,
                cls,
            })
        }

        /// 文字行方向分类:返回 true 表示需要旋转 180 度。
        fn flipped(&mut self, crop: &image::RgbImage) -> bool {
            let Some(cls) = self.cls.as_mut() else {
                return false;
            };
            let rs = image::imageops::resize(crop, 192, 48, image::imageops::FilterType::Triangle);
            let mut data = Vec::with_capacity(192 * 48 * 3);
            for &c in &[2u8, 1, 0] {
                for p in rs.pixels() {
                    data.push((p.0[c as usize] as f32 / 255.0 - 0.5) / 0.5);
                }
            }
            let Ok(input) = Tensor::from_array(([1usize, 3, 48usize, 192usize], data)) else {
                return false;
            };
            let Ok(out) = cls.run(ort::inputs!["x" => &input]) else {
                return false;
            };
            let Some(v) = out.values().next() else {
                return false;
            };
            let Ok((_, logits)) = v.try_extract_tensor::<f32>() else {
                return false;
            };
            logits.len() >= 2 && logits[1] > logits[0]
        }

        fn det_map(
            &mut self,
            img: &image::DynamicImage,
        ) -> Result<(Vec<u32>, u32, u32, f32), String> {
            let (w, h) = (img.width(), img.height());
            // 短边缩到 640 附近,长边最多 960,只缩不放
            let scale = (640.0 / w.min(h) as f32)
                .min(960.0 / w.max(h) as f32)
                .min(1.0);
            let (rw0, rh0) = (
                (w as f32 * scale).max(1.0) as u32,
                (h as f32 * scale).max(1.0) as u32,
            );
            let pad32 = |v: u32| ((v + 31) / 32) * 32;
            let (fw, fh) = (pad32(rw0), pad32(rh0.max(32)));
            let rs = img
                .resize_exact(
                    rw0.max(1),
                    rh0.max(1),
                    image::imageops::FilterType::Triangle,
                )
                .to_rgb8();
            let mut canvas = image::RgbImage::new(fw, fh);
            image::imageops::replace(&mut canvas, &rs, 0, 0);
            const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
            const ISTDV: [f32; 3] = [1.0 / 0.229, 1.0 / 0.224, 1.0 / 0.225];
            let mut data = Vec::with_capacity((fw * fh * 3) as usize);
            for &c in &[2u8, 1, 0] {
                for p in canvas.pixels() {
                    let v = &p.0[c as usize];
                    data.push((*v as f32 / 255.0 - MEAN[c as usize]) * ISTDV[c as usize]);
                }
            }
            let input = Tensor::from_array(([1usize, 3, fh as usize, fw as usize], data))
                .map_err(|e| e.to_string())?;
            let outputs = self
                .det
                .run(ort::inputs!["x" => &input])
                .map_err(|e| e.to_string())?;
            let (_, map): (&ort::value::Shape, &[f32]) = outputs["sigmoid_0.tmp_0"]
                .try_extract_tensor::<f32>()
                .map_err(|e| e.to_string())?;
            Ok((
                map.iter().map(|f| (f * u16::MAX as f32) as u32).collect(),
                fw,
                fh,
                scale,
            ))
        }

        /// 整图 OCR,返回识别文本(原图坐标)。
        pub fn extract(&mut self, img: &image::DynamicImage) -> Result<Vec<Txt>, String> {
            let (map, fw, fh, scale) = self.det_map(img)?;
            let thr = u16::MAX as u32 / 2;
            let mut seen = vec![false; (fw * fh) as usize];
            let mut boxes: Vec<(u32, u32, u32, u32)> = Vec::new();
            let (w, h) = (img.width(), img.height());
            for y in 0..fh {
                for x in 0..fw {
                    let idx = (y * fw + x) as usize;
                    if seen[idx] || map[idx] < thr {
                        continue;
                    }
                    let (mut minx, mut miny, mut maxx, mut maxy, mut area) = (x, y, x, y, 0u32);
                    let mut q = VecDeque::new();
                    q.push_back(idx);
                    seen[idx] = true;
                    while let Some(i) = q.pop_front() {
                        let (cx, cy) = ((i as u32) % fw, (i as u32) / fw);
                        minx = minx.min(cx);
                        maxx = maxx.max(cx);
                        miny = miny.min(cy);
                        maxy = maxy.max(cy);
                        area += 1;
                        let ns = [
                            i.checked_sub(1).filter(|_| cx > 0),
                            Some(i + 1).filter(|_| cx + 1 < fw),
                            i.checked_sub(fw as usize).filter(|_| cy > 0),
                            Some(i + fw as usize).filter(|_| cy + 1 < fh),
                        ];
                        for n in ns.into_iter().flatten() {
                            if !seen[n] && map[n] >= thr {
                                seen[n] = true;
                                q.push_back(n);
                            }
                        }
                    }
                    // 去掉 pad 后映射回原图坐标,略外扩
                    let (bx0, by0, bx1, by1) = (
                        ((minx as f32 / scale) as u32).saturating_sub(3),
                        ((miny as f32 / scale) as u32).saturating_sub(3),
                        (((maxx + 1) as f32 / scale) as u32 + 3).min(w),
                        (((maxy + 1) as f32 / scale) as u32 + 3).min(h),
                    );
                    if bx1 > bx0 + 2 && by1 > by0 + 2 && area >= 12 {
                        boxes.push((bx0, by0, bx1, by1));
                    }
                }
            }
            // 行序:先按 y 分档再按 x
            boxes.sort_by_key(|&(x0, y0, _, _)| (y0 / 32, x0));
            let rgb = img.to_rgb8();
            let mut out = Vec::new();
            for &(bx0, by0, bx1, by1) in &boxes {
                let mut crop =
                    image::imageops::crop_imm(&rgb, bx0, by0, bx1 - bx0, by1 - by0).to_image();
                if self.flipped(&crop) {
                    crop = image::imageops::rotate180(&crop);
                }
                let (cw, ch) = (crop.width(), crop.height());
                let rw = (((cw as f32 / ch as f32) * 48.0) as u32).clamp(8, 320);
                let c48 =
                    image::imageops::resize(&crop, rw, 48, image::imageops::FilterType::Triangle);
                let mut data = Vec::with_capacity(48 * rw as usize * 3);
                for &c in &[2u8, 1, 0] {
                    for p in c48.pixels() {
                        let v = p.0[c as usize] as f32 / 255.0;
                        data.push((v - 0.5) / 0.5);
                    }
                }
                let input = Tensor::from_array(([1usize, 3, 48usize, rw as usize], data))
                    .map_err(|e| e.to_string())?;
                let outputs = self
                    .rec
                    .run(ort::inputs!["x" => &input])
                    .map_err(|e| e.to_string())?;
                let (shape, logits): (&ort::value::Shape, &[f32]) = outputs["softmax_11.tmp_0"]
                    .try_extract_tensor::<f32>()
                    .map_err(|e| e.to_string())?;
                // 形状 [1, W', C]
                let dims = shape.to_ixdyn();
                let seq_len = dims[1] as usize;
                let classes = dims[2] as usize;
                let mut text = String::new();
                let (mut conf_sum, mut hits, mut last) = (0.0f32, 0usize, 0usize);
                for t in 0..seq_len {
                    let row = &logits[(t * classes)..((t + 1) * classes)];
                    let mut best = 0usize;
                    let mut bp = row[0];
                    for (i, &p) in row.iter().enumerate() {
                        if p > bp {
                            best = i;
                            bp = p;
                        }
                    }
                    if best != last && best != 0 {
                        if let Some(ch) = self.dict.get(best - 1) {
                            text.push_str(ch);
                            conf_sum += bp;
                            hits += 1;
                        }
                    }
                    last = best;
                }
                if hits == 0 || text.trim().is_empty() {
                    continue;
                }
                let conf = conf_sum / hits as f32;
                if conf < 0.6 {
                    continue;
                }
                out.push(Txt {
                    text,
                    conf,
                    x0: bx0,
                    y0: by0,
                    x1: bx1,
                    y1: by1,
                });
            }
            Ok(out)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ocr_roundtrip() {
            let dir = match models_dir() {
                Some(d) => d,
                None => return,
            };
            let fixture = dir.join("ocr-test.png");
            if !fixture.is_file() {
                eprintln!("无 {} 跳过", fixture.display());
                return;
            }
            let store = crate::db::Store::open_mem().unwrap();
            let mut ocr = Ocr::load(&store).unwrap_or_else(|e| panic!("加载 OCR 失败: {e}"));
            let img = image::ImageReader::open(&fixture)
                .unwrap()
                .decode()
                .unwrap();
            let out = ocr.extract(&img).unwrap();
            let joined: String = out
                .iter()
                .map(|t| t.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            eprintln!("识别结果: {out:?}");
            assert!(!out.is_empty(), "未能识别出文本");
            assert!(
                joined.contains("HELLO") || joined.contains("123"),
                "结果不含目标: {joined}"
            );
        }
    }
}

/// 人脸(M2c):两套检测+识别方案,用户在 选项 > 常规 里选择。
/// 默认 YuNet 检测 + AdaFace IR-50 识别(均为商业友好许可);
/// 兼容 insightface antelopev2(SCRFD + ArcFace,权重许可非商用,自备自用)。
/// 检测模型类型按输出张量数自动识别(单输出 = YuNet,多输出 = SCRFD),
/// 文件放错也能跑;获取方式见 README。
pub mod face {
    use super::{env, models_dir};
    use ort::session::Session;
    use ort::session::SessionOutputs;
    use ort::value::{Tensor, TensorElementType, ValueType};

    /// 人脸方案:向量与阈值不可跨方案混用,非默认权重由用户自备。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FaceAlgo {
        /// YuNet 检测 + SFace 识别(OpenCV Zoo;默认,Apache-2.0 含权重)。
        SFace,
        /// YuNet 检测 + AdaFace IR-50 识别(权重非商用,自备)。
        AdaFace,
        /// insightface antelopev2(SCRFD + ArcFace;权重非商用,自备自用)。
        AntelopeV2,
    }

    impl FaceAlgo {
        pub fn key(self) -> &'static str {
            match self {
                Self::SFace => "sface",
                Self::AdaFace => "adaface",
                Self::AntelopeV2 => "antelopev2",
            }
        }

        pub fn from_key(k: &str) -> Self {
            if k == Self::AdaFace.key() {
                Self::AdaFace
            } else if k == Self::AntelopeV2.key() {
                Self::AntelopeV2
            } else {
                Self::SFace
            }
        }

        pub fn label(self) -> &'static str {
            match self {
                Self::SFace => crate::i18n::t("YuNet + SFace(推荐,Apache-2.0 含权重)"),
                Self::AdaFace => crate::i18n::t("YuNet + AdaFace(权重许可非商用,自备)"),
                Self::AntelopeV2 => crate::i18n::t("insightface antelopev2(权重非商用,自备)"),
            }
        }

        /// 该方案要求的模型文件名(检测, 识别),放数据目录 models\ 下。
        pub fn files(self) -> (&'static str, &'static str) {
            match self {
                Self::SFace => ("face_det.onnx", "face_rec_sface.onnx"),
                Self::AdaFace => ("face_det.onnx", "face_rec.onnx"),
                Self::AntelopeV2 => ("face_det_v2.onnx", "face_rec_v2.onnx"),
            }
        }

        /// 嵌入维度(模板 dim 过滤与输出校验用)。
        pub fn emb_dim(self) -> usize {
            match self {
                Self::SFace => 128,
                Self::AdaFace | Self::AntelopeV2 => 512,
            }
        }

        /// 聚类阈值按识别方案区分。
        pub fn cluster_thr(self) -> f32 {
            match self {
                Self::SFace => 0.45,
                Self::AdaFace | Self::AntelopeV2 => 0.6,
            }
        }

        pub fn suggestion_thr(self) -> f32 {
            match self {
                Self::SFace => 0.4,
                Self::AdaFace | Self::AntelopeV2 => 0.55,
            }
        }

        /// 识别对齐使用标准 112×112 五点模板。
        pub fn canon(self) -> [(f32, f32); 5] {
            [
                (38.2946, 51.6963),
                (73.5318, 51.5014),
                (56.0252, 71.7366),
                (41.5493, 92.3655),
                (70.7299, 92.2041),
            ]
        }

        /// SFace 使用未归一化的 RGB。
        pub fn rec_raw_rgb(self) -> bool {
            matches!(self, Self::SFace)
        }

        /// 受支持的导出契约(检测输出布局/识别输入与对齐尺寸),变更需同步升管线版本。
        pub fn contract(self) -> &'static str {
            match self {
                Self::SFace => "yunet12/sface-rawrgb-112",
                Self::AdaFace => "yunet12/adaface-bgr-112",
                Self::AntelopeV2 => "scrfd9/arcface-bgr-112",
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct Det {
        pub x0: f32,
        pub y0: f32,
        pub x1: f32,
        pub y1: f32,
        pub score: f32,
        pub kps: [(f32, f32); 5],
    }

    #[derive(Debug, Clone)]
    pub struct Face {
        pub det: Det,
        pub emb: Vec<f32>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Detector {
        YuNet,
        Scrfd,
    }

    #[derive(Debug, Clone, Copy)]
    struct InputSize {
        width: Option<u32>,
        height: Option<u32>,
    }

    pub struct Faces {
        algo: FaceAlgo,
        det: Session,
        det_kind: Detector,
        det_input: String,
        det_size: InputSize,
        det_outputs: Vec<String>,
        rec: Session,
        rec_input: String,
        rec_out: String,
    }

    fn open_session(p: &std::path::Path) -> Result<Session, String> {
        Session::builder(env()?)
            .map_err(|e| e.to_string())?
            .commit_from_file(p)
            .map_err(|e| e.to_string())
    }

    fn float_shape(dtype: &ValueType) -> Result<&[i64], String> {
        match dtype {
            ValueType::Tensor { ty: TensorElementType::Float32, shape, .. } => Ok(shape),
            _ => Err("人脸模型仅支持 float32 张量".into()),
        }
    }

    fn input_size(shape: &[i64]) -> Result<InputSize, String> {
        let [batch, channels, height, width] = shape else {
            return Err(format!("输入必须为 NCHW: {shape:?}"));
        };
        if !matches!(*batch, -1 | 1) || *channels != 3 {
            return Err(format!("输入必须为单张三通道: {shape:?}"));
        }
        let dimension = |v: i64| match v {
            -1 => Ok(None),
            1.. => u32::try_from(v).map(Some).map_err(|_| "输入尺寸过大".to_string()),
            _ => Err("输入尺寸无效".to_string()),
        };
        Ok(InputSize { width: dimension(*width)?, height: dimension(*height)? })
    }

    fn session_input(session: &Session) -> Result<(String, InputSize), String> {
        let [input] = session.inputs() else {
            return Err("人脸模型必须只有一个输入".into());
        };
        if input.name().is_empty() {
            return Err("人脸模型输入名为空".into());
        }
        Ok((input.name().to_string(), input_size(float_shape(input.dtype())?)?))
    }

    fn row_signature(shape: &[i64], columns: i64, batched: bool) -> bool {
        let (rows, cols) = match shape {
            [batch, rows, cols] if matches!(*batch, -1 | 1) => (*rows, *cols),
            [rows, cols] if !batched => (*rows, *cols),
            _ => return false,
        };
        (rows == -1 || rows > 0) && cols == columns
    }

    fn detector_signature(outputs: &[(&str, &[i64])]) -> Result<(Detector, Vec<String>), String> {
        if outputs.iter().any(|(name, _)| name.is_empty())
            || outputs.iter().enumerate().any(|(i, (name, _))| {
                outputs[..i].iter().any(|(other, _)| name == other)
            })
        {
            return Err("检测输出名为空或重复".into());
        }
        if outputs.len() == 12 {
            let rows = |shape: &[i64]| match shape {
                [1, r, _] => Some(*r),
                _ => None,
            };
            let max_rows = outputs.iter().filter_map(|(_, s)| rows(s)).max().unwrap_or(-1);
            if max_rows <= 0 {
                return Err("YuNet 输出形状必须为一维批张量".into());
            }
            let mut taken = [false; 12];
            let mut names = Vec::with_capacity(12);
            for (prefix, columns) in [("cls", 1), ("obj", 1), ("bbox", 4), ("kps", 10)] {
                for (stride_index, stride) in [8, 16, 32].into_iter().enumerate() {
                    let expected = max_rows / (4i64.pow(stride_index as u32));
                    let named = format!("{prefix}_{stride}");
                    let index = match outputs.iter().position(|(n, _)| *n == named) {
                        Some(i) => i,
                        None => (0..outputs.len())
                            .find(|&i| {
                                !taken[i]
                                    && rows(outputs[i].1) == Some(expected)
                                    && outputs[i]
                                        .1
                                        .get(2)
                                        .map(|c| *c == columns)
                                        .unwrap_or(false)
                            })
                            .ok_or_else(|| {
                                format!("不支持的 YuNet 输出签名: 无法定位 stride {stride} 的 {prefix}")
                            })?,
                    };
                    if !row_signature(outputs[index].1, columns, true) {
                        return Err(format!(
                            "YuNet 输出 {} 形状无效: {:?}",
                            outputs[index].0, outputs[index].1
                        ));
                    }
                    if taken[index] {
                        return Err(format!("YuNet 输出重复使用: {}", outputs[index].0));
                    }
                    taken[index] = true;
                    names.push(outputs[index].0.to_string());
                }
            }
            return Ok((Detector::YuNet, names));
        }
        if outputs.len() == 9 {
            let known = ["448", "471", "494", "451", "474", "497", "454", "477", "500"];
            let named = known.iter().all(|name| outputs.iter().any(|(n, _)| n == name));
            let mut ordered: Vec<(&str, &[i64])> = if named {
                known
                    .iter()
                    .filter_map(|name| outputs.iter().find(|(n, _)| n == name).copied())
                    .collect()
            } else {
                outputs.iter().copied().collect()
            };
            if !named {
                if ordered.iter().any(|(_, shape)| shape.len() != 2) {
                    return Err("SCRFD 输出形状必须为二维".into());
                }
                ordered.sort_by_key(|(_, shape)| {
                    (
                        shape.get(1).copied().unwrap_or(0),
                        std::cmp::Reverse(shape.first().copied().unwrap_or(0).max(0)),
                    )
                });
            }
            for (i, (name, shape)) in ordered.iter().enumerate() {
                let columns = [1, 4, 10][i / 3];
                if !row_signature(shape, columns, false) {
                    return Err(format!("SCRFD 输出 {name} 形状无效: {shape:?}"));
                }
            }
            return Ok((Detector::Scrfd, ordered.iter().map(|(name, _)| name.to_string()).collect()));
        }
        Err("不支持的检测输出签名: 需要 YuNet2023 或九输出 SCRFD".into())
    }

    #[derive(Debug, Clone, Copy)]
    struct Letterbox {
        width: u32,
        height: u32,
        resized_width: u32,
        resized_height: u32,
        scale_x: f32,
        scale_y: f32,
    }

    fn letterbox(width: u32, height: u32, input: InputSize, kind: Detector) -> Result<Letterbox, String> {
        if width == 0 || height == 0 {
            return Err("图像尺寸为空".into());
        }
        if [input.width, input.height].iter().flatten().any(|v| *v == 0 || *v % 32 != 0) {
            return Err("检测输入尺寸必须为 32 的正整数倍".into());
        }
        let limit = if kind == Detector::YuNet { 1280 } else { 640 };
        let bound_w = input.width.unwrap_or(limit);
        let bound_h = input.height.unwrap_or(limit);
        let mut scale = (bound_w as f64 / width as f64).min(bound_h as f64 / height as f64);
        if kind == Detector::YuNet && input.width.is_none() && input.height.is_none() {
            scale = scale.min(1.0);
        }
        let rw = ((width as f64 * scale).round() as u32).clamp(1, bound_w);
        let rh = ((height as f64 * scale).round() as u32).clamp(1, bound_h);
        Ok(Letterbox {
            width: input.width.unwrap_or(rw.div_ceil(32) * 32),
            height: input.height.unwrap_or(rh.div_ceil(32) * 32),
            resized_width: rw,
            resized_height: rh,
            scale_x: rw as f32 / width as f32,
            scale_y: rh as f32 / height as f32,
        })
    }

    fn detector_input(img: &image::RgbImage, layout: Letterbox, kind: Detector) -> Vec<f32> {
        let resized = image::imageops::resize(
            img, layout.resized_width, layout.resized_height, image::imageops::FilterType::Triangle,
        );
        let mut canvas = image::RgbImage::new(layout.width, layout.height);
        image::imageops::replace(&mut canvas, &resized, 0, 0);
        let channels = if kind == Detector::YuNet { [2, 1, 0] } else { [0, 1, 2] };
        let mut data = Vec::with_capacity(layout.width as usize * layout.height as usize * 3);
        for c in channels {
            for p in canvas.pixels() {
                let v = p.0[c] as f32;
                data.push(if kind == Detector::YuNet { v } else { (v - 127.5) / 128.0 });
            }
        }
        data
    }

    #[derive(Clone, Copy)]
    struct TensorView<'a> {
        shape: &'a [i64],
        values: &'a [f32],
    }

    fn output_tensor<'a>(outputs: &'a SessionOutputs<'_>, name: &str) -> Result<TensorView<'a>, String> {
        if name.is_empty() {
            return Err("输出名为空".into());
        }
        let value = outputs.get(name).ok_or_else(|| format!("缺少输出: {name}"))?;
        let (shape, values) = value.try_extract_tensor::<f32>()
            .map_err(|e| format!("输出 {name}: {e}"))?;
        Ok(TensorView { shape, values })
    }

    fn check_rows(tensor: TensorView<'_>, rows: usize, columns: usize, batched: bool) -> Result<(), String> {
        let valid = match tensor.shape {
            [1, r, c] => *r == rows as i64 && *c == columns as i64,
            [r, c] if !batched => *r == rows as i64 && *c == columns as i64,
            _ => false,
        };
        if !valid || rows.checked_mul(columns) != Some(tensor.values.len()) {
            return Err(format!("检测输出形状或长度异常: {:?}, 预期 {rows}×{columns}", tensor.shape));
        }
        Ok(())
    }

    fn grid(width: u32, height: u32, stride: u32) -> Result<(usize, usize), String> {
        if !matches!(stride, 8 | 16 | 32) || width == 0 || height == 0
            || width % stride != 0 || height % stride != 0
        {
            return Err("检测网格尺寸无效".into());
        }
        let gw = (width / stride) as usize;
        let cells = gw.checked_mul((height / stride) as usize).ok_or("检测网格过大")?;
        Ok((gw, cells))
    }

    fn valid_detection(d: &Det) -> bool {
        [d.x0, d.y0, d.x1, d.y1, d.score].iter().all(|v| v.is_finite())
            && d.x1 > d.x0 && d.y1 > d.y0
            && d.kps.iter().all(|(x, y)| x.is_finite() && y.is_finite())
    }

    fn decode_yunet_stride(
        width: u32, height: u32, stride: u32, tensors: [TensorView<'_>; 4],
    ) -> Result<Vec<Det>, String> {
        let (gw, cells) = grid(width, height, stride)?;
        for (tensor, columns) in tensors.iter().zip([1, 1, 4, 10]) {
            check_rows(*tensor, cells, columns, true)?;
        }
        let [cls, obj, bbox, kps] = tensors;
        let mut dets = Vec::new();
        for i in 0..cells {
            if !cls.values[i].is_finite() || !obj.values[i].is_finite() {
                continue;
            }
            let score = (cls.values[i].clamp(0.0, 1.0) * obj.values[i].clamp(0.0, 1.0)).sqrt();
            if score < 0.5 {
                continue;
            }
            let (col, row, s) = ((i % gw) as f32, (i / gw) as f32, stride as f32);
            let b = &bbox.values[i * 4..i * 4 + 4];
            let k = &kps.values[i * 10..i * 10 + 10];
            let (cx, cy) = ((col + b[0]) * s, (row + b[1]) * s);
            let (w, h) = (b[2].exp() * s, b[3].exp() * s);
            let d = Det {
                x0: cx - w * 0.5, y0: cy - h * 0.5,
                x1: cx + w * 0.5, y1: cy + h * 0.5, score,
                kps: std::array::from_fn(|j| ((col + k[j * 2]) * s, (row + k[j * 2 + 1]) * s)),
            };
            if valid_detection(&d) {
                dets.push(d);
            }
        }
        Ok(dets)
    }

    fn decode_scrfd_stride(
        width: u32, height: u32, stride: u32, tensors: [TensorView<'_>; 3],
    ) -> Result<Vec<Det>, String> {
        let (gw, cells) = grid(width, height, stride)?;
        let rows = cells.checked_mul(2).ok_or("SCRFD 网格过大")?;
        for (tensor, columns) in tensors.iter().zip([1, 4, 10]) {
            check_rows(*tensor, rows, columns, false)?;
        }
        let [scores, bbox, kps] = tensors;
        let mut dets = Vec::new();
        for i in 0..rows {
            let score = scores.values[i];
            if !score.is_finite() || score < 0.5 {
                continue;
            }
            let cell = i / 2;
            let s = stride as f32;
            let (ax, ay) = ((cell % gw) as f32 * s, (cell / gw) as f32 * s);
            let b = &bbox.values[i * 4..i * 4 + 4];
            let k = &kps.values[i * 10..i * 10 + 10];
            let d = Det {
                x0: ax - b[0] * s, y0: ay - b[1] * s,
                x1: ax + b[2] * s, y1: ay + b[3] * s, score,
                kps: std::array::from_fn(|j| (ax + k[j * 2] * s, ay + k[j * 2 + 1] * s)),
            };
            if valid_detection(&d) {
                dets.push(d);
            }
        }
        Ok(dets)
    }

    fn map_detection(mut d: Det, layout: Letterbox) -> Det {
        d.x0 /= layout.scale_x;
        d.x1 /= layout.scale_x;
        d.y0 /= layout.scale_y;
        d.y1 /= layout.scale_y;
        for (x, y) in &mut d.kps {
            *x /= layout.scale_x;
            *y /= layout.scale_y;
        }
        d
    }

    fn nms(mut dets: Vec<Det>) -> Vec<Det> {
        dets.retain(valid_detection);
        dets.sort_by(|a, b| b.score.total_cmp(&a.score));
        let mut keep: Vec<Det> = Vec::new();
        for d in dets {
            if keep.iter().all(|k| {
                let ix = (d.x1.min(k.x1) as f64 - d.x0.max(k.x0) as f64).max(0.0);
                let iy = (d.y1.min(k.y1) as f64 - d.y0.max(k.y0) as f64).max(0.0);
                let area = |b: &Det| (b.x1 as f64 - b.x0 as f64) * (b.y1 as f64 - b.y0 as f64);
                let inter = ix * iy;
                inter / (area(&d) + area(k) - inter) < 0.4
            }) {
                keep.push(d);
            }
        }
        keep
    }

    #[derive(Debug, Clone, Copy)]
    struct Similarity {
        a: f64,
        b: f64,
        tx: f64,
        ty: f64,
    }

    fn fit_similarity(src: &[(f32, f32); 5], dst: &[(f32, f32); 5]) -> Option<Similarity> {
        if src.iter().chain(dst).any(|(x, y)| !x.is_finite() || !y.is_finite()) {
            return None;
        }
        let mean = |points: &[(f32, f32); 5]| {
            points.iter().fold((0.0, 0.0), |(x, y), p| (x + p.0 as f64 / 5.0, y + p.1 as f64 / 5.0))
        };
        let (mx, my) = mean(src);
        let (nx, ny) = mean(dst);
        let (mut dot, mut cross) = (0.0, 0.0);
        let (mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0);
        let (mut dxx, mut dyy, mut dxy) = (0.0, 0.0, 0.0);
        for (s, d) in src.iter().zip(dst) {
            let (sx, sy) = (s.0 as f64 - mx, s.1 as f64 - my);
            let (dx, dy) = (d.0 as f64 - nx, d.1 as f64 - ny);
            dot += sx * dx + sy * dy;
            cross += sx * dy - sy * dx;
            sxx += sx * sx;
            syy += sy * sy;
            sxy += sx * sy;
            dxx += dx * dx;
            dyy += dy * dy;
            dxy += dx * dy;
        }
        let den = sxx + syy;
        let dst_den = dxx + dyy;
        if den <= 1e-12 || dst_den <= 1e-12
            || sxx * syy - sxy * sxy <= den * den * 1e-12
            || dxx * dyy - dxy * dxy <= dst_den * dst_den * 1e-12
        {
            return None;
        }
        let (a, b) = (dot / den, cross / den);
        let (tx, ty) = (nx - a * mx + b * my, ny - b * mx - a * my);
        if ![a, b, tx, ty].iter().all(|v| v.is_finite()) || a * a + b * b <= 1e-24 {
            return None;
        }
        Some(Similarity { a, b, tx, ty })
    }

    impl Similarity {
        fn inverse(self, u: f64, v: f64) -> (f64, f64) {
            let (x, y) = (u - self.tx, v - self.ty);
            let den = self.a * self.a + self.b * self.b;
            ((self.a * x + self.b * y) / den, (-self.b * x + self.a * y) / den)
        }
    }

    fn sample_black(img: &image::RgbImage, x: f64, y: f64) -> [f32; 3] {
        if !x.is_finite() || !y.is_finite() || x <= -1.0 || y <= -1.0
            || x >= img.width() as f64 || y >= img.height() as f64
        {
            return [0.0; 3];
        }
        let (x0, y0) = (x.floor() as i64, y.floor() as i64);
        let (fx, fy) = (x - x0 as f64, y - y0 as f64);
        let mut value = [0.0f64; 3];
        for (px, wx) in [(x0, 1.0 - fx), (x0 + 1, fx)] {
            for (py, wy) in [(y0, 1.0 - fy), (y0 + 1, fy)] {
                if px >= 0 && py >= 0 && px < img.width() as i64 && py < img.height() as i64 {
                    let p = img.get_pixel(px as u32, py as u32);
                    for c in 0..3 {
                        value[c] += p.0[c] as f64 * wx * wy;
                    }
                }
            }
        }
        value.map(|v| v as f32)
    }

    fn aligned_crop(
        img: &image::RgbImage, src: &[(f32, f32); 5], dst: &[(f32, f32); 5],
    ) -> Option<Vec<[f32; 3]>> {
        if img.width() == 0 || img.height() == 0 {
            return None;
        }
        let transform = fit_similarity(src, dst)?;
        let mut crop = Vec::with_capacity(112 * 112);
        for v in 0..112 {
            for u in 0..112 {
                let (x, y) = transform.inverse(u as f64, v as f64);
                crop.push(sample_black(img, x, y));
            }
        }
        Some(crop)
    }

    fn recognition_input(crop: &[[f32; 3]], algo: FaceAlgo) -> Vec<f32> {
        let channels = if algo == FaceAlgo::AdaFace { [2, 1, 0] } else { [0, 1, 2] };
        let mut data = Vec::with_capacity(crop.len() * 3);
        for c in channels {
            for p in crop {
                data.push(if algo.rec_raw_rgb() { p[c] } else { (p[c] - 127.5) / 127.5 });
            }
        }
        data
    }

    fn normalize_embedding(tensor: TensorView<'_>, expected: usize) -> Result<Vec<f32>, String> {
        let valid = match tensor.shape {
            [d] | [1, d] => *d == expected as i64,
            _ => false,
        };
        if !valid || tensor.values.len() != expected {
            return Err(format!("rec 维度异常: {:?}, 预期 {expected}", tensor.shape));
        }
        if tensor.values.iter().any(|v| !v.is_finite()) {
            return Err("rec 嵌入包含非有限值".into());
        }
        let norm = tensor.values.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
        if !norm.is_finite() || norm == 0.0 {
            return Err("rec 嵌入范数无效".into());
        }
        Ok(tensor.values.iter().map(|v| (*v as f64 / norm) as f32).collect())
    }

    impl Faces {
        pub fn load(algo: FaceAlgo) -> Result<Self, String> {
            let dir = models_dir().ok_or("无数据目录")?;
            let (det_f, rec_f) = algo.files();
            Self::load_from_paths(algo, &dir.join(det_f), &dir.join(rec_f))
        }

        /// 显式指定模型路径,不访问应用数据目录。
        pub fn load_from_paths(
            algo: FaceAlgo, det_path: &std::path::Path, rec_path: &std::path::Path,
        ) -> Result<Self, String> {
            let det = open_session(det_path)?;
            let (det_input, det_size) = session_input(&det)?;
            let specs = det.outputs().iter().map(|o| Ok((o.name(), float_shape(o.dtype())?)))
                .collect::<Result<Vec<_>, String>>()?;
            let (det_kind, det_outputs) = detector_signature(&specs)?;
            letterbox(1, 1, det_size, det_kind)?;
            let rec = open_session(rec_path)?;
            let (rec_input, rec_size) = session_input(&rec)?;
            if [rec_size.width, rec_size.height].iter().flatten().any(|v| *v != 112) {
                return Err("识别输入必须为 112×112".into());
            }
            let [output] = rec.outputs() else {
                return Err("识别模型必须只有一个输出".into());
            };
            let shape = float_shape(output.dtype())?;
            let dim = algo.emb_dim() as i64;
            let valid = match shape {
                [d] => *d == dim || *d == -1,
                [batch, d] => matches!(*batch, -1 | 1) && (*d == dim || *d == -1),
                _ => false,
            };
            if output.name().is_empty() || !valid {
                return Err(format!("识别输出签名无效: {shape:?}"));
            }
            let rec_out = output.name().to_string();
            Ok(Self { algo, det, det_kind, det_input, det_size, det_outputs, rec, rec_input, rec_out })
        }

        /// 指定方案的检测及识别模型是否已就位。
        pub fn models_present(algo: FaceAlgo) -> bool {
            let dir = match models_dir() {
                Some(d) => d,
                None => return false,
            };
            let (det_f, rec_f) = algo.files();
            dir.join(det_f).is_file() && dir.join(rec_f).is_file()
        }

        /// 检测并映射到原图坐标,过滤无效框后执行 NMS。
        fn detect(&mut self, img: &image::RgbImage) -> Result<Vec<Det>, String> {
            let layout = letterbox(img.width(), img.height(), self.det_size, self.det_kind)?;
            let data = detector_input(img, layout, self.det_kind);
            let input = Tensor::from_array((
                [1usize, 3, layout.height as usize, layout.width as usize], data,
            )).map_err(|e| e.to_string())?;
            let outputs = self.det.run(ort::inputs![self.det_input.as_str() => &input])
                .map_err(|e| format!("det 运行: {e}"))?;
            let mut dets = Vec::new();
            for (i, stride) in [8, 16, 32].into_iter().enumerate() {
                let tensor = |index: usize| {
                    let name = self.det_outputs.get(index).ok_or("检测输出签名不完整")?;
                    output_tensor(&outputs, name)
                };
                let decoded = match self.det_kind {
                    Detector::YuNet => decode_yunet_stride(
                        layout.width, layout.height, stride,
                        [tensor(i)?, tensor(i + 3)?, tensor(i + 6)?, tensor(i + 9)?],
                    )?,
                    Detector::Scrfd => decode_scrfd_stride(
                        layout.width, layout.height, stride,
                        [tensor(i)?, tensor(i + 3)?, tensor(i + 6)?],
                    )?,
                };
                dets.extend(decoded.into_iter().map(|d| map_detection(d, layout)));
            }
            Ok(nms(dets))
        }

        /// 检测、五点对齐并生成有限的单位嵌入向量。
        pub fn analyze(&mut self, img: &image::DynamicImage) -> Result<Vec<Face>, String> {
            let rgb = img.to_rgb8();
            let dets = self.detect(&rgb)?;
            let mut out = Vec::new();
            for d in dets {
                let Some(crop) = aligned_crop(&rgb, &d.kps, &self.algo.canon()) else {
                    continue;
                };
                let data = recognition_input(&crop, self.algo);
                let input = Tensor::from_array(([1usize, 3, 112usize, 112usize], data))
                    .map_err(|e| e.to_string())?;
                let outputs = self.rec.run(ort::inputs![self.rec_input.as_str() => &input])
                    .map_err(|e| format!("rec 运行: {e}"))?;
                let tensor = output_tensor(&outputs, &self.rec_out)?;
                let emb = normalize_embedding(tensor, self.algo.emb_dim())?;
                out.push(Face { det: d, emb });
            }
            Ok(out)
        }
    }

    /// 单个模型文件的只读诊断:路径、存在性、内容哈希与 ONNX 输入/输出签名。
    #[derive(Debug, Clone)]
    pub struct ModelDiag {
        /// 角色标签(检测/识别)。
        pub role: &'static str,
        /// 方案要求的文件名。
        pub file: &'static str,
        /// 生效的绝对路径。
        pub path: std::path::PathBuf,
        /// 文件是否存在。
        pub exists: bool,
        /// 内容 SHA256 前 16 位;缺失或不可读时为对应标记。
        pub sha16: String,
        /// 输入签名 "名[形状]"。
        pub inputs: Vec<String>,
        /// 输出签名 "名[形状]"。
        pub outputs: Vec<String>,
        /// 补充说明:检测器类型、识别维度或错误原因。
        pub note: String,
        /// 是否与方案导出契约不符(含缺失、加载失败、类型不符)。
        pub mismatch: bool,
    }

    /// 一个方案的检测+识别双文件诊断汇总。
    #[derive(Debug, Clone)]
    pub struct FaceDiag {
        pub contract: &'static str,
        pub det: ModelDiag,
        pub rec: ModelDiag,
    }

    /// 检测器类型的展示标签。
    fn detector_label(kind: Detector) -> &'static str {
        match kind {
            Detector::YuNet => "YuNet",
            Detector::Scrfd => "SCRFD",
        }
    }

    /// 从导出契约前缀解析预期的检测器类型标签("yunet12"→YuNet,"scrfd9"→SCRFD)。
    pub fn contract_detector(contract: &str) -> Option<&'static str> {
        match contract.split('/').next().unwrap_or("") {
            "yunet12" => Some(detector_label(Detector::YuNet)),
            "scrfd9" => Some(detector_label(Detector::Scrfd)),
            _ => None,
        }
    }

    fn shape_str(shape: &[i64]) -> String {
        if shape.is_empty() {
            "?".to_string()
        } else {
            shape.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("×")
        }
    }

    fn dtype_shape(dt: &ValueType) -> Vec<i64> {
        match dt {
            ValueType::Tensor { shape, .. } => shape.to_vec(),
            _ => Vec::new(),
        }
    }

    /// 构造诊断骨架:核对路径、存在性与内容哈希,不加载模型。
    fn diag_base(
        role: &'static str, file: &'static str, dir: Option<&std::path::Path>,
    ) -> ModelDiag {
        let path = dir
            .map(|d| d.join(file))
            .unwrap_or_else(|| std::path::PathBuf::from(file));
        let exists = path.is_file();
        let sha16 = if !exists {
            "absent".to_string()
        } else {
            crate::ai_models::sha256_file(&path)
                .map(|s| s[..16].to_string())
                .unwrap_or_else(|_| "unreadable".to_string())
        };
        ModelDiag {
            role, file, path, exists, sha16,
            inputs: Vec::new(), outputs: Vec::new(), note: String::new(), mismatch: false,
        }
    }

    /// 采集一个已存在模型会话的输入/输出签名(仅用于展示)。
    fn fill_io(d: &mut ModelDiag, session: &Session) {
        d.inputs = session
            .inputs()
            .iter()
            .map(|i| format!("{}[{}]", i.name(), shape_str(&dtype_shape(i.dtype()))))
            .collect();
        d.outputs = session
            .outputs()
            .iter()
            .map(|o| format!("{}[{}]", o.name(), shape_str(&dtype_shape(o.dtype()))))
            .collect();
    }

    /// 诊断检测模型:按输出签名判定检测器类型,与契约预期类型对照。
    fn diag_detector(file: &'static str, dir: Option<&std::path::Path>, contract: &str) -> ModelDiag {
        let mut d = diag_base(crate::i18n::t("检测"), file, dir);
        if !d.exists {
            d.note = crate::i18n::t("文件缺失").to_string();
            d.mismatch = true;
            return d;
        }
        let session = match open_session(&d.path) {
            Ok(s) => s,
            Err(e) => {
                d.note = crate::i18n::t_with("加载失败:{e}", &[("e", e)]);
                d.mismatch = true;
                return d;
            }
        };
        fill_io(&mut d, &session);
        let specs: Vec<(&str, &[i64])> = match session
            .outputs()
            .iter()
            .map(|o| float_shape(o.dtype()).map(|s| (o.name(), s)))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(v) => v,
            Err(e) => {
                d.note = e;
                d.mismatch = true;
                return d;
            }
        };
        let expected = contract_detector(contract);
        match detector_signature(&specs) {
            Ok((kind, _)) => {
                let label = detector_label(kind);
                d.note = crate::i18n::t_with("检测器:{p1}", &[("p1", label.to_string())]);
                if expected.is_some_and(|e| e != label) {
                    d.mismatch = true;
                    d.note = crate::i18n::t_with(
                        "检测器不符:实际 {p1},契约要求 {p2}",
                        &[
                            ("p1", label.to_string()),
                            ("p2", expected.unwrap_or("?").to_string()),
                        ],
                    );
                }
            }
            Err(e) => {
                d.note = e;
                d.mismatch = true;
            }
        }
        d
    }

    /// 诊断识别模型:输入必须 112×112、单输出,维度须与方案要求一致。
    fn diag_recognizer(file: &'static str, dir: Option<&std::path::Path>, emb_dim: usize) -> ModelDiag {
        let mut d = diag_base(crate::i18n::t("识别"), file, dir);
        if !d.exists {
            d.note = crate::i18n::t("文件缺失").to_string();
            d.mismatch = true;
            return d;
        }
        let session = match open_session(&d.path) {
            Ok(s) => s,
            Err(e) => {
                d.note = crate::i18n::t_with("加载失败:{e}", &[("e", e)]);
                d.mismatch = true;
                return d;
            }
        };
        fill_io(&mut d, &session);
        let (_name, size) = match session_input(&session) {
            Ok(v) => v,
            Err(e) => {
                d.note = e;
                d.mismatch = true;
                return d;
            }
        };
        if [size.width, size.height].iter().flatten().any(|v| *v != 112) {
            d.note = crate::i18n::t("识别输入必须为 112×112").to_string();
            d.mismatch = true;
            return d;
        }
        let dim = session
            .outputs()
            .first()
            .and_then(|o| dtype_shape(o.dtype()).last().copied())
            .unwrap_or(-1);
        if session.outputs().len() != 1 {
            d.note = crate::i18n::t("识别模型必须只有一个输出").to_string();
            d.mismatch = true;
            return d;
        }
        if dim > 0 && dim as usize != emb_dim {
            d.note = crate::i18n::t_with(
                "维度不符:实际 {p1},方案要求 {p2}",
                &[("p1", dim.to_string()), ("p2", emb_dim.to_string())],
            );
            d.mismatch = true;
            return d;
        }
        d.note = crate::i18n::t_with(
            "识别维度:{p1}(方案要求 {p2})",
            &[
                ("p1", if dim > 0 { dim.to_string() } else { crate::i18n::t("动态").to_string() }),
                ("p2", emb_dim.to_string()),
            ],
        );
        d
    }

    /// 对指定方案做只读诊断:核对生效模型路径、内容哈希与导出契约。
    /// 逐文件独立采集,单个缺失/损坏不中断;结果只展示,不改写也不自动替换错放的模型。
    pub fn diagnose(algo: FaceAlgo) -> FaceDiag {
        let dir = models_dir();
        let (det_f, rec_f) = algo.files();
        let contract = algo.contract();
        FaceDiag {
            contract,
            det: diag_detector(det_f, dir.as_deref(), contract),
            rec: diag_recognizer(rec_f, dir.as_deref(), algo.emb_dim()),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn yunet_entries() -> Vec<(String, Vec<i64>)> {
            let mut out = Vec::new();
            for (stride, rows) in [(8, 256i64), (16, 64), (32, 16)] {
                for (prefix, cols) in [("cls", 1), ("obj", 1), ("bbox", 4), ("kps", 10)] {
                    out.push((format!("{prefix}_{stride}"), vec![1, rows, cols]));
                }
            }
            out
        }

        fn as_refs<'a, N: AsRef<str>>(entries: &'a [(N, Vec<i64>)]) -> Vec<(&'a str, &'a [i64])> {
            entries.iter().map(|(n, s)| (n.as_ref(), s.as_slice())).collect()
        }

        #[test]
        fn contract_detector_maps_known_prefixes() {
            assert_eq!(contract_detector("yunet12/sface-rawrgb-112"), Some("YuNet"));
            assert_eq!(contract_detector("yunet12/adaface-bgr-112"), Some("YuNet"));
            assert_eq!(contract_detector("scrfd9/arcface-bgr-112"), Some("SCRFD"));
            assert_eq!(contract_detector("unknown/x"), None);
            assert_eq!(contract_detector(""), None);
        }

        #[test]
        fn contract_detector_agrees_with_all_algos() {
            for algo in [FaceAlgo::SFace, FaceAlgo::AdaFace, FaceAlgo::AntelopeV2] {
                assert!(
                    contract_detector(algo.contract()).is_some(),
                    "{} 契约前缀无法解析检测器类型",
                    algo.key()
                );
            }
            // SFace/AdaFace 用 YuNet,antelopev2 用 SCRFD。
            assert_eq!(contract_detector(FaceAlgo::SFace.contract()), Some("YuNet"));
            assert_eq!(contract_detector(FaceAlgo::AntelopeV2.contract()), Some("SCRFD"));
        }

        #[test]
        fn shape_str_renders_dims() {
            assert_eq!(shape_str(&[]), "?");
            assert_eq!(shape_str(&[1, 3, 112, 112]), "1×3×112×112");
            assert_eq!(shape_str(&[-1, 512]), "-1×512");
        }

        #[test]
        fn detector_signature_yunet_orders_by_type() {
            let entries = yunet_entries();
            let (kind, names) = detector_signature(&as_refs(&entries)).unwrap();
            assert_eq!(kind, Detector::YuNet);
            let want = [
                "cls_8", "cls_16", "cls_32", "obj_8", "obj_16", "obj_32", "bbox_8", "bbox_16",
                "bbox_32", "kps_8", "kps_16", "kps_32",
            ];
            assert_eq!(names, want);
        }

        #[test]
        fn detector_signature_yunet_accepts_unnamed_shapes() {
            let entries: Vec<(String, Vec<i64>)> = yunet_entries()
                .into_iter()
                .enumerate()
                .map(|(i, (_, shape))| (format!("out_{i}"), shape))
                .collect();
            let mut shuffled = entries.clone();
            shuffled.rotate_left(5);
            let (kind, names) = detector_signature(&as_refs(&shuffled)).unwrap();
            assert_eq!(kind, Detector::YuNet);
            let first = shuffled.iter().find(|(n, _)| n == &names[0]).unwrap();
            assert_eq!(first.1, vec![1, 256, 1]);
        }

        #[test]
        fn detector_signature_rejects_bad_yunet_shape() {
            let mut entries = yunet_entries();
            entries[2].1 = vec![1, 256, 5];
            assert!(detector_signature(&as_refs(&entries)).is_err());
        }

        #[test]
        fn detector_signature_scrfd_sorts_to_named_layout() {
            let named = [
                ("494", vec![800i64, 1]),
                ("500", vec![800, 10]),
                ("451", vec![12800, 4]),
                ("454", vec![12800, 10]),
                ("471", vec![3200, 1]),
                ("477", vec![3200, 10]),
                ("448", vec![12800, 1]),
                ("497", vec![800, 4]),
                ("474", vec![3200, 4]),
            ];
            let (kind, names) = detector_signature(&as_refs(&named)).unwrap();
            assert_eq!(kind, Detector::Scrfd);
            assert_eq!(&names[..3], &["448", "471", "494"]);
            assert_eq!(&names[6..], &["454", "477", "500"]);

            let unnamed: Vec<(String, Vec<i64>)> = named
                .iter()
                .enumerate()
                .map(|(i, (_, shape))| (format!("o{i}"), shape.clone()))
                .collect();
            let (_, names) = detector_signature(&as_refs(&unnamed)).unwrap();
            assert_eq!(names[0], "o6", "cls 12800");
            assert_eq!(names[3], "o2", "bbox 12800");
            assert_eq!(names[6], "o3", "kps 12800");
            assert_eq!(names[8], "o1", "kps 800");
        }

        #[test]
        fn similarity_roundtrip_and_degenerate_points() {
            let src = [(10.0f32, 20.0), (40.0, 20.0), (25.0, 35.0), (15.0, 55.0), (35.0, 55.0)];
            let t = fit_similarity(&src, &src).unwrap();
            for (x, y) in src {
                let (u, v) = t.inverse(x as f64, y as f64);
                assert!((u - x as f64).abs() < 1e-6);
                assert!((v - y as f64).abs() < 1e-6);
            }
            let same = [(1.0f32, 1.0); 5];
            assert!(fit_similarity(&same, &same).is_none());
        }

        #[test]
        fn sample_black_returns_black_outside() {
            let img = image::RgbImage::from_pixel(2, 2, image::Rgb([1, 2, 3]));
            assert_eq!(sample_black(&img, -2.0, 0.0), [0.0; 3]);
            assert_eq!(sample_black(&img, 3.0, 0.0), [0.0; 3]);
            assert_eq!(sample_black(&img, 0.0, 0.0), [1.0, 2.0, 3.0]);
        }

        #[test]
        fn recognition_input_is_nchw_and_model_orders() {
            let crop = [[10.0f32, 20.0, 30.0]; 4];
            let sface = recognition_input(&crop, FaceAlgo::SFace);
            assert_eq!(sface.len(), 12);
            assert_eq!(&sface[0..4], &[10.0, 10.0, 10.0, 10.0]);
            assert_eq!(&sface[4..8], &[20.0, 20.0, 20.0, 20.0]);
            let ada = recognition_input(&crop, FaceAlgo::AdaFace);
            assert!((ada[0] - (30.0f32 - 127.5) / 127.5).abs() < 1e-6);
            assert!((ada[8] - (10.0f32 - 127.5) / 127.5).abs() < 1e-6);
        }

        /// 检测框几何/得分是否在合理范围(纯函数,catch 解码与坐标错位)。
        fn det_is_sane(d: &Det, width: u32, height: u32) -> Result<(), String> {
            if !(d.score > 0.0 && d.score <= 1.0) {
                return Err(format!("得分越界: {}", d.score));
            }
            if [d.x0, d.y0, d.x1, d.y1].iter().any(|v| !v.is_finite()) {
                return Err("框坐标非有限".to_string());
            }
            if d.x1 <= d.x0 || d.y1 <= d.y0 {
                return Err(format!("框反向或零面积: [{}, {}, {}, {}]", d.x0, d.y0, d.x1, d.y1));
            }
            let mx = width as f32 * 0.1;
            let my = height as f32 * 0.1;
            if d.x0 < -mx || d.y0 < -my || d.x1 > width as f32 + mx || d.y1 > height as f32 + my {
                return Err(format!("框超出图像范围: [{}, {}, {}, {}]", d.x0, d.y0, d.x1, d.y1));
            }
            if d.kps.iter().any(|p| !p.0.is_finite() || !p.1.is_finite()) {
                return Err("关键点含非有限值".to_string());
            }
            let inside = |p: &(f32, f32)| {
                p.0 >= d.x0 - mx && p.0 <= d.x1 + mx && p.1 >= d.y0 - my && p.1 <= d.y1 + my
            };
            let n = d.kps.iter().filter(|p| inside(p)).count();
            if n < 3 {
                return Err(format!("多数关键点落在框外({n}/5),疑似对齐或解码错位"));
            }
            Ok(())
        }

        /// 嵌入是有限值且单位 L2 范数(纯函数,catch 归一化与 NaN/Inf)。
        fn emb_is_unit(v: &[f32]) -> bool {
            v.iter().all(|x| x.is_finite())
                && (v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt() - 1.0).abs() < 1e-3
        }

        fn cos_sim(a: &[f32], b: &[f32]) -> Option<f64> {
            if a.len() != b.len() || a.is_empty() {
                return None;
            }
            let dot: f64 = a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum();
            let na: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
            let nb: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
            if na == 0.0 || nb == 0.0 {
                return None;
            }
            Some(dot / (na * nb))
        }

        fn iou(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f64 {
            let ix = (a.2.min(b.2) - a.0.max(b.0)).max(0.0) as f64;
            let iy = (a.3.min(b.3) - a.1.max(b.1)).max(0.0) as f64;
            let inter = ix * iy;
            let ua = (a.2 - a.0).max(0.0) as f64 * (a.3 - a.1).max(0.0) as f64;
            let ub = (b.2 - b.0).max(0.0) as f64 * (b.3 - b.1).max(0.0) as f64;
            let union = ua + ub - inter;
            if union > 0.0 {
                inter / union
            } else {
                0.0
            }
        }

        /// 读同目录 `<图>.maliang_ref.txt` 里的 OpenCV 参考:首词 `box`/`emb`,其后为浮点。
        fn read_reference(
            path: &std::path::Path,
        ) -> Option<(Option<(f32, f32, f32, f32)>, Option<Vec<f32>>)> {
            let text = std::fs::read_to_string(path).ok()?;
            let mut boxv = None;
            let mut emb = None;
            for line in text.lines() {
                let mut it = line.split_whitespace();
                match it.next() {
                    Some("box") => {
                        let n: Vec<f32> = it.filter_map(|s| s.parse().ok()).take(4).collect();
                        if n.len() == 4 {
                            boxv = Some((n[0], n[1], n[2], n[3]));
                        }
                    }
                    Some("emb") => {
                        emb = Some(it.filter_map(|s| s.parse::<f32>().ok()).collect());
                    }
                    _ => {}
                }
            }
            Some((boxv, emb))
        }

        #[test]
        fn det_is_sane_accepts_valid_box() {
            let d = Det {
                x0: 100., y0: 80., x1: 220., y1: 210., score: 0.9,
                kps: [(130., 130.), (190., 130.), (160., 160.), (135., 185.), (185., 185.)],
            };
            assert!(det_is_sane(&d, 640, 480).is_ok());
        }

        #[test]
        fn det_is_sane_rejects_bad_score_flip_and_offset() {
            let base = Det {
                x0: 100., y0: 80., x1: 220., y1: 210., score: 0.9,
                kps: [(130., 130.), (190., 130.), (160., 160.), (135., 185.), (185., 185.)],
            };
            assert!(det_is_sane(&Det { score: 1.5, ..base.clone() }, 640, 480).is_err());
            assert!(det_is_sane(&Det { x0: 220., x1: 100., ..base.clone() }, 640, 480).is_err());
            let out_of_frame = Det {
                x0: 2000., y0: 80., x1: 2100., y1: 210.,
                kps: [(2030., 130.), (2050., 130.), (2040., 160.), (2035., 185.), (2050., 185.)],
                ..base.clone()
            };
            assert!(det_is_sane(&out_of_frame, 640, 480).is_err());
            let kps_off = Det {
                kps: [(0., 0.), (5., 5.), (10., 10.), (15., 15.), (20., 20.)],
                ..base.clone()
            };
            assert!(det_is_sane(&kps_off, 640, 480).is_err());
        }

        #[test]
        fn emb_is_unit_rejects_nan_zero_and_offnorm() {
            assert!(emb_is_unit(&[0.6, 0.8]));
            assert!(!emb_is_unit(&[1.0, 1.0]));
            assert!(!emb_is_unit(&[0.0, 0.0]));
            assert!(!emb_is_unit(&[f32::NAN, 1.0]));
        }

        #[test]
        fn cos_and_iou_reference_math() {
            assert_eq!(cos_sim(&[1., 0.], &[1., 0.]), Some(1.0));
            assert!(cos_sim(&[1., 0.], &[0., 1.]).unwrap().abs() < 1e-9);
            assert_eq!(cos_sim(&[1., 0.], &[1., 0., 0.]), None);
            assert!((iou((0., 0., 10., 10.), (0., 0., 10., 10.)) - 1.0).abs() < 1e-9);
            assert!(iou((0., 0., 10., 10.), (20., 20., 30., 30.)).abs() < 1e-9);
        }

        #[test]
        #[ignore = "需要真实人脸模型与测试照片,设置 MALIANG_FACE_TEST_IMAGE 后运行"]
        fn face_roundtrip_reference_models() {
            let fixture = std::path::PathBuf::from(
                std::env::var("MALIANG_FACE_TEST_IMAGE")
                    .unwrap_or_else(|_| panic!("缺少 MALIANG_FACE_TEST_IMAGE")),
            );
            let img = image::ImageReader::open(&fixture)
                .expect("打开测试图片")
                .decode()
                .expect("解码测试图片");
            let (w, h) = (img.width(), img.height());
            // 可选参考对照:OpenCV(cv2.FaceDetectorYN + cv2.FaceRecognizerSF)在同图上
            // 输出的框与向量,写进同目录 `<图>.maliang_ref.txt`(行首 `box`/`emb`)。
            let reference = read_reference(&fixture.with_extension("maliang_ref.txt"));
            fn best_face(out: &[Face]) -> &Face {
                out.iter()
                    .max_by(|a, b| a.det.score.partial_cmp(&b.det.score).unwrap())
                    .expect("非空结果里应有最高分脸")
            }
            let mut checked = 0usize;
            for algo in [FaceAlgo::SFace, FaceAlgo::AdaFace] {
                let present = Faces::models_present(algo);
                if algo == FaceAlgo::SFace {
                    assert!(present, "SFace 模型未就位(默认方案),不能把缺失当通过");
                } else if !present {
                    eprintln!("AdaFace 模型未就位,跳过(权重非商用,可自备)");
                    continue;
                }
                let mut f = Faces::load(algo)
                    .unwrap_or_else(|e| panic!("加载 {} 模型失败: {e}", algo.key()));
                let out = f.analyze(&img).unwrap_or_else(|e| panic!("{} 分析失败: {e}", algo.key()));
                assert!(!out.is_empty(), "{} 未检测到人脸,无法对照", algo.key());
                let best = best_face(&out);
                assert_eq!(best.emb.len(), algo.emb_dim());
                assert!(emb_is_unit(&best.emb), "{} 嵌入非单位范数或含 NaN/Inf", algo.key());
                det_is_sane(&best.det, w, h)
                    .unwrap_or_else(|e| panic!("{} 检测框异常: {e}", algo.key()));
                // 决定性:再跑一次向量应一致(排除跨照片共享缓冲污染)。
                let again = f.analyze(&img).unwrap();
                let best2 = best_face(&again);
                let dc = cos_sim(&best.emb, &best2.emb).unwrap_or(-1.0);
                assert!(dc > 0.999, "{} 两次分析向量不一致(cos={dc:.4}),疑缓冲未隔离", algo.key());
                // 与 OpenCV 参考对照(仅 SFace,且 sidecar 存在时)。
                if algo == FaceAlgo::SFace {
                    if let Some((rbox, remb)) = &reference {
                        if let Some(rb) = rbox {
                            let v = iou(
                                (best.det.x0, best.det.y0, best.det.x1, best.det.y1),
                                *rb,
                            );
                            assert!(v > 0.5, "对照 OpenCV 检测框 IoU 过低 {v:.3}(我方与参考偏差过大)");
                        }
                        if let Some(re) = remb {
                            let c = cos_sim(&best.emb, re).unwrap_or(-1.0);
                            assert!(c > 0.9, "对照 OpenCV 识别向量 cos 过低 {c:.3}(对齐/通道/归一化不一致)");
                        }
                    } else {
                        eprintln!("未找到参考件,跳过 OpenCV 对照(仅做了自洽与几何校验)");
                    }
                }
                checked += 1;
            }
            assert!(checked > 0, "至少要有一个就位方案完成对照");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::try_load;

    #[test]
    fn loads_model_if_present() {
        let dir = super::models_dir().expect("无数据目录");
        let models: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|x| x == "onnx").unwrap_or(false))
                    .collect()
            })
            .unwrap_or_default();
        if models.is_empty() {
            eprintln!("暂无模型文件({} 不存在或为空),跳过", dir.display());
            return;
        }
        for p in models {
            let info =
                try_load(&p).unwrap_or_else(|e| panic!("加载模型 {} 失败: {e}", p.display()));
            eprintln!("{info}");
        }
    }
}
