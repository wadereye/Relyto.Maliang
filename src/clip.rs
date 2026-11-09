//! M4b CLIP 语义搜索:ViT-B-32 双编码器(Qdrant ONNX 导出)+ BPE 分词器。
//!
//! # 第三方代码归属
//!
//! 本文件中的 BPE 分词器实现参考了 OpenAI 的 CLIP 项目
//! (https://github.com/openai/CLIP,`clip/simple_tokenizer.py`),
//! 该项目以 MIT 许可证发布:
//!
//! ```text
//! MIT License
//!
//! Copyright (c) 2021 OpenAI
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.
//! ```
//!
//! 完整的第三方声明见仓库根目录 `THIRD_PARTY_NOTICES.md`。
//!
//! - 模型(`%APPDATA%\Relyto\Maliang\models\`):`clip_vision.onnx`(输入 `pixel_values`
//!   f32[b,3,224,224] → 输出 `image_embeds` f32[b,512])、`clip_text.onnx`
//!   (输入 `input_ids`/`attention_mask` i64[b,77] → 输出 `text_embeds` f32[b,512])、
//!   `clip_merges.txt`(vocab 由 merges 重建,无需 vocab.json)。
//! - 分词器参考 openai/CLIP `simple_tokenizer.py` 实现(见上方第三方声明):正则
//!   `'s|'t|'re|'ve|'m|'ll|'d|[\p{L}]+|[\p{N}]|[^\s\p{L}\p{N}]+`(注意本版无 ` ?`
//!   前缀、`[\p{N}]` 单字符),BPE 按 merges rank 逐级合并,`</w>` 只加在词尾。
//! - 嵌入统一 L2 归一化,余弦 = 点积。图像嵌入存 `clips` 表,文本查询实时编码。

use crate::ai::{env, models_dir};
use crate::db::Store;
use crate::i18n::{t, t_with};
use ort::session::Session;
use ort::value::Tensor;
use std::collections::HashMap;
use std::path::Path;

/// 嵌入维度(ViT-B-32 投影后)。
pub const DIM: usize = 512;
/// CLIP context_length(含 SOT/EOT)。
pub const CTX: usize = 77;

// ---------- 分词器 ----------

/// bytes_to_unicode():返回 (按 bs 插入顺序的字符表, 字节→字符映射)。
/// 字符表插入序:33..=126、161..=172、174..=255,再补剩余字节;
/// vocab 前 256 项按此顺序编号。
fn bytes_to_unicode() -> (Vec<char>, [char; 256]) {
    let mut bs: Vec<u32> = (33..=126).chain(161..=172).chain(174..=255).collect();
    let mut cs: Vec<u32> = bs.clone();
    let mut n: u32 = 0;
    for b in 0u32..256 {
        if !bs.contains(&b) {
            bs.push(b);
            cs.push(256 + n);
            n += 1;
        }
    }
    let ordered: Vec<char> = cs
        .iter()
        .map(|&c| char::from_u32(c).unwrap_or('?'))
        .collect();
    let mut table = ['\0'; 256];
    for (i, &b) in bs.iter().enumerate() {
        table[b as usize] = ordered[i];
    }
    (ordered, table)
}

/// SimpleTokenizer 的 Rust 实现(仅支持 encode,不支持 decode)。
pub struct Tokenizer {
    b2u: [char; 256],
    encoder: HashMap<String, u32>,
    ranks: HashMap<(String, String), u32>,
    sot: u32,
    eot: u32,
}

