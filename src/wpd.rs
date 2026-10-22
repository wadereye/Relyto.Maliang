//! WPD(Windows Portable Devices)设备枚举 —— 对齐原版「从设备导入」。
//!
//! # 参考来源：Picasa 3 的设备导入（WPD）
//!
//! 本模块对 WPD 的使用**全部通过 GUID 常量 + `CoCreateInstance`**,不依赖
//! 任何名字字符串。所用 GUID 为 Windows 公开常量:
//!
//! | COM 对象 | GUID | 对应环节 |
//! |---|---|---|
//! | CLSID_PortableDeviceManager | `{0AF10CEC-2ECD-4B92-9581-34F6AE0637F3}` | WPD 管理器 |
//! | IID_IPortableDeviceManager | `{A1567595-4C2F-4574-A6FA-ECEF917B9A40}` | 同上 |
//! | CLSID_PortableDeviceValues | `{0C15D503-D017-47CE-9016-7B3F978721CC}` | WPD 属性集 |
//! | CLSID_PortableDevice | `{F7C0039A-4762-488A-B4B3-760EF9A1BA9B}` | WPD 设备对象 |
//!
//! 关键行为:
//! - **客户端标识**(WPD 属性集):创建 `IPortableDeviceValues` 后,
//!   SetStringValue 写 `WPD_CLIENT_*` 键族(fmtid `{204D9F0C-2292-4080-9F42-40664E70F859}`):
//!   pid=2 ← 客户端名(本项目用 `Maliang`),pid=3/4/5 ← 版本号,
//!   pid=8 ← 0x20000(WPD_CLIENT_SECURITY_QUALITY_OF_SERVICE)。
//! - **双栈并存**(WPD 设备枚举):WIA DevMgr 与 WPD Manager **都**创建,
//!   WPD 失败仅清标志不中断 —— 即 WPD 是**尽力而为**,不是硬依赖。
//! - **统一列表**(设备列表合成):第一遍迭代 WPD 设备(类型标 3/2),
//!   第二遍迭代 WIA 设备,两类构造成同一种设备记录追加进同一数组。
//! - **盘符卷是另一条通道**:与便携设备列表是否同列表渲染:未确认。
//! - **Open 权限失败重试**(设备打开重试):E_ACCESSDENIED(0x80070005)→
//!   把 pid=9 的 SECURITY_QOS 改 0x80000000 后重试。
//! - **文件传输走 WIA 通道**:QI `IWiaDataTransfer` + 临时目录。
//!
//! # 我方实现范围(诚实声明)
//!
//! 1. **设备枚举**(本文件):`IPortableDeviceManager::GetDevices` + 每设备的
//!    FriendlyName/Description/Manufacturer —— 对齐WPD 管理器/WPD 设备信息。
//! 2. **客户端信息**:沿用 fmtid/pid 结构,name 换成 `Maliang`,版本号用我方 0.1。
//! 3. **统一列表**:WPD 设备 + 可移动盘符卷合成一个列表。
//! 4. **不做文件传输**:本项目无 WIA;真要拷文件时走
//!    `IPortableDeviceResources::GetStream`——
//!    当前 UI 只展示设备,传输留待后续任务,不做无依据的推测。
//! 5. 非 Windows 平台全部返回空列表(无操作退化,同 `win.rs` 惯例)。

#[cfg(windows)]
mod imp {
    use anyhow::{anyhow, Result};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Devices::PortableDevices::{
        IPortableDeviceManager, IPortableDeviceValues, PortableDeviceManager,
        PortableDeviceValues,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

    /// `WPD_CLIENT_*` 键族的 fmtid
    /// (WPD 属性集,WPD_CLIENT_NAME/_MAJOR_VERSION/... 共用此 fmtid)。
    const WPD_CLIENT_FMTID: windows::core::GUID =
        windows::core::GUID::from_u128(0x204d_9f0c_2292_4080_9f42_4066_4e70_f859);

    const CLIENT_NAME: &str = "Maliang";

    /// 构造客户端信息 `IPortableDeviceValues`。
    ///
    /// 对齐WPD 属性集:
    /// - pid 2 = WPD_CLIENT_NAME(字符串)
    /// - pid 3 = major / pid 4 = minor / pid 5 = revision(整数)
    /// - pid 8 = WPD_CLIENT_SECURITY_QUALITY_OF_SERVICE = 0x20000
    ///
    /// (coclass GUID 与原版一致:`PortableDeviceValues` = `{0C15D503-...}`,
    /// windows crate 常量 `0x0c15d503_d017_47ce_9016_7b3f978721cc` 吻合。)
    pub fn client_values() -> Result<IPortableDeviceValues> {
        unsafe {
            let v: IPortableDeviceValues =
                CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)?;
            let key = |pid: u32| PROPERTYKEY {
                fmtid: WPD_CLIENT_FMTID,
                pid,
            };
            v.SetStringValue(&key(2), &windows::core::HSTRING::from(CLIENT_NAME))?;
            v.SetUnsignedIntegerValue(&key(3), 0)?; // major
            v.SetUnsignedIntegerValue(&key(4), 1)?; // minor
            v.SetUnsignedIntegerValue(&key(5), 0)?; // revision
            v.SetUnsignedIntegerValue(&key(8), 0x2000_0)?; // SECURITY_QOS
            Ok(v)
        }
    }

