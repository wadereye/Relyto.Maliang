//! P2-18 分享链路:邮件(Simple MAPI)与礼品光盘(IMAPI2)。
//!
//! 参考 Picasa 3 的分享实现:
//! - **邮件 = Simple MAPI**:`LoadLibraryA("MAPI32.DLL")` + `GetProcAddress("MAPISendMail")`,
//!   发送 flags=9(`MAPI_LOGON_UI|MAPI_DIALOG`),附件暂存 `temp\email\`,
//!   文件名格式 `"%0*d-%s"`(序号零填充)。
//! - **尺寸选项**:1600/1024/800。
//! - **刻录 = IMAPI2**:走 IMAPI2 COM 文件系统映像接口
//!   (`DDiscFormat2EraseEvents`);主程序有 IMAPI2 引擎连接失败的错误提示。
//!   盘上卷标 "Picasa CD",JPEG 质量 85(`option_jpegquality=0x55`),
//!   清单导出 `option_manifest`。
//!
//! 与原版的差异(诚实声明,不臆造):
//! 1. 原版用 ANSI `MAPISendMail`;我方用同 DLL 的 `MAPISendMailW`(Vista+ 标准导出),
//!    否则中文路径在 ANSI 代码页会丢字。flags/结构体逐字段对齐。
//! 2. 原版 Gift CD 会复制幻灯播放器到盘上并写 `autorun.inf`;
//!    我方没有盘上播放器,故**不写 autorun.inf**,只写照片 + `manifest.txt`
//!    (`option_manifest` 有原版语义支撑)。
//! 3. 原版刻录走插件 Primo 备援;我方只用 IMAPI2(与原版主引擎一致)。

use crate::effects::FilterStack;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

/// 邮件/光盘尺寸档(与原版 UI 文案一致:1600/1024/800)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareSize {
    Large,
    Medium,
    Small,
}

impl ShareSize {
    /// 最长边像素(与原版一致:1600 / 1024 / 800)。
    #[must_use]
    pub fn px(self) -> u32 {
        match self {
            ShareSize::Large => 1600,
            ShareSize::Medium => 1024,
            ShareSize::Small => 800,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ShareSize::Large => "大:1600 像素(打印/分享)",
            ShareSize::Medium => "中:1024 像素(分享)",
            ShareSize::Small => "小:800 像素(博客/网页)",
        }
    }

    pub const ALL: [ShareSize; 3] = [
        ShareSize::Large,
        ShareSize::Medium,
        ShareSize::Small,
    ];
}

/// 原版附件文件名格式 `"%0*d-%s"`:
/// 零填充序号(宽度 = 总数的十进制位数)-原文件名。
#[must_use]
pub fn share_file_name(index: usize, total: usize, stem: &str) -> String {
    let width = total.to_string().len().max(1);
    format!("{:0width$}-{stem}", index + 1, width = width)
}

/// JPEG 编码质量(原版 `option_jpegquality=0x55`=85,刻录选项;
/// 邮件专属质量未确认,沿用同一常量)。
pub const SHARE_JPEG_QUALITY: u8 = 85;

/// 把条目(原图 + 可选效果栈)重采样到 `max_px` 内并编码为 JPEG 写入 `dir`。
/// 返回暂存文件列表(顺序与条目一致)。
pub fn stage_jpegs(
    items: &[(PathBuf, FilterStack)],
    max_px: u32,
    dir: &Path,
) -> Result<Vec<PathBuf>> {
    if items.is_empty() {
        return Err(anyhow!("没有可分享的照片(原版 IDS_NO_IMAGES_TO_SEND)"));
    }
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).with_context(|| format!("创建暂存目录失败: {}", dir.display()))?;
    let mut out = Vec::with_capacity(items.len());
    for (i, (src, stack)) in items.iter().enumerate() {
        let img = image::open(src).with_context(|| format!("解码失败: {}", src.display()))?;
        let mut rgba = img.to_rgba8();
        if !stack.is_empty() {
            crate::effects::apply_stack(&mut rgba, stack);
        }
        let flat = image::DynamicImage::ImageRgba8(rgba);
        // 重采样(原版统一导出管线有 "Resample-%d" 标记,tsv:761)
        let flat = if flat.width().max(flat.height()) > max_px {
            let (w, h) = if flat.width() >= flat.height() {
                (
                    max_px,
                    (u64::from(flat.height()) * u64::from(max_px) / u64::from(flat.width())).max(1)
                        as u32,
                )
            } else {
                (
                    (u64::from(flat.width()) * u64::from(max_px) / u64::from(flat.height())).max(1)
                        as u32,
                    max_px,
                )
            };
            flat.resize_exact(w, h, image::imageops::FilterType::Triangle)
        } else {
            flat
        };
        let stem = src
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("photo{i}"));
        let name = format!("{}.jpg", share_file_name(i, items.len(), &stem));
        let target = dir.join(name);
        let mut jpg: Vec<u8> = Vec::new();
        let mut enc =
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, SHARE_JPEG_QUALITY);
        enc.encode(
            flat.to_rgb8().as_raw(),
            flat.width(),
            flat.height(),
            image::ExtendedColorType::Rgb8,
        )?;
        std::fs::write(&target, &jpg)?;
        out.push(target);
    }
    Ok(out)
}

