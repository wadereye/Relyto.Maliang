//! 界面多语言:简体中文(默认)与英文。
//!
//! - 中文原文即 key:`t("原文")` 在中文下原样返回,英文查表,缺译文时回落原文。
//! - 语言持久化在 settings 表 `ui_lang`(zh/en),启动时读取,切换入口在 选项 > 常规。
//! - 带参数文案用 `t_with`,占位符 `{名字}` 在译文里按语言语序自由摆放。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

/// 界面语言。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    /// 简体中文,默认。
    Zh,
    /// English。
    En,
}

impl Lang {
    /// 设置项 `ui_lang` 的取值。
    pub fn key(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    pub fn from_key(s: &str) -> Option<Lang> {
        match s {
            "zh" => Some(Lang::Zh),
            "en" => Some(Lang::En),
            _ => None,
        }
    }

    /// 语言名,始终以该语言自身书写显示。
    pub fn label(self) -> &'static str {
        match self {
            Lang::Zh => "中文(默认)",
            Lang::En => "English",
        }
    }

    /// 全部可选语言(下拉框顺序)。
    pub const ALL: [Lang; 2] = [Lang::Zh, Lang::En];
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// 当前界面语言。渲染期随处可取。
pub fn lang() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::En,
        _ => Lang::Zh,
    }
}

/// 切换界面语言(只改状态;持久化由调用方写 settings)。
pub fn set_lang(l: Lang) {
    CURRENT.store(
        match l {
            Lang::Zh => 0,
            Lang::En => 1,
        },
        Ordering::Relaxed,
    );
}

/// 只读查设置表里的语言;库、表或值缺失时返回 None。
fn lang_from_db(path: &std::path::Path) -> Option<Lang> {
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = rusqlite::Connection::open_with_flags(path, flags).ok()?;
    let v = conn
        .query_row("SELECT v FROM settings WHERE k='ui_lang'", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()?;
    Lang::from_key(&v)
}

/// 启动期读语言设置(只读,失败保持中文);供不走 App::new 的命令行分支复用。
pub fn init_from_settings() {
    if let Some(l) = lang_from_db(&crate::db::data_dir_loc().join("index.db")) {
        set_lang(l);
    }
}

/// 英文词表:中文原文 -> 英文。本体在 [`crate::i18n_en`],缺译文自动回落中文。
use crate::i18n_en::EN_TABLE;

fn en_map() -> &'static HashMap<&'static str, &'static str> {
    static MAP: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| EN_TABLE.iter().copied().collect())
}

