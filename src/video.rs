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
//!   播放时作时间过滤裁剪,**不修改原视频像素**。单位按微秒处理
//!   (导出时除以 1000 归一)。我方未实现 trim UI,暂不落库。

use std::path::Path;

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
}
