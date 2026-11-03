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

/// 取用户壳目录(我的文档 / 图片 / 桌面),按存在性过滤。
///
/// 对应参考件首启默认扫描范围 —— 安装文案原文:
/// "Only search My Documents, My Pictures, and the Desktop"。
/// 参考件首启只登记这三个壳目录,绝不扫盘根;扩大范围由用户手动添加。
pub fn known_dirs() -> Vec<std::path::PathBuf> {
    SHELL_DIRS.iter().filter_map(|f| known_path(*f)).collect()
}

/// 工具栏「查看选项」的壳目录快捷键(参考件 `ID_VIEW_DESKTOP` /
/// `ID_VIEW_MYDOCS` / `ID_VIEW_MYPICTURES` / `ID_VIEW_MYCOMPUTER`)。
///
/// 我的电脑在参考件里是浏览树根(可下探任意盘);我方左栏是图库树,
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
/// 每次调用都要过一次 UAC(Windows 不给"记住"runas 同意),现在只服务
/// 「每次单独授权」这一档;默认档走计划任务 [`mft_task_install`]。
/// `params` 是完整命令行参数串(如 `--mft-scan --mft-report <文件>`)。
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

/// 全盘索引更新器的计划任务名(注册一次,以后静默触发)。
pub const MFT_TASK_NAME: &str = "RelytoMaliangIndexer";
/// 计划任务固定执行的参数:不带报告路径(由子进程自己按数据目录推导),
/// 这样数据目录改了也不用重注册;`--mft-task` 让子进程摘掉自己的控制台。
pub const MFT_TASK_ARGS: &str = "--mft-scan --mft-task";

/// 隐藏窗口运行命令行工具并等它结束,返回退出码(schtasks 专用,不闪黑框)。
pub fn run_hidden(program: &str, args: &[&str]) -> Result<i32, String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Stdio;
        // CREATE_NO_WINDOW:子进程不分配控制台。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let status = std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("启动 {program} 失败: {e}"))?
            .wait()
            .map_err(|e| format!("等待 {program} 结束失败: {e}"))?;
        Ok(status.code().unwrap_or(-1))
    }
    #[cfg(not(windows))]
    {
        let _ = (program, args);
        Err("仅支持 Windows".into())
    }
}

/// 当前用户 SID 字符串(计划任务 `<UserId>` 用 SID,比 `域\别名` 无歧义:
/// 微软账户、Entra 加入的机器上别名都不等于登录名)。
#[cfg(windows)]
fn current_user_sid() -> Result<String, String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL};
    use windows::Win32::Security::{
        Authorization::ConvertSidToStringSidW, GetTokenInformation, TokenUser, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: 打开的是本进程令牌,TOKEN_QUERY 只读,句柄由本函数负责关闭。
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|e| format!("打开进程令牌失败: {e}"))?;
    let mut len = 0u32;
    // SAFETY: 第一次传空缓冲只问所需长度,是 Win32 约定用法。
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
    }
    let mut buf = vec![0u8; len.max(1) as usize];
    // SAFETY: buf 长度即上一步问到的长度;输出结构里的 SID 指针指向 buf 内部。
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
            len,
            &mut len,
        )
    };
    if ok.is_err() {
        // SAFETY: 令牌句柄在两条路径上都要关。
        unsafe {
            let _ = CloseHandle(token);
        }
        return Err("读取令牌用户失败".to_string());
    }
    // SAFETY: GetTokenInformation 成功 ⇒ buf 头部是一个合法 TOKEN_USER。
    let sid = unsafe { (&*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    let mut sid_str = PWSTR::null();
    // SAFETY: sid 来自上一步的令牌查询;转换出的字符串由 LocalFree 释放。
    let res = unsafe { ConvertSidToStringSidW(sid, &mut sid_str) };
    let mut out = String::new();
    unsafe {
        if res.is_ok() && !sid_str.is_null() {
            // 转换结果是 NUL 结尾宽串,自己找终止符(PWSTR 无现成的读取方法)。
            let mut n = 0usize;
            while *sid_str.0.add(n) != 0 {
                n += 1;
            }
            out = String::from_utf16_lossy(std::slice::from_raw_parts(sid_str.0, n));
            let _ = LocalFree(HLOCAL(sid_str.0 as *mut std::ffi::c_void));
        }
        let _ = CloseHandle(token);
    }
    if out.is_empty() {
        Err("SID 转换失败".to_string())
    } else {
        Ok(out)
    }
}

/// `schtasks.exe` 的绝对路径(靠 PATH 找在提权环境里不总可靠)。
#[cfg(windows)]
fn schtasks_exe() -> String {
    std::env::var("SystemRoot")
        .map(|root| format!(r"{}\System32\schtasks.exe", root))
        .unwrap_or_else(|_| r"C:\Windows\System32\schtasks.exe".to_string())
}

fn xml_esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 计划任务定义 XML(UTF-16 编码由调用方落盘时处理)。
///
/// `InteractiveToken` + `HighestAvailable`:任务由计划任务服务负责提权,
/// 所以**非提权的本程序调用 `/Run` 不会再看到 UAC 弹窗** —— 这就是
/// "授权一次、以后静默"的机制;同时任务是服务拉起的,不挂控制台。
/// `ExecutionTimeLimit` 兜底,免得子进程挂死后留一个永动任务实例。
pub fn mft_task_xml(sid: &str, exe: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Relyto.Maliang 全盘文件名索引更新(快速查找数据源,只读 NTFS 卷日志)</Description>
  </RegistrationInfo>
  <Triggers />
  <Principals>
    <Principal id="Author">
      <UserId>{sid}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT15M</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{cmd}</Command>
      <Arguments>{args}</Arguments>
    </Exec>
  </Actions>
</Task>"#,
        sid = xml_esc(sid),
        cmd = xml_esc(&exe.to_string_lossy()),
        args = xml_esc(MFT_TASK_ARGS),
    )
}

