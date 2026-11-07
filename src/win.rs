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
        matches!(
            self,
            DriveType::Removable | DriveType::Fixed | DriveType::RamDisk
        )
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

/// 本机可浏览的盘根(按扫描策略分流:固定盘/可移动盘/内存盘)。
///
/// 导入页的「我的电脑」源用它;非 Windows 返回家目录,保证列表不空。
pub fn drive_roots() -> Vec<std::path::PathBuf> {
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::GetLogicalDrives;
        let mut out: Vec<std::path::PathBuf> = Vec::new();
        // SAFETY: GetLogicalDrives 只返回位掩码,不写内存。
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26u32 {
            if mask & (1u32 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = std::path::PathBuf::from(format!("{letter}:\\"));
            if drive_type(&root).map(|t| t.scannable()).unwrap_or(false) {
                out.push(root);
            }
        }
        out
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME")
            .map(|h| vec![std::path::PathBuf::from(h)])
            .unwrap_or_default()
    }
}

/// 取盘符根目录的卷标(资源管理器里显示的名字)。
/// WPD/盘符统一设备列表用它显示「E:\ — 我的SD卡」。
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

/// 取盘符根目录的文件系统名(NTFS / FAT32 / exFAT / ReFS …)。
/// USN/MFT 卷级扫描的前置判定:只有 NTFS 才有 USN journal。
/// 取不到返回 `None`。
pub fn volume_filesystem(root: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
        let root = root.to_string_lossy().into_owned();
        if !root.ends_with('\\') && !root.ends_with('/') {
            return None;
        }
        let wide = to_wide(&root);
        let mut fs_buf = [0u16; 64];
        // SAFETY: wide 是 NUL 结尾的 UTF-16 串;fs_buf 长度与切片一致,其余出参不需要。
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
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// 云盘占位文件判定:网盘(OneDrive / SeaFile / 百度网盘等)的
/// 「按需下载」占位文件带这些属性位,**打开内容就会触发联网下载**。
///
/// 只看属性位,不打开文件 —— `GetFileAttributesW` 是纯元数据查询,安全:
/// - `OFFLINE`(0x1000):文件物理上不在线上(传统离线存储);
/// - `RECALL_ON_OPEN`(0x40000):打开(含句柄级读)就会召回;
/// - `UNPINNED`(0x0010_0000):云文件 API 的「未固定」= 按需占位;
/// - `RECALL_ON_DATA_ACCESS`(0x0040_0000):读数据才召回(OneDrive 默认)。
///
/// 用途:缩略图回填等**自动**内容访问前先问这一句,占位文件跳过,
/// 别替用户把整库拉下来。用户主动打开/编辑不设限(那是明确意图)。
pub fn is_cloud_placeholder(p: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::GetFileAttributesW;
        const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
        const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
        const FILE_ATTRIBUTE_UNPINNED: u32 = 0x0010_0000;
        const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
        let wide: Vec<u16> = p
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: wide 以 NUL 结尾;INVALID_FILE_ATTRIBUTES(查询失败)当不占位,
        // 让后续打开按真实错误处理,别把权限问题误伤成跳过。
        let attrs = unsafe { GetFileAttributesW(PCWSTR(wide.as_ptr())) };
        if attrs == u32::MAX {
            return false;
        }
        attrs
            & (FILE_ATTRIBUTE_OFFLINE
                | FILE_ATTRIBUTE_RECALL_ON_OPEN
                | FILE_ATTRIBUTE_UNPINNED
                | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
            != 0
    }
    #[cfg(not(windows))]
    {
        let _ = p;
        false
    }
}

/// 取用户壳目录(我的文档 / 图片 / 桌面),按存在性过滤。
///
/// 对应参考件首启默认扫描范围 —— 安装文案原文:
/// "Only search My Documents, My Pictures, and the Desktop"。
/// 参考件首启只登记这三个壳目录,绝不扫盘根;扩大范围由用户手动添加。
pub fn known_dirs() -> Vec<std::path::PathBuf> {
    SHELL_DIRS.iter().filter_map(|f| known_path(*f)).collect()
}

/// 工具栏「查看选项」的壳目录快捷键(参考件 View 菜单里的
/// Desktop / My Documents / My Pictures / My Computer 四项)。
///
/// 「我的电脑」在经典图库里是浏览树根(可下探任意盘);我方左栏是图库树,
/// 覆盖不到全盘,故路径给 `None`,由调用方给出明确说明而不是假装能跳。
pub fn shell_shortcuts() -> Vec<(&'static str, Option<std::path::PathBuf>)> {
    vec![
        ("桌面", known_path(FOLDERID::Desktop)),
        ("我的文档", known_path(FOLDERID::Documents)),
        ("我的图片", known_path(FOLDERID::Pictures)),
        ("我的电脑", None),
    ]
}

/// 壳目录枚举(Windows 用 KNOWNFOLDERID,其它平台退回家目录同名子目录)。
#[derive(Clone, Copy)]
enum FOLDERID {
    Desktop,
    Documents,
    Pictures,
}

/// 单个壳目录路径;不存在(未重定向/未创建)返回 None。
fn known_path(which: FOLDERID) -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Com::CoTaskMemFree;
        use windows::Win32::UI::Shell::{
            FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Pictures, SHGetKnownFolderPath,
            KNOWN_FOLDER_FLAG,
        };
        let fid = match which {
            FOLDERID::Desktop => &FOLDERID_Desktop,
            FOLDERID::Documents => &FOLDERID_Documents,
            FOLDERID::Pictures => &FOLDERID_Pictures,
        };
        // SAFETY: rfid 指向静态常量 GUID;htoken 传 NULL 表示当前用户。
        let pw = unsafe {
            SHGetKnownFolderPath(fid, KNOWN_FOLDER_FLAG(0), HANDLE(std::ptr::null_mut()))
        }
        .ok()?;
        let s = unsafe { pw.to_string() }.unwrap_or_default();
        unsafe { CoTaskMemFree(Some(pw.as_ptr() as _)) };
        if s.is_empty() {
            return None;
        }
        let p = std::path::PathBuf::from(s);
        p.is_dir().then_some(p)
    }
    #[cfg(not(windows))]
    {
        let name = match which {
            FOLDERID::Desktop => "Desktop",
            FOLDERID::Documents => "Documents",
            FOLDERID::Pictures => "Pictures",
        };
        let p = dirs::home_dir()?.join(name);
        p.is_dir().then_some(p)
    }
}

