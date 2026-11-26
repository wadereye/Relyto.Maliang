//! 系统通讯簿(Windows Contacts)只读读取。
//!
//! Windows 的「联系人」文件夹(`%USERPROFILE%\Contacts`)下是 `.contact`
//! XML 文件,常见结构:
//!
//! ```xml
//! <c:contact xmlns:c="http://schemas.microsoft.com/Contact" ...>
//!   <c:FormattedName>张三</c:FormattedName>
//!   <c:EmailAddressList>
//!     <c:EmailAddress><c:Address>a@b.com</c:Address></c:EmailAddress>
//!   </c:EmailAddressList>
//! </c:contact>
//! ```
//!
//! 这里不引入 XML 解析依赖:按标签做最小提取(够读通讯簿),遇到非法
//! 文件直接跳过。读取失败/目录不存在时返回空清单,由 UI 显示提示。

use std::path::{Path, PathBuf};

/// 一条通讯簿条目(一个地址一行)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    /// 展示名(取不到时退回文件名/地址)。
    pub display: String,
    /// 邮箱地址(小写归一)。
    pub email: String,
}

/// Windows 联系人目录;不存在时返回 `None`。
pub fn contacts_dir() -> Option<PathBuf> {
    let profile = std::env::var_os("USERPROFILE")?;
    let dir = PathBuf::from(profile).join("Contacts");
    dir.is_dir().then_some(dir)
}

/// 枚举系统通讯簿里的全部邮箱地址(按展示名排序,地址去重)。
pub fn load_system_contacts() -> Vec<Contact> {
    let Some(dir) = contacts_dir() else {
        return Vec::new();
    };
    load_from_dir(&dir)
}

fn load_from_dir(dir: &Path) -> Vec<Contact> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Contact> = Vec::new();
    for ent in entries.flatten() {
        let path = ent.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if !ext.eq_ignore_ascii_case("contact") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        out.extend(parse_contact_xml(&text, &stem));
    }
    out.sort_by(|a, b| {
        a.display
            .to_lowercase()
            .cmp(&b.display.to_lowercase())
            .then_with(|| a.email.cmp(&b.email))
    });
    out.dedup_by(|a, b| a.email == b.email);
    out
}

/// 从一个 `.contact` XML 文本里提取 (展示名, 地址) 列表。
fn parse_contact_xml(xml: &str, fallback_name: &str) -> Vec<Contact> {
    let display = first_tag_text(xml, "FormattedName")
        .or_else(|| first_tag_text(xml, "DisplayName"))
        .map(|s| unescape_xml(&s))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| fallback_name.to_string());

    let mut emails = Vec::new();
    let mut rest = xml;
    // 每个 <c:EmailAddress> 块里最多取一个 <c:Address>。
    while let Some(start) = find_tag(rest, "EmailAddress") {
        let after = &rest[start.1..];
        let block_end = after
            .find("</c:EmailAddress>")
            .or_else(|| after.find("</EmailAddress>"))
            .unwrap_or(after.len());
        let block = &after[..block_end];
        if let Some(addr) = first_tag_text(block, "Address") {
            let addr = unescape_xml(&addr).trim().to_ascii_lowercase();
            if is_email_like(&addr) {
                emails.push(Contact {
                    display: display.clone(),
                    email: addr,
                });
            }
        }
        rest = &after[block_end.min(after.len())..];
    }
    // 少数文件直接把地址写在 SMTP 属性里;再兜底扫一遍裸地址。
    if emails.is_empty() {
        for token in xml.split(|c: char| {
            c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | ',' | ';')
        }) {
            let t = token.trim().to_ascii_lowercase();
            if is_email_like(&t) && !t.contains("schemas.microsoft.com") {
                emails.push(Contact {
                    display: display.clone(),
                    email: t,
                });
                break;
            }
        }
    }
    emails
}

fn is_email_like(s: &str) -> bool {
    let Some(at) = s.find('@') else {
        return false;
    };
    at > 0
        && at + 1 < s.len()
        && s[at + 1..].contains('.')
        && !s.contains(char::is_whitespace)
}

/// 找到 `<tag ...>` 或 `<c:tag ...>`,返回 (开始, 内容起点)。
fn find_tag<'a>(s: &'a str, tag: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(i) = s[from..].find('<') {
        let start = from + i;
        let rest = &s[start..];
        let body = rest.strip_prefix('<')?;
        // 跳过闭合标签与声明。
        if body.starts_with('/') || body.starts_with('?') || body.starts_with('!') {
            from = start + 1;
            continue;
        }
        let name_end = body
            .find(|c: char| c == '>' || c == ' ' || c == '/' || c == '\t' || c == '\r' || c == '\n')
            .unwrap_or(body.len());
        let name = &body[..name_end];
        let local = name.rsplit(':').next().unwrap_or(name);
        if local.eq_ignore_ascii_case(tag) {
            let close = body.find('>')?;
            let self_closing = body[..close].ends_with('/');
            let content_start = start + 1 + close + 1;
            if self_closing {
                from = content_start;
                continue;
            }
            return Some((start, content_start));
        }
        from = start + 1;
    }
    None
}

/// 取第一个标签的文本内容(不递归,直接取到对应闭合标签)。
fn first_tag_text(xml: &str, tag: &str) -> Option<String> {
    let (_, content_start) = find_tag(xml, tag)?;
    let rest = &xml[content_start..];
    let end = rest.find("</")?;
    let text = &rest[..end];
    // 只接受直接文本;若内部还有子标签则跳过。
    if text.contains('<') {
        return None;
    }
    Some(text.trim().to_string())
}

fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_windows_contact() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<c:contact xmlns:c="http://schemas.microsoft.com/Contact" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <c:FormattedName>张三</c:FormattedName>
  <c:EmailAddressList>
    <c:EmailAddress>
      <c:Address>Zhang.San@Example.com</c:Address>
      <c:Type>SMTP</c:Type>
    </c:EmailAddress>
  </c:EmailAddressList>
</c:contact>"#;
        let got = parse_contact_xml(xml, "fallback");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].display, "张三");
        assert_eq!(got[0].email, "zhang.san@example.com");
    }

    #[test]
    fn parses_multiple_addresses_and_entities() {
        let xml = r#"<c:contact>
  <c:FormattedName>Tom &amp; Jerry</c:FormattedName>
  <c:EmailAddress><c:Address>a@b.com</c:Address></c:EmailAddress>
  <c:EmailAddress><c:Address>c@d.org</c:Address></c:EmailAddress>
</c:contact>"#;
        let got = parse_contact_xml(xml, "x");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].display, "Tom & Jerry");
        assert_eq!(got[0].email, "a@b.com");
        assert_eq!(got[1].email, "c@d.org");
    }

    #[test]
    fn falls_back_to_stem_when_no_display_name() {
        let xml = r#"<c:contact><c:EmailAddress><c:Address>only@mail.com</c:Address></c:EmailAddress></c:contact>"#;
        let got = parse_contact_xml(xml, "contact-file");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].display, "contact-file");
    }

    #[test]
    fn ignores_files_without_email() {
        let xml = r#"<c:contact><c:FormattedName>No Mail</c:FormattedName></c:contact>"#;
        assert!(parse_contact_xml(xml, "x").is_empty());
    }
}