impl Tokenizer {
    /// 从 merges.txt 加载;encoder 按 merges 重建。
    pub fn load(merges_path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(merges_path)
            .map_err(|e| t_with("读 merges: {e}", &[("e", e.to_string())]))?;
        let (ordered, b2u) = bytes_to_unicode();
        let mut encoder: HashMap<String, u32> = HashMap::with_capacity(49408);
        let mut id = 0u32;
        for &c in &ordered {
            encoder.insert(c.to_string(), id);
            id += 1;
        }
        for &c in &ordered {
            encoder.insert(format!("{c}</w>"), id);
            id += 1;
        }
        let mut ranks: HashMap<(String, String), u32> = HashMap::with_capacity(48894);
        let mut lines = text.lines();
        let _version = lines.next(); // "#version: 0.2 ..."
        let mut rank = 0u32;
        for line in lines.take(49152 - 256 - 2) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
                continue;
            }
            encoder.insert(format!("{}{}", parts[0], parts[1]), id);
            id += 1;
            ranks.insert((parts[0].to_string(), parts[1].to_string()), rank);
            rank += 1;
        }
        let sot = id;
        encoder.insert("<|startoftext|>".to_string(), id);
        id += 1;
        let eot = id;
        encoder.insert("<|endoftext|>".to_string(), id);
        Ok(Self {
            b2u,
            encoder,
            ranks,
            sot,
            eot,
        })
    }

    /// encode:返回 `[SOT] + bpe tokens + [EOT]`(不含 padding;调用方负责截断/填充)。
    pub fn encode(&self, text: &str) -> Vec<u32> {
        let mut ids = Vec::new();
        ids.push(self.sot);
        for tok in self.split_tokens(&text.to_lowercase()) {
            // 词 → utf-8 字节 → unicode 字符序列
            let mut word_str = String::new();
            for b in tok.bytes() {
                word_str.push(self.b2u[b as usize]);
            }
            for sym in self.bpe(&word_str) {
                if let Some(&i) = self.encoder.get(&sym) {
                    ids.push(i);
                }
                // 理论上 merge 产物必在 vocab;容错跳过不 panic
            }
        }
        ids.push(self.eot);
        ids
    }

    /// 分词正则近似(大小写不敏感;输入已 lower):
    /// `<\|startoftext\|>|<\|endoftext\|>|'s|'t|'re|'ve|'m|'ll|'d|[\p{L}]+|[\p{N}]|[^\s\p{L}\p{N}]+`
    /// 注:正则无 ` ?` 前缀 → 空白不进 token,此处直接跳过;
    /// `\p{L}` 用 `is_alphabetic() && !is_numeric()` 近似,`\p{N}` 用 `is_numeric()`。
    fn split_tokens(&self, text: &str) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        let is_letter = |c: char| c.is_alphabetic() && !c.is_numeric();
        let mut out: Vec<String> = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            // 1) 特殊 token 字面量(优先于其他分支)
            if c == '<' {
                if let Some(t) = match_lit(&chars, i, "<|startoftext|>") {
                    out.push(t.to_string());
                    i += 15;
                    continue;
                }
                if let Some(t) = match_lit(&chars, i, "<|endoftext|>") {
                    out.push(t.to_string());
                    i += 13;
                    continue;
                }
            }
            // 2) 英文缩写(优先于字母串)
            if c == '\'' && i + 1 < chars.len() {
                let rest: String = chars[i + 1..].iter().collect();
                let mut hit = None;
                for pat in ["s", "t", "re", "ve", "m", "ll", "d"] {
                    if rest.starts_with(pat) {
                        hit = Some(pat);
                        break;
                    }
                }
                if let Some(pat) = hit {
                    out.push(format!("'{pat}"));
                    i += 1 + pat.len();
                    continue;
                }
            }
            // 3) [\p{L}]+ 字母串
            if is_letter(c) {
                let mut j = i;
                while j < chars.len() && is_letter(chars[j]) {
                    j += 1;
                }
                out.push(chars[i..j].iter().collect());
                i = j;
                continue;
            }
            // 4) [\p{N}] 单数字字符(注意:无 +,逐字符)
            if c.is_numeric() {
                out.push(c.to_string());
                i += 1;
                continue;
            }
            // 5) [^\s\p{L}\p{N}]+ 其他符号串
            if !c.is_whitespace() {
                let mut j = i;
                while j < chars.len() {
                    let d = chars[j];
                    if d.is_whitespace() || is_letter(d) || d.is_numeric() {
                        break;
                    }
                    j += 1;
                }
                out.push(chars[i..j].iter().collect());
                i = j;
                continue;
            }
            // 6) 空白:跳过
            i += 1;
        }
        out
    }

    /// bpe():词尾字符加 `</w>`,反复合并 rank 最小的相邻 pair。
    fn bpe(&self, token: &str) -> Vec<String> {
        let syms: Vec<char> = token.chars().collect();
        let mut word: Vec<String> = Vec::with_capacity(syms.len());
        for (i, c) in syms.iter().enumerate() {
            if i + 1 == syms.len() {
                word.push(format!("{c}</w>"));
            } else {
                word.push(c.to_string());
            }
        }
        if word.len() == 1 {
            return word;
        }
        loop {
            // 找 rank 最小的相邻 pair
            let mut best: Option<(usize, u32)> = None;
            for i in 0..word.len().saturating_sub(1) {
                let pair = (word[i].clone(), word[i + 1].clone());
                if let Some(&r) = self.ranks.get(&pair) {
                    if best.map(|(_, br)| r < br).unwrap_or(true) {
                        best = Some((i, r));
                    }
                }
            }
            let Some((fi, _)) = best else { break };
            let first = word[fi].clone();
            let second = word[fi + 1].clone();
            // 从左到右合并该 pair 的所有不重叠出现
            let mut new_word: Vec<String> = Vec::with_capacity(word.len());
            let mut i = 0;
            while i < word.len() {
                if i + 1 < word.len() && word[i] == first && word[i + 1] == second {
                    new_word.push(format!("{first}{second}"));
                    i += 2;
                } else {
                    new_word.push(word[i].clone());
                    i += 1;
                }
            }
            word = new_word;
            if word.len() == 1 {
                break;
            }
        }
        word
    }
}

/// chars[i..] 是否以字面量 lit 开头(用于特殊 token)。
fn match_lit<'a>(chars: &[char], i: usize, lit: &'a str) -> Option<&'a str> {
    let lc: Vec<char> = lit.chars().collect();
    if i + lc.len() <= chars.len() && chars[i..i + lc.len()] == lc[..] {
        Some(lit)
    } else {
        None
    }
}

// ---------- 编码器 ----------

fn l2_normalize(v: &[f32]) -> Result<Vec<f32>, String> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !norm.is_finite() || norm < 1e-12 {
        return Err(t("嵌入范数异常(全零或 NaN)").into());
    }
    Ok(v.iter().map(|x| x / norm).collect())
}

// ---------- 中文 CLIP(Chinese-CLIP ViT-B-16)----------

/// Chinese-CLIP 上下文长度(context_length=52)。
pub const CN_CTX: usize = 52;

/// CJK 统一表意文字范围( Bert basic_tokenizer 的 _is_chinese_char 同集)。
fn is_chinese_char(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x20000..=0x2A6DF
        | 0x2A700..=0x2B73F | 0x2B740..=0x2B81F | 0x2B820..=0x2CEAF
        | 0xF900..=0xFAFF | 0x2F800..=0x2FA1F)
}

/// 标点拆分(Bert _is_punctuation 近似:ASCII 标点 + 常用中文全角标点;
/// Unicode P* 全表无标准库支持,中文查询覆盖已够)。
fn is_punct(c: char) -> bool {
    if matches!(c as u32,
        0x21..=0x2F | 0x3A..=0x40 | 0x5B..=0x60 | 0x7B..=0x7E)
    {
        return true;
    }
    matches!(
        c,
        '，' | '。'
            | '！'
            | '？'
            | '；'
            | '：'
            | '、'
            | '“'
            | '”'
            | '‘'
            | '’'
            | '（'
            | '）'
            | '《'
            | '》'
            | '【'
            | '】'
            | '—'
            | '…'
            | '·'
            | '～'
    )
}

