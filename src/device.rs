//! 设备热插拔检测(`WM_DEVICECHANGE` 链路,与 Picasa 3 的行为一致)。
//!
//! # 行为要点
//!
//! - 主窗口过程:`WM_DEVICECHANGE`(0x219)直接转交设备热插拔处理。
//! - 设备热插拔处理关注 **3 个 wParam**:
//!   - `7`  = `DBT_DEVNODES_CHANGED` —— 设备节点树变化(**总闸**);
//!   - `0x8000` = `DBT_DEVICEARRIVAL` —— 设备到达;
//!   - `0x8004` = `DBT_DEVICEREMOVECOMPLETE` —— 设备移除完成。
//! - lParam 结构是 `DEV_BROADCAST_DEVICEINTERFACE_W`:
//!   `dbcc_devicetype`(值 `2` = `DBT_DEVTYP_DEVICEINTERFACE`)、
//!   `dbcc_flags`(位掩码,循环右移找首个置位,上限 26 位)。
//! - **不使用 WMI,也不使用 SetupAPI / `RegisterDeviceNotification`**
//!   → 因此本模块**不**做 GUID 过滤注册,只依赖 `DBT_DEVNODES_CHANGED` +
//!   `DBT_DEVTYP_DEVICEINTERFACE` 的粗过滤。
//! - 检测到变化后的动作:置刷新标志并触发刷新。
//!   设备列表重建流程:释放旧数组 → 卷类型分流(可移动磁盘列表)→
//!   设备列表合成(WPD 全量枚举)→ 设备列表排序。
//! - 设备类型是 **3 值枚举**,对应"可移动磁盘 / 相机 / 手机"。
//! - 设备枚举用 **WPD**(`IPortableDevice` + `PROPVARIANT`)。
//! - 本项目**不**引入独立检测进程,后台线程即可。
//!
//! # 本模块的实现范围(诚实声明)
//!
//! - **消息层**:`WM_DEVICECHANGE` + 3 个 wParam +
//!   `dbcc_devicetype == 2` 的粗过滤。
//! - **动作层**:"置刷新标志 + 触发刷新",即
//!   **检测到设备变化 → 触发一次可移动卷重扫**。
//! - **设备列表层**:
//!   - 盘符卷:用 `GetLogicalDrives` + `GetDriveTypeW` 枚举 `DRIVE_REMOVABLE`;
//!   - 便携设备(相机/手机):**已实现**,见 `crate::wpd::devices()`
//!     (`IPortableDeviceManager` 同一 coclass GUID)。
//!   - 两者的合流与降级见 `crate::wpd::devices_unified()`。
//! - **仍未做**:便携设备的**照片传输**(需 WPD 对象流),
//!   UI 已明示并给出「扫描此卷」的替代路径。
//!
//! 设备变化 → 通过 channel 通知 UI → UI 重新扫描新出现的卷。
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, RegisterClassW, TranslateMessage,
    WINDOW_EX_STYLE, WNDCLASSW,
};

/// `WM_DEVICECHANGE`(`0x219`)。
pub const WM_DEVICECHANGE: u32 = 0x0219;
/// `DBT_DEVNODES_CHANGED` —— 设备节点树变化(设备热插拔处理)。
pub const DBT_DEVNODES_CHANGED: u32 = 7;
/// `DBT_DEVICEARRIVAL` —— 设备到达(设备热插拔处理)。
pub const DBT_DEVICEARRIVAL: u32 = 0x8000;
/// `DBT_DEVICEREMOVECOMPLETE` —— 设备移除完成(设备热插拔处理)。
pub const DBT_DEVICEREMOVECOMPLETE: u32 = 0x8004;
/// `DBT_DEVTYP_DEVICEINTERFACE` —— 设备接口类(`dbcc_devicetype == 2`)。
pub const DBT_DEVTYP_DEVICEINTERFACE: u32 = 2;