/// `known_dirs` 的三件套(顺序沿用参考件:文档、图片、桌面)。
const SHELL_DIRS: [FOLDERID; 3] = [FOLDERID::Documents, FOLDERID::Pictures, FOLDERID::Desktop];

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
            SystemParametersInfoW, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE, SPI_SETDESKWALLPAPER,
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
/// 视频播放的**等效替代**:原版在库内用 DirectShow filter graph
/// 渲染到一个视频子窗口,我方不内嵌视频管线,改为交给系统默认播放器 ——
/// 与原版「能看」的目标一致,差异在于不内嵌视频管线,特效与体量从简。
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

/// ShellExecuteExW + runas:提权拉起一个程序,窗口一律 SW_HIDE(不留黑框)。
///
/// `wait_ms` = `Some(n)`:最多等 n 毫秒并返回退出码(注册/删除计划任务要用);
/// `wait_ms` = `None`:发后即忘(索引器子进程,结果靠 `--mft-report` 文件回报)。
#[cfg(windows)]
fn shell_execute_elevated(
    file: &str,
    params: &str,
    wait_ms: Option<u32>,
) -> Result<Option<u32>, String> {
    use windows::core::{HRESULT, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, HWND};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let file_w = to_wide(file);
    let params_w = to_wide(params);
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: HWND::default(),
        lpVerb: windows::core::w!("runas"),
        lpFile: PCWSTR(file_w.as_ptr()),
        lpParameters: PCWSTR(params_w.as_ptr()),
        lpDirectory: PCWSTR::null(),
        // 原来是 SW_SHOWNORMAL:debug 构建是控制台子系统,提权子进程因此会弹一个
        // 黑色控制台框。SW_HIDE 对 release(GUI 子系统)无副作用,对 debug 就把框藏了。
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: sei 各指针均为合法 NUL 结尾宽串或空;结构体大小已填。
    unsafe { ShellExecuteExW(&mut sei) }.map_err(|e| {
        // 1223 = ERROR_CANCELLED,用户在 UAC 上点了"否"。
        if e.code() == HRESULT::from_win32(1223) {
            "管理员授权被取消".to_string()
        } else {
            format!("提权启动失败: {e}")
        }
    })?;
    let h = sei.hProcess;
    if h.is_invalid() {
        return Ok(None);
    }
    let out = match wait_ms {
        // SAFETY: h 是 ShellExecuteEx 成功时交给我们的进程句柄;等待/取退出码只读它,
        // 两条分支各 CloseHandle 一次配平。
        Some(ms) => {
            let waited = unsafe { WaitForSingleObject(h, ms).0 };
            let mut exit = 0u32;
            // WAIT_OBJECT_0 == 0:进程按预期结束。
            let got = waited == 0 && unsafe { GetExitCodeProcess(h, &mut exit).is_ok() };
            unsafe {
                let _ = CloseHandle(h);
            }
            if !got {
                return Err(if waited == 0 {
                    "提权进程退出码读取失败".to_string()
                } else {
                    "提权进程超时未结束".to_string()
                });
            }
            Some(exit)
        }
        None => {
            unsafe {
                let _ = CloseHandle(h);
            }
            None
        }
    };
    Ok(out)
}

/// 用 UAC 提权拉起**自身**子进程(runas 动词),发后即忘。
///
/// 每次调用都要过一次 UAC(Windows 不给"记住"runas 同意),所以只用在
/// 「本次单独授权跑一轮全盘扫描」这一档;常驻档走服务 [`service_install_elevated`]。
/// `params` 是完整命令行参数串(如 `--mft-scan`)。
pub fn launch_elevated(params: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().map_err(|e| format!("取自身路径失败: {e}"))?;
        shell_execute_elevated(&exe.to_string_lossy(), params, None).map(|_| ())
    }
    #[cfg(not(windows))]
    {
        let _ = params;
        Err("仅支持 Windows".into())
    }
}