/// 去控制字符 + 空白规范化(Bert _clean_text)。
fn clean_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let cp = c as u32;
        if cp == 0 || cp == 0xFFFD || c.is_control() {
            continue;
        }
        if c == '　' {
            out.push(' ');
        } else if c.is_whitespace() {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

/// Bert BasicTokenizer + WordPiece(vocab.txt)。仅 encode。
/// 不做 NFD strip accents(中文无影响,英文 accent 罕见)。
pub struct BertTokenizer {
    vocab: HashMap<String, u32>,
    cls: u32,
    sep: u32,
    unk: u32,
}

impl BertTokenizer {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| t_with("读 vocab: {e}", &[("e", e.to_string())]))?;
        let mut vocab = HashMap::with_capacity(22000);
        for (i, line) in text.lines().enumerate() {
            vocab.entry(line.to_string()).or_insert(i as u32);
        }
        Ok(Self {
            cls: *vocab.get("[CLS]").unwrap_or(&101),
            sep: *vocab.get("[SEP]").unwrap_or(&102),
            unk: *vocab.get("[UNK]").unwrap_or(&100),
            vocab,
        })
    }

    /// [CLS] + BasicTokenizer + WordPiece + [SEP]。
    pub fn encode(&self, text: &str) -> Vec<u32> {
        let mut ids = vec![self.cls];
        for tok in self.basic_tokenize(text) {
            ids.extend(self.wordpiece(&tok));
        }
        ids.push(self.sep);
        ids
    }

    fn basic_tokenize(&self, text: &str) -> Vec<String> {
        let cleaned = clean_text(text);
        // CJK 单字拆开(每字前后补空格)
        let mut spaced = String::with_capacity(cleaned.len() * 2);
        for c in cleaned.chars() {
            if is_chinese_char(c) {
                spaced.push(' ');
                spaced.push(c);
                spaced.push(' ');
            } else {
                spaced.push(c);
            }
        }
        let mut out = Vec::new();
        for tok in spaced.split_whitespace() {
            let lower = tok.to_lowercase();
            // 标点单独成 token
            let mut buf = String::new();
            for c in lower.chars() {
                if is_punct(c) {
                    if !buf.is_empty() {
                        out.push(std::mem::take(&mut buf));
                    }
                    out.push(c.to_string());
                } else {
                    buf.push(c);
                }
            }
            if !buf.is_empty() {
                out.push(buf);
            }
        }
        out
    }

    /// WordPiece 最长匹配(未命中 → [UNK] 并跳 1 字)。
    fn wordpiece(&self, token: &str) -> Vec<u32> {
        let chars: Vec<char> = token.chars().collect();
        if chars.len() > 100 {
            return vec![self.unk];
        }
        let mut out = Vec::new();
        let mut start = 0usize;
        while start < chars.len() {
            let mut end = chars.len();
            let mut found = None;
            while start < end {
                let sub: String = chars[start..end].iter().collect();
                let key = if start > 0 { format!("##{sub}") } else { sub };
                if let Some(&id) = self.vocab.get(&key) {
                    found = Some((id, end));
                    break;
                }
                end -= 1;
            }
            match found {
                Some((id, e)) => {
                    out.push(id);
                    start = e;
                }
                None => {
                    out.push(self.unk);
                    start += 1;
                }
            }
        }
        out
    }
}

/// CLIP 图像预处理:短边缩到 224(CatmullRom ≈ PIL bicubic)、居中裁剪、
/// (x/255 - CLIP mean) / CLIP std,RGB CHW。中英文 CLIP 共用。
fn preprocess_224(img: &image::DynamicImage) -> Result<Vec<f32>, String> {
    const S: u32 = 224;
    const MEAN: [f32; 3] = [0.48145466, 0.4578275, 0.40821073];
    const STD: [f32; 3] = [0.26862954, 0.26130258, 0.27577711];
    let (w, h) = (img.width().max(1), img.height().max(1));
    let scale = S as f32 / w.min(h) as f32;
    let rw = ((w as f32 * scale).round() as u32).max(1);
    let rh = ((h as f32 * scale).round() as u32).max(1);
    let resized = img.resize_exact(rw, rh, image::imageops::FilterType::CatmullRom);
    let x0 = (rw.saturating_sub(S)) / 2;
    let y0 = (rh.saturating_sub(S)) / 2;
    let cropped = resized.crop_imm(x0, y0, S.min(rw), S.min(rh)).to_rgb8();
    let (cw, ch) = (cropped.width(), cropped.height());
    if cw != S || ch != S {
        return Err(t_with(
            "裁剪尺寸异常 {cw}x{ch}",
            &[("cw", cw.to_string()), ("ch", ch.to_string())],
        ));
    }
    let mut data = Vec::with_capacity((S * S * 3) as usize);
    for c in 0..3usize {
        for p in cropped.pixels() {
            data.push((p.0[c] as f32 / 255.0 - MEAN[c]) / STD[c]);
        }
    }
    Ok(data)
}

/// CLIP 图像编码器(仅 vision session,语义索引用)。
pub struct ClipVision {
    vision: Session,
}

impl ClipVision {
    pub fn load() -> Result<Self, String> {
        let dir = models_dir().ok_or(t("无数据目录"))?;
        let p = dir.join("clip_vision.onnx");
        if !p.is_file() {
            return Err(t_with("缺模型 {p1}", &[("p1", (p.display()).to_string())]));
        }
        let vision = Session::builder(env()?)
            .map_err(|e| t_with("会话构建: {e}", &[("e", e.to_string())]))?
            .commit_from_file(&p)
            .map_err(|e| t_with("加载 clip_vision: {e}", &[("e", e.to_string())]))?;
        Ok(Self { vision })
    }

    /// 图像 → 512 维 L2 归一化嵌入(预处理见 preprocess_224)。
    pub fn embed_image(&mut self, img: &image::DynamicImage) -> Result<Vec<f32>, String> {
        let data = preprocess_224(img)?;
        let input = Tensor::from_array(([1usize, 3, 224, 224], data)).map_err(|e| e.to_string())?;
        let outputs = self
            .vision
            .run(ort::inputs!["pixel_values" => &input])
            .map_err(|e| t_with("clip vision 运行: {e}", &[("e", e.to_string())]))?;
        let (_, vals): (&ort::value::Shape, &[f32]) = outputs["image_embeds"]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        if vals.len() != DIM {
            return Err(t_with(
                "clip 图像嵌入维度异常: {p1}",
                &[("p1", (vals.len()).to_string())],
            ));
        }
        l2_normalize(vals)
    }
}

