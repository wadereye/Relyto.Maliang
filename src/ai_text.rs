//! 查询预处理:指令词剥离、词典替换、中→英翻译、prompt 模板与结果缓存。

use crate::ai::env;
use crate::db::Store;
use ort::session::Session;
use ort::value::Tensor;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

/// 指令词(按长度降序剥离,避免"的照片"被"照片"抢先)。
const FILLERS: &[&str] = &[
    "帮我找一下",
    "帮我找",
    "帮我搜",
    "找一下",
    "来一张",
    "给我看看",
    "的照片",
    "的图片",
    "图片",
    "照片",
    "一下",
    "有可能是",
    "可能是",
    "看起来像",
    "关于",
    "那些",
    "这些",
    "所有",
    "几张",
    "请问",
    "我想看",
    "看看",
];

/// 内置词典(中文词, 英文片段)。
const BUILTIN_DICT: &[(&str, &str)] = &[
    ("身份证", "ID card"),
    ("发票", "invoice"),
    ("收据", "receipt"),
    ("截图", "screenshot"),
    ("二维码", "QR code"),
    ("白板", "whiteboard"),
    ("海边", "beach"),
    ("沙滩", "beach"),
    ("夜景", "night view"),
    ("日落", "sunset"),
    ("日出", "sunrise"),
    ("烟花", "fireworks"),
    ("咖啡", "coffee"),
    ("蛋糕", "cake"),
    ("火锅", "hot pot"),
    ("烧烤", "barbecue"),
    ("面条", "noodles"),
    ("米饭", "rice"),
    ("早餐", "breakfast"),
    ("猫", "cat"),
    ("狗", "dog"),
    ("鸟", "bird"),
    ("汽车", "car"),
    ("自行车", "bicycle"),
    ("摩托车", "motorcycle"),
    ("飞机", "airplane"),
    ("火车", "train"),
    ("花", "flower"),
    ("书", "book"),
    ("手机", "smartphone"),
    ("电脑", "computer"),
    ("相机", "camera"),
    ("婚礼", "wedding"),
    ("合影", "group photo"),
    ("自拍", "selfie"),
    ("孩子", "child"),
    ("老人", "elderly person"),
    ("雪山", "snow mountain"),
    ("森林", "forest"),
    ("湖泊", "lake"),
    ("河流", "river"),
    ("城市", "city"),
    ("街道", "street"),
    ("建筑", "building"),
    ("室内", "indoor"),
    ("办公室", "office"),
    ("教室", "classroom"),
    ("宠物", "pet"),
    ("旅行", "travel"),
    ("聚会", "party"),
    ("运动", "sports"),
    ("游泳", "swimming"),
    ("跑步", "running"),
    ("骑行", "cycling"),
    ("登山", "hiking"),
];

/// 判断是否含中日韩统一表意文字。
pub fn has_cjk(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF))
}

/// 是否纯英文查询(无 CJK),可跳过翻译。
pub fn is_english(s: &str) -> bool {
    !has_cjk(s) && s.chars().any(|c| c.is_ascii_alphabetic())
}