/// 提权拉起自身跑 `--apply-update <工作目录> <令牌> <是否起服务>`,并等它退出
/// (最多 5 分钟:留给用户响应 UAC、服务收尾、大包校验的时间)。
///
/// 更新安装必须提权的两个理由:服务在跑时锁着程序文件,停它要 SERVICE_STOP;
/// 程序装在受保护目录时,往同目录写新 exe 也要管理员。把这两件事合并进一个
/// 子进程,用户只为一次安装授权一次。`params` 由调用方拼好(含工作目录)。
pub fn launch_update_helper(params: &str) -> Result<Option<u32>, String> {
    #[cfg(windows)]
    {
        let exe = std::env::current_exe().map_err(|e| format!("取自身路径失败: {e}"))?;
        shell_execute_elevated(&exe.to_string_lossy(), params, Some(300_000))
    }
    #[cfg(not(windows))]
    {
        let _ = params;
        Err("仅支持 Windows".into())
    }
}

// 全盘索引服务的安装/查询/卸载(替掉原先「计划任务 + 每次一次性拉起子进程」)。
//
// 索引引擎必须常驻:USN 保鲜是挂在内核里阻塞读 journal 的活进程,不是跑完就退的
// 批处理。所以进程模型换成拆进程 —— 一个自动启动的提权服务 + 非提权 UI
// 客户端。`CreateServiceW`/`DeleteService` 都要管理员,于是安装与卸载各走一次
// 「提权重启自己 + 子命令」([`service_install_elevated`] / [`service_uninstall_elevated`]),
// 这是本功能唯一需要用户点"是"的地方;之后由 SCM 自动拉起,UI 侧全程静默。

/// 服务当前状态(查询不需要管理员)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// 从未注册,或已被卸载。
    NotInstalled,
    /// 已注册但没在跑 —— UI 该给"启动服务"的出口,而不是引导用户重装。
    Stopped,
    Running,
    /// 启动中/停止中等过渡态。原始码留给日志,别当成确定结论。
    Transition(u32),
}

/// SCM 句柄的 RAII 包装:`CloseServiceHandle` 在每条返回路径上都要调,手写必漏。
#[cfg(windows)]
struct ScHandle(windows::Win32::System::Services::SC_HANDLE);

#[cfg(windows)]
impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: 句柄由本包装独占;关闭失败(已无效)只返回 FALSE,无副作用。
        unsafe {
            let _ = windows::Win32::System::Services::CloseServiceHandle(self.0);
        }
    }
}

/// 查服务状态。SCM 都打不开(极罕见)按未注册处理,让 UI 直接走降级路径。
pub fn service_state() -> ServiceState {
    #[cfg(windows)]
    {
        use windows::core::{HRESULT, PCWSTR};
        use windows::Win32::Foundation::ERROR_SERVICE_DOES_NOT_EXIST;
        use windows::Win32::System::Services::{
            OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_MANAGER_CONNECT,
            SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STOPPED,
        };

        let name = to_wide(crate::service::SERVICE_NAME);
        // SAFETY: 前两个参数传 null 是 SCM 约定(本机 + 默认数据库);
        // name 是 NUL 结尾宽串,存活到调用结束。
        let scm = match unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
        {
            Ok(h) => ScHandle(h),
            Err(_) => return ServiceState::NotInstalled,
        };
        let svc = match unsafe { OpenServiceW(scm.0, PCWSTR(name.as_ptr()), SERVICE_QUERY_STATUS) } {
            Ok(h) => ScHandle(h),
            Err(e) => {
                return if e.code() == HRESULT::from_win32(ERROR_SERVICE_DOES_NOT_EXIST.0) {
                    ServiceState::NotInstalled
                } else {
                    // 打得开 SCM 却开不了这个服务:多半是权限被组策略收走。
                    // 报 Stopped 而非 NotInstalled,免得 UI 引导用户去重装一个已存在的服务。
                    ServiceState::Stopped
                };
            }
        };
        let mut st = SERVICE_STATUS::default();
        // SAFETY: st 是本栈上的完整结构,由 QueryServiceStatus 填写。
        if unsafe { QueryServiceStatus(svc.0, &mut st) }.is_err() {
            return ServiceState::Stopped;
        }
        if st.dwCurrentState == SERVICE_RUNNING {
            ServiceState::Running
        } else if st.dwCurrentState == SERVICE_STOPPED {
            ServiceState::Stopped
        } else {
            ServiceState::Transition(st.dwCurrentState.0)
        }
    }
    #[cfg(not(windows))]
    {
        ServiceState::NotInstalled
    }
}

/// 服务是否已注册(含停着的)。
pub fn service_installed() -> bool {
    service_state() != ServiceState::NotInstalled
}

