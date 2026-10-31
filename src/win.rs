//! Windows 平台能力封装(卷类型分流 / 线程优先级 / Shell 缩略图 / 控制台挂载)。
//!
//! 对应原版 Picasa 3 的三处参考结论：
//! - 卷类型分流用 `GetLogicalDriveStringsA` + `GetDriveTypeA` 分流卷类型,
//!   **网络盘(DRIVE_REMOTE)完全不扫**;固定盘按 `GetVolumeInformationA` 读文件系统,
//!   FAT/FAT32/NTFS 之外的(KODAK 卷等)才收录。
//! - 线程优先级调整 / UI 线程优先级动态 `SetThreadPriority`:扫描线程降优先级、UI 线程提优先级。
//! - Shell 缩略图走 COM `IThumbnailProvider` / `IShellItemImageFactory`,
//!   借 Windows Shell 扩展解 RAW(`.NEF/.CR2/.ARW/.RAF` 等本项目 image crate 解不了的格式)。
//!
//! 非 Windows 平台全部退化为无操作 / 返回 None,保证跨平台可编译。
#![allow(dead_code)]

use std::path::Path;

/// 卷类型(与 Win32 `GetDriveTypeA` 返回值一致)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveType {
    Unknown,
    NoRootDir,
    Removable,
    Fixed,
    Remote,
    CdRom,
    RamDisk,
}

impl DriveType {
    /// 原版卷类型分流的实际策略:
    /// - DRIVE_REMOTE(网络盘)→ **不收录**
    /// - DRIVE_CDROM / DRIVE_NO_ROOT_DIR / DRIVE_UNKNOWN → 不收录
    /// - DRIVE_REMOVABLE(移动硬盘/U盘/相机卡)→ 收录
    /// - DRIVE_FIXED(固定盘)→ 收录
    pub fn scannable(self) -> bool {
        matches!(self, DriveType::Removable | DriveType::Fixed | DriveType::RamDisk)
    }
}

/// 取盘符根目录的卷类型。非 Windows 或盘不存在返回 `None`。
pub fn drive_type(root: &Path) -> Option<DriveType> {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::GetDriveTypeW;
        let root = root.to_string_lossy().into_owned();
        if !root.ends_with('\\') && !root.ends_with('/') {
            return None;
        }
        let wide = to_wide(&root);
        // SAFETY: wide 是 NUL 结尾的 UTF-16 串,GetDriveTypeW 只读不写。
        let t = unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) };
        Some(match t {
            0 => DriveType::Unknown,
            1 => DriveType::NoRootDir,
            2 => DriveType::Removable,
            3 => DriveType::Fixed,
            4 => DriveType::Remote,
            5 => DriveType::CdRom,
            6 => DriveType::RamDisk,
            _ => DriveType::Unknown,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        None
    }
}

/// 取盘符根目录的卷标(资源管理器里显示的名字)。
/// P1-13:WPD/盘符统一设备列表用它显示「E:\ — 我的SD卡」。
/// 取不到或为空返回 `None`。
pub fn volume_label(root: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
        let root = root.to_string_lossy().into_owned();
        let wide = to_wide(&root);
        let mut name_buf = [0u16; 128];
        // SAFETY: name_buf 是 NUL 结尾的定长 UTF-16 数组,长度与缓冲区一致;
        // 其余出参传 None 表示不需要。
        let ok = unsafe {
            GetVolumeInformationW(
                PCWSTR(wide.as_ptr()),
                Some(&mut name_buf),
                None,
                None,
                None,
                None,
            )
        };
        if ok.is_err() {
            return None;
        }
        let label = from_wide(&name_buf);
        if label.is_empty() {
            None
        } else {
            Some(label)
        }
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        None
    }
}

/// 固定盘的文件系统名(如 "NTFS" / "exFAT")。原版用它排除 FAT/FAT32。
/// 取不到返回 `None`。
pub fn volume_filesystem(root: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
        let root = root.to_string_lossy().into_owned();
        let wide = to_wide(&root);
        let mut fs_buf = [0u16; 64];
        // SAFETY: fs_buf 是 NUL 结尾的定长 UTF-16 数组,长度与缓冲区一致;
        // 其余出参传 None 表示不需要。
        let ok = unsafe {
            GetVolumeInformationW(
                PCWSTR(wide.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut fs_buf),
            )
        };
        if ok.is_err() {
            return None;
        }
        let fs = from_wide(&fs_buf);
        if fs.is_empty() {
            None
        } else {
            Some(fs)
        }
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        None
    }
}

