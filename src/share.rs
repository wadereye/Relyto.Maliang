//! 分享链路:邮件(Simple MAPI)与礼品光盘(IMAPI2)。
//!
//! 分享实现:
//! - **邮件 = Simple MAPI**:`LoadLibraryA("MAPI32.DLL")` + `GetProcAddress("MAPISendMail")`,
//!   发送 flags=9(`MAPI_LOGON_UI|MAPI_DIALOG`),附件暂存 `temp\email\`,
//!   文件名格式 `"%0*d-%s"`(序号零填充)。
//! - **尺寸选项**:1600/1024/800。
//! - **刻录 = IMAPI2**:走 IMAPI2 COM 文件系统映像接口,
//!   JPEG 质量 85,并提供盘上清单导出。
//!
//! 实现口径说明:
//! 1. 邮件用同 DLL 的 `MAPISendMailW`(Vista+ 标准导出),
//!    否则中文路径在 ANSI 代码页会丢字。
//! 2. 礼品光盘**不写 autorun.inf**,只写照片 + `manifest.txt`
//!    盘上清单文件。
//! 3. 刻录只用 IMAPI2。

use crate::effects::FilterStack;
use crate::i18n::{t, t_with};
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

/// 收件人清洗:去空白、去空项、必须含 `@`、大小写不敏感去重并保序。
#[must_use]
pub fn normalize_recipients(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for item in raw {
        let addr = item.trim();
        if addr.is_empty() || !addr.contains('@') {
            continue;
        }
        let key = addr.to_ascii_lowercase();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(addr.to_string());
    }
    out
}

/// 邮件/光盘尺寸档(8 档列表
/// `160|320|480|640|800|1024|1200|1600`;默认 1024 像素)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareSize {
    Px160,
    Px320,
    Px480,
    Px640,
    Px800,
    Px1024,
    Px1200,
    Px1600,
}

impl ShareSize {
    /// 最长边像素(尺寸档列表)。
    #[must_use]
    pub fn px(self) -> u32 {
        match self {
            ShareSize::Px160 => 160,
            ShareSize::Px320 => 320,
            ShareSize::Px480 => 480,
            ShareSize::Px640 => 640,
            ShareSize::Px800 => 800,
            ShareSize::Px1024 => 1024,
            ShareSize::Px1200 => 1200,
            ShareSize::Px1600 => 1600,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ShareSize::Px160 => t("160 像素(缩略图)"),
            ShareSize::Px320 => t("320 像素(网页小图)"),
            ShareSize::Px480 => t("480 像素(小图)"),
            ShareSize::Px640 => t("640 像素(普通)"),
            ShareSize::Px800 => t("800 像素(博客/网页)"),
            ShareSize::Px1024 => t("1024 像素(推荐)"),
            ShareSize::Px1200 => t("1200 像素(大图)"),
            ShareSize::Px1600 => t("1600 像素(打印/分享)"),
        }
    }

    pub const ALL: [ShareSize; 8] = [
        ShareSize::Px160,
        ShareSize::Px320,
        ShareSize::Px480,
        ShareSize::Px640,
        ShareSize::Px800,
        ShareSize::Px1024,
        ShareSize::Px1200,
        ShareSize::Px1600,
    ];

    /// 按像素值取档(选项持久化用;未知值回退默认档 1024)。
    #[must_use]
    pub fn from_px(px: u32) -> ShareSize {
        Self::ALL
            .into_iter()
            .find(|s| s.px() == px)
            .unwrap_or(ShareSize::Px1024)
    }
}

/// 附件文件名格式 `"%0*d-%s"`:
/// 零填充序号(宽度 = 总数的十进制位数)-原文件名。
#[must_use]
pub fn share_file_name(index: usize, total: usize, stem: &str) -> String {
    let width = total.to_string().len().max(1);
    format!("{:0width$}-{stem}", index + 1, width = width)
}

/// JPEG 编码质量(刻录输出与邮件暂存共用同一常量)。
pub const SHARE_JPEG_QUALITY: u8 = 85;