/// 清掉旧版遗留的一次性索引计划任务(名字固定;不存在也算成功)。
///
/// 常驻服务上线前,索引靠计划任务定时跑 `--mft-scan` 维护;两套机制同时活着
/// 会并发写同一份 nameindex.bin。这一步只在提权的安装/装卸子进程里做,
/// 那里才有删任务的权限;任何失败都忽略,不能拖累服务本身的装配。
#[cfg(windows)]
pub fn remove_legacy_indexer_task() {
    use std::os::windows::process::CommandExt;
    /// 控制台子进程不该在 GUI 后面闪一个黑框。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const LEGACY_TASK: &str = "RelytoMaliangIndexer";
    let _ = std::process::Command::new("schtasks.exe")
        .args(["/Delete", "/TN", LEGACY_TASK, "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

#[cfg(not(windows))]
pub fn remove_legacy_indexer_task() {}

/// 提权进程内执行:注册服务(自动启动 + LocalSystem)+ 写描述 + 立刻启动。
/// 服务已存在时改配置指向当前 exe(升级/搬目录后能自愈),而不是报"已存在"。
pub fn service_install(exe: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows::core::{HRESULT, PCWSTR, PWSTR};
        use windows::Win32::Foundation::ERROR_SERVICE_ALREADY_RUNNING;
        use windows::Win32::System::Services::{
            ChangeServiceConfig2W, ChangeServiceConfigW, CreateServiceW, OpenSCManagerW,
            OpenServiceW, StartServiceW, ENUM_SERVICE_TYPE, SC_MANAGER_CONNECT,
            SC_MANAGER_CREATE_SERVICE, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
            SERVICE_CONFIG_DESCRIPTION, SERVICE_DESCRIPTIONW, SERVICE_ERROR, SERVICE_ERROR_NORMAL,
            SERVICE_NO_CHANGE, SERVICE_START_TYPE, SERVICE_WIN32_OWN_PROCESS,
        };

        let name = to_wide(crate::service::SERVICE_NAME);
        let display = to_wide(crate::service::SERVICE_DISPLAY);
        let desc = to_wide(crate::service::SERVICE_DESC);
        // 路径必须带引号:安装目录常含空格。
        let bin = to_wide(&format!("\"{}\" --service", exe.display()));

        // SAFETY: null = 本机 + 默认数据库;CREATE_SERVICE 是注册所需的最小附加权限。
        let scm = ScHandle(
            unsafe {
                OpenSCManagerW(
                    PCWSTR::null(),
                    PCWSTR::null(),
                    SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE,
                )
            }
            .map_err(|e| format!("打开服务控制管理器失败(需要管理员): {e}"))?,
        );

        // SAFETY: 已存在 → 拿全权限句柄改配置;不存在 → 走 CreateServiceW。
        let existing =
            unsafe { OpenServiceW(scm.0, PCWSTR(name.as_ptr()), SERVICE_ALL_ACCESS) }.ok();
        let svc = match existing {
            Some(h) => {
                let h = ScHandle(h);
                // SAFETY: 只改二进制路径与显示名,其余字段用 SERVICE_NO_CHANGE / null 保持原值;
                // 各宽串在本函数内存活。
                unsafe {
                    ChangeServiceConfigW(
                        h.0,
                        ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
                        SERVICE_START_TYPE(SERVICE_NO_CHANGE),
                        SERVICE_ERROR(SERVICE_NO_CHANGE),
                        PCWSTR(bin.as_ptr()),
                        PCWSTR::null(),
                        None,
                        PCWSTR::null(),
                        PCWSTR::null(),
                        PCWSTR::null(),
                        PCWSTR(display.as_ptr()),
                    )
                }
                .map_err(|e| format!("更新服务配置失败: {e}"))?;
                h
            }
            None => {
                // SAFETY: lpServiceStartName 传 NULL ⇒ 服务跑在 LocalSystem 下,
                // 读 $MFT / USN 日志正好需要这个账户;lpdwTagId/lpDependencies 不需要。
                let h = unsafe {
                    CreateServiceW(
                        scm.0,
                        PCWSTR(name.as_ptr()),
                        PCWSTR(display.as_ptr()),
                        SERVICE_ALL_ACCESS,
                        SERVICE_WIN32_OWN_PROCESS,
                        SERVICE_AUTO_START,
                        SERVICE_ERROR_NORMAL,
                        PCWSTR(bin.as_ptr()),
                        PCWSTR::null(),
                        None,
                        PCWSTR::null(),
                        PCWSTR::null(),
                        PCWSTR::null(),
                    )
                }
                .map_err(|e| format!("注册服务失败(需要管理员): {e}"))?;
                ScHandle(h)
            }
        };

        // 描述写在服务的"描述"列 —— 用户在 services.msc 里要能看出它是干什么的、
        // 停掉会失去什么,否则只会被当成可疑的常驻提权进程卸掉。
        let d = SERVICE_DESCRIPTIONW {
            lpDescription: PWSTR(desc.as_ptr() as *mut u16),
        };
        // SAFETY: d 指向本栈上的 desc 宽串;lpInfo 按 SERVICE_CONFIG_DESCRIPTION 解释。
        unsafe {
            ChangeServiceConfig2W(
                svc.0,
                SERVICE_CONFIG_DESCRIPTION,
                Some(&d as *const SERVICE_DESCRIPTIONW as *const std::ffi::c_void),
            )
        }
        .map_err(|e| format!("写服务描述失败: {e}"))?;

        // SAFETY: 不带参数启动(--service 已写在二进制路径里)。
        let started = match unsafe { StartServiceW(svc.0, None) } {
            Ok(()) => Ok(()),
            // 已在跑就是成功:重复安装不该报错。
            Err(e) if e.code() == HRESULT::from_win32(ERROR_SERVICE_ALREADY_RUNNING.0) => Ok(()),
            Err(e) => Err(format!("服务已注册但启动失败: {e}")),
        };
        if started.is_ok() {
            // 常驻服务上线后,旧版那条定时扫描若还留着就会和它并发写同一份
            // nameindex.bin。装服务这一步是提权的,顺手把遗留任务清掉。
            remove_legacy_indexer_task();
        }
        started
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        Err("仅支持 Windows".into())
    }
}

/// Win32 的通用 DELETE 访问权(WinNT.h),服务句柄上就是"可删除"。
/// windows crate 没导出这个名字,按裸值落常量;`service_delete_right_is_win32_value` 钉住。
#[cfg(windows)]
const SERVICE_DELETE: u32 = 0x0001_0000;

/// 发停止命令并等到 STOPPED(尽力而为):返回是否真看到了停干净。
///
/// 凡是要动服务的动作都得先过这一关 —— 进程还活着时它锁着自己的 exe 映像,
/// 新二进制覆盖不上,删注册也只会被 SCM 标成"待删除"。
/// 返回 `false` 不代表失败:卸载方照样往下删(句柄一放就生效),
/// 只有"只停不删"的入口需要把它当错误告诉用户。
#[cfg(windows)]
fn stop_and_wait(svc: &ScHandle) -> bool {
    use windows::Win32::System::Services::{
        ControlService, QueryServiceStatus, SERVICE_CONTROL_STOP, SERVICE_STATUS, SERVICE_STOPPED,
    };
    let mut st = SERVICE_STATUS::default();
    // SAFETY: st 由 ControlService / QueryServiceStatus 填写。
    // ControlService 在服务已停时会失败,那不是错误 —— 后面的等待循环按实际状态判。
    unsafe {
        let _ = ControlService(svc.0, SERVICE_CONTROL_STOP, &mut st);
    }
    // 最多等 10s:保鲜线程可能正挂在内核的 USN 阻塞读里,收到 STOP 后要自己收尾。
    for _ in 0..50 {
        // SAFETY: 同上。
        // 查不到状态 = 句柄已经作废(注册项没了),没什么可停的,按停干净算。
        if unsafe { QueryServiceStatus(svc.0, &mut st) }.is_err() {
            return true;
        }
        if st.dwCurrentState == SERVICE_STOPPED {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

/// 提权进程内执行:**只停服务,注册项原样留着**。
///
/// 为什么要有这一步:服务在跑就锁着自己的 exe,重编/升级/搬目录全都动不了它。
/// 让用户去 services.msc 里翻一个陌生名字、或敲 `net stop RelytoMaliangIndex`,
/// 等于把这个能力做在了产品外面 —— 停与起都必须在本程序的选项页里点得到。
pub fn service_stop() -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::System::Services::{
            OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS, SERVICE_STOP,
        };
        let name = to_wide(crate::service::SERVICE_NAME);
        // SAFETY: null = 本机 + 默认数据库。
        let scm = ScHandle(
            unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
                .map_err(|e| format!("打开服务控制管理器失败(需要管理员): {e}"))?,
        );
        // SAFETY: name 是 NUL 结尾宽串;停服务要 STOP + QUERY_STATUS 两个权限。
        let svc = match unsafe {
            OpenServiceW(
                scm.0,
                PCWSTR(name.as_ptr()),
                SERVICE_STOP | SERVICE_QUERY_STATUS,
            )
        } {
            Ok(h) => ScHandle(h),
            // 压根没注册 = 没有可停的东西,和卸载一样保持幂等。
            Err(_) => return Ok(()),
        };
        if stop_and_wait(&svc) {
            Ok(())
        } else {
            Err("索引服务 10 秒内没停下来(可能正卡在磁盘读上),稍后再点一次".into())
        }
    }
    #[cfg(not(windows))]
    {
        Err("仅支持 Windows".into())
    }
}

/// 提权进程内执行:**只启动已注册的服务**,不碰配置、不改映像路径。
///
/// 停过的服务不会自己回来(SCM 只在开机按启动类型拉),所以「停」必须在产品里
/// 配一个「起」,否则用户点完停就只有重装这一条回头路,还得多花一次 UAC。
pub fn service_start() -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows::core::{HRESULT, PCWSTR};
        use windows::Win32::Foundation::ERROR_SERVICE_ALREADY_RUNNING;
        use windows::Win32::System::Services::{
            OpenSCManagerW, OpenServiceW, StartServiceW, SC_MANAGER_CONNECT, SERVICE_START,
        };
        let name = to_wide(crate::service::SERVICE_NAME);
        // SAFETY: null = 本机 + 默认数据库。
        let scm = ScHandle(
            unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
                .map_err(|e| format!("打开服务控制管理器失败(需要管理员): {e}"))?,
        );
        // SAFETY: name 是 NUL 结尾宽串。
        let svc = unsafe { OpenServiceW(scm.0, PCWSTR(name.as_ptr()), SERVICE_START) }
            .map_err(|_| "索引服务尚未安装,请先点「安装索引服务…」")?;
        let svc = ScHandle(svc);
        // SAFETY: 不带参数启动(--service 已写在二进制路径里)。
        match unsafe { StartServiceW(svc.0, None) } {
            Ok(()) => Ok(()),
            // 已经在跑(比如开机自动起来了)就是成功:重复点不该报错。
            Err(e) if e.code() == HRESULT::from_win32(ERROR_SERVICE_ALREADY_RUNNING.0) => Ok(()),
            Err(e) => Err(format!("启动索引服务失败: {e}")),
        }
    }
    #[cfg(not(windows))]
    {
        Err("仅支持 Windows".into())
    }
}

/// 提权进程内执行:停服务 → 等它真停 → 删注册。
///
/// 删除前必须等到 STOPPED:服务还在跑就删,SCM 只标记"待删除",
/// 同名服务在我们的句柄关掉前既查不到也重建不了,用户看到的是一团乱。
/// 等满 10s 仍没停也照删(标记待删除,句柄一放就生效)—— 卸载失败卡住比多等一轮更糟。
pub fn service_uninstall() -> Result<(), String> {
    #[cfg(windows)]
    {
        // 这个子进程是提权的:服务撤掉之后,旧版定时扫描不该继续孤零零地
        // 写索引文件,顺手把遗留任务也清掉(不存在时是空操作)。
        remove_legacy_indexer_task();
        use windows::core::PCWSTR;
        use windows::Win32::System::Services::{
            DeleteService, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS,
            SERVICE_STOP,
        };

        let name = to_wide(crate::service::SERVICE_NAME);
        // SAFETY: null = 本机 + 默认数据库。
        let scm = ScHandle(
            unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
                .map_err(|e| format!("打开服务控制管理器失败(需要管理员): {e}"))?,
        );
        // SAFETY: name 是 NUL 结尾宽串;要停、要查、要删,所以三个权限都拿。
        let svc = match unsafe {
            OpenServiceW(
                scm.0,
                PCWSTR(name.as_ptr()),
                SERVICE_STOP | SERVICE_QUERY_STATUS | SERVICE_DELETE,
            )
        } {
            Ok(h) => ScHandle(h),
            // 打不开就当已卸载:卸载要幂等,重复点不该报错。
            Err(_) => return Ok(()),
        };
        let _ = stop_and_wait(&svc);
        // SAFETY: 删注册。
        unsafe { DeleteService(svc.0) }.map_err(|e| format!("删除服务失败: {e}"))
    }
    #[cfg(not(windows))]
    {
        Err("仅支持 Windows".into())
    }
}

/// 从非提权 UI 触发安装:提权重启自己跑 `--install-service`,等它退出并读退出码。
/// 整个功能唯一一次 UAC 弹窗就在这儿;装完服务自动启动,以后不再弹。
pub fn service_install_elevated() -> Result<(), String> {
    #[cfg(windows)]
    {
        elevated_self("--install-service")
    }
    #[cfg(not(windows))]
    {
        Err("仅支持 Windows".into())
    }
}

/// 撤销授权 = 卸载服务(改服务注册要提权,所以这一步仍会弹一次 UAC)。
pub fn service_uninstall_elevated() -> Result<(), String> {
    #[cfg(windows)]
    {
        elevated_self("--uninstall-service")
    }
    #[cfg(not(windows))]
    {
        Err("仅支持 Windows".into())
    }
}

/// 从非提权 UI 触发「只停服务」:停服务句柄要 SERVICE_STOP,也只有提权进程拿得到。
pub fn service_stop_elevated() -> Result<(), String> {
    #[cfg(windows)]
    {
        elevated_self("--stop-service")
    }
    #[cfg(not(windows))]
    {
        Err("仅支持 Windows".into())
    }
}

/// 从非提权 UI 触发「只启动服务」(StartService 同样要 SERVICE_START)。
pub fn service_start_elevated() -> Result<(), String> {
    #[cfg(windows)]
    {
        elevated_self("--start-service")
    }
    #[cfg(not(windows))]
    {
        Err("仅支持 Windows".into())
    }
}

/// 提权重启自身跑一条子命令并等结果。子进程的具体失败原因在它自己的日志里,
/// 这里只有退出码可用(release 是 GUI 子系统,stdout 无处可去,不指望它)。
#[cfg(windows)]
fn elevated_self(flag: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("取自身路径失败: {e}"))?;
    match shell_execute_elevated(&exe.to_string_lossy(), flag, Some(120_000))? {
        Some(0) => Ok(()),
        Some(code) => Err(format!("提权子进程退出码 {code}(详见数据目录 index-service.log)")),
        None => Err("提权子进程未返回结果".to_string()),
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
    let hbitmap = unsafe {
        factory
            .GetImage(dim, SIIGBF_RESIZETOFIT | SIIGBF_BIGGERSIZEOK)
            .ok()?
    };

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
        tmp.bmiHeader.biSize =
            std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
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
        info.bmiHeader.biSize =
            std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
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

/// 弹出 Windows 资源管理器**原生**右键菜单(含"打开方式""发送到"等全部扩展项)。
///
/// COM 流程:`SHCreateItemFromParsingName` → `BindToHandler(BHID_SFUIObject)`
/// → `IContextMenu::QueryContextMenu` 填充 HMENU → `TrackPopupMenuEx`
/// (`TPM_RETURNCMD` 模态收命令号) → `InvokeCommand`。
/// 菜单位于光标处;模态阻塞直到用户选完/取消,返回是否执行了命令。
/// 文件与文件夹通用(文件夹拿到的是目录自身的上下文菜单)。
pub fn shell_context_menu(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::System::Com::{
            CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
        };
        let p = path.to_string_lossy().into_owned();
        let wide = to_wide(&p);
        // 与 shell_thumbnail 同一套配平约定:本调用可能发生在任一线程。
        // SAFETY: CoInitializeEx/CoUninitialize 标准配对;后续 unsafe 块各自注释。
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let needs_uninit = hr.is_ok();
            let res = shell_context_menu_inner(&wide);
            if needs_uninit {
                CoUninitialize();
            }
            res
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

#[cfg(windows)]
unsafe fn shell_context_menu_inner(wide: &[u16]) -> bool {
    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::Shell::{
        BHID_SFUIObject, CMF_NORMAL, IContextMenu, SHCreateItemFromParsingName,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreatePopupMenu, DestroyMenu, GetCursorPos, GetForegroundWindow, TrackPopupMenuEx,
        TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    };

    // SAFETY: wide 为 NUL 结尾宽串;COM 已由外层初始化。
    // 结果行可能在右键前被删/不可达(网络路径),失败即静默放弃,不 panic。
    let Ok(item) = SHCreateItemFromParsingName::<_, _, windows::Win32::UI::Shell::IShellItem>(
        PCWSTR(wide.as_ptr()),
        None,
    ) else {
        return false;
    };
    let Ok(menu) = item.BindToHandler::<_, IContextMenu>(None, &BHID_SFUIObject) else {
        return false;
    };

    // SAFETY: hmenu 由 CreatePopupMenu 创建,失败即退出。
    let Ok(hmenu) = CreatePopupMenu() else {
        return false;
    };
    // 命令号区间 [1, 0x7FFF]:TrackPopupMenuEx(TPM_RETURNCMD) 返回选中号,
    // InvokeCommand 的整数动词 = 选中号 - idcmdfirst。
    let first_cmd: u32 = 1;
    if menu.QueryContextMenu(hmenu, 0, first_cmd, 0x7FFF, CMF_NORMAL).is_err() {
        let _ = DestroyMenu(hmenu);
        return false;
    }
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let hwnd = GetForegroundWindow();
    // SAFETY: hmenu 有效;TPM_RETURNCMD 下返回值即命令号(0 = 未选)。
    let cmd = TrackPopupMenuEx(
        hmenu,
        (TPM_RETURNCMD | TPM_LEFTALIGN | TPM_RIGHTBUTTON).0,
        pt.x,
        pt.y,
        hwnd,
        None,
    )
    .0 as u32;
    let _ = DestroyMenu(hmenu);
    if cmd == 0 {
        return false; // 取消
    }
    let ici = windows::Win32::UI::Shell::CMINVOKECOMMANDINFO {
        cbSize: std::mem::size_of::<windows::Win32::UI::Shell::CMINVOKECOMMANDINFO>() as u32,
        hwnd,
        lpVerb: PCSTR((cmd - first_cmd) as *const u8),
        nShow: windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: ici 已填 cbSize/hwnd/lpVerb(整数动词);menu 有效。
    menu.InvokeCommand(&ici).is_ok()
}

#[cfg(windows)]
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

// ---- 单实例(同一登录会话只允许一个马良 GUI 进程) ----

/// 单实例命名互斥体。`Local\` 前缀 = 作用域限定在当前登录会话:
/// 同一会话的双开被拦下,不同用户/不同 RDP 会话互不干扰。
/// 内核对象由进程退出自动清理,不落盘、无残留问题。
#[cfg(windows)]
const SINGLE_INSTANCE_MUTEX: &str = r"Local\Relyto.Maliang.Gui";

/// 主窗口标题:激活已有实例时 FindWindow 按它找(与 main.rs 的 ViewportBuilder 一致)。
#[cfg(windows)]
pub const MAIN_WINDOW_TITLE: &str = "Relyto.Maliang";

/// 单实例守卫:存活期间本进程握有单实例权。
/// 提前 drop 等于让出单实例权,调用方须一路持有到 main 结束。
pub struct InstanceGuard {
    #[cfg(windows)]
    _handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(not(windows))]
pub struct InstanceGuard;

/// 尝试成为本会话唯一的马良 GUI。
/// 返回 `Some(守卫)` = 拿到单实例权,继续启动;
/// 返回 `None` = 已有实例在跑,调用方应激活既有窗口后退出。
pub fn acquire_single_instance() -> Option<InstanceGuard> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, CloseHandle, GetLastError};
        use windows::Win32::System::Threading::CreateMutexW;
        use windows::core::PCWSTR;

        let name = to_wide(SINGLE_INSTANCE_MUTEX);
        // SAFETY: 名字是 NUL 结尾的 UTF-16 串;句柄属性为空 = 默认安全描述符。
        match unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) } {
            Ok(h) => {
                // CreateMutexW 语义:互斥体已存在时照样返回有效句柄,
                // 靠 GetLastError == ERROR_ALREADY_EXISTS 区分「新建」和「已有人」。
                if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                    unsafe { let _ = CloseHandle(h); }
                    None
                } else {
                    Some(InstanceGuard { _handle: h })
                }
            }
            // 打不开命名互斥体(极端受限环境)就放行,别把程序挡死在门外。
            Err(_) => Some(InstanceGuard { _handle: windows::Win32::Foundation::HANDLE::default() }),
        }
    }
    #[cfg(not(windows))]
    {
        Some(InstanceGuard)
    }
}

