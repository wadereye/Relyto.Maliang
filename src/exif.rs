/// 一张照片的全部 EXIF 元数据(单次打开、单次解析)。
///
/// 与原版一致的效率取向:`FindNextFileW` 一次拿全属性,
/// EXIF 也只解析一次再分发出多个字段(原版是从同一个 IFD 结构里取的)。
/// 我方原先 `read_taken`/`read_orientation`/`read_gps`/`read_cam` 各自开一次文件
/// 解析一次 EXIF(每文件 4 次 IO),这里合并为 1 次。
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub taken: Option<String>,
    /// Orientation(1..=8);缺省 1。
    pub orientation: u32,
    pub cam: Option<String>,
    pub gps: Option<(f64, f64)>,
    /// 像素宽高(EXIF PixelXDimension/PixelYDimension;缺失为 None)。
    pub width: Option<i32>,
    pub height: Option<i32>,
}

/// 单次打开文件、解析一次 EXIF,取出全部需要的字段。
/// 文件不可读或无 EXIF 时返回各字段为空的 `Meta`(不报错,扫描继续)。
pub fn read_meta(p: &std::path::Path) -> Meta {
    let mut m = Meta {
        orientation: 1,
        ..Default::default()
    };
    let Ok(file) = std::fs::File::open(p) else {
        return m;
    };
    let mut buf = std::io::BufReader::new(file);
    let Ok(exif) = exif::Reader::new().read_from_container(&mut buf) else {
        return m;
    };

    // 拍摄时间:DateTimeOriginal 优先,回退 DateTime。
    m.taken = exif
        .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
        .or_else(|| exif.get_field(exif::Tag::DateTime, exif::In::PRIMARY))
        .and_then(|f| match &f.value {
            exif::Value::Ascii(v) => v.first().and_then(|s| {
                let b: Vec<u8> = s.as_slice().to_vec();
                exif::DateTime::from_ascii(&b).ok().map(|dt| {
                    format!(
                        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                        dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
                    )
                })
            }),
            _ => None,
        });

    // Orientation
    m.orientation = exif
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .filter(|v| (1..=8).contains(v))
        .unwrap_or(1);

    // 相机型号
    let get_ascii = |tag: exif::Tag| -> Option<String> {
        let f = exif.get_field(tag, exif::In::PRIMARY)?;
        match &f.value {
            exif::Value::Ascii(v) => {
                let s: Vec<u8> = v.first()?.as_slice().to_vec();
                Some(String::from_utf8_lossy(&s).trim().to_string())
            }
            _ => None,
        }
    };
    let make = get_ascii(exif::Tag::Make);
    let model = get_ascii(exif::Tag::Model);
    m.cam = match (make, model) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => {
            // Model 常已含 Make 前缀(如 "Canon Canon EOS"),去重
            if b.starts_with(&a) {
                Some(b)
            } else {
                Some(format!("{a} {b}"))
            }
        }
        (Some(a), _) if !a.is_empty() => Some(a),
        (_, Some(b)) if !b.is_empty() => Some(b),
        _ => None,
    };

    // GPS
    m.gps = gps_coord(&exif, exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef, b'S')
        .zip(gps_coord(&exif, exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef, b'W'))
        .filter(|(lat, lon)| (-90.0..=90.0).contains(lat) && (-180.0..=180.0).contains(lon));

    // 像素宽高
    let px = |tag: exif::Tag| -> Option<i32> {
        exif.get_field(tag, exif::In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
            .map(|v| v as i32)
            .filter(|v| *v > 0)
    };
    m.width = px(exif::Tag::PixelXDimension);
    m.height = px(exif::Tag::PixelYDimension);

    m
}

/// 读取拍摄时间（DateTimeOriginal，回退 DateTime）-> ISO "2023-12-31T13:52:06"。
///
/// 便捷单字段读取;批量扫描请用 `read_meta`(一次解析取全部字段)。
#[allow(dead_code)]
pub fn read_taken(p: &std::path::Path) -> Option<String> {
    let file = std::fs::File::open(p).ok()?;
    let mut buf = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut buf).ok()?;
    let f = exif
        .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
        .or_else(|| exif.get_field(exif::Tag::DateTime, exif::In::PRIMARY))?;
    let first = match &f.value {
        exif::Value::Ascii(v) => v.first()?.as_slice().to_vec(),
        _ => return None,
    };
    let dt = exif::DateTime::from_ascii(&first).ok()?;
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
    ))
}