/// 线程优先级档位(对应 Win32 `THREAD_PRIORITY_*`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    AboveNormal,
    Normal,
    BelowNormal,
    Idle,
}

/// 设置**当前线程**的优先级。动态调优先级的约定:
/// 扫描线程用 `BelowNormal`(不抢 UI 核),UI 线程用 `AboveNormal`。
/// 非 Windows 为无操作。
pub fn set_current_thread_priority(p: Priority) {
    #[cfg(windows)]
    {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY, THREAD_PRIORITY_ABOVE_NORMAL,
            THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_IDLE, THREAD_PRIORITY_NORMAL,
        };
        let prio: THREAD_PRIORITY = match p {
            Priority::AboveNormal => THREAD_PRIORITY_ABOVE_NORMAL,
            Priority::Normal => THREAD_PRIORITY_NORMAL,
            Priority::BelowNormal => THREAD_PRIORITY_BELOW_NORMAL,
            Priority::Idle => THREAD_PRIORITY_IDLE,
        };
        // SAFETY: GetCurrentThread 返回伪句柄,SetThreadPriority 对其合法。
        unsafe {
            let _ = SetThreadPriority(GetCurrentThread(), prio);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = p;
    }
}

/// 把标准输出/错误挂回**父进程的控制台**(GUI 子系统 exe 的命令行子命令专用)。
///
/// 背景:release 二进制按 GUI 子系统链接(双击启动不弹黑框),此时标准句柄
/// 不指向任何终端,`println!` 的输出会消失。从终端启动并携带命令行参数时,
/// 先调本函数:挂上父进程(终端)的控制台,再把 stdout/stderr 重定向到
/// `CONOUT$`,子命令的输出就能回到调用方终端。从资源管理器双击、或本来就
/// 有控制台(debug 构建)时挂载失败,静默跳过,无副作用。
pub fn attach_parent_console() {
    #[cfg(windows)]
    {
        use windows::core::w;
        use windows::Win32::Foundation::GENERIC_WRITE;
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        use windows::Win32::System::Console::{
            AttachConsole, GetStdHandle, SetConsoleOutputCP, SetStdHandle, ATTACH_PARENT_PROCESS,
            STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
        };

        // SAFETY: 只带一个常量参数;失败仅意味着没有可挂的父控制台。
        if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_err() {
            return;
        }
        // 挂载成功后把控制台输出代码页切到 UTF-8(65001 = CP_UTF8),
        // println! 的中文才能在 936 代码页的 conhost 里正常显示。
        unsafe {
            let _ = SetConsoleOutputCP(65001);
        }
        for slot in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // 已有合法句柄(用户重定向了 > file 或管道)时不覆盖,保留重定向语义;
            // GUI 子系统普通启动时标准句柄为空,才挂到 CONOUT$ 屏幕缓冲。
            let cur = unsafe { GetStdHandle(slot) };
            if let Ok(h) = &cur {
                if !h.is_invalid() {
                    continue;
                }
            }
            // SAFETY: CONOUT$ 是系统约定的控制台输出设备名;
            // SetStdHandle 替换进程级标准句柄,替换后的句柄由进程持有到退出。
            if let Ok(h) = unsafe {
                CreateFileW(
                    w!("CONOUT$"),
                    GENERIC_WRITE.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    Default::default(),
                    None,
                )
            } {
                unsafe {
                    let _ = SetStdHandle(slot, h);
                }
            }
        }
    }
    #[cfg(not(windows))]
    {}
}

/// 取系统逻辑 CPU 数(用于扫描线程数上限)。
pub fn cpu_count() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

