/// 一张照片的全部 EXIF 元数据(单次打开、单次解析)。
///
/// 效率取向:`FindNextFileW` 一次拿全属性,
/// EXIF 也只解析一次再分发出多个字段。
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
/// 用内嵌 EXIF 缩略图加速:首扫免全图解码。
/// 仅处理 JPEG 压缩的缩略图(相机原图最常见);缺失/非 JPEG 返回 None。
///
/// 只认 **IFD1**:JPEG 的 IFD0 描述的是主图本身,它的 StripOffsets 指向
/// JPEG 熵数据,当缩略图用等于没加速。
pub fn read_jpeg_embedded_thumb(p: &std::path::Path) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(p).ok()?;
    let head = read_head(&mut f, HEAD_SCAN);
    let base = jpeg_exif_base(&head)?;
    let mut t = Tiff::open(&mut f, base)?;
    let ifd0 = t.ifd0;
    let (_, ifd1) = read_ifd(&mut t, ifd0)?;
    if ifd1 == 0 {
        return None;
    }
    let (entries, _) = read_ifd(&mut t, ifd1)?;
    let mut slices = Vec::new();
    ifd_slices(&t, &entries, &mut slices);
    let (off, len) = slices.first().copied()?;
    read_exact_at(t.into_file(), base + off, len as usize)
}

// ================= TIFF 容器的按需读取 =================
//
// 相机 RAW 与高分辨率 JPEG 动辄几十 MB,而派生是 rayon 并行的
// (批大小可达 128)—— 每个文件 `fs::read` 整读会把峰值内存推到 GB 级。
// 下面这套只 seek 读真正需要的窗口:头部标记、IFD 目录、最终那一段切片。

/// 头部扫描窗口:JPEG 的 APP1 Exif 段几乎总在文件最前面。
const HEAD_SCAN: usize = 256 * 1024;
/// 判定 SOF 类型时探测的字节数(内嵌预览自带 EXIF 段,SOF 可能落在几 KB 之后)。
const SOF_PROBE: usize = 16 * 1024;
/// 单张内嵌预览的体积上限(超过的一律不是预览,是 raw 数据)。
const MAX_SLICE: usize = 24 * 1024 * 1024;
/// IFD 链的遍历深度上限(坏文件的 next 指针可能成环)。
const MAX_IFD_DEPTH: usize = 3;
/// 一次最多返回几个预览候选。
pub const MAX_PREVIEW_CANDIDATES: usize = 3;

/// 从 `off` 起精确读 `len` 字节;越界或短读返回 None。
fn read_exact_at(f: &mut std::fs::File, off: u64, len: usize) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    if len == 0 || len > MAX_SLICE {
        return None;
    }
    f.seek(SeekFrom::Start(off)).ok()?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// 读文件开头**至多** `len` 字节(短文件按实际长度返回)。
fn read_head(f: &mut std::fs::File, len: usize) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let _ = f.seek(SeekFrom::Start(0));
    let mut v = Vec::with_capacity(len.min(64 * 1024));
    let _ = (&mut *f).take(len as u64).read_to_end(&mut v);
    v
}

/// 在 JPEG 标记流里找 APP1 Exif 段,返回 TIFF 头的**绝对**文件偏移。
///
/// 段被扫描窗口截断时当作没有 EXIF —— 那样的文件极少,代价只是退回整图解码。
fn jpeg_exif_base(head: &[u8]) -> Option<u64> {
    if head.len() < 4 || head[0] != 0xFF || head[1] != 0xD8 {
        return None; // 不是 JPEG
    }
    let mut i = 2usize;
    while i + 4 <= head.len() {
        if head[i] != 0xFF {
            return None;
        }
        let marker = head[i + 1];
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2; // 无长度字段的独立标记
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            return None; // EOI / SOS 之后是熵数据,EXIF 只可能在前面
        }
        let len = u16::from_be_bytes([head[i + 2], head[i + 3]]) as usize;
        if len < 2 {
            return None;
        }
        let seg_end = i + 2 + len;
        if seg_end > head.len() {
            return None;
        }
        if marker == 0xE1 && len >= 8 {
            let payload = &head[i + 4..seg_end];
            // "Exif\0\0" 前缀(按字节比较,避免反斜杠字面量)
            if payload.starts_with(b"Exif") && payload[4] == 0 && payload[5] == 0 {
                return Some((i + 4 + 6) as u64);
            }
        }
        i = seg_end;
    }
    None
}

