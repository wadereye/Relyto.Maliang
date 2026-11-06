//! PSD / PSB **只读速看预览**。
//!
//! 只做一件事:把文件里可供显示的像素解出来交给缩略图与预览窗。不写回任何
//! 字节,不参与编辑与导出。数据布局按 Adobe 公开的《Photoshop File Formats
//! Specification》实现。
//!
//! 显示优先级:
//! 1. 合并合成图(merged image data) —— 最贴近文档保存时的画面;
//! 2. 没有合成图时,退而按图层记录做 normal + 不透明度的近似合成;
//! 3. 两者都拿不到(例如只读了截断文件)由调用方决定是否借系统缩略图。
//!
//! 刻意收窄的边界:
//! - 支持 1/8/16/32 位,RGB / 灰度 / 索引色 / 位图 / CMYK / Lab / 双色调 /
//!   多通道;CMYK、Lab、双色调、多通道都是**显示用近似换算**,不做色彩管理。
//! - 图层兜底只做 normal 混合,不解释调整图层、智能对象、文字图层与图层样式。
//! - 全部长度、尺寸、解压输出、图层数都有上限,坏文件返回错误而不是占内存。
use anyhow::{anyhow, Result};
use image::{DynamicImage, RgbaImage};
use std::io::Read;
use std::path::Path;

/// 只读预览专用的扩展名(不进编辑链)。
pub const PSD_EXTS: [&str; 2] = ["psd", "psb"];

const MAX_FILE: u64 = 256 * 1024 * 1024;
const MAX_SIDE: u32 = 16_384;
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_DECODED: u64 = 128 * 1024 * 1024;
const MAX_LAYERS: usize = 256;

struct Header {
    version: u16,
    channels: u16,
    width: u32,
    height: u32,
    depth: u16,
    mode: u16,
}

pub fn is_psd_ext(ext: &str) -> bool {
    PSD_EXTS.contains(&ext)
}

pub fn is_psd_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| is_psd_ext(&e.to_ascii_lowercase()))
        .unwrap_or(false)
}

fn be16(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off.checked_add(2)?)?;
    Some(u16::from_be_bytes([s[0], s[1]]))
}

fn be32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn be64(b: &[u8], off: usize) -> Option<u64> {
    let s = b.get(off..off.checked_add(8)?)?;
    Some(u64::from_be_bytes(s.try_into().ok()?))
}

fn len_at(b: &[u8], off: usize, unit: usize) -> Option<u64> {
    match unit {
        4 => be32(b, off).map(u64::from),
        8 => be64(b, off),
        _ => None,
    }
}

fn parse_header(b: &[u8]) -> Result<Header> {
    if b.get(..4) != Some(b"8BPS".as_slice()) {
        return Err(anyhow!("不是 PSD 签名"));
    }
    let version = be16(b, 4).ok_or_else(|| anyhow!("头被截断"))?;
    let channels = be16(b, 12).ok_or_else(|| anyhow!("头被截断"))?;
    let height = be32(b, 14).ok_or_else(|| anyhow!("头被截断"))?;
    let width = be32(b, 18).ok_or_else(|| anyhow!("头被截断"))?;
    let depth = be16(b, 22).ok_or_else(|| anyhow!("头被截断"))?;
    let mode = be16(b, 24).ok_or_else(|| anyhow!("头被截断"))?;
    let h = Header { version, channels, width, height, depth, mode };
    validate(&h)?;
    Ok(h)
}

/// 每平面每行占多少字节(位图按位打包,16/32 位按大端采样)。
fn plane_row_bytes(width: u32, depth: u16) -> Result<usize> {
    let w = width as usize;
    match depth {
        1 => Ok(w.saturating_add(7) / 8),
        8 => Ok(w),
        16 => w.checked_mul(2).ok_or_else(|| anyhow!("行长度溢出")),
        32 => w.checked_mul(4).ok_or_else(|| anyhow!("行长度溢出")),
        _ => Err(anyhow!("暂不支持 {} 位/通道", depth)),
    }
}

/// 每采样占几个字节;ZIP 预测按这个步长分相位做。
fn sample_step(depth: u16) -> usize {
    match depth {
        16 => 2,
        32 => 4,
        _ => 1,
    }
}

fn validate(h: &Header) -> Result<usize> {
    if ![1u16, 2].contains(&h.version) {
        return Err(anyhow!("版本不是 PSD/PSB"));
    }
    if ![1u16, 8, 16, 32].contains(&h.depth) {
        return Err(anyhow!("暂不支持 {} 位/通道", h.depth));
    }
    if h.channels == 0 || h.channels > 56 {
        return Err(anyhow!("通道数不合理: {}", h.channels));
    }
    if ![0u16, 1, 2, 3, 4, 7, 8, 9].contains(&h.mode) {
        return Err(anyhow!("色彩模式未公开: {}", h.mode));
    }
    if h.width == 0 || h.height == 0 || h.width > MAX_SIDE || h.height > MAX_SIDE {
        return Err(anyhow!("尺寸越界: {}x{}", h.width, h.height));
    }
    let wh = u64::from(h.width) * u64::from(h.height);
    if wh > MAX_PIXELS {
        return Err(anyhow!("像素总量越界"));
    }
    let expected = (plane_row_bytes(h.width, h.depth)? as u64)
        .checked_mul(u64::from(h.height))
        .and_then(|v| v.checked_mul(u64::from(h.channels)))
        .ok_or_else(|| anyhow!("合成图长度溢出"))?;
    if expected > MAX_DECODED {
        return Err(anyhow!("合成图解压上限越界"));
    }
    usize::try_from(expected).map_err(|_| anyhow!("合成图长度溢出"))
}

