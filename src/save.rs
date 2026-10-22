//! 双状态保存烘培:重写原图(旋转打包、q90 重编码),先备份原版。
//! 备份目录:`<同级>/photooriginals/<mtime 时间戳>.jpg`,绝不覆盖已有备份。
use crate::db::Store;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

pub struct Baked {
    /// 被重写的原图路径(预留:GUI 完成提示)。
    #[allow(dead_code)]
    pub path: String,
    /// 原版备份位置(预留:GUI 完成提示)。
    #[allow(dead_code)]
    pub backup: String,
}

/// 从 photos 行取 (path, rot)。
fn photo(store: &Store, id: i64) -> Result<(String, i32)> {
    let conn = store.0.lock().unwrap();
    let (p, rot): (String, i32) = conn
        .query_row(
            "SELECT path, rot FROM photos WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .context("照片不在索引中")?;
    Ok((p, rot))
}

/// 提取 JPEG 全部 APP1(Exif)段负载(不含 FF E1 与长度)。
fn jpeg_app1_payloads(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if bytes.get(0..2) != Some(&[0xFF, 0xD8]) {
        return out;
    }
    let mut i = 2usize;
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            break;
        }
        let m = bytes[i + 1];
        if m == 0x00 || m == 0xFF || m == 0xD8 || m == 0xD9 {
            break;
        }
        if m == 0xDA {
            break; // 扫描数据开始,之后再无 APP1 信息段
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if len < 2 {
            break;
        }
        let seg_len = len + 2;
        if i + seg_len > bytes.len() {
            break;
        }
        if m == 0xE1 {
            out.push(bytes[i + 4..i + seg_len].to_vec());
        }
        i += seg_len;
    }
    out
}

/// 在 JPEG 的 SOI 之后插入 APP1 段。
fn insert_app1(dst: &mut Vec<u8>, payloads: &[Vec<u8>]) {
    if payloads.is_empty() {
        return;
    }
    dst.insert(2, 0xD9);
    dst.insert(2, 0xFF);
    for payload in payloads {
        let len = (payload.len() + 2) as u16;
        dst.splice(2..2, payload.iter().copied());
        dst.splice(2..2, [0xFF, 0xE1, (len >> 8) as u8, (len & 0xFF) as u8]);
    }
}

fn unique_backup(dir: &Path, stem: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut cand = dir.join(format!("{stem}.jpg"));
    let mut n = 1;
    while cand.exists() {
        cand = dir.join(format!("{stem} ({n}).jpg"));
        n += 1;
    }
    Ok(cand)
}

#[allow(clippy::too_many_lines)]
pub fn bake(store: &Store, id: i64) -> Result<Baked> {
    let (path_s, rot) = photo(store, id)?;
    if rot == 1 {
        return Err(anyhow!("图片已按 EXIF 方向摆正,无需烘培"));
    }
    let src = Path::new(&path_s);
    let src_bytes = std::fs::read(src).with_context(|| format!("读取原图失败: {path_s}"))?;
    let mtime = src
        .metadata()?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let stem = chrono_fmt(mtime as i64);
    let parent = src.parent().context("无父目录")?;
    let bak_dir = parent.join("photooriginals");
    let bak = unique_backup(&bak_dir, &stem)?;
    std::fs::write(&bak, &src_bytes)?;
    let backup_s = bak.to_string_lossy().into_owned();

    let img = image::load_from_memory(&src_bytes)?;
    let flat = crate::thumbs::apply_exif_rotation(img, rot as u32);
    let mut jpg: Vec<u8> = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 90);
    enc.encode(flat.to_rgb8().as_raw(), flat.width(), flat.height(), image::ExtendedColorType::Rgb8)?;
    if src_bytes.get(0..2) == Some(&[0xFF, 0xD8]) {
        let payloads = jpeg_app1_payloads(&src_bytes);
        insert_app1(&mut jpg, &payloads); // 保留拍摄时间/相机等 EXIF
    }
    let tmp = src.with_extension("rk-tmp");
    std::fs::write(&tmp, &jpg)?;
    std::fs::rename(&tmp, src)?;
    let md = std::fs::metadata(src)?;
    let mtime_ms = md
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    let conn = store.0.lock().unwrap();
    conn.execute(
        "UPDATE photos SET rot=1, mtime=?2, size=?3, baked=1, dhash=NULL, thumb_ok=0 WHERE id=?1",
        rusqlite::params![id, mtime_ms, jpg.len() as i64],
    )?;
    Ok(Baked { path: path_s, backup: backup_s })
}

fn chrono_fmt(epoch_secs: i64) -> String {
    // 无 chrono 依赖,手写 UTC→本地近似(直接用 UTC 时间戳格式化)
    let days = epoch_secs / 86400;
    let secs = epoch_secs % 86400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{ks:02}{mm:02}{ss:02}",
        ks = secs / 3600,
        mm = (secs % 3600) / 60,
        ss = secs % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app1_roundtrip() {
        let mut j = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0xAA, 0xBB];
        let payload = vec![b'E'; 10];
        insert_app1(&mut j, &[payload]);
        assert_eq!(j[2..6], [0xFF, 0xE1, 0x00, 0x0C]);
        let got = jpeg_app1_payloads(&j);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], vec![b'E'; 10]);
    }

    #[test]
    fn civil_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }
}
