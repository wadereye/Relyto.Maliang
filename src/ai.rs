//! 模型功能(OCR/人脸/CLIP)。默认关闭;`cargo build --features ai` 启用。
//! 运行时模型文件放 `%APPDATA%\Relyto\Maliang\models\`,缺失时全部功能优雅不可用。

use ort::environment::Environment;
use ort::session::Session;
use std::sync::OnceLock;

pub fn models_dir() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("Relyto").join("Maliang").join("models"))
}

static ENV: OnceLock<Environment> = OnceLock::new();

/// 进程级唯一 Environment(同进程只允许一个)。
pub(crate) fn env() -> &'static Environment {
    ENV.get_or_init(|| ort::init().build().expect("初始化 onnxruntime 失败"))
}

/// 用 onnxruntime 加载一个模型,返回其输入输出签名(连通性冒烟/排障用)。
#[allow(dead_code)]
pub fn try_load(path: &std::path::Path) -> Result<String, String> {
    let session = Session::builder(env())
        .map_err(|e| format!("会话构建: {e}"))?
        .commit_from_file(path)
        .map_err(|e| format!("加载模型: {e}"))?;
    let inputs: Vec<&str> = session.inputs().iter().map(|i| i.name()).collect();
    let outputs: Vec<&str> = session.outputs().iter().map(|o| o.name()).collect();
    Ok(format!("{path:?}: inputs={inputs:?}, outputs={outputs:?}"))
}

/// 适度现代地列出可用模型(无则空;预留:UI 侧据此显示"未启用")。
#[allow(dead_code)]
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
    }

    fn open_session(p: &std::path::Path) -> Result<Session, String> {
        Session::builder(env())
            .map_err(|e| e.to_string())?
            .commit_from_file(p)
            .map_err(|e| e.to_string())
    }

    impl Ocr {
        pub fn load() -> Result<Self, String> {
            let dir = models_dir().ok_or("无数据目录")?;
            let det = open_session(&dir.join("ch_PP-OCRv4_det_infer.onnx"))?;
            let rec = open_session(&dir.join("ch_PP-OCRv4_rec_infer.onnx"))?;
            let dict = std::fs::read_to_string(dir.join("ppocr_keys_v1.txt"))
                .map_err(|e| format!("读字符表: {e}"))?
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect();
            Ok(Self { det, rec, dict })
        }

        fn det_map(&mut self, img: &image::DynamicImage) -> Result<(Vec<u32>, u32, u32, f32), String> {
            let (w, h) = (img.width(), img.height());
            // 短边缩到 640 附近,长边最多 960,只缩不放
            let scale = (640.0 / w.min(h) as f32)
                .min(960.0 / w.max(h) as f32)
                .min(1.0);
            let (rw0, rh0) = ((w as f32 * scale).max(1.0) as u32, (h as f32 * scale).max(1.0) as u32);
            let pad32 = |v: u32| ((v + 31) / 32) * 32;
            let (fw, fh) = (pad32(rw0), pad32(rh0.max(32)));
            let rs = img.resize_exact(rw0.max(1), rh0.max(1), image::imageops::FilterType::Triangle).to_rgb8();
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
            Ok((map.iter().map(|f| (f * u16::MAX as f32) as u32).collect(), fw, fh, scale))
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
                let crop = image::imageops::crop_imm(&rgb, bx0, by0, bx1 - bx0, by1 - by0).to_image();
                let (cw, ch) = (crop.width(), crop.height());
                let rw = (((cw as f32 / ch as f32) * 48.0) as u32).clamp(8, 320);
                let c48 = image::imageops::resize(&crop, rw, 48, image::imageops::FilterType::Triangle);
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
                out.push(Txt { text, conf, x0: bx0, y0: by0, x1: bx1, y1: by1 });
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
            let mut ocr = Ocr::load().unwrap_or_else(|e| panic!("加载 OCR 失败: {e}"));
            let img = image::ImageReader::open(&fixture).unwrap().decode().unwrap();
            let out = ocr.extract(&img).unwrap();
            let joined: String = out.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" ");
            eprintln!("识别结果: {out:?}");
            assert!(!out.is_empty(), "未能识别出文本");
            assert!(joined.contains("HELLO") || joined.contains("123"), "结果不含目标: {joined}");
        }
    }
}