/// 设备变化事件(对应原版置刷新标志 + 触发刷新的动作)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceEvent {
    /// 设备到达(`DBT_DEVICEARRIVAL` + `DBT_DEVTYP_DEVICEINTERFACE`)。
    Arrived,
    /// 设备移除完成(`DBT_DEVICEREMOVECOMPLETE` + `DBT_DEVTYP_DEVICEINTERFACE`)。
    Removed,
    /// 设备节点树变化(`DBT_DEVNODES_CHANGED`)—— 原版的"总闸"。
    NodesChanged,
}

impl DeviceEvent {
    /// 事件的可读描述(供 UI 状态栏显示)。
    pub fn label(&self) -> &'static str {
        match self {
            DeviceEvent::Arrived => "设备接入",
            DeviceEvent::Removed => "设备移除",
            DeviceEvent::NodesChanged => "设备节点变化",
        }
    }
}

/// 把 `WM_DEVICECHANGE` 的 (wParam, lParam) 翻译成设备事件。
///
/// 翻译规则(与 Picasa 3 行为一致):
///
/// ```text
/// if ((wParam == 0x8000 || wParam == 0x8004) && dbcc_devicetype == 2)  → 到达/移除
/// if (wParam == 7)                                                  → 节点树变化
/// ```
///
/// `dbcc_devicetype` 取自 `lParam + 0x04`(设备热插拔处理)。
/// `lParam` 为 0 时(原版 `DBT_DEVNODES_CHANGED` 就是这样传的)按 0 处理。
pub fn translate_device_change(wparam: u32, lparam: usize) -> Option<DeviceEvent> {
    if wparam == DBT_DEVNODES_CHANGED {
        return Some(DeviceEvent::NodesChanged);
    }
    if wparam != DBT_DEVICEARRIVAL && wparam != DBT_DEVICEREMOVECOMPLETE {
        return None;
    }
    // dbcc_devicetype 在 lParam + 0x04;lParam==0 时不成立。
    if lparam == 0 {
        return None;
    }
    // ⚠️ 原版读的是 **dword**(设备热插拔处理:`*(int *)(lParam + 4) == 2`),
    // 不是 1 字节 —— 早期实现只读 u8,当 devicetype 是 0x102 之类时会误判成 2。
    // 用 `read_unaligned`:lParam 指向的结构体只保证 4 字节对齐,dword 字段可能不对齐。
    let devicetype = unsafe { std::ptr::read_unaligned((lparam + 0x04) as *const i32) };
    if devicetype != DBT_DEVTYP_DEVICEINTERFACE as i32 {
        return None;
    }
    Some(if wparam == DBT_DEVICEARRIVAL {
        DeviceEvent::Arrived
    } else {
        DeviceEvent::Removed
    })
}

/// 枚举当前系统上的**可移动卷**(对应原版卷类型分流的可移动磁盘列表)。
///
/// 原版用 `Removable Drive(%s)` 命名这类设备;本实现用 `GetLogicalDrives`
/// + `GetDriveTypeW` 得到盘符列表,返回形如 `E:\` 的根路径。
pub fn removable_volumes() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    if mask == 0 {
        return out;
    }
    for i in 0..26u32 {
        let bit = 1u32 << i;
        if mask & bit == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root: Vec<u16> = vec![letter as u16, b':' as u16, b'\\' as u16, 0];
        let dtype = unsafe { GetDriveTypeW(windows::core::PCWSTR(root.as_ptr())) };
        // DRIVE_REMOVABLE = 2
        if dtype == 2 {
            out.push(PathBuf::from(format!("{}:\\", letter)));
        }
    }
    out
}

/// 设备热插拔监听器(后台线程 + 隐藏窗口)。
pub struct DeviceWatcher {
    /// 停止标志(置 1 后线程退出)。
    stop: Arc<AtomicBool>,
    /// 事件接收端(UI 轮询)。
    pub rx: mpsc::Receiver<DeviceEvent>,
    handle: Option<std::thread::JoinHandle<()>>,
}

// 隐藏窗口的窗口类名(只用于本进程内的消息窗口)。
const CLASS_NAME: &[u16] = &[
    b'M' as u16, b'a' as u16, b'l' as u16, b'i' as u16, b'a' as u16, b'n' as u16, b'D' as u16,
    b'e' as u16, b'v' as u16, b'i' as u16, b'c' as u16, b'e' as u16, 0,
];

