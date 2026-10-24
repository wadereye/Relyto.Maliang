//! P2-17 视频支持 —— 扩展名识别 + 记录类型语义(参考 Picasa 3)。
//!
//! ## 参考来源：Picasa 3 的视频处理
//!
//! - **扩展名池**(判定时逐一匹配):
//!   `avi divx asf mpg mpeg ty mts m2t m2ts mmv m2v mkv mod tod tp ts mp4 3gp 3g2 mov wmv`
//! - **记录类型** `{8, 9, 10, 0xb, 0xc, 0x17, 0x1d}` = 视频
//!   (占位图、moviestart 触发、导出后缀三处一致):
//!   - `9`  = QuickTime(.mov),仅当 qtsupport.dll 可用才加载
//!   - `8`  = DirectShow/AVI 系(加载时 RIFF/AVI 头解析取宽高,AVI 头解析)
//!   - `0xb`/`0xc` = WMV/ASF 系(ASF 头解析,ASF 头解析)
//!   - `10`/`0x1d` = DirectShow 通用视频
//!   - `0x17` = 视频,但缩略图走疑似网络加载器(视频缩略图加载)——语义未完全确认
//! - **缩略图**:原版走外部进程(DirectShow 渲染 + IPC 回传 + JPEG 编码,
//!   最多等 60 秒)。
//!   **我方等效**:Windows Shell 视频缩略提供程序(`win::shell_thumbnail`,
//!   底层同为系统 DirectShow 系编解码),无需外部进程。
//! - **播放**:原版走库内 DirectShow filter graph 渲染到播放子窗口。
//!   **我方等效**:Shell 关联程序打开(`win::shell_open`)—— 库内视频渲染管线
//!   体量过大,不在本轮实现(诚实声明)。
//! - **moviestart/movieend** = 编辑串里的 hex64 值(按 64 位十六进制解析),
//!   播放时作时间过滤裁剪,**不修改原视频像素**。
//!
//!   ## 单位(已确认)
//!
//!   两个值是 **100 纳秒计数**(10 MHz,即 DirectShow 的 `REFERENCE_TIME`),
//!   **10 000 000 = 1 秒**。两处换算相互印证:
//!   - 原版 trim UI 文本用 `值 / 10 000 000` 得到秒数,拼成 `"起秒/止秒"` 显示;
//!   - 原版把值交给播放器前同样做 `/ 10 000 000.0`。
//!
//!   序列化形态:`moviestart=%I64x` / `movieend=%I64x`(16 进制,读回用 `sscanf("%I64x")`,
//!   读写都不做缩放)。视频导出路径另有一个 `值 / 1000`(100 ns → µs)用于时间码运算。
//!
//!   我方 trim UI 用**秒**为单位(内部仍按 100 ns 计数存取)。

use std::path::Path;

/// 每秒钟对应的 100 纳秒计数。
pub const MOVIE_TICKS_PER_SEC: i64 = 10_000_000;

/// 视频裁剪区间(单位 = 100 纳秒,见 [`MOVIE_TICKS_PER_SEC`])。
///
/// 对应原版编辑串里的 `moviestart` / `movieend` 一对。
/// 原版的语义是**时间过滤**:播放与导出只取 `[start, end]` 这一段,
/// 不改动原文件像素。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MovieTrim {
    /// 裁剪起点(100 ns 计数)。
    pub start: i64,
    /// 裁剪终点(100 ns 计数)。
    pub end: i64,
}

impl MovieTrim {
    /// 用秒构造(内部转成 100 ns 计数)。
    pub fn from_secs(start: f64, end: f64) -> MovieTrim {
        MovieTrim {
            start: (start * MOVIE_TICKS_PER_SEC as f64).round() as i64,
            end: (end * MOVIE_TICKS_PER_SEC as f64).round() as i64,
        }
    }

    /// 起点(秒)。
    pub fn start_secs(&self) -> f64 {
        self.start as f64 / MOVIE_TICKS_PER_SEC as f64
    }