/// 礼品光盘内容暂存:照片 + `manifest.txt` 清单(原版 `option_manifest`)。
/// **不写 autorun.inf**(原版指向盘上的幻灯播放器;我方无盘上播放器)。
pub fn stage_gift_cd(
    items: &[(PathBuf, FilterStack)],
    max_px: u32,
    dir: &Path,
) -> Result<Vec<PathBuf>> {
    let files = stage_jpegs(items, max_px, dir)?;
    let mut manifest = String::from("Maliang Gift CD\r\n====\r\n");
    for f in &files {
        let name = f
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        manifest.push_str(&name);
        manifest.push_str("\r\n");
    }
    std::fs::write(dir.join("manifest.txt"), manifest)?;
    Ok(files)
}

#[cfg(windows)]
mod win {
    use super::*;
    use windows::core::{w, BSTR, PCSTR, PWSTR};
    use windows::Win32::Foundation::VARIANT_BOOL;
    use windows::Win32::Storage::Imapi::{
        IDiscFormat2Data, IDiscMaster2, IDiscRecorder2, IFileSystemImage, MsftDiscFormat2Data,
        MsftDiscMaster2, MsftDiscRecorder2, MsftFileSystemImage,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    };

    /// Simple MAPI 收件人描述(MapiRecipDescW,SDK 字段逐一对齐)。
    #[repr(C)]
    #[allow(dead_code)]
    struct MapiRecipDescW {
        ul_reserved: u32,
        fl_flags: u32,
        n_position: u32,
        lpsz_name: PWSTR,
        lpsz_address: PWSTR,
        ul_eid_size: u32,
        lp_entry_id: *mut core::ffi::c_void,
    }

    /// Simple MAPI 附件描述(MapiFileDescW)。
    #[repr(C)]
    struct MapiFileDescW {
        ul_reserved: u32,
        fl_flags: u32,
        n_position: u32,
        lpsz_path_name: PWSTR,
        lpsz_file_name: PWSTR,
        lp_file_type: *mut core::ffi::c_void,
    }

    /// Simple MAPI 消息(MapiMessageW)。
    #[repr(C)]
    struct MapiMessageW {
        ul_reserved: u32,
        lpsz_subject: PWSTR,
        lpsz_note_text: PWSTR,
        lpsz_message_type: PWSTR,
        lpsz_date_received: PWSTR,
        lpsz_conversation_id: PWSTR,
        fl_flags: u32,
        lp_originator: *mut MapiRecipDescW,
        n_recip_count: u32,
        lp_recips: *mut MapiRecipDescW,
        n_file_count: u32,
        lp_files: *mut MapiFileDescW,
    }

    type SendMailWFn = unsafe extern "system" fn(usize, usize, *mut MapiMessageW, u32, u32) -> u32;

