//! 复杂查询解析(M5,可选):时间/地点/分类/自由文本;模型不可用时走规则兜底。

/// 解析后的查询结构。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    /// 参与语义与全文检索的自由文本。
    pub text: String,
    /// 分类相簿(顶层节点)。
    pub category: Option<String>,
    /// 相对天数范围(0=今天,-1=昨天,None=不限)。
    pub day_offset: Option<i32>,
    /// 年份与月份(0 表示未指定)。
    pub year: i32,
    pub month: u32,
    /// 是否包含"附近/旁边"这类地点意图。
    pub near: bool,
}

/// 规则解析:识别时间词、分类词与地点意图,剩余文本作为自由查询。
pub fn heuristic(query: &str) -> Parsed {
    let mut p = Parsed::default();
    let mut rest = query.to_string();
    for (word, cat) in [
        ("截图", "文档票据"),
        ("证件", "文档票据"),
        ("发票", "文档票据"),
        ("文档", "文档票据"),
        ("收据", "文档票据"),
        ("猫", "动物"),
        ("狗", "动物"),
        ("动物", "动物"),
        ("人物", "人物"),
        ("自拍", "人物"),
        ("合影", "人物"),
        ("美食", "美食"),
        ("菜", "美食"),
        ("风景", "场景"),
        ("夜景", "场景"),
        ("海", "场景"),
        ("山", "场景"),
        ("车", "物品"),
        ("花", "物品"),
    ] {
        if rest.contains(word) {
            p.category = Some(cat.to_string());
            break;
        }
    }
    for word in ["今天", "今日"] {
        if rest.contains(word) {
            p.day_offset = Some(0);
            rest = rest.replace(word, " ");
        }
    }
    for word in ["昨天", "昨日"] {
        if rest.contains(word) {
            p.day_offset = Some(-1);
            rest = rest.replace(word, " ");
        }
    }
    if let Some((y, m)) = find_year_month(&rest) {
        p.year = y;
        p.month = m;
        let token = format!("{y}年{m}月");
        rest = rest.replace(&token, " ");
    }
    for word in ["附近", "旁边", "周围的"] {
        if rest.contains(word) {
            p.near = true;
            rest = rest.replace(word, " ");
        }
    }
    p.text = rest
        .replace("的", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    p
}

/// 把时间条件换算成日期串区间 [start,end);无时间条件返回 None。
pub fn date_window(p: &Parsed, today: chrono::NaiveDate) -> Option<(String, String)> {
    use chrono::{Duration, NaiveDate};
    let mut win: Option<(NaiveDate, NaiveDate)> = None;
    if p.year > 0 {
        let m = p.month.clamp(1, 12);
        let first = NaiveDate::from_ymd_opt(p.year, m, 1)?;
        let next = if m == 12 {
            NaiveDate::from_ymd_opt(p.year + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(p.year, m + 1, 1)
        }?;
        win = Some((first, next));
    }
    if let Some(off) = p.day_offset {
        let d = today.checked_add_signed(Duration::days(off as i64))?;
        let next = d.checked_add_signed(Duration::days(1))?;
        win = Some(match win {
            Some((a, b)) => (a.max(d), b.min(next)),
            None => (d, next),
        });
    }
    let (a, b) = win?;
    // 交集为空时返回零宽区间,调用方不会命中任何照片。
    let b = b.max(a);
    Some((a.to_string(), b.to_string()))
}

/// 解析查询:优先采用模型输出,缺失或解析失败时走规则兜底。
pub fn parse_query(query: &str, model_output: Option<&str>) -> Parsed {
    model_output
        .and_then(parse_model_output)
        .unwrap_or_else(|| heuristic(query))
}

/// 从文本里抽"YYYY年M月"或"YYYY-MM"。
fn find_year_month(s: &str) -> Option<(i32, u32)> {
    let chars: Vec<char> = s.chars().collect();
    for i in 0..chars.len().saturating_sub(4) {
        if !chars[i..i + 4].iter().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let year: i32 = chars[i..i + 4].iter().collect::<String>().parse().ok()?;
        let mut j = i + 4;
        if j < chars.len() && (chars[j] == '年' || chars[j] == '-' || chars[j] == '/') {
            j += 1;
        }
        let start = j;
        while j < chars.len() && j - start < 2 && chars[j].is_ascii_digit() {
            j += 1;
        }
        if j > start {
            let month: u32 = chars[start..j].iter().collect::<String>().parse().ok()?;
            if (1..=12).contains(&month) {
                return Some((year, month));
            }
        }
    }
    None
}

/// 从模型输出里抠 JSON 字段;失败返回 None。
pub fn parse_model_output(s: &str) -> Option<Parsed> {
    let body = s
        .find('{')
        .and_then(|a| s.rfind('}').map(|b| (a, b)))
        .filter(|(a, b)| *b > *a)
        .map(|(a, b)| &s[a..=b])?;
    let mut p = Parsed::default();
    p.text = json_str(body, "text").unwrap_or_default();
    p.category = json_str(body, "category").filter(|c| !c.is_empty());
    if let Some(n) = json_num(body, "day_offset") {
        p.day_offset = Some(n as i32);
    }
    if let Some(n) = json_num(body, "year") {
        p.year = n as i32;
    }
    if let Some(n) = json_num(body, "month") {
        p.month = n as u32;
    }
    p.near = json_bool(body, "near").unwrap_or(false);
    if p.text.is_empty() && p.category.is_none() && p.day_offset.is_none() {
        None
    } else {
        Some(p)
    }
}

fn json_str(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let i = s.find(&pat)? + pat.len();
    let rest = s[i..].trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn json_num(s: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{key}\"");
    let i = s.find(&pat)? + pat.len();
    let rest = s[i..].trim_start().strip_prefix(':')?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_bool(s: &str, key: &str) -> Option<bool> {
    let pat = format!("\"{key}\"");
    let i = s.find(&pat)? + pat.len();
    let rest = s[i..].trim_start().strip_prefix(':')?.trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_time_category_and_text() {
        let p = heuristic("昨天拍的发票 2024年5月");
        assert_eq!(p.day_offset, Some(-1));
        assert_eq!(p.category.as_deref(), Some("文档票据"));
        assert_eq!((p.year, p.month), (2024, 5));
        assert!(p.text.contains("拍"));
    }

    #[test]
    fn parses_near_and_empty() {
        let p = heuristic("附近的猫");
        assert_eq!(p.category.as_deref(), Some("动物"));
        assert!(p.near);
        assert!(heuristic("").text.is_empty());
    }

    #[test]
    fn date_window_covers_day_month_and_intersection() {
        use chrono::NaiveDate;
        let today = NaiveDate::from_ymd_opt(2026, 11, 9).unwrap();
        let empty = date_window(&Parsed::default(), today);
        assert_eq!(empty, None);
        let t = Parsed { day_offset: Some(0), ..Parsed::default() };
        assert_eq!(date_window(&t, today), Some(("2026-11-09".into(), "2026-11-10".into())));
        let y = Parsed { day_offset: Some(-1), ..Parsed::default() };
        assert_eq!(date_window(&y, today), Some(("2026-11-08".into(), "2026-11-09".into())));
        let m = Parsed { year: 2024, month: 2, ..Parsed::default() };
        assert_eq!(date_window(&m, today), Some(("2024-02-01".into(), "2024-03-01".into())));
        let both = Parsed { year: 2026, month: 11, day_offset: Some(-1), ..Parsed::default() };
        assert_eq!(date_window(&both, today), Some(("2026-11-08".into(), "2026-11-09".into())));
        let disjoint = Parsed { year: 2024, month: 2, day_offset: Some(0), ..Parsed::default() };
        assert_eq!(date_window(&disjoint, today), Some(("2026-11-09".into(), "2026-11-09".into())));
    }

    #[test]
    fn parse_query_prefers_model_output_and_falls_back() {
        let p = parse_query(
            "昨天的猫",
            Some(r#"{"text":"cat","category":"动物","day_offset":-1}"#),
        );
        assert_eq!(p.text, "cat");
        assert_eq!(p.category.as_deref(), Some("动物"));
        assert_eq!(p.day_offset, Some(-1));
        for out in [None, Some("not json")] {
            let f = parse_query("昨天的猫", out);
            assert_eq!(f.day_offset, Some(-1));
            assert_eq!(f.category.as_deref(), Some("动物"));
        }
    }

    #[test]
    fn parses_model_json() {
        let p = parse_model_output(
            r#"{"text":"beach","category":"场景","year":2023,"month":7,"near":true}"#,
        )
        .unwrap();
        assert_eq!(p.text, "beach");
        assert_eq!(p.category.as_deref(), Some("场景"));
        assert_eq!((p.year, p.month), (2023, 7));
        assert!(p.near);
        assert!(parse_model_output("no json here").is_none());
    }
}