/// 这段字节是不是 `image` crate 解得了的 JPEG(基线 / 扩展 / 渐进)。
///
/// NEF / CR2 的 **raw** IFD 也写着 JPEG 压缩(6),但那是无损 JPEG(SOF3),
/// 解不了 —— 先花 16KB 判掉,免得把 20MB 的 raw 数据白读进内存。
///
/// 扫到探测窗口末尾还没定论时**放行**:误放只是解码失败后试下一个候选,
/// 误杀则会让一张本来能用的预览白白退回整图解码。
fn jpeg_decodable(head: &[u8]) -> bool {
    if head.len() < 4 || head[0] != 0xFF || head[1] != 0xD8 {
        return false;
    }
    let mut i = 2usize;
    while i + 4 <= head.len() {
        if head[i] != 0xFF {
            return false; // 标记流断了:结构非法
        }
        let m = head[i + 1];
        if m == 0x01 || (0xD0..=0xD7).contains(&m) {
            i += 2;
            continue;
        }
        if m == 0xD9 || m == 0xDA {
            return false; // 见到熵数据前都没碰上 SOF
        }
        if (0xC0..=0xC2).contains(&m) {
            return true; // SOF0 基线 / SOF1 扩展 / SOF2 渐进
        }
        // SOF3..SOF15 里的无损 / 算术 / 差分编码;0xC4(DHT)与 0xC8 不是 SOF,要跳过。
        if (0xC3..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8) {
            return false;
        }
        let len = u16::from_be_bytes([head[i + 2], head[i + 3]]) as usize;
        if len < 2 {
            return false;
        }
        i += 2 + len;
    }
    true
}

/// 一条 IFD 目录项。
#[derive(Clone, Copy)]
struct IfdEntry {
    tag: u16,
    typ: u16,
    cnt: u32,
    val: u32,
}

impl IfdEntry {
    /// `cnt==1` 时的标量值。SHORT 的值存在 4 字节槽的**前两字节**里,
    /// 小端整读出来落在低 16 位,大端整读出来落在高 16 位。
    fn scalar(self, le: bool) -> u32 {
        if self.typ == 3 && self.cnt == 1 {
            if le {
                self.val & 0xFFFF
            } else {
                self.val >> 16
            }
        } else {
            self.val
        }
    }
}

/// TIFF 容器读取器。`base` 是 TIFF 头在文件里的绝对偏移
/// (JPEG 里是 APP1 的 `Exif\0\0` 之后;裸 TIFF / RAW 里是 0),
/// 目录项里的所有偏移都相对它。
struct Tiff<'a> {
    f: &'a mut std::fs::File,
    base: u64,
    le: bool,
    ifd0: u32,
}

impl<'a> Tiff<'a> {
    /// 校验 TIFF 头(II/MM + 42)并定位 IFD0。
    fn open(f: &'a mut std::fs::File, base: u64) -> Option<Self> {
        let h = read_exact_at(f, base, 8)?;
        let le = match (h[0], h[1]) {
            (b'I', b'I') => true,
            (b'M', b'M') => false,
            _ => return None,
        };
        let rd16 = |i: usize| {
            if le {
                u16::from_le_bytes([h[i], h[i + 1]])
            } else {
                u16::from_be_bytes([h[i], h[i + 1]])
            }
        };
        let rd32 = |i: usize| {
            if le {
                u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]])
            } else {
                u32::from_be_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]])
            }
        };
        if rd16(2) != 42 {
            return None;
        }
        let ifd0 = rd32(4);
        if ifd0 == 0 {
            return None;
        }
        Some(Self { f, base, le, ifd0 })
    }

    /// 按**相对 TIFF 头**的偏移读一段。
    fn bytes(&mut self, off: u64, len: usize) -> Option<Vec<u8>> {
        read_exact_at(self.f, self.base + off, len)
    }

    fn u16(&mut self, off: u64) -> Option<u16> {
        let b = self.bytes(off, 2)?;
        Some(if self.le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    }

    fn u32(&mut self, off: u64) -> Option<u32> {
        let b = self.bytes(off, 4)?;
        Some(if self.le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    }

    /// 交还文件句柄(读切片阶段不再需要 TIFF 状态)。
    fn into_file(self) -> &'a mut std::fs::File {
        self.f
    }
}