/// 取用户壳目录(我的文档 / 图片 / 桌面),按存在性过滤。
///
/// 对应参考件首启默认扫描范围 —— 安装文案原文:
/// "Only search My Documents, My Pictures, and the Desktop"。
/// 参考件首启只登记这三个壳目录,绝不扫盘根;扩大范围由用户手动添加。
pub fn known_dirs() -> Vec<std::path::PathBuf> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Com::CoTaskMemFree;
        use windows::Win32::UI::Shell::{
            FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Pictures, KNOWN_FOLDER_FLAG,
            SHGetKnownFolderPath,
        };
        let mut v = Vec::new();
        for fid in [&FOLDERID_Documents, &FOLDERID_Pictures, &FOLDERID_Desktop] {
            // SAFETY: rfid 指向静态常量 GUID;htoken 传 NULL 表示当前用户。
            if let Ok(pw) =
                unsafe { SHGetKnownFolderPath(fid, KNOWN_FOLDER_FLAG(0), HANDLE(std::ptr::null_mut())) }
            {
                let s = unsafe { pw.to_string() }.unwrap_or_default();
                unsafe { CoTaskMemFree(Some(pw.as_ptr() as _)) };
                if !s.is_empty() {
                    let p = std::path::PathBuf::from(s);
                    if p.is_dir() {
                        v.push(p);
                    }
                }
            }
        }
        v
    }
    #[cfg(not(windows))]
    {
        let mut v = Vec::new();
        if let Some(h) = dirs::home_dir() {
            for n in ["Documents", "Pictures", "Desktop"] {
                let p = h.join(n);
                if p.is_dir() {
                    v.push(p);
                }
            }
        }
        v
    }
}