    /// 用系统默认邮件客户端打开带附件的撰写窗口。
    ///
    /// 对齐原版:`mapi32.dll` 动态加载(MAPI 动态加载)+
    /// 发送 flags=9(邮件发送,`MAPI_LOGON_UI|MAPI_DIALOG`)。
    /// 差异:用 Unicode 导出 `MAPISendMailW`(中文路径不丢字),结构体逐字段对齐。
    pub fn send_email(subject: &str, files: &[PathBuf]) -> Result<()> {
        if files.is_empty() {
            return Err(anyhow!("没有附件(原版 IDS_NO_IMAGES_TO_SEND)"));
        }
        unsafe {
            // 原版把 mapi32.dll 句柄缓存在全局且从不 FreeLibrary,
            // 我方同语义(进程退出由系统回收)。
            let lib = windows::Win32::System::LibraryLoader::LoadLibraryW(w!("MAPI32.DLL"))
                .map_err(|e| anyhow!("加载 mapi32.dll 失败: {e}"))?;
            let proc = windows::Win32::System::LibraryLoader::GetProcAddress(
                lib,
                PCSTR::from_raw(b"MAPISendMailW\0".as_ptr()),
            );
            let Some(proc) = proc else {
                return Err(anyhow!("mapi32.dll 无 MAPISendMailW(系统过旧)"));
            };
            let send: SendMailWFn = std::mem::transmute(proc);

            // 字符串缓冲必须活到调用结束
            let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
            let mut subject_buf = wide(subject);
            let mut path_bufs: Vec<Vec<u16>> =
                files.iter().map(|f| wide(&f.to_string_lossy())).collect();
            let mut file_descs: Vec<MapiFileDescW> = path_bufs
                .iter_mut()
                .enumerate()
                .map(|(i, buf)| MapiFileDescW {
                    ul_reserved: 0,
                    fl_flags: 0,
                    n_position: (i + 1) as u32,
                    lpsz_path_name: PWSTR(buf.as_mut_ptr()),
                    lpsz_file_name: PWSTR::null(),
                    lp_file_type: std::ptr::null_mut(),
                })
                .collect();
            let mut msg = MapiMessageW {
                ul_reserved: 0,
                lpsz_subject: PWSTR(subject_buf.as_mut_ptr()),
                lpsz_note_text: PWSTR::null(),
                lpsz_message_type: PWSTR::null(),
                lpsz_date_received: PWSTR::null(),
                lpsz_conversation_id: PWSTR::null(),
                fl_flags: 0,
                lp_originator: std::ptr::null_mut(),
                n_recip_count: 0,
                lp_recips: std::ptr::null_mut(),
                n_file_count: files.len() as u32,
                lp_files: file_descs.as_mut_ptr(),
            };
            // flags = 9 = MAPI_LOGON_UI | MAPI_DIALOG(原版邮件发送)
            let hr = send(0, 0, &mut msg, 9, 0);
            // SUCCESS_SUCCESS = 0
            if hr != 0 {
                return Err(anyhow!("MAPISendMailW 返回 0x{hr:x}(用户取消或邮件客户端失败)"));
            }
            Ok(())
        }
    }

    /// IMAPI2 刻录:把 `staging` 目录构建为镜像并写入第一个光驱。
    ///
    /// 对齐原版 IMAPI2 实现(文件系统映像接口 +
    /// `DDiscFormat2EraseEvents`/`LIBID_IMAPILib2`);卷标参考原版
    /// (原版 "Picasa CD" → 我方 "Maliang CD")。
    pub fn burn_folder(staging: &Path, label: &str, client: &str) -> Result<()> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let init = hr.is_ok();
            let _guard = CoUninitGuard { do_uninit: init };

            let master: IDiscMaster2 = CoCreateInstance(&MsftDiscMaster2, None, CLSCTX_ALL)
                .map_err(|e| anyhow!("无法连接 IMAPI2(原版同款错误文案场景): {e}"))?;
            let count = master
                .Count()
                .map_err(|e| anyhow!("IMAPI2 枚举光驱失败: {e}"))?;
            if count < 1 {
                return Err(anyhow!("没有检测到光驱"));
            }
            let id = master
                .get_Item(0)
                .map_err(|e| anyhow!("读取光驱标识失败: {e}"))?;
            let recorder: IDiscRecorder2 = CoCreateInstance(&MsftDiscRecorder2, None, CLSCTX_ALL)?;
            recorder
                .InitializeDiscRecorder(&id)
                .map_err(|e| anyhow!("初始化光驱失败: {e}"))?;

            let image: IFileSystemImage =
                CoCreateInstance(&MsftFileSystemImage, None, CLSCTX_ALL)?;
            image
                .ChooseImageDefaults(&recorder)
                .map_err(|e| anyhow!("选择镜像默认值失败: {e}"))?;
            image
                .SetVolumeName(&BSTR::from(label))
                .map_err(|e| anyhow!("设置卷标失败: {e}"))?;
            image
                .Root()
                .map_err(|e| anyhow!("取根目录失败: {e}"))?
                .AddTree(&BSTR::from(staging.as_os_str().to_string_lossy().as_ref()), VARIANT_BOOL(0))
                .map_err(|e| anyhow!("添加文件失败(原版 BurnCollection::CantAddFolder 同语义): {e}"))?;
            let result = image
                .CreateResultImage()
                .map_err(|e| anyhow!("构建镜像失败: {e}"))?;
            let stream = result
                .ImageStream()
                .map_err(|e| anyhow!("取镜像流失败: {e}"))?;