    /// 一台便携设备(MTP/PTP 相机、手机等)。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct WpdDevice {
        /// 设备 ID(WPD 的 PnP 串,如 `\\?\usb#vid_...`)。
        pub device_id: String,
        /// 友好名(原版 `GetDeviceFriendlyName`;失败回落设备 ID)。
        pub friendly_name: String,
        /// 厂商描述(可能为空)。
        pub description: String,
        /// 制造商(可能为空)。
        pub manufacturer: String,
    }

    /// 枚举全部便携设备。
    ///
    /// 对齐WPD 管理器(创建 Manager)+ WPD 设备信息(迭代设备,
    /// 设备 ID/名称经 `WideCharToMultiByte(CP_UTF8)` 转多字节 —— 我方直接
    /// 转 UTF-8 String,等价)。coclass GUID 与原版一致:
    /// `PortableDeviceManager` = `{0AF10CEC-2ECD-4B92-9581-34F6AE0637F3}`
    /// (crate 常量 `0x0af10cec_2ecd_4b92_9581_34f6ae0637f3` 吻合)。
    ///
    /// **双栈语义**:失败返回 `Err`,调用方(`devices_unified`)捕获后继续用
    /// 盘符卷 —— 对齐原版「WPD 失败仅清标志,不中断」(WPD 设备枚举)。
    pub fn devices() -> Result<Vec<WpdDevice>> {
        let _ = client_values()?; // 与原版一致:创建 Manager 前先构造 client info
        unsafe {
            let manager: IPortableDeviceManager =
                CoCreateInstance(&PortableDeviceManager, None, CLSCTX_INPROC_SERVER)?;
            // 第一次调用拿数量(原版同款两段式:GetDevices(NULL,&n) → 分配 → 再调)
            let mut count: u32 = 0;
            manager
                .GetDevices(std::ptr::null_mut(), &mut count)
                .map_err(|e| anyhow!("WPD GetDevices(计数)失败: {e}"))?;
            if count == 0 {
                return Ok(Vec::new());
            }
            let mut buf = vec![PWSTR::null(); count as usize];
            manager
                .GetDevices(buf.as_mut_ptr(), &mut count)
                .map_err(|e| anyhow!("WPD GetDevices(取值)失败: {e}"))?;
            let ids: Vec<String> = buf
                .into_iter()
                .take(count as usize)
                .map(take_pwstr)
                .collect();

            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                // 友好名:原版WPD 设备信息里设备 ID 与名称都转 UTF-8;
                // 名称取不到时回落设备 ID(与原版「名称为空仍列出」一致)。
                let friendly =
                    query_string(&manager, &id, 0).unwrap_or_else(|| id.clone());
                let description = query_string(&manager, &id, 1).unwrap_or_default();
                let manufacturer = query_string(&manager, &id, 2).unwrap_or_default();
                out.push(WpdDevice {
                    device_id: id,
                    friendly_name: friendly,
                    description,
                    manufacturer,
                });
            }
            Ok(out)
        }
    }

    /// 按 `GetDeviceFriendlyName`(what=0)/`GetDeviceDescription`(1)/
    /// `GetDeviceManufacturer`(2)查询字符串(两段式:先拿长度再取值)。
    fn query_string(m: &IPortableDeviceManager, id: &str, what: u8) -> Option<String> {
        let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let id = PCWSTR(wide.as_ptr());
            let mut len: u32 = 0;
            let hr = match what {
                0 => m.GetDeviceFriendlyName(id, PWSTR::null(), &mut len),
                1 => m.GetDeviceDescription(id, PWSTR::null(), &mut len),
                _ => m.GetDeviceManufacturer(id, PWSTR::null(), &mut len),
            };
            if hr.is_err() || len == 0 {
                return None;
            }
            let mut buf = vec![0u16; len as usize];
            let hr = match what {
                0 => m.GetDeviceFriendlyName(id, PWSTR(buf.as_mut_ptr()), &mut len),
                1 => m.GetDeviceDescription(id, PWSTR(buf.as_mut_ptr()), &mut len),
                _ => m.GetDeviceManufacturer(id, PWSTR(buf.as_mut_ptr()), &mut len),
            };
            if hr.is_err() {
                return None;
            }
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..end]))
        }
    }

    /// 取走 CoTaskMemAlloc 的宽字符串并释放(原版同款:对象 ID 是
    /// CoTaskMem 分配的宽字符串,见宽字符串分配的
    /// `Next(...) + CoTaskMemFree`)。
    fn take_pwstr(p: PWSTR) -> String {
        unsafe {
            if p.0.is_null() {
                return String::new();
            }
            let mut len = 0usize;
            while *p.0.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(p.0, len);
            let s = String::from_utf16_lossy(slice);
            CoTaskMemFree(Some(p.0.cast()));
            s
        }
    }

    /// 供测试:GUID 常量正确性(防手抖写错 fmtid)。
    #[cfg(test)]
    pub(crate) fn client_fmtid_matches_original() -> bool {
        WPD_CLIENT_FMTID == windows::core::GUID::from_u128(0x204d_9f0c_2292_4080_9f42_4066_4e70_f859)
    }
}