/// 用系统关联的编辑器打开文件(参考件「Open File(s) in an Editor」)。
///
/// ShellExecuteW 的 `edit` 动词:文件类型关联了编辑器时用编辑器,
/// 未关联时退回 `open`(用默认程序打开)。
pub fn shell_edit(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let wide = to_wide(&path.to_string_lossy().into_owned());
        // SAFETY: 参数均为合法 NUL 结尾宽字符串;动词为静态常量。
        let ret = unsafe {
            ShellExecuteW(
                None,
                windows::core::w!("edit"),
                PCWSTR(wide.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if (ret.0 as isize) > 32 {
            return true;
        }
        // 未关联 edit 动词 → 退回默认程序。
        shell_open(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// 把图片文件设为桌面壁纸(参考件「Set as Desktop」)。
///
/// `SystemParametersInfoW(SPI_SETDESKWALLPAPER)`:路径必须是位图文件
/// (JPG 现代系统可用);调用方负责先把当前照片渲染成 JPEG 落盘。
pub fn set_wallpaper(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            SystemParametersInfoW, SPI_SETDESKWALLPAPER, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE,
        };

        let wide = to_wide(&path.to_string_lossy().into_owned());
        // SAFETY: pvParam 指向 NUL 结尾宽字符串;SPIF 组合是标准写法。
        unsafe {
            SystemParametersInfoW(
                SPI_SETDESKWALLPAPER,
                0,
                Some(wide.as_ptr() as *mut core::ffi::c_void),
                SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
            )
        }
        .is_ok()
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// 用系统关联程序打开文件。
///
/// P2-17 视频播放的**等效替代**:原版在库内用 DirectShow filter graph
/// 渲染到一个视频子窗口,我方不内嵌视频管线,改为交给系统默认播放器 ——
/// 与原版「能看」的目标一致,管线体量差异在 PROGRESS.md 十八节诚实标注。
///
/// 返回是否成功(ShellExecuteW 返回值 > 32 视为成功)。
pub fn shell_open(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let wide = to_wide(&path.to_string_lossy().into_owned());
        // SAFETY: 参数均为合法 NUL 结尾宽字符串;动词 "open" 为静态常量。
        let ret = unsafe {
            ShellExecuteW(
                None,
                windows::core::w!("open"),
                PCWSTR(wide.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        // ShellExecuteW 约定:返回值 > 32 表示成功;<= 32 是 SE_ERR_* 错误码。
        (ret.0 as isize) > 32
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// 通过 Windows Shell 取缩略图(借系统已安装的相机 RAW 解码扩展)。
///
/// 走 Windows Shell 的 COM 路线:`SHCreateItemFromParsingName` →
/// `IShellItemImageFactory::GetImage` → `HBITMAP` → `GetDIBits` 取 BGRA 像素。
/// 原版走 `IThumbnailProvider` 的 `BindToHandler` 变体,此处用
/// `IShellItemImageFactory`(等价能力,接口更直接)。
///
/// 返回 `(BGRA 像素缓冲, 宽, 高)`(与 GDI `BITMAPINFO` 的 32bpp 一致)。
/// 非 Windows、Shell 未就绪、或该格式无缩略图提供程序时返回 `None`。
pub fn shell_thumbnail(path: &Path, size: u32) -> Option<(Vec<u8>, u32, u32)> {
    #[cfg(windows)]
    {
        use windows::Win32::System::Com::{
            CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
        };

        let p = path.to_string_lossy().into_owned();
        let wide = to_wide(&p);

        // COM 初始化:本函数可能在任意工作线程被调用,必须自己配平。
        // SAFETY: CoInitializeEx/CoUninitialize 是标准的 COM 线程配对调用。
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            // S_OK / S_FALSE 都意味着本线程的 apartment 由本次调用建立,需要配平 CoUninitialize;
            // RPC_E_CHANGED_MODE 表示线程已用别的模型初始化过,不能再 CoUninitialize。
            let needs_uninit = hr.is_ok();
            let res = shell_thumbnail_inner(&wide, size);
            if needs_uninit {
                CoUninitialize();
            }
            res
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (path, size);
        None
    }
}

#[cfg(windows)]
unsafe fn shell_thumbnail_inner(wide: &[u16], size: u32) -> Option<(Vec<u8>, u32, u32)> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HWND, SIZE};
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, ReleaseDC, BITMAPINFO, DIB_RGB_COLORS,
    };
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK,
        SIIGBF_RESIZETOFIT,
    };

    // SAFETY: 调用方保证 COM 已初始化;wide 为 NUL 结尾。
    let factory: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()? };
    let dim = SIZE {
        cx: size as i32,
        cy: size as i32,
    };
    // SAFETY: factory 有效;flags 组合合法。
    let hbitmap = unsafe { factory.GetImage(dim, SIIGBF_RESIZETOFIT | SIIGBF_BIGGERSIZEOK).ok()? };

    // 取 HBITMAP 的像素。
    // SAFETY: hbitmap 是有效 HBITMAP;GetDC(NULL) 返回屏幕 DC,ReleaseDC 配平。
    unsafe {
        let hdc = GetDC(HWND(std::ptr::null_mut()));
        if hdc.is_invalid() {
            let _ = DeleteObject(hbitmap);
            return None;
        }
        // 先问一次尺寸
        let mut tmp = BITMAPINFO::default();
        tmp.bmiHeader.biSize = std::mem::size_of::<
            windows::Win32::Graphics::Gdi::BITMAPINFOHEADER,
        >() as u32;
        let got = GetDIBits(hdc, hbitmap, 0, 0, None, &mut tmp, DIB_RGB_COLORS);
        if got == 0 {
            ReleaseDC(HWND(std::ptr::null_mut()), hdc);
            let _ = DeleteObject(hbitmap);
            return None;
        }
        let w = tmp.bmiHeader.biWidth.unsigned_abs();
        let h = tmp.bmiHeader.biHeight.unsigned_abs();
        if w == 0 || h == 0 {
            ReleaseDC(HWND(std::ptr::null_mut()), hdc);
            let _ = DeleteObject(hbitmap);
            return None;
        }
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of::<
            windows::Win32::Graphics::Gdi::BITMAPINFOHEADER,
        >() as u32;
        info.bmiHeader.biWidth = w as i32;
        info.bmiHeader.biHeight = h as i32;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = 0; // BI_RGB
        let lines = GetDIBits(
            hdc,
            hbitmap,
            0,
            h,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut info,
            DIB_RGB_COLORS,
        );
        ReleaseDC(HWND(std::ptr::null_mut()), hdc);
        let _ = DeleteObject(hbitmap);
        if lines == 0 {
            return None;
        }
        Some((buf, w, h))
    }
}

#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_type_policy_matches_original() {
        // 原版:网络盘不扫、光驱不扫;固定盘/移动盘扫。
        assert!(!DriveType::Remote.scannable());
        assert!(!DriveType::CdRom.scannable());
        assert!(!DriveType::Unknown.scannable());
        assert!(!DriveType::NoRootDir.scannable());
        assert!(DriveType::Fixed.scannable());
        assert!(DriveType::Removable.scannable());
    }

    #[test]
    fn drive_type_of_nonexistent_is_none_or_unknown() {
        // Z: 通常不存在;无论返回 None 还是 Unknown,都不应被判为可扫描。
        let t = drive_type(Path::new("Z:\\"));
        assert!(!matches!(t, Some(DriveType::Fixed) | Some(DriveType::Removable)));
    }

    #[test]
    fn cpu_count_is_at_least_one() {
        assert!(cpu_count() >= 1);
    }
}