/// 读取 Orientation（1..=8）；缺省返回 1。
pub fn read_orientation(p: &std::path::Path) -> u32 {
    let file = match std::fs::File::open(p) {
        Ok(f) => f,
        Err(_) => return 1,
    };
    let mut buf = std::io::BufReader::new(file);
    let exif = match exif::Reader::new().read_from_container(&mut buf) {
        Ok(e) => e,
        Err(_) => return 1,
    };
    match exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY) {
        Some(f) => f.value.get_uint(0).filter(|v| (1..=8).contains(v)).unwrap_or(1),
        None => 1,
    }
}

/// 读取相机型号(Make + Model 合并;都没有返回 None)。
///
/// 便捷单字段读取;批量扫描请用 `read_meta`。
#[allow(dead_code)]
pub fn read_cam(p: &std::path::Path) -> Option<String> {
    let file = std::fs::File::open(p).ok()?;
    let mut buf = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut buf).ok()?;
    let get = |tag: exif::Tag| -> Option<String> {
        let f = exif.get_field(tag, exif::In::PRIMARY)?;
        match &f.value {
            exif::Value::Ascii(v) => {
                let s: Vec<u8> = v.first()?.as_slice().to_vec();
                Some(String::from_utf8_lossy(&s).trim().to_string())
            }
            _ => None,
        }
    };
    let make = get(exif::Tag::Make);
    let model = get(exif::Tag::Model);
    let joined = match (make, model) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => {
            // Model 常已含 Make 前缀(如 "Canon Canon EOS"),去重
            if b.starts_with(&a) {
                b
            } else {
                format!("{a} {b}")
            }
        }
        (Some(a), _) if !a.is_empty() => a,
        (_, Some(b)) if !b.is_empty() => b,
        _ => return None,
    };
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// 读取 GPS 坐标 -> (纬度, 经度),十进制度;南纬/西经为负。
/// 无 GPS 或数值非法返回 None(相机未开定位的普通照片走此路)。
#[allow(dead_code)]
pub fn read_gps(p: &std::path::Path) -> Option<(f64, f64)> {
    let file = std::fs::File::open(p).ok()?;
    let mut buf = std::io::BufReader::new(file);
    let exif = exif::Reader::new().read_from_container(&mut buf).ok()?;
    let lat = gps_coord(&exif, exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef, b'S')?;
    let lon = gps_coord(&exif, exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef, b'W')?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None; // 范围过滤
    }
    Some((lat, lon))
}

/// 度分秒有理数 + 参考方向 -> 十进制度。
fn gps_coord(exif: &exif::Exif, val_tag: exif::Tag, ref_tag: exif::Tag, neg_ref: u8) -> Option<f64> {
    let f = exif.get_field(val_tag, exif::In::PRIMARY)?;
    let rats: Vec<exif::Rational> = match &f.value {
        exif::Value::Rational(v) => v.clone(),
        _ => return None,
    };
    if rats.len() < 3 {
        return None;
    }
    let to_f64 = |r: &exif::Rational| -> Option<f64> {
        if r.denom == 0 {
            None
        } else {
            Some(r.num as f64 / r.denom as f64)
        }
    };
    let d = to_f64(&rats[0])?;
    let m = to_f64(&rats[1])?;
    let s = to_f64(&rats[2])?;
    let mut deg = d + m / 60.0 + s / 3600.0;
    if let Some(rf) = exif.get_field(ref_tag, exif::In::PRIMARY) {
        if let exif::Value::Ascii(v) = &rf.value {
            if let Some(first) = v.first() {
                if !first.is_empty() && first[0] == neg_ref {
                    deg = -deg;
                }
            }
        }
    }
    Some(deg)
}

/// 提取 JPEG 内嵌 EXIF 缩略图(IFD1 的 JPEGInterchangeFormat/Length)。
/// 对应原版 "Use EXIF thumbnails" 加速:首扫免全图解码。
/// 仅处理 JPEG 压缩的缩略图(相机原图最常见);缺失/非 JPEG 返回 None。
pub fn read_jpeg_embedded_thumb(p: &std::path::Path) -> Option<Vec<u8>> {
    let data = std::fs::read(p).ok()?;
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None; // 不是 JPEG
    }
    let mut i = 2usize;
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            return None;
        }
        let marker = data[i + 1];
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2; // 无长度字段的独立标记
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            break; // EOI / SOS 之后是熵数据,不再扫
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        if len < 2 {
            return None;
        }
        let seg_end = i + 2 + len;
        if seg_end > data.len() {
            return None;
        }
        if marker == 0xE1 && len >= 8 {
            let payload = &data[i + 4..seg_end];
            // "Exif\0\0" 前缀(避免反斜杠字面量,按字节比较)
            if payload.starts_with(b"Exif") && payload[4] == 0 && payload[5] == 0 {
                return extract_tiff_thumbnail(&data, i + 4 + 6);
            }
        }
        i = seg_end;
    }
    None
}