#[cfg(windows)]
pub use imp::WpdDevice;

/// 统一设备列表:WPD 设备 + 可移动盘符卷。
///
/// 对齐原版「WPD + WIA 统一列表」(设备列表合成):
/// 两类来源构造成**同一种记录**追加进同一数组。我方用
/// `DeviceEntry` 替代原版的设备信息结构,盘符卷替代原版 WIA 通道。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEntry {
    /// WPD/MTP 便携设备(相机、手机)。
    Portable(WpdDevice),
    /// 可移动盘符卷(U 盘、SD 卡读卡器 —— 原版的可移动盘检测通道)。
    Volume(std::path::PathBuf),
}

impl DeviceEntry {
    #[must_use]
    pub fn display_name(&self) -> String {
        match self {
            Self::Portable(d) => {
                if d.manufacturer.is_empty() {
                    d.friendly_name.clone()
                } else {
                    format!("{} ({})", d.friendly_name, d.manufacturer)
                }
            }
            Self::Volume(p) => {
                let s = p.to_string_lossy();
                match crate::win::volume_label(p) {
                    Some(label) if !label.is_empty() => format!("{s} — {label}"),
                    _ => s.to_string(),
                }
            }
        }
    }

    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Portable(_) => "便携设备",
            Self::Volume(_) => "可移动磁盘",
        }
    }
}

/// 全部可导入设备(WPD 失败不中断,尽力而为 —— 对齐WPD 设备枚举)。
#[must_use]
pub fn devices_unified() -> Vec<DeviceEntry> {
    let mut out = Vec::new();
    // WPD 通道:失败仅静默(原版就是清标志继续)
    #[cfg(windows)]
    match imp::devices() {
        Ok(ds) => out.extend(ds.into_iter().map(DeviceEntry::Portable)),
        Err(_) => {}
    }
    // 盘符卷通道(对应原版的可移动盘检测)
    out.extend(
        crate::device::removable_volumes()
            .into_iter()
            .map(DeviceEntry::Volume),
    );
    out
}

#[cfg(not(windows))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WpdDevice {
    pub device_id: String,
    pub friendly_name: String,
    pub description: String,
    pub manufacturer: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn client_fmtid_is_original_value() {
        // WPD 属性集的 fmtid {204D9F0C-2292-4080-9F42-40664E70F859}
        assert!(imp::client_fmtid_matches_original());
    }

    #[test]
    #[cfg(windows)]
    fn enumerate_devices_does_not_panic() {
        // 真机环境可能一台设备都没有;本测试只验证 COM 链路不崩、
        // 返回值合法(空列表或若干设备)。对齐原版「WPD 尽力而为」。
        let r = devices_unified();
        let n = r.len();
        for e in &r {
            assert!(!e.display_name().is_empty());
            assert!(!e.kind().is_empty());
        }
        let _ = n;
    }

    #[test]
    fn unified_list_kind_labels_are_stable() {
        let p = DeviceEntry::Portable(WpdDevice {
            device_id: "\\\\?\\usb#test".into(),
            friendly_name: "测试相机".into(),
            description: String::new(),
            manufacturer: "TestCorp".into(),
        });
        assert_eq!(p.kind(), "便携设备");
        assert!(p.display_name().contains("测试相机"));
        assert!(p.display_name().contains("TestCorp"));
    }

    #[test]
    fn portable_display_without_manufacturer() {
        let p = DeviceEntry::Portable(WpdDevice {
            device_id: "x".into(),
            friendly_name: "Cam".into(),
            description: String::new(),
            manufacturer: String::new(),
        });
        assert_eq!(p.display_name(), "Cam");
    }
}