/// CLIP 文本编码器(仅 text session + 分词器,语义搜索用)。
pub struct ClipText {
    text: Session,
    tok: Tokenizer,
}

impl ClipText {
    pub fn load() -> Result<Self, String> {
        let dir = models_dir().ok_or(t("无数据目录"))?;
        let p = dir.join("clip_text.onnx");
        if !p.is_file() {
            return Err(t_with("缺模型 {p1}", &[("p1", (p.display()).to_string())]));
        }
        let text = Session::builder(env()?)
            .map_err(|e| t_with("会话构建: {e}", &[("e", e.to_string())]))?
            .commit_from_file(&p)
            .map_err(|e| t_with("加载 clip_text: {e}", &[("e", e.to_string())]))?;
        let tok = Tokenizer::load(&dir.join("clip_merges.txt"))?;
        Ok(Self { text, tok })
    }

    /// 文本 → 512 维 L2 归一化嵌入。截断到 77(SOT+≤75 词+EOT),
    /// 不足用 EOT(49407)填充、attention_mask 对应置 0。
    pub fn embed_text(&mut self, s: &str) -> Result<Vec<f32>, String> {
        let mut ids = self.tok.encode(s);
        ids.truncate(CTX);
        let mut input_ids = vec![self.tok.eot as i64; CTX];
        let mut mask = vec![0i64; CTX];
        for (i, &t) in ids.iter().enumerate() {
            input_ids[i] = t as i64;
            mask[i] = 1;
        }
        let ids_t = Tensor::from_array(([1usize, CTX], input_ids)).map_err(|e| e.to_string())?;
        let mask_t = Tensor::from_array(([1usize, CTX], mask)).map_err(|e| e.to_string())?;
        let outputs = self
            .text
            .run(ort::inputs![
                "input_ids" => &ids_t,
                "attention_mask" => &mask_t
            ])
            .map_err(|e| t_with("clip text 运行: {e}", &[("e", e.to_string())]))?;
        let (_, vals): (&ort::value::Shape, &[f32]) = outputs["text_embeds"]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        if vals.len() != DIM {
            return Err(t_with(
                "clip 文本嵌入维度异常: {p1}",
                &[("p1", (vals.len()).to_string())],
            ));
        }
        l2_normalize(vals)
    }
}

/// Chinese-CLIP 图像编码器(visual.onnx;预处理与英文 CLIP 相同)。
pub struct CnClipVision {
    vision: Session,
}

impl CnClipVision {
    pub fn load() -> Result<Self, String> {
        let dir = models_dir().ok_or(t("无数据目录"))?;
        let p = dir.join("cn_clip_visual.onnx");
        if !p.is_file() {
            return Err(t_with("缺模型 {p1}", &[("p1", (p.display()).to_string())]));
        }
        let vision = Session::builder(env()?)
            .map_err(|e| t_with("会话构建: {e}", &[("e", e.to_string())]))?
            .commit_from_file(&p)
            .map_err(|e| t_with("加载 cn_clip_visual: {e}", &[("e", e.to_string())]))?;
        Ok(Self { vision })
    }

    /// 图像 → 512 维 L2 归一化嵌入。
    pub fn embed_image(&mut self, img: &image::DynamicImage) -> Result<Vec<f32>, String> {
        let data = preprocess_224(img)?;
        let input = Tensor::from_array(([1usize, 3, 224, 224], data)).map_err(|e| e.to_string())?;
        let outputs = self
            .vision
            .run(ort::inputs!["pixel_values" => &input])
            .map_err(|e| t_with("cn_clip vision 运行: {e}", &[("e", e.to_string())]))?;
        let (_, vals): (&ort::value::Shape, &[f32]) = outputs["image_features"]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        if vals.len() != DIM {
            return Err(t_with(
                "cn_clip 图像嵌入维度异常: {p1}",
                &[("p1", (vals.len()).to_string())],
            ));
        }
        l2_normalize(vals)
    }
}

/// Chinese-CLIP 文本编码器(text.onnx + Bert WordPiece 分词器)。
pub struct CnClipText {
    text: Session,
    tok: BertTokenizer,
}

impl CnClipText {
    pub fn load() -> Result<Self, String> {
        let dir = models_dir().ok_or(t("无数据目录"))?;
        let p = dir.join("cn_clip_text.onnx");
        if !p.is_file() {
            return Err(t_with("缺模型 {p1}", &[("p1", (p.display()).to_string())]));
        }
        let text = Session::builder(env()?)
            .map_err(|e| t_with("会话构建: {e}", &[("e", e.to_string())]))?
            .commit_from_file(&p)
            .map_err(|e| t_with("加载 cn_clip_text: {e}", &[("e", e.to_string())]))?;
        let tok = BertTokenizer::load(&dir.join("cn_clip_vocab.txt"))?;
        Ok(Self { text, tok })
    }

    /// 文本 → 512 维 L2 归一化嵌入。截断到 CN_CTX(52),PAD 填充 + mask 0。
    /// 注意:Chinese-CLIP 训练用模板"一张{}的照片",模板由调用方决定。
    pub fn embed_text(&mut self, s: &str) -> Result<Vec<f32>, String> {
        let mut ids = self.tok.encode(s);
        ids.truncate(CN_CTX);
        let mut input_ids = vec![0i64; CN_CTX];
        let mut mask = vec![0i64; CN_CTX];
        let ttype = vec![0i64; CN_CTX];
        for (i, &t) in ids.iter().enumerate() {
            input_ids[i] = t as i64;
            mask[i] = 1;
        }
        let ids_t = Tensor::from_array(([1usize, CN_CTX], input_ids)).map_err(|e| e.to_string())?;
        let mask_t = Tensor::from_array(([1usize, CN_CTX], mask)).map_err(|e| e.to_string())?;
        let type_t = Tensor::from_array(([1usize, CN_CTX], ttype)).map_err(|e| e.to_string())?;
        let outputs = self
            .text
            .run(ort::inputs![
                "input_ids" => &ids_t,
                "attention_mask" => &mask_t,
                "token_type_ids" => &type_t
            ])
            .map_err(|e| t_with("cn_clip text 运行: {e}", &[("e", e.to_string())]))?;
        let (_, vals): (&ort::value::Shape, &[f32]) = outputs["text_features"]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        if vals.len() != DIM {
            return Err(t_with(
                "cn_clip 文本嵌入维度异常: {p1}",
                &[("p1", (vals.len()).to_string())],
            ));
        }
        l2_normalize(vals)
    }
}