/// 读一个 IFD:返回(目录项, next-IFD 偏移)。
fn read_ifd(t: &mut Tiff, ifd_off: u32) -> Option<(Vec<IfdEntry>, u32)> {
    let n_true = t.u16(ifd_off as u64)? as usize;
    // next 指针的位置按**真实**条数算,截断只影响我们读多少条。
    let next = t
        .u32(ifd_off as u64 + 2 + (n_true * 12) as u64)
        .unwrap_or(0);
    let n = n_true.min(1024);
    if n == 0 {
        return Some((Vec::new(), next));
    }
    let raw = t.bytes(ifd_off as u64 + 2, n * 12)?;
    let le = t.le;
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let e = &raw[k * 12..k * 12 + 12];
        let rd16 = |i: usize| {
            if le {
                u16::from_le_bytes([e[i], e[i + 1]])
            } else {
                u16::from_be_bytes([e[i], e[i + 1]])
            }
        };
        let rd32 = |i: usize| {
            if le {
                u32::from_le_bytes([e[i], e[i + 1], e[i + 2], e[i + 3]])
            } else {
                u32::from_be_bytes([e[i], e[i + 1], e[i + 2], e[i + 3]])
            }
        };
        out.push(IfdEntry {
            tag: rd16(0),
            typ: rd16(2),
            cnt: rd32(4),
            val: rd32(8),
        });
    }
    Some((out, next))
}

/// 从一个 IFD 收 JPEG 压缩(Compression=6)的图像切片,偏移相对 TIFF 头。
fn ifd_slices(t: &Tiff, entries: &[IfdEntry], out: &mut Vec<(u64, u64)>) {
    let le = t.le;
    let find = |tag: u16| entries.iter().find(|e| e.tag == tag).copied();
    let Some(comp) = find(0x0103) else { return };
    if comp.scalar(le) != 6 {
        return; // 只要 JPEG 压缩
    }
    let mut push = |off: u64, len: u64| {
        if len > 0 && len <= MAX_SLICE as u64 {
            out.push((off, len));
        }
    };
    // ① JPEGInterchangeFormat / Length —— EXIF 缩略图与相机内嵌预览的常规写法。
    if let (Some(o), Some(l)) = (find(0x0201), find(0x0202)) {
        push(o.scalar(le) as u64, l.scalar(le) as u64);
        return;
    }
    // ② 单条带 StripOffsets / StripByteCounts(DNG 等把预览写成一条带)。
    //    多条带要拼接且各家不保证连续,不碰。
    if let (Some(so), Some(sb)) = (find(0x0111), find(0x0117)) {
        if so.cnt == 1 && sb.cnt == 1 {
            push(so.scalar(le) as u64, sb.scalar(le) as u64);
        }
    }
}

/// SubIFDs(0x014A)指向的一个或多个 IFD 偏移。
///
/// Nikon NEF 的全尺寸预览、DNG 的 raw + 预览都挂在这里。
fn sub_ifd_offsets(t: &mut Tiff, e: Option<IfdEntry>) -> Vec<u32> {
    let Some(e) = e else { return Vec::new() };
    if e.typ != 4 {
        return Vec::new(); // 只认 LONG
    }
    if e.cnt <= 1 {
        return vec![e.scalar(t.le)];
    }
    let n = e.cnt.min(8) as usize;
    let Some(raw) = t.bytes(e.val as u64, n * 4) else {
        return Vec::new();
    };
    let le = t.le;
    (0..n)
        .map(|k| {
            let b = &raw[k * 4..k * 4 + 4];
            if le {
                u32::from_le_bytes([b[0], b[1], b[2], b[3]])
            } else {
                u32::from_be_bytes([b[0], b[1], b[2], b[3]])
            }
        })
        .collect()
}