/// 剥离口语指令词并压空白。
pub fn strip_instruction(q: &str) -> String {
    let mut s = q.trim().to_string();
    for f in FILLERS {
        s = s.replace(f, " ");
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 词典(内置 + 数据库覆盖)。
pub struct Dict {
    map: HashMap<String, String>,
}

impl Dict {
    /// 从库与内置词典装配。
    pub fn load(store: &Store) -> Self {
        let mut map: HashMap<String, String> = BUILTIN_DICT
            .iter()
            .map(|(zh, en)| (zh.to_string(), en.to_string()))
            .collect();
        store.read(|conn| {
            if let Ok(mut st) = conn.prepare("SELECT term_zh, term_en FROM ai_dict") {
                if let Ok(rows) =
                    st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                {
                    for (zh, en) in rows.flatten() {
                        map.insert(zh, en);
                    }
                }
            }
        });
        Dict { map }
    }

    /// 最长匹配替换,返回(替换后的文本, 命中的中文词)。
    pub fn replace(&self, s: &str) -> (String, Vec<String>) {
        let mut out = String::new();
        let mut hits = Vec::new();
        let chars: Vec<char> = s.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let mut matched: Option<(usize, &String)> = None;
            for len in (1..=chars.len() - i).rev() {
                let seg: String = chars[i..i + len].iter().collect();
                if let Some(en) = self.map.get(&seg) {
                    matched = Some((len, en));
                    break;
                }
            }
            if let Some((len, en)) = matched {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(en);
                hits.push(chars[i..i + len].iter().collect());
                i += len;
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        (out, hits)
    }
}

/// 中→英翻译器接口。
pub trait Translator {
    fn translate(&mut self, zh: &str) -> Result<String, String>;
}

/// Opus-MT 词表(每行一个 sentencepiece 符号,行号即 id)。
pub struct MtVocab {
    tokens: Vec<String>,
    map: HashMap<String, u32>,
    max_len: usize,
}

impl MtVocab {
    /// 从 vocab.txt 加载。
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("读 vocab: {e}"))?;
        let mut tokens: Vec<String> = Vec::new();
        let mut map: HashMap<String, u32> = HashMap::new();
        for line in text.split('\n') {
            let t = line.strip_suffix('\r').unwrap_or(line);
            map.entry(t.to_string()).or_insert(tokens.len() as u32);
            tokens.push(t.to_string());
        }
        let max_len = tokens
            .iter()
            .map(|t| t.chars().count())
            .max()
            .unwrap_or(1)
            .min(32);
        Ok(Self {
            tokens,
            map,
            max_len,
        })
    }

    /// 特殊符号 id;缺失时用兜底值。
    fn id_of(&self, tok: &str, fallback: i64) -> i64 {
        self.map.get(tok).map(|&v| v as i64).unwrap_or(fallback)
    }

    /// 贪心最长匹配编码;英文首字符前补 sentencepiece 词界符。
    pub fn encode(&self, text: &str) -> Vec<u32> {
        let unk = self.id_of("<unk>", 2) as u32;
        let mut chars: Vec<char> = text.trim().replace(' ', "\u{2581}").chars().collect();
        if chars.is_empty() {
            return Vec::new();
        }
        if chars[0].is_ascii_alphanumeric() {
            chars.insert(0, '\u{2581}');
        }
        let mut out = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let mut hit: Option<(usize, u32)> = None;
            let max = self.max_len.min(chars.len() - i);
            for len in (1..=max).rev() {
                let seg: String = chars[i..i + len].iter().collect();
                if let Some(&id) = self.map.get(&seg) {
                    hit = Some((len, id));
                    break;
                }
            }
            match hit {
                Some((len, id)) => {
                    out.push(id);
                    i += len;
                }
                None => {
                    out.push(unk);
                    i += 1;
                }
            }
        }
        out
    }

    /// 拼回文本(跳过特殊符号,▁ 视作空格)。
    pub fn decode(&self, ids: &[i64]) -> String {
        let mut out = String::new();
        for &id in ids {
            let Some(t) = self.tokens.get(id as usize) else {
                continue;
            };
            if matches!(t.as_str(), "<pad>" | "</s>" | "<s>" | "<unk>") {
                continue;
            }
            out.push_str(t);
        }
        out.replace('\u{2581}', " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// 取模型输出里的一路 f32 张量(平铺成 Vec)。
fn take_tensor(
    outs: &ort::session::SessionOutputs<'_>,
    preferred: &str,
) -> Result<Vec<f32>, String> {
    let first = outs.values().next();
    let v = outs
        .get(preferred)
        .or(first.as_deref())
        .ok_or("模型无输出")?;
    let (_, vals): (&ort::value::Shape, &[f32]) =
        v.try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
    Ok(vals.to_vec())
}

/// 贪心解码的最大步数。
const MT_MAX_DEC: usize = 48;

/// 一次解码步;alt=true 时用 decoder_input_ids 命名(兼容不同导出)。
fn decode_attempt(
    session: &mut Session,
    enc: &Tensor<f32>,
    mask: &Tensor<i64>,
    ids: &[i64],
    alt: bool,
) -> Result<Vec<f32>, String> {
    let d = Tensor::from_array(([1usize, ids.len()], ids.to_vec())).map_err(|e| e.to_string())?;
    let outs = if alt {
        session.run(ort::inputs![
            "encoder_hidden_states" => enc,
            "encoder_attention_mask" => mask,
            "decoder_input_ids" => &d
        ])
    } else {
        session.run(ort::inputs![
            "encoder_hidden_states" => enc,
            "encoder_attention_mask" => mask,
            "input_ids" => &d
        ])
    }
    .map_err(|e| format!("Opus-MT 解码失败: {e}"))?;
    take_tensor(&outs, "logits")
}

/// Opus-MT 中→英(encoder/decoder 双会话)。
pub struct MtTranslator {
    encoder: Session,
    decoder: Session,
    vocab: MtVocab,
    start: i64,
    eos: i64,
}

impl MtTranslator {
    /// 按槽位解析并加载;模型不齐时返回错误(调用方回退词典)。
    pub fn load(store: &Store) -> Result<Self, String> {
        let pick = |primary: &str, legacy: &str| -> Option<PathBuf> {
            crate::ai_models::file_path(store, "mt_zh_en", primary)
                .filter(|p| p.is_file())
                .or_else(|| {
                    crate::ai_models::file_path(store, "mt_zh_en", legacy).filter(|p| p.is_file())
                })
        };
        let enc = pick("encoder.onnx", "mt_encoder.onnx").ok_or("缺少 Opus-MT 编码器")?;
        let dec = pick("decoder.onnx", "mt_decoder.onnx").ok_or("缺少 Opus-MT 解码器")?;
        let vocab_path = pick("vocab.txt", "mt_vocab.txt").ok_or("缺少 Opus-MT 词表")?;
        let encoder = Session::builder(env()?)
            .map_err(|e| format!("会话构建: {e}"))?
            .commit_from_file(&enc)
            .map_err(|e| format!("加载 Opus-MT 编码器: {e}"))?;
        let decoder = Session::builder(env()?)
            .map_err(|e| format!("会话构建: {e}"))?
            .commit_from_file(&dec)
            .map_err(|e| format!("加载 Opus-MT 解码器: {e}"))?;
        let vocab = MtVocab::load(&vocab_path)?;
        let start = vocab.id_of("<pad>", 0);
        let eos = vocab.id_of("</s>", 2);
        Ok(Self {
            encoder,
            decoder,
            vocab,
            start,
            eos,
        })
    }

    fn translate_text(&mut self, zh: &str) -> Result<String, String> {
        let ids = self.vocab.encode(zh);
        if ids.is_empty() {
            return Err("空查询".into());
        }
        let seq = ids.len();
        let input = Tensor::from_array((
            [1usize, seq],
            ids.iter().map(|&v| v as i64).collect::<Vec<_>>(),
        ))
        .map_err(|e| e.to_string())?;
        let mask =
            Tensor::from_array(([1usize, seq], vec![1i64; seq])).map_err(|e| e.to_string())?;
        let enc_hidden = {
            let outs = self
                .encoder
                .run(ort::inputs!["input_ids" => &input, "attention_mask" => &mask])
                .map_err(|e| format!("Opus-MT 编码失败: {e}"))?;
            take_tensor(&outs, "last_hidden_state")?
        };
        let hidden = enc_hidden.len() / seq;
        if hidden == 0 {
            return Err("Opus-MT 编码输出异常".into());
        }
        let enc_t =
            Tensor::from_array(([1usize, seq, hidden], enc_hidden)).map_err(|e| e.to_string())?;
        let mut dec: Vec<i64> = vec![self.start];
        for _ in 0..MT_MAX_DEC {
            let dlen = dec.len();
            let logits = match decode_attempt(&mut self.decoder, &enc_t, &mask, &dec, false) {
                Ok(v) => v,
                Err(_) => decode_attempt(&mut self.decoder, &enc_t, &mask, &dec, true)?,
            };
            let vocab = logits.len() / dlen;
            if vocab == 0 {
                return Err("Opus-MT 解码输出异常".into());
            }
            let last = &logits[(dlen - 1) * vocab..dlen * vocab];
            let mut best = 0usize;
            for (i, &x) in last.iter().enumerate() {
                if x > last[best] {
                    best = i;
                }
            }
            if best as i64 == self.eos {
                break;
            }
            dec.push(best as i64);
        }
        dec.remove(0);
        let text = self.vocab.decode(&dec);
        if text.is_empty() {
            Err("无翻译结果".into())
        } else {
            Ok(text)
        }
    }
}

impl Translator for MtTranslator {
    fn translate(&mut self, zh: &str) -> Result<String, String> {
        self.translate_text(zh)
    }
}

/// 英文 prompt 模板。
pub fn prompt_template(en: &str) -> String {
    let s = en.trim();
    if s.is_empty() {
        "a photo".into()
    } else if s.to_ascii_lowercase().starts_with("a ") || s.to_ascii_lowercase().starts_with("an ")
    {
        s.to_string()
    } else {
        format!("a photo of {s}")
    }
}

/// 预处理结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub raw: String,
    pub stripped: String,
    pub translated: bool,
    pub prompt: String,
    pub cache_hit: bool,
}

/// 最近 50 条查询的缓存(含翻译与 prompt)。
#[derive(Default)]
pub struct QueryCache {
    entries: VecDeque<(String, Prepared)>,
}

impl QueryCache {
    /// 缓存容量。
    pub const CAP: usize = 50;

    /// 取缓存并移到最近。
    pub fn get(&mut self, raw: &str) -> Option<Prepared> {
        let pos = self.entries.iter().position(|(k, _)| k == raw)?;
        let (k, v) = self.entries.remove(pos)?;
        let mut hit = v.clone();
        hit.cache_hit = true;
        self.entries.push_front((k, v));
        Some(hit)
    }

    /// 写入缓存(按容量逐出最旧)。
    pub fn put(&mut self, raw: &str, p: Prepared) {
        if let Some(pos) = self.entries.iter().position(|(k, _)| k == raw) {
            self.entries.remove(pos);
        }
        self.entries.push_front((raw.to_string(), p));
        while self.entries.len() > Self::CAP {
            self.entries.pop_back();
        }
    }
}

/// 全链路预处理:剥离 → 词典 → 未知中文交翻译器 → 模板。
pub fn prepare(
    raw: &str,
    dict: &Dict,
    tr: Option<&mut dyn Translator>,
    cache: &mut QueryCache,
) -> Prepared {
    if let Some(hit) = cache.get(raw) {
        return hit;
    }
    let stripped = strip_instruction(raw);
    let (mut en, hits) = dict.replace(&stripped);
    let mut translated = !hits.is_empty();
    if !is_english(&en) {
        if let Some(t) = tr {
            if let Ok(x) = t.translate(&en) {
                en = x;
                translated = true;
            }
        }
    }
    let prompt = prompt_template(&en);
    let p = Prepared {
        raw: raw.to_string(),
        stripped,
        translated,
        prompt,
        cache_hit: false,
    };
    cache.put(raw, p.clone());
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_stripping_keeps_content() {
        let s = strip_instruction("帮我找一下身份证的照片");
        assert!(!s.contains("帮我"));
        assert!(!s.contains("一下"));
        assert!(s.contains("身份证"));
    }

    #[test]
    fn dictionary_uses_longest_match() {
        let store = Store::open_mem().unwrap();
        let d = Dict::load(&store);
        let (out, hits) = d.replace("海边的自拍");
        assert!(out.contains("beach"));
        assert!(out.contains("selfie"));
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn prepare_templates_and_caches() {
        let store = Store::open_mem().unwrap();
        let d = Dict::load(&store);
        let mut cache = QueryCache::default();
        let p = prepare("海边", &d, None, &mut cache);
        assert!(p.prompt.starts_with("a photo of"));
        assert!(p.translated);
        let again = prepare("海边", &d, None, &mut cache);
        assert!(again.cache_hit);
        assert_eq!(again.prompt, p.prompt);
    }

    /// 查询预处理是模型输入,不随界面语言变化。
    #[test]
    fn prompt_pipeline_is_language_independent() {
        let store = Store::open_mem().unwrap();
        let d = Dict::load(&store);
        let probe = |l: crate::i18n::Lang| {
            let _g = crate::i18n::LangGuard::to(l);
            let mut cache = QueryCache::default();
            let p = prepare("帮我找一下海边的自拍", &d, None, &mut cache);
            (p.prompt, p.stripped, prompt_template("beach"))
        };
        assert_eq!(probe(crate::i18n::Lang::Zh), probe(crate::i18n::Lang::En));
    }

    #[test]
    fn english_skips_translation() {
        let store = Store::open_mem().unwrap();
        let d = Dict::load(&store);
        let mut cache = QueryCache::default();
        let p = prepare("a cat on the beach", &d, None, &mut cache);
        assert_eq!(p.prompt, "a cat on the beach", "英文原样进入文本编码");
        assert!(!p.translated);
    }

    #[test]
    fn cache_evicts_oldest() {
        let mut c = QueryCache::default();
        for i in 0..(QueryCache::CAP + 10) {
            let p = Prepared {
                raw: format!("q{i}"),
                stripped: String::new(),
                translated: false,
                prompt: String::new(),
                cache_hit: false,
            };
            c.put(&format!("q{i}"), p);
        }
        assert_eq!(c.entries.len(), QueryCache::CAP);
        assert!(c.get("q0").is_none());
    }

    #[test]
    fn mt_vocab_longest_match_and_unknown() {
        let dir =
            std::env::temp_dir().join(format!("maliang-mt-vocab-{}", crate::ai_queue::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("vocab.txt");
        std::fs::write(&p, "<pad>\n</s>\n<unk>\n海\n海滩\n▁cat\ncat\n").unwrap();
        let v = MtVocab::load(&p).unwrap();
        assert_eq!(v.encode("海滩"), vec![4], "最长匹配优先");
        assert_eq!(v.encode("cat"), vec![5], "英文补词界符");
        assert_eq!(v.encode("月"), vec![2], "未登录字符落 unk");
        assert_eq!(v.decode(&[6, 5]), "cat cat");
    }
}