/// 窗口过程:只处理 `WM_DEVICECHANGE`,其余交给 `DefWindowProcW`。
unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_DEVICECHANGE {
        // 事件通过全局 channel 传出(窗口过程拿不到自定义数据指针,
        // 这里用一个进程内单例的 Sender)。
        if let Some(tx) = device_event_sender() {
            if let Some(ev) = translate_device_change(wparam.0 as u32, lparam.0 as usize) {
                let _ = tx.send(ev);
            }
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// 进程内的 Sender(窗口过程只能通过全局状态拿到)。
///
/// 用 `Mutex<Option<Sender>>` 而非 `OnceLock`:后者一旦设置就不可替换,
/// 监听器若被重启(停止后再启动)就会把事件发到**已废弃**的旧 channel。
static EVENT_TX: std::sync::Mutex<Option<mpsc::Sender<DeviceEvent>>> = std::sync::Mutex::new(None);

fn device_event_sender() -> Option<mpsc::Sender<DeviceEvent>> {
    EVENT_TX.lock().ok().and_then(|g| g.clone())
}

/// 设置当前的事件 Sender(返回是否设置成功)。
fn set_device_event_sender(tx: mpsc::Sender<DeviceEvent>) -> bool {
    let mut g = match EVENT_TX.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    *g = Some(tx);
    true
}

/// 启动设备热插拔监听。
///
/// 行为:
/// - 建一个**隐藏窗口**(不显示、不出现在任务栏),接收 `WM_DEVICECHANGE`;
/// - 消息循环在独立线程跑(不阻塞 UI);
/// - 设备事件通过 `rx` 通知 UI。
///
/// 失败时(非 Windows / 建窗失败)返回一个 `rx` 立即关闭的监听器,
/// UI 侧 `try_recv` 得不到任何事件,等于"未启用",不影响其它功能。
pub fn spawn_device_watcher() -> DeviceWatcher {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();

    // 全局 Sender(窗口过程需要);可替换,支持监听器重启。
    set_device_event_sender(tx.clone());

    let handle = std::thread::spawn(move || {
        unsafe {
            let hinst = GetModuleHandleW(None).unwrap_or_default();
            let cls = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: HINSTANCE(hinst.0),
                lpszClassName: windows::core::PCWSTR(CLASS_NAME.as_ptr()),
                ..Default::default()
            };
            let atom = RegisterClassW(&cls);
            if atom == 0 {
                return; // 建类失败 → 静默降级(等价于"未启用")
            }
            // 隐藏窗口:样式为 0 且从不 ShowWindow → 不可见。
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::PCWSTR(CLASS_NAME.as_ptr()),
                windows::core::PCWSTR::null(),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                HINSTANCE(hinst.0),
                None,
            );
            if hwnd.is_err() {
                return;
            }
            // 消息循环。
            let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
            while !stop2.load(Ordering::Relaxed) {
                // 用 PeekMessage 轮询(带小间隔),便于响应 stop 标志。
                let r = PeekMessageW(
                    &mut msg,
                    None,
                    0,
                    0,
                    windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
                );
                if r.as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd.unwrap());
        }
    });

    DeviceWatcher {
        stop,
        rx,
        handle: Some(handle),
    }
}

impl DeviceWatcher {
    /// 停止监听并回收线程。
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        // 清掉全局 Sender:窗口线程已退出,不该再往这个 channel 发事件。
        if let Ok(mut g) = EVENT_TX.lock() {
            *g = None;
        }
    }
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 便捷函数:比较两次可移动卷列表,返回新增的卷根路径。
///
/// 对应原版设备列表重建重建列表后与旧列表合并的动作 —— 本项目只关心
/// "新出现的卷"(新插入的相机/手机的挂载点),因为那才是需要扫描的。
pub fn new_volumes(old: &[PathBuf], new: &[PathBuf]) -> Vec<PathBuf> {
    new.iter().filter(|v| !old.contains(v)).cloned().collect()
}