/// TIFF 容器里的一张内嵌 JPEG 预览。
pub struct TiffPreview {
    /// 切片字节(已判定为 `image` crate 解得了的基线/渐进 JPEG)。
    pub bytes: Vec<u8>,
    /// IFD0 声明的图像宽高(缺标签为 0)。
    pub base_w: u32,
    pub base_h: u32,
    /// IFD0 的 Orientation(1..=8;缺省 1)。
    pub orient: u32,
}

/// 取容器里的内嵌 JPEG 预览候选,按体积降序。
///
/// 适用于 JPEG(APP1 Exif)、裸 TIFF 与相机 RAW。遍历 IFD0 → SubIFD → IFD1,
/// 无损 JPEG 的 raw IFD 在只读 16KB 头部时就被剔掉。
/// 非 TIFF / 无候选返回空表,调用方落回 Shell 或整图解码。
pub fn read_tiff_previews(p: &std::path::Path, max: usize) -> Vec<TiffPreview> {
    if max == 0 {
        return Vec::new();
    }
    let Ok(mut f) = std::fs::File::open(p) else {
        return Vec::new();
    };
    let head = read_head(&mut f, HEAD_SCAN);
    let is_jpeg = head.len() >= 2 && head[0] == 0xFF && head[1] == 0xD8;
    // 裸 TIFF / RAW 的 TIFF 头就在文件开头;JPEG 必须先找到 APP1。
    let base = if is_jpeg {
        match jpeg_exif_base(&head) {
            Some(b) => b,
            None => return Vec::new(),
        }
    } else {
        0
    };
    let Some(mut t) = Tiff::open(&mut f, base) else {
        return Vec::new();
    };
    let ifd0 = t.ifd0;
    let Some((entries0, next0)) = read_ifd(&mut t, ifd0) else {
        return Vec::new();
    };
    let le = t.le;
    let attr = |tag: u16| {
        entries0
            .iter()
            .find(|e| e.tag == tag)
            .map(|e| e.scalar(le))
            .unwrap_or(0)
    };
    let orient = match attr(0x0112) {
        v if (1..=8).contains(&v) => v,
        _ => 1,
    };
    let (base_w, base_h) = (attr(0x0100), attr(0x0101));

    let mut slices: Vec<(u64, u64)> = Vec::new();
    let mut cur = Some((entries0, next0));
    for _ in 0..=MAX_IFD_DEPTH {
        let Some((entries, next)) = cur.take() else {
            break;
        };
        ifd_slices(&t, &entries, &mut slices);
        let subs = sub_ifd_offsets(&mut t, entries.iter().find(|e| e.tag == 0x014A).copied());
        for sub in subs {
            if let Some((se, _)) = read_ifd(&mut t, sub) {
                ifd_slices(&t, &se, &mut slices);
            }
        }
        if next == 0 {
            break;
        }
        cur = read_ifd(&mut t, next);
    }

    // 大切片优先:相机 RAW 里最大的那段内嵌 JPEG 才是能用的预览。
    slices.sort_by_key(|(_, len)| std::cmp::Reverse(*len));
    slices.dedup();
    let f = t.into_file();
    let mut out = Vec::new();
    for (off, len) in slices {
        if out.len() >= max {
            break;
        }
        // 先探头部再决定是否把整段读进来 —— raw IFD 的无损 JPEG 在这一步被剔掉。
        let Some(h) = read_exact_at(f, base + off, (len as usize).min(SOF_PROBE)) else {
            continue;
        };
        if !jpeg_decodable(&h) {
            continue;
        }
        let Some(bytes) = read_exact_at(f, base + off, len as usize) else {
            continue;
        };
        out.push(TiffPreview {
            bytes,
            base_w,
            base_h,
            orient,
        });
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
        // IFD0 是空的:8(TIFF 头)+ 2(计数=0)+ 0 条目录项 + 4(next 指针)。
        let ifd1_at = 8 + 2 + 4;
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

    /// 裸 TIFF(相机 RAW 的容器形状):IFD0 挂 SubIFD,预览切片在 SubIFD 里。
    /// `sof` 是预览 JPEG 的第一个 SOF 标记(0xC0 基线 / 0xC3 无损)。
    fn raw_with_subifd_preview(payload_sof: u8, orient: u32) -> Vec<u8> {
        let preview: Vec<u8> = vec![0xFF, 0xD8, 0xFF, payload_sof, 0x00, 0x11, 7, 7, 7, 7];
        let mut t: Vec<u8> = Vec::new();
        t.extend_from_slice(b"II");
        t.extend_from_slice(&42u16.to_le_bytes());
        t.extend_from_slice(&8u32.to_le_bytes()); // IFD0 @8
        let sub_at = 8 + 2 + 4 * 12 + 4;
        let preview_at = sub_at + 2 + 3 * 12 + 4;
        t.extend_from_slice(&4u16.to_le_bytes()); // IFD0:4 条
        push_tiff_entry(&mut t, 0x0100, 4, 1, 6000); // ImageWidth
        push_tiff_entry(&mut t, 0x0101, 4, 1, 4000); // ImageLength
        push_tiff_entry(&mut t, 0x0112, 3, 1, orient); // Orientation
        push_tiff_entry(&mut t, 0x014A, 4, 1, sub_at as u32); // SubIFDs
        t.extend_from_slice(&0u32.to_le_bytes()); // next=0
        t.extend_from_slice(&3u16.to_le_bytes()); // SubIFD:3 条
        push_tiff_entry(&mut t, 0x0103, 3, 1, 6); // JPEG 压缩
        push_tiff_entry(&mut t, 0x0201, 4, 1, preview_at as u32);
        push_tiff_entry(&mut t, 0x0202, 4, 1, preview.len() as u32);
        t.extend_from_slice(&0u32.to_le_bytes()); // next=0
        t.extend_from_slice(&preview);
        t
    }

    /// RAW 的内嵌预览优先:SubIFD 里的 JPEG 切片要被找出来,
    /// 并连带 IFD0 的宽高 / Orientation 一起交给调用方判定要不要再转一次。
    #[test]
    fn raw_subifd_preview_is_found_with_ifd0_attrs() {
        let dir = std::env::temp_dir().join(format!("rk-raw-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("shot.nef");
        std::fs::write(&p, raw_with_subifd_preview(0xC0, 6)).unwrap();
        let got = read_tiff_previews(&p, MAX_PREVIEW_CANDIDATES);
        assert_eq!(got.len(), 1, "SubIFD 的 JPEG 切片应被找到");
        assert_eq!(got[0].base_w, 6000);
        assert_eq!(got[0].base_h, 4000);
        assert_eq!(got[0].orient, 6);
        assert_eq!(&got[0].bytes[0..2], &[0xFF, 0xD8]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无损 JPEG(SOF3)是 **raw 数据**不是预览:必须在只读 16KB 头部时剔掉,
    /// 否则几十 MB 的 raw 会被整段读进内存(并行派生下直接爆)。
    #[test]
    fn raw_lossless_ifd_is_rejected_by_header_probe() {
        let dir = std::env::temp_dir().join(format!("rk-raw2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("shot.cr2");
        std::fs::write(&p, raw_with_subifd_preview(0xC3, 1)).unwrap();
        assert!(
            read_tiff_previews(&p, MAX_PREVIEW_CANDIDATES).is_empty(),
            "SOF3 无损 JPEG 不得当预览返回"
        );
        // 非 TIFF / 非 JPEG 的普通文件同样是空表(调用方落回 Shell 或整图解码)
        let p2 = dir.join("plain.png");
        std::fs::write(&p2, [0x89, b'P', b'N', b'G']).unwrap();
        assert!(read_tiff_previews(&p2, MAX_PREVIEW_CANDIDATES).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// JPEG 的 APP1 Exif 也走同一套扫描器:IFD1 的缩略图应能被 `read_tiff_previews` 找到。
    #[test]
    fn jpeg_app1_preview_via_tiff_scanner() {
        let dir = std::env::temp_dir().join(format!("rk-raw3-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let payload: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 1, 2, 3, 4];
        let p = dir.join("t2.jpg");
        std::fs::write(&p, jpeg_with_embedded_thumb(&payload)).unwrap();
        let got = read_tiff_previews(&p, MAX_PREVIEW_CANDIDATES);
        assert_eq!(got.len(), 1, "IFD1 的 JPEG 缩略图应被找到");
        assert_eq!(got[0].bytes, payload);
        assert_eq!(got[0].orient, 1, "缺 Orientation 标签 → 1");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