/// 人脸(M2c):SCRFD 检测 + ArcFace 识别(antelopev2,512 维向量)。
pub mod face {
    use super::{env, models_dir};
    use ort::session::Session;
    use ort::value::Tensor;

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

    pub struct Faces {
        det: Session,
        rec: Session,
    }

    fn open_session(p: &std::path::Path) -> Result<Session, String> {
        Session::builder(env())
            .map_err(|e| e.to_string())?
            .commit_from_file(p)
            .map_err(|e| e.to_string())
    }

    const CANON: [(f32, f32); 5] = [
        (30.294_6, 51.696_3),
        (65.531_8, 51.501_4),
        (48.025_2, 71.736_6),
        (33.549_3, 92.365_5),
        (62.729_9, 92.204_1),
    ];

    impl Faces {
        pub fn load() -> Result<Self, String> {
            let dir = models_dir().ok_or("无数据目录")?;
            Ok(Self {
                det: open_session(&dir.join("face_det.onnx"))?,
                rec: open_session(&dir.join("face_rec.onnx"))?,
            })
        }

        /// 检测所有人脸框(含 5 关键点),坐标为原图像素。
        fn detect(&mut self, img: &image::DynamicImage) -> Result<Vec<Det>, String> {
            const IN: u32 = 640;
            let (w, h) = (img.width(), img.height());
            let scale = (IN as f32 / w as f32).min(IN as f32 / h as f32);
            let rw = ((w as f32 * scale).floor() as u32).max(1);
            let rh = ((h as f32 * scale).floor() as u32).max(1);
            let rs = img
                .resize_exact(rw, rh, image::imageops::FilterType::Triangle)
                .to_rgb8();
            let mut canvas = image::RgbImage::new(IN, IN);
            image::imageops::replace(&mut canvas, &rs, 0, 0);
            let mut data = Vec::with_capacity((IN * IN * 3) as usize);
            for &c in &[2u8, 1, 0] {
                for p in canvas.pixels() {
                    data.push((p.0[c as usize] as f32 - 127.5) / 128.0);
                }
            }
            let input = Tensor::from_array(([1usize, 3, IN as usize, IN as usize], data))
                .map_err(|e| e.to_string())?;
            let outputs = self
                .det
                .run(ort::inputs!["input.1" => &input])
                .map_err(|e| format!("det 运行: {e}"))?;
            let strides = [(8usize, "448", "451", "454"), (16, "471", "474", "477"), (32, "494", "497", "500")];
            let mut dets: Vec<Det> = Vec::new();
            for (stride, sn, bn, kn) in strides {
                let (_s_shape, s_vals) = outputs[sn]
                    .try_extract_tensor::<f32>()
                    .map_err(|e| format!("det 分数: {e}"))?;
                let (_, b_vals) = outputs[bn]
                    .try_extract_tensor::<f32>()
                    .map_err(|e| format!("det 框: {e}"))?;
                let (_, k_vals) = outputs[kn]
                    .try_extract_tensor::<f32>()
                    .map_err(|e| format!("det 点: {e}"))?;
                let n = s_vals.len();
                let gw = IN as usize / stride;
                let an = 2usize;
                for i in 0..n {
                    if s_vals[i] < 0.5 {
                        continue;
                    }
                    let cell = i / an;
                    let ai = i % an;
                    let cy = (cell / gw) as f32 * stride as f32;
                    let cx = (cell % gw) as f32 * stride as f32;
                    let mut ax = cx;
                    let mut ay = cy;
                    if ai == 1 {
                        ax += 0.5 * stride as f32;
                        ay += 0.5 * stride as f32;
                    }
                    let b = &b_vals[i * 4..i * 4 + 4];
                    let (x0, y0, x1, y1) = (
                        ax - b[0] * stride as f32,
                        ay - b[1] * stride as f32,
                        ax + b[2] * stride as f32,
                        ay + b[3] * stride as f32,
                    );
                    let k: Vec<(f32, f32)> = (0..5)
                        .map(|j| {
                            let kv = &k_vals[i * 10..i * 10 + 10];
                            (ax + kv[j * 2] * stride as f32, ay + kv[j * 2 + 1] * stride as f32)
                        })
                        .collect();
                    dets.push(Det {
                        x0: x0 / scale,
                        y0: y0 / scale,
                        x1: x1 / scale,
                        y1: y1 / scale,
                        score: s_vals[i],
                        kps: k.iter().map(|(x, y)| (x / scale, y / scale)).collect::<Vec<_>>()
                            .try_into()
                            .unwrap(),
                    });
                }
            }
            // NMS(IoU 0.4)
            dets.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
            let mut keep: Vec<Det> = Vec::new();
            for d in dets {
                let ok = keep.iter().all(|k: &Det| {
                    let ix = (d.x1.min(k.x1) - d.x0.max(k.x0)).max(0.0);
                    let iy = (d.y1.min(k.y1) - d.y0.max(k.y0)).max(0.0);
                    let inter = ix * iy;
                    let ua = (d.x1 - d.x0) * (d.y1 - d.y0) + (k.x1 - k.x0) * (k.y1 - k.y0) - inter;
                    ua <= 0.0 || inter / ua < 0.4
                });
                if ok {
                    keep.push(d);
                }
            }
            Ok(keep)
        }

