//! Picasa.ini 互操作(保守子集):
//!   [图片名] 节内写 rotate= 与 star=。
//! Picasa 真实格式尚未完全确认(rect64 未验证);仅保证我们写出的
//! 内容可以被本程序自己读回。XMP sidecar 仍是权威通道。
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

const NAMES: [&str; 2] = ["picasa.ini", ".picasa.ini"];
const OUR_MARKER: &str = "; written by Relyto.Maliang";

type Sections = BTreeMap<String, Vec<(String, String)>>; // 节名 → 键值(有序)

fn ini_path(folder: &Path) -> Option<std::path::PathBuf> {
    NAMES.iter().map(|n| folder.join(n)).find(|p| p.exists())
}

fn parse(text: &str) -> Sections {
    let mut out: Sections = BTreeMap::new();
    let mut cur: Option<String> = None;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with(';') || l.is_empty() {
            continue;
        }
        if let Some(name) = l.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            cur = Some(name.trim().to_string());
            out.entry(cur.clone().unwrap()).or_default();
        } else if let Some(c) = &cur {
            if let Some((k, v)) = l.split_once('=') {
                out.entry(c.clone()).or_default().push((
                    k.trim().to_ascii_lowercase(),
                    v.trim().to_string(),
                ));
            }
        }
    }
    out
}

fn serialize(sections: &Sections) -> String {
    let mut s = String::from(OUR_MARKER);
    s.push('\n');
    for (name, kvs) in sections {
        s.push('[');
        s.push_str(name);
        s.push_str("]\n");
        for (k, v) in kvs {
            s.push_str(k);
            s.push('=');
            s.push_str(v);
            s.push('\n');
        }
    }
    s
}

fn write_all(folder: &Path, sections: &Sections) -> Result<()> {
    let p = folder.join(NAMES[0]);
    std::fs::write(&p, serialize(sections)).with_context(|| format!("写 picasa.ini 失败: {}", p.display()))?;
    Ok(())
}

/// 设置一张图的 star(Option)与 rotate 度数(Option:0/90/180/270,None=保留原值)。
pub fn upsert(folder: &Path, name: &str, star: Option<bool>, rotate: Option<u8>) -> Result<()> {
    let mut sections: Sections = parse(
        &ini_path(folder)
            .map(|p| std::fs::read_to_string(p).unwrap_or_default())
            .unwrap_or_default(),
    );
    let prev_rot: Option<u8> = sections
        .get(name)
        .and_then(|kv| kv.iter().find(|(k, _)| k == "rotate"))
        .and_then(|(_, v)| v.parse::<u8>().ok());
    let entry = sections.entry(name.to_string()).or_default();
    entry.retain(|(k, _)| k != "star" && k != "rotate");
    if let Some(s) = star {
        if s {
            entry.push(("star".into(), "yes".into()));
        }
    }
    if let Some(r) = rotate.or(prev_rot) {
        entry.push(("rotate".into(), r.to_string()));
    }
    if entry.is_empty() {
        sections.remove(name);
    }
    write_all(folder, &sections)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let tmp = std::env::temp_dir().join(format!("rk-ini-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        upsert(&tmp, "A.jpg", Some(true), Some(90)).unwrap();
        upsert(&tmp, "B.jpg", Some(false), None).unwrap();
        let text = std::fs::read_to_string(tmp.join("picasa.ini")).unwrap();
        let secs = parse(&text);
        assert!(secs["A.jpg"].contains(&("star".into(), "yes".into())));
        // B.jpg 无 star 且无 rotate → 节为空被清除(读取时视为未加星,等价)
        assert!(!secs.contains_key("B.jpg"));
        assert!(secs["A.jpg"].contains(&("rotate".into(), "90".into())));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
