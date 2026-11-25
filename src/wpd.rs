//! WPD 便携设备照片枚举与复制导入。
//!
//! | 用途 | 公开 SDK 值 |
//! |---|---|
//! | PortableDeviceManager CLSID | `{0AF10CEC-2ECD-4B92-9581-34F6AE0637F3}` |
//! | PortableDevice CLSID | `{F7C0039A-4762-488A-B4B3-760EF9A1BA9B}` |
//! | PortableDeviceValues CLSID | `{0C15D503-D017-47CE-9016-7B3F978721CC}` |
//! | WPD_CLIENT_* fmtid | `{204D9F0C-2292-4080-9F42-40664E70F859}` |
//! | WPD_OBJECT_PROPERTIES_V1 fmtid | `{EF6B490D-5CD8-437A-AFFC-DA8B60EE4A3C}` |
//! | 根对象 WPD_DEVICE_OBJECT_ID | `L"DEVICE"` |
//! | WPD_OBJECT_NAME / CONTENT_TYPE / SIZE pid | `4 / 7 / 11` |
//! | WPD_CONTENT_TYPE_IMAGE | `{EF2107D5-A52A-4243-A26B-62D4176D7603}` |
//! | WPD_CONTENT_TYPE_FOLDER | `{27E2E392-A111-48E0-AB0C-E17705A05F85}` |
//! | WPD_RESOURCE_DEFAULT fmtid + pid | `{E81E79BE-34F0-41BF-B53F-F1A06AE87842}` + `0` |

use anyhow::Result;
use std::path::{Path, PathBuf};

/// 清洗设备文件名并在目标目录内避开已有文件。
pub fn unique_dest(dir: &Path, name: &str, exists: &dyn Fn(&Path) -> bool) -> PathBuf {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let sanitized = sanitized.trim_end_matches([' ', '.']);
    let filename = if sanitized.is_empty() {
        "photo.jpg"
    } else {
        sanitized
    };
    let path = Path::new(filename);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(filename);
    let suffix = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| format!(".{s}"))
        .unwrap_or_default();
    let mut sequence = 0;
    loop {
        let candidate = if sequence == 0 {
            filename.to_string()
        } else {
            format!("{stem}({sequence}){suffix}")
        };
        let dest = dir.join(candidate);
        if !exists(&dest) {
            return dest;
        }
        sequence += 1;
    }
}

