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

/// 把 epoch 秒格式化成**本地时间**的 `YYYYMMDD-HHMMSS`(备份文件名)。
///
/// 备份文件放在 `<原图同级>/photooriginals/`,名字取自原图的 mtime ——
/// 用本地时间才和资源管理器里看到的"修改日期"对得上。
fn chrono_fmt(epoch_secs: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(epoch_secs, 0)
        .single()
        .map(|dt| dt.format("%Y%m%d-%H%M%S").to_string())
        .unwrap_or_else(|| "19700101-000000".into())
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

    /// 回归:备份文件名里的日期必须是**本地时间**,不是 UTC。
    ///
    /// 之前手写日期逻辑时直接按 UTC 格式化,导致备份文件名比资源管理器里
    /// 显示的修改时间差一整个时区(中国 +8h),用户无法按日期定位备份。
    #[test]
    fn chrono_fmt_uses_local_time() {
        use chrono::TimeZone;
        // 2026-10-22 16:00:00 UTC
        let epoch: i64 = 1_769_414_400;
        let s = chrono_fmt(epoch);
        // 把「本地时间字符串」解析回时间戳,应无损还原成原 epoch ——
        // 若还在按 UTC 输出,这个往返会正好差一个时区偏移。
        let naive = chrono::NaiveDateTime::parse_from_str(&s, "%Y%m%d-%H%M%S")
            .expect("格式应为 YYYYMMDD-HHMMSS");
        let back = chrono::Local
            .from_local_datetime(&naive)
            .single()
            .expect("本地时间无歧义")
            .timestamp();
        assert_eq!(back, epoch, "备份文件名应可无损还原为原 mtime");
    }
}
