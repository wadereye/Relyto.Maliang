//! XMP sidecar 互操作:星标(Rating 0/5)与标签(dc:subject Bag)。
//! 文件名:`<原图名>.xmp`,写在原图同目录。
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub struct Meta {
    pub star: i32,
    pub tags: Vec<String>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn sidecar_path(photo: &Path) -> PathBuf {
    photo.with_extension("xmp")
}

pub fn write(photo: &Path, meta: &Meta) -> Result<()> {
    let tag_lis: String = meta
        .tags
        .iter()
        .map(|t| format!("       <rdf:li>{}</rdf:li>\n", esc(t)))
        .collect();
    let xml = format!(
        r#"<?xpacket begin="﻿" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/">
   <xmp:Rating>{rating}</xmp:Rating>
   <dc:subject>
    <rdf:Bag>
{tag_lis}    </rdf:Bag>
   </dc:subject>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#,
        rating = if meta.star > 0 { 5 } else { 0 },
        tag_lis = tag_lis
    );
    let sp = sidecar_path(photo);
    let tmp = sp.with_extension("xmp.rk-tmp");
    std::fs::write(&tmp, xml.as_bytes())?;
    std::fs::rename(&tmp, &sp).with_context(|| format!("写入 XMP sidecar 失败: {}", sp.display()))?;
    Ok(())
}

/// 解析 sidecar;缺失时返回全零默认。
/// 当前写路径以数据库为权威(见 app::sync_sidecars),此读取能力预留给
/// 未来的 XMP 导入(把外部工具写的标签/星标读入索引)。
#[allow(dead_code)]
pub fn read(photo: &Path) -> Meta {
    let sp = sidecar_path(photo);
    let default = Meta { star: 0, tags: Vec::new() };
    let data = match std::fs::read(&sp) {
        Ok(d) => d,
        Err(_) => return default,
    };
    let text = String::from_utf8_lossy(&data);
    // 简化的区间解析,足够我们自产文件
    let mut star = 0;
    for m in ["<xmp:Rating>5</xmp:Rating>", "<xmp:Rating>4</xmp:Rating>"] {
        if text.contains(m) {
            star = 1;
            break;
        }
    }
    let mut out = Meta { star, tags: Vec::new() };
    let mut rest = text.as_ref();
    while let Some(a) = rest.find("<rdf:li>") {
        let b = match rest[a..].find("</rdf:li>") {
            Some(b) => a + b,
            None => break,
        };
        let t = rest[a + 8..b].trim().to_string();
        if !t.is_empty() {
            out.tags.push(t);
        }
        rest = &rest[b + 9..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rk-xmp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let photo = dir.join("a.jpg");
        std::fs::write(&photo, b"not-a-real-jpeg").unwrap();
        write(
            &photo,
            &Meta { star: 1, tags: vec!["cat".into(), "海边".into()] },
        )
        .unwrap();
        let back = read(&photo);
        assert_eq!(back.star, 1);
        assert_eq!(back.tags, vec!["cat".to_string(), "海边".to_string()]);
        // DB 权威语义:标签清空后 XMP 镜像也为空(不会残留复活)
        write(&photo, &Meta { star: 0, tags: Vec::new() }).unwrap();
        let back2 = read(&photo);
        assert_eq!(back2.star, 0);
        assert!(back2.tags.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