            let fmt: IDiscFormat2Data = CoCreateInstance(&MsftDiscFormat2Data, None, CLSCTX_ALL)?;
            fmt.SetRecorder(&recorder)
                .map_err(|e| anyhow!("绑定光驱失败: {e}"))?;
            fmt.SetClientName(&BSTR::from(client))
                .map_err(|e| anyhow!("设置客户端名失败: {e}"))?;
            fmt.SetForceMediaToBeClosed(VARIANT_BOOL(1))
                .map_err(|e| anyhow!("设置关闭介质失败: {e}"))?;
            fmt.Write(&stream)
                .map_err(|e| anyhow!("刻录失败(检查空盘/光驱状态): {e}"))?;
            Ok(())
        }
    }

    struct CoUninitGuard {
        do_uninit: bool,
    }
    impl Drop for CoUninitGuard {
        fn drop(&mut self) {
            if self.do_uninit {
                unsafe { CoUninitialize() };
            }
        }
    }
}

#[cfg(windows)]
pub use win::{burn_folder, send_email};

#[cfg(not(windows))]
pub fn send_email(_subject: &str, _files: &[PathBuf]) -> Result<()> {
    Err(anyhow!("邮件发送仅支持 Windows(Simple MAPI)"))
}

#[cfg(not(windows))]
pub fn burn_folder(_staging: &Path, _label: &str, _client: &str) -> Result<()> {
    Err(anyhow!("刻录仅支持 Windows(IMAPI2)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_size_px_matches_original_strings() {
        // 原版 UI 文案:1600(推荐)/1024(中)/800(小)
        assert_eq!(ShareSize::Large.px(), 1600);
        assert_eq!(ShareSize::Medium.px(), 1024);
        assert_eq!(ShareSize::Small.px(), 800);
    }

    #[test]
    fn file_name_matches_original_format() {
        // 原版 `"%0*d-%s"`:序号从 1 起,零填充宽度 = 总数十进制位数
        assert_eq!(share_file_name(0, 3, "a"), "1-a");
        assert_eq!(share_file_name(1, 12, "b"), "02-b");
        assert_eq!(share_file_name(11, 120, "c"), "012-c");
    }

    #[test]
    fn stage_jpegs_resizes_and_names() {
        let dir = std::env::temp_dir().join(format!("maliang-share-{}", std::process::id()));
        let src_dir = std::env::temp_dir().join(format!("maliang-share-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&src_dir);
        std::fs::create_dir_all(&src_dir).unwrap();
        // 一张 2000x1000 的图 → Large(1600) 应缩到 1600x800
        let img = image::RgbImage::from_pixel(2000, 1000, image::Rgb([120, 120, 120]));
        let src = src_dir.join("照片 A.jpg");
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(Vec::new()), image::ImageFormat::Jpeg)
            .unwrap();
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2000, 1000, image::Rgb([120, 120, 120])))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(&src, &buf).unwrap();

        let items = vec![(src.clone(), FilterStack::new())];
        let files = stage_jpegs(&items, ShareSize::Large.px(), &dir).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].file_name().unwrap().to_string_lossy().ends_with("-照片 A.jpg"));
        let got = image::open(&files[0]).unwrap();
        assert_eq!((got.width(), got.height()), (1600, 800), "最长边应缩到 1600");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&src_dir);
    }

    #[test]
    fn stage_gift_cd_writes_manifest() {
        let dir = std::env::temp_dir().join(format!("maliang-cdstage-{}", std::process::id()));
        let src_dir = std::env::temp_dir().join(format!("maliang-cdsrc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&src_dir);
        std::fs::create_dir_all(&src_dir).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3])))
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        let src = src_dir.join("x.jpg");
        std::fs::write(&src, &buf).unwrap();
        let files = stage_gift_cd(&[(src, FilterStack::new())], 800, &dir).unwrap();
        assert_eq!(files.len(), 1);
        let manifest = std::fs::read_to_string(dir.join("manifest.txt")).unwrap();
        assert!(manifest.contains("-x.jpg"), "清单应列出暂存文件");
        assert!(!dir.join("autorun.inf").exists(), "无盘上播放器,不写 autorun.inf");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&src_dir);
    }

    #[test]
    fn stage_jpegs_rejects_empty() {
        let dir = std::env::temp_dir().join(format!("maliang-share-e-{}", std::process::id()));
        let err = stage_jpegs(&[], 800, &dir).unwrap_err();
        assert!(err.to_string().contains("IDS_NO_IMAGES_TO_SEND"));
    }
}