/// 在 TIFF 头(tiff_start 处为 "II"/"MM")里找 IFD1 的 JPEG 缩略图切片。
fn extract_tiff_thumbnail(data: &[u8], tiff_start: usize) -> Option<Vec<u8>> {
    let t = &data[tiff_start..];
    if t.len() < 8 {
        return None;
    }
    let le = match &t[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let rd16 = |b: &[u8]| -> u16 {
        if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    };
    let rd32 = |b: &[u8]| -> u32 {
        if le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        }
    };
    if rd16(&t[2..4]) != 42 {
        return None;
    }
    let ifd0 = rd32(&t[4..8]) as usize;
    let ifd1 = ifd_next(t, ifd0, &rd16, &rd32)?;
    let entries = ifd_entries(t, ifd1, &rd16, &rd32);
    let comp = entries.iter().find(|(tag, _)| *tag == 0x0103).map(|(_, v)| *v);
    if comp != Some(6) {
        return None; // 仅 JPEG 压缩(6)
    }
    let off = entries.iter().find(|(tag, _)| *tag == 0x0201).map(|(_, v)| *v as usize)?;
    let len = entries.iter().find(|(tag, _)| *tag == 0x0202).map(|(_, v)| *v as usize)?;
    if len == 0 || len > 16 * 1024 * 1024 {
        return None;
    }
    let start = tiff_start + off;
    let end = start + len;
    if end > data.len() {
        return None;
    }
    Some(data[start..end].to_vec())
}

/// IFD 的 next-IFD 偏移(entries 之后 4 字节)。
fn ifd_next(t: &[u8], off: usize, rd16: &impl Fn(&[u8]) -> u16, rd32: &impl Fn(&[u8]) -> u32) -> Option<usize> {
    if off + 2 > t.len() {
        return None;
    }
    let n = rd16(&t[off..off + 2]) as usize;
    let next_at = off + 2 + n * 12;
    if next_at + 4 > t.len() {
        return None;
    }
    let v = rd32(&t[next_at..next_at + 4]) as usize;
    if v == 0 {
        None
    } else {
        Some(v)
    }
}