// ---------- 存储与检索 ----------

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(v.len() * 4);
    for x in v {
        b.extend_from_slice(&x.to_le_bytes());
    }
    b
}

fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// 语义模型语言(支持中文 CLIP;两个模型各自独立索引)。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Lang {
    En,
    Zh,
}

impl Lang {
    fn table(self) -> &'static str {
        match self {
            Lang::En => "clips",
            Lang::Zh => "clips_cn",
        }
    }
    /// 查询文本是否更像中文(含 CJK 字符即判中文,自动选模型用)。
    pub fn detect(query: &str) -> Lang {
        if query.chars().any(|c| is_chinese_char(c)) {
            Lang::Zh
        } else {
            Lang::En
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Lang::En => t("英文"),
            Lang::Zh => t("中文"),
        }
    }
}

/// 写入某语言模型的嵌入(维度必须为 DIM)。
pub fn save_clip_lang(store: &Store, lang: Lang, photo_id: i64, vec: &[f32]) -> Result<(), String> {
    if vec.len() != DIM {
        return Err(t_with(
            "嵌入维度 {p1} != {DIM}",
            &[("p1", (vec.len()).to_string()), ("DIM", DIM.to_string())],
        ));
    }
    let sql = format!(
        "INSERT OR REPLACE INTO {}(photo_id,dim,vec) VALUES(?1,?2,?3)",
        lang.table()
    );
    let blob = vec_to_blob(vec);
    store
        .write(move |conn| -> Result<(), String> {
            conn.execute(&sql, rusqlite::params![photo_id, vec.len() as i64, blob])
                .map_err(|e| e.to_string())
                .map(|_| ())
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 读取单张照片的嵌入(测试/调试用;GUI 走 search 全量比对)。
#[allow(dead_code)]
pub fn clip_vec(store: &Store, photo_id: i64) -> Option<Vec<f32>> {
    let b: Vec<u8> = store
        .read(|conn| {
            conn.query_row(
                "SELECT vec FROM clips WHERE photo_id=?1",
                rusqlite::params![photo_id],
                |r| r.get(0),
            )
        })
        .ok()?;
    Some(blob_to_vec(&b))
}

/// 某语言模型已嵌入的照片数(GUI 显示"已索引 x/y")。
pub fn clip_count_lang(store: &Store, lang: Lang) -> usize {
    store.read(|conn| {
        conn.query_row(&format!("SELECT COUNT(*) FROM {}", lang.table()), [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize
    })
}

/// 某语言模型下尚未嵌入的照片(增量索引用)。
pub fn photos_todo_lang(store: &Store, lang: Lang, cap: usize) -> Vec<(i64, String)> {
    let sql = format!(
        "SELECT id, path FROM photos
         WHERE id NOT IN (SELECT photo_id FROM {})
         ORDER BY id LIMIT ?1",
        lang.table()
    );
    store.read(|conn| {
        let mut st = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map(rusqlite::params![cap as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    })
}

/// 后台语义索引:按语言选模型(缺模型则报错,由 GUI 提示用户)。
pub fn scan_batch_lang(store: &Store, lang: Lang, cap: usize) -> Result<(usize, usize), String> {
    let todo = photos_todo_lang(store, lang, cap);
    let mut ok = 0usize;
    match lang {
        Lang::En => {
            let mut engine = ClipVision::load()?;
            for (pid, path) in &todo {
                let img = match image::open(path) {
                    Ok(i) => i,
                    Err(_) => continue,
                };
                if let Ok(v) = engine.embed_image(&img) {
                    let _ = save_clip_lang(store, lang, *pid, &v);
                    ok += 1;
                }
            }
        }
        Lang::Zh => {
            let mut engine = CnClipVision::load()?;
            for (pid, path) in &todo {
                let img = match image::open(path) {
                    Ok(i) => i,
                    Err(_) => continue,
                };
                if let Ok(v) = engine.embed_image(&img) {
                    let _ = save_clip_lang(store, lang, *pid, &v);
                    ok += 1;
                }
            }
        }
    }
    Ok((todo.len(), ok))
}

/// 新版自然语言检索:预处理 → MobileCLIP2 文本编码 → 向量/标签/OCR 融合。
pub fn search_natural(store: &Store, query: &str, limit: usize) -> Result<Vec<(i64, f32)>, String> {
    if !crate::ai_models::stage_ready(store, crate::ai_queue::Stage::Clip) {
        return Err(t("缺少 MobileCLIP2 模型").into());
    }
    let dict = crate::ai_text::Dict::load(store);
    let mut cache = crate::ai_text::QueryCache::default();
    // 词典优先;词典未覆盖的部分若装了 Opus-MT 再兜底翻译。
    let mut mt = crate::ai_text::MtTranslator::load(store).ok();
    let prepared = crate::ai_text::prepare(
        query,
        &dict,
        mt.as_mut()
            .map(|t| t as &mut dyn crate::ai_text::Translator),
        &mut cache,
    );
    let mut text = MobileClipText::load(store)?;
    let emb = text.embed_text(&prepared.prompt)?;
    let clip_ver = crate::ai_queue::stage_version(store, crate::ai_queue::Stage::Clip);
    let tag_ver = crate::ai_queue::stage_version(store, crate::ai_queue::Stage::Tags);
    let ids = crate::ai_search::hybrid(
        store,
        &clip_ver,
        &tag_ver,
        &prepared.stripped,
        Some(&emb),
        limit,
    );
    Ok(ids
        .into_iter()
        .enumerate()
        .map(|(i, id)| (id, 1.0 / (1.0 + i as f32)))
        .collect())
}

/// 新链路是否可用(只认新版产物文件)。
pub fn natural_ready(store: &Store) -> bool {
    crate::ai_models::stage_ready(store, crate::ai_queue::Stage::Clip)
}

/// 统一检索入口:两条链路的嵌入空间不同,只选一条,不混排。
pub fn search_best(
    store: &Store,
    query: &str,
    limit: usize,
) -> Result<(Vec<(i64, f32)>, bool), String> {
    if natural_ready(store) {
        search_natural(store, query, limit).map(|hits| (hits, true))
    } else {
        let lang = Lang::detect(query);
        search_lang(store, lang, query, limit).map(|hits| (hits, false))
    }
}

/// 当前生效链路的已索引张数。
pub fn progress(store: &Store, lang: Lang) -> usize {
    if natural_ready(store) {
        let ver = crate::ai_queue::stage_version(store, crate::ai_queue::Stage::Clip);
        crate::ai_search::count(store, &ver)
    } else {
        clip_count_lang(store, lang)
    }
}

/// 中文查询模板(模型输入,不随界面语言变化)。
pub fn zh_query_template(query: &str) -> String {
    if query.contains("照片") {
        query.to_string()
    } else {
        format!("一张{}的照片", query)
    }
}

/// 语义搜索:按语言选文本模型;中文查询自动套中文模板。
pub fn search_lang(
    store: &Store,
    lang: Lang,
    query: &str,
    limit: usize,
) -> Result<Vec<(i64, f32)>, String> {
    let q = match lang {
        Lang::En => {
            let mut engine = ClipText::load()?;
            engine.embed_text(query)?
        }
        Lang::Zh => {
            let q = zh_query_template(query);
            let mut engine = CnClipText::load()?;
            engine.embed_text(&q)?
        }
    };
    let rows: Vec<(i64, Vec<u8>)> = store
        .read(|conn| -> Result<Vec<(i64, Vec<u8>)>, String> {
            let mut st = conn
                .prepare(&format!("SELECT photo_id, vec FROM {}", lang.table()))
                .map_err(|e| e.to_string())?;
            let rows = st
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
                .map_err(|e| e.to_string())?;
            let mut out: Vec<(i64, Vec<u8>)> = Vec::new();
            for row in rows {
                if let Ok(r) = row {
                    out.push(r);
                }
            }
            Ok(out)
        })
        .map_err(|e| e.to_string())?;
    let mut hits: Vec<(i64, f32)> = Vec::new();
    for (pid, blob) in rows {
        let v = blob_to_vec(&blob);
        if v.len() != DIM {
            continue;
        }
        let s: f32 = q.iter().zip(v.iter()).map(|(a, b)| a * b).sum();
        if s.is_finite() {
            hits.push((pid, s));
        }
    }
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(limit);
    Ok(hits)
}

/// MobileCLIP2 图像预处理:短边缩到 256、居中裁剪、ToTensor(0~1,官方不做均值方差归一化)。
fn preprocess_256(img: &image::DynamicImage) -> Result<Vec<f32>, String> {
    const S: u32 = 256;
    let (w, h) = (img.width().max(1), img.height().max(1));
    let scale = S as f32 / w.min(h) as f32;
    let rw = ((w as f32 * scale).round() as u32).max(1);
    let rh = ((h as f32 * scale).round() as u32).max(1);
    let resized = img.resize_exact(rw, rh, image::imageops::FilterType::CatmullRom);
    let x0 = rw.saturating_sub(S) / 2;
    let y0 = rh.saturating_sub(S) / 2;
    let cropped = resized.crop_imm(x0, y0, S.min(rw), S.min(rh)).to_rgb8();
    let (cw, ch) = (cropped.width(), cropped.height());
    if cw != S || ch != S {
        return Err(t_with(
            "裁剪尺寸异常 {cw}x{ch}",
            &[("cw", cw.to_string()), ("ch", ch.to_string())],
        ));
    }
    let mut data = Vec::with_capacity((S * S * 3) as usize);
    for c in 0..3usize {
        for p in cropped.pixels() {
            data.push(p.0[c] as f32 / 255.0);
        }
    }
    Ok(data)
}

fn first_embedding(
    outputs: &ort::session::SessionOutputs<'_>,
    preferred: &str,
) -> Result<Vec<f32>, String> {
    let first = outputs.values().next();
    let v = outputs
        .get(preferred)
        .or(first.as_deref())
        .ok_or(t("模型无输出"))?;
    let (_, vals): (&ort::value::Shape, &[f32]) =
        v.try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
    l2_normalize(vals)
}

/// MobileCLIP2-S2 视觉编码器(槽位 clip_vision,输入 256)。
pub struct MobileClipVision {
    vision: Session,
}

impl MobileClipVision {
    /// 按槽位解析并加载。
    pub fn load(store: &Store) -> Result<Self, String> {
        let p = crate::ai_models::file_path(store, "clip_vision", "vision.onnx")
            .or_else(|| crate::ai_models::file_path(store, "clip_vision", "clip_vision.onnx"))
            .ok_or(t("缺少 MobileCLIP2 视觉模型"))?;
        let vision = Session::builder(env()?)
            .map_err(|e| t_with("会话构建: {e}", &[("e", e.to_string())]))?
            .commit_from_file(&p)
            .map_err(|e| t_with("加载视觉模型: {e}", &[("e", e.to_string())]))?;
        Ok(Self { vision })
    }

    /// 图像 → L2 归一化嵌入。
    pub fn embed_image(&mut self, img: &image::DynamicImage) -> Result<Vec<f32>, String> {
        let data = preprocess_256(img)?;
        let input = Tensor::from_array(([1usize, 3, 256, 256], data)).map_err(|e| e.to_string())?;
        let outputs = self
            .vision
            .run(ort::inputs!["pixel_values" => &input])
            .map_err(|e| t_with("视觉模型运行: {e}", &[("e", e.to_string())]))?;
        first_embedding(&outputs, "image_embeds")
    }
}

/// MobileCLIP2-S2 文本编码器(槽位 clip_text)。
pub struct MobileClipText {
    text: Session,
    tok: Tokenizer,
    /// 部分导出只声明 input_ids;有 attention_mask 输入时才一并喂入。
    has_mask: bool,
}

impl MobileClipText {
    /// 按槽位解析并加载(旧平铺 merges 自动回退)。
    pub fn load(store: &Store) -> Result<Self, String> {
        let p = crate::ai_models::file_path(store, "clip_text", "text.onnx")
            .or_else(|| crate::ai_models::file_path(store, "clip_text", "clip_text.onnx"))
            .ok_or(t("缺少 MobileCLIP2 文本模型"))?;
        let merges = p
            .parent()
            .map(|d| d.join("merges.txt"))
            .filter(|m| m.is_file())
            .or_else(|| crate::ai_models::file_path(store, "clip_text", "clip_merges.txt"))
            .ok_or(t("缺少分词表 merges.txt"))?;
        let text = Session::builder(env()?)
            .map_err(|e| t_with("会话构建: {e}", &[("e", e.to_string())]))?
            .commit_from_file(&p)
            .map_err(|e| t_with("加载文本模型: {e}", &[("e", e.to_string())]))?;
        let has_mask = text.inputs().iter().any(|i| i.name() == "attention_mask");
        let tok = Tokenizer::load(&merges)?;
        Ok(Self {
            text,
            tok,
            has_mask,
        })
    }

    /// 文本 → L2 归一化嵌入(截断到 77,不足用 pad(0)填充)。
    pub fn embed_text(&mut self, s: &str) -> Result<Vec<f32>, String> {
        let mut ids = self.tok.encode(s);
        ids.truncate(CTX);
        // 双向文本塔不屏蔽填充位时,填充值会参与注意力;官方分词器用 0 填充。
        let mut input_ids = vec![0i64; CTX];
        let mut mask = vec![0i64; CTX];
        for (i, &t) in ids.iter().enumerate() {
            input_ids[i] = t as i64;
            mask[i] = 1;
        }
        let ids_t = Tensor::from_array(([1usize, CTX], input_ids)).map_err(|e| e.to_string())?;
        let outputs = if self.has_mask {
            let mask_t = Tensor::from_array(([1usize, CTX], mask)).map_err(|e| e.to_string())?;
            self.text
                .run(ort::inputs!["input_ids" => &ids_t, "attention_mask" => &mask_t])
        } else {
            self.text.run(ort::inputs!["input_ids" => &ids_t])
        }
        .map_err(|e| t_with("文本模型运行: {e}", &[("e", e.to_string())]))?;
        first_embedding(&outputs, "text_embeds")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> Store {
        Store::open_mem().unwrap()
    }

    fn models_dir_present() -> bool {
        models_dir().map(|d| d.is_dir()).unwrap_or(false)
    }

    /// 中文查询模板是模型输入,不随界面语言变化。
    #[test]
    fn zh_prompt_is_language_independent() {
        let probe = |l: crate::i18n::Lang| {
            let _g = crate::i18n::LangGuard::to(l);
            (
                zh_query_template("一只猫"),
                zh_query_template("一张人像的照片"),
            )
        };
        assert_eq!(probe(crate::i18n::Lang::Zh), probe(crate::i18n::Lang::En));
        assert_eq!(zh_query_template("一只猫"), "一张一只猫的照片");
        assert_eq!(zh_query_template("一张人像的照片"), "一张人像的照片");
    }

    /// BertTokenizer 黄金序列。
    #[test]
    fn bert_tokenizer_golden() {
        let Some(dir) = models_dir() else { return };
        let vp = dir.join("cn_clip_vocab.txt");
        if !vp.is_file() {
            return;
        }
        let tok = BertTokenizer::load(&vp).unwrap();
        assert_eq!(tok.encode("一只猫"), vec![101, 671, 1372, 4344, 102]);
        assert_eq!(
            tok.encode("一张人像的照片"),
            vec![101, 671, 2476, 782, 1008, 4638, 4212, 4275, 102]
        );
        assert_eq!(
            tok.encode("Hello, World!"),
            vec![101, 8701, 117, 8572, 106, 102]
        );
        assert_eq!(tok.encode("猫和人。"), vec![101, 4344, 1469, 782, 511, 102]);
        // 语言探测:CJK → 中文
        assert_eq!(Lang::detect("一只猫"), Lang::Zh);
        assert_eq!(Lang::detect("a cat"), Lang::En);
        assert_eq!(Lang::detect("猫 cat"), Lang::Zh);
    }

    /// clips_cn 表 roundtrip(无模型也能跑;中英索引互不干扰)。
    #[test]
    fn cn_clip_db_roundtrip() {
        let store = test_store();
        {
            let conn = store.test_conn();
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(2,'b.jpg','.',0,0)",
                [],
            )
            .unwrap();
        }
        let v = vec![0.25f32; DIM];
        save_clip_lang(&store, Lang::Zh, 1, &v).unwrap();
        assert_eq!(clip_count_lang(&store, Lang::Zh), 1);
        assert_eq!(
            clip_count_lang(&store, Lang::En),
            0,
            "英文索引不受中文写入影响"
        );
        assert!(save_clip_lang(&store, Lang::Zh, 2, &[0.0; 8]).is_err());
        // photos_todo 按语言过滤
        let todo = photos_todo_lang(&store, Lang::Zh, 10);
        assert_eq!(todo.len(), 1, "已嵌入的照片不应再出现");
        assert_eq!(todo[0].0, 2);
    }

    /// 中文模型质量 — face-test.jpg(人像)对"人像"类查询余弦
    /// 应显著高于"猫/狗"类。
    #[test]
    fn cn_clip_search_ranks() {
        let Some(dir) = models_dir() else { return };
        let fixture = dir.join("face-test.jpg");
        if !fixture.is_file() || !dir.join("cn_clip_vocab.txt").is_file() {
            return;
        }
        let img = image::ImageReader::open(&fixture)
            .unwrap()
            .decode()
            .unwrap();
        let mut vis = CnClipVision::load().unwrap();
        let iv = vis.embed_image(&img).unwrap();
        let mut te = CnClipText::load().unwrap();
        let person = te.embed_text("一张人像的照片").unwrap();
        let cat = te.embed_text("一只猫的照片").unwrap();
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let (sp, sc) = (dot(&person, &iv), dot(&cat, &iv));
        eprintln!("cn_clip: 人像 {sp:.4} vs 猫 {sc:.4}");
        assert!(sp > 0.3, "人像查询余弦 {sp:.4} 应 > 0.3");
        assert!(sp > sc + 0.02, "人像({sp:.4}) 应显著高于猫({sc:.4})");
    }

    #[test]
    fn tokenizer_structure() {
        if !models_dir_present() {
            return;
        }
        let dir = models_dir().unwrap();
        let mp = dir.join("clip_merges.txt");
        if !mp.is_file() {
            return;
        }
        let tok = Tokenizer::load(&mp).unwrap();
        assert_eq!(tok.sot, 49406);
        assert_eq!(tok.eot, 49407);
        // 黄金序列:
        // 'a photo of a cat' -> [49406, 320, 1125, 539, 320, 2368, 49407]
        assert_eq!(
            tok.encode("a photo of a cat"),
            vec![49406, 320, 1125, 539, 320, 2368, 49407]
        );
        // 缩写 + 单字符数字 + 符号:
        // "it's a test, 123!" -> [49406, 585, 568, 320, 1628, 267, 272, 273, 274, 256, 49407]
        assert_eq!(
            tok.encode("it's a test, 123!"),
            vec![49406, 585, 568, 320, 1628, 267, 272, 273, 274, 256, 49407]
        );
        // 大小写不敏感:'A PHOTO of a Dog' 与 'a photo of a dog' 同序
        assert_eq!(
            tok.encode("A PHOTO of a Dog"),
            tok.encode("a photo of a dog")
        );
    }

    #[test]
    fn clip_db_roundtrip() {
        let store = test_store();
        {
            let conn = store.test_conn();
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.','0',1)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(2,'b.jpg','.','0',1)",
                [],
            )
            .unwrap();
        }
        let v = vec![0.5f32; DIM];
        save_clip_lang(&store, Lang::En, 1, &v).unwrap();
        assert_eq!(clip_vec(&store, 1), Some(v));
        assert!(clip_vec(&store, 2).is_none());
        assert_eq!(clip_count_lang(&store, Lang::En), 1);
        // 维度不符应报错
        assert!(save_clip_lang(&store, Lang::En, 2, &[0.0; 8]).is_err());
    }

    #[test]
    fn clip_text_embed() {
        if !models_dir_present() {
            return;
        }
        let dir = models_dir().unwrap();
        if !dir.join("clip_text.onnx").is_file() {
            return;
        }
        let mut e = ClipText::load().unwrap();
        let v = e.embed_text("a photo of a cat").unwrap();
        assert_eq!(v.len(), DIM);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.01, "范数应≈1,实际 {norm}");
        // 语义合理性:狗-猫 比 狗-飞机 更近
        let dog = e.embed_text("a photo of a dog").unwrap();
        let cat = e.embed_text("a photo of a cat").unwrap();
        let plane = e.embed_text("a photo of an airplane").unwrap();
        let dc: f32 = dog.iter().zip(&cat).map(|(a, b)| a * b).sum();
        let dp: f32 = dog.iter().zip(&plane).map(|(a, b)| a * b).sum();
        assert!(dc > dp, "dog-cat={dc} 应 > dog-plane={dp}");
    }

    #[test]
    fn clip_image_embed() {
        if !models_dir_present() {
            return;
        }
        let dir = models_dir().unwrap();
        let fixture = dir.join("face-test.jpg");
        if !fixture.is_file() || !dir.join("clip_vision.onnx").is_file() {
            return;
        }
        let mut e = ClipVision::load().unwrap();
        let img = image::ImageReader::open(&fixture)
            .unwrap()
            .decode()
            .unwrap();
        let v = e.embed_image(&img).unwrap();
        assert_eq!(v.len(), DIM);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.01, "范数应≈1,实际 {norm}");
    }

    #[test]
    fn clip_search_ranks() {
        if !models_dir_present() {
            return;
        }
        let dir = models_dir().unwrap();
        let fixture = dir.join("face-test.jpg");
        if !fixture.is_file()
            || !dir.join("clip_vision.onnx").is_file()
            || !dir.join("clip_text.onnx").is_file()
        {
            return;
        }
        let store = test_store();
        {
            let conn = store.test_conn();
            for (id, path) in [(1i64, "x.jpg"), (2, "y.jpg")] {
                conn.execute(
                    "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.','0',1)",
                    rusqlite::params![id, path],
                )
                .unwrap();
            }
        }
        let mut vis = ClipVision::load().unwrap();
        let img = image::ImageReader::open(&fixture)
            .unwrap()
            .decode()
            .unwrap();
        let v = vis.embed_image(&img).unwrap();
        save_clip_lang(&store, Lang::En, 1, &v).unwrap();
        // photo 2:与 v 严格正交的确定性向量(前后半交换+负号,点积恒 0)
        let mut o = vec![0.0f32; DIM];
        for i in 0..DIM / 2 {
            o[i] = -v[DIM / 2 + i];
            o[DIM / 2 + i] = v[i];
        }
        save_clip_lang(&store, Lang::En, 2, &o).unwrap();
        // decoy:由 v 旋转得到的确定性向量,与任何文本查询的期望相关度都低
        let hits = search_lang(&store, Lang::En, "a photo of a person", 10).unwrap();
        assert_eq!(hits.len(), 2, "应返回 2 条");
        assert_eq!(hits[0].0, 1, "人脸照片应排第一");
        assert!(hits[0].1 > 0.15, "图文余弦应 > 0.15,实际 {}", hits[0].1);
        assert_eq!(hits[1].0, 2);
        assert!(
            hits[1].1 < hits[0].1,
            "无关照片分数应更低:{} < {}",
            hits[1].1,
            hits[0].1
        );
    }
}