    /// 终点(秒)。
    pub fn end_secs(&self) -> f64 {
        self.end as f64 / MOVIE_TICKS_PER_SEC as f64
    }

    /// 时长(秒)。终点不大于起点时返回 0。
    pub fn duration_secs(&self) -> f64 {
        (self.end - self.start).max(0) as f64 / MOVIE_TICKS_PER_SEC as f64
    }

    /// 是否空区间(未裁剪)。
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// 把起点/终点限制在 `[0, total]` 内,并保证 `start <= end`。
    pub fn clamped_to(&self, total_secs: f64) -> MovieTrim {
        let total = (total_secs * MOVIE_TICKS_PER_SEC as f64).round() as i64;
        let s = self.start.clamp(0, total.max(0));
        let e = self.end.clamp(0, total.max(0));
        MovieTrim {
            start: s.min(e),
            end: e.max(s),
        }
    }

    /// 序列化成原版编辑串片段:`moviestart=%I64x;movieend=%I64x;`(16 进制小写)。
    pub fn serialize(&self) -> String {
        format!(
            "moviestart={:x};movieend={:x};",
            self.start as u64, self.end as u64
        )
    }

    /// 从效果项参数里取区间(参数缺失或非法时返回 `None`)。
    ///
    /// 参数名 `start` / `end`,值是 16 进制 100 ns 计数。
    pub fn from_params(params: &std::collections::BTreeMap<String, String>) -> Option<MovieTrim> {
        let s = u64::from_str_radix(params.get("start")?.trim(), 16).ok()?;
        let e = u64::from_str_radix(params.get("end")?.trim(), 16).ok()?;
        Some(MovieTrim {
            start: s as i64,
            end: e as i64,
        })
    }

    /// 转成效果项参数(与 [`MovieTrim::from_params`] 互逆)。
    pub fn to_params(&self) -> std::collections::BTreeMap<String, String> {
        let mut m = std::collections::BTreeMap::new();
        m.insert("start".into(), format!("{:x}", self.start as u64));
        m.insert("end".into(), format!("{:x}", self.end as u64));
        m
    }

    /// 从原版编辑串里解析(找不到对应键时返回 `None`)。
    ///
    /// 只认 `moviestart=` / `movieend=` 前缀,值是 16 进制 64 位整数。
    pub fn parse(filters: &str) -> Option<MovieTrim> {
        let mut start = None;
        let mut end = None;
        for item in filters.split(';') {
            let item = item.trim();
            if let Some(v) = item.strip_prefix("moviestart=") {
                start = u64::from_str_radix(v.trim(), 16).ok();
            } else if let Some(v) = item.strip_prefix("movieend=") {
                end = u64::from_str_radix(v.trim(), 16).ok();
            }
        }
        match (start, end) {
            (Some(s), Some(e)) => Some(MovieTrim {
                start: s as i64,
                end: e as i64,
            }),
            _ => None,
        }
    }

    /// UI 用的 `"起秒 / 止秒"` 文本(一位小数),与原版 trim 面板同形。
    pub fn label(&self) -> String {
        format!("{:.1} / {:.1}", self.start_secs(), self.end_secs())
    }
}

/// 视频扩展名全集(见模块文档)。
/// 顺序按池内出现顺序;`mov` 在池尾(QuickTime 分组)。
pub const MOVIE_EXTS: [&str; 21] = [
    "avi", "divx", "asf", "mpg", "mpeg", "ty", "mts", "m2t", "m2ts", "mmv", "m2v", "mkv", "mod",
    "tod", "tp", "ts", "mp4", "3gp", "3g2", "mov", "wmv",
];