/// 把条目(原图 + 可选效果栈)重采样到 `max_px` 内并编码为 JPEG 写入 `dir`。
/// 返回暂存文件列表(顺序与条目一致)。
pub fn stage_jpegs(
    items: &[(PathBuf, FilterStack)],
    max_px: u32,
    dir: &Path,
) -> Result<Vec<PathBuf>> {
    if items.is_empty() {
        return Err(anyhow!(t("没有可分享的照片")));
    }
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).with_context(|| {
        t_with(
            "创建暂存目录失败: {p1}",
            &[("p1", (dir.display()).to_string())],
        )
    })?;
    let mut out = Vec::with_capacity(items.len());
    for (i, (src, stack)) in items.iter().enumerate() {
        let img = image::open(src)
            .with_context(|| t_with("解码失败: {p1}", &[("p1", (src.display()).to_string())]))?;
        // 与预览同一坐标空间:先按 EXIF 方向摆正再套效果栈(含几何类效果)。
        let img = crate::thumbs::apply_exif_rotation(img, crate::exif::read_orientation(src));
        let mut rgba = img.to_rgba8();
        if !stack.is_empty() {
            crate::effects::apply_stack(&mut rgba, stack);
        }
        let flat = image::DynamicImage::ImageRgba8(rgba);
        // 重采样
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

/// 礼品光盘内容暂存:照片 + `manifest.txt` 清单。
/// **不写 autorun.inf**(无盘上播放器)。
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

    /// Simple MAPI 收件人描述(MapiRecipDescW,字段顺序与宽度按 SDK 声明)。
    #[repr(C)]
    struct MapiRecipDescW {
        ul_reserved: u32,
        ul_recip_class: u32,
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
    /// `mapi32.dll` 动态加载(MAPI 动态加载)+
    /// 发送 flags=9(邮件发送,`MAPI_LOGON_UI|MAPI_DIALOG`)。
    /// 用 Unicode 导出 `MAPISendMailW`(中文路径不丢字)。
    pub fn send_email(subject: &str, files: &[PathBuf], to: &[String]) -> Result<()> {
        if files.is_empty() {
            return Err(anyhow!(t("没有附件")));
        }
        let addrs = super::normalize_recipients(to);
        unsafe {
            // mapi32.dll 句柄不 FreeLibrary,进程退出由系统回收。
            let lib = windows::Win32::System::LibraryLoader::LoadLibraryW(w!("MAPI32.DLL"))
                .map_err(|e| {
                    anyhow!(
                        "{}",
                        t_with("加载 mapi32.dll 失败: {e}", &[("e", e.to_string())])
                    )
                })?;
            let proc = windows::Win32::System::LibraryLoader::GetProcAddress(
                lib,
                PCSTR::from_raw(b"MAPISendMailW\0".as_ptr()),
            );
            let Some(proc) = proc else {
                return Err(anyhow!(t("mapi32.dll 无 MAPISendMailW(系统过旧)")));
            };
            let send: SendMailWFn = std::mem::transmute(proc);

            // 字符串缓冲必须活到调用结束
            let wide =
                |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
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
            // 收件人按 MAPI_TO 填写,地址缓冲同样活到调用结束
            let mut addr_bufs: Vec<Vec<u16>> = addrs.iter().map(|a| wide(a)).collect();
            let mut recips: Vec<MapiRecipDescW> = addr_bufs
                .iter_mut()
                .map(|buf| MapiRecipDescW {
                    ul_reserved: 0,
                    ul_recip_class: 1,
                    lpsz_name: PWSTR::null(),
                    lpsz_address: PWSTR(buf.as_mut_ptr()),
                    ul_eid_size: 0,
                    lp_entry_id: std::ptr::null_mut(),
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
                n_recip_count: recips.len() as u32,
                lp_recips: if recips.is_empty() {
                    std::ptr::null_mut()
                } else {
                    recips.as_mut_ptr()
                },
                n_file_count: files.len() as u32,
                lp_files: file_descs.as_mut_ptr(),
            };
            // flags = 9 = MAPI_LOGON_UI | MAPI_DIALOG(邮件发送)
            let hr = send(0, 0, &mut msg, 9, 0);
            // SUCCESS_SUCCESS = 0
            if hr != 0 {
                return Err(anyhow!(
                    "{}",
                    t_with(
                        "MAPISendMailW 返回 0x{hr}(用户取消或邮件客户端失败)",
                        &[("hr", format!("{hr:x}", hr = hr))]
                    )
                ));
            }
            Ok(())
        }
    }

    /// IMAPI2 刻录:把 `staging` 目录构建为镜像并写入第一个光驱。
    ///
    /// 走 IMAPI2 文件系统映像接口,卷标与客户端名由调用方传入。
    pub fn burn_folder(staging: &Path, label: &str, client: &str) -> Result<()> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let init = hr.is_ok();
            let _guard = CoUninitGuard { do_uninit: init };

            let master: IDiscMaster2 = CoCreateInstance(&MsftDiscMaster2, None, CLSCTX_ALL)
                .map_err(|e| {
                    anyhow!(
                        "{}",
                        t_with("无法连接 IMAPI2: {e}", &[("e", e.to_string())])
                    )
                })?;
            let count = master.Count().map_err(|e| {
                anyhow!(
                    "{}",
                    t_with("IMAPI2 枚举光驱失败: {e}", &[("e", e.to_string())])
                )
            })?;
            if count < 1 {
                return Err(anyhow!(t("没有检测到光驱")));
            }
            let id = master.get_Item(0).map_err(|e| {
                anyhow!(
                    "{}",
                    t_with("读取光驱标识失败: {e}", &[("e", e.to_string())])
                )
            })?;
            let recorder: IDiscRecorder2 = CoCreateInstance(&MsftDiscRecorder2, None, CLSCTX_ALL)?;
            recorder.InitializeDiscRecorder(&id).map_err(|e| {
                anyhow!("{}", t_with("初始化光驱失败: {e}", &[("e", e.to_string())]))
            })?;

            let image: IFileSystemImage = CoCreateInstance(&MsftFileSystemImage, None, CLSCTX_ALL)?;
            image.ChooseImageDefaults(&recorder).map_err(|e| {
                anyhow!(
                    "{}",
                    t_with("选择镜像默认值失败: {e}", &[("e", e.to_string())])
                )
            })?;
            image
                .SetVolumeName(&BSTR::from(label))
                .map_err(|e| anyhow!("{}", t_with("设置卷标失败: {e}", &[("e", e.to_string())])))?;
            image
                .Root()
                .map_err(|e| anyhow!("{}", t_with("取根目录失败: {e}", &[("e", e.to_string())])))?
                .AddTree(
                    &BSTR::from(staging.as_os_str().to_string_lossy().as_ref()),
                    VARIANT_BOOL(0),
                )
                .map_err(|e| anyhow!("{}", t_with("添加文件失败: {e}", &[("e", e.to_string())])))?;
            let result = image
                .CreateResultImage()
                .map_err(|e| anyhow!("{}", t_with("构建镜像失败: {e}", &[("e", e.to_string())])))?;
            let stream = result
                .ImageStream()
                .map_err(|e| anyhow!("{}", t_with("取镜像流失败: {e}", &[("e", e.to_string())])))?;

            let fmt: IDiscFormat2Data = CoCreateInstance(&MsftDiscFormat2Data, None, CLSCTX_ALL)?;
            fmt.SetRecorder(&recorder)
                .map_err(|e| anyhow!("{}", t_with("绑定光驱失败: {e}", &[("e", e.to_string())])))?;
            fmt.SetClientName(&BSTR::from(client)).map_err(|e| {
                anyhow!(
                    "{}",
                    t_with("设置客户端名失败: {e}", &[("e", e.to_string())])
                )
            })?;
            fmt.SetForceMediaToBeClosed(VARIANT_BOOL(1)).map_err(|e| {
                anyhow!(
                    "{}",
                    t_with("设置关闭介质失败: {e}", &[("e", e.to_string())])
                )
            })?;
            fmt.Write(&stream).map_err(|e| {
                anyhow!(
                    "{}",
                    t_with("刻录失败(检查空盘/光驱状态): {e}", &[("e", e.to_string())])
                )
            })?;
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

/// 按固定优先级选择可用的邮件客户端。
#[cfg(any(not(windows), test))]
pub fn pick_launcher(lookup: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    ["xdg-email", "thunderbird", "geary"]
        .into_iter()
        .find(|name| lookup(name))
}

/// 按固定优先级选择可用的光盘写入工具。
#[cfg(any(not(windows), test))]
pub fn pick_burn_tool(lookup: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    ["growisofs", "wodim", "xorriso"]
        .into_iter()
        .find(|name| lookup(name))
}

#[cfg(any(not(windows), test))]
fn percent_encode(value: &str, preserve_slash: bool) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'@')
            || (preserve_slash && *byte == b'/')
        {
            out.push(char::from(*byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(any(not(windows), test))]
fn mailto_uri(to: &[String], subject: &str) -> String {
    let mut uri = format!("mailto:{}", to.join(",").replace('\'', "%27"));
    if !subject.is_empty() {
        uri.push_str("?subject=");
        uri.push_str(&percent_encode(subject, false));
    }
    uri
}

/// 生成邮件客户端参数,保留收件人、主题与附件。
#[cfg(any(not(windows), test))]
pub fn build_mail_args(
    cmd: &str,
    to: &[String],
    subject: &str,
    files: &[PathBuf],
) -> Vec<String> {
    match cmd {
        "xdg-email" => {
            let mut args = vec!["--utf8".to_string(), "--subject".to_string(), subject.to_string()];
            for file in files {
                args.push("--attach".to_string());
                args.push(file.to_string_lossy().into_owned());
            }
            if !to.is_empty() {
                args.push("--".to_string());
                args.extend(to.iter().cloned());
            }
            args
        }
        "thunderbird" if files.is_empty() => vec![mailto_uri(to, subject)],
        "thunderbird" => {
            let mut compose = format!(
                "to='{}',subject='{}'",
                to.join(",").replace('\'', "%27"),
                percent_encode(subject, false)
            );
            if !files.is_empty() {
                let attachments = files
                    .iter()
                    .map(|file| {
                        format!(
                            "file://{}",
                            percent_encode(&file.to_string_lossy(), true)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                compose.push_str(&format!(",attachment='{attachments}'"));
            }
            vec!["-compose".to_string(), compose]
        }
        "geary" => vec![mailto_uri(to, subject)],
        _ => Vec::new(),
    }
}

#[cfg(any(not(windows), test))]
fn ensure_mail_attachments_supported(cmd: &str, files: &[PathBuf]) -> Result<()> {
    if cmd == "geary" && !files.is_empty() {
        return Err(anyhow!(t("Geary 不支持命令行附件,请改用其他邮件客户端")));
    }
    Ok(())
}

#[cfg(any(not(windows), test))]
fn ensure_burn_source_supported(tool: &str, iso_or_dir: &Path) -> Result<()> {
    if tool == "wodim" && iso_or_dir.is_dir() {
        return Err(anyhow!(t(
            "wodim 只能写入 ISO 镜像,请安装 growisofs 或 xorriso"
        )));
    }
    Ok(())
}

/// 生成光盘写入工具参数,目录由支持该模式的工具直接写入。
#[cfg(any(not(windows), test))]
pub fn build_burn_args(tool: &str, iso_or_dir: &Path, device: &str) -> Vec<String> {
    let path = iso_or_dir.to_string_lossy();
    match tool {
        "growisofs" if iso_or_dir.is_dir() => vec![
            "-Z".to_string(),
            device.to_string(),
            "-R".to_string(),
            "-J".to_string(),
            path.into_owned(),
        ],
        "growisofs" => vec!["-Z".to_string(), format!("{device}={path}")],
        "wodim" => vec![
            "-v".to_string(),
            format!("dev={device}"),
            path.into_owned(),
        ],
        "xorriso" if iso_or_dir.is_dir() => vec![
            "-outdev".to_string(),
            device.to_string(),
            "-blank".to_string(),
            "as_needed".to_string(),
            "-map".to_string(),
            path.into_owned(),
            "/".to_string(),
            "-commit".to_string(),
        ],
        "xorriso" => vec![
            "-as".to_string(),
            "cdrecord".to_string(),
            "-v".to_string(),
            format!("dev={device}"),
            "blank=as_needed".to_string(),
            path.into_owned(),
        ],
        _ => Vec::new(),
    }
}

#[cfg(windows)]
pub use win::{burn_folder, send_email};

#[cfg(not(windows))]
fn which_in_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
        })
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn send_email(subject: &str, files: &[PathBuf], to: &[String]) -> Result<()> {
    if files.is_empty() {
        return Err(anyhow!(t("没有附件")));
    }
    let addrs = normalize_recipients(to);
    let Some(cmd) = pick_launcher(&which_in_path) else {
        return Err(anyhow!(t(
            "未找到邮件客户端(需 xdg-email / thunderbird / geary)"
        )));
    };
    ensure_mail_attachments_supported(cmd, files)?;
    std::process::Command::new(cmd)
        .args(build_mail_args(cmd, &addrs, subject, files))
        .spawn()
        .map(|_| ())
        .map_err(|e| {
            anyhow!(
                "{}",
                t_with(
                    "启动邮件客户端失败:{cmd}: {e}",
                    &[("cmd", cmd.to_string()), ("e", e.to_string())]
                )
            )
        })
}

#[cfg(not(windows))]
pub fn burn_folder(staging: &Path, _label: &str, _client: &str) -> Result<()> {
    let Some(tool) = pick_burn_tool(&which_in_path) else {
        return Err(anyhow!(t(
            "未找到光盘写入工具(growisofs / wodim / xorriso)"
        )));
    };
    ensure_burn_source_supported(tool, staging)?;
    let device = std::env::var_os("CDR_DEVICE")
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/dev/sr0".to_string());
    std::process::Command::new(tool)
        .args(build_burn_args(tool, staging, &device))
        .spawn()
        .map(|_| ())
        .map_err(|e| {
            anyhow!(
                "{}",
                t_with(
                    "启动光盘写入工具失败:{cmd}: {e}",
                    &[("cmd", tool.to_string()), ("e", e.to_string())]
                )
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_candidates_are_ordered_and_arg_sets_differ() {
        assert_eq!(pick_launcher(&|n| n == "thunderbird"), Some("thunderbird"));
        assert_eq!(pick_launcher(&|_| false), None);
        let a = build_mail_args(
            "xdg-email",
            &["x@y.z".into()],
            "副 本",
            &[PathBuf::from("/t/1.jpg")],
        );
        assert_eq!(
            a,
            vec![
                "--utf8",
                "--subject",
                "副 本",
                "--attach",
                "/t/1.jpg",
                "--",
                "x@y.z",
            ]
        );
        let b = build_mail_args("thunderbird", &["x@y.z".into()], "副本", &[]);
        assert!(b.iter().any(|s| s == "mailto:x@y.z?subject=%E5%89%AF%E6%9C%AC"));
    }

    #[test]
    fn thunderbird_compose_keeps_attachments_and_geary_reports_limit() {
        let args = build_mail_args(
            "thunderbird",
            &["x@y.z".into(), "a@b.c".into()],
            "副 本",
            &[PathBuf::from("/t/1.jpg"), PathBuf::from("/t/2.jpg")],
        );
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "-compose");
        assert_eq!(
            args[1],
            "to='x@y.z,a@b.c',subject='%E5%89%AF%20%E6%9C%AC',attachment='file:///t/1.jpg,file:///t/2.jpg'"
        );
        let err = ensure_mail_attachments_supported("geary", &[PathBuf::from("/t/1.jpg")])
            .unwrap_err();
        assert!(err.to_string().contains("Geary"));
        assert!(ensure_mail_attachments_supported("geary", &[]).is_ok());
    }

    #[test]
    fn burn_tool_prefers_growisofs_and_builds_documented_args() {
        assert_eq!(
            pick_burn_tool(&|n| n == "wodim" || n == "growisofs"),
            Some("growisofs")
        );
        assert_eq!(pick_burn_tool(&|_| false), None);
        assert_eq!(
            build_burn_args("growisofs", Path::new("."), "/dev/sr0"),
            vec!["-Z", "/dev/sr0", "-R", "-J", "."]
        );
        assert_eq!(
            build_burn_args("growisofs", Path::new("/t/image.iso"), "/dev/sr0"),
            vec!["-Z", "/dev/sr0=/t/image.iso"]
        );
        assert_eq!(
            build_burn_args("wodim", Path::new("/t/image.iso"), "/dev/dvd"),
            vec!["-v", "dev=/dev/dvd", "/t/image.iso"]
        );
        let err = ensure_burn_source_supported("wodim", Path::new(".")).unwrap_err();
        assert!(err.to_string().contains("ISO"));
        assert!(ensure_burn_source_supported("growisofs", Path::new(".")).is_ok());
        assert!(ensure_burn_source_supported("xorriso", Path::new(".")).is_ok());
        assert_eq!(
            build_burn_args("xorriso", Path::new("/t/image.iso"), "/dev/cdrom"),
            vec![
                "-as",
                "cdrecord",
                "-v",
                "dev=/dev/cdrom",
                "blank=as_needed",
                "/t/image.iso",
            ]
        );
        assert_eq!(
            build_burn_args("xorriso", Path::new("."), "/dev/cdrom"),
            vec![
                "-outdev",
                "/dev/cdrom",
                "-blank",
                "as_needed",
                "-map",
                ".",
                "/",
                "-commit",
            ]
        );
    }

    #[test]
    fn recipients_are_trimmed_deduped_and_validated() {
        let raw = [
            "  A@X.COM ".to_string(),
            "".to_string(),
            "   ".to_string(),
            "no-at-sign".to_string(),
            "a@x.com".to_string(),
            "b@y.com".to_string(),
        ];
        assert_eq!(
            normalize_recipients(&raw),
            vec!["A@X.COM".to_string(), "b@y.com".to_string()],
            "去空白、缺 @ 丢弃、大小写去重且保序"
        );
        assert!(normalize_recipients(&[]).is_empty());
    }

    #[test]
    fn share_size_px_matches_original_strings() {
        // 尺寸档列表 160|320|480|640|800|1024|1200|1600,
        // 默认档 1024。
        for px in [160, 320, 480, 640, 800, 1024, 1200, 1600] {
            assert_eq!(ShareSize::from_px(px).px(), px);
        }
        assert_eq!(ShareSize::from_px(999), ShareSize::Px1024);
    }

    #[test]
    fn file_name_matches_original_format() {
        // `"%0*d-%s"`:序号从 1 起,零填充宽度 = 总数十进制位数
        assert_eq!(share_file_name(0, 3, "a"), "1-a");
        assert_eq!(share_file_name(1, 12, "b"), "02-b");
        assert_eq!(share_file_name(11, 120, "c"), "012-c");
    }

    #[test]
    fn stage_jpegs_resizes_and_names() {
        let dir = std::env::temp_dir().join(format!("maliang-share-{}", std::process::id()));
        let src_dir =
            std::env::temp_dir().join(format!("maliang-share-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&src_dir);
        std::fs::create_dir_all(&src_dir).unwrap();
        // 一张 2000x1000 的图 → Large(1600) 应缩到 1600x800
        let img = image::RgbImage::from_pixel(2000, 1000, image::Rgb([120, 120, 120]));
        let src = src_dir.join("照片 A.jpg");
        image::DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(Vec::new()),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        let mut buf: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2000,
            1000,
            image::Rgb([120, 120, 120]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut buf),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
        std::fs::write(&src, &buf).unwrap();

        let items = vec![(src.clone(), FilterStack::new())];
        let files = stage_jpegs(&items, ShareSize::Px1600.px(), &dir).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-照片 A.jpg"));
        let got = image::open(&files[0]).unwrap();
        assert_eq!(
            (got.width(), got.height()),
            (1600, 800),
            "最长边应缩到 1600"
        );
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
            .write_to(
                &mut std::io::Cursor::new(&mut buf),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        let src = src_dir.join("x.jpg");
        std::fs::write(&src, &buf).unwrap();
        let files = stage_gift_cd(&[(src, FilterStack::new())], 800, &dir).unwrap();
        assert_eq!(files.len(), 1);
        let manifest = std::fs::read_to_string(dir.join("manifest.txt")).unwrap();
        assert!(manifest.contains("-x.jpg"), "清单应列出暂存文件");
        assert!(
            !dir.join("autorun.inf").exists(),
            "无盘上播放器,不写 autorun.inf"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&src_dir);
    }

    #[test]
    fn stage_jpegs_rejects_empty() {
        let dir = std::env::temp_dir().join(format!("maliang-share-e-{}", std::process::id()));
        let err = stage_jpegs(&[], 800, &dir).unwrap_err();
        assert!(err.to_string().contains("没有可分享的照片"));
    }
}
