//! 动图**只读速看播放**:GIF 与动图 WebP。
//!
//! 缩略图与索引仍只取首帧;只有单图预览会把帧序列解出来按延迟轮播。
//! 解码器返回的每一帧已经是完整画布上的合成结果(含 dispose 处理),
//! 这里不再自行拼帧,只负责内存/帧数上限、帧延迟钳制与播放时间轴。
//!
//! 只读:不写原文件、不参与烘培、不套效果栈,也不进导出链。
use image::{AnimationDecoder, RgbaImage};
use std::io::Cursor;
use std::path::Path;

/// 单个动图文件最多读这么大;再大就退回静态预览。
const MAX_FILE: u64 = 64 * 1024 * 1024;
/// 帧数上限:坏了的文件不能一直吐帧把内存吃光。
const MAX_FRAMES: usize = 240;
/// 所有帧的 RGBA 总量上限。
const MAX_FRAMES_BYTES: u64 = 96 * 1024 * 1024;
/// 单帧边长上限。
const MAX_SIDE: u32 = 4096;
/// 浏览器/图库通行的延迟钳制:0 或极短延迟按 100ms 兜,超长按 2s 截断。
const MIN_DELAY_MS: u32 = 20;
const MAX_DELAY_MS: u32 = 2000;
const DEFAULT_DELAY_MS: u32 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopMode {
    Forever,
    /// 循环回放的次数(0 = 只放一遍)。
    Times(u32),
}

/// 一帧完整画布 + 显示时长。
pub struct Frame {
    pub rgba: RgbaImage,
    pub delay_ms: u32,
}

/// 解好的帧序列;帧数恒 ≥ 2。
pub struct Clip {
    pub frames: Vec<Frame>,
    pub loops: LoopMode,
    /// 一轮的总时长,时间轴取模用。
    total_ms: u64,
}

impl Clip {
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// 已播放 elapsed 毫秒时该显示哪一帧;None 表示有限循环已放完。
    pub fn frame_index_at(&self, elapsed_ms: u64) -> Option<(usize, u64)> {
        if self.frames.is_empty() || self.total_ms == 0 {
            return None;
        }
        match self.loops {
            LoopMode::Times(0) => {
                if elapsed_ms >= self.total_ms {
                    return None;
                }
            }
            LoopMode::Times(n) => {
                if elapsed_ms >= self.total_ms.saturating_mul(u64::from(n) + 1) {
                    return None;
                }
            }
            LoopMode::Forever => {}
        }
        let mut t = elapsed_ms % self.total_ms;
        for (i, f) in self.frames.iter().enumerate() {
            let d = u64::from(f.delay_ms);
            if t < d {
                // 返回本帧还剩多少毫秒,UI 据此定时重绘。
                return Some((i, d - t));
            }
            t -= d;
        }
        Some((self.frames.len() - 1, 1))
    }
}

/// 可能是动图的扩展名(WebP 需再看文件头;静态 WebP 会自然退回)。
pub fn maybe_animated(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "gif" | "webp"))
        .unwrap_or(false)
}

/// 解帧序列;不是动图、超上限或坏文件都返回 None,调用方继续走静态预览。
pub fn decode_clip(p: &Path) -> Option<Clip> {
    if !maybe_animated(p) {
        return None;
    }
    if std::fs::metadata(p).ok()?.len() > MAX_FILE {
        return None;
    }
    let bytes = std::fs::read(p).ok()?;
    let buf = Cursor::new(bytes);
    let clip = if buf.get_ref().starts_with(b"GIF87a") || buf.get_ref().starts_with(b"GIF89a") {
        let dec = image::codecs::gif::GifDecoder::new(buf).ok()?;
        collect(dec)?
    } else if buf.get_ref().starts_with(b"RIFF") && buf.get_ref().get(8..12) == Some(b"WEBP") {
        let dec = image::codecs::webp::WebPDecoder::new(buf).ok()?;
        collect(dec)?
    } else {
        return None;
    };
    // 单帧图片不是动图,没必要占用逐帧播放的状态与内存。
    (clip.len() >= 2).then_some(clip)
}