/// 翻译:中文原文即 key。英文下查词表,未收录时回落原文。
pub fn t(zh: &'static str) -> &'static str {
    if lang() == Lang::Zh {
        return zh;
    }
    en_map().get(zh).copied().unwrap_or(zh)
}

/// 测试辅助:带参替换(&str 版)。
#[cfg(test)]
pub fn tf(zh: &'static str, args: &[(&str, &str)]) -> String {
    let mut s = t(zh).to_string();
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

/// 带参翻译(值为 String 版):供 `format!("...{n}...")` 隐式捕获文案机械迁移用。
pub fn t_with(zh: &'static str, args: &[(&str, String)]) -> String {
    let mut s = t(zh).to_string();
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

/// 测试期语言守卫:构造时切到目标语言,drop 时恢复;持锁避免并行测试互相干扰。
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(test)]
#[allow(dead_code)]
pub(crate) struct LangGuard(Lang, std::sync::MutexGuard<'static, ()>);
#[cfg(test)]
impl LangGuard {
    pub(crate) fn to(l: Lang) -> Self {
        let lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = lang();
        set_lang(l);
        LangGuard(prev, lock)
    }
}
#[cfg(test)]
impl Drop for LangGuard {
    fn drop(&mut self) {
        set_lang(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 中文回落原文() {
        let _g = LangGuard::to(Lang::Zh);
        assert_eq!(t("语言"), "语言");
    }

    #[test]
    fn 英文命中与回落() {
        let _g = LangGuard::to(Lang::En);
        assert_eq!(t("语言"), "Language");
        assert_eq!(t("词表未收录的句子"), "词表未收录的句子");
    }

    #[test]
    fn 带参替换() {
        let _g = LangGuard::to(Lang::En);
        assert_eq!(tf("共 {n} 项", &[("n", "3")]), "共 3 项");
    }

    #[test]
    fn 词表键不重复() {
        let mut keys: Vec<&str> = EN_TABLE.iter().map(|(k, _)| *k).collect();
        keys.sort_unstable();
        let n = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), n, "EN_TABLE 存在重复 key");
    }

    /// 占位符名(忽略 `{p1:.2}` 里的格式说明)。
    fn ph(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'{' {
                if let Some(j) = s[i + 1..].find('}') {
                    let inner = &s[i + 1..i + 1 + j];
                    let name = inner.split(':').next().unwrap_or(inner);
                    if !name.is_empty()
                        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    {
                        out.push(name.to_string());
                    }
                    i += j + 2;
                    continue;
                }
            }
            i += 1;
        }
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn 只读读取语言设置() {
        let p = std::env::temp_dir().join(format!("maliang-lang-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        assert_eq!(lang_from_db(&p), None);
        {
            let c = rusqlite::Connection::open(&p).unwrap();
            c.execute("CREATE TABLE settings (k TEXT PRIMARY KEY, v TEXT)", [])
                .unwrap();
            c.execute("INSERT INTO settings VALUES ('ui_lang','en')", [])
                .unwrap();
        }
        assert_eq!(lang_from_db(&p), Some(Lang::En));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 词表译文结构合法() {
        for (k, v) in EN_TABLE {
            assert!(!v.is_empty(), "空译文: {k}");
            assert!(
                !v.chars().any(|c| c >= '\u{4e00}' && c <= '\u{9fff}'),
                "译文残留中文: {k}"
            );
            assert_eq!(ph(k), ph(v), "占位符不一致: {k}");
        }
    }

    #[test]
    fn 关键界面文案均有英文译文() {
        let _g = LangGuard::to(Lang::En);
        for k in [
            "AI 分类",
            "库内检索:标签、图中文字或描述",
            "输入标签名 / 图中文字 / 描述,即输即搜…",
            "没有命中的照片;先启用 AI 索引可生成标签、文字与描述索引。",
            "启用后台 AI 索引",
            "增强",
            "红色",
            "灰色",
            "分类 «{p1}» · {p2} 张",
            "库内检索 «{p1}» · {p2} 张",
            "人脸模型未就位:可在 选项 > AI 模型 或人脸视图里下载",
            "更多选项(复选文件、选源、测速)在 选项 > AI 模型。",
            "索引服务不可用时,全盘文件与文件夹的查找、以及未导入目录的实时发现都不可用;标签和图中文字请到「标签」页检索。",
            "共 {p1} 个(右键设颜色/重命名/删除)",
            "重命名 «{p1}»",
            "新标签名",
            "标签已重命名",
            "重命名失败:名称为空或已存在",
            "确定删除标签 «{p1}»(含子标签)? 将同时从 {p2} 张照片上移除",
            "标签已删除",
            "删除标签失败",
            "重命名标签",
            "删除标签",
            "已按基准照片周边 {p1} 公里筛选(仅含带定位的照片)",
            "未指定基准照片,地点条件暂不参与筛选",
            "可用子命令:",
            "未归簇 {p1} 脸 · 点「重新聚类」收齐",
            "「{p1}」不可撤销:人物命名与合并/拆分判定会一并删除,需重新人工整理。",
            "清除并重新扫描",
            "切换并重建",
            "文本命中",
            "忽略此组",
            "仅显示前 {p1} 张,用库内检索可缩小范围。",
        ] {
            assert_ne!(t(k), k, "缺英文译文: {k}");
        }
    }

    #[test]
    fn 语言键值往返() {
        let _g = LangGuard::to(Lang::Zh);
        assert!(matches!(Lang::from_key("en"), Some(Lang::En)));
        assert!(matches!(Lang::from_key("zh"), Some(Lang::Zh)));
        assert!(Lang::from_key("xx").is_none());
        assert_eq!(Lang::En.key(), "en");
    }
}