/// 解析 IFD 条目为 (tag, 值 u32)(count=1 的 LONG 内联;SHORT 取前 2 字节)。
fn ifd_entries(t: &[u8], off: usize, rd16: &impl Fn(&[u8]) -> u16, rd32: &impl Fn(&[u8]) -> u32) -> Vec<(u16, u32)> {
    let mut out = Vec::new();
    if off + 2 > t.len() {
        return out;
    }
    let n = rd16(&t[off..off + 2]) as usize;
    for k in 0..n {
        let e = off + 2 + k * 12;
        if e + 12 > t.len() {
            break;
        }
        let tag = rd16(&t[e..e + 2]);
        let typ = rd16(&t[e + 2..e + 4]);
        let cnt = rd32(&t[e + 4..e + 8]);
        let raw = rd32(&t[e + 8..e + 12]);
        let v = if typ == 3 && cnt == 1 {
            rd16(&t[e + 8..e + 10]) as u32
        } else {
            raw
        };
        out.push((tag, v));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn push_tiff_entry(buf: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: u32) {
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&typ.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&value.to_le_bytes());
    }

    /// 最小 JPEG + Exif + GPS(小端 TIFF)。
    fn jpeg_with_gps(lat: (u32, u32, u32), lat_ref: u8, lon: (u32, u32, u32), lon_ref: u8) -> Vec<u8> {
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 @8
        let gps_at = 8 + 2 + 12 + 4;
        tiff.extend_from_slice(&1u16.to_le_bytes()); // IFD0:1 条
        push_tiff_entry(&mut tiff, 0x8825, 4, 1, gps_at as u32); // GPSInfo 指针
        tiff.extend_from_slice(&0u32.to_le_bytes()); // next=0
        tiff.extend_from_slice(&4u16.to_le_bytes()); // GPS IFD:4 条
        let entries_len = 2 + 4 * 12 + 4;
        let lat_at = gps_at + entries_len;
        let lon_at = lat_at + 24;
        push_tiff_entry(&mut tiff, 0x0001, 2, 2, u32::from_le_bytes([lat_ref, 0, 0, 0]));
        push_tiff_entry(&mut tiff, 0x0002, 5, 3, lat_at as u32);
        push_tiff_entry(&mut tiff, 0x0003, 2, 2, u32::from_le_bytes([lon_ref, 0, 0, 0]));
        push_tiff_entry(&mut tiff, 0x0004, 5, 3, lon_at as u32);
        tiff.extend_from_slice(&0u32.to_le_bytes()); // next=0
        for (n, d) in [(lat.0, 1u32), (lat.1, 1), (lat.2, 100)] {
            tiff.extend_from_slice(&n.to_le_bytes());
            tiff.extend_from_slice(&d.to_le_bytes());
        }
        for (n, d) in [(lon.0, 1u32), (lon.1, 1), (lon.2, 100)] {
            tiff.extend_from_slice(&n.to_le_bytes());
            tiff.extend_from_slice(&d.to_le_bytes());
        }
        let mut jpg: Vec<u8> = vec![0xFF, 0xD8];
        let seg_len = 2 + 6 + tiff.len();
        jpg.extend_from_slice(&[0xFF, 0xE1]);
        jpg.extend_from_slice(&(seg_len as u16).to_be_bytes());
        jpg.extend_from_slice(b"Exif\0\0");
        jpg.extend_from_slice(&tiff);
        jpg.extend_from_slice(&[0xFF, 0xD9]);
        jpg
    }

    #[test]
    fn read_gps_parses_deg_min_sec() {
        let dir = std::env::temp_dir().join(format!("rk-gps-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("g.jpg");
        // 31°14'21.6"N 121°28'12"E -> 31.2393, 121.48
        std::fs::File::create(&p)
            .unwrap()
            .write_all(&jpeg_with_gps((31, 14, 2160), b'N', (121, 28, 1200), b'E'))
            .unwrap();
        let (lat, lon) = read_gps(&p).expect("应解析出 GPS");
        assert!((lat - 31.2393).abs() < 1e-4, "lat={lat}");
        assert!((lon - 121.47).abs() < 1e-4, "lon={lon}");
        // 南/西为负
        let p2 = dir.join("s.jpg");
        std::fs::File::create(&p2)
            .unwrap()
            .write_all(&jpeg_with_gps((31, 14, 2160), b'S', (121, 28, 1200), b'W'))
            .unwrap();
        let (lat2, lon2) = read_gps(&p2).expect("应解析出 GPS(南西)");
        assert!(lat2 < 0.0 && lon2 < 0.0, "{lat2},{lon2}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_gps_none_for_plain_jpeg() {
        let dir = std::env::temp_dir().join(format!("rk-gps2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("plain.jpg");
        std::fs::write(&p, [0xFF, 0xD8, 0xFF, 0xD9]).unwrap();
        assert!(read_gps(&p).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 最小 JPEG + Exif + IFD1 JPEG 缩略图(小端 TIFF)。
    fn jpeg_with_embedded_thumb(payload: &[u8]) -> Vec<u8> {
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 @8
        let ifd1_at = 8 + 2 + 0 * 12 + 4;
        tiff.extend_from_slice(&0u16.to_le_bytes()); // IFD0 空
        tiff.extend_from_slice(&(ifd1_at as u32).to_le_bytes()); // next=IFD1
        tiff.extend_from_slice(&3u16.to_le_bytes()); // IFD1:3 条
        let entries_len = 2 + 3 * 12 + 4;
        let thumb_at = ifd1_at + entries_len;
        push_tiff_entry(&mut tiff, 0x0103, 3, 1, 6); // JPEG 压缩
        push_tiff_entry(&mut tiff, 0x0201, 4, 1, thumb_at as u32);
        push_tiff_entry(&mut tiff, 0x0202, 4, 1, payload.len() as u32);
        tiff.extend_from_slice(&0u32.to_le_bytes()); // next=0
        tiff.extend_from_slice(payload);
        let mut jpg: Vec<u8> = vec![0xFF, 0xD8];
        let seg_len = 2 + 6 + tiff.len();
        jpg.extend_from_slice(&[0xFF, 0xE1]);
        jpg.extend_from_slice(&(seg_len as u16).to_be_bytes());
        jpg.extend_from_slice(b"Exif\0\0");
        jpg.extend_from_slice(&tiff);
        jpg.extend_from_slice(&[0xFF, 0xD9]);
        jpg
    }

    #[test]
    fn extract_embedded_thumbnail() {
        let dir = std::env::temp_dir().join(format!("rk-thumb-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let payload = [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4];
        let p = dir.join("t.jpg");
        std::fs::write(&p, jpeg_with_embedded_thumb(&payload)).unwrap();
        let got = read_jpeg_embedded_thumb(&p).expect("应提取到内嵌缩略图");
        assert_eq!(got, payload);
        // 无 EXIF 的普通 JPEG -> None
        let p2 = dir.join("n.jpg");
        std::fs::write(&p2, [0xFF, 0xD8, 0xFF, 0xD9]).unwrap();
        assert!(read_jpeg_embedded_thumb(&p2).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
