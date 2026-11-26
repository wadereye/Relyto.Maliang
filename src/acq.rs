//! WIA/TWAIN 扫描器与相机导入。
//!
//! | 能力 | 实现 |
//! |---|---|
//! | WIA 设备 | `IWiaDevMgr2::GetImageDlg` — 用系统对话框选设备、扫描，落盘到目标目录 |
//! | TWAIN 枚举 | `twain_32.dll::GetProfileList`(legacy 名单,尽力而为) |
//! | TWAIN 导入 | 当前诚实标注为未实现 |
//!
//! 设计口径:所有 API 返回 `Result<Vec<PathBuf>>` 落盘路径列表,UI 只做结果展示。

use anyhow::Result;
use std::path::{Path, PathBuf};

/// 从 WIA 扫描器/相机导入照片到 `dest_dir`,返回落盘路径。
///
/// 用 `IWiaDevMgr2::GetImageDlg` 打开标准系统对话框(设备选择 + 扫描),
/// 用户确认后文件落盘,路径由 `ppbstrFilepaths` 拿回;用户取消对话框
/// 返回 Ok(空列表)——"没导入"是正常结果。
pub fn import_wia(dest_dir: &Path) -> Result<Vec<PathBuf>> {
    #[cfg(windows)]
    {
        imp::import_wia(dest_dir)
    }
    #[cfg(not(windows))]
    {
        let _ = dest_dir;
        Ok(Vec::new())
    }
}

/// Legacy TWAIN 扫描仪名单(尽力而为;有命令行的 Windows 才会有数据)。
pub fn twain_device_names() -> Vec<String> {
    #[cfg(windows)]
    {
        imp::twain_device_names()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// 诚实标注:TWAIN 导入暂未实现。
pub fn import_twain(_dest_dir: &Path) -> Result<Vec<PathBuf>> {
    Err(anyhow::anyhow!(
        "{}",
        crate::i18n::t("TWAIN 扫描仪导入暂未实现")
    ))
}

#[cfg(windows)]
mod imp {
    use anyhow::{anyhow, Result};
    use std::path::{Path, PathBuf};
    use windows::Win32::Devices::ImageAcquisition::{IWiaDevMgr2, WiaDevMgr2};
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };

    /// 为当前线程初始化并配平 COM(与 `crate::wpd` 同口径)。
    fn with_com<T>(work: impl FnOnce() -> Result<T>) -> Result<T> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if hr.is_err() && hr != RPC_E_CHANGED_MODE {
                return Err(anyhow!("COM 初始化失败"));
            }
            let initialized = hr.is_ok();
            let result = work();
            if initialized {
                CoUninitialize();
            }
            result
        }
    }

    pub fn import_wia(dest_dir: &Path) -> Result<Vec<PathBuf>> {
        with_com(|| unsafe {
            let mgr: IWiaDevMgr2 =
                CoCreateInstance(&WiaDevMgr2, None, CLSCTX_INPROC_SERVER)?;
            let mut num_files: i32 = 0;
            let mut file_paths: *mut windows::core::BSTR = std::ptr::null_mut();
            let folder = windows::core::BSTR::from(dest_dir.to_string_lossy().into_owned());
            let filename = windows::core::BSTR::new();
            mgr.GetImageDlg(
                0,
                &windows::core::BSTR::new(),
                windows::Win32::Foundation::HWND::default(),
                &folder,
                &filename,
                &mut num_files,
                &mut file_paths,
                std::ptr::null_mut(),
            )?;
            let mut out = Vec::new();
            if !file_paths.is_null() && num_files > 0 {
                for i in 0..num_files as usize {
                    let b = file_paths.add(i).read();
                    let s = b.to_string();
                    if !s.is_empty() {
                        out.push(PathBuf::from(s));
                    }
                }
            }
            Ok(out)
        })
    }

    /// Legacy TWAIN 枚举:`twain_32.dll::GetProfileList` 尽力而为列出
    /// 此 shell 环境已注册的扫描仪名单;无数据则返回空。
    pub fn twain_device_names() -> Vec<String> {
        use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
        use windows::core::PCSTR;

        type GetProfileList = unsafe extern "system" fn(*mut u32, *mut u16) -> i32;
        unsafe {
            let mod_ = LoadLibraryA(PCSTR::from_raw(b"twain_32.dll\0".as_ptr()));
            let Ok(m) = mod_ else {
                return Vec::new();
            };
            let Some(proc) = GetProcAddress(m, PCSTR::from_raw(b"GetProfileList\0".as_ptr())) else {
                return Vec::new();
            };
            let f: GetProfileList = core::mem::transmute(proc);
            let mut count = 0u32;
            let mut buf = [0u16; 1024];
            if f(&mut count, buf.as_mut_ptr()) == 0 {
                return Vec::new();
            }
            // buf 内 `\0` 分隔的设备名;用连续两个 0 结束。
            let mut out = Vec::new();
            let mut cur = String::new();
            let mut zeros = 0;
            for &c in buf.iter() {
                if c == 0 {
                    zeros += 1;
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                    if zeros >= 2 {
                        break;
                    }
                } else {
                    zeros = 0;
                    if out.len() >= 32 {
                        break;
                    }
                    cur.push(char::from_u32(c as u32).unwrap_or('?'));
                }
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twain_enum_does_not_panic() {
        // 无设备或有设备,都应返回 Vec
        let _ = twain_device_names();
    }

    #[test]
    #[cfg(windows)]
    fn import_twain_is_honest_not_implemented() {
        let r = import_twain(std::path::Path::new("."));
        assert!(r.is_err(), "TWAIN 导入当前应诚实返回错误");
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn import_wia_stub_ok_or_err() {
        let _r = import_wia(std::path::Path::new("."));
    }
}