/// 把已在运行的实例主窗口拉到前台(最小化则先还原)。
/// 找不到窗口(可能刚退到托盘/标题改了)就安静返回,不报错。
pub fn activate_running_instance() {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
        };
        use windows::core::PCWSTR;

        let title = to_wide(MAIN_WINDOW_TITLE);
        // SAFETY: 类名为空指针、窗口名是 NUL 结尾 UTF-16,二者只读。
        if let Ok(hwnd) = unsafe { FindWindowW(PCWSTR::null(), PCWSTR(title.as_ptr())) } {
            if hwnd != HWND::default() {
                // 第二个实例通常就是前台进程,SetForegroundWindow 在此条件下被放行。
                unsafe {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                    let _ = SetForegroundWindow(hwnd);
                }
            }
        }
    }
}

/// 弹「已在运行」提示框。放在激活已有窗口之后,用户先看到切换、再看到说明。
pub fn alert_already_running() {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND,
        };
        use windows::core::PCWSTR;
        let text = to_wide("马良已经在运行了,同一会话只允许一个进程。\n已为你切换到已打开的窗口。");
        let title = to_wide("马良");
        // SAFETY: 文本/标题均为 NUL 结尾 UTF-16;无父窗口。
        unsafe {
            let _ = MessageBoxW(
                None,
                PCWSTR(text.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
            );
        }
    }
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
        assert!(!matches!(
            t,
            Some(DriveType::Fixed) | Some(DriveType::Removable)
        ));
    }

    #[test]
    fn cpu_count_is_at_least_one() {
        assert!(cpu_count() >= 1);
    }

    /// SERVICE_DELETE 是裸常量(windows crate 没导出这个名字),钉住 WinNT.h 的值。
    #[cfg(windows)]
    #[test]
    fn service_delete_right_is_win32_value() {
        assert_eq!(SERVICE_DELETE, 0x0001_0000);
        // 与 crate 导出的同类访问权不重叠,避免"或"上去其实没拿到删除权。
        use windows::Win32::System::Services::{SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_STOP};
        assert_eq!(SERVICE_STOP | SERVICE_QUERY_STATUS | SERVICE_DELETE, 0x0001_0024);
        assert_ne!(SERVICE_DELETE, SERVICE_START);
        // 「停止索引服务…」只要停 + 查两项:少一项 SCM 直接拒绝,
        // 多要删除权就等于把「停止」偷偷做成「卸载」。
        assert_eq!(SERVICE_STOP | SERVICE_QUERY_STATUS, 0x0000_0024);
        // 「启动索引服务…」只要 SERVICE_START(0x10),它不碰注册项。
        assert_eq!(SERVICE_START, 0x0000_0010);
    }

    /// 服务名/显示名/描述不能为空,也不能带会破坏命令行拼接的引号。
    #[test]
    fn service_metadata_is_usable() {
        use crate::service::{SERVICE_DESC, SERVICE_DISPLAY, SERVICE_NAME};
        for s in [SERVICE_NAME, SERVICE_DISPLAY, SERVICE_DESC] {
            assert!(!s.trim().is_empty());
        }
        // 服务名是 SCM 的键,不能含空格/引号,也不该跟着显示名一起改。
        assert!(!SERVICE_NAME.contains(|c: char| c.is_whitespace() || c == '"'));
        assert!(!SERVICE_DESC.contains('"'));
        // 描述要说清停掉的后果,否则用户只看到一个常驻提权进程。
        assert!(SERVICE_DESC.contains("降级"));
    }

    /// 未注册服务时状态必须是 NotInstalled(而不是误报 Stopped/Running),
    /// UI 靠它决定给"安装服务"还是"启动服务"。
    #[test]
    fn service_state_is_queryable_without_admin() {
        let st = service_state();
        // 本机可能真装了这个服务,所以只钉"查询不 panic、结论自洽"这一层。
        assert_eq!(service_installed(), st != ServiceState::NotInstalled);
        assert!(matches!(
            st,
            ServiceState::NotInstalled
                | ServiceState::Stopped
                | ServiceState::Running
                | ServiceState::Transition(_)
        ));
    }
}