fn collect<D>(dec: D) -> Option<Clip>
where
    D: for<'a> AnimationDecoder<'a>,
{
    let loops = match dec.loop_count() {
        image::metadata::LoopCount::Infinite => LoopMode::Forever,
        image::metadata::LoopCount::Finite(n) => LoopMode::Times(n.get()),
    };
    let mut frames = Vec::new();
    let mut used: u64 = 0;
    for f in dec.into_frames() {
        if frames.len() >= MAX_FRAMES {
            break;
        }
        let f = f.ok()?;
        let (n, d) = f.delay().numer_denom_ms();
        let rgba = f.into_buffer();
        let (w, h) = rgba.dimensions();
        if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
            return None;
        }
        let bytes = u64::from(w) * u64::from(h) * 4;
        if used.saturating_add(bytes) > MAX_FRAMES_BYTES {
            break;
        }
        used += bytes;
        let ms = if d == 0 || n == 0 {
            DEFAULT_DELAY_MS
        } else {
            (f64::from(n) / f64::from(d)).round() as u32
        };
        frames.push(Frame {
            rgba,
            delay_ms: ms.clamp(MIN_DELAY_MS, MAX_DELAY_MS),
        });
    }
    if frames.len() < 2 {
        return None;
    }
    let total_ms = frames.iter().map(|f| u64::from(f.delay_ms)).sum();
    Some(Clip {
        frames,
        loops,
        total_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(delays: &[u32], loops: LoopMode) -> Clip {
        let frames = delays
            .iter()
            .map(|&d| Frame {
                rgba: RgbaImage::new(1, 1),
                delay_ms: d,
            })
            .collect();
        let total_ms = delays.iter().map(|&d| u64::from(d)).sum();
        Clip {
            frames,
            loops,
            total_ms,
        }
    }

    #[test]
    fn timeline_walks_frames_and_reports_remaining_time() {
        let c = clip(&[100, 200, 50], LoopMode::Forever);
        assert_eq!(c.frame_index_at(0), Some((0, 100)));
        assert_eq!(c.frame_index_at(99), Some((0, 1)));
        assert_eq!(c.frame_index_at(100), Some((1, 200)));
        assert_eq!(c.frame_index_at(300), Some((2, 50)));
        assert_eq!(c.frame_index_at(350), Some((0, 100)));
        assert_eq!(c.frame_index_at(700), Some((0, 100)));
    }

    #[test]
    fn finite_loops_stop_on_last_frame() {
        let c = clip(&[100, 100], LoopMode::Times(0));
        assert_eq!(c.frame_index_at(199), Some((1, 1)));
        assert_eq!(c.frame_index_at(200), None);
        let twice = clip(&[100, 100], LoopMode::Times(1));
        assert_eq!(twice.frame_index_at(399), Some((1, 1)));
        assert_eq!(twice.frame_index_at(400), None);
    }

    #[test]
    fn non_animated_paths_are_skipped() {
        assert!(maybe_animated(Path::new("a.GIF")));
        assert!(maybe_animated(Path::new("a.webp")));
        assert!(!maybe_animated(Path::new("a.jpg")));
        assert!(decode_clip(Path::new("no-such-file.gif")).is_none());
    }

    #[test]
    fn a_static_gif_does_not_become_a_clip() {
        // 1x1 的单帧 GIF:能解出来,但只有一帧 → 不该进播放器。
        let p = std::env::temp_dir().join(format!("maliang-anim-static-{}", std::process::id()));
        let one = RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]));
        let mut out = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut out)
            .encode(one.as_raw(), 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        std::fs::write(&p, &out).unwrap();
        assert!(decode_clip(&p).is_none());
        let _ = std::fs::remove_file(p);
    }
}