#[cfg(windows)]
mod imp {
    use super::{unique_dest, WpdObject};
    use crate::i18n::{t, t_with};
    use anyhow::{anyhow, bail, Result};
    use std::collections::HashSet;
    use std::fs::{File, OpenOptions};
    use std::io::{ErrorKind, Write};
    use std::path::{Path, PathBuf};
    use windows::core::{GUID, HRESULT, HSTRING, PCWSTR, PWSTR};
    use windows::Win32::Devices::PortableDevices::{
        IPortableDevice, IPortableDeviceContent, IPortableDeviceManager, IPortableDeviceValues,
        PortableDevice, PortableDeviceManager, PortableDeviceValues, WPD_DEVICE_OBJECT_ID,
    };
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, RPC_E_CHANGED_MODE};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IStream,
        COINIT_APARTMENTTHREADED, CLSCTX_INPROC_SERVER, STGM_READ,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

    /// WPD_CLIENT_* 键族的 fmtid。
    const WPD_CLIENT_FMTID: GUID = GUID::from_u128(0x204d_9f0c_2292_4080_9f42_4066_4e70_f859);
    /// 通用对象属性的 fmtid。
    const WPD_OBJECT_PROPERTIES_V1: GUID =
        GUID::from_u128(0xef6b_490d_5cd8_437a_affc_da8b_60ee_4a3c);
    /// 对象名称属性键。
    const WPD_OBJECT_NAME: PROPERTYKEY = PROPERTYKEY {
        fmtid: WPD_OBJECT_PROPERTIES_V1,
        pid: 4,
    };
    /// 对象内容类型属性键。
    const WPD_OBJECT_CONTENT_TYPE: PROPERTYKEY = PROPERTYKEY {
        fmtid: WPD_OBJECT_PROPERTIES_V1,
        pid: 7,
    };
    /// 对象字节大小属性键。
    const WPD_OBJECT_SIZE: PROPERTYKEY = PROPERTYKEY {
        fmtid: WPD_OBJECT_PROPERTIES_V1,
        pid: 11,
    };
    /// 图像对象的内容类型。
    const WPD_CONTENT_TYPE_IMAGE: GUID =
        GUID::from_u128(0xef2107d5_a52a_4243_a26b_62d4176_d7603);
    /// 文件夹对象的内容类型。
    const WPD_CONTENT_TYPE_FOLDER: GUID =
        GUID::from_u128(0x27e2e392_a111_48e0_ab0c_e17705a05f85);
    /// 默认对象数据资源的 fmtid。
    const WPD_RESOURCE_FMTID: GUID =
        GUID::from_u128(0xe81e79be_34f0_41bf_b53f_f1a06ae87842);
    /// 默认对象数据资源键。
    const WPD_RESOURCE_DEFAULT: PROPERTYKEY = PROPERTYKEY {
        fmtid: WPD_RESOURCE_FMTID,
        pid: 0,
    };
    /// WPD 客户端名称。
    const CLIENT_NAME: &str = "Maliang";

    /// 构造 WPD 客户端信息属性集。
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
        /// 友好名(`GetDeviceFriendlyName`;失败回落设备 ID)。
        pub friendly_name: String,
        /// 厂商描述(可能为空)。
        pub description: String,
        /// 制造商(可能为空)。
        pub manufacturer: String,
    }

    /// 为当前 WPD 工作线程初始化并配平 COM。
    fn with_com<T>(work: impl FnOnce() -> Result<T>) -> Result<T> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if hr.is_err() && hr != RPC_E_CHANGED_MODE {
                return Err(windows::core::Error::from(hr).into());
            }
            let initialized = hr.is_ok();
            let result = work();
            if initialized {
                CoUninitialize();
            }
            result
        }
    }

    /// 打开便携设备,仅在访问被拒时降低客户端安全等级重试。
    fn open(device_id: &str) -> Result<IPortableDevice> {
        unsafe {
            let device: IPortableDevice =
                CoCreateInstance(&PortableDevice, None, CLSCTX_INPROC_SERVER)?;
            let client = client_values()?;
            let id = HSTRING::from(device_id);
            if let Err(original) = device.Open(&id, &client) {
                if original.code() != HRESULT::from_win32(ERROR_ACCESS_DENIED.0) {
                    return Err(original.into());
                }
                let fallback = PROPERTYKEY {
                    fmtid: WPD_CLIENT_FMTID,
                    pid: 9,
                };
                client.SetUnsignedIntegerValue(&fallback, 0x8000_0000)?;
                device
                    .Open(&id, &client)
                    .map_err(|e| {
                    anyhow!(
                        "{}",
                        t_with("重试打开便携设备失败:{e}", &[("e", e.to_string())])
                    )
                })?;
            }
            Ok(device)
        }
    }

    /// 在设备打开期间执行内容库操作。
    fn with_device<T>(
        device_id: &str,
        work: impl FnOnce(&IPortableDevice, &IPortableDeviceContent) -> Result<T>,
    ) -> Result<T> {
        with_com(|| {
            let device = open(device_id)?;
            let content = unsafe { device.Content()? };
            let result = work(&device, &content);
            drop(content);
            let _ = unsafe { device.Close() };
            result
        })
    }

    /// 枚举全部便携设备。
    pub fn devices() -> Result<Vec<WpdDevice>> {
        with_com(|| {
            let _ = client_values()?;
            unsafe {
                let manager: IPortableDeviceManager =
                    CoCreateInstance(&PortableDeviceManager, None, CLSCTX_INPROC_SERVER)?;
                let mut count: u32 = 0;
                manager
                    .GetDevices(std::ptr::null_mut(), &mut count)
                    .map_err(|e| {
                        anyhow!(
                            "{}",
                            t_with("WPD GetDevices(计数)失败:{e}", &[("e", e.to_string())])
                        )
                    })?;
                if count == 0 {
                    return Ok(Vec::new());
                }
                let mut buf = vec![PWSTR::null(); count as usize];
                manager
                    .GetDevices(buf.as_mut_ptr(), &mut count)
                    .map_err(|e| {
                        anyhow!(
                            "{}",
                            t_with("WPD GetDevices(取值)失败:{e}", &[("e", e.to_string())])
                        )
                    })?;
                let ids: Vec<String> = buf
                    .into_iter()
                    .take(count as usize)
                    .map(take_pwstr)
                    .collect();

                let mut out = Vec::with_capacity(ids.len());
                for id in ids {
                    let friendly = query_string(&manager, &id, 0).unwrap_or_else(|| id.clone());
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
        })
    }

    /// 按设备管理器接口查询名称、描述或厂商字符串。
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

    /// 取走并释放 CoTaskMemAlloc 分配的宽字符串。
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

    /// 枚举一个内容库节点的直接子对象 ID。
    fn children(content: &IPortableDeviceContent, parent: PCWSTR) -> Result<Vec<String>> {
        unsafe {
            let objects = content.EnumObjects(0, parent, None::<&IPortableDeviceValues>)?;
            let mut out = Vec::new();
            loop {
                let mut ids = [PWSTR::null()];
                let mut fetched = 0u32;
                let hr = objects.Next(&mut ids, &mut fetched);
                if fetched > 0 {
                    out.push(take_pwstr(ids[0]));
                } else if !ids[0].0.is_null() {
                    CoTaskMemFree(Some(ids[0].0.cast()));
                }
                if hr.is_err() {
                    return Err(windows::core::Error::from(hr).into());
                }
                if fetched == 0 {
                    break;
                }
            }
            Ok(out)
        }
    }

    /// 读取对象支持的属性,缺失属性由调用方回落默认值。
    fn object_values(
        properties: &windows::Win32::Devices::PortableDevices::IPortableDeviceProperties,
        object_id: &str,
    ) -> Result<IPortableDeviceValues> {
        let id = HSTRING::from(object_id);
        unsafe {
            let keys = properties.GetSupportedProperties(&id)?;
            properties
                .GetValues(&id, &keys)
                .map_err(|e| {
                    anyhow!(
                        "{}",
                        t_with("读取对象属性失败:{e}", &[("e", e.to_string())])
                    )
                })
        }
    }

    /// 读取对象字符串属性,缺失时返回空串。
    fn string_value(values: &IPortableDeviceValues, key: &PROPERTYKEY) -> String {
        unsafe {
            values
                .GetStringValue(key)
                .map(take_pwstr)
                .unwrap_or_default()
        }
    }

    /// 递归收集图像对象并避免容器循环导致重复枚举。
    fn collect_pictures(
        content: &IPortableDeviceContent,
        parent: PCWSTR,
        depth: u32,
        visited: &mut HashSet<String>,
        out: &mut Vec<WpdObject>,
    ) -> Result<()> {
        if depth > 32 {
            bail!("{}", t("设备目录层级超过 32 层,已停止枚举"));
        }
        for id in children(content, parent)? {
            if !visited.insert(id.clone()) {
                continue;
            }
            let properties = unsafe { content.Properties()? };
            let values = object_values(&properties, &id)?;
            let content_type = unsafe {
                values
                    .GetGuidValue(&WPD_OBJECT_CONTENT_TYPE)
                    .unwrap_or_else(|_| GUID::from_u128(0))
            };
            if content_type == WPD_CONTENT_TYPE_IMAGE {
                let name = string_value(&values, &WPD_OBJECT_NAME);
                let size = unsafe {
                    values
                        .GetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE)
                        .unwrap_or(0)
                };
                out.push(WpdObject {
                    id,
                    name: if name.is_empty() {
                        "photo.jpg".into()
                    } else {
                        name
                    },
                    size,
                    is_image: true,
                });
            } else if content_type == WPD_CONTENT_TYPE_FOLDER {
                let parent = HSTRING::from(&id);
                collect_pictures(
                    content,
                    PCWSTR(parent.as_wide().as_ptr()),
                    depth + 1,
                    visited,
                    out,
                )?;
            }
        }
        Ok(())
    }

    /// 枚举设备内容库中的全部照片。
    pub fn list_pictures(device_id: &str) -> Result<Vec<WpdObject>> {
        with_device(device_id, |_, content| {
            let mut out = Vec::new();
            collect_pictures(
                content,
                WPD_DEVICE_OBJECT_ID,
                0,
                &mut HashSet::new(),
                &mut out,
            )?;
            Ok(out)
        })
    }

    /// 在目标目录创建本次传输独占的临时文件。
    fn create_temp(dir: &Path) -> Result<(PathBuf, File)> {
        for sequence in 0..10_000 {
            let path = dir.join(format!(".maliang-{}-{sequence}.part", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((path, file)),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        bail!("{}", t("无法创建设备导入临时文件"))
    }

    /// 读取照片数据流,完整写入临时文件后再改为最终文件名。
    pub fn fetch(device_id: &str, object_id: &str, dest_dir: &Path) -> Result<PathBuf> {
        with_device(device_id, |_, content| {
            let properties = unsafe { content.Properties()? };
            let values = object_values(&properties, object_id)?;
            let content_type = unsafe {
                values
                    .GetGuidValue(&WPD_OBJECT_CONTENT_TYPE)
                    .unwrap_or_else(|_| GUID::from_u128(0))
            };
            if content_type != WPD_CONTENT_TYPE_IMAGE {
                bail!("{}", t("所选对象不是照片"));
            }
            let name = string_value(&values, &WPD_OBJECT_NAME);
            std::fs::create_dir_all(dest_dir)?;
            let dest = unique_dest(dest_dir, &name, &|path| path.exists());
            let (temp, mut file) = create_temp(dest_dir)?;
            let result = (|| -> Result<u64> {
                let resources = unsafe { content.Transfer()? };
                let mut optimal = 0u32;
                let mut stream: Option<IStream> = None;
                unsafe {
                    resources.GetStream(
                        &HSTRING::from(object_id),
                        &WPD_RESOURCE_DEFAULT,
                        STGM_READ.0,
                        &mut optimal,
                        &mut stream,
                    )?;
                }
                let stream = stream
                    .ok_or_else(|| anyhow!("{}", t("设备未返回照片数据流")))?;
                let mut buffer = [0u8; 64 * 1024];
                let mut total = 0u64;
                loop {
                    let mut read = 0u32;
                    unsafe {
                        stream
                            .Read(buffer.as_mut_ptr().cast(), buffer.len() as u32, Some(&mut read))
                            .ok()?;
                    }
                    if read == 0 {
                        break;
                    }
                    file.write_all(&buffer[..read as usize])?;
                    total += u64::from(read);
                }
                if total == 0 {
                    bail!("{}", t("设备返回的照片数据为空"));
                }
                file.flush()?;
                drop(stream);
                drop(file);
                if dest.exists() {
                    bail!(
                        "{}",
                        t_with(
                            "目标文件已存在:{p1}",
                            &[("p1", dest.display().to_string())]
                        )
                    );
                }
                std::fs::rename(&temp, &dest)?;
                Ok(total)
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temp);
            }
            result?;
            Ok(dest)
        })
    }

    /// 供测试:GUID 常量正确性(防手抖写错 fmtid)。
    #[cfg(test)]
    pub(crate) fn client_fmtid_matches_original() -> bool {
        WPD_CLIENT_FMTID
            == windows::core::GUID::from_u128(0x204d_9f0c_2292_4080_9f42_4066_4e70_f859)
    }
}

/// 便携设备内容库中的一张照片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WpdObject {
    /// 设备内容库对象 ID。
    pub id: String,
    /// 设备提供的文件名。
    pub name: String,
    /// 设备声明的字节大小。
    pub size: u64,
    /// 是否属于图像内容类型。
    pub is_image: bool,
}

#[cfg(windows)]
pub use imp::WpdDevice;

/// 只枚举 WPD 便携设备,不混入盘符卷。
pub fn portable_devices() -> Result<Vec<WpdDevice>> {
    #[cfg(windows)]
    {
        imp::devices()
    }
    #[cfg(not(windows))]
    {
        Err(anyhow::anyhow!("{}", crate::i18n::t("当前平台不支持 WPD 便携设备")))
    }
}

/// 枚举指定便携设备中的照片。
pub fn list_pictures(device_id: &str) -> Result<Vec<WpdObject>> {
    #[cfg(windows)]
    {
        imp::list_pictures(device_id)
    }
    #[cfg(not(windows))]
    {
        let _ = device_id;
        Err(anyhow::anyhow!("{}", crate::i18n::t("当前平台不支持 WPD 便携设备")))
    }
}

/// 把指定设备照片复制到用户所选目录并返回最终路径。
pub fn fetch(device_id: &str, object_id: &str, dest_dir: &Path) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        imp::fetch(device_id, object_id, dest_dir)
    }
    #[cfg(not(windows))]
    {
        let _ = (device_id, object_id, dest_dir);
        Err(anyhow::anyhow!("{}", crate::i18n::t("当前平台不支持 WPD 便携设备")))
    }
}