/// 只读头 26 字节取尺寸:索引阶段不需要碰后面的像素数据。
///
/// 尺寸判定比解码宽松:高位深、CMYK 之类不出图的文档也该有正确的宽高列。
pub fn dimensions(p: &Path) -> Option<(u32, u32)> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut head = [0u8; 26];
    f.read_exact(&mut head).ok()?;
    if head.get(..4) != Some(b"8BPS".as_slice()) {
        return None;
    }
    if !matches!(be16(&head, 4), Some(1 | 2)) {
        return None;
    }
    let w = be32(&head, 18)?;
    let h = be32(&head, 14)?;
    if w == 0 || h == 0 || w > 300_000 || h > 300_000 {
        return None;
    }
    Some((w, h))
}

/// 解出可显示的合成图;任何失败都返回 None,由调用方决定是否走系统缩略图。
pub fn decode(p: &Path) -> Option<DynamicImage> {
    decode_inner(p).ok()
}

fn decode_inner(p: &Path) -> Result<DynamicImage> {
    let size = std::fs::metadata(p).map_err(|e| anyhow!("读文件失败: {e}"))?.len();
    if size > MAX_FILE {
        return Err(anyhow!("文件超过只读预览上限"));
    }
    let bytes = std::fs::read(p).map_err(|e| anyhow!("读文件失败: {e}"))?;
    let h = parse_header(&bytes)?;
    // 长度字段在不同版本/写入器里可能是 4 字节或 8 字节。按三种常见组合试解,
    // 每种都要求压缩标记合法、像素长度精确吻合,避免把乱跳当成成功。
    for (data_unit, layer_unit) in [(4usize, 4usize), (4, 8), (8, 8)] {
        if let Ok(img) = decode_layout(&bytes, &h, data_unit, layer_unit) {
            return Ok(img);
        }
    }
    // 合成图缺失(保存时没有勾选「最大化兼容性」)时,用图层记录做近似合成。
    for (data_unit, layer_unit) in [(4usize, 4usize), (4, 8), (8, 8)] {
        if let Ok(img) = decode_layers_layout(&bytes, &h, data_unit, layer_unit) {
            return Ok(img);
        }
    }
    Err(anyhow!("合成图与图层记录都不可用"))
}

/// 从 off 处取一个「u32/u64 长度 + 数据」段,返回数据并推进游标。
fn take_section<'a>(b: &'a [u8], off: &mut usize, unit: usize) -> Result<&'a [u8]> {
    let len64 = len_at(b, *off, unit).ok_or_else(|| anyhow!("段长度被截断"))?;
    let len = usize::try_from(len64).map_err(|_| anyhow!("段长度溢出"))?;
    let start = off.checked_add(unit).ok_or_else(|| anyhow!("段游标溢出"))?;
    let end = start.checked_add(len).ok_or_else(|| anyhow!("段游标溢出"))?;
    let data = b.get(start..end).ok_or_else(|| anyhow!("段数据被截断"))?;
    *off = end;
    Ok(data)
}

fn decode_layout(b: &[u8], h: &Header, data_unit: usize, layer_unit: usize) -> Result<DynamicImage> {
    let mut off = 26usize;
    let palette = take_section(b, &mut off, data_unit)?;
    let _resources = take_section(b, &mut off, data_unit)?;
    let _layers = take_section(b, &mut off, layer_unit)?;
    let compression = be16(b, off).ok_or_else(|| anyhow!("压缩标记被截断"))?;
    off = off.checked_add(2).ok_or_else(|| anyhow!("压缩标记溢出"))?;
    let payload = b.get(off..).ok_or_else(|| anyhow!("图像数据被截断"))?;

    let expected = validate(h)?;
    let row_bytes = plane_row_bytes(h.width, h.depth)?;
    let rows = (h.height as usize)
        .checked_mul(h.channels as usize)
        .ok_or_else(|| anyhow!("行数溢出"))?;
    let planes = decode_plane(payload, compression, h, expected, row_bytes, rows)?;
    if planes.len() != expected {
        return Err(anyhow!("合成图像素长度不符"));
    }
    let plane_len = row_bytes
        .checked_mul(h.height as usize)
        .ok_or_else(|| anyhow!("平面长度溢出"))?;
    to_rgba(&planes, plane_len, h, palette)
}

