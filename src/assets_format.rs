//! 资源包格式(构建期与运行期共用)。
//!
//! 资源包是一个小端二进制容器:一段 magic + 版本号 + 若干「段」。
//! 每段由「名字 + 一串 f64 数值」组成,按名字寻址。
//! 文本形式(`assets/presets.txt`)是同一份数据的手写可读版本,
//! 构建期由它生成二进制,运行期直接读二进制。
//!
//! 数值一律用 **f64** 存储:调用方按需转成 f32/f64,
//! 避免在容器里就损失精度。

use std::collections::BTreeMap;

/// 容器 magic。
pub const MAGIC: &[u8; 8] = b"MLNGASST";
/// 容器版本。
pub const VERSION: u16 = 1;

/// 段表:段名 → 数值列表。
pub type Sections = BTreeMap<String, Vec<f64>>;

/// 解析文本形式的资源。
pub fn parse_sections(text: &str) -> Result<Sections, String> {
    let mut map: Sections = BTreeMap::new();
    let mut cur: Option<String> = None;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            let name = line
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_string();
            if name.is_empty() {
                return Err(format!("第 {} 行段名为空", i + 1));
            }
            map.entry(name.clone()).or_default();
            cur = Some(name);
            continue;
        }
        let Some(name) = cur.clone() else {
            return Err(format!("第 {} 行的数值没有所属段", i + 1));
        };
        let vals = map.get_mut(&name).expect("段已存在");
        for tok in line.split(|c: char| c == ',' || c.is_whitespace()) {
            if tok.is_empty() {
                continue;
            }
            let v: f64 = tok
                .parse()
                .map_err(|_| format!("第 {} 行数值 {:?} 无法解析", i + 1, tok))?;
            vals.push(v);
        }
    }
    Ok(map)
}

/// 把段表编码为二进制容器。
pub fn encode(sections: &Sections) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(sections.len() as u16).to_le_bytes());
    for (name, vals) in sections {
        let nb = name.as_bytes();
        out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        out.extend_from_slice(nb);
        out.extend_from_slice(&(vals.len() as u32).to_le_bytes());
        for v in vals {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

/// 解码二进制容器;格式不符时返回 `None`。
pub fn decode(bytes: &[u8]) -> Option<Sections> {
    let mut p = 0usize;
    let take = |p: &mut usize, n: usize| -> Option<&[u8]> {
        let s = bytes.get(*p..*p + n)?;
        *p += n;
        Some(s)
    };
    if take(&mut p, MAGIC.len())? != MAGIC {
        return None;
    }
    let ver = take(&mut p, 2)?;
    if u16::from_le_bytes([ver[0], ver[1]]) != VERSION {
        return None;
    }
    let cnt = take(&mut p, 2)?;
    let count = u16::from_le_bytes([cnt[0], cnt[1]]) as usize;
    let mut map = Sections::new();
    for _ in 0..count {
        let nl = take(&mut p, 2)?;
        let nlen = u16::from_le_bytes([nl[0], nl[1]]) as usize;
        let name_bytes = take(&mut p, nlen)?;
        let name = std::str::from_utf8(name_bytes).ok()?.to_string();
        let vl = take(&mut p, 4)?;
        let vlen = u32::from_le_bytes([vl[0], vl[1], vl[2], vl[3]]) as usize;
        let mut vals = Vec::with_capacity(vlen);
        for _ in 0..vlen {
            let b = take(&mut p, 8)?;
            vals.push(f64::from_le_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ]));
        }
        map.insert(name, vals);
    }
    if p != bytes.len() {
        return None;
    }
    Some(map)
}
