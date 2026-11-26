//! 应用内视频解码播放(Windows Media Foundation,仅视频帧;音频未接入)。
//!
//! 用 MF SourceReader 把视频流解成 RGB32 帧,供 egui 纹理显示。
//! 失败时调用方应回退到系统播放器。

#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(windows)]
mod imp {
    use std::path::Path;
    use std::sync::Once;
    use windows::core::{GUID, PCWSTR, PROPVARIANT};
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::StructuredStorage::PropVariantToInt64;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    static MF_INIT: Once = Once::new();
    static mut MF_OK: bool = false;

    fn ensure_mf() -> bool {
        MF_INIT.call_once(|| unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let hr = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET);
            MF_OK = hr.is_ok();
        });
        unsafe { MF_OK }
    }

    pub struct VideoPlayer {
        reader: IMFSourceReader,
        width: u32,
        height: u32,
        duration_100ns: i64,
        frame_rate: f64,
        cur_100ns: i64,
        ended: bool,
    }

    unsafe impl Send for VideoPlayer {}

    fn unpack_u64(v: u64) -> (u32, u32) {
        ((v >> 32) as u32, (v & 0xFFFF_FFFF) as u32)
    }

    impl VideoPlayer {
        pub fn open(path: &Path) -> Result<Self, String> {
            if !ensure_mf() {
                return Err("Media Foundation 初始化失败".into());
            }
            unsafe { Self::open_inner(path) }
        }

        unsafe fn open_inner(path: &Path) -> Result<Self, String> {
            let wide: Vec<u16> = path
                .as_os_str()
                .to_string_lossy()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let url = PCWSTR(wide.as_ptr());

            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 1).map_err(|e| format!("MFCreateAttributes: {e}"))?;
            if let Some(a) = &attrs {
                let _ = a.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1u32);
            }

            let reader = MFCreateSourceReaderFromURL(url, attrs.as_ref())
                .map_err(|e| format!("打开视频失败: {e}"))?;

            let out_type = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
            out_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| format!("SetGUID major: {e}"))?;
            out_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .map_err(|e| format!("SetGUID subtype: {e}"))?;
            reader
                .SetCurrentMediaType(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    None,
                    &out_type,
                )
                .map_err(|e| format!("设置视频输出格式失败: {e}"))?;

            let native = reader
                .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
                .map_err(|e| format!("GetCurrentMediaType: {e}"))?;

            let size_u64 = native
                .GetUINT64(&MF_MT_FRAME_SIZE)
                .map_err(|e| format!("GetUINT64 FRAME_SIZE: {e}"))?;
            let (w, h) = unpack_u64(size_u64);
            if w == 0 || h == 0 {
                return Err("无法获取视频尺寸".into());
            }

            let (num, den) = native
                .GetUINT64(&MF_MT_FRAME_RATE)
                .ok()
                .map(unpack_u64)
                .unwrap_or((30, 1));
            let frame_rate = if num > 0 && den > 0 {
                num as f64 / den as f64
            } else {
                30.0
            };

            let mut duration_100ns = 0i64;
            if let Ok(var) = reader.GetPresentationAttribute(
                MF_SOURCE_READER_MEDIASOURCE.0 as u32,
                &MF_PD_DURATION,
            ) {
                duration_100ns = propvariant_to_i64(&var).unwrap_or(0);
            }

            Ok(Self {
                reader,
                width: w,
                height: h,
                duration_100ns,
                frame_rate,
                cur_100ns: 0,
                ended: false,
            })
        }

        pub fn size(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        pub fn duration_secs(&self) -> f64 {
            (self.duration_100ns.max(0) as f64) / 10_000_000.0
        }

        pub fn position_secs(&self) -> f64 {
            (self.cur_100ns.max(0) as f64) / 10_000_000.0
        }

        pub fn frame_duration(&self) -> std::time::Duration {
            std::time::Duration::from_secs_f64(1.0 / self.frame_rate.clamp(1.0, 120.0))
        }

        pub fn ended(&self) -> bool {
            self.ended
        }

        pub fn seek_secs(&mut self, secs: f64) -> Result<(), String> {
            let target = ((secs.max(0.0) * 10_000_000.0) as i64).min(self.duration_100ns.max(0));
            unsafe {
                let var = PROPVARIANT::from(target);
                self.reader
                    .SetCurrentPosition(&GUID::zeroed(), &var)
                    .map_err(|e| format!("Seek 失败: {e}"))?;
            }
            self.cur_100ns = target;
            self.ended = false;
            Ok(())
        }

        /// 读取下一帧,返回 (rgba_bytes, width, height)。到结尾返回 Ok(None)。
        pub fn read_frame(&mut self) -> Result<Option<(Vec<u8>, u32, u32)>, String> {
            if self.ended {
                return Ok(None);
            }
            unsafe { self.read_frame_inner() }
        }

        unsafe fn read_frame_inner(&mut self) -> Result<Option<(Vec<u8>, u32, u32)>, String> {
            let mut flags = 0u32;
            let mut timestamp = 0i64;
            let mut sample: Option<IMFSample> = None;
            self.reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )
                .map_err(|e| format!("ReadSample: {e}"))?;

            if (flags as i32) & (MF_SOURCE_READERF_ENDOFSTREAM.0 as i32) != 0 {
                self.ended = true;
                return Ok(None);
            }
            let Some(sample) = sample else {
                // 无样本但未结束,尝试下一次
                return Ok(None);
            };
            self.cur_100ns = timestamp;

            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| format!("ConvertToContiguousBuffer: {e}"))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len: u32 = 0;
            buffer
                .Lock(&mut ptr, None, Some(&mut len))
                .map_err(|e| format!("Lock: {e}"))?;
            let slice = std::slice::from_raw_parts(ptr, len as usize);
            // RGB32 from MF is BGRA (bottom-up possible, but with video processing usually top-down)
            // Convert BGRA -> RGBA
            let mut rgba = vec![0u8; (self.width * self.height * 4) as usize];
            let stride = (self.width * 4) as usize;
            let src_stride = if (len as usize) >= stride * self.height as usize {
                stride
            } else {
                stride
            };
            // MF RGB32 is often bottom-up (negative stride). Detect by buffer size vs expected.
            let expected = stride * self.height as usize;
            let bottom_up = len as usize >= expected; // default treat as bottom-up for RGB32
            for y in 0..self.height as usize {
                let src_y = if bottom_up { self.height as usize - 1 - y } else { y };
                let src_off = src_y * src_stride;
                let dst_off = y * stride;
                if src_off + stride > slice.len() || dst_off + stride > rgba.len() {
                    continue;
                }
                for x in 0..self.width as usize {
                    let si = src_off + x * 4;
                    let di = dst_off + x * 4;
                    let b = slice[si];
                    let g = slice[si + 1];
                    let r = slice[si + 2];
                    let a = slice[si + 3];
                    rgba[di] = r;
                    rgba[di + 1] = g;
                    rgba[di + 2] = b;
                    rgba[di + 3] = if a == 0 { 255 } else { a };
                }
            }
            let _ = buffer.Unlock();
            Ok(Some((rgba, self.width, self.height)))
        }
    }

    fn propvariant_to_i64(var: &PROPVARIANT) -> Option<i64> {
        unsafe { PropVariantToInt64(var).ok() }
    }
}

#[cfg(windows)]
pub use imp::VideoPlayer;

#[cfg(not(windows))]
pub struct VideoPlayer;

#[cfg(not(windows))]
impl VideoPlayer {
    pub fn open(_path: &std::path::Path) -> Result<Self, String> {
        Err("应用内视频播放仅支持 Windows".into())
    }
    pub fn size(&self) -> (u32, u32) { (0, 0) }
    pub fn duration_secs(&self) -> f64 { 0.0 }
    pub fn position_secs(&self) -> f64 { 0.0 }
    pub fn frame_duration(&self) -> std::time::Duration { std::time::Duration::from_millis(33) }
    pub fn ended(&self) -> bool { true }
    pub fn seek_secs(&mut self, _secs: f64) -> Result<(), String> { Ok(()) }
    pub fn read_frame(&mut self) -> Result<Option<(Vec<u8>, u32, u32)>, String> { Ok(None) }
}