/// 一次性提权,把索引更新器注册成「按需启动 + 最高权限」的计划任务。
/// 这是本功能唯一需要用户点"是"的地方;之后每次启动都静默。
/// `tmp_dir` 用来落任务定义 XML(schtasks `/XML` 只吃 UTF-16 文件)。
pub fn mft_task_install(exe: &Path, tmp_dir: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let xml = mft_task_xml(&current_user_sid()?, exe);
        let def = tmp_dir.join("mft_task.xml");
        let mut bytes: Vec<u8> = vec![0xFF, 0xFE];
        bytes.extend(xml.encode_utf16().flat_map(|u| u.to_le_bytes()));
        std::fs::write(&def, &bytes).map_err(|e| format!("写任务定义失败: {e}"))?;
        let params = format!("/Create /F /TN {MFT_TASK_NAME} /XML \"{}\"", def.display());
        let res = shell_execute_elevated(&schtasks_exe(), &params, Some(90_000));
        let _ = std::fs::remove_file(&def);
        match res? {
            Some(0) => Ok(()),
            Some(code) => Err(format!("计划任务注册失败(schtasks 退出码 {code})")),
            None => Err("计划任务注册未返回结果".to_string()),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (exe, tmp_dir);
        Err("仅支持 Windows".into())
    }
}

/// 计划任务是否已注册(查询不需要管理员)。
pub fn mft_task_registered() -> bool {
    #[cfg(windows)]
    {
        matches!(
            run_hidden(&schtasks_exe(), &["/Query", "/TN", MFT_TASK_NAME]),
            Ok(0)
        )
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 触发计划任务更新索引:静默(不弹 UAC、不弹窗口),结果由子进程写报告文件。
pub fn mft_task_run() -> Result<(), String> {
    #[cfg(windows)]
    {
        match run_hidden(&schtasks_exe(), &["/Run", "/TN", MFT_TASK_NAME]) {
            Ok(0) => Ok(()),
            Ok(code) => Err(format!("计划任务启动失败(schtasks 退出码 {code})")),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(windows))]
    {
        Err("仅支持 Windows".into())
    }
}

/// 撤销授权 = 删除计划任务(改任务定义要提权,所以这一步仍会弹一次 UAC)。
pub fn mft_task_uninstall() -> Result<(), String> {
    #[cfg(windows)]
    {
        let params = format!("/Delete /F /TN {MFT_TASK_NAME}");
        match shell_execute_elevated(&schtasks_exe(), &params, Some(90_000))? {
            Some(0) => Ok(()),
            Some(code) => Err(format!("计划任务删除失败(schtasks 退出码 {code})")),
            None => Err("计划任务删除未返回结果".to_string()),
        }
    }
    #[cfg(not(windows))]
    {
        Err("仅支持 Windows".into())
    }
}

/// 摘掉进程自己的控制台。
///
/// 被计划任务拉起时(debug 构建是控制台子系统)系统会给一个新控制台,
/// 那就是用户看到的黑框;release 是 GUI 子系统、本来没有控制台,调用无副作用。
pub fn hide_console() {
    #[cfg(windows)]
    {
        // SAFETY: FreeConsole 只解除本进程与控制台的关联,无参数无返回值风险。
        unsafe {
            let _ = windows::Win32::System::Console::FreeConsole();
        }
    }
}

/// 在资源管理器中定位文件(`explorer /select,"路径"`)。
pub fn reveal_in_explorer(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let args = format!("/select,\"{}\"", path.display());
        let file_w = to_wide("explorer.exe");
        let args_w = to_wide(&args);
        // SAFETY: 均为合法 NUL 结尾宽串;动词 "open" 为静态常量。
        let ret = unsafe {
            ShellExecuteW(
                None,
                windows::core::w!("open"),
                PCWSTR(file_w.as_ptr()),
                PCWSTR(args_w.as_ptr()),
                None,
                SW_SHOWNORMAL,
            )
        };
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
        assert!(!matches!(
            t,
            Some(DriveType::Fixed) | Some(DriveType::Removable)
        ));
    }

    #[test]
    fn cpu_count_is_at_least_one() {
        assert!(cpu_count() >= 1);
    }

    #[test]
    fn mft_task_xml_points_at_exe_and_stays_silent_about_privileges() {
        // 任务定义必须:跑当前 exe + 固定的 --mft-scan --mft-task(不带可变的报告
        // 路径,否则数据目录一改就永远等不到信号)+ 最高权限 + 交互登录型。
        let xml = mft_task_xml("S-1-5-21-1&2", Path::new(r"D:\Some Dir\maliang.exe"));
        assert!(xml.contains(r#"<Command>D:\Some Dir\maliang.exe</Command>"#));
        assert!(xml.contains(&format!("<Arguments>{MFT_TASK_ARGS}</Arguments>")));
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        // 非提权调用方要靠它免 UAC,前提就是这条:注册成最高权限的按需任务。
        assert!(xml.contains("<Triggers />"));
        // 身份里的 & 必须转义,否则整份 XML 解析失败(schtasks 报"格式不正确")。
        assert!(xml.contains("S-1-5-21-1&amp;2"));
        assert!(!xml.contains("mft_report"));
    }
}