/// 统一设备列表:WPD 设备 + 可移动盘符卷。
///
/// 设备列表合成:两类来源构造成**同一种记录**追加进同一数组,
/// WPD 设备与盘符卷统一由 `DeviceEntry` 表示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEntry {
    /// WPD/MTP 便携设备(相机、手机)。
    Portable(WpdDevice),
    /// 可移动盘符卷(U 盘、SD 卡读卡器)。
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

/// 全部可导入设备(WPD 失败不中断,尽力而为)。
#[must_use]
pub fn devices_unified() -> Vec<DeviceEntry> {
    let mut out = Vec::new();
    // WPD 通道:失败仅静默(清标志继续)
    #[cfg(windows)]
    match imp::devices() {
        Ok(ds) => out.extend(ds.into_iter().map(DeviceEntry::Portable)),
        Err(_) => {}
    }
    // 盘符卷通道(可移动盘检测)
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
    use std::path::{Path, PathBuf};

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
        // 返回值合法(空列表或若干设备);WPD 尽力而为。
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

    #[test]
    fn unique_dest_sanitizes_name_and_adds_sequence() {
        let dir = Path::new("D:/x");
        let exists = |p: &Path| {
            p == dir.join("a.jpg") || p.to_string_lossy().ends_with("(1).jpg")
        };
        assert_eq!(
            unique_dest(dir, "a.jpg", &exists),
            PathBuf::from("D:/x/a(2).jpg")
        );
        assert_eq!(
            unique_dest(dir, "a:b*?.jpg", &|_| false),
            PathBuf::from("D:/x/a_b__.jpg")
        );
        assert_eq!(
            unique_dest(dir, "", &|_| false),
            PathBuf::from("D:/x/photo.jpg")
        );
    }

    #[test]
    fn unique_dest_never_escapes_the_selected_directory() {
        let dir = Path::new("D:/x");
        assert_eq!(
            unique_dest(dir, "../a/\\b?.jpg", &|_| false),
            PathBuf::from("D:/x/.._a__b_.jpg")
        );
        assert_eq!(
            unique_dest(dir, ".", &|p| p.ends_with("photo.jpg")),
            PathBuf::from("D:/x/photo(1).jpg")
        );
    }
}
