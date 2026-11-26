//! 应用内视频解码播放(Windows Media Foundation,视频帧 + 音频)。
//!
//! - 视频:MF SourceReader 把视频流解成 RGB32 帧,供 egui 纹理显示。
//! - 音频:第二个 SourceReader 解出 PCM,经 WASAPI 共享模式送到默认输出设备;
//!   共享模式下用 `AUTOCONVERTPCM` 让系统重采样,免去自己写重采样器。
//! - 同步:有音频时以音频播放位置为主时钟推进视频帧,无音频则退回墙钟节奏。
//! - 任一路失败时调用方应回退到系统播放器;音频单独失败只是静音,视频照常。

#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(windows)]
mod imp {
    use std::path::Path;
    use std::sync::Once;
    use windows::core::{GUID, PCWSTR, PROPVARIANT};
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
    };
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::StructuredStorage::PropVariantToInt64;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    static MF_INIT: Once = Once::new();
    static mut MF_OK: bool = false;

    fn ensure_mf() -> bool {
        MF_INIT.call_once(|| unsafe {
            // 已由别的组件初始化成 STA 时这里会返回 RPC_E_CHANGED_MODE,
            // 不影响 MFStartup,忽略返回值即可。
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
        /// 音频输出;视频没有音轨或音频初始化失败时为 None(静音播放)。
        audio: Option<AudioSink>,
    }

    unsafe impl Send for VideoPlayer {}

    fn unpack_u64(v: u64) -> (u32, u32) {
        ((v >> 32) as u32, (v & 0xFFFF_FFFF) as u32)
    }

    fn wide_url(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }

    impl VideoPlayer {
        pub fn open(path: &Path) -> Result<Self, String> {
            if !ensure_mf() {
                return Err("Media Foundation 初始化失败".into());
            }
            unsafe { Self::open_inner(path) }
        }

        unsafe fn open_inner(path: &Path) -> Result<Self, String> {
            let wide = wide_url(path);
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

            // 音频是增强项:拿不到就静音,视频照常播放。
            let audio = AudioSink::open(path).ok();

            Ok(Self {
                reader,
                width: w,
                height: h,
                duration_100ns,
                frame_rate,
                cur_100ns: 0,
                ended: false,
                audio,
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

        /// 是否有可用的音频输出。
        pub fn has_audio(&self) -> bool {
            self.audio.is_some()
        }

        /// 音频播放位置(秒);无音频返回 None。
        pub fn audio_position_secs(&self) -> Option<f64> {
            self.audio.as_ref().map(|a| a.position_secs())
        }

        /// 播放/暂停音频,与视频播放状态保持一致。
        pub fn set_audio_playing(&mut self, playing: bool) -> Result<(), String> {
            match self.audio.as_mut() {
                Some(a) => a.set_playing(playing),
                None => Ok(()),
            }
        }

        /// 给音频设备补充 PCM(每个 UI 帧调用一次)。
        pub fn feed_audio(&mut self) -> Result<(), String> {
            match self.audio.as_mut() {
                Some(a) => a.feed(),
                None => Ok(()),
            }
        }

        /// 音频出错时丢弃音频,退回静音视频。
        pub fn drop_audio(&mut self) {
            self.audio = None;
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
            if let Some(a) = self.audio.as_mut() {
                let _ = a.seek(target as f64 / 10_000_000.0);
            }
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
            let src_stride = stride;
            // MF RGB32 is often bottom-up (negative stride). Detect by buffer size vs expected.
            let expected = stride * self.height as usize;
            let bottom_up = len as usize >= expected; // default treat as bottom-up for RGB32
            for y in 0..self.height as usize {
                let src_y = if bottom_up {
                    self.height as usize - 1 - y
                } else {
                    y
                };
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

    /// 音频输出:MF 解 PCM + WASAPI 共享模式渲染。
    struct AudioSink {
        reader: IMFSourceReader,
        client: IAudioClient,
        render: IAudioRenderClient,
        /// 设备缓冲帧数。
        buffer_frames: u32,
        sample_rate: u32,
        block_align: u32,
        /// 已解出但还没写进设备的 PCM 字节。
        pending: Vec<u8>,
        eos: bool,
        playing: bool,
        /// 上次 seek 的起点(秒);播放位置 = 起点 + 已播帧 / 采样率。
        base_secs: f64,
        /// 自上次 seek 起写入设备的帧数。
        frames_written: u64,
    }

    unsafe impl Send for AudioSink {}

    impl AudioSink {
        fn open(path: &Path) -> Result<Self, String> {
            unsafe { Self::open_inner(path) }
        }

        unsafe fn open_inner(path: &Path) -> Result<Self, String> {
            let wide = wide_url(path);
            let url = PCWSTR(wide.as_ptr());

            let reader = MFCreateSourceReaderFromURL(url, None)
                .map_err(|e| format!("打开音轨失败: {e}"))?;
            // 只留音频流,避免重复解视频。
            let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
            reader
                .SetStreamSelection(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, true)
                .map_err(|e| format!("选择音轨失败: {e}"))?;

            let out_type = MFCreateMediaType().map_err(|e| format!("音频 MFCreateMediaType: {e}"))?;
            out_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(|e| format!("音频 major: {e}"))?;
            out_type
                .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)
                .map_err(|e| format!("音频 subtype: {e}"))?;
            // 统一成 16bit 立体声 44.1k;设备侧用 AUTOCONVERTPCM 重采样。
            out_type
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)
                .map_err(|e| format!("音频通道: {e}"))?;
            out_type
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 44_100)
                .map_err(|e| format!("音频采样率: {e}"))?;
            out_type
                .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .map_err(|e| format!("音频位深: {e}"))?;
            out_type
                .SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)
                .map_err(|e| format!("音频块对齐: {e}"))?;
            out_type
                .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 176_400)
                .map_err(|e| format!("音频码率: {e}"))?;
            reader
                .SetCurrentMediaType(
                    MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32,
                    None,
                    &out_type,
                )
                .map_err(|e| format!("设置音频输出格式失败: {e}"))?;

            let native = reader
                .GetCurrentMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32)
                .map_err(|e| format!("音频 GetCurrentMediaType: {e}"))?;
            let channels = native.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(2);
            let sample_rate = native
                .GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)
                .unwrap_or(44_100);
            let bits = native.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).unwrap_or(16);
            let block_align = native
                .GetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT)
                .unwrap_or(channels * bits / 8);
            if channels == 0
                || sample_rate == 0
                || bits == 0
                || bits % 8 != 0
                || block_align == 0
            {
                return Err("音频格式不可用".into());
            }

            let wfx = WAVEFORMATEX {
                // WAVE_FORMAT_PCM
                wFormatTag: 1,
                nChannels: channels as u16,
                nSamplesPerSec: sample_rate,
                nAvgBytesPerSec: sample_rate.saturating_mul(block_align),
                nBlockAlign: block_align as u16,
                wBitsPerSample: bits as u16,
                cbSize: 0,
            };

            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| format!("音频设备枚举失败: {e}"))?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| format!("没有默认音频输出设备: {e}"))?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("激活音频客户端失败: {e}"))?;
            // 1 秒缓冲;共享模式 + 自动转换,允许源格式与混音格式不同。
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    10_000_000,
                    0,
                    &wfx,
                    None,
                )
                .map_err(|e| format!("初始化音频输出失败: {e}"))?;
            let render: IAudioRenderClient = client
                .GetService()
                .map_err(|e| format!("取音频渲染接口失败: {e}"))?;
            let buffer_frames = client
                .GetBufferSize()
                .map_err(|e| format!("取音频缓冲大小失败: {e}"))?;
            client.Start().map_err(|e| format!("启动音频失败: {e}"))?;

            Ok(Self {
                reader,
                client,
                render,
                buffer_frames,
                sample_rate,
                block_align,
                pending: Vec::new(),
                eos: false,
                playing: true,
                base_secs: 0.0,
                frames_written: 0,
            })
        }

        fn position_secs(&self) -> f64 {
            let padding = unsafe { self.client.GetCurrentPadding() }.unwrap_or(0);
            let played = self.frames_written.saturating_sub(padding as u64);
            self.base_secs + played as f64 / self.sample_rate as f64
        }

        fn set_playing(&mut self, playing: bool) -> Result<(), String> {
            if playing == self.playing {
                return Ok(());
            }
            unsafe {
                if playing {
                    self.client.Start().map_err(|e| format!("启动音频失败: {e}"))?;
                } else {
                    self.client.Stop().map_err(|e| format!("暂停音频失败: {e}"))?;
                }
            }
            self.playing = playing;
            Ok(())
        }

        fn seek(&mut self, secs: f64) -> Result<(), String> {
            let secs = secs.max(0.0);
            unsafe {
                let var = PROPVARIANT::from((secs * 10_000_000.0) as i64);
                self.reader
                    .SetCurrentPosition(&GUID::zeroed(), &var)
                    .map_err(|e| format!("音频 Seek 失败: {e}"))?;
                self.pending.clear();
                self.eos = false;
                // Reset 前必须先 Stop,之后按原播放状态决定是否重启。
                let _ = self.client.Stop();
                let _ = self.client.Reset();
                self.frames_written = 0;
                self.base_secs = secs;
                if self.playing {
                    self.client.Start().map_err(|e| format!("重启音频失败: {e}"))?;
                }
            }
            Ok(())
        }

        fn feed(&mut self) -> Result<(), String> {
            unsafe { self.feed_inner() }
        }

        unsafe fn feed_inner(&mut self) -> Result<(), String> {
            // 目标:保持约 0.5 秒的待写数据,避免 UI 卡顿时断音。
            let target_bytes = self.sample_rate as usize * self.block_align as usize / 2;
            let mut reads = 0;
            while !self.eos && self.pending.len() < target_bytes && reads < 16 {
                reads += 1;
                let mut flags = 0u32;
                let mut timestamp = 0i64;
                let mut sample: Option<IMFSample> = None;
                self.reader
                    .ReadSample(
                        MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut timestamp),
                        Some(&mut sample),
                    )
                    .map_err(|e| format!("音频 ReadSample: {e}"))?;
                if (flags as i32) & (MF_SOURCE_READERF_ENDOFSTREAM.0 as i32) != 0 {
                    self.eos = true;
                    break;
                }
                let Some(sample) = sample else {
                    continue;
                };
                let buffer = sample
                    .ConvertToContiguousBuffer()
                    .map_err(|e| format!("音频 ConvertToContiguousBuffer: {e}"))?;
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut len: u32 = 0;
                buffer
                    .Lock(&mut ptr, None, Some(&mut len))
                    .map_err(|e| format!("音频 Lock: {e}"))?;
                if !ptr.is_null() && len > 0 {
                    let slice = std::slice::from_raw_parts(ptr, len as usize);
                    self.pending.extend_from_slice(slice);
                }
                let _ = buffer.Unlock();
            }

            // 尽量把待写数据塞进设备缓冲。
            loop {
                let padding = self
                    .client
                    .GetCurrentPadding()
                    .map_err(|e| format!("音频 GetCurrentPadding: {e}"))?;
                let avail = self.buffer_frames.saturating_sub(padding);
                if avail == 0 {
                    break;
                }
                let frames = (self.pending.len() / self.block_align as usize) as u32;
                if frames == 0 {
                    break;
                }
                let n = avail.min(frames);
                let dst = self
                    .render
                    .GetBuffer(n)
                    .map_err(|e| format!("音频 GetBuffer: {e}"))?;
                let bytes = n as usize * self.block_align as usize;
                std::ptr::copy_nonoverlapping(self.pending.as_ptr(), dst, bytes);
                self.render
                    .ReleaseBuffer(n, 0)
                    .map_err(|e| format!("音频 ReleaseBuffer: {e}"))?;
                self.pending.drain(0..bytes);
                self.frames_written += n as u64;
            }
            Ok(())
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
    pub fn size(&self) -> (u32, u32) {
        (0, 0)
    }
    pub fn duration_secs(&self) -> f64 {
        0.0
    }
    pub fn position_secs(&self) -> f64 {
        0.0
    }
    pub fn frame_duration(&self) -> std::time::Duration {
        std::time::Duration::from_millis(33)
    }
    pub fn ended(&self) -> bool {
        true
    }
    pub fn has_audio(&self) -> bool {
        false
    }
    pub fn audio_position_secs(&self) -> Option<f64> {
        None
    }
    pub fn set_audio_playing(&mut self, _playing: bool) -> Result<(), String> {
        Ok(())
    }
    pub fn feed_audio(&mut self) -> Result<(), String> {
        Ok(())
    }
    pub fn drop_audio(&mut self) {}
    pub fn seek_secs(&mut self, _secs: f64) -> Result<(), String> {
        Ok(())
    }
    pub fn read_frame(&mut self) -> Result<Option<(Vec<u8>, u32, u32)>, String> {
        Ok(None)
    }
}