/// 解一个平面(或合成图的全部平面):Raw / PackBits / ZIP / ZIP 预测。
fn decode_plane(
    payload: &[u8],
    compression: u16,
    h: &Header,
    expected: usize,
    row_bytes: usize,
    rows: usize,
) -> Result<Vec<u8>> {
    match compression {
        0 => {
            if payload.len() != expected && payload.len() != expected + 1 {
                return Err(anyhow!("未压缩数据长度不符"));
            }
            Ok(payload.get(..expected).ok_or_else(|| anyhow!("未压缩数据不足"))?.to_vec())
        }
        1 => decode_rle(payload, h, expected, row_bytes, rows),
        2 => decode_zip(payload, expected, false, h, row_bytes, rows),
        3 => decode_zip(payload, expected, true, h, row_bytes, rows),
        _ => Err(anyhow!("压缩方式未公开: {compression}")),
    }
}

/// PackBits 单行解到 out,输出严格不超过 row_bytes。
fn packbits_row(src: &[u8], row_bytes: usize, out: &mut Vec<u8>) -> Result<()> {
    let mut i = 0usize;
    let start = out.len();
    while i < src.len() {
        let n = src[i] as i8 as i32;
        i += 1;
        if n >= 0 {
            let cnt = n as usize + 1;
            let seg = src.get(i..i.saturating_add(cnt)).ok_or_else(|| anyhow!("PackBits 段被截断"))?;
            i = i.saturating_add(cnt);
            if out.len().saturating_sub(start).saturating_add(cnt) > row_bytes {
                return Err(anyhow!("PackBits 行超长"));
            }
            out.extend_from_slice(seg);
        } else if n != -128 {
            let cnt = (-n) as usize + 1;
            let v = *src.get(i).ok_or_else(|| anyhow!("PackBits 段被截断"))?;
            i += 1;
            if out.len().saturating_sub(start).saturating_add(cnt) > row_bytes {
                return Err(anyhow!("PackBits 行超长"));
            }
            out.extend(std::iter::repeat(v).take(cnt));
        }
        if out.len() - start == row_bytes {
            return Ok(());
        }
        if out.len() - start > row_bytes {
            return Err(anyhow!("PackBits 行超长"));
        }
    }
    Err(anyhow!("PackBits 行未填满"))
}

fn decode_rle(payload: &[u8], h: &Header, expected: usize, row_bytes: usize, rows: usize) -> Result<Vec<u8>> {
    // 计数表:每行一个,PSD 用 u16、PSB 用 u32。
    let unit = if h.version == 2 { 4usize } else { 2usize };
    let table_bytes = rows.checked_mul(unit).ok_or_else(|| anyhow!("计数表溢出"))?;
    let table = payload.get(..table_bytes).ok_or_else(|| anyhow!("计数表被截断"))?;
    let mut pos = table_bytes;
    let mut out = Vec::with_capacity(expected);
    for i in 0..rows {
        let n = match unit {
            2 => be16(table, i * 2).map(usize::from),
            _ => be32(table, i * 4).map(|v| v as usize),
        }
        .ok_or_else(|| anyhow!("计数项被截断"))?;
        let end = pos.checked_add(n).ok_or_else(|| anyhow!("行游标溢出"))?;
        let seg = payload.get(pos..end).ok_or_else(|| anyhow!("行数据被截断"))?;
        pos = end;
        packbits_row(seg, row_bytes, &mut out)?;
    }
    if payload.len().saturating_sub(pos) > 1 {
        return Err(anyhow!("RLE 尾部还有未消费数据(段长口径不符)"));
    }
    Ok(out)
}

fn decode_zip(
    payload: &[u8],
    expected: usize,
    prediction: bool,
    h: &Header,
    row_bytes: usize,
    rows: usize,
) -> Result<Vec<u8>> {
    // take 到 expected+1,既拿到完整输出,又挡住声称超大膨胀的输入。
    let mut dec = flate2::read::ZlibDecoder::new(payload);
    let mut out = Vec::new();
    (&mut dec)
        .take(expected as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| anyhow!("ZIP 解压失败: {e}"))?;
    if out.len() != expected {
        return Err(anyhow!("ZIP 输出长度不符"));
    }
    if prediction {
        // 预测逐行做;8 位是逐字节差,16/32 位按采样字节分相位做,
        // 即高位字节组与低位字节组各自累加。
        let step = sample_step(h.depth);
        for r in 0..rows {
            let base = r * row_bytes;
            for phase in 0..step {
                let mut x = base + phase + step;
                while x < base + row_bytes {
                    let prev = out[x - step];
                    out[x] = out[x].wrapping_add(prev);
                    x += step;
                }
            }
        }
    }
    Ok(out)
}

/// 各色彩模式的基础通道数(多出的那一个通道按 alpha 处理)。
fn base_channels(mode: u16, channels: u16) -> usize {
    match mode {
        3 | 9 => 3,
        4 => 4,
        7 => (channels as usize).clamp(1, 3),
        _ => 1,
    }
}