/// 供 `wnd_proc` 使用的 `c_void` 引用占位(避免未使用导入告警)。
#[allow(dead_code)]
fn _unused_cvoid(_: *const c_void) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// `DBT_DEVNODES_CHANGED` 恒成立(原版的"总闸")。
    #[test]
    fn nodes_changed_always_translates() {
        assert_eq!(
            translate_device_change(DBT_DEVNODES_CHANGED, 0),
            Some(DeviceEvent::NodesChanged)
        );
    }

    /// 到达/移除需要 `dbcc_devicetype == 2`。
    #[test]
    fn arrival_requires_device_interface_type() {
        // 构造一个 lParam:偏移 0x04 处放 dbcc_devicetype = 2(dword)
        let mut buf = [0u8; 32];
        buf[0x04..0x08].copy_from_slice(&(DBT_DEVTYP_DEVICEINTERFACE as i32).to_le_bytes());
        let lp = buf.as_ptr() as usize;
        assert_eq!(
            translate_device_change(DBT_DEVICEARRIVAL, lp),
            Some(DeviceEvent::Arrived)
        );
        assert_eq!(
            translate_device_change(DBT_DEVICEREMOVECOMPLETE, lp),
            Some(DeviceEvent::Removed)
        );

        // 类型不匹配 → 不处理
        let mut buf2 = [0u8; 32];
        buf2[0x04..0x08].copy_from_slice(&5i32.to_le_bytes()); // DBT_DEVTYP_HANDLE
        assert_eq!(translate_device_change(DBT_DEVICEARRIVAL, buf2.as_ptr() as usize), None);
    }

    /// 回归测试:原版读的是 **dword**(`*(int *)(lParam+4)`),
    /// 所以 devicetype = 0x102 时**不能**被误判成 2
    /// (早期实现只读 1 字节,低字节恰好是 0x02 → 会误判为"设备接口")。
    #[test]
    fn devicetype_is_read_as_dword_not_byte() {
        let mut buf = [0u8; 32];
        // 0x102 的小端表示:低字节 0x02,次字节 0x01
        buf[0x04..0x08].copy_from_slice(&0x102i32.to_le_bytes());
        assert_eq!(
            translate_device_change(DBT_DEVICEARRIVAL, buf.as_ptr() as usize),
            None,
            "0x102 不等于 2,必须被拒绝(只读低字节会误判)"
        );
        // 真正的 2 必须放行
        let mut buf2 = [0u8; 32];
        buf2[0x04..0x08].copy_from_slice(&2i32.to_le_bytes());
        assert_eq!(
            translate_device_change(DBT_DEVICEARRIVAL, buf2.as_ptr() as usize),
            Some(DeviceEvent::Arrived)
        );
    }

    /// 其它 wParam 一律不处理。
    #[test]
    fn other_wparams_are_ignored() {
        assert_eq!(translate_device_change(0x0200, 0), None);
        assert_eq!(translate_device_change(0x0007 - 1, 0), None);
    }

    /// lParam == 0 时(原版 `DBT_DEVNODES_CHANGED` 就是这样传的)不崩溃。
    #[test]
    fn zero_lparam_is_safe() {
        assert_eq!(translate_device_change(DBT_DEVICEARRIVAL, 0), None);
        assert_eq!(translate_device_change(DBT_DEVICEREMOVECOMPLETE, 0), None);
    }

    /// 新卷识别。
    #[test]
    fn new_volumes_detects_added_roots() {
        let old = vec![PathBuf::from("E:\\"), PathBuf::from("F:\\")];
        let new = vec![PathBuf::from("E:\\"), PathBuf::from("G:\\")];
        let added = new_volumes(&old, &new);
        assert_eq!(added, vec![PathBuf::from("G:\\")]);
        assert!(new_volumes(&old, &old).is_empty());
    }

    /// 可移动卷枚举不应崩溃(有无可移动设备都能返回)。
    #[test]
    fn removable_volumes_list_is_sane() {
        let v = removable_volumes();
        for p in &v {
            assert!(p.to_string_lossy().ends_with('\\'));
            assert_eq!(p.to_string_lossy().len(), 3, "应为形如 X:\\ 的根路径");
        }
    }
}