        /// 检测 + 5 点对齐 + 512 维嵌入。
        pub fn analyze(&mut self, img: &image::DynamicImage) -> Result<Vec<Face>, String> {
            let dets = self.detect(img)?;
            let mut out = Vec::new();
            for d in dets {
                // 相似变换:src→canon。θ = atan2(Σ s_x d_y - s_y d_x, Σ d_x s_x + d_y s_y)
                let (mut dx, mut dy, mut cx, mut cy) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
                for (s, dd) in d.kps.iter().zip(CANON.iter()) {
                    cx += s.0;
                    cy += s.1;
                    dx += dd.0;
                    dy += dd.1;
                }
                let (mx, my, nx, ny) = (cx / 5.0, cy / 5.0, dx / 5.0, dy / 5.0);
                let (mut r11, mut r21) = (0.0f32, 0.0f32);
                for (s, dd) in d.kps.iter().zip(CANON.iter()) {
                    let sx0 = s.0 - mx;
                    let sy0 = s.1 - my;
                    let dx0 = dd.0 - nx;
                    let dy0 = dd.1 - ny;
                    r11 += dx0 * sx0 + dy0 * sy0;
                    r21 += sy0 * dx0 - sx0 * dy0;
                }
                let th = r21.atan2(r11);
                let (ct, st) = (th.cos(), th.sin());
                let (mut num, mut den) = (0.0f32, 0.0f32);
                for (s, dd) in d.kps.iter().zip(CANON.iter()) {
                    let sx0 = s.0 - mx;
                    let sy0 = s.1 - my;
                    num += sx0 * (ct * dd.0 - st * dd.1) + sy0 * (st * dd.0 + ct * dd.1);
                    den += sx0 * sx0 + sy0 * sy0;
                }
                let s = if den > 0.0 { num / den } else { 1.0 };
                // 采样:目标像素中心 (u+0.5,v+0.5) 反变换回原图:src = s⁻¹·Rᵀ·(tgt - t2) + m_kps,t2 = m_canon - s·R·m_kps
                let txa = nx - s * (ct * mx - st * my);
                let tya = ny - s * (st * mx + ct * my);
                let img_rgb = img.to_rgb32f();
                let (iw, ih) = (img_rgb.width(), img_rgb.height());
                let mut crop = Vec::with_capacity(112 * 112 * 3);
                for v in 0..112usize {
                    for u in 0..112usize {
                        let (txv, tyv) = ((u as f32 + 0.5), (v as f32 + 0.5));
                        let ex = txv - txa;
                        let ey = tyv - tya;
                        let sxx = (ex * ct + ey * st) / s + mx;
                        let syy = (-ex * st + ey * ct) / s + my;
                        let x0 = sxx.floor().max(0.0).min(iw as f32 - 1.0 - 0.0);
                        let y0 = syy.floor().max(0.0).min(ih as f32 - 1.0);
                        let x1 = (x0 + 1.0).min(iw as f32 - 1.0);
                        let y1 = (y0 + 1.0).min(ih as f32 - 1.0);
                        let (fx, fy) = (sxx - x0, syy - y0);
                        let sample = |x: u32, y: u32| -> [f32; 3] {
                            let p = img_rgb.get_pixel(x as u32, y as u32);
                            [p.0[0], p.0[1], p.0[2]]
                        };
                        let p00 = sample(x0 as u32, y0 as u32);
                        let p10 = sample(x1 as u32, y0 as u32);
                        let p01 = sample(x0 as u32, y1 as u32);
                        let p11 = sample(x1 as u32, y1 as u32);
                        let mut px = [0.0f32; 3];
                        for c in 0..3 {
                            px[c] = p00[c] * (1.0 - fx) * (1.0 - fy)
                                + p10[c] * fx * (1.0 - fy)
                                + p01[c] * (1.0 - fx) * fy
                                + p11[c] * fx * fy;
                        }
                        crop.push((px[2] - 127.5) / 128.0);
                        crop.push((px[1] - 127.5) / 128.0);
                        crop.push((px[0] - 127.5) / 128.0);
                    }
                }
                let input = Tensor::from_array(([1usize, 3, 112usize, 112usize], crop))
                    .map_err(|e| e.to_string())?;
                let outputs = self
                    .rec
                    .run(ort::inputs!["input.1" => &input])
                    .map_err(|e| format!("rec 运行: {e}"))?;
                let (_, vals) = outputs["1333"]
                    .try_extract_tensor::<f32>()
                    .map_err(|e| format!("rec 输出: {e}"))?;
                let dim = vals.len();
                if dim != 512 {
                    return Err(format!("rec 维度异常: {dim}"));
                }
                let mut emb = vals.to_vec();
                let norm = emb.iter().map(|v| v * v).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for v in &mut emb {
                        *v /= norm;
                    }
                }
                out.push(Face { det: d, emb });
            }
            Ok(out)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn face_roundtrip() {
            let dir = match models_dir() {
                Some(d) => d,
                None => return,
            };
            let fixture = dir.join("face-test.jpg");
            if !fixture.is_file() {
                eprintln!("无 {} 跳过", fixture.display());
                return;
            }
            let mut f = Faces::load().unwrap_or_else(|e| panic!("加载人脸模型失败: {e}"));
            let img = image::ImageReader::open(&fixture).unwrap().decode().unwrap();
            let out = f.analyze(&img).unwrap();
            eprintln!(
                "检测到 {} 张脸, 首个: {:?}",
                out.len(),
                out.first().map(|x| (x.det.x0, x.det.y0, x.det.x1, x.det.y1, x.det.score, x.det.kps))
            );
            assert!(!out.is_empty(), "未检测到人脸");
            assert!(out[0].emb.len() == 512, "嵌入维度错误");
            assert!(out[0].det.score > 0.5);
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
            let info = try_load(&p).unwrap_or_else(|e| panic!("加载模型 {} 失败: {e}", p.display()));
            eprintln!("{info}");
        }
    }
}