/// 只读视图:按平面 + 位深取采样,统一折算到 0.0~1.0。
struct PlaneView<'a> {
    planes: &'a [u8],
    plane_len: usize,
    channels: usize,
    depth: u16,
    /// 每行像素数(位图按行打包,换算位索引用)。
    width: usize,
    row_bytes: usize,
    palette: &'a [u8],
}

impl<'a> PlaneView<'a> {
    fn plane(&self, i: usize) -> Result<&'a [u8]> {
        let a = i.checked_mul(self.plane_len).ok_or_else(|| anyhow!("平面游标溢出"))?;
        let b = a.checked_add(self.plane_len).ok_or_else(|| anyhow!("平面游标溢出"))?;
        self.planes.get(a..b).ok_or_else(|| anyhow!("平面数据不足"))
    }

    /// 第 i 个像素在第 p 个平面上的值(0.0~1.0)。
    fn val(&self, p: usize, i: usize) -> Result<f32> {
        let b = self.plane(p)?;
        Ok(match self.depth {
            1 => {
                // 位图:置位视为落墨(黑),未置位为纸白。每行单独字节对齐。
                let byte = (i / self.width) * self.row_bytes + (i % self.width) / 8;
                if b.get(byte).copied().unwrap_or(0) & (0x80 >> (i % 8)) != 0 {
                    0.0
                } else {
                    1.0
                }
            }
            8 => f32::from(*b.get(i).ok_or_else(|| anyhow!("采样越界"))?) / 255.0,
            16 => {
                let j = i.checked_mul(2).ok_or_else(|| anyhow!("采样游标溢出"))?;
                let s = b.get(j..j.checked_add(2).ok_or_else(|| anyhow!("采样游标溢出"))?)
                    .ok_or_else(|| anyhow!("采样越界"))?;
                f32::from(u16::from_be_bytes([s[0], s[1]])) / 65535.0
            }
            32 => {
                let j = i.checked_mul(4).ok_or_else(|| anyhow!("采样游标溢出"))?;
                let s = b.get(j..j.checked_add(4).ok_or_else(|| anyhow!("采样游标溢出"))?)
                    .ok_or_else(|| anyhow!("采样越界"))?;
                f32::from_be_bytes([s[0], s[1], s[2], s[3]]).clamp(0.0, 1.0)
            }
            _ => return Err(anyhow!("位深未公开")),
        })
    }

    fn alpha(&self, mode: u16, i: usize) -> Result<f32> {
        let base = base_channels(mode, self.channels as u16);
        if self.channels > base {
            self.val(base, i)
        } else {
            Ok(1.0)
        }
    }

    /// 显示用 RGB 换算(0.0~1.0)。CMYK / Lab / 双色调都是近似,不做色彩管理。
    fn color(&self, mode: u16, i: usize) -> Result<[f32; 3]> {
        let c = self.channels;
        let need = base_channels(mode, c as u16);
        if c < need {
            return Err(anyhow!("通道数不足"));
        }
        Ok(match mode {
            0 | 1 | 8 => {
                let v = self.val(0, i)?;
                [v, v, v]
            }
            2 => {
                if self.depth != 8 || self.palette.len() < 768 {
                    return Err(anyhow!("索引色调色板缺失"));
                }
                let k = (self.val(0, i)? * 255.0).round() as usize;
                [
                    f32::from(self.palette[k]) / 255.0,
                    f32::from(self.palette[256 + k]) / 255.0,
                    f32::from(self.palette[512 + k]) / 255.0,
                ]
            }
            3 | 7 => {
                if c < 3 {
                    let v = self.val(0, i)?;
                    [v, v, v]
                } else {
                    [self.val(0, i)?, self.val(1, i)?, self.val(2, i)?]
                }
            }
            4 => {
                if c < 4 {
                    return Err(anyhow!("CMYK 通道不足"));
                }
                let (cc, mm, yy, kk) = (
                    self.val(0, i)?,
                    self.val(1, i)?,
                    self.val(2, i)?,
                    self.val(3, i)?,
                );
                [
                    (1.0 - cc) * (1.0 - kk),
                    (1.0 - mm) * (1.0 - kk),
                    (1.0 - yy) * (1.0 - kk),
                ]
            }
            9 => lab_to_rgb(self.val(0, i)?, self.val(1, i)?, self.val(2, i)?),
            _ => return Err(anyhow!("该色彩模式暂无对应资料: {mode}")),
        })
    }
}

/// Lab(PSD 里 L 0..255、a/b 以 128 为零点)→ sRGB 的显示近似。
fn lab_to_rgb(l: f32, a: f32, b: f32) -> [f32; 3] {
    let l = l * 100.0;
    let a = a * 255.0 - 128.0;
    let b = b * 255.0 - 128.0;
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let inv = |t: f32| -> f32 {
        let t3 = t * t * t;
        if t3 > 0.008856 { t3 } else { (t - 16.0 / 116.0) / 7.787 }
    };
    let x = 0.9505 * inv(fx);
    let y = inv(fy);
    let z = 1.089 * inv(fz);
    let gamma = |v: f32| -> f32 {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.0031308 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
    };
    [
        gamma(3.2406 * x - 1.5372 * y - 0.4986 * z),
        gamma(-0.9689 * x + 1.8758 * y + 0.0415 * z),
        gamma(0.0557 * x - 0.2040 * y + 1.0570 * z),
    ]
}