/// 是否视频文件(大小写不敏感)。
///
/// 扩展名逐一匹配命中即真。
pub fn is_video(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| MOVIE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// 数据库 `photos.kind` 的取值(对齐原版记录类型的"是否视频"二分;
/// 原版 7 种视频子类型对应不同解码路径,我方只有一条 Shell 路径,无须细分)。
pub mod kind {
    /// 图片。
    pub const PHOTO: i32 = 0;
    /// 视频。
    pub const MOVIE: i32 = 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_set_matches_original_pool() {
        // 参考原版:21 个扩展名,顺序如常量。
        assert_eq!(MOVIE_EXTS.len(), 21);
        // 三处独立来源里点名的格式必须在内
        for e in ["avi", "mpg", "wmv", "mov", "mp4", "mts", "mkv", "3gp"] {
            assert!(MOVIE_EXTS.contains(&e), "{e} 必须在原版扩展名池里");
        }
        // 图片扩展名绝不能混进来
        for e in ["jpg", "png", "nef", "cr2"] {
            assert!(!MOVIE_EXTS.contains(&e));
        }
    }

    #[test]
    fn is_video_matches_case_insensitively_and_rejects_images() {
        assert!(is_video(Path::new("clip.AVI")));
        assert!(is_video(Path::new("a.mOv")));
        assert!(is_video(Path::new("x.MP4")));
        assert!(is_video(Path::new("/p/ holidays 2010 .wmv".trim())));
        assert!(!is_video(Path::new("a.jpg")));
        assert!(!is_video(Path::new("a.nef")));
        assert!(!is_video(Path::new("noext")));
        // 原版 `ty`(TiVo)与 `mmv`(MicroMV)这类冷门格式也要认
        assert!(is_video(Path::new("tivo.ty")));
        assert!(is_video(Path::new("sony.mmv")));
    }

    // ===== moviestart/movieend 对应的 trim 数据模型 =====

    #[test]
    fn movie_ticks_per_second_is_ten_million() {
        // 单位:100 纳秒计数,10 000 000 = 1 秒。
        assert_eq!(MOVIE_TICKS_PER_SEC, 10_000_000);
    }

    #[test]
    fn trim_roundtrips_through_serialize() {
        let t = MovieTrim::from_secs(3.5, 12.25);
        let s = t.serialize();
        // 3.5 s = 35 000 000 = 0x2160ec0;12.25 s = 122 500 000 = 0x74d33a0
        assert_eq!(s, "moviestart=2160ec0;movieend=74d33a0;");
        let back = MovieTrim::parse(&s).expect("应能解析");
        assert_eq!(back, t);
    }

    #[test]
    fn trim_parse_tolerates_other_filter_items() {
        // 30 s = 300 000 000 = 0x11e1a300;60 s = 600 000 000 = 0x23c34600
        let filters = "bw;moviestart=11e1a300;movieend=23c34600;sepia;";
        let t = MovieTrim::parse(filters).expect("应能解析");
        assert_eq!(t.start_secs(), 30.0);
        assert_eq!(t.end_secs(), 60.0);
        assert!((t.duration_secs() - 30.0).abs() < 1e-9);
    }

    #[test]
    fn trim_parse_returns_none_without_both_keys() {
        assert!(MovieTrim::parse("moviestart=1e;").is_none());
        assert!(MovieTrim::parse("bw;sepia;").is_none());
    }

    #[test]
    fn trim_clamps_to_total_and_orders_ends() {
        let t = MovieTrim::from_secs(-2.0, 999.0).clamped_to(10.0);
        assert_eq!(t.start, 0);
        assert_eq!(t.end_secs(), 10.0);
        // 反序的区间也应被理顺
        let rev = MovieTrim::from_secs(8.0, 2.0).clamped_to(10.0);
        assert_eq!(rev.start_secs(), 2.0);
        assert_eq!(rev.end_secs(), 8.0);
    }

    #[test]
    fn trim_empty_when_end_not_after_start() {
        assert!(MovieTrim::default().is_empty());
        assert!(MovieTrim::from_secs(5.0, 5.0).is_empty());
        assert!(!MovieTrim::from_secs(5.0, 6.0).is_empty());
    }

    #[test]
    fn trim_label_is_seconds_with_one_decimal() {
        assert_eq!(MovieTrim::from_secs(3.5, 12.25).label(), "3.5 / 12.2");
    }
}