fn to_rgba(planes: &[u8], plane_len: usize, h: &Header, palette: &[u8]) -> Result<DynamicImage> {
    let row_bytes = plane_row_bytes(h.width, h.depth)?;
    let wh = h.width as usize * h.height as usize;
    let view = PlaneView {
        planes,
        plane_len,
        channels: h.channels as usize,
        depth: h.depth,
        width: h.width as usize,
        row_bytes,
        palette,
    };
    let mut out = vec![0u8; wh * 4];
    for i in 0..wh {
        let rgb = view.color(h.mode, i)?;
        let a = view.alpha(h.mode, i)?;
        for (k, v) in rgb.iter().enumerate() {
            out[4 * i + k] = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        out[4 * i + 3] = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    RgbaImage::from_raw(h.width, h.height, out)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| anyhow!("合成图缓冲区长度不符"))
}

// ===== 没有合并合成图时的图层兜底 =====

struct LayerChan {
    id: i16,
    len: usize,
}

struct LayerRec {
    left: u32,
    top: u32,
    w: u32,
    h: u32,
    chans: Vec<LayerChan>,
    opacity: u8,
    flags: u8,
}

fn decode_layers_layout(b: &[u8], h: &Header, data_unit: usize, layer_unit: usize) -> Result<DynamicImage> {
    let mut off = 26usize;
    let palette = take_section(b, &mut off, data_unit)?;
    let _resources = take_section(b, &mut off, data_unit)?;
    let layers = take_section(b, &mut off, layer_unit)?;
    composite_layers(layers, h, palette)
}

/// 读图层记录表,返回记录与「通道像素数据」的起点。
fn parse_layer_records(sec: &[u8], h: &Header) -> Result<(Vec<LayerRec>, usize)> {
    let unit = if h.version == 2 { 8usize } else { 4usize };
    let info_len = usize::try_from(len_at(sec, 0, unit).ok_or_else(|| anyhow!("图层段被截断"))?)
        .map_err(|_| anyhow!("图层段长度溢出"))?;
    let info_end = unit
        .checked_add(info_len)
        .map(|v| v.min(sec.len()))
        .ok_or_else(|| anyhow!("图层段长度溢出"))?;
    let mut off = unit;
    let cnt = be16(sec, off).ok_or_else(|| anyhow!("图层数被截断"))? as i16;
    off += 2;
    let n = cnt.unsigned_abs() as usize;
    if n == 0 || n > MAX_LAYERS {
        return Err(anyhow!("图层数不合理: {n}"));
    }
    let chan_unit = if h.version == 2 { 8usize } else { 4usize };
    let mut recs = Vec::with_capacity(n);
    for _ in 0..n {
        let top = be32(sec, off).ok_or_else(|| anyhow!("图层矩形被截断"))?;
        let left = be32(sec, off + 4).ok_or_else(|| anyhow!("图层矩形被截断"))?;
        let bottom = be32(sec, off + 8).ok_or_else(|| anyhow!("图层矩形被截断"))?;
        let right = be32(sec, off + 12).ok_or_else(|| anyhow!("图层矩形被截断"))?;
        off += 16;
        let nc = be16(sec, off).ok_or_else(|| anyhow!("通道数被截断"))? as usize;
        off += 2;
        if nc == 0 || nc > 56 {
            return Err(anyhow!("图层通道数不合理: {nc}"));
        }
        let mut chans = Vec::with_capacity(nc);
        for _ in 0..nc {
            let id = sec
                .get(off..off + 2)
                .and_then(|s| <[u8; 2]>::try_from(s).ok())
                .map(i16::from_be_bytes)
                .ok_or_else(|| anyhow!("通道标识被截断"))?;
            off += 2;
            let len = usize::try_from(
                len_at(sec, off, chan_unit).ok_or_else(|| anyhow!("通道长度被截断"))?,
            )
            .map_err(|_| anyhow!("通道长度溢出"))?;
            off += chan_unit;
            chans.push(LayerChan { id, len });
        }
        // 混合模式签名 + 键 + 不透明度 + 裁剪 + 标志 + 填充。
        if sec.len() < off.saturating_add(12) {
            return Err(anyhow!("混合模式字段被截断"));
        }
        let opacity = sec[off + 8];
        let flags = sec[off + 10];
        off += 12;
        let extra = be32(sec, off).ok_or_else(|| anyhow!("附加数据长度被截断"))? as usize;
        off = off
            .checked_add(4)
            .and_then(|v| v.checked_add(extra))
            .ok_or_else(|| anyhow!("附加数据溢出"))?;
        if off > info_end {
            return Err(anyhow!("图层记录越过图层段"));
        }
        recs.push(LayerRec {
            left,
            top,
            w: right.saturating_sub(left),
            h: bottom.saturating_sub(top),
            chans,
            opacity,
            flags,
        });
    }
    Ok((recs, off))
}

/// 按 normal + 不透明度从底到顶合成。非 normal 混合键、蒙版与调整图层
/// 一律按 normal 近似 —— 速看预览不承诺与编辑器的图层效果一致。
fn composite_layers(sec: &[u8], h: &Header, palette: &[u8]) -> Result<DynamicImage> {
    let (recs, mut off) = parse_layer_records(sec, h)?;
    let mut budget: u64 = 0;
    let mut planes: Vec<Vec<(i16, Vec<u8>)>> = Vec::with_capacity(recs.len());
    for rec in &recs {
        if rec.w == 0 || rec.h == 0 || rec.w > MAX_SIDE || rec.h > MAX_SIDE {
            return Err(anyhow!("图层尺寸越界"));
        }
        let row = plane_row_bytes(rec.w, h.depth)?;
        let expected = row
            .checked_mul(rec.h as usize)
            .ok_or_else(|| anyhow!("图层数据长度溢出"))?;
        let mut chans = Vec::with_capacity(rec.chans.len());
        for ch in &rec.chans {
            let comp = be16(sec, off).ok_or_else(|| anyhow!("通道压缩标记被截断"))?;
            off += 2;
            let data_len = ch.len.saturating_sub(2);
            let end = off.checked_add(data_len).ok_or_else(|| anyhow!("通道数据溢出"))?;
            let seg = sec.get(off..end).ok_or_else(|| anyhow!("通道数据被截断"))?;
            off = end;
            let plane = decode_plane(seg, comp, h, expected, row, rec.h as usize)?;
            budget = budget.saturating_add(plane.len() as u64);
            if budget > MAX_DECODED {
                return Err(anyhow!("图层数据总量越界"));
            }
            chans.push((ch.id, plane));
        }
        planes.push(chans);
    }
    let cw = h.width as usize;
    let chh = h.height as usize;
    let canvas_len = cw
        .checked_mul(chh)
        .and_then(|v| v.checked_mul(4))
        .ok_or_else(|| anyhow!("画布长度溢出"))?;
    // u8 直存 RGBA:大文档的 f32 画布会平白多占 4 倍内存。
    let mut canvas = vec![0u8; canvas_len];
    for k in (0..recs.len()).rev() {
        let rec = &recs[k];
        // flags 的第 1 位在公开实现里表示「隐藏」。
        if rec.flags & 0x02 != 0 || rec.w == 0 || rec.h == 0 {
            continue;
        }
        let row = plane_row_bytes(rec.w, h.depth)?;
        let plane_len = row * rec.h as usize;
        // 找颜色通道与透明度通道(-1)。
        let mut color_ids: Vec<(usize, i16)> = Vec::new();
        let mut alpha_idx: Option<usize> = None;
        for (i, (id, _)) in planes[k].iter().enumerate() {
            if *id == -1 {
                alpha_idx = Some(i);
            } else if (0..4).contains(id) {
                color_ids.push((i, *id));
            }
        }
        let need = base_channels(h.mode, h.channels);
        if color_ids.len() < need {
            continue;
        }
        let mut buf = Vec::with_capacity(plane_len * (color_ids.len() + 1));
        for (idx, _) in &color_ids {
            buf.extend_from_slice(&planes[k][*idx].1);
        }
        let has_alpha = alpha_idx.is_some();
        if let Some(ai) = alpha_idx {
            buf.extend_from_slice(&planes[k][ai].1);
        }
        let view = PlaneView {
            planes: &buf,
            plane_len,
            channels: color_ids.len() + usize::from(has_alpha),
            depth: h.depth,
            width: rec.w as usize,
            row_bytes: row,
            palette,
        };
        // 通道 id 未必按 0/1/2 排列,按 id 重排成连续平面。
        let order: Vec<usize> = (0..need)
            .map(|want| {
                color_ids
                    .iter()
                    .position(|(_, id)| *id == want as i16)
                    .unwrap_or(0)
            })
            .collect();
        let opacity = f32::from(rec.opacity) / 255.0;
        for y in 0..rec.h as usize {
            let dy = rec.top as usize + y;
            if dy >= chh {
                break;
            }
            for x in 0..rec.w as usize {
                let dx = rec.left as usize + x;
                if dx >= cw {
                    continue;
                }
                let si = y * rec.w as usize + x;
                let mut rgb = [0f32; 3];
                for (k2, &p) in order.iter().enumerate().take(3) {
                    rgb[k2] = view.val(p, si).unwrap_or(0.0);
                }
                let mut sa = opacity;
                if has_alpha {
                    sa *= view.val(color_ids.len(), si).unwrap_or(1.0);
                }
                if sa <= 0.0 {
                    continue;
                }
                // CMYK / Lab 这类模式按合成图同一套换算走。
                let rgb = match h.mode {
                    4 => {
                        let cc = view.val(order[0], si).unwrap_or(0.0);
                        let mm = view.val(order[1], si).unwrap_or(0.0);
                        let yy = view.val(order[2], si).unwrap_or(0.0);
                        let kk = if order.len() > 3 {
                            view.val(order[3], si).unwrap_or(0.0)
                        } else {
                            0.0
                        };
                        [(1.0 - cc) * (1.0 - kk), (1.0 - mm) * (1.0 - kk), (1.0 - yy) * (1.0 - kk)]
                    }
                    9 => lab_to_rgb(rgb[0], rgb[1], rgb[2]),
                    _ => rgb,
                };
                let di = (dy * cw + dx) * 4;
                let da = f32::from(canvas[di + 3]) / 255.0;
                let out_a = sa + da * (1.0 - sa);
                if out_a <= 0.0 {
                    continue;
                }
                for c in 0..3 {
                    let dst = f32::from(canvas[di + c]) / 255.0;
                    let v = (rgb[c] * sa + dst * da * (1.0 - sa)) / out_a;
                    canvas[di + c] = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                }
                canvas[di + 3] = (out_a.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        }
    }
    // 透明画布合成到浅灰底,与缩略图 JPEG 缓存的口径一致。
    let mut out = vec![0u8; canvas_len];
    for i in 0..cw * chh {
        let a = f32::from(canvas[4 * i + 3]) / 255.0;
        for c in 0..3 {
            let v = f32::from(canvas[4 * i + c]) / 255.0 * a + 0.973 * (1.0 - a);
            out[4 * i + c] = (v * 255.0).round() as u8;
        }
        out[4 * i + 3] = 255;
    }
    RgbaImage::from_raw(h.width, h.height, out)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| anyhow!("图层合成缓冲区长度不符"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(w: u32, hh: u32, ch: u16, mode: u16, version: u16) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"8BPS");
        b.extend_from_slice(&version.to_be_bytes());
        b.extend_from_slice(&[0u8; 6]);
        b.extend_from_slice(&ch.to_be_bytes());
        b.extend_from_slice(&hh.to_be_bytes());
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&8u16.to_be_bytes());
        b.extend_from_slice(&mode.to_be_bytes());
        b
    }

    fn with_depth(mut b: Vec<u8>, depth: u16) -> Vec<u8> {
        b[22..24].copy_from_slice(&depth.to_be_bytes());
        b
    }

    /// 三个空段 + 压缩标记 + 像素数据。
    fn wrap(mut b: Vec<u8>, compression: u16, payload: &[u8], palette: &[u8]) -> Vec<u8> {
        b.extend_from_slice(&(palette.len() as u32).to_be_bytes());
        b.extend_from_slice(palette);
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&compression.to_be_bytes());
        b.extend_from_slice(payload);
        b
    }

    fn write_tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("maliang-psd-{name}-{}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn raw_rgb_uses_planar_order() {
        // 2x2,平面顺序 RRRR GGGG BBBB。
        let planes = [10u8, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
        let bytes = wrap(header(2, 2, 3, 3, 1), 0, &planes, &[]);
        let p = write_tmp("raw", &bytes);
        let img = decode(&p).expect("未压缩合成图应可解").to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [10, 50, 90, 255]);
        assert_eq!(img.get_pixel(1, 1).0, [40, 80, 120, 255]);
        assert_eq!(dimensions(&p), Some((2, 2)));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn rle_rows_use_packbits_counts() {
        let rows = [[1u8, 2], [3, 4], [5, 6], [7, 8], [9, 10], [11, 12]];
        let mut counts = Vec::new();
        let mut data = Vec::new();
        for r in rows {
            counts.extend_from_slice(&3u16.to_be_bytes());
            data.extend_from_slice(&[1, r[0], r[1]]);
        }
        counts.extend_from_slice(&data);
        let bytes = wrap(header(2, 2, 3, 3, 1), 1, &counts, &[]);
        let p = write_tmp("rle", &bytes);
        let img = decode(&p).expect("RLE 合成图应可解").to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [1, 5, 9, 255]);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn zip_with_prediction_roundtrips() {
        let raw = [10u8, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
        let mut predicted = raw.to_vec();
        let plane = 4usize;
        for p in 0..3 {
            for r in 0..2 {
                let base = p * plane + r * 2;
                for x in (1..2).rev() {
                    predicted[base + x] = predicted[base + x].wrapping_sub(predicted[base + x - 1]);
                }
            }
        }
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &predicted).unwrap();
        let zipped = enc.finish().unwrap();
        let bytes = wrap(header(2, 2, 3, 3, 1), 3, &zipped, &[]);
        let p = write_tmp("zip", &bytes);
        let img = decode(&p).expect("ZIP 合成图应可解").to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [10, 50, 90, 255]);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn indexed_palette_maps_through_color_mode_data() {
        let mut palette = vec![0u8; 768];
        palette[7] = 200;
        palette[256 + 7] = 100;
        palette[512 + 7] = 50;
        let bytes = wrap(header(1, 1, 1, 2, 1), 0, &[7], &palette);
        let p = write_tmp("indexed", &bytes);
        let img = decode(&p).expect("索引色合成图应可解").to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [200, 100, 50, 255]);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn truncated_and_unknown_modes_fail_softly() {
        let short = write_tmp("short", b"8BPS");
        assert!(decode(&short).is_none());
        assert!(dimensions(&short).is_none());
        let _ = std::fs::remove_file(short);

        let unknown = wrap(header(1, 1, 1, 99, 1), 0, &[0], &[]);
        let p = write_tmp("unknown", &unknown);
        assert!(decode(&p).is_none());
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn psb_uses_wide_lengths_and_u32_counts() {
        let mut b = header(1, 1, 3, 3, 2);
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(&0u64.to_be_bytes());
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&[1, 2, 3]);
        let p = write_tmp("psb", &b);
        let img = decode(&p).expect("PSB 合成图应可解").to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [1, 2, 3, 255]);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn cmyk_and_high_bit_depth_convert_for_display() {
        // CMYK 全零墨量 = 白;第 5 通道是 alpha。
        let cmyk = wrap(header(1, 1, 5, 4, 1), 0, &[0, 0, 0, 0, 255], &[]);
        let p = write_tmp("cmyk", &cmyk);
        assert_eq!(decode(&p).unwrap().to_rgba8().get_pixel(0, 0).0, [255, 255, 255, 255]);
        let _ = std::fs::remove_file(p);

        // 16 位 RGB:0x0000 / 0x8000 / 0xFFFF → 0 / 128 / 255 附近。
        let px: Vec<u8> = [0u16, 0x8000, 0xFFFF]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        let b = with_depth(header(1, 1, 3, 3, 1), 16);
        let p = write_tmp("rgb16", &wrap(b, 0, &px, &[]));
        let got = decode(&p).unwrap().to_rgba8().get_pixel(0, 0).0;
        assert!(got[0] == 0 && got[1] >= 127 && got[1] <= 129 && got[2] == 255);
        let _ = std::fs::remove_file(p);

        // 32 位浮点灰度 0.5 → 128。
        let b = with_depth(header(1, 1, 1, 1, 1), 32);
        let p = write_tmp("gray32", &wrap(b, 0, &0.5f32.to_be_bytes(), &[]));
        assert_eq!(decode(&p).unwrap().to_rgba8().get_pixel(0, 0).0[0], 128);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn lab_conversion_is_bounded_and_neutral_gray_stays_gray() {
        let [r, g, b] = lab_to_rgb(0.5, 128.0 / 255.0, 128.0 / 255.0);
        assert!((r - g).abs() < 0.02 && (g - b).abs() < 0.02, "中性灰不该偏色");
        for v in [r, g, b] {
            assert!((0.0..=1.0).contains(&v));
        }
    }

    #[test]
    fn missing_composite_falls_back_to_layer_records() {
        // 1x1 RGB 文档:合成图段只有压缩标记、没有像素;图层段里放一个红色不透明层。
        let mut layer = Vec::new();
        // layer info 长度(占位,稍后回填) + 图层数 1
        layer.extend_from_slice(&0u32.to_be_bytes());
        layer.extend_from_slice(&1i16.to_be_bytes());
        for v in [0u32, 0u32, 1u32, 1u32] {
            layer.extend_from_slice(&v.to_be_bytes()); // top,left,bottom,right
        }
        layer.extend_from_slice(&3u16.to_be_bytes()); // 3 通道
        let chan_bytes = 2u32 + 1u32; // 压缩标记 + 1 字节像素
        for id in 0i16..3 {
            layer.extend_from_slice(&id.to_be_bytes());
            layer.extend_from_slice(&chan_bytes.to_be_bytes());
        }
        layer.extend_from_slice(b"8BIMnorm");
        layer.extend_from_slice(&[255u8, 0, 0, 0]); // 不透明度/裁剪/标志/填充
        layer.extend_from_slice(&0u32.to_be_bytes()); // 附加数据
        for (comp, px) in [(0u16, 200u8), (0, 10), (0, 30)] {
            layer.extend_from_slice(&comp.to_be_bytes());
            layer.extend_from_slice(&[px]);
        }
        let info_len = (layer.len() - 4) as u32;
        layer[0..4].copy_from_slice(&info_len.to_be_bytes());

        let mut b = header(1, 1, 3, 3, 1);
        b.extend_from_slice(&0u32.to_be_bytes()); // 色彩模式数据
        b.extend_from_slice(&0u32.to_be_bytes()); // 图像资源
        b.extend_from_slice(&(layer.len() as u32).to_be_bytes());
        b.extend_from_slice(&layer);
        b.extend_from_slice(&0u16.to_be_bytes()); // 合成图:没有像素数据
        let p = write_tmp("layers", &b);
        let img = decode(&p).expect("图层兜底应出图").to_rgba8();
        let got = img.get_pixel(0, 0).0;
        assert_eq!(&got[..3], &[200, 10, 30]);
        assert_eq!(got[3], 255);
        let _ = std::fs::remove_file(p);
    }
}